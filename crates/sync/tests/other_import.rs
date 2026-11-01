//! Photos imported on another PC (from a card or a folder) and sent to the home PC: the scan, the copy, the upload queue, and what
//! happens once the home PC has answered. The home side is the real `Home` with a renderer that knows the real content hash; the cloud
//! is a folder.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Fx, HashRenderer, Spy, sc, sha16, sha_hex};
use lightcraft_cloud_client::home::{check_import_note, content_hash_bytes};
use lightcraft_cloud_client::other::{ImportQueue, ImportScan, Other, OtherConfig, Progress, UploadBudget, check_destination};
use lightcraft_cloud_client::{Error, Remote};
use serde_json::{Value, json};

const DEST: &str = "261101 - Lisbon";

fn photo(tag: &str) -> Vec<u8> {
    format!("the bytes of {tag} ").repeat(80).into_bytes()
}

fn xmp(exposure: f64) -> Vec<u8> {
    sc().s(&[("light.exposure", json!(exposure))]).build()
}

struct W {
    fx: Fx,
    spy: Arc<Spy>,
    other: Other,
    laptop: PathBuf,
    card: PathBuf,
    told: Arc<Mutex<Vec<PathBuf>>>,
    cancel: AtomicBool,
}

impl W {
    fn new(tag: &str) -> W {
        W::with(tag, |_| {})
    }

    fn with(tag: &str, f: impl FnOnce(&mut OtherConfig)) -> W {
        let fx = Fx::new(tag);
        let laptop = fx.root.join("laptop");
        let card = fx.root.join("card");
        std::fs::create_dir_all(laptop.join("photos")).unwrap();
        std::fs::create_dir_all(&card).unwrap();
        let mut cfg = OtherConfig::new(laptop.join("photos"), laptop.join("state"), "laptop", vec![DEST.into()]);
        cfg.free_margin = 0;
        f(&mut cfg);
        let spy = Spy::new(&fx.remote);
        let remote: Arc<dyn Remote> = spy.clone();
        let other = Other::new(cfg, remote).unwrap();
        let mut w = W { fx, spy, other, laptop, card, told: Arc::default(), cancel: AtomicBool::new(false) };
        w.restart(w.cfg());
        w
    }

    /// A new `Other` over the same folders: the app was restarted.
    fn restart(&mut self, cfg: OtherConfig) {
        let remote: Arc<dyn Remote> = self.spy.clone();
        let mut o = Other::new(cfg, remote).unwrap();
        o.set_retry_delay(Duration::ZERO);
        let told = self.told.clone();
        o.set_adopted_hook(move |p| {
            told.lock().unwrap().push(p.to_path_buf());
            Ok(())
        });
        self.other = o;
    }

    fn cfg(&self) -> OtherConfig {
        self.other.config().clone()
    }

