//! The background engine's workers: what the [`Service`](crate::service::Service) calls to do a job.
//!
//! * [`HomeWorker`] is the home PC's (publishing, phone edits, imports). Full build only: it needs LightCraft's engine for rendering.
//! * [`OtherWorker`] is another PC's (a copy of the chosen folders, edits sent back). Both builds.
//! * [`RoleWorker`] picks one of the two by the config's `role`, and again when the config changes. The light build has no home
//!   worker, so given a home config it refuses, in words the user can act on.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lightcraft_cloud_client::{Error, Remote};

use crate::config::{Config, Role};
use crate::lightcraft::LightCraftLink;
use crate::live::Feed;
use crate::pending::{self, PendingList};
use crate::refresh;
use crate::service::{BridgeReport, JobError, PurgeOutcome, QueueView, SyncOutcome, UploadOutcome, Worker, WorkerInfo};

#[cfg(feature = "home")]
mod home;
mod other;
#[cfg(feature = "home")]
pub use home::{HomeWorker, RendererFactory, newest_wins_notices};
pub use other::OtherWorker;

/// Opens the remote of a config (the real one is [`Config::open_remote`]; tests can count or fake it).
pub type RemoteFactory = Box<dyn Fn(&Config) -> Result<Arc<dyn Remote>, String>>;

/// Makes the link to LightCraft's control port (the real one is loopback TCP; tests give a fake).
pub type LinkFactory = Box<dyn Fn(u16) -> Box<dyn LightCraftLink>>;

/// What the light build says when it is given a home PC's config.
pub const LIGHT_BUILD_REFUSES_HOME: &str =
    "This is the light build of LightCraft Cloud, which cannot publish photos. Use the full build on the home PC, or choose \"This is another PC\" in Settings.";

/// The path the app writes its own log to.
pub fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("logs").join("lightcraft-cloud-desktop.log")
}

/// Whether a remote error means the cloud storage refused this app's login, which retrying can never fix (a failure to reach the login
/// server, on the other hand, is just being offline, even though its text also says "couldn't fetch token").
pub(crate) fn login_failed(message: &str) -> bool {
    ["invalid_grant", "invalid_client", "unauthorized_client"].iter().any(|w| message.contains(w))
}

/// What the user is told then, with the remote's name when rclone's message gives it.
pub(crate) fn login_line(message: &str) -> String {
    let remote = message.split("config reconnect ").nth(1).and_then(|r| r.split(['"', '\\', ' ']).next()).filter(|r| r.ends_with(':'));
    format!("The cloud storage login has expired or been withdrawn, so nothing can sync until it is renewed. In a terminal run: rclone config reconnect {}", remote.unwrap_or("<remote>:"))
}

pub(crate) fn to_job_error(e: Error) -> JobError {
    match e {
        Error::Busy(_) => JobError::Busy,
        Error::Invalid(m) => JobError::Refused(m),
        Error::Remote(m) if login_failed(&m) => JobError::Failed(login_line(&m)),
        e => JobError::Failed(e.to_string()),
    }
}

/// Tell a running LightCraft about the photos that wait for it (both roles; nothing when the feature is off).
pub(crate) fn flush_pending(cfg: Option<&Config>, link: &LinkFactory, retry_given_up: bool) -> BridgeReport {
    let Some(cfg) = cfg else { return BridgeReport::default() };
    let port = cfg.file.lightcraft_control_port;
    if port == 0 {
        return BridgeReport::default();
    }
    let list = PendingList::new(&cfg.state_dir);
    let mut link = link(port);
    // 1. the photos first, so that a photo that is new to the library is in it when its folder is read
    let r = pending::flush(&list, &mut *link, retry_given_up);
    let mut rep = BridgeReport { port, delivered: r.delivered, waiting: r.waiting, given_up: r.given_up, reachable: r.reachable, problems: r.problems, log: r.log, ..BridgeReport::default() };
    // 2. LightCraft's setting for sidecars (asked at every check, even with nothing to refresh) and then the folders whose sidecars were
    //    written. Not when the photos found the port silent: that costs no second try.
    if rep.reachable != Some(false) {
        let folders = refresh::list(&cfg.state_dir);
        let f = refresh::run(&folders, &mut *link, cfg.file.lightcraft_refresh_edits);
        rep.sidecars = f.auto_write;
        rep.refresh_waiting = if cfg.file.lightcraft_refresh_edits { f.waiting } else { 0 };
        rep.refreshed = f.refreshed;
        rep.reachable = f.reachable.or(rep.reachable);
        rep.log.extend(f.log);
    } else {
        rep.refresh_waiting = if cfg.file.lightcraft_refresh_edits { refresh::list(&cfg.state_dir).list().len() } else { 0 };
    }
    rep
}

