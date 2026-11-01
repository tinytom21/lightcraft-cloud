//! The background engine: the [`Scheduler`] says what is due, a [`Worker`] does it, and the [`Service`] keeps the books (the
//! status the windows and the tray show, the notifications, Pause). No threads, no window and no real clock in here:
//! [`Service::step`] runs one due job, [`Service::handle`] takes a command, and [`run_loop`] is the thread that drives both.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, SystemTime};

use lightcraft_cloud_client::home::utc_iso;

use crate::appstate::{Activity, AppStatus, LastSync, Persisted};
use crate::clock::{Clock, parse_rfc3339};
use crate::notifier::{self, Notifier};
use crate::scheduler::{Ended, Job, Reason, Scheduler, Timing};

/// Why a job did not work.
#[derive(Debug, Clone, PartialEq)]
pub enum JobError {
    /// Another run (the command line's) holds the lock: try again soon, say nothing.
    Busy,
    /// A safety check said no; nothing was changed.
    Refused(String),
    Failed(String),
}

impl JobError {
    pub fn text(&self) -> String {
        match self {
            JobError::Busy => "another run holds the lock".into(),
            JobError::Refused(m) => format!("refused: {m}"),
            JobError::Failed(m) => m.clone(),
        }
    }
}

/// What a sync did (the numbers the Status window and the notifications use).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncOutcome {
    /// Photos in the published manifest.
    pub photos: usize,
    /// Phone edits applied as they were (not counting merged ones).
    pub applied: usize,
    pub merged: usize,
    /// Phone edits that lost to a newer PC sidecar.
    pub kept: usize,
    /// Folders (relative to the photo folder) with applied or merged edits.
    pub folders: Vec<String>,
    /// The photo folder's name (for "in <folder>" in the notification).
    pub source_name: String,
    /// "Newest wins" decisions, one line each.
    pub notices: Vec<String>,
    /// Photos the renderer could not handle.
    pub failed: usize,
    /// Photos other PCs uploaded that this sync added to the photo folder (not phone edits).
    pub imported: usize,
    /// Those photos by the device that sent them, for the notification.
    pub import_devices: Vec<(String, usize)>,
    /// Imports still waiting on the remote after the sync (no disk space, files not all uploaded yet, ...).
    pub imports_left: usize,
    /// Things about the imports the user can act on (disk full, a folder that cannot be written), one line each.
    pub import_problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PurgeOutcome {
    Done { folders: usize, versions: usize },
}

/// What the service needs to know about the settings.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerInfo {
    /// There is a config with a photo folder and a place to publish to.
    pub configured: bool,
    pub notify: bool,
    /// The weekly purge is on (a `photo_backup` command is set).
    pub purge: bool,
}

pub trait Worker {
    fn info(&self) -> WorkerInfo;
    /// Read the settings again (they were saved in a window).
    fn reload(&mut self) -> Result<(), String>;
    /// One full sync: apply phone edits, publish the photo folder.
    fn sync(&mut self) -> Result<SyncOutcome, JobError>;
    /// The cheap inbox poll: how many phone edits wait.
    fn inbox_waiting(&mut self) -> Result<usize, String>;
    /// The cheap poll for photos other PCs uploaded: how many committed imports wait (one listing). A worker with no imports
    /// says none.
    fn imports_waiting(&mut self) -> Result<usize, String> {
        Ok(0)
    }
    /// The weekly purge.
    fn purge(&mut self) -> Result<PurgeOutcome, JobError>;
    /// What the sync state says about the last run, for the first status after a start: (ISO time, result, photos published).
    fn last_run(&self) -> Option<(String, String, usize)>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    SyncNow,
    Pause,
    Resume,
    /// A change in the photo folder (already filtered by the watcher).
    FolderChanged,
    /// The config file changed.
    ConfigChanged,
    Quit,
}

type Sink = Box<dyn FnMut(&AppStatus) + Send>;
type LogFn = Box<dyn Fn(&str) + Send>;

pub struct Service<W: Worker> {
    worker: W,
    sched: Scheduler,
    clock: Arc<dyn Clock>,
    notifier: Box<dyn Notifier>,
    state_dir: PathBuf,
    persisted: Persisted,
    status: AppStatus,
    info: WorkerInfo,
    sink: Sink,
    log: LogFn,
    quit: bool,
    /// The failure the user was last told about (the same one is not announced every 15 minutes).
    announced: Option<String>,
    /// Imports the last sync left waiting: the 2-minute check starts a sync for imports beyond these, not for the same ones
    /// again (the 15-minute sync retries those).
    imports_stuck: usize,
}

