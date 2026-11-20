//! "Add Optimization" for the Custom setup flow (Fabric and NeoForge).
//!
//! Nothing here names a mod or a project id. The two reference packs in assets/optimization/ are only
//! read for the game version each loader's optimization set targets (`dependencies.minecraft` in
//! their modrinth.index.json). That version is the ceiling: the option works for exactly that version
//! and is refused for any other. Which mods get installed is decided live, per instance, from Modrinth:
//!
//!   1. search the "optimization" category for this loader + game version, most downloaded first;
//!   2. per candidate, ask Modrinth for a real build for this exact version + loader, and check that
//!      every *required* dependency has one too (so a mod is never installed half-working);
//!   3. drop candidates that declare themselves incompatible with one already chosen;
//!   4. install through the normal installer (hash-checked downloads, dependency handling);
//!   5. scan the jars that landed and remove any of ours the jar metadata says can't run here.
//!
//! Applying is best effort: the instance always exists afterwards, and problems come back as data
//! (`error`, `skipped`, `warnings`) for the UI instead of failing the whole creation.
use crate::deps;
use crate::modrinth::api_get;
use crate::mods::{self, Log};
use crate::store::Instance;
use crate::util::{track_expected, Res};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::sync::{Mutex, OnceLock};

/// Loaders the option supports.
pub const SUPPORTED: [&str; 2] = ["fabric", "neoforge"];
/// Modrinth's own category for performance mods; a category name, not a project.
const CATEGORY: &str = "optimization";
/// How many search hits to consider. More than we install, because some get rejected.
const CANDIDATES: usize = 40;
/// Upper bound on how many top-level optimization mods one instance gets.
const MAX_MODS: usize = 14;

/// The packs the ceilings are read from. Embedded, so the app never needs them on disk.
static REFERENCE_PACKS: [&[u8]; 2] = [
    include_bytes!("../assets/optimization/Just_Optimized_FABRIC.mrpack"),
    include_bytes!("../assets/optimization/Just_Optimized_NEOFORGE.mrpack"),
];

fn parse_ver(v: &str) -> Option<Vec<u64>> { v.split('.').map(|p| p.parse().ok()).collect() }

fn is_newer(a: &str, b: &str) -> bool {
    match (parse_ver(a), parse_ver(b)) { (Some(x), Some(y)) => deps::cmp_ver(&x, &y) == std::cmp::Ordering::Greater, _ => false }
}

/// (loader, minecraft version) a modrinth.index.json asks for.
fn index_target(idx: &Value) -> Option<(String, String)> {
    let d = idx["dependencies"].as_object()?;
    let (_, loader) = mods::MRPACK_LOADERS.iter().find(|(k, _)| d.contains_key(*k))?;
    Some((loader.to_string(), d.get("minecraft")?.as_str()?.to_string()))
}

fn pack_target(bytes: &[u8]) -> Option<(String, String)> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).ok()?;
    let mut s = String::new();
    z.by_name("modrinth.index.json").ok()?.read_to_string(&mut s).ok()?;
    index_target(&serde_json::from_str(s.trim_start_matches('\u{feff}')).ok()?)
}

/// loader -> highest game version any reference pack declares for it.
fn pack_ceilings() -> &'static BTreeMap<String, String> {
    static C: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    C.get_or_init(|| {
        let mut m: BTreeMap<String, String> = BTreeMap::new();
        for (loader, mc) in REFERENCE_PACKS.iter().filter_map(|b| pack_target(b)) {
            if !SUPPORTED.contains(&loader.as_str()) { continue; }
            match m.get(&loader) { Some(cur) if !is_newer(&mc, cur) => {} _ => { m.insert(loader, mc); } }
        }
        m
    })
}

/// The one game version per loader the option is available for. A loader the packs don't cover falls
/// back to `newest_supported(loader)`, the highest game version the loader itself has builds for.
pub fn ceilings(newest_supported: impl Fn(&str) -> Option<String>) -> BTreeMap<String, String> {
    let mut m = pack_ceilings().clone();
    for l in SUPPORTED { if !m.contains_key(l) { if let Some(v) = newest_supported(l) { m.insert(l.to_string(), v); } } }
    m
}

/// Server-side gate: the UI hides the option, but the API enforces the same rule. The option covers the
/// ceiling version and every release version below it; whether a given mod exists for that version is
/// decided per mod when resolving, never assumed here.
pub fn check(loader: &str, mc: &str, ceilings: &BTreeMap<String, String>) -> Result<(), String> {
    if !SUPPORTED.contains(&loader) { return Err("Add Optimization works with Fabric and NeoForge instances.".into()); }
    let name = crate::tl::label(loader);
    match ceilings.get(loader) {
        None => Err(format!("Add Optimization has no target game version for {name}.")),
        Some(c) => match (parse_ver(mc), parse_ver(c)) {
            (Some(m), Some(t)) if deps::cmp_ver(&m, &t) != std::cmp::Ordering::Greater => Ok(()),
            (None, _) => Err(format!("Add Optimization needs a release version of Minecraft (this instance is {mc}).")),
            _ => Err(format!("Add Optimization is available for {name} up to Minecraft {c} (this instance is {mc}).")),
        },
    }
}

