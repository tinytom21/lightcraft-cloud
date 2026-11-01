//! Live progress of a pass: what the app shows while a long sync runs (a line, a bar, the time left).
//!
//! * [`Progress`]: a small plain snapshot (the stage, how many photos and bytes are done of how many, when the stage began and
//!   when something last finished or moved). `Home` and `Other` hand one to the hook set with `set_progress_hook` after every
//!   change; with no hook nothing is measured or sent, and the sync is exactly what it was.
//! * [`Estimator`]: the time left, from the speed of this stage so far. Pure: the clock is a parameter.
//! * [`describe`]: the words ("Downloading 34 of 261 files · 1.2 of 8.0 GB · about 12 minutes left") and the bar's fill. Pure too.
//!
//! Times are milliseconds since the Unix epoch, so a snapshot can be written to a file and read by another process.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// What a sync is doing now. Several stages have a count (photos), some also a size; the others are only named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    // another PC
    LookingAtLibrary,
    CheckingEdits,
    Downloading,
    LookingForNewPhotos,
    Sending,
    AskingHome,
    // the home PC
    Inbox,
    Importing,
    Checking,
    Reading,
    Previews,
    Thumbnails,
    UploadingOriginals,
    UploadingThumbnails,
    UploadingPreviews,
    Publishing,
    /// A stage this version does not know (a status file from a newer one), or none yet.
    #[default]
    #[serde(other)]
    Working,
}

impl Stage {
    /// The words with a count: "{head} 3 of 12 {tail}" (an empty tail adds nothing), and without one.
    fn words(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Stage::Working => ("Working", "", "Working…"),
            Stage::LookingAtLibrary => ("Looking at the cloud library", "", "Looking at the cloud library…"),
            Stage::CheckingEdits => ("Checking edits of", "files", "Checking edits…"),
            Stage::Downloading => ("Downloading", "files", "Downloading files…"),
            Stage::LookingForNewPhotos => ("Looking at", "new files", "Looking for new photos on this PC…"),
            Stage::Sending => ("Sending", "files to the home PC", "Sending files to the home PC…"),
            Stage::AskingHome => ("Asking the home PC about", "files", "Asking the home PC about the files sent…"),
            Stage::Inbox => ("Applying edits from other devices:", "edits", "Applying edits from other devices…"),
            Stage::Importing => ("Adding", "files from other PCs", "Adding files from other PCs…"),
            Stage::Checking => ("Checking", "files", "Checking files…"),
            Stage::Reading => ("Reading new photo files", "", "Reading new photo files…"),
            Stage::Previews => ("Making previews", "", "Making previews…"),
            Stage::Thumbnails => ("Making thumbnails", "", "Making thumbnails…"),
            Stage::UploadingOriginals => ("Uploading originals", "", "Uploading originals…"),
            Stage::UploadingThumbnails => ("Uploading thumbnails", "", "Uploading thumbnails…"),
            Stage::UploadingPreviews => ("Uploading previews", "", "Uploading previews…"),
            Stage::Publishing => ("Updating the photo list", "", "Updating the photo list…"),
        }
    }

    /// Whether the stage waits on the network (the words of a stall differ).
    pub fn is_network(self) -> bool {
        !matches!(self, Stage::Checking | Stage::Reading | Stage::Previews | Stage::Thumbnails | Stage::LookingForNewPhotos | Stage::Working)
    }

    /// Whether the bytes are worth showing (a stage of whole photos).
    fn shows_bytes(self) -> bool {
        matches!(self, Stage::Downloading | Stage::Sending | Stage::UploadingOriginals | Stage::UploadingThumbnails | Stage::UploadingPreviews)
    }
}

/// A snapshot of where a stage is. Totals of 0 are "not known".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Progress {
    pub stage: Stage,
    pub done_items: u64,
    pub total_items: u64,
    /// Bytes of the finished items.
    pub done_bytes: u64,
    pub total_bytes: u64,
    /// Bytes of the item being transferred now, when that is known cheaply (not counted in `done_bytes`).
    pub partial_bytes: u64,
    /// When the stage began (ms since the epoch).
    pub started_ms: u64,
    /// When an item last finished or its bytes last moved; 0: not yet (the stage's start counts then).
    pub last_item_ms: u64,
    /// Predicted end of the stage (ms since the epoch), set by the app from an [`Estimator`]; None: not known yet.
    pub finish_ms: Option<u64>,
}