/// Picks the worker for the config's role.
pub struct RoleWorker {
    config_path: PathBuf,
    /// None in the light build.
    home: Option<Box<dyn Worker>>,
    other: Box<dyn Worker>,
    role: Role,
    /// Whether a config exists (the light build with a home config is "set up", but refuses).
    configured: bool,
}

impl RoleWorker {
    pub fn new(config_path: PathBuf, home: Option<Box<dyn Worker>>, other: Box<dyn Worker>) -> RoleWorker {
        let mut w = RoleWorker { config_path, home, other, role: Role::Home, configured: false };
        let _ = w.pick();
        w
    }

    fn pick(&mut self) -> Result<(), String> {
        match std::fs::read_to_string(&self.config_path) {
            Ok(text) => {
                self.configured = true;
                // (a config that does not parse is the active worker's to complain about; it keeps what it had)
                if let Ok(f) = toml::from_str::<crate::config::File>(&text) {
                    self.role = f.role;
                }
                Ok(())
            }
            Err(_) => {
                self.configured = false;
                self.role = Role::Home;
                Ok(())
            }
        }
    }

    pub fn role(&self) -> Role {
        self.role
    }

    fn active(&mut self) -> Option<&mut Box<dyn Worker>> {
        match self.role {
            Role::Other => Some(&mut self.other),
            Role::Home => self.home.as_mut(),
        }
    }

    fn refuses(&self) -> bool {
        self.role == Role::Home && self.home.is_none() && self.configured
    }
}

impl Worker for RoleWorker {
    fn info(&self) -> WorkerInfo {
        match (self.role, &self.home) {
            (Role::Other, _) => self.other.info(),
            (Role::Home, Some(h)) => h.info(),
            // the light build with a home config: it is "set up", and each job says why it cannot run
            (Role::Home, None) => WorkerInfo { configured: self.configured, notify: true, purge: false },
        }
    }

    fn reload(&mut self) -> Result<(), String> {
        self.pick()?;
        // both read the file, so that a role changed in Settings finds the other one up to date
        let o = self.other.reload();
        let h = self.home.as_mut().map(|h| h.reload()).unwrap_or(Ok(()));
        match self.role {
            Role::Other => o,
            Role::Home => h,
        }
    }

    fn set_progress(&mut self, feed: Arc<Feed>) {
        if let Some(h) = self.home.as_mut() {
            h.set_progress(feed.clone());
        }
        self.other.set_progress(feed);
    }

    fn sync(&mut self) -> Result<SyncOutcome, JobError> {
        if self.refuses() {
            return Err(JobError::Refused(LIGHT_BUILD_REFUSES_HOME.into()));
        }
        match self.active() {
            Some(w) => w.sync(),
            None => Err(JobError::Refused("the app is not set up yet".into())),
        }
    }

    fn inbox_waiting(&mut self) -> Result<usize, String> {
        match self.active() {
            Some(w) => w.inbox_waiting(),
            None => Ok(0),
        }
    }

    fn imports_waiting(&mut self) -> Result<usize, String> {
        match self.active() {
            Some(w) => w.imports_waiting(),
            None => Ok(0),
        }
    }

    fn lightcraft_flush(&mut self, retry_given_up: bool) -> BridgeReport {
        match self.active() {
            Some(w) => w.lightcraft_flush(retry_given_up),
            None => BridgeReport::default(),
        }
    }

    fn queue_view(&mut self) -> Option<QueueView> {
        match self.role {
            Role::Other => self.other.queue_view(),
            Role::Home => None,
        }
    }

