# The desktop app

Technical reference for `lightcraft-cloud-desktop`, the PC app. For an overview, start with the [README](../README.md). Building it: [building.md](building.md).

## Desktop app (`lightcraft-cloud-desktop`)

The PC app, a Rust port of what `lc-cloud` did, writing the same remote layout ([FORMAT.md](../FORMAT.md)), so the phone app
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

The tray icon (idle green, syncing blue, changes waiting amber, paused grey, error red) has the menu Sync now, Pause/Resume,
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
`mobile/` (its edits travel through `inbox/<device>/`, like a phone's, see [FORMAT.md](../FORMAT.md)).

In LightCraft on that PC, open Settings, then the Import tab, and under **XMP sidecars** tick **Automatically write changes into XMP
sidecars** and set **Sidecar names** to `IMG_1.CR3.xmp`. It is off in a fresh LightCraft, and without it edits stay in LightCraft's
catalog, where this app cannot see them, so they are never sent. Only edits made after it is on are written.

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
did not write; with the control port on, the app asks LightCraft to read it (see "Showing new edits in LightCraft"), and without the
port you run Synchronize Folder in LightCraft yourself. A photo moved to `removed/` leaves a missing file in LightCraft; remove it there.

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
| `lightcraft_control_port` | `7980` | the port of your running LightCraft's control channel (see "Telling your LightCraft about new photos"); `0` switches it off, and then nothing at all is sent to LightCraft |
| `lightcraft_refresh_edits` | `true` | ask LightCraft to read the sidecars the sync wrote, so new edits show without Synchronize Folder (see "Showing new edits in LightCraft"); `false` turns only that off |
| `state_dir` | `~/.local/state/lightcraft-cloud-desktop` | must not be under `~/Pictures` |
| `rclone_config` | `$RCLONE_CONFIG`, else `~/.config/rclone/rclone.conf` | rclone config used for an rclone remote |
| `librclone` | `$LIBRCLONE`, else `librclone.so` next to the executable, else `~/.local/lib/lightcraft-cloud/librclone.so`, else the system search path | the host `librclone.so` (see "Building the Rust part" in [building.md](building.md)); the desktop app looks in that order, so an installed app needs the library in one of those places |

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
  LightCraft keeps a reference to each file where it is: nothing is copied, moved, or deleted. Besides that, only the two requests
  in "Showing new edits in LightCraft" are ever sent (every line is checked against that exact list before it is written);
* a photo leaves the list only when LightCraft's reply says it has it (added, or already in the library). One it refuses is
  retried and logged; after ten tries it is left alone, **Problems** in the Status window says so once, and it is tried once
  more the next time the app starts;
* LightCraft closed, or the channel off: the list is kept and the Status window shows "Waiting for LightCraft: N photos" with a
  hint that the port has not answered. The photos go in the next time LightCraft opens with the channel on;
* the app never starts LightCraft, never passes `--library`, never opens or names your library: LightCraft adds the photos to
  whichever library it has open. The Flatpak can only see some folders (`xdg-pictures` by default): a photo folder it cannot see
  comes back as a failure ("No such file"), so give it access (`flatpak override --user --filesystem=<folder> ai.storyteller.lightcraft`).

### Showing new edits in LightCraft

When the app writes a sidecar into your photo folder (a phone edit applied or merged on the home PC; the home PC's newer sidecar taken
on another PC), LightCraft keeps showing the old edit until you run **Synchronize Folder** on that folder. With the control port on, the
app does that for you, until LightCraft does it by itself (upstream PR #3421), when this setting can be switched off:

* the folder of each sidecar the app writes goes on a second small list in the state folder (`lightcraft-refresh/`), once per folder
  however many sidecars it holds; nothing is lost if either program restarts. A folder that is not imported in LightCraft (normal on a
  PC where you did not import it) is tried three times, then dropped with one line in the log, not in Problems;
* at the next check (within 2 minutes, and right after a sync that wrote sidecars), after the new photos have been added, the app
  asks `library.xmpPreferences` with no parameters, which only reads the setting. **Only if LightCraft writes its edits into sidecars**
  does it go on, because reading a sidecar makes the sidecar win, and in a LightCraft that keeps edits only in its catalog a sidecar
  that merely looks newer could replace them;
* then, for up to three folders per check, one at a time, it sends `folder.synchronize` with exactly `readMetadata` on and
  `importNew`, `relinkMoved` and `removeMissing` off: LightCraft reads the sidecars saved after the library last had its say, the
  sidecar wins, as one undo step. No photo is added or removed and no file is touched; a sidecar LightCraft wrote itself is never an
  update. An answer that takes up to a minute is waited for; no answer keeps the folder for the next check;
* if LightCraft is **not** saving edits to sidecars, nothing is sent, the folders wait, and Status shows an amber line, on both roles:
  "LightCraft isn't saving edits to sidecars, so they can't sync. In LightCraft: Settings, Import tab, XMP sidecars: tick Automatically
  write changes into XMP sidecars." It goes as soon as the option is on. This is also why edits made on a Windows PC never reached the
  phone: without that option they stay in LightCraft's catalog. The line is silent when LightCraft is closed or the port is off, and it
  shows even with `lightcraft_refresh_edits = false`;
* sidecars must be named like the photo with `.xmp` added, `IMG_1.CR3.xmp` (Settings, Import tab, **Sidecar names**): LightCraft's
  other naming, `IMG_1.xmp`, would be one file for a raw and its JPEG.

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
