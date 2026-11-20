//! Orbit's game launcher (port of launcher.py): installs what a version needs, then starts Minecraft.
//!   1. build a version profile (vanilla + loader)   2. download client, libraries, assets
//!   3. extract natives   4. find Java   5. build the command line and spawn the game
//! The player is either a Microsoft/Mojang account (see auth.rs) or an offline player.
use crate::auth::Identity;
use crate::store::{self, Instance};
use crate::tl;
use crate::util::{download_file, get_json, run_parallel, sha1_file, Res};
use md5::{Digest, Md5};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const OSN: &str = if cfg!(windows) { "windows" } else if cfg!(target_os = "macos") { "osx" } else { "linux" };
const ARCH64: bool = cfg!(target_pointer_width = "64");
const SEP: &str = if cfg!(windows) { ";" } else { ":" };
const LIB_URL: &str = "https://libraries.minecraft.net/";
const ASSET_URL: &str = "https://resources.download.minecraft.net/";
const RUNTIME_INDEX: &str = "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
const NATIVE_EXT: [&str; 4] = [".dll", ".so", ".dylib", ".jnilib"];

static RUN: Mutex<Option<HashMap<String, Value>>> = Mutex::new(None);
static PROCS: Mutex<Option<HashMap<String, Child>>> = Mutex::new(None);
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

fn with_run<R>(f: impl FnOnce(&mut HashMap<String, Value>) -> R) -> R {
    let mut g = RUN.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}
fn update(iid: &str, f: impl FnOnce(&mut Value)) { with_run(|m| if let Some(v) = m.get_mut(iid) { f(v); }); }

pub fn status(iid: &str) -> Value { with_run(|m| m.get(iid).cloned()).unwrap_or_else(|| json!({"status": "idle"})) }
pub fn running() -> Value {
    with_run(|m| Value::Object(m.iter().filter(|(_, s)| matches!(s["status"].as_str(), Some("installing") | Some("running"))).map(|(k, v)| (k.clone(), v.clone())).collect()))
}

// ---------- rules ----------

fn rule_match(rule: &Value, features: &Map<String, Value>) -> bool {
    if let Some(os) = rule["os"].as_object() {
        if let Some(n) = os.get("name").and_then(|n| n.as_str()) { if n != OSN { return false; } }
        if os.get("arch").and_then(|a| a.as_str()) == Some("x86") && ARCH64 { return false; }
    }
    for (k, v) in rule["features"].as_object().into_iter().flatten() {
        if features.get(k).and_then(|x| x.as_bool()).unwrap_or(false) != v.as_bool().unwrap_or(false) { return false; }
    }
    true
}

pub fn rules_ok(rules: &Value, features: &Map<String, Value>) -> bool {
    let Some(rs) = rules.as_array().filter(|r| !r.is_empty()) else { return true };
    let mut ok = false;
    for r in rs { if rule_match(r, features) { ok = r["action"] == "allow"; } }
    ok
}

// ---------- fetch ----------

fn fetch(url: &str, dest: &Path, sha1: Option<&str>, size: Option<u64>) -> Res<()> {
    if dest.is_file() {
        if let Some(s) = size { if std::fs::metadata(dest).map(|m| m.len()).ok() == Some(s) { return Ok(()); } }
        else if let Some(h) = sha1 { if sha1_file(dest).ok().as_deref() == Some(h) { return Ok(()); } }
        else { return Ok(()); }
    }
    if url.is_empty() { return Err(format!("{} is missing and has no download address.", dest.file_name().unwrap_or_default().to_string_lossy())); }
    let dir = dest.parent().unwrap_or(Path::new("."));
    let name = dest.file_name().unwrap().to_string_lossy().to_string();
    let mut err = String::new();
    for _ in 0..3 {
        match download_file(url, dir, &name, sha1.map(|_| "sha1"), sha1) { Ok(_) => return Ok(()), Err(e) => { err = e; std::thread::sleep(Duration::from_secs(1)); } }
    }
    Err(err)
}

// ---------- profile ----------

pub fn profile_key(inst: &Instance) -> String {
    let c = &inst.state["config"];
    let lv = c["loader_version"].as_str().unwrap_or("");
    format!("{}-{}{}", c["loader"].as_str().unwrap_or(""), c["mc"].as_str().unwrap_or(""), if lv.is_empty() { String::new() } else { format!("-{lv}") })
}

pub fn profile_path(meta: &Path, inst: &Instance) -> PathBuf { meta.join("quarry-profiles").join(format!("{}.json", profile_key(inst))) }

