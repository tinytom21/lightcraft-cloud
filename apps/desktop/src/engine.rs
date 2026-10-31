//! [`EngineRenderer`]: LightCraft's engine as the sync crate's [`Renderer`]. Rendering only.
//!
//! The engine runs in this process, on a scratch library of its own in the app's state folder (`<state>/lib`), driven with
//! the same commands the Python tool sends to `lightcraft-cli`: `library.import` (mode `add`: the photo stays where it is),
//! `photo.inspect`, `library.smartPreviews`, `app.export` (thumbnails), `photo.readMetadataFromFile` (a changed sidecar),
//! `photo.deletePermanently` (a catalog entry; files on disk stay). Writing XMP next to the photos is switched off
//! (`library.xmpPreferences autoWrite=false`).
//!
//! The user's LightCraft library is never opened, locked or written: [`EngineRenderer::open`] refuses a library folder
//! under `~/Pictures` or one named `LightCraft Library`, and the app has no setting to point it anywhere else.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lightcraft_cloud_client::home::{Added, PhotoInfo, Renderer};
use lightcraft_cloud_client::{Error, Result};
use lightcraft_engine::Session;
use lightcraft_engine::catalog::{PhotoId, Source};
use lightcraft_mcp::{Backend, Headless};
use serde_json::{Value, json};

pub struct EngineRenderer {
    /// The engine in this process (`app.export` lives in this wrapper, as in `lightcraft-cli`).
    h: Headless,
    /// source path -> the scratch library's id
    ids: HashMap<PathBuf, u64>,
}

fn engine(e: impl std::fmt::Display) -> Error {
    Error::Engine(e.to_string())
}

/// The scratch library must be this app's own: not under `~/Pictures` and not a LightCraft library by name.
pub fn check_library_path(lib: &Path) -> Result<()> {
    let abs = std::path::absolute(lib).map_err(engine)?;
    let resolved = {
        let mut existing = abs.clone();
        let mut tail = Vec::new();
        while !existing.exists() {
            match (existing.file_name().map(|n| n.to_owned()), existing.parent().map(Path::to_path_buf)) {
                (Some(n), Some(p)) => {
                    tail.push(n);
                    existing = p;
                }
                _ => break,
            }
        }
        let mut r = std::fs::canonicalize(&existing).unwrap_or(existing);
        for t in tail.iter().rev() {
            r.push(t);
        }
        r
    };
    if resolved.to_string_lossy().contains("LightCraft Library") {
        return Err(Error::Invalid(format!("library path {} is not allowed (it looks like a LightCraft library)", resolved.display())));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        let pictures = std::fs::canonicalize(home.join("Pictures")).unwrap_or_else(|_| home.join("Pictures"));
        if resolved.starts_with(&pictures) {
            return Err(Error::Invalid(format!("library path {} is under ~/Pictures; not allowed", resolved.display())));
        }
    }
    Ok(())
}

impl EngineRenderer {
    /// Open (or create) the scratch library at `lib`.
    pub fn open(lib: &Path) -> Result<Self> {
        check_library_path(lib)?;
        std::fs::create_dir_all(lib).map_err(|e| Error::Io(format!("{}: {e}", lib.display())))?;
        // (a plain session: no denoise, SAM or face models are wired in; rendering only)
        let mut h = Headless::new(Session::new().with_fs());
        h.session.open_library(lib, false).map_err(|e| Error::Engine(format!("scratch library {}: {e}", lib.display())))?;
        for w in h.session.take_library_warnings() {
            log::warn!("scratch library: {w}");
        }
        h.session.execute("library.xmpPreferences", &json!({"autoWrite": false})).map_err(engine)?;
        let ids = h
            .session
            .catalog
            .photos()
            .filter_map(|p| match &p.source {
                Source::File { path } => Some((PathBuf::from(path), p.id.0)),
                Source::Demo { .. } => None,
            })
            .collect();
        Ok(EngineRenderer { h, ids })
    }

    fn run(&mut self, command: &str, params: Value) -> Result<Value> {
        self.h.session.execute(command, &params).map_err(|e| Error::Engine(format!("{command}: {e}")))
    }

    fn inspect(&mut self, id: u64) -> Result<PhotoInfo> {
        let v = self.run("photo.inspect", json!({"id": id}))?;
        let text = |p: &Value| p.as_str().map(str::to_string);
        Ok(PhotoInfo {
            content_hash: text(&v["content_hash"]).ok_or_else(|| Error::Engine("photo.inspect: no content_hash".into()))?,
            captured: text(&v["captured"]),
            camera: text(&v["meta"]["camera"]),
            lens: text(&v["meta"]["lens"]),
            width: v["width"].as_u64(),
            height: v["height"].as_u64(),
            rating: v["rating"].as_i64(),
            flag: text(&v["flag"]),
            label: text(&v["label"]),
        })
    }

    fn id_of(&self, photo: &Path) -> Option<u64> {
        self.ids.get(photo).copied()
    }

    fn need(&self, photo: &Path) -> Result<u64> {
        self.id_of(photo).ok_or_else(|| Error::Engine(format!("{} is not in the scratch library", photo.display())))
    }
}

