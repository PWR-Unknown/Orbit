# Orbit (Rust)

The Orbit UI (`ui/index.html`) served from a Rust backend inside a native window (WebView2 on Windows,
WebKitGTK on Linux). The data folder format is unchanged, so existing instances/settings carry over.

## Build
- Windows: `build_windows.bat release` -> `dist\Orbit.exe` (portable) and `dist\Orbit-Setup.exe` (needs Rust, and Inno Setup 6 for the installer). Without an argument it makes a quick `--profile fast` build of `orbit` only (`target\fast\orbit.exe`).
  The installer's version is read from `Cargo.toml`, so it always matches the exe.
- Linux: `./build_linux.sh release` -> `dist/Orbit-linux.tar.gz`, then `./install.sh` (no sudo). Without an argument it makes a quick `--profile fast` build of `orbit` only (`target/fast/orbit`); links with mold or lld when installed.

## Installer
`installer/orbit.iss` uses `PrivilegesRequired=lowest`: installs to `%LOCALAPPDATA%\Programs\Orbit`,
Start Menu entry, optional desktop shortcut, uninstaller in Apps & Features, no UAC prompt. It works with Inno Setup 6.0+
(the 64-bit setting is chosen by Inno version) and warns if the WebView2 Runtime is missing.

## Updater
`orbit-updater` is a separate program (built together with Orbit, installed next to it) that updates Orbit without Orbit's
help: it reads the installed version from `orbit --version`, looks for a newer release, downloads it, checks the SHA-256, swaps
the files (keeping `orbit.exe.old` / `orbit.old` as a backup, restoring it if anything fails) and starts Orbit again.
- `orbit-updater` asks first; `--check` only reports (exit code 10 = update available); `--yes`, `--no-launch`, `--dir <folder>`.
- Where releases come from is **not hard-coded**: `--source`, `$ORBIT_UPDATE_SOURCE`, an `updater.json` (`{"source": "..."}`) next to
  it, or the `DEFAULT_SOURCE` constant in `src/bin/orbit-updater.rs`. A source is `github:owner/repo` (latest release; assets
  `Orbit-windows.zip` or `Orbit.exe`, `Orbit-linux.tar.gz`; a `.sha256` asset or GitHub's own digest is used to verify) or the URL
  of a manifest: `{"version": "0.3.0", "notes": "...", "assets": {"windows": {"url": "...", "sha256": "..."}, "linux": {...}}}`.
- Put an `updater.json` in the project root (it ships with your repo) and the build scripts and installer ship it next to the updater.
  Without any source the updater says so and does nothing. `GITHUB_TOKEN` is used if set (avoids GitHub's anonymous rate limit).
- **In-app button:** Settings > About Orbit > "Check for updates" (then "Update now"). Orbit runs `orbit-updater --check --json`
  (`/api/update/check`), shows the version and release notes, and on "Update now" starts `orbit-updater --yes --wait-pid <orbit>`
  and closes itself (`/api/update/install`); the updater swaps the files and reopens Orbit. If an update fails, the old version
  is restored and reopened, and the reason is shown once (`.orbit-update-error`, read by `/api/update/info`).
  The button needs `orbit-updater` next to `orbit(.exe)` (the installer and release archives include it).
- **Releases:** `.github/workflows/release.yml` builds and publishes `Orbit-windows.zip`, `Orbit-Setup.exe` and `Orbit-linux.tar.gz`
  when you push a tag like `v0.3.0` (the tag must equal `version` in `Cargo.toml`) and bakes `github:<owner>/<repo>` into the updater.
- Windows installer adds an "Update Orbit" Start Menu entry. Release archives may contain `orbit-updater` too; it is replaced as well.

## Minecraft 1.12 and older
Launching works for vanilla and Forge on 1.12 and below: those versions describe their command line with `minecraftArguments`
(handled when merging a loader profile), need Java 8 (Orbit fetches Mojang's `jre-legacy`; a newer system Java is never used for them,
and loader installers run on it too), and old Forge builds with a doubled version in their file name (1.7.10, 1.8.x) resolve.
Forge servers on 1.12 and older (no `@args` file) start from their `forge-...jar`. Fabric and Quilt don't exist before 1.14 and NeoForge before 1.20.2;
Orbit says so instead of failing. Not verifiable offline here, so try one real 1.12.2 Forge instance/server on your machine.

## Server page
The server's Overview shows the live player count (`online / max`, plus names the server reports) by pinging the server's own port while it runs.

Adding mods and adding plugins is one flow: the Content tab's "Browse Modrinth" opens the Discover page already filtered to the
server (loader, Minecraft version, server-side mods only) and its Install button installs straight into the server
(`discover_search` in `src/servers.rs` serves both). "Upload .jar" and "Install dependencies" are the same for both; "Install modpack" is mods-only.

**Daily restart** (server Settings tab): pick a time and how long before it players are warned. At the countdown start Orbit
sends `say` messages (5 min / 2 min / 1 min / 30 s / 10 s / 5..1 s, only the marks that fit), then stops the server and starts it
again. It only restarts a server that is running under Orbit at that moment (a stopped server is never started by the schedule),
needs Orbit open, and cancels if the server is stopped by hand during the countdown. Proxies (Velocity) have no `say`, so they
restart without a chat countdown. Stored per server in `orbit-server.json` as `auto_restart: {enabled, time, countdown}`. The local clock
comes from the window (`tz_offset` setting, minutes east of UTC, refreshed every 15 minutes), because the standard library has no time zones.

## Discord Rich Presence
Settings > Discord shows "Playing <instance>" (with Minecraft version, loader and an elapsed timer) on your Discord profile while a game runs,
or "Hosting <server>" while only a server runs, like the Modrinth app. Off by default; "Hide instance and server names" keeps private names out.
It talks to the Discord desktop app over its local IPC pipe/socket (`src/discord.rs`, std only, no extra crates, no network port), and silently
retries if Discord isn't open. **One-time setup:** create an application at https://discord.com/developers/applications (its name is what Discord
shows after "Playing"), upload `assets/Orbit.png` as an art asset named `orbit` (Rich Presence > Art Assets), and put the application ID in
`APP_ID` in `src/discord.rs` (or set `ORBIT_DISCORD_APP_ID`). Settings keys: `discord_presence`, `discord_hide_names`.

## Window
- Windows: Orbit draws its own Modrinth-style top bar (back/forward, page title, downloads, "instances running" pill,
  minimise / maximise / close). Dragging the bar is handed to Windows itself, so edge-snapping, Win+Arrow and
  "drag a maximised window down to restore" are native. The page talks to the window through a small
  `window.pywebview.api` shim (`WINDOW_API` in `src/main.rs`) over wry's IPC.
- Linux / macOS: the normal system title bar is kept; the page just leaves its own window buttons out.

## Startup problems
Orbit never dies silently: a missing WebView2 / WebKitGTK, a blocked local port or an unexpected crash shows a dialog
and is also written to `%APPDATA%\Orbit\orbit.log` (`~/.local/share/Orbit/orbit.log`). WebView2's profile is kept in
`%APPDATA%\Orbit\webview`, so the exe works from read-only folders too.

## Mods
- Files that arrive with a modpack, an import or by hand are looked up on Modrinth by SHA-1 (automatically after a
  modpack install / import, or with "Match with Modrinth") and become normal Modrinth installs: real title, icon,
  page link, version switching and update checks. This applies to instances and servers alike.
- Anything Modrinth doesn't know still gets its icon: it is read from the file itself (`fabric.mod.json`,
  `quilt.mod.json`, `mods.toml` / `neoforge.mods.toml` `logoFile`, or `pack.png`). See `src/icons.rs`.

