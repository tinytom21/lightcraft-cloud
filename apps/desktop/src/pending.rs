//! The photos that were added to the photo folder and that LightCraft has not been told about yet, and the rules for telling it.
//!
//! * **The list is durable and is written first.** Each photo is a small file in `<state>/lightcraft-pending/`, created (through
//!   a temporary file and a rename) by the home sync's hook while the import is still unfiled, so a crash or a restart cannot
//!   lose it (the sync crate calls the hook again for a photo it finds on disk after a crash). One file per photo means the
//!   command line and the running app can both add to it without one overwriting the other.
//! * **A photo leaves the list only when LightCraft's reply says it has the photo**: it named it as a duplicate, or it counted it
//!   among the ones it added. Its reply names duplicates and failures by path but lists the added ones only by photo id, so a
//!   photo counts as added when it is neither a named duplicate nor a named failure *and* the number of ids equals the number
//!   of such photos. Any other reply (an id count that does not add up, a list missing) keeps the photos for another try; sending
//!   them again is safe, they come back as duplicates.
//! * **Failures wait, quietly and for a while**: each failure is logged with LightCraft's reason and counted against the photo;
//!   at [`MAX_ATTEMPTS`] the photo stops being retried every two minutes and one line goes to Problems (once for the whole batch).
//!   It stays on disk, and is tried again the next time the app starts.
//! * A closed LightCraft or a port that does not answer is not a failure of any photo: nothing is counted, the list is kept.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::lightcraft::{ImportReply, LightCraftLink, LinkError};

/// How many photos go in one `library.import`. LightCraft answers on its UI thread, so a batch is kept small.
pub const BATCH: usize = 10;
/// How many batches one check sends (the rest go at the next check), so one check cannot take long.
pub const MAX_BATCHES: usize = 10;
/// Failed attempts (LightCraft answered and did not take the photo) before the app stops retrying it every check.
pub const MAX_ATTEMPTS: u32 = 10;

/// One photo waiting for LightCraft.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    /// Absolute path of the photo, in the photo folder.
    pub path: String,
    /// ISO time it was queued.
    pub added: String,
    /// How many times LightCraft answered without taking it.
    pub attempts: u32,
    pub last_error: Option<String>,
    /// Retrying stopped (and Problems was told). Cleared when the photo is delivered.
    pub given_up: bool,
}

/// The list, as a folder of one file per photo.
#[derive(Debug, Clone)]
pub struct PendingList {
    dir: PathBuf,
}

impl PendingList {
    pub const DIR: &'static str = "lightcraft-pending";

    pub fn new(state_dir: &Path) -> PendingList {
        PendingList { dir: state_dir.join(Self::DIR) }
    }

    /// The file for a path: its LightCraft content hash (32 hex, so any length of path gives a short, safe name).
    fn file_of(&self, path: &str) -> PathBuf {
        self.dir.join(format!("{}.json", lightcraft_cloud_client::home::content_hash_bytes(path.as_bytes())))
    }

    fn write(&self, e: &Entry) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|err| format!("{}: {err}", self.dir.display()))?;
        let dest = self.file_of(&e.path);
        let tmp = dest.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(e).map_err(|err| err.to_string())?;
        let write = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            std::io::Write::write_all(&mut f, &bytes)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &dest)
        };
        write().map_err(|err| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {err}", dest.display())
        })
    }

    /// Queue a photo. One that is queued already is left as it is (its attempts are kept). The path must be absolute.
    pub fn add(&self, path: &Path, now: &str) -> Result<(), String> {
        if !path.is_absolute() {
            return Err(format!("{} is not an absolute path", path.display()));
        }
        let Some(p) = path.to_str() else { return Err(format!("{} is not valid UTF-8, so LightCraft cannot be told about it", path.display())) };
        if self.file_of(p).exists() {
            return Ok(());
        }
        self.write(&Entry { path: p.to_string(), added: now.to_string(), ..Entry::default() })
    }

    /// Everything queued, oldest first. A file that cannot be read is skipped (and left alone).
    pub fn list(&self) -> Vec<Entry> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut v: Vec<Entry> = rd
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
            .filter_map(|e| serde_json::from_slice::<Entry>(&std::fs::read(e.path()).ok()?).ok())
            .filter(|e| !e.path.is_empty())
            .collect();
        v.sort_by(|a, b| (&a.added, &a.path).cmp(&(&b.added, &b.path)));
        v
    }

    pub fn remove(&self, path: &str) -> Result<(), String> {
        match std::fs::remove_file(self.file_of(path)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {e}", self.file_of(path).display())),
        }
    }

    pub fn update(&self, e: &Entry) -> Result<(), String> {
        self.write(e)
    }
}

