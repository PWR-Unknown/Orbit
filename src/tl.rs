//! Version profiles, loader versions, TLauncher export/import (port of tlauncher.py).
use crate::mods::{is_loader, Log};
use crate::store::Instance;
use crate::util::{download_file, get_json, get_text, Res};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
pub const MARKER: &str = "quarry-export.json";
pub const CONTENT_DIRS: [&str; 4] = ["mods", "resourcepacks", "shaderpacks", "datapacks"];
pub const CONTENT_EXT: [&str; 3] = [".jar", ".zip", ".disabled"];
const SKIP_DIRS: [&str; 9] = ["versions", "libraries", "assets", "runtime", "logs", "crash-reports", "saves", "natives", "bin"];

pub fn meta_url(loader: &str) -> Option<&'static str> {
    match loader { "fabric" => Some("https://meta.fabricmc.net/v2/versions/loader"), "quilt" => Some("https://meta.quiltmc.org/v3/versions/loader"), _ => None }
}
pub fn label(l: &str) -> &str { match l { "fabric" => "Fabric", "quilt" => "Quilt", "forge" => "Forge", "neoforge" => "NeoForge", o => o } }

pub fn has_ext(name: &str) -> bool { let l = name.to_lowercase(); CONTENT_EXT.iter().any(|e| l.ends_with(e)) }

pub fn vanilla_profile(mc: &str) -> Res {
    let m = get_json(MANIFEST)?;
    let e = m["versions"].as_array().into_iter().flatten().find(|v| v["id"] == mc).ok_or_else(|| format!("Minecraft {mc} isn't in Mojang's version list."))?;
    get_json(e["url"].as_str().unwrap_or(""))
}

fn lib_key(lib: &Value) -> String {
    let n = lib["name"].as_str().unwrap_or("");
    let p: Vec<&str> = n.split(':').collect();
    let mut k: Vec<&str> = p.iter().take(2).cloned().collect();
    k.extend(p.iter().skip(3));
    k.join(":")
}

/// Standalone profile = Mojang's profile + the loader's libraries/arguments.
pub fn merge(vanilla: &Value, md: &Value, name: &str) -> Res {
    // Minecraft 1.12 and older describe their command line with one `minecraftArguments` string (a loader's replaces
    // Mojang's, because Forge's already carries every argument plus its --tweakClass); 1.13+ use `arguments` lists.
    let legacy = vanilla.get("arguments").is_none() && vanilla.get("minecraftArguments").is_some();
    if !legacy && (vanilla.get("arguments").is_none() || md.get("arguments").is_none()) {
        return Err("This Minecraft version's profile has neither `arguments` nor `minecraftArguments`.".into());
    }
    let mut out: Map<String, Value> = vanilla.as_object().cloned().unwrap_or_default();
    out.remove("inheritsFrom");
    out.insert("id".into(), json!(name));
    out.insert("type".into(), json!("modified"));
    out.insert("mainClass".into(), md["mainClass"].clone());
    if legacy {
        let mine = md["minecraftArguments"].as_str().or_else(|| vanilla["minecraftArguments"].as_str()).unwrap_or("");
        out.insert("minecraftArguments".into(), json!(mine));
        out.remove("arguments");
    } else {
        let mut args = Map::new();
        for k in ["jvm", "game"] {
            let mut v = vanilla["arguments"][k].as_array().cloned().unwrap_or_default();
            v.extend(md["arguments"][k].as_array().cloned().unwrap_or_default());
            args.insert(k.into(), Value::Array(v));
        }
        out.insert("arguments".into(), Value::Object(args));
    }
    let (mut seen, mut libs) = (std::collections::HashSet::new(), vec![]);
    let all = md["libraries"].as_array().into_iter().flatten().chain(vanilla["libraries"].as_array().into_iter().flatten());
    for l in all { if seen.insert(lib_key(l)) { libs.push(l.clone()); } }
    out.insert("libraries".into(), Value::Array(libs));
    Ok(Value::Object(out))
}

// ---------- Fabric / Quilt ----------

fn meta_list(loader: &str, mc: &str) -> Res<Vec<Value>> {
    let v = get_json(&format!("{}/{mc}", meta_url(loader).unwrap()))?;
    match v.as_array() { Some(a) if !a.is_empty() => Ok(a.clone()), _ => Err(format!("{} has no loader for Minecraft {mc}.", label(loader))) }
}

