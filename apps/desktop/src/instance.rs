//! One app per user: an exclusive lock on `<state>/instance.lock`, held for as long as the app runs. A second launch finds
//! it taken and exits. (The sync's own run lock, `<state>/lock`, is a different file: it also keeps the command line's
//! `sync --once` and the app from syncing at the same time.)

use std::path::Path;

pub struct InstanceLock {
    _file: std::fs::File,
}

fn open(state_dir: &Path) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(state_dir).map_err(|e| format!("{}: {e}", state_dir.display()))?;
    let path = state_dir.join("instance.lock");
    std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))
}

impl InstanceLock {
    /// `Ok(None)`: another instance is running.
    pub fn acquire(state_dir: &Path) -> Result<Option<InstanceLock>, String> {
        let file = open(state_dir)?;
        Ok(file.try_lock().ok().map(|()| InstanceLock { _file: file }))
    }

    /// Whether an app is running on this state folder (for the windows, which show a hint when it is not). Creates nothing.
    pub fn is_held(state_dir: &Path) -> bool {
        match std::fs::File::open(state_dir.join("instance.lock")) {
            Ok(f) => f.try_lock().is_err(),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_instance_is_turned_away_until_the_first_ends() {
        let d = std::env::temp_dir().join(format!("lc-desktop-instance-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(!InstanceLock::is_held(&d)); // (nothing created by asking)
        assert!(!d.exists());
        let first = InstanceLock::acquire(&d).unwrap().expect("the first instance gets the lock");
        assert!(InstanceLock::is_held(&d));
        assert!(InstanceLock::acquire(&d).unwrap().is_none());
        drop(first);
        assert!(!InstanceLock::is_held(&d));
        assert!(InstanceLock::acquire(&d).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&d);
    }
}
