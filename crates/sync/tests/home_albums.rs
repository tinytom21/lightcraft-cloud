//! LightCraft's albums in the manifest (FORMAT.md, `albums`): read through an album source after a publish, kept when the source
//! is down, and the manifest rewritten when only the albums change.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::*;
use lightcraft_cloud_client::home::*;
use serde_json::{Value, json};

#[derive(Default)]
struct Data {
    tree: Vec<AlbumNode>,
    members: HashMap<u64, Vec<u64>>,
    hashes: HashMap<u64, String>,
    down: bool,
    inspects: usize,
}

struct Src(Arc<Mutex<Data>>);

impl AlbumSource for Src {
    fn albums(&mut self) -> Result<Vec<AlbumNode>, SourceError> {
        let d = self.0.lock().unwrap();
        if d.down { Err(SourceError::Unreachable("connection refused".into())) } else { Ok(d.tree.clone()) }
    }
    fn members(&mut self, album: u64, offset: u64, limit: u64) -> Result<MemberPage, SourceError> {
        let d = self.0.lock().unwrap();
        let all = d.members.get(&album).cloned().unwrap_or_default();
        Ok(MemberPage { total: all.len() as u64, ids: all.into_iter().skip(offset as usize).take(limit as usize).collect() })
    }
    fn content_hash(&mut self, photo: u64) -> Result<Option<String>, SourceError> {
        let mut d = self.0.lock().unwrap();
        d.inspects += 1;
        Ok(d.hashes.get(&photo).cloned())
    }
}

fn node(id: u64, name: &str, folder: bool, children: Vec<AlbumNode>) -> AlbumNode {
    AlbumNode { id, name: name.into(), folder, smart: false, children }
}

struct Setup {
    fx: Fx,
    data: Arc<Mutex<Data>>,
}

fn setup(tag: &str, cfg: impl FnOnce(&mut HomeConfig)) -> (Setup, Fake) {
    let mut fx = Fx::with(tag, cfg);
    let data = Arc::new(Mutex::new(Data::default()));
    fx.home.set_album_source(Src(data.clone()));
    for (rel, body) in [("A/one.jpg", "one"), ("A/two.jpg", "two"), ("A/two.arw", "two raw"), ("B/three.jpg", "three")] {
        fx.touch(rel, body.as_bytes(), 100);
    }
    {
        let mut d = data.lock().unwrap();
        // catalog ids: 1 one, 2 two (jpeg), 3 two (raw alternate), 4 three, 5 a photo outside the synced folder
        for (i, body) in [(1, "one"), (2, "two"), (3, "two raw"), (4, "three")] {
            d.hashes.insert(i, content_hash(body.as_bytes()));
        }
        d.hashes.insert(5, "f".repeat(32));
        d.tree = vec![node(10, "Trips", true, vec![node(11, "Lisbon", false, vec![])]), node(12, "Best", false, vec![])];
        d.members.insert(11, vec![1, 3, 5]);
        d.members.insert(12, vec![4, 2]);
    }
    (Setup { fx, data }, Fake::default())
}

fn albums_of(fx: &Fx) -> Value {
    fx.manifest().get("albums").cloned().unwrap_or(Value::Null)
}

#[test]
fn the_albums_are_published_as_manifest_photo_ids_in_lightcrafts_order() {
    let (s, mut r) = setup("al1", |_| {});
    let rep = s.fx.home.sync(&mut r, false).unwrap();
    assert!(rep.manifest_uploaded);
    let (one, two, three) = (content_hash(b"one"), content_hash(b"two"), content_hash(b"three"));
    let m = s.fx.manifest();
    let ids: Vec<&str> = m["photos"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 3, "the raw partner is an alternate, not a photo");
    assert_eq!(
        albums_of(&s.fx),
        json!([
            {"id": "11", "name": "Lisbon", "path": ["Trips"], "smart": false, "photos": [one, two]},
            {"id": "12", "name": "Best", "path": [], "smart": false, "photos": [three, two]}
        ])
    );
    let a = rep.albums.unwrap();
    assert_eq!((a.published, a.problem, a.unknown), (2, None, 0));
    assert!(a.last_read.is_some());
}

