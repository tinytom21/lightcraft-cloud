//! What the always-running app remembers and shows, in the state folder:
//!
//! ```text
//! <state>/app.json      Pause (persisted), when the last purge ran
//! <state>/status.json   what the windows and the tray show: written by the app after every change, read by the windows
//! ```
//!
//! The sync's own state (`state.json`, sidecar versions, history) is the sync crate's.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::clock::parse_rfc3339;

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    lightcraft_cloud_client::replace_file(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Settings of the app's running that do not belong in the config file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Persisted {
    pub paused: bool,
    /// ISO time of the last purge (done, or refused for want of a backup).
    pub last_purge: Option<String>,
}

impl Persisted {
    fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("app.json")
    }

    /// A missing or damaged file is the defaults.
    pub fn load(state_dir: &Path) -> Persisted {
        std::fs::read(Self::path(state_dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, state_dir: &Path) -> Result<(), String> {
        write_atomic(&Self::path(state_dir), &serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Activity {
    #[default]
    Idle,
    Syncing {
        since: String,
        reason: String,
    },
    /// Photos imported on this PC are being sent to the home PC (a short, bounded job).
    Uploading {
        since: String,
    },
}

/// A line of the Status window's lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub when: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastSync {
    pub when: String,
    pub ok: bool,
    /// "ok", "ok, 2 photos failed", or the error.
    pub result: String,
    /// Photos in the published manifest.
    pub photos: usize,
    pub applied: usize,
    pub merged: usize,
    /// Phone edits not applied because the PC's sidecar was newer.
    pub kept: usize,
}

/// What the other-PC role shows in Status (None on the home PC).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OtherView {
    /// Photos of the chosen folders that are on this PC.
    pub photos: usize,
    /// Photos of the chosen folders that are not here yet.
    pub waiting_to_download: usize,
    /// Edits made here that are not at the home PC yet.
    pub edits_waiting: usize,
    /// Edits sent that the home PC has not merged yet.
    pub held_back: usize,
    /// Photos this PC received from the home PC since the app was first started.
    pub received: usize,
    /// Photos imported on this PC that are not at the home PC yet (not sent).
    pub uploads_waiting: usize,
    /// Their size in bytes.
    pub uploads_bytes: u64,
    /// Photos sent whose answer from the home PC has not been seen yet.
    pub awaiting_home: usize,
    /// Photos the home PC refused that the user has not dealt with (Try again / Keep on this PC only). Counted until then, so the
    /// state line never reads "Up to date" while one is left.
    pub rejected: usize,
    /// Photos that were in the folder when this PC first looked and are not in the cloud library: not sent unless the user says so.
    pub held: usize,
    /// How many of the photos that were there are not looked at yet (`held` may still grow).
    pub held_unchecked: usize,
}

/// Everything the windows and the tray show.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppStatus {
    /// ISO time of the last write.
    pub updated: String,
    /// False until there is a config with a photo folder and a place to publish to: the app opens Setup instead of syncing.
    pub configured: bool,
    pub paused: bool,
    pub activity: Activity,
    pub last_sync: Option<LastSync>,
    /// Phone edits applied since the app was first started (merged ones included).
    pub edits_applied: usize,
    /// Phone edits waiting in the remote's inbox, as the last check saw them.
    pub inbox_pending: Option<usize>,
    /// Photos other PCs uploaded that wait on the remote to be added to the photo folder, as the last check saw them.
    pub imports_pending: Option<usize>,
    /// Photos from other PCs added to the photo folder since the app was first started.
    pub photos_imported: usize,
    /// ... and by the last sync.
    pub last_imported: usize,
    /// The port of LightCraft's control channel the app uses; 0: that is switched off.
    pub lightcraft_port: u16,
    /// Photos added to the photo folder that LightCraft has not been told about yet (given-up ones not counted).
    pub lightcraft_waiting: usize,
    /// Whether LightCraft answered at the last try; None: it was not asked (nothing was waiting) or not yet.
    pub lightcraft_reachable: Option<bool>,
    /// Photos LightCraft would not add after several tries (they are in the photo folder; the user adds them).
    pub lightcraft_given_up: usize,
    /// What is wrong right now, one line each: the problems the last sync found. Unlike the Problems list (a history), it clears when
    /// a sync finds nothing wrong, and it is what keeps the state line from saying "Up to date".
    pub active_problems: Vec<String>,
    /// The other-PC role's numbers (None on the home PC).
    pub other: Option<OtherView>,
    /// The cloud library could not be reached at the last try (calm: tried again later).
    pub offline: bool,
    /// ISO time of the next scheduled sync (none while paused or not set up).
    pub next_sync: Option<String>,
    pub last_purge: Option<String>,
    /// "Newest wins" decisions, newest first.
    pub notices: Vec<Notice>,
    /// What went wrong, newest first.
    pub errors: Vec<Notice>,
    /// The error the last sync ended with; cleared by a sync that works.
    pub last_error: Option<String>,
}

/// How many lines the lists keep.
pub const KEEP_LINES: usize = 20;

impl AppStatus {
    fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("status.json")
    }

    pub fn load(state_dir: &Path) -> Option<AppStatus> {
        std::fs::read(Self::path(state_dir)).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }

    pub fn save(&self, state_dir: &Path) -> Result<(), String> {
        write_atomic(&Self::path(state_dir), &serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?)
    }

    pub fn push_notice(&mut self, when: &str, text: impl Into<String>) {
        self.notices.insert(0, Notice { when: when.into(), text: text.into() });
        self.notices.truncate(KEEP_LINES);
    }

    pub fn push_error(&mut self, when: &str, text: impl Into<String>) {
        self.errors.insert(0, Notice { when: when.into(), text: text.into() });
        self.errors.truncate(KEEP_LINES);
    }

    pub fn tray_state(&self) -> TrayState {
        TrayState::of(self)
    }

    /// Everything that is waiting, in the words the state line uses (empty: nothing is).
    pub fn waiting_items(&self) -> Vec<String> {
        let photos = |n: usize| if n == 1 { "1 photo".to_string() } else { format!("{n} photos") };
        let edits = |n: usize| if n == 1 { "1 edit".to_string() } else { format!("{n} edits") };
        let mut v = Vec::new();
        if let Some(o) = &self.other {
            if o.waiting_to_download > 0 {
                v.push(format!("{} waiting to download", photos(o.waiting_to_download)));
            }
            if o.uploads_waiting > 0 {
                v.push(format!("{} waiting to upload", photos(o.uploads_waiting)));
            }
            if o.awaiting_home > 0 {
                v.push(format!("{} sent, waiting for the home PC", photos(o.awaiting_home)));
            }
            if o.edits_waiting > 0 {
                v.push(format!("{} waiting to send", edits(o.edits_waiting)));
            }
            if o.held_back > 0 {
                v.push(format!("{} waiting to be merged at the home PC", edits(o.held_back)));
            }
        } else {
            if let Some(n) = self.inbox_pending.filter(|n| *n > 0) {
                v.push(format!("{} waiting", if n == 1 { "1 phone edit".to_string() } else { format!("{n} phone edits") }));
            }
            if let Some(n) = self.imports_pending.filter(|n| *n > 0) {
                v.push(format!("{} from other PCs waiting to be added", photos(n)));
            }
        }
        if self.lightcraft_port != 0 && self.lightcraft_waiting > 0 {
            v.push(format!("{} waiting for LightCraft", photos(self.lightcraft_waiting)));
        }
        v
    }

    /// Photos the home PC refused and nobody has dealt with.
    pub fn refused(&self) -> usize {
        self.other.as_ref().map_or(0, |o| o.rejected)
    }

    /// Everything that is wrong right now.
    pub fn problem_count(&self) -> usize {
        self.active_problems.len() + usize::from(self.lightcraft_given_up > 0 && self.lightcraft_port != 0) + usize::from(self.refused() > 0)
    }

    /// What the state line says and how worried to be. **Only a truly settled state is [`Tone::Ok`] ("Up to date")**: not while a sync
    /// or an upload runs, anything waits (photos to download, to upload, sent and not answered, edits, phone edits, imports, photos for
    /// LightCraft), a problem is active, the cloud library cannot be reached, it is paused or not set up, or nothing has synced yet.
    pub fn health(&self) -> Health {
        let h = |kind, tone, text: String| Health { kind, tone, text };
        match &self.activity {
            Activity::Syncing { reason, .. } => return h(HealthKind::Syncing, Tone::Busy, format!("Syncing now ({reason})")),
            Activity::Uploading { .. } => return h(HealthKind::Uploading, Tone::Busy, "Sending photos to the home PC".to_string()),
            Activity::Idle => {}
        }
        if self.last_error.is_some() {
            return h(HealthKind::Failed, Tone::Bad, "The last sync failed".to_string());
        }
        if !self.configured {
            return h(HealthKind::NotSetUp, Tone::Wait, "Waiting for setup".to_string());
        }
        if self.paused {
            return h(HealthKind::Paused, Tone::Paused, "Paused".to_string());
        }
        if self.offline {
            return h(HealthKind::Offline, Tone::Offline, "Offline: will try again".to_string());
        }
        let problems = self.problem_count();
        if problems > 0 {
            let only_refused = self.refused() > 0 && problems == 1;
            let text = if only_refused {
                if self.refused() == 1 { "1 photo the home PC refused".to_string() } else { format!("{} photos the home PC refused", self.refused()) }
            } else if problems == 1 { "1 problem needs attention".to_string() } else { format!("{problems} problems need attention") };
            return h(HealthKind::Problem, Tone::Bad, text);
        }
        let waiting = self.waiting_items();
        if !waiting.is_empty() {
            return h(HealthKind::Waiting, Tone::Wait, waiting.join("; "));
        }
        if self.last_sync.is_none() {
            return h(HealthKind::NotSynced, Tone::Wait, "Waiting for the first sync".to_string());
        }
        h(HealthKind::UpToDate, Tone::Ok, "Up to date".to_string())
    }
}

