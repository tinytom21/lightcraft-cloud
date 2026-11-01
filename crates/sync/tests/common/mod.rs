//! Shared test support for the home-side tests: LightCraft-style sidecars, a fake renderer, a fixture.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use lightcraft_cloud_client::home::{Added, Home, HomeConfig, PhotoInfo, Renderer};
use lightcraft_cloud_client::{DirRemote, Error, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
pub const NS_LC: &str = "http://ns.lightcraft.app/lc/1.0/";
pub const NS_RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

pub fn sha16(b: &[u8]) -> String {
    sha_hex(b)[..16].to_string()
}

pub fn sha_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

pub fn settings() -> Value {
    serde_json::from_str(include_str!("settings.json")).unwrap()
}

/// A copy of the default settings with ("light.exposure", 0.5) style edits applied.
pub fn with_settings(edits: &[(&str, Value)]) -> Value {
    let mut s = settings();
    for (path, v) in edits {
        let mut d = &mut s;
        let parts: Vec<&str> = path.split('.').collect();
        for k in &parts[..parts.len() - 1] {
            d = d.get_mut(*k).unwrap();
        }
        d[parts[parts.len() - 1]] = v.clone();
    }
    s
}

#[derive(Clone, PartialEq)]
pub enum Lay {
    Compact,
    /// `json.dumps` default spacing
    Spaced,
    Indent(&'static str),
}

struct SpacedFmt;

impl serde_json::ser::Formatter for SpacedFmt {
    fn begin_array_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W, first: bool) -> std::io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }
    fn begin_object_key<W: ?Sized + std::io::Write>(&mut self, w: &mut W, first: bool) -> std::io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }
    fn begin_object_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        w.write_all(b": ")
    }
}

pub fn dump(v: &Value, lay: &Lay) -> String {
    use serde::Serialize;
    let mut out = Vec::new();
    match lay {
        Lay::Compact => return serde_json::to_string(v).unwrap(),
        Lay::Spaced => v.serialize(&mut serde_json::Serializer::with_formatter(&mut out, SpacedFmt)).unwrap(),
        Lay::Indent(i) => {
            v.serialize(&mut serde_json::Serializer::with_formatter(&mut out, serde_json::ser::PrettyFormatter::with_indent(i.as_bytes()))).unwrap()
        }
    }
    String::from_utf8(out).unwrap()
}

/// A LightCraft-style sidecar: properties as child elements, lc:settings as JSON, an xpacket wrapper.
#[derive(Clone)]
pub struct Sc {
    pub settings: Value,
    pub rating: i32,
    pub flag: String,
    pub extra: String,
    pub lay: Lay,
    pub packet: bool,
    pub decl: bool,
    pub tool: String,
}

pub fn sc() -> Sc {
    Sc {
        settings: settings(),
        rating: 0,
        flag: "none".into(),
        extra: String::new(),
        lay: Lay::Compact,
        packet: true,
        decl: false,
        tool: "LightCraft".into(),
    }
}

impl Sc {
    pub fn s(mut self, edits: &[(&str, Value)]) -> Self {
        self.settings = with_settings(edits);
        self
    }
    pub fn rating(mut self, r: i32) -> Self {
        self.rating = r;
        self
    }
    pub fn flag(mut self, f: &str) -> Self {
        self.flag = f.into();
        self
    }
    pub fn extra(mut self, e: &str) -> Self {
        self.extra = e.into();
        self
    }
    pub fn lay(mut self, l: Lay) -> Self {
        self.lay = l;
        self
    }
    pub fn decl(mut self) -> Self {
        self.decl = true;
        self
    }
    pub fn tool(mut self, t: &str) -> Self {
        self.tool = t.into();
        self
    }
    pub fn build(&self) -> Vec<u8> {
        let j = dump(&self.settings, &self.lay).replace('&', "&amp;").replace('<', "&lt;");
        let mut out = String::new();
        if self.decl {
            out += "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";
        }
        if self.packet {
            out += "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n";
        }
        out += &format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="LightCraft">
 <rdf:RDF xmlns:rdf="{NS_RDF}">
  <rdf:Description rdf:about=""
    xmlns:xmp="{NS_XMP}"
    xmlns:exif="http://ns.adobe.com/exif/1.0/"
    xmlns:lc="{NS_LC}">
   <xmp:CreatorTool>{}</xmp:CreatorTool>
   <xmp:Rating>{}</xmp:Rating>
   <exif:DateTimeOriginal>2026-10-27T23:51:26</exif:DateTimeOriginal>
   <lc:settings>{j}</lc:settings>
   <lc:flag>{}</lc:flag>{}
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#,
            self.tool, self.rating, self.flag, self.extra
        );
        if self.packet {
            out += "<?xpacket end=\"w\"?>";
        }
        out.into_bytes()
    }
}

