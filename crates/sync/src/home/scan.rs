//! Scanning the photo folder: which files are photos, which sidecar belongs to which photo, what changed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::state::{Row, VersionStore};
use crate::util::sidecar_version;

pub const PHOTO_EXTS: [&str; 18] =
    ["arw", "dng", "cr2", "cr3", "nef", "nrw", "raf", "orf", "rw2", "pef", "srw", "jpg", "jpeg", "heic", "heif", "tif", "tiff", "png"];
pub const JPEG_EXTS: [&str; 2] = ["jpg", "jpeg"];
pub const RAW_EXTS: [&str; 11] = ["arw", "dng", "cr2", "cr3", "nef", "nrw", "raf", "orf", "rw2", "pef", "srw"];
/// A file modified more recently than this is still being written: left for the next run.
pub const STABLE_SECS: f64 = 10.0;

pub fn ext_lower(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) if i > 0 => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

pub fn is_photo(path: &str) -> bool {
    let e = ext_lower(path);
    PHOTO_EXTS.contains(&e.as_str())
}

pub fn is_raw(path: &str) -> bool {
    RAW_EXTS.contains(&ext_lower(path).as_str())
}

pub fn is_jpeg(path: &str) -> bool {
    JPEG_EXTS.contains(&ext_lower(path).as_str())
}

/// `(folder, file name)` of a `/`-separated relative path.
fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// The file name without its last extension (a leading dot is not an extension).
pub fn stem(path: &str) -> &str {
    let name = split(path).1;
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

fn with_xmp(path: &str) -> String {
    let (dir, _) = split(path);
    let s = stem(path);
    if dir.is_empty() { format!("{s}.xmp") } else { format!("{dir}/{s}.xmp") }
}

// ------------------------------------------------------------------ exclude patterns

/// Shell-style match (`*`, `?`, `[abc]`, `[!a-z]`), case-sensitive: Python's `fnmatch.fnmatchcase`.
pub fn fnmatch(name: &str, pat: &str) -> bool {
    fn go(n: &[char], p: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&n[i..], &p[1..])),
            Some('?') => !n.is_empty() && go(&n[1..], &p[1..]),
            Some('[') => {
                // a class runs to the first `]` after its first member
                let mut j = 1;
                if p.get(j) == Some(&'!') {
                    j += 1;
                }
                if p.get(j) == Some(&']') {
                    j += 1;
                }
                while j < p.len() && p[j] != ']' {
                    j += 1;
                }
                if j >= p.len() {
                    return !n.is_empty() && n[0] == '[' && go(&n[1..], &p[1..]);
                }
                let Some(&c) = n.first() else { return false };
                let (neg, set) = if p[1] == '!' { (true, &p[2..j]) } else { (false, &p[1..j]) };
                let mut hit = false;
                let mut k = 0;
                while k < set.len() {
                    if k + 2 < set.len() && set[k + 1] == '-' {
                        hit |= set[k] <= c && c <= set[k + 2];
                        k += 3;
                    } else {
                        hit |= set[k] == c;
                        k += 1;
                    }
                }
                hit != neg && go(&n[1..], &p[j + 1..])
            }
            Some(&c) => n.first() == Some(&c) && go(&n[1..], &p[1..]),
        }
    }
    let (n, p): (Vec<char>, Vec<char>) = (name.chars().collect(), pat.chars().collect());
    go(&n, &p)
}

pub fn is_excluded_dir(name: &str, exclude: &[String]) -> bool {
    exclude.iter().any(|p| fnmatch(name, p))
}

/// True if any directory component of a root-relative path matches an exclude pattern. The file name itself is never
/// matched (a file merely named like the pattern is not excluded).
pub fn is_excluded(rel: &str, exclude: &[String]) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    parts[..parts.len() - 1].iter().any(|p| is_excluded_dir(p, exclude))
}

/// Hidden files and folders are never published.
pub fn is_skipped_name(name: &str) -> bool {
    name.starts_with('.')
}

