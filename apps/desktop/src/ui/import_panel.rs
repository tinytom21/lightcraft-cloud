//! The Import tab (another PC): choose a card or folder, name the destination folder, look at what is new, import. The photos are copied
//! into the photo folder, queued, and sent to the home PC by the app in the background; this tab only does the first part.
//!
//! The work (a scan hashes every photo; a copy reads and writes gigabytes) runs on a thread of its own behind [`ImportBackend`], so the
//! window keeps painting, shows progress and can cancel. The panel is a plain function of a `&mut Ui` and its state, so it is tested
//! headlessly with a fake backend.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};

use egui::{ProgressBar, RichText, Ui};

use lightcraft_cloud_client::other::{ImportRun, ImportScan, Phase, Progress, check_destination, existing_folders};

use crate::config::{self, Config, Role};
use crate::pending::PendingList;
use crate::ui::settings_panel::{FolderPicker, Parent, RfdPicker, human_size};
use crate::ui::theme::{self, Kind as ButtonKind};

/// What a background job reports.
pub enum Event {
    Scanned(Result<ImportScan, String>),
    /// The copy ended (`Err`: it could not start). The folder is the destination if it was added to the folders this PC keeps.
    Imported(Result<ImportRun, String>, Option<String>),
}

/// The work behind the tab. The real one is [`ThreadBackend`]; tests give a fake.
pub trait ImportBackend {
    /// Folders to offer as the destination.
    fn folders(&self) -> Vec<String>;
    /// The destination as it will be used, or why it is refused (the home PC's own rule).
    fn check(&self, dest: &str) -> Result<String, String>;
    fn scan(&mut self, source: PathBuf, dest: String);
    fn import(&mut self, scan: ImportScan);
    /// Ask the running job to stop (it stops soon, and what was done stays).
    fn cancel(&mut self);
    fn busy(&self) -> bool;
    fn progress(&self) -> Progress;
    fn poll(&mut self) -> Vec<Event>;
}

/// Where the tab is.
pub enum Stage {
    Choose,
    Scanning,
    Scanned(Box<ImportScan>),
    Importing(Box<ImportScan>),
    Done(Box<ImportRun>),
}

pub struct ImportPanel {
    pub source: String,
    pub dest: String,
    pub backend: Box<dyn ImportBackend>,
    pub picker: Box<dyn FolderPicker>,
    pub existing: Vec<String>,
    pub stage: Stage,
    /// Why the last job could not run.
    pub error: Option<String>,
    /// Folders the imports added to the config; the window passes them on to Settings (`take_chosen`).
    chosen: Vec<String>,
    /// Whether a cancel was asked for (the buttons say so until the job ends).
    cancelling: bool,
    /// The scan the user asked to redo after cancelling.
    cancelled_scan: bool,
}

impl ImportPanel {
    pub fn new(backend: Box<dyn ImportBackend>) -> Self {
        let existing = backend.folders();
        ImportPanel {
            source: String::new(),
            dest: String::new(),
            backend,
            picker: Box::new(RfdPicker::with_title("Choose the card or folder to import from")),
            existing,
            stage: Stage::Choose,
            error: None,
            chosen: Vec::new(),
            cancelling: false,
            cancelled_scan: false,
        }
    }

    /// The folders imports added to the config since the last call.
    pub fn take_chosen(&mut self) -> Vec<String> {
        std::mem::take(&mut self.chosen)
    }

    fn poll(&mut self) {
        if let Some(p) = self.picker.poll() {
            self.source = p.to_string_lossy().into_owned();
            self.stage = Stage::Choose;
        }
        for ev in self.backend.poll() {
            self.cancelling = false;
            match ev {
                Event::Scanned(Ok(scan)) => {
                    self.error = None;
                    self.cancelled_scan = scan.cancelled;
                    self.stage = if scan.cancelled { Stage::Choose } else { Stage::Scanned(Box::new(scan)) };
                }
                Event::Scanned(Err(e)) => {
                    self.error = Some(e);
                    self.stage = Stage::Choose;
                }
                Event::Imported(Ok(run), chosen) => {
                    self.error = None;
                    if let Some(c) = chosen {
                        self.existing = self.backend.folders();
                        self.chosen.push(c);
                    }
                    self.stage = Stage::Done(Box::new(run));
                }
                Event::Imported(Err(e), _) => {
                    self.error = Some(e);
                    self.stage = Stage::Choose;
                }
            }
        }
    }

