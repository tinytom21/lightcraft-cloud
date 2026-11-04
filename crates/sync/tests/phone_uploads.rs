//! Photos from the phone to the home PC: the phone's queue ([`CloudLibrary::enqueue_uploads`], [`CloudLibrary::upload_step`]) over a
//! folder standing in for the cloud, and end to end with the real `Home` adopting them into `Phone/<date>`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{Fx, HashRenderer, Spy};
use lightcraft_cloud_client::home::content_hash_bytes;
use lightcraft_cloud_client::other::UploadBudget;
use lightcraft_cloud_client::{CloudLibrary, Engine, UploadCandidate};
use serde_json::{Value, json};

const DATE: &str = "2026-03-14";

fn photo(tag: &str) -> Vec<u8> {
    format!("the bytes of phone photo {tag} ").repeat(60).into_bytes()
}

/// Answers like the engine would, for a library refresh.
#[derive(Default)]
struct FakeEngine {
    calls: Vec<(String, Value)>,
    smart: PathBuf,
}

impl Engine for FakeEngine {
    fn call(&mut self, command: &str, params: Value) -> Result<Value, String> {
        self.calls.push((command.to_string(), params.clone()));
        match command {
            "library.addRemote" => {
                let ids: serde_json::Map<String, Value> =
                    params["photos"].as_array().unwrap().iter().enumerate().map(|(i, p)| (p["id"].as_str().unwrap().to_string(), json!(i as u64 + 1))).collect();
                Ok(json!({"added": ids.len(), "updated": 0, "removed": 0, "ids": ids}))
            }
            "photo.sidecarPaths" => {
                let add = self.calls.iter().rev().find(|(c, _)| c == "library.addRemote").unwrap().1.clone();
                let root = add["root"].as_str().unwrap().to_string();
                let photos = add["photos"].as_array().unwrap().clone();
                let paths: Vec<Value> = params["ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| {
                        let p = photos[i.as_u64().unwrap() as usize - 1]["path"].as_str().unwrap();
                        let path = format!("{root}/{}", Path::new(p).with_extension("xmp").display());
                        json!({"id": i, "exists": Path::new(&path).exists(), "path": path})
                    })
                    .collect();
                Ok(json!({"paths": paths}))
            }
            "photo.readMetadataFromFile" => Ok(json!({"read": [], "unchanged": [], "failed": []})),
            "library.smartPreviewsLocation" => Ok(json!({"path": self.smart.to_string_lossy()})),
            other => Err(format!("unexpected {other}")),
        }
    }
}

struct World {
    fx: Fx,
    spy: Arc<Spy>,
    phone_dir: PathBuf,
    lib: CloudLibrary,
    n: usize,
}

fn open(dir: &Path, spy: &Arc<Spy>) -> CloudLibrary {
    let mut lib = CloudLibrary::open(dir, spy.clone(), "test:", "pixel-7").unwrap();
    lib.set_retry_delay(Duration::ZERO);
    lib
}

impl World {
    fn new(tag: &str) -> World {
        let fx = Fx::new(tag);
        let spy = Spy::new(&fx.remote);
        let phone_dir = fx.root.join("phone");
        std::fs::create_dir_all(&phone_dir).unwrap();
        let lib = open(&phone_dir, &spy);
        World { fx, spy, phone_dir, lib, n: 0 }
    }

    /// The app was killed and started again.
    fn restart(&mut self) {
        self.lib = open(&self.phone_dir, &self.spy);
    }

    /// A copy the app made of a picked photo.
    fn cand(&mut self, name: &str, bytes: &[u8], date: &str) -> UploadCandidate {
        self.n += 1;
        let path = self.lib.upload_staging_dir().join(format!("{}-{name}", self.n));
        std::fs::write(&path, bytes).unwrap();
        UploadCandidate { path, name: name.into(), date: date.into(), taken_ms: 1_773_500_000_000 }
    }

    fn home_sync(&self) {
        self.fx.home.sync(&mut HashRenderer::default(), false).unwrap_or_else(|e| panic!("home sync: {e}"));
    }

    fn refresh(&self) {
        let mut eng = FakeEngine { smart: self.fx.root.join("smart"), ..FakeEngine::default() };
        self.lib.refresh(&mut eng).unwrap_or_else(|e| panic!("phone refresh: {e}"));
    }

