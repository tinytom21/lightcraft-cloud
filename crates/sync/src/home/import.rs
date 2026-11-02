//! Imports: photos other PCs uploaded (FORMAT.md, "Imports"), adopted into the photo folder by the home sync.
//!
//! This writes into the user's master photo folder, so every step is defensive:
//!
//! * a note (`imports/<device>/<id>.json`, written last by the uploader) is validated strictly before anything is downloaded;
//!   a path is never used unless every component is plain and the folder, resolved through any symlinks, is inside the photo
//!   folder;
//! * the bytes are downloaded to a temporary file in the state folder and must have the size and content hash the note names;
//! * nothing is ever overwritten: a name that is taken by other bytes gets `-2`, `-3`, ... and a file and sidecar are
//!   created through a temporary file in the same folder, `fsync`, and a hard link that fails if the name exists (a rename
//!   would replace), the sidecar first so a scan never sees a photo without it;
//! * the import is filed away (result written, files and note moved to `imports-done/`) only after the photo is on disk, so a
//!   crash anywhere is repaired by running again: identical bytes at the target are a duplicate, not a second copy;
//! * one import that cannot be adopted never stops the others and never fails the sync. Only a problem with the note or the
//!   bytes rejects; a network or disk problem leaves the import waiting.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use super::hash::content_hash_file;
use super::scan::{is_excluded, is_photo, is_skipped_name, stem};
use super::{Home, State, validate_xmp};
use crate::progress::Stage;
use crate::util::{free_space, iso, link_no_replace, parse_iso, sync_dir, temp_beside};
use crate::{Error, Result, valid_device};

/// An import whose files are still missing this long after its note was first seen is rejected: the uploader gave up or died,
/// and a note that can never be completed should not be asked about for ever. Hours.
pub const IMPORT_INCOMPLETE_LIMIT_HOURS: u64 = 48;

/// Free space left over after an imported photo is written, by default (256 MiB).
pub const DEFAULT_IMPORT_FREE_MARGIN: u64 = 256 << 20;

/// A note is a few hundred bytes.
const MAX_NOTE_BYTES: u64 = 64 * 1024;
/// The XMP validator's own limit.
const MAX_SIDECAR_BYTES: u64 = 2_000_000;
/// How many `-N` names are tried before giving up (the import then waits).
const MAX_VARIANTS: u32 = 10_000;

/// A validated commit note.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportNote {
    /// The LightCraft content hash of the file's bytes (32 lower-case hex).
    pub id: String,
    /// Relative to the photo folder, `/`-separated; empty for the folder itself.
    pub folder: String,
    pub name: String,
    pub size: u64,
    pub mtime: SystemTime,
    pub sidecar: Option<String>,
}

/// A photo that was adopted.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedPhoto {
    pub device: String,
    pub id: String,
    /// Relative to the photo folder (and to `originals/`), POSIX separators: the final path, after any renaming.
    pub path: String,
    /// The sidecar this adoption wrote (relative), if it did.
    pub sidecar: Option<String>,
}

/// What one pass over the imports did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportSummary {
    /// The photos adopted, in order. Part B of the feature tells LightCraft which folders to look at from this.
    pub added: Vec<ImportedPhoto>,
    pub duplicates: usize,
    pub rejected: usize,
    /// Committed imports left on the remote for a later sync.
    pub waiting: usize,
    /// One line per import.
    pub items: Vec<String>,
    /// Folders (relative to the photo folder, `.` for the folder itself) that got a photo.
    pub folders: Vec<String>,
    /// Files written (relative): complete, so the same sync publishes them even though they are new.
    pub written: Vec<String>,
    /// Problems the user can act on (disk full, a file in the way, the remote unreachable).
    pub errors: Vec<String>,
}

// ------------------------------------------------------------------ the note

fn is_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Windows device names (with or without an extension) cannot be files there.
fn is_reserved(component: &str) -> bool {
    let base = component.split('.').next().unwrap_or("").trim_end_matches(' ').to_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$")
        || ((base.starts_with("COM") || base.starts_with("LPT"))
            && base.chars().count() == 4
            && base.chars().nth(3).is_some_and(|c| matches!(c, '0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}')))
}

