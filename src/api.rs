//! The JSON API the UI talks to (port of orbit.py's router). Same URLs as the Python version, so
//! ui/index.html works as it is. Errors are plain strings -> HTTP 400 (NOT_PORTED: prefix -> 501).
use crate::mods::{self, Log};
use crate::util::{SharedTrack, Track};
use crate::{auth, dialogs, gamedata, icons, importers, launcher, meta, optimization, servers, store, syncing};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub type Res = Result<Value, String>;
pub struct Req<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub q: HashMap<String, String>,
    pub body: Value,
    pub raw: Vec<u8>,
    pub task: Option<String>,
}

pub enum Out {
    Json(Value),
    Bytes(Vec<u8>, &'static str),
}

const THEMES: [&str; 12] = ["dark", "light", "oled", "blackout", "system", "christmas-light", "christmas-dark", "halloween-light", "halloween-dark", "glitch", "synthwave", "aurora"];
const SHOT_TYPES: [(&str, &str); 4] = [("png", "image/png"), ("jpg", "image/jpeg"), ("jpeg", "image/jpeg"), ("webp", "image/webp")];
const SHOT_SKIP: [&str; 14] = ["saves", "mods", "libraries", "versions", "assets", "logs", "config", "resourcepacks", "shaderpacks", "datapacks", "natives", "runtime", "crash-reports", "bin"];

fn s<'a>(b: &'a Value, k: &str) -> &'a str { b[k].as_str().unwrap_or("") }
fn re(p: &str) -> Regex { Regex::new(p).unwrap() }
fn ms(v: &Value) -> Option<i64> { v.as_i64().or_else(|| v.as_str().and_then(|x| x.trim().parse().ok())) }

fn valid_name(n: &str) -> Result<String, String> {
    let n = n.trim();
    if !n.is_empty() && !re(r"^[A-Za-z0-9_]{3,16}$").is_match(n) { return Err("Player names are 3 to 16 letters, numbers or underscores.".into()); }
    Ok(n.to_string())
}

fn accounts(st: &Map<String, Value>) -> Vec<String> {
    let mut v: Vec<String> = st.get("accounts").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    if let Some(u) = st.get("username").and_then(|u| u.as_str()).filter(|u| !u.is_empty()) { if !v.iter().any(|x| x == u) { v.insert(0, u.to_string()); } }
    v
}

fn expand(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix('~') { return store::home_dir().join(rest.trim_start_matches(['/', '\\'])); }
    PathBuf::from(raw)
}

fn dir_setting(st: &mut Map<String, Value>, b: &Value, key: &str, label: &str) -> Result<(), String> {
    if let Some(raw) = b.get(key).and_then(|v| v.as_str()) {
        let raw = raw.trim();
        if raw.is_empty() { st.remove(key); } else {
            let p = expand(raw).canonicalize().unwrap_or_else(|_| expand(raw));
            if !p.is_dir() { return Err(format!("That {label} folder doesn't exist.")); }
            st.insert(key.into(), json!(p.to_string_lossy()));
        }
    }
    Ok(())
}

pub fn use_key() {
    mods::clear_cf_key();
    if let Some(k) = store::settings().get("cf_key").and_then(|v| v.as_str()) { mods::set_cf_key(k); }
}

// ---------- caches ----------

static VCACHE: Mutex<Vec<((bool, bool), Instant, Value)>> = Mutex::new(Vec::new());
static LVCACHE: Mutex<Vec<((String, String), Instant, Value)>> = Mutex::new(Vec::new());
static TCACHE: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());
static JOB_LOCK: Mutex<()> = Mutex::new(());
static PROGRESS: Mutex<Vec<(String, SharedTrack)>> = Mutex::new(Vec::new());
/// Tasks cancelled before they got to run (they were queued behind another job).
static CANCELLED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Cancel a running or queued install: the running job's downloads stop at their next chunk.
fn cancel_task(id: &str) {
    let t = PROGRESS.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().find(|(k, _)| k == id).map(|(_, t)| t.clone());
    match t { Some(t) => t.lock().unwrap().cancel = true, None => CANCELLED.lock().unwrap_or_else(|e| e.into_inner()).push(id.to_string()) }
}

/// Run `f` as the one active job, capturing readable progress lines. Errors become the last log line
/// (and are also returned) so the UI shows them like the Python version did.
pub fn run_job<F: FnOnce(&mut Log) -> Result<(), String>>(task: &Option<String>, f: F) -> (Log, Option<String>) {
    let _g = JOB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(t) = task {
        let mut c = CANCELLED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = c.iter().position(|x| x == t) { c.remove(i); return (vec!["Cancelled.".into()], Some("Cancelled.".into())); }
    }
    use_key();
    let tr: SharedTrack = Arc::new(Mutex::new(Track { expected: 1, ..Default::default() }));
    crate::util::set_track(Some(tr.clone()));
    if let Some(t) = task {
        let mut p = PROGRESS.lock().unwrap_or_else(|e| e.into_inner());
        p.push((t.clone(), tr.clone()));
        while p.len() > 40 { p.remove(0); }
    }
    let mut log = vec![];
    let r = f(&mut log);
    crate::util::set_track(None);
    let err = r.err();
    if let Some(e) = &err { log.push(e.clone()); }
    (log.into_iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect(), err)
}

// ---------- settings ----------

fn inside(x: &Path, y: &Path) -> bool { x == y || x.starts_with(y) }

fn set_instances_dir(raw: &str, mv: bool) -> Result<usize, String> {
    let raw = raw.trim();
    let new = if raw.is_empty() { store::default_instances() } else { expand(raw) };
    let _ = std::fs::create_dir_all(&new);
    let new = new.canonicalize().unwrap_or(new);
    let old_raw = store::inst_dir();
    let old = old_raw.canonicalize().unwrap_or(old_raw);
    if new == old { return Ok(0); }
    if inside(&new, &old) || inside(&old, &new) { return Err("The new folder can't be inside the current one (or the other way round).".into()); }
    let probe = new.join(format!(".orbit-write-test-{:06x}", rand::random::<u32>() & 0xffffff));
    std::fs::write(&probe, "x").map_err(|e| format!("Can't write to that folder: {e}"))?;
    let _ = std::fs::remove_file(&probe);
    let moving = if mv { store::instance_folders(&old) } else { vec![] };
    let clash: Vec<String> = moving.iter().filter(|p| new.join(p.file_name().unwrap()).exists()).map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    if !clash.is_empty() { return Err(format!("These instances already exist in the new folder: {}", clash.join(", "))); }
    let _g = JOB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for p in &moving {
        let dest = new.join(p.file_name().unwrap());
        if std::fs::rename(p, &dest).is_err() { copy_dir(p, &dest)?; std::fs::remove_dir_all(p).map_err(|e| e.to_string())?; }
    }
    let mut st = store::settings();
    if raw.is_empty() { st.remove("instances_dir"); } else { st.insert("instances_dir".into(), json!(new.to_string_lossy())); }
    store::save_settings(&st)?;
    Ok(moving.len())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for e in std::fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
        let (p, d) = (e.path(), dst.join(e.file_name()));
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) { copy_dir(&p, &d)?; } else { std::fs::copy(&p, &d).map_err(|e| e.to_string())?; }
    }
    Ok(())
}

