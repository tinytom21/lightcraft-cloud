//! The look of the windows: LightCraft's Charcoal theme, so they look like part of the desktop app.
//!
//! The values come from LightCraft's `crates/ui-egui/src/theme.rs` (v0.15 line, MIT OR Apache-2.0): the Charcoal `Tokens`
//! defaults, `apply_kind`'s visuals, rounding and spacing (with `craft_ui::theme::WidgetPalette::apply_to` written out), and
//! `font_definitions` (Inter Regular, and SemiBold as the `semibold` family). The button, tab and section looks follow
//! `widgets.rs` and `widgets/button.rs` there. Only Charcoal is kept: there is no theme switching, and nothing here depends
//! on `lightcraft-ui-egui`. The Inter fonts (SIL OFL 1.1, see `assets/fonts/OFL-Inter.txt`) are embedded.
//!
//! The helpers read [`CHARCOAL`] directly rather than the context, so a panel drawn without [`install`] (a test, say) still
//! gets the right colours; only the semibold face needs [`install`], and falls back to the regular one without it.

use std::sync::Arc;

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Response, RichText, Sense, Stroke, StrokeKind, Ui, Visuals, vec2};

/// The font family name of Inter SemiBold.
pub const FONT_SEMIBOLD: &str = "semibold";

/// The colours and sizes the windows use (LightCraft's `Tokens`, the ones that matter here).
#[derive(Clone, Copy, Debug)]
pub struct Tokens {
    /// Top bar and panels.
    pub chrome: Color32,
    /// The area behind the panels.
    pub canvas: Color32,
    pub divider: Color32,
    pub inset: Color32,
    pub field: Color32,
    pub field_border: Color32,
    pub button: Color32,
    pub button_border: Color32,
    pub hover: Color32,
    pub pressed: Color32,
    pub text: Color32,
    pub text_label: Color32,
    pub text_dim: Color32,
    pub text_disabled: Color32,
    pub accent: Color32,
    /// Errors and problems.
    pub reject: Color32,
    /// Cautions: a muted amber.
    pub caution: Color32,
    /// All is well. LightCraft has no such token; this is its green colour label (`label_color(Green)`), which sits with the
    /// other Charcoal colours.
    pub ok: Color32,
    pub top_bar_h: f32,
}

/// LightCraft's Charcoal tokens.
pub const CHARCOAL: Tokens = Tokens {
    chrome: Color32::from_rgb(0x2d, 0x2d, 0x2d),
    canvas: Color32::from_rgb(0x1c, 0x1c, 0x1c),
    divider: Color32::from_rgb(0x1c, 0x1c, 0x1c),
    inset: Color32::from_rgb(0x23, 0x23, 0x23),
    field: Color32::from_rgb(0x22, 0x22, 0x22),
    field_border: Color32::from_rgb(0x3c, 0x3c, 0x3c),
    button: Color32::from_rgb(0x24, 0x24, 0x24),
    button_border: Color32::from_rgb(0x4c, 0x4c, 0x4c),
    hover: Color32::from_rgb(0x3a, 0x3a, 0x3a),
    pressed: Color32::from_rgb(0x3f, 0x3f, 0x3f),
    text: Color32::from_rgb(0xe2, 0xe2, 0xe2),
    text_label: Color32::from_rgb(0xbc, 0xbc, 0xbc),
    text_dim: Color32::from_rgb(0x8e, 0x8e, 0x8e),
    text_disabled: Color32::from_rgb(0x5c, 0x5c, 0x5c),
    accent: Color32::from_rgb(0x01, 0x65, 0xdd),
    reject: Color32::from_rgb(0xe0, 0x4a, 0x4a),
    caution: Color32::from_rgb(0xe3, 0xa8, 0x3c),
    ok: Color32::from_rgb(88, 176, 92),
    top_bar_h: 42.0,
};

/// Whether the semibold family is installed (a context that never saw [`install`] would panic on an unknown family).
fn has_semibold(ctx: &egui::Context) -> bool {
    ctx.fonts_mut(|f| f.definitions().families.contains_key(&FontFamily::Name(FONT_SEMIBOLD.into())))
}

/// Inter SemiBold at `size`, or Inter Regular / egui's default where [`install`] has not run.
pub fn semibold(ctx: &egui::Context, size: f32) -> FontId {
    if has_semibold(ctx) { FontId::new(size, FontFamily::Name(FONT_SEMIBOLD.into())) } else { FontId::proportional(size) }
}

