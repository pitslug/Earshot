//! Optimisation flags of the vendored C/C++ builds (whisper.cpp in whisper-rs-sys, llama.cpp
//! in llama-cpp-sys-2), T2.2c.
//!
//! On MSVC with the Visual Studio generator, the `cmake` crate (0.1.58) replaces
//! `CMAKE_<LANG>_FLAGS_<CMAKE_BUILD_TYPE>` with cc's flags minus every `/O` flag, unless the
//! variable is already defined. whisper-rs-sys sets `CMAKE_BUILD_TYPE=Release` in release
//! builds and always builds the Release configuration, so a release whisper.cpp had no `/O2`
//! (measured: one 30 s encoder window on one thread took about 20 s instead of 1.4 s). Debug
//! builds were not affected: there the crate overrides the RelWithDebInfo flags, while the
//! Release configuration that is built keeps CMake's default `/O2`.
//!
//! `.cargo/config.toml` therefore defines [`FLAG_VARS`] as [`MSVC_RELEASE_FLAGS`] (CMake's
//! MSVC defaults with the dynamic CRT); both sys crates forward every `CMAKE_*` variable.
//! Cargo's `[env]` cannot be scoped to a target, so those values are right only for
//! `*-windows-msvc` with the dynamic CRT (every build today). [`release_flags_env`] gives
//! the override for any other target or for `+crt-static`; `cargo xtask build` applies it.
//! [`check_optimised`] reads what was actually built (the CMake caches) and runs after
//! every release build xtask makes (`cargo xtask build`, `accept m2`, `accept m6`).

use std::path::Path;

use anyhow::{Result, bail};

/// The CMake variables `.cargo/config.toml` defines.
pub const FLAG_VARS: [&str; 2] = ["CMAKE_C_FLAGS_RELEASE", "CMAKE_CXX_FLAGS_RELEASE"];
/// CMake's MSVC Release defaults, dynamic CRT (the `.cargo/config.toml` values).
pub const MSVC_RELEASE_FLAGS: &str = "/MD /O2 /Ob2 /DNDEBUG";
/// The same with the static CRT (`-C target-feature=+crt-static`).
pub const MSVC_STATIC_RELEASE_FLAGS: &str = "/MT /O2 /Ob2 /DNDEBUG";
/// CMake's GCC/Clang Release defaults (non-MSVC targets).
pub const GNU_RELEASE_FLAGS: &str = "-O3 -DNDEBUG";
/// Packages whose CMake builds receive the flags.
pub const CMAKE_PACKAGES: [&str; 2] = ["whisper-rs-sys", "llama-cpp-sys-2"];

/// Whether rustflags (`RUSTFLAGS`, or `CARGO_ENCODED_RUSTFLAGS` with its 0x1f separators)
/// link the static CRT.
#[must_use]
pub fn crt_static(rustflags: &str) -> bool {
    rustflags.contains("+crt-static")
}

/// Environment that replaces the `.cargo/config.toml` flags for a build: empty for MSVC with
/// the dynamic CRT (the config values stand), the static-CRT flags for `+crt-static`, CMake's
/// GNU defaults for any other toolchain.
#[must_use]
pub fn release_flags_env(msvc: bool, static_crt: bool) -> Vec<(&'static str, &'static str)> {
    let flags = match (msvc, static_crt) {
        (true, false) => return Vec::new(),
        (true, true) => MSVC_STATIC_RELEASE_FLAGS,
        (false, _) => GNU_RELEASE_FLAGS,
    };
    FLAG_VARS.iter().map(|v| (*v, flags)).collect()
}

/// Whether `target` (`None`: the host xtask was built for) uses the MSVC toolchain.
#[must_use]
pub fn is_msvc(target: Option<&str>) -> bool {
    target.map_or(cfg!(target_env = "msvc"), |t| t.ends_with("-msvc"))
}

