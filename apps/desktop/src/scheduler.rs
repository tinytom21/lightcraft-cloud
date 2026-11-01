//! When the background engine does what. Pure: it is told the time and the events, and says which job is due; it sleeps
//! and runs nothing, so tests move a fake clock.
//!
//! The rules (the Python tool's systemd units and watcher, in one place):
//!
//! * a full sync every 15 minutes (counted from the end of the last one), the first shortly after start;
//! * a cheap inbox check every 2 minutes; it starts a sync only when phone edits wait;
//! * a folder change starts a sync once the folder has been quiet for 30 seconds, or 5 minutes after the first change at
//!   the latest (so a copy that never ends cannot hold it off for ever);
//! * a purge about once a week;
//! * photos imported on this PC go to the home PC in bounded jobs of their own (lowest priority, so a sync for a sidecar edit never
//!   waits behind them): the next one a moment after the last if work is left, later when offline or when only the home PC's
//!   answer is awaited, and none while paused;
//! * "Sync now" runs at once, even while paused; Pause stops everything else (and forgets the folder changes seen) until
//!   Resume, which syncs soon after;
//! * one job at a time: the caller runs [`Scheduler::poll`]'s job to the end before asking again.

use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub sync_every: Duration,
    pub inbox_every: Duration,
    /// Quiet time after the last folder change before the sync starts.
    pub quiet: Duration,
    /// The longest a sync waits after the first folder change.
    pub max_wait: Duration,
    pub purge_every: Duration,
    /// From start to the first sync, and to a purge that is already due.
    pub startup_delay: Duration,
    /// Before trying again when another run (the command line's) held the lock.
    pub busy_retry: Duration,
    /// Between two bounded upload jobs while photos are left to send.
    pub upload_yield: Duration,
    /// Before the next upload job when offline, or while only the home PC's answers are awaited.
    pub upload_retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            sync_every: Duration::from_secs(15 * 60),
            inbox_every: Duration::from_secs(2 * 60),
            quiet: Duration::from_secs(30),
            max_wait: Duration::from_secs(5 * 60),
            purge_every: Duration::from_secs(7 * 86_400),
            startup_delay: Duration::from_secs(20),
            busy_retry: Duration::from_secs(60),
            upload_yield: Duration::from_secs(1),
            upload_retry: Duration::from_secs(5 * 60),
        }
    }
}

/// Why a sync runs (shown in the Status window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Startup,
    Timer,
    /// The inbox check found phone edits.
    PhoneEdits,
    FolderChanged,
    /// "Sync now".
    Manual,
    Resumed,
    /// Settings were saved.
    Settings,
    /// The user asked for the photos that were already in the folder to be sent.
    SendHeld,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::Startup => "start",
            Reason::Timer => "every 15 minutes",
            Reason::PhoneEdits => "phone edits waiting",
            Reason::FolderChanged => "photos changed",
            Reason::Manual => "Sync now",
            Reason::Resumed => "resumed",
            Reason::Settings => "settings saved",
            Reason::SendHeld => "sending the photos that were here",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Sync(Reason),
    InboxCheck,
    Purge,
    /// A bounded share of the photos imported on this PC, sent to the home PC.
    Upload,
}

/// How a job ended, as far as the schedule cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Done,
    /// The sync could not start because another run held the lock.
    Busy,
    /// An inbox check that found this many phone edits waiting.
    Waiting(usize),
    /// An upload job that left photos to send: the next one follows at once.
    UploadMore,
    /// An upload job that has nothing more to send now: look again later (offline, or awaiting the home PC's answers).
    UploadLater,
    /// An upload job that found the queue empty (or could not start because another run held the lock and nothing else is known).
    UploadDone,
}

pub struct Scheduler {
    t: Timing,
    enabled: bool,
    paused: bool,
    next_sync: SystemTime,
    next_inbox: SystemTime,
    next_purge: Option<SystemTime>,
    /// When the next upload job is due; None: nothing to send.
    next_upload: Option<SystemTime>,
    /// A sync asked for (Sync now, phone edits found, settings saved).
    requested: Option<Reason>,
    /// The first and the last folder change since the last sync started.
    folder: Option<(SystemTime, SystemTime)>,
    /// The first sync after start (or after setup) is a `Startup` one.
    first: bool,
}