    fn step(&self) -> lightcraft_cloud_client::UploadStep {
        self.lib.upload_step(&UploadBudget::default()).unwrap_or_else(|e| panic!("upload step: {e}"))
    }

    fn queue_files(&self) -> Vec<String> {
        self.fx.tree(&self.phone_dir.join("uploads")).into_iter().filter(|p| !p.starts_with("staging/")).collect()
    }

    fn imports(&self) -> Vec<String> {
        self.fx.remote_tree().into_iter().filter(|p| p.starts_with("imports/")).collect()
    }
}

fn id_of(bytes: &[u8]) -> String {
    content_hash_bytes(bytes)
}

#[test]
fn only_photos_the_home_pc_reads_are_queued_heic_is_counted_and_a_bad_date_is_refused() {
    let mut w = World::new("pu-rules");
    let cands = vec![
        w.cand("IMG_1.jpg", &photo("1"), DATE),
        w.cand("IMG_2.JPEG", &photo("2"), DATE),
        w.cand("IMG_3.png", &photo("3"), DATE),
        w.cand("IMG_4.tif", &photo("4"), DATE),
        w.cand("IMG_5.DNG", &photo("5"), DATE),
        w.cand("IMG_6.ARW", &photo("6"), DATE),
        w.cand("IMG_7.heic", &photo("7"), DATE),
        w.cand("IMG_8.HEIF", &photo("8"), DATE),
        w.cand("IMG_9.webp", &photo("9"), DATE),
        w.cand("IMG_10.gif", &photo("10"), DATE),
        w.cand("IMG_11.jpg", &photo("11"), "14/03/2026"),
        w.cand("empty.jpg", b"", DATE),
        w.cand("IMG_12.jpg", &photo("1"), DATE), // the same bytes as IMG_1
    ];
    let paths: Vec<PathBuf> = cands.iter().map(|c| c.path.clone()).collect();
    let r = w.lib.enqueue_uploads(&cands).unwrap();
    assert_eq!((r.queued, r.skipped_heic, r.skipped_other, r.already_queued, r.known), (6, 2, 2, 1, 0), "{r:?}");
    assert_eq!(r.failed.len(), 2, "{r:?}");
    assert!(r.failed[0].contains("IMG_11.jpg") && r.failed[1].contains("empty.jpg"), "{r:?}");
    assert!(paths.iter().all(|p| !p.exists()), "every file handed over is taken: moved into the queue or deleted");
    let s = w.lib.upload_status();
    assert_eq!((s.total, s.waiting, s.skipped_heic, s.skipped_other), (6, 6, 2, 2), "{s:?}");
    assert_eq!(s.bytes_total, [photo("1"), photo("2"), photo("3"), photo("4"), photo("5"), photo("6")].iter().map(|p| p.len() as u64).sum::<u64>());
    // nothing is on the remote until a step runs
    assert!(w.imports().is_empty());
}

#[test]
fn a_photo_is_sent_with_the_bytes_first_and_the_note_last_into_phone_date() {
    let mut w = World::new("pu-send");
    let bytes = photo("A");
    let c = w.cand("IMG_0001.jpg", &bytes, DATE);
    w.lib.enqueue_uploads(&[c]).unwrap();
    let step = w.step();
    assert_eq!((step.committed, step.files_sent, step.awaiting_home), (1, 1, 1), "{step:?}");
    let id = id_of(&bytes);
    let puts = w.spy.puts();
    assert_eq!(puts, [format!("imports/pixel-7/{id}/IMG_0001.jpg"), format!("imports/pixel-7/{id}.json")], "the note is last");
    let note: Value = serde_json::from_slice(&w.fx.rfile(&format!("imports/pixel-7/{id}.json")).unwrap()).unwrap();
    assert_eq!(note["folder"], format!("Phone/{DATE}"));
    assert_eq!((note["format"].clone(), note["name"].clone(), note["size"].clone(), note["sidecar"].clone()), (json!(1), json!("IMG_0001.jpg"), json!(bytes.len()), Value::Null));
    assert_eq!(note["id"], id);
    assert!(note["mtime"].as_str().unwrap().starts_with("2026-03-14T"), "{note}");
    assert_eq!(w.fx.rfile(&format!("imports/pixel-7/{id}/IMG_0001.jpg")).unwrap(), bytes);
    let s = w.lib.upload_status();
    assert_eq!((s.waiting, s.awaiting_home, s.added), (0, 1, 0), "{s:?}");
    // the copy stays until the home PC has answered
    assert!(w.queue_files().contains(&format!("{id}.photo")), "{:?}", w.queue_files());
    // another step with nothing new sends nothing
    let step = w.step();
    assert_eq!((step.committed, step.files_sent), (0, 0));
    assert!(w.spy.puts().is_empty());
}

