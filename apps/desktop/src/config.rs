//! The desktop app's own config file (TOML, like the Python tool's) and where it lives.
//!
//! Default path `~/.config/lightcraft-cloud-desktop/config.toml`, state folder `~/.local/state/lightcraft-cloud-desktop/`.
//! The Python tool's `~/.config/lightcraft-cloud/` and `~/.local/state/lightcraft-cloud/` are never read or written.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lightcraft_cloud_client::home::{HomeConfig, PairPrimary};
use lightcraft_cloud_client::other::OtherConfig;
use lightcraft_cloud_client::valid_device;
use lightcraft_cloud_client::{DirRemote, RcloneRemote, Remote};
use serde::{Deserialize, Serialize};

pub const APP_DIR: &str = "lightcraft-cloud-desktop";

/// What this PC is in the cloud library.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The master: publishes its photo folder, applies the phone's edits, adds the photos other PCs send.
    #[default]
    Home,
    /// Keeps a copy of the folders its user chose, and sends its edits to the home PC.
    Other,
}

/// What the file says; every key but `remote` is optional. Unknown keys are ignored (the Python tool's config can be
/// reused).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct File {
    /// `"home"` (the default: what every config before this key was) or `"other"`.
    pub role: Role,
    /// Folder to publish; for role `other`, the local photo folder the chosen folders are downloaded into.
    pub source: String,
    /// Role `other`: the folders to keep a copy of, relative to the cloud library's `originals/` (photos directly in them).
    pub folders: Vec<String>,
    /// Role `other`: this PC's name in the cloud library's inbox (`inbox/<device>/`). Default: the host name, made valid. Must not be a
    /// phone's name.
    pub device: Option<String>,
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
    /// The port of LightCraft's control channel (`lightcraft --control PORT`, or `LIGHTCRAFT_CONTROL_PORT`) on this PC. A
    /// running LightCraft that answers there is asked to add the photos other PCs sent (never anything else, and never by
    /// opening a library). 0 switches it off.
    pub lightcraft_control_port: u16,
    /// After a sidecar is written into the photo folder (a phone edit applied, or the home PC's version taken), ask a running LightCraft
    /// to read the sidecars of that folder, as Synchronize Folder does, so the edit shows without anyone running it. Only when
    /// LightCraft writes its edits into sidecars itself. Needs the control port (`lightcraft_control_port`). Off: nothing is queued
    /// or sent for this; the warning that LightCraft is not saving sidecars still shows.
    pub lightcraft_refresh_edits: bool,
    /// Look on GitHub, shortly after the start and then once a day, for a newer release of this app (see [`crate::update`]). Only
    /// a note in the tray and Status; nothing is installed unless the user clicks.
    pub check_for_updates: bool,
}

impl Default for File {
    fn default() -> Self {
        File {
            role: Role::Home,
            source: "~/Pictures/Photos".into(),
            folders: Vec::new(),
            device: None,
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
            lightcraft_control_port: crate::lightcraft::DEFAULT_PORT,
            lightcraft_refresh_edits: true,
            check_for_updates: true,
        }
    }
}

/// The keys this app writes (and removes when unset); any other key in the file is left as it is.
const KNOWN_KEYS: &[&str] = &[
    "role",
    "folders",
    "device",
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
    "lightcraft_control_port",
    "lightcraft_refresh_edits",
    "check_for_updates",
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
        lightcraft_cloud_client::replace_file(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Add `folder` to the folders this PC keeps (role `other`) in the config file at `path`; the app notices the change. `Ok(false)`: it was
/// there already. The photo folder itself is `""`. Everything else in the file stays as it is.
pub fn add_chosen_folder(path: &Path, folder: &str) -> Result<bool, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("config {}: {e}", path.display()))?;
    let mut file: File = toml::from_str(&text).map_err(|e| format!("config {}: {e}", path.display()))?;
    // ("" is the photo folder itself)
    let wanted = folder.trim().trim_matches('/');
    if file.folders.iter().any(|c| c.trim().trim_matches('/') == wanted) {
        return Ok(false);
    }
    file.folders.push(wanted.to_string());
    file.save(path)?;
    Ok(true)
}

/// The user's home folder: `$HOME`, and on Windows (which has none) `%USERPROFILE%`.
pub fn home_dir() -> Result<PathBuf, String> {
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from).filter(|h| !h.as_os_str().is_empty());
    var("HOME").or_else(|| if cfg!(windows) { var("USERPROFILE") } else { None }).ok_or_else(|| "HOME is not set".to_string())
}

