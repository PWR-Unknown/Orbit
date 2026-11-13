//! Import instances from other launchers (port of importers.py): Modrinth App, CurseForge app, TLauncher.
use crate::mods::{self, content_path, disabled_path, is_loader};
use crate::modrinth::{api_get, api_post, jl};
use crate::store::Instance;
use crate::tl;
use crate::util::{sha1_file, Res};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const SETTING_FILES: [&str; 4] = ["options.txt", "optionsof.txt", "optionsshaders.txt", "servers.dat"];
pub const ICON_TYPES: [(&str, &str); 6] = [(".png", "image/png"), (".jpg", "image/jpeg"), (".jpeg", "image/jpeg"), (".webp", "image/webp"), (".gif", "image/gif"), (".svg", "image/svg+xml")];
const LOADER_ORDER: [&str; 4] = ["neoforge", "forge", "fabric", "quilt"];

pub fn icon_type(p: &Path) -> Option<&'static str> {
    let e = format!(".{}", p.extension()?.to_string_lossy().to_lowercase());
    ICON_TYPES.iter().find(|(x, _)| *x == e).map(|x| x.1)
}

fn read_json(p: &Path) -> Option<Value> { serde_json::from_str(std::fs::read_to_string(p).ok()?.trim_start_matches('\u{feff}')).ok() }

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Seconds since the epoch from a number (s or ms) or an ISO date string. 0 if unknown.
fn ts(v: &Value) -> f64 {
    if let Some(n) = v.as_f64() { return if n > 1e11 { n / 1000.0 } else { n.max(0.0) }; }
    if let Some(s) = v.as_str().filter(|s| s.len() >= 10) {
        if let Some(c) = Regex::new(r"^(\d{4})-(\d\d)-(\d\d)(?:[T ](\d\d):(\d\d):(\d\d))?").unwrap().captures(s) {
            let g = |i: usize| c.get(i).map(|m| m.as_str().parse::<i64>().unwrap_or(0)).unwrap_or(0);
            if g(1) > 1970 { return (days_from_civil(g(1), g(2), g(3)) * 86400 + g(4) * 3600 + g(5) * 60 + g(6)) as f64; }
        }
    }
    0.0
}

fn mem(v: &Value) -> i64 {
    let n = v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
    if (512..=65536).contains(&n) { n } else { 0 }
}

fn jload(v: &Value, default: Value) -> Value {
    match v {
        Value::Null => default,
        Value::String(s) if s.is_empty() => default,
        Value::String(s) => serde_json::from_str(s).unwrap_or(default),
        o => o.clone(),
    }
}

fn s_of(v: &Value) -> String { v.as_str().map(String::from).unwrap_or_else(|| if v.is_null() { String::new() } else { v.to_string() }) }

fn cand(id: &str, name: &str, mc: &str, loader: &str, lv: &str, folder: Option<&Path>, extra: Value) -> Value {
    let loader = loader.to_lowercase();
    let bad = if !is_loader(&loader) {
        if loader.is_empty() || loader == "vanilla" { "Vanilla instances aren't supported yet.".to_string() } else { format!("The {loader} loader isn't supported.") }
    } else if mc.is_empty() { "No Minecraft version found.".to_string() } else { String::new() };
    let counts = folder.map(tl::count_content).unwrap_or_default();
    let total: i64 = counts.values().filter_map(|v| v.as_i64()).sum();
    let mut c = json!({"id": id, "name": if name.is_empty() { id } else { name }, "mc": mc, "loader": loader, "loader_version": lv,
        "folder": folder.map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(), "counts": counts, "groups": [], "icon": null,
        "created": 0, "last_played": 0, "played": 0, "settings": {"memory": 0, "jvm_args": "", "java_path": ""}, "titles": {}, "unsupported": bad, "total": total});
    if let Some(o) = extra.as_object() { for (k, v) in o { c[k] = v.clone(); } }
    c
}

