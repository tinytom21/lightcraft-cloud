# lc-cloud

Publish a folder of photos as a LightCraft cloud library. `lc-cloud` runs on your PC, mirrors the folder to any
[rclone](https://rclone.org) remote (Google Drive, S3, WebDAV, ...), and writes thumbnails, smart previews and a manifest
next to it. The LightCraft phone app browses that library, downloads a small preview to edit, and sends the edits back
as XMP sidecars. `lc-cloud` pulls them into the matching sidecar on the PC. The PC stays the master copy: originals are
only ever mirrored one way, and photo files are never modified.

Phone app: https://github.com/artcraft-android/lightcraft-android (cloud library support is a pending PR).
Remote layout and the inbox protocol: [FORMAT.md](FORMAT.md).

## Requirements

- Linux with Python 3.11+ (`tomllib`)
- [rclone](https://rclone.org) with a configured remote
- the LightCraft Flatpak (`ai.storyteller.lightcraft`), whose `lightcraft-cli` renders thumbnails and previews
- optional: `restic` (or a wrapper), only for `purge-history`
- optional: `inotify-tools`, only for `lc-cloud-watch`; `notify-send` for desktop notifications

## Install

```sh
git clone https://github.com/tinytom21/lightcraft-cloud ~/src/lightcraft-cloud
cd ~/src/lightcraft-cloud
mkdir -p ~/.local/bin ~/.config/lightcraft-cloud ~/.config/systemd/user
ln -s "$PWD/lightcraft_cloud.py" ~/.local/bin/lc-cloud
ln -s "$PWD/lc-cloud-watch" ~/.local/bin/lc-cloud-watch
cp config.example.toml ~/.config/lightcraft-cloud/config.toml   # edit it
cp units/* ~/.config/systemd/user/ && systemctl --user daemon-reload
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

## Commands

```
lc-cloud [--config FILE] sync [--dry-run]   publish originals, previews, thumbs and manifest; apply phone edits
lc-cloud status                             last run, tracked photos, remote size, pending inbox files
lc-cloud purge-history [--days N]           delete remote history/ and inbox-done/ folders, and stored sidecar versions, older than N days
lc-cloud inbox-check                        start a sync only if phone edits are waiting (used by the inbox timer)
```

Config file: `--config FILE`, else `$LC_CLOUD_CONFIG`, else `~/.config/lightcraft-cloud/config.toml`.
Environment: `LC_INBOX_SYNC_CMD` (command `inbox-check` runs; default `systemctl --user start --no-block lightcraft-cloud.service`),
`LC_WATCH_CMD`, `LC_WATCH_QUIET` (default 30 s), `LC_WATCH_MAX` (default 300 s) for `lc-cloud-watch`.

## Config reference

See [config.example.toml](config.example.toml). All keys are optional.

| Key | Default | Meaning |
| --- | --- | --- |
| `source` | `~/Pictures/Photos` | folder to publish |
| `remote` | `gdrive:LightCraft Cloud` | rclone `remote:path` |
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

## Phone edits that meet PC edits

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

## Safety

- `--max-delete` and the `max_delete` check stop a mass deletion when the source drive is missing or damaged.
- An empty source folder is refused (drive not mounted).
- Anything replaced or deleted on the remote is moved with `rclone --backup-dir` into `history/<date>/` instead of being lost.
- Processed phone edits move to `inbox-done/`; inbox names are validated and device names must be plain.
- The scratch library lives in `state_dir`, which is refused if it is under `~/Pictures`, and the LightCraft CLI is never
  run against a path under `~/Pictures` or one named `LightCraft Library`. Your real library is never touched.
- `purge-history` refuses unless the newest backup snapshot (via `photo_backup`) is at most 48 hours old.
- Only one sync runs at a time (lock file); `--dry-run` changes nothing.

## Tests

```sh
python3 -m unittest discover -s tests
```

## License

License: MIT OR Apache-2.0. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
