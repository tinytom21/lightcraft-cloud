//! Photos from the phone to the home PC (FORMAT.md, "Imports"): the phone is an uploader like another PC, with a queue of its own.
//!
//! The app copies the photos it wants to send to files (a picker gives content URIs, the camera folder gives MediaStore rows) and
//! hands the paths to [`CloudLibrary::enqueue_uploads`]. Each one is hashed with the crate's own content hash, checked against the
//! manifest (a photo the library already has is never queued) and moved into the queue folder:
//!
//! ```text
//! <dir>/uploads/<id>.json    the entry: name, destination folder, size, when it was queued / committed, what the home PC said
//! <dir>/uploads/<id>.photo   the photo's bytes, until the home PC has answered
//! <dir>/uploads/tally.json   what finished since the report was last cleared (counts and bytes), what was skipped
//! <dir>/uploads/staging/     where the app puts the copies it hands over (same file system: handing over is a rename)
//! ```
//!
//! [`CloudLibrary::upload_step`] does a bounded amount of work per call and can be stopped at any point: the photo's bytes, then the
//! commit note `imports/<device>/<id>.json` last (what is already on the remote at the right size is not sent again, so a killed
//! upload resumes), then the home PC's `imports-done/.../<id>.result.json` is read and the local copy deleted. The sending itself
//! is [`crate::imports`], shared with another PC.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::{CloudLibrary, lock};
use crate::home::{check_import_target, content_hash_file, is_photo};
use crate::imports::{Awaiting, OutResult, Outgoing, collect_results, import_note, list_outgoing, send_import};
use crate::other::UploadBudget;
use crate::util::{iso, write_atomic};
use crate::{Error, Result};

/// The queue's folder below the state folder.
pub const UPLOADS_DIR: &str = "uploads";
/// The folder below the photo folder that phone uploads go to: `Phone/<YYYY-MM-DD>`.
pub const PHONE_FOLDER: &str = "Phone";

/// One photo the app hands over.
#[derive(Debug, Clone, PartialEq)]
pub struct UploadCandidate {
    /// A file the app made (a copy of the picked photo). It is taken: moved into the queue, or deleted when the photo is not queued.
    pub path: PathBuf,
    /// The photo's file name, with its extension (what the home PC will call it).
    pub name: String,
    /// The local date the photo was taken, `YYYY-MM-DD`: the folder below `Phone/`.
    pub date: String,
    /// When it was taken, ms since 1970 (UTC), or 0 when unknown (the file's own time is used).
    pub taken_ms: i64,
    /// Found by the automatic camera job (not picked by the person): held back while only a metered network is up and the app asks for
    /// Wi-Fi (see [`CloudLibrary::upload_step`]).
    pub automatic: bool,
}

/// What [`CloudLibrary::enqueue_uploads`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnqueueReport {
    pub queued: usize,
    /// The library has the photo already (same content id): not queued.
    pub known: usize,
    /// Already waiting in the queue.
    pub already_queued: usize,
    /// HEIC / HEIF: LightCraft cannot read them yet, so they are not sent.
    pub skipped_heic: usize,
    /// Another format the home PC does not take.
    pub skipped_other: usize,
    /// `name: why` for photos that could not be queued (unreadable file, a bad date).
    pub failed: Vec<String>,
}

/// A photo the home PC refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRejection {
    pub name: String,
    pub reason: String,
}

/// The state of the uploads, for a status line (no network).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadStatus {
    /// Photos in this batch: waiting, sent and answered.
    pub total: usize,
    /// Photos not sent yet (the one being sent included).
    pub waiting: usize,
    /// Of those, the ones found by the automatic camera job (they wait for Wi-Fi when the app says so).
    pub automatic_waiting: usize,
    /// Photos sent whose answer has not come yet (the home PC has not synchronized).
    pub awaiting_home: usize,
    /// A step is sending right now.
    pub sending: bool,
    /// The home PC added them to its photo folder.
    pub added: usize,
    /// The home PC had them already.
    pub duplicate: usize,
    /// Offered photos the library already had.
    pub known: usize,
    pub rejected: Vec<UploadRejection>,
    pub skipped_heic: usize,
    pub skipped_other: usize,
    /// Bytes of the batch that reached the remote, and all of its bytes.
    pub bytes_done: u64,
    pub bytes_total: u64,
}

