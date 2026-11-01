//! Noticing the photos the user imported on this PC, in LightCraft, into the synced photo folder, and queueing them for the home PC.
//!
//! The user chooses where LightCraft imports to; whatever folder structure they set up is kept: a photo at
//! `2026/261101 - Lisbon/Day 1/DSC1.ARW` goes to the home PC as `folder = "2026/261101 - Lisbon/Day 1"` (`""` for the photo folder
//! itself), and every such folder becomes a chosen folder of this PC (a chosen folder means the photos directly in it).
//!
//! Each pass walks the photo folder (cheap: names, sizes and times only) and takes a file that
//!
//! * is a photo of a type the home PC accepts, not hidden, not a temporary or partial file, not under an `exclude` pattern;
//! * has no record: it is not one this PC downloaded, not one already queued;
//! * has **settled**: its size and modification time have not changed for [`DEFAULT_SETTLE`] (LightCraft may still be copying);
//! * is not one the library has or ever had (below), is not in the queue already, and is not part of the baseline.
//!
//! It is then hashed once (the answer is remembered by path, size and time, so the next walk costs nothing) and queued. Nothing here
//! tells LightCraft about the photo: LightCraft imported it.
//!
//! **Safety rules.**
//!
//! * *A deleted photo never comes back.* Every id that was ever in a manifest is kept for good (`seen-ids.txt`, which un-choosing a
//!   folder does not touch). A file with such an id is never uploaded: not after its folder was un-chosen, not after the home PC
//!   deleted it, not when it is restored by hand from `removed/`.
//! * *An id the library has is a duplicate*: no upload, and the pass matches the file to the manifest's photo by id, so there is no
//!   second download either. An id already queued is not queued twice.
//! * *The baseline.* The photos that are in the folder when this PC first looks are not sent by themselves (a wrong folder choice
//!   would send a whole disk). The Status window says how many of them are not in the library and has a "Send them" button.
//! * *A file that changes after it was queued* is never sent as it was found: the upload hashes it again; a changed file is dropped
//!   from the queue and found again from its current bytes.
//! * *A file renamed or moved before it was sent*: the old entry drops, the new path is found and queued once.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::import::OutEntry;
use super::pass::push_unique;
use super::state::State;
use super::{Other, OtherReport};
use crate::home::{check_import_note, content_hash_file, find_sidecars, is_excluded, is_photo, is_skipped_name, rel_join, stem, validate_xmp};
use crate::util::{iso, parse_iso, sha256_hex, write_atomic};
use crate::{Error, Result};

/// How long a file's size and time must stay the same before it is taken (LightCraft may still be copying a big import).
pub const DEFAULT_SETTLE: Duration = Duration::from_secs(120);
/// How much hashing one pass does before it leaves the rest for the next.
pub const DEFAULT_HASH_BUDGET: Duration = Duration::from_secs(30);
/// The XMP validator's limit.
const MAX_SIDECAR: u64 = 2_000_000;

/// `/`-separated relative path of `rel` (what the manifest and the commit note use), whatever the system's separator is.
pub fn slash_path(rel: &Path) -> String {
    rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/")
}

fn nanos(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// What this PC remembers about each photo file of the folder.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Seen {
    size: u64,
    mtime_ns: u64,
    /// ISO time the size and time were first seen to be these.
    since: String,
    /// The content hash (of these bytes: it is dropped when the size or time change).
    id: Option<String>,
    /// In the folder when this PC first looked: not sent unless the user says so.
    baseline: bool,
    /// The home PC would not accept it, and why (told once for the file, as a line in the problems).
    refused: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Memo {
    /// The first look at the folder is done: the files it found are the baseline.
    baseline_done: bool,
    files: BTreeMap<String, Seen>,
}

impl Memo {
    fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("pickup.json")
    }

    fn load(state_dir: &Path) -> Memo {
        std::fs::read(Self::path(state_dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self, state_dir: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self).map_err(|e| Error::Io(format!("pickup: {e}")))?;
        write_atomic(&Self::path(state_dir), &bytes)
    }
}

/// Forget what was learnt about one file (its bytes changed under it): the next pass looks at it afresh.
pub(super) fn forget_file(state_dir: &Path, rel: &str) {
    let mut m = Memo::load(state_dir);
    if m.files.remove(rel).is_some() {
        let _ = m.save(state_dir);
    }
}

fn seen_path(state_dir: &Path) -> PathBuf {
    state_dir.join("seen-ids.txt")
}

/// Every id that was ever in a manifest, or None when no manifest has been read yet.
fn load_seen(state_dir: &Path) -> Option<HashSet<String>> {
    let t = std::fs::read_to_string(seen_path(state_dir)).ok()?;
    Some(t.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
}

/// Add the manifest's ids to the ones ever seen. Append-only: nothing ever leaves the set.
pub(super) fn record_seen<'a>(state_dir: &Path, ids: impl Iterator<Item = &'a str>) -> Result<()> {
    use std::io::Write;
    let have = load_seen(state_dir);
    let mut known = have.clone().unwrap_or_default();
    let mut add = String::new();
    for id in ids {
        if known.insert(id.to_string()) {
            add.push_str(id);
            add.push('\n');
        }
    }
    if have.is_some() && add.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(state_dir).map_err(|e| Error::io(state_dir.display(), e))?;
    let p = seen_path(state_dir);
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p).map_err(|e| Error::io(p.display(), e))?;
    f.write_all(add.as_bytes()).and_then(|()| f.sync_all()).map_err(|e| Error::io(p.display(), e))
}

