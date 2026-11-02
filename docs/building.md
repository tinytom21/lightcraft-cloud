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
previews. It builds and runs on Linux and on Windows (x86-64, MSVC). `cargo build --release -p lightcraft-cloud-desktop --no-default-features` is the light build for another PC: the same
tray app, windows and sync client, with no LightCraft crate besides the shared sync crate in `cargo tree`, and a much smaller
binary. Given a config for the home PC it says so and does nothing. Run the tests and clippy for both
(`cargo test -p lightcraft-cloud-desktop --no-default-features`).

The tests that talk to a real rclone need a host build of `librclone.so`. Point `LIBRCLONE` at it, for example
`LIBRCLONE=~/src/lightcraft-android/apps/lightcraft-android/librclone-host/librclone.so cargo test --workspace`
(built by `apps/lightcraft-android/scripts/build-librclone.sh host` in the phone app repository). Without it those tests
print "skipped" and pass.

The sync crate's tests drive the lightcraft-android engine (tag `v0.1.0`, pinned by `rev` in `crates/sync/Cargo.toml`), because the
upstream LightCraft engine does not have the command `library.addRemote` yet. The desktop app keeps the upstream engine
(`v0.17.0`), so `Cargo.lock` holds both. Bump the `rev` when the phone fork's engine changes. Changing the app's engine version
re-renders nothing by itself (see "Rendering" in [desktop-app.md](desktop-app.md)).

**Tests that are not portable.** `cargo test --workspace` is run on Windows and on Linux by CI (`desktop.yml`, job "full build and all tests"), with no
test skipped by default: a test that cannot run prints `skipped:` or `SKIPPED:` and the job fails on the word. Tests that make symbolic links need the
privilege to (a CI runner has it; `LC_REQUIRE_SYMLINKS=1` turns a missing privilege into a failure instead of a printed skip). What is limited to one system, and why:

