//! Dependencies of mods and plugins, whatever way they got into the folder.
//!
//! `mods.rs` / `servers.rs` already pull in the dependencies Modrinth lists when something is installed *from*
//! Modrinth. That misses everything else: modpack files, imports, jars dropped in by hand or uploaded, and
//! plugins. So this module reads the metadata inside each jar (the source of truth the loader itself uses):
//!   Fabric  fabric.mod.json      Quilt  quilt.mod.json      Forge / NeoForge  META-INF/(neoforge.)mods.toml
//!   Bukkit / Spigot / Paper  plugin.yml, paper-plugin.yml      Velocity  velocity-plugin.json      Bungee  bungee.yml
//! works out what is missing, maps the missing ids to Modrinth projects and installs them (`ensure`).
//! The same inventory feeds the crash diagnostics (`diagnose.rs`).
use crate::modrinth::api_get;
use crate::mods::Log;
use crate::util::Res;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- reading jars

#[derive(Clone, Debug, Default)]
pub struct Dep { pub id: String, pub range: String, pub side: String }

#[derive(Clone, Debug, Default)]
pub struct ModInfo {
    pub file: String,
    pub path: PathBuf,
    pub disabled: bool,
    pub size: u64,
    pub platforms: Vec<&'static str>,
    /// Primary id first, then what it `provides`.
    pub ids: Vec<String>,
    /// Ids of jars bundled inside this one (Fabric `jars`, Forge jar-in-jar). They satisfy dependencies too.
    pub nested: Vec<String>,
    pub name: String,
    pub version: String,
    pub requires: Vec<Dep>,
    pub breaks: Vec<Dep>,
    /// Accepted Minecraft versions (any one may match).
    pub mc: Vec<String>,
    /// "client" | "server" | "*"
    pub env: String,
    pub api_version: String,
    pub folia: Option<bool>,
    pub error: Option<String>,
}

impl ModInfo {
    pub fn id(&self) -> &str { self.ids.first().map(|s| s.as_str()).unwrap_or("") }
    pub fn label(&self) -> String {
        if !self.name.is_empty() { self.name.clone() } else if !self.id().is_empty() { self.id().to_string() } else { self.file.clone() }
    }
    pub fn base_file(&self) -> &str { self.file.strip_suffix(".disabled").unwrap_or(&self.file) }
}

fn strip_bom(s: &str) -> &str { s.trim_start_matches('\u{feff}') }

fn read_text<R: Read + Seek>(z: &mut zip::ZipArchive<R>, name: &str) -> Option<String> {
    let e = z.by_name(name).ok()?;
    if e.size() > 4 << 20 { return None; }
    let mut s = String::new();
    e.take(4 << 20).read_to_string(&mut s).ok()?;
    Some(s)
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => vec![],
    }
}

fn add_id(mi: &mut ModInfo, id: &str) {
    let id = id.trim().to_lowercase();
    if !id.is_empty() && !mi.ids.contains(&id) { mi.ids.push(id); }
}

