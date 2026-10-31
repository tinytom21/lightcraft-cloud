//! The eframe shell: one window with a Status tab and a Settings tab. The tray starts it as `lightcraft-cloud-desktop ui
//! <tab>` in a process of its own, so a problem in a window cannot stop the background engine, and the window needs no
//! channel to it: it reads `status.json` and writes the config file, which the app notices.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::appstate::AppStatus;
use crate::autostart;
use crate::config::{self, Config, File};
use crate::instance::InstanceLock;
use crate::settings::{Form, SaveEnv};
use crate::ui::settings_panel::SettingsPanel;
use crate::ui::status::{self, StatusView};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Status,
    Settings,
}

impl Tab {
    pub fn parse(s: &str) -> Option<Tab> {
        match s {
            "status" => Some(Tab::Status),
            "settings" | "setup" => Some(Tab::Settings),
            _ => None,
        }
    }
}

struct Window {
    tab: Tab,
    settings: SettingsPanel,
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.refresh();
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        if !self.setup_only {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Status, "Status");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
            });
            ui.separator();
        }
        egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
            Tab::Status if !self.setup_only => {
                status::show(ui, &StatusView { status: self.status.as_ref(), running: self.running, now: SystemTime::now() })
            }
            _ => {
                self.settings.show(ui);
                if self.settings.saved && self.setup_only {
                    self.setup_only = false;
                    self.tab = Tab::Status;
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
    let env = SaveEnv { config_path: path, autostart_dir, exe: std::env::current_exe().map_err(|e| e.to_string())? };
    let setup_only = !exists;
    let title = if setup_only { "Set up LightCraft Cloud" } else { "LightCraft Cloud" };
    let mut win = Window {
        tab: if setup_only { Tab::Settings } else { tab },
        settings: SettingsPanel::new(form, remotes, env, setup_only),
        state_dir,
        status: None,
        running: false,
        setup_only,
    };
    win.refresh();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title(title).with_inner_size([620.0, 640.0]).with_min_inner_size([480.0, 420.0]),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(title, options, Box::new(|_cc| Ok(Box::new(win)))).map_err(|e| e.to_string())?;
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
    let lib: Option<PathBuf> = match &file.librclone {
        Some(l) => Some(config::expand(l)?),
        None => std::env::var_os("LIBRCLONE").filter(|v| !v.is_empty()).map(PathBuf::from),
    };
    lightcraft_cloud_client::list_remotes(lib.as_deref(), &conf).map_err(|e| e.to_string())
}
