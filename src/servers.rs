//! Orbit server manager (port of servers.py): run Minecraft servers from Orbit.
//!
//! Pelican/Pterodactyl-style "eggs" (Vanilla, Paper, Purpur, Folia, Spigot, Fabric, Quilt, Forge, NeoForge,
//! Velocity) create a ready-to-run server in one click. Existing server folders are picked up too. Each
//! server gets a live console, RAM settings, a Mods or Plugins tab and modpack installs for the modded eggs.
use crate::api::{run_job, Out, Req};
use crate::mods::{self, Log};
use crate::util::{self, download_file, Res};
use crate::{icons, launcher, meta, store};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WIN: bool = cfg!(windows);
const CFG_NAME: &str = "orbit-server.json";       // per-server settings Orbit keeps inside the server folder
const CONTENT_NAME: &str = "orbit-content.json";   // what Orbit installed into mods/ or plugins/
const ICON_NAME: &str = "server-icon.png";         // the file Minecraft itself shows in the multiplayer list (64x64)
const DEFAULT_RAM: i64 = 4096;

pub struct Egg { pub key: &'static str, pub name: &'static str, pub content: Option<&'static str>,
                 pub loaders: &'static [&'static str], pub mc: bool, pub proxy: bool, pub desc: &'static str }

pub const EGGS: [Egg; 10] = [
    Egg { key: "vanilla", name: "Vanilla", content: None, loaders: &[], mc: true, proxy: false, desc: "Mojang's own server. No mods or plugins." },
    Egg { key: "paper", name: "Paper", content: Some("plugins"), loaders: &["paper", "spigot", "bukkit"], mc: true, proxy: false,
          desc: "Fast, fixed-up Spigot fork. Runs Bukkit, Spigot and Paper plugins. The usual pick." },
    Egg { key: "purpur", name: "Purpur", content: Some("plugins"), loaders: &["purpur", "paper", "spigot", "bukkit"], mc: true, proxy: false,
          desc: "Paper with lots of gameplay options. Runs the same plugins." },
    Egg { key: "folia", name: "Folia", content: Some("plugins"), loaders: &["folia"], mc: true, proxy: false,
          desc: "Paper fork that spreads big worlds over many threads. Needs Folia-ready plugins." },
    Egg { key: "spigot", name: "Spigot", content: Some("plugins"), loaders: &["spigot", "bukkit"], mc: true, proxy: false,
          desc: "The original plugin server. Built on your PC with BuildTools, which needs Git." },
    Egg { key: "fabric", name: "Fabric", content: Some("mods"), loaders: &["fabric"], mc: true, proxy: false,
          desc: "Light, fast mod loader with a huge mod scene." },
    Egg { key: "quilt", name: "Quilt", content: Some("mods"), loaders: &["quilt", "fabric"], mc: true, proxy: false,
          desc: "Fabric-compatible mod loader with its own extras." },
    Egg { key: "forge", name: "Forge", content: Some("mods"), loaders: &["forge"], mc: true, proxy: false,
          desc: "The classic mod loader, best for older versions and big modpacks." },
    Egg { key: "neoforge", name: "NeoForge", content: Some("mods"), loaders: &["neoforge"], mc: true, proxy: false,
          desc: "Modern successor to Forge (Minecraft 1.20.2 and newer)." },
    Egg { key: "velocity", name: "Velocity", content: Some("plugins"), loaders: &["velocity"], mc: false, proxy: true,
          desc: "Modern proxy that links several servers together. Takes Velocity plugins." },
];
const CUSTOM: Egg = Egg { key: "custom", name: "Custom", content: None, loaders: &[], mc: true, proxy: false, desc: "" };

pub fn egg_info(egg: &str) -> &'static Egg { EGGS.iter().find(|e| e.key == egg).unwrap_or(&CUSTOM) }
fn is_egg(egg: &str) -> bool { EGGS.iter().any(|e| e.key == egg) }
fn rx(p: &str) -> Regex { Regex::new(p).unwrap() }
fn s<'a>(v: &'a Value, k: &str) -> &'a str { v[k].as_str().unwrap_or("") }
fn num(v: &Value) -> Option<i64> { v.as_i64().or_else(|| v.as_str().and_then(|x| x.trim().parse().ok())).or_else(|| v.as_f64().map(|f| f as i64)) }
fn err<T>(m: impl Into<String>) -> Res<T> { Err(m.into()) }


fn quiet(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }
    cmd
}

fn which(prog: &str) -> Option<PathBuf> {
    let exts: &[&str] = if WIN { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|d| exts.iter().map(|e| d.join(format!("{prog}{e}"))).find(|p| p.is_file()))
}

// ---------------------------------------------------------------- settings / folders

pub fn servers_dir() -> PathBuf {
    store::settings().get("servers_dir").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(PathBuf::from)
        .unwrap_or_else(|| store::data_home().join("servers"))
}

/// Settings > "Install missing dependencies automatically" (on unless switched off).
pub fn auto_deps() -> bool { store::settings().get("auto_deps").and_then(|v| v.as_bool()).unwrap_or(true) }
fn default_ram() -> i64 { store::settings().get("server_memory").and_then(num).filter(|n| *n != 0).unwrap_or(DEFAULT_RAM) }

fn stop_timeout() -> u64 {
    store::settings().get("server_stop_timeout").and_then(num).filter(|n| *n != 0).unwrap_or(60).clamp(10, 300) as u64
}

pub fn server_root(name: &str) -> Res<PathBuf> {
    if name.is_empty() || Path::new(name).file_name().map(|n| n.to_string_lossy() != name).unwrap_or(true) || name.starts_with('.') {
        return err("Unknown server.");
    }
    let p = servers_dir().join(name);
    if !p.is_dir() { return err(format!("Server '{name}' wasn't found in {}.", servers_dir().display())); }
    Ok(p)
}

fn read_json_file(p: &Path) -> Option<Value> { serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok() }
fn read_cfg(root: &Path) -> Value { read_json_file(&root.join(CFG_NAME)).filter(|v| v.is_object()).unwrap_or(json!({})) }
fn write_cfg(root: &Path, cfg: &Value) -> Res<()> {
    std::fs::write(root.join(CFG_NAME), serde_json::to_string_pretty(cfg).unwrap()).map_err(|e| e.to_string())
}
pub fn read_content(root: &Path) -> Value { read_json_file(&root.join(CONTENT_NAME)).filter(|v| v.is_object()).unwrap_or(json!({"items": {}})) }
pub fn write_content(root: &Path, data: &Value) -> Res<()> {
    std::fs::write(root.join(CONTENT_NAME), serde_json::to_string_pretty(data).unwrap()).map_err(|e| e.to_string())
}

fn list_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    v.sort();
    v
}

// ---------------------------------------------------------------- detection (works on folders Orbit didn't create)

fn has(names: &[String], pat: &str) -> bool { let r = rx(&format!("(?i){pat}")); names.iter().any(|n| r.is_match(n)) }

pub fn detect_egg(root: &Path) -> String {
    let cfg = read_cfg(root);
    if let Some(e) = cfg["egg"].as_str().filter(|e| is_egg(e)) { return e.into(); }
    let names = list_names(root);
    for (pat, egg) in [(r"^fabric-server-launch\.jar$", "fabric"), (r"^quilt-server-launch\.jar$", "quilt"),
                       (r"^velocity.*\.jar$", "velocity"), (r"^purpur.*\.jar$", "purpur"), (r"^folia.*\.jar$", "folia"),
                       (r"^neoforge.*\.jar$", "neoforge"), (r"paper.*\.jar$", "paper"), (r"(spigot|craftbukkit).*\.jar$", "spigot")] {
        if has(&names, pat) { return egg.into(); }
    }
    let lib = root.join("libraries").join("net");
    if lib.join("neoforged").is_dir() { return "neoforge".into(); }
    if lib.join("minecraftforge").is_dir() { return "forge".into(); }
    if root.join("plugins").is_dir() && !root.join("mods").is_dir() { return "paper".into(); }
    if root.join("mods").is_dir() {
        let mods = list_names(&root.join("mods"));
        if mods.join(" ").to_lowercase().contains("fabric") { return "fabric".into(); }
        if mods.iter().any(|f| f.ends_with(".jar")) { return "forge".into(); }
    }
    if has(&names, r"\.jar$") { "vanilla".into() } else { "custom".into() }
}

pub fn content_kind(root: &Path, egg: &str) -> Option<&'static str> {
    if let Some(k) = egg_info(egg).content { return Some(k); }
    if egg == "custom" {
        if root.join("mods").is_dir() { return Some("mods"); }
        if root.join("plugins").is_dir() { return Some("plugins"); }
    }
    None
}

fn guess_mc(root: &Path, cfg: &Value) -> String {
    if let Some(m) = cfg["mc"].as_str().filter(|m| !m.is_empty()) { return m.into(); }
    if let Ok(f) = std::fs::File::open(root.join("logs").join("latest.log")) {
        let r = rx(r"Starting minecraft server version (\S+)");
        for line in BufReader::new(f).split(b'\n').take(80).flatten() {
            if let Some(m) = r.captures(&String::from_utf8_lossy(&line)) { return m[1].to_string(); }
        }
    }
    if let Ok(t) = std::fs::read_to_string(root.join("version_history.json")) {
        if let Some(m) = rx(r"MC: ([\d.]+)").captures(&t) { return m[1].to_string(); }
    }
    let neo = root.join("libraries/net/neoforged/neoforge");
    if neo.is_dir() {
        let mut vs = list_names(&neo);
        vs.sort_by(|a, b| util::cmp_version_str(a, b));
        if let Some(v) = vs.last() { return meta::neo_mc(v); }
    }
    let srv = root.join("libraries/net/minecraft/server");
    if srv.is_dir() {
        let mut vs = list_names(&srv);
        vs.sort_by(|a, b| util::cmp_version_str(a, b));
        if let Some(v) = vs.last() { return v.split('-').next().unwrap_or("").into(); }
    }
    String::new()
}

pub fn read_properties(root: &Path) -> Map<String, Value> {
    let mut out = Map::new();
    if let Ok(t) = std::fs::read_to_string(root.join("server.properties")) {
        for line in t.lines() {
            let t = line.trim();
            if !t.is_empty() && !t.starts_with('#') {
                if let Some((k, v)) = t.split_once('=') { out.insert(k.trim().into(), json!(v)); }
            }
        }
    }
    out
}

pub fn write_properties(root: &Path, updates: &Map<String, Value>) -> Res<()> {
    let name_ok = rx(r"^[\w.-]+$");
    if updates.keys().any(|k| !name_ok.is_match(k)) { return err("Invalid property name."); }
    let nl = rx(r"[\r\n]+");
    let val = |v: &Value| nl.replace_all(&match v { Value::String(s) => s.clone(), o => o.to_string() }, " ").into_owned();
    let p = root.join("server.properties");
    let (mut seen, mut lines) = (HashSet::new(), vec![]);
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    for l in text.lines() {
        let t = l.trim();
        let mut line = l.to_string();
        if !t.is_empty() && !t.starts_with('#') && t.contains('=') {
            let k = t.split_once('=').unwrap().0.trim();
            if let Some(v) = updates.get(k) { seen.insert(k.to_string()); line = format!("{k}={}", val(v)); }
        }
        lines.push(line);
    }
    for (k, v) in updates { if !seen.contains(k) { lines.push(format!("{k}={}", val(v))); } }
    std::fs::write(&p, format!("{}\n", lines.join("\n").trim_end_matches('\n'))).map_err(|e| e.to_string())
}

fn xmx() -> Regex { rx(r"-Xmx(\d+)([GgMm])") }
fn mb_of(c: &regex::Captures) -> i64 { c[1].parse::<i64>().unwrap_or(0) * if c[2].eq_ignore_ascii_case("g") { 1024 } else { 1 } }

/// -Xmx from a server's own start scripts / jvm args files (MB), or None.
fn ram_from_files(root: &Path) -> Option<i64> {
    let mut cands: Vec<PathBuf> = ["start.bat", "run.bat", "start.sh", "run.sh", "user_jvm_args.txt", "java_opts.txt"].iter().map(|n| root.join(n)).collect();
    let r = rx(r"@(\S+\.txt)");
    for f in cands.clone() {
        if let Some(c) = std::fs::read_to_string(&f).ok().and_then(|t| r.captures(&t).map(|c| c[1].to_string())) { cands.push(root.join(c)); }
    }
    let x = xmx();
    for f in cands {
        let Ok(t) = std::fs::read_to_string(&f) else { continue };
        for line in t.lines() {
            let l = line.trim();
            if l.starts_with('#') || l.starts_with("rem ") || l.starts_with("REM ") { continue; }
            if let Some(m) = x.captures(line) { return Some(mb_of(&m)); }
        }
    }
    None
}

fn set_ram(root: &Path, cfg: &mut Value, mb: i64) {
    cfg["ram"] = json!(mb);
    if cfg["created"].as_bool().unwrap_or(false) { return; }
    let x = xmx();
    for n in ["start.bat", "run.bat", "start.sh", "run.sh", "user_jvm_args.txt", "java_opts.txt"] {
        let f = root.join(n);
        let Ok(t) = std::fs::read_to_string(&f) else { continue };
        if x.is_match(&t) { let _ = std::fs::write(&f, x.replace_all(&t, format!("-Xmx{mb}M").as_str()).into_owned()); return; }
    }
}