// ---------- Modrinth App ----------

fn mr_dirs(folder: &Path) -> (PathBuf, PathBuf) {
    if folder.join("profiles").is_dir() { (folder.join("profiles"), folder.to_path_buf()) }
    else { (folder.to_path_buf(), folder.parent().unwrap_or(folder).to_path_buf()) }
}

pub fn modrinth_profile_dir(root: &Path) -> PathBuf { mr_dirs(root).0 }

const COLS: [(&str, &[&str]); 13] = [
    ("name", &["name", "title"]), ("game_version", &["game_version", "minecraft_version", "mc_version", "version"]),
    ("mod_loader", &["mod_loader", "loader", "modloader"]), ("mod_loader_version", &["mod_loader_version", "loader_version", "modloader_version"]),
    ("groups", &["groups"]), ("icon_path", &["icon_path", "icon"]), ("created", &["created", "date_created"]), ("last_played", &["last_played", "modified"]),
    ("submitted_time_played", &["submitted_time_played", "time_played", "played"]), ("recent_time_played", &["recent_time_played"]),
    ("override_mc_memory_max", &["override_mc_memory_max", "memory_max", "memory"]),
    ("override_extra_launch_args", &["override_extra_launch_args", "extra_launch_args", "java_args"]),
    ("override_java_path", &["override_java_path", "java_path"]),
];

fn key_of(p: &str) -> String { p.replace('\\', "/").trim_end_matches('/').rsplit('/').next().unwrap_or("").trim().to_lowercase() }

fn cell(r: rusqlite::types::ValueRef) -> Value {
    use rusqlite::types::ValueRef::*;
    match r { Null => Value::Null, Integer(i) => json!(i), Real(f) => json!(f), Text(t) => json!(String::from_utf8_lossy(t)), Blob(b) => json!(String::from_utf8_lossy(b)) }
}

fn read_db(db: &Path) -> (HashMap<String, Value>, String) {
    let tmp = std::env::temp_dir().join(format!("orbit-mrdb-{:08x}", rand::random::<u32>()));
    let _ = std::fs::create_dir_all(&tmp);
    let res = (|| -> Result<HashMap<String, Value>, String> {
        let name = db.file_name().unwrap().to_string_lossy().into_owned();
        let mut copied = false;
        for suf in ["", "-wal", "-shm"] {
            let src = db.with_file_name(format!("{name}{suf}"));
            if src.is_file() && std::fs::copy(&src, tmp.join(format!("app.db{suf}"))).is_ok() { copied = true; }
        }
        let con = if copied { rusqlite::Connection::open(tmp.join("app.db")) }
            else { rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) }.map_err(|e| format!("Couldn't read app.db: {e}"))?;
        let tables: Vec<String> = con.prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view')").and_then(|mut s| s.query_map([], |r| r.get(0))?.collect())
            .map_err(|e| format!("Couldn't read app.db: {e}"))?;
        let pref = ["profiles", "instances", "profile", "instance"];
        let mut sorted = tables.clone();
        sorted.sort_by_key(|n| (!pref.contains(&n.to_lowercase().as_str()), n.clone()));
        let mut best: Option<String> = None;
        for t in &sorted {
            let cols: Vec<String> = con.prepare(&format!("PRAGMA table_info(\"{t}\")")).and_then(|mut s| s.query_map([], |r| r.get(1))?.collect()).unwrap_or_default();
            if cols.iter().any(|c| c == "path") && cols.iter().any(|c| COLS[0].1.iter().chain(COLS[1].1.iter()).any(|n| n == c)) { best = Some(t.clone()); break; }
        }
        let t = best.ok_or_else(|| format!("No profiles table found in app.db (tables: {}).", tables.join(", ")))?;
        let mut stmt = con.prepare(&format!("SELECT * FROM \"{t}\"")).map_err(|e| e.to_string())?;
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut out = HashMap::new();
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            let mut m: HashMap<String, Value> = HashMap::new();
            for (i, n) in names.iter().enumerate() { m.insert(n.clone(), cell(r.get_ref(i).map_err(|e| e.to_string())?)); }
            let mut row = Map::new();
            for (k, cands) in COLS {
                let v = cands.iter().find_map(|c| m.get(*c).filter(|v| !v.is_null() && v.as_str() != Some(""))).cloned().unwrap_or(Value::Null);
                row.insert(k.into(), v);
            }
            let path = s_of(&m["path"]);
            row.insert("_path".into(), json!(path));
            out.insert(key_of(&path), Value::Object(row));
        }
        Ok(out)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    match res { Ok(m) => (m, String::new()), Err(e) => (HashMap::new(), e) }
}

