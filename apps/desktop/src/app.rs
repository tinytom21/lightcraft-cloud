//! The always-running app: the background engine on a thread, the folder watcher, the tray icon on the main thread, and the
//! windows as child processes. `run` starts it; a second launch finds the instance lock taken and says so.
//!
//! ```text
//! main thread    tray icon + menu  <-- status messages --  engine thread (Service: scheduler + HomeWorker)
//!                      |  menu clicks --> commands -------->     ^
//! config thread   watches the config file (reload) and the photo folder (changes) -> commands
//! windows         `lightcraft-cloud-desktop ui status|settings`: separate processes; they read status.json and write the config
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime};

use lightcraft_cloud_client::home::utc_iso;

use crate::appstate::AppStatus;
use crate::clock::SystemClock;
use crate::config::{self, Config, File};
use crate::engine::LazyRenderer;
use crate::instance::InstanceLock;
use crate::notifier::{DesktopNotifier, NoNotifier, Notifier};
use crate::scheduler::Timing;
use crate::service::{Command, Service, run_loop};
use crate::watcher::FolderWatcher;
use crate::worker::{HomeWorker, log_path};

pub struct RunOptions {
    pub config: Option<PathBuf>,
    /// No tray, no windows, no desktop notifications (for a server or a test).
    pub headless: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    Quit,
    /// Another instance holds the lock; this one did nothing.
    AlreadyRunning,
}

enum Msg {
    Status(Box<AppStatus>),
    #[cfg(target_os = "linux")]
    Menu(tray_icon::menu::MenuId),
    EngineStopped,
}

/// The state folder: the config's `state_dir`, else the default.
pub fn state_dir_for(config_path: &Path) -> Result<PathBuf, String> {
    let file: Option<File> = std::fs::read_to_string(config_path).ok().and_then(|t| toml::from_str(&t).ok());
    match file.and_then(|f| f.state_dir) {
        Some(d) => config::expand(&d),
        None => config::default_state_dir(),
    }
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

fn append_log(state_dir: &Path, line: &str) {
    let f = log_path(state_dir);
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut h) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
        let _ = writeln!(h, "{} {line}", utc_iso(SystemTime::now()));
    }
}

/// Watches the config file and the photo folder it names; sends the engine commands.
fn spawn_config_thread(config_path: PathBuf, cmd: Sender<Command>, stop: Receiver<()>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut last = stamp(&config_path);
        let mut _watcher: Option<FolderWatcher> = None;
        let mut first = true;
        loop {
            if first || stamp(&config_path) != last {
                last = stamp(&config_path);
                _watcher = None; // (stop watching the old folder first)
                if let Ok(c) = Config::load(Some(&config_path)) {
                    let tx = cmd.clone();
                    match FolderWatcher::start(&c.source, c.file.exclude.clone(), move || {
                        let _ = tx.send(Command::FolderChanged);
                    }) {
                        Ok(w) => _watcher = Some(w),
                        Err(e) => log::warn!("{e}"),
                    }
                }
                if !first {
                    let _ = cmd.send(Command::ConfigChanged);
                }
                first = false;
            }
            if stop.recv_timeout(Duration::from_secs(2)).is_ok() {
                return;
            }
        }
    })
}

/// Windows opened from the tray, one per kind; a window already open is left alone.
struct Windows {
    exe: PathBuf,
    config: Option<PathBuf>,
    open: Vec<(&'static str, Child)>,
}

impl Windows {
    fn open(&mut self, kind: &'static str) {
        self.open.retain_mut(|(_, c)| c.try_wait().ok().flatten().is_none());
        if self.open.iter().any(|(k, _)| *k == kind) {
            return;
        }
        let mut cmd = std::process::Command::new(&self.exe);
        cmd.arg("ui").arg(kind);
        if let Some(c) = &self.config {
            cmd.arg("--config").arg(c);
        }
        match cmd.spawn() {
            Ok(child) => self.open.push((kind, child)),
            Err(e) => log::warn!("cannot open the {kind} window: {e}"),
        }
    }