fn eula_state(root: &Path) -> Option<bool> {
    let t = std::fs::read_to_string(root.join("eula.txt")).ok()?;
    Some(rx(r"(?m)^eula\s*=\s*(\w+)").captures(&t).map(|m| m[1].eq_ignore_ascii_case("true")).unwrap_or(false))
}

fn accept_eula(root: &Path) -> Res<()> {
    std::fs::write(root.join("eula.txt"), "# By changing the setting below to TRUE you are indicating your agreement to the Minecraft EULA (https://aka.ms/MinecraftEULA).\neula=true\n").map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- processes

#[derive(Clone)]
struct JProc { pid: u32, mb: f64, cmd: String, cwd: String }
static PCACHE: Mutex<Option<(Instant, Vec<JProc>)>> = Mutex::new(None);

/// Running java processes (cached for a few seconds, shared by every server).
fn java_procs() -> Vec<JProc> {
    let mut g = PCACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, v)) = g.as_ref() { if t.elapsed() < Duration::from_millis(2500) { return v.clone(); } }
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true,
        ProcessRefreshKind::new().with_memory().with_cmd(UpdateKind::Always).with_cwd(UpdateKind::Always));
    let out: Vec<JProc> = sys.processes().iter().filter_map(|(pid, p)| {
        let name = p.name().to_string_lossy().to_lowercase();
        if !["java", "java.exe", "javaw", "javaw.exe"].contains(&name.as_str()) { return None; }
        Some(JProc { pid: pid.as_u32(), mb: p.memory() as f64 / 1048576.0,
                     cmd: p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" "),
                     cwd: p.cwd().map(|c| c.to_string_lossy().into_owned()).unwrap_or_default() })
    }).collect();
    *g = Some((Instant::now(), out.clone()));
    out
}
fn bust_pcache() { *PCACHE.lock().unwrap_or_else(|e| e.into_inner()) = None; }

fn norm(p: &str) -> String { format!("{}/", p.replace('\\', "/").to_lowercase().trim_end_matches('/')) }

fn procs_for(root: &Path) -> Vec<JProc> {
    let d = norm(&root.to_string_lossy());
    java_procs().into_iter().filter(|p| norm(&p.cmd).contains(&d) || (!p.cwd.is_empty() && norm(&p.cwd).starts_with(&d))).collect()
}

/// A server started by Orbit: we own its stdin and stdout, so there is a live console and a clean stop.
struct Run {
    alive: AtomicBool, ready: AtomicBool, stopping: AtomicBool, pid: AtomicU32, started: f64,
    lines: Mutex<(Vec<String>, usize)>, stdin: Mutex<Option<ChildStdin>>,
}
impl Run {
    fn new() -> Run {
        Run { alive: AtomicBool::new(true), ready: AtomicBool::new(false), stopping: AtomicBool::new(false), pid: AtomicU32::new(0),
              started: store::now(), lines: Mutex::new((vec![], 0)), stdin: Mutex::new(None) }
    }
    fn add(&self, line: impl Into<String>) {
        let mut g = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        g.0.push(line.into());
        if g.0.len() > 3000 { g.0.drain(..1000); g.1 += 1000; }
    }
    fn read(&self, since: usize) -> (Vec<String>, usize) {
        let g = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        (g.0[since.saturating_sub(g.1).min(g.0.len())..].to_vec(), g.1 + g.0.len())
    }
    fn write_line(&self, text: &str) -> std::io::Result<()> {
        let mut g = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_mut() {
            Some(i) => { i.write_all(format!("{text}\n").as_bytes())?; i.flush() }
            None => Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "no stdin")),
        }
    }
}

static RUNS: Mutex<Option<HashMap<String, Arc<Run>>>> = Mutex::new(None);
fn run_of(name: &str) -> Option<Arc<Run>> { RUNS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(name).cloned()) }

pub fn status(name: &str, root: &Path) -> Value {
    if let Some(run) = run_of(name).filter(|r| r.alive.load(Relaxed)) {
        let pid = run.pid.load(Relaxed);
        let procs = java_procs();
        let mem = procs.iter().filter(|p| p.pid == pid).map(|p| p.mb).chain(procs_for(root).iter().map(|p| p.mb)).fold(0.0, f64::max);
        let state = if run.stopping.load(Relaxed) { "stopping" } else if run.ready.load(Relaxed) { "running" } else { "starting" };
        return json!({"running": true, "state": state, "managed": true, "pid": if pid != 0 { json!(pid) } else { Value::Null },
                      "ram_mb": mem as i64, "uptime": (store::now() - run.started) as i64});
    }
    let ext = procs_for(root);
    if let Some(best) = ext.iter().max_by(|a, b| a.mb.partial_cmp(&b.mb).unwrap_or(std::cmp::Ordering::Equal)) {
        return json!({"running": true, "state": "running", "managed": false, "pid": best.pid, "ram_mb": best.mb as i64, "uptime": 0});
    }
    json!({"running": false, "state": "stopped", "managed": false, "pid": null, "ram_mb": 0, "uptime": 0})
}

/// `status` plus, for a server that is up, how many players are on it right now (asked the same way the game's
/// server list does, over the server's own port). Kept out of `status` so list views never open sockets.
pub fn live_status(name: &str, root: &Path) -> Value {
    let mut st = status(name, root);
    st["players"] = Value::Null;
    st["max_players"] = Value::Null;
    st["player_names"] = json!([]);
    if st["state"] == "running" {
        let cfg = read_cfg(root);
        let port = read_properties(root).get("server-port").and_then(|p| p.as_str().and_then(|p| p.trim().parse::<u16>().ok()))
            .or_else(|| cfg.get("port").and_then(num).map(|p| p as u16)).unwrap_or(25565);
        let r = crate::gamedata::ping_timeout(&format!("127.0.0.1:{port}"), Duration::from_millis(700));
        if r["online"] == true {
            st["players"] = r["players"].clone();
            st["max_players"] = r["max"].clone();
            st["player_names"] = r["sample"].clone();
        }
    }
    st
}

/// `status` for the Servers overview: a running server also reports `players` / `max_players` (online/max), but never
/// the player names. Stopped or still-starting servers open no socket.
fn list_status(name: &str, root: &Path) -> Value {
    let mut st = live_status(name, root);
    if let Some(o) = st.as_object_mut() { o.remove("player_names"); }
    st
}

fn kill_tree(pid: u32) {
    let mut c;
    if WIN {
        c = Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/T", "/F"]);
    } else {                                                  // started with its own process group: pgid == pid
        c = Command::new("kill");
        c.args(["-9", "--", &format!("-{pid}")]);
    }
    let _ = quiet(&mut c).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

fn pick_jar(root: &Path, cfg: &Value, _egg: &str) -> Option<String> {
    if let Some(j) = cfg["jar"].as_str().filter(|j| !j.is_empty() && root.join(j).is_file()) { return Some(j.into()); }
    let bad = rx(r"(?i)installer|buildtools");
    let jars: Vec<String> = list_names(root).into_iter().filter(|f| f.to_lowercase().ends_with(".jar") && !bad.is_match(f)).collect();
    for pat in [r"(?i)^(fabric|quilt)-server-launch", r"(?i)^server\.jar$", r"(?i)paper|purpur|folia|velocity|spigot|forge|minecraft_server|server"] {
        let r = rx(pat);
        if let Some(j) = jars.iter().find(|j| r.is_match(j)) { return Some(j.clone()); }
    }
    jars.first().cloned()
}

fn rglob(dir: &Path, file: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() { rglob(&p, file, out) } else if p.file_name().map(|n| n == file).unwrap_or(false) { out.push(p) }
    }
}

