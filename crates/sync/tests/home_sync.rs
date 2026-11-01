//! Publishing and the inbox against a folder standing in for the remote, with a fake renderer: ports of the scenarios of
//! the Python tool's tests (scan rules, exclude, pairing, manifest, inbox merge, versions, dry run, safety) and of what
//! `rclone sync --backup-dir` did for it.

mod common;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use common::*;
use lightcraft_cloud_client::home::*;
use lightcraft_cloud_client::{DirRemote, Error};
use serde_json::{Value, json};

const PID: &str = "55555555555555555555555555555555";

fn row(size: u64, mtime_ns: i64, status: &str) -> Row {
    Row { size, mtime_ns, status: status.into(), ..Row::default() }
}

fn names(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

// ------------------------------------------------------------------ scan rules

#[test]
fn scan_extensions_and_skips() {
    let d = temp("scan1");
    for n in ["a.ARW", "b.jpg", "c.HEIC", "d.txt", ".hidden.jpg", ".lightcraft-ab.tmp", "e.xmp", "f.DNG", "g.png"] {
        touch(&d, n, b"x", 100);
    }
    touch(&d, ".hiddendir/x.jpg", b"x", 100);
    touch(&d, "sub/h.tif", b"x", 100);
    let s = scan_source(&d, SystemTime::now(), &[], None);
    assert_eq!(s.entries.keys().map(String::as_str).collect::<Vec<_>>(), ["a.ARW", "b.jpg", "c.HEIC", "f.DNG", "g.png", "sub/h.tif"]);
    assert!(s.unstable.is_empty());
}

#[test]
fn scan_unstable_recent_files() {
    let d = temp("scan2");
    touch(&d, "a.jpg", b"x", 2);
    touch(&d, "b.jpg", b"x", 60);
    touch(&d, "b.xmp", b"x", 1);
    let s = scan_source(&d, SystemTime::now(), &[], None);
    assert_eq!(s.unstable.iter().map(String::as_str).collect::<Vec<_>>(), ["a.jpg", "b.xmp"]);
    assert!(diff_scan(&s, &BTreeMap::new()).new.is_empty()); // both skipped this run
}

#[test]
fn scan_unstable_not_counted_removed() {
    let d = temp("scan3");
    touch(&d, "a.jpg", b"x", 2);
    let s = scan_source(&d, SystemTime::now(), &[], None);
    let known = BTreeMap::from([("a.jpg".to_string(), row(s.entries["a.jpg"].size, 0, "ok"))]);
    let plan = diff_scan(&s, &known);
    assert!(plan.removed.is_empty() && plan.changed.is_empty());
}

#[test]
fn scan_sidecar_naming_rules() {
    let d = temp("scan4");
    for n in ["x/P1.ARW", "x/P1.jpg", "x/P1.xmp", "x/P1.jpg.xmp", "x/Q.ARW", "x/Q.ARW.xmp", "x/R.jpg", "x/R.xmp"] {
        touch(&d, n, b"x", 100);
    }
    let e = scan_source(&d, SystemTime::now(), &[], None).entries;
    assert_eq!(e["x/P1.ARW"].sidecar.as_deref(), Some("x/P1.xmp")); // raw owns the stem sidecar
    assert_eq!(e["x/P1.jpg"].sidecar.as_deref(), Some("x/P1.jpg.xmp")); // JPG uses <file>.xmp
    assert_eq!(e["x/Q.ARW"].sidecar.as_deref(), Some("x/Q.ARW.xmp"));
    assert_eq!(e["x/R.jpg"].sidecar.as_deref(), Some("x/R.xmp"));
    assert_eq!(e["x/P1.jpg"].sidecar_target, "x/P1.jpg.xmp");
}

#[test]
fn scan_diff_classification() {
    let d = temp("scan5");
    touch(&d, "a.jpg", b"1", 100);
    touch(&d, "b.jpg", b"22", 100);
    touch(&d, "c.jpg", b"3", 100);
    touch(&d, "c.xmp", b"<x/>", 100);
    let s = scan_source(&d, SystemTime::now(), &[], None);
    let mut known: BTreeMap<String, Row> = s
        .entries
        .iter()
        .map(|(p, e)| {
            (p.clone(), Row { size: e.size, mtime_ns: e.mtime_ns, sidecar_hash: e.sidecar_hash.clone(), status: "ok".into(), ..Row::default() })
        })
        .collect();
    known.get_mut("b.jpg").unwrap().size = 99; // changed
    known.get_mut("c.jpg").unwrap().sidecar_hash = Some("0".repeat(16)); // sidecar changed
    known.insert("gone.jpg".into(), row(1, 1, "ok"));
    known.remove("a.jpg"); // new
    let plan = diff_scan(&s, &known);
    assert_eq!(
        (names(&plan.new), names(&plan.changed), names(&plan.sc_changed), names(&plan.removed)),
        (vec!["a.jpg"], vec!["b.jpg"], vec!["c.jpg"], vec!["gone.jpg"])
    );
}

// ------------------------------------------------------------------ exclude

#[test]
fn exclude_matching() {
    let ex = vec!["darktable_exported".to_string(), "tmp*".to_string()];
    assert!(is_excluded("darktable_exported/a.jpg", &ex)); // root level
    assert!(is_excluded("2022/x/darktable_exported/a.jpg", &ex)); // nested
    assert!(is_excluded("2022/darktable_exported/deep/a.jpg", &ex));
    assert!(is_excluded("tmp2/a.jpg", &ex)); // glob
    assert!(!is_excluded("2022/darktable_exported", &ex)); // a FILE named like the folder
    assert!(!is_excluded("darktable_exported", &ex));
    assert!(!is_excluded("Darktable_Exported/a.jpg", &ex)); // case-sensitive
    assert!(!is_excluded("2022/a.jpg", &ex));
}

#[test]
fn exclude_scan_skips_excluded() {
    let d = temp("excl");
    for n in
        ["keep/a.jpg", "darktable_exported/b.jpg", "keep/darktable_exported/c.jpg", "keep/darktable_exported/deep/d.jpg", "darktable_exported.jpg"]
    {
        touch(&d, n, b"x", 100);
    }
    let ex = vec!["darktable_exported".to_string()];
    let e = scan_source(&d, SystemTime::now(), &ex, None).entries;
    assert_eq!(e.keys().map(String::as_str).collect::<Vec<_>>(), ["darktable_exported.jpg", "keep/a.jpg"]);
    assert_eq!(scan_source(&d, SystemTime::now(), &[], None).entries.len(), 5);
}

#[test]
fn exclude_max_delete_accounting() {
    let d = temp("excl2");
    touch(&d, "keep/a.jpg", b"x", 100);
    for i in 0..60 {
        touch(&d, &format!("darktable_exported/{i}.jpg"), b"x", 100);
    }
    let s = scan_source(&d, SystemTime::now(), &["darktable_exported".to_string()], None);
    // never published (no state rows): nothing removed at all
    let a = &s.entries["keep/a.jpg"];
    let known = BTreeMap::from([("keep/a.jpg".to_string(), row(a.size, a.mtime_ns, "ok"))]);
    assert!(diff_scan(&s, &known).removed.is_empty());
    // tracked but never published (failed/dup rows) do not count toward max_delete
    let known: BTreeMap<String, Row> = (0..60).map(|i| (format!("darktable_exported/{i}.jpg"), row(1, 1, "failed"))).collect();
    let plan = diff_scan(&s, &known);
    assert_eq!(plan.removed.len(), 60); // still cleaned out of state
    assert!(published_removals(&plan.removed, &known).is_empty());
    // previously published photos that become excluded are removals, subject to max_delete
    let known: BTreeMap<String, Row> = (0..60).map(|i| (format!("darktable_exported/{i}.jpg"), row(1, 1, "ok"))).collect();
    let plan = diff_scan(&s, &known);
    assert_eq!(published_removals(&plan.removed, &known).len(), 60);
}

#[test]
fn fnmatch_patterns() {
    assert!(fnmatch("tmp2", "tmp*") && fnmatch("a1", "a[0-9]") && !fnmatch("a1", "a[!0-9]") && fnmatch("ab", "a?"));
}

// ------------------------------------------------------------------ manifest, pairing

fn mrow(id: &str, size: u64, flag: &str) -> Row {
    Row {
        size,
        mtime_ns: 1_700_000_000_000_000_000,
        id: Some(id.into()),
        status: "ok".into(),
        meta: Meta {
            captured: Some("2024-05-01T09:30:00+02:00".into()),
            camera: Some("C".into()),
            width: Some(6000),
            height: Some(4000),
            rating: Some(3),
            flag: Some(flag.into()),
            preview_key: Some("k".into()),
            ..Meta::default()
        },
        ..Row::default()
    }
}

#[test]
fn manifest_build() {
    let (i1, i2) = ("1".repeat(32), "2".repeat(32));
    let mut rows = BTreeMap::new();
    rows.insert("b/2.jpg".to_string(), Row { sidecar: Some("b/2.xmp".into()), sidecar_hash: Some("ab".repeat(8)), ..mrow(&i2, 5, "none") });
    rows.insert("a/1.jpg".to_string(), mrow(&i1, 5, "pick"));
    rows.insert("a/dup.jpg".to_string(), mrow(&i1, 5, "pick"));
    rows.insert("a/bad.jpg".to_string(), Row { id: None, status: "failed".into(), ..mrow("", 5, "none") });
    let ph = build_manifest_photos(&rows, &HashMap::from([(i1.clone(), 1234)]), PairPrimary::Jpeg);
    assert_eq!(ph.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(), ["a/1.jpg", "b/2.jpg"]);
    let p = &ph[0];
    assert_eq!(p.thumb, format!("thumbs/{i1}.jpg"));
    assert_eq!(
        (p.preview.as_deref(), p.preview_bytes, p.pick, p.rating),
        (Some(format!("previews/{i1}.lcsp").as_str()), Some(1234), Some(true), Some(3))
    );
    assert_eq!(p.mtime, "2023-11-14T22:13:20Z");
    assert!(ph[1].preview.is_none() && ph[1].preview_key.is_none()); // no staged preview
    assert_eq!(ph[1].sidecar_version.as_deref(), Some("abababababababab"));
    let v = serde_json::to_value(p).unwrap();
    for k in [
        "id",
        "path",
        "size",
        "mtime",
        "sidecar",
        "sidecarVersion",
        "captured",
        "camera",
        "lens",
        "width",
        "height",
        "rating",
        "label",
        "thumb",
        "preview",
        "previewBytes",
        "previewKey",
        "pick",
        "flag",
    ] {
        assert!(v.get(k).is_some(), "{k}");
    }
    assert!(v.get("alternates").is_none());
}

fn paired(paths: &[&str], primary: PairPrimary) -> Vec<(String, Vec<String>)> {
    let rows: BTreeMap<String, Row> = paths.iter().enumerate().map(|(i, p)| (p.to_string(), mrow(&format!("{:032x}", i + 1), 5, "none"))).collect();
    build_manifest_photos(&rows, &HashMap::new(), primary)
        .into_iter()
        .map(|p| (p.path.clone(), p.alternates.iter().flatten().map(|a| a.path.clone()).collect()))
        .collect()
}

#[test]
fn pair_default_jpeg_primary() {
    let got = paired(&["A/x.ARW", "A/x.JPG", "A/lone.ARW", "B/lone2.jpg"], PairPrimary::Jpeg);
    let want: Vec<(String, Vec<String>)> =
        vec![("A/lone.ARW".into(), vec![]), ("A/x.JPG".into(), vec!["A/x.ARW".into()]), ("B/lone2.jpg".into(), vec![])];
    assert_eq!(got, want);
    let rows: BTreeMap<String, Row> =
        ["A/x.ARW", "A/x.JPG"].iter().enumerate().map(|(i, p)| (p.to_string(), mrow(&format!("{:032x}", i + 1), 5, "none"))).collect();
    let ph = build_manifest_photos(&rows, &HashMap::new(), PairPrimary::Jpeg);
    let v = serde_json::to_value(&ph[0]).unwrap();
    let alt = v["alternates"][0].as_object().unwrap();
    let mut keys: Vec<&str> = alt.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["height", "id", "path", "preview", "previewBytes", "previewKey", "sidecar", "sidecarVersion", "thumb", "width"]);
    assert_eq!(alt["thumb"], json!(format!("thumbs/{}.jpg", alt["id"].as_str().unwrap())));
    assert_eq!(manifest_ids(&ph).len(), 2);
}

