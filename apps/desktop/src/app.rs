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
use std::process::{Child, ChildStdin, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime};

use lightcraft_cloud_client::home::utc_iso;

use crate::appstate::AppStatus;
use crate::clock::SystemClock;
use crate::config::{self, Config, File};
use crate::instance::InstanceLock;
use crate::notifier::{DesktopNotifier, NoNotifier, Notifier};
use crate::scheduler::Timing;
use crate::service::{Command, Service, run_loop};
use crate::update::{self, UpdateState, Updater};
use crate::watcher::FolderWatcher;
use crate::worker::{OtherWorker, RoleWorker, log_path};

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
    /// The update state changed (see [`crate::update`]).
    Update(Box<UpdateState>),
    #[cfg(any(target_os = "linux", windows))]
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

/// `<state>/quit-request`: how `lightcraft-cloud-desktop quit` asks the running app to stop (the installer uses it). The app checks for it
/// every two seconds, deletes it, and stops after the job it is running (see [`request_quit`]).
pub fn quit_request_path(state_dir: &Path) -> PathBuf {
    state_dir.join("quit-request")
}

/// `<state>/sync-request`: how the Status window (a process of its own) asks the running app for "Sync now", like the tray menu. The
/// app checks for it every two seconds and deletes it.
pub fn sync_request_path(state_dir: &Path) -> PathBuf {
    state_dir.join("sync-request")
}

/// Ask the running app for "Sync now" (see [`sync_request_path`]).
pub fn request_sync(state_dir: &Path) -> Result<(), String> {
    std::fs::write(sync_request_path(state_dir), "sync").map_err(|e| format!("cannot ask the app to sync: {e}"))
}

/// What asking the app to quit came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitOutcome {
    /// No app was running.
    NotRunning,
    /// It was running and has ended.
    Stopped,
    /// It took the request but is still running after the wait (a long sync: it ends when the sync does).
    StillRunning,
    /// An app is running that did not take the request: a version from before `quit` existed.
    NotAnswered,
}