impl Scheduler {
    /// `last_purge`: when the purge last ran (None: never; the first is due `startup_delay` after the first sync would be).
    pub fn new(now: SystemTime, t: Timing, enabled: bool, paused: bool, last_purge: Option<SystemTime>) -> Self {
        let next_purge = Some(match last_purge {
            Some(p) => (p + t.purge_every).max(now + t.startup_delay * 2),
            None => now + t.startup_delay * 2,
        });
        Scheduler {
            t,
            enabled,
            paused,
            next_sync: now + t.startup_delay,
            next_inbox: now + t.inbox_every,
            next_purge,
            next_upload: None,
            requested: None,
            folder: None,
            first: true,
        }
    }

    pub fn timing(&self) -> Timing {
        self.t
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether there is anything to run yet (a config). Becoming enabled schedules a sync at once.
    pub fn set_enabled(&mut self, now: SystemTime, enabled: bool) {
        if enabled && !self.enabled {
            self.next_sync = now;
            self.first = true;
        }
        self.enabled = enabled;
    }

    pub fn set_paused(&mut self, now: SystemTime, paused: bool) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        if paused {
            self.folder = None;
        } else {
            // (what changed meanwhile is found by the full sync)
            self.next_sync = self.next_sync.min(now);
            self.requested.get_or_insert(Reason::Resumed);
        }
    }

    /// A sync is wanted by `at` at the latest (photos that were not ready are looked at again): an earlier one stays.
    pub fn sync_by(&mut self, at: SystemTime) {
        self.next_sync = self.next_sync.min(at);
    }

    /// "Sync now", or a sync another part asked for.
    pub fn request_sync(&mut self, reason: Reason) {
        self.requested = Some(reason);
    }

    /// Photos wait to be sent: an upload job is due at `at` (an earlier time already set stays).
    pub fn want_upload(&mut self, at: SystemTime) {
        self.next_upload = Some(self.next_upload.map_or(at, |t| t.min(at)));
    }

    /// The queue is empty: no upload job is due.
    pub fn no_upload(&mut self) {
        self.next_upload = None;
    }

    pub fn upload_due(&self) -> Option<SystemTime> {
        self.next_upload
    }

    /// A relevant change in the photo folder.
    pub fn folder_event(&mut self, now: SystemTime) {
        if self.paused || !self.enabled {
            return;
        }
        let first = self.folder.map_or(now, |(f, _)| f);
        self.folder = Some((first, now));
    }

    fn folder_due(&self) -> Option<SystemTime> {
        self.folder.map(|(first, last)| (last + self.t.quiet).min(first + self.t.max_wait))
    }

    /// The job that is due, if any. Does not change the schedule: [`Scheduler::started`] and [`Scheduler::ended`] do.
    pub fn poll(&self, now: SystemTime) -> Option<Job> {
        if !self.enabled {
            return None;
        }
        if let Some(r) = self.requested {
            return Some(Job::Sync(r));
        }
        if self.paused {
            return None;
        }
        if self.folder_due().is_some_and(|t| t <= now) {
            return Some(Job::Sync(Reason::FolderChanged));
        }
        if self.next_sync <= now {
            return Some(Job::Sync(if self.first { Reason::Startup } else { Reason::Timer }));
        }
        if self.next_inbox <= now {
            return Some(Job::InboxCheck);
        }
        if self.next_purge.is_some_and(|t| t <= now) {
            return Some(Job::Purge);
        }
        if self.next_upload.is_some_and(|t| t <= now) {
            return Some(Job::Upload);
        }
        None
    }

    /// A job is about to run: the requests and folder changes it covers are taken.
    pub fn started(&mut self, job: Job) {
        if let Job::Sync(_) = job {
            self.requested = None;
            self.folder = None;
            self.first = false;
        }
    }

    /// A job ended (at `now`): schedule the next one.
    pub fn ended(&mut self, now: SystemTime, job: Job, how: Ended) {
        match (job, how) {
            (Job::Sync(_), Ended::Busy) => {
                self.next_sync = now + self.t.busy_retry;
                self.next_inbox = self.next_inbox.max(now + self.t.busy_retry);
            }
            (Job::Sync(_), _) => {
                self.next_sync = now + self.t.sync_every;
                self.next_inbox = now + self.t.inbox_every; // (the sync read the inbox)
            }
            (Job::InboxCheck, how) => {
                self.next_inbox = now + self.t.inbox_every;
                if let Ended::Waiting(n) = how
                    && n > 0
                {
                    self.requested.get_or_insert(Reason::PhoneEdits);
                }
            }
            (Job::Purge, _) => self.next_purge = Some(now + self.t.purge_every),
            (Job::Upload, Ended::UploadMore) => self.next_upload = Some(now + self.t.upload_yield),
            (Job::Upload, Ended::Busy) => self.next_upload = Some(now + self.t.busy_retry),
            (Job::Upload, Ended::UploadLater) => self.next_upload = Some(now + self.t.upload_retry),
            (Job::Upload, _) => self.next_upload = None,
        }
    }

