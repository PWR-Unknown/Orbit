//! orbit-updater: a standalone program that updates an installed Orbit. It does not need Orbit to be running, and
//! Orbit does not need to know about it; it only replaces files next to itself (or in --dir).
//!
//!   orbit-updater                 check, ask, download, verify, replace, relaunch
//!   orbit-updater --check         only report (exit code 10 = an update is available, 0 = up to date)
//!   orbit-updater --yes           don't ask
//!   orbit-updater --dir <folder>  where Orbit lives (default: the folder this program is in)
//!   orbit-updater --source <s>    `github:owner/repo` or the URL of a manifest.json (see README, "Updater")
//!   orbit-updater --no-launch     don't start Orbit afterwards
//!   orbit-updater --wait-pid <n>  wait for that process to exit first (what Orbit passes; also makes failures quiet:
//!                                 the error goes to `.orbit-update-error` and the old Orbit is started again)
//!   orbit-updater --check --json  print one JSON line instead of text (what Orbit's in-app "Check for updates" reads)
//!
//! The source can also come from $ORBIT_UPDATE_SOURCE or an `updater.json` ({"source": "..."}) next to this program.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[path = "../util.rs"]
#[allow(dead_code)]
mod util;

/// Set this to your release feed before shipping, or leave it empty and ship an updater.json instead.
/// Can also be baked in at build time: `ORBIT_UPDATE_SOURCE=github:owner/repo cargo build --release`.
const DEFAULT_SOURCE: &str = match option_env!("ORBIT_UPDATE_SOURCE") { Some(s) => s, None => "" };
/// The updater records what it installed here, for Orbit builds that can't report their own version.
const VERSION_FILE: &str = ".orbit-version";
/// Written when a background update (started from inside Orbit) fails; Orbit shows it once and deletes it.
const ERROR_FILE: &str = ".orbit-update-error";
const EXE: &str = if cfg!(windows) { "orbit.exe" } else { "orbit" };
const SELF_EXE: &str = if cfg!(windows) { "orbit-updater.exe" } else { "orbit-updater" };
const PLATFORM: &str = if cfg!(windows) { "windows" } else { "linux" };
const OWN_VERSION: &str = env!("CARGO_PKG_VERSION");

type Res<T = ()> = Result<T, String>;

// ---------- versions ----------

/// The version rules (pre-release below the release, unparseable is never "newer") are shared with Orbit: src/util.rs.
fn is_newer(latest: &str, current: &str) -> bool { util::is_newer_version(latest, current) }

// ---------- release feed ----------

#[derive(Debug, Clone, PartialEq)]
struct Release { version: String, notes: String, url: String, name: String, sha256: Option<String> }

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(15)).timeout_read(Duration::from_secs(60))
        .user_agent(&format!("orbit-updater/{OWN_VERSION}")).build()
}

fn get_json(url: &str) -> Res<Value> {
    let mut req = agent().get(url).set("Accept", "application/json");
    if url.starts_with("https://api.github.com/") {
        if let Ok(tok) = std::env::var("GITHUB_TOKEN") { if !tok.trim().is_empty() { req = req.set("Authorization", &format!("Bearer {}", tok.trim())); } }
    }
    match req.call() {
        Ok(r) => r.into_json().map_err(|e| format!("{url} didn't return JSON: {e}")),
        Err(ureq::Error::Status(404, _)) => Err(format!("Nothing found at {url}. For a GitHub source the repo needs a published release (drafts and pre-releases don't count) and must be public.")),
        Err(ureq::Error::Status(c @ (403 | 429), _)) => Err(format!("{url} refused the request (HTTP {c}). GitHub limits anonymous requests; wait a while, or set GITHUB_TOKEN.")),
        Err(e) => Err(format!("Couldn't reach {url}: {e}. Check your internet connection.")),
    }
}

fn norm_hash(h: &str) -> Option<String> {
    let h = h.trim().trim_start_matches("sha256:").split_whitespace().next()?.to_lowercase();
    (h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())).then_some(h)
}

