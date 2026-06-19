// When the `zvec` feature is on, the binary links zvec's native libzvec_c_api.{dylib,so}.
// zvec-rust's build script does not add an rpath, so the binary fails at runtime with
// "Library not loaded: @rpath/libzvec_c_api.dylib". Add an rpath to the executable's own
// directory so shipping the lib NEXT TO the binary (install step) makes it self-contained,
// plus the build's native-lib output dir so it runs straight from `cargo run`/target.
fn main() {
    if std::env::var("CARGO_FEATURE_ZVEC").is_ok() {
        // resolve relative to the binary itself (ship the dylib alongside dmem)
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path");
        // also let it run from target/ during dev: rpath the zvec-sys prebuilt dir if found
        if let Ok(target_dir) = std::env::var("OUT_DIR") {
            // OUT_DIR is .../target/<profile>/build/dm-lite-XXXX/out
            // the native lib lives at .../target/<profile>/build/zvec-sys-YYYY/out/zvec-prebuilt
            if let Some(build_dir) = std::path::Path::new(&target_dir)
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
}
