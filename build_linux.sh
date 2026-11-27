#!/bin/sh
# Needs: rust, libwebkit2gtk-4.1-dev, libgtk-3-dev, libxdo-dev (Debian/Mint) or webkit2gtk-4.1 gtk3 (Arch/CachyOS)
#   ./build_linux.sh          fast build of orbit only (target/fast/orbit), for everyday work
#   ./build_linux.sh release  full optimised build of orbit + orbit-updater, packaged as dist/Orbit-linux.tar.gz
set -e
cd "$(dirname "$0")"
if [ "$1" != "release" ]; then
  # faster links when mold or lld is installed and the C compiler can use it; otherwise the normal linker
  for ld in mold lld; do
    if command -v "$ld" >/dev/null 2>&1 && echo 'int main(void){return 0;}' | cc -x c - -fuse-ld="$ld" -o /dev/null >/dev/null 2>&1; then
      export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C link-arg=-fuse-ld=$ld"; break
    fi
  done
  cargo build --profile fast --bin orbit
  echo "Fast build: target/fast/orbit   (run './build_linux.sh release' for the packaged release)"
  exit 0
fi
cargo build --release --bin orbit --bin orbit-updater
rm -rf dist/Orbit-linux && mkdir -p dist/Orbit-linux
cp target/release/orbit dist/Orbit-linux/orbit
cp target/release/orbit-updater dist/Orbit-linux/orbit-updater
[ -f updater.json ] && cp updater.json dist/Orbit-linux/updater.json
cp assets/Orbit.png dist/Orbit-linux/orbit.png
cp linux/install.sh linux/uninstall.sh dist/Orbit-linux/
tar -C dist -czf dist/Orbit-linux.tar.gz Orbit-linux
echo "Wrote dist/Orbit-linux.tar.gz  (extract, then run ./install.sh)"