/// A manifest: {"version": "0.3.0", "notes": "...", "assets": {"windows": {"url", "sha256"}, "linux": {...}}}
fn release_from_manifest(m: &Value, platform: &str) -> Res<Release> {
    let version = m["version"].as_str().ok_or("The update manifest has no \"version\".")?.to_string();
    let a = &m["assets"][platform];
    let url = a["url"].as_str().ok_or_else(|| format!("The update manifest has no download for {platform}."))?.to_string();
    let name = url.split('?').next().unwrap_or("").rsplit('/').next().unwrap_or("").to_string();
    Ok(Release { version, notes: m["notes"].as_str().unwrap_or("").to_string(), url, name, sha256: a["sha256"].as_str().and_then(norm_hash) })
}

/// A GitHub "latest release" document. Assets: Windows `Orbit-windows.zip` or `Orbit.exe`; Linux `Orbit-linux.tar.gz`.
fn release_from_github(r: &Value, platform: &str) -> Res<Release> {
    let version = r["tag_name"].as_str().ok_or("That release has no tag.")?.to_string();
    let assets = r["assets"].as_array().cloned().unwrap_or_default();
    let wanted: &[&str] = if platform == "windows" { &["orbit-windows.zip", "orbit.exe", "orbit-portable.exe"] } else { &["orbit-linux.tar.gz", "orbit-linux.tgz"] };
    let pick = wanted.iter().find_map(|w| assets.iter().find(|a| a["name"].as_str().map(|n| n.eq_ignore_ascii_case(w)).unwrap_or(false)))
        .ok_or_else(|| format!("The latest release has no {platform} download (looked for {}).", wanted.join(", ")))?;
    let name = pick["name"].as_str().unwrap_or("").to_string();
    let side = assets.iter().find(|a| a["name"].as_str() == Some(&format!("{name}.sha256"))).and_then(|a| a["browser_download_url"].as_str()).map(String::from);
    let sha256 = pick["digest"].as_str().and_then(norm_hash)
        .or_else(|| side.and_then(|u| agent().get(&u).call().ok()?.into_string().ok()).and_then(|t| norm_hash(&t)));
    Ok(Release { version, notes: r["body"].as_str().unwrap_or("").to_string(), url: pick["browser_download_url"].as_str().unwrap_or("").to_string(), name, sha256 })
}

fn latest(source: &str) -> Res<Release> {
    if let Some(repo) = source.strip_prefix("github:") {
        let j = get_json(&format!("https://api.github.com/repos/{}/releases/latest", repo.trim_matches('/')))?;
        release_from_github(&j, PLATFORM)
    } else {
        release_from_manifest(&get_json(source)?, PLATFORM)
    }
}

// ---------- files ----------

fn download(url: &str, dest: &Path, show: bool) -> Res<String> {
    let resp = agent().get(url).call().map_err(|e| format!("Download failed: {e}"))?;
    let total: u64 = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut r = resp.into_reader();
    let mut f = std::fs::File::create(dest).map_err(|e| format!("Can't write {}: {e}", dest.display()))?;
    let (mut h, mut buf, mut got, mut last) = (Sha256::new(), vec![0u8; 64 * 1024], 0u64, 0u64);
    loop {
        let n = r.read(&mut buf).map_err(|e| format!("Download interrupted: {e}"))?;
        if n == 0 { break; }
        h.update(&buf[..n]);
        f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        got += n as u64;
        if show && total > 0 && got * 100 / total >= last + 10 { last = got * 100 / total; print!("\r  {last}%"); let _ = std::io::stdout().flush(); }
    }
    if show { println!("\r  100%"); }
    Ok(hex::encode(h.finalize()))
}