/// Best guess at (minecraft version, loader) from a profile's files.
fn infer(d: &Path) -> (String, String) {
    let mut votes: HashMap<&str, i32> = HashMap::new();
    let mut mcs: HashMap<String, i32> = HashMap::new();
    let mut jars: Vec<PathBuf> = std::fs::read_dir(d.join("mods")).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().map(|e| e == "jar").unwrap_or(false)).collect()).unwrap_or_default();
    jars.sort(); jars.truncate(60);
    let ver_re = Regex::new(r"^[~=^]?\s*(\d{1,2}\.\d{1,2}(?:\.\d{1,2})?)$").unwrap();
    for f in jars {
        let Ok(fh) = std::fs::File::open(&f) else { continue };
        let Ok(mut z) = zip::ZipArchive::new(fh) else { continue };
        let has = |z: &mut zip::ZipArchive<std::fs::File>, n: &str| z.by_name(n).is_ok();
        let (neo, quilt, fabric, forge) = (has(&mut z, "META-INF/neoforge.mods.toml"), has(&mut z, "quilt.mod.json"), has(&mut z, "fabric.mod.json"), has(&mut z, "META-INF/mods.toml"));
        let l = if neo { Some("neoforge") } else if quilt && !fabric { Some("quilt") } else if fabric { Some("fabric") } else if forge { Some("forge") } else { None };
        if let Some(l) = l { *votes.entry(l).or_default() += 1; }
        if fabric {
            let mut s = String::new();
            if let Ok(mut e) = z.by_name("fabric.mod.json") { let _ = std::io::Read::read_to_string(&mut e, &mut s); }
            if let Ok(j) = serde_json::from_str::<Value>(s.trim_start_matches('\u{feff}')) {
                let dep = &j["depends"]["minecraft"];
                let list: Vec<&str> = match dep { Value::String(x) => vec![x.as_str()], Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect(), _ => vec![] };
                for v in list { if let Some(c) = ver_re.captures(v.trim()) { *mcs.entry(c[1].to_string()).or_default() += 1; } }
            }
        }
    }
    let mut loader = votes.iter().max_by_key(|(_, c)| **c).map(|(l, _)| l.to_string()).unwrap_or_default();
    let name = d.file_name().unwrap_or_default().to_string_lossy().to_string();
    let low = name.to_lowercase();
    if let Some(l) = LOADER_ORDER.iter().find(|l| low.contains(*l)) { loader = l.to_string(); }
    let mc = Regex::new(r"(?:^|[^\d.])(\d{1,2}\.\d{1,2}(?:\.\d{1,2})?)(?:$|[^\d])").unwrap().captures(&name).map(|c| c[1].to_string())
        .or_else(|| mcs.iter().max_by_key(|(_, c)| **c).map(|(v, _)| v.clone())).unwrap_or_default();
    (mc, loader)
}

