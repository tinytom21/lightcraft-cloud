# Building and testing

For developers. For an overview, start with the [README](../README.md). The app itself: [desktop-app.md](desktop-app.md).

## What is in this repository

| Path | What it is |
| --- | --- |
| `crates/sync` | Rust crate `lightcraft-cloud-client`: the sync client shared by the apps (talks to the remote through librclone, reads the manifest, fetches thumbnails and previews, sends edits back; the phone app uses it) and, in its `home` module, the PC side: publishing a photo folder, applying the phone's edits, merging sidecars. |
| `apps/desktop` | `lightcraft-cloud-desktop`: the PC app: an always-running tray app (and the command line). Two builds: the full one is the home PC (renders thumbnails and smart previews with LightCraft's engine) or another PC; the light one (`--no-default-features`, no engine) is another PC only. See "Desktop app" and "Another PC" in [desktop-app.md](desktop-app.md). |
| `tools/parity` | The harness that runs the Python tool and the Rust app on the same generated photos and compares everything they write. |
| `legacy/` | The first PC tool, `lc-cloud`, written in Python. `lightcraft-cloud-desktop` replaces it; see [legacy-lc-cloud.md](legacy-lc-cloud.md). |

## Building the Rust part

Rust 1.99.0 (`rust-toolchain.toml` picks it). librclone is loaded at run time, so it is not needed to build.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The desktop app has two builds. The default one (feature `home`) is the full app and needs LightCraft's engine to render
previews. `cargo build --release -p lightcraft-cloud-desktop --no-default-features` is the light build for another PC: the same
tray app, windows and sync client, with no LightCraft crate besides the shared sync crate in `cargo tree`, and a much smaller
binary. Given a config for the home PC it says so and does nothing. Run the tests and clippy for both
(`cargo test -p lightcraft-cloud-desktop --no-default-features`).

The tests that talk to a real rclone need a host build of `librclone.so`. Point `LIBRCLONE` at it, for example
`LIBRCLONE=~/src/lightcraft-android/apps/lightcraft-android/librclone-host/librclone.so cargo test --workspace`
(built by `apps/lightcraft-android/scripts/build-librclone.sh host` in the phone app repository). Without it those tests
print "skipped" and pass.

The sync crate's tests drive the lightcraft-android engine (tag `v0.1.0`, pinned by `rev` in `crates/sync/Cargo.toml`), because the
upstream LightCraft engine does not have the command `library.addRemote` yet. The desktop app keeps the upstream engine
(`v0.15.0`), so `Cargo.lock` holds both. Bump the `rev` when the phone fork's engine changes.

## Windows (light build: another PC)

CI (`.github/workflows/desktop.yml`) builds the light build (`--no-default-features`: no engine, role "another PC") on
`windows-latest` and uploads `lightcraft-cloud-windows-light.zip`: the exe, `librclone.dll` (rclone's librclone built from
[`tools/librclone`](../tools/librclone), rclone v1.75.2, with Go and cgo), the licences and a short `README.txt`. Keep the DLL next to the exe.

* Config `%APPDATA%\lightcraft-cloud-desktop\config.toml`, state and log `%LOCALAPPDATA%\lightcraft-cloud-desktop\`, rclone's config
  `%APPDATA%\rclone\rclone.conf` (or `RCLONE_CONFIG` / `rclone_config`). "Start when I log in" sets a value in
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (no administrator needed).
* The exe is a windows-subsystem program: no console opens for the tray app. In a terminal the commands (`status`, `sync --once`,
  `run --headless`) print as usual, though the prompt may come back before they finish.
* **Telling LightCraft about downloaded photos** (only needed for photos that arrive from the home PC) uses LightCraft's control
  port, which is off by default and which Windows users cannot enable the way the Flatpak does. Either start LightCraft with
  `lightcraft.exe --control 7980` (edit the shortcut's Target; installed in `%LOCALAPPDATA%\Programs\ArtCraft\LightCraft`, or
  `C:\Program Files\ArtCraft\LightCraft` for all users), or set `setx LIGHTCRAFT_CONTROL_PORT 7980` once and restart LightCraft
  ([control protocol](https://github.com/storytold/lightcraft/blob/main/docs/control-protocol.md): loopback only, no authentication).
  The app's `lightcraft_control_port` (default 7980, 0 = off) must match. LightCraft's Windows installer also installs
  `lightcraft-cli.exe`, but this app does not need it: it speaks the control protocol over a TCP socket itself. Without the port the photos
  still land in the photo folder; run Synchronize Folder in LightCraft. The same port is how new phone edits show up in LightCraft by
  themselves (see "Showing new edits in LightCraft" in [desktop-app.md](desktop-app.md)); it also lets the app tell you when LightCraft is not saving edits to sidecars,
  which on Windows is the usual reason edits never reach the phone.

### Parity with the Python tool

```sh
tools/parity/parity.py          # generated photos under /tmp/lc-parity; needs rclone, the LightCraft Flatpak and cargo
```

It generates a photo folder (procedural scenes from LightCraft's `lightcraft-scenes`, a synthetic DNG with a JPEG of the same
stem, sidecars, an identical copy, an excluded folder; no real photos), copies it twice, and runs `lc-cloud sync` on one copy
and `lightcraft-cloud-desktop sync --once` on the other, each with a throwaway config and state folder and a local-folder
remote (the Python tool is given `--config`, so it never reads `~/.config/lightcraft-cloud/`; `RCLONE_CONFIG` is an empty
file). Five rounds: first publish; new, changed and removed photos and edited sidecars; fourteen synthetic phone edits in the
inbox (apply, merge, newest wins both ways, a chain of edits, an identical edit, edits for the raw and the JPEG of a pair, a
crs attribute-style sidecar, and three kinds of reject); publishing what the inbox wrote; a quiet run. After each it compares
the remote file lists, the manifests field by field, thumbnails (JPEG, dimensions) and smart previews (header), the state rows,
the version stores and, after the inbox, the sidecars byte for byte, the local history and the inbox lines of the log. A last
check lets the Rust app take over the Python tool's remote with a fresh state folder (nothing is uploaded again). Because the
Flatpak is the same engine release, thumbnails and smart previews come out byte-identical as well; the harness only requires
the same format and dimensions.

Result of the last run: no unexpected difference. Intended ones: `publisher` in the manifest (`lightcraft-cloud-desktop <version>`
instead of `lightcraft-cloud 0.1.0`) and `generated`.

Tests: `cargo test --workspace` runs about a hundred tests of the home side against a folder standing in for the remote with a fake
renderer (ports of the Python tool's scenarios: scan rules, exclude, pairing, manifest, inbox, merge, versions, dry run, safety,
publishing and history), plus tests with the real engine in `apps/desktop/tests` and, when `LIBRCLONE` is set, through librclone.