/// The files in the memo that exist here with the id they had when they were hashed: id -> path. For matching the manifest's photo
/// with the file here, wherever the home PC put it.
pub(super) fn local_files_by_id(state_dir: &Path, source: &Path, folders: &[String]) -> HashMap<String, String> {
    let memo = Memo::load(state_dir);
    let mut out = HashMap::new();
    for (rel, s) in &memo.files {
        let Some(id) = &s.id else { continue };
        let folder = rel.rsplit_once('/').map_or("", |(d, _)| d);
        if !folders.iter().any(|f| f == folder) {
            continue;
        }
        let mut p = source.to_path_buf();
        p.extend(rel.split('/'));
        if std::fs::metadata(&p).is_ok_and(|m| m.is_file() && m.len() == s.size && m.modified().is_ok_and(|t| nanos(t) == s.mtime_ns)) {
            out.entry(id.clone()).or_insert_with(|| rel.clone());
        }
    }
    out
}

struct Walked {
    rel: String,
    abs: PathBuf,
    size: u64,
    mtime: SystemTime,
}

/// The state of the baseline for the Status window.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Held {
    /// Baseline photos that are not in the library (and not queued).
    pub held: usize,
    /// Baseline photos not looked at yet (their count is not final).
    pub unchecked: usize,
}

