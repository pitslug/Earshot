//! `earshot.exe` delay-loads `llama.dll` (T6.21b): llama.cpp is needed only for a local
//! summary, so the exe starts without its DLLs and the summary step checks for them first
//! (`session::missing_llama_dlls`) and says what is missing, instead of Windows refusing to
//! start the program. The target is read from `CARGO_CFG_*` (the build script runs on the
//! host).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" && env == "msvc" {
        println!("cargo:rustc-link-arg-bin=earshot=/DELAYLOAD:llama.dll");
        println!("cargo:rustc-link-arg-bin=earshot=delayimp.lib");
    }
}
