//! Photos this PC imports itself, from a card or any folder, and sends to the home PC (FORMAT.md, "Imports").
//!
//! Three parts, each safe to stop at any point:
//!
//! 1. **Scan** ([`Other::import_scan`]): walk the source (never modified), hash every photo with LightCraft's content hash, and sort
//!    them into new ones and ones the cloud library, this PC's records or the upload queue already have. Slow on a card, so it reports
//!    progress and can be cancelled.
//! 2. **Copy** ([`Other::import_run`]): each new photo goes into `<photo folder>/<destination>/` with the same safety as the home PC's
//!    adoption: a temporary file beside the target, `fsync`, the bytes hashed again after they were written (cards lie), a sidecar
//!    before its photo, and a link that fails if the name exists (a name clash gets `-2`, `-3`, ..., the sidecar named to match;
//!    nothing is ever overwritten). The photo is then entered in the **upload queue** and LightCraft is told. A run that was cut off
//!    is finished by running it again: what arrived is found by its bytes and skipped.
//! 3. **Upload** ([`Other::upload_step`]): a bounded amount of work per call, so the app's other jobs (sidecar edits) are never kept
//!    waiting. Per photo: its bytes, its sidecar, then the commit note `imports/<device>/<id>.json` last; a file already on the remote at
//!    the right size is not sent again. Once the home PC has filed the import away, `imports-done/<day>/<device>/<id>.result.json` says
//!    what became of it; see [`OutEntry`] and the module docs of [`super`] for what the pass does with the id after that.
//!
//! The queue is one small file per photo in `<state>/imports-out/` (the import window and the app are different processes, and a file
//! each means neither overwrites the other's work). The sidecar is kept there as it was when the photo was imported, so an edit made
//! afterwards is an edit (sent through the inbox once the home PC has the photo), never part of the import.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::pass::{human, push_unique};
use super::state::State;
use super::{Other, fetch_manifest};
use crate::home::{
    ContentHasher, ImportNote, RunLock, bad_component, check_import_target, content_hash_file, create_new_file, entry_named, find_sidecars, has_bytes, is_photo,
    is_skipped_name, is_variant_of, rel_join, stem, validate_xmp, variant,
};
use crate::util::{free_space, iso, link_no_replace, parse_iso, sha256_hex, sync_dir, write_atomic};
use crate::{Error, Result};

/// The queue's folder below the state folder.
pub const OUT_DIR: &str = "imports-out";
/// The XMP validator's limit (the home PC rejects a bigger sidecar).
const MAX_SIDECAR: u64 = 2_000_000;
/// A name of a copy in progress: hidden, so a scan of the photo folder never sees it.
const STAGING_PREFIX: &str = ".lc-import-";

// ------------------------------------------------------------------ the queue

/// What the home PC said about an import (`imports-done/.../<id>.result.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutResult {
    /// `added`, `duplicate` or `rejected`.
    pub status: String,
    /// The photo's final path at the home PC (relative to `originals/`), for `added` and `duplicate`.
    pub path: Option<String>,
    pub reason: Option<String>,
}

impl OutResult {
    pub fn is_rejected(&self) -> bool {
        self.status == "rejected"
    }
}

/// One photo that was imported on this PC and goes to the home PC.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OutEntry {
    /// LightCraft's content hash of the photo's bytes.
    pub id: String,
    /// The photo's folder in the photo folder, `/`-separated (the destination the user chose).
    pub folder: String,
    /// The photo's file name here (after any `-2`), which is the name the note announces.
    pub name: String,
    pub size: u64,
    /// UTC, as the note writes it.
    pub mtime: String,
    /// The sidecar's file name here, if the photo had one when it was imported (it is in `<id>.xmp` beside this file).
    pub sidecar: Option<String>,
    /// SHA-256 of that sidecar's bytes: what the home PC gets, and the bytes later edits are measured against.
    pub sidecar_sha: Option<String>,
    pub sidecar_size: u64,
    /// When it was queued (ISO).
    pub queued: String,
    /// When its commit note was written (ISO): the home PC has everything it needs.
    pub uploaded: Option<String>,
    /// What the home PC did with it.
    pub result: Option<OutResult>,
    /// LightCraft was told about the photo (the same durable list the downloads use).
    pub announced: bool,
    /// A rejection was given to the user (once).
    pub reported: bool,
}

impl OutEntry {
    /// The photo's path below the photo folder.
    pub fn path(&self) -> String {
        rel_join(&self.folder, &self.name)
    }

    /// The sidecar's path below the photo folder.
    pub fn sidecar_path(&self) -> Option<String> {
        self.sidecar.as_ref().map(|s| rel_join(&self.folder, s))
    }

    fn waiting_to_upload(&self) -> bool {
        self.uploaded.is_none() && self.result.is_none()
    }

    fn note(&self) -> serde_json::Value {
        json!({"format": 1, "id": self.id, "folder": self.folder, "name": self.name, "size": self.size, "mtime": self.mtime, "sidecar": self.sidecar})
    }
}

/// The numbers the Status window shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueueStatus {
    /// Photos not sent yet.
    pub to_upload: usize,
    pub bytes: u64,
    /// Photos sent whose result has not been seen yet.
    pub awaiting_home: usize,
    /// Rejections not given to the user yet.
    pub unreported: usize,
}

/// The upload queue: a folder of one file per photo.
#[derive(Debug, Clone)]
pub struct ImportQueue {
    dir: PathBuf,
}

