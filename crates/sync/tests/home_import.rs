//! Imports: photos other PCs uploaded (`imports/<device>/<id>.json` with its files), adopted into the photo folder by the home
//! sync (FORMAT.md, "Imports"). One test (or table) per rule; every crash point of an adoption is tried.

mod common;

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;
use lightcraft_cloud_client::home::{
    Home, ImportSummary, Row, State, SyncReport, check_import_note, content_hash_bytes, content_hash_file, content_hash_reader,
};
use lightcraft_cloud_client::{DirRemote, Error};
use serde_json::{Value, json};

const DEV: &str = "laptop";
const MTIME: &str = "2024-05-01T12:00:00Z";
const MTIME_EPOCH: u64 = 1_714_564_800;

fn bytes_of(tag: &str) -> Vec<u8> {
    format!("photo bytes of {tag} ").repeat(50).into_bytes()
}

fn xmp() -> Vec<u8> {
    sc().build()
}

/// One upload: the files go first, the note last.
struct Up {
    device: String,
    folder: String,
    name: String,
    bytes: Vec<u8>,
    sidecar: Option<(String, Vec<u8>)>,
    mtime: String,
}

fn up(folder: &str, name: &str, bytes: &[u8]) -> Up {
    Up { device: DEV.into(), folder: folder.into(), name: name.into(), bytes: bytes.to_vec(), sidecar: None, mtime: MTIME.into() }
}

impl Up {
    fn sidecar(mut self, name: &str, data: Vec<u8>) -> Self {
        self.sidecar = Some((name.into(), data));
        self
    }
    fn device(mut self, d: &str) -> Self {
        self.device = d.into();
        self
    }
    fn mtime(mut self, m: &str) -> Self {
        self.mtime = m.into();
        self
    }
    fn id(&self) -> String {
        content_hash_bytes(&self.bytes)
    }
    fn note(&self) -> Value {
        json!({"format": 1, "id": self.id(), "folder": self.folder, "name": self.name, "size": self.bytes.len(), "mtime": self.mtime,
               "sidecar": self.sidecar.as_ref().map(|s| s.0.clone())})
    }
}

