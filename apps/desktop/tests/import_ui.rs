//! The Import tab of another PC, headless, with a fake backend: what it asks for, what it refuses up front, what it shows while it works
//! and when it is done. Also the Status panel's states. Both builds run this.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use lightcraft_cloud_client::other::{ImportRun, ImportScan, Phase, Progress, ScanFile, check_destination};
use lightcraft_cloud_desktop::appstate::{Activity, AppStatus, LastSync, OtherView};
use lightcraft_cloud_desktop::clock::parse_rfc3339;
use lightcraft_cloud_desktop::ui::import_panel::{Event, ImportBackend, ImportPanel, Stage};
use lightcraft_cloud_desktop::ui::settings_panel::FolderPicker;
use lightcraft_cloud_desktop::ui::status::{self, StatusView};

#[derive(Default)]
struct Calls {
    scans: Vec<(PathBuf, String)>,
    imports: usize,
    cancels: usize,
    events: Vec<Event>,
    progress: Progress,
    link_off: bool,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Calls>>);

const EXCLUDE: &[&str] = &["private"];

impl ImportBackend for Fake {
    fn folders(&self) -> Vec<String> {
        vec!["260101 - Porto".into(), "261101 - Lisbon".into()]
    }
    fn check(&self, dest: &str) -> Result<String, String> {
        check_destination(dest, &EXCLUDE.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }
    fn lightcraft_on(&self) -> bool {
        !self.0.lock().unwrap().link_off
    }
    fn scan(&mut self, source: PathBuf, dest: String) {
        self.0.lock().unwrap().scans.push((source, dest));
    }
    fn import(&mut self, _scan: ImportScan) {
        self.0.lock().unwrap().imports += 1;
    }
    fn cancel(&mut self) {
        self.0.lock().unwrap().cancels += 1;
    }
    fn busy(&self) -> bool {
        false
    }
    fn progress(&self) -> Progress {
        self.0.lock().unwrap().progress.clone()
    }
    fn poll(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.0.lock().unwrap().events)
    }
}

struct NoPicker;

impl FolderPicker for NoPicker {
    fn start(&mut self, _from: &str, _parent: Option<&dyn lightcraft_cloud_desktop::ui::settings_panel::Parent>) {}
    fn waiting(&self) -> bool {
        false
    }
    fn poll(&mut self) -> Option<PathBuf> {
        None
    }
}

fn panel(fake: &Fake) -> ImportPanel {
    let mut p = ImportPanel::new(Box::new(fake.clone()));
    p.picker = Box::new(NoPicker);
    p
}

fn harness<'a>(p: ImportPanel) -> Harness<'a, ImportPanel> {
    Harness::builder().with_size(egui::vec2(760.0, 1100.0)).build_ui_state(|ui, p: &mut ImportPanel| p.show(ui, None), p)
}

fn file(name: &str, size: u64) -> ScanFile {
    ScanFile { path: PathBuf::from(format!("/card/{name}")), name: name.into(), size, mtime: SystemTime::UNIX_EPOCH, id: "0".repeat(32), sidecar: None, arrived: None }
}

fn scan(new: usize, in_library: usize, on_pc: usize) -> ImportScan {
    ImportScan {
        source: PathBuf::from("/card"),
        dest: "261101 - Lisbon".into(),
        new: (0..new).map(|i| file(&format!("P{i}.ARW"), 24 << 20)).collect(),
        in_library,
        on_this_pc: on_pc,
        new_bytes: (new as u64) * (24 << 20),
        free: Some(220 << 30),
        margin: 256 << 20,
        library_checked: true,
        ..ImportScan::default()
    }
}

