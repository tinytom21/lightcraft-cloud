//! Where the library lives: any rclone remote ([`crate::RcloneRemote`]) or a plain folder ([`DirRemote`]).

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// One file or folder of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    /// Relative to the listed folder, `/`-separated.
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Storage the library's files are read from and (the inbox only) written to. Paths are relative to
/// the remote's root, `/`-separated, without a leading slash. Methods may be called from several
/// threads at once.
pub trait Remote: Send + Sync {
    /// The entries of folder `dir` (`""` = the root), below it too when `recursive`. A folder that is
    /// not there is [`Error::NotFound`].
    fn list(&self, dir: &str, recursive: bool) -> Result<Vec<RemoteEntry>>;
    /// Copy the remote file `path` to the local file `local` (replacing it; the caller makes the
    /// write atomic by passing a temporary name).
    fn get(&self, path: &str, local: &Path) -> Result<()>;
    /// Copy the local file to `path` on the remote (creating folders as needed).
    fn put(&self, local: &Path, path: &str) -> Result<()>;
    /// Move (rename) the remote file `from` to `to`, creating folders as needed and replacing what is at `to`. Only the
    /// home side (publishing, the inbox) needs it; a remote that cannot says so.
    fn move_file(&self, from: &str, to: &str) -> Result<()> {
        Err(Error::Invalid(format!("this remote cannot move {from} to {to}")))
    }
    /// Delete the folder `dir` and everything in it. Only the home side (purging old history) needs it.
    fn purge_dir(&self, dir: &str) -> Result<()> {
        Err(Error::Invalid(format!("this remote cannot delete the folder {dir}")))
    }
    /// Remove the folder `dir` if it is empty, and say nothing if it is not or is not there (tidying only, so an error is never
    /// worth failing for). A remote that cannot leaves the folder.
    fn remove_empty_dir(&self, _dir: &str) -> Result<()> {
        Ok(())
    }
    /// Bytes moved so far by the transfers running right now (all threads), if the remote can say that cheaply; `None` if it cannot. Read
    /// from another thread while a `get` or `put` is blocking, so the app can show that a big file is moving. Only for progress: nothing
    /// depends on it.
    fn bytes_in_flight(&self) -> Option<u64> {
        None
    }
    /// The entry at `path`, if there is one.
    fn stat(&self, path: &str) -> Result<Option<RemoteEntry>> {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        match self.list(dir, false) {
            Ok(v) => Ok(v.into_iter().find(|e| e.path == name)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// A folder on this device standing in for the remote (tests, a synced folder).
#[derive(Debug, Clone)]
pub struct DirRemote {
    root: PathBuf,
}

impl DirRemote {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DirRemote { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn at(&self, path: &str) -> Result<PathBuf> {
        let mut p = self.root.clone();
        for part in path.split('/').filter(|c| !c.is_empty()) {
            if part == ".." || part == "." {
                return Err(Error::Invalid(format!("bad remote path {path}")));
            }
            p.push(part);
        }
        Ok(p)
    }
}

fn walk(base: &Path, dir: &Path, recursive: bool, out: &mut Vec<RemoteEntry>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let md = e.metadata()?;
        let rel = e.path().strip_prefix(base).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        out.push(RemoteEntry { path: rel, size: if md.is_dir() { 0 } else { md.len() }, is_dir: md.is_dir() });
        if md.is_dir() && recursive {
            walk(base, &e.path(), true, out)?;
        }
    }
    Ok(())
}

impl Remote for DirRemote {
    fn list(&self, dir: &str, recursive: bool) -> Result<Vec<RemoteEntry>> {
        let at = self.at(dir)?;
        if !at.is_dir() {
            return Err(Error::NotFound(dir.to_string()));
        }
        let mut out = Vec::new();
        walk(&at, &at, recursive, &mut out).map_err(|e| Error::io(dir, e))?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn get(&self, path: &str, local: &Path) -> Result<()> {
        let from = self.at(path)?;
        if !from.is_file() {
            return Err(Error::NotFound(path.to_string()));
        }
        std::fs::copy(&from, local).map(|_| ()).map_err(|e| Error::io(path, e))
    }

    fn put(&self, local: &Path, path: &str) -> Result<()> {
        let to = self.at(path)?;
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(path, e))?;
        }
        std::fs::copy(local, &to).map(|_| ()).map_err(|e| Error::io(path, e))
    }

    fn move_file(&self, from: &str, to: &str) -> Result<()> {
        let (src, dst) = (self.at(from)?, self.at(to)?);
        if !src.is_file() {
            return Err(Error::NotFound(from.to_string()));
        }
        if let Some(dir) = dst.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(to, e))?;
        }
        std::fs::rename(&src, &dst).map_err(|e| Error::io(from, e))
    }

    fn remove_empty_dir(&self, dir: &str) -> Result<()> {
        let at = self.at(dir)?;
        if at != self.root {
            let _ = std::fs::remove_dir(&at); // (fails when it is not empty: then it stays)
        }
        Ok(())
    }

    fn purge_dir(&self, dir: &str) -> Result<()> {
        let at = self.at(dir)?;
        if at == self.root {
            return Err(Error::Invalid("refusing to delete the remote's root".into()));
        }
        match std::fs::remove_dir_all(&at) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::NotFound(dir.to_string())),
            Err(e) => Err(Error::io(dir, e)),
        }
    }
}
