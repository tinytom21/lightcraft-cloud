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
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
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
}

/// The tray icon's look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Idle,
    Syncing,
    /// Phone edits wait in the inbox.
    PhoneEdits,
    Paused,
    Error,
}

impl TrayState {
    pub const ALL: [TrayState; 5] = [TrayState::Idle, TrayState::Syncing, TrayState::PhoneEdits, TrayState::Paused, TrayState::Error];

    /// Syncing beats everything; then an error, then Pause, then waiting edits.
    pub fn of(s: &AppStatus) -> TrayState {
        if matches!(s.activity, Activity::Syncing { .. }) {
            TrayState::Syncing
        } else if s.last_error.is_some() {
            TrayState::Error
        } else if s.paused {
            TrayState::Paused
        } else if s.inbox_pending.is_some_and(|n| n > 0) {
            TrayState::PhoneEdits
        } else {
            TrayState::Idle
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            TrayState::Idle => "LightCraft Cloud: up to date",
            TrayState::Syncing => "LightCraft Cloud: syncing",
            TrayState::PhoneEdits => "LightCraft Cloud: phone edits are waiting",
            TrayState::Paused => "LightCraft Cloud: paused",
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

    #[test]
    fn the_tray_state_follows_a_priority() {
        let mut s = AppStatus { configured: true, ..AppStatus::default() };
        assert_eq!(s.tray_state(), TrayState::Idle);
        s.inbox_pending = Some(2);
        assert_eq!(s.tray_state(), TrayState::PhoneEdits);
        s.paused = true;
        assert_eq!(s.tray_state(), TrayState::Paused);
        s.last_error = Some("boom".into());
        assert_eq!(s.tray_state(), TrayState::Error);
        s.activity = Activity::Syncing { since: "t".into(), reason: "x".into() };
        assert_eq!(s.tray_state(), TrayState::Syncing);
        s.inbox_pending = Some(0);
        s.activity = Activity::Idle;
        s.last_error = None;
        s.paused = false;
        assert_eq!(s.tray_state(), TrayState::Idle);
        assert_eq!(TrayState::ALL.iter().map(|t| t.tooltip()).collect::<std::collections::HashSet<_>>().len(), 5);
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
