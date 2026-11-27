//! "Add Optimization" for the Custom setup flow (Fabric, NeoForge, Quilt and Forge).
//!
//! The text lists in assets/optimization/ (one per loader) define the set. Each loader has a ceiling: the option works
//! up to and including that game version and is refused above it. All four loaders use one rule for it: the newest game
//! version where the loader exists AND at least two anchor mods (see `anchors`) have a build; a `minecraft = ...` line
//! in a list is only an override of that. For each mod in the list:
//!
//!   1. check the project exists on Modrinth (by project id, or slug for Forge); if not, skip it;
//!   2. ask Modrinth for its builds for this instance's exact game version and loader (Quilt also accepts Fabric
//!      builds), and take the newest release, falling back to the newest beta/alpha only if there is no release;
//!      a mod with no such build is skipped, and so is one whose required dependencies have none;
//!   3. drop candidates that declare themselves incompatible with one already chosen;
//!   4. install through the normal installer (hash-checked downloads, dependency handling);
//!   5. scan the jars that landed and remove any of ours that conflict with another or are for the wrong loader
//!      (a jar whose metadata names an older patch release is kept and only reported: Modrinth listed it for this version).
//!
//! Applying is best effort: the instance always exists afterwards, and problems come back as data
//! (`error`, `skipped`, `warnings`) for the UI instead of failing the whole creation.
use crate::meta::runs;
use crate::mods::api_get;
use crate::mods::{self, Log};
use crate::store::Instance;
use crate::util::{parse_ver, track_expected, Res};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

/// Loaders the option supports.
pub const SUPPORTED: [&str; 4] = ["fabric", "neoforge", "quilt", "forge"];

/// The packs the ceilings are read from: plain text lists, embedded so the app never needs them on disk.
/// Format: `key = value` header lines (minecraft, <loader> = <loader version>), then a `[mods]`
/// section with one mod name per line. `#` starts a comment.
static REFERENCE_PACKS: [&str; 4] = [
    include_str!("../assets/optimization/Just_Optimized_FABRIC.txt"),
    include_str!("../assets/optimization/Just_Optimized_NEOFORGE.txt"),
    include_str!("../assets/optimization/Just_Optimized_QUILT.txt"),
    include_str!("../assets/optimization/Just_Optimized_FORGE.txt"),
];

/// Parse a mod list into `dependencies` (the loader and game version header lines) and `mods` (the names).
fn parse_pack(txt: &str) -> Value {
    let (mut deps, mut mods, mut in_mods) = (serde_json::Map::new(), Vec::new(), false);
    for line in txt.trim_start_matches('\u{feff}').lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if line.eq_ignore_ascii_case("[mods]") { in_mods = true; continue; }
        if in_mods { mods.push(json!(line)); }
        else if let Some((k, v)) = line.split_once('=') { deps.insert(k.trim().to_string(), json!(v.trim())); }
    }
    json!({"dependencies": deps, "mods": mods})
}

fn is_newer(a: &str, b: &str) -> bool { crate::util::is_newer_version(a, b) }

/// (loader, minecraft version) a modrinth.index.json asks for.
fn index_target(idx: &Value) -> Option<(String, String)> {
    let d = idx["dependencies"].as_object()?;
    let (_, loader) = mods::MRPACK_LOADERS.iter().find(|(k, _)| d.contains_key(*k))?;
    Some((loader.to_string(), d.get("minecraft")?.as_str()?.to_string()))
}

/// The loader a parsed list is for (from its loader header line).
fn pack_loader_of(idx: &Value) -> Option<&'static str> {
    let d = idx["dependencies"].as_object()?;
    mods::MRPACK_LOADERS.iter().find(|(k, _)| d.contains_key(*k)).map(|(_, l)| *l)
}

fn pack_target(txt: &str) -> Option<(String, String)> { index_target(&parse_pack(txt)) }

/// loader -> highest game version any reference pack pins with a `minecraft = ...` line (an override, not the rule).
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

/// How many anchor mods must have a build for a game version to count as supported.
const ANCHORS_NEEDED: usize = 2;