fn settings(r: &Req) -> Res {
    let mut moved = 0;
    if r.method == "POST" {
        let b = &r.body;
        if let Some(d) = b.get("instances_dir").and_then(|v| v.as_str()) { moved = set_instances_dir(d, b["move"] == true)?; }
        let mut st = store::settings();
        if let Some(k) = b.get("cf_key").and_then(|v| v.as_str()) { st.insert("cf_key".into(), json!(k.trim())); VCACHE.lock().unwrap().clear(); }
        if let Some(t) = b["theme"].as_str().filter(|t| THEMES.contains(t)) { st.insert("theme".into(), json!(t)); }
        if let Some(c) = b["accent"].as_str() {
            // "" = the active theme's own accent; otherwise a #rrggbb colour
            if c.is_empty() || (c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|h| h.is_ascii_hexdigit())) { st.insert("accent".into(), json!(c)); }
        }
        if let Some(h) = b["high_contrast"].as_bool() { st.insert("high_contrast".into(), json!(h)); }
        if let Some(w) = b["close_warning"].as_bool() { st.insert("close_warning".into(), json!(w)); }
        if let Some(n) = b["close_timer"].as_i64() { st.insert("close_timer".into(), json!(n.clamp(0, 120))); }
        if let Some(a) = b["animations"].as_bool() { st.insert("animations".into(), json!(a)); } // theme animations on/off
        if let Some(n) = b["snow_height"].as_i64() { st.insert("snow_height".into(), json!(n.clamp(4, 100))); } // Christmas: max snow pile height (px)
        if let Some(o) = b["ui_flags"].as_object().filter(|o| serde_json::to_string(o).map(|t| t.len() <= 20000).unwrap_or(false)) { st.insert("ui_flags".into(), serde_json::Value::Object(o.clone())); } // small UI flags (tour seen, pinned instances, ...)
        if let Some(n) = b["mouse_radius"].as_i64() { st.insert("mouse_radius".into(), json!(n.clamp(10, 100))); } // Christmas/Halloween: mouse radius (%)
        if let Some(n) = b["snow_fall"].as_i64() { st.insert("snow_fall".into(), json!(n.clamp(1, 100))); } // Christmas: snowfall amount (1 to 100, 10 = default)
        if let Some(n) = b["snow_speed"].as_i64() { st.insert("snow_speed".into(), json!(n.clamp(10, 1000))); } // Christmas: snowfall speed (%)
        // minutes east of UTC, reported by the window; the daily server restart uses it to read the local clock
        if let Some(o) = b["tz_offset"].as_i64().filter(|o| (-840..=840).contains(o)) { st.insert("tz_offset".into(), json!(o)); }
        let mut accts = accounts(&st);
        if b.get("username").is_some() {
            let u = valid_name(s(b, "username"))?;
            if !u.is_empty() && !accts.contains(&u) { accts.push(u.clone()); }
            st.insert("username".into(), json!(u));
        }
        if let Some(a) = b["add_account"].as_str().filter(|a| !a.is_empty()) {
            let u = valid_name(a)?;
            if !u.is_empty() && !accts.contains(&u) { accts.push(u.clone()); }
            if !u.is_empty() && st.get("username").and_then(|x| x.as_str()).unwrap_or("").is_empty() { st.insert("username".into(), json!(u)); }
        }
        let mut av: Map<String, Value> = st.get("avatars").and_then(|a| a.as_object().cloned()).unwrap_or_default();
        if let (Some(n), Some(d)) = (b["avatar_set"].as_str().filter(|x| !x.is_empty()), b["avatar_data"].as_str()) {
            if !accts.iter().any(|a| a == n) { return Err("That account doesn't exist.".into()); }
            let preset = re(r"^preset:[a-z]{2,20}$").is_match(d);
            if !preset && (!d.starts_with("data:image/png;base64,") || d.len() > 400_000) { return Err("The picture must be a PNG image under 300 KB.".into()); }
            av.insert(n.into(), json!(d));
        }
        if let Some(n) = b["avatar_reset"].as_str() { av.remove(n); }
        if let Some(n) = b["remove_account"].as_str().filter(|x| !x.is_empty()) {
            av.remove(n);
            accts.retain(|a| a != n);
            if st.get("username").and_then(|x| x.as_str()) == Some(n) { st.insert("username".into(), json!(accts.first().cloned().unwrap_or_default())); }
        }
        av.retain(|k, _| accts.contains(k));
        st.insert("accounts".into(), json!(accts));
        st.insert("avatars".into(), Value::Object(av));
        if b.get("memory").is_some() {
            let (mem, max) = (ms(&b["memory"]).unwrap_or(0), crate::util::max_game_memory_mb());
            if mem != 0 && !(512..=max).contains(&mem) { return Err(format!("Memory should be between 512 and {max} MB (the memory in this computer).")); }
            st.insert("memory".into(), json!(mem));
        }
        if let Some(o) = b["java_paths"].as_object() {
            let mut slots = st.get("java_paths").and_then(|v| v.as_object().cloned()).unwrap_or_default();
            for major in store::JAVA_SLOTS {
                let Some(p) = o.get(&major.to_string()).and_then(|v| v.as_str()).map(str::trim) else { continue };
                if p.is_empty() { slots.remove(&major.to_string()); continue; }
                if !Path::new(p).is_file() { return Err(format!("That Java {major} file doesn't exist.")); }
                slots.insert(major.to_string(), json!(p));
            }
            st.insert("java_paths".into(), Value::Object(slots));
        }
        if let Some(e) = b["env_vars"].as_str() {
            let e: String = e.trim().chars().take(1000).collect();
            launcher::parse_env(&e)?;
            st.insert("env_vars".into(), json!(e));
        }
        if let Some(f) = b["fullscreen"].as_bool() { st.insert("fullscreen".into(), json!(f)); }
        for (k, label) in [("width", "Width"), ("height", "Height")] {
            if b.get(k).is_some() {
                let n = ms(&b[k]).filter(|n| (320..=16384).contains(n)).ok_or_else(|| format!("{label} should be a number between 320 and 16384."))?;
                st.insert(k.into(), json!(n));
            }
        }
        for k in ["hook_pre", "hook_wrapper", "hook_post"] {
            if let Some(h) = b[k].as_str() { st.insert(k.into(), json!(h.trim().chars().take(1000).collect::<String>())); }
        }
        for (k, _) in syncing::ITEMS {
            if let Some(on) = b[k].as_bool() { st.insert(k.into(), json!(on)); }
        }
        if let Some(a) = b["auto_deps"].as_bool() { st.insert("auto_deps".into(), json!(a)); }
        if let Some(a) = b["jvm_args"].as_str() { st.insert("jvm_args".into(), json!(a.trim().chars().take(500).collect::<String>())); }
        dir_setting(&mut st, b, "tlauncher_dir", "TLauncher")?;
        dir_setting(&mut st, b, "modrinth_dir", "Modrinth App")?;
        dir_setting(&mut st, b, "curseforge_dir", "CurseForge")?;
        // Microsoft accounts
        if let Some(c) = b["ms_client_id"].as_str() {
            let c = c.trim();
            if !c.is_empty() && !re(r"^[0-9a-fA-F-]{36}$").is_match(c) { return Err("An Azure application ID looks like 00000000-0000-0000-0000-000000000000.".into()); }
            st.insert("ms_client_id".into(), json!(c));
        }
        if let Some(a) = b["ms_active"].as_str() {
            if !a.is_empty() && !auth::accounts().iter().any(|x| x["id"] == a) { return Err("That Microsoft account isn't signed in.".into()); }
            st.insert("ms_active".into(), json!(a));
        }
        store::save_settings(&st)?;
        if let Some(id) = b["ms_remove"].as_str().filter(|x| !x.is_empty()) { auth::remove(id)?; }
    }
    let st = store::settings();
    let g = |k: &str| st.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let on = |k: &str| st.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let ms_accts = auth::accounts();
    let active = g("ms_active");
    let player = ms_accts.iter().find(|a| a["id"] == active.as_str()).and_then(|a| a["name"].as_str().map(String::from)).unwrap_or_else(|| g("username"));
    Ok(json!({
        "has_cf_key": !g("cf_key").is_empty(),
        "folder": store::inst_dir().to_string_lossy(), "default_folder": store::default_instances().to_string_lossy(),
        "username": g("username"), "accounts": accounts(&st), "avatars": st.get("avatars").cloned().unwrap_or(json!({})),
        "memory": st.get("memory").cloned().unwrap_or(json!(4096)),
        "java_paths": st.get("java_paths").cloned().unwrap_or(json!({})), "jvm_args": g("jvm_args"), "env_vars": g("env_vars"), "max_memory": crate::util::max_game_memory_mb(),
        "fullscreen": on("fullscreen"), "width": store::window_width(&st), "height": store::window_height(&st),
        "hook_pre": g("hook_pre"), "hook_wrapper": g("hook_wrapper"), "hook_post": g("hook_post"),
        "sync_options": on("sync_options"), "sync_servers": on("sync_servers"), "sync_packs": on("sync_packs"),
        "auto_deps": servers::auto_deps(),
        "tl_dir": store::tl_root().to_string_lossy(), "tl_default": store::default_mc_dir().to_string_lossy(),
        "tl_is_default": g("tlauncher_dir").is_empty(), "is_default": g("instances_dir").is_empty(),
        "theme": st.get("theme").cloned().unwrap_or(json!("oled")), "moved": moved,
        "accent": g("accent"), "high_contrast": st.get("high_contrast").and_then(|v| v.as_bool()).unwrap_or(true),
        "close_warning": store::close_warning(), "close_timer": store::close_timer(),
        "animations": st.get("animations").and_then(|v| v.as_bool()).unwrap_or(true),
        "snow_height": store::snow_height(), "snow_speed": store::snow_speed(), "snow_fall": store::snow_fall(), "mouse_radius": store::mouse_radius(),
        "ui_flags": st.get("ui_flags").cloned().unwrap_or(json!({})),
        "mr_dir": store::mr_root().to_string_lossy(), "mr_default": store::default_modrinth_dir().to_string_lossy(),
        "mr_is_default": g("modrinth_dir").is_empty(),
        "cfa_dir": store::cfapp_root().to_string_lossy(), "cfa_default": store::default_curseforge_dir().to_string_lossy(),
        "cfa_is_default": g("curseforge_dir").is_empty(),
        "count": store::instance_folders(&store::inst_dir()).len(),
        "ms_accounts": ms_accts, "ms_active": active, "ms_client_id": auth::client_id(), "player": player,
    }))
}

/// Settings > Java & memory > the "Valid" badge: is `path` a Java of the version `slot` stands for?
fn java_check(path: &str, slot: &str) -> Res {
    let (path, slot) = (Path::new(path.trim()), slot.parse::<u32>().unwrap_or(0));
    if !path.is_file() { return Ok(json!({"valid": false, "reason": "File not found"})); }
    let major = launcher::java_major(path);
    if major == 0 { return Ok(json!({"valid": false, "reason": "Not a Java runtime"})); }
    if major != slot { return Ok(json!({"valid": false, "reason": format!("This is Java {major}, not Java {slot}")})); }
    Ok(json!({"valid": true, "major": major}))
}

// ---------- groups ----------

fn all_groups() -> Vec<String> {
    let mut names: Vec<String> = store::settings().get("groups").and_then(|g| g.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    for p in store::instance_folders(&store::inst_dir()) {
        let inst = store::Instance::open(&p);
        names.extend(inst.state["groups"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)));
    }
    names.sort_by_key(|n| n.to_lowercase()); names.dedup();
    names
}

fn add_group(name: &str) -> Result<String, String> {
    let name: String = name.trim().chars().take(40).collect();
    if name.is_empty() { return Err("Give the group a name.".into()); }
    if let Some(g) = all_groups().into_iter().find(|g| g.to_lowercase() == name.to_lowercase()) { return Ok(g); }
    let mut st = store::settings();
    let mut cur: Vec<Value> = st.get("groups").and_then(|g| g.as_array().cloned()).unwrap_or_default();
    cur.push(json!(name));
    st.insert("groups".into(), json!(cur));
    store::save_settings(&st)?;
    Ok(name)
}

