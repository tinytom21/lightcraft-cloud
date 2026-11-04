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
lightcraft-cloud-desktop quit [--wait SECONDS] [--config FILE]     ask the running app to stop and wait for it (the installer uses this)
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
Status..., Settings..., Quit, and, while a newer version exists, "Update to vX.Y.Z" at the top (see "Installing and updating"). On Linux it is a StatusNotifierItem over D-Bus (`tray-icon`'s `ksni` backend: no GTK or
libappindicator to install). The desktop needs a tray host: KDE has one; GNOME needs the AppIndicator extension (Ubuntu ships
it). The windows (egui 0.37, the version LightCraft v0.17.0 uses) run as `lightcraft-cloud-desktop ui status|settings`, in a
process of their own, so a problem in a window cannot stop the sync; they read `<state>/status.json` and write the config file,
which the app notices within two seconds.

* **Setup / Settings**: the photo folder (text field + Browse), folders to leave out, the storage (one of your rclone remotes,
  listed by name only, plus a folder on it; or a folder on this PC), "Originals and previews" or "Previews only", notifications,
  "Start when I log in", "Check for new versions". With no config the app opens Setup instead of syncing; saving writes the config file (keys the app does
  not know are kept) and the app starts syncing.
* **Status**: last sync (time, result), photos published, phone edits applied, the inbox, the next scheduled sync, "Newest wins"
  decisions, problems.
* "Start when I log in" writes `~/.config/autostart/lightcraft-cloud-desktop.desktop` (`Exec=<this binary> run`) only when the
  box is ticked in Settings, and removes it when unticked. Nothing else writes there. On Windows it is the value `LightCraft Cloud` in
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (`"<exe>" run`), which the installer's task of the same name writes too.

### Installing and updating

**Windows** installs with `lightcraft-cloud-<version>-windows-setup.exe` (another PC) or `lightcraft-cloud-<version>-windows-home-setup.exe` (the
home PC, with the engine): per user, no administrator, to `%LOCALAPPDATA%\Programs\LightCraft Cloud\`; see "Windows" in [building.md](building.md) for
what it does. Running a newer setup over an installed or running copy is an update: the installer asks the app to quit (`quit`), replaces the files and
starts it again. Config and state live elsewhere, so they carry over, also from a copy that was run from the old zip. The two installers share one
identity (the same Apps & features entry, folder, Start menu entry and "start when I log in" value), so there is one LightCraft Cloud per PC: the home
installer goes over an installed other-PC app (an upgrade; Apps & features then reads "LightCraft Cloud (home PC)"), and the other-PC installer **refuses**
to go over the home one (it changes nothing and exits with an error), because it would take the engine away and the home PC would stop publishing without
a word. To go back to the other-PC app on purpose, uninstall first. `--version` prints a second line, `edition: home` or `edition: light`; the installer
keeps the same fact in `edition.txt` next to the program (an install from v0.2.0 has none, and is light). **Linux** installs with the `install.sh` in the archive
(`lightcraft-cloud-<version>-linux-light.tar.gz` or `-linux-home.tar.gz`; no `sudo`): the program in `~/.local/bin`, `librclone.so` in
`~/.local/lib/lightcraft-cloud/`, which is one of the places the app looks for it.

**`quit`.** `lightcraft-cloud-desktop quit` writes `<state>/quit-request` and waits (60 seconds by default, `--wait`). The running app
takes the file within two seconds and ends after the job it is running, so a sync is not cut off if it ends in time. Exit code 0: stopped;
10: no app was running; 11: still running after the wait; 12: the app did not take the request (an older version). It prints one line and
opens no window or console.

**The update check** (`check_for_updates`, default on; the Settings checkbox "Check for new versions"). About 90 seconds after the start
and then once a day (an hour later if a check failed), the app asks `https://api.github.com/repos/tinytom21/lightcraft-cloud/releases/latest`
for the latest published release, with the User-Agent `lightcraft-cloud-desktop/<version> (update check; +<repository url>)` and no
other data. Drafts and prereleases are ignored; versions are compared as semantic versions (`0.10.0` is newer than `0.9.0`,
`0.3.0-rc.1` is older than `0.3.0`). A check that fails writes one line to the log and nothing else: the result is kept in
`<state>/update.json`, separate from `status.json`, so it can never change the tray icon or the Status window's state line. With the
setting off nothing is requested and nothing is shown.

