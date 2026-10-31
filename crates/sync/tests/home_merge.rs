//! The sidecar merge and the inbox's pure rules: ports of the Python tests `SidecarMerge`, `Conflict`, `Inbox`.

mod common;

use common::*;
use lightcraft_cloud_client::home::{decide, merge_sidecars, notify_message, parse_inbox_name, phone_is_newer, validate_xmp};
use serde_json::{Value, json};

fn merge(base: &[u8], pc: &[u8], phone: &[u8], newer: bool) -> (Vec<u8>, Vec<String>) {
    let r = merge_sidecars(base, pc, phone, newer).expect("the documents should merge");
    assert_eq!(validate_xmp(&r.0), None);
    r
}

fn m(base: &[u8], pc: &[u8], phone: &[u8]) -> (Vec<u8>, Vec<String>) {
    merge(base, pc, phone, true)
}

fn try_merge(base: &[u8], pc: &[u8], phone: &[u8]) -> Option<(Vec<u8>, Vec<String>)> {
    merge_sidecars(base, pc, phone, true)
}

const ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn conflict_decisions_all_branches() {
    let d = |b: &str, c: Option<&str>, i: f64, l: Option<f64>, h: Option<&str>, can: bool| decide(b, c, i, l, h, can).0;
    assert_eq!(d("none", None, 100., None, None, false), "apply"); // no sidecar either side
    assert_eq!(d("abc", Some("abc"), 100., Some(500.), None, false), "apply"); // base matches (even if mtime newer)
    assert_eq!(d("abc", Some("abc"), 100., Some(500.), Some("abc"), false), "noop");
    assert_eq!(d("abc", Some("zzz"), 200., Some(100.), None, false), "apply"); // conflict, phone newer
    assert_eq!(d("abc", Some("zzz"), 100., Some(200.), None, false), "keep"); // conflict, PC newer
    assert_eq!(d("none", Some("zzz"), 200., Some(100.), None, false), "apply");
    assert_eq!(d("none", Some("zzz"), 100., Some(200.), None, false), "keep");
    assert_eq!(d("abc", None, 100., None, None, false), "apply"); // PC sidecar vanished
    assert_eq!(d("abc", Some("zzz"), 100., Some(100.), None, false), "keep"); // tie: PC wins
}

#[test]
fn merge_verdict_only_on_conflict() {
    let d = |b: &str, c: Option<&str>, i: f64, l: Option<f64>, h: Option<&str>| decide(b, c, i, l, h, true).0;
    assert_eq!(d("abc", Some("zzz"), 100., Some(500.), None), "merge");
    assert_eq!(d("abc", Some("zzz"), 900., Some(500.), None), "merge");
    assert_eq!(d("abc", Some("abc"), 100., Some(500.), None), "apply");
    assert_eq!(d("abc", Some("zzz"), 100., Some(500.), Some("zzz")), "noop");
    assert_eq!(d("abc", None, 100., None, None), "apply");
    assert_eq!(d("none", None, 100., None, None), "apply");
    assert!(phone_is_newer(2., Some(1.)) && phone_is_newer(1., None));
    assert!(!(phone_is_newer(1., Some(1.)) || phone_is_newer(1., Some(2.)))); // a tie goes to the PC
}

#[test]
fn notify_messages() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(
        notify_message(1, &s(&["2024/Trip"]), "Photos", 0),
        "1 phone edit applied in 2024/Trip \u{2014} in LightCraft run Synchronize Folder on this folder"
    );
    assert_eq!(
        notify_message(2, &s(&[".", "B"]), "Photos", 0),
        "2 phone edits applied in Photos, B \u{2014} in LightCraft run Synchronize Folder on these folders"
    );
    assert!(notify_message(4, &s(&["A", "B", "C", "D"]), "Photos", 0).contains("A, B, C and 1 more"));
    assert_eq!(notify_message(3, &[], "Photos", 0), "3 phone edits applied \u{2014} in LightCraft run Synchronize Folder");
    assert_eq!(
        notify_message(2, &s(&["X"]), "Photos", 1),
        "2 phone edits applied (1 merged with PC changes) in X \u{2014} in LightCraft run Synchronize Folder on this folder"
    );
    assert_eq!(notify_message(1, &[], "Photos", 1), "1 phone edit applied (1 merged with PC changes) \u{2014} in LightCraft run Synchronize Folder");
}

