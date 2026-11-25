//! Mod manager core (port of modrinth_mods.py): install / update / remove content from Modrinth and
//! CurseForge, .mrpack modpacks (import and export), search, project pages, and the Modrinth/CurseForge API clients.
use crate::store::{Instance, STATE_NAME};
use crate::util::{agent, download_file, host_of, safe_join, track_expected, Res};
use serde_json::{json, Map, Value};
use sha1::Sha1;
use sha2::{Digest, Sha512};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const CF_LOADERS: [(&str, i64); 4] = [("forge", 1), ("fabric", 4), ("quilt", 5), ("neoforge", 6)];
pub fn cf_loader(l: &str) -> Option<i64> { CF_LOADERS.iter().find(|(n, _)| *n == l).map(|x| x.1) }
pub fn is_loader(l: &str) -> bool { cf_loader(l).is_some() }

/// (type, directory inside an instance, CurseForge class id)
pub const TYPES: [(&str, &str, i64); 5] = [
    ("modpack", "", 4471), ("mod", "mods", 6), ("resourcepack", "resourcepacks", 12),
    ("datapack", "datapacks", 6945), ("shader", "shaderpacks", 6552),
];
pub fn type_dir(t: &str) -> &'static str { TYPES.iter().find(|x| x.0 == t).map(|x| x.1).unwrap_or("mods") }
pub fn type_class(t: &str) -> i64 { TYPES.iter().find(|x| x.0 == t).map(|x| x.2).unwrap_or(6) }
pub fn is_type(t: &str) -> bool { TYPES.iter().any(|x| x.0 == t) }
pub fn dir_type(d: &str) -> Option<&'static str> { TYPES.iter().find(|x| !x.1.is_empty() && x.1 == d).map(|x| x.0) }

pub const MRPACK_LOADERS: [(&str, &str); 4] = [("fabric-loader", "fabric"), ("quilt-loader", "quilt"), ("forge", "forge"), ("neoforge", "neoforge")];

pub type Log = Vec<String>;

impl Instance {
    pub fn dir_for(&self, ptype: Option<&str>) -> PathBuf {
        self.root.join(type_dir(ptype.filter(|t| !t.is_empty()).unwrap_or("mod")))
    }
    pub fn mods(&self) -> &Map<String, Value> {
        static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
        self.state["mods"].as_object().unwrap_or_else(|| EMPTY.get_or_init(Map::new))
    }
    pub fn mods_mut(&mut self) -> &mut Map<String, Value> {
        if !self.state["mods"].is_object() { self.state["mods"] = json!({}); }
        self.state["mods"].as_object_mut().unwrap()
    }
    /// The {mc, loader, ...} config; an error when the instance was never initialised.
    pub fn cfg(&self) -> Res<&Value> {
        match self.state["config"].as_object() { Some(o) if !o.is_empty() => Ok(&self.state["config"]), _ => Err("Instance has no game version / loader set.".into()) }
    }
    pub fn mc(&self) -> String { self.state["config"]["mc"].as_str().unwrap_or("").to_string() }
    pub fn loader(&self) -> String { self.state["config"]["loader"].as_str().unwrap_or("vanilla").to_string() }
}

fn st(v: &Value, k: &str) -> String { v[k].as_str().unwrap_or("").to_string() }
fn quote(s: &str) -> String { percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string() }

pub fn content_path(inst: &Instance, m: &Value) -> PathBuf {
    match m["relpath"].as_str().filter(|r| !r.is_empty()) {
        Some(r) => inst.root.join(r),
        None => inst.dir_for(m["type"].as_str()).join(m["filename"].as_str().unwrap_or("")),
    }
}

fn disabled_name(p: &Path) -> PathBuf { let mut n = p.file_name().unwrap_or_default().to_os_string(); n.push(".disabled"); p.with_file_name(n) }
pub fn disabled_path(p: &Path) -> PathBuf { disabled_name(p) }

fn dup_elsewhere(inst: &Instance, key: &str, slug: &str) -> Option<Value> {
    inst.mods().iter().find(|(k, m)| k.as_str() != key && m["slug"].as_str() == Some(slug)).map(|(_, m)| m.clone())
}

// ---------- Modrinth ----------

pub fn best_version(project: &str, mc: &str, loader: Option<&str>) -> Res<Option<Value>> {
    let mut p = vec![("game_versions", jl(&[mc]))];
    if let Some(l) = loader { p.push(("loaders", jl(&[l]))); }
    let v = api_get(&format!("/project/{project}/version"), &p)?;
    let Some(arr) = v.as_array().filter(|a| !a.is_empty()) else { return Ok(None) };
    Ok(Some(arr.iter().find(|x| x["version_type"] == "release").unwrap_or(&arr[0]).clone()))
}

fn primary_file(v: &Value) -> Option<&Value> {
    let files = v["files"].as_array()?;
    files.iter().find(|f| f["primary"] == true).or_else(|| files.first())
}

pub fn install_project(inst: &mut Instance, project: &str, explicit: bool, seen: &mut HashSet<String>, ptype: &str, log: &mut Log) -> Res<()> {
    let cfg = inst.cfg()?.clone();
    let meta = api_get(&format!("/project/{project}"), &[])?;
    if meta.is_null() { log.push(format!("! project '{project}' not found on Modrinth")); return Ok(()); }
    let pid = st(&meta, "id");
    if !seen.insert(pid.clone()) { return Ok(()); }
    let title = st(&meta, "title");
    if let Some(d) = dup_elsewhere(inst, &pid, &st(&meta, "slug")) {
        log.push(format!("= {title} is already installed from {}, skipping", d["source"].as_str().unwrap_or("modrinth")));
        return Ok(());
    }
    let loader = (ptype == "mod").then(|| st(&cfg, "loader")).filter(|l| l != "vanilla");
    let Some(version) = best_version(&pid, &st(&cfg, "mc"), loader.as_deref())? else {
        log.push(format!("! {title}: no version for {} / {}", st(&cfg, "mc"), st(&cfg, "loader")));
        return Ok(());
    };
    let installed = inst.mods().get(&pid).cloned();
    if let Some(i) = &installed {
        if i["version_id"] == version["id"] {
            log.push(format!("= {title} already up to date"));
            let e = i["explicit"].as_bool().unwrap_or(false) || explicit;
            inst.mods_mut().get_mut(&pid).unwrap()["explicit"] = json!(e);
            return Ok(());
        }
    }
    if ptype == "mod" {
        for dep in version["dependencies"].as_array().cloned().unwrap_or_default() {
            match dep["dependency_type"].as_str() {
                Some("required") => {
                    let mut dp = dep["project_id"].as_str().map(String::from);
                    if dp.is_none() {
                        if let Some(vid) = dep["version_id"].as_str() {
                            let dv = api_get(&format!("/version/{vid}"), &[])?;
                            dp = dv["project_id"].as_str().map(String::from);
                        }
                    }
                    if let Some(d) = dp { install_project(inst, &d, false, seen, "mod", log)?; }
                }
                Some("incompatible") => log.push(format!("! note: {title} is incompatible with project {}", dep["project_id"].as_str().unwrap_or("?"))),
                _ => {}
            }
        }
    }
    let dest = inst.dir_for(Some(ptype));
    let f = primary_file(&version).ok_or("That version has no files.")?.clone();
    let filename = download_file(f["url"].as_str().unwrap_or(""), &dest, f["filename"].as_str().unwrap_or(""), Some("sha512"), f["hashes"]["sha512"].as_str())?;
    if let Some(i) = &installed {
        if i["filename"].as_str() != Some(&filename) {
            let _ = std::fs::remove_file(inst.dir_for(i["type"].as_str()).join(i["filename"].as_str().unwrap_or("")));
        }
    }
    let expl = explicit || installed.as_ref().map(|i| i["explicit"].as_bool().unwrap_or(false)).unwrap_or(false);
    let ver = st(&version, "version_number");
    inst.mods_mut().insert(pid, json!({"type": ptype, "icon_url": meta["icon_url"], "title": title, "slug": meta["slug"],
        "source": "modrinth", "version_id": version["id"], "version": ver, "filename": filename, "explicit": expl}));
    log.push(format!("+ {title} {ver}"));
    Ok(())
}

