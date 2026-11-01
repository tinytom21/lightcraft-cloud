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

/// Whether a path is a sidecar (`.xmp`, any case).
pub fn is_sidecar(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
}

/// Whether a path is a file another PC's pass looks at: a sidecar LightCraft wrote (an edit to send), or a photo (one LightCraft just
/// imported into the folder, to send).
pub fn is_watched_on_another_pc(path: &Path) -> bool {
    is_sidecar(path) || path.file_name().is_some_and(|n| lightcraft_cloud_client::other::is_pickup_name(&n.to_string_lossy()))
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
        Self::start_filtered(source, exclude, false, on_change)
    }

    /// [`FolderWatcher::start`]; with `sidecars_only` only a change to a sidecar or a photo file counts (another PC's role: a new photo
    /// is news, to be sent; anything else is not).
    pub fn start_filtered(source: &Path, exclude: Vec<String>, sidecars_only: bool, on_change: impl Fn() + Send + 'static) -> Result<FolderWatcher, String> {
        let root = source.to_path_buf();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res
                && counts(&ev.kind)
                && ev.paths.iter().any(|p| (!sidecars_only || is_watched_on_another_pc(p)) && relevant(&root, &exclude, p))
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
    fn only_xmp_files_are_sidecars() {
        assert!(is_sidecar(Path::new("/p/a.xmp")) && is_sidecar(Path::new("/p/a.jpg.XMP")));
        assert!(!is_sidecar(Path::new("/p/a.jpg")) && !is_sidecar(Path::new("/p/xmp")) && !is_sidecar(Path::new("/p/a.xmp.tmp")));
    }

    #[test]
    fn another_pc_wakes_for_sidecars_and_photos_only() {
        assert!(is_watched_on_another_pc(Path::new("/p/2026/Day 1/a.jpg")) && is_watched_on_another_pc(Path::new("/p/a.ARW")) && is_watched_on_another_pc(Path::new("/p/a.xmp")));
        assert!(!is_watched_on_another_pc(Path::new("/p/a.txt")) && !is_watched_on_another_pc(Path::new("/p/a.jpg.part")) && !is_watched_on_another_pc(Path::new("/p/.a.jpg")));
    }

    #[test]
    fn a_watch_for_another_pc_hears_photos_and_sidecars_not_other_files() {
        let d = std::env::temp_dir().join(format!("lc-desktop-watch-sc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let (tx, rx) = mpsc::channel();
        let _w = FolderWatcher::start_filtered(&d, vec![], true, move || {
            let _ = tx.send(());
        })
        .unwrap();
        std::fs::write(d.join("a.txt"), b"x").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(700)).is_err(), "a text file is not news");
        std::fs::create_dir_all(d.join("2026/Day 1")).unwrap();
        std::thread::sleep(Duration::from_millis(500)); // (the watch reaches the new folders)
        while rx.try_recv().is_ok() {}
        std::fs::write(d.join("2026/Day 1/a.jpg"), b"x").unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok(), "no event for a photo in a nested folder");
        while rx.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(300));
        while rx.try_recv().is_ok() {}
        std::fs::write(d.join("a.xmp"), b"x").unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok(), "no event for a sidecar");
        let _ = std::fs::remove_dir_all(&d);
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
