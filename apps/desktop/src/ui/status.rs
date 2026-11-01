//! The Status panel: how the app is doing, from the `status.json` the running app writes.

use std::time::SystemTime;

use egui::{Color32, CornerRadius, Label, Margin, Rect, RichText, Role, Sense, Stroke, Ui, WidgetInfo, pos2, vec2};
use lightcraft_cloud_client::{Progress, Stage, Summary, describe, to_ms};

use crate::appstate::{Activity, AppStatus, Notice, OtherView, Tone, relative, upcoming};
use crate::ui::settings_panel::human_size;
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

/// How tall the progress bar is.
const BAR_H: f32 = 8.0;

/// The width of the bar's filled part for `fraction` of `total` (None: the bar does not know; see [`busy_span`]). A bar that has begun
/// shows at least a sliver, so "just started" is not mistaken for "nothing".
pub fn fill_width(fraction: f32, total: f32) -> f32 {
    let f = fraction.clamp(0.0, 1.0);
    if f <= 0.0 { 0.0 } else { (total * f).max(BAR_H.min(total)) }
}

/// The moving piece of a bar that cannot say how far it is: where it starts and ends, for `phase` 0..1 (a third of the bar, going to
/// the right and starting again).
pub fn busy_span(phase: f32, total: f32) -> (f32, f32) {
    let len = total / 3.0;
    let start = (total + len) * phase.rem_euclid(1.0) - len;
    (start.max(0.0), (start + len).min(total))
}

/// The progress bar: the Charcoal track with the accent fill. Labelled for screen readers and tests with its percentage.
pub fn progress_bar(ui: &mut Ui, fraction: Option<f32>) {
    let t = theme::CHARCOAL;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), BAR_H), Sense::hover());
    response.widget_info(|| {
        let mut info = match fraction {
            Some(f) => WidgetInfo::labeled(Role::ProgressIndicator, true, format!("Progress {}%", (f * 100.0).floor())),
            None => WidgetInfo::labeled(Role::ProgressIndicator, true, "Progress, working"),
        };
        info.value = fraction.map(|f| (f * 100.0).floor() as f64);
        info
    });
    let round = CornerRadius::same((BAR_H / 2.0) as u8);
    ui.painter().rect_filled(rect, round, t.inset);
    ui.painter().rect_stroke(rect, round, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
    let (from, to) = match fraction {
        Some(f) => (0.0, fill_width(f, rect.width())),
        None => {
            // (it moves with the time of the frame; the window asks to be drawn again while it shows)
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            busy_span((ui.input(|i| i.time) / 1.6) as f32, rect.width())
        }
    };
    if to > from {
        let fill = Rect::from_min_max(pos2(rect.left() + from, rect.top()), pos2(rect.left() + to, rect.bottom()));
        ui.painter().rect_filled(fill, round, t.accent);
    }
}

/// What the running job says, under the state line: the line, a bar, and a calm note when nothing has moved for a while.
fn progress_block(ui: &mut Ui, progress: Option<&Progress>, now: SystemTime) {
    let t = theme::CHARCOAL;
    let summary = progress.map(|p| describe(p, to_ms(now)));
    theme::gap(ui);
    theme::panel(ui, None, |ui| {
        let Summary { line, note, fraction } = summary.unwrap_or(Summary { line: "Getting started…".into(), note: None, fraction: None });
        ui.add(Label::new(RichText::new(line).color(t.text)).wrap());
        progress_bar(ui, fraction);
        if let Some(n) = note {
            ui.add(Label::new(RichText::new(n).color(t.caution)).wrap());
        }
    });
}

/// The state line's dot: green only for a settled state.
pub fn tone_color(tone: Tone) -> Color32 {
    let t = theme::CHARCOAL;
    match tone {
        Tone::Ok => t.ok,
        Tone::Busy => t.accent,
        Tone::Wait => t.caution,
        Tone::Offline => t.text_label,
        Tone::Paused => t.text_dim,
        Tone::Bad => t.reject,
    }
}