    fn card(&self, rel: &str, data: &[u8]) {
        let p = self.card.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    fn scan(&self, dest: &str) -> ImportScan {
        self.other.import_scan(&self.card, dest, &mut |_| {}, &self.cancel).unwrap_or_else(|e| panic!("scan: {e}"))
    }

    fn import(&self) -> (ImportScan, lightcraft_cloud_client::other::ImportRun) {
        let s = self.scan(DEST);
        let r = self.other.import_run(&s, &mut |_| {}, &self.cancel).unwrap_or_else(|e| panic!("import: {e}"));
        (s, r)
    }

    fn photos(&self) -> PathBuf {
        self.laptop.join("photos")
    }

    fn local(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.photos().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn tree(&self, dir: &Path) -> Vec<String> {
        self.fx.tree(dir).into_iter().map(|p| p.replace('\\', "/")).filter(|p| !p.split('/').any(|c| c.starts_with('.'))).collect()
    }

    fn queue(&self) -> ImportQueue {
        ImportQueue::new(&self.laptop.join("state"))
    }

    fn upload_all(&self) -> usize {
        let mut sent = 0;
        for _ in 0..50 {
            let r = self.other.upload_step(&UploadBudget::default()).unwrap_or_else(|e| panic!("upload: {e}"));
            sent += r.committed;
            if !r.more {
                break;
            }
        }
        sent
    }

    fn home_sync(&self) {
        self.fx.home.sync(&mut HashRenderer::default(), false).unwrap_or_else(|e| panic!("home sync: {e}"));
    }

    fn pass(&self) -> lightcraft_cloud_client::other::OtherReport {
        self.other.sync().unwrap_or_else(|e| panic!("pass: {e}"))
    }

    fn seed_home(&self) {
        self.fx.touch("Seed/seed.jpg", b"the photo that was here before", 3600);
    }
}

fn tree_hash(w: &W, dir: &Path) -> String {
    let mut h = String::new();
    for p in w.fx.tree(dir) {
        h += &format!("{p}:{};", sha_hex(&std::fs::read(dir.join(&p)).unwrap()));
    }
    h
}

// ------------------------------------------------------------------ the destination

#[test]
fn the_window_refuses_a_destination_the_home_pc_would_reject_by_the_same_code() {
    let exclude = vec!["exported".to_string(), "*.tmp".to_string()];
    let note = |folder: &str| {
        json!({"format": 1, "id": content_hash_bytes(b"x"), "folder": folder, "name": "a.jpg", "size": 1, "mtime": "2024-05-01T12:00:00Z", "sidecar": null}).to_string()
    };
    let id = content_hash_bytes(b"x");
    let cases = [
        "261101 - Lisbon", "2026/261101 - Lisbon", "exported", "a/exported/b", "x.tmp", "..", "a/../b", "a//b", ".hidden", "a/.hidden", "CON", "a/nul.txt", "ends.", "has:colon",
        "back\\slash", "star*", "/abs", "a/b/", "über mähren", "ok.with.dots",
    ];
    for dest in cases {
        let ours = check_destination(dest, &exclude);
        // (the window trims one trailing slash and the spaces round the name; the note must say the same folder)
        let shown = dest.trim().trim_end_matches('/').trim();
        let theirs = check_import_note(note(shown).as_bytes(), &id, &exclude);
        assert_eq!(ours.is_ok(), theirs.is_ok() && !shown.is_empty(), "{dest:?}: window {ours:?}, home {theirs:?}");
    }
    assert_eq!(check_destination("  261101 - Lisbon/ ", &exclude).unwrap(), "261101 - Lisbon");
    assert!(check_destination("", &exclude).unwrap_err().contains("Type a name"));
    assert!(check_destination("exported", &exclude).unwrap_err().contains("exclude"));
}

#[test]
fn a_destination_the_home_pc_excludes_is_refused_by_the_scan_before_anything_is_read() {
    let w = W::with("excl", |c| c.exclude = vec!["private".into()]);
    w.card("a.jpg", &photo("a"));
    let e = w.other.import_scan(&w.card, "private", &mut |_| panic!("nothing is read"), &w.cancel).unwrap_err();
    assert!(matches!(e, Error::Invalid(m) if m.contains("exclude")));
    assert!(w.other.import_scan(&w.card, "..", &mut |_| {}, &w.cancel).is_err());
}

// ------------------------------------------------------------------ the scan

#[test]
fn the_scan_counts_new_photos_and_those_already_known_and_brings_sidecars_along() {
    let w = W::new("scan");
    w.seed_home();
    let known_in_library = photo("home has it");
    w.fx.touch("Elsewhere/L.jpg", &known_in_library, 3600);
    w.home_sync();
    // cards: DCIM/100/ with raw+jpeg pairs, a sidecar, junk, a hidden file, a twin, a bad name, a bad sidecar
    w.card("DCIM/100/DSC0001.ARW", &photo("one"));
    w.card("DCIM/100/DSC0001.xmp", &xmp(0.3));
    w.card("DCIM/100/DSC0002.JPG", &photo("two"));
    w.card("DCIM/101/DSC0003.jpg", &photo("three"));
    w.card("DCIM/101/DSC0003.jpg.xmp", b"this is not xmp");
    w.card("DCIM/101/L.jpg", &known_in_library);
    w.card("DCIM/101/twin-of-two.jpg", &photo("two"));
    w.card("DCIM/101/notes.txt", b"not a photo");
    w.card("DCIM/101/.hidden.jpg", &photo("hidden"));
    w.card(".Trashes/x.jpg", &photo("trash"));
    w.card("DCIM/101/bad:name.jpg", &photo("bad"));
    w.card("DCIM/101/empty.jpg", b"");
    // a photo this PC already has (its queue)
    let mine = photo("mine");
    w.card("DCIM/102/mine.jpg", &mine);
    std::fs::create_dir_all(w.laptop.join("state/imports-out")).unwrap();
    let e = lightcraft_cloud_client::other::OutEntry { id: content_hash_bytes(&mine), folder: DEST.into(), name: "m.jpg".into(), size: mine.len() as u64, ..Default::default() };
    w.queue().add(&e, None).unwrap();

    let mut seen = Vec::new();
    let s = w.other.import_scan(&w.card, DEST, &mut |p: &Progress| seen.push(p.clone()), &w.cancel).unwrap();
    let names: Vec<&str> = s.new.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["DSC0001.ARW", "DSC0002.JPG", "DSC0003.jpg"], "{s:?}");
    assert_eq!((s.in_library, s.on_this_pc, s.twins), (1, 1, 1), "{s:?}");
    assert!(s.library_checked && !s.cancelled);
    assert_eq!(s.new[0].sidecar.as_ref().map(|c| c.name.as_str()), Some("DSC0001.xmp"));
    assert!(s.new[2].sidecar.is_none(), "an unacceptable sidecar is not brought along");
    assert!(s.skipped.iter().any(|m| m.contains("DSC0003.jpg.xmp") && m.contains("not used")), "{:?}", s.skipped);
    assert!(s.skipped.iter().any(|m| m.contains("bad:name.jpg")), "{:?}", s.skipped);
    assert!(s.skipped.iter().any(|m| m.contains("empty.jpg")), "{:?}", s.skipped);
    assert!(!s.skipped.iter().any(|m| m.contains("hidden") || m.contains("trash")), "hidden files and folders are not even mentioned");
    let want: u64 = [photo("one"), photo("two"), photo("three")].iter().map(|b| b.len() as u64).sum::<u64>() + xmp(0.3).len() as u64;
    assert_eq!(s.new_bytes, want);
    assert!(s.free.is_some_and(|f| f > 0) || cfg!(not(unix)));
    // progress: the hashing is reported with bytes
    let last = seen.iter().rev().find(|p| p.phase == lightcraft_cloud_client::other::Phase::Hashing).unwrap();
    assert_eq!(last.bytes_done, last.bytes_total.min(last.bytes_done));
    assert!(last.bytes_done > 0 && last.total >= 5);
    // nothing was written anywhere but the state folder's own scratch
    assert!(!w.photos().join(DEST).exists(), "a scan creates no folder");
}

#[test]
fn a_scan_can_be_cancelled_and_says_so() {
    let w = W::new("cancel");
    for i in 0..6 {
        w.card(&format!("p{i}.jpg"), &photo(&format!("c{i}")));
    }
    let n = std::cell::Cell::new(0);
    let cancel = AtomicBool::new(false);
    let s = w
        .other
        .import_scan(
            &w.card,
            DEST,
            &mut |p: &Progress| {
                if p.phase == lightcraft_cloud_client::other::Phase::Hashing {
                    n.set(n.get() + 1);
                    if n.get() > 2 {
                        cancel.store(true, Ordering::SeqCst);
                    }
                }
            },
            &cancel,
        )
        .unwrap();
    assert!(s.cancelled);
    assert!(s.new.len() < 6);
}

#[test]
fn the_library_being_out_of_reach_is_not_an_error_the_records_of_this_pc_are_still_checked() {
    let w = W::new("offline-scan");
    w.seed_home();
    w.home_sync();
    w.card("a.jpg", &photo("a"));
    w.spy.fail_list_of.lock().unwrap().replace(String::new());
    // (the manifest is read with `get`)
    w.spy.fail_gets.store(1000, Ordering::SeqCst);
    let s = w.scan(DEST);
    assert!(!s.library_checked);
    assert_eq!(s.new.len(), 1);
}

// ------------------------------------------------------------------ the copy

#[test]
fn photos_are_copied_beside_their_sidecars_the_source_is_untouched_and_lightcraft_is_told() {
    let w = W::new("copy");
    w.card("DCIM/100/DSC0001.ARW", &photo("one"));
    w.card("DCIM/100/DSC0001.xmp", &xmp(0.3));
    w.card("DCIM/101/DSC0002.jpg", &photo("two"));
    std::fs::File::open(w.card.join("DCIM/101/DSC0002.jpg")).unwrap().set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_714_564_800)).ok();
    let f = std::fs::OpenOptions::new().write(true).open(w.card.join("DCIM/101/DSC0002.jpg")).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_714_564_800)).unwrap();
    drop(f);
    let before = tree_hash(&w, &w.card);
    let (_, r) = w.import();
    assert_eq!((r.copied, r.failed.len(), r.cancelled), (2, 0, false), "{r:?}");
    assert_eq!(w.tree(&w.photos()), [format!("{DEST}/DSC0001.ARW"), format!("{DEST}/DSC0001.xmp"), format!("{DEST}/DSC0002.jpg")]);
    assert_eq!(w.local(&format!("{DEST}/DSC0001.ARW")), photo("one"));
    assert_eq!(w.local(&format!("{DEST}/DSC0001.xmp")), xmp(0.3));
    assert_eq!(tree_hash(&w, &w.card), before, "the source is never modified");
    let mt = std::fs::metadata(w.photos().join(DEST).join("DSC0002.jpg")).unwrap().modified().unwrap();
    assert_eq!(mt, std::time::UNIX_EPOCH + Duration::from_secs(1_714_564_800), "the time of the photo is kept");
    // nothing hidden is left beside them
    assert!(w.fx.tree(&w.photos().join(DEST)).iter().all(|p| !p.starts_with('.')), "{:?}", w.fx.tree(&w.photos().join(DEST)));
    // queued, once, with the note's fields
    let q = w.queue().list();
    assert_eq!(q.len(), 2);
    let one = q.iter().find(|e| e.name == "DSC0001.ARW").unwrap();
    assert_eq!((one.folder.as_str(), one.sidecar.as_deref(), one.size), (DEST, Some("DSC0001.xmp"), photo("one").len() as u64));
    assert_eq!(one.sidecar_sha.as_deref(), Some(sha_hex(&xmp(0.3)).as_str()));
    assert_eq!(one.id, content_hash_bytes(&photo("one")));
    assert_eq!(w.queue().snapshot(&one.id), Some(xmp(0.3)));
    assert_eq!(w.told.lock().unwrap().len(), 2);
    assert!(w.told.lock().unwrap().iter().all(|p| p.is_absolute() && p.starts_with(w.photos())));
    assert!(q.iter().all(|e| e.announced));
    // the same card again: zero new
    let again = w.scan(DEST);
    assert_eq!((again.new.len(), again.on_this_pc), (0, 2), "{again:?}");
}

