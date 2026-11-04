//! The upload queue of the photos this PC found in its photo folder and sends to the home PC (FORMAT.md, "Imports").
//!
//! The user imports photos in LightCraft, into the synced photo folder, in whatever folders they like. This PC's pass notices the new
//! files (see [`super::pickup`]) and queues them; this module sends them:
//!
//! * [`ImportQueue`]: one small file per photo in `<state>/imports-out/` (the app and the Status window are different processes, and
//!   a file each means neither overwrites the other's work). The sidecar is kept there as it was when the photo was found, so an edit
//!   made afterwards is an edit (sent through the inbox once the home PC has the photo), never part of the import.
//! * [`Other::upload_step`]: a bounded amount of work per call, so the app's other jobs (sidecar edits) are never kept waiting. Per
//!   photo: its bytes (hashed again first: only the bytes that were found are ever sent), its sidecar, then the commit note
//!   `imports/<device>/<id>.json` last; a file already on the remote at the right size is not sent again. Once the home PC has filed
//!   the import away, `imports-done/<day>/<device>/<id>.result.json` says what became of it; the pass then matches the manifest's photo
//!   to the file here by its id (see [`super`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::Other;
use super::pass::push_unique;
use super::state::State;
use crate::home::{RunLock, content_hash_file, rel_join};
use crate::progress::Stage;
use crate::imports::{Awaiting, Outgoing, collect_results, import_note, list_outgoing, send_import};
use crate::util::{iso, write_atomic};
use crate::{Error, Result};

/// The queue's folder below the state folder.
pub const OUT_DIR: &str = "imports-out";

// ------------------------------------------------------------------ the queue

pub use crate::imports::OutResult;

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
    /// The user chose "Keep on this PC only": the rejection is accepted, the photo is never sent again (the file stays, and a scan
    /// counts it as known).
    pub dismissed: bool,
    /// The user chose "Try again": the old rejection's result file may still be on the remote, so a new answer counts only once the
    /// home PC has taken the new note away.
    pub retried: bool,
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

    /// The home PC refused it and the user has not dealt with that.
    pub fn is_refused(&self) -> bool {
        self.result.as_ref().is_some_and(OutResult::is_rejected) && !self.dismissed
    }

    fn waiting_to_upload(&self) -> bool {
        self.uploaded.is_none() && self.result.is_none()
    }

    fn note(&self) -> serde_json::Value {
        import_note(&self.id, &self.folder, &self.name, self.size, &self.mtime, self.sidecar.as_deref())
    }

    /// The commit note as it will be written.
    pub(super) fn note_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.note()).unwrap_or_default()
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
    /// Photos the home PC refused that the user has not dealt with (try again, or keep on this PC only).
    pub rejected: usize,
    /// Photos that were in the folder when this PC first looked and are not in the cloud library (not sent unless the user says so).
    pub held: usize,
    /// Of the photos that were there at the start, how many have not been looked at yet (`held` may still grow).
    pub held_unchecked: usize,
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
            if e.is_refused() {
                s.rejected += 1;
                if !e.reported {
                    s.unreported += 1;
                }
            }
        }
        if let Some(h) = self.read_held() {
            s.held = h.held;
            s.held_unchecked = h.unchecked;
        }
        s
    }

    fn held_path(&self) -> PathBuf {
        self.dir.parent().unwrap_or(&self.dir).join("pickup-held.json")
    }

    fn read_held(&self) -> Option<super::pickup::Held> {
        std::fs::read(self.held_path()).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }

    pub(super) fn write_held(&self, h: &super::pickup::Held) {
        if self.read_held().as_ref() != Some(h)
            && let Ok(b) = serde_json::to_vec(h)
        {
            let _ = write_atomic(&self.held_path(), &b);
        }
    }

    fn send_flag(&self) -> PathBuf {
        self.dir.join(".send-held")
    }

    /// "Send them": ask the app to queue the photos that were here at the start (the file is looked at by the next pass).
    pub fn request_send_held(&self) {
        let _ = write_atomic(&self.send_flag(), b"1");
    }

    pub fn send_requested(&self) -> bool {
        self.send_flag().exists()
    }

    pub(super) fn take_send_request(&self) -> bool {
        std::fs::remove_file(self.send_flag()).is_ok()
    }

    /// The photos the home PC refused that nobody has dealt with, oldest first.
    pub fn refused(&self) -> Vec<OutEntry> {
        self.list().into_iter().filter(OutEntry::is_refused).collect()
    }

    /// "Try again": queue the refused photos for sending anew (the local files stay). Returns how many.
    pub fn retry_refused(&self) -> usize {
        let mut n = 0;
        for mut e in self.refused() {
            e.result = None;
            e.uploaded = None;
            e.reported = false;
            e.retried = true;
            if self.save(&e).is_ok() {
                n += 1;
            }
        }
        n
    }

    /// "Keep on this PC only": accept the refusals. The photos stay here and are never sent again. Returns how many.
    pub fn keep_refused(&self) -> usize {
        let mut n = 0;
        for mut e in self.refused() {
            e.dismissed = true;
            if self.save(&e).is_ok() {
                n += 1;
            }
        }
        n
    }
}

impl Other {
    pub(super) fn out_queue(&self) -> ImportQueue {
        ImportQueue::new(&self.cfg.state_dir)
    }

