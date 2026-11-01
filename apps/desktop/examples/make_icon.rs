//! Writes `assets/icon.ico`, the exe's icon on Windows: the tray's idle picture at several sizes.
//! `cargo run -p lightcraft-cloud-desktop --no-default-features --example make_icon`

fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon.ico");
    std::fs::write(&path, lightcraft_cloud_desktop::tray::ico_bytes(&lightcraft_cloud_desktop::tray::ICO_SIZES)).expect("write icon.ico");
    println!("wrote {}", path.display());
}