/// How the state line is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Settled: green.
    Ok,
    /// Working: blue.
    Busy,
    /// Waiting for something: amber.
    Wait,
    /// The cloud library cannot be reached: calm light grey (the Problems list has the one amber line).
    Offline,
    /// Paused by the user: grey.
    Paused,
    /// Needs the user, or failed: red.
    Bad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthKind {
    Syncing,
    Uploading,
    Failed,
    NotSetUp,
    Paused,
    Offline,
    Problem,
    Waiting,
    NotSynced,
    UpToDate,
}

/// The state line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub kind: HealthKind,
    pub tone: Tone,
    pub text: String,
}

/// The tray icon's look. Some states share an icon (the picture is one of five) and differ in the tooltip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Idle,
    Syncing,
    /// Photos are being sent to the home PC (the syncing icon).
    Uploading,
    /// Phone edits wait in the inbox (home PC).
    PhoneEdits,
    /// Something waits, or the cloud library cannot be reached (the amber icon).
    Waiting,
    Paused,
    /// A problem needs the user (the red icon).
    Problem,
    Error,
}

impl TrayState {
    pub const ALL: [TrayState; 8] =
        [TrayState::Idle, TrayState::Syncing, TrayState::Uploading, TrayState::PhoneEdits, TrayState::Waiting, TrayState::Paused, TrayState::Problem, TrayState::Error];