    /// The next scheduled full sync (None while paused or not set up), or the folder-change sync if that comes first.
    pub fn next_sync_at(&self) -> Option<SystemTime> {
        if !self.enabled || self.paused {
            return None;
        }
        Some(self.folder_due().map_or(self.next_sync, |f| f.min(self.next_sync)))
    }

    /// When to look again: the earliest time something becomes due (the epoch: something is due now).
    pub fn next_wake(&self) -> Option<SystemTime> {
        if !self.enabled {
            return None;
        }
        if self.requested.is_some() {
            return Some(SystemTime::UNIX_EPOCH);
        }
        if self.paused {
            return None;
        }
        [self.folder_due(), Some(self.next_sync), Some(self.next_inbox), self.next_purge, self.next_upload].into_iter().flatten().min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, FakeClock};

    const S: fn(u64) -> Duration = Duration::from_secs;

    fn setup() -> (FakeClock, Scheduler) {
        let c = FakeClock::new();
        let s = Scheduler::new(c.now(), Timing::default(), true, false, Some(c.now()));
        (c, s)
    }

    /// Like `setup`, but the first sync is an hour away, so folder changes are the only thing due.
    fn setup_late() -> (FakeClock, Scheduler) {
        let c = FakeClock::new();
        let t = Timing { startup_delay: S(3600), ..Timing::default() };
        let s = Scheduler::new(c.now(), t, true, false, Some(c.now()));
        (c, s)
    }

    /// Run whatever is due; the job `Done`.
    fn run(c: &FakeClock, s: &mut Scheduler) -> Option<Job> {
        let job = s.poll(c.now())?;
        s.started(job);
        s.ended(c.now(), job, Ended::Done);
        Some(job)
    }

    #[test]
    fn nothing_is_due_at_the_start_and_the_first_sync_follows_the_startup_delay() {
        let (c, mut s) = setup();
        assert_eq!(s.poll(c.now()), None);
        c.advance(S(19));
        assert_eq!(s.poll(c.now()), None);
        c.advance(S(1));
        assert_eq!(run(&c, &mut s), Some(Job::Sync(Reason::Startup)));
        assert_eq!(s.poll(c.now()), None);
    }

    #[test]
    fn a_full_sync_every_15_minutes_from_the_end_of_the_last() {
        let (c, mut s) = setup();
        c.advance(S(20));
        run(&c, &mut s); // the startup sync ends now
        let start = c.now();
        let (mut syncs, mut checks) = (Vec::new(), 0);
        for _ in 0..(31 * 60 / 10) {
            c.advance(S(10));
            while let Some(j) = run(&c, &mut s) {
                match j {
                    Job::Sync(r) => syncs.push((c.now().duration_since(start).unwrap().as_secs(), r)),
                    Job::InboxCheck => checks += 1,
                    Job::Purge | Job::Upload => {}
                }
            }
        }
        assert_eq!(syncs, vec![(900, Reason::Timer), (1800, Reason::Timer)]);
        assert_eq!(checks, 14, "one check every 2 minutes in between, none right after a sync");
    }

    #[test]
    fn the_inbox_is_checked_every_2_minutes_and_phone_edits_start_a_sync() {
        let (c, mut s) = setup();
        c.advance(S(20));
        run(&c, &mut s); // startup sync; inbox clock restarts
        c.advance(S(119));
        assert_eq!(s.poll(c.now()), None);
        c.advance(S(1));
        let job = s.poll(c.now()).unwrap();
        assert_eq!(job, Job::InboxCheck);
        s.started(job);
        s.ended(c.now(), job, Ended::Waiting(0));
        assert_eq!(s.poll(c.now()), None); // nothing waiting: no sync
        c.advance(S(120));
        let job = s.poll(c.now()).unwrap();
        s.started(job);
        s.ended(c.now(), job, Ended::Waiting(3));
        assert_eq!(s.poll(c.now()), Some(Job::Sync(Reason::PhoneEdits))); // at once, not at the next 15-minute mark
        s.started(Job::Sync(Reason::PhoneEdits));
        s.ended(c.now(), Job::Sync(Reason::PhoneEdits), Ended::Done);
        assert_eq!(s.poll(c.now()), None);
    }