/// Why `c` cannot be one component of an imported path, or None.
fn bad_component(c: &str) -> Option<String> {
    if c.is_empty() {
        return Some("it has an empty component".into());
    }
    if c == "." || c == ".." {
        return Some(format!("it has the component {c:?}"));
    }
    if c.len() > 255 {
        return Some("a component is longer than 255 bytes".into());
    }
    if let Some(ch) = c.chars().find(|ch| ch.is_control()) {
        return Some(format!("it has the control character U+{:04X}", ch as u32));
    }
    if let Some(ch) = c.chars().find(|ch| matches!(ch, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*')) {
        return Some(format!("it has the character {ch:?}, which is not allowed in a file name"));
    }
    if c.ends_with('.') || c.ends_with(' ') {
        return Some(format!("{c:?} ends with a dot or a space"));
    }
    if is_reserved(c) {
        return Some(format!("{c:?} is a reserved Windows name"));
    }
    if is_skipped_name(c) {
        return Some(format!("{c:?} is hidden: hidden files and folders are never published"));
    }
    None
}

fn bad_folder(folder: &str) -> Option<String> {
    if folder.is_empty() {
        return None; // (the photo folder itself)
    }
    if folder.len() > 1024 {
        return Some("it is longer than 1024 bytes".into());
    }
    folder.split('/').find_map(bad_component)
}

/// Whether a photo called `name` may be imported into `folder` (relative to the photo folder, `/`-separated, `""` for the folder
/// itself): the one rule home applies to a note, and the one another PC applies before it copies or uploads anything. Err: why not.
pub fn check_import_target(folder: &str, name: &str, exclude: &[String]) -> std::result::Result<(), String> {
    if let Some(why) = bad_folder(folder) {
        return Err(format!("bad folder {folder:?}: {why}"));
    }
    if let Some(why) = bad_component(name) {
        return Err(format!("bad name {name:?}: {why}"));
    }
    if !is_photo(name) {
        return Err(format!("{name:?} is not a photo file type"));
    }
    if !folder.is_empty() && is_excluded(&format!("{folder}/{name}"), exclude) {
        return Err(format!("the folder {folder:?} matches an exclude pattern, so it is not published here"));
    }
    Ok(())
}

/// Check a commit note. `file_id`: the id in the note's file name (`<id>.json`), which the note must repeat. Err: why the import
/// is rejected. `exclude`: the configured exclude patterns; a folder that matches one is never published, so never adopted.
pub fn check_import_note(data: &[u8], file_id: &str, exclude: &[String]) -> std::result::Result<ImportNote, String> {
    if data.len() as u64 > MAX_NOTE_BYTES {
        return Err("the note is too big".into());
    }
    let v: Value = serde_json::from_slice(data).map_err(|e| format!("the note is not valid JSON: {e}"))?;
    let Some(o) = v.as_object() else { return Err("the note is not a JSON object".into()) };
    match o.get("format").and_then(Value::as_u64) {
        Some(1) => {}
        other => {
            return Err(format!(
                "the note's format is {} (this version reads format 1)",
                other.map_or("missing or not a number".to_string(), |n| n.to_string())
            ));
        }
    }
    if !is_id(file_id) {
        return Err(format!("the note's file name {file_id:?} is not a 32 hex digit id"));
    }
    let id = o.get("id").and_then(Value::as_str).unwrap_or("");
    if !is_id(id) {
        return Err(format!("the note's id {id:?} is not 32 lower-case hex digits"));
    }
    if id != file_id {
        return Err(format!("the note's id {id} is not the id in its file name {file_id}"));
    }
    let text = |k: &str| -> std::result::Result<String, String> {
        o.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| format!("the note's {k} is missing or not text"))
    };
    let (folder, name) = (text("folder")?, text("name")?);
    check_import_target(&folder, &name, exclude)?;
    let size = o.get("size").and_then(Value::as_u64).ok_or("the note's size is missing or not a whole number")?;
    let mtime = o
        .get("mtime")
        .and_then(Value::as_str)
        .and_then(parse_iso)
        .ok_or("the note's mtime is missing or not a UTC time like 2024-05-01T12:00:00Z")?;
    let sidecar = match o.get("sidecar") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            if let Some(why) = bad_component(s) {
                return Err(format!("bad sidecar name {s:?}: {why}"));
            }
            // the engine finds a sidecar as `<stem>.xmp` or `<file name>.xmp`: anything else would be an unrelated file
            if *s != format!("{}.xmp", stem(&name)) && *s != format!("{name}.xmp") {
                return Err(format!("the sidecar {s:?} is not named for the photo {name:?} (<stem>.xmp or <name>.xmp)"));
            }
            Some(s.clone())
        }
        Some(_) => return Err("the note's sidecar is not text or null".into()),
    };
    Ok(ImportNote { id: id.to_string(), folder, name, size, mtime, sidecar })
}

// ------------------------------------------------------------------ the files

/// Why an import did not go through.
enum Fail {
    /// The note or the bytes are wrong: reject.
    Reject(String),
    /// The remote or the disk is the problem: leave it for the next sync; the message is a problem to show.
    Wait(String),
    /// Files the note names are not there (yet): leave it, quietly, for up to [`IMPORT_INCOMPLETE_LIMIT_HOURS`].
    Incomplete(String),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        Fail::Wait(e.to_string())
    }
}

