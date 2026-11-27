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
    /// Set by the Cancel button in the downloads panel; downloads stop at the next chunk.
    pub cancel: bool,
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
    if tr.as_ref().map(|t| t.lock().unwrap().cancel).unwrap_or(false) { return Err("Cancelled.".into()); }
    if let Some(t) = &tr { let mut t = t.lock().unwrap(); t.started += 1; t.label = filename.clone(); t.cur = 0; t.cur_total = 0; }
    let fail = |m: String| { let _ = std::fs::remove_file(&tmp); Err(m) };
    let resp = match agent(120).get(url).call() { Ok(r) => r, Err(e) => return fail(format!("Download failed for {filename}: {}", http_err(url, e))) };
    if let Some(t) = &tr { t.lock().unwrap().cur_total = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0); }
    let mut out = match std::fs::File::create(&tmp) { Ok(f) => f, Err(e) => return fail(format!("Download failed for {filename}: {e}")) };
    let mut rd = resp.into_reader();
    let mut buf = vec![0u8; 1 << 16];
    let (mut h1, mut h512, mut h256, mut h5) = (Sha1::new(), Sha512::new(), Sha256::new(), md5::Md5::new());
    loop {
        let n = match rd.read(&mut buf) { Ok(n) => n, Err(e) => return fail(format!("Download failed for {filename}: {e}")) };
        if n == 0 { break; }
        if tr.as_ref().map(|t| t.lock().unwrap().cancel).unwrap_or(false) { return fail("Cancelled.".into()); }
        match algo { Some("sha1") => h1.update(&buf[..n]), Some("sha256") => h256.update(&buf[..n]), Some("md5") => h5.update(&buf[..n]), Some(_) => h512.update(&buf[..n]), None => {} }
        if let Err(e) = out.write_all(&buf[..n]) { return fail(format!("Download failed for {filename}: {e}")); }
        if let Some(t) = &tr { t.lock().unwrap().cur += n as u64; }
    }
    drop(out);
    if let (Some(a), Some(exp)) = (algo, expected) {
        let got = match a { "sha1" => hex::encode(h1.finalize()), "sha256" => hex::encode(h256.finalize()), "md5" => hex::encode(h5.finalize()), _ => hex::encode(h512.finalize()) };
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

/// Resolve `rel` inside `root`, refusing anything that climbs out of it (`..`, absolute paths, symlinks).
/// `what` names the folder in the error ("server", "instance").
pub fn resolve_in(root: &Path, rel: &str, what: &str) -> Res<PathBuf> {
    let out = || format!("That path is outside the {what} folder.");
    let base = root.canonicalize().map_err(|e| e.to_string())?;
    let mut t = base.clone();
    for c in Path::new(rel.trim_start_matches(['/', '\\'])).components() {
        match c {
            std::path::Component::Normal(p) => t.push(p),
            std::path::Component::ParentDir => { if !t.pop() || !t.starts_with(&base) { return Err(out()); } }
            std::path::Component::CurDir => {}
            _ => return Err(out()),
        }
    }
    let t = if t.exists() { t.canonicalize().map_err(|e| e.to_string())? } else { t };
    if t != base && !t.starts_with(&base) { return Err(out()); }
    Ok(t)
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

/// Installed RAM in MB (0 when it can't be read).
pub fn system_memory_mb() -> i64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    (sys.total_memory() / (1024 * 1024)) as i64
}

/// The most memory a game may be given (Settings > Java & memory): the installed RAM in whole 256 MB steps, 65536 when unknown.
pub fn max_game_memory_mb() -> i64 {
    let m = system_memory_mb();
    if m > 0 { (m / 256 * 256).max(512) } else { 65536 }
}

// ---------- versions ----------

/// A parsed version: the numeric parts plus an optional pre-release tag (`1.21-beta.2` -> [1, 21] + "beta.2").
/// A pre-release sorts BELOW the same release. Build metadata after `+` is ignored, and so is a numeric suffix
/// after `-` (`1.7.10-10.13.4.1614-1.7.10` style maven names); a suffix that starts with a letter is the pre-release tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ver { pub nums: Vec<u64>, pub pre: Option<String> }

impl Ver {
    pub fn release(nums: Vec<u64>) -> Ver { Ver { nums, pre: None } }
    pub fn is_release(&self) -> bool { self.pre.is_none() }
}

/// Compare numeric parts, a missing part counting as 0 (`1.21` == `1.21.0`).
pub fn cmp_nums(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y { return x.cmp(&y); }
    }
    std::cmp::Ordering::Equal
}

