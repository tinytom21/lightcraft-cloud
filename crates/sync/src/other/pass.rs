//! One pass of the other PC (see the module docs of [`super`]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::import::{ImportQueue, OutEntry, OutResult};
use super::state::{Rec, State};
use super::{Other, OtherReport, OtherStatus, fetch_manifest, folder_of};
use crate::home::{RunLock, content_hash_file, count_shots, find_sidecars, stem};
use crate::manifest::Item;
use crate::progress::Stage;
use crate::util::{free_space, is_busy, iso, link_no_replace, parse_iso, parse_stamp, sha256_hex, sidecar_version, replace_file, sync_dir, temp_beside, utc_stamp, write_atomic};
use crate::{Error, Result};

/// The inbox rejects bigger sidecars.
const MAX_SIDECAR: u64 = 2 * 1024 * 1024;
/// Photos staged (downloaded, checked, not yet in the folder) before they are put in place, so a cut-off keeps what arrived and the
/// state is written once per batch rather than once per photo.
const BATCH_FILES: usize = 20;
const BATCH_BYTES: u64 = 512 << 20;
/// Leftover `.part` files older than this are removed.
const STALE_PART: Duration = Duration::from_secs(86_400);

fn check_source(root: &Path) -> Result<()> {
    if !root.exists() {
        return Err(Error::Invalid(format!("The photo folder {} does not exist (drive not mounted?).", root.display())));
    }
    if !root.is_dir() {
        return Err(Error::Invalid(format!("The photo folder {} is not a folder.", root.display())));
    }
    Ok(())
}

/// Whether a manifest path may be written below the photo folder: relative, `/`-separated, no `.` or `..`, nothing the file system treats
/// specially. On Windows also the characters and names it refuses.
pub(super) fn safe_rel(rel: &str) -> bool {
    safe_rel_on(rel, cfg!(windows))
}

/// [`safe_rel`] for a given system (a test checks the Windows rules on every system).
pub(super) fn safe_rel_on(rel: &str, windows: bool) -> bool {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6",
        "LPT7", "LPT8", "LPT9",
    ];
    !rel.is_empty()
        && rel.split('/').all(|c| {
            let basic = !c.is_empty() && c != "." && c != ".." && !c.contains('\\') && !c.chars().any(char::is_control);
            let windows = !windows
                || (!c.contains(['<', '>', ':', '"', '|', '?', '*'])
                    && !c.ends_with(['.', ' '])
                    && !RESERVED.contains(&c.split('.').next().unwrap_or("").to_ascii_uppercase().as_str()));
            basic && windows
        })
}

pub(super) fn push_unique(v: &mut Vec<String>, line: String) {
    if !v.contains(&line) {
        v.push(line);
    }
}

fn basename(rel: &str) -> &str {
    rel.rsplit_once('/').map_or(rel, |(_, n)| n)
}

fn join_rel(folder: &str, name: &str) -> String {
    if folder.is_empty() { name.to_string() } else { format!("{folder}/{name}") }
}

/// `dir/name.HHMMSS.ext`: where a replaced sidecar goes in the history.
fn with_time(rel: &str, hhmmss: &str) -> String {
    let (dir, name) = rel.rsplit_once('/').map_or(("", rel), |(d, n)| (d, n));
    let named = match name.rfind('.') {
        Some(i) if i > 0 => format!("{}.{hhmmss}{}", &name[..i], &name[i..]),
        _ => format!("{name}.{hhmmss}"),
    };
    join_rel(dir, &named)
}

/// A photo that is not here yet, downloaded and checked, waiting for its turn to be put in the folder.
struct Staged<'a> {
    item: &'a Item,
    tmp: PathBuf,
    target: PathBuf,
    size: u64,
    sidecar: Option<StagedSidecar>,
}

struct StagedSidecar {
    /// The downloaded file (None: a sidecar was already there and stays).
    tmp: Option<PathBuf>,
    dest: PathBuf,
    rel: String,
    sha: String,
}

enum New {
    Download,
    Skip,
    Adopt,
}

type Siblings<'a> = HashMap<&'a str, Vec<String>>;
type SidecarsByFolder = HashMap<String, HashMap<String, Option<String>>>;

impl Other {
    /// One pass. Fails as a whole when the manifest cannot be fetched, is of a newer format, or the network drops (what had arrived
    /// stays, and the next pass goes on from there); every other trouble is a line in `problems`.
    pub fn sync(&self) -> Result<OtherReport> {
        check_source(&self.cfg.source)?;
        let _lock = RunLock::acquire(&self.cfg.state_dir, false)?;
        let mut st = State::open(&self.cfg.state_dir)?;
        let mut rep = OtherReport::default();
        let r = self.pass(&mut st, &mut rep);
        // (local work only: a PC that is offline still notices what was imported)
        self.rp.stage(Stage::LookingForNewPhotos, 0, 0);
        self.pickup(&st, &mut rep);
        if r.is_ok() {
            st.last_pass = Some(iso(self.now()));
        }
        // (what was done stays recorded even when a later step failed)
        let saved = st.save();
        r?;
        saved?;
        Ok(rep)
    }

