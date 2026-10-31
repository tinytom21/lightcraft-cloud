//! The desktop app's own config file (TOML, like the Python tool's) and where it lives.
//!
//! Default path `~/.config/lightcraft-cloud-desktop/config.toml`, state folder `~/.local/state/lightcraft-cloud-desktop/`.
//! The Python tool's `~/.config/lightcraft-cloud/` and `~/.local/state/lightcraft-cloud/` are never read or written.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lightcraft_cloud_client::home::{HomeConfig, PairPrimary};
use lightcraft_cloud_client::{DirRemote, RcloneRemote, Remote};
use serde::{Deserialize, Serialize};

pub const APP_DIR: &str = "lightcraft-cloud-desktop";

/// What the file says; every key but `remote` is optional. Unknown keys are ignored (the Python tool's config can be
/// reused).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Desktop notifications (phone edits applied, a sync that failed).
    pub notify: bool,
    /// The weekly purge deletes the remote's `history/` and `inbox-done/` day folders older than this many days.
    pub device_history_days: u32,
    /// A command that answers `snapshots --latest 1 --json` (restic). The purge only runs when the newest snapshot is at most
    /// 48 hours old; an empty string switches the purge off.
    pub photo_backup: String,
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
            notify: true,
            device_history_days: 30,
            photo_backup: "restic".into(),
        }
    }
}

/// The keys this app writes (and removes when unset); any other key in the file is left as it is.
const KNOWN_KEYS: &[&str] = &[
    "source",
    "remote",
    "exclude",
    "pair_primary",
    "max_delete",
    "thumb_px",
    "thumb_quality",
    "previews_only",
    "state_dir",
    "rclone_config",
    "librclone",
    "notify",
    "device_history_days",
    "photo_backup",
];

impl File {
    /// Write the settings to `path` (the folder is created). Keys this app does not know stay as they are in an existing file;
    /// comments do not survive.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut table: toml::Table = match std::fs::read_to_string(path) {
            Ok(t) => toml::from_str(&t).map_err(|e| format!("config {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
            Err(e) => return Err(format!("config {}: {e}", path.display())),
        };
        for k in KNOWN_KEYS {
            table.remove(*k);
        }
        let mine = toml::Table::try_from(self).map_err(|e| e.to_string())?;
        for (k, v) in mine {
            table.insert(k, v);
        }
        let text = toml::to_string_pretty(&table).map_err(|e| e.to_string())?;
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
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
        Config::from_file(path, file)
    }

    /// A config from settings that were not read from the file (the Settings window checks what it is about to save).
    pub fn from_file(path: PathBuf, file: File) -> Result<Config, String> {
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

    /// Where the config is, given `--config`: that, else the default path.
    pub fn resolve_path(path: Option<&Path>) -> Result<PathBuf, String> {
        match path {
            Some(p) => Ok(p.to_path_buf()),
            None => default_config_path(),
        }
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
        let lib = find_librclone(self.file.librclone.as_deref())?;
        RcloneRemote::open(lib.as_deref(), &conf, spec, Some(&self.state_dir.join("rclone-tmp")))
            .map(|r| Arc::new(r) as Arc<dyn Remote>)
            .map_err(|e| e.to_string())
    }
}

/// Where to load `librclone.so` from: the config's `librclone`, else `$LIBRCLONE`, else the first of `librclone.so` next to the
/// running executable (symlinks resolved) and `~/.local/lib/lightcraft-cloud/librclone.so` that exists. `None` leaves it to the
/// dynamic loader's search path.
pub fn find_librclone(configured: Option<&str>) -> Result<Option<PathBuf>, String> {
    let configured = configured.map(expand).transpose()?;
    let env = std::env::var_os("LIBRCLONE").filter(|v| !v.is_empty()).map(PathBuf::from);
    let exe = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok());
    Ok(librclone_in(configured, env, exe.as_deref(), home_dir().ok().as_deref()))
}

fn librclone_in(configured: Option<PathBuf>, env: Option<PathBuf>, exe: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    configured.or(env).or_else(|| {
        let beside_exe = exe.and_then(Path::parent).map(|d| d.join("librclone.so"));
        let in_home = home.map(|h| h.join(".local/lib/lightcraft-cloud/librclone.so"));
        beside_exe.into_iter().chain(in_home).find(|p| p.is_file())
    })
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
    fn librclone_is_looked_for_in_order() {
        let d = std::env::temp_dir().join(format!("lc-desktop-lib-{}", std::process::id()));
        let (bin, home) = (d.join("bin"), d.join("home"));
        let home_lib = home.join(".local/lib/lightcraft-cloud");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&home_lib).unwrap();
        let exe = bin.join("lightcraft-cloud-desktop");
        let find = |c: Option<&str>, e: Option<&str>| librclone_in(c.map(PathBuf::from), e.map(PathBuf::from), Some(&exe), Some(&home));
        assert_eq!(find(None, None), None);
        std::fs::write(home_lib.join("librclone.so"), "").unwrap();
        assert_eq!(find(None, None), Some(home_lib.join("librclone.so")));
        std::fs::write(bin.join("librclone.so"), "").unwrap();
        assert_eq!(find(None, None), Some(bin.join("librclone.so")));
        assert_eq!(find(None, Some("/env/librclone.so")), Some(PathBuf::from("/env/librclone.so")));
        assert_eq!(find(Some("/cfg/librclone.so"), Some("/env/librclone.so")), Some(PathBuf::from("/cfg/librclone.so")));
        let _ = std::fs::remove_dir_all(&d);
    }

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
    fn saving_keeps_unknown_keys_and_drops_unset_ones() {
        let d = std::env::temp_dir().join(format!("lc-desktop-cfg3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let f = d.join("sub/c.toml");
        let mut file = File { source: "/tmp/s".into(), remote: Some("gd:LC".into()), exclude: vec!["a".into()], notify: false, ..File::default() };
        file.save(&f).unwrap(); // (creates the folder)
        let back: File = toml::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(
            (back.source.as_str(), back.remote.as_deref(), back.exclude.clone(), back.notify),
            ("/tmp/s", Some("gd:LC"), vec!["a".to_string()], false)
        );
        // an older file with a key this app does not know, and a remote that is now unset
        std::fs::write(&f, "remote = \"old:x\"\nphoto_backup_extra = \"keep me\"\n").unwrap();
        file.remote = None;
        file.save(&f).unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.contains("photo_backup_extra = \"keep me\"") && !text.contains("old:x") && !text.contains("remote ="), "{text}");
        assert!(!f.with_extension("toml.tmp").exists());
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
