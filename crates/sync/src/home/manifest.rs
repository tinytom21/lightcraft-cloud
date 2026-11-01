//! Building `mobile/manifest.json` (FORMAT.md) from the state rows.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;

use super::PairPrimary;
use super::scan::{is_jpeg, is_raw, stem};
use super::state::Row;
use crate::util::iso;

/// The raw or JPEG partner of a photo.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Alternate {
    pub id: String,
    pub path: String,
    pub sidecar: Option<String>,
    #[serde(rename = "sidecarVersion")]
    pub sidecar_version: Option<String>,
    pub preview: Option<String>,
    #[serde(rename = "previewKey")]
    pub preview_key: Option<String>,
    #[serde(rename = "previewBytes")]
    pub preview_bytes: Option<u64>,
    pub thumb: String,
    pub width: Option<u64>,
    pub height: Option<u64>,
}

/// One photo of the manifest, fields in the order the Python publisher writes them.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ManifestPhoto {
    pub id: String,
    pub path: String,
    pub size: u64,
    pub mtime: String,
    pub sidecar: Option<String>,
    #[serde(rename = "sidecarVersion")]
    pub sidecar_version: Option<String>,
    pub captured: Option<String>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub rating: Option<i64>,
    pub pick: Option<bool>,
    pub flag: Option<String>,
    pub label: Option<String>,
    pub thumb: String,
    pub preview: Option<String>,
    #[serde(rename = "previewKey")]
    pub preview_key: Option<String>,
    #[serde(rename = "previewBytes")]
    pub preview_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alternates: Option<Vec<Alternate>>,
}

impl ManifestPhoto {
    fn alternate(&self) -> Alternate {
        Alternate {
            id: self.id.clone(),
            path: self.path.clone(),
            sidecar: self.sidecar.clone(),
            sidecar_version: self.sidecar_version.clone(),
            preview: self.preview.clone(),
            preview_key: self.preview_key.clone(),
            preview_bytes: self.preview_bytes,
            thumb: self.thumb.clone(),
            width: self.width,
            height: self.height,
        }
    }
}

/// Within one folder a raw and a JPEG with the same stem are one shot, but only when it is unambiguous: exactly one raw
/// and exactly one JPEG for that stem. Returns `{primary id: alternate id}`.
fn pair_up(items: &[(&String, ManifestPhoto)], primary: PairPrimary) -> HashMap<String, String> {
    let mut groups: BTreeMap<(String, String), Vec<&(&String, ManifestPhoto)>> = BTreeMap::new();
    for it in items {
        let dir = it.0.rsplit_once('/').map_or("", |(d, _)| d).to_string();
        groups.entry((dir, stem(it.0).to_lowercase())).or_default().push(it);
    }
    let mut pairs = HashMap::new();
    for members in groups.values() {
        let raws: Vec<_> = members.iter().filter(|m| is_raw(m.0)).collect();
        let jpgs: Vec<_> = members.iter().filter(|m| is_jpeg(m.0)).collect();
        if raws.len() == 1 && jpgs.len() == 1 {
            let (pri, alt) = if primary == PairPrimary::Jpeg { (jpgs[0], raws[0]) } else { (raws[0], jpgs[0]) };
            pairs.insert(pri.1.id.clone(), alt.1.id.clone());
        }
    }
    pairs
}

/// How many shots these file paths are, counted the way the home PC counts the manifest's photos: a raw and a JPEG with the same
/// stem in the same folder are one shot (only when exactly one raw and exactly one JPEG share the stem), every other file is a shot of
/// its own. Paths are relative and `/`-separated. Used by the other PC, whose records are per file.
pub(crate) fn count_shots<'a>(paths: impl IntoIterator<Item = &'a str>) -> usize {
    let mut groups: BTreeMap<(&str, String), (usize, usize, usize)> = BTreeMap::new();
    for p in paths {
        let dir = p.rsplit_once('/').map_or("", |(d, _)| d);
        let g = groups.entry((dir, stem(p).to_lowercase())).or_default();
        g.0 += 1;
        g.1 += usize::from(is_raw(p));
        g.2 += usize::from(is_jpeg(p));
    }
    groups.values().map(|&(n, raws, jpgs)| if raws == 1 && jpgs == 1 { n - 1 } else { n }).sum()
}