impl Progress {
    /// Bytes moved so far, the item in flight included.
    pub fn bytes_so_far(&self) -> u64 {
        let in_flight = if self.total_bytes > 0 { self.partial_bytes.min(self.total_bytes.saturating_sub(self.done_bytes)) } else { 0 };
        self.done_bytes.saturating_add(in_flight)
    }

    /// How much of the stage is done, 0..=1; None when no total is known.
    pub fn fraction(&self) -> Option<f32> {
        if self.total_bytes > 0 {
            Some((self.bytes_so_far() as f64 / self.total_bytes as f64).clamp(0.0, 1.0) as f32)
        } else if self.total_items > 0 {
            Some((self.done_items as f64 / self.total_items as f64).clamp(0.0, 1.0) as f32)
        } else {
            None
        }
    }

    /// The last moment something happened in this stage.
    pub fn last_activity_ms(&self) -> u64 {
        self.last_item_ms.max(self.started_ms)
    }
}

/// Milliseconds since the epoch.
pub fn to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis().min(u64::MAX as u128) as u64)
}

/// Told after every change. Called on the thread that did the work (or one of its helpers), possibly from several threads at once, so
/// it must be quick and must not block; whatever it does, the sync is not affected (a panic in it is caught and ignored).
pub type ProgressHook = Arc<dyn Fn(&Progress) + Send + Sync>;

// ------------------------------------------------------------------ the reporter the sync code uses

type Now = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// What `Home` and `Other` report through. Without a hook every call returns at once.
pub(crate) struct Reporter {
    hook: Option<ProgressHook>,
    now: Now,
    cur: Mutex<Progress>,
}

impl Reporter {
    pub(crate) fn new(hook: Option<ProgressHook>, now: Now) -> Reporter {
        Reporter { hook, now, cur: Mutex::new(Progress::default()) }
    }

    pub(crate) fn hook(&self) -> Option<ProgressHook> {
        self.hook.clone()
    }

    fn edit(&self, f: impl FnOnce(&mut Progress, u64)) {
        let Some(hook) = &self.hook else { return };
        let now = to_ms((self.now)());
        let mut cur = self.cur.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut cur, now);
        let snapshot = *cur;
        // (under the lock, so the snapshots arrive in order)
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(&snapshot)));
    }

    /// A new stage. `total_items` / `total_bytes`: 0 when not known.
    pub(crate) fn stage(&self, stage: Stage, total_items: u64, total_bytes: u64) {
        self.edit(|p, now| *p = Progress { stage, total_items, total_bytes, started_ms: now, ..Progress::default() });
    }

    /// The number of items finished is now `n` (a loop with several ways to the next item says it at the top).
    pub(crate) fn at(&self, n: u64) {
        self.edit(|p, now| {
            if n > p.done_items {
                p.last_item_ms = now;
            }
            p.done_items = n;
        });
    }

    /// One more item finished, of `bytes`. (The bytes of what is in flight are `partial`'s: it keeps them until told otherwise.)
    pub(crate) fn item_done(&self, bytes: u64) {
        self.edit(|p, now| {
            p.done_items += 1;
            p.done_bytes = p.done_bytes.saturating_add(bytes);
            p.last_item_ms = now;
        });
    }

    /// The item in flight has moved `bytes` so far; a change counts as something happening.
    pub(crate) fn partial(&self, bytes: u64) {
        self.edit(|p, now| {
            if bytes != p.partial_bytes {
                p.last_item_ms = now;
            }
            p.partial_bytes = bytes;
        });
    }

    /// Run `f`, and meanwhile (if there is a hook and the remote can say) tell the bytes that are moving inside the file being transferred,
    /// twice a second, from a thread of its own: a big file on a slow link is then seen to be moving, and the bar fills smoothly.
    pub(crate) fn watch<R>(&self, remote: &dyn crate::Remote, f: impl FnOnce() -> R) -> R {
        if self.hook.is_none() || remote.bytes_in_flight().is_none() {
            return f();
        }
        struct Stop<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                let stopped = || stop.load(std::sync::atomic::Ordering::SeqCst);
                while !stopped() {
                    if let Some(b) = remote.bytes_in_flight() {
                        self.partial(b);
                    }
                    for _ in 0..10 {
                        if stopped() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            });
            let _stop = Stop(&stop);
            f()
        })
    }

    /// The stage is over: everything counted.
    pub(crate) fn finish(&self) {
        self.edit(|p, now| {
            p.done_items = p.total_items.max(p.done_items);
            if p.total_bytes > 0 {
                p.done_bytes = p.total_bytes.max(p.done_bytes);
            }
            p.partial_bytes = 0;
            p.last_item_ms = now;
        });
    }
}

