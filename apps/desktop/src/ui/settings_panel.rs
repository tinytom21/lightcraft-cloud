//! The Setup / Settings panel: edits a [`Form`]; saving is [`Form::save`].

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use egui::{Margin, RichText, Ui};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use lightcraft_cloud_client::other::FolderInfo;

use crate::config::{Config, File, Role};
use crate::settings::{Form, SaveEnv, Storage};
use crate::ui::theme::{self, Kind as ButtonKind};

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

/// Reads the folders of the cloud library from its manifest (the real one does it on a thread of its own: it needs the network).
pub trait FolderLister {
    /// Start reading the library the settings point at; the result arrives through [`FolderLister::poll`].
    fn start(&mut self, file: &File);
    fn waiting(&self) -> bool;
    fn poll(&mut self) -> Option<Result<Vec<FolderInfo>, String>>;
    /// Ask whether a device named `device` has already sent edits to the library (`inbox/<device>/` exists); the answer comes through
    /// [`FolderLister::poll_device_check`]. A lister that cannot say does nothing.
    fn start_device_check(&mut self, _file: &File, _device: &str) {}
    fn poll_device_check(&mut self) -> Option<bool> {
        None
    }
}

#[derive(Default)]
pub struct ThreadLister {
    rx: Option<Receiver<Result<Vec<FolderInfo>, String>>>,
    device_rx: Option<Receiver<bool>>,
}

impl FolderLister for ThreadLister {
    fn start(&mut self, file: &File) {
        if self.rx.is_some() {
            return;
        }
        let (tx, rx) = channel();
        self.rx = Some(rx);
        let file = file.clone();
        std::thread::spawn(move || {
            let r = Config::from_file(PathBuf::from("config.toml"), file)
                .and_then(|c| c.open_remote())
                .and_then(|remote| lightcraft_cloud_client::other::list_folders(&*remote).map_err(|e| e.to_string()));
            let _ = tx.send(r);
        });
    }

    fn start_device_check(&mut self, file: &File, device: &str) {
        let (tx, rx) = channel();
        self.device_rx = Some(rx);
        let (file, device) = (file.clone(), device.to_string());
        std::thread::spawn(move || {
            let used = Config::from_file(PathBuf::from("config.toml"), file)
                .and_then(|c| c.open_remote())
                .and_then(|remote| lightcraft_cloud_client::other::device_in_use(&*remote, &device).map_err(|e| e.to_string()));
            // (no answer, no caution: the folder list shows the real trouble)
            if let Ok(true) = used {
                let _ = tx.send(true);
            }
        });
    }

    fn poll_device_check(&mut self) -> Option<bool> {
        let rx = self.device_rx.as_ref()?;
        match rx.try_recv() {
            Ok(b) => {
                self.device_rx = None;
                Some(b)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.device_rx = None;
                None
            }
        }
    }

    fn waiting(&self) -> bool {
        self.rx.is_some()
    }

    fn poll(&mut self) -> Option<Result<Vec<FolderInfo>, String>> {
        let rx = self.rx.as_ref()?;
        match rx.try_recv() {
            Ok(r) => {
                self.rx = None;
                Some(r)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                Some(Err("reading the cloud library stopped unexpectedly".into()))
            }
        }
    }
}

/// What the folder list shows.
pub enum Folders {
    NotRead,
    Reading,
    Read(Vec<FolderInfo>),
    Failed(String),
}

/// "1.4 GiB", "320.0 MiB".
pub fn human_size(n: u64) -> String {
    let mut f = n as f64;
    let mut unit = "B";
    for u in ["KiB", "MiB", "GiB", "TiB"] {
        if f < 1024.0 {
            break;
        }
        f /= 1024.0;
        unit = u;
    }
    if unit == "B" { format!("{n} B") } else { format!("{f:.1} {unit}") }
}

fn photos_text(n: usize) -> String {
    if n == 1 { "1 photo".to_string() } else { format!("{n} photos") }
}

