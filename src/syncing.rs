//! Settings > Syncing: options.txt, servers.dat and the resourcepacks folder can be shared by every instance. The shared
//! copies live in <data>/sync. Just before a game starts, whatever is newer there is copied into the instance; when the
//! game closes, whatever the instance changed is copied back. These are copies, not links, so it works on every system
//! without special rights.
use crate::store;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// (settings key, name inside an instance and in the shared folder)
pub const ITEMS: [(&str, &str); 3] = [("sync_options", "options.txt"), ("sync_servers", "servers.dat"), ("sync_packs", "resourcepacks")];

pub fn dir() -> PathBuf { store::data_home().join("sync") }

/// Copies `from` over `to` where `to` is missing or older; a folder is merged file by file. Copies keep their time stamp.
fn newer(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            newer(&e.path(), &to.join(e.file_name()))?;
        }
        return Ok(());
    }
    if !from.is_file() { return Ok(()); }
    let stamp = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let (src, dst) = (stamp(from), stamp(to));
    if dst.is_some() && src <= dst { return Ok(()); }
    if let Some(d) = to.parent() { std::fs::create_dir_all(d)?; }
    std::fs::copy(from, to)?;
    if let Some(t) = src { std::fs::File::options().write(true).open(to)?.set_modified(t)?; }
    Ok(())
}

/// What went wrong while syncing the items that are switched on, one line each.
fn each(st: &Map<String, Value>, root: &Path, into_instance: bool) -> Vec<String> {
    let shared = dir();
    ITEMS.iter().filter(|(k, _)| st.get(*k).and_then(|v| v.as_bool()).unwrap_or(false)).filter_map(|(_, name)| {
        let (a, b) = (shared.join(name), root.join(name));
        let (from, to) = if into_instance { (a, b) } else { (b, a) };
        newer(&from, &to).err().map(|e| format!("Couldn't sync {name}: {e}"))
    }).collect()
}

/// Before a game starts: the shared copies come into the instance.
pub fn pull(root: &Path, st: &Map<String, Value>) -> Vec<String> { each(st, root, true) }

/// After a game closes: the instance's changes go into the shared copies.
pub fn push(root: &Path, st: &Map<String, Value>) -> Vec<String> { each(st, root, false) }

/// The edit button: opens the shared options.txt in the default editor, or the folder that holds the shared item.
pub fn open(item: &str) -> Result<(), String> {
    let (_, name) = ITEMS.iter().find(|(k, _)| k.strip_prefix("sync_") == Some(item)).ok_or("Unknown item.")?;
    let dir = dir();
    let target = dir.join(name);
    if *name == "resourcepacks" {
        crate::api::open_folder(&target);
    } else if !target.is_file() {
        return Err("Nothing is shared yet. It appears after you play an instance with this turned on.".into());
    } else if *name == "options.txt" {
        crate::api::open_path(&target.to_string_lossy());
    } else {
        crate::api::open_folder(&dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_copies_only_what_is_missing_or_older() {
        let tmp = std::env::temp_dir().join(format!("orbit-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (a, b) = (tmp.join("a"), tmp.join("b"));
        std::fs::create_dir_all(a.join("packs")).unwrap();
        std::fs::write(a.join("options.txt"), "one").unwrap();
        std::fs::write(a.join("packs").join("p.zip"), "zip").unwrap();
        newer(&a, &b).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("options.txt")).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(b.join("packs").join("p.zip")).unwrap(), "zip");
        std::fs::write(b.join("options.txt"), "two").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(b.join("options.txt")).unwrap().set_modified(later).unwrap();
        newer(&a, &b).unwrap();                                   // a is older: b keeps its change
        assert_eq!(std::fs::read_to_string(b.join("options.txt")).unwrap(), "two");
        newer(&b, &a).unwrap();                                   // b is newer: a gets it
        assert_eq!(std::fs::read_to_string(a.join("options.txt")).unwrap(), "two");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