pub fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&")
}

/// The text of the first `<qname>...</qname>` element.
pub fn text_of(doc: &[u8], qname: &str) -> Option<String> {
    let s = std::str::from_utf8(doc).unwrap();
    let a = s.find(&format!("<{qname}>"))? + qname.len() + 2;
    let b = s[a..].find(&format!("</{qname}>"))? + a;
    Some(unescape(&s[a..b]))
}

pub fn settings_of(doc: &[u8]) -> Value {
    serde_json::from_str(&text_of(doc, "lc:settings").unwrap()).unwrap()
}

pub fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

// ------------------------------------------------------------------ fake renderer

/// A renderer that derives everything from the file bytes, and remembers what it was asked.
#[derive(Default)]
pub struct Fake {
    /// path -> content hash of the photos in its "library"
    pub lib: HashMap<PathBuf, String>,
    pub calls: Vec<String>,
    pub fail_import: Vec<String>,
    pub fail_thumb: bool,
}

pub fn content_hash(bytes: &[u8]) -> String {
    sha_hex(bytes)[..32].to_string()
}

impl Fake {
    pub fn count(&self, what: &str) -> usize {
        self.calls.iter().filter(|c| c.starts_with(what)).count()
    }

    fn info(&self, photo: &Path) -> Result<PhotoInfo> {
        let bytes = std::fs::read(photo).map_err(|e| Error::Engine(format!("{}: {e}", photo.display())))?;
        let sidecar_flag = sidecar_of(photo).and_then(|s| text_of(&s, "lc:flag"));
        Ok(PhotoInfo {
            content_hash: content_hash(&bytes),
            captured: Some("2024-05-01T09:30:00+02:00".into()),
            camera: Some("ExampleCam X1".into()),
            lens: None,
            width: Some(6000),
            height: Some(4000),
            rating: sidecar_of(photo).and_then(|s| text_of(&s, "xmp:Rating")).and_then(|r| r.parse().ok()).or(Some(0)),
            flag: Some(sidecar_flag.unwrap_or_else(|| "none".into())),
            label: None,
        })
    }
}

/// The sidecar the engine would read for `photo`: `<stem>.xmp`, else `<file>.xmp`.
pub fn sidecar_of(photo: &Path) -> Option<Vec<u8>> {
    std::fs::read(photo.with_extension("xmp")).ok().or_else(|| {
        let mut p = photo.as_os_str().to_owned();
        p.push(".xmp");
        std::fs::read(PathBuf::from(p)).ok()
    })
}