    /// The numbers of the upload queue, from the state folder alone (no network).
    pub fn import_queue_status(&self) -> QueueStatus {
        self.out_queue().status()
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
            let finished_and_deleted = e.result.is_some() && gone;
            if tracked || finished_and_deleted {
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
            self.rp.stage(Stage::AskingHome, 0, 0);
            self.collect_results(&queue, &mut entries, &mut rep)?;
        }
        let started = Instant::now();
        if entries.iter().any(OutEntry::waiting_to_upload) {
            self.rp.stage(Stage::LookingAtLibrary, 0, 0);
        }
        let remote_files = if entries.iter().any(OutEntry::waiting_to_upload) { Some(self.list_outgoing()?) } else { None };
        let batch = || entries.iter().filter(|e| e.waiting_to_upload()).take(budget.max_files);
        if batch().next().is_some() {
            self.rp.stage(Stage::Sending, batch().count() as u64, batch().map(|e| e.size).sum());
        }
        let sending: Result<()> = self.rp.watch(&*self.remote, || {
            for (taken, e) in entries.iter_mut().filter(|e| e.waiting_to_upload()).enumerate() {
                if taken >= budget.max_files || rep.bytes_sent >= budget.max_bytes || started.elapsed() >= budget.max_time {
                    rep.more = true;
                    break;
                }
                let sent = self.upload_one(&queue, e, remote_files.as_ref().unwrap_or(&HashMap::new()), &mut rep);
                self.rp.partial(0);
                self.rp.item_done(e.size);
                match sent {
                    Ok(()) => {}
                    Err(err @ Error::Remote(_)) => return Err(err),
                    Err(err) => push_unique(&mut rep.problems, format!("{}: {err}", e.name)),
                }
            }
            Ok(())
        });
        sending?;
        rep.queue = queue.status();
        Ok(rep)
    }

    fn list_outgoing(&self) -> Result<HashMap<String, u64>> {
        list_outgoing(&*self.remote, self.retrier(), &self.cfg.device)
    }

    fn upload_one(&self, queue: &ImportQueue, e: &mut OutEntry, remote: &HashMap<String, u64>, rep: &mut UploadReport) -> Result<()> {
        let Some(photo) = self.target(&e.path()).filter(|p| p.is_file()) else {
            push_unique(&mut rep.problems, format!("{} is not on this PC any more, so it was not sent to the home PC.", e.name));
            queue.remove(&e.id);
            return Ok(());
        };
        // only the bytes that were found are ever sent: a file that changed since is dropped and found again from its current bytes
        if !content_hash_file(&photo).is_ok_and(|(h, n)| h == e.id && n == e.size) {
            self.logln(format!("{} changed after it was found; it is looked at again before it is sent", e.path()));
            queue.remove(&e.id);
            super::pickup::forget_file(&self.cfg.state_dir, &e.path());
            return Ok(());
        }
        let sidecar_bytes = match &e.sidecar {
            Some(_) => match queue.snapshot(&e.id) {
                Some(b) => Some(b),
                None => {
                    // (the snapshot is gone: send the photo without a sidecar rather than never)
                    e.sidecar = None;
                    e.sidecar_sha = None;
                    return queue.save(e);
                }
            },
            None => None,
        };
        let note = serde_json::to_vec(&e.note()).map_err(|x| Error::Io(format!("note: {x}")))?;
        let out = Outgoing {
            id: &e.id,
            name: &e.name,
            size: e.size,
            photo: &photo,
            sidecar: e.sidecar.as_deref().zip(sidecar_bytes.as_deref()),
            note: &note,
        };
        let sent = send_import(&*self.remote, self.retrier(), &self.cfg.device, &self.cfg.state_dir.join("outbox"), remote, &out)?;
        rep.files_sent += sent.files;
        rep.bytes_sent += sent.bytes;
        e.uploaded = Some(iso(self.now()));
        queue.save(e)?;
        rep.committed += 1;
        self.logln(format!("sent {} to the home PC for adding", e.name));
        Ok(())
    }

    /// Look for `imports-done/<day>/<device>/<id>.result.json` of the photos sent and not answered yet.
    fn collect_results(&self, queue: &ImportQueue, entries: &mut [OutEntry], rep: &mut UploadReport) -> Result<()> {
        let awaiting: Vec<Awaiting> =
            entries.iter().filter(|e| e.uploaded.is_some() && e.result.is_none()).map(|e| Awaiting { id: &e.id, uploaded: e.uploaded.as_deref().unwrap_or(""), retried: e.retried }).collect();
        let found = collect_results(&*self.remote, self.retrier(), &self.cfg.device, &self.cfg.state_dir.join("outbox"), &awaiting)?;
        for (id, result) in found {
            let Some(e) = entries.iter_mut().find(|e| e.id == id) else { continue };
            e.retried = false;
            if result.is_rejected() {
                e.reported = true;
                push_unique(
                    &mut rep.problems,
                    format!("{} was not added at the home PC: {}. It stays in your photo folder.", e.name, result.reason.as_deref().unwrap_or("no reason given")),
                );
            }
            rep.results.push((id, result.status.clone()));
            e.result = Some(result);
            queue.save(e)?;
        }
        Ok(())
    }
}
