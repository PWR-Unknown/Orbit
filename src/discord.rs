//! Discord Rich Presence ("Playing <instance>" with an elapsed timer), like the Modrinth app.
//!
//! Talks to the Discord desktop app over its local IPC channel (a named pipe on Windows, a unix socket on
//! Linux / macOS). That is a local file, not a network port, so Orbit's loopback-only design is untouched.
//! Off by default (Settings > Discord). Needs a free application ID from https://discord.com/developers/applications:
//! its name is what Discord shows after "Playing", and an uploaded art asset named `orbit` is the big picture.
//! No extra crates: just std + serde_json. Every failure is silent (Discord closed, wrong ID, ...) and retried.
use crate::{launcher, servers, store};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Mutex;
use std::time::Duration;

static STATUS: Mutex<String> = Mutex::new(String::new());
fn set_status(s: &str) { *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = s.to_string(); }
/// What the poller is doing right now, shown on Settings > Discord.
pub fn status() -> String { STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone() }

/// Put your Discord application ID here (or set ORBIT_DISCORD_APP_ID). Empty = the feature stays idle.
const APP_ID: &str = "1576287384163586189";
/// Name of the art asset uploaded under Rich Presence > Art Assets in the developer portal.
const LARGE_IMAGE: &str = "orbit";

trait Conn: Read + Write + Send {}
impl<T: Read + Write + Send> Conn for T {}

