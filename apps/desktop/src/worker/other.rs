//! [`OtherWorker`]: the other-PC role's jobs done for real: the sync crate's [`Other`] on the configured folder and remote. The
//! config is read again when a window saved it. No engine: this is what the light build runs.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use lightcraft_cloud_client::Error;
use lightcraft_cloud_client::home::utc_iso;
use lightcraft_cloud_client::other::{Other, OtherReport, QueueStatus, UploadBudget};

use super::{LinkFactory, RemoteFactory, flush_pending, to_job_error};
use crate::appstate::OtherView;
use crate::clock::Clock;
use crate::config::Config;
use crate::lightcraft::TcpLink;
use crate::live::Feed;
use crate::pending::PendingList;
use crate::service::{BridgeReport, JobError, PurgeOutcome, QueueView, SyncOutcome, UploadOutcome, Worker, WorkerInfo};

pub struct OtherWorker {
    config_path: PathBuf,
    cfg: Option<Config>,
    config_error: Option<String>,
    remote: RemoteFactory,
    clock: Arc<dyn Clock>,
    link: LinkFactory,
    retry_delay: Option<std::time::Duration>,
    /// Where a pass and an upload step report how far they are.
    progress: Option<Arc<Feed>>,
}

impl OtherWorker {
    pub fn new(config_path: PathBuf, remote: RemoteFactory, clock: Arc<dyn Clock>) -> OtherWorker {
        let link: LinkFactory = Box::new(|port| Box::new(TcpLink::new(port)));
        let mut w = OtherWorker { config_path, cfg: None, config_error: None, remote, clock, link, retry_delay: None, progress: None };
        let _ = w.reload();
        w
    }

    /// Replace how LightCraft is reached (tests).
    pub fn set_link_factory(&mut self, f: LinkFactory) {
        self.link = f;
    }

    /// Pause before the second and third try of a network call that failed (tests make it short; the default is the sync crate's).
    pub fn set_retry_delay(&mut self, d: std::time::Duration) {
        self.retry_delay = Some(d);
    }

    /// A config that names the local photo folder and the place the cloud library is (no folder to keep is needed: photos imported into the
    /// photo folder are sent all the same).
    fn usable(cfg: &Config) -> bool {
        cfg.remote_spec().is_ok() && !cfg.file.source.trim().is_empty()
    }

    fn the_config(&self) -> Result<&Config, JobError> {
        match (&self.cfg, &self.config_error) {
            (Some(c), _) => Ok(c),
            (None, Some(e)) => Err(JobError::Refused(e.clone())),
            (None, None) => Err(JobError::Refused("the app is not set up yet".into())),
        }
    }

    fn other(&self, cfg: &Config) -> Result<Other, JobError> {
        let remote = (self.remote)(cfg).map_err(JobError::Failed)?;
        let mut other = Other::new(cfg.other_config().map_err(JobError::Refused)?, remote).map_err(to_job_error)?;
        other.set_clock({
            let c = self.clock.clone();
            move || c.now()
        });
        if let Some(d) = self.retry_delay {
            other.set_retry_delay(d);
        }
        if let Some(feed) = &self.progress {
            let feed = feed.clone();
            other.set_progress_hook(move |p| feed.report(p));
        }
        // (the photos this PC downloads are queued for LightCraft in the same durable list the home PC's adopted photos use)
        if cfg.file.lightcraft_control_port != 0 {
            let list = PendingList::new(&cfg.state_dir);
            other.set_adopted_hook(move |p| list.add(p, &utc_iso(std::time::SystemTime::now())));
            // (and the folders where the home PC's sidecars replace this PC's, for the same LightCraft to read again)
            if cfg.file.lightcraft_refresh_edits {
                other.set_sidecar_hook(crate::refresh::queue_hook(&cfg.state_dir));
            }
        }
        let logfile = super::log_path(&cfg.state_dir);
        other.set_logger(move |m| {
            let line = format!("{} {m}", utc_iso(std::time::SystemTime::now()));
            log::info!("{m}");
            if let Some(dir) = logfile.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&logfile) {
                let _ = writeln!(f, "{line}");
            }
        });
        Ok(other)
    }
}

/// An error of a pass as a job error: a network failure is calm, a newer format or a bad setting is a refusal.
fn pass_error(e: Error) -> JobError {
    match e {
        Error::Remote(m) if !super::login_failed(&m) => JobError::Offline(m),
        Error::Format(m) => JobError::Refused(m),
        Error::NotFound(_) => JobError::Failed("The cloud library has no photos yet: the home PC has not published anything there.".into()),
        e => to_job_error(e),
    }
}

