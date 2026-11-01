//! The light build's worker: this build has no LightCraft engine, so it cannot publish a photo folder. Until it learns the other-PC
//! role it refuses every job, in words the user can act on.

use std::path::PathBuf;

use crate::config::Config;
use crate::service::{JobError, PurgeOutcome, SyncOutcome, Worker, WorkerInfo};

/// What the light build says when it is given a home PC's config.
pub const LIGHT_BUILD_REFUSES_HOME: &str = "This is the light build of LightCraft Cloud, which cannot publish photos. Use the full build on the home PC.";

pub struct LightWorker {
    config_path: PathBuf,
    cfg: Option<Config>,
    config_error: Option<String>,
}

impl LightWorker {
    pub fn new(config_path: PathBuf) -> LightWorker {
        let mut w = LightWorker { config_path, cfg: None, config_error: None };
        let _ = w.reload();
        w
    }
}

impl Worker for LightWorker {
    fn info(&self) -> WorkerInfo {
        WorkerInfo { configured: self.cfg.is_some(), notify: self.cfg.as_ref().is_none_or(|c| c.file.notify), purge: false }
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
                self.config_error = Some(e.clone());
                Err(e)
            }
        }
    }

    fn sync(&mut self) -> Result<SyncOutcome, JobError> {
        Err(JobError::Refused(LIGHT_BUILD_REFUSES_HOME.into()))
    }

    fn inbox_waiting(&mut self) -> Result<usize, String> {
        Ok(0)
    }

    fn purge(&mut self) -> Result<PurgeOutcome, JobError> {
        Err(JobError::Refused(LIGHT_BUILD_REFUSES_HOME.into()))
    }

    fn last_run(&self) -> Option<(String, String, usize)> {
        None
    }
}
