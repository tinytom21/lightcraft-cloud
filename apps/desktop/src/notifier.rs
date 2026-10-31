//! Desktop notifications, with the Python tool's texts: the title "LightCraft cloud"; "N phone edits applied ..." (folder
//! naming included, from the sync crate); and, for a sync that failed, "Publisher failed".

use lightcraft_cloud_client::home::notify_message;

pub const TITLE: &str = "LightCraft cloud";

pub trait Notifier: Send {
    fn notify(&self, summary: &str, body: &str);
}

/// The desktop's notification service (D-Bus, through `notify-rust`). Failures are logged, never raised; the notification is
/// shown from a thread of its own so a slow notification daemon cannot hold up a sync.
pub struct DesktopNotifier;

impl Notifier for DesktopNotifier {
    fn notify(&self, summary: &str, body: &str) {
        let (s, b) = (summary.to_string(), body.to_string());
        std::thread::spawn(move || {
            if let Err(e) = notify_rust::Notification::new().appname("LightCraft Cloud").summary(&s).body(&b).show() {
                log::warn!("desktop notification unavailable: {e}");
            }
        });
    }
}

/// Does nothing (headless runs, tests).
pub struct NoNotifier;

impl Notifier for NoNotifier {
    fn notify(&self, _summary: &str, _body: &str) {}
}

/// "2 phone edits applied (1 merged with PC changes) in 2024/Trip - in LightCraft run Synchronize Folder on this folder".
/// `applied`: edits applied, merged ones included.
pub fn edits_applied(applied: usize, merged: usize, folders: &[String], source_name: &str) -> String {
    notify_message(applied, folders, source_name, merged)
}

/// Phone edits that lost to a newer PC sidecar (they are kept in the history).
pub fn edits_kept(kept: usize) -> String {
    let edits = if kept == 1 { "1 phone edit was".to_string() } else { format!("{kept} phone edits were") };
    format!("{edits} not applied: the PC's sidecar was newer (the phone's version is kept in history)")
}

/// A sync that did not work.
pub fn publisher_failed() -> &'static str {
    "Publisher failed - see the Status window or the log"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_texts_are_the_python_tools() {
        let none: [String; 0] = [];
        assert_eq!(edits_applied(1, 0, &none, "Photos"), "1 phone edit applied \u{2014} in LightCraft run Synchronize Folder");
        assert_eq!(
            edits_applied(3, 2, &["A".into()], "Photos"),
            "3 phone edits applied (2 merged with PC changes) in A \u{2014} in LightCraft run Synchronize Folder on this folder"
        );
        assert_eq!(
            edits_applied(5, 0, &["".into(), "B".into(), "C".into(), "D".into()], "Photos"),
            "5 phone edits applied in Photos, B, C and 1 more \u{2014} in LightCraft run Synchronize Folder on these folders"
        );
        assert_eq!(TITLE, "LightCraft cloud");
        assert!(edits_kept(1).starts_with("1 phone edit was not applied") && edits_kept(2).starts_with("2 phone edits were not applied"));
        assert!(publisher_failed().starts_with("Publisher failed"));
    }
}