impl Other {
    /// Every photo file below the photo folder that could be taken: not hidden, not a temporary file, not under an `exclude` pattern.
    fn walk_photos(&self) -> Vec<Walked> {
        let mut out = Vec::new();
        let mut stack: Vec<(PathBuf, String)> = vec![(self.cfg.source.clone(), String::new())];
        while let Some((dir, rel)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Ok(ft) = e.file_type() else { continue };
                let r = rel_join(&rel, &name);
                if ft.is_dir() {
                    if !is_skipped_name(&name) && !is_excluded(&format!("{r}/x"), &self.cfg.exclude) {
                        stack.push((e.path(), r));
                    }
                } else if ft.is_file() && is_pickup_name(&name) && !is_excluded(&r, &self.cfg.exclude) {
                    let Ok(md) = e.metadata() else { continue };
                    out.push(Walked { rel: r, abs: e.path(), size: md.len(), mtime: md.modified().unwrap_or(UNIX_EPOCH) });
                }
            }
        }
        out.sort_by(|a, b| a.rel.cmp(&b.rel));
        out
    }

    /// Whether the user asked for the baseline photos to be sent.
    pub fn send_requested(&self) -> bool {
        self.out_queue().send_requested()
    }

    /// Notice the photos in the photo folder that are new here, and queue them (see the module docs). Local work only: it needs no
    /// network, so a PC that is offline still queues. A failure is a line in `problems`, never an error of the pass.
    pub(super) fn pickup(&self, st: &State, rep: &mut OtherReport) {
        if let Err(e) = self.pickup_inner(st, rep) {
            push_unique(&mut rep.problems, format!("Looking for new photos failed: {e}"));
        }
    }

    fn pickup_inner(&self, st: &State, rep: &mut OtherReport) -> Result<()> {
        let sd = &self.cfg.state_dir;
        let queue = self.out_queue();
        let now = self.now();
        let now_iso = iso(now);
        let mut memo = Memo::load(sd);
        let before = serde_json::to_vec(&memo).unwrap_or_default();

        // a photo that was found and then moved or renamed (or deleted) before it was sent: the old entry goes, the new path is found
        for e in queue.list() {
            if e.uploaded.is_none() && e.result.is_none() && self.target(&e.path()).is_none_or(|p| !p.is_file()) {
                self.logln(format!("{} is gone from {}; it is dropped from the upload queue", e.name, e.folder));
                queue.remove(&e.id);
            }
        }
        let walked = self.walk_photos();
        let first = !memo.baseline_done;
        let present: HashSet<&str> = walked.iter().map(|w| w.rel.as_str()).collect();
        memo.files.retain(|k, _| present.contains(k.as_str()));
        for w in &walked {
            let ns = nanos(w.mtime);
            match memo.files.get_mut(&w.rel) {
                Some(s) if s.size == w.size && s.mtime_ns == ns => {}
                Some(s) => {
                    // changed under us: whatever was learnt is stale, and the quiet period starts again
                    *s = Seen { size: w.size, mtime_ns: ns, since: now_iso.clone(), baseline: s.baseline, ..Seen::default() };
                }
                None => {
                    memo.files.insert(w.rel.clone(), Seen { size: w.size, mtime_ns: ns, since: now_iso.clone(), baseline: first, ..Seen::default() });
                }
            }
        }
        memo.baseline_done = true;
        if queue.take_send_request() {
            for s in memo.files.values_mut() {
                s.baseline = false;
            }
        }

        let seen_ids = load_seen(sd);
        let mut queued: HashSet<String> = queue.list().into_iter().map(|e| e.id).collect();
        let rec_paths: HashSet<&str> = st.photos.values().map(|r| r.path.as_str()).collect();
        let queue_paths: HashSet<String> = queue.list().iter().map(OutEntry::path).collect();
        let known = |id: &str, queued: &HashSet<String>| seen_ids.as_ref().is_some_and(|s| s.contains(id)) || st.photos.contains_key(id) || queued.contains(id);
        let settle = self.cfg.settle;
        let started = Instant::now();
        let mut hashed = 0usize;
        let (mut unsettled, mut left) = (0usize, 0usize);
        let mut dirs: HashMap<PathBuf, (Vec<String>, BTreeSet<String>)> = HashMap::new();
        let mut held = Held::default();

        // new files first, the baseline (which only has to be counted) after
        let mut order: Vec<&Walked> = walked.iter().collect();
        order.sort_by_key(|w| memo.files.get(&w.rel).is_some_and(|s| s.baseline));
        for w in order {
            if rec_paths.contains(w.rel.as_str()) || queue_paths.contains(&w.rel) {
                continue;
            }
            let Some(s) = memo.files.get(&w.rel).cloned() else { continue };
            if s.refused.is_some() {
                continue;
            }
            let settled = parse_iso(&s.since).is_some_and(|t| now.duration_since(t).unwrap_or(Duration::ZERO) >= settle);
            let id = match s.id.clone() {
                Some(id) => id,
                None => {
                    if !s.baseline && !settled {
                        unsettled += 1;
                        continue;
                    }
                    if hashed > 0 && started.elapsed() >= self.cfg.hash_budget {
                        left += 1;
                        if s.baseline {
                            held.unchecked += 1;
                        }
                        continue;
                    }
                    let (id, n) = match content_hash_file(&w.abs) {
                        Ok(x) => x,
                        Err(e) => {
                            self.logln(format!("{}: cannot be read yet ({e})", w.rel));
                            unsettled += 1;
                            continue;
                        }
                    };
                    hashed += 1;
                    if n != w.size {
                        unsettled += 1; // (still being written)
                        continue;
                    }
                    if let Some(m) = memo.files.get_mut(&w.rel) {
                        m.id = Some(id.clone());
                    }
                    id
                }
            };
            if s.baseline {
                if seen_ids.is_some() && !known(&id, &queued) {
                    held.held += 1;
                }
                continue;
            }
            if known(&id, &queued) {
                continue;
            }
            // a new photo: where it goes, its sidecar, and whether the home PC will take it
            let (folder, name) = match w.rel.rsplit_once('/') {
                Some((d, n)) => (d.to_string(), n.to_string()),
                None => (String::new(), w.rel.clone()),
            };
            let (sidecar, snapshot) = self.sidecar_beside(&w.abs, &name, &mut dirs);
            let e = OutEntry {
                id: id.clone(),
                folder,
                name,
                size: w.size,
                mtime: iso(w.mtime.min(now)),
                sidecar: sidecar.clone(),
                sidecar_sha: snapshot.as_ref().map(|b| sha256_hex(b)),
                sidecar_size: snapshot.as_ref().map_or(0, |b| b.len() as u64),
                queued: now_iso.clone(),
                // (LightCraft imported it: it is not told)
                announced: true,
                ..OutEntry::default()
            };
            if let Err(why) = check_import_note(e.note_bytes().as_slice(), &id, &self.cfg.exclude) {
                if let Some(m) = memo.files.get_mut(&w.rel) {
                    m.refused = Some(why);
                }
                continue;
            }
            queue.add(&e, snapshot.as_deref())?;
            queued.insert(id);
            rep.picked_up += 1;
            self.logln(format!("found {} in {}: queued for the home PC", e.name, if e.folder.is_empty() { "the photo folder" } else { &e.folder }));
        }

        // the files the home PC would not take: one line, however many
        let refused: Vec<(&String, &String)> = memo.files.iter().filter_map(|(k, s)| s.refused.as_ref().map(|r| (k, r))).collect();
        match refused.as_slice() {
            [] => {}
            [(rel, why)] => push_unique(&mut rep.problems, format!("{rel} is not sent to the home PC: {why}")),
            [(rel, why), ..] => push_unique(
                &mut rep.problems,
                format!("{} photos cannot be sent to the home PC (for example {rel}: {why}); they stay on this PC.", refused.len()),
            ),
        }
        // folders to keep: every folder a queued photo sits in
        let chosen: HashSet<&str> = self.cfg.folders.iter().map(String::as_str).collect();
        for e in queue.list() {
            if !e.dismissed && !chosen.contains(e.folder.as_str()) && !rep.new_folders.contains(&e.folder) {
                rep.new_folders.push(e.folder.clone());
            }
        }
        rep.new_folders.sort();
        rep.unsettled = unsettled;
        rep.pickup_left = left;
        rep.recheck = if left > 0 { Some(Duration::from_secs(30)) } else if unsettled > 0 { Some(settle + Duration::from_secs(5)) } else { None };
        queue.write_held(&held);
        if serde_json::to_vec(&memo).unwrap_or_default() != before {
            memo.save(sd)?;
        }
        Ok(())
    }

    /// The sidecar beside a photo (either naming style) and its bytes, if the home PC would take it.
    fn sidecar_beside(&self, photo: &Path, name: &str, dirs: &mut HashMap<PathBuf, (Vec<String>, BTreeSet<String>)>) -> (Option<String>, Option<Vec<u8>>) {
        let Some(dir) = photo.parent() else { return (None, None) };
        let (photos, files) = dirs.entry(dir.to_path_buf()).or_insert_with(|| {
            let mut photos = Vec::new();
            let mut files = BTreeSet::new();
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if is_photo(&n) {
                    photos.push(n.clone());
                }
                files.insert(n);
            }
            (photos, files)
        });
        let (found, _) = find_sidecars(photos, files);
        let Some(Some(sc)) = found.get(name) else { return (None, None) };
        // the home PC wants exactly `<stem>.xmp` or `<name>.xmp`
        if *sc != format!("{}.xmp", stem(name)) && *sc != format!("{name}.xmp") {
            return (None, None);
        }
        let p = dir.join(sc);
        match std::fs::read(&p) {
            Ok(b) if b.len() as u64 <= MAX_SIDECAR && validate_xmp(&b).is_none() => (Some(sc.clone()), Some(b)),
            _ => (None, None),
        }
    }
}

/// Whether `file` (a photo's name) is one pickup would look at.
pub fn is_pickup_name(name: &str) -> bool {
    !is_skipped_name(name) && !name.ends_with('~') && is_photo(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_is_written_with_slashes_whatever_the_system_uses() {
        let mut p = PathBuf::new();
        for c in ["2026", "261101 - Lisbon", "Day 1", "DSC1.ARW"] {
            p.push(c);
        }
        assert_eq!(slash_path(&p), "2026/261101 - Lisbon/Day 1/DSC1.ARW");
        assert_eq!(slash_path(Path::new("")), "");
        #[cfg(windows)]
        assert_eq!(slash_path(Path::new("2026\\Day 1\\a.jpg")), "2026/Day 1/a.jpg");
    }

    #[test]
    fn temporary_hidden_and_other_files_are_not_looked_at() {
        for n in ["a.jpg", "DSC1.ARW", "x.CR3", "a b.tif"] {
            assert!(is_pickup_name(n), "{n}");
        }
        for n in [".a.jpg", "a.jpg~", "a.jpg.part", "a.jpg.tmp", "a.xmp", "notes.txt", "a.crdownload", ".lc-import-x.part"] {
            assert!(!is_pickup_name(n), "{n}");
        }
    }
}