    /// The icon this state is drawn with: one of the five pictures.
    pub fn icon(self) -> TrayState {
        match self {
            TrayState::Uploading => TrayState::Syncing,
            TrayState::Waiting => TrayState::PhoneEdits,
            TrayState::Problem => TrayState::Error,
            s => s,
        }
    }

    /// Syncing beats everything; then an upload, an error, Pause, a problem, and whatever waits. Idle only when truly settled.
    pub fn of(s: &AppStatus) -> TrayState {
        let hl = s.health();
        match hl.kind {
            HealthKind::Syncing => TrayState::Syncing,
            HealthKind::Uploading => TrayState::Uploading,
            HealthKind::Failed => TrayState::Error,
            HealthKind::Paused => TrayState::Paused,
            HealthKind::Problem => TrayState::Problem,
            HealthKind::Offline | HealthKind::NotSetUp | HealthKind::NotSynced => TrayState::Waiting,
            HealthKind::Waiting if s.other.is_none() && s.waiting_items().len() == 1 && s.inbox_pending.is_some_and(|n| n > 0) => TrayState::PhoneEdits,
            HealthKind::Waiting => TrayState::Waiting,
            HealthKind::UpToDate => TrayState::Idle,
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            TrayState::Idle => "LightCraft Cloud: up to date",
            TrayState::Syncing => "LightCraft Cloud: syncing",
            TrayState::Uploading => "LightCraft Cloud: sending photos to the home PC",
            TrayState::PhoneEdits => "LightCraft Cloud: phone edits are waiting",
            TrayState::Waiting => "LightCraft Cloud: something is waiting (see Status)",
            TrayState::Paused => "LightCraft Cloud: paused",
            TrayState::Problem => "LightCraft Cloud: a problem needs attention (see Status)",
            TrayState::Error => "LightCraft Cloud: the last sync failed",
        }
    }
}