fn put_remote(fx: &Fx, rel: &str, data: &[u8]) {
    let p = fx.remote.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

/// Upload the files of an import (not the note).
fn upload_files(fx: &Fx, u: &Up) {
    let id = u.id();
    put_remote(fx, &format!("imports/{}/{id}/{}", u.device, u.name), &u.bytes);
    if let Some((n, d)) = &u.sidecar {
        put_remote(fx, &format!("imports/{}/{id}/{n}", u.device), d);
    }
}

fn put_note(fx: &Fx, u: &Up) {
    put_remote(fx, &format!("imports/{}/{}.json", u.device, u.id()), u.note().to_string().as_bytes());
}

/// Upload files, then the note. Returns the id.
fn upload(fx: &Fx, u: &Up) -> String {
    upload_files(fx, u);
    put_note(fx, u);
    u.id()
}

fn seed(fx: &Fx) {
    fx.touch("Seed/seed.jpg", b"the photo that was here before", 100);
}

fn sync(fx: &Fx) -> SyncReport {
    fx.home.sync(&mut Fake::default(), false).unwrap()
}

fn result_of(fx: &Fx, id: &str) -> Value {
    let hits: Vec<String> = fx.remote_tree().into_iter().filter(|p| p.starts_with("imports-done/") && p.ends_with(&format!("/{id}.result.json"))).collect();
    assert!(!hits.is_empty(), "no result for {id}: {:?}", fx.remote_tree());
    serde_json::from_slice(&fx.rfile(hits.last().unwrap()).unwrap()).unwrap()
}

fn status_of(fx: &Fx, id: &str) -> String {
    result_of(fx, id)["status"].as_str().unwrap().to_string()
}

fn imports_files(fx: &Fx) -> Vec<String> {
    fx.remote_tree().into_iter().filter(|p| p.starts_with("imports/")).collect()
}

/// The visible files of the photo folder (hidden temp files are not counted).
fn visible(fx: &Fx) -> Vec<String> {
    fx.tree(&fx.src).into_iter().filter(|p| !p.split('/').any(|c| c.starts_with('.'))).collect()
}

fn hidden(fx: &Fx) -> Vec<String> {
    fx.tree(&fx.src).into_iter().filter(|p| p.split('/').any(|c| c.starts_with('.'))).collect()
}

fn read_src(fx: &Fx, rel: &str) -> Vec<u8> {
    std::fs::read(fx.src.join(rel)).unwrap()
}

fn set_margin(fx: &mut Fx, margin: u64) {
    let mut cfg = fx.home.config().clone();
    cfg.import_free_margin = margin;
    let mut home = Home::new(cfg, Arc::new(DirRemote::new(&fx.remote))).unwrap();
    home.set_retry_delay(Duration::ZERO);
    let l = fx.log.clone();
    home.set_logger(move |m| l.lock().unwrap().push(m.to_string()));
    fx.home = home;
}

fn logged(fx: &Fx, what: &str) -> bool {
    fx.lines().iter().any(|l| l.contains(what))
}

// ------------------------------------------------------------------ the content hash

/// A small valid PNG (stored deflate blocks), so the engine's probe accepts it.
fn png(w: u32, h: u32, seed: u8) -> Vec<u8> {
    fn crc(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend(data);
        out.extend(&body);
        out.extend(crc(&body).to_be_bytes());
    }
    let mut raw = Vec::new();
    for y in 0..h {
        raw.push(0u8);
        for x in 0..w {
            raw.extend([(x as u8).wrapping_add(seed), (y as u8).wrapping_mul(3), seed]);
        }
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &v in &raw {
        a = (a + u32::from(v)) % 65521;
        b = (b + a) % 65521;
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, blk) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        z.extend((blk.len() as u16).to_le_bytes());
        z.extend((!(blk.len() as u16)).to_le_bytes());
        z.extend(*blk);
    }
    z.extend(((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[test]
fn the_streamed_content_hash_equals_the_engines_and_a_known_answer() {
    for (i, (w, h)) in [(8, 8), (33, 17), (200, 120)].into_iter().enumerate() {
        let file = png(w, h, i as u8);
        let engine = lightcraft_engine::files::probe_bytes("x.png", &file).expect("the engine reads the test image").content_hash.unwrap();
        assert_eq!(content_hash_bytes(&file), engine, "{w}x{h}");
        if i == 0 {
            assert_eq!(engine, KNOWN_PNG_8X8, "the engine's value for a fixed file");
        }
        // however the stream is cut up
        let mut cursor = std::io::Cursor::new(file.clone());
        assert_eq!(content_hash_reader(&mut cursor).unwrap(), (engine.clone(), file.len() as u64));
        let mut slow = OneByte(&file, 0);
        assert_eq!(content_hash_reader(&mut slow).unwrap().0, engine);
    }
    // known answers, so a change of key or algorithm is caught without the engine too (KNOWN_PNG_8X8 is the engine's own value;
    // the other two are this function's, which the loop above shows to be the engine's algorithm)
    assert_eq!(content_hash_bytes(b""), KNOWN_EMPTY);
    assert_eq!(content_hash_bytes(b"LightCraft"), KNOWN_WORD);
    assert!(content_hash_bytes(b"LightCraft").chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) && content_hash_bytes(b"x").len() == 32);
}

const KNOWN_PNG_8X8: &str = "8934dca486bb4d50e13d9f3a8a89526a";
const KNOWN_EMPTY: &str = "bc3cec50e85d3e34547b206501a212aa";
const KNOWN_WORD: &str = "2117e25bd2576e9f361123c1adf25e4c";

struct OneByte<'a>(&'a [u8], usize);

impl std::io::Read for OneByte<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.1 >= self.0.len() {
            return Ok(0);
        }
        buf[0] = self.0[self.1];
        self.1 += 1;
        Ok(1)
    }
}

#[test]
fn a_big_file_is_hashed_in_pieces_and_equals_the_in_memory_value() {
    let dir = temp("bighash");
    let big: Vec<u8> = (0..(5 * (1 << 20) + 123)).map(|i| (i * 31 % 251) as u8).collect();
    std::fs::write(dir.join("big.bin"), &big).unwrap();
    assert_eq!(content_hash_file(&dir.join("big.bin")).unwrap(), (content_hash_bytes(&big), big.len() as u64));
    let _ = std::fs::remove_dir_all(dir);
}

// ------------------------------------------------------------------ the note, strictly

fn good_note(id: &str) -> Value {
    json!({"format": 1, "id": id, "folder": "261101 - Lisbon", "name": "DSC0001.ARW", "size": 12, "mtime": MTIME, "sidecar": "DSC0001.ARW.xmp"})
}

const ID: &str = "0123456789abcdef0123456789abcdef";

fn check(v: &Value) -> Result<(), String> {
    check_import_note(v.to_string().as_bytes(), ID, &["Exports".to_string()]).map(|_| ())
}

fn with(edits: &[(&str, Value)]) -> Value {
    let mut v = good_note(ID);
    for (k, x) in edits {
        v[*k] = x.clone();
    }
    v
}

#[test]
fn a_good_note_is_accepted_and_unknown_fields_are_ignored() {
    let mut v = good_note(ID);
    v["somethingNew"] = json!([1, 2]);
    let n = check_import_note(v.to_string().as_bytes(), ID, &[]).unwrap();
    assert_eq!((n.folder.as_str(), n.name.as_str(), n.size, n.sidecar.as_deref()), ("261101 - Lisbon", "DSC0001.ARW", 12, Some("DSC0001.ARW.xmp")));
    assert_eq!(n.mtime, UNIX_EPOCH + Duration::from_secs(MTIME_EPOCH));
    // no sidecar (null or absent), a file in the root of the folder, a stem-named sidecar
    assert!(check(&with(&[("sidecar", Value::Null)])).is_ok());
    let mut absent = good_note(ID);
    absent.as_object_mut().unwrap().remove("sidecar");
    assert!(check(&absent).is_ok());
    assert!(check(&with(&[("folder", json!(""))])).is_ok());
    assert!(check(&with(&[("sidecar", json!("DSC0001.xmp"))])).is_ok());
    assert!(check(&with(&[("folder", json!("a/b/c d"))])).is_ok());
}

#[test]
fn bad_ids_and_formats_are_rejected() {
    for id in ["", "ABCDEF0123456789ABCDEF0123456789", "0123456789abcdef0123456789abcde", "0123456789abcdef0123456789abcdef0", "+123456789abcdef0123456789abcdef", "g123456789abcdef0123456789abcdef"] {
        assert!(check_import_note(good_note(id).to_string().as_bytes(), id, &[]).is_err(), "id {id:?}");
        assert!(check_import_note(good_note(id).to_string().as_bytes(), ID, &[]).is_err(), "id {id:?} vs the file's");
    }
    // the note's id must be the one in its file name
    assert!(check_import_note(good_note("ffffffffffffffffffffffffffffffff").to_string().as_bytes(), ID, &[]).is_err());
    for f in [json!(0), json!(2), json!("1"), json!(1.5), Value::Null] {
        assert!(check(&with(&[("format", f.clone())])).is_err(), "format {f}");
    }
    let mut no_format = good_note(ID);
    no_format.as_object_mut().unwrap().remove("format");
    assert!(check(&no_format).is_err());
}

#[test]
fn unsafe_folders_names_and_sidecars_are_rejected() {
    let bad_folders = [
        "/etc",
        "/",
        "../x",
        "a/../b",
        "..",
        "a/..",
        "./a",
        "a/./b",
        "a\\b",
        "C:\\x",
        "a//b",
        "a/",
        "/a",
        "a/\u{0}b",
        "a/b\nc",
        "a\tb",
        "a/\u{7f}",
        "a/\u{85}b",
        "CON",
        "a/aux",
        "a/Nul/b",
        "com1",
        "LPT9/x",
        "con.txt/x",
        "trailing./x",
        "x/trailing.",
        "x/trailing ",
        "a/.hidden",
        ".hidden",
        "Exports",
        "2024/Exports/raw",
        "a:b",
        "a/b*",
        "a?b",
        "a|b",
        "a<b",
        "q\"q",
    ];
    for f in bad_folders {
        assert!(check(&with(&[("folder", json!(f))])).is_err(), "folder {f:?} must be rejected");
    }
    assert!(check(&with(&[("folder", json!("x".repeat(300)))])).is_err(), "a component over 255 bytes");
    let bad_names = [
        "",
        "a/b.jpg",
        "../a.jpg",
        "..",
        ".",
        "/a.jpg",
        "a\\b.jpg",
        "a\u{0}.jpg",
        "a\n.jpg",
        "nul.jpg",
        "COM3.jpg",
        "a.jpg.",
        "a.jpg ",
        ".a.jpg",
        "notes.txt",
        "script.sh",
        "photo",
        "a*.jpg",
        "a:b.jpg",
    ];
    for n in bad_names {
        assert!(check(&with(&[("name", json!(n)), ("sidecar", Value::Null)])).is_err(), "name {n:?} must be rejected");
    }
    let bad_sidecars = ["", "a/DSC0001.ARW.xmp", "../DSC0001.ARW.xmp", "\\DSC0001.ARW.xmp", "DSC0001.ARW.XMP.exe", "other.xmp", "DSC0001.ARW.xmp\n", "dsc0001.xmp", "DSC0001.ARW.txt"];
    for s in bad_sidecars {
        assert!(check(&with(&[("sidecar", json!(s))])).is_err(), "sidecar {s:?} must be rejected");
    }
    assert!(check(&with(&[("sidecar", json!(5))])).is_err());
    assert!(check(&with(&[("size", json!(-1))])).is_err());
    assert!(check(&with(&[("size", json!("12"))])).is_err());
    for m in ["", "yesterday", "2024-05-01", "2024-05-01 12:00:00", "2024-13-01T12:00:00Z", "2024-05-01T25:00:00Z", "1969-12-31T23:59:59Z"] {
        assert!(check(&with(&[("mtime", json!(m))])).is_err(), "mtime {m:?}");
    }
    assert!(check_import_note(b"not json", ID, &[]).is_err());
    assert!(check_import_note(b"[1]", ID, &[]).is_err());
    assert!(check_import_note(&vec![b' '; 100_000], ID, &[]).is_err(), "a note is small");
}

#[test]
fn a_rejected_note_is_moved_to_imports_done_with_its_reason_and_the_sync_goes_on() {
    let fx = Fx::new("rejects");
    seed(&fx);
    // a good one, a path traversal, and a note that is not JSON
    let good = up("Trip", "good.jpg", &bytes_of("good"));
    let evil = up("../../outside", "evil.jpg", &bytes_of("evil"));
    let junk_id = "ee".repeat(16);
    upload(&fx, &good);
    let evil_id = upload(&fx, &evil);
    put_remote(&fx, &format!("imports/{DEV}/{junk_id}.json"), b"{ not json");
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.rejected, r.imports.waiting), (1, 2, 0));
    assert_eq!(status_of(&fx, &good.id()), "added");
    let res = result_of(&fx, &evil_id);
    assert_eq!((res["status"].as_str(), res["path"].is_null(), res["id"].as_str()), (Some("rejected"), true, Some(evil_id.as_str())));
    assert!(res["reason"].as_str().unwrap().contains("folder"), "{res}");
    assert_eq!(status_of(&fx, &junk_id), "rejected");
    // moved, not deleted: the note and the files are under imports-done, none are left in imports
    assert!(imports_files(&fx).is_empty(), "{:?}", imports_files(&fx));
    let done = fx.remote_tree();
    assert!(done.iter().any(|p| p.starts_with("imports-done/") && p.ends_with(&format!("/{DEV}/{evil_id}.json"))));
    assert!(done.iter().any(|p| p.starts_with("imports-done/") && p.ends_with(&format!("/{DEV}/{evil_id}/evil.jpg"))));
    // nothing was written outside, and nothing for the rejects inside
    assert!(!fx.root.join("outside").exists() && !fx.root.parent().unwrap().join("outside").exists());
    assert_eq!(visible(&fx), vec!["Seed/seed.jpg".to_string(), "Trip/good.jpg".to_string()]);
}

