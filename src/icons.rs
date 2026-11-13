//! Icons for content that isn't on Modrinth / CurseForge (or hasn't been matched yet): the logo is read
//! straight out of the mod / pack file itself.
//!   mods:           fabric.mod.json "icon", quilt.mod.json "icon", mods.toml / neoforge.mods.toml "logoFile"
//!   resource packs: pack.png  (also used as a fallback for everything else)
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

const MAX_ICON: u64 = 1_500_000;
type Found = Option<(Vec<u8>, &'static str)>;
type Cached = ((u64, u64), Found);
static CACHE: Mutex<Option<HashMap<String, Cached>>> = Mutex::new(None);

fn read_entry<R: Read + Seek>(z: &mut zip::ZipArchive<R>, name: &str, limit: u64) -> Option<Vec<u8>> {
    let name = name.trim_start_matches("./").trim_start_matches('/');
    let e = z.by_name(name).ok()?;
    if e.size() > limit { return None; }
    let mut buf = Vec::with_capacity(e.size() as usize);
    e.take(limit).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Only real raster images are served (never SVG), whatever the file claims to be.
fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") { Some("image/png") }
    else if b.starts_with(&[0xff, 0xd8, 0xff]) { Some("image/jpeg") }
    else if b.starts_with(b"GIF8") { Some("image/gif") }
    else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" { Some("image/webp") }
    else { None }
}

/// "icon" is either a path or {"16": path, "32": path, ...}; take the smallest one that is at least 64 px.
fn pick(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(o) => {
            let mut sizes: Vec<(u32, &str)> = o.iter().filter_map(|(k, v)| Some((k.parse::<u32>().ok()?, v.as_str()?))).collect();
            sizes.sort_by_key(|x| x.0);
            sizes.iter().find(|(n, _)| *n >= 64).or(sizes.last()).map(|(_, p)| p.to_string())
        }
        _ => None,
    }
}

fn logo_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"logoFile\s*=\s*["']([^"']+)["']"#).unwrap())
}

fn declared_icon(z: &mut zip::ZipArchive<std::fs::File>) -> Option<String> {
    for n in ["fabric.mod.json", "quilt.mod.json"] {
        let Some(b) = read_entry(z, n, 1 << 20) else { continue };
        let Ok(j) = serde_json::from_slice::<Value>(b.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&b)) else { continue };
        let icon = if n.starts_with("quilt") { &j["quilt_loader"]["metadata"]["icon"] } else { &j["icon"] };
        if let Some(p) = pick(icon) { return Some(p); }
    }
    for n in ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"] {
        let Some(b) = read_entry(z, n, 1 << 20) else { continue };
        if let Some(c) = logo_re().captures(&String::from_utf8_lossy(&b)) { return Some(c[1].to_string()); }
    }
    None
}

fn read_icon(path: &Path) -> Found {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()?;
    let mut tries: Vec<String> = declared_icon(&mut z).into_iter().collect();
    tries.push("pack.png".into());
    for p in tries {
        if let Some(b) = read_entry(&mut z, &p, MAX_ICON) {
            if let Some(ct) = sniff(&b) { return Some((b, ct)); }
        }
    }
    None
}

/// The icon inside `path` (a .jar / .zip, possibly with a .disabled suffix), cached until the file changes.
pub fn icon_for(path: &Path) -> Found {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    let (key, sig) = (path.to_string_lossy().into_owned(), (md.len(), mtime));
    if let Some((s, v)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&key)) {
        if *s == sig { return v.clone(); }
    }
    let found = read_icon(path);
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    if m.len() > 600 { m.clear(); }
    m.insert(key, (sig, found.clone()));
    found
}
