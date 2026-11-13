// The Windows GNU toolchain's minimal install lacks the import library for shlwapi.dll (used to open
// web links). winlibs/libshlwapi.a was generated from winlibs/shlwapi.def with Rust's bundled lld-link.
// Other targets (Linux, macOS, Windows MSVC) don't use it.
fn main() {
    println!("cargo:rerun-if-changed=winlibs");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.ends_with("windows-gnu") {
        let dir = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("winlibs");
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
}