/// The merged vanilla+loader profile for this instance, cached under meta/.
pub fn profile_for(inst: &Instance, meta: &Path) -> Res {
    let path = profile_path(meta, inst);
    if let Ok(t) = std::fs::read_to_string(&path) { if let Ok(v) = serde_json::from_str(&t) { return Ok(v); } }
    let (mc, loader) = (inst.mc(), inst.loader());
    let lv = inst.state["config"]["loader_version"].as_str().filter(|s| !s.is_empty());
    let key = profile_key(inst);
    let vanilla = tl::vanilla_profile(&mc)?;
    let merged = if loader == "vanilla" { let mut v = vanilla.clone(); v["id"] = json!(key); v } else {
        let prof = if tl::meta_url(&loader).is_some() { tl::meta_profile(&loader, &mc, lv)?.0 } else {
            std::fs::create_dir_all(meta).map_err(|e| e.to_string())?;
            tl::installer_profile(&loader, &mc, meta, lv)?.0
        };
        tl::merge(&vanilla, &prof, &key)?
    };
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&path, serde_json::to_string_pretty(&merged).unwrap()).map_err(|e| e.to_string())?;
    Ok(merged)
}

// ---------- libraries ----------

struct LibFile { path: String, url: String, sha1: Option<String>, size: Option<u64>, native: bool }

fn lib_files(lib: &Value) -> Vec<LibFile> {
    let mut out = vec![];
    let name = lib["name"].as_str().unwrap_or("");
    if let Some(d) = lib["downloads"].as_object() {
        if let Some(a) = d.get("artifact").filter(|a| a.is_object()) {
            out.push(LibFile { path: a["path"].as_str().unwrap_or("").into(), url: a["url"].as_str().unwrap_or("").into(), sha1: a["sha1"].as_str().map(String::from),
                size: a["size"].as_u64(), native: name.contains(":natives-") });
        }
        if let Some(cls) = lib["natives"][OSN].as_str() {
            let key = cls.replace("${arch}", if ARCH64 { "64" } else { "32" });
            if let Some(c) = d.get("classifiers").and_then(|c| c.get(&key)).filter(|c| c.is_object()) {
                out.push(LibFile { path: c["path"].as_str().unwrap_or("").into(), url: c["url"].as_str().unwrap_or("").into(), sha1: c["sha1"].as_str().map(String::from), size: c["size"].as_u64(), native: true });
            }
        }
    } else {
        let p: Vec<&str> = name.split(':').collect();
        if p.len() >= 3 {
            let (g, a, v) = (p[0], p[1], p[2]);
            let path = format!("{}/{a}/{v}/{a}-{v}{}.jar", g.replace('.', "/"), p.get(3).map(|c| format!("-{c}")).unwrap_or_default());
            let base = lib["url"].as_str().unwrap_or(LIB_URL).trim_end_matches('/').to_string();
            out.push(LibFile { url: format!("{base}/{path}"), path, sha1: lib["sha1"].as_str().map(String::from), size: None, native: false });
        }
    }
    out
}

