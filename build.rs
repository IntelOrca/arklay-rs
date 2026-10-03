use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=SDL3_DIR");
    println!("cargo:rerun-if-changed=../vendor/sdl3");

    // Prefer an explicit SDL3_DIR, then the workspace vendor directory
    // (<workspace>/vendor/sdl3, one level above this crate). When neither
    // exists, sdl3-sys falls back to the system SDL3.
    let dir = std::env::var_os("SDL3_DIR")
        .map(PathBuf::from)
        .filter(|p| p.join("lib").is_dir())
        .or_else(|| {
            let vendor = PathBuf::from("../vendor/sdl3");
            vendor.join("lib").is_dir().then_some(vendor)
        });

    if let Some(dir) = dir {
        // Absolute paths keep the link search and rpath valid no matter where
        // the binary is run from.
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        let lib = dir.join("lib");
        println!("cargo:rustc-link-search=native={}", lib.display());
        println!("cargo:rustc-link-lib=dylib=SDL3");
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
        }
    }
}