#[test]
fn an_excluded_folder_and_a_bad_sidecar_are_rejected_in_a_sync() {
    let fx = Fx::with("rejects2", |c| c.exclude = vec!["Exports".into()]);
    seed(&fx);
    let ex = up("2024/Exports", "a.jpg", &bytes_of("ex"));
    let bad_xmp = up("Trip", "b.jpg", &bytes_of("b")).sidecar("b.xmp", b"this is not xmp".to_vec());
    let huge = up("Trip", "c.jpg", &bytes_of("c")).sidecar("c.xmp", vec![b'x'; 2_100_000]);
    for u in [&ex, &bad_xmp, &huge] {
        upload(&fx, u);
    }
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.rejected), (0, 3));
    for u in [&ex, &bad_xmp, &huge] {
        assert_eq!(status_of(&fx, &u.id()), "rejected");
    }
    assert!(result_of(&fx, &ex.id())["reason"].as_str().unwrap().contains("exclude"));
    assert!(result_of(&fx, &bad_xmp.id())["reason"].as_str().unwrap().contains("sidecar"));
    assert_eq!(visible(&fx), vec!["Seed/seed.jpg".to_string()]);
}

#[cfg(unix)]
#[test]
fn a_symlink_in_the_photo_folder_cannot_lead_outside_it() {
    let fx = Fx::new("symlink");
    seed(&fx);
    let outside = fx.root.join("elsewhere");
    std::fs::create_dir_all(outside.join("deeper")).unwrap();
    std::os::unix::fs::symlink(&outside, fx.src.join("Link")).unwrap();
    std::os::unix::fs::symlink("Real", fx.src.join("Alias")).unwrap(); // (a link that stays inside is fine)
    std::fs::create_dir_all(fx.src.join("Real")).unwrap();
    let a = up("Link", "a.jpg", &bytes_of("a"));
    let b = up("Link/deeper/new", "b.jpg", &bytes_of("b"));
    let c = up("Alias/sub", "c.jpg", &bytes_of("c"));
    for u in [&a, &b, &c] {
        upload(&fx, u);
    }
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.rejected), (1, 2), "{:?}", r.imports.items);
    assert_eq!(status_of(&fx, &a.id()), "rejected");
    assert_eq!(status_of(&fx, &b.id()), "rejected");
    assert!(result_of(&fx, &a.id())["reason"].as_str().unwrap().contains("outside"));
    assert_eq!(status_of(&fx, &c.id()), "added");
    assert!(fx.tree(&outside).is_empty(), "nothing was written outside the photo folder: {:?}", fx.tree(&outside));
    assert_eq!(read_src(&fx, "Real/sub/c.jpg"), bytes_of("c"));
    // a link as the file itself is not overwritten either
    std::os::unix::fs::symlink(outside.join("target.jpg"), fx.src.join("Real/sub/d.jpg")).unwrap();
    let d = up("Real/sub", "d.jpg", &bytes_of("d"));
    upload(&fx, &d);
    let r = sync(&fx);
    assert_eq!(r.imports.added[0].path, "Real/sub/d-2.jpg");
    assert!(!outside.join("target.jpg").exists());
}

