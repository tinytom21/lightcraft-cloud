//! The background engine's workers: what the [`Service`](crate::service::Service) calls to do a job. [`HomeWorker`] is the home PC's
//! (publishing, phone edits, imports: the full build only, it needs LightCraft's engine for rendering); the light build, which has
//! no engine, has [`LightWorker`], which says so.

use std::path::{Path, PathBuf};

#[cfg(feature = "home")]
use lightcraft_cloud_client::Error;

#[cfg(feature = "home")]
use crate::service::JobError;

#[cfg(feature = "home")]
mod home;
#[cfg(feature = "home")]
pub use home::{HomeWorker, LinkFactory, RemoteFactory, RendererFactory, newest_wins_notices};

#[cfg(not(feature = "home"))]
mod light;
#[cfg(not(feature = "home"))]
pub use light::{LIGHT_BUILD_REFUSES_HOME, LightWorker};

/// The path the app writes its own log to.
pub fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("logs").join("lightcraft-cloud-desktop.log")
}

#[cfg(feature = "home")]
pub(crate) fn to_job_error(e: Error) -> JobError {
    match e {
        Error::Busy(_) => JobError::Busy,
        Error::Invalid(m) => JobError::Refused(m),
        e => JobError::Failed(e.to_string()),
    }
}
