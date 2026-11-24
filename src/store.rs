//! Paths, settings.json and per-instance state. File formats match the Python version,
//! so an existing Orbit data folder works as it is.
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const STATE_NAME: &str = "modrinth-manager.json";
static LOCK: Mutex<()> = Mutex::new(());

pub fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn data_home() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(home_dir).join("Orbit")
    } else {
        home_dir().join(".local").join("share").join("Orbit")
    }
}

pub fn default_instances() -> PathBuf {
    data_home().join("instances")
}

pub fn default_mc_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(home_dir).join(".minecraft")
    } else if cfg!(target_os = "macos") {
        home_dir().join("Library/Application Support/minecraft")
    } else {
        home_dir().join(".minecraft")
    }
}

pub fn default_modrinth_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(home_dir).join("ModrinthApp").join("profiles")
    } else if cfg!(target_os = "macos") {
        home_dir().join("Library/Application Support/ModrinthApp/profiles")
    } else {
        home_dir().join(".local/share/ModrinthApp/profiles")
    }
}

pub fn default_curseforge_dir() -> PathBuf {
    home_dir().join("curseforge").join("minecraft")
}

pub fn settings() -> Map<String, Value> {
    std::fs::read_to_string(data_home().join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

pub fn save_settings(s: &Map<String, Value>) -> Result<(), String> {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = data_home();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("settings.json");
    std::fs::write(&path, Value::Object(s.clone()).to_string()).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)); // holds the CurseForge key
    }
    Ok(())
}

/// Settings > Launch > "Closing Orbit": whether closing Orbit with games / servers running shows a warning (default on)...
pub fn close_warning() -> bool { settings().get("close_warning").and_then(|v| v.as_bool()).unwrap_or(true) }
/// ...and how many seconds its Close button stays disabled (0 to 120, default 5).
pub fn close_timer() -> u64 { settings().get("close_timer").and_then(|v| v.as_u64()).unwrap_or(5).min(120) }

/// Settings > Appearance > Christmas snow: the tallest the snow pile on the floor may grow, in px (4 to 100, default 34)...
pub fn snow_height() -> u64 { settings().get("snow_height").and_then(|v| v.as_u64()).unwrap_or(34).clamp(4, 100) }
/// ...and how fast it falls, in percent of the normal speed (10 to 300, default 100).
pub fn snow_speed() -> u64 { settings().get("snow_speed").and_then(|v| v.as_u64()).unwrap_or(100).clamp(10, 1000) }

/// Settings > Java & memory keeps one Java per major version a Minecraft release can ask for.
pub const JAVA_SLOTS: [u32; 4] = [25, 21, 17, 8];

/// The slot a required Java version belongs to (Minecraft 1.17 asks for Java 16, which runs on the Java 17 slot).
pub fn java_slot(major: u32) -> u32 {
    match major { 0..=8 => 8, 9..=17 => 17, 18..=21 => 21, _ => 25 }
}

/// The Java chosen in Settings for the Java version `major`, or "" to let Orbit pick one.
pub fn java_choice(st: &Map<String, Value>, major: u32) -> String {
    st.get("java_paths").and_then(|v| v.get(java_slot(major).to_string())).and_then(|v| v.as_str()).unwrap_or("").trim().to_string()
}

/// Older Orbit versions kept one Java for every game in `java_path`; it now lives in the slot of its own version.
pub fn migrate_java() {
    let mut st = settings();
    let Some(old) = st.remove("java_path") else { return };
    let old = old.as_str().unwrap_or("").trim().to_string();
    let major = if old.is_empty() { 0 } else { crate::launcher::java_major(Path::new(&old)) };
    if major > 0 {
        let mut slots = st.get("java_paths").and_then(|v| v.as_object().cloned()).unwrap_or_default();
        slots.entry(java_slot(major).to_string()).or_insert(json!(old));
        st.insert("java_paths".into(), Value::Object(slots));
    }
    let _ = save_settings(&st);
}

/// Settings > Window: the size the game window opens with (854 x 480 until changed)...
pub const DEFAULT_WINDOW: (i64, i64) = (854, 480);
pub fn window_width(st: &Map<String, Value>) -> i64 { st.get("width").and_then(|v| v.as_i64()).filter(|n| (320..=16384).contains(n)).unwrap_or(DEFAULT_WINDOW.0) }
pub fn window_height(st: &Map<String, Value>) -> i64 { st.get("height").and_then(|v| v.as_i64()).filter(|n| (320..=16384).contains(n)).unwrap_or(DEFAULT_WINDOW.1) }

fn setting_path(key: &str, default: PathBuf) -> PathBuf {
    settings().get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or(default)
}

pub fn inst_dir() -> PathBuf { setting_path("instances_dir", default_instances()) }
pub fn tl_root() -> PathBuf { setting_path("tlauncher_dir", default_mc_dir()) }
pub fn mr_root() -> PathBuf { setting_path("modrinth_dir", default_modrinth_dir()) }
pub fn cfapp_root() -> PathBuf { setting_path("curseforge_dir", default_curseforge_dir()) }