// ------------------------------------------------------------------ the happy path

#[test]
fn a_photo_and_its_sidecar_are_adopted_and_published_by_the_same_sync() {
    let fx = Fx::new("adopt");
    seed(&fx);
    let u = up("261101 - Lisbon", "DSC0001.ARW", &bytes_of("lisbon")).sidecar("DSC0001.ARW.xmp", xmp());
    let id = upload(&fx, &u);
    let r = sync(&fx);
    let path = "261101 - Lisbon/DSC0001.ARW";
    assert_eq!(read_src(&fx, path), u.bytes);
    assert_eq!(read_src(&fx, "261101 - Lisbon/DSC0001.ARW.xmp"), xmp());
    assert_eq!(std::fs::metadata(fx.src.join(path)).unwrap().modified().unwrap(), UNIX_EPOCH + Duration::from_secs(MTIME_EPOCH));
    // the report
    let i: &ImportSummary = &r.imports;
    assert_eq!((i.added.len(), i.duplicates, i.rejected, i.waiting), (1, 0, 0, 0));
    assert_eq!((i.added[0].device.as_str(), i.added[0].id.as_str(), i.added[0].path.as_str()), (DEV, id.as_str(), path));
    assert_eq!(i.added[0].sidecar.as_deref(), Some("261101 - Lisbon/DSC0001.ARW.xmp"));
    assert_eq!(i.folders, vec!["261101 - Lisbon".to_string()]);
    // published by this very sync
    assert!(r.plan.new.contains(&path.to_string()), "{:?}", r.plan);
    assert_eq!(fx.rfile(&format!("originals/{path}")).unwrap(), u.bytes);
    assert!(fx.rfile("originals/261101 - Lisbon/DSC0001.ARW.xmp").is_some());
    assert!(fx.manifest()["photos"].as_array().unwrap().iter().any(|p| p["path"] == path));
    // the result, and the layout of imports-done: moved, not deleted
    let res = result_of(&fx, &id);
    assert_eq!(res, json!({"id": id, "status": "added", "path": path, "reason": null}));
    assert!(imports_files(&fx).is_empty());
    let tree = fx.remote_tree();
    for want in [format!("/{DEV}/{id}.json"), format!("/{DEV}/{id}.result.json"), format!("/{DEV}/{id}/DSC0001.ARW"), format!("/{DEV}/{id}/DSC0001.ARW.xmp")] {
        assert!(tree.iter().any(|p| p.starts_with("imports-done/") && p.ends_with(&want)), "{want} in {tree:?}");
    }
    let day = tree.iter().find(|p| p.starts_with("imports-done/")).unwrap().split('/').nth(1).unwrap().to_string();
    assert!(day.len() == 10 && day.as_bytes()[4] == b'-', "{day}");
    // nothing temporary is left in the folder, and a second sync has nothing to do
    assert!(hidden(&fx).is_empty(), "{:?}", hidden(&fx));
    let again = sync(&fx);
    assert_eq!((again.imports.added.len(), again.imports.duplicates), (0, 0));
}

#[test]
fn a_note_is_final_only_when_it_is_there_a_file_with_no_note_is_ignored() {
    let fx = Fx::new("nonote");
    seed(&fx);
    let u = up("Trip", "a.jpg", &bytes_of("a"));
    upload_files(&fx, &u); // (no note yet: the upload is still going)
    let before = imports_files(&fx);
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.waiting, r.imports.rejected), (0, 0, 0));
    assert_eq!(imports_files(&fx), before);
    assert_eq!(fx.home.imports_waiting().unwrap(), 0);
    put_note(&fx, &u);
    assert_eq!(fx.home.imports_waiting().unwrap(), 1);
    let r = sync(&fx);
    assert_eq!(r.imports.added.len(), 1);
}

#[test]
fn an_empty_imports_area_behaves_exactly_as_before() {
    let fx = Fx::new("empty");
    seed(&fx);
    let r = sync(&fx);
    assert_eq!(r.imports, ImportSummary::default());
    assert!(!fx.lines().iter().any(|l| l.starts_with("imports")), "{:?}", fx.lines());
    assert!(fx.remote_tree().iter().all(|p| !p.starts_with("imports")));
    assert_eq!(fx.home.imports_waiting().unwrap(), 0);
    // stray things in imports/ are not imports
    put_remote(&fx, "imports/readme.txt", b"hi");
    put_remote(&fx, &format!("imports/{DEV}/notes.txt"), b"hi");
    put_remote(&fx, "imports/bad dev/x.json", b"{}");
    let before = fx.remote_tree();
    let r = sync(&fx);
    assert_eq!(r.imports, ImportSummary::default());
    assert_eq!(fx.remote_tree(), before);
}

#[test]
fn a_dry_run_reports_and_changes_nothing() {
    let fx = Fx::new("dry");
    seed(&fx);
    sync(&fx);
    upload(&fx, &up("Trip", "a.jpg", &bytes_of("a")));
    let (remote_before, src_before, state_before) = (fx.tree_hash(&fx.remote), fx.tree_hash(&fx.src), fx.tree_hash(&fx.state));
    let r = fx.home.sync(&mut Fake::default(), true).unwrap();
    assert!(r.imports.added.is_empty() && r.imports.items.iter().any(|l| l.contains("a.jpg")), "{:?}", r.imports);
    assert_eq!((fx.tree_hash(&fx.remote), fx.tree_hash(&fx.src), fx.tree_hash(&fx.state)), (remote_before, src_before, state_before));
}

#[test]
fn adopted_photos_do_not_touch_max_delete_and_one_bad_import_stops_nobody() {
    let fx = Fx::with("maxdel", |c| c.max_delete = 0);
    seed(&fx);
    let ids: Vec<String> = (0..3).map(|i| upload(&fx, &up("Trip", &format!("p{i}.jpg"), &bytes_of(&format!("p{i}"))))).collect();
    let bad = up("../x", "bad.jpg", &bytes_of("bad"));
    upload(&fx, &bad);
    let wrong_size = up("Trip", "w.jpg", &bytes_of("w"));
    upload_files(&fx, &wrong_size);
    let mut n = wrong_size.note();
    n["size"] = json!(5);
    put_remote(&fx, &format!("imports/{DEV}/{}.json", wrong_size.id()), n.to_string().as_bytes());
    let incomplete = up("Trip", "inc.jpg", &bytes_of("inc"));
    put_note(&fx, &incomplete); // (note, but the photo is not there yet)
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.rejected, r.imports.waiting), (3, 2, 1), "{:?}", r.imports);
    for id in &ids {
        assert_eq!(status_of(&fx, id), "added");
    }
    assert_eq!(r.photos, 4);
    // a later sync removing nothing is fine too, and max_delete still counts only vanished photos
    std::fs::remove_file(fx.src.join("Trip/p0.jpg")).unwrap();
    let err = fx.home.sync(&mut Fake::default(), false).unwrap_err();
    assert!(err.to_string().contains("max_delete"), "{err}");
}