## Dependencies (mods and plugins)
`src/deps.rs` reads the metadata inside every jar (`fabric.mod.json`, `quilt.mod.json`, `mods.toml` / `neoforge.mods.toml`,
`plugin.yml`, `paper-plugin.yml`, `velocity-plugin.json`, bundled jar-in-jar ids), finds what is missing, maps it to a
Modrinth project and installs it. This covers modpack files, imports, hand-dropped and uploaded jars, not only things
installed from Modrinth. It runs automatically before an instance launches and when a server starts (turn off with the
`auto_deps` setting), and on demand with "Install missing dependencies" in the Content tab. Matching is deliberately
conservative (exact slug or same-name search hit); anything else is reported, never guessed. CurseForge-only and
Hangar/Spigot-only dependencies are reported for manual install.

## Crash diagnosis
"Diagnose crash" (instance Logs tab) / "Diagnose" (server console) reads `crash-reports`, `logs/latest.log`, the launch log,
`hs_err_pid*.log` and the captured console, combines it with a scan of the mods folder, and ranks likely causes with
short evidence, suspect mods and a tick-off checklist of concrete steps, for causes such as missing/wrong-version dependency, wrong Java,
out of memory, mixin failure, duplicate / wrong-loader / client-only mods, graphics driver, port in use, EULA, corrupt world or jar.
Wording is kept short on purpose; each finding's own fixes come first, then a per-cause playbook (`playbook()` in `src/diagnose.rs`).
Command line (Linux or `--no-default-features` builds; the Windows GUI exe has no console): `orbit --diagnose <folder>`.
Tests: `cargo test --no-default-features`.