    fn send_requested(&mut self) -> bool {
        match self.role {
            Role::Other => self.other.send_requested(),
            Role::Home => false,
        }
    }

    fn upload_step(&mut self) -> Result<UploadOutcome, JobError> {
        match self.role {
            Role::Other => self.other.upload_step(),
            Role::Home => Ok(UploadOutcome::default()),
        }
    }

    fn purge(&mut self) -> Result<PurgeOutcome, JobError> {
        if self.refuses() {
            return Err(JobError::Refused(LIGHT_BUILD_REFUSES_HOME.into()));
        }
        match self.active() {
            Some(w) => w.purge(),
            None => Err(JobError::Refused("the app is not set up yet".into())),
        }
    }

    fn is_other(&self) -> bool {
        self.role == Role::Other
    }

    fn last_run(&self) -> Option<(String, String, usize)> {
        match self.role {
            Role::Other => self.other.last_run(),
            Role::Home => self.home.as_ref().and_then(|h| h.last_run()),
        }
    }
}

#[cfg(test)]
mod login_tests {
    use super::*;

    const DEAD: &str = r#"couldn't find root directory ID: Get "https://www.googleapis.com/drive/v3/files/root": couldn't fetch token: invalid_grant: maybe token expired? - try refreshing with "rclone config reconnect gdrive:""#;
    const NO_NET: &str = r#"couldn't find root directory ID: Get "https://www.googleapis.com/drive/v3/files/root": couldn't fetch token: Post "https://oauth2.googleapis.com/token": dial tcp: lookup oauth2.googleapis.com: no such host"#;

    #[test]
    fn a_refused_login_is_a_failure_that_says_what_to_do_and_no_network_is_not() {
        assert!(login_failed(DEAD) && !login_failed(NO_NET));
        let JobError::Failed(text) = to_job_error(Error::Remote(DEAD.into())) else { panic!() };
        assert!(text.ends_with("rclone config reconnect gdrive:"), "{text}");
        assert!(login_line("invalid_grant").ends_with("rclone config reconnect <remote>:"));
    }
}

#[cfg(test)]
mod flush_tests {
    use super::*;
    use crate::lightcraft::{ImportReply, LinkError, SyncReply};
    use std::sync::Mutex;

    /// A LightCraft that records what it was asked, in order, and has the setting it is told.
    #[derive(Clone)]
    struct Recorder {
        calls: Arc<Mutex<Vec<String>>>,
        auto_write: Arc<Mutex<Result<bool, LinkError>>>,
        identify: Arc<Mutex<Option<LinkError>>>,
    }

    impl LightCraftLink for Recorder {
        fn identify(&mut self) -> Result<(), LinkError> {
            self.calls.lock().unwrap().push("identify".into());
            self.identify.lock().unwrap().clone().map_or(Ok(()), Err)
        }
        fn import_add(&mut self, paths: &[String]) -> Result<ImportReply, LinkError> {
            self.calls.lock().unwrap().push(format!("import {}", paths.len()));
            Ok(ImportReply { imported: paths.len(), ..ImportReply::default() })
        }
        fn xmp_auto_write(&mut self) -> Result<bool, LinkError> {
            self.calls.lock().unwrap().push("prefs".into());
            self.auto_write.lock().unwrap().clone()
        }
        fn sync_folder(&mut self, f: &str) -> Result<SyncReply, LinkError> {
            self.calls.lock().unwrap().push(format!("sync {f}"));
            Ok(SyncReply { read: 1, ..SyncReply::default() })
        }
    }