// ---------------------------------------------------------------- resolving

#[derive(Clone, Debug)]
struct Candidate { pid: String, title: String, incompatible: HashSet<String>, required: HashSet<String> }

enum Reject { Skip(String), Net(String) }

const OFFLINE: &str = "Couldn't reach Modrinth to look up optimization mods. Check your connection, then add them from Discover.";

fn is_net(e: &str) -> bool { e.starts_with("Network error") || e.starts_with("Modrinth API error") }

fn str_list(v: &Value, k: &str) -> Vec<String> { v[k].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect() }

/// The project a dependency entry points at (it may only carry a version id).
fn dep_project(dep: &Value) -> Res<Option<String>> {
    if let Some(p) = dep["project_id"].as_str() { return Ok(Some(p.to_string())); }
    match dep["version_id"].as_str() {
        Some(v) => Ok(api_get(&format!("/version/{v}"), &[])?["project_id"].as_str().map(String::from)),
        None => Ok(None),
    }
}

/// Does `pid` have a build for this exact game version and loader? Memoised: shared dependencies repeat.
fn has_build(pid: &str, mc: &str, loader: &str, memo: &Mutex<HashMap<String, Result<bool, String>>>) -> Result<bool, String> {
    if let Some(r) = memo.lock().unwrap_or_else(|e| e.into_inner()).get(pid) { return r.clone(); }
    let r = mods::best_version(pid, mc, Some(loader)).map(|v| v.map(|v| fits(&v, mc, loader)).unwrap_or(false));
    memo.lock().unwrap_or_else(|e| e.into_inner()).insert(pid.to_string(), r.clone());
    r
}

/// Don't trust the query filter alone: the version must list this game version and loader itself.
fn fits(v: &Value, mc: &str, loader: &str) -> bool {
    str_list(v, "game_versions").iter().any(|g| g == mc) && str_list(v, "loaders").iter().any(|l| l == loader)
}

fn evaluate(hit: &Value, mc: &str, loader: &str, memo: &Mutex<HashMap<String, Result<bool, String>>>) -> Result<Candidate, Reject> {
    let title = hit["title"].as_str().unwrap_or("A mod").to_string();
    let pid = hit["project_id"].as_str().ok_or_else(|| Reject::Skip("no project id".into()))?.to_string();
    if hit["client_side"] == "unsupported" { return Err(Reject::Skip("server-only, nothing to do on a client".into())); }
    let net = |e: String| if is_net(&e) { Reject::Net(e) } else { Reject::Skip(e) };
    let version = mods::best_version(&pid, mc, Some(loader)).map_err(net)?
        .filter(|v| fits(v, mc, loader))
        .ok_or_else(|| Reject::Skip(format!("no build for {} {mc}", crate::tl::label(loader))))?;
    let (mut incompatible, mut required) = (HashSet::new(), HashSet::new());
    for dep in version["dependencies"].as_array().into_iter().flatten() {
        match dep["dependency_type"].as_str() {
            Some("incompatible") => { if let Some(p) = dep["project_id"].as_str() { incompatible.insert(p.to_string()); } }
            Some("required") => {
                let Some(dp) = dep_project(dep).map_err(net)? else { continue };
                if dp != pid {
                    if !has_build(&dp, mc, loader, memo).map_err(net)? {
                        return Err(Reject::Skip(format!("needs a library that has no {} {mc} build", crate::tl::label(loader))));
                    }
                    required.insert(dp);
                }
            }
            _ => {}
        }
    }
    Ok(Candidate { pid, title, incompatible, required })
}

/// Walk candidates best-first, keep those that don't clash with what is already kept. Pure, so it is testable.
fn select(ranked: Vec<Candidate>, max: usize) -> (Vec<Candidate>, Vec<(String, String)>) {
    let (mut kept, mut skipped): (Vec<Candidate>, Vec<(String, String)>) = (vec![], vec![]);
    for c in ranked {
        if kept.len() >= max { break; }
        let clash = kept.iter().find(|k| k.incompatible.contains(&c.pid) || c.incompatible.contains(&k.pid));
        match clash {
            Some(k) => skipped.push((c.title.clone(), format!("incompatible with {}", k.title))),
            None => kept.push(c),
        }
    }
    (kept, skipped)
}

struct Plan { picks: Vec<Candidate>, skipped: Vec<(String, String)> }

