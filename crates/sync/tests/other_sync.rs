//! The other PC's sync pass (FORMAT.md, "A PC that is not the home PC"): what it downloads, what it sends back, what it replaces,
//! what it moves aside, and what it leaves alone. The cloud library is built by hand here (no home sync): the manifest is written
//! the way the publisher writes it, and a "home merge" is a new sidecar plus a new manifest. One test per rule.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{sc, sha16, sha_hex, temp};
use lightcraft_cloud_client::home::{content_hash_bytes, parse_inbox_name};
use lightcraft_cloud_client::other::{Other, OtherConfig, OtherReport, list_folders};
use lightcraft_cloud_client::{DirRemote, Error, Remote, RemoteEntry};
use serde_json::{Value, json};

const MTIME: &str = "2024-05-01T12:00:00Z";
const MTIME_EPOCH: u64 = 1_714_564_800;
const DEV: &str = "laptop";

fn photo_bytes(tag: &str) -> Vec<u8> {
    format!("photo bytes of {tag} ").repeat(40).into_bytes()
}

/// A LightCraft-style sidecar; `n` makes versions that differ.
fn xmp(n: i64) -> Vec<u8> {
    sc().s(&[("light.exposure", json!(n as f64 / 10.0))]).rating(n as i32 % 6).build()
}

/// One photo of the cloud library.
#[derive(Clone)]
struct Ph {
    path: String,
    bytes: Vec<u8>,
    sidecar: Option<(String, Vec<u8>)>,
    alts: Vec<Ph>,
}

fn ph(path: &str) -> Ph {
    Ph { path: path.into(), bytes: photo_bytes(path), sidecar: None, alts: vec![] }
}

impl Ph {
    fn sc(mut self, path: &str, data: Vec<u8>) -> Self {
        self.sidecar = Some((path.into(), data));
        self
    }
    fn alt(mut self, a: Ph) -> Self {
        self.alts.push(a);
        self
    }
    fn id(&self) -> String {
        content_hash_bytes(&self.bytes)
    }
    fn json(&self) -> Value {
        let mut v = json!({"id": self.id(), "path": self.path, "size": self.bytes.len(), "mtime": MTIME,
            "sidecar": self.sidecar.as_ref().map(|s| s.0.clone()), "sidecarVersion": self.sidecar.as_ref().map(|s| sha16(&s.1))});
        if !self.alts.is_empty() {
            v["alternates"] = Value::Array(self.alts.iter().map(Ph::json).collect());
        }
        v
    }
    fn files(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = vec![(self.path.clone(), self.bytes.clone())];
        out.extend(self.sidecar.clone());
        for a in &self.alts {
            out.extend(a.files());
        }
        out
    }
}

/// A remote that counts `get`s, and can be told to fail them.
struct Counting {
    inner: DirRemote,
    gets: AtomicUsize,
    /// Fail every `get` from the nth on (0: never).
    fail_from: AtomicUsize,
    offline: std::sync::atomic::AtomicBool,
}

impl Counting {
    fn down(&self) -> Result<(), Error> {
        if self.offline.load(Ordering::SeqCst) { Err(Error::Remote("network is unreachable".into())) } else { Ok(()) }
    }
}

impl Remote for Counting {
    fn list(&self, dir: &str, recursive: bool) -> Result<Vec<RemoteEntry>, Error> {
        self.down()?;
        self.inner.list(dir, recursive)
    }
    fn get(&self, path: &str, local: &Path) -> Result<(), Error> {
        self.down()?;
        let n = self.gets.fetch_add(1, Ordering::SeqCst) + 1;
        let from = self.fail_from.load(Ordering::SeqCst);
        if from != 0 && n >= from {
            return Err(Error::Remote(format!("connection lost on {path}")));
        }
        self.inner.get(path, local)
    }
    fn put(&self, local: &Path, path: &str) -> Result<(), Error> {
        self.down()?;
        self.inner.put(local, path)
    }
}

struct Fx {
    root: PathBuf,
    remote: PathBuf,
    src: PathBuf,
    state: PathBuf,
    other: Other,
    net: Arc<Counting>,
    log: Arc<Mutex<Vec<String>>>,
    hooked: Arc<Mutex<Vec<PathBuf>>>,
    hook_fails: Arc<std::sync::atomic::AtomicBool>,
    clock: Arc<Mutex<SystemTime>>,
    photos: Vec<Ph>,
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn start() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_793_527_200) // 2026-11-01T10:00:00Z
}

impl Fx {
    fn new(tag: &str, folders: &[&str]) -> Fx {
        Fx::with(tag, folders, |_| {})
    }