#[test]
fn the_work_per_step_is_bounded_and_the_rest_waits_for_the_next() {
    let mut w = World::new("pu-budget");
    let cands: Vec<_> = (0..5).map(|i| w.cand(&format!("IMG_{i}.jpg"), &photo(&format!("b{i}")), DATE)).collect();
    w.lib.enqueue_uploads(&cands).unwrap();
    let small = UploadBudget { max_files: 2, ..UploadBudget::default() };
    let a = w.lib.upload_step(&small).unwrap();
    assert_eq!((a.committed, a.more), (2, true), "{a:?}");
    let b = w.lib.upload_step(&small).unwrap();
    assert_eq!((b.committed, b.more), (2, true), "{b:?}");
    let c = w.lib.upload_step(&small).unwrap();
    assert_eq!((c.committed, c.more), (1, false), "{c:?}");
    assert_eq!(c.status.waiting, 0);
    assert_eq!(c.status.awaiting_home, 5);
    // a byte budget stops after the file that crossed it
    let mut w = World::new("pu-budget-bytes");
    let cands: Vec<_> = (0..3).map(|i| w.cand(&format!("IMG_{i}.jpg"), &photo(&format!("c{i}")), DATE)).collect();
    w.lib.enqueue_uploads(&cands).unwrap();
    let tiny = UploadBudget { max_bytes: 1, ..UploadBudget::default() };
    let a = w.lib.upload_step(&tiny).unwrap();
    assert_eq!((a.committed, a.more), (1, true), "{a:?}");
}

#[test]
fn a_killed_upload_resumes_and_what_is_already_there_at_the_right_size_is_not_sent_again() {
    let mut w = World::new("pu-resume");
    let (a, b) = (photo("ra"), photo("rb"));
    let cands = vec![w.cand("A.jpg", &a, DATE), w.cand("B.jpg", &b, DATE)];
    w.lib.enqueue_uploads(&cands).unwrap();
    // the first photo's bytes and note go; the second photo's bytes fail (offline), the step fails with a remote error
    w.spy.fail_puts.store(0, Ordering::SeqCst);
    let first = w.lib.upload_step(&UploadBudget { max_files: 1, ..UploadBudget::default() }).unwrap();
    assert_eq!(first.committed, 1);
    w.spy.puts();
    w.spy.fail_puts.store(3, Ordering::SeqCst);
    let err = w.lib.upload_step(&UploadBudget::default()).unwrap_err();
    assert!(matches!(err, lightcraft_cloud_client::Error::Remote(_)), "{err}");
    let s = w.lib.upload_status();
    assert_eq!((s.waiting, s.awaiting_home), (1, 1), "what was sent stays recorded: {s:?}");
    // the app is killed with the bytes of B on the remote but the commit recorded nowhere: simulate with the file there already
    let id_b = id_of(&b);
    let p = w.fx.remote.join(format!("imports/pixel-7/{id_b}/B.jpg"));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, &b).unwrap();
    w.restart();
    w.spy.fail_puts.store(0, Ordering::SeqCst);
    w.spy.puts();
    let step = w.step();
    assert_eq!(step.committed, 1);
    assert_eq!(w.spy.puts(), [format!("imports/pixel-7/{id_b}.json")], "the bytes were there at the right size: only the note goes");
    // a file there with the wrong size (a torn upload) is sent again
    let mut w = World::new("pu-torn");
    let c = w.cand("T.jpg", &a, DATE);
    w.lib.enqueue_uploads(&[c]).unwrap();
    let id = id_of(&a);
    let p = w.fx.remote.join(format!("imports/pixel-7/{id}/T.jpg"));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, &a[..100]).unwrap();
    w.step();
    assert_eq!(w.fx.rfile(&format!("imports/pixel-7/{id}/T.jpg")).unwrap(), a);
    // the queue survives a restart before any sending
    let mut w = World::new("pu-restart");
    let c = w.cand("R.jpg", &a, DATE);
    w.lib.enqueue_uploads(&[c]).unwrap();
    w.restart();
    assert_eq!(w.lib.upload_status().waiting, 1);
    assert_eq!(w.step().committed, 1);
}