#[test]
fn pair_primary_raw() {
    assert_eq!(paired(&["A/x.jpeg", "A/x.nef"], PairPrimary::Raw), vec![("A/x.nef".to_string(), vec!["A/x.jpeg".to_string()])]);
}

#[test]
fn not_pairs() {
    // different folders, two JPEGs differing only in extension case, other formats
    let got = paired(&["A/x.jpg", "A/x.JPG", "B/x.ARW", "A/y.png", "A/y.ARW"], PairPrimary::Jpeg);
    assert_eq!(got.len(), 5);
    assert!(got.iter().all(|(_, a)| a.is_empty()));
}

// ------------------------------------------------------------------ sidecar versions

#[test]
fn scan_saves_stable_sidecars_once() {
    let d = temp("ver1");
    let src = d.join("src");
    touch(&src, "a.jpg", b"x", 100);
    touch(&src, "a.xmp", b"<a>one</a>", 100);
    touch(&src, "b.jpg", b"x", 100);
    touch(&src, "b.xmp", b"<b>being written</b>", 2);
    let v = VersionStore::new(d.join("v"));
    let s = scan_source(&src, SystemTime::now(), &[], Some(&v));
    assert_eq!(s.unstable.iter().map(String::as_str).collect::<Vec<_>>(), ["b.xmp"]);
    let files = |dir: &std::path::Path| -> Vec<String> {
        std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()
    };
    assert_eq!(files(&d.join("v")), [format!("{}.xmp", sha16(b"<a>one</a>"))]); // not the unstable one, no temp files
    assert_eq!(s.entries["a.jpg"].sidecar_hash.as_deref(), Some(sha16(b"<a>one</a>").as_str()));
    let f = d.join("v").join(format!("{}.xmp", sha16(b"<a>one</a>")));
    assert_eq!(std::fs::read(&f).unwrap(), b"<a>one</a>");
    set_age(&f, 100_000);
    let before = std::fs::metadata(&f).unwrap().modified().unwrap();
    scan_source(&src, SystemTime::now(), &[], Some(&v));
    assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), before); // not rewritten
    touch(&src, "a.xmp", b"<a>two</a>", 100); // every version is kept
    scan_source(&src, SystemTime::now(), &[], Some(&v));
    assert_eq!(files(&d.join("v")).len(), 2);
}

#[test]
fn scan_without_store_writes_nothing() {
    let d = temp("ver2");
    touch(&d.join("src"), "a.jpg", b"x", 100);
    touch(&d.join("src"), "a.xmp", b"<a/>", 100);
    scan_source(&d.join("src"), SystemTime::now(), &[], None);
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
}

#[test]
fn store_and_load_check_the_hash() {
    let d = temp("ver3");
    let v = VersionStore::new(d.join("v"));
    let h = sha16(b"<one/>");
    v.store(b"<one/>", &h);
    assert_eq!(v.load(&h).unwrap(), b"<one/>");
    assert!(v.load(&"0".repeat(16)).is_none());
    std::fs::write(d.join("v").join(format!("{h}.xmp")), b"<tor").unwrap(); // a torn or damaged file
    assert!(v.load(&h).is_none());
}

