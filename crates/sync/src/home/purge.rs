//! Housekeeping the always-running app does about once a week (the Python tool's `purge-history`), and the cheap inbox poll
//! it does every couple of minutes (`inbox-check`).

use std::collections::HashSet;
use std::time::Duration;

use super::inbox::parse_inbox_name;
use super::{Home, State};
use crate::util::iso;
use crate::{Error, Result};

/// A purge is refused unless the photos were backed up this recently (hours): history is the only copy of what a sync or a
/// phone edit replaced.
pub const MAX_BACKUP_AGE_HOURS: f64 = 48.0;

/// What [`Home::purge_history`] removed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PurgeReport {
    /// Day folders under `history/` and `inbox-done/` on the remote.
    pub folders: usize,
    /// Stored sidecar versions on this PC.
    pub versions: usize,
    /// Folders dated before this day (`YYYY-mm-dd`) went.
    pub cutoff: String,
}

fn is_day(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 10 && b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
}

impl Home {
    /// How many phone edits wait in the remote's inbox: one listing, no lock, no state, no access to the photo folder. Only
    /// files with a valid inbox name count (stray files do not). An inbox that is not there is empty.
    pub fn inbox_waiting(&self) -> Result<usize> {
        Ok(self.inbox_pending()?.iter().filter(|p| parse_inbox_name(p).is_some()).count())
    }

    /// Delete the remote's `history/` and `inbox-done/` day folders older than `days`, and the stored sidecar versions
    /// older than that (except those of the current sidecars). `backup_age_hours`: how old the newest backup of the photos
    /// is; more than [`MAX_BACKUP_AGE_HOURS`] (or not a number) and nothing is deleted.
    pub fn purge_history(&self, days: u32, backup_age_hours: f64) -> Result<PurgeReport> {
        if backup_age_hours.is_nan() || backup_age_hours > MAX_BACKUP_AGE_HOURS {
            return Err(Error::Invalid(format!(
                "newest backup is {backup_age_hours:.0} h old (> {MAX_BACKUP_AGE_HOURS:.0} h); refusing to purge history"
            )));
        }
        let cutoff_time = self.now().checked_sub(Duration::from_secs(u64::from(days) * 86_400)).unwrap_or(std::time::UNIX_EPOCH);
        let cutoff = iso(cutoff_time)[..10].to_string();
        let mut report = PurgeReport { cutoff: cutoff.clone(), ..PurgeReport::default() };
        for top in ["history", "inbox-done"] {
            let entries = match self.remote.list(top, false) {
                Ok(v) => v,
                Err(Error::NotFound(_)) => continue,
                Err(e) => {
                    self.logln(format!("purge: cannot list {top}: {e}"));
                    continue;
                }
            };
            for e in entries.iter().filter(|e| e.is_dir && is_day(&e.path) && e.path.as_str() < cutoff.as_str()) {
                self.logln(format!("purge {top}/{}", e.path));
                self.retry(|| self.remote.purge_dir(&format!("{top}/{}", e.path)))?;
                report.folders += 1;
            }
        }
        self.logln(format!("purged {} folders older than {days} days (cutoff {cutoff}); newest backup {backup_age_hours:.1} h old", report.folders));
        let st = State::open(&self.cfg.state_dir, true)?;
        let keep: HashSet<String> = st.files().values().filter_map(|r| r.sidecar_hash.clone()).collect();
        report.versions = self.versions().purge(cutoff_time, &keep);
        self.logln(format!("purged {} stored sidecar versions older than {days} days (versions of current sidecars are kept)", report.versions));
        Ok(report)
    }
}