    #[test]
    fn folder_changes_wait_for_30_quiet_seconds_but_not_for_ever() {
        let (c, mut s) = setup_late();
        s.folder_event(c.now());
        c.advance(S(29));
        assert_eq!(s.poll(c.now()), None);
        s.folder_event(c.now()); // another change: the quiet window restarts
        c.advance(S(29));
        assert_eq!(s.poll(c.now()), None);
        c.advance(S(1));
        // (the startup delay also passed meanwhile; the folder sync is the one that is due first)
        assert_eq!(s.poll(c.now()), Some(Job::Sync(Reason::FolderChanged)));
        s.started(Job::Sync(Reason::FolderChanged));
        s.ended(c.now(), Job::Sync(Reason::FolderChanged), Ended::Done);
        assert_eq!(s.poll(c.now()), None);

        // a change every 20 s never goes quiet: the sync starts 5 minutes after the first
        let (c, mut s) = setup_late();
        let mut waited = 0;
        s.folder_event(c.now());
        while !matches!(s.poll(c.now()), Some(Job::Sync(Reason::FolderChanged))) {
            c.advance(S(20));
            waited += 20;
            s.folder_event(c.now());
            assert!(waited <= 320, "never started");
        }
        assert!((280..=320).contains(&waited), "{waited}");
    }

    #[test]
    fn changes_during_a_sync_cause_another_one_afterwards() {
        let (c, mut s) = setup_late();
        s.folder_event(c.now());
        c.advance(S(30));
        let job = s.poll(c.now()).unwrap();
        s.started(job);
        s.folder_event(c.now()); // while it runs
        s.ended(c.now(), job, Ended::Done);
        assert_eq!(s.poll(c.now()), None);
        c.advance(S(30));
        assert_eq!(s.poll(c.now()), Some(Job::Sync(Reason::FolderChanged)));
    }

    #[test]
    fn sync_now_runs_at_once_even_while_paused() {
        let (c, mut s) = setup();
        s.set_paused(c.now(), true);
        s.request_sync(Reason::Manual);
        assert_eq!(s.poll(c.now()), Some(Job::Sync(Reason::Manual)));
        s.started(Job::Sync(Reason::Manual));
        s.ended(c.now(), Job::Sync(Reason::Manual), Ended::Done);
        assert_eq!(s.poll(c.now()), None);
        assert!(s.paused());
    }

    #[test]
    fn pause_stops_the_timers_the_watcher_and_the_inbox_until_resume() {
        let (c, mut s) = setup();
        s.set_paused(c.now(), true);
        s.folder_event(c.now());
        c.advance(S(3 * 86_400));
        assert_eq!(s.poll(c.now()), None);
        assert_eq!((s.next_wake(), s.next_sync_at()), (None, None));
        s.set_paused(c.now(), false);
        // resuming syncs soon; the folder change seen while paused is not remembered (the full sync finds it)
        assert_eq!(s.poll(c.now()), Some(Job::Sync(Reason::Resumed)));
        s.started(Job::Sync(Reason::Resumed));
        s.ended(c.now(), Job::Sync(Reason::Resumed), Ended::Done);
        assert_eq!(s.poll(c.now()), None);
        assert!(s.next_sync_at().unwrap() > c.now());
    }

    #[test]
    fn nothing_runs_until_the_app_is_set_up() {
        let c = FakeClock::new();
        let mut s = Scheduler::new(c.now(), Timing::default(), false, false, None);
        s.request_sync(Reason::Manual);
        s.folder_event(c.now());
        c.advance(S(86_400));
        assert_eq!((s.poll(c.now()), s.next_wake(), s.next_sync_at()), (None, None, None));
        s.set_enabled(c.now(), true);
        assert_eq!(s.poll(c.now()), Some(Job::Sync(Reason::Manual)));
        s.started(Job::Sync(Reason::Manual));
        s.ended(c.now(), Job::Sync(Reason::Manual), Ended::Done);
        assert!(!matches!(s.poll(c.now()), Some(Job::Sync(_)))); // (one sync, not two)
    }

    #[test]
    fn a_busy_lock_is_retried_after_a_minute_not_after_15() {
        let (c, mut s) = setup();
        c.advance(S(20));
        let job = s.poll(c.now()).unwrap();
        s.started(job);
        s.ended(c.now(), job, Ended::Busy);
        c.advance(S(59));
        assert!(!matches!(s.poll(c.now()), Some(Job::Sync(_))));
        c.advance(S(1));
        assert_eq!(s.poll(c.now()), Some(Job::Sync(Reason::Timer)).or(Some(Job::InboxCheck)));
    }