#[test]
fn imports_come_after_the_inbox_and_before_the_scan() {
    let fx = Fx::new("order");
    seed(&fx);
    upload(&fx, &up("Trip", "a.jpg", &bytes_of("a")));
    sync(&fx);
    let lines = fx.lines();
    let at = |p: &str| lines.iter().position(|l| l.starts_with(p)).unwrap_or_else(|| panic!("no {p} in {lines:?}"));
    assert!(at("inbox:") < at("imports:") && at("imports:") < at("scan:"), "{lines:?}");
}

// ------------------------------------------------------------------ download and verify

#[test]
fn a_size_or_hash_mismatch_is_rejected_and_never_adopted() {
    let fx = Fx::new("verify");
    seed(&fx);
    // the note says 20 bytes, the file has more
    let a = up("Trip", "a.jpg", &bytes_of("a"));
    upload_files(&fx, &a);
    let mut n = a.note();
    n["size"] = json!(20);
    put_remote(&fx, &format!("imports/{DEV}/{}.json", a.id()), n.to_string().as_bytes());
    // the right size and the wrong bytes (the id is the hash of other bytes)
    let b = up("Trip", "b.jpg", &bytes_of("b"));
    let mut tampered = b.bytes.clone();
    tampered[3] ^= 0xff;
    put_remote(&fx, &format!("imports/{DEV}/{}/b.jpg", b.id()), &tampered);
    put_note(&fx, &b);
    // an upload cut short
    let c = up("Trip", "c.jpg", &bytes_of("c"));
    put_remote(&fx, &format!("imports/{DEV}/{}/c.jpg", c.id()), &c.bytes[..c.bytes.len() / 2]);
    put_note(&fx, &c);
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.rejected, r.imports.waiting), (0, 3, 0), "{:?}", r.imports.items);
    for u in [&a, &b, &c] {
        let res = result_of(&fx, &u.id());
        assert_eq!(res["status"], "rejected");
        assert!(res["reason"].as_str().unwrap().contains("match"), "{res}");
    }
    assert_eq!(visible(&fx), vec!["Seed/seed.jpg".to_string()]);
    assert!(hidden(&fx).is_empty());
}

#[test]
fn files_not_there_yet_wait_a_transient_error_rejects_nothing_and_a_long_incomplete_import_is_rejected() {
    let (fx, spy) = Fx::spied("wait", |_| {});
    seed(&fx);
    let u = up("Trip", "a.jpg", &bytes_of("a")).sidecar("a.xmp", xmp());
    put_note(&fx, &u); // (the note is there, the files are not: an inconsistent remote)
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.rejected, r.imports.waiting), (0, 0, 1));
    assert!(logged(&fx, "imports:") && logged(&fx, "waiting"), "{:?}", fx.lines());
    assert!(fx.rfile(&format!("imports/{DEV}/{}.json", u.id())).is_some());
    assert_eq!(fx.home.imports_waiting().unwrap(), 1);
    // only the photo arrives: the sidecar the note names is still missing
    put_remote(&fx, &format!("imports/{DEV}/{}/a.jpg", u.id()), &u.bytes);
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.waiting), (0, 1));
    assert_eq!(visible(&fx), vec!["Seed/seed.jpg".to_string()], "the photo is not adopted without the sidecar the note names");
    // everything is there, but the network fails: still not rejected
    put_remote(&fx, &format!("imports/{DEV}/{}/a.xmp", u.id()), &xmp());
    spy.fail_gets.store(100, Ordering::SeqCst);
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.rejected, r.imports.waiting), (0, 0, 1), "{:?}", r.imports);
    assert!(fx.rfile(&format!("imports/{DEV}/{}.json", u.id())).is_some());
    spy.fail_gets.store(0, Ordering::SeqCst);
    *spy.fail_list_of.lock().unwrap() = Some("imports".into());
    let r = sync(&fx); // (the listing fails: the sync goes on, nothing is rejected)
    assert_eq!((r.imports.added.len(), r.imports.rejected), (0, 0));
    assert!(!r.imports.errors.is_empty());
    *spy.fail_list_of.lock().unwrap() = None;
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.waiting), (1, 0));

    // an import that stays incomplete for longer than the limit is rejected
    let w = up("Trip", "w.jpg", &bytes_of("w"));
    put_note(&fx, &w);
    sync(&fx);
    assert!(fx.rfile(&format!("imports/{DEV}/{}.json", w.id())).is_some());
    let mut fx = fx;
    let later = SystemTime::now() + Duration::from_secs(47 * 3600);
    fx.home.set_clock(move || later);
    let r = sync(&fx);
    assert_eq!((r.imports.rejected, r.imports.waiting), (0, 1), "47 hours is not yet too long");
    let later = SystemTime::now() + Duration::from_secs(49 * 3600);
    fx.home.set_clock(move || later);
    let r = sync(&fx);
    assert_eq!((r.imports.rejected, r.imports.waiting), (1, 0));
    let res = result_of(&fx, &w.id());
    assert_eq!(res["status"], "rejected");
    assert!(res["reason"].as_str().unwrap().contains("48"), "{res}");
    let _ = &mut fx;
}

// ------------------------------------------------------------------ duplicates and names

#[test]
fn an_id_the_library_already_knows_is_a_duplicate_and_nothing_is_written() {
    let fx = Fx::new("dup-state");
    seed(&fx);
    let u = up("Trip", "a.jpg", &bytes_of("a")).sidecar("a.xmp", xmp());
    let mut st = State::open(&fx.state, false).unwrap();
    st.put("Old/place/a_original.jpg", Row { size: u.bytes.len() as u64, status: "ok".into(), id: Some(u.id()), ..Row::default() });
    st.commit().unwrap();
    drop(st);
    upload(&fx, &u);
    let before = visible(&fx);
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.duplicates), (0, 1));
    let res = result_of(&fx, &u.id());
    assert_eq!((res["status"].as_str(), res["path"].as_str()), (Some("duplicate"), Some("Old/place/a_original.jpg")));
    assert_eq!(visible(&fx), before);
    assert!(imports_files(&fx).is_empty());
}

