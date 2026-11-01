# LightCraft Cloud

LightCraft Cloud keeps your photo library in step between your PCs and your phone. The PC is home: it holds the
original photos and is the only place they are ever changed. Each PC publishes its photo folder to any
[rclone](https://rclone.org) remote (Google Drive, S3, WebDAV, ...) together with thumbnails and small smart previews.
The LightCraft phone app browses that library, downloads a preview to edit, and sends the edits back as XMP sidecars, which
the PC then applies. Originals are only ever mirrored one way, and photo files are never modified. Other PCs can keep a copy of
the folders you choose and send their edits back the same way.

Remote layout and the inbox protocol: [FORMAT.md](FORMAT.md).
Phone app: https://github.com/artcraft-android/lightcraft-android (cloud library support is a pending PR).

## What is in this repository

| Path | What it is |
| --- | --- |
| `crates/sync` | Rust crate `lightcraft-cloud-client`: the sync client shared by the apps (talks to the remote through librclone, reads the manifest, fetches thumbnails and previews, sends edits back; the phone app uses it) and, in its `home` module, the PC side: publishing a photo folder, applying the phone's edits, merging sidecars. |
| `apps/desktop` | `lightcraft-cloud-desktop`: the PC app: an always-running tray app (and the command line). Two builds: the full one is the home PC (renders thumbnails and smart previews with LightCraft's engine) or another PC; the light one (`--no-default-features`, no engine) is another PC only. See "Desktop app" and "Another PC" below. |
| `tools/parity` | The harness that runs the Python tool and the Rust app on the same generated photos and compares everything they write. |
| `legacy/` | The current PC tool, `lc-cloud`, written in Python. It is still what runs the sync today; see "Legacy tool" below. |

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

## Desktop app (`lightcraft-cloud-desktop`)

The Rust port of what `lc-cloud` does on the PC, writing the same remote layout ([FORMAT.md](FORMAT.md)), so the phone app
works with either. It has its own config and state; it never reads or writes the Python tool's
(`~/.config/lightcraft-cloud/`, `~/.local/state/lightcraft-cloud/`), and refuses a `state_dir` inside the latter.

```
lightcraft-cloud-desktop [run] [--headless] [--config FILE]        the always-running app (below)
lightcraft-cloud-desktop sync --once [--dry-run] [--config FILE]   apply phone edits, then publish the photo folder
lightcraft-cloud-desktop status [--config FILE]                    last run, tracked photos, remote size, pending inbox files
lightcraft-cloud-desktop inbox-check [--config FILE]               how many phone edits wait in the cloud inbox
lightcraft-cloud-desktop purge-history [--days N] [--config FILE]  delete old cloud history, only after a recent backup
```

`sync --once` does what `lc-cloud sync` does, in the same six steps: the inbox (three-way merge of phone edits into the
sidecars, every replaced version kept in `<state>/history/`), the scan (excludes, raw+JPEG pairs, sidecar naming, files still
being written are left for the next run), thumbnails and smart previews for new and changed photos, a one-way mirror of the
originals (anything replaced or removed on the remote goes to `history/<date>/` with a `.HHMMSS` suffix, `max_delete` guards
against a missing drive), the mobile files, and the manifest last. `--dry-run` changes nothing.

### The always-running app

`lightcraft-cloud-desktop` with no command (or `run`) replaces the Python tool's systemd timers, watcher and notifications:

* a full sync every 15 minutes (counted from the end of the last one), the first 20 seconds after the start;
* a cheap check of the cloud inbox every 2 minutes (one listing, nothing else), which starts a sync only when phone edits wait
  (`inbox-check`);
* a folder watcher (inotify through the `notify` crate) that starts a sync once the photo folder has been quiet for 30 seconds
  (5 minutes after the first change at the latest). Hidden files and excluded folders do not count;
* a weekly `purge-history`: the remote's `history/` and `inbox-done/` day folders older than `device_history_days` (30), and the
  stored sidecar versions older than that, except those of current sidecars. The same safety rule as the Python tool: it only
  runs if `photo_backup snapshots --latest 1 --json` (restic) says the newest backup is at most 48 hours old; otherwise nothing
  is deleted and the Status window says so. Set `photo_backup = ""` to switch the purge off;
* Sync now, Pause/Resume (remembered across restarts; "Sync now" still works while paused), one sync at a time (the sync's run
  lock, so `sync --once` and the app never run together), one app at a time (`<state>/instance.lock`: a second launch says so
  and exits);
* desktop notifications (`notify-rust`, over D-Bus; off with `notify = false` or the checkbox in Settings), with the Python
  tool's texts: "N phone edits applied (M merged with PC changes) in <folders> - in LightCraft run Synchronize Folder ...",
  and "Publisher failed - see the Status window or the log" (once per distinct error, not every 15 minutes). A phone edit that
  lost to a newer PC sidecar is announced too, and listed under "Newest wins" in the Status window.

The tray icon (idle green, syncing blue, phone edits waiting amber, paused grey, error red) has the menu Sync now, Pause/Resume,
Status..., Settings..., Quit. On Linux it is a StatusNotifierItem over D-Bus (`tray-icon`'s `ksni` backend: no GTK or
libappindicator to install). The desktop needs a tray host: KDE has one; GNOME needs the AppIndicator extension (Ubuntu ships
it). The windows (egui 0.37, the version LightCraft v0.15.0 uses) run as `lightcraft-cloud-desktop ui status|settings`, in a
process of their own, so a problem in a window cannot stop the sync; they read `<state>/status.json` and write the config file,
which the app notices within two seconds.

* **Setup / Settings**: the photo folder (text field + Browse), folders to leave out, the storage (one of your rclone remotes,
  listed by name only, plus a folder on it; or a folder on this PC), "Originals and previews" or "Previews only", notifications,
  "Start when I log in". With no config the app opens Setup instead of syncing; saving writes the config file (keys the app does
  not know are kept) and the app starts syncing.
* **Status**: last sync (time, result), photos published, phone edits applied, the inbox, the next scheduled sync, "Newest wins"
  decisions, problems.
* "Start when I log in" writes `~/.config/autostart/lightcraft-cloud-desktop.desktop` (`Exec=<this binary> run`) only when the
  box is ticked in Settings, and removes it when unticked. Nothing else writes there.

**Secrets, for now**: the app uses your existing rclone config (`rclone_config`, default the standard one), like the Python tool.
It lists remote names only and never shows, logs or copies the config's contents. Storing the cloud credentials in the system
keyring arrives with sign-in / pairing.

One photo folder per app for now (the sync, its state and the manifest are per folder).

### Another PC

A second PC (a laptop, say) keeps a full copy of the folders you choose, in a photo folder of its own, and sends its edits to the
home PC. The home PC stays the master: it publishes, merges and deletes; the other PC never writes the cloud library's `originals/` or
`mobile/` (its edits travel through `inbox/<device>/`, like a phone's, see [FORMAT.md](FORMAT.md)).

Set it up in Setup: **This is another PC**, pick where the cloud library is, the folder on this PC for the photos, and tick the
folders to keep (the list is read from the cloud library, with each folder's photos and size). In the config:

```toml
role = "other"
source = "~/Pictures/LightCraft Cloud"   # the local photo folder: the chosen folders are downloaded here
remote = "gdrive:LightCraft Cloud"
folders = ["261101 - Lisbon", "2025/Porto"]   # relative to the library's originals/; the photos directly in each
device = "laptop"                          # optional; default: this PC's host name
```

Add the photo folder to LightCraft on that PC, with LightCraft's automatic writing of sidecars on. Every 15 minutes, and 30 seconds
after a sidecar or a photo file changes (5 minutes at the most, the same watcher rule as the home PC), the app:

* downloads what is missing from the chosen folders (originals, sidecars, raw+JPEG partners), checks each file against its content
  hash, never replaces a file, and keeps going where it left off if the network drops. It checks the free disk space first and says
  so when it is short;
* sends a sidecar LightCraft changed here to the inbox (once), and takes the home PC's newer sidecar when this PC has no edit
  waiting (the old one is kept in `<state>/history/`). If both changed, the edit here goes first and the file here is left alone
  until the home PC's merge comes back, so nothing you did here is lost;
* moves a photo the home PC deleted (with its sidecar) to `<state>/removed/<date>/`, never deleting it. It does that only for a
  file that is still exactly what was received, and refuses a pass that would move more than `max_delete` photos;
* tells your running LightCraft to add the photos it downloaded (the same list and link as on the home PC, see below).

* **sends the photos you import on this PC.** Import in LightCraft, into this photo folder (set LightCraft's import destination to
  it), in whatever folders you like: the app notices new photo files at any depth, waits until a file has stopped changing for two
  minutes (LightCraft may still be copying), and sends it to the home PC with its folder kept (`2026/261101 - Lisbon/Day 1/DSC1.ARW`
  arrives at the same path; a sidecar beside it goes along), then adds each folder to the folders kept on this PC. It does not tell
  LightCraft about them: LightCraft imported them. Photos imported anywhere else are not uploaded. Nothing is ever sent that the
  cloud library has or ever had, so a photo deleted at home does not come back, even from a restored file; the same photo at two
  paths goes once. **The photos already in the folder when the app first looks are not sent by themselves** (a wrong folder choice
  must not upload a disk): Status says how many of them are not in the library, and "Send them" queues them. A photo the home PC
  refuses stays here and is counted in Status until you choose "Try again" or "Keep on this PC only". Uploads run in short jobs of
  their own, stop while paused, and wait when offline.

**Deleting on this PC** only affects this PC. A photo you delete here is not downloaded again, even when the home PC changes it.
To get it back, untick its folder in Settings, save, then tick it again and save. A folder you untick is no longer updated; its
photos stay where they are.

What LightCraft has to do: a **new photo** is added through its control port. A **sidecar the app replaced** is a file LightCraft
did not write, so use LightCraft's own "re-read changed sidecars" (Synchronize Folder) to see it; the app has no command to ask
LightCraft to do that for you. A photo moved to `removed/` leaves a missing file in LightCraft; remove it there.

The tray and the pause rules are the same. Being offline is a calm state: one line in Problems, no notification, a retry at the
next pass. The app never looks at other devices' edits in the inbox. The device name must not be a phone's: the first pass refuses a
name that already has files in `inbox/`.

Limits for now: a folder means the photos directly in it (tick each subfolder you want); a photo moved by the home PC stays where
this PC has it; if the home PC keeps its own sidecar instead of applying yours, your edit stays here until its sidecar changes
again; the free-disk-space check is Linux only so far.

### Config

Default `~/.config/lightcraft-cloud-desktop/config.toml` (`$XDG_CONFIG_HOME` is honoured), or `--config FILE`. TOML; unknown keys
are ignored, so a Python config can be reused. State in `~/.local/state/lightcraft-cloud-desktop/` (or `state_dir`): `state.json`,
`sidecar-versions/`, `history/`, `mobile/` (the staged files as uploaded), `lib/` (the engine's scratch library), `logs/`.

| Key | Default | Meaning |
| --- | --- | --- |
| `remote` | **none, required** | an rclone `remote:path` (a crypt remote works: `lccrypt:`), or a local folder (starts with `/`, `~` or `.`; for tests). There is deliberately no default: nothing is published to a remote nobody named. |
| `role` | `"home"` | `"home"` (publishes) or `"other"` (keeps a copy of some folders; see "Another PC") |
| `source` | `~/Pictures/Photos` | folder to publish; on another PC, the local photo folder |
| `folders` | `[]` | role `other`: the cloud library folders to keep, relative to `originals/` |
| `device` | the host name, made valid | role `other`: this PC's name in `inbox/<device>/`; not a phone's name |
| `exclude` | `[]` | folder-name patterns (fnmatch, case-sensitive) skipped at any depth |
| `pair_primary` | `"jpeg"` | which file of a raw+JPEG pair is the photo (`"jpeg"` or `"raw"`); the other becomes an alternate |
| `max_delete` | `50` | refuse when more published photos vanished than this since the last run; also the most files one mirror removes |
| `thumb_px`, `thumb_quality` | `512`, `80` | thumbnail long edge and JPEG quality |
| `previews_only` | `false` | upload sidecars, thumbnails, smart previews and the manifest, but not the photo files |
| `notify` | `true` | desktop notifications (the app) |
| `device_history_days`, `photo_backup` | `30`, `"restic"` | the weekly purge: age limit; the backup command it asks (`photo_backup snapshots --latest 1 --json`); `""` switches the purge off |
| `lightcraft_control_port` | `7980` | the port of your running LightCraft's control channel (see "Telling your LightCraft about new photos"); `0` switches it off |
| `state_dir` | `~/.local/state/lightcraft-cloud-desktop` | must not be under `~/Pictures` |
| `rclone_config` | `$RCLONE_CONFIG`, else `~/.config/rclone/rclone.conf` | rclone config used for an rclone remote |
| `librclone` | `$LIBRCLONE`, else `librclone.so` next to the executable, else `~/.local/lib/lightcraft-cloud/librclone.so`, else the system search path | the host `librclone.so` (see "Building the Rust part"); the desktop app looks in that order, so an installed app needs the library in one of those places |

### Telling your LightCraft about new photos

When another PC sends photos (FORMAT.md, "Imports") and the sync adds them to the photo folder, the app can ask the LightCraft
you have open to add them to its library, so they appear without a manual import. It does this through LightCraft's control
channel, which is off until you turn it on. For the Flatpak:

```sh
flatpak override --user --env=LIGHTCRAFT_CONTROL_PORT=7980 ai.storyteller.lightcraft
```

(or start LightCraft with `--control 7980`; any other port works if you set `lightcraft_control_port` to match). **The channel
listens on `127.0.0.1` only and has no password:** any program running as you on this PC can drive LightCraft through it, so
turn it on only if you accept that. The app never turns it on, and `lightcraft_control_port = 0` switches the feature off.

What the app does with it, and nothing else:

* the photos the sync adds are written to a list in the state folder (`lightcraft-pending/`) before anything is sent, so a crash
  or restart loses none;
* every 2 minutes, and right after a sync that added photos, if the list is not empty it checks that what answers on the port is
  LightCraft (`engine.commands`) and then sends `library.import` with mode `add` and the photos' paths, ten at a time. `add` means
  LightCraft keeps a reference to each file where it is: nothing is copied, moved, or deleted. No other command is ever sent;
* a photo leaves the list only when LightCraft's reply says it has it (added, or already in the library). One it refuses is
  retried and logged; after ten tries it is left alone, **Problems** in the Status window says so once, and it is tried once
  more the next time the app starts;
* LightCraft closed, or the channel off: the list is kept and the Status window shows "Waiting for LightCraft: N photos" with a
  hint that the port has not answered. The photos go in the next time LightCraft opens with the channel on;
* the app never starts LightCraft, never passes `--library`, never opens or names your library: LightCraft adds the photos to
  whichever library it has open. The Flatpak can only see some folders (`xdg-pictures` by default): a photo folder it cannot see
  comes back as a failure ("No such file"), so give it access (`flatpak override --user --filesystem=<folder> ai.storyteller.lightcraft`).

### Rendering

Thumbnails and smart previews come from LightCraft's engine (`lightcraft-engine`, pinned to `v0.15.0`, no `sam` and no
`denoise` features), in this process, through the same commands the Python tool sends to `lightcraft-cli`: `library.import`
(mode `add`), `photo.inspect`, `library.smartPreviews`, `app.export` (JPEG, long edge `thumb_px`, quality 80, sRGB, with the
photo's current edits) and, for a changed sidecar, `photo.readMetadataFromFile`. `previewKey` is the engine's own name for the
smart preview (`lightcraft_engine::smart::file_name`, a hash of the content hash). The sync crate does not depend on the
engine: it calls a `Renderer` trait, and `apps/desktop` supplies the engine-backed one.

Render-only is enforced, not assumed: the engine works on a scratch library in `<state>/lib` (writing XMP next to the photos is
switched off), the app refuses a library folder under `~/Pictures` or named `LightCraft Library` and has no setting to point it
elsewhere, and the tests check that the source folder is byte- and mtime-identical after a sync. The user's LightCraft library
is never opened, locked or written.

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

## Legacy tool (`lc-cloud`)

The current PC side, still in use. Publish a folder of photos as a LightCraft cloud library. `lc-cloud` runs on your PC, mirrors the folder to any
[rclone](https://rclone.org) remote (Google Drive, S3, WebDAV, ...), and writes thumbnails, smart previews and a manifest
next to it. The LightCraft phone app browses that library, downloads a small preview to edit, and sends the edits back
as XMP sidecars. `lc-cloud` pulls them into the matching sidecar on the PC. The PC stays the master copy: originals are
only ever mirrored one way, and photo files are never modified.

Phone app: https://github.com/artcraft-android/lightcraft-android (cloud library support is a pending PR).
Remote layout and the inbox protocol: [FORMAT.md](FORMAT.md).

### Requirements

- Linux with Python 3.11+ (`tomllib`)
- [rclone](https://rclone.org) with a configured remote
- the LightCraft Flatpak (`ai.storyteller.lightcraft`), whose `lightcraft-cli` renders thumbnails and previews
- optional: `restic` (or a wrapper), only for `purge-history`
- optional: `inotify-tools`, only for `lc-cloud-watch`; `notify-send` for desktop notifications

### Install

```sh
git clone https://github.com/tinytom21/lightcraft-cloud ~/src/lightcraft-cloud
cd ~/src/lightcraft-cloud
mkdir -p ~/.local/bin ~/.config/lightcraft-cloud ~/.config/systemd/user
ln -sfn "$PWD/legacy/lightcraft_cloud.py" ~/.local/bin/lc-cloud
ln -sfn "$PWD/legacy/lc-cloud-watch" ~/.local/bin/lc-cloud-watch
cp legacy/config.example.toml ~/.config/lightcraft-cloud/config.toml   # edit it
cp legacy/units/* ~/.config/systemd/user/ && systemctl --user daemon-reload
lc-cloud sync --dry-run
```

The units call `%h/.local/bin/lc-cloud`. None is enabled by default:

| Unit | What it does |
| --- | --- |
| `lightcraft-cloud.timer` | runs `lc-cloud sync` every 15 minutes: publish, then apply phone edits |
| `lightcraft-cloud-watch.service` | `lc-cloud-watch`: runs a sync about 30 s after the photo folder changes (the timer stays as fallback) |
| `lightcraft-cloud-inbox.timer` | every 2 minutes, one cheap listing of the remote inbox; starts a sync only if phone edits are waiting |
| `lightcraft-cloud-purge.timer` | weekly `lc-cloud purge-history` |
| `lightcraft-cloud-notify.service` | desktop notification when a sync or purge fails (used via `OnFailure=`) |

Enable what you want, e.g. `systemctl --user enable --now lightcraft-cloud.timer lightcraft-cloud-inbox.timer`.

### Commands

```
lc-cloud [--config FILE] sync [--dry-run]   publish originals, previews, thumbs and manifest; apply phone edits
lc-cloud status                             last run, tracked photos, remote size, pending inbox files
lc-cloud purge-history [--days N]           delete remote history/ and inbox-done/ folders, and stored sidecar versions, older than N days
lc-cloud inbox-check                        start a sync only if phone edits are waiting (used by the inbox timer)
```

Config file: `--config FILE`, else `$LC_CLOUD_CONFIG`, else `~/.config/lightcraft-cloud/config.toml`.
Environment: `LC_INBOX_SYNC_CMD` (command `inbox-check` runs; default `systemctl --user start --no-block lightcraft-cloud.service`),
`LC_WATCH_CMD`, `LC_WATCH_QUIET` (default 30 s), `LC_WATCH_MAX` (default 300 s) for `lc-cloud-watch`.

### Config reference

See [legacy/config.example.toml](legacy/config.example.toml). All keys are optional.

| Key | Default | Meaning |
| --- | --- | --- |
| `role` | `"home"` | `"home"` (publishes) or `"other"` (keeps a copy of some folders; see "Another PC") |
| `source` | `~/Pictures/Photos` | folder to publish; on another PC, the local photo folder |
| `folders` | `[]` | role `other`: the cloud library folders to keep, relative to `originals/` |
| `device` | the host name, made valid | role `other`: this PC's name in `inbox/<device>/`; not a phone's name |
| `remote` | `gdrive:LightCraft Cloud` | rclone `remote:path`; a crypt remote works too (see "Encrypting the remote") |
| `exclude` | `[]` | folder-name patterns (fnmatch, case-sensitive) skipped at any depth |
| `pair_primary` | `"jpeg"` | which file of a RAW+JPEG pair is primary (`"jpeg"` or `"raw"`); the other becomes an alternate |
| `max_delete` | `50` | abort if more source photos vanished than this since the last run; also passed to `rclone --max-delete` |
| `device_history_days` | `30` | age limit for `purge-history` |
| `thumb_px` | `512` | thumbnail long edge |
| `notify` | `true` | desktop notification when phone edits were applied or merged |
| `state_dir` | `~/.local/state/lightcraft-cloud` | scratch library, state database, logs |
| `rclone` | `rclone` from PATH, else `~/.local/bin/rclone` | rclone binary |
| `lightcraft_cli` | unset | path to a `lightcraft-cli`; unset runs it through `flatpak run --command=lightcraft-cli ai.storyteller.lightcraft` |
| `photo_backup` | `restic` | command that answers `snapshots --latest 1 --json`; point it at a wrapper if your repository needs flags or environment |

With the Flatpak, `lc-cloud` grants it `--filesystem=<source>:ro` and `--filesystem=<state_dir>` for each call, so no
manual Flatpak override is needed.

### Encrypting the remote

`lc-cloud` and the phone app reach the library only through rclone, so an rclone [`crypt`](https://rclone.org/crypt/)
remote can encrypt everything on your side before it reaches the provider. Neither side has a setting for it: both open
the crypt remote instead of the plain one, and [FORMAT.md](FORMAT.md) describes what you see through it. Publishing,
thumbnails, previews, phone edits, the three-way merge and `purge-history` were tested through a crypt remote over a
local folder and over Google Drive.

#### On the PC

1. Create a new, empty folder on the provider for the encrypted tree, e.g. `LightCraft Cloud Encrypted`. Do not use the
   folder of an existing plain library.
2. Create the crypt remote with `rclone config` (`n`, name `lccrypt`, storage `crypt`) and answer:

   | Option | Answer |
   | --- | --- |
   | `remote` | the plain remote and the new folder, e.g. `gdrive:LightCraft Cloud Encrypted` |
   | `filename_encryption` | `standard` |
   | `directory_name_encryption` | `true` |
   | `password`, `password2` | `g` (generate); rclone shows each one once |

   The same without prompts (`--obscure` stores the passwords the way rclone expects):

   ```sh
   rclone config create lccrypt crypt remote="gdrive:LightCraft Cloud Encrypted" \
     filename_encryption=standard directory_name_encryption=true \
     password="$(openssl rand -base64 32)" password2="$(openssl rand -base64 32)" --obscure
   ```
3. **Back up the passwords** (both) outside this PC, e.g. in a password manager: the ones rclone showed when it generated
   them, or a copy of the `[lccrypt]` section of the rclone config (`rclone config file` shows where; the passwords in it are
   obscured, not encrypted, so keep the copy private).
   Without them nobody can read the cloud copy, you included. The PC holds the originals and is not affected: you lose only
   the cloud copy, and recover by creating a new crypt remote and publishing to it again (see "Switching" below).
4. Set `remote = "lccrypt:"` in `~/.config/lightcraft-cloud/config.toml`, then publish and check:

   ```sh
   lc-cloud sync --dry-run && lc-cloud sync
   rclone lsf -R lccrypt: | head                          # originals/..., mobile/manifest.json: the FORMAT.md layout
   rclone lsf -R "gdrive:LightCraft Cloud Encrypted" | head   # only random-looking names
   ```

`lc-cloud` runs `rclone` with its usual config lookup. For another config file, set `rclone` in `config.toml` to a small
wrapper script that adds `--config FILE`, or set `RCLONE_CONFIG` for the systemd units. Keep the passwords in the rclone
config, not inline in `remote` (a `:crypt,...:` connection string works, but `lc-cloud status` and error messages print `remote`).

#### On the phone

Make the phone-only drive remote as described in the app's cloud-library guide, with `root_folder_id` set to the id of the
**encrypted** folder (the one with the random names; its address ends in `/folders/<id>`):

```sh
rclone config create phone drive scope=drive root_folder_id=<id>
```

Then add a crypt section over it that has the same passwords as `lccrypt`. `rclone config show lccrypt` prints
`*** ENCRYPTED ***` instead of the passwords, and the app cannot decrypt with that, so copy the section from the config file
instead (use your remote's name for `lccrypt`):

```sh
umask 077
{ rclone config show phone
  echo
  awk '/^\[/{p=($0=="[lccrypt]")} p' "$(rclone config file | tail -1)" | sed 's/^remote = .*/remote = phone:/'
} > phone-rclone.conf
```

Copy `phone-rclone.conf` to the phone and, in the app, pick the remote `lccrypt:` (leave the folder empty), not `phone:`, which
only holds encrypted names. The file now holds the Drive token **and the encryption key**: keep it private, and delete it from
the PC and from wherever you transferred it (a share, a USB drive, a messenger) once the app has its own copy.

#### What is hidden

- **Hidden from the provider:** the contents of every file (photos, sidecars with your edits, thumbnails, previews, the
  manifest with paths, camera, lens and dates), and all file and folder names, including `originals`, `mobile`, `inbox`,
  `history` and the phone names under `inbox/`.
- **Still visible to it:** the number of files and folders and the folder structure; file sizes (each file is 32 bytes plus 16
  per 64 KiB larger, under 0.1 % in the test) and modification times; when files change and from where you connect; that the
  files are rclone crypt (each starts with `RCLONE\0\0`); name lengths to within 16 bytes.

The provider's own previews, search and duplicate detection see only opaque files (Google Drive listed every file as
`application/octet-stream`), and you cannot browse the library in its web interface.

#### Name length

Encrypted names are longer: with the default `filename_encoding` (base32) a name of *n* bytes (UTF-8, so `Å` is 2) becomes
`ceil(8 * 16 * (n div 16 + 1) / 5)` characters, and every folder level grows the same way. Measured:

| Plain name | Encrypted name | `originals/2026/Home/<name>` | base32768: name / path |
| --- | --- | --- | --- |
| 12 bytes (`IMG_0101.JPG`) | 26 | 107 | 9 / 39 |
| 53 bytes | 103 | 184 | 35 / 65 |
| 143 bytes | 231 | 312 | 77 / 107 |
| 144 bytes | 256 | 337 | 86 / 116 |

Paths under `history/<date>/` add about 80 characters (`history/2026-10-28/originals/2026/Home/IMG_0101.003440.JPG` is
187). `lc-cloud` keeps replaced and deleted files in `history/` with a 7-byte `.HHMMSS` suffix, so a name has to be at most
136 bytes for the file to be replaceable.

A backend with a 255-character name limit (OneDrive; local disks and most SFTP servers count 255 bytes) rejects a 144-byte
name, or a 137-byte one when it has to go to `history/`. `lc-cloud sync` then stops at step 4 with `file name too long`: the
other originals are uploaded, but no thumbnails, previews or manifest, until the name is shortened. OneDrive also limits paths
to 400 characters. If you have long names or deep folders there, create the crypt remote with
`filename_encoding = base32768` (an advanced option; it counts UTF-16 characters, 15 bits each, so by calculation names up to
about 450 bytes fit). It cannot be changed later without publishing again, and it does not help on a local disk, which counts
bytes (three per character). Google Drive accepted the 256-character names and the longest path of the test, 391 characters;
the default encoding is fine there.

#### Switching an existing library

Encryption belongs to the remote, so files already on the provider are not encrypted in place; you publish again to a new
folder:

1. Let the phone's waiting edits arrive: push them on the phone, then `lc-cloud sync` until `lc-cloud status` shows
   `inbox: 0 pending`.
2. Create the crypt remote over a new folder (above) and set `remote = "lccrypt:"`. Keep `state_dir`: it holds the thumbnails,
   previews and the sidecar versions the merge needs.
3. `lc-cloud sync` uploads originals, thumbnails, previews and the manifest to the new remote. It is a full upload, as long
   as the first publish was.
4. Give the phone the new config file and pick the new remote. It downloads thumbnails and previews again.
5. When it works, delete the old plain folder (`rclone purge`); the new remote starts without the old `history/`.

### Phone edits that meet PC edits

When you edit a photo on the PC while the phone edits the same photo, `lc-cloud` does not throw one edit away. It
merges them setting by setting, using the sidecar version the phone started from as the common ancestor: what only the
phone changed comes from the phone, what only the PC changed stays, and a setting both changed goes to the newer edit
(the sync log lists these, e.g. `inbox: ... -> merge (1 conflict: lc:settings/light/exposure)`). Crop, geometry, curve,
profile and white balance are merged as a whole, and so is any list (masks, spots, ...). Other apps' properties in the sidecar
(darktable, Lightroom, ...), namespace prefixes and the XMP packet wrapper are kept as they are. The notification then reads
e.g. "2 phone edits applied (1 merged with PC changes) in 2024/Trip", and the replaced PC sidecar and the phone's edit both
go to the local history (`<state_dir>/history/<date>/`).

To have the ancestor at hand, `lc-cloud` keeps every sidecar version it publishes and every phone edit it receives in
`<state_dir>/sidecar-versions/<sha16>.xmp` (a few KiB each). `purge-history` deletes versions older than the day limit,
except those of sidecars currently in the library. If the ancestor is not there, or the two edits cannot be merged safely
(different LightCraft engine versions, an unparsable file, ...), the newest edit wins as a whole file and the loser goes to
the local history. Details: [FORMAT.md](FORMAT.md).

### Safety

- `--max-delete` and the `max_delete` check stop a mass deletion when the source drive is missing or damaged.
- An empty source folder is refused (drive not mounted).
- Anything replaced or deleted on the remote is moved with `rclone --backup-dir` into `history/<date>/` instead of being lost.
- Processed phone edits move to `inbox-done/`; inbox names are validated and device names must be plain.
- The scratch library lives in `state_dir`, which is refused if it is under `~/Pictures`, and the LightCraft CLI is never
  run against a path under `~/Pictures` or one named `LightCraft Library`. Your real library is never touched.
- `purge-history` refuses unless the newest backup snapshot (via `photo_backup`) is at most 48 hours old.
- Only one sync runs at a time (lock file); `--dry-run` changes nothing.

### Tests

```sh
python3 -m unittest discover -s legacy/tests
```

## License

License: MIT OR Apache-2.0. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
