//! The publisher's `mobile/manifest.json` (format 1; unknown fields are ignored).

use serde_json::{Value, json};

use crate::{Error, Result};

/// The newest manifest format this client reads.
pub const FORMAT: u64 = 1;

/// One file of the library: a photo, or a raw/JPEG partner (an alternate) of one.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// The content hash.
    pub id: String,
    /// The original, relative to `originals/` and to the local mirror.
    pub path: String,
    pub sidecar: Option<String>,
    pub version: Option<String>,
    pub preview: Option<String>,
    pub preview_key: Option<String>,
    pub preview_bytes: Option<u64>,
    pub thumb: Option<String>,
    pub size: Option<u64>,
    /// The file's modification time (UTC ISO), when the manifest has it.
    pub mtime: Option<String>,
    /// A partner of another photo: shown as part of it, not on its own.
    pub alternate: bool,
}

fn text(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn item(v: &Value, alternate: bool) -> Result<Item> {
    let bad = |what: &str| Error::Format(format!("manifest: a photo has no {what}"));
    Ok(Item {
        id: text(v, "id").ok_or_else(|| bad("id"))?,
        path: text(v, "path").ok_or_else(|| bad("path"))?,
        sidecar: text(v, "sidecar"),
        version: text(v, "sidecarVersion"),
        preview: text(v, "preview"),
        preview_key: text(v, "previewKey"),
        preview_bytes: v.get("previewBytes").and_then(Value::as_u64),
        thumb: text(v, "thumb"),
        size: v.get("size").and_then(Value::as_u64),
        mtime: text(v, "mtime"),
        alternate,
    })
}

/// One album of the manifest (`albums`, additive; FORMAT.md): the PC's album as a list of photo ids.
#[derive(Debug, Clone, PartialEq)]
pub struct Album {
    /// Stable on the PC (its catalog album id as text); a rename keeps it.
    pub id: String,
    pub name: String,
    /// The folders above the album, outermost first.
    pub path: Vec<String>,
    pub smart: bool,
    /// Manifest photo ids (primaries) in the PC's order.
    pub photos: Vec<String>,
}

fn clean(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string()).filter(|t| !t.is_empty())
}

/// The albums of a manifest; an entry without a usable id is skipped, a name that is blank becomes `Untitled`, a repeated id keeps the
/// first. Absent or not a list: none.
fn albums(m: &Value) -> Vec<Album> {
    let mut out: Vec<Album> = Vec::new();
    for a in m.get("albums").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = text(a, "id") else { continue };
        if out.iter().any(|o| o.id == id) {
            continue;
        }
        let strings = |k: &str| -> Vec<String> { a.get(k).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect() };
        out.push(Album {
            name: text(a, "name").and_then(|n| clean(&n)).unwrap_or_else(|| "Untitled".into()),
            path: strings("path").iter().map(|p| clean(p).unwrap_or_else(|| "Untitled".into())).collect(),
            smart: a.get("smart").and_then(Value::as_bool).unwrap_or(false),
            photos: strings("photos"),
            id,
        });
    }
    out
}

/// A parsed manifest: its files (primaries and alternates, flattened) and what `library.addRemote`
/// is given for each.
pub struct Parsed {
    pub generated: String,
    pub items: Vec<Item>,
    /// Same order as `items`.
    pub photos: Vec<Value>,
    /// Same order as `items`: which shot (a photo of the manifest, with its alternates) each file belongs to.
    pub shots: Vec<usize>,
    /// The PC's albums (empty when the manifest has none).
    pub albums: Vec<Album>,
}

pub fn parse(bytes: &[u8]) -> Result<Parsed> {
    let m: Value = serde_json::from_slice(bytes).map_err(|e| Error::Format(format!("manifest is not JSON: {e}")))?;
    let format = m.get("format").and_then(Value::as_u64).ok_or_else(|| Error::Format("manifest has no format".into()))?;
    if format > FORMAT {
        return Err(Error::Format(format!("this library is format {format}; this app reads format {FORMAT}: update the app")));
    }
    let list = m.get("photos").and_then(Value::as_array).ok_or_else(|| Error::Format("manifest has no photos".into()))?;
    let (mut items, mut photos, mut shots) = (Vec::new(), Vec::new(), Vec::new());
    for (shot, p) in list.iter().enumerate() {
        let main = item(p, false)?;
        let mut entry = json!({"id": main.id, "path": main.path});
        for k in ["size", "captured", "width", "height", "rating", "flag", "label", "camera", "lens"] {
            if let Some(v) = p.get(k).filter(|v| !v.is_null() || k == "label") {
                entry[k] = v.clone();
            }
        }
        items.push(main);
        photos.push(entry);
        shots.push(shot);
        for alt in p.get("alternates").and_then(Value::as_array).into_iter().flatten() {
            let a = item(alt, true)?;
            let mut e = json!({"id": a.id, "path": a.path});
            // (the partner is the same shot: its capture data is the primary's)
            for k in ["captured", "camera", "lens"] {
                if let Some(v) = p.get(k).filter(|v| !v.is_null()) {
                    e[k] = v.clone();
                }
            }
            for k in ["size", "width", "height"] {
                if let Some(v) = alt.get(k).filter(|v| !v.is_null()) {
                    e[k] = v.clone();
                }
            }
            items.push(a);
            photos.push(e);
            shots.push(shot);
        }
    }
    Ok(Parsed { generated: text(&m, "generated").unwrap_or_default(), items, photos, shots, albums: albums(&m) })
}