impl<W: Worker> Service<W> {
    /// `sink` is called with the status after every change (the tray follows it); the status is also written to
    /// `<state>/status.json` for the windows.
    pub fn new(worker: W, clock: Arc<dyn Clock>, notifier: Box<dyn Notifier>, state_dir: PathBuf, timing: Timing, sink: Sink, log: LogFn) -> Self {
        let now = clock.now();
        let persisted = Persisted::load(&state_dir);
        let info = worker.info();
        let last_purge = persisted.last_purge.as_deref().and_then(parse_rfc3339);
        let sched = Scheduler::new(now, timing, info.configured, persisted.paused, last_purge);
        let mut status = AppStatus::load(&state_dir).unwrap_or_default();
        status.activity = Activity::Idle;
        status.last_error = None; // (a failure of the last run is not this run's)
        if status.last_sync.is_none()
            && let Some((when, result, photos)) = worker.last_run()
        {
            status.last_sync = Some(LastSync { when, ok: result.starts_with("ok"), result, photos, applied: 0, merged: 0, kept: 0 });
        }
        let mut s = Service { worker, sched, clock, notifier, state_dir, persisted, status, info, sink, log, quit: false, announced: None, imports_stuck: 0 };
        s.publish();
        s
    }

    pub fn status(&self) -> &AppStatus {
        &self.status
    }

    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    fn iso(&self, t: SystemTime) -> String {
        utc_iso(t)
    }

    fn say(&self, m: impl AsRef<str>) {
        (self.log)(m.as_ref());
    }

    /// Update the status from the schedule, write it, and tell the sink.
    fn publish(&mut self) {
        self.status.updated = self.iso(self.clock.now());
        self.status.configured = self.info.configured;
        self.status.paused = self.sched.paused();
        self.status.next_sync = self.sched.next_sync_at().map(|t| self.iso(t));
        if let Err(e) = self.status.save(&self.state_dir) {
            self.say(format!("cannot write the status: {e}"));
        }
        (self.sink)(&self.status);
    }

    fn save_persisted(&self) {
        if let Err(e) = self.persisted.save(&self.state_dir) {
            self.say(format!("cannot save the app's state: {e}"));
        }
    }

    pub fn handle(&mut self, cmd: Command) {
        let now = self.clock.now();
        match cmd {
            Command::SyncNow => self.sched.request_sync(Reason::Manual),
            Command::Pause | Command::Resume => {
                let paused = cmd == Command::Pause;
                self.sched.set_paused(now, paused);
                self.persisted.paused = paused;
                self.save_persisted();
                self.say(if paused { "paused" } else { "resumed" });
            }
            Command::FolderChanged => self.sched.folder_event(now),
            Command::ConfigChanged => match self.worker.reload() {
                Ok(()) => {
                    self.info = self.worker.info();
                    self.sched.set_enabled(now, self.info.configured);
                    if self.info.configured {
                        self.sched.request_sync(Reason::Settings);
                    }
                    self.say(format!("settings reloaded (configured: {})", self.info.configured));
                }
                Err(e) => {
                    self.say(format!("settings not reloaded: {e}"));
                    self.status.push_error(&self.iso(now), format!("settings: {e}"));
                }
            },
            Command::Quit => self.quit = true,
        }
        self.publish();
    }

    /// When to look again (see [`Scheduler::next_wake`]).
    pub fn next_wake(&self) -> Option<SystemTime> {
        self.sched.next_wake()
    }

    /// Run the job that is due, if there is one. Returns whether one ran.
    pub fn step(&mut self) -> bool {
        let Some(job) = self.sched.poll(self.clock.now()) else { return false };
        if job == Job::Purge && !self.info.purge {
            // (switched off: look again next week)
            self.sched.started(job);
            self.sched.ended(self.clock.now(), job, Ended::Done);
            return true;
        }
        self.sched.started(job);
        let ended = match job {
            Job::Sync(reason) => self.run_sync(reason),
            Job::InboxCheck => self.run_inbox_check(),
            Job::Purge => self.run_purge(),
        };
        self.sched.ended(self.clock.now(), job, ended);
        self.publish();
        true
    }