fn is_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl ImportQueue {
    pub fn new(state_dir: &Path) -> ImportQueue {
        ImportQueue { dir: state_dir.join(OUT_DIR) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn json(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn xmp(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.xmp"))
    }

    /// Every entry, oldest first. A file that is not an entry is ignored.
    pub fn list(&self) -> Vec<OutEntry> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut v: Vec<OutEntry> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let id = name.strip_suffix(".json").filter(|i| is_id(i))?.to_string();
                let entry: OutEntry = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
                (entry.id == id).then_some(entry)
            })
            .collect();
        v.sort_by(|a, b| (&a.queued, &a.id).cmp(&(&b.queued, &b.id)));
        v
    }

    pub fn get(&self, id: &str) -> Option<OutEntry> {
        is_id(id).then(|| std::fs::read(self.json(id)).ok().and_then(|b| serde_json::from_slice(&b).ok())).flatten()
    }

    pub fn save(&self, e: &OutEntry) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(e).map_err(|x| Error::Io(format!("import queue: {x}")))?;
        write_atomic(&self.json(&e.id), &bytes)
    }

    /// Queue a photo. The sidecar's bytes are kept beside the entry first, so an entry never names a snapshot that is not there.
    pub fn add(&self, e: &OutEntry, sidecar: Option<&[u8]>) -> Result<()> {
        if let Some(b) = sidecar {
            write_atomic(&self.xmp(&e.id), b)?;
        }
        self.save(e)
    }

    pub fn remove(&self, id: &str) {
        let _ = std::fs::remove_file(self.json(id));
        let _ = std::fs::remove_file(self.xmp(id));
    }

    /// The sidecar as it was when the photo was imported.
    pub fn snapshot(&self, id: &str) -> Option<Vec<u8>> {
        std::fs::read(self.xmp(id)).ok()
    }

    pub fn status(&self) -> QueueStatus {
        let mut s = QueueStatus::default();
        for e in self.list() {
            if e.waiting_to_upload() {
                s.to_upload += 1;
                s.bytes += e.size + e.sidecar_size;
            } else if e.result.is_none() {
                s.awaiting_home += 1;
            }
            if e.result.as_ref().is_some_and(OutResult::is_rejected) && !e.reported {
                s.unreported += 1;
            }
        }
        s
    }

    /// One copy at a time, across processes.
    fn lock(&self) -> Result<std::fs::File> {
        std::fs::create_dir_all(&self.dir).map_err(|e| Error::io(self.dir.display(), e))?;
        let p = self.dir.join(".copy.lock");
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&p).map_err(|e| Error::io(p.display(), e))?;
        f.try_lock().map_err(|_| Error::Busy("Another import is running.".into()))?;
        Ok(f)
    }
}

// ------------------------------------------------------------------ the destination

/// Check the name the user typed for the destination folder, with the rule the home PC applies to a note (so the window refuses what the
/// home PC would reject). Returns the folder as it will be used: trimmed, no trailing `/`. Err: what to tell the user.
pub fn check_destination(dest: &str, exclude: &[String]) -> std::result::Result<String, String> {
    let d = dest.trim().trim_end_matches('/').trim();
    if d.is_empty() {
        return Err("Type a name for the folder the photos go into, for example 261101 - Lisbon.".into());
    }
    check_import_target(d, "photo.jpg", exclude).map(|()| d.to_string()).map_err(|why| format!("The home PC would not accept this folder: {why}."))
}

// ------------------------------------------------------------------ progress

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Looking,
    Hashing,
    Copying,
}

/// How far a scan or a copy is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    pub phase: Phase,
    pub done: usize,
    pub total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// The file being worked on.
    pub current: String,
}

// ------------------------------------------------------------------ the scan

/// A sidecar next to a photo on the source.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSidecar {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
}

/// A photo of the source that is not in the library yet.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanFile {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub mtime: SystemTime,
    /// LightCraft's content hash.
    pub id: String,
    pub sidecar: Option<SourceSidecar>,
    /// Already in the destination folder (an earlier run was cut off): the path there, relative to the photo folder.
    pub arrived: Option<String>,
}

/// What a scan found.
#[derive(Debug, Clone, Default)]
pub struct ImportScan {
    pub source: PathBuf,
    /// The destination folder, as it will be used.
    pub dest: String,
    pub new: Vec<ScanFile>,
    /// Photos the cloud library has.
    pub in_library: usize,
    /// Photos this PC has (its records, or the upload queue).
    pub on_this_pc: usize,
    /// The same photo twice on the source: only one is imported.
    pub twins: usize,
    /// Files that cannot be imported, with the reason.
    pub skipped: Vec<String>,
    /// Bytes still to copy (photos and sidecars; what arrived already is not counted).
    pub new_bytes: u64,
    /// Free space where the photos go (None: unknown).
    pub free: Option<u64>,
    /// The margin that is kept free.
    pub margin: u64,
    /// The cloud library's list of photos was read (else only this PC's records were checked).
    pub library_checked: bool,
    pub cancelled: bool,
    /// Files of photo types seen on the source.
    pub photos_seen: usize,
}

impl ImportScan {
    /// Photos that were already known (library or this PC).
    pub fn known(&self) -> usize {
        self.in_library + self.on_this_pc
    }

