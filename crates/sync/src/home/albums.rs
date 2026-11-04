//! Albums for the manifest (FORMAT.md, `albums`): the home PC reads LightCraft's albums, through a [`AlbumSource`] the app plugs in
//! (the running LightCraft's control channel; nothing here opens a library), and publishes them as lists of manifest photo ids.
//!
//! * What is read: the album tree, the members of each album (paged), and the content hash of each member (the manifest's photo
//!   id). A hash is looked up once per catalog photo id and remembered in `<state>/album-hashes.json`; at most [`INSPECT_LIMIT`]
//!   lookups are made per sync, so a first run spreads over several syncs and the sync itself never waits for it.
//! * What is kept: the last complete-enough read, as content hashes, in `<state>/albums.json`. When LightCraft is not open or a
//!   query fails, that read is published again unchanged. Hashes are turned into manifest ids only when the manifest is built, so
//!   an alternate maps to its primary and a photo that is not published is left out.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::manifest::ManifestPhoto;
use crate::util::write_atomic;

/// The most photos looked up (one `photo.inspect` each) in one sync.
pub const INSPECT_LIMIT: usize = 2000;
/// Members asked for at once (LightCraft's control channel allows up to 1000).
pub const PAGE: u64 = 1000;
/// A guard against a source that never ends a list.
const MAX_PAGES_PER_ALBUM: u64 = 2000;

/// One node of LightCraft's album tree.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumNode {
    pub id: u64,
    pub name: String,
    /// A folder holds albums, not photos.
    pub folder: bool,
    pub smart: bool,
    pub children: Vec<AlbumNode>,
}

/// A page of an album's members (catalog photo ids, deleted ones left out).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemberPage {
    /// How many photos the album has in all.
    pub total: u64,
    pub ids: Vec<u64>,
}

/// Why a read did not work.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceError {
    /// Nothing answered (LightCraft is not open, or the channel is off).
    Unreachable(String),
    /// LightCraft answered, but not with what was asked for.
    Failed(String),
}

impl SourceError {
    pub fn text(&self) -> &str {
        match self {
            SourceError::Unreachable(m) | SourceError::Failed(m) => m,
        }
    }
}

/// Where the albums come from. Read-only by construction: nothing here can change a library.
pub trait AlbumSource: Send {
    /// The album tree, top level first (folders hold albums).
    fn albums(&mut self) -> Result<Vec<AlbumNode>, SourceError>;
    /// Members of an album (a smart album's current matches), `limit` at most [`PAGE`].
    fn members(&mut self, album: u64, offset: u64, limit: u64) -> Result<MemberPage, SourceError>;
    /// The content hash of a catalog photo; `Ok(None)` when LightCraft has no such photo or it has no hash.
    fn content_hash(&mut self, photo: u64) -> Result<Option<String>, SourceError>;
}

/// One album as read: members by content hash, in LightCraft's order.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct StoredAlbum {
    pub id: String,
    pub name: String,
    pub path: Vec<String>,
    pub smart: bool,
    pub hashes: Vec<String>,
}

/// The last read (`albums.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct StoredAlbums {
    pub format: u32,
    /// When it was read (UTC ISO).
    pub read: String,
    /// False while some photos' hashes were not looked up yet.
    pub complete: bool,
    pub albums: Vec<StoredAlbum>,
}

/// One album of the manifest.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ManifestAlbum {
    pub id: String,
    pub name: String,
    pub path: Vec<String>,
    pub smart: bool,
    pub photos: Vec<String>,
}

/// What a read produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadOutcome {
    pub albums: Vec<StoredAlbum>,
    /// Every member's hash is known.
    pub complete: bool,
    /// Lookups made in this read.
    pub inspected: usize,
    /// Members whose hash is still unknown (the next sync looks them up).
    pub unknown: usize,
}

