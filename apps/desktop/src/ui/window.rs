//! The eframe shell: one window with a Status tab and a Settings tab. The tray starts it as `lightcraft-cloud-desktop ui
//! <tab>` in a process of its own, so a problem in a window cannot stop the background engine, and the window needs no
//! channel to it: it reads `status.json` and writes the config file, which the app notices.

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::appstate::AppStatus;
use crate::autostart;
use crate::config::{self, Config, File};
use crate::instance::InstanceLock;
use crate::settings::{Form, SaveEnv};
use crate::ui::import_panel::{ImportPanel, ThreadBackend};
use crate::ui::settings_panel::SettingsPanel;
use crate::ui::status::{self, StatusAction, StatusView};
use lightcraft_cloud_client::other::ImportQueue;
use crate::ui::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Status,
    /// Another PC only: import photos from a card or a folder.
    Import,
    Settings,
}

impl Tab {
    pub fn parse(s: &str) -> Option<Tab> {
        match s {
            "status" => Some(Tab::Status),
            "import" => Some(Tab::Import),
            "settings" | "setup" => Some(Tab::Settings),
            _ => None,
        }
    }
}

/// Reads tab names ("status", "settings", one per line) from `input` until it ends: the app writes one to the open window's
/// stdin when a tray click asks for a window that is already there. Each one sets `pending`, raises the window and repaints.
/// The end of the input is not a request to close: a window started by hand has no app behind it.
pub fn read_tab_requests(input: impl BufRead, pending: Arc<Mutex<Option<Tab>>>, ctx: egui::Context) {
    for line in input.lines() {
        let Ok(line) = line else { return };
        if let Some(tab) = Tab::parse(line.trim()) {
            *pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(tab);
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
    }
}

struct Window {
    tab: Tab,
    /// A tab asked for by the app (see [`read_tab_requests`]).
    pending: Arc<Mutex<Option<Tab>>>,
    /// The title still says "Set up ..." until the first save.
    retitle: bool,
    settings: SettingsPanel,
    /// Another PC only.
    import: Option<ImportPanel>,
    state_dir: PathBuf,
    status: Option<AppStatus>,
    running: bool,
    /// Without a config only Setup is offered.
    setup_only: bool,
}

impl Window {
    fn refresh(&mut self) {
        self.status = AppStatus::load(&self.state_dir);
        self.running = InstanceLock::is_held(&self.state_dir);
    }
}

impl eframe::App for Window {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.refresh();
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        let asked = self.pending.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(t) = asked.filter(|_| !self.setup_only) {
            self.tab = if t == Tab::Import && self.import.is_none() { Tab::Status } else { t };
        }
        if !self.setup_only {
            let has_import = self.import.is_some();
            theme::top_bar(ui, |ui| {
                for (tab, label) in [(Tab::Status, "Status"), (Tab::Import, "Import"), (Tab::Settings, "Settings")] {
                    if tab == Tab::Import && !has_import {
                        continue;
                    }
                    if theme::tab(ui, self.tab == tab, label).clicked() {
                        self.tab = tab;
                    }
                }
            });
        }
        theme::page(ui, |ui| match self.tab {
            Tab::Status if !self.setup_only => {
                let queue = ImportQueue::new(&self.state_dir);
                let refused = if self.import.is_some() { status::refused_lines(&queue.refused()) } else { Vec::new() };
                match status::show_with_actions(ui, &StatusView { status: self.status.as_ref(), running: self.running, now: SystemTime::now() }, &refused) {
                    // (the app picks the change up from the queue folder; the photos stay where they are)
                    Some(StatusAction::TryAgain) => {
                        queue.retry_refused();
                    }
                    Some(StatusAction::KeepHere) => {
                        queue.keep_refused();
                    }
                    None => {}
                }
            }
            Tab::Import if !self.setup_only && self.import.is_some() => {
                if let Some(panel) = self.import.as_mut() {
                    panel.show(ui, Some(frame));
                    // (the destination is a folder this PC keeps now: Settings must not save it away again)
                    for f in panel.take_chosen() {
                        self.settings.form.set_folder(&f, true);
                    }
                }
            }
            _ => {
                self.settings.show_with_parent(ui, Some(frame));
                if self.settings.saved && self.setup_only {
                    self.setup_only = false;
                    self.tab = Tab::Status;
                }
                if self.settings.saved && self.retitle {
                    self.retitle = false;
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title("LightCraft Cloud".into()));
                }
            }
        });
    }
}

/// Open the window. `config_path`: `--config`, else the default.
pub fn run_window(tab: Tab, config_path: Option<PathBuf>) -> Result<(), String> {
    let path = Config::resolve_path(config_path.as_deref())?;
    let (file, exists) = match std::fs::read_to_string(&path) {
        Ok(t) => (toml::from_str::<File>(&t).map_err(|e| format!("config {}: {e}", path.display()))?, true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (File::default(), false),
        Err(e) => return Err(format!("config {}: {e}", path.display())),
    };
    let autostart_dir = autostart::default_dir()?;
    let login = autostart::is_enabled(&autostart_dir);
    let form = if exists { Form::from_file(&file, login) } else { Form::first_run(login) };
    let state_dir = match &file.state_dir {
        Some(d) => config::expand(d)?,
        None => config::default_state_dir()?,
    };
    let remotes = list_remotes(&file);
    let env = SaveEnv { config_path: path.clone(), autostart_dir, exe: std::env::current_exe().map_err(|e| e.to_string())? };
    let setup_only = !exists;
    let import = if exists && file.role == config::Role::Other { ThreadBackend::new(&path).map(|b| ImportPanel::new(Box::new(b))) } else { None };
    let title = if setup_only { "Set up LightCraft Cloud" } else { "LightCraft Cloud" };
    let import_missing = import.is_none();
    let mut win = Window {
        tab: if setup_only { Tab::Settings } else if tab == Tab::Import && import_missing { Tab::Status } else { tab },
        pending: Arc::new(Mutex::new(None)),
        retitle: setup_only,
        settings: SettingsPanel::new(form, remotes, env, setup_only),
        import,
        state_dir,
        status: None,
        running: false,
        setup_only,
    };
    win.refresh();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title(title).with_inner_size([620.0, 700.0]).with_min_inner_size([480.0, 420.0]),
        ..eframe::NativeOptions::default()
    };
    let pending = win.pending.clone();
    eframe::run_native(
        title,
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || read_tab_requests(std::io::stdin().lock(), pending, ctx));
            Ok(Box::new(win))
        }),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// The names of the rclone remotes (the config's contents are not read out).
fn list_remotes(file: &File) -> Result<Vec<String>, String> {
    let conf = match &file.rclone_config {
        Some(c) => config::expand(c)?,
        None => match std::env::var_os("RCLONE_CONFIG").filter(|v| !v.is_empty()) {
            Some(c) => PathBuf::from(c),
            None => config::home_dir()?.join(".config/rclone/rclone.conf"),
        },
    };
    let lib = config::find_librclone(file.librclone.as_deref())?;
    lightcraft_cloud_client::list_remotes(lib.as_deref(), &conf).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_set_the_tab_and_the_end_of_input_changes_nothing() {
        let pending = Arc::new(Mutex::new(None));
        read_tab_requests(std::io::Cursor::new("status\nnonsense\nsettings\n"), pending.clone(), egui::Context::default());
        assert_eq!(*pending.lock().unwrap(), Some(Tab::Settings));
        let pending = Arc::new(Mutex::new(None));
        read_tab_requests(std::io::Cursor::new(""), pending.clone(), egui::Context::default());
        assert_eq!(*pending.lock().unwrap(), None);
    }
}