/// What LightCraft's reply says about one photo of a batch.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// LightCraft has it (added now, or a duplicate of one it had).
    Done,
    /// It was not taken, and why. The photo stays queued.
    Failed(String),
}

/// Match a reply to the photos that were sent, one verdict per photo, in the order sent.
pub fn classify(sent: &[String], reply: &ImportReply) -> Vec<(String, Verdict)> {
    let named = |p: &String| reply.duplicates.contains(p) || reply.failed.iter().any(|f| &f.0 == p);
    let unnamed = sent.iter().filter(|p| !named(p)).count();
    // the ids of the added photos carry no paths: they can be matched to the photos not named elsewhere only when they number the same
    let adds_up = reply.imported == unnamed;
    sent.iter()
        .map(|p| {
            let verdict = if let Some((_, why)) = reply.failed.iter().find(|f| &f.0 == p) {
                Verdict::Failed(why.clone())
            } else if reply.duplicates.contains(p) || adds_up {
                Verdict::Done
            } else {
                Verdict::Failed(format!(
                    "LightCraft's reply does not say which photos it added ({} added, {} not named as duplicates or failures)",
                    reply.imported, unnamed
                ))
            };
            (p.clone(), verdict)
        })
        .collect()
}

/// What one check did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlushReport {
    /// Photos LightCraft now has (taken off the list).
    pub delivered: usize,
    /// Photos still waiting for LightCraft (not counting the ones given up on).
    pub waiting: usize,
    /// Photos given up on until the app restarts.
    pub given_up: usize,
    /// Whether the port answered as LightCraft: None when it was not asked (nothing to send).
    pub reachable: Option<bool>,
    /// Lines for Problems: a photo that reached the retry limit, once. Not repeated at the next check.
    pub problems: Vec<String>,
    /// Lines for the log.
    pub log: Vec<String>,
}