#[test]
fn the_tab_asks_for_a_source_and_a_destination_offers_existing_folders_and_refuses_what_the_home_pc_would() {
    let fake = Fake::default();
    let mut h = harness(panel(&fake));
    h.run();
    for l in ["Import photos", "Memory card or folder to import from", "Browse…", "Folder name in your photo folder", "Look at the photos", "260101 - Porto", "261101 - Lisbon"] {
        h.get_by_label(l);
    }
    // nothing typed: the button does nothing
    h.get_by_label("Look at the photos").click();
    h.run();
    assert!(fake.0.lock().unwrap().scans.is_empty());
    // an existing folder is picked from the list (adding to a folder is allowed)
    h.state_mut().source = "/media/card".into();
    h.get_by_label("260101 - Porto").click();
    h.run();
    assert_eq!(h.state().dest, "260101 - Porto");
    h.get_by_label_contains("This folder exists; the photos are added to it.");
    // a destination home would reject is refused up front, with the reason, and the button stays dead
    for (dest, why) in [("private", "exclude"), ("../up", "\"..\""), (".hidden", "hidden"), ("CON", "reserved")] {
        h.state_mut().dest = dest.into();
        h.run();
        h.get_by_label_contains("The home PC would not accept this folder");
        h.get_by_label_contains(why);
        h.get_by_label("Look at the photos").click();
        h.run();
        assert!(fake.0.lock().unwrap().scans.is_empty(), "{dest}");
    }
    // a good new name
    h.state_mut().dest = "  261201 - Madeira/ ".into();
    h.run();
    h.get_by_label_contains("A new folder is made for them.");
    h.get_by_label("Look at the photos").click();
    h.run();
    assert_eq!(fake.0.lock().unwrap().scans, [(PathBuf::from("/media/card"), "261201 - Madeira".to_string())], "the name is passed on as the home PC will see it");
    assert!(matches!(h.state().stage, Stage::Scanning));
}

#[test]
fn while_scanning_it_shows_progress_and_can_be_cancelled() {
    let fake = Fake::default();
    let mut p = panel(&fake);
    p.source = "/media/card".into();
    p.dest = "261101 - Lisbon".into();
    let mut h = harness(p);
    h.run();
    h.get_by_label("Look at the photos").click();
    h.run();
    fake.0.lock().unwrap().progress = Progress {
        phase: Phase::Hashing,
        done: 11,
        total: 480,
        bytes_done: 3 << 30,
        bytes_total: 11 << 30,
        current: "DCIM/100MSDCF/DSC00012.ARW".into(),
    };
    h.run();
    h.get_by_label("Looking at the photos");
    h.get_by_label("Checking photo 12 of 480 (3.0 GiB of 11.0 GiB)");
    h.get_by_label("DCIM/100MSDCF/DSC00012.ARW");
    h.get_by_label("Cancel").click();
    h.run();
    assert_eq!(fake.0.lock().unwrap().cancels, 1);
    h.get_by_label("Stopping…");
    // the cancelled scan returns to the choice, with a note, and the entries are as they were
    let mut cancelled = scan(1, 0, 0);
    cancelled.cancelled = true;
    fake.0.lock().unwrap().events.push(Event::Scanned(Ok(cancelled)));
    h.run();
    assert!(matches!(h.state().stage, Stage::Choose));
    h.get_by_label("The last look was cancelled.");
    assert_eq!(h.state().source, "/media/card");
}

#[test]
fn the_result_of_a_scan_says_how_many_are_new_how_many_are_known_the_size_and_the_free_space() {
    let fake = Fake::default();
    let mut p = panel(&fake);
    p.source = "/media/card".into();
    p.dest = "261101 - Lisbon".into();
    p.stage = Stage::Scanning;
    fake.0.lock().unwrap().events.push(Event::Scanned(Ok(ImportScan { twins: 2, skipped: vec!["bad:name.jpg: the name is not allowed".into()], ..scan(12, 30, 5) })));
    let mut h = harness(p);
    h.run();
    h.get_by_label("12 new photos (288.0 MiB)");
    h.get_by_label("35 already in the library or on this PC");
    h.get_by_label("To copy: 288.0 MiB. Free space: 220.0 GiB.");
    h.get_by_label_contains("Into the folder \"261101 - Lisbon\"");
    h.get_by_label("Import 12 photos").click();
    h.run();
    assert_eq!(fake.0.lock().unwrap().imports, 1);
    assert!(matches!(h.state().stage, Stage::Importing(_)));
    fake.0.lock().unwrap().progress =
        Progress { phase: Phase::Copying, done: 3, total: 12, bytes_done: 100 << 20, bytes_total: 288 << 20, current: "P3.ARW".into() };
    h.run();
    h.get_by_label("Copying the photos");
    h.get_by_label("Copying photo 4 of 12 (100.0 MiB of 288.0 MiB)");
    h.get_by_label("Stop");
}