#[test]
fn a_name_that_is_taken_gets_a_number_the_sidecar_follows_and_nothing_is_overwritten() {
    let w = W::new("clash");
    let dest = w.photos().join(DEST);
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("IMG_1.jpg"), b"somebody else's photo").unwrap();
    std::fs::write(dest.join("IMG_1.xmp"), b"somebody else's sidecar").unwrap();
    std::fs::write(dest.join("IMG_2-2.jpg"), b"another").unwrap();
    w.card("a/IMG_1.jpg", &photo("first"));
    w.card("a/IMG_1.xmp", &xmp(0.1));
    w.card("b/IMG_1.jpg", &photo("second"));
    w.card("b/IMG_1.jpg.xmp", &xmp(0.2));
    w.card("c/IMG_2.jpg", &photo("third"));
    let (_, r) = w.import();
    assert_eq!(r.copied, 3, "{r:?}");
    assert_eq!(std::fs::read(dest.join("IMG_1.jpg")).unwrap(), b"somebody else's photo");
    assert_eq!(std::fs::read(dest.join("IMG_1.xmp")).unwrap(), b"somebody else's sidecar");
    // the first card photo became -2 with a stem sidecar of its own; the second -3 with a file-style sidecar
    assert_eq!(w.local(&format!("{DEST}/IMG_1-2.jpg")), photo("first"));
    assert_eq!(w.local(&format!("{DEST}/IMG_1-2.xmp")), xmp(0.1));
    assert_eq!(w.local(&format!("{DEST}/IMG_1-3.jpg")), photo("second"));
    assert_eq!(w.local(&format!("{DEST}/IMG_1-3.jpg.xmp")), xmp(0.2));
    assert_eq!(w.local(&format!("{DEST}/IMG_2.jpg")), photo("third"));
    let names: Vec<(String, String)> = r.renamed.clone();
    assert!(names.contains(&("IMG_1.jpg".into(), "IMG_1-2.jpg".into())) && names.contains(&("IMG_1.jpg".into(), "IMG_1-3.jpg".into())), "{names:?}");
    // the queue names the files as they are called here, and the sidecars as the home PC's rule wants them
    for e in w.queue().list() {
        let note = json!({"format": 1, "id": e.id, "folder": e.folder, "name": e.name, "size": e.size, "mtime": e.mtime, "sidecar": e.sidecar});
        check_import_note(note.to_string().as_bytes(), &e.id, &[]).unwrap_or_else(|why| panic!("{}: the home PC would reject it: {why}", e.name));
    }
}

