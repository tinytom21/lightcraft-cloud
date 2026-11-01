//! [`EngineRenderer`]: LightCraft's engine as the sync crate's [`Renderer`]. Rendering only.
//!
//! The engine runs in this process, on a scratch library of its own in the app's state folder (`<state>/lib`), driven with
//! the same commands the Python tool sends to `lightcraft-cli`: `library.import` (mode `add`: the photo stays where it is),
//! `photo.inspect`, `library.smartPreviews`, `app.export` (thumbnails), `photo.readMetadataFromFile` (a changed sidecar),
//! `photo.deletePermanently` (a catalog entry; files on disk stay). Writing XMP next to the photos is switched off
//! (`library.xmpPreferences autoWrite=false`).
//!
//! The user's LightCraft library is never opened, locked or written: [`EngineRenderer::open`] refuses a library folder
//! under a Pictures folder (`~/Pictures`, `%USERPROFILE%\Pictures`, Windows' own Pictures folder), one named like `LightCraft Library`, or
//! one that holds or sits inside a catalog it did not make; and the app has no setting to point it anywhere else.

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

/// The file that marks a scratch library as this app's own (written when the app creates one).
pub const SCRATCH_MARKER: &str = ".lightcraft-cloud-scratch";

/// The names LightCraft's catalog has in a library folder (the engine's `catalog.snap`, `catalog.log`, `catalog.lock`).
const CATALOG_FILES: [&str; 3] = ["catalog.snap", "catalog.log", "catalog.lock"];

/// Where LightCraft's own library may be, as this PC says. Refusing a scratch library near any of these is the second line of defence
/// (the first is that the app only ever passes `<state>/lib`): the engine must never open, lock or write the user's library.
#[derive(Debug, Clone, Default)]
pub struct LibraryGuard {
    /// `Pictures` folders: LightCraft's default library is `<home>\Pictures\LightCraft Library` (`%USERPROFILE%` on Windows, `$HOME`
    /// elsewhere), and Windows may have moved Pictures (OneDrive).
    pub pictures: Vec<PathBuf>,
    /// `$LIGHTCRAFT_LIBRARY`, which LightCraft opens instead of the default.
    pub library_env: Option<PathBuf>,
    /// Windows (and macOS) file systems ignore case: `c:\users\me\pictures` is the same folder.
    pub case_insensitive: bool,
}

impl LibraryGuard {
    /// The places this PC names: `$HOME` and (on Windows) `%USERPROFILE%` with `Pictures`, Windows' own Pictures folder and the
    /// OneDrive ones, and `$LIGHTCRAFT_LIBRARY`.
    pub fn from_env() -> LibraryGuard {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let mut pictures: Vec<PathBuf> = Vec::new();
        for home in [var("HOME"), if cfg!(windows) { var("USERPROFILE") } else { None }].into_iter().flatten() {
            pictures.push(home.join("Pictures"));
        }
        if cfg!(windows) {
            for k in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
                pictures.extend(var(k).map(|d| d.join("Pictures")));
            }
            #[cfg(windows)]
            pictures.extend(crate::winsys::known_pictures_folder());
        }
        LibraryGuard { pictures, library_env: var("LIGHTCRAFT_LIBRARY"), case_insensitive: cfg!(windows) || cfg!(target_os = "macos") }
    }

    /// The path as a list of comparable components: made absolute, symbolic links resolved as far as the path exists, the Windows
    /// `\\?\` prefix dropped, lower-cased where case does not count.
    fn parts(&self, p: &Path) -> Vec<String> {
        let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        let mut existing = abs.clone();
        let mut tail = Vec::new();
        while !existing.exists() {
            match (existing.file_name().map(|n| n.to_owned()), existing.parent().map(Path::to_path_buf)) {
                (Some(n), Some(parent)) => {
                    tail.push(n);
                    existing = parent;
                }
                _ => break,
            }
        }
        let mut resolved = std::fs::canonicalize(&existing).unwrap_or(existing);
        for t in tail.iter().rev() {
            resolved.push(t);
        }
        let text = resolved.to_string_lossy().replace('\\', "/");
        let text = text.strip_prefix("//?/UNC/").map(|r| format!("//{r}")).or_else(|| text.strip_prefix("//?/").map(str::to_string)).unwrap_or(text);
        text.split('/')
            .filter(|c| !c.is_empty())
            .map(|c| if self.case_insensitive { c.to_lowercase() } else { c.to_string() })
            .collect()
    }

    fn within(&self, inner: &[String], outer: &[String]) -> bool {
        inner.len() >= outer.len() && inner[..outer.len()] == *outer
    }

    /// `Err` when `lib` could be, or could hold, LightCraft's library.
    pub fn check(&self, lib: &Path) -> Result<()> {
        let mine = self.parts(lib);
        let refuse = |why: &str| Err(Error::Invalid(format!("library path {} is not allowed ({why})", lib.display())));
        // a folder named like LightCraft's library, anywhere in the path
        if mine.iter().any(|c| c.to_lowercase().contains("lightcraft library")) {
            return refuse("it looks like a LightCraft library");
        }
        for pictures in &self.pictures {
            if self.within(&mine, &self.parts(pictures)) {
                return refuse("it is under the Pictures folder, where LightCraft keeps its library");
            }
        }
        if let Some(user) = &self.library_env {
            let user = self.parts(user);
            if self.within(&mine, &user) || self.within(&user, &mine) {
                return refuse("it is, or is next to the place of, the library LIGHTCRAFT_LIBRARY names");
            }
        }
        // a catalog in the folder itself (unless the folder is this app's own scratch library) or in any folder above it
        let abs = std::path::absolute(lib).unwrap_or_else(|_| lib.to_path_buf());
        let has_catalog = |d: &Path| CATALOG_FILES.iter().any(|f| d.join(f).exists());
        if has_catalog(&abs) {
            let ours = abs.join(SCRATCH_MARKER).is_file() || abs.parent().is_some_and(|p| p.join("state.json").is_file());
            if !ours {
                return refuse("it already holds a LightCraft catalog that this app did not make");
            }
        }
        for up in abs.ancestors().skip(1) {
            if has_catalog(up) {
                return refuse(&format!("it is inside a LightCraft library ({})", up.display()));
            }
        }
        Ok(())
    }
}

/// The scratch library must be this app's own: not under a `Pictures` folder, not named like a LightCraft library, not LightCraft's
/// `$LIGHTCRAFT_LIBRARY`, and not a folder that holds (or sits inside) a catalog this app did not make. See [`LibraryGuard`].
pub fn check_library_path(lib: &Path) -> Result<()> {
    LibraryGuard::from_env().check(lib)
}

impl EngineRenderer {
    /// Open (or create) the scratch library at `lib`.
    pub fn open(lib: &Path) -> Result<Self> {
        check_library_path(lib)?;
        std::fs::create_dir_all(lib).map_err(|e| Error::Io(format!("{}: {e}", lib.display())))?;
        // (this folder is the app's own from now on: a later check accepts the catalog the engine puts in it)
        let marker = lib.join(SCRATCH_MARKER);
        if !marker.exists() {
            let _ = std::fs::write(&marker, concat!("made by lightcraft-cloud-desktop ", env!("CARGO_PKG_VERSION"), "\n"));
        }
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