fn add_platform(mi: &mut ModInfo, p: &'static str) { if !mi.platforms.contains(&p) { mi.platforms.push(p); } }

fn parse_fabric(mi: &mut ModInfo, t: &str) {
    let Ok(j) = serde_json::from_str::<Value>(strip_bom(t)) else { return };
    add_platform(mi, "fabric");
    add_id(mi, j["id"].as_str().unwrap_or(""));
    for p in j["provides"].as_array().into_iter().flatten() { add_id(mi, p.as_str().or(p["id"].as_str()).unwrap_or("")); }
    mi.name = j["name"].as_str().unwrap_or("").to_string();
    mi.version = j["version"].as_str().unwrap_or("").to_string();
    mi.env = j["environment"].as_str().unwrap_or("*").to_string();
    for (k, v) in j["depends"].as_object().into_iter().flatten() {
        if k == "minecraft" { mi.mc = strings(v); } else { mi.requires.push(Dep { id: k.to_lowercase(), range: strings(v).join("||"), side: String::new() }); }
    }
    for (k, v) in j["breaks"].as_object().into_iter().flatten() { mi.breaks.push(Dep { id: k.to_lowercase(), range: strings(v).join("||"), side: String::new() }); }
}

fn parse_quilt(mi: &mut ModInfo, t: &str) {
    let Ok(j) = serde_json::from_str::<Value>(strip_bom(t)) else { return };
    let q = &j["quilt_loader"];
    add_platform(mi, "quilt");
    add_id(mi, q["id"].as_str().unwrap_or(""));
    for p in q["provides"].as_array().into_iter().flatten() { add_id(mi, p.as_str().or(p["id"].as_str()).unwrap_or("")); }
    mi.name = q["metadata"]["name"].as_str().unwrap_or("").to_string();
    mi.version = q["version"].as_str().unwrap_or("").to_string();
    mi.env = match j["minecraft"]["environment"].as_str() { Some("client") => "client", Some("dedicated_server") => "server", _ => "*" }.to_string();
    for (list, out) in [(&q["depends"], 0), (&q["breaks"], 1)] {
        for d in list.as_array().into_iter().flatten() {
            let (id, range, optional) = match d { Value::String(s) => (s.as_str(), String::new(), false),
                o => (o["id"].as_str().unwrap_or(""), strings(&o["versions"]).join("||"), o["optional"] == true) };
            if id.is_empty() || optional { continue; }
            if id == "minecraft" && out == 0 { mi.mc = strings(&d["versions"]); continue; }
            let dep = Dep { id: id.to_lowercase(), range, side: String::new() };
            if out == 0 { mi.requires.push(dep) } else { mi.breaks.push(dep) }
        }
    }
}

fn parse_forge(mi: &mut ModInfo, t: &str, neo: bool) {
    let Ok(v) = toml::from_str::<toml::Value>(strip_bom(t)) else { return };
    add_platform(mi, if neo { "neoforge" } else { "forge" });
    let mods: Vec<&toml::Value> = v.get("mods").and_then(|m| m.as_array()).into_iter().flatten().collect();
    for m in &mods {
        let id = m.get("modId").and_then(|x| x.as_str()).unwrap_or("");
        add_id(mi, id);
        for p in m.get("provides").and_then(|x| x.as_array()).into_iter().flatten() { add_id(mi, p.as_str().unwrap_or("")); }
        if mi.name.is_empty() { mi.name = m.get("displayName").and_then(|x| x.as_str()).unwrap_or("").to_string(); }
        if mi.version.is_empty() { mi.version = m.get("version").and_then(|x| x.as_str()).filter(|s| !s.contains("${")).unwrap_or("").to_string(); }
    }
    let owners: Vec<String> = mods.iter().filter_map(|m| m.get("modId").and_then(|x| x.as_str()).map(|s| s.to_string())).collect();
    for (owner, list) in v.get("dependencies").and_then(|d| d.as_table()).into_iter().flatten() {
        if !owners.contains(owner) { continue; }
        for d in list.as_array().into_iter().flatten() {
            let id = d.get("modId").and_then(|x| x.as_str()).unwrap_or("").to_lowercase();
            if id.is_empty() { continue; }
            let range = d.get("versionRange").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let side = d.get("side").and_then(|x| x.as_str()).unwrap_or("BOTH").to_uppercase();
            let ty = d.get("type").and_then(|x| x.as_str());
            if matches!(ty, Some("incompatible")) { mi.breaks.push(Dep { id, range, side }); continue; }
            let required = match (d.get("mandatory").and_then(|x| x.as_bool()), ty) { (Some(b), _) => b, (None, Some(t)) => t == "required", _ => true };
            if id == "minecraft" { if !range.is_empty() { mi.mc.push(range); } continue; }
            if required { mi.requires.push(Dep { id, range, side }); }
        }
    }
}

fn parse_mcmod_info(mi: &mut ModInfo, t: &str) {
    let Ok(j) = serde_json::from_str::<Value>(strip_bom(t)) else { return };
    let list = j.as_array().cloned().or_else(|| j["modList"].as_array().cloned()).unwrap_or_default();
    for m in list {
        add_platform(mi, "forge");
        add_id(mi, m["modid"].as_str().unwrap_or(""));
        if mi.name.is_empty() { mi.name = m["name"].as_str().unwrap_or("").to_string(); }
        if mi.version.is_empty() { mi.version = m["version"].as_str().unwrap_or("").to_string(); }
    }
}

fn yaml_list(v: Option<&serde_yaml::Value>) -> Vec<String> {
    match v {
        Some(serde_yaml::Value::String(s)) => vec![s.clone()],
        Some(serde_yaml::Value::Sequence(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => vec![],
    }
}

fn parse_bukkit(mi: &mut ModInfo, t: &str, paper: bool, bungee: bool) {
    let Ok(y) = serde_yaml::from_str::<serde_yaml::Value>(strip_bom(t)) else { mi.error = Some("its plugin descriptor can't be read".into()); return };
    let g = |k: &str| y.get(k);
    add_platform(mi, if bungee { "bungee" } else if paper { "paper" } else { "bukkit" });
    add_id(mi, g("name").and_then(|v| v.as_str()).unwrap_or(""));
    for p in yaml_list(g("provides")) { add_id(mi, &p); }
    mi.name = g("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    mi.version = match g("version") { Some(serde_yaml::Value::String(s)) => s.clone(), Some(v) => serde_yaml::to_string(v).unwrap_or_default().trim().to_string(), None => String::new() };
    mi.api_version = match g("api-version") { Some(serde_yaml::Value::String(s)) => s.clone(), Some(serde_yaml::Value::Number(n)) => n.to_string(), _ => String::new() };
    mi.folia = g("folia-supported").and_then(|v| v.as_bool());
    for d in yaml_list(g("depend")).into_iter().chain(yaml_list(g("depends"))) { mi.requires.push(Dep { id: d.to_lowercase(), ..Default::default() }); }
    if paper {
        for section in ["server", "bootstrap"] {
            for (k, v) in y.get("dependencies").and_then(|d| d.get(section)).and_then(|s| s.as_mapping()).into_iter().flatten() {
                let required = v.get("required").and_then(|r| r.as_bool()).unwrap_or(true);
                if let (true, Some(name)) = (required, k.as_str()) { mi.requires.push(Dep { id: name.to_lowercase(), ..Default::default() }); }
            }
        }
    }
}

fn parse_velocity(mi: &mut ModInfo, t: &str) {
    let Ok(j) = serde_json::from_str::<Value>(strip_bom(t)) else { return };
    add_platform(mi, "velocity");
    add_id(mi, j["id"].as_str().unwrap_or(""));
    mi.name = j["name"].as_str().unwrap_or("").to_string();
    mi.version = j["version"].as_str().unwrap_or("").to_string();
    for d in j["dependencies"].as_array().into_iter().flatten() {
        if d["optional"] != true { if let Some(id) = d["id"].as_str() { mi.requires.push(Dep { id: id.to_lowercase(), ..Default::default() }); } }
    }
}

fn fill<R: Read + Seek>(z: &mut zip::ZipArchive<R>, mi: &mut ModInfo, depth: u8) {
    if let Some(t) = read_text(z, "fabric.mod.json") { parse_fabric(mi, &t); }
    if let Some(t) = read_text(z, "quilt.mod.json") { parse_quilt(mi, &t); }
    if let Some(t) = read_text(z, "META-INF/neoforge.mods.toml") { parse_forge(mi, &t, true); }
    else if let Some(t) = read_text(z, "META-INF/mods.toml") { parse_forge(mi, &t, false); }
    if let Some(t) = read_text(z, "paper-plugin.yml") { parse_bukkit(mi, &t, true, false); }
    if let Some(t) = read_text(z, "plugin.yml") {
        if mi.platforms.contains(&"paper") { // both descriptors: the Bukkit one still names the plugin for older servers
            if let Ok(y) = serde_yaml::from_str::<serde_yaml::Value>(strip_bom(&t)) { mi.folia = mi.folia.or(y.get("folia-supported").and_then(|v| v.as_bool())); }
            add_platform(mi, "bukkit");
        } else { parse_bukkit(mi, &t, false, false); }
    }
    if let Some(t) = read_text(z, "bungee.yml") { parse_bukkit(mi, &t, false, true); }
    if let Some(t) = read_text(z, "velocity-plugin.json") { parse_velocity(mi, &t); }
    if mi.platforms.is_empty() { if let Some(t) = read_text(z, "mcmod.info") { parse_mcmod_info(mi, &t); } }
    if depth == 0 {
        let names: Vec<String> = z.file_names().filter(|n| (n.starts_with("META-INF/jars/") || n.starts_with("META-INF/jarjar/")) && n.ends_with(".jar")).take(96).map(String::from).collect();
        for n in names {
            let Ok(mut e) = z.by_name(&n) else { continue };
            if e.size() > 24 << 20 { continue; }
            let mut buf = Vec::with_capacity(e.size() as usize);
            if e.read_to_end(&mut buf).is_err() { continue; }
            drop(e);
            let Ok(mut inner) = zip::ZipArchive::new(Cursor::new(buf)) else { continue };
            let mut sub = ModInfo::default();
            fill(&mut inner, &mut sub, 1);
            mi.nested.extend(sub.ids.iter().cloned());
            mi.nested.extend(sub.nested.iter().cloned());
        }
    }
}

pub fn read_jar(path: &Path) -> ModInfo {
    let file = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut mi = ModInfo { file: file.clone(), path: path.to_path_buf(), disabled: file.to_lowercase().ends_with(".disabled"),
        size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0), env: "*".into(), ..Default::default() };
    match std::fs::File::open(path) {
        Err(e) => mi.error = Some(format!("can't be opened ({e})")),
        Ok(f) => match zip::ZipArchive::new(f) {
            Ok(mut z) => fill(&mut z, &mut mi, 0),
            Err(e) => mi.error = Some(format!("isn't a valid .jar file ({e})")),
        },
    }
    mi
}

/// Every .jar / .jar.disabled directly inside `dir`, with its metadata.
pub fn scan_dir(dir: &Path) -> Vec<ModInfo> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| {
        let n = p.file_name().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        p.is_file() && (n.ends_with(".jar") || n.ends_with(".jar.disabled"))
    }).collect()).unwrap_or_default();
    paths.sort_by_key(|p| p.file_name().map(|s| s.to_string_lossy().to_lowercase()));
    crate::mods::par_map(&paths, 8, |p| read_jar(p))
}