    fn with(tag: &str, folders: &[&str], tune: impl FnOnce(&mut OtherConfig)) -> Fx {
        let root = temp(tag);
        let (remote, src, state) = (root.join("remote"), root.join("pc/photos"), root.join("pc/state"));
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        let mut cfg = OtherConfig::new(&src, &state, DEV, folders.iter().map(|f| f.to_string()).collect());
        cfg.free_margin = 0;
        tune(&mut cfg);
        let net = Arc::new(Counting {
            inner: DirRemote::new(&remote),
            gets: AtomicUsize::new(0),
            fail_from: AtomicUsize::new(0),
            offline: Default::default(),
        });
        let mut other = Other::new(cfg, net.clone()).unwrap();
        other.set_retry_delay(Duration::ZERO);
        let clock = Arc::new(Mutex::new(start()));
        let c = clock.clone();
        other.set_clock(move || *c.lock().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        other.set_logger(move |m| l.lock().unwrap().push(m.to_string()));
        let hooked = Arc::new(Mutex::new(Vec::new()));
        let hook_fails = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (h, hf) = (hooked.clone(), hook_fails.clone());
        other.set_adopted_hook(move |p| {
            if hf.load(Ordering::SeqCst) {
                return Err("LightCraft is not there".into());
            }
            h.lock().unwrap().push(p.to_path_buf());
            Ok(())
        });
        Fx { root, remote, src, state, other, net, log, hooked, hook_fails, clock, photos: vec![] }
    }

    fn tick(&self, secs: u64) {
        let mut c = self.clock.lock().unwrap();
        *c += Duration::from_secs(secs);
    }

    fn put(&self, rel: &str, data: &[u8]) {
        let p = self.remote.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    /// The cloud library as the publisher leaves it: the photos' files under originals/, and the manifest, written last.
    fn publish(&mut self, photos: Vec<Ph>) {
        self.publish_with_format(photos, 1);
    }

    fn publish_with_format(&mut self, photos: Vec<Ph>, format: u64) {
        for p in &photos {
            for (rel, data) in p.files() {
                self.put(&format!("originals/{rel}"), &data);
            }
        }
        let m = json!({"format": format, "generated": "2026-11-01T09:00:00Z", "publisher": "test", "photos": photos.iter().map(Ph::json).collect::<Vec<_>>()});
        self.put("mobile/manifest.json", m.to_string().as_bytes());
        self.photos = photos;
    }

    /// The home PC changed the sidecar of photo `path` (and republished).
    fn home_edits(&mut self, path: &str, sidecar: Vec<u8>) {
        let mut photos = self.photos.clone();
        let p = photos.iter_mut().find(|p| p.path == path).unwrap();
        let name = p.sidecar.as_ref().map_or_else(|| format!("{path}.xmp"), |s| s.0.clone());
        p.sidecar = Some((name, sidecar));
        self.publish(photos);
    }

    /// The home PC deleted photo `path`.
    fn home_deletes(&mut self, path: &str) {
        let photos: Vec<Ph> = self.photos.iter().filter(|p| p.path != path).cloned().collect();
        self.publish(photos);
    }

    fn sync(&self) -> OtherReport {
        self.other.sync().unwrap_or_else(|e| panic!("sync failed: {e}"))
    }

    fn local(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.src.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn has(&self, rel: &str) -> bool {
        self.src.join(rel).exists()
    }

    /// The photo folder's visible files.
    fn tree(&self) -> Vec<String> {
        tree(&self.src).into_iter().filter(|p| !p.split('/').any(|c| c.starts_with('.'))).collect()
    }

    fn hidden(&self) -> Vec<String> {
        tree(&self.src).into_iter().filter(|p| p.split('/').any(|c| c.starts_with('.'))).collect()
    }

    fn inbox(&self) -> Vec<String> {
        let mut v: Vec<String> = tree(&self.remote.join("inbox")).into_iter().collect();
        v.sort();
        v
    }

    fn edit_locally(&self, rel: &str, data: &[u8]) {
        std::fs::write(self.src.join(rel), data).unwrap();
    }

    fn logged(&self, what: &str) -> bool {
        self.log.lock().unwrap().iter().any(|l| l.contains(what))
    }

    fn state_tree(&self, sub: &str) -> Vec<String> {
        tree(&self.state.join(sub))
    }
}

fn tree(root: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn mtime_of(p: &Path) -> u64 {
    std::fs::metadata(p).unwrap().modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

// ------------------------------------------------------------------------------------------------ 1, 2: downloading

#[test]
fn only_the_chosen_folders_are_downloaded_with_their_sidecars_and_alternates() {
    let mut fx = Fx::new("dl", &["Lisbon"]);
    fx.publish(vec![
        ph("Lisbon/A.jpg").sc("Lisbon/A.xmp", xmp(1)),
        ph("Lisbon/B.ARW").sc("Lisbon/B.xmp", xmp(2)).alt(ph("Lisbon/B.JPG").sc("Lisbon/B.JPG.xmp", xmp(3))),
        ph("Porto/C.jpg").sc("Porto/C.xmp", xmp(4)),
    ]);
    let r = fx.sync();
    assert_eq!(fx.tree(), ["Lisbon/A.jpg", "Lisbon/A.xmp", "Lisbon/B.ARW", "Lisbon/B.JPG", "Lisbon/B.JPG.xmp", "Lisbon/B.xmp"]);
    assert_eq!(fx.local("Lisbon/A.jpg"), photo_bytes("Lisbon/A.jpg"));
    assert_eq!(fx.local("Lisbon/B.JPG.xmp"), xmp(3));
    assert_eq!(r.downloaded.len(), 3, "{r:?}");
    assert_eq!((r.photos, r.waiting_to_download, r.edits_waiting), (3, 0, 0));
    assert!(fx.hidden().is_empty(), "no temporary file is left: {:?}", fx.hidden());
    // LightCraft is told about each new photo, by its absolute path
    let mut told = fx.hooked.lock().unwrap().clone();
    told.sort();
    assert_eq!(told, [fx.src.join("Lisbon/A.jpg"), fx.src.join("Lisbon/B.ARW"), fx.src.join("Lisbon/B.JPG")]);
    // a second pass has nothing to do and tells no one again
    let r2 = fx.sync();
    assert!(r2.downloaded.is_empty() && r2.edits_sent == 0 && r2.sidecars_updated.is_empty(), "{r2:?}");
    assert_eq!(fx.hooked.lock().unwrap().len(), 3);
}

#[test]
fn a_downloaded_photo_keeps_the_modification_time_of_the_manifest() {
    let mut fx = Fx::new("mtime", &["A"]);
    fx.publish(vec![ph("A/x.jpg")]);
    fx.sync();
    assert_eq!(mtime_of(&fx.src.join("A/x.jpg")), MTIME_EPOCH);
}

#[test]
fn a_file_whose_bytes_do_not_match_its_id_is_not_kept_and_the_next_pass_tries_again() {
    let mut fx = Fx::new("hash", &["A"]);
    let good = ph("A/x.jpg");
    fx.publish(vec![good.clone()]);
    fx.put("originals/A/x.jpg", b"corrupted on the way"); // (same size is not required: the hash decides)
    let r = fx.sync();
    assert!(!fx.has("A/x.jpg") && fx.hidden().is_empty(), "{:?}", fx.tree());
    assert!(r.problems.iter().any(|p| p.contains("A/x.jpg")), "{r:?}");
    assert_eq!((r.waiting_to_download, fx.hooked.lock().unwrap().len()), (1, 0));
    fx.put("originals/A/x.jpg", &good.bytes);
    let r = fx.sync();
    assert_eq!(fx.local("A/x.jpg"), good.bytes);
    assert_eq!((r.downloaded.len(), r.waiting_to_download), (1, 0));
}

#[test]
fn a_pass_that_was_cut_off_is_finished_by_the_next_without_fetching_again_what_arrived() {
    let mut fx = Fx::new("resume", &["A"]);
    let photos: Vec<Ph> = (1..=5).map(|i| ph(&format!("A/{i}.jpg"))).collect();
    fx.publish(photos);
    fx.net.fail_from.store(5, Ordering::SeqCst); // the manifest is get #1, three photos arrive, then the connection drops
    let err = fx.other.sync();
    assert!(matches!(err, Err(Error::Remote(_))), "{err:?}");
    let have = fx.tree().len();
    assert_eq!(have, 3, "{:?}", fx.tree());
    assert!(fx.hidden().is_empty(), "{:?}", fx.hidden());
    fx.net.fail_from.store(0, Ordering::SeqCst);
    fx.net.gets.store(0, Ordering::SeqCst);
    let r = fx.sync();
    assert_eq!(fx.tree().len(), 5);
    assert_eq!(r.downloaded.len(), 2);
    assert_eq!(fx.net.gets.load(Ordering::SeqCst), 3, "the manifest and the two missing photos, nothing else");
    assert_eq!(fx.hooked.lock().unwrap().len(), 5, "each photo was announced once");
}

#[test]
fn a_different_file_at_the_target_is_never_replaced_and_the_same_one_is_not_downloaded_again() {
    let mut fx = Fx::new("exists", &["A"]);
    fx.publish(vec![ph("A/mine.jpg"), ph("A/same.jpg")]);
    std::fs::create_dir_all(fx.src.join("A")).unwrap();
    std::fs::write(fx.src.join("A/mine.jpg"), b"my own file").unwrap();
    std::fs::write(fx.src.join("A/same.jpg"), photo_bytes("A/same.jpg")).unwrap();
    let r = fx.sync();
    assert_eq!(fx.local("A/mine.jpg"), b"my own file");
    assert!(r.problems.iter().any(|p| p.contains("A/mine.jpg")), "{r:?}");
    assert!(r.downloaded.is_empty());
    assert_eq!(r.photos, 1, "the identical file counts as received");
}

#[test]
fn a_newer_format_is_refused_before_anything_is_written() {
    let mut fx = Fx::new("format", &["A"]);
    fx.publish_with_format(vec![ph("A/x.jpg")], 2);
    let err = fx.other.sync().unwrap_err();
    assert!(matches!(err, Error::Format(_)) && err.to_string().contains("format 2"), "{err}");
    assert!(fx.tree().is_empty());
}

#[cfg(unix)]
#[test]
fn too_little_free_disk_space_is_a_clear_error_and_nothing_is_downloaded() {
    let mut fx = Fx::with("space", &["A"], |c| c.free_margin = u64::MAX / 2);
    fx.publish(vec![ph("A/x.jpg"), ph("A/y.jpg")]);
    let r = fx.sync();
    assert!(fx.tree().is_empty() && fx.hidden().is_empty());
    let msg = r.problems.join(" | ");
    assert!(msg.contains("free disk space") && msg.contains("2 more photos"), "{msg}");
    assert_eq!(r.waiting_to_download, 2);
}

#[test]
fn a_manifest_path_that_leaves_the_photo_folder_is_refused() {
    let mut fx = Fx::new("evil", &["A/../.."]);
    let mut bad = ph("A/x.jpg");
    bad.path = "A/../../escape.jpg".into();
    fx.publish(vec![bad]);
    let r = fx.sync();
    assert!(!fx.root.join("pc/escape.jpg").exists() && fx.tree().is_empty());
    assert!(r.downloaded.is_empty() && r.problems.iter().any(|p| p.contains("escape.jpg")), "{r:?}");
}

// ------------------------------------------------------------------------------------------------ 3: edits go to the inbox

#[test]
fn a_changed_local_sidecar_goes_to_the_inbox_once_with_the_version_it_started_from() {
    let mut fx = Fx::new("send", &["A"]);
    let p = ph("A/x.jpg").sc("A/x.xmp", xmp(1));
    fx.publish(vec![p.clone()]);
    fx.sync();
    assert!(fx.inbox().is_empty(), "downloading a sidecar is not an edit");
    fx.tick(60);
    fx.edit_locally("A/x.xmp", &xmp(2));
    let r = fx.sync();
    assert_eq!(r.edits_sent, 1);
    let files = fx.inbox();
    assert_eq!(files.len(), 1, "{files:?}");
    let name = files[0].strip_prefix("").unwrap();
    let parsed = parse_inbox_name(name).expect("a valid inbox name");
    assert_eq!((parsed.device.as_str(), parsed.id.as_str(), parsed.base.as_str()), (DEV, p.id().as_str(), sha16(&xmp(1)).as_str()));
    assert_eq!(parsed.stamp, "20261101T100100Z");
    assert_eq!(std::fs::read(fx.remote.join("inbox").join(name)).unwrap(), xmp(2));
    // not sent twice
    fx.tick(900);
    assert_eq!(fx.sync().edits_sent, 0);
    assert_eq!(fx.inbox().len(), 1);
    // the next edit starts from the version of the one before (the home PC keeps every inbox file), and its stamp is later
    fx.tick(60);
    fx.edit_locally("A/x.xmp", &xmp(3));
    fx.sync();
    let files = fx.inbox();
    assert_eq!(files.len(), 2);
    let second = parse_inbox_name(files.last().unwrap()).unwrap();
    assert_eq!(second.base, sha16(&xmp(2)));
    assert!(second.stamp > parsed.stamp);
}

#[test]
fn two_edits_in_the_same_second_get_different_stamps() {
    let mut fx = Fx::new("stamp", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1))]);
    fx.sync();
    fx.edit_locally("A/x.xmp", &xmp(2));
    fx.sync();
    fx.edit_locally("A/x.xmp", &xmp(3)); // (the clock has not moved)
    fx.sync();
    let names = fx.inbox();
    assert_eq!(names.len(), 2);
    assert_ne!(parse_inbox_name(&names[0]).unwrap().stamp, parse_inbox_name(&names[1]).unwrap().stamp);
}

#[test]
fn an_edit_of_a_photo_that_had_no_sidecar_is_sent_with_base_none() {
    let mut fx = Fx::new("none", &["A"]);
    fx.publish(vec![ph("A/x.jpg")]);
    fx.sync();
    fx.edit_locally("A/x.xmp", &xmp(1)); // (LightCraft writes a sidecar next to the photo)
    let r = fx.sync();
    assert_eq!(r.edits_sent, 1);
    assert_eq!(parse_inbox_name(&fx.inbox()[0]).unwrap().base, "none");
}

#[test]
fn an_edit_of_an_alternate_is_addressed_to_the_alternates_id() {
    let mut fx = Fx::new("alt", &["A"]);
    let alt = ph("A/B.JPG").sc("A/B.JPG.xmp", xmp(3));
    fx.publish(vec![ph("A/B.ARW").sc("A/B.xmp", xmp(2)).alt(alt.clone())]);
    fx.sync();
    fx.edit_locally("A/B.JPG.xmp", &xmp(4));
    fx.sync();
    let names = fx.inbox();
    assert_eq!(names.len(), 1);
    let n = parse_inbox_name(&names[0]).unwrap();
    assert_eq!((n.id, n.base), (alt.id(), sha16(&xmp(3))));
}

#[test]
fn an_edit_that_could_not_be_sent_stays_waiting_and_goes_when_the_network_is_back() {
    let mut fx = Fx::new("offline-send", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1))]);
    fx.sync();
    fx.edit_locally("A/x.xmp", &xmp(2));
    fx.net.offline.store(true, Ordering::SeqCst);
    assert!(matches!(fx.other.sync(), Err(Error::Remote(_))));
    assert!(fx.inbox().is_empty());
    fx.net.offline.store(false, Ordering::SeqCst);
    let r = fx.sync();
    assert_eq!((r.edits_sent, fx.inbox().len()), (1, 1));
}