fn extract_natives(jars: &[PathBuf], ndir: &Path, excludes: &[String]) -> Res<()> {
    std::fs::create_dir_all(ndir).map_err(|e| e.to_string())?;
    for jar in jars {
        let f = std::fs::File::open(jar).map_err(|e| e.to_string())?;
        let mut z = zip::ZipArchive::new(f).map_err(|e| format!("{}: {e}", jar.display()))?;
        for i in 0..z.len() {
            let mut e = z.by_index(i).map_err(|e| e.to_string())?;
            let n = e.name().to_string();
            if e.is_dir() || n.starts_with("META-INF") || excludes.iter().any(|x| n.starts_with(x.as_str())) || !NATIVE_EXT.iter().any(|x| n.to_lowercase().ends_with(x)) { continue; }
            let dest = ndir.join(Path::new(&n).file_name().unwrap_or_default());
            if !dest.exists() {
                let mut out = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
                std::io::copy(&mut e, &mut out).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

// ---------- install ----------

pub struct Plan { classpath: Vec<PathBuf>, natives: PathBuf, game_assets: PathBuf, log_arg: Option<String>, assets_root: PathBuf }

fn install(profile: &Value, meta: &Path, inst_root: &Path, prog: &dyn Fn(&str, f64)) -> Res<Plan> {
    let vid = profile["id"].as_str().unwrap_or("").to_string();
    let vdir = meta.join("versions").join(&vid);
    let client = vdir.join(format!("{vid}.jar"));
    prog("Downloading Minecraft", 0.02);
    let c = &profile["downloads"]["client"];
    fetch(c["url"].as_str().unwrap_or(""), &client, c["sha1"].as_str(), c["size"].as_u64())?;

    let (mut libs, mut classpath, mut native_jars, mut excludes): (Vec<(String, PathBuf, Option<String>, Option<u64>)>, Vec<PathBuf>, Vec<PathBuf>, Vec<String>) = (vec![], vec![], vec![], vec![]);
    let none = Map::new();
    for lib in profile["libraries"].as_array().into_iter().flatten() {
        if !rules_ok(&lib["rules"], &none) { continue; }
        for lf in lib_files(lib) {
            let dest = meta.join("libraries").join(&lf.path);
            libs.push((lf.url.clone(), dest.clone(), lf.sha1.clone(), lf.size));
            if lf.native {
                native_jars.push(dest.clone());
                excludes.extend(lib["extract"]["exclude"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)));
            }
            if !(lib["natives"].is_object() && lf.native) { classpath.push(dest); }
        }
    }
    run_parallel(&libs, 16, |(u, d, s, z)| fetch(u, d, s.as_deref(), *z), |n, t| prog(&format!("Downloading libraries ({n}/{t})"), 0.05 + 0.25 * n as f64 / t.max(1) as f64))?;

    let ai = &profile["assetIndex"];
    let idx_path = meta.join("assets").join("indexes").join(format!("{}.json", ai["id"].as_str().unwrap_or("")));
    fetch(ai["url"].as_str().unwrap_or(""), &idx_path, ai["sha1"].as_str(), ai["size"].as_u64())?;
    let index: Value = serde_json::from_str(&std::fs::read_to_string(&idx_path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let objects = index["objects"].as_object().cloned().unwrap_or_default();
    let mut todo: HashMap<String, u64> = HashMap::new();
    for o in objects.values() { todo.insert(o["hash"].as_str().unwrap_or("").into(), o["size"].as_u64().unwrap_or(0)); }
    let obj_dir = meta.join("assets").join("objects");
    let todo: Vec<(String, u64)> = todo.into_iter().collect();
    run_parallel(&todo, 16, |(h, s)| fetch(&format!("{ASSET_URL}{}/{h}", &h[..2]), &obj_dir.join(&h[..2]).join(h), Some(h), Some(*s)),
        |n, t| prog(&format!("Downloading assets ({n}/{t})"), 0.3 + 0.6 * n as f64 / t.max(1) as f64))?;

    let mut game_assets = meta.join("assets");
    if index["virtual"] == true || index["map_to_resources"] == true {      // Minecraft 1.6 and older
        let target = if index["map_to_resources"] == true { inst_root.join("resources") } else { meta.join("assets").join("virtual").join("legacy") };
        for (name, o) in &objects {
            let dest = target.join(name);
            if !dest.exists() {
                let h = o["hash"].as_str().unwrap_or("");
                let _ = std::fs::create_dir_all(dest.parent().unwrap());
                std::fs::copy(obj_dir.join(&h[..2]).join(h), &dest).map_err(|e| e.to_string())?;
            }
        }
        game_assets = target;
    }

    let mut log_arg = None;
    let lc = &profile["logging"]["client"];
    if let Some(lf) = lc["file"].as_object().filter(|f| !f.is_empty()) {
        let lpath = meta.join("assets").join("log_configs").join(lf["id"].as_str().unwrap_or("log.xml"));
        fetch(lf["url"].as_str().unwrap_or(""), &lpath, lf["sha1"].as_str(), lf["size"].as_u64())?;
        log_arg = lc["argument"].as_str().map(|a| a.replace("${path}", &lpath.to_string_lossy()));
    }
    prog("Preparing natives", 0.92);
    let ndir = vdir.join("natives");
    extract_natives(&native_jars, &ndir, &excludes)?;
    classpath.push(client);
    Ok(Plan { classpath, natives: ndir, game_assets, log_arg, assets_root: meta.join("assets") })
}

// ---------- Java ----------

fn runtime_platform() -> &'static str {
    let arm = std::env::consts::ARCH.contains("arm") || std::env::consts::ARCH.contains("aarch");
    match OSN {
        "windows" => if arm { "windows-arm64" } else if ARCH64 { "windows-x64" } else { "windows-x86" },
        "osx" => if arm { "mac-os-arm64" } else { "mac-os" },
        _ => if ARCH64 { "linux" } else { "linux-i386" },
    }
}

fn runtime_exe(base: &Path) -> PathBuf {
    match OSN { "windows" => base.join("bin").join("javaw.exe"), "osx" => base.join("jre.bundle/Contents/Home/bin/java"), _ => base.join("bin").join("java") }
}

fn ensure_runtime(component: &str, meta: &Path, prog: &dyn Fn(&str, f64)) -> Res<PathBuf> {
    let base = meta.join("runtime").join(component);
    let (ok, exe) = (base.join(".quarry-ok"), runtime_exe(&base));
    if ok.exists() && exe.exists() { return Ok(exe); }
    let idx = get_json(RUNTIME_INDEX)?;
    let entry = idx[runtime_platform()][component][0]["manifest"]["url"].as_str().ok_or_else(|| format!("Mojang has no Java runtime '{component}' for this system."))?.to_string();
    let files = get_json(&entry)?["files"].as_object().cloned().unwrap_or_default();
    std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let mut jobs: Vec<(Value, PathBuf, bool)> = vec![];
    for (rel, f) in &files {
        let dest = crate::util::safe_join(&base, rel).map_err(|_| "Java runtime listing tried to write outside its folder.".to_string())?;
        match f["type"].as_str() {
            Some("directory") => { std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?; }
            Some("file") => jobs.push((f["downloads"]["raw"].clone(), dest, f["executable"] == true)),
            _ => {}
        }
    }
    run_parallel(&jobs, 8, |(raw, dest, x)| {
        fetch(raw["url"].as_str().unwrap_or(""), dest, raw["sha1"].as_str(), raw["size"].as_u64())?;
        #[cfg(unix)] if *x { use std::os::unix::fs::PermissionsExt; if let Ok(m) = std::fs::metadata(dest) { let _ = std::fs::set_permissions(dest, std::fs::Permissions::from_mode(m.permissions().mode() | 0o111)); } }
        let _ = x; Ok(())
    }, |n, t| prog(&format!("Downloading Java ({n}/{t})"), 0.05 + 0.85 * n as f64 / t.max(1) as f64))?;
    std::fs::write(&ok, "1").map_err(|e| e.to_string())?;
    Ok(exe)
}

pub fn java_major(exe: &Path) -> u32 {
    let mut c = Command::new(exe);
    c.arg("-version");
    #[cfg(windows)] { use std::os::windows::process::CommandExt; c.creation_flags(0x08000000); }
    let Ok(o) = c.output() else { return 0 };
    let t = format!("{}{}", String::from_utf8_lossy(&o.stderr), String::from_utf8_lossy(&o.stdout));
    let Some(m) = Regex::new(r#"version "(\d+)(?:\.(\d+))?"#).unwrap().captures(&t) else { return 0 };
    let a: u32 = m[1].parse().unwrap_or(0);
    if a == 1 { m.get(2).and_then(|x| x.as_str().parse().ok()).unwrap_or(0) } else { a }
}

/// What a version profile asks of Java: (runtime component, major version). Minecraft 1.12 and older ship no
/// `javaVersion` at all; they need Java 8 (Forge 1.12 breaks on Java 9+), which Mojang serves as "jre-legacy".
pub fn java_need(profile: &Value) -> (String, u32) {
    let jv = &profile["javaVersion"];
    match (jv["component"].as_str(), jv["majorVersion"].as_u64()) {
        (Some(c), m) => (c.to_string(), m.unwrap_or(8) as u32),
        (None, Some(m)) => ("".to_string(), m as u32),
        _ => ("jre-legacy".to_string(), 8),
    }
}

pub fn find_java(profile: &Value, settings: &Map<String, Value>, meta: &Path, prog: &dyn Fn(&str, f64)) -> Res<PathBuf> {
    let (comp, want) = java_need(profile);
    let custom = settings.get("java_path").and_then(|v| v.as_str()).unwrap_or("").trim();
    if !custom.is_empty() {
        if !Path::new(custom).is_file() { return Err(format!("The Java you chose in Settings doesn't exist: {custom}")); }
        return Ok(PathBuf::from(custom));
    }
    let mut err = None;
    if !comp.is_empty() {
        match ensure_runtime(&comp, meta, prog) { Ok(p) => return Ok(p), Err(e) => err = Some(e) }
    }
    // Java 8 era (1.16.5 and older) can't run on a newer system Java, so only an exact match is acceptable there.
    if let Some(sj) = tl::find_java_on_path() { let m = java_major(&sj); if if want <= 8 { m == 8 } else { m >= want } { return Ok(sj); } }
    Err(format!("Java {want} is needed and couldn't be downloaded{}. Install it or choose one in Settings > Launch.", err.map(|e| format!(" ({e})")).unwrap_or_default()))
}

// ---------- command ----------

fn expand(args: &Value, features: &Map<String, Value>) -> Vec<String> {
    let mut out = vec![];
    for a in args.as_array().into_iter().flatten() {
        if let Some(s) = a.as_str() { out.push(s.to_string()); }
        else if rules_ok(&a["rules"], features) {
            let v = if a.get("value").is_some() { &a["value"] } else { &a["values"] };
            match v { Value::String(s) => out.push(s.clone()), Value::Array(x) => out.extend(x.iter().filter_map(|s| s.as_str().map(String::from))), _ => {} }
        }
    }
    out
}

pub fn offline_uuid(name: &str) -> String {
    let mut h: [u8; 16] = Md5::digest(format!("OfflinePlayer:{name}").as_bytes()).into();
    h[6] = (h[6] & 0x0f) | 0x30; h[8] = (h[8] & 0x3f) | 0x80;
    let x = hex::encode(h);
    format!("{}-{}-{}-{}-{}", &x[..8], &x[8..12], &x[12..16], &x[16..20], &x[20..])
}

/// Minimal shlex.split: whitespace-separated, with '…' and "…" quoting and backslash escapes.
pub fn shlex_split(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote, mut has) = (vec![], String::new(), None::<char>, false);
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => { if let Some(n) = it.next() { if n != '"' && n != '\\' { cur.push('\\'); } cur.push(n); } }
            (Some(_), c) => cur.push(c),
            (None, '\'') | (None, '"') => { quote = Some(c); has = true; }
            (None, '\\') => { if let Some(n) = it.next() { cur.push(n); has = true; } }
            (None, c) if c.is_whitespace() => { if has || !cur.is_empty() { out.push(std::mem::take(&mut cur)); has = false; } }
            (None, c) => cur.push(c),
        }
    }
    if has || !cur.is_empty() { out.push(cur); }
    out
}

fn build_command(profile: &Value, plan: &Plan, java: &Path, inst_root: &Path, who: &Identity, memory_mb: i64, extra_jvm: &str, quick: Option<(&str, &str)>) -> Vec<String> {
    let none = Map::new();
    let (jvm, game) = if profile.get("arguments").is_some() {
        (expand(&profile["arguments"]["jvm"], &none), expand(&profile["arguments"]["game"], &none))
    } else {
        (vec!["-Djava.library.path=${natives_directory}".into(), "-cp".into(), "${classpath}".into()],
         profile["minecraftArguments"].as_str().unwrap_or("").split_whitespace().map(String::from).collect())
    };
    let lib_dir = plan.assets_root.parent().unwrap().join("libraries");
    let cp = plan.classpath.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join(SEP);
    let mut v: HashMap<&str, String> = HashMap::new();
    for (k, val) in [("auth_player_name", who.name.clone()), ("auth_uuid", who.uuid.clone()), ("auth_access_token", who.token.clone()), ("auth_session", who.token.clone()),
        ("auth_xuid", who.xuid.clone()), ("clientid", "0".into()), ("user_type", who.user_type.clone()), ("user_properties", "{}".into()),
        ("version_name", profile["id"].as_str().unwrap_or("").into()), ("version_type", "Orbit".into()), ("game_directory", inst_root.to_string_lossy().into()),
        ("assets_root", plan.assets_root.to_string_lossy().into()), ("game_assets", plan.game_assets.to_string_lossy().into()),
        ("assets_index_name", profile["assetIndex"]["id"].as_str().unwrap_or("").into()), ("natives_directory", plan.natives.to_string_lossy().into()),
        ("launcher_name", "Orbit".into()), ("launcher_version", "1.0".into()), ("library_directory", lib_dir.to_string_lossy().into()),
        ("classpath_separator", SEP.into()), ("classpath", cp)] { v.insert(k, val); }
    let re = Regex::new(r"\$\{(\w+)\}").unwrap();
    let sub = |s: &str| re.replace_all(s, |c: &regex::Captures| v.get(&c[1]).cloned().unwrap_or_else(|| c[0].to_string())).into_owned();
    let mut cmd = vec![java.to_string_lossy().into_owned(), format!("-Xmx{memory_mb}M"), "-Xms256M".into()];
    cmd.extend(shlex_split(extra_jvm));
    if let Some(l) = &plan.log_arg { cmd.push(l.clone()); }
    cmd.extend(jvm.iter().map(|a| sub(a)));
    cmd.push(profile["mainClass"].as_str().unwrap_or("").into());
    cmd.extend(game.iter().map(|a| sub(a)));
    if let Some((kind, val)) = quick {
        if profile["arguments"].to_string().contains("quickPlay") {
            match kind { "singleplayer" => cmd.extend(["--quickPlaySingleplayer".into(), val.into()]), "multiplayer" => cmd.extend(["--quickPlayMultiplayer".into(), val.into()]), _ => {} }
        }
    }
    cmd
}

// ---------- running ----------

fn is_active(v: &Value) -> bool { matches!(v["status"].as_str(), Some("installing") | Some("running")) }

/// True while any run (installing or playing) of this instance is live, whichever account it plays as.
pub fn instance_busy(iid: &str) -> bool {
    with_run(|m| m.values().any(|s| is_active(s) && s["iid"].as_str() == Some(iid)))
}

/// Which account a run plays as: (display name, key "ms:<id>" / "off:<name>"). `settings` already holds any override.
fn account_of(settings: &Map<String, Value>) -> (String, String) {
    let active = settings.get("ms_active").and_then(|v| v.as_str()).unwrap_or("");
    if !active.is_empty() {
        if let Some(a) = crate::auth::accounts().into_iter().find(|a| a["id"] == active) { return (a["name"].as_str().unwrap_or("").to_string(), format!("ms:{active}")); }
    }
    let n = settings.get("username").and_then(|v| v.as_str()).filter(|n| !n.is_empty()).unwrap_or("Player").to_string();
    (n.clone(), format!("off:{n}"))
}

/// Starts the game. The first run of an instance is keyed by the instance id. While it is still going, `extra` = true
/// starts one more run of the same instance (key "<iid>@2", "<iid>@3", ...) under `account` = ("ms" | "offline", id | name).
pub fn start(inst_root: &Path, iid: &str, name: &str, mut settings: Map<String, Value>, meta: PathBuf, quick: Option<(String, String)>, account: Option<(String, String)>, extra: bool) -> Res {
    if let Some((kind, val)) = &account {
        match kind.as_str() {
            "ms" => { if !crate::auth::accounts().iter().any(|a| a["id"] == val.as_str()) { return Err("That Microsoft account isn't signed in.".into()); } settings.insert("ms_active".into(), json!(val)); }
            "offline" => { settings.insert("ms_active".into(), json!("")); settings.insert("username".into(), json!(val)); }
            _ => return Err("Unknown account type.".into()),
        }
    }
    let (label, akey) = account_of(&settings);
    let mut st = json!({"status": "installing", "message": "Starting...", "progress": 0.0, "name": name, "error": "", "started": store::now(), "iid": iid, "account": label, "acct": akey});
    let key = with_run(|m| {
        let key = if !m.get(iid).map(is_active).unwrap_or(false) { iid.to_string() }
            else if extra {
                if m.values().any(|s| is_active(s) && s["iid"].as_str() == Some(iid) && s["acct"] == st["acct"]) { return Err(format!("{name} is already running as {label}.")); }
                match (2..).map(|n| format!("{iid}@{n}")).find(|k| !m.get(k).map(is_active).unwrap_or(false)) { Some(k) => k, None => return Err("Too many runs.".into()) }
            } else { return Err(format!("{name} is already running.")) };
        st["run"] = json!(key);
        m.insert(key.clone(), st.clone());
        Ok(key)
    })?;
    let (root, iid2, key2) = (inst_root.to_path_buf(), iid.to_string(), key.clone());
    std::thread::spawn(move || {
        if let Err(e) = run(&key2, &iid2, &root, &settings, &meta, quick) { update(&key2, |s| { s["status"] = json!("error"); s["error"] = json!(e); }); }
        PROCS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).remove(&key2);
    });
    Ok(st)
}

pub fn stop(iid: &str) -> Res<()> {
    let mut g = PROCS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(child) = g.get_or_insert_with(HashMap::new).get_mut(iid) else { return Err("That instance isn't running.".into()) };
    #[cfg(unix)] { let _ = Command::new("kill").arg("-TERM").arg(child.id().to_string()).status(); }
    #[cfg(not(unix))] { let _ = child.kill(); }
    drop(g);
    for _ in 0..16 {
        std::thread::sleep(Duration::from_millis(500));
        if PROCS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get(iid).is_none() { return Ok(()); }
    }
    if let Some(c) = PROCS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get_mut(iid) { let _ = c.kill(); }
    Ok(())
}

/// Closes every game Orbit started (Orbit itself is closing). Asks first so Minecraft can save its world: SIGTERM on
/// Linux / macOS, a normal close-window request on Windows. Whatever is still open after 20 s is killed.
pub fn stop_all() {
    let ids: Vec<String> = PROCS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).keys().cloned().collect();
    let hs: Vec<_> = ids.into_iter().map(|id| std::thread::spawn(move || close_game(&id))).collect();
    for h in hs { let _ = h.join(); }
}

