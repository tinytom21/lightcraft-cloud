//! The "Add a device" panel, rendered headlessly. Synthetic configs with fake values only.

use std::path::PathBuf;

use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use lightcraft_cloud_client::pairing;
use lightcraft_cloud_desktop::ui::add_device::{AddDevicePanel, qr_image};

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-add-device-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn conf(d: &std::path::Path) -> PathBuf {
    let p = d.join("rclone.conf");
    std::fs::write(
        &p,
        "[gdrive]\ntype = drive\nclient_id = FAKEID\nclient_secret = FAKESECRET\nscope = drive\n\
         token = {\"access_token\":\"FAKEACCESS\",\"token_type\":\"Bearer\",\"refresh_token\":\"FAKEREFRESH\",\"expiry\":\"2026-01-01T00:00:00Z\"}\n\n\
         [elsewhere]\ntype = s3\nsecret_access_key = OTHERFAKESECRET\n\n\
         [keyed]\ntype = drive\nservice_account_file = /home/x/sa.json\n",
    )
    .unwrap();
    p
}

fn harness<'a>(p: AddDevicePanel) -> Harness<'a, AddDevicePanel> {
    Harness::builder().with_size(egui::vec2(700.0, 1300.0)).build_ui_state(|ui, p: &mut AddDevicePanel| p.show(ui), p)
}

const CODE: &str = "0123-4567-897";

#[test]
fn it_warns_first_then_asks_for_the_code_and_shows_only_a_qr_and_a_text_that_opens_with_it() {
    let d = temp("a");
    let mut h = harness(AddDevicePanel::new("gdrive:LightCraft Cloud".into(), conf(&d)));
    h.run();
    h.get_by_label("Add a device");
    h.get_by_label_contains("This gives the device access to your cloud storage account, using the same sign-in as this PC");
    h.get_by_label_contains("The device shows a code. Type it here.");
    h.get_by_label_contains("Do not share your screen");
    h.get_by_label("Only tested with Google Drive.");
    assert!(!h.state().is_showing());
    assert!(h.query_by_label_contains("Copy text for another PC").is_none());
    // no valid code: the button is disabled, and a typo is named
    assert!(h.get_by_label("Show pairing QR").accesskit_node().is_disabled());
    h.state_mut().code = "0123-4567-898".into();
    h.run();
    h.get_by_label("That code has a typo; check it against the device.");
    assert!(h.get_by_label("Show pairing QR").accesskit_node().is_disabled());
    h.state_mut().code = CODE.into();
    h.run();
    assert!(h.query_by_label_contains("typo").is_none());
    h.get_by_label("Show pairing QR").click();
    h.run();
    assert!(h.state().is_showing(), "{:?}", h.state().refusal());
    let text = h.state().pairing().unwrap().to_string();
    h.get_by_label("Copy text for another PC");
    // no code is displayed anywhere, and the field is gone
    assert!(h.query_by_label_contains(CODE).is_none() && h.query_by_label_contains("01234567897").is_none());
    assert!(h.query_by_label("Code from the device").is_none());
    let bundle = pairing::open(&text, CODE).unwrap();
    assert_eq!(bundle.remote, "gdrive:LightCraft Cloud");
    assert!(bundle.config.contains("FAKEREFRESH") && bundle.config.contains("FAKEACCESS"), "the token travels unchanged");
    assert!(!bundle.config.contains("OTHERFAKESECRET") && !bundle.config.contains("keyed"));
    // the pairing is not in the Debug output
    let dbg = format!("{:?}", h.state());
    assert!(!dbg.contains(CODE) && !dbg.contains(&text[..30]), "{dbg}");
    // Hide wipes it, the code too
    h.get_by_label("Hide").click();
    h.run();
    assert!(!h.state().is_showing());
    assert!(h.state().code.is_empty());
    h.get_by_label("Show pairing QR");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn copying_puts_the_text_on_the_clipboard() {
    let d = temp("c");
    let mut p = AddDevicePanel::new("gdrive:x".into(), conf(&d));
    p.code = CODE.into();
    let mut h = harness(p);
    h.run();
    h.get_by_label("Show pairing QR").click();
    h.run();
    let text = h.state().pairing().unwrap().to_string();
    h.get_by_label("Copy text for another PC").click();
    // (the commands of a frame are in that frame's output only, so look after every step)
    let mut copied = None;
    for _ in 0..4 {
        h.step();
        copied = copied.or(h.output().platform_output.commands.iter().find_map(|c| match c {
            egui::OutputCommand::CopyText(t) => Some(t.clone()),
            _ => None,
        }));
    }
    assert_eq!(copied.as_deref(), Some(text.as_str()));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn remotes_that_cannot_travel_show_the_reason_and_no_code() {
    let d = temp("b");
    for (remote, expect) in [
        ("/home/me/Photos/LightCraft Cloud", "folder on this PC"),
        ("keyed:x", "service_account_file"),
        ("missing:x", "no remote of that name"),
    ] {
        let mut p = AddDevicePanel::new(remote.into(), conf(&d));
        p.code = CODE.into();
        let mut h = harness(p);
        h.run();
        h.get_by_label("Show pairing QR").click();
        h.run();
        assert!(!h.state().is_showing());
        h.get_by_label_contains(expect);
    }
    // an unreadable config
    let mut p = AddDevicePanel::new("gdrive:x".into(), d.join("nope.conf"));
    p.code = CODE.into();
    let mut h = harness(p);
    h.run();
    h.get_by_label("Show pairing QR").click();
    h.run();
    h.get_by_label_contains("Could not read the rclone config");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_qr_is_black_on_white_with_a_quiet_zone() {
    let text = pairing::seal(&pairing::Bundle::new("gdrive:x", "[gdrive]\ntype = drive\n"), CODE).unwrap();
    let img = qr_image(&text).unwrap();
    let [w, h] = img.size;
    assert_eq!(w, h);
    let px = |x: usize, y: usize| img.pixels[y * w + x];
    // the four corner modules' worth of border is white
    for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1), (w / 2, 3 * 8)] {
        assert_eq!(px(x, y), egui::Color32::WHITE);
    }
    // the first finder pattern starts right after the quiet zone: a dark module at (4,4)
    assert_eq!(px(4 * 8, 4 * 8), egui::Color32::BLACK);
    assert!(img.pixels.iter().all(|c| *c == egui::Color32::WHITE || *c == egui::Color32::BLACK));
    assert!(w >= 21 * 8, "{w}");
}