// ------------------------------------------------------------------------------------------------ 4: the home PC's version comes back

/// Windows: a sidecar another program holds (readable, but not replaceable) is shown as a problem each pass, adds nothing to the history,
/// and is taken once it is let go.
#[cfg(windows)]
#[test]
fn a_held_sidecar_is_reported_keeps_no_extra_history_and_is_taken_when_released() {
    use std::os::windows::fs::OpenOptionsExt;
    let mut fx = Fx::new("held-sidecar", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1))]);
    fx.sync();
    fx.tick(3600);
    fx.home_edits("A/x.jpg", xmp(2));
    let held = std::fs::OpenOptions::new().read(true).share_mode(1).open(fx.src.join("A/x.xmp")).unwrap(); // (FILE_SHARE_READ only)
    for _ in 0..2 {
        fx.tick(3600);
        let r = fx.sync();
        assert!(r.sidecars_updated.is_empty() && r.problems.iter().any(|p| p.contains("A/x.xmp")), "{r:?}");
        assert!(fx.state_tree("history").is_empty());
    }
    drop(held);
    let r = fx.sync();
    assert_eq!(r.sidecars_updated, ["A/x.xmp"]);
    assert!(r.problems.is_empty(), "{r:?}");
    assert_eq!(fx.local("A/x.xmp"), xmp(2));
    assert_eq!(fx.state_tree("history").len(), 1);
}

