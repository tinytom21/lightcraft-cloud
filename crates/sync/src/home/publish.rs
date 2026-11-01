//! Publishing: the six steps of a sync (inbox, scan, renderer, originals, mobile files, manifest), the mirror that
//! replaces `rclone sync --backup-dir`, and status.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::import::ImportSummary;
use super::inbox::InboxSummary;
use super::manifest::{ManifestPhoto, build_manifest_photos, manifest_ids};
use super::scan::{FileInfo, Plan, Scan, diff_scan, ext_lower, in_scope, published_removals, scan_source_settled, walk_source};
use super::state::{Meta, Row, RunLock, State, Uploaded};
use super::{Added, Home, PhotoInfo, Renderer};
use crate::progress::Stage;
use crate::util::{is_busy, iso, parallel, utc_stamp, write_atomic};
use crate::{Error, FORMAT, Result};

/// Uploads at once.
const JOBS: usize = 4;

/// What one mirror (originals, thumbs or previews) did, or in a dry run would do.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MirrorStats {
    /// Files new on the remote.
    pub copied_new: usize,
    /// Files replaced; the old copy went to `history/`.
    pub replaced: usize,
    /// Files gone from the source; the remote copy went to `history/`.
    pub removed: usize,
    pub unchanged: usize,
}

impl fmt::Display for MirrorStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "copied_new={} replaced_remote_backed_up={} removed_to_history={} unchanged={}",
            self.copied_new, self.replaced, self.removed, self.unchanged
        )
    }
}

/// What [`Home::sync`] did.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub dry: bool,
    pub plan: Plan,
    pub inbox: InboxSummary,
    /// Photos other PCs uploaded, adopted into the folder (step 2, after the inbox and before the scan).
    pub imports: ImportSummary,
    /// Photos that could not be imported by the renderer.
    pub failed: Vec<String>,
    pub originals: MirrorStats,
    pub thumbs: MirrorStats,
    pub previews: MirrorStats,
    pub manifest_uploaded: bool,
    /// Photos in the manifest.
    pub photos: usize,
    /// Things the user can act on, one line each (a sidecar or a photo another program holds, so it waits for the next sync). The
    /// inbox's own are in `inbox.problems`.
    pub problems: Vec<String>,
}

/// What [`Home::status`] reports.
#[derive(Debug, Clone, Default)]
pub struct StatusReport {
    pub last_run: Option<String>,
    pub last_result: Option<String>,
    pub last_counts: Option<String>,
    pub tracked: usize,
    pub not_published: usize,
    /// `None`: the remote could not be listed.
    pub remote: Option<(usize, u64)>,
    pub remote_error: Option<String>,
    pub inbox_pending: Option<usize>,
    /// Committed imports (a note with its files) waiting on the remote; `None`: could not be listed.
    pub imports_pending: Option<usize>,
}

fn refuse<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(m.into()))
}

fn check_source(root: &Path) -> Result<()> {
    if !root.exists() {
        return refuse(format!("source {} does not exist (drive not mounted?)", root.display()));
    }
    if !root.is_dir() {
        return refuse(format!("source {} is not a directory", root.display()));
    }
    match std::fs::read_dir(root) {
        Err(e) => refuse(format!("source {} unreadable: {e}", root.display())),
        Ok(mut it) => {
            if it.next().is_none() {
                return refuse(format!("source {} is empty (drive not mounted?)", root.display()));
            }
            Ok(())
        }
    }
}

/// `dir/name.153012.ext` for rclone's `--suffix .153012 --suffix-keep-extension`.
fn with_suffix(rel: &str, suffix: &str) -> String {
    let (dir, name) = rel.rsplit_once('/').map_or(("", rel), |(d, n)| (d, n));
    let named = match name.rfind('.') {
        Some(i) if i > 0 => format!("{}{suffix}{}", &name[..i], &name[i..]),
        _ => format!("{name}{suffix}"),
    };
    if dir.is_empty() { named } else { format!("{dir}/{named}") }
}

fn sha_dir_files(dir: &Path) -> BTreeMap<String, FileInfo> {
    if dir.is_dir() { walk_source(dir, &[]) } else { BTreeMap::new() }
}