    /// Why the import cannot start for want of disk space.
    pub fn space_problem(&self) -> Option<String> {
        let free = self.free?;
        let need = self.new_bytes.saturating_add(self.margin);
        (free < need).then(|| {
            format!(
                "Not enough free space: the photos need {} and {} is kept free, but only {} is free.",
                human(self.new_bytes),
                human(self.margin),
                human(free)
            )
        })
    }
}

struct Found {
    path: PathBuf,
    name: String,
    size: u64,
    mtime: SystemTime,
    sidecar: Option<SourceSidecar>,
}

/// Every photo below `root` (hidden files and folders and links are not followed), with the sidecar next to it. `None`: cancelled.
fn walk_source(root: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut(&Progress), skipped: &mut Vec<String>) -> Option<Vec<Found>> {
    let mut out: Vec<Found> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut p = Progress::default();
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                skipped.push(format!("{}: cannot be read ({e})", dir.display()));
                continue;
            }
        };
        let mut names: BTreeSet<String> = BTreeSet::new();
        let mut here: Vec<(String, PathBuf)> = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if is_skipped_name(&name) {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                names.insert(name.clone());
                here.push((name, e.path()));
            }
        }
        let photos: Vec<String> = here.iter().filter(|(n, _)| is_photo(n)).map(|(n, _)| n.clone()).collect();
        let (sidecars, _) = find_sidecars(&photos, &names);
        for (name, path) in here.into_iter().filter(|(n, _)| is_photo(n)) {
            let Ok(md) = std::fs::metadata(&path) else {
                skipped.push(format!("{name}: cannot be read"));
                continue;
            };
            let sidecar = sidecars.get(&name).cloned().flatten().map(|sn| {
                let sp = path.with_file_name(&sn);
                let size = std::fs::metadata(&sp).map_or(0, |m| m.len());
                SourceSidecar { path: sp, name: sn, size }
            });
            out.push(Found { path, name, size: md.len(), mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH), sidecar });
        }
        p.done = out.len();
        p.current = dir.display().to_string();
        progress(&p);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Some(out)
}

/// The content hash and length of a file, reported as it is read. `Ok(None)`: cancelled.
fn hash_file(path: &Path, cancel: &AtomicBool, mut on_bytes: impl FnMut(u64)) -> std::io::Result<Option<(String, u64)>> {
    let mut f = std::fs::File::open(path)?;
    let mut h = ContentHasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let k = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        h.update(&buf[..k]);
        n += k as u64;
        on_bytes(k as u64);
    }
    Ok(Some((h.finish(), n)))
}

fn note_for(f: &Found, id: &str) -> ImportNote {
    let sidecar = f.sidecar.as_ref().map(|s| sidecar_style(&f.name, &s.name));
    ImportNote { id: id.to_string(), folder: String::new(), name: f.name.clone(), size: f.size, mtime: f.mtime, sidecar }
}

/// A sidecar's name in the way the engine names it for `photo`: `<stem>.xmp` when the source's was named for the stem, else `<name>.xmp`.
fn sidecar_style(photo: &str, source_sidecar: &str) -> String {
    if source_sidecar.to_lowercase() == format!("{}.xmp", stem(photo).to_lowercase()) { format!("{}.xmp", stem(photo)) } else { format!("{photo}.xmp") }
}

/// The folders to offer as a destination: the `chosen` ones and those that exist below `photo_folder` (two levels deep), without hidden
/// ones, ones the home PC would not accept (`exclude`, bad names), sorted.
pub fn existing_folders(photo_folder: &Path, chosen: &[String], exclude: &[String]) -> Vec<String> {
    let mut set: BTreeSet<String> = chosen.iter().filter(|f| !f.is_empty()).cloned().collect();
    let mut stack: Vec<(PathBuf, String, usize)> = vec![(photo_folder.to_path_buf(), String::new(), 0)];
    while let Some((dir, rel, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !e.file_type().is_ok_and(|t| t.is_dir()) || bad_component(&name).is_some() {
                continue;
            }
            let r = rel_join(&rel, &name);
            if check_import_target(&r, "photo.jpg", exclude).is_err() {
                continue;
            }
            set.insert(r.clone());
            if depth < 1 {
                stack.push((e.path(), r, depth + 1));
            }
        }
    }
    set.into_iter().collect()
}

impl Other {
    fn out_queue(&self) -> ImportQueue {
        ImportQueue::new(&self.cfg.state_dir)
    }

    /// The numbers of the upload queue, from the state folder alone (no network).
    pub fn import_queue_status(&self) -> QueueStatus {
        self.out_queue().status()
    }

    /// Check a destination folder name the way the home PC will.
    pub fn check_destination(&self, dest: &str) -> std::result::Result<String, String> {
        check_destination(dest, &self.cfg.exclude)
    }

    fn dest_dir(&self, dest: &str) -> PathBuf {
        let mut p = self.cfg.source.clone();
        p.extend(dest.split('/'));
        p
    }

    /// Folders to offer as a destination: the chosen folders and the folders that exist in the photo folder (two levels deep).
    pub fn existing_folders(&self) -> Vec<String> {
        existing_folders(&self.cfg.source, &self.cfg.folders, &self.cfg.exclude)
    }