#[test]
fn a_newer_home_sidecar_replaces_an_unchanged_local_one_and_the_old_one_is_kept_in_history() {
    let mut fx = Fx::new("pull", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1))]);
    fx.sync();
    fx.tick(3600);
    fx.home_edits("A/x.jpg", xmp(2));
    let r = fx.sync();
    assert_eq!(fx.local("A/x.xmp"), xmp(2));
    assert_eq!(r.sidecars_updated, ["A/x.xmp"]);
    assert!(fx.hidden().is_empty());
    let hist = fx.state_tree("history");
    assert_eq!(hist.len(), 1, "{hist:?}");
    assert!(hist[0].starts_with("2026-11-01/A/x.") && hist[0].ends_with(".xmp"), "{hist:?}");
    assert_eq!(std::fs::read(fx.state.join("history").join(&hist[0])).unwrap(), xmp(1));
    assert!(fx.inbox().is_empty(), "what came from the home PC is not sent back");
    assert_eq!(fx.sync().sidecars_updated.len(), 0);
}

#[test]
fn a_sidecar_the_home_pc_made_for_a_photo_without_one_is_downloaded() {
    let mut fx = Fx::new("pull-new", &["A"]);
    fx.publish(vec![ph("A/x.jpg")]);
    fx.sync();
    fx.home_edits("A/x.jpg", xmp(2));
    fx.sync();
    assert_eq!(fx.local("A/x.jpg.xmp"), xmp(2));
    assert!(fx.state_tree("history").is_empty());
}