When a newer version exists the tray menu gets "Update to vX.Y.Z" (Windows) or "Version X.Y.Z is available: open the page" (Linux), the Status
window gets a row with the version, a link to the release notes and, on Windows, an **Update now** button, and one desktop notification is
shown per version (not again after a restart; not at all with notifications off). Nothing is downloaded or installed until the click.

**Update now** (Windows). The window asks the app (the file `<state>/update-request`); the app then downloads
`lightcraft-cloud-<version>-windows-setup.exe` (the home build: `lightcraft-cloud-<version>-windows-home-setup.exe`) and `SHA256SUMS` from that release into `<state>/update/` (Status shows the progress bar and sizes),
compares the installer's SHA-256 with the one in `SHA256SUMS`, and, if no sync or upload is running ("Updating when this sync finishes" otherwise), runs the
installer with `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`. The installer stops the app, replaces the files and starts the new version. Anything
that goes wrong first (no network, a checksum that does not match, a file name or address that is not the release's, an installer that
fails) is shown in Status in plain words and leaves the running app as it was; the old version keeps working. The leftover installer is removed at the next start.
**A build updates only to its own kind**: the home build looks for the home installer, the light build for the light one, and neither fetches the other's; a
release that lacks the right installer (or `SHA256SUMS`) is offered as nothing at all on Windows. Only addresses under `https://github.com/tinytom21/lightcraft-cloud/releases/download/<tag>/` are downloaded, and a redirect is followed only to GitHub's own file
hosts (`objects.githubusercontent.com` and its release-assets and github-releases siblings), over HTTPS. The address cannot be changed by a setting,
an environment variable or an option.

**What this does and does not protect.** `SHA256SUMS` is a file of the same GitHub release as the installer, so the check catches a
damaged or cut-off download, not a release published by someone who controls the GitHub account. The installer is not code-signed, so
SmartScreen warns when it is run by hand from a browser download (a file the app downloads itself carries no browser mark, so SmartScreen is not expected to ask; this has not been checked on a real desktop yet). On Linux the app never replaces
itself: it only shows that a newer version exists, with the link.

**Secrets**: the home PC uses your existing rclone config (`rclone_config`, default the standard one), like the Python tool.
It lists remote names only and never logs the config's contents; only "Add a device" reads the one remote it hands over, in memory (see "Pairing").
The system keyring is not used. A PC that joined by pairing keeps its cloud sign-in in `paired-rclone.conf` in the app's config folder, a plain
file with mode 600 on Linux (on Windows, the permissions of your user folder), beside `config.toml`. Pairing never writes your own `rclone.conf`.

### Pairing

Pairing gives another device (the phone, or another PC) the rclone settings for this library's remote, without copying config files by hand.
On the home PC, Status has **Add a device**. It opens its own window, which says first what pairing hands over, then asks for a code. The joining device (the phone, or another PC) makes the code and shows it; you type it into a password-style field here. **Show pairing QR** is enabled only for a valid code (otherwise: "That code has a typo; check it against the device."), and then shows a QR code and **Copy text for another PC**. This PC never displays the code, so a screen share or a photo of this window is useless without the code from the device. The QR code and the text are the same data: the one remote the `remote` setting names (a crypt, alias, chunker, compress or hasher remote also
includes the remote it wraps, and nothing else), with the access token blanked and marked expired so that rclone refreshes it on first use, deflated and encrypted with
ChaCha20-Poly1305. The code is 10 random Crockford base32 characters plus one check character (the sum of `(2i+1) * value` over the 10, mod 32), shown as `XXXX-XXXX-XXX`; typed case, spaces and dashes are ignored, and I, L read as 1 and O as 0. The check catches every single wrong character and nearly every adjacent swap. The key comes from the whole normalised code (all 11 characters) with Argon2id (32 MiB, 3 passes), so the text alone cannot be opened, and the code alone is of no use. The text is `LCP1` followed by base45 (uppercase
letters and digits, which a QR code stores compactly); a Google Drive remote fits a QR code of about version 13 to 15. The typed code and the text are kept in memory only and are cleared
when you press Hide or close the window. The config is read from your rclone config when you press the button, and is never written to disk by this.

What it hands over: the same access to your cloud storage account as this PC has. The token cannot be limited to the LightCraft folder. Use it only for devices you own, and do not share the screen
or post the picture. To cut off a lost device, sign this app out in the provider's account page; that signs out every device that uses this sign-in, this PC too. It is only tested with Google Drive.