fn close_game(iid: &str) {
    let pid = match PROCS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get(iid) { Some(c) => c.id(), None => return };
    #[cfg(unix)] { let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).status(); }
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill").arg("/PID").arg(pid.to_string()).creation_flags(0x08000000).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(500));
        if PROCS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get(iid).is_none() { return; }
    }
    if let Some(c) = PROCS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get_mut(iid) { let _ = c.kill(); }
}

/// Credits `secs` of play to the instance: its own counters (last played, total) and the persistent playtime history.
fn add_play(root: &Path, session: &str, start: f64, account: &str, secs: i64) {
    let mut inst = Instance::open(root);
    inst.state["last_played"] = json!(store::now());
    inst.state["played_seconds"] = json!(inst.state["played_seconds"].as_i64().unwrap_or(0) + secs);
    let _ = inst.save();
    store::playtime_add(root, session, start, account, secs);
}

/// Before the game starts: fetch what the installed mods need and don't have. Best effort, never blocks a launch.
fn auto_deps(iid: &str, root: &Path, prog: &dyn Fn(&str, f64)) {
    let mut inst = Instance::open(root);
    if inst.loader() == "vanilla" || inst.cfg().is_err() { return; }
    prog("Checking mod dependencies", 0.005);
    let mut out = None;
    let (log, _) = crate::api::run_job(&None, |log| { out = crate::mods::ensure_deps(&mut inst, log); Ok(()) });
    let Some(o) = out else { return };
    let mut lines: Vec<String> = log.into_iter().filter(|l| l.starts_with('+') || l.starts_with('!')).collect();
    lines.extend(crate::deps::describe(&o));
    if lines.is_empty() { return; }
    if !o.installed.is_empty() { prog(&format!("Installed missing dependencies: {}", o.installed.join(", ")), 0.01); }
    update(iid, |s| s["deps"] = o.to_json());
    let _ = std::fs::create_dir_all(root.join("logs"));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(root.join("logs").join("orbit-deps.log")) {
        use std::io::Write;
        for l in lines { let _ = writeln!(f, "[{}] {l}", store::now() as u64); }
    }
}

