#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#![recursion_limit = "256"] //!Temp fix for now
//! Orbit (Rust). A native window (WebView2 on Windows, WebKitGTK on Linux) showing the same
//! UI as before, backed by a small loopback-only HTTP/JSON server with a per-run secret token.

mod api;
mod auth;
mod deps;
mod diagnose;
mod dialogs;
mod gamedata;
mod icons;
mod importers;
mod launcher;
mod meta;
mod mods;
mod optimization;
mod servers;
mod store;
mod syncing;
mod util;

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
#[cfg(feature = "gui")]
use tao::dpi::LogicalSize;
#[cfg(feature = "gui")]
use tao::event::{Event, WindowEvent};
#[cfg(feature = "gui")]
use tao::event_loop::{ControlFlow, EventLoopBuilder};
#[cfg(feature = "gui")]
use tao::window::WindowBuilder;
#[cfg(feature = "gui")]
use wry::WebViewBuilder;

const INDEX_HTML: &str = include_str!("../ui/index.html");

fn query(raw: &str) -> HashMap<String, String> {
    let dec = |s: &str| percent_encoding::percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().into_owned();
    raw.split('&').filter(|p| !p.is_empty()).map(|p| match p.split_once('=') {
        Some((k, v)) => (dec(k), dec(v)),
        None => (dec(p), String::new()),
    }).collect()
}

fn handle(mut req: tiny_http::Request, token: &str) {
    let respond = |req: tiny_http::Request, code: u16, body: Vec<u8>, ctype: &str| {
        let _ = req.respond(
            tiny_http::Response::from_data(body)
                .with_status_code(code)
                .with_header(tiny_http::Header::from_bytes("Content-Type", ctype).unwrap())
                .with_header(tiny_http::Header::from_bytes("Cache-Control", "no-store").unwrap()),
        );
    };
    let err = |req: tiny_http::Request, code: u16, msg: &str| {
        respond(req, code, serde_json::json!({"error": msg}).to_string().into_bytes(), "application/json");
    };

    let host_ok = req.headers().iter().find(|h| h.field.equiv("Host"))
        .map(|h| { let v = h.value.as_str(); let n = v.split(':').next().unwrap_or(""); n == "127.0.0.1" || n == "localhost" })
        .unwrap_or(false);
    if !host_ok { return err(req, 403, "forbidden"); }

    let url = req.url().to_string();
    let (path, qs) = url.split_once('?').unwrap_or((&url, ""));
    let method = req.method().as_str().to_string();
    let q = query(qs);

    if method == "GET" && path == "/" {
        let html = INDEX_HTML.replace("__TOKEN__", token);
        return respond(req, 200, html.into_bytes(), "text/html; charset=utf-8");
    }

    let sent = req.headers().iter().find(|h| h.field.equiv("X-Token")).map(|h| h.value.as_str().to_string());
    let image = method == "GET" && (path.starts_with("/api/screenshots/") || path.starts_with("/api/worlds/")
        || path.ends_with("/icon") || path.ends_with("/content-icon"));   // <img> can't send headers
    if sent.as_deref() != Some(token) && !(image && q.get("t").map(|t| t.as_str()) == Some(token)) {
        return err(req, 403, "forbidden");
    }

    let mut raw = Vec::new();
    if req.as_reader().take(512 * 1024 * 1024).read_to_end(&mut raw).is_err() { return err(req, 400, "bad body"); }
    let is_raw = path == "/api/modpack/upload" || (method == "POST" && path.ends_with("/icon"))
        || (method == "POST" && path.ends_with("/content/upload"));
    let body = if is_raw || raw.is_empty() { serde_json::json!({}) } else {
        match serde_json::from_slice(&raw) { Ok(v) => v, Err(_) => return err(req, 400, "bad JSON") }
    };

    // Closing Orbit: what is still running (for the warning) and the clean shutdown itself.
    if path.starts_with("/api/app/") {
        return match (method.as_str(), path) {
            ("GET", "/api/app/running") => {
                let inst: Vec<String> = launcher::running().as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
                respond(req, 200, serde_json::json!({"instances": inst, "servers": servers::running_names()}).to_string().into_bytes(), "application/json")
            }
            ("POST", "/api/app/quit") => {
                respond(req, 200, b"{\"ok\":true}".to_vec(), "application/json");
                begin_quit();
            }
            _ => err(req, 404, "not found"),
        };
    }

    let task = body["task"].as_str().filter(|t| !t.is_empty()).map(|t| t.chars().take(40).collect::<String>());
    let r = api::Req { method: &method, path, q, body, raw, task };
    // A panic in one handler must not take the whole app down.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| api::route(&r)));
    match result {
        Ok(Ok(api::Out::Json(v))) => respond(req, 200, v.to_string().into_bytes(), "application/json"),
        Ok(Ok(api::Out::Bytes(b, ct))) => respond(req, 200, b, ct),
        Ok(Err(msg)) => match msg.strip_prefix("NOT_PORTED:") {
            Some(m) => err(req, 501, m.trim()),
            None => err(req, 400, &msg),
        },
        Err(_) => err(req, 500, "internal error"),
    }
}