#[test]
fn inbox_names() {
    let p = parse_inbox_name(&format!("phone/{ID}-20261027T022310Z-{}.xmp", "b".repeat(16))).unwrap();
    assert_eq!((p.device.as_str(), p.id.as_str(), p.stamp.as_str(), p.base.as_str()), ("phone", ID, "20261027T022310Z", "bbbbbbbbbbbbbbbb"));
    assert_eq!(parse_inbox_name(&format!("p/{ID}-20261027T022310Z-none.xmp")).unwrap().base, "none");
    for bad in [
        "x.xmp".to_string(),
        format!("p/{ID}-20261027T022310Z.xmp"),
        format!("p/q/{ID}-20261027T022310Z-none.xmp"),
        format!("p/{}-20261027T022310Z-none.xmp", &ID[..31]),
        format!("p/{ID}-20261327T022310Z-none.xmp"),
        format!("p/{ID}-20261027T022310Z-XYZ.xmp"),
        format!("p/{ID}-20261027T022310Z-none.jpg"),
        format!("../{ID}-20261027T022310Z-none.xmp"),
    ] {
        assert!(parse_inbox_name(&bad).is_none(), "{bad}");
    }
}

#[test]
fn validate_inbox_files() {
    let ok = br#"<x:xmpmeta xmlns:x="a" xmlns:lc="b"><lc:settings>{}</lc:settings></x:xmpmeta>"#;
    assert_eq!(validate_xmp(ok), None);
    assert_eq!(validate_xmp(br#"<r xmlns:crs="c" crs:Exposure2012="1"/>"#), None);
    assert!(validate_xmp(b"<a><lc:settings></a>").unwrap().contains("well-formed"));
    assert!(validate_xmp(b"<a/>").unwrap().contains("develop"));
    let mut big = ok.to_vec();
    big.extend(vec![b' '; 2 * 1024 * 1024]);
    assert!(validate_xmp(&big).unwrap().contains("large"));
    assert!(validate_xmp(br#"<!DOCTYPE a [<!ENTITY e "x">]><a>lc:settings</a>"#).unwrap().contains("DTD"));
}

// ------------------------------------------------------------------ the merge

#[test]
fn disjoint_changes_in_one_section_both_kept() {
    let b = sc().build();
    let pc = sc().s(&[("light.exposure", j(0.7))]).build();
    let ph = sc().s(&[("light.contrast", j(25.0)), ("color.vibrance", j(30.0))]).build();
    for newer in [true, false] {
        let (out, conflicts) = merge(&b, &pc, &ph, newer);
        assert!(conflicts.is_empty());
        assert_eq!(settings_of(&out), with_settings(&[("light.exposure", j(0.7)), ("light.contrast", j(25.0)), ("color.vibrance", j(30.0))]));
    }
}

#[test]
fn same_leaf_newer_side_wins_and_is_reported() {
    let b = sc().build();
    let pc = sc().s(&[("light.exposure", j(0.7)), ("wb.temp", j(5000.0))]).build();
    let ph = sc().s(&[("light.exposure", j(-0.3)), ("color.vibrance", j(9.0))]).build();
    let (out, conflicts) = merge(&b, &pc, &ph, true);
    assert_eq!(conflicts, ["lc:settings/light/exposure"]);
    assert_eq!(settings_of(&out), with_settings(&[("light.exposure", j(-0.3)), ("wb.temp", j(5000.0)), ("color.vibrance", j(9.0))]));
    let (out, conflicts) = merge(&b, &pc, &ph, false);
    assert_eq!(conflicts, ["lc:settings/light/exposure"]);
    assert_eq!(settings_of(&out)["light"]["exposure"], json!(0.7));
    assert_eq!(settings_of(&out)["color"]["vibrance"], json!(9.0));
}

#[test]
fn equal_change_on_both_sides_is_no_conflict() {
    let both = with_settings(&[("light.exposure", j(0.5))]);
    let (out, conflicts) = m(&sc().build(), &Sc { settings: both.clone(), ..sc() }.rating(2).build(), &Sc { settings: both.clone(), ..sc() }.build());
    assert!(conflicts.is_empty());
    assert_eq!((settings_of(&out), text_of(&out, "xmp:Rating").as_deref()), (both, Some("2")));
}

#[test]
fn atomic_sections_are_taken_whole() {
    let edits: [(&str, [(&str, Value); 2], [(&str, Value); 2]); 5] = [
        ("crop", [("crop.geometry.angle", j(5.0)), ("crop.aspect", json!("3:2"))], [("crop.flip_h", json!(true)), ("crop.flip_v", json!(true))]),
        ("geometry", [("geometry.rotate", j(2.0)), ("geometry.scale", j(90.0))], [("geometry.vertical", j(3.0)), ("geometry.horizontal", j(4.0))]),
        ("curve", [("curve.lights", j(10.0)), ("curve.darks", j(-10.0))], [("curve.shadows", j(7.0)), ("curve.master", json!([[0, 0], [1, 1]]))]),
        (
            "profile",
            [("profile.amount", j(50.0)), ("profile.id", json!("lc.mono"))],
            [("profile.amount", j(80.0)), ("profile.id", json!("lc.sepia"))],
        ),
        ("wb", [("wb.temp", json!(5200)), ("wb.tint", json!(12))], [("wb.temp", json!(7000)), ("wb.mode", json!("custom"))]),
    ];
    for (section, a, b_) in edits {
        let base = sc().build();
        let (pc, ph) = (sc().s(&a).build(), sc().s(&b_).build());
        let (out, conflicts) = merge(&base, &pc, &ph, true);
        assert_eq!(conflicts, [format!("lc:settings/{section}")], "{section}"); // not a leaf-by-leaf mix
        assert_eq!(settings_of(&out)[section], with_settings(&b_)[section], "{section}");
        let (out, _) = merge(&base, &pc, &ph, false);
        assert_eq!(settings_of(&out)[section], with_settings(&a)[section], "{section}");
    }
}

#[test]
fn atomic_section_changed_by_one_side_only() {
    let pc = sc().s(&[("crop.geometry.angle", j(5.0))]).build();
    let ph = sc().s(&[("light.exposure", j(1.0))]).build();
    let (out, conflicts) = m(&sc().build(), &pc, &ph);
    assert!(conflicts.is_empty());
    assert_eq!(settings_of(&out), with_settings(&[("crop.geometry.angle", j(5.0)), ("light.exposure", j(1.0))]));
}

#[test]
fn lists_are_atomic() {
    let mask = |n: u32| json!([{"id": n, "kind": "radial"}]);
    let both = json!([{"id": 1, "kind": "radial"}, {"id": 2, "kind": "radial"}]);
    let b = sc().build();
    let pc = sc().s(&[("masks", mask(1))]).build();
    let ph = sc().s(&[("masks", both.clone())]).build();
    let (out, conflicts) = merge(&b, &pc, &ph, false);
    assert_eq!(conflicts, ["lc:settings/masks"]);
    assert_eq!(settings_of(&out)["masks"], mask(1)); // no element-wise mixing
    let (out, _) = merge(&b, &pc, &ph, true);
    assert_eq!(settings_of(&out)["masks"], both);
    let (out, conflicts) = m(&b, &b, &ph); // changed on one side only
    assert_eq!((settings_of(&out)["masks"].clone(), conflicts), (both, vec![]));
}

#[test]
fn setting_deleted_on_one_side() {
    let mut gone = settings();
    gone["light"].as_object_mut().unwrap().remove("blacks");
    let gone_sc = Sc { settings: gone.clone(), ..sc() };
    let (out, conflicts) = m(&sc().build(), &sc().s(&[("light.exposure", j(1.0))]).build(), &gone_sc.build());
    assert!(conflicts.is_empty());
    assert!(settings_of(&out)["light"].get("blacks").is_none());
    assert_eq!(settings_of(&out)["light"]["exposure"], json!(1.0));
    let (out, conflicts) = m(&sc().build(), &gone_sc.build(), &sc().s(&[("color.vibrance", j(3.0))]).build()); // PC deleted it
    assert!(conflicts.is_empty());
    assert!(settings_of(&out)["light"].get("blacks").is_none());
    assert_eq!(settings_of(&out)["color"]["vibrance"], json!(3.0));
}

#[test]
fn property_deleted_on_one_side() {
    let label = "\n   <xmp:Label>Red</xmp:Label>";
    let b = sc().extra(label).build();
    let (out, conflicts) = m(&b, &sc().extra(label).rating(4).build(), &sc().build()); // phone deleted the label
    assert!(conflicts.is_empty());
    assert_eq!(text_of(&out, "xmp:Label"), None);
    assert_eq!(text_of(&out, "xmp:Rating").as_deref(), Some("4"));
    assert!(!contains(&out, "Label"));
    assert!(!contains(&out, "\n\n")); // the whole line went
    let (out, conflicts) = m(&b, &sc().build(), &sc().extra(label).rating(5).build()); // PC deleted it, phone kept it
    assert_eq!((conflicts, text_of(&out, "xmp:Label"), text_of(&out, "xmp:Rating").as_deref()), (vec![], None, Some("5")));
    let blue = label.replace("Red", "Blue");
    let (out, conflicts) = merge(&b, &sc().build(), &sc().extra(&blue).build(), true); // deleted vs changed
    assert_eq!((conflicts, text_of(&out, "xmp:Label").as_deref()), (vec!["xmp:Label".to_string()], Some("Blue")));
    let (out, conflicts) = merge(&b, &sc().build(), &sc().extra(&blue).build(), false);
    assert_eq!((conflicts, text_of(&out, "xmp:Label")), (vec!["xmp:Label".to_string()], None));
}

#[test]
fn property_added_on_one_side() {
    let (out, conflicts) = m(&sc().build(), &sc().rating(2).build(), &sc().extra("\n   <xmp:Label>Green</xmp:Label>").build());
    assert_eq!((conflicts, text_of(&out, "xmp:Label").as_deref(), text_of(&out, "xmp:Rating").as_deref()), (vec![], Some("Green"), Some("2")));
    assert!(contains(&out, "\n   <xmp:Label>Green</xmp:Label>")); // indented like its siblings
}

#[test]
fn rating_and_flag_on_different_sides() {
    let (out, conflicts) = m(&sc().build(), &sc().rating(4).build(), &sc().flag("pick").build());
    assert!(conflicts.is_empty());
    assert_eq!((text_of(&out, "xmp:Rating").as_deref(), text_of(&out, "lc:flag").as_deref()), (Some("4"), Some("pick")));
    let (out, conflicts) = merge(&sc().build(), &sc().rating(4).build(), &sc().rating(1).build(), false);
    assert_eq!((conflicts, text_of(&out, "xmp:Rating").as_deref()), (vec!["xmp:Rating".to_string()], Some("4")));
}

#[test]
fn housekeeping_comes_from_the_newer_side() {
    let extra = |d: &str| format!("\n   <xmp:ModifyDate>{d}</xmp:ModifyDate>");
    let b = sc().extra(&extra("2026-01-01")).tool("LightCraft 1").build();
    let pc = sc().extra(&extra("2026-02-01")).tool("LightCraft 2").build();
    let ph = sc().extra(&extra("2026-03-01")).tool("LightCraft Android").build();
    for (newer, want) in [(true, ("2026-03-01", "LightCraft Android")), (false, ("2026-02-01", "LightCraft 2"))] {
        let (out, conflicts) = merge(&b, &pc, &ph, newer);
        assert!(conflicts.is_empty());
        assert_eq!((text_of(&out, "xmp:ModifyDate").unwrap(), text_of(&out, "xmp:CreatorTool").unwrap()), (want.0.to_string(), want.1.to_string()));
    }
}

fn darktable(rating: u32, exposure: &str, contrast: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="XMP Core 4.4.0-Exiv2">
 <rdf:RDF xmlns:rdf="{NS_RDF}">
  <rdf:Description rdf:about=""
    xmlns:xmp="{NS_XMP}"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmlns:darktable="http://darktable.sf.net/"
    xmp:Rating="{rating}"
    crs:Exposure2012="{exposure}"
    crs:Contrast2012='{contrast}'
    darktable:xmp_version="5">
   <darktable:history>
    <rdf:Seq><rdf:li darktable:num="0" darktable:operation="exposure"/></rdf:Seq>
   </darktable:history>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#
    )
    .into_bytes()
}

#[test]
fn attribute_form_properties() {
    let (out, conflicts) = m(&darktable(1, "+0.00", "+0"), &darktable(3, "+0.50", "+0"), &darktable(1, "+0.00", "+25"));
    assert!(conflicts.is_empty());
    let want = darktable(3, "+0.50", "+25");
    assert_eq!(String::from_utf8(out.clone()).unwrap(), String::from_utf8(want).unwrap()); // only the values changed; layout, quotes and foreign data untouched
    assert!(contains(&out, "darktable:xmp_version=\"5\""));
    assert!(contains(&out, "crs:Contrast2012='+25'")); // quote style kept
    let (_, conflicts) = merge(&darktable(1, "+0.00", "+0"), &darktable(3, "+0.50", "+0"), &darktable(1, "+0.90", "+0"), true);
    assert_eq!(conflicts, ["crs:Exposure2012"]);
}

#[test]
fn attribute_and_element_forms_are_the_same_property() {
    let attr = |r: u32| {
        format!(r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="{NS_RDF}"><rdf:Description rdf:about="" xmlns:xmp="{NS_XMP}" xmlns:crs="c" xmp:Rating="{r}" crs:Exposure2012="1"/></rdf:RDF></x:xmpmeta>"#).into_bytes()
    };
    let elem = |r: u32| {
        format!(r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="{NS_RDF}"><rdf:Description rdf:about="" xmlns:xmp="{NS_XMP}" xmlns:crs="c"><xmp:Rating>{r}</xmp:Rating><crs:Exposure2012>1</crs:Exposure2012></rdf:Description></rdf:RDF></x:xmpmeta>"#).into_bytes()
    };
    let (out, conflicts) = m(&attr(1), &attr(1), &elem(5)); // PC writes attributes, phone elements
    assert!(conflicts.is_empty());
    assert!(contains(&out, "xmp:Rating=\"5\"")); // rewritten in the PC's form
    let (out, conflicts) = m(&elem(1), &elem(1), &attr(5));
    assert!(conflicts.is_empty());
    assert!(contains(&out, "<xmp:Rating>5</xmp:Rating>"));
    let (out, conflicts) = m(&attr(1), &attr(2), &elem(2)); // same value in different forms: no change
    assert_eq!((conflicts, out), (vec![], attr(2)));
}

#[test]
fn foreign_properties_comments_and_layout_preserved() {
    let foreign = "\n   <!-- kept -->\n   <darktable:history xmlns:darktable=\"http://darktable.sf.net/\"><rdf:Seq><rdf:li>exposure</rdf:li></rdf:Seq></darktable:history>";
    let pc = sc().s(&[("light.exposure", j(0.7))]).extra(foreign).decl().build();
    let (out, _) = m(&sc().build(), &pc, &sc().s(&[("color.vibrance", j(9.0))]).rating(2).build());
    assert!(contains(&out, foreign));
    let cut = |d: &[u8]| d[..d.windows(13).position(|w| w == b"<lc:settings>").unwrap()].to_vec();
    let want = String::from_utf8(cut(&pc)).unwrap().replace("<xmp:Rating>0<", "<xmp:Rating>2<");
    assert_eq!(String::from_utf8(cut(&out)).unwrap(), want); // everything before: byte for byte
}

#[test]
fn prefixes_xpacket_and_declaration_preserved() {
    let pc = sc().s(&[("light.exposure", j(0.7))]).decl().build();
    let (out, _) = m(&sc().build(), &pc, &sc().s(&[("color.vibrance", j(9.0))]).flag("pick").build());
    assert!(out.starts_with(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta".as_bytes()
    ));
    assert!(out.ends_with(b"</x:xmpmeta>\n<?xpacket end=\"w\"?>"));
    assert!(!contains(&out, "ns0"));
    for needle in ["xmlns:xmp=\"", "xmlns:lc=\"", "<lc:flag>pick</lc:flag>", "<x:xmpmeta xmlns:x=", "rdf:about"] {
        assert!(contains(&out, needle), "{needle}");
    }
    // only the two changed values differ from the PC file
    let before = String::from_utf8(pc).unwrap();
    let after = String::from_utf8(out).unwrap();
    assert_eq!(before.replace("<lc:flag>none<", "<lc:flag>pick<").replace("\"vibrance\":0.0", "\"vibrance\":9.0"), after);
}

#[test]
fn json_layout_follows_the_pc_file() {
    let ph = sc().s(&[("color.vibrance", j(9.0))]).build();
    let merged = with_settings(&[("light.exposure", j(0.7)), ("color.vibrance", j(9.0))]);
    let pc_ind = sc().s(&[("light.exposure", j(0.7))]).lay(Lay::Indent("  ")).build();
    let (out, _) = m(&sc().build(), &pc_ind, &ph);
    assert_eq!(text_of(&out, "lc:settings").unwrap(), dump(&merged, &Lay::Indent("  ")));
    let pc_sp = sc().s(&[("light.exposure", j(0.7))]).lay(Lay::Spaced).build();
    let (out, _) = m(&sc().build(), &pc_sp, &ph);
    assert!(contains(&out, "\"vibrance\": 9.0"));
    let pc_tab = sc().s(&[("light.exposure", j(0.7))]).lay(Lay::Indent("\t")).build();
    let (out, _) = m(&sc().build(), &pc_tab, &ph);
    assert!(contains(&out, "\n\t\"version\": 1,"));
    let (out, _) = m(&sc().build(), &sc().s(&[("light.exposure", j(0.7))]).build(), &ph);
    assert!(contains(&out, "\"vibrance\":9.0")); // compact stays compact
}

#[test]
fn text_is_escaped() {
    let tricky = with_settings(&[("profile.id", json!("a<b>&\"c\""))]);
    let (out, _) = m(&sc().build(), &Sc { settings: tricky, ..sc() }.build(), &sc().s(&[("light.exposure", j(1.0))]).build());
    assert_eq!(settings_of(&out)["profile"]["id"], json!("a<b>&\"c\""));
}

#[test]
fn complex_properties_and_namespace_declarations() {
    let dc = "http://purl.org/dc/elements/1.1/";
    let phone = sc()
        .extra(&format!("\n   <d:subject xmlns:d=\"{dc}\"><rdf:Bag><rdf:li>trip</rdf:li><rdf:li>sea &amp; sun</rdf:li></rdf:Bag></d:subject>"))
        .rating(3)
        .build();
    // the PC does not declare dc at all; it binds the phone's prefix d to something else
    let pc = String::from_utf8(sc().build()).unwrap().replace("xmlns:lc=\"", "xmlns:d=\"urn:other\" xmlns:lc=\"").into_bytes();
    let (out, conflicts) = m(&sc().build(), &pc, &phone);
    assert!(conflicts.is_empty());
    let s = String::from_utf8(out.clone()).unwrap();
    assert!(s.contains(&format!("xmlns:d=\"{dc}\"")) && s.contains("<rdf:li>sea &amp; sun</rdf:li>"), "{s}");
    assert_eq!(text_of(&out, "xmp:Rating").as_deref(), Some("3"));
    // replacing a complex value in place
    let subj = |items: &str| format!("\n   <dc:subject xmlns:dc=\"{dc}\"><rdf:Bag>{items}</rdf:Bag></dc:subject>");
    let phone2 = sc().extra(&subj("<rdf:li>trip</rdf:li><rdf:li>new</rdf:li>")).build();
    let pc2 = sc().extra(&subj("<rdf:li>trip</rdf:li>")).flag("pick").build();
    let base2 = sc().extra(&subj("<rdf:li>trip</rdf:li>")).build();
    let (out, conflicts) = m(&base2, &pc2, &phone2);
    assert!(conflicts.is_empty());
    assert!(contains(&out, "<rdf:li>trip</rdf:li><rdf:li>new</rdf:li>"));
    assert_eq!(text_of(&out, "lc:flag").as_deref(), Some("pick"));
}

#[test]
fn attribute_added_with_an_undeclared_prefix() {
    let crs = "http://ns.adobe.com/camera-raw-settings/1.0/";
    let phone = String::from_utf8(sc().build())
        .unwrap()
        .replace("xmlns:lc=\"", &format!("xmlns:crs=\"{crs}\" crs:Exposure2012=\"+1.00\" xmlns:lc=\""))
        .into_bytes();
    let (out, conflicts) = m(&sc().build(), &sc().rating(1).build(), &phone);
    assert!(conflicts.is_empty());
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("crs:Exposure2012=\"+1.00\""));
    assert_eq!(s.matches("xmlns:crs=").count(), 1);
}

#[test]
fn self_closing_description() {
    let crs = "http://ns.adobe.com/camera-raw-settings/1.0/";
    let doc = |body: &str, attrs: &str| {
        format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="{NS_RDF}">
  <rdf:Description rdf:about="" xmlns:xmp="{NS_XMP}" xmlns:crs="{crs}" xmlns:dc="http://purl.org/dc/elements/1.1/" crs:Exposure2012="0"{attrs}{body}
 </rdf:RDF>
</x:xmpmeta>
"#
        )
        .into_bytes()
    };
    let base = doc("/>", "");
    let pc = doc("/>", " xmp:Rating=\"2\"");
    let phone = doc("><dc:title><rdf:Alt><rdf:li xml:lang=\"x-default\">Sunset</rdf:li></rdf:Alt></dc:title></rdf:Description>", "");
    let (out, conflicts) = m(&base, &pc, &phone);
    assert!(conflicts.is_empty());
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("xmp:Rating=\"2\"") && s.contains("Sunset</rdf:li>") && s.contains("</rdf:Description>"), "{s}");
    assert_eq!(s.matches("xmp:Rating").count(), 1);
}

#[test]
fn nothing_to_merge_returns_the_pc_bytes() {
    let pc = sc().s(&[("light.exposure", j(0.7))]).build();
    assert_eq!(m(&sc().build(), &pc, &sc().build()).0, pc); // phone changed nothing
    assert_eq!(m(&sc().build(), &pc, &pc).0, pc); // both made the same edit
}

#[test]
fn gives_up_when_it_cannot_merge() {
    let b = sc().build();
    let ok = sc().s(&[("light.exposure", j(0.7))]).build();
    assert!(try_merge(&b, &ok, &sc().s(&[("color.vibrance", j(5.0))]).build()).is_some());
    for key in ["version", "process"] {
        // engine versions differ
        assert!(try_merge(&b, &ok, &sc().s(&[(key, json!(99))]).build()).is_none(), "{key}");
        assert!(try_merge(&b, &sc().s(&[(key, json!(99))]).build(), &ok).is_none(), "{key}");
    }
    let bad_json = String::from_utf8(b.clone()).unwrap().replace("\"version\":1,", "\"version\":1,,").into_bytes();
    for (x, y, z) in [(&bad_json, &ok, &b), (&b, &bad_json, &ok), (&b, &ok, &bad_json)] {
        assert!(try_merge(x, y, z).is_none());
    }
    let bad_xml = ok[..ok.len() - 40].to_vec();
    for (x, y, z) in [(&bad_xml, &ok, &b), (&b, &bad_xml, &ok), (&b, &ok, &bad_xml), (&b, &ok, &Vec::new())] {
        assert!(try_merge(x, y, z).is_none());
    }
    let crs_only = format!(r#"<x:xmpmeta xmlns:x="a"><rdf:RDF xmlns:rdf="{NS_RDF}"><rdf:Description rdf:about="" xmlns:crs="c" crs:Exposure2012="1"/></rdf:RDF></x:xmpmeta>"#).into_bytes();
    assert!(try_merge(&crs_only, &ok, &b).is_none()); // the base has no lc:settings
    assert!(try_merge(&crs_only, &crs_only, &b).is_none());
    assert!(try_merge(&crs_only, &crs_only, &crs_only).is_some()); // nobody has any
    assert!(try_merge(&b, &ok, br#"<a xmlns:lc="x">lc:settings</a>"#).is_none()); // no rdf:Description
    let mut dtd = b"<!DOCTYPE a [<!ENTITY e 'x'>]>".to_vec();
    dtd.extend(&b);
    assert!(try_merge(&b, &ok, &dtd).is_none());
    let latin = String::from_utf8(sc().decl().build()).unwrap().replace("encoding=\"UTF-8\"", "encoding=\"ISO-8859-1\"").into_bytes();
    assert!(try_merge(&b, &latin, &b).is_none());
    assert!(try_merge(&b, &sc().decl().build(), &b).is_some());
    let twice = String::from_utf8(b.clone()).unwrap().replace("<lc:flag>none</lc:flag>", "<lc:flag>a</lc:flag><lc:flag>b</lc:flag>").into_bytes();
    assert!(try_merge(&b, &ok, &twice).is_none()); // a property twice
    let (t, a) = (text_of(&b, "lc:settings").unwrap(), String::from_utf8(b.clone()).unwrap());
    let not_obj = a.replace(&t.replace('<', "&lt;").replace('&', "&amp;"), "[1]").into_bytes();
    assert!(try_merge(&b, &ok, &not_obj).is_none()); // not a JSON object
}

#[test]
fn phone_chain_revert_wins() {
    // The phone's next edit names its own previous edit E1 as base, which the PC never had as a version.
    let v0 = sc().build();
    let pc1 = sc().s(&[("color.vibrance", j(30.0))]).rating(2).build();
    let e1 = sc().s(&[("light.exposure", j(0.5)), ("light.contrast", j(20.0))]).build();
    let (m1, conflicts) = m(&v0, &pc1, &e1);
    assert!(conflicts.is_empty());
    let e2 = sc().s(&[("light.exposure", j(0.0)), ("light.contrast", j(20.0))]).build();
    let (m2, conflicts) = merge(&e1, &m1, &e2, false); // even though the PC is "newer"
    assert!(conflicts.is_empty());
    assert_eq!(settings_of(&m2), with_settings(&[("light.exposure", j(0.0)), ("light.contrast", j(20.0)), ("color.vibrance", j(30.0))]));
    assert_eq!(text_of(&m2, "xmp:Rating").as_deref(), Some("2")); // the PC's own change survives
    let (m2b, _) = merge(&v0, &m1, &e2, false); // (against V0 the revert looks like no change)
    assert_eq!(settings_of(&m2b)["light"]["exposure"], json!(0.5));
}