#[test]
fn the_same_photo_offered_twice_is_queued_once() {
    let mut w = World::new("pu-twice");
    let bytes = photo("tw");
    let c = w.cand("X.jpg", &bytes, DATE);
    assert_eq!(w.lib.enqueue_uploads(&[c]).unwrap().queued, 1);
    let c = w.cand("X (1).jpg", &bytes, "2026-03-15");
    let r = w.lib.enqueue_uploads(&[c]).unwrap();
    assert_eq!((r.queued, r.already_queued), (0, 1));
    assert_eq!(w.lib.upload_status().total, 1);
}

#[test]
fn awkward_file_names_are_made_safe() {
    let mut w = World::new("pu-names");
    let c = w.cand("holiday: day 1?.jpg", &photo("n1"), DATE);
    let c2 = w.cand(".hidden.jpg", &photo("n2"), DATE);
    let r = w.lib.enqueue_uploads(&[c, c2]).unwrap();
    assert_eq!(r.queued, 2, "{r:?}");
    w.step();
    let names: Vec<String> = w.imports().into_iter().filter(|p| p.matches('/').count() == 3).collect();
    assert!(names.iter().any(|n| n.ends_with("/holiday_ day 1_.jpg")), "{names:?}");
    assert!(names.iter().any(|n| n.ends_with("/hidden.jpg")), "{names:?}");
}

#[test]
fn the_home_pc_adopts_into_phone_date_and_the_phone_sees_results_and_the_new_photo() {
    let mut w = World::new("pu-e2e");
    // the home PC has published a photo; the phone has it in its manifest
    let published = photo("published");
    w.fx.touch("Trip/Seed.jpg", &published, 3600);
    w.home_sync();
    w.refresh();
    assert!(w.lib.has_photo(&id_of(&published)));

    let fresh = photo("fresh");
    let heic = photo("heic");
    let stale = photo("stale"); // the home PC has it, but the phone has not heard yet
    w.fx.touch("Elsewhere/Stale.jpg", &stale, 3600);
    w.home_sync();
    let cands = vec![
        w.cand("IMG_NEW.jpg", &fresh, DATE),
        w.cand("IMG_PUBLISHED.jpg", &published, DATE), // a duplicate of a published photo
        w.cand("IMG_HEIC.heic", &heic, DATE),
        w.cand("IMG_STALE.jpg", &stale, DATE),
    ];
    let r = w.lib.enqueue_uploads(&cands).unwrap();
    assert_eq!((r.queued, r.known, r.skipped_heic), (2, 1, 1), "{r:?}");
    let s = w.lib.upload_status();
    assert_eq!((s.total, s.waiting, s.known, s.skipped_heic), (2, 2, 1, 1), "{s:?}");

    assert_eq!(w.step().committed, 2);
    // the home PC adopts what it was sent in its next sync: the new one into Phone/<date>, the stale one is a duplicate
    w.home_sync();
    assert_eq!(std::fs::read(w.fx.src.join(format!("Phone/{DATE}/IMG_NEW.jpg"))).unwrap(), fresh);
    assert!(!w.fx.src.join(format!("Phone/{DATE}/IMG_STALE.jpg")).exists());
    assert!(!w.fx.src.join(format!("Phone/{DATE}/IMG_PUBLISHED.jpg")).exists());
    assert!(!w.fx.src.join(format!("Phone/{DATE}/IMG_HEIC.heic")).exists());

    // the phone reads the answers and deletes its copies
    let step = w.step();
    let mut results = step.results.clone();
    results.sort();
    let mut want = vec![(id_of(&fresh), "added".to_string()), (id_of(&stale), "duplicate".to_string())];
    want.sort();
    assert_eq!(results, want, "{step:?}");
    let s = w.lib.upload_status();
    assert_eq!((s.added, s.duplicate, s.waiting, s.awaiting_home, s.rejected.len()), (1, 1, 0, 0, 0), "{s:?}");
    assert_eq!((s.skipped_heic, s.known), (1, 1));
    assert_eq!(w.queue_files(), ["tally.json"], "the copies are deleted: {:?}", w.queue_files());
    assert_eq!(w.lib.uploads_pending(), 0);

    // the photo is in the manifest after the home PC publishes (the same sync adopted and published it) and the phone refreshes
    assert!(!w.lib.has_photo(&id_of(&fresh)), "the phone has not refreshed yet");
    w.refresh();
    assert!(w.lib.has_photo(&id_of(&fresh)));
    let manifest = w.fx.manifest();
    let item = manifest["photos"].as_array().unwrap().iter().find(|p| p["id"] == id_of(&fresh)).unwrap();
    assert_eq!(item["path"], format!("Phone/{DATE}/IMG_NEW.jpg"));
    // offering it again: the library has it, nothing is queued
    let again = w.cand("IMG_NEW.jpg", &fresh, DATE);
    let r = w.lib.enqueue_uploads(&[again]).unwrap();
    assert_eq!((r.queued, r.known), (0, 1));
    // a finished batch shows until it is cleared
    w.lib.clear_upload_report().unwrap();
    assert!(w.lib.upload_status().is_empty(), "{:?}", w.lib.upload_status());
}

