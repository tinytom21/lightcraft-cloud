//! LightCraft's content hash (the `id` of a photo everywhere), computed from a stream without the engine.
//!
//! The same algorithm and key as the engine's `lightcraft_preview::hash_bytes` (`crates/preview/src/hash.rs` in LightCraft):
//! SipHash-1-3 with a 128-bit output and the fixed key below, over the file's bytes, shown as 32 lower-case hex digits. A test
//! in `tests/home_import.rs` compares it with the engine's value (the engine is a dev-dependency only) and with a known answer.

use std::hash::Hasher;
use std::io::Read;
use std::path::Path;

use siphasher::sip128::{Hasher128 as _, SipHasher13};

use crate::{Error, Result};

/// The engine's key (the ASCII of "LightCra" and "ft Conte").
const KEY: (u64, u64) = (0x4c69_6768_7443_7261, 0x6674_2043_6f6e_7465);

/// A hash being fed in pieces; the result does not depend on how the bytes are cut up.
#[derive(Clone)]
pub struct ContentHasher(SipHasher13);

impl Default for ContentHasher {
    fn default() -> Self {
        ContentHasher(SipHasher13::new_with_keys(KEY.0, KEY.1))
    }
}

impl ContentHasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.write(bytes);
    }

    /// The 32 hex digits.
    pub fn finish(&self) -> String {
        format!("{:032x}", self.0.finish128().as_u128())
    }
}

/// The content hash of bytes in memory.
pub fn content_hash_bytes(bytes: &[u8]) -> String {
    let mut h = ContentHasher::new();
    h.update(bytes);
    h.finish()
}

/// The content hash and the length of everything `r` yields, read in 1 MiB pieces.
pub fn content_hash_reader(r: &mut impl Read) -> std::io::Result<(String, u64)> {
    let mut h = ContentHasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let k = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((h.finish(), n))
}

/// The content hash and the size of a file, streamed (a large file is never held in memory).
pub fn content_hash_file(path: &Path) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(path).map_err(|e| Error::io(path.display(), e))?;
    content_hash_reader(&mut f).map_err(|e| Error::io(path.display(), e))
}