// ---------------------------------------------------------------- versions and ranges

fn parse_ver(s: &str) -> Option<Vec<u64>> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    if !s.starts_with(|c: char| c.is_ascii_digit()) { return None; }
    let head = s.split(['+', '-', '_', ' ']).next().unwrap_or("");
    let mut out = vec![];
    for part in head.split('.') {
        match part.parse::<u64>() { Ok(n) => out.push(n), Err(_) => break }
    }
    if out.is_empty() { None } else { Some(out) }
}

pub fn cmp_ver(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y { return x.cmp(&y); }
    }
    std::cmp::Ordering::Equal
}

fn single_range(r: &str, v: &[u64]) -> Option<bool> {
    let r = r.trim();
    if r.is_empty() || r == "*" || r.eq_ignore_ascii_case("x") { return Some(true); }
    if r.starts_with(['[', '(']) { // Maven: [1.20,1.21) (,1.21) [1.20.1]
        let mut any = false;
        let re = regex::Regex::new(r"[\[\(][^\]\)]*[\]\)]").unwrap();
        for m in re.find_iter(r) {
            let s = m.as_str();
            let (lo_inc, hi_inc) = (s.starts_with('['), s.ends_with(']'));
            let inner = &s[1..s.len() - 1];
            let ok = match inner.split_once(',') {
                None => parse_ver(inner).map(|x| cmp_ver(v, &x) == std::cmp::Ordering::Equal)?,
                Some((lo, hi)) => {
                    let lo_ok = if lo.trim().is_empty() { true } else { let x = parse_ver(lo)?; let c = cmp_ver(v, &x); c == std::cmp::Ordering::Greater || (lo_inc && c == std::cmp::Ordering::Equal) };
                    let hi_ok = if hi.trim().is_empty() { true } else { let x = parse_ver(hi)?; let c = cmp_ver(v, &x); c == std::cmp::Ordering::Less || (hi_inc && c == std::cmp::Ordering::Equal) };
                    lo_ok && hi_ok
                }
            };
            any |= ok;
        }
        return Some(any);
    }
    // Fabric / semver style: space separated comparators that all have to hold
    for part in r.split_whitespace() {
        let part = part.trim_end_matches('-');
        let (op, rest) = ["<=", ">=", "<", ">", "=", "~", "^"].iter().find(|o| part.starts_with(**o)).map(|o| (*o, &part[o.len()..])).unwrap_or(("", part));
        let rest_t = rest.trim_end_matches(".x").trim_end_matches(".*").trim_end_matches(".X");
        let wild = rest_t.len() != rest.len();
        let x = parse_ver(rest_t)?;
        let c = cmp_ver(v, &x);
        use std::cmp::Ordering::*;
        let ok = match op {
            ">=" => c != Less, ">" => c == Greater, "<=" => c != Greater, "<" => c == Less,
            "~" => { let mut hi = x.clone(); if hi.len() >= 2 { hi.truncate(2); hi[1] += 1; } else { hi = vec![hi[0] + 1]; } c != Less && cmp_ver(v, &hi) == Less }
            "^" => { let mut hi = vec![x[0] + 1]; if x[0] == 0 && x.len() > 1 { hi = vec![0, x[1] + 1]; } c != Less && cmp_ver(v, &hi) == Less }
            _ if wild => { v.iter().zip(x.iter()).all(|(a, b)| a == b) && v.len() >= x.len() }
            _ => c == Equal,
        };
        if !ok { return Some(false); }
    }
    Some(true)
}