/// The value of `var` in a `CMakeCache.txt` (`NAME:TYPE=value`), empty when absent.
fn cache_value(text: &str, var: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix(var)?.strip_prefix(':'))
        .and_then(|l| l.split_once('='))
        .map(|(_, v)| v.to_owned())
        .unwrap_or_default()
}

/// The flag problems of one CMake cache: for the configuration that is built (Release under
/// a multi-config Visual Studio generator, `CMAKE_BUILD_TYPE` otherwise), each language whose
/// flags have no `/O2`, `-O2` or `-O3`.
#[must_use]
pub fn cache_problems(text: &str) -> Vec<String> {
    let config = if cache_value(text, "CMAKE_GENERATOR").starts_with("Visual Studio") {
        "RELEASE".to_owned()
    } else {
        cache_value(text, "CMAKE_BUILD_TYPE").to_uppercase()
    };
    ["C", "CXX"]
        .iter()
        .filter_map(|lang| {
            let var = format!("CMAKE_{lang}_FLAGS_{config}");
            let flags = cache_value(text, &var);
            let optimised = flags
                .split_whitespace()
                .any(|f| matches!(f, "/O2" | "-O2" | "-O3"));
            (!optimised).then(|| format!("{var} = {flags:?}"))
        })
        .collect()
}

/// Checks every [`CMAKE_PACKAGES`] build under `profile_dir` (`<target dir>[/<triple>]/release`)
/// and returns how many caches were read. `clean` is the `cargo clean` arguments that select
/// this profile (for example `--release`, plus `--target` and `--target-dir` when used).
///
/// # Errors
/// No CMake cache at all, an unreadable one, or one without optimisation; the message names
/// the exact `cargo clean` command (the env vars have no rebuild trigger).
pub fn check_optimised(profile_dir: &Path, clean: &str) -> Result<usize> {
    let mut checked = 0;
    let mut problems = Vec::new();
    let entries = std::fs::read_dir(profile_dir.join("build"))
        .map_err(|e| anyhow::anyhow!("{}: {e}", profile_dir.join("build").display()))?;
    for entry in entries.filter_map(std::result::Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(package) = CMAKE_PACKAGES
            .iter()
            .find(|p| name.strip_prefix(*p).is_some_and(|r| r.starts_with('-')))
        else {
            continue;
        };
        let cache = entry
            .path()
            .join("out")
            .join("build")
            .join("CMakeCache.txt");
        if !cache.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&cache)
            .map_err(|e| anyhow::anyhow!("{}: {e}", cache.display()))?;
        checked += 1;
        for p in cache_problems(&text) {
            problems.push(format!(
                "{}: {p}; run `cargo clean {clean} -p {package}` and build again",
                cache.display()
            ));
        }
    }
    if checked == 0 {
        bail!(
            "no whisper.cpp or llama.cpp CMake build under {}",
            profile_dir.display()
        );
    }
    if !problems.is_empty() {
        bail!(
            "C/C++ built without optimisation (see .cargo/config.toml, xtask native_flags):\n{}",
            problems.join("\n")
        );
    }
    Ok(checked)
}

