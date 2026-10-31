//! The real engine behind the sync: generated photos (LightCraft's procedural scenes, no real photos) published to a folder
//! standing in for the remote. Rendering only: the source folder is never written, and no LightCraft library is opened.

#[path = "../examples/fixture.rs"]
mod fixture;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use lightcraft_cloud_client::DirRemote;
use lightcraft_cloud_client::home::{Home, HomeConfig};
use lightcraft_cloud_desktop::engine::{EngineRenderer, check_library_path};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-desktop-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn files(root: &Path) -> Vec<String> {
    fn walk(base: &Path, d: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                walk(base, &e.path(), out);
            } else {
                out.push(e.path().strip_prefix(base).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut v = Vec::new();
    walk(root, root, &mut v);
    v.sort();
    v
}

fn tree_hash(root: &Path) -> String {
    let mut h = Sha256::new();
    for p in files(root) {
        h.update(p.as_bytes());
        h.update(std::fs::read(root.join(&p)).unwrap());
        h.update(
            std::fs::metadata(root.join(&p)).unwrap().modified().unwrap().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos().to_le_bytes(),
        );
    }
    format!("{:x}", h.finalize())
}

fn jpeg_size(d: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 9 < d.len() {
        if d[i] != 0xFF {
            i += 1;
            continue;
        }
        if matches!(d[i + 1], 0xC0..=0xC2) {
            return Some((u32::from(d[i + 7]) << 8 | u32::from(d[i + 8]), u32::from(d[i + 5]) << 8 | u32::from(d[i + 6])));
        }
        i += 2 + (usize::from(d[i + 2]) << 8 | usize::from(d[i + 3]));
    }
    None
}

struct Env {
    root: PathBuf,
    src: PathBuf,
    remote: PathBuf,
    state: PathBuf,
    home: Home,
}

fn env(tag: &str) -> (Env, EngineRenderer) {
    let root = temp(tag);
    let (src, remote, state) = (root.join("src"), root.join("remote"), root.join("state"));
    fixture::make(&src);
    let mut cfg = HomeConfig::new(&src, &state, "test");
    cfg.exclude = vec!["exported".into()];
    let mut home = Home::new(cfg, Arc::new(DirRemote::new(&remote))).unwrap();
    home.set_logger(|_| {});
    let r = EngineRenderer::open(&state.join("lib")).unwrap();
    (Env { root, src, remote, state, home }, r)
}

fn manifest(e: &Env) -> Value {
    serde_json::from_slice(&std::fs::read(e.remote.join("mobile/manifest.json")).unwrap()).unwrap()
}

#[test]
fn thumbnails_previews_and_manifest_come_out_of_the_engine() {
    let (e, mut r) = env("render");
    let before = tree_hash(&e.src);
    let rep = e.home.sync(&mut r, false).unwrap();
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    // the source folder is not written to: no sidecars, no marker files, same mtimes
    assert_eq!(tree_hash(&e.src), before);
    let m = manifest(&e);
    let photos = m["photos"].as_array().unwrap();
    // 12 photo files listed: the identical copy once (under the first), the excluded folder left out, the pair as one photo
    assert_eq!(photos.len(), 11, "{:#?}", photos.iter().map(|p| p["path"].clone()).collect::<Vec<_>>());
    let pair = photos.iter().find(|p| p["path"] == "2024/Trip/PAIR_0001.JPG").unwrap();
    assert_eq!(pair["alternates"][0]["path"], "2024/Trip/PAIR_0001.DNG");
    assert!(photos.iter().all(|p| p["path"] != "Other/IMG_0001_copy.jpg" && p["path"] != "exported/skip.jpg"));
    for p in photos.iter().chain(photos.iter().flat_map(|p| p["alternates"].as_array().into_iter().flatten())) {
        let id = p["id"].as_str().unwrap();
        assert_eq!(id.len(), 32);
        // the thumbnail: a JPEG, at most 512 px on the long edge, the photo's aspect ratio
        let t = std::fs::read(e.remote.join(format!("mobile/thumbs/{id}.jpg"))).unwrap();
        assert_eq!(&t[..2], &[0xFF, 0xD8], "{id}");
        let (w, h) = jpeg_size(&t).unwrap();
        assert!(w.max(h) <= 512, "{w}x{h}");
        let (pw, ph) = (p["width"].as_u64().unwrap() as f64, p["height"].as_u64().unwrap() as f64);
        assert!(((w as f64 / h as f64) - pw / ph).abs() < 0.02, "{w}x{h} vs {pw}x{ph}");
        // the smart preview: `LCSP1`, a JSON header, a JPEG; its name in the manifest is the engine's
        let lcsp = std::fs::read(e.remote.join(format!("mobile/previews/{id}.lcsp"))).unwrap();
        assert!(lcsp.starts_with(b"LCSP1\n"), "{id}");
        assert_eq!(p["previewBytes"].as_u64().unwrap(), lcsp.len() as u64);
        let key = p["previewKey"].as_str().unwrap();
        assert_eq!(key.len(), 32);
        assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(key, id, "the engine names a preview by a hash of the content hash");
    }
    // the exposure edit in IMG_0002.xmp is in its thumbnail: the same photo without the sidecar renders differently
    let edited = photos.iter().find(|p| p["path"] == "2024/Trip/IMG_0002.jpg").unwrap();
    assert_eq!(edited["sidecar"], "2024/Trip/IMG_0002.xmp");
    assert_eq!(edited["rating"], 3);
    let id = edited["id"].as_str().unwrap();
    let with_edit = std::fs::read(e.remote.join(format!("mobile/thumbs/{id}.jpg"))).unwrap();
    let plain = temp("render-plain");
    let state2 = plain.join("state");
    std::fs::create_dir_all(plain.join("src/2024/Trip")).unwrap();
    std::fs::copy(e.src.join("2024/Trip/IMG_0002.jpg"), plain.join("src/2024/Trip/IMG_0002.jpg")).unwrap();
    let old = SystemTime::now() - Duration::from_secs(3600);
    std::fs::OpenOptions::new().write(true).open(plain.join("src/2024/Trip/IMG_0002.jpg")).unwrap().set_modified(old).unwrap();
    let mut home2 = Home::new(HomeConfig::new(plain.join("src"), &state2, "t2"), Arc::new(DirRemote::new(plain.join("remote")))).unwrap();
    home2.set_logger(|_| {});
    let mut r2 = EngineRenderer::open(&state2.join("lib")).unwrap();
    home2.sync(&mut r2, false).unwrap();
    let without = std::fs::read(plain.join(format!("remote/mobile/thumbs/{id}.jpg"))).unwrap();
    assert_ne!(with_edit, without, "the sidecar's exposure edit should show in the thumbnail");
    assert_eq!(jpeg_size(&with_edit), jpeg_size(&without));
    // the scratch library lives in the state folder, and nothing else of the app's is outside state and remote
    assert!(e.state.join("lib").is_dir());
    assert!(files(&e.root).iter().all(|f| f.starts_with("src/") || f.starts_with("remote/") || f.starts_with("state/")));
    let _ = std::fs::remove_dir_all(&e.root);
    let _ = std::fs::remove_dir_all(&plain);
}

#[test]
fn changes_edits_and_removals_follow_through_the_engine() {
    let (e, mut r) = env("follow");
    e.home.sync(&mut r, false).unwrap();
    let m1 = manifest(&e);
    let find = |m: &Value, path: &str| m["photos"].as_array().unwrap().iter().find(|p| p["path"] == path).cloned();
    let before = find(&m1, "Edits/E0.jpg").unwrap();
    // a sidecar is written by the user: the thumbnail is made again, the preview and the id are kept
    let thumb_before = std::fs::read(e.remote.join(format!("mobile/thumbs/{}.jpg", before["id"].as_str().unwrap()))).unwrap();
    let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" xmp:Rating="5" crs:Exposure2012="-1.50"/></rdf:RDF></x:xmpmeta>"#;
    let p = e.src.join("Edits/E0.xmp");
    std::fs::write(&p, xmp).unwrap();
    std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(SystemTime::now() - Duration::from_secs(600)).unwrap();
    let rep = e.home.sync(&mut r, false).unwrap();
    assert_eq!(rep.plan.sc_changed, ["Edits/E0.jpg"]);
    let after = find(&manifest(&e), "Edits/E0.jpg").unwrap();
    assert_eq!(
        (after["rating"].clone(), after["id"].clone(), after["previewKey"].clone()),
        (Value::from(5), before["id"].clone(), before["previewKey"].clone())
    );
    assert_ne!(after["sidecarVersion"], before["sidecarVersion"]);
    let thumb_after = std::fs::read(e.remote.join(format!("mobile/thumbs/{}.jpg", before["id"].as_str().unwrap()))).unwrap();
    assert_ne!(thumb_before, thumb_after);
    // a photo removed from the source leaves the manifest, and the scratch library forgets it
    std::fs::remove_file(e.src.join("Edits/E1.jpg")).unwrap();
    e.home.sync(&mut r, false).unwrap();
    assert!(find(&manifest(&e), "Edits/E1.jpg").is_none());
    // the file on disk was not deleted by the engine's catalog operation (it is already gone here), and a new run is quiet
    let rep = e.home.sync(&mut r, false).unwrap();
    assert!(!rep.plan.any() && !rep.manifest_uploaded);
    // a fresh process (a new renderer on the same scratch library) knows the photos: nothing is imported again
    drop(r);
    let mut r2 = EngineRenderer::open(&e.state.join("lib")).unwrap();
    let q = e.src.join("Edits/E2.xmp");
    std::fs::write(&q, xmp.replace("-1.50", "+0.50")).unwrap();
    std::fs::OpenOptions::new().write(true).open(&q).unwrap().set_modified(SystemTime::now() - Duration::from_secs(600)).unwrap();
    let rep = e.home.sync(&mut r2, false).unwrap();
    assert_eq!(rep.plan.sc_changed, ["Edits/E2.jpg"]);
    let _ = std::fs::remove_dir_all(&e.root);
}

#[test]
fn the_scratch_library_is_never_a_users_library() {
    let home = std::env::var("HOME").unwrap();
    for bad in [
        format!("{home}/Pictures/LightCraft Library"),
        format!("{home}/Pictures/anything/lib"),
        "/data/LightCraft Library".to_string(),
        "/tmp/x/LightCraft Library/lib".to_string(),
    ] {
        assert!(check_library_path(Path::new(&bad)).is_err(), "{bad}");
        assert!(EngineRenderer::open(Path::new(&bad)).is_err(), "{bad}");
    }
    assert!(!Path::new(&format!("{home}/Pictures/anything")).exists());
    assert!(check_library_path(Path::new("/tmp/lc-ok/state/lib")).is_ok());
}