/// Does `version` satisfy `range` (alternatives separated by `||`)? None when either side can't be understood,
/// so an odd version string never produces a false alarm.
pub fn range_match(range: &str, version: &str) -> Option<bool> {
    let v = parse_ver(version)?;
    let mut unknown = false;
    for alt in range.split("||") {
        match single_range(alt, &v) { Some(true) => return Some(true), Some(false) => {}, None => unknown = true }
    }
    if unknown { None } else { Some(false) }
}

// ---------------------------------------------------------------- analysis

#[derive(Clone, Debug)]
pub struct Ctx {
    pub mc: String,
    /// fabric, quilt, forge, neoforge, paper, purpur, folia, spigot, velocity, ...
    pub platform: String,
    pub server: bool,
    /// Modrinth loader categories that can run here.
    pub loaders: Vec<String>,
}

impl Ctx {
    pub fn is_plugin_host(&self) -> bool { matches!(self.platform.as_str(), "paper" | "purpur" | "folia" | "spigot" | "velocity" | "bukkit") }
    fn allowed(&self) -> &'static [&'static str] {
        match self.platform.as_str() {
            "fabric" => &["fabric"], "quilt" => &["quilt", "fabric"], "forge" => &["forge"], "neoforge" => &["neoforge", "forge"],
            "paper" | "purpur" | "folia" | "spigot" | "bukkit" => &["bukkit", "paper"], "velocity" => &["velocity"], _ => &[],
        }
    }
    /// Would this jar be loaded here at all?
    pub fn accepts(&self, m: &ModInfo) -> bool {
        let a = self.allowed();
        a.is_empty() || m.platforms.is_empty() || m.platforms.iter().any(|p| a.contains(p))
    }
}

const BUILTIN: [&str; 22] = ["minecraft", "java", "forge", "neoforge", "fabricloader", "fabric-loader", "fabric_loader", "quilt_loader", "quilt-loader",
    "quilt_base", "javafml", "lowcodefml", "mc", "spigot", "bukkit", "paper", "purpur", "folia", "velocity", "bungeecord", "craftbukkit", "mixin"];

fn is_fabric_api_module(id: &str) -> bool {
    if id == "fabric" || id == "fabric-api" || id == "fabric-api-base" || id == "fabric-crash-report-info-v1" { return true; }
    id.starts_with("fabric-") && !id.contains("permissions") && !id.contains("kotlin") && regex::Regex::new(r"-v\d+$").unwrap().is_match(id)
}

/// Several ids can stand for one project (every Fabric API module is part of fabric-api).
fn canonical(id: &str) -> String { if is_fabric_api_module(id) { "fabric-api".into() } else { id.to_string() } }

#[derive(Clone, Debug, Default)]
pub struct Missing { pub id: String, pub range: String, pub needed_by: Vec<String>, pub file: String }
#[derive(Clone, Debug, Default)]
pub struct Wrong { pub id: String, pub range: String, pub have: String, pub needed_by: Vec<String> }

