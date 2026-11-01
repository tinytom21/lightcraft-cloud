//! On Windows the exe gets its icon (and a name and version in its properties) from a resource compiled in.

fn main() {
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/icon.ico");
            res.set("ProductName", "LightCraft Cloud");
            res.set("FileDescription", "LightCraft Cloud");
            res.set("LegalCopyright", "MIT OR Apache-2.0");
            if let Err(e) = res.compile() {
                panic!("cannot compile the Windows resource: {e}");
            }
        }
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
}
