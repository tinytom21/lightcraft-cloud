//! The uploader's half of the imports protocol (FORMAT.md, "Imports"), shared by every device that sends photos to the home PC: another PC
//! ([`crate::other`]) and the phone ([`crate::CloudLibrary::upload_step`]).
//!
//! * [`import_note`]: the commit note;
//! * [`send_import`]: the photo, its sidecar if any, then the note last; a file already on the remote at the right size is not sent again;
//! * [`collect_results`]: the home PC's `imports-done/<day>/<device>/<id>.result.json` of the imports sent and not answered yet;
//! * [`Retrier`]: a network call that is tried three times.
//!
//! What stays with each device is what it sends and when: the other PC's pickup queue and the phone's queue are their own files.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::util::{iso, parse_iso, write_atomic};
use crate::{Error, Remote, Result};

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

/// `{"format":1,...}`: the commit note of an import.
pub(crate) fn import_note(id: &str, folder: &str, name: &str, size: u64, mtime: &str, sidecar: Option<&str>) -> Value {
    json!({"format": 1, "id": id, "folder": folder, "name": name, "size": size, "mtime": mtime, "sidecar": sidecar})
}

/// Network calls are tried three times (a failure that is not the network's is final at once).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Retrier {
    pub delay: Duration,
}

impl Retrier {
    pub fn run<T>(&self, mut f: impl FnMut() -> Result<T>) -> Result<T> {
        let mut last = None;
        for attempt in 0..3 {
            match f() {
                Err(Error::Remote(m)) => {
                    last = Some(Error::Remote(m));
                    if attempt < 2 {
                        std::thread::sleep(self.delay);
                    }
                }
                r => return r,
            }
        }
        Err(last.unwrap_or_else(|| Error::Remote("no answer".into())))
    }
}

/// `imports/<device>/` as the remote has it: `<id>/<name>` and `<id>.json` to size.
pub(crate) fn list_outgoing(remote: &dyn Remote, rt: Retrier, device: &str) -> Result<HashMap<String, u64>> {
    match rt.run(|| remote.list(&format!("imports/{device}"), true)) {
        Ok(v) => Ok(v.into_iter().filter(|e| !e.is_dir).map(|e| (e.path, e.size)).collect()),
        Err(Error::NotFound(_)) => Ok(HashMap::new()),
        Err(e) => Err(e),
    }
}

/// One import to send.
pub(crate) struct Outgoing<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub size: u64,
    /// The photo's bytes.
    pub photo: &'a Path,
    /// The sidecar's name and bytes.
    pub sidecar: Option<(&'a str, &'a [u8])>,
    pub note: &'a [u8],
}

/// What [`send_import`] sent.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Sent {
    pub files: usize,
    pub bytes: u64,
}

/// Send an import: the photo, the sidecar, and the note last (it is what tells the home PC the files are all there). `listing` is
/// [`list_outgoing`]; a file in it at the right size is not sent again. Small files are written to `tmp_dir` first.
pub(crate) fn send_import(remote: &dyn Remote, rt: Retrier, device: &str, tmp_dir: &Path, listing: &HashMap<String, u64>, o: &Outgoing) -> Result<Sent> {
    let mut sent = Sent::default();
    let id = o.id;
    if listing.get(&format!("{id}/{}", o.name)) != Some(&o.size) {
        rt.run(|| remote.put(o.photo, &format!("imports/{device}/{id}/{}", o.name)))?;
        sent.files += 1;
        sent.bytes += o.size;
    }
    let small = |tmp_name: String, bytes: &[u8], remote_path: String| -> Result<()> {
        let tmp = tmp_dir.join(tmp_name);
        write_atomic(&tmp, bytes)?;
        let r = rt.run(|| remote.put(&tmp, &remote_path));
        let _ = std::fs::remove_file(&tmp);
        r
    };
    if let Some((sn, bytes)) = o.sidecar
        && listing.get(&format!("{id}/{sn}")) != Some(&(bytes.len() as u64))
    {
        small(format!("{id}.xmp"), bytes, format!("imports/{device}/{id}/{sn}"))?;
        sent.files += 1;
        sent.bytes += bytes.len() as u64;
    }
    if listing.get(&format!("{id}.json")) != Some(&(o.note.len() as u64)) {
        // (the note is not counted: the caller counts the photos committed)
        small(format!("{id}.note.json"), o.note, format!("imports/{device}/{id}.json"))?;
    }
    Ok(sent)
}