    fn close_all(&mut self) {
        for (_, c) in &mut self.open {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

pub fn run(opts: RunOptions) -> Result<Ended, String> {
    let config_path = Config::resolve_path(opts.config.as_deref())?;
    let state_dir = state_dir_for(&config_path)?;
    let Some(_instance) = InstanceLock::acquire(&state_dir)? else { return Ok(Ended::AlreadyRunning) };
    append_log(&state_dir, "app started");

    let (cmd_tx, cmd_rx) = channel::<Command>();
    let (msg_tx, msg_rx) = channel::<Msg>();
    let (stop_tx, stop_rx) = channel::<()>();

    let config_thread = spawn_config_thread(config_path.clone(), cmd_tx.clone(), stop_rx);

    let engine = {
        let (config_path, state_dir, msg_tx, headless) = (config_path.clone(), state_dir.clone(), msg_tx.clone(), opts.headless);
        std::thread::spawn(move || {
            let clock: Arc<dyn crate::clock::Clock> = Arc::new(SystemClock);
            let worker = HomeWorker::new(
                config_path,
                Box::new(|cfg: &Config| Box::new(LazyRenderer::new(cfg.state_dir.join("lib")))),
                Box::new(|cfg: &Config| cfg.open_remote()),
                clock.clone(),
            );
            let notifier: Box<dyn Notifier> = if headless { Box::new(NoNotifier) } else { Box::new(DesktopNotifier) };
            let sink_tx = msg_tx.clone();
            let log_dir = state_dir.clone();
            let mut service = Service::new(
                worker,
                clock.clone(),
                notifier,
                state_dir,
                Timing::default(),
                Box::new(move |s| {
                    let _ = sink_tx.send(Msg::Status(Box::new(s.clone())));
                }),
                Box::new(move |m| append_log(&log_dir, m)),
            );
            run_loop(&mut service, &cmd_rx, &*clock);
            let _ = msg_tx.send(Msg::EngineStopped);
        })
    };

    let result = if opts.headless { main_loop_headless(&msg_rx) } else { main_loop_tray(&opts, &config_path, &state_dir, &cmd_tx, &msg_tx, &msg_rx) };

    let _ = cmd_tx.send(Command::Quit);
    let _ = stop_tx.send(());
    let _ = engine.join();
    let _ = config_thread.join();
    append_log(&state_dir, "app stopped");
    result.map(|()| Ended::Quit)
}

/// Without a tray the main thread only waits for the engine to stop (a signal ends the process).
fn main_loop_headless(msg_rx: &Receiver<Msg>) -> Result<(), String> {
    while let Ok(m) = msg_rx.recv() {
        if matches!(m, Msg::EngineStopped) {
            break;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn main_loop_tray(
    opts: &RunOptions,
    config_path: &Path,
    state_dir: &Path,
    cmd_tx: &Sender<Command>,
    msg_tx: &Sender<Msg>,
    msg_rx: &Receiver<Msg>,
) -> Result<(), String> {
    use crate::tray::{MenuAction, Tray};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut windows = Windows { exe, config: opts.config.clone(), open: Vec::new() };
    let initial = AppStatus::load(state_dir).unwrap_or_default();
    let mut tray = match Tray::new(&initial) {
        Ok(t) => Some(t),
        Err(e) => {
            append_log(state_dir, &format!("no tray icon: {e}"));
            eprintln!("lightcraft-cloud-desktop: no tray icon ({e}); the app keeps running");
            None
        }
    };
    let tx = msg_tx.clone();
    tray_icon::menu::MenuEvent::set_event_handler(Some(move |e: tray_icon::menu::MenuEvent| {
        let _ = tx.send(Msg::Menu(e.id));
    }));
    if !config_path.exists() {
        windows.open("settings"); // first run: Setup instead of syncing
    }
    let mut paused = initial.paused;
    while let Ok(m) = msg_rx.recv() {
        match m {
            Msg::Status(s) => {
                paused = s.paused;
                if let Some(t) = tray.as_mut() {
                    t.update(&s);
                }
            }
            Msg::Menu(id) => match tray.as_ref().and_then(|t| t.action(&id)) {
                Some(MenuAction::SyncNow) => {
                    let _ = cmd_tx.send(Command::SyncNow);
                }
                Some(MenuAction::PauseResume) => {
                    let _ = cmd_tx.send(if paused { Command::Resume } else { Command::Pause });
                }
                Some(MenuAction::Status) => windows.open("status"),
                Some(MenuAction::Settings) => windows.open("settings"),
                Some(MenuAction::Quit) => break,
                None => {}
            },
            Msg::EngineStopped => break,
        }
    }
    windows.close_all();
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main_loop_tray(
    _opts: &RunOptions,
    _config_path: &Path,
    _state_dir: &Path,
    _cmd_tx: &Sender<Command>,
    _msg_tx: &Sender<Msg>,
    _msg_rx: &Receiver<Msg>,
) -> Result<(), String> {
    Err("the tray icon is only built for Linux so far; use `run --headless`".into())
}