/// Whether the mirror covers this path (not hidden anywhere, not under an excluded folder).
pub fn in_scope(rel: &str, exclude: &[String]) -> bool {
    !rel.split('/').any(is_skipped_name) && !is_excluded(rel, exclude)
}

// ------------------------------------------------------------------ walking

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub abs: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
}

pub fn ns_of(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

pub fn secs_of(ns: i64) -> f64 {
    ns as f64 / 1e9
}

/// All non-hidden regular files below `root`, skipping hidden and excluded directories. Keys are `/`-separated.
pub fn walk_source(root: &Path, exclude: &[String]) -> BTreeMap<String, FileInfo> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if is_skipped_name(&name) {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if ft.is_dir() {
                if !is_excluded_dir(&name, exclude) {
                    stack.push((e.path(), r));
                }
            } else if ft.is_file()
                && let Ok(md) = e.metadata()
            {
                out.insert(r, FileInfo { abs: e.path(), size: md.len(), mtime_ns: md.modified().map(ns_of).unwrap_or(0) });
            }
        }
    }
    out
}

/// Mirrors the engine's StemOwners rule: when several photos in a folder share a stem, the raw (else the first by name)
/// owns `<stem>.xmp` and the others use `<file>.xmp`. Returns `{photo: existing sidecar or None}` and `{photo: the
/// sidecar path to create}`.
pub fn find_sidecars(photos: &[String], files: &BTreeSet<String>) -> (HashMap<String, Option<String>>, HashMap<String, String>) {
    let lower: HashMap<String, &String> = files.iter().map(|f| (f.to_lowercase(), f)).collect();
    let mut groups: BTreeMap<(String, String), Vec<&String>> = BTreeMap::new();
    for p in photos {
        groups.entry((split(p).0.to_string(), stem(p).to_lowercase())).or_default().push(p);
    }
    let (mut existing, mut target) = (HashMap::new(), HashMap::new());
    for members in groups.values() {
        let owner = (members.len() > 1).then(|| {
            let mut m = members.clone();
            m.sort_by_key(|x| (!is_raw(x), x.to_lowercase()));
            m[0]
        });
        for &p in members {
            let stem_x = with_xmp(p);
            let full_x = format!("{p}.xmp");
            let uses_stem = owner.is_none_or(|o| o == p);
            target.insert(p.clone(), if uses_stem { stem_x.clone() } else { full_x.clone() });
            let cands = if uses_stem { vec![stem_x, full_x] } else { vec![full_x] };
            let found = cands.iter().find_map(|c| lower.get(&c.to_lowercase()).map(|f| (*f).clone()));
            existing.insert(p.clone(), found);
        }
    }
    (existing, target)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub size: u64,
    pub mtime_ns: i64,
    pub sidecar: Option<String>,
    pub sidecar_hash: Option<String>,
    pub sidecar_mtime_ns: Option<i64>,
    pub sidecar_target: String,
}

pub struct Scan {
    pub entries: BTreeMap<String, Entry>,
    /// Photos or sidecars modified less than [`STABLE_SECS`] ago.
    pub unstable: BTreeSet<String>,
    /// Every file below the root, with its size and mtime (what the mirror compares).
    pub files: BTreeMap<String, FileInfo>,
}

/// `versions`: the sidecar version store to keep every stable sidecar's bytes in; `None` writes nothing.
pub fn scan_source(root: &Path, now: SystemTime, exclude: &[String], versions: Option<&VersionStore>) -> Scan {
    scan_source_settled(root, now, exclude, versions, &[])
}