enum Outcome {
    Added { path: String, sidecar: Option<String>, wrote_sidecar: bool, reason: Option<String>, folder: String },
    /// `on_disk`: found by its bytes in the photo folder (not by the published state): a crashed adoption's photo looks like this.
    Duplicate { path: String, on_disk: bool },
    /// A dry run: this is where it would go.
    Would { path: String },
}

struct Listing {
    /// (device, id from the file name) -> size of every note.
    notes: BTreeMap<(String, String), u64>,
    /// (device, id) -> (file name, size) of what is in `imports/<device>/<id>/`.
    data: HashMap<(String, String), Vec<(String, u64)>>,
}

struct Run<'a> {
    root: &'a Path,
    tmp: PathBuf,
    date: String,
    /// id -> path (relative) of the photos the library has, and the ones adopted during this pass.
    known: HashMap<String, String>,
}

/// Create `dir/name` whole or not at all: a temporary file in `dir`, `fsync`, then a link that never replaces.
/// `AlreadyExists` when the name is taken.
fn create_new_file(dir: &Path, name: &str, fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>) -> std::io::Result<()> {
    let dest = dir.join(name);
    let tmp = temp_beside(&dest);
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        fill(&mut f)?;
        f.sync_all()
    })();
    let linked = written.and_then(|()| link_no_replace(&tmp, &dest));
    let _ = std::fs::remove_file(&tmp);
    if linked.is_ok() {
        sync_dir(dir);
    }
    linked
}

fn ext_of(name: &str) -> &str {
    &name[stem(name).len()..]
}

/// The file name of the `n`th try (1: the name in the note), and its sidecar's, named the way the note's was.
fn variant(note: &ImportNote, n: u32) -> (String, Option<String>) {
    if n == 1 {
        return (note.name.clone(), note.sidecar.clone());
    }
    let (s, e) = (stem(&note.name), ext_of(&note.name));
    let name = format!("{s}-{n}{e}");
    let sidecar = note.sidecar.as_ref().map(|sc| if *sc == format!("{s}.xmp") { format!("{s}-{n}.xmp") } else { format!("{name}.xmp") });
    (name, sidecar)
}

/// Whether `entry` is the name in the note or one of its numbered variants (case-insensitively: a case-insensitive filesystem
/// would treat them as the same file).
fn is_variant_of(note: &ImportNote, entry: &str) -> bool {
    let (e, name) = (entry.to_lowercase(), note.name.to_lowercase());
    if e == name {
        return true;
    }
    let (s, x) = (stem(&name).to_string(), ext_of(&name).to_string());
    e.strip_prefix(&format!("{s}-")).and_then(|r| r.strip_suffix(x.as_str())).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The entry of `dir` with this name, ignoring case, as it is spelled on disk.
fn entry_named(dir: &Path, name: &str) -> std::io::Result<Option<String>> {
    let want = name.to_lowercase();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    for e in rd {
        let n = e?.file_name().to_string_lossy().into_owned();
        if n.to_lowercase() == want {
            return Ok(Some(n));
        }
    }
    Ok(None)
}

/// A regular file (not a link) of this size and content hash.
fn has_bytes(path: &Path, size: u64, id: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() == size) && content_hash_file(path).is_ok_and(|(h, _)| h == id)
}

