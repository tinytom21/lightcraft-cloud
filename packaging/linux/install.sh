#!/bin/sh
# Installs LightCraft Cloud for the current user: no sudo, nothing outside your home folder.
#   ~/.local/bin/lightcraft-cloud-desktop
#   ~/.local/lib/lightcraft-cloud/librclone.so   (the app looks for it there, and next to itself; see "librclone" in the config reference)
#   ~/.local/lib/lightcraft-cloud/licenses/
# Run it from the unpacked folder:  ./install.sh
# Your settings and state (~/.config/lightcraft-cloud-desktop, ~/.local/state/lightcraft-cloud-desktop) are not touched.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
bin="${HOME:?HOME is not set}/.local/bin"
lib="$HOME/.local/lib/lightcraft-cloud"

for f in lightcraft-cloud-desktop librclone.so; do
    if [ ! -f "$here/$f" ]; then
        echo "install.sh: $f is not next to this script. Run it from the unpacked folder." >&2
        exit 1
    fi
done

mkdir -p "$bin" "$lib/licenses"

# (copy to a temporary name and rename, so a copy that is running now is replaced cleanly, not "text file busy")
put() { # put <file> <mode> <destination>
    cp "$1" "$3.new.$$"
    chmod "$2" "$3.new.$$"
    mv -f "$3.new.$$" "$3"
}

put "$here/lightcraft-cloud-desktop" 755 "$bin/lightcraft-cloud-desktop"
put "$here/librclone.so" 644 "$lib/librclone.so"
for l in "$here"/LICENSE-*.txt; do
    [ -f "$l" ] && put "$l" 644 "$lib/licenses/$(basename "$l")"
done

# The programs menu entry (starting it while the app runs opens its Status window)
apps="$HOME/.local/share/applications"
icons="$HOME/.local/share/icons/hicolor/256x256/apps"
id=io.github.tinytom21.lightcraftcloud
mkdir -p "$apps" "$icons"
[ -f "$here/lightcraft-cloud.png" ] && put "$here/lightcraft-cloud.png" 644 "$icons/$id.png"
cat > "$apps/$id.desktop.new.$$" <<EOF
[Desktop Entry]
Type=Application
Name=LightCraft Cloud
Comment=Sync LightCraft photos and edits with your phone and other PCs
Exec="$bin/lightcraft-cloud-desktop" run
Icon=$id
Terminal=false
Categories=Graphics;Photography;
StartupNotify=false
EOF
chmod 644 "$apps/$id.desktop.new.$$"
mv -f "$apps/$id.desktop.new.$$" "$apps/$id.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps" >/dev/null 2>&1 || true

echo "Installed LightCraft Cloud $("$bin/lightcraft-cloud-desktop" --version 2>/dev/null | head -n 1 | sed 's/^lightcraft-cloud-desktop //')."
echo "  program: $bin/lightcraft-cloud-desktop"
echo "  library: $lib/librclone.so"
echo "  menu:    $apps/$id.desktop"
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "Note: $bin is not in your PATH. Start the app with the full path, or add it to PATH." ;;
esac
echo "If LightCraft Cloud is running, choose Quit in its tray menu and start it again to use the new version."
echo "Start it with: lightcraft-cloud-desktop   (Setup opens on the first run)"