/// A folder of the user's profile that Windows names by a variable (`%APPDATA%`, `%LOCALAPPDATA%`); None elsewhere.
fn windows_dir(var: &str) -> Option<PathBuf> {
    if cfg!(windows) { std::env::var_os(var).map(PathBuf::from).filter(|h| !h.as_os_str().is_empty()) } else { None }
}

/// This PC's host name (`$COMPUTERNAME` on Windows, `$HOSTNAME`, the kernel's, `/etc/hostname`), or None.
fn host_name() -> Option<String> {
    let var = |k: &str| std::env::var(k).ok();
    let file = |p: &str| std::fs::read_to_string(p).ok();
    var("COMPUTERNAME")
        .or_else(|| var("HOSTNAME"))
        .or_else(|| file("/proc/sys/kernel/hostname"))
        .or_else(|| file("/etc/hostname"))
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

/// A host name made into a valid device name (see `valid_device`): other characters become `-`, a leading dot goes, 64 at most.
pub fn device_from_host(host: &str) -> String {
    let mut d: String = host.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '-' }).collect();
    d = d.trim_start_matches('.').chars().take(64).collect();
    if d.is_empty() { "pc".to_string() } else { d }
}

/// The default device name: this PC's host name, made valid.
pub fn default_device() -> String {
    device_from_host(&host_name().unwrap_or_default())
}

/// `~/` and `~` at the start of a path.
pub fn expand(p: &str) -> Result<PathBuf, String> {
    match p {
        "~" => home_dir(),
        _ => match p.strip_prefix("~/").or_else(|| if cfg!(windows) { p.strip_prefix("~\\") } else { None }) {
            Some(rest) => Ok(home_dir()?.join(rest)),
            None => Ok(PathBuf::from(p)),
        },
    }
}

pub fn default_config_path() -> Result<PathBuf, String> {
    Ok(match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join(APP_DIR).join("config.toml"),
        None => match windows_dir("APPDATA") {
            Some(a) => a.join(APP_DIR).join("config.toml"),
            None => home_dir()?.join(".config").join(APP_DIR).join("config.toml"),
        },
    })
}

pub fn default_state_dir() -> Result<PathBuf, String> {
    Ok(match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join(APP_DIR),
        None => match windows_dir("LOCALAPPDATA") {
            Some(a) => a.join(APP_DIR),
            None => home_dir()?.join(".local/state").join(APP_DIR),
        },
    })
}

