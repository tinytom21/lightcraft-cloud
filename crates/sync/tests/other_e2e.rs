//! A home PC and another PC end to end over a folder standing in for the cloud: the home side is the real `Home` with a fake
//! renderer, the other side is `Other`. Two folders, one chosen.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{Fx, HashRenderer, sc, settings_of, text_of};
use lightcraft_cloud_client::other::{Other, OtherConfig, OtherReport};
use lightcraft_cloud_client::DirRemote;
use serde_json::json;

struct World {
    fx: Fx,
    other: Other,
    laptop: PathBuf,
}

impl World {
    fn new(tag: &str) -> World {
        let fx = Fx::new(tag);
        let laptop = fx.root.join("laptop");
        let mut cfg = OtherConfig::new(laptop.join("photos"), laptop.join("state"), "laptop", vec!["Lisbon".into()]);
        cfg.free_margin = 0;
        std::fs::create_dir_all(laptop.join("photos")).unwrap();
        let mut other = Other::new(cfg, Arc::new(DirRemote::new(&fx.remote))).unwrap();
        other.set_retry_delay(Duration::ZERO);
        World { fx, other, laptop }
    }

    fn home_sync(&self) {
        self.fx.home.sync(&mut HashRenderer::default(), false).unwrap_or_else(|e| panic!("home sync: {e}"));
    }

    fn other_sync(&self) -> OtherReport {
        self.other.sync().unwrap_or_else(|e| panic!("other sync: {e}"))
    }

    fn pc(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.laptop.join("photos").join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn pc_tree(&self) -> Vec<String> {
        self.fx.tree(&self.laptop.join("photos")).into_iter().map(|p| p.replace('\\', "/")).filter(|p| !p.split('/').any(|c| c.starts_with('.'))).collect()
    }

    fn home(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.fx.src.join(rel)).unwrap()
    }
}

fn xmp(exposure: f64, rating: i32) -> Vec<u8> {
    sc().s(&[("light.exposure", json!(exposure))]).rating(rating).build()
}

fn photo(tag: &str) -> Vec<u8> {
    format!("the bytes of {tag} ").repeat(60).into_bytes()
}

