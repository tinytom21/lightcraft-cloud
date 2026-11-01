//! The Status and Settings panels, rendered headlessly with `egui_kittest` (no window, no display).

use std::path::PathBuf;
use std::time::SystemTime;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use lightcraft_cloud_desktop::appstate::{Activity, AppStatus, LastSync, Notice};
#[cfg(feature = "home")]
use lightcraft_cloud_desktop::autostart;
use lightcraft_cloud_desktop::clock::parse_rfc3339;
use lightcraft_cloud_client::other::FolderInfo;
use lightcraft_cloud_desktop::appstate::OtherView;
use lightcraft_cloud_desktop::config::{Config, Role};
use lightcraft_cloud_desktop::settings::{Form, SaveEnv, Storage};
use lightcraft_cloud_desktop::ui::SettingsPanel;
use lightcraft_cloud_desktop::ui::settings_panel::{FolderLister, Folders};
use lightcraft_cloud_desktop::ui::status::{self, StatusView};

fn now() -> SystemTime {
    parse_rfc3339("2026-11-01T12:00:00Z").unwrap()
}

fn status_harness<'a>(s: Option<AppStatus>, running: bool) -> Harness<'a, Option<AppStatus>> {
    Harness::new_ui_state(move |ui, s: &mut Option<AppStatus>| status::show(ui, &StatusView { status: s.as_ref(), running, now: now(), update: None }), s)
}

fn busy_status() -> AppStatus {
    AppStatus {
        configured: true,
        edits_applied: 7,
        inbox_pending: Some(2),
        imports_pending: Some(3),
        photos_imported: 12,
        last_imported: 4,
        next_sync: Some("2026-11-01T12:12:00Z".into()),
        last_sync: Some(LastSync {
            when: "2026-11-01T11:55:00Z".into(),
            ok: true,
            result: "ok".into(),
            photos: 1234,
            applied: 2,
            merged: 1,
            kept: 1,
        }),
        notices: vec![Notice { when: "2026-11-01T11:55:00Z".into(), text: "Newest wins: the PC's newer sidecar was kept (x.xmp)".into() }],
        errors: vec![Notice { when: "2026-11-01T10:00:00Z".into(), text: "remote: quota exceeded".into() }],
        ..AppStatus::default()
    }
}

#[test]
fn the_status_panel_shows_the_last_sync_the_inbox_and_the_next_run() {
    let mut h = status_harness(Some(busy_status()), true);
    h.run();
    h.get_by_label("Status");
    h.get_by_label_contains("5 min ago (ok)");
    h.get_by_label("1234");
    h.get_by_label_contains("3 in the last sync (1 merged with PC changes), 1 kept off by a newer PC sidecar; 7 in all");
    h.get_by_label("2 phone edits waiting");
    h.get_by_label("3 files from other PCs waiting");
    h.get_by_label("4 in the last sync; 12 in all");
    h.get_by_label("in 12 min");
    h.get_by_label_contains("Newest wins: the PC's newer sidecar was kept");
    h.get_by_label_contains("remote: quota exceeded");
    // (phone edits and imports are waiting: not "Up to date")
    h.get_by_label("2 phone edits waiting; 3 files from other PCs waiting to be added");
    assert!(h.query_by_label("Up to date").is_none());
    assert!(h.query_by_label_contains("not running").is_none());
}

#[test]
fn the_status_panel_says_when_syncing_paused_failed_not_set_up_or_not_running() {
    let mut s = busy_status();
    s.activity = Activity::Syncing { since: "2026-11-01T12:00:00Z".into(), reason: "Sync now".into(), progress: None };
    let mut h = status_harness(Some(s.clone()), true);
    h.run();
    h.get_by_label("Syncing now (Sync now)");

    s.activity = Activity::Idle;
    s.paused = true;
    s.next_sync = None;
    let mut h = status_harness(Some(s.clone()), false);
    h.run();
    h.get_by_label("Paused");
    h.get_by_label("paused (Sync now still works)");
    h.get_by_label_contains("The app is not running");

    s.last_error = Some("boom".into());
    let mut h = status_harness(Some(s.clone()), true);
    h.run();
    h.get_by_label("The last sync failed");

    let mut h = status_harness(Some(AppStatus::default()), true);
    h.run();
    h.get_by_label_contains("Not set up yet");
    h.get_by_label("never");
    h.get_by_label("after Setup");

    let mut h = status_harness(None, false);
    h.run();
    h.get_by_label("The app has not run yet.");
}