    /// Look at `source` (recursively) for photos to import into `dest`. Reads the source and the cloud library's manifest; writes
    /// nothing. The cloud library being out of reach is not an error: only this PC's records are checked then
    /// (`library_checked` is false). A cancelled scan returns what it had, with `cancelled` set.
    pub fn import_scan(&self, source: &Path, dest: &str, progress: &mut dyn FnMut(&Progress), cancel: &AtomicBool) -> Result<ImportScan> {
        let dest = self.check_destination(dest).map_err(Error::Invalid)?;
        if !source.is_dir() {
            return Err(Error::Invalid(format!("{} is not a folder.", source.display())));
        }
        let dest_dir = self.dest_dir(&dest);
        if let (Ok(s), Ok(d)) = (std::fs::canonicalize(source), std::fs::canonicalize(&dest_dir))
            && s.starts_with(&d)
        {
            return Err(Error::Invalid("The folder you are importing from is the destination folder.".into()));
        }
        let mut scan = ImportScan { source: source.to_path_buf(), dest: dest.clone(), margin: self.cfg.free_margin, ..ImportScan::default() };

        // what is already known
        let mut in_lib: HashSet<String> = HashSet::new();
        let tmp = self.cfg.state_dir.join(format!("import-manifest.json.tmp-{}", std::process::id()));
        if let Ok(parsed) = fetch_manifest(&*self.remote, &tmp) {
            in_lib.extend(parsed.items.into_iter().map(|i| i.id));
            scan.library_checked = true;
        }
        let queue = self.out_queue();
        let mut here: HashSet<String> = queue.list().into_iter().map(|e| e.id).collect();
        if let Ok(st) = State::open(&self.cfg.state_dir) {
            here.extend(st.photos.keys().cloned());
        }

        let Some(found) = walk_source(source, cancel, progress, &mut scan.skipped) else {
            scan.cancelled = true;
            return Ok(scan);
        };
        let dest_names = std::fs::read_dir(&dest_dir).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>()).unwrap_or_default();
        let mut p = Progress { phase: Phase::Hashing, total: found.len(), bytes_total: found.iter().map(|f| f.size).sum(), ..Progress::default() };
        let mut seen: HashSet<String> = HashSet::new();
        for f in found {
            if let Err(why) = check_import_target(&dest, &f.name, &self.cfg.exclude) {
                scan.skipped.push(format!("{}: {why}", f.name));
                continue;
            }
            scan.photos_seen += 1;
            p.current = f.name.clone();
            progress(&p);
            if f.size == 0 {
                scan.skipped.push(format!("{}: the file is empty", f.name));
                p.done += 1;
                continue;
            }
            let before = p.bytes_done;
            let hashed = hash_file(&f.path, cancel, |n| {
                p.bytes_done += n;
                progress(&p);
            });
            let (id, len) = match hashed {
                Ok(Some(x)) => x,
                Ok(None) => {
                    scan.cancelled = true;
                    return Ok(scan);
                }
                Err(e) => {
                    p.bytes_done = before + f.size;
                    scan.skipped.push(format!("{}: cannot be read ({e})", f.name));
                    p.done += 1;
                    continue;
                }
            };
            p.done += 1;
            if len != f.size {
                scan.skipped.push(format!("{}: changed while it was read; run the scan again", f.name));
                continue;
            }
            if !seen.insert(id.clone()) {
                scan.twins += 1;
                continue;
            }
            if in_lib.contains(&id) {
                scan.in_library += 1;
                continue;
            }
            if here.contains(&id) {
                scan.on_this_pc += 1;
                continue;
            }
            // a copy an earlier, cut-off run left in the destination folder (the same bytes under the name or a numbered variant)
            let note = note_for(&f, &id);
            let arrived = dest_names.iter().filter(|n| is_variant_of(&note, n)).find(|n| has_bytes(&dest_dir.join(n), f.size, &id)).map(|n| rel_join(&dest, n));
            let mut sidecar = f.sidecar;
            if let Some(sc) = &sidecar {
                let bad = if sc.size > MAX_SIDECAR {
                    Some("it is bigger than 2 MB".to_string())
                } else {
                    match std::fs::read(&sc.path) {
                        Ok(b) => validate_xmp(&b),
                        Err(e) => Some(e.to_string()),
                    }
                };
                if let Some(why) = bad {
                    scan.skipped.push(format!("{}: the sidecar is not used ({why}); the photo is imported without it", sc.name));
                    sidecar = None;
                }
            }
            if arrived.is_none() {
                scan.new_bytes += f.size + sidecar.as_ref().map_or(0, |s| s.size);
            }
            scan.new.push(ScanFile { path: f.path, name: f.name, size: f.size, mtime: f.mtime, id, sidecar, arrived });
        }
        scan.free = free_space(&dest_dir);
        Ok(scan)
    }
}

// ------------------------------------------------------------------ the copy

/// What a copy did.
#[derive(Debug, Clone, Default)]
pub struct ImportRun {
    pub dest: String,
    /// Photos copied into the destination folder.
    pub copied: usize,
    /// Photos an earlier run had copied, now in the queue.
    pub resumed: usize,
    pub bytes: u64,
    /// Photos that got another name because theirs was taken: (name on the source, name here).
    pub renamed: Vec<(String, String)>,
    /// Photos that could not be imported, with the reason.
    pub failed: Vec<String>,
    /// Other things worth saying (a sidecar that was kept, ...).
    pub notes: Vec<String>,
    pub cancelled: bool,
    /// Set when the copy stopped for want of disk space.
    pub no_space: Option<String>,
    /// Photos left to do when it stopped early (cancel, no space).
    pub left: usize,
}

