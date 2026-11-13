//! Crash diagnostics: read what a crashed game / server left behind and say what most likely went wrong.
//!
//! Evidence comes from three places that are combined:
//!   1. the files themselves: `crash-reports/*.txt`, `logs/latest.log`, Orbit's own launch log, JVM `hs_err_pid*.log`
//!      and, for servers Orbit started, the console it captured;
//!   2. a scan of the mods / plugins folder (`deps.rs`): missing or disabled dependencies, duplicates, jars built for
//!      another loader or Minecraft version, client-only mods on a server, damaged jars;
//!   3. stack traces, mapped back to the mod that owns the failing class (by the jar named in the trace, or by
//!      looking the class up inside the installed jars).
//! Every finding gets a confidence score; the highest one is reported as the likely root cause, with the evidence
//! lines that led there and concrete steps. Loading-phase failures (missing dependency, wrong Java, mixin) outrank a
//! stack trace because the trace is usually only the symptom.
use crate::deps::{self, Ctx, ModInfo, Report};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub struct Input {
    pub kind: &'static str,            // "instance" | "server"
    pub name: String,
    pub root: PathBuf,
    pub mc: String,
    pub platform: String,              // loader / server type
    pub server: bool,
    pub ram_mb: Option<i64>,
    pub port: Option<i64>,
    pub mods_dir: Option<PathBuf>,
    pub loaders: Vec<String>,
    /// Text Orbit captured itself (server console, the launcher's last error).
    pub console: Vec<String>,
}

#[derive(Clone, Default)]
struct Suspect { id: String, name: String, file: String }

#[derive(Clone, Default)]
struct Finding {
    id: &'static str, title: String, summary: String, score: i32, found_by: &'static str,
    evidence: Vec<String>, fixes: Vec<String>, suspects: Vec<Suspect>, action: Option<&'static str>, install: Vec<String>,
}

impl Finding {
    fn new(id: &'static str, title: &str, score: i32) -> Finding { Finding { id, title: title.into(), score, found_by: "log", ..Default::default() } }
    fn sum(mut self, s: impl Into<String>) -> Self { self.summary = s.into(); self }
    fn fix(mut self, s: impl Into<String>) -> Self { self.fixes.push(s.into()); self }
    fn ev(mut self, v: Vec<String>) -> Self { self.evidence = v; self }
    fn who(mut self, v: Vec<Suspect>) -> Self { self.suspects = v; self }
    fn act(mut self, a: &'static str) -> Self { self.action = Some(a); self }
}

struct Source { kind: &'static str, name: String, mtime: f64, text: String }
struct Line { src: usize, text: String }
struct Hit { idx: usize, caps: Vec<String> }

#[derive(Clone)]
struct Entry { idx: usize, cls: String, msg: String, frames: Vec<(String, String)> }  // frames: (class, jar hint)
type Chain = Vec<Entry>;

fn re(p: &str) -> Regex { Regex::new(p).unwrap() }

fn tail(path: &Path, max: u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.seek(SeekFrom::End(0)).ok()?;
    f.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    let mut b = vec![];
    f.read_to_end(&mut b).ok()?;
    let s = String::from_utf8_lossy(&b).into_owned();
    Some(if len > max { s.split_once('\n').map(|x| x.1.to_string()).unwrap_or(s) } else { s })
}

fn mtime(p: &Path) -> f64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn newest(dir: &Path, pick: impl Fn(&str) -> bool) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path())
        .filter(|p| p.is_file() && p.file_name().map(|n| pick(&n.to_string_lossy())).unwrap_or(false))
        .max_by(|a, b| mtime(a).partial_cmp(&mtime(b)).unwrap_or(std::cmp::Ordering::Equal))
}

fn gather(inp: &Input) -> Vec<Source> {
    let mut v = vec![];
    let mut add = |kind: &'static str, p: Option<PathBuf>, max: u64| {
        if let Some(p) = p.filter(|p| p.is_file()) {
            if let Some(text) = tail(&p, max) { v.push(Source { kind, name: p.file_name().unwrap().to_string_lossy().into_owned(), mtime: mtime(&p), text }); }
        }
    };
    add("crash-report", newest(&inp.root.join("crash-reports"), |n| n.starts_with("crash-") && n.ends_with(".txt")), 1 << 20);
    add("jvm-crash", newest(&inp.root, |n| n.starts_with("hs_err_pid") && n.ends_with(".log")), 256 << 10);
    add("log", Some(inp.root.join("logs").join("latest.log")), 2 << 20);
    add("launch-log", Some(inp.root.join("logs").join("quarry-launch.log")), 1 << 20);
    if !inp.console.is_empty() { v.push(Source { kind: "console", name: "Orbit console".into(), mtime: 0.0, text: inp.console.join("\n") }); }
    v
}

// ---------------------------------------------------------------- stack traces

fn exc_re() -> Regex { re(r#"^(?:\[[^\]]*\]\s*)*(?:\([^)]*\)\s*)?(?:Exception in thread "[^"]*"\s+)?(Caused by:\s+)?((?:[A-Za-z_$][\w$]*\.)+[A-Z][\w$]*(?:Exception|Error|Throwable))(?::\s*(.*))?$"#) }
fn frame_re() -> Regex { re(r"^\s*at\s+(?:[\w.$]+//)?([\w$.<>]+)\.[\w$<>]+\([^)]*\)(.*)$") }
fn hint_re() -> Regex { re(r"\[([^\[\]]+?\.jar)(?:%23\d+)?[!:/\]]") }

const IGNORE_CLASS: [&str; 22] = ["java.", "jdk.", "sun.", "javax.", "com.sun.", "net.minecraft.", "com.mojang.", "org.lwjgl.", "io.netty.", "org.apache.", "com.google.", "it.unimi.",
    "net.fabricmc.loader.", "net.minecraftforge.fml.", "net.minecraftforge.eventbus.", "net.neoforged.fml.", "net.neoforged.bus.", "cpw.mods.", "org.spongepowered.", "org.bukkit.craftbukkit.", "org.quiltmc.loader.", "org.objectweb."];
const LOADER_JARS: [&str; 10] = ["minecraft", "client-extra", "server-extra", "forge-", "fmlcore", "javafmllanguage", "lowcodelanguage", "mclanguage", "fabric-loader", "intermediary"];

fn chains(lines: &[Line]) -> Vec<(usize, Chain)> {
    let (er, fr, hr) = (exc_re(), frame_re(), hint_re());
    let mut out: Vec<(usize, Chain)> = vec![];
    let mut cur: Option<Chain> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.text.trim_end();
        if let Some(c) = er.captures(t) {
            let e = Entry { idx: i, cls: c[2].to_string(), msg: c.get(3).map(|m| m.as_str().to_string()).unwrap_or_default(), frames: vec![] };
            match (c.get(1).is_some(), cur.as_mut()) {
                (true, Some(ch)) => ch.push(e),
                _ => { if let Some(ch) = cur.take() { let s = ch[0].idx; out.push((s, ch)); } cur = Some(vec![e]); }
            }
        } else if let Some(c) = fr.captures(t) {
            if let Some(ch) = cur.as_mut() {
                let hint = hr.captures(&c[2]).map(|h| h[1].to_string()).unwrap_or_default();
                ch.last_mut().unwrap().frames.push((c[1].to_string(), hint));
            }
        } else if t.trim_start().starts_with("... ") || t.trim_start().starts_with("Suppressed:") || t.trim().is_empty() && cur.as_ref().map(|c| c.last().unwrap().frames.is_empty()).unwrap_or(false) {
            // continuation lines of a trace
        } else if let Some(ch) = cur.take() { let s = ch[0].idx; out.push((s, ch)); }
    }
    if let Some(ch) = cur.take() { let s = ch[0].idx; out.push((s, ch)); }
    out
}

fn is_loader_jar(j: &str) -> bool { let l = j.to_lowercase(); LOADER_JARS.iter().any(|p| l.starts_with(p)) }

/// Which installed jar holds each of these classes?
fn class_owners(inv: &[ModInfo], classes: &[String]) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    if classes.is_empty() { return out; }
    for (i, m) in inv.iter().enumerate() {
        if m.disabled || m.error.is_some() { continue; }
        let Ok(f) = std::fs::File::open(&m.path) else { continue };
        let Ok(mut z) = zip::ZipArchive::new(f) else { continue };
        for c in classes {
            if out.contains_key(c) { continue; }
            if z.by_name(&format!("{}.class", c.replace('.', "/"))).is_ok() { out.insert(c.clone(), i); }
        }
    }
    out
}

// ---------------------------------------------------------------- analysis

struct An<'a> {
    inp: &'a Input, srcs: Vec<Source>, lines: Vec<Line>, inv: Vec<ModInfo>, rep: Report,
    out: Vec<Finding>, chains: Vec<(usize, Chain)>,
}