#[test]
fn when_both_changed_the_local_edit_goes_to_the_inbox_first_and_is_not_overwritten() {
    let mut fx = Fx::new("both", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1))]);
    fx.sync();
    fx.tick(60);
    fx.edit_locally("A/x.xmp", &xmp(2)); // on this PC
    fx.home_edits("A/x.jpg", xmp(3)); // at home, meanwhile
    let r = fx.sync();
    assert_eq!(r.edits_sent, 1);
    assert_eq!(fx.local("A/x.xmp"), xmp(2), "the local edit is still there");
    assert!(r.sidecars_updated.is_empty());
    let sent = parse_inbox_name(&fx.inbox()[0]).unwrap();
    assert_eq!(sent.base, sha16(&xmp(1)), "it started from the version this PC had downloaded");
    assert_eq!(std::fs::read(fx.remote.join("inbox").join(&fx.inbox()[0])).unwrap(), xmp(2));
}

#[test]
fn until_the_home_pc_has_merged_the_manifest_is_unchanged_and_the_local_edit_stays_put() {
    let mut fx = Fx::new("ordering", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1))]);
    fx.sync();
    fx.edit_locally("A/x.xmp", &xmp(2));
    fx.home_edits("A/x.jpg", xmp(3));
    fx.sync(); // sends xmp(2); the home PC has not run
    for _ in 0..3 {
        fx.tick(900);
        let r = fx.sync(); // 15 minutes later, again and again: the manifest still says xmp(3)
        assert_eq!((r.edits_sent, r.sidecars_updated.len(), r.held_back), (0, 0, 1), "{r:?}");
        assert_eq!(fx.local("A/x.xmp"), xmp(2));
    }
    assert_eq!(fx.inbox().len(), 1, "sent once");
    assert!(fx.state_tree("history").is_empty());
    // the home PC merges (its result is a third version) and republishes: now it comes back
    fx.home_edits("A/x.jpg", xmp(4));
    let r = fx.sync();
    assert_eq!(r.sidecars_updated, ["A/x.xmp"]);
    assert_eq!(fx.local("A/x.xmp"), xmp(4));
    let hist = fx.state_tree("history");
    assert_eq!(hist.len(), 1);
    assert_eq!(std::fs::read(fx.state.join("history").join(&hist[0])).unwrap(), xmp(2), "the local edit is in history, never lost");
    assert_eq!(fx.inbox().len(), 1, "the merge result is not sent back");
    assert_eq!(fx.sync().held_back, 0);
}

#[test]
fn an_edit_made_while_waiting_for_the_merge_is_sent_too_and_nothing_local_is_replaced() {
    let mut fx = Fx::new("again", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1))]);
    fx.sync();
    fx.edit_locally("A/x.xmp", &xmp(2));
    fx.home_edits("A/x.jpg", xmp(3));
    fx.sync();
    fx.tick(60);
    fx.edit_locally("A/x.xmp", &xmp(5)); // edited again before the home PC merged anything
    let r = fx.sync();
    assert_eq!(r.edits_sent, 1);
    assert_eq!(fx.local("A/x.xmp"), xmp(5));
    let names = fx.inbox();
    assert_eq!(names.len(), 2);
    assert_eq!(parse_inbox_name(&names[1]).unwrap().base, sha16(&xmp(2)));
}