impl Renderer for EngineRenderer {
    fn add(&mut self, photo: &Path) -> Result<Added> {
        let r = self.run("library.import", json!({"paths": [photo.to_string_lossy()], "mode": "add"}))?;
        if let Some(f) = r["failed"].as_array().and_then(|f| f.first()) {
            return Err(Error::Engine(format!("import failed: {f}")));
        }
        let me = photo.to_string_lossy();
        let id = if let Some(id) = r["imported"].as_array().and_then(|a| a.first()).and_then(Value::as_u64) {
            id
        } else if let Some(d) = r["duplicates"].as_array().and_then(|d| d.iter().find(|d| d["path"].as_str() == Some(me.as_ref()))) {
            let existing = d["existing"].as_u64();
            // the same file imported by an earlier run (its entry is still in the scratch library): not a duplicate
            match existing.filter(|e| self.ids.get(photo) == Some(e)) {
                Some(id) => id,
                None => return Ok(Added::Duplicate),
            }
        } else {
            return Err(Error::Engine(format!("nothing was imported from {}: {r}", photo.display())));
        };
        self.ids.insert(photo.to_path_buf(), id);
        self.inspect(id).map(Added::Photo)
    }

    fn reread(&mut self, photo: &Path) -> Result<PhotoInfo> {
        let Some(id) = self.id_of(photo) else {
            return match self.add(photo)? {
                Added::Photo(i) => Ok(i),
                Added::Duplicate => Err(Error::Engine(format!("{} duplicates another photo", photo.display()))),
            };
        };
        self.run("photo.readMetadataFromFile", json!({"ids": [id]}))?;
        self.inspect(id)
    }

    fn thumbnail(&mut self, photo: &Path, long_edge: u32, quality: u8, out: &Path) -> Result<()> {
        let id = self.need(photo)?;
        let params = json!({"ids": [id], "path": out.to_string_lossy(), "longEdge": long_edge, "quality": quality, "colorSpace": "srgb", "conflict": "overwrite"});
        self.h
            .call("engine.execute", json!({"command": "app.export", "params": params}))
            .map(|_| ())
            .map_err(|e| Error::Engine(format!("app.export: {e}")))
    }

    fn smart_preview(&mut self, photo: &Path, out: &Path) -> Result<String> {
        let id = self.need(photo)?;
        self.run("library.smartPreviews", json!({"ids": [id]}))?;
        let loc = self.run("library.smartPreviewsLocation", json!({}))?;
        let dir = loc["path"].as_str().map(PathBuf::from).ok_or_else(|| Error::Engine("the scratch library has no smart previews folder".into()))?;
        let ph = self.h.session.catalog.photo(PhotoId(id)).ok_or_else(|| Error::Engine("photo vanished from the scratch library".into()))?;
        // (the engine names a proxy by a hash of the photo's content hash: that name is the manifest's previewKey)
        let name = lightcraft_engine::smart::file_name(ph);
        let file = lightcraft_engine::smart::file_path(&dir, ph);
        std::fs::copy(&file, out).map_err(|e| Error::Engine(format!("smart preview {}: {e}", file.display())))?;
        Ok(name.trim_end_matches(".lcsp").to_string())
    }

    fn forget(&mut self, photos: &[PathBuf]) -> Result<()> {
        let ids: Vec<u64> = photos.iter().filter_map(|p| self.ids.remove(p)).collect();
        if ids.is_empty() {
            return Ok(());
        }
        self.run("photo.deletePermanently", json!({"ids": ids})).map(|_| ())
    }

    fn finish(&mut self) -> Result<()> {
        self.h.session.persist().map_err(engine)
    }
}

/// Opens the engine's scratch library on first use: a dry run, or a run with nothing to render, never opens it.
pub struct LazyRenderer {
    lib: PathBuf,
    inner: Option<EngineRenderer>,
}

impl LazyRenderer {
    pub fn new(lib: PathBuf) -> Self {
        LazyRenderer { lib, inner: None }
    }

    fn get(&mut self) -> Result<&mut EngineRenderer> {
        if self.inner.is_none() {
            self.inner = Some(EngineRenderer::open(&self.lib)?);
        }
        self.inner.as_mut().ok_or_else(|| Error::Engine("the scratch library did not open".into()))
    }
}

impl Renderer for LazyRenderer {
    fn add(&mut self, photo: &Path) -> Result<Added> {
        self.get()?.add(photo)
    }
    fn reread(&mut self, photo: &Path) -> Result<PhotoInfo> {
        self.get()?.reread(photo)
    }
    fn thumbnail(&mut self, photo: &Path, long_edge: u32, quality: u8, out: &Path) -> Result<()> {
        self.get()?.thumbnail(photo, long_edge, quality, out)
    }
    fn smart_preview(&mut self, photo: &Path, out: &Path) -> Result<String> {
        self.get()?.smart_preview(photo, out)
    }
    fn forget(&mut self, photos: &[PathBuf]) -> Result<()> {
        if self.inner.is_none() && !self.lib.is_dir() {
            return Ok(()); // no scratch library yet, so nothing to drop
        }
        self.get()?.forget(photos)
    }
    fn finish(&mut self) -> Result<()> {
        match self.inner.as_mut() {
            Some(r) => r.finish(),
            None => Ok(()),
        }
    }
}