fn resolve(mc: &str, loader: &str) -> Result<Plan, String> {
    let facets = json!([["project_type:mod"], ["categories!=datapack"], [format!("categories:{CATEGORY}")], [format!("categories:{loader}")], [format!("versions:{mc}")]]);
    let res = api_get("/search", &[("facets", facets.to_string()), ("limit", CANDIDATES.to_string()), ("index", "downloads".into())])
        .map_err(|e| if is_net(&e) { OFFLINE.to_string() } else { format!("Couldn't look up optimization mods ({e}).") })?;
    let hits: Vec<Value> = res["hits"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        return Err(format!("Modrinth lists no optimization mods for {} {mc} yet.", crate::tl::label(loader)));
    }
    let memo = Mutex::new(HashMap::new());
    let results = mods::par_map(&hits, 6, |h| evaluate(h, mc, loader, &memo));
    let (mut ranked, mut skipped, mut net_err) = (vec![], vec![], None);
    for (h, r) in hits.iter().zip(results) {
        match r {
            Ok(c) => ranked.push(c),
            Err(Reject::Skip(why)) => skipped.push((h["title"].as_str().unwrap_or("A mod").to_string(), why)),
            Err(Reject::Net(e)) => { net_err.get_or_insert(e); }
        }
    }
    if ranked.is_empty() {
        return Err(match net_err {
            Some(_) => OFFLINE.to_string(),
            None => format!("None of the optimization mods on Modrinth have a compatible {} {mc} build.", crate::tl::label(loader)),
        });
    }
    let (picks, clashed) = select(ranked, MAX_MODS);
    skipped.extend(clashed);
    Ok(Plan { picks, skipped })
}

// ---------------------------------------------------------------- applying

fn skip_json(list: &[(String, String)]) -> Vec<Value> { list.iter().map(|(t, r)| json!({"title": t, "reason": r})).collect() }

/// Remove libraries that only existed for a mod we dropped. A library stays if another kept mod owns it,
/// or if it was installed explicitly (not as someone's dependency).
fn remove_orphans(inst: &mut Instance, deps: &HashSet<String>, owned: &HashMap<String, HashSet<String>>) {
    for k in deps {
        let dep_only = inst.mods().get(k).map(|m| m["explicit"] != true).unwrap_or(false);
        if dep_only && !owned.values().any(|d| d.contains(k)) { remove_installed(inst, k); }
    }
}

fn remove_installed(inst: &mut Instance, pid: &str) {
    if let Some(m) = inst.mods_mut().remove(pid) { let _ = std::fs::remove_file(mods::content_path(inst, &m)); }
}

