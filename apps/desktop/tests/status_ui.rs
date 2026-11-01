//! The Status panel's states on another PC, headless: what each one says, its colour, and the buttons for refused photos and for the
//! photos that were already in the folder. Both builds run this.

use std::time::SystemTime;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use lightcraft_cloud_desktop::appstate::{Activity, AppStatus, LastSync, OtherView};
use lightcraft_cloud_desktop::clock::parse_rfc3339;
use lightcraft_cloud_desktop::ui::status::{self, StatusView};

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
fn while_photos_download_the_rows_agree_with_the_progress_line() {
    use lightcraft_cloud_client::{Progress, Stage};
    let mut s = settled_other();
    let before = s.other.as_ref().unwrap().photos;
    s.other.as_mut().unwrap().waiting_to_download = 0; // (saved before this pass: the pass found 33 more)
    s.activity = Activity::Syncing {
        since: "2026-11-01T12:00:00Z".into(),
        reason: "timer".into(),
        progress: Some(Progress { stage: Stage::Downloading, done_items: 27, total_items: 60, ..Default::default() }),
    };
    let mut h = status_harness(s);
    h.run();
    h.get_by_label("33 photos");
    h.get_by_label(&(before + 27).to_string());
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
        ("uploading", Box::new(|s| s.activity = Activity::Uploading { since: "2026-11-01T12:00:00Z".into(), progress: None }), "Sending photos to the home PC", HealthKind::Uploading),
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

#[test]
fn photos_that_were_here_at_the_start_are_counted_with_a_button_to_send_them() {
    use lightcraft_cloud_desktop::ui::status::{StatusAction, show_with_actions};
    let mut s = settled_other();
    s.other.as_mut().unwrap().held = 12;
    // informational: nothing is wrong, so the state stays green
    assert_eq!(s.health().text, "Up to date");
    let chosen = std::cell::Cell::new(None);
    let mut h = Harness::new_ui_state(
        |ui, s: &mut AppStatus| {
            if let Some(a) = show_with_actions(ui, &StatusView { status: Some(s), running: true, now: now() }, &[]) {
                chosen.set(Some(a));
            }
        },
        s.clone(),
    );
    h.run();
    h.get_by_label("12 photos here aren't in the cloud library");
    h.get_by_label_contains("not sent by themselves");
    h.get_by_label("Send them").click();
    h.run();
    assert_eq!(chosen.take(), Some(StatusAction::SendHeld));
    s.other.as_mut().unwrap().held = 1;
    s.other.as_mut().unwrap().held_unchecked = 30;
    let mut h = status_harness(s);
    h.run();
    h.get_by_label("1 photo here isn't in the cloud library (still looking at 30 more)");
    let mut h = status_harness(settled_other());
    h.run();
    assert!(h.query_by_label("Send them").is_none());
}

// ------------------------------------------------------------------ live progress

fn now_ms() -> u64 {
    lightcraft_cloud_client::to_ms(now())
}

fn syncing(progress: Option<lightcraft_cloud_client::Progress>) -> AppStatus {
    let mut s = settled_other();
    s.activity = Activity::Syncing { since: "2026-11-01T11:50:00Z".into(), reason: "timer".into(), progress };
    s
}

fn download(done: u64, total: u64, done_b: u64, total_b: u64) -> lightcraft_cloud_client::Progress {
    use lightcraft_cloud_client::{Progress, Stage};
    Progress {
        stage: Stage::Downloading,
        done_items: done,
        total_items: total,
        done_bytes: done_b,
        total_bytes: total_b,
        started_ms: now_ms() - 120_000,
        last_item_ms: now_ms() - 3_000,
        finish_ms: Some(now_ms() + 12 * 60_000),
        ..Progress::default()
    }
}

#[test]
fn a_download_shows_its_line_with_counts_sizes_and_time_left_and_a_bar_at_the_right_fill() {
    let gb = 1u64 << 30;
    let mut h = status_harness(syncing(Some(download(34, 261, gb + gb / 5, 8 * gb))));
    h.run();
    h.get_by_label("Syncing now (timer)"); // the state line is as it was
    h.get_by_label("Downloading 34 of 261 photos · 1.2 of 8.0 GB · about 12 minutes left");
    h.get_by_label("Progress 15%");
    assert!(h.query_by_label_contains("No progress for").is_none(), "moving: no note");
    // the same on the home PC (no other-PC rows), with an upload
    use lightcraft_cloud_client::{Progress, Stage};
    let mut s = syncing(Some(Progress {
        stage: Stage::UploadingOriginals,
        done_items: 5,
        total_items: 30,
        done_bytes: 200 << 20,
        total_bytes: 1_181_116_006,
        started_ms: now_ms() - 60_000,
        last_item_ms: now_ms() - 1_000,
        finish_ms: Some(now_ms() + 3 * 60_000),
        ..Progress::default()
    }));
    s.other = None;
    let mut h = status_harness(s);
    h.run();
    h.get_by_label("Uploading originals 5 of 30 · 200.0 MB of 1.1 GB · about 3 minutes left");
    h.get_by_label("Progress 17%");
}

#[test]
fn a_stage_without_a_count_is_only_named_and_its_bar_says_it_is_busy() {
    use lightcraft_cloud_client::{Progress, Stage};
    let p = Progress { stage: Stage::LookingAtLibrary, started_ms: now_ms() - 5_000, ..Progress::default() };
    let mut h = status_harness(syncing(Some(p)));
    h.run();
    h.get_by_label("Looking at the cloud library…");
    h.get_by_label("Progress, working");
    // a job that has not reported yet
    let mut h = status_harness(syncing(None));
    h.run();
    h.get_by_label("Getting started…");
    // an upload job
    let mut s = settled_other();
    s.activity = Activity::Uploading {
        since: "2026-11-01T11:59:00Z".into(),
        progress: Some(Progress { stage: Stage::Sending, done_items: 3, total_items: 12, started_ms: now_ms() - 2_000, last_item_ms: now_ms() - 500, ..Progress::default() }),
    };
    let mut h = status_harness(s);
    h.run();
    h.get_by_label("Sending photos to the home PC"); // the state line
    h.get_by_label("Sending 3 of 12 photos to the home PC");
    h.get_by_label("Progress 25%");
}

#[test]
fn two_minutes_without_anything_finishing_is_a_calm_note_and_not_an_error() {
    let mut p = download(34, 261, 100, 1000);
    p.finish_ms = None;
    p.started_ms = now_ms() - 30 * 60_000;
    p.last_item_ms = now_ms() - 3 * 60_000 - 5_000;
    let mut h = status_harness(syncing(Some(p)));
    h.run();
    h.get_by_label("No progress for 3 minutes: still trying, the network may be slow or down");
    h.get_by_label("Downloading 34 of 261 photos · 100 of 1000 bytes");
    // the state line is still the busy one; no problem is raised
    h.get_by_label("Syncing now (timer)");
    assert!(h.query_by_label_contains("problem").is_none());
    // one second short of two minutes: no note
    p.last_item_ms = now_ms() - 119_000;
    let mut h = status_harness(syncing(Some(p)));
    h.run();
    assert!(h.query_by_label_contains("No progress").is_none());
    // exactly two minutes: the note
    p.last_item_ms = now_ms() - 120_000;
    let mut h = status_harness(syncing(Some(p)));
    h.run();
    h.get_by_label("No progress for 2 minutes: still trying, the network may be slow or down");
}

#[test]
fn nothing_about_progress_shows_when_idle_or_when_the_app_is_not_running() {
    let mut h = status_harness(settled_other());
    h.run();
    assert!(h.query_by_label_contains("Progress").is_none() && h.query_by_label_contains("Getting started").is_none());
    // a status left behind by an app that was stopped mid-job must not claim a time
    let s = syncing(Some(download(34, 261, 1, 2)));
    let mut h = Harness::new_ui_state(|ui, s: &mut AppStatus| status::show(ui, &StatusView { status: Some(s), running: false, now: now() }), s);
    h.run();
    assert!(h.query_by_label_contains("Downloading 34").is_none() && h.query_by_label("Progress 50%").is_none());
    h.get_by_label_contains("not running");
}

#[test]
fn the_bars_fill_is_exact_never_past_the_track_and_a_begun_job_shows_a_sliver() {
    assert_eq!(status::fill_width(0.0, 400.0), 0.0);
    assert_eq!(status::fill_width(0.5, 400.0), 200.0);
    assert_eq!(status::fill_width(1.0, 400.0), 400.0);
    assert_eq!(status::fill_width(7.0, 400.0), 400.0);
    assert_eq!(status::fill_width(-1.0, 400.0), 0.0);
    assert_eq!(status::fill_width(0.001, 400.0), 8.0, "a sliver, so it is not mistaken for nothing");
    assert_eq!(status::fill_width(0.5, 3.0), 3.0, "and not wider than a tiny track");
    // the moving piece of a bar that cannot say how far it is stays inside the track
    for i in 0..=20 {
        let (a, b) = status::busy_span(i as f32 / 20.0, 300.0);
        assert!(0.0 <= a && a <= b && b <= 300.0, "{i}: {a} {b}");
    }
    assert_eq!(status::busy_span(0.5, 300.0).1 - status::busy_span(0.5, 300.0).0, 100.0);
}

#[test]
fn the_tray_tooltip_is_the_same_line_and_fits_where_tooltips_are_short() {
    let s = syncing(Some(download(34, 261, 1 << 30, 8 << 30)));
    assert_eq!(s.tooltip(now()), "LightCraft Cloud: Downloading 34 of 261 photos · 1.0 of 8.0 GB · about 12 minutes left");
    assert_eq!(settled_other().tooltip(now()), "LightCraft Cloud: up to date");
    let mut p = download(34, 261, 1 << 30, 8 << 30);
    p.last_item_ms = now_ms() - 10 * 60_000;
    let long = syncing(Some(p)).tooltip(now());
    assert!(long.chars().count() <= lightcraft_cloud_desktop::appstate::TOOLTIP_MAX, "{long}");
    assert!(long.starts_with("LightCraft Cloud: Downloading 34 of 261 photos") && long.ends_with('…'), "{long}");
}
