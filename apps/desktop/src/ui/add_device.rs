//! The "Add a device" panel (home PC): shows a pairing QR and a short code that hand this library's rclone remote to a phone or
//! another PC. See `lightcraft_cloud_client::pairing`.
//!
//! The pairing text and the code live in memory only, in [`Shown`], and are wiped when it is dropped (the window closing) or
//! [`AddDevicePanel::clear`]ed. The rclone config is read when the button is pressed, is not written anywhere, and is not kept.
//! Nothing here logs, and nothing derives `Debug`.

use std::path::PathBuf;

use egui::{Color32, ColorImage, RichText, TextureHandle, TextureOptions, Ui, vec2};
use lightcraft_cloud_client::pairing::{self, Bundle};
use zeroize::Zeroize;

use crate::ui::theme::{self, Kind as ButtonKind};

/// What is shown once the button was pressed.
struct Shown {
    text: String,
    code: String,
    qr: TextureHandle,
}

impl Drop for Shown {
    fn drop(&mut self) {
        self.text.zeroize();
        self.code.zeroize();
    }
}

pub struct AddDevicePanel {
    /// `name:folder`, the config's `remote`.
    remote: String,
    /// The rclone config the remote is defined in.
    rclone_config: PathBuf,
    shown: Option<Shown>,
    /// Why nothing could be shown (words for the user, never the config's contents).
    refusal: Option<String>,
}

impl std::fmt::Debug for AddDevicePanel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddDevicePanel").field("showing", &self.shown.is_some()).finish_non_exhaustive()
    }
}

/// Pixels per QR module in the texture, and the quiet zone around it in modules (the QR standard asks for four).
const SCALE: usize = 8;
const QUIET: usize = 4;

/// The QR of `text` as black on white with a quiet zone, `SCALE` pixels per module. None if it does not fit a QR code.
pub fn qr_image(text: &str) -> Option<ColorImage> {
    let qr = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M).ok()?;
    let n = qr.width();
    let side = (n + 2 * QUIET) * SCALE;
    let colors = qr.to_colors();
    let mut pixels = vec![Color32::WHITE; side * side];
    for y in 0..n {
        for x in 0..n {
            if colors[y * n + x] == qrcode::Color::Dark {
                for dy in 0..SCALE {
                    let row = ((y + QUIET) * SCALE + dy) * side + (x + QUIET) * SCALE;
                    pixels[row..row + SCALE].fill(Color32::BLACK);
                }
            }
        }
    }
    Some(ColorImage { size: [side, side], source_size: vec2(side as f32, side as f32), pixels })
}

impl AddDevicePanel {
    pub fn new(remote: String, rclone_config: PathBuf) -> Self {
        AddDevicePanel { remote, rclone_config, shown: None, refusal: None }
    }

    /// Whether a code is on the screen.
    pub fn is_showing(&self) -> bool {
        self.shown.is_some()
    }

    /// The text and code (for tests and for the copy button; both are secrets).
    pub fn pairing(&self) -> Option<(&str, &str)> {
        self.shown.as_ref().map(|s| (s.text.as_str(), s.code.as_str()))
    }

    pub fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }

    /// Wipe the text and code.
    pub fn clear(&mut self) {
        self.shown = None;
    }

    /// Read the rclone config, keep only this library's remote, seal it, and draw the QR.
    pub fn make(&mut self, ctx: &egui::Context) {
        self.shown = None;
        self.refusal = None;
        match self.build(ctx) {
            Ok(s) => self.shown = Some(s),
            Err(e) => self.refusal = Some(e),
        }
    }

    fn build(&self, ctx: &egui::Context) -> Result<Shown, String> {
        // (the config text is a local, zeroized when this returns; it is never written)
        let conf = zeroize::Zeroizing::new(std::fs::read_to_string(&self.rclone_config).map_err(|e| {
            format!("Could not read the rclone config ({}): {}.", self.rclone_config.display(), e.kind())
        })?);
        let config = pairing::export_config(&conf, &self.remote).map_err(|e| e.to_string())?;
        let bundle = Bundle::new(self.remote.clone(), config);
        let (text, code) = pairing::seal(&bundle).map_err(|e| e.to_string())?;
        let image = qr_image(&text).ok_or_else(|| "The pairing text is too long for a QR code.".to_string())?;
        let qr = ctx.load_texture("pairing-qr", image, TextureOptions::NEAREST);
        Ok(Shown { text, code, qr })
    }

    pub fn show(&mut self, ui: &mut Ui) {
        let t = theme::CHARCOAL;
        theme::title(ui, "Add a device");
        ui.add_space(10.0);
        theme::panel(ui, None, |ui| {
            ui.add(egui::Label::new(RichText::new("This gives the device access to your cloud storage account, using the same sign-in as this PC. Only do this for devices you own. Do not share your screen and do not post the code or the picture anywhere.").color(t.caution)).wrap());
            theme::hint(ui, "Only tested with Google Drive.");
        });
        theme::gap(ui);
        if self.shown.is_none() {
            if theme::button(ui, "Show pairing code", ButtonKind::Primary).clicked() {
                let ctx = ui.ctx().clone();
                self.make(&ctx);
            }
            if let Some(r) = &self.refusal {
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(r).color(t.reject)).wrap());
            }
            return;
        }
        let mut copy = None;
        if let Some(s) = &self.shown {
            theme::panel(ui, Some("Scan this with the phone, then type the code"), |ui| {
                let side = ui.available_width().clamp(200.0, 520.0);
                ui.vertical_centered(|ui| {
                    ui.add(egui::Image::new((s.qr.id(), vec2(side, side))));
                    ui.add_space(6.0);
                    ui.label(RichText::new(&s.code).monospace().size(36.0).color(t.text));
                });
                theme::hint(ui, "The code is typed on the other device. It works only with this picture.");
            });
            theme::gap(ui);
            ui.horizontal(|ui| {
                if theme::button(ui, "Copy text for another PC", ButtonKind::Secondary).clicked() {
                    copy = Some(s.text.clone());
                }
                if theme::button(ui, "Hide", ButtonKind::Secondary).clicked() {
                    copy = Some(String::new());
                }
            });
            theme::hint(ui, "On another PC: Settings, I have a pairing code, paste the text and type the code. The text is copied to the clipboard; clear it when you are done.");
        }
        match copy {
            Some(t) if t.is_empty() => self.clear(),
            Some(t) => ui.ctx().copy_text(t),
            None => {}
        }
    }
}