#[test]
fn nothing_new_too_little_space_and_an_unreachable_library_are_said_plainly() {
    let fake = Fake::default();
    let mut p = panel(&fake);
    p.stage = Stage::Scanning;
    fake.0.lock().unwrap().events.push(Event::Scanned(Ok(scan(0, 40, 0))));
    let mut h = harness(p);
    h.run();
    h.get_by_label("No new photos");
    h.get_by_label("40 already in the library or on this PC");
    assert!(h.query_by_label_contains("Import 0").is_none() && h.query_by_label("Import 0 photos").is_none());

    let mut tight = scan(3, 0, 0);
    tight.free = Some(100 << 20);
    tight.library_checked = false;
    let mut p = panel(&fake);
    p.stage = Stage::Scanning;
    fake.0.lock().unwrap().events.push(Event::Scanned(Ok(tight)));
    let mut h = harness(p);
    h.run();
    h.get_by_label_contains("Not enough free space");
    h.get_by_label_contains("The cloud library could not be reached");
    h.get_by_label("Import 3 photos").click();
    h.run();
    assert_eq!(fake.0.lock().unwrap().imports, 0, "too little room: the import cannot start");
}

#[test]
fn the_summary_says_what_was_done_and_the_chosen_folder_is_handed_to_the_window() {
    let fake = Fake::default();
    let mut p = panel(&fake);
    p.stage = Stage::Importing(Box::new(scan(5, 0, 0)));
    let run = ImportRun {
        dest: "261101 - Lisbon".into(),
        copied: 4,
        resumed: 1,
        renamed: vec![("IMG_1.jpg".into(), "IMG_1-2.jpg".into())],
        failed: vec!["P9.ARW: the card gave different bytes than the scan saw".into()],
        ..ImportRun::default()
    };
    fake.0.lock().unwrap().events.push(Event::Imported(Ok(run), Some("261101 - Lisbon".into())));
    let mut h = harness(p);
    h.run();
    h.get_by_label("Imported 5 photos into \"261101 - Lisbon\".");
    h.get_by_label_contains("1 photo had been copied by an earlier try");
    h.get_by_label_contains("1 photo got another name");
    h.get_by_label_contains("Not imported: P9.ARW");
    h.get_by_label_contains("sent to the home PC in the background");
    h.get_by_label("LightCraft is told about them too.");
    assert!(h.query_by_label_contains("add the folder in LightCraft yourself").is_none());
    h.get_by_label_contains("This folder is now one this PC keeps");
    assert_eq!(h.state_mut().take_chosen(), ["261101 - Lisbon"]);
    assert!(h.state_mut().take_chosen().is_empty());
    h.get_by_label("Import more").click();
    h.run();
    assert!(matches!(h.state().stage, Stage::Choose));
}

#[test]
fn a_stopped_import_says_how_many_are_left() {
    let fake = Fake::default();
    let mut p = panel(&fake);
    p.stage = Stage::Importing(Box::new(scan(10, 0, 0)));
    let run = ImportRun { dest: "X".into(), copied: 4, cancelled: true, left: 6, ..ImportRun::default() };
    fake.0.lock().unwrap().events.push(Event::Imported(Ok(run), None));
    let mut h = harness(p);
    h.run();
    h.get_by_label("Stopped");
    h.get_by_label_contains("Stopped after 4 photos into \"X\". 6 photos left");
}

// ------------------------------------------------------------------ the Status panel's states

