//! Another PC in the background engine, on real sync code: `RoleWorker` + `OtherWorker` + `Other` on a hand-made cloud library (a folder),
//! a fake clock and a fake LightCraft. Both builds run this: nothing here needs the engine. Nothing touches the user's config, state,
//! remotes or desktop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lightcraft_cloud_client::home::content_hash_bytes;
use lightcraft_cloud_client::{DirRemote, Error, Remote, RemoteEntry};
use lightcraft_cloud_desktop::clock::FakeClock;
use lightcraft_cloud_desktop::config::Config;
use lightcraft_cloud_desktop::lightcraft::{ImportReply, LightCraftLink, LinkError};
use lightcraft_cloud_desktop::notifier::Notifier;
use lightcraft_cloud_desktop::scheduler::Timing;
use lightcraft_cloud_desktop::service::{Command, OFFLINE_LINE, Service};
use lightcraft_cloud_desktop::worker::{OtherWorker, RoleWorker};
use serde_json::json;
use sha2::{Digest, Sha256};

const MTIME: &str = "2024-05-01T12:00:00Z";

fn photo(tag: &str) -> Vec<u8> {
    format!("photo bytes of {tag} ").repeat(30).into_bytes()
}

fn sha16(b: &[u8]) -> String {
    Sha256::digest(b).iter().take(8).map(|x| format!("{x:02x}")).collect()
}

#[derive(Clone, Default)]
struct Spy(Arc<Mutex<Vec<String>>>);

impl Notifier for Spy {
    fn notify(&self, _s: &str, body: &str) {
        self.0.lock().unwrap().push(body.to_string());
    }
}

/// Stands in for LightCraft: takes every photo it is sent.
#[derive(Clone, Default)]
struct FakeLightCraft(Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<String>>>);

impl LightCraftLink for FakeLightCraft {
    fn identify(&mut self) -> Result<(), LinkError> {
        Ok(())
    }
    fn import_add(&mut self, paths: &[String]) -> Result<ImportReply, LinkError> {
        self.0.lock().unwrap().extend(paths.iter().cloned());
        Ok(ImportReply { imported: paths.len(), ..ImportReply::default() })
    }
    fn xmp_auto_write(&mut self) -> Result<bool, LinkError> {
        Ok(true)
    }
    fn sync_folder(&mut self, folder: &str) -> Result<lightcraft_cloud_desktop::lightcraft::SyncReply, LinkError> {
        self.1.lock().unwrap().push(folder.to_string());
        Ok(lightcraft_cloud_desktop::lightcraft::SyncReply { read: 1, ..Default::default() })
    }
}

/// A remote that can lose its network.
struct Switch {
    inner: DirRemote,
    offline: Arc<AtomicBool>,
    /// Every path put, in order.
    puts: Arc<Mutex<Vec<String>>>,
}

impl Switch {
    fn up(&self) -> Result<(), Error> {
        if self.offline.load(Ordering::SeqCst) { Err(Error::Remote("dial tcp: network is unreachable".into())) } else { Ok(()) }
    }
}

impl Remote for Switch {
    fn list(&self, dir: &str, recursive: bool) -> Result<Vec<RemoteEntry>, Error> {
        self.up()?;
        self.inner.list(dir, recursive)
    }
    fn get(&self, path: &str, local: &Path) -> Result<(), Error> {
        self.up()?;
        self.inner.get(path, local)
    }
    fn put(&self, local: &Path, path: &str) -> Result<(), Error> {
        self.up()?;
        self.puts.lock().unwrap().push(path.to_string());
        self.inner.put(local, path)
    }
}