    #[test]
    fn the_purge_runs_about_weekly() {
        let c = FakeClock::new();
        let mut s = Scheduler::new(c.now(), Timing::default(), true, false, Some(c.now()));
        let mut purges = Vec::new();
        // a month in 5-minute steps, running whatever is due
        for step in 0..(30 * 24 * 12) {
            c.advance(S(300));
            while let Some(j) = s.poll(c.now()) {
                s.started(j);
                if j == Job::Purge {
                    purges.push((step + 1) * 300);
                }
                s.ended(c.now(), j, Ended::Done);
            }
        }
        assert_eq!(purges.len(), 4, "{purges:?}"); // days 7, 14, 21, 28
        assert!(purges[0] >= 7 * 86_400 && purges[0] < 7 * 86_400 + 3_600);
        // never purged: the first one comes soon after the start, not a week later
        let c = FakeClock::new();
        let mut s = Scheduler::new(c.now(), Timing::default(), true, false, None);
        c.advance(S(41));
        let mut got = Vec::new();
        while let Some(j) = run(&c, &mut s) {
            got.push(j);
        }
        assert!(got.contains(&Job::Purge), "{got:?}");
    }

    #[test]
    fn next_wake_is_the_earliest_thing_due() {
        let (c, mut s) = setup();
        assert_eq!(s.next_wake(), Some(c.now() + S(20)));
        s.folder_event(c.now());
        assert_eq!(s.next_wake(), Some(c.now() + S(20))); // (the startup sync comes before the 30 s quiet window ends)
        s.request_sync(Reason::Manual);
        assert_eq!(s.next_wake(), Some(SystemTime::UNIX_EPOCH));
    }

    #[test]
    fn upload_jobs_follow_each_other_a_second_apart_while_photos_are_left_and_wait_when_there_is_nothing_to_send() {
        let c = FakeClock::new();
        let t = Timing { startup_delay: S(7200), inbox_every: S(7200), ..Timing::default() };
        let mut s = Scheduler::new(c.now(), t, true, false, Some(c.now()));
        s.want_upload(c.now());
        assert_eq!(s.poll(c.now()), Some(Job::Upload));
        s.started(Job::Upload);
        s.ended(c.now(), Job::Upload, Ended::UploadMore);
        assert_eq!(s.poll(c.now()), None, "a second's pause: the other jobs get their turn");
        c.advance(S(1));
        assert_eq!(s.poll(c.now()), Some(Job::Upload));
        s.ended(c.now(), Job::Upload, Ended::UploadLater);
        c.advance(S(299));
        assert_eq!(s.poll(c.now()), None);
        c.advance(S(1));
        assert_eq!(s.poll(c.now()), Some(Job::Upload), "offline, or awaiting the home PC: five minutes later");
        s.ended(c.now(), Job::Upload, Ended::UploadDone);
        assert_eq!(s.upload_due(), None);
        c.advance(S(1_000));
        assert!(!matches!(s.poll(c.now()), Some(Job::Upload)));
        // another run holding the lock: a minute
        s.want_upload(c.now());
        s.ended(c.now(), Job::Upload, Ended::Busy);
        assert_eq!(s.upload_due(), Some(c.now() + S(60)));
    }

    #[test]
    fn a_sync_for_an_edit_goes_before_the_next_upload_job_and_pause_stops_uploads() {
        let (c, mut s) = setup_late();
        s.want_upload(c.now());
        s.folder_event(c.now());
        c.advance(S(30));
        // both are due: the sync first
        assert_eq!(s.poll(c.now()), Some(Job::Sync(Reason::FolderChanged)));
        s.started(Job::Sync(Reason::FolderChanged));
        s.ended(c.now(), Job::Sync(Reason::FolderChanged), Ended::Done);
        assert_eq!(s.poll(c.now()), Some(Job::Upload));
        // Pause: nothing runs, not even a job that is due; Resume runs it
        s.set_paused(c.now(), true);
        assert_eq!(s.poll(c.now()), None);
        s.set_paused(c.now(), false);
        let mut got = Vec::new();
        while let Some(j) = run(&c, &mut s) {
            got.push(j);
        }
        assert!(got.contains(&Job::Upload), "{got:?}");
        // "Sync now" still works while paused and does not run the upload
        s.want_upload(c.now());
        s.set_paused(c.now(), true);
        s.request_sync(Reason::Manual);
        assert_eq!(s.poll(c.now()), Some(Job::Sync(Reason::Manual)));
        s.started(Job::Sync(Reason::Manual));
        assert_eq!(s.poll(c.now()), None);
    }
}
