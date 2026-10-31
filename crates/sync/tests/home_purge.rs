//! The weekly purge and the cheap inbox poll the always-running app does: ports of the Python tool's `purge-history` and
//! `inbox-check` tests.

mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;
use lightcraft_cloud_client::Error;
use lightcraft_cloud_client::home::{Row, State, VersionStore};

const ID: &str = "11111111111111111111111111111111";

fn inbox_name(device: &str, stamp: &str) -> String {
    format!("{device}/{ID}-{stamp}-{}.xmp", "b".repeat(16))
}

// ------------------------------------------------------------------ inbox poll

#[test]
fn inbox_poll_counts_only_valid_names() {
    let fx = Fx::new("poll1");
    fx.put_inbox(&inbox_name("phone1", "20260102T030405Z"), b"<x/>");
    fx.put_inbox("stray.txt", b"x");
    fx.put_inbox(&format!("{ID}-20260102T030405Z-none.xmp"), b"x"); // (not in a device folder)
    fx.put_inbox("bad dev/x.xmp", b"x");
    fx.put_inbox("phone1/notes.xmp", b"x");
    assert_eq!(fx.home.inbox_waiting().unwrap(), 1);
}

#[test]
fn inbox_poll_with_only_stray_files_finds_nothing() {
    let fx = Fx::new("poll2");
    fx.put_inbox("stray.txt", b"x");
    fx.put_inbox("phone1/junk.xmp", b"x");
    assert_eq!(fx.home.inbox_waiting().unwrap(), 0);
}

#[test]
fn inbox_poll_of_an_empty_or_missing_inbox_is_zero() {
    let fx = Fx::new("poll3");
    assert_eq!(fx.home.inbox_waiting().unwrap(), 0); // no inbox folder at all
    std::fs::create_dir_all(fx.remote.join("inbox")).unwrap();
    assert_eq!(fx.home.inbox_waiting().unwrap(), 0);
}

#[test]
fn inbox_poll_counts_many_and_touches_nothing() {
    let fx = Fx::new("poll4");
    for i in 0..5 {
        fx.put_inbox(&inbox_name("p", &format!("2026010{}T030405Z", i + 1)), b"<x/>");
    }
    let before = (fx.tree_hash(&fx.remote), fx.tree(&fx.state));
    assert_eq!(fx.home.inbox_waiting().unwrap(), 5);
    assert_eq!((fx.tree_hash(&fx.remote), fx.tree(&fx.state)), before); // no state, no lock, nothing moved
    assert!(!fx.state.join("state.json").exists());
}

// ------------------------------------------------------------------ purge

fn at(secs: u64) -> impl Fn() -> SystemTime + Send + Sync + 'static {
    move || UNIX_EPOCH + Duration::from_secs(secs)
}

/// 2026-11-01 00:00:00 UTC
const NOW: u64 = 1_793_491_200;