## Appearance
Settings > Appearance has the colour theme, a **High contrast** switch (stronger text and borders, clear focus outline; works with every
theme) and an **Accent colour** picker (presets or any colour). Text in the accent colour is lightened/darkened until it is readable on the
current theme (7:1 in high contrast). Stored as the `accent` and `high_contrast` settings.

## Notes
- Some names on disk still say "quarry" (`quarry-profiles`, `quarry-export.json`, `quarry-launch.log`, ...). They are left
  as they are on purpose: renaming them would orphan existing players' data.

## Add Optimization (Custom setup)

Tick **Add Optimization** when creating a Fabric, NeoForge, Quilt or Forge instance and Orbit adds performance mods after creating it.

- **Target version and older.** Every loader follows one rule: the ceiling is the newest game version where the loader exists
  and at least two anchor mods have a build (Forge: Embeddium, ModernFix; Quilt and Fabric: Sodium, Lithium; NeoForge: Sodium,
  ModernFix); all Minecraft versions are searched, and if none qualifies the option says so. A `minecraft = ...` line in a list in
  `assets/optimization/*.txt` (plain-text mod lists for Fabric, NeoForge, Quilt and Forge, embedded at build time) only overrides that
  rule; today only the Fabric list has one. The option works for the ceiling version and any release version below it, and the API
  enforces this too. A mod is added only if Modrinth has a build for that exact version and
  loader; otherwise it is skipped together with any libraries that were only needed for it.
- **Per-mod lookup.** Each mod name in the loader's text list is looked up on Modrinth (by slug, then by search; it must exist), then Modrinth is asked for
  its builds for the instance's exact game version and loader (Quilt also accepts Fabric builds). The newest release is used
  (the newest beta/alpha only if no release exists); a mod with no such build, or whose required dependencies have none, is
  skipped. Mutually `incompatible` mods are dropped, the rest are installed with the normal hash-checked installer, and jars
  that the final scan says can't run here are removed.
- **Never blocks creation.** The instance is always created; if Modrinth is unreachable or nothing fits, the reply's
  `optimization` object carries `error`, `skipped` and `warnings`, and the UI shows them.

API: `GET /api/optimization` -> `{loaders, ceilings, errors}` (`errors` says why a loader has no ceiling); `POST /api/instances` accepts `"optimize": true`.
