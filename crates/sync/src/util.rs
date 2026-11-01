//! Small helpers: hashes, UTC times, atomic file writes, bounded parallelism.

// (the two Win32 calls below; nothing else here is unsafe)
#![cfg_attr(windows, allow(unsafe_code))]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The manifest's `sidecarVersion` of a sidecar: the first 16 hex of its SHA-256.
pub fn sidecar_version(bytes: &[u8]) -> String {
    sha256_hex(bytes)[..16].to_string()
}

fn secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// (year, month, day, hour, minute, second) in UTC.
fn civil(t: SystemTime) -> (i64, u32, u32, u32, u32, u32) {
    let s = secs(t);
    let (days, rem) = (s.div_euclid(86_400), s.rem_euclid(86_400));
    // days since 1970-01-01 -> civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32)
}

/// `YYYYmmddTHHMMSSZ`, the inbox's stamp.
pub fn utc_stamp(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s) = civil(t);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// `YYYY-mm-ddTHH:MM:SSZ`.
pub fn iso(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s) = civil(t);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

pub fn parse_stamp(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (n(0..4)?, n(4..6)?, n(6..8)?, n(9..11)?, n(11..13)?, n(13..15)?);
    // civil -> days
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let t = days * 86_400 + h * 3600 + mi * 60 + sec;
    Some(if t >= 0 { UNIX_EPOCH + Duration::from_secs(t as u64) } else { UNIX_EPOCH })
}

/// `YYYY-mm-ddTHH:MM:SSZ`, with optional fractional seconds (`...SS.123Z`): what [`iso`] writes, and a bit more. A real UTC date
/// from 1970 on, else None.
pub fn parse_iso(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() < 20 || !s.is_ascii() || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[b.len() - 1] != b'Z' {
        return None;
    }
    let stamp = format!("{}{}{}T{}{}{}Z", &s[0..4], &s[5..7], &s[8..10], &s[11..13], &s[14..16], &s[17..19]);
    if !stamp.bytes().enumerate().all(|(i, c)| if i == 8 { c == b'T' } else if i == 15 { c == b'Z' } else { c.is_ascii_digit() }) {
        return None;
    }
    let t = parse_stamp(&stamp)?;
    if utc_stamp(t) != stamp {
        return None; // (not a real date, or before 1970)
    }
    match &s[19..s.len() - 1] {
        "" => Some(t),
        frac => {
            let digits = frac.strip_prefix('.')?;
            if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let nanos = format!("{digits:0<9}").parse::<u32>().ok()?;
            Some(t + Duration::new(0, nanos))
        }
    }
}

pub fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A name for a temporary file next to `dest` (same folder, so the rename is atomic).
pub fn temp_beside(dest: &Path) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    dest.with_file_name(format!(".{name}.part-{}-{n}", std::process::id()))
}

/// Write `bytes` to `dest` atomically (temporary file, fsync, rename).
pub fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir.display(), e))?;
    }
    let tmp = temp_beside(dest);
    let r = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        replace_file(&tmp, dest)
    })();
    r.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::io(dest.display(), e)
    })
}

/// Hard-link `tmp` to `dest`, failing with `AlreadyExists` if `dest` is there: unlike a rename it can never replace a file.
/// A filesystem without hard links (exFAT, FAT32, some network shares) gets [`move_no_replace`].
pub(crate) fn link_no_replace(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => move_no_replace(tmp, dest),
    }
}

/// Move `tmp` to `dest` unless `dest` exists (`AlreadyExists`, whatever it is, a dangling link included). On Windows this is one call
/// that refuses to replace (`MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`), so a race cannot overwrite anything; elsewhere it is a check
/// and a rename, which cannot be atomic.
#[cfg(windows)]
pub(crate) fn move_no_replace(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let (from, to) = (wide(&absolute(tmp)?), wide(&absolute(dest)?));
    // SAFETY: both are NUL-terminated UTF-16 strings that outlive the call.
    let ok = unsafe { windows_sys::Win32::Storage::FileSystem::MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
    if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(windows))]
pub(crate) fn move_no_replace(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dest) {
        Ok(_) => Err(std::io::ErrorKind::AlreadyExists.into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::rename(tmp, dest),
        Err(e) => Err(e),
    }
}