fn is_stable(v: &Value) -> bool {
    let l = &v["loader"];
    l["stable"].as_bool().unwrap_or_else(|| !l["version"].as_str().unwrap_or("").contains('-'))
}

pub fn meta_profile(loader: &str, mc: &str, version: Option<&str>) -> Res<(Value, String)> {
    let base = meta_url(loader).ok_or("not a meta loader")?;
    let pick = match version {
        Some(v) => v.to_string(),
        None => { let vs = meta_list(loader, mc)?; vs.iter().find(|v| is_stable(v)).unwrap_or(&vs[0])["loader"]["version"].as_str().unwrap_or("").to_string() }
    };
    Ok((get_json(&format!("{base}/{mc}/{pick}/profile/json"))?, pick))
}

// ---------- Forge / NeoForge ----------

fn vkey(s: &str) -> Vec<u64> { s.split(['.', '-']).map(|x| x.parse().unwrap_or(0)).collect() }

fn neo_prefix(mc: &str) -> String {
    let p: Vec<&str> = mc.split('.').collect();
    if p[0] == "1" && p.len() > 1 { format!("{}.{}.", p[1], p.get(2).unwrap_or(&"0")) } else { format!("{mc}.") }
}

/// Forge's maven folder name for a build. A few old versions repeat the Minecraft version at the end
/// (`1.7.10-10.13.4.1614-1.7.10`, `1.8.9-11.15.1.2318-1.8.9`); promotions only list the bare build number.
fn forge_full(mc: &str, version: &str) -> String {
    if version.contains('-') { return format!("{mc}-{version}"); }
    if matches!(mc, "1.7.10" | "1.8.9" | "1.8.8" | "1.8") && !version.starts_with(&format!("{mc}-")) { return format!("{mc}-{version}-{mc}"); }
    format!("{mc}-{version}")
}

fn installer_url(loader: &str, mc: &str, version: &str) -> String {
    if loader == "forge" {
        let v = forge_full(mc, version);
        format!("https://maven.minecraftforge.net/net/minecraftforge/forge/{v}/forge-{v}-installer.jar")
    } else {
        format!("https://maven.neoforged.net/releases/net/neoforged/neoforge/{version}/neoforge-{version}-installer.jar")
    }
}