enum CopyFail {
    Cancelled,
    Bad(String),
}

/// Copy `src` to `dst` (which must not exist), hashing what is read; then read `dst` back and hash that. Both must be `id` and `size`.
fn copy_verified(src: &Path, dst: &Path, id: &str, size: u64, cancel: &AtomicBool, on_bytes: &mut dyn FnMut(u64)) -> std::result::Result<(), CopyFail> {
    let bad = |what: &str, e: std::io::Error| CopyFail::Bad(format!("{what}: {e}"));
    let mut input = std::fs::File::open(src).map_err(|e| bad("cannot be read", e))?;
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(dst).map_err(|e| bad("cannot be written", e))?;
    let r = (|| {
        let mut h = ContentHasher::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut n = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(CopyFail::Cancelled);
            }
            let k = match input.read(&mut buf) {
                Ok(0) => break,
                Ok(k) => k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(bad("cannot be read", e)),
            };
            h.update(&buf[..k]);
            out.write_all(&buf[..k]).map_err(|e| bad("cannot be written", e))?;
            n += k as u64;
            on_bytes(k as u64);
        }
        out.sync_all().map_err(|e| bad("cannot be written", e))?;
        if n != size || h.finish() != id {
            return Err(CopyFail::Bad("the card gave different bytes than the scan saw (a bad card or cable?); it was not imported".into()));
        }
        // (what is on disk, not what was meant to be written)
        match content_hash_file(dst) {
            Ok((h, len)) if h == id && len == size => Ok(()),
            Ok(_) => Err(CopyFail::Bad("the copy on this PC does not match the original (a bad disk?); it was not kept".into())),
            Err(e) => Err(CopyFail::Bad(e.to_string())),
        }
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(dst);
    }
    r
}

struct Placed {
    name: String,
    sidecar: Option<String>,
    kept_reason: Option<String>,
    /// The bytes were already there under this name.
    existing: bool,
}

/// Put the verified `staged` file in `dir` under the note's name, or its first free numbered one; the sidecar first. Never replaces.
/// The same loop as the home PC's adoption (`Home::place`), on the same helpers.
fn place_here(dir: &Path, note: &ImportNote, staged: &Path, mtime: SystemTime, sidecar: Option<&[u8]>) -> std::result::Result<Placed, String> {
    let io = |what: &str, e: std::io::Error| format!("{what} in {}: {e}", dir.display());
    let (mut n, mut races) = (1u32, 0u32);
    while n <= 10_000 && races < 20 {
        let (pname, sname) = variant(note, n);
        if let Some(real) = entry_named(dir, &pname).map_err(|e| io("cannot list", e))? {
            if has_bytes(&dir.join(&real), note.size, &note.id) {
                return Ok(Placed { name: real, sidecar: sname, kept_reason: None, existing: true });
            }
            n += 1;
            continue;
        }
        let (mut wrote_sidecar, mut kept_reason) = (false, None);
        if let (Some(sn), Some(bytes)) = (&sname, sidecar) {
            let taken = match entry_named(dir, sn).map_err(|e| io("cannot list", e))? {
                Some(real) => Some(real),
                None => match create_new_file(dir, sn, |f| f.write_all(bytes)) {
                    Ok(()) => {
                        wrote_sidecar = true;
                        None
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        races += 1;
                        continue;
                    }
                    Err(e) => return Err(io("cannot write the sidecar", e)),
                },
            };
            if let Some(real) = taken {
                let same = std::fs::read(dir.join(&real)).is_ok_and(|b| b == bytes);
                if !same && n > 1 {
                    n += 1;
                    continue;
                }
                if !same {
                    kept_reason = Some(format!("the sidecar {real} was already there with other contents and was kept"));
                }
            }
        }
        // (the staged file is the photo; its time is the source's)
        let _ = std::fs::OpenOptions::new().write(true).open(staged).and_then(|f| f.set_modified(mtime));
        let dest = dir.join(&pname);
        match link_no_replace(staged, &dest) {
            Ok(()) => {
                let _ = std::fs::remove_file(staged);
                sync_dir(dir);
                return Ok(Placed { name: pname, sidecar: sname, kept_reason, existing: false });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if wrote_sidecar && let Some(sn) = &sname {
                    let _ = std::fs::remove_file(dir.join(sn));
                }
                races += 1;
            }
            Err(e) => return Err(io("cannot write the photo", e)),
        }
    }
    Err(format!("no free name for {} in {}", note.name, dir.display()))
}

