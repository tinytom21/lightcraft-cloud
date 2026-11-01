//! Photos imported in LightCraft on another PC, into its synced photo folder: the pass notices them (nested folders kept), queues them,
//! the app sends them, and what happens once the home PC has answered. The home side is the real `Home` with a renderer that knows
//! the real content hash; the cloud is a folder.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Fx, HashRenderer, Spy, sc, sha16};
use lightcraft_cloud_client::home::{check_import_note, content_hash_bytes};
use lightcraft_cloud_client::other::{ImportQueue, Other, OtherConfig, UploadBudget};
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
    told: Arc<Mutex<Vec<PathBuf>>>,
    /// The time this PC's clock shows (starts at the real time; tests move it).
    clock: Arc<Mutex<std::time::SystemTime>>,
}

impl W {
    fn new(tag: &str) -> W {
        W::with(tag, |_| {})
    }

    /// Set up, and take the first look at the (empty) folder: what is put there later is not the baseline.
    fn with(tag: &str, f: impl FnOnce(&mut OtherConfig)) -> W {
        let w = W::bare(tag, f);
        w.seed_home();
        w.home_sync();
        w.find();
        w
    }

    fn advance(&self, secs: u64) {
        let mut c = self.clock.lock().unwrap();
        *c += Duration::from_secs(secs);
    }

    /// Set up, with nothing looked at yet.
    fn bare(tag: &str, f: impl FnOnce(&mut OtherConfig)) -> W {
        let fx = Fx::new(tag);
        let laptop = fx.root.join("laptop");
        std::fs::create_dir_all(laptop.join("photos")).unwrap();
        let mut cfg = OtherConfig::new(laptop.join("photos"), laptop.join("state"), "laptop", vec![DEST.into()]);
        cfg.free_margin = 0;
        cfg.settle = Duration::ZERO;
        f(&mut cfg);
        let spy = Spy::new(&fx.remote);
        let remote: Arc<dyn Remote> = spy.clone();
        let other = Other::new(cfg, remote).unwrap();
        let mut w = W { fx, spy, other, laptop, told: Arc::default(), clock: Arc::new(Mutex::new(std::time::SystemTime::now())) };
        w.restart(w.cfg());
        w
    }