// ---------- CurseForge ----------

pub fn cf_resolve(name: &str, ptype: &str) -> Res<i64> {
    if let Ok(n) = name.parse::<i64>() { return Ok(n); }
    let res = cf_get("/mods/search", &[("gameId", CF_GAME_ID.to_string()), ("classId", type_class(ptype).to_string()), ("slug", name.to_string())])?;
    res["data"][0]["id"].as_i64().ok_or_else(|| format!("CurseForge: no mod with slug '{name}'."))
}

pub fn cf_best_file(mod_id: i64, mc: &str, loader: Option<&str>) -> Res<Option<Value>> {
    let mut p = vec![("gameVersion", mc.to_string()), ("pageSize", "50".to_string())];
    if let Some(l) = loader.and_then(cf_loader) { p.push(("modLoaderType", l.to_string())); }
    let res = cf_get(&format!("/mods/{mod_id}/files"), &p)?;
    let mut files: Vec<Value> = res["data"].as_array().cloned().unwrap_or_default().into_iter().filter(|f| f["isAvailable"] != false).collect();
    if files.is_empty() { return Ok(None); }
    files.sort_by(|a, b| st(b, "fileDate").cmp(&st(a, "fileDate")));
    Ok(Some(files.iter().find(|f| f["releaseType"] == 1).unwrap_or(&files[0]).clone()))
}

fn cf_hash(f: &Value) -> (Option<&'static str>, Option<String>) {
    for h in f["hashes"].as_array().into_iter().flatten() {
        if h["algo"] == 1 { return (Some("sha1"), h["value"].as_str().map(String::from)); }
    }
    (None, None)
}

pub fn install_cf(inst: &mut Instance, mod_id: i64, explicit: bool, seen: &mut HashSet<String>, ptype: &str, log: &mut Log) -> Res<()> {
    let cfg = inst.cfg()?.clone();
    let key = format!("cf:{mod_id}");
    if !seen.insert(key.clone()) { return Ok(()); }
    let meta = cf_get(&format!("/mods/{mod_id}"), &[])?["data"].clone();
    if meta.is_null() { log.push(format!("! CurseForge project {mod_id} not found")); return Ok(()); }
    let title = st(&meta, "name");
    let page = meta["links"]["websiteUrl"].as_str().map(String::from)
        .unwrap_or_else(|| format!("https://www.curseforge.com/minecraft/mc-mods/{}", st(&meta, "slug")));
    if let Some(d) = dup_elsewhere(inst, &key, &st(&meta, "slug")) {
        log.push(format!("= {title} is already installed from {}, skipping", d["source"].as_str().unwrap_or("modrinth")));
        return Ok(());
    }
    let loader = (ptype == "mod").then(|| st(&cfg, "loader"));
    let Some(f) = cf_best_file(mod_id, &st(&cfg, "mc"), loader.as_deref())? else {
        log.push(format!("! {title}: no file for {} / {}", st(&cfg, "mc"), st(&cfg, "loader")));
        return Ok(());
    };
    let installed = inst.mods().get(&key).cloned();
    if let Some(i) = &installed {
        if i["version_id"].as_str() == Some(&f["id"].to_string()) {
            log.push(format!("= {title} already up to date"));
            let e = i["explicit"].as_bool().unwrap_or(false) || explicit;
            inst.mods_mut().get_mut(&key).unwrap()["explicit"] = json!(e);
            return Ok(());
        }
    }
    if ptype == "mod" {
        for dep in f["dependencies"].as_array().cloned().unwrap_or_default() {
            match dep["relationType"].as_i64() {
                Some(3) => { if let Some(id) = dep["modId"].as_i64() { install_cf(inst, id, false, seen, "mod", log)?; } }
                Some(5) => log.push(format!("! note: {title} is incompatible with CurseForge project {}", dep["modId"])),
                _ => {}
            }
        }
    }
    let Some(url) = f["downloadUrl"].as_str().filter(|u| !u.is_empty()) else {
        log.push(format!("! {title}: the author doesn't allow downloads from other apps. Download it manually: {page}/files/{}", f["id"]));
        return Ok(());
    };
    let (algo, exp) = cf_hash(&f);
    let dest = inst.dir_for(Some(ptype));
    let filename = download_file(url, &dest, f["fileName"].as_str().unwrap_or(""), algo, exp.as_deref())?;
    if let Some(i) = &installed {
        if i["filename"].as_str() != Some(&filename) {
            let _ = std::fs::remove_file(inst.dir_for(i["type"].as_str()).join(i["filename"].as_str().unwrap_or("")));
        }
    }
    let expl = explicit || installed.as_ref().map(|i| i["explicit"].as_bool().unwrap_or(false)).unwrap_or(false);
    let disp = f["displayName"].as_str().map(String::from).unwrap_or_else(|| f["id"].to_string());
    inst.mods_mut().insert(key, json!({"type": ptype, "icon_url": meta["logo"]["thumbnailUrl"], "title": title, "slug": meta["slug"],
        "source": "curseforge", "version_id": f["id"].to_string(), "version": disp, "filename": filename, "explicit": expl}));
    log.push(format!("+ {title} {disp} (CurseForge)"));
    Ok(())
}

// ---------- modpacks (.mrpack) ----------

pub struct PackInfo {
    pub title: String, pub mc: String, pub loader: String, pub loader_version: String, pub version: String,
    pub project_id: Value, pub icon_url: Value, pub pack: PathBuf, pub index: Value, pub version_id: String,
}