/// The mods a game version needs builds of before the option is offered for it (a loader whose core optimization
/// mods don't exist for a version isn't worth offering it on).
pub fn anchors(loader: &str) -> &'static [&'static str] {
    match loader {
        "forge" => &["Embeddium", "ModernFix"],
        "quilt" | "fabric" => &["Sodium", "Lithium"],
        "neoforge" => &["Sodium", "ModernFix"],
        _ => &[],
    }
}

/// Per loader: the ceiling game version, or why there is none.
#[derive(Clone, Debug, Default)]
pub struct Ceilings { pub versions: BTreeMap<String, String>, pub errors: BTreeMap<String, String> }

/// The ceilings: a pinned `minecraft` line wins, every other loader asks `newest_supported(loader)` (see
/// `newest_with_anchors`). A loader that gets neither a version nor an error is simply unlisted.
pub fn ceilings(newest_supported: impl Fn(&str) -> Res<String>) -> Ceilings {
    let mut c = Ceilings { versions: pack_ceilings().clone(), errors: BTreeMap::new() };
    for l in SUPPORTED {
        if c.versions.contains_key(l) { continue; }
        match newest_supported(l) { Ok(v) => { c.versions.insert(l.to_string(), v); } Err(e) => { c.errors.insert(l.to_string(), e); } }
    }
    c
}

/// Game versions (newest first) where `loader` exists and at least `ANCHORS_NEEDED` anchors have a build: the first one.
/// `anchor_builds[i]` is the set of game versions anchor `i` has a build for. `has_loader` is only asked about versions
/// the anchors already qualify, and may fail (network) without the search quietly moving on to an older version.
pub fn newest_with_anchors(loader: &str, versions: &[String], anchor_builds: &[HashSet<String>], has_loader: impl Fn(&str) -> Res<bool>) -> Res<String> {
    for mc in versions {
        if anchor_builds.iter().filter(|s| s.contains(mc)).count() < ANCHORS_NEEDED { continue; }
        if has_loader(mc)? { return Ok(mc.clone()); }
    }
    let name = crate::meta::label(loader);
    Err(format!("Add Optimization isn't available for {name} right now: no Minecraft version has {name} together with builds of at least {ANCHORS_NEEDED} of {}.", anchors(loader).join(", ")))
}

/// For each anchor of `loader`, the game versions Modrinth has a build for (any build a `loader` instance could run).
pub fn anchor_game_versions(loader: &str) -> Res<Vec<HashSet<String>>> {
    let names = anchors(loader);
    let forms: Vec<Vec<String>> = names.iter().map(|n| slug_forms(n, loader)).collect();
    let mut all: Vec<String> = forms.iter().flatten().cloned().collect();
    all.sort();
    all.dedup();
    let mut by_slug: HashMap<String, String> = HashMap::new();
    for p in api_get("/projects", &[("ids", serde_json::to_string(&all).unwrap_or_default())])?.as_array().into_iter().flatten() {
        if let (Some(sl), Some(id)) = (p["slug"].as_str(), p["id"].as_str()) { by_slug.insert(sl.to_lowercase(), id.to_string()); }
    }
    let ok = runs(loader);
    let mut out = vec![];
    for f in &forms {
        let mut set = HashSet::new();
        if let Some(id) = f.iter().find_map(|sl| by_slug.get(sl)) {
            let vs = api_get(&format!("/project/{id}/version"), &[("loaders", mods::jl(ok))])?;
            for v in vs.as_array().into_iter().flatten() {
                if str_list(v, "loaders").iter().any(|l| ok.contains(&l.as_str())) { set.extend(str_list(v, "game_versions")); }
            }
        }
        out.push(set);
    }
    Ok(out)
}