#[test]
fn the_status_panel_shows_the_lightcraft_row_only_while_photos_wait() {
    let mut s = busy_status();
    s.lightcraft_port = 7980;
    let mut h = status_harness(Some(s.clone()), true);
    h.run();
    assert!(h.query_by_label_contains("Waiting for LightCraft").is_none());
    s.lightcraft_waiting = 2;
    s.lightcraft_reachable = Some(false);
    let mut h = status_harness(Some(s.clone()), true);
    h.run();
    h.get_by_label("Waiting for LightCraft: 2 files");
    h.get_by_label_contains("not answering on port 7980");
    s.lightcraft_reachable = Some(true);
    let mut h = status_harness(Some(s), true);
    h.run();
    h.get_by_label("Waiting for LightCraft: 2 files");
    assert!(h.query_by_label_contains("not answering").is_none());
}

struct Rig {
    dir: PathBuf,
    #[cfg_attr(not(feature = "home"), allow(dead_code))]
    photos: PathBuf,
    env: SaveEnv,
}

fn rig(tag: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!("lc-desktop-ui-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let photos = dir.join("photos");
    std::fs::create_dir_all(&photos).unwrap();
    let env =
        SaveEnv { config_path: dir.join("cfg/config.toml"), autostart_dir: dir.join("autostart"), exe: PathBuf::from("/x/lightcraft-cloud-desktop") };
    Rig { dir, photos, env }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(feature = "home")]
fn panel(r: &Rig, setup: bool) -> SettingsPanel {
    let form = Form::first_run(false);
    SettingsPanel::new(form, Ok(vec!["gdrive".into(), "lccrypt".into()]), r.env.clone(), setup)
}

fn settings_harness<'a>(p: SettingsPanel) -> Harness<'a, SettingsPanel> {
    // (tall: the form is longer than a default test window, and a button below its edge cannot be clicked)
    Harness::builder().with_size(egui::vec2(800.0, 1500.0)).build_ui_state(|ui, p: &mut SettingsPanel| p.show(ui), p)
}

#[cfg(feature = "home")]
#[test]
fn setup_offers_every_choice_and_refuses_an_empty_form_without_writing() {
    let r = rig("a");
    let mut h = settings_harness(panel(&r, true));
    h.run();
    for label in [
        "Set up LightCraft Cloud",
        "Photo folder",
        "Browse…",
        "One of my rclone remotes",
        "A folder on this PC",
        "Originals and previews",
        "Previews only",
        "Show desktop notifications",
        "Start when I log in",
        "Save and start",
    ] {
        h.get_by_label(label);
    }
    h.state_mut().form.source = String::new();
    h.get_by_label("Save and start").click();
    h.run();
    h.get_by_label_contains("Choose the photo folder");
    h.get_by_label_contains("Choose where to store the cloud library");
    assert!(!h.state().saved && !r.env.config_path.exists() && !r.env.autostart_dir.exists());
}

#[cfg(feature = "home")]
#[test]
fn choosing_a_remote_and_saving_writes_the_config_and_nothing_else() {
    let r = rig("b");
    let mut h = settings_harness(panel(&r, true));
    h.state_mut().form.source = r.photos.to_string_lossy().into_owned();
    h.run();
    h.get_by_role_and_label(egui::accesskit::Role::ComboBox, "Remote").click();
    h.run();
    h.get_by_label("lccrypt").click();
    h.run();
    h.get_by_label("Previews only").click();
    h.run();
    h.get_by_label("Show desktop notifications").click(); // off
    h.run();
    h.get_by_label("Save and start").click();
    h.run();
    assert!(h.state().saved, "{:?}", h.state().problems);
    let c = Config::load(Some(&r.env.config_path)).unwrap();
    assert_eq!((c.file.remote.as_deref(), c.file.previews_only, c.file.notify), (Some("lccrypt:LightCraft Cloud"), true, false));
    assert!(!r.env.autostart_dir.exists(), "the autostart entry is only written when its box is ticked");
    h.get_by_label_contains("Saved ");
}

#[cfg(feature = "home")]
#[test]
fn ticking_start_when_i_log_in_writes_the_autostart_entry_into_the_given_folder_only() {
    let r = rig("c");
    let mut p = panel(&r, false);
    p.form.source = r.photos.to_string_lossy().into_owned();
    p.form.storage = Storage::Rclone { remote: "gdrive".into(), path: "LC".into() };
    let mut h = settings_harness(p);
    h.run();
    h.get_by_label("Settings");
    h.get_by_label("Save");
    h.get_by_label("Start when I log in").click();
    h.run();
    assert!(h.state().form.start_at_login);
    h.get_by_label("Save").click();
    h.run();
    assert!(h.state().saved);
    assert!(autostart::is_enabled(&r.env.autostart_dir));
}

#[cfg(feature = "home")]
#[test]
fn a_remote_that_is_not_in_the_rclone_config_is_refused_and_a_failed_listing_is_explained() {
    let r = rig("d");
    let mut p = panel(&r, false);
    p.form.source = r.photos.to_string_lossy().into_owned();
    p.form.storage = Storage::Rclone { remote: "gone".into(), path: "LC".into() };
    let mut h = settings_harness(p);
    h.run();
    h.get_by_label("Save").click();
    h.run();
    h.get_by_label_contains("\"gone\" is not in your rclone config");
    assert!(!r.env.config_path.exists());

    let mut p = panel(&r, false);
    p.remotes = Err("rclone config /x/rclone.conf not found".into());
    let mut h = settings_harness(p);
    h.run();
    h.get_by_label_contains("Could not list your rclone remotes: rclone config /x/rclone.conf not found");
}

#[cfg(feature = "home")]
#[test]
fn a_folder_on_this_pc_can_be_the_storage() {
    let r = rig("e");
    let mut p = panel(&r, false);
    p.form.source = r.photos.to_string_lossy().into_owned();
    let mut h = settings_harness(p);
    h.run();
    h.get_by_label("A folder on this PC").click();
    h.run();
    h.state_mut().form.storage = Storage::Local { dir: r.dir.join("cloud").to_string_lossy().into_owned() };
    h.run();
    h.get_by_label("Save").click();
    h.run();
    assert!(h.state().saved, "{:?}", h.state().problems);
    assert_eq!(Config::load(Some(&r.env.config_path)).unwrap().file.remote, Some(r.dir.join("cloud").to_string_lossy().into_owned()));
}

// ------------------------------------------------------------------ another PC

/// A lister that answers at once with a fixed list (or error), and remembers it was asked.
#[derive(Clone, Default)]
struct FakeLister(std::sync::Arc<std::sync::Mutex<(bool, Option<Result<Vec<FolderInfo>, String>>)>>);

impl FolderLister for FakeLister {
    fn start(&mut self, _file: &lightcraft_cloud_desktop::config::File) {
        self.0.lock().unwrap().0 = true;
    }
    fn waiting(&self) -> bool {
        false
    }
    fn poll(&mut self) -> Option<Result<Vec<FolderInfo>, String>> {
        let g = self.0.lock().unwrap();
        if g.0 { g.1.clone() } else { None }
    }
}

fn folder(name: &str, photos: usize, mb: u64) -> FolderInfo {
    FolderInfo { folder: name.into(), photos, bytes: mb << 20 }
}

fn other_panel(r: &Rig, lister: &FakeLister) -> SettingsPanel {
    let mut form = Form::first_run(false);
    form.set_role(Role::Other);
    form.source = r.dir.join("pc-photos").to_string_lossy().into_owned();
    form.storage = Storage::Local { dir: r.dir.join("cloud").to_string_lossy().into_owned() };
    let mut p = SettingsPanel::new(form, Ok(vec![]), r.env.clone(), true);
    p.lister = Box::new(lister.clone());
    p
}

#[cfg(feature = "home")]
#[test]
fn setup_asks_what_the_pc_is_and_another_pc_gets_its_own_form() {
    let r = rig("o1");
    let mut h = settings_harness(panel(&r, true));
    h.run();
    h.get_by_label("What is this PC?");
    h.get_by_label("This is my home PC");
    h.get_by_label("This is another PC");
    h.get_by_label("Folders to leave out (one name pattern per line, e.g. exported)");
    h.get_by_label("Previews only");
    assert!(h.query_by_label("Name of this PC").is_none());
    h.get_by_label("This is another PC").click();
    h.run();
    assert_eq!(h.state().form.role, Role::Other);
    for label in ["Photos on this PC", "Folder for the photos", "Folders to keep on this PC", "Show the folders in the cloud library", "Name of this PC", "Cloud library"] {
        h.get_by_label(label);
    }
    assert!(h.query_by_label("Previews only").is_none() && h.query_by_label("Photo folder").is_none());
    // the Setup text says plainly where LightCraft must import to, and what is not uploaded
    h.get_by_label_contains("Set LightCraft's import destination to this folder");
    h.get_by_label_contains("Photos imported anywhere else are not uploaded");
    h.get_by_label_contains("added to the ones kept on this PC automatically");
    assert!(h.state().form.source.is_empty(), "no usual folder is offered for another PC");
    h.get_by_label("This is my home PC").click();
    h.run();
    h.get_by_label("Previews only");
}

#[cfg(not(feature = "home"))]
#[test]
fn the_light_build_offers_only_another_pc() {
    let r = rig("o1l");
    let mut h = settings_harness(other_panel(&r, &FakeLister::default()));
    h.run();
    assert!(h.query_by_label("This is my home PC").is_none() && h.query_by_label("What is this PC?").is_none());
    h.get_by_label("Folders to keep on this PC");
    h.get_by_label("Folder for the photos");
}

#[test]
fn the_folders_are_listed_with_sizes_and_photo_counts_and_the_ticked_ones_are_saved() {
    let r = rig("o2");
    let lister = FakeLister::default();
    lister.0.lock().unwrap().1 = Some(Ok(vec![folder("", 2, 10), folder("Lisbon", 120, 3500), folder("Porto", 1, 12)]));
    let mut h = settings_harness(other_panel(&r, &lister));
    h.run();
    h.get_by_label_contains("The folders are listed here");
    h.get_by_label("Show the folders in the cloud library").click();
    h.run();
    h.get_by_label("(photos at the top level)  \u{2014}  2 photos, 10.0 MiB");
    h.get_by_label("Lisbon  \u{2014}  120 photos, 3.4 GiB").click();
    h.get_by_label("Porto  \u{2014}  1 photo, 12.0 MiB").click();
    h.run();
    h.get_by_label("2 folders ticked: 121 photos, 3.4 GiB in all");
    h.get_by_label("Read the list again");
    h.get_by_label("Porto  \u{2014}  1 photo, 12.0 MiB").click(); // untick again
    h.run();
    h.get_by_label("1 folder ticked: 120 photos, 3.4 GiB in all");
    h.get_by_label("Save and start").click();
    h.run();
    assert!(h.state().saved, "{:?}", h.state().problems);
    let c = Config::load(Some(&r.env.config_path)).unwrap();
    assert_eq!((c.file.role, c.file.folders.clone(), c.file.remote.is_some()), (Role::Other, vec!["Lisbon".to_string()], true));
    assert!(r.dir.join("pc-photos").is_dir(), "the photo folder was made");
}

#[test]
fn a_folder_that_left_the_cloud_library_is_shown_as_such_and_a_failed_read_is_explained() {
    let r = rig("o3");
    let lister = FakeLister::default();
    lister.0.lock().unwrap().1 = Some(Ok(vec![folder("Lisbon", 3, 30)]));
    let mut p = other_panel(&r, &lister);
    p.form.set_folder("Lisbon", true);
    p.form.set_folder("Gone", true);
    let mut h = settings_harness(p);
    h.run();
    h.get_by_label("Show the folders in the cloud library").click();
    h.run();
    h.get_by_label("Gone  \u{2014}  no longer in the cloud library");
    h.get_by_label("2 folders ticked: 3 photos, 30.0 MiB in all");

    lister.0.lock().unwrap().1 = Some(Err("remote: connection refused".into()));
    let mut h = settings_harness(other_panel(&r, &lister));
    h.run();
    h.get_by_label("Show the folders in the cloud library").click();
    h.run();
    h.get_by_label_contains("Could not read the cloud library: remote: connection refused");
    assert!(matches!(h.state().folders, Folders::Failed(_)));
}

#[test]
fn saved_settings_of_another_pc_show_their_folders_without_a_click() {
    let r = rig("o4");
    let lister = FakeLister::default();
    lister.0.lock().unwrap().1 = Some(Ok(vec![folder("Lisbon", 3, 30), folder("Porto", 5, 50)]));
    let mut p = other_panel(&r, &lister);
    p.setup = false;
    p.form.set_folder("Porto", true);
    let mut h = settings_harness(p);
    h.run();
    h.run();
    assert!(lister.0.lock().unwrap().0, "it asked");
    h.get_by_label("Porto  \u{2014}  5 photos, 50.0 MiB");
    h.get_by_label("1 folder ticked: 5 photos, 50.0 MiB in all");
    h.get_by_label("Settings");
}

fn other_status() -> AppStatus {
    AppStatus {
        configured: true,
        other: Some(OtherView { photos: 321, waiting_to_download: 12, edits_waiting: 2, held_back: 1, received: 400, ..OtherView::default() }),
        next_sync: Some("2026-11-01T12:12:00Z".into()),
        last_sync: Some(LastSync { when: "2026-11-01T11:55:00Z".into(), ok: true, result: "ok".into(), photos: 321, applied: 0, merged: 0, kept: 0 }),
        ..AppStatus::default()
    }
}

#[test]
fn another_pcs_status_shows_what_it_holds_and_what_waits_and_none_of_the_home_rows() {
    let mut h = status_harness(Some(other_status()), true);
    h.run();
    h.get_by_label("Photos on this PC");
    h.get_by_label("321");
    h.get_by_label("Waiting to download");
    h.get_by_label("12 photos");
    h.get_by_label("Edits waiting to send");
    h.get_by_label("2 edits");
    h.get_by_label_contains("1 edit sent, not merged there yet");
    h.get_by_label("in 12 min");
    h.get_by_label("12 photos waiting to download; 2 edits waiting to send; 1 edit waiting to be merged at the home PC");
    assert!(h.query_by_label("Up to date").is_none(), "something is waiting");
    for gone in ["Photos published", "Phone edits applied", "Inbox", "Imports", "Newest wins"] {
        assert!(h.query_by_label(gone).is_none(), "{gone}");
    }
    h.get_by_label("Problems");

    let mut s = other_status();
    s.other = Some(OtherView::default());
    let mut h = status_harness(Some(s), true);
    h.run();
    h.get_by_label("nothing");
    h.get_by_label("none");
}

#[test]
fn being_offline_reads_as_a_calm_state_not_a_failure() {
    let mut s = other_status();
    s.offline = true;
    s.errors = vec![Notice { when: "2026-11-01T11:55:00Z".into(), text: lightcraft_cloud_desktop::service::OFFLINE_LINE.into() }];
    let mut h = status_harness(Some(s), true);
    h.run();
    h.get_by_label("Offline: will try again");
    h.get_by_label_contains("Cannot reach the cloud library right now");
    assert!(h.query_by_label("The last sync failed").is_none());
}
