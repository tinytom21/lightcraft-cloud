//! The background engine with a fake clock and a fake worker: no threads, no sleeping. What it does when, what it tells the
//! user, and that Pause and a failing job behave.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lightcraft_cloud_desktop::appstate::{Activity, AppStatus, Persisted, TrayState};
use lightcraft_cloud_desktop::clock::{Clock, FakeClock};
use lightcraft_cloud_desktop::notifier::{self, Notifier};
use lightcraft_cloud_desktop::scheduler::Timing;
use lightcraft_cloud_desktop::service::{BridgeReport, Command, JobError, PurgeOutcome, Service, SyncOutcome, Worker, WorkerInfo};

#[derive(Default)]
struct Script {
    /// What the next syncs return (the last one repeats).
    syncs: Vec<Result<SyncOutcome, JobError>>,
    inbox: Vec<Result<usize, String>>,
    /// Committed imports from other PCs (not recorded in `calls`: the existing scripts list the inbox checks only).
    imports: Vec<Result<usize, String>>,
    purges: Vec<Result<PurgeOutcome, JobError>>,
    calls: Vec<String>,
    info: Option<WorkerInfo>,
    /// What `reload` makes the info.
    after_reload: Option<WorkerInfo>,
    /// What the next LightCraft tries return (the last repeats); each try's `retry_given_up` goes to `bridge_calls`.
    bridge: Vec<BridgeReport>,
    bridge_calls: Vec<bool>,
    /// The worker is another PC's.
    other: bool,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Script>>);

fn next<T: Clone>(v: &mut Vec<T>, dflt: T) -> T {
    if v.len() > 1 { v.remove(0) } else { v.first().cloned().unwrap_or(dflt) }
}

impl Worker for Fake {
    fn info(&self) -> WorkerInfo {
        self.0.lock().unwrap().info.clone().unwrap_or(WorkerInfo { configured: true, notify: true, purge: false })
    }
    fn reload(&mut self) -> Result<(), String> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("reload".into());
        if let Some(i) = s.after_reload.take() {
            s.info = Some(i);
        }
        Ok(())
    }
    fn sync(&mut self) -> Result<SyncOutcome, JobError> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("sync".into());
        next(&mut s.syncs, Ok(SyncOutcome { photos: 10, source_name: "Photos".into(), ..SyncOutcome::default() }))
    }
    fn inbox_waiting(&mut self) -> Result<usize, String> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("inbox".into());
        next(&mut s.inbox, Ok(0))
    }
    fn imports_waiting(&mut self) -> Result<usize, String> {
        let mut s = self.0.lock().unwrap();
        next(&mut s.imports, Ok(0))
    }
    fn lightcraft_flush(&mut self, retry_given_up: bool) -> BridgeReport {
        let mut s = self.0.lock().unwrap();
        s.bridge_calls.push(retry_given_up);
        next(&mut s.bridge, BridgeReport::default())
    }
    fn purge(&mut self) -> Result<PurgeOutcome, JobError> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("purge".into());
        next(&mut s.purges, Ok(PurgeOutcome::Done { folders: 1, versions: 2 }))
    }
    fn is_other(&self) -> bool {
        self.0.lock().unwrap().other
    }
    fn last_run(&self) -> Option<(String, String, usize)> {
        None
    }
}

impl Fake {
    fn calls(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap().calls)
    }
}

#[derive(Clone, Default)]
struct Spy(Arc<Mutex<Vec<(String, String)>>>);

impl Notifier for Spy {
    fn notify(&self, summary: &str, body: &str) {
        self.0.lock().unwrap().push((summary.to_string(), body.to_string()));
    }
}

impl Spy {
    fn texts(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap()).into_iter().map(|(_, b)| b).collect()
    }
}

struct Rig {
    clock: Arc<FakeClock>,
    fake: Fake,
    spy: Spy,
    state: std::path::PathBuf,
    svc: Service<Fake>,
    seen: Arc<Mutex<Vec<AppStatus>>>,
}

