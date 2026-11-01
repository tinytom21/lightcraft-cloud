//! The home PC end to end with the real engine, on whatever system the tests run on (CI runs it on Windows and Linux): generated photos
//! (LightCraft's procedural scenes, no real photos) in a temporary folder are published to a folder standing in for the cloud (through the
//! plain folder remote, and through the real librclone when `LIBRCLONE` names it), a phone edit in the inbox is applied, a photo another PC
//! uploaded is adopted, and another PC's code then reads what the home PC wrote. What it checks is the same everywhere; on Windows it is
//! the proof that paths, locks, renames and the engine work together: every path in the manifest uses `/` and the other PC's pass finds the
//! same photos, counted the way the home PC counts them.

#[path = "../examples/fixture.rs"]
mod fixture;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lightcraft_cloud_client::home::{Home, HomeConfig, content_hash_bytes};
use lightcraft_cloud_client::other::{Other, OtherConfig};
use lightcraft_cloud_client::{DirRemote, RcloneRemote, Remote};
use lightcraft_cloud_desktop::engine::EngineRenderer;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-desktop-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn sha16(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect::<String>()[..16].to_string()
}

fn manifest(remote_dir: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(remote_dir.join("mobile/manifest.json")).unwrap()).unwrap()
}

/// Every string value of a JSON document.
fn strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| strings(x, out)),
        _ => {}
    }
}

fn find<'a>(m: &'a Value, path: &str) -> &'a Value {
    m["photos"].as_array().unwrap().iter().find(|p| p["path"] == path).unwrap_or_else(|| panic!("{path} is not in the manifest"))
}