/// An absolute path; a long one (over 259 characters) gets the `\\?\` prefix that lets Windows go past MAX_PATH. (Rust's own file
/// functions do this by themselves; the raw Win32 calls here do not.)
#[cfg(windows)]
fn absolute(p: &Path) -> std::io::Result<PathBuf> {
    let abs = std::path::absolute(p)?;
    let s = abs.to_string_lossy();
    if s.len() >= 260 && !s.starts_with(r"\\?\") {
        return Ok(match s.strip_prefix(r"\\") {
            Some(unc) => PathBuf::from(format!(r"\\?\UNC\{unc}")),
            None => PathBuf::from(format!(r"\\?\{s}")),
        });
    }
    Ok(abs)
}

/// Whether `e` means another program has the file open in a way that stops us replacing it (a sharing or lock violation, or access
/// denied while it is held): worth trying again a little later. Always false off Windows, where a rename never meets that.
pub fn is_busy(e: &std::io::Error) -> bool {
    // ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
    cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

/// Rename `tmp` over `dest`, replacing it. Windows refuses while another program (LightCraft with a sidecar open, a virus scanner, the
/// indexer) holds `dest` without sharing: that is tried again for about a second and a half, and if it persists the error is returned
/// (see [`is_busy`]); a caller treats that as "later", never as a failed pass.
pub fn replace_file(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    replace_file_waiting(tmp, dest, Duration::from_millis(50), 6)
}

fn replace_file_waiting(tmp: &Path, dest: &Path, first_wait: Duration, tries: u32) -> std::io::Result<()> {
    let mut wait = first_wait;
    for attempt in 1.. {
        match std::fs::rename(tmp, dest) {
            Err(e) if is_busy(&e) && attempt < tries => {
                std::thread::sleep(wait);
                wait *= 2;
            }
            r => return r,
        }
    }
    unreachable!()
}

pub(crate) fn sync_dir(dir: &Path) {
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
}

/// Free bytes on the filesystem holding `path` (or its nearest folder that exists); None when that cannot be told.
#[cfg(unix)]
pub(crate) fn free_space(path: &Path) -> Option<u64> {
    let abs = std::path::absolute(path).ok()?;
    let mut p: &Path = &abs;
    loop {
        if let Ok(s) = rustix::fs::statvfs(p) {
            return Some(s.f_bavail.saturating_mul(s.f_frsize));
        }
        p = p.parent()?;
    }
}

/// The same on Windows: the bytes free to this user (quotas counted), from `GetDiskFreeSpaceExW`.
#[cfg(windows)]
pub(crate) fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let abs = absolute(path).ok()?;
    let mut p: &Path = &abs;
    loop {
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut avail = 0u64;
        // SAFETY: `wide` is NUL-terminated and outlives the call; `avail` is a valid place for the result; the other two are optional.
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut())
        };
        if ok != 0 {
            return Some(avail);
        }
        p = p.parent()?;
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn free_space(_path: &Path) -> Option<u64> {
    None
}

