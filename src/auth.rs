//! Microsoft / Mojang sign-in (device-code flow) and the player identity used at launch.
//!
//!   Microsoft device code -> access + refresh token
//!   -> Xbox Live (XBL) -> XSTS (rp://api.minecraftservices.com/) -> Minecraft services token
//!   -> /minecraft/profile (name + uuid; 404 = the account doesn't own Java Edition)
//!
//! Tokens live in <data>/auth.json (mode 0600), never in settings.json. The Azure application
//! (client) id comes from Settings (`ms_client_id`) or the ORBIT_MS_CLIENT_ID environment variable:
//! Microsoft requires every launcher to register its own and have it approved for Minecraft.
//! Without an active Microsoft account Orbit launches an offline player, as before.
use crate::store;
use crate::util::{agent, Res};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

const SCOPE: &str = "XboxLive.signin offline_access";
const DEVICE: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";

pub struct Identity { pub name: String, pub uuid: String, pub token: String, pub xuid: String, pub user_type: String }

static LOGIN: Mutex<Option<Value>> = Mutex::new(None);
static FILE_LOCK: Mutex<()> = Mutex::new(());
/// Bumped whenever a sign-in starts or is cancelled; a background poller whose number is stale stops quietly.
static GEN: AtomicU64 = AtomicU64::new(0);

fn auth_path() -> PathBuf { store::data_home().join("auth.json") }

fn load() -> Value {
    std::fs::read_to_string(auth_path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).filter(|v| v.is_object()).unwrap_or_else(|| json!({"accounts": []}))
}

fn save(v: &Value) -> Res<()> {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::create_dir_all(store::data_home()).map_err(|e| e.to_string())?;
    let p = auth_path();
    std::fs::write(&p, v.to_string()).map_err(|e| e.to_string())?;
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)); }
    Ok(())
}