    /// The sidecar beside the photo here, for a photo that is called something else at the home PC.
    fn local_sidecar_rel(&self, rec: &Rec, item: &Item) -> Option<String> {
        if let Some(s) = &rec.sidecar {
            return Some(s.clone());
        }
        let folder = folder_of(&rec.path);
        let name = basename(&rec.path);
        let candidates = [format!("{}.xmp", stem(name)), format!("{name}.xmp")];
        if let Some(c) = candidates.iter().find(|c| self.target(&join_rel(folder, c)).is_some_and(|p| p.is_file())) {
            return Some(join_rel(folder, c));
        }
        let home_sidecar = basename(item.sidecar.as_deref()?);
        let by_stem = home_sidecar.to_lowercase() == format!("{}.xmp", stem(basename(&item.path)).to_lowercase());
        Some(join_rel(folder, &if by_stem { candidates[0].clone() } else { candidates[1].clone() }))
    }

    /// Whether the sidecar next to an imported photo that the home PC does not have yet differs from the one that was sent.
    fn ledger_edit_waiting(&self, e: &OutEntry) -> bool {
        let folder = e.folder.as_str();
        let candidates = [e.sidecar.clone(), Some(format!("{}.xmp", stem(&e.name))), Some(format!("{}.xmp", e.name))];
        let local = candidates
            .into_iter()
            .flatten()
            .find_map(|c| self.target(&join_rel(folder, &c)).and_then(|p| std::fs::read(p).ok()));
        match (local, &e.sidecar_sha) {
            (Some(b), Some(sha)) => &sha256_hex(&b) != sha,
            (Some(_), None) => true,
            _ => false,
        }
    }

    /// The numbers for the status window, from the state and the local sidecars alone (no network).
    pub fn status(&self) -> Result<OtherStatus> {
        let st = State::open(&self.cfg.state_dir)?;
        let queue = ImportQueue::new(&self.cfg.state_dir);
        let mut s = OtherStatus { waiting_to_download: st.waiting, last_pass: st.last_pass.clone(), queue: queue.status(), ..OtherStatus::default() };
        // (the records are per file; the photos shown are shots, the way the home PC counts its manifest: a raw and a JPEG of one shot are one)
        let mut here: Vec<String> = Vec::new();
        for e in queue.list().iter().filter(|e| !st.photos.contains_key(&e.id) && !e.result.as_ref().is_some_and(OutResult::is_rejected)) {
            if self.target(&e.path()).is_some_and(|p| p.is_file()) {
                here.push(e.path());
                if self.ledger_edit_waiting(e) {
                    s.edits_waiting += 1;
                }
            }
        }
        for r in st.photos.values().filter(|r| !r.gone) {
            here.push(r.path.clone());
            let Some(bytes) = r.sidecar.as_deref().and_then(|rel| self.target(rel)).and_then(|p| std::fs::read(p).ok()) else { continue };
            let sha = sha256_hex(&bytes);
            if r.synced.as_deref() != Some(&sha) && r.manifest.as_deref() != Some(&sha[..16]) {
                s.edits_waiting += 1;
            }
        }
        s.photos = count_shots(here.iter().map(String::as_str));
        Ok(s)
    }

    /// The absolute path of a manifest path below the photo folder, if it is one that may be written.
    pub(super) fn target(&self, rel: &str) -> Option<PathBuf> {
        if !safe_rel(rel) {
            return None;
        }
        let mut p = self.cfg.source.clone();
        p.extend(rel.split('/'));
        Some(p)
    }

    pub(super) fn retrier(&self) -> crate::imports::Retrier {
        crate::imports::Retrier { delay: self.retry_delay }
    }

    pub(super) fn retry<T>(&self, f: impl FnMut() -> Result<T>) -> Result<T> {
        self.retrier().run(f)
    }

    pub(super) fn get(&self, remote_path: &str, local: &Path) -> Result<()> {
        self.retry(|| self.remote.get(remote_path, local))
    }

    /// A device name another device already sends edits under is refused the first time this PC uses it.
    fn claim_device(&self, st: &mut State) -> Result<()> {
        if st.device.as_deref() == Some(&self.cfg.device) {
            return Ok(());
        }
        let taken = match self.retry(|| self.remote.list("inbox", false)) {
            Ok(v) => v.iter().any(|e| e.is_dir && e.path == self.cfg.device),
            Err(Error::NotFound(_)) => false,
            Err(e) => return Err(e),
        };
        if taken {
            return Err(Error::Invalid(format!(
                "The name \"{0}\" is already used by another device that sends edits to this cloud library (a phone, or another PC). \
                 Choose another name for this PC in Settings.",
                self.cfg.device
            )));
        }
        st.device = Some(self.cfg.device.clone());
        Ok(())
    }