fn remove_stale_staging(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with(STAGING_PREFIX) && n.ends_with(".part") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

impl Other {
    /// Copy the new photos of a scan into `<photo folder>/<destination>/` and queue them for the home PC. Run it again after a cut-off:
    /// what arrived is not copied twice. The source is only read. Stops early (cancel, no disk space) without undoing anything.
    pub fn import_run(&self, scan: &ImportScan, progress: &mut dyn FnMut(&Progress), cancel: &AtomicBool) -> Result<ImportRun> {
        let dest = self.check_destination(&scan.dest).map_err(Error::Invalid)?;
        let queue = self.out_queue();
        let _lock = queue.lock()?;
        let mut run = ImportRun { dest: dest.clone(), ..ImportRun::default() };
        if !self.cfg.source.is_dir() {
            return Err(Error::Invalid(format!("The photo folder {} does not exist (drive not mounted?).", self.cfg.source.display())));
        }
        let dir = self.dest_dir(&dest);
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(dir.display(), e))?;
        // (links must not lead out of the photo folder)
        let (root, real) = (std::fs::canonicalize(&self.cfg.source), std::fs::canonicalize(&dir));
        match (root, real) {
            (Ok(r), Ok(d)) if d.starts_with(&r) && d != r => {}
            _ => return Err(Error::Invalid(format!("The folder {dest:?} leads outside the photo folder."))),
        }
        remove_stale_staging(&dir);
        let mut p = Progress { phase: Phase::Copying, total: scan.new.len(), bytes_total: scan.new_bytes, ..Progress::default() };
        let mut already: HashSet<String> = queue.list().into_iter().map(|e| e.id).collect();
        for (i, f) in scan.new.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                run.cancelled = true;
                run.left = scan.new.len() - i;
                break;
            }
            p.current = f.name.clone();
            progress(&p);
            if !already.insert(f.id.clone()) {
                p.done += 1;
                continue; // (queued meanwhile, by an earlier run)
            }
            let mut sidecar_bytes: Option<Vec<u8>> = None;
            if let Some(sc) = &f.sidecar {
                match std::fs::read(&sc.path) {
                    Ok(b) if b.len() as u64 <= MAX_SIDECAR && validate_xmp(&b).is_none() => sidecar_bytes = Some(b),
                    Ok(_) => run.notes.push(format!("{}: the sidecar is not acceptable; the photo was imported without it", sc.name)),
                    Err(e) => run.notes.push(format!("{}: the sidecar cannot be read ({e}); the photo was imported without it", sc.name)),
                }
            }
            let sc_len = sidecar_bytes.as_ref().map_or(0, |b| b.len() as u64);
            let mut note = ImportNote {
                id: f.id.clone(),
                folder: dest.clone(),
                name: f.name.clone(),
                size: f.size,
                mtime: f.mtime,
                sidecar: f.sidecar.as_ref().filter(|_| sidecar_bytes.is_some()).map(|s| sidecar_style(&f.name, &s.name)),
            };
            note.mtime = note.mtime.min(SystemTime::now());
            let placed = if let Some(rel) = f.arrived.as_ref().filter(|r| has_bytes(&self.cfg.source.join(r.replace('/', std::path::MAIN_SEPARATOR_STR)), f.size, &f.id)) {
                run.resumed += 1;
                Placed { name: rel.rsplit('/').next().unwrap_or(rel).to_string(), sidecar: note.sidecar.clone(), kept_reason: None, existing: true }
            } else {
                if let Some(free) = free_space(&dir)
                    && free < f.size.saturating_add(sc_len).saturating_add(self.cfg.free_margin)
                {
                    run.no_space = Some(format!(
                        "Not enough free space for {} ({} needed, {} free, and {} is kept free). {} not imported.",
                        f.name,
                        human(f.size + sc_len),
                        human(free),
                        human(self.cfg.free_margin),
                        if scan.new.len() - i == 1 { "1 photo was".to_string() } else { format!("{} photos were", scan.new.len() - i) }
                    ));
                    run.left = scan.new.len() - i;
                    already.remove(&f.id);
                    break;
                }
                let staged = dir.join(format!("{STAGING_PREFIX}{}.part", f.id));
                let _ = std::fs::remove_file(&staged);
                let base = p.bytes_done;
                let mut got = 0u64;
                let copied = copy_verified(&f.path, &staged, &f.id, f.size, cancel, &mut |n| {
                    got += n;
                    p.bytes_done = base + got;
                    progress(&p);
                });
                match copied {
                    Ok(()) => {}
                    Err(CopyFail::Cancelled) => {
                        run.cancelled = true;
                        run.left = scan.new.len() - i;
                        already.remove(&f.id);
                        break;
                    }
                    Err(CopyFail::Bad(why)) => {
                        run.failed.push(format!("{}: {why}", f.name));
                        already.remove(&f.id);
                        p.done += 1;
                        continue;
                    }
                }
                match place_here(&dir, &note, &staged, note.mtime, sidecar_bytes.as_deref()) {
                    Ok(pl) => {
                        let _ = std::fs::remove_file(&staged);
                        pl
                    }
                    Err(why) => {
                        let _ = std::fs::remove_file(&staged);
                        run.failed.push(format!("{}: {why}", f.name));
                        already.remove(&f.id);
                        p.done += 1;
                        continue;
                    }
                }
            };
            if !placed.existing {
                run.copied += 1;
                run.bytes += f.size;
            }
            if placed.name != f.name {
                run.renamed.push((f.name.clone(), placed.name.clone()));
            }
            if let Some(r) = &placed.kept_reason {
                run.notes.push(format!("{}: {r}", placed.name));
            }
            // the sidecar that goes along is whatever sits beside the photo now, if the home PC would accept it
            let (mut sidecar, mut snapshot) = (None, None);
            if let Some(sn) = &placed.sidecar
                && let Ok(b) = std::fs::read(dir.join(sn))
                && b.len() as u64 <= MAX_SIDECAR
                && validate_xmp(&b).is_none()
            {
                sidecar = Some(sn.clone());
                snapshot = Some(b);
            }
            let final_path = dir.join(&placed.name);
            let mtime = std::fs::metadata(&final_path).and_then(|m| m.modified()).unwrap_or(note.mtime).min(SystemTime::now());
            let entry = OutEntry {
                id: f.id.clone(),
                folder: dest.clone(),
                name: placed.name.clone(),
                size: f.size,
                mtime: iso(mtime),
                sidecar,
                sidecar_sha: snapshot.as_ref().map(|b| sha256_hex(b)),
                sidecar_size: snapshot.as_ref().map_or(0, |b| b.len() as u64),
                queued: iso(SystemTime::now()),
                announced: false,
                ..OutEntry::default()
            };
            queue.add(&entry, snapshot.as_deref())?;
            // (LightCraft is told through the durable list; if that cannot be written the upload step tries again)
            if self.announce(&final_path).is_ok() {
                let mut e = entry;
                e.announced = true;
                let _ = queue.save(&e);
            }
            self.logln(format!("imported {} into {dest}", placed.name));
            p.done += 1;
        }
        progress(&p);
        Ok(run)
    }
}

