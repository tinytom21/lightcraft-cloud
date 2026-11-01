//! The Status panel: how the app is doing, from the `status.json` the running app writes.

use std::time::SystemTime;

use egui::{Color32, CornerRadius, Label, Margin, RichText, Sense, Stroke, Ui, vec2};

use crate::appstate::{Activity, AppStatus, Notice, OtherView, relative, upcoming};
use crate::ui::theme;

/// What the panel shows.
pub struct StatusView<'a> {
    /// None: the app has not written a status yet.
    pub status: Option<&'a AppStatus>,
    /// Whether the app is running (its instance lock is held).
    pub running: bool,
    pub now: SystemTime,
}

const LABEL_W: f32 = 150.0;

/// One line of the details: the label in the dim colour on the left, the value beside it (wrapping in the room left).
fn row(ui: &mut Ui, label: &str, value: impl Into<String>) {
    let t = theme::CHARCOAL;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        ui.allocate_ui(vec2(LABEL_W, 0.0), |ui| {
            ui.set_width(LABEL_W);
            ui.add(Label::new(RichText::new(label).color(t.text_dim)).wrap());
        });
        ui.add(Label::new(RichText::new(value.into()).color(t.text)).wrap());
    });
}

/// A list of notices under a section header, newest first as the app wrote them: the time dim, the text beside it.
fn list(ui: &mut Ui, title: &str, empty: &str, items: &[Notice], now: SystemTime, color: Option<Color32>) {
    let t = theme::CHARCOAL;
    theme::gap(ui);
    theme::panel(ui, Some(title), |ui| {
        if items.is_empty() {
            theme::hint(ui, empty);
            return;
        }
        for n in items.iter().take(8) {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                ui.allocate_ui(vec2(LABEL_W, 0.0), |ui| {
                    ui.set_width(LABEL_W);
                    ui.add(Label::new(RichText::new(relative(&n.when, now)).color(t.text_dim)).wrap());
                });
                // (not being able to reach the cloud library is not an error: amber, like Paused)
                let shade = if n.text == crate::service::OFFLINE_LINE { t.caution } else { color.unwrap_or(t.text) };
                ui.add(Label::new(RichText::new(&n.text).color(shade)).wrap());
            });
        }
    });
}

/// A caution bar: an amber stripe and the message.
fn caution(ui: &mut Ui, text: &str) {
    let t = theme::CHARCOAL;
    egui::Frame::new().fill(t.chrome).corner_radius(CornerRadius::same(6)).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            let (dot, _) = ui.allocate_exact_size(vec2(8.0, 18.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.0, t.caution);
            ui.add(Label::new(RichText::new(text).color(t.caution)).wrap());
        });
    });
    ui.add_space(8.0);
}

/// The state line: what the app is doing, large, with a dot in the colour that says whether to worry.
fn state_line(ui: &mut Ui, text: &str, color: Color32) {
    let t = theme::CHARCOAL;
    theme::panel(ui, None, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let (dot, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 5.5, color);
            ui.painter().circle_stroke(dot.center(), 5.5, Stroke::new(1.0, color.gamma_multiply(0.5)));
            ui.add(Label::new(RichText::new(text).font(theme::semibold(ui.ctx(), 17.0)).color(t.text)).wrap());
        });
    });
}

/// The LightCraft row: "Waiting for LightCraft: N photos" while photos wait to be added there (nothing when the feature is off
/// or nothing waits), and a hint when the port has not answered.
pub fn lightcraft_row(s: &AppStatus) -> Option<(String, Option<String>)> {
    if s.lightcraft_port == 0 || s.lightcraft_waiting == 0 {
        return None;
    }
    let n = s.lightcraft_waiting;
    let text = format!("Waiting for LightCraft: {n} {}", if n == 1 { "photo" } else { "photos" });
    let hint = (s.lightcraft_reachable == Some(false)).then(|| {
        format!(
            "LightCraft is not answering on port {}. Open LightCraft with its control port on (see the README); the photos are added when it answers.",
            s.lightcraft_port
        )
    });
    Some((text, hint))
}

