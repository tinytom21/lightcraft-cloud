//! Showing new sidecar edits in a running LightCraft, until LightCraft does it by itself (upstream PR #3421).
//!
//! When this app writes a sidecar into the photo folder (a phone edit applied or merged on the home PC; the home PC's version taken on
//! another PC), LightCraft keeps showing the old edit until someone runs *Synchronize Folder* on that folder by hand. This module
//! does that by hand's job: it remembers the folders, and asks a running LightCraft to read the sidecars in them.
//!
//! * **The list of folders is a small durable list of its own** (`<state>/lightcraft-refresh/`, one file per folder, the same kind of
//!   list as the photos' one in [`crate::pending`], so a restart of either program loses nothing). A folder is queued once however many
//!   sidecars were written into it. A new sidecar in a folder whose earlier tries failed starts its tries over.
//! * **Order of a check:** the photos are added first ([`crate::pending::flush`]), so that a photo that is new to the library is in it
//!   before its folder is refreshed; then this.
//! * **Only with LightCraft writing sidecars.** Reading a sidecar makes the sidecar win over what the library holds, as one undo step.
//!   That is right for a library that writes every edit into its sidecar (the sidecar then says what the library says, apart from the
//!   edit just synced), and wrong for one that keeps edits only in its catalog (a sidecar that merely looks newer would replace
//!   them). So before anything is sent the app asks `library.xmpPreferences` (no parameters, so it changes nothing) for `autoWrite`.
//!   Off: nothing is sent, the folders stay on the list, and the Status window says what to switch on (the same answer is also what
//!   explains why edits made on a Windows PC never reached the phone).
//! * **What is sent** is `folder.synchronize` with `importNew`, `relinkMoved` and `removeMissing` off and `readMetadata` on: no photo is
//!   added, relinked or removed, and no file is touched. The folder is sent as the system writes it, never as LightCraft reports it.
//! * **A folder leaves the list** when LightCraft answers without failures. Failures (a sidecar it could not read, a folder that is not
//!   in its library - normal on a PC where the folder was never imported) are tried [`MAX_TRIES`] times, then the folder is dropped with
//!   one line in the log; nothing goes to Problems. A closed LightCraft, a silent port or a slow answer keep the list and count nothing.
//! * **A check is bounded:** at most [`MAX_FOLDERS`] folders, one at a time, each with the link's generous but fixed deadline.

use std::path::{Path, PathBuf};

use crate::lightcraft::{LightCraftLink, LinkError};
use crate::pending::PendingList;

/// The folder (inside the state folder) of the list.
pub const DIR: &str = "lightcraft-refresh";
/// How many folders one check sends (the rest go at the next check).
pub const MAX_FOLDERS: usize = 3;
/// Answers that were not a success (LightCraft answered and could not do it) before a folder is dropped.
pub const MAX_TRIES: u32 = 3;

/// The list of folders waiting to be read again.
pub fn list(state_dir: &Path) -> PendingList {
    PendingList::in_dir(state_dir, DIR)
}

/// A path as the system writes it: absolute, with the system's separators (a path built from a `/`-separated name on Windows would
/// otherwise reach LightCraft as `C:\Photos/Trip`, which LightCraft may not match with its own `C:\Photos\Trip`).
pub fn system_form(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    abs.components().collect()
}

/// The folder to refresh for a sidecar that was just written.
pub fn folder_of(sidecar: &Path) -> Option<PathBuf> {
    sidecar.parent().map(system_form).filter(|f| f.is_absolute())
}

/// The hook the sync crate calls for each sidecar it writes into the photo folder: queues the sidecar's folder (once).
pub fn queue_hook(state_dir: &Path) -> impl Fn(&Path) -> Result<(), String> + Send + Sync + 'static {
    let list = list(state_dir);
    move |sidecar| match folder_of(sidecar) {
        Some(folder) => list.touch(&folder, &lightcraft_cloud_client::home::utc_iso(std::time::SystemTime::now())),
        None => Ok(()),
    }
}