/// The `cargo clean` arguments for a release build into `target_dir` (when not the default)
/// for `target` (when cross).
#[must_use]
pub fn clean_args(target: Option<&str>, target_dir: Option<&Path>) -> String {
    let mut args = String::from("--release");
    if let Some(t) = target {
        args.push_str(&format!(" --target {t}"));
    }
    if let Some(d) = target_dir {
        args.push_str(&format!(" --target-dir {}", d.display()));
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(generator: &str, build_type: &str, release: &str, other: &str) -> String {
        format!(
            "CMAKE_GENERATOR:INTERNAL={generator}\nCMAKE_BUILD_TYPE:UNINITIALIZED={build_type}\n\
             CMAKE_C_FLAGS_RELEASE:STRING={release}\nCMAKE_CXX_FLAGS_RELEASE:STRING={release}\n\
             CMAKE_C_FLAGS_RELWITHDEBINFO:STRING={other}\n\
             CMAKE_CXX_FLAGS_RELWITHDEBINFO:STRING={other}\n"
        )
    }

    #[test]
    fn native_flags_config_defines_the_msvc_release_flags() {
        // The guard ci runs: deleting or changing the `.cargo/config.toml` lines fails here.
        let text = std::fs::read_to_string(crate::repo_root().join(".cargo").join("config.toml"))
            .expect("read .cargo/config.toml");
        let config: toml::Table = toml::from_str(&text).expect("parse .cargo/config.toml");
        for var in FLAG_VARS {
            assert_eq!(
                config["env"][var].as_str(),
                Some(MSVC_RELEASE_FLAGS),
                "{var} in .cargo/config.toml"
            );
        }
        assert!(
            cache_problems(&cache(
                "Visual Studio 17 2022",
                "Release",
                MSVC_RELEASE_FLAGS,
                ""
            ))
            .is_empty()
        );
    }

    #[test]
    fn native_flags_the_built_configuration_is_checked() {
        // The T2.2c failure: Visual Studio, Release, flags without /O.
        let bad = cache(
            "Visual Studio 17 2022",
            "Release",
            " -nologo -MD -Brepro -W0",
            "",
        );
        let p = cache_problems(&bad);
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p[0].starts_with("CMAKE_C_FLAGS_RELEASE"), "{p:?}");
        // Debug profile: Visual Studio still builds Release, whatever CMAKE_BUILD_TYPE says.
        let debug = cache(
            "Visual Studio 17 2022",
            "RelWithDebInfo",
            "/MD /O2 /Ob2 /DNDEBUG",
            "-W0",
        );
        assert!(cache_problems(&debug).is_empty());
        // Ninja (ARM64) builds CMAKE_BUILD_TYPE.
        let ninja = cache("Ninja", "RelWithDebInfo", "/O2", "-W0");
        assert_eq!(cache_problems(&ninja).len(), 2);
        assert!(cache_problems(&cache("Ninja", "Release", "-O3 -DNDEBUG", "")).is_empty());
    }

    #[test]
    fn native_flags_check_reads_the_profile_and_names_the_clean_command() {
        let root =
            std::env::temp_dir().join(format!("earshot-native-flags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let put = |dir: &str, text: &str| {
            let d = root.join("build").join(dir).join("out").join("build");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("CMakeCache.txt"), text).unwrap();
        };
        std::fs::create_dir_all(root.join("build")).unwrap();
        assert!(
            check_optimised(&root, "--release").is_err(),
            "no cache is an error"
        );
        put(
            "whisper-rs-sys-1",
            &cache("Visual Studio 17 2022", "Release", MSVC_RELEASE_FLAGS, ""),
        );
        put(
            "unrelated-sys-1",
            &cache("Visual Studio 17 2022", "Release", "-W0", ""),
        );
        assert_eq!(check_optimised(&root, "--release").unwrap(), 1);
        put(
            "llama-cpp-sys-2-9",
            &cache("Visual Studio 17 2022", "Release", "-W0", ""),
        );
        let err = check_optimised(&root, &clean_args(Some("x86_64-pc-windows-msvc"), None))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "`cargo clean --release --target x86_64-pc-windows-msvc -p llama-cpp-sys-2`"
            ),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn native_flags_are_overridden_off_msvc_and_for_the_static_crt() {
        assert!(release_flags_env(true, false).is_empty());
        assert_eq!(
            release_flags_env(true, true),
            FLAG_VARS.map(|v| (v, MSVC_STATIC_RELEASE_FLAGS))
        );
        assert_eq!(
            release_flags_env(false, false),
            FLAG_VARS.map(|v| (v, GNU_RELEASE_FLAGS))
        );
        assert!(crt_static("-C\u{1f}target-feature=+crt-static"));
        assert!(!crt_static("-C target-cpu=native"));
        assert!(is_msvc(Some("aarch64-pc-windows-msvc")));
        assert!(!is_msvc(Some("aarch64-linux-android")));
    }
}