// ---------- instances ----------

pub struct Instance {
    pub root: PathBuf,
    pub state: Value,
}

impl Instance {
    pub fn open(root: &Path) -> Instance {
        let state = std::fs::read_to_string(root.join(STATE_NAME))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| json!({"config": {}, "mods": {}}));
        Instance { root: root.to_path_buf(), state }
    }

    pub fn save(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
        let text = serde_json::to_string_pretty(&self.state).map_err(|e| e.to_string())?;
        std::fs::write(self.root.join(STATE_NAME), text).map_err(|e| e.to_string())
    }

    pub fn id(&self) -> String {
        self.root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }

    pub fn config(&self) -> &Value { &self.state["config"] }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn load_inst(id: &str) -> Result<Instance, String> {
    if !valid_id(id) {
        return Err("bad instance id".into());
    }
    let p = inst_dir().join(id);
    if !p.join(STATE_NAME).exists() {
        return Err("Instance not found".into());
    }
    Ok(Instance::open(&p))
}

pub fn instance_folders(base: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(base) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.join(STATE_NAME).is_file()).collect(),
        Err(_) => vec![],
    };
    v.sort();
    v
}

pub fn each_instance() -> Vec<Instance> {
    instance_folders(&inst_dir()).iter().map(|p| Instance::open(p)).collect()
}

pub fn summary(inst: &Instance) -> Value {
    let st = &inst.state;
    json!({
        "id": inst.id(),
        "path": inst.root.to_string_lossy(),
        "name": st.get("name").cloned().unwrap_or_else(|| json!(inst.id())),
        "mc": st["config"]["mc"], "loader": st["config"]["loader"],
        "loader_version": st["config"].get("loader_version").cloned().unwrap_or(json!("")),
        "mods": st["mods"].as_object().map(|m| m.len()).unwrap_or(0),
        "created": st.get("created").cloned().unwrap_or(json!(0)),
        "last_played": st.get("last_played").cloned().unwrap_or(json!(0)),
        "played": st.get("played_seconds").cloned().unwrap_or(json!(0)),
        "cfg": st.get("settings").cloned().unwrap_or(json!({})),
        "groups": st.get("groups").cloned().unwrap_or(json!([])),
        "icon": st.get("icon").cloned().unwrap_or(json!("")),
    })
}

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Shared libraries, assets, Java runtimes and version profiles.
pub fn meta_dir() -> PathBuf { data_home().join("meta") }

// ---------- playtime ----------
// Kept in <data>/playtime.json, apart from the instance folders, so the history survives deleting an instance.
//   {"entries": {"<iid>" | "<iid>~del<ts>": {"name","mc","loader","total","first","last","deleted","deleted_at","sessions":[{"id","start","secs","account"}]}}}
// A live instance is keyed by its id; deleting it moves its entry to "<iid>~del<ts>" so a new instance that reuses the id starts fresh.
// Two runs of one instance at once each count their own time, so an instance's total can exceed wall-clock time.
static PT_LOCK: Mutex<()> = Mutex::new(());
const PT_MAX_SESSIONS: usize = 300;

fn pt_path() -> PathBuf { data_home().join("playtime.json") }

fn pt_load() -> Value {
    std::fs::read_to_string(pt_path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).filter(|v| v["entries"].is_object()).unwrap_or_else(|| json!({"entries": {}}))
}

fn pt_save(v: &Value) {
    let dir = data_home();
    if std::fs::create_dir_all(&dir).is_err() { return; }
    let tmp = dir.join("playtime.json.tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() && std::fs::rename(&tmp, pt_path()).is_err() { let _ = std::fs::remove_file(&tmp); }
}

/// The entry for a live instance, created (seeded with the old per-instance counter) when it doesn't exist yet.
fn pt_entry<'a>(db: &'a mut Value, inst: &Instance) -> &'a mut Value {
    let id = inst.id();
    let e = &mut db["entries"][id.as_str()];
    if !e.is_object() {
        *e = json!({"total": inst.state.get("played_seconds").and_then(|v| v.as_i64()).unwrap_or(0), "sessions": [], "first": inst.state.get("created").cloned().unwrap_or(json!(0)), "last": inst.state.get("last_played").cloned().unwrap_or(json!(0))});
    }
    e["name"] = inst.state.get("name").cloned().unwrap_or_else(|| json!(id));
    e["mc"] = inst.state["config"]["mc"].clone();
    e["loader"] = inst.state["config"]["loader"].clone();
    e
}

