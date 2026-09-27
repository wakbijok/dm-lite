// When the `zvec` feature is on, the binary links zvec's native libzvec_c_api (a .dylib on
// macOS, .so on Linux, .dll on Windows). zvec-rust's build script does not add an rpath, so
// without help the binary fails at runtime with "Library not loaded". We add an rpath to the
// executable's own directory so shipping the native lib NEXT TO the binary makes it
// self-contained, per-OS:
//   macOS:  @executable_path     Linux: $ORIGIN     Windows: no rpath (the loader searches
// the exe's own directory for DLLs automatically; ship zvec_c_api.dll beside dmem.exe).
fn main() {
    if std::env::var("CARGO_FEATURE_ZVEC").is_err() {
        return;
    }
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let origin = match target_os.as_str() {
        "macos" | "ios" => Some("@executable_path"),
        "windows" => None, // DLLs load from the exe's directory; no rpath concept
        _ => Some("$ORIGIN"), // linux and other ELF unixes
    };
    let Some(origin) = origin else { return };

    // resolve relative to the binary itself (ship the native lib alongside dmem)
    println!("cargo:rustc-link-arg=-Wl,-rpath,{origin}");

    // also let it run straight from `cargo run`/tests in-tree: rpath the zvec-sys prebuilt
    // output dir if found. OUT_DIR is .../target/<profile>/build/dm-lite-XXXX/out; the native
    // lib lives at .../target/<profile>/build/zvec-sys-YYYY/out/zvec-prebuilt.
    if let Ok(out_dir) = std::env::var("OUT_DIR") {
        if let Some(build_dir) = std::path::Path::new(&out_dir)
            .ancestors()
            .find(|p| p.file_name().map(|n| n == "build").unwrap_or(false))
        {
            if let Ok(entries) = std::fs::read_dir(build_dir) {
                for e in entries.flatten() {
                    let p = e.path().join("out").join("zvec-prebuilt");
                    if p.join("libzvec_c_api.dylib").exists() || p.join("libzvec_c_api.so").exists() {
                        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", p.display());
                    }
                }
            }
        }
    }
}