// ------------------------------------------------------------------ the time left

/// A stage must have run this long, and moved something, before a time is guessed.
const MIN_ELAPSED_MS: u64 = 10_000;
/// The recent speed counts over this long (the rest of the weight is the speed since the stage began).
const WINDOW_MS: u64 = 120_000;
/// Samples are taken at most this often.
const SAMPLE_MS: u64 = 1_000;

/// The speed of one stage so far, and from it the time left. Feed it every snapshot ([`Estimator::observe`]), ask [`Estimator::finish_ms`].
///
/// The work is measured in bytes when the totals are known (so a few big photos weigh what they are) and in photos otherwise. The speed
/// is half the average since the stage began and half the average over the last two minutes. The predicted end is held until a new
/// guess differs from it by more than a sixth of the time left (and ten seconds), so the shown time counts down instead of jittering.
#[derive(Debug, Default)]
pub struct Estimator {
    stage: Option<(Stage, u64)>,
    samples: VecDeque<(u64, f64)>,
    held: Option<u64>,
}

impl Estimator {
    pub fn new() -> Estimator {
        Estimator::default()
    }

    /// Take a snapshot at `now_ms`.
    pub fn observe(&mut self, p: &Progress, now_ms: u64) {
        if self.stage != Some((p.stage, p.started_ms)) {
            *self = Estimator { stage: Some((p.stage, p.started_ms)), samples: VecDeque::from([(p.started_ms, 0.0)]), held: None };
        }
        let (work, total) = Self::measure(p);
        if self.samples.back().is_none_or(|(t, _)| now_ms >= t + SAMPLE_MS) {
            self.samples.push_back((now_ms, work));
            while self.samples.len() > 2 && self.samples.front().is_some_and(|(t, _)| now_ms.saturating_sub(*t) > WINDOW_MS + SAMPLE_MS) {
                self.samples.pop_front();
            }
        } else if let Some(last) = self.samples.back_mut() {
            last.1 = work; // (the newest sample follows the work, so the final value is never lost)
        }
        let elapsed = now_ms.saturating_sub(p.started_ms);
        let remaining_work = total - work;
        if total <= 0.0 || work <= 0.0 || remaining_work <= 0.0 || elapsed < MIN_ELAPSED_MS {
            if remaining_work <= 0.0 {
                self.held = None;
            }
            return;
        }
        let avg = work / elapsed as f64;
        let recent = {
            let (t0, w0) = self.samples.iter().find(|(t, _)| now_ms.saturating_sub(*t) <= WINDOW_MS).copied().unwrap_or((p.started_ms, 0.0));
            if now_ms > t0 { (work - w0) / (now_ms - t0) as f64 } else { avg }
        };
        let rate = 0.5 * avg + 0.5 * recent.max(0.0);
        if rate <= 0.0 {
            return;
        }
        let guess = now_ms + (remaining_work / rate) as u64;
        self.held = Some(match self.held {
            Some(h) if h.abs_diff(guess) <= (h.saturating_sub(now_ms) / 6).max(10_000) => h,
            _ => guess,
        });
    }

    /// The predicted end of the stage, if there is enough to go on.
    pub fn finish_ms(&self) -> Option<u64> {
        self.held
    }

    /// The work done and the work there is: bytes when known, else photos.
    fn measure(p: &Progress) -> (f64, f64) {
        if p.total_bytes > 0 { (p.bytes_so_far() as f64, p.total_bytes as f64) } else { (p.done_items as f64, p.total_items as f64) }
    }
}

// ------------------------------------------------------------------ the words

/// No finished photo, no moved byte for this long is worth saying so.
pub const STALL_MS: u64 = 120_000;

/// What to show for a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// "Downloading 34 of 261 files · 1.2 of 8.0 GB · about 12 minutes left".
    pub line: String,
    /// A calm note when nothing has moved for a while; the line stays.
    pub note: Option<String>,
    /// The bar's fill; None: not known (show a bar that only says "busy").
    pub fraction: Option<f32>,
}

