//! The home side on Windows, where another program (LightCraft with a sidecar open, a virus scanner, the indexer) can hold a file so that
//! this app can neither replace nor read it. None of that happens on Linux, so these tests are Windows only (CI runs them on a real
//! runner; `common::Fx` is the same fixture the portable tests use). The rule they pin down: a held file is "later", never a failed sync,
//! never a file that looks deleted, and it is always shown to the user (`problems`), not only written to the log.
#![cfg(windows)]

mod common;

use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use common::*;
use lightcraft_cloud_client::home::SyncReport;
use serde_json::json;

const LATER: &str = "20990101T000000Z";

/// Open `path` the way a program that shares nothing does (a held handle without `FILE_SHARE_*`).
fn hold(path: &Path) -> std::fs::File {
    std::fs::OpenOptions::new().read(true).share_mode(0).open(path).unwrap()
}

/// Open `path` for reading and let others read it but not replace it: how a program looks at a sidecar it is about to write.
fn hold_for_reading(path: &Path) -> std::fs::File {
    std::fs::OpenOptions::new().read(true).share_mode(1).open(path).unwrap()
}

fn problems(r: &SyncReport) -> Vec<String> {
    r.inbox.problems.iter().chain(&r.problems).cloned().collect()
}

#[test]
fn a_sidecar_another_program_holds_does_not_stop_the_sync_the_edit_waits_in_the_inbox_and_is_applied_when_it_is_let_go() {
    let fx = Fx::new("win-busy-inbox");
    let mut r = Fake::default();
    fx.touch("A/one.jpg", b"one", 100);
    let old = sc().rating(1).build();
    fx.touch("A/one.xmp", &old, 100);
    fx.home.sync(&mut r, false).unwrap();
    let id = content_hash(b"one");
    let edited = sc().s(&[("light.exposure", j(0.7))]).rating(5).build();
    let name = format!("phone/{id}-{LATER}-{}.xmp", sha16(&old));
    fx.put_inbox(&name, &edited);
    // a photo that is new in the same sync must still be published
    fx.touch("A/two.jpg", b"two", 100);

    let held = hold_for_reading(&fx.src.join("A/one.xmp"));
    let rep = fx.home.sync(&mut r, false).expect("a held sidecar is not a failed sync");
    assert_eq!((rep.inbox.applied, rep.inbox.left), (0, 1), "{:?}", rep.inbox);
    let shown = problems(&rep);
    assert!(shown.iter().any(|p| p.contains("A/one.xmp") && p.contains("in use")), "the user is told, not only the log: {shown:?}");
    assert_eq!(std::fs::read(fx.src.join("A/one.xmp")).unwrap(), old, "the sidecar was not touched");
    assert!(fx.remote.join("inbox").join(&name).exists(), "the edit stays in the cloud inbox");
    assert!(!fx.remote_tree().iter().any(|p| p.starts_with("inbox-done/")), "and is not filed away");
    assert!(fx.manifest()["photos"].as_array().unwrap().iter().any(|p| p["path"] == "A/two.jpg"), "the rest of the sync went on");
    assert!(fx.home.inbox_waiting().unwrap() == 1);

    drop(held);
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!((rep.inbox.applied, rep.inbox.left), (1, 0), "{:?}", rep.inbox);
    assert!(problems(&rep).is_empty(), "{:?}", problems(&rep));
    assert_eq!(std::fs::read(fx.src.join("A/one.xmp")).unwrap(), edited);
    assert!(!fx.remote.join("inbox").join(&name).exists());
    assert!(fx.remote_tree().iter().any(|p| p.starts_with("inbox-done/") && p.ends_with(".xmp")));
    let one = fx.manifest()["photos"].as_array().unwrap().iter().find(|p| p["path"] == "A/one.jpg").cloned().unwrap();
    assert_eq!((one["sidecarVersion"].clone(), one["rating"].clone()), (json!(sha16(&edited)), json!(5)));
}