impl Renderer for Fake {
    fn add(&mut self, photo: &Path) -> Result<Added> {
        self.calls.push(format!("add {}", photo.display()));
        let name = photo.to_string_lossy().into_owned();
        if self.fail_import.iter().any(|f| name.ends_with(f)) {
            return Err(Error::Engine("cannot decode".into()));
        }
        let i = self.info(photo)?;
        if self.lib.iter().any(|(p, h)| *h == i.content_hash && p != photo) {
            return Ok(Added::Duplicate);
        }
        self.lib.insert(photo.to_path_buf(), i.content_hash.clone());
        Ok(Added::Photo(i))
    }
    fn reread(&mut self, photo: &Path) -> Result<PhotoInfo> {
        self.calls.push(format!("reread {}", photo.display()));
        self.info(photo)
    }
    fn thumbnail(&mut self, photo: &Path, long_edge: u32, quality: u8, out: &Path) -> Result<()> {
        self.calls.push(format!("thumb {} {long_edge} {quality}", photo.display()));
        if self.fail_thumb {
            return Err(Error::Engine("render failed".into()));
        }
        let mut b = format!("thumb {long_edge} ").into_bytes();
        b.extend(self.info(photo)?.content_hash.bytes());
        b.extend(sidecar_of(photo).unwrap_or_default());
        std::fs::write(out, b).map_err(|e| Error::Engine(e.to_string()))
    }
    fn smart_preview(&mut self, photo: &Path, out: &Path) -> Result<String> {
        self.calls.push(format!("preview {}", photo.display()));
        let h = self.info(photo)?.content_hash;
        std::fs::write(out, format!("lcsp {h}")).map_err(|e| Error::Engine(e.to_string()))?;
        Ok(content_hash(h.as_bytes()))
    }
    fn forget(&mut self, photos: &[PathBuf]) -> Result<()> {
        for p in photos {
            self.calls.push(format!("forget {}", p.display()));
            self.lib.remove(p);
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ fixture

pub struct Fx {
    pub root: PathBuf,
    pub src: PathBuf,
    pub state: PathBuf,
    pub remote: PathBuf,
    pub home: Home,
    pub log: Arc<std::sync::Mutex<Vec<String>>>,
}

pub fn temp(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("lc-home-{tag}-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Write a file whose mtime is `age` seconds ago.
pub fn touch(root: &Path, rel: &str, data: &[u8], age: u64) -> PathBuf {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, data).unwrap();
    set_age(&p, age);
    p
}

pub fn set_age(p: &Path, age: u64) {
    let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(age)).unwrap();
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Fx {
    pub fn new(tag: &str) -> Fx {
        Fx::with(tag, |_| {})
    }

    pub fn with(tag: &str, f: impl FnOnce(&mut HomeConfig)) -> Fx {
        let root = temp(tag);
        let (src, state, remote) = (root.join("src"), root.join("state"), root.join("remote"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        let mut cfg = HomeConfig::new(&src, &state, "test-remote");
        f(&mut cfg);
        let mut home = Home::new(cfg, Arc::new(DirRemote::new(&remote))).unwrap();
        home.set_retry_delay(Duration::ZERO);
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let l = log.clone();
        home.set_logger(move |m| l.lock().unwrap().push(m.to_string()));
        Fx { root, src, state, remote, home, log }
    }

    pub fn touch(&self, rel: &str, data: &[u8], age: u64) -> PathBuf {
        touch(&self.src, rel, data, age)
    }

    pub fn lines(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    pub fn rfile(&self, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(self.remote.join(rel)).ok()
    }

    pub fn manifest(&self) -> Value {
        serde_json::from_slice(&self.rfile("mobile/manifest.json").expect("no manifest")).unwrap()
    }

    /// Every file below `dir` (relative), sorted.
    pub fn tree(&self, dir: &Path) -> Vec<String> {
        fn walk(base: &Path, d: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                if e.path().is_dir() {
                    walk(base, &e.path(), out);
                } else {
                    out.push(e.path().strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/"));
                }
            }
        }
        let mut v = Vec::new();
        walk(dir, dir, &mut v);
        v.sort();
        v
    }

    pub fn remote_tree(&self) -> Vec<String> {
        self.tree(&self.remote)
    }

    /// The history files below a remote folder, e.g. `history/<date>/originals`.
    pub fn history(&self) -> Vec<String> {
        self.remote_tree().into_iter().filter(|p| p.starts_with("history/")).collect()
    }

    /// A hash of everything below `dir`, for "nothing changed" checks.
    pub fn tree_hash(&self, dir: &Path) -> String {
        let mut h = Sha256::new();
        for p in self.tree(dir) {
            if p == "lock" {
                continue;
            }
            h.update(p.as_bytes());
            h.update(std::fs::read(dir.join(&p)).unwrap());
        }
        format!("{:x}", h.finalize())
    }

    pub fn put_inbox(&self, name: &str, data: &[u8]) {
        let p = self.remote.join("inbox").join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
}

pub fn j(v: f64) -> Value {
    json!(v)
}

// ------------------------------------------------------------------ a remote that counts and can fail

use lightcraft_cloud_client::{Remote, RemoteEntry};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Spy {
    pub inner: DirRemote,
    pub puts: Mutex<Vec<String>>,
    pub moves: Mutex<Vec<(String, String)>>,
    /// The next this-many puts fail with a remote error.
    pub fail_puts: AtomicUsize,
    /// Moves whose destination contains this fail (always).
    pub fail_move_to: Mutex<Option<String>>,
    /// The next this-many gets fail with a remote error.
    pub fail_gets: AtomicUsize,
    /// Listings of a folder starting with this fail with a remote error (always).
    pub fail_list_of: Mutex<Option<String>>,
}

impl Spy {
    pub fn new(root: &Path) -> Arc<Spy> {
        Arc::new(Spy {
            inner: DirRemote::new(root),
            puts: Mutex::default(),
            moves: Mutex::default(),
            fail_puts: AtomicUsize::new(0),
            fail_move_to: Mutex::default(),
            fail_gets: AtomicUsize::new(0),
            fail_list_of: Mutex::default(),
        })
    }
    pub fn puts(&self) -> Vec<String> {
        std::mem::take(&mut *self.puts.lock().unwrap())
    }
    pub fn moves(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.moves.lock().unwrap())
    }
}

impl Remote for Spy {
    fn list(&self, dir: &str, recursive: bool) -> Result<Vec<RemoteEntry>> {
        if self.fail_list_of.lock().unwrap().as_ref().is_some_and(|f| dir.starts_with(f.as_str())) {
            return Err(Error::Remote("offline".into()));
        }
        self.inner.list(dir, recursive)
    }
    fn get(&self, path: &str, local: &Path) -> Result<()> {
        if self.fail_gets.load(Ordering::SeqCst) > 0 {
            self.fail_gets.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::Remote("flaky get".into()));
        }
        self.inner.get(path, local)
    }
    fn put(&self, local: &Path, path: &str) -> Result<()> {
        if self.fail_puts.load(Ordering::SeqCst) > 0 {
            self.fail_puts.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::Remote("flaky".into()));
        }
        self.puts.lock().unwrap().push(path.to_string());
        self.inner.put(local, path)
    }
    fn move_file(&self, from: &str, to: &str) -> Result<()> {
        if self.fail_move_to.lock().unwrap().as_ref().is_some_and(|f| to.contains(f.as_str())) {
            return Err(Error::Remote("boom".into()));
        }
        self.moves.lock().unwrap().push((from.to_string(), to.to_string()));
        self.inner.move_file(from, to)
    }
}

impl Fx {
    /// The same fixture, with a [`Spy`] as the remote.
    pub fn spied(tag: &str, f: impl FnOnce(&mut HomeConfig)) -> (Fx, Arc<Spy>) {
        let mut fx = Fx::with(tag, f);
        let spy = Spy::new(&fx.remote);
        fx.rehome(spy.clone(), "test-remote");
        (fx, spy)
    }

    /// A new `Home` over the same folders with another remote object / label (a changed config).
    pub fn rehome(&mut self, remote: Arc<dyn Remote>, label: &str) {
        let mut cfg = self.home.config().clone();
        cfg.remote_label = label.into();
        let mut home = Home::new(cfg, remote).unwrap();
        home.set_retry_delay(Duration::ZERO);
        let l = self.log.clone();
        home.set_logger(move |m| l.lock().unwrap().push(m.to_string()));
        self.home = home;
    }
}

// ------------------------------------------------------------------ a renderer with the real content hash

use lightcraft_cloud_client::home::content_hash_bytes;

/// A renderer that describes photos by their real content hash (the id the other PC checks its downloads against).
#[derive(Default)]
pub struct HashRenderer {
    pub seen: std::collections::HashMap<PathBuf, String>,
}

impl HashRenderer {
    fn info(photo: &Path) -> Result<PhotoInfo> {
        let bytes = std::fs::read(photo).map_err(|e| Error::Engine(format!("{}: {e}", photo.display())))?;
        Ok(PhotoInfo { content_hash: content_hash_bytes(&bytes), width: Some(6000), height: Some(4000), rating: Some(0), ..PhotoInfo::default() })
    }
}

impl Renderer for HashRenderer {
    fn add(&mut self, photo: &Path) -> Result<Added> {
        let i = Self::info(photo)?;
        self.seen.insert(photo.to_path_buf(), i.content_hash.clone());
        Ok(Added::Photo(i))
    }
    fn reread(&mut self, photo: &Path) -> Result<PhotoInfo> {
        Self::info(photo)
    }
    fn thumbnail(&mut self, _photo: &Path, _long_edge: u32, _quality: u8, out: &Path) -> Result<()> {
        std::fs::write(out, b"thumb").map_err(|e| Error::Engine(e.to_string()))
    }
    fn smart_preview(&mut self, photo: &Path, out: &Path) -> Result<String> {
        let h = Self::info(photo)?.content_hash;
        std::fs::write(out, format!("lcsp {h}")).map_err(|e| Error::Engine(e.to_string()))?;
        Ok(content_hash_bytes(h.as_bytes()))
    }
    fn forget(&mut self, photos: &[PathBuf]) -> Result<()> {
        for p in photos {
            self.seen.remove(p);
        }
        Ok(())
    }
}