#[test]
fn a_cut_off_import_is_finished_by_running_it_again_and_what_arrived_is_not_copied_twice() {
    let w = W::new("resume");
    for i in 0..4 {
        w.card(&format!("p{i}.jpg"), &photo(&format!("r{i}")));
    }
    // the first run is cancelled after the second photo
    let s = w.scan(DEST);
    let cancel = AtomicBool::new(false);
    let n = std::cell::Cell::new(0);
    let r = w
        .other
        .import_run(
            &s,
            &mut |p: &Progress| {
                if p.done >= 2 && n.replace(1) == 0 {
                    cancel.store(true, Ordering::SeqCst);
                }
            },
            &cancel,
        )
        .unwrap();
    assert!(r.cancelled && r.copied == 2 && r.left == 2, "{r:?}");
    assert_eq!(w.queue().list().len(), 2);
    // a crash between the link and the queue entry: a copy with no entry, and a leftover temporary
    let victim = w.queue().list().into_iter().find(|e| e.name == "p0.jpg").unwrap();
    w.queue().remove(&victim.id);
    std::fs::write(w.photos().join(DEST).join(".lc-import-deadbeef.part"), b"half").unwrap();
    let (s2, r2) = w.import();
    assert_eq!(s2.new.len(), 3, "{s2:?}");
    assert_eq!(s2.new.iter().filter(|f| f.arrived.is_some()).count(), 1, "the copy that arrived is recognised by its bytes");
    assert_eq!((r2.copied, r2.resumed), (2, 1), "{r2:?}");
    assert_eq!(w.tree(&w.photos()), ["p0.jpg", "p1.jpg", "p2.jpg", "p3.jpg"].map(|n| format!("{DEST}/{n}")), "no second copies");
    assert_eq!(w.queue().list().len(), 4);
    assert!(w.fx.tree(&w.photos().join(DEST)).iter().all(|p| !p.contains(".part")), "the leftover is removed");
    assert_eq!(w.scan(DEST).new.len(), 0);
}

#[test]
fn bytes_that_change_between_the_scan_and_the_copy_are_caught_by_the_hash_and_nothing_is_kept() {
    let w = W::new("liar");
    let good = photo("good");
    w.card("a.jpg", &good);
    w.card("b.jpg", &photo("b"));
    let s = w.scan(DEST);
    // the card now returns other bytes of the same length for a.jpg
    let mut bad = good.clone();
    bad[10] ^= 0xff;
    std::fs::write(w.card.join("a.jpg"), &bad).unwrap();
    let r = w.other.import_run(&s, &mut |_| {}, &w.cancel).unwrap();
    assert_eq!(r.copied, 1, "{r:?}");
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].contains("a.jpg") && r.failed[0].contains("different bytes"), "{:?}", r.failed);
    assert_eq!(w.tree(&w.photos()), [format!("{DEST}/b.jpg")]);
    assert!(w.fx.tree(&w.photos().join(DEST)).iter().all(|p| !p.starts_with('.')), "no temporary file is left");
    assert_eq!(w.queue().list().len(), 1);
}

#[test]
fn an_import_stops_cleanly_when_the_disk_is_too_full_and_says_how_many_are_left() {
    let w = W::with("space", |c| c.free_margin = u64::MAX / 2);
    w.card("a.jpg", &photo("a"));
    w.card("b.jpg", &photo("b"));
    let s = w.scan(DEST);
    if s.free.is_none() {
        return; // (free space cannot be told on this system)
    }
    assert!(s.space_problem().is_some_and(|p| p.contains("Not enough free space")));
    let r = w.other.import_run(&s, &mut |_| {}, &w.cancel).unwrap();
    assert!(r.no_space.as_deref().is_some_and(|m| m.contains("2 photos were")), "{r:?}");
    assert_eq!((r.copied, r.left), (0, 2));
    assert!(w.queue().list().is_empty());
}

#[test]
fn the_source_inside_the_destination_is_refused() {
    let w = W::new("inside");
    let dest = w.photos().join(DEST);
    std::fs::create_dir_all(dest.join("sub")).unwrap();
    std::fs::write(dest.join("sub/a.jpg"), photo("a")).unwrap();
    let e = w.other.import_scan(&dest.join("sub"), DEST, &mut |_| {}, &w.cancel).unwrap_err();
    assert!(matches!(e, Error::Invalid(m) if m.contains("destination")));
}