/// The row of one folder: its name (the top level has none), photos and size.
pub fn folder_label(f: &FolderInfo) -> String {
    let name = if f.folder.is_empty() { "(photos at the top level)" } else { f.folder.as_str() };
    format!("{name}  \u{2014}  {}, {}", photos_text(f.photos), human_size(f.bytes))
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
    pub lister: Box<dyn FolderLister>,
    pub folders: Folders,
    pub problems: Vec<String>,
    pub done: Vec<String>,
    /// Set when a save worked (the window tells the app through the config file; this is for the shell and tests).
    pub saved: bool,
    /// "I have a pairing code" (another PC): the pasted text and the typed code live here until used, then are wiped.
    pub join: Join,
    /// The user's own rclone remotes, kept while a paired config's remotes are shown.
    own_remotes: Result<Vec<String>, String>,
    /// A device of this PC's name has already sent edits to the library.
    pub device_in_use: bool,
}

/// The "I have a pairing code" box. No `Debug`: it holds secrets.
#[derive(Default)]
pub struct Join {
    pub open: bool,
    pub text: String,
    pub code: String,
    /// What went wrong, in words (never the text or the code).
    pub error: Option<String>,
    pub done: bool,
}

impl Join {
    fn wipe(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.text);
        zeroize::Zeroize::zeroize(&mut self.code);
    }
}