#[test]
fn home_and_another_pc_over_a_shared_cloud_library() {
    let w = World::new("e2e");
    let home_sidecar_a = xmp(0.0, 0);
    w.fx.touch("Lisbon/A.jpg", &photo("A"), 3600);
    w.fx.touch("Lisbon/A.xmp", &home_sidecar_a, 3600);
    w.fx.touch("Lisbon/B.jpg", &photo("B"), 3600);
    w.fx.touch("Porto/C.jpg", &photo("C"), 3600);
    w.fx.touch("Porto/C.xmp", &xmp(0.1, 1), 3600);

    // publish, then download: only the chosen folder
    w.home_sync();
    let r = w.other_sync();
    assert_eq!(w.pc_tree(), ["Lisbon/A.jpg", "Lisbon/A.xmp", "Lisbon/B.jpg"], "{r:?}");
    assert_eq!(w.pc("Lisbon/A.jpg"), photo("A"));
    assert_eq!(w.pc("Lisbon/A.xmp"), home_sidecar_a);
    assert!(r.problems.is_empty(), "{r:?}");

    // an edit on the other PC (LightCraft wrote B's first sidecar, and changed A's) travels through the inbox; the home PC applies it
    let edited_b = xmp(0.7, 2);
    std::fs::write(w.laptop.join("photos/Lisbon/B.xmp"), &edited_b).unwrap();
    let a_edit = xmp(0.5, 0);
    std::fs::write(w.laptop.join("photos/Lisbon/A.xmp"), &a_edit).unwrap();
    let r = w.other_sync();
    assert_eq!(r.edits_sent, 2, "{r:?}");
    w.home_sync();
    assert_eq!(w.home("Lisbon/B.xmp"), edited_b, "a new sidecar for a photo that had none");
    assert_eq!(w.home("Lisbon/A.xmp"), a_edit, "applied as it was: nothing at home had changed");

    // ... and what the home PC published (the same bytes) leaves this PC's files alone and nothing more is sent
    let r = w.other_sync();
    assert_eq!((r.edits_sent, r.sidecars_updated.len(), r.held_back), (0, 0, 0), "{r:?}");
    assert_eq!(w.pc("Lisbon/A.xmp"), a_edit);

    // the home PC changes a photo this PC has not touched: it arrives, the old sidecar goes to the history
    let home_b = xmp(0.7, 5);
    w.fx.touch("Lisbon/B.xmp", &home_b, 3600);
    w.home_sync();
    let r = w.other_sync();
    assert_eq!(r.sidecars_updated, ["Lisbon/B.xmp"], "{r:?}");
    assert_eq!(w.pc("Lisbon/B.xmp"), home_b);

    // both changed at once: this PC changes the exposure of A, the home PC its rating, before either has seen the other
    let local = xmp(0.9, 0);
    std::fs::write(w.laptop.join("photos/Lisbon/A.xmp"), &local).unwrap();
    let home_rated = xmp(0.5, 3);
    w.fx.touch("Lisbon/A.xmp", &home_rated, 3600);
    // the home PC's own publication of its edit comes first, as it would with its watcher
    w.home_sync();
    let r = w.other_sync();
    assert_eq!((r.edits_sent, r.sidecars_updated.len()), (1, 0), "the local edit goes first and is not overwritten: {r:?}");
    assert_eq!(w.pc("Lisbon/A.xmp"), local);
    let r = w.other_sync();
    assert_eq!((r.edits_sent, r.held_back), (0, 1), "waiting for the home PC: {r:?}");
    assert_eq!(w.pc("Lisbon/A.xmp"), local);
    // the home PC merges: exposure from this PC, rating from the home PC
    w.home_sync();
    let merged = w.home("Lisbon/A.xmp");
    assert_eq!(settings_of(&merged)["light"]["exposure"], json!(0.9));
    assert_eq!(text_of(&merged, "xmp:Rating").as_deref(), Some("3"));
    let r = w.other_sync();
    assert_eq!(r.sidecars_updated, ["Lisbon/A.xmp"], "the merge result comes back: {r:?}");
    assert_eq!(w.pc("Lisbon/A.xmp"), merged);
    let hist = w.fx.tree(&w.laptop.join("state/history"));
    assert!(hist.iter().any(|h| std::fs::read(w.laptop.join("state/history").join(h)).unwrap() == local), "the local edit survives in the history: {hist:?}");
    assert_eq!(w.other_sync().sidecars_updated.len(), 0);

    // the home PC deletes a photo: it spreads (moved aside, never deleted)
    std::fs::remove_file(w.fx.src.join("Lisbon/B.jpg")).unwrap();
    std::fs::remove_file(w.fx.src.join("Lisbon/B.xmp")).unwrap();
    w.home_sync();
    let r = w.other_sync();
    assert_eq!(r.removed, ["Lisbon/B.jpg"], "{r:?}");
    assert_eq!(w.pc_tree(), ["Lisbon/A.jpg", "Lisbon/A.xmp"]);
    let moved = w.fx.tree(&w.laptop.join("state/removed"));
    assert_eq!(moved.len(), 2, "{moved:?}");

    // deleting on the other PC affects only that PC
    std::fs::remove_file(w.laptop.join("photos/Lisbon/A.jpg")).unwrap();
    w.other_sync();
    w.other_sync();
    assert!(w.fx.src.join("Lisbon/A.jpg").exists(), "the home PC still has it");
    assert!(!w.laptop.join("photos/Lisbon/A.jpg").exists(), "and this PC does not get it back by itself");
    assert!(!w.laptop.join("photos/Porto").exists(), "the folder that was not chosen never came");
}
