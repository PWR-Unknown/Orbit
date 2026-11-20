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
