LightCraft Cloud for Windows (light build: another PC)
======================================================

This is the build for a PC that keeps a copy of some folders of your LightCraft
cloud library and sends its edits and imported photos back to the home PC. It
has no LightCraft engine and does not publish.

Files
  lightcraft-cloud-desktop.exe   the tray app, its windows and the command line
  librclone.dll                  rclone's library (keep it next to the exe)
  LICENSE-*.txt, ...             licences

First run
  1. Put both files in one folder, for example
     %LOCALAPPDATA%\Programs\LightCraft Cloud\
  2. Start lightcraft-cloud-desktop.exe. A tray icon appears (look under the
     ^ arrow of the notification area) and Setup opens on first run.
     Windows SmartScreen may warn about an unsigned program: More info > Run anyway.
  3. In Setup choose "another PC", the cloud library (an rclone remote from
     %APPDATA%\rclone\rclone.conf, or a folder), your photo folder, and the
     folders to keep.

Where things are
  Settings   %APPDATA%\lightcraft-cloud-desktop\config.toml
  State/log  %LOCALAPPDATA%\lightcraft-cloud-desktop\
  rclone     %APPDATA%\rclone\rclone.conf  (or rclone_config in config.toml)
  "Start when I log in" (Settings) adds a per-user entry; no administrator needed.

Command line (the output appears in the terminal; the prompt may come back first)
  lightcraft-cloud-desktop.exe status
  lightcraft-cloud-desktop.exe sync --once
  lightcraft-cloud-desktop.exe run --headless

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