fn scenario(tag: &str, rclone: Option<&Path>) {
    let root = temp(tag);
    let (src, remote_dir, state) = (root.join("photos"), root.join("remote Dir"), root.join("state"));
    std::fs::create_dir_all(&remote_dir).unwrap();
    fixture::make(&src);
    let remote: Arc<dyn Remote> = match rclone {
        None => Arc::new(DirRemote::new(&remote_dir)),
        Some(so) => {
            let conf = root.join("rclone.conf");
            std::fs::write(&conf, "").unwrap();
            Arc::new(RcloneRemote::open(Some(so), &conf, &remote_dir.to_string_lossy(), None).unwrap())
        }
    };
    let mut cfg = HomeConfig::new(&src, &state, "e2e");
    cfg.exclude = vec!["exported".into()];
    let mut home = Home::new(cfg, remote.clone()).unwrap();
    let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let l = log.clone();
    home.set_logger(move |m| l.lock().unwrap().push(m.to_string()));
    let mut engine = EngineRenderer::open(&state.join("lib")).unwrap();

    // ---- 1. scan, render, mirror, manifest
    let rep = home.sync(&mut engine, false).unwrap();
    assert!(rep.failed.is_empty() && rep.problems.is_empty() && rep.inbox.problems.is_empty(), "{:?} {:?}", rep.failed, rep.problems);
    assert!(rep.manifest_uploaded);
    let m = manifest(&remote_dir);
    assert_eq!(m["format"], 1);
    assert_eq!(rep.photos, 11, "12 files, the copy listed once, the excluded folder left out, the pair as one photo");
    assert_eq!(m["photos"].as_array().unwrap().len(), 11);
    // every path in the manifest is `/`-separated, however this system spells paths
    let mut all = Vec::new();
    strings(&m, &mut all);
    assert!(all.iter().all(|s| !s.contains('\\')), "a backslash in the manifest: {:?}", all.iter().filter(|s| s.contains('\\')).collect::<Vec<_>>());
    assert!(all.iter().any(|s| s == "2024/Trip/PAIR_0001.JPG"), "nested paths use /");
    // the originals are mirrored byte for byte, under the same `/` paths; the thumbnails and previews are there
    for (rel, orig) in [("2024/Trip/IMG_0001.jpg", "2024/Trip/IMG_0001.jpg"), ("2024/Trip/PAIR_0001.DNG", "2024/Trip/PAIR_0001.DNG"), ("Other/IMG_0003.png", "Other/IMG_0003.png")] {
        assert_eq!(std::fs::read(remote_dir.join("originals").join(rel)).unwrap(), std::fs::read(src.join(orig)).unwrap(), "{rel}");
    }
    assert!(!remote_dir.join("originals/exported").exists());
    let e0 = find(&m, "Edits/E0.jpg").clone();
    let id = e0["id"].as_str().unwrap().to_string();
    let thumb_before = std::fs::read(remote_dir.join(format!("mobile/thumbs/{id}.jpg"))).unwrap();
    assert_eq!(&thumb_before[..2], &[0xFF, 0xD8]);
    assert!(std::fs::read(remote_dir.join(format!("mobile/previews/{id}.lcsp"))).unwrap().starts_with(b"LCSP1\n"));
    assert!(e0["sidecar"].is_null());

    // ---- 2. a phone edit in the inbox is applied to a sidecar in the photo folder, and the thumbnail follows
    let edit = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" xmp:Rating="4" crs:Exposure2012="+1.20"/></rdf:RDF></x:xmpmeta>
"#;
    let inbox_name = format!("{id}-20990101T000000Z-none.xmp");
    std::fs::create_dir_all(remote_dir.join("inbox/phone1")).unwrap();
    std::fs::write(remote_dir.join("inbox/phone1").join(&inbox_name), edit).unwrap();
    assert_eq!(home.inbox_waiting().unwrap(), 1);
    let rep = home.sync(&mut engine, false).unwrap();
    assert_eq!((rep.inbox.applied, rep.inbox.left, rep.inbox.rejected), (1, 0, 0), "{:?}", rep.inbox);
    assert!(rep.inbox.problems.is_empty() && rep.problems.is_empty());
    assert_eq!(std::fs::read(src.join("Edits/E0.xmp")).unwrap(), edit, "written into the photo folder");
    let m = manifest(&remote_dir);
    let e0 = find(&m, "Edits/E0.jpg");
    assert_eq!((e0["sidecar"].clone(), e0["sidecarVersion"].clone(), e0["rating"].clone()), (json!("Edits/E0.xmp"), json!(sha16(edit)), json!(4)));
    assert_ne!(std::fs::read(remote_dir.join(format!("mobile/thumbs/{id}.jpg"))).unwrap(), thumb_before, "rendered again with the edit");
    assert_eq!(std::fs::read(remote_dir.join("originals/Edits/E0.xmp")).unwrap(), edit, "and published");
    assert!(!remote_dir.join("inbox/phone1").join(&inbox_name).exists());
    assert!(std::fs::read_dir(remote_dir.join("inbox-done")).unwrap().next().is_some(), "the file is filed away, not lost");

    // ---- 3. a photo another PC uploaded is adopted into the folder and published by the same sync
    let bytes = fixture::jpeg(7, 320, 200);
    let imp_id = content_hash_bytes(&bytes);
    let dir = remote_dir.join(format!("imports/laptop/{imp_id}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("IMP_0001.jpg"), &bytes).unwrap();
    std::fs::write(
        remote_dir.join(format!("imports/laptop/{imp_id}.json")),
        json!({"format": 1, "id": imp_id, "folder": "Late/Day 1", "name": "IMP_0001.jpg", "size": bytes.len(), "mtime": "2026-10-01T10:00:00Z", "sidecar": null}).to_string(),
    )
    .unwrap();
    assert_eq!(home.imports_waiting().unwrap(), 1);
    let rep = home.sync(&mut engine, false).unwrap();
    assert_eq!((rep.imports.added.len(), rep.imports.rejected), (1, 0), "{:?} {:?}", rep.imports.items, rep.imports.errors);
    assert_eq!(rep.imports.added[0].path, "Late/Day 1/IMP_0001.jpg");
    assert_eq!(std::fs::read(src.join("Late").join("Day 1").join("IMP_0001.jpg")).unwrap(), bytes);
    let m = manifest(&remote_dir);
    assert_eq!(find(&m, "Late/Day 1/IMP_0001.jpg")["id"], json!(imp_id));
    assert_eq!(rep.photos, 12);
    assert_eq!(std::fs::read(remote_dir.join("originals/Late/Day 1/IMP_0001.jpg")).unwrap(), bytes);
    assert!(std::fs::read_dir(remote_dir.join("imports-done")).unwrap().next().is_some());
    // a quiet run changes nothing and renders nothing
    let rep = home.sync(&mut engine, false).unwrap();
    assert!(!rep.plan.any() && !rep.manifest_uploaded && rep.problems.is_empty(), "{:?}", rep.plan);

    // ---- 4. another PC reads what this home PC wrote, with the other-PC code
    let laptop = (root.join("laptop/photos"), root.join("laptop/state"));
    std::fs::create_dir_all(&laptop.0).unwrap();
    let chosen = ["Edits", "Late/Day 1", "2024/Trip"];
    let mut ocfg = OtherConfig::new(&laptop.0, &laptop.1, "laptop", chosen.iter().map(|s| s.to_string()).collect());
    ocfg.free_margin = 0;
    let other = Other::new(ocfg, remote.clone()).unwrap();
    let r = other.sync().unwrap();
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    // the photos of the chosen folders, counted the way the home PC counts them (a photo is a shot: the pair counts once) ...
    let m = manifest(&remote_dir);
    let home_count = m["photos"].as_array().unwrap().iter().filter(|p| chosen.iter().any(|c| p["path"].as_str().unwrap().rsplit_once('/').map(|(d, _)| d) == Some(*c))).count();
    assert_eq!(home_count, 11);
    assert_eq!((r.photos, r.waiting_to_download, r.downloaded_photos), (home_count, 0, home_count), "{r:?}");
    assert_eq!(other.status().unwrap().photos, home_count);
    // ... while the files are one more (the pair's raw)
    assert_eq!(r.downloaded.len(), 12);
    assert!(r.downloaded.iter().all(|p| !p.contains('\\')), "{:?}", r.downloaded);
    for p in &r.downloaded {
        assert!(laptop.0.join(p).is_file(), "{p}");
    }
    assert_eq!(std::fs::read(laptop.0.join("Edits/E0.xmp")).unwrap(), edit, "the phone's edit came through the home PC to this PC");
    assert_eq!(std::fs::read(laptop.0.join("Late/Day 1/IMP_0001.jpg")).unwrap(), bytes);
    let again = other.sync().unwrap();
    assert!(again.downloaded.is_empty() && again.problems.is_empty(), "{again:?}");

    // the scratch library lives in the state folder, and nothing of the app's is outside state, remote and the chosen places
    assert!(state.join("lib").is_dir());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_home_pc_publishes_applies_an_edit_adopts_an_import_and_another_pc_reads_it_over_a_folder_remote() {
    scenario("dir", None);
}

#[test]
fn the_same_through_the_real_librclone() {
    let Some(so) = std::env::var_os("LIBRCLONE").map(PathBuf::from).filter(|p| p.is_file()) else {
        eprintln!("skipped: set LIBRCLONE to a librclone");
        return;
    };
    scenario("rclone", Some(&so));
}