pub fn load_mrpack(path: &Path) -> Res<PackInfo> {
    let bad = || "That file is not a valid .mrpack modpack.".to_string();
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut z = zip::ZipArchive::new(f).map_err(|_| bad())?;
    let idx: Value = {
        let mut e = z.by_name("modrinth.index.json").map_err(|_| bad())?;
        let mut s = String::new();
        std::io::Read::read_to_string(&mut e, &mut s).map_err(|_| bad())?;
        serde_json::from_str(s.trim_start_matches('\u{feff}')).map_err(|_| bad())?
    };
    let deps = idx["dependencies"].as_object().cloned().unwrap_or_default();
    let key = MRPACK_LOADERS.iter().find(|(k, _)| deps.contains_key(*k));
    let loader = match key {
        Some((_, l)) => Some(l.to_string()),
        None if deps.contains_key("minecraft") && deps.keys().all(|k| k == "minecraft") => Some("vanilla".to_string()),
        None => None,
    };
    let mc = deps.get("minecraft").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let (true, Some(loader)) = (!mc.is_empty(), loader) else {
        return Err("Only vanilla, Fabric, Quilt, Forge and NeoForge modpacks are supported.".into());
    };
    let lv = key.and_then(|(k, _)| deps.get(*k)).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(PackInfo {
        title: idx["name"].as_str().filter(|s| !s.is_empty()).map(String::from).unwrap_or(stem), mc, loader, loader_version: lv,
        version: idx["versionId"].as_str().unwrap_or("").to_string(), project_id: Value::Null, icon_url: Value::Null,
        pack: path.to_path_buf(), index: idx, version_id: String::new(),
    })
}

pub fn fetch_modpack(ident: &str, tmp: &Path, mc: Option<&str>) -> Res<PackInfo> {
    let meta = api_get(&format!("/project/{ident}"), &[])?;
    if meta.is_null() { return Err("That modpack wasn't found on Modrinth.".into()); }
    let title = st(&meta, "title");
    let params: Vec<(&str, String)> = mc.map(|m| vec![("game_versions", jl(&[m]))]).unwrap_or_default();
    let versions = api_get(&format!("/project/{}/version", st(&meta, "id")), &params)?;
    let Some(vs) = versions.as_array().filter(|a| !a.is_empty()) else {
        return Err(match mc { Some(m) => format!("{title} has no version for Minecraft {m}."), None => format!("{title} has no versions.") });
    };
    let v = vs.iter().find(|x| x["version_type"] == "release").unwrap_or(&vs[0]);
    download_pack(&meta, v, tmp)
}

/// Download one modpack version (a Modrinth version object) and read it.
fn download_pack(meta: &Value, v: &Value, tmp: &Path) -> Res<PackInfo> {
    let title = st(meta, "title");
    let f = v["files"].as_array().into_iter().flatten().find(|x| x["filename"].as_str().map(|n| n.ends_with(".mrpack")).unwrap_or(false))
        .ok_or_else(|| format!("{title} {} has no .mrpack file.", st(v, "version_number")))?;
    let name = download_file(f["url"].as_str().unwrap_or(""), tmp, f["filename"].as_str().unwrap_or(""), Some("sha512"), f["hashes"]["sha512"].as_str())?;
    let mut info = load_mrpack(&tmp.join(name))?;
    info.title = title; info.version = st(v, "version_number"); info.version_id = st(v, "id");
    info.project_id = meta["id"].clone(); info.icon_url = meta["icon_url"].clone();
    Ok(info)
}

/// Every version of a modpack project, newest first, for the "change version" picker.
pub fn pack_versions(project_id: &str) -> Res<Vec<Value>> {
    let v = api_get(&format!("/project/{project_id}/version"), &[])?;
    let Some(a) = v.as_array() else { return Err("That modpack wasn't found on Modrinth.".into()) };
    let mut out: Vec<Value> = a.iter().filter(|x| x["files"].as_array().map(|f| f.iter().any(|y| y["filename"].as_str().map(|n| n.ends_with(".mrpack")).unwrap_or(false))).unwrap_or(false))
        .map(|x| {
            let cl: String = x["changelog"].as_str().unwrap_or("").chars().take(1200).collect();
            json!({"id": x["id"], "version_number": x["version_number"], "name": x["name"], "type": x["version_type"], "date": x["date_published"],
                "mc": x["game_versions"], "loaders": x["loaders"], "downloads": x["downloads"], "changelog": cl})
        }).collect();
    out.sort_by(|a, b| b["date"].as_str().cmp(&a["date"].as_str()));
    Ok(out)
}

/// Download a specific version of the modpack this instance came from.
pub fn fetch_pack_version(project_id: &str, version_id: &str, tmp: &Path) -> Res<PackInfo> {
    let meta = api_get(&format!("/project/{project_id}"), &[])?;
    if meta.is_null() { return Err("That modpack wasn't found on Modrinth.".into()); }
    let v = api_get(&format!("/version/{version_id}"), &[])?;
    if v.is_null() || v["project_id"] != meta["id"] { return Err("That version doesn't belong to this modpack.".into()); }
    download_pack(&meta, &v, tmp)
}

/// Paths (relative to the instance) a pack puts on disk: its listed files plus everything in its overrides.
pub fn pack_file_list(info: &PackInfo) -> Vec<String> {
    let mut out: Vec<String> = info.index["files"].as_array().into_iter().flatten()
        .filter(|f| f["env"]["client"] != "unsupported").filter_map(|f| f["path"].as_str().map(String::from)).collect();
    if let Ok(f) = std::fs::File::open(&info.pack) {
        if let Ok(mut z) = zip::ZipArchive::new(f) {
            for i in 0..z.len() {
                let Ok(e) = z.by_index(i) else { continue };
                let name = e.name().to_string();
                let (head, rel) = name.split_once('/').unwrap_or((&name, ""));
                if (head == "overrides" || head == "client-overrides") && !rel.is_empty() && !e.is_dir() { out.push(rel.to_string()); }
            }
        }
    }
    out.sort(); out.dedup();
    out
}

fn file_matches(p: &Path, fe: &Value) -> bool {
    if !p.is_file() { return false; }
    let (algo, exp) = if let Some(h) = fe["hashes"]["sha512"].as_str() { ("sha512", h) } else if let Some(h) = fe["hashes"]["sha1"].as_str() { ("sha1", h) } else { return false };
    crate::util::hash_file(p, algo).map(|h| h.eq_ignore_ascii_case(exp)).unwrap_or(false)
}