#[test]
fn purge_keeps_current_versions_and_drops_old_ones() {
    let d = temp("ver4");
    let v = VersionStore::new(d.join("v"));
    let datas: Vec<(&str, Vec<u8>)> = ["old_current", "old_unused", "new_unused"].iter().map(|n| (*n, format!("<v>{n}</v>").into_bytes())).collect();
    for (_, data) in &datas {
        v.store(data, &sha16(data));
    }
    for (n, data) in &datas {
        if n.starts_with("old") {
            set_age(&d.join("v").join(format!("{}.xmp", sha16(data))), 90 * 86400);
        }
    }
    std::fs::write(d.join("v/.lightcraft-ab12cd34.tmp"), b"<half").unwrap(); // interrupted write
    set_age(&d.join("v/.lightcraft-ab12cd34.tmp"), 90 * 86400);
    let keep = HashSet::from([sha16(&datas[0].1)]);
    let n = v.purge(SystemTime::now() - Duration::from_secs(30 * 86400), &keep);
    assert_eq!(n, 2);
    let mut left: Vec<String> = std::fs::read_dir(d.join("v")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    let mut want = vec![format!("{}.xmp", sha16(&datas[0].1)), format!("{}.xmp", sha16(&datas[2].1))];
    want.sort();
    assert_eq!(left, want);
}

// ------------------------------------------------------------------ safety

#[test]
fn source_checks() {
    let mut r = Fake::default();
    let fx = Fx::new("safe1");
    // empty
    assert!(matches!(fx.home.sync(&mut r, false), Err(Error::Invalid(m)) if m.contains("empty")));
    assert!(!fx.state.exists()); // refused before anything was created
    // missing, and a file
    let mut cfg = fx.home.config().clone();
    cfg.source = fx.root.join("missing");
    let h = Home::new(cfg.clone(), Arc::new(DirRemote::new(&fx.remote))).unwrap();
    assert!(matches!(h.sync(&mut r, false), Err(Error::Invalid(m)) if m.contains("does not exist")));
    cfg.source = touch(&fx.root, "file", b"x", 100);
    let h = Home::new(cfg, Arc::new(DirRemote::new(&fx.remote))).unwrap();
    assert!(matches!(h.sync(&mut r, false), Err(Error::Invalid(m)) if m.contains("not a directory")));
}

#[test]
fn state_inside_pictures_is_refused() {
    let home = std::env::var("HOME").unwrap();
    let cfg = HomeConfig::new("/tmp/nonexistent-src", format!("{home}/Pictures/_x_state"), "r");
    let e = Home::new(cfg, Arc::new(DirRemote::new("/tmp/nonexistent-remote"))).err().unwrap();
    assert!(matches!(e, Error::Invalid(m) if m.contains("Pictures")));
    assert!(!std::path::Path::new(&format!("{home}/Pictures/_x_state")).exists());
}

#[test]
fn only_one_run_at_a_time() {
    let d = temp("lock");
    let a = RunLock::acquire(&d, false).unwrap();
    assert!(a.is_some());
    assert!(RunLock::acquire(&d, false).is_err());
    assert!(RunLock::acquire(&d, true).is_err()); // a dry run honours it too
    drop(a);
    assert!(RunLock::acquire(&d, false).is_ok());
    assert!(RunLock::acquire(&temp("nolock").join("none"), true).unwrap().is_none()); // dry run: no lock file, nothing created
}

fn photo(fx: &Fx, rel: &str, content: &str) {
    fx.touch(rel, content.as_bytes(), 100);
}

#[test]
fn too_many_disappeared_photos_are_refused() {
    let fx = Fx::with("maxdel", |c| c.max_delete = 3);
    let mut r = Fake::default();
    for i in 0..8 {
        photo(&fx, &format!("p{i}.jpg"), &format!("photo {i}"));
    }
    fx.home.sync(&mut r, false).unwrap();
    for i in 0..5 {
        std::fs::remove_file(fx.src.join(format!("p{i}.jpg"))).unwrap();
    }
    let before = fx.remote_tree();
    let e = fx.home.sync(&mut r, false).unwrap_err();
    assert!(matches!(&e, Error::Invalid(m) if m.contains("disappeared")), "{e}");
    assert_eq!(fx.remote_tree(), before); // nothing was touched
    // within the limit it goes through, and the removed originals are kept in history
    std::fs::write(fx.src.join("p0.jpg"), b"photo 0").unwrap();
    set_age(&fx.src.join("p0.jpg"), 100);
    std::fs::write(fx.src.join("p1.jpg"), b"photo 1").unwrap();
    set_age(&fx.src.join("p1.jpg"), 100);
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!(rep.originals.removed, 3);
    // (p0 and p1 were written again with the same bytes: not replaced)
    for gone in ["p2", "p3", "p4"] {
        assert!(fx.history().iter().any(|h| h.contains(&format!("/originals/{gone}."))), "{gone}");
    }
    assert_eq!(fx.history().iter().filter(|h| h.contains("/originals/p")).count(), 3);
}

// ------------------------------------------------------------------ publishing

fn date_dir(fx: &Fx) -> String {
    fx.history().first().map(|h| h.split('/').nth(1).unwrap().to_string()).unwrap_or_default()
}

#[test]
fn first_sync_publishes_everything() {
    let fx = Fx::new("pub1");
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    photo(&fx, "B/two.arw", "two");
    fx.touch("A/one.xmp", &sc().build(), 100);
    fx.touch("notes.txt", b"hello", 100);
    fx.touch(".hidden.jpg", b"h", 100);
    fx.touch(".git/config", b"h", 100);
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!((rep.plan.new.len(), rep.photos, rep.manifest_uploaded), (2, 2, true));
    let tree = fx.remote_tree();
    let (h1, h2) = (content_hash(b"one"), content_hash(b"two"));
    for f in [
        "originals/A/one.jpg",
        "originals/A/one.xmp",
        "originals/B/two.arw",
        "originals/notes.txt",
        "mobile/manifest.json",
        &format!("mobile/thumbs/{h1}.jpg"),
        &format!("mobile/thumbs/{h2}.jpg"),
        &format!("mobile/previews/{h1}.lcsp"),
        &format!("mobile/previews/{h2}.lcsp"),
    ] {
        assert!(tree.contains(&f.to_string()), "{f} missing from {tree:?}");
    }
    assert_eq!(tree.len(), 9, "{tree:?}"); // nothing hidden, no temp files, no history
    let m = fx.manifest();
    assert_eq!((m["format"].clone(), m["publisher"].as_str().unwrap().starts_with("lightcraft-cloud ")), (json!(1), true));
    let photos = m["photos"].as_array().unwrap();
    assert_eq!(photos.len(), 2);
    let one = &photos[0];
    assert_eq!(one["path"], "A/one.jpg");
    assert_eq!(one["id"], json!(h1));
    assert_eq!((one["sidecar"].clone(), one["sidecarVersion"].clone()), (json!("A/one.xmp"), json!(sha16(&sc().build()))));
    assert_eq!((one["thumb"].clone(), one["preview"].clone()), (json!(format!("thumbs/{h1}.jpg")), json!(format!("previews/{h1}.lcsp"))));
    assert_eq!(one["previewKey"], json!(content_hash(h1.as_bytes())));
    assert_eq!(one["previewBytes"], json!(format!("lcsp {h1}").len()));
    assert_eq!((one["width"].clone(), one["camera"].clone(), one["pick"].clone()), (json!(6000), json!("ExampleCam X1"), json!(false)));
    assert_eq!(photos[1]["sidecar"], Value::Null);
    // thumbnails are 512 px / q80 by default
    assert!(r.calls.iter().any(|c| c.ends_with("one.jpg 512 80")));
    assert_eq!(fx.rfile(&format!("mobile/thumbs/{h1}.jpg")).unwrap()[..9], *b"thumb 512");
    // the second sync has nothing to do
    let before = fx.remote_tree();
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert!(!rep.plan.any() && !rep.manifest_uploaded);
    assert_eq!((rep.originals.copied_new + rep.originals.replaced + rep.originals.removed, rep.originals.unchanged), (0, 4));
    assert_eq!(fx.remote_tree(), before);
    assert!(fx.lines().iter().any(|l| l == "  manifest unchanged; not uploading"));
}

#[test]
fn a_changed_photo_replaces_the_remote_copy_and_the_old_one_goes_to_history() {
    let fx = Fx::new("pub2");
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    fx.home.sync(&mut r, false).unwrap();
    std::fs::write(fx.src.join("A/one.jpg"), b"one edited in another program").unwrap();
    set_age(&fx.src.join("A/one.jpg"), 100);
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!((rep.plan.changed.len(), rep.originals.replaced), (1, 1));
    assert_eq!(fx.rfile("originals/A/one.jpg").unwrap(), b"one edited in another program");
    let h = fx.history();
    assert_eq!(h.len(), 3, "{h:?}"); // the old photo, its old thumbnail and its old preview
    let old = h.iter().find(|p| p.contains("/originals/A/")).unwrap();
    let date = date_dir(&fx);
    assert!(old.starts_with(&format!("history/{date}/originals/A/one.")) && old.ends_with(".jpg"), "{old}");
    assert_eq!(fx.rfile(old).unwrap(), b"one");
    assert!(h.iter().any(|p| p.contains("/mobile/thumbs/")) && h.iter().any(|p| p.contains("/mobile/previews/")));
    assert_eq!(fx.manifest()["photos"][0]["id"], json!(content_hash(b"one edited in another program")));
}

#[test]
fn a_touched_file_with_the_same_bytes_is_not_uploaded_again() {
    let (fx, spy) = Fx::spied("pub-touch", |_| {});
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    fx.touch("A/one.xmp", &sc().build(), 100);
    fx.home.sync(&mut r, false).unwrap();
    spy.puts();
    spy.moves();
    // only the modification time moves (a backup restore, a sync tool): like rclone, the checksum says nothing changed
    set_age(&fx.src.join("A/one.jpg"), 5000);
    set_age(&fx.src.join("A/one.xmp"), 5000);
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!((rep.originals.replaced, rep.originals.unchanged), (0, 2));
    assert!(spy.moves().iter().all(|(_, to)| !to.starts_with("history/")) && fx.history().is_empty());
    assert_eq!(rep.plan.changed.len(), 1); // (the scan sees a changed photo, and the renderer reads it again)
}

#[test]
fn a_changed_sidecar_re_renders_the_thumbnail_and_keeps_the_preview() {
    let fx = Fx::new("pub3");
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    fx.touch("A/one.xmp", &sc().build(), 100);
    fx.home.sync(&mut r, false).unwrap();
    let m1 = fx.manifest();
    r.calls.clear();
    let edited = sc().s(&[("light.exposure", j(0.7))]).rating(4).flag("pick").build();
    fx.touch("A/one.xmp", &edited, 100);
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!(names(&rep.plan.sc_changed), ["A/one.jpg"]);
    assert_eq!((r.count("add"), r.count("preview"), r.count("reread"), r.count("thumb")), (0, 0, 1, 1));
    let m2 = fx.manifest();
    let (a, b) = (&m1["photos"][0], &m2["photos"][0]);
    assert_eq!(b["sidecarVersion"], json!(sha16(&edited)));
    assert_ne!(a["sidecarVersion"], b["sidecarVersion"]);
    assert_eq!((b["rating"].clone(), b["flag"].clone(), b["pick"].clone()), (json!(4), json!("pick"), json!(true)));
    assert_eq!((a["previewKey"].clone(), a["previewBytes"].clone()), (b["previewKey"].clone(), b["previewBytes"].clone())); // the preview is kept
    let id = a["id"].as_str().unwrap();
    assert!(fx.rfile(&format!("mobile/thumbs/{id}.jpg")).unwrap().ends_with(&edited)); // the thumbnail carries the edit
    assert!(fx.home.versions().load(&sha16(&edited)).is_some());
}

#[test]
fn a_photo_removed_from_the_source_goes_to_history_and_leaves_the_manifest() {
    let fx = Fx::new("pub4");
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    photo(&fx, "A/two.jpg", "two");
    fx.home.sync(&mut r, false).unwrap();
    std::fs::remove_file(fx.src.join("A/two.jpg")).unwrap();
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!((names(&rep.plan.removed), rep.originals.removed, rep.thumbs.removed, rep.previews.removed), (vec!["A/two.jpg"], 1, 1, 1));
    assert!(fx.rfile("originals/A/two.jpg").is_none());
    assert_eq!(fx.manifest()["photos"].as_array().unwrap().len(), 1);
    let h2 = content_hash(b"two");
    assert!(fx.rfile(&format!("mobile/thumbs/{h2}.jpg")).is_none() && fx.rfile(&format!("mobile/previews/{h2}.lcsp")).is_none());
    assert_eq!(fx.history().len(), 3);
    assert!(r.calls.iter().any(|c| c.starts_with("forget") && c.ends_with("two.jpg")));
}

#[test]
fn unstable_files_wait_for_the_next_run() {
    let fx = Fx::new("pub5");
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    fx.touch("A/new.jpg", b"being copied", 2);
    fx.touch("A/one.xmp", b"<fresh/>", 1);
    let rep = fx.home.sync(&mut r, false).unwrap();
    // one.jpg's sidecar is still being written, so neither is published; new.jpg neither
    assert!(rep.plan.new.is_empty());
    assert!(fx.rfile("originals/A/one.jpg").is_some()); // (the photo file itself is stable: mirrored)
    assert!(fx.rfile("originals/A/new.jpg").is_none() && fx.rfile("originals/A/one.xmp").is_none());
}

#[test]
fn a_photo_that_fails_to_import_is_tracked_and_not_published() {
    let fx = Fx::new("pub6");
    let mut r = Fake { fail_import: vec!["bad.jpg".into()], ..Fake::default() };
    photo(&fx, "ok.jpg", "fine");
    photo(&fx, "bad.jpg", "broken");
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!(names(&rep.failed), ["bad.jpg"]);
    assert_eq!(fx.manifest()["photos"].as_array().unwrap().len(), 1);
    assert!(fx.rfile("originals/bad.jpg").is_some()); // (the original is still mirrored)
    assert!(fx.lines().iter().any(|l| l.contains("WARNING: 1 photos failed to import")));
    let st = State::open(&fx.state, true).unwrap();
    assert_eq!(st.get("last_result"), Some("ok, 1 photos failed"));
    assert_eq!(st.files()["bad.jpg"].status, "failed");
    // not retried until it changes
    r.calls.clear();
    fx.home.sync(&mut r, false).unwrap();
    assert_eq!(r.count("add"), 0);
}

#[test]
fn identical_files_are_listed_once() {
    let fx = Fx::new("pub7");
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "same bytes");
    photo(&fx, "B/copy.jpg", "same bytes");
    let rep = fx.home.sync(&mut r, false).unwrap();
    let ph = fx.manifest()["photos"].clone();
    assert_eq!(ph.as_array().unwrap().len(), 1);
    assert_eq!(ph[0]["path"], "A/one.jpg");
    assert!(rep.failed.is_empty());
    assert!(fx.rfile("originals/B/copy.jpg").is_some()); // both are mirrored
    assert_eq!(State::open(&fx.state, true).unwrap().files()["B/copy.jpg"].status, "dup");
}