/// Leftovers of an adoption that crashed: `.<photo name>.part-<pid>-<n>` beside the photo (hidden, never published; after a crash
/// between writing and linking, a whole copy of a photo). Only an adoption writes such names for photos, and only under the run
/// lock, so any that is there is stale.
fn clean_stale_temps(dir: &Path, note: &ImportNote) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some((inner, tail)) = name.strip_prefix('.').and_then(|r| r.rsplit_once(".part-")) else { continue };
        let numbered = tail.split_once('-').is_some_and(|(a, b)| [a, b].iter().all(|x| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit())));
        if numbered && is_variant_of(note, inner) && e.file_type().is_ok_and(|t| t.is_file()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

pub(crate) fn rel_join(folder: &str, name: &str) -> String {
    if folder.is_empty() { name.to_string() } else { format!("{folder}/{name}") }
}

/// The folder `folder` (relative to `root`, the canonical photo folder) as it really is on disk: every component that exists is
/// resolved through symlinks and must be inside `root`; the rest does not exist yet (`create`: it is made now). Returns the real
/// path and its path relative to `root`.
fn real_folder(root: &Path, folder: &str, create: bool) -> std::result::Result<(PathBuf, String), Fail> {
    let mut cur = root.to_path_buf();
    for comp in folder.split('/').filter(|c| !c.is_empty()) {
        let next = cur.join(comp);
        let mut meta = std::fs::symlink_metadata(&next);
        if create && matches!(&meta, Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
            match std::fs::create_dir(&next) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(Fail::Wait(format!("cannot create the folder {}: {e}", next.display()))),
            }
            meta = std::fs::symlink_metadata(&next);
        }
        match meta {
            Ok(_) => {
                let real = std::fs::canonicalize(&next).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Fail::Reject(format!("{} is a link to nowhere", next.display()))
                    } else {
                        Fail::Wait(format!("cannot resolve {}: {e}", next.display()))
                    }
                })?;
                if !real.starts_with(root) {
                    return Err(Fail::Reject(format!("the folder {folder:?} resolves outside the photo folder (through a link)")));
                }
                if !real.is_dir() {
                    return Err(Fail::Wait(format!("{} exists and is not a folder", real.display())));
                }
                cur = real;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => cur = next,
            Err(e) => return Err(Fail::Wait(format!("cannot look at {}: {e}", next.display()))),
        }
    }
    let rel = cur.strip_prefix(root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    Ok((cur, rel))
}

impl Home {
    fn crash(&self, point: &str) {
        if self.import_crash.as_deref() == Some(point) {
            panic!("injected crash at {point}");
        }
    }

    /// Test seam: panic at this point of an adoption (`before_write`, `after_sidecar`, `after_temp`, `after_link`, `after_photo`,
    /// `after_result`, `after_first_move`, `after_data_moved`), as a crash would stop it.
    #[doc(hidden)]
    pub fn set_import_crash_point(&mut self, point: Option<&str>) {
        self.import_crash = point.map(str::to_string);
    }

