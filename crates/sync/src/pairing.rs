//! Pairing: hand the rclone remote of a cloud library to another device without copying config files by hand.
//!
//! The home PC [`export_config`]s the one remote (and what it wraps) out of its rclone config, [`seal`]s it into a
//! piece of text (shown as a QR code and as copyable text). The code is made by the JOINING device ([`new_code`]), shown on its
//! own screen and typed into the home PC, so the home PC's screen only ever shows the locked text. The joining device
//! [`open`]s the text with the code it made. The text IS the encrypted bundle: no server, no network, nothing to maintain.
//!
//! Text = `LCP1` + base45 (RFC 9285; upper case letters, digits and ` $%*+-./:`, which a QR code stores in its
//! compact alphanumeric mode) of `header || ciphertext`. Header (41 bytes, authenticated as AAD):
//! `version(1) | argon2 memory KiB (u32 LE) | passes (u32 LE) | lanes (u32 LE) | salt(16) | nonce(12)`.
//! Ciphertext: ChaCha20-Poly1305 of the deflated JSON of the [`Bundle`], key = Argon2id(code, salt, header's params).
//!
//! Nothing here logs. [`Bundle`]'s `Debug` shows no contents, and its secrets are zeroized when it is dropped.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::{Error, Result};

/// The text starts with this (and its format version).
pub const PREFIX: &str = "LCP1";
/// The bundle's `format`.
pub const BUNDLE_FORMAT: u32 = 1;
const HEADER_VERSION: u8 = 1;
const HEADER_LEN: usize = 1 + 4 + 4 + 4 + 16 + 12;
const TAG_LEN: usize = 16;
/// Key derivation settings written into new texts.
const KDF_MEMORY_KIB: u32 = 32 * 1024;
const KDF_PASSES: u32 = 3;
const KDF_LANES: u32 = 1;
/// What `open` accepts from a text it did not make (a hostile header must not make it allocate gigabytes or spin for minutes).
const MAX_MEMORY_KIB: u32 = 128 * 1024;
const MAX_PASSES: u32 = 10;
const MAX_LANES: u32 = 8;
/// A config is a few hundred bytes; this only bounds what a hostile text can inflate to.
const MAX_PLAIN: u64 = 1 << 20;
const CODE_LEN: usize = 10;
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// What travels: the remote the library lives on and the rclone config text that defines it (and nothing else).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    pub format: u32,
    /// For example `gdrive:LightCraft Cloud`.
    pub remote: String,
    /// rclone config text (INI) holding only the sections the remote needs. Secret.
    pub config: String,
}

impl Bundle {
    pub fn new(remote: impl Into<String>, config: impl Into<String>) -> Self {
        Bundle { format: BUNDLE_FORMAT, remote: remote.into(), config: config.into() }
    }
}

impl std::fmt::Debug for Bundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bundle").field("format", &self.format).finish_non_exhaustive()
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        self.config.zeroize();
        self.remote.zeroize();
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Export: pick the sections out of an rclone config

/// Keys that name a file on this PC: they cannot travel (the inline equivalents can).
const LOCAL_FILE_KEYS: &[&str] = &[
    "key_file",
    "service_account_file",
    "pem_file",
    "ca_cert",
    "client_cert",
    "client_key",
    "shared_credentials_file",
    "known_hosts_file",
    "pubkey_file",
    "client_certificate_path",
    "sas_credentials_file",
    "credentials_file",
    "token_file",
    "ssh",
];

/// Backends that wrap another remote in their `remote` key.
const WRAPPERS: &[&str] = &["crypt", "alias", "chunker", "compress", "hasher"];

struct Section {
    name: String,
    lines: Vec<(String, String)>,
}