#[test]
fn identical_bytes_already_at_the_target_are_a_duplicate_with_no_write() {
    let fx = Fx::new("dup-file");
    let u = up("Trip", "a.jpg", &bytes_of("a")).sidecar("a.xmp", xmp());
    let existing = fx.touch("Trip/a.jpg", &u.bytes, 500);
    fx.touch("Trip/a.xmp", b"<mine/>", 500);
    seed(&fx);
    let mtime = std::fs::metadata(&existing).unwrap().modified().unwrap();
    upload(&fx, &u);
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.duplicates), (0, 1));
    assert_eq!(result_of(&fx, &u.id())["path"], "Trip/a.jpg");
    assert_eq!(status_of(&fx, &u.id()), "duplicate");
    assert_eq!(std::fs::metadata(&existing).unwrap().modified().unwrap(), mtime, "untouched");
    assert_eq!(read_src(&fx, "Trip/a.xmp"), b"<mine/>".to_vec(), "the sidecar there is never replaced");
    assert_eq!(visible(&fx).len(), 3);
}

#[test]
fn the_same_photo_from_two_devices_is_added_once() {
    let fx = Fx::new("dup-devices");
    seed(&fx);
    let a = up("Trip", "a.jpg", &bytes_of("a")).device("laptop");
    let b = up("Elsewhere", "copy of a.jpg", &bytes_of("a")).device("tablet");
    upload(&fx, &a);
    upload(&fx, &b);
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.duplicates), (1, 1));
    assert_eq!(r.imports.added[0].path, "Trip/a.jpg");
    assert_eq!(visible(&fx).len(), 2);
    let result_for = |device: &str| -> Value {
        let p = fx.remote_tree().into_iter().find(|p| p.starts_with("imports-done/") && p.ends_with(&format!("/{device}/{}.result.json", a.id()))).unwrap();
        serde_json::from_slice(&fx.rfile(&p).unwrap()).unwrap()
    };
    assert_eq!(result_for("laptop")["status"], "added");
    assert_eq!((result_for("tablet")["status"].as_str(), result_for("tablet")["path"].as_str()), (Some("duplicate"), Some("Trip/a.jpg")));
}

#[test]
fn a_target_with_other_bytes_is_never_overwritten_a_numbered_name_is_used_and_the_sidecar_follows() {
    let fx = Fx::new("rename");
    seed(&fx);
    let theirs = fx.touch("Trip/DSC0001.ARW", b"somebody else's photo", 500);
    fx.touch("Trip/DSC0001.xmp", b"<theirs/>", 500);
    // stem-named sidecar
    let one = up("Trip", "DSC0001.ARW", &bytes_of("one")).sidecar("DSC0001.xmp", xmp());
    let r = sync_with(&fx, &[&one]);
    assert_eq!(r.imports.added[0].path, "Trip/DSC0001-2.ARW");
    assert_eq!(r.imports.added[0].sidecar.as_deref(), Some("Trip/DSC0001-2.xmp"));
    assert_eq!(result_of(&fx, &one.id())["path"], "Trip/DSC0001-2.ARW", "the other PC learns the final path from the result");
    assert_eq!(read_src(&fx, "Trip/DSC0001-2.ARW"), one.bytes);
    assert_eq!(read_src(&fx, "Trip/DSC0001-2.xmp"), xmp());
    // file-named sidecar, and the next free number
    let two = up("Trip", "DSC0001.ARW", &bytes_of("two")).sidecar("DSC0001.ARW.xmp", xmp());
    let r = sync_with(&fx, &[&two]);
    assert_eq!(r.imports.added[0].path, "Trip/DSC0001-3.ARW");
    assert_eq!(r.imports.added[0].sidecar.as_deref(), Some("Trip/DSC0001-3.ARW.xmp"));
    assert_eq!(read_src(&fx, "Trip/DSC0001-3.ARW.xmp"), xmp());
    // no sidecar at all
    let three = up("Trip", "DSC0001.ARW", &bytes_of("three"));
    let r = sync_with(&fx, &[&three]);
    assert_eq!((r.imports.added[0].path.as_str(), r.imports.added[0].sidecar.clone()), ("Trip/DSC0001-4.ARW", None));
    // the originals are untouched
    assert_eq!(std::fs::read(&theirs).unwrap(), b"somebody else's photo".to_vec());
    assert_eq!(read_src(&fx, "Trip/DSC0001.xmp"), b"<theirs/>".to_vec());
    // a numbered name whose sidecar name is taken is skipped too
    fx.touch("Trip/DSC0002-2.xmp", b"<orphan/>", 500);
    fx.touch("Trip/DSC0002.ARW", b"another", 500);
    let four = up("Trip", "DSC0002.ARW", &bytes_of("four")).sidecar("DSC0002.xmp", xmp());
    let r = sync_with(&fx, &[&four]);
    assert_eq!(r.imports.added[0].path, "Trip/DSC0002-3.ARW");
    assert_eq!(read_src(&fx, "Trip/DSC0002-2.xmp"), b"<orphan/>".to_vec());
    // a file name differing only in case counts as taken
    fx.touch("Trip/IMG_9.JPG", b"upper", 500);
    let five = up("Trip", "img_9.jpg", &bytes_of("five"));
    let r = sync_with(&fx, &[&five]);
    assert_eq!(r.imports.added[0].path, "Trip/img_9-2.jpg");
}

fn sync_with(fx: &Fx, ups: &[&Up]) -> SyncReport {
    for u in ups {
        upload(fx, u);
    }
    sync(fx)
}

#[test]
fn an_existing_sidecar_is_never_replaced_and_the_photo_still_arrives() {
    let fx = Fx::new("keep-sidecar");
    seed(&fx);
    fx.touch("Trip/a.xmp", b"<the user's own edits/>", 500);
    let u = up("Trip", "a.jpg", &bytes_of("a")).sidecar("a.xmp", xmp());
    let r = sync_with(&fx, &[&u]);
    assert_eq!(r.imports.added[0].path, "Trip/a.jpg");
    assert_eq!(read_src(&fx, "Trip/a.xmp"), b"<the user's own edits/>".to_vec());
    let res = result_of(&fx, &u.id());
    assert_eq!(res["status"], "added");
    assert!(res["reason"].as_str().unwrap().contains("sidecar"), "{res}");
}

