//! The other PC's side: a PC that is not the home PC keeps a full copy of the folders its user chose from the cloud library, in a
//! photo folder of its own, and sends its edits back the way a phone does (FORMAT.md, "A PC that is not the home PC").
//!
//! One pass ([`Other::sync`]) does, in this order:
//!
//! 1. read `mobile/manifest.json` (a higher `format` is refused);
//! 2. a photo that left the manifest (the home PC deleted it) is moved to `<state>/removed/<date>/`, never deleted;
//! 3. for every photo of a chosen folder that is here: a local sidecar LightCraft changed goes to `inbox/<device>/`; a newer sidecar
//!    from the home PC replaces an unchanged local one (the old one goes to `<state>/history/`); when both changed, the local one
//!    is sent first and **kept**, and the home PC's merge comes back in a later pass;
//! 4. photos that are missing are downloaded (checked against their id, never replacing a file) and announced to LightCraft through
//!    the adopted-photo hook.
//!
//! 5. (after the pass, whether or not it could reach the cloud library) the photos the user imported in LightCraft into the photo
//!    folder are noticed and queued for the home PC: see [`pickup`]; they are sent by [`Other::upload_step`].
//!
//! The engine is not a dependency, and nothing here opens, locks or writes a LightCraft library. This PC never writes `originals/` or
//! `mobile/` on the remote; its only writes there are new files under `inbox/<device>/`.
//!
//! **Deleting on this PC.** A photo whose file is gone from the photo folder is remembered as *left out* and is not downloaded again,
//! however often the home PC changes it. It comes back when its folder is chosen again: un-choosing a folder forgets everything this PC
//! remembers about its photos (the files stay where they are), so choosing it again downloads what is missing. When the home PC deletes
//! the photo too, the memory goes with it.

mod import;
mod pass;
mod pickup;
mod state;

pub use import::{ImportQueue, OUT_DIR, OutEntry, OutResult, QueueStatus, UploadBudget, UploadReport};
pub use pickup::{DEFAULT_HASH_BUDGET, DEFAULT_SETTLE, is_pickup_name, slash_path};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::home::{AdoptedHook, DEFAULT_IMPORT_FREE_MARGIN};
use crate::manifest;
use crate::progress::{Progress, Reporter};
use crate::util::{iso, temp_beside};
use crate::{Error, Remote, Result, valid_device};

/// What to keep from the cloud library where.
#[derive(Debug, Clone)]
pub struct OtherConfig {
    /// The local photo folder (the manifest's paths are relative to `originals/` and land here as they are).
    pub source: PathBuf,
    /// This app's state folder.
    pub state_dir: PathBuf,
    /// This PC's name in `inbox/<device>/`: `[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}`, not a phone's.
    pub device: String,
    /// The chosen folders, relative to `originals/` (`""`: the root itself). A folder means the photos directly in it.
    pub folders: Vec<String>,
    /// A name for the remote, for the status.
    pub remote_label: String,
    /// Refuse to move more photos aside than this in one pass (the home PC's `max_delete`).
    pub max_delete: usize,
    /// Free disk space that must be left over after a download (bytes).
    pub free_margin: u64,
    /// Folder-name patterns the home PC leaves out of the library (its `exclude`). Photos imported here are refused up front for a
    /// destination that matches one, the way the home PC would reject them.
    pub exclude: Vec<String>,
    /// How long a new photo's size and time must stay the same before this PC takes it (LightCraft may still be copying).
    pub settle: Duration,
    /// How long one pass hashes new photos before it leaves the rest for the next.
    pub hash_budget: Duration,
}

impl OtherConfig {
    pub fn new(source: impl Into<PathBuf>, state_dir: impl Into<PathBuf>, device: impl Into<String>, folders: Vec<String>) -> Self {
        OtherConfig {
            source: source.into(),
            state_dir: state_dir.into(),
            device: device.into(),
            folders,
            remote_label: String::new(),
            max_delete: 50,
            free_margin: DEFAULT_IMPORT_FREE_MARGIN,
            exclude: Vec::new(),
            settle: DEFAULT_SETTLE,
            hash_budget: DEFAULT_HASH_BUDGET,
        }
    }
}