fn now() -> SystemTime {
    parse_rfc3339("2026-11-01T12:00:00Z").unwrap()
}

fn status_harness<'a>(s: AppStatus) -> Harness<'a, AppStatus> {
    Harness::new_ui_state(move |ui, s: &mut AppStatus| status::show(ui, &StatusView { status: Some(s), running: true, now: now() }), s)
}

fn settled_other() -> AppStatus {
    AppStatus {
        configured: true,
        other: Some(OtherView { photos: 321, received: 400, ..OtherView::default() }),
        next_sync: Some("2026-11-01T12:12:00Z".into()),
        last_sync: Some(LastSync { when: "2026-11-01T11:55:00Z".into(), ok: true, result: "ok".into(), photos: 321, applied: 0, merged: 0, kept: 0 }),
        ..AppStatus::default()
    }
}

#[test]
fn the_state_line_is_green_up_to_date_only_when_nothing_waits_and_nothing_is_wrong() {
    use lightcraft_cloud_desktop::appstate::{HealthKind, Tone};
    let mut h = status_harness(settled_other());
    h.run();
    h.get_by_label("Up to date");
    h.get_by_label("nothing waiting");
    assert_eq!(status::tone_color(Tone::Ok), lightcraft_cloud_desktop::ui::theme::CHARCOAL.ok);
    // each other state, its own words and colour
    let cases: Vec<(&str, Box<dyn Fn(&mut AppStatus)>, &str, HealthKind)> = vec![
        ("waiting to download", Box::new(|s| s.other.as_mut().unwrap().waiting_to_download = 12), "12 photos waiting to download", HealthKind::Waiting),
        ("uploads queued", Box::new(|s| { let o = s.other.as_mut().unwrap(); o.uploads_waiting = 12; o.uploads_bytes = 300 << 20; }), "12 photos waiting to upload", HealthKind::Waiting),
        ("sent, not answered", Box::new(|s| s.other.as_mut().unwrap().awaiting_home = 3), "3 photos sent, waiting for the home PC", HealthKind::Waiting),
        ("a problem", Box::new(|s| s.active_problems = vec!["IMG_1.jpg is already on this PC with different contents".into()]), "1 problem needs attention", HealthKind::Problem),
        ("offline", Box::new(|s| s.offline = true), "Offline: will try again", HealthKind::Offline),
        ("uploading", Box::new(|s| s.activity = Activity::Uploading { since: "2026-11-01T12:00:00Z".into() }), "Sending photos to the home PC", HealthKind::Uploading),
    ];
    let mut colours = std::collections::HashSet::new();
    for (what, change, text, kind) in cases {
        let mut s = settled_other();
        change(&mut s);
        assert_eq!(s.health().kind, kind, "{what}");
        colours.insert(format!("{kind:?}/{:?}", status::tone_color(s.health().tone)));
        let mut h = status_harness(s);
        h.run();
        h.get_by_label(text);
        assert!(h.query_by_label("Up to date").is_none(), "{what}: must not read as up to date");
    }
    assert_eq!(colours.len(), 4, "waiting, a problem, offline and uploading each have their own state and colour: {colours:?}");
    let distinct: std::collections::HashSet<&str> = colours.iter().map(|c| c.split_once('/').unwrap().1).collect();
    assert_eq!(distinct.len(), 4, "and the colours differ from each other: {colours:?}");
    assert!(!distinct.contains(format!("{:?}", status::tone_color(Tone::Ok)).as_str()), "none is the green of Up to date");
    // the queue row
    let mut s = settled_other();
    let o = s.other.as_mut().unwrap();
    o.uploads_waiting = 12;
    o.uploads_bytes = 300 << 20;
    o.awaiting_home = 1;
    let mut h = status_harness(s);
    h.run();
    h.get_by_label("Imported photos");
    h.get_by_label("12 photos (300.0 MiB) waiting to upload; 1 photo sent, waiting for the home PC to add it");
}

