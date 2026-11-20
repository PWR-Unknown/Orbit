//! Modrinth + CurseForge API clients.
use crate::util::{agent, Res};
use serde_json::Value;
use std::sync::Mutex;

pub const API: &str = "https://api.modrinth.com/v2";
pub const CF_API: &str = "https://api.curseforge.com/v1";
pub const CF_GAME_ID: i64 = 432;
pub const MRPACK_HOSTS: [&str; 4] = ["cdn.modrinth.com", "github.com", "raw.githubusercontent.com", "gitlab.com"];

static CF_KEY: Mutex<String> = Mutex::new(String::new());
pub fn set_cf_key(k: &str) { if !k.trim().is_empty() { *CF_KEY.lock().unwrap_or_else(|e| e.into_inner()) = k.trim().to_string(); } }
pub fn clear_cf_key() { CF_KEY.lock().unwrap_or_else(|e| e.into_inner()).clear(); }
pub fn cf_key() -> String { CF_KEY.lock().unwrap_or_else(|e| e.into_inner()).clone() }

/// The Modrinth API root. Debug builds honour ORBIT_MODRINTH_API so the API layer can be tested against a local mock.
pub fn api_base() -> String {
    #[cfg(debug_assertions)]
    if let Ok(v) = std::env::var("ORBIT_MODRINTH_API") { if !v.is_empty() { return v.trim_end_matches('/').to_string(); } }
    API.to_string()
}

/// JSON-encode a list for a query parameter, e.g. loaders=["fabric"].
pub fn jl<S: AsRef<str>>(items: &[S]) -> String {
    Value::Array(items.iter().map(|s| Value::String(s.as_ref().to_string())).collect()).to_string()
}

fn pairs(params: &[(&str, String)]) -> Vec<(String, String)> {
    params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

/// Modrinth GET. 404 -> Null.
pub fn api_get(path: &str, params: &[(&str, String)]) -> Res {
    let mut req = agent(30).get(&format!("{}{path}", api_base()));
    for (k, v) in pairs(params) { req = req.query(&k, &v); }
    match req.call() {
        Ok(r) => r.into_json().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(404, _)) => Ok(Value::Null),
        Err(ureq::Error::Status(c, _)) => Err(format!("Modrinth API error {c}")),
        Err(ureq::Error::Transport(t)) => Err(format!("Network error: {t}")),
    }
}

pub fn api_post(path: &str, body: &Value) -> Res {
    match agent(60).post(&format!("{}{path}", api_base())).send_json(body) {
        Ok(r) => r.into_json().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(404, _)) => Ok(Value::Null),
        Err(ureq::Error::Status(c, _)) => Err(format!("Modrinth API error {c}")),
        Err(ureq::Error::Transport(t)) => Err(format!("Network error: {t}")),
    }
}

pub fn cf_get(path: &str, params: &[(&str, String)]) -> Res {
    let key = cf_key();
    if key.is_empty() {
        return Err("CurseForge needs an API key. Get one from console.curseforge.com and enter it in the app.".into());
    }
    let mut req = agent(30).get(&format!("{CF_API}{path}")).set("x-api-key", &key).set("Accept", "application/json");
    for (k, v) in pairs(params) { req = req.query(&k, &v); }
    match req.call() {
        Ok(r) => r.into_json().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(404, _)) => Ok(Value::Null),
        Err(ureq::Error::Status(c, _)) if c == 401 || c == 403 => Err("CurseForge rejected the API key. Check it and try again.".into()),
        Err(ureq::Error::Status(c, _)) => Err(format!("CurseForge API error {c}")),
        Err(ureq::Error::Transport(t)) => Err(format!("Network error: {t}")),
    }
}

pub fn host_allowed(url: &str) -> bool {
    url.starts_with("https://") && MRPACK_HOSTS.contains(&crate::util::host_of(url).as_str())
}
