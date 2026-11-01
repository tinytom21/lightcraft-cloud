//! The Setup / Settings form without the window: what the user can change, the checks, and saving (the config file, and the
//! autostart entry when the "Start when I log in" box was changed). The egui panel in `ui` only draws and edits a [`Form`].

use std::collections::BTreeSet;
use std::path::PathBuf;

use lightcraft_cloud_client::valid_device;

use crate::autostart;
use crate::config::{Config, File, Role, expand, local_remote};

/// Where the cloud library lives: one of the user's rclone remotes and a folder on it, or (for a synced folder) a folder on
/// this PC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Storage {
    Rclone { remote: String, path: String },
    Local { dir: String },
}

impl Storage {
    /// `name:folder`, `name:`, or a local folder.
    pub fn parse(spec: &str) -> Storage {
        if local_remote(spec).ok().flatten().is_some() {
            return Storage::Local { dir: spec.to_string() };
        }
        match spec.split_once(':') {
            Some((remote, path)) => Storage::Rclone { remote: remote.to_string(), path: path.to_string() },
            None => Storage::Rclone { remote: spec.to_string(), path: String::new() },
        }
    }

    pub fn spec(&self) -> String {
        match self {
            Storage::Rclone { remote, path } => format!("{remote}:{}", path.trim().trim_matches('/')),
            Storage::Local { dir } => dir.trim().to_string(),
        }
    }
}

/// Where the Settings window saves to (tests point these at temporary folders).
#[derive(Debug, Clone)]
pub struct SaveEnv {
    pub config_path: PathBuf,
    pub autostart_dir: PathBuf,
    /// The app's own executable, for the autostart entry.
    pub exe: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Form {
    /// What this PC is. The light build has no choice: it is always another PC.
    pub role: Role,
    pub source: String,
    /// Role other: the folders of the cloud library to keep a copy of (relative to `originals/`).
    pub folders: BTreeSet<String>,
    /// Role other: this PC's name in the cloud library; empty: the host name.
    pub device: String,
    /// One folder-name pattern per line.
    pub exclude: String,
    pub storage: Storage,
    pub previews_only: bool,
    pub notifications: bool,
    pub start_at_login: bool,
    /// The settings the form does not show stay as they were (thumbnail size, `max_delete`, extra keys...).
    base: File,
    login_before: bool,
}

impl Form {
    /// The form for a config file's settings; `start_at_login`: whether the autostart entry exists now.
    pub fn from_file(file: &File, start_at_login: bool) -> Form {
        Form {
            role: if cfg!(feature = "home") { file.role } else { Role::Other },
            source: file.source.clone(),
            folders: file.folders.iter().map(|f| f.trim().trim_matches('/').to_string()).collect(),
            device: file.device.clone().unwrap_or_default(),
            exclude: file.exclude.join("\n"),
            storage: match file.remote.as_deref() {
                Some(spec) => Storage::parse(spec),
                None => Storage::Rclone { remote: String::new(), path: "LightCraft Cloud".into() },
            },
            previews_only: file.previews_only,
            notifications: file.notify,
            start_at_login,
            base: file.clone(),
            login_before: start_at_login,
        }
    }

    /// The form of a first run: nothing chosen yet but the usual folder names.
    pub fn first_run(start_at_login: bool) -> Form {
        let mut f = Form::from_file(&File::default(), start_at_login);
        if !cfg!(feature = "home") {
            f.source.clear(); // (the home PC's usual folder is not offered to another PC)
        }
        f
    }

    /// Choose what this PC is. Another PC's photo folder is its own to choose: the home PC's usual folder is not offered for it.
    pub fn set_role(&mut self, role: Role) {
        if role == self.role {
            return;
        }
        let usual = File::default().source;
        if role == Role::Other && self.source == usual {
            self.source.clear();
        } else if role == Role::Home && self.source.trim().is_empty() {
            self.source = usual;
        }
        self.role = role;
    }

    pub fn is_other(&self) -> bool {
        self.role == Role::Other
    }

    pub fn set_folder(&mut self, folder: &str, on: bool) {
        if on {
            self.folders.insert(folder.to_string());
        } else {
            self.folders.remove(folder);
        }
    }