/// Inter as the UI face (Regular, and SemiBold as its own family), kept ahead of egui's defaults, which cover other scripts.
pub fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("Inter".into(), Arc::new(FontData::from_static(include_bytes!("../../assets/fonts/Inter-Regular.ttf"))));
    fonts.font_data.insert("Inter-SemiBold".into(), Arc::new(FontData::from_static(include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"))));
    let defaults: Vec<String> = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut prop = vec!["Inter".to_string()];
    prop.extend(defaults.iter().cloned());
    fonts.families.insert(FontFamily::Proportional, prop);
    let mut semi = vec!["Inter-SemiBold".to_string()];
    semi.extend(defaults);
    fonts.families.insert(FontFamily::Name(FONT_SEMIBOLD.into()), semi);
    fonts
}

/// Install the fonts and style `ctx` the way LightCraft's `apply` does (the visuals go to both of egui's styles, so the
/// system's light/dark setting never swaps in egui's stock look).
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());
    apply(ctx);
}

fn apply(ctx: &egui::Context) {
    let t = CHARCOAL;
    let mut v = Visuals::dark();
    v.panel_fill = t.chrome;
    v.window_fill = t.chrome;
    v.extreme_bg_color = t.field;
    v.faint_bg_color = t.inset;
    v.window_stroke = Stroke::new(1.0, t.button_border);
    v.window_corner_radius = CornerRadius::same(6);
    v.menu_corner_radius = CornerRadius::same(6);
    v.selection.bg_fill = t.accent.gamma_multiply(0.6);
    v.selection.stroke = Stroke::new(1.0, t.accent);
    v.override_text_color = Some(t.text_label);
    v.popup_shadow = egui::epaint::Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(140) };
    v.window_shadow = egui::epaint::Shadow { offset: [0, 8], blur: 30, spread: 0, color: Color32::from_black_alpha(160) };
    // (craft_ui's `WidgetPalette::apply_to`: a fill, the radius and the foreground for each widget state)
    let w = &mut v.widgets;
    for (state, fill) in [
        (&mut w.noninteractive, t.chrome),
        (&mut w.inactive, t.button),
        (&mut w.hovered, t.hover),
        (&mut w.active, t.pressed),
        (&mut w.open, t.hover),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.corner_radius = CornerRadius::same(4);
        state.fg_stroke = Stroke::new(1.0, t.text_label);
    }
    w.noninteractive.bg_stroke = Stroke::new(1.0, t.divider);
    w.inactive.bg_stroke = Stroke::new(1.0, t.button_border);
    ctx.set_visuals_of(egui::Theme::Dark, v.clone());
    ctx.set_visuals_of(egui::Theme::Light, v);
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = vec2(6.0, 4.0);
        s.spacing.button_padding = vec2(8.0, 3.0);
        s.spacing.interact_size.y = 22.0;
        s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(13.0));
        s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(13.0));
        s.text_styles.insert(egui::TextStyle::Small, FontId::proportional(11.0));
        s.text_styles.insert(egui::TextStyle::Heading, FontId::new(16.0, FontFamily::Name(FONT_SEMIBOLD.into())));
        s.animation_time = 0.08;
    });
}

/// How loud a button is (LightCraft's `Kind`, without the ones the windows do not use).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The step to take: filled with the accent.
    Primary,
    /// Any other action: a neutral fill with a border.
    Secondary,
}