#[test]
fn raw_and_jpeg_pairs_are_published_as_one_photo_with_an_alternate() {
    for (primary, want_path, want_alt) in [(PairPrimary::Jpeg, "A/x.JPG", "A/x.ARW"), (PairPrimary::Raw, "A/x.ARW", "A/x.JPG")] {
        let fx = Fx::with("pair", |c| c.pair_primary = primary);
        let mut r = Fake::default();
        photo(&fx, "A/x.ARW", "raw bytes");
        photo(&fx, "A/x.JPG", "jpeg bytes");
        photo(&fx, "A/alone.ARW", "alone");
        fx.touch("A/x.xmp", &sc().build(), 100);
        fx.home.sync(&mut r, false).unwrap();
        let ph = fx.manifest()["photos"].clone();
        let ph = ph.as_array().unwrap();
        assert_eq!(ph.len(), 2);
        let x = ph.iter().find(|p| p["path"] == want_path).unwrap();
        assert_eq!(x["alternates"][0]["path"], want_alt);
        let alt_id = x["alternates"][0]["id"].as_str().unwrap();
        assert!(fx.rfile(&format!("mobile/thumbs/{alt_id}.jpg")).is_some() && fx.rfile(&format!("mobile/previews/{alt_id}.lcsp")).is_some());
        assert!(fx.rfile("originals/A/x.ARW").is_some() && fx.rfile("originals/A/x.JPG").is_some()); // both uploaded
        assert!(ph.iter().all(|p| p["path"] != want_alt));
    }
}

#[test]
fn excluded_folders_are_not_published_at_all() {
    let fx = Fx::with("pub8", |c| c.exclude = vec!["exported".into(), "Thumb*".into()]);
    let mut r = Fake::default();
    photo(&fx, "keep/a.jpg", "a");
    photo(&fx, "exported/b.jpg", "b");
    photo(&fx, "keep/exported/c.jpg", "c");
    photo(&fx, "Thumbnails/d.jpg", "d");
    fx.touch("keep/exported/notes.txt", b"n", 100);
    fx.home.sync(&mut r, false).unwrap();
    let up: Vec<String> = fx.remote_tree().into_iter().filter(|p| p.starts_with("originals/")).collect();
    assert_eq!(up, ["originals/keep/a.jpg"]);
    assert_eq!(fx.manifest()["photos"].as_array().unwrap().len(), 1);
    // a folder that becomes excluded counts as removed
    let fx2 = Fx::with("pub9", |_| {});
    photo(&fx2, "keep/a.jpg", "a");
    photo(&fx2, "later/b.jpg", "b");
    fx2.home.sync(&mut Fake::default(), false).unwrap();
    let mut cfg = fx2.home.config().clone();
    cfg.exclude = vec!["later".into()];
    let h = Home::new(cfg, Arc::new(DirRemote::new(&fx2.remote))).unwrap();
    let rep = h.sync(&mut Fake::default(), false).unwrap();
    // like rclone's filters, the exclude covers the remote too: the original stays where it is, but the photo leaves the
    // manifest and its thumbnail and preview go to history
    assert_eq!((rep.originals.removed, rep.thumbs.removed, rep.previews.removed), (0, 1, 1));
    assert!(fx2.rfile("originals/later/b.jpg").is_some());
    assert_eq!(fx2.manifest()["photos"].as_array().unwrap().len(), 1);
}

