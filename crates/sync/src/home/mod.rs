//! The home side: what a PC does to keep a cloud library (FORMAT.md) in step with a photo folder.
//!
//! * publishing: scan the folder, mirror the originals one way, make thumbnails and smart previews, write the manifest;
//! * the inbox: apply the phone's edits to the sidecars next to the photos, merging them with the PC's own;
//! * state: what was published, and every sidecar version, in the app's own state folder.
//!
//! This is what the Python tool `lc-cloud` does, producing the same remote layout. The engine is not a dependency: a
//! [`Renderer`] makes the thumbnails and smart previews and describes the photos; the app plugs in its own.
//! Nothing here opens, locks or writes a LightCraft library.

mod inbox;
mod json;
mod manifest;
mod merge;
mod publish;
mod purge;
mod scan;
mod state;
mod xmp;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub use inbox::{InboxName, InboxSummary, decide, notify_message, parse_inbox_name, phone_is_newer};
pub use manifest::{Alternate, ManifestPhoto, build_manifest_photos, manifest_ids};
pub use publish::{MirrorStats, StatusReport, SyncReport};
pub use purge::{MAX_BACKUP_AGE_HOURS, PurgeReport};
pub use scan::{Entry, FileInfo, Plan, STABLE_SECS, Scan, diff_scan, find_sidecars, fnmatch, is_excluded, published_removals, scan_source};
pub use state::{Meta, Row, RunLock, State, Uploaded, VersionStore};

use crate::util::iso;
use crate::{Error, Remote, Result};

/// Merge three versions of a sidecar (the common ancestor, the PC's, the phone's): the merged bytes and the conflicts
/// that were settled by "newest wins", or `None` when the documents cannot be merged safely.
pub fn merge_sidecars(base: &[u8], pc: &[u8], phone: &[u8], phone_newer: bool) -> Option<(Vec<u8>, Vec<String>)> {
    merge::merge_sidecars(base, pc, phone, phone_newer)
}

/// Whether the bytes are an acceptable sidecar from the phone: `None` if so, else why not.
pub fn validate_xmp(data: &[u8]) -> Option<String> {
    xmp::validate(data)
}

/// Which file of a raw+JPEG pair is the photo and which its alternate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PairPrimary {
    #[default]
    Jpeg,
    Raw,
}

impl std::str::FromStr for PairPrimary {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "jpeg" => Ok(PairPrimary::Jpeg),
            "raw" => Ok(PairPrimary::Raw),
            other => Err(Error::Invalid(format!("pair_primary must be \"jpeg\" or \"raw\", not {other:?}"))),
        }
    }
}

/// What to publish where.
#[derive(Debug, Clone)]
pub struct HomeConfig {
    /// The photo folder.
    pub source: PathBuf,
    /// This app's state folder (never another tool's).
    pub state_dir: PathBuf,
    /// A name for the remote, recorded with the manifest so a changed remote gets the manifest again.
    pub remote_label: String,
    /// Folder-name patterns (fnmatch, case-sensitive) skipped at any depth.
    pub exclude: Vec<String>,
    pub pair_primary: PairPrimary,
    /// Refuse when more published photos vanished than this since the last run; also the most files one mirror removes.
    pub max_delete: usize,
    /// Long edge of the thumbnails.
    pub thumb_px: u32,
    pub thumb_quality: u8,
    /// Upload thumbnails, smart previews, the manifest and sidecars, but not the photo files themselves.
    pub previews_only: bool,
    /// The manifest's `publisher`.
    pub publisher: String,
}

