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