/// Send what is queued. `retry_given_up`: also try the photos that were given up on (the first check after the app started).
pub fn flush(list: &PendingList, link: &mut dyn LightCraftLink, retry_given_up: bool) -> FlushReport {
    let mut rep = FlushReport::default();
    let mut entries = list.list();
    // (`add` only writes absolute paths; anything else in the folder was not written by this app and is never sent)
    entries.retain(|e| Path::new(&e.path).is_absolute());
    rep.given_up = entries.iter().filter(|e| e.given_up).count();
    let mut todo: Vec<Entry> = entries.iter().filter(|e| retry_given_up || !e.given_up).cloned().collect();
    rep.waiting = entries.iter().filter(|e| !e.given_up).count();
    if todo.is_empty() {
        return rep;
    }
    if let Err(e) = link.identify() {
        rep.reachable = Some(false);
        rep.log.push(format!("lightcraft: not sending {} photos: {}", todo.len(), e.text()));
        return rep;
    }
    rep.reachable = Some(true);
    let mut newly_given_up: Vec<(String, String)> = Vec::new();
    for batch in todo.chunks_mut(BATCH).take(MAX_BATCHES) {
        let sent: Vec<String> = batch.iter().map(|e| e.path.clone()).collect();
        let reply = match link.import_add(&sent) {
            Ok(r) => r,
            Err(e) => {
                // (not a verdict on any photo: nothing is counted against them)
                if e.is_unreachable() || matches!(e, LinkError::NotReady(_)) {
                    rep.reachable = Some(false);
                }
                rep.log.push(format!("lightcraft: import of {} photos did not work: {}", sent.len(), e.text()));
                break;
            }
        };
        for (entry, (_, verdict)) in batch.iter_mut().zip(classify(&sent, &reply)) {
            match verdict {
                Verdict::Done => match list.remove(&entry.path) {
                    Ok(()) => {
                        rep.delivered += 1;
                        if entry.given_up {
                            rep.given_up -= 1;
                        } else {
                            rep.waiting -= 1;
                        }
                        rep.log.push(format!("lightcraft: has {}", entry.path));
                    }
                    Err(e) => rep.log.push(format!("lightcraft: has {} but it could not be taken off the list: {e}", entry.path)),
                },
                Verdict::Failed(why) => {
                    entry.attempts += 1;
                    entry.last_error = Some(why.clone());
                    rep.log.push(format!("lightcraft: did not take {} (attempt {}): {why}", entry.path, entry.attempts));
                    if entry.attempts >= MAX_ATTEMPTS && !entry.given_up {
                        entry.given_up = true;
                        rep.waiting -= 1;
                        rep.given_up += 1;
                        newly_given_up.push((entry.path.clone(), why));
                    }
                    if let Err(e) = list.update(entry) {
                        rep.log.push(format!("lightcraft: cannot save the list: {e}"));
                    }
                }
            }
        }
    }
    if let Some((path, why)) = newly_given_up.first() {
        let name = Path::new(path).file_name().map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned());
        rep.problems.push(if newly_given_up.len() == 1 {
            format!("LightCraft would not add {name} after {MAX_ATTEMPTS} tries ({why}). It is in your photo folder; add it in LightCraft yourself.")
        } else {
            format!(
                "LightCraft would not add {} photos after {MAX_ATTEMPTS} tries (for example {name}: {why}). They are in your photo folder; add them in LightCraft yourself.",
                newly_given_up.len()
            )
        });
    }
    rep
}