Remotes that cannot travel are refused with a reason shown in the window: a library in a folder on this PC, `union` and `combine`, a remote whose chain has no section in the config, and any remote that
names a file on this PC (`key_file`, `service_account_file`, `pem_file`, `ca_cert`, `client_cert`, `client_key` and similar), since that file would not exist on the other device.

On another PC, Setup and Settings have **Pair with my home PC**: it shows this PC's freshly made code in large type (a new one each time the box is opened, kept in memory only) with the instruction to type it into Status > Add a device on the home PC; then paste the text the home PC shows. The settings are written to `paired-rclone.conf` in the app's config folder, `remote` and `rclone_config` are
set to it, and the folder list is read at once. **One of my rclone remotes** still works as before. If `inbox/<device>/` already exists in the library, a caution says a device of that name has already sent edits and
suggests another name unless it is the same device. Existing configs load unchanged.

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

**What the numbers count.** *Photos* are shots, the way the home PC counts its library: a raw file and a JPEG of the same shot (same folder
and name, one of each) are one photo, as in the manifest's `alternates`. "Photos on this PC", "Waiting to download", "Photos received" and the
notification "N new photos from the home PC" count photos, so the totals on the home PC and on this PC agree when every folder is kept. A
photo counts as here as soon as one of its files is. Everything that moves one file at a time says **files**: the progress line ("Downloading
27 of 60 files", "Sending 3 of 12 files to the home PC"), the imported files waiting to go up or to be answered, and the list of files waiting
for LightCraft. While files download, the Status rows keep the photo counts of the last finished pass (they are not mixed with a count of
files). Updating from v0.2.0 needs no re-download: nothing on disk changed, and "Photos received" starts again from the photos here that
came from the home PC.

Limits for now: a folder means the photos directly in it (tick each subfolder you want); a photo moved by the home PC stays where
this PC has it; if the home PC keeps its own sidecar instead of applying yours, your edit stays here until its sidecar changes
again; the free-disk-space check needs the system to say how much is free (Linux and Windows do).

### Config

Default `~/.config/lightcraft-cloud-desktop/config.toml` (`$XDG_CONFIG_HOME` is honoured), or `--config FILE`. TOML; unknown keys
are ignored, so a Python config can be reused. State in `~/.local/state/lightcraft-cloud-desktop/` (or `state_dir`): `state.json`,
`sidecar-versions/`, `history/`, `mobile/` (the staged files as uploaded), `lib/` (the engine's scratch library), `logs/`.

| Key | Default | Meaning |
| --- | --- | --- |
| `remote` | **none, required** | an rclone `remote:path` (a crypt remote works: `lccrypt:`), or a local folder (starts with `/`, `~` or `.`; for tests). There is deliberately no default: nothing is published to a remote nobody named. |
| `role` | `"home"` | `"home"` (publishes) or `"other"` (keeps a copy of some folders; see "Another PC") |
| `source` | `~/Pictures/Photos` | folder to publish; on another PC, the local photo folder. `~` is the home folder (`%USERPROFILE%` on Windows, where `~\` works too) |
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
| `check_for_updates` | `true` | look on GitHub for a newer version, shortly after the start and once a day (see "Installing and updating"); `false`: no request is made and no update is shown |
| `lightcraft_refresh_edits` | `true` | ask LightCraft to read the sidecars the sync wrote, so new edits show without Synchronize Folder (see "Showing new edits in LightCraft"); `false` turns only that off |
| `publish_albums` | `true` | read LightCraft's albums over the control channel after each sync and publish them to the phone (see "Publishing your albums"); `false`: nothing is read and the manifest has no albums |
| `state_dir` | `~/.local/state/lightcraft-cloud-desktop` | must not be under `~/Pictures` |
| `rclone_config` | `$RCLONE_CONFIG`, else `~/.config/rclone/rclone.conf` | rclone config used for an rclone remote |
| `librclone` | `$LIBRCLONE`, else `librclone.so` (Windows: `librclone.dll`) next to the executable, else `~/.local/lib/lightcraft-cloud/librclone.so`, else the system search path | the host `librclone.so` (see "Building the Rust part" in [building.md](building.md)); the desktop app looks in that order, so an installed app needs the library in one of those places (the Linux archives' `install.sh` and the Windows installer put it there) |

### Windows as the home PC

The Windows home PC is the same program as the Linux one (`lightcraft-cloud-<version>-windows-home-setup.exe`), built and tested on Windows 10/11 x86-64 and
not code-signed. Where Windows differs from Linux, and what the app does about it:

* **Paths.** Everything in the cloud is `/`-separated whatever the PC uses: the manifest, the originals' paths, the sidecar names, the import notes. The app turns
  Windows paths into `/` paths when it reads the photo folder and back when it writes into it; a test publishes a nested folder with the real engine and checks that
  no backslash reaches the manifest, then lets the other-PC code read it. Paths longer than 260 characters work (the app asks for the long form where it calls Windows
  itself). A folder name Windows cannot hold (`CON`, `a:b`, a trailing dot) is refused from an import, as FORMAT.md says.
* **Case.** Windows does not tell `IMG_1.JPG` from `img_1.jpg`. Sidecars are found whatever their capitals, a raw and a JPEG pair by stem whatever the capitals, and a
  photo imported under a name that is taken is renamed `-2`, `-3`... (also ignoring case). "Folders to leave out" patterns stay case-sensitive (write them as the folder
  is spelled), as on the other systems, so a library behaves the same on all of them.
* **Files other programs hold.** Windows lets a program open a file so that nobody else can read or replace it. A phone edit that cannot be written because LightCraft (or a
  virus scanner, or the indexer) has the sidecar open is tried again for a second and a half, then left in the cloud inbox for the next sync; the rest of the sync goes on, the
  Status window shows "The sidecar ... is in use by another program", and the 2-minute inbox check does not start a sync for it again (the 15-minute sync retries). A sidecar that
  cannot be *read* is not treated as deleted: its photo is left for the next sync and Status says so. A photo that cannot be opened to be read (rendered, hashed) is left out of
  that sync altogether and listed in Status; nothing half-done is published. An upload that fails because the photo is held fails the sync with the file named, and the next one
  tries again.
* **Your LightCraft library.** The app never opens it. The scratch library it renders with is `<state>\lib` (`%LOCALAPPDATA%\lightcraft-cloud-desktop\lib`); the app refuses a scratch
  library under `%USERPROFILE%\Pictures`, Windows' own Pictures folder and the OneDrive ones, in any spelling of the path (capitals, `\\?\`, `..`), one named like `LightCraft Library`,
  one inside or around the folder `LIGHTCRAFT_LIBRARY` names, and any folder that holds (or sits inside) a LightCraft catalog (`catalog.snap`, `catalog.log`, `catalog.lock`)
  that this app did not make. The folders it made carry a `.lightcraft-cloud-scratch` mark; a scratch library from an earlier version (no mark) is recognised by its state file next to it.
  The state folder itself must not be under `Pictures` either.
* **One run at a time.** The run lock and the one-app lock are Windows file locks, released by Windows when the process ends, so a crash or a kill leaves nothing behind.
* **The folder watcher** uses Windows' directory change notifications. A burst of changes that overflows Windows' buffer is covered by the 15-minute sync.
* **`photo_backup`.** The command is started without a console window. `restic` is found on the PATH as `restic.exe`; for a `.cmd` or `.bat` give its path (Windows starts those through `cmd`).
  restic gets its repository and password from the user's environment (`RESTIC_REPOSITORY`, `RESTIC_PASSWORD`/`RESTIC_PASSWORD_FILE`). Without a backup newer than 48 hours the history is never
  purged, and the Status window says so; `photo_backup = ""` switches the purge off.
* **Setup.** "This is my home PC" offers `~/Pictures/Photos` as the folder (`C:\Users\you\Pictures\Photos`); choose the folder your photos are really in.
* **Not different:** notifications (Windows toasts), the tray icon (`.ico` inside the exe), the windows-subsystem exe (no console; `--headless` and the commands print into the terminal that started them),
  the control port (see Quick start), the updater (see above).

What the Windows home PC cannot do: run on ARM Windows (no build), publish a photo folder on OneDrive "files on demand" without Windows downloading each file as it is read, or notice changes on a network
drive that does not send change notifications (the 15-minute sync still runs). Everything else of the Linux home PC is there.

### Photos from a phone

The home PC does nothing different for a phone: the phone's photos arrive as imports under its device name and are added to the photo folder
as `Phone/<date taken>/<file name>` (the folders are created; a name that is taken gets `-2`, `-3`, ...). Status counts them with the other
imports. A "Folders to leave out" pattern that matches the `Phone` folder makes the home PC refuse them, and the phone shows that.

### Telling your LightCraft about new photos

When another PC or a phone sends photos (FORMAT.md, "Imports") and the sync adds them to the photo folder, the app can ask the LightCraft
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
  in "Showing new edits in LightCraft" and the three reads in "Publishing your albums" are ever sent (every line is checked against that exact list
  before it is written);
* a photo leaves the list only when LightCraft's reply says it has it (added, or already in the library). One it refuses is
  retried and logged; after ten tries it is left alone, **Problems** in the Status window says so once, and it is tried once
  more the next time the app starts;
* LightCraft closed, or the channel off: the list is kept and the Status window shows "Waiting for LightCraft: N files" with a
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

### Publishing your albums

The phone shows your LightCraft albums, view-only, in an album folder called **From PC**. The home PC publishes them in the manifest's `albums`
(FORMAT.md). To know them it asks the LightCraft you have open, over the same control channel, so it needs the control port on and
LightCraft open; nothing is read from disk, and the app never opens, copies or looks into your library or its catalog.

* **What is read**, after each sync has published the photos, and nothing else: `albums.list` (the album tree), then for each album
  `catalog.query` with only the album as filter, a page of up to 1000 photos at a time (a smart album answers with its current matches, so
  smart albums are published as the list of their photos), then `photo.inspect` for each photo the app has not looked up before, to get its
  content hash, which is the photo's id in the manifest. The hashes are kept in `<state>/album-hashes.json` and the last read in
  `<state>/albums.json`, so a photo is looked up once. At most 2000 photos are looked up per sync, so the first run on a big library
  spreads over several syncs (Status says how many are still to look up) and the sync itself never waits for it.
* **Read-only, enforced.** The same gate that checks every request the app sends admits exactly these three shapes and refuses everything
  else: `albums.list` with no parameters; `catalog.query` with exactly `filter` (only `album`, a number), `offset` and `limit` (at most
  1000) and no `sort`; `photo.inspect` with exactly `id`. No album or photo command (`album.create`, `album.addPhotos`, `photo.delete`, ...)
  can pass, and tests try each.
* **Which photos.** An album member whose file is not in the synced photo folder, or is left out by `exclude`, is not listed. The raw or
  JPEG partner of a published photo is listed as the photo it belongs to. Photos in LightCraft's bin are not members.
* **LightCraft closed, or a query failing:** the albums published last time stay exactly as they were (the photos still sync), and
  Status shows a quiet line: "Albums: last read 3 h ago; LightCraft was not open". The manifest is rewritten when only the albums changed
  (a rename, a photo added to an album), and not otherwise.
* **Off:** `publish_albums = false` reads nothing and the manifest carries no `albums`; the phone then removes its **From PC** folder.
  `lightcraft_control_port = 0` leaves the albums as they were last read, if ever.

### Rendering

Thumbnails and smart previews come from LightCraft's engine (`lightcraft-engine`, pinned to `v0.17.0`, no `sam` and no
`denoise` features), in this process, through the same commands the Python tool sends to `lightcraft-cli`: `library.import`
(mode `add`), `photo.inspect`, `library.smartPreviews`, `app.export` (JPEG, long edge `thumb_px`, quality 80, sRGB, with the
photo's current edits) and, for a changed sidecar, `photo.readMetadataFromFile`. `previewKey` is the engine's own name for the
smart preview (`lightcraft_engine::smart::file_name`, a hash of the content hash). The sync crate does not depend on the
engine: it calls a `Renderer` trait, and `apps/desktop` supplies the engine-backed one.

Render-only is enforced, not assumed: the engine works on a scratch library in `<state>/lib` (writing XMP next to the photos is
switched off), the app refuses a library folder under `~/Pictures` or named `LightCraft Library` and has no setting to point it
elsewhere, and the tests check that the source folder is byte- and mtime-identical after a sync. The user's LightCraft library
is never opened, locked or written.

**A new engine version does not re-render anything by itself.** The state remembers each photo's size, time and sidecar, not the
engine that made its thumbnail, and a sync renders only the photos the scan lists as new, changed or with a changed sidecar. After an
update that carries a newer engine, the first sync finds nothing to do and uploads nothing: the thumbnails and smart previews already
in the cloud stay as they are (the new engine opens the old scratch library in `<state>/lib` as it is). A photo that is new, changed or
has a new edit is rendered by the new engine from then on. The `previewKey` and the photo `id` do not depend on the engine version.
The manifest's `publisher` (`lightcraft-cloud-desktop <version>`) is not compared when deciding whether to upload the manifest.
