//! [`HomeWorker`]: the engine's jobs done for real: the sync crate's [`Home`] on the configured folder and remote, with the
//! engine as the renderer. The config is read again when a window saved it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lightcraft_cloud_client::home::{Home, InboxSummary, Renderer, State, utc_iso};
use lightcraft_cloud_client::{Error, Remote};

use crate::backup;
use crate::clock::Clock;
use crate::config::Config;
use crate::lightcraft::{LightCraftLink, TcpLink};
use crate::pending::{self, PendingList};
use crate::service::{BridgeReport, JobError, PurgeOutcome, SyncOutcome, Worker, WorkerInfo};

/// Makes the renderer for one sync (the real one opens the engine's scratch library; tests give a fake one).
pub type RendererFactory = Box<dyn Fn(&Config) -> Box<dyn Renderer>>;

/// Opens the remote of a config (the real one is [`Config::open_remote`]; tests can count or fake it).
pub type RemoteFactory = Box<dyn Fn(&Config) -> Result<Arc<dyn Remote>, String>>;

/// Makes the link to LightCraft's control port (the real one is loopback TCP; tests give a fake).
pub type LinkFactory = Box<dyn Fn(u16) -> Box<dyn LightCraftLink>>;

pub struct HomeWorker {
    /// Where the config is (it may not exist yet: the app then opens Setup).
    config_path: PathBuf,
    cfg: Option<Config>,
    /// Why the config could not be used, if it exists but is wrong.
    config_error: Option<String>,
    renderer: RendererFactory,
    remote: RemoteFactory,
    clock: Arc<dyn Clock>,
    link: LinkFactory,
}

impl HomeWorker {
    pub fn new(config_path: PathBuf, renderer: RendererFactory, remote: RemoteFactory, clock: Arc<dyn Clock>) -> HomeWorker {
        let link: LinkFactory = Box::new(|port| Box::new(TcpLink::new(port)));
        let mut w = HomeWorker { config_path, cfg: None, config_error: None, renderer, remote, clock, link };
        let _ = w.reload();
        w
    }

    /// Replace how LightCraft is reached (tests).
    pub fn set_link_factory(&mut self, f: LinkFactory) {
        self.link = f;
    }

    pub fn config(&self) -> Option<&Config> {
        self.cfg.as_ref()
    }

    /// A config that names a photo folder and a place to publish to.
    fn usable(cfg: &Config) -> bool {
        cfg.remote_spec().is_ok() && !cfg.file.source.trim().is_empty()
    }

    fn home(&self, cfg: &Config) -> Result<Home, JobError> {
        let remote = (self.remote)(cfg).map_err(JobError::Failed)?;
        let mut home = Home::new(cfg.home_config().map_err(JobError::Refused)?, remote).map_err(to_job_error)?;
        home.set_clock({
            let c = self.clock.clone();
            move || c.now()
        });
        pending::install_hook(&mut home, &cfg.state_dir, cfg.file.lightcraft_control_port);
        let logfile = cfg.state_dir.join("logs").join("lightcraft-cloud-desktop.log");
        home.set_logger(move |m| {
            let line = format!("{} {m}", utc_iso(std::time::SystemTime::now()));
            log::info!("{m}");
            if let Some(dir) = logfile.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&logfile) {
                let _ = writeln!(f, "{line}");
            }
        });
        Ok(home)
    }

    fn the_config(&self) -> Result<&Config, JobError> {
        match (&self.cfg, &self.config_error) {
            (Some(c), _) => Ok(c),
            (None, Some(e)) => Err(JobError::Refused(e.clone())),
            (None, None) => Err(JobError::Refused("the app is not set up yet".into())),
        }
    }
}

fn to_job_error(e: Error) -> JobError {
    match e {
        Error::Busy(_) => JobError::Busy,
        Error::Invalid(m) => JobError::Refused(m),
        e => JobError::Failed(e.to_string()),
    }
}

/// The lines of an inbox pass that settled a conflict by "newest wins" (the pass lists `"<action> <file>: <reason>"`).
pub fn newest_wins_notices(summary: &InboxSummary) -> Vec<String> {
    summary
        .items
        .iter()
        .filter_map(|item| {
            let (head, reason) = item.split_once(": ")?;
            let (action, file) = head.split_once(' ')?;
            match (action, reason) {
                ("apply", r) if r.starts_with("conflict: phone edit is newer") => {
                    Some(format!("Newest wins: the phone's edit replaced the PC's older sidecar ({file}; the PC's version is kept in history)"))
                }
                ("keep", r) if r.starts_with("conflict: PC sidecar is newer") => {
                    Some(format!("Newest wins: the PC's newer sidecar was kept ({file}; the phone's edit is kept in history)"))
                }
                _ => None,
            }
        })
        .collect()
}

impl Worker for HomeWorker {
    fn info(&self) -> WorkerInfo {
        match &self.cfg {
            Some(c) => WorkerInfo { configured: Self::usable(c), notify: c.file.notify, purge: !c.file.photo_backup.trim().is_empty() },
            None => WorkerInfo { configured: false, notify: true, purge: false },
        }
    }