/// An import that was sent and has no answer yet.
pub(crate) struct Awaiting<'a> {
    pub id: &'a str,
    /// When its note was written (ISO).
    pub uploaded: &'a str,
    /// Sent again after a rejection: the old result file may still be on the remote, so an answer counts only once the home PC has
    /// taken the new note away.
    pub retried: bool,
}

/// Look for `imports-done/<day>/<device>/<id>.result.json` of the imports in `awaiting`; returns what was found, one result per id.
/// `tmp_dir` is where a result file is read through. A result that cannot be read (yet) is skipped; a network failure is an error.
pub(crate) fn collect_results(remote: &dyn Remote, rt: Retrier, device: &str, tmp_dir: &Path, awaiting: &[Awaiting]) -> Result<Vec<(String, OutResult)>> {
    let mut found: Vec<(String, OutResult)> = Vec::new();
    if awaiting.is_empty() {
        return Ok(found);
    }
    let oldest = awaiting.iter().map(|a| a.uploaded).min().and_then(parse_iso);
    // (a day before the oldest upload, for a clock that is a little off)
    let from = oldest.map(|t| iso(t - Duration::from_secs(86_400))[..10].to_string()).unwrap_or_default();
    let days = match rt.run(|| remote.list("imports-done", false)) {
        Ok(v) => v,
        Err(Error::NotFound(_)) => return Ok(found),
        Err(e) => return Err(e),
    };
    let mut days: Vec<String> = days.into_iter().filter(|d| d.is_dir && d.path.as_str() >= from.as_str()).map(|d| d.path).collect();
    // (newest first: after a "try again" an older result of the same photo may still be there)
    days.sort_by(|a, b| b.cmp(a));
    let note_still_there = if awaiting.iter().any(|a| a.retried) { list_outgoing(remote, rt, device)? } else { HashMap::new() };
    for day in days {
        let files = match rt.run(|| remote.list(&format!("imports-done/{day}/{device}"), false)) {
            Ok(v) => v,
            Err(Error::NotFound(_)) => continue,
            Err(e) => return Err(e),
        };
        for f in files.into_iter().filter(|f| !f.is_dir) {
            let Some(id) = f.path.strip_suffix(".result.json") else { continue };
            let Some(a) = awaiting.iter().find(|a| a.id == id) else { continue };
            if found.iter().any(|(i, _)| i == id) || (a.retried && note_still_there.contains_key(&format!("{id}.json"))) {
                continue;
            }
            let tmp = tmp_dir.join(format!("{id}.result.json"));
            if let Some(dir) = tmp.parent() {
                std::fs::create_dir_all(dir).map_err(|x| Error::io(dir.display(), x))?;
            }
            let got = rt.run(|| remote.get(&format!("imports-done/{day}/{device}/{}", f.path), &tmp));
            let bytes = got.and_then(|()| std::fs::read(&tmp).map_err(|x| Error::io(tmp.display(), x)));
            let _ = std::fs::remove_file(&tmp);
            let v: Value = match bytes.and_then(|b| serde_json::from_slice(&b).map_err(|x| Error::Format(x.to_string()))) {
                Ok(v) => v,
                Err(err @ Error::Remote(_)) => return Err(err),
                Err(_) => continue, // (not readable yet or ever: the import keeps waiting)
            };
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string();
            if !matches!(status.as_str(), "added" | "duplicate" | "rejected") {
                continue;
            }
            let path = v.get("path").and_then(|s| s.as_str()).map(str::to_string);
            let reason = v.get("reason").and_then(|s| s.as_str()).map(str::to_string);
            found.push((id.to_string(), OutResult { status, path, reason }));
        }
    }
    Ok(found)
}