#[test]
fn the_folders_on_offer_are_the_chosen_ones_and_those_that_exist_without_hidden_or_unusable_names() {
    let w = W::new("folders");
    for d in ["2026/260101 - Porto", ".hidden", "plain", "CON"] {
        std::fs::create_dir_all(w.photos().join(d)).unwrap();
    }
    assert_eq!(w.other.existing_folders(), ["2026", "2026/260101 - Porto", "261101 - Lisbon", "plain"]);
}

// ------------------------------------------------------------------ the upload queue

fn import_n(w: &W, n: usize, tag: &str) -> Vec<String> {
    for i in 0..n {
        w.card(&format!("{tag}{i}.jpg"), &photo(&format!("{tag}{i}")));
    }
    w.import();
    (0..n).map(|i| content_hash_bytes(&photo(&format!("{tag}{i}")))).collect()
}

#[test]
fn the_files_go_up_first_and_each_commit_note_last_and_is_the_format_the_home_pc_reads() {
    let w = W::new("order");
    w.card("DSC1.ARW", &photo("one"));
    w.card("DSC1.xmp", &xmp(0.3));
    w.import();
    w.spy.puts();
    assert_eq!(w.upload_all(), 1);
    let id = content_hash_bytes(&photo("one"));
    assert_eq!(w.spy.puts(), [format!("imports/laptop/{id}/DSC1.ARW"), format!("imports/laptop/{id}/DSC1.xmp"), format!("imports/laptop/{id}.json")]);
    let note: Value = serde_json::from_slice(&w.fx.rfile(&format!("imports/laptop/{id}.json")).unwrap()).unwrap();
    assert_eq!(note["folder"], DEST);
    assert_eq!((note["name"].as_str(), note["sidecar"].as_str(), note["format"].as_u64()), (Some("DSC1.ARW"), Some("DSC1.xmp"), Some(1)));
    check_import_note(note.to_string().as_bytes(), &id, &[]).unwrap();
    assert_eq!(w.fx.rfile(&format!("imports/laptop/{id}/DSC1.xmp")).unwrap(), xmp(0.3));
    // nothing is sent twice
    w.upload_all();
    assert!(w.spy.puts().is_empty());
}

#[test]
fn a_file_is_sent_again_only_when_it_is_not_on_the_remote_at_the_right_size() {
    let w = W::new("resend");
    let ids = import_n(&w, 2, "r");
    // an earlier try got the first photo up whole and the second one cut short
    let put = |rel: &str, data: &[u8]| {
        let p = w.fx.remote.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    };
    put(&format!("imports/laptop/{}/r0.jpg", ids[0]), &photo("r0"));
    put(&format!("imports/laptop/{}/r1.jpg", ids[1]), &photo("r1")[..100]);
    w.spy.puts();
    w.upload_all();
    let puts = w.spy.puts();
    assert_eq!(puts, [format!("imports/laptop/{}.json", ids[0]), format!("imports/laptop/{}/r1.jpg", ids[1]), format!("imports/laptop/{}.json", ids[1])]);
    assert_eq!(w.fx.rfile(&format!("imports/laptop/{}/r1.jpg", ids[1])).unwrap(), photo("r1"));
}

#[test]
fn each_step_is_bounded_so_a_big_queue_never_holds_a_job_for_long() {
    let w = W::new("bounded");
    import_n(&w, 7, "b");
    let budget = UploadBudget { max_files: 3, ..UploadBudget::default() };
    let mut steps = Vec::new();
    loop {
        let r = w.other.upload_step(&budget).unwrap();
        steps.push((r.committed, r.more));
        if !r.more {
            break;
        }
    }
    assert_eq!(steps, [(3, true), (3, true), (1, false)]);
    // the byte budget bounds it too
    let w = W::new("bounded2");
    import_n(&w, 4, "c");
    let one = photo("c0").len() as u64;
    let r = w.other.upload_step(&UploadBudget { max_bytes: one, ..UploadBudget::default() }).unwrap();
    assert!(r.committed < 4 && r.more, "{r:?}");
    let r = w.other.upload_step(&UploadBudget { max_time: Duration::ZERO, ..UploadBudget::default() }).unwrap();
    assert_eq!(r.committed, 0, "a spent time budget starts nothing");
    assert!(r.more);
}

#[test]
fn offline_the_queue_waits_untouched_survives_a_restart_and_goes_up_when_the_network_is_back() {
    let mut w = W::new("offline");
    let ids = import_n(&w, 3, "o");
    w.spy.fail_puts.store(1000, Ordering::SeqCst);
    for _ in 0..3 {
        let e = w.other.upload_step(&UploadBudget::default()).unwrap_err();
        assert!(matches!(e, Error::Remote(_)), "{e}");
    }
    let s = w.other.import_queue_status();
    assert_eq!((s.to_upload, s.awaiting_home), (3, 0));
    assert!(w.fx.remote_tree().iter().all(|p| !p.starts_with("imports/")), "nothing arrived, no note: {:?}", w.fx.remote_tree());
    // the app is restarted days later
    let cfg = w.cfg();
    w.restart(cfg);
    assert_eq!(w.other.import_queue_status().to_upload, 3);
    w.spy.fail_puts.store(0, Ordering::SeqCst);
    assert_eq!(w.upload_all(), 3);
    for id in ids {
        assert!(w.fx.rfile(&format!("imports/laptop/{id}.json")).is_some());
    }
    let s = w.other.import_queue_status();
    assert_eq!((s.to_upload, s.awaiting_home), (0, 3));
}

#[test]
fn a_photo_deleted_before_it_was_sent_is_dropped_with_a_line_and_the_rest_goes_on() {
    let w = W::new("gone");
    import_n(&w, 2, "g");
    std::fs::remove_file(w.photos().join(DEST).join("g0.jpg")).unwrap();
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert_eq!(r.committed, 1);
    assert!(r.problems.iter().any(|p| p.contains("g0.jpg") && p.contains("not on this PC")), "{:?}", r.problems);
    assert_eq!(w.queue().list().len(), 1);
}