    fn reload(&mut self) -> Result<(), String> {
        if !self.config_path.exists() {
            self.cfg = None;
            self.config_error = None;
            return Ok(());
        }
        match Config::load(Some(&self.config_path)) {
            Ok(c) => {
                self.cfg = Some(c);
                self.config_error = None;
                Ok(())
            }
            Err(e) => {
                // (keep working with the settings that worked)
                self.config_error = Some(e.clone());
                Err(e)
            }
        }
    }

    fn sync(&mut self) -> Result<SyncOutcome, JobError> {
        let cfg = self.the_config()?;
        if !Self::usable(cfg) {
            return Err(JobError::Refused("the app is not set up yet".into()));
        }
        let home = self.home(cfg)?;
        let mut renderer = (self.renderer)(cfg);
        let report = home.sync(&mut *renderer, false).map_err(to_job_error)?;
        let source_name = cfg.source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut import_devices: Vec<(String, usize)> = Vec::new();
        for p in &report.imports.added {
            match import_devices.iter_mut().find(|d| d.0 == p.device) {
                Some(d) => d.1 += 1,
                None => import_devices.push((p.device.clone(), 1)),
            }
        }
        Ok(SyncOutcome {
            imported: report.imports.added.len(),
            import_devices,
            imports_left: report.imports.waiting,
            import_problems: report.imports.errors.clone(),
            photos: report.photos,
            applied: report.inbox.applied,
            merged: report.inbox.merged,
            kept: report.inbox.kept,
            folders: report.inbox.folders.clone(),
            source_name,
            notices: newest_wins_notices(&report.inbox),
            failed: report.failed.len(),
        })
    }

    fn inbox_waiting(&mut self) -> Result<usize, String> {
        let cfg = self.the_config().map_err(|e| e.text())?;
        let home = self.home(cfg).map_err(|e| e.text())?;
        home.inbox_waiting().map_err(|e| e.to_string())
    }

    fn imports_waiting(&mut self) -> Result<usize, String> {
        let cfg = self.the_config().map_err(|e| e.text())?;
        let home = self.home(cfg).map_err(|e| e.text())?;
        home.imports_waiting().map_err(|e| e.to_string())
    }

    fn lightcraft_flush(&mut self, retry_given_up: bool) -> BridgeReport {
        let Some(cfg) = self.cfg.as_ref() else { return BridgeReport::default() };
        let port = cfg.file.lightcraft_control_port;
        if port == 0 {
            return BridgeReport::default();
        }
        let list = PendingList::new(&cfg.state_dir);
        let mut link = (self.link)(port);
        let r = pending::flush(&list, &mut *link, retry_given_up);
        BridgeReport { port, delivered: r.delivered, waiting: r.waiting, reachable: r.reachable, problems: r.problems, log: r.log }
    }

    fn purge(&mut self) -> Result<PurgeOutcome, JobError> {
        let cfg = self.the_config()?;
        let home = self.home(cfg)?;
        let age = backup::latest_age_hours(cfg.file.photo_backup.trim(), self.clock.now()).map_err(JobError::Refused)?;
        let r = home.purge_history(cfg.file.device_history_days, age).map_err(to_job_error)?;
        Ok(PurgeOutcome::Done { folders: r.folders, versions: r.versions })
    }

    fn last_run(&self) -> Option<(String, String, usize)> {
        let cfg = self.cfg.as_ref()?;
        let st = State::open(&cfg.state_dir, true).ok()?;
        let when = st.get("last_run")?.to_string();
        let result = st.get("last_result").unwrap_or("ok").to_string();
        let published = st.files().values().filter(|r| r.status == "ok").count();
        Some((when, result, published))
    }
}

/// The path the app writes its own log to.
pub fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("logs").join("lightcraft-cloud-desktop.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_newest_wins_conflicts_become_notices() {
        let s = InboxSummary {
            items: vec![
                "apply phone1/ab-20261101T000000Z-none.xmp: new sidecar".into(),
                "apply phone1/cd-20261101T000000Z-none.xmp: conflict: phone edit is newer; PC version kept in history".into(),
                "keep phone1/ef-20261101T000000Z-none.xmp: conflict: PC sidecar is newer; phone edit kept in history".into(),
                "merge phone1/gh-20261101T000000Z-none.xmp: conflict: PC sidecar changed since the phone's version".into(),
                "reject phone1/x.xmp: file name is not <device>/<id>-<UTCstamp>-<base>.xmp".into(),
            ],
            ..InboxSummary::default()
        };
        let n = newest_wins_notices(&s);
        assert_eq!(n.len(), 2);
        assert!(n[0].contains("the phone's edit replaced the PC's older sidecar") && n[0].contains("cd-20261101"));
        assert!(n[1].contains("the PC's newer sidecar was kept") && n[1].contains("ef-20261101"));
    }
}
