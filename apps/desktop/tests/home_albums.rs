//! The home PC's albums end to end: the real engine renders generated photos, a fake LightCraft (a loopback server that speaks the control
//! protocol and nothing else) answers the album reads, and the manifest carries the albums. The fake records every line it is sent: all of
//! them must pass the gate, and none may change anything.

#[path = "../examples/fixture.rs"]
mod fixture;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use lightcraft_cloud_client::home::{Home, HomeConfig, content_hash_file};
use lightcraft_cloud_client::{DirRemote, Remote};
use lightcraft_cloud_desktop::albums::LinkAlbums;
use lightcraft_cloud_desktop::engine::EngineRenderer;
use lightcraft_cloud_desktop::lightcraft::{TcpLink, allowed};
use serde_json::{Value, json};

fn temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("lc-desktop-albums-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn the_albums_of_a_fake_lightcraft_reach_the_manifest_and_only_reads_were_sent() {
    let root = temp("e2e");
    let (src, remote_dir, state) = (root.join("photos"), root.join("remote"), root.join("state"));
    std::fs::create_dir_all(&remote_dir).unwrap();
    fixture::make(&src);
    let h = |rel: &str| content_hash_file(&src.join(rel)).unwrap().0;
    // catalog ids -> the photo files they stand for (6 is a file outside the synced folder)
    let hashes = Arc::new(Mutex::new(std::collections::HashMap::from([
        (1u64, h("2024/Trip/IMG_0001.jpg")),
        (2, h("2024/Trip/IMG_0002.jpg")),
        (3, h("2024/Trip/PAIR_0001.DNG")), // the alternate of PAIR_0001.JPG
        (4, h("Other/IMG_0003.png")),
        (6, "e".repeat(32)),
    ])));
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    {
        let (hashes, seen) = (hashes.clone(), seen.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut w = stream.try_clone().unwrap();
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    seen.lock().unwrap().push(line.clone());
                    let v: Value = serde_json::from_str(&line).unwrap();
                    let id = &v["id"];
                    let p = &v["params"]["params"];
                    let reply = match (v["method"].as_str(), v["params"]["command"].as_str()) {
                        (Some("engine.commands"), _) => {
                            json!({"id": id, "ok": true, "result": [{"id": "library.folders", "enabled": true}, {"id": "library.import", "enabled": true}]})
                        }
                        (_, Some("albums.list")) => json!({"id": id, "ok": true, "result": [
                            {"id": 1, "name": "Trips", "folder": true, "count": 0, "cover": null, "children": [
                                {"id": 2, "name": "Lisbon", "folder": false, "count": 3, "cover": null, "children": []},
                                {"id": 3, "name": "Good", "folder": false, "count": 1, "cover": null, "children": [], "smart": true, "rules": {}, "rulesText": "x", "problems": []}]}]}),
                        (_, Some("catalog.query")) => {
                            let album = p["filter"]["album"].as_u64().unwrap();
                            let all: Vec<u64> = if album == 2 { vec![1, 3, 6] } else { vec![4, 2] };
                            let page: Vec<Value> = all.iter().skip(p["offset"].as_u64().unwrap() as usize).take(p["limit"].as_u64().unwrap() as usize).map(|i| json!({"id": i, "deleted": false})).collect();
                            json!({"id": id, "ok": true, "result": {"total": all.len(), "photos": page}})
                        }
                        (_, Some("photo.inspect")) => {
                            let hash = hashes.lock().unwrap().get(&p["id"].as_u64().unwrap()).cloned();
                            json!({"id": id, "ok": true, "result": {"id": p["id"], "content_hash": hash}})
                        }
                        _ => json!({"id": id, "ok": false, "error": "refused by the test"}),
                    };
                    let _ = writeln!(w, "{reply}");
                }
            }
        });
    }
    let remote: Arc<dyn Remote> = Arc::new(DirRemote::new(&remote_dir));
    let mut cfg = HomeConfig::new(&src, &state, "e2e");
    cfg.exclude = vec!["exported".into()];
    let mut home = Home::new(cfg, remote).unwrap();
    home.set_album_source(LinkAlbums::new(Box::new(TcpLink::new(port))));
    let mut engine = EngineRenderer::open(&state.join("lib")).unwrap();
    let rep = home.sync(&mut engine, false).unwrap();
    assert!(rep.manifest_uploaded && rep.failed.is_empty());
    let m: Value = serde_json::from_slice(&std::fs::read(remote_dir.join("mobile/manifest.json")).unwrap()).unwrap();
    let id_of = |path: &str| m["photos"].as_array().unwrap().iter().find(|p| p["path"] == path).unwrap()["id"].clone();
    assert_eq!(
        m["albums"],
        json!([
            {"id": "2", "name": "Lisbon", "path": ["Trips"], "smart": false, "photos": [id_of("2024/Trip/IMG_0001.jpg"), id_of("2024/Trip/PAIR_0001.JPG")]},
            {"id": "3", "name": "Good", "path": ["Trips"], "smart": true, "photos": [id_of("Other/IMG_0003.png"), id_of("2024/Trip/IMG_0002.jpg")]}
        ]),
        "the DNG partner maps to its JPEG; the file outside the folder is left out; the smart album comes as its members"
    );
    let a = rep.albums.unwrap();
    assert_eq!((a.published, a.problem), (2, None));
    // everything LightCraft was sent passes the gate, and is one of the reads
    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty());
    for line in &seen {
        assert!(allowed(line), "{line}");
        let v: Value = serde_json::from_str(line).unwrap();
        let c = v["params"]["command"].as_str().unwrap_or("engine.commands");
        assert!(["engine.commands", "albums.list", "catalog.query", "photo.inspect"].contains(&c), "{line}");
    }
    // LightCraft closes: the next sync keeps the albums and says why
    let before = m["albums"].clone();
    drop(home);
    let mut home = Home::new(HomeConfig::new(&src, &state, "e2e"), Arc::new(DirRemote::new(&remote_dir))).unwrap();
    let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    home.set_album_source(LinkAlbums::new(Box::new(TcpLink::new(dead))));
    let rep = home.sync(&mut engine, false).unwrap();
    assert_eq!(rep.albums.unwrap().problem.as_deref(), Some("LightCraft was not open"));
    let m: Value = serde_json::from_slice(&std::fs::read(remote_dir.join("mobile/manifest.json")).unwrap()).unwrap();
    assert_eq!(m["albums"], before);
    let _ = std::fs::remove_dir_all(&root);
}