/// Adds `secs` to one play session (`session` = its unique id, `start` = when it began) of an instance.
pub fn playtime_add(root: &Path, session: &str, start: f64, account: &str, secs: i64) {
    let _g = PT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let inst = Instance::open(root);
    let mut db = pt_load();
    let e = pt_entry(&mut db, &inst);
    e["total"] = json!(e["total"].as_i64().unwrap_or(0) + secs.max(0));
    e["last"] = json!(now());
    let list = e["sessions"].as_array_mut().unwrap();
    if let Some(s) = list.iter_mut().find(|s| s["id"] == session) { s["secs"] = json!(s["secs"].as_i64().unwrap_or(0) + secs.max(0)); }
    else {
        list.push(json!({"id": session, "start": start, "secs": secs.max(0), "account": account}));
        if list.len() > PT_MAX_SESSIONS { let extra = list.len() - PT_MAX_SESSIONS; list.drain(..extra); }
    }
    pt_save(&db);
}

/// Called right before an instance folder is deleted: archives its playtime so it stays in the history.
pub fn playtime_archive(id: &str) {
    let _g = PT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = inst_dir().join(id);
    if !root.join(STATE_NAME).is_file() { return; }
    let inst = Instance::open(&root);
    let mut db = pt_load();
    let mut e = pt_entry(&mut db, &inst).clone();
    if e["total"].as_i64().unwrap_or(0) == 0 { db["entries"].as_object_mut().map(|m| m.remove(id)); pt_save(&db); return; }   // never played: nothing to keep
    let ts = now() as u64;
    e["deleted"] = json!(true);
    e["deleted_at"] = json!(ts);
    db["entries"].as_object_mut().map(|m| { m.remove(id); m.insert(format!("{id}~del{ts}"), e); });
    pt_save(&db);
}

/// Everything for Settings > Playtime: per-instance totals (live and deleted) and the overall total.
pub fn playtime_report() -> Value {
    let _g = PT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let db = pt_load();
    let mut rows: Vec<Value> = vec![];
    let mut seen: std::collections::HashSet<String> = Default::default();
    for inst in each_instance() {
        let id = inst.id();
        seen.insert(id.clone());
        let e = db["entries"].get(id.as_str()).filter(|e| e.is_object());
        // never trust a total lower than the legacy per-instance counter (instances played before this file existed)
        let legacy = inst.state.get("played_seconds").and_then(|v| v.as_i64()).unwrap_or(0);
        let total = e.and_then(|e| e["total"].as_i64()).unwrap_or(0).max(legacy);
        rows.push(json!({"id": id, "name": inst.state.get("name").cloned().unwrap_or_else(|| json!(id)), "mc": inst.state["config"]["mc"], "loader": inst.state["config"]["loader"],
            "total": total, "last": inst.state.get("last_played").cloned().unwrap_or(json!(0)), "deleted": false,
            "sessions": e.map(|e| e["sessions"].clone()).unwrap_or(json!([]))}));
    }
    for (k, e) in db["entries"].as_object().into_iter().flatten() {
        if seen.contains(k) && e["deleted"] != true { continue; }
        // an entry whose instance folder vanished without going through Orbit (deleted by hand) counts as deleted too
        let gone = e["deleted"] == true || !seen.contains(k);
        rows.push(json!({"id": k, "name": e["name"], "mc": e["mc"], "loader": e["loader"], "total": e["total"].as_i64().unwrap_or(0), "last": e["last"],
            "deleted": gone, "deleted_at": e.get("deleted_at").cloned().unwrap_or(json!(null)), "sessions": e["sessions"]}));
    }
    rows.retain(|r| r["total"].as_i64().unwrap_or(0) > 0);
    rows.sort_by(|a, b| b["total"].as_i64().cmp(&a["total"].as_i64()));
    let total: i64 = rows.iter().map(|r| r["total"].as_i64().unwrap_or(0)).sum();
    json!({"total": total, "instances": rows})
}

#[cfg(test)]
mod playtime_tests {
    use super::*;

    #[test]
    fn playtime_sessions_total_and_archive() {
        let tmp = std::env::temp_dir().join(format!("orbit-pt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("HOME", &tmp); std::env::set_var("USERPROFILE", &tmp); std::env::set_var("APPDATA", &tmp);
        let root = inst_dir().join("demo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(STATE_NAME), r#"{"name":"Demo","config":{"mc":"1.20","loader":"fabric"},"mods":{},"played_seconds":100}"#).unwrap();
        playtime_add(&root, "s1", 1.0, "Bob", 0);       // started
        playtime_add(&root, "s2", 2.0, "Alice", 30);    // a second run at the same time
        playtime_add(&root, "s1", 1.0, "Bob", 60);
        let r = playtime_report();
        assert_eq!(r["total"], 190);                     // 100 seeded from the old counter + 30 + 60
        assert_eq!(r["instances"][0]["sessions"].as_array().unwrap().len(), 2);
        playtime_archive("demo");
        std::fs::remove_dir_all(&root).unwrap();
        let r = playtime_report();
        assert_eq!(r["total"], 190);
        assert_eq!(r["instances"][0]["deleted"], true);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