/// The hook the sync crate calls for each photo it leaves in the photo folder.
#[cfg(feature = "home")]
pub fn install_hook(home: &mut lightcraft_cloud_client::home::Home, state_dir: &Path, port: u16) {
    if port == 0 {
        return;
    }
    let list = PendingList::new(state_dir);
    home.set_adopted_hook(move |p| list.add(p, &lightcraft_cloud_client::home::utc_iso(std::time::SystemTime::now())));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lc-desktop-pending-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn reply(imported: usize, dups: &[&str], failed: &[(&str, &str)]) -> ImportReply {
        ImportReply {
            imported,
            duplicates: s(dups),
            failed: failed.iter().map(|(p, w)| (p.to_string(), w.to_string())).collect(),
            scanned: None,
        }
    }

    fn verdicts(sent: &[&str], r: &ImportReply) -> Vec<bool> {
        classify(&s(sent), r).into_iter().map(|(_, v)| v == Verdict::Done).collect()
    }

    // ---- reply handling

    #[test]
    fn a_photo_counts_as_delivered_only_when_the_reply_says_so() {
        // all added
        assert_eq!(verdicts(&["/a", "/b"], &reply(2, &[], &[])), [true, true]);
        // a named duplicate is done, a named failure is not, the rest are the added ones
        assert_eq!(verdicts(&["/a", "/dup", "/gone", "/b"], &reply(2, &["/dup"], &[("/gone", "No such file")])), [true, true, false, true]);
        // only duplicates; only failures
        assert_eq!(verdicts(&["/a"], &reply(0, &["/a"], &[])), [true]);
        assert_eq!(verdicts(&["/a"], &reply(0, &[], &[("/a", "unrecognized file format")])), [false]);
    }

    #[test]
    fn an_id_count_that_does_not_add_up_keeps_the_unnamed_photos() {
        // three unnamed photos but one id: which one? none is taken off
        let v = classify(&s(&["/a", "/b", "/c"]), &reply(1, &[], &[]));
        assert!(v.iter().all(|(_, v)| matches!(v, Verdict::Failed(m) if m.contains("does not say which"))));
        // more ids than photos: also not trusted
        assert_eq!(verdicts(&["/a"], &reply(2, &[], &[])), [false]);
        // named ones still count while the unnamed are in doubt
        assert_eq!(verdicts(&["/a", "/b", "/dup"], &reply(0, &["/dup"], &[])), [false, false, true]);
        // a path in the reply that was not sent changes nothing
        assert_eq!(verdicts(&["/a"], &reply(1, &["/other"], &[("/other2", "x")])), [true]);
    }

    // ---- the durable list

    #[test]
    fn the_list_survives_a_restart_is_ordered_and_adding_twice_changes_nothing() {
        let d = temp("list");
        let l = PendingList::new(&d);
        assert!(l.list().is_empty());
        l.add(Path::new("/p/Trip/b.jpg"), "2026-11-01T10:00:02Z").unwrap();
        l.add(Path::new("/p/Trip/a.jpg"), "2026-11-01T10:00:01Z").unwrap();
        let mut e = l.list()[0].clone();
        e.attempts = 3;
        e.last_error = Some("busy".into());
        l.update(&e).unwrap();
        l.add(Path::new("/p/Trip/a.jpg"), "2026-11-01T11:00:00Z").unwrap(); // (again: kept as it was)
        let again = PendingList::new(&d); // (a new process)
        let got = again.list();
        assert_eq!(got.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["/p/Trip/a.jpg", "/p/Trip/b.jpg"]);
        assert_eq!((got[0].attempts, got[0].last_error.as_deref(), got[0].added.as_str()), (3, Some("busy"), "2026-11-01T10:00:01Z"));
        again.remove("/p/Trip/a.jpg").unwrap();
        again.remove("/p/Trip/a.jpg").unwrap(); // (already gone is fine)
        assert_eq!(again.list().len(), 1);
        // a relative path is refused, a damaged file is skipped and never takes the others down
        assert!(l.add(Path::new("Trip/c.jpg"), "t").is_err());
        std::fs::write(d.join(PendingList::DIR).join("damaged.json"), b"{ no").unwrap();
        assert_eq!(l.list().len(), 1);
        assert!(!std::fs::read_dir(d.join(PendingList::DIR)).unwrap().flatten().any(|e| e.file_name().to_string_lossy().ends_with(".tmp")));
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---- flushing

    #[derive(Default)]
    struct FakeLink {
        identify: Option<LinkError>,
        /// One answer per call to `import_add`; the last repeats.
        answers: Vec<Result<ImportReply, LinkError>>,
        identified: usize,
        sent: Vec<Vec<String>>,
    }

    impl LightCraftLink for FakeLink {
        fn identify(&mut self) -> Result<(), LinkError> {
            self.identified += 1;
            self.identify.clone().map_or(Ok(()), Err)
        }
        fn import_add(&mut self, paths: &[String]) -> Result<ImportReply, LinkError> {
            assert!(self.identify.is_none(), "nothing may be sent to a peer that was not identified");
            self.sent.push(paths.to_vec());
            let i = (self.sent.len() - 1).min(self.answers.len() - 1);
            self.answers[i].clone()
        }
    }

    fn queued(tag: &str, paths: &[&str]) -> (PathBuf, PendingList) {
        let d = temp(tag);
        let l = PendingList::new(&d);
        for (i, p) in paths.iter().enumerate() {
            l.add(Path::new(p), &format!("2026-11-01T10:00:{i:02}Z")).unwrap();
        }
        (d, l)
    }

    #[test]
    fn nothing_to_send_means_nobody_is_asked() {
        let (d, l) = queued("empty", &[]);
        let mut link = FakeLink::default();
        let r = flush(&l, &mut link, false);
        assert_eq!((link.identified, r.reachable, r.waiting), (0, None, 0));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_closed_lightcraft_keeps_the_list_and_counts_nothing_against_the_photos() {
        let (d, l) = queued("closed", &["/p/a.jpg", "/p/b.jpg"]);
        let mut link = FakeLink { identify: Some(LinkError::Unreachable("Connection refused".into())), ..FakeLink::default() };
        for _ in 0..(MAX_ATTEMPTS * 3) {
            let r = flush(&l, &mut link, false);
            assert_eq!((r.reachable, r.waiting, r.delivered), (Some(false), 2, 0));
            assert!(r.problems.is_empty());
        }
        assert!(link.sent.is_empty());
        assert!(l.list().iter().all(|e| e.attempts == 0 && !e.given_up));
        // something else on the port: same
        link.identify = Some(LinkError::NotLightCraft("no".into()));
        assert_eq!(flush(&l, &mut link, false).reachable, Some(false));
        assert_eq!(l.list().len(), 2);
        // it opens: the photos go in
        link.identify = None;
        link.answers = vec![Ok(reply(2, &[], &[]))];
        let r = flush(&l, &mut link, false);
        assert_eq!((r.reachable, r.delivered, r.waiting), (Some(true), 2, 0));
        assert!(l.list().is_empty());
        assert_eq!(link.sent, vec![s(&["/p/a.jpg", "/p/b.jpg"])], "oldest first");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_confirmed_photos_leave_the_list_and_batches_are_small() {
        let paths: Vec<String> = (0..25).map(|i| format!("/p/{i:02}.jpg")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let (d, l) = queued("batches", &refs);
        let mut link = FakeLink { answers: vec![Ok(reply(10, &[], &[])), Ok(reply(8, &["/p/11.jpg"], &[("/p/12.jpg", "unrecognized file format")])), Ok(reply(5, &[], &[]))], ..FakeLink::default() };
        let r = flush(&l, &mut link, false);
        assert_eq!(link.sent.iter().map(Vec::len).collect::<Vec<_>>(), [10, 10, 5]);
        assert_eq!(link.identified, 1, "one identification for all the batches");
        assert_eq!((r.delivered, r.waiting), (24, 1));
        let left = l.list();
        assert_eq!(left.len(), 1);
        assert_eq!((left[0].path.as_str(), left[0].attempts, left[0].last_error.as_deref()), ("/p/12.jpg", 1, Some("unrecognized file format")));
        assert!(r.log.iter().any(|m| m.contains("/p/12.jpg") && m.contains("unrecognized file format")), "the reason is logged: {:?}", r.log);
        assert!(r.problems.is_empty(), "one failure is not yet a problem");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn one_check_sends_a_bounded_number_of_batches() {
        let paths: Vec<String> = (0..(BATCH * MAX_BATCHES + 7)).map(|i| format!("/p/{i:03}.jpg")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let (d, l) = queued("bounded", &refs);
        let mut link = FakeLink { answers: vec![Ok(reply(BATCH, &[], &[]))], ..FakeLink::default() };
        let r = flush(&l, &mut link, false);
        assert_eq!((link.sent.len(), r.delivered, r.waiting), (MAX_BATCHES, BATCH * MAX_BATCHES, 7));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failing_photo_is_retried_then_surfaces_in_problems_exactly_once() {
        let (d, l) = queued("giveup", &["/p/bad.jpg", "/p/good.jpg"]);
        let mut link = FakeLink::default();
        let mut problems = Vec::new();
        for i in 1..=MAX_ATTEMPTS + 5 {
            let before = link.sent.len();
            link.answers = vec![Ok(reply(if i == 1 { 1 } else { 0 }, &[], &[("/p/bad.jpg", "No such file or directory (os error 2)")]))];
            let r = flush(&l, &mut link, false);
            problems.extend(r.problems.clone());
            if i < MAX_ATTEMPTS {
                assert_eq!(r.waiting, 1, "attempt {i}");
                assert_eq!(link.sent.len(), before + 1);
            }
            if i > MAX_ATTEMPTS {
                assert_eq!(link.sent.len(), before, "given up: not sent again, nobody asked ({i})");
                assert_eq!((r.waiting, r.given_up), (0, 1));
            }
        }
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("bad.jpg") && problems[0].contains("No such file"), "{problems:?}");
        let e = &l.list()[0];
        assert_eq!((e.path.as_str(), e.attempts, e.given_up), ("/p/bad.jpg", MAX_ATTEMPTS, true));
        // after a restart it gets one more try (LightCraft may see the folder now), and is delivered
        link.answers = vec![Ok(reply(1, &[], &[]))];
        let r = flush(&l, &mut link, true);
        assert_eq!((r.delivered, r.waiting, r.given_up), (1, 0, 0));
        assert!(l.list().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn many_photos_giving_up_together_make_one_line() {
        let (d, l) = queued("many", &["/p/1.jpg", "/p/2.jpg", "/p/3.jpg"]);
        let mut link = FakeLink {
            answers: vec![Ok(reply(0, &[], &[("/p/1.jpg", "denied"), ("/p/2.jpg", "denied"), ("/p/3.jpg", "denied")]))],
            ..FakeLink::default()
        };
        let all: Vec<String> = (0..MAX_ATTEMPTS).flat_map(|_| flush(&l, &mut link, false).problems).collect();
        assert_eq!(all.len(), 1);
        assert!(all[0].contains("3 photos") && all[0].contains("denied"), "{all:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_reply_that_does_not_add_up_and_a_lost_connection_keep_everything() {
        let (d, l) = queued("doubt", &["/p/a.jpg", "/p/b.jpg"]);
        let mut link = FakeLink { answers: vec![Ok(reply(1, &[], &[]))], ..FakeLink::default() };
        let r = flush(&l, &mut link, false);
        assert_eq!((r.delivered, r.waiting), (0, 2));
        assert_eq!(l.list().len(), 2);
        // the connection drops in the middle: no verdicts at all, nothing counted, and the second batch is not tried
        let (d2, l2) = queued("drop", &(0..15).map(|i| format!("/p/{i:02}.jpg")).collect::<Vec<_>>().iter().map(String::as_str).collect::<Vec<_>>());
        let mut link = FakeLink { answers: vec![Ok(reply(10, &[], &[])), Err(LinkError::Unreachable("closed".into()))], ..FakeLink::default() };
        let r = flush(&l2, &mut link, false);
        assert_eq!((r.delivered, r.waiting, r.reachable, link.sent.len()), (10, 5, Some(false), 2));
        assert!(l2.list().iter().all(|e| e.attempts == 0));
        // LightCraft refusing the whole request (not a verdict on photos either)
        let mut link = FakeLink { answers: vec![Err(LinkError::Refused("invalid parameters".into()))], ..FakeLink::default() };
        let r = flush(&l2, &mut link, false);
        assert_eq!((r.delivered, r.waiting), (0, 5));
        assert!(r.log.iter().any(|m| m.contains("invalid parameters")));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&d2);
    }

    #[test]
    fn the_sent_paths_are_the_absolute_paths_in_the_list_and_a_relative_entry_is_dropped() {
        let (d, l) = queued("abs", &["/home/u/Photos/Trip/a.jpg"]);
        std::fs::write(
            d.join(PendingList::DIR).join("0123.json"),
            json!({"path": "relative/b.jpg", "added": "2026-11-01T09:00:00Z"}).to_string(),
        )
        .unwrap();
        let mut link = FakeLink { answers: vec![Ok(reply(1, &[], &[]))], ..FakeLink::default() };
        let r = flush(&l, &mut link, false);
        assert_eq!(link.sent, vec![s(&["/home/u/Photos/Trip/a.jpg"])]);
        assert_eq!(r.delivered, 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