// ------------------------------------------------------------------ the home PC's answer

/// Import on the laptop, send, let the home PC adopt and publish.
fn round_trip(w: &W, n: usize, tag: &str) -> Vec<String> {
    w.seed_home();
    let ids = import_n(w, n, tag);
    w.upload_all();
    w.home_sync();
    ids
}

#[test]
fn an_added_photo_is_finished_when_the_result_is_seen_and_this_pc_never_downloads_it_again() {
    let w = W::new("added");
    let ids = round_trip(&w, 2, "a");
    // the result first, then the manifest (the pass)
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert_eq!(r.results.len(), 2);
    let e = w.queue().get(&ids[0]).unwrap();
    assert_eq!(e.result.as_ref().map(|x| (x.status.as_str(), x.path.as_deref())), Some(("added", Some(&*format!("{DEST}/a0.jpg")))));
    assert_eq!(w.other.import_queue_status().awaiting_home, 0);
    let before = w.tree(&w.photos());
    let rep = w.pass();
    assert_eq!((rep.downloaded.len(), rep.waiting_to_download, rep.photos), (0, 0, 2), "{rep:?}");
    assert_eq!(w.tree(&w.photos()), before, "no second copy");
    // the entries are tidied away once the pass has them
    w.other.upload_step(&UploadBudget::default()).unwrap();
    assert!(w.queue().list().is_empty());
    assert_eq!(w.other.status().unwrap().photos, 2);
    // and later passes keep it as an ordinary photo of this PC
    assert_eq!(w.pass().downloaded.len(), 0);
}

#[test]
fn the_manifest_listing_the_photo_before_the_result_is_seen_changes_nothing_either() {
    let w = W::new("manifest-first");
    let ids = round_trip(&w, 2, "m");
    let before = w.tree(&w.photos());
    let rep = w.pass(); // the manifest lists both; the result has not been read
    assert_eq!((rep.downloaded.len(), rep.photos), (0, 2), "{rep:?}");
    assert!(rep.problems.is_empty(), "{:?}", rep.problems);
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert_eq!(r.results.len(), 2, "the result is still found");
    assert!(w.queue().list().is_empty() || w.queue().list().iter().all(|e| e.result.is_some()));
    w.other.upload_step(&UploadBudget::default()).unwrap();
    assert!(w.queue().list().is_empty(), "{:?}", w.queue().list());
    assert_eq!(w.tree(&w.photos()), before);
    assert_eq!(w.pass().downloaded.len(), 0);
    assert_eq!(ids.len(), 2);
}

#[test]
fn when_the_home_pc_renamed_the_photo_the_file_here_stays_and_is_not_treated_as_deleted_or_downloaded_again() {
    let w = W::new("renamed");
    w.seed_home();
    // the home PC already has another photo under the name the card's photo has
    w.fx.touch(&format!("{DEST}/IMG_1.jpg"), &photo("home's own"), 3600);
    w.card("IMG_1.jpg", &photo("from the card"));
    w.card("IMG_1.xmp", &xmp(0.4));
    w.import();
    w.upload_all();
    w.home_sync();
    let id = content_hash_bytes(&photo("from the card"));
    let res = w.fx.remote_tree().into_iter().find(|p| p.ends_with(&format!("{id}.result.json"))).unwrap();
    let res: Value = serde_json::from_slice(&w.fx.rfile(&res).unwrap()).unwrap();
    assert_eq!((res["status"].as_str(), res["path"].as_str()), (Some("added"), Some(&*format!("{DEST}/IMG_1-2.jpg"))));
    assert_eq!(std::fs::read(w.fx.src.join(DEST).join("IMG_1-2.xmp")).unwrap(), xmp(0.4));

    for order in 0..2 {
        if order == 0 {
            w.pass(); // manifest before the result
            w.other.upload_step(&UploadBudget::default()).unwrap();
        } else {
            w.other.upload_step(&UploadBudget::default()).unwrap(); // result before the manifest
            w.pass();
        }
        assert_eq!(w.local(&format!("{DEST}/IMG_1.jpg")), photo("from the card"), "the file stays where it is");
        assert!(!w.photos().join(DEST).join("IMG_1-2.jpg").exists(), "no second copy under the home PC's name");
        assert!(w.fx.tree(&w.laptop.join("state")).iter().all(|p| !p.starts_with("removed")), "never treated as deleted");
    }
    // the home PC's own photo cannot come down while a different file has its name: the pass says so (an active problem), nothing is lost
    let rep = w.pass();
    assert!(rep.problems.iter().any(|p| p.contains("IMG_1.jpg") && p.contains("different contents")), "{:?}", rep.problems);
    assert_eq!(w.local(&format!("{DEST}/IMG_1.jpg")), photo("from the card"));
    assert_eq!(rep.downloaded.len(), 0);
}

