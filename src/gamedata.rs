//! Reads Minecraft's own data: worlds (level.dat + stats), servers.dat and live server status.
use flate2::read::GzDecoder;
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::{Duration, Instant};

// ---------- NBT ----------

struct R<'a> { b: &'a [u8], i: usize }

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ()> {
        if self.i + n > self.b.len() { return Err(()); }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
    fn i8(&mut self) -> Result<i8, ()> { Ok(self.take(1)?[0] as i8) }
    fn i16(&mut self) -> Result<i16, ()> { Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap())) }
    fn u16(&mut self) -> Result<u16, ()> { Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap())) }
    fn i32(&mut self) -> Result<i32, ()> { Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap())) }
    fn i64(&mut self) -> Result<i64, ()> { Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap())) }
    fn f32(&mut self) -> Result<f32, ()> { Ok(f32::from_be_bytes(self.take(4)?.try_into().unwrap())) }
    fn f64(&mut self) -> Result<f64, ()> { Ok(f64::from_be_bytes(self.take(8)?.try_into().unwrap())) }
    fn string(&mut self) -> Result<String, ()> {
        let n = self.u16()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    fn len(&mut self) -> Result<usize, ()> {
        let n = self.i32()?;
        if n < 0 || n as usize > self.b.len() { Err(()) } else { Ok(n as usize) }
    }
}

fn payload(r: &mut R, t: i8, depth: u32) -> Result<Value, ()> {
    if depth > 64 { return Err(()); }
    Ok(match t {
        1 => json!(r.i8()?),
        2 => json!(r.i16()?),
        3 => json!(r.i32()?),
        4 => json!(r.i64()?),
        5 => json!(r.f32()?),
        6 => json!(r.f64()?),
        7 => { let n = r.len()?; r.take(n)?; Value::Null }          // byte arrays aren't needed
        8 => json!(r.string()?),
        9 => {
            let it = r.i8()?;
            let n = r.len()?;
            let mut v = Vec::new();
            for _ in 0..n { v.push(payload(r, it, depth + 1)?); }
            Value::Array(v)
        }
        10 => {
            let mut m = Map::new();
            loop {
                let it = r.i8()?;
                if it == 0 { break; }
                let k = r.string()?;
                m.insert(k, payload(r, it, depth + 1)?);
            }
            Value::Object(m)
        }
        11 => { let n = r.len()?; for _ in 0..n { r.i32()?; } Value::Null }
        12 => { let n = r.len()?; for _ in 0..n { r.i64()?; } Value::Null }
        _ => return Err(()),
    })
}

/// Parse an NBT file into plain JSON values. `{}` if it can't be read.
pub fn read_nbt(path: &Path, compressed: Option<bool>) -> Value {
    let Ok(raw) = std::fs::read(path) else { return json!({}) };
    let gz = compressed.unwrap_or(raw.starts_with(&[0x1f, 0x8b]));
    let data = if gz {
        let mut out = Vec::new();
        if GzDecoder::new(&raw[..]).read_to_end(&mut out).is_err() { return json!({}); }
        out
    } else { raw };
    let mut r = R { b: &data, i: 0 };
    match (r.i8(), r.string()) {
        (Ok(10), Ok(_)) => payload(&mut r, 10, 0).unwrap_or_else(|_| json!({})),
        _ => json!({}),
    }
}

// ---------- worlds ----------

fn world_playtime(w: &Path) -> i64 {
    let mut total = 0i64;
    if let Ok(rd) = std::fs::read_dir(w.join("stats")) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|s| s.to_str()) != Some("json") { continue; }
            let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            let c = &v["stats"]["minecraft:custom"];
            total += c["minecraft:play_time"].as_i64().filter(|n| *n != 0)
                .or_else(|| c["minecraft:play_one_minute"].as_i64()).unwrap_or(0);
        }
    }
    total / 20
}