// ------------------------------------------------------------------ the upload

/// How much one [`Other::upload_step`] may do before it hands control back.
#[derive(Debug, Clone, Copy)]
pub struct UploadBudget {
    pub max_files: usize,
    pub max_bytes: u64,
    pub max_time: Duration,
}

impl Default for UploadBudget {
    fn default() -> Self {
        UploadBudget { max_files: 25, max_bytes: 256 << 20, max_time: Duration::from_secs(20) }
    }
}

/// What one upload step did.
#[derive(Debug, Clone, Default)]
pub struct UploadReport {
    pub files_sent: usize,
    pub bytes_sent: u64,
    /// Photos whose commit note was written in this step.
    pub committed: usize,
    /// Results seen in this step: (id, status).
    pub results: Vec<(String, String)>,
    /// Lines for Problems (a rejection, a photo that vanished before it was sent).
    pub problems: Vec<String>,
    /// The budget ran out with photos left: call again soon.
    pub more: bool,
    /// The queue after the step.
    pub queue: QueueStatus,
}

impl Other {
    /// Do a bounded amount of the upload queue's work: tell LightCraft about photos it has not heard of, look for results of imports
    /// already sent, send photos, tidy up. A network failure is an `Err(Error::Remote)` (what was sent stays recorded); everything
    /// else is a line in `problems`. Takes the run lock, so it never overlaps a pass.
    pub fn upload_step(&self, budget: &UploadBudget) -> Result<UploadReport> {
        let _lock = RunLock::acquire(&self.cfg.state_dir, false)?;
        let queue = self.out_queue();
        let mut rep = UploadReport::default();
        let st = State::open(&self.cfg.state_dir)?;
        let mut entries = queue.list();
        // tidy: what needs no network
        entries.retain(|e| {
            let gone = self.target(&e.path()).is_none_or(|p| !p.is_file());
            let tracked = e.result.as_ref().is_some_and(|r| !r.is_rejected()) && st.photos.contains_key(&e.id);
            let done_unreported = e.result.as_ref().is_some_and(|r| r.is_rejected()) && e.reported;
            let finished_and_deleted = e.result.is_some() && gone;
            if tracked || done_unreported || finished_and_deleted {
                queue.remove(&e.id);
                return false;
            }
            true
        });
        for e in entries.iter_mut().filter(|e| !e.announced) {
            if let Some(p) = self.target(&e.path())
                && self.announce(&p).is_ok()
            {
                e.announced = true;
                let _ = queue.save(e);
            }
        }
        if entries.iter().any(|e| e.uploaded.is_some() && e.result.is_none()) {
            self.collect_results(&queue, &mut entries, &mut rep)?;
        }
        let started = Instant::now();
        let remote_files = if entries.iter().any(OutEntry::waiting_to_upload) { Some(self.list_outgoing()?) } else { None };
        for (taken, e) in entries.iter_mut().filter(|e| e.waiting_to_upload()).enumerate() {
            if taken >= budget.max_files || rep.bytes_sent >= budget.max_bytes || started.elapsed() >= budget.max_time {
                rep.more = true;
                break;
            }
            match self.upload_one(&queue, e, remote_files.as_ref().unwrap_or(&HashMap::new()), &mut rep) {
                Ok(()) => {}
                Err(err @ Error::Remote(_)) => return Err(err),
                Err(err) => push_unique(&mut rep.problems, format!("{}: {err}", e.name)),
            }
        }
        rep.queue = queue.status();
        Ok(rep)
    }

    /// `imports/<device>/` as the remote has it: `<id>/<name>` and `<id>.json` to size.
    fn list_outgoing(&self) -> Result<HashMap<String, u64>> {
        match self.retry(|| self.remote.list(&format!("imports/{}", self.cfg.device), true)) {
            Ok(v) => Ok(v.into_iter().filter(|e| !e.is_dir).map(|e| (e.path, e.size)).collect()),
            Err(Error::NotFound(_)) => Ok(HashMap::new()),
            Err(e) => Err(e),
        }
    }

