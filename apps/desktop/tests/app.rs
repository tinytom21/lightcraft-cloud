//! The background engine on real sync code: `HomeWorker` + `Home` on a generated photo folder and a folder standing in for the
//! remote, with a trivial renderer (the engine itself is covered by `engine.rs`), a fake clock, and a fake `restic`. Nothing
//! touches the user's config, state, remotes or desktop.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lightcraft_cloud_client::Result;
use lightcraft_cloud_client::home::{Added, PhotoInfo, Renderer};
use lightcraft_cloud_desktop::appstate::AppStatus;
use lightcraft_cloud_desktop::clock::{Clock, FakeClock};
use lightcraft_cloud_desktop::config::Config;
use lightcraft_cloud_desktop::notifier::Notifier;
use lightcraft_cloud_desktop::scheduler::Timing;
use lightcraft_cloud_desktop::service::{Command, Service};
use lightcraft_cloud_desktop::worker::HomeWorker;
use sha2::{Digest, Sha256};

struct Trivial;

impl Renderer for Trivial {
    fn add(&mut self, photo: &Path) -> Result<Added> {
        let hash: String = Sha256::digest(std::fs::read(photo).unwrap()).iter().take(16).map(|b| format!("{b:02x}")).collect();
        Ok(Added::Photo(PhotoInfo { content_hash: hash, width: Some(10), height: Some(10), ..PhotoInfo::default() }))
    }
    fn reread(&mut self, photo: &Path) -> Result<PhotoInfo> {
        match self.add(photo)? {
            Added::Photo(p) => Ok(p),
            Added::Duplicate => unreachable!(),
        }
    }
    fn thumbnail(&mut self, _photo: &Path, _long_edge: u32, _quality: u8, out: &Path) -> Result<()> {
        std::fs::write(out, b"thumb").map_err(|e| lightcraft_cloud_client::Error::Io(e.to_string()))
    }
    fn smart_preview(&mut self, photo: &Path, out: &Path) -> Result<String> {
        std::fs::write(out, b"preview").map_err(|e| lightcraft_cloud_client::Error::Io(e.to_string()))?;
        Ok(photo.file_stem().unwrap().to_string_lossy().into_owned())
    }
    fn forget(&mut self, _photos: &[PathBuf]) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Spy(Arc<Mutex<Vec<String>>>);

impl Notifier for Spy {
    fn notify(&self, _s: &str, body: &str) {
        self.0.lock().unwrap().push(body.to_string());
    }
}

struct Env {
    root: PathBuf,
    photos: PathBuf,
    remote: PathBuf,
    state: PathBuf,
    config: PathBuf,
    clock: Arc<FakeClock>,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fake_restic(dir: &Path, time: &str) -> PathBuf {
    let f = dir.join("restic");
    std::fs::write(&f, format!("#!/bin/sh\necho '[{{\"time\":\"{time}\"}}]'\n")).unwrap();
    std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    f
}

fn env(tag: &str, backup_time: &str) -> Env {
    let root = std::env::temp_dir().join(format!("lc-desktop-app-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (photos, remote, state, config) = (root.join("photos"), root.join("remote"), root.join("state"), root.join("cfg/config.toml"));
    for (i, name) in ["a.jpg", "2024/b.jpg"].iter().enumerate() {
        let p = photos.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("photo number {i}")).unwrap();
        std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(86_400)).unwrap();
    }
    std::fs::create_dir_all(&remote).unwrap();
    let restic = fake_restic(&root, backup_time);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        format!(
            "source = '{}'\nremote = '{}'\nstate_dir = '{}'\nnotify = true\nphoto_backup = '{}'\ndevice_history_days = 30\n",
            photos.display(),
            remote.display(),
            state.display(),
            restic.display()
        ),
    )
    .unwrap();
    Env { root, photos, remote, state, config, clock: Arc::new(FakeClock::new()) }
}

fn service(e: &Env, spy: &Spy) -> Service<HomeWorker> {
    let worker =
        HomeWorker::new(e.config.clone(), Box::new(|_cfg: &Config| Box::new(Trivial)), Box::new(|cfg: &Config| cfg.open_remote()), e.clock.clone());
    Service::new(worker, e.clock.clone(), Box::new(spy.clone()), e.state.clone(), Timing::default(), Box::new(|_| {}), Box::new(|_| {}))
}

fn run_due(svc: &mut Service<HomeWorker>, e: &Env, secs: u64) {
    e.clock.advance(Duration::from_secs(secs));
    while svc.step() {}
}

#[test]
fn the_first_sync_publishes_the_folder_and_the_status_says_so() {
    let e = env("a", "2026-11-01T00:00:00Z");
    let spy = Spy::default();
    let mut svc = service(&e, &spy);
    assert!(svc.status().configured);
    run_due(&mut svc, &e, 20);
    let s = svc.status();
    assert!(s.last_error.is_none(), "{:?}", s.errors);
    let last = s.last_sync.as_ref().unwrap();
    assert_eq!((last.ok, last.photos), (true, 2));
    assert!(e.remote.join("mobile/manifest.json").is_file());
    assert!(e.remote.join("originals/a.jpg").is_file() && e.remote.join("originals/2024/b.jpg").is_file());
    assert_eq!(AppStatus::load(&e.state).unwrap().last_sync.unwrap().photos, 2);
    // the app's own state: the sync's lock, and nothing under the photo folder was written
    assert!(e.state.join("state.json").is_file());
    assert!(!std::fs::read_dir(&e.photos).unwrap().flatten().any(|x| x.file_name().to_string_lossy().ends_with(".xmp")));
    assert!(spy.0.lock().unwrap().is_empty());
}

#[test]
fn a_phone_edit_in_the_inbox_is_noticed_by_the_cheap_check_and_synced() {
    let e = env("b", "2026-11-01T00:00:00Z");
    let spy = Spy::default();
    let mut svc = service(&e, &spy);
    run_due(&mut svc, &e, 20);
    assert_eq!(svc.status().inbox_pending, Some(0));
    // a (junk) edit waits under a valid inbox name; the check sees it, a sync follows and settles it (rejected, kept aside)
    let name = format!("phone1/{}-20261101T000100Z-none.xmp", "a".repeat(32));
    let p = e.remote.join("inbox").join(&name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, b"not an xmp").unwrap();
    run_due(&mut svc, &e, 120);
    assert!(!p.exists(), "the sync should have taken the file out of the inbox");
    assert!(e.remote.join("inbox-done").is_dir());
    assert_eq!(svc.status().inbox_pending, Some(0));
    // stray files do not start anything
    std::fs::write(e.remote.join("inbox/stray.txt"), b"x").unwrap();
    let before = svc.status().last_sync.clone();
    run_due(&mut svc, &e, 120);
    assert_eq!(svc.status().last_sync, before);
}

#[test]
fn a_folder_change_syncs_a_new_photo_after_the_quiet_time() {
    let e = env("c", "2026-11-01T00:00:00Z");
    let mut svc = service(&e, &Spy::default());
    run_due(&mut svc, &e, 20);
    let p = e.photos.join("c.jpg");
    std::fs::write(&p, b"a third photo").unwrap();
    std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(86_400)).unwrap();
    svc.handle(Command::FolderChanged);
    run_due(&mut svc, &e, 29);
    assert_eq!(svc.status().last_sync.as_ref().unwrap().photos, 2);
    run_due(&mut svc, &e, 1);
    assert_eq!(svc.status().last_sync.as_ref().unwrap().photos, 3);
}

