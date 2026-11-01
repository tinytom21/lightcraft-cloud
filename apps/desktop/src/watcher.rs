//! The folder watcher: tells the engine when something in the photo folder changed. Which changes count (the Python tool's
//! `lc-cloud-watch`): not hidden files, not anything under an excluded folder, not mere reads. The 30-second debounce is
//! the scheduler's.

use std::path::Path;

use lightcraft_cloud_client::home::is_excluded;
use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecursiveMode, Watcher};

/// Whether a change at `path` should lead to a sync.
pub fn relevant(source: &Path, exclude: &[String], path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(source) else { return false };
    if rel.as_os_str().is_empty() {
        return false;
    }
    if rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
        return false; // temp and hidden files and folders
    }
    // (the patterns are matched against `/`-separated paths, as the sync crate's are)
    let rel = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
    !is_excluded(&rel, exclude)
}

/// Whether this kind of event is a change (not a read).
pub fn counts(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_) => true,
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) | EventKind::Any | EventKind::Other => false,
    }
}

/// Watching the photo folder, recursively. Dropping it stops the watch.
pub struct FolderWatcher {
    _watcher: notify::RecommendedWatcher,
}

impl FolderWatcher {
    /// `on_change` is called (from the watcher's thread) for every relevant change; keep it quick.
    pub fn start(source: &Path, exclude: Vec<String>, on_change: impl Fn() + Send + 'static) -> Result<FolderWatcher, String> {
        let root = source.to_path_buf();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res
                && counts(&ev.kind)
                && ev.paths.iter().any(|p| relevant(&root, &exclude, p))
            {
                on_change();
            }
        })
        .map_err(|e| format!("cannot watch {}: {e}", source.display()))?;
        watcher.watch(source, RecursiveMode::Recursive).map_err(|e| format!("cannot watch {}: {e}", source.display()))?;
        Ok(FolderWatcher { _watcher: watcher })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn ex(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn hidden_excluded_and_outside_paths_do_not_count() {
        let src = Path::new("/p/Photos");
        let e = ex(&["exported", "Thumb*"]);
        assert!(relevant(src, &e, Path::new("/p/Photos/2024/a.jpg")));
        assert!(relevant(src, &e, Path::new("/p/Photos/2024")));
        assert!(!relevant(src, &e, Path::new("/p/Photos/2024/.a.jpg.tmp")));
        assert!(!relevant(src, &e, Path::new("/p/Photos/.hidden/a.jpg")));
        assert!(!relevant(src, &e, Path::new("/p/Photos/exported/a.jpg")));
        assert!(!relevant(src, &e, Path::new("/p/Photos/x/Thumbnails/a.jpg")));
        assert!(relevant(src, &e, Path::new("/p/Photos/exported"))); // (the folder itself appearing: one useless sync, as before)
        assert!(!relevant(src, &e, Path::new("/elsewhere/a.jpg")));
        assert!(!relevant(src, &e, Path::new("/p/Photos")));
    }

    #[test]
    fn reads_are_not_changes_but_writes_closing_are() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind};
        assert!(counts(&EventKind::Create(CreateKind::File)));
        assert!(counts(&EventKind::Remove(RemoveKind::File)));
        assert!(counts(&EventKind::Modify(ModifyKind::Any)));
        assert!(counts(&EventKind::Access(AccessKind::Close(AccessMode::Write))));
        assert!(!counts(&EventKind::Access(AccessKind::Open(AccessMode::Any))));
        assert!(!counts(&EventKind::Access(AccessKind::Close(AccessMode::Read))));
        assert!(!counts(&EventKind::Any));
    }

    #[test]
    fn a_real_watch_reports_a_new_photo() {
        let d = std::env::temp_dir().join(format!("lc-desktop-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        let (tx, rx) = mpsc::channel();
        let _w = FolderWatcher::start(&d, ex(&["skip"]), move || {
            let _ = tx.send(());
        })
        .unwrap();
        std::fs::write(d.join("sub/a.jpg"), b"x").unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok(), "no event for a new photo");
        let _ = std::fs::remove_dir_all(&d);
    }
}
