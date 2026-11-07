//! Tauri's build step, and (T7.2) the delay-load of `llama.dll`: llama.cpp is needed only
//! for a local summary, so the app starts without its DLLs and the meeting pre-flight checks
//! for them first (`host::prepare_summary`, the CLI's rule) instead of Windows refusing to
//! start the program. The target is read from `CARGO_CFG_*` (the build script runs on the
//! host).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" && env == "msvc" {
        println!("cargo:rustc-link-arg-bin=earshot-desktop=/DELAYLOAD:llama.dll");
        println!("cargo:rustc-link-arg-bin=earshot-desktop=delayimp.lib");
        // Function names for crash logs (T9.1j): a public-symbols program database, the only one
        // that ships. The full one beside it names the build user in the linker's temp paths.
        let release = std::env::var("PROFILE").is_ok_and(|p| p == "release");
        let debug = std::env::var("DEBUG").is_ok_and(|d| d != "false" && d != "0");
        if release && debug {
            let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
            // <profile>\build\<package>\out
            if let Some(profile) = out.ancestors().nth(3) {
                let file = profile.join("earshot_desktop.stripped.pdb");
                println!(
                    "cargo:rustc-link-arg-bin=earshot-desktop=/PDBSTRIPPED:{}",
                    file.display()
                );
            }
        }
    }
    tauri_build::build();
}