    fn pass(&self, st: &mut State, rep: &mut OtherReport) -> Result<()> {
        let tmp = self.cfg.state_dir.join("manifest.json.tmp");
        self.rp.stage(Stage::LookingAtLibrary, 0, 0);
        let parsed = self.retry(|| fetch_manifest(&*self.remote, &tmp))?;
        self.claim_device(st)?;
        // every id the library ever had, for good: what was deleted there is never sent back from here
        super::pickup::record_seen(&self.cfg.state_dir, parsed.items.iter().map(|i| i.id.as_str()))?;
        let chosen: BTreeSet<&str> = self.cfg.folders.iter().map(String::as_str).collect();
        // a folder that is no longer chosen is no longer looked after: its photos stay, and so is forgotten what was remembered of them
        st.photos.retain(|_, r| chosen.contains(folder_of(&r.path)));
        // the photos this PC imported itself and sent: matched by id, wherever the home PC put them (the file here stays where it is)
        let ledger: HashMap<String, OutEntry> = ImportQueue::new(&self.cfg.state_dir)
            .list()
            .into_iter()
            .filter(|e| !e.result.as_ref().is_some_and(OutResult::is_rejected))
            .filter(|e| self.target(&e.path()).is_some_and(|p| p.is_file()))
            .map(|e| (e.id.clone(), e))
            .collect();
        // files found here (by their id) that the library has at another path
        let found = super::pickup::local_files_by_id(&self.cfg.state_dir, &self.cfg.source, &self.cfg.folders);
        let items: Vec<&Item> = parsed
            .items
            .iter()
            .filter(|i| {
                chosen.contains(folder_of(&i.path)) || ledger.contains_key(&i.id) || found.contains_key(&i.id) || st.photos.get(&i.id).is_some_and(|r| r.imported)
            })
            .collect();
        let by_id: HashMap<&str, &Item> = items.iter().map(|i| (i.id.as_str(), *i)).collect();
        // (which shot of the manifest each file belongs to: the counts shown are shots, as on the home PC)
        let shot_of: HashMap<&str, usize> = parsed.items.iter().zip(&parsed.shots).map(|(i, s)| (i.id.as_str(), *s)).collect();
        self.remove_vanished(st, &by_id, rep);

        let mut siblings: Siblings = HashMap::new();
        for i in &items {
            siblings.entry(folder_of(&i.path)).or_default().push(i.path.clone());
        }
        let mut sidecars = SidecarsByFolder::new();
        let mut todo: Vec<&Item> = Vec::new();
        self.rp.stage(Stage::CheckingEdits, items.len() as u64, 0);
        for (n, item) in items.iter().enumerate() {
            self.rp.at(n as u64);
            let id = item.id.as_str();
            if !st.photos.contains_key(id)
                && let Some(e) = ledger.get(id)
            {
                // The home PC has the photo this PC imported. The file here stays where it is. What the cloud library has of the sidecar is
                // what was sent (`synced`): a sidecar edited since is an edit, sent now with the home PC's version as its base.
                st.photos.insert(
                    id.to_string(),
                    Rec { path: e.path(), sidecar: e.sidecar_path(), synced: e.sidecar_sha.clone(), base: item.version.clone(), imported: true, ..Rec::default() },
                );
                self.logln(format!("{} is in the cloud library now (at {}); it stays where it is on this PC", e.path(), item.path));
            }
            let mut adopted_local = false;
            if !st.photos.contains_key(id)
                && !ledger.contains_key(id)
                && let Some(rel) = found.get(id)
            {
                // The same bytes are here already, under another name or in another folder than the library has them: this is that
                // photo. No second copy is downloaded.
                st.photos.insert(id.to_string(), Rec { path: rel.clone(), imported: true, ..Rec::default() });
                adopted_local = true;
                self.logln(format!("{rel} is the library's {} (same photo); no second copy is downloaded", item.path));
            }
            let adopted = match st.photos.get(id) {
                None => match self.classify_new(item, rep) {
                    New::Download => {
                        todo.push(item);
                        continue;
                    }
                    New::Skip => continue,
                    New::Adopt => true,
                },
                Some(r) if r.gone => {
                    rep.left_out += 1;
                    continue;
                }
                Some(_) => adopted_local,
            };
            if adopted {
                st.photos.insert(id.to_string(), Rec { path: item.path.clone(), ..Rec::default() });
            }
            let Some(rec) = st.photos.get_mut(id) else { continue };
            let Some(abs) = self.target(&rec.path) else {
                push_unique(&mut rep.problems, format!("{}: this name cannot be used on this PC", rec.path));
                continue;
            };
            if rec.placing {
                // an earlier pass put the file in place and could not tell LightCraft, or was cut off before it finished
                if !abs.exists() {
                    todo.push(item);
                    continue;
                }
                match self.announce(&abs) {
                    Ok(()) => rec.placing = false,
                    Err(e) => self.logln(format!("LightCraft is not told about {} yet: {e}", rec.path)),
                }
            } else if !adopted && !abs.exists() {
                rec.gone = true;
                rep.left_out += 1;
                self.logln(format!("{} was deleted on this PC: it is not downloaded again", rec.path));
                continue;
            }
            let folder = folder_of(&rec.path).to_string();
            let sc_rel = if rec.path != item.path {
                // (named differently here and at the home PC: the sidecar here is the one beside the file here)
                self.local_sidecar_rel(rec, item)
            } else {
                match &item.sidecar {
                    Some(s) => Some(join_rel(&folder, basename(s))),
                    None => self.existing_sidecar(&folder, &rec.path, &siblings, &mut sidecars),
                }
            };
            if adopted {
                self.adopt_sidecar(rec, item, sc_rel.as_deref());
            }
            match self.sync_sidecar(rec, item, sc_rel, rep) {
                Ok(true) => {
                    if let Err(e) = st.save() {
                        self.logln(format!("state not saved: {e}"));
                    }
                }
                Ok(false) => {}
                Err(e @ Error::Remote(_)) => return Err(e),
                Err(e) => push_unique(&mut rep.problems, format!("{}: {e}", item.path)),
            }
        }
        self.rp.finish();
        // edits to imported photos the home PC does not have yet wait here (they are not sent early)
        for e in ledger.values().filter(|e| !st.photos.contains_key(&e.id)) {
            if self.ledger_edit_waiting(e) {
                rep.edits_waiting += 1;
            }
        }
        rep.queue = ImportQueue::new(&self.cfg.state_dir).status();
        let has_file = |st: &State| -> HashSet<usize> {
            items.iter().filter(|i| st.photos.get(&i.id).is_some_and(|r| !r.gone)).filter_map(|i| shot_of.get(i.id.as_str()).copied()).collect()
        };
        let before = has_file(st);
        let todo_ids: Vec<(&str, &str)> = todo.iter().map(|i| (i.id.as_str(), i.path.as_str())).collect();
        let download = self.rp.watch(&*self.remote, || self.download(st, todo, rep));
        // photos = shots with a file here; waiting = shots with no file here yet; arrived = shots that had none before this pass
        rep.photos = count_shots(items.iter().filter_map(|i| st.photos.get(&i.id).filter(|r| !r.gone).map(|r| r.path.as_str())));
        let now_here = has_file(st);
        let all: HashSet<usize> = items.iter().filter_map(|i| shot_of.get(i.id.as_str()).copied()).collect();
        let started_here: HashSet<usize> =
            items.iter().filter(|i| st.photos.contains_key(&i.id)).filter_map(|i| shot_of.get(i.id.as_str()).copied()).collect();
        rep.waiting_to_download = all.difference(&started_here).count();
        rep.downloaded_photos = todo_ids
            .iter()
            .filter(|(_, path)| rep.downloaded.iter().any(|d| d == path))
            .filter_map(|(id, _)| shot_of.get(id).copied())
            .filter(|s| !before.contains(s) && now_here.contains(s))
            .collect::<HashSet<_>>()
            .len();
        // the shots this PC holds that came from the home PC (not imported here), for the "received" count's start
        rep.from_home_photos = count_shots(st.photos.values().filter(|r| !r.imported).map(|r| r.path.as_str()));
        st.waiting = rep.waiting_to_download;
        download
    }

