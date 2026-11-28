//! An empty `advapi32` for Android builds from a Windows host (T10.1, docs/android-build.md).
//!
//! whisper-rs-sys 0.15.0's build script asks for `advapi32` under `cfg!(target_os = "windows")`,
//! which is the host's cfg in a build script, so a cross build from Windows to Android links
//! `-ladvapi32` and lld fails. Nothing of whisper.cpp uses it there: an empty archive (an ar
//! file is its 8-byte magic) satisfies the request and adds no code. Remove this when the pin
//! moves to a whisper-rs-sys that tests the target (`CARGO_CFG_TARGET_OS`).

use std::env;
use std::path::PathBuf;

/// An ar archive with no members.
const EMPTY_AR: &[u8] = b"!<arch>\n";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let host_windows = env::var("HOST").is_ok_and(|h| h.contains("windows"));
    if target_os == "android" && host_windows {
        let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
        let stub = out.join("android-stubs");
        std::fs::create_dir_all(&stub).expect("create the stub folder");
        std::fs::write(stub.join("libadvapi32.a"), EMPTY_AR).expect("write the empty advapi32");
        println!("cargo:rustc-link-search=native={}", stub.display());
    }
}