#[test]
fn when_the_home_pc_applied_the_edit_as_it_was_the_local_file_is_simply_in_step() {
    let mut fx = Fx::new("applied", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1))]);
    fx.sync();
    fx.edit_locally("A/x.xmp", &xmp(2));
    fx.sync();
    fx.home_edits("A/x.jpg", xmp(2)); // the home PC took the edit unchanged
    let r = fx.sync();
    assert!(r.sidecars_updated.is_empty() && r.edits_sent == 0 && r.held_back == 0, "{r:?}");
    assert_eq!(fx.local("A/x.xmp"), xmp(2));
    assert!(fx.state_tree("history").is_empty());
    // and a later home change is pulled as usual
    fx.home_edits("A/x.jpg", xmp(7));
    assert_eq!(fx.sync().sidecars_updated.len(), 1);
}

// ------------------------------------------------------------------------------------------------ 5: the home PC deleted it

#[test]
fn a_photo_the_home_pc_deleted_is_moved_aside_with_its_sidecar_never_deleted() {
    let mut fx = Fx::new("removed", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1)), ph("A/y.jpg")]);
    fx.sync();
    fx.tick(86_400);
    fx.home_deletes("A/x.jpg");
    let r = fx.sync();
    assert_eq!(fx.tree(), ["A/y.jpg"]);
    assert_eq!(r.removed, ["A/x.jpg"]);
    assert_eq!(fx.state_tree("removed"), ["2026-11-02/A/x.jpg", "2026-11-02/A/x.xmp"]);
    assert_eq!(std::fs::read(fx.state.join("removed/2026-11-02/A/x.jpg")).unwrap(), photo_bytes("A/x.jpg"));
    assert_eq!(r.photos, 1);
    assert!(fx.sync().removed.is_empty());
}

#[test]
fn a_photo_whose_bytes_changed_on_this_pc_is_left_where_it_is() {
    let mut fx = Fx::new("modified", &["A"]);
    fx.publish(vec![ph("A/x.jpg"), ph("A/y.jpg")]);
    fx.sync();
    std::fs::write(fx.src.join("A/x.jpg"), b"retouched in another program").unwrap();
    fx.home_deletes("A/x.jpg");
    let r = fx.sync();
    assert!(r.removed.is_empty());
    assert_eq!(fx.local("A/x.jpg"), b"retouched in another program");
    assert!(fx.state_tree("removed").is_empty());
    assert!(r.problems.iter().any(|p| p.contains("A/x.jpg")), "{r:?}");
}

#[test]
fn more_removals_than_max_delete_are_refused_and_nothing_moves() {
    let mut fx = Fx::with("maxdel", &["A"], |c| c.max_delete = 2);
    let photos: Vec<Ph> = (1..=5).map(|i| ph(&format!("A/{i}.jpg"))).collect();
    fx.publish(photos);
    fx.sync();
    fx.publish(vec![]); // (a broken or emptied manifest looks like this)
    let r = fx.sync();
    assert_eq!(fx.tree().len(), 5);
    assert!(r.removed.is_empty() && fx.state_tree("removed").is_empty());
    assert!(r.problems.iter().any(|p| p.contains("5 photos") && p.contains("max_delete")), "{r:?}");
    // two at a time is within the limit
    fx.publish((3..=5).map(|i| ph(&format!("A/{i}.jpg"))).collect());
    let r = fx.sync();
    assert_eq!(r.removed.len(), 2);
}

// ------------------------------------------------------------------------------------------------ 6: deleted here

#[test]
fn a_photo_deleted_on_this_pc_is_not_downloaded_again_by_itself() {
    let mut fx = Fx::new("deleted-here", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1)), ph("A/y.jpg")]);
    fx.sync();
    std::fs::remove_file(fx.src.join("A/x.jpg")).unwrap();
    std::fs::remove_file(fx.src.join("A/x.xmp")).unwrap();
    fx.tick(900);
    let r = fx.sync();
    assert_eq!(fx.tree(), ["A/y.jpg"]);
    assert_eq!((r.left_out, r.waiting_to_download, r.downloaded.len()), (1, 0, 0), "{r:?}");
    assert!(fx.logged("A/x.jpg was deleted on this PC"));
    // not after the home PC edits it either, nor on any later pass
    fx.home_edits("A/x.jpg", xmp(2));
    fx.sync();
    fx.sync();
    assert_eq!(fx.tree(), ["A/y.jpg"]);
    // and when the home PC deletes it too, the memory of it goes
    fx.home_deletes("A/x.jpg");
    fx.sync();
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1)), ph("A/y.jpg")]); // (a new photo with the same bytes, much later)
    fx.sync();
    assert!(fx.has("A/x.jpg"), "a photo that left the cloud and came back is a new photo");
}

#[test]
fn choosing_the_folder_again_brings_a_deleted_photo_back() {
    let mut fx = Fx::new("rechoose", &["A"]);
    fx.publish(vec![ph("A/x.jpg"), ph("A/y.jpg")]);
    fx.sync();
    std::fs::remove_file(fx.src.join("A/x.jpg")).unwrap();
    fx.sync();
    assert!(!fx.has("A/x.jpg"));
    // un-tick the folder (a pass with no folders), then tick it again
    let mut cfg = OtherConfig::new(&fx.src, &fx.state, DEV, vec![]);
    cfg.free_margin = 0;
    let off = Other::new(cfg, fx.net.clone()).unwrap();
    off.sync().unwrap();
    assert!(fx.has("A/y.jpg"), "un-ticking a folder leaves its files alone");
    let mut cfg = OtherConfig::new(&fx.src, &fx.state, DEV, vec!["A".into()]);
    cfg.free_margin = 0;
    let on = Other::new(cfg, fx.net.clone()).unwrap();
    let r = on.sync().unwrap();
    assert!(fx.has("A/x.jpg"), "{r:?}");
    assert_eq!(r.downloaded, ["A/x.jpg"]);
}