    fn setup(tag: &str, extra: &str) -> (PathBuf, Config, Recorder) {
        let d = std::env::temp_dir().join(format!("lc-desktop-flush-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("c.toml");
        std::fs::write(&f, format!("remote = '{}'\nstate_dir = '{}'\nlightcraft_control_port = 7999\n{extra}", d.join("cloud").display(), d.join("state").display())).unwrap();
        let cfg = Config::load(Some(&f)).unwrap();
        let rec = Recorder { calls: Arc::default(), auto_write: Arc::new(Mutex::new(Ok(true))), identify: Arc::default() };
        (d, cfg, rec)
    }

    fn factory(rec: &Recorder) -> LinkFactory {
        let rec = rec.clone();
        Box::new(move |_| Box::new(rec.clone()))
    }

    #[test]
    fn photos_are_added_first_and_then_the_folders_are_read_and_the_setting_is_asked_even_with_nothing_queued() {
        let (d, cfg, rec) = setup("order", "");
        let link = factory(&rec);
        // nothing queued: only the question
        let r = flush_pending(Some(&cfg), &link, false);
        assert_eq!((r.sidecars, r.refresh_waiting, r.reachable), (Some(true), 0, Some(true)));
        assert_eq!(*rec.calls.lock().unwrap(), ["identify", "prefs"]);
        rec.calls.lock().unwrap().clear();
        // a new photo and the folder of a sidecar written beside it
        let photo = d.join("Photos").join("Trip").join("new.jpg");
        PendingList::new(&cfg.state_dir).add(&photo, "2026-11-01T10:00:00Z").unwrap();
        refresh::queue_hook(&cfg.state_dir)(&photo.with_extension("xmp")).unwrap();
        let r = flush_pending(Some(&cfg), &link, false);
        let calls = rec.calls.lock().unwrap().clone();
        let folder = photo.parent().unwrap().to_str().unwrap().to_string();
        assert_eq!(calls, ["identify", "import 1", "identify", "prefs", &format!("sync {folder}")], "the import first");
        assert_eq!((r.delivered, r.refreshed, r.refresh_waiting, r.waiting), (1, 1, 0, 0));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn autowrite_off_is_reported_and_sends_nothing_a_closed_lightcraft_keeps_the_folders_and_the_setting_off_in_the_config_does_too() {
        let (d, cfg, rec) = setup("off", "");
        let link = factory(&rec);
        let hook = refresh::queue_hook(&cfg.state_dir);
        hook(&d.join("Photos").join("A").join("a.xmp")).unwrap();
        hook(&d.join("Photos").join("B").join("b.xmp")).unwrap();
        *rec.auto_write.lock().unwrap() = Ok(false);
        let r = flush_pending(Some(&cfg), &link, false);
        assert_eq!((r.sidecars, r.refresh_waiting, r.refreshed), (Some(false), 2, 0));
        assert!(!rec.calls.lock().unwrap().iter().any(|c| c.starts_with("sync")));
        // closed
        *rec.identify.lock().unwrap() = Some(LinkError::Unreachable("refused".into()));
        let r = flush_pending(Some(&cfg), &link, false);
        assert_eq!((r.sidecars, r.refresh_waiting, r.reachable), (None, 2, Some(false)));
        *rec.identify.lock().unwrap() = None;
        // on: drained
        *rec.auto_write.lock().unwrap() = Ok(true);
        let r = flush_pending(Some(&cfg), &link, false);
        assert_eq!((r.sidecars, r.refresh_waiting, r.refreshed), (Some(true), 0, 2));
        let _ = std::fs::remove_dir_all(&d);
        // the choice off in the config: still asked (for the warning), nothing sent, nothing dropped, nothing shown as waiting
        let (d, cfg, rec) = setup("cfgoff", "lightcraft_refresh_edits = false\n");
        hook_one(&cfg, &d);
        let r = flush_pending(Some(&cfg), &factory(&rec), false);
        assert_eq!((r.sidecars, r.refreshed, r.refresh_waiting), (Some(true), 0, 0));
        assert!(!rec.calls.lock().unwrap().iter().any(|c| c.starts_with("sync")));
        assert_eq!(refresh::list(&cfg.state_dir).list().len(), 1, "kept for when it is switched on again");
        let _ = std::fs::remove_dir_all(&d);
        // the port off: nothing at all
        let (d, cfg, rec) = setup("portoff", "");
        let mut cfg = cfg;
        cfg.file.lightcraft_control_port = 0;
        assert_eq!(flush_pending(Some(&cfg), &factory(&rec), false), BridgeReport::default());
        assert!(rec.calls.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn hook_one(cfg: &Config, d: &Path) {
        refresh::queue_hook(&cfg.state_dir)(&d.join("Photos").join("A").join("a.xmp")).unwrap();
    }
}
