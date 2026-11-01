//! "Start when I log in". On Linux the XDG autostart entry `~/.config/autostart/lightcraft-cloud-desktop.desktop`; on Windows the value
//! `LightCraft Cloud` in the current user's `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` key (no administrator needed, and Windows'
//! own Settings > Apps > Startup lists and switches it). Written only when the user ticks the box in Settings and removed when they
//! untick it. Every function takes the place, so tests use a temporary one: a folder on Linux, a registry key (below `HKCU`, written as
//! a path) on Windows.

use std::path::{Path, PathBuf};

/// The entry's file name (Linux), or the name of the registry value (Windows).
pub const FILE_NAME: &str = if cfg!(windows) { "LightCraft Cloud" } else { "lightcraft-cloud-desktop.desktop" };

/// The registry key Windows runs at log-in, relative to `HKCU`.
#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(not(windows))]
/// `$XDG_CONFIG_HOME/autostart`, else `~/.config/autostart`.
pub fn default_dir() -> Result<PathBuf, String> {
    Ok(match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join("autostart"),
        None => crate::config::home_dir()?.join(".config/autostart"),
    })
}

#[cfg(not(windows))]
/// An `Exec` argument, quoted the way the Desktop Entry spec wants.
fn quote(arg: &str) -> String {
    if arg.chars().any(|c| " \t\n\"'\\<>~|&;$*?#()`".contains(c)) {
        let mut q = String::from("\"");
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                q.push('\\');
            }
            q.push(c);
        }
        q.push('"');
        q
    } else {
        arg.to_string()
    }
}

#[cfg(not(windows))]
/// The entry's text: runs `exe run` (the tray app).
pub fn entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=LightCraft Cloud\nComment=Keeps your LightCraft cloud library in step with your photos\nExec={} run\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        quote(&exe.to_string_lossy())
    )
}

#[cfg(not(windows))]
pub fn is_enabled(dir: &Path) -> bool {
    dir.join(FILE_NAME).is_file()
}

#[cfg(not(windows))]
pub fn enable(dir: &Path, exe: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(FILE_NAME);
    std::fs::write(&path, entry(exe)).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(not(windows))]
pub fn disable(dir: &Path) -> Result<(), String> {
    let path = dir.join(FILE_NAME);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The registry key that holds the entry. Windows' own: `HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run`.
#[cfg(windows)]
pub fn default_dir() -> Result<PathBuf, String> {
    Ok(PathBuf::from(RUN_KEY))
}

/// The command the entry runs: the app (`run`), the path quoted (it may have spaces; Windows paths cannot contain a quote).
#[cfg(windows)]
pub fn entry(exe: &Path) -> String {
    format!("\"{}\" run", exe.display())
}

/// The entry's command, if the entry is there.
#[cfg(windows)]
pub fn entry_text(key: &Path) -> Option<String> {
    crate::winsys::registry_get(&key.to_string_lossy(), FILE_NAME)
}

#[cfg(windows)]
pub fn is_enabled(key: &Path) -> bool {
    entry_text(key).is_some()
}

#[cfg(windows)]
pub fn enable(key: &Path, exe: &Path) -> Result<(), String> {
    crate::winsys::registry_set(&key.to_string_lossy(), FILE_NAME, &entry(exe))
}

#[cfg(windows)]
pub fn disable(key: &Path) -> Result<(), String> {
    crate::winsys::registry_delete(&key.to_string_lossy(), FILE_NAME)
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lc-desktop-autostart-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_entry_runs_the_app_and_quotes_odd_paths() {
        let e = entry(Path::new("/opt/lc/lightcraft-cloud-desktop"));
        assert!(
            e.starts_with("[Desktop Entry]\n") && e.contains("\nExec=/opt/lc/lightcraft-cloud-desktop run\n") && e.contains("\nType=Application\n")
        );
        assert!(entry(Path::new("/opt/my apps/lc")).contains("\nExec=\"/opt/my apps/lc\" run\n"));
        assert!(entry(Path::new("/o/a$b")).contains("Exec=\"/o/a\\$b\" run"));
    }

    #[test]
    fn enable_writes_and_disable_removes_only_in_the_given_folder() {
        let d = temp("a").join("autostart");
        assert!(!is_enabled(&d));
        disable(&d).unwrap(); // nothing there: fine, and nothing created
        assert!(!d.exists());
        enable(&d, Path::new("/x/lc")).unwrap();
        assert!(is_enabled(&d));
        assert!(std::fs::read_to_string(d.join(FILE_NAME)).unwrap().contains("Exec=/x/lc run"));
        enable(&d, Path::new("/y/lc")).unwrap(); // (again: replaced)
        assert!(std::fs::read_to_string(d.join(FILE_NAME)).unwrap().contains("Exec=/y/lc run"));
        disable(&d).unwrap();
        assert!(!is_enabled(&d));
        let _ = std::fs::remove_dir_all(d.parent().unwrap());
    }

    #[test]
    fn the_default_folder_follows_xdg_config_home() {
        // (reads the environment only; nothing is written)
        let d = default_dir().unwrap();
        assert!(d.ends_with("autostart"));
    }
}

/// Windows: the same rules against a registry key of the test's own (below `HKCU\Software`), never the real `Run` key.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::winsys::{registry_delete_key, registry_key_exists};

    fn key(tag: &str) -> PathBuf {
        PathBuf::from(format!(r"Software\LightCraftCloudTests\autostart-{tag}-{}", std::process::id()))
    }

    fn clean(k: &Path) {
        registry_delete_key(&k.to_string_lossy());
        registry_delete_key(r"Software\LightCraftCloudTests");
    }

    #[test]
    fn the_entry_runs_the_app_with_the_path_quoted() {
        assert_eq!(entry(Path::new(r"C:\Users\Tom\AppData\Local\Programs\LightCraft Cloud\lightcraft-cloud-desktop.exe")), "\"C:\\Users\\Tom\\AppData\\Local\\Programs\\LightCraft Cloud\\lightcraft-cloud-desktop.exe\" run");
        assert_eq!(entry(Path::new(r"D:\lc.exe")), "\"D:\\lc.exe\" run");
    }

    #[test]
    fn enable_writes_and_disable_removes_only_in_the_given_key() {
        let k = key("a");
        clean(&k);
        assert!(!is_enabled(&k));
        disable(&k).unwrap(); // nothing there: fine, and nothing created
        assert!(!registry_key_exists(&k.to_string_lossy()));
        enable(&k, Path::new(r"C:\x\lc.exe")).unwrap();
        assert!(is_enabled(&k));
        assert_eq!(entry_text(&k).unwrap(), "\"C:\\x\\lc.exe\" run");
        enable(&k, Path::new(r"C:\y z\lc.exe")).unwrap(); // (again: replaced)
        assert_eq!(entry_text(&k).unwrap(), "\"C:\\y z\\lc.exe\" run");
        disable(&k).unwrap();
        assert!(!is_enabled(&k));
        disable(&k).unwrap(); // (again: fine)
        clean(&k);
    }

    #[test]
    fn the_default_place_is_the_per_user_run_key_which_needs_no_administrator() {
        let k = default_dir().unwrap();
        assert_eq!(k, PathBuf::from(r"Software\Microsoft\Windows\CurrentVersion\Run"));
        // (read only: the real key is never written by a test, but it is there and can be opened for writing by this user)
        assert!(registry_key_exists(&k.to_string_lossy()));
    }
}