#[test]
fn previews_only_uploads_no_photo_files() {
    let fx = Fx::with("pub10", |c| c.previews_only = true);
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    fx.touch("A/one.xmp", &sc().build(), 100);
    fx.home.sync(&mut r, false).unwrap();
    let tree = fx.remote_tree();
    assert!(!tree.contains(&"originals/A/one.jpg".to_string()));
    assert!(tree.contains(&"originals/A/one.xmp".to_string()) && tree.contains(&"mobile/manifest.json".to_string()));
    assert_eq!(tree.iter().filter(|p| p.starts_with("mobile/thumbs/") || p.starts_with("mobile/previews/")).count(), 2);
    // an original already on the remote is left alone, not moved to history
    fx.remote_write("originals/A/old.jpg", b"from before");
    fx.home.sync(&mut r, false).unwrap();
    assert!(fx.rfile("originals/A/old.jpg").is_some() && fx.history().is_empty());
}

#[test]
fn a_flaky_remote_is_retried_and_a_dead_one_fails_cleanly() {
    let (fx, spy) = Fx::spied("pub11", |_| {});
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    spy.fail_puts.store(2, std::sync::atomic::Ordering::SeqCst); // two failures: the third attempt works
    fx.home.sync(&mut r, false).unwrap();
    assert!(fx.rfile("originals/A/one.jpg").is_some());
    // the manifest upload fails for good: the sync fails, and the next run uploads it
    photo(&fx, "A/two.jpg", "two");
    *spy.fail_move_to.lock().unwrap() = Some("mobile/manifest.json".into());
    let e = fx.home.sync(&mut r, false).unwrap_err();
    assert!(matches!(e, Error::Remote(_)));
    assert_eq!(fx.manifest()["photos"].as_array().unwrap().len(), 1); // the old manifest is still there
    *spy.fail_move_to.lock().unwrap() = None;
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert!(rep.manifest_uploaded);
    assert_eq!(fx.manifest()["photos"].as_array().unwrap().len(), 2);
    assert!(fx.rfile("originals/A/two.jpg").is_some());
    // (as in the Python tool, the temporary manifest of the failed run stays on the remote)
}

#[test]
fn the_manifest_is_written_last() {
    let (fx, spy) = Fx::spied("pub12", |_| {});
    photo(&fx, "A/one.jpg", "one");
    fx.home.sync(&mut Fake::default(), false).unwrap();
    let puts = spy.puts();
    let last = puts.last().unwrap();
    assert!(last.starts_with("mobile/manifest.json.tmp-"), "{puts:?}");
    assert!(puts.iter().position(|p| p.starts_with("mobile/thumbs/")).unwrap() < puts.len() - 1);
    assert_eq!(spy.moves().last().unwrap().1, "mobile/manifest.json");
}

#[test]
fn manifest_is_uploaded_once_per_photo_list_and_remote() {
    let (mut fx, spy) = Fx::spied("pub13", |_| {});
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    assert!(fx.home.sync(&mut r, false).unwrap().manifest_uploaded);
    assert!(!fx.home.sync(&mut r, false).unwrap().manifest_uploaded);
    photo(&fx, "A/two.jpg", "two");
    assert!(fx.home.sync(&mut r, false).unwrap().manifest_uploaded); // a changed list
    // a new remote gets the manifest although the list is unchanged
    let remote2 = temp("pub13b");
    fx.rehome(Spy::new(&remote2), "other-remote");
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert!(rep.manifest_uploaded);
    assert!(remote2.join("mobile/manifest.json").exists() && remote2.join("originals/A/one.jpg").exists());
    assert!(!fx.home.sync(&mut r, false).unwrap().manifest_uploaded);
    drop(spy);
}

#[test]
fn a_library_published_by_another_tool_is_trusted_when_sizes_match() {
    let (fx, spy) = Fx::spied("pub14", |_| {});
    photo(&fx, "A/one.jpg", "one");
    fx.home.sync(&mut Fake::default(), false).unwrap();
    spy.puts();
    spy.moves();
    // a fresh state folder (the new app taking over a library): nothing is uploaded again or moved to history
    std::fs::remove_dir_all(&fx.state).unwrap();
    let rep = fx.home.sync(&mut Fake::default(), false).unwrap();
    assert_eq!((rep.originals.copied_new, rep.originals.replaced, rep.originals.unchanged), (0, 0, 1));
    assert_eq!((rep.thumbs.copied_new, rep.thumbs.replaced), (0, 0));
    let puts = spy.puts();
    assert!(puts.iter().all(|p| p.starts_with("mobile/manifest.json")), "{puts:?}");
    assert!(fx.history().is_empty());
}

#[test]
fn dry_run_changes_nothing() {
    let fx = Fx::new("dry1");
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    photo(&fx, "A/two.jpg", "two");
    fx.home.sync(&mut r, false).unwrap();
    // a new photo, a changed one, a removed one, and a waiting phone edit
    photo(&fx, "A/three.jpg", "three");
    std::fs::write(fx.src.join("A/one.jpg"), b"one changed").unwrap();
    set_age(&fx.src.join("A/one.jpg"), 100);
    std::fs::remove_file(fx.src.join("A/two.jpg")).unwrap();
    let id = content_hash(b"one");
    fx.put_inbox(&format!("phone/{id}-20990101T000000Z-none.xmp"), &sc().build());
    let before = (fx.tree_hash(&fx.src), fx.tree_hash(&fx.state), fx.tree_hash(&fx.remote));
    r.calls.clear();
    let rep = fx.home.sync(&mut r, true).unwrap();
    assert_eq!(
        (names(&rep.plan.new), names(&rep.plan.changed), names(&rep.plan.removed)),
        (vec!["A/three.jpg"], vec!["A/one.jpg"], vec!["A/two.jpg"])
    );
    assert_eq!((rep.originals.copied_new, rep.originals.replaced, rep.originals.removed), (1, 1, 1));
    assert_eq!(rep.inbox.applied, 1);
    assert_eq!((fx.tree_hash(&fx.src), fx.tree_hash(&fx.state), fx.tree_hash(&fx.remote)), before);
    assert!(r.calls.is_empty()); // the renderer was not asked for anything
    assert!(fx.lines().iter().any(|l| l.contains("DRY RUN done")));
}

#[test]
fn dry_run_without_a_state_folder_creates_nothing() {
    let fx = Fx::new("dry2");
    photo(&fx, "a.jpg", "1");
    let rep = fx.home.sync(&mut Fake::default(), true).unwrap();
    assert!(!fx.state.exists());
    assert!(fx.remote_tree().is_empty());
    assert_eq!(names(&rep.plan.new), ["a.jpg"]);
    assert!(fx.lines().iter().any(|l| l == "inbox: applied=0 merged=0 kept-pc=0 rejected=0 identical=0"));
}

// ------------------------------------------------------------------ the inbox

struct Im {
    fx: Fx,
    st: State,
}

impl Im {
    /// Photo `A/x.jpg` with the PC sidecar `pc` (written `age` seconds ago), the versions in `stored` known.
    fn new(pc: &[u8], age: u64, stored: &[&[u8]]) -> Im {
        let fx = Fx::new("inbox");
        fx.touch("A/x.jpg", b"x", 100);
        fx.touch("A/x.xmp", pc, age);
        let mut st = State::open(&fx.state, false).unwrap();
        st.put("A/x.jpg", Row { size: 1, mtime_ns: 1, status: "ok".into(), id: Some(PID.into()), sidecar: Some("A/x.xmp".into()), ..Row::default() });
        st.commit().unwrap();
        for d in stored {
            fx.home.versions().store(d, &sha16(d));
        }
        Im { fx, st }
    }

    fn name(data_base: &[u8], stamp: &str) -> String {
        format!("phone/{PID}-{stamp}-{}.xmp", sha16(data_base))
    }

    fn run(&self, files: &[(String, Vec<u8>)], dry: bool) -> InboxSummary {
        for (n, d) in files {
            self.fx.put_inbox(n, d);
        }
        self.fx.home.process_inbox(&self.st, dry).unwrap()
    }

    fn pc(&self) -> Vec<u8> {
        std::fs::read(self.fx.src.join("A/x.xmp")).unwrap()
    }

    /// The local history of folder A: file name -> bytes.
    fn history(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        for date in std::fs::read_dir(self.fx.state.join("history")).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(date.path().join("A")).into_iter().flatten().flatten() {
                out.insert(f.file_name().to_string_lossy().into_owned(), std::fs::read(f.path()).unwrap());
            }
        }
        out
    }
}