    /// LightCraft is told (through the app's hook) that a photo is in the folder.
    pub(super) fn announce(&self, abs: &Path) -> std::result::Result<(), String> {
        match &self.adopted_hook {
            Some(h) => h(abs),
            None => Ok(()),
        }
    }

    /// What to do with a manifest photo this PC has no record of.
    fn classify_new(&self, item: &Item, rep: &mut OtherReport) -> New {
        let Some(target) = self.target(&item.path) else {
            push_unique(&mut rep.problems, format!("{}: this name cannot be used on this PC; skipped", item.path));
            return New::Skip;
        };
        let md = match std::fs::symlink_metadata(&target) {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return New::Download,
            Err(e) => {
                push_unique(&mut rep.problems, format!("{}: {e}", item.path));
                return New::Skip;
            }
        };
        let differs = format!("{} is already on this PC with different contents; the copy in the cloud library was not downloaded, so your file stays.", item.path);
        if !md.is_file() || item.size.is_some_and(|s| s != md.len()) {
            push_unique(&mut rep.problems, differs);
            return New::Skip;
        }
        match content_hash_file(&target) {
            Ok((h, _)) if h == item.id => New::Adopt,
            Ok(_) => {
                push_unique(&mut rep.problems, differs);
                New::Skip
            }
            Err(e) => {
                push_unique(&mut rep.problems, format!("{}: {e}", item.path));
                New::Skip
            }
        }
    }

    /// A photo that was already here, with the same bytes: a sidecar that is here too is taken as in step (this PC's file is not an
    /// edit to send, and the cloud's is not fetched over it); the next change on either side is handled as usual.
    fn adopt_sidecar(&self, rec: &mut Rec, item: &Item, sc_rel: Option<&str>) {
        let Some(bytes) = sc_rel.and_then(|r| self.target(r)).and_then(|p| std::fs::read(p).ok()) else { return };
        let sha = sha256_hex(&bytes);
        if item.version.as_deref().is_some_and(|v| v != &sha[..16]) {
            self.logln(format!("{}: the sidecar on this PC differs from the cloud's; it is left as it is", rec.path));
        }
        rec.manifest = item.version.clone();
        rec.base = item.version.clone();
        rec.synced = Some(sha);
    }