pub fn client_id() -> String {
    store::settings().get("ms_client_id").and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        .or_else(|| std::env::var("ORBIT_MS_CLIENT_ID").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
        // a release can bake in its approved application id: ORBIT_MS_CLIENT_ID=... cargo build --release
        .or_else(|| option_env!("ORBIT_MS_CLIENT_ID").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
        .unwrap_or_default()
}

/// Accounts for the UI. Never includes tokens.
pub fn accounts() -> Vec<Value> {
    load()["accounts"].as_array().into_iter().flatten().map(|a| json!({"id": a["id"], "name": a["name"], "uuid": a["id"], "type": "microsoft", "skin": a["skin"]})).collect()
}

pub fn remove(id: &str) -> Res<()> {
    let mut a = load();
    if let Some(list) = a["accounts"].as_array_mut() { list.retain(|x| x["id"] != id); }
    save(&a)?;
    let mut s = store::settings();
    if s.get("ms_active").and_then(|v| v.as_str()) == Some(id) { s.insert("ms_active".into(), json!("")); store::save_settings(&s)?; }
    Ok(())
}

// ---------- HTTP helpers that keep the error body (Xbox explains itself in it) ----------

fn send(req: Result<ureq::Response, ureq::Error>) -> Res<(u16, Value)> {
    match req {
        Ok(r) => { let c = r.status(); Ok((c, r.into_json().unwrap_or(Value::Null))) }
        Err(ureq::Error::Status(c, r)) => Ok((c, r.into_json().unwrap_or(Value::Null))),
        Err(ureq::Error::Transport(t)) => Err(format!("Couldn't reach Microsoft's servers: {t}")),
    }
}

fn post_json(url: &str, body: &Value) -> Res<(u16, Value)> {
    send(agent(30).post(url).set("Accept", "application/json").send_json(body.clone()))
}

fn post_form(url: &str, form: &[(&str, &str)]) -> Res<(u16, Value)> { send(agent(30).post(url).send_form(form)) }

// ---------- Xbox / Minecraft chain ----------

fn xerr(code: i64) -> String {
    match code {
        2148916233 => "This Microsoft account has no Xbox profile yet. Sign in at xbox.com once to create one, then try again.".into(),
        2148916235 => "Xbox Live isn't available in your country or region.".into(),
        2148916236 | 2148916237 => "This account needs adult verification on Xbox before it can sign in.".into(),
        2148916238 => "This is a child account. Add it to a Microsoft family and allow online play, then try again.".into(),
        c => format!("Xbox refused the sign-in (error {c})."),
    }
}

/// Microsoft access token -> (minecraft token, expires_in secs, profile id, profile name).
fn minecraft_login(ms_access: &str) -> Res<(String, i64, String, String, String)> {
    let (c, xbl) = post_json("https://user.auth.xboxlive.com/user/authenticate", &json!({
        "Properties": {"AuthMethod": "RPS", "SiteName": "user.auth.xboxlive.com", "RpsTicket": format!("d={ms_access}")},
        "RelyingParty": "http://auth.xboxlive.com", "TokenType": "JWT"}))?;
    if c != 200 { return Err(format!("Xbox Live sign-in failed ({c}).")); }
    let (xbl_token, uhs) = (xbl["Token"].as_str().unwrap_or("").to_string(), xbl["DisplayClaims"]["xui"][0]["uhs"].as_str().unwrap_or("").to_string());
    let (c, xsts) = post_json("https://xsts.auth.xboxlive.com/xsts/authorize", &json!({
        "Properties": {"SandboxId": "RETAIL", "UserTokens": [xbl_token]}, "RelyingParty": "rp://api.minecraftservices.com/", "TokenType": "JWT"}))?;
    if c != 200 { return Err(xsts["XErr"].as_i64().map(xerr).unwrap_or_else(|| format!("Xbox authorization failed ({c})."))); }
    let xsts_token = xsts["Token"].as_str().unwrap_or("");
    let (c, mc) = post_json("https://api.minecraftservices.com/authentication/login_with_xbox", &json!({"identityToken": format!("XBL3.0 x={uhs};{xsts_token}")}))?;
    if c != 200 { return Err(format!("Minecraft services refused the sign-in ({c}). If this keeps happening, the app id may not be approved for Minecraft yet.")); }
    let token = mc["access_token"].as_str().unwrap_or("").to_string();
    let expires = mc["expires_in"].as_i64().unwrap_or(86400);
    let (c, prof) = send(agent(30).get("https://api.minecraftservices.com/minecraft/profile").set("Authorization", &format!("Bearer {token}")).call())?;
    if c == 404 { return Err("This Microsoft account doesn't own Minecraft: Java Edition.".into()); }
    if c != 200 { return Err(format!("Couldn't load the Minecraft profile ({c}).")); }
    let skin = prof["skins"].as_array().into_iter().flatten()
        .find(|k| k["state"] == "ACTIVE").or_else(|| prof["skins"].get(0))
        .and_then(|k| k["url"].as_str()).unwrap_or("").replace("http://", "https://");
    Ok((token, expires, prof["id"].as_str().unwrap_or("").to_string(), prof["name"].as_str().unwrap_or("").to_string(), skin))
}

fn store_account(ms: &Value, mc: (String, i64, String, String, String)) -> Res<String> {
    let (token, expires, id, name, skin) = mc;
    let mut a = load();
    if !a["accounts"].is_array() { a["accounts"] = json!([]); }
    let list = a["accounts"].as_array_mut().ok_or("The account file is damaged.")?;
    // a refresh response may omit a new refresh token; keep the old one rather than losing the sign-in
    let old = list.iter().find(|x| x["id"] == id).and_then(|x| x["refresh"].as_str()).map(String::from);
    let refresh = ms["refresh_token"].as_str().filter(|r| !r.is_empty()).map(String::from).or(old).unwrap_or_default();
    let entry = json!({"id": id, "name": name, "refresh": refresh, "mc_token": token, "mc_expires": store::now() + expires as f64, "skin": skin});
    match list.iter_mut().find(|x| x["id"] == entry["id"]) { Some(x) => *x = entry, None => list.push(entry) }
    save(&a)?;
    Ok(id)
}

// ---------- device-code login (runs in the background; the UI polls status()) ----------

fn set_login(v: Value) { *LOGIN.lock().unwrap_or_else(|e| e.into_inner()) = Some(v); }

pub fn status() -> Value { LOGIN.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| json!({"state": "idle"})) }

/// Start a sign-in. Returns {user_code, verification_uri, ...} for the UI to show.
pub fn begin() -> Res {
    let cid = client_id();
    if cid.is_empty() {
        return Err("Microsoft sign-in needs an Azure application (client) ID. Register a free one at portal.azure.com (a public client, 'Personal Microsoft accounts', device-code flow enabled), have it approved for Minecraft at aka.ms/AppRegInfo, then enter it in Settings > Accounts.".into());
    }
    if status()["state"] == "pending" { return Ok(status()); }
    let gen = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let (c, d) = post_form(DEVICE, &[("client_id", &cid), ("scope", SCOPE)])?;
    if c != 200 { return Err(d["error_description"].as_str().map(|s| s.lines().next().unwrap_or(s).to_string()).unwrap_or_else(|| format!("Microsoft refused the request ({c})."))); }
    let info = json!({"state": "pending", "user_code": d["user_code"], "verification_uri": d["verification_uri"], "message": d["message"], "expires_in": d["expires_in"]});
    set_login(info.clone());
    let device_code = d["device_code"].as_str().unwrap_or("").to_string();
    let interval = d["interval"].as_u64().unwrap_or(5).max(2);
    let expires = d["expires_in"].as_u64().unwrap_or(900);
    std::thread::spawn(move || {
        let live = || GEN.load(Ordering::SeqCst) == gen;
        let fail = |m: String| { if live() { set_login(json!({"state": "error", "error": m})); } };
        let start = std::time::Instant::now();
        let mut wait = interval;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(wait));
            if !live() { return; }
            if start.elapsed().as_secs() > expires { return fail("The sign-in code expired. Start again.".into()); }
            let r = post_form(TOKEN, &[("grant_type", "urn:ietf:params:oauth:grant-type:device_code"), ("client_id", &cid), ("device_code", &device_code)]);
            let (code, body) = match r { Ok(x) => x, Err(e) => return fail(e) };
            if !live() { return; }
            if code == 200 {
                let res = minecraft_login(body["access_token"].as_str().unwrap_or("")).and_then(|mc| { let name = mc.3.clone(); store_account(&body, mc).map(|id| (id, name)) });
                return match res {
                    Ok((id, name)) => {
                        if !live() { return; }
                        let mut s = store::settings();
                        s.insert("ms_active".into(), json!(id));
                        let _ = store::save_settings(&s);
                        set_login(json!({"state": "done", "name": name, "id": id}));
                    }
                    Err(e) => fail(e),
                };
            }
            match body["error"].as_str() {
                Some("authorization_pending") => {}
                Some("slow_down") => wait += 5,
                Some("authorization_declined") => return fail("Sign-in was declined.".into()),
                Some("expired_token") => return fail("The sign-in code expired. Start again.".into()),
                _ => return fail(body["error_description"].as_str().map(|s| s.lines().next().unwrap_or(s).to_string()).unwrap_or_else(|| "Microsoft sign-in failed.".into())),
            }
        }
    });
    Ok(info)
}