/// Move an installed modpack instance to another version of the same pack (update or downgrade) in place:
/// files the new version no longer ships are removed, changed ones replaced, unchanged ones kept, and the
/// Minecraft version / loader follow the pack. Worlds, screenshots and mods the player added themselves stay.
pub fn switch_modpack(inst: &mut Instance, info: &PackInfo, log: &mut Log) -> Res<()> {
    let new_files: HashSet<String> = pack_file_list(info).into_iter().collect();
    let mut old_files: Vec<String> = inst.state["modpack"]["files"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
    if old_files.is_empty() {
        // installed before Orbit recorded the file list: rebuild it from the version that is installed now
        let pid = st(&inst.state["modpack"], "project_id");
        let cur = st(&inst.state["modpack"], "version");
        if !pid.is_empty() {
            let tmp = std::env::temp_dir().join(format!("orbit-oldpack-{:08x}", rand::random::<u32>()));
            let old = (|| -> Res<PackInfo> {
                let vs = pack_versions(&pid)?;
                let v = vs.iter().find(|x| x["version_number"].as_str() == Some(cur.as_str())).ok_or("old version not listed")?;
                fetch_pack_version(&pid, v["id"].as_str().unwrap_or(""), &tmp)
            })();
            if let Ok(o) = old { old_files = pack_file_list(&o); }
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }
    let old_set: HashSet<String> = old_files.iter().cloned().collect();
    let mut removed = 0;
    // 1. drop what the old version shipped and the new one doesn't (content folders only: configs and saves are left alone)
    for rel in &old_files {
        if new_files.contains(rel) { continue; }
        if dir_type(rel.split('/').next().unwrap_or("")).is_none() { continue; }
        let Ok(t) = safe_join(&inst.root, rel) else { continue };
        let dis = { let mut n = t.file_name().unwrap_or_default().to_os_string(); n.push(".disabled"); t.with_file_name(n) };
        let mut gone = false;
        for f in [&t, &dis] { if f.is_file() && std::fs::remove_file(f).is_ok() { gone = true; } }
        let keys: Vec<String> = inst.mods().iter().filter(|(k, m)| **k == format!("pack:{rel}") || content_path(inst, m) == t).map(|(k, _)| k.clone()).collect();
        for k in keys { inst.mods_mut().remove(&k); gone = true; }
        if gone { removed += 1; }
    }
    // entries that came straight from a pack file we couldn't place (no recorded list): drop the ones the new pack doesn't have
    if old_set.is_empty() {
        let stale: Vec<String> = inst.mods().iter().filter(|(_, m)| m["source"] == "modpack" && !m["relpath"].as_str().map(|r| new_files.contains(r)).unwrap_or(false)).map(|(k, _)| k.clone()).collect();
        for k in stale {
            if let Some(m) = inst.mods().get(&k).cloned() { let _ = std::fs::remove_file(content_path(inst, &m)); }
            inst.mods_mut().remove(&k); removed += 1;
        }
    }
    if removed > 0 { log.push(format!("Removed {removed} files the new version doesn't use")); }
    // 2. the instance follows the pack's game version and loader
    inst.state["config"]["mc"] = json!(info.mc);
    inst.state["config"]["loader"] = json!(info.loader);
    if info.loader_version.is_empty() { inst.state["config"].as_object_mut().map(|o| o.remove("loader_version")); } else { inst.state["config"]["loader_version"] = json!(info.loader_version); }
    inst.save()?;
    // 3. everything the new version lists (apply_modpack skips files that are already identical)
    apply_modpack(inst, info, log)
}

pub fn apply_modpack(inst: &mut Instance, info: &PackInfo, log: &mut Log) -> Res<()> {
    std::fs::create_dir_all(&inst.root).map_err(|e| e.to_string())?;
    let files: Vec<Value> = info.index["files"].as_array().cloned().unwrap_or_default();
    let wanted = |f: &Value| f["env"]["client"] != "unsupported";
    track_expected(files.iter().filter(|f| wanted(f)).count());
    let total = files.len();
    for (n, fe) in files.iter().enumerate() {
        if !wanted(fe) { continue; }
        let rel = fe["path"].as_str().unwrap_or("");
        let target = safe_join(&inst.root, rel)?;
        let urls: Vec<&str> = fe["downloads"].as_array().into_iter().flatten().filter_map(|u| u.as_str())
            .filter(|u| MRPACK_HOSTS.contains(&host_of(u).as_str())).collect();
        if urls.is_empty() { return Err(format!("No allowed download source for {rel}.")); }
        let (algo, exp) = if let Some(h) = fe["hashes"]["sha512"].as_str() { (Some("sha512"), Some(h)) }
            else if let Some(h) = fe["hashes"]["sha1"].as_str() { (Some("sha1"), Some(h)) } else { (None, None) };
        let parent = target.parent().unwrap_or(&inst.root).to_path_buf();
        let fname = target.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        // already here (enabled or switched off) and identical: keep it, and keep whatever entry already describes it
        let dis = { let mut n = target.file_name().unwrap_or_default().to_os_string(); n.push(".disabled"); target.with_file_name(n) };
        if file_matches(&target, fe) || file_matches(&dis, fe) {
            if !inst.mods().values().any(|m| content_path(inst, m) == target) {
                if let Some(t) = dir_type(rel.split('/').next().unwrap_or("")) {
                    let stem = Path::new(&fname).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    inst.mods_mut().insert(format!("pack:{rel}"), json!({"type": t, "title": stem, "slug": stem, "source": "modpack", "version_id": "",
                        "version": info.version, "filename": fname, "relpath": rel, "explicit": true}));
                }
            }
            log.push(format!("= [{}/{total}] {fname} (unchanged)", n + 1));
            continue;
        }
        for (k, u) in urls.iter().enumerate() {
            match download_file(u, &parent, &fname, algo, exp) { Ok(_) => break, Err(e) => if k == urls.len() - 1 { return Err(e); } }
        }
        let top = rel.split('/').next().unwrap_or("");
        if let Some(t) = dir_type(top) {
            // a replaced file (same path, new build): drop the entry that described the old one
            let old: Vec<String> = inst.mods().iter().filter(|(_, m)| content_path(inst, m) == target).map(|(k, _)| k.clone()).collect();
            for k in old { inst.mods_mut().remove(&k); }
            let stem = Path::new(&fname).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            inst.mods_mut().insert(format!("pack:{rel}"), json!({"type": t, "title": stem, "slug": stem, "source": "modpack", "version_id": "",
                "version": info.version, "filename": fname, "relpath": rel, "explicit": true}));
        }
        log.push(format!("+ [{}/{total}] {fname}", n + 1));
    }
    let f = std::fs::File::open(&info.pack).map_err(|e| e.to_string())?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
    for i in 0..z.len() {
        let mut e = z.by_index(i).map_err(|e| e.to_string())?;
        let name = e.name().to_string();
        let (head, rel) = name.split_once('/').unwrap_or((&name, ""));
        if !(head == "overrides" || head == "client-overrides") || rel.is_empty() || e.is_dir() { continue; }
        let t = safe_join(&inst.root, rel)?;
        if let Some(p) = t.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
        let mut out = std::fs::File::create(&t).map_err(|e| e.to_string())?;
        std::io::copy(&mut e, &mut out).map_err(|e| e.to_string())?;
    }
    inst.state["modpack"] = json!({"title": info.title, "version": info.version, "project_id": info.project_id, "version_id": info.version_id, "files": pack_file_list(info)});
    inst.save()?;
    // Match the pack's files (and any loose jars in its overrides) with Modrinth: real names, icons, update checks.
    // Not being able to reach Modrinth right now must not fail the install, so this is best effort.
    crate::gamedata::sync_untracked(inst);
    let linked = crate::importers::link_modrinth(inst);
    inst.state["mr_link"] = json!(1);
    inst.save()?;
    if linked["linked"].as_i64().unwrap_or(0) > 0 { log.push(format!("Matched {} of {} files with Modrinth", linked["linked"], linked["checked"])); }
    log.push(format!("Installed modpack {} {}", info.title, info.version));
    Ok(())
}

// ---------- search ----------

fn env_tag(client: &str, server: &str) -> Option<&'static str> {
    if client == "unsupported" && server != "unsupported" { return Some("server"); }
    if server == "unsupported" && client != "unsupported" { return Some("client"); }
    if !client.is_empty() && !server.is_empty() { return Some("client-or-server"); }
    None
}

pub fn search_projects(source: &str, query: &str, mc: &str, loader: &str, limit: usize, ptype: &str, offset: usize, sort: &str, cats: &[String]) -> Res {
    let mut out = vec![];
    let total;
    if source == "curseforge" {
        let sf = match sort { "downloads" => 6, "newest" => 11, "updated" => 3, _ => 2 };
        let mut p = vec![("gameId", CF_GAME_ID.to_string()), ("classId", type_class(ptype).to_string()), ("sortField", sf.to_string()),
            ("sortOrder", "desc".into()), ("pageSize", limit.to_string()), ("index", offset.to_string())];
        if !query.is_empty() { p.push(("searchFilter", query.into())); }
        if !mc.is_empty() { p.push(("gameVersion", mc.into())); }
        if let (Some(l), true) = (cf_loader(loader), ptype == "mod" || ptype == "modpack") { p.push(("modLoaderType", l.to_string())); }
        let res = cf_get("/mods/search", &p)?;
        total = res["pagination"]["totalCount"].as_i64().unwrap_or(0);
        for m in res["data"].as_array().into_iter().flatten() {
            let slug = st(m, "slug");
            out.push(json!({"source": "curseforge", "key": format!("cf:{}", m["id"]), "ident": m["id"].to_string(), "slug": slug, "title": m["name"],
                "author": m["authors"][0]["name"].as_str().unwrap_or("unknown"), "description": m["summary"].as_str().unwrap_or(""),
                "downloads": m["downloadCount"].as_f64().unwrap_or(0.0) as i64, "follows": m["thumbsUpCount"].as_i64().unwrap_or(0),
                "updated": m["dateModified"], "icon_url": m["logo"]["thumbnailUrl"],
                "categories": m["categories"].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str()).take(4).collect::<Vec<_>>(),
                "url": m["links"]["websiteUrl"].as_str().map(String::from).unwrap_or_else(|| format!("https://www.curseforge.com/minecraft/mc-mods/{slug}")),
                "downloadable": m["allowModDistribution"] != false}));
        }
    } else {
        let mut facets: Vec<Vec<String>> = vec![vec![format!("project_type:{}", if ptype == "datapack" { "mod" } else { ptype })]];
        if ptype == "datapack" { facets.push(vec!["categories:datapack".into()]); }
        else if ptype == "mod" || ptype == "modpack" {
            if ptype == "mod" { facets.push(vec!["categories!=datapack".into()]); }
            if !loader.is_empty() { facets.push(vec![format!("categories:{loader}")]); }
        }
        if !mc.is_empty() { facets.push(vec![format!("versions:{mc}")]); }
        for c in cats { facets.push(vec![format!("categories:{c}")]); }
        let idx = match sort { "downloads" | "follows" | "newest" | "updated" => sort, _ => "relevance" };
        let mut p = vec![("facets", serde_json::to_string(&facets).unwrap()), ("limit", limit.to_string()), ("index", idx.to_string())];
        if !query.is_empty() { p.push(("query", query.into())); }
        if offset > 0 { p.push(("offset", offset.to_string())); }
        let res = api_get("/search", &p)?;
        total = res["total_hits"].as_i64().unwrap_or(0);
        for h in res["hits"].as_array().into_iter().flatten() {
            let mut tags: Vec<String> = env_tag(h["client_side"].as_str().unwrap_or(""), h["server_side"].as_str().unwrap_or("")).map(String::from).into_iter().collect();
            tags.extend(h["display_categories"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)));
            let slug = st(h, "slug");
            out.push(json!({"source": "modrinth", "key": h["project_id"], "ident": slug, "slug": slug, "title": h["title"],
                "author": h["author"].as_str().unwrap_or("unknown"), "description": h["description"], "downloads": h["downloads"],
                "follows": h["follows"].as_i64().unwrap_or(0), "updated": h["date_modified"], "icon_url": h["icon_url"],
                "categories": tags.iter().take(5).collect::<Vec<_>>(), "more": tags.len().saturating_sub(5),
                "url": format!("https://modrinth.com/{}/{slug}", if ptype == "datapack" { "mod" } else { ptype }), "downloadable": true}));
        }
    }
    Ok(json!({"hits": out, "total": total}))
}

