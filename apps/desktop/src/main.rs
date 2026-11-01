//! `lightcraft-cloud-desktop`: LightCraft Cloud for the PC. With no command (or `run`) it is the always-running app: a tray
//! icon, a sync every 15 minutes, a check for phone edits every 2, a folder watcher, a weekly purge. The command line:
//!
//! ```text
//! lightcraft-cloud-desktop [run [--headless]] [--config FILE]
//! lightcraft-cloud-desktop sync --once [--dry-run] [--config FILE]
//! lightcraft-cloud-desktop status [--config FILE]
//! lightcraft-cloud-desktop inbox-check [--config FILE]
//! lightcraft-cloud-desktop purge-history [--days N] [--config FILE]
//! ```
//!
//! Publishes the photo folder to the cloud library (an rclone remote, or a local folder) and applies the phone's edits to
//! the sidecars, like the Python tool `lc-cloud`, with the same remote layout (FORMAT.md). It keeps its own config and
//! state (`~/.config/lightcraft-cloud-desktop/`, `~/.local/state/lightcraft-cloud-desktop/`).

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::SystemTime;

use lightcraft_cloud_client::Error;
#[cfg(feature = "home")]
use lightcraft_cloud_client::home::Home;
use lightcraft_cloud_client::home::utc_iso;
use lightcraft_cloud_desktop::config::{Config, Role};
use lightcraft_cloud_desktop::{app, ui};

const USAGE: &str = "\
lightcraft-cloud-desktop: keep the cloud library in step with a photo folder

USAGE:
  lightcraft-cloud-desktop [run] [--headless] [--config FILE]
      The always-running app: tray icon (Sync now, Pause/Resume, Status, Settings, Quit), a sync every 15 minutes, a check
      for phone edits every 2, a sync shortly after the photo folder changes, a weekly history purge. With no config it
      opens Setup. --headless: no tray, no windows, no desktop notifications. A second copy just exits.
  lightcraft-cloud-desktop sync --once [--dry-run] [--config FILE]
      Apply the phone's edits to the sidecars, then publish the photo folder: originals, thumbnails,
      smart previews, manifest. --dry-run changes nothing and says what would happen.
  lightcraft-cloud-desktop status [--config FILE]
      Last run, tracked photos, remote size, pending phone edits.
  lightcraft-cloud-desktop inbox-check [--config FILE]
      How many phone edits wait in the cloud inbox (one listing; changes nothing).
  lightcraft-cloud-desktop purge-history [--days N] [--config FILE]
      Delete cloud history/inbox-done day folders older than N days (default device_history_days = 30), only if the
      newest backup (photo_backup, restic) is at most 48 hours old.
  lightcraft-cloud-desktop --version | --help

On another PC (config: role = other; the light build is always one) `sync --once` makes one pass: it downloads the chosen
folders, sends this PC's sidecar edits to the home PC and takes back its newer ones; `status` shows the counts.

Config: --config FILE, else ~/.config/lightcraft-cloud-desktop/config.toml (TOML; see the README).
State:  ~/.local/state/lightcraft-cloud-desktop/ (or `state_dir` in the config).
";

struct Args {
    cmd: String,
    /// A second word (the `ui` command's window).
    sub: Option<String>,
    once: bool,
    dry: bool,
    headless: bool,
    days: Option<u32>,
    config: Option<PathBuf>,
}

fn parse(args: &[String]) -> std::result::Result<Args, String> {
    let mut a = Args { cmd: String::new(), sub: None, once: false, dry: false, headless: false, days: None, config: None };
    let mut it = args.iter();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--once" => a.once = true,
            "--dry-run" => a.dry = true,
            "--headless" => a.headless = true,
            "--days" => a.days = Some(it.next().and_then(|d| d.parse().ok()).ok_or("--days needs a number")?),
            "--config" => a.config = Some(PathBuf::from(it.next().ok_or("--config needs a file")?)),
            s if s.starts_with("--config=") => a.config = Some(PathBuf::from(&s["--config=".len()..])),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s if a.cmd.is_empty() => a.cmd = s.to_string(),
            s if a.cmd == "ui" && a.sub.is_none() => a.sub = Some(s.to_string()),
            s => return Err(format!("unexpected argument {s}")),
        }
    }
    Ok(a)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("lightcraft-cloud-desktop {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let a = match parse(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("lightcraft-cloud-desktop: {e}\n\n{USAGE}");
            return ExitCode::from(64);
        }
    };
    match run(a) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Refused(m)) => {
            println!("{} REFUSED: {m}", utc_iso(SystemTime::now()));
            ExitCode::from(2)
        }
        Err(Failure::Error(m)) => {
            println!("{} ERROR: {m}", utc_iso(SystemTime::now()));
            ExitCode::from(1)
        }
    }
}