fn legacy_profile(d: &Path) -> Option<Value> {
    let data = read_json(&d.join("profile.json")).filter(|v| v.is_object())?;
    let meta = if data["metadata"].is_object() { &data["metadata"] } else { &data };
    let java = if data["java"].is_object() { &data["java"] } else { &Value::Null };
    let lv = if meta["loader_version"].is_object() { meta["loader_version"]["id"].clone() } else { meta["loader_version"].clone() };
    Some(json!({"name": meta["name"], "game_version": meta["game_version"], "mod_loader": meta["loader"], "mod_loader_version": lv, "groups": meta["groups"],
        "icon_path": meta["icon"], "created": meta["date_created"], "last_played": meta["last_played"], "submitted_time_played": meta["submitted_time_played"],
        "override_mc_memory_max": data["memory"]["maximum"], "override_extra_launch_args": java["extra_arguments"], "override_java_path": java["override_version"]}))
}

fn icon_path(raw: &Value, d: &Path, app: &Path) -> Option<String> {
    let raw = raw.as_str().filter(|s| !s.is_empty())?;
    let p = PathBuf::from(raw);
    let cands = if p.is_absolute() { vec![p] } else { vec![d.join(&p), app.join(&p), d.parent().unwrap_or(d).join(&p)] };
    cands.into_iter().find(|c| c.is_file() && icon_type(c).is_some() && std::fs::metadata(c).map(|m| m.len() < 2_000_000).unwrap_or(false)).map(|c| c.to_string_lossy().into_owned())
}

fn sorted_children(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort_by_key(|x| x.file_name().unwrap_or_default().to_string_lossy().to_lowercase()); v
}

pub fn scan_modrinth(folder: &Path) -> Res<Vec<Value>> {
    let (profiles, app) = mr_dirs(folder);
    if !profiles.is_dir() { return Err(format!("{} doesn't exist. Set the Modrinth App folder in Settings > Import folders.", profiles.display())); }
    let db = app.join("app.db");
    let (rows, dberr) = if db.is_file() { read_db(&db) } else { (HashMap::new(), format!("No app.db found in {}.", app.display())) };
    let mut out = vec![];
    for d in sorted_children(&profiles).into_iter().filter(|d| d.is_dir()) {
        let name = d.file_name().unwrap().to_string_lossy().to_string();
        let r = rows.get(&key_of(&name)).cloned().or_else(|| legacy_profile(&d));
        let Some(r) = r else {
            if !tl::count_content(&d).is_empty() {
                let (mc, loader) = infer(&d);
                let mut c = cand(&name, &name, &mc, &loader, "", Some(&d), json!({}));
                if !c["unsupported"].as_str().unwrap_or("").is_empty() {
                    c["unsupported"] = json!(format!("No profile information found{}.", if dberr.is_empty() { String::new() } else { format!(" ({dberr})") }));
                } else { c["guessed"] = json!(true); }
                out.push(c);
            }
            continue;
        };
        let args = jload(&r["override_extra_launch_args"], json!([]));
        let groups: Vec<Value> = jload(&r["groups"], json!([])).as_array().into_iter().flatten().filter(|g| !s_of(g).trim().is_empty()).map(|g| json!(s_of(g))).collect();
        let played = r["submitted_time_played"].as_i64().unwrap_or(0) + r["recent_time_played"].as_i64().unwrap_or(0);
        let jvm = match &args { Value::Array(a) => a.iter().map(s_of).collect::<Vec<_>>().join(" "), o => s_of(o) };
        out.push(cand(&name, &s_of(&r["name"]), &s_of(&r["game_version"]), &s_of(&r["mod_loader"]), &s_of(&r["mod_loader_version"]), Some(&d),
            json!({"groups": groups, "icon": icon_path(&r["icon_path"], &d, &app), "created": ts(&r["created"]), "last_played": ts(&r["last_played"]), "played": played,
                "settings": {"memory": mem(&r["override_mc_memory_max"]), "jvm_args": jvm, "java_path": s_of(&r["override_java_path"])}})));
    }
    Ok(out)
}

// ---------- CurseForge app ----------