fn forge_args(root: &Path) -> Option<String> {
    let mut found = vec![];
    rglob(&root.join("libraries"), if WIN { "win_args.txt" } else { "unix_args.txt" }, &mut found);
    found.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    found.last().map(|p| p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/"))
}

fn write_jvm_args(root: &Path, ram: i64, extra: &[String]) -> Res<()> {
    let f = root.join("user_jvm_args.txt");
    let x = rx(r"^\s*-X(ms|mx)");
    let mut lines: Vec<String> = std::fs::read_to_string(&f).map(|t| t.lines().filter(|l| !x.is_match(l)).map(String::from).collect()).unwrap_or_default();
    lines.push(format!("-Xms{}M", ram.min(1024)));
    lines.push(format!("-Xmx{ram}M"));
    lines.extend(extra.iter().cloned());
    std::fs::write(&f, format!("{}\n", lines.join("\n"))).map_err(|e| e.to_string())
}

fn set_label(msg: &str) { if let Some(t) = util::track() { t.lock().unwrap_or_else(|e| e.into_inner()).label = msg.to_string(); } }

/// Java for a Minecraft version: the user's pick, else Mojang's runtime for that version, else system Java.
fn get_java(mc: &str, cfg_java: &str) -> Res<String> {
    // Unknown Minecraft version: Java 21. A known one whose profile can't be fetched is an error, never an empty profile (that would pick Java 8).
    let mut prof = json!({"javaVersion": {"component": "java-runtime-delta", "majorVersion": 21}});
    if !mc.is_empty() {
        prof = meta::vanilla_profile(mc).map_err(|e| format!("Couldn't find out which Java Minecraft {mc} needs ({e}). Check your connection and try again."))?;
    }
    let mut st = store::settings();
    if !cfg_java.is_empty() { st.insert("java_path".into(), json!(cfg_java)); }
    let j = launcher::find_java(&prof, &st, &store::meta_dir(), &|m: &str, _f: f64| set_label(m))?;
    let alt = j.with_file_name("java.exe");
    Ok(if j.file_name().map(|n| n.to_string_lossy().to_lowercase() == "javaw.exe").unwrap_or(false) && alt.exists() { alt } else { j }.to_string_lossy().into_owned())
}

/// (command list, working dir) for starting this server.
fn start_command(root: &Path, cfg: &Value) -> Res<(Vec<String>, PathBuf)> {
    let egg = detect_egg(root);
    if !cfg["created"].as_bool().unwrap_or(false) {
        for n in if WIN { ["run.bat", "start.bat"] } else { ["run.sh", "start.sh"] } {
            if root.join(n).is_file() {
                let cmd = if WIN { vec!["cmd.exe".into(), "/c".into(), n.into()] }
                          else { vec![which("bash").map(|p| p.to_string_lossy().into_owned()).unwrap_or("sh".into()), n.into()] };
                return Ok((cmd, root.to_path_buf()));
            }
        }
    }
    let ram = cfg.get("ram").and_then(num).filter(|r| *r != 0).or_else(|| ram_from_files(root)).unwrap_or_else(default_ram);
    let java = get_java(&{ let m = s(cfg, "mc"); if m.is_empty() { guess_mc(root, cfg) } else { m.into() } }, s(cfg, "java"))?;
    let extra = launcher::shlex_split(s(cfg, "jvm"));
    if (egg == "forge" || egg == "neoforge") && forge_args(root).is_some() {
        write_jvm_args(root, ram, &extra)?;
        return Ok((vec![java, "@user_jvm_args.txt".into(), format!("@{}", forge_args(root).unwrap()), "nogui".into()], root.to_path_buf()));
    }
    let jar = pick_jar(root, cfg, &egg).ok_or("Orbit couldn't find a server .jar in this folder.")?;
    let mut cmd = vec![java, format!("-Xms{}M", ram.min(1024)), format!("-Xmx{ram}M")];
    cmd.extend(extra);
    cmd.extend(["-jar".into(), jar]);
    if egg != "velocity" { cmd.push("nogui".into()); }
    Ok((cmd, root.to_path_buf()))
}

fn pump(run: Arc<Run>, root: PathBuf, seen_mc: bool, rd: impl Read, watch_ready: bool) {
    let (mut seen, mut buf) = (seen_mc, Vec::new());
    let mut br = BufReader::new(rd);
    let (ver, ready) = (rx(r"Starting minecraft server version (\S+)"), rx(r"Done \([\d.,]+s\)!|Done \(\d+ms\)|Listening on"));
    let ok = rx(r"^[\w.\-]+$");
    loop {
        buf.clear();
        match br.read_until(b'\n', &mut buf) { Ok(0) | Err(_) => break, Ok(_) => {} }
        let line = String::from_utf8_lossy(&buf).trim_end_matches(['\r', '\n']).to_string();
        if watch_ready {
            if !seen {
                if let Some(m) = ver.captures(&line).filter(|m| ok.is_match(&m[1])) {
                    seen = true;
                    let mut c = read_cfg(&root);
                    if c["mc"].is_null() { c["mc"] = json!(&m[1]); let _ = write_cfg(&root, &c); }
                }
            }
            if !run.ready.load(Relaxed) && ready.is_match(&line) { run.ready.store(true, Relaxed); }
        }
        run.add(line);
    }
}

fn launch(run: Arc<Run>, root: PathBuf, cfg: Value) {
    let result = (|| -> Res<()> {
        run.add("[Orbit] Getting ready to start...");
        if auto_deps() && content_dir(&root).is_ok() {
            run.add("[Orbit] Checking dependencies of mods / plugins...");
            let mut out = None;
            let (log, _) = run_job(&None, |log| { out = ensure_deps(&root, log); Ok(()) });
            if let Some(o) = out {
                for l in log.iter().filter(|l| l.starts_with('+')).chain(crate::deps::describe(&o).iter()) { run.add(format!("[Orbit] {}", l.trim())); }
            }
        }
        let (cmd, cwd) = start_command(&root, &cfg)?;
        run.add(format!("[Orbit] {}", cmd.join(" ")));
        let mut c = Command::new(&cmd[0]);
        c.args(&cmd[1..]).current_dir(&cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        { use std::os::windows::process::CommandExt; c.creation_flags(0x08000000); }
        #[cfg(unix)]
        { use std::os::unix::process::CommandExt; c.process_group(0); }
        let mut child: Child = c.spawn().map_err(|e| format!("Couldn't start: {e}"))?;
        run.pid.store(child.id(), Relaxed);
        *run.stdin.lock().unwrap_or_else(|e| e.into_inner()) = child.stdin.take();
        let seen = !s(&cfg, "mc").is_empty();
        let (r2, root2) = (run.clone(), root.clone());
        let errs = child.stderr.take().map(|e| std::thread::spawn(move || pump(r2, root2, true, e, false)));
        if let Some(o) = child.stdout.take() { pump(run.clone(), root.clone(), seen, o, true); }
        if let Some(h) = errs { let _ = h.join(); }
        let code = child.wait().map_err(|e| e.to_string())?.code();
        run.add(format!("[Orbit] Process exited (code {})", code.map(|c| c.to_string()).unwrap_or("None".into())));
        Ok(())
    })();
    if let Err(e) = result { run.add(format!("[Orbit] {e}")); }
    run.alive.store(false, Relaxed);
    *run.stdin.lock().unwrap_or_else(|e| e.into_inner()) = None;
    bust_pcache();
}

pub fn start(name: &str) -> Res {
    let root = server_root(name)?;
    if status(name, &root)["running"] == true { return err(format!("{name} is already running.")); }
    let run = Arc::new(Run::new());
    {
        let mut g = RUNS.lock().unwrap_or_else(|e| e.into_inner());
        let m = g.get_or_insert_with(HashMap::new);
        if let Some(old) = m.get(name) {
            let (lines, _) = old.read(0);
            let keep: Vec<String> = lines[lines.len().saturating_sub(500)..].to_vec();
            let mut l = run.lines.lock().unwrap();
            l.0 = keep;
            l.0.push("--------------------------------------------------".into());
        }
        m.insert(name.into(), run.clone());
    }
    let cfg = read_cfg(&root);
    std::thread::spawn(move || launch(run, root, cfg));
    Ok(json!({"started": true}))
}

pub fn stop(name: &str, force: bool) -> Res {
    let root = server_root(name)?;
    if let Some(run) = run_of(name).filter(|r| r.alive.load(Relaxed)) {
        run.stopping.store(true, Relaxed);
        if force {
            let pid = run.pid.load(Relaxed);
            if pid != 0 { kill_tree(pid); }
            return Ok(json!({"stopping": true}));
        }
        let _ = run.write_line(if detect_egg(&root) == "velocity" { "end" } else { "stop" });
        let secs = stop_timeout();
        std::thread::spawn(move || {
            for _ in 0..secs {
                std::thread::sleep(Duration::from_secs(1));
                if !run.alive.load(Relaxed) { return; }
            }
            run.add("[Orbit] The server didn't stop in time, closing it.");
            let pid = run.pid.load(Relaxed);
            if pid != 0 { kill_tree(pid); }
        });
        return Ok(json!({"stopping": true}));
    }
    let ext = procs_for(&root);
    if ext.is_empty() { return err(format!("{name} isn't running.")); }
    for p in ext {
        if WIN { kill_tree(p.pid); } else { let _ = Command::new("kill").args(["-TERM", &p.pid.to_string()]).status(); }
    }
    bust_pcache();
    Ok(json!({"stopping": true}))
}

pub fn send_command(name: &str, command: &str) -> Res {
    let command = rx(r"[\r\n]+").replace_all(command, " ").trim().to_string();
    let run = run_of(name).filter(|r| r.alive.load(Relaxed)).ok_or("Commands only work for servers started from Orbit.")?;
    if command.is_empty() { return err("Type a command first."); }
    run.write_line(&command).map_err(|e| format!("Couldn't send that: {e}"))?;
    run.add(format!("> {command}"));
    Ok(json!({"ok": true}))
}

pub fn console(name: &str, since: usize) -> Res {
    let root = server_root(name)?;
    if let Some(run) = run_of(name) {
        let (mut lines, mut total) = run.read(since);
        let reset = since > total;
        if reset { let r = run.read(0); lines = r.0; total = r.1; }
        return Ok(json!({"mode": if run.alive.load(Relaxed) { "live" } else { "ended" }, "lines": lines, "seq": total, "reset": reset}));
    }
    let f = root.join("logs").join("latest.log");
    let Ok(size) = std::fs::metadata(&f).map(|m| m.len() as usize) else { return Ok(json!({"mode": "none", "lines": [], "seq": 0, "reset": false})) };
    if size == since { return Ok(json!({"mode": "log", "lines": [], "seq": size, "reset": false})); }
    use std::io::Seek;
    let mut fh = std::fs::File::open(&f).map_err(|e| e.to_string())?;
    let _ = fh.seek(std::io::SeekFrom::Start(size.saturating_sub(65536) as u64));
    let mut raw = Vec::new();
    let _ = fh.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let all: Vec<&str> = text.lines().collect();
    Ok(json!({"mode": "log", "lines": all[all.len().saturating_sub(400)..], "seq": size, "reset": true}))
}

// ---------------------------------------------------------------- listing

fn summary(root: &Path) -> Value {
    let name = root.file_name().unwrap().to_string_lossy().into_owned();
    let cfg = read_cfg(root);
    let egg = detect_egg(root);
    let info = egg_info(&egg);
    let kind = content_kind(root, &egg);
    let props = read_properties(root);
    let ram = cfg.get("ram").and_then(num).filter(|r| *r != 0).or_else(|| ram_from_files(root));
    let count = kind.filter(|k| root.join(k).is_dir()).map(|k| list_names(&root.join(k)).iter().filter(|f| { let l = f.to_lowercase(); l.ends_with(".jar") || l.ends_with(".jar.disabled") }).count()).unwrap_or(0);
    let port = props.get("server-port").and_then(num).or_else(|| cfg.get("port").and_then(num)).unwrap_or(25565);
    let icon = std::fs::metadata(root.join(ICON_NAME)).and_then(|m| m.modified()).ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs().to_string()).unwrap_or_default();
    let (ar_on, ar_at, ar_cd) = auto_restart_cfg(&cfg);
    let times: Vec<String> = ar_at.iter().map(|t| format!("{:02}:{:02}", t / 60, t % 60)).collect();
    let ar = json!({"enabled": ar_on, "time": times[0], "times": times, "countdown": ar_cd});
    json!({"name": name, "icon": icon, "auto_restart": ar, "egg": egg, "egg_name": info.name, "mc": if info.proxy { String::new() } else { guess_mc(root, &cfg) },
           "loader_version": s(&cfg, "loader_version"), "content": kind, "count": count, "ram": ram, "ram_default": ram.is_none(),
           "port": port, "motd": props.get("motd").and_then(|v| v.as_str()).unwrap_or(""),
           "max_players": props.get("max-players").and_then(|v| v.as_str()).unwrap_or(""),
           "created": cfg["created"].as_bool().unwrap_or(false), "proxy": info.proxy, "status": list_status(&name, root), "eula": eula_state(root)})
}

pub fn list_servers() -> Res {
    let d = servers_dir();
    let _ = std::fs::create_dir_all(&d);
    let mut roots: Vec<PathBuf> = std::fs::read_dir(&d).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir() && !p.file_name().unwrap().to_string_lossy().starts_with('.')).collect()).unwrap_or_default();
    roots.sort_by_key(|p| p.file_name().unwrap().to_string_lossy().to_lowercase());
    java_procs();                                           // scan once up front so the workers share the result
    let items: Vec<Value> = mods::par_map(&roots, 8, |r| summary(r));
    let used: HashSet<i64> = items.iter().filter_map(|i| i["port"].as_i64()).collect();
    let mut port = 25565;
    while used.contains(&port) { port += 1; }
    Ok(json!({"servers": items, "dir": d.to_string_lossy(), "default_ram": default_ram(), "next_port": port,
              "stop_timeout": stop_timeout(), "default_dir": store::data_home().join("servers").to_string_lossy()}))
}

pub fn detail(name: &str) -> Res {
    let root = server_root(name)?;
    let mut sm = summary(&root);
    let cfg = read_cfg(&root);
    sm["java"] = json!(s(&cfg, "java")); sm["jvm"] = json!(s(&cfg, "jvm")); sm["modpack"] = cfg["modpack"].clone();
    sm["properties"] = Value::Object(read_properties(&root)); sm["path"] = json!(root.to_string_lossy());
    sm["loaders"] = json!(egg_info(s(&sm, "egg")).loaders);
    Ok(sm)
}

// ---------------------------------------------------------------- eggs: versions + install

fn mc_releases() -> Res<Vec<String>> {
    let m = util::get_json(meta::MANIFEST)?;
    Ok(m["versions"].as_array().into_iter().flatten().filter(|v| v["type"] == "release").filter_map(|v| v["id"].as_str().map(String::from)).collect())
}

fn papermc_versions(project: &str) -> Res<Vec<String>> {
    let mut out: Vec<String> = vec![];
    if let Ok(j) = util::get_json(&format!("https://fill.papermc.io/v3/projects/{project}")) {
        match &j["versions"] {
            Value::Object(m) => out = m.values().flat_map(|g| g.as_array().cloned().unwrap_or_default()).filter_map(|x| x.as_str().map(String::from)).collect(),
            Value::Array(a) => out = a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            _ => {}
        }
    }
    if out.is_empty() {
        let j = util::get_json(&format!("https://api.papermc.io/v2/projects/{project}"))?;
        out = j["versions"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
    }
    let st = rx(r"^\d+(\.\d+)*$");
    let stable: Vec<String> = out.iter().filter(|v| st.is_match(v)).cloned().collect();
    let mut v = if stable.is_empty() { out } else { stable };
    v.sort_by(|a, b| util::cmp_version_str(b, a));
    Ok(v)
}

fn papermc_download(project: &str, v: &str) -> Res<(String, Option<String>)> {
    if let Ok(b) = util::get_json(&format!("https://fill.papermc.io/v3/projects/{project}/versions/{v}/builds/latest")) {
        let d = &b["downloads"]["server:default"];
        if let Some(u) = d["url"].as_str() { return Ok((u.into(), d["checksums"]["sha256"].as_str().map(String::from))); }
    }
    let b = util::get_json(&format!("https://api.papermc.io/v2/projects/{project}/versions/{v}/builds"))?;
    let all: Vec<Value> = b["builds"].as_array().cloned().unwrap_or_default();
    let def: Vec<&Value> = all.iter().filter(|x| x["channel"].as_str().unwrap_or("default") == "default").collect();
    let x: &Value = def.last().copied().or(all.last()).ok_or_else(|| format!("{} has no build for {v}.", egg_info(project).name))?;
    let d = &x["downloads"]["application"];
    Ok((format!("https://api.papermc.io/v2/projects/{project}/versions/{v}/builds/{}/downloads/{}", x["build"], s(d, "name")), d["sha256"].as_str().map(String::from)))
}

static VC: Mutex<Option<HashMap<String, (Instant, Value)>>> = Mutex::new(None);

pub fn egg_versions(egg: &str) -> Res {
    if !is_egg(egg) { return err("Unknown server type."); }
    if let Some((t, v)) = VC.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(egg)) {
        if t.elapsed() < Duration::from_secs(600) { return Ok(v.clone()); }
    }
    let vs: Vec<String> = match egg {
        "vanilla" | "spigot" => mc_releases()?,
        "paper" | "folia" | "velocity" => papermc_versions(egg)?,
        "purpur" => { let mut v: Vec<String> = util::get_json("https://api.purpurmc.org/v2/purpur")?["versions"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect(); v.reverse(); v }
        "fabric" | "quilt" => {
            let base = if egg == "fabric" { "https://meta.fabricmc.net/v2/versions/game" } else { "https://meta.quiltmc.org/v3/versions/game" };
            util::get_json(base)?.as_array().into_iter().flatten().filter(|v| v["stable"].as_bool().unwrap_or(false)).filter_map(|v| v["version"].as_str().map(String::from)).collect()
        }
        "forge" => meta::forge_game_versions()?,
        _ => meta::neo_game_versions()?,
    };
    let res = json!({"latest": vs.first().cloned().unwrap_or_default(), "versions": vs});
    VC.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(egg.into(), (Instant::now(), res.clone()));
    Ok(res)
}

pub fn loader_versions(egg: &str, mc: &str) -> Res {
    if ["fabric", "quilt", "forge", "neoforge"].contains(&egg) { return meta::loader_versions(egg, mc); }
    Ok(json!({"versions": [], "stable": "", "latest": ""}))
}

fn run_cmd(cmd: &[String], cwd: &Path, label: &str) -> Res<()> {
    set_label(label);
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]).current_dir(cwd).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = quiet(&mut c).spawn().map_err(|e| format!("{label} failed: {e}"))?;
    let tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let mut hs = vec![];
    for rd in [child.stdout.take().map(|r| Box::new(r) as Box<dyn Read + Send>), child.stderr.take().map(|r| Box::new(r) as Box<dyn Read + Send>)].into_iter().flatten() {
        let (tail, label) = (tail.clone(), label.to_string());
        hs.push(std::thread::spawn(move || {
            for line in BufReader::new(rd).split(b'\n').flatten() {
                let l = String::from_utf8_lossy(&line).trim().to_string();
                if l.is_empty() { continue; }
                set_label(&format!("{label}: {}", l.chars().take(60).collect::<String>()));
                let mut t = tail.lock().unwrap();
                t.push(l);
                if t.len() > 6 { t.remove(0); }
            }
        }));
    }
    for h in hs { let _ = h.join(); }
    let st = child.wait().map_err(|e| e.to_string())?;
    if !st.success() {
        let t = tail.lock().unwrap();
        return err(format!("{label} failed (exit code {}). {}", st.code().unwrap_or(-1), t[t.len().saturating_sub(3)..].join(" | ")));
    }
    Ok(())
}