    /// The sidecar a photo that has none in the cloud has here, if LightCraft wrote one (the engine's naming rule).
    fn existing_sidecar(&self, folder: &str, path: &str, siblings: &Siblings, cache: &mut SidecarsByFolder) -> Option<String> {
        let map = cache.entry(folder.to_string()).or_insert_with(|| {
            let mut files = BTreeSet::new();
            let dir = self.target(folder).unwrap_or_else(|| self.cfg.source.clone());
            let dir = if folder.is_empty() { self.cfg.source.clone() } else { dir };
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    if let Some(n) = e.file_name().to_str() {
                        files.insert(join_rel(folder, n));
                    }
                }
            }
            let photos = siblings.get(folder).cloned().unwrap_or_default();
            find_sidecars(&photos, &files).0
        });
        map.get(path).cloned().flatten()
    }

    /// The sidecar of one photo that is here. `Ok(true)`: an edit was sent (the caller saves the state).
    fn sync_sidecar(&self, rec: &mut Rec, item: &Item, sc_rel: Option<String>, rep: &mut OtherReport) -> Result<bool> {
        let remote_ver = item.version.clone().filter(|_| item.sidecar.is_some());
        rec.sidecar = sc_rel.clone().or(rec.sidecar.take());
        let sc_abs = sc_rel.as_deref().and_then(|r| self.target(r));
        let local: Option<Vec<u8>> = match &sc_abs {
            Some(p) => match std::fs::read(p) {
                Ok(b) => Some(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    push_unique(&mut rep.problems, format!("{}: {e}", p.display()));
                    return Ok(false);
                }
            },
            None => None,
        };
        let sha = local.as_ref().map(|b| sha256_hex(b));
        // the same as the cloud's: in step (the home PC took an edit as it was, or this PC just got it)
        if let (Some(s), Some(v)) = (&sha, &remote_ver)
            && s[..16] == *v
        {
            rec.manifest = Some(v.clone());
            rec.base = Some(v.clone());
            rec.synced = Some(s.clone());
            rec.awaiting = None;
            return Ok(false);
        }
        // a local change not sent yet goes first, and the local file is not touched this pass
        if let (Some(bytes), Some(s)) = (&local, &sha)
            && rec.synced.as_deref() != Some(s.as_str())
        {
            if bytes.len() as u64 >= MAX_SIDECAR {
                rep.edits_waiting += 1;
                push_unique(&mut rep.problems, format!("The sidecar of {} is over 2 MB; it was not sent to the home PC.", item.path));
                return Ok(false);
            }
            return match self.send(rec, item, bytes, s, remote_ver.as_deref()) {
                Ok(()) => {
                    rep.edits_sent += 1;
                    Ok(true)
                }
                Err(e @ Error::Remote(_)) => Err(e),
                Err(e) => {
                    rep.edits_waiting += 1;
                    push_unique(&mut rep.problems, format!("The edit of {} was not sent: {e}", item.path));
                    Ok(false)
                }
            };
        }
        let key = remote_ver.clone().unwrap_or_else(|| "none".to_string());
        if rec.awaiting.as_deref() == Some(key.as_str()) {
            // our edit is in the inbox and the home PC has not published a result yet
            rep.held_back += 1;
            return Ok(false);
        }
        rec.awaiting = None;
        if remote_ver == rec.manifest {
            return Ok(false);
        }
        let Some(v) = remote_ver else {
            rec.manifest = None; // (the home PC dropped its sidecar; the one here stays)
            return Ok(false);
        };
        let (Some(rel), Some(dest), Some(remote_sc)) = (sc_rel, sc_abs, item.sidecar.as_deref()) else { return Ok(false) };
        self.pull(rec, &rel, &dest, remote_sc, &v, local.as_deref(), rep)?;
        Ok(false)
    }

    /// Send an edit to `inbox/<device>/` as a new file named `<id>-<stamp>-<base>.xmp`.
    fn send(&self, rec: &mut Rec, item: &Item, bytes: &[u8], sha: &str, remote_ver: Option<&str>) -> Result<()> {
        let mut t = self.now();
        if let Some(last) = rec.stamp.as_deref().and_then(parse_stamp)
            && t <= last
        {
            t = last + Duration::from_secs(1);
        }
        let stamp = utc_stamp(t);
        let base = rec.base.clone().unwrap_or_else(|| "none".to_string());
        let name = format!("{}-{stamp}-{base}.xmp", item.id);
        let outbox = self.cfg.state_dir.join("outbox");
        let tmp = outbox.join(&name);
        write_atomic(&tmp, bytes)?;
        let sent = self.retry(|| self.remote.put(&tmp, &format!("inbox/{}/{name}", self.cfg.device)));
        let _ = std::fs::remove_file(&tmp);
        sent?;
        rec.synced = Some(sha.to_string());
        rec.base = Some(sidecar_version(bytes));
        rec.stamp = Some(stamp);
        rec.awaiting = Some(remote_ver.unwrap_or("none").to_string());
        self.logln(format!("sent the edit of {} to the home PC (it started from {base})", item.path));
        Ok(())
    }

    /// Replace the local sidecar (unchanged since it was synced) with the home PC's, keeping the old one in the history.
    #[allow(clippy::too_many_arguments)]
    fn pull(&self, rec: &mut Rec, rel: &str, dest: &Path, remote_sc: &str, version: &str, old: Option<&[u8]>, rep: &mut OtherReport) -> Result<()> {
        let tmp = temp_beside(dest);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(dir.display(), e))?;
        }
        let fetched = self.get(&format!("originals/{remote_sc}"), &tmp).and_then(|()| std::fs::read(&tmp).map_err(|e| Error::io(tmp.display(), e)));
        let bytes = match fetched {
            Ok(b) => b,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };
        if sidecar_version(&bytes) != version {
            // (the home PC changed it again while it was being fetched, or the copy is damaged: the next pass looks again)
            let _ = std::fs::remove_file(&tmp);
            self.logln(format!("{rel}: the downloaded sidecar is not the version the manifest named; trying again next time"));
            return Ok(());
        }
        let kept = match old.filter(|o| *o != bytes.as_slice()) {
            Some(old) => Some(self.keep_history(rel, old)?),
            None => None,
        };
        if let Err(e) = replace_file(&tmp, dest) {
            let _ = std::fs::remove_file(&tmp);
            if is_busy(&e) {
                // (another program holds the sidecar, or this user may not replace it: the file is unchanged, so its history copy goes, and the
                // next pass looks again. It is a problem Status shows, since a file that stays held would otherwise never update.)
                if let Some(k) = kept {
                    let _ = std::fs::remove_file(k);
                }
                self.logln(format!("{rel}: the sidecar cannot be replaced now ({e}); the home PC's version is taken next time"));
                push_unique(&mut rep.problems, format!("The sidecar {rel} is in use or cannot be replaced; the home PC's version will be tried again next sync."));
                return Ok(());
            }
            return Err(Error::io(dest.display(), e));
        }
        self.sidecar_written(dest);
        rec.manifest = Some(version.to_string());
        rec.base = Some(version.to_string());
        rec.synced = Some(sha256_hex(&bytes));
        rec.awaiting = None;
        rep.sidecars_updated.push(rel.to_string());
        self.logln(format!("took the home PC's sidecar for {rel}"));
        Ok(())
    }

    fn keep_history(&self, rel: &str, old: &[u8]) -> Result<PathBuf> {
        let now = iso(self.now());
        let (date, time) = (&now[..10], now[11..19].replace(':', ""));
        let base = self.cfg.state_dir.join("history").join(date);
        let mut name = with_time(rel, &time);
        let mut n = 1;
        while base.join(&name).exists() {
            n += 1;
            name = with_time(rel, &format!("{time}-{n}"));
        }
        let mut p = base;
        p.extend(name.split('/'));
        write_atomic(&p, old)?;
        Ok(p)
    }

    /// A photo the cloud library no longer lists was deleted at home: move it (and its sidecar) aside, never delete it.
    fn remove_vanished(&self, st: &mut State, by_id: &HashMap<&str, &Item>, rep: &mut OtherReport) {
        let vanished: Vec<String> = st.photos.keys().filter(|id| !by_id.contains_key(id.as_str())).cloned().collect();
        // (a photo already deleted here only has to be forgotten)
        let mut live = Vec::new();
        for id in vanished {
            if st.photos.get(&id).is_some_and(|r| r.gone) {
                st.photos.remove(&id);
            } else {
                live.push(id);
            }
        }
        if live.len() > self.cfg.max_delete {
            push_unique(
                &mut rep.problems,
                format!(
                    "The cloud library no longer lists {} photos that are on this PC, more than max_delete ({}). Nothing was moved. \
                     If the home PC really deleted them, raise max_delete in the config.",
                    live.len(),
                    self.cfg.max_delete
                ),
            );
            return;
        }
        let dir = self.cfg.state_dir.join("removed").join(self.today());
        for id in live {
            let Some(rec) = st.photos.get(&id).cloned() else { continue };
            let Some(abs) = self.target(&rec.path) else {
                st.photos.remove(&id);
                continue;
            };
            if !abs.exists() {
                st.photos.remove(&id);
                continue;
            }
            let same = std::fs::metadata(&abs).is_ok_and(|m| m.is_file()) && content_hash_file(&abs).is_ok_and(|(h, _)| h == id);
            if !same {
                push_unique(
                    &mut rep.problems,
                    format!("{} was deleted at home, but it was changed on this PC after it came from the cloud, so it was left where it is.", rec.path),
                );
                st.photos.remove(&id);
                continue;
            }
            let mut moved = vec![(rec.path.clone(), abs)];
            if let Some(sc) = rec.sidecar.as_deref()
                && let Some(p) = self.target(sc)
                && p.is_file()
            {
                moved.push((sc.to_string(), p));
            }
            let mut ok = true;
            for (rel, from) in &moved {
                if let Err(e) = move_aside(from, &dir, rel) {
                    push_unique(&mut rep.problems, format!("{rel} could not be moved aside: {e}"));
                    ok = false;
                    break;
                }
            }
            if ok {
                self.logln(format!("{} was deleted at home: moved to {}", rec.path, dir.display()));
                rep.removed.push(rec.path.clone());
                st.photos.remove(&id);
            }
        }
    }

    fn download(&self, st: &mut State, mut todo: Vec<&Item>, rep: &mut OtherReport) -> Result<()> {
        if todo.is_empty() {
            return Ok(());
        }
        todo.sort_by(|a, b| a.path.cmp(&b.path));
        let sizes_known = todo.iter().all(|i| i.size.is_some());
        self.rp.stage(Stage::Downloading, todo.len() as u64, if sizes_known { todo.iter().filter_map(|i| i.size).sum() } else { 0 });
        let mut staged: Vec<Staged> = Vec::new();
        let mut staged_bytes = 0u64;
        let mut cleaned: HashSet<PathBuf> = HashSet::new();
        let mut result = Ok(());
        for (i, item) in todo.iter().enumerate() {
            let need = item.size.unwrap_or(0).saturating_add(self.cfg.free_margin);
            if let Some(free) = free_space(&self.cfg.source)
                && free < need
            {
                let rest = &todo[i..];
                let total: u64 = rest.iter().map(|i| i.size.unwrap_or(0)).sum();
                push_unique(
                    &mut rep.problems,
                    format!(
                        "Not enough free disk space for {} more {} ({} needed, {} free, and {} is kept free). Free some space, or choose fewer folders.",
                        rest.len(),
                        if rest.len() == 1 { "file" } else { "files" },
                        human(total),
                        human(free),
                        human(self.cfg.free_margin)
                    ),
                );
                break;
            }
            let staged_one = self.stage(item, &mut cleaned, rep);
            self.rp.partial(0);
            self.rp.item_done(item.size.unwrap_or(0));
            match staged_one {
                Ok(Some(s)) => {
                    staged_bytes += s.size;
                    staged.push(s);
                }
                Ok(None) => {}
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
            if staged.len() >= BATCH_FILES || staged_bytes >= BATCH_BYTES {
                self.place(st, &mut staged, rep);
                staged_bytes = 0;
            }
        }
        // (also after a failure: what arrived stays)
        self.place(st, &mut staged, rep);
        result
    }

    /// Download one photo (and its sidecar) next to where it goes and check it. `Ok(None)`: skipped, with a line in `problems`.
    fn stage<'a>(&self, item: &'a Item, cleaned: &mut HashSet<PathBuf>, rep: &mut OtherReport) -> Result<Option<Staged<'a>>> {
        let Some(target) = self.target(&item.path) else { return Ok(None) };
        let Some(dir) = target.parent().map(Path::to_path_buf) else { return Ok(None) };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            push_unique(&mut rep.problems, format!("{}: {e}", item.path));
            return Ok(None);
        }
        if cleaned.insert(dir.clone()) {
            remove_stale_parts(&dir);
        }
        let tmp = temp_beside(&target);
        let fail = |tmp: &Path, e: Error, rep: &mut OtherReport| -> Result<Option<Staged<'a>>> {
            let _ = std::fs::remove_file(tmp);
            match e {
                Error::Remote(_) => Err(e),
                e => {
                    push_unique(&mut rep.problems, format!("{}: {e}", item.path));
                    Ok(None)
                }
            }
        };
        if let Err(e) = self.get(&format!("originals/{}", item.path), &tmp) {
            return fail(&tmp, e, rep);
        }
        let (hash, len) = match content_hash_file(&tmp) {
            Ok(x) => x,
            Err(e) => return fail(&tmp, e, rep),
        };
        if hash != item.id || item.size.is_some_and(|s| s != len) {
            let _ = std::fs::remove_file(&tmp);
            push_unique(
                &mut rep.problems,
                format!("{}: the downloaded file does not match its id (damaged in the cloud or on the way); it is tried again next time.", item.path),
            );
            return Ok(None);
        }
        if let Some(t) = item.mtime.as_deref().and_then(parse_iso) {
            let t = t.min(self.now());
            let _ = std::fs::OpenOptions::new().write(true).open(&tmp).and_then(|f| f.set_modified(t));
        }
        let mut sidecar = None;
        if let (Some(sc), Some(v)) = (&item.sidecar, &item.version) {
            let rel = join_rel(folder_of(&item.path), basename(sc));
            if let Some(dest) = self.target(&rel) {
                if dest.exists() {
                    // (one is there already, from an earlier download or the user: it stays)
                    if let Ok(b) = std::fs::read(&dest) {
                        sidecar = Some(StagedSidecar { tmp: None, dest, rel, sha: sha256_hex(&b) });
                    }
                } else {
                    let tmp_sc = temp_beside(&dest);
                    let got = self.get(&format!("originals/{sc}"), &tmp_sc).and_then(|()| std::fs::read(&tmp_sc).map_err(|e| Error::io(tmp_sc.display(), e)));
                    match got {
                        Ok(b) if sidecar_version(&b) == *v => sidecar = Some(StagedSidecar { tmp: Some(tmp_sc), dest, rel, sha: sha256_hex(&b) }),
                        Ok(_) => {
                            let _ = std::fs::remove_file(&tmp_sc);
                            self.logln(format!("{sc}: the downloaded sidecar is not the version the manifest named; the next pass fetches it"));
                        }
                        Err(e) => {
                            let _ = std::fs::remove_file(&tmp_sc);
                            let _ = std::fs::remove_file(&tmp);
                            return match e {
                                Error::Remote(_) => Err(e),
                                e => {
                                    push_unique(&mut rep.problems, format!("{sc}: {e}"));
                                    Ok(None)
                                }
                            };
                        }
                    }
                }
            }
        }
        Ok(Some(Staged { item, tmp, target, size: len, sidecar }))
    }

    /// Put the staged photos in the folder: the record first (so a crash is repaired by the next pass), each sidecar before its photo,
    /// both by a link that cannot replace anything, then tell LightCraft.
    fn place(&self, st: &mut State, staged: &mut Vec<Staged>, rep: &mut OtherReport) {
        if staged.is_empty() {
            return;
        }
        let batch = std::mem::take(staged);
        for s in &batch {
            st.photos.insert(s.item.id.clone(), Rec { path: s.item.path.clone(), placing: true, ..Rec::default() });
        }
        if let Err(e) = st.save() {
            push_unique(&mut rep.problems, format!("The state could not be saved, so nothing was put in the photo folder: {e}"));
            for s in &batch {
                st.photos.remove(&s.item.id);
                let _ = std::fs::remove_file(&s.tmp);
                if let Some(t) = s.sidecar.as_ref().and_then(|c| c.tmp.as_ref()) {
                    let _ = std::fs::remove_file(t);
                }
            }
            return;
        }
        for s in batch {
            let id = &s.item.id;
            let mut placed_sidecar = None;
            if let Some(sc) = &s.sidecar {
                let linked = match &sc.tmp {
                    Some(t) => link_no_replace(t, &sc.dest),
                    None => Ok(()),
                };
                if let Some(t) = &sc.tmp {
                    let _ = std::fs::remove_file(t);
                }
                match linked {
                    Ok(()) => placed_sidecar = Some(sc),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => placed_sidecar = Some(sc),
                    Err(e) => self.logln(format!("{}: the sidecar could not be put in place ({e}); the next pass fetches it", sc.rel)),
                }
            }
            match link_no_replace(&s.tmp, &s.target) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && content_hash_file(&s.target).is_ok_and(|(h, _)| h == *id) => {}
                Err(e) => {
                    let why = if e.kind() == std::io::ErrorKind::AlreadyExists { "another file appeared there meanwhile".to_string() } else { e.to_string() };
                    push_unique(&mut rep.problems, format!("{} could not be put in the photo folder: {why}", s.item.path));
                    let _ = std::fs::remove_file(&s.tmp);
                    st.photos.remove(id);
                    continue;
                }
            }
            let _ = std::fs::remove_file(&s.tmp);
            if let Some(dir) = s.target.parent() {
                sync_dir(dir);
            }
            if let Some(rec) = st.photos.get_mut(id) {
                if let Some(sc) = placed_sidecar {
                    rec.sidecar = Some(sc.rel.clone());
                    rec.manifest = s.item.version.clone();
                    rec.base = s.item.version.clone();
                    rec.synced = Some(sc.sha.clone());
                } else if let Some(sc) = &s.item.sidecar {
                    rec.sidecar = Some(join_rel(folder_of(&s.item.path), basename(sc)));
                }
                match self.announce(&s.target) {
                    Ok(()) => rec.placing = false,
                    Err(e) => self.logln(format!("LightCraft is not told about {} yet: {e}", s.item.path)),
                }
            }
            self.logln(format!("downloaded {}", s.item.path));
            rep.downloaded.push(s.item.path.clone());
        }
        if let Err(e) = st.save() {
            self.logln(format!("state not saved: {e}"));
        }
    }
}

