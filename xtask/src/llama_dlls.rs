//! llama.cpp's DLLs next to a built `earshot.exe` (T6.21b).
//!
//! `earshot-llm-local` links llama.cpp and its ggml as DLLs (`dynamic-link`; whisper.cpp's
//! ggml stays static, AGENTS.md), and `earshot.exe` delay-loads `llama.dll` for a local
//! summary. Under `cargo test` / `cargo run` cargo puts llama-cpp-sys-2's `<OUT_DIR>/bin` on
//! PATH; an exe that xtask builds and spawns directly, or that is copied elsewhere, needs
//! them beside it. [`install_beside`] copies `llama.dll` and llama.cpp's own `ggml*.dll`
//! from that directory, found from cargo's `--message-format=json` build messages, next to
//! the built exe. Nothing else is copied: whisper.cpp builds no DLL, and only llama.cpp's
//! build output is read, so its ggml and whisper's never mix.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

/// The DLLs every llama.cpp build has (a GPU build adds its backend's `ggml-<gpu>.dll`).
pub const REQUIRED: &[&str] = &["llama.dll", "ggml.dll", "ggml-base.dll", "ggml-cpu.dll"];

/// llama-cpp-sys-2's `<out_dir>/bin` directories named in cargo's JSON build messages, in
/// order, without repeats.
#[must_use]
pub fn bin_dirs(messages: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for m in messages
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        let is_llama = m["reason"] == "build-script-executed"
            && m["package_id"]
                .as_str()
                .is_some_and(|p| p.contains("llama-cpp-sys-2"));
        if let (true, Some(out)) = (is_llama, m["out_dir"].as_str()) {
            let dir = PathBuf::from(out).join("bin");
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// Whether `name` is one of llama.cpp's DLLs (`llama.dll`, `ggml*.dll`; ASCII case is
/// ignored, as Windows does).
#[must_use]
pub fn is_llama_dll(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "llama.dll" || (n.starts_with("ggml") && n.ends_with(".dll"))
}

/// llama.cpp's DLLs in `dir`, sorted by name.
///
/// # Errors
/// An unreadable directory, or one without every [`REQUIRED`] DLL.
pub fn dlls_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if e.file_type()?.is_file() && is_llama_dll(&name) {
            found.push(e.path());
        }
    }
    found.sort();
    for want in REQUIRED {
        ensure!(
            found
                .iter()
                .any(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(*want))),
            "{} has no {want} (llama-cpp-sys-2's build output)",
            dir.display()
        );
    }
    Ok(found)
}

/// Copies llama.cpp's DLLs of the build in `messages` into the directory of `exe`; a DLL
/// already there with the same bytes is left alone (it may be loaded by a running exe).
/// Returns the file names.
///
/// # Errors
/// No llama-cpp-sys-2 build in `messages`, more than one (two llama.cpp builds cannot both
/// be beside one exe), a build without its DLLs, or a failed copy.
pub fn install_beside(messages: &str, exe: &Path) -> Result<Vec<String>> {
    let dirs = bin_dirs(messages);
    let src = match dirs.as_slice() {
        [one] => one,
        [] => bail!("cargo reported no llama-cpp-sys-2 build output (no llama.cpp DLLs to copy)"),
        many => bail!("cargo reported several llama-cpp-sys-2 builds: {many:?}"),
    };
    let dest = exe
        .parent()
        .with_context(|| format!("{} has no directory", exe.display()))?;
    let mut names = Vec::new();
    for dll in dlls_in(src)? {
        let name = dll
            .file_name()
            .context("a DLL without a name")?
            .to_os_string();
        let to = dest.join(&name);
        if !same_bytes(&dll, &to)? {
            std::fs::copy(&dll, &to)
                .with_context(|| format!("copy {} to {}", dll.display(), to.display()))?;
        }
        names.push(name.to_string_lossy().into_owned());
    }
    Ok(names)
}