    pub fn show(&mut self, ui: &mut Ui, parent: Option<&dyn Parent>) {
        self.poll();
        if self.picker.waiting() || self.backend.busy() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        let t = theme::CHARCOAL;
        theme::title(ui, "Import photos");
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new("Copy the photos from a memory card or a folder into your photo folder. They are then sent to the home PC.").color(t.text_dim)).wrap());
        ui.add_space(12.0);
        match &self.stage {
            Stage::Choose => self.choose(ui, parent),
            Stage::Scanning => self.working(ui, "Looking at the photos", "Cancel"),
            Stage::Scanned(_) => self.scanned(ui),
            Stage::Importing(_) => self.working(ui, "Copying the photos", "Stop"),
            Stage::Done(_) => self.done(ui),
        }
        if let Some(e) = &self.error {
            ui.add_space(10.0);
            ui.add(egui::Label::new(RichText::new(e).color(t.reject)).wrap());
        }
    }

    fn dest_check(&self) -> Result<String, String> {
        self.backend.check(&self.dest)
    }

    fn choose(&mut self, ui: &mut Ui, parent: Option<&dyn Parent>) {
        let t = theme::CHARCOAL;
        theme::panel(ui, Some("Where the photos are"), |ui| {
            let label = theme::field_label(ui, "Memory card or folder to import from");
            ui.horizontal(|ui| {
                let browse = 96.0;
                let room = (ui.available_width() - browse - ui.spacing().item_spacing.x).max(120.0);
                ui.add_sized([room, 30.0], theme::text_edit(&mut self.source).hint_text("/media/card")).labelled_by(label.id);
                if theme::button(ui, "Browse\u{2026}", ButtonKind::Secondary).clicked() {
                    self.picker.start(&self.source, parent);
                }
            });
            theme::hint(ui, "Every photo in it and in the folders below it is looked at. Nothing on it is changed or deleted.");
        });
        theme::gap(ui);
        theme::panel(ui, Some("Where they go"), |ui| {
            let label = theme::field_label(ui, "Folder name in your photo folder");
            ui.add_sized([ui.available_width(), 30.0], theme::text_edit(&mut self.dest).hint_text("261101 - Lisbon")).labelled_by(label.id);
            match (self.dest.trim().is_empty(), self.dest_check()) {
                (true, _) => theme::hint(ui, "For example 261101 - Lisbon. It is created if it is not there yet."),
                (false, Err(why)) => {
                    ui.add(egui::Label::new(RichText::new(why).color(t.reject)).wrap());
                }
                (false, Ok(d)) => {
                    let exists = self.existing.contains(&d);
                    theme::hint(ui, if exists { "This folder exists; the photos are added to it." } else { "A new folder is made for them." });
                }
            }
            if !self.existing.is_empty() {
                ui.add_space(2.0);
                theme::field_label(ui, "Or add to a folder you already have");
                egui::ScrollArea::vertical().max_height(130.0).id_salt("existing-folders").show(ui, |ui| {
                    for f in self.existing.clone() {
                        let on = self.dest.trim().trim_end_matches('/') == f;
                        if ui.selectable_label(on, f.as_str()).clicked() {
                            self.dest = f;
                        }
                    }
                });
            }
        });
        ui.add_space(14.0);
        let ready = !self.source.trim().is_empty() && self.dest_check().is_ok();
        let mut go = false;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(ready, |ui| go = theme::button(ui, "Look at the photos", ButtonKind::Primary).clicked());
            if self.cancelled_scan {
                theme::hint(ui, "The last look was cancelled.");
            }
        });
        if go {
            self.error = None;
            self.cancelled_scan = false;
            let dest = self.dest_check().unwrap_or_default();
            self.stage = Stage::Scanning;
            self.backend.scan(PathBuf::from(self.source.trim()), dest);
        }
    }

    fn working(&mut self, ui: &mut Ui, what: &str, stop: &str) {
        let p = self.backend.progress();
        theme::panel(ui, Some(what), |ui| {
            ui.add(ProgressBar::new(progress_fraction(&p)).show_percentage());
            ui.add(egui::Label::new(RichText::new(progress_line(&p)).color(theme::CHARCOAL.text)).wrap());
            if !p.current.is_empty() {
                theme::hint(ui, &p.current);
            }
            if self.cancelling {
                theme::hint(ui, "Stopping\u{2026}");
            } else if theme::button(ui, stop, ButtonKind::Secondary).clicked() {
                self.cancelling = true;
                self.backend.cancel();
            }
        });
    }

    fn scanned(&mut self, ui: &mut Ui) {
        let t = theme::CHARCOAL;
        let Stage::Scanned(scan) = &self.stage else { return };
        let scan = scan.clone();
        theme::panel(ui, Some("What was found"), |ui| {
            let head = scan_headline(&scan);
            ui.label(RichText::new(head).font(theme::semibold(ui.ctx(), 15.0)).color(t.text));
            if let Some(k) = known_line(&scan) {
                ui.add(egui::Label::new(RichText::new(k).color(t.text)).wrap());
            }
            if scan.twins > 0 {
                theme::hint(ui, &format!("{} on the card {} the same photo as another, and {} imported once.", scan.twins, if scan.twins == 1 { "is" } else { "are" }, if scan.twins == 1 { "is" } else { "are" }));
            }
            let arrived = scan.new.iter().filter(|f| f.arrived.is_some()).count();
            if arrived > 0 {
                theme::hint(ui, &format!("{arrived} of the new photos were copied by an earlier try and only have to be queued."));
            }
            ui.add(egui::Label::new(RichText::new(space_line(&scan)).color(t.text)).wrap());
            theme::hint(ui, &format!("Into the folder \"{}\".", scan.dest));
            if let Some(p) = scan.space_problem() {
                ui.add(egui::Label::new(RichText::new(p).color(t.reject)).wrap());
            }
            if !scan.library_checked {
                ui.add(egui::Label::new(RichText::new("The cloud library could not be reached, so only this PC was checked. A photo that is already in the library is recognised when it is sent.").color(t.caution)).wrap());
            }
            if !scan.skipped.is_empty() {
                egui::CollapsingHeader::new(RichText::new(format!("{} not imported", count(scan.skipped.len(), "file"))).color(t.caution)).id_salt("skipped").show(ui, |ui| {
                    for s in scan.skipped.iter().take(30) {
                        theme::hint(ui, s);
                    }
                    if scan.skipped.len() > 30 {
                        theme::hint(ui, &format!("and {} more", scan.skipped.len() - 30));
                    }
                });
            }
        });
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            let n = scan.new.len();
            if n > 0 {
                ui.add_enabled_ui(scan.space_problem().is_none(), |ui| {
                    if theme::button(ui, &format!("Import {}", count(n, "photo")), ButtonKind::Primary).clicked() {
                        self.error = None;
                        self.stage = Stage::Importing(scan.clone());
                        self.backend.import(*scan.clone());
                    }
                });
            }
            if theme::button(ui, "Back", ButtonKind::Secondary).clicked() {
                self.stage = Stage::Choose;
            }
        });
    }

    fn done(&mut self, ui: &mut Ui) {
        let t = theme::CHARCOAL;
        let Stage::Done(run) = &self.stage else { return };
        let lines = summary_lines(run);
        let (cancelled, no_space) = (run.cancelled, run.no_space.clone());
        theme::panel(ui, Some(if cancelled { "Stopped" } else { "Done" }), |ui| {
            for (i, l) in lines.iter().enumerate() {
                let color = if i == 0 { t.text } else { t.text_label };
                ui.add(egui::Label::new(RichText::new(l).color(color)).wrap());
            }
            if let Some(m) = &no_space {
                ui.add(egui::Label::new(RichText::new(m).color(t.reject)).wrap());
            }
        });
        ui.add_space(14.0);
        if theme::button(ui, "Import more", ButtonKind::Secondary).clicked() {
            self.stage = Stage::Choose;
            self.source.clear();
        }
    }
}