impl Drop for Join {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl SettingsPanel {
    pub fn new(form: Form, remotes: Result<Vec<String>, String>, env: SaveEnv, setup: bool) -> Self {
        let own_remotes = remotes.clone();
        SettingsPanel { form, remotes, own_remotes, join: Join::default(), device_in_use: false, env, setup, picker: Box::<RfdPicker>::default(), lister: Box::<ThreadLister>::default(), folders: Folders::NotRead, problems: Vec::new(), done: Vec::new(), saved: false }
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

    /// Open the pasted pairing text with the typed code: on success the config is written to `paired-rclone.conf` beside the app's
    /// config (never the user's own rclone.conf), the form uses it, and the folder list is read.
    pub fn join_with_code(&mut self) {
        use lightcraft_cloud_client::pairing;
        let result = pairing::open(&self.join.text, &self.join.code).and_then(|bundle| {
            let dir = self.env.config_path.parent().map(PathBuf::from).unwrap_or_default();
            let conf = dir.join(crate::settings::PAIRED_CONFIG);
            pairing::write_config(&conf, &bundle.config)?;
            let names = pairing::section_names(&bundle.config);
            self.form.set_paired(&conf, &bundle.remote).map_err(lightcraft_cloud_client::Error::Invalid)?;
            Ok(names)
        });
        match result {
            Ok(names) => {
                self.join.wipe();
                self.join.text.clear();
                self.join.code.clear();
                self.join.error = None;
                self.join.done = true;
                self.join.open = false;
                self.remotes = Ok(names);
                self.problems.clear();
                self.read_folders();
            }
            Err(e) => {
                self.join.error = Some(e.to_string());
                self.join.done = false;
            }
        }
    }

    /// Stop using the paired storage and go back to the user's own rclone remotes.
    fn use_own_remotes(&mut self) {
        self.form.clear_paired();
        self.remotes = self.own_remotes.clone();
        self.folders = Folders::NotRead;
        self.device_in_use = false;
        self.join.done = false;
    }

    pub fn show(&mut self, ui: &mut Ui) {
        self.show_with_parent(ui, None);
    }

    /// [`SettingsPanel::show`], with the window that Browse… dialogs belong to.
    pub fn show_with_parent(&mut self, ui: &mut Ui, parent: Option<&dyn Parent>) {
        if let Some(p) = self.picker.poll() {
            self.form.source = p.to_string_lossy().into_owned();
        }
        if let Some(r) = self.lister.poll() {
            self.folders = match r {
                Ok(v) => Folders::Read(v),
                Err(e) => Folders::Failed(e),
            };
        }
        // (settings that are already saved show the folders of the cloud library at once; a first setup waits for the button)
        if self.form.is_other() && !self.setup && matches!(self.folders, Folders::NotRead) && !self.form.storage.spec().trim_matches(':').is_empty() {
            self.read_folders();
        }
        if let Some(b) = self.lister.poll_device_check() {
            self.device_in_use = b;
        }
        if self.picker.waiting() || self.lister.waiting() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        let t = theme::CHARCOAL;
        theme::title(ui, if self.setup { "Set up LightCraft Cloud" } else { "Settings" });
        if self.setup {
            ui.add_space(4.0);
            let intro = if self.form.is_other() {
                "Choose where the cloud library is and which folders to keep on this PC. Nothing is downloaded until you save."
            } else {
                "Choose the photos to publish and where the cloud library is stored. Nothing is uploaded until you save."
            };
            ui.add(egui::Label::new(RichText::new(intro).color(t.text_dim)).wrap());
        }
        ui.add_space(12.0);

        if cfg!(feature = "home") {
            theme::panel(ui, Some("What is this PC?"), |ui| {
                let mut role = self.form.role;
                ui.radio_value(&mut role, Role::Home, "This is my home PC");
                theme::hint(ui, "It keeps the photos and puts them in the cloud library. Other PCs and the phone work from them.");
                ui.radio_value(&mut role, Role::Other, "This is another PC");
                theme::hint(ui, "It keeps a copy of the folders you choose and sends its edits back to the home PC.");
                self.form.set_role(role);
            });
            theme::gap(ui);
        }

        if self.form.is_other() {
            self.storage_panel(ui);
            theme::gap(ui);
            theme::panel(ui, Some("Photos on this PC"), |ui| {
                let label = theme::field_label(ui, "Folder for the photos");
                self.source_row(ui, parent, label.id, folder_hint());
                ui.add(egui::Label::new(RichText::new("Set LightCraft's import destination to this folder. New photos you import there, in any folders inside it, are sent to the home PC by themselves, with your folders kept. Photos imported anywhere else are not uploaded.").color(t.text)).wrap());
                theme::hint(ui, "The folders you choose below are downloaded here. Use a folder that is only for this, and add it to LightCraft. The folders you import into are added to the ones kept on this PC automatically.");
            });
            theme::gap(ui);
            self.folders_panel(ui);
            theme::gap(ui);
            theme::panel(ui, Some("Name of this PC"), |ui| {
                let label = theme::field_label(ui, "Shown to the home PC with the edits you send");
                ui.add_sized([240.0, 30.0], theme::text_edit(&mut self.form.device).hint_text(crate::config::default_device())).labelled_by(label.id);
                theme::hint(ui, "Leave it empty to use this PC's name. It must not be the name of a phone you use with this library.");
                if self.device_in_use {
                    ui.add(egui::Label::new(RichText::new("A device with this name has already sent edits to this library. Give this PC another name above, unless this is the same device.").color(t.caution)).wrap());
                }
            });
        } else {
            theme::panel(ui, None, |ui| {
                let label = theme::field_label(ui, "Photo folder");
                self.source_row(ui, parent, label.id, photos_hint());
                ui.add_space(4.0);
                let label = theme::field_label(ui, "Folders to leave out (one name pattern per line, e.g. exported)");
                let width = ui.available_width();
                ui.add(egui::TextEdit::multiline(&mut self.form.exclude).desired_rows(3).desired_width(width).margin(Margin::symmetric(8, 6))).labelled_by(label.id);
            });
            theme::gap(ui);
            self.storage_panel(ui);
        }

        theme::gap(ui);
        theme::panel(ui, None, |ui| {
            ui.checkbox(&mut self.form.notifications, "Show desktop notifications");
            ui.checkbox(&mut self.form.start_at_login, "Start when I log in");
            ui.checkbox(&mut self.form.check_for_updates, "Check for new versions");
        });

        ui.add_space(14.0);
        if theme::button(ui, if self.setup { "Save and start" } else { "Save" }, ButtonKind::Primary).clicked() {
            self.save();
        }
        for p in &self.problems {
            ui.add(egui::Label::new(RichText::new(p).color(t.reject)).wrap());
        }
        for d in &self.done {
            ui.add(egui::Label::new(RichText::new(d).color(t.ok)).wrap());
        }
    }

    fn read_folders(&mut self) {
        if self.lister.waiting() {
            return;
        }
        self.folders = Folders::Reading;
        self.device_in_use = false;
        let file = self.form.to_file();
        self.lister.start(&file);
        if self.form.is_other() {
            let device = Some(self.form.device.trim().to_string()).filter(|d| !d.is_empty()).unwrap_or_else(crate::config::default_device);
            self.lister.start_device_check(&file, &device);
        }
    }

    /// The line with the photo folder and its Browse… button.
    fn source_row(&mut self, ui: &mut Ui, parent: Option<&dyn Parent>, label: egui::Id, hint: &str) {
        ui.horizontal(|ui| {
            let browse = 96.0;
            let room = (ui.available_width() - browse - ui.spacing().item_spacing.x).max(120.0);
            ui.add_sized([room, 30.0], theme::text_edit(&mut self.form.source).hint_text(hint)).labelled_by(label);
            if theme::button(ui, "Browse…", ButtonKind::Secondary).clicked() {
                self.picker.start(&self.form.source, parent);
            }
        });
    }

    /// Which folders of the cloud library this PC keeps: the list read from its manifest, with sizes and photo counts.
    fn folders_panel(&mut self, ui: &mut Ui) {
        let t = theme::CHARCOAL;
        theme::panel(ui, Some("Folders to keep on this PC"), |ui| {
            let reading = self.lister.waiting();
            let label = if matches!(self.folders, Folders::Read(_)) { "Read the list again" } else { "Show the folders in the cloud library" };
            let can_read = !self.form.storage.spec().trim_matches(':').is_empty();
            ui.horizontal(|ui| {
                if theme::button(ui, label, ButtonKind::Secondary).clicked() && !reading && can_read {
                    self.read_folders();
                }
                if reading {
                    theme::hint(ui, "Reading the cloud library\u{2026}");
                }
            });
            if !can_read {
                theme::hint(ui, "Choose where the cloud library is first.");
            }
            let form = &mut self.form;
            match &self.folders {
                Folders::NotRead => {
                    if form.folders.is_empty() {
                        theme::hint(ui, "The folders are listed here, with how many photos they hold and how much room they need.");
                    } else {
                        let names: Vec<&str> = form.folders.iter().map(String::as_str).collect();
                        ui.add(egui::Label::new(RichText::new(format!("Kept now: {}", names.join(", "))).color(t.text)).wrap());
                    }
                }
                Folders::Reading => {}
                Folders::Failed(e) => {
                    ui.add(egui::Label::new(RichText::new(format!("Could not read the cloud library: {e}")).color(t.reject)).wrap());
                }
                Folders::Read(list) => {
                    if list.is_empty() {
                        theme::hint(ui, "The cloud library has no photos yet.");
                    }
                    for f in list {
                        let mut on = form.folders.contains(&f.folder);
                        if ui.checkbox(&mut on, folder_label(f)).changed() {
                            form.set_folder(&f.folder, on);
                        }
                    }
                    let gone: Vec<String> = form.folders.iter().filter(|c| !list.iter().any(|f| &f.folder == *c)).cloned().collect();
                    for g in gone {
                        let mut on = true;
                        if ui.checkbox(&mut on, format!("{g}  \u{2014}  no longer in the cloud library")).changed() {
                            form.set_folder(&g, false);
                        }
                    }
                    let chosen: Vec<&FolderInfo> = list.iter().filter(|f| form.folders.contains(&f.folder)).collect();
                    let (photos, bytes) = (chosen.iter().map(|f| f.photos).sum::<usize>(), chosen.iter().map(|f| f.bytes).sum::<u64>());
                    let n = form.folders.len();
                    let line = if n == 0 {
                        "No folder ticked yet.".to_string()
                    } else {
                        format!("{} ticked: {}, {} in all", if n == 1 { "1 folder".to_string() } else { format!("{n} folders") }, photos_text(photos), human_size(bytes))
                    };
                    ui.add(egui::Label::new(RichText::new(line).color(t.text)).wrap());
                }
            }
            theme::hint(ui, "A photo you delete on this PC stays deleted. To get it back, untick its folder, save, then tick it again. A folder you untick keeps its photos here but is no longer updated.");
        });
    }

    /// "I have a pairing code": paste the text, type the code.
    fn join_box(&mut self, ui: &mut Ui) {
        let t = theme::CHARCOAL;
        if !self.join.open {
            if theme::button(ui, "I have a pairing code", ButtonKind::Secondary).clicked() {
                self.join.open = true;
            }
            return;
        }
        theme::hint(ui, "On the home PC, open Status, then Add a device. Paste the text it shows (use Copy text for another PC), then type the code.");
        let l = theme::field_label(ui, "Pairing text");
        let width = ui.available_width();
        ui.add_sized([width, 30.0], theme::text_edit(&mut self.join.text).password(true).hint_text("LCP1…")).labelled_by(l.id);
        let l = theme::field_label(ui, "Code");
        ui.add_sized([200.0, 30.0], theme::text_edit(&mut self.join.code).hint_text("XXXXX-XXXXX")).labelled_by(l.id);
        ui.horizontal(|ui| {
            if theme::button(ui, "Use this pairing", ButtonKind::Primary).clicked() {
                self.join_with_code();
            }
            if theme::button(ui, "Cancel", ButtonKind::Secondary).clicked() {
                self.join = Join::default();
            }
        });
        if let Some(e) = &self.join.error {
            ui.add(egui::Label::new(RichText::new(e).color(t.reject)).wrap());
        }
    }

    /// Where the cloud library is stored (both roles); the "previews only" choice is the home PC's.
    fn storage_panel(&mut self, ui: &mut Ui) {
        let t = theme::CHARCOAL;
        theme::panel(ui, Some(if self.form.is_other() { "Cloud library" } else { "Storage" }), |ui| {
            if self.form.is_paired() {
                ui.add(egui::Label::new(RichText::new(format!("Paired storage: {}", self.form.storage.spec())).color(t.text)).wrap());
                theme::hint(ui, "From a pairing code. Its sign-in is kept in a file in this app's config folder, apart from your own rclone config.");
                if theme::button(ui, "Use one of my rclone remotes instead", ButtonKind::Secondary).clicked() {
                    self.use_own_remotes();
                }
                return;
            }
            let mut kind = if matches!(self.form.storage, Storage::Rclone { .. }) { Kind::Rclone } else { Kind::Local };
            let before = kind;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                ui.radio_value(&mut kind, Kind::Rclone, "One of my rclone remotes");
                ui.radio_value(&mut kind, Kind::Local, "A folder on this PC");
            });
            if kind != before {
                self.form.storage = match kind {
                    Kind::Rclone => Storage::Rclone { remote: String::new(), path: "LightCraft Cloud".into() },
                    Kind::Local => Storage::Local { dir: String::new() },
                };
                self.folders = Folders::NotRead;
            }
            if self.form.is_other() {
                self.join_box(ui);
            }
            let names: Vec<String> = self.remotes.clone().unwrap_or_default();
            match &mut self.form.storage {
                Storage::Rclone { remote, path } => {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        let shown = if remote.is_empty() { "Choose a remote".to_string() } else { remote.clone() };
                        egui::ComboBox::from_label("Remote").selected_text(shown).show_ui(ui, |ui| {
                            for n in &names {
                                ui.selectable_value(remote, n.clone(), n);
                            }
                        });
                        let label = ui.label("Folder on it");
                        ui.add_sized([200.0, 30.0], theme::text_edit(path).hint_text("LightCraft Cloud")).labelled_by(label.id);
                    });
                    match &self.remotes {
                        Err(e) => {
                            ui.add(egui::Label::new(RichText::new(format!("Could not list your rclone remotes: {e}")).color(t.caution)).wrap());
                        }
                        Ok(n) if n.is_empty() => {
                            ui.add(egui::Label::new(RichText::new("Your rclone config has no remotes yet. Add one with `rclone config`.").color(t.caution)).wrap());
                        }
                        Ok(_) => {}
                    }
                    ui.add(egui::Label::new(RichText::new("The app uses your existing rclone config for now (sign-in comes later).").size(11.5).color(t.text_dim)).wrap());
                }
                Storage::Local { dir } => {
                    let label = theme::field_label(ui, "Folder on this PC");
                    let width = ui.available_width();
                    ui.add_sized([width, 30.0], theme::text_edit(dir).hint_text(folder_hint())).labelled_by(label.id);
                }
            }
            if !self.form.is_other() {
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 16.0;
                    ui.radio_value(&mut self.form.previews_only, false, "Originals and previews");
                    ui.radio_value(&mut self.form.previews_only, true, "Previews only");
                });
            }
        });
    }
}

/// What an empty folder field shows as a hint: a path of this system.
fn folder_hint() -> &'static str {
    if cfg!(windows) { r"D:\Photos\LightCraft Cloud" } else { "/path/to/a/folder" }
}

fn photos_hint() -> &'static str {
    if cfg!(windows) { r"D:\Photos" } else { "/path/to/your/photos" }
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