#[test]
fn a_rejected_photo_stays_on_this_pc_and_the_reason_is_shown_once() {
    let w = W::new("rejected");
    w.seed_home();
    w.card("bad.jpg", &photo("corrupt on the way"));
    w.import();
    let id = content_hash_bytes(&photo("corrupt on the way"));
    // the bytes are damaged on the way to the cloud library
    w.upload_all();
    let up = w.fx.remote.join(format!("imports/laptop/{id}/bad.jpg"));
    let mut bytes = std::fs::read(&up).unwrap();
    bytes[5] ^= 0x55;
    std::fs::write(&up, bytes).unwrap();
    w.home_sync();
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert_eq!(r.results, [(id.clone(), "rejected".to_string())]);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(r.problems[0].contains("bad.jpg") && r.problems[0].contains("does not match"), "{:?}", r.problems);
    assert_eq!(w.local(&format!("{DEST}/bad.jpg")), photo("corrupt on the way"), "the local file stays");
    // once: the next step has nothing to say, and the entry is gone
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert!(r.problems.is_empty());
    assert!(w.queue().list().is_empty());
    assert!(!w.fx.remote_tree().iter().any(|p| p.ends_with("bad.jpg") && p.starts_with("originals")), "the home PC did not take it");
    let rep = w.pass();
    assert_eq!((rep.downloaded.len(), rep.problems.len()), (0, 0), "{rep:?}");
}

