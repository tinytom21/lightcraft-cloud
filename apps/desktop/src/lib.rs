//! LightCraft Cloud for the PC: the always-running app (a tray icon, a background engine, a Status and a Settings window), the
//! command line, and LightCraft's engine as the sync crate's renderer. The command line is `main.rs`; the sync itself is
//! `lightcraft_cloud_client::home`.
//!
//! The `home` feature (on by default) is the home PC's half, and the only part that needs LightCraft's engine ([`engine`], publishing).
//! `--no-default-features` builds the light app: the same tray, windows and scheduler, no engine.
//!
//! * [`scheduler`], [`service`], [`watcher`], [`worker`]: the background engine (UI-independent; tests move a fake [`clock`]);
//! * [`lightcraft`], [`pending`]: telling a running LightCraft to add the photos that were adopted (its control channel, and the
//!   durable list);
//! * [`appstate`], [`config`], [`settings`], [`autostart`], [`instance`], [`backup`], [`notifier`]: what it keeps and uses;
//! * [`ui`], [`tray`], [`app`]: the windows (egui, in a process of their own), the tray icon, and what ties them together.

pub mod app;
pub mod appstate;
pub mod autostart;
pub mod backup;
pub mod clock;
pub mod config;
#[cfg(feature = "home")]
pub mod engine;
pub mod instance;
pub mod lightcraft;
pub mod notifier;
pub mod pending;
pub mod scheduler;
pub mod service;
pub mod settings;
pub mod tray;
pub mod ui;
pub mod watcher;
#[cfg(windows)]
pub mod winsys;
pub mod worker;
