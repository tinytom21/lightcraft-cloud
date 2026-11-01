//! The background engine: the [`Scheduler`] says what is due, a [`Worker`] does it, and the [`Service`] keeps the books (the
//! status the windows and the tray show, the notifications, Pause). No threads, no window and no real clock in here:
//! [`Service::step`] runs one due job, [`Service::handle`] takes a command, and [`run_loop`] is the thread that drives both.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, SystemTime};

use lightcraft_cloud_client::home::utc_iso;

use crate::appstate::{Activity, AppStatus, LastSync, OtherView, Persisted};
use crate::clock::{Clock, parse_rfc3339};
use crate::live::{Feed, Pump};
use crate::notifier::{self, Notifier};
use crate::scheduler::{Ended, Job, Reason, Scheduler, Timing};

/// Why a job did not work.
#[derive(Debug, Clone, PartialEq)]
pub enum JobError {
    /// Another run (the command line's) holds the lock: try again soon, say nothing.
    Busy,
    /// A safety check said no; nothing was changed.
    Refused(String),
    /// The cloud library could not be reached (no network, a remote that is down): nothing was done, and it is not a failure to make a
    /// fuss about. The detail goes to the log; the user sees one calm line.
    Offline(String),
    Failed(String),
}

/// The one line the Problems list gets when the cloud library cannot be reached (however often that happens).
pub const OFFLINE_LINE: &str = "Cannot reach the cloud library right now (offline?). The app tries again later.";

impl JobError {
    pub fn text(&self) -> String {
        match self {
            JobError::Busy => "another run holds the lock".into(),
            JobError::Refused(m) => format!("refused: {m}"),
            JobError::Offline(m) => format!("offline: {m}"),
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
    /// The other PC's role: the numbers of the pass (None on the home PC).
    pub other: Option<OtherView>,
    /// Photos an other PC downloaded in this sync.
    pub downloaded: usize,
    /// Things the user can act on, one line each (the other PC's role).
    pub problems: Vec<String>,
    /// The other PC's role: new photos were not ready (still being copied, or not all hashed yet): look again after this long.
    pub recheck: Option<Duration>,
}

/// What telling LightCraft about the adopted photos did (see [`crate::pending`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BridgeReport {
    /// The port of LightCraft's control channel; 0: the feature is off (and the other fields are empty).
    pub port: u16,
    /// Photos LightCraft now has.
    pub delivered: usize,
    /// Photos still waiting for LightCraft.
    pub waiting: usize,
    /// Photos LightCraft would not add after several tries.
    pub given_up: usize,
    /// Whether the port answered as LightCraft; None when it was not asked.
    pub reachable: Option<bool>,
    /// Lines for Problems.
    pub problems: Vec<String>,
    /// Lines for the log.
    pub log: Vec<String>,
}

/// The photos imported on this PC and their way to the home PC, from the state folder alone.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct QueueView {
    /// Photos not sent yet, and their size.
    pub to_upload: usize,
    pub bytes: u64,
    /// Photos sent whose answer is not seen yet.
    pub awaiting_home: usize,
    /// Photos the home PC refused that the user has not dealt with.
    pub rejected: usize,
    /// Photos that were in the folder at the start and are not in the library (held back), and how many are not looked at yet.
    pub held: usize,
    pub held_unchecked: usize,
}