impl Summary {
    /// Everything in one line, for the tray's tooltip.
    pub fn one_line(&self) -> String {
        match &self.note {
            Some(n) => format!("{}. {n}", self.line),
            None => self.line.clone(),
        }
    }
}

/// How long nothing has happened, if it is long enough to say so: at 2 minutes exactly, and not before.
pub fn stalled_for(p: &Progress, now_ms: u64) -> Option<Duration> {
    let quiet = now_ms.saturating_sub(p.last_activity_ms());
    (quiet >= STALL_MS).then(|| Duration::from_millis(quiet))
}

/// The words for `p` at `now_ms`.
pub fn describe(p: &Progress, now_ms: u64) -> Summary {
    let (head, tail, plain) = p.stage.words();
    let mut line = if p.total_items > 0 {
        let done = p.done_items.min(p.total_items);
        let counted = format!("{head} {done} of {}", p.total_items);
        if tail.is_empty() { counted } else { format!("{counted} {tail}") }
    } else {
        plain.to_string()
    };
    if p.stage.shows_bytes() && p.total_bytes > 0 {
        line += &format!(" · {}", bytes_pair(p.bytes_so_far(), p.total_bytes));
    }
    let stalled = stalled_for(p, now_ms);
    let counted = p.total_items > 0 || p.total_bytes > 0;
    let finished = counted && p.fraction() == Some(1.0);
    if stalled.is_none() && counted && !finished {
        match p.finish_ms {
            Some(f) => line += &format!(" · {}", time_left(Duration::from_millis(f.saturating_sub(now_ms)))),
            None if now_ms.saturating_sub(p.started_ms) >= 4_000 => line += " · working out time left",
            None => {}
        }
    }
    let note = stalled.map(|d| {
        let m = d.as_secs() / 60;
        let n = if m == 1 { "1 minute".to_string() } else { format!("{m} minutes") };
        if p.stage.is_network() {
            format!("No progress for {n}: still trying, the network may be slow or down")
        } else {
            format!("No progress for {n}: still working, this can take a while")
        }
    });
    Summary { line, note, fraction: p.fraction() }
}

/// "less than a minute left", "about 12 minutes left", "about 1 hour 20 minutes left".
pub fn time_left(left: Duration) -> String {
    let secs = left.as_secs();
    if secs < 60 {
        return "less than a minute left".into();
    }
    let minutes = (secs as f64 / 60.0).round() as u64;
    if minutes < 60 {
        return if minutes == 1 { "about 1 minute left".into() } else { format!("about {minutes} minutes left") };
    }
    let minutes = (secs as f64 / 300.0).round() as u64 * 5; // (hours: to the nearest five minutes)
    if minutes >= 24 * 60 {
        return "more than a day left".into();
    }
    let (h, m) = (minutes / 60, minutes % 60);
    let hours = if h == 1 { "1 hour".to_string() } else { format!("{h} hours") };
    match m {
        0 => format!("about {hours} left"),
        m => format!("about {hours} {m} minutes left"),
    }
}

fn unit_of(n: u64) -> (f64, &'static str) {
    let mut f = n as f64;
    let mut unit = "bytes";
    for u in ["KB", "MB", "GB", "TB"] {
        if f < 1024.0 {
            break;
        }
        f /= 1024.0;
        unit = u;
    }
    (f, unit)
}