fn neo_versions() -> Res<Vec<String>> {
    let v = get_json("https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge")?;
    Ok(v["versions"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect())
}

fn forge_promos() -> Res<Map<String, Value>> {
    Ok(get_json("https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json")?["promos"].as_object().cloned().unwrap_or_default())
}

pub fn installer_info(loader: &str, mc: &str, version: Option<&str>) -> Res<(String, String)> {
    if let Some(v) = version { return Ok((v.to_string(), installer_url(loader, mc, v))); }
    if loader == "forge" {
        let promos = forge_promos()?;
        let b = promos.get(&format!("{mc}-recommended")).or_else(|| promos.get(&format!("{mc}-latest"))).and_then(|v| v.as_str())
            .ok_or_else(|| format!("Forge has no build for Minecraft {mc}."))?.to_string();
        let u = installer_url(loader, mc, &b);
        return Ok((b, u));
    }
    if mc == "1.20.1" { return Err("NeoForge starts at Minecraft 1.20.2. Use Forge for 1.20.1.".into()); }
    let pre = neo_prefix(mc);
    let cands: Vec<String> = neo_versions()?.into_iter().filter(|v| v.starts_with(&pre)).collect();
    if cands.is_empty() { return Err(format!("NeoForge has no build for Minecraft {mc}.")); }
    let stable: Vec<&String> = cands.iter().filter(|c| !c.contains('-')).collect();
    let pool: Vec<&String> = if stable.is_empty() { cands.iter().collect() } else { stable };
    let v = pool.into_iter().max_by_key(|c| vkey(c)).unwrap().clone();
    let u = installer_url(loader, mc, &v);
    Ok((v, u))
}

/// Every loader build for a Minecraft version: {versions (newest first), stable, latest}.
pub fn loader_versions(loader: &str, mc: &str) -> Res {
    if meta_url(loader).is_some() {
        let vs = meta_list(loader, mc)?;
        let names: Vec<String> = vs.iter().map(|v| v["loader"]["version"].as_str().unwrap_or("").to_string()).collect();
        let stable = vs.iter().find(|v| is_stable(v)).map(|v| v["loader"]["version"].as_str().unwrap_or("").to_string()).unwrap_or_else(|| names[0].clone());
        return Ok(json!({"versions": names, "stable": stable, "latest": names[0]}));
    }
    if loader == "forge" {
        let mut names: Vec<String> = vec![];
        if let Ok(meta) = get_json("https://files.minecraftforge.net/net/minecraftforge/forge/maven-metadata.json") {
            let pre = format!("{mc}-");
            names = meta[mc].as_array().into_iter().flatten().filter_map(|x| x.as_str()).filter_map(|x| x.strip_prefix(&pre).map(String::from)).collect();
        }
        let promos = match forge_promos() { Ok(p) => p, Err(e) => { if names.is_empty() { return Err(e); } Map::new() } };
        let rec = promos.get(&format!("{mc}-recommended")).and_then(|v| v.as_str()).map(String::from);
        let lat = promos.get(&format!("{mc}-latest")).and_then(|v| v.as_str()).map(String::from);
        names.extend(rec.iter().cloned().chain(lat.iter().cloned()));
        names.sort_by(|a, b| vkey(b).cmp(&vkey(a)));
        names.dedup();
        if names.is_empty() { return Err(format!("Forge has no build for Minecraft {mc}.")); }
        return Ok(json!({"versions": names, "stable": rec.unwrap_or_else(|| names[0].clone()), "latest": names[0]}));
    }
    if mc == "1.20.1" { return Err("NeoForge starts at Minecraft 1.20.2. Use Forge for 1.20.1.".into()); }
    let pre = neo_prefix(mc);
    let mut names: Vec<String> = neo_versions()?.into_iter().filter(|v| v.starts_with(&pre)).collect();
    names.sort_by(|a, b| vkey(b).cmp(&vkey(a)));
    if names.is_empty() { return Err(format!("NeoForge has no build for Minecraft {mc}.")); }
    let stable = names.iter().find(|v| !v.contains('-')).cloned().unwrap_or_else(|| names[0].clone());
    Ok(json!({"versions": names, "stable": stable, "latest": names[0]}))
}

fn installed_profile(root: &Path, loader: &str, before: &[String]) -> Res {
    let vdir = root.join("versions");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&vdir).map_err(|e| e.to_string())?.flatten().map(|e| e.path())
        .filter(|d| d.is_dir() && d.join(format!("{}.json", d.file_name().unwrap().to_string_lossy())).exists() && !d.join(MARKER).exists()).collect();
    let fresh: Vec<PathBuf> = dirs.iter().filter(|d| !before.contains(&d.file_name().unwrap().to_string_lossy().into_owned())).cloned().collect();
    if !fresh.is_empty() { dirs = fresh; }
    let word = |n: &str| { let l = n.to_lowercase(); l.contains(loader) && (loader != "forge" || !l.contains("neoforge")) };
    dirs.retain(|d| word(&d.file_name().unwrap().to_string_lossy()));
    let best = dirs.into_iter().max_by_key(|d| {
        let f = d.join(format!("{}.json", d.file_name().unwrap().to_string_lossy()));
        std::fs::metadata(f).and_then(|m| m.modified()).ok()
    }).ok_or("The installer finished but I can't find the profile it created.")?;
    let f = best.join(format!("{}.json", best.file_name().unwrap().to_string_lossy()));
    serde_json::from_str(&std::fs::read_to_string(f).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// Java that runs a loader's installer: the one Minecraft itself needs (Java 8 for 1.12 and older, which old Forge
/// installers and old Forge builds require), else whatever Java is on the PATH.
fn installer_java(loader: &str, mc: &str) -> Res<PathBuf> {
    if let Ok(p) = vanilla_profile(mc) {
        let st = crate::store::settings();
        if let Ok(j) = crate::launcher::find_java(&p, &st, &crate::store::meta_dir(), &|_: &str, _: f64| {}) { return Ok(j); }
    }
    find_java_on_path().ok_or_else(|| format!("Java is needed to set up {}. Install Java, then try again.", label(loader)))
}

pub fn find_java_on_path() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(exe)).find(|f| f.is_file()))
}