fn day_dirs(fx: &Fx, top: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(fx.remote.join(top))
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn purge_deletes_old_day_folders_and_keeps_new_ones() {
    let mut fx = Fx::new("purge1");
    fx.home.set_clock(at(NOW));
    for (top, d) in
        [("history", "2026-09-01"), ("history", "2026-10-02"), ("history", "2026-10-31"), ("inbox-done", "2026-08-15"), ("inbox-done", "2026-10-30")]
    {
        let p = fx.remote.join(top).join(d).join("a/b.xmp");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }
    std::fs::create_dir_all(fx.remote.join("history/not-a-date")).unwrap();
    let r = fx.home.purge_history(30, 1.0).unwrap();
    assert_eq!((r.folders, r.cutoff.as_str()), (2, "2026-10-02")); // (2026-10-02 itself is not older than the cutoff)
    assert_eq!(day_dirs(&fx, "history"), ["2026-10-02", "2026-10-31", "not-a-date"]);
    assert_eq!(day_dirs(&fx, "inbox-done"), ["2026-10-30"]);
    // a second purge has nothing left to do
    assert_eq!(fx.home.purge_history(30, 1.0).unwrap().folders, 0);
}

#[test]
fn purge_without_history_folders_is_fine() {
    let mut fx = Fx::new("purge2");
    fx.home.set_clock(at(NOW));
    assert_eq!(fx.home.purge_history(30, 0.0).unwrap().folders, 0);
}

#[test]
fn purge_keeps_current_versions_and_drops_old_ones() {
    let mut fx = Fx::new("purge3");
    fx.home.set_clock(at(NOW));
    let store = VersionStore::new(fx.state.join("sidecar-versions"));
    let datas: Vec<(&str, Vec<u8>)> = ["old_current", "old_unused", "new_unused"].iter().map(|n| (*n, format!("<v>{n}</v>").into_bytes())).collect();
    for (_, d) in &datas {
        store.store(d, &sha16(d));
    }
    let long_ago = SystemTime::now() - Duration::from_secs(90 * 86_400);
    let age = |p: std::path::PathBuf| std::fs::OpenOptions::new().write(true).open(p).unwrap().set_modified(long_ago).unwrap();
    for (n, d) in &datas {
        if n.starts_with("old") {
            age(store.dir().join(format!("{}.xmp", sha16(d))));
        }
    }
    std::fs::write(store.dir().join(".lightcraft-ab12cd34.tmp"), b"<half").unwrap(); // an interrupted write
    age(store.dir().join(".lightcraft-ab12cd34.tmp"));
    // (the fake clock says 2026-11; "now" for file times is the real clock, so move the cutoff with it)
    fx.home.set_clock(SystemTime::now);
    let mut st = State::open(&fx.state, false).unwrap();
    st.put(
        "a.jpg",
        Row { size: 1, mtime_ns: 1, status: "ok".into(), sidecar: Some("a.xmp".into()), sidecar_hash: Some(sha16(&datas[0].1)), ..Row::default() },
    );
    st.commit().unwrap();
    let r = fx.home.purge_history(30, 1.0).unwrap();
    assert_eq!(r.versions, 2); // the unused old version and the temp file
    let mut left: Vec<String> = std::fs::read_dir(store.dir()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    let mut want = vec![format!("{}.xmp", sha16(&datas[0].1)), format!("{}.xmp", sha16(&datas[2].1))];
    want.sort();
    assert_eq!(left, want);
}

#[test]
fn purge_is_refused_without_a_recent_backup_and_deletes_nothing() {
    let mut fx = Fx::new("purge4");
    fx.home.set_clock(at(NOW));
    let p = fx.remote.join("history/2020-01-01/a.xmp");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, b"x").unwrap();
    let store = VersionStore::new(fx.state.join("sidecar-versions"));
    store.store(b"<v/>", &sha16(b"<v/>"));
    let f = store.dir().join(format!("{}.xmp", sha16(b"<v/>")));
    std::fs::OpenOptions::new().write(true).open(&f).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(1)).unwrap();
    for age in [100.0, 48.5, f64::NAN, f64::INFINITY] {
        match fx.home.purge_history(30, age) {
            Err(Error::Invalid(m)) => assert!(m.contains("refusing to purge history"), "{m}"),
            other => panic!("{age}: {other:?}"),
        }
    }
    assert!(p.exists() && f.exists());
    // exactly at the limit is allowed
    assert!(fx.home.purge_history(30, 48.0).is_ok());
    assert!(!p.exists());
}

#[test]
fn dir_remote_will_not_purge_its_root() {
    use lightcraft_cloud_client::{DirRemote, Remote};
    let d = temp("purge5");
    let r = DirRemote::new(&d);
    assert!(r.purge_dir("").is_err());
    assert!(matches!(r.purge_dir("nope"), Err(Error::NotFound(_))));
    assert!(d.exists());
}