enum Failure {
    /// A safety check said no; nothing was changed.
    Refused(String),
    Error(String),
}

impl From<String> for Failure {
    fn from(m: String) -> Self {
        Failure::Error(m)
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        match e {
            Error::Invalid(m) | Error::Busy(m) => Failure::Refused(m),
            e => Failure::Error(e.to_string()),
        }
    }
}

#[cfg(feature = "home")]
fn build_home(cfg: &Config, log_to_file: bool) -> std::result::Result<Home, Failure> {
    let mut home = Home::new(cfg.home_config()?, cfg.open_remote()?)?;
    // (photos adopted by the command line are queued too: the running app tells LightCraft about them)
    lightcraft_cloud_desktop::pending::install_hook(&mut home, &cfg.state_dir, cfg.file.lightcraft_control_port);
    let logfile = cfg.state_dir.join("logs").join("lightcraft-cloud-desktop.log");
    home.set_logger(move |m| {
        let line = format!("{} {m}", utc_iso(SystemTime::now()));
        println!("{line}");
        if log_to_file && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&logfile) {
            let _ = writeln!(f, "{line}");
        }
    });
    Ok(home)
}

fn run(a: Args) -> std::result::Result<(), Failure> {
    match a.cmd.as_str() {
        "" | "run" => {
            let opts = app::RunOptions { config: a.config, headless: a.headless };
            return match app::run(opts)? {
                app::Ended::Quit => Ok(()),
                app::Ended::AlreadyRunning => {
                    println!("LightCraft Cloud is already running (look for its tray icon).");
                    Ok(())
                }
            };
        }
        "ui" => {
            let tab = a.sub.as_deref().and_then(ui::Tab::parse).ok_or_else(|| Failure::Error("usage: ui status|settings".into()))?;
            return ui::run_window(tab, a.config).map_err(Failure::Error);
        }
        _ => {}
    }
    let cfg = Config::load(a.config.as_deref())?;
    if cfg.file.role == Role::Other {
        return run_other(a, cfg);
    }
    run_command(a, cfg)
}

/// The commands of another PC (both builds): a pass of the other-PC role, and its status.
fn run_other(a: Args, cfg: Config) -> std::result::Result<(), Failure> {
    use lightcraft_cloud_client::other::Other;
    use lightcraft_cloud_desktop::pending::PendingList;
    let mut other = Other::new(cfg.other_config()?, cfg.open_remote()?)?;
    let logfile = lightcraft_cloud_desktop::worker::log_path(&cfg.state_dir);
    let log_to_file = !a.dry;
    other.set_logger(move |m| {
        let line = format!("{} {m}", utc_iso(SystemTime::now()));
        println!("{line}");
        if log_to_file {
            if let Some(dir) = logfile.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&logfile) {
                let _ = writeln!(f, "{line}");
            }
        }
    });
    if cfg.file.lightcraft_control_port != 0 {
        let list = PendingList::new(&cfg.state_dir);
        other.set_adopted_hook(move |p| list.add(p, &utc_iso(SystemTime::now())));
    }
    match a.cmd.as_str() {
        "sync" => {
            if !a.once {
                return Err(Failure::Error("only `sync --once` exists so far (there is no background mode yet)".into()));
            }
            if a.dry {
                return Err(Failure::Refused("--dry-run is not available on another PC".into()));
            }
            let r = other.sync()?;
            println!(
                "{} photos downloaded, {} sidecars updated from the home PC, {} edits sent ({} waiting for the home PC to merge), {} moved aside, {} left out",
                r.downloaded.len(),
                r.sidecars_updated.len(),
                r.edits_sent,
                r.held_back,
                r.removed.len(),
                r.left_out
            );
            println!("on this PC: {} photos; waiting to download: {}; edits waiting to send: {}", r.photos, r.waiting_to_download, r.edits_waiting);
            for p in &r.problems {
                println!("PROBLEM: {p}");
            }
            Ok(())
        }
        "status" => {
            let s = other.status()?;
            println!("role:       another PC ({}), folders: {}", cfg.device(), cfg.file.folders.join(", "));
            println!("last pass:  {}", s.last_pass.as_deref().unwrap_or("never"));
            println!("on this PC: {} photos", s.photos);
            println!("waiting to download: {}", s.waiting_to_download);
            println!("edits waiting to send: {}", s.edits_waiting);
            println!("imported photos: {} waiting to upload, {} sent and waiting for the home PC", s.queue.to_upload, s.queue.awaiting_home);
            Ok(())
        }
        "inbox-check" | "purge-history" => Err(Failure::Refused(format!("{} is for the home PC; this is another PC", a.cmd))),
        other => Err(Failure::Error(format!("unknown command {other}\n\n{USAGE}"))),
    }
}