pub fn install_mod(inst: &mut Instance, source: &str, ident: &str, explicit: bool, ptype: &str, log: &mut Log) -> Res<()> {
    if !is_type(ptype) || ptype == "modpack" { return Err(format!("Can't add '{ptype}' to an instance.")); }
    let mut seen = HashSet::new();
    if source == "curseforge" { let id = cf_resolve(ident, ptype)?; install_cf(inst, id, explicit, &mut seen, ptype, log) }
    else { install_project(inst, ident, explicit, &mut seen, ptype, log) }
}

// ---------- dependencies of everything in mods/ (see deps.rs) ----------

pub fn deps_ctx(inst: &Instance) -> crate::deps::Ctx {
    let l = inst.loader();
    crate::deps::Ctx { mc: inst.mc(), platform: l.clone(), server: false, loaders: vec![l] }
}

struct InstInstaller<'a>(&'a mut Instance);
impl crate::deps::Installer for InstInstaller<'_> {
    fn installed(&self, pid: &str) -> Option<String> { self.0.mods().get(pid).map(|m| st(m, "title")) }
    fn install(&mut self, pid: &str, seen: &mut HashSet<String>, log: &mut Log) -> Res<()> { install_project(self.0, pid, false, seen, "mod", log) }
}

/// What the jars in this instance's mods folder need and don't have, without touching the network.
pub fn deps_report(inst: &Instance) -> Option<crate::deps::Report> {
    if inst.loader() == "vanilla" || inst.cfg().is_err() { return None; }
    Some(crate::deps::analyze(&crate::deps::scan_dir(&inst.dir_for(Some("mod"))), &deps_ctx(inst)))
}

/// Install every missing dependency of the mods in this instance, whatever way they got there.
/// None for instances that can't have mods.
pub fn ensure_deps(inst: &mut Instance, log: &mut Log) -> Option<crate::deps::Outcome> {
    if inst.loader() == "vanilla" || inst.cfg().is_err() { return None; }
    let (ctx, dir) = (deps_ctx(inst), inst.dir_for(Some("mod")));
    let out = crate::deps::ensure(&dir, &ctx, &mut InstInstaller(inst), log);
    let _ = inst.save();
    Some(out)
}

// ---------- updates / versions ----------

pub fn updatable(key: &str, m: &Value) -> bool {
    !(key.starts_with("pack:") || key.starts_with("tl:") || key.starts_with("imp:") || m["disabled"] == true)
}