#[test]
fn a_missing_photo_folder_is_a_refusal_shown_to_the_user_not_a_crash() {
    let e = env("d", "2026-11-01T00:00:00Z");
    let spy = Spy::default();
    let mut svc = service(&e, &spy);
    std::fs::remove_dir_all(&e.photos).unwrap();
    run_due(&mut svc, &e, 20);
    let s = svc.status();
    assert!(s.last_error.as_deref().unwrap().contains("does not exist"), "{:?}", s.last_error);
    assert_eq!(spy.0.lock().unwrap().len(), 1);
}

#[test]
fn the_weekly_purge_deletes_old_history_when_the_backup_is_fresh() {
    let e = env("e", "2026-10-31T20:00:00Z"); // 4 h before the fake clock's start
    let old = e.remote.join("history/2026-01-01/a.xmp");
    let new = e.remote.join("history/2026-10-30/a.xmp");
    for p in [&old, &new] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }
    let mut svc = service(&e, &Spy::default());
    run_due(&mut svc, &e, 40);
    assert!(!old.exists() && new.exists());
    assert!(svc.status().last_purge.is_some() && svc.status().notices.is_empty());
}

#[test]
fn the_purge_does_nothing_without_a_recent_backup() {
    let e = env("f", "2026-10-01T00:00:00Z"); // a month old
    let old = e.remote.join("history/2026-01-01/a.xmp");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, b"x").unwrap();
    let mut svc = service(&e, &Spy::default());
    run_due(&mut svc, &e, 40);
    assert!(old.exists());
    let s = svc.status();
    assert!(s.notices[0].text.contains("History purge skipped") && s.notices[0].text.contains("refusing to purge history"), "{:?}", s.notices);
    assert!(s.last_error.is_none());
}