#[test]
fn the_same_photo_from_two_devices_is_added_once_and_the_second_is_a_duplicate_that_still_finishes() {
    let w = W::new("two");
    w.seed_home();
    w.card("IMG_5.jpg", &photo("shared"));
    w.import();
    // the second device (alpha) uploaded the same bytes under its own name first
    let id = content_hash_bytes(&photo("shared"));
    let p = w.fx.remote.join(format!("imports/alpha/{id}/IMG_9.jpg"));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, photo("shared")).unwrap();
    let note = json!({"format": 1, "id": id, "folder": "From alpha", "name": "IMG_9.jpg", "size": photo("shared").len(), "mtime": "2024-05-01T12:00:00Z", "sidecar": null});
    std::fs::write(w.fx.remote.join(format!("imports/alpha/{id}.json")), note.to_string()).unwrap();
    w.upload_all();
    w.home_sync();
    let statuses: Vec<String> = ["alpha", "laptop"]
        .iter()
        .map(|d| {
            let f = w.fx.remote_tree().into_iter().find(|p| p.starts_with("imports-done/") && p.ends_with(&format!("{d}/{id}.result.json"))).unwrap();
            serde_json::from_slice::<Value>(&w.fx.rfile(&f).unwrap()).unwrap()["status"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(statuses, ["added", "duplicate"]);
    assert_eq!(w.fx.tree(&w.fx.src).iter().filter(|p| p.ends_with(".jpg") && !p.starts_with("Seed")).count(), 1, "one copy at home");
    // the laptop's file finishes as a duplicate of the tablet's folder: not downloaded, not deleted
    w.other.upload_step(&UploadBudget::default()).unwrap();
    let before = w.tree(&w.photos());
    let rep = w.pass();
    assert_eq!(rep.downloaded.len(), 0, "{rep:?}");
    assert_eq!(w.tree(&w.photos()), before);
    assert_eq!(w.pass().photos, 1);
}

// ------------------------------------------------------------------ edits before the home PC has the photo

#[test]
fn an_edit_made_before_the_home_pc_adopted_the_photo_waits_then_goes_to_the_inbox_with_the_right_base() {
    let w = W::new("edit");
    w.seed_home();
    w.home_sync();
    w.card("DSC1.jpg", &photo("edited"));
    let original = xmp(0.1);
    w.card("DSC1.xmp", &original);
    w.import();
    let id = content_hash_bytes(&photo("edited"));
    let s0 = w.queue().get(&id).unwrap().sidecar_sha.unwrap();
    // LightCraft changes the sidecar after the import, before anything was sent
    let edited = xmp(0.9);
    std::fs::write(w.photos().join(DEST).join("DSC1.xmp"), &edited).unwrap();
    let rep = w.pass();
    assert_eq!(rep.edits_sent, 0, "not sent early");
    assert_eq!(rep.edits_waiting, 1, "but not forgotten: it shows as waiting: {rep:?}");
    assert_eq!(w.other.status().unwrap().edits_waiting, 1);
    assert!(w.fx.rfile("inbox").is_none() && !w.fx.remote.join("inbox").exists());
    // the photo goes up with the sidecar as it was imported, not as it is now
    w.upload_all();
    assert_eq!(w.fx.rfile(&format!("imports/laptop/{id}/DSC1.xmp")).unwrap(), original);
    let rep = w.pass();
    assert_eq!(rep.edits_sent, 0, "the home PC has not adopted it yet");
    assert!(!w.fx.remote.join("inbox").exists());
    // the home PC adopts and publishes; the next pass sends the edit
    w.home_sync();
    assert_eq!(std::fs::read(w.fx.src.join(DEST).join("DSC1.xmp")).unwrap(), original);
    let rep = w.pass();
    assert_eq!(rep.edits_sent, 1, "{rep:?}");
    let sent: Vec<String> = std::fs::read_dir(w.fx.remote.join("inbox/laptop")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].starts_with(&id) && sent[0].ends_with(&format!("-{}.xmp", sha16(&original))), "base is the version the home PC has: {sent:?} (s0 {s0})");
    assert_eq!(std::fs::read(w.fx.remote.join("inbox/laptop").join(&sent[0])).unwrap(), edited);
    assert_eq!(w.local(&format!("{DEST}/DSC1.xmp")), edited, "the local edit is untouched");
    w.home_sync();
    assert_eq!(std::fs::read(w.fx.src.join(DEST).join("DSC1.xmp")).unwrap(), edited, "applied at home");
    let rep = w.pass();
    assert_eq!((rep.edits_sent, rep.edits_waiting), (0, 0), "{rep:?}");
}

#[test]
fn a_photo_that_arrived_in_the_queue_but_was_never_edited_sends_nothing() {
    let w = W::new("noedit");
    w.card("DSC1.jpg", &photo("quiet"));
    w.card("DSC1.xmp", &xmp(0.1));
    w.seed_home();
    w.import();
    w.upload_all();
    w.home_sync();
    let rep = w.pass();
    assert_eq!((rep.edits_sent, rep.edits_waiting, rep.downloaded.len()), (0, 0, 0), "{rep:?}");
    assert!(!w.fx.remote.join("inbox").exists());
}

// ------------------------------------------------------------------ end to end

impl W {
    fn offline(&self, off: bool) {
        *self.spy.fail_list_of.lock().unwrap() = off.then(String::new);
        self.spy.fail_gets.store(if off { 100_000 } else { 0 }, Ordering::SeqCst);
        self.spy.fail_puts.store(if off { 100_000 } else { 0 }, Ordering::SeqCst);
    }
}

#[test]
fn a_holiday_end_to_end_offline_import_restart_adopted_published_seen_without_a_second_copy() {
    let mut w = W::new("holiday");
    w.seed_home();
    // the home PC already has a different photo called IMG_1.jpg in the holiday folder
    w.fx.touch(&format!("{DEST}/IMG_1.jpg"), &photo("at home"), 3600);
    w.home_sync();
    // the card is imported on the laptop with no network at all
    w.offline(true);
    w.card("DCIM/IMG_1.jpg", &photo("card one"));
    w.card("DCIM/IMG_1.xmp", &xmp(0.2));
    w.card("DCIM/IMG_2.ARW", &photo("card two"));
    w.card("DCIM/IMG_3.jpg", &photo("card three"));
    let (s, r) = w.import();
    assert!(!s.library_checked, "offline");
    assert_eq!((r.copied, r.failed.len()), (3, 0), "{r:?}");
    assert!(w.other.upload_step(&UploadBudget::default()).is_err());
    assert!(w.other.sync().is_err());
    // days pass; the app is restarted; the network comes back
    let cfg = w.cfg();
    w.restart(cfg);
    assert_eq!(w.other.import_queue_status().to_upload, 3, "the queue survived");
    w.offline(false);
    assert_eq!(w.upload_all(), 3);
    // the home PC adopts, with a clash, and publishes
    w.home_sync();
    assert_eq!(std::fs::read(w.fx.src.join(DEST).join("IMG_1.jpg")).unwrap(), photo("at home"), "never overwritten at home");
    assert_eq!(std::fs::read(w.fx.src.join(DEST).join("IMG_1-2.jpg")).unwrap(), photo("card one"));
    assert_eq!(std::fs::read(w.fx.src.join(DEST).join("IMG_1-2.xmp")).unwrap(), xmp(0.2), "the sidecar followed the new name");
    let m = w.fx.manifest();
    let paths: Vec<String> = m["photos"].as_array().unwrap().iter().map(|p| p["path"].as_str().unwrap().to_string()).collect();
    assert!(paths.contains(&format!("{DEST}/IMG_1-2.jpg")) && paths.contains(&format!("{DEST}/IMG_2.ARW")), "{paths:?}");
    // the laptop sees it: the results first, then the manifest, and nothing is downloaded twice
    let before = w.tree(&w.photos());
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert_eq!(r.results.iter().filter(|(_, s)| s == "added").count(), 3, "{r:?}");
    let rep = w.pass();
    assert!(rep.downloaded.is_empty(), "{rep:?}");
    assert_eq!(w.tree(&w.photos()), before);
    assert_eq!(w.local(&format!("{DEST}/IMG_1.jpg")), photo("card one"));
    assert!(rep.problems.iter().all(|p| p.contains("IMG_1.jpg")), "only the clash with the home PC's own IMG_1.jpg is reported: {:?}", rep.problems);
    w.other.upload_step(&UploadBudget::default()).unwrap();
    assert!(w.queue().list().is_empty());
    // the same card again: zero new
    let again = w.scan(DEST);
    assert_eq!((again.new.len(), again.in_library), (0, 3), "{again:?}");
    // a second device sends the same photo, and one of this PC's photos is damaged on the way to the cloud
    let id2 = content_hash_bytes(&photo("card two"));
    let p = w.fx.remote.join(format!("imports/alpha/{id2}/IMG_2.ARW"));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, photo("card two")).unwrap();
    let note = json!({"format": 1, "id": id2, "folder": "Alpha", "name": "IMG_2.ARW", "size": photo("card two").len(), "mtime": "2024-05-01T12:00:00Z", "sidecar": null});
    std::fs::write(w.fx.remote.join(format!("imports/alpha/{id2}.json")), note.to_string()).unwrap();
    w.card("DCIM2/IMG_4.jpg", &photo("card four"));
    let (_, r) = w.import();
    assert_eq!(r.copied, 1);
    w.upload_all();
    let id4 = content_hash_bytes(&photo("card four"));
    let up = w.fx.remote.join(format!("imports/laptop/{id4}/IMG_4.jpg"));
    let mut b = std::fs::read(&up).unwrap();
    b[3] ^= 1;
    std::fs::write(&up, b).unwrap();
    w.home_sync();
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert_eq!(r.results, [(id4.clone(), "rejected".to_string())], "{r:?}");
    assert_eq!(r.problems.len(), 1);
    assert_eq!(w.local(&format!("{DEST}/IMG_4.jpg")), photo("card four"));
    // alpha's copy of "card two" was a duplicate of the laptop's: one photo at home
    let n2 = w.fx.tree(&w.fx.src).iter().filter(|p| p.ends_with("IMG_2.ARW")).count();
    assert_eq!(n2, 1);
}