const LATER: &str = "20990101T000000Z";

#[test]
fn inbox_edit_for_an_alternate_id_is_applied_to_its_own_sidecar() {
    let fx = Fx::new("inbox-alt");
    fx.touch("A/x.ARW", b"r", 100);
    fx.touch("A/x.JPG", b"j", 100);
    fx.touch("A/x.xmp", b"<old/>", 1000);
    fx.touch("A/x.JPG.xmp", b"<oldj/>", 1000);
    let mut st = State::open(&fx.state, false).unwrap();
    let (raw_id, jpg_id) = ("1".repeat(32), "2".repeat(32));
    st.put("A/x.ARW", Row { size: 1, mtime_ns: 1, status: "ok".into(), id: Some(raw_id.clone()), sidecar: Some("A/x.xmp".into()), ..Row::default() });
    st.put("A/x.JPG", Row { size: 1, mtime_ns: 1, status: "ok".into(), id: Some(jpg_id), sidecar: Some("A/x.JPG.xmp".into()), ..Row::default() });
    let xmp = br#"<x:xmpmeta xmlns:x="a" xmlns:lc="b"><lc:settings>{}</lc:settings></x:xmpmeta>"#;
    fx.put_inbox(&format!("phone/{raw_id}-{LATER}-none.xmp"), xmp); // the raw is the alternate (JPEG is primary)
    let s = fx.home.process_inbox(&st, false).unwrap();
    assert_eq!((s.applied, s.rejected), (1, 0), "{s:?}");
    assert_eq!(s.folders, ["A"]);
    assert_eq!(std::fs::read(fx.src.join("A/x.xmp")).unwrap(), xmp); // the raw's own sidecar
    assert_eq!(std::fs::read(fx.src.join("A/x.JPG.xmp")).unwrap(), b"<oldj/>");
}

#[test]
fn an_applied_phone_edit_is_published_by_the_same_sync() {
    // The inbox step writes the sidecar "now"; the scan must not treat that file as still being written (it would wait for
    // the next sync, which published the old rating and the old sidecar).
    let fx = Fx::new("inbox-same-sync");
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    let old = sc().rating(1).build();
    fx.touch("A/one.xmp", &old, 100);
    fx.home.sync(&mut r, false).unwrap();
    let id = content_hash(b"one");
    let edited = sc().s(&[("light.exposure", j(0.7))]).rating(5).build();
    fx.put_inbox(&format!("phone/{id}-{LATER}-{}.xmp", sha16(&old)), &edited);
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!(rep.inbox.applied, 1, "{:?}", rep.inbox);
    assert_eq!(names(&rep.plan.sc_changed), ["A/one.jpg"]);
    assert!(fx.rfile("originals/A/one.xmp").unwrap().ends_with(&edited));
    let b = &fx.manifest()["photos"][0];
    assert_eq!((b["sidecarVersion"].clone(), b["rating"].clone()), (json!(sha16(&edited)), json!(5)));
    // and nothing is left for the next one
    let again = fx.home.sync(&mut r, false).unwrap();
    assert!(!again.plan.any(), "{:?}", again.plan);
}

#[test]
fn a_merged_phone_edit_is_published_by_the_same_sync() {
    let fx = Fx::new("inbox-same-sync-merge");
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    let v0 = sc().build();
    fx.touch("A/one.xmp", &v0, 100);
    fx.home.sync(&mut r, false).unwrap();
    // the PC edits the sidecar (published by the next sync), then the phone edits from the old version
    let pc = sc().s(&[("color.vibrance", j(30.0))]).build();
    fx.touch("A/one.xmp", &pc, 100);
    fx.home.sync(&mut r, false).unwrap();
    let id = content_hash(b"one");
    let phone = sc().s(&[("light.exposure", j(0.5))]).rating(4).build();
    fx.put_inbox(&format!("phone/{id}-{LATER}-{}.xmp", sha16(&v0)), &phone);
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!((rep.inbox.merged, rep.inbox.applied), (1, 0), "{:?}", rep.inbox);
    let merged = std::fs::read(fx.src.join("A/one.xmp")).unwrap();
    assert_ne!(merged, pc);
    assert_eq!(names(&rep.plan.sc_changed), ["A/one.jpg"]);
    assert_eq!(fx.manifest()["photos"][0]["sidecarVersion"], json!(sha16(&merged)));
    assert!(fx.rfile("originals/A/one.xmp").unwrap().ends_with(&merged));
}

#[test]
fn inbox_conflict_is_merged() {
    let v0 = sc().build();
    let pc1 = sc().s(&[("color.vibrance", j(30.0))]).rating(2).build();
    let e1 = sc().s(&[("light.exposure", j(0.5)), ("wb.temp", j(5000.0))]).flag("pick").build();
    let im = Im::new(&pc1, 1000, &[&v0]);
    let name = Im::name(&v0, LATER);
    let s = im.run(&[(name.clone(), e1.clone())], false);
    assert_eq!((s.applied, s.merged, s.kept, s.noop, s.rejected), (0, 1, 0, 0, 0), "{s:?}");
    assert_eq!(s.folders, ["A"]);
    let merged = im.pc();
    assert_eq!(settings_of(&merged), with_settings(&[("color.vibrance", j(30.0)), ("light.exposure", j(0.5)), ("wb.temp", j(5000.0))]));
    assert_eq!((text_of(&merged, "xmp:Rating").as_deref(), text_of(&merged, "lc:flag").as_deref()), (Some("2"), Some("pick")));
    let h = im.history();
    assert_eq!(h.len(), 2, "{:?}", h.keys());
    assert_eq!(h[&format!("x.xmp.phone.{LATER}.xmp")], e1);
    assert!(h.iter().any(|(n, d)| n.starts_with("x.xmp.pc-") && *d == pc1));
    assert!(im.fx.lines().contains(&format!("inbox: {name} -> merge (0 conflicts)")));
    assert_eq!(s.items, [format!("merge {name}: 0 conflicts")]);
    // the inbox file moved to inbox-done, and the merge's inputs and output are kept as versions
    assert!(im.fx.rfile(&format!("inbox/{name}")).is_none());
    assert_eq!(im.fx.remote_tree().iter().filter(|p| p.starts_with("inbox-done/")).count(), 1);
    let vs = im.fx.home.versions();
    assert!(vs.load(&sha16(&e1)).is_some() && vs.load(&sha16(&merged)).is_some());
}

#[test]
fn inbox_conflicts_are_logged() {
    let v0 = sc().build();
    let pc1 = sc().s(&[("light.exposure", j(0.9)), ("light.contrast", j(5.0))]).rating(2).build();
    let e1 = sc().s(&[("light.exposure", j(0.1)), ("light.contrast", j(7.0))]).rating(5).build();
    let im = Im::new(&pc1, 1000, &[&v0]);
    let name = Im::name(&v0, LATER);
    let s = im.run(&[(name.clone(), e1)], false);
    assert_eq!(s.merged, 1);
    let line = im.fx.lines().into_iter().find(|l| l.contains("-> merge")).unwrap();
    assert!(line.starts_with(&format!("inbox: {name} -> merge (3 conflicts: ")), "{line}");
    for c in ["xmp:Rating", "lc:settings/light/exposure", "lc:settings/light/contrast"] {
        assert!(line.contains(c), "{c} in {line}"); // (Python lists them in the document's key order)
    }
    let light = &settings_of(&im.pc())["light"];
    assert_eq!((light["exposure"].clone(), light["contrast"].clone()), (json!(0.1), json!(7.0))); // phone is newer
}

#[test]
fn inbox_pc_newer_wins_the_conflicting_leaf() {
    let v0 = sc().build();
    let im = Im::new(&sc().s(&[("light.exposure", j(0.9))]).build(), 0, &[&v0]); // the PC file was just written
    let e1 = sc().s(&[("light.exposure", j(0.1)), ("color.vibrance", j(4.0))]).build();
    let s = im.run(&[(Im::name(&v0, "20000101T000000Z"), e1)], false);
    assert_eq!((s.merged, s.kept), (1, 0));
    assert_eq!(settings_of(&im.pc()), with_settings(&[("light.exposure", j(0.9)), ("color.vibrance", j(4.0))]));
}

