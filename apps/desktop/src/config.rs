//! The desktop app's own config file (TOML, like the Python tool's) and where it lives.
//!
//! Default path `~/.config/lightcraft-cloud-desktop/config.toml`, state folder `~/.local/state/lightcraft-cloud-desktop/`.
//! The Python tool's `~/.config/lightcraft-cloud/` and `~/.local/state/lightcraft-cloud/` are never read or written.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lightcraft_cloud_client::home::{HomeConfig, PairPrimary};
use lightcraft_cloud_client::{DirRemote, RcloneRemote, Remote};
use serde::Deserialize;

pub const APP_DIR: &str = "lightcraft-cloud-desktop";

/// What the file says; every key but `remote` is optional. Unknown keys are ignored (the Python tool's config can be
/// reused).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct File {
    /// Folder to publish.
    pub source: String,
    /// An rclone `remote:path` (e.g. `gdrive:LightCraft Cloud`, or a crypt remote `lccrypt:`), or a local folder (starts
    /// with `/`, `~` or `.`). There is no default: a config without it is refused, so nothing is ever published to a
    /// remote nobody named.
    pub remote: Option<String>,
    /// Folder-name patterns (fnmatch, case-sensitive) skipped at any depth.
    pub exclude: Vec<String>,
    /// For raw+JPEG pairs, which file is the photo: `"jpeg"` or `"raw"`.
    pub pair_primary: String,
    /// Refuse when more published photos vanished than this since the last run.
    pub max_delete: usize,
    /// Long edge of the thumbnails.
    pub thumb_px: u32,
    /// JPEG quality of the thumbnails.
    pub thumb_quality: u8,
    /// Do not upload the photo files themselves, only sidecars, thumbnails, smart previews and the manifest.
    pub previews_only: bool,
    pub state_dir: Option<String>,
    /// The rclone config file (for an rclone remote); default `$RCLONE_CONFIG`, else `~/.config/rclone/rclone.conf`.
    pub rclone_config: Option<String>,
    /// The host `librclone.so`; default `$LIBRCLONE`, else the system's search path.
    pub librclone: Option<String>,
}

impl Default for File {
    fn default() -> Self {
        File {
            source: "~/Pictures/Photos".into(),
            remote: None,
            exclude: Vec::new(),
            pair_primary: "jpeg".into(),
            max_delete: 50,
            thumb_px: 512,
            thumb_quality: 80,
            previews_only: false,
            state_dir: None,
            rclone_config: None,
            librclone: None,
        }
    }
}

pub fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|h| !h.as_os_str().is_empty()).ok_or_else(|| "HOME is not set".to_string())
}

/// `~/` and `~` at the start of a path.
pub fn expand(p: &str) -> Result<PathBuf, String> {
    match p {
        "~" => home_dir(),
        _ => match p.strip_prefix("~/") {
            Some(rest) => Ok(home_dir()?.join(rest)),
            None => Ok(PathBuf::from(p)),
        },
    }
}

pub fn default_config_path() -> Result<PathBuf, String> {
    Ok(match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join(APP_DIR).join("config.toml"),
        None => home_dir()?.join(".config").join(APP_DIR).join("config.toml"),
    })
}

pub fn default_state_dir() -> Result<PathBuf, String> {
    Ok(match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join(APP_DIR),
        None => home_dir()?.join(".local/state").join(APP_DIR),
    })
}

