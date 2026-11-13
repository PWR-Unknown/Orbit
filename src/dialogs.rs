//! Native folder / save dialogs. With the `gui` feature they use rfd; headless builds fall back to
//! zenity/kdialog on Linux and otherwise report "unavailable".
use std::path::Path;

#[cfg(feature = "gui")]
pub fn pick_folder(start: &str) -> Option<String> {
    let mut d = rfd::FileDialog::new();
    if !start.is_empty() && Path::new(start).is_dir() { d = d.set_directory(start); }
    d.pick_folder().map(|p| p.to_string_lossy().into_owned())
}

#[cfg(feature = "gui")]
pub fn pick_save(name: &str, start: &str) -> Option<String> {
    let mut d = rfd::FileDialog::new().add_filter("Modrinth modpack", &["mrpack"]).set_file_name(name);
    if !start.is_empty() && Path::new(start).is_dir() { d = d.set_directory(start); }
    d.save_file().map(|p| p.to_string_lossy().into_owned())
}

#[cfg(not(feature = "gui"))]
pub fn pick_folder(start: &str) -> Option<String> {
    let start = if Path::new(start).is_dir() { start.to_string() } else { crate::store::home_dir().to_string_lossy().into_owned() };
    run(&[("zenity", vec!["--file-selection".into(), "--directory".into(), format!("--filename={}/", start.trim_end_matches('/'))]),
          ("kdialog", vec!["--getexistingdirectory".into(), start])])
}

#[cfg(not(feature = "gui"))]
pub fn pick_save(name: &str, start: &str) -> Option<String> {
    let dir = if Path::new(start).is_dir() { start.to_string() } else { crate::store::home_dir().to_string_lossy().into_owned() };
    let full = Path::new(&dir).join(name).to_string_lossy().into_owned();
    run(&[("zenity", vec!["--file-selection".into(), "--save".into(), "--confirm-overwrite".into(), format!("--filename={full}")]),
          ("kdialog", vec!["--getsavefilename".into(), full, "*.mrpack".into()])])
}

#[cfg(not(feature = "gui"))]
fn run(cmds: &[(&str, Vec<String>)]) -> Option<String> {
    for (c, args) in cmds {
        if let Ok(o) = std::process::Command::new(c).args(args).output() {
            let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
            return if t.is_empty() { None } else { Some(t) };
        }
    }
    None
}