#[test]
fn the_home_pc_does_not_read_as_all_done_with_an_import_stuck_or_photos_waiting_for_lightcraft() {
    let home = || AppStatus {
        configured: true,
        inbox_pending: Some(0),
        imports_pending: Some(0),
        last_sync: Some(LastSync { when: "2026-11-01T11:55:00Z".into(), ok: true, result: "ok".into(), photos: 10, applied: 0, merged: 0, kept: 0 }),
        ..AppStatus::default()
    };
    let mut h = status_harness(home());
    h.run();
    h.get_by_label("Up to date");
    let mut s = home();
    s.imports_pending = Some(2);
    let mut h = status_harness(s);
    h.run();
    h.get_by_label("2 photos from other PCs waiting to be added");
    assert!(h.query_by_label("Up to date").is_none());
    let mut s = home();
    s.lightcraft_port = 7980;
    s.lightcraft_waiting = 3;
    let mut h = status_harness(s);
    h.run();
    h.get_by_label("3 photos waiting for LightCraft");
    assert!(h.query_by_label("Up to date").is_none());
    let mut s = home();
    s.lightcraft_port = 7980;
    s.lightcraft_given_up = 1;
    let mut h = status_harness(s);
    h.run();
    h.get_by_label("1 problem needs attention");
}

#[test]
fn with_the_lightcraft_link_off_the_summary_says_to_add_the_folder_by_hand() {
    let fake = Fake::default();
    fake.0.lock().unwrap().link_off = true;
    let mut p = panel(&fake);
    p.stage = Stage::Importing(Box::new(scan(2, 0, 0)));
    let run = ImportRun { dest: "261101 - Lisbon".into(), copied: 2, ..ImportRun::default() };
    fake.0.lock().unwrap().events.push(Event::Imported(Ok(run), None));
    let mut h = harness(p);
    h.run();
    h.get_by_label_contains("LightCraft is not told about them");
    h.get_by_label_contains("add the folder in LightCraft yourself");
    assert!(h.query_by_label("LightCraft is told about them too.").is_none());
}

#[test]
fn refused_photos_keep_the_state_off_green_show_their_reasons_and_offer_two_ways_out() {
    use lightcraft_cloud_desktop::appstate::HealthKind;
    use lightcraft_cloud_desktop::ui::status::{StatusAction, show_with_actions};
    let mut s = settled_other();
    s.other.as_mut().unwrap().rejected = 2;
    assert_eq!(s.health().kind, HealthKind::Problem);
    assert_eq!(s.health().text, "2 photos the home PC refused");
    s.other.as_mut().unwrap().rejected = 1;
    assert_eq!(s.health().text, "1 photo the home PC refused");
    let refused = vec![("DSC1.ARW".to_string(), "the downloaded DSC1.ARW does not match its note".to_string())];
    let chosen = std::cell::Cell::new(None);
    let mut h = Harness::new_ui_state(
        |ui, s: &mut AppStatus| {
            if let Some(a) = show_with_actions(ui, &StatusView { status: Some(s), running: true, now: now() }, &refused) {
                chosen.set(Some(a));
            }
        },
        s.clone(),
    );
    h.run();
    h.get_by_label("1 photo the home PC refused"); // the state line
    h.get_by_label("Refused by the home PC");
    h.get_by_label("1 photo, still on this PC");
    h.get_by_label_contains("DSC1.ARW: the downloaded DSC1.ARW does not match its note");
    assert!(h.query_by_label("Up to date").is_none());
    h.get_by_label("Try again").click();
    h.run();
    assert_eq!(chosen.take(), Some(StatusAction::TryAgain));
    h.get_by_label("Keep on this PC only").click();
    h.run();
    assert_eq!(chosen.take(), Some(StatusAction::KeepHere));
    // nothing refused: no row, no buttons, green again
    let mut h = status_harness(settled_other());
    h.run();
    assert!(h.query_by_label("Try again").is_none() && h.query_by_label("Refused by the home PC").is_none());
    h.get_by_label("Up to date");
}