    pub fn exclude_list(&self) -> Vec<String> {
        self.exclude.split(['\n', ',']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
    }

    /// The settings this form makes.
    pub fn to_file(&self) -> File {
        let mut f = self.base.clone();
        f.role = self.role;
        f.source = self.source.trim().to_string();
        f.folders = self.folders.iter().cloned().collect();
        f.device = Some(self.device.trim().to_string()).filter(|d| !d.is_empty());
        f.exclude = self.exclude_list();
        f.remote = Some(self.storage.spec());
        f.previews_only = self.previews_only;
        f.notify = self.notifications;
        f
    }

    /// What is wrong with the form, in words for the user (empty: it can be saved). `remotes`: the rclone remote names, when
    /// they could be listed.
    pub fn problems(&self, remotes: Option<&[String]>) -> Vec<String> {
        let mut p = Vec::new();
        let source = self.source.trim();
        if self.is_other() {
            self.other_problems(source, &mut p);
        } else if source.is_empty() {
            p.push("Choose the photo folder to publish.".to_string());
        } else {
            match expand(source) {
                Ok(d) if d.is_dir() => {}
                Ok(d) => p.push(format!("The photo folder {} does not exist.", d.display())),
                Err(e) => p.push(e),
            }
        }
        match &self.storage {
            Storage::Rclone { remote, .. } => {
                if remote.trim().is_empty() {
                    p.push("Choose where to store the cloud library: one of your rclone remotes.".to_string());
                } else if remotes.is_some_and(|r| !r.contains(remote)) {
                    p.push(format!("The rclone remote \"{remote}\" is not in your rclone config."));
                }
            }
            Storage::Local { dir } => {
                if dir.trim().is_empty() {
                    p.push("Choose the folder on this PC that holds the cloud library.".to_string());
                }
            }
        }
        if p.is_empty()
            && let Err(e) = Config::from_file(PathBuf::from("config.toml"), self.to_file())
        {
            p.push(e);
        }
        p
    }

    /// Another PC's own checks: its photo folder (made when saved, if its parent is there), the folders to keep, its name.
    fn other_problems(&self, source: &str, p: &mut Vec<String>) {
        if source.is_empty() {
            p.push("Choose the folder on this PC where the photos are kept.".to_string());
        } else {
            match expand(source) {
                Ok(d) if d.is_dir() => {}
                Ok(d) if d.exists() => p.push(format!("{} is a file, not a folder.", d.display())),
                Ok(d) if d.parent().is_some_and(|parent| parent.as_os_str().is_empty() || parent.is_dir()) => {} // (made when saved)
                Ok(d) => p.push(format!("The folder {} cannot be made: the folder above it does not exist.", d.display())),
                Err(e) => p.push(e),
            }
        }
        if self.folders.is_empty() {
            p.push("Tick at least one folder to keep on this PC.".to_string());
        }
        let device = self.device.trim();
        if !device.is_empty() && !valid_device(device) {
            p.push("The name of this PC may use letters, digits, - _ and . only (up to 64 characters).".to_string());
        }
    }

    /// Check and write the config; apply the autostart box only if it was changed. Returns what was done, or the problems.
    pub fn save(&mut self, env: &SaveEnv, remotes: Option<&[String]>) -> Result<Vec<String>, Vec<String>> {
        let problems = self.problems(remotes);
        if !problems.is_empty() {
            return Err(problems);
        }
        let mut done = Vec::new();
        if self.is_other() {
            let dir = expand(self.source.trim()).map_err(|e| vec![e])?;
            std::fs::create_dir_all(&dir).map_err(|e| vec![format!("{}: {e}", dir.display())])?;
        }
        self.to_file().save(&env.config_path).map_err(|e| vec![e])?;
        done.push(format!("Saved {}", env.config_path.display()));
        if self.start_at_login != self.login_before {
            if self.start_at_login {
                autostart::enable(&env.autostart_dir, &env.exe).map_err(|e| vec![e])?;
                done.push("The app will start when you log in.".to_string());
            } else {
                autostart::disable(&env.autostart_dir).map_err(|e| vec![e])?;
                done.push("The app will no longer start when you log in.".to_string());
            }
            self.login_before = self.start_at_login;
        }
        self.base = self.to_file();
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lc-desktop-settings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn env(d: &std::path::Path) -> SaveEnv {
        SaveEnv { config_path: d.join("cfg/config.toml"), autostart_dir: d.join("autostart"), exe: PathBuf::from("/x/lightcraft-cloud-desktop") }
    }

    #[test]
    fn storage_is_a_remote_and_a_folder_on_it_or_a_local_folder() {
        assert_eq!(Storage::parse("gdrive:LightCraft Cloud"), Storage::Rclone { remote: "gdrive".into(), path: "LightCraft Cloud".into() });
        assert_eq!(Storage::parse("lccrypt:"), Storage::Rclone { remote: "lccrypt".into(), path: String::new() });
        assert_eq!(Storage::parse("/srv/lc"), Storage::Local { dir: "/srv/lc".into() });
        assert_eq!(Storage::Rclone { remote: "gd".into(), path: " /LC/ ".into() }.spec(), "gd:LC");
        assert_eq!(Storage::parse("a:b c").spec(), "a:b c");
    }

    #[cfg(feature = "home")]
    #[test]
    fn the_form_checks_the_folder_and_the_storage() {
        let d = temp("a");
        let mut f = Form::first_run(false);
        f.source = String::new();
        assert_eq!(f.problems(None).len(), 2); // (no folder, no remote)
        f.source = d.join("nope").to_string_lossy().into_owned();
        f.storage = Storage::Rclone { remote: "gd".into(), path: "LC".into() };
        assert!(f.problems(None)[0].contains("does not exist"));
        f.source = d.to_string_lossy().into_owned();
        assert!(f.problems(None).is_empty());
        assert!(f.problems(Some(&["other".to_string()]))[0].contains("not in your rclone config"));
        assert!(f.problems(Some(&["gd".to_string()])).is_empty());
        f.storage = Storage::Local { dir: " ".into() };
        assert!(f.problems(None)[0].contains("folder on this PC"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(feature = "home")]
    #[test]
    fn saving_writes_the_config_and_leaves_the_rest_alone() {
        let d = temp("b");
        let e = env(&d);
        let photos = d.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        let mut f = Form::first_run(false);
        f.source = photos.to_string_lossy().into_owned();
        f.exclude = "exported, Thumbnails\n\n  cache ".into();
        f.storage = Storage::Rclone { remote: "gd".into(), path: "LC".into() };
        f.previews_only = true;
        f.notifications = false;
        let done = f.save(&e, Some(&["gd".to_string()])).unwrap();
        assert_eq!(done.len(), 1); // (the autostart box was not touched: nothing written there)
        assert!(!e.autostart_dir.exists());
        let c = Config::load(Some(&e.config_path)).unwrap();
        assert_eq!(
            (c.file.exclude.clone(), c.file.remote.as_deref(), c.file.previews_only, c.file.notify),
            (vec!["exported".to_string(), "Thumbnails".into(), "cache".into()], Some("gd:LC"), true, false)
        );
        assert_eq!((c.file.thumb_px, c.file.max_delete), (512, 50));
        // a form made from the saved file keeps what it does not show
        std::fs::write(&e.config_path, format!("source = \"{}\"\nremote = \"gd:LC\"\nthumb_px = 256\nmax_delete = 7\n", photos.display())).unwrap();
        let c = Config::load(Some(&e.config_path)).unwrap();
        let mut f2 = Form::from_file(&c.file, false);
        f2.notifications = false;
        f2.save(&e, None).unwrap();
        let c = Config::load(Some(&e.config_path)).unwrap();
        assert_eq!((c.file.thumb_px, c.file.max_delete, c.file.notify), (256, 7, false));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(feature = "home")]
    #[test]
    fn a_bad_form_writes_nothing() {
        let d = temp("c");
        let e = env(&d);
        let mut f = Form::first_run(true);
        f.source = "/nonexistent-folder-for-lc-test".into();
        assert!(f.save(&e, None).is_err());
        assert!(!e.config_path.exists() && !e.autostart_dir.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(feature = "home")]
    #[test]
    fn the_login_box_writes_and_removes_the_autostart_entry_only_when_changed() {
        let d = temp("d");
        let e = env(&d);
        let mut f = Form::first_run(false);
        f.source = d.to_string_lossy().into_owned();
        f.storage = Storage::Local { dir: d.join("lib").to_string_lossy().into_owned() };
        f.start_at_login = true;
        let done = f.save(&e, None).unwrap();
        assert!(done.iter().any(|m| m.contains("start when you log in")) && autostart::is_enabled(&e.autostart_dir));
        let entry = e.autostart_dir.join(autostart::FILE_NAME);
        std::fs::write(&entry, "marker").unwrap();
        f.save(&e, None).unwrap(); // saved again, box unchanged: the entry is not rewritten
        assert_eq!(std::fs::read_to_string(&entry).unwrap(), "marker");
        f.start_at_login = false;
        f.save(&e, None).unwrap();
        assert!(!autostart::is_enabled(&e.autostart_dir));
        // a form opened with the box already ticked and saved as it is leaves the entry alone
        enable_marker(&entry);
        let mut g = Form::first_run(true);
        g.source = f.source.clone();
        g.storage = f.storage.clone();
        g.save(&e, None).unwrap();
        assert_eq!(std::fs::read_to_string(&entry).unwrap(), "marker");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn another_pc_needs_a_folder_here_folders_to_keep_and_a_valid_name() {
        let d = temp("e");
        let mut f = Form::first_run(false);
        f.set_role(Role::Other);
        assert!(f.source.is_empty(), "the home PC's usual folder is not offered");
        f.storage = Storage::Local { dir: d.join("lib").to_string_lossy().into_owned() };
        let p = f.problems(None);
        assert_eq!(p.len(), 2, "{p:?}"); // (no folder here, nothing ticked)
        f.source = d.join("pc-photos").to_string_lossy().into_owned(); // (does not exist yet: made when saved)
        f.set_folder("Lisbon", true);
        assert!(f.problems(None).is_empty());
        f.device = "my laptop!".into();
        assert!(f.problems(None)[0].contains("name of this PC"));
        f.device = "laptop".into();
        f.source = d.join("nope/deeper").to_string_lossy().into_owned();
        assert!(f.problems(None)[0].contains("cannot be made"));
        f.source = d.join("pc-photos").to_string_lossy().into_owned();
        f.set_folder("Lisbon", false);
        f.set_folder("Porto", true);
        f.set_folder("Faro", true);
        let e = env(&d);
        let done = f.save(&e, None).unwrap();
        assert_eq!(done.len(), 1);
        assert!(d.join("pc-photos").is_dir());
        let c = Config::load(Some(&e.config_path)).unwrap();
        assert_eq!((c.file.role, c.file.folders.clone(), c.file.device.as_deref()), (Role::Other, vec!["Faro".to_string(), "Porto".into()], Some("laptop")));
        // reopened: the form shows what was saved; the home PC's settings are untouched by an other PC's form
        let g = Form::from_file(&c.file, false);
        assert_eq!((g.role, g.folders.len(), g.device.as_str()), (Role::Other, 2, "laptop"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(feature = "home")]
    #[test]
    fn a_home_form_writes_role_home_and_no_other_pc_keys_that_matter() {
        let d = temp("f");
        let e = env(&d);
        let mut f = Form::first_run(false);
        f.source = d.to_string_lossy().into_owned();
        f.storage = Storage::Local { dir: d.join("lib").to_string_lossy().into_owned() };
        f.save(&e, None).unwrap();
        let c = Config::load(Some(&e.config_path)).unwrap();
        assert_eq!((c.file.role, c.file.folders.is_empty(), c.file.device), (Role::Home, true, None));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_first_run_form_starts_as_the_builds_role() {
        let f = Form::first_run(false);
        if cfg!(feature = "home") {
            assert_eq!((f.role, f.source.as_str()), (Role::Home, "~/Pictures/Photos"));
        } else {
            // the light build is another PC: it never offers the home PC's usual photo folder
            assert_eq!((f.role, f.source.as_str()), (Role::Other, ""));
        }
        // and an existing home config opens as another PC's form in the light build only
        let file = File { role: Role::Home, ..File::default() };
        assert_eq!(Form::from_file(&file, false).role == Role::Other, !cfg!(feature = "home"));
    }

    #[cfg(feature = "home")]
    fn enable_marker(entry: &std::path::Path) {
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(entry, "marker").unwrap();
    }
}