fn temp(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let d =
        std::env::temp_dir().join(format!("lc-desktop-svc-{tag}-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn rig_in(state: std::path::PathBuf, clock: Arc<FakeClock>, fake: Fake) -> Rig {
    let spy = Spy::default();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let svc = Service::new(
        fake.clone(),
        clock.clone(),
        Box::new(spy.clone()),
        state.clone(),
        Timing::default(),
        Box::new(move |s| s2.lock().unwrap().push(s.clone())),
        Box::new(|_| {}),
    );
    Rig { clock, fake, spy, state, svc, seen }
}

fn rig(tag: &str) -> Rig {
    rig_in(temp(tag), Arc::new(FakeClock::new()), Fake::default())
}

impl Rig {
    fn advance(&mut self, secs: u64) -> Vec<String> {
        self.clock.advance(Duration::from_secs(secs));
        while self.svc.step() {}
        self.fake.calls()
    }
}

fn edits(applied: usize, merged: usize, folders: &[&str]) -> Result<SyncOutcome, JobError> {
    Ok(SyncOutcome {
        photos: 5,
        applied,
        merged,
        folders: folders.iter().map(|s| s.to_string()).collect(),
        source_name: "Photos".into(),
        ..SyncOutcome::default()
    })
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

#[test]
fn the_first_sync_comes_after_the_startup_delay_then_every_15_minutes() {
    let mut r = rig("a");
    assert_eq!(r.advance(19), Vec::<String>::new());
    assert_eq!(r.advance(1), ["sync"]);
    let st = r.svc.status().clone();
    assert!(st.configured && st.last_sync.as_ref().is_some_and(|l| l.ok && l.photos == 10 && l.result == "ok"));
    assert!(st.next_sync.is_some());
    // 14 minutes later: only inbox checks (every 2 minutes); at 15 a sync
    let mut calls = Vec::new();
    for _ in 0..14 {
        calls.extend(r.advance(60));
    }
    assert!(calls.iter().all(|c| c == "inbox"), "{calls:?}");
    assert_eq!(calls.len(), 7);
    assert_eq!(r.advance(60), ["sync"]);
}

#[test]
fn waiting_phone_edits_start_a_sync_and_nothing_waiting_does_not() {
    let mut r = rig("b");
    r.advance(20); // startup sync
    assert_eq!(r.advance(120), ["inbox"]);
    assert_eq!(r.svc.status().inbox_pending, Some(0));
    r.fake.0.lock().unwrap().inbox = vec![Ok(3)];
    assert_eq!(r.advance(120), ["inbox", "sync"]); // the sync follows at once
    assert_eq!(r.svc.status().inbox_pending, Some(0)); // (the sync emptied the inbox)
    assert_eq!(r.svc.status().last_sync.as_ref().unwrap().photos, 10);
}

#[test]
fn a_failing_inbox_check_is_swallowed() {
    let mut r = rig("c");
    r.advance(20);
    r.fake.0.lock().unwrap().inbox = vec![Err("no network".into())];
    assert_eq!(r.advance(120), ["inbox"]);
    assert!(r.svc.status().errors.is_empty() && r.svc.status().last_error.is_none());
    assert!(r.spy.texts().is_empty());
    assert_eq!(r.advance(120), ["inbox"]); // and it tries again
}

#[test]
fn folder_changes_start_a_sync_after_30_quiet_seconds() {
    let mut r = rig("d");
    r.advance(20);
    r.svc.handle(Command::FolderChanged);
    assert_eq!(r.advance(29), Vec::<String>::new());
    r.svc.handle(Command::FolderChanged);
    assert_eq!(r.advance(29), Vec::<String>::new());
    assert_eq!(r.advance(1), ["sync"]);
}

#[test]
fn sync_now_runs_even_while_paused_and_pause_stops_the_rest_and_survives_a_restart() {
    let mut r = rig("e");
    r.svc.handle(Command::Pause);
    assert!(r.svc.status().paused && r.svc.status().next_sync.is_none());
    assert_eq!(r.svc.status().tray_state(), TrayState::Paused);
    r.svc.handle(Command::FolderChanged);
    assert_eq!(r.advance(3 * 86_400), Vec::<String>::new());
    r.svc.handle(Command::SyncNow);
    assert_eq!(r.advance(0), ["sync"]);
    assert_eq!(r.advance(3600), Vec::<String>::new());
    // restart: still paused
    let (state, clock, fake) = (r.state.clone(), r.clock.clone(), r.fake.clone());
    assert!(Persisted::load(&state).paused);
    let mut r2 = rig_in(temp("e2"), clock.clone(), fake);
    std::fs::remove_dir_all(&r2.state).unwrap();
    r2.state = state.clone();
    let mut again = rig_in(state, clock, Fake::default());
    assert!(again.svc.status().paused);
    assert_eq!(again.advance(86_400), Vec::<String>::new());
    again.svc.handle(Command::Resume);
    assert_eq!(again.advance(0), ["sync"]);
    assert!(!Persisted::load(&again.state).paused);
    std::mem::forget(r2); // (its folder is gone already)
}

#[test]
fn applied_edits_are_announced_with_the_pythons_text_unless_notifications_are_off() {
    let mut r = rig("f");
    r.fake.0.lock().unwrap().syncs = vec![edits(3, 2, &["2024/Trip", ""])];
    r.advance(20);
    assert_eq!(
        r.spy.texts(),
        ["5 phone edits applied (2 merged with PC changes) in 2024/Trip, Photos \u{2014} in LightCraft run Synchronize Folder on these folders"]
    );
    assert_eq!(r.svc.status().edits_applied, 5);
    assert_eq!(r.svc.status().last_sync.as_ref().map(|l| (l.applied, l.merged)), Some((3, 2)));

    let fake = Fake::default();
    fake.0.lock().unwrap().info = Some(WorkerInfo { configured: true, notify: false, purge: false });
    fake.0.lock().unwrap().syncs = vec![edits(1, 0, &[])];
    let mut quiet = rig_in(temp("f2"), Arc::new(FakeClock::new()), fake);
    quiet.advance(20);
    assert!(quiet.spy.texts().is_empty());
    assert_eq!(quiet.svc.status().edits_applied, 1); // (still counted in the Status window)
}

#[test]
fn newest_wins_decisions_are_kept_for_the_status_window_and_phone_edits_that_lost_are_announced() {
    let mut r = rig("g");
    r.fake.0.lock().unwrap().syncs = vec![Ok(SyncOutcome {
        kept: 2,
        notices: vec!["Newest wins: the PC's newer sidecar was kept (a)".into(), "Newest wins: x".into()],
        ..SyncOutcome::default()
    })];
    r.advance(20);
    assert_eq!(
        r.svc.status().notices.iter().map(|n| n.text.as_str()).collect::<Vec<_>>(),
        ["Newest wins: x", "Newest wins: the PC's newer sidecar was kept (a)"]
    );
    assert_eq!(r.spy.texts(), [notifier::edits_kept(2)]);
}

#[test]
fn a_failed_sync_shows_in_the_status_and_is_announced_once_per_distinct_error() {
    let mut r = rig("h");
    r.fake.0.lock().unwrap().syncs = vec![Err(JobError::Failed("remote: quota exceeded".into()))];
    r.advance(20);
    let st = r.svc.status().clone();
    assert_eq!(st.last_error.as_deref(), Some("remote: quota exceeded"));
    assert_eq!(st.tray_state(), TrayState::Error);
    assert_eq!(st.last_sync.as_ref().map(|l| l.ok), Some(false));
    assert_eq!(r.spy.texts(), [notifier::publisher_failed()]);
    // the same error 15 minutes later: in the list again, but no second popup
    r.advance(15 * 60);
    assert_eq!(r.svc.status().errors.len(), 2);
    assert!(r.spy.texts().is_empty());
    // a different error is announced
    r.fake.0.lock().unwrap().syncs = vec![Err(JobError::Refused("source /x is empty (drive not mounted?)".into()))];
    r.advance(15 * 60);
    assert_eq!(r.spy.texts().len(), 1);
    assert!(r.svc.status().last_error.as_deref().unwrap().starts_with("refused: source /x is empty"));
    // a sync that works clears the error, and the same error later is announced again
    r.fake.0.lock().unwrap().syncs = vec![Ok(SyncOutcome::default())];
    r.advance(15 * 60);
    assert!(r.svc.status().last_error.is_none() && r.svc.status().tray_state() == TrayState::Idle);
    r.fake.0.lock().unwrap().syncs = vec![Err(JobError::Refused("source /x is empty (drive not mounted?)".into()))];
    r.advance(15 * 60);
    assert_eq!(r.spy.texts().len(), 1);
}

#[test]
fn a_held_run_lock_is_retried_after_a_minute_without_noise() {
    let mut r = rig("i");
    r.fake.0.lock().unwrap().syncs = vec![Err(JobError::Busy), Ok(SyncOutcome::default())];
    assert_eq!(r.advance(20), ["sync"]);
    assert!(r.svc.status().errors.is_empty() && r.svc.status().last_error.is_none() && r.spy.texts().is_empty());
    assert!(r.advance(59).iter().all(|c| c != "sync"));
    assert!(r.advance(1).contains(&"sync".to_string()));
}

#[test]
fn status_shows_syncing_while_a_job_runs() {
    let mut r = rig("j");
    r.advance(20);
    let seen = r.seen.lock().unwrap();
    assert!(
        seen.iter().any(|s| matches!(s.activity, Activity::Syncing { .. }) && s.tray_state() == TrayState::Syncing),
        "the sink never saw the syncing state"
    );
    assert!(matches!(seen.last().unwrap().activity, Activity::Idle));
    // and what the windows read is on disk
    drop(seen);
    assert_eq!(AppStatus::load(&r.state).unwrap().last_sync.unwrap().photos, 10);
}

#[test]
fn the_purge_runs_weekly_and_a_refusal_is_a_note_not_an_error() {
    let fake = Fake::default();
    fake.0.lock().unwrap().info = Some(WorkerInfo { configured: true, notify: true, purge: true });
    let mut r = rig_in(temp("k"), Arc::new(FakeClock::new()), fake);
    let mut purges = 0;
    r.fake.0.lock().unwrap().purges = vec![Ok(PurgeOutcome::Done { folders: 0, versions: 0 })];
    for _ in 0..(10 * 24) {
        purges += r.advance(3600).iter().filter(|c| *c == "purge").count();
    }
    assert_eq!(purges, 2, "first soon after the start, the next a week later");
    assert!(r.svc.status().last_purge.is_some());
    r.fake.0.lock().unwrap().purges = vec![Err(JobError::Refused("newest backup is 100 h old (> 48 h); refusing to purge history".into()))];
    for _ in 0..(8 * 24) {
        r.advance(3600);
    }
    let st = r.svc.status();
    assert!(st.notices[0].text.contains("History purge skipped") && st.notices[0].text.contains("refusing to purge history"));
    assert!(st.last_error.is_none() && st.errors.is_empty() && r.spy.texts().is_empty());
}

#[test]
fn the_purge_can_be_switched_off() {
    let fake = Fake::default();
    fake.0.lock().unwrap().info = Some(WorkerInfo { configured: true, notify: true, purge: false });
    let mut r = rig_in(temp("l"), Arc::new(FakeClock::new()), fake);
    let mut calls = Vec::new();
    for _ in 0..(8 * 24) {
        calls.extend(r.advance(3600));
    }
    assert!(!calls.iter().any(|c| c == "purge"));
}

#[test]
fn without_a_config_nothing_runs_until_settings_are_saved() {
    let fake = Fake::default();
    fake.0.lock().unwrap().info = Some(WorkerInfo { configured: false, notify: true, purge: false });
    fake.0.lock().unwrap().after_reload = Some(WorkerInfo { configured: true, notify: true, purge: false });
    let mut r = rig_in(temp("m"), Arc::new(FakeClock::new()), fake);
    assert!(!r.svc.status().configured);
    assert_eq!(r.advance(86_400), Vec::<String>::new());
    r.svc.handle(Command::SyncNow);
    assert_eq!(r.advance(10), Vec::<String>::new());
    r.svc.handle(Command::ConfigChanged);
    assert!(r.svc.status().configured);
    assert_eq!(r.advance(0), ["reload", "sync"]);
}

#[test]
fn quit_stops_the_loop() {
    use std::sync::mpsc::channel;
    let mut r = rig("n");
    let (tx, rx) = channel();
    tx.send(Command::Quit).unwrap();
    // (a real loop on a fake clock: Quit is the first thing it reads, so it returns without waiting)
    lightcraft_cloud_desktop::service::run_loop(&mut r.svc, &rx, &*r.clock);
    assert!(r.svc.quit_requested());
    let _ = r.clock.now();
}

// ------------------------------------------------------------------ photos other PCs uploaded

fn imported(devices: &[(&str, usize)], left: usize) -> Result<SyncOutcome, JobError> {
    Ok(SyncOutcome {
        photos: 5,
        imported: devices.iter().map(|d| d.1).sum(),
        import_devices: devices.iter().map(|(d, n)| (d.to_string(), *n)).collect(),
        imports_left: left,
        source_name: "Photos".into(),
        ..SyncOutcome::default()
    })
}

#[test]
fn waiting_imports_start_a_sync_and_are_counted_apart_from_phone_edits() {
    let mut r = rig("imp1");
    r.advance(20);
    r.fake.0.lock().unwrap().imports = vec![Ok(3)];
    assert_eq!(r.advance(120), ["inbox", "sync"]);
    let st = r.svc.status();
    assert_eq!((st.inbox_pending, st.imports_pending), (Some(0), Some(0)), "the sync took them");
    assert_eq!(st.edits_applied, 0, "photos from other PCs are not phone edits");
}

#[test]
fn an_import_that_stays_stuck_does_not_start_a_sync_every_two_minutes() {
    let mut r = rig("imp2");
    r.fake.0.lock().unwrap().syncs = vec![imported(&[], 1)]; // (every sync leaves one import waiting, e.g. the disk is full)
    r.advance(20);
    r.fake.0.lock().unwrap().imports = vec![Ok(1)];
    assert_eq!(r.advance(120), ["inbox"], "the same single import is still there: the 15 minute sync retries it");
    assert_eq!(r.svc.status().imports_pending, Some(1));
    assert_eq!(r.advance(120), ["inbox"]);
    r.fake.0.lock().unwrap().imports = vec![Ok(2)];
    assert_eq!(r.advance(120), ["inbox", "sync"], "a new import does start one");
}

#[test]
fn a_failing_imports_check_is_swallowed_and_does_not_hide_phone_edits() {
    let mut r = rig("imp3");
    r.advance(20);
    r.fake.0.lock().unwrap().imports = vec![Err("no network".into())];
    assert_eq!(r.advance(120), ["inbox"]);
    assert!(r.svc.status().errors.is_empty() && r.svc.status().last_error.is_none());
    r.fake.0.lock().unwrap().inbox = vec![Ok(1)];
    assert_eq!(r.advance(120), ["inbox", "sync"]);
}

#[test]
fn photos_added_from_other_pcs_are_announced_once_per_sync_and_counted() {
    let mut r = rig("imp4");
    r.fake.0.lock().unwrap().syncs = vec![imported(&[("laptop", 3)], 0), imported(&[("laptop", 1), ("tablet", 2)], 0), imported(&[], 0)];
    r.advance(20);
    assert_eq!(r.spy.texts(), ["3 photos added from laptop"]);
    assert_eq!(r.svc.status().photos_imported, 3);
    r.svc.handle(Command::SyncNow);
    r.advance(0);
    assert_eq!(r.spy.texts(), ["3 photos added from laptop and tablet"]);
    assert_eq!((r.svc.status().photos_imported, r.svc.status().last_imported), (6, 3));
    r.svc.handle(Command::SyncNow);
    r.advance(0);
    assert!(r.spy.texts().is_empty(), "nothing added, nothing said");
    assert_eq!(r.svc.status().last_imported, 0);
    // one photo
    r.fake.0.lock().unwrap().syncs = vec![imported(&[("laptop", 1)], 0)];
    r.svc.handle(Command::SyncNow);
    r.advance(0);
    assert_eq!(r.spy.texts(), ["1 photo added from laptop"]);
    // notifications off: still counted
    let fake = Fake::default();
    fake.0.lock().unwrap().info = Some(WorkerInfo { configured: true, notify: false, purge: false });
    fake.0.lock().unwrap().syncs = vec![imported(&[("laptop", 2)], 0)];
    let mut quiet = rig_in(temp("imp4b"), Arc::new(FakeClock::new()), fake);
    quiet.advance(20);
    assert!(quiet.spy.texts().is_empty());
    assert_eq!(quiet.svc.status().photos_imported, 2);
}

#[test]
fn import_problems_reach_the_status_window_once() {
    let mut r = rig("imp5");
    let out = || Ok(SyncOutcome { import_problems: vec!["laptop/ab: not enough free disk space".into()], imports_left: 1, ..SyncOutcome::default() });
    r.fake.0.lock().unwrap().syncs = vec![out()];
    r.advance(20);
    r.svc.handle(Command::SyncNow);
    r.advance(0);
    let errs: Vec<_> = r.svc.status().errors.iter().map(|e| e.text.clone()).collect();
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].contains("not enough free disk space"));
    assert!(r.svc.status().last_error.is_none(), "the sync itself worked");
}

// ------------------------------------------------------------------ telling LightCraft

fn bridge(waiting: usize, reachable: Option<bool>, problems: &[&str]) -> BridgeReport {
    BridgeReport { port: 7980, waiting, reachable, problems: problems.iter().map(|p| p.to_string()).collect(), ..BridgeReport::default() }
}

#[test]
fn lightcraft_is_tried_on_every_two_minute_check_and_right_after_a_sync_that_added_photos() {
    let mut r = rig("lc1");
    r.fake.0.lock().unwrap().bridge = vec![bridge(2, Some(false), &[])];
    r.fake.0.lock().unwrap().syncs = vec![imported(&[("laptop", 2)], 0), imported(&[], 0)];
    r.advance(20); // the startup sync added two photos
    assert_eq!(r.fake.0.lock().unwrap().bridge_calls, [true], "tried at once, and it is the first try since the start");
    let st = r.svc.status();
    assert_eq!((st.lightcraft_port, st.lightcraft_waiting, st.lightcraft_reachable), (7980, 2, Some(false)));
    r.advance(120);
    assert_eq!(r.fake.0.lock().unwrap().bridge_calls, [true, false], "the 2 minute check; the given-up photos are retried only the first time");
    r.fake.0.lock().unwrap().bridge = vec![bridge(0, Some(true), &[])];
    r.advance(120);
    let st = r.svc.status();
    assert_eq!((st.lightcraft_waiting, st.lightcraft_reachable), (0, Some(true)));
    // a sync that added nothing does not try again by itself
    let n = r.fake.0.lock().unwrap().bridge_calls.len();
    r.svc.handle(Command::SyncNow);
    r.advance(0);
    assert_eq!(r.fake.0.lock().unwrap().bridge_calls.len(), n);
}

#[test]
fn a_problem_from_lightcraft_is_shown_once_and_a_silent_lightcraft_never_stops_syncing() {
    let mut r = rig("lc2");
    r.advance(20);
    r.fake.0.lock().unwrap().bridge = vec![bridge(1, Some(false), &[]), bridge(0, None, &["LightCraft would not add a.jpg after 10 tries (denied)."]), bridge(0, None, &[])];
    for _ in 0..4 {
        r.advance(120);
    }
    let st = r.svc.status();
    assert_eq!(st.errors.iter().filter(|e| e.text.contains("would not add a.jpg")).count(), 1);
    assert_eq!(st.lightcraft_reachable, Some(false), "not asked is not the same as answering: the last answer stays");
    assert!(st.last_error.is_none(), "the sync itself is fine");
    // the 15 minute sync still happens
    let calls = r.advance(900);
    assert!(calls.contains(&"sync".to_string()), "{calls:?}");
}

fn sidecars(on: Option<bool>, folders: usize) -> BridgeReport {
    BridgeReport { port: 7980, reachable: Some(true), sidecars: on, refresh_waiting: folders, ..BridgeReport::default() }
}

#[test]
fn the_sidecar_warning_appears_when_lightcraft_says_it_is_off_and_goes_as_soon_as_it_is_on_or_lightcraft_is_gone() {
    use lightcraft_cloud_desktop::appstate::{HealthKind, SIDECARS_OFF_SHORT, Tone};
    for other in [false, true] {
        let mut r = rig(if other { "sc-other" } else { "sc-home" });
        r.fake.0.lock().unwrap().other = other;
        r.advance(20);
        assert!(!r.svc.status().lightcraft_sidecars_off);
        r.fake.0.lock().unwrap().bridge = vec![sidecars(Some(false), 2), sidecars(Some(false), 2), sidecars(Some(true), 0)];
        r.advance(120);
        let st = r.svc.status();
        assert!(st.lightcraft_sidecars_off() && st.lightcraft_refresh_waiting == 2, "{st:?}");
        let h = st.health();
        assert_eq!((h.kind, h.tone, h.text.as_str()), (HealthKind::Waiting, Tone::Wait, SIDECARS_OFF_SHORT), "calm amber, not a failure");
        assert!(st.last_error.is_none() && st.errors.is_empty(), "nothing is listed as a problem: {st:?}");
        r.advance(120);
        assert!(r.svc.status().lightcraft_sidecars_off(), "it stays while LightCraft keeps saying so");
        r.advance(120);
        let st = r.svc.status();
        assert!(!st.lightcraft_sidecars_off());
        assert_eq!(st.health().kind, HealthKind::UpToDate, "gone as soon as it is on");
        // LightCraft closes (no answer): nothing to say, so nothing is shown
        r.fake.0.lock().unwrap().bridge = vec![sidecars(Some(false), 0), BridgeReport { port: 7980, reachable: Some(false), ..BridgeReport::default() }];
        r.advance(120);
        assert!(r.svc.status().lightcraft_sidecars_off());
        r.advance(120);
        assert!(!r.svc.status().lightcraft_sidecars_off(), "silent when LightCraft is closed");
        // the feature off
        r.fake.0.lock().unwrap().bridge = vec![sidecars(Some(false), 0), BridgeReport::default()];
        r.advance(120);
        r.advance(120);
        assert!(!r.svc.status().lightcraft_sidecars_off());
    }
}

#[test]
fn a_sync_that_wrote_sidecars_asks_lightcraft_at_once_and_one_that_wrote_none_does_not() {
    let mut r = rig("sc-sync");
    r.fake.0.lock().unwrap().syncs = vec![edits(0, 0, &[]), edits(2, 0, &["A"]), Ok(SyncOutcome { sidecars: 1, ..SyncOutcome::default() })];
    r.advance(20); // the startup sync: nothing written
    assert_eq!(r.fake.0.lock().unwrap().bridge_calls.len(), 0);
    r.svc.handle(Command::SyncNow);
    r.advance(0);
    assert_eq!(r.fake.0.lock().unwrap().bridge_calls.len(), 1, "two phone edits applied");
    r.svc.handle(Command::SyncNow);
    r.advance(0);
    assert_eq!(r.fake.0.lock().unwrap().bridge_calls.len(), 2, "the home PC's sidecar taken on another PC");
}

#[test]
fn with_the_feature_off_nothing_shows() {
    let mut r = rig("lc3");
    r.advance(20);
    r.advance(120);
    let st = r.svc.status();
    assert_eq!((st.lightcraft_port, st.lightcraft_waiting, st.lightcraft_reachable), (0, 0, None));
}

// ------------------------------------------------------------------ another PC: calm when the cloud library cannot be reached

#[test]
fn being_offline_is_one_calm_line_in_problems_however_often_it_happens() {
    let mut r = rig("offline");
    r.fake.0.lock().unwrap().syncs = vec![Err(JobError::Offline("network is unreachable (os error 101)".into()))];
    r.advance(20);
    for _ in 0..6 {
        r.advance(15 * 60);
    }
    let s = r.svc.status();
    assert_eq!(s.errors.len(), 1, "{:?}", s.errors);
    assert_eq!(s.errors[0].text, lightcraft_cloud_desktop::service::OFFLINE_LINE);
    assert!(s.offline && s.last_error.is_none(), "not a failure: {s:?}");
    assert_ne!(s.tray_state(), TrayState::Error);
    assert!(r.spy.texts().is_empty(), "no notification");
    // back online: the flag goes, the line stays as history
    r.fake.0.lock().unwrap().syncs = vec![Ok(SyncOutcome::default())];
    r.advance(15 * 60);
    assert!(!r.svc.status().offline);
    // offline again later: a new line (the newest one is no longer the offline line only if something else came between)
    r.fake.0.lock().unwrap().syncs = vec![Err(JobError::Offline("again".into()))];
    r.advance(15 * 60);
    assert_eq!(r.svc.status().errors.len(), 1, "the line is still the newest, so it is not added twice");
}

#[test]
fn paused_and_offline_does_nothing_at_all() {
    let mut r = rig("paused-offline");
    r.fake.0.lock().unwrap().syncs = vec![Err(JobError::Offline("x".into()))];
    r.svc.handle(Command::Pause);
    assert_eq!(r.advance(86_400), Vec::<String>::new());
    assert!(r.svc.status().errors.is_empty());
}

#[test]
fn the_same_refusal_is_said_once() {
    let mut r = rig("refused-once");
    r.fake.0.lock().unwrap().syncs = vec![Err(JobError::Refused("use the full build".into()))];
    r.advance(20);
    for _ in 0..4 {
        r.advance(15 * 60);
    }
    let s = r.svc.status();
    assert_eq!(s.errors.len(), 1, "{:?}", s.errors);
    assert!(s.errors[0].text.contains("use the full build"));
}

#[test]
fn another_pcs_numbers_reach_the_status_and_new_photos_are_announced_once() {
    use lightcraft_cloud_desktop::appstate::OtherView;
    let mut r = rig("other-numbers");
    r.fake.0.lock().unwrap().other = true;
    let out = |downloaded, waiting| {
        Ok(SyncOutcome {
            photos: 12,
            downloaded,
            other: Some(OtherView { photos: 12, waiting_to_download: waiting, edits_waiting: 1, held_back: 0, received: 0, ..OtherView::default() }),
            problems: vec!["Not enough free disk space for 3 more photos".into()],
            ..SyncOutcome::default()
        })
    };
    r.fake.0.lock().unwrap().syncs = vec![out(4, 3), out(2, 1), out(0, 0)];
    r.advance(20);
    assert_eq!(r.spy.texts(), ["4 new photos from the home PC"]);
    let o = r.svc.status().other.clone().unwrap();
    assert_eq!((o.photos, o.waiting_to_download, o.edits_waiting, o.received), (12, 3, 1, 4));
    r.advance(15 * 60);
    assert_eq!(r.spy.texts(), ["2 new photos from the home PC"]);
    r.advance(15 * 60);
    assert!(r.spy.texts().is_empty());
    assert_eq!(r.svc.status().other.as_ref().map(|o| (o.received, o.waiting_to_download)), Some((6, 0)));
    assert_eq!(r.svc.status().errors.len(), 1, "the same problem is listed once");
}