#[derive(Default, Debug)]
pub struct Report {
    pub missing: Vec<Missing>,
    /// Needed, and present, but switched off.
    pub disabled: Vec<Missing>,
    pub wrong_version: Vec<Wrong>,
    pub duplicates: Vec<(String, Vec<String>)>,
    pub wrong_loader: Vec<(String, Vec<&'static str>)>,
    pub client_only: Vec<String>,
    pub mc_mismatch: Vec<(String, String)>,
    pub broken: Vec<(String, String)>,
    pub conflicts: Vec<(String, String, String)>,
    pub api_too_new: Vec<(String, String)>,
    pub folia_unsupported: Vec<String>,
}

fn push_need(list: &mut Vec<Missing>, id: &str, range: &str, by: &str, file: &str) {
    match list.iter_mut().find(|m| m.id == id) {
        Some(m) => { if !m.needed_by.iter().any(|b| b == by) { m.needed_by.push(by.to_string()); } }
        None => list.push(Missing { id: id.to_string(), range: range.to_string(), needed_by: vec![by.to_string()], file: file.to_string() }),
    }
}

pub fn analyze(inv: &[ModInfo], ctx: &Ctx) -> Report {
    let mut rep = Report::default();
    let mut provided: HashMap<String, Vec<String>> = HashMap::new();      // id -> versions
    let mut off: HashMap<String, String> = HashMap::new();                // id -> file
    for m in inv {
        if let Some(e) = &m.error { rep.broken.push((m.file.clone(), e.clone())); continue; }
        if !ctx.accepts(m) { continue; }
        for id in m.ids.iter().chain(m.nested.iter()) {
            if m.disabled { off.entry(id.clone()).or_insert_with(|| m.file.clone()); }
            else { provided.entry(id.clone()).or_default().push(m.version.clone()); }
        }
    }
    let live: Vec<&ModInfo> = inv.iter().filter(|m| m.error.is_none() && !m.disabled).collect();
    let mc_ver = parse_ver(&ctx.mc);

    for m in &live {
        if !ctx.accepts(m) { if !m.platforms.is_empty() { rep.wrong_loader.push((m.file.clone(), m.platforms.clone())); } continue; }
        if ctx.server && m.env == "client" { rep.client_only.push(m.file.clone()); }
        let label = m.label();
        for d in &m.requires {
            if BUILTIN.contains(&d.id.as_str()) { continue; }
            if (d.side == "CLIENT" && ctx.server) || (d.side == "SERVER" && !ctx.server) { continue; }
            let id = canonical(&d.id);
            if m.ids.contains(&d.id) { continue; }
            match provided.get(&d.id).or_else(|| provided.get(&id)) {
                Some(vs) => {
                    let known: Vec<&String> = vs.iter().filter(|v| !v.is_empty()).collect();
                    if !d.range.is_empty() && !known.is_empty() && known.iter().all(|v| range_match(&d.range, v) == Some(false)) {
                        match rep.wrong_version.iter_mut().find(|w| w.id == d.id) {
                            Some(w) => w.needed_by.push(label.clone()),
                            None => rep.wrong_version.push(Wrong { id: d.id.clone(), range: d.range.clone(), have: known[0].clone(), needed_by: vec![label.clone()] }),
                        }
                    }
                }
                None => match off.get(&d.id).or_else(|| off.get(&id)) {
                    Some(f) => push_need(&mut rep.disabled, &d.id, &d.range, &label, f),
                    None => push_need(&mut rep.missing, &id, &d.range, &label, &m.file),
                },
            }
        }
        if !m.mc.is_empty() && mc_ver.is_some() && m.mc.iter().all(|r| range_match(r, &ctx.mc) == Some(false)) {
            rep.mc_mismatch.push((m.file.clone(), m.mc.join(" or ")));
        }
        if !m.api_version.is_empty() {
            if let (Some(api), Some(mc)) = (parse_ver(&m.api_version), &mc_ver) {
                if cmp_ver(&api[..api.len().min(2)], &mc[..mc.len().min(2)]) == std::cmp::Ordering::Greater { rep.api_too_new.push((m.file.clone(), m.api_version.clone())); }
            }
        }
        if ctx.platform == "folia" && m.platforms.iter().any(|p| *p == "bukkit" || *p == "paper") && m.folia != Some(true) { rep.folia_unsupported.push(m.file.clone()); }
        for b in &m.breaks {
            if let Some(vs) = provided.get(&b.id) {
                if b.range.is_empty() || vs.iter().any(|v| range_match(&b.range, v) == Some(true) || (v.is_empty() && b.range.trim() == "*")) {
                    if let Some(other) = live.iter().find(|o| o.ids.contains(&b.id) && o.file != m.file) {
                        rep.conflicts.push((m.file.clone(), other.file.clone(), format!("{label} says it can't run together with {}", other.label())));
                    }
                }
            }
        }
    }
    // the same mod twice
    let mut by_id: HashMap<&str, Vec<&ModInfo>> = HashMap::new();
    for m in &live { if ctx.accepts(m) && !m.id().is_empty() { by_id.entry(m.id()).or_default().push(m); } }
    let mut dups: Vec<(String, Vec<String>)> = by_id.into_iter().filter(|(_, v)| v.len() > 1).map(|(k, v)| (k.to_string(), v.iter().map(|m| m.file.clone()).collect())).collect();
    dups.sort();
    rep.duplicates = dups;
    // pairs that are known not to work together
    let has = |id: &str| live.iter().find(|m| ctx.accepts(m) && m.ids.iter().any(|i| i == id));
    for (a, b) in [("sodium", "optifabric"), ("iris", "optifabric"), ("rubidium", "embeddium")] {
        if let (Some(x), Some(y)) = (has(a), has(b)) {
            if !rep.conflicts.iter().any(|c| (c.0 == x.file && c.1 == y.file) || (c.0 == y.file && c.1 == x.file)) {
                rep.conflicts.push((x.file.clone(), y.file.clone(), format!("{} and {} are known not to work together", x.label(), y.label())));
            }
        }
    }
    rep
}

// ---------------------------------------------------------------- finding the Modrinth project

#[derive(Clone, Debug)]
pub struct Resolved { pub pid: String, pub slug: String, pub title: String }

const ALIASES: [(&str, &str); 12] = [
    ("fabric", "fabric-api"), ("architectury", "architectury-api"), ("owo", "owo-lib"), ("kotlinforforge", "kotlin-for-forge"),
    ("forgeconfigapiport", "forge-config-api-port"), ("puzzleslib", "puzzles-lib"), ("geckolib3", "geckolib"),
    ("cardinal-components-base", "cardinal-components-api"), ("cardinal-components-entity", "cardinal-components-api"),
    ("moonlight", "selene"), ("fabric-language-kotlin", "fabric-language-kotlin"), ("modmenu", "modmenu")];

fn norm(s: &str) -> String { s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase() }

type CacheKey = String;
static CACHE: Mutex<Option<HashMap<CacheKey, (Instant, Option<Resolved>)>>> = Mutex::new(None);

fn project_ok(p: &Value, ctx: &Ctx) -> bool {
    if p.is_null() || !matches!(p["project_type"].as_str(), Some("mod") | Some("plugin")) { return false; }
    let loaders: Vec<&str> = p["loaders"].as_array().into_iter().flatten().filter_map(|l| l.as_str()).collect();
    if !ctx.loaders.is_empty() && !loaders.is_empty() && !ctx.loaders.iter().any(|l| loaders.contains(&l.as_str())) { return false; }
    if ctx.server && p["server_side"] == "unsupported" { return false; }
    true
}

fn as_resolved(p: &Value) -> Option<Resolved> {
    Some(Resolved { pid: p["id"].as_str()?.to_string(), slug: p["slug"].as_str().unwrap_or("").to_string(), title: p["title"].as_str().unwrap_or("").to_string() })
}

/// Map a mod id / plugin name to a Modrinth project that can run in `ctx`. Only confident matches are returned:
/// an exact slug, or a search hit whose slug or title is the same name. A wrong guess here would put an unrelated
/// mod into someone's game, so "not found" is the safe answer.
pub fn resolve(id: &str, ctx: &Ctx) -> Res<Option<Resolved>> {
    let key = format!("{id}|{}|{}", ctx.mc, ctx.loaders.join(","));
    if let Some((t, r)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&key)) {
        if t.elapsed() < Duration::from_secs(600) { return Ok(r.clone()); }
    }
    let safe = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !safe(id) || id.len() > 64 { return Ok(None); }
    let mut cands: Vec<String> = vec![];
    if let Some((_, slug)) = ALIASES.iter().find(|(k, _)| *k == id) { cands.push(slug.to_string()); }
    cands.push(id.to_string());
    cands.push(id.replace('_', "-"));
    cands.dedup();
    let mut found: Option<Resolved> = None;
    for c in cands.iter().collect::<HashSet<_>>().into_iter().collect::<Vec<_>>() {
        let p = api_get(&format!("/project/{c}"), &[])?;
        if project_ok(&p, ctx) { found = as_resolved(&p); break; }
    }
    if found.is_none() {
        let mut facets: Vec<Vec<String>> = vec![];
        if !ctx.loaders.is_empty() { facets.push(ctx.loaders.iter().map(|l| format!("categories:{l}")).collect()); }
        if !ctx.mc.is_empty() && ctx.platform != "velocity" { facets.push(vec![format!("versions:{}", ctx.mc)]); }
        let res = api_get("/search", &[("query", id.to_string()), ("limit", "10".into()), ("facets", json!(facets).to_string())])?;
        let want = norm(id);
        for h in res["hits"].as_array().into_iter().flatten() {
            if norm(h["slug"].as_str().unwrap_or("")) == want || norm(h["title"].as_str().unwrap_or("")) == want {
                if ctx.server && h["server_side"] == "unsupported" { continue; }
                found = Some(Resolved { pid: h["project_id"].as_str().unwrap_or("").to_string(), slug: h["slug"].as_str().unwrap_or("").to_string(), title: h["title"].as_str().unwrap_or("").to_string() });
                break;
            }
        }
        if found.as_ref().map(|f| f.pid.is_empty()).unwrap_or(false) { found = None; }
    }
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    if m.len() > 500 { m.clear(); }
    m.insert(key, (Instant::now(), found.clone()));
    Ok(found)
}