/// The numbers of a pass for the Status window and the notifications.
pub(crate) fn outcome_of(r: &OtherReport) -> SyncOutcome {
    SyncOutcome {
        photos: r.photos,
        downloaded: r.downloaded.len(),
        problems: r.problems.clone(),
        other: Some(OtherView {
            photos: r.photos,
            waiting_to_download: r.waiting_to_download,
            edits_waiting: r.edits_waiting,
            held_back: r.held_back,
            received: 0,
            uploads_waiting: r.queue.to_upload,
            uploads_bytes: r.queue.bytes,
            awaiting_home: r.queue.awaiting_home,
            rejected: r.queue.rejected,
            held: r.queue.held,
            held_unchecked: r.queue.held_unchecked,
        }),
        recheck: r.recheck,
        sidecars: r.sidecars_updated.len(),
        ..SyncOutcome::default()
    }
}

fn view_of(q: &QueueStatus) -> QueueView {
    QueueView {
        to_upload: q.to_upload,
        bytes: q.bytes,
        awaiting_home: q.awaiting_home,
        rejected: q.rejected,
        held: q.held,
        held_unchecked: q.held_unchecked,
    }
}

impl Worker for OtherWorker {
    fn info(&self) -> WorkerInfo {
        match &self.cfg {
            Some(c) => WorkerInfo { configured: Self::usable(c), notify: c.file.notify, purge: false },
            None => WorkerInfo { configured: false, notify: true, purge: false },
        }
    }

    fn reload(&mut self) -> Result<(), String> {
        if !self.config_path.exists() {
            self.cfg = None;
            self.config_error = None;
            return Ok(());
        }
        match Config::load(Some(&self.config_path)) {
            Ok(c) => {
                self.cfg = Some(c);
                self.config_error = None;
                Ok(())
            }
            Err(e) => {
                // (keep working with the settings that worked)
                self.config_error = Some(e.clone());
                Err(e)
            }
        }
    }

    fn sync(&mut self) -> Result<SyncOutcome, JobError> {
        let cfg = self.the_config()?;
        if !Self::usable(cfg) {
            return Err(JobError::Refused("the app is not set up yet".into()));
        }
        let other = self.other(cfg)?;
        let report = other.sync().map_err(pass_error);
        let report = report?;
        // every folder the user imported into is a folder this PC keeps now (a chosen folder means the photos directly in it)
        let mut added = false;
        for f in &report.new_folders {
            match crate::config::add_chosen_folder(&self.config_path, f) {
                Ok(a) => added |= a,
                Err(e) => log::warn!("cannot add the folder {f:?} to the chosen folders: {e}"),
            }
        }
        if added {
            let _ = self.reload();
        }
        Ok(outcome_of(&report))
    }

    fn set_progress(&mut self, feed: Arc<Feed>) {
        self.progress = Some(feed);
    }

    /// Nothing to poll: this PC does not look at other devices' edits.
    fn inbox_waiting(&mut self) -> Result<usize, String> {
        Ok(0)
    }

    fn lightcraft_flush(&mut self, retry_given_up: bool) -> BridgeReport {
        flush_pending(self.cfg.as_ref(), &self.link, retry_given_up)
    }

    fn queue_view(&mut self) -> Option<QueueView> {
        let cfg = self.cfg.as_ref()?;
        Some(view_of(&lightcraft_cloud_client::other::ImportQueue::new(&cfg.state_dir).status()))
    }

    fn send_requested(&mut self) -> bool {
        self.cfg.as_ref().is_some_and(|c| lightcraft_cloud_client::other::ImportQueue::new(&c.state_dir).send_requested())
    }

    fn upload_step(&mut self) -> Result<UploadOutcome, JobError> {
        let cfg = self.the_config()?;
        if cfg.remote_spec().is_err() || cfg.file.source.trim().is_empty() {
            return Ok(UploadOutcome::default());
        }
        let other = self.other(cfg)?;
        let r = other.upload_step(&UploadBudget::default()).map_err(pass_error)?;
        Ok(UploadOutcome {
            committed: r.committed,
            results: r.results.len(),
            problems: r.problems,
            more: r.more,
            queue: view_of(&r.queue),
            network: r.files_sent > 0 || r.committed > 0 || !r.results.is_empty(),
        })
    }

    fn is_other(&self) -> bool {
        true
    }

    fn purge(&mut self) -> Result<PurgeOutcome, JobError> {
        Ok(PurgeOutcome::Done { folders: 0, versions: 0 })
    }

    fn last_run(&self) -> Option<(String, String, usize)> {
        let cfg = self.cfg.as_ref()?;
        let o = Other::new(cfg.other_config().ok()?, Arc::new(lightcraft_cloud_client::DirRemote::new("."))).ok()?;
        let s = o.status().ok()?;
        Some((s.last_pass?, "ok".to_string(), s.photos))
    }
}