fn mode_name(n: i64) -> &'static str {
    match n { 1 => "Creative", 2 => "Adventure", 3 => "Spectator", _ => "Survival" }
}

pub fn list_worlds(root: &Path) -> Vec<Value> {
    let mut out: Vec<(f64, Value)> = vec![];
    let Ok(rd) = std::fs::read_dir(root.join("saves")) else { return vec![] };
    for e in rd.flatten() {
        let w = e.path();
        if !w.is_dir() { continue; }
        let has_level = w.join("level.dat").is_file();
        let lv = if has_level { read_nbt(&w.join("level.dat"), None)["Data"].clone() } else { Value::Null };
        let lv_empty = lv.as_object().map(|m| m.is_empty()).unwrap_or(true);
        if lv_empty && !w.join("region").is_dir() { continue; }       // not a world
        let mtime = std::fs::metadata(if has_level { w.join("level.dat") } else { w.clone() })
            .and_then(|m| m.modified()).ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let last = lv["LastPlayed"].as_i64().filter(|n| *n > 0).map(|n| n as f64 / 1000.0).unwrap_or(mtime);
        let folder = w.file_name().unwrap().to_string_lossy().into_owned();
        out.push((last, json!({
            "folder": folder,
            "name": lv["LevelName"].as_str().unwrap_or(&folder),
            "last_played": last,
            "mode": mode_name(lv["GameType"].as_i64().unwrap_or(0)),
            "hardcore": lv["hardcore"].as_i64().unwrap_or(0) != 0,
            "version": lv["Version"]["Name"].as_str().unwrap_or(""),
            "played": world_playtime(&w),
            "icon": w.join("icon.png").is_file(),
        })));
    }
    out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    out.into_iter().map(|x| x.1).collect()
}

// ---------- servers ----------

pub fn list_servers(root: &Path) -> Vec<Value> {
    let d = read_nbt(&root.join("servers.dat"), Some(false));
    let mut out = vec![];
    for s in d["servers"].as_array().into_iter().flatten() {
        let Some(ip) = s["ip"].as_str().filter(|x| !x.is_empty()) else { continue };
        let icon = s["icon"].as_str().filter(|x| !x.is_empty()).map(|x| format!("data:image/png;base64,{x}")).unwrap_or_default();
        out.push(json!({"name": s["name"].as_str().filter(|x| !x.is_empty()).unwrap_or(ip), "ip": ip, "icon": icon}));
    }
    out
}

fn varint(mut n: u32) -> Vec<u8> {
    let mut out = vec![];
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        out.push(if n != 0 { b | 0x80 } else { b });
        if n == 0 { return out; }
    }
}

fn read_varint(s: &mut TcpStream) -> std::io::Result<u32> {
    let mut n = 0u32;
    for i in 0..5 {
        let mut b = [0u8; 1];
        s.read_exact(&mut b)?;
        n |= ((b[0] & 0x7f) as u32) << (7 * i);
        if b[0] & 0x80 == 0 { return Ok(n); }
    }
    Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad varint"))
}

fn motd_text(d: &Value) -> String {
    match d {
        Value::String(s) => s.clone(),
        Value::Object(_) => {
            let mut s = d["text"].as_str().unwrap_or("").to_string();
            for e in d["extra"].as_array().into_iter().flatten() { s += &motd_text(e); }
            s
        }
        Value::Array(a) => a.iter().map(motd_text).collect(),
        _ => String::new(),
    }
}

fn strip_codes(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '§' { it.next(); } else { out.push(c); }
    }
    out.trim().to_string()
}

fn split_host(ip: &str) -> (String, u16) {
    let ip = ip.trim();
    if let Some(rest) = ip.strip_prefix('[') {                       // [::1]:25565
        if let Some((h, tail)) = rest.split_once(']') {
            let p = tail.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(25565);
            return (h.to_string(), p);
        }
    }
    if ip.matches(':').count() == 1 {
        if let Some((h, p)) = ip.split_once(':') {
            if let Ok(p) = p.parse() { return (h.to_string(), p); }
        }
    }
    (ip.to_string(), 25565)
}