#[test]
fn without_a_config_the_app_waits_for_setup_and_picks_the_config_up_when_it_appears() {
    let e = env("g", "2026-11-01T00:00:00Z");
    let saved = std::fs::read_to_string(&e.config).unwrap();
    std::fs::remove_file(&e.config).unwrap();
    let mut svc = service(&e, &Spy::default());
    assert!(!svc.status().configured);
    run_due(&mut svc, &e, 3600);
    assert!(svc.status().last_sync.is_none() && !e.remote.join("mobile").exists());
    std::fs::write(&e.config, saved).unwrap();
    svc.handle(Command::ConfigChanged);
    while svc.step() {}
    assert!(svc.status().configured);
    assert_eq!(svc.status().last_sync.as_ref().unwrap().photos, 2);
}

#[test]
fn the_cli_and_the_app_do_not_sync_at_the_same_time() {
    use lightcraft_cloud_client::home::RunLock;
    let e = env("h", "2026-11-01T00:00:00Z");
    let mut svc = service(&e, &Spy::default());
    let held = RunLock::acquire(&e.state, false).unwrap().unwrap(); // `sync --once` running
    run_due(&mut svc, &e, 20);
    assert!(svc.status().last_sync.is_none() && svc.status().last_error.is_none() && !e.remote.join("mobile").exists());
    drop(held);
    run_due(&mut svc, &e, 60);
    assert_eq!(svc.status().last_sync.as_ref().unwrap().photos, 2);
    let _ = e.clock.now();
}

// ------------------------------------------------------------------ telling LightCraft about adopted photos

