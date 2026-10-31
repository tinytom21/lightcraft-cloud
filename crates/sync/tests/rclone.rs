//! `RcloneRemote` through the real librclone (host build: `apps/lightcraft-android/scripts/build-librclone.sh host` in the
//! lightcraft-android repo). Set `LIBRCLONE` to the path of `librclone.so`; skipped when it is not set.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Fake, sc, touch};
use lightcraft_cloud_client::home::{Home, HomeConfig};
use lightcraft_cloud_client::{Error, RcloneRemote, Remote};

fn so() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("LIBRCLONE")?);
    p.is_file().then_some(p)
}

#[test]
fn rclone_remote_lists_gets_and_puts_in_a_local_folder() {
    let Some(so) = so() else {
        eprintln!("skipped: set LIBRCLONE to a librclone.so");
        return;
    };
    let base = std::env::temp_dir().join(format!("lc-cloud-rclone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let remote_dir = base.join("remote Dir");
    std::fs::create_dir_all(remote_dir.join("mobile/thumbs")).unwrap();
    std::fs::write(remote_dir.join("mobile/manifest.json"), b"{}").unwrap();
    std::fs::write(remote_dir.join("mobile/thumbs/a.jpg"), b"12345").unwrap();
    let conf = base.join("rclone.conf");
    std::fs::write(&conf, "").unwrap();
    let r = RcloneRemote::open(Some(&so), &conf, &remote_dir.to_string_lossy(), None).unwrap();

    let mut top: Vec<_> = r.list("mobile", false).unwrap().into_iter().map(|e| (e.path, e.is_dir, e.size)).collect();
    top.sort();
    assert_eq!(top, vec![("manifest.json".to_string(), false, 2), ("thumbs".to_string(), true, 0)]);
    let mut all: Vec<_> = r.list("", true).unwrap().into_iter().filter(|e| !e.is_dir).map(|e| (e.path, e.size)).collect();
    all.sort();
    assert_eq!(all, vec![("mobile/manifest.json".to_string(), 2), ("mobile/thumbs/a.jpg".to_string(), 5)]);
    assert!(matches!(r.list("nope", false), Err(Error::NotFound(_))));

    let out = base.join("got/manifest.json");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    r.get("mobile/thumbs/a.jpg", &out).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"12345");
    assert!(matches!(r.get("mobile/missing.jpg", &base.join("got/x")), Err(Error::NotFound(_))));

    // a put under a new name, creating the folders
    let up = base.join("up.tmp");
    std::fs::write(&up, b"<xmp/>").unwrap();
    r.put(&up, "inbox/dev/abc-20261026T073320Z-none.xmp").unwrap();
    assert_eq!(std::fs::read(remote_dir.join("inbox/dev/abc-20261026T073320Z-none.xmp")).unwrap(), b"<xmp/>");
    assert_eq!(r.stat("inbox/dev/abc-20261026T073320Z-none.xmp").unwrap().map(|e| e.size), Some(6));
    assert_eq!(r.stat("inbox/dev/none").unwrap(), None);
    // a missing config is reported, not loaded
    assert!(RcloneRemote::open(Some(&so), &base.join("absent.conf"), "x:", None).is_err());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_missing_library_is_a_clear_error() {
    // (a different path than the real one: the process-wide library may already be loaded by the test above, which is fine)
    if RcloneRemote::available(Some(Path::new("/nonexistent/librclone.so"))) {
        return;
    }
    let e = RcloneRemote::open(Some(Path::new("/nonexistent/librclone.so")), Path::new("/x"), "x:", None).err().unwrap();
    assert!(e.to_string().contains("librclone.so not found"), "{e}");
}

#[test]
fn rclone_remote_moves_and_carries_a_whole_publish_and_inbox() {
    let Some(so) = so() else {
        eprintln!("skipped: set LIBRCLONE to a librclone.so");
        return;
    };
    let base = std::env::temp_dir().join(format!("lc-cloud-rclone-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (src, remote_dir) = (base.join("src"), base.join("remote Dir"));
    std::fs::create_dir_all(&remote_dir).unwrap();
    let conf = base.join("rclone.conf");
    std::fs::write(&conf, "").unwrap();
    let r = RcloneRemote::open(Some(&so), &conf, &remote_dir.to_string_lossy(), None).unwrap();
    // move: server-side, creating folders, replacing; a missing source is NotFound
    std::fs::write(remote_dir.join("a.txt"), b"one").unwrap();
    r.move_file("a.txt", "history/2026-10-31/a.123456.txt").unwrap();
    assert!(!remote_dir.join("a.txt").exists());
    assert_eq!(std::fs::read(remote_dir.join("history/2026-10-31/a.123456.txt")).unwrap(), b"one");
    assert!(matches!(r.move_file("a.txt", "b.txt"), Err(Error::NotFound(_))));

    // the home side over librclone: publish, change, remove, and a phone edit
    touch(&src, "A/one.jpg", b"one", 100);
    touch(&src, "A/two.jpg", b"two", 100);
    touch(&src, "A/one.xmp", &sc().build(), 100);
    let mut home = Home::new(HomeConfig::new(&src, base.join("state"), "rclone-test"), Arc::new(r)).unwrap();
    home.set_logger(|_| {});
    let mut fake = Fake::default();
    let rep = home.sync(&mut fake, false).unwrap();
    assert_eq!((rep.plan.new.len(), rep.originals.copied_new, rep.manifest_uploaded), (2, 3, true));
    assert!(remote_dir.join("mobile/manifest.json").is_file() && remote_dir.join("originals/A/two.jpg").is_file());
    std::fs::remove_file(src.join("A/two.jpg")).unwrap();
    let rep = home.sync(&mut fake, false).unwrap();
    assert_eq!((rep.originals.removed, rep.thumbs.removed, rep.previews.removed), (1, 1, 1));
    assert!(!remote_dir.join("originals/A/two.jpg").exists());
    let hist: Vec<_> = walk(&remote_dir.join("history")).into_iter().filter(|p| p.contains("two.")).collect();
    assert_eq!(hist.len(), 1, "{hist:?}");
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(remote_dir.join("mobile/manifest.json")).unwrap()).unwrap();
    let id = manifest["photos"][0]["id"].as_str().unwrap().to_string();
    let base_v = manifest["photos"][0]["sidecarVersion"].as_str().unwrap().to_string();
    let edit = sc().rating(5).build();
    std::fs::create_dir_all(remote_dir.join("inbox/phone")).unwrap();
    std::fs::write(remote_dir.join(format!("inbox/phone/{id}-20990101T000000Z-{base_v}.xmp")), &edit).unwrap();
    let rep = home.sync(&mut fake, false).unwrap();
    assert_eq!(rep.inbox.applied, 1);
    assert_eq!(std::fs::read(src.join("A/one.xmp")).unwrap(), edit);
    assert!(walk(&remote_dir.join("inbox-done")).iter().any(|p| p.ends_with(".xmp")));
    assert!(walk(&remote_dir.join("inbox")).is_empty());
    let _ = std::fs::remove_dir_all(&base);
}

fn walk(d: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        if e.path().is_dir() {
            out.extend(walk(&e.path()));
        } else {
            out.push(e.path().to_string_lossy().into_owned());
        }
    }
    out
}
