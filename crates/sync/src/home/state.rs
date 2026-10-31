//! What the home side remembers between runs, in its state folder:
//!
//! ```text
//! <state>/state.json          what was published per photo (the Python tool's state.sqlite)
//! <state>/sidecar-versions/   every sidecar version published or received, <sha16>.xmp (the merge's common ancestors)
//! <state>/history/<date>/     sidecars replaced by phone edits, and phone edits that lost
//! <state>/mobile/             the staged thumbnails, smart previews and manifest, as uploaded
//! <state>/logs/, lock
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::{sha256_hex, write_atomic};
use crate::{Error, Result};

/// What the renderer reported about a photo (and the manifest repeats).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Meta {
    pub captured: Option<String>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub rating: Option<i64>,
    pub flag: Option<String>,
    pub label: Option<String>,
    #[serde(rename = "previewKey")]
    pub preview_key: Option<String>,
}

/// One tracked source photo.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Row {
    pub size: u64,
    pub mtime_ns: i64,
    pub sidecar: Option<String>,
    pub sidecar_hash: Option<String>,
    /// The content hash (the manifest's `id`).
    pub id: Option<String>,
    pub meta: Meta,
    /// `ok`, `dup` (same bytes as another photo), `failed`, or `retry`.
    pub status: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct File {
    format: u32,
    files: BTreeMap<String, Row>,
    kv: BTreeMap<String, String>,
    /// remote path -> (size, mtime_ns) of the local file last uploaded there
    uploaded: BTreeMap<String, (u64, i64)>,
}

/// The state folder of one home library.
pub struct State {
    dir: PathBuf,
    file: File,
    readonly: bool,
}

impl State {
    /// `readonly`: nothing is created or written (a dry run).
    pub fn open(dir: &Path, readonly: bool) -> Result<Self> {
        let path = dir.join("state.json");
        let file = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| Error::Format(format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File { format: 1, ..File::default() },
            Err(e) => return Err(Error::io(path.display(), e)),
        };
        if !readonly {
            std::fs::create_dir_all(dir.join("logs")).map_err(|e| Error::io(dir.display(), e))?;
        }
        Ok(State { dir: dir.to_path_buf(), file, readonly })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn files(&self) -> &BTreeMap<String, Row> {
        &self.file.files
    }

    pub fn put(&mut self, path: &str, row: Row) {
        self.file.files.insert(path.to_string(), row);
    }

    pub fn delete(&mut self, path: &str) {
        self.file.files.remove(path);
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.file.kv.get(k).map(String::as_str)
    }

    pub fn set(&mut self, k: &str, v: &str) {
        self.file.kv.insert(k.to_string(), v.to_string());
    }

    pub fn uploaded(&self, remote_path: &str) -> Option<(u64, i64)> {
        self.file.uploaded.get(remote_path).copied()
    }

    pub fn set_uploaded(&mut self, remote_path: &str, size: u64, mtime_ns: i64) {
        self.file.uploaded.insert(remote_path.to_string(), (size, mtime_ns));
    }

    pub fn forget_uploaded(&mut self, remote_path: &str) {
        self.file.uploaded.remove(remote_path);
    }

    pub fn commit(&self) -> Result<()> {
        if self.readonly {
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(&self.file).map_err(|e| Error::Format(e.to_string()))?;
        write_atomic(&self.dir.join("state.json"), &bytes)
    }
}

/// The sidecar versions kept as merge ancestors: `<dir>/<sha16>.xmp`.
#[derive(Debug, Clone)]
pub struct VersionStore {
    dir: PathBuf,
}

impl VersionStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        VersionStore { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Keep the bytes of one sidecar version (once). The store is a convenience: without a version, a conflict falls back
    /// to newest-wins, so a failed write is only logged.
    pub fn store(&self, data: &[u8], hash: &str) {
        let f = self.dir.join(format!("{hash}.xmp"));
        if !f.exists()
            && let Err(e) = write_atomic(&f, data)
        {
            log::warn!("cannot keep sidecar version {hash}: {e}");
        }
    }

    /// The stored version with this hash, or None (never stored, purged, or damaged).
    pub fn load(&self, hash: &str) -> Option<Vec<u8>> {
        let data = std::fs::read(self.dir.join(format!("{hash}.xmp"))).ok()?;
        (sha256_hex(&data)[..16] == *hash).then_some(data)
    }
}

impl VersionStore {
    /// Delete stored versions last written before `cutoff`, except the hashes in `keep` (the sidecars the state lists now).
    /// Leftover temporary files of an interrupted write go too. Returns how many were removed.
    pub fn purge(&self, cutoff: std::time::SystemTime, keep: &std::collections::HashSet<String>) -> usize {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return 0 };
        let mut n = 0;
        for e in rd.flatten() {
            let path = e.path();
            let keep_it =
                path.extension().is_some_and(|x| x == "xmp") && path.file_stem().is_some_and(|s| keep.contains(&s.to_string_lossy().into_owned()));
            let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < cutoff);
            if !keep_it && old && std::fs::remove_file(&path).is_ok() {
                n += 1;
            }
        }
        n
    }
}

/// Keep a replaced sidecar or a losing phone edit: `<state>/history/<date>/<rel>.<tag>.xmp`.
pub fn save_history(state_dir: &Path, date: &str, rel: &str, data: &[u8], tag: &str) -> Result<PathBuf> {
    let f = state_dir.join("history").join(date).join(format!("{rel}.{tag}.xmp"));
    write_atomic(&f, data)?;
    Ok(f)
}

/// One run at a time: an exclusive lock on `<state>/lock`, released when dropped.
pub struct RunLock {
    _file: std::fs::File,
}

impl RunLock {
    /// `None` for a dry run when there is no lock file yet (nothing is created).
    pub fn acquire(state_dir: &Path, dry: bool) -> Result<Option<Self>> {
        let path = state_dir.join("lock");
        let file = if dry {
            if !path.exists() {
                return Ok(None);
            }
            std::fs::File::open(&path)
        } else {
            std::fs::create_dir_all(state_dir).map_err(|e| Error::io(state_dir.display(), e))?;
            std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)
        }
        .map_err(|e| Error::io(path.display(), e))?;
        file.try_lock().map_err(|_| Error::Invalid("another run holds the lock".into()))?;
        Ok(Some(RunLock { _file: file }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips() {
        let d = std::env::temp_dir().join(format!("lc-home-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut s = State::open(&d, false).unwrap();
        s.put("a.jpg", Row { size: 3, status: "ok".into(), id: Some("x".into()), ..Row::default() });
        s.set("k", "v");
        s.set_uploaded("originals/a.jpg", 3, 7);
        s.commit().unwrap();
        let s2 = State::open(&d, true).unwrap();
        assert_eq!(s2.files()["a.jpg"].size, 3);
        assert_eq!((s2.get("k"), s2.uploaded("originals/a.jpg")), (Some("v"), Some((3, 7))));
        let _ = std::fs::remove_dir_all(&d);
    }
}