pub fn cancel() { GEN.fetch_add(1, Ordering::SeqCst); set_login(json!({"state": "idle"})); }

// ---------- identity at launch ----------

pub fn offline_identity(name: &str) -> Identity {
    let n = if name.is_empty() { "Player" } else { name };
    Identity { name: n.into(), uuid: crate::launcher::offline_uuid(n), token: "0".into(), xuid: "0".into(), user_type: "legacy".into() }
}

/// Who plays: the active Microsoft account (refreshed when needed) or the offline player.
pub fn identity(settings: &Map<String, Value>) -> Res<Identity> {
    let active = settings.get("ms_active").and_then(|v| v.as_str()).unwrap_or("");
    if active.is_empty() { return Ok(offline_identity(settings.get("username").and_then(|v| v.as_str()).unwrap_or(""))); }
    let mut a = load();
    let Some(acc) = a["accounts"].as_array().and_then(|l| l.iter().find(|x| x["id"] == active)).cloned() else {
        return Ok(offline_identity(settings.get("username").and_then(|v| v.as_str()).unwrap_or("")));
    };
    let valid = acc["mc_expires"].as_f64().unwrap_or(0.0) - 300.0 > store::now() && acc["mc_token"].as_str().map(|t| !t.is_empty()).unwrap_or(false);
    let (token, name, id) = if valid { (acc["mc_token"].as_str().unwrap().to_string(), acc["name"].as_str().unwrap_or("").to_string(), active.to_string()) } else {
        let expired = || format!("Your Microsoft sign-in for {} has expired. Sign in again in Settings > Accounts.", acc["name"].as_str().unwrap_or("this account"));
        let (c, ms) = post_form(TOKEN, &[("grant_type", "refresh_token"), ("client_id", &client_id()), ("refresh_token", acc["refresh"].as_str().unwrap_or("")), ("scope", SCOPE)])?;
        if c != 200 { return Err(expired()); }
        let mc = minecraft_login(ms["access_token"].as_str().unwrap_or(""))?;
        let (t, n) = (mc.0.clone(), mc.3.clone());
        let id = store_account(&ms, mc)?;
        a = load();
        (t, n, id)
    };
    let _ = a;
    Ok(Identity { name, uuid: id, token, xuid: "0".into(), user_type: "msa".into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xbox_errors_are_explained() {
        assert!(xerr(2148916233).contains("no Xbox profile"));
        assert!(xerr(2148916238).contains("child account"));
        assert!(xerr(1).contains("error 1"));
    }

    #[test]
    fn offline_identity_defaults() {
        let i = offline_identity("");
        assert_eq!(i.name, "Player");
        assert_eq!(i.user_type, "legacy");
        let i = offline_identity("Steve");
        assert_eq!(i.token, "0");
    }

    #[test]
    fn cancel_invalidates_a_running_sign_in() {
        let before = GEN.load(Ordering::SeqCst);
        cancel();
        assert!(GEN.load(Ordering::SeqCst) > before);
        assert_eq!(status()["state"], "idle");
    }
}