impl An<'_> {
    fn grep(&self, r: &Regex) -> Vec<Hit> {
        self.lines.iter().enumerate().filter_map(|(i, l)| r.captures(&l.text).map(|c| Hit { idx: i, caps: c.iter().map(|m| m.map(|x| x.as_str().to_string()).unwrap_or_default()).collect() })).collect()
    }
    fn line(&self, i: usize) -> String { self.lines.get(i).map(|l| l.text.trim().chars().take(240).collect()).unwrap_or_default() }
    fn snip(&self, hits: &[Hit], max: usize) -> Vec<String> {
        let mut v: Vec<String> = vec![];
        for h in hits { let t = self.line(h.idx); if !t.is_empty() && !v.contains(&t) { v.push(t); } if v.len() >= max { break; } }
        v
    }
    fn after(&self, i: usize, n: usize) -> Vec<String> { (i + 1..(i + 1 + n).min(self.lines.len())).map(|k| self.line(k)).filter(|s| !s.is_empty()).collect() }

    fn suspect(&self, m: &ModInfo) -> Suspect { Suspect { id: m.id().to_string(), name: m.label(), file: m.file.clone() } }
    fn by_id(&self, id: &str) -> Option<Suspect> {
        let id = id.to_lowercase();
        self.inv.iter().find(|m| m.ids.contains(&id) || m.label().to_lowercase() == id).map(|m| self.suspect(m))
    }
    fn by_jar(&self, jar: &str) -> Option<Suspect> {
        let j = jar.to_lowercase();
        self.inv.iter().find(|m| m.base_file().to_lowercase() == j).map(|m| self.suspect(m))
    }
    /// A file inside some jar (a mixin config, say) -> that jar.
    fn by_entry(&self, entry: &str) -> Option<Suspect> {
        for m in self.inv.iter().filter(|m| !m.disabled && m.error.is_none()) {
            if let Ok(f) = std::fs::File::open(&m.path) { if let Ok(mut z) = zip::ZipArchive::new(f) { if z.by_name(entry).is_ok() { return Some(self.suspect(m)); } } }
        }
        None
    }

    /// The mods that own the frames of this chain, the deepest cause first.
    fn blame(&self, ch: &Chain) -> Vec<Suspect> {
        let mut classes: Vec<String> = vec![];
        for e in ch.iter().rev() {
            for (c, hint) in e.frames.iter().take(30) {
                if hint.is_empty() && !IGNORE_CLASS.iter().any(|p| c.starts_with(p)) && !classes.contains(c) && classes.len() < 40 { classes.push(c.clone()); }
            }
        }
        let owners = class_owners(&self.inv, &classes);
        let mut out: Vec<Suspect> = vec![];
        let mut push = |s: Suspect| if !s.file.is_empty() && !out.iter().any(|o| o.file == s.file) { out.push(s); };
        for e in ch.iter().rev() {
            for (c, hint) in e.frames.iter().take(30) {
                if !hint.is_empty() && !is_loader_jar(hint) { if let Some(s) = self.by_jar(hint) { push(s); } }
                else if let Some(i) = owners.get(c) { push(self.suspect(&self.inv[*i])); }
            }
        }
        out.truncate(3);
        out
    }

    fn primary_chain(&self) -> Option<&Chain> {
        let kind = |c: &(usize, Chain)| self.srcs[self.lines[c.0].src].kind;
        self.chains.iter().find(|c| kind(c) == "crash-report").or_else(|| self.chains.iter().rev().find(|c| c.1.len() > 1 || c.1[0].frames.len() > 2)).or(self.chains.last()).map(|c| &c.1)
    }

    fn add(&mut self, f: Finding) {
        if let Some(o) = self.out.iter_mut().find(|o| o.id == f.id) {
            if f.found_by != o.found_by { o.found_by = "both"; }
            o.score = o.score.max(f.score) + if f.found_by != "log" || o.found_by == "both" { 4 } else { 0 };
            for e in f.evidence { if !o.evidence.contains(&e) { o.evidence.push(e); } }
            for x in f.fixes { if !o.fixes.contains(&x) { o.fixes.push(x); } }
            for s in f.suspects { if !o.suspects.iter().any(|y| y.file == s.file) { o.suspects.push(s); } }
            for x in f.install { if !o.install.contains(&x) { o.install.push(x); } }
            if o.summary.is_empty() || f.score > o.score { if !f.summary.is_empty() { o.summary = f.summary; } }
            if o.action.is_none() { o.action = f.action; }
        } else { self.out.push(f); }
    }

    fn ram_hint(&self) -> String {
        let cur = self.inp.ram_mb.unwrap_or(4096);
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let phys = (sys.total_memory() / (1024 * 1024)) as i64;
        let mut want = ((cur * 3 / 2) + 1023) / 1024 * 1024;
        want = want.max(4096);
        if phys > 0 { want = want.min((phys * 3 / 4) / 512 * 512).max(cur); }
        format!("{} GB", (want as f64 / 1024.0 * 10.0).round() / 10.0)
    }
    fn where_ram(&self) -> &'static str {
        if self.inp.server { "the server's Settings tab (RAM)" } else { "this instance's Settings (Memory) or Orbit's global Memory setting" }
    }
}

fn library_hints() -> Vec<(&'static str, &'static str, &'static str)> {
    // (package prefix, what it is, Modrinth project)
    vec![("kotlin.", "the Kotlin runtime", "fabric-language-kotlin"), ("net.fabricmc.fabric.", "Fabric API", "fabric-api"),
         ("me.shedaniel.clothconfig2", "Cloth Config", "cloth-config"), ("dev.architectury", "Architectury API", "architectury-api"),
         ("software.bernie.geckolib", "GeckoLib", "geckolib"), ("com.terraformersmc.modmenu", "Mod Menu", "modmenu"),
         ("io.wispforest.owo", "owo-lib", "owo-lib"), ("me.lucko.fabric.api.permissions", "Fabric Permissions API", "fabric-permissions-api"),
         ("net.fabricmc.language.kotlin", "Fabric Language Kotlin", "fabric-language-kotlin")]
}