// ------------------------------------------------------------------ disk space

#[test]
fn too_little_free_space_leaves_the_import_waiting_with_a_clear_error() {
    let mut fx = Fx::new("space");
    seed(&fx);
    set_margin(&mut fx, u64::MAX / 2);
    let u = up("Trip", "a.jpg", &bytes_of("a"));
    upload(&fx, &u);
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.rejected, r.imports.waiting), (0, 0, 1));
    assert!(r.imports.errors.iter().any(|e| e.contains("free") && e.contains("a.jpg")), "{:?}", r.imports.errors);
    assert_eq!(visible(&fx), vec!["Seed/seed.jpg".to_string()]);
    assert!(hidden(&fx).is_empty(), "{:?}", hidden(&fx));
    assert!(fx.rfile(&format!("imports/{DEV}/{}.json", u.id())).is_some(), "still waiting");
    set_margin(&mut fx, 0);
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.waiting), (1, 0));
}

// ------------------------------------------------------------------ crashes

const CRASH_POINTS: [&str; 8] = ["before_write", "after_sidecar", "after_temp", "after_link", "after_photo", "after_result", "after_first_move", "after_data_moved"];

#[test]
fn a_crash_at_any_point_is_safe_and_a_rerun_finishes_with_no_second_copy() {
    for point in CRASH_POINTS {
        for conflict in [false, true] {
            let mut fx = Fx::new(&format!("crash-{point}-{conflict}"));
            seed(&fx);
            if conflict {
                fx.touch("Trip/a.jpg", b"another photo with the same name", 500);
            }
            let u = up("Trip", "a.jpg", &bytes_of("a")).sidecar("a.xmp", xmp());
            let id = upload(&fx, &u);
            let want = if conflict { "Trip/a-2.jpg" } else { "Trip/a.jpg" };
            fx.home.set_import_crash_point(Some(point));
            let died = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = fx.home.sync(&mut Fake::default(), false);
            }));
            assert!(died.is_err(), "{point}: the crash point was not reached");
            fx.home.set_import_crash_point(None);
            // whatever the state, the photo folder holds no half-written visible file
            for f in visible(&fx) {
                if f == "Trip/a.jpg" && conflict {
                    continue;
                }
                let bytes = read_src(&fx, &f);
                assert!(bytes == u.bytes || bytes == xmp() || f.starts_with("Seed/") || f == "Trip/a.jpg", "{point}: {f} is partial or foreign");
            }
            let r = sync(&fx);
            assert_eq!(r.imports.rejected, 0, "{point}");
            let copies = visible(&fx).into_iter().filter(|f| read_src(&fx, f) == u.bytes).collect::<Vec<_>>();
            assert_eq!(copies, vec![want.to_string()], "{point} conflict={conflict}: exactly one copy");
            assert_eq!(read_src(&fx, &want.replace(".jpg", ".xmp")), xmp(), "{point}: the sidecar is there and matches the photo");
            assert!(imports_files(&fx).is_empty(), "{point}: {:?}", imports_files(&fx));
            let res = result_of(&fx, &id);
            assert!(matches!(res["status"].as_str(), Some("added") | Some("duplicate")), "{point}: {res}");
            assert_eq!(res["path"], want, "{point}");
            // and it was published
            assert!(fx.rfile(&format!("originals/{want}")).is_some(), "{point}");
            assert_eq!(visible(&fx).len(), if conflict { 4 } else { 3 }, "{point}: {:?}", visible(&fx));
            assert!(hidden(&fx).is_empty(), "{point}: the temporary file of the crashed pass is cleaned up: {:?}", hidden(&fx));
        }
    }
}

#[test]
fn the_sidecar_is_written_before_the_photo() {
    let mut fx = Fx::new("sidecar-first");
    seed(&fx);
    let u = up("Trip", "a.jpg", &bytes_of("a")).sidecar("a.xmp", xmp());
    upload(&fx, &u);
    fx.home.set_import_crash_point(Some("after_sidecar"));
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = fx.home.sync(&mut Fake::default(), false);
    }));
    assert_eq!(visible(&fx), vec!["Seed/seed.jpg".to_string(), "Trip/a.xmp".to_string()], "the sidecar is there, the photo is not yet");
    fx.home.set_import_crash_point(Some("after_photo"));
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = fx.home.sync(&mut Fake::default(), false);
    }));
    assert_eq!(visible(&fx).len(), 3);
}

#[cfg(unix)]
#[test]
fn a_failure_to_write_leaves_the_import_waiting_and_is_not_a_rejection() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new("blocked");
    seed(&fx);
    fx.touch("Trip/other.jpg", b"already here", 500);
    let dir = fx.src.join("Trip");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap(); // (nothing can be created in it)
    let u = up("Trip", "a.jpg", &bytes_of("a"));
    upload(&fx, &u);
    let r = sync(&fx);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    if r.imports.added.len() == 1 {
        return; // (running as root: the permission does not stop anything)
    }
    assert_eq!((r.imports.added.len(), r.imports.rejected, r.imports.waiting), (0, 0, 1), "{:?}", r.imports);
    assert!(!r.imports.errors.is_empty());
    assert!(hidden(&fx).is_empty(), "{:?}", hidden(&fx));
    assert!(fx.rfile(&format!("imports/{DEV}/{}.json", u.id())).is_some());
    let r = sync(&fx);
    assert_eq!(r.imports.added.len(), 1);
}

#[test]
fn a_file_where_a_folder_should_be_makes_the_import_wait() {
    let fx = Fx::new("file-in-way");
    seed(&fx);
    fx.touch("Trip.d", b"i am a file, not a folder", 500);
    let u = up("Trip.d", "a.jpg", &bytes_of("a"));
    upload(&fx, &u);
    let r = sync(&fx);
    assert_eq!((r.imports.added.len(), r.imports.rejected, r.imports.waiting), (0, 0, 1), "{:?}", r.imports);
    assert!(r.imports.errors.iter().any(|e| e.contains("not a folder")), "{:?}", r.imports.errors);
}

