//! The inbox: edits the phone sent (FORMAT.md, "Inbox" and "Conflict policy"), applied to the sidecars on the PC.

use std::collections::HashMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use super::merge::merge_sidecars;
use super::scan::{PHOTO_EXTS, find_sidecars, is_skipped_name, secs_of};
use super::state::{Row, save_history};
use super::xmp;
use super::{Home, State};
use crate::util::{parse_stamp, sidecar_version, temp_beside, utc_stamp, write_atomic};
use crate::{Error, Result};

/// What one pass over the inbox did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InboxSummary {
    pub applied: usize,
    pub merged: usize,
    /// Conflicts where the PC's sidecar was newer: the phone's edit went to the local history.
    pub kept: usize,
    pub rejected: usize,
    /// Identical to the current sidecar: nothing to do.
    pub noop: usize,
    pub items: Vec<String>,
    /// Folders (relative to the source) with applied or merged edits.
    pub folders: Vec<String>,
    /// The sidecars (relative to the source) written by this pass: complete files, not ones still being saved by something else.
    pub written: Vec<String>,
}

/// A parsed `<device>/<id>-<UTCstamp>-<base>.xmp`.
#[derive(Debug, Clone, PartialEq)]
pub struct InboxName {
    pub device: String,
    pub id: String,
    pub stamp: String,
    pub base: String,
    pub epoch: f64,
}

fn is_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `relpath` like `phone1/<id>-<stamp>-<base>.xmp`; None when it is not a valid inbox file name. The device name ends up
/// in local history file names: plain characters only.
pub fn parse_inbox_name(relpath: &str) -> Option<InboxName> {
    let parts: Vec<&str> = relpath.split('/').collect();
    if parts.len() != 2 || !crate::valid_device(parts[0]) {
        return None;
    }
    let stem = parts[1].strip_suffix(".xmp")?;
    let id = stem.get(..32)?;
    let rest = stem.get(32..)?.strip_prefix('-')?;
    let stamp = rest.get(..16)?;
    let base = rest.get(16..)?.strip_prefix('-')?;
    if !is_hex(id, 32) || !(is_hex(base, 16) || base == "none") {
        return None;
    }
    let t = parse_stamp(stamp)?;
    if utc_stamp(t) != stamp {
        return None; // (not a real date)
    }
    let epoch = t.duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    Some(InboxName { device: parts[0].to_string(), id: id.to_string(), stamp: stamp.to_string(), base: base.to_string(), epoch })
}

/// Newest wins, comparing the inbox stamp with the PC sidecar's mtime; ties go to the PC.
pub fn phone_is_newer(inbox_epoch: f64, local_epoch: Option<f64>) -> bool {
    local_epoch.is_none_or(|l| inbox_epoch > l)
}

/// The conflict policy. `base`: the sidecar version the phone started from, or `none`. `cur_version`: the current PC
/// sidecar's version. `local_epoch`: when the PC sidecar was last written. `can_merge`: the caller has the `base`
/// version, so a conflict can be merged setting by setting. Returns `(action, reason)`, the action one of `apply`,
/// `keep`, `noop`, `merge`; `merge` only with `can_merge` (the caller tries the merge and, if that gives up, asks again
/// without it).
pub fn decide(
    base: &str,
    cur_version: Option<&str>,
    inbox_epoch: f64,
    local_epoch: Option<f64>,
    inbox_hash: Option<&str>,
    can_merge: bool,
) -> (&'static str, &'static str) {
    if inbox_hash.is_some() && inbox_hash == cur_version {
        return ("noop", "identical to current sidecar");
    }
    if cur_version.is_none() && base == "none" {
        return ("apply", "no sidecar on either side");
    }
    if cur_version == Some(base) {
        return ("apply", "base matches current sidecar");
    }
    if cur_version.is_none() {
        return ("apply", "PC sidecar is gone; phone edit restores it");
    }
    if can_merge {
        return ("merge", "conflict: PC sidecar changed since the phone's version");
    }
    if phone_is_newer(inbox_epoch, local_epoch) {
        return ("apply", "conflict: phone edit is newer; PC version kept in history");
    }
    ("keep", "conflict: PC sidecar is newer; phone edit kept in history")
}

