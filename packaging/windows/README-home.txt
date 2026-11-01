LightCraft Cloud for Windows (home PC edition)
==============================================

This is the edition for the PC where your master photo library lives. It carries
LightCraft's rendering engine, publishes your photo folder to your cloud storage
(originals, thumbnails, previews and the photo list), applies the edits that
arrive from your phone and your other PCs to your sidecar files, and adds the
photos other PCs send. It can also be used as an "other PC" (choose "This is
another PC" in Setup), but the lighter edition,
lightcraft-cloud-<version>-windows-setup.exe, is meant for that.

It only renders: it never opens, locks or changes your LightCraft library (the
catalog). It works on a private scratch library of its own, in the state folder
(below), and refuses to use a folder that looks like yours.

Install and update
  Run lightcraft-cloud-<version>-windows-home-setup.exe from the Releases page
  (https://github.com/tinytom21/lightcraft-cloud/releases). It installs for you only:
  no administrator needed, to %LOCALAPPDATA%\Programs\LightCraft Cloud\. It adds a Start
  menu entry and an entry in Settings > Apps ("LightCraft Cloud (home PC)"). The option
  "Start when I log in" is ticked by default; it is the same switch as the box in the
  app's Settings.
  Windows SmartScreen may warn that the installer is from an unknown publisher: it is not
  code-signed. Choose More info > Run anyway.

  It installs over an earlier install of this program, including the other-PC edition
  (same place, same settings). The installer of the other-PC edition will not go over
  this one: it stops and changes nothing, because it would stop this PC from publishing.

  Updating: when a newer version is published, the tray menu shows "Update to vX.Y.Z" and
  the Status window shows "Update now". Nothing is installed until you click. The app
  downloads the home PC installer (windows-home-setup.exe, never the other-PC one) from
  the GitHub release, checks it against the checksum in the release, waits until no sync
  is running, and runs it; the app restarts by itself. "Check for new versions" in
  Settings switches the daily look off (config key check_for_updates).

  Uninstalling: Settings > Apps > LightCraft Cloud. This removes the program and the
  "start when I log in" entry. It keeps your settings, state and rclone config (below) and
  never touches your photo folder or your LightCraft library.

Installed files
  lightcraft-cloud-desktop.exe   the tray app, its windows and the command line (the engine is inside)
  librclone.dll                  rclone's library (keep it next to the exe)
  LICENSE-*.txt, README.txt      licences and this file

First run
  1. Start LightCraft Cloud from the Start menu. A tray icon appears (look under the ^ arrow
     of the notification area) and Setup opens on first run.
  2. In Setup choose "This is my home PC", your photo folder (the folder your LightCraft
     photos are in, for example D:\Photos; not your LightCraft library folder), where to
     store the cloud library (an rclone remote from %APPDATA%\rclone\rclone.conf, or a
     folder), and "Originals and previews" or "Previews only". Save: the first sync
     uploads the library.

Where things are
  Settings   %APPDATA%\lightcraft-cloud-desktop\config.toml
  State/log  %LOCALAPPDATA%\lightcraft-cloud-desktop\   (the scratch library is in its lib\ folder)
  rclone     %APPDATA%\rclone\rclone.conf  (or rclone_config in config.toml)

Command line (the output appears in the terminal; the prompt may come back first)
  lightcraft-cloud-desktop.exe status
  lightcraft-cloud-desktop.exe sync --once [--dry-run]
  lightcraft-cloud-desktop.exe inbox-check
  lightcraft-cloud-desktop.exe purge-history
  lightcraft-cloud-desktop.exe run --headless
  lightcraft-cloud-desktop.exe quit        (asks the running app to stop; the installer uses it)

LightCraft settings (on this PC, once)
  Edits only travel if LightCraft saves them as sidecar files. In LightCraft open
  Settings, then the Import tab, and under XMP sidecars tick "Automatically write
  changes into XMP sidecars" and set Sidecar names to IMG_1.CR3.xmp. It is off in a
  fresh LightCraft, and only edits made after it is on are written.

  The app tells your running LightCraft about photos other PCs sent and about sidecars
  it wrote (so you need not run Synchronize Folder), through LightCraft's control port.
  Start LightCraft with the port on, once: either a shortcut with
    "...\lightcraft.exe" --control 7980
  or, for every start:  setx LIGHTCRAFT_CONTROL_PORT 7980   (then restart LightCraft).
  The port listens on this PC only and has no password. Without it everything still
  works; run Synchronize Folder in LightCraft to pick up new photos and edits.

Things that are different on Windows
  * A sidecar or photo that LightCraft (or a virus scanner, or the indexer) has open
    when the app needs it is not an error: the app waits for the next sync, and Status
    says which file it is waiting for. A phone edit stays in the cloud inbox until the
    sidecar can be replaced.
  * Folder names in "Folders to leave out" are compared with their exact capitals, even
    though Windows itself does not tell Exported from exported. Write the pattern the
    way the folder is spelled.
  * "Old cloud history is deleted only after a recent backup" uses restic, if you have
    it. Put restic.exe on your PATH (or give the full path to restic.exe or restic.cmd
    in photo_backup in config.toml, or set photo_backup = "" to switch the cleaning off).
    restic needs RESTIC_REPOSITORY and RESTIC_PASSWORD (or RESTIC_PASSWORD_FILE) in your
    user environment, since the app starts it without a console.
  * Files that are only online (OneDrive "files on demand") are downloaded by Windows when
    the app reads them. Keep the photo folder on the PC, or set it to "always keep on this
    device".