pub fn scan_curseforge(folder: &Path) -> Res<Vec<Value>> {
    let mut base = folder.join("Instances");
    if !base.is_dir() && folder.file_name().map(|n| n.to_string_lossy().to_lowercase() == "instances").unwrap_or(false) { base = folder.to_path_buf(); }
    if !base.is_dir() { return Err(format!("No Instances folder in {}. Set the CurseForge folder in Settings > Import folders.", folder.display())); }
    let mut out = vec![];
    for d in sorted_children(&base) {
        let Some(j) = d.is_dir().then(|| read_json(&d.join("minecraftinstance.json"))).flatten().filter(|v| v.is_object()) else { continue };
        let bl = &j["baseModLoader"];
        let lname = bl["name"].as_str().unwrap_or("").to_lowercase();
        let loader = LOADER_ORDER.iter().find(|l| lname.starts_with(*l)).map(|l| l.to_string())
            .or_else(|| match bl["type"].as_i64() { Some(1) => Some("forge"), Some(4) => Some("fabric"), Some(5) => Some("quilt"), Some(6) => Some("neoforge"), _ => None }.map(String::from)).unwrap_or_default();
        let lv = bl["forgeVersion"].as_str().filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(|| lname.split_once('-').map(|x| x.1.to_string()).unwrap_or_default());
        let mc = bl["minecraftVersion"].as_str().or(j["gameVersion"].as_str()).unwrap_or("");
        let mut titles = Map::new();
        for a in j["installedAddons"].as_array().into_iter().flatten() {
            if let (Some(f), Some(n)) = (a["installedFile"]["fileName"].as_str(), a["name"].as_str()) { titles.insert(f.into(), json!(n)); }
        }
        let m = if j["isMemoryOverride"] != false { mem(&j["allocatedMemory"]) } else { 0 };
        let args = j["javaArgsOverride"].as_str().map(|s| s.trim().to_string()).unwrap_or_default();
        let name = d.file_name().unwrap().to_string_lossy().to_string();
        out.push(cand(&name, j["name"].as_str().unwrap_or(&name), mc, &loader, &lv, Some(&d),
            json!({"titles": titles, "last_played": ts(&j["lastPlayed"]), "created": ts(&j["dateCreated"]), "settings": {"memory": m, "jvm_args": args, "java_path": ""}})));
    }
    Ok(out)
}

pub fn scan_tlauncher(root: &Path) -> Vec<Value> {
    tl::scan(root).into_iter().map(|v| {
        cand(v["id"].as_str().unwrap_or(""), v["name"].as_str().unwrap_or(""), v["mc"].as_str().unwrap_or(""), v["loader"].as_str().unwrap_or(""),
            v["loader_version"].as_str().unwrap_or(""), None,
            json!({"settings": {"memory": v["memory"].as_i64().unwrap_or(0), "jvm_args": "", "java_path": ""}, "sources": v["sources"], "modpack": v["modpack"]}))
    }).collect()
}

// ---------- copying ----------

fn stem(n: &str) -> String { n.strip_suffix(".disabled").or_else(|| n.strip_suffix(".DISABLED")).unwrap_or(n).to_string() }

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(t) = e.file_type() else { continue };
        if t.is_symlink() { continue; }
        if t.is_dir() { walk_files(&p, out); } else if t.is_file() { out.push(p); }
    }
}

fn plain(src: &Path, dst: &Path, only: impl Fn(&Path) -> bool) -> i64 {
    let mut files = vec![]; walk_files(src, &mut files);
    let mut c = 0;
    for f in files {
        if !only(&f) { continue; }
        let dest = dst.join(f.strip_prefix(src).unwrap());
        if dest.exists() { continue; }
        if let Some(p) = dest.parent() { let _ = std::fs::create_dir_all(p); }
        if std::fs::copy(&f, &dest).is_ok() { c += 1; }
    }
    c
}