/// What one bounded upload job did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UploadOutcome {
    /// Photos whose commit note was written.
    pub committed: usize,
    /// Answers from the home PC seen.
    pub results: usize,
    /// Lines for Problems (a photo the home PC rejected, one that vanished before it was sent).
    pub problems: Vec<String>,
    /// The job's share of work ran out with photos left: another job follows at once.
    pub more: bool,
    /// The queue after the job.
    pub queue: QueueView,
    /// The cloud library answered (so it is not offline).
    pub network: bool,
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
    /// Tell a running LightCraft about the photos the sync added (at most a short try; never blocks for long). A worker with no
    /// LightCraft bridge does nothing. `retry_given_up`: the first try after the app started also retries the photos given up on.
    fn lightcraft_flush(&mut self, _retry_given_up: bool) -> BridgeReport {
        BridgeReport::default()
    }
    /// The upload queue, from the state folder alone (no network); None: this worker has none.
    fn queue_view(&mut self) -> Option<QueueView> {
        None
    }
    /// The user pressed "Send them": the photos that were here at the start are to be queued (a pass does it).
    fn send_requested(&mut self) -> bool {
        false
    }
    /// A bounded share of the upload queue: look for the home PC's answers, send some photos. Never holds a pass up: the work per
    /// call is limited, and the next call is a job of its own.
    fn upload_step(&mut self) -> Result<UploadOutcome, JobError> {
        Ok(UploadOutcome::default())
    }
    /// Where a job reports how far it is (see [`crate::live`]); a worker that has nothing to report ignores it. Called once, before the
    /// first job.
    fn set_progress(&mut self, _feed: Arc<Feed>) {}
    /// Whether this worker is another PC's (the status then shows that role's rows).
    fn is_other(&self) -> bool {
        false
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
    /// The upload queue changed (the Status window asked for photos to be sent, or a refused photo was dealt with).
    UploadsChanged,
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
    sink: Arc<Mutex<Sink>>,
    log: LogFn,
    quit: bool,
    /// The failure the user was last told about (the same one is not announced every 15 minutes).
    announced: Option<String>,
    /// Imports the last sync left waiting: the 2-minute check starts a sync for imports beyond these, not for the same ones
    /// again (the 15-minute sync retries those).
    imports_stuck: usize,
    /// No try to reach LightCraft has been made since the app started.
    bridge_first: bool,
    /// When the last sync wanted to be looked at again (new photos that were not ready yet).
    recheck: Option<SystemTime>,
    /// What the running job reports (see [`crate::live`]).
    feed: Arc<Feed>,
    /// How often a running job's progress is published.
    progress_every: Duration,
}

/// How long the end of a job waits for a progress write that is still going on.
const PUMP_GRACE: Duration = Duration::from_secs(2);

impl<W: Worker> Service<W> {
    /// `sink` is called with the status after every change (the tray follows it); the status is also written to
    /// `<state>/status.json` for the windows.
    pub fn new(mut worker: W, clock: Arc<dyn Clock>, notifier: Box<dyn Notifier>, state_dir: PathBuf, timing: Timing, sink: Sink, log: LogFn) -> Self {
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
        let feed = Feed::new(clock.clone());
        worker.set_progress(feed.clone());
        let sink = Arc::new(Mutex::new(sink));
        let mut s = Service {
            worker,
            sched,
            clock,
            notifier,
            state_dir,
            persisted,
            status,
            info,
            sink,
            log,
            quit: false,
            announced: None,
            imports_stuck: 0,
            bridge_first: true,
            recheck: None,
            feed,
            progress_every: Duration::from_secs(1),
        };
        s.publish();
        s.refresh_queue(true);
        s
    }

    pub fn status(&self) -> &AppStatus {
        &self.status
    }

    /// How often a running job's progress is published (default one second; tests make it short).
    pub fn set_progress_interval(&mut self, every: Duration) {
        self.progress_every = every;
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
        // (the rows of the other-PC role show as soon as the config says so, with zeros until the first pass)
        if !self.worker.is_other() {
            self.status.other = None;
        } else if self.status.other.is_none() {
            self.status.other = Some(OtherView::default());
        }
        self.status.paused = self.sched.paused();
        self.status.next_sync = self.sched.next_sync_at().map(|t| self.iso(t));
        if let Err(e) = self.status.save(&self.state_dir) {
            self.say(format!("cannot write the status: {e}"));
        }
        (self.sink.lock().unwrap_or_else(|e| e.into_inner()))(&self.status);
    }