impl UploadStatus {
    /// Nothing to show.
    pub fn is_empty(&self) -> bool {
        *self == UploadStatus::default()
    }
}

/// What one [`CloudLibrary::upload_step`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadStep {
    pub files_sent: usize,
    pub bytes_sent: u64,
    /// Photos whose commit note was written in this step.
    pub committed: usize,
    /// Results seen in this step: (id, status).
    pub results: Vec<(String, String)>,
    /// Problems the user can act on (a copy that vanished).
    pub problems: Vec<String>,
    /// The budget ran out with photos left: call again soon.
    pub more: bool,
    /// Photos sent that the home PC has not answered: look again later.
    pub awaiting_home: usize,
    pub status: UploadStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct UploadEntry {
    id: String,
    name: String,
    /// `Phone/<date>`.
    folder: String,
    size: u64,
    mtime: String,
    queued: String,
    /// Queued by the automatic camera job (a photo the person picked is `false`; entries from before this field are picked).
    automatic: bool,
    /// When the commit note was written.
    uploaded: Option<String>,
    result: Option<OutResult>,
}

impl UploadEntry {
    fn waiting(&self) -> bool {
        self.uploaded.is_none() && self.result.is_none()
    }
    fn awaiting(&self) -> bool {
        self.uploaded.is_some() && self.result.is_none()
    }
    fn rejected(&self) -> bool {
        self.result.as_ref().is_some_and(OutResult::is_rejected)
    }
    fn active(&self) -> bool {
        self.result.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Tally {
    added: usize,
    duplicate: usize,
    known: usize,
    skipped_heic: usize,
    skipped_other: usize,
    /// Bytes of the photos that finished (added or duplicate).
    bytes: u64,
}

fn is_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `YYYY-MM-DD` of a real-looking date.
fn valid_date(d: &str) -> bool {
    let b = d.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' || ![0, 1, 2, 3, 5, 6, 8, 9].iter().all(|&i| b[i].is_ascii_digit()) {
        return false;
    }
    let (y, m, day): (u32, u32, u32) = (d[..4].parse().unwrap_or(0), d[5..7].parse().unwrap_or(0), d[8..].parse().unwrap_or(0));
    y >= 1900 && (1..=12).contains(&m) && (1..=31).contains(&day)
}

/// A file name the home PC accepts: characters it refuses become `_`, no leading dot, no trailing dot or space, at most 200 bytes
/// (the extension kept).
fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut s: String = base.chars().map(|c| if c.is_control() || "/\\<>:\"|?*".contains(c) { '_' } else { c }).collect();
    s = s.trim_start_matches('.').trim_end_matches(['.', ' ']).to_string();
    if s.len() > 200 {
        let ext = s.rfind('.').map(|i| s[i..].to_string()).filter(|e| e.len() <= 12).unwrap_or_default();
        let keep = 200 - ext.len();
        let mut cut = keep;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s = format!("{}{ext}", &s[..cut]);
    }
    s
}

fn ext_of(name: &str) -> String {
    name.rfind('.').map(|i| name[i + 1..].to_lowercase()).unwrap_or_default()
}

/// Whether the phone sends a file called `name`: the photo types the home PC adopts, except HEIC / HEIF (LightCraft cannot read them).
pub fn is_sendable(name: &str) -> bool {
    is_photo(name) && !is_heic(name)
}

/// HEIC / HEIF by name.
pub fn is_heic(name: &str) -> bool {
    matches!(ext_of(name).as_str(), "heic" | "heif")
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    let tmp = crate::util::temp_beside(to);
    let r = std::fs::copy(from, &tmp).and_then(|_| std::fs::rename(&tmp, to));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return r;
    }
    let _ = std::fs::remove_file(from);
    Ok(())
}

impl CloudLibrary {
    fn uploads_dir(&self) -> PathBuf {
        self.dir.join(UPLOADS_DIR)
    }