impl Section {
    fn get(&self, key: &str) -> Option<&str> {
        self.lines.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

fn parse_ini(text: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[')
            && let Some(name) = rest.strip_suffix(']') {
                out.push(Section { name: name.trim().to_string(), lines: Vec::new() });
                continue;
            }
        if let (Some(cur), Some((k, v))) = (out.last_mut(), line.split_once('=')) {
            cur.lines.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    out
}

/// The section name a remote string (`name:path`, `name,opt=x:path`) points at; None for a local path or a `:backend:` string.
fn remote_name(remote: &str) -> Option<String> {
    let (head, tail) = remote.split_once(':')?;
    let name = head.split(',').next().unwrap_or("").trim();
    // (a letter then `:\` or `:/` is a Windows drive path)
    let drive = name.len() == 1 && name.chars().all(|c| c.is_ascii_alphabetic()) && tail.starts_with(['\\', '/']);
    if name.is_empty() || drive {
        return None;
    }
    Some(name.to_string())
}

/// The names of the sections of rclone config text (for listing the remotes a pairing brought).
pub fn section_names(conf_text: &str) -> Vec<String> {
    parse_ini(conf_text).into_iter().map(|s| s.name).collect()
}

fn refuse<T>(why: &str) -> Result<T> {
    Err(Error::Invalid(why.to_string()))
}

/// The rclone config text for `remote` only: its section, and the sections of what it wraps (crypt, alias, chunker, compress, hasher).
/// Never any other remote. Refuses what cannot travel, with a message for the person.
pub fn export_config(conf_text: &str, remote: &str) -> Result<String> {
    let sections = parse_ini(conf_text);
    let Some(first) = remote_name(remote) else {
        return refuse(
            "This library is a folder on this PC, not an rclone remote, so there is nothing to hand to another device. \
             Pairing works for a library on a cloud remote such as Google Drive.",
        );
    };
    let mut wanted: Vec<&Section> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut next = Some(first);
    while let Some(name) = next.take() {
        if !seen.insert(name.clone()) {
            return refuse("The remote refers back to itself, so it cannot be handed over.");
        }
        let Some(sec) = sections.iter().find(|s| s.name == name) else {
            return refuse("The rclone config on this PC has no remote of that name (or one it wraps), so there is nothing to hand over.");
        };
        let ty = sec.get("type").unwrap_or("");
        match ty {
            "local" => return refuse("This remote is a folder on this PC, so it cannot be handed to another device."),
            "union" | "combine" => {
                return refuse("A union or combine remote joins several remotes and cannot be handed over. Pair with one plain remote instead.");
            }
            _ => {}
        }
        for key in LOCAL_FILE_KEYS {
            if sec.get(key).is_some_and(|v| !v.is_empty()) {
                return Err(Error::Invalid(format!(
                    "This remote uses a file on this PC (the setting \"{key}\") that cannot travel to another device. \
                     Put its contents into the rclone config itself, or set the remote up again without a file."
                )));
            }
        }
        if WRAPPERS.contains(&ty) {
            match sec.get("remote").and_then(remote_name) {
                Some(inner) => next = Some(inner),
                None => return refuse("A remote this one wraps is a folder on this PC, so it cannot be handed to another device."),
            }
        }
        wanted.push(sec);
    }
    let mut out = String::new();
    for sec in wanted {
        out.push_str(&format!("[{}]\n", sec.name));
        for (k, v) in &sec.lines {
            // (the token travels as it is: rclone will not renew a token whose access token was blanked, tested on Google Drive)
            out.push_str(&format!("{k} = {v}\n"));
        }
        out.push('\n');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------------------------------
// Base45 (RFC 9285)

const B45: &[u8; 45] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ $%*+-./:";

fn base45_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 3 / 2 + 2);
    for ch in data.chunks(2) {
        if let [a, b] = ch {
            let n = *a as usize * 256 + *b as usize;
            for d in [n % 45, (n / 45) % 45, n / 2025] {
                s.push(B45[d] as char);
            }
        } else {
            let n = ch[0] as usize;
            s.push(B45[n % 45] as char);
            s.push(B45[n / 45] as char);
        }
    }
    s
}

fn base45_decode(text: &str) -> Option<Vec<u8>> {
    let vals: Vec<usize> = text
        .bytes()
        .map(|c| B45.iter().position(|&b| b == c))
        .collect::<Option<_>>()?;
    let mut out = Vec::with_capacity(vals.len() * 2 / 3 + 1);
    for ch in vals.chunks(3) {
        match ch {
            [c, d, e] => {
                let n = c + d * 45 + e * 2025;
                if n > 0xFFFF {
                    return None;
                }
                out.push((n >> 8) as u8);
                out.push((n & 0xFF) as u8);
            }
            [c, d] => {
                let n = c + d * 45;
                if n > 0xFF {
                    return None;
                }
                out.push(n as u8);
            }
            _ => return None,
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------------------------------
// Code

/// A new pairing code, made by the JOINING device and typed into the home PC: 10 random Crockford base32 characters and one
/// check character, shown as `XXXX-XXXX-XXX`. The check character is the sum of `(2*i + 1) * value(char i)` over the 10
/// random characters, mod 32 (odd weights: every single wrong character changes it; an adjacent swap changes it unless the two
/// characters differ by exactly 16 in value). The key is derived from the whole normalised code (all 11 characters).
pub fn new_code() -> String {
    let mut raw = [0u8; CODE_LEN];
    OsRng.fill_bytes(&mut raw);
    // (32 symbols, so the low 5 bits of a random byte are uniform)
    let mut body: Vec<u8> = raw.iter().map(|b| CROCKFORD[(*b & 31) as usize]).collect();
    raw.zeroize();
    body.push(check_char(&body));
    let s: String = body.iter().map(|b| *b as char).collect();
    body.zeroize();
    format!("{}-{}-{}", &s[..4], &s[4..8], &s[8..])
}

/// The check character for the first `CODE_LEN` characters of `body` (already normalised, in the alphabet).
fn check_char(body: &[u8]) -> u8 {
    let sum: usize = body
        .iter()
        .take(CODE_LEN)
        .enumerate()
        .map(|(i, c)| (2 * i + 1) * CROCKFORD.iter().position(|a| a == c).unwrap_or(0))
        .sum();
    CROCKFORD[sum % 32]
}

/// Upper case, no spaces or dashes, I and L read as 1, O as 0. None when it is not 11 symbols of the alphabet with the right check.
fn normalize_code(code: &str) -> Option<Zeroizing<Vec<u8>>> {
    let mut out = Zeroizing::new(Vec::with_capacity(CODE_LEN + 1));
    for c in code.chars() {
        if c == '-' || c.is_whitespace() {
            continue;
        }
        let c = match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            u => u,
        };
        if !c.is_ascii() || !CROCKFORD.contains(&(c as u8)) {
            return None;
        }
        out.push(c as u8);
    }
    (out.len() == CODE_LEN + 1 && check_char(&out) == out[CODE_LEN]).then_some(out)
}

const TYPO: &str = "That code has a typo; check it against the device.";

/// Checks a typed code (case, spaces, dashes ignored; I and L read as 1, O as 0) and returns it normalised (11 characters, no
/// dashes). Err (a typo, a wrong length, a wrong check character) says so in words and never echoes what was typed.
pub fn check_code(code: &str) -> Result<String> {
    match normalize_code(code) {
        Some(c) => Ok(String::from_utf8_lossy(&c).into_owned()),
        None => Err(Error::Invalid(TYPO.to_string())),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Seal and open

#[derive(Clone, Copy)]
struct Kdf {
    memory_kib: u32,
    passes: u32,
    lanes: u32,
}

const DEFAULT_KDF: Kdf = Kdf { memory_kib: KDF_MEMORY_KIB, passes: KDF_PASSES, lanes: KDF_LANES };

fn derive_key(code: &[u8], salt: &[u8], kdf: Kdf) -> Option<Zeroizing<[u8; 32]>> {
    let params = Params::new(kdf.memory_kib, kdf.passes, kdf.lanes, Some(32)).ok()?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params).hash_password_into(code, salt, &mut key[..]).ok()?;
    Some(key)
}

fn seal_with(bundle: &Bundle, code: &str, kdf: Kdf) -> Result<String> {
    let bad = |what: &str| Error::Invalid(format!("Could not make the pairing text ({what})."));
    let code = normalize_code(code).ok_or_else(|| Error::Invalid(TYPO.to_string()))?;
    let json = Zeroizing::new(serde_json::to_vec(bundle).map_err(|_| bad("encode"))?);
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&json).map_err(|_| bad("compress"))?;
    let packed = Zeroizing::new(enc.finish().map_err(|_| bad("compress"))?);

    let mut header = Vec::with_capacity(HEADER_LEN);
    header.push(HEADER_VERSION);
    header.extend_from_slice(&kdf.memory_kib.to_le_bytes());
    header.extend_from_slice(&kdf.passes.to_le_bytes());
    header.extend_from_slice(&kdf.lanes.to_le_bytes());
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);
    header.extend_from_slice(&salt);
    header.extend_from_slice(&nonce);

    let key = derive_key(&code, &salt, kdf).ok_or_else(|| bad("key"))?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key[..]));
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: &packed, aad: &header })
        .map_err(|_| bad("encrypt"))?;
    header.extend_from_slice(&ct);
    Ok(format!("{PREFIX}{}", base45_encode(&header)))
}

/// Seals the bundle with the code the joining device showed (checked first; see [`check_code`]). The text is a secret.
pub fn seal(bundle: &Bundle, code: &str) -> Result<String> {
    check_code(code)?;
    seal_with(bundle, code, DEFAULT_KDF)
}

/// Opens a pairing text with its code. The messages never echo what was typed.
pub fn open(text: &str, code: &str) -> Result<Bundle> {
    let err = |m: &str| Error::Invalid(m.to_string());
    // (a pasted text may have been wrapped on the way: line breaks and edge spaces are not part of it)
    let cleaned: String = text.chars().filter(|c| !matches!(c, '\r' | '\n' | '\t')).collect();
    let Some(body) = cleaned.trim().strip_prefix(PREFIX) else {
        return Err(err("That is not a LightCraft pairing text. It should start with LCP1."));
    };
    let code = normalize_code(code).ok_or_else(|| err(TYPO))?;
    let damaged = || err("The pairing text is damaged or incomplete. Copy it again, all of it.");
    let data = base45_decode(body).ok_or_else(damaged)?;
    if data.len() < HEADER_LEN + TAG_LEN {
        return Err(damaged());
    }
    let (header, ct) = data.split_at(HEADER_LEN);
    if header[0] != HEADER_VERSION {
        return Err(err("This pairing text was made by a newer version of LightCraft Cloud. Update this app."));
    }
    let u32_at = |i: usize| u32::from_le_bytes(header[i..i + 4].try_into().unwrap());
    let kdf = Kdf { memory_kib: u32_at(1), passes: u32_at(5), lanes: u32_at(9) };
    // (checked before anything is allocated)
    if kdf.memory_kib > MAX_MEMORY_KIB || kdf.passes > MAX_PASSES || kdf.lanes > MAX_LANES || kdf.passes == 0 || kdf.lanes == 0 {
        return Err(err("This pairing text asks for more work than this app will do, so it was not opened. It may not be genuine."));
    }
    let wrong = || err("The code does not match this pairing text, or the text was changed. Check both and try again.");
    let key = derive_key(&code, &header[13..29], kdf).ok_or_else(wrong)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key[..]));
    let packed = Zeroizing::new(
        cipher.decrypt(Nonce::from_slice(&header[29..41]), Payload { msg: ct, aad: header }).map_err(|_| wrong())?,
    );
    let mut json = Zeroizing::new(Vec::new());
    flate2::read::DeflateDecoder::new(&packed[..]).take(MAX_PLAIN + 1).read_to_end(&mut json).map_err(|_| damaged())?;
    if json.len() as u64 > MAX_PLAIN {
        return Err(damaged());
    }
    let bundle: Bundle = serde_json::from_slice(&json).map_err(|_| damaged())?;
    if bundle.format != BUNDLE_FORMAT {
        return Err(err("This pairing text was made by a newer version of LightCraft Cloud. Update this app."));
    }
    Ok(bundle)
}