/// Pre-release tags compare dot part by dot part; inside a part, digit runs compare as numbers (`pre2` < `pre10`).
fn cmp_pre(a: &str, b: &str) -> std::cmp::Ordering {
    fn chunks(s: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = vec![];
        for c in s.to_lowercase().chars() {
            let d = c.is_ascii_digit();
            match out.last_mut() { Some((ld, t)) if *ld == d => t.push(c), _ => out.push((d, c.to_string())) }
        }
        out
    }
    let (ca, cb): (Vec<_>, Vec<_>) = (a.split(['.', '-', '_']).flat_map(chunks).collect(), b.split(['.', '-', '_']).flat_map(chunks).collect());
    for (x, y) in ca.iter().zip(cb.iter()) {
        let o = match (x.0, y.0) {
            (true, true) => { let (p, q) = (x.1.trim_start_matches('0'), y.1.trim_start_matches('0')); p.len().cmp(&q.len()).then(p.cmp(q)) }
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            (false, false) => x.1.cmp(&y.1),
        };
        if o != std::cmp::Ordering::Equal { return o; }
    }
    ca.len().cmp(&cb.len())
}

impl Ord for Ver {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        cmp_nums(&self.nums, &o.nums).then_with(|| match (&self.pre, &o.pre) {
            (None, None) => std::cmp::Ordering::Equal,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(a), Some(b)) => cmp_pre(a, b),
        })
    }
}
impl PartialOrd for Ver { fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) } }

/// The one version parser. None when the text isn't a version: it must start with a digit and every dot part of the
/// numeric core must be a plain number (`24w10a`, `1.2.x`, `1.2.3a` are None, never silently 0).
pub fn parse_ver(s: &str) -> Option<Ver> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    if !s.starts_with(|c: char| c.is_ascii_digit()) { return None; }
    let s = s.split('+').next().unwrap_or("");
    let (core, suffix) = match s.split_once(['-', '_', ' ']) { Some((c, r)) => (c, r.trim()), None => (s, "") };
    let nums = core.split('.').map(|p| p.parse::<u64>().ok()).collect::<Option<Vec<u64>>>()?;
    let pre = suffix.starts_with(|c: char| c.is_ascii_alphabetic()).then(|| suffix.to_string());
    Some(Ver { nums, pre })
}

/// Order two version strings, newest = Greater. A string that isn't a version sorts below every real one
/// (and two of those compare as text), so a bad entry sinks to the bottom instead of being read as `0`.
pub fn cmp_version_str(a: &str, b: &str) -> std::cmp::Ordering {
    match (parse_ver(a), parse_ver(b)) {
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.cmp(b)),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => a.cmp(b),
    }
}

/// Is `latest` a newer version than `current`? False when either isn't a version.
pub fn is_newer_version(latest: &str, current: &str) -> bool {
    match (parse_ver(latest), parse_ver(current)) { (Some(a), Some(b)) => a > b, _ => false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn pre_releases_sort_below_the_release() {
        assert!(parse_ver("1.21-beta") < parse_ver("1.21"));
        assert!(parse_ver("21.1.0-beta") < parse_ver("21.1.0"));
        assert!(parse_ver("21.1.0-beta") > parse_ver("21.0.9"));
        assert!(parse_ver("1.0.0-alpha") < parse_ver("1.0.0-beta") && parse_ver("1.0.0-beta") < parse_ver("1.0.0-rc1"));
        assert!(parse_ver("1.0.0-pre2") < parse_ver("1.0.0-pre10"));
        assert_eq!(parse_ver("1.21").unwrap().cmp(&parse_ver("1.21.0").unwrap()), Equal);
        assert_eq!(parse_ver("v0.4.2+build.7"), Some(Ver::release(vec![0, 4, 2])));
    }

    #[test]
    fn unparseable_is_none_not_zero() {
        for bad in ["", "24w10a", "1.2.x", "1.2.3a", "abc", "1..2", "latest"] { assert!(parse_ver(bad).is_none(), "{bad}"); }
        assert_eq!(cmp_version_str("24w10a", "1.0"), Less);
        assert_eq!(cmp_version_str("1.10", "1.9"), Greater);
        assert!(!is_newer_version("junk", "1.0") && !is_newer_version("1.1", "junk"));
        assert!(is_newer_version("0.5.0", "0.5.0-beta") && !is_newer_version("0.5.0-beta", "0.5.0"));
    }

    #[test]
    fn numeric_suffix_is_a_build_not_a_pre_release() {
        assert_eq!(parse_ver("10.13.4.1614-1.7.10"), Some(Ver::release(vec![10, 13, 4, 1614])));
    }

    #[test]
    fn resolve_in_refuses_escapes() {
        let d = std::env::temp_dir().join(format!("orbit-res-{:08x}", rand::random::<u32>()));
        std::fs::create_dir_all(d.join("a")).unwrap();
        assert!(resolve_in(&d, "a", "x").is_ok() && resolve_in(&d, "", "x").is_ok() && resolve_in(&d, "a/../a", "x").is_ok());
        assert!(resolve_in(&d, "..", "x").is_err() && resolve_in(&d, "a/../../b", "x").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