/// Install the resolved optimization set into `inst` (a Fabric or NeoForge instance already at the ceiling
/// version). Never fails: the returned object says what happened.
pub fn apply(inst: &mut Instance, log: &mut Log) -> Value {
    let (mc, loader) = (inst.mc(), inst.loader());
    let fail = |e: String| json!({"requested": true, "ok": false, "error": e, "installed": [], "skipped": [], "warnings": []});
    let plan = match resolve(&mc, &loader) { Ok(p) => p, Err(e) => { log.push(format!("! {e}")); return fail(e); } };
    track_expected(plan.picks.len());
    let (mut skipped, mut warnings) = (plan.skipped.clone(), Vec::<String>::new());
    let (mut ours, mut seen) = (Vec::<Candidate>::new(), HashSet::new());
    let mut owned: HashMap<String, HashSet<String>> = HashMap::new();   // kept mod -> libraries installed for it

    for c in &plan.picks {
        let before: HashSet<String> = inst.mods().keys().cloned().collect();
        let r = mods::install_project(inst, &c.pid, true, &mut seen, "mod", log);
        let fresh: HashSet<String> = inst.mods().keys().filter(|k| !before.contains(*k) && **k != c.pid).cloned().collect();
        match r {
            Ok(()) if inst.mods().contains_key(&c.pid) => {
                owned.insert(c.pid.clone(), fresh.into_iter().chain(c.required.iter().cloned()).collect());
                ours.push(c.clone());
            }
            other => {
                // Not installed: leave nothing of it behind, neither the mod nor libraries pulled in for it.
                skipped.push((c.title.clone(), other.err().unwrap_or_else(|| "wasn't installed (already present from another source, or its build vanished)".into())));
                remove_orphans(inst, &fresh, &owned);
            }
        }
        let _ = inst.save();
    }

    // Anything the jars themselves say is missing (declared dependencies Modrinth didn't list).
    if let Some(o) = mods::ensure_deps(inst, log) {
        for u in &o.unresolved { warnings.push(format!("{} couldn't be installed automatically ({}).", u.id, u.why)); }
        if o.offline { warnings.push("Modrinth became unreachable while checking dependencies.".into()); }
    }

    // Last check, on the real files: drop any of ours the jar metadata says can't run here.
    if let Some(rep) = mods::deps_report(inst) {
        let file_of = |pid: &str| inst.mods().get(pid).and_then(|m| m["filename"].as_str().map(String::from)).unwrap_or_default();
        let order: Vec<(String, String)> = ours.iter().map(|c| (c.pid.clone(), file_of(&c.pid))).collect();
        let pos = |f: &str| order.iter().position(|(_, n)| n == f);
        let mut victims: Vec<(usize, String)> = vec![];
        for (a, b, why) in &rep.conflicts {
            if let Some(i) = [pos(a), pos(b)].into_iter().flatten().max() { victims.push((i, why.clone())); }
        }
        for (f, range) in &rep.mc_mismatch { if let Some(i) = pos(f) { victims.push((i, format!("built for Minecraft {range}, not {mc}"))); } }
        for (f, _) in &rep.wrong_loader { if let Some(i) = pos(f) { victims.push((i, format!("isn't a {} mod", crate::tl::label(&loader)))); } }
        victims.sort_by_key(|v| v.0);
        victims.dedup_by_key(|v| v.0);
        for (i, why) in victims.iter().rev() {
            let pid = order[*i].0.clone();
            remove_installed(inst, &pid);
            if let Some(d) = owned.remove(&pid) { remove_orphans(inst, &d, &owned); }
            if let Some(p) = ours.iter().position(|c| c.pid == pid) { let c = ours.remove(p); skipped.push((c.title, why.clone())); }
        }
    }
    if !skipped.is_empty() { let _ = mods::ensure_deps(inst, log); }   // a library shared with a dropped mod comes back if a kept one needs it
    let _ = inst.save();

    let installed: Vec<Value> = ours.iter().map(|c| {
        let m = inst.mods().get(&c.pid).cloned().unwrap_or(json!({}));
        json!({"title": c.title, "version": m["version"], "pid": c.pid})
    }).collect();
    if installed.is_empty() {
        let e = "No optimization mods could be installed.".to_string();
        log.push(format!("! {e}"));
        let mut r = fail(e);
        r["skipped"] = json!(skip_json(&skipped));
        r["warnings"] = json!(warnings);
        return r;
    }
    log.push(format!("Added {} optimization mods", installed.len()));
    json!({"requested": true, "ok": true, "installed": installed, "skipped": skip_json(&skipped), "warnings": warnings})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(pid: &str, inc: &[&str]) -> Candidate { Candidate { pid: pid.into(), title: pid.to_uppercase(), incompatible: inc.iter().map(|s| s.to_string()).collect(), required: HashSet::new() } }

    #[test]
    fn ceilings_come_from_the_reference_packs() {
        let c = ceilings(|_| None);
        assert_eq!(c.get("fabric").map(String::as_str), Some("26.3"));
        assert_eq!(c.get("neoforge").map(String::as_str), Some("1.21.1"));
    }

    #[test]
    fn missing_pack_falls_back_to_newest_supported() {
        let c = ceilings(|l| Some(format!("fallback-{l}")));
        assert_eq!(c["fabric"], "26.3");           // pack wins when there is one
    }

    #[test]
    fn gate_allows_the_ceiling_and_older_release_versions_on_supported_loaders() {
        let c = ceilings(|_| None);
        assert!(check("fabric", "26.3", &c).is_ok());
        assert!(check("neoforge", "1.21.1", &c).is_ok());
        assert!(check("fabric", "1.21.1", &c).is_ok());
        assert!(check("neoforge", "1.20.4", &c).is_ok());
        assert!(check("neoforge", "26.3", &c).is_err());      // above NeoForge's ceiling
        assert!(check("fabric", "26.4", &c).is_err());
        assert!(check("fabric", "24w10a", &c).is_err());       // snapshots
        assert!(check("neoforge", "1.21.1-pre1", &c).is_err());
        assert!(check("forge", "1.21.1", &c).is_err());
        assert!(check("vanilla", "26.3", &c).is_err());
    }

    #[test]
    fn newer_compares_numerically() {
        assert!(is_newer("26.3", "1.21.1"));
        assert!(is_newer("1.21.10", "1.21.9"));
        assert!(!is_newer("1.21.1", "1.21.1"));
        assert!(!is_newer("24w10a", "1.21"));
    }

    #[test]
    fn select_drops_mutually_incompatible_mods_keeping_the_better_ranked() {
        let (kept, skipped) = select(vec![cand("a", &[]), cand("b", &["a"]), cand("c", &[]), cand("d", &["c"])], 10);
        assert_eq!(kept.iter().map(|k| k.pid.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(skipped.len(), 2);
    }

    #[test]
    fn select_respects_the_cap() {
        let (kept, _) = select((0..5).map(|i| cand(&format!("m{i}"), &[])).collect(), 3);
        assert_eq!(kept.len(), 3);
    }
}