/// Ask the app running on `state_dir` to quit, and wait for it. It takes the request within a few seconds (`ack_wait`) unless it is too
/// old to know the request; then it finishes the job it is running, if any (a sync cannot be interrupted politely), and ends within
/// `exit_wait` or not. Nothing is touched when no app runs.
pub fn request_quit(state_dir: &Path, ack_wait: Duration, exit_wait: Duration) -> Result<QuitOutcome, String> {
    let file = quit_request_path(state_dir);
    if !InstanceLock::is_held(state_dir) {
        let _ = std::fs::remove_file(&file);
        return Ok(QuitOutcome::NotRunning);
    }
    std::fs::write(&file, b"quit\n").map_err(|e| format!("{}: {e}", file.display()))?;
    let poll = Duration::from_millis(100);
    let start = std::time::Instant::now();
    while file.exists() {
        if !InstanceLock::is_held(state_dir) {
            let _ = std::fs::remove_file(&file);
            return Ok(QuitOutcome::Stopped);
        }
        if start.elapsed() >= ack_wait {
            let _ = std::fs::remove_file(&file); // (an old app never reads it; a new one must not find it later)
            return Ok(QuitOutcome::NotAnswered);
        }
        std::thread::sleep(poll);
    }
    let start = std::time::Instant::now();
    while InstanceLock::is_held(state_dir) {
        if start.elapsed() >= exit_wait {
            return Ok(QuitOutcome::StillRunning);
        }
        std::thread::sleep(poll);
    }
    Ok(QuitOutcome::Stopped)
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
        // (the Status window changes the queue from another process: its queue folder changing wakes the upload job)
        let queue_dir = state_dir_for(&config_path).ok().map(|d| d.join(lightcraft_cloud_client::other::OUT_DIR));
        let quit_file = state_dir_for(&config_path).ok().map(|d| quit_request_path(&d));
        let sync_file = state_dir_for(&config_path).ok().map(|d| sync_request_path(&d));
        let queue_mtime = || queue_dir.as_deref().and_then(|d| std::fs::metadata(d).ok()).and_then(|m| m.modified().ok());
        let mut queue_stamp = queue_mtime();
        loop {
            if let Some(q) = quit_file.as_deref()
                && q.exists()
                && std::fs::remove_file(q).is_ok()
            {
                let _ = cmd.send(Command::Quit);
            }
            if let Some(f) = sync_file.as_deref()
                && f.exists()
                && std::fs::remove_file(f).is_ok()
            {
                let _ = cmd.send(Command::SyncNow);
            }
            let now = queue_mtime();
            if now != queue_stamp {
                queue_stamp = now;
                if now.is_some() {
                    let _ = cmd.send(Command::UploadsChanged);
                }
            }
            if first || stamp(&config_path) != last {
                last = stamp(&config_path);
                _watcher = None; // (stop watching the old folder first)
                if let Ok(c) = Config::load(Some(&config_path)) {
                    let tx = cmd.clone();
                    // (another PC looks at sidecars and photo files only)
                    let sidecars_only = c.file.role == config::Role::Other;
                    match FolderWatcher::start_filtered(&c.source, c.file.exclude.clone(), sidecars_only, move || {
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

/// The check for a newer version, and the update itself after a click, on a thread of its own: a download must not hold up anything else.
/// It never joins at the end (an update that is downloading when the app quits is simply abandoned; its half file is cleaned up at the next start).
fn spawn_update_thread(config_path: PathBuf, state_dir: PathBuf, busy: Arc<AtomicBool>, msg_tx: Sender<Msg>, headless: bool, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let notifier: Box<dyn Notifier> = if headless { Box::new(NoNotifier) } else { Box::new(DesktopNotifier) };
        let log_dir = state_dir.clone();
        let mut updater = Updater::new(&state_dir, SystemTime::now(), Box::new(move || !busy.load(Ordering::SeqCst)), notifier, Box::new(move |m| append_log(&log_dir, m)));
        let mut sent: Option<UpdateState> = None;
        while !stop.load(Ordering::SeqCst) {
            // (a config that cannot be read is the defaults: checking on)
            let file: File = std::fs::read_to_string(&config_path).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default();
            updater.tick(SystemTime::now(), update::Options { enabled: file.check_for_updates, notify: file.notify });
            if sent.as_ref() != Some(updater.state()) {
                sent = Some(updater.state().clone());
                let _ = msg_tx.send(Msg::Update(Box::new(updater.state().clone())));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// The one window the tray opens (it has a Status and a Settings tab). A click while it is open writes the tab's name to its
/// stdin; the window switches and raises itself (see `ui::window::read_tab_requests`).
struct Windows {
    exe: PathBuf,
    config: Option<PathBuf>,
    open: Option<(Child, ChildStdin)>,
}

impl Windows {
    fn open(&mut self, kind: &'static str) {
        if let Some((child, stdin)) = &mut self.open
            && child.try_wait().ok().flatten().is_none()
            && writeln!(stdin, "{kind}").and_then(|()| stdin.flush()).is_ok()
        {
            return;
        }
        self.close_all(); // (gone, or not listening: start over)
        let mut cmd = std::process::Command::new(&self.exe);
        cmd.arg("ui").arg(kind).stdin(Stdio::piped());
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // (CREATE_NO_WINDOW: no console flashes up)
        if let Some(c) = &self.config {
            cmd.arg("--config").arg(c);
        }
        match cmd.spawn() {
            Ok(mut child) => match child.stdin.take() {
                Some(stdin) => self.open = Some((child, stdin)),
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            },
            Err(e) => log::warn!("cannot open the {kind} window: {e}"),
        }
    }

    fn close_all(&mut self) {
        if let Some((mut c, stdin)) = self.open.take() {
            drop(stdin);
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Windows {
    fn drop(&mut self) {
        self.close_all();
    }
}

/// The worker for the config's role. The full build can be the home PC (publishing, with LightCraft's engine as the renderer) or
/// another PC; the light build only another PC.
fn make_worker(config_path: PathBuf, clock: Arc<dyn crate::clock::Clock>) -> RoleWorker {
    #[cfg(feature = "home")]
    let home: Option<Box<dyn crate::service::Worker>> = {
        use crate::engine::LazyRenderer;
        Some(Box::new(crate::worker::HomeWorker::new(
            config_path.clone(),
            Box::new(|cfg: &Config| Box::new(LazyRenderer::new(cfg.state_dir.join("lib")))),
            Box::new(|cfg: &Config| cfg.open_remote()),
            clock.clone(),
        )))
    };
    #[cfg(not(feature = "home"))]
    let home: Option<Box<dyn crate::service::Worker>> = None;
    let other = OtherWorker::new(config_path.clone(), Box::new(|cfg: &Config| cfg.open_remote()), clock);
    RoleWorker::new(config_path, home, Box::new(other))
}

pub fn run(opts: RunOptions) -> Result<Ended, String> {
    let config_path = Config::resolve_path(opts.config.as_deref())?;
    let state_dir = state_dir_for(&config_path)?;
    let Some(_instance) = InstanceLock::acquire(&state_dir)? else { return Ok(Ended::AlreadyRunning) };
    append_log(&state_dir, "app started");
    let _ = std::fs::remove_file(quit_request_path(&state_dir)); // (a request from before this start is not for this run)
    let _ = std::fs::remove_file(sync_request_path(&state_dir));

    let (cmd_tx, cmd_rx) = channel::<Command>();
    let (msg_tx, msg_rx) = channel::<Msg>();
    let (stop_tx, stop_rx) = channel::<()>();

    let config_thread = spawn_config_thread(config_path.clone(), cmd_tx.clone(), stop_rx);
    let busy = Arc::new(AtomicBool::new(false));
    let stop_updates = Arc::new(AtomicBool::new(false));
    spawn_update_thread(config_path.clone(), state_dir.clone(), busy.clone(), msg_tx.clone(), opts.headless, stop_updates.clone());

    let engine = {
        let (config_path, state_dir, msg_tx, headless, busy) = (config_path.clone(), state_dir.clone(), msg_tx.clone(), opts.headless, busy.clone());
        std::thread::spawn(move || {
            let clock: Arc<dyn crate::clock::Clock> = Arc::new(SystemClock);
            let worker = make_worker(config_path, clock.clone());
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
                    busy.store(s.activity.is_busy(), Ordering::SeqCst);
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
    stop_updates.store(true, Ordering::SeqCst);
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

/// The next message for the tray's loop. On Linux the tray icon lives on its own thread, so this just waits. On Windows the icon's
/// window belongs to this thread, which must handle the window messages (the clicks, the menu) between looks at the channel.
#[cfg(target_os = "linux")]
fn next_msg(msg_rx: &Receiver<Msg>) -> Option<Msg> {
    msg_rx.recv().ok()
}

#[cfg(windows)]
fn next_msg(msg_rx: &Receiver<Msg>) -> Option<Msg> {
    use std::sync::mpsc::RecvTimeoutError;
    loop {
        crate::winsys::pump_messages();
        match msg_rx.recv_timeout(Duration::from_millis(40)) {
            Ok(m) => return Some(m),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
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
    let mut windows = Windows { exe, config: opts.config.clone(), open: None };
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
    let mut update = UpdateState::default();
    while let Some(m) = next_msg(msg_rx) {
        match m {
            Msg::Update(u) => {
                update = *u;
                if let Some(t) = tray.as_mut() {
                    t.set_update(update_menu_label(&update).as_deref());
                }
            }
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
                Some(MenuAction::Update) => {
                    if let Some(view) = update.view(&update::current_version()) {
                        if view.can_install {
                            if let Err(e) = update::request_install(state_dir, &view.version) {
                                append_log(state_dir, &format!("update: {e}"));
                            }
                        } else {
                            open_page(&view.page);
                        }
                    }
                }
                Some(MenuAction::Quit) => break,
                None => {}
            },
            Msg::EngineStopped => break,
        }
    }
    windows.close_all();
    Ok(())
}

/// The tray menu's entry about a newer version: None when there is none.
fn update_menu_label(state: &UpdateState) -> Option<String> {
    use crate::update::Phase;
    let v = state.view(&update::current_version())?;
    Some(match (&v.phase, v.can_install) {
        (Phase::Idle | Phase::Failed { .. }, true) => format!("Update to v{}", v.version),
        (Phase::Waiting, _) => format!("Updating to v{} when the sync ends", v.version),
        (_, true) => format!("Updating to v{}…", v.version),
        (_, false) => format!("Version {} is available: open the page", v.version),
    })
}

/// Open a web page in the user's browser (Linux; Windows updates itself and has no use for it).
fn open_page(url: &str) {
    #[cfg(not(windows))]
    if let Err(e) = std::process::Command::new("xdg-open").arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        log::warn!("cannot open {url}: {e}");
    }
    #[cfg(windows)]
    let _ = url;
}

#[cfg(not(any(target_os = "linux", windows)))]
fn main_loop_tray(
    _opts: &RunOptions,
    _config_path: &Path,
    _state_dir: &Path,
    _cmd_tx: &Sender<Command>,
    _msg_tx: &Sender<Msg>,
    _msg_rx: &Receiver<Msg>,
) -> Result<(), String> {
    Err("the tray icon is only built for Linux and Windows so far; use `run --headless`".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the window: logs that it started, then every line it is sent.
    #[cfg(unix)]
    fn fake_exe(dir: &Path, log: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let exe = dir.join("fake-window");
        std::fs::write(&exe, format!("#!/bin/sh\necho \"start $2\" >> '{0}'\nwhile read l; do echo \"got $l\" >> '{0}'; done\n", log.display())).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    /// The same as a small program (`examples/fake_window.rs`, built beside the tests); it takes the log as its `--config`.
    #[cfg(windows)]
    fn fake_exe(_dir: &Path, _log: &Path) -> PathBuf {
        let deps = std::env::current_exe().unwrap();
        let exe = deps.parent().unwrap().parent().unwrap().join("examples").join("fake_window.exe");
        assert!(exe.is_file(), "{} is not built", exe.display());
        exe
    }

    fn fake_config(log: &Path) -> Option<PathBuf> {
        cfg!(windows).then(|| log.to_path_buf())
    }

    fn wait_for(log: &Path, want: &str) -> String {
        for _ in 0..200 {
            let t = std::fs::read_to_string(log).unwrap_or_default().replace("\r\n", "\n");
            if t == want {
                return t;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        std::fs::read_to_string(log).unwrap_or_default().replace("\r\n", "\n")
    }

    /// A stand-in for the running app: holds the instance lock, takes a quit request (or not), and ends after `linger`.
    fn fake_app(dir: &Path, answers: bool, linger: Duration) -> std::thread::JoinHandle<()> {
        let lock = InstanceLock::acquire(dir).unwrap().expect("the lock is free");
        let file = quit_request_path(dir);
        std::thread::spawn(move || {
            let _lock = lock;
            let start = std::time::Instant::now();
            loop {
                if answers && file.exists() {
                    let _ = std::fs::remove_file(&file);
                    std::thread::sleep(linger);
                    return;
                }
                if start.elapsed() > Duration::from_secs(20) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    }

    #[test]
    fn quit_asks_the_running_app_and_waits_for_it_and_says_what_happened() {
        let _g = crate::instance::SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = std::env::temp_dir().join(format!("lc-quit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let (ack, exit) = (Duration::from_secs(2), Duration::from_secs(5));
        // no app: nothing happens, and no request is left behind for the next one
        std::fs::write(quit_request_path(&d), "old").unwrap();
        assert_eq!(request_quit(&d, ack, exit), Ok(QuitOutcome::NotRunning));
        assert!(!quit_request_path(&d).exists());
        // an app that takes the request and ends
        let app = fake_app(&d, true, Duration::from_millis(300));
        assert_eq!(request_quit(&d, ack, exit), Ok(QuitOutcome::Stopped));
        assert!(!InstanceLock::is_held(&d));
        app.join().unwrap();
        // an app in the middle of a long job: it took the request, and ends later than we wait
        let app = fake_app(&d, true, Duration::from_millis(1500));
        assert_eq!(request_quit(&d, ack, Duration::from_millis(300)), Ok(QuitOutcome::StillRunning));
        assert!(!quit_request_path(&d).exists());
        app.join().unwrap(); // (it ends)
        assert!(!InstanceLock::is_held(&d));
        // an older app that never reads the request: not answered, and the request is withdrawn
        let app = fake_app(&d, false, Duration::ZERO);
        assert_eq!(request_quit(&d, Duration::from_millis(400), exit), Ok(QuitOutcome::NotAnswered));
        assert!(!quit_request_path(&d).exists(), "an app started later must not find it");
        drop(app); // (the stand-in ends on its own after twenty seconds; the folder goes now)
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_second_click_switches_the_open_window_and_a_closed_one_is_started_again() {
        let dir = std::env::temp_dir().join(format!("lc-windows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let mut w = Windows { exe: fake_exe(&dir, &log), config: fake_config(&log), open: None };
        let spawn_lock = || crate::instance::SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let _g = spawn_lock();
            w.open("status");
        }
        w.open("settings");
        w.open("status");
        assert_eq!(wait_for(&log, "start status\ngot settings\ngot status\n"), "start status\ngot settings\ngot status\n");
        // the window goes away on its own: the next click starts a new one
        w.open.as_mut().unwrap().0.kill().unwrap();
        w.open.as_mut().unwrap().0.wait().unwrap();
        {
            let _g = spawn_lock();
            w.open("settings");
        }
        assert!(wait_for(&log, "start status\ngot settings\ngot status\nstart settings\n").ends_with("start settings\n"));
        w.close_all();
        assert!(w.open.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