fn ptype_loader(inst: &Instance, t: &str) -> Option<String> {
    let l = inst.loader();
    (t == "mod" && l != "vanilla").then_some(l)
}

pub fn par_map<T: Sync, R: Send>(items: &[T], workers: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let out: std::sync::Mutex<Vec<Option<R>>> = std::sync::Mutex::new((0..items.len()).map(|_| None).collect());
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..workers.min(items.len()).max(1) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i >= items.len() { break; }
                let r = f(&items[i]);
                out.lock().unwrap()[i] = Some(r);
            });
        }
    });
    out.into_inner().unwrap().into_iter().map(|x| x.unwrap()).collect()
}

pub fn check_updates(inst: &Instance) -> Vec<Value> {
    let mc = inst.mc();
    let items: Vec<(String, Value)> = inst.mods().iter().filter(|(k, m)| updatable(k, m)).map(|(k, m)| (k.clone(), m.clone())).collect();
    let res = par_map(&items, 8, |(key, m)| -> Option<Value> {
        let t = m["type"].as_str().unwrap_or("mod");
        let ld = ptype_loader(inst, t);
        if let Some(id) = key.strip_prefix("cf:") {
            if cf_key().is_empty() { return None; }
            let f = cf_best_file(id.parse().ok()?, &mc, ld.as_deref()).ok()??;
            if f["id"].to_string() != m["version_id"].as_str().unwrap_or("") {
                return Some(json!({"key": key, "new_id": f["id"].to_string(), "new": f["displayName"].as_str().map(String::from).unwrap_or_else(|| f["id"].to_string())}));
            }
        } else {
            let v = best_version(key, &mc, ld.as_deref()).ok()??;
            if v["id"] != m["version_id"] { return Some(json!({"key": key, "new_id": v["id"], "new": v["version_number"]})); }
        }
        None
    });
    items.iter().zip(res).filter_map(|((_, m), r)| {
        r.map(|mut r| { r["title"] = m["title"].clone(); r["icon_url"] = m["icon_url"].clone(); r["current"] = json!(m["version"].as_str().unwrap_or("")); r })
    }).collect()
}

pub fn list_versions(inst: &Instance, key: &str) -> Res<Vec<Value>> {
    let m = inst.mods().get(key).ok_or("That item isn't in this instance.")?.clone();
    let mc = inst.mc();
    let ld = ptype_loader(inst, m["type"].as_str().unwrap_or("mod"));
    let mut out = vec![];
    if let Some(id) = key.strip_prefix("cf:") {
        let mut p = vec![("gameVersion", mc), ("pageSize", "50".to_string())];
        if let Some(l) = ld.as_deref().and_then(cf_loader) { p.push(("modLoaderType", l.to_string())); }
        let res = cf_get(&format!("/mods/{id}/files"), &p)?;
        let mut files: Vec<Value> = res["data"].as_array().cloned().unwrap_or_default().into_iter().filter(|f| f["isAvailable"] != false).collect();
        files.sort_by(|a, b| st(b, "fileDate").cmp(&st(a, "fileDate")));
        for f in files {
            let ty = match f["releaseType"].as_i64() { Some(2) => "beta", Some(3) => "alpha", _ => "release" };
            out.push(json!({"id": f["id"].to_string(), "name": f["displayName"].as_str().unwrap_or(""), "number": f["displayName"].as_str().unwrap_or(""),
                "type": ty, "date": f["fileDate"], "current": f["id"].to_string() == m["version_id"].as_str().unwrap_or("")}));
        }
    } else {
        let mut p = vec![("game_versions", jl(&[mc]))];
        if let Some(l) = &ld { p.push(("loaders", jl(&[l]))); }
        let vs = api_get(&format!("/project/{key}/version"), &p)?;
        for v in vs.as_array().into_iter().flatten() {
            out.push(json!({"id": v["id"], "name": v["name"].as_str().unwrap_or(""), "number": v["version_number"], "type": v["version_type"],
                "date": v["date_published"], "current": v["id"] == m["version_id"]}));
        }
    }
    Ok(out)
}

pub fn install_version(inst: &mut Instance, key: &str, version_id: &str, ptype: &str, log: &mut Log) -> Res<()> {
    if !inst.mods().contains_key(key) {
        if key.starts_with("cf:") || !is_type(ptype) || ptype == "modpack" { return Err("That item isn't in this instance.".into()); }
        let meta = api_get(&format!("/project/{key}"), &[])?;
        if meta.is_null() { return Err("That project no longer exists.".into()); }
        inst.mods_mut().insert(key.to_string(), json!({"type": ptype, "icon_url": meta["icon_url"], "title": meta["title"], "slug": meta["slug"],
            "source": "modrinth", "version_id": "", "version": "", "filename": "", "explicit": true}));
    }
    let m = inst.mods()[key].clone();
    let dest = inst.dir_for(m["type"].as_str());
    let (filename, nid, nver);
    if let Some(id) = key.strip_prefix("cf:") {
        let f = cf_get(&format!("/mods/{id}/files/{version_id}"), &[])?["data"].clone();
        let url = f["downloadUrl"].as_str().filter(|u| !u.is_empty()).ok_or("The author doesn't allow downloading that version from other apps.")?;
        let (algo, exp) = cf_hash(&f);
        filename = download_file(url, &dest, f["fileName"].as_str().unwrap_or(""), algo, exp.as_deref())?;
        nid = f["id"].to_string(); nver = f["displayName"].as_str().map(String::from).unwrap_or_else(|| f["id"].to_string());
    } else {
        let v = api_get(&format!("/version/{version_id}"), &[])?;
        if v.is_null() { return Err("That version no longer exists.".into()); }
        let f = primary_file(&v).ok_or("That version has no files.")?.clone();
        filename = download_file(f["url"].as_str().unwrap_or(""), &dest, f["filename"].as_str().unwrap_or(""), Some("sha512"), f["hashes"]["sha512"].as_str())?;
        nid = st(&v, "id"); nver = st(&v, "version_number");
    }
    if m["filename"].as_str().map(|f| !f.is_empty() && f != filename).unwrap_or(false) {
        let old = content_path(inst, &m);
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::remove_file(disabled_name(&old));
    }
    if m["disabled"] == true {
        let n = dest.join(&filename);
        let _ = std::fs::rename(&n, disabled_name(&n));
    }
    let e = inst.mods_mut().get_mut(key).unwrap();
    e["version_id"] = json!(nid); e["version"] = json!(nver); e["filename"] = json!(filename);
    e.as_object_mut().unwrap().remove("relpath");
    inst.save()?;
    log.push(format!("~ {} {nver}", st(&m, "title")));
    Ok(())
}