#[test]
fn an_unchanged_manifest_is_not_uploaded_but_a_change_in_the_albums_alone_rewrites_it() {
    let (s, mut r) = setup("al2", |_| {});
    s.fx.home.sync(&mut r, false).unwrap();
    assert!(!s.fx.home.sync(&mut r, false).unwrap().manifest_uploaded, "same photos, same albums");
    s.data.lock().unwrap().members.insert(12, vec![4]);
    let rep = s.fx.home.sync(&mut r, false).unwrap();
    assert_eq!((rep.plan.new.len(), rep.plan.changed.len(), rep.manifest_uploaded), (0, 0, true));
    assert_eq!(albums_of(&s.fx)[1]["photos"], json!([content_hash(b"three")]));
    // a rename of an album: same id, new name
    s.data.lock().unwrap().tree[1].name = "Favourites".into();
    assert!(s.fx.home.sync(&mut r, false).unwrap().manifest_uploaded);
    assert_eq!(albums_of(&s.fx)[1]["id"], "12");
    assert_eq!(albums_of(&s.fx)[1]["name"], "Favourites");
}

#[test]
fn photos_already_looked_up_are_not_looked_up_again() {
    let (s, mut r) = setup("al3", |_| {});
    s.fx.home.sync(&mut r, false).unwrap();
    let first = s.data.lock().unwrap().inspects;
    assert_eq!(first, 5);
    s.fx.home.sync(&mut r, false).unwrap();
    assert_eq!(s.data.lock().unwrap().inspects, first);
    s.data.lock().unwrap().members.insert(12, vec![4, 2, 99]);
    s.fx.home.sync(&mut r, false).unwrap();
    assert_eq!(s.data.lock().unwrap().inspects, first + 1, "only the new id");
}

#[test]
fn when_lightcraft_is_not_open_the_last_albums_stay_and_the_report_says_so() {
    let (s, mut r) = setup("al4", |_| {});
    s.fx.home.sync(&mut r, false).unwrap();
    let before = albums_of(&s.fx);
    {
        let mut d = s.data.lock().unwrap();
        d.down = true;
        d.members.insert(12, vec![]);
    }
    let rep = s.fx.home.sync(&mut r, false).unwrap();
    assert!(!rep.manifest_uploaded);
    assert_eq!(albums_of(&s.fx), before);
    let a = rep.albums.unwrap();
    assert_eq!(a.problem.as_deref(), Some("LightCraft was not open"));
    assert_eq!(a.published, 2);
    assert!(a.last_read.is_some());
    // and a photo added meanwhile is published with the albums as they were
    s.fx.touch("B/four.jpg", b"four", 100);
    let rep = s.fx.home.sync(&mut r, false).unwrap();
    assert!(rep.manifest_uploaded);
    assert_eq!(albums_of(&s.fx), before);
    s.data.lock().unwrap().down = false;
    s.fx.home.sync(&mut r, false).unwrap();
    assert_eq!(albums_of(&s.fx)[1]["photos"], json!([]));
}

#[test]
fn a_source_that_was_never_open_gives_a_manifest_without_albums() {
    let (s, mut r) = setup("al5", |_| {});
    s.data.lock().unwrap().down = true;
    let rep = s.fx.home.sync(&mut r, false).unwrap();
    assert!(s.fx.manifest().get("albums").is_none());
    assert_eq!(rep.albums.unwrap().problem.as_deref(), Some("LightCraft was not open"));
}

#[test]
fn with_publish_albums_off_nothing_is_read_and_the_manifest_has_no_albums() {
    let (s, mut r) = setup("al6", |c| c.publish_albums = false);
    let rep = s.fx.home.sync(&mut r, false).unwrap();
    assert!(rep.albums.is_none());
    assert!(s.fx.manifest().get("albums").is_none());
    assert_eq!(s.data.lock().unwrap().inspects, 0);
}

#[test]
fn a_dry_run_reads_nothing() {
    let (s, mut r) = setup("al7", |_| {});
    s.fx.home.sync(&mut r, true).unwrap();
    assert_eq!(s.data.lock().unwrap().inspects, 0);
    assert!(!s.fx.state.join("albums.json").exists());
}
