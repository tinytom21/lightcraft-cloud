//! Writes a small generated photo folder for the tests and the parity harness: procedural scenes from LightCraft's
//! `lightcraft-scenes` as JPEG/PNG, a synthetic DNG with a JPEG of the same stem (a raw+JPEG pair), sidecars with edits, an
//! identical copy, an excluded folder. No real photos.
//!
//! ```text
//! cargo run -p lightcraft-cloud-desktop --example fixture -- /tmp/lc-fixture/src
//! ```

#![allow(dead_code)]

use std::path::Path;

use lightcraft_codecs::encode::{ChromaSubsampling, EncodeImage, EncodeMeta, encode_jpeg, encode_png};
use lightcraft_raster::{Rgb32f, Rgba8};

/// Linear Rec.2020 -> 8-bit sRGB (clipped; good enough for a test image).
fn to_srgb8(img: &Rgb32f) -> Rgba8 {
    let enc = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
        (s * 255.0 + 0.5) as u8
    };
    let m = [[1.6605, -0.5876, -0.0728], [-0.1246, 1.1329, -0.0083], [-0.0182, -0.1006, 1.1187]];
    Rgba8 {
        width: img.width,
        height: img.height,
        data: img
            .data
            .iter()
            .map(|p| {
                let c = |r: usize| enc(m[r][0] * p[0] + m[r][1] * p[1] + m[r][2] * p[2]);
                [c(0), c(1), c(2), 255]
            })
            .collect(),
    }
}

fn jpeg(scene: usize, w: usize, h: usize) -> Vec<u8> {
    let scenes = lightcraft_scenes::demo_library();
    let img = to_srgb8(&scenes[scene % scenes.len()].render(w, h));
    encode_jpeg(&EncodeImage::rgba8(&img), 90, ChromaSubsampling::S420, &EncodeMeta::default()).expect("jpeg")
}

fn png(scene: usize, w: usize, h: usize) -> Vec<u8> {
    let scenes = lightcraft_scenes::demo_library();
    let img = to_srgb8(&scenes[scene % scenes.len()].render(w, h));
    encode_png(&EncodeImage::rgba8(&img), &EncodeMeta::default()).expect("png")
}

/// A tiny Bayer DNG written by LightCraft's own DNG writer.
fn dng(seed: u16) -> Vec<u8> {
    use lightcraft_raw::*;
    let (w, h) = (64usize, 48usize);
    let data: Vec<u16> = (0..w * h).map(|i| 256 + (((i % w) * 200 + (i / w) * 150) as u16).wrapping_add(seed * 977) % 9000).collect();
    let raw = RawImage {
        format: RawFormat::Dng,
        width: w,
        height: h,
        cpp: 1,
        data: RawData::U16(data),
        cfa: Some(Cfa::bayer("RGGB").expect("cfa")),
        bits: 14,
        black: BlackLevel::uniform(256.0),
        white: vec![16000.0],
        active_area: Rect::new(0, 0, w, h),
        crop: Rect::new(0, 0, w, h),
        orientation: Orientation::Normal,
        color: ColorData {
            illuminant: [17, 21],
            color_matrix: [
                Some(lightcraft_color::Mat3([[0.9, 0.2, -0.15], [-0.3, 1.25, 0.08], [0.02, -0.12, 0.85]])),
                Some(lightcraft_color::Mat3([[0.7, 0.3, -0.1], [-0.35, 1.3, 0.1], [0.05, -0.2, 1.0]])),
            ],
            as_shot_neutral: Some([0.5, 1.0, 0.7]),
            ..Default::default()
        },
        wb_multipliers: None,
        linearized: false,
        pixel_aspect: 1.0,
        opcodes: OpcodeLists::default(),
        metadata: lightcraft_meta::Metadata::default(),
        lens_metadata: Default::default(),
    };
    write_dng(&raw, &DngWriteOptions::default()).expect("dng")
}

fn xmp(rating: u32, exposure: &str) -> String {
    format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
 <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" xmp:Rating="{rating}" crs:Exposure2012="{exposure}"/>
</rdf:RDF></x:xmpmeta>
"#
    )
}

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(&p, bytes).expect("write");
    // old enough for the publisher to treat it as finished
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::OpenOptions::new().write(true).open(&p).and_then(|f| f.set_modified(t)).expect("mtime");
}

/// The generated folder. Returns nothing; see the paths below.
pub fn make(root: &Path) {
    put(root, "2024/Trip/IMG_0001.jpg", &jpeg(0, 640, 400));
    put(root, "2024/Trip/IMG_0002.jpg", &jpeg(1, 400, 640));
    put(root, "2024/Trip/IMG_0002.xmp", xmp(3, "+0.80").as_bytes());
    // a raw+JPEG pair (same stem): the JPEG is the primary, the DNG its alternate
    put(root, "2024/Trip/PAIR_0001.DNG", &dng(1));
    put(root, "2024/Trip/PAIR_0001.JPG", &jpeg(2, 600, 400));
    // a raw alone, with a sidecar
    put(root, "2024/Trip/RAW_0002.dng", &dng(2));
    put(root, "2024/Trip/RAW_0002.xmp", xmp(1, "+0.30").as_bytes());
    put(root, "Other/IMG_0003.png", &png(3, 320, 240));
    // the same bytes as IMG_0001: listed once
    put(root, "Other/IMG_0001_copy.jpg", &jpeg(0, 640, 400));
    // small photos for the inbox scenarios of the parity harness
    for i in 0..6usize {
        put(root, &format!("Edits/E{i}.jpg"), &jpeg(i + 1, 160 + 8 * i, 100 + 4 * i));
    }
    put(root, "exported/skip.jpg", &jpeg(4, 200, 200));
    put(root, "notes.txt", b"not a photo\n");
}

/// What changes between two syncs in the parity harness: a new photo, and a photo replaced in place.
pub fn round2(root: &Path) {
    put(root, "Late/NEW_0001.jpg", &jpeg(9, 300, 200));
    put(root, "Other/IMG_0003.png", &png(10, 320, 240));
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: fixture DIR [round2]");
    if args.next().as_deref() == Some("round2") {
        round2(Path::new(&dir));
    } else {
        make(Path::new(&dir));
    }
    println!("fixture written to {dir}");
}