/// A 30 point button, drawn as LightCraft's `Button` (Medium) is. Returns the response; `.clicked()` is the press.
pub fn button(ui: &mut Ui, text: &str, kind: Kind) -> Response {
    let t = CHARCOAL;
    let font = match kind {
        Kind::Primary => semibold(ui.ctx(), 13.0),
        Kind::Secondary => FontId::proportional(13.0),
    };
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, Color32::WHITE);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 24.0, 30.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, text));
    let pressed = resp.is_pointer_button_down_on();
    let (fill, border, content) = match kind {
        Kind::Primary => {
            // white on the accent, darker as it is hovered and pressed
            let darker = |c: Color32, by: f32| c.lerp_to_gamma(Color32::BLACK, by);
            let fill = if pressed {
                darker(t.accent, 0.22)
            } else if resp.hovered() {
                darker(t.accent, 0.12)
            } else {
                t.accent
            };
            (fill, None, Color32::WHITE)
        }
        Kind::Secondary => (if pressed { t.pressed } else if resp.hovered() { t.hover } else { t.button }, Some(t.button_border), t.text),
    };
    let p = ui.painter();
    p.rect_filled(rect, 5.0, fill);
    if let Some(b) = border {
        p.rect_stroke(rect, 5.0, Stroke::new(1.0, b), StrokeKind::Inside);
    }
    if resp.has_focus() {
        p.rect_stroke(rect.expand(2.0), 7.0, Stroke::new(2.0, t.accent), StrokeKind::Outside);
    }
    p.galley_with_override_text_color(rect.center() - galley.size() / 2.0, galley, content);
    resp
}

/// A tab of the top bar, drawn as LightCraft's segmented buttons are: bordered, the chosen one pressed in.
pub fn tab(ui: &mut Ui, selected: bool, text: &str) -> Response {
    let t = CHARCOAL;
    let galley = ui.painter().layout_no_wrap(text.to_string(), semibold(ui.ctx(), 12.5), t.text);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 32.0, 26.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, text));
    let fill = if selected {
        t.pressed
    } else if resp.hovered() {
        t.hover
    } else {
        t.button
    };
    let p = ui.painter();
    p.rect(rect, CornerRadius::same(4), fill, Stroke::new(1.0, t.button_border), StrokeKind::Inside);
    if resp.has_focus() {
        p.rect_stroke(rect.expand(2.0), 6.0, Stroke::new(2.0, t.accent), StrokeKind::Outside);
    }
    p.galley(rect.center() - galley.size() / 2.0, galley, if selected { t.text } else { t.text_label });
    resp
}

/// The top bar: the chrome colour with the tabs inside, over the canvas.
pub fn top_bar<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) {
    let t = CHARCOAL;
    egui::Panel::top("top_bar")
        .exact_size(t.top_bar_h)
        .frame(egui::Frame::NONE.fill(t.chrome).inner_margin(Margin { left: 12, right: 12, top: 0, bottom: 0 }))
        .show_separator_line(false)
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                add(ui)
            });
        });
}

/// The area under the top bar: the canvas colour, a margin, and scrolling when the window is too small.
pub fn page<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) {
    let t = CHARCOAL;
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t.canvas).inner_margin(Margin::same(16))).show(ui, |ui| {
        // (a bar that takes its own room, so it never covers the panels' right edge)
        ui.spacing_mut().scroll.floating = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // (a column no wider than reads well, left-aligned like LightCraft's settings pages)
            ui.set_max_width(ui.available_width().min(720.0));
            add(ui)
        });
    });
}

/// A page title: Inter SemiBold, in the text colour.
pub fn title(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(semibold(ui.ctx(), 18.0)).color(CHARCOAL.text));
}

/// Quiet explanatory text (LightCraft's `hint`).
pub fn hint(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(11.5).color(CHARCOAL.text_dim));
}

/// A field's label (LightCraft's row labels: the label colour, one step up from the hint).
pub fn field_label(ui: &mut Ui, text: &str) -> Response {
    ui.label(RichText::new(text).font(semibold(ui.ctx(), 12.0)).color(CHARCOAL.text_label))
}

/// A panel: the chrome colour on the canvas, rounded, with an optional section header (LightCraft's settings `heading`:
/// semibold, the text colour) and `add` below it.
pub fn panel<R>(ui: &mut Ui, heading: Option<&str>, add: impl FnOnce(&mut Ui) -> R) -> R {
    let t = CHARCOAL;
    egui::Frame::new().fill(t.chrome).corner_radius(CornerRadius::same(6)).inner_margin(Margin::symmetric(16, 14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 8.0;
        if let Some(h) = heading {
            ui.label(RichText::new(h).font(semibold(ui.ctx(), 13.0)).color(t.text));
        }
        add(ui)
    })
    .inner
}

/// The space between two panels.
pub fn gap(ui: &mut Ui) {
    ui.add_space(12.0);
}

/// A single-line field, the way LightCraft's search field and inputs sit: tall enough to read, padded inside.
pub fn text_edit<'t>(text: &'t mut String) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text).margin(Margin::symmetric(8, 6))
}