/// rclone's own config file: `$RCLONE_CONFIG`, else `%APPDATA%\\rclone\\rclone.conf` on Windows, else `~/.config/rclone/rclone.conf`.
pub fn default_rclone_config() -> Result<PathBuf, String> {
    if let Some(c) = std::env::var_os("RCLONE_CONFIG").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(c));
    }
    Ok(match windows_dir("APPDATA") {
        Some(a) => a.join("rclone").join("rclone.conf"),
        None => home_dir()?.join(".config/rclone/rclone.conf"),
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

    /// This PC's name in the inbox (role `other`): the config's `device`, else the host name made valid.
    pub fn device(&self) -> String {
        match self.file.device.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) => d.to_string(),
            None => default_device(),
        }
    }

    /// The other-PC role's settings for the sync crate.
    pub fn other_config(&self) -> Result<OtherConfig, String> {
        let spec = self.remote_spec()?;
        let label = match local_remote(spec)? {
            Some(dir) => dir.display().to_string(),
            None => spec.to_string(),
        };
        let device = self.device();
        if !valid_device(&device) {
            return Err(format!("device name `{device}`: letters, digits, `_`, `-` and `.` (up to 64, not starting with a dot)"));
        }
        let mut c = OtherConfig::new(&self.source, &self.state_dir, device, self.file.folders.clone());
        c.remote_label = label;
        c.max_delete = self.file.max_delete;
        c.exclude = self.file.exclude.clone();
        Ok(c)
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
            None => default_rclone_config()?,
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

/// The file name of rclone's library on this system.
const LIBRCLONE: &str = if cfg!(windows) { "librclone.dll" } else { "librclone.so" };

fn librclone_in(configured: Option<PathBuf>, env: Option<PathBuf>, exe: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    configured.or(env).or_else(|| {
        let beside_exe = exe.and_then(Path::parent).map(|d| d.join(LIBRCLONE));
        let in_home = home.map(|h| h.join(".local/lib/lightcraft-cloud").join(LIBRCLONE));
        beside_exe.into_iter().chain(in_home).find(|p| p.is_file())
    })
}

/// `C:\\folder`, `C:/folder` or `\\\\server\\share`: a Windows path (a bare `C:` is an rclone remote called C, as rclone itself reads it).
fn is_windows_path(spec: &str) -> bool {
    let b = spec.as_bytes();
    spec.starts_with("\\\\") || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
}

/// `Some(folder)` when `spec` names a local folder rather than an rclone remote.
pub fn local_remote(spec: &str) -> Result<Option<PathBuf>, String> {
    if spec.starts_with('/') || spec.starts_with('~') || spec.starts_with('.') || (cfg!(windows) && is_windows_path(spec)) {
        let p = expand(spec)?;
        return std::path::absolute(&p).map(Some).map_err(|e| format!("{spec}: {e}"));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How a local remote `/tmp/r` is labelled: made absolute (on Windows that puts the current drive in front).
    fn abs_tmp_r() -> String {
        std::path::absolute("/tmp/r").unwrap().display().to_string()
    }

    #[test]
    fn windows_paths_are_local_folders_only_on_windows() {
        for p in [r"C:\photos", "D:/lib", r"\\nas\share\lib"] {
            assert!(is_windows_path(p), "{p}");
            assert_eq!(local_remote(p).unwrap().is_some(), cfg!(windows), "{p}");
        }
        for r in ["gdrive:LC", "C:", "c:lib", "lccrypt:"] {
            assert!(!is_windows_path(r), "{r} is an rclone remote");
        }
    }

    #[test]
    fn a_tilde_means_the_users_home_folder_on_either_kind_of_slash_where_the_system_has_both() {
        let home = home_dir().unwrap();
        assert_eq!(expand("~").unwrap(), home);
        assert_eq!(expand("~/Pictures/Photos").unwrap(), home.join("Pictures/Photos"));
        let back = expand(r"~\Pictures\Photos").unwrap();
        if cfg!(windows) {
            assert_eq!(back, home.join(r"Pictures\Photos"));
        } else {
            assert_eq!(back, PathBuf::from(r"~\Pictures\Photos"), "a backslash is an ordinary character in a Linux name");
        }
        assert_eq!(expand("/abs/x").unwrap(), PathBuf::from("/abs/x"));
    }

    #[test]
    fn a_host_name_becomes_a_valid_device_name() {
        for (host, want) in [("laptop", "laptop"), ("Toms Laptop", "Toms-Laptop"), (".hidden", "hidden"), ("", "pc"), ("é", "-"), ("a.b_c-d", "a.b_c-d")] {
            let d = device_from_host(host);
            assert_eq!(d, want, "{host:?}");
            assert!(valid_device(&d), "{d}");
        }
        assert_eq!(device_from_host(&"x".repeat(100)).len(), 64);
        assert!(valid_device(&default_device()));
    }

    #[test]
    fn the_import_destination_becomes_a_chosen_folder_without_touching_the_rest_of_the_config() {
        let d = std::env::temp_dir().join(format!("lc-desktop-chosen-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("c.toml");
        std::fs::write(&f, "role = \"other\"\nsource = \"/tmp/s\"\nremote = \"/tmp/r\"\nfolders = [\"A\"]\nsomething_else = 7\n").unwrap();
        assert_eq!(add_chosen_folder(&f, "261101 - Lisbon/"), Ok(true));
        assert_eq!(add_chosen_folder(&f, "261101 - Lisbon"), Ok(false), "already chosen");
        assert_eq!(add_chosen_folder(&f, "A"), Ok(false));
        let t = std::fs::read_to_string(&f).unwrap();
        let c = Config::load(Some(&f)).unwrap();
        assert_eq!(c.file.folders, ["A", "261101 - Lisbon"]);
        assert!(t.contains("something_else = 7") && t.contains("role = \"other\""), "{t}");
        assert!(add_chosen_folder(&d.join("nope.toml"), "X").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_role_defaults_to_home_and_other_pc_settings_are_read_and_kept() {
        let d = std::env::temp_dir().join(format!("lc-desktop-role-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("c.toml");
        std::fs::write(&f, "remote = \"/tmp/r\"\nstate_dir = \"/tmp/lc-state-x\"\n").unwrap();
        let c = Config::load(Some(&f)).unwrap();
        assert_eq!((c.file.role, c.file.folders.is_empty()), (Role::Home, true), "an existing config behaves as before");
        std::fs::write(&f, "role = \"other\"\nsource = \"/tmp/s\"\nremote = \"/tmp/r\"\nstate_dir = \"/tmp/lc-state-x\"\nfolders = [\"A\", \"B/C\"]\ndevice = \"laptop\"\n").unwrap();
        let c = Config::load(Some(&f)).unwrap();
        assert_eq!((c.file.role, c.device()), (Role::Other, "laptop".to_string()));
        let o = c.other_config().unwrap();
        assert_eq!((o.folders, o.device.as_str(), o.remote_label.as_str()), (vec!["A".to_string(), "B/C".into()], "laptop", abs_tmp_r().as_str()));
        c.file.save(&f).unwrap();
        let back = Config::load(Some(&f)).unwrap();
        assert_eq!((back.file.role, back.file.folders.len(), back.file.device.as_deref()), (Role::Other, 2, Some("laptop")));
        std::fs::write(&f, "role = \"other\"\ndevice = \"my laptop!\"\nremote = \"/tmp/r\"\nstate_dir = \"/tmp/lc-state-x\"\n").unwrap();
        assert!(Config::load(Some(&f)).unwrap().other_config().is_err());
        std::fs::write(&f, "role = \"boss\"\nremote = \"/tmp/r\"\n").unwrap();
        assert!(Config::load(Some(&f)).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

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
        std::fs::write(home_lib.join(LIBRCLONE), "").unwrap();
        assert_eq!(find(None, None), Some(home_lib.join(LIBRCLONE)));
        std::fs::write(bin.join(LIBRCLONE), "").unwrap();
        assert_eq!(find(None, None), Some(bin.join(LIBRCLONE)));
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
        assert_eq!((h.exclude, h.pair_primary, h.remote_label.as_str()), (vec!["a".to_string()], PairPrimary::Raw, abs_tmp_r().as_str()));
        std::fs::write(&f, "remote = \"/tmp/r\"\npair_primary = \"tiff\"\nstate_dir = \"/tmp/lc-state-x\"\n").unwrap();
        assert!(Config::load(Some(&f)).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_lightcraft_port_defaults_to_7980_zero_is_off_and_it_is_kept_when_saving() {
        let d = std::env::temp_dir().join(format!("lc-desktop-cfg4-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("c.toml");
        std::fs::write(&f, "remote = \"/tmp/r\"\nstate_dir = \"/tmp/lc-state-x\"\nfuture_key = true\n").unwrap();
        assert_eq!(Config::load(Some(&f)).unwrap().file.lightcraft_control_port, 7980, "an existing config without the key");
        std::fs::write(&f, "remote = \"/tmp/r\"\nstate_dir = \"/tmp/lc-state-x\"\nlightcraft_control_port = 0\n").unwrap();
        let c = Config::load(Some(&f)).unwrap();
        assert_eq!(c.file.lightcraft_control_port, 0);
        c.file.save(&f).unwrap();
        assert_eq!(Config::load(Some(&f)).unwrap().file.lightcraft_control_port, 0, "saving the settings keeps it");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn refreshing_edits_in_lightcraft_is_on_unless_switched_off_and_the_choice_is_kept_when_saving() {
        let d = std::env::temp_dir().join(format!("lc-desktop-cfg5-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("c.toml");
        std::fs::write(&f, "remote = \"/tmp/r\"\nstate_dir = \"/tmp/lc-state-x\"\n").unwrap();
        assert!(Config::load(Some(&f)).unwrap().file.lightcraft_refresh_edits, "an existing config without the key");
        std::fs::write(&f, "remote = \"/tmp/r\"\nstate_dir = \"/tmp/lc-state-x\"\nlightcraft_refresh_edits = false\n").unwrap();
        let c = Config::load(Some(&f)).unwrap();
        assert!(!c.file.lightcraft_refresh_edits);
        c.file.save(&f).unwrap();
        assert!(!Config::load(Some(&f)).unwrap().file.lightcraft_refresh_edits, "saving the settings keeps it");
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