/// What one check did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefreshReport {
    /// What LightCraft said about writing edits into sidecars (`autoWrite`); None when it did not say (closed, or an answer that is
    /// not understood).
    pub auto_write: Option<bool>,
    /// Folders LightCraft read the sidecars of (taken off the list).
    pub refreshed: usize,
    /// Folders given up on after [`MAX_TRIES`] answers that were not a success (taken off the list).
    pub dropped: usize,
    /// Folders still on the list.
    pub waiting: usize,
    /// Whether the port answered as LightCraft; None: it was not asked.
    pub reachable: Option<bool>,
    /// Lines for the log.
    pub log: Vec<String>,
}

/// Ask a running LightCraft about writing edits to sidecars and, when it does and `refresh` is on, read the sidecars of the folders on
/// the list. The link is identified here (fresh, so a connection dropped by an earlier request is not trusted).
pub fn run(list: &PendingList, link: &mut dyn LightCraftLink, refresh: bool) -> RefreshReport {
    let mut rep = RefreshReport::default();
    let mut entries = list.list();
    // (`touch` only writes absolute paths; anything else in the folder was not written by this app and is never sent)
    entries.retain(|e| Path::new(&e.path).is_absolute());
    rep.waiting = entries.len();
    if let Err(e) = link.identify() {
        rep.reachable = Some(false);
        if !entries.is_empty() {
            rep.log.push(format!("lightcraft: not refreshing {} folders: {}", entries.len(), e.text()));
        }
        return rep;
    }
    rep.reachable = Some(true);
    match link.xmp_auto_write() {
        Ok(on) => rep.auto_write = Some(on),
        Err(e) => {
            if e.is_unreachable() || matches!(e, LinkError::NotReady(_)) {
                rep.reachable = Some(false);
            }
            if !entries.is_empty() {
                rep.log.push(format!("lightcraft: could not read the XMP preferences, so {} folders are not refreshed: {}", entries.len(), e.text()));
            }
            return rep;
        }
    }
    if !refresh || rep.auto_write != Some(true) || entries.is_empty() {
        return rep;
    }
    for entry in entries.iter_mut().take(MAX_FOLDERS) {
        let (why, reconnect) = match link.sync_folder(&entry.path) {
            Ok(r) if r.failed.is_empty() => {
                if let Err(e) = list.remove(&entry.path) {
                    rep.log.push(format!("lightcraft: read {} but it could not be taken off the list: {e}", entry.path));
                    continue;
                }
                rep.refreshed += 1;
                rep.waiting -= 1;
                rep.log.push(format!("lightcraft: read the sidecars in {} ({} updated)", entry.path, r.read));
                if r.imported + r.relinked + r.removed > 0 {
                    rep.log.push(format!(
                        "lightcraft: unexpected answer for {}: {} added, {} relinked, {} removed (only reading was asked)",
                        entry.path, r.imported, r.relinked, r.removed
                    ));
                }
                continue;
            }
            Ok(r) => {
                let (p, why) = &r.failed[0];
                (format!("{} of {} could not be read, for example {p}: {why}", r.failed.len(), r.failed.len() + r.read), false)
            }
            Err(e) if e.is_unreachable() || matches!(e, LinkError::NotReady(_)) => {
                // (no answer is not a verdict on the folder: nothing is counted, and the folders after it wait for the next check)
                rep.reachable = Some(false);
                rep.log.push(format!("lightcraft: refresh of {} did not work: {}", entry.path, e.text()));
                break;
            }
            Err(e) => (e.text().to_string(), true),
        };
        entry.attempts += 1;
        entry.last_error = Some(why.clone());
        if entry.attempts >= MAX_TRIES {
            let _ = list.remove(&entry.path);
            rep.dropped += 1;
            rep.waiting -= 1;
            rep.log.push(if why.contains("no photo in the library") {
                format!("lightcraft: {} is not a folder of LightCraft's library, so there is nothing to refresh there (dropped after {} tries)", entry.path, entry.attempts)
            } else {
                format!("lightcraft: gave up refreshing {} after {} tries: {why}", entry.path, entry.attempts)
            });
        } else {
            rep.log.push(format!("lightcraft: refresh of {} did not work (try {}): {why}", entry.path, entry.attempts));
            if let Err(e) = list.update(entry) {
                rep.log.push(format!("lightcraft: cannot save the list of folders: {e}"));
            }
        }
        // (an error drops the connection: the next folder needs a new one)
        if reconnect && link.identify().is_err() {
            rep.reachable = Some(false);
            break;
        }
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lightcraft::SyncReply;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lc-desktop-refresh-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// An absolute folder of this system (no separator is written out here, so it is the same test on Windows).
    fn folder(name: &str) -> PathBuf {
        std::env::temp_dir().join("lc-refresh-photos").join(name)
    }

    fn text(p: &Path) -> String {
        p.to_str().unwrap().to_string()
    }

    /// A LightCraft that can be told what to answer; records what it was asked, in order.
    struct Fake {
        calls: Vec<String>,
        identify: Option<LinkError>,
        auto_write: Result<bool, LinkError>,
        /// Answer per folder (by the last name of the path); unlisted folders are fine.
        answers: Vec<(String, Result<SyncReply, LinkError>)>,
        sent: Vec<String>,
    }

    impl Fake {
        fn new(auto_write: bool) -> Fake {
            Fake { calls: vec![], identify: None, auto_write: Ok(auto_write), answers: vec![], sent: vec![] }
        }
        fn answer(mut self, folder_name: &str, a: Result<SyncReply, LinkError>) -> Fake {
            self.answers.push((folder_name.to_string(), a));
            self
        }
    }

    impl LightCraftLink for Fake {
        fn identify(&mut self) -> Result<(), LinkError> {
            self.calls.push("identify".into());
            self.identify.clone().map_or(Ok(()), Err)
        }
        fn import_add(&mut self, _paths: &[String]) -> Result<crate::lightcraft::ImportReply, LinkError> {
            panic!("a refresh never imports");
        }
        fn xmp_auto_write(&mut self) -> Result<bool, LinkError> {
            self.calls.push("prefs".into());
            self.auto_write.clone()
        }
        fn sync_folder(&mut self, f: &str) -> Result<SyncReply, LinkError> {
            self.calls.push("sync".into());
            self.sent.push(f.to_string());
            let name = Path::new(f).file_name().unwrap().to_str().unwrap();
            match self.answers.iter().find(|a| a.0 == name) {
                Some((_, a)) => a.clone(),
                None => Ok(SyncReply { read: 1, ..SyncReply::default() }),
            }
        }
    }

    fn refused(m: &str) -> Result<SyncReply, LinkError> {
        Err(LinkError::Refused(m.into()))
    }

    fn queue(tag: &str, names: &[&str]) -> (PathBuf, PendingList) {
        let d = temp(tag);
        let l = list(&d);
        for (i, n) in names.iter().enumerate() {
            l.touch(&folder(n), &format!("2026-11-01T10:00:{i:02}Z")).unwrap();
        }
        (d, l)
    }

    #[test]
    fn the_hook_queues_the_folder_of_the_sidecar_once_however_many_sidecars_are_written() {
        let d = temp("hook");
        let hook = queue_hook(&d);
        hook(&folder("Trip").join("a.xmp")).unwrap();
        hook(&folder("Trip").join("b.CR3.xmp")).unwrap();
        hook(&folder("Trip").join("a.xmp")).unwrap();
        hook(&folder("Trip").join("Day2").join("c.xmp")).unwrap();
        let got: Vec<String> = list(&d).list().into_iter().map(|e| e.path).collect();
        assert_eq!(got, [text(&folder("Trip")), text(&folder("Trip").join("Day2"))], "the folder, as the system writes it");
        // a new process sees the same list; a relative sidecar path is made absolute rather than refused
        hook(Path::new("rel.xmp")).unwrap();
        assert!(list(&d).list().iter().all(|e| Path::new(&e.path).is_absolute()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_new_sidecar_in_a_folder_that_failed_before_starts_its_tries_over() {
        let (d, l) = queue("retouch", &["Trip"]);
        let mut link = Fake::new(true).answer("Trip", refused("busy"));
        run(&l, &mut link, true);
        assert_eq!(l.list()[0].attempts, 1);
        l.touch(&folder("Trip"), "2026-11-01T11:00:00Z").unwrap();
        let e = &l.list()[0];
        assert_eq!((e.attempts, e.last_error.as_deref(), e.added.as_str()), (0, None, "2026-11-01T10:00:00Z"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn with_autowrite_off_nothing_is_sent_and_the_list_is_kept_until_it_is_on() {
        let (d, l) = queue("off", &["A", "B"]);
        let mut link = Fake::new(false);
        for _ in 0..5 {
            let r = run(&l, &mut link, true);
            assert_eq!((r.auto_write, r.refreshed, r.waiting, r.reachable), (Some(false), 0, 2, Some(true)));
        }
        assert!(link.sent.is_empty(), "{:?}", link.calls);
        assert!(l.list().iter().all(|e| e.attempts == 0), "waiting for a setting is not a failure of a folder");
        // switched on in LightCraft: the next check drains it, oldest first
        link.auto_write = Ok(true);
        let r = run(&l, &mut link, true);
        assert_eq!((r.auto_write, r.refreshed, r.waiting), (Some(true), 2, 0));
        assert_eq!(link.sent, [text(&folder("A")), text(&folder("B"))]);
        assert!(l.list().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_setting_is_asked_at_every_check_even_with_nothing_queued_and_nothing_else_is_sent() {
        let (d, l) = queue("empty", &[]);
        let mut link = Fake::new(false);
        let r = run(&l, &mut link, true);
        assert_eq!((r.auto_write, r.waiting, link.calls.clone()), (Some(false), 0, vec!["identify".to_string(), "prefs".to_string()]));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn switched_off_in_the_config_still_asks_but_sends_nothing_and_drops_nothing() {
        let (d, l) = queue("cfgoff", &["A"]);
        let mut link = Fake::new(true);
        let r = run(&l, &mut link, false);
        assert_eq!((r.auto_write, r.refreshed, r.waiting), (Some(true), 0, 1));
        assert!(link.sent.is_empty());
        assert_eq!(l.list().len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_closed_lightcraft_keeps_the_list_counts_nothing_and_says_nothing_about_the_setting() {
        let (d, l) = queue("closed", &["A", "B"]);
        let mut link = Fake::new(true);
        link.identify = Some(LinkError::Unreachable("Connection refused".into()));
        for _ in 0..(MAX_TRIES * 4) {
            let r = run(&l, &mut link, true);
            assert_eq!((r.reachable, r.auto_write, r.waiting, r.refreshed, r.dropped), (Some(false), None, 2, 0, 0));
        }
        assert!(l.list().iter().all(|e| e.attempts == 0));
        // it answers the connection but not the question (slow, or goes away): same
        link.identify = None;
        link.auto_write = Err(LinkError::Unreachable("no answer in time".into()));
        let r = run(&l, &mut link, true);
        assert_eq!((r.reachable, r.auto_write, r.waiting), (Some(false), None, 2));
        // and a folder whose answer never comes keeps itself and the folders behind it
        link.auto_write = Ok(true);
        link = link.answer("A", Err(LinkError::Unreachable("no answer in time".into())));
        link.sent.clear();
        let r = run(&l, &mut link, true);
        assert_eq!((r.reachable, r.waiting, link.sent.len()), (Some(false), 2, 1), "the second folder is not tried after a silence");
        assert!(l.list().iter().all(|e| e.attempts == 0 && !e.given_up));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_folder_that_is_not_in_the_library_is_tried_a_few_times_then_dropped_with_one_log_line_and_the_others_go_on() {
        let (d, l) = queue("notin", &["Unimported", "Good"]);
        let why = format!("{}: no photo in the library was imported from it", text(&folder("Unimported")));
        let mut link = Fake::new(true).answer("Unimported", refused(&why));
        let mut lines = Vec::new();
        for i in 1..=MAX_TRIES {
            let r = run(&l, &mut link, true);
            lines.extend(r.log.iter().filter(|m| m.contains("Unimported")).cloned());
            if i == 1 {
                assert_eq!((r.refreshed, r.waiting, r.dropped), (1, 1, 0), "the good folder is read on the first check, behind the refusal");
            }
        }
        assert!(l.list().is_empty());
        assert_eq!(lines.iter().filter(|m| m.contains("dropped")).count(), 1, "{lines:?}");
        assert!(lines.last().unwrap().contains("not a folder of LightCraft's library"), "{lines:?}");
        // it is not asked again
        link.sent.clear();
        run(&l, &mut link, true);
        assert!(link.sent.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unreadable_sidecars_count_as_a_failed_try_and_the_folder_is_dropped_after_the_limit() {
        let (d, l) = queue("failed", &["Bad"]);
        let failed = SyncReply { read: 2, failed: vec![(text(&folder("Bad").join("x.xmp")), "not an XMP file".into())], ..SyncReply::default() };
        let mut link = Fake::new(true).answer("Bad", Ok(failed));
        for i in 1..MAX_TRIES {
            let r = run(&l, &mut link, true);
            assert_eq!((r.waiting, r.dropped), (1, 0), "try {i}");
            assert_eq!(l.list()[0].attempts, i);
        }
        let r = run(&l, &mut link, true);
        assert_eq!((r.waiting, r.dropped), (0, 1));
        assert!(r.log.iter().any(|m| m.contains("gave up") && m.contains("not an XMP file")), "{:?}", r.log);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn one_check_sends_a_bounded_number_of_folders_one_at_a_time() {
        let names: Vec<String> = (0..MAX_FOLDERS * 2 + 1).map(|i| format!("F{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (d, l) = queue("bounded", &refs);
        let mut link = Fake::new(true);
        let r = run(&l, &mut link, true);
        assert_eq!((r.refreshed, r.waiting, link.sent.len()), (MAX_FOLDERS, MAX_FOLDERS + 1, MAX_FOLDERS));
        assert_eq!(link.sent, names.iter().take(MAX_FOLDERS).map(|n| text(&folder(n))).collect::<Vec<_>>(), "oldest first");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_reply_that_counts_added_or_removed_photos_is_logged_as_unexpected_but_the_folder_is_done() {
        let (d, l) = queue("odd", &["A"]);
        let mut link = Fake::new(true).answer("A", Ok(SyncReply { read: 1, removed: 1, ..SyncReply::default() }));
        let r = run(&l, &mut link, true);
        assert_eq!((r.refreshed, r.waiting), (1, 0));
        assert!(r.log.iter().any(|m| m.contains("unexpected answer")), "{:?}", r.log);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_folder_is_sent_as_the_system_writes_it() {
        let (d, l) = queue("form", &["Trip"]);
        let mut link = Fake::new(true);
        run(&l, &mut link, true);
        assert_eq!(link.sent, [text(&folder("Trip"))]);
        // a folder built from a `/`-separated name is written with the system's separators
        let joined = system_form(&folder("Trip").join("2026/Day 1"));
        assert_eq!(joined, folder("Trip").join("2026").join("Day 1"));
        assert_eq!(folder_of(&joined.join("a.xmp")).unwrap(), joined);
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
#[cfg(windows)]
mod windows_tests {
    use super::*;

    #[test]
    fn drive_and_share_folders_keep_their_backslashes_and_a_mixed_name_is_made_uniform() {
        for (sidecar, folder) in [
            (r"C:\Photos\Trip\a.xmp", r"C:\Photos\Trip"),
            (r"C:\Photos/Trip/a.xmp", r"C:\Photos\Trip"),
            (r"D:\Mixed Case\IMG_1.CR3.xmp", r"D:\Mixed Case"),
            (r"\\nas\photos\Trip\a.xmp", r"\\nas\photos\Trip"),
        ] {
            assert_eq!(folder_of(Path::new(sidecar)).unwrap().to_str().unwrap(), folder, "{sidecar}");
        }
        let d = std::env::temp_dir().join(format!("lc-refresh-win-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        queue_hook(&d)(Path::new(r"C:\Photos\Trip\a.xmp")).unwrap();
        assert_eq!(list(&d).list()[0].path, r"C:\Photos\Trip");
        let _ = std::fs::remove_dir_all(&d);
    }
}