/// A loaded config.
#[derive(Debug)]
pub struct Config {
    pub path: PathBuf,
    pub file: File,
    pub source: PathBuf,
    pub state_dir: PathBuf,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Config, String> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path()?,
        };
        let text = std::fs::read_to_string(&path).map_err(|e| format!("config {}: {e}", path.display()))?;
        let file: File = toml::from_str(&text).map_err(|e| format!("config {}: {e}", path.display()))?;
        file.pair_primary.parse::<PairPrimary>().map_err(|e| e.to_string())?;
        let source = expand(&file.source)?;
        let state_dir = match &file.state_dir {
            Some(d) => expand(d)?,
            None => default_state_dir()?,
        };
        let c = Config { path, file, source, state_dir };
        c.refuse_the_python_tools_folders()?;
        Ok(c)
    }

    /// This app never shares state with `lc-cloud`: refuse a state folder that is the Python tool's.
    fn refuse_the_python_tools_folders(&self) -> Result<(), String> {
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let theirs = home_dir()?.join(".local/state/lightcraft-cloud");
        let mine = canon(&self.state_dir);
        if mine == canon(&theirs) || mine.starts_with(canon(&theirs)) {
            return Err(format!("state_dir {} is the Python tool's (lc-cloud) state folder; this app keeps its own", self.state_dir.display()));
        }
        Ok(())
    }

    pub fn remote_spec(&self) -> Result<&str, String> {
        self.file
            .remote
            .as_deref()
            .map(|r| r.trim_end_matches('/'))
            .filter(|r| !r.is_empty())
            .ok_or_else(|| format!("config {}: `remote` is not set (an rclone remote:path, or a local folder)", self.path.display()))
    }

    pub fn home_config(&self) -> Result<HomeConfig, String> {
        let spec = self.remote_spec()?;
        let label = match local_remote(spec)? {
            Some(dir) => dir.display().to_string(),
            None => spec.to_string(),
        };
        let mut c = HomeConfig::new(&self.source, &self.state_dir, label);
        c.exclude = self.file.exclude.clone();
        c.pair_primary = self.file.pair_primary.parse::<PairPrimary>().map_err(|e| e.to_string())?;
        c.max_delete = self.file.max_delete;
        c.thumb_px = self.file.thumb_px;
        c.thumb_quality = self.file.thumb_quality;
        c.previews_only = self.file.previews_only;
        c.publisher = format!("lightcraft-cloud-desktop {}", env!("CARGO_PKG_VERSION"));
        Ok(c)
    }

    /// The remote: a local folder, or an rclone remote through librclone.
    pub fn open_remote(&self) -> Result<Arc<dyn Remote>, String> {
        let spec = self.remote_spec()?;
        if let Some(dir) = local_remote(spec)? {
            return Ok(Arc::new(DirRemote::new(dir)));
        }
        let conf = match &self.file.rclone_config {
            Some(c) => expand(c)?,
            None => match std::env::var_os("RCLONE_CONFIG").filter(|v| !v.is_empty()) {
                Some(c) => PathBuf::from(c),
                None => home_dir()?.join(".config/rclone/rclone.conf"),
            },
        };
        let lib: Option<PathBuf> = match &self.file.librclone {
            Some(l) => Some(expand(l)?),
            None => std::env::var_os("LIBRCLONE").filter(|v| !v.is_empty()).map(PathBuf::from),
        };
        RcloneRemote::open(lib.as_deref(), &conf, spec, Some(&self.state_dir.join("rclone-tmp")))
            .map(|r| Arc::new(r) as Arc<dyn Remote>)
            .map_err(|e| e.to_string())
    }
}

/// `Some(folder)` when `spec` names a local folder rather than an rclone remote.
pub fn local_remote(spec: &str) -> Result<Option<PathBuf>, String> {
    if spec.starts_with('/') || spec.starts_with('~') || spec.starts_with('.') {
        let p = expand(spec)?;
        return std::path::absolute(&p).map(Some).map_err(|e| format!("{spec}: {e}"));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_folders_and_rclone_remotes_are_told_apart() {
        assert!(local_remote("/tmp/x").unwrap().is_some());
        assert!(local_remote("./x").unwrap().is_some());
        assert!(local_remote("gdrive:LightCraft Cloud").unwrap().is_none());
        assert!(local_remote("lccrypt:").unwrap().is_none());
    }

    #[test]
    fn a_config_needs_a_remote_and_ignores_unknown_keys() {
        let d = std::env::temp_dir().join(format!("lc-desktop-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("c.toml");
        std::fs::write(&f, "source = \"/tmp/s\"\nstate_dir = \"/tmp/lc-state-x\"\nnotify = true\nphoto_backup = \"restic\"\n").unwrap();
        let c = Config::load(Some(&f)).unwrap();
        assert!(c.remote_spec().unwrap_err().contains("remote"));
        std::fs::write(&f, "source = \"/tmp/s\"\nremote = \"/tmp/r/\"\nstate_dir = \"/tmp/lc-state-x\"\nexclude = [\"a\"]\npair_primary = \"RAW\"\n")
            .unwrap();
        let c = Config::load(Some(&f)).unwrap();
        let h = c.home_config().unwrap();
        assert_eq!((h.exclude, h.pair_primary, h.remote_label.as_str()), (vec!["a".to_string()], PairPrimary::Raw, "/tmp/r"));
        std::fs::write(&f, "remote = \"/tmp/r\"\npair_primary = \"tiff\"\nstate_dir = \"/tmp/lc-state-x\"\n").unwrap();
        assert!(Config::load(Some(&f)).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_python_tools_state_folder_is_refused() {
        let d = std::env::temp_dir().join(format!("lc-desktop-cfg2-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("c.toml");
        std::fs::write(&f, "remote = \"/tmp/r\"\nstate_dir = \"~/.local/state/lightcraft-cloud\"\n").unwrap();
        assert!(Config::load(Some(&f)).unwrap_err().contains("Python"));
        std::fs::write(&f, "remote = \"/tmp/r\"\nstate_dir = \"~/.local/state/lightcraft-cloud/sub\"\n").unwrap();
        assert!(Config::load(Some(&f)).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