impl Home {
    /// One sync: apply phone edits, publish the folder. `dry`: report what would happen, change nothing.
    pub fn sync(&self, renderer: &mut dyn Renderer, dry: bool) -> Result<SyncReport> {
        let t0 = self.now();
        check_source(&self.cfg.source)?;
        if dry {
            self.logln("DRY RUN: nothing will be changed");
        }
        let _lock = RunLock::acquire(&self.cfg.state_dir, dry)?;
        let mut st = State::open(&self.cfg.state_dir, dry)?;
        let r = self.sync_locked(&mut st, renderer, dry);
        // (what was done stays recorded even when a later step failed)
        if !dry {
            st.commit()?;
        }
        let report = r?;
        if !dry {
            self.logln(format!("sync done in {:.1}s", self.now().duration_since(t0).map_or(0.0, |d| d.as_secs_f64())));
        }
        Ok(report)
    }

    fn sync_locked(&self, st: &mut State, renderer: &mut dyn Renderer, dry: bool) -> Result<SyncReport> {
        let cfg = &self.cfg;
        let mut report = SyncReport { dry, ..SyncReport::default() };
        let known = st.files().clone();
        // (1) inbox
        self.logln("step 1/6: inbox");
        let inbox = self.process_inbox(st, dry)?;
        self.logln(format!(
            "inbox: applied={} merged={} kept-pc={} rejected={} identical={}",
            inbox.applied, inbox.merged, inbox.kept, inbox.rejected, inbox.noop
        ));
        report.inbox = inbox;
        // (1b) photos other PCs uploaded, adopted into the folder before the scan, so they are published by this same sync
        let imports = self.process_imports(st, dry)?;
        if !imports.items.is_empty() || !imports.errors.is_empty() {
            self.logln(format!(
                "imports: added={} duplicate={} rejected={} waiting={}",
                imports.added.len(),
                imports.duplicates,
                imports.rejected,
                imports.waiting
            ));
        }
        report.imports = imports;
        // (2) scan
        self.logln("step 2/6: scan source");
        self.rp.stage(Stage::Checking, 0, 0);
        let versions = self.versions();
        let settled: Vec<String> = report.inbox.written.iter().chain(&report.imports.written).cloned().collect();
        let mut scan = scan_source_settled(&cfg.source, self.now(), &cfg.exclude, (!dry).then_some(&versions), &settled);
        if scan.entries.is_empty() {
            return refuse(format!("no photos found under {}; refusing (unmounted?)", cfg.source.display()));
        }
        let plan = diff_scan(&scan, &known);
        let gone = published_removals(&plan.removed, &known).len();
        if gone > cfg.max_delete {
            return refuse(format!(
                "{gone} source photos disappeared since the last run (max_delete={}); refusing. Check the drive, or raise max_delete in the config.",
                cfg.max_delete
            ));
        }
        self.logln(format!(
            "scan: {} photos, new={} changed={} sidecar-changed={} removed={} retry={} unstable-skipped={}",
            scan.entries.len(),
            plan.new.len(),
            plan.changed.len(),
            plan.sc_changed.len(),
            plan.removed.len(),
            plan.retry.len(),
            scan.unstable.len()
        ));
        report.plan = plan.clone();
        for (sc, why) in &scan.unreadable {
            self.logln(format!("WARNING: the sidecar {sc} cannot be read now ({why}); its photo waits for the next sync"));
            report.problems.push(format!("The sidecar {sc} cannot be read now (another program may have it open); its photo is left for the next sync."));
        }
        let mobile = self.mobile();
        let date = self.today();
        let suffix = format!(".{}", &utc_stamp(self.now())[9..15]);
        if dry {
            for (k, v) in [("new", &plan.new), ("changed", &plan.changed), ("sc_changed", &plan.sc_changed), ("removed", &plan.removed)] {
                for p in v {
                    self.logln(format!("  plan {k}: {p}"));
                }
            }
            self.logln("step 4/6 (dry-run): mirror originals");
            report.originals = self.mirror_originals(st, &scan, &date, &suffix, true)?;
            self.logln(format!("  originals: {}", report.originals));
            for sub in ["thumbs", "previews"] {
                if mobile.join(sub).is_dir() {
                    let s = self.mirror_staged(st, sub, &date, &suffix, true)?;
                    self.logln(format!("  mobile/{sub}: {s}"));
                    if sub == "thumbs" {
                        report.thumbs = s;
                    } else {
                        report.previews = s;
                    }
                }
            }
            let nb = plan.new.len() + plan.changed.len() + plan.sc_changed.len();
            self.logln(format!(
                "  would render {nb} thumbnails / build {} smart previews; manifest would be rewritten if anything changed",
                plan.new.len() + plan.changed.len()
            ));
            self.logln("DRY RUN done");
            return Ok(report);
        }
        // (3) renderer
        self.logln("step 3/6: scratch library (import, smart previews, thumbnails)");
        if plan.any() {
            let (failed, busy) = self.refresh_library(st, &plan, &scan, renderer)?;
            report.failed = failed;
            if !busy.is_empty() {
                // (held by another program: left out of this sync altogether, so nothing half-done is published)
                self.logln(format!("WARNING: {} photos are in use by another program and wait for the next sync: {:?}", busy.len(), &busy[..busy.len().min(5)]));
                report.problems.push(format!(
                    "{} {} in use by another program ({}), so {} left for the next sync.",
                    busy.len(),
                    if busy.len() == 1 { "photo is" } else { "photos are" },
                    busy.iter().take(3).cloned().collect::<Vec<_>>().join(", ") + &if busy.len() > 3 { ", ...".to_string() } else { String::new() },
                    if busy.len() == 1 { "it is" } else { "they are" }
                ));
                scan.unstable.extend(busy);
            }
            if !report.failed.is_empty() {
                self.logln(format!("WARNING: {} photos failed to import: {:?}", report.failed.len(), &report.failed[..report.failed.len().min(5)]));
            }
        }
        let previews = staged_sizes(&mobile.join("previews"), "lcsp");
        let photos = build_manifest_photos(st.files(), &previews, cfg.pair_primary);
        let live = manifest_ids(&photos);
        for sub in ["thumbs", "previews"] {
            for (name, fi) in sha_dir_files(&mobile.join(sub)) {
                let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
                if !live.contains(stem) || name.starts_with('.') {
                    let _ = std::fs::remove_file(&fi.abs);
                }
            }
        }
        // (4) originals
        self.logln("step 4/6: mirror originals");
        report.originals = self.mirror_originals(st, &scan, &date, &suffix, false)?;
        self.logln(format!("  originals: {}", report.originals));
        // (5) mobile files
        self.logln("step 5/6: mirror mobile thumbs + previews");
        for sub in ["thumbs", "previews"] {
            std::fs::create_dir_all(mobile.join(sub)).map_err(|e| Error::io(mobile.join(sub).display(), e))?;
            let s = self.mirror_staged(st, sub, &date, &suffix, false)?;
            self.logln(format!("  mobile/{sub}: {s}"));
            if sub == "thumbs" {
                report.thumbs = s;
            } else {
                report.previews = s;
            }
        }
        // (6) the manifest, last
        self.logln("step 6/6: manifest");
        self.rp.stage(Stage::Publishing, 0, 0);
        report.manifest_uploaded = self.publish_manifest(st, &photos)?;
        report.photos = photos.len();
        st.set("last_run", &iso(self.now()));
        st.set("last_result", &if report.failed.is_empty() { "ok".to_string() } else { format!("ok, {} photos failed", report.failed.len()) });
        st.set(
            "last_counts",
            &json!({"new": plan.new.len(), "changed": plan.changed.len(), "sc_changed": plan.sc_changed.len(), "removed": plan.removed.len(), "retry": plan.retry.len(), "photos": photos.len()}).to_string(),
        );
        Ok(report)
    }

