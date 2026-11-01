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
struct FakeLightCraft(Arc<Mutex<Vec<String>>>);

impl LightCraftLink for FakeLightCraft {
    fn identify(&mut self) -> Result<(), LinkError> {
        Ok(())
    }
    fn import_add(&mut self, paths: &[String]) -> Result<ImportReply, LinkError> {
        self.0.lock().unwrap().extend(paths.iter().cloned());
        Ok(ImportReply { imported: paths.len(), ..ImportReply::default() })
    }
}

/// A remote that can lose its network.
struct Switch {
    inner: DirRemote,
    offline: Arc<AtomicBool>,
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
            "role = \"{role}\"\nsource = \"{}\"\nremote = \"{}\"\nstate_dir = \"{}\"\nfolders = [\"Lisbon\"]\ndevice = \"laptop\"\nnotify = true\nlightcraft_control_port = 7999\n",
            photos.display(),
            cloud.display(),
            state.display()
        ),
    )
    .unwrap();
    Env { root, cloud, photos, state, config, clock: Arc::new(FakeClock::new()), offline: Arc::default(), lightcraft: FakeLightCraft::default() }
}

fn service(e: &Env, spy: &Spy, with_home: bool) -> Service<RoleWorker> {
    let offline = e.offline.clone();
    let mut other = OtherWorker::new(
        e.config.clone(),
        Box::new(move |cfg: &Config| {
            let dir = lightcraft_cloud_desktop::config::local_remote(cfg.remote_spec()?)?.unwrap();
            Ok(Arc::new(Switch { inner: DirRemote::new(dir), offline: offline.clone() }) as Arc<dyn Remote>)
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
    let mut told = e.lightcraft.0.lock().unwrap().clone();
    told.sort();
    assert_eq!(told, [e.photos.join("Lisbon/A.jpg").display().to_string(), e.photos.join("Lisbon/B.jpg").display().to_string()]);
    assert_eq!(s.lightcraft_waiting, 0);
    // the next pass has nothing to do, so says nothing
    run_due(&mut svc, &e, 15 * 60);
    assert_eq!(spy.0.lock().unwrap().len(), 1);
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
fn a_config_without_folders_is_not_set_up() {
    let e = env("f", "other");
    let text = std::fs::read_to_string(&e.config).unwrap().replace("folders = [\"Lisbon\"]", "folders = []");
    std::fs::write(&e.config, text).unwrap();
    let spy = Spy::default();
    let mut svc = service(&e, &spy, false);
    assert!(!svc.status().configured);
    run_due(&mut svc, &e, 60);
    assert!(!e.photos.join("Lisbon").exists());
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