/// A LightCraft on a loopback port: the command list, and an import that adds everything it is sent. Keeps the lines it got.
fn fake_lightcraft(port: u16) -> Arc<Mutex<Vec<String>>> {
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut w = stream.try_clone().unwrap();
            for line in BufReader::new(stream).lines().map_while(std::result::Result::ok) {
                s.lock().unwrap().push(line.clone());
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                let id = &v["id"];
                let reply = if v["method"] == "engine.commands" {
                    serde_json::json!({"id": id, "ok": true, "result": [{"id": "library.folders", "enabled": true}, {"id": "library.import", "enabled": true}]})
                } else {
                    let n = v["params"]["params"]["paths"].as_array().unwrap().len();
                    serde_json::json!({"id": id, "ok": true, "result": {"duplicates": [], "failed": [], "imported": (1..=n).collect::<Vec<_>>(), "scanned": n, "sidecars": 0}})
                };
                let _ = writeln!(w, "{reply}");
            }
        }
    });
    seen
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn an_adopted_photo_waits_for_lightcraft_across_a_restart_and_goes_in_when_it_opens() {
    let e = env("lc", "2026-11-01T00:00:00Z");
    let port = free_port();
    let mut cfg = std::fs::read_to_string(&e.config).unwrap();
    cfg.push_str(&format!("lightcraft_control_port = {port}\nsome_future_key = 1\n"));
    std::fs::write(&e.config, cfg).unwrap();
    // another PC uploaded a photo
    let bytes = b"a photo from the laptop".to_vec();
    let id = lightcraft_cloud_client::home::content_hash_bytes(&bytes);
    let put = |rel: &str, data: &[u8]| {
        let p = e.remote.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    };
    put(&format!("imports/laptop/{id}/c.jpg"), &bytes);
    put(
        &format!("imports/laptop/{id}.json"),
        serde_json::json!({"format": 1, "id": id, "folder": "Trip", "name": "c.jpg", "size": bytes.len(), "mtime": "2024-05-01T12:00:00Z", "sidecar": null}).to_string().as_bytes(),
    );
    let spy = Spy::default();
    let mut svc = service(&e, &spy);
    run_due(&mut svc, &e, 20); // (nothing listens on the port: LightCraft is closed)
    let photo = std::fs::canonicalize(&e.photos).unwrap().join("Trip/c.jpg");
    assert_eq!(std::fs::read(&photo).unwrap(), bytes, "the photo was adopted");
    let st = svc.status().clone();
    assert!(st.last_error.is_none(), "{:?}", st.errors);
    assert_eq!((st.lightcraft_port, st.lightcraft_waiting, st.lightcraft_reachable), (port, 1, Some(false)));
    assert!(e.state.join("lightcraft-pending").is_dir());
    // the app restarts: the list is still there; LightCraft is still closed
    drop(svc);
    let mut svc = service(&e, &spy);
    run_due(&mut svc, &e, 120);
    assert_eq!((svc.status().lightcraft_waiting, svc.status().lightcraft_reachable), (1, Some(false)));
    // LightCraft opens
    let seen = fake_lightcraft(port);
    run_due(&mut svc, &e, 120);
    let st = svc.status();
    assert_eq!((st.lightcraft_waiting, st.lightcraft_reachable), (0, Some(true)));
    assert!(st.errors.is_empty(), "{:?}", st.errors);
    let lines = seen.lock().unwrap().clone();
    assert_eq!(lines.len(), 2, "{lines:?}");
    let sent: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(sent["params"]["command"], "library.import");
    assert_eq!(sent["params"]["params"], serde_json::json!({"paths": [photo.to_str().unwrap()], "mode": "add"}));
    assert!(lightcraft_cloud_desktop::pending::PendingList::new(&e.state).list().is_empty());
    // nothing is sent again
    run_due(&mut svc, &e, 240);
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[test]
fn port_zero_turns_the_feature_off_nothing_is_queued_or_sent() {
    let e = env("lc0", "2026-11-01T00:00:00Z");
    let mut cfg = std::fs::read_to_string(&e.config).unwrap();
    cfg.push_str("lightcraft_control_port = 0\n");
    std::fs::write(&e.config, cfg).unwrap();
    let bytes = b"another photo".to_vec();
    let id = lightcraft_cloud_client::home::content_hash_bytes(&bytes);
    let put = |rel: &str, data: &[u8]| {
        let p = e.remote.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    };
    put(&format!("imports/laptop/{id}/d.jpg"), &bytes);
    put(
        &format!("imports/laptop/{id}.json"),
        serde_json::json!({"format": 1, "id": id, "folder": "", "name": "d.jpg", "size": bytes.len(), "mtime": "2024-05-01T12:00:00Z"}).to_string().as_bytes(),
    );
    let mut svc = service(&e, &Spy::default());
    run_due(&mut svc, &e, 20);
    assert!(e.photos.join("d.jpg").is_file());
    assert!(!e.state.join("lightcraft-pending").exists());
    assert_eq!((svc.status().lightcraft_port, svc.status().lightcraft_waiting), (0, 0));
}

/// A remote whose uploads each take a moment (for the demo below).
struct SlowPuts {
    inner: lightcraft_cloud_client::DirRemote,
    pause: Duration,
}

impl lightcraft_cloud_client::Remote for SlowPuts {
    fn list(&self, dir: &str, recursive: bool) -> Result<Vec<lightcraft_cloud_client::RemoteEntry>> {
        self.inner.list(dir, recursive)
    }
    fn get(&self, path: &str, local: &Path) -> Result<()> {
        self.inner.get(path, local)
    }
    fn put(&self, local: &Path, path: &str) -> Result<()> {
        if path.starts_with("originals/") {
            std::thread::sleep(self.pause);
        }
        self.inner.put(local, path)
    }
}

/// For looking at the Status window by hand, not a test: `LC_DEMO_DIR=<scratch folder> cargo test --test app demo_ -- --ignored` publishes
/// a made-up folder of generated photos (in the scratch folder, never anything of the user's) with slow uploads and a trivial renderer,
/// with the app's instance lock held and status.json written as the app writes it, for about a minute.
#[test]
#[ignore = "a demo for a screenshot; set LC_DEMO_DIR"]
fn demo_a_long_publish_for_the_status_window() {
    let Some(dir) = std::env::var_os("LC_DEMO_DIR").map(PathBuf::from) else { return };
    let _ = std::fs::remove_dir_all(&dir);
    let (photos, remote, state, config) = (dir.join("photos"), dir.join("remote"), dir.join("state"), dir.join("cfg/config.toml"));
    for i in 0..40 {
        let bytes: Vec<u8> = (0..(2 << 20) + i * 50_000).map(|n| ((n * 29 + i * 11) % 249) as u8).collect();
        let p = photos.join(format!("Trip/IMG_{i:04}.jpg"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(86_400)).unwrap();
    }
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, format!("source = '{}'\nremote = '{}'\nstate_dir = '{}'\nnotify = false\n", photos.display(), remote.display(), state.display())).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let _instance = lightcraft_cloud_desktop::instance::InstanceLock::acquire(&state).unwrap().expect("a lock of our own");
    let clock: Arc<dyn Clock> = Arc::new(lightcraft_cloud_desktop::clock::SystemClock);
    let pause: u64 = std::env::var("LC_DEMO_PAUSE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000);
    let worker = HomeWorker::new(
        config.clone(),
        Box::new(|_cfg: &Config| Box::new(Trivial)),
        Box::new(move |cfg: &Config| {
            let dir = lightcraft_cloud_desktop::config::local_remote(cfg.remote_spec()?)?.unwrap();
            Ok(Arc::new(SlowPuts { inner: lightcraft_cloud_client::DirRemote::new(dir), pause: Duration::from_millis(pause) }) as Arc<dyn lightcraft_cloud_client::Remote>)
        }),
        clock.clone(),
    );
    let mut svc = Service::new(worker, clock, Box::new(Spy::default()), state, Timing::default(), Box::new(|_| {}), Box::new(|_| {}));
    svc.handle(Command::SyncNow);
    assert!(svc.step());
    eprintln!("demo finished: {:?}", svc.status().last_sync);
}