/// What one pass did (and what is left to do).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OtherReport {
    /// Photos downloaded (paths relative to the photo folder).
    pub downloaded: Vec<String>,
    /// Local sidecars replaced by the home PC's newer version.
    pub sidecars_updated: Vec<String>,
    /// Edits sent to the inbox.
    pub edits_sent: usize,
    /// Edits sent that the home PC has not merged yet (the local sidecar stays as it is until it does).
    pub held_back: usize,
    /// Photos moved to `<state>/removed/` because the home PC deleted them.
    pub removed: Vec<String>,
    /// Photos deleted on this PC, which are not downloaded again.
    pub left_out: usize,
    /// Photos of the chosen folders that are on this PC, counted as shots the way the home PC counts: a raw and a JPEG of one shot are
    /// one photo (`downloaded` and the progress lines count files).
    pub photos: usize,
    /// Photos (shots) of the chosen folders with no file here yet.
    pub waiting_to_download: usize,
    /// Photos (shots) that arrived in this pass: none of their files was here before, one is now. `downloaded` counts files.
    pub downloaded_photos: usize,
    /// Photos (shots) here that came from the home PC (not imported on this PC), whatever the pass did.
    pub from_home_photos: usize,
    /// Edits that could not be sent (too large, ...); a network failure is an `Err` instead.
    pub edits_waiting: usize,
    /// Things the user can act on, one line each.
    pub problems: Vec<String>,
    /// The photos this PC found in its folder and sends, as the pass left them.
    pub queue: QueueStatus,
    /// Photos found in this pass and queued for the home PC.
    pub picked_up: usize,
    /// Folders (relative, `/`-separated) holding queued photos that are not chosen folders yet: the app adds them to its choice.
    pub new_folders: Vec<String>,
    /// New photos that have not settled yet (still being copied?).
    pub unsettled: usize,
    /// New photos left unhashed for want of time in this pass.
    pub pickup_left: usize,
    /// When to look again for the photos that were not ready.
    pub recheck: Option<Duration>,
}

/// The numbers the Status window shows, from the state alone (no network).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OtherStatus {
    /// Photos (shots, as the home PC counts them) on this PC.
    pub photos: usize,
    /// Photos (shots) with no file here yet, as of the last pass.
    pub waiting_to_download: usize,
    /// Local sidecar changes not sent yet, as the files are now.
    pub edits_waiting: usize,
    /// When the last pass ended well (ISO).
    pub last_pass: Option<String>,
    /// The photos this PC imported itself that are on their way to the home PC.
    pub queue: QueueStatus,
}

/// One folder of the cloud library with photos directly in it, for choosing.
#[derive(Debug, Clone, PartialEq)]
pub struct FolderInfo {
    /// Relative to `originals/`; `""` for photos at the top.
    pub folder: String,
    /// Photos (a raw+JPEG pair counts once).
    pub photos: usize,
    /// All their files (partners included).
    pub bytes: u64,
}

pub(crate) fn folder_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

/// Download and parse the manifest. `tmp`: a scratch file name.
pub(crate) fn fetch_manifest(remote: &dyn Remote, tmp: &Path) -> Result<manifest::Parsed> {
    if let Some(dir) = tmp.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir.display(), e))?;
    }
    let got = remote.get("mobile/manifest.json", tmp);
    let bytes = got.and_then(|()| std::fs::read(tmp).map_err(|e| Error::io(tmp.display(), e)));
    let _ = std::fs::remove_file(tmp);
    manifest::parse(&bytes?)
}

/// The folders of the cloud library to choose from, read from its manifest: each folder that has photos directly in it, with the number
/// of photos and the size of their files, sorted by name.
pub fn list_folders(remote: &dyn Remote) -> Result<Vec<FolderInfo>> {
    let tmp = temp_beside(&std::env::temp_dir().join("lightcraft-cloud-manifest.json"));
    let parsed = fetch_manifest(remote, &tmp)?;
    let mut by: std::collections::BTreeMap<String, FolderInfo> = std::collections::BTreeMap::new();
    for it in &parsed.items {
        let f = by.entry(folder_of(&it.path).to_string()).or_insert_with_key(|k| FolderInfo { folder: k.clone(), photos: 0, bytes: 0 });
        if !it.alternate {
            f.photos += 1;
        }
        f.bytes += it.size.unwrap_or(0);
    }
    Ok(by.into_values().collect())
}

