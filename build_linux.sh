#!/bin/sh
# Needs: rust, libwebkit2gtk-4.1-dev, libgtk-3-dev, libxdo-dev (Debian/Mint) or webkit2gtk-4.1 gtk3 (Arch/CachyOS)
set -e
cd "$(dirname "$0")"
cargo build --release
rm -rf dist/Orbit-linux && mkdir -p dist/Orbit-linux
cp target/release/orbit dist/Orbit-linux/orbit
cp target/release/orbit-updater dist/Orbit-linux/orbit-updater
[ -f updater.json ] && cp updater.json dist/Orbit-linux/updater.json
cp assets/Orbit.png dist/Orbit-linux/orbit.png
cp linux/install.sh linux/uninstall.sh dist/Orbit-linux/
tar -C dist -czf dist/Orbit-linux.tar.gz Orbit-linux
echo "Wrote dist/Orbit-linux.tar.gz  (extract, then run ./install.sh)"