    /// Import, smart-preview and thumbnail what the plan lists; update the state rows. Returns the paths that failed.
    fn refresh_library(&self, st: &mut State, plan: &Plan, scan: &Scan, renderer: &mut dyn Renderer) -> Result<(Vec<String>, Vec<String>)> {
        let cfg = &self.cfg;
        let known = st.files().clone();
        let src = &cfg.source;
        let todo_import: Vec<&String> = plan.new.iter().chain(&plan.changed).chain(&plan.retry).collect();
        // drop library entries for changed/removed photos
        let drop: Vec<PathBuf> =
            plan.changed.iter().chain(&plan.removed).chain(&plan.retry).filter(|p| known.contains_key(*p)).map(|p| src.join(p)).collect();
        if !drop.is_empty()
            && let Err(e) = renderer.forget(&drop)
        {
            self.logln(format!("WARNING: could not drop {} photos from the scratch library: {e}", drop.len()));
        }
        for p in &plan.removed {
            st.delete(p);
        }
        let mobile = self.mobile();
        let (thumbs, previews) = (mobile.join("thumbs"), mobile.join("previews"));
        for d in [&thumbs, &previews] {
            std::fs::create_dir_all(d).map_err(|e| Error::io(d.display(), e))?;
        }
        let mut infos: BTreeMap<String, PhotoInfo> = BTreeMap::new();
        let mut dups: BTreeSet<&String> = BTreeSet::new();
        let mut added: Vec<&String> = Vec::new();
        let mut busy: Vec<String> = Vec::new();
        let rereads = plan.sc_changed.iter().filter(|p| known.get(*p).is_some_and(|r| r.status == "ok")).count();
        self.rp.stage(Stage::Reading, (todo_import.len() + rereads) as u64, 0);
        for (n, &p) in todo_import.iter().enumerate() {
            self.rp.at(n as u64);
            // A photo another program has open without sharing (Windows: LightCraft importing it, a scanner) cannot be read now. It is
            // not a failure: nothing is recorded for it, and the next sync looks again.
            if let Err(e) = std::fs::File::open(src.join(p))
                && is_busy(&e)
            {
                busy.push(p.clone());
                continue;
            }
            match renderer.add(&src.join(p)) {
                Ok(Added::Photo(i)) => {
                    infos.insert(p.clone(), i);
                    added.push(p);
                }
                Ok(Added::Duplicate) => {
                    dups.insert(p);
                }
                Err(e) => self.logln(format!("WARNING: import failed: {p}: {e}")),
            }
        }
        // sidecar changed: read it again
        let mut reread_n = 0;
        for p in &plan.sc_changed {
            if known.get(p).is_some_and(|r| r.status == "ok") {
                self.rp.at((todo_import.len() + reread_n) as u64);
                reread_n += 1;
                match renderer.reread(&src.join(p)) {
                    Ok(i) => {
                        infos.insert(p.clone(), i);
                    }
                    Err(e) => self.logln(format!("WARNING: reading the sidecar of {p} failed: {e}")),
                }
            }
        }
        self.rp.finish();
        // smart previews for newly imported photos
        let mut keys: HashMap<String, String> = HashMap::new();
        self.rp.stage(Stage::Previews, added.len() as u64, 0);
        for (n, p) in added.into_iter().enumerate() {
            self.rp.at(n as u64);
            let h = &infos[p].content_hash;
            let tmp = previews.join(format!(".tmp-{h}.lcsp"));
            match renderer.smart_preview(&src.join(p), &tmp) {
                Ok(key) => {
                    let dest = previews.join(format!("{h}.lcsp"));
                    std::fs::rename(&tmp, &dest).map_err(|e| Error::io(dest.display(), e))?;
                    keys.insert(p.clone(), key);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    self.logln(format!("WARNING: could not make the smart preview of {p}: {e}"));
                }
            }
        }
        self.rp.finish();
        // thumbnails: new + sidecar-changed, rendered with the current edits
        self.rp.stage(Stage::Thumbnails, infos.len() as u64, 0);
        for (n, (p, i)) in infos.iter().enumerate() {
            self.rp.at(n as u64);
            let h = &i.content_hash;
            let tmp = thumbs.join(format!(".tmp-{h}.jpg"));
            match renderer.thumbnail(&src.join(p), cfg.thumb_px, cfg.thumb_quality, &tmp) {
                Ok(()) if tmp.exists() => {
                    let dest = thumbs.join(format!("{h}.jpg"));
                    std::fs::rename(&tmp, &dest).map_err(|e| Error::io(dest.display(), e))?;
                }
                Ok(()) => self.logln(format!("WARNING: no thumbnail came out for {p}")),
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    self.logln(format!("WARNING: thumbnail of {p} failed: {e}"));
                }
            }
        }
        self.rp.finish();
        if let Err(e) = renderer.finish() {
            self.logln(format!("WARNING: could not save the scratch library: {e}"));
        }
        // update state rows
        let mut failed = Vec::new();
        for p in todo_import.iter().copied().chain(&plan.sc_changed) {
            if busy.contains(p) {
                continue;
            }
            let e = &scan.entries[p];
            let mut row = Row {
                size: e.size,
                mtime_ns: e.mtime_ns,
                sidecar: e.sidecar.clone(),
                sidecar_hash: e.sidecar_hash.clone(),
                status: "ok".into(),
                ..Row::default()
            };
            let was = known.get(p);
            if let Some(i) = infos.get(p) {
                let preview_key =
                    keys.get(p).cloned().or_else(|| if plan.sc_changed.contains(p) { was.and_then(|w| w.meta.preview_key.clone()) } else { None });
                row.id = Some(i.content_hash.clone());
                row.meta = Meta {
                    captured: i.captured.clone(),
                    camera: i.camera.clone(),
                    lens: i.lens.clone(),
                    width: i.width,
                    height: i.height,
                    rating: i.rating,
                    flag: i.flag.clone(),
                    label: i.label.clone(),
                    preview_key,
                };
            } else if dups.contains(p) || (plan.sc_changed.contains(p) && was.is_some_and(|w| w.status == "dup")) {
                row.status = "dup".into();
                row.note = Some("duplicate of a photo already in the library".into());
                row.id = None;
            } else if plan.sc_changed.contains(p) && was.is_some_and(|w| w.status != "ok") {
                row.status = was.map(|w| w.status.clone()).unwrap_or_default();
                row.note = was.and_then(|w| w.note.clone());
            } else {
                row.status = "failed".into();
                row.note = Some(if plan.sc_changed.contains(p) { "inspect failed" } else { "import failed" }.into());
                failed.push(p.clone());
            }
            st.put(p, row);
        }
        st.commit()?;
        Ok((failed, busy))
    }

    fn mirror_originals(&self, st: &mut State, scan: &Scan, date: &str, suffix: &str, dry: bool) -> Result<MirrorStats> {
        let cfg = &self.cfg;
        let local: BTreeMap<&str, &FileInfo> = scan
            .files
            .iter()
            .filter(|(r, _)| !scan.unstable.contains(*r) && (!cfg.previews_only || ext_lower(r) == "xmp"))
            .map(|(r, f)| (r.as_str(), f))
            .collect();
        let removable = |rel: &str| in_scope(rel, &cfg.exclude) && !scan.unstable.contains(rel) && (!cfg.previews_only || ext_lower(rel) == "xmp");
        self.mirror(st, "originals", "originals", &local, &removable, date, suffix, dry)
    }

    fn mirror_staged(&self, st: &mut State, sub: &str, date: &str, suffix: &str, dry: bool) -> Result<MirrorStats> {
        let dir = self.mobile().join(sub);
        let files = sha_dir_files(&dir);
        let local: BTreeMap<&str, &FileInfo> = files.iter().map(|(r, f)| (r.as_str(), f)).collect();
        self.mirror(st, &format!("mobile/{sub}"), &format!("mobile/{sub}"), &local, &|rel| in_scope(rel, &[]), date, suffix, dry)
    }

    /// One-way mirror of `local` (relative path -> file) into `prefix/` on the remote, like `rclone sync --backup-dir`:
    /// a changed file replaces the remote copy, a file gone from `local` (and `removable`) is removed, and every copy that
    /// is replaced or removed first moves to `history/<date>/<backup_sub>/` with a `.HHMMSS` suffix.
    ///
    /// A file is unchanged when the remote has the same size and the state remembers uploading this size and mtime (or
    /// remembers nothing: a library published by another tool is trusted when the sizes match). More removals than
    /// `max_delete` abort the run after the uploads, removing nothing.
    #[allow(clippy::too_many_arguments)]
    fn mirror(
        &self,
        st: &mut State,
        prefix: &str,
        backup_sub: &str,
        local: &BTreeMap<&str, &FileInfo>,
        removable: &dyn Fn(&str) -> bool,
        date: &str,
        suffix: &str,
        dry: bool,
    ) -> Result<MirrorStats> {
        if !dry {
            self.rp.stage(Stage::LookingAtLibrary, 0, 0);
        }
        let remote: BTreeMap<String, u64> = match self.remote.list(prefix, true) {
            Ok(v) => v.into_iter().filter(|e| !e.is_dir).map(|e| (e.path, e.size)).collect(),
            Err(Error::NotFound(_)) => BTreeMap::new(),
            Err(e) => return Err(e),
        };
        let mut stats = MirrorStats::default();
        let (mut new, mut replace): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
        for (&rel, f) in local {
            let key = format!("{prefix}/{rel}");
            match remote.get(rel) {
                None => new.push(rel),
                Some(&sz) if sz != f.size => replace.push(rel),
                Some(_) => match st.uploaded(&key).cloned() {
                    Some(rec) if rec.size == f.size && rec.mtime_ns == f.mtime_ns => stats.unchanged += 1,
                    // the mtime moved: like rclone (same size, same checksum: only the time differs), the content decides
                    Some(rec) if rec.size == f.size && rec.sha256.is_some() && rec.sha256 == file_sha(&f.abs) => {
                        stats.unchanged += 1;
                        if !dry {
                            st.set_uploaded(&key, Uploaded { mtime_ns: f.mtime_ns, ..rec });
                        }
                    }
                    Some(_) => replace.push(rel),
                    None => {
                        stats.unchanged += 1;
                        if !dry {
                            st.set_uploaded(&key, Uploaded { size: f.size, mtime_ns: f.mtime_ns, sha256: None });
                        }
                    }
                },
            }
        }
        let extra: Vec<&String> = remote.keys().filter(|r| !local.contains_key(r.as_str()) && removable(r)).collect();
        stats.copied_new = new.len();
        stats.replaced = replace.len();
        stats.removed = extra.len();
        if dry {
            return Ok(stats);
        }
        // uploads (a replaced copy moves to history first)
        let jobs: Vec<(&str, bool)> = new.iter().map(|r| (*r, false)).chain(replace.iter().map(|r| (*r, true))).collect();
        if !jobs.is_empty() {
            let stage = match prefix {
                "originals" => Stage::UploadingOriginals,
                "mobile/thumbs" => Stage::UploadingThumbnails,
                _ => Stage::UploadingPreviews,
            };
            self.rp.stage(stage, jobs.len() as u64, jobs.iter().map(|(r, _)| local[r].size).sum());
        }
        let results = self.rp.watch(&*self.remote, || parallel(&jobs, JOBS, |(rel, replacing)| -> Result<()> {
            let sent = (|| {
                if *replacing {
                    let backup = format!("history/{date}/{backup_sub}/{}", with_suffix(rel, suffix));
                    self.retry(|| self.remote.move_file(&format!("{prefix}/{rel}"), &backup))?;
                }
                self.retry(|| self.remote.put(&local[rel].abs, &format!("{prefix}/{rel}")))
            })();
            // (a file that failed is counted too: the stage ends when every upload has been tried)
            self.rp.item_done(local[rel].size);
            sent
        }));
        let mut first_err = None;
        for ((rel, _), r) in jobs.iter().zip(results) {
            match r {
                Ok(()) => st.set_uploaded(
                    &format!("{prefix}/{rel}"),
                    Uploaded { size: local[rel].size, mtime_ns: local[rel].mtime_ns, sha256: file_sha(&local[rel].abs) },
                ),
                Err(e) => {
                    st.forget_uploaded(&format!("{prefix}/{rel}"));
                    self.logln(format!("  upload of {prefix}/{rel} failed: {e}"));
                    first_err.get_or_insert(e);
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        if extra.len() > self.cfg.max_delete {
            return refuse(format!(
                "{} files under {prefix}/ are gone from the source (max_delete={}); nothing was removed. Check the drive, or raise max_delete in the config.",
                extra.len(),
                self.cfg.max_delete
            ));
        }
        for rel in extra {
            let backup = format!("history/{date}/{backup_sub}/{}", with_suffix(rel, suffix));
            self.retry(|| self.remote.move_file(&format!("{prefix}/{rel}"), &backup))?;
            st.forget_uploaded(&format!("{prefix}/{rel}"));
        }
        Ok(stats)
    }

    /// Upload `mobile/manifest.json` unless the remote in use already has this exact photo list. Returns whether it was
    /// uploaded. The local copy is what the remote has: replaced only after the upload worked, and the remote it went to is
    /// recorded, so a failed upload or a changed remote is retried instead of skipped.
    fn publish_manifest(&self, st: &mut State, photos: &[ManifestPhoto]) -> Result<bool> {
        let mpath = self.mobile().join("manifest.json");
        let new_photos = serde_json::to_value(photos).map_err(|e| Error::Format(e.to_string()))?;
        let old: Option<Value> = std::fs::read(&mpath).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let sent_to = st.get("manifest_uploaded").map(str::to_string);
        if let Some(old) = &old
            && old.get("photos") == Some(&new_photos)
            && old.get("format").and_then(Value::as_u64) == Some(FORMAT)
            && sent_to.as_deref() == Some(self.cfg.remote_label.as_str())
        {
            self.logln("  manifest unchanged; not uploading");
            return Ok(false);
        }
        // (a struct, so the fields keep the order the Python tool writes them in)
        #[derive(serde::Serialize)]
        struct Doc<'a> {
            format: u64,
            generated: String,
            publisher: &'a str,
            photos: &'a [ManifestPhoto],
        }
        let doc = Doc { format: FORMAT, generated: iso(self.now()), publisher: &self.cfg.publisher, photos };
        let mut bytes = Vec::new();
        {
            use serde::Serialize;
            let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
            let mut ser = serde_json::Serializer::with_formatter(&mut bytes, fmt);
            doc.serialize(&mut ser).map_err(|e| Error::Format(e.to_string()))?;
        }
        let staged = mpath.with_file_name("manifest.json.new");
        write_atomic(&staged, &bytes)?;
        let tmp = format!("mobile/manifest.json.tmp-{:08x}", rand_u32());
        self.retry(|| self.remote.put(&staged, &tmp))?;
        self.retry(|| self.remote.move_file(&tmp, "mobile/manifest.json"))?;
        std::fs::rename(&staged, &mpath).map_err(|e| Error::io(mpath.display(), e))?;
        st.set("manifest_uploaded", &self.cfg.remote_label);
        self.logln(format!("  manifest uploaded ({} photos)", photos.len()));
        Ok(true)
    }

    /// Last run, tracked photos, remote size, pending inbox files. Reads only.
    pub fn status(&self) -> Result<StatusReport> {
        let st = State::open(&self.cfg.state_dir, true)?;
        let mut r = StatusReport {
            last_run: st.get("last_run").map(str::to_string),
            last_result: st.get("last_result").map(str::to_string),
            last_counts: st.get("last_counts").map(str::to_string),
            tracked: st.files().len(),
            not_published: st.files().values().filter(|r| r.status != "ok").count(),
            ..StatusReport::default()
        };
        match self.remote.list("", true) {
            Ok(v) => r.remote = Some((v.iter().filter(|e| !e.is_dir).count(), v.iter().map(|e| e.size).sum())),
            Err(Error::NotFound(_)) => r.remote = Some((0, 0)),
            Err(e) => r.remote_error = Some(e.to_string()),
        }
        r.inbox_pending = self.inbox_pending().ok().map(|v| v.len());
        r.imports_pending = self.imports_waiting().ok();
        Ok(r)
    }
}

/// SHA-256 of a file's bytes, read in chunks.
fn file_sha(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn staged_sizes(dir: &Path, ext: &str) -> HashMap<String, u64> {
    sha_dir_files(dir)
        .into_iter()
        .filter_map(|(name, f)| {
            let (stem, e) = name.rsplit_once('.')?;
            (e == ext).then(|| (stem.to_string(), f.size))
        })
        .collect()
}

fn rand_u32() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new().build_hasher().finish() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names_keep_the_extension() {
        assert_eq!(with_suffix("A/photo.jpg", ".153012"), "A/photo.153012.jpg");
        assert_eq!(with_suffix("a.b.xmp", ".1"), "a.b.1.xmp");
        assert_eq!(with_suffix("noext", ".1"), "noext.1");
    }
}
