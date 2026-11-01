//! The windows: Status and Settings (Setup on a first run), in egui at the version LightCraft uses. The panels are plain
//! functions of a `&mut egui::Ui` and a small state each, so they are tested headlessly with `egui_kittest` and can move into
//! LightCraft later. `window` is the eframe shell around them; the tray starts it in a process of its own.

pub mod import_panel;
pub mod settings_panel;
pub mod status;
pub mod theme;
pub mod window;

pub use import_panel::{ImportBackend, ImportPanel, ThreadBackend};
pub use settings_panel::{FolderPicker, SettingsPanel};
pub use window::{Tab, run_window};