/// Run the official Forge/NeoForge installer (needs Java) and read the profile it made.
pub fn installer_profile(loader: &str, mc: &str, root: &Path, version: Option<&str>) -> Res<(Value, String)> {
    let (version, url) = installer_info(loader, mc, version)?;
    let java = installer_java(loader, mc)?;
    let tmp = std::env::temp_dir().join(format!("orbit-installer-{:08x}", rand::random::<u32>()));
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let res = (|| -> Res<(Value, String)> {
        let expected = get_text(&format!("{url}.sha1")).ok().and_then(|t| t.split_whitespace().next().map(String::from));
        let jar = download_file(&url, &tmp, url.rsplit('/').next().unwrap(), expected.as_ref().map(|_| "sha1"), expected.as_deref())?;
        std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
        let lp = root.join("launcher_profiles.json");
        if !lp.exists() { std::fs::write(&lp, r#"{"profiles": {}}"#).map_err(|e| e.to_string())?; }
        let vdir = root.join("versions");
        let before: Vec<String> = std::fs::read_dir(&vdir).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        let mut cmd = std::process::Command::new(&java);
        cmd.arg("-jar").arg(tmp.join(&jar)).arg("--installClient").arg(root).current_dir(&tmp);
        #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }
        let out = cmd.output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            let tail: Vec<&str> = text.trim().lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
            return Err(format!("The {} installer failed: {}", label(loader), tail.join(" | ")));
        }
        Ok((installed_profile(root, loader, &before)?, version.clone()))
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

// ---------- export ----------

fn pick_name(root: &Path, display: &str, iid: &str) -> Res<String> {
    let base = Regex::new(r"[^A-Za-z0-9._ -]+").unwrap().replace_all(display, "").trim_matches([' ', '.']).to_string();
    let base = if base.is_empty() { iid.to_string() } else { base };
    let suffix = &iid[iid.len().saturating_sub(4)..];
    for cand in [base.clone(), format!("{base}-orbit"), format!("{base}-{suffix}")] {
        let d = root.join("versions").join(&cand);
        if !d.exists() { return Ok(cand); }
        if let Some(i) = std::fs::read_to_string(d.join(MARKER)).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            if i["instance"] == iid { return Ok(cand); }
        }
    }
    Err(format!("Can't find a free version name for '{base}' in TLauncher's folder."))
}

fn sidecar(name: &str, mc: &str, loader: &str, lv: &str) -> Value {
    let mut vtype = json!({"name": loader});
    if loader == "forge" { vtype["id"] = json!(1); }
    let stamp = -((crate::store::now() * 1000.0) as i64);
    json!({"activateSkinCapeForUserVersion": false, "skipHashsumValidation": false, "additionalFiles": [], "tlauncherVersion": 0,
        "modpack": {"modpackMemory": false, "memory": 0, "id": stamp, "name": name, "favorite": false,
            "version": {"mods": [], "resourcePacks": [], "maps": [], "shaderpacks": [], "gameVersionDTO": {"name": mc},
                "minecraftVersionName": {"name": lv}, "id": stamp - 1, "name": "1.0", "available": false, "remove": false, "minecraftVersionTypes": [vtype]},
            "userInstall": false, "populateStatus": false, "available": false, "parser": false, "notExists": false, "totalComments": 0},
        "source": "local_version_repo", "jar": mc, "skinVersion": false})
}

pub fn export(inst: &Instance, iid: &str, root: &Path) -> Res {
    let cfg = inst.cfg()?;
    let (mc, loader) = (cfg["mc"].as_str().unwrap_or("").to_string(), cfg["loader"].as_str().unwrap_or("").to_string());
    if loader == "vanilla" {
        return Err("Vanilla instances don't need exporting: pick the release in TLauncher and point its game folder at this instance.".into());
    }
    if !root.is_dir() { return Err(format!("{} doesn't exist. Set your TLauncher folder in Settings > TLauncher.", root.display())); }
    let name = pick_name(root, inst.state["name"].as_str().unwrap_or(iid), iid)?;
    let vanilla = vanilla_profile(&mc)?;
    let want = cfg["loader_version"].as_str().filter(|s| !s.is_empty());
    let (prof, lv) = if meta_url(&loader).is_some() { meta_profile(&loader, &mc, want)? } else { installer_profile(&loader, &mc, root, want)? };
    let d = root.join("versions").join(&name);
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    let w = |f: String, v: &Value| std::fs::write(d.join(f), serde_json::to_string_pretty(v).unwrap()).map_err(|e| e.to_string());
    w(format!("{name}.json"), &merge(&vanilla, &prof, &name)?)?;
    w("TLauncherAdditional.json".into(), &sidecar(&name, &mc, &loader, &lv))?;
    std::fs::write(d.join(MARKER), json!({"instance": iid}).to_string()).map_err(|e| e.to_string())?;
    Ok(json!({"name": name, "folder": d.to_string_lossy(), "loader_version": lv, "gamedir": inst.root.to_string_lossy()}))
}

// ---------- import: TLauncher -> Orbit ----------

const LOADER_LIBS: [(&str, &str); 4] = [("net.fabricmc:fabric-loader", "fabric"), ("org.quiltmc:quilt-loader", "quilt"),
    ("net.neoforged:neoforge", "neoforge"), ("net.minecraftforge:forge", "forge")];
const MC_LIBS: [&str; 2] = ["net.fabricmc:intermediary", "org.quiltmc:hashed"];

fn read_json(p: &Path) -> Option<Value> { serde_json::from_str(std::fs::read_to_string(p).ok()?.trim_start_matches('\u{feff}')).ok() }

/// (name, mc, loader, loader_version) for a TLauncher version, or None.
pub fn detect(vid: &str, vj: &Value, side: Option<&Value>) -> Option<(String, String, String, String)> {
    let libs: Vec<&str> = vj["libraries"].as_array().into_iter().flatten().map(|l| l["name"].as_str().unwrap_or("")).collect();
    let (mut loader, mut lv): (Option<String>, String) = (None, String::new());
    for (prefix, ld) in LOADER_LIBS {
        let pre = format!("{prefix}:");
        if let Some(h) = libs.iter().find(|n| n.starts_with(&pre) && n.matches(':').count() == 2) {
            loader = Some(ld.to_string()); lv = h.split(':').nth(2).unwrap_or("").to_string(); break;
        }
    }
    let empty = json!({});
    let mp = side.map(|s| &s["modpack"]).filter(|m| m.is_object()).unwrap_or(&empty);
    let ver = &mp["version"];
    if loader.is_none() {
        let t = ver["minecraftVersionTypes"][0]["name"].as_str().unwrap_or("");
        loader = is_loader(t).then(|| t.to_string());
        lv = ver["minecraftVersionName"]["name"].as_str().unwrap_or("").to_string();
    }
    let loader = loader?;
    let mut mc = ver["gameVersionDTO"]["name"].as_str().filter(|s| !s.is_empty()).map(String::from)
        .or_else(|| side.and_then(|s| s["jar"].as_str()).filter(|s| !s.is_empty()).map(String::from))
        .or_else(|| vj["inheritsFrom"].as_str().filter(|s| !s.is_empty()).map(String::from));
    if mc.is_none() {
        mc = libs.iter().find(|n| MC_LIBS.iter().any(|p| n.starts_with(p)) && n.matches(':').count() == 2).and_then(|n| n.split(':').nth(2)).map(String::from);
    }
    if mc.is_none() && loader == "forge" && lv.contains('-') {
        let (a, b) = lv.split_once('-').unwrap();
        mc = Some(a.to_string()); lv = b.to_string();
    }
    if mc.is_none() && loader == "neoforge" && Regex::new(r"^\d+\.\d+").unwrap().is_match(&lv) {
        let p: Vec<&str> = lv.split('.').collect();
        mc = Some(format!("1.{}{}", p[0], if p.get(1).map(|b| *b != "0").unwrap_or(false) { format!(".{}", p[1]) } else { String::new() }));
    }
    let mc = mc?;
    if lv.starts_with(&format!("{mc}-")) { lv = lv[mc.len() + 1..].to_string(); }
    Some((mp["name"].as_str().filter(|s| !s.is_empty()).unwrap_or(vid).to_string(), mc, loader, lv))
}

pub fn count_content(base: &Path) -> Map<String, Value> {
    let mut out = Map::new();
    for d in CONTENT_DIRS {
        let n = std::fs::read_dir(base.join(d)).map(|rd| rd.flatten().filter(|e| e.path().is_file() && has_ext(&e.file_name().to_string_lossy())).count()).unwrap_or(0);
        if n > 0 { out.insert(d.into(), json!(n)); }
    }
    out
}

fn sorted_dirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| e.path()).filter(|x| x.is_dir()).collect()).unwrap_or_default();
    v.sort(); v
}