fn walk(nodes: &[AlbumNode], path: &mut Vec<String>, out: &mut Vec<(u64, String, Vec<String>, bool)>, depth: usize) {
    if depth > 64 {
        return;
    }
    for n in nodes {
        if n.folder {
            path.push(n.name.clone());
            walk(&n.children, path, out, depth + 1);
            path.pop();
        } else {
            out.push((n.id, n.name.clone(), path.clone(), n.smart));
        }
    }
}

/// Read the albums: the tree, the members of each album, and their hashes (from `cache`, else looked up, at most `budget` of them).
/// `cache` maps a catalog photo id to its content hash, `""` for a photo that has none. It is filled as the lookups go, so the
/// caller can keep what was learned even when this returns an error.
pub fn read_albums(src: &mut dyn AlbumSource, cache: &mut HashMap<u64, String>, budget: usize) -> Result<ReadOutcome, SourceError> {
    let tree = src.albums()?;
    let mut flat = Vec::new();
    walk(&tree, &mut Vec::new(), &mut flat, 0);
    let mut members: Vec<Vec<u64>> = Vec::with_capacity(flat.len());
    for (id, ..) in &flat {
        let (mut ids, mut seen) = (Vec::new(), HashSet::new());
        let mut offset = 0;
        for _ in 0..MAX_PAGES_PER_ALBUM {
            let page = src.members(*id, offset, PAGE)?;
            if page.ids.is_empty() {
                break;
            }
            offset += page.ids.len() as u64;
            ids.extend(page.ids.into_iter().filter(|i| seen.insert(*i)));
            if offset >= page.total {
                break;
            }
        }
        members.push(ids);
    }
    // the hashes not known yet, each once, in the order the albums list them
    let mut wanted = Vec::new();
    let mut queued = HashSet::new();
    for ids in &members {
        for id in ids {
            if !cache.contains_key(id) && queued.insert(*id) {
                wanted.push(*id);
            }
        }
    }
    let mut inspected = 0;
    for id in wanted.iter().take(budget) {
        let h = src.content_hash(*id)?;
        cache.insert(*id, h.unwrap_or_default());
        inspected += 1;
    }
    let mut unknown = 0;
    let albums = flat
        .into_iter()
        .zip(members)
        .map(|((id, name, path, smart), ids)| {
            let mut seen = HashSet::new();
            let mut hashes = Vec::new();
            for i in ids {
                match cache.get(&i).map(String::as_str) {
                    Some("") => {}
                    Some(h) => {
                        if seen.insert(h.to_string()) {
                            hashes.push(h.to_string());
                        }
                    }
                    None => unknown += 1,
                }
            }
            StoredAlbum { id: id.to_string(), name, path, smart, hashes }
        })
        .collect();
    Ok(ReadOutcome { albums, complete: unknown == 0, inspected, unknown })
}

/// The albums of the manifest: each stored member's hash becomes the manifest id of its photo. A hash that is an alternate's maps to
/// its primary; one that is not published is left out. A photo listed twice is listed once.
pub fn manifest_albums(stored: &[StoredAlbum], photos: &[ManifestPhoto]) -> Vec<ManifestAlbum> {
    let mut to_id: HashMap<&str, &str> = HashMap::new();
    for p in photos {
        to_id.insert(&p.id, &p.id);
        for a in p.alternates.iter().flatten() {
            to_id.insert(&a.id, &p.id);
        }
    }
    stored
        .iter()
        .map(|a| {
            let mut seen = HashSet::new();
            let photos = a.hashes.iter().filter_map(|h| to_id.get(h.as_str())).filter(|id| seen.insert(**id)).map(|id| id.to_string()).collect();
            ManifestAlbum { id: a.id.clone(), name: a.name.clone(), path: a.path.clone(), smart: a.smart, photos }
        })
        .collect()
}

const ALBUMS_FILE: &str = "albums.json";
const HASHES_FILE: &str = "album-hashes.json";

