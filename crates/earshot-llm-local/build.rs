//! Puts llama.cpp's DLL directory on the native library search path.
//!
//! llama-cpp-sys-2 builds llama.cpp and its ggml as DLLs (`dynamic-link`, see the crate
//! docs) and installs them in `<its OUT_DIR>/bin`. Cargo adds native search paths inside the
//! target directory to `PATH` for `cargo test` and `cargo run`, so this makes the DLLs
//! loadable there. A shipped binary carries them next to the executable (T6.21).

use std::path::PathBuf;

fn main() {
    // No `rerun-if-env-changed` for DEP_LLAMA_GGML_CMAKE_DIR: cargo sets DEP_* itself and
    // ignores that directive for them. This script runs again whenever llama-cpp-sys-2's
    // build script does (cargo tracks the `links` dependency), and on edits to itself; a
    // rebuilt llama.dll at the same path needs nothing here.
    println!("cargo:rerun-if-changed=build.rs");
    let cmake_dir = std::env::var_os("DEP_LLAMA_GGML_CMAKE_DIR")
        .map(PathBuf::from)
        .expect("llama-cpp-sys-2 did not report its install directory (DEP_LLAMA_GGML_CMAKE_DIR)");
    // <out>/lib/cmake -> <out>/bin
    let bin = cmake_dir
        .parent()
        .and_then(|lib| lib.parent())
        .map(|out| out.join("bin"))
        .expect("DEP_LLAMA_GGML_CMAKE_DIR is <out>/lib/cmake");
    // The target's OS, not the host's (`cfg!` in a build script is the host).
    let windows_target = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    assert!(
        bin.join("llama.dll").is_file() || !windows_target,
        "llama.dll not found in {}",
        bin.display()
    );
    println!("cargo:rustc-link-search=native={}", bin.display());
}