fn app_id() -> Option<String> {
    let id = std::env::var("ORBIT_DISCORD_APP_ID").unwrap_or_else(|_| APP_ID.to_string());
    let id = id.trim().to_string();
    (id.len() >= 17 && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

/// True when an application ID is configured (the settings page says so when it isn't).
pub fn configured() -> bool { app_id().is_some() }

// ---------- IPC ----------

#[cfg(unix)]
fn connect() -> Option<Box<dyn Conn>> {
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    let mut bases: Vec<PathBuf> = Vec::new();
    for k in ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"] {
        if let Ok(v) = std::env::var(k) { if !v.is_empty() { bases.push(PathBuf::from(v)); } }
    }
    bases.push(PathBuf::from("/tmp"));
    for b in bases {
        // plain install, Flatpak and Snap keep the socket in different places
        for dir in [b.clone(), b.join("app/com.discordapp.Discord"), b.join("snap.discord")] {
            for n in 0..10 {
                if let Ok(s) = UnixStream::connect(dir.join(format!("discord-ipc-{n}"))) {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
                    return Some(Box::new(s));
                }
            }
        }
    }
    None
}

#[cfg(windows)]
fn connect() -> Option<Box<dyn Conn>> {
    for n in 0..10 {
        if let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(format!(r"\\.\pipe\discord-ipc-{n}")) {
            return Some(Box::new(f));
        }
    }
    None
}

#[cfg(not(any(unix, windows)))]
fn connect() -> Option<Box<dyn Conn>> { None }

/// One IPC frame: opcode (u32 LE), length (u32 LE), JSON.
fn frame(op: u32, v: &Value) -> Vec<u8> {
    let body = v.to_string().into_bytes();
    let mut buf = Vec::with_capacity(8 + body.len());
    buf.extend_from_slice(&op.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    buf
}

fn send(c: &mut dyn Conn, op: u32, v: &Value) -> std::io::Result<()> {
    c.write_all(&frame(op, v))?;
    c.flush()
}

fn recv(c: &mut dyn Conn) -> std::io::Result<(u32, Value)> {
    let mut h = [0u8; 8];
    c.read_exact(&mut h)?;
    let op = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
    let len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
    if len > 64 * 1024 { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large")); }
    let mut b = vec![0u8; len];
    c.read_exact(&mut b)?;
    Ok((op, serde_json::from_slice(&b).unwrap_or(Value::Null)))
}

fn open(id: &str) -> Result<Box<dyn Conn>, &'static str> {
    let mut c = connect().ok_or("Can't find the Discord app. Open the Discord desktop app (not the website) and run it and Orbit as the same user, both normally or both as administrator.")?;
    send(&mut *c, 0, &json!({"v": 1, "client_id": id})).map_err(|_| "Discord closed the connection.")?;
    let (op, _) = recv(&mut *c).map_err(|_| "Discord didn't answer. Restart Discord and try again.")?;
    if op == 1 { Ok(c) } else { Err("Discord refused the application ID. Check that APP_ID is the Application ID (not the client secret or a user ID).") }
}

fn set_activity(c: &mut dyn Conn, a: Option<&Want>, nonce: &mut u64) -> std::io::Result<()> {
    *nonce += 1;
    let activity = match a {
        None => Value::Null,
        Some(w) => {
            let mut act = json!({
                "details": w.details,
                "timestamps": {"start": w.start},
                "assets": {"large_image": LARGE_IMAGE, "large_text": "Orbit"},
            });
            if let Some(s) = &w.state { act["state"] = json!(s); }
            act
        }
    };
    send(c, 1, &json!({"cmd": "SET_ACTIVITY", "args": {"pid": std::process::id(), "activity": activity}, "nonce": nonce.to_string()}))?;
    recv(c).map(|_| ())
}

// ---------- what to show ----------

#[derive(Clone, PartialEq, Debug)]
struct Want { details: String, state: Option<String>, start: i64 }

fn cut(s: &str) -> String { s.chars().take(100).collect() }

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// The newest running game, else a running server, else nothing.
fn compute(hide_names: bool, seen: &mut HashMap<String, i64>) -> Option<Want> {
    let runs = launcher::running();
    let newest = runs.as_object().into_iter().flatten()
        .filter(|(_, v)| v["status"].as_str() == Some("running"))
        .max_by(|a, b| a.1["started"].as_f64().unwrap_or(0.0).total_cmp(&b.1["started"].as_f64().unwrap_or(0.0)));
    if let Some((_, v)) = newest {
        let name = v["name"].as_str().unwrap_or("Minecraft");
        let details = if hide_names { "Playing Minecraft".to_string() } else { cut(&format!("Playing {name}")) };
        let state = v["iid"].as_str().filter(|i| store::valid_id(i)).and_then(|iid| {
            let inst = store::Instance::open(&store::inst_dir().join(iid));
            let mc = inst.config()["mc"].as_str().unwrap_or("").to_string();
            let loader = inst.config()["loader"].as_str().unwrap_or("").to_string();
            match (mc.is_empty(), loader.is_empty() || loader == "vanilla") {
                (true, _) => None,
                (false, true) => Some(format!("Minecraft {mc}")),
                (false, false) => Some(format!("Minecraft {mc} · {}", cap(&loader))),
            }
        });
        // Discord shows one activity per app, so a server running at the same time goes on the second line
        let names = servers::running_names();
        let host = match names.len() {
            0 => None,
            _ if hide_names => Some("Hosting a server".to_string()),
            1 => Some(format!("Hosting {}", names[0])),
            n => Some(format!("Hosting {} servers", n)),
        };
        let state = match (state, host) {
            (Some(a), Some(b)) => Some(cut(&format!("{a} · {b}"))),
            (a, b) => a.or(b),
        };
        return Some(Want { details, state, start: v["started"].as_f64().unwrap_or_else(store::now) as i64 });
    }

    let names = servers::running_names();
    seen.retain(|k, _| names.contains(k));
    let first = names.first()?;
    let start = *seen.entry(first.clone()).or_insert_with(|| store::now() as i64);
    let details = if hide_names { "Hosting a Minecraft server".to_string() } else { cut(&format!("Hosting {first}")) };
    Some(Want { details, state: None, start })
}

// ---------- background thread ----------

/// Starts the poller. Cheap when the setting is off: it only reads settings.json every few seconds.
pub fn start() {
    std::thread::spawn(|| { let _ = std::panic::catch_unwind(run); });
}

fn flag(st: &Map<String, Value>, k: &str) -> bool { st.get(k).and_then(|v| v.as_bool()).unwrap_or(false) }

fn run() {
    let mut conn: Option<Box<dyn Conn>> = None;
    let mut conn_id = String::new();
    let mut shown: Option<Want> = None;
    let mut seen: HashMap<String, i64> = HashMap::new();
    let mut nonce = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let st = store::settings();
        let id = app_id();
        let on = flag(&st, "discord_presence");
        let want = match (&id, on) {
            (Some(_), true) => compute(flag(&st, "discord_hide_names"), &mut seen),
            _ => None,
        };
        set_status(if id.is_none() { "No Discord application ID is set in this build (see src/discord.rs)." }
            else if !on { "Off." }
            else if want.is_none() { "On. Waiting for a game or server to start." }
            else { "Connecting to Discord..." });
        let (Some(id), Some(want)) = (id, want) else {
            // switched off, or nothing is running: clear the status and let go of Discord
            if let Some(mut c) = conn.take() { let _ = set_activity(&mut *c, None, &mut nonce); }
            shown = None;
            continue;
        };
        if conn.is_none() || conn_id != id {
            match open(&id) {
                Ok(c) => conn = Some(c),
                Err(why) => { set_status(why); conn = None; }
            }
            conn_id = id;
            shown = None;
        }
        let Some(c) = conn.as_mut() else { continue };   // Discord isn't running: try again in 5 s
        if shown.as_ref() != Some(&want) {
            match set_activity(&mut **c, Some(&want), &mut nonce) {
                Ok(()) => { set_status(&format!("Connected. Showing: {}", want.details)); shown = Some(want); }
                Err(_) => { set_status("Lost the connection to Discord. Retrying."); conn = None; }
            }
        } else {
            set_status(&format!("Connected. Showing: {}", want.details));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_layout() {
        let f = frame(1, &json!({"a": 1}));
        assert_eq!(&f[0..4], &1u32.to_le_bytes());
        assert_eq!(&f[4..8], &(7u32).to_le_bytes());   // {"a":1} is 7 bytes
        assert_eq!(&f[8..], br#"{"a":1}"#);
    }

    #[test]
    fn app_id_must_be_a_snowflake() {
        std::env::set_var("ORBIT_DISCORD_APP_ID", "not-an-id");
        assert!(app_id().is_none());
        std::env::set_var("ORBIT_DISCORD_APP_ID", "123456789012345678");
        assert_eq!(app_id().as_deref(), Some("123456789012345678"));
        std::env::remove_var("ORBIT_DISCORD_APP_ID");
    }

    #[test]
    fn capitalises() { assert_eq!(cap("fabric"), "Fabric"); assert_eq!(cap(""), ""); }
}