pub(crate) fn load_stored(state_dir: &Path) -> Option<StoredAlbums> {
    std::fs::read(state_dir.join(ALBUMS_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

pub(crate) fn save_stored(state_dir: &Path, s: &StoredAlbums) -> crate::Result<()> {
    let bytes = serde_json::to_vec(s).map_err(|e| crate::Error::Format(e.to_string()))?;
    write_atomic(&state_dir.join(ALBUMS_FILE), &bytes)
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct HashFile {
    format: u32,
    hashes: BTreeMap<String, String>,
}

pub(crate) fn load_hashes(state_dir: &Path) -> HashMap<u64, String> {
    let f: HashFile = std::fs::read(state_dir.join(HASHES_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    f.hashes.into_iter().filter_map(|(k, v)| Some((k.parse().ok()?, v))).collect()
}

pub(crate) fn save_hashes(state_dir: &Path, h: &HashMap<u64, String>) -> crate::Result<()> {
    let f = HashFile { format: 1, hashes: h.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() };
    let bytes = serde_json::to_vec(&f).map_err(|e| crate::Error::Format(e.to_string()))?;
    write_atomic(&state_dir.join(HASHES_FILE), &bytes)
}

/// What a sync says about the albums (for Status).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlbumsReport {
    /// Albums in the manifest.
    pub published: usize,
    /// When the albums were last read (UTC ISO); `None`: never.
    pub last_read: Option<String>,
    /// Why this sync did not read them: `LightCraft was not open`, or LightCraft's complaint.
    pub problem: Option<String>,
    /// Members whose hash is not looked up yet (they join the albums in the next syncs).
    pub unknown: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        tree: Vec<AlbumNode>,
        members: HashMap<u64, Vec<u64>>,
        hashes: HashMap<u64, String>,
        inspects: Vec<u64>,
        pages: Vec<(u64, u64, u64)>,
        down: bool,
    }

    fn node(id: u64, name: &str, folder: bool, children: Vec<AlbumNode>) -> AlbumNode {
        AlbumNode { id, name: name.into(), folder, smart: false, children }
    }

    impl AlbumSource for Fake {
        fn albums(&mut self) -> Result<Vec<AlbumNode>, SourceError> {
            if self.down { Err(SourceError::Unreachable("closed".into())) } else { Ok(self.tree.clone()) }
        }
        fn members(&mut self, album: u64, offset: u64, limit: u64) -> Result<MemberPage, SourceError> {
            self.pages.push((album, offset, limit));
            let all = self.members.get(&album).cloned().unwrap_or_default();
            Ok(MemberPage { total: all.len() as u64, ids: all.into_iter().skip(offset as usize).take(limit as usize).collect() })
        }
        fn content_hash(&mut self, photo: u64) -> Result<Option<String>, SourceError> {
            self.inspects.push(photo);
            Ok(self.hashes.get(&photo).cloned())
        }
    }

    fn fake() -> Fake {
        let mut f = Fake { tree: vec![], members: HashMap::new(), hashes: HashMap::new(), inspects: vec![], pages: vec![], down: false };
        f.tree = vec![node(1, "Trips", true, vec![node(2, "Lisbon", false, vec![]), node(3, "Inner", true, vec![node(4, "Deep", false, vec![])])]), node(5, "Best", false, vec![])];
        f.members.insert(2, vec![10, 11, 12]);
        f.members.insert(4, vec![11]);
        f.members.insert(5, vec![12, 10]);
        for i in [10, 11, 12] {
            f.hashes.insert(i, format!("{i:032x}"));
        }
        f
    }

    #[test]
    fn the_tree_is_flattened_with_the_folder_chain_above_each_album_and_hashes_are_looked_up_once() {
        let mut f = fake();
        let mut cache = HashMap::new();
        let r = read_albums(&mut f, &mut cache, 100).unwrap();
        let shape: Vec<_> = r.albums.iter().map(|a| (a.id.as_str(), a.name.as_str(), a.path.join("/"), a.hashes.len())).collect();
        assert_eq!(shape, vec![("2", "Lisbon", "Trips".to_string(), 3), ("4", "Deep", "Trips/Inner".to_string(), 1), ("5", "Best", String::new(), 2)]);
        assert_eq!(r.albums[2].hashes, vec![format!("{:032x}", 12), format!("{:032x}", 10)], "LightCraft's order");
        assert!(r.complete && r.unknown == 0);
        assert_eq!(f.inspects, vec![10, 11, 12], "each photo once however many albums hold it");
        // a second read uses the cache
        f.inspects.clear();
        read_albums(&mut f, &mut cache, 100).unwrap();
        assert!(f.inspects.is_empty());
    }

    #[test]
    fn members_are_paged_and_the_lookups_per_read_are_bounded() {
        let mut f = fake();
        f.tree = vec![node(1, "Big", false, vec![])];
        f.members.insert(1, (1..=2500).collect());
        for i in 1..=2500 {
            f.hashes.insert(i, format!("{i:032x}"));
        }
        let mut cache = HashMap::new();
        let r = read_albums(&mut f, &mut cache, INSPECT_LIMIT).unwrap();
        assert_eq!(f.pages, vec![(1, 0, 1000), (1, 1000, 1000), (1, 2000, 1000)]);
        assert_eq!((r.inspected, r.unknown, r.complete), (2000, 500, false));
        assert_eq!(r.albums[0].hashes.len(), 2000);
        let r = read_albums(&mut f, &mut cache, INSPECT_LIMIT).unwrap();
        assert_eq!((r.inspected, r.unknown, r.complete), (500, 0, true));
        assert_eq!(r.albums[0].hashes.len(), 2500);
    }

    #[test]
    fn a_photo_without_a_hash_is_left_out_and_not_asked_for_again() {
        let mut f = fake();
        f.hashes.remove(&11);
        let mut cache = HashMap::new();
        let r = read_albums(&mut f, &mut cache, 100).unwrap();
        assert_eq!(r.albums[0].hashes.len(), 2);
        assert!(r.complete);
        f.inspects.clear();
        read_albums(&mut f, &mut cache, 100).unwrap();
        assert!(f.inspects.is_empty());
    }

    #[test]
    fn a_source_that_is_down_is_an_error_and_what_was_learned_stays_in_the_cache() {
        let mut f = fake();
        f.down = true;
        assert!(matches!(read_albums(&mut f, &mut HashMap::new(), 10), Err(SourceError::Unreachable(_))));
    }

    fn photo(id: &str, alt: Option<&str>) -> ManifestPhoto {
        use super::super::manifest::Alternate;
        ManifestPhoto {
            id: id.into(),
            path: format!("{id}.jpg"),
            size: 1,
            mtime: "2026-01-01T00:00:00Z".into(),
            sidecar: None,
            sidecar_version: None,
            captured: None,
            camera: None,
            lens: None,
            width: None,
            height: None,
            rating: None,
            pick: None,
            flag: None,
            label: None,
            thumb: format!("thumbs/{id}.jpg"),
            preview: None,
            preview_key: None,
            preview_bytes: None,
            alternates: alt.map(|a| {
                vec![Alternate {
                    id: a.into(),
                    path: format!("{a}.arw"),
                    sidecar: None,
                    sidecar_version: None,
                    preview: None,
                    preview_key: None,
                    preview_bytes: None,
                    thumb: format!("thumbs/{a}.jpg"),
                    width: None,
                    height: None,
                }]
            }),
        }
    }

    #[test]
    fn hashes_become_manifest_ids_an_alternate_maps_to_its_primary_and_unpublished_photos_are_left_out() {
        let stored = vec![StoredAlbum {
            id: "7".into(),
            name: "A".into(),
            path: vec!["F".into()],
            smart: true,
            hashes: vec!["raw".into(), "jpg".into(), "other".into(), "gone".into(), "other".into()],
        }];
        let photos = vec![photo("jpg", Some("raw")), photo("other", None)];
        let m = manifest_albums(&stored, &photos);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].photos, vec!["jpg".to_string(), "other".to_string()]);
        assert_eq!(serde_json::to_value(&m[0]).unwrap(), serde_json::json!({"id": "7", "name": "A", "path": ["F"], "smart": true, "photos": ["jpg", "other"]}));
    }
}
