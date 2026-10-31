//! `lightcraft-cloud-desktop`: LightCraft Cloud for the PC. Command line for now (a tray app is to come):
//!
//! ```text
//! lightcraft-cloud-desktop sync --once [--dry-run] [--config FILE]
//! lightcraft-cloud-desktop status [--config FILE]
//! ```
//!
//! Publishes the photo folder to the cloud library (an rclone remote, or a local folder) and applies the phone's edits to
//! the sidecars, like the Python tool `lc-cloud`, with the same remote layout (FORMAT.md). It keeps its own config and
//! state (`~/.config/lightcraft-cloud-desktop/`, `~/.local/state/lightcraft-cloud-desktop/`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;

use lightcraft_cloud_client::home::{Added, Home, PhotoInfo, Renderer, notify_message, utc_iso};
use lightcraft_cloud_client::{Error, Result};
use lightcraft_cloud_desktop::config::Config;
use lightcraft_cloud_desktop::engine;

const USAGE: &str = "\
lightcraft-cloud-desktop: keep the cloud library in step with a photo folder

USAGE:
  lightcraft-cloud-desktop sync --once [--dry-run] [--config FILE]
      Apply the phone's edits to the sidecars, then publish the photo folder: originals, thumbnails,
      smart previews, manifest. --dry-run changes nothing and says what would happen.
  lightcraft-cloud-desktop status [--config FILE]
      Last run, tracked photos, remote size, pending phone edits.
  lightcraft-cloud-desktop --version | --help

Config: --config FILE, else ~/.config/lightcraft-cloud-desktop/config.toml (TOML; see the README).
State:  ~/.local/state/lightcraft-cloud-desktop/ (or `state_dir` in the config).
";

struct Args {
    cmd: String,
    once: bool,
    dry: bool,
    config: Option<PathBuf>,
}

fn parse(args: &[String]) -> std::result::Result<Args, String> {
    let mut a = Args { cmd: String::new(), once: false, dry: false, config: None };
    let mut it = args.iter();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--once" => a.once = true,
            "--dry-run" => a.dry = true,
            "--config" => a.config = Some(PathBuf::from(it.next().ok_or("--config needs a file")?)),
            s if s.starts_with("--config=") => a.config = Some(PathBuf::from(&s["--config=".len()..])),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s if a.cmd.is_empty() => a.cmd = s.to_string(),
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
        Some("--help" | "-h" | "help") | None => {
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
            Error::Invalid(m) => Failure::Refused(m),
            e => Failure::Error(e.to_string()),
        }
    }
}

fn build_home(cfg: &Config, log_to_file: bool) -> std::result::Result<Home, Failure> {
    let mut home = Home::new(cfg.home_config()?, cfg.open_remote()?)?;
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
    let cfg = Config::load(a.config.as_deref())?;
    match a.cmd.as_str() {
        "sync" => {
            if !a.once {
                return Err(Failure::Error("only `sync --once` exists so far (there is no background mode yet)".into()));
            }
            let home = build_home(&cfg, !a.dry)?;
            let mut renderer = LazyRenderer { lib: cfg.state_dir.join("lib"), inner: None };
            let report = home.sync(&mut renderer, a.dry)?;
            let n = report.inbox.applied + report.inbox.merged;
            if n > 0 && !a.dry {
                let name = cfg.source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                println!("{}", notify_message(n, &report.inbox.folders, &name, report.inbox.merged));
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
            Ok(())
        }
        "" => Err(Failure::Error(format!("no command given\n\n{USAGE}"))),
        other => Err(Failure::Error(format!("unknown command {other}\n\n{USAGE}"))),
    }
}

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

/// Opens the engine's scratch library on first use: a dry run, or a run with nothing to render, never opens it.
struct LazyRenderer {
    lib: PathBuf,
    inner: Option<engine::EngineRenderer>,
}

impl LazyRenderer {
    fn get(&mut self) -> Result<&mut engine::EngineRenderer> {
        if self.inner.is_none() {
            self.inner = Some(engine::EngineRenderer::open(&self.lib)?);
        }
        self.inner.as_mut().ok_or_else(|| Error::Engine("the scratch library did not open".into()))
    }
}

impl Renderer for LazyRenderer {
    fn add(&mut self, photo: &Path) -> Result<Added> {
        self.get()?.add(photo)
    }
    fn reread(&mut self, photo: &Path) -> Result<PhotoInfo> {
        self.get()?.reread(photo)
    }
    fn thumbnail(&mut self, photo: &Path, long_edge: u32, quality: u8, out: &Path) -> Result<()> {
        self.get()?.thumbnail(photo, long_edge, quality, out)
    }
    fn smart_preview(&mut self, photo: &Path, out: &Path) -> Result<String> {
        self.get()?.smart_preview(photo, out)
    }
    fn forget(&mut self, photos: &[PathBuf]) -> Result<()> {
        if self.inner.is_none() && !self.lib.is_dir() {
            return Ok(()); // no scratch library yet, so nothing to drop
        }
        self.get()?.forget(photos)
    }
    fn finish(&mut self) -> Result<()> {
        match self.inner.as_mut() {
            Some(r) => r.finish(),
            None => Ok(()),
        }
    }
}