fn count(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

/// "12 new photos (1.4 GiB)".
pub fn scan_headline(s: &ImportScan) -> String {
    match s.new.len() {
        0 => "No new photos".to_string(),
        n => format!("{} ({})", if n == 1 { "1 new photo".to_string() } else { format!("{n} new photos") }, human_size(s.new.iter().map(|f| f.size + f.sidecar.as_ref().map_or(0, |c| c.size)).sum())),
    }
}

/// "3 already in the library or on this PC", or None when there are none.
pub fn known_line(s: &ImportScan) -> Option<String> {
    let k = s.known();
    (k > 0).then(|| format!("{k} already in the library or on this PC"))
}

/// "To copy: 1.4 GiB. Free space: 220.0 GiB."
pub fn space_line(s: &ImportScan) -> String {
    match s.free {
        Some(f) => format!("To copy: {}. Free space: {}.", human_size(s.new_bytes), human_size(f)),
        None => format!("To copy: {}. Free space: unknown.", human_size(s.new_bytes)),
    }
}

pub fn progress_fraction(p: &Progress) -> f32 {
    if p.bytes_total > 0 {
        (p.bytes_done as f64 / p.bytes_total as f64).clamp(0.0, 1.0) as f32
    } else if p.total > 0 {
        (p.done as f64 / p.total as f64).clamp(0.0, 1.0) as f32
    } else {
        0.0
    }
}

pub fn progress_line(p: &Progress) -> String {
    match p.phase {
        Phase::Looking => format!("Looking for photos\u{2026} {} found", p.done),
        Phase::Hashing => format!("Checking photo {} of {} ({} of {})", (p.done + 1).min(p.total.max(1)), p.total, human_size(p.bytes_done), human_size(p.bytes_total)),
        Phase::Copying => format!("Copying photo {} of {} ({} of {})", (p.done + 1).min(p.total.max(1)), p.total, human_size(p.bytes_done), human_size(p.bytes_total)),
    }
}

/// The lines of the final summary; the first is the headline.
pub fn summary_lines(r: &ImportRun) -> Vec<String> {
    let mut v = Vec::new();
    let did = r.copied + r.resumed;
    v.push(if r.cancelled {
        format!("Stopped after {} into \"{}\". {} left; look again and import to finish.", count(did, "photo"), r.dest, count(r.left, "photo"))
    } else {
        format!("{} into \"{}\".", if did == 1 { "Imported 1 photo".to_string() } else { format!("Imported {did} photos") }, r.dest)
    });
    if r.resumed > 0 {
        v.push(format!("{} had been copied by an earlier try and only had to be queued.", count(r.resumed, "photo")));
    }
    if !r.renamed.is_empty() {
        v.push(format!("{} got another name because that name was taken (nothing was overwritten).", count(r.renamed.len(), "photo")));
    }
    for n in &r.notes {
        v.push(n.clone());
    }
    for f in r.failed.iter().take(10) {
        v.push(format!("Not imported: {f}"));
    }
    if r.failed.len() > 10 {
        v.push(format!("and {} more not imported", r.failed.len() - 10));
    }
    if did > 0 {
        v.push("They are sent to the home PC in the background, whenever this PC is online; Status shows how far that is. LightCraft is told about them too.".into());
        v.push("This folder is now one this PC keeps.".into());
    }
    v
}

// ------------------------------------------------------------------ the real backend

/// Runs the scan and the copy on a thread of its own, with the sync crate's [`Other`](lightcraft_cloud_client::other::Other) built from
/// the config file.
pub struct ThreadBackend {
    config_path: PathBuf,
    exclude: Vec<String>,
    photo_folder: PathBuf,
    chosen: Vec<String>,
    rx: Option<Receiver<Event>>,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<Progress>>,
}

impl ThreadBackend {
    /// `None` when the config is not another PC's.
    pub fn new(config_path: &Path) -> Option<ThreadBackend> {
        let cfg = Config::load(Some(config_path)).ok().filter(|c| c.file.role == Role::Other)?;
        Some(ThreadBackend {
            config_path: config_path.to_path_buf(),
            exclude: cfg.file.exclude.clone(),
            photo_folder: cfg.source.clone(),
            chosen: cfg.file.folders.clone(),
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Mutex::new(Progress::default())),
        })
    }

    fn other(config_path: &Path) -> Result<lightcraft_cloud_client::other::Other, String> {
        let cfg = Config::load(Some(config_path))?;
        if cfg.file.role != Role::Other {
            return Err("This PC is not set up as another PC.".into());
        }
        let mut other = lightcraft_cloud_client::other::Other::new(cfg.other_config()?, cfg.open_remote()?).map_err(|e| e.to_string())?;
        if cfg.file.lightcraft_control_port != 0 {
            let list = PendingList::new(&cfg.state_dir);
            other.set_adopted_hook(move |p| list.add(p, &lightcraft_cloud_client::home::utc_iso(std::time::SystemTime::now())));
        }
        let logfile = crate::worker::log_path(&cfg.state_dir);
        other.set_logger(move |m| {
            use std::io::Write;
            if let Some(dir) = logfile.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&logfile) {
                let _ = writeln!(f, "{} {m}", lightcraft_cloud_client::home::utc_iso(std::time::SystemTime::now()));
            }
        });
        Ok(other)
    }

    fn start(&mut self, job: impl FnOnce(Arc<Mutex<Progress>>, Arc<AtomicBool>) -> Event + Send + 'static) {
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.cancel.store(false, Ordering::SeqCst);
        *self.progress.lock().unwrap_or_else(|e| e.into_inner()) = Progress::default();
        let (progress, cancel) = (self.progress.clone(), self.cancel.clone());
        std::thread::spawn(move || {
            let _ = tx.send(job(progress, cancel));
        });
    }
}