| Test | Runs on | Why |
| --- | --- | --- |
| `crates/sync/tests/home_windows.rs` (a sidecar or photo another program holds) | Windows | Linux has no mandatory file locks, so nothing there can be held. |
| `other_import.rs: a_destination_the_home_pc_would_reject...` (`CON`, `a:b`, `ends.` as folder names) | Linux | Windows cannot create such folders, so no photo can be in one; the rule itself is tested on every system by `safe_rel_on` and the home import tests. |
| `src/pending.rs` `mod tests` (written with POSIX paths) | Linux | They are not absolute paths on Windows; the module `windows_tests` beside it covers the same rules with Windows paths. |
| `src/autostart.rs` | Linux: the `.desktop` file; Windows: the `Run` key (in a registry key of the test's own) | The two systems store "start when I log in" differently. |
| `src/app.rs` `fake_exe` | Linux: a shell script; Windows: the `fake_window` example | A script cannot be started by `CreateProcess` as a program. |
| `config.rs` `a_tilde_means...` | both, with different results for `~\` | A backslash is an ordinary character in a Linux file name. |
| `util.rs` Windows tests (long paths, a held file) | Windows | The Win32 calls they test do not exist elsewhere. |

The slow engine tests (`apps/desktop/tests/engine.rs`, `home_e2e.rs`) render with the CPU on both systems (`LIGHTCRAFT_GPU=0` in CI).

## Windows (both builds)

`.github/workflows/release.yml` builds both builds on `windows-latest` (`--no-default-features`: the light build, no engine, role "another PC"; and the default features: the home
build with the engine) and packs each into an installer with Inno Setup (one script, `packaging/windows/lightcraft-cloud.iss`; `/DHome` makes the home one):
`lightcraft-cloud-<version>-windows-setup.exe` (light; **this name must never change**, every v0.2.0 app's updater looks for it) and `lightcraft-cloud-<version>-windows-home-setup.exe`
(the `windows-latest` image ships Inno Setup 6.7.1, which CI uses and prints;
if the image ever lacks it, CI installs 6.6.1 with Chocolatey; the script needs 6.3 or newer). Each installer is per user (no administrator): `%LOCALAPPDATA%\Programs\LightCraft Cloud\` with the
exe, `librclone.dll` (rclone's librclone built from [`tools/librclone`](../tools/librclone), rclone v1.75.2, with Go and cgo), the licences and
`README.txt` (`packaging/windows/README.txt` for the light one, `README-home.txt` for the home one). `desktop.yml` builds, lints and tests both builds on every push (and the whole workspace's tests on Windows
and Linux), but makes no zip.

* **One identity, two editions.** Both installers have the same `AppId`, folder, Start menu entry, Run value and uninstall entry; the installer writes `edition.txt` (`home` or `light`) next to the exe,
  and `--version` says the same on its second line. Installing the home installer over the light app is an upgrade. The light installer over the home app is **refused** by `InitializeSetup` (nothing is
  changed, the exit code is 1) because it would replace the engine and silently stop the home PC; an install from v0.2.0 has no `edition.txt` and counts as light. After an uninstall (which removes
  `edition.txt`) either goes on. `test-installer.ps1 -Variant light|home -OtherSetup <the other installer>` tests all of that on the runner. The same-AppId choice means one program per PC (the
  instance lock would stop a second copy anyway) and a plain upgrade path from "other PC" to "home PC".
* **The C runtime is inside the exe.** `.cargo/config.toml` sets `-C target-feature=+crt-static` for `x86_64-pc-windows-msvc`, so `cargo build --release -p lightcraft-cloud-desktop` (either build) needs no
  Visual C++ Redistributable on a clean PC. CI proves it per build: `tools/ci/pe_imports.py` lists the exe's imports (and delay-loaded ones) and fails on any `vcruntime*`, `msvcp*`, `ucrtbase` or
  `api-ms-win-crt-*` import, and on any DLL that is not a file of `System32` (API sets excepted).

* The installer stops a running app with the new exe's `quit` command, replaces the files, and starts the app again; if the install
  fails it starts the old one. `lightcraft-cloud-desktop quit [--wait SECONDS]` writes `<state>/quit-request`; the running app notices it
  within two seconds, deletes it and ends after the job it is running. Exit codes: 0 stopped, 10 not running, 11 still running after
  the wait (default 60 s), 12 not answered (an app from before `quit` existed). The installer ends what is left with `taskkill` after 11 and 12.
  A sync survives that: files are written to a temporary name and renamed, the manifest is written last, the run lock is an OS file lock that
  goes with the process, and the next sync carries on. Leftover `.part-*` files are removed after a day.
* "Start when I log in" is a task, on by default. It writes the same value as the Settings box, `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
  `LightCraft Cloud` = `"<exe>" run`. A silent run (the updater's) keeps what the user has: a value that is there is rewritten to the new
  location, one that is not stays off.
* Uninstall removes the program files and that value. It keeps `%APPDATA%\lightcraft-cloud-desktop`, `%LOCALAPPDATA%\lightcraft-cloud-desktop` and
  `%APPDATA%\rclone`, and never looks at a photo folder.
* `packaging/windows/test-installer.ps1` is the test CI runs on the real Windows runner (silent install to the default place, `--version`,
  the Run value, the Start menu and Apps & features entries, install again over a running copy, `quit`, a Run value that was turned off or
  points at the zip, silent uninstall, planted files that must survive). A runner has no tray session, so it cannot show the tray icon, a click, the wizard's
  pages or SmartScreen.
* Config `%APPDATA%\lightcraft-cloud-desktop\config.toml`, state and log `%LOCALAPPDATA%\lightcraft-cloud-desktop\`, rclone's config
  `%APPDATA%\rclone\rclone.conf` (or `RCLONE_CONFIG` / `rclone_config`).
* The exe is a windows-subsystem program: no console opens for the tray app. In a terminal the commands (`status`, `sync --once`,
  `run --headless`, `quit`) print as usual, though the prompt may come back before they finish.
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

## Releasing

A release is made from a commit on `main` whose `desktop.yml` run is green. Nothing publishes by itself.

1. Set the version in `Cargo.toml` (`[workspace.package] version`; the desktop crate and the sync crate follow it), run `cargo check` so `Cargo.lock` follows, update the docs, merge.
2. Tag it and push the tag: `git tag v0.2.0 && git push origin v0.2.0`. The tag must be `v` plus the Cargo version; the workflow stops otherwise.
3. `release.yml` builds, on real runners: the two Windows installers (and runs `test-installer.ps1` against each, with the other as `-OtherSetup`), `linux-light` and `linux-home` on
   Ubuntu 22.04 (the oldest runner, so glibc 2.35 is the minimum; the job prints the real minimum and fails if it is higher; each archive's `install.sh` is
   run in a throwaway home folder), and `SHA256SUMS` of the four files. Then it creates a **draft** release with those five files and generated notes.
   A tag with a `-` in it (`v0.3.0-rc.1`) makes a draft prerelease.
4. Open the draft on GitHub, read the notes, try the installer on a real Windows desktop (tray icon, Status window, SmartScreen), and publish it by hand.
   The in-app updater only sees published releases (never drafts or prereleases), and it needs, for the light app, `lightcraft-cloud-<version>-windows-setup.exe` and, for the home app,
   `lightcraft-cloud-<version>-windows-home-setup.exe`, with `SHA256SUMS`, in the release; an app offers nothing when the file of its own kind is missing.

`workflow_dispatch` runs the same builds and checks and publishes nothing. The updater reads
`https://api.github.com/repos/tinytom21/lightcraft-cloud/releases/latest`; its address, and the address installers are downloaded from, are constants in `apps/desktop/src/update.rs`
(there is deliberately no setting, environment variable or option for them; the tests use a local server through a function only tests can call).

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