#[test]
fn an_unticked_folder_is_no_longer_managed_so_the_home_pc_deleting_a_photo_there_moves_nothing() {
    let mut fx = Fx::new("untick", &["A", "B"]);
    fx.publish(vec![ph("A/x.jpg"), ph("B/y.jpg")]);
    fx.sync();
    let mut cfg = OtherConfig::new(&fx.src, &fx.state, DEV, vec!["A".into()]);
    cfg.free_margin = 0;
    let only_a = Other::new(cfg, fx.net.clone()).unwrap();
    only_a.sync().unwrap();
    fx.home_deletes("B/y.jpg");
    let r = only_a.sync().unwrap();
    assert!(fx.has("B/y.jpg") && r.removed.is_empty());
}

// ------------------------------------------------------------------------------------------------ 7: telling LightCraft

#[test]
fn a_photo_lightcraft_could_not_be_told_about_is_announced_by_the_next_pass_not_downloaded_twice() {
    let mut fx = Fx::new("hook", &["A"]);
    fx.publish(vec![ph("A/x.jpg")]);
    fx.hook_fails.store(true, Ordering::SeqCst);
    fx.sync();
    assert!(fx.has("A/x.jpg"));
    assert!(fx.hooked.lock().unwrap().is_empty());
    fx.hook_fails.store(false, Ordering::SeqCst);
    fx.net.gets.store(0, Ordering::SeqCst);
    fx.sync();
    assert_eq!(*fx.hooked.lock().unwrap(), [fx.src.join("A/x.jpg")]);
    assert_eq!(fx.net.gets.load(Ordering::SeqCst), 1, "only the manifest");
    fx.sync();
    assert_eq!(fx.hooked.lock().unwrap().len(), 1);
}

#[test]
fn a_replaced_sidecar_is_not_announced_to_lightcraft() {
    let mut fx = Fx::new("no-hook-for-sidecars", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1))]);
    fx.sync();
    fx.home_edits("A/x.jpg", xmp(2));
    fx.sync();
    assert_eq!(fx.hooked.lock().unwrap().len(), 1, "the photo only");
}

// ------------------------------------------------------------------------------------------------ the rest of the contract

#[test]
fn offline_is_a_remote_error_that_changes_nothing() {
    let mut fx = Fx::new("offline", &["A"]);
    fx.publish(vec![ph("A/x.jpg")]);
    fx.net.offline.store(true, Ordering::SeqCst);
    let e = fx.other.sync().unwrap_err();
    assert!(matches!(e, Error::Remote(_)), "{e}");
    assert!(fx.tree().is_empty());
    fx.net.offline.store(false, Ordering::SeqCst);
    assert_eq!(fx.sync().downloaded.len(), 1);
}

#[test]
fn no_manifest_yet_is_not_found() {
    let fx = Fx::new("nomanifest", &["A"]);
    let e = fx.other.sync().unwrap_err();
    assert!(matches!(e, Error::NotFound(_)), "{e}");
}

#[test]
fn a_device_name_another_device_already_sends_edits_under_is_refused_on_the_first_pass() {
    let mut fx = Fx::new("clash", &["A"]);
    fx.publish(vec![ph("A/x.jpg")]);
    fx.put(&format!("inbox/{DEV}/{}-20261031T000000Z-none.xmp", "0".repeat(32)), &xmp(1));
    let e = fx.other.sync().unwrap_err();
    assert!(matches!(e, Error::Invalid(_)) && e.to_string().contains(DEV), "{e}");
    assert!(fx.tree().is_empty());
}

#[test]
fn a_device_name_this_pc_already_used_is_not_a_clash() {
    let mut fx = Fx::new("noclash", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1))]);
    fx.sync();
    fx.edit_locally("A/x.xmp", &xmp(2));
    fx.sync(); // (this created inbox/laptop)
    fx.sync();
}

#[test]
fn the_photo_folder_must_exist_and_the_device_name_must_be_valid() {
    let fx = Fx::new("badcfg", &["A"]);
    let missing = OtherConfig::new(fx.root.join("nope"), &fx.state, DEV, vec!["A".into()]);
    let o = Other::new(missing, fx.net.clone()).unwrap();
    assert!(matches!(o.sync(), Err(Error::Invalid(m)) if m.contains("does not exist")));
    let bad = OtherConfig::new(&fx.src, &fx.state, "my laptop!", vec![]);
    assert!(matches!(Other::new(bad, fx.net.clone()), Err(Error::Invalid(_))));
}

#[test]
fn the_status_numbers_come_from_the_state_without_the_network() {
    let mut fx = Fx::new("status", &["A"]);
    fx.publish(vec![ph("A/x.jpg").sc("A/x.xmp", xmp(1)), ph("A/y.jpg")]);
    fx.sync();
    fx.edit_locally("A/x.xmp", &xmp(2));
    fx.net.offline.store(true, Ordering::SeqCst);
    let s = fx.other.status().unwrap();
    assert_eq!((s.photos, s.waiting_to_download, s.edits_waiting), (2, 0, 1), "{s:?}");
}