/// Download / build the server software into root. Returns the settings Orbit should remember.
fn install_egg(root: &Path, egg: &str, mc: &str, lv: &str) -> Res<Map<String, Value>> {
    let info = egg_info(egg);
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let mut out = Map::new();
    out.insert("mc".into(), json!(if info.mc { mc } else { "" }));
    out.insert("loader_version".into(), json!(lv));
    if info.mc && mc.is_empty() { return err("Pick a Minecraft version."); }
    let jar = |out: &mut Map<String, Value>, j: &str| { out.insert("jar".into(), json!(j)); };
    match egg {
        "vanilla" => {
            let d = meta::vanilla_profile(mc)?["downloads"]["server"].clone();
            if d.is_null() { return err(format!("Minecraft {mc} has no server download.")); }
            download_file(s(&d, "url"), root, "server.jar", Some("sha1"), d["sha1"].as_str())?;
            jar(&mut out, "server.jar");
        }
        "paper" | "folia" | "velocity" => {
            let (url, sha) = papermc_download(egg, mc)?;
            download_file(&url, root, "server.jar", sha.as_ref().map(|_| "sha256"), sha.as_deref())?;
            jar(&mut out, "server.jar");
        }
        "purpur" => {
            let b = util::get_json(&format!("https://api.purpurmc.org/v2/purpur/{mc}/latest"))?["build"].as_str().map(String::from).ok_or("Purpur has no build for that version.")?;
            // The build's own page lists the jar's MD5
            let md5 = util::get_json(&format!("https://api.purpurmc.org/v2/purpur/{mc}/{b}")).ok().and_then(|j| j["md5"].as_str().map(String::from));
            download_file(&format!("https://api.purpurmc.org/v2/purpur/{mc}/{b}/download"), root, "server.jar", md5.as_ref().map(|_| "md5"), md5.as_deref())?;
            jar(&mut out, "server.jar");
        }
        "fabric" => {
            let loaders = util::get_json(&format!("https://meta.fabricmc.net/v2/versions/loader/{mc}"))?;
            let ls = loaders.as_array().filter(|l| !l.is_empty()).ok_or_else(|| format!("Fabric has no loader for Minecraft {mc}."))?;
            let l = if !lv.is_empty() { lv.to_string() } else {
                ls.iter().find(|x| x["loader"]["stable"].as_bool().unwrap_or(false)).unwrap_or(&ls[0])["loader"]["version"].as_str().unwrap_or("").to_string() };
            let ins = util::get_json("https://meta.fabricmc.net/v2/versions/installer")?;
            let ia = ins.as_array().cloned().unwrap_or_default();
            let i = ia.iter().find(|x| x["stable"].as_bool().unwrap_or(false)).or(ia.first()).map(|x| s(x, "version").to_string()).ok_or("Fabric's installer list is empty right now.")?;
            download_file(&format!("https://meta.fabricmc.net/v2/versions/loader/{mc}/{l}/{i}/server/jar"), root, "fabric-server-launch.jar", None, None)?;
            jar(&mut out, "fabric-server-launch.jar");
            out.insert("loader_version".into(), json!(l));
        }
        "quilt" => {
            // the stable installer (like Fabric's), checked against the .sha1 its maven repository publishes
            let ins = util::get_json("https://meta.quiltmc.org/v3/versions/installer")?;
            let ia = ins.as_array().cloned().unwrap_or_default();
            let url = ia.iter().find(|x| x["stable"].as_bool().unwrap_or_else(|| !s(x, "version").contains('-'))).or(ia.first())
                .and_then(|x| x["url"].as_str()).ok_or("Quilt's installer list is empty right now.")?.to_string();
            meta::download_checked_installer(&url, root, "quilt-installer.jar")?;
            // pin the loader version so it can be stored (and shown) with the server
            let l = if lv.is_empty() { meta::loader_versions("quilt", mc)?["stable"].as_str().unwrap_or("").to_string() } else { lv.to_string() };
            let mut args: Vec<String> = vec!["-jar".into(), "quilt-installer.jar".into(), "install".into(), "server".into(), mc.into()];
            if !l.is_empty() { args.push(l.clone()); }
            args.extend(["--download-server".into(), "--install-dir=.".into()]);
            let mut cmd = vec![get_java(mc, "")?];
            cmd.extend(args);
            run_cmd(&cmd, root, "Installing Quilt")?;
            let _ = std::fs::remove_file(root.join("quilt-installer.jar"));
            jar(&mut out, "quilt-server-launch.jar");
            out.insert("loader_version".into(), json!(l));
        }
        "forge" | "neoforge" => {
            let (v, url) = meta::installer_info(egg, mc, if lv.is_empty() { None } else { Some(lv) })?;
            meta::download_checked_installer(&url, root, "installer.jar")?;
            run_cmd(&[get_java(mc, "")?, "-jar".into(), "installer.jar".into(), "--installServer".into()], root, &format!("Installing {}", info.name))?;
            let _ = std::fs::remove_file(root.join("installer.jar"));
            let _ = std::fs::remove_file(root.join("installer.jar.log"));
            out.insert("loader_version".into(), json!(v));
            // Forge 1.12 and older have no @args file: the server is a plain `forge-<mc>-<build>[-<mc>]-universal.jar`.
            if egg == "forge" && forge_args(root).is_none() {
                let mut c: Vec<String> = list_names(root).into_iter().filter(|n| { let l = n.to_lowercase(); l.starts_with("forge-") && l.ends_with(".jar") && !l.contains("installer") }).collect();
                c.sort();
                match c.first() { Some(j) => jar(&mut out, j), None => return err("The Forge installer finished but left no server jar behind.") }
            }
        }
        "spigot" => {
            if which("git").is_none() {
                return err("Spigot is built on your PC with BuildTools, which needs Git. Install Git, or pick Paper (it runs the same plugins).");
            }
            let bt = root.join("_buildtools");
            std::fs::create_dir_all(&bt).map_err(|e| e.to_string())?;
            download_file("https://hub.spigotmc.org/jenkins/job/BuildTools/lastSuccessfulBuild/artifact/target/BuildTools.jar", &bt, "BuildTools.jar", None, None)?;
            run_cmd(&[get_java(mc, "")?, "-jar".into(), "BuildTools.jar".into(), "--rev".into(), mc.into()], &bt, "Building Spigot (a few minutes)")?;
            let mut jars: Vec<String> = list_names(&bt).into_iter().filter(|n| n.starts_with("spigot-") && n.ends_with(".jar")).collect();
            jars.sort();
            let j = jars.first().ok_or("BuildTools finished but no Spigot jar was produced.")?.clone();
            std::fs::copy(bt.join(&j), root.join(&j)).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_dir_all(&bt);
            jar(&mut out, &j);
        }
        _ => return err("Unknown server type."),
    }
    Ok(out)
}

fn clean_name(raw: &str) -> Res<String> {
    let n = rx(r"[^A-Za-z0-9 _.-]").replace_all(raw, "").trim_matches([' ', '.']).to_string();
    if n.is_empty() { return err("Give the server a name (letters, numbers, spaces, - _ .)."); }
    let n = if n.starts_with(['_', '.']) { format!("S{n}") } else { n };
    Ok(n.chars().take(48).collect())
}

/// Run `f` as the one active job; a failure becomes a plain error.
fn job<F: FnOnce(&mut Log) -> Res<()>>(task: &Option<String>, f: F) -> Res<Log> {
    let (log, e) = run_job(task, f);
    match e { Some(e) => Err(e), None => Ok(log) }
}

pub fn create_server(task: &Option<String>, b: &Value) -> Res {
    let name = clean_name(s(b, "name"))?;
    let root = servers_dir().join(&name);
    if root.exists() { return err(format!("A server called '{name}' already exists.")); }
    if !b["eula"].as_bool().unwrap_or(false) && b["eula"].as_str().map(|e| e.is_empty() || e == "false").unwrap_or(true) { return err("Accept the Minecraft EULA to create a server."); }
    let ram = if b["ram"].is_null() || b["ram"] == 0 || b["ram"] == "" { default_ram() } else { num(&b["ram"]).ok_or("RAM must be a number of MB.")? };
    if !(512..=262144).contains(&ram) { return err("RAM should be between 512 MB and 256 GB."); }
    let port = if b["port"].is_null() || b["port"] == 0 || b["port"] == "" { 25565 } else { num(&b["port"]).ok_or("Port must be a number.")? };
    if !(1024..=65535).contains(&port) { return err("Port should be between 1024 and 65535."); }
    let pack = s(b, "modpack").trim().to_string();
    let (egg, mc, lv) = (s(b, "egg").to_string(), s(b, "mc").to_string(), s(b, "loader_version").to_string());
    if pack.is_empty() && !is_egg(&egg) { return err("Pick a server type."); }
    std::fs::create_dir_all(servers_dir()).map_err(|e| e.to_string())?;
    let motd = { let m = s(b, "motd"); if m.is_empty() { format!("{name} - an Orbit server") } else { m.to_string() } };
    let log = job(task, |log| {
        let tmp = std::env::temp_dir().join(format!("orbit-pack-{:08x}", rand::random::<u32>()));
        let r = (|| -> Res<()> {
            std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
            std::fs::create_dir(&root).map_err(|e| e.to_string())?;
            let (mut e, mut m, mut l) = (egg.clone(), mc.clone(), lv.clone());
            let mut info = None;
            if !pack.is_empty() {
                let i = mods::fetch_modpack(&pack, &tmp, None)?;
                e = i.loader.clone(); m = i.mc.clone(); l = i.loader_version.clone();
                if !is_egg(&e) || egg_info(&e).content != Some("mods") {
                    return err("That modpack needs a loader Orbit can't run as a server (only Fabric, Quilt, Forge and NeoForge).");
                }
                info = Some(i);
            }
            let mut cfg = json!({"created": true, "egg": e, "ram": ram, "port": port});
            for (k, v) in install_egg(&root, &e, &m, &l)? { cfg[k] = v; }
            if let Some(i) = &info {
                apply_pack(&root, i, log)?;
                cfg["modpack"] = json!({"title": i.title, "version": i.version, "project_id": i.project_id});
            }
            accept_eula(&root)?;
            if e != "velocity" {
                let mut props = Map::new();
                props.insert("server-port".into(), json!(port));
                props.insert("query.port".into(), json!(port));
                props.insert("motd".into(), json!(motd));
                write_properties(&root, &props)?;
            }
            write_cfg(&root, &cfg)
        })();
        if r.is_err() { let _ = std::fs::remove_dir_all(&root); }
        let _ = std::fs::remove_dir_all(&tmp);
        r
    })?;
    if !pack.is_empty() { let _ = link_content(&name); }   // best effort: real names + icons for the pack's files
    Ok(json!({"name": name, "log": log}))
}

pub fn server_icon_get(name: &str) -> Res<Vec<u8>> {
    let f = server_root(name)?.join(ICON_NAME);
    if !f.is_file() { return err("No icon."); }
    std::fs::read(f).map_err(|e| e.to_string())
}
pub fn server_icon_set(name: &str, data: &[u8]) -> Res {
    let f = server_root(name)?.join(ICON_NAME);
    if data.len() < 8 || data[..8] != *b"\x89PNG\r\n\x1a\n" { return err("The icon must be a PNG image."); }
    if data.len() > 500_000 { return err("That icon is too big."); }
    std::fs::write(f, data).map_err(|e| e.to_string())?;
    Ok(json!({"ok": true}))
}
pub fn server_icon_reset(name: &str) -> Res {
    let _ = std::fs::remove_file(server_root(name)?.join(ICON_NAME));
    Ok(json!({"ok": true}))
}

pub fn delete_server(name: &str) -> Res {
    let root = server_root(name)?;
    if status(name, &root)["running"] == true { return err("Stop the server before deleting it."); }
    std::fs::remove_dir_all(&root).map_err(|e| e.to_string())?;
    if let Some(m) = RUNS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() { m.remove(name); }
    Ok(json!({"ok": true}))
}