/// The imported-photos row of another PC: what is on its way to the home PC. None when nothing is.
pub fn queue_row(o: &OtherView) -> String {
    let photos = |n: usize| if n == 1 { "1 photo".to_string() } else { format!("{n} photos") };
    let mut parts = Vec::new();
    if o.uploads_waiting > 0 {
        parts.push(format!("{} ({}) waiting to upload", photos(o.uploads_waiting), human_size(o.uploads_bytes)));
    }
    if o.awaiting_home > 0 {
        parts.push(format!("{} sent, waiting for the home PC to add {}", photos(o.awaiting_home), if o.awaiting_home == 1 { "it" } else { "them" }));
    }
    if parts.is_empty() { "nothing waiting".to_string() } else { parts.join("; ") }
}

/// The LightCraft row: "Waiting for LightCraft: N photos" while photos wait to be added there (nothing when the feature is off
/// or nothing waits), and a hint when the port has not answered.
pub fn lightcraft_row(s: &AppStatus) -> Option<(String, Option<String>)> {
    let folders = if s.lightcraft_port != 0 { s.lightcraft_refresh_waiting } else { 0 };
    if s.lightcraft_port == 0 || (s.lightcraft_waiting == 0 && folders == 0) {
        return None;
    }
    let n = s.lightcraft_waiting;
    let mut text = if n == 0 { "Waiting for LightCraft".to_string() } else { format!("Waiting for LightCraft: {n} {}", if n == 1 { "photo" } else { "photos" }) };
    if folders > 0 {
        let f = format!("{folders} {} to read the new edits of", if folders == 1 { "folder" } else { "folders" });
        text += &if n == 0 { format!(": {f}") } else { format!("; {f}") };
    }
    let hint = (s.lightcraft_reachable == Some(false)).then(|| {
        format!(
            "LightCraft is not answering on port {}. Open LightCraft with its control port on (see the README); what waits is sent when it answers.",
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
    // (the counts are saved when a pass ends; while it downloads, the running count is the true one)
    let (here, waiting) = match s.activity.progress().filter(|p| p.stage == Stage::Downloading && p.total_items > 0) {
        Some(p) => (o.photos + p.done_items as usize, p.total_items.saturating_sub(p.done_items) as usize),
        None => (o.photos, o.waiting_to_download),
    };
    row(ui, "Photos on this PC", here.to_string());
    row(ui, "Waiting to download", if waiting == 0 { "nothing".to_string() } else { photos(waiting) });
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
    row(ui, "Imported photos", queue_row(o));
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

/// What the user chose in the Status panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusAction {
    /// Send the photos the home PC refused again.
    TryAgain,
    /// Accept the refusals: the photos stay on this PC only.
    KeepHere,
    /// Queue the photos that were in the folder at the start and are not in the cloud library.
    SendHeld,
}

pub fn show(ui: &mut Ui, v: &StatusView) {
    let _ = show_with_actions(ui, v, &[]);
}

/// The photos the home PC refused, as (file name, reason) for the Status panel. Read from the queue by the window.
pub fn refused_lines(entries: &[lightcraft_cloud_client::other::OutEntry]) -> Vec<(String, String)> {
    entries.iter().map(|e| (e.name.clone(), e.result.as_ref().and_then(|r| r.reason.clone()).unwrap_or_else(|| "no reason given".into()))).collect()
}

/// [`show`], with the buttons of the refused-photos row; `refused` lists those photos with the home PC's reason.
pub fn show_with_actions(ui: &mut Ui, v: &StatusView, refused: &[(String, String)]) -> Option<StatusAction> {
    let mut action = None;
    let t = theme::CHARCOAL;
    theme::title(ui, "Status");
    ui.add_space(10.0);
    let Some(s) = v.status else {
        theme::panel(ui, None, |ui| {
            ui.label(RichText::new(if v.running { "The app is starting; nothing to show yet." } else { "The app has not run yet." }).color(t.text));
        });
        return None;
    };
    if !v.running {
        caution(ui, "The app is not running. Start it to keep the cloud library up to date.");
    }
    if !s.configured {
        caution(ui, "Not set up yet: choose the photo folder and where to store the library in Settings.");
    }
    let health = s.health();
    let (state_text, color) = (health.text, tone_color(health.tone));
    state_line(ui, &state_text, color);
    if s.lightcraft_sidecars_off() {
        caution(ui, crate::appstate::SIDECARS_OFF_LINE);
    }
    // (only while the app is running: a status file left by one that stopped mid-job says nothing true about the time)
    if v.running && s.activity.is_busy() {
        progress_block(ui, s.activity.progress(), v.now);
    }
    theme::gap(ui);
    theme::panel(ui, None, |ui| {
        if let Some(o) = &s.other {
            other_rows(ui, s, o, v.now);
            if o.held > 0 {
                action = held_row(ui, o.held, o.held_unchecked);
            }
            if o.rejected > 0 {
                action = refused_row(ui, o.rejected, refused).or(action);
            }
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
    action
}

/// The row for the photos that were in the folder when this PC first looked and are not in the library: not sent by themselves.
pub fn held_text(held: usize, unchecked: usize) -> String {
    let n = if held == 1 { "1 photo here isn't".to_string() } else { format!("{held} photos here aren't") };
    if unchecked > 0 { format!("{n} in the cloud library (still looking at {unchecked} more)") } else { format!("{n} in the cloud library") }
}

fn held_row(ui: &mut Ui, held: usize, unchecked: usize) -> Option<StatusAction> {
    row(ui, "Not sent", held_text(held, unchecked));
    theme::hint(ui, "They were in this folder before the app first looked, so they are not sent by themselves. Photos you import from now on are.");
    let mut action = None;
    if theme::button(ui, "Send them", theme::Kind::Secondary).clicked() {
        action = Some(StatusAction::SendHeld);
    }
    action
}

/// The row for photos the home PC refused, with the reasons and the two ways to deal with them.
fn refused_row(ui: &mut Ui, n: usize, refused: &[(String, String)]) -> Option<StatusAction> {
    let t = theme::CHARCOAL;
    row(ui, "Refused by the home PC", if n == 1 { "1 photo, still on this PC".to_string() } else { format!("{n} photos, still on this PC") });
    for (name, why) in refused.iter().take(5) {
        ui.add(Label::new(RichText::new(format!("{name}: {why}")).size(11.5).color(t.reject)).wrap());
    }
    if refused.len() > 5 {
        theme::hint(ui, &format!("and {} more", refused.len() - 5));
    }
    theme::hint(ui, "They are still on this PC. Try again sends them anew; Keep on this PC only accepts that they stay here.");
    let mut action = None;
    ui.horizontal(|ui| {
        if theme::button(ui, "Try again", theme::Kind::Primary).clicked() {
            action = Some(StatusAction::TryAgain);
        }
        if theme::button(ui, "Keep on this PC only", theme::Kind::Secondary).clicked() {
            action = Some(StatusAction::KeepHere);
        }
    });
    action
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
        s.lightcraft_reachable = Some(true);
        // folders whose new edits are to be read
        s.lightcraft_waiting = 0;
        assert_eq!(lightcraft_row(&s), None);
        s.lightcraft_refresh_waiting = 2;
        assert_eq!(lightcraft_row(&s).unwrap().0, "Waiting for LightCraft: 2 folders to read the new edits of");
        s.lightcraft_waiting = 1;
        s.lightcraft_refresh_waiting = 1;
        assert_eq!(lightcraft_row(&s).unwrap().0, "Waiting for LightCraft: 1 photo; 1 folder to read the new edits of");
        s.lightcraft_port = 0;
        assert_eq!(lightcraft_row(&s), None, "off");
    }
}
