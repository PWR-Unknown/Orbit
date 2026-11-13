//! Small shared helpers: HTTP (JSON + file downloads with hash checks and progress), hashing,
//! background-job progress tracking, and path safety.
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type Res<T = Value> = Result<T, String>;

pub const UA: &str = concat!("Orbit/", env!("CARGO_PKG_VERSION"), " (Minecraft launcher)");

pub fn agent(secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(15)).timeout(Duration::from_secs(secs)).user_agent(UA).build()
}

fn http_err(url: &str, e: ureq::Error) -> String {
    let host = url.split('/').nth(2).unwrap_or(url);
    match e {
        ureq::Error::Status(c, _) => format!("{host} answered {c} for that request."),
        ureq::Error::Transport(t) => format!("Couldn't reach {host}: {t}"),
    }
}

/// GET a JSON document. Errors are readable messages.
pub fn get_json(url: &str) -> Res {
    agent(30).get(url).call().map_err(|e| http_err(url, e))?.into_json::<Value>().map_err(|e| e.to_string())
}

pub fn get_text(url: &str) -> Res<String> {
    agent(30).get(url).call().map_err(|e| http_err(url, e))?.into_string().map_err(|e| e.to_string())
}

pub fn hash_file(path: &Path, algo: &str) -> Res<String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 16];
    macro_rules! run { ($h:expr) => {{
        let mut h = $h;
        loop { let n = f.read(&mut buf).map_err(|e| e.to_string())?; if n == 0 { break; } h.update(&buf[..n]); }
        hex::encode(h.finalize())
    }}; }
    Ok(match algo { "sha1" => run!(Sha1::new()), "sha256" => run!(Sha256::new()), _ => run!(Sha512::new()) })
}

pub fn sha1_file(p: &Path) -> Res<String> { hash_file(p, "sha1") }

// ---------- progress of the running job (one at a time) ----------

#[derive(Default)]
pub struct Track {
    pub expected: usize, pub started: usize, pub done: usize,
    pub cur: u64, pub cur_total: u64, pub label: String,
}

impl Track {
    pub fn snapshot(&self) -> Value {
        let total = self.expected.max(self.started).max(1) as f64;
        let frac = if self.cur_total > 0 { self.cur as f64 / self.cur_total as f64 } else { 0.0 };
        serde_json::json!({"pct": ((self.done as f64 + frac) / total).min(0.99), "label": self.label,
            "files": self.done, "total": total as usize, "bytes": self.cur, "size": self.cur_total})
    }
}

pub type SharedTrack = Arc<Mutex<Track>>;
static CURRENT: Mutex<Option<SharedTrack>> = Mutex::new(None);
pub fn set_track(t: Option<SharedTrack>) { *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = t; }
pub fn track() -> Option<SharedTrack> { CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone() }
pub fn track_expected(n: usize) { if let Some(t) = track() { t.lock().unwrap().expected = n.max(1); } }

/// Download `url` into `dir/filename` (never trusting a path from the server), verifying a hash.
/// Returns the file name actually written.
pub fn download_file(url: &str, dir: &Path, filename: &str, algo: Option<&str>, expected: Option<&str>) -> Res<String> {
    let filename = Path::new(filename).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if filename.is_empty() { return Err("Empty file name in download.".into()); }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let dest = dir.join(&filename);
    let tmp = dir.join(format!("{filename}.part"));
    let tr = track();
    if let Some(t) = &tr { let mut t = t.lock().unwrap(); t.started += 1; t.label = filename.clone(); t.cur = 0; t.cur_total = 0; }
    let fail = |m: String| { let _ = std::fs::remove_file(&tmp); Err(m) };
    let resp = match agent(120).get(url).call() { Ok(r) => r, Err(e) => return fail(format!("Download failed for {filename}: {}", http_err(url, e))) };
    if let Some(t) = &tr { t.lock().unwrap().cur_total = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0); }
    let mut out = match std::fs::File::create(&tmp) { Ok(f) => f, Err(e) => return fail(format!("Download failed for {filename}: {e}")) };
    let mut rd = resp.into_reader();
    let mut buf = vec![0u8; 1 << 16];
    let (mut h1, mut h512, mut h256) = (Sha1::new(), Sha512::new(), Sha256::new());
    loop {
        let n = match rd.read(&mut buf) { Ok(n) => n, Err(e) => return fail(format!("Download failed for {filename}: {e}")) };
        if n == 0 { break; }
        match algo { Some("sha1") => h1.update(&buf[..n]), Some("sha256") => h256.update(&buf[..n]), Some(_) => h512.update(&buf[..n]), None => {} }
        if let Err(e) = out.write_all(&buf[..n]) { return fail(format!("Download failed for {filename}: {e}")); }
        if let Some(t) = &tr { t.lock().unwrap().cur += n as u64; }
    }
    drop(out);
    if let (Some(a), Some(exp)) = (algo, expected) {
        let got = match a { "sha1" => hex::encode(h1.finalize()), "sha256" => hex::encode(h256.finalize()), _ => hex::encode(h512.finalize()) };
        if !got.eq_ignore_ascii_case(exp) { return fail(format!("Hash mismatch for {filename}, download discarded.")); }
    }
    std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    if let Some(t) = &tr { let mut t = t.lock().unwrap(); t.done += 1; t.cur = 0; t.cur_total = 0; }
    Ok(filename)
}

/// `dir/rel`, refusing anything that would land outside `dir`.
pub fn safe_join(dir: &Path, rel: &str) -> Res<PathBuf> {
    let rel = rel.replace('\\', "/");
    if rel.starts_with('/') || rel.split('/').any(|c| c == "..") || rel.contains(':') {
        return Err(format!("Modpack tried to write outside its instance: {rel}"));
    }
    Ok(dir.join(rel))
}

pub fn host_of(url: &str) -> String {
    url.split("://").nth(1).unwrap_or("").split(['/', '?', '#']).next().unwrap_or("").split('@').last().unwrap_or("").split(':').next().unwrap_or("").to_lowercase()
}


pub fn run_parallel<T: Send + Sync, F: Fn(&T) -> Res<()> + Send + Sync>(items: &[T], workers: usize, f: F, mut prog: impl FnMut(usize, usize)) -> Res<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let err: Mutex<Option<String>> = Mutex::new(None);
    let total = items.len();
    std::thread::scope(|s| {
        for _ in 0..workers.max(1).min(total.max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= total || err.lock().unwrap().is_some() { break; }
                match f(&items[i]) { Ok(()) => { done.fetch_add(1, Ordering::SeqCst); } Err(e) => { *err.lock().unwrap() = Some(e); break; } }
            });
        }
        let mut last = 0;
        while done.load(Ordering::SeqCst) < total && err.lock().unwrap().is_none() {
            let d = done.load(Ordering::SeqCst);
            if d != last { prog(d, total); last = d; }
            std::thread::sleep(Duration::from_millis(120));
        }
    });
    prog(done.load(Ordering::SeqCst), total);
    match err.into_inner().unwrap() { Some(e) => Err(e), None => Ok(()) }
}