/// The rows of another PC: what it holds, what it still has to fetch, what it still has to send.
fn other_rows(ui: &mut Ui, s: &AppStatus, o: &OtherView, now: SystemTime) {
    let photos = |n: usize| if n == 1 { "1 photo".to_string() } else { format!("{n} photos") };
    match &s.last_sync {
        Some(l) => row(ui, "Last sync", format!("{} ({})", relative(&l.when, now), l.result)),
        None => row(ui, "Last sync", "never"),
    }
    row(ui, "Photos on this PC", o.photos.to_string());
    row(ui, "Waiting to download", if o.waiting_to_download == 0 { "nothing".to_string() } else { photos(o.waiting_to_download) });
    row(
        ui,
        "Edits waiting to send",
        match o.edits_waiting {
            0 => "none".to_string(),
            1 => "1 edit".to_string(),
            n => format!("{n} edits"),
        },
    );
    if o.held_back > 0 {
        row(ui, "At the home PC", format!("{} sent, not merged there yet", if o.held_back == 1 { "1 edit".to_string() } else { format!("{} edits", o.held_back) }));
    }
    row(ui, "Photos received", format!("{} from the home PC since the app was set up", photos(o.received)));
    if let Some((text, hint)) = lightcraft_row(s) {
        row(ui, "LightCraft", text);
        if let Some(h) = hint {
            theme::hint(ui, &h);
        }
    }
    row(
        ui,
        "Next sync",
        match (&s.next_sync, s.paused, s.configured) {
            (_, _, false) => "after Setup".to_string(),
            (_, true, _) => "paused (Sync now still works)".to_string(),
            (_, _, _) if matches!(s.activity, Activity::Syncing { .. }) => "now (syncing)".to_string(),
            (Some(t), _, _) => upcoming(t, now),
            (None, _, _) => "not scheduled".to_string(),
        },
    );
}

pub fn show(ui: &mut Ui, v: &StatusView) {
    let t = theme::CHARCOAL;
    theme::title(ui, "Status");
    ui.add_space(10.0);
    let Some(s) = v.status else {
        theme::panel(ui, None, |ui| {
            ui.label(RichText::new(if v.running { "The app is starting; nothing to show yet." } else { "The app has not run yet." }).color(t.text));
        });
        return;
    };
    if !v.running {
        caution(ui, "The app is not running. Start it to keep the cloud library up to date.");
    }
    if !s.configured {
        caution(ui, "Not set up yet: choose the photo folder and where to store the library in Settings.");
    }
    let (state_text, color) = match (&s.activity, s.paused, &s.last_error) {
        (Activity::Syncing { reason, .. }, _, _) => (format!("Syncing now ({reason})"), t.caution),
        (_, _, Some(_)) => ("The last sync failed".to_string(), t.reject),
        (_, true, _) => ("Paused".to_string(), t.caution),
        _ if s.offline => ("Offline: will try again".to_string(), t.caution),
        _ => ("Up to date".to_string(), t.ok),
    };
    state_line(ui, &state_text, color);
    theme::gap(ui);
    theme::panel(ui, None, |ui| {
        if let Some(o) = &s.other {
            other_rows(ui, s, o, v.now);
            return;
        }
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
            "Imports",
            match s.imports_pending {
                Some(0) => "no photos from other PCs waiting".to_string(),
                Some(1) => "1 photo from other PCs waiting".to_string(),
                Some(n) => format!("{n} photos from other PCs waiting"),
                None => "not checked yet".to_string(),
            },
        );
        row(ui, "Photos from other PCs", format!("{} in the last sync; {} in all", s.last_imported, s.photos_imported));
        if let Some((text, hint)) = lightcraft_row(s) {
            row(ui, "LightCraft", text);
            if let Some(h) = hint {
                theme::hint(ui, &h);
            }
        }
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
    if s.other.is_none() {
        list(ui, "Newest wins", "No conflicts between phone and PC edits.", &s.notices, v.now, None);
    }
    list(ui, "Problems", "None.", &s.errors, v.now, Some(theme::CHARCOAL.reject));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lightcraft_row_shows_only_while_photos_wait_and_hints_when_the_port_is_silent() {
        let mut s = AppStatus { lightcraft_port: 7980, ..AppStatus::default() };
        assert_eq!(lightcraft_row(&s), None);
        s.lightcraft_waiting = 1;
        assert_eq!(lightcraft_row(&s), Some(("Waiting for LightCraft: 1 photo".into(), None)));
        s.lightcraft_waiting = 3;
        s.lightcraft_reachable = Some(true);
        assert_eq!(lightcraft_row(&s).unwrap().0, "Waiting for LightCraft: 3 photos");
        assert!(lightcraft_row(&s).unwrap().1.is_none());
        s.lightcraft_reachable = Some(false);
        assert!(lightcraft_row(&s).unwrap().1.unwrap().contains("7980"));
        s.lightcraft_port = 0;
        assert_eq!(lightcraft_row(&s), None, "off");
    }
}