/// Server-side gate: the UI hides the option, but the API enforces the same rule. The option covers the
/// ceiling version and every release version below it; whether a given mod exists for that version is
/// decided per mod when resolving, never assumed here.
pub fn check(loader: &str, mc: &str, ceilings: &Ceilings) -> Result<(), String> {
    if !SUPPORTED.contains(&loader) { return Err("Add Optimization works with Fabric, NeoForge, Quilt and Forge instances.".into()); }
    let name = crate::meta::label(loader);
    match ceilings.versions.get(loader) {
        None => Err(ceilings.errors.get(loader).cloned().unwrap_or_else(|| format!("Add Optimization has no target game version for {name}."))),
        Some(c) => match (parse_ver(mc).filter(|v| v.is_release()), parse_ver(c)) {
            (Some(m), Some(t)) if m <= t => Ok(()),
            (None, _) => Err(format!("Add Optimization needs a release version of Minecraft (this instance is {mc}).")),
            _ => Err(format!("Add Optimization is available for {name} up to Minecraft {c} (this instance is {mc}).")),
        },
    }
}

// ---------------------------------------------------------------- resolving

#[derive(Clone, Debug)]
struct Candidate { pid: String, title: String, incompatible: HashSet<String>, required: HashSet<String>, pinned: Option<Value> }

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
    str_list(v, "game_versions").iter().any(|g| g == mc) && str_list(v, "loaders").iter().any(|l| runs(loader).contains(&l.as_str()))
}