fn rewrite_groups(f: impl Fn(Vec<String>) -> Vec<String>) -> Result<(), String> {
    for mut inst in store::each_instance() {
        let g: Vec<String> = inst.state["groups"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
        let n = f(g.clone());
        if n != g { inst.state["groups"] = json!(n); inst.save()?; }
    }
    let mut st = store::settings();
    let g: Vec<String> = st.get("groups").and_then(|g| g.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    st.insert("groups".into(), json!(f(g)));
    store::save_settings(&st)
}

fn rename_group(old: &str, new: &str) -> Result<(), String> {
    let new: String = new.trim().chars().take(40).collect();
    if new.is_empty() { return Err("Give the group a name.".into()); }
    if new.to_lowercase() != old.to_lowercase() && all_groups().iter().any(|g| g.to_lowercase() == new.to_lowercase()) { return Err("A group with that name already exists.".into()); }
    rewrite_groups(|g| { let mut out: Vec<String> = vec![]; for x in g { let x = if x == old { new.clone() } else { x }; if !out.contains(&x) { out.push(x); } } out })
}

fn delete_group(name: &str) -> Result<(), String> { rewrite_groups(|g| g.into_iter().filter(|x| x != name).collect()) }

// ---------- instances ----------

fn valid_id(id: &str) -> Result<(), String> { if store::valid_id(id) { Ok(()) } else { Err("bad instance id".into()) } }
fn inst_path(id: &str) -> Result<PathBuf, String> { valid_id(id)?; Ok(store::inst_dir().join(id)) }

pub fn open_path(target: &str) {
    use std::process::Command;
    let _ = if cfg!(windows) {
        if target.starts_with("http") { Command::new("rundll32").args(["url.dll,FileProtocolHandler", target]).spawn() } else { Command::new("explorer").arg(target).spawn() }
    } else if cfg!(target_os = "macos") { Command::new("open").arg(target).spawn() } else { Command::new("xdg-open").arg(target).spawn() };
}

pub fn open_folder(path: &Path) { let _ = std::fs::create_dir_all(path); open_path(&path.to_string_lossy()); }

fn sync_inst(inst: &mut store::Instance) {
    let (changed, new) = gamedata::sync_untracked(inst);
    // Instances made before modpack files were matched with Modrinth get one pass, so their mods get icons + updates too.
    let stale = inst.state["mr_link"] != 1 && inst.mods().values().any(|m| m["source"] == "modpack");
    if stale { inst.state["mr_link"] = json!(1); }
    if changed || stale { let _ = inst.save(); }
    if !new.is_empty() || stale {                     // find them on Modrinth in the background (icon, updates)
        let root = inst.root.clone();
        std::thread::spawn(move || { static L: Mutex<()> = Mutex::new(()); let _g = L.lock().unwrap_or_else(|e| e.into_inner());
            let mut i = store::Instance::open(&root); importers::link_modrinth(&mut i); let _ = i.save(); });
    }
}

fn list_instances() -> Vec<Value> {
    let mut out: Vec<Value> = store::instance_folders(&store::inst_dir()).iter().map(|p| { let mut i = store::Instance::open(p); sync_inst(&mut i); store::summary(&i) }).collect();
    out.sort_by(|a, b| b["created"].as_f64().partial_cmp(&a["created"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn new_iid(name: &str) -> String {
    let slug = re(r"[^a-z0-9]+").replace_all(&name.to_lowercase(), "-").trim_matches('-').to_string();
    format!("{}-{:04x}", if slug.is_empty() { "instance".into() } else { slug }, rand::random::<u16>())
}

pub fn create_instance_raw(name: &str, mc: &str, loader: &str, lv: &str) -> Result<store::Instance, String> {
    let name: String = name.trim().chars().take(48).collect();
    if name.is_empty() || mc.is_empty() || !(mods::is_loader(loader) || loader == "vanilla") { return Err("Name, game version and loader are required.".into()); }
    let lv = if loader == "vanilla" { "" } else { lv.trim() };
    if !lv.is_empty() && !re(r"^[A-Za-z0-9._+\-]{1,64}$").is_match(lv) { return Err("That loader version isn't valid.".into()); }
    let mut inst = store::Instance::open(&store::inst_dir().join(new_iid(&name)));
    let mut cfg = json!({"mc": mc, "loader": loader});
    if !lv.is_empty() { cfg["loader_version"] = json!(lv); }
    inst.state["name"] = json!(name);
    inst.state["created"] = json!(store::now());
    inst.state["config"] = cfg;
    std::fs::create_dir_all(inst.root.join("mods")).map_err(|e| e.to_string())?;
    inst.save()?;
    Ok(inst)
}

/// A full copy of the instance (mods, worlds, settings) under a new id, with its own name, creation date and playtime.
fn duplicate_instance(src: &store::Instance, iid: &str) -> Res {
    if launcher::instance_busy(iid) { return Err("Close the game before duplicating this instance.".into()); }
    let name: String = format!("{} copy", src.state["name"].as_str().unwrap_or(iid)).chars().take(48).collect();
    let root = store::inst_dir().join(new_iid(&name));
    if let Err(e) = copy_dir(&src.root, &root) { let _ = std::fs::remove_dir_all(&root); return Err(e); }
    let mut dup = store::Instance::open(&root);
    dup.state["name"] = json!(name);
    dup.state["created"] = json!(store::now());
    dup.state.as_object_mut().map(|o| { o.remove("last_played"); o.remove("played_seconds"); });
    dup.save()?;
    Ok(store::summary(&dup))
}

fn create_instance(name: &str, mc: &str, loader: &str, lv: &str) -> Res { Ok(store::summary(&create_instance_raw(name, mc, loader, lv)?)) }

/// loader -> the highest game version "Add Optimization" works for, up to and including (see optimization.rs), or why there is none.
/// Looking it up takes a few Modrinth requests per loader, so the answer is kept for a while.
fn optimization_ceilings() -> optimization::Ceilings {
    static CACHE: Mutex<Option<(Instant, optimization::Ceilings)>> = Mutex::new(None);
    if let Some((t, c)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        // a failed lookup (offline, nothing found) is retried sooner than a good one
        if t.elapsed().as_secs() < if c.errors.is_empty() { 600 } else { 60 } { return c.clone(); }
    }
    let c = optimization::ceilings(|loader| {
        let vs = game_versions(false)?;
        let names: Vec<String> = vs["versions"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
        let anchors = optimization::anchor_game_versions(loader)?;
        optimization::newest_with_anchors(loader, &names, &anchors, |mc| match loader_versions(loader, mc) {
            Ok(_) => Ok(true),
            Err(e) if e.contains("isn't available") || e.contains("starts at Minecraft") => Ok(false),
            Err(e) => Err(e),
        })
    });
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), c.clone()));
    c
}

fn optimization_info() -> Res { let c = optimization_ceilings(); Ok(json!({"loaders": optimization::SUPPORTED, "ceilings": c.versions, "errors": c.errors})) }

/// Custom setup with "Add Optimization" ticked. The rule is enforced here, before anything is created; after
/// that the instance always exists and the optimization outcome (ok / error / skipped) rides along in the reply.
fn create_instance_optimized(task: &Option<String>, name: &str, mc: &str, loader: &str, lv: &str) -> Res {
    optimization::check(loader, mc, &optimization_ceilings())?;
    let mut inst = create_instance_raw(name, mc, loader, lv)?;
    let mut report = Value::Null;
    let (log, _) = run_job(task, |log| { report = optimization::apply(&mut inst, log); Ok(()) });
    let mut out = store::summary(&inst);
    out["optimization"] = report;
    out["log"] = json!(log);
    Ok(out)
}

fn set_icon(inst: &mut store::Instance, data: Option<&[u8]>) -> Result<(), String> {
    if let Some(d) = data {
        if d.len() < 8 || d[..8] != *b"\x89PNG\r\n\x1a\n" { return Err("The icon must be a PNG image.".into()); }
        if d.len() > 2_000_000 { return Err("That icon is too big (2 MB at most).".into()); }
    }
    let icon_re = re(r"(?i)^icon(-\d+)?\.(png|jpe?g|webp|gif|svg)$");
    for e in std::fs::read_dir(&inst.root).into_iter().flatten().flatten() {
        if e.path().is_file() && icon_re.is_match(&e.file_name().to_string_lossy()) { let _ = std::fs::remove_file(e.path()); }
    }
    inst.state.as_object_mut().map(|o| o.remove("icon"));
    if let Some(d) = data {
        let name = format!("icon-{}.png", (store::now() * 1000.0) as i64);
        std::fs::write(inst.root.join(&name), d).map_err(|e| e.to_string())?;
        inst.state["icon"] = json!(name);
    }
    inst.save()
}

fn loader_versions(loader: &str, mc: &str) -> Res {
    if !mods::is_loader(loader) || !re(r"^[A-Za-z0-9._\-]{1,32}$").is_match(mc) { return Err("bad loader or game version".into()); }
    let k = (loader.to_string(), mc.to_string());
    if let Some((_, t, v)) = LVCACHE.lock().unwrap().iter().find(|(c, _, _)| *c == k) { if t.elapsed().as_secs() < 600 { return Ok(v.clone()); } }
    let v = meta::loader_versions(loader, mc)?;
    let mut c = LVCACHE.lock().unwrap();
    c.retain(|(x, _, _)| *x != k);
    c.push((k, Instant::now(), v.clone()));
    Ok(v)
}

fn export_pack(inst: &store::Instance, b: &Value) -> Res {
    let name = s(b, "name").trim();
    let version = s(b, "version").trim();
    let safe = re(r#"[\\/:*?"<>|]+"#).replace_all(&format!("{name} {version}"), "").trim_matches([' ', '.']).to_string();
    let safe = if safe.is_empty() { "modpack".to_string() } else { safe };
    let dest = s(b, "dest").trim();
    let d = if !dest.is_empty() {
        let mut d = expand(dest);
        if d.extension().map(|e| e.to_string_lossy().to_lowercase()) != Some("mrpack".into()) { d = d.with_file_name(format!("{}.mrpack", d.file_name().unwrap_or_default().to_string_lossy())); }
        d
    } else {
        let base = store::home_dir().join("Downloads");
        let mut d = base.join(format!("{safe}.mrpack"));
        let mut n = 2;
        while d.exists() { d = base.join(format!("{safe} ({n}).mrpack")); n += 1; }
        d
    };
    let include: Vec<String> = b["include"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
    let _g = JOB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    use_key();
    mods::build_mrpack(inst, &d, name, version, s(b, "description"), &include)
}

fn toggle_content(inst: &mut store::Instance, key: &str) -> Result<(), String> {
    let m = inst.mods().get(key).cloned().ok_or("That item isn't in this instance.")?;
    let p = mods::content_path(inst, &m);
    let off = mods::disabled_path(&p);
    if m["disabled"] == true { if off.exists() { std::fs::rename(&off, &p).map_err(|e| e.to_string())?; } inst.mods_mut().get_mut(key).unwrap()["disabled"] = json!(false); }
    else { if p.exists() { std::fs::rename(&p, &off).map_err(|e| e.to_string())?; } inst.mods_mut().get_mut(key).unwrap()["disabled"] = json!(true); }
    inst.save()
}

fn list_files(inst: &store::Instance, rel: &str) -> Res {
    let base = inst.root.canonicalize().unwrap_or(inst.root.clone());
    let d = crate::util::resolve_in(&inst.root, rel, "instance")?;
    if !d.is_dir() { return Ok(json!({"path": rel, "entries": []})); }
    let mut out: Vec<Value> = vec![];
    for e in std::fs::read_dir(&d).map_err(|e| e.to_string())?.flatten() {
        let Ok(m) = e.metadata() else { continue };
        let mt = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        out.push(json!({"name": e.file_name().to_string_lossy(), "dir": m.is_dir(), "size": if m.is_dir() { 0 } else { m.len() }, "mtime": mt}));
    }
    out.sort_by_key(|e| (!e["dir"].as_bool().unwrap_or(false), e["name"].as_str().unwrap_or("").to_lowercase()));
    let path = if d == base { String::new() } else { d.strip_prefix(&base).unwrap().to_string_lossy().replace('\\', "/") };
    Ok(json!({"path": path, "entries": out}))
}

fn read_log(inst: &store::Instance) -> Res {
    let cands: Vec<PathBuf> = ["latest.log", "quarry-launch.log"].iter().map(|n| inst.root.join("logs").join(n)).filter(|p| p.is_file()).collect();
    let Some(f) = cands.into_iter().max_by_key(|c| std::fs::metadata(c).and_then(|m| m.modified()).ok()) else { return Ok(json!({"text": "", "name": ""})) };
    use std::io::{Read, Seek, SeekFrom};
    let mut fh = std::fs::File::open(&f).map_err(|e| e.to_string())?;
    let len = fh.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    fh.seek(SeekFrom::Start(len.saturating_sub(120_000))).map_err(|e| e.to_string())?;
    let mut buf = vec![]; fh.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    Ok(json!({"text": lines[lines.len().saturating_sub(400)..].join("\n"), "name": f.file_name().unwrap().to_string_lossy()}))
}

fn page_url(key: &str, m: &Value) -> String {
    let slug = m["slug"].as_str().unwrap_or("");
    if slug.is_empty() || ["pack:", "tl:", "imp:"].iter().any(|p| key.starts_with(p)) { return String::new(); }
    let t = m["type"].as_str().unwrap_or("mod");
    if key.starts_with("cf:") {
        let c = match t { "resourcepack" => "texture-packs", "shader" => "shaders", "datapack" => "data-packs", _ => "mc-mods" };
        return format!("https://www.curseforge.com/minecraft/{c}/{slug}");
    }
    format!("https://modrinth.com/{t}/{slug}")
}

fn instance_detail(iid: &str) -> Res {
    let mut inst = store::load_inst(iid)?;
    sync_inst(&mut inst);
    let inst = store::load_inst(iid)?;
    let mut d = store::summary(&inst);
    d["launch"] = launcher::status(iid);
    d["content"] = json!(inst.mods().iter().map(|(k, m)| { let mut m = m.clone(); m["key"] = json!(k); m["type"] = json!(m["type"].as_str().unwrap_or("mod")); m["url"] = json!(page_url(k, &m));
        // no Modrinth / CurseForge icon (hand-added, imported, not matched): the page asks for the one inside the file
        let no_icon = m["icon_url"].as_str().map(|u| u.is_empty()).unwrap_or(true);
        m["icon_local"] = json!(no_icon && matches!(m["type"].as_str(), Some("mod") | Some("resourcepack") | Some("datapack")));
        m }).collect::<Vec<_>>());
    d["modpack"] = inst.state.get("modpack").cloned().map(|mut m| { if let Some(o) = m.as_object_mut() { o.remove("files"); } m }).unwrap_or(Value::Null);
    Ok(d)
}

fn s_(v: &Value) -> String { v.as_str().unwrap_or("").to_string() }

/// Versions of the modpack this instance was installed from, with the installed one marked.
fn modpack_versions(inst: &store::Instance) -> Res {
    let mp = &inst.state["modpack"];
    let pid = s_(&mp["project_id"]);
    if pid.is_empty() { return Ok(json!({"linked": false, "versions": []})); }
    use_key();
    let vs = mods::pack_versions(&pid)?;
    let cur_id = s_(&mp["version_id"]);
    let cur_num = s_(&mp["version"]);
    let vs: Vec<Value> = vs.into_iter().map(|mut v| {
        let is_cur = if !cur_id.is_empty() { v["id"].as_str() == Some(cur_id.as_str()) } else { v["version_number"].as_str() == Some(cur_num.as_str()) };
        v["current"] = json!(is_cur); v
    }).collect();
    Ok(json!({"linked": true, "title": mp["title"], "version": mp["version"], "versions": vs}))
}

fn set_installation(inst: &mut store::Instance, iid: &str, b: &Value) -> Res {
    let (loader, mc) = (s(b, "loader"), s(b, "mc").trim());
    if !(mods::is_loader(loader) || loader == "vanilla") || !re(r"^[A-Za-z0-9._\-]{1,32}$").is_match(mc) { return Err("Pick a platform and a game version.".into()); }
    let lv = if loader == "vanilla" { "" } else { s(b, "loader_version").trim() };
    if !lv.is_empty() && !re(r"^[A-Za-z0-9._+\-]{1,64}$").is_match(lv) { return Err("That loader version isn't valid.".into()); }
    if launcher::instance_busy(iid) { return Err("Close the game before changing its installation.".into()); }
    inst.state["config"]["mc"] = json!(mc);
    inst.state["config"]["loader"] = json!(loader);
    if lv.is_empty() { inst.state["config"].as_object_mut().map(|o| o.remove("loader_version")); } else { inst.state["config"]["loader_version"] = json!(lv); }
    inst.save()?;
    instance_detail(iid)
}

fn all_worlds() -> Vec<Value> {
    let mut out = vec![];
    for inst in store::each_instance() {
        for mut w in gamedata::list_worlds(&inst.root) {
            w["iid"] = json!(inst.id());
            w["inst"] = inst.state.get("name").cloned().unwrap_or(json!(inst.id()));
            out.push(w);
        }
    }
    out.sort_by(|a, b| b["last_played"].as_f64().partial_cmp(&a["last_played"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    out
}

// ---------- screenshots ----------

fn shot_type(p: &Path) -> Option<&'static str> {
    let e = p.extension()?.to_string_lossy().to_lowercase();
    SHOT_TYPES.iter().find(|(x, _)| *x == e).map(|x| x.1)
}

fn source_dirs(inst: &store::Instance) -> Vec<PathBuf> {
    let mut out = vec![];
    for x in inst.state["imported_from"].as_array().into_iter().flatten() {
        let id = x["id"].as_str().unwrap_or("");
        match x["launcher"].as_str() {
            Some("modrinth") => out.push(importers::modrinth_profile_dir(&store::mr_root()).join(id)),
            Some("curseforge") => { let b = store::cfapp_root().join("Instances"); out.push(if b.is_dir() { b } else { store::cfapp_root() }.join(id)); }
            _ => {}
        }
    }
    if let Some(p) = inst.state["source_path"].as_str().filter(|s| !s.is_empty()) { out.push(PathBuf::from(p)); }
    out.into_iter().filter(|d| d.is_dir()).collect()
}

fn screenshot_dirs(inst: &store::Instance) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = vec![];
    let mut bases = vec![inst.root.clone()];
    bases.extend(source_dirs(inst));
    for base in bases {
        let mut stack = vec![(base, 0)];
        while let Some((d, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let x = e.path();
                if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) { continue; }
                let name = e.file_name().to_string_lossy().to_lowercase();
                if name == "screenshots" { found.push(x); } else if depth < 2 && !SHOT_SKIP.contains(&name.as_str()) { stack.push((x, depth + 1)); }
            }
        }
    }
    let (mut seen, mut out) = (std::collections::HashSet::new(), vec![]);
    for d in found { if seen.insert(d.canonicalize().unwrap_or(d.clone())) { out.push(d); } }
    out
}

fn shot_file(iid: &str, f: &str) -> Result<PathBuf, String> {
    let inst = store::load_inst(iid)?;
    let p = Path::new(f).canonicalize().map_err(|_| "Screenshot not found.".to_string())?;
    let ok = shot_type(&p).is_some() && p.is_file() && screenshot_dirs(&inst).iter().any(|d| d.canonicalize().map(|c| p.starts_with(c)).unwrap_or(false));
    if ok { Ok(p) } else { Err("Screenshot not found.".into()) }
}

fn rglob(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) { rglob(&p, out); } else { out.push(p); }
    }
}

fn list_screenshots() -> Vec<Value> {
    let (mut out, mut seen) = (vec![], std::collections::HashSet::new());
    for inst in store::each_instance() {
        for d in screenshot_dirs(&inst) {
            let mut files = vec![]; rglob(&d, &mut files);
            for f in files {
                if shot_type(&f).is_none() || !f.is_file() { continue; }
                let Ok(m) = std::fs::metadata(&f) else { continue };
                if !seen.insert(f.canonicalize().unwrap_or(f.clone())) { continue; }
                let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0);
                out.push(json!({"iid": inst.id(), "inst": inst.state.get("name").cloned().unwrap_or(json!(inst.id())), "file": f.file_name().unwrap().to_string_lossy(), "path": f.to_string_lossy(), "t": t, "size": m.len()}));
            }
        }
    }
    out.sort_by(|a, b| b["t"].as_f64().partial_cmp(&a["t"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    out
}

// ---------- modpacks / creating instances from projects ----------

fn pack_job(task: &Option<String>, get_info: impl FnOnce(&Path) -> Result<mods::PackInfo, String>, tmp: PathBuf) -> Res {
    let _ = std::fs::create_dir_all(&tmp);
    let mut created: Option<String> = None;
    let (log, err) = run_job(task, |log| {
        let info = get_info(&tmp)?;
        let mut inst = create_instance_raw(&info.title, &info.mc, &info.loader, &info.loader_version)?;
        created = Some(inst.id());
        mods::apply_modpack(&mut inst, &info, log)?;
        let _ = mods::ensure_deps(&mut inst, log);   // libraries the jars need are added now, not when the player later clicks "Install missing dependencies"
        inst.save()
    });
    let _ = std::fs::remove_dir_all(&tmp);
    if let Some(e) = err {
        if let Some(id) = &created { let _ = std::fs::remove_dir_all(store::inst_dir().join(id)); }   // don't leave a half-installed instance
        return Err(e);
    }
    Ok(json!({"id": created, "log": log}))
}

fn tmp_dir() -> PathBuf { std::env::temp_dir().join(format!("orbit-pack-{:08x}", rand::random::<u32>())) }

fn install_modpack(task: &Option<String>, source: &str, ident: &str, mc: &str, version_id: &str) -> Res {
    if source != "modrinth" { return Err("CurseForge modpacks aren't supported yet. Switch the source to Modrinth.".into()); }
    let (ident, mc, vid) = (ident.to_string(), mc.to_string(), version_id.to_string());
    pack_job(task, move |tmp| if vid.is_empty() { mods::fetch_modpack(&ident, tmp, if mc.is_empty() { None } else { Some(&mc) }) } else { mods::fetch_pack_version(&ident, &vid, tmp) }, tmp_dir())
}

fn upload_modpack(task: &Option<String>, name: &str, data: &[u8]) -> Res {
    if !name.to_lowercase().ends_with(".mrpack") { return Err("Choose a .mrpack file (Modrinth modpack).".into()); }
    if data.is_empty() { return Err("That file is empty.".into()); }
    let tmp = tmp_dir();
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let path = tmp.join("upload.mrpack");
    std::fs::write(&path, data).map_err(|e| e.to_string())?;
    pack_job(task, move |_| mods::load_mrpack(&path), tmp)
}

fn create_from_project(task: &Option<String>, source: &str, ident: &str, ptype: &str) -> Res {
    if source != "modrinth" { return Err("Only Modrinth projects can start an instance for now.".into()); }
    if ptype == "modpack" { return install_modpack(task, source, ident, "", ""); }
    use_key();
    let meta = mods::api_get(&format!("/project/{ident}"), &[])?;
    if meta.is_null() { return Err("That project wasn't found on Modrinth.".into()); }
    let title = s(&meta, "title").to_string();
    let loaders: Vec<&str> = meta["loaders"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
    let loader = if ptype == "mod" {
        meta::LOADER_PRIORITY.into_iter().find(|l| loaders.contains(l)).ok_or_else(|| format!("{title} doesn't support a loader Orbit can run."))?
    } else { "fabric" };
    let params: Vec<(&str, String)> = if ptype == "mod" { vec![("loaders", mods::jl(&[loader]))] } else { vec![] };
    let vers = mods::api_get(&format!("/project/{}/version", s(&meta, "id")), &params)?;
    let Some(vs) = vers.as_array().filter(|v| !v.is_empty()) else { return Err(format!("{title} has no versions to start from.")) };
    let v = vs.iter().find(|x| x["version_type"] == "release").unwrap_or(&vs[0]);
    let gv: Vec<&str> = v["game_versions"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
    let order = game_versions(false)?;
    let mc = order["versions"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).find(|n| gv.contains(n)).map(String::from)
        .or_else(|| gv.last().map(|s| s.to_string())).ok_or("That project lists no game versions.")?;
    let mut inst = create_instance_raw(&title, &mc, loader, "")?;
    let iid = inst.id();
    let (pid, ptype2) = (s(&meta, "id").to_string(), ptype.to_string());
    let (log, _) = run_job(task, |log| { mods::install_mod(&mut inst, "modrinth", &pid, true, &ptype2, log)?; let _ = mods::ensure_deps(&mut inst, log); inst.save() });
    if store::load_inst(&iid)?.mods().is_empty() {
        let _ = std::fs::remove_dir_all(store::inst_dir().join(&iid));
        return Err(if log.is_empty() { format!("Couldn't install {title}.") } else { log.join(" ") });
    }
    Ok(json!({"id": iid, "log": log}))
}

// ---------- Minecraft versions / filters ----------

fn game_versions(snapshots: bool) -> Res {
    use_key();
    let cf_on = !mods::cf_key().is_empty();
    let ck = (snapshots, cf_on);
    if let Some((_, t, v)) = VCACHE.lock().unwrap().iter().find(|(c, _, _)| *c == ck) { if t.elapsed().as_secs() < 3600 { return Ok(v.clone()); } }
    let mr = mods::api_get("/tag/game_version", &[])?;
    let mut vs: Vec<Value> = mr.as_array().cloned().unwrap_or_default().into_iter().filter(|v| snapshots || v["version_type"] == "release").collect();
    vs.sort_by(|a, b| s(b, "date").cmp(s(a, "date")));
    let mut names: Vec<String> = vs.iter().map(|v| s(v, "version").to_string()).collect();
    let mut both = false;
    if cf_on {
        if let Ok(cf) = mods::cf_get("/minecraft/version", &[]) {
            let set: std::collections::HashSet<&str> = cf["data"].as_array().into_iter().flatten().filter_map(|v| v["versionString"].as_str()).collect();
            names.retain(|n| set.contains(n.as_str()));
            both = true;
        }
    }
    let res = json!({"versions": names, "both": both, "latest": vs.iter().find(|v| v["version_type"] == "release").map(|v| v["version"].clone())});
    let mut c = VCACHE.lock().unwrap();
    c.retain(|(k, _, _)| *k != ck);
    c.push((ck, Instant::now(), res.clone()));
    Ok(res)
}

fn tag_filters(ptype: &str) -> Res {
    let kind = if ptype == "mod" || ptype == "datapack" { "mod" } else { ptype };
    let cached = TCACHE.lock().unwrap().iter().find(|(k, _)| k == kind).map(|(_, v)| v.clone());
    let groups = match cached { Some(g) => g, None => {
        let tags = mods::api_get("/tag/category", &[])?;
        let mut groups = Map::new();
        for t in tags.as_array().into_iter().flatten() {
            if t["project_type"] == kind {
                let h = t["header"].as_str().unwrap_or("categories").to_string();
                groups.entry(h).or_insert_with(|| json!([])).as_array_mut().unwrap().push(t["name"].clone());
            }
        }
        let g = Value::Object(groups);
        TCACHE.lock().unwrap().push((kind.to_string(), g.clone()));
        g
    } };
    let order = ["categories", "features", "resolutions", "performance impact"];
    Ok(json!({"groups": order.iter().filter(|h| groups.get(**h).is_some()).map(|h| json!({"header": h, "items": groups[*h]})).collect::<Vec<_>>()}))
}

// ---------- importing from other launchers ----------

fn launcher_info(l: &str) -> Result<(&'static str, PathBuf), String> {
    match l { "tlauncher" => Ok(("TLauncher", store::tl_root())), "modrinth" => Ok(("Modrinth App", store::mr_root())), "curseforge" => Ok(("CurseForge", store::cfapp_root())), _ => Err("Unknown launcher.".into()) }
}

fn scan_launcher(l: &str, root: &Path) -> Result<Vec<Value>, String> {
    match l { "tlauncher" => Ok(importers::scan_tlauncher(root)), "modrinth" => importers::scan_modrinth(root), _ => importers::scan_curseforge(root) }
}

fn imported_ids() -> std::collections::HashSet<(String, String)> {
    let mut have = std::collections::HashSet::new();
    for p in store::instance_folders(&store::inst_dir()) {
        let st = store::Instance::open(&p).state;
        if let Some(v) = st["tlauncher_version"].as_str() { have.insert(("tlauncher".to_string(), v.to_string())); }
        for x in st["imported_from"].as_array().into_iter().flatten() { have.insert((s(x, "launcher").to_string(), s(x, "id").to_string())); }
    }
    have
}

fn import_scan(l: &str) -> Res {
    let (label, root) = launcher_info(l)?;
    if !root.is_dir() { return Err(format!("The {label} folder {} doesn't exist. Set it in Settings > Import folders.", root.display())); }
    let have = imported_ids();
    let mut items = scan_launcher(l, &root)?;
    for c in items.iter_mut() {
        c["imported"] = json!(have.contains(&(l.to_string(), s(c, "id").to_string())));
        c["icon"] = json!(c["icon"].as_str().map(|x| !x.is_empty()).unwrap_or(false));      // the path stays on the server
    }
    Ok(json!({"launcher": l, "label": label, "folder": root.to_string_lossy(), "versions": items}))
}

fn import_run(l: &str, picks: &[Value], saves: bool) -> Res {
    let (label, root) = launcher_info(l)?;
    let found: HashMap<String, Value> = scan_launcher(l, &root)?.into_iter().map(|c| (s(&c, "id").to_string(), c)).collect();
    let mut out = vec![];
    for it in picks {
        let cid = it["id"].as_str().map(String::from).unwrap_or_else(|| it["id"].to_string());
        let mut c = found.get(&cid).cloned().ok_or_else(|| format!("{label} instance '{cid}' wasn't found."))?;
        if !s(&c, "unsupported").is_empty() { return Err(format!("{}: {}", s(&c, "name"), s(&c, "unsupported"))); }
        let source: Option<PathBuf> = if l == "tlauncher" {
            let src = s(it, "source");
            if !src.is_empty() && !Path::new(src).is_dir() { return Err(format!("The folder {src} doesn't exist.")); }
            let p = (!src.is_empty()).then(|| PathBuf::from(src));
            c["shared"] = json!(p.as_ref().map(|p| p.canonicalize().ok() == root.canonicalize().ok()).unwrap_or(false));
            p
        } else { Some(PathBuf::from(s(&c, "folder"))) };
        let mut inst = create_instance_raw(s(&c, "name"), s(&c, "mc"), s(&c, "loader"), s(&c, "loader_version"))?;
        inst.state["imported_from"] = json!([{"launcher": l, "id": cid}]);
        if l == "tlauncher" { inst.state["tlauncher_version"] = json!(cid); }
        let mut res = importers::apply_candidate(&mut inst, &c, source.as_deref(), saves)?;
        for g in inst.state["groups"].as_array().cloned().unwrap_or_default() { let _ = add_group(g.as_str().unwrap_or("")); }
        res["id"] = json!(inst.id());
        res["name"] = inst.state["name"].clone();
        out.push(res);
    }
    Ok(json!({"imported": out}))
}

// ---------- routing ----------

/// Crash diagnosis for an instance folder (also used by the command line).
pub fn diagnose_instance(inst: &store::Instance) -> Value {
    let iid = inst.id();
    let st = store::settings();
    let ram = inst.state["settings"]["memory"].as_i64().filter(|m| *m > 0).or(st.get("memory").and_then(|v| v.as_i64()).filter(|m| *m > 0)).or(Some(4096));
    let l = inst.loader();
    let mut console = vec![];
    if let Some(e) = launcher::status(&iid)["error"].as_str() { console.push(e.to_string()); }
    let inp = crate::diagnose::Input { kind: "instance", name: inst.state["name"].as_str().unwrap_or(&iid).to_string(), root: inst.root.clone(), mc: inst.mc(),
        platform: l.clone(), server: false, ram_mb: ram, port: None, mods_dir: (l != "vanilla").then(|| inst.dir_for(Some("mod"))), loaders: vec![l], console };
    let mut v = crate::diagnose::run(&inp);
    // the page switches mods by their key in the instance record
    let key_of = |file: &str| inst.mods().iter().find(|(_, m)| m["filename"].as_str() == Some(file.strip_suffix(".disabled").unwrap_or(file))).map(|(k, _)| k.clone());
    let fix = |f: &mut Value| for s in f["suspects"].as_array_mut().into_iter().flatten() { let k = key_of(s["file"].as_str().unwrap_or("")); s["key"] = json!(k); };
    if !v["primary"].is_null() { fix(&mut v["primary"]); }
    for f in v["findings"].as_array_mut().into_iter().flatten() { fix(f); }
    v
}

/// `orbit --diagnose <folder>`: print the diagnosis of an instance or server folder.
pub fn cli_diagnose(path: &str) -> i32 {
    let root = std::path::PathBuf::from(path);
    if !root.is_dir() { eprintln!("Not a folder: {path}"); return 2; }
    let name = root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let v = if root.join(store::STATE_NAME).is_file() { diagnose_instance(&store::Instance::open(&root)) }
        else if root.join("server.properties").is_file() || root.join("orbit-server.json").is_file() { servers::diagnose_root(&root, &name).unwrap_or_else(|e| json!({"text": e})) }
        else { eprintln!("That folder is neither an Orbit instance nor a server folder."); return 2; };
    println!("{}", v["text"].as_str().unwrap_or(""));
    if v["status"] == "crash" { 1 } else { 0 }
}

fn instance_action(r: &Req, iid: &str, act: &str) -> Result<Out, String> {
    let (m, b, q) = (r.method, &r.body, &r.q);
    let mut inst = store::load_inst(iid)?;
    let json_out = |v: Res| v.map(Out::Json);
    let with_log = |inst_id: &str, log: Log| -> Result<Out, String> { let mut d = instance_detail(inst_id)?; d["log"] = json!(log); Ok(Out::Json(d)) };
    let keys_of = |v: &Value| -> Vec<String> { v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect() };
    match (m, act) {
        (_, "install") => {
            let ptype = if s(b, "type").is_empty() { "mod" } else { s(b, "type") };
            if inst.loader() == "vanilla" && ptype == "mod" { return Err("Vanilla instances can't load mods. Create a Fabric, Quilt, Forge or NeoForge instance for mods.".into()); }
            let (source, ident) = (s(b, "source").to_string(), s(b, "ident").to_string());
            let (log, _) = run_job(&r.task, |log| mods::install_mod(&mut inst, &source, &ident, true, ptype, log).and_then(|_| inst.save()));
            with_log(iid, log)
        }
        (_, "update") => {
            let keys: Option<std::collections::HashSet<String>> = b["keys"].as_array().filter(|k| !k.is_empty()).map(|_| keys_of(&b["keys"]).into_iter().collect());
            let (log, _) = run_job(&r.task, |log| mods::cmd_update(&mut inst, keys.as_ref(), log));
            with_log(iid, log)
        }
        (_, "updates") => {
            use_key();
            let mut ups = mods::check_updates(&inst);
            for u in ups.iter_mut() {
                let key = s(u, "key").to_string();
                let base = inst.mods().get(&key).map(|m| page_url(&key, m)).unwrap_or_default();
                u["url"] = json!(if base.is_empty() { String::new() } else { format!("{base}{}{}", if key.starts_with("cf:") { "/files/" } else { "/version/" }, s(u, "new_id")) });
            }
            Ok(Out::Json(json!({"updates": ups})))
        }
        (_, "versions") => { use_key(); json_out(mods::list_versions(&inst, q.get("key").map(|s| s.as_str()).unwrap_or("")).map(|v| json!({"versions": v}))) }
        (_, "set-version") => {
            let (key, vid) = (s(b, "key").to_string(), s(b, "version_id").to_string());
            let ptype = if s(b, "type").is_empty() { "mod".to_string() } else { s(b, "type").to_string() };
            let (log, _) = run_job(&r.task, |log| mods::install_version(&mut inst, &key, &vid, &ptype, log));
            with_log(iid, log)
        }
        (_, "diagnose") => Ok(Out::Json(diagnose_instance(&inst))),
        ("POST", "deps") => {
            if inst.loader() == "vanilla" { return Err("Vanilla instances don't take mods.".into()); }
            let mut outcome = None;
            let (log, _) = run_job(&r.task, |log| { outcome = mods::ensure_deps(&mut inst, log); Ok(()) });
            let mut lines = log;
            let deps_json = outcome.map(|o| { lines.extend(crate::deps::describe(&o)); o.to_json() }).unwrap_or(Value::Null);
            if lines.is_empty() { lines.push("Every mod already has what it needs.".into()); }
            let mut d = instance_detail(iid)?; d["log"] = json!(lines); d["deps"] = deps_json; Ok(Out::Json(d))
        }
        (_, "deps") => json_out(Ok(match mods::deps_report(&inst) {
            Some(rep) => { let mut v = crate::deps::report_json(&rep); v["supported"] = json!(true); v }
            None => json!({"supported": false}),
        })),
        (_, "modpack-versions") => json_out(modpack_versions(&inst)),
        (_, "modpack-switch") => {
            if launcher::instance_busy(iid) { return Err("Close the game before changing the modpack version.".into()); }
            let pid = s_(&inst.state["modpack"]["project_id"]);
            if pid.is_empty() { return Err("This instance isn't linked to a Modrinth modpack.".into()); }
            let vid = s(b, "version_id").to_string();
            if vid.is_empty() { return Err("Pick a version.".into()); }
            let tmp = tmp_dir();
            let _ = std::fs::create_dir_all(&tmp);
            let (log, err) = run_job(&r.task, |log| {
                let info = mods::fetch_pack_version(&pid, &vid, &tmp)?;
                mods::switch_modpack(&mut inst, &info, log)?;
                let _ = mods::ensure_deps(&mut inst, log);
                inst.save()
            });
            let _ = std::fs::remove_dir_all(&tmp);
            if let Some(e) = err { return Err(e); }
            let mut d = instance_detail(iid)?; d["log"] = json!(log); Ok(Out::Json(d))
        }
        (_, "installation") => json_out(set_installation(&mut inst, iid, b)),
        (_, "repair") => {
            let _ = std::fs::remove_file(launcher::profile_path(&store::meta_dir(), &inst));
            let mut d = instance_detail(iid)?;
            d["log"] = json!(["Repair queued. The game files are re-checked the next time you press Play."]);
            Ok(Out::Json(d))
        }
        (_, "remove") => {
            let keys = if b["keys"].is_array() { keys_of(&b["keys"]) } else { vec![s(b, "key").to_string()] };
            let (log, _) = run_job(&r.task, |log| mods::cmd_remove(&mut inst, &keys, log));
            with_log(iid, log)
        }
        (_, "play") => {
            let quick = if !s(b, "world").is_empty() { Some(("singleplayer".to_string(), s(b, "world").to_string())) }
                else if !s(b, "server").is_empty() { Some(("multiplayer".to_string(), s(b, "server").to_string())) } else { None };
            let name = inst.state["name"].as_str().unwrap_or(iid).to_string();
            let account = if !s(b, "account_id").is_empty() { Some(("ms".to_string(), s(b, "account_id").to_string())) }
                else if !s(b, "account_name").is_empty() { Some(("offline".to_string(), valid_name(s(b, "account_name"))?)) } else { None };
            json_out(launcher::start(&inst.root, iid, &name, store::settings(), store::meta_dir(), quick, account, b["extra"] == true))
        }
        (_, "worlds") => Ok(Out::Json(json!({"worlds": gamedata::list_worlds(&inst.root)}))),
        (_, "servers") => { let sv = gamedata::list_servers(&inst.root); Ok(Out::Json(json!({"servers": if q.contains_key("ping") && q["ping"] != "0" { gamedata::ping_all(sv) } else { sv }}))) }
        (_, "stop") => {
            // "run" names one of this instance's runs ("<iid>@2"); without it the main run stops
            let key = match s(b, "run") { "" => iid.to_string(), k if k.strip_prefix(&iid[..]).map_or(false, |r| r.starts_with('@')) => k.to_string(), _ => return Err("That run doesn't belong to this instance.".into()) };
            launcher::stop(&key)?; Ok(Out::Json(launcher::status(&key)))
        }
        (_, "status") => Ok(Out::Json(match q.get("run").map(|k| k.as_str()) {
            Some(k) if k.strip_prefix(&iid[..]).map_or(false, |r| r.starts_with('@')) => launcher::status(k),
            _ => launcher::status(iid) })),
        (_, "logs") => json_out(read_log(&inst)),
        (_, "files") => json_out(list_files(&inst, q.get("path").map(|s| s.as_str()).unwrap_or(""))),
        (_, "toggle") => { toggle_content(&mut inst, s(b, "key"))?; let mut d = instance_detail(iid)?; d["log"] = json!([]); Ok(Out::Json(d)) }
        (_, "config") => {
            if let Some(n) = b["name"].as_str().map(|n| n.trim()).filter(|n| !n.is_empty()) { inst.state["name"] = json!(n.chars().take(48).collect::<String>()); }
            if !inst.state["settings"].is_object() { inst.state["settings"] = json!({}); }
            if b.get("memory").is_some() {
                let mem = ms(&b["memory"]).unwrap_or(0);
                let max = crate::util::max_game_memory_mb();
                if mem != 0 && !(512..=max).contains(&mem) { return Err(format!("Memory should be between 512 and {max} MB (the memory in this computer).")); }
                inst.state["settings"]["memory"] = json!(mem);
            }
            if let Some(a) = b["jvm_args"].as_str() { inst.state["settings"]["jvm_args"] = json!(a.trim().chars().take(500).collect::<String>()); }
            if let Some(jp) = b["java_path"].as_str() {
                let jp = jp.trim();
                if !jp.is_empty() && !Path::new(jp).is_file() { return Err("That Java file doesn't exist.".into()); }
                inst.state["settings"]["java_path"] = json!(jp);
            }
            inst.save()?;
            json_out(instance_detail(iid))
        }
        (_, "relink") => {
            let r2 = importers::link_modrinth(&mut inst);
            inst.save()?;
            let line = if !r2["error"].as_str().unwrap_or("").is_empty() { format!("Couldn't reach Modrinth: {}", r2["error"].as_str().unwrap()) }
                else if r2["checked"].as_i64().unwrap_or(0) > 0 { format!("Matched {} of {} files with Modrinth.", r2["linked"], r2["checked"]) }
                else { "Nothing to match. Every file is already linked.".to_string() };
            with_log(iid, vec![line])
        }
        (_, "groups") => {
            let mut gs: Vec<String> = vec![];
            for g in b["groups"].as_array().into_iter().flatten() { let g = add_group(g.as_str().unwrap_or(""))?; if !gs.contains(&g) { gs.push(g); } }
            inst.state["groups"] = json!(gs);
            inst.save()?;
            json_out(instance_detail(iid))
        }
        (_, "export") => json_out(export_pack(&inst, b)),
        ("POST", "icon") => {
            let reset = q.get("reset").map(|x| !x.is_empty() && x != "0").unwrap_or(false);
            if !reset && r.raw.is_empty() { return Err("No image was sent.".into()); }
            set_icon(&mut inst, if reset { None } else { Some(&r.raw) })?;
            json_out(instance_detail(iid))
        }
        (_, "icon") => {
            let f = inst.state["icon"].as_str().unwrap_or("");
            let p = inst.root.join(f);
            match (f.is_empty(), importers::icon_type(&p)) { (false, Some(ct)) if p.is_file() => Ok(Out::Bytes(std::fs::read(&p).map_err(|e| e.to_string())?, ct)), _ => Err("No icon.".into()) }
        }
        (_, "content-icon") => {
            let m = inst.mods().get(q.get("key").map(|k| k.as_str()).unwrap_or("")).cloned().ok_or("No icon.")?;
            let mut p = mods::content_path(&inst, &m);
            if m["disabled"] == true { p = mods::disabled_path(&p); }
            if !p.starts_with(&inst.root) { return Err("No icon.".into()); }
            match icons::icon_for(&p) { Some((b, ct)) => Ok(Out::Bytes(b, ct)), None => Err("No icon.".into()) }
        }
        (_, "tlauncher") => { let (iid2, root) = (iid.to_string(), store::tl_root()); json_out(importers::export_to_tlauncher(&inst, &iid2, &root)) }
        ("POST", "duplicate") => json_out(duplicate_instance(&inst, iid)),
        (_, "open") => { let _ = std::fs::create_dir_all(inst.root.join("mods")); open_folder(&inst.root); Ok(Out::Json(json!({"ok": true}))) }
        _ => Err("unknown action".into()),
    }
}

// ---------- in-app updates ----------
// The work is done by `orbit-updater` (src/bin/orbit-updater.rs), installed next to Orbit: Orbit only asks it whether a
// newer release exists, and, when the person clicks "Update now", starts it and closes so it can swap the files.

const NO_UPDATER: &str = "In-app updates need orbit-updater next to Orbit, and it isn't there. Reinstall with Orbit-Setup.exe / the full release archive, or download the new version by hand.";

fn exe_dir() -> Option<PathBuf> { std::env::current_exe().ok()?.parent().map(Path::to_path_buf) }

fn updater_path() -> Option<PathBuf> {
    let p = exe_dir()?.join(if cfg!(windows) { "orbit-updater.exe" } else { "orbit-updater" });
    p.is_file().then_some(p)
}

/// Cheap, no network: this build's version, whether the updater is installed, and (once) why the last update failed.
fn update_info() -> Res {
    let mut last_error = Value::Null;
    if let Some(f) = exe_dir().map(|d| d.join(".orbit-update-error")) {
        if let Ok(t) = std::fs::read_to_string(&f) { if !t.trim().is_empty() { last_error = json!(t.trim()); } }
        let _ = std::fs::remove_file(&f);
    }
    Ok(json!({"version": env!("CARGO_PKG_VERSION"), "updater": updater_path().is_some(), "last_error": last_error}))
}

/// Runs `orbit-updater --check --json` and returns {current, latest, available, notes}.
fn update_check() -> Res {
    let up = updater_path().ok_or(NO_UPDATER)?;
    let mut c = std::process::Command::new(&up);
    c.args(["--check", "--json"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; c.creation_flags(0x08000000); }   // no console flash
    let mut child = c.spawn().map_err(|e| format!("Couldn't start the updater: {e}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed().as_secs() < 90 => std::thread::sleep(std::time::Duration::from_millis(100)),
            _ => { let _ = child.kill(); let _ = child.wait(); return Err("The update check took too long. Check your internet connection and try again.".into()); }
        }
    }
    let mut out = String::new();
    if let Some(mut o) = child.stdout.take() { use std::io::Read; let _ = o.read_to_string(&mut out); }
    let v: Value = out.lines().rev().find_map(|l| serde_json::from_str(l.trim()).ok()).ok_or("The updater gave no answer.")?;
    if let Some(e) = v["error"].as_str() { return Err(e.to_string()); }
    Ok(v)
}

/// Starts `orbit-updater --yes --wait-pid <this process>` detached, then closes Orbit so the files can be replaced.
/// The updater downloads, verifies the checksum, swaps the files (keeping .old backups) and starts the new Orbit.
fn update_install() -> Res {
    let up = updater_path().ok_or(NO_UPDATER)?;
    let mut c = std::process::Command::new(&up);
    c.arg("--yes").arg("--wait-pid").arg(std::process::id().to_string()).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; c.creation_flags(0x08000000 | 0x00000200); }   // no console, own process group
    #[cfg(unix)] { use std::os::unix::process::CommandExt; c.process_group(0); }
    c.spawn().map_err(|e| format!("Couldn't start the updater: {e}"))?;
    std::thread::spawn(|| { std::thread::sleep(std::time::Duration::from_millis(1200)); std::process::exit(0); });
    Ok(json!({"ok": true}))
}

pub fn route(r: &Req) -> Result<Out, String> {
    let p: Vec<String> = r.path.trim_matches('/').split('/').map(|x| percent_encoding::percent_decode_str(x).decode_utf8_lossy().into_owned()).collect();
    let (m, path, q, b) = (r.method, r.path, &r.q, &r.body);
    let qs = |k: &str| q.get(k).map(|s| s.as_str()).unwrap_or("");
    let j = |v: Res| v.map(Out::Json);
    match path {
        "/api/ping" => return j(Ok(json!({"ok": true}))),
        "/api/update/info" => return j(update_info()),
        "/api/update/check" if m == "POST" => return j(update_check()),
        "/api/update/install" if m == "POST" => return j(update_install()),
        "/api/settings" => return j(settings(r)),
        "/api/auth/begin" if m == "POST" => return j(if auth::MS_LOGIN_DISABLED { Err("Microsoft sign-in is temporarily disabled.".into()) } else { auth::begin() }),
        "/api/auth/status" => return j(Ok(auth::status())),
        "/api/auth/cancel" if m == "POST" => { auth::cancel(); return j(Ok(json!({"ok": true}))); }
        "/api/modpack/install" if m == "POST" => return j(install_modpack(&r.task, if s(b, "source").is_empty() { "modrinth" } else { s(b, "source") }, s(b, "ident"), s(b, "mc"), s(b, "version_id"))),
        "/api/import/scan" => return j(import_scan(qs("launcher"))),
        "/api/import/inspect" => {
            let pth = Path::new(qs("path"));
            if !pth.is_dir() { return Err("That folder doesn't exist.".into()); }
            let c = importers::count_content(pth);
            let total: i64 = c.values().filter_map(|v| v.as_i64()).sum();
            return j(Ok(json!({"path": pth.to_string_lossy(), "counts": c, "total": total})));
        }
        "/api/import/run" if m == "POST" => return j(import_run(s(b, "launcher"), b["items"].as_array().map(|v| v.as_slice()).unwrap_or(&[]), b["saves"] == true)),
        "/api/modpack/upload" if m == "POST" => return j(upload_modpack(&r.task, qs("name"), &r.raw)),
        "/api/instances/from-project" if m == "POST" => return j(create_from_project(&r.task, if s(b, "source").is_empty() { "modrinth" } else { s(b, "source") }, s(b, "ident"), if s(b, "type").is_empty() { "mod" } else { s(b, "type") })),
        "/api/groups" => {
            if m == "POST" {
                if !s(b, "rename").is_empty() { rename_group(s(b, "rename"), s(b, "name"))?; }
                else if !s(b, "delete").is_empty() { delete_group(s(b, "delete"))?; }
                else { add_group(s(b, "name"))?; }
            }
            return j(Ok(json!({"groups": all_groups()})));
        }
        "/api/playtime" => return j(Ok(store::playtime_report())),
        "/api/java/detect" => return j(Ok(json!({"javas": launcher::detect_javas().into_iter().map(|(p, m)| json!({"path": p.to_string_lossy(), "major": m})).collect::<Vec<_>>()}))),
        "/api/java/check" => return j(java_check(qs("path"), qs("slot"))),
        "/api/sync/open" if m == "POST" => { syncing::open(s(b, "item"))?; return j(Ok(json!({"ok": true}))); }
        "/api/launch-status" => return j(Ok(json!({"running": launcher::running()}))),
        "/api/project" => { use_key(); return j(mods::project_page(qs("id"))); }
        "/api/project/versions" => return j(mods::project_versions(qs("id")).map(|v| json!({"versions": v}))),
        "/api/progress" => {
            let t = PROGRESS.lock().unwrap().iter().rev().find(|(k, _)| k == qs("id")).map(|(_, t)| t.clone());
            return j(Ok(t.map(|t| t.lock().unwrap().snapshot()).unwrap_or(json!({"pct": null}))));
        }
        "/api/task/cancel" if m == "POST" => { cancel_task(s(b, "task")); return j(Ok(json!({"ok": true}))); }
        "/api/tasks" => return j(tasks(r)),
        "/api/open-url" if m == "POST" => {
            let u = s(b, "url");
            let ok = ["https://modrinth.com/", "https://www.curseforge.com/", "https://curseforge.com/"].iter().any(|h| u.starts_with(h));
            if !ok { return Err("That link can't be opened from here.".into()); }
            open_path(u);
            return j(Ok(json!({"ok": true})));
        }
        "/api/browse" if m == "POST" => return j(Ok(json!({"path": dialogs::pick_folder(s(b, "start"))}))),
        "/api/optimization" => return j(optimization_info()),
        "/api/loader-versions" => return j(loader_versions(qs("loader"), qs("mc"))),
        "/api/pick-save" if m == "POST" => {
            let name = if s(b, "name").is_empty() { "modpack.mrpack" } else { s(b, "name") };
            return j(Ok(json!({"path": dialogs::pick_save(name, s(b, "start"))})));
        }
        "/api/reveal" if m == "POST" => {
            let f = PathBuf::from(s(b, "path"));
            if f.extension().map(|e| e.to_string_lossy().to_lowercase()) != Some("mrpack".into()) || !f.is_file() { return Err("That file isn't there any more.".into()); }
            if let Some(parent) = f.parent() { open_folder(parent); }
            return j(Ok(json!({"ok": true})));
        }
        "/api/filters" => return j(tag_filters(if q.contains_key("type") { qs("type") } else { "mod" })),
        "/api/versions" => return j(game_versions(qs("snapshots") == "1")),
        "/api/search" if q.contains_key("srv") && !qs("srv").is_empty() => return servers::discover_search(q).map(Out::Json),
        "/api/search" => {
            use_key();
            let t = if q.contains_key("type") { qs("type") } else { "mod" };
            if !mods::is_type(t) { return Err("bad content type".into()); }
            let view = qs("limit").parse::<usize>().unwrap_or(20).clamp(1, 50);
            let page = qs("page").parse::<usize>().unwrap_or(1).max(1);
            let cats: Vec<String> = qs("cats").split(',').filter(|c| re(r"^[a-z0-9-]+$").is_match(c)).map(String::from).collect();
            let mut res = mods::search_projects(if q.contains_key("source") { qs("source") } else { "modrinth" }, qs("q"), qs("mc"), qs("loader"), view, t, (page - 1) * view,
                if q.contains_key("sort") { qs("sort") } else { "relevance" }, &cats)?;
            let have: std::collections::HashSet<String> = if !qs("inst").is_empty() && t != "modpack" { store::load_inst(qs("inst"))?.mods().keys().cloned().collect() } else { Default::default() };
            let hide = qs("hide") == "1";
            let hits: Vec<Value> = res["hits"].as_array().cloned().unwrap_or_default().into_iter().map(|mut h| { h["installed"] = json!(have.contains(h["key"].as_str().unwrap_or(""))); h })
                .filter(|h| !(hide && h["installed"] == true)).collect();
            res["hits"] = json!(hits);
            return j(Ok(res));
        }
        "/api/instances" => {
            if m == "POST" && b["optimize"] == true { return j(create_instance_optimized(&r.task, s(b, "name"), s(b, "mc"), s(b, "loader"), s(b, "loader_version"))); }
            if m == "POST" { return j(create_instance(s(b, "name"), s(b, "mc"), s(b, "loader"), s(b, "loader_version"))); }
            return j(Ok(json!({"instances": list_instances()})));
        }
        _ => {}
    }
    if (p.len() >= 2 && p[0] == "api" && p[1] == "servers") || (p.len() == 2 && p[0] == "api" && ["server-eggs", "server-versions", "server-settings", "server-packs"].contains(&p[1].as_str())) {
        use_key();
        return servers::route(r, &p);
    }
    if p.len() >= 2 && p[0] == "api" && p[1] == "screenshots" {
        if p.len() == 2 { return j(Ok(json!({"shots": list_screenshots()}))); }
        let iid = &p[2];
        let inst = store::load_inst(iid)?;
        if p.len() == 4 && p[3] == "open" {
            let dirs = screenshot_dirs(&inst);
            open_folder(&dirs.first().cloned().unwrap_or_else(|| inst.root.join("screenshots")));
            return j(Ok(json!({"ok": true})));
        }
        let f = shot_file(iid, qs("f"))?;
        if m == "DELETE" { std::fs::remove_file(&f).map_err(|e| e.to_string())?; return j(Ok(json!({"ok": true}))); }
        return Ok(Out::Bytes(std::fs::read(&f).map_err(|e| e.to_string())?, shot_type(&f).unwrap_or("image/png")));
    }
    if p.len() >= 2 && p[0] == "api" && p[1] == "worlds" {
        if p.len() == 2 { return j(Ok(json!({"worlds": all_worlds()}))); }
        if p.len() == 5 && p[4] == "icon" {
            let saves = inst_path(&p[2])?.join("saves");
            let w = saves.join(&p[3]).canonicalize().map_err(|_| "No icon.".to_string())?;
            let ic = w.join("icon.png");
            if w.parent() == saves.canonicalize().ok().as_deref() && ic.is_file() { return Ok(Out::Bytes(std::fs::read(&ic).map_err(|e| e.to_string())?, "image/png")); }
        }
        return Err("No icon.".into());
    }
    if p.len() >= 3 && p[0] == "api" && p[1] == "instances" {
        let iid = &p[2];
        let act = p.get(3).map(|s| s.as_str()).unwrap_or("");
        if act.is_empty() && m == "GET" { return j(instance_detail(iid)); }
        if act.is_empty() && m == "DELETE" {
            store::load_inst(iid)?;
            if launcher::instance_busy(iid) { return Err("Close the game before deleting this instance.".into()); }
            store::playtime_archive(iid);                                       // keep its playtime in Settings > Playtime
            std::fs::remove_dir_all(inst_path(iid)?).map_err(|e| e.to_string())?;
            return j(Ok(json!({"ok": true})));
        }
        return instance_action(r, iid, act);
    }
    Err("not found".into())
}

fn tasks(r: &Req) -> Res {
    let f = store::data_home().join("tasks.json");
    if r.method == "POST" {
        let keep: Vec<Value> = r.body["tasks"].as_array().into_iter().flatten().filter(|t| t.is_object()).map(|t| {
            let mut m = Map::new();
            for k in ["id", "title", "icon_url", "sub", "status", "ts"] {
                let v = match &t[k] { Value::String(x) => x.clone(), Value::Null => String::new(), o => o.to_string() };
                m.insert(k.into(), json!(v.chars().take(200).collect::<String>()));
            }
            Value::Object(m)
        }).collect();
        let keep: Vec<Value> = keep.iter().rev().take(60).rev().cloned().collect();
        let _ = std::fs::create_dir_all(store::data_home());
        std::fs::write(&f, json!(keep).to_string()).map_err(|e| e.to_string())?;
        return Ok(json!({"tasks": keep}));
    }
    let t = std::fs::read_to_string(&f).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).unwrap_or(json!([]));
    Ok(json!({"tasks": t}))
}