fn run(iid: &str, real_iid: &str, inst_root: &Path, settings: &Map<String, Value>, meta: &Path, quick: Option<(String, String)>) -> Res<()> {
    let prog = |msg: &str, frac: f64| { let m = msg.to_string(); update(iid, |s| { s["message"] = json!(m); s["progress"] = json!((frac * 1000.0).round() / 1000.0); }); };
    if iid == real_iid && crate::servers::auto_deps() { auto_deps(iid, inst_root, &prog); }   // an extra run starts beside a game that is already set up
    let inst = Instance::open(inst_root);
    let icfg = inst.state["settings"].as_object().cloned().unwrap_or_default();
    let who = crate::auth::identity(settings)?;
    let (profile, plan, java) = {
        let _g = INSTALL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        prog("Preparing the game profile", 0.01);
        let profile = profile_for(&inst, meta)?;
        let plan = install(&profile, meta, inst_root, &prog)?;
        prog("Finding Java", 0.95);
        let mut js = settings.clone();
        js.insert("java_path".into(), json!(icfg.get("java_path").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).or(settings.get("java_path").and_then(|v| v.as_str())).unwrap_or("")));
        let java = find_java(&profile, &js, meta, &prog)?;
        (profile, plan, java)
    };
    let mem = icfg.get("memory").and_then(|v| v.as_i64()).filter(|m| *m > 0).or(settings.get("memory").and_then(|v| v.as_i64()).filter(|m| *m > 0)).unwrap_or(4096);
    let extra = [settings.get("jvm_args"), icfg.get("jvm_args")].iter().filter_map(|x| x.and_then(|v| v.as_str())).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
    let mut cmd = build_command(&profile, &plan, &java, inst_root, &who, mem, &extra, quick.as_ref().map(|(a, b)| (a.as_str(), b.as_str())));
    let logs = inst_root.join("logs");
    std::fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    let suffix = iid.strip_prefix(real_iid).and_then(|r| r.strip_prefix('@')).map(|n| format!("-{n}")).unwrap_or_default();
    let log_path = logs.join(format!("quarry-launch{suffix}.log"));
    let logf = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
    if cmd.iter().map(|c| c.len() + 1).sum::<usize>() > 30000 {                   // Windows command-line limit
        let argfile = inst_root.join(format!("quarry-args{suffix}.txt"));
        let body = cmd[1..].iter().map(|a| format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))).collect::<Vec<_>>().join("\n");
        std::fs::write(&argfile, body).map_err(|e| e.to_string())?;
        cmd = vec![cmd[0].clone(), format!("@{}", argfile.display())];
    }
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]).current_dir(inst_root).stdout(Stdio::from(logf.try_clone().map_err(|e| e.to_string())?)).stderr(Stdio::from(logf)).stdin(Stdio::null());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; c.creation_flags(0x08000000 | 0x00000200); }
    #[cfg(unix)] { use std::os::unix::process::CommandExt; c.process_group(0); }
    let t0 = Instant::now();
    let child = c.spawn().map_err(|e| format!("Couldn't start Java: {e}"))?;
    let pid = child.id();
    PROCS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(iid.to_string(), child);
    update(iid, |s| { s["status"] = json!("running"); s["message"] = json!("Running"); s["progress"] = json!(1.0); s["pid"] = json!(pid); s["log"] = json!(log_path.to_string_lossy()); });
    let (acct_name, _) = account_of(settings);
    let started = store::now();
    let session = format!("{}-{}", (started * 1000.0) as u64, iid);
    add_play(inst_root, &session, started, &acct_name, 0);
    let mut counted: i64 = 0;                                                       // whole seconds already credited
    let mut last_flush = Instant::now();
    let status = loop {
        let done = PROCS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get_mut(iid).map(|c| c.try_wait());
        match done { Some(Ok(Some(st))) => break st, Some(Ok(None)) => {}, _ => break std::process::ExitStatus::default() }
        if last_flush.elapsed() >= Duration::from_secs(60) { let now_s = t0.elapsed().as_secs() as i64; add_play(inst_root, &session, started, &acct_name, now_s - counted); counted = now_s; last_flush = Instant::now(); }
        std::thread::sleep(Duration::from_millis(500));
    };
    PROCS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).remove(iid);
    let secs = t0.elapsed().as_secs();
    add_play(inst_root, &session, started, &acct_name, secs as i64 - counted);
    let rc = status.code();
    #[cfg(unix)] let signalled = { use std::os::unix::process::ExitStatusExt; status.signal() };
    #[cfg(not(unix))] let signalled: Option<i32> = None;
    let fine = match (rc, signalled) { (Some(0), _) | (Some(143), _) | (Some(1), _) => true, (_, Some(15)) => true, _ => false };
    if !fine || (rc != Some(0) && secs < 20) {
        let t = std::fs::read_to_string(&log_path).unwrap_or_default();
        let tail: Vec<&str> = t.trim().lines().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
        update(iid, |s| { s["status"] = json!("error"); s["error"] = json!(format!("Minecraft closed with code {}. {}", rc.map(|c| c.to_string()).unwrap_or_else(|| format!("signal {signalled:?}")), tail.join(" | "))); });
    } else { update(iid, |s| { s["status"] = json!("idle"); s["message"] = json!(""); }); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_versions_without_java_info_get_java_8() {
        assert_eq!(java_need(&json!({"id": "1.12.2"})), ("jre-legacy".to_string(), 8));
        assert_eq!(java_need(&json!({"javaVersion": {"component": "java-runtime-delta", "majorVersion": 21}})), ("java-runtime-delta".to_string(), 21));
    }

    #[test]
    fn legacy_profile_builds_a_command_line() {
        let profile = json!({"id": "1.12.2", "mainClass": "net.minecraft.client.main.Main", "assetIndex": {"id": "1.12"},
            "minecraftArguments": "--username ${auth_player_name} --gameDir ${game_directory} --assetsDir ${assets_root} --accessToken ${auth_access_token}"});
        let plan = Plan { classpath: vec![PathBuf::from("a.jar")], natives: PathBuf::from("/n"), game_assets: PathBuf::from("/a"), log_arg: None, assets_root: PathBuf::from("/m/assets") };
        let who = Identity { name: "Steve".into(), uuid: "u".into(), token: "0".into(), xuid: "0".into(), user_type: "legacy".into() };
        let c = build_command(&profile, &plan, Path::new("java"), Path::new("/i"), &who, 2048, "", None);
        assert!(c.contains(&"-Djava.library.path=/n".to_string()) && c.contains(&"-cp".to_string()));
        let at = c.iter().position(|x| x == "net.minecraft.client.main.Main").unwrap();
        assert_eq!(&c[at + 1..at + 3], ["--username", "Steve"]);
    }
}