#[test]
fn inbox_base_not_stored_falls_back_to_newest_wins() {
    let v0 = sc().build();
    let pc1 = sc().s(&[("color.vibrance", j(30.0))]).build();
    let e1 = sc().s(&[("light.exposure", j(0.5))]).build();
    let im = Im::new(&pc1, 1000, &[]); // v0 was never stored
    let name = Im::name(&v0, LATER);
    let s = im.run(&[(name.clone(), e1.clone())], false);
    assert_eq!((s.applied, s.merged, s.kept), (1, 0, 0));
    assert_eq!(im.pc(), e1); // whole-file newest-wins, as before
    let h = im.history();
    assert!(h.keys().all(|n| n.starts_with("x.xmp.pc-")) && h.values().collect::<Vec<_>>() == [&pc1]);
    assert!(im.fx.lines().contains(&format!("inbox: {name} -> apply (conflict: phone edit is newer; PC version kept in history)")));
    // and when the PC is newer, the PC keeps its sidecar and the phone's edit goes to history
    let im = Im::new(&pc1, 0, &[]);
    let s = im.run(&[(Im::name(&v0, "20000101T000000Z"), e1.clone())], false);
    assert_eq!((s.applied, s.merged, s.kept), (0, 0, 1));
    assert_eq!(im.pc(), pc1);
    assert_eq!(im.history().values().collect::<Vec<_>>(), [&e1]);
}

#[test]
fn inbox_unmergeable_falls_back_to_newest_wins() {
    let v0 = sc().build();
    let pc1 = sc().s(&[("color.vibrance", j(30.0))]).build();
    let e1 = sc().s(&[("process", json!(3)), ("light.exposure", j(0.5))]).build(); // another engine process
    let im = Im::new(&pc1, 1000, &[&v0]);
    let s = im.run(&[(Im::name(&v0, LATER), e1.clone())], false);
    assert_eq!((s.applied, s.merged, s.kept), (1, 0, 0));
    assert_eq!(im.pc(), e1);
    assert!(im.fx.lines()[0].contains("-> apply (conflict: phone edit is newer"));
}

#[test]
fn inbox_base_none_with_existing_sidecar_is_not_merged() {
    let pc1 = sc().s(&[("color.vibrance", j(30.0))]).build();
    let im = Im::new(&pc1, 1000, &[&sc().build()]);
    let s = im.run(&[(format!("phone/{PID}-{LATER}-none.xmp"), sc().s(&[("light.exposure", j(0.5))]).build())], false);
    assert_eq!((s.applied, s.merged), (1, 0));
}

#[test]
fn inbox_plain_apply_does_not_merge() {
    let v0 = sc().build();
    let e1 = sc().s(&[("light.exposure", j(0.5))]).build();
    let im = Im::new(&v0, 1000, &[&v0]);
    let s = im.run(&[(Im::name(&v0, LATER), e1.clone())], false);
    assert_eq!((s.applied, s.merged), (1, 0));
    assert_eq!(im.pc(), e1);
}

#[test]
fn inbox_merge_that_changes_nothing_is_a_noop() {
    let v0 = sc().build();
    let pc1 = sc().s(&[("color.vibrance", j(30.0))]).build();
    let e1 = sc().lay(Lay::Indent("  ")).build(); // same settings, other layout
    let im = Im::new(&pc1, 1000, &[&v0]);
    let s = im.run(&[(Im::name(&v0, LATER), e1)], false);
    assert_eq!((s.applied, s.merged, s.noop), (0, 0, 1));
    assert_eq!((im.pc(), im.history().len()), (pc1, 0));
}

#[test]
fn inbox_phone_chain_in_one_run() {
    // E2 names E1 as its base. E1 was merged, so the PC never had E1 as a version; the inbox file is the store.
    let v0 = sc().build();
    let pc1 = sc().s(&[("color.vibrance", j(30.0))]).build();
    let e1 = sc().s(&[("light.exposure", j(0.5)), ("light.contrast", j(20.0))]).build();
    let e2 = sc().s(&[("light.exposure", j(0.0)), ("light.contrast", j(20.0))]).build(); // reverts E1's exposure
    let im = Im::new(&pc1, 1000, &[&v0]);
    let s = im.run(&[(Im::name(&v0, "20990101T000000Z"), e1.clone()), (Im::name(&e1, "20990101T000100Z"), e2)], false);
    assert_eq!((s.merged, s.applied, s.kept), (2, 0, 0), "{:?}", s.items);
    assert_eq!(settings_of(&im.pc()), with_settings(&[("light.exposure", j(0.0)), ("light.contrast", j(20.0)), ("color.vibrance", j(30.0))]));
}

#[test]
fn inbox_phone_chain_across_runs() {
    let v0 = sc().build();
    let pc1 = sc().s(&[("color.vibrance", j(30.0))]).build();
    let e1 = sc().s(&[("light.exposure", j(0.5))]).build();
    let e2 = sc().s(&[("light.exposure", j(0.0))]).build();
    let im = Im::new(&pc1, 1000, &[&v0]);
    im.run(&[(Im::name(&v0, LATER), e1.clone())], false);
    assert_eq!(settings_of(&im.pc()), with_settings(&[("color.vibrance", j(30.0)), ("light.exposure", j(0.5))]));
    set_age(&im.fx.src.join("A/x.xmp"), 500);
    let s = im.run(&[(Im::name(&e1, "20990101T000100Z"), e2)], false);
    assert_eq!(s.merged, 1);
    assert_eq!(settings_of(&im.pc()), with_settings(&[("color.vibrance", j(30.0))]));
}

#[test]
fn inbox_dry_run_merges_in_memory_only() {
    let v0 = sc().build();
    let pc1 = sc().s(&[("color.vibrance", j(30.0))]).build();
    let e1 = sc().s(&[("light.exposure", j(0.5))]).build();
    let im = Im::new(&pc1, 1000, &[&v0]);
    im.fx.put_inbox(&Im::name(&v0, LATER), &e1);
    let before = (im.fx.tree_hash(&im.fx.src), im.fx.tree_hash(&im.fx.state), im.fx.tree_hash(&im.fx.remote));
    let s = im.fx.home.process_inbox(&im.st, true).unwrap();
    assert_eq!((s.merged, s.applied, s.kept), (1, 0, 0));
    assert_eq!((im.fx.tree_hash(&im.fx.src), im.fx.tree_hash(&im.fx.state), im.fx.tree_hash(&im.fx.remote)), before); // nothing moved on the remote either
    assert!(im.fx.home.versions().load(&sha16(&e1)).is_none()); // dry run stores no version
}

#[test]
fn inbox_files_are_stored_on_receipt_when_valid() {
    let v0 = sc().build();
    let e1 = sc().s(&[("light.exposure", j(0.5))]).build();
    let im = Im::new(&v0, 1000, &[]);
    let s = im.run(&[(Im::name(&v0, LATER), e1.clone()), (format!("phone/{}-{LATER}-none.xmp", "6".repeat(32)), e1.clone())], false); // (second: unknown id, rejected)
    assert_eq!((s.applied, s.rejected), (1, 1));
    assert_eq!(im.fx.home.versions().load(&sha16(&e1)).unwrap(), e1);
    let im2 = Im::new(&v0, 1000, &[]);
    im2.run(&[(Im::name(&v0, "20990101T000100Z"), b"<junk/>".to_vec())], false); // invalid: not stored
    assert!(im2.fx.home.versions().load(&sha16(b"<junk/>")).is_none());
}

#[test]
fn inbox_rejections_go_to_inbox_done_with_a_note() {
    let v0 = sc().build();
    let im = Im::new(&v0, 1000, &[]);
    let good = format!("phone/{PID}-{LATER}-none.xmp");
    let files = [
        ("phone/not-a-valid-name.xmp".to_string(), v0.clone()),
        (format!("phone/{}-{LATER}-none.xmp", "6".repeat(32)), v0.clone()), // unknown id
        (format!("phone/{PID}-20990101T000100Z-none.xmp"), b"<junk/>".to_vec()), // not develop data
        (good.clone(), sc().s(&[("light.exposure", j(0.1))]).build()),
    ];
    let s = im.run(&files, false);
    assert_eq!((s.rejected, s.applied), (3, 1), "{:?}", s.items);
    let tree = im.fx.remote_tree();
    assert!(tree.iter().all(|p| !p.starts_with("inbox/")), "{tree:?}");
    let done: Vec<&String> = tree.iter().filter(|p| p.starts_with("inbox-done/")).collect();
    assert_eq!(done.len(), 4 + 3, "{done:?}"); // four files and three .rejected notes
    let note = done.iter().find(|p| p.ends_with("not-a-valid-name.xmp.rejected")).unwrap();
    assert!(String::from_utf8(im.fx.rfile(note).unwrap()).unwrap().contains("file name is not"));
    // oldest stamp first: the rejects and the apply were processed in stamp order
    assert!(s.items[0].starts_with("apply ") || s.items[0].starts_with("reject "));
}