/// "just now", "5 min ago", "in 12 min", "2 h ago", "3 days ago" for an ISO time (UTC, as the app writes them).
pub fn relative(iso: &str, now: SystemTime) -> String {
    let Some(t) = parse_rfc3339(iso) else { return iso.to_string() };
    let (secs, future) = match now.duration_since(t) {
        Ok(d) => (d.as_secs(), false),
        Err(e) => (e.duration().as_secs(), true),
    };
    let span = match secs {
        0..=44 => return "just now".to_string(),
        45..=3_599 => format!("{} min", secs.div_ceil(60).max(1)),
        3_600..=86_399 => format!("{} h", secs / 3_600),
        _ => format!("{} days", secs / 86_400),
    };
    if future { format!("in {span}") } else { format!("{span} ago") }
}

/// For a time that is still to come: "now" once it has passed (or while it is being served), else "in 12 min". Never a time in
/// the past: a sync that is running or overdue is happening now, not "3 min ago".
pub fn upcoming(iso: &str, now: SystemTime) -> String {
    match parse_rfc3339(iso) {
        Some(t) if t > now => relative(iso, now),
        Some(_) => "now".to_string(),
        None => iso.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lc-desktop-appstate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn pause_survives_a_restart_and_a_damaged_file_is_the_defaults() {
        let d = temp("a");
        assert_eq!(Persisted::load(&d), Persisted::default());
        Persisted { paused: true, last_purge: Some("2026-11-01T00:00:00Z".into()) }.save(&d).unwrap();
        assert_eq!(Persisted::load(&d), Persisted { paused: true, last_purge: Some("2026-11-01T00:00:00Z".into()) });
        std::fs::write(d.join("app.json"), b"{ not json").unwrap();
        assert_eq!(Persisted::load(&d), Persisted::default());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn status_round_trips_and_keeps_a_bounded_list() {
        let d = temp("b");
        assert!(AppStatus::load(&d).is_none());
        let mut s = AppStatus {
            configured: true,
            edits_applied: 4,
            activity: Activity::Syncing { since: "t".into(), reason: "timer".into() },
            ..AppStatus::default()
        };
        for i in 0..30 {
            s.push_notice("t", format!("n{i}"));
            s.push_error("t", format!("e{i}"));
        }
        s.save(&d).unwrap();
        let back = AppStatus::load(&d).unwrap();
        assert_eq!(back, s);
        assert_eq!((back.notices.len(), back.notices[0].text.as_str(), back.errors[KEEP_LINES - 1].text.as_str()), (KEEP_LINES, "n29", "e10"));
        let _ = std::fs::remove_dir_all(&d);
    }

    fn settled() -> AppStatus {
        AppStatus {
            configured: true,
            last_sync: Some(LastSync { when: "2026-11-01T11:55:00Z".into(), ok: true, result: "ok".into(), photos: 10, applied: 0, merged: 0, kept: 0 }),
            ..AppStatus::default()
        }
    }

    fn other_settled() -> AppStatus {
        AppStatus { other: Some(OtherView { photos: 10, ..OtherView::default() }), ..settled() }
    }

    #[test]
    fn the_tray_state_follows_a_priority() {
        let mut s = settled();
        assert_eq!(s.tray_state(), TrayState::Idle);
        s.inbox_pending = Some(2);
        assert_eq!(s.tray_state(), TrayState::PhoneEdits);
        s.paused = true;
        assert_eq!(s.tray_state(), TrayState::Paused);
        s.last_error = Some("boom".into());
        assert_eq!(s.tray_state(), TrayState::Error);
        s.activity = Activity::Uploading { since: "t".into() };
        assert_eq!(s.tray_state(), TrayState::Uploading);
        s.activity = Activity::Syncing { since: "t".into(), reason: "x".into() };
        assert_eq!(s.tray_state(), TrayState::Syncing);
        s.inbox_pending = Some(0);
        s.activity = Activity::Idle;
        s.last_error = None;
        s.paused = false;
        assert_eq!(s.tray_state(), TrayState::Idle);
        assert_eq!(TrayState::ALL.iter().map(|t| t.tooltip()).collect::<std::collections::HashSet<_>>().len(), TrayState::ALL.len());
        // the new states use the pictures that exist
        assert_eq!((TrayState::Uploading.icon(), TrayState::Waiting.icon(), TrayState::Problem.icon()), (TrayState::Syncing, TrayState::PhoneEdits, TrayState::Error));
    }

    #[test]
    fn only_a_truly_settled_state_reads_up_to_date_in_green() {
        let ok = settled().health();
        assert_eq!((ok.kind, ok.tone, ok.text.as_str()), (HealthKind::UpToDate, Tone::Ok, "Up to date"));
        assert_eq!(other_settled().health().tone, Tone::Ok);
        // every way of not being settled, one by one, on both roles
        let waiting: Vec<(&str, Box<dyn Fn(&mut AppStatus)>, &str)> = vec![
            ("download", Box::new(|s| s.other.as_mut().unwrap().waiting_to_download = 12), "12 photos waiting to download"),
            ("upload", Box::new(|s| s.other.as_mut().unwrap().uploads_waiting = 12), "12 photos waiting to upload"),
            ("one upload", Box::new(|s| s.other.as_mut().unwrap().uploads_waiting = 1), "1 photo waiting to upload"),
            ("home answer", Box::new(|s| s.other.as_mut().unwrap().awaiting_home = 3), "3 photos sent, waiting for the home PC"),
            ("edits", Box::new(|s| s.other.as_mut().unwrap().edits_waiting = 2), "2 edits waiting to send"),
            ("merge", Box::new(|s| s.other.as_mut().unwrap().held_back = 1), "1 edit waiting to be merged at the home PC"),
            ("lightcraft", Box::new(|s| { s.lightcraft_port = 7980; s.lightcraft_waiting = 4; }), "4 photos waiting for LightCraft"),
        ];
        for (what, change, text) in &waiting {
            let mut s = other_settled();
            change(&mut s);
            let h = s.health();
            assert_eq!((h.kind, h.tone, h.text.as_str()), (HealthKind::Waiting, Tone::Wait, *text), "{what}");
            assert_eq!(s.tray_state(), TrayState::Waiting, "{what}");
        }
        // the home PC
        let home: Vec<(&str, Box<dyn Fn(&mut AppStatus)>, &str)> = vec![
            ("phone edits", Box::new(|s| s.inbox_pending = Some(2)), "2 phone edits waiting"),
            ("an import stuck waiting", Box::new(|s| s.imports_pending = Some(1)), "1 photo from other PCs waiting to be added"),
            ("photos for lightcraft", Box::new(|s| { s.lightcraft_port = 7980; s.lightcraft_waiting = 1; }), "1 photo waiting for LightCraft"),
        ];
        for (what, change, text) in &home {
            let mut s = settled();
            change(&mut s);
            let h = s.health();
            assert_eq!((h.kind, h.tone, h.text.as_str()), (HealthKind::Waiting, Tone::Wait, *text), "{what}");
        }
        // LightCraft switched off (port 0): nothing is waiting for it
        let mut s = settled();
        s.lightcraft_waiting = 5;
        assert_eq!(s.health().kind, HealthKind::UpToDate);
        // several things at once
        let mut s = other_settled();
        s.other.as_mut().unwrap().waiting_to_download = 2;
        s.other.as_mut().unwrap().uploads_waiting = 1;
        assert_eq!(s.health().text, "2 photos waiting to download; 1 photo waiting to upload");
    }

    #[test]
    fn a_problem_offline_a_failure_a_pause_and_work_in_progress_each_have_their_own_state() {
        let mut s = other_settled();
        s.active_problems = vec!["x.jpg: no".into()];
        let h = s.health();
        assert_eq!((h.kind, h.tone, h.text.as_str()), (HealthKind::Problem, Tone::Bad, "1 problem needs attention"));
        assert_eq!(s.tray_state(), TrayState::Problem);
        s.active_problems.push("y".into());
        assert_eq!(s.health().text, "2 problems need attention");
        let mut s = settled();
        s.lightcraft_port = 7980;
        s.lightcraft_given_up = 2;
        assert_eq!(s.health().kind, HealthKind::Problem, "photos LightCraft would not add");
        s.lightcraft_port = 0;
        assert_eq!(s.health().kind, HealthKind::UpToDate);
        // the problem outranks a waiting item, offline outranks a problem, a failure outranks both, work in progress outranks all
        let mut s = other_settled();
        s.other.as_mut().unwrap().uploads_waiting = 3;
        s.active_problems = vec!["p".into()];
        assert_eq!(s.health().kind, HealthKind::Problem);
        s.offline = true;
        let h = s.health();
        assert_eq!((h.kind, h.tone, h.text.as_str()), (HealthKind::Offline, Tone::Offline, "Offline: will try again"));
        assert_eq!(s.tray_state(), TrayState::Waiting);
        s.last_error = Some("boom".into());
        assert_eq!((s.health().kind, s.health().tone), (HealthKind::Failed, Tone::Bad));
        s.activity = Activity::Uploading { since: "t".into() };
        assert_eq!((s.health().kind, s.health().tone), (HealthKind::Uploading, Tone::Busy));
        s.activity = Activity::Syncing { since: "t".into(), reason: "timer".into() };
        assert_eq!((s.health().kind, s.health().text.as_str()), (HealthKind::Syncing, "Syncing now (timer)"));
        let mut s = other_settled();
        s.paused = true;
        assert_eq!((s.health().kind, s.health().tone), (HealthKind::Paused, Tone::Paused));
        // not set up, and nothing synced yet, are not "up to date" either
        assert_eq!(AppStatus::default().health().kind, HealthKind::NotSetUp);
        let s = AppStatus { configured: true, ..AppStatus::default() };
        assert_eq!((s.health().kind, s.health().tone), (HealthKind::NotSynced, Tone::Wait));
    }

    #[test]
    fn relative_times_read_naturally() {
        let now = parse_rfc3339("2026-11-01T12:00:00Z").unwrap();
        assert_eq!(relative("2026-11-01T11:59:50Z", now), "just now");
        assert_eq!(relative("2026-11-01T11:55:00Z", now), "5 min ago");
        assert_eq!(relative("2026-11-01T12:12:00Z", now), "in 12 min");
        assert_eq!(relative("2026-11-01T10:00:00Z", now), "2 h ago");
        assert_eq!(relative("2026-10-29T12:00:00Z", now), "3 days ago");
        assert_eq!(relative("not a time", now), "not a time");
        let _ = Duration::ZERO;
    }

    #[test]
    fn an_upcoming_time_never_reads_as_past() {
        let now = parse_rfc3339("2026-11-01T12:00:00Z").unwrap();
        assert_eq!(upcoming("2026-11-01T12:12:00Z", now), "in 12 min");
        assert_eq!(upcoming("2026-11-01T11:57:00Z", now), "now");
        assert_eq!(upcoming("2026-11-01T12:00:00Z", now), "now");
        assert_eq!(upcoming("not a time", now), "not a time");
    }
}