pub fn update_config(name: &str, b: &Value) -> Res {
    let root = server_root(name)?;
    let mut cfg = read_cfg(&root);
    if b.get("ram").is_some() {
        let ram = num(&b["ram"]).ok_or("RAM must be a number of MB.")?;
        if !(512..=262144).contains(&ram) { return err("RAM should be between 512 MB and 256 GB."); }
        set_ram(&root, &mut cfg, ram);
    }
    if b.get("java").is_some() {
        let j = s(b, "java").trim();
        if !j.is_empty() && !Path::new(j).is_file() { return err("That Java file doesn't exist."); }
        cfg["java"] = json!(j);
    }
    if b.get("jvm").is_some() { cfg["jvm"] = json!(s(b, "jvm").trim().chars().take(500).collect::<String>()); }
    if b.get("mc").is_some() { cfg["mc"] = json!(s(b, "mc").trim().chars().take(20).collect::<String>()); }
    if b.get("port").is_some() {
        let port = num(&b["port"]).ok_or("Port must be a number.")?;
        if !(1024..=65535).contains(&port) { return err("Port should be between 1024 and 65535."); }
        cfg["port"] = json!(port);
        if detect_egg(&root) != "velocity" {
            let mut m = Map::new();
            m.insert("server-port".into(), json!(port));
            write_properties(&root, &m)?;
        }
    }
    if let Some(a) = b.get("auto_restart").filter(|a| a.is_object()) {
        let mut cur = if cfg["auto_restart"].is_object() { cfg["auto_restart"].clone() } else { json!({}) };
        if let Some(e) = a["enabled"].as_bool() { cur["enabled"] = json!(e); }
        if let Some(list) = a.get("times").and_then(|t| t.as_array()) {
            let mut out: Vec<String> = vec![];
            for t in list {
                let t = t.as_str().unwrap_or("").trim();
                if parse_hhmm(t).is_none() { return err("Restart times should look like 04:30."); }
                if !out.iter().any(|x| x == t) { out.push(t.to_string()); }
            }
            if out.is_empty() { return err("Add at least one restart time."); }
            if out.len() > 24 { return err("That's too many restart times (24 at most)."); }
            out.sort();
            cur["times"] = json!(out);
            cur["time"] = json!(out[0]);
        } else if a.get("time").is_some() {
            let t = s(a, "time").trim();
            if parse_hhmm(t).is_none() { return err("The restart time should look like 04:30."); }
            cur["time"] = json!(t);
            cur["times"] = json!([t]);
        }
        if a.get("countdown").is_some() {
            let c = num(&a["countdown"]).ok_or("The countdown must be a number of seconds.")?;
            if !(0..=600).contains(&c) { return err("The countdown should be between 0 and 600 seconds."); }
            cur["countdown"] = json!(c);
        }
        cfg["auto_restart"] = cur;
    }
    if b["accept_eula"].as_bool().unwrap_or(false) { accept_eula(&root)?; }
    write_cfg(&root, &cfg)?;
    detail(name)
}

// ---------------------------------------------------------------- daily auto-restart

/// "HH:MM" (24 h) -> minutes after midnight.
fn parse_hhmm(t: &str) -> Option<i64> {
    let (h, m) = t.split_once(':')?;
    let (h, m) = (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?);
    if t.len() == 5 && (0..24).contains(&h) && (0..60).contains(&m) { Some(h * 60 + m) } else { None }
}

/// (enabled, minutes of the day (sorted, at least one), countdown seconds) from a server's config. Off by default;
/// 04:00 and 60 s when unset. Reads `times` (a list) and falls back to the older single `time`.
fn auto_restart_cfg(cfg: &Value) -> (bool, Vec<i64>, i64) {
    let a = &cfg["auto_restart"];
    let mut at: Vec<i64> = a["times"].as_array().into_iter().flatten().filter_map(|t| t.as_str().and_then(parse_hhmm)).collect();
    if at.is_empty() { at.push(a["time"].as_str().and_then(parse_hhmm).unwrap_or(4 * 60)); }
    at.sort(); at.dedup();
    (a["enabled"].as_bool().unwrap_or(false), at, a["countdown"].as_i64().unwrap_or(60).clamp(0, 600))
}

static AR_FIRED: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);

/// Local clock offset in seconds, or None when it isn't known yet. The window reports it (settings `tz_offset`,
/// minutes east of UTC) because the standard library has no time zone support. On Linux / macOS the system's own
/// `date +%z` is the fallback, so the schedule also works before the window has reported. Until the offset is
/// known the scheduler does nothing (guessing UTC would restart servers at the wrong hour).
fn tz_offset_secs() -> Option<i64> {
    if let Some(m) = store::settings().get("tz_offset").and_then(|v| v.as_i64()) { return Some(m * 60); }
    #[cfg(unix)]
    {
        if let Ok(o) = Command::new("date").arg("+%z").stdin(Stdio::null()).output() {
            return parse_zone(String::from_utf8_lossy(&o.stdout).trim());
        }
    }
    None
}

/// "+0530" / "-0800" -> seconds east of UTC. (Only called on unix; Windows gets the offset from the UI.)
#[cfg(any(unix, test))]
fn parse_zone(z: &str) -> Option<i64> {
    if z.len() != 5 || !z.is_char_boundary(1) { return None; }
    let sign = match &z[..1] { "+" => 1, "-" => -1, _ => return None };
    let (h, m) = (z[1..3].parse::<i64>().ok()?, z[3..5].parse::<i64>().ok()?);
    Some(sign * (h * 3600 + m * 60))
}

/// Waits until the server is really gone: Orbit's own process has exited and no java process still has its folder.
/// The process list is cached for a few seconds, so it is refreshed on every look.
fn wait_stopped(name: &str, root: &Path, secs: u64) -> bool {
    for _ in 0..secs {
        bust_pcache();
        if status(name, root)["running"] != true { return true; }
        std::thread::sleep(Duration::from_secs(1));
    }
    bust_pcache();
    status(name, root)["running"] != true
}

/// `start`, but tolerates the server's old process still being listed for a moment after it stopped.
fn start_when_free(name: &str) -> Res {
    let mut last = String::new();
    for _ in 0..15 {
        bust_pcache();
        match start(name) {
            Ok(v) => return Ok(v),
            Err(e) if e.contains("already running") => { last = e; std::thread::sleep(Duration::from_secs(1)); }
            Err(e) => return Err(e),
        }
    }
    err(last)
}

/// Starts the thread that restarts servers once a day. Call once at startup. A server is restarted only if it is
/// running under Orbit at the moment the countdown begins; a stopped server is never started by the schedule.
pub fn start_scheduler() {
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(10));
        let Some(off) = tz_offset_secs() else { continue };
        let local = store::now() as i64 + off;
        let sod = local.rem_euclid(86400);
        let Ok(rd) = std::fs::read_dir(servers_dir()) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let root = e.path();
            if name.starts_with('.') || !root.is_dir() { continue; }
            let (on, at, countdown) = auto_restart_cfg(&read_cfg(&root));
            if !on { continue; }
            // the countdown starts `countdown` seconds before each chosen time, so the restart itself happens on time
            let Some((late, at_min)) = at.iter().map(|t| {
                let trigger = (t * 60 - countdown).rem_euclid(86400);
                ((sod - trigger).rem_euclid(86400), *t)
            }).filter(|(late, _)| *late < 30).min() else { continue };
            let key = (local - late) * 2000 + at_min;   // one firing per day and per chosen time
            {
                let mut g = AR_FIRED.lock().unwrap_or_else(|e| e.into_inner());
                let m = g.get_or_insert_with(HashMap::new);
                if m.get(&name) == Some(&key) { continue; }
                m.insert(name.clone(), key);
            }
            // "ready" is read from the console; if that line was never recognised, a server that has been up for a
            // few minutes still counts, so the schedule can't be silently skipped
            let Some(run) = run_of(&name).filter(|r| r.alive.load(Relaxed) && !r.stopping.load(Relaxed)
                && (r.ready.load(Relaxed) || store::now() - r.started > 180.0)) else { continue };
            std::thread::spawn(move || scheduled_restart(&name, run, countdown));
        }
    });
}

/// Counts down in the server chat with `say` ("Server <what> in 5 minutes", "... in 30 seconds", ...) and then waits out the
/// last mark. Shared by the scheduled restart and by the shutdown when Orbit closes, so players get the same kind of
/// warning either way. Returns false if the server went away meanwhile (or, with `cancel_on_stop`, was stopped by hand,
/// which cancels a restart).
fn say_countdown(name: &str, run: &Run, countdown: i64, what: &str, cancel_on_stop: bool) -> bool {
    let proxy = server_root(name).map(|r| detect_egg(&r) == "velocity").unwrap_or(false);   // proxies have no `say`
    let say = |text: String| { if !proxy { let _ = send_command(name, &format!("say {text}")); } };
    let wait = |secs: i64| -> bool {
        for _ in 0..secs.max(0) {
            std::thread::sleep(Duration::from_secs(1));
            if !run.alive.load(Relaxed) || (cancel_on_stop && run.stopping.load(Relaxed)) { return false; }
        }
        true
    };
    let mut marks: Vec<i64> = [300, 120, 60, 30, 10, 5, 4, 3, 2, 1].into_iter().filter(|m| *m < countdown).collect();
    if countdown > 0 { marks.insert(0, countdown); }
    let mut left = countdown;
    for m in marks {
        if !wait(left - m) { return false; }
        left = m;
        say(if m >= 60 && m % 60 == 0 { format!("Server {what} in {} minute{}.", m / 60, if m == 60 { "" } else { "s" }) }
            else { format!("Server {what} in {m} second{}.", if m == 1 { "" } else { "s" }) });
    }
    wait(left)
}

/// Counts down in the server chat (`say`), then stops the server and starts it again. Stopping the server by hand
/// during the countdown cancels the restart.
fn scheduled_restart(name: &str, run: Arc<Run>, countdown: i64) {
    let proxy = server_root(name).map(|r| detect_egg(&r) == "velocity").unwrap_or(false);   // proxies have no `say`
    let say = |text: String| { if !proxy { let _ = send_command(name, &format!("say {text}")); } };
    run.add(format!("[Orbit] Scheduled restart in {countdown} s."));
    if !say_countdown(name, &run, countdown, "restarting", true) { return; }
    say("Restarting now. Back in a moment!".into());
    std::thread::sleep(Duration::from_secs(1));
    if stop(name, false).is_err() { return; }
    let Ok(root) = server_root(name) else { return };
    if !wait_stopped(name, &root, stop_timeout() + 15) {
        run.add("[Orbit] Scheduled restart: the server didn't stop in time, so it was left alone.");
        return;
    }
    match start_when_free(name) {
        Ok(_) => {}
        Err(e) => { if let Some(r) = run_of(name) { r.add(format!("[Orbit] Scheduled restart couldn't start the server: {e}")); } }
    }
}

// ---------------------------------------------------------------- closing Orbit

/// How long players are warned before a server is stopped because Orbit is closing.
const CLOSE_WARN_SECS: i64 = 10;

/// Servers Orbit itself is running (any state). Servers started outside Orbit are not ours to stop.
pub fn running_names() -> Vec<String> {
    let mut v: Vec<String> = RUNS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        .map(|m| m.iter().filter(|(_, r)| r.alive.load(Relaxed)).map(|(k, _)| k.clone()).collect()).unwrap_or_default();
    v.sort_by_key(|n| n.to_lowercase());
    v
}

/// Stops every server Orbit is running, all at once. Each one that is up and may have players on gets the in-game
/// shutdown warning first (the same `say` countdown as the scheduled restart); a server that won't stop in time is killed.
pub fn shutdown_all() {
    let hs: Vec<_> = running_names().into_iter().map(|n| std::thread::spawn(move || shutdown_one(&n))).collect();
    for h in hs { let _ = h.join(); }
}

