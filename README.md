# Orbit

**A fast, lightweight Minecraft launcher and server manager, written in Rust.**

Orbit pairs a clean Modrinth-style interface with a small native backend (WebView2 on Windows, WebKitGTK on Linux). No Electron and no bundled browser, just a compact executable that starts quickly and stays out of your way.

## Features

- **Instances and modpacks:** create instances for Vanilla, Fabric, Quilt, Forge and NeoForge, and install `.mrpack` modpacks in one click.
- **Modrinth built in:** browse, install, switch versions and check for updates. Files you add by hand are matched to Modrinth by hash.
- **Add Optimization:** an optional setup step that picks a performance mod set live from Modrinth for your exact loader and Minecraft version, and checks compatibility before installing.
- **Server manager:** one-click Vanilla, Paper, Purpur, Folia, Spigot, Fabric, Quilt, Forge, NeoForge and Velocity servers, with a live console, RAM settings, live player count, and a daily restart with in-game countdown warnings.
- **Automatic dependencies:** reads mod and plugin metadata, finds what's missing and installs it before launch or server start.
- **Crash diagnosis:** reads crash reports and logs, checks your mods folder, and ranks the likely causes with a checklist of fixes.
- **Importers:** bring over instances from the Modrinth App, CurseForge and TLauncher.
- **Wide version support:** Minecraft 1.7.10 through the latest, including automatic legacy Java for old versions.

## Build from source

Requires Rust and Inno 6. See `build_windows.bat` and `build_linux.sh`.