impl HomeConfig {
    pub fn new(source: impl Into<PathBuf>, state_dir: impl Into<PathBuf>, remote_label: impl Into<String>) -> Self {
        HomeConfig {
            source: source.into(),
            state_dir: state_dir.into(),
            remote_label: remote_label.into(),
            exclude: Vec::new(),
            pair_primary: PairPrimary::Jpeg,
            max_delete: 50,
            thumb_px: 512,
            thumb_quality: 80,
            previews_only: false,
            publisher: format!("lightcraft-cloud {}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// What a [`Renderer`] knows about a photo.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PhotoInfo {
    /// The LightCraft `content_hash` of the original's bytes (32 hex): the photo's id everywhere.
    pub content_hash: String,
    pub captured: Option<String>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub rating: Option<i64>,
    pub flag: Option<String>,
    pub label: Option<String>,
}

/// The result of [`Renderer::add`].
#[derive(Debug, Clone, PartialEq)]
pub enum Added {
    Photo(PhotoInfo),
    /// The same bytes as a photo already added: listed once, under the first.
    Duplicate,
}

/// The engine, as publishing needs it. Photos are named by their path in the source folder. Implementations keep their
/// own scratch library (never the user's), and read the photo's sidecar so renders carry the current edits.
pub trait Renderer {
    /// Take a photo (and its sidecar) into the scratch library and describe it.
    fn add(&mut self, photo: &Path) -> Result<Added>;
    /// The sidecar changed: read it again and describe the photo. A photo the scratch library lost is added again.
    fn reread(&mut self, photo: &Path) -> Result<PhotoInfo>;
    /// Write a JPEG thumbnail with the photo's current edits (`long_edge` px, quality 1..100, sRGB) to `out` (a `.jpg` name).
    fn thumbnail(&mut self, photo: &Path, long_edge: u32, quality: u8, out: &Path) -> Result<()>;
    /// Write the photo's smart preview to `out` (a `.lcsp` name); returns its `previewKey` (the engine's file name without
    /// `.lcsp`).
    fn smart_preview(&mut self, photo: &Path, out: &Path) -> Result<String>;
    /// Drop photos (changed or gone from the source).
    fn forget(&mut self, photos: &[PathBuf]) -> Result<()>;
    /// Called after a batch of the calls above: save the scratch library.
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}

/// `YYYY-mm-ddTHH:MM:SSZ`.
pub fn utc_iso(t: SystemTime) -> String {
    iso(t)
}

type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;
type Logger = Arc<dyn Fn(&str) + Send + Sync>;

/// One photo folder published to one remote.
pub struct Home {
    cfg: HomeConfig,
    remote: Arc<dyn Remote>,
    clock: Clock,
    log: Logger,
    retry_delay: Duration,
}

impl Home {
    /// Refuses a state folder under `~/Pictures` (keep it out of backups and away from the user's libraries).
    pub fn new(cfg: HomeConfig, remote: Arc<dyn Remote>) -> Result<Self> {
        check_state_dir(&cfg.state_dir)?;
        Ok(Home { cfg, remote, clock: Arc::new(SystemTime::now), log: Arc::new(|m| log::info!("{m}")), retry_delay: Duration::from_secs(2) })
    }

    pub fn config(&self) -> &HomeConfig {
        &self.cfg
    }

    /// Replace the clock (tests).
    pub fn set_clock(&mut self, clock: impl Fn() -> SystemTime + Send + Sync + 'static) {
        self.clock = Arc::new(clock);
    }

    /// Where log lines go (default: the `log` crate at info level).
    pub fn set_logger(&mut self, f: impl Fn(&str) + Send + Sync + 'static) {
        self.log = Arc::new(f);
    }

    /// Pause before the second and third attempt of a remote call that failed (default 2 s).
    pub fn set_retry_delay(&mut self, d: Duration) {
        self.retry_delay = d;
    }

    pub fn remote(&self) -> &Arc<dyn Remote> {
        &self.remote
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

    /// The sidecar version store of this library.
    pub fn versions(&self) -> VersionStore {
        VersionStore::new(self.cfg.state_dir.join("sidecar-versions"))
    }

    fn mobile(&self) -> PathBuf {
        self.cfg.state_dir.join("mobile")
    }
}

fn check_state_dir(dir: &Path) -> Result<()> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Ok(()) };
    let pictures = std::fs::canonicalize(home.join("Pictures")).unwrap_or_else(|_| home.join("Pictures"));
    // resolve as far as the path exists
    let abs = std::path::absolute(dir).map_err(|e| Error::io(dir.display(), e))?;
    let mut existing = abs.clone();
    let mut tail = Vec::new();
    while !existing.exists() {
        match (existing.file_name().map(|n| n.to_owned()), existing.parent().map(Path::to_path_buf)) {
            (Some(n), Some(p)) => {
                tail.push(n);
                existing = p;
            }
            _ => break,
        }
    }
    let mut resolved = std::fs::canonicalize(&existing).unwrap_or(existing);
    for t in tail.iter().rev() {
        resolved.push(t);
    }
    if resolved.starts_with(&pictures) {
        return Err(Error::Invalid(format!(
            "state_dir {} is under ~/Pictures; refusing (keep it outside backups and away from your libraries)",
            resolved.display()
        )));
    }
    Ok(())
}