fn analyse(an: &mut An) {
    let inp = an.inp;
    let ctx = Ctx { mc: inp.mc.clone(), platform: inp.platform.clone(), server: inp.server, loaders: inp.loaders.clone() };
    let _ = &ctx;

    // ---- dependencies named by the loader itself
    let fab = re(r"Mod '([^']+)' \(([^)]+)\) \S+ requires (?:any version|version (.+?)) of (?:mod )?(?:'[^']*' \(([\w.\-]+)\)|'?([\w.\-]+)'?), (?:which is missing|but only the wrong version is present: ([^!\s]+))");
    let forge = re(r"Mod ID: '([^']+)', Requested by: '([^']+)', Expected range: '([^']*)', Actual version: '([^']*)'");
    let neo = re(r"Mod '([^']+)' \(([^)]+)\) requires ([\w.\-]+) (\S+) or above");
    let bukkit_a = re(r"Unknown dependency (\S+?)\.? Please download");
    let bukkit_b = re(r"Unknown/missing dependency plugins: \[([^\]]+)\]");
    let bukkit_c = re(r"UnknownDependencyException: (.+)$");
    let bukkit_d = re(r"Could not load '([^']+)'");
    let mut missing: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut wrong: Vec<(String, String, String, String)> = vec![];       // dep, owner, range, have
    let mut ev: Vec<String> = vec![];
    let note = |ev: &mut Vec<String>, s: String| if ev.len() < 4 && !ev.contains(&s) { ev.push(s); };

    for h in an.grep(&fab) {
        let c = &h.caps;
        let dep = if !c[4].is_empty() { c[4].clone() } else { c[5].clone() }.to_lowercase();
        let owner = if c[1].is_empty() { c[2].clone() } else { c[1].clone() };
        note(&mut ev, an.line(h.idx));
        if c[6].is_empty() { missing.entry(dep).or_default().insert(owner); } else { wrong.push((dep, owner, c[3].clone(), c[6].clone())); }
    }
    for h in an.grep(&forge) {
        let c = &h.caps;
        note(&mut ev, an.line(h.idx));
        if c[4].contains("MISSING") { missing.entry(c[1].to_lowercase()).or_default().insert(c[2].clone()); } else { wrong.push((c[1].to_lowercase(), c[2].clone(), c[3].clone(), c[4].clone())); }
    }
    for h in an.grep(&neo) {
        let c = &h.caps;
        note(&mut ev, an.line(h.idx));
        let absent = an.after(h.idx, 2).iter().any(|l| l.contains("not installed") || l.contains("missing"));
        if absent { missing.entry(c[3].to_lowercase()).or_default().insert(c[1].clone()); } else { wrong.push((c[3].to_lowercase(), c[1].clone(), format!("{} or above", c[4]), String::new())); }
    }
    for pat in [&bukkit_a, &bukkit_b, &bukkit_c] {
        for h in an.grep(pat) {
            let owner = (h.idx.saturating_sub(3)..=h.idx).rev().find_map(|k| bukkit_d.captures(&an.lines[k].text).map(|c| c[1].to_string())).unwrap_or_default();
            let owner = owner.rsplit('/').next().unwrap_or("").trim_end_matches(".jar").to_string();
            note(&mut ev, an.line(h.idx));
            for d in h.caps[1].split(',') { let d = d.trim().trim_matches(['[', ']', '\'']).to_lowercase(); if !d.is_empty() { missing.entry(d).or_default().insert(owner.clone()); } }
        }
    }

    // classify ids that are not mods
    let loader_ids = ["forge", "neoforge", "fabricloader", "fabric-loader", "quilt_loader", "quilt-loader", "fabric_loader"];
    let mut loader_wrong: Vec<(String, String, String, String)> = vec![];
    wrong.retain(|w| { if loader_ids.contains(&w.0.as_str()) { loader_wrong.push(w.clone()); false } else { true } });
    let mc_wrong: Vec<_> = { let (m, rest): (Vec<_>, Vec<_>) = wrong.drain(..).partition(|w| w.0 == "minecraft"); wrong = rest; m };
    let java_wrong: Vec<_> = { let (m, rest): (Vec<_>, Vec<_>) = wrong.drain(..).partition(|w| w.0 == "java"); wrong = rest; m };
    missing.retain(|k, _| !["minecraft", "java", "forge", "neoforge", "fabricloader", "fabric-loader"].contains(&k.as_str()));

    if !missing.is_empty() {
        let list: Vec<String> = missing.iter().map(|(d, by)| { let by: Vec<&String> = by.iter().filter(|b| !b.is_empty()).collect(); if by.is_empty() { d.clone() } else { format!("{d} (needed by {})", by.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")) } }).collect();
        let f = Finding::new("missing-dependency", "A mod or plugin is missing something it needs", 90)
            .sum(format!("The {} can't load: missing {}.", if inp.platform.contains("paper") || ["spigot", "purpur", "folia", "velocity"].contains(&inp.platform.as_str()) { "server" } else { "game" }, list.join("; ")))
            .ev(ev.clone())
            .fix("Click \"Install missing dependencies\" below.")
            .fix("Add anything Orbit can't find by hand (see the mod page's \"Requires\" list).")
            .act("install-deps");
        an.add(f);
    }
    if !wrong.is_empty() {
        let list: Vec<String> = wrong.iter().map(|(d, o, r, h)| format!("{o} wants {d} {}{}", if r.is_empty() { "(another version)".into() } else { r.clone() }, if h.is_empty() { String::new() } else { format!(", found {h}") })).collect();
        an.add(Finding::new("wrong-dependency-version", "A dependency is the wrong version", 82)
            .sum(format!("Wrong dependency version: {}.", list.join("; ")))
            .ev(ev.clone())
            .fix("Update the named dependency (Content tab > Update).")
            .fix("If that fails, switch the dependent mod to a version that matches."));
    }
    if !mc_wrong.is_empty() {
        let list: Vec<String> = mc_wrong.iter().map(|(_, o, r, h)| format!("{o} needs Minecraft {r}{}", if h.is_empty() { String::new() } else { format!(" (this is {h})") })).collect();
        let sus: Vec<Suspect> = mc_wrong.iter().filter_map(|w| an.by_id(&w.1).or_else(|| an.by_jar(&w.1))).collect();
        an.add(Finding::new("wrong-minecraft-version", "A mod is built for a different Minecraft version", 90)
            .sum(format!("{}.", list.join("; "))).ev(ev.clone()).who(sus)
            .fix("Replace the named mod with a build for this Minecraft version (Content tab > version button).")
            .fix("Or set the instance to the version the mods target (Installation tab)."));
    }
    if !java_wrong.is_empty() {
        let need = java_wrong[0].2.clone();
        an.add(Finding::new("java-version", "Java is too old for a mod", 92)
            .sum(format!("{} requires Java {need}.", java_wrong[0].1)).ev(ev.clone())
            .fix("Clear the Java path in Settings so Orbit picks the right Java."));
    }
    if !loader_wrong.is_empty() {
        let list: Vec<String> = loader_wrong.iter().map(|(d, o, r, h)| format!("{o} needs {d} {r}{}", if h.is_empty() { String::new() } else { format!(" (installed: {h})") })).collect();
        an.add(Finding::new("loader-version", "The mod loader is too old", 86)
            .sum(format!("{}.", list.join("; "))).ev(ev.clone())
            .fix(if inp.server { "Update the server loader: run its installer again or create a server on a newer loader." } else { "Raise the loader version in the instance's Installation tab." }));
    }

    // ---- Fabric / Quilt conflicts the loader reported
    let conf = re(r"Mod '([^']+)' \(([^)]+)\).*\b(?:conflicts with|is incompatible with|breaks)\b.*? of (?:mod )?'?([\w.\-]+)'?");
    let hits = an.grep(&conf);
    if !hits.is_empty() {
        let mut sus = vec![];
        for h in &hits { for id in [&h.caps[2], &h.caps[3]] { if let Some(s) = an.by_id(id) { if !sus.iter().any(|x: &Suspect| x.file == s.file) { sus.push(s); } } } }
        an.add(Finding::new("mod-conflict", "Two mods can't run together", 88)
            .sum(format!("The loader reports an incompatibility: {}", an.line(hits[0].idx))).ev(an.snip(&hits, 3)).who(sus)
            .fix("Switch one of the two mods off (Content tab)."));
    }

    // ---- wrong Java
    let uc = re(r"UnsupportedClassVersionError: ([\w./$]+) has been compiled by a more recent version of the Java Runtime \(class file version (\d+)\.\d+\), this version of the Java Runtime only recognizes class file versions up to (\d+)\.\d+");
    let uc2 = re(r"Unsupported major\.minor version (\d+)\.\d+");
    let mut hits = an.grep(&uc);
    if hits.is_empty() { hits = an.grep(&uc2); }
    if let Some(h) = hits.first() {
        let need: i64 = h.caps[2.min(h.caps.len() - 1)].parse::<i64>().unwrap_or(0) - 44;
        let (cls, have) = if h.caps.len() > 3 { (h.caps[1].clone(), h.caps[3].parse::<i64>().unwrap_or(0) - 44) } else { (String::new(), 0) };
        let need = if h.caps.len() > 3 { need } else { h.caps[1].parse::<i64>().unwrap_or(0) - 44 };
        let culprit = if cls.starts_with("net.minecraft") || cls.starts_with("net/minecraft") || cls.is_empty() { "Minecraft itself".to_string() }
            else { let ch: Chain = vec![Entry { idx: h.idx, cls: cls.clone(), msg: String::new(), frames: vec![(cls.replace('/', "."), String::new())] }];
                an.blame(&ch).first().map(|s| s.name.clone()).unwrap_or_else(|| cls.clone()) };
        an.add(Finding::new("java-version", &format!("Java {need} or newer is needed"), 94)
            .sum(format!("{culprit} needs Java {need}{} but the game is running on {}.", if have > 0 { format!(" or newer") } else { String::new() }, if have > 0 { format!("Java {have}") } else { "an older Java".to_string() }))
            .ev(an.snip(&hits, 2))
            .fix(if inp.server { "Clear the Java path in the server's Settings tab, or pick Java ".to_string() + &need.to_string() + " or newer." }
                 else { format!("Clear the Java path in Settings, or choose Java {need}+.") }));
    }

    // ---- JVM could not start
    let jvm = re(r"Could not reserve enough space for (?:object heap|\d+KB object heap)|Invalid (?:maximum|initial) heap size|Unrecognized VM option '([^']+)'|Unrecognized option: (\S+)|Could not create the Java Virtual Machine|Error: Could not find or load main class (\S+)|Couldn't start Java: (.+)|Error occurred during initialization of (?:VM|boot layer)");
    let hits = an.grep(&jvm);
    if !hits.is_empty() {
        let t = an.line(hits[0].idx);
        let mut f = Finding::new("jvm-start-failure", "Java couldn't start", 93).ev(an.snip(&hits, 3));
        if t.contains("reserve enough space") || t.contains("heap size") {
            f = f.sum("Java was given more memory than is available.")
                .fix(format!("Lower the memory in {} to 4 GB.", an.where_ram()));
        } else if t.contains("Unrecognized") {
            f = f.sum(format!("Java doesn't accept this option: {}.", hits.iter().find_map(|h| [1usize, 2].iter().map(|i| h.caps[*i].clone()).find(|s| !s.is_empty())).unwrap_or_default()))
                .fix("Remove the custom JVM arguments in Settings.");
        } else if t.contains("Couldn't start Java") {
            f = f.sum("Orbit couldn't launch Java.").fix("Clear the custom Java path in Settings.");
        } else {
            f = f.sum("Java failed during startup.").fix("Clear the custom Java path and JVM arguments in Settings.");
        }
        an.add(f);
    }

    // ---- out of memory
    let oom = re(r"java\.lang\.OutOfMemoryError: (Java heap space|GC overhead limit exceeded|Metaspace|unable to create (?:new )?native thread|Direct buffer memory|Requested array size exceeds[^\n]*|Compressed class space)");
    let hits = an.grep(&oom);
    if !hits.is_empty() {
        let kind = hits[0].caps[1].clone();
        let mut f = Finding::new("out-of-memory", "Out of memory", 86).ev(an.snip(&hits, 2));
        if kind.starts_with("unable to create") {
            f = f.sum("Java couldn't start another thread: out of memory or thread limit hit.")
                .fix("Lower the game's memory (don't raise it) and close other programs.");
        } else if kind == "Metaspace" || kind.starts_with("Compressed") {
            f = f.sum("Java ran out of class-definition memory (common with big mod lists).")
                .fix("Add \"-XX:MaxMetaspaceSize=512m\" to the JVM arguments in Settings.");
        } else {
            f = f.sum(format!("Out of memory: all {} used ({}).", inp.ram_mb.map(|m| format!("{:.1} GB", m as f64 / 1024.0)).unwrap_or("memory".into()), kind))
                .fix(format!("Raise the memory in {} to about {}.", an.where_ram(), an.ram_hint()))
                .fix("Already high? Remove recently added mods, lower render distance, and stay under 75% of total RAM.");
            let n = an.inv.iter().filter(|m| !m.disabled).count();
            if n > 150 && inp.ram_mb.unwrap_or(4096) < 6144 { f = f.fix(format!("{n} mods installed: give big packs 6 GB or more.")); }
        }
        an.add(f);
    }

    // ---- corrupt jar
    let zip = re(r"(?:ZipException|ZipError|java\.util\.zip\.\w+)[: ]+(.*)|Invalid or corrupt jarfile (\S+)|error in opening zip file|zip END header not found|invalid CEN header");
    let hits = an.grep(&zip);
    let broken_files: Vec<String> = an.rep.broken.iter().map(|b| b.0.clone()).collect();
    if !hits.is_empty() || !broken_files.is_empty() {
        let jar_name = re(r"([^\s/\\'\x22\[\]]+\.jar)");
        let mut files: Vec<String> = broken_files.clone();
        for h in &hits { for k in h.idx.saturating_sub(2)..(h.idx + 3).min(an.lines.len()) { if let Some(c) = jar_name.captures(&an.lines[k].text) { let f = c[1].to_string(); if !files.contains(&f) { files.push(f); } } } }
        let found_by = if hits.is_empty() { "scan" } else if broken_files.is_empty() { "log" } else { "both" };
        let mut f = Finding::new("corrupt-jar", "A mod file is damaged", if hits.is_empty() { 66 } else { 88 })
            .sum(if files.is_empty() { "A .jar can't be read (usually an interrupted download).".to_string() } else { format!("Unreadable .jar: {}.", files.join(", ")) })
            .ev(an.snip(&hits, 2)).fix("Remove the file (Content tab), then add it again from Modrinth.").fix("If it repeats, allow Orbit in your antivirus.");
        f.found_by = found_by;
        for n in &files { if let Some(s) = an.by_jar(n) { f.suspects.push(s); } }
        an.add(f);
    }

    // ---- mixin failures
    let mix = re(r"Mixin apply (?:for mod (\S+) )?failed ([\w.\-]+\.json):(\S+) from mod (\S+)|Mixin transformation of ([\w./$]+) failed|MixinApplyError|InvalidMixinException|MixinTransformerError|Critical injection failure|Mixin (?:apply|prepare) failed|InvalidInjectionException");
    let hits = an.grep(&mix);
    if !hits.is_empty() {
        let cfg = re(r"([\w.\-]+\.mixins?\.json)");
        let mut sus: Vec<Suspect> = vec![];
        let mut target = String::new();
        for h in &hits {
            if !h.caps[4].is_empty() { if let Some(s) = an.by_id(&h.caps[4]) { if !sus.iter().any(|x| x.file == s.file) { sus.push(s); } } }
            if !h.caps[5].is_empty() && target.is_empty() { target = h.caps[5].clone(); }
            for k in h.idx..(h.idx + 4).min(an.lines.len()) { if let Some(c) = cfg.captures(&an.lines[k].text) { if let Some(s) = an.by_entry(&c[1]) { if !sus.iter().any(|x| x.file == s.file) { sus.push(s); } } } }
        }
        if sus.is_empty() { if let Some(ch) = an.primary_chain() { sus = an.blame(ch); } }
        sus.truncate(3);
        let who = sus.iter().map(|s| s.name.clone()).collect::<Vec<_>>().join(", ");
        an.add(Finding::new("mixin-failure", "A mod could not patch the game", 86)
            .sum(format!("{}{}A mixin failed: wrong-version mod or two mods patching the same code.", if who.is_empty() { String::new() } else { format!("{who}: ") }, if target.is_empty() { String::new() } else { format!("while patching {target}. ") }))
            .ev(an.snip(&hits, 3)).who(sus)
            .fix("Switch the named mod off (Content tab) and start again.")
            .fix("Update it, or install the build for this Minecraft version and loader.")
            .fix("Two mods named? Keep only one."));
    }

    // ---- duplicate mods
    let dup = re(r"(?i)duplicate (?:mod|plugin|id)s?\b[^\n]*|Ambiguous plugin name '([^']+)'|Mod '[^']+' \(([\w.\-]+)\)[^\n]*is present multiple times|Found duplicate mod");
    let hits = an.grep(&dup);
    if !hits.is_empty() || !an.rep.duplicates.is_empty() {
        let mut f = Finding::new("duplicate-mod", "The same mod is installed twice", if hits.is_empty() { 62 } else { 87 })
            .ev(an.snip(&hits, 2)).fix("Keep the newest file of each pair; delete or switch off the other.");
        f.found_by = if hits.is_empty() { "scan" } else if an.rep.duplicates.is_empty() { "log" } else { "both" };
        let list: Vec<String> = an.rep.duplicates.iter().map(|(id, fs)| format!("{id}: {}", fs.join(" + "))).collect();
        f.summary = if list.is_empty() { "The loader found two copies of one mod.".into() } else { format!("Two files provide the same mod ({}).", list.join("; ")) };
        for (_, fs) in &an.rep.duplicates { for x in fs { if let Some(s) = an.by_jar(x) { f.suspects.push(s); } } }
        an.add(f);
    }

    // ---- wrong loader / client-only on server / minecraft version (static + log)
    let loader_log = re(r"(?i)missing (?:neoforge\.)?mods\.toml|InvalidModFileException|Invalid mod file|Mod file .* is not a valid mod|does not contain (?:a )?fabric\.mod\.json|Missing metadata");
    let hits = an.grep(&loader_log);
    if !hits.is_empty() || !an.rep.wrong_loader.is_empty() {
        let mut f = Finding::new("wrong-loader", "A mod was made for a different loader", if hits.is_empty() { 60 } else { 89 })
            .ev(an.snip(&hits, 2)).fix("Remove the listed files, or get the build for this loader.")
            .fix("To keep them, create a new instance on their loader.");
        f.found_by = if hits.is_empty() { "scan" } else if an.rep.wrong_loader.is_empty() { "log" } else { "both" };
        let list: Vec<String> = an.rep.wrong_loader.iter().map(|(file, p)| format!("{file} ({})", p.join("/"))).collect();
        f.summary = if list.is_empty() { format!("A file in the mods folder isn't a {} mod.", inp.platform) } else { format!("These are not {} mods: {}.", inp.platform, list.join(", ")) };
        for (file, _) in &an.rep.wrong_loader { if let Some(s) = an.by_jar(file) { f.suspects.push(s); } }
        an.add(f);
    }
    let client = re(r"Attempted to load class (\S+) for invalid dist DEDICATED_SERVER|NoClassDefFoundError: net/minecraft/client/|ClassNotFoundException: net\.minecraft\.client\.|Cannot load class net\.minecraft\.client");
    let hits = if inp.server { an.grep(&client) } else { vec![] };
    if inp.server && (!hits.is_empty() || !an.rep.client_only.is_empty()) {
        let mut sus: Vec<Suspect> = an.rep.client_only.iter().filter_map(|f| an.by_jar(f)).collect();
        if sus.is_empty() { if let Some(ch) = an.primary_chain() { sus = an.blame(ch); } }
        let mut f = Finding::new("client-mod-on-server", "A client-only mod is on the server", if hits.is_empty() { 66 } else { 89 }).ev(an.snip(&hits, 2)).who(sus.clone())
            .fix("Remove client-only mods (minimaps, shaders, UI mods) from the server; players keep theirs.");
        f.summary = if sus.is_empty() { "A mod that only works on the player's side was loaded on the dedicated server.".into() } else { format!("{} only work on the player's side and can't be loaded on a dedicated server.", sus.iter().map(|s| s.name.clone()).collect::<Vec<_>>().join(", ")) };
        f.found_by = if hits.is_empty() { "scan" } else if an.rep.client_only.is_empty() { "log" } else { "both" };
        an.add(f);
    }
    if !an.rep.mc_mismatch.is_empty() {
        let list: Vec<String> = an.rep.mc_mismatch.iter().map(|(f, r)| format!("{f} (for {r})")).collect();
        let mut f = Finding::new("wrong-minecraft-version", "A mod is built for a different Minecraft version", 64).sum(format!("Not built for Minecraft {}: {}.", inp.mc, list.join(", ")))
            .fix("Replace them with builds for this version, or remove them.");
        f.found_by = "scan";
        for (file, _) in &an.rep.mc_mismatch { if let Some(s) = an.by_jar(file) { f.suspects.push(s); } }
        an.add(f);
    }
    if !an.rep.conflicts.is_empty() {
        let mut f = Finding::new("mod-conflict", "Two mods can't run together", 60).sum(an.rep.conflicts.iter().map(|c| c.2.clone()).collect::<Vec<_>>().join("; ") + ".")
            .fix("Keep only one mod of each conflicting pair.");
        f.found_by = "scan";
        for c in &an.rep.conflicts { for x in [&c.0, &c.1] { if let Some(s) = an.by_jar(x) { f.suspects.push(s); } } }
        an.add(f);
    }
    if !an.rep.api_too_new.is_empty() {
        let list: Vec<String> = an.rep.api_too_new.iter().map(|(f, a)| format!("{f} (needs {a})")).collect();
        let mut f = Finding::new("plugin-api-too-new", "A plugin is newer than the server", 70).sum(format!("Built for a newer Minecraft than {}: {}.", inp.mc, list.join(", ")))
            .fix("Update the server, or install an older plugin release.");
        f.found_by = "scan";
        for (file, _) in &an.rep.api_too_new { if let Some(s) = an.by_jar(file) { f.suspects.push(s); } }
        an.add(f);
    }
    let api = re(r"Unsupported API version (\S+)|plugin was (?:made|built) for a newer|This plugin is outdated");
    let hits = an.grep(&api);
    if !hits.is_empty() {
        an.add(Finding::new("plugin-api-too-new", "A plugin doesn't match the server version", 84).sum("A plugin's API version doesn't match this server.")
            .ev(an.snip(&hits, 2)).fix("Update the plugin or the server so versions match."));
    }
    if !an.rep.folia_unsupported.is_empty() {
        let mut f = Finding::new("folia-unsupported", "Plugins that Folia can't load", 72).sum(format!("Folia needs `folia-supported: true`. Missing in: {}.", an.rep.folia_unsupported.join(", ")))
            .fix("Remove them, or switch to Paper/Purpur.");
        f.found_by = "scan";
        for file in &an.rep.folia_unsupported { if let Some(s) = an.by_jar(file) { f.suspects.push(s); } }
        an.add(f);
    }
    if !an.rep.disabled.is_empty() {
        let list: Vec<String> = an.rep.disabled.iter().map(|d| format!("{} ({}) is needed by {}", d.id, d.file, d.needed_by.join(", "))).collect();
        let mut f = Finding::new("disabled-dependency", "A needed mod is switched off", 70).sum(format!("{}.", list.join("; "))).fix("Switch it back on (Content tab).");
        f.found_by = "scan";
        for d in &an.rep.disabled { if let Some(s) = an.by_jar(&d.file) { f.suspects.push(s); } }
        an.add(f);
    }
    // static scan result for missing dependencies even when no log line named them
    if !an.rep.missing.is_empty() {
        let list: Vec<String> = an.rep.missing.iter().map(|m| format!("{} (needed by {})", m.id, m.needed_by.join(", "))).collect();
        let mut f = Finding::new("missing-dependency", "A mod or plugin is missing something it needs", 66)
            .sum(format!("Missing dependencies: {}.", list.join("; ")))
            .fix("Click \"Install missing dependencies\" below.").act("install-deps");
        f.found_by = "scan";
        an.add(f);
    }

    // ---- mod loading failure named by the loader
    let load = re(r"Exception caught from container (\S+)|Failed to load mod (\S+)|([\w\-. ]+) \(([\w\-]+)\) has failed to load correctly|Mod loading has failed|ModLoadingException|LoadingFailedException");
    let hits = an.grep(&load);
    if !hits.is_empty() {
        let mut sus = vec![];
        for h in &hits { for k in [1usize, 2, 4] { if !h.caps[k].is_empty() { if let Some(s) = an.by_id(h.caps[k].trim_matches(|c| c == ',' || c == '.')) { if !sus.iter().any(|x: &Suspect| x.file == s.file) { sus.push(s); } } } } }
        let who = sus.iter().map(|s| s.name.clone()).collect::<Vec<_>>().join(", ");
        an.add(Finding::new("mod-load-failure", "A mod failed while loading", if sus.is_empty() { 55 } else { 72 })
            .sum(if who.is_empty() { "Mod loading stopped. See the lines below.".to_string() } else { format!("{who} failed to load.") })
            .ev(an.snip(&hits, 3)).who(sus).fix("Switch the named mod off, then update it or find a build for this version and loader."));
    }

    // ---- missing classes / binary incompatibility
    let ncd = re(r"(NoSuchMethodError|NoSuchFieldError|NoClassDefFoundError|ClassNotFoundException|AbstractMethodError|IncompatibleClassChangeError): ?([^\n]*)");
    let hits = an.grep(&ncd);
    if !hits.is_empty() {
        let h = &hits[0];
        let missing_class = h.caps[2].split(|c: char| c == ' ' || c == '(' || c == ';').next().unwrap_or("").replace('/', ".");
        let lib = library_hints().into_iter().find(|(p, _, _)| missing_class.starts_with(p));
        let mut sus = an.primary_chain().map(|c| an.blame(c)).unwrap_or_default();
        sus.truncate(2);
        let mut f = Finding::new("missing-class", "A mod needs code that isn't there", 72).ev(an.snip(&hits, 3)).who(sus.clone());
        if let Some((_, what, slug)) = lib {
            f = f.sum(format!("A class from {what} ({missing_class}) couldn't be found, so something that depends on {what} is running without it.")).fix(format!("Install {what} (Modrinth: \"{slug}\") for this version and loader."));
            f.install.push(slug.to_string()); f.score = 80;
        } else {
            f = f.sum(format!("{}{} can't be found in the form the calling mod expects. A mod was built for a different version of Minecraft, of its loader or of another mod.",
                if sus.is_empty() { String::new() } else { format!("{}: ", sus.iter().map(|s| s.name.clone()).collect::<Vec<_>>().join(", ")) }, if missing_class.is_empty() { h.caps[1].clone() } else { format!("`{missing_class}`") }))
                .fix("Update all mods together to one Minecraft version.")
                .fix("Switch the named mod off to confirm.");
        }
        an.add(f);
    }

    // ---- client-side / graphics
    let gl = re(r"GLFW error (\d+)[^\n]*|The driver does not appear to support OpenGL|Pixel format not accelerated|Couldn't set pixel format|Failed to initialize GLFW|OpenGL is not supported|GLX: Failed to create context|WGL: [^\n]*|Failed to create window|org\.lwjgl\.LWJGLException|NoSuchMethodError: org\.lwjgl|GL_INVALID_[A-Z_]+|OpenGL (?:error|Error)|Rendering overlay|Unable to initialize OpenGL");
    let hits = if !inp.server { an.grep(&gl) } else { vec![] };
    if !hits.is_empty() {
        an.add(Finding::new("graphics-driver", "A graphics problem stopped the game", 78).ev(an.snip(&hits, 3))
            .sum("OpenGL failed: outdated driver, or a shader/rendering mod the GPU can't run.")
            .fix("Update the graphics driver from NVIDIA, AMD or Intel directly.")
            .fix("On laptops, run Java on the dedicated GPU.")
            .fix("Test without shader packs and rendering mods (Sodium, Iris, OptiFine)."));
    }

    // ---- JVM native crash
    let jv: Vec<String> = an.srcs.iter().filter(|s| s.kind == "jvm-crash").map(|s| s.text.clone()).collect();
    for text in jv {
        let acc = re(r"(?m)^#\s+(EXCEPTION_ACCESS_VIOLATION|SIGSEGV|SIGBUS|SIGILL|SIGABRT|EXCEPTION_STACK_OVERFLOW|EXCEPTION_ILLEGAL_INSTRUCTION)[^\n]*");
        let lib = re(r"(?m)^#\s+[CV]\s+\[([^\]+]+?)(?:\+0x[0-9a-fA-F]+)?\]");
        let oomn = re(r"(?m)^#\s+(There is insufficient memory for the Java Runtime Environment to continue|Out of Memory Error[^\n]*|Native memory allocation \((?:mmap|malloc)\) failed[^\n]*)");
        let jre = re(r"(?m)^#\s+(?:JRE version|Java VM): ([^\n]+)");
        let sig = acc.captures(&text).map(|c| c[0].trim_start_matches('#').trim().to_string());
        let module = lib.captures(&text).map(|c| c[1].to_string());
        let ver = jre.captures(&text).map(|c| c[1].to_string()).unwrap_or_default();
        let mut ev: Vec<String> = sig.iter().cloned().collect();
        if let Some(m) = &module { ev.push(format!("Problematic frame: {m}")); }
        if !ver.is_empty() { ev.push(format!("Java: {ver}")); }
        if let Some(c) = oomn.captures(&text) {
            let mut e = vec![c[0].trim_start_matches('#').trim().to_string()];
            e.extend(ev.clone());
            an.add(Finding::new("system-out-of-memory", "The computer ran out of memory", 90).ev(e)
                .sum("The game's memory setting is larger than the free RAM.")
                .fix(format!("Lower the memory in {} to about half of total RAM.", an.where_ram()))
                .fix("Close other programs; on Windows keep the page file enabled."));
        } else if let Some(m) = module {
            let ml = m.to_lowercase();
            let gpu = re(r"^(?:nvoglv|nvwgf2um|nvd3dum|nvlddmkm|atio6axx|atioglxx|atig6pxx|amdxx|aticfx|ig\d+icd|igd\w*|igxel\w*|ig9\w*|iris_dri|radeonsi|nouveau|libglx|libegl|libgl|opengl32|libnvidia|swrast|amdvlk|libdrm|vulkan|ig\w+icd)");
            let overlay = re(r"(?i)gameoverlayrenderer|rtss|obs-?(?:graphics|capture)|discord|nvspcap|nvtt|overwolf|medal|msi\w*afterburner|dxtory|bandicam|fraps|reshade|avghook|aswhook");
            if gpu.is_match(&ml) {
                an.add(Finding::new("graphics-driver", "The graphics driver crashed", 90).ev(ev)
                    .sum(format!("Java crashed inside the graphics driver ({m})."))
                    .fix("Update or reinstall the graphics driver.")
                    .fix("Test without shaders and rendering mods; lower render distance.")
                    .fix("On laptops, run Java on the dedicated GPU."));
            } else if overlay.is_match(&ml) {
                an.add(Finding::new("overlay-software", "Overlay or capture software crashed the game", 86).ev(ev)
                    .sum(format!("The crash was inside {m}, an overlay or recording tool."))
                    .fix("Close that program (overlay, recorder, FPS counter) or exclude Java in it."));
            } else if ml.starts_with("jvm") || ml.starts_with("libjvm") {
                an.add(Finding::new("jvm-crash", "The Java runtime itself crashed", 70).ev(ev)
                    .sum("Java itself crashed: a Java bug, bad RAM, or a mod corrupting memory.")
                    .fix("Clear the Java path in Settings so Orbit uses its own Java.")
                    .fix("Remove recently added mods, especially ones with native code.")
                    .fix("If crashes are random, test the RAM."));
            } else {
                an.add(Finding::new("native-crash", "Native code crashed the game", 72).ev(ev)
                    .sum(format!("The crash was inside native library {m}."))
                    .fix("Search that file name to find its owner, then update or remove it."));
            }
        } else if sig.is_some() {
            an.add(Finding::new("jvm-crash", "The Java runtime crashed", 62).ev(ev).sum("Java crashed (see the hs_err file).").fix("Update Java and the graphics driver, then remove recent mods."));
        }
    }

    // ---- server specific
    if inp.server {
        let bind = re(r"FAILED TO BIND TO PORT|Address already in use|java\.net\.BindException|Perhaps a server is already running on that port");
        let hits = an.grep(&bind);
        if !hits.is_empty() {
            let port = inp.port.map(|p| format!(" {p}")).unwrap_or_default();
            an.add(Finding::new("port-in-use", &format!("Port{port} is already in use"), 96).ev(an.snip(&hits, 2))
                .sum(format!("Another program (often another copy of this server) uses port{port}."))
                .fix("Stop the other server on that port (check the Servers page), or close the program using it.")
                .fix("Or pick another port in the server's Settings tab."));
        }
        let eula = re(r"You need to agree to the EULA|Failed to load eula\.txt|eula\.txt");
        let hits = an.grep(&eula);
        if hits.iter().any(|h| an.lines[h.idx].text.contains("agree") || an.lines[h.idx].text.contains("Failed")) {
            an.add(Finding::new("eula", "The Minecraft EULA hasn't been accepted", 97).ev(an.snip(&hits, 2))
                .sum("The server won't start until eula=true is set.")
                .fix("Click \"Accept the EULA\" below, then start again.").act("accept-eula"));
        }
        let wd = re(r"A single server tick took ([\d.]+) seconds|Considering it to be crashed, server will forcibly shutdown|ServerHangWatchdog|Server Watchdog");
        let hits = an.grep(&wd);
        if !hits.is_empty() {
            let secs = hits.iter().find_map(|h| h.caps[1].parse::<f64>().ok()).map(|s| format!(" (one tick took {s:.0} seconds)")).unwrap_or_default();
            let sus = an.primary_chain().map(|c| an.blame(c)).unwrap_or_default();
            // the main thread's stack in the watchdog dump is the stack to blame, which `primary_chain` may have missed
            an.add(Finding::new("watchdog", "The server froze and was shut down", 84).ev(an.snip(&hits, 3)).who(sus)
                .sum(format!("The server froze{secs} and the watchdog stopped it."))
                .fix("Find the plugin or mod in the \"Server thread\" stack in the log; update or remove it.")
                .fix(format!("Check memory (now {}); lower view and simulation distance.", inp.ram_mb.map(|m| format!("{:.1} GB", m as f64 / 1024.0)).unwrap_or_else(|| "the default".into())))
                .fix("Check for big redstone or mob farms, or bad chunks, just before the freeze."));
        }
    }

    // ---- world, datapacks, configs, disk
    let newer = re(r"(?i)(?:world|level|chunk)[^\n]*(?:saved|played|stored)[^\n]*newer version|(?:newer|later) version of Minecraft|downgrad");
    let hits = an.grep(&newer);
    if !hits.is_empty() {
        an.add(Finding::new("world-newer", "The world was saved by a newer Minecraft", 85).ev(an.snip(&hits, 2))
            .sum("This world was saved by a newer Minecraft.")
            .fix("Use the newer version, or restore a backup from before the upgrade."));
    }
    let wc = re(r"Exception reading [^\n]*level\.dat|Failed to read level\.dat|EOFException: Unexpected end of ZLIB input stream|Failed to (?:read|load) (?:chunk|region)[^\n]*|Chunk file at [^\n]* is in the wrong location|Unable to read or access file [^\n]*region|Failed to load level|Missing region file|Corrupted? (?:chunk|region)");
    let hits = an.grep(&wc);
    if !hits.is_empty() {
        an.add(Finding::new("world-corrupt", "World data is damaged", 80).ev(an.snip(&hits, 3))
            .sum("Part of the world (level.dat or a region file) is unreadable.")
            .fix("Restore a world backup if you have one.")
            .fix("level.dat: rename `level.dat_old` to `level.dat`.")
            .fix("One bad region file: copy it elsewhere, then delete it from `region/` to regenerate that area."));
    }
    let reg = re(r"(?i)Missing registry entries|Registry entries missing|Unknown registry key|Failed to load registries due to above errors|Unbound values in registry");
    let hits = an.grep(&reg);
    if !hits.is_empty() {
        an.add(Finding::new("registry-missing", "Mods used by this world are missing", 78).ev(an.snip(&hits, 3))
            .sum("The world uses blocks, items or biomes from mods that are gone or changed.")
            .fix("Install the original mods in the same versions.")
            .fix("If you removed a mod on purpose, restore a backup from before that."));
    }
    let dp = re(r"Failed to load datapacks, can't proceed with server load|Couldn't parse (?:data file|element) ([\w:/.\-]+)|Parsing error loading (?:recipe|data) ([\w:/.\-]+)|Couldn't load tag ([\w:/.\-]+)|Failed to parse (?:loot table|recipe|advancement) ([\w:/.\-]+)");
    let hits = an.grep(&dp);
    if !hits.is_empty() {
        an.add(Finding::new("datapack-error", "A data pack has an error", 70).ev(an.snip(&hits, 3))
            .sum("A data pack couldn't be read.")
            .fix("Remove the data pack named below (world's `datapacks` folder), or update it."));
    }
    let cfgp = re(r"(?i)(?:Failed to (?:load|read|parse)|Error (?:loading|reading|parsing)|Unable to (?:load|read|parse)|Could not (?:load|parse|read)|Invalid)[^\n]*config[^\n]*?([\w\-./\\]+\.(?:toml|json5?|cfg|properties|ya?ml|conf))|ParsingException|Failed loading config file ([\w\-./\\]+)|MalformedJsonException");
    let hits = an.grep(&cfgp);
    if !hits.is_empty() {
        let file = hits.iter().find_map(|h| [1usize, 2].iter().map(|i| h.caps[*i].clone()).find(|s| !s.is_empty())).unwrap_or_default();
        an.add(Finding::new("config-error", "A configuration file is broken", 68).ev(an.snip(&hits, 3))
            .sum(if file.is_empty() { "A config file has a syntax error.".into() } else { format!("{file} has a syntax error.") })
            .fix(if file.is_empty() { "Fix the file named below (Files tab), or delete it to get a fresh default.".to_string() }
                 else { format!("Open `config/{file}` in the Files tab, fix the line the error points at, or delete the file so the mod writes a fresh default.") }));
    }
    let disk = re(r"No space left on device|There is not enough space on the disk|Read-only file system|AccessDeniedException[^\n]*|Access is denied|Permission denied|FileSystemException[^\n]*(?:being used by another process|Permission)");
    let hits = an.grep(&disk);
    if !hits.is_empty() {
        let t = an.line(hits[0].idx);
        let space = t.contains("space");
        an.add(Finding::new(if space { "disk-full" } else { "file-access" }, if space { "The disk is full" } else { "Files can't be accessed" }, 80).ev(an.snip(&hits, 3))
            .sum(if space { "The drive has no free space." } else { "A file is locked by another program or the folder is protected." }.to_string())
            .fix(if space { "Free disk space: old worlds, logs and `crash-reports` first." } else { "Close other Minecraft copies, keep the folder out of OneDrive, and allow Java in your antivirus." }));
    }
    let sess = re(r"Invalid session|Failed to verify username|Authentication servers are down|Bad login|Unverified username");
    let hits = if !inp.server { an.grep(&sess) } else { vec![] };
    if !hits.is_empty() {
        an.add(Finding::new("session", "Login to Minecraft's servers failed", 58).ev(an.snip(&hits, 2))
            .sum("Account verification failed.")
            .fix("Sign in again in Settings > Accounts, then relaunch."));
    }

    // ---- exit codes Orbit recorded
    let code = re(r"Minecraft closed with code (-?\d+)|Process exited \(code (-?\d+)\)");
    for h in an.grep(&code) {
        let n: i64 = [1usize, 2].iter().map(|i| h.caps[*i].clone()).find(|s| !s.is_empty()).and_then(|s| s.parse().ok()).unwrap_or(0);
        let (title, why) = match n {
            -805306369 => ("The game was closed from outside", "Exit code 0xCFFFFFFF means the process was ended by something else (Task Manager, antivirus, or the system running out of memory)."),
            -1073741819 | 3221225477 => ("Java crashed in native code", "Exit code 0xC0000005 is a memory access violation, usually the graphics driver or overlay software."),
            -1073740791 | 3221226505 => ("Java crashed in native code", "Exit code 0xC0000409 means a native library crashed: typically a graphics driver, overlay or antivirus hook."),
            -1073740940 | 3221226356 => ("Java crashed (heap corruption)", "Exit code 0xC0000374 is memory corruption inside a native library."),
            137 => ("The system killed the process", "Exit code 137 means the process was killed, most often because the computer ran out of memory."),
            _ => continue,
        };
        an.add(Finding::new("exit-code", title, 52).sum(why).ev(vec![an.line(h.idx)])
            .fix("Update the graphics driver, close overlay software, and lower the memory setting."));
        break;
    }

    // ---- fall back to the stack trace itself
    let strong = an.out.iter().any(|f| f.score >= 70 && f.found_by != "scan");
    if !strong {
        if let Some(ch) = an.primary_chain().cloned() {
            let root = ch.last().unwrap().clone();
            let sus = an.blame(&ch);
            let short = root.cls.rsplit('.').next().unwrap_or(&root.cls).to_string();
            let msg = if root.msg.is_empty() { String::new() } else { format!(": {}", root.msg.chars().take(160).collect::<String>()) };
            let mut ev = vec![format!("{}{msg}", root.cls)];
            ev.extend(root.frames.iter().take(3).map(|(c, _)| format!("  at {c}")));
            if let Some(first) = ch.first().filter(|f| f.idx != root.idx) { ev.insert(0, format!("{}{}", first.cls, if first.msg.is_empty() { String::new() } else { format!(": {}", first.msg.chars().take(120).collect::<String>()) })); }
            if !sus.is_empty() {
                let who = sus.iter().map(|s| s.name.clone()).collect::<Vec<_>>().join(", ");
                an.add(Finding::new("mod-exception", &format!("{} crashed with {short}", sus[0].name), 58).ev(ev).who(sus)
                    .sum(format!("{short} was thrown by {who}'s code (best lead, not certain)."))
                    .fix("Switch the named mod off (Content tab) and run again; if it works, update it.")
                    .fix("Several mods listed? Test them one at a time, first one first."));
            } else {
                an.add(Finding::new("generic-exception", &format!("Unexpected error: {short}"), 36).ev(ev)
                    .sum(format!("Stopped with {short}{msg}. No installed mod is in the stack trace."))
                    .fix("Search the exception name with your Minecraft version and loader.")
                    .fix("Undo your most recent mod or setting change, then try again."));
            }
        }
    }

    // ---- the crash report's own "Suspected Mods" list sharpens the suspect list
    if let Some(s) = an.srcs.iter().find(|s| s.kind == "crash-report") {
        let sm = re(r"(?m)^Suspected Mods?:\s*\n?((?:[ \t]+.+\n?)+)|^Suspected Mods?: (.+)$");
        let one = re(r"^\s*(.+?) \(([\w\-.]+)\)(?:, Version: .*)?$");
        let mut named: Vec<Suspect> = vec![];
        if let Some(c) = sm.captures(&s.text) {
            let block = c.get(1).or(c.get(2)).map(|m| m.as_str().to_string()).unwrap_or_default();
            for l in block.lines().take(6) {
                let l = l.trim();
                if l.is_empty() || l == "None" || l.starts_with("Issue tracker") || l.starts_with("Mod List") { continue; }
                if let Some(m) = one.captures(l) { if let Some(x) = an.by_id(&m[2]) { if !named.iter().any(|n| n.file == x.file) { named.push(x); } } }
            }
        }
        if !named.is_empty() {
            let top = an.out.iter().position(|f| f.score == an.out.iter().map(|g| g.score).max().unwrap_or(0));
            if let Some(i) = top {
                if matches!(an.out[i].id, "mod-exception" | "generic-exception" | "mixin-failure" | "missing-class" | "mod-load-failure") {
                    for s in named { if !an.out[i].suspects.iter().any(|x| x.file == s.file) { an.out[i].suspects.insert(0, s); } }
                    an.out[i].suspects.truncate(3);
                    an.out[i].score += 4;
                    let ev = "The crash report itself lists these as suspected mods.".to_string();
                    if !an.out[i].evidence.contains(&ev) { an.out[i].evidence.push(ev); }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- output

/// Extra, concrete follow-up steps per finding id. They are appended after the finding's own fixes, so the
/// first fix stays the most specific one (it may carry a number such as the memory to use).
fn playbook(id: &str, server: bool) -> &'static [&'static str] {
    match id {
        "missing-dependency" => &["Wait for the downloads to finish, then start again.", "Still failing? Run Diagnose again: it names whatever is left.", "Match each manual download to the Minecraft version and loader shown on the Content tab."],
        "wrong-dependency-version" => &["Open the Content tab and note the version of the named dependency.", "Click its update button, or pick a version inside the required range.", "Start again; if another mod now complains, update that one too."],
        "wrong-minecraft-version" => &["Open the Content tab and find the named files.", "Click each mod's version button and choose a build for your Minecraft version.", "No matching build exists? Remove the mod or move to a version it supports."],
        "java-version" => if server {
            &["Open the server's Settings tab and clear the Java path.", "Start the server; Orbit downloads the Java it needs.", "Still failing? Run `java -version` in a terminal and compare it to the version named above."]
        } else {
            &["Open Settings > Launch and clear the Java path.", "Launch again; Orbit fetches the Java this version needs.", "Still failing? Check the instance's own Settings for a Java path override."]
        },
        "loader-version" => &["Check which loader version the mod asks for (its Modrinth page, Versions tab).", "Raise the loader to that version or newer.", "Update your mods afterwards so they match the new loader."],
        "mod-conflict" => &["Switch off one of the two mods (Content tab toggle) and start again.", "Still failing? Swap them: switch the other one off instead.", "Pick the one you need more and look for an alternative to the other."],
        "jvm-start-failure" => &["Open Settings and clear the custom JVM arguments.", "Clear the custom Java path too, then start again.", "Add your arguments back one at a time to find the one that breaks it."],
        "out-of-memory" => &["Change the memory, then restart fully (a running game keeps its old value).", "Don't exceed about 75% of total RAM; Windows and the browser need the rest.", "Still failing? Remove the newest mods and lower render distance to 8 or less."],
        "corrupt-jar" => &["Remove the named file on the Content tab.", "Download it again from Modrinth.", "Repeats? Allow Orbit's folder in your antivirus and check the disk for errors."],
        "mixin-failure" => &["Find the named mod on the Content tab and switch it off.", "Start again. If the crash is gone, that mod is the cause.", "Update the mod, or pick a build for this Minecraft version and loader.", "Two mods named? Keep the one you need more."],
        "duplicate-mod" => &["Sort the Content tab by name and find the pair.", "Switch off the older file, or remove it.", "Start again to confirm."],
        "wrong-loader" => &["Open the Installation tab and note the loader.", "Remove each listed mod, or get its build for that loader.", "Want to keep them? Create a new instance on the loader they target."],
        "client-mod-on-server" => &["Open the server's Content tab and switch off each named mod.", "Start the server again.", "Players keep these mods on their own game."],
        "plugin-api-too-new" => &["Check the plugin's Modrinth page for supported Minecraft versions.", "Install the release that matches your server, or update the server to that version.", "Restart the server and watch the console for the load message."],
        "folia-unsupported" => &["Open the plugin's page and check whether it supports Folia.", "Remove the plugins that don't.", "Or create a Paper server if you need them."],
        "disabled-dependency" => &["Open the Content tab and switch the named mod back on.", "Start again."],
        "mod-load-failure" => &["Read the evidence lines: they name the mod and the reason.", "Switch that mod off, then start again.", "Update it or find a build for this version and loader."],
        "missing-class" => &["Install the named library (or the mod that provides the missing class).", "Update every mod to the same Minecraft version.", "Switch the suspect mod off to confirm, then start again."],
        "graphics-driver" => &["Download the newest driver from NVIDIA, AMD or Intel and install it.", "Restart the computer after installing.", "Still crashing? Remove shader packs, then Sodium/Iris/OptiFine, and test.", "On a laptop, set Java (javaw.exe) to \"High performance\" in Windows Graphics settings."],
        "system-out-of-memory" => &["Close the browser and other large programs.", "Set the game's memory to about half of total RAM, then restart.", "On Windows, check that the page file is on (System > Advanced > Performance)."],
        "overlay-software" => &["Close the program named above (Discord overlay, recorder, FPS counter).", "Start again. Works? Add Java to that program's exclusion list.", "Keep the overlay off for Minecraft."],
        "jvm-crash" => &["Clear the Java path so Orbit uses its own Java.", "Remove mods added just before the first crash.", "Random crashes? Run a RAM test (Windows Memory Diagnostic)."],
        "native-crash" => &["Search the library name from the evidence to find its owner.", "Update or remove that program or mod.", "Start again to confirm."],
        "port-in-use" => &["Open the Servers page and stop any other server on this port.", "Not that? Close the program using the port, or change the port in Settings.", "Start the server again."],
        "eula" => &["Start the server again.", "Still stopping? Open eula.txt (Files tab) and make sure it says eula=true."],
        "watchdog" => &["Open the server's Console and find the \"Server thread\" lines before the freeze.", "Update or remove the plugin or mod named there.", "Lower view-distance and simulation-distance in server.properties.", "Restart and watch for lag spikes after the next start."],
        "world-newer" => &["Check the version the world was last played on (shown in the evidence).", "Set the instance or server to that version, or newer.", "Or restore a backup made before the upgrade."],
        "world-corrupt" => &["Stop the game or server first.", "Copy the world folder somewhere safe before changing anything.", "Restore a backup, or try the file-level fixes above.", "Start and check the log for new errors."],
        "registry-missing" => &["List the mod namespaces in the evidence lines.", "Install those mods in the versions the world was made with.", "Back up the world before loading it with any change."],
        "datapack-error" => &["Open the world's `datapacks` folder (Files tab).", "Remove the data pack named in the evidence.", "Start again, then re-add it in a version made for this Minecraft."],
        "config-error" => &["Open the file named in the evidence (Files tab > config).", "Fix the syntax error, or delete the file so a default is written.", "Start again."],
        "disk-full" => &["Free at least 2 GB on the drive holding the game or server.", "Delete old crash reports, logs and unused worlds.", "Start again."],
        "file-access" => &["Close every other Minecraft or Java process (Task Manager).", "Move the folder out of OneDrive or other synced folders.", "Allow Java in your antivirus, then start again."],
        "session" => &["Open Settings > Accounts and sign out, then sign in again.", "Check your internet connection and the Minecraft services status.", "Launch again."],
        "exit-code" => &["Update the graphics driver.", "Close overlay and recording software.", "Lower the memory setting and launch again."],
        "mod-exception" => &["Switch the named mod off (Content tab) and start again.", "Gone? Update the mod, or report it with the crash report (Copy report).", "Several mods listed? Test them one at a time."],
        "generic-exception" => &["Switch off the last mod you added and start again.", "Copy the report and search the exception name with your version and loader.", "Update the loader and all mods together."],
        _ => &[],
    }
}

fn level(score: i32) -> &'static str { if score >= 80 { "high" } else if score >= 60 { "medium" } else { "low" } }

fn finding_json(f: &Finding) -> Value {
    json!({"id": f.id, "title": f.title, "summary": f.summary, "confidence": f.score.min(99), "level": level(f.score), "found_by": f.found_by,
           "evidence": f.evidence, "fixes": f.fixes, "action": f.action, "install": f.install,
           "suspects": f.suspects.iter().map(|s| json!({"id": s.id, "name": s.name, "file": s.file})).collect::<Vec<_>>()})
}

fn report_text(inp: &Input, status: &str, fs: &[Finding], srcs: &[Source]) -> String {
    let mut t = format!("Orbit crash diagnosis: {} ({}{}{})\n", inp.name, inp.kind, if inp.platform.is_empty() { String::new() } else { format!(", {}", inp.platform) }, if inp.mc.is_empty() { String::new() } else { format!(" {}", inp.mc) });
    if fs.is_empty() {
        t.push_str(if status == "no-logs" { "\nNo crash report or log yet. Run again after it fails.\n" } else { "\nNo known problem found.\n" });
    }
    for (i, f) in fs.iter().enumerate() {
        t.push_str(&format!("\n{}{} [{} confidence, {}%]\n{}\n", if i == 0 && status == "crash" { "LIKELY CAUSE: " } else if i == 0 { "FOUND: " } else { "ALSO: " }, f.title, level(f.score), f.score.min(99), f.summary));
        if !f.suspects.is_empty() { t.push_str(&format!("Suspects: {}\n", f.suspects.iter().map(|s| format!("{} ({})", s.name, s.file)).collect::<Vec<_>>().join(", "))); }
        for e in &f.evidence { t.push_str(&format!("  > {e}\n")); }
        for (k, x) in f.fixes.iter().enumerate() { t.push_str(&format!("  {}. {x}\n", k + 1)); }
    }
    if !srcs.is_empty() { t.push_str(&format!("\nRead: {}\n", srcs.iter().map(|s| s.name.clone()).collect::<Vec<_>>().join(", "))); }
    t
}

/// Analyse `inp` and return the diagnosis as JSON (see the UI's "Diagnose" panel).
pub fn run(inp: &Input) -> Value {
    let srcs = gather(inp);
    let inv = inp.mods_dir.as_deref().map(deps::scan_dir).unwrap_or_default();
    let ctx = Ctx { mc: inp.mc.clone(), platform: inp.platform.clone(), server: inp.server, loaders: inp.loaders.clone() };
    let rep = deps::analyze(&inv, &ctx);
    let mut lines = vec![];
    for (i, s) in srcs.iter().enumerate() { for l in s.text.lines() { lines.push(Line { src: i, text: l.to_string() }); } }
    let ch = chains(&lines);
    let mut an = An { inp, srcs, lines, inv, rep, out: vec![], chains: ch };
    analyse(&mut an);

    let signals = re(r"Minecraft Crash Report|This crash report has been saved to|Game crashed!|Mod loading has failed|Incompatible mod set|Failed to start the minecraft server|Encountered an unexpected exception|Exception in server tick loop|Process exited \(code [^0)]|Minecraft closed with code|has crashed|FATAL");
    let crashed = an.srcs.iter().any(|s| matches!(s.kind, "crash-report" | "jvm-crash")) || !an.grep(&signals).is_empty() || an.out.iter().any(|f| f.found_by != "scan" && f.score >= 70);
    an.out.sort_by(|a, b| b.score.cmp(&a.score).then(a.id.cmp(b.id)));
    for f in an.out.iter_mut() { for st in playbook(f.id, inp.server) { f.fixes.push((*st).to_string()); } }
    let status = if an.srcs.is_empty() && an.out.is_empty() { "no-logs" } else if crashed { "crash" } else if an.out.is_empty() { "clean" } else { "problems" };
    let summary = match (status, an.out.first()) {
        ("crash", Some(f)) | ("problems", Some(f)) => format!("{}: {}", f.title, f.summary),
        ("crash", None) => "A crash was recorded, but no known cause matched. See the files below.".into(),
        ("no-logs", _) => "No crash report or log found yet.".into(),
        _ => "No problems found.".into(),
    };
    let text = report_text(inp, status, &an.out, &an.srcs);
    json!({
        "status": status, "summary": summary, "primary": an.out.first().map(finding_json).unwrap_or(Value::Null),
        "findings": an.out.iter().map(finding_json).collect::<Vec<_>>(),
        "scan": deps::report_json(&an.rep),
        "sources": an.srcs.iter().map(|s| json!({"kind": s.kind, "name": s.name, "modified": s.mtime, "lines": s.text.lines().count()})).collect::<Vec<_>>(),
        "context": {"kind": inp.kind, "name": inp.name, "platform": inp.platform, "mc": inp.mc, "ram_mb": inp.ram_mb, "mods": an.inv.iter().filter(|m| !m.disabled).count()},
        "text": text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(tag: &str, files: &[(&str, &str)]) -> Input {
        let root = std::env::temp_dir().join(format!("orbit-diag-{tag}-{:08x}", rand::random::<u32>()));
        for (p, c) in files { let f = root.join(p); std::fs::create_dir_all(f.parent().unwrap()).unwrap(); std::fs::write(f, c).unwrap(); }
        Input { kind: "instance", name: tag.into(), root: root.clone(), mc: "1.20.1".into(), platform: "fabric".into(), server: false, ram_mb: Some(2048), port: None,
                mods_dir: Some(root.join("mods")), loaders: vec!["fabric".into()], console: vec![] }
    }
    fn top(v: &Value) -> String { v["primary"]["id"].as_str().unwrap_or("").to_string() }

    #[test]
    fn fabric_missing_dependency() {
        let i = setup("fabdep", &[("logs/latest.log", "[12:00:01] [main/ERROR] (FabricLoader) Incompatible mod set!\nnet.fabricmc.loader.impl.FormattedException: Some of your mods are incompatible with the game or each other!\nA potential solution has been determined, this may resolve your problem:\n\t - Install fabric-api, version 0.90.0 or later.\nMore details:\n\t - Mod 'Cool Mod' (coolmod) 1.0 requires version 0.90.0 or later of fabric-api, which is missing!\n\tat net.fabricmc.loader.impl.FabricLoaderImpl.load(FabricLoaderImpl.java:1)\n")]);
        let v = run(&i);
        assert_eq!(top(&v), "missing-dependency", "{}", v["text"]);
        assert_eq!(v["primary"]["action"], "install-deps");
        assert!(v["primary"]["summary"].as_str().unwrap().contains("fabric-api"));
    }

    #[test]
    fn forge_missing_dependency_and_mc() {
        let mut i = setup("forge", &[("logs/latest.log", "Missing or unsupported mandatory dependencies:\n\tMod ID: 'curios', Requested by: 'mymod', Expected range: '[5.1,)', Actual version: '[MISSING]'\n\tMod ID: 'minecraft', Requested by: 'oldmod', Expected range: '[1.19.2]', Actual version: '1.20.1'\n")]);
        i.platform = "forge".into();
        let v = run(&i);
        let ids: Vec<&str> = v["findings"].as_array().unwrap().iter().map(|f| f["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"missing-dependency") && ids.contains(&"wrong-minecraft-version"), "{ids:?}");
    }

    #[test]
    fn out_of_memory_and_java() {
        let v = run(&setup("oom", &[("logs/latest.log", "[main/ERROR]: Exception in thread \"Render thread\" java.lang.OutOfMemoryError: Java heap space\n\tat a.b.C.d(C.java:1)\n")]));
        assert_eq!(top(&v), "out-of-memory");
        assert!(v["primary"]["fixes"][0].as_str().unwrap().contains("GB"));
        let v = run(&setup("java", &[("logs/quarry-launch.log", "Exception in thread \"main\" java.lang.UnsupportedClassVersionError: net/minecraft/client/main/Main has been compiled by a more recent version of the Java Runtime (class file version 65.0), this version of the Java Runtime only recognizes class file versions up to 61.0\n")]));
        assert_eq!(top(&v), "java-version");
        assert!(v["primary"]["summary"].as_str().unwrap().contains("Java 21"), "{}", v["text"]);
    }

    #[test]
    fn mixin_blames_the_jar_that_owns_the_config() {
        use std::io::Write;
        let i = setup("mixin", &[("logs/latest.log", "[main/ERROR]: Mixin apply for mod shiny failed shiny.mixins.json:GameRendererMixin from mod shiny -> net.minecraft.client.render.GameRenderer: org.spongepowered.asm.mixin.injection.throwables.InjectionError: Critical injection failure\n")]);
        let d = i.root.join("mods"); std::fs::create_dir_all(&d).unwrap();
        let mut z = zip::ZipWriter::new(std::fs::File::create(d.join("shiny-1.0.jar")).unwrap());
        z.start_file("fabric.mod.json", zip::write::SimpleFileOptions::default()).unwrap(); z.write_all(br#"{"id":"shiny","name":"Shiny","version":"1.0"}"#).unwrap();
        z.start_file("shiny.mixins.json", zip::write::SimpleFileOptions::default()).unwrap(); z.write_all(b"{}").unwrap(); z.finish().unwrap();
        let v = run(&i);
        assert_eq!(top(&v), "mixin-failure");
        assert_eq!(v["primary"]["suspects"][0]["file"], "shiny-1.0.jar");
    }

    #[test]
    fn stack_trace_is_mapped_to_a_mod_and_crash_report_suspects_are_used() {
        use std::io::Write;
        let report = "---- Minecraft Crash Report ----\nDescription: Ticking entity\n\njava.lang.NullPointerException: Cannot invoke \"x\" because \"y\" is null\n\tat com.shiny.mod.Thing.tick(Thing.java:42)\n\tat net.minecraft.world.World.tick(World.java:1)\n\n-- Head --\nSuspected Mods: \n\tShiny (shiny), Version: 1.0\n";
        let i = setup("npe", &[("crash-reports/crash-2026-01-01_00.00.00-client.txt", report)]);
        let d = i.root.join("mods"); std::fs::create_dir_all(&d).unwrap();
        let mut z = zip::ZipWriter::new(std::fs::File::create(d.join("shiny-1.0.jar")).unwrap());
        z.start_file("fabric.mod.json", zip::write::SimpleFileOptions::default()).unwrap(); z.write_all(br#"{"id":"shiny","name":"Shiny","version":"1.0"}"#).unwrap();
        z.start_file("com/shiny/mod/Thing.class", zip::write::SimpleFileOptions::default()).unwrap(); z.write_all(b"x").unwrap(); z.finish().unwrap();
        let v = run(&i);
        assert_eq!(v["status"], "crash");
        assert_eq!(top(&v), "mod-exception", "{}", v["text"]);
        assert_eq!(v["primary"]["suspects"][0]["id"], "shiny");
    }

    #[test]
    fn native_gpu_crash_and_server_issues() {
        let v = run(&setup("gpu", &[("hs_err_pid123.log", "#\n# A fatal error has been detected by the Java Runtime Environment:\n#\n#  EXCEPTION_ACCESS_VIOLATION (0xc0000005) at pc=0x00007ff8\n# Problematic frame:\n# C  [ig9icd64.dll+0x1b2c3]\n#\n# JRE version: OpenJDK Runtime Environment (17.0.9+9) \n")]));
        assert_eq!(top(&v), "graphics-driver");
        let mut s = setup("srv", &[("logs/latest.log", "[Server thread/ERROR]: **** FAILED TO BIND TO PORT!\n[Server thread/WARN]: Perhaps a server is already running on that port?\n")]);
        s.server = true; s.platform = "paper".into(); s.port = Some(25565);
        let v = run(&s);
        assert_eq!(top(&v), "port-in-use");
        let mut s = setup("eula", &[("logs/latest.log", "[main/INFO]: You need to agree to the EULA in order to run the server. Go to eula.txt for more info.\n")]);
        s.server = true; s.platform = "paper".into();
        let v = run(&s);
        assert_eq!(top(&v), "eula");
        assert_eq!(v["primary"]["action"], "accept-eula");
    }

    #[test]
    fn clean_and_empty() {
        let v = run(&setup("empty", &[]));
        assert_eq!(v["status"], "no-logs");
        let v = run(&setup("clean", &[("logs/latest.log", "[main/INFO]: Done (3.2s)!\n")]));
        assert_eq!(v["status"], "clean");
    }

    #[test]
    fn fixes_are_short_and_stepwise() {
        let v = run(&setup("steps", &[("logs/latest.log", "[main/ERROR]: Exception in thread \"Render thread\" java.lang.OutOfMemoryError: Java heap space\n\tat a.b.C.d(C.java:1)\n")]));
        let fixes = v["primary"]["fixes"].as_array().unwrap();
        assert!(fixes.len() >= 4, "{}", v["text"]);
        assert!(fixes.iter().all(|f| f.as_str().unwrap().split_whitespace().count() <= 30), "{}", v["text"]);
    }
}