struct Env {
    root: PathBuf,
    cloud: PathBuf,
    photos: PathBuf,
    state: PathBuf,
    config: PathBuf,
    clock: Arc<FakeClock>,
    offline: Arc<AtomicBool>,
    lightcraft: FakeLightCraft,
    puts: Arc<Mutex<Vec<String>>>,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn put(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

/// A cloud library with Lisbon/A.jpg (+ sidecar) and Lisbon/B.jpg, and Porto/C.jpg; the PC keeps Lisbon.
fn env(tag: &str, role: &str) -> Env {
    let root = std::env::temp_dir().join(format!("lc-desktop-other-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (cloud, photos, state, config) = (root.join("cloud"), root.join("pc/photos"), root.join("pc/state"), root.join("cfg/config.toml"));
    let sidecar = b"<x:xmpmeta>version one</x:xmpmeta>".to_vec();
    let mut list = Vec::new();
    for (path, tag, sc) in [("Lisbon/A.jpg", "A", Some("Lisbon/A.xmp")), ("Lisbon/B.jpg", "B", None), ("Porto/C.jpg", "C", None)] {
        let bytes = photo(tag);
        put(&cloud, &format!("originals/{path}"), &bytes);
        if let Some(sc) = sc {
            put(&cloud, &format!("originals/{sc}"), &sidecar);
        }
        list.push(json!({"id": content_hash_bytes(&bytes), "path": path, "size": bytes.len(), "mtime": MTIME,
            "sidecar": sc, "sidecarVersion": sc.map(|_| sha16(&sidecar))}));
    }
    put(&cloud, "mobile/manifest.json", json!({"format": 1, "generated": "2026-11-01T09:00:00Z", "photos": list}).to_string().as_bytes());
    std::fs::create_dir_all(&photos).unwrap();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        format!(
            "role = \"{role}\"\nsource = '{}'\nremote = '{}'\nstate_dir = '{}'\nfolders = [\"Lisbon\"]\ndevice = \"laptop\"\nnotify = true\nlightcraft_control_port = 7999\n",
            photos.display(),
            cloud.display(),
            state.display()
        ),
    )
    .unwrap();
    Env { root, cloud, photos, state, config, clock: Arc::new(FakeClock::new()), offline: Arc::default(), lightcraft: FakeLightCraft::default(), puts: Arc::default() }
}

fn service(e: &Env, spy: &Spy, with_home: bool) -> Service<RoleWorker> {
    let (offline, puts) = (e.offline.clone(), e.puts.clone());
    let mut other = OtherWorker::new(
        e.config.clone(),
        Box::new(move |cfg: &Config| {
            let dir = lightcraft_cloud_desktop::config::local_remote(cfg.remote_spec()?)?.unwrap();
            Ok(Arc::new(Switch { inner: DirRemote::new(dir), offline: offline.clone(), puts: puts.clone() }) as Arc<dyn Remote>)
        }),
        e.clock.clone(),
    );
    let lc = e.lightcraft.clone();
    other.set_link_factory(Box::new(move |_| Box::new(lc.clone())));
    other.set_retry_delay(Duration::ZERO);
    // (the home worker is not needed here; a test of the full build's choice between the two is in the app's own tests)
    let _ = with_home;
    let worker = RoleWorker::new(e.config.clone(), None, Box::new(other));
    Service::new(worker, e.clock.clone(), Box::new(spy.clone()), e.state.clone(), Timing::default(), Box::new(|_| {}), Box::new(|_| {}))
}

fn run_due(svc: &mut Service<RoleWorker>, e: &Env, secs: u64) {
    e.clock.advance(Duration::from_secs(secs));
    while svc.step() {}
}

#[test]
fn the_first_pass_downloads_the_chosen_folder_tells_lightcraft_and_the_status_says_so() {
    let e = env("a", "other");
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    assert!(svc.status().other.is_some(), "the rows of another PC show from the start");
    run_due(&mut svc, &e, 20);
    assert!(e.photos.join("Lisbon/A.jpg").exists() && e.photos.join("Lisbon/A.xmp").exists() && e.photos.join("Lisbon/B.jpg").exists());
    assert!(!e.photos.join("Porto").exists());
    let s = svc.status();
    let o = s.other.clone().unwrap();
    assert_eq!((o.photos, o.waiting_to_download, o.edits_waiting, o.received), (2, 0, 0, 2), "{s:?}");
    assert!(s.last_sync.as_ref().is_some_and(|l| l.ok && l.photos == 2));
    assert_eq!(spy.0.lock().unwrap().clone(), ["2 new photos from the home PC"]);
    // LightCraft was told about both photos, by absolute path, and nothing waits any more
    // (compared as the system writes paths: on Windows `pc/photos` in the test's root is `pc\photos` in what the app reports)
    let norm = |p: String| p.replace('/', std::path::MAIN_SEPARATOR_STR);
    let mut told: Vec<String> = e.lightcraft.0.lock().unwrap().iter().cloned().map(norm).collect();
    told.sort();
    assert_eq!(told, [norm(e.photos.join("Lisbon").join("A.jpg").display().to_string()), norm(e.photos.join("Lisbon").join("B.jpg").display().to_string())]);
    assert_eq!(s.lightcraft_waiting, 0);
    // the next pass has nothing to do, so says nothing
    run_due(&mut svc, &e, 15 * 60);
    assert_eq!(spy.0.lock().unwrap().len(), 1);
}

#[test]
fn the_home_pcs_new_sidecar_is_taken_and_its_folder_is_read_in_lightcraft_once() {
    let e = env("refresh", "other");
    let mut svc = service(&e, &Spy::default(), false);
    run_due(&mut svc, &e, 20);
    run_due(&mut svc, &e, 15 * 60);
    assert!(e.lightcraft.1.lock().unwrap().is_empty(), "downloading a photo with its sidecar needs no refresh: the import reads the sidecar");
    // the home PC applies a phone edit: a new sidecar and a new manifest
    let new = b"<x:xmpmeta>version two, from the phone</x:xmpmeta>".to_vec();
    put(&e.cloud, "originals/Lisbon/A.xmp", &new);
    let mpath = e.cloud.join("mobile/manifest.json");
    let mut m: serde_json::Value = serde_json::from_slice(&std::fs::read(&mpath).unwrap()).unwrap();
    m["photos"][0]["sidecarVersion"] = json!(sha16(&new));
    m["generated"] = json!("2026-11-01T10:00:00Z");
    put(&e.cloud, "mobile/manifest.json", m.to_string().as_bytes());
    run_due(&mut svc, &e, 15 * 60);
    assert_eq!(std::fs::read(e.photos.join("Lisbon/A.xmp")).unwrap(), new);
    assert_eq!(*e.lightcraft.1.lock().unwrap(), [e.photos.join("Lisbon").to_str().unwrap().to_string()]);
    assert_eq!(svc.status().lightcraft_refresh_waiting, 0);
    run_due(&mut svc, &e, 15 * 60);
    assert_eq!(e.lightcraft.1.lock().unwrap().len(), 1, "nothing changed, so nothing more is read");
}

#[test]
fn a_sidecar_change_starts_a_pass_after_30_quiet_seconds_and_goes_to_the_inbox() {
    let e = env("b", "other");
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    run_due(&mut svc, &e, 20);
    std::fs::write(e.photos.join("Lisbon/A.xmp"), b"<x:xmpmeta>edited on the laptop</x:xmpmeta>").unwrap();
    svc.handle(Command::FolderChanged);
    run_due(&mut svc, &e, 29);
    assert!(!e.cloud.join("inbox").exists(), "still quiet-waiting");
    run_due(&mut svc, &e, 1);
    let sent: Vec<_> = std::fs::read_dir(e.cloud.join("inbox/laptop")).unwrap().flatten().collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(std::fs::read(sent[0].path()).unwrap(), b"<x:xmpmeta>edited on the laptop</x:xmpmeta>");
}

#[test]
fn no_pass_ever_looks_at_the_inbox_of_the_cloud_library() {
    let e = env("c", "other");
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    run_due(&mut svc, &e, 20);
    // an edit from a phone sits in the inbox: this PC is not interested (and the two-minute check does not even go out)
    put(&e.cloud, &format!("inbox/phone/{}-20261101T000000Z-none.xmp", "a".repeat(32)), b"x");
    e.offline.store(true, Ordering::SeqCst);
    run_due(&mut svc, &e, 120);
    assert!(svc.status().errors.is_empty(), "the two-minute check made no network call, so it could not fail");
}

#[test]
fn offline_is_one_calm_line_and_the_next_pass_after_the_network_is_back_catches_up() {
    let e = env("d", "other");
    let spy = Spy::default();
    e.offline.store(true, Ordering::SeqCst);
    let mut svc = service(&e, &spy, false);
    run_due(&mut svc, &e, 20);
    for _ in 0..8 {
        run_due(&mut svc, &e, 15 * 60);
    }
    let s = svc.status();
    assert_eq!(s.errors.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(), [OFFLINE_LINE]);
    assert!(s.offline && s.last_error.is_none());
    assert!(spy.0.lock().unwrap().is_empty(), "no notification");
    assert!(!e.photos.join("Lisbon/A.jpg").exists());
    e.offline.store(false, Ordering::SeqCst);
    run_due(&mut svc, &e, 15 * 60);
    assert!(e.photos.join("Lisbon/A.jpg").exists() && !svc.status().offline);
}

#[test]
fn paused_means_nothing_runs() {
    let e = env("e", "other");
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    svc.handle(Command::Pause);
    svc.handle(Command::FolderChanged);
    run_due(&mut svc, &e, 3 * 86_400);
    assert!(!e.photos.join("Lisbon/A.jpg").exists());
    assert!(svc.status().errors.is_empty());
    svc.handle(Command::Resume);
    run_due(&mut svc, &e, 0);
    assert!(e.photos.join("Lisbon/A.jpg").exists());
}

#[test]
fn a_config_without_folders_is_set_up_it_keeps_nothing_and_still_sends_what_was_imported() {
    let e = env("f", "other");
    let text = std::fs::read_to_string(&e.config).unwrap().replace("folders = [\"Lisbon\"]", "folders = []");
    std::fs::write(&e.config, text).unwrap();
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    assert!(svc.status().configured);
    run_due(&mut svc, &e, 60);
    assert!(!e.photos.join("Lisbon").exists(), "nothing is chosen, so nothing is downloaded");
    assert!(svc.status().errors.is_empty(), "{:?}", svc.status().errors);
}

#[cfg(not(feature = "home"))]
#[test]
fn the_light_build_given_a_home_config_says_so_once_and_does_not_sync() {
    let e = env("g", "home");
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    assert!(svc.status().configured, "it is set up, it just cannot run");
    run_due(&mut svc, &e, 20);
    for _ in 0..4 {
        run_due(&mut svc, &e, 15 * 60);
    }
    let s = svc.status();
    assert_eq!(s.errors.len(), 1, "{:?}", s.errors);
    assert!(s.errors[0].text.contains("light build") && s.errors[0].text.contains("full build"), "{:?}", s.errors);
    assert!(s.other.is_none());
    assert!(!e.photos.join("Lisbon/A.jpg").exists() && !e.state.join("other-state.json").exists());
    assert!(!e.cloud.join("inbox").exists());
}

// ------------------------------------------------------------------ photos imported on this PC

use lightcraft_cloud_client::other::{ImportQueue, Other};

/// LightCraft imports `n` small photos into `Holiday/Day 1` (written straight into the photo folder), and this PC's pass notices them
/// (here with no waiting for them to settle: a pass of the sync crate by hand, as the command line would run one).
fn import_card(e: &Env, n: usize, tag: &str) {
    for i in 0..n {
        put(&e.photos, &format!("Holiday/Day 1/P{i:03}.jpg"), format!("card photo {tag} {i} ").repeat(40).as_bytes());
    }
    let cfg = Config::load(Some(&e.config)).unwrap();
    let mut oc = cfg.other_config().unwrap();
    oc.settle = Duration::ZERO;
    let other = Other::new(oc, Arc::new(DirRemote::new(&e.cloud))).unwrap();
    let r = other.sync().unwrap();
    assert_eq!(r.picked_up, n, "{r:?}");
}

fn notes_in_cloud(e: &Env) -> usize {
    std::fs::read_dir(e.cloud.join("imports/laptop")).map(|rd| rd.flatten().filter(|f| f.file_name().to_string_lossy().ends_with(".json")).count()).unwrap_or(0)
}

#[test]
fn imported_photos_go_up_in_bounded_jobs_and_the_state_never_says_up_to_date_while_any_waits() {
    use lightcraft_cloud_desktop::appstate::HealthKind;
    let e = env("up1", "other");
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    run_due(&mut svc, &e, 20);
    assert_eq!(svc.status().health().kind, HealthKind::UpToDate, "{:?}", svc.status());
    import_card(&e, 60, "a");
    svc.handle(Command::UploadsChanged);
    let o = svc.status().other.clone().unwrap();
    assert_eq!(o.uploads_waiting, 60);
    let h = svc.status().health();
    assert_eq!((h.kind, h.text.as_str()), (HealthKind::Waiting, "60 photos waiting to upload"));
    // one bounded job at a time, a second apart
    let mut counts = Vec::new();
    for _ in 0..4 {
        e.clock.advance(Duration::from_secs(1));
        while svc.step() {
            counts.push(notes_in_cloud(&e));
        }
    }
    assert_eq!(counts, [25, 50, 60], "25 photos a job, and a second between jobs for everything else");
    let o = svc.status().other.clone().unwrap();
    assert_eq!((o.uploads_waiting, o.awaiting_home), (0, 60));
    let h = svc.status().health();
    assert_eq!((h.kind, h.text.as_str()), (HealthKind::Waiting, "60 photos sent, waiting for the home PC"));
    // the home PC answers: added (and one rejected); the answers are picked up at the next look
    let ids: Vec<String> = ImportQueue::new(&e.state).list().into_iter().map(|x| x.id).collect();
    for (i, id) in ids.iter().enumerate() {
        let status = if i == 0 { "rejected" } else { "added" };
        let reason = if i == 0 { json!("the downloaded P000.jpg does not match its note") } else { json!(null) };
        put(&e.cloud, &format!("imports-done/2026-11-01/laptop/{id}.result.json"), json!({"id": id, "status": status, "path": null, "reason": reason}).to_string().as_bytes());
    }
    e.clock.advance(Duration::from_secs(5 * 60));
    while svc.step() {}
    let s = svc.status();
    assert_eq!(s.other.as_ref().map(|o| o.awaiting_home), Some(0));
    let rejected: Vec<_> = s.errors.iter().filter(|x| x.text.contains("was not added at the home PC")).collect();
    assert_eq!(rejected.len(), 1, "{:?}", s.errors);
    assert!(rejected[0].text.contains("does not match its note"));
    // (the rejected one is told once; the rest are the home PC's now)
    e.clock.advance(Duration::from_secs(15 * 60));
    while svc.step() {}
    assert_eq!(svc.status().errors.iter().filter(|x| x.text.contains("was not added")).count(), 1);
}

#[test]
fn offline_the_queue_waits_in_one_amber_line_and_goes_up_when_the_network_is_back() {
    use lightcraft_cloud_desktop::appstate::{HealthKind, Tone};
    let e = env("up2", "other");
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    run_due(&mut svc, &e, 20);
    spy.0.lock().unwrap().clear(); // (the first pass downloaded the folder and said so)
    e.offline.store(true, Ordering::SeqCst);
    import_card(&e, 5, "b");
    svc.handle(Command::UploadsChanged);
    for _ in 0..10 {
        run_due(&mut svc, &e, 5 * 60);
    }
    let s = svc.status();
    assert_eq!(s.errors.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(), [OFFLINE_LINE], "one calm line however often it is tried");
    let h = s.health();
    assert_eq!((h.kind, h.tone, h.text.as_str()), (HealthKind::Offline, Tone::Offline, "Offline: will try again"));
    assert!(s.last_error.is_none());
    assert_eq!(s.other.as_ref().map(|o| o.uploads_waiting), Some(5));
    assert!(spy.0.lock().unwrap().is_empty(), "no notification");
    assert_eq!(notes_in_cloud(&e), 0);
    e.offline.store(false, Ordering::SeqCst);
    run_due(&mut svc, &e, 5 * 60);
    assert_eq!(notes_in_cloud(&e), 5);
    assert!(!svc.status().offline);
}

#[test]
fn a_restart_finds_the_queue_and_sends_it_and_pause_stops_the_sending() {
    let e = env("up3", "other");
    let spy = Spy::default();
    {
        let mut svc = service(&e, &spy, false);
        run_due(&mut svc, &e, 20);
        import_card(&e, 3, "c");
        // (the app quits before it sent anything)
    }
    let mut svc = service(&e, &spy, false);
    assert_eq!(svc.status().other.as_ref().map(|o| o.uploads_waiting), Some(3), "found again at the start");
    svc.handle(Command::Pause);
    run_due(&mut svc, &e, 3600);
    assert_eq!(notes_in_cloud(&e), 0, "paused: nothing is sent");
    svc.handle(Command::Resume);
    run_due(&mut svc, &e, 30);
    assert_eq!(notes_in_cloud(&e), 3);
}

#[test]
fn a_sidecar_edit_is_sent_before_the_rest_of_a_long_queue() {
    let e = env("up4", "other");
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    run_due(&mut svc, &e, 20);
    import_card(&e, 60, "d");
    svc.handle(Command::UploadsChanged);
    e.clock.advance(Duration::from_secs(1));
    assert!(svc.step()); // the first bounded job
    assert_eq!(notes_in_cloud(&e), 25);
    std::fs::write(e.photos.join("Lisbon/A.xmp"), b"<x:xmpmeta>edited on the laptop</x:xmpmeta>").unwrap();
    svc.handle(Command::FolderChanged);
    run_due(&mut svc, &e, 31);
    run_due(&mut svc, &e, 2);
    run_due(&mut svc, &e, 2);
    let puts = e.puts.lock().unwrap().clone();
    let edit = puts.iter().position(|p| p.starts_with("inbox/laptop/")).expect("the edit was sent");
    let last_upload = puts.iter().rposition(|p| p.starts_with("imports/laptop/") && p.ends_with(".json")).unwrap();
    let first_batch = puts.iter().enumerate().filter(|(_, p)| p.starts_with("imports/laptop/") && p.ends_with(".json")).map(|(i, _)| i).nth(24).unwrap();
    assert!(edit > first_batch && edit < last_upload, "the edit went between the upload jobs: batch {first_batch}, edit {edit}, end {last_upload}");
    assert_eq!(notes_in_cloud(&e), 60);
}

// ------------------------------------------------------------------ noticing what LightCraft imported, through the app

fn write_photo(e: &Env, rel: &str, tag: &str) {
    put(&e.photos, rel, format!("imported in lightcraft {tag} ").repeat(40).as_bytes());
}

#[test]
fn photos_written_into_nested_folders_are_found_after_they_settle_sent_and_their_folders_chosen_and_lightcraft_is_not_told() {
    let e = env("found", "other");
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    run_due(&mut svc, &e, 20);
    let told_before = e.lightcraft.0.lock().unwrap().len();
    write_photo(&e, "2026/261101 - Lisbon/Day 1/A.jpg", "a");
    write_photo(&e, "2026/261101 - Lisbon/Day 2/B.jpg", "b");
    write_photo(&e, "Root.jpg", "r");
    svc.handle(Command::FolderChanged);
    // 30 quiet seconds later the pass looks: the files are brand new, so nothing is taken yet, and another look is planned
    run_due(&mut svc, &e, 31);
    assert_eq!(notes_in_cloud(&e), 0);
    assert_eq!(ImportQueue::new(&e.state).list().len(), 0);
    // two minutes later they have settled: found, queued, sent
    run_due(&mut svc, &e, 125);
    run_due(&mut svc, &e, 2);
    assert_eq!(notes_in_cloud(&e), 3, "{:?}", svc.status());
    let c = Config::load(Some(&e.config)).unwrap();
    for f in ["2026/261101 - Lisbon/Day 1", "2026/261101 - Lisbon/Day 2", ""] {
        assert!(c.file.folders.iter().any(|x| x == f), "{f:?} is a chosen folder now: {:?}", c.file.folders);
    }
    assert!(c.file.folders.iter().any(|x| x == "Lisbon"), "what was chosen stays");
    let note = std::fs::read_dir(e.cloud.join("imports/laptop")).unwrap().flatten().find(|f| f.file_name().to_string_lossy().ends_with(".json")).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(note.path()).unwrap()).unwrap();
    assert!(v["folder"].as_str().is_some_and(|f| f.is_empty() || f.starts_with("2026/261101 - Lisbon/Day ")), "{v}");
    // LightCraft imported them: it is not told (only downloads go through the pending list)
    assert_eq!(e.lightcraft.0.lock().unwrap().len(), told_before);
    let pending = lightcraft_cloud_desktop::pending::PendingList::new(&e.state).list();
    assert!(pending.iter().all(|p| !p.path.contains("Day 1") && !p.path.contains("Root.jpg")), "{pending:?}");
}

#[test]
fn paused_nothing_is_found_or_sent_and_offline_a_new_photo_is_still_queued() {
    use lightcraft_cloud_desktop::appstate::HealthKind;
    let e = env("found2", "other");
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    run_due(&mut svc, &e, 20);
    svc.handle(Command::Pause);
    write_photo(&e, "Trip/A.jpg", "p");
    svc.handle(Command::FolderChanged);
    run_due(&mut svc, &e, 3 * 86_400);
    assert!(ImportQueue::new(&e.state).list().is_empty() && notes_in_cloud(&e) == 0, "paused: no pickup work, nothing sent");
    svc.handle(Command::Resume);
    e.offline.store(true, Ordering::SeqCst);
    run_due(&mut svc, &e, 1);
    run_due(&mut svc, &e, 15 * 60);
    run_due(&mut svc, &e, 15 * 60);
    assert_eq!(ImportQueue::new(&e.state).list().len(), 1, "offline, the photo was noticed and queued");
    assert_eq!(notes_in_cloud(&e), 0);
    let s = svc.status();
    assert_eq!(s.errors.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(), [OFFLINE_LINE]);
    assert_eq!(s.health().kind, HealthKind::Offline);
    e.offline.store(false, Ordering::SeqCst);
    run_due(&mut svc, &e, 15 * 60);
    run_due(&mut svc, &e, 5 * 60);
    assert_eq!(notes_in_cloud(&e), 1);
}

#[test]
fn the_photos_that_were_there_at_the_start_wait_for_send_them_and_the_status_counts_them() {
    let e = env("baseline", "other");
    write_photo(&e, "Old/A.jpg", "old a");
    write_photo(&e, "Old/B.jpg", "old b");
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    run_due(&mut svc, &e, 20); // the first look: the baseline
    run_due(&mut svc, &e, 15 * 60);
    assert_eq!(svc.status().other.as_ref().map(|o| o.held), Some(2), "{:?}", svc.status());
    assert_eq!(notes_in_cloud(&e), 0);
    let h = svc.status().health();
    assert_eq!(h.text, "Up to date", "informational: the user did not ask for them to be sent");
    // "Send them" (the Status window writes the request; the app notices the queue folder change and looks)
    ImportQueue::new(&e.state).request_send_held();
    svc.handle(Command::UploadsChanged);
    run_due(&mut svc, &e, 1);
    run_due(&mut svc, &e, 2);
    assert_eq!(notes_in_cloud(&e), 2);
    assert_eq!(svc.status().other.as_ref().map(|o| o.held), Some(0));
}

/// A remote whose downloads each take a moment, so that a pass lasts long enough to be seen from the side.
struct Slowly {
    inner: DirRemote,
    pause: Duration,
}

impl Remote for Slowly {
    fn list(&self, dir: &str, recursive: bool) -> Result<Vec<RemoteEntry>, Error> {
        self.inner.list(dir, recursive)
    }
    fn get(&self, path: &str, local: &Path) -> Result<(), Error> {
        if path.ends_with(".jpg") {
            std::thread::sleep(self.pause);
        }
        self.inner.get(path, local)
    }
    fn put(&self, local: &Path, path: &str) -> Result<(), Error> {
        self.inner.put(local, path)
    }
}

#[test]
fn a_real_pass_publishes_its_download_progress_while_it_runs_and_leaves_none_behind() {
    use lightcraft_cloud_client::Stage;
    use lightcraft_cloud_desktop::appstate::{Activity, AppStatus};
    let e = env("progress", "other");
    let spy = Spy::default();
    let mut other = OtherWorker::new(
        e.config.clone(),
        Box::new(|cfg: &Config| {
            let dir = lightcraft_cloud_desktop::config::local_remote(cfg.remote_spec()?)?.unwrap();
            Ok(Arc::new(Slowly { inner: DirRemote::new(dir), pause: Duration::from_millis(120) }) as Arc<dyn Remote>)
        }),
        e.clock.clone(),
    );
    other.set_retry_delay(Duration::ZERO);
    let worker = RoleWorker::new(e.config.clone(), None, Box::new(other));
    let seen: Arc<Mutex<Vec<AppStatus>>> = Arc::default();
    let s2 = seen.clone();
    let mut svc = Service::new(
        worker,
        e.clock.clone(),
        Box::new(spy.clone()),
        e.state.clone(),
        Timing::default(),
        Box::new(move |s| s2.lock().unwrap().push(s.clone())),
        Box::new(|_| {}),
    );
    svc.set_progress_interval(Duration::from_millis(30));
    run_due(&mut svc, &e, 20);
    assert!(e.photos.join("Lisbon/A.jpg").exists() && e.photos.join("Lisbon/B.jpg").exists());
    let all = seen.lock().unwrap().clone();
    let stages: Vec<Stage> = all.iter().filter_map(|s| s.activity.progress()).map(|p| p.stage).collect();
    assert!(stages.contains(&Stage::Downloading), "the pass was seen downloading: {stages:?}");
    let dl: Vec<_> = all.iter().filter_map(|s| s.activity.progress()).filter(|p| p.stage == Stage::Downloading).collect();
    assert!(dl.iter().all(|p| p.total_items == 2 && p.total_bytes == 2 * photo("A").len() as u64));
    assert!(matches!(all.last().unwrap().activity, Activity::Idle));
    assert!(AppStatus::load(&e.state).unwrap().activity.progress().is_none());
}

/// For looking at the Status window by hand, not a test: `LC_DEMO_DIR=<scratch folder> cargo test --test other_app demo_ -- --ignored`
/// runs a pass of another PC against a made-up cloud library (a folder of generated files in the scratch folder, never anything of the
/// user's) whose downloads are slow, with the app's instance lock held and status.json written as the app writes it, for about a
/// minute. Open the window on `<dir>/cfg/config.toml` meanwhile.
#[test]
#[ignore = "a demo for a screenshot; set LC_DEMO_DIR"]
fn demo_a_long_download_for_the_status_window() {
    let Some(dir) = std::env::var_os("LC_DEMO_DIR").map(PathBuf::from) else { return };
    let _ = std::fs::remove_dir_all(&dir);
    let (cloud, photos, state, config) = (dir.join("cloud"), dir.join("pc/photos"), dir.join("pc/state"), dir.join("cfg/config.toml"));
    let mut list = Vec::new();
    for i in 0..60 {
        let bytes: Vec<u8> = (0..(2 << 20) + i * 40_000).map(|n| ((n * 31 + i * 7) % 251) as u8).collect();
        let path = format!("Lisbon/IMG_{i:04}.jpg");
        put(&cloud, &format!("originals/{path}"), &bytes);
        list.push(json!({"id": content_hash_bytes(&bytes), "path": path, "size": bytes.len(), "mtime": MTIME}));
    }
    put(&cloud, "mobile/manifest.json", json!({"format": 1, "generated": "2026-11-01T09:00:00Z", "photos": list}).to_string().as_bytes());
    std::fs::create_dir_all(&photos).unwrap();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        format!(
            "role = \"other\"\nsource = '{}'\nremote = '{}'\nstate_dir = '{}'\nfolders = [\"Lisbon\"]\ndevice = \"demo-laptop\"\nnotify = false\n",
            photos.display(),
            cloud.display(),
            state.display()
        ),
    )
    .unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let _instance = lightcraft_cloud_desktop::instance::InstanceLock::acquire(&state).unwrap().expect("a lock of our own");
    let clock: Arc<dyn lightcraft_cloud_desktop::clock::Clock> = Arc::new(lightcraft_cloud_desktop::clock::SystemClock);
    let pause: u64 = std::env::var("LC_DEMO_PAUSE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(900);
    let other = OtherWorker::new(
        config.clone(),
        Box::new(move |cfg: &Config| {
            let dir = lightcraft_cloud_desktop::config::local_remote(cfg.remote_spec()?)?.unwrap();
            Ok(Arc::new(Slowly { inner: DirRemote::new(dir), pause: Duration::from_millis(pause) }) as Arc<dyn Remote>)
        }),
        clock.clone(),
    );
    let worker = RoleWorker::new(config.clone(), None, Box::new(other));
    let mut svc = Service::new(worker, clock, Box::new(Spy::default()), state.clone(), Timing::default(), Box::new(|_| {}), Box::new(|_| {}));
    svc.handle(Command::SyncNow);
    assert!(svc.step());
    eprintln!("demo finished: {:?}", svc.status().last_sync);
}
