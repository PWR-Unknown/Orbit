//! Export an Orbit instance as a Modrinth modpack (.mrpack), format version 1.
//! Mods/packs found on Modrinth (by SHA-1) become downloads; everything else goes in overrides/.
use crate::modrinth as mr;
use crate::store::{Instance, STATE_NAME};
use serde_json::{json, Map, Value};
use sha1::Sha1;
use sha2::{Digest, Sha512};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

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
        v = crate::tl::loader_versions(loader, mc)
            .map_err(|e| format!("Couldn't work out which {} version this instance uses: {e}", crate::tl::label(loader)))?["stable"]
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
        if let Some(o) = mr::api_post("/version_files", &json!({"hashes": chunk, "algorithm": "sha1"}))?.as_object() {
            found.extend(o.clone());
        }
    }
    let mut pids: Vec<String> = found.values().filter_map(|v| v["project_id"].as_str().map(String::from)).collect();
    pids.sort();
    pids.dedup();
    let mut projects: HashMap<String, Value> = HashMap::new();
    for chunk in pids.chunks(100) {
        let ids = serde_json::to_string(chunk).unwrap();
        for p in mr::api_get("/projects", &[("ids", ids)])?.as_array().into_iter().flatten() {
            if let Some(id) = p["id"].as_str() { projects.insert(id.to_string(), p.clone()); }
        }
    }
    let mut out = HashMap::new();
    for (h, v) in found {
        let file = v["files"].as_array().into_iter().flatten().find(|f| f["hashes"]["sha1"].as_str() == Some(&h));
        if let Some(url) = file.and_then(|f| f["url"].as_str()).filter(|u| mr::host_allowed(u)) {
            let proj = projects.get(v["project_id"].as_str().unwrap_or("")).cloned().unwrap_or(json!({}));
            out.insert(h, (url.to_string(), proj));
        }
    }
    Ok(out)
}

pub fn build(inst: &Instance, dest: &Path, name: &str, version: &str, description: &str, include: &[String]) -> Result<Value, String> {
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