/// What to install from a downloaded file: (name inside the install folder, bytes). Handles a bare exe, .zip and .tar.gz.
fn payload(file: &Path, name: &str) -> Res<Vec<(String, Vec<u8>)>> {
    let lower = name.to_lowercase();
    let want = |n: &str| -> Option<&'static str> {
        let base = n.rsplit(['/', '\\']).next().unwrap_or("").to_lowercase();
        if base == EXE { Some(EXE) } else if base == SELF_EXE { Some(SELF_EXE) } else { None }
    };
    let mut out: Vec<(String, Vec<u8>)> = vec![];
    if lower.ends_with(".zip") {
        let mut z = zip::ZipArchive::new(std::fs::File::open(file).map_err(|e| e.to_string())?).map_err(|e| format!("That download isn't a valid zip: {e}"))?;
        for i in 0..z.len() {
            let mut e = z.by_index(i).map_err(|e| e.to_string())?;
            if e.is_dir() { continue; }
            if let Some(n) = want(e.name()) { let mut b = vec![]; e.read_to_end(&mut b).map_err(|e| e.to_string())?; out.push((n.to_string(), b)); }
        }
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        let gz = flate2::read::GzDecoder::new(std::fs::File::open(file).map_err(|e| e.to_string())?);
        for e in tar::Archive::new(gz).entries().map_err(|e| format!("That download isn't a valid .tar.gz: {e}"))? {
            let mut e = e.map_err(|e| e.to_string())?;
            let p = e.path().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
            if let Some(n) = want(&p) { let mut b = vec![]; e.read_to_end(&mut b).map_err(|e| e.to_string())?; out.push((n.to_string(), b)); }
        }
    } else {
        out.push((EXE.to_string(), std::fs::read(file).map_err(|e| e.to_string())?));
    }
    if !out.iter().any(|(n, _)| n == EXE) { return Err(format!("The download doesn't contain {EXE}.")); }
    Ok(out)
}

/// Put `bytes` at dir/name, keeping the old file as `<name>.old`. Works while the old file is running (Windows allows
/// renaming a running exe). Returns an undo that restores the old file.
fn replace(dir: &Path, name: &str, bytes: &[u8]) -> Res<Option<(PathBuf, PathBuf)>> {
    let (target, new) = (dir.join(name), dir.join(format!("{name}.new")));
    let mut old = dir.join(format!("{name}.old"));
    std::fs::write(&new, bytes).map_err(|e| format!("Can't write into {}: {e}", dir.display()))?;
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)); }
    // a leftover .old can still be running (Windows keeps it locked): then use a different backup name
    if old.exists() && std::fs::remove_file(&old).is_err() { old = dir.join(format!("{name}.old{:x}", std::process::id())); let _ = std::fs::remove_file(&old); }
    let had = target.exists();
    if had { std::fs::rename(&target, &old).map_err(|e| { let _ = std::fs::remove_file(&new); format!("Can't replace {name} (is Orbit still running? close it and try again): {e}") })?; }
    if let Err(e) = std::fs::rename(&new, &target) {
        if had { let _ = std::fs::rename(&old, &target); }
        let _ = std::fs::remove_file(&new);
        return Err(format!("Couldn't put the new {name} in place: {e}"));
    }
    Ok(had.then_some((target, old)))
}

fn wait_for_exit(pid: u32) {
    for _ in 0..120 {
        let alive = if cfg!(windows) {
            std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())).unwrap_or(false)
        } else { Path::new(&format!("/proc/{pid}")).exists() };
        if !alive { return; }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn first_version(text: &str) -> Option<String> {
    text.split_whitespace().find(|w| w.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)).map(String::from)
}