static SHUTTING: AtomicBool = AtomicBool::new(false);

/// Anything Orbit started that is still running: games (or installs) and servers.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn app_busy() -> bool {
    launcher::running().as_object().map(|m| !m.is_empty()).unwrap_or(false) || !servers::running_names().is_empty()
}

/// Clean shutdown, then exit: games get a close request, servers get the in-game warning and a `stop` (all in parallel),
/// and anything that won't go is killed. Safe to call more than once.
fn begin_quit() {
    if SHUTTING.swap(true, SeqCst) { return; }
    std::thread::spawn(|| {
        let a = std::thread::spawn(servers::shutdown_all);
        let b = std::thread::spawn(launcher::stop_all);
        let _ = a.join();
        let _ = b.join();
        std::thread::sleep(std::time::Duration::from_millis(1500));   // lets the launcher record the play time
        log_line("closed after a clean shutdown");
        std::process::exit(0);
    });
}

/// Where startup problems are written (the release build has no console to print to).
fn log_path() -> std::path::PathBuf { store::data_home().join("orbit.log") }

fn log_line(msg: &str) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(store::data_home());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "[{}] {}", store::now() as u64, msg.replace('\n', " "));
    }
}

/// A message the person can actually see. A GUI-subsystem exe that dies silently looks like "nothing happened".
#[cfg(feature = "gui")]
fn fatal(msg: &str) {
    log_line(msg);
    rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("Orbit").set_description(msg)
        .set_buttons(rfd::MessageButtons::Ok).show();
}
#[cfg(not(feature = "gui"))]
fn fatal(msg: &str) { log_line(msg); eprintln!("Orbit: {msg}"); }

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown error".into())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|a| a == "--version" || a == "-V").unwrap_or(false) {      // read by orbit-updater
        println!("orbit {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.get(1).map(|a| a == "--diagnose").unwrap_or(false) {
        match args.get(2) { Some(p) => std::process::exit(api::cli_diagnose(p)), None => { eprintln!("usage: orbit --diagnose <instance-or-server-folder>"); std::process::exit(2); } }
    }
    // Panics in request handlers are caught per request (see `handle`); this only records them.
    std::panic::set_hook(Box::new(|info| log_line(&format!("panic: {info}"))));
    match std::panic::catch_unwind(run) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) if msg.is_empty() => {}          // already explained in a dialog (see `webview_failed`)
        Ok(Err(msg)) => fatal(&msg),
        Err(p) => fatal(&format!("Orbit hit an unexpected error and has to close.\n\n{}\n\nDetails were saved to {}", panic_text(&*p), log_path().display())),
    }
}

fn run() -> Result<(), String> {
    // WebView2 keeps its profile next to the exe by default, which fails in a read-only folder (portable exe on a
    // USB stick, Program Files, ...). Keep it with the rest of Orbit's data instead.
    #[cfg(all(feature = "gui", windows))]
    {
        if std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_none() {
            let d = store::data_home().join("webview");
            if std::fs::create_dir_all(&d).is_ok() { std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &d); }
        }
    }

    store::migrate_java();

    // A missing / unplugged instances folder must not stop Orbit from opening (Settings can fix it).
    let _ = std::fs::create_dir_all(store::inst_dir());
    let token = std::env::var("ORBIT_TOKEN").unwrap_or_else(|_| hex::encode(rand::random::<[u8; 16]>()));

    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| format!("Orbit couldn't open its local port ({e}). A firewall or security program may be blocking it."))?;
    let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
    servers::start_scheduler();
    let t = token.clone();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let t = t.clone();
            std::thread::spawn(move || handle(req, &t));
        }
    });

    run_window(format!("http://127.0.0.1:{port}/"))
}

#[cfg(not(feature = "gui"))]
fn run_window(url: String) -> Result<(), String> {
    // Headless: print the address (the page needs the token in its HTML, which the server adds itself).
    eprintln!("Orbit is serving on {url} (headless build). Press Ctrl+C to stop.");
    loop { std::thread::sleep(std::time::Duration::from_secs(3600)); }
}

