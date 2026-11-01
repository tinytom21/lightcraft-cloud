//! The live progress of a running job, from the worker's hook to the published status: published while the job blocks, throttled, never
//! able to fail or hold up the job, never left behind afterwards. A fake worker reports like the sync crate's hook does; the short
//! pauses in it are real (tens of milliseconds) because the pump is a real thread.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lightcraft_cloud_client::{Progress, Stage};
use lightcraft_cloud_desktop::appstate::{Activity, AppStatus};
use lightcraft_cloud_desktop::clock::FakeClock;
use lightcraft_cloud_desktop::live::Feed;
use lightcraft_cloud_desktop::notifier::NoNotifier;
use lightcraft_cloud_desktop::scheduler::Timing;
use lightcraft_cloud_desktop::service::{JobError, PurgeOutcome, Service, SyncOutcome, Worker, WorkerInfo};

/// A worker whose sync reports `steps` downloads, `pause` apart, through the feed it was given.
struct Reporting {
    feed: Option<Arc<Feed>>,
    steps: u64,
    pause: Duration,
    fail: bool,
}

impl Worker for Reporting {
    fn info(&self) -> WorkerInfo {
        WorkerInfo { configured: true, notify: false, purge: false }
    }
    fn reload(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn set_progress(&mut self, feed: Arc<Feed>) {
        self.feed = Some(feed);
    }
    fn sync(&mut self) -> Result<SyncOutcome, JobError> {
        let feed = self.feed.clone().expect("the service gave the worker its feed");
        for i in 0..=self.steps {
            feed.report(&Progress {
                stage: Stage::Downloading,
                done_items: i,
                total_items: self.steps,
                done_bytes: i * 1000,
                total_bytes: self.steps * 1000,
                started_ms: 1_000,
                last_item_ms: 1_000 + i,
                ..Progress::default()
            });
            std::thread::sleep(self.pause);
        }
        if self.fail { Err(JobError::Failed("boom".into())) } else { Ok(SyncOutcome { photos: self.steps as usize, ..SyncOutcome::default() }) }
    }
    fn inbox_waiting(&mut self) -> Result<usize, String> {
        Ok(0)
    }
    fn purge(&mut self) -> Result<PurgeOutcome, JobError> {
        Ok(PurgeOutcome::Done { folders: 0, versions: 0 })
    }
    fn last_run(&self) -> Option<(String, String, usize)> {
        None
    }
}

fn temp(tag: &str) -> std::path::PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("lc-desktop-progress-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

type Seen = Arc<Mutex<Vec<AppStatus>>>;

fn service(state: &std::path::Path, worker: Reporting, sink_delay: Duration) -> (Service<Reporting>, Seen, Arc<FakeClock>) {
    let seen: Seen = Arc::default();
    let s2 = seen.clone();
    let clock = Arc::new(FakeClock::new());
    let mut svc = Service::new(
        worker,
        clock.clone(),
        Box::new(NoNotifier),
        state.to_path_buf(),
        Timing::default(),
        Box::new(move |s| {
            std::thread::sleep(sink_delay);
            s2.lock().unwrap().push(s.clone());
        }),
        Box::new(|_| {}),
    );
    svc.set_progress_interval(Duration::from_millis(20));
    (svc, seen, clock)
}

/// Run the startup sync (due 20 s after the start).
fn run_sync(svc: &mut Service<Reporting>, clock: &FakeClock) {
    clock.advance(Duration::from_secs(20));
    assert!(svc.step());
}

#[test]
fn a_running_job_publishes_its_progress_to_the_sink_and_to_status_json_and_leaves_none_behind() {
    let state = temp("a");
    let (mut svc, seen, clock) = service(&state, Reporting { feed: None, steps: 5, pause: Duration::from_millis(30), fail: false }, Duration::ZERO);
    run_sync(&mut svc, &clock);
    let all = seen.lock().unwrap().clone();
    let with: Vec<&Progress> = all.iter().filter_map(|s| s.activity.progress()).collect();
    assert!(with.len() >= 2, "published while the job ran: {} of {}", with.len(), all.len());
    assert!(with.iter().all(|p| p.stage == Stage::Downloading && p.total_items == 5));
    assert!(with.windows(2).all(|w| w[0].done_items <= w[1].done_items));
    assert!(all.iter().filter(|s| s.activity.progress().is_some()).all(|s| matches!(s.activity, Activity::Syncing { .. })));
    // throttled: one publish per interval at most (the job runs about 180 ms; the interval is 20 ms)
    assert!(with.len() <= 10, "{}", with.len());
    // afterwards: Idle, no progress, in the sink and in the file the windows read
    let last = all.last().unwrap();
    assert_eq!(last.activity, Activity::Idle);
    let on_disk = AppStatus::load(&state).unwrap();
    assert_eq!(on_disk.activity, Activity::Idle);
    assert!(on_disk.last_sync.as_ref().is_some_and(|l| l.ok && l.photos == 5));
    // (a late pump write would show up now)
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(AppStatus::load(&state).unwrap().activity, Activity::Idle);
    assert_eq!(seen.lock().unwrap().last().unwrap().activity, Activity::Idle);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn a_job_shorter_than_the_interval_publishes_no_progress_at_all() {
    let state = temp("b");
    let (mut svc, seen, clock) = service(&state, Reporting { feed: None, steps: 2, pause: Duration::ZERO, fail: false }, Duration::ZERO);
    svc.set_progress_interval(Duration::from_secs(5));
    run_sync(&mut svc, &clock);
    assert!(seen.lock().unwrap().iter().all(|s| s.activity.progress().is_none()), "nothing flickers for a small job");
    assert_eq!(svc.status().activity, Activity::Idle);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn a_status_write_that_fails_cannot_fail_the_sync() {
    let state = temp("c");
    let (mut svc, _seen, clock) = service(&state, Reporting { feed: None, steps: 4, pause: Duration::from_millis(30), fail: false }, Duration::ZERO);
    // the state folder goes away and a plain file takes its place: every write of status.json fails from now on
    std::fs::remove_dir_all(&state).unwrap();
    std::fs::write(&state, b"not a folder").unwrap();
    run_sync(&mut svc, &clock);
    let st = svc.status();
    assert!(st.last_sync.as_ref().is_some_and(|l| l.ok && l.photos == 4), "{:?}", st.last_sync);
    assert!(st.last_error.is_none() && st.errors.is_empty(), "no failure was made up: {:?}", st.errors);
    let _ = std::fs::remove_file(&state);
}

#[test]
fn a_slow_sink_and_a_panicking_sink_do_not_hold_up_or_fail_the_sync() {
    let state = temp("d");
    let (mut svc, seen, clock) = service(&state, Reporting { feed: None, steps: 3, pause: Duration::from_millis(30), fail: false }, Duration::from_millis(150));
    let t0 = std::time::Instant::now();
    run_sync(&mut svc, &clock);
    // the job itself took about 90 ms; the slow sink is only waited for at the very end (its grace is bounded), never once per report
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    assert!(svc.status().last_sync.as_ref().is_some_and(|l| l.ok));
    assert_eq!(seen.lock().unwrap().last().unwrap().activity, Activity::Idle);
    let _ = std::fs::remove_dir_all(&state);

    // a sink that panics while a job runs
    let state = temp("e");
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let clock = Arc::new(FakeClock::new());
    let mut svc = Service::new(
        Reporting { feed: None, steps: 3, pause: Duration::from_millis(30), fail: false },
        clock.clone(),
        Box::new(NoNotifier),
        state.clone(),
        Timing::default(),
        Box::new(move |s| {
            if s.activity.progress().is_some() && c.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("the tray is gone");
            }
        }),
        Box::new(|_| {}),
    );
    svc.set_progress_interval(Duration::from_millis(20));
    clock.advance(Duration::from_secs(20));
    assert!(svc.step());
    assert!(svc.status().last_sync.as_ref().is_some_and(|l| l.ok));
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn a_failed_job_ends_idle_with_its_failure_and_no_progress() {
    let state = temp("f");
    let (mut svc, seen, clock) = service(&state, Reporting { feed: None, steps: 3, pause: Duration::from_millis(30), fail: true }, Duration::ZERO);
    run_sync(&mut svc, &clock);
    let st = svc.status();
    assert_eq!(st.activity, Activity::Idle);
    assert_eq!(st.last_error.as_deref(), Some("boom"));
    assert_eq!(seen.lock().unwrap().last().unwrap().activity, Activity::Idle);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn a_status_left_by_a_run_that_was_cut_off_mid_job_starts_idle_without_its_progress() {
    let state = temp("g");
    let stale = AppStatus {
        configured: true,
        activity: Activity::Syncing {
            since: "2026-11-01T10:00:00Z".into(),
            reason: "timer".into(),
            progress: Some(Progress { stage: Stage::Downloading, done_items: 3, total_items: 9, ..Progress::default() }),
        },
        ..AppStatus::default()
    };
    stale.save(&state).unwrap();
    assert!(AppStatus::load(&state).unwrap().activity.progress().is_some());
    let (svc, _seen, _clock) = service(&state, Reporting { feed: None, steps: 1, pause: Duration::ZERO, fail: false }, Duration::ZERO);
    assert_eq!(svc.status().activity, Activity::Idle);
    assert_eq!(AppStatus::load(&state).unwrap().activity, Activity::Idle, "and the file says so at once");
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn a_status_file_from_before_progress_still_loads() {
    let state = temp("h");
    let old = r#"{"updated":"2026-11-01T09:00:00Z","configured":true,"activity":{"kind":"syncing","since":"2026-11-01T09:00:00Z","reason":"timer"}}"#;
    std::fs::write(state.join("status.json"), old).unwrap();
    let st = AppStatus::load(&state).expect("an old file loads");
    assert_eq!(st.activity, Activity::Syncing { since: "2026-11-01T09:00:00Z".into(), reason: "timer".into(), progress: None });
    let old = r#"{"configured":true,"activity":{"kind":"uploading","since":"2026-11-01T09:00:00Z"}}"#;
    std::fs::write(state.join("status.json"), old).unwrap();
    assert_eq!(AppStatus::load(&state).unwrap().activity, Activity::Uploading { since: "2026-11-01T09:00:00Z".into(), progress: None });
    // and a new file round-trips
    let p = Progress { stage: Stage::Sending, done_items: 3, total_items: 12, ..Progress::default() };
    let new = AppStatus { activity: Activity::Uploading { since: "x".into(), progress: Some(p) }, ..AppStatus::default() };
    new.save(&state).unwrap();
    assert_eq!(AppStatus::load(&state).unwrap(), new);
    let _ = std::fs::remove_dir_all(&state);
}