/// Photos sharing an id (identical content) are listed once (first path). A raw+JPEG pair (same folder and stem) is
/// listed once, as the primary, with the other file under `alternates`. `preview_sizes`: staged smart previews by id.
pub fn build_manifest_photos(rows: &BTreeMap<String, Row>, preview_sizes: &HashMap<String, u64>, primary: PairPrimary) -> Vec<ManifestPhoto> {
    let mut items: Vec<(&String, ManifestPhoto)> = Vec::new();
    let mut seen = BTreeSet::new();
    for (path, r) in rows {
        let Some(id) = r.id.as_ref().filter(|i| !i.is_empty()) else { continue };
        if r.status != "ok" || !seen.insert(id.clone()) {
            continue;
        }
        let pv = preview_sizes.get(id).copied();
        let m = &r.meta;
        items.push((
            path,
            ManifestPhoto {
                id: id.clone(),
                path: path.clone(),
                size: r.size,
                mtime: iso(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(r.mtime_ns.max(0) as u64)),
                sidecar: r.sidecar.clone(),
                sidecar_version: r.sidecar_hash.clone(),
                captured: m.captured.clone(),
                camera: m.camera.clone(),
                lens: m.lens.clone(),
                width: m.width,
                height: m.height,
                rating: m.rating,
                pick: m.flag.as_ref().map(|f| f == "pick"),
                flag: m.flag.clone(),
                label: m.label.clone(),
                thumb: format!("thumbs/{id}.jpg"),
                preview: pv.map(|_| format!("previews/{id}.lcsp")),
                preview_key: if pv.is_some() { m.preview_key.clone() } else { None },
                preview_bytes: pv,
                alternates: None,
            },
        ));
    }
    let pairs = pair_up(&items, primary);
    let alt_ids: BTreeSet<&String> = pairs.values().collect();
    let by_id: HashMap<&String, &ManifestPhoto> = items.iter().map(|(_, p)| (&p.id, p)).collect();
    let mut out = Vec::new();
    for (_, ph) in &items {
        if alt_ids.contains(&ph.id) {
            continue;
        }
        let mut ph = ph.clone();
        if let Some(alt) = pairs.get(&ph.id) {
            ph.alternates = Some(vec![by_id[alt].alternate()]);
        }
        out.push(ph);
    }
    out
}

/// Every id a manifest refers to, alternates included (their thumbnails and previews must be kept).
pub fn manifest_ids(photos: &[ManifestPhoto]) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for p in photos {
        ids.insert(p.id.clone());
        ids.extend(p.alternates.iter().flatten().map(|a| a.id.clone()));
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::count_shots;

    #[test]
    fn a_raw_and_a_jpeg_with_one_stem_in_one_folder_are_one_shot_and_nothing_else_pairs() {
        assert_eq!(count_shots(["A/x.ARW", "A/x.JPG"]), 1);
        assert_eq!(count_shots(["A/x.ARW", "A/X.jpeg"]), 1, "stems compare ignoring case");
        assert_eq!(count_shots(["A/x.ARW", "B/x.JPG"]), 2, "different folders");
        assert_eq!(count_shots(["A/x.ARW", "A/x.DNG", "A/x.JPG"]), 3, "two raws: ambiguous, so no pair");
        assert_eq!(count_shots(["A/x.ARW", "A/x.JPG", "A/x.jpeg"]), 3);
        assert_eq!(count_shots(["x.png", "x.jpg"]), 2, "a PNG is not a raw");
        assert_eq!(count_shots(std::iter::empty()), 0);
    }
}
