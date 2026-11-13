#!/bin/sh
# Per-user install: no sudo. Needs WebKitGTK (libwebkit2gtk-4.1) already on the system.
set -e
here="$(cd "$(dirname "$0")" && pwd)"
bin="$HOME/.local/bin"; apps="$HOME/.local/share/applications"; icons="$HOME/.local/share/icons/hicolor/256x256/apps"
mkdir -p "$bin" "$apps" "$icons"
install -m 755 "$here/orbit" "$bin/orbit"
[ -f "$here/orbit-updater" ] && install -m 755 "$here/orbit-updater" "$bin/orbit-updater"
install -m 644 "$here/orbit.png" "$icons/orbit.png"
cat > "$apps/orbit.desktop" <<DESK
[Desktop Entry]
Type=Application
Name=Orbit
Comment=Minecraft launcher
Exec=$bin/orbit
Icon=orbit
Categories=Game;
Terminal=false
DESK
update-desktop-database "$apps" 2>/dev/null || true
echo "Installed to $bin/orbit (make sure ~/.local/bin is on your PATH)."
