# LightCraft Cloud

LightCraft Cloud keeps your photo library in step between your PCs and your phone. The PC is home: it holds the
original photos and is the only place they are ever changed. Each PC publishes its photo folder to any
[rclone](https://rclone.org) remote (Google Drive, S3, WebDAV, ...) together with thumbnails and small smart previews.
The LightCraft phone app browses that library, downloads a preview to edit, and sends the edits back as XMP sidecars, which
the PC then applies. Originals are only ever mirrored one way, and photo files are never modified.

Remote layout and the inbox protocol: [FORMAT.md](FORMAT.md).
Phone app: https://github.com/artcraft-android/lightcraft-android (cloud library support is a pending PR).

## What is in this repository

| Path | What it is |
| --- | --- |
| `crates/sync` | Rust crate `lightcraft-cloud-client`: the sync client shared by the apps (talks to the remote through librclone, reads the manifest, fetches thumbnails and previews, sends edits back; the phone app uses it) and, in its `home` module, the PC side: publishing a photo folder, applying the phone's edits, merging sidecars. |
| `apps/desktop` | `lightcraft-cloud-desktop`: the PC app. A command line so far (`sync --once`, `status`); the tray app is to come. Renders thumbnails and smart previews with LightCraft's engine. See "Desktop app" below. |
| `tools/parity` | The harness that runs the Python tool and the Rust app on the same generated photos and compares everything they write. |
| `legacy/` | The current PC tool, `lc-cloud`, written in Python. It is still what runs the sync today; see "Legacy tool" below. |

## Building the Rust part

Rust 1.99.0 (`rust-toolchain.toml` picks it). librclone is loaded at run time, so it is not needed to build.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The tests that talk to a real rclone need a host build of `librclone.so`. Point `LIBRCLONE` at it, for example
`LIBRCLONE=~/src/lightcraft-android/apps/lightcraft-android/librclone-host/librclone.so cargo test --workspace`
(built by `apps/lightcraft-android/scripts/build-librclone.sh host` in the phone app repository). Without it those tests
print "skipped" and pass.

Some tests in `crates/sync/tests/cloud.rs` are marked `#[ignore]` because they need the engine command `library.addRemote`,
which the upstream LightCraft engine (pinned to `v0.15.0` for the tests) does not have yet. They pass against the
lightcraft-android engine; run them with `-- --ignored` once the engine has the command.

## Desktop app (`lightcraft-cloud-desktop`)

The Rust port of what `lc-cloud` does on the PC, writing the same remote layout ([FORMAT.md](FORMAT.md)), so the phone app
works with either. It has its own config and state; it never reads or writes the Python tool's
(`~/.config/lightcraft-cloud/`, `~/.local/state/lightcraft-cloud/`), and refuses a `state_dir` inside the latter.

```
lightcraft-cloud-desktop sync --once [--dry-run] [--config FILE]   apply phone edits, then publish the photo folder
lightcraft-cloud-desktop status [--config FILE]                    last run, tracked photos, remote size, pending inbox files
```

`sync --once` does what `lc-cloud sync` does, in the same six steps: the inbox (three-way merge of phone edits into the
sidecars, every replaced version kept in `<state>/history/`), the scan (excludes, raw+JPEG pairs, sidecar naming, files still
being written are left for the next run), thumbnails and smart previews for new and changed photos, a one-way mirror of the
originals (anything replaced or removed on the remote goes to `history/<date>/` with a `.HHMMSS` suffix, `max_delete` guards
against a missing drive), the mobile files, and the manifest last. `--dry-run` changes nothing. There is no background mode
yet, and `purge-history`, `inbox-check`, the watcher, the systemd units and desktop notifications are not ported.

### Config

Default `~/.config/lightcraft-cloud-desktop/config.toml` (`$XDG_CONFIG_HOME` is honoured), or `--config FILE`. TOML; unknown keys
are ignored, so a Python config can be reused. State in `~/.local/state/lightcraft-cloud-desktop/` (or `state_dir`): `state.json`,
`sidecar-versions/`, `history/`, `mobile/` (the staged files as uploaded), `lib/` (the engine's scratch library), `logs/`.

| Key | Default | Meaning |
| --- | --- | --- |
| `remote` | **none, required** | an rclone `remote:path` (a crypt remote works: `lccrypt:`), or a local folder (starts with `/`, `~` or `.`; for tests). There is deliberately no default: nothing is published to a remote nobody named. |
| `source` | `~/Pictures/Photos` | folder to publish |
| `exclude` | `[]` | folder-name patterns (fnmatch, case-sensitive) skipped at any depth |
| `pair_primary` | `"jpeg"` | which file of a raw+JPEG pair is the photo (`"jpeg"` or `"raw"`); the other becomes an alternate |
| `max_delete` | `50` | refuse when more published photos vanished than this since the last run; also the most files one mirror removes |
| `thumb_px`, `thumb_quality` | `512`, `80` | thumbnail long edge and JPEG quality |
| `previews_only` | `false` | upload sidecars, thumbnails, smart previews and the manifest, but not the photo files |
| `state_dir` | `~/.local/state/lightcraft-cloud-desktop` | must not be under `~/Pictures` |
| `rclone_config` | `$RCLONE_CONFIG`, else `~/.config/rclone/rclone.conf` | rclone config used for an rclone remote |
| `librclone` | `$LIBRCLONE`, else the system search path | the host `librclone.so` (see "Building the Rust part") |

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
| `source` | `~/Pictures/Photos` | folder to publish |
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
