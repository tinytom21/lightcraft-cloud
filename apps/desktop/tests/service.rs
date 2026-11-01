//! The background engine with a fake clock and a fake worker: no threads, no sleeping. What it does when, what it tells the
//! user, and that Pause and a failing job behave.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lightcraft_cloud_desktop::appstate::{Activity, AppStatus, Persisted, TrayState};
use lightcraft_cloud_desktop::clock::{Clock, FakeClock};
use lightcraft_cloud_desktop::notifier::{self, Notifier};
use lightcraft_cloud_desktop::scheduler::Timing;
use lightcraft_cloud_desktop::service::{Command, JobError, PurgeOutcome, Service, SyncOutcome, Worker, WorkerInfo};

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
    fn purge(&mut self) -> Result<PurgeOutcome, JobError> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("purge".into());
        next(&mut s.purges, Ok(PurgeOutcome::Done { folders: 1, versions: 2 }))
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