#[test]
fn a_failure_while_filing_the_import_away_is_picked_up_by_the_next_sync() {
    let (fx, spy) = Fx::spied("filing", |_| {});
    seed(&fx);
    let u = up("Trip", "a.jpg", &bytes_of("a")).sidecar("a.xmp", xmp());
    let id = upload(&fx, &u);
    *spy.fail_move_to.lock().unwrap() = Some("imports-done".into());
    let r = sync(&fx);
    assert_eq!((r.imports.waiting, r.imports.rejected), (1, 0), "{:?}", r.imports);
    assert_eq!(read_src(&fx, "Trip/a.jpg"), u.bytes, "the photo is safe on disk");
    *spy.fail_move_to.lock().unwrap() = None;
    let r = sync(&fx);
    assert_eq!(r.imports.waiting, 0);
    assert!(matches!(status_of(&fx, &id).as_str(), "added" | "duplicate"));
    assert!(imports_files(&fx).is_empty());
    assert_eq!(visible(&fx).iter().filter(|f| read_src(&fx, f) == u.bytes).count(), 1);
}

// ------------------------------------------------------------------ the mtime

#[test]
fn the_mtime_comes_from_the_note_and_never_from_the_future() {
    let fx = Fx::new("mtime");
    seed(&fx);
    let future = up("Trip", "future.jpg", &bytes_of("future")).mtime("2999-01-01T00:00:00Z");
    let fractional = up("Trip", "frac.jpg", &bytes_of("frac")).mtime("2024-05-01T12:00:00.250Z");
    let r = sync_with(&fx, &[&future, &fractional]);
    assert_eq!(r.imports.added.len(), 2);
    let m = |p: &str| std::fs::metadata(fx.src.join(p)).unwrap().modified().unwrap();
    assert!(m("Trip/future.jpg") <= SystemTime::now(), "clamped to now");
    assert_eq!(m("Trip/frac.jpg").duration_since(UNIX_EPOCH).unwrap().as_secs(), MTIME_EPOCH);
    // a photo whose mtime is clamped to now is still published by this sync
    assert!(fx.rfile("originals/Trip/future.jpg").is_some());
}

// ------------------------------------------------------------------ the cheap check, status, purge

#[test]
fn imports_waiting_counts_committed_notes_only_and_status_shows_it() {
    let fx = Fx::new("waiting");
    seed(&fx);
    upload(&fx, &up("Trip", "a.jpg", &bytes_of("a")));
    upload(&fx, &up("Trip", "b.jpg", &bytes_of("b")).device("tablet"));
    upload_files(&fx, &up("Trip", "c.jpg", &bytes_of("c"))); // (no note)
    put_remote(&fx, &format!("imports/{DEV}/{}.result.json", "dd".repeat(16)), b"{}");
    put_remote(&fx, &format!("imports/{DEV}/notes.json"), b"{}"); // (a note whose name is not an id still is a note: it is rejected)
    assert_eq!(fx.home.imports_waiting().unwrap(), 3);
    assert_eq!(fx.home.status().unwrap().imports_pending, Some(3));
    assert_eq!(fx.home.inbox_waiting().unwrap(), 0, "phone edits are counted apart");
    let (fx2, spy) = Fx::spied("waiting2", |_| {});
    *spy.fail_list_of.lock().unwrap() = Some("imports".into());
    assert!(fx2.home.imports_waiting().is_err());
    // the poll lists once and reads no file
    let (fx3, spy3) = Fx::spied("waiting3", |_| {});
    upload(&fx3, &up("Trip", "a.jpg", &bytes_of("a")));
    let _ = spy3.puts();
    assert_eq!(fx3.home.imports_waiting().unwrap(), 1);
    assert!(spy3.puts().is_empty() && spy3.moves().is_empty());
}

#[test]
fn the_weekly_purge_treats_imports_done_like_inbox_done_with_the_same_backup_check() {
    let fx = Fx::new("purge");
    let day = |secs: u64| lightcraft_cloud_client::home::utc_iso(UNIX_EPOCH + Duration::from_secs(secs))[..10].to_string();
    let now = 1_793_000_000u64;
    let (old, new) = (day(now - 40 * 86_400), day(now - 2 * 86_400));
    for d in [&old, &new] {
        put_remote(&fx, &format!("imports-done/{d}/{DEV}/{}.json", "aa".repeat(16)), b"{}");
        put_remote(&fx, &format!("imports-done/{d}/{DEV}/{}/a.jpg", "aa".repeat(16)), b"x");
        put_remote(&fx, &format!("inbox-done/{d}/phone/x.xmp"), b"x");
    }
    put_remote(&fx, &format!("imports/{DEV}/{}.json", "bb".repeat(16)), b"{}"); // (waiting imports are never purged)
    let mut fx = fx;
    fx.home.set_clock(move || UNIX_EPOCH + Duration::from_secs(now));
    // no recent backup: nothing is deleted
    let err = fx.home.purge_history(30, 100.0).unwrap_err();
    assert!(matches!(err, Error::Invalid(_)));
    assert!(fx.remote_tree().iter().any(|p| p.starts_with(&format!("imports-done/{old}/"))));
    let r = fx.home.purge_history(30, 1.0).unwrap();
    assert_eq!(r.folders, 2, "one old day in inbox-done and one in imports-done");
    let tree = fx.remote_tree();
    assert!(!tree.iter().any(|p| p.starts_with(&format!("imports-done/{old}/"))));
    assert!(tree.iter().any(|p| p.starts_with(&format!("imports-done/{new}/"))));
    assert!(tree.iter().any(|p| p.starts_with("imports/")));
    let _ = &mut fx;
}

#[test]
fn the_hash_of_what_was_adopted_is_the_id_the_manifest_will_use() {
    // (the engine gives the same value as `content_hash`, see the first test), so the next sync knows the photo by the note's id
    let fx = Fx::new("known-after");
    seed(&fx);
    let u = up("Trip", "a.jpg", &bytes_of("a"));
    sync_with(&fx, &[&u]);
    assert_eq!(content_hash_file(&fx.src.join("Trip/a.jpg")).unwrap(), (u.id(), u.bytes.len() as u64));
    // the same upload again (a retry by the other PC) is a duplicate of what the folder has, even if the state forgot it
    std::fs::remove_file(fx.state.join("state.json")).unwrap();
    let again = up("Trip", "a.jpg", &bytes_of("a")).device("tablet");
    let r = sync_with(&fx, &[&again]);
    assert_eq!((r.imports.added.len(), r.imports.duplicates), (0, 1));
    assert_eq!(visible(&fx).len(), 2);
}