pub fn project_page(ident: &str) -> Res {
    let p = api_get(&format!("/project/{}", quote(ident)), &[])?;
    if p.is_null() { return Err("That project wasn't found on Modrinth.".into()); }
    let team = api_get(&format!("/project/{}/members", st(&p, "id")), &[])?;
    let mut team: Vec<Value> = team.as_array().cloned().unwrap_or_default();
    team.sort_by_key(|t| t["ordering"].as_i64().unwrap_or(0));
    let lic = &p["license"];
    let cats: Vec<Value> = p["categories"].as_array().into_iter().flatten().chain(p["additional_categories"].as_array().into_iter().flatten()).cloned().collect();
    Ok(json!({"id": p["id"], "slug": p["slug"], "title": p["title"], "description": p["description"], "body": p["body"].as_str().unwrap_or(""),
        "icon_url": p["icon_url"], "downloads": p["downloads"], "followers": p["followers"].as_i64().unwrap_or(0), "type": p["project_type"],
        "categories": cats, "game_versions": p["game_versions"].as_array().cloned().unwrap_or_default(), "loaders": p["loaders"].as_array().cloned().unwrap_or_default(),
        "client": p["client_side"], "server": p["server_side"],
        "license": lic["name"].as_str().filter(|s| !s.is_empty()).or(lic["id"].as_str()).unwrap_or(""),
        "published": p["published"], "updated": p["updated"],
        "links": {"issues": p["issues_url"], "source": p["source_url"], "wiki": p["wiki_url"], "discord": p["discord_url"], "donate": p["donation_urls"][0]["url"].as_str().unwrap_or("")},
        "url": format!("https://modrinth.com/{}/{}", st(&p, "project_type"), st(&p, "slug")),
        "members": team.iter().map(|t| json!({"name": t["user"]["username"].as_str().unwrap_or("?"), "avatar": t["user"]["avatar_url"],
            "role": t["role"].as_str().unwrap_or(""), "owner": t["is_owner"] == true || t["role"] == "Owner"})).collect::<Vec<_>>()}))
}

pub fn project_versions(ident: &str) -> Res {
    let vs = api_get(&format!("/project/{}/version", quote(ident)), &[])?;
    Ok(json!(vs.as_array().into_iter().flatten().map(|v| json!({"id": v["id"], "number": v["version_number"], "name": v["name"].as_str().unwrap_or(""),
        "type": v["version_type"], "date": v["date_published"], "downloads": v["downloads"].as_i64().unwrap_or(0),
        "game_versions": v["game_versions"], "loaders": v["loaders"]})).collect::<Vec<_>>()))
}

pub fn cmd_update(inst: &mut Instance, keys: Option<&HashSet<String>>, log: &mut Log) -> Res<()> {
    let list: Vec<(String, Value)> = inst.mods().iter().map(|(k, m)| (k.clone(), m.clone())).collect();
    track_expected(keys.map(|k| k.len()).unwrap_or_else(|| list.iter().filter(|(k, m)| updatable(k, m)).count()));
    let mut seen = HashSet::new();
    for (key, m) in list {
        if !updatable(&key, &m) || keys.map(|k| !k.contains(&key)).unwrap_or(false) { continue; }
        let t = m["type"].as_str().unwrap_or("mod").to_string();
        let explicit = m["explicit"].as_bool().unwrap_or(false);
        if let Some(id) = key.strip_prefix("cf:") { install_cf(inst, id.parse().map_err(|_| "bad CurseForge id")?, explicit, &mut seen, &t, log)?; }
        else { install_project(inst, &key, explicit, &mut seen, &t, log)?; }
    }
    inst.save()
}

pub fn cmd_remove(inst: &mut Instance, names: &[String], log: &mut Log) -> Res<()> {
    for name in names {
        let found = inst.mods().iter().find(|(k, m)| *k == name || m["slug"].as_str() == Some(name)).map(|(k, _)| k.clone());
        let Some(k) = found else { log.push(format!("! '{name}' is not installed")); continue };
        let m = inst.mods_mut().remove(&k).unwrap();
        let p = content_path(inst, &m);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(disabled_name(&p));
        log.push(format!("- removed {}", st(&m, "title")));
    }
    inst.save()
}

// ---- Modrinth + CurseForge API clients ----

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

/// Shared reply handling for the Modrinth and CurseForge calls: 404 -> Null, other failures -> readable errors.
/// `service` names the API in errors; `key_rejected` is the message for a 401/403 (None = treat like any status error).
fn api_reply(r: Result<ureq::Response, ureq::Error>, service: &str, key_rejected: Option<&str>) -> Res {
    match r {
        Ok(r) => r.into_json().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(404, _)) => Ok(Value::Null),
        Err(ureq::Error::Status(401 | 403, _)) if key_rejected.is_some() => Err(key_rejected.unwrap_or_default().to_string()),
        Err(ureq::Error::Status(c, _)) => Err(format!("{service} API error {c}")),
        Err(ureq::Error::Transport(t)) => Err(format!("Network error: {t}")),
    }
}

/// Modrinth GET. 404 -> Null.
pub fn api_get(path: &str, params: &[(&str, String)]) -> Res {
    let mut req = agent(30).get(&format!("{}{path}", api_base()));
    for (k, v) in pairs(params) { req = req.query(&k, &v); }
    api_reply(req.call(), "Modrinth", None)
}

pub fn api_post(path: &str, body: &Value) -> Res {
    api_reply(agent(60).post(&format!("{}{path}", api_base())).send_json(body), "Modrinth", None)
}

pub fn cf_get(path: &str, params: &[(&str, String)]) -> Res {
    let key = cf_key();
    if key.is_empty() {
        return Err("CurseForge needs an API key. Get one from console.curseforge.com and enter it in the app.".into());
    }
    let mut req = agent(30).get(&format!("{CF_API}{path}")).set("x-api-key", &key).set("Accept", "application/json");
    for (k, v) in pairs(params) { req = req.query(&k, &v); }
    api_reply(req.call(), "CurseForge", Some("CurseForge rejected the API key. Check it and try again."))
}

pub fn host_allowed(url: &str) -> bool {
    url.starts_with("https://") && MRPACK_HOSTS.contains(&host_of(url).as_str())
}

// ---- .mrpack export (format version 1) ----
// Export an Orbit instance as a Modrinth modpack (.mrpack), format version 1.
// Mods/packs found on Modrinth (by SHA-1) become downloads; everything else goes in overrides/.

const HOSTED_DIRS: [&str; 4] = ["mods", "resourcepacks", "shaderpacks", "datapacks"];

fn is_internal(rel: &str) -> bool {
    let rel = rel.replace('\\', "/");
    let rel = rel.trim_matches('/');
    if rel == STATE_NAME { return true; }
    !rel.contains('/') && regex::Regex::new(r"(?i)^icon(-\d+)?\.(png|jpe?g|webp|gif|svg)$").unwrap().is_match(rel)
}

fn loader_dependency(inst: &Instance) -> Result<Map<String, Value>, String> {
    let cfg = inst.config();
    let loader = cfg["loader"].as_str().unwrap_or("vanilla");
    let key = match loader {
        "vanilla" => return Ok(Map::new()),
        "fabric" => "fabric-loader", "quilt" => "quilt-loader", "forge" => "forge", "neoforge" => "neoforge",
        o => return Err(format!("Unknown loader {o}.")),
    };
    let mut v = cfg["loader_version"].as_str().unwrap_or("").to_string();
    if v.is_empty() {
        let mc = cfg["mc"].as_str().unwrap_or("");
        v = crate::meta::loader_versions(loader, mc)
            .map_err(|e| format!("Couldn't work out which {} version this instance uses: {e}", crate::meta::label(loader)))?["stable"]
            .as_str().unwrap_or("").to_string();
    }
    let mut m = Map::new();
    m.insert(key.into(), json!(v));
    Ok(m)
}

