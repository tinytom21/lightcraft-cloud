LightCraft Cloud for Windows (other-PC edition)
===============================================

This is the edition for a PC that keeps a copy of some folders of your LightCraft
cloud library and sends its edits and imported photos back to the home PC. It
has no LightCraft engine and does not publish. If this PC is your home PC (the
one with the master copy of your photos), you want the home PC edition instead,
lightcraft-cloud-<version>-windows-home-setup.exe from the same Releases page.
It installs over this one and keeps your settings. The other way round is not
done for you: the installer of this edition stops without changing anything if
the home PC edition is installed (uninstall it first if you really want this
one).

Install and update
  Run lightcraft-cloud-<version>-windows-setup.exe from the Releases page
  (https://github.com/tinytom21/lightcraft-cloud/releases). It installs for you only:
  no administrator needed, to %LOCALAPPDATA%\Programs\LightCraft Cloud\. It adds a Start
  menu entry and an entry in Settings > Apps. The option "Start when I log in" is ticked
  by default; it is the same switch as the box in the app's Settings.
  Windows SmartScreen may warn that the installer is from an unknown publisher: it is not
  code-signed. Choose More info > Run anyway.

  Updating: when a newer version is published, the tray menu shows "Update to vX.Y.Z" and
  the Status window shows "Update now". Nothing is installed until you click. The app
  downloads the installer of this edition (windows-setup.exe, never the home PC one) from
  the GitHub release, checks it against the checksum in the
  release, waits until no sync is running, and runs it; the app restarts by itself. You can
  also run a newer setup.exe by hand over the old one. "Check for new versions" in
  Settings switches the daily look off (config key check_for_updates).

  Uninstalling: Settings > Apps > LightCraft Cloud. This removes the program and the
  "start when I log in" entry. It keeps your settings, state and rclone config (below) and
  never touches your photo folder.

  If you ran the zip of an earlier version: just run the installer. Your settings and state
  carry on (they do not depend on where the program is), and the "start when I log in"
  entry is moved to the new place. The old folder with the loose exe and dll is no longer
  used; delete it when you like.

Installed files
  lightcraft-cloud-desktop.exe   the tray app, its windows and the command line
  librclone.dll                  rclone's library (keep it next to the exe)
  LICENSE-*.txt, README.txt      licences and this file

First run
  1. Start LightCraft Cloud from the Start menu. A tray icon appears (look under the ^ arrow
     of the notification area) and Setup opens on first run.
  2. In Setup choose "another PC", the cloud library (an rclone remote from
     %APPDATA%\rclone\rclone.conf, or a folder), your photo folder, and the
     folders to keep.

Where things are
  Settings   %APPDATA%\lightcraft-cloud-desktop\config.toml
  State/log  %LOCALAPPDATA%\lightcraft-cloud-desktop\
  rclone     %APPDATA%\rclone\rclone.conf  (or rclone_config in config.toml)

Command line (the output appears in the terminal; the prompt may come back first)
  lightcraft-cloud-desktop.exe status
  lightcraft-cloud-desktop.exe sync --once
  lightcraft-cloud-desktop.exe run --headless
  lightcraft-cloud-desktop.exe quit        (asks the running app to stop; the installer uses it)

Edits only travel if LightCraft saves them as sidecar files. In LightCraft open
Settings, then the Import tab, and under XMP sidecars tick "Automatically write
changes into XMP sidecars" and set Sidecar names to IMG_1.CR3.xmp. It is off in a
fresh LightCraft, and only edits made after it is on are written.

Photos downloaded from the home PC are added to LightCraft through its control
port. Start LightCraft with the port on, once: either a shortcut with
  "...\lightcraft.exe" --control 7980
or, for every start:  setx LIGHTCRAFT_CONTROL_PORT 7980   (then restart LightCraft).
The port listens on this PC only. Without it the photos still arrive in your photo
folder; use Synchronize Folder in LightCraft to pick them up.

Showing new edits in LightCraft: when the app writes a sidecar into your photo
folder (an edit from the phone, or the home PC's newer sidecar), LightCraft keeps
showing the old edit until you run Synchronize Folder. With the port on, the app
asks LightCraft to do that for you, for the folders that changed, within a couple
of minutes. It only does so while LightCraft is saving edits to sidecars (the
option above); otherwise Status shows a yellow line telling you to switch it on,
and nothing is sent. Sidecar names must be IMG_1.CR3.xmp. To stop it, put
lightcraft_refresh_edits = false in the config file (the yellow line still shows).
When LightCraft itself notices changed sidecars (upstream pull request #3421) this
will not be needed any more.