#[test]
fn the_folders_to_choose_from_are_read_from_the_manifest_with_sizes_and_photo_counts() {
    let mut fx = Fx::new("folders", &[]);
    fx.publish(vec![
        ph("Lisbon/A.jpg"),
        ph("Lisbon/B.ARW").alt(ph("Lisbon/B.JPG")),
        ph("Porto/C.jpg"),
        ph("loose.jpg"),
    ]);
    let f = list_folders(&*fx.net).unwrap();
    let got: Vec<(String, usize, u64)> = f.iter().map(|f| (f.folder.clone(), f.photos, f.bytes)).collect();
    let n = |p: &str| photo_bytes(p).len() as u64;
    assert_eq!(
        got,
        [
            (String::new(), 1, n("loose.jpg")),
            ("Lisbon".into(), 2, n("Lisbon/A.jpg") + n("Lisbon/B.ARW") + n("Lisbon/B.JPG")),
            ("Porto".into(), 1, n("Porto/C.jpg")),
        ]
    );
    let _ = sha_hex(b"");
}

// ------------------------------------------------------------------------------------------------ live progress

#[test]
fn a_progress_hook_sees_the_pass_stage_by_stage_and_the_download_ends_with_the_full_count_and_size() {
    use lightcraft_cloud_client::{Progress, Stage};
    let mut fx = Fx::new("progress", &["Lisbon"]);
    fx.publish(vec![ph("Lisbon/A.jpg").sc("Lisbon/A.xmp", xmp(1)), ph("Lisbon/B.jpg"), ph("Lisbon/C.jpg"), ph("Porto/D.jpg")]);
    let seen: Arc<Mutex<Vec<Progress>>> = Arc::default();
    let sink = seen.clone();
    fx.other.set_progress_hook(move |p| sink.lock().unwrap().push(*p));
    fx.sync();
    let all = seen.lock().unwrap().clone();
    let mut stages: Vec<Progress> = Vec::new();
    for p in &all {
        match stages.last_mut() {
            Some(l) if l.stage == p.stage && l.started_ms == p.started_ms => *l = *p,
            _ => stages.push(*p),
        }
    }
    let names: Vec<Stage> = stages.iter().map(|p| p.stage).collect();
    assert_eq!(names, [Stage::LookingAtLibrary, Stage::CheckingEdits, Stage::Downloading, Stage::LookingForNewPhotos], "{stages:?}");
    let sizes: u64 = ["Lisbon/A.jpg", "Lisbon/B.jpg", "Lisbon/C.jpg"].iter().map(|p| photo_bytes(p).len() as u64).sum();
    let dl = stages[2];
    assert_eq!((dl.done_items, dl.total_items, dl.done_bytes, dl.total_bytes), (3, 3, sizes, sizes));
    assert_eq!((stages[1].done_items, stages[1].total_items), (3, 3));
    // the stamps are the pass's own clock (the fixture's), not the real time
    assert_eq!(dl.started_ms, 1_793_527_200_000);
    // a second pass has nothing to download: no download stage to announce
    seen.lock().unwrap().clear();
    fx.sync();
    assert!(!seen.lock().unwrap().iter().any(|p| p.stage == Stage::Downloading));
}

/// A remote whose `get` takes a few steps and says how far it is, like rclone's `core/stats`.
struct Slow {
    inner: DirRemote,
    moved: std::sync::atomic::AtomicU64,
}

impl Remote for Slow {
    fn list(&self, dir: &str, recursive: bool) -> Result<Vec<RemoteEntry>, Error> {
        self.inner.list(dir, recursive)
    }
    fn get(&self, path: &str, local: &Path) -> Result<(), Error> {
        if path.ends_with(".jpg") {
            for _ in 0..12 {
                self.moved.fetch_add(50, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let r = self.inner.get(path, local);
        self.moved.store(0, Ordering::SeqCst);
        r
    }
    fn put(&self, local: &Path, path: &str) -> Result<(), Error> {
        self.inner.put(local, path)
    }
    fn bytes_in_flight(&self) -> Option<u64> {
        Some(self.moved.load(Ordering::SeqCst))
    }
}

#[test]
fn bytes_moving_inside_a_big_file_are_reported_and_count_as_activity() {
    use lightcraft_cloud_client::Progress;
    let mut fx = Fx::new("inflight", &["A"]);
    fx.publish(vec![ph("A/big.jpg")]);
    let slow = Arc::new(Slow { inner: DirRemote::new(&fx.remote), moved: Default::default() });
    let mut o = Other::new(fx.other.config().clone(), slow).unwrap();
    o.set_retry_delay(Duration::ZERO);
    let seen: Arc<Mutex<Vec<Progress>>> = Arc::default();
    let sink = seen.clone();
    // (the real clock here: the fixture's stands still, and activity is what moves last_item_ms)
    o.set_progress_hook(move |p| sink.lock().unwrap().push(*p));
    o.sync().unwrap();
    let all = seen.lock().unwrap().clone();
    let during: Vec<&Progress> = all.iter().filter(|p| p.partial_bytes > 0).collect();
    assert!(during.len() >= 2, "the poller saw the file move: {all:?}");
    assert!(during.windows(2).any(|w| w[0].partial_bytes < w[1].partial_bytes));
    assert!(during.iter().all(|p| p.last_item_ms >= p.started_ms && p.last_item_ms > 0), "moving bytes are activity");
    let fraction = during.iter().filter_map(|p| p.fraction()).fold(0.0f32, f32::max);
    assert!(fraction > 0.0, "the bar moves inside the file");
}