/// Run `f` over `items` on up to `jobs` threads; results in item order.
pub fn parallel<T: Sync, R: Send>(items: &[T], jobs: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results: std::sync::Mutex<Vec<(usize, R)>> = std::sync::Mutex::new(Vec::with_capacity(items.len()));
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1).min(items.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let r = f(item);
                    results.lock().unwrap_or_else(|e| e.into_inner()).push((i, r));
                }
            });
        }
    });
    let mut v = results.into_inner().unwrap_or_else(|e| e.into_inner());
    v.sort_by_key(|(i, _)| *i);
    v.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_round_trip() {
        let t = UNIX_EPOCH + Duration::from_secs(1_793_000_000);
        let s = utc_stamp(t);
        assert_eq!(parse_stamp(&s), Some(t));
        assert_eq!(utc_stamp(UNIX_EPOCH), "19700101T000000Z");
        assert_eq!(iso(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn parallel_keeps_order() {
        let v: Vec<u32> = (0..50).collect();
        assert_eq!(parallel(&v, 6, |x| x * 2), v.iter().map(|x| x * 2).collect::<Vec<_>>());
        assert!(parallel::<u32, u32>(&[], 6, |x| *x).is_empty());
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lc-util-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_move_that_may_not_replace_never_does_and_the_link_falls_back_to_it() {
        let d = scratch("move");
        std::fs::write(d.join("tmp"), b"new").unwrap();
        std::fs::write(d.join("dest"), b"old").unwrap();
        // (the fallback of a file system without hard links, called directly)
        assert_eq!(move_no_replace(&d.join("tmp"), &d.join("dest")).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(d.join("dest")).unwrap(), b"old");
        assert_eq!(std::fs::read(d.join("tmp")).unwrap(), b"new", "the source stays when the move is refused");
        move_no_replace(&d.join("tmp"), &d.join("fresh")).unwrap();
        assert_eq!(std::fs::read(d.join("fresh")).unwrap(), b"new");
        assert!(!d.join("tmp").exists(), "a move takes the source with it");
        // and the link itself, which tries a hard link first
        std::fs::write(d.join("tmp2"), b"two").unwrap();
        assert_eq!(link_no_replace(&d.join("tmp2"), &d.join("fresh")).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        link_no_replace(&d.join("tmp2"), &d.join("linked")).unwrap();
        assert_eq!(std::fs::read(d.join("linked")).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_write_replaces_atomically_and_leaves_no_temporary_file() {
        let d = scratch("atomic");
        write_atomic(&d.join("sub/f.xmp"), b"one").unwrap();
        write_atomic(&d.join("sub/f.xmp"), b"two").unwrap();
        assert_eq!(std::fs::read(d.join("sub/f.xmp")).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(d.join("sub")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["f.xmp".to_string()]);
        assert!(!is_busy(&std::io::Error::from(std::io::ErrorKind::NotFound)));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn free_space_is_told_for_a_folder_a_file_and_a_path_that_does_not_exist_yet() {
        let d = scratch("free");
        std::fs::write(d.join("f"), b"x").unwrap();
        for p in [d.clone(), d.join("f"), d.join("not/yet/there"), PathBuf::from("relative/not/there")] {
            let free = free_space(&p);
            assert!(free.is_some_and(|n| n > 0), "{}: {free:?}", p.display());
        }
        let _ = std::fs::remove_dir_all(d);
    }

    /// Windows: a sidecar LightCraft holds open without sharing cannot be replaced. That is "later", not a failure: it is tried again
    /// for a moment, then reported as busy; and it goes through at once when the other program lets go.
    #[cfg(windows)]
    #[test]
    fn a_file_another_program_holds_open_is_tried_again_and_then_reported_busy() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = scratch("busy");
        let (tmp, dest) = (d.join("tmp"), d.join("dest.xmp"));
        std::fs::write(&dest, b"old").unwrap();
        std::fs::write(&tmp, b"new").unwrap();
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&dest).unwrap();
        let e = replace_file_waiting(&tmp, &dest, Duration::from_millis(1), 4).unwrap_err();
        assert!(is_busy(&e), "{e:?}");
        assert_eq!(std::fs::read(&tmp).unwrap(), b"new", "the new file is still there to be tried again");
        // (let go while the retries are waiting)
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(held);
        });
        replace_file(&tmp, &dest).unwrap();
        t.join().unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        // a file that is open WITH sharing (how std and most programs open) is replaced at once
        std::fs::write(&tmp, b"newer").unwrap();
        let _open = std::fs::File::open(&dest).unwrap();
        replace_file(&tmp, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"newer");
        let _ = std::fs::remove_dir_all(d);
    }

    /// Windows: paths past MAX_PATH (260) work for everything the sync does, including the raw Win32 calls.
    #[cfg(windows)]
    #[test]
    fn paths_longer_than_260_characters_work() {
        let d = scratch("long");
        let mut deep = d.clone();
        while deep.as_os_str().len() < 330 {
            deep.push("a folder with a longish name");
        }
        let (a, b) = (deep.join("one.jpg"), deep.join("two.jpg"));
        write_atomic(&a, b"bytes").unwrap();
        link_no_replace(&a, &b).unwrap();
        assert_eq!(link_no_replace(&a, &b).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        let c = deep.join("three.jpg");
        move_no_replace(&b, &c).unwrap();
        // (not `a`: it is a hard link of the same file as `c`, and a move of a file onto itself is no conflict)
        let other = deep.join("four.jpg");
        write_atomic(&other, b"other").unwrap();
        assert_eq!(move_no_replace(&other, &c).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&c).unwrap(), b"bytes");
        assert!(a.as_os_str().len() > 260);
        assert!(free_space(&a).is_some_and(|n| n > 0));
        let _ = std::fs::remove_dir_all(r"\\?\".to_string() + &d.to_string_lossy());
    }
}