#[test]
fn inbox_pending_lists_only_xmp_files_in_device_folders() {
    let im = Im::new(&sc().build(), 1000, &[]);
    assert!(im.fx.home.inbox_pending().unwrap().is_empty()); // no inbox folder at all
    im.fx.put_inbox(&format!("phone/{PID}-{LATER}-none.xmp"), b"x");
    im.fx.put_inbox("phone/readme.txt", b"x");
    assert_eq!(im.fx.home.inbox_pending().unwrap().len(), 1);
}

#[test]
fn a_full_round_trip_phone_edit_then_publish() {
    // publish, the phone edits, sync applies the edit, the thumbnail and the manifest follow
    let fx = Fx::new("round");
    let mut r = Fake::default();
    photo(&fx, "A/one.jpg", "one");
    fx.touch("A/one.xmp", &sc().build(), 1000);
    fx.home.sync(&mut r, false).unwrap();
    let m1 = fx.manifest();
    let id = m1["photos"][0]["id"].as_str().unwrap().to_string();
    let base = m1["photos"][0]["sidecarVersion"].as_str().unwrap().to_string();
    let e1 = sc().s(&[("light.exposure", j(0.5))]).rating(5).build();
    fx.put_inbox(&format!("phone/{id}-{LATER}-{base}.xmp"), &e1);
    // (the sync that applies the edit must wait for the new sidecar to be stable: it is written now)
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!(rep.inbox.applied, 1);
    assert_eq!(std::fs::read(fx.src.join("A/one.xmp")).unwrap(), e1);
    assert!(fx.rfile(&format!("inbox/phone/{id}-{LATER}-{base}.xmp")).is_none());
    // (the fresh sidecar is unstable for a few seconds: the next sync publishes it)
    set_age(&fx.src.join("A/one.xmp"), 100);
    fx.home.sync(&mut r, false).unwrap();
    let m2 = fx.manifest();
    assert_eq!(m2["photos"][0]["sidecarVersion"], json!(sha16(&e1)));
    assert_eq!(m2["photos"][0]["rating"], json!(5));
    assert_eq!(fx.rfile("originals/A/one.xmp").unwrap(), e1);
}

#[test]
fn status_reports_the_last_run() {
    let fx = Fx::new("status");
    photo(&fx, "A/one.jpg", "one");
    let before = fx.home.status().unwrap();
    assert_eq!((before.last_run, before.tracked), (None, 0));
    fx.home.sync(&mut Fake::default(), false).unwrap();
    fx.put_inbox(&format!("phone/{PID}-{LATER}-none.xmp"), b"x");
    let s = fx.home.status().unwrap();
    assert_eq!((s.last_result.as_deref(), s.tracked, s.not_published, s.inbox_pending), (Some("ok"), 1, 0, Some(1)));
    assert!(s.remote.unwrap().0 >= 5);
    assert!(s.last_counts.unwrap().contains("\"photos\":1"));
}

#[test]
fn the_state_folder_holds_only_this_apps_files() {
    let fx = Fx::new("statefiles");
    photo(&fx, "A/one.jpg", "one");
    fx.touch("A/one.xmp", &sc().build(), 100);
    fx.home.sync(&mut Fake::default(), false).unwrap();
    let t = fx.tree(&fx.state);
    assert!(
        t.contains(&"state.json".to_string())
            && t.contains(&"mobile/manifest.json".to_string())
            && t.iter().any(|p| p.starts_with("sidecar-versions/")),
        "{t:?}"
    );
    assert!(t.iter().all(|p| !p.contains("state.sqlite")));
    // the source folder is never written to
    assert_eq!(fx.tree(&fx.src), ["A/one.jpg", "A/one.xmp"]);
}

impl Fx {
    fn remote_write(&self, rel: &str, data: &[u8]) {
        let p = self.remote.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
}

// ------------------------------------------------------------------ live progress

/// The last snapshot of each stage, in the order the stages came.
fn last_per_stage(seen: &[lightcraft_cloud_client::Progress]) -> Vec<lightcraft_cloud_client::Progress> {
    let mut out: Vec<lightcraft_cloud_client::Progress> = Vec::new();
    for p in seen {
        match out.last_mut() {
            Some(l) if l.stage == p.stage && l.started_ms == p.started_ms => *l = *p,
            _ => out.push(*p),
        }
    }
    out
}

#[test]
fn a_progress_hook_sees_the_stages_of_a_sync_with_their_counts_and_each_ends_full_and_changes_nothing_published() {
    use lightcraft_cloud_client::Stage;
    let mut fx = Fx::new("progress");
    let mut r = Fake::default();
    let mut sizes = 0u64;
    for i in 0..4 {
        let data = format!("photo number {i} with some bytes").repeat(i + 1);
        sizes += data.len() as u64;
        photo(&fx, &format!("p{i}.jpg"), &data);
    }
    let seen: Arc<std::sync::Mutex<Vec<lightcraft_cloud_client::Progress>>> = Arc::default();
    let sink = seen.clone();
    fx.home.set_progress_hook(move |p| sink.lock().unwrap().push(*p));
    fx.home.sync(&mut r, false).unwrap();
    let stages = last_per_stage(&seen.lock().unwrap());
    let names: Vec<Stage> = stages.iter().map(|p| p.stage).collect();
    assert_eq!(
        names,
        [
            Stage::Checking,
            Stage::Reading,
            Stage::Previews,
            Stage::Thumbnails,
            Stage::LookingAtLibrary,
            Stage::UploadingOriginals,
            Stage::LookingAtLibrary,
            Stage::UploadingThumbnails,
            Stage::LookingAtLibrary,
            Stage::UploadingPreviews,
            Stage::Publishing
        ],
        "{stages:?}"
    );
    let by = |s: Stage| stages.iter().find(|p| p.stage == s).copied().unwrap();
    for s in [Stage::Reading, Stage::Previews, Stage::Thumbnails] {
        let p = by(s);
        assert_eq!((p.done_items, p.total_items), (4, 4), "{s:?}");
    }
    let up = by(Stage::UploadingOriginals);
    assert_eq!((up.done_items, up.total_items), (4, 4));
    assert_eq!((up.done_bytes, up.total_bytes), (sizes, sizes), "the bytes of the originals");
    // within a stage the count only grows, and the clock stamps are the sync's own
    let all = seen.lock().unwrap().clone();
    for w in all.windows(2).filter(|w| w[0].stage == w[1].stage && w[0].started_ms == w[1].started_ms) {
        assert!(w[0].done_items <= w[1].done_items && w[0].done_bytes <= w[1].done_bytes && w[0].last_item_ms <= w[1].last_item_ms);
    }
    // the same library, published without a hook, is the same bytes: progress changes nothing that is published
    let fx2 = Fx::new("progress-plain");
    let mut r2 = Fake::default();
    for i in 0..4 {
        photo(&fx2, &format!("p{i}.jpg"), &format!("photo number {i} with some bytes").repeat(i + 1));
    }
    fx2.home.sync(&mut r2, false).unwrap();
    assert_eq!(fx.remote_tree(), fx2.remote_tree());
    // a sync with nothing to do says no upload stage
    seen.lock().unwrap().clear();
    fx.home.sync(&mut r, false).unwrap();
    let again: Vec<Stage> = last_per_stage(&seen.lock().unwrap()).iter().map(|p| p.stage).collect();
    assert!(!again.iter().any(|s| matches!(s, Stage::UploadingOriginals | Stage::Previews | Stage::Thumbnails)), "{again:?}");
}

#[test]
fn a_hook_that_panics_or_is_slow_does_not_fail_or_change_a_sync() {
    let mut fx = Fx::new("progress-panic");
    let mut r = Fake::default();
    photo(&fx, "a.jpg", "photo a");
    photo(&fx, "b.jpg", "photo b");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    fx.home.set_progress_hook(move |_| {
        if c.fetch_add(1, std::sync::atomic::Ordering::SeqCst).is_multiple_of(2) {
            panic!("the status file cannot be written");
        }
        std::thread::sleep(Duration::from_millis(1));
    });
    let rep = fx.home.sync(&mut r, false).unwrap();
    assert_eq!(rep.photos, 2);
    assert!(calls.load(std::sync::atomic::Ordering::SeqCst) > 4);
    assert!(fx.rfile("mobile/manifest.json").is_some());
}