/// Asks `orbit --version` (with a timeout: an old build without that flag would just open its window), then falls back
/// to what this updater recorded last time, then to its own version.
fn installed_version(exe: &Path, dir: &Path) -> String {
    let mut c = std::process::Command::new(exe);
    c.arg("--version").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; c.creation_flags(0x08000000); }
    if let Ok(mut child) = c.spawn() {
        for _ in 0..40 {
            match child.try_wait() {
                Ok(Some(_)) => {
                    let mut s = String::new();
                    if let Some(mut o) = child.stdout.take() { let _ = o.read_to_string(&mut s); }
                    if let Some(v) = first_version(&s) { return v; }
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    std::fs::read_to_string(dir.join(VERSION_FILE)).ok().and_then(|s| first_version(&s)).unwrap_or_else(|| OWN_VERSION.to_string())
}

/// PIDs of Orbit processes running from `exe`.
fn running_orbit(exe: &Path) -> Vec<u32> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let want = canon(exe);
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::new().with_exe(UpdateKind::Always));
    sys.processes().iter().filter(|(_, p)| p.exe().map(|e| canon(e) == want).unwrap_or(false)).map(|(pid, _)| pid.as_u32()).collect()
}

fn source(arg: Option<String>, here: &Path) -> Res<String> {
    let file = std::fs::read_to_string(here.join("updater.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["source"].as_str().map(String::from));
    [arg, std::env::var("ORBIT_UPDATE_SOURCE").ok(), file, Some(DEFAULT_SOURCE.to_string())].into_iter().flatten().map(|s| s.trim().to_string()).find(|s| !s.is_empty())
        .ok_or_else(|| "No update source is set, so there is nowhere to look for new versions.\nCreate a file named updater.json next to orbit-updater containing {\"source\": \"github:owner/repo\"} (or the URL of a manifest.json), or run orbit-updater --source github:owner/repo.".to_string())
}

struct Opts { check: bool, yes: bool, launch: bool, json: bool, dir: Option<PathBuf>, source: Option<String>, pid: Option<u32> }

fn parse_args(a: &[String]) -> Res<Opts> {
    let mut o = Opts { check: false, yes: false, launch: true, json: false, dir: None, source: None, pid: None };
    let mut it = a.iter();
    while let Some(arg) = it.next() {
        let mut val = || it.next().cloned().ok_or_else(|| format!("{arg} needs a value."));
        match arg.as_str() {
            "--check" => o.check = true, "--json" => o.json = true, "--yes" | "-y" => o.yes = true, "--no-launch" => o.launch = false,
            "--dir" => o.dir = Some(PathBuf::from(val()?)), "--source" => o.source = Some(val()?),
            "--wait-pid" => o.pid = val()?.parse().ok(),
            "--help" | "-h" => return Err("usage: orbit-updater [--check] [--json] [--yes] [--no-launch] [--dir <folder>] [--source github:owner/repo|<manifest url>] [--wait-pid <pid>]".into()),
            x => return Err(format!("Unknown option {x}. Try --help.")),
        }
    }
    Ok(o)
}

fn here() -> PathBuf { std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from(".")) }

fn run() -> Res<i32> {
    let o = parse_args(&std::env::args().skip(1).collect::<Vec<_>>())?;
    let r = run_with(&o);
    // Started by Orbit itself (--wait-pid): nobody is watching a console, and Orbit has already closed. So on failure
    // leave the reason where Orbit will show it, and bring the (restored) old Orbit back instead of leaving nothing.
    if let (Err(e), Some(_)) = (&r, o.pid) {
        let dir = o.dir.clone().unwrap_or_else(here);
        let _ = std::fs::write(dir.join(ERROR_FILE), e);
        if o.launch { start_orbit(&dir); }
    }
    r
}

fn start_orbit(dir: &Path) -> bool {
    let mut c = std::process::Command::new(dir.join(EXE));
    c.current_dir(dir);
    #[cfg(windows)] { use std::os::windows::process::CommandExt; c.creation_flags(0x00000008 | 0x00000200); }
    c.spawn().is_ok()
}

fn run_with(o: &Opts) -> Res<i32> {
    let here = here();
    let dir = o.dir.clone().unwrap_or_else(|| here.clone());
    let exe = dir.join(EXE);
    let src = source(o.source.clone(), &here)?;
    let current = if exe.is_file() { installed_version(&exe, &dir) } else { "0.0.0".to_string() };
    if o.json {
        // One line, nothing else on stdout: Orbit parses this.
        let out = match latest(&src) {
            Ok(r) => serde_json::json!({"current": current, "latest": r.version.trim_start_matches(['v', 'V']), "available": is_newer(&r.version, &current), "notes": r.notes.trim()}),
            Err(e) => serde_json::json!({"error": e}),
        };
        println!("{out}");
        return Ok(0);
    }
    println!("Orbit installed: {}", if exe.is_file() { current.as_str() } else { "not found in this folder" });
    let rel = latest(&src)?;
    println!("Latest release:  {}", rel.version);
    if !is_newer(&rel.version, &current) { println!("Orbit is up to date."); return Ok(0); }
    if !rel.notes.trim().is_empty() { println!("\n{}\n", rel.notes.trim().lines().take(15).collect::<Vec<_>>().join("\n")); }
    if o.check { println!("An update is available."); return Ok(10); }
    if !o.yes {
        print!("Update Orbit {current} -> {}? [y/N] ", rel.version.trim_start_matches('v'));
        let _ = std::io::stdout().flush();
        let mut a = String::new();
        let _ = std::io::stdin().read_line(&mut a);
        if !a.trim().eq_ignore_ascii_case("y") { println!("Cancelled."); return Ok(1); }
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".orbit-update-{:08x}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    println!("Downloading {}...", rel.name);
    let got = download(&rel.url, &tmp, true).map_err(|e| { let _ = std::fs::remove_file(&tmp); e })?;
    let verdict = match &rel.sha256 {
        Some(want) if *want != got => { let _ = std::fs::remove_file(&tmp); return Err("The download's checksum doesn't match, so it was thrown away. Nothing was changed.".into()); }
        Some(_) => "checksum OK",
        None => "no checksum published, skipping the check",
    };
    println!("Verified ({verdict}).");
    let files = payload(&tmp, &rel.name);
    let _ = std::fs::remove_file(&tmp);
    let files = files?;
    if let Some(p) = o.pid { println!("Waiting for Orbit to close..."); wait_for_exit(p); }
    let mut still_open = false;
    if exe.is_file() {
        let mut pids = running_orbit(&exe);
        if !pids.is_empty() {
            println!("Orbit is open. Close it to continue (waiting up to 60 seconds)...");
            for _ in 0..60 { if pids.is_empty() { break; } std::thread::sleep(Duration::from_secs(1)); pids = running_orbit(&exe); }
            still_open = !pids.is_empty();
            if still_open { println!("Orbit is still open; updating anyway. Restart it afterwards to use the new version."); }
        }
    }
    let mut undo = vec![];
    for (name, bytes) in &files {
        match replace(&dir, name, bytes) {
            Ok(u) => undo.extend(u),
            Err(e) => {
                for (t, old) in undo.iter().rev() { let _ = std::fs::remove_file(t); let _ = std::fs::rename(old, t); }
                return Err(format!("{e}\nThe previous version was restored."));
            }
        }
    }
    println!("Updated to {}. The previous version is kept as {EXE}.old.", rel.version.trim_start_matches('v'));
    let _ = std::fs::write(dir.join(VERSION_FILE), rel.version.trim_start_matches(['v', 'V']));
    if o.launch && !still_open {
        if !start_orbit(&dir) { println!("Couldn't start Orbit automatically; open it from the Start menu / your launcher."); }
    }
    Ok(0)
}

fn main() {
    // Started from the Start Menu, Windows closes the console the moment we finish, which hides every message.
    let pause = cfg!(windows) && !std::env::args().any(|a| matches!(a.as_str(), "--yes" | "-y" | "--check" | "--json"));
    let json = std::env::args().any(|a| a == "--json");
    let code = match run() {
        Ok(c) => c,
        Err(e) if json => { println!("{}", serde_json::json!({"error": e})); 2 }
        Err(e) => { eprintln!("orbit-updater: {e}"); 2 }
    };
    if pause { print!("\nPress Enter to close. "); let _ = std::io::stdout().flush(); let _ = std::io::stdin().read_line(&mut String::new()); }
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(tag: &str) -> PathBuf { let d = std::env::temp_dir().join(format!("orbit-upd-{tag}-{:08x}", rand::random::<u32>())); std::fs::create_dir_all(&d).unwrap(); d }

    #[test]
    fn version_order() {
        assert!(is_newer("v0.3.0", "0.2.0") && is_newer("0.10.0", "0.9.9") && is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.2.0", "0.2.0") && !is_newer("0.2", "0.2.0") && !is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("0.2.0-beta", "0.2.0") && is_newer("0.2.0", "0.2.0-beta"));
        assert!(!is_newer("nightly", "0.2.0") && !is_newer("0.3.0", "dev"));
    }

    #[test]
    fn manifest_and_github_feeds() {
        let h = "a".repeat(64);
        let m = json!({"version": "0.3.0", "notes": "n", "assets": {"windows": {"url": "https://x/Orbit-windows.zip", "sha256": h}, "linux": {"url": "https://x/Orbit-linux.tar.gz"}}});
        let r = release_from_manifest(&m, "windows").unwrap();
        assert_eq!((r.name.as_str(), r.sha256.as_deref()), ("Orbit-windows.zip", Some(h.as_str())));
        assert_eq!(release_from_manifest(&m, "linux").unwrap().sha256, None);
        assert!(release_from_manifest(&m, "macos").is_err());
        let g = json!({"tag_name": "v0.3.0", "body": "b", "assets": [
            {"name": "Orbit-Setup.exe", "browser_download_url": "https://x/s"}, {"name": "Orbit.exe", "browser_download_url": "https://x/o", "digest": format!("sha256:{h}")}]});
        let r = release_from_github(&g, "windows").unwrap();
        assert_eq!((r.url.as_str(), r.sha256.as_deref()), ("https://x/o", Some(h.as_str())));
        assert!(release_from_github(&g, "linux").is_err());
    }

    #[test]
    fn extracts_zip_and_tar_gz() {
        let d = tmp("pay");
        let zp = d.join("a.zip");
        { let mut z = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
          z.start_file::<_, ()>(format!("Orbit/{EXE}"), zip::write::SimpleFileOptions::default()).unwrap(); z.write_all(b"EXE").unwrap();
          z.start_file::<_, ()>("readme.txt", zip::write::SimpleFileOptions::default()).unwrap(); z.write_all(b"x").unwrap(); z.finish().unwrap(); }
        assert_eq!(payload(&zp, "a.zip").unwrap(), vec![(EXE.to_string(), b"EXE".to_vec())]);
        let tp = d.join("a.tar.gz");
        { let gz = flate2::write::GzEncoder::new(std::fs::File::create(&tp).unwrap(), flate2::Compression::fast());
          let mut t = tar::Builder::new(gz);
          for (n, b) in [("Orbit-linux/orbit", &b"BIN"[..]), ("Orbit-linux/orbit-updater", b"UPD"), ("Orbit-linux/install.sh", b"sh")] {
              let mut h = tar::Header::new_gnu(); h.set_size(b.len() as u64); h.set_mode(0o755); h.set_cksum(); t.append_data(&mut h, n, b).unwrap(); }
          t.into_inner().unwrap().finish().unwrap(); }
        if !cfg!(windows) { let p = payload(&tp, "a.tar.gz").unwrap(); assert_eq!(p.len(), 2); assert_eq!(p[0], (EXE.to_string(), b"BIN".to_vec())); }
        assert!(payload(&zp, "b.zip").is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn replace_keeps_a_backup_and_works_from_nothing() {
        let d = tmp("rep");
        assert!(replace(&d, EXE, b"v1").unwrap().is_none());
        let undo = replace(&d, EXE, b"v2").unwrap().unwrap();
        assert_eq!(std::fs::read(d.join(EXE)).unwrap(), b"v2");
        assert_eq!(std::fs::read(&undo.1).unwrap(), b"v1");
        assert!(!d.join(format!("{EXE}.new")).exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