fn evaluate(hit: &Value, mc: &str, loader: &str, memo: &Mutex<HashMap<String, Result<bool, String>>>) -> Result<Candidate, Reject> {
    let title = hit["title"].as_str().unwrap_or("A mod").to_string();
    let pid = hit["project_id"].as_str().ok_or_else(|| Reject::Skip("no project id".into()))?.to_string();
    if hit["client_side"] == "unsupported" { return Err(Reject::Skip("server-only, nothing to do on a client".into())); }
    let net = |e: String| if is_net(&e) { Reject::Net(e) } else { Reject::Skip(e) };
    let version = mods::best_version(&pid, mc, Some(loader)).map_err(net)?
        .filter(|v| fits(v, mc, loader))
        .ok_or_else(|| Reject::Skip(format!("no build for {} {mc}", crate::meta::label(loader))))?;
    let (mut incompatible, mut required) = (HashSet::new(), HashSet::new());
    for dep in version["dependencies"].as_array().into_iter().flatten() {
        match dep["dependency_type"].as_str() {
            Some("incompatible") => { if let Some(p) = dep["project_id"].as_str() { incompatible.insert(p.to_string()); } }
            Some("required") => {
                let Some(dp) = dep_project(dep).map_err(net)? else { continue };
                if dp != pid {
                    if !has_build(&dp, mc, loader, memo).map_err(net)? {
                        return Err(Reject::Skip(format!("needs a library that has no {} {mc} build", crate::meta::label(loader))));
                    }
                    required.insert(dp);
                }
            }
            _ => {}
        }
    }
    Ok(Candidate { pid, title, incompatible, required, pinned: None })
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

/// The parsed list for this loader, if there is one.
fn reference_pack(loader: &str) -> Option<Value> {
    REFERENCE_PACKS.iter().map(|b| parse_pack(b)).find(|idx| pack_loader_of(idx) == Some(loader))
}

/// Lower-case letters and digits only, so "Mod Menu", "mod-menu" and "modmenu" compare equal.
fn norm(s: &str) -> String { s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect() }

/// Likely Modrinth slugs for a mod name: hyphenated, run together, and each with the loader appended.
fn slug_forms(name: &str, loader: &str) -> Vec<String> {
    let lower = name.to_lowercase();
    let hy = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join("-");
    let joined = norm(name);
    let mut out = vec![hy.clone(), joined.clone()];
    for l in runs(loader) { out.push(format!("{hy}-{l}")); out.push(format!("{joined}-{l}")); }
    out.dedup();
    out
}

fn resolve(mc: &str, loader: &str) -> Result<Plan, String> {
    let net = |e: String| if is_net(&e) { OFFLINE.to_string() } else { format!("Couldn't look up optimization mods ({e}).") };
    let mut skipped: Vec<(String, String)> = vec![];
    let pack = reference_pack(loader).unwrap_or(Value::Null);
    let (mut names, mut seen_n): (Vec<String>, HashSet<String>) = (vec![], HashSet::new());
    for n in str_list(&pack, "mods") { if seen_n.insert(norm(&n)) { names.push(n); } }
    if names.is_empty() { return Err(format!("There is no optimization list for {}.", crate::meta::label(loader))); }
    // 1. does each mod exist on Modrinth? First by likely slugs (one batched request), then by searching its name.
    let mut by_slug: HashMap<String, Value> = HashMap::new();
    let cands: Vec<Vec<String>> = names.iter().map(|n| slug_forms(n, loader)).collect();
    let all: Vec<String> = { let mut u: Vec<String> = cands.iter().flatten().cloned().collect(); u.sort(); u.dedup(); u };
    for chunk in all.chunks(100) {
        let ids = serde_json::to_string(chunk).unwrap_or_default();
        for p in api_get("/projects", &[("ids", ids)]).map_err(net)?.as_array().into_iter().flatten() {
            if let Some(sl) = p["slug"].as_str() { by_slug.insert(sl.to_lowercase(), p.clone()); }
        }
    }
    let direct: Vec<Option<Value>> = cands.iter().map(|c| c.iter().find_map(|sl| by_slug.get(sl).cloned())).collect();
    let missing: Vec<(usize, &String)> = names.iter().enumerate().filter(|(i, _)| direct[*i].is_none()).collect();
    let searched = mods::par_map(&missing, 6, |(_, n)| -> Result<Option<Value>, String> {
        let res = api_get("/search", &[("query", n.to_string()), ("limit", "10".into()), ("facets", "[[\"project_type:mod\"]]".into())])?;
        let want = norm(n);
        Ok(res["hits"].as_array().into_iter().flatten()
            .find(|h| h["slug"].as_str().map(norm).as_deref() == Some(want.as_str()) || h["title"].as_str().map(norm).as_deref() == Some(want.as_str()))
            .map(|h| json!({"id": h["project_id"], "slug": h["slug"], "title": h["title"], "client_side": h["client_side"]})))
    });
    let mut from_search: HashMap<usize, Value> = HashMap::new();
    for ((i, _), r) in missing.iter().zip(searched) { if let Some(p) = r.map_err(net)? { from_search.insert(*i, p); } }
    let (mut order, mut projects): (Vec<(String, Value)>, HashMap<String, Value>) = (vec![], HashMap::new());
    for (i, n) in names.iter().enumerate() {
        match direct[i].clone().or_else(|| from_search.remove(&i)).and_then(|p| p["id"].as_str().map(|id| (id.to_string(), p.clone()))) {
            Some((id, p)) => { if projects.insert(id.clone(), p).is_none() { order.push((id, Value::Null)); } }
            None => skipped.push((n.clone(), "isn't on Modrinth".into())),
        }
    }
    // 2. for each mod that exists, evaluate() asks Modrinth for its builds for this exact game version and loader
    //    and takes the newest release (see mods::best_version); no such build means the mod is skipped
    let memo = Mutex::new(HashMap::new());
    let results = mods::par_map(&order, 6, |(pid, ver)| {
        let proj = projects.get(pid).cloned().unwrap_or(json!({}));
        let title = proj["title"].as_str().unwrap_or("A mod").to_string();
        if proj["client_side"] == "unsupported" { return Err(Reject::Skip("server-only, nothing to do on a client".into())); }
        let _ = (&ver, &title);
        evaluate(&json!({"project_id": pid, "title": title, "client_side": proj["client_side"]}), mc, loader, &memo)
    });
    let (mut ranked, mut net_err) = (vec![], None);
    for ((pid, _), r) in order.iter().zip(results) {
        match r {
            Ok(c) => ranked.push(c),
            Err(Reject::Skip(why)) => skipped.push((projects.get(pid).and_then(|p| p["title"].as_str()).unwrap_or("A mod").to_string(), why)),
            Err(Reject::Net(e)) => { net_err.get_or_insert(e); }
        }
    }
    if ranked.is_empty() {
        return Err(match net_err {
            Some(_) => OFFLINE.to_string(),
            None => format!("None of the optimization mods have a compatible {} {mc} build.", crate::meta::label(loader)),
        });
    }
    let (picks, clashed) = select(ranked, usize::MAX);
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

/// Install the resolved optimization set into `inst` (an instance on a supported loader at or below the ceiling
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
        let r = mods::install_project_with(inst, &c.pid, c.pinned.as_ref(), true, &mut seen, "mod", log);
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
        // Modrinth already lists these for this exact game version, so a jar whose own metadata names an older patch release is kept and only reported
        for (f, range) in &rep.mc_mismatch { warnings.push(format!("{f} declares Minecraft {range}; kept because Modrinth lists it for {mc}.")); }
        for (f, _) in &rep.wrong_loader { if let Some(i) = pos(f) { victims.push((i, format!("isn't a {} mod", crate::meta::label(&loader)))); } }
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

    fn cand(pid: &str, inc: &[&str]) -> Candidate { Candidate { pid: pid.into(), title: pid.to_uppercase(), incompatible: inc.iter().map(|s| s.to_string()).collect(), required: HashSet::new(), pinned: None } }

    fn vs(l: &[&str]) -> Vec<String> { l.iter().map(|s| s.to_string()).collect() }
    fn set(l: &[&str]) -> HashSet<String> { l.iter().map(|s| s.to_string()).collect() }

    #[test]
    fn only_a_pinned_line_overrides_the_rule() {
        let c = ceilings(|_| Err("none".into()));
        assert_eq!(c.versions.get("fabric").map(String::as_str), Some("26.3"));   // pinned in its list
        assert!(!c.versions.contains_key("neoforge") && !c.versions.contains_key("forge") && !c.versions.contains_key("quilt"));
        assert_eq!(c.errors.len(), 3);
    }

    #[test]
    fn unpinned_loaders_take_the_computed_version() {
        let c = ceilings(|l| Ok(format!("computed-{l}")));
        assert_eq!(c.versions["fabric"], "26.3");           // pin wins when there is one
        assert_eq!(c.versions["neoforge"], "computed-neoforge");
        assert!(c.errors.is_empty());
    }

    #[test]
    fn gate_allows_the_ceiling_and_older_release_versions_on_supported_loaders() {
        let c = ceilings(|l| match l { "forge" => Ok("1.20.1".to_string()), "neoforge" => Ok("26.1".to_string()), "quilt" => Ok("1.21.1".to_string()), _ => Err("x".into()) });
        assert!(check("fabric", "26.3", &c).is_ok());
        assert!(check("neoforge", "26.1", &c).is_ok());
        assert!(check("neoforge", "1.21.1", &c).is_ok());      // no longer stuck at 1.21.1
        assert!(check("fabric", "1.21.1", &c).is_ok());
        assert!(check("neoforge", "1.20.4", &c).is_ok());
        assert!(check("neoforge", "26.3", &c).is_err());       // above NeoForge's ceiling
        assert!(check("fabric", "26.4", &c).is_err());
        assert!(check("fabric", "24w10a", &c).is_err());       // snapshots
        assert!(check("fabric", "1.21.1-pre1", &c).is_err());  // pre-releases
        assert!(check("forge", "1.20.1", &c).is_ok());
        assert!(check("forge", "1.21.1", &c).is_err());
        assert!(check("quilt", "1.21.1", &c).is_ok());
        let none = ceilings(|l| Err(format!("no {l}")));
        assert_eq!(check("forge", "1.20.1", &none).unwrap_err(), "no forge");   // the search's own error comes back
        assert_eq!(runs("quilt"), ["quilt", "fabric"]);
        assert_eq!(pack_loader_of(&parse_pack(REFERENCE_PACKS[2])), Some("quilt"));
        assert_eq!(pack_loader_of(&parse_pack(REFERENCE_PACKS[1])), Some("neoforge"));
        assert!(!str_list(&reference_pack("quilt").unwrap(), "mods").is_empty());
        assert!(str_list(&reference_pack("forge").unwrap(), "mods").contains(&"Embeddium".to_string()));
        assert!(pack_target(REFERENCE_PACKS[1]).is_none() && pack_target(REFERENCE_PACKS[2]).is_none() && pack_target(REFERENCE_PACKS[3]).is_none()); // no pin
        assert_eq!(norm("Mod Menu"), norm("mod-menu"));
        assert!(slug_forms("Mod Menu", "fabric").contains(&"modmenu".to_string()) && slug_forms("Sodium", "quilt").contains(&"sodium-fabric".to_string()));
        assert!(check("vanilla", "26.3", &c).is_err());
    }

    #[test]
    fn neoforge_list_has_no_fabric_only_mods() {
        let l = str_list(&reference_pack("neoforge").unwrap(), "mods");
        for bad in ["Fabric API", "Mod Menu", "Lithium", "C2ME", "VMP", "Krypton", "Ixeris", "Fabric Language Kotlin", "KotlinLangForge"] { assert!(!l.contains(&bad.to_string()), "{bad}"); }
        for good in ["Sodium", "Sodium Extra", "Reese's Sodium Options"] { assert!(l.contains(&good.to_string()), "{good}"); }
        assert_eq!(l.iter().filter(|m| norm(m).contains("kotlin")).count(), 1);
    }

    #[test]
    fn ceiling_is_the_newest_version_with_the_loader_and_two_anchors() {
        let versions = vs(&["26.3", "26.2", "1.21.11", "1.21.1", "1.20.1"]);
        // Embeddium stops at 1.21.1, ModernFix reaches 1.21.11; 26.x has neither
        let anchors = [set(&["1.21.1", "1.20.1"]), set(&["1.21.11", "1.21.1", "1.20.1"])];
        let got = newest_with_anchors("forge", &versions, &anchors, |_| Ok(true));
        assert_eq!(got.unwrap(), "1.21.1");                      // 1.21.11 has only one anchor
        // the loader missing on the newest qualifying version moves the search down
        let got = newest_with_anchors("forge", &versions, &anchors, |mc| Ok(mc != "1.21.1"));
        assert_eq!(got.unwrap(), "1.20.1");
    }

    #[test]
    fn the_search_goes_past_the_newest_ten_versions() {
        let versions: Vec<String> = (0..40).map(|i| format!("1.{}", 40 - i)).collect();        // 1.40 ... 1.1
        let anchors = [set(&["1.3"]), set(&["1.3", "1.2"])];
        assert_eq!(newest_with_anchors("quilt", &versions, &anchors, |_| Ok(true)).unwrap(), "1.3");
    }

    #[test]
    fn no_version_qualifies_is_a_clear_error() {
        let versions = vs(&["1.21.1", "1.20.1"]);
        let e = newest_with_anchors("forge", &versions, &[set(&["1.21.1"]), set(&["1.20.1"])], |_| Ok(true)).unwrap_err();
        assert!(e.contains("Forge") && e.contains("Embeddium") && e.contains("ModernFix"), "{e}");
        let e = newest_with_anchors("quilt", &versions, &[set(&["1.21.1"]), set(&["1.21.1"])], |_| Ok(false)).unwrap_err();
        assert!(e.contains("Quilt") && e.contains("Sodium") && e.contains("Lithium"), "{e}");
        // a failed loader lookup is passed on, not read as "no loader here"
        let e = newest_with_anchors("quilt", &versions, &[set(&["1.21.1"]), set(&["1.21.1"])], |_| Err("offline".into())).unwrap_err();
        assert_eq!(e, "offline");
        assert_eq!(anchors("forge"), ["Embeddium", "ModernFix"]);
        assert_eq!(anchors("quilt"), ["Sodium", "Lithium"]);
    }

    #[test]
    fn newer_compares_numerically() {
        assert!(is_newer("26.3", "1.21.1"));
        assert!(is_newer("1.21.10", "1.21.9"));
        assert!(!is_newer("1.21.1", "1.21.1"));
        assert!(!is_newer("24w10a", "1.21"));
        assert!(is_newer("1.21.1", "1.21.1-beta"));
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