/// "2 phone edits applied (1 merged with PC changes) in 2024/Trip - ...": the desktop notification's text. `n`: edits
/// applied, merged ones included; `merged`: how many of them were merged with PC changes.
pub fn notify_message(n: usize, folders: &[String], source_name: &str, merged: usize) -> String {
    let mut edits = if n == 1 { "1 phone edit".to_string() } else { format!("{n} phone edits") };
    edits += &if merged > 0 { format!(" applied ({merged} merged with PC changes)") } else { " applied".to_string() };
    let names: Vec<&str> = folders.iter().map(|f| if f.is_empty() || f == "." { source_name } else { f.as_str() }).collect();
    if names.is_empty() {
        return format!("{edits} \u{2014} in LightCraft run Synchronize Folder");
    }
    let shown = names.iter().take(3).copied().collect::<Vec<_>>().join(", ")
        + &if names.len() > 3 { format!(" and {} more", names.len() - 3) } else { String::new() };
    let which = if names.len() == 1 { "this folder" } else { "these folders" };
    format!("{edits} in {shown} \u{2014} in LightCraft run Synchronize Folder on {which}")
}

/// The current sidecar of photo `rel` as (relative path, version, mtime in seconds), found by rescanning its folder.
type Found = (Option<String>, Option<String>, Option<f64>);

fn scan_sidecar_now(source: &Path, rel: &str) -> Result<Found> {
    let folder = source.join(rel).parent().map(Path::to_path_buf).unwrap_or_else(|| source.to_path_buf());
    let (files, photos) = folder_files(source, &folder)?;
    let (existing, _) = find_sidecars(&photos, &files);
    let Some(Some(sc)) = existing.get(rel) else { return Ok((None, None, None)) };
    let abs = source.join(sc);
    let data = std::fs::read(&abs).map_err(|e| Error::io(abs.display(), e))?;
    let mtime = std::fs::metadata(&abs).and_then(|m| m.modified()).map_err(|e| Error::io(abs.display(), e))?;
    Ok((Some(sc.clone()), Some(sidecar_version(&data)), Some(secs_of(super::scan::ns_of(mtime)))))
}

fn find_target_sidecar(source: &Path, rel: &str) -> Result<String> {
    let folder = source.join(rel).parent().map(Path::to_path_buf).unwrap_or_else(|| source.to_path_buf());
    let (files, photos) = folder_files(source, &folder)?;
    let (_, target) = find_sidecars(&photos, &files);
    target.get(rel).cloned().ok_or_else(|| Error::Invalid(format!("{rel} is not in its folder any more")))
}

fn folder_files(source: &Path, folder: &Path) -> Result<(std::collections::BTreeSet<String>, Vec<String>)> {
    let mut files = std::collections::BTreeSet::new();
    let mut photos = Vec::new();
    for e in std::fs::read_dir(folder).map_err(|e| Error::io(folder.display(), e))?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if e.file_type().is_ok_and(|t| t.is_file()) && !is_skipped_name(&name) {
            let r = e.path().strip_prefix(source).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or(name);
            let ext = super::scan::ext_lower(&r);
            if PHOTO_EXTS.contains(&ext.as_str()) {
                photos.push(r.clone());
            }
            files.insert(r);
        }
    }
    Ok((files, photos))
}