#[test]
fn photos_from_two_days_go_to_two_folders_and_a_name_clash_gets_a_number() {
    let mut w = World::new("pu-days");
    w.fx.touch("Phone/2026-03-14/IMG_1.jpg", &photo("other bytes"), 3600);
    w.home_sync();
    let (a, b) = (photo("d1"), photo("d2"));
    let cands = vec![w.cand("IMG_1.jpg", &a, "2026-03-14"), w.cand("IMG_1.jpg", &b, "2026-03-15")];
    w.lib.enqueue_uploads(&cands).unwrap();
    w.step();
    w.home_sync();
    assert_eq!(std::fs::read(w.fx.src.join("Phone/2026-03-14/IMG_1-2.jpg")).unwrap(), a, "the existing photo is never overwritten");
    assert_eq!(std::fs::read(w.fx.src.join("Phone/2026-03-15/IMG_1.jpg")).unwrap(), b);
    assert_eq!(std::fs::read(w.fx.src.join("Phone/2026-03-14/IMG_1.jpg")).unwrap(), photo("other bytes"));
}

#[test]
fn a_refusal_is_shown_with_its_reason_and_offering_the_photo_again_sends_it_anew() {
    let mut w = World::new("pu-reject");
    // the home PC excludes the Phone folder
    let mut cfg = w.fx.home.config().clone();
    cfg.exclude = vec!["Phone".into()];
    let mut home = lightcraft_cloud_client::home::Home::new(cfg.clone(), Arc::new(lightcraft_cloud_client::DirRemote::new(&w.fx.remote))).unwrap();
    home.set_retry_delay(Duration::ZERO);
    w.fx.home = home;
    w.fx.touch("Seed/seed.jpg", b"the photo that was here before", 100);
    let bytes = photo("rej");
    let c = w.cand("R.jpg", &bytes, DATE);
    w.lib.enqueue_uploads(&[c]).unwrap();
    w.step();
    w.home_sync();
    let step = w.step();
    assert_eq!(step.results, [(id_of(&bytes), "rejected".to_string())]);
    let s = w.lib.upload_status();
    assert_eq!(s.rejected.len(), 1, "{s:?}");
    assert_eq!(s.rejected[0].name, "R.jpg");
    assert!(s.rejected[0].reason.contains("exclude"), "{:?}", s.rejected);
    assert_eq!((s.waiting, s.awaiting_home), (0, 0));
    assert_eq!(w.queue_files().iter().filter(|f| f.ends_with(".photo")).count(), 0, "the copy is deleted");
    assert_eq!(w.lib.uploads_pending(), 0);
    // offered again, it is queued again (the home PC may have changed its mind)
    let again = w.cand("R.jpg", &bytes, DATE);
    let r = w.lib.enqueue_uploads(&[again]).unwrap();
    assert_eq!(r.queued, 1, "{r:?}");
    let s = w.lib.upload_status();
    assert_eq!((s.waiting, s.rejected.len()), (1, 0), "{s:?}");
}

#[test]
fn nothing_queued_is_nothing_to_show_and_a_step_does_no_network() {
    let w = World::new("pu-empty");
    assert!(w.lib.upload_status().is_empty());
    let step = w.lib.upload_step(&UploadBudget::default()).unwrap();
    assert_eq!((step.committed, step.files_sent, step.awaiting_home), (0, 0, 0));
    assert!(w.spy.puts().is_empty());
}