// ---------------------------------------------------------------- installing

/// How `ensure` puts a project in place. Instances and servers keep their records differently.
pub trait Installer {
    /// Title if this Modrinth project is already recorded as installed.
    fn installed(&self, pid: &str) -> Option<String>;
    fn install(&mut self, pid: &str, seen: &mut HashSet<String>, log: &mut Log) -> Res<()>;
}

#[derive(Debug, Default, Clone)]
pub struct Unresolved { pub id: String, pub needed_by: Vec<String>, pub why: String, pub url: String }

#[derive(Debug, Default)]
pub struct Outcome { pub installed: Vec<String>, pub unresolved: Vec<Unresolved>, pub disabled: Vec<Missing>, pub offline: bool }

impl Outcome {
    pub fn to_json(&self) -> Value {
        json!({"installed": self.installed, "offline": self.offline,
            "unresolved": self.unresolved.iter().map(|u| json!({"id": u.id, "needed_by": u.needed_by, "why": u.why, "url": u.url})).collect::<Vec<_>>(),
            "disabled": self.disabled.iter().map(|d| json!({"id": d.id, "file": d.file, "needed_by": d.needed_by})).collect::<Vec<_>>()})
    }
}

fn search_url(id: &str, ctx: &Ctx) -> String {
    format!("https://modrinth.com/{}s?q={}", if ctx.is_plugin_host() { "plugin" } else { "mod" },
        percent_encoding::utf8_percent_encode(id, percent_encoding::NON_ALPHANUMERIC))
}

/// Install whatever the jars in `dir` need and don't have. Repeats a few rounds because a new dependency can
/// have dependencies of its own. Network trouble never fails the caller: it is reported in the outcome.
pub fn ensure(dir: &Path, ctx: &Ctx, inst: &mut dyn Installer, log: &mut Log) -> Outcome {
    let mut out = Outcome::default();
    let mut tried: HashMap<String, Resolved> = HashMap::new();
    let mut failed: Vec<Unresolved> = vec![];
    'rounds: for _ in 0..4 {
        let rep = analyze(&scan_dir(dir), ctx);
        let todo: Vec<Missing> = rep.missing.into_iter().filter(|m| !tried.contains_key(&m.id) && !failed.iter().any(|f| f.id == m.id)).collect();
        if todo.is_empty() { break; }
        let mut progress = false;
        for m in todo {
            let un = |why: String| Unresolved { id: m.id.clone(), needed_by: m.needed_by.clone(), why, url: search_url(&m.id, ctx) };
            match resolve(&m.id, ctx) {
                Err(e) => { out.offline = true; failed.push(un(format!("Couldn't reach Modrinth: {e}"))); break 'rounds; }
                Ok(None) => failed.push(un("Not found on Modrinth. Install it by hand.".into())),
                Ok(Some(r)) => {
                    if let Some(t) = inst.installed(&r.pid) {
                        failed.push(un(format!("{t} is installed but doesn't provide '{}'. It may be the wrong version.", m.id)));
                        continue;
                    }
                    match inst.install(&r.pid, &mut HashSet::new(), log) {
                        Ok(()) => { tried.insert(m.id.clone(), r); progress = true; }
                        Err(e) => failed.push(un(e)),
                    }
                }
            }
        }
        if !progress { break; }
    }
    let rep = analyze(&scan_dir(dir), ctx);
    let still: HashSet<String> = rep.missing.iter().map(|m| m.id.clone()).collect();
    for (id, r) in &tried {
        if still.contains(id) {
            let by = rep.missing.iter().find(|m| &m.id == id).map(|m| m.needed_by.clone()).unwrap_or_default();
            failed.push(Unresolved { id: id.clone(), needed_by: by, why: format!("{} has no version for {} on {}.", r.title, if ctx.mc.is_empty() { "this version" } else { &ctx.mc }, ctx.platform), url: search_url(id, ctx) });
        } else { out.installed.push(r.title.clone()); }
    }
    for m in rep.missing { if !failed.iter().any(|f| f.id == m.id) && !tried.contains_key(&m.id) { failed.push(Unresolved { id: m.id.clone(), needed_by: m.needed_by, why: "Couldn't be installed.".into(), url: search_url(&m.id, ctx) }); } }
    out.installed.sort();
    out.unresolved = failed;
    out.disabled = rep.disabled;
    out
}

