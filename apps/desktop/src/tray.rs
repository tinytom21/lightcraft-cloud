//! The tray icon. Five pictures: idle (green), syncing (blue), amber (phone edits or anything else waiting, offline), paused (grey), red
//! (a failure or a problem); uploading shares the syncing picture. Idle only when truly settled. Menu: Sync now, Pause/Resume, Status…,
//! Settings…, Quit. The icons are generated here (a disc with a lens ring), so there are no image files.
//!
//! On Linux the icon is a StatusNotifierItem over D-Bus (`tray-icon`'s `ksni` backend: no GTK and no libappindicator to
//! install). The desktop needs a tray host: KDE has one, GNOME needs its AppIndicator extension (Ubuntu ships it).
//!
//! On Windows it is a notification-area icon (`tray-icon`'s own backend). Its window belongs to the main thread, which pumps the
//! Win32 messages (see `app::main_loop_tray`). The same icons are the exe's icon (`assets/icon.ico`, see [`ico_bytes`]).

use crate::appstate::TrayState;

/// What a menu entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    SyncNow,
    PauseResume,
    Status,
    Settings,
    Quit,
}

pub const ICON_SIZE: u32 = 32;

fn color(state: TrayState) -> [u8; 3] {
    match state.icon() {
        TrayState::Idle => [0x2e, 0x9d, 0x5b],
        TrayState::Syncing => [0x2f, 0x7f, 0xd8],
        TrayState::PhoneEdits => [0xe0, 0x9a, 0x1f],
        TrayState::Paused => [0x8a, 0x8f, 0x98],
        _ => [0xd0, 0x3b, 0x3b],
    }
}

/// The icon as RGBA, `size` x `size`: a coloured disc with a dark rim and a white lens ring; the disc has a notch cut for a
/// spinner feel while syncing, and a bar for Paused, so the states differ by more than colour.
pub fn icon_rgba(state: TrayState, size: u32) -> Vec<u8> {
    let state = state.icon();
    let mut px = vec![0u8; (size * size * 4) as usize];
    let c = size as f32 / 2.0;
    let [r, g, b] = color(state);
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
            let d = (dx * dx + dy * dy).sqrt() / c; // 0 centre .. 1 edge
            let rgba = if d > 0.94 {
                [0, 0, 0, 0]
            } else if d > 0.82 {
                [0x20, 0x24, 0x2b, 0xff]
            } else if (0.34..0.46).contains(&d) {
                [0xff, 0xff, 0xff, 0xf0]
            } else if (state == TrayState::Paused && dx.abs() < c * 0.12 && d < 0.3)
                || (state == TrayState::Syncing && d < 0.82 && dx > 0.0 && dy.abs() < c * 0.1 && d > 0.46)
            {
                [0xff, 0xff, 0xff, 0xff]
            } else {
                [r, g, b, 0xff]
            };
            let i = ((y * size + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&rgba);
        }
    }
    px
}

/// A Windows icon file (`.ico`) of the idle picture at each of `sizes` (at most 256 each): 32-bit BMP images, the form every Windows
/// version reads. `assets/icon.ico` is this with [`ICO_SIZES`]; a test keeps the two equal, and `examples/make_icon.rs` writes it.
pub fn ico_bytes(sizes: &[u32]) -> Vec<u8> {
    let mut images: Vec<Vec<u8>> = Vec::new();
    for &size in sizes {
        let rgba = icon_rgba(TrayState::Idle, size);
        let mut img = Vec::new();
        // BITMAPINFOHEADER: the height counts the AND mask too, so it is doubled
        for v in [40u32, size, size * 2, 1 | (32 << 16), 0, 0, 0, 0, 0, 0] {
            img.extend_from_slice(&v.to_le_bytes());
        }
        for y in (0..size).rev() {
            for x in 0..size {
                let i = ((y * size + x) * 4) as usize;
                img.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]); // (BGRA, bottom row first)
            }
        }
        img.extend(std::iter::repeat_n(0u8, (size.div_ceil(32) * 4 * size) as usize)); // (AND mask: all opaque, the alpha channel rules)
        images.push(img);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&size, img) in sizes.iter().zip(&images) {
        let dim = if size >= 256 { 0 } else { size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        out.extend_from_slice(&img);
    }
    out
}

/// The sizes in the exe's icon.
pub const ICO_SIZES: [u32; 6] = [16, 24, 32, 48, 64, 256];

/// The label of the Pause/Resume entry.
pub fn pause_label(paused: bool) -> &'static str {
    if paused { "Resume" } else { "Pause" }
}

#[cfg(any(target_os = "linux", windows))]
pub use native::Tray;

#[cfg(any(target_os = "linux", windows))]
mod native {
    use tray_icon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    use super::{ICON_SIZE, MenuAction, icon_rgba, pause_label};
    use crate::appstate::{AppStatus, TrayState};

    pub struct Tray {
        icon: TrayIcon,
        pause: MenuItem,
        ids: Vec<(MenuId, MenuAction)>,
        shown: Option<(TrayState, bool)>,
    }

