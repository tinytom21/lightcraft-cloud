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
use crate::pending::{self, PendingList};
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

pub(crate) fn to_job_error(e: Error) -> JobError {
    match e {
        Error::Busy(_) => JobError::Busy,
        Error::Invalid(m) => JobError::Refused(m),
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
    let r = pending::flush(&list, &mut *link, retry_given_up);
    BridgeReport { port, delivered: r.delivered, waiting: r.waiting, given_up: r.given_up, reachable: r.reachable, problems: r.problems, log: r.log }
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