/// Move `from` to `<dir>/<rel>` (a number is added to the name if that is taken); copy and delete if the two are on different drives.
fn move_aside(from: &Path, dir: &Path, rel: &str) -> std::io::Result<()> {
    let mut dest = dir.to_path_buf();
    dest.extend(rel.split('/'));
    let mut n = 1;
    while dest.exists() {
        n += 1;
        dest = dir.to_path_buf();
        dest.extend(rel.split('/'));
        let name = format!("{}.{n}", dest.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        dest.set_file_name(name);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(from, &dest).is_ok() {
        return Ok(());
    }
    // (another drive: copy, make sure it is there, then remove)
    let md = std::fs::metadata(from)?;
    std::fs::copy(from, &dest)?;
    std::fs::File::open(&dest)?.sync_all()?;
    if let Ok(t) = md.modified() {
        let _ = std::fs::OpenOptions::new().write(true).open(&dest).and_then(|f| f.set_modified(t));
    }
    std::fs::remove_file(from)
}

/// Temporary files a cut-off download left in `dir` (hidden, `.name.part-<pid>-<n>`), once they are a day old.
fn remove_stale_parts(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.')
            && name.contains(".part-")
            && e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|age| age > STALE_PART)
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

pub(super) fn human(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let mut f = n as f64;
    let mut unit = "B";
    for u in ["KiB", "MiB", "GiB", "TiB"] {
        f /= 1024.0;
        unit = u;
        if f < 1024.0 {
            break;
        }
    }
    format!("{f:.1} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_path_is_safe_only_if_this_system_can_write_it() {
        for rel in ["a.jpg", "2026/Trip 1/DSC1.ARW", "a b/c.xmp", "x/.hidden-is-the-home-pcs-business.jpg", "COMX/a.jpg", "con-ference/a.jpg"] {
            assert!(safe_rel_on(rel, false) && safe_rel_on(rel, true), "{rel}");
        }
        for rel in ["", "/a.jpg", "a//b.jpg", "a/", "./a.jpg", "a/./b.jpg", "../a.jpg", "a/../b.jpg", "a\\b.jpg", "a/b\u{0}c.jpg", "a/b\nc.jpg"] {
            assert!(!safe_rel_on(rel, false) && !safe_rel_on(rel, true), "{rel:?}");
        }
        // what only Windows refuses: the characters and names it reserves, a trailing dot or space, a stream (`photo.jpg:x`)
        for rel in [
            "a:b.jpg", "photo.jpg:stream", "a<b.jpg", "a>b.jpg", "a\"b.jpg", "a|b.jpg", "a?b.jpg", "a*b.jpg", "CON", "con.txt", "a/NUL.jpg", "a/aux/b.jpg",
            "Com1.jpg", "lpt9.xmp", "LPT1", "trailing.dot./a.jpg", "a/trailing space /b.jpg", "a/b.", "a/b ",
        ] {
            assert!(safe_rel_on(rel, false), "{rel:?} is fine on Linux");
            assert!(!safe_rel_on(rel, true), "{rel:?} must be refused on Windows");
        }
        assert!(safe_rel("a/b.jpg"));
        assert_eq!(safe_rel("a:b.jpg"), !cfg!(windows));
    }
}