    fn list_imports(&self) -> Result<Listing> {
        let entries = match self.remote.list("imports", true) {
            Ok(v) => v,
            Err(Error::NotFound(_)) => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut l = Listing { notes: BTreeMap::new(), data: HashMap::new() };
        for e in entries.into_iter().filter(|e| !e.is_dir) {
            let parts: Vec<&str> = e.path.split('/').collect();
            match parts[..] {
                [device, file] if valid_device(device) => {
                    if let Some(id) = file.strip_suffix(".json").filter(|_| !file.ends_with(".result.json")) {
                        l.notes.insert((device.to_string(), id.to_string()), e.size);
                    }
                }
                [device, id, file] if valid_device(device) => {
                    l.data.entry((device.to_string(), id.to_string())).or_default().push((file.to_string(), e.size));
                }
                _ => {}
            }
        }
        Ok(l)
    }

    /// How many committed imports (a note in the right place) wait on the remote: one listing, nothing is read, no lock, no
    /// state, no access to the photo folder. Files with no note are an upload still in progress and do not count.
    pub fn imports_waiting(&self) -> Result<usize> {
        Ok(self.list_imports()?.notes.len())
    }

    fn fetch(&self, remote_path: &str, local: &Path) -> Result<()> {
        self.retry(|| self.remote.get(remote_path, local))
    }

    /// Adopt the photos other PCs uploaded. `dry`: validate and report, change nothing. The caller holds the run lock (`sync` does).
    pub fn process_imports(&self, st: &mut State, dry: bool) -> Result<ImportSummary> {
        let mut sum = ImportSummary::default();
        let listing = match self.list_imports() {
            Ok(l) => l,
            Err(e) => {
                // (the steps that follow report a remote that is really down; this must not fail a sync that worked before)
                self.logln(format!("imports: cannot list the imports ({e}); trying again next sync"));
                sum.errors.push(format!("cannot list the imports: {e}"));
                return Ok(sum);
            }
        };
        if !dry {
            // (the waiting clock of a note that is gone)
            for k in st.keys_with_prefix("import-wait:") {
                if !listing.notes.keys().any(|(d, i)| format!("import-wait:{d}/{i}") == k) {
                    st.unset(&k);
                }
            }
        }
        if listing.notes.is_empty() {
            return Ok(sum);
        }
        let root = match std::fs::canonicalize(&self.cfg.source) {
            Ok(r) => r,
            Err(e) => {
                sum.errors.push(format!("cannot resolve the photo folder: {e}"));
                return Ok(sum);
            }
        };
        let tmp = self.cfg.state_dir.join("imports-tmp");
        if !dry {
            let _ = std::fs::remove_dir_all(&tmp); // (a crashed run's downloads; we hold the run lock)
            std::fs::create_dir_all(&tmp).map_err(|e| Error::io(tmp.display(), e))?;
        }
        let mut known: HashMap<String, String> = HashMap::new();
        for (p, r) in st.files() {
            if r.status == "ok"
                && let Some(id) = r.id.as_deref().filter(|i| !i.is_empty())
            {
                known.entry(id.to_string()).or_insert_with(|| p.clone());
            }
        }
        let mut run = Run { root: &root, tmp, date: self.today(), known };
        self.rp.stage(Stage::Importing, listing.notes.len() as u64, 0);
        for (n, ((device, fid), note_size)) in listing.notes.iter().enumerate() {
            self.rp.at(n as u64);
            let label = format!("{device}/{fid}");
            let files = listing.data.get(&(device.clone(), fid.clone())).cloned().unwrap_or_default();
            let key = format!("import-wait:{label}");
            let step = self.import_one(&run, st, dry, device, fid, *note_size, &files, &key).and_then(|o| {
                // tell the caller before anything is filed away (a crash after this repeats it, never loses it)
                let on_disk = match &o {
                    Outcome::Added { path, .. } | Outcome::Duplicate { path, on_disk: true } => Some(path),
                    _ => None,
                };
                match (&self.adopted_hook, on_disk) {
                    (Some(hook), Some(path)) if !dry => hook(&crate::util::plain_path(&run.root.join(path))).map_err(|e| Fail::Wait(format!("cannot record {path} for the caller: {e}")))?,
                    _ => {}
                }
                Ok(o)
            });
            let mut result: Option<Value> = None;
            match step {
                Ok(Outcome::Added { path, sidecar, wrote_sidecar, reason, folder }) => {
                    self.logln(format!("imports: add {label} -> {path}"));
                    sum.items.push(format!("add {label}: {path}"));
                    if !sum.folders.contains(&folder) {
                        sum.folders.push(folder);
                    }
                    sum.written.push(path.clone());
                    if wrote_sidecar && let Some(sc) = &sidecar {
                        sum.written.push(sc.clone());
                    }
                    run.known.insert(fid.clone(), path.clone());
                    result = Some(json!({"id": fid, "status": "added", "path": path, "reason": reason}));
                    sum.added.push(ImportedPhoto { device: device.clone(), id: fid.clone(), path, sidecar: if wrote_sidecar { sidecar } else { None } });
                }
                Ok(Outcome::Duplicate { path, .. }) => {
                    self.logln(format!("imports: duplicate {label} (already {path})"));
                    sum.items.push(format!("duplicate {label}: {path}"));
                    sum.duplicates += 1;
                    run.known.entry(fid.clone()).or_insert_with(|| path.clone());
                    result = Some(json!({"id": fid, "status": "duplicate", "path": path, "reason": null}));
                }
                Ok(Outcome::Would { path }) => {
                    self.logln(format!("imports: would add {label} -> {path}"));
                    sum.items.push(format!("would add {label}: {path}"));
                }
                Err(Fail::Reject(why)) => {
                    self.logln(format!("imports: REJECT {label}: {why}"));
                    sum.items.push(format!("reject {label}: {why}"));
                    sum.rejected += 1;
                    result = Some(json!({"id": fid, "status": "rejected", "path": null, "reason": why}));
                }
                Err(Fail::Incomplete(why)) => {
                    self.logln(format!("imports: {label} waiting for its files: {why}"));
                    sum.items.push(format!("waiting {label}: {why}"));
                    sum.waiting += 1;
                }
                Err(Fail::Wait(why)) => {
                    self.logln(format!("imports: {label} waiting: {why}"));
                    sum.items.push(format!("waiting {label}: {why}"));
                    sum.errors.push(format!("{label}: {why}"));
                    sum.waiting += 1;
                }
            }
            let Some(result) = result.filter(|_| !dry) else { continue };
            // only now, with the photo safely on disk (or the import refused), is the import filed away
            match self.file_away(&run.date, device, fid, &files, &result) {
                Ok(()) => st.unset(&key),
                Err(e) => {
                    self.logln(format!("imports: {label} is done but could not be filed away yet: {e}"));
                    sum.items.push(format!("waiting {label}: could not be filed away: {e}"));
                    sum.errors.push(format!("{label}: could not be filed away: {e}"));
                    sum.waiting += 1;
                }
            }
        }
        self.rp.finish();
        if !dry {
            let _ = std::fs::remove_dir_all(&run.tmp);
        }
        Ok(sum)
    }

    /// Everything that can go wrong with one import short of a crash.
    fn import_one(
        &self,
        run: &Run,
        st: &mut State,
        dry: bool,
        device: &str,
        fid: &str,
        note_size: u64,
        files: &[(String, u64)],
        wait_key: &str,
    ) -> std::result::Result<Outcome, Fail> {
        if note_size > MAX_NOTE_BYTES {
            return Err(Fail::Reject("the note is too big".into()));
        }
        let data = self.get_bytes(&format!("imports/{device}/{fid}.json"))?;
        let note = check_import_note(&data, fid, &self.cfg.exclude).map_err(Fail::Reject)?;
        // where it would go, and that it stays inside the photo folder
        let (_, rel_folder) = real_folder(run.root, &note.folder, false)?;
        self.check_real_folder(&note, &rel_folder)?;
        if !dry {
            clean_stale_temps(&run.root.join(&rel_folder), &note);
        }
        // a photo the library already has (or a file with the same bytes at the target) is a duplicate: no download, no write
        if let Some(p) = run.known.get(&note.id) {
            return Ok(Outcome::Duplicate { path: p.clone(), on_disk: false });
        }
        if let Some(p) = self.find_same(run.root, &rel_folder, &note)? {
            return Ok(Outcome::Duplicate { path: p, on_disk: true });
        }
        if dry {
            return Ok(Outcome::Would { path: rel_join(&rel_folder, &note.name) });
        }
        // the files the note names must be there
        let have = |n: &str| files.iter().any(|(f, _)| f == n);
        let missing: Vec<&str> = std::iter::once(note.name.as_str()).chain(note.sidecar.as_deref()).filter(|n| !have(n)).collect();
        if !missing.is_empty() {
            let Some(since) = st.get(wait_key).and_then(parse_iso) else {
                st.set(wait_key, &iso(self.now()));
                return Err(Fail::Incomplete(format!("{} not uploaded yet", missing.join(", "))));
            };
            if self.now().duration_since(since).unwrap_or(Duration::ZERO) >= Duration::from_secs(IMPORT_INCOMPLETE_LIMIT_HOURS * 3600) {
                return Err(Fail::Reject(format!(
                    "{} still not uploaded {IMPORT_INCOMPLETE_LIMIT_HOURS} hours after the note appeared",
                    missing.join(", ")
                )));
            }
            return Err(Fail::Incomplete(format!("{} not uploaded yet (since {})", missing.join(", "), iso(since))));
        }
        // download (the sidecar first: it is small and may reject), then verify
        let side = match &note.sidecar {
            Some(sn) => {
                let local = run.tmp.join(format!("{device}-{fid}.sidecar"));
                self.fetch(&format!("imports/{device}/{fid}/{sn}"), &local)?;
                let len = std::fs::metadata(&local).map_err(|e| Error::io(local.display(), e))?.len();
                if len > MAX_SIDECAR_BYTES {
                    let _ = std::fs::remove_file(&local);
                    return Err(Fail::Reject(format!("the sidecar {sn} is too big ({len} bytes)")));
                }
                let bytes = std::fs::read(&local).map_err(|e| Error::io(local.display(), e))?;
                let _ = std::fs::remove_file(&local);
                if let Some(why) = validate_xmp(&bytes) {
                    return Err(Fail::Reject(format!("the sidecar {sn} is not acceptable: {why}")));
                }
                Some(bytes)
            }
            None => None,
        };
        let local = run.tmp.join(format!("{device}-{fid}.photo"));
        let result = (|| {
            self.fetch(&format!("imports/{device}/{fid}/{}", note.name), &local)?;
            let (hash, size) = content_hash_file(&local)?;
            if size != note.size || hash != note.id {
                return Err(Fail::Reject(format!(
                    "the downloaded {} does not match its note (note: {} bytes, id {}; got {size} bytes, hash {hash})",
                    note.name, note.size, note.id
                )));
            }
            self.place(run, &note, &local, side.as_deref())
        })();
        let _ = std::fs::remove_file(&local);
        result
    }

    /// The folder as it is on disk (after links) must be one that is published.
    fn check_real_folder(&self, note: &ImportNote, rel_folder: &str) -> std::result::Result<(), Fail> {
        if !rel_folder.is_empty() && (rel_folder.split('/').any(is_skipped_name) || is_excluded(&format!("{rel_folder}/{}", note.name), &self.cfg.exclude)) {
            return Err(Fail::Reject(format!("the folder {:?} is really {rel_folder:?}, which is hidden or excluded, so it is not published", note.folder)));
        }
        Ok(())
    }

    /// A file in the target folder that has the note's bytes, under the note's name or one of its numbered variants.
    fn find_same(&self, root: &Path, rel_folder: &str, note: &ImportNote) -> std::result::Result<Option<String>, Fail> {
        let dir = if rel_folder.is_empty() { root.to_path_buf() } else { root.join(rel_folder) };
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Fail::Wait(format!("cannot read {}: {e}", dir.display()))),
        };
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| is_variant_of(note, n)).collect();
        names.sort();
        Ok(names.into_iter().find(|n| has_bytes(&dir.join(n), note.size, &note.id)).map(|n| rel_join(rel_folder, &n)))
    }

    /// Write the verified bytes (and the sidecar) into the photo folder; never replacing anything.
    fn place(&self, run: &Run, note: &ImportNote, photo: &Path, sidecar: Option<&[u8]>) -> std::result::Result<Outcome, Fail> {
        let (plan_abs, _) = real_folder(run.root, &note.folder, false)?;
        let need = note.size.saturating_add(sidecar.map_or(0, |s| s.len() as u64)).saturating_add(self.cfg.import_free_margin);
        if let Some(free) = free_space(&plan_abs)
            && free < need
        {
            return Err(Fail::Wait(format!(
                "not enough free disk space to add {}: {free} bytes free, {need} needed (the file, its sidecar and a margin of {} bytes); the import waits",
                note.name, self.cfg.import_free_margin
            )));
        }
        let (dir, rel_folder) = real_folder(run.root, &note.folder, true)?;
        self.check_real_folder(note, &rel_folder)?;
        let mtime = note.mtime.min(self.now());
        self.crash("before_write");
        let (mut n, mut races) = (1u32, 0u32);
        while n <= MAX_VARIANTS && races < 20 {
            let (pname, sname) = variant(note, n);
            let io = |what: &str, e: std::io::Error| Fail::Wait(format!("{what} in {}: {e}", dir.display()));
            // the photo's name: taken by the same bytes means it is already here; by other bytes, try the next number
            if let Some(real) = entry_named(&dir, &pname).map_err(|e| io("cannot list", e))? {
                if has_bytes(&dir.join(&real), note.size, &note.id) {
                    return Ok(Outcome::Duplicate { path: rel_join(&rel_folder, &real), on_disk: true });
                }
                n += 1;
                continue;
            }
            // the sidecar goes in before the photo, so a scan never sees the photo without it; one that is there stays
            let (mut wrote_sidecar, mut reason) = (false, None);
            if let (Some(sn), Some(bytes)) = (&sname, sidecar) {
                let taken_by = match entry_named(&dir, sn).map_err(|e| io("cannot list", e))? {
                    Some(real) => Some(real),
                    None => match create_new_file(&dir, sn, |f| f.write_all(bytes)) {
                        Ok(()) => {
                            wrote_sidecar = true;
                            None
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            races += 1;
                            continue; // (somebody made it just now: look again)
                        }
                        Err(e) => return Err(io("cannot write the sidecar", e)),
                    },
                };
                if let Some(real) = taken_by {
                    // (a sidecar of a crashed earlier pass has the same bytes: it is ours)
                    let same = std::fs::read(dir.join(&real)).is_ok_and(|b| b == bytes);
                    if !same && n > 1 {
                        n += 1; // (a numbered name must be free, sidecar included)
                        continue;
                    }
                    if !same {
                        reason = Some(format!("the sidecar {real} was already there with other contents and was kept"));
                    }
                }
            }
            self.crash("after_sidecar");
            // the photo: written beside its final name, then linked in
            let dest = dir.join(&pname);
            let tmp = temp_beside(&dest);
            let written = (|| {
                let mut src = std::fs::File::open(photo)?;
                let mut f = std::fs::File::create(&tmp)?;
                std::io::copy(&mut src, &mut f)?;
                f.set_modified(mtime)?;
                f.sync_all()
            })();
            if let Err(e) = written {
                let _ = std::fs::remove_file(&tmp);
                return Err(io("cannot write the photo", e));
            }
            self.crash("after_temp");
            match link_no_replace(&tmp, &dest) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // a file appeared under this name since we looked; the photo has not been written, so the sidecar this pass
                    // wrote for it must not stay behind to become that other file's
                    let _ = std::fs::remove_file(&tmp);
                    if wrote_sidecar && let Some(sn) = &sname {
                        let _ = std::fs::remove_file(dir.join(sn));
                    }
                    races += 1;
                    continue;
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(io("cannot write the photo", e));
                }
            }
            self.crash("after_link");
            let _ = std::fs::remove_file(&tmp);
            sync_dir(&dir);
            self.crash("after_photo");
            return Ok(Outcome::Added {
                path: rel_join(&rel_folder, &pname),
                sidecar: sname.map(|s| rel_join(&rel_folder, &s)),
                wrote_sidecar,
                reason,
                folder: if rel_folder.is_empty() { ".".into() } else { rel_folder },
            });
        }
        Err(Fail::Wait(format!("no free name for {} in {} after {n} tries", note.name, dir.display())))
    }

    /// Write the result and move the import's files, then its note (the last thing, so a crash before it finds the import
    /// again), under `imports-done/<date>/<device>/`.
    fn file_away(&self, date: &str, device: &str, id: &str, files: &[(String, u64)], result: &Value) -> Result<()> {
        let done = format!("imports-done/{date}/{device}");
        let mut text = serde_json::to_string(result).map_err(|e| Error::Format(e.to_string()))?;
        text.push('\n');
        self.put_bytes(text.as_bytes(), &format!("{done}/{id}.result.json"))?;
        self.crash("after_result");
        // (the photo itself last among its files)
        let ordered = files.iter().filter(|n| !is_photo(&n.0)).chain(files.iter().filter(|n| is_photo(&n.0)));
        for (i, (name, _)) in ordered.enumerate() {
            self.retry(|| self.remote.move_file(&format!("imports/{device}/{id}/{name}"), &format!("{done}/{id}/{name}")))?;
            if i == 0 {
                self.crash("after_first_move");
            }
        }
        self.crash("after_data_moved");
        self.retry(|| self.remote.move_file(&format!("imports/{device}/{id}.json"), &format!("{done}/{id}.json")))?;
        // The import is filed away. Moving files leaves their folder behind, and a remote like Google Drive lists every folder with a
        // call of its own, so an empty `imports/<device>/<id>/` per photo ever imported would make each look at `imports/` (every
        // 2 minutes) slower for good. The note is gone, so the uploader is done with the folder; tidying is best effort.
        let _ = self.remote.remove_empty_dir(&format!("imports/{device}/{id}"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lc-import-unit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_link_never_replaces_a_file() {
        let d = dir("link");
        std::fs::write(d.join("tmp"), b"new").unwrap();
        std::fs::write(d.join("dest"), b"old").unwrap();
        let e = link_no_replace(&d.join("tmp"), &d.join("dest")).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(d.join("dest")).unwrap(), b"old");
        // not even a link that points nowhere
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(d.join("nowhere"), d.join("dangling")).is_ok();
        // (Windows: making a link needs a privilege that an ordinary user may not have; the test then has nothing to check here)
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(d.join("nowhere"), d.join("dangling")).is_ok();
        assert!(linked || cfg!(windows), "a symbolic link could not be made");
        if linked {
            assert_eq!(link_no_replace(&d.join("tmp"), &d.join("dangling")).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
            assert!(!d.join("nowhere").exists());
        }
        link_no_replace(&d.join("tmp"), &d.join("fresh")).unwrap();
        assert_eq!(std::fs::read(d.join("fresh")).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_new_file_is_created_whole_or_not_at_all() {
        let d = dir("new");
        create_new_file(&d, "a.jpg", |f| f.write_all(b"bytes")).unwrap();
        assert_eq!(std::fs::read(d.join("a.jpg")).unwrap(), b"bytes");
        let e = create_new_file(&d, "a.jpg", |f| f.write_all(b"other")).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(d.join("a.jpg")).unwrap(), b"bytes");
        // a write that fails leaves nothing behind
        let e = create_new_file(&d, "b.jpg", |_| Err(std::io::Error::other("disk full")));
        assert!(e.is_err() && !d.join("b.jpg").exists());
        let names: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["a.jpg".to_string()], "no temporary file is left");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn numbered_names_keep_the_sidecar_style() {
        let n = |name: &str, sc: Option<&str>| ImportNote {
            id: "0".repeat(32),
            folder: String::new(),
            name: name.into(),
            size: 1,
            mtime: SystemTime::UNIX_EPOCH,
            sidecar: sc.map(str::to_string),
        };
        assert_eq!(variant(&n("A.ARW", Some("A.xmp")), 3), ("A-3.ARW".to_string(), Some("A-3.xmp".to_string())));
        assert_eq!(variant(&n("A.ARW", Some("A.ARW.xmp")), 2), ("A-2.ARW".to_string(), Some("A-2.ARW.xmp".to_string())));
        assert_eq!(variant(&n("A.ARW", None), 2), ("A-2.ARW".to_string(), None));
        assert_eq!(variant(&n("A.ARW", Some("A.xmp")), 1), ("A.ARW".to_string(), Some("A.xmp".to_string())));
        assert!(is_variant_of(&n("A.ARW", None), "a-12.arw") && is_variant_of(&n("A.ARW", None), "A.ARW"));
        assert!(!is_variant_of(&n("A.ARW", None), "A-x.ARW") && !is_variant_of(&n("A.ARW", None), "A-.ARW") && !is_variant_of(&n("A.ARW", None), "B-2.ARW"));
    }
}