/// Whether `to` is a file with the same bytes as `from`: lengths first, then SHA-256 read
/// in blocks (`ggml-cuda.dll` is about 56 MB, so neither file is read whole into memory).
pub(crate) fn same_bytes(from: &Path, to: &Path) -> Result<bool> {
    if !to.is_file() {
        return Ok(false);
    }
    let len = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.len())
            .with_context(|| format!("cannot read {}", p.display()))
    };
    if len(from)? != len(to)? {
        return Ok(false);
    }
    Ok(crate::download::sha256_file(from)? == crate::download::sha256_file(to)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_llama_dlls_compare_by_length_then_content() {
        let root = tmp("same");
        let (a, b) = (root.join("a.dll"), root.join("b.dll"));
        std::fs::write(&a, "abcd").unwrap();
        assert!(!same_bytes(&a, &b).unwrap(), "missing destination");
        std::fs::write(&b, "abc").unwrap();
        assert!(!same_bytes(&a, &b).unwrap(), "other length");
        std::fs::write(&b, "abce").unwrap();
        assert!(!same_bytes(&a, &b).unwrap(), "same length, other bytes");
        std::fs::write(&b, "abcd").unwrap();
        assert!(same_bytes(&a, &b).unwrap(), "same bytes");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "earshot-xtask-llama-dlls-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn message(out_dir: &Path) -> String {
        serde_json::json!({
            "reason": "build-script-executed",
            "package_id": "registry+https://github.com/rust-lang/crates.io-index#llama-cpp-sys-2@0.1.160",
            "out_dir": out_dir,
        })
        .to_string()
    }

    #[test]
    fn build_llama_dlls_are_found_from_the_build_messages() {
        let a = Path::new("C:/t/build/llama-cpp-sys-2-abc/out");
        let whisper = serde_json::json!({
            "reason": "build-script-executed",
            "package_id": "registry+https://github.com/rust-lang/crates.io-index#whisper-rs-sys@0.15.0",
            "out_dir": "C:/t/build/whisper-rs-sys-1/out",
        })
        .to_string();
        let msgs = format!("{whisper}\nnot json\n{}\n{}\n", message(a), message(a));
        assert_eq!(bin_dirs(&msgs), [a.join("bin")], "repeats count once");
        assert!(bin_dirs(&whisper).is_empty(), "never whisper's build");
        for yes in [
            "llama.dll",
            "LLAMA.DLL",
            "ggml.dll",
            "ggml-base.dll",
            "ggml-cuda.dll",
        ] {
            assert!(is_llama_dll(yes), "{yes}");
        }
        for no in [
            "llama.lib",
            "ggml.lib",
            "llama.pdb",
            "whisper.dll",
            "onnxruntime.dll",
            "xggml.dll",
        ] {
            assert!(!is_llama_dll(no), "{no}");
        }
    }

    #[test]
    fn build_llama_dlls_are_copied_beside_the_exe() {
        let root = tmp("copy");
        let out = root.join("build").join("llama-cpp-sys-2-abc").join("out");
        let bin = out.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for n in [
            "llama.dll",
            "ggml.dll",
            "ggml-base.dll",
            "ggml-cpu.dll",
            "ggml-cuda.dll",
        ] {
            std::fs::write(bin.join(n), n).unwrap();
        }
        // Not llama.cpp's runtime files: never copied.
        std::fs::write(bin.join("llama.lib"), "lib").unwrap();
        std::fs::write(bin.join("notes.txt"), "x").unwrap();
        let release = root.join("release");
        std::fs::create_dir_all(&release).unwrap();
        let exe = release.join("earshot.exe");
        std::fs::write(&exe, "exe").unwrap();

        let msgs = message(&out);
        let names = install_beside(&msgs, &exe).unwrap();
        assert_eq!(
            names,
            [
                "ggml-base.dll",
                "ggml-cpu.dll",
                "ggml-cuda.dll",
                "ggml.dll",
                "llama.dll"
            ]
        );
        for n in &names {
            assert_eq!(std::fs::read_to_string(release.join(n)).unwrap(), *n);
        }
        assert!(!release.join("llama.lib").exists());
        assert!(!release.join("notes.txt").exists());

        // A rebuild replaces a changed DLL; an identical one is left as it is.
        std::fs::write(bin.join("llama.dll"), "llama v2").unwrap();
        install_beside(&msgs, &exe).unwrap();
        assert_eq!(
            std::fs::read_to_string(release.join("llama.dll")).unwrap(),
            "llama v2"
        );
        // Same length, other bytes: replaced too.
        std::fs::write(bin.join("llama.dll"), "llama v3").unwrap();
        install_beside(&msgs, &exe).unwrap();
        assert_eq!(
            std::fs::read_to_string(release.join("llama.dll")).unwrap(),
            "llama v3"
        );

        // No llama.cpp build, two of them, or a build without its DLLs: errors.
        assert!(install_beside("", &exe).is_err());
        let other = root.join("build").join("llama-cpp-sys-2-def").join("out");
        assert!(install_beside(&format!("{}\n{}", msgs, message(&other)), &exe).is_err());
        std::fs::remove_file(bin.join("ggml-base.dll")).unwrap();
        let e = install_beside(&msgs, &exe).unwrap_err();
        assert!(format!("{e}").contains("ggml-base.dll"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