    fn upload_one(&self, queue: &ImportQueue, e: &mut OutEntry, remote: &HashMap<String, u64>, rep: &mut UploadReport) -> Result<()> {
        let Some(photo) = self.target(&e.path()).filter(|p| p.is_file()) else {
            push_unique(&mut rep.problems, format!("{} is not on this PC any more, so it was not sent to the home PC.", e.name));
            queue.remove(&e.id);
            return Ok(());
        };
        let len = std::fs::metadata(&photo).map_or(0, |m| m.len());
        if len != e.size && !content_hash_file(&photo).is_ok_and(|(h, n)| h == e.id && n == e.size) {
            push_unique(&mut rep.problems, format!("{} was changed on this PC after it was imported, so it was not sent to the home PC.", e.name));
            queue.remove(&e.id);
            return Ok(());
        }
        let dev = &self.cfg.device;
        let photo_remote = format!("imports/{dev}/{}/{}", e.id, e.name);
        if remote.get(&format!("{}/{}", e.id, e.name)) != Some(&e.size) {
            self.retry(|| self.remote.put(&photo, &photo_remote))?;
            rep.files_sent += 1;
            rep.bytes_sent += e.size;
        }
        if let Some(sn) = &e.sidecar {
            let Some(bytes) = queue.snapshot(&e.id) else {
                // (the snapshot is gone: send the photo without a sidecar rather than never)
                e.sidecar = None;
                e.sidecar_sha = None;
                return queue.save(e);
            };
            if remote.get(&format!("{}/{sn}", e.id)) != Some(&(bytes.len() as u64)) {
                let tmp = self.cfg.state_dir.join("outbox").join(format!("{}.xmp", e.id));
                write_atomic(&tmp, &bytes)?;
                let sent = self.retry(|| self.remote.put(&tmp, &format!("imports/{dev}/{}/{sn}", e.id)));
                let _ = std::fs::remove_file(&tmp);
                sent?;
                rep.files_sent += 1;
                rep.bytes_sent += bytes.len() as u64;
            }
        }
        // the note, last: it is what tells the home PC the files are all there
        let note = serde_json::to_vec(&e.note()).map_err(|x| Error::Io(format!("note: {x}")))?;
        if remote.get(&format!("{}.json", e.id)) != Some(&(note.len() as u64)) {
            let tmp = self.cfg.state_dir.join("outbox").join(format!("{}.note.json", e.id));
            write_atomic(&tmp, &note)?;
            let sent = self.retry(|| self.remote.put(&tmp, &format!("imports/{dev}/{}.json", e.id)));
            let _ = std::fs::remove_file(&tmp);
            sent?;
        }
        e.uploaded = Some(iso(self.now()));
        queue.save(e)?;
        rep.committed += 1;
        self.logln(format!("sent {} to the home PC for adding", e.name));
        Ok(())
    }

    /// Look for `imports-done/<day>/<device>/<id>.result.json` of the photos sent and not answered yet.
    fn collect_results(&self, queue: &ImportQueue, entries: &mut [OutEntry], rep: &mut UploadReport) -> Result<()> {
        let oldest = entries.iter().filter(|e| e.uploaded.is_some() && e.result.is_none()).filter_map(|e| e.uploaded.as_deref()).min().and_then(parse_iso);
        // (a day before the oldest upload, for a clock that is a little off)
        let from = oldest.map(|t| iso(t - Duration::from_secs(86_400))[..10].to_string()).unwrap_or_default();
        let days = match self.retry(|| self.remote.list("imports-done", false)) {
            Ok(v) => v,
            Err(Error::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut days: Vec<String> = days.into_iter().filter(|d| d.is_dir && d.path.as_str() >= from.as_str()).map(|d| d.path).collect();
        days.sort();
        let dev = &self.cfg.device;
        for day in days {
            let files = match self.retry(|| self.remote.list(&format!("imports-done/{day}/{dev}"), false)) {
                Ok(v) => v,
                Err(Error::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            for f in files.into_iter().filter(|f| !f.is_dir) {
                let Some(id) = f.path.strip_suffix(".result.json") else { continue };
                let Some(e) = entries.iter_mut().find(|e| e.id == id && e.uploaded.is_some() && e.result.is_none()) else { continue };
                let tmp = self.cfg.state_dir.join("outbox").join(format!("{id}.result.json"));
                if let Some(dir) = tmp.parent() {
                    std::fs::create_dir_all(dir).map_err(|x| Error::io(dir.display(), x))?;
                }
                let got = self.get(&format!("imports-done/{day}/{dev}/{}", f.path), &tmp);
                let bytes = got.and_then(|()| std::fs::read(&tmp).map_err(|x| Error::io(tmp.display(), x)));
                let _ = std::fs::remove_file(&tmp);
                let v: serde_json::Value = match bytes.and_then(|b| serde_json::from_slice(&b).map_err(|x| Error::Format(x.to_string()))) {
                    Ok(v) => v,
                    Err(err @ Error::Remote(_)) => return Err(err),
                    Err(_) => continue, // (not readable yet or ever: the entry keeps waiting)
                };
                let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string();
                if !matches!(status.as_str(), "added" | "duplicate" | "rejected") {
                    continue;
                }
                let path = v.get("path").and_then(|s| s.as_str()).map(str::to_string);
                let reason = v.get("reason").and_then(|s| s.as_str()).map(str::to_string);
                e.result = Some(OutResult { status: status.clone(), path, reason: reason.clone() });
                if status == "rejected" {
                    e.reported = true;
                    push_unique(
                        &mut rep.problems,
                        format!("{} was not added at the home PC: {}. It stays in your photo folder.", e.name, reason.as_deref().unwrap_or("no reason given")),
                    );
                }
                rep.results.push((id.to_string(), status));
                queue.save(e)?;
            }
        }
        Ok(())
    }
}