fn find_sources(root: &Path, id: &str, name: &str) -> Vec<Value> {
    let names = [name.to_lowercase(), id.to_lowercase()];
    let low = |p: &Path| p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
    let mut cands: Vec<(&str, PathBuf)> = vec![];
    let mp = root.join("modpacks");
    let mp_dirs = if mp.is_dir() { sorted_dirs(&mp) } else { vec![] };
    for d in &mp_dirs { if names.contains(&low(d)) { cands.push(("Modpack folder", d.clone())); } }
    cands.push(("Version folder", root.join("versions").join(id)));
    for d in &mp_dirs { if !names.contains(&low(d)) { cands.push(("Another modpack folder", d.clone())); } }
    for d in sorted_dirs(root) {
        let l = low(&d);
        if SKIP_DIRS.contains(&l.as_str()) || l == "modpacks" { continue; }
        cands.push(("Folder in TLauncher", d.clone()));
        for sub in sorted_dirs(&d) {
            let sl = sub.file_name().unwrap_or_default().to_string_lossy().to_string();
            if !SKIP_DIRS.contains(&sl.to_lowercase().as_str()) && !CONTENT_DIRS.contains(&sl.as_str()) && sl != "config" { cands.push(("Folder in TLauncher", sub)); }
        }
    }
    cands.push(("Main folder (shared by all versions)", root.to_path_buf()));
    let (mut seen, mut out) = (std::collections::HashSet::new(), vec![]);
    for (label, p) in cands {
        let key = p.canonicalize().unwrap_or(p.clone());
        if !p.is_dir() || !seen.insert(key) { continue; }
        let c = count_content(&p);
        if !c.is_empty() {
            let total: i64 = c.values().filter_map(|v| v.as_i64()).sum();
            out.push(json!({"label": label, "path": p.to_string_lossy(), "counts": c, "total": total}));
        }
    }
    out
}