    /// Publish the running job's progress from a thread of its own until [`Service::stop_pump`]: the job blocks this thread. The status
    /// the pump writes is this one with the progress filled in, so what it shows is exactly what [`Service::publish`] just wrote. A
    /// write that fails is noted once and ignored; one that is slow slows only the pump.
    fn start_pump(&self) -> (Pump, Arc<Mutex<Option<String>>>) {
        self.feed.reset();
        let base = self.status.clone();
        let (state_dir, sink, clock) = (self.state_dir.clone(), self.sink.clone(), self.clock.clone());
        let failure: Arc<Mutex<Option<String>>> = Arc::default();
        let f = failure.clone();
        let pump = Pump::start(self.feed.clone(), self.progress_every, move |p| {
            let mut st = base.clone();
            st.activity = base.activity.with_progress(p);
            st.updated = utc_iso(clock.now());
            if let Err(e) = st.save(&state_dir) {
                f.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
            }
            (sink.lock().unwrap_or_else(|e| e.into_inner()))(&st);
        });
        (pump, failure)
    }

    fn stop_pump(&self, (pump, failure): (Pump, Arc<Mutex<Option<String>>>)) {
        pump.stop(PUMP_GRACE);
        if let Some(e) = failure.lock().unwrap_or_else(|e| e.into_inner()).take() {
            self.say(format!("cannot write the progress: {e}"));
        }
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
            Command::UploadsChanged => {
                self.refresh_queue(false);
                if self.worker.send_requested() {
                    self.sched.request_sync(Reason::SendHeld);
                }
            }
            Command::Quit => self.quit = true,
        }
        self.publish();
    }

    /// Read the upload queue and schedule (or drop) the upload job to match: photos to send are due at once (a short wait after a
    /// start, so the first sync goes first), only answers to wait for are looked for later.
    fn refresh_queue(&mut self, starting: bool) {
        let Some(v) = self.worker.queue_view() else { return };
        let now = self.clock.now();
        let t = self.sched.timing();
        if let Some(o) = self.status.other.as_mut() {
            o.uploads_waiting = v.to_upload;
            o.uploads_bytes = v.bytes;
            o.awaiting_home = v.awaiting_home;
            o.rejected = v.rejected;
            o.held = v.held;
            o.held_unchecked = v.held_unchecked;
        }
        if v.to_upload > 0 {
            self.sched.want_upload(if starting { now + t.startup_delay } else { now });
        } else if v.awaiting_home > 0 {
            self.sched.want_upload(now + t.upload_retry);
        } else {
            self.sched.no_upload();
        }
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
            Job::Upload => self.run_upload(),
        };
        self.sched.ended(self.clock.now(), job, ended);
        if let (Job::Sync(_), Some(t)) = (job, self.recheck.take()) {
            self.sched.sync_by(t);
        }
        self.publish();
        true
    }

    fn run_sync(&mut self, reason: Reason) -> Ended {
        let since = self.iso(self.clock.now());
        self.status.activity = Activity::Syncing { since, reason: reason.label().into(), progress: None };
        self.publish();
        self.say(format!("sync ({})", reason.label()));
        let pump = self.start_pump();
        let result = self.worker.sync();
        self.stop_pump(pump);
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
                for p in &o.problems {
                    if !self.status.errors.iter().any(|e| e.text == *p) {
                        self.status.push_error(&now, p.clone());
                    }
                }
                self.status.active_problems = o.problems.iter().chain(&o.import_problems).cloned().collect();
                if let Some(mut view) = o.other.clone() {
                    view.received = self.status.other.as_ref().map_or(0, |v| v.received) + o.downloaded;
                    self.status.other = Some(view);
                }
                self.recheck = o.recheck.map(|d| self.clock.now() + d);
                self.status.offline = false;
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
                if o.downloaded > 0 && self.info.notify {
                    self.notifier.notify(notifier::TITLE, &notifier::photos_downloaded(o.downloaded));
                }
                if o.imported > 0 || o.downloaded > 0 {
                    self.run_bridge();
                }
                self.refresh_queue(false);
                Ended::Done
            }
            Err(JobError::Offline(detail)) => {
                // calm: the detail in the log, one line in Problems however often it happens, no notification, no "failed" state
                self.say(format!("sync: cannot reach the cloud library: {detail}"));
                if self.status.errors.first().is_none_or(|e| e.text != OFFLINE_LINE) {
                    self.status.push_error(&now, OFFLINE_LINE);
                }
                self.status.offline = true;
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
                // (a refusal that repeats, because nothing changed, is listed once; the other failures are listed each time, as before)
                let again = matches!(e, JobError::Refused(_)) && self.status.errors.first().is_some_and(|l| l.text == text);
                if !again {
                    self.status.push_error(&now, text.clone());
                }
                self.status.last_error = Some(text.clone());
                if self.info.notify && self.announced.as_deref() != Some(text.as_str()) {
                    self.notifier.notify(notifier::TITLE, notifier::publisher_failed());
                }
                self.announced = Some(text);
                Ended::Done
            }
        }
    }

    /// Tell LightCraft about the photos that wait for it, if it is running. Its answer never decides whether a sync happens.
    fn run_bridge(&mut self) {
        let now = self.iso(self.clock.now());
        let r = self.worker.lightcraft_flush(self.bridge_first);
        if r.port != 0 {
            self.bridge_first = false;
        }
        for l in &r.log {
            self.say(l);
        }
        self.status.lightcraft_port = r.port;
        self.status.lightcraft_waiting = r.waiting;
        self.status.lightcraft_given_up = r.given_up;
        if r.reachable.is_some() {
            self.status.lightcraft_reachable = r.reachable;
        }
        for p in &r.problems {
            if !self.status.errors.iter().any(|e| e.text == *p) {
                self.status.push_error(&now, p.clone());
            }
        }
    }

    /// One bounded share of the upload queue. Offline is calm (one amber line, tried again later); the queue is on disk, so nothing is
    /// lost by waiting.
    fn run_upload(&mut self) -> Ended {
        let since = self.iso(self.clock.now());
        self.status.activity = Activity::Uploading { since, progress: None };
        self.publish();
        let pump = self.start_pump();
        let result = self.worker.upload_step();
        self.stop_pump(pump);
        let now = self.iso(self.clock.now());
        self.status.activity = Activity::Idle;
        match result {
            Ok(o) => {
                if let Some(v) = self.status.other.as_mut() {
                    v.uploads_waiting = o.queue.to_upload;
                    v.uploads_bytes = o.queue.bytes;
                    v.awaiting_home = o.queue.awaiting_home;
                    v.rejected = o.queue.rejected;
                }
                for p in &o.problems {
                    if !self.status.errors.iter().any(|e| e.text == *p) {
                        self.status.push_error(&now, p.clone());
                    }
                }
                if o.network {
                    self.status.offline = false;
                }
                if o.committed > 0 || o.results > 0 {
                    self.say(format!("upload: {} photos sent, {} answers from the home PC", o.committed, o.results));
                }
                if o.more || o.queue.to_upload > 0 {
                    Ended::UploadMore
                } else if o.queue.awaiting_home > 0 {
                    Ended::UploadLater
                } else {
                    Ended::UploadDone
                }
            }
            Err(JobError::Offline(detail)) => {
                self.say(format!("upload: cannot reach the cloud library: {detail}"));
                if self.status.errors.first().is_none_or(|e| e.text != OFFLINE_LINE) {
                    self.status.push_error(&now, OFFLINE_LINE);
                }
                self.status.offline = true;
                Ended::UploadLater
            }
            Err(JobError::Busy) => Ended::Busy,
            Err(e) => {
                let text = e.text();
                self.say(format!("upload failed: {text}"));
                if !self.status.errors.iter().any(|l| l.text == text) {
                    self.status.push_error(&now, text);
                }
                Ended::UploadLater
            }
        }
    }

    fn run_inbox_check(&mut self) -> Ended {
        self.run_bridge();
        self.refresh_queue(false);
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
