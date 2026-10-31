//! The Status panel: how the app is doing, from the `status.json` the running app writes.

use std::time::SystemTime;

use egui::{Color32, RichText, Ui};

use crate::appstate::{Activity, AppStatus, Notice, relative, upcoming};

/// What the panel shows.
pub struct StatusView<'a> {
    /// None: the app has not written a status yet.
    pub status: Option<&'a AppStatus>,
    /// Whether the app is running (its instance lock is held).
    pub running: bool,
    pub now: SystemTime,
}

const OK: Color32 = Color32::from_rgb(0x2e, 0x9d, 0x5b);
const WARN: Color32 = Color32::from_rgb(0xd9, 0x8a, 0x1f);
const BAD: Color32 = Color32::from_rgb(0xd0, 0x3b, 0x3b);

fn row(ui: &mut Ui, label: &str, value: impl Into<String>) {
    ui.label(RichText::new(label).weak());
    ui.label(value.into());
    ui.end_row();
}

fn list(ui: &mut Ui, title: &str, empty: &str, items: &[Notice], now: SystemTime, color: Option<Color32>) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).strong());
    if items.is_empty() {
        ui.label(RichText::new(empty).weak());
        return;
    }
    for n in items.iter().take(8) {
        let text = format!("{}  {}", relative(&n.when, now), n.text);
        match color {
            Some(c) => ui.colored_label(c, text),
            None => ui.label(text),
        };
    }
}

pub fn show(ui: &mut Ui, v: &StatusView) {
    ui.heading("Status");
    ui.add_space(6.0);
    let Some(s) = v.status else {
        ui.label(if v.running { "The app is starting; nothing to show yet." } else { "The app has not run yet." });
        return;
    };
    if !v.running {
        ui.colored_label(WARN, "The app is not running. Start it to keep the cloud library up to date.");
    }
    if !s.configured {
        ui.colored_label(WARN, "Not set up yet: choose the photo folder and where to store the library in Settings.");
    }
    let (state_text, color) = match (&s.activity, s.paused, &s.last_error) {
        (Activity::Syncing { reason, .. }, _, _) => (format!("Syncing now ({reason})"), WARN),
        (_, _, Some(_)) => ("The last sync failed".to_string(), BAD),
        (_, true, _) => ("Paused".to_string(), WARN),
        _ => ("Up to date".to_string(), OK),
    };
    ui.colored_label(color, RichText::new(state_text).strong());
    ui.add_space(6.0);
    egui::Grid::new("status_grid").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
        match &s.last_sync {
            Some(l) => {
                row(ui, "Last sync", format!("{} ({})", relative(&l.when, v.now), l.result));
                row(ui, "Photos published", l.photos.to_string());
                let mut edits = format!("{} in the last sync", l.applied + l.merged);
                if l.merged > 0 {
                    edits += &format!(" ({} merged with PC changes)", l.merged);
                }
                if l.kept > 0 {
                    edits += &format!(", {} kept off by a newer PC sidecar", l.kept);
                }
                row(ui, "Phone edits applied", format!("{edits}; {} in all", s.edits_applied));
            }
            None => {
                row(ui, "Last sync", "never");
                row(ui, "Photos published", "0");
                row(ui, "Phone edits applied", format!("{} in all", s.edits_applied));
            }
        }
        row(
            ui,
            "Inbox",
            match s.inbox_pending {
                Some(0) => "no phone edits waiting".to_string(),
                Some(1) => "1 phone edit waiting".to_string(),
                Some(n) => format!("{n} phone edits waiting"),
                None => "not checked yet".to_string(),
            },
        );
        row(
            ui,
            "Next sync",
            match (&s.next_sync, s.paused, s.configured) {
                (_, _, false) => "after Setup".to_string(),
                (_, true, _) => "paused (Sync now still works)".to_string(),
                (_, _, _) if matches!(s.activity, Activity::Syncing { .. }) => "now (syncing)".to_string(),
                (Some(t), _, _) => upcoming(t, v.now),
                (None, _, _) => "not scheduled".to_string(),
            },
        );
        if let Some(p) = &s.last_purge {
            row(ui, "History purge", relative(p, v.now));
        }
    });
    list(ui, "Newest wins", "No conflicts between phone and PC edits.", &s.notices, v.now, None);
    list(ui, "Problems", "None.", &s.errors, v.now, Some(BAD));
}