fn walk(root: &Path, include: &[String]) -> Vec<String> {
    let Ok(root) = root.canonicalize() else { return vec![] };
    let (mut seen, mut out) = (HashSet::new(), vec![]);
    for raw in include {
        let rel = raw.replace('\\', "/");
        let rel = rel.trim_matches('/');
        if rel.is_empty() || rel.split('/').any(|p| p == "..") { continue; }
        let Ok(p) = root.join(rel).canonicalize() else { continue };
        if p == root || !p.starts_with(&root) { continue; }
        let mut files: Vec<PathBuf> = vec![];
        if p.is_file() { files.push(p.clone()); } else if p.is_dir() { collect(&p, &mut files); }
        for f in files {
            if f.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(true) { continue; }
            let Ok(r) = f.strip_prefix(&root) else { continue };
            let r = r.to_string_lossy().replace('\\', "/");
            if !is_internal(&r) && seen.insert(r.clone()) { out.push(r); }
        }
    }
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut items: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    items.sort();
    for p in items {
        if p.is_dir() { collect(&p, out) } else if p.is_file() { out.push(p) }
    }
}

fn hashes(path: &Path) -> std::io::Result<(String, String)> {
    let (mut s1, mut s512) = (Sha1::new(), Sha512::new());
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 { break; }
        s1.update(&buf[..n]);
        s512.update(&buf[..n]);
    }
    Ok((hex::encode(s1.finalize()), hex::encode(s512.finalize())))
}

/// sha1 -> (download url, project object) for files Modrinth has.
fn lookup(sha1s: &[String]) -> Result<HashMap<String, (String, Value)>, String> {
    let mut found = Map::new();
    for chunk in sha1s.chunks(200) {
        if let Some(o) = api_post("/version_files", &json!({"hashes": chunk, "algorithm": "sha1"}))?.as_object() {
            found.extend(o.clone());
        }
    }
    let mut pids: Vec<String> = found.values().filter_map(|v| v["project_id"].as_str().map(String::from)).collect();
    pids.sort();
    pids.dedup();
    let mut projects: HashMap<String, Value> = HashMap::new();
    for chunk in pids.chunks(100) {
        let ids = serde_json::to_string(chunk).unwrap();
        for p in api_get("/projects", &[("ids", ids)])?.as_array().into_iter().flatten() {
            if let Some(id) = p["id"].as_str() { projects.insert(id.to_string(), p.clone()); }
        }
    }
    let mut out = HashMap::new();
    for (h, v) in found {
        let file = v["files"].as_array().into_iter().flatten().find(|f| f["hashes"]["sha1"].as_str() == Some(&h));
        if let Some(url) = file.and_then(|f| f["url"].as_str()).filter(|u| host_allowed(u)) {
            let proj = projects.get(v["project_id"].as_str().unwrap_or("")).cloned().unwrap_or(json!({}));
            out.insert(h, (url.to_string(), proj));
        }
    }
    Ok(out)
}

pub fn build_mrpack(inst: &Instance, dest: &Path, name: &str, version: &str, description: &str, include: &[String]) -> Result<Value, String> {
    let (name, version) = (name.trim(), version.trim());
    if name.is_empty() { return Err("Give the modpack a name.".into()); }
    if version.is_empty() { return Err("Give the modpack a version number.".into()); }
    let mut deps = Map::new();
    deps.insert("minecraft".into(), inst.config()["mc"].clone());
    deps.extend(loader_dependency(inst)?);
    let paths = walk(&inst.root, include);
    if paths.is_empty() { return Err("Nothing is selected to export.".into()); }

    let mut hs: HashMap<String, (String, String)> = HashMap::new();
    for rel in &paths {
        let top = rel.split('/').next().unwrap_or("");
        let lower = rel.to_lowercase();
        if HOSTED_DIRS.contains(&top) && rel.matches('/').count() == 1 && (lower.ends_with(".jar") || lower.ends_with(".zip")) {
            if let Ok(h) = hashes(&inst.root.join(rel)) { hs.insert(rel.clone(), h); }
        }
    }
    let (mut known, mut warning) = (HashMap::new(), String::new());
    if !hs.is_empty() {
        let all: Vec<String> = hs.values().map(|h| h.0.clone()).collect();
        match lookup(&all) {
            Ok(k) => known = k,
            Err(e) => warning = format!("Couldn't reach Modrinth ({e}), so every file was packed into the modpack itself."),
        }
    }

    let (mut files, mut embedded) = (vec![], vec![]);
    for rel in &paths {
        let hit = hs.get(rel).and_then(|h| known.get(&h.0).map(|k| (h, k)));
        let Some((h, (url, proj))) = hit else { embedded.push(rel.clone()); continue };
        let side = |k: &str| proj[k].as_str().unwrap_or("required").to_string();
        files.push(json!({"path": rel, "hashes": {"sha1": h.0, "sha512": h.1},
            "env": {"client": side("client_side"), "server": side("server_side")},
            "downloads": [url],
            "fileSize": std::fs::metadata(inst.root.join(rel)).map(|m| m.len()).unwrap_or(0)}));
    }

    let mut index = json!({"formatVersion": 1, "game": "minecraft", "versionId": version, "name": name,
                           "files": files, "dependencies": deps});
    let desc = description.trim();
    if !desc.is_empty() { index["summary"] = json!(desc.chars().take(1000).collect::<String>()); }

    if let Some(parent) = dest.parent() { std::fs::create_dir_all(parent).map_err(|e| e.to_string())?; }
    let tmp = dest.with_file_name(format!("{}.part", dest.file_name().unwrap_or_default().to_string_lossy()));
    let write = || -> Result<(), String> {
        let mut z = zip::ZipWriter::new(std::fs::File::create(&tmp).map_err(|e| e.to_string())?);
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        z.start_file("modrinth.index.json", opts).map_err(|e| e.to_string())?;
        z.write_all(serde_json::to_string_pretty(&index).unwrap().as_bytes()).map_err(|e| e.to_string())?;
        for rel in &embedded {
            z.start_file(format!("overrides/{rel}"), opts).map_err(|e| e.to_string())?;
            let mut f = std::fs::File::open(inst.root.join(rel)).map_err(|e| e.to_string())?;
            std::io::copy(&mut f, &mut z).map_err(|e| e.to_string())?;
        }
        z.finish().map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, dest).map_err(|e| e.to_string())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("Couldn't write {}: {e}", dest.display()));
    }
    Ok(json!({"path": dest.to_string_lossy(), "hosted": files.len(), "embedded": embedded.len(),
              "size": std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0), "warning": warning}))
}
