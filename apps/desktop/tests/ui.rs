//! The Status and Settings panels, rendered headlessly with `egui_kittest` (no window, no display).

use std::path::PathBuf;
use std::time::SystemTime;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use lightcraft_cloud_desktop::appstate::{Activity, AppStatus, LastSync, Notice};
use lightcraft_cloud_desktop::autostart;
use lightcraft_cloud_desktop::clock::parse_rfc3339;
use lightcraft_cloud_desktop::config::Config;
use lightcraft_cloud_desktop::settings::{Form, SaveEnv, Storage};
use lightcraft_cloud_desktop::ui::SettingsPanel;
use lightcraft_cloud_desktop::ui::status::{self, StatusView};

fn now() -> SystemTime {
    parse_rfc3339("2026-11-01T12:00:00Z").unwrap()
}

fn status_harness<'a>(s: Option<AppStatus>, running: bool) -> Harness<'a, Option<AppStatus>> {
    Harness::new_ui_state(move |ui, s: &mut Option<AppStatus>| status::show(ui, &StatusView { status: s.as_ref(), running, now: now() }), s)
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
    h.get_by_label("3 photos from other PCs waiting");
    h.get_by_label("4 in the last sync; 12 in all");
    h.get_by_label("in 12 min");
    h.get_by_label_contains("Newest wins: the PC's newer sidecar was kept");
    h.get_by_label_contains("remote: quota exceeded");
    h.get_by_label("Up to date");
    assert!(h.query_by_label_contains("not running").is_none());
}

#[test]
fn the_status_panel_says_when_syncing_paused_failed_not_set_up_or_not_running() {
    let mut s = busy_status();
    s.activity = Activity::Syncing { since: "2026-11-01T12:00:00Z".into(), reason: "Sync now".into() };
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
    h.get_by_label("Waiting for LightCraft: 2 photos");
    h.get_by_label_contains("not answering on port 7980");
    s.lightcraft_reachable = Some(true);
    let mut h = status_harness(Some(s), true);
    h.run();
    h.get_by_label("Waiting for LightCraft: 2 photos");
    assert!(h.query_by_label_contains("not answering").is_none());
}

struct Rig {
    dir: PathBuf,
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

fn panel(r: &Rig, setup: bool) -> SettingsPanel {
    let form = Form::first_run(false);
    SettingsPanel::new(form, Ok(vec!["gdrive".into(), "lccrypt".into()]), r.env.clone(), setup)
}

fn settings_harness<'a>(p: SettingsPanel) -> Harness<'a, SettingsPanel> {
    Harness::new_ui_state(|ui, p: &mut SettingsPanel| p.show(ui), p)
}

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