    /// A new `Other` over the same folders: the app was restarted.
    fn restart(&mut self, cfg: OtherConfig) {
        let remote: Arc<dyn Remote> = self.spy.clone();
        let mut o = Other::new(cfg, remote).unwrap();
        o.set_retry_delay(Duration::ZERO);
        let clock = self.clock.clone();
        o.set_clock(move || *clock.lock().unwrap());
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

    /// A file LightCraft wrote into the photo folder (any depth).
    fn put(&self, rel: &str, data: &[u8]) {
        let p = self.photos().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    /// A file in the holiday folder.
    fn card(&self, rel: &str, data: &[u8]) {
        self.put(&format!("{DEST}/{rel}"), data);
    }

    /// A pass (it may not reach the cloud library): notices the new files.
    fn find(&self) {
        let _ = self.other.sync();
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


// ------------------------------------------------------------------ the destination



// ------------------------------------------------------------------ the scan




// ------------------------------------------------------------------ the copy








// ------------------------------------------------------------------ the upload queue

fn import_n(w: &W, n: usize, tag: &str) -> Vec<String> {
    for i in 0..n {
        w.card(&format!("{tag}{i}.jpg"), &photo(&format!("{tag}{i}")));
    }
    w.find();
    (0..n).map(|i| content_hash_bytes(&photo(&format!("{tag}{i}")))).collect()
}

#[test]
fn the_files_go_up_first_and_each_commit_note_last_and_is_the_format_the_home_pc_reads() {
    let w = W::new("order");
    w.card("DSC1.ARW", &photo("one"));
    w.card("DSC1.xmp", &xmp(0.3));
    w.find();
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
    w.find();
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
    w.find();
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
    // told once, but not forgotten: it is counted until the user deals with it
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert!(r.problems.is_empty());
    assert_eq!(r.queue.rejected, 1);
    assert_eq!(w.queue().refused().len(), 1);
    assert!(!w.fx.remote_tree().iter().any(|p| p.ends_with("bad.jpg") && p.starts_with("originals")), "the home PC did not take it");
    let rep = w.pass();
    assert_eq!((rep.downloaded.len(), rep.problems.len()), (0, 0), "{rep:?}");
}

#[test]
fn the_same_photo_from_two_devices_is_added_once_and_the_second_is_a_duplicate_that_still_finishes() {
    let w = W::new("two");
    w.seed_home();
    w.card("IMG_5.jpg", &photo("shared"));
    w.find();
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
    w.find();
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
    w.find();
    w.upload_all();
    w.home_sync();
    let rep = w.pass();
    assert_eq!((rep.edits_sent, rep.edits_waiting, rep.downloaded.len()), (0, 0, 0), "{rep:?}");
    assert!(!w.fx.remote.join("inbox").exists());
}

impl W {
    fn offline(&self, off: bool) {
        *self.spy.fail_list_of.lock().unwrap() = off.then(String::new);
        self.spy.fail_gets.store(if off { 100_000 } else { 0 }, Ordering::SeqCst);
        self.spy.fail_puts.store(if off { 100_000 } else { 0 }, Ordering::SeqCst);
    }
}

// ------------------------------------------------------------------ a refusal stays until it is dealt with

/// A photo whose upload was damaged on the way: the home PC refuses it.
fn refused_photo(w: &W) -> String {
    w.seed_home();
    w.home_sync();
    w.card("bad.jpg", &photo("refused one"));
    w.find();
    let id = content_hash_bytes(&photo("refused one"));
    w.upload_all();
    let up = w.fx.remote.join(format!("imports/laptop/{id}/bad.jpg"));
    let mut bytes = std::fs::read(&up).unwrap();
    bytes[5] ^= 0x55;
    std::fs::write(&up, bytes).unwrap();
    w.home_sync();
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert_eq!(r.queue.rejected, 1);
    id
}

#[test]
fn a_refused_photo_stays_counted_across_restarts_and_passes_until_the_user_deals_with_it() {
    let mut w = W::new("refused-count");
    let id = refused_photo(&w);
    for _ in 0..2 {
        let cfg = w.cfg();
        w.restart(cfg);
        w.other.upload_step(&UploadBudget::default()).unwrap();
        assert_eq!(w.other.import_queue_status().rejected, 1);
        assert_eq!(w.other.status().unwrap().queue.rejected, 1);
        assert_eq!(w.pass().queue.rejected, 1);
    }
    let e = w.queue().refused().pop().unwrap();
    assert_eq!(e.id, id);
    assert!(e.result.unwrap().reason.unwrap().contains("does not match"), "the reason is kept");
    assert_eq!(w.local(&format!("{DEST}/bad.jpg")), photo("refused one"));
}

#[test]
fn try_again_requeues_the_refused_photos_and_a_clean_upload_is_added_even_on_the_same_day() {
    let mut w = W::new("refused-retry");
    let id = refused_photo(&w);
    let cfg = w.cfg();
    w.restart(cfg); // (the choice is made in another process; the queue is on disk)
    assert_eq!(w.queue().retry_refused(), 1);
    assert_eq!(w.queue().retry_refused(), 0);
    let s = w.other.import_queue_status();
    assert_eq!((s.rejected, s.to_upload), (0, 1));
    w.spy.puts();
    assert_eq!(w.upload_all(), 1);
    assert!(w.spy.puts().iter().any(|p| p.ends_with("bad.jpg")), "sent again from the file that stayed here");
    // the home PC has not looked yet: the old refusal (same file name, same day) must not be taken for the answer
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert!(r.results.is_empty(), "{r:?}");
    assert_eq!(w.other.import_queue_status().awaiting_home, 1);
    // (the damage was only on the way: the good bytes are what is uploaded now)
    w.home_sync();
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert_eq!(r.results, [(id.clone(), "added".to_string())], "{r:?}");
    assert_eq!(w.other.import_queue_status().rejected, 0);
    assert_eq!(std::fs::read(w.fx.src.join(DEST).join("bad.jpg")).unwrap(), photo("refused one"));
    assert_eq!(w.local(&format!("{DEST}/bad.jpg")), photo("refused one"));
}

#[test]
fn keep_on_this_pc_only_accepts_the_refusal_keeps_the_file_and_never_sends_it_again() {
    let mut w = W::new("refused-keep");
    refused_photo(&w);
    assert_eq!(w.queue().keep_refused(), 1);
    assert_eq!(w.queue().keep_refused(), 0);
    let cfg = w.cfg();
    w.restart(cfg);
    assert_eq!(w.other.import_queue_status().rejected, 0, "the count is cleared and stays cleared");
    w.spy.puts();
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert_eq!((r.committed, r.queue.to_upload, r.queue.rejected), (0, 0, 0));
    assert!(w.spy.puts().is_empty(), "never sent again");
    assert_eq!(w.local(&format!("{DEST}/bad.jpg")), photo("refused one"));
    assert_eq!(w.queue().list().len(), 1, "remembered, so the same photo is not sent again");
    // the same bytes at another path are not queued again either
    w.card("copy of bad.jpg", &photo("refused one"));
    w.find();
    assert_eq!(w.queue().list().len(), 1);
    // and the pass does not try to match it with anything
    let rep = w.pass();
    assert_eq!((rep.downloaded.len(), rep.queue.rejected), (0, 0));
}

// ------------------------------------------------------------------ noticing what LightCraft imported

fn entries(w: &W) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = w.queue().list().into_iter().map(|e| (e.folder, e.name)).collect();
    v.sort();
    v
}

#[test]
fn photos_in_nested_folders_keep_their_structure_every_folder_becomes_a_chosen_folder_and_the_root_works_too() {
    let w = W::new("nested");
    let a = "2026/261101 - Lisbon/Day 1";
    w.put(&format!("{a}/DSC1.ARW"), &photo("one"));
    w.put(&format!("{a}/DSC1.xmp"), &xmp(0.2));
    w.put("2026/261101 - Lisbon/Day 2/DSC2.jpg", &photo("two"));
    w.put("A/B/C/D/deep.jpg", &photo("deep"));
    w.put("ROOT.jpg", &photo("root"));
    let rep = w.pass();
    assert_eq!(rep.picked_up, 4, "{rep:?}");
    assert_eq!(
        entries(&w),
        [
            ("".to_string(), "ROOT.jpg".to_string()),
            ("2026/261101 - Lisbon/Day 1".into(), "DSC1.ARW".into()),
            ("2026/261101 - Lisbon/Day 2".into(), "DSC2.jpg".into()),
            ("A/B/C/D".into(), "deep.jpg".into()),
        ],
        "the folder is the file's own folder, exactly, at any depth"
    );
    assert_eq!(rep.new_folders, ["", "2026/261101 - Lisbon/Day 1", "2026/261101 - Lisbon/Day 2", "A/B/C/D"], "the folders to keep: each one, none of its parents");
    let one = w.queue().list().into_iter().find(|e| e.name == "DSC1.ARW").unwrap();
    assert_eq!(one.sidecar.as_deref(), Some("DSC1.xmp"));
    assert_eq!(w.queue().snapshot(&one.id), Some(xmp(0.2)));
    // the home PC adopts every one at the same path
    w.upload_all();
    w.home_sync();
    for rel in [&format!("{a}/DSC1.ARW"), &format!("{a}/DSC1.xmp"), "2026/261101 - Lisbon/Day 2/DSC2.jpg", "A/B/C/D/deep.jpg", "ROOT.jpg"] {
        assert!(w.fx.src.join(rel).is_file(), "{rel} at home");
    }
    assert_eq!(std::fs::read(w.fx.src.join(a).join("DSC1.ARW")).unwrap(), photo("one"));
    // the folder names are the same on both PCs (no `\`, whatever the system's separator)
    assert!(w.queue().list().iter().all(|e| !e.folder.contains('\\')));
}

#[test]
fn a_new_photo_is_taken_only_after_its_size_and_time_have_been_quiet_for_the_settle_time() {
    let w = W::with("settle", |c| c.settle = Duration::from_secs(120));
    w.put("A/big.ARW", &photo("copying"));
    let rep = w.pass();
    assert_eq!((rep.picked_up, rep.unsettled), (0, 1), "{rep:?}");
    assert!(rep.recheck.is_some_and(|d| d >= Duration::from_secs(120)), "it asks to be looked at again: {:?}", rep.recheck);
    w.advance(100);
    // LightCraft is still writing: the size changes, the quiet period starts again
    w.put("A/big.ARW", &photo("copying, and more of it"));
    w.pass();
    w.advance(100);
    assert_eq!(w.pass().picked_up, 0, "100 quiet seconds since the last change are not enough");
    w.advance(25);
    let rep = w.pass();
    assert_eq!(rep.picked_up, 1, "{rep:?}");
    let e = w.queue().list().remove(0);
    assert_eq!(e.id, content_hash_bytes(&photo("copying, and more of it")), "the bytes that were there at the end");
}

#[test]
fn hashing_is_limited_per_pass_and_the_rest_waits_for_the_next() {
    let w = W::with("budget", |c| c.hash_budget = Duration::ZERO);
    for i in 0..3 {
        w.put(&format!("A/p{i}.jpg"), &photo(&format!("b{i}")));
    }
    let rep = w.pass();
    assert_eq!((rep.picked_up, rep.pickup_left), (1, 2), "{rep:?}");
    assert!(rep.recheck.is_some());
    assert_eq!(w.pass().picked_up, 1);
    assert_eq!(w.pass().picked_up, 1);
    assert_eq!(w.pass().picked_up, 0);
    assert_eq!(w.queue().list().len(), 3);
}

#[test]
fn hidden_temporary_partial_and_excluded_files_are_not_taken() {
    let w = W::with("junk", |c| c.exclude = vec!["exported".into()]);
    w.put("A/.hidden.jpg", &photo("h"));
    w.put(".trash/x.jpg", &photo("t"));
    w.put("A/x.jpg.part", &photo("p"));
    w.put("A/x.jpg.tmp", &photo("p2"));
    w.put("A/x.jpg~", &photo("p3"));
    w.put("A/notes.txt", &photo("n"));
    w.put("exported/a.jpg", &photo("e"));
    w.put("A/exported/b.jpg", &photo("e2"));
    w.put("A/real.jpg", &photo("real"));
    assert_eq!(w.pass().picked_up, 1);
    assert_eq!(entries(&w), [("A".to_string(), "real.jpg".to_string())]);
}

#[test]
fn a_sidecar_in_either_naming_style_goes_along_if_the_home_pc_would_take_it() {
    let w = W::new("sidecars");
    w.put("A/one.ARW", &photo("one"));
    w.put("A/one.xmp", &xmp(0.1)); // <stem>.xmp
    w.put("A/two.jpg", &photo("two"));
    w.put("A/two.jpg.xmp", &xmp(0.2)); // <name>.xmp
    w.put("A/three.jpg", &photo("three"));
    w.put("A/three.xmp", b"this is not xmp");
    w.put("A/four.jpg", &photo("four"));
    w.put("A/FOUR.XMP", &xmp(0.4)); // (the home PC wants exactly `four.xmp` or `four.jpg.xmp`)
    assert_eq!(w.pass().picked_up, 4);
    let by = |n: &str| w.queue().list().into_iter().find(|e| e.name == n).unwrap();
    assert_eq!((by("one.ARW").sidecar.as_deref(), by("two.jpg").sidecar.as_deref()), (Some("one.xmp"), Some("two.jpg.xmp")));
    assert_eq!((by("three.jpg").sidecar, by("four.jpg").sidecar), (None, None));
    assert_eq!(w.queue().snapshot(&by("two.jpg").id), Some(xmp(0.2)));
    // all four are adopted at home (none of them is rejected for its sidecar)
    w.upload_all();
    w.home_sync();
    w.other.upload_step(&UploadBudget::default()).unwrap();
    assert!(w.queue().refused().is_empty());
    assert!(w.fx.src.join("A/two.jpg.xmp").is_file() && w.fx.src.join("A/four.jpg").is_file());
}

#[test]
fn lightcraft_is_not_told_about_photos_it_imported_only_about_photos_downloaded_from_home() {
    let w = W::new("not-told");
    w.fx.touch(&format!("{DEST}/FromHome.jpg"), &photo("from home"), 3600);
    w.home_sync();
    w.pass();
    assert_eq!(*w.told.lock().unwrap(), [w.photos().join(DEST).join("FromHome.jpg")], "a download is announced");
    w.card("Imported.jpg", &photo("imported here"));
    w.put("Other/Imported2.jpg", &photo("imported here too"));
    let rep = w.pass();
    assert_eq!(rep.picked_up, 2);
    w.upload_all();
    w.home_sync();
    w.other.upload_step(&UploadBudget::default()).unwrap();
    w.pass();
    w.other.upload_step(&UploadBudget::default()).unwrap();
    assert_eq!(w.told.lock().unwrap().len(), 1, "nothing was announced for the two that LightCraft imported: {:?}", w.told.lock().unwrap());
    assert!(w.queue().list().iter().all(|e| e.announced), "and the queue does not try to either");
}

// (names like CON, `a:b` and `ends.` cannot exist as folders on Windows, so they cannot be picked up there: the rule is covered by
// `safe_rel_on` and the home-side import tests, which run everywhere)
#[cfg(unix)]
#[test]
fn a_destination_the_home_pc_would_reject_is_not_queued_and_is_one_line_in_the_problems() {
    let w = W::new("refuse-dest");
    w.put("fine/ok.jpg", &photo("ok"));
    w.put("CON/a.jpg", &photo("reserved"));
    let rep = w.pass();
    assert_eq!(rep.picked_up, 1);
    assert_eq!(rep.problems.len(), 1, "{:?}", rep.problems);
    assert!(rep.problems[0].contains("CON/a.jpg") && rep.problems[0].contains("reserved Windows name"), "{:?}", rep.problems);
    // many: still one line, and the same each pass (it is active until the files are gone), nothing queued
    w.put("a:b/x.jpg", &photo("colon"));
    w.put("ends./y.jpg", &photo("dot"));
    for _ in 0..2 {
        let rep = w.pass();
        assert_eq!(rep.problems.len(), 1, "{:?}", rep.problems);
        assert!(rep.problems[0].starts_with("3 photos cannot be sent to the home PC (for example "), "{:?}", rep.problems);
    }
    assert_eq!(entries(&w), [("fine".to_string(), "ok.jpg".to_string())]);
    // when they are moved away the line goes
    std::fs::remove_dir_all(w.photos().join("CON")).unwrap();
    std::fs::remove_dir_all(w.photos().join("a:b")).unwrap();
    std::fs::remove_dir_all(w.photos().join("ends.")).unwrap();
    assert!(w.pass().problems.is_empty());
}

// ------------------------------------------------------------------ the safety rules

#[test]
fn a_photo_deleted_at_home_never_goes_back_up_even_after_its_folder_was_unticked_or_it_was_restored_by_hand() {
    let mut w = W::new("resurrect");
    w.fx.touch(&format!("{DEST}/Gone.jpg"), &photo("will be deleted"), 3600);
    w.fx.touch(&format!("{DEST}/Gone2.jpg"), &photo("deleted too"), 3600);
    w.home_sync();
    w.pass();
    assert!(w.photos().join(DEST).join("Gone.jpg").is_file());
    // 1. un-choose the folder: the files stay, and so does the memory of what the library had
    let mut cfg = w.cfg();
    cfg.folders = vec![];
    w.restart(cfg);
    w.pass();
    assert!(w.photos().join(DEST).join("Gone.jpg").is_file());
    std::fs::remove_file(w.fx.src.join(DEST).join("Gone.jpg")).unwrap();
    w.home_sync();
    assert_eq!(w.pass().picked_up, 0);
    assert!(w.queue().list().is_empty(), "{:?}", entries(&w));
    assert_eq!(w.upload_all(), 0);
    assert!(w.fx.remote_tree().iter().all(|p| !p.starts_with("imports/")));
    // 2. the other one: chosen, the home PC deletes it, it is moved aside; restoring it by hand does not send it back
    let mut cfg = w.cfg();
    cfg.folders = vec![DEST.into()];
    w.restart(cfg);
    w.pass();
    std::fs::remove_file(w.fx.src.join(DEST).join("Gone2.jpg")).unwrap();
    w.home_sync();
    let rep = w.pass();
    assert_eq!(rep.removed, [format!("{DEST}/Gone2.jpg")], "{rep:?}");
    let moved = w.fx.tree(&w.laptop.join("state/removed"));
    assert_eq!(moved.len(), 1, "{moved:?}");
    std::fs::copy(w.laptop.join("state/removed").join(&moved[0]), w.photos().join(DEST).join("Restored.jpg")).unwrap();
    assert_eq!(w.pass().picked_up, 0);
    w.restart(w.cfg());
    assert_eq!(w.pass().picked_up, 0);
    assert!(w.queue().list().is_empty());
}

#[test]
fn a_photo_the_library_already_has_is_not_sent_and_not_downloaded_a_second_time() {
    let mut w = W::bare("dupe", |c| c.folders = vec![DEST.into(), "Elsewhere".into()]);
    // the same bytes are in the folder (and so in the baseline) when the library gets them under another name
    w.put(&format!("{DEST}/mine.jpg"), &photo("same photo"));
    w.seed_home();
    w.home_sync();
    w.find();
    w.fx.touch("Elsewhere/theirs.jpg", &photo("same photo"), 3600);
    w.home_sync();
    let rep = w.pass();
    assert_eq!((rep.downloaded.len(), rep.waiting_to_download), (0, 0), "{rep:?}");
    assert!(!w.photos().join("Elsewhere").exists(), "no second copy");
    assert!(w.queue().list().is_empty());
    // an import of the same bytes into a third place later: not sent
    w.put("Third/copy.jpg", &photo("same photo"));
    assert_eq!(w.pass().picked_up, 0);
    let cfg = w.cfg();
    w.restart(cfg);
    assert_eq!(w.pass().downloaded.len(), 0);
    assert!(w.queue().list().is_empty());
}

#[test]
fn a_photo_at_two_local_paths_is_queued_once() {
    let w = W::new("twice");
    w.put("A/x.jpg", &photo("twin"));
    w.put("B/sub/y.jpg", &photo("twin"));
    assert_eq!(w.pass().picked_up, 1);
    assert_eq!(w.pass().picked_up, 0);
    assert_eq!(w.queue().list().len(), 1);
    assert_eq!(w.upload_all(), 1);
}

#[test]
fn the_photos_that_were_here_at_the_start_are_not_sent_until_the_user_says_so() {
    let mut w = W::bare("baseline", |_| {});
    w.fx.touch("Library/known.jpg", &photo("the library has this one"), 3600);
    w.seed_home();
    w.home_sync();
    w.put("Old trip/a.jpg", &photo("old a"));
    w.put("Old trip/b/c.jpg", &photo("old c"));
    w.put("Library copy/known.jpg", &photo("the library has this one"));
    let rep = w.pass(); // the first look
    assert_eq!(rep.picked_up, 0, "nothing is sent by itself");
    let q = w.other.import_queue_status();
    assert_eq!((q.held, q.held_unchecked), (2, 0), "two photos here are not in the library: {q:?}");
    // later passes and a restart change nothing
    w.pass();
    let cfg = w.cfg();
    w.restart(cfg);
    assert_eq!(w.pass().picked_up, 0);
    assert_eq!(w.other.import_queue_status().held, 2);
    // a photo that arrives afterwards is taken as usual
    w.put("New/n.jpg", &photo("new"));
    assert_eq!(w.pass().picked_up, 1);
    // "Send them"
    assert!(!w.other.send_requested());
    w.queue().request_send_held();
    assert!(w.other.send_requested());
    let rep = w.pass();
    assert_eq!(rep.picked_up, 2, "{rep:?}");
    assert!(!w.other.send_requested());
    assert_eq!(w.other.import_queue_status().held, 0);
    assert_eq!(entries(&w).len(), 3);
}

#[test]
fn a_file_renamed_or_moved_before_it_was_sent_is_queued_once_under_its_new_path() {
    let w = W::new("moved");
    w.put("A/old name.jpg", &photo("mover"));
    assert_eq!(w.pass().picked_up, 1);
    std::fs::create_dir_all(w.photos().join("B/deeper")).unwrap();
    std::fs::rename(w.photos().join("A/old name.jpg"), w.photos().join("B/deeper/new name.jpg")).unwrap();
    let rep = w.pass();
    assert_eq!(rep.picked_up, 1, "{rep:?}");
    assert_eq!(entries(&w), [("B/deeper".to_string(), "new name.jpg".to_string())]);
    assert!(rep.problems.is_empty(), "no complaint about the old path: {:?}", rep.problems);
    assert_eq!(w.upload_all(), 1);
    assert_eq!(w.pass().picked_up, 0);
}

#[test]
fn after_it_was_sent_a_deleted_file_stays_at_home_and_does_not_come_back() {
    let w = W::new("deleted-after");
    w.put("A/x.jpg", &photo("sent then deleted"));
    w.pass();
    w.upload_all();
    w.home_sync();
    std::fs::remove_file(w.photos().join("A/x.jpg")).unwrap();
    w.other.upload_step(&UploadBudget::default()).unwrap();
    w.pass();
    assert!(w.fx.src.join("A/x.jpg").is_file(), "the home PC keeps it");
    assert_eq!(w.pass().picked_up, 0);
}

#[test]
fn a_file_that_changed_after_it_was_queued_is_never_sent_as_it_was_found_and_is_found_again() {
    let w = W::new("changed");
    w.put("A/x.jpg", &photo("first bytes"));
    w.pass();
    let old_id = content_hash_bytes(&photo("first bytes"));
    // LightCraft rewrites the file (another size) before the upload
    w.put("A/x.jpg", &photo("second bytes, longer than the first"));
    w.spy.puts();
    assert_eq!(w.upload_all(), 0, "not sent");
    assert!(w.spy.puts().iter().all(|p| !p.contains("x.jpg")), "no bytes went up");
    assert!(w.queue().get(&old_id).is_none(), "the old entry is gone");
    // the new bytes are found, and sent
    assert_eq!(w.pass().picked_up, 1);
    let new_id = content_hash_bytes(&photo("second bytes, longer than the first"));
    assert!(w.queue().get(&new_id).is_some());
    assert_eq!(w.upload_all(), 1);
    assert_eq!(w.fx.rfile(&format!("imports/laptop/{new_id}/x.jpg")).unwrap(), photo("second bytes, longer than the first"));
    // an edit that keeps the size and the time is caught by the hash
    w.put("B/y.jpg", &photo("same size A"));
    w.pass();
    let p = w.photos().join("B/y.jpg");
    let when = std::fs::metadata(&p).unwrap().modified().unwrap();
    std::fs::write(&p, photo("same size B")).unwrap();
    std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(when).unwrap();
    assert_eq!(photo("same size A").len(), photo("same size B").len());
    assert_eq!(w.upload_all(), 0);
    assert!(w.fx.rfile(&format!("imports/laptop/{}/y.jpg", content_hash_bytes(&photo("same size A")))).is_none());
    assert_eq!(w.pass().picked_up, 1, "found again from the current bytes");
    assert!(w.queue().get(&content_hash_bytes(&photo("same size B"))).is_some());
}

#[test]
fn offline_a_new_photo_is_still_noticed_and_queued_and_nothing_is_sent() {
    let w = W::new("offline-find");
    w.offline(true);
    w.put("A/x.jpg", &photo("offline"));
    assert!(w.other.sync().is_err(), "the pass cannot reach the cloud library");
    assert_eq!(w.queue().list().len(), 1, "but the photo was noticed");
    assert!(w.other.upload_step(&UploadBudget::default()).is_err());
    assert!(w.fx.remote_tree().iter().all(|p| !p.starts_with("imports/")));
    w.offline(false);
    assert_eq!(w.upload_all(), 1);
}

// ------------------------------------------------------------------ the whole holiday

#[test]
fn a_holiday_end_to_end_files_in_nested_folders_offline_restart_adopted_at_the_same_paths_published_seen_and_a_home_delete_stays_deleted() {
    let mut w = W::new("holiday");
    let day1 = "2026/261101 - Lisbon/Day 1";
    // LightCraft imports into the synced folder, with no network at all
    w.offline(true);
    w.put(&format!("{day1}/IMG_1.jpg"), &photo("one"));
    w.put(&format!("{day1}/IMG_1.xmp"), &xmp(0.2));
    w.put(&format!("{day1}/IMG_2.ARW"), &photo("two"));
    w.put("2026/261101 - Lisbon/Day 2/IMG_3.jpg", &photo("three"));
    w.put("Loose.jpg", &photo("loose"));
    let _ = w.other.sync();
    assert_eq!(w.other.import_queue_status().to_upload, 4);
    assert!(w.other.upload_step(&UploadBudget::default()).is_err());
    // days pass: the app is restarted, the network is back
    let cfg = w.cfg();
    w.restart(cfg);
    assert_eq!(w.other.import_queue_status().to_upload, 4, "the queue survived");
    w.offline(false);
    assert_eq!(w.upload_all(), 4);
    // the home PC adopts them at the same nested paths and publishes
    w.home_sync();
    for rel in [format!("{day1}/IMG_1.jpg"), format!("{day1}/IMG_1.xmp"), format!("{day1}/IMG_2.ARW"), "2026/261101 - Lisbon/Day 2/IMG_3.jpg".to_string(), "Loose.jpg".to_string()] {
        assert!(w.fx.src.join(&rel).is_file(), "{rel} at home");
    }
    let m = w.fx.manifest();
    let paths: Vec<String> = m["photos"].as_array().unwrap().iter().map(|p| p["path"].as_str().unwrap().to_string()).collect();
    assert!(paths.contains(&format!("{day1}/IMG_1.jpg")) && paths.contains(&"Loose.jpg".to_string()), "{paths:?}");
    // this PC sees it: answers first, then the manifest; the folders are chosen now; no second copy anywhere
    let mut cfg = w.cfg();
    cfg.folders = vec![day1.into(), "2026/261101 - Lisbon/Day 2".into(), "".into()];
    w.restart(cfg);
    let before = w.tree(&w.photos());
    let r = w.other.upload_step(&UploadBudget::default()).unwrap();
    assert_eq!(r.results.iter().filter(|(_, s)| s == "added").count(), 4, "{r:?}");
    let rep = w.pass();
    assert_eq!((rep.downloaded.len(), rep.waiting_to_download, rep.picked_up), (0, 0, 0), "{rep:?}");
    assert!(rep.problems.is_empty(), "{:?}", rep.problems);
    assert_eq!(w.tree(&w.photos()), before);
    assert_eq!(w.other.status().unwrap().photos, 4);
    w.other.upload_step(&UploadBudget::default()).unwrap();
    assert!(w.queue().list().is_empty());
    // the home PC deletes one: it is moved aside here, and it does not go back up, however the folder is looked at
    std::fs::remove_file(w.fx.src.join(day1).join("IMG_2.ARW")).unwrap();
    w.home_sync();
    let rep = w.pass();
    assert_eq!(rep.removed, [format!("{day1}/IMG_2.ARW")], "{rep:?}");
    for _ in 0..2 {
        assert_eq!(w.pass().picked_up, 0);
        let cfg = w.cfg();
        w.restart(cfg);
    }
    assert_eq!(w.upload_all(), 0);
    w.home_sync();
    assert!(!w.fx.src.join(day1).join("IMG_2.ARW").exists(), "still deleted at home");
}