impl ImportBackend for ThreadBackend {
    fn folders(&self) -> Vec<String> {
        existing_folders(&self.photo_folder, &self.chosen, &self.exclude)
    }

    fn check(&self, dest: &str) -> Result<String, String> {
        check_destination(dest, &self.exclude)
    }

    fn scan(&mut self, source: PathBuf, dest: String) {
        let path = self.config_path.clone();
        self.start(move |progress, cancel| {
            let r = Self::other(&path).and_then(|o| {
                o.import_scan(&source, &dest, &mut |p| *progress.lock().unwrap_or_else(|e| e.into_inner()) = p.clone(), &cancel).map_err(|e| e.to_string())
            });
            Event::Scanned(r)
        });
    }

    fn import(&mut self, scan: ImportScan) {
        let path = self.config_path.clone();
        self.start(move |progress, cancel| {
            let r = Self::other(&path).and_then(|o| o.import_run(&scan, &mut |p| *progress.lock().unwrap_or_else(|e| e.into_inner()) = p.clone(), &cancel).map_err(|e| e.to_string()));
            match r {
                Ok(run) => {
                    // the destination becomes a folder this PC keeps, so the photos the home PC publishes are matched and kept up to date
                    let chosen = (run.copied + run.resumed > 0).then(|| config::add_chosen_folder(&path, &run.dest)).and_then(|r| r.ok()).filter(|added| *added).map(|_| run.dest.clone());
                    Event::Imported(Ok(run), chosen)
                }
                Err(e) => Event::Imported(Err(e), None),
            }
        });
    }

    fn cancel(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    fn progress(&self) -> Progress {
        self.progress.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn poll(&mut self) -> Vec<Event> {
        let Some(rx) = &self.rx else { return Vec::new() };
        match rx.try_recv() {
            Ok(ev) => {
                self.rx = None;
                vec![ev]
            }
            Err(TryRecvError::Empty) => Vec::new(),
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                vec![Event::Scanned(Err("The import stopped unexpectedly.".into()))]
            }
        }
    }
}