/// Copy content, config, game settings (and optionally worlds). Existing files are never overwritten.
pub fn copy_files(inst: &mut Instance, source: &Path, shared: bool, saves: bool, titles: &Value) -> i64 {
    let mut n = 0;
    for dname in tl::CONTENT_DIRS {
        let d = source.join(dname);
        if !d.is_dir() { continue; }
        for f in sorted_children(&d) {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            let is_real = std::fs::symlink_metadata(&f).map(|m| m.is_file()).unwrap_or(false);
            if !is_real || !tl::has_ext(&name) { continue; }
            let dest = inst.root.join(dname).join(&name);
            if dest.exists() { continue; }
            let _ = std::fs::create_dir_all(dest.parent().unwrap());
            if std::fs::copy(&f, &dest).is_err() { continue; }
            let st = stem(&name);
            let pstem = Path::new(&st).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            inst.mods_mut().insert(format!("imp:{dname}/{st}"), json!({"type": mods::dir_type(dname), "title": titles[&st].as_str().unwrap_or(&pstem), "slug": pstem,
                "source": "imported", "version_id": "", "version": "", "filename": st, "explicit": true, "disabled": name.to_lowercase().ends_with(".disabled")}));
            n += 1;
        }
    }
    if !shared && source.join("config").is_dir() { n += plain(&source.join("config"), &inst.root.join("config"), |_| true); }
    if source.join("shaderpacks").is_dir() {
        n += plain(&source.join("shaderpacks"), &inst.root.join("shaderpacks"), |f| f.extension().map(|e| e.to_string_lossy().to_lowercase() == "txt").unwrap_or(false));
    }
    for fname in SETTING_FILES {
        let f = source.join(fname);
        if f.is_file() && !inst.root.join(fname).exists() && std::fs::copy(&f, inst.root.join(fname)).is_ok() { n += 1; }
    }
    if saves && source.join("saves").is_dir() {
        for w in sorted_children(&source.join("saves")) {
            let name = w.file_name().unwrap().to_string_lossy().to_string();
            if w.is_dir() && !std::fs::symlink_metadata(&w).map(|m| m.file_type().is_symlink()).unwrap_or(false) && !inst.root.join("saves").join(&name).exists() {
                plain(&w, &inst.root.join("saves").join(&name), |_| true);
                n += 1;
            }
        }
    }
    n
}

fn set_icon(inst: &mut Instance, src: &str) -> std::io::Result<()> {
    let p = Path::new(src);
    let name = format!("icon{}", p.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default());
    std::fs::copy(p, inst.root.join(&name))?;
    inst.state["icon"] = json!(name);
    Ok(())
}

/// Fill a freshly created instance from an import candidate. Returns a summary.
pub fn apply_candidate(inst: &mut Instance, c: &Value, source: Option<&Path>, saves: bool) -> Res {
    let mut got: Vec<String> = vec![];
    if !inst.state["settings"].is_object() { inst.state["settings"] = json!({}); }
    let s = &c["settings"];
    if let Some(m) = s["memory"].as_i64().filter(|m| *m > 0) { inst.state["settings"]["memory"] = json!(m); got.push(format!("memory ({m} MB)")); }
    if let Some(a) = s["jvm_args"].as_str().filter(|a| !a.is_empty()) { inst.state["settings"]["jvm_args"] = json!(a.chars().take(500).collect::<String>()); got.push("Java arguments".into()); }
    if let Some(j) = s["java_path"].as_str().filter(|j| !j.is_empty() && Path::new(j).is_file()) { inst.state["settings"]["java_path"] = json!(j); got.push("Java path".into()); }
    if let Some(g) = c["groups"].as_array().filter(|g| !g.is_empty()) {
        let mut seen = vec![]; for x in g { if !seen.contains(x) { seen.push(x.clone()); } }
        inst.state["groups"] = json!(seen); got.push("groups".into());
    }
    if let Some(i) = c["icon"].as_str().filter(|i| !i.is_empty()) { if set_icon(inst, i).is_ok() { got.push("icon".into()); } }
    for (k, to) in [("created", "created"), ("last_played", "last_played"), ("played", "played_seconds")] {
        if c[k].as_f64().map(|v| v > 0.0).unwrap_or(false) { inst.state[to] = c[k].clone(); }
    }
    let mut files = 0;
    if let Some(src) = source {
        files = copy_files(inst, src, c["shared"] == true, saves, &c["titles"]);
        if SETTING_FILES.iter().any(|f| src.join(f).is_file()) { got.push("game options".into()); }
    }
    inst.save()?;
    let res = link_modrinth(inst);
    inst.save()?;
    Ok(json!({"files": files, "linked": res["linked"], "checked": res["checked"], "net_error": res["error"], "settings": got}))
}