/// The page's top bar was written for pywebview and talks to `window.pywebview.api`. This is the same API on top
/// of wry's IPC, so the one UI works unchanged. Windows only: there Orbit draws its own title bar (the window has
/// no native one, but keeps its resize frame, so Aero Snap, Win+Arrow and edge-drag snapping stay native).
/// On Linux / macOS the window keeps its normal title bar and the page leaves the window buttons out.
#[cfg(all(feature = "gui", windows))]
const WINDOW_API: &str = r#"(function(){
  var pend = {}, n = 0;
  function call(m){ return new Promise(function(res){ var id = ++n; pend[id] = res; window.ipc.postMessage(JSON.stringify({id: id, m: m})); }); }
  window.__orbitReply = function(id, v){ var r = pend[id]; if (r) { delete pend[id]; r(v); } };
  window.pywebview = { api: {
    native_resize: function(){ return Promise.resolve(true); },
    minimize: function(){ return call("minimize"); },
    close: function(){ return call("close"); },
    toggle_max: function(){ return call("toggle_max"); },
    is_max: function(){ return call("is_max"); },
    start_move: function(){ return call("start_move"); }
  } };
  window.__orbitNative = true;
  var fire = function(){ window.dispatchEvent(new Event("pywebviewready")); };
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", function(){ setTimeout(fire, 0); });
  else setTimeout(fire, 0);
  window.addEventListener("load", fire);
})();"#;

#[cfg(feature = "gui")]
#[derive(Debug)]
enum Ui { Call(String) }

/// The window was asked to close. Nothing running: close. Otherwise the page shows the warning (`window.__orbitClose`)
/// and later calls /api/app/quit; if the page can't answer, shut down straight away.
#[cfg(feature = "gui")]
fn request_close(webview: &wry::WebView, flow: &mut ControlFlow) {
    if SHUTTING.load(SeqCst) { return; }
    if !app_busy() { *flow = ControlFlow::Exit; return; }
    let js = "(function(){if(window.__orbitClose){window.__orbitClose();return true}return false})()";
    if webview.evaluate_script_with_callback(js, |r| { if r.trim() != "true" { begin_quit(); } }).is_err() { begin_quit(); }
}

#[cfg(feature = "gui")]
fn webview_failed(e: &str) -> String {
    if cfg!(windows) {
        let msg = format!("Orbit needs the Microsoft Edge WebView2 Runtime, and it couldn't start ({e}).\n\nOpen the download page now? Install the \"Evergreen\" runtime, then start Orbit again.");
        let yes = matches!(rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("Orbit").set_description(&msg)
            .set_buttons(rfd::MessageButtons::YesNo).show(), rfd::MessageDialogResult::Yes);
        log_line(&msg);
        if yes { api::open_path("https://developer.microsoft.com/microsoft-edge/webview2/"); }
        String::new()
    } else {
        format!("Orbit couldn't start its web view ({e}).\n\nOn Linux it needs WebKitGTK 4.1 (libwebkit2gtk-4.1 / webkit2gtk-4.1) installed.")
    }
}

#[cfg(feature = "gui")]
fn run_window(url: String) -> Result<(), String> {
    let event_loop = EventLoopBuilder::<Ui>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let wb = WindowBuilder::new()
        .with_title("Orbit")
        .with_inner_size(LogicalSize::new(1440.0, 920.0))
        .with_min_inner_size(LogicalSize::new(1120.0, 720.0));
    #[cfg(windows)]
    let wb = wb.with_decorations(false);
    let window = wb.build(&event_loop).map_err(|e| format!("Orbit couldn't create its window: {e}"))?;

    let builder = WebViewBuilder::new().with_url(url)
        .with_ipc_handler(move |req| { let _ = proxy.send_event(Ui::Call(req.body().clone())); });
    #[cfg(windows)]
    let builder = builder.with_initialization_script(WINDOW_API);
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window).map_err(|e| webview_failed(&e.to_string()))?;
    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window.default_vbox().ok_or("Orbit couldn't set up its window (no GTK container).")?;
        builder.build_gtk(vbox).map_err(|e| webview_failed(&e.to_string()))?
    };

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => request_close(&webview, flow),
            Event::UserEvent(Ui::Call(msg)) => {
                let v: serde_json::Value = serde_json::from_str(&msg).unwrap_or_default();
                let reply = match v["m"].as_str().unwrap_or("") {
                    "minimize" => { window.set_minimized(true); serde_json::Value::Null }
                    "close" => { request_close(&webview, flow); serde_json::Value::Null }
                    "toggle_max" => { let m = !window.is_maximized(); window.set_maximized(m); serde_json::json!(m) }
                    "is_max" => serde_json::json!(window.is_maximized()),
                    // Hands the drag to Windows itself, so the snap assist, edge-snapping and "pull a maximised window down" all work.
                    "start_move" => { let _ = window.drag_window(); serde_json::Value::Null }
                    _ => serde_json::Value::Null,
                };
                if let Some(id) = v["id"].as_i64() {
                    let _ = webview.evaluate_script(&format!("window.__orbitReply({id},{reply})"));
                }
            }
            _ => {}
        }
    })
}