fn shutdown_one(name: &str) {
    let Some(run) = run_of(name).filter(|r| r.alive.load(Relaxed)) else { return };
    let Ok(root) = server_root(name) else { return };
    if run.ready.load(Relaxed) && !run.stopping.load(Relaxed) {
        // nobody on it (a successful ping that says 0) needs no warning; if the ping fails, warn to be safe
        let nobody = live_status(name, &root)["players"].as_i64() == Some(0);
        if !nobody {
            run.add(format!("[Orbit] Orbit is closing: stopping this server in {CLOSE_WARN_SECS} s."));
            if !say_countdown(name, &run, CLOSE_WARN_SECS, "shutting down", false) { return; }
            let _ = send_command(name, "say Shutting down now. See you soon!");
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    if !run.stopping.load(Relaxed) { let _ = stop(name, false); }
    if !wait_stopped(name, &root, stop_timeout() + 15) {
        run.add("[Orbit] The server didn't stop in time, closing it.");
        let _ = stop(name, true);
        wait_stopped(name, &root, 10);
    }
}

// ---------------------------------------------------------------- mods / plugins / modpacks

fn content_dir(root: &Path) -> Res<(&'static str, PathBuf)> {
    let kind = content_kind(root, &detect_egg(root)).ok_or("This server type doesn't take mods or plugins.")?;
    Ok((kind, root.join(kind)))
}

fn is_jar(f: &str) -> bool { let l = f.to_lowercase(); l.ends_with(".jar") || l.ends_with(".jar.disabled") }
fn base_name(f: &str) -> &str { if f.to_lowercase().ends_with(".disabled") { &f[..f.len() - 9] } else { f } }
fn merge(mut a: Value, b: Value) -> Value {
    if let (Some(x), Some(y)) = (a.as_object_mut(), b.as_object()) { for (k, v) in y { x.insert(k.clone(), v.clone()); } }
    a
}

pub fn list_content(name: &str) -> Res {
    let root = server_root(name)?;
    let Some(kind) = content_kind(&root, &detect_egg(&root)) else { return Ok(json!({"kind": null, "items": []})) };
    let data = read_content(&root);
    let mut by_file: HashMap<String, (String, Value)> = HashMap::new();
    for (k, m) in data["items"].as_object().into_iter().flatten() { by_file.insert(s(m, "filename").to_string(), (k.clone(), m.clone())); }
    let d = root.join(kind);
    let mut files = list_names(&d);
    files.sort_by_key(|f| f.to_lowercase());
    let strip = rx(r"(?i)\.jar(\.disabled)?$");
    let mut items = vec![];
    for f in files {
        if !is_jar(&f) { continue; }
        let low = f.to_lowercase();
        let (key, m) = by_file.get(base_name(&f)).map(|(k, m)| (Some(k.clone()), m.clone())).unwrap_or((None, Value::Null));
        let size = std::fs::metadata(d.join(&f)).map(|m| m.len()).unwrap_or(0);
        items.push(json!({"file": f, "disabled": low.ends_with(".disabled"), "size": size, "key": key,
            "title": m["title"].as_str().filter(|t| !t.is_empty()).map(String::from).unwrap_or_else(|| strip.replace(&f, "").into_owned()),
            "version": m["version"].as_str().unwrap_or(""), "icon_url": m["icon_url"], "source": m["source"].as_str().unwrap_or("manual"),
            "slug": m["slug"].as_str().unwrap_or(""), "explicit": m["explicit"].as_bool().unwrap_or(true)}));
    }
    Ok(json!({"kind": kind, "items": items}))
}

fn safe_file(d: &Path, f: &str) -> Res<PathBuf> {
    if Path::new(f).file_name().map(|n| n.to_string_lossy() != f).unwrap_or(true) || !is_jar(f) { return err("Not a mod or plugin file."); }
    Ok(d.join(f))
}

pub fn toggle_content(name: &str, f: &str) -> Res {
    let root = server_root(name)?;
    let (_, d) = content_dir(&root)?;
    let p = safe_file(&d, f)?;
    if !p.is_file() { return err("That file is gone."); }
    std::fs::rename(&p, d.join(if f.to_lowercase().ends_with(".disabled") { f[..f.len() - 9].to_string() } else { format!("{f}.disabled") })).map_err(|e| e.to_string())?;
    list_content(name)
}

pub fn remove_content(name: &str, files: &[String]) -> Res {
    let root = server_root(name)?;
    let (_, d) = content_dir(&root)?;
    let mut data = read_content(&root);
    for f in files {
        let _ = std::fs::remove_file(safe_file(&d, f)?);
        let base = base_name(f).to_string();
        if let Some(items) = data["items"].as_object_mut() { items.retain(|_, m| s(m, "filename") != base); }
    }
    write_content(&root, &data)?;
    list_content(name)
}

pub fn upload_content(name: &str, filename: &str, data: &[u8]) -> Res {
    let root = server_root(name)?;
    let (_, d) = content_dir(&root)?;
    let f = Path::new(filename).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !f.to_lowercase().ends_with(".jar") { return err("Only .jar files can be uploaded."); }
    if data.is_empty() { return err("That file is empty."); }
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    std::fs::write(d.join(f), data).map_err(|e| e.to_string())?;
    list_content(name)
}

fn content_facets(kind: &str, loaders: &[&str], mc: Option<&str>) -> Vec<Vec<String>> {
    let mut f = vec![vec![format!("project_type:{kind}")]];
    if kind == "mod" { f.push(vec!["categories!=datapack".into()]); }
    if !loaders.is_empty() { f.push(loaders.iter().map(|l| format!("categories:{l}")).collect()); }
    if let Some(m) = mc.filter(|m| !m.is_empty()) { f.push(vec![format!("versions:{m}")]); }
    f
}

fn sort_index(q: &HashMap<String, String>, default: &str) -> String {
    match q.get("sort").map(|s| s.as_str()) { Some(s @ ("relevance" | "downloads" | "follows" | "newest" | "updated")) => s.into(), _ => default.into() }
}

fn search_params(q: &HashMap<String, String>, facets: &[Vec<String>], limit: usize, page: usize, index: String) -> Vec<(&'static str, String)> {
    let mut p = vec![("facets", json!(facets).to_string()), ("limit", limit.to_string()), ("index", index)];
    if let Some(t) = q.get("q").filter(|t| !t.is_empty()) { p.push(("query", t.clone())); }
    if page > 1 { p.push(("offset", ((page - 1) * limit).to_string())); }
    p
}

fn page_of(q: &HashMap<String, String>) -> usize { q.get("page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(1).max(1) }

fn server_mc(root: &Path, egg: &str, cfg: &Value) -> Option<String> {
    if egg_info(egg).proxy { return None; }
    Some(match s(cfg, "mc") { "" => guess_mc(root, cfg), m => m.to_string() }).filter(|m| !m.is_empty())
}

/// Modrinth search limited to what runs on this server (loader, Minecraft version, server side).
pub fn search_content(name: &str, q: &HashMap<String, String>) -> Res {
    let root = server_root(name)?;
    let egg = detect_egg(&root);
    let info = egg_info(&egg);
    let cfg = read_cfg(&root);
    let cont = content_kind(&root, &egg);
    let ptype = q.get("type").cloned().unwrap_or_else(|| if cont == Some("plugins") { "plugin".into() } else { "mod".into() });
    let loaders: Vec<&str> = match ptype.as_str() {
        "modpack" => { if cont != Some("mods") { return err("Modpacks need a Fabric, Quilt, Forge or NeoForge server."); } vec![info.key] }
        "mod" | "plugin" => { if cont.is_none() { return err("This server type doesn't take mods or plugins."); } info.loaders.to_vec() }
        _ => return err("bad type"),
    };
    let mc = server_mc(&root, &egg, &cfg);
    let mut facets = content_facets(&ptype, &loaders, mc.as_deref());
    if ptype != "plugin" { facets.push(vec!["server_side:required".into(), "server_side:optional".into()]); }
    let page = page_of(q);
    let res = crate::mods::api_get("/search", &search_params(q, &facets, 20, page, sort_index(q, "relevance")))?;
    let items = read_content(&root)["items"].clone();
    let have: HashSet<String> = items.as_object().into_iter().flatten().map(|(_, m)| s(m, "slug").to_string()).collect();
    let hits: Vec<Value> = res["hits"].as_array().into_iter().flatten().map(|h| json!({
        "key": h["project_id"], "ident": h["slug"], "slug": h["slug"], "title": h["title"], "author": h["author"].as_str().unwrap_or(""),
        "description": h["description"], "downloads": h["downloads"], "icon_url": h["icon_url"],
        "categories": h["display_categories"].as_array().map(|a| a.iter().take(4).cloned().collect::<Vec<_>>()).unwrap_or_default(),
        "installed": items.get(s(h, "project_id")).is_some() || have.contains(s(h, "slug")), "updated": h["date_modified"],
        "url": format!("https://modrinth.com/{ptype}/{}", s(h, "slug"))})).collect();
    Ok(json!({"hits": hits, "total": res["total_hits"].as_i64().unwrap_or(0), "mc": mc, "loaders": loaders, "page": page, "limit": 20}))
}

fn best(project: &str, mc: Option<&str>, loaders: &[&str]) -> Res<Option<Value>> {
    let mut p: Vec<(&str, String)> = vec![];
    if !loaders.is_empty() { p.push(("loaders", crate::mods::jl(loaders))); }
    if let Some(m) = mc { p.push(("game_versions", crate::mods::jl(&[m]))); }
    let vs = crate::mods::api_get(&format!("/project/{project}/version"), &p)?;
    let Some(a) = vs.as_array().filter(|a| !a.is_empty()) else { return Ok(None) };
    Ok(Some(a.iter().find(|v| v["version_type"] == "release").unwrap_or(&a[0]).clone()))
}

fn install_content_item(root: &Path, d: &Path, project: &str, loaders: &[&str], mc: Option<&str>, explicit: bool,
                        seen: &mut HashSet<String>, log: &mut Log) -> Res<()> {
    let meta = crate::mods::api_get(&format!("/project/{project}"), &[])?;
    if meta.is_null() { log.push(format!("  ! {project} wasn't found on Modrinth")); return Ok(()); }
    let (id, title) = (s(&meta, "id").to_string(), s(&meta, "title").to_string());
    if !seen.insert(id.clone()) { return Ok(()); }
    let Some(v) = best(&id, mc, loaders)? else {
        log.push(format!("  ! {title}: no version for {} / {}", mc.unwrap_or("any version"), loaders.join(", ")));
        if explicit {
            return err(format!("{title} has no version for Minecraft {} on {}.", mc.unwrap_or("?"), if loaders.is_empty() { "this server".into() } else { loaders.join(", ") }));
        }
        return Ok(());
    };
    let data = read_content(root);
    let old = data["items"].get(&id).cloned();
    if let Some(o) = &old {
        if o["version_id"] == v["id"] && d.join(s(o, "filename")).exists() { log.push(format!("  = {title} already up to date")); return Ok(()); }
    }
    for dep in v["dependencies"].as_array().into_iter().flatten() {
        if dep["dependency_type"] != "required" { continue; }
        let mut pid = dep["project_id"].as_str().map(String::from);
        if pid.is_none() {
            if let Some(vid) = dep["version_id"].as_str() {
                pid = crate::mods::api_get(&format!("/version/{vid}"), &[])?["project_id"].as_str().map(String::from);
            }
        }
        if let Some(p) = pid { install_content_item(root, d, &p, loaders, mc, false, seen, log)?; }
    }
    let files = v["files"].as_array().cloned().unwrap_or_default();
    let f = files.iter().find(|x| x["primary"].as_bool().unwrap_or(false)).or(files.first()).ok_or("That version has no files.")?;
    std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    let fname = download_file(s(f, "url"), d, s(f, "filename"), Some("sha512"), f["hashes"]["sha512"].as_str())?;
    let mut data = read_content(root);                      // dependencies may have written meanwhile
    if let Some(o) = &old {
        if s(o, "filename") != fname {
            let _ = std::fs::remove_file(d.join(s(o, "filename")));
            let _ = std::fs::remove_file(d.join(format!("{}.disabled", s(o, "filename"))));
        }
    }
    let ex = explicit || old.as_ref().and_then(|o| o["explicit"].as_bool()).unwrap_or(false);
    data["items"][&id] = json!({"title": title, "slug": meta["slug"], "icon_url": meta["icon_url"], "source": "modrinth",
        "version": v["version_number"], "version_id": v["id"], "filename": fname, "explicit": ex});
    write_content(root, &data)?;
    log.push(format!("  + {title} {}", s(&v, "version_number")));
    Ok(())
}

pub fn install_content(task: &Option<String>, name: &str, project: &str) -> Res {
    let root = server_root(name)?;
    let egg = detect_egg(&root);
    let (_, d) = content_dir(&root)?;
    let cfg = read_cfg(&root);
    let mc = server_mc(&root, &egg, &cfg);
    if mc.is_none() && !egg_info(&egg).proxy {
        return err("Orbit can't tell which Minecraft version this server runs. Set it in the server's Settings tab.");
    }
    let loaders = egg_info(&egg).loaders;
    let log = job(task, |log| install_content_item(&root, &d, project, loaders, mc.as_deref(), true, &mut HashSet::new(), log))?;
    Ok(merge(json!({"log": log}), list_content(name)?))
}

// ---------------------------------------------------------------- dependencies (see deps.rs)

fn deps_ctx(root: &Path) -> Option<(crate::deps::Ctx, PathBuf, Option<String>)> {
    let egg = detect_egg(root);
    let (_, d) = content_dir(root).ok()?;
    let mc = server_mc(root, &egg, &read_cfg(root));
    let ctx = crate::deps::Ctx { mc: mc.clone().unwrap_or_default(), platform: egg.clone(), server: true,
        loaders: egg_info(&egg).loaders.iter().map(|l| l.to_string()).collect() };
    Some((ctx, d, mc))
}

struct SrvInstaller<'a> { root: &'a Path, d: &'a Path, loaders: Vec<&'static str>, mc: Option<String> }
impl crate::deps::Installer for SrvInstaller<'_> {
    fn installed(&self, pid: &str) -> Option<String> { read_content(self.root)["items"].get(pid).map(|m| s(m, "title").to_string()) }
    fn install(&mut self, pid: &str, seen: &mut HashSet<String>, log: &mut Log) -> Res<()> {
        install_content_item(self.root, self.d, pid, &self.loaders, self.mc.as_deref(), false, seen, log)
    }
}

/// Install what the mods / plugins in this server's folder need and don't have. None when the server can't take
/// them, or when Orbit doesn't know the Minecraft version (a guess could fetch the wrong builds).
pub fn ensure_deps(root: &Path, log: &mut Log) -> Option<crate::deps::Outcome> {
    let (ctx, d, mc) = deps_ctx(root)?;
    if mc.is_none() && !egg_info(&ctx.platform).proxy { return None; }
    let loaders = egg_info(&ctx.platform).loaders.to_vec();
    Some(crate::deps::ensure(&d, &ctx, &mut SrvInstaller { root, d: &d, loaders, mc }, log))
}

/// Crash diagnosis for a server folder (also used by the command line).
pub fn diagnose_root(root: &Path, name: &str) -> Res {
    let egg = detect_egg(root);
    let cfg = read_cfg(root);
    let mut console = vec![];
    if let Some(run) = run_of(name) { let (l, _) = run.read(0); console = l[l.len().saturating_sub(800)..].to_vec(); }
    let (mods_dir, _) = content_dir(root).map(|(_, d)| (Some(d), ())).unwrap_or((None, ()));
    let props = read_properties(root);
    let inp = crate::diagnose::Input { kind: "server", name: name.to_string(), root: root.to_path_buf(), mc: server_mc(root, &egg, &cfg).unwrap_or_default(),
        platform: egg.clone(), server: true, ram_mb: cfg.get("ram").and_then(num).filter(|r| *r != 0).or_else(|| ram_from_files(root)).or(Some(default_ram())),
        port: props.get("server-port").and_then(num).or_else(|| cfg.get("port").and_then(num)), mods_dir,
        loaders: egg_info(&egg).loaders.iter().map(|l| l.to_string()).collect(), console };
    Ok(crate::diagnose::run(&inp))
}

pub fn deps_report(name: &str) -> Res {
    let root = server_root(name)?;
    let Some((ctx, d, _)) = deps_ctx(&root) else { return Ok(json!({"supported": false})) };
    let mut v = crate::deps::report_json(&crate::deps::analyze(&crate::deps::scan_dir(&d), &ctx));
    v["supported"] = json!(true);
    Ok(v)
}

pub fn install_deps(task: &Option<String>, name: &str) -> Res {
    let root = server_root(name)?;
    if content_dir(&root).is_err() { return err("This server type doesn't take mods or plugins."); }
    let mut outcome = None;
    let log = job(task, |log| {
        outcome = ensure_deps(&root, log);
        if outcome.is_none() { return err("Orbit can't tell which Minecraft version this server runs. Set it in the server's Settings tab."); }
        Ok(())
    })?;
    let o = outcome.unwrap();
    let mut lines = log;
    lines.extend(crate::deps::describe(&o));
    Ok(merge(json!({"log": lines, "deps": o.to_json()}), list_content(name)?))
}

// ---------------------------------------------------------------- Discover page, Modrinth matching, updates

/// Search for the Discover page when it is adding plugins or mods to a server (same shape as /api/search).
pub fn discover_search(q: &HashMap<String, String>) -> Res {
    let name = q.get("srv").cloned().unwrap_or_default();
    let root = server_root(&name)?;
    let egg = detect_egg(&root);
    let ptype = match content_kind(&root, &egg) {
        Some("plugins") => "plugin",
        Some("mods") => "mod",
        _ => return err("This server doesn't take mods or plugins."),
    };
    let mc = q.get("mc").filter(|m| !m.is_empty()).cloned();
    let cats: Vec<String> = q.get("cats").map(|c| c.split(',').filter(|c| rx(r"^[a-z0-9-]+$").is_match(c)).map(String::from).collect()).unwrap_or_default();
    let mut facets = content_facets(ptype, egg_info(&egg).loaders, mc.as_deref());
    if ptype == "mod" { facets.push(vec!["server_side:required".into(), "server_side:optional".into()]); }
    facets.extend(cats.iter().map(|c| vec![format!("categories:{c}")]));
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(20).clamp(1, 50);
    let res = crate::mods::api_get("/search", &search_params(q, &facets, limit, page_of(q), sort_index(q, "relevance")))?;
    let items = read_content(&root)["items"].clone();
    let have: HashSet<String> = items.as_object().into_iter().flatten().map(|(_, m)| s(m, "slug").to_string()).collect();
    let mut hits: Vec<Value> = res["hits"].as_array().into_iter().flatten().map(|h| {
        let tags: Vec<Value> = h["display_categories"].as_array().cloned().unwrap_or_default();
        json!({"source": "modrinth", "key": h["project_id"], "ident": h["slug"], "slug": h["slug"], "title": h["title"],
            "author": h["author"].as_str().unwrap_or("unknown"), "description": h["description"], "downloads": h["downloads"],
            "follows": h["follows"].as_i64().unwrap_or(0), "updated": h["date_modified"], "icon_url": h["icon_url"],
            "categories": tags.iter().take(5).collect::<Vec<_>>(), "more": tags.len().saturating_sub(5), "downloadable": true,
            "url": format!("https://modrinth.com/{}/{}", ptype, s(h, "slug")),
            "installed": items.get(s(h, "project_id")).is_some() || have.contains(s(h, "slug"))})
    }).collect();
    if q.get("hide").map(|h| h == "1").unwrap_or(false) { hits.retain(|h| h["installed"] != true); }
    Ok(json!({"hits": hits, "total": res["total_hits"].as_i64().unwrap_or(0)}))
}

/// Look every file Orbit doesn't know (uploaded or put in by hand) up on Modrinth by SHA-1 and turn the
/// ones found into regular Modrinth installs, so they get icons and update checks.
pub fn link_content(name: &str) -> Res {
    let root = server_root(name)?;
    let (_, d) = content_dir(&root)?;
    let mut data = read_content(&root);
    // Files that came with a modpack count as unmatched too: Modrinth knows most of them, so they get real names + icons.
    let known: HashSet<String> = data["items"].as_object().into_iter().flatten().filter(|(_, m)| m["source"] != "modpack").map(|(_, m)| s(m, "filename").to_string()).collect();
    let mut todo: HashMap<String, Vec<String>> = HashMap::new();
    for f in list_names(&d) {
        if !is_jar(&f) { continue; }
        let base = base_name(&f).to_string();
        if known.contains(&base) { continue; }
        if let Ok(h) = util::sha1_file(&d.join(&f)) { todo.entry(h).or_default().push(base); }
    }
    if todo.is_empty() { return Ok(merge(json!({"linked": 0, "checked": 0, "error": ""}), list_content(name)?)); }
    let hashes: Vec<String> = todo.keys().cloned().collect();
    let lookup = || -> Res<(Map<String, Value>, HashMap<String, Value>)> {
        let mut found = Map::new();
        for ch in hashes.chunks(200) {
            if let Some(o) = crate::mods::api_post("/version_files", &json!({"hashes": ch, "algorithm": "sha1"}))?.as_object() { found.extend(o.clone()); }
        }
        let mut pids: Vec<String> = found.values().filter_map(|v| v["project_id"].as_str().map(String::from)).collect();
        pids.sort();
        pids.dedup();
        let mut projects = HashMap::new();
        for ch in pids.chunks(100) {
            for p in crate::mods::api_get("/projects", &[("ids", crate::mods::jl(ch))])?.as_array().into_iter().flatten() {
                if let Some(id) = p["id"].as_str() { projects.insert(id.to_string(), p.clone()); }
            }
        }
        Ok((found, projects))
    };
    let (found, projects) = match lookup() {
        Ok(x) => x,
        Err(e) => return Ok(merge(json!({"linked": 0, "checked": todo.len(), "error": e}), list_content(name)?)),
    };
    let mut linked = 0;
    for (h, v) in found {
        let Some(proj) = v["project_id"].as_str().and_then(|p| projects.get(p)) else { continue };
        let pid = s(proj, "id").to_string();
        if data["items"].get(&pid).is_some() { continue; }
        let base = todo[&h][0].clone();
        if let Some(items) = data["items"].as_object_mut() { items.retain(|_, m| !(m["source"] == "modpack" && s(m, "filename") == base)); }
        data["items"][&pid] = json!({"title": proj["title"], "slug": proj["slug"], "icon_url": proj["icon_url"], "source": "modrinth",
            "version": v["version_number"], "version_id": v["id"], "filename": base, "explicit": true});
        linked += 1;
    }
    write_content(&root, &data)?;
    Ok(merge(json!({"linked": linked, "checked": todo.len(), "error": ""}), list_content(name)?))
}

/// Installed Modrinth items that have a newer version for this server.
pub fn content_updates(name: &str) -> Res {
    let root = server_root(name)?;
    let (_, d) = content_dir(&root)?;
    let egg = detect_egg(&root);
    let mc = server_mc(&root, &egg, &read_cfg(&root));
    if mc.is_none() && !egg_info(&egg).proxy { return Ok(json!({"updates": []})); }
    let loaders = egg_info(&egg).loaders;
    let items: Vec<(String, Value)> = read_content(&root)["items"].as_object().into_iter().flatten()
        .filter(|(_, m)| m["source"].as_str().unwrap_or("modrinth") == "modrinth"
            && (d.join(s(m, "filename")).exists() || d.join(format!("{}.disabled", s(m, "filename"))).exists()))
        .map(|(k, m)| (k.clone(), m.clone())).collect();
    let res: Vec<Option<Value>> = mods::par_map(&items, 8, |(k, m)| {
        let v = best(k, mc.as_deref(), loaders).ok().flatten()?;
        if v["id"] == m["version_id"] { return None; }
        Some(json!({"key": k, "new_id": v["id"], "new": v["version_number"], "title": m["title"], "icon_url": m["icon_url"],
                    "current": m["version"].as_str().unwrap_or(""), "url": format!("https://modrinth.com/project/{k}/version/{}", s(&v, "id"))}))
    });
    Ok(json!({"updates": res.into_iter().flatten().collect::<Vec<_>>()}))
}

pub fn update_content(task: &Option<String>, name: &str, keys: &[String]) -> Res {
    let root = server_root(name)?;
    let (_, d) = content_dir(&root)?;
    let egg = detect_egg(&root);
    let mc = server_mc(&root, &egg, &read_cfg(&root));
    if mc.is_none() && !egg_info(&egg).proxy {
        return err("Orbit can't tell which Minecraft version this server runs. Set it in the server's Settings tab.");
    }
    let loaders = egg_info(&egg).loaders;
    let data = read_content(&root);
    let keys: Vec<String> = keys.iter().filter(|k| data["items"].get(k.as_str()).is_some()).cloned().collect();
    if keys.is_empty() { return err("Nothing to update."); }
    let off: HashMap<String, bool> = keys.iter().map(|k| (k.clone(), d.join(format!("{}.disabled", s(&data["items"][k], "filename"))).exists())).collect();
    let log = job(task, |log| {
        for k in &keys {
            install_content_item(&root, &d, k, loaders, mc.as_deref(), false, &mut HashSet::new(), log)?;
            if off[k] {                                      // keep it disabled if it was
                let m = read_content(&root)["items"].get(k.as_str()).cloned();
                if let Some(m) = m.filter(|m| d.join(s(m, "filename")).exists()) {
                    let _ = std::fs::rename(d.join(s(&m, "filename")), d.join(format!("{}.disabled", s(&m, "filename"))));
                }
            }
        }
        Ok(())
    })?;
    Ok(merge(json!({"log": log}), list_content(name)?))
}

/// Download a Modrinth pack's server files and unpack its overrides into root (client-only files skipped).
fn apply_pack(root: &Path, info: &mods::PackInfo, log: &mut Log) -> Res<()> {
    let files: Vec<Value> = info.index["files"].as_array().into_iter().flatten().filter(|f| f["env"]["server"] != "unsupported").cloned().collect();
    util::track_expected(files.len());
    let mut data = read_content(root);
    for (n, fe) in files.iter().enumerate() {
        let rel = s(fe, "path");
        let target = util::safe_join(root, rel)?;
        let urls: Vec<&str> = fe["downloads"].as_array().into_iter().flatten().filter_map(|u| u.as_str())
            .filter(|u| crate::mods::MRPACK_HOSTS.contains(&util::host_of(u).as_str())).collect();
        if urls.is_empty() { return err(format!("No allowed download source for {rel}.")); }
        let h = &fe["hashes"];
        let algo = if h.get("sha512").is_some() { Some("sha512") } else if h.get("sha1").is_some() { Some("sha1") } else { None };
        let dir = target.parent().ok_or("bad path")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let tname = target.file_name().unwrap().to_string_lossy().into_owned();
        for (k, u) in urls.iter().enumerate() {
            match download_file(u, dir, &tname, algo, algo.and_then(|a| h[a].as_str())) {
                Ok(_) => break,
                Err(e) if k == urls.len() - 1 => return Err(e),
                Err(_) => {}
            }
        }
        let top = Path::new(rel).components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default();
        if (top == "mods" || top == "plugins") && tname.to_lowercase().ends_with(".jar") {
            let stem = Path::new(&tname).file_stem().unwrap_or_default().to_string_lossy().into_owned();
            data["items"][format!("pack:{rel}")] = json!({"title": stem, "slug": stem, "icon_url": null, "source": "modpack",
                "version": info.version, "version_id": "", "filename": tname, "explicit": true});
        }
        log.push(format!("  + [{}/{}] {tname}", n + 1, files.len()));
    }
    let mut z = zip::ZipArchive::new(std::fs::File::open(&info.pack).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    for i in 0..z.len() {
        let mut m = z.by_index(i).map_err(|e| e.to_string())?;
        let name = m.name().to_string();
        let (head, rel) = name.split_once('/').unwrap_or((&name, ""));
        if !["overrides", "server-overrides"].contains(&head) || rel.is_empty() || m.is_dir() { continue; }
        let t = util::safe_join(root, rel)?;
        std::fs::create_dir_all(t.parent().unwrap()).map_err(|e| e.to_string())?;
        let mut out = std::fs::File::create(&t).map_err(|e| e.to_string())?;
        std::io::copy(&mut m, &mut out).map_err(|e| e.to_string())?;
    }
    write_content(root, &data)
}

/// Put a Modrinth modpack onto a server that already runs the same loader and Minecraft version.
pub fn install_modpack(task: &Option<String>, name: &str, ident: &str) -> Res {
    let root = server_root(name)?;
    let egg = detect_egg(&root);
    if content_kind(&root, &egg) != Some("mods") || egg == "custom" { return err("Modpacks need a Fabric, Quilt, Forge or NeoForge server."); }
    let cfg = read_cfg(&root);
    if status(name, &root)["running"] == true { return err("Stop the server before installing a modpack."); }
    let log = job(task, |log| {
        let tmp = std::env::temp_dir().join(format!("orbit-pack-{:08x}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let r = (|| -> Res<()> {
            let mc = server_mc(&root, &egg, &cfg);
            let info = mods::fetch_modpack(ident, &tmp, mc.as_deref())?;
            if info.loader != egg {
                let l = info.loader.clone();
                return err(format!("{} is a {}{} pack, but this is a {} server. Create a new server from the pack instead.", info.title,
                    l[..1].to_uppercase(), &l[1..], egg_info(&egg).name));
            }
            if let Some(m) = mc.filter(|m| *m != info.mc) {
                return err(format!("{} is for Minecraft {}, this server runs {m}. Create a new server from the pack instead.", info.title, info.mc));
            }
            apply_pack(&root, &info, log)?;
            let mut c = read_cfg(&root);
            c["modpack"] = json!({"title": info.title, "version": info.version, "project_id": info.project_id});
            write_cfg(&root, &c)
        })();
        let _ = std::fs::remove_dir_all(&tmp);
        r
    })?;
    let _ = link_content(name);                       // best effort: real names + icons for the pack's files
    Ok(merge(json!({"log": log}), list_content(name)?))
}

/// Modrinth modpacks for the new-server dialog (only loaders that can run as a server).
pub fn search_packs(q: &HashMap<String, String>) -> Res {
    let page = page_of(q);
    let mut facets: Vec<Vec<String>> = vec![vec!["project_type:modpack".into()],
        ["fabric", "quilt", "forge", "neoforge"].iter().map(|l| format!("categories:{l}")).collect(),
        vec!["server_side:required".into(), "server_side:optional".into()]];
    if let Some(m) = q.get("mc").filter(|m| !m.is_empty()) { facets.push(vec![format!("versions:{m}")]); }
    let idx = if q.get("q").map(|t| !t.is_empty()).unwrap_or(false) { "relevance" } else { "downloads" };
    let res = crate::mods::api_get("/search", &search_params(q, &facets, 15, page, idx.into()))?;
    let hits: Vec<Value> = res["hits"].as_array().into_iter().flatten().map(|h| json!({
        "key": h["project_id"], "ident": h["slug"], "title": h["title"], "author": h["author"].as_str().unwrap_or(""),
        "description": h["description"], "downloads": h["downloads"], "icon_url": h["icon_url"],
        "categories": h["categories"].as_array().into_iter().flatten().filter(|c| ["fabric", "quilt", "forge", "neoforge"].contains(&c.as_str().unwrap_or(""))).collect::<Vec<_>>(),
        "mc": h["versions"].as_array().and_then(|v| v.last()).and_then(|v| v.as_str()).unwrap_or("")})).collect();
    Ok(json!({"hits": hits, "total": res["total_hits"].as_i64().unwrap_or(0)}))
}

// ---------------------------------------------------------------- files

const TEXT_EXT: [&str; 22] = [".properties", ".txt", ".yml", ".yaml", ".json", ".json5", ".toml", ".cfg", ".conf", ".config", ".log",
    ".ini", ".xml", ".md", ".sh", ".bat", ".mcmeta", ".csv", ".lang", ".snbt", ".js", ".html"];

/// Resolve `rel` inside the server folder, refusing anything that climbs out of it (including via symlinks).
fn fpath(root: &Path, rel: &str) -> Res<PathBuf> { util::resolve_in(root, rel, "server") }

pub fn list_files(name: &str, rel: &str) -> Res {
    let root = server_root(name)?;
    let d = fpath(&root, rel)?;
    if !d.is_dir() { return err("That folder doesn't exist."); }
    let mut ents: Vec<PathBuf> = std::fs::read_dir(&d).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).collect();
    ents.sort_by_key(|p| (!p.is_dir(), p.file_name().unwrap().to_string_lossy().to_lowercase()));
    let mut out = vec![];
    for f in ents {
        let Ok(md) = std::fs::metadata(&f) else { continue };
        let n = f.file_name().unwrap().to_string_lossy().into_owned();
        let ext = f.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default();
        out.push(json!({"name": n, "dir": md.is_dir(), "size": if md.is_dir() { 0 } else { md.len() }, "text": TEXT_EXT.contains(&ext.as_str()) || ext == ".secret" || n == "eula.txt"}));
    }
    let rp = d.strip_prefix(root.canonicalize().map_err(|e| e.to_string())?).unwrap_or(Path::new("")).to_string_lossy().replace('\\', "/");
    Ok(json!({"path": rp, "items": out}))
}

pub fn read_file(name: &str, rel: &str) -> Res {
    let f = fpath(&server_root(name)?, rel)?;
    if !f.is_file() { return err("That file doesn't exist."); }
    if std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0) > 1_000_000 { return err("That file is too big to edit here (1 MB limit)."); }
    let bytes = std::fs::read(&f).map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map(|t| json!({"text": t})).map_err(|_| "That isn't a text file.".to_string())
}

pub fn write_file(name: &str, rel: &str, text: &str) -> Res {
    let f = fpath(&server_root(name)?, rel)?;
    if !f.is_file() { return err("That file doesn't exist."); }
    std::fs::write(&f, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(json!({"ok": true}))
}

pub fn delete_file(name: &str, rel: &str) -> Res {
    let root = server_root(name)?;
    let f = fpath(&root, rel)?;
    if f == root.canonicalize().map_err(|e| e.to_string())? || !f.exists() { return err("Nothing to delete there."); }
    let res = if f.is_dir() { std::fs::remove_dir_all(&f) } else { std::fs::remove_file(&f) };
    res.map_err(|e| e.to_string())?;
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------- settings + routing

fn settings_view() -> Value {
    json!({"dir": servers_dir().to_string_lossy(), "default_ram": default_ram(), "stop_timeout": stop_timeout(),
           "default_dir": store::data_home().join("servers").to_string_lossy()})
}

pub fn update_settings(b: &Value) -> Res {
    let mut st = store::settings();
    if b.get("servers_dir").is_some() {
        let raw = s(b, "servers_dir").trim();
        if raw.is_empty() { st.remove("servers_dir"); } else {
            let p = match raw.strip_prefix('~') { Some(r) => store::home_dir().join(r.trim_start_matches(['/', '\\'])), None => PathBuf::from(raw) };
            std::fs::create_dir_all(&p).map_err(|e| format!("Can't use that folder: {e}"))?;
            st.insert("servers_dir".into(), json!(p.canonicalize().unwrap_or(p).to_string_lossy()));
        }
    }
    if b.get("server_memory").is_some() {
        let mem = num(&b["server_memory"]).ok_or("Default RAM must be a number of MB.")?;
        if !(512..=262144).contains(&mem) { return err("Default RAM should be between 512 MB and 256 GB."); }
        st.insert("server_memory".into(), json!(mem));
    }
    if b.get("server_stop_timeout").is_some() {
        let t = num(&b["server_stop_timeout"]).ok_or("Stop timeout must be a number of seconds.")?;
        st.insert("server_stop_timeout".into(), json!(t.clamp(10, 300)));
    }
    store::save_settings(&st)?;
    Ok(json!({"dir": servers_dir().to_string_lossy(), "default_ram": default_ram(), "stop_timeout": stop_timeout()}))
}

/// `p` is the split path, e.g. ["api", "servers", "My Server", "start"].
pub fn route(r: &Req, p: &[String]) -> Result<Out, String> {
    let (m, b, q, n) = (r.method, &r.body, &r.q, p.len());
    let j = |v: Res| v.map(Out::Json);
    match p[1].as_str() {
        "server-eggs" => return j(Ok(json!({"eggs": EGGS.iter().map(|e| json!({"id": e.key, "name": e.name, "desc": e.desc, "content": e.content, "proxy": e.proxy})).collect::<Vec<_>>()}))),
        "server-versions" => {
            let egg = q.get("egg").cloned().unwrap_or_default();
            let mut v = egg_versions(&egg)?;
            if let Some(mc) = q.get("mc").filter(|m| !m.is_empty()) { v["loaders"] = loader_versions(&egg, mc)?; }
            return j(Ok(v));
        }
        "server-settings" => return j(if m == "POST" { update_settings(b) } else { Ok(settings_view()) }),
        "server-packs" => return j(search_packs(q)),
        _ => {}
    }
    if n == 2 { return j(if m == "POST" { create_server(&r.task, b) } else { list_servers() }); }
    let name = p[2].as_str();
    if n == 3 { return j(if m == "DELETE" { delete_server(name) } else { detail(name) }); }
    let act = p[3].as_str();
    let root = server_root(name)?;
    let qs = |k: &str| q.get(k).cloned().unwrap_or_default();
    match (act, m) {
        ("start", "POST") => return j(start(name)),
        ("stop", "POST") => return j(stop(name, b["force"].as_bool().unwrap_or(false))),
        ("restart", "POST") => {
            if status(name, &root)["running"] == true {
                stop(name, false)?;
                wait_stopped(name, &root, stop_timeout() + 5);
            }
            return j(start_when_free(name));
        }
        ("command", "POST") => return j(send_command(name, s(b, "command"))),
        ("console", _) => return j(console(name, qs("since").parse().unwrap_or(0))),
        ("icon", "POST") => return j(if !qs("reset").is_empty() { server_icon_reset(name) } else { server_icon_set(name, &r.raw) }),
        ("icon", _) => return server_icon_get(name).map(|b| Out::Bytes(b, "image/png")),
        ("stats", _) => return j(Ok(live_status(name, &root))),
        ("diagnose", _) => return j(diagnose_root(&root, name)),
        ("config", "POST") => return j(update_config(name, b)),
        ("properties", _) => {
            if m == "POST" { write_properties(&root, b["props"].as_object().unwrap_or(&Map::new()))?; }
            return j(Ok(json!({"props": read_properties(&root)})));
        }
        ("content", _) => {
            if n == 4 { return j(list_content(name)); }
            match (p[4].as_str(), m) {
                ("search", _) => return j(search_content(name, q)),
                ("install", "POST") => return j(install_content(&r.task, name, &match &b["project"] { Value::String(x) => x.clone(), o => o.to_string() })),
                ("remove", "POST") => return j(remove_content(name, &b["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect::<Vec<_>>())),
                ("toggle", "POST") => return j(toggle_content(name, s(b, "file"))),
                ("upload", "POST") => return j(upload_content(name, &qs("filename"), &r.raw)),
                ("link", "POST") => return j(link_content(name)),
                ("deps", "POST") => return j(install_deps(&r.task, name)),
                ("deps", _) => return j(deps_report(name)),
                ("updates", _) => return j(content_updates(name)),
                ("update", "POST") => return j(update_content(&r.task, name, &b["keys"].as_array().into_iter().flatten().filter_map(|k| k.as_str().map(String::from)).collect::<Vec<_>>())),
                ("modpack", "POST") => return j(install_modpack(&r.task, name, &match &b["ident"] { Value::String(x) => x.clone(), o => o.to_string() })),
                _ => {}
            }
        }
        ("content-icon", _) => {
            let (_, d) = content_dir(&root)?;
            let path = safe_file(&d, &qs("f"))?;
            return icons::icon_for(&path).map(|(b, ct)| Out::Bytes(b, ct)).ok_or_else(|| "No icon.".to_string());
        }
        ("files", _) => return j(list_files(name, &qs("path"))),
        ("file", "POST") => return j(write_file(name, s(b, "path"), s(b, "text"))),
        ("file", "DELETE") => return j(delete_file(name, &qs("path"))),
        ("file", _) => return j(read_file(name, &qs("path"))),
        ("open", "POST") => { crate::api::open_folder(&root); return j(Ok(json!({"ok": true}))); }
        _ => {}
    }
    err("not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_time_parsing() {
        assert_eq!(parse_hhmm("04:30"), Some(270));
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        for bad in ["24:00", "4:30", "12:60", "ab:cd", "", "1230"] { assert_eq!(parse_hhmm(bad), None, "{bad}"); }
    }

    #[test]
    fn zone_parsing() {
        assert_eq!(parse_zone("+0200"), Some(7200));
        assert_eq!(parse_zone("-0830"), Some(-30600));
        for bad in ["", "0200", "+02", "+ab00"] { assert_eq!(parse_zone(bad), None, "{bad}"); }
    }

    #[test]
    fn auto_restart_defaults_and_clamping() {
        assert_eq!(auto_restart_cfg(&json!({})), (false, vec![240], 60));
        assert_eq!(auto_restart_cfg(&json!({"auto_restart": {"enabled": true, "time": "03:15", "countdown": 9999}})), (true, vec![195], 600));
        assert_eq!(auto_restart_cfg(&json!({"auto_restart": {"enabled": true, "times": ["16:00", "04:00", "04:00"]}})), (true, vec![240, 960], 60));
    }
}