/// "1.2 of 8.0 GB", "200.0 MB of 1.1 GB": each in its own unit, the unit said once when it is the same.
pub fn bytes_pair(done: u64, total: u64) -> String {
    let (d, du) = unit_of(done);
    let (t, tu) = unit_of(total);
    let one = |v: f64, u: &str| if u == "bytes" { (v as u64).to_string() } else { format!("{v:.1}") };
    if du == tu { format!("{} of {} {tu}", one(d, du), one(t, tu)) } else { format!("{} {du} of {} {tu}", one(d, du), one(t, tu)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1000;
    const MIN: u64 = 60 * SEC;

    fn p(stage: Stage, done: u64, total: u64, done_b: u64, total_b: u64, started: u64, last: u64) -> Progress {
        Progress { stage, done_items: done, total_items: total, done_bytes: done_b, total_bytes: total_b, started_ms: started, last_item_ms: last, ..Progress::default() }
    }

    #[test]
    fn time_left_is_rounded_the_way_a_person_says_it() {
        let s = Duration::from_secs;
        assert_eq!(time_left(s(0)), "less than a minute left");
        assert_eq!(time_left(s(59)), "less than a minute left");
        assert_eq!(time_left(s(60)), "about 1 minute left");
        assert_eq!(time_left(s(89)), "about 1 minute left");
        assert_eq!(time_left(s(12 * 60 + 10)), "about 12 minutes left");
        assert_eq!(time_left(s(59 * 60 + 29)), "about 59 minutes left");
        assert_eq!(time_left(s(3600)), "about 1 hour left");
        assert_eq!(time_left(s(80 * 60 + 40)), "about 1 hour 20 minutes left");
        assert_eq!(time_left(s(2 * 3600 + 2 * 60)), "about 2 hours left");
        assert_eq!(time_left(s(23 * 3600 + 58 * 60)), "more than a day left");
    }

    #[test]
    fn sizes_are_said_in_the_units_a_person_reads() {
        assert_eq!(bytes_pair(1288490189, 8589934592), "1.2 of 8.0 GB");
        assert_eq!(bytes_pair(200 << 20, 1181116006), "200.0 MB of 1.1 GB");
        assert_eq!(bytes_pair(0, 5), "0 of 5 bytes");
        assert_eq!(bytes_pair(512, 3 << 20), "512 bytes of 3.0 MB");
        assert_eq!(bytes_pair(0, 3 << 20), "0 bytes of 3.0 MB");
    }

    #[test]
    fn the_line_names_the_stage_and_counts_what_it_knows() {
        let at = |pr: &Progress, now| describe(pr, now).line;
        let mut d = p(Stage::Downloading, 34, 261, 1_288_490_189, 8_589_934_592, 0, 100 * SEC);
        d.finish_ms = Some(MIN * 12 + 100 * SEC);
        assert_eq!(at(&d, 100 * SEC), "Downloading 34 of 261 files · 1.2 of 8.0 GB · about 12 minutes left");
        let s = p(Stage::Sending, 3, 12, 0, 0, 0, 5 * SEC);
        assert_eq!(at(&s, 6 * SEC), "Sending 3 of 12 files to the home PC · working out time left");
        assert_eq!(at(&s, 2 * SEC), "Sending 3 of 12 files to the home PC", "a short job does not flash a guess");
        assert_eq!(at(&p(Stage::LookingAtLibrary, 0, 0, 0, 0, 0, 0), 30 * SEC), "Looking at the cloud library…");
        assert_eq!(at(&p(Stage::Checking, 0, 0, 0, 0, 0, 0), 5 * SEC), "Checking files…");
        assert_eq!(at(&p(Stage::Previews, 12, 30, 0, 0, 0, 0), SEC), "Making previews 12 of 30");
        let mut u = p(Stage::UploadingOriginals, 5, 30, 200 << 20, 1_181_116_006, 0, 0);
        u.finish_ms = Some(3 * MIN);
        assert_eq!(at(&u, 0), "Uploading originals 5 of 30 · 200.0 MB of 1.1 GB · about 3 minutes left");
        // unknown sizes: no byte part; a count that overran its total is clamped
        assert_eq!(at(&p(Stage::Downloading, 9, 5, 0, 0, 0, 0), SEC), "Downloading 5 of 5 files");
        // nothing known at all
        assert_eq!(at(&Progress::default(), 0), "Working…");
    }

    #[test]
    fn a_finished_stage_has_no_time_left_to_show() {
        let mut d = p(Stage::Downloading, 5, 5, 10, 10, 0, SEC);
        d.finish_ms = Some(5 * MIN);
        assert_eq!(describe(&d, 20 * SEC).line, "Downloading 5 of 5 files · 10 of 10 bytes");
        assert_eq!(describe(&d, 20 * SEC).fraction, Some(1.0));
    }

    #[test]
    fn nothing_moving_for_two_minutes_is_said_calmly_and_not_before() {
        let d = p(Stage::Downloading, 3, 10, 30, 100, 0, 10 * SEC);
        let at = |now| describe(&d, now);
        assert_eq!(at(10 * SEC + STALL_MS - 1).note, None);
        let n = at(10 * SEC + STALL_MS).note.unwrap();
        assert_eq!(n, "No progress for 2 minutes: still trying, the network may be slow or down");
        assert_eq!(at(10 * SEC + 3 * MIN + 59 * SEC).note.unwrap(), "No progress for 3 minutes: still trying, the network may be slow or down");
        assert!(at(10 * SEC + STALL_MS).one_line().starts_with("Downloading 3 of 10 files"));
        // a stall hides the time left, which would be a guess on nothing
        let mut with_eta = d;
        with_eta.finish_ms = Some(10 * MIN);
        assert!(!describe(&with_eta, 10 * SEC + STALL_MS).line.contains("left"));
        // a stage that has not finished anything yet counts from its start
        let fresh = p(Stage::LookingAtLibrary, 0, 0, 0, 0, 7 * SEC, 0);
        assert_eq!(describe(&fresh, 7 * SEC + STALL_MS - 1).note, None);
        assert!(describe(&fresh, 7 * SEC + STALL_MS).note.is_some());
        // local work says it differently
        let local = p(Stage::Previews, 3, 10, 0, 0, 0, 0);
        assert!(describe(&local, STALL_MS).note.unwrap().contains("still working"));
        // a byte that moved inside a big file is activity (see Reporter::partial)
        assert_eq!(stalled_for(&Progress { last_item_ms: 119 * SEC, ..d }, 120 * SEC), None);
    }

    #[test]
    fn the_bar_fills_by_bytes_when_sizes_are_known_else_by_photos_and_is_unknown_with_no_total() {
        let mut a = p(Stage::Downloading, 1, 10, 100, 1000, 0, 0);
        assert_eq!(a.fraction(), Some(0.1));
        a.partial_bytes = 400;
        assert_eq!(a.fraction(), Some(0.5), "the file in flight counts");
        a.partial_bytes = 50_000;
        assert_eq!(a.fraction(), Some(1.0), "but never past the end");
        assert_eq!(p(Stage::Downloading, 3, 12, 0, 0, 0, 0).fraction(), Some(0.25));
        assert_eq!(p(Stage::Downloading, 3, 0, 0, 0, 0, 0).fraction(), None);
        assert_eq!(p(Stage::Downloading, 0, 0, 0, 0, 0, 0).fraction(), None, "zero totals are unknown, not complete");
    }

    /// Feed an estimator a job that does `rate` bytes per second.
    fn run(est: &mut Estimator, rate: u64, total: u64, from_s: u64, to_s: u64) -> Progress {
        let mut last = Progress::default();
        for s in from_s..=to_s {
            last = p(Stage::Downloading, s * rate / 1000, total / 1000, s * rate, total, 0, s * SEC);
            est.observe(&last, s * SEC);
        }
        last
    }

    #[test]
    fn the_time_left_waits_for_ten_seconds_and_some_data() {
        let mut e = Estimator::new();
        run(&mut e, 1000, 100_000, 0, 9);
        assert_eq!(e.finish_ms(), None, "9 s in");
        let mut e = Estimator::new();
        e.observe(&p(Stage::Downloading, 0, 10, 0, 100, 0, 0), 30 * SEC);
        assert_eq!(e.finish_ms(), None, "nothing done yet");
        let mut e = Estimator::new();
        run(&mut e, 1000, 100_000, 0, 10);
        let f = e.finish_ms().unwrap();
        assert!(f.abs_diff(100 * SEC) < 2 * SEC, "100 s job: {f}");
    }

    #[test]
    fn a_steady_job_counts_down_without_jitter() {
        let mut e = Estimator::new();
        let mut shown = Vec::new();
        for s in 10..=90 {
            // (a speed that wobbles by a third each second)
            let rate = if s % 2 == 0 { 1300 } else { 800 };
            let _ = rate;
            let done = s * 1000 + if s % 2 == 0 { 300 } else { 0 };
            e.observe(&p(Stage::Downloading, 0, 100, done, 100_000, 0, s * SEC), s * SEC);
            shown.push(e.finish_ms().unwrap());
        }
        let changes = shown.windows(2).filter(|w| w[0] != w[1]).count();
        assert!(changes <= 3, "the predicted end moved {changes} times in 80 refreshes: {shown:?}");
        assert!(shown.iter().all(|f| f.abs_diff(100 * SEC) < 15 * SEC), "{shown:?}");
    }

    #[test]
    fn a_job_that_slows_down_or_grows_is_followed() {
        let mut e = Estimator::new();
        run(&mut e, 1000, 1_000_000, 0, 60);
        let fast = e.finish_ms().unwrap();
        // the link slows to a tenth: after two more minutes the end has moved well out
        let mut done = 60_000u64;
        for s in 61..=200 {
            done += 100;
            e.observe(&p(Stage::Downloading, 0, 1000, done, 1_000_000, 0, s * SEC), s * SEC);
        }
        assert!(e.finish_ms().unwrap() > fast + 1000 * SEC, "{} vs {fast}", e.finish_ms().unwrap());
        // the job grows (more photos found): the end moves out by about as much
        let mut e = Estimator::new();
        run(&mut e, 1000, 100_000, 0, 20);
        let before = e.finish_ms().unwrap();
        e.observe(&p(Stage::Downloading, 0, 1000, 21_000, 200_000, 0, 21 * SEC), 21 * SEC);
        assert!(e.finish_ms().unwrap() > before + 50 * SEC);
        // the job shrinks below what is done: nothing left, no guess
        e.observe(&p(Stage::Downloading, 0, 1000, 21_000, 20_000, 0, 22 * SEC), 22 * SEC);
        assert_eq!(e.finish_ms(), None);
    }

    #[test]
    fn photos_are_counted_when_sizes_are_not_known_and_a_new_stage_starts_over() {
        let mut e = Estimator::new();
        for s in 1..=20 {
            e.observe(&p(Stage::Sending, s / 2, 100, 0, 0, 0, s * SEC), s * SEC);
        }
        let f = e.finish_ms().unwrap();
        assert!(f.abs_diff(200 * SEC) < 20 * SEC, "{f}"); // (half a photo a second: 200 s in all)
        e.observe(&p(Stage::Sending, 11, 100, 0, 0, 25 * SEC, 25 * SEC), 26 * SEC); // (a stage that began later: new)
        assert_eq!(e.finish_ms(), None);
        // zero totals never divide
        let mut z = Estimator::new();
        z.observe(&p(Stage::Sending, 0, 0, 0, 0, 0, 0), 60 * SEC);
        assert_eq!(z.finish_ms(), None);
    }

    #[test]
    fn an_old_status_without_the_new_fields_or_with_a_stage_from_the_future_still_reads() {
        let old: Progress = serde_json::from_str("{}").unwrap();
        assert_eq!(old, Progress::default());
        let newer: Progress = serde_json::from_str(r#"{"stage":"reticulating","done_items":4}"#).unwrap();
        assert_eq!((newer.stage, newer.done_items), (Stage::Working, 4));
        let back: Progress = serde_json::from_str(&serde_json::to_string(&p(Stage::UploadingOriginals, 1, 2, 3, 4, 5, 6)).unwrap()).unwrap();
        assert_eq!(back, p(Stage::UploadingOriginals, 1, 2, 3, 4, 5, 6));
    }

    #[test]
    fn the_reporter_says_nothing_without_a_hook_and_keeps_its_order_and_survives_a_hook_that_panics() {
        let silent = Reporter::new(None, Arc::new(SystemTime::now));
        silent.stage(Stage::Downloading, 3, 30);
        silent.item_done(10);
        assert!(silent.hook().is_none());

        let seen: Arc<Mutex<Vec<Progress>>> = Arc::default();
        let sink = seen.clone();
        let t = Arc::new(Mutex::new(1_000u64));
        let clock = t.clone();
        let r = Reporter::new(
            Some(Arc::new(move |p| sink.lock().unwrap().push(*p))),
            Arc::new(move || UNIX_EPOCH + Duration::from_millis(*clock.lock().unwrap())),
        );
        r.stage(Stage::Downloading, 3, 30);
        *t.lock().unwrap() = 2_000;
        r.partial(4);
        r.partial(4);
        *t.lock().unwrap() = 3_000;
        r.item_done(10);
        r.item_done(10);
        r.item_done(10);
        r.finish();
        let v = seen.lock().unwrap().clone();
        assert_eq!(v[0], p(Stage::Downloading, 0, 3, 0, 30, 1_000, 0));
        assert_eq!(v[1].last_item_ms, 2_000, "bytes moving inside a file is activity");
        assert_eq!(v[2].last_item_ms, 2_000, "no change, no new activity");
        let last = v.last().unwrap();
        assert_eq!((last.done_items, last.total_items, last.done_bytes, last.total_bytes, last.partial_bytes), (3, 3, 30, 30, 0));
        assert!(v.windows(2).all(|w| w[0].done_items <= w[1].done_items));

        let boom = Reporter::new(Some(Arc::new(|_| panic!("a hook that panics"))), Arc::new(SystemTime::now));
        boom.stage(Stage::Sending, 1, 0);
        boom.item_done(0); // (nothing happens to the caller)
    }
}