/// TLauncher versions that use a loader and weren't made by Orbit.
pub fn scan(root: &Path) -> Vec<Value> {
    let mut out = vec![];
    for d in sorted_dirs(&root.join("versions")) {
        if d.join(MARKER).exists() { continue; }
        let id = d.file_name().unwrap().to_string_lossy().to_string();
        let Some(vj) = read_json(&d.join(format!("{id}.json"))).filter(|v| v.is_object()) else { continue };
        let side = read_json(&d.join("TLauncherAdditional.json")).filter(|v| v.is_object());
        let Some((name, mc, loader, lv)) = detect(&id, &vj, side.as_ref()) else { continue };
        let mp = side.as_ref().map(|s| s["modpack"].clone()).unwrap_or(Value::Null);
        let mem = if mp["modpackMemory"] == true { mp["memory"].as_i64().unwrap_or(0) } else { 0 };
        let sources = find_sources(root, &id, &name);
        out.push(json!({"id": id, "name": name, "mc": mc, "loader": loader, "loader_version": lv, "modpack": side.is_some(),
            "memory": if (512..=65536).contains(&mem) { mem } else { 0 }, "sources": sources}));
    }
    out
}

#[allow(dead_code)]
pub fn _unused(_: &mut Log) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_a_1_12_forge_profile() {
        let vanilla = json!({"id": "1.12.2", "mainClass": "net.minecraft.client.main.Main", "minecraftArguments": "--username ${auth_player_name} --version ${version_name}",
            "libraries": [{"name": "com.mojang:authlib:1.5.25"}], "assetIndex": {"id": "1.12"}});
        let forge = json!({"id": "1.12.2-forge-14.23.5.2860", "inheritsFrom": "1.12.2", "mainClass": "net.minecraft.launchwrapper.Launch",
            "minecraftArguments": "--username ${auth_player_name} --version ${version_name} --tweakClass net.minecraftforge.fml.common.launcher.FMLTweaker",
            "libraries": [{"name": "net.minecraftforge:forge:1.12.2-14.23.5.2860"}, {"name": "com.mojang:authlib:1.5.25"}]});
        let m = merge(&vanilla, &forge, "forge-1.12.2").unwrap();
        assert_eq!(m["mainClass"], "net.minecraft.launchwrapper.Launch");
        assert!(m["minecraftArguments"].as_str().unwrap().contains("--tweakClass"));
        assert!(m.get("arguments").is_none() && m.get("inheritsFrom").is_none());
        assert_eq!(m["libraries"].as_array().unwrap().len(), 2);            // authlib not duplicated
    }

    #[test]
    fn forge_folder_names_for_old_versions() {
        assert_eq!(forge_full("1.12.2", "14.23.5.2860"), "1.12.2-14.23.5.2860");
        assert_eq!(forge_full("1.7.10", "10.13.4.1614"), "1.7.10-10.13.4.1614-1.7.10");
        assert_eq!(forge_full("1.20.1", "47.3.0"), "1.20.1-47.3.0");
    }
}