/// Human lines for the console / log.
pub fn describe(o: &Outcome) -> Vec<String> {
    let mut v = vec![];
    if !o.installed.is_empty() { v.push(format!("Installed missing dependencies: {}", o.installed.join(", "))); }
    for u in &o.unresolved { v.push(format!("Still missing: {} (needed by {}). {}", u.id, u.needed_by.join(", "), u.why)); }
    for d in &o.disabled { v.push(format!("{} is switched off ({}), but {} needs it.", d.id, d.file, d.needed_by.join(", "))); }
    v
}

/// The static report as JSON for the UI (no network).
pub fn report_json(rep: &Report) -> Value {
    json!({
        "missing": rep.missing.iter().map(|m| json!({"id": m.id, "range": m.range, "needed_by": m.needed_by})).collect::<Vec<_>>(),
        "disabled": rep.disabled.iter().map(|m| json!({"id": m.id, "file": m.file, "needed_by": m.needed_by})).collect::<Vec<_>>(),
        "wrong_version": rep.wrong_version.iter().map(|w| json!({"id": w.id, "range": w.range, "have": w.have, "needed_by": w.needed_by})).collect::<Vec<_>>(),
        "duplicates": rep.duplicates.iter().map(|(id, f)| json!({"id": id, "files": f})).collect::<Vec<_>>(),
        "wrong_loader": rep.wrong_loader.iter().map(|(f, p)| json!({"file": f, "built_for": p})).collect::<Vec<_>>(),
        "client_only": rep.client_only, "broken": rep.broken.iter().map(|(f, e)| json!({"file": f, "error": e})).collect::<Vec<_>>(),
        "mc_mismatch": rep.mc_mismatch.iter().map(|(f, r)| json!({"file": f, "needs": r})).collect::<Vec<_>>(),
        "conflicts": rep.conflicts.iter().map(|(a, b, w)| json!({"a": a, "b": b, "why": w})).collect::<Vec<_>>(),
        "api_too_new": rep.api_too_new.iter().map(|(f, a)| json!({"file": f, "api": a})).collect::<Vec<_>>(),
        "folia_unsupported": rep.folia_unsupported,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(range_match(">=1.20 <1.21", "1.20.1"), Some(true));
        assert_eq!(range_match(">=1.20 <1.21", "1.21"), Some(false));
        assert_eq!(range_match("~1.20.1", "1.20.4"), Some(true));
        assert_eq!(range_match("~1.20.1", "1.21"), Some(false));
        assert_eq!(range_match("1.20.x", "1.20.6"), Some(true));
        assert_eq!(range_match("1.20.x", "1.19.4"), Some(false));
        assert_eq!(range_match("[1.20.1,1.21)", "1.20.4"), Some(true));
        assert_eq!(range_match("[1.20.1,1.21)", "1.21"), Some(false));
        assert_eq!(range_match("[1.20.1]", "1.20.1"), Some(true));
        assert_eq!(range_match("[1.20,)", "1.19.2"), Some(false));
        assert_eq!(range_match("*", "1.0"), Some(true));
        assert_eq!(range_match(">=1.19 || 1.18.2", "1.18.2"), Some(true));
        assert_eq!(range_match(">=1.19", "24w14a"), None);
    }

    fn jar(dir: &Path, name: &str, files: &[(&str, &str)]) {
        use std::io::Write;
        std::fs::create_dir_all(dir).unwrap();
        let mut z = zip::ZipWriter::new(std::fs::File::create(dir.join(name)).unwrap());
        for (k, v) in files { z.start_file(*k, zip::write::SimpleFileOptions::default()).unwrap(); z.write_all(v.as_bytes()).unwrap(); }
        z.finish().unwrap();
    }
    fn tmp(tag: &str) -> PathBuf { let d = std::env::temp_dir().join(format!("orbit-test-{tag}-{:08x}", rand::random::<u32>())); let _ = std::fs::remove_dir_all(&d); d }
    fn fabric() -> Ctx { Ctx { mc: "1.20.1".into(), platform: "fabric".into(), server: false, loaders: vec!["fabric".into()] } }

    #[test]
    fn fabric_inventory() {
        let d = tmp("fab");
        jar(&d, "cool.jar", &[("fabric.mod.json", r#"{"id":"coolmod","version":"1.0","name":"Cool","depends":{"fabricloader":">=0.14","minecraft":"~1.20.1","fabric-api-base":"*","libx":">=2.0","hidden":"*"}}"#)]);
        jar(&d, "old.jar", &[("fabric.mod.json", r#"{"id":"oldmod","version":"1","depends":{"minecraft":"~1.19"}}"#)]);
        jar(&d, "forgey.jar", &[("META-INF/mods.toml", "[[mods]]\nmodId=\"forgey\"\nversion=\"1\"\n")]);
        jar(&d, "a.jar", &[("fabric.mod.json", r#"{"id":"dupe","version":"1"}"#)]);
        jar(&d, "b.jar", &[("fabric.mod.json", r#"{"id":"dupe","version":"2"}"#)]);
        jar(&d, "libx.jar", &[("fabric.mod.json", r#"{"id":"libx","version":"1.5"}"#)]);
        jar(&d, "hidden.jar.disabled", &[("fabric.mod.json", r#"{"id":"hidden","version":"1"}"#)]);
        std::fs::write(d.join("broken.jar"), b"nope").unwrap();
        let r = analyze(&scan_dir(&d), &fabric());
        let ids: Vec<&str> = r.missing.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["fabric-api"], "fabric-api-base collapses to fabric-api: {ids:?}");
        assert_eq!(r.wrong_version.len(), 1, "libx 1.5 is older than >=2.0");
        assert_eq!(r.disabled.len(), 1);
        assert_eq!(r.duplicates.len(), 1);
        assert_eq!(r.wrong_loader.len(), 1);
        assert_eq!(r.mc_mismatch.len(), 1);
        assert_eq!(r.broken.len(), 1);
    }

    #[test]
    fn nested_jars_satisfy_deps() {
        use std::io::Write;
        let d = tmp("nest");
        // fabric-api style: the outer jar bundles a module jar
        let inner = { let mut buf = std::io::Cursor::new(vec![]); { let mut z = zip::ZipWriter::new(&mut buf);
            z.start_file("fabric.mod.json", zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(br#"{"id":"fabric-api-base","version":"0.4"}"#).unwrap(); z.finish().unwrap(); } buf.into_inner() };
        std::fs::create_dir_all(&d).unwrap();
        let mut z = zip::ZipWriter::new(std::fs::File::create(d.join("fabric-api.jar")).unwrap());
        z.start_file("fabric.mod.json", zip::write::SimpleFileOptions::default()).unwrap(); z.write_all(br#"{"id":"fabric-api","version":"0.9"}"#).unwrap();
        z.start_file("META-INF/jars/fabric-api-base-0.4.jar", zip::write::SimpleFileOptions::default()).unwrap(); z.write_all(&inner).unwrap(); z.finish().unwrap();
        jar(&d, "cool.jar", &[("fabric.mod.json", r#"{"id":"cool","version":"1","depends":{"fabric-api-base":"*","fabric-resource-loader-v0":"*"}}"#)]);
        let r = analyze(&scan_dir(&d), &fabric());
        assert!(r.missing.is_empty(), "{:?}", r.missing);
    }

    #[test]
    fn forge_and_plugins() {
        let d = tmp("forge");
        jar(&d, "f.jar", &[("META-INF/mods.toml", "[[mods]]\nmodId=\"fm\"\nversion=\"${file.jarVersion}\"\n[[dependencies.fm]]\nmodId=\"forge\"\nmandatory=true\nversionRange=\"[47,)\"\n[[dependencies.fm]]\nmodId=\"curios\"\nmandatory=true\nversionRange=\"[5,)\"\nside=\"BOTH\"\n[[dependencies.fm]]\nmodId=\"jei\"\nmandatory=false\n[[dependencies.fm]]\nmodId=\"clientonly\"\nmandatory=true\nside=\"CLIENT\"\n")]);
        let ctx = Ctx { mc: "1.20.1".into(), platform: "forge".into(), server: true, loaders: vec!["forge".into()] };
        let r = analyze(&scan_dir(&d), &ctx);
        let ids: Vec<&str> = r.missing.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["curios"]);
        let p = tmp("plug");
        jar(&p, "Fancy.jar", &[("plugin.yml", "name: Fancy\nversion: 1.2\nmain: a.B\napi-version: '1.21'\ndepend: [LibPlug, NoThere]\nsoftdepend: [Extra]\n")]);
        jar(&p, "LibPlug.jar", &[("plugin.yml", "name: LibPlug\nversion: 3\nmain: a.C\n")]);
        let pc = Ctx { mc: "1.20.4".into(), platform: "paper".into(), server: true, loaders: vec!["paper".into()] };
        let r = analyze(&scan_dir(&p), &pc);
        let ids: Vec<&str> = r.missing.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["nothere"]);
        assert_eq!(r.api_too_new.len(), 1);
    }

    /// End to end against a local stand-in for Modrinth: resolves, downloads, verifies the hash, re-scans.
    #[test]
    fn ensure_installs_from_modrinth_mock() {
        use sha2::{Digest, Sha512};
        use std::io::Write;
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let mut buf = std::io::Cursor::new(vec![]);
        { let mut z = zip::ZipWriter::new(&mut buf); z.start_file("fabric.mod.json", zip::write::SimpleFileOptions::default()).unwrap();
          z.write_all(br#"{"id":"fabric-api","version":"0.9","provides":["fabric-api-base"]}"#).unwrap(); z.finish().unwrap(); }
        let bytes = buf.into_inner();
        let hash = hex::encode(Sha512::digest(&bytes));
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                let path = url.split('?').next().unwrap_or("").to_string();
                let json = |v: serde_json::Value| tiny_http::Response::from_string(v.to_string()).with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap());
                let project = json!({"id": "P1", "slug": "fabric-api", "title": "Fabric API", "project_type": "mod", "loaders": ["fabric"], "server_side": "optional", "icon_url": null});
                let _ = match path.as_str() {
                    "/project/fabric-api" | "/project/P1" => req.respond(json(project)),
                    "/project/P1/version" => req.respond(json(json!([{"id": "V1", "version_number": "0.9", "version_type": "release", "dependencies": [],
                        "files": [{"primary": true, "url": format!("http://127.0.0.1:{port}/dl/fabric-api.jar"), "filename": "fabric-api.jar", "hashes": {"sha512": hash}}]}]))),
                    "/dl/fabric-api.jar" => req.respond(tiny_http::Response::from_data(bytes.clone())),
                    "/search" => req.respond(json(json!({"hits": []}))),
                    _ => req.respond(tiny_http::Response::from_string("{}").with_status_code(404)),
                };
            }
        });
        std::env::set_var("ORBIT_MODRINTH_API", format!("http://127.0.0.1:{port}"));
        let root = tmp("ens");
        let mods = root.join("mods");
        jar(&mods, "cool.jar", &[("fabric.mod.json", r#"{"id":"coolmod","version":"1.0","depends":{"fabric-api-base":"*","ghostlib":"*"}}"#)]);
        let mut inst = crate::store::Instance::open(&root);
        inst.state["config"] = json!({"mc": "1.20.1", "loader": "fabric"});
        let mut log = vec![];
        let out = crate::mods::ensure_deps(&mut inst, &mut log).unwrap();
        assert_eq!(out.installed, vec!["Fabric API".to_string()], "log: {log:?}");
        assert_eq!(out.unresolved.len(), 1);
        assert_eq!(out.unresolved[0].id, "ghostlib");
        assert!(mods.join("fabric-api.jar").is_file());
        assert_eq!(inst.mods()["P1"]["explicit"], false, "recorded as a dependency");
        // second run: nothing left to fetch, nothing re-downloaded
        let again = crate::mods::ensure_deps(&mut inst, &mut vec![]).unwrap();
        assert!(again.installed.is_empty());
    }
}