#[test]
fn a_sidecar_that_cannot_be_read_is_not_published_as_gone_and_its_photo_waits() {
    let fx = Fx::new("win-unreadable");
    let mut r = Fake::default();
    fx.touch("A/one.jpg", b"one", 100);
    let sidecar = sc().rating(2).build();
    fx.touch("A/one.xmp", &sidecar, 100);
    fx.home.sync(&mut r, false).unwrap();
    let before = fx.manifest()["photos"][0].clone();
    assert_eq!(before["sidecarVersion"], json!(sha16(&sidecar)));

    let held = hold(&fx.src.join("A/one.xmp"));
    let rep = fx.home.sync(&mut r, false).expect("not a failed sync");
    assert!(!rep.plan.any(), "its photo is left alone, not 'sidecar removed': {:?}", rep.plan);
    assert!(problems(&rep).iter().any(|p| p.contains("A/one.xmp") && p.contains("cannot be read")), "{:?}", problems(&rep));
    assert_eq!(fx.manifest()["photos"][0], before, "the published sidecar is still the sidecar");
    assert!(fx.rfile("originals/A/one.xmp").is_some(), "and still in the cloud");
    drop(held);
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert!(problems(&rep).is_empty(), "{:?}", problems(&rep));
}

#[test]
fn a_photo_another_program_holds_waits_for_the_next_sync_and_nothing_half_done_is_published() {
    let fx = Fx::new("win-busy-photo");
    let mut r = Fake::default();
    fx.touch("A/one.jpg", b"one", 100);
    let two = fx.touch("A/two.jpg", b"two", 100);
    let held = hold(&two);
    let rep = fx.home.sync(&mut r, false).expect("not a failed sync");
    assert!(rep.failed.is_empty(), "a held photo is not a photo that failed to import: {:?}", rep.failed);
    assert!(problems(&rep).iter().any(|p| p.contains("A/two.jpg") && p.contains("in use")), "{:?}", problems(&rep));
    let m = fx.manifest();
    let paths: Vec<&str> = m["photos"].as_array().unwrap().iter().map(|p| p["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["A/one.jpg"]);
    assert!(fx.rfile("originals/A/two.jpg").is_none());
    drop(held);
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!(rep.plan.new, ["A/two.jpg"]);
    assert!(problems(&rep).is_empty());
    assert!(fx.rfile("originals/A/two.jpg").is_some());
}

/// The manifest a Windows home writes has `/` in every path, however the folder was spelled.
#[test]
fn the_manifest_and_the_remote_use_forward_slashes_in_nested_folders() {
    let fx = Fx::new("win-slashes");
    let mut r = Fake::default();
    fx.touch("2024/Trip to Porto/Day 1/IMG_1.jpg", b"one", 100);
    fx.touch("2024/Trip to Porto/Day 1/IMG_1.xmp", &sc().build(), 100);
    fx.home.sync(&mut r, false).unwrap();
    let m = fx.manifest();
    let p = &m["photos"][0];
    assert_eq!(p["path"], "2024/Trip to Porto/Day 1/IMG_1.jpg");
    assert_eq!(p["sidecar"], "2024/Trip to Porto/Day 1/IMG_1.xmp");
    assert!(!m.to_string().contains("\\\\"), "no backslash anywhere in the manifest: {m}");
    assert!(fx.remote_tree().contains(&"originals/2024/Trip to Porto/Day 1/IMG_1.jpg".to_string()));
}

/// Windows does not tell `Pictures` from `PICTURES`, nor `C:\Users\me` from `\\?\C:\Users\me`: the state folder is refused in every spelling.
#[test]
fn the_state_folder_under_pictures_is_refused_in_any_spelling() {
    use lightcraft_cloud_client::home::{Home, HomeConfig};
    use lightcraft_cloud_client::DirRemote;
    use std::sync::Arc;
    let home = std::env::var("USERPROFILE").unwrap();
    for bad in [
        format!(r"{home}\Pictures\_x_state"),
        format!(r"{home}\PICTURES\_x_state"),
        format!(r"{}\pictures\_x_state", home.to_lowercase()),
        format!(r"\\?\{home}\Pictures\_x_state"),
        format!(r"{home}\Documents\..\Pictures\_x_state"),
        format!("{home}/Pictures/_x_state"),
    ] {
        let cfg = HomeConfig::new(r"C:\nowhere", &bad, "r");
        let e = Home::new(cfg, Arc::new(DirRemote::new(r"C:\nowhere-remote"))).err().unwrap_or_else(|| panic!("{bad} was accepted"));
        assert!(e.to_string().contains("Pictures"), "{bad}: {e}");
    }
    assert!(!std::path::Path::new(&format!(r"{home}\Pictures\_x_state")).exists());
    // a folder next to Pictures, whose name merely starts with it, is fine
    let ok = HomeConfig::new(r"C:\nowhere", format!(r"{home}\Pictures2\_x_state"), "r");
    assert!(Home::new(ok, Arc::new(DirRemote::new(r"C:\nowhere-remote"))).is_ok());
}

/// LightCraft is told about files by path. The sync works with `<source>\A/B/x.xmp` and the `\\?\` form `canonicalize` gives; what LightCraft is
/// handed is the plain way a person writes a path, one kind of separator, so that its own folder list matches it.
#[test]
fn what_lightcraft_is_told_is_a_plain_windows_path() {
    use std::sync::{Arc, Mutex};
    let mut fx = Fx::new("win-plain-paths");
    let told: Arc<Mutex<Vec<String>>> = Arc::default();
    let (t1, t2) = (told.clone(), told.clone());
    fx.home.set_sidecar_hook(move |p| {
        t1.lock().unwrap().push(p.to_string_lossy().into_owned());
        Ok(())
    });
    fx.home.set_adopted_hook(move |p| {
        t2.lock().unwrap().push(p.to_string_lossy().into_owned());
        Ok(())
    });
    let mut r = Fake::default();
    fx.touch("2024/Trip/one.jpg", b"one", 100);
    let old = sc().rating(1).build();
    fx.touch("2024/Trip/one.xmp", &old, 100);
    fx.home.sync(&mut r, false).unwrap();
    // a phone edit for the nested photo, and a photo another PC sent into a nested folder
    let id = content_hash(b"one");
    fx.put_inbox(&format!("phone/{id}-{LATER}-{}.xmp", sha16(&old)), &sc().rating(5).build());
    let bytes = b"imported photo bytes".to_vec();
    let imp = lightcraft_cloud_client::home::content_hash_bytes(&bytes);
    let dir = fx.remote.join(format!("imports/laptop/{imp}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("two.jpg"), &bytes).unwrap();
    std::fs::write(
        fx.remote.join(format!("imports/laptop/{imp}.json")),
        json!({"format": 1, "id": imp, "folder": "Late/Day 1", "name": "two.jpg", "size": bytes.len(), "mtime": "2026-10-01T10:00:00Z", "sidecar": null}).to_string(),
    )
    .unwrap();
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!((rep.inbox.applied, rep.imports.added.len()), (1, 1), "{:?} {:?}", rep.inbox, rep.imports.items);
    let told = told.lock().unwrap().clone();
    assert_eq!(told.len(), 2, "{told:?}");
    for t in &told {
        assert!(!t.contains('/') && !t.starts_with(r"\\?\"), "not a plain Windows path: {t}");
        assert!(std::path::Path::new(t).is_file(), "{t}");
    }
    assert!(told.iter().any(|t| t.ends_with(r"2024\Trip\one.xmp")) && told.iter().any(|t| t.ends_with(r"Late\Day 1\two.jpg")), "{told:?}");
}