impl Home {
    pub(super) fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let tmp = std::env::temp_dir().join(format!(
            "lc-home-get-{}-{}",
            std::process::id(),
            temp_beside(Path::new("x")).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        ));
        let r = self.retry(|| self.remote.get(path, &tmp)).and_then(|_| std::fs::read(&tmp).map_err(|e| Error::io(path, e)));
        let _ = std::fs::remove_file(&tmp);
        r
    }

    pub(super) fn put_bytes(&self, bytes: &[u8], path: &str) -> Result<()> {
        let tmp = std::env::temp_dir().join(format!(
            "lc-home-put-{}-{}",
            std::process::id(),
            temp_beside(Path::new("x")).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        ));
        std::fs::write(&tmp, bytes).map_err(|e| Error::io(tmp.display(), e))?;
        let r = self.retry(|| self.remote.put(&tmp, path));
        let _ = std::fs::remove_file(&tmp);
        r
    }

    /// Run a remote call; a failure of the remote (network, quota, ...) is tried up to three times in all, like rclone's
    /// own `--retries`.
    pub(super) fn retry<T>(&self, mut f: impl FnMut() -> Result<T>) -> Result<T> {
        let mut attempt = 0;
        loop {
            match f() {
                Err(Error::Remote(e)) if attempt < 2 => {
                    attempt += 1;
                    self.logln(format!("  remote error, retrying ({attempt}/2): {e}"));
                    std::thread::sleep(self.retry_delay * attempt);
                }
                r => return r,
            }
        }
    }

    /// The pending inbox files (`inbox/<device>/<name>.xmp`), relative to `inbox/`.
    pub fn inbox_pending(&self) -> Result<Vec<String>> {
        match self.remote.list("inbox", true) {
            Ok(v) => Ok(v.into_iter().filter(|e| !e.is_dir && e.path.ends_with(".xmp")).map(|e| e.path).collect()),
            Err(Error::NotFound(_)) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Apply the phone's edits. `dry`: decide and report, change nothing.
    pub fn process_inbox(&self, st: &State, dry: bool) -> Result<InboxSummary> {
        let mut summary = InboxSummary::default();
        let items = self.inbox_pending()?;
        if items.is_empty() {
            return Ok(summary);
        }
        let mut parsed: Vec<(Option<super::inbox::InboxName>, String)> = items.into_iter().map(|p| (parse_inbox_name(&p), p)).collect();
        // oldest stamp first; unparseable ones last (they are rejected)
        parsed.sort_by(|a, b| {
            let ka = a.0.as_ref().map_or(1e18, |p| p.epoch);
            let kb = b.0.as_ref().map_or(1e18, |p| p.epoch);
            ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.cmp(&b.1))
        });
        let mut by_id: HashMap<&str, (&String, &Row)> = HashMap::new();
        for (p, r) in st.files() {
            // every ok row, so an edit addressed to a pair's alternate id (raw or JPEG) finds its own file
            if r.status == "ok"
                && let Some(id) = r.id.as_deref().filter(|i| !i.is_empty())
            {
                by_id.entry(id).or_insert((p, r));
            }
        }
        let versions = self.versions();
        let source = &self.cfg.source;
        let date = self.today();
        let mut cur: HashMap<String, (String, f64)> = HashMap::new(); // sidecars written during this run
        let mut scan_cache: HashMap<String, Found> = HashMap::new();

        for (p, path) in parsed {
            let reject = |summary: &mut InboxSummary, why: &str| -> Result<()> {
                summary.rejected += 1;
                summary.items.push(format!("reject {path}: {why}"));
                self.logln(format!("inbox: REJECT {path}: {why}"));
                if !dry {
                    let dest = format!("inbox-done/{date}/{path}");
                    self.retry(|| self.remote.move_file(&format!("inbox/{path}"), &dest))?;
                    self.put_bytes(format!("rejected {}: {why}\n", crate::util::iso(self.now())).as_bytes(), &format!("{dest}.rejected"))?;
                }
                Ok(())
            };
            let done = || -> Result<()> {
                if !dry {
                    self.retry(|| self.remote.move_file(&format!("inbox/{path}"), &format!("inbox-done/{date}/{path}")))?;
                }
                Ok(())
            };
            let Some(p) = p else {
                reject(&mut summary, "file name is not <device>/<id>-<UTCstamp>-<base>.xmp")?;
                continue;
            };
            let data = self.get_bytes(&format!("inbox/{path}"))?;
            if let Some(err) = xmp::validate(&data) {
                reject(&mut summary, &err)?;
                continue;
            }
            let inbox_hash = sidecar_version(&data);
            if !dry {
                versions.store(&data, &inbox_hash); // the phone's next edit may name this as its base
            }
            let Some((rel, _)) = by_id.get(p.id.as_str()).map(|(rel, r)| ((*rel).clone(), *r)) else {
                reject(&mut summary, &format!("unknown photo id {}", p.id))?;
                continue;
            };
            if !scan_cache.contains_key(&rel) {
                scan_cache.insert(rel.clone(), scan_sidecar_now(source, &rel)?);
            }
            let (sc_rel, mut sc_hash, mut sc_mtime) = scan_cache[&rel].clone();
            if let Some((h, t)) = cur.get(&rel) {
                sc_hash = Some(h.clone());
                sc_mtime = Some(*t);
            }
            // a conflict is merged setting by setting when the version the phone started from is still known
            let base_data = match &sc_hash {
                Some(h) if p.base != "none" && p.base != *h => versions.load(&p.base),
                _ => None,
            };
            let (mut action, mut reason) = decide(&p.base, sc_hash.as_deref(), p.epoch, sc_mtime, Some(&inbox_hash), base_data.is_some());
            let mut reason_s = reason.to_string();
            let mut merged: Option<Vec<u8>> = None;
            if action == "merge" {
                let res = (|| {
                    let pc_data = std::fs::read(source.join(sc_rel.as_deref()?)).ok()?;
                    let r = merge_sidecars(base_data.as_deref()?, &pc_data, &data, phone_is_newer(p.epoch, sc_mtime))?;
                    Some((pc_data, r))
                })();
                match res {
                    None => {
                        (action, reason) = decide(&p.base, sc_hash.as_deref(), p.epoch, sc_mtime, Some(&inbox_hash), false);
                        reason_s = reason.to_string();
                    }
                    Some((pc_data, (out, _))) if out == pc_data => {
                        (action, reason_s) = ("noop", "merge changes nothing: the PC sidecar already has the phone's edits".to_string());
                    }
                    Some((_, (out, conflicts))) => {
                        let n = conflicts.len();
                        let shown = conflicts.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
                            + &if n > 6 { format!(", +{} more", n - 6) } else { String::new() };
                        reason_s =
                            format!("{n} conflict{}", if n == 1 { "" } else { "s" }) + &if n > 0 { format!(": {shown}") } else { String::new() };
                        merged = Some(out);
                    }
                }
            }
            self.logln(format!("inbox: {path} -> {action} ({reason_s})"));
            summary.items.push(format!("{action} {path}: {reason_s}"));
            let target = match &sc_rel {
                Some(s) => s.clone(),
                None => find_target_sidecar(source, &rel)?,
            };
            let tag = format!("{}.{}", p.device, p.stamp);
            if action == "noop" {
                summary.noop += 1;
                done()?;
                continue;
            }
            if dry {
                match action {
                    "apply" => summary.applied += 1,
                    "merge" => summary.merged += 1,
                    _ => summary.kept += 1,
                }
                continue;
            }
            let abs_t = source.join(&target);
            if action == "apply" || action == "merge" {
                if let Ok(prev) = std::fs::read(&abs_t) {
                    let m = std::fs::metadata(&abs_t).and_then(|m| m.modified()).map_err(|e| Error::io(abs_t.display(), e))?;
                    save_history(&self.cfg.state_dir, &date, &target, &prev, &format!("pc-{}", utc_stamp(m)))?;
                }
                if merged.is_some() {
                    save_history(&self.cfg.state_dir, &date, &target, &data, &tag)?;
                }
                let out = merged.as_deref().unwrap_or(&data);
                write_atomic(&abs_t, out)?;
                let new_hash = sidecar_version(out);
                versions.store(out, &new_hash);
                cur.insert(rel.clone(), (new_hash.clone(), p.epoch));
                scan_cache.insert(rel.clone(), (Some(target.clone()), Some(new_hash), Some(p.epoch)));
                if merged.is_none() {
                    summary.applied += 1;
                } else {
                    summary.merged += 1;
                }
                let folder = target.rsplit_once('/').map_or(".", |(d, _)| d).to_string();
                if !summary.folders.contains(&folder) {
                    summary.folders.push(folder);
                }
                if !summary.written.contains(&target) {
                    summary.written.push(target.clone());
                }
            } else {
                save_history(&self.cfg.state_dir, &date, &target, &data, &tag)?;
                summary.kept += 1;
            }
            done()?;
        }
        Ok(summary)
    }
}