    fn icon_of(state: TrayState) -> Result<Icon, String> {
        Icon::from_rgba(icon_rgba(state, ICON_SIZE), ICON_SIZE, ICON_SIZE).map_err(|e| e.to_string())
    }

    impl Tray {
        pub fn new(status: &AppStatus) -> Result<Tray, String> {
            let state = status.tray_state();
            let menu = Menu::new();
            let sync = MenuItem::new("Sync now", true, None);
            let pause = MenuItem::new(pause_label(status.paused), true, None);
            let st = MenuItem::new("Status…", true, None);
            let se = MenuItem::new("Settings…", true, None);
            let quit = MenuItem::new("Quit", true, None);
            let e = |e: tray_icon::menu::Error| e.to_string();
            menu.append(&sync).map_err(e)?;
            menu.append(&pause).map_err(e)?;
            menu.append(&PredefinedMenuItem::separator()).map_err(e)?;
            menu.append(&st).map_err(e)?;
            menu.append(&se).map_err(e)?;
            menu.append(&PredefinedMenuItem::separator()).map_err(e)?;
            menu.append(&quit).map_err(e)?;
            let icon = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_icon(icon_of(state)?)
                .with_tooltip(state.tooltip())
                .with_title("LightCraft Cloud")
                .build()
                .map_err(|e| e.to_string())?;
            let ids = vec![
                (sync.id().clone(), MenuAction::SyncNow),
                (pause.id().clone(), MenuAction::PauseResume),
                (st.id().clone(), MenuAction::Status),
                (se.id().clone(), MenuAction::Settings),
                (quit.id().clone(), MenuAction::Quit),
            ];
            Ok(Tray { icon, pause, ids, shown: Some((state, status.paused)) })
        }

        pub fn action(&self, id: &MenuId) -> Option<MenuAction> {
            self.ids.iter().find(|(i, _)| i == id).map(|(_, a)| *a)
        }

        /// Follow the status: icon, tooltip, and the Pause/Resume label (only when something changed).
        pub fn update(&mut self, status: &AppStatus) {
            let now = (status.tray_state(), status.paused);
            if self.shown == Some(now) {
                return;
            }
            if let Ok(i) = icon_of(now.0) {
                let _ = self.icon.set_icon(Some(i));
            }
            let _ = self.icon.set_tooltip(Some(now.0.tooltip()));
            self.pause.set_text(pause_label(now.1));
            self.shown = Some(now);
        }
    }
}

/// Whether a tray exists on this platform in this build.
pub const AVAILABLE: bool = cfg!(any(target_os = "linux", windows));

#[cfg(test)]
mod tests {
    use super::*;

    fn px(v: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [v[i], v[i + 1], v[i + 2], v[i + 3]]
    }

    #[test]
    fn every_picture_is_its_own_icon_with_a_clear_corner_and_a_coloured_disc_and_the_other_states_share_one() {
        let mut seen = std::collections::HashSet::new();
        for s in TrayState::ALL.into_iter().filter(|s| s.icon() == *s) {
            let v = icon_rgba(s, ICON_SIZE);
            assert_eq!(v.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
            assert_eq!(px(&v, ICON_SIZE, 0, 0)[3], 0, "{s:?}: corner transparent");
            let [r, g, b, a] = px(&v, ICON_SIZE, 8, 16); // (inside the disc, off the lens ring and the glyphs)
            assert_eq!(a, 255);
            assert_eq!([r, g, b], color(s));
            assert!(seen.insert(v), "{s:?} looks like another state");
        }
        assert_eq!(seen.len(), 5, "five pictures");
        for s in TrayState::ALL {
            assert_eq!(icon_rgba(s, ICON_SIZE), icon_rgba(s.icon(), ICON_SIZE), "{s:?} is drawn with its shared picture");
        }
    }

    #[test]
    fn the_exe_icon_file_is_the_idle_picture_at_its_sizes() {
        let ico = ico_bytes(&ICO_SIZES);
        assert_eq!(&ico[..6], &[0, 0, 1, 0, ICO_SIZES.len() as u8, 0]);
        // every entry points at an image inside the file whose header says the size the entry does
        for (n, size) in ICO_SIZES.iter().enumerate() {
            let e = &ico[6 + 16 * n..6 + 16 * (n + 1)];
            let (len, off) = (u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize, u32::from_le_bytes(e[12..16].try_into().unwrap()) as usize);
            assert!(off + len <= ico.len());
            assert_eq!(u32::from_le_bytes(ico[off + 4..off + 8].try_into().unwrap()), *size);
            assert_eq!(len, 40 + (size * size * 4) as usize + (size.div_ceil(32) * 4 * size) as usize);
        }
        // the file in the repository is what this makes (`cargo run -p lightcraft-cloud-desktop --example make_icon` rewrites it)
        assert!(include_bytes!("../assets/icon.ico").as_slice() == ico.as_slice(), "assets/icon.ico is out of date: run the make_icon example");
    }

    #[test]
    fn the_pause_entry_names_what_it_will_do() {
        assert_eq!((pause_label(false), pause_label(true)), ("Pause", "Resume"));
    }
}