/// [`scan_source`], where the sidecars in `settled` (this run wrote them whole, atomically) count as stable however new
/// they are: the files the inbox step has just written are published by the same sync, not the next one.
pub fn scan_source_settled(root: &Path, now: SystemTime, exclude: &[String], versions: Option<&VersionStore>, settled: &[String]) -> Scan {
    let files = walk_source(root, exclude);
    let photos: Vec<String> = files.keys().filter(|r| is_photo(r)).cloned().collect();
    let all: BTreeSet<String> = files.keys().cloned().collect();
    let (existing, target) = find_sidecars(&photos, &all);
    let now_s = secs_of(ns_of(now));
    let (mut entries, mut unstable) = (BTreeMap::new(), BTreeSet::new());
    for p in &photos {
        let f = &files[p];
        let mut e = Entry {
            size: f.size,
            mtime_ns: f.mtime_ns,
            sidecar: None,
            sidecar_hash: None,
            sidecar_mtime_ns: None,
            sidecar_target: target[p].clone(),
        };
        if now_s - secs_of(f.mtime_ns) < STABLE_SECS {
            unstable.insert(p.clone());
        }
        if let Some(sc) = &existing[p] {
            let s = &files[sc];
            e.sidecar = Some(sc.clone());
            e.sidecar_mtime_ns = Some(s.mtime_ns);
            if now_s - secs_of(s.mtime_ns) < STABLE_SECS && !settled.contains(sc) {
                unstable.insert(sc.clone());
            }
            match std::fs::read(&s.abs) {
                Err(_) => e.sidecar = None,
                Ok(data) => {
                    let h = sidecar_version(&data);
                    // (a file still being written may never be published)
                    if let Some(v) = versions.filter(|_| !unstable.contains(sc)) {
                        v.store(&data, &h);
                    }
                    e.sidecar_hash = Some(h);
                }
            }
        }
        entries.insert(p.clone(), e);
    }
    Scan { entries, unstable, files }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub new: Vec<String>,
    pub changed: Vec<String>,
    pub sc_changed: Vec<String>,
    pub removed: Vec<String>,
    pub retry: Vec<String>,
}

impl Plan {
    pub fn any(&self) -> bool {
        !(self.new.is_empty() && self.changed.is_empty() && self.sc_changed.is_empty() && self.removed.is_empty() && self.retry.is_empty())
    }
}

/// Unstable photos are skipped (treated as unchanged for this run) but never counted as removed.
pub fn diff_scan(scan: &Scan, known: &BTreeMap<String, Row>) -> Plan {
    let mut plan = Plan::default();
    for (p, e) in &scan.entries {
        if scan.unstable.contains(p) || e.sidecar.as_ref().is_some_and(|s| scan.unstable.contains(s)) {
            continue;
        }
        match known.get(p) {
            None => plan.new.push(p.clone()),
            Some(k) if k.size != e.size || k.mtime_ns != e.mtime_ns => plan.changed.push(p.clone()),
            Some(k) if k.sidecar_hash != e.sidecar_hash => plan.sc_changed.push(p.clone()),
            Some(k) if k.status == "retry" => plan.retry.push(p.clone()),
            Some(_) => {}
        }
    }
    plan.removed = known.keys().filter(|p| !scan.entries.contains_key(*p)).cloned().collect();
    plan
}

/// Removed paths that were actually published (status ok); never-published rows don't count.
pub fn published_removals<'a>(removed: &'a [String], known: &BTreeMap<String, Row>) -> Vec<&'a String> {
    removed.iter().filter(|p| known.get(*p).is_none_or(|r| r.status == "ok")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnmatch_is_python_like() {
        assert!(fnmatch("tmp2", "tmp*"));
        assert!(fnmatch("a1", "a[0-9]"));
        assert!(!fnmatch("a1", "a[!0-9]"));
        assert!(fnmatch("ab", "a?"));
        assert!(!fnmatch("Darktable_Exported", "darktable_exported"));
        assert!(fnmatch("a[", "a["));
    }

    #[test]
    fn stems_and_extensions() {
        assert_eq!(stem("A/b.c.jpg"), "b.c");
        assert_eq!(ext_lower("A/IMG.JPG"), "jpg");
        assert_eq!(with_xmp("A/b.c.jpg"), "A/b.c.xmp");
        assert_eq!(with_xmp("x.png"), "x.xmp");
        assert!(is_photo("a.CR3") && !is_photo("a.txt") && !is_photo(".jpg"));
    }
}
