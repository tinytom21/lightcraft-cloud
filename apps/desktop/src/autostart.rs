//! "Start when I log in": the XDG autostart entry `~/.config/autostart/lightcraft-cloud-desktop.desktop`. Written only when the
//! user ticks the box in Settings and removed when they untick it; every function takes the folder, so tests use a temporary
//! one.

use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "lightcraft-cloud-desktop.desktop";

/// `$XDG_CONFIG_HOME/autostart`, else `~/.config/autostart`.
pub fn default_dir() -> Result<PathBuf, String> {
    Ok(match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join("autostart"),
        None => crate::config::home_dir()?.join(".config/autostart"),
    })
}

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

/// The entry's text: runs `exe run` (the tray app).
pub fn entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=LightCraft Cloud\nComment=Keeps your LightCraft cloud library in step with your photos\nExec={} run\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        quote(&exe.to_string_lossy())
    )
}

pub fn is_enabled(dir: &Path) -> bool {
    dir.join(FILE_NAME).is_file()
}

pub fn enable(dir: &Path, exe: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(FILE_NAME);
    std::fs::write(&path, entry(exe)).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn disable(dir: &Path) -> Result<(), String> {
    let path = dir.join(FILE_NAME);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
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
