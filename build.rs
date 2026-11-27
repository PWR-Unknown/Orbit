fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // The UI page is embedded gzipped (about a quarter of its size) and inflated once at startup; see INDEX_HTML in src/main.rs.
    println!("cargo:rerun-if-changed=ui/index.html");
    {
        use std::io::Write;
        let html = std::fs::read("ui/index.html").expect("ui/index.html is missing");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&html).unwrap();
        let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("index.html.gz");
        std::fs::write(out, gz.finish().unwrap()).unwrap();
    }
    println!("cargo:rerun-if-changed=assets/orbit.ico");
    // Embed the icon in the exe. Checks the *target* (not the machine building), and a missing resource compiler
    // (rc.exe / windres) only costs the icon, never the build.
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/orbit.ico");
            if let Err(e) = res.compile() {
                println!("cargo:warning=couldn't embed the app icon ({e}); the exe still works");
            }
        }
    }
}