// ---------- match with Modrinth ----------

/// Look every imported / modpack file up on Modrinth by SHA-1 and turn the matches into regular Modrinth installs
/// (real title, icon, page link, update checks). Files Modrinth doesn't know stay as they are.
pub fn link_modrinth(inst: &mut Instance) -> Value {
    let mut todo: HashMap<String, Vec<String>> = HashMap::new();
    for (k, m) in inst.mods().clone() {
        if m["source"] != "imported" && m["source"] != "modpack" { continue; }
        let mut p = content_path(inst, &m);
        if m["disabled"] == true { p = disabled_path(&p); }
        if p.is_file() { if let Ok(h) = sha1_file(&p) { todo.entry(h).or_default().push(k); } }
    }
    if todo.is_empty() { return json!({"linked": 0, "checked": 0, "error": ""}); }
    let net = (|| -> Result<(Value, HashMap<String, Value>), String> {
        let hashes: Vec<&String> = todo.keys().collect();
        let mut found = Map::new();
        for ch in hashes.chunks(200) {
            if let Some(o) = api_post("/version_files", &json!({"hashes": ch, "algorithm": "sha1"}))?.as_object() { found.extend(o.clone()); }
        }
        let mut pids: Vec<String> = found.values().filter_map(|v| v["project_id"].as_str().map(String::from)).collect();
        pids.sort(); pids.dedup();
        let mut projects = HashMap::new();
        for ch in pids.chunks(100) {
            for p in api_get("/projects", &[("ids", jl(ch))])?.as_array().into_iter().flatten() { projects.insert(p["id"].as_str().unwrap_or("").to_string(), p.clone()); }
        }
        Ok((Value::Object(found), projects))
    })();
    let (found, projects) = match net { Ok(x) => x, Err(e) => return json!({"linked": 0, "checked": todo.len(), "error": e}) };
    let mut linked = 0;
    for (h, v) in found.as_object().unwrap() {
        let Some(proj) = v["project_id"].as_str().and_then(|p| projects.get(p)) else { continue };
        for k in todo.get(h).into_iter().flatten() {
            let pid = proj["id"].as_str().unwrap_or("").to_string();
            if inst.mods().contains_key(&pid) { continue; }
            let m = inst.mods_mut().remove(k).unwrap();
            let mut e = json!({"type": m["type"].as_str().unwrap_or("mod"), "icon_url": proj["icon_url"], "title": proj["title"], "slug": proj["slug"],
                "source": "modrinth", "version_id": v["id"], "version": v["version_number"], "filename": m["filename"], "explicit": true, "disabled": m["disabled"] == true});
            // a modpack file in a sub-folder (mods/extra/x.jar) isn't where `type + filename` would put it: keep its path
            if let Some(rp) = m["relpath"].as_str().filter(|r| r.matches('/').count() > 1) { e["relpath"] = json!(rp); }
            inst.mods_mut().insert(pid, e);
            linked += 1;
        }
    }
    json!({"linked": linked, "checked": todo.len(), "error": ""})
}