// ---------------------------------------------------------------------------------------------------------------------
// Write

/// Writes `text` to `path` atomically (a temporary file next to it, then a rename), mode 600 on Unix; makes the folder.
pub fn write_config(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir).map_err(|e| Error::io("create the config folder", e))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let result = (|| -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        crate::replace_file(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::io("write the paired config", e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: Kdf = Kdf { memory_kib: 64, passes: 1, lanes: 1 };

    /// A valid code from 10 chosen characters (adds the check character).
    fn code_of(body: &str) -> String {
        let mut b = body.as_bytes().to_vec();
        b.push(check_char(&b));
        let t = |r: std::ops::Range<usize>| std::str::from_utf8(&b[r]).unwrap().to_string();
        format!("{}-{}-{}", t(0..4), t(4..8), t(8..11))
    }

    fn drive_conf() -> String {
        // Synthetic values only.
        "[gdrive]\n\
         type = drive\n\
         client_id = fake-client-id-for-tests-only-000000000000000000.example.invalid\n\
         client_secret = fake-client-secret-FAKEFAKEFAKEFAKE\n\
         scope = drive\n\
         token = {\"access_token\":\"ya29.FAKEaccessTOKENfakeFAKEaccessTOKENfakeFAKEaccessTOKENfakeFAKEaccessTOKENfakeFAKEaccessTOKENfakeFAKEaccessTOKENfakeFAKEaccessTOKENfake\",\"token_type\":\"Bearer\",\"refresh_token\":\"1//0gFAKErefreshFAKErefreshFAKErefreshFAKErefreshFAKErefreshFAKErefreshFAKEre\",\"expiry\":\"2026-10-01T12:34:56.789012345+02:00\"}\n\
         root_folder_id = 1AbCdEfGhIjKlMnOpQrStUvWxYz0123456\n\
         team_drive = \n"
            .to_string()
    }

    #[test]
    fn base45_known_vectors() {
        // RFC 9285 section 4.3
        assert_eq!(base45_encode(b"AB"), "BB8");
        assert_eq!(base45_encode(b"Hello!!"), "%69 VD92EX0");
        assert_eq!(base45_encode(b"base-45"), "UJCLQE7W581");
        assert_eq!(base45_decode("QED8WEX0").unwrap(), b"ietf!");
        assert!(base45_decode("GGW").is_none());
        assert!(base45_decode("A").is_none());
    }

    #[test]
    fn round_trip() {
        let b = Bundle::new("gdrive:LightCraft Cloud", drive_conf());
        let code = new_code();
        let text = seal(&b, &code).unwrap();
        assert!(text.starts_with("LCP1"));
        assert_eq!(code.len(), 13);
        assert_eq!((code.as_bytes()[4], code.as_bytes()[9]), (b'-', b'-'));
        assert_eq!(open(&text, &code).unwrap(), b);
        // case, spaces, dashes, I/L/O confusion
        let sloppy = code.to_lowercase().replace('-', " ").replace('1', "l").replace('0', "o");
        assert_eq!(open(&text, &sloppy).unwrap(), b);
        // a wrapped paste
        let wrapped = format!("{}\n{}\n", &text[..100], &text[100..]);
        assert_eq!(open(&wrapped, &code).unwrap(), b);
    }

    #[test]
    fn wrong_code_and_tamper_and_prefix() {
        let b = Bundle::new("gdrive:x", "[gdrive]\ntype = drive\n");
        let text = seal_with(&b, &code_of("ABCDE12345"), FAST).unwrap();
        let e = open(&text, &code_of("ABCDE12346")).unwrap_err().to_string();
        assert!(e.contains("does not match"), "{e}");
        assert!(!e.contains("ABCD"), "{e}");
        let e = open(&text, "ABC").unwrap_err().to_string();
        assert!(e.contains("typo"), "{e}");
        let e = open("LCP2ABC", &code_of("ABCDE12345")).unwrap_err().to_string();
        assert!(e.contains("not a LightCraft pairing text"), "{e}");
        // flip a character in the ciphertext
        let mut chars: Vec<char> = text.chars().collect();
        let i = chars.len() - 5;
        chars[i] = if chars[i] == 'A' { 'B' } else { 'A' };
        let altered: String = chars.into_iter().collect();
        let e = open(&altered, &code_of("ABCDE12345")).unwrap_err().to_string();
        assert!(e.contains("does not match") || e.contains("damaged"), "{e}");
        // a header byte (a parameter) is authenticated too
        let mut data = base45_decode(&text[4..]).unwrap();
        data[13] ^= 1; // salt
        let e = open(&format!("LCP1{}", base45_encode(&data)), &code_of("ABCDE12345")).unwrap_err().to_string();
        assert!(e.contains("does not match"), "{e}");
        let e = open(&text[..text.len() - 6], &code_of("ABCDE12345")).unwrap_err().to_string();
        assert!(e.contains("damaged") || e.contains("does not match"), "{e}");
        let e = open("LCP1", &code_of("ABCDE12345")).unwrap_err().to_string();
        assert!(e.contains("damaged"), "{e}");
    }

    #[test]
    fn hostile_params_are_rejected_before_work() {
        let b = Bundle::new("a:b", "x");
        let text = seal_with(&b, &code_of("ABCDE12345"), FAST).unwrap();
        for (off, val) in [(1usize, u32::MAX), (1, 4 * 1024 * 1024), (5, u32::MAX), (5, 0), (9, u32::MAX), (9, 0)] {
            let mut data = base45_decode(&text[4..]).unwrap();
            data[off..off + 4].copy_from_slice(&val.to_le_bytes());
            let t0 = std::time::Instant::now();
            let e = open(&format!("LCP1{}", base45_encode(&data)), &code_of("ABCDE12345")).unwrap_err().to_string();
            assert!(e.contains("more work"), "{off} {val}: {e}");
            assert!(t0.elapsed().as_secs() < 1);
        }
        let mut data = base45_decode(&text[4..]).unwrap();
        data[0] = 9;
        let e = open(&format!("LCP1{}", base45_encode(&data)), &code_of("ABCDE12345")).unwrap_err().to_string();
        assert!(e.contains("newer version"), "{e}");
    }

    #[test]
    fn default_settings_round_trip() {
        let code = new_code();
        let text = seal(&Bundle::new("a:b", "c"), &code).unwrap();
        let data = base45_decode(&text[4..]).unwrap();
        assert_eq!(u32::from_le_bytes(data[1..5].try_into().unwrap()), 32 * 1024);
        assert_eq!(u32::from_le_bytes(data[5..9].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(data[9..13].try_into().unwrap()), 1);
        assert_eq!(open(&text, &code).unwrap().config, "c");
    }

    #[test]
    fn codes_have_a_format_and_a_check_that_catches_typos() {
        for _ in 0..200 {
            let c = new_code();
            assert_eq!(c.len(), 13);
            assert!(c.bytes().enumerate().all(|(i, b)| if i == 4 || i == 9 { b == b'-' } else { CROCKFORD.contains(&b) }), "{c}");
            assert_eq!(check_code(&c.to_lowercase()).unwrap(), c.replace('-', ""));
        }
        assert_ne!(new_code(), new_code());
        // a fixed one: sum of (2i+1)*i for i in 0..10 is 615, 615 mod 32 = 7
        assert_eq!(code_of("0123456789"), "0123-4567-897");
        // forgiving input
        assert_eq!(check_code(" 0123 4567 897 ").unwrap(), "01234567897");
        assert_eq!(check_code("oi23-4567-897").unwrap(), "01234567897");
        assert_eq!(check_code("0l23-4567-897").unwrap(), "01234567897");
        // every single substituted character is caught; adjacent swaps are caught unless the values differ by 16
        let base: Vec<u8> = code_of("7M3QX0K9TB").bytes().filter(|b| *b != b'-').collect();
        let s = |v: &[u8]| String::from_utf8(v.to_vec()).unwrap();
        assert!(check_code(&s(&base)).is_ok());
        for i in 0..11 {
            for &c in CROCKFORD.iter() {
                if c == base[i] {
                    continue;
                }
                let mut v = base.clone();
                v[i] = c;
                assert!(check_code(&s(&v)).is_err(), "substitution at {i}");
            }
        }
        let val = |c: u8| CROCKFORD.iter().position(|a| *a == c).unwrap() as i32;
        for i in 0..9 {
            let mut v = base.clone();
            v.swap(i, i + 1);
            let differ = (val(base[i]) - val(base[i + 1])).abs();
            if differ != 0 && differ != 16 {
                assert!(check_code(&s(&v)).is_err(), "swap at {i}");
            }
        }
        // wrong length, wrong alphabet, empty
        for bad in ["", "ABC", "0123-4567-8", "0123-4567-89XX", "UUUU-UUUU-UUU"] {
            let e = check_code(bad).unwrap_err().to_string();
            assert!(e.contains("typo"), "{e}");
        }
        // seal refuses a code with a typo
        assert!(seal(&Bundle::new("a:b", "c"), "0123-4567-898").is_err());
    }

    #[test]
    fn debug_shows_no_contents() {
        let b = Bundle::new("gdrive:Secret Folder", "client_secret = SUPERSECRET");
        let d = format!("{b:?} {:#?}", b);
        assert!(!d.contains("SUPERSECRET") && !d.contains("Secret Folder"), "{d}");
    }

    #[test]
    fn only_the_named_remote_survives() {
        let conf = format!(
            "# comment\n{}\n[other]\ntype = s3\nsecret_access_key = OTHERFAKESECRET\naccess_key_id = OTHERFAKEID\n\n[dropbox]\ntype = dropbox\ntoken = {{\"access_token\":\"DBXFAKE\",\"refresh_token\":\"r\"}}\n",
            drive_conf()
        );
        let out = export_config(&conf, "gdrive:LightCraft Cloud").unwrap();
        assert!(out.contains("[gdrive]") && out.contains("client_secret = fake-client-secret-FAKE"));
        assert!(!out.contains("OTHERFAKE") && !out.contains("DBXFAKE") && !out.contains("[other]") && !out.contains("[dropbox]"));
        assert!(!out.contains("comment"));
        assert!(out.contains("root_folder_id = 1AbCdEfGhIjKlMnOpQrStUvWxYz0123456"));
        // the token travels unchanged
        let tok_line = out.lines().find(|l| l.starts_with("token = ")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&tok_line["token = ".len()..]).unwrap();
        assert!(v["access_token"].as_str().unwrap().starts_with("ya29.FAKE"));
        assert!(v["refresh_token"].as_str().unwrap().starts_with("1//0gFAKE"));
        // a section name with spaces, and no path part
        let out = export_config("[my cloud]\ntype = drive\n[x]\ntype = drive\n", "my cloud:").unwrap();
        assert!(out.contains("[my cloud]") && !out.contains("[x]"));
    }

    #[test]
    fn odd_token_lines_travel_unchanged() {
        let out = export_config("[g]\ntype = drive\ntoken = not json\n", "g:").unwrap();
        assert!(out.contains("token = not json"));
    }

    #[test]
    fn crypt_chain_includes_what_it_wraps() {
        let conf = "[secret]\ntype = crypt\nremote = chunky:enc\npassword = FAKEPASS1\n\n\
                    [chunky]\ntype = chunker\nremote = gd:base\n\n[gd]\ntype = drive\nclient_id = a\n\n[unrelated]\ntype = drive\nclient_secret = NOPE\n";
        let out = export_config(conf, "secret:photos").unwrap();
        for s in ["[secret]", "[chunky]", "[gd]", "FAKEPASS1"] {
            assert!(out.contains(s), "{s}");
        }
        assert!(!out.contains("NOPE") && !out.contains("[unrelated]"));
        // alias, compress, hasher
        for ty in ["alias", "compress", "hasher"] {
            let c = format!("[w]\ntype = {ty}\nremote = gd:x\n[gd]\ntype = drive\n[z]\ntype = drive\n");
            let out = export_config(&c, "w:").unwrap();
            assert!(out.contains("[gd]") && out.contains("[w]") && !out.contains("[z]"), "{ty}");
        }
    }

    #[test]
    fn refusals() {
        let msg = |conf: &str, remote: &str| export_config(conf, remote).unwrap_err().to_string();
        assert!(msg("[a]\ntype = drive\n", "/home/me/photos").contains("folder on this PC"));
        assert!(msg("[a]\ntype = drive\n", "photos").contains("folder on this PC"));
        assert!(msg("[a]\ntype = drive\n", "C:\\photos").contains("folder on this PC"));
        assert!(msg("[a]\ntype = drive\n", ":local:/x").contains("folder on this PC"));
        assert!(msg("[a]\ntype = local\n", "a:x").contains("folder on this PC"));
        assert!(msg("[u]\ntype = union\nupstreams = a: b:\n", "u:").contains("union or combine"));
        assert!(msg("[u]\ntype = combine\n", "u:").contains("union or combine"));
        assert!(msg("[a]\ntype = drive\n", "missing:x").contains("no remote of that name"));
        assert!(msg("[c]\ntype = crypt\nremote = gone:x\n", "c:").contains("no remote of that name"));
        assert!(msg("[c]\ntype = crypt\nremote = /local/dir\n", "c:").contains("folder on this PC"));
        assert!(msg("[c]\ntype = crypt\nremote = c:x\n", "c:").contains("refers back"));
        for key in LOCAL_FILE_KEYS {
            let m = msg(&format!("[a]\ntype = x\n{key} = /some/file\n"), "a:");
            assert!(m.contains(key), "{key}: {m}");
        }
        // a wrapped remote with a file key is refused too
        assert!(msg("[c]\ntype = crypt\nremote = a:x\n[a]\ntype = drive\nservice_account_file = /f.json\n", "c:").contains("service_account_file"));
        // empty value is no file; inline credentials travel
        assert!(export_config("[a]\ntype = drive\nservice_account_file =\nservice_account_credentials = {}\n", "a:").is_ok());
    }

    #[test]
    fn write_config_is_atomic_and_private() {
        let dir = std::env::temp_dir().join(format!("lcp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("paired-rclone.conf");
        write_config(&path, "one").unwrap();
        write_config(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert!(!path.with_file_name("paired-rclone.conf.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn realistic_drive_text_fits_a_small_qr() {
        let conf = export_config(&drive_conf(), "gdrive:LightCraft Cloud").unwrap();
        let text = seal(&Bundle::new("gdrive:LightCraft Cloud", conf.clone()), &new_code()).unwrap();
        let mut sizes = Vec::new();
        for (name, ec) in [("L", qrcode::EcLevel::L), ("M", qrcode::EcLevel::M)] {
            let qr = qrcode::QrCode::with_error_correction_level(text.as_bytes(), ec).unwrap();
            let v = match qr.version() {
                qrcode::Version::Normal(v) => v,
                qrcode::Version::Micro(_) => 0,
            };
            sizes.push(format!("EC {name}: version {v}"));
            assert!(v <= 25, "EC {name}: version {v}");
        }
        eprintln!("PAIRING SIZE: config {} bytes, text {} characters; {}", conf.len(), text.len(), sizes.join("; "));
    }
}