    fn run_sync(&mut self, reason: Reason) -> Ended {
        let since = self.iso(self.clock.now());
        self.status.activity = Activity::Syncing { since, reason: reason.label().into() };
        self.publish();
        self.say(format!("sync ({})", reason.label()));
        let result = self.worker.sync();
        let now = self.iso(self.clock.now());
        self.status.activity = Activity::Idle;
        match result {
            Ok(o) => {
                let n = o.applied + o.merged;
                self.status.edits_applied += n;
                self.status.inbox_pending = Some(0);
                self.status.imports_pending = Some(o.imports_left);
                self.imports_stuck = o.imports_left;
                self.status.photos_imported += o.imported;
                self.status.last_imported = o.imported;
                for p in &o.import_problems {
                    if !self.status.errors.iter().any(|e| e.text == *p) {
                        self.status.push_error(&now, p.clone());
                    }
                }
                self.status.last_error = None;
                self.announced = None;
                let result = if o.failed == 0 { "ok".to_string() } else { format!("ok, {} photos failed", o.failed) };
                self.status.last_sync =
                    Some(LastSync { when: now.clone(), ok: true, result, photos: o.photos, applied: o.applied, merged: o.merged, kept: o.kept });
                for line in &o.notices {
                    self.status.push_notice(&now, line.clone());
                }
                if n > 0 && self.info.notify {
                    self.notifier.notify(notifier::TITLE, &notifier::edits_applied(n, o.merged, &o.folders, &o.source_name));
                }
                if o.imported > 0 && self.info.notify {
                    self.notifier.notify(notifier::TITLE, &notifier::photos_added(&o.import_devices));
                }
                if o.kept > 0 && self.info.notify {
                    self.notifier.notify(notifier::TITLE, &notifier::edits_kept(o.kept));
                }
                Ended::Done
            }
            Err(JobError::Busy) => {
                self.say("sync skipped: another run holds the lock");
                Ended::Busy
            }
            Err(e) => {
                let text = e.text();
                self.say(format!("sync failed: {text}"));
                let photos = self.status.last_sync.as_ref().map_or(0, |l| l.photos); // (what was published before still is)
                self.status.last_sync = Some(LastSync { when: now.clone(), ok: false, result: text.clone(), photos, applied: 0, merged: 0, kept: 0 });
                self.status.push_error(&now, text.clone());
                self.status.last_error = Some(text.clone());
                if self.info.notify && self.announced.as_deref() != Some(text.as_str()) {
                    self.notifier.notify(notifier::TITLE, notifier::publisher_failed());
                }
                self.announced = Some(text);
                Ended::Done
            }
        }
    }

    fn run_inbox_check(&mut self) -> Ended {
        let mut waiting = 0;
        match self.worker.inbox_waiting() {
            Ok(n) => {
                self.status.inbox_pending = Some(n);
                if n > 0 {
                    self.say(format!("inbox: {n} pending, starting sync"));
                }
                waiting += n;
            }
            // (a network error is logged and swallowed, like `inbox-check`)
            Err(e) => self.say(format!("inbox: check failed: {e}")),
        }
        match self.worker.imports_waiting() {
            Ok(n) => {
                self.status.imports_pending = Some(n);
                // (imports the last sync left waiting do not start a sync again; a new one does)
                self.imports_stuck = self.imports_stuck.min(n);
                let new = n - self.imports_stuck;
                if new > 0 {
                    self.say(format!("imports: {n} waiting, starting sync"));
                }
                waiting += new;
            }
            Err(e) => self.say(format!("imports: check failed: {e}")),
        }
        if waiting > 0 { Ended::Waiting(waiting) } else { Ended::Done }
    }

    fn run_purge(&mut self) -> Ended {
        let now = self.iso(self.clock.now());
        match self.worker.purge() {
            Ok(PurgeOutcome::Done { folders, versions }) => {
                self.say(format!("purge: {folders} history folders and {versions} stored sidecar versions removed"));
            }
            Err(JobError::Busy) => {}
            Err(e) => {
                // (no recent backup is the rule doing its job, not a failure of the sync: a line in the Status window)
                self.say(format!("purge: {}", e.text()));
                self.status.push_notice(&now, format!("History purge skipped: {}", e.text()));
            }
        }
        self.persisted.last_purge = Some(now.clone());
        self.status.last_purge = Some(now);
        self.save_persisted();
        Ended::Done
    }
}

/// The longest the loop sleeps without looking (the clock may have jumped: the machine slept).
const LONGEST_SLEEP: Duration = Duration::from_secs(30);

/// Drive a service from a thread until Quit (or until every sender is gone).
pub fn run_loop<W: Worker>(service: &mut Service<W>, rx: &Receiver<Command>, clock: &dyn Clock) {
    loop {
        while service.step() {
            while let Ok(c) = rx.try_recv() {
                service.handle(c);
            }
            if service.quit_requested() {
                return;
            }
        }
        if service.quit_requested() {
            return;
        }
        let wait = service
            .next_wake()
            .map_or(LONGEST_SLEEP, |t| t.duration_since(clock.now()).unwrap_or(Duration::ZERO))
            .clamp(Duration::from_millis(5), LONGEST_SLEEP);
        match rx.recv_timeout(wait) {
            Ok(c) => service.handle(c),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
