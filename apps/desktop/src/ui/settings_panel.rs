//! The Setup / Settings panel: edits a [`Form`]; saving is [`Form::save`].

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use egui::{Color32, RichText, Ui};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::settings::{Form, SaveEnv, Storage};

/// The window a dialog belongs to (eframe's `Frame`): without a parent the desktop portal cannot place the dialog and it
/// can end up hidden behind the window.
pub trait Parent: HasWindowHandle + HasDisplayHandle {}
impl<T: HasWindowHandle + HasDisplayHandle + ?Sized> Parent for T {}

/// Picks a folder (the real one opens the desktop's dialog on a thread of its own, so the window keeps painting).
pub trait FolderPicker {
    /// Start choosing; the result arrives through [`FolderPicker::poll`].
    fn start(&mut self, from: &str, parent: Option<&dyn Parent>);
    /// Whether a dialog is open (the window keeps repainting so its result shows without mouse movement).
    fn waiting(&self) -> bool;
    fn poll(&mut self) -> Option<PathBuf>;
}

/// How long a dialog is given before a new click may start over (the desktop's picker service can die and leave its thread
/// waiting for ever; the abandoned thread is harmless). Long enough that a double click never opens two.
const RESTART_AFTER: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct RfdPicker {
    rx: Option<Receiver<Option<PathBuf>>>,
    since: Option<Instant>,
}

impl RfdPicker {
    /// A sender for a new dialog, unless one was started less than `wait` ago and is still open; an older one is dropped.
    fn begin(&mut self, now: Instant, wait: Duration) -> Option<Sender<Option<PathBuf>>> {
        if self.rx.is_some() && self.since.is_some_and(|t| now.saturating_duration_since(t) < wait) {
            return None;
        }
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.since = Some(now);
        Some(tx)
    }
}

impl FolderPicker for RfdPicker {
    fn start(&mut self, from: &str, parent: Option<&dyn Parent>) {
        let Some(tx) = self.begin(Instant::now(), RESTART_AFTER) else { return };
        let from = crate::config::expand(from).ok().filter(|p| p.is_dir());
        // Built here, where the window handle is at hand; rfd's `FileDialog` is `Send` and only carries the raw window id.
        let mut d = rfd::FileDialog::new().set_title("Choose the photo folder");
        if let Some(p) = parent {
            d = d.set_parent(p);
        }
        if let Some(f) = from {
            d = d.set_directory(f);
        }
        std::thread::spawn(move || {
            let _ = tx.send(d.pick_folder());
        });
    }

    fn waiting(&self) -> bool {
        self.rx.is_some()
    }

    fn poll(&mut self) -> Option<PathBuf> {
        let rx = self.rx.as_ref()?;
        match rx.try_recv() {
            Ok(p) => {
                self.rx = None;
                p
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                None
            }
        }
    }
}

/// Which storage kind the radio shows.
#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Rclone,
    Local,
}

pub struct SettingsPanel {
    pub form: Form,
    /// The rclone remote names; an error when rclone could not be asked (shown, never the config's contents).
    pub remotes: Result<Vec<String>, String>,
    pub env: SaveEnv,
    /// First run: the panel is titled Setup and says nothing has been saved yet.
    pub setup: bool,
    pub picker: Box<dyn FolderPicker>,
    pub problems: Vec<String>,
    pub done: Vec<String>,
    /// Set when a save worked (the window tells the app through the config file; this is for the shell and tests).
    pub saved: bool,
}

impl SettingsPanel {
    pub fn new(form: Form, remotes: Result<Vec<String>, String>, env: SaveEnv, setup: bool) -> Self {
        SettingsPanel { form, remotes, env, setup, picker: Box::<RfdPicker>::default(), problems: Vec::new(), done: Vec::new(), saved: false }
    }

    pub fn save(&mut self) {
        let remotes = self.remotes.as_ref().ok().cloned();
        match self.form.save(&self.env, remotes.as_deref()) {
            Ok(done) => {
                self.done = done;
                self.problems.clear();
                self.saved = true;
                self.setup = false;
            }
            Err(p) => {
                self.problems = p;
                self.done.clear();
            }
        }
    }

    pub fn show(&mut self, ui: &mut Ui) {
        self.show_with_parent(ui, None);
    }