/// The light build has no engine and publishes nothing; the commands below are the home PC's.
#[cfg(not(feature = "home"))]
fn run_command(a: Args, _cfg: Config) -> std::result::Result<(), Failure> {
    match a.cmd.as_str() {
        "sync" | "status" | "inbox-check" | "purge-history" => Err(Failure::Refused(lightcraft_cloud_desktop::worker::LIGHT_BUILD_REFUSES_HOME.into())),
        other => Err(Failure::Error(format!("unknown command {other}\n\n{USAGE}"))),
    }
}

#[cfg(feature = "home")]
fn run_command(a: Args, cfg: Config) -> std::result::Result<(), Failure> {
    use lightcraft_cloud_client::home::notify_message;
    use lightcraft_cloud_desktop::{backup, engine, notifier};
    match a.cmd.as_str() {
        "sync" => {
            if !a.once {
                return Err(Failure::Error("only `sync --once` exists so far (there is no background mode yet)".into()));
            }
            let home = build_home(&cfg, !a.dry)?;
            let mut renderer = engine::LazyRenderer::new(cfg.state_dir.join("lib"));
            let report = home.sync(&mut renderer, a.dry)?;
            let n = report.inbox.applied + report.inbox.merged;
            if n > 0 && !a.dry {
                let name = cfg.source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                println!("{}", notify_message(n, &report.inbox.folders, &name, report.inbox.merged));
            }
            if !report.imports.added.is_empty() && !a.dry {
                let mut devices: Vec<(String, usize)> = Vec::new();
                for p in &report.imports.added {
                    match devices.iter_mut().find(|d| d.0 == p.device) {
                        Some(d) => d.1 += 1,
                        None => devices.push((p.device.clone(), 1)),
                    }
                }
                println!("{}", notifier::photos_added(&devices));
            }
            Ok(())
        }
        "status" => {
            let home = build_home(&cfg, false)?;
            let s = home.status()?;
            println!("last run:   {} ({})", s.last_run.as_deref().unwrap_or("never"), s.last_result.as_deref().unwrap_or("-"));
            println!("last plan:  {}", s.last_counts.as_deref().unwrap_or("-"));
            println!("tracked:    {} photos ({} not published)", s.tracked, s.not_published);
            match (&s.remote, &s.remote_error) {
                (Some((n, bytes)), _) => println!("remote:     {n} files, {} at {}", human(*bytes), cfg.remote_spec()?),
                (None, e) => println!("remote:     error: {}", e.as_deref().unwrap_or("not reachable")),
            }
            match s.inbox_pending {
                Some(n) => println!("inbox:      {n} pending"),
                None => println!("inbox:      (could not be listed)"),
            }
            match s.imports_pending {
                Some(n) => println!("imports:    {n} pending"),
                None => println!("imports:    (could not be listed)"),
            }
            Ok(())
        }
        "inbox-check" => {
            let home = build_home(&cfg, false)?;
            match home.inbox_waiting() {
                Ok(n) => println!("inbox: {n} pending"),
                // (a network error is reported and swallowed, like the Python tool's inbox-check)
                Err(e) => println!("inbox: check failed: {e}"),
            }
            Ok(())
        }
        "purge-history" => {
            let home = build_home(&cfg, !a.dry)?;
            let age = backup::latest_age_hours(cfg.file.photo_backup.trim(), SystemTime::now()).map_err(Failure::Refused)?;
            home.purge_history(a.days.unwrap_or(cfg.file.device_history_days), age)?;
            Ok(())
        }
        other => Err(Failure::Error(format!("unknown command {other}\n\n{USAGE}"))),
    }
}

#[cfg(feature = "home")]
fn human(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let mut f = n as f64;
    let mut unit = "B";
    for u in ["KiB", "MiB", "GiB", "TiB"] {
        f /= 1024.0;
        unit = u;
        if f < 1024.0 {
            break;
        }
    }
    format!("{f:.1} {unit}")
}