/// Minecraft status ping. Returns {online, players, max, motd, version, ms, icon}.
pub fn ping(ip: &str) -> Value { ping_timeout(ip, Duration::from_secs(2)) }

pub fn ping_timeout(ip: &str, timeout: Duration) -> Value {
    let (host, port) = split_host(ip);
    let run = || -> std::io::Result<Value> {
        let t0 = Instant::now();
        let addr = (host.as_str(), port).to_socket_addrs()?.next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no address"))?;
        let mut s = TcpStream::connect_timeout(&addr, timeout)?;
        s.set_read_timeout(Some(timeout))?;
        s.set_write_timeout(Some(timeout))?;
        let h = host.as_bytes();
        let mut body = vec![0u8];
        body.extend(varint(765));
        body.extend(varint(h.len() as u32));
        body.extend(h);
        body.extend(port.to_be_bytes());
        body.extend(varint(1));
        let mut pkt = varint(body.len() as u32);
        pkt.extend(body);
        pkt.extend([1u8, 0u8]);
        s.write_all(&pkt)?;
        read_varint(&mut s)?;                                         // packet length
        read_varint(&mut s)?;                                         // packet id
        let n = read_varint(&mut s)? as usize;
        if n > 1 << 22 { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "too big")); }
        let mut buf = vec![0u8; n];
        s.read_exact(&mut buf)?;
        let ms = t0.elapsed().as_millis() as u64;
        let j: Value = serde_json::from_slice(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(json!({
            "online": true,
            "players": j["players"]["online"].as_i64().unwrap_or(0),
            "max": j["players"]["max"].as_i64().unwrap_or(0),
            "motd": strip_codes(&motd_text(&j["description"])),
            "version": j["version"]["name"].as_str().unwrap_or(""),
            "ms": ms,
            "sample": j["players"]["sample"].as_array().into_iter().flatten().filter_map(|p| p["name"].as_str()).map(|n| strip_codes(n)).collect::<Vec<_>>(),
            "icon": j["favicon"].as_str().unwrap_or(""),
        }))
    };
    run().unwrap_or_else(|_| json!({"online": false}))
}

pub fn ping_all(mut servers: Vec<Value>) -> Vec<Value> {
    let ips: Vec<String> = servers.iter().map(|s| s["ip"].as_str().unwrap_or("").to_string()).collect();
    let handles: Vec<_> = ips.into_iter().map(|ip| std::thread::spawn(move || ping(&ip))).collect();
    for (s, h) in servers.iter_mut().zip(handles) {
        let mut r = h.join().unwrap_or_else(|_| json!({"online": false}));
        let icon = r.as_object_mut().and_then(|m| m.remove("icon")).and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        if s["icon"].as_str().unwrap_or("").is_empty() && !icon.is_empty() { s["icon"] = json!(icon); }
        s["status"] = r;
    }
    servers
}

// ---------- mods placed by hand ----------

fn jar_title(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut z = zip::ZipArchive::new(f).ok()?;
    let read = |z: &mut zip::ZipArchive<std::fs::File>, n: &str| -> Option<String> {
        let mut e = z.by_name(n).ok()?;
        let mut s = String::new();
        e.read_to_string(&mut s).ok()?;
        Some(s)
    };
    for n in ["fabric.mod.json", "quilt.mod.json"] {
        if let Some(t) = read(&mut z, n) {
            if let Ok(j) = serde_json::from_str::<Value>(t.trim_start_matches('\u{feff}')) {
                let j = if n.starts_with("quilt") { j["quilt_loader"]["metadata"].clone() } else { j };
                if let Some(name) = j["name"].as_str().map(|s| s.trim()).filter(|s| !s.is_empty()) { return Some(name.to_string()); }
            }
        }
    }
    let re = regex::Regex::new(r#"displayName\s*=\s*["']([^"']+)["']"#).unwrap();
    for n in ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"] {
        if let Some(t) = read(&mut z, n) { if let Some(m) = re.captures(&t) { return Some(m[1].trim().to_string()); } }
    }
    None
}