/// Whether `inbox/<device>/` is already on the remote: a device of that name has sent edits (so a second PC with the same name would
/// mix up its edits with that one's).
pub fn device_in_use(remote: &dyn Remote, device: &str) -> Result<bool> {
    if !valid_device(device) {
        return Err(Error::Invalid("not a valid device name".into()));
    }
    match remote.list(&format!("inbox/{device}"), false) {
        Ok(entries) => Ok(!entries.is_empty()),
        Err(Error::NotFound(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;
type Logger = Arc<dyn Fn(&str) + Send + Sync>;

/// This PC's copy of some folders of one cloud library.
pub struct Other {
    cfg: OtherConfig,
    remote: Arc<dyn Remote>,
    clock: Clock,
    log: Logger,
    retry_delay: Duration,
    adopted_hook: Option<AdoptedHook>,
    /// Told of every sidecar this PC replaced with the home PC's (see [`Other::set_sidecar_hook`]).
    sidecar_hook: Option<AdoptedHook>,
    /// Where the stages of a pass and of an upload step are reported (see [`Other::set_progress_hook`]).
    rp: Reporter,
}

impl Other {
    /// Refuses a bad device name, and a state folder under `~/Pictures` (keep it out of backups and away from the user's libraries).
    pub fn new(mut cfg: OtherConfig, remote: Arc<dyn Remote>) -> Result<Self> {
        if !valid_device(&cfg.device) {
            return Err(Error::Invalid(format!(
                "device name `{}`: letters, digits, `_`, `-` and `.` (up to 64, not starting with a dot)",
                cfg.device
            )));
        }
        crate::home::check_state_dir(&cfg.state_dir)?;
        cfg.source = std::path::absolute(&cfg.source).map_err(|e| Error::io(cfg.source.display(), e))?;
        cfg.folders = cfg.folders.iter().map(|f| f.trim().trim_matches('/').to_string()).collect();
        Ok(Other { cfg, remote, clock: Arc::new(SystemTime::now), log: Arc::new(|m| log::info!("{m}")), retry_delay: Duration::from_secs(2), adopted_hook: None, sidecar_hook: None, rp: Reporter::new(None, Arc::new(SystemTime::now)) })
    }

    pub fn config(&self) -> &OtherConfig {
        &self.cfg
    }

    /// Replace the clock (tests).
    pub fn set_clock(&mut self, clock: impl Fn() -> SystemTime + Send + Sync + 'static) {
        self.clock = Arc::new(clock);
        self.rp = Reporter::new(self.rp.hook(), self.clock.clone());
    }

    /// Be told how far a pass or an upload step has come, after every change: the stage (looking at the cloud library, downloading,
    /// sending, ...), the photos and bytes done of the total where those are known, and when something last finished. Called from the
    /// thread that syncs, quickly and in order; it must not block. With no hook set nothing is measured. See [`crate::describe`] and
    /// [`crate::Estimator`] for the words and the time left.
    pub fn set_progress_hook(&mut self, f: impl Fn(&Progress) + Send + Sync + 'static) {
        self.rp = Reporter::new(Some(Arc::new(f)), self.clock.clone());
    }

    /// Where log lines go (default: the `log` crate at info level).
    pub fn set_logger(&mut self, f: impl Fn(&str) + Send + Sync + 'static) {
        self.log = Arc::new(f);
    }

    /// Pause before the second and third attempt of a network call that failed (default 2 s).
    pub fn set_retry_delay(&mut self, d: Duration) {
        self.retry_delay = d;
    }

    /// Be told the absolute path of every photo this PC downloads, once it is in the photo folder (and again on the next pass if the
    /// hook said no, until it says yes). The app uses it to tell a running LightCraft to add the photo.
    pub fn set_adopted_hook(&mut self, f: impl Fn(&Path) -> std::result::Result<(), String> + Send + Sync + 'static) {
        self.adopted_hook = Some(Arc::new(f));
    }

    /// Be told the absolute path of every sidecar this PC replaced with the home PC's version (a pull), just after it is in the photo
    /// folder. The app uses it to have a running LightCraft read the new edits. Not called for a pass where nothing changed. An `Err`
    /// is only logged: the sidecar is already written.
    pub fn set_sidecar_hook(&mut self, f: impl Fn(&Path) -> std::result::Result<(), String> + Send + Sync + 'static) {
        self.sidecar_hook = Some(Arc::new(f));
    }

    pub(crate) fn sidecar_written(&self, path: &Path) {
        if let Some(h) = &self.sidecar_hook
            && let Err(e) = h(path)
        {
            self.logln(format!("{}: the sidecar hook failed: {e}", path.display()));
        }
    }

    fn now(&self) -> SystemTime {
        (self.clock)()
    }

    fn logln(&self, m: impl AsRef<str>) {
        (self.log)(m.as_ref());
    }

    fn today(&self) -> String {
        iso(self.now())[..10].to_string()
    }
}

#[cfg(test)]
mod device_tests {
    use super::*;

    #[test]
    fn a_device_with_edits_in_the_inbox_is_in_use() {
        let d = std::env::temp_dir().join(format!("lc-device-in-use-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("inbox/laptop")).unwrap();
        std::fs::write(d.join("inbox/laptop/x.json"), "{}").unwrap();
        std::fs::create_dir_all(d.join("inbox/empty")).unwrap();
        let r = crate::DirRemote::new(d.clone());
        assert!(device_in_use(&r, "laptop").unwrap());
        assert!(!device_in_use(&r, "other-pc").unwrap());
        assert!(!device_in_use(&r, "empty").unwrap());
        assert!(device_in_use(&r, "../x").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