    /// [`SettingsPanel::show`], with the window that Browse… dialogs belong to.
    pub fn show_with_parent(&mut self, ui: &mut Ui, parent: Option<&dyn Parent>) {
        if let Some(p) = self.picker.poll() {
            self.form.source = p.to_string_lossy().into_owned();
        }
        if self.picker.waiting() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        ui.heading(if self.setup { "Set up LightCraft Cloud" } else { "Settings" });
        if self.setup {
            ui.label("Choose the photos to publish and where the cloud library is stored. Nothing is uploaded until you save.");
        }
        ui.add_space(8.0);

        let label = ui.label(RichText::new("Photo folder").strong());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.form.source).desired_width(380.0).hint_text("/path/to/your/photos")).labelled_by(label.id);
            if ui.button("Browse…").clicked() {
                self.picker.start(&self.form.source, parent);
            }
        });
        ui.add_space(6.0);
        let label = ui.label(RichText::new("Folders to leave out (one name pattern per line, e.g. exported)").strong());
        ui.add(egui::TextEdit::multiline(&mut self.form.exclude).desired_rows(3).desired_width(460.0)).labelled_by(label.id);

        ui.add_space(10.0);
        ui.label(RichText::new("Storage").strong());
        let mut kind = if matches!(self.form.storage, Storage::Rclone { .. }) { Kind::Rclone } else { Kind::Local };
        let before = kind;
        ui.horizontal(|ui| {
            ui.radio_value(&mut kind, Kind::Rclone, "One of my rclone remotes");
            ui.radio_value(&mut kind, Kind::Local, "A folder on this PC");
        });
        if kind != before {
            self.form.storage = match kind {
                Kind::Rclone => Storage::Rclone { remote: String::new(), path: "LightCraft Cloud".into() },
                Kind::Local => Storage::Local { dir: String::new() },
            };
        }
        let names: Vec<String> = self.remotes.clone().unwrap_or_default();
        match &mut self.form.storage {
            Storage::Rclone { remote, path } => {
                ui.horizontal(|ui| {
                    let shown = if remote.is_empty() { "Choose a remote".to_string() } else { remote.clone() };
                    egui::ComboBox::from_label("Remote").selected_text(shown).show_ui(ui, |ui| {
                        for n in &names {
                            ui.selectable_value(remote, n.clone(), n);
                        }
                    });
                    let label = ui.label("Folder on it");
                    ui.add(egui::TextEdit::singleline(path).desired_width(220.0).hint_text("LightCraft Cloud")).labelled_by(label.id);
                });
                match &self.remotes {
                    Err(e) => {
                        ui.colored_label(Color32::from_rgb(0xd9, 0x8a, 0x1f), format!("Could not list your rclone remotes: {e}"));
                    }
                    Ok(n) if n.is_empty() => {
                        ui.colored_label(Color32::from_rgb(0xd9, 0x8a, 0x1f), "Your rclone config has no remotes yet. Add one with `rclone config`.");
                    }
                    Ok(_) => {}
                }
                ui.label(RichText::new("The app uses your existing rclone config for now (sign-in comes later).").weak());
            }
            Storage::Local { dir } => {
                let label = ui.label("Folder on this PC");
                ui.add(egui::TextEdit::singleline(dir).desired_width(460.0).hint_text("/path/to/a/folder")).labelled_by(label.id);
            }
        }
        ui.add_space(6.0);
        ui.radio_value(&mut self.form.previews_only, false, "Originals and previews");
        ui.radio_value(&mut self.form.previews_only, true, "Previews only");

        ui.add_space(10.0);
        ui.checkbox(&mut self.form.notifications, "Show desktop notifications");
        ui.checkbox(&mut self.form.start_at_login, "Start when I log in");

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button(if self.setup { "Save and start" } else { "Save" }).clicked() {
                self.save();
            }
        });
        for p in &self.problems {
            ui.colored_label(Color32::from_rgb(0xd0, 0x3b, 0x3b), p);
        }
        for d in &self.done {
            ui.colored_label(Color32::from_rgb(0x2e, 0x9d, 0x5b), d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stuck_dialog_can_be_started_over_but_a_double_click_cannot() {
        let mut p = RfdPicker::default();
        let t0 = Instant::now();
        let wait = Duration::from_secs(5);
        let old = p.begin(t0, wait).expect("first click starts");
        assert!(p.begin(t0 + Duration::from_millis(300), wait).is_none());
        let new = p.begin(t0 + Duration::from_secs(6), wait).expect("a later click starts over");
        assert!(old.send(None).is_err(), "the old receiver was dropped");
        new.send(Some(PathBuf::from("/x"))).unwrap();
        assert_eq!(p.poll(), Some(PathBuf::from("/x")));
        assert!(!p.waiting());
    }
}