/// Register content files that are in the instance folders but not in its state (dropped in by hand), and
/// forget entries Orbit only knew as 'imported' whose file was deleted by hand. Returns (changed, new keys).
pub fn sync_untracked(inst: &mut crate::store::Instance) -> (bool, Vec<String>) {
    use crate::tl;
    let mut tracked: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in inst.mods().values() {
        match m["relpath"].as_str().filter(|r| !r.is_empty()) {
            Some(r) => { tracked.insert(r.replace('\\', "/").to_lowercase()); }
            None => {
                let d = inst.dir_for(m["type"].as_str());
                tracked.insert(format!("{}/{}", d.file_name().unwrap_or_default().to_string_lossy(), m["filename"].as_str().unwrap_or("")).to_lowercase());
            }
        }
    }
    let (mut changed, mut new) = (false, vec![]);
    for dname in tl::CONTENT_DIRS {
        let d = inst.root.join(dname);
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let mut files: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            let low = name.to_lowercase();
            let is_file = std::fs::symlink_metadata(&f).map(|m| m.is_file()).unwrap_or(false);
            if !is_file || !tl::has_ext(&name) { continue; }
            let stem = if low.ends_with(".disabled") { name[..name.len() - 9].to_string() } else { name.clone() };
            if !tracked.insert(format!("{dname}/{stem}").to_lowercase()) { continue; }
            let key = format!("imp:{dname}/{stem}");
            if inst.mods().contains_key(&key) { continue; }
            let Some(t) = crate::mods::dir_type(dname) else { continue };
            let pstem = Path::new(&stem).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let title = if dname == "mods" { jar_title(&f) } else { None }.unwrap_or_else(|| pstem.clone());
            inst.mods_mut().insert(key.clone(), json!({"type": t, "title": title, "slug": pstem, "source": "imported", "version_id": "", "version": "",
                "filename": stem, "explicit": true, "disabled": low.ends_with(".disabled")}));
            new.push(key);
            changed = true;
        }
    }
    let gone: Vec<String> = inst.mods().iter().filter(|(_, m)| m["source"] == "imported" && m["relpath"].as_str().map(|r| r.is_empty()).unwrap_or(true)).filter(|(_, m)| {
        let p = inst.dir_for(m["type"].as_str()).join(m["filename"].as_str().unwrap_or(""));
        !p.exists() && !crate::mods::disabled_path(&p).exists()
    }).map(|(k, _)| k.clone()).collect();
    for k in gone { inst.mods_mut().remove(&k); changed = true; }
    (changed, new)
}

#[cfg(test)]
mod ping_tests {
    use super::*;
    use std::net::TcpListener;

    /// A fake server answering the status handshake the way real ones do (1.12 included).
    #[test]
    fn reads_the_live_player_count() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf);
            let json = br#"{"version":{"name":"1.12.2","protocol":340},"players":{"max":20,"online":3,"sample":[{"name":"Alex","id":"x"},{"name":"Steve","id":"y"}]},"description":{"text":"hi"}}"#;
            let mut body = vec![0u8];
            body.extend(varint(json.len() as u32));
            body.extend(json);
            let mut pkt = varint(body.len() as u32);
            pkt.extend(body);
            s.write_all(&pkt).unwrap();
        });
        let r = ping_timeout(&format!("127.0.0.1:{port}"), Duration::from_secs(2));
        assert_eq!((r["online"].clone(), r["players"].clone(), r["max"].clone()), (json!(true), json!(3), json!(20)));
        assert_eq!(r["sample"], json!(["Alex", "Steve"]));
    }
}