    /// Where the app puts the copies it hands to [`CloudLibrary::enqueue_uploads`] (the same file system as the queue).
    pub fn upload_staging_dir(&self) -> PathBuf {
        let d = self.uploads_dir().join("staging");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn entry_json(&self, id: &str) -> PathBuf {
        self.uploads_dir().join(format!("{id}.json"))
    }

    fn entry_photo(&self, id: &str) -> PathBuf {
        self.uploads_dir().join(format!("{id}.photo"))
    }

    fn upload_entries(&self) -> Vec<UploadEntry> {
        let Ok(rd) = std::fs::read_dir(self.uploads_dir()) else { return Vec::new() };
        let mut v: Vec<UploadEntry> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let id = name.strip_suffix(".json").filter(|i| is_id(i))?.to_string();
                let entry: UploadEntry = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
                (entry.id == id).then_some(entry)
            })
            .collect();
        v.sort_by(|a, b| (&a.queued, &a.id).cmp(&(&b.queued, &b.id)));
        v
    }

    fn save_entry(&self, e: &UploadEntry) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(e).map_err(|x| Error::Io(format!("upload queue: {x}")))?;
        write_atomic(&self.entry_json(&e.id), &bytes)
    }

    fn remove_entry(&self, id: &str) {
        let _ = std::fs::remove_file(self.entry_json(id));
        let _ = std::fs::remove_file(self.entry_photo(id));
    }

    fn read_tally(&self) -> Tally {
        std::fs::read(self.uploads_dir().join("tally.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn write_tally(&self, t: &Tally) -> Result<()> {
        let bytes = serde_json::to_vec(t).map_err(|x| Error::Io(format!("upload tally: {x}")))?;
        write_atomic(&self.uploads_dir().join("tally.json"), &bytes)
    }

    /// Whether the library (the last manifest) has a photo with this content id.
    pub fn has_photo(&self, id: &str) -> bool {
        lock(&self.inner).items.iter().any(|i| i.id == id)
    }

    /// Queue photos for the home PC. Photos the library has already, HEIC / HEIF and unknown formats are counted and not queued; every
    /// file handed over is taken (moved into the queue, or deleted). Fails only when the queue folder cannot be written.
    pub fn enqueue_uploads(&self, photos: &[UploadCandidate]) -> Result<EnqueueReport> {
        let _g = lock(&self.upload_lock);
        let dir = self.uploads_dir();
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(dir.display(), e))?;
        let mut rep = EnqueueReport::default();
        let mut entries = self.upload_entries();
        // a new batch: nothing is going on, so what the last one left (counts, refusals) is dropped
        if !photos.is_empty() && !entries.iter().any(UploadEntry::active) {
            for e in entries.iter().filter(|e| e.result.is_some()) {
                self.remove_entry(&e.id);
            }
            entries.retain(|e| e.result.is_none());
            self.write_tally(&Tally::default())?;
        }
        let mut tally = self.read_tally();
        for c in photos {
            let name = clean_name(&c.name);
            let r = self.enqueue_one(c, &name, &mut entries, &mut tally, &mut rep);
            if let Err(e) = r {
                rep.failed.push(format!("{name}: {e}"));
            }
            if c.path.exists() && !self.is_queue_file(&c.path) {
                let _ = std::fs::remove_file(&c.path);
            }
        }
        self.write_tally(&tally)?;
        Ok(rep)
    }

    fn is_queue_file(&self, p: &Path) -> bool {
        p.starts_with(self.uploads_dir()) && !p.starts_with(self.uploads_dir().join("staging"))
    }

    fn enqueue_one(&self, c: &UploadCandidate, name: &str, entries: &mut Vec<UploadEntry>, tally: &mut Tally, rep: &mut EnqueueReport) -> Result<()> {
        if is_heic(name) {
            rep.skipped_heic += 1;
            tally.skipped_heic += 1;
            return Ok(());
        }
        if !is_sendable(name) {
            rep.skipped_other += 1;
            tally.skipped_other += 1;
            return Ok(());
        }
        if !valid_date(&c.date) {
            return Err(Error::Invalid(format!("the date {:?} is not YYYY-MM-DD", c.date)));
        }
        let folder = format!("{PHONE_FOLDER}/{}", c.date);
        check_import_target(&folder, name, &[]).map_err(Error::Invalid)?;
        let (id, size) = content_hash_file(&c.path)?;
        if size == 0 {
            return Err(Error::Invalid("the file is empty".into()));
        }
        if self.has_photo(&id) {
            rep.known += 1;
            tally.known += 1;
            return Ok(());
        }
        if let Some(old) = entries.iter_mut().find(|e| e.id == id) {
            if !old.rejected() {
                // (picked by the person now: no longer held for Wi-Fi)
                if old.automatic && !c.automatic && old.waiting() {
                    old.automatic = false;
                    self.save_entry(old)?;
                }
                rep.already_queued += 1;
                return Ok(());
            }
            old.automatic = c.automatic;
            // refused before and offered again: send it anew
            old.result = None;
            old.uploaded = None;
            old.folder = folder;
            old.name = name.to_string();
            move_file(&c.path, &self.entry_photo(&id)).map_err(|e| Error::io(c.path.display(), e))?;
            self.save_entry(old)?;
            rep.queued += 1;
            return Ok(());
        }
        let mtime = if c.taken_ms > 0 {
            iso(std::time::UNIX_EPOCH + std::time::Duration::from_millis(c.taken_ms as u64))
        } else {
            std::fs::metadata(&c.path).and_then(|m| m.modified()).map(iso).unwrap_or_else(|_| iso(self.now()))
        };
        // (the photo first: an entry never names bytes that are not there)
        move_file(&c.path, &self.entry_photo(&id)).map_err(|e| Error::io(c.path.display(), e))?;
        let e = UploadEntry { id, name: name.to_string(), folder, size, mtime, queued: iso(self.now()), automatic: c.automatic, ..UploadEntry::default() };
        self.save_entry(&e)?;
        entries.push(e);
        rep.queued += 1;
        Ok(())
    }

    /// The uploads' state, from the state folder alone (no network).
    pub fn upload_status(&self) -> UploadStatus {
        let entries = self.upload_entries();
        let tally = self.read_tally();
        let mut s = UploadStatus {
            added: tally.added,
            duplicate: tally.duplicate,
            known: tally.known,
            skipped_heic: tally.skipped_heic,
            skipped_other: tally.skipped_other,
            bytes_done: tally.bytes,
            bytes_total: tally.bytes,
            sending: self.uploading.load(Ordering::SeqCst),
            ..UploadStatus::default()
        };
        for e in &entries {
            if e.rejected() {
                s.rejected.push(UploadRejection { name: e.name.clone(), reason: e.result.as_ref().and_then(|r| r.reason.clone()).unwrap_or_else(|| "no reason given".into()) });
                continue;
            }
            s.bytes_total += e.size;
            if e.waiting() {
                s.waiting += 1;
                s.automatic_waiting += usize::from(e.automatic);
            } else if e.awaiting() {
                s.awaiting_home += 1;
                s.bytes_done += e.size;
            }
        }
        if s.sending
            && let Some(n) = self.remote.bytes_in_flight()
        {
            s.bytes_done = (s.bytes_done + n).min(s.bytes_total);
        }
        s.total = s.waiting + s.awaiting_home + s.added + s.duplicate + s.rejected.len();
        if s.total == 0 && s.skipped_heic == 0 && s.skipped_other == 0 && s.known == 0 {
            return UploadStatus::default();
        }
        s
    }

    /// Photos queued that have not been answered yet (waiting or sent).
    pub fn uploads_pending(&self) -> usize {
        self.upload_entries().iter().filter(|e| e.active()).count()
    }

    /// Forget what finished (counts, refusals, skipped photos): the status line goes away. Photos still being sent stay.
    pub fn clear_upload_report(&self) -> Result<()> {
        let _g = lock(&self.upload_lock);
        for e in self.upload_entries().iter().filter(|e| e.result.is_some()) {
            self.remove_entry(&e.id);
        }
        let t = self.read_tally();
        let active = self.upload_entries().iter().any(UploadEntry::active);
        self.write_tally(&if active { Tally { added: t.added, duplicate: t.duplicate, bytes: t.bytes, ..Tally::default() } } else { Tally::default() })
    }

    /// Do a bounded amount of the queue's work: look for the home PC's answers to photos already sent, send photos (bytes, then the
    /// note last), tidy up. A network failure is an `Err(Error::Remote)` (what was sent stays recorded); anything else is a line in
    /// `problems`. Only one step runs at a time.
    ///
    /// `allow_automatic`: whether photos found by the automatic camera job may be sent now. The app passes `false` while "Only on Wi-Fi"
    /// is on and the network is metered; those photos stay queued (`status.automatic_waiting`) and photos the person picked go.
    pub fn upload_step(&self, budget: &UploadBudget, allow_automatic: bool) -> Result<UploadStep> {
        let _g = lock(&self.upload_lock);
        struct Flag<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Flag<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let mut entries = self.upload_entries();
        let mut step = UploadStep::default();
        if entries.iter().any(UploadEntry::active) {
            self.uploading.store(true, Ordering::SeqCst);
        }
        let _flag = Flag(&self.uploading);
        let r = self.upload_inner(budget, allow_automatic, &mut entries, &mut step);
        self.uploading.store(false, Ordering::SeqCst);
        step.awaiting_home = entries.iter().filter(|e| e.awaiting()).count();
        step.status = self.upload_status();
        r.map(|()| step)
    }

    fn upload_inner(&self, budget: &UploadBudget, allow_automatic: bool, entries: &mut [UploadEntry], step: &mut UploadStep) -> Result<()> {
        let rt = self.retrier();
        let outbox = self.dir.join("outbox");
        // a copy that vanished (nothing to send): refused here, with the reason
        for e in entries.iter_mut().filter(|e| e.waiting()) {
            if !std::fs::metadata(self.entry_photo(&e.id)).is_ok_and(|m| m.len() == e.size) {
                e.result = Some(OutResult { status: "rejected".into(), path: None, reason: Some("the copy kept on this phone is gone; pick the photo again".into()) });
                self.save_entry(e)?;
                step.problems.push(format!("{} is not on this phone's upload queue any more, so it was not sent.", e.name));
            }
        }
        // answers first: they free the disk
        let awaiting: Vec<Awaiting> = entries.iter().filter(|e| e.awaiting()).map(|e| Awaiting { id: &e.id, uploaded: e.uploaded.as_deref().unwrap_or(""), retried: false }).collect();
        let found = collect_results(&*self.remote, rt, &self.device, &outbox, &awaiting)?;
        let mut tally = self.read_tally();
        for (id, result) in found {
            let Some(e) = entries.iter_mut().find(|e| e.id == id) else { continue };
            let _ = std::fs::remove_file(self.entry_photo(&id));
            step.results.push((id.clone(), result.status.clone()));
            match result.status.as_str() {
                "added" | "duplicate" => {
                    if result.status == "added" {
                        tally.added += 1;
                    } else {
                        tally.duplicate += 1;
                    }
                    tally.bytes += e.size;
                    self.write_tally(&tally)?;
                    // (the tally first: a crash between the two counts the photo twice at worst, never loses it)
                    let _ = std::fs::remove_file(self.entry_json(&id));
                    e.result = Some(result);
                }
                _ => {
                    e.result = Some(result);
                    self.save_entry(e)?;
                }
            }
        }
        let started = Instant::now();
        let may = |e: &UploadEntry| e.waiting() && (allow_automatic || !e.automatic);
        if !entries.iter().any(may) {
            return Ok(());
        }
        let listing = list_outgoing(&*self.remote, rt, &self.device)?;
        let mut taken = 0;
        for e in entries.iter_mut().filter(|e| may(e)) {
            if taken >= budget.max_files || step.bytes_sent >= budget.max_bytes || started.elapsed() >= budget.max_time {
                step.more = true;
                break;
            }
            taken += 1;
            let note = serde_json::to_vec(&import_note(&e.id, &e.folder, &e.name, e.size, &e.mtime, None)).map_err(|x| Error::Io(format!("note: {x}")))?;
            let photo = self.entry_photo(&e.id);
            let out = Outgoing { id: &e.id, name: &e.name, size: e.size, photo: &photo, sidecar: None, note: &note };
            let sent = send_import(&*self.remote, rt, &self.device, &outbox, &listing, &out)?;
            step.files_sent += sent.files;
            step.bytes_sent += sent.bytes;
            e.uploaded = Some(iso(self.now()));
            self.save_entry(e)?;
            step.committed += 1;
        }
        Ok(())
    }
}
