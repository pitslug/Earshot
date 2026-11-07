//! `cargo xtask build --bundle` (T9.1c, T9.1p): the Windows installer, unsigned. One per-user NSIS
//! installer for each architecture, with no MSI and no "everyone" option, so an
//! install and an update never need admin. `cargo xtask bundle-check` installs it and judges it.
//!
//! 1. **The app.** [`crate::build::app`], the same build as `accept m7`: front end first, the
//!    target's environment, a short target directory, llama.cpp's DLLs beside the exe.
//! 2. **The tools.** Tauri's bundler would download NSIS and its plugin itself. They come
//!    from [`TOOLS_MANIFEST`] instead (size and SHA-256 checked) and are placed in
//!    `<target-dir>\.tauri`, where `bundle.useLocalToolsDir` makes the bundler look. That folder
//!    is visible to every shell, which `%LOCALAPPDATA%` is not from an agent shell (MSIX
//!    redirection, AGENTS.md).
//! 3. **The bundler.** `tauri bundle` from the pinned CLI, through pnpm, over that build. It runs
//!    with its proxy pointed at a closed loopback port ([`no_network_env`]), so anything it
//!    still wanted to fetch fails rather than arrives, and any sign in its output that it tried
//!    fails the run ([`bundler_downloaded`]). `--no-binary-patching` keeps the exe byte for byte
//!    what the build made. Its settings are an overlay written by [`bundle_config`], so a
//!    plain `tauri build` stays off and `tauri.conf.json` keeps no copy of a build's paths.
//!
//! What an install holds: the exe (the benchmark meeting is compiled into it), llama.cpp's DLLs,
//! the exe's public symbols for crash logs ([`APP_SYMBOLS`]) and the meeting's attribution. Nothing
//! in it names the build user ([`paths`]). No model, no ONNX Runtime (it lives in the models folder), no
//! CUDA or QNN library ([`refused_in_install`]), no updater. WebView2 mode is `skip`: Tauri's
//! other modes fetch from Microsoft at install or bundle time, a destination PLAN 3.9 does not
//! list, so a machine without WebView2 gets no window (Windows 11 ships it).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;

use crate::accept::m7::{self, X64_WINDOWS};
use crate::build::{self, ARM64_WINDOWS};
use crate::download::sha256_file;
use crate::llama_dlls;
use crate::manifest::{self, Manifest};
use crate::proc::run_with_timeout;

pub mod paths;
pub mod registry;
use registry::{Reg, RegistryGuard};

/// The `@tauri-apps/cli` the pins below belong to (tauri-bundler 2.10.3 inside it).
pub const CLI_VERSION: &str = "2.12.3";
/// The tools' manifest, relative to the repo root.
pub const TOOLS_MANIFEST: &str = "xtask/bundler-tools.toml";
/// tauri-bundler's `NSIS_REQUIRED_FILES`, plus the `NSIS.exe` it hashes: with any missing it
/// deletes the folder and downloads.
pub const NSIS_REQUIRED: &[&str] = &[
    "NSIS.exe",
    "makensis.exe",
    "Bin/makensis.exe",
    "Stubs/lzma-x86-unicode",
    "Stubs/lzma_solid-x86-unicode",
    "Plugins/x86-unicode/additional/nsis_tauri_utils.dll",
    "Include/MUI2.nsh",
    "Include/FileFunc.nsh",
    "Include/x64.nsh",
    "Include/nsDialogs.nsh",
    "Include/WinMessages.nsh",
    "Include/Win/COM.nsh",
    "Include/Win/Propkey.nsh",
    "Include/Win/RestartManager.nsh",
];
/// Proxy exemptions removed from the bundler's environment.
pub const NO_PROXY_VARS: &[&str] = &["NO_PROXY", "no_proxy"];
/// A closed loopback port (discard): the bundler's HTTP client honours these variables.
const NO_ROUTE: &str = "http://127.0.0.1:9";
/// The attribution of the benchmark meeting, relative to the repo root.
const MEETING_NOTICE: &str = "apps/desktop/src-tauri/resources/benchmark/NOTICE.md";
/// Where that attribution sits in an install.
const MEETING_NOTICE_IN_INSTALL: &str = "resources/benchmark/NOTICE.md";
/// The app's executable.
const APP_EXE: &str = "earshot.exe";
/// The app's public-symbols program database as the link makes it (the app's `build.rs`).
pub const APP_SYMBOLS_BUILT: &str = "earshot.stripped.pdb";
/// Its name in an install: the one the exe's debug record names, beside the exe, where a crash
/// log's backtrace finds it. It holds function names only; the full database never ships.
pub const APP_SYMBOLS: &str = "earshot.pdb";
/// Deadline for the bundler.
const BUNDLER_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Deadline for a silent install or an administrative extract.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Set as the `Run` value while an uninstaller runs: its deletion means the uninstaller has done
/// its last registry step ([`nsis_uninstall`]). Not a real command, so a logon that finds one left
/// by a killed run fails to start it and nothing else.
const RUN_SENTINEL: &str = "earshot-bundle-check-sentinel";
/// How long a silent uninstall may take: the uninstaller re-runs itself from a temp copy and
/// returns before it has finished.
const UNINSTALL_WAIT: Duration = Duration::from_secs(90);

/// The installers made for `target`.
///
/// # Errors
/// A target that gets no installer.
pub fn formats(target: &str) -> Result<&'static [&'static str]> {
    match target {
        X64_WINDOWS | ARM64_WINDOWS => Ok(&["nsis"]),
        other => bail!("no installers for {other}: only {X64_WINDOWS} and {ARM64_WINDOWS}"),
    }
}

/// `target`, or the host's Windows triple.
#[must_use]
pub fn effective_target(target: Option<&str>) -> &str {
    target.unwrap_or(if cfg!(target_arch = "aarch64") {
        ARM64_WINDOWS
    } else {
        X64_WINDOWS
    })
}

/// The installer's path below `<profile>\bundle`. The bundler names it from the version, so a
/// version from anywhere but the workspace fails the check that the file exists.
///
/// # Errors
/// An unknown target.
pub fn installer_path(version: &str, target: &str) -> Result<PathBuf> {
    let arch = match target {
        X64_WINDOWS => "x64",
        ARM64_WINDOWS => "arm64",
        other => bail!("no installer architecture for {other}"),
    };
    Ok(Path::new("nsis").join(format!("Earshot_{version}_{arch}-setup.exe")))
}

/// The workspace version: the one source (`version.workspace = true` in the app, no `version` in
/// `tauri.conf.json`, so Tauri's CLI reads it from there).
///
/// # Errors
/// An unreadable `Cargo.toml`, or one with no `[workspace.package]` version.
pub fn workspace_version(root: &Path) -> Result<String> {
    let path = root.join("Cargo.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let cargo: toml::Table = text
        .parse()
        .with_context(|| format!("parse {}", path.display()))?;
    cargo
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .context("Cargo.toml has no [workspace.package] version")
}

/// The Vulkan loader. The exe and `ggml-vulkan.dll` import it, so without it the app would not
/// start on a PC whose drivers do not provide one; with it and no driver, Vulkan finds no device
/// and summaries run on the CPU (T8.4ac).
pub const VULKAN_LOADER: &str = "vulkan-1.dll";
/// Its licence, which ships beside it.
pub const VULKAN_LICENCE: &str = "VulkanRT-License.txt";
/// The two below `<target-dir>\.tauri`, as `xtask/bundler-tools.toml` extracts them.
const VULKAN_RUNTIME_DIR: &str = "downloads/vulkan-runtime/VulkanRT-X64-1.4.363.0-Components";

fn has_vulkan_summaries(features: &[String]) -> bool {
    features.iter().any(|f| f == build::SHIPPED_X64_FEATURE)
}

/// The DLLs an install holds beside the exe for `features`.
#[must_use]
pub fn resource_dlls(features: &[String]) -> Vec<&'static str> {
    let mut dlls = llama_dlls::REQUIRED.to_vec();
    if features.iter().any(|f| f == "cuda") {
        dlls.push("ggml-cuda.dll");
    }
    if has_vulkan_summaries(features) {
        dlls.extend(["ggml-vulkan.dll", VULKAN_LOADER]);
    }
    dlls
}

/// Whether `name` is NVIDIA's or Qualcomm's library, or ONNX Runtime's provider for theirs: none
/// of these is ever in an install, nor a static import of what is (D23, T8.4a, T8.4).
#[must_use]
pub fn is_vendor_accelerator_library(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "cublas",
        "cudnn",
        "cudart",
        "nvcuda",
        "nvrtc",
        "onnxruntime_providers_cuda",
        "onnxruntime_providers_qnn",
        "qnn",
        "libqnn",
    ]
    .iter()
    .any(|p| n.starts_with(p))
}

/// Whether a file named `name` must not be in an install built with `features`: a vendor library,
/// or llama.cpp's CUDA backend outside a CUDA build (a stale one would need NVIDIA's libraries).
#[must_use]
pub fn refused_in_install(name: &str, features: &[String]) -> bool {
    is_vendor_accelerator_library(name)
        || (name.eq_ignore_ascii_case("ggml-cuda.dll") && !features.iter().any(|f| f == "cuda"))
}

/// The settings laid over `tauri.conf.json` for one bundle run.
#[must_use]
pub fn bundle_config(
    exe_dir: &Path,
    notice: &Path,
    formats: &[&str],
    features: &[String],
) -> serde_json::Value {
    let mut resources = serde_json::Map::new();
    for dll in resource_dlls(features) {
        resources.insert(exe_dir.join(dll).display().to_string(), json!(dll));
    }
    if has_vulkan_summaries(features) {
        resources.insert(
            exe_dir.join(VULKAN_LICENCE).display().to_string(),
            json!(VULKAN_LICENCE),
        );
    }
    resources.insert(
        exe_dir.join(APP_SYMBOLS_BUILT).display().to_string(),
        json!(APP_SYMBOLS),
    );
    resources.insert(
        notice.display().to_string(),
        json!(MEETING_NOTICE_IN_INSTALL),
    );
    json!({
        "bundle": {
            "active": true,
            "targets": formats,
            "useLocalToolsDir": true,
            "createUpdaterArtifacts": false,
            "resources": resources,
            "windows": {
                "webviewInstallMode": { "type": "skip" },
                "nsis": { "installMode": "currentUser" },
            },
        }
    })
}

/// pnpm's arguments for the bundler. `cross` is the `--target` of the build, `None` for a build
/// for the host, whose exe is not under a triple's folder.
#[must_use]
pub fn cli_args(cross: Option<&str>, formats: &[&str], config: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "--filter",
        "@earshot/desktop",
        "exec",
        "tauri",
        "bundle",
        "--bundles",
    ]
    .map(Into::into)
    .to_vec();
    args.push(formats.join(","));
    args.extend(["--config".into(), config.display().to_string()]);
    args.extend(["--no-binary-patching".into(), "--no-sign".into()]);
    if let Some(t) = cross {
        args.extend(["--target".into(), t.to_owned()]);
    }
    args
}

/// The bundler's environment that leaves its HTTP client nowhere to go.
#[must_use]
pub fn no_network_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("ALL_PROXY", NO_ROUTE),
        ("HTTPS_PROXY", NO_ROUTE),
        ("HTTP_PROXY", NO_ROUTE),
    ]
}

/// Whether `name` is a setting of tauri-bundler's own: `TAURI_BUNDLER_TOOLS_GITHUB_MIRROR` and
/// `..._TEMPLATE` give its download agent another route with no proxy, so none of these reaches it.
#[must_use]
pub fn is_bundler_override(name: &str) -> bool {
    name.to_ascii_uppercase().starts_with("TAURI_BUNDLER_")
}

/// Whether the bundler's output shows it looked for a tool it did not find (tauri-bundler's own
/// log lines before and during a download).
#[must_use]
pub fn bundler_downloaded(output: &str) -> bool {
    [
        "Downloading",
        "Redownloading",
        "Verifying NSIS package",
        "Verifying wix package",
        "Recreating it",
    ]
    .iter()
    .any(|sign| output.contains(sign))
}

/// What the bundler checks for in `tools` and does not find.
#[must_use]
pub fn missing_tools(tools: &Path) -> Vec<String> {
    NSIS_REQUIRED
        .iter()
        .filter(|f| !tools.join("NSIS").join(f).is_file())
        .map(|f| format!("NSIS/{f}"))
        .collect()
}

/// Every file below `dir`, sorted. A missing folder has none.
///
/// # Errors
/// An unreadable folder.
pub fn files_under(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(d) = pending.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {}", d.display())),
        };
        for e in entries {
            let e = e?;
            if e.file_type()?.is_dir() {
                pending.push(e.path());
            } else {
                files.push(e.path());
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Makes `dst` hold exactly the files of `src` plus each `(file, path below dst)` of `extra`: a
/// changed file is replaced, and one that came from elsewhere goes, so the bundler never reads
/// anything the manifest did not pin.
///
/// # Errors
/// A file that cannot be read, copied or removed.
pub fn sync_tree(src: &Path, dst: &Path, extra: &[(PathBuf, PathBuf)]) -> Result<()> {
    let mut wanted = std::collections::BTreeMap::new();
    for file in files_under(src)? {
        let rel = file.strip_prefix(src)?.to_path_buf();
        wanted.insert(dst.join(rel), file);
    }
    for (file, rel) in extra {
        wanted.insert(dst.join(rel), file.clone());
    }
    for (to, from) in &wanted {
        if !llama_dlls::same_bytes(from, to)? {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            std::fs::copy(from, to)
                .with_context(|| format!("copy {} to {}", from.display(), to.display()))?;
        }
    }
    for file in files_under(dst)? {
        if !wanted.contains_key(&file) {
            std::fs::remove_file(&file).with_context(|| format!("remove {}", file.display()))?;
        }
    }
    remove_empty_folders(dst)
}

/// Removes the folders below `dir` that hold no file, deepest first.
fn remove_empty_folders(dir: &Path) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let e = e?;
        if e.file_type()?.is_dir() {
            remove_empty_folders(&e.path())?;
            if std::fs::read_dir(e.path())?.next().is_none() {
                std::fs::remove_dir(e.path())?;
            }
        }
    }
    Ok(())
}

/// Fetches the tools into `tools` (every byte re-checked against the manifest) and places them
/// where the bundler looks.
///
/// # Errors
/// A failed download or check, or a tool the bundler would still go looking for.
pub fn place_tools(root: &Path, tools: &Path) -> Result<()> {
    let m = Manifest::load(&root.join(TOOLS_MANIFEST))?;
    let arts = m.select(&["bundler".to_owned()])?;
    let reports = manifest::fetch_all(&arts, root, tools, true, &m.policy())?;
    manifest::print_reports("bundle tools", tools, &reports);
    sync_tree(
        &tools.join("downloads").join("nsis-3.13").join("nsis-3.13"),
        &tools.join("NSIS"),
        &[(
            tools.join("downloads").join("nsis_tauri_utils.dll"),
            Path::new("Plugins")
                .join("x86-unicode")
                .join("additional")
                .join("nsis_tauri_utils.dll"),
        )],
    )?;
    let missing = missing_tools(tools);
    ensure!(
        missing.is_empty(),
        "the pinned tools lack what the bundler checks for: {missing:?} (a CLI upgrade needs \
         {TOOLS_MANIFEST} updated first)"
    );
    Ok(())
}

/// The parts of a PE file the bundle checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pe {
    /// COFF machine.
    pub machine: u16,
    /// DLLs named in the import and delay-import tables.
    pub imports: Vec<String>,
}

/// The COFF machine of `target`.
///
/// # Errors
/// An unknown target.
pub fn machine_of(target: &str) -> Result<u16> {
    match target {
        X64_WINDOWS => Ok(0x8664),
        ARM64_WINDOWS => Ok(0xaa64),
        other => bail!("no machine type for {other}"),
    }
}

/// Reads the machine and imports of the PE file at `path`.
///
/// # Errors
/// An unreadable file, or one that is not a well-formed PE.
pub fn read_pe(path: &Path) -> Result<Pe> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    parse_pe(&bytes).with_context(|| format!("{} is not a usable PE file", path.display()))
}

fn parse_pe(d: &[u8]) -> Result<Pe> {
    let le16 = |o: usize| -> Result<u16> {
        let b = o
            .checked_add(2)
            .and_then(|e| d.get(o..e))
            .context("cut off")?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    };
    let le32 = |o: usize| -> Result<usize> {
        let b = o
            .checked_add(4)
            .and_then(|e| d.get(o..e))
            .context("cut off")?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    ensure!(d.starts_with(b"MZ"), "no MZ header");
    let pe = le32(0x3c)?;
    ensure!(
        pe.checked_add(4).and_then(|e| d.get(pe..e)) == Some(&b"PE\0\0"[..]),
        "no PE header"
    );
    let machine = le16(pe + 4)?;
    let sections = usize::from(le16(pe + 6)?);
    let optional = pe + 24;
    let directories = optional
        + match le16(optional)? {
            0x20b => 112,
            0x10b => 96,
            other => bail!("unknown optional header {other:#x}"),
        };
    let table = optional + usize::from(le16(pe + 20)?);
    let to_offset = |rva: usize| -> Result<usize> {
        for s in 0..sections {
            let at = table + 40 * s;
            let (virtual_size, start) = (le32(at + 8)?, le32(at + 12)?);
            let (raw_size, raw) = (le32(at + 16)?, le32(at + 20)?);
            if rva >= start && rva - start < virtual_size.max(raw_size) {
                return Ok(rva - start + raw);
            }
        }
        bail!("address {rva:#x} is in no section")
    };
    let mut imports = Vec::new();
    // Import directory 1: 20-byte entries, name at +12. Delay-import directory 13: 32-byte
    // entries, name at +4 (PE format, "Optional Header Data Directories").
    for (index, entry, name_at) in [(1, 20, 12), (13, 32, 4)] {
        let rva = le32(directories + 8 * index)?;
        if rva == 0 {
            continue;
        }
        let mut at = to_offset(rva)?;
        loop {
            let name = le32(at + name_at)?;
            if name == 0 {
                break;
            }
            let start = to_offset(name)?;
            let rest = d.get(start..).context("cut off")?;
            let len = rest.iter().position(|b| *b == 0).context("no name end")?;
            imports.push(String::from_utf8_lossy(&rest[..len]).into_owned());
            at += entry;
        }
    }
    Ok(Pe { machine, imports })
}

/// Refuses a binary built for another machine, or one that would not start without a library an
/// install never holds.
///
/// # Errors
/// What is wrong with it, by `name`.
pub fn check_binary(name: &str, pe: &Pe, target: &str, features: &[String]) -> Result<()> {
    ensure!(
        pe.machine == machine_of(target)?,
        "{name} is for machine {:#x}, not {target}",
        pe.machine
    );
    let refused: Vec<&String> = pe
        .imports
        .iter()
        .filter(|i| refused_in_install(i, features))
        .collect();
    ensure!(
        refused.is_empty(),
        "{name} imports {refused:?}, which no install holds (a CUDA build's install is T8.4a's)"
    );
    // Without the loader such an exe does not start where drivers provide none (T8.4ac).
    ensure!(
        !pe.imports
            .iter()
            .any(|i| i.eq_ignore_ascii_case(VULKAN_LOADER))
            || resource_dlls(features).contains(&VULKAN_LOADER),
        "{name} imports {VULKAN_LOADER}, which an install built with {features:?} does not hold"
    );
    Ok(())
}

/// The silent NSIS command line. `/D=` must come last and unquoted (NSIS reads to the end of the
/// line); `/NS` leaves the user's Desktop and Start menu alone.
#[must_use]
pub fn nsis_install_args(dir: &Path) -> String {
    format!("/S /NS /D={}", dir.display())
}

/// Runs the bundle: returns the installers made.
///
/// # Errors
/// A failed build, tool fetch, binary check or bundler run, or an installer that is not there.
pub fn run(
    root: &Path,
    target: Option<&str>,
    features: &[String],
    target_dir: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let triple = effective_target(target).to_owned();
    let wanted = formats(&triple)?;
    let version = workspace_version(root)?;
    let lockfile = ["install", "--frozen-lockfile"];
    let out = run_with_timeout(
        &mut crate::ui::pnpm_command(root, &lockfile),
        BUNDLER_TIMEOUT,
    )
    .context("pnpm install did not finish within the deadline")?;
    crate::ui::evaluate(&lockfile, &out)?;

    let features = &build::shipped_features(target, features);
    let exe = build::app(root, target, features, target_dir)?;
    let exe_dir = exe
        .parent()
        .with_context(|| format!("{} has no folder", exe.display()))?;
    let dir = build::short_target_dir(root, target_dir, std::env::var_os("SystemDrive"), target);
    place_tools(root, &dir.join(".tauri"))?;
    if has_vulkan_summaries(features) {
        let runtime = dir.join(".tauri").join(VULKAN_RUNTIME_DIR);
        for (from, name) in [
            (runtime.join("x64").join(VULKAN_LOADER), VULKAN_LOADER),
            (runtime.join(VULKAN_LICENCE), VULKAN_LICENCE),
        ] {
            let to = exe_dir.join(name);
            std::fs::copy(&from, &to)
                .with_context(|| format!("copy {} to {}", from.display(), to.display()))?;
        }
    }
    ensure!(
        exe_dir.join(APP_SYMBOLS_BUILT).is_file(),
        "the build left no {APP_SYMBOLS_BUILT} beside {APP_EXE}"
    );
    check_binary(APP_EXE, &read_pe(&exe)?, &triple, features)?;
    for dll in resource_dlls(features) {
        let path = exe_dir.join(dll);
        check_binary(dll, &read_pe(&path)?, &triple, features)?;
    }
    let config = dir.join(format!("earshot-bundle-{triple}.json"));
    let overlay = bundle_config(exe_dir, &root.join(MEETING_NOTICE), wanted, features);
    std::fs::write(&config, serde_json::to_string_pretty(&overlay)?)
        .with_context(|| format!("write {}", config.display()))?;

    // A run that makes nothing cannot pass on an installer left by an earlier one.
    let installer = exe_dir
        .join("bundle")
        .join(installer_path(&version, &triple)?);
    if installer.exists() {
        std::fs::remove_file(&installer)
            .with_context(|| format!("remove {}", installer.display()))?;
    }
    let installers = vec![installer];

    let args = cli_args(target, wanted, &config);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut cmd = crate::ui::pnpm_command(root, &refs);
    for (name, _) in std::env::vars_os() {
        if name
            .to_str()
            .is_some_and(|n| crate::ci::is_package_env(n) || is_bundler_override(n))
        {
            cmd.env_remove(&name);
        }
    }
    for name in NO_PROXY_VARS {
        cmd.env_remove(name);
    }
    cmd.envs(no_network_env())
        .env("CARGO_TARGET_DIR", &dir)
        .env(
            "CARGO_HOME",
            build::neutral_cargo_home(std::env::var_os("SystemDrive")),
        );
    eprintln!("xtask build: pnpm {}", args.join(" "));
    let out = run_with_timeout(&mut cmd, BUNDLER_TIMEOUT)
        .context("the bundler did not finish within the deadline")?;
    out.require_complete("the bundler's output")?;
    ensure!(out.success, "the bundler failed: {}", out.describe());
    ensure!(
        !bundler_downloaded(&out.stdout) && !bundler_downloaded(&out.stderr),
        "the bundler looked for a tool it should already have: {}",
        out.describe()
    );
    for path in &installers {
        let size = std::fs::metadata(path)
            .with_context(|| format!("the bundler did not make {}", path.display()))?
            .len();
        println!("{} ({size} bytes)", path.display());
    }
    Ok(installers)
}

/// The manufacturer the bundler writes into the registry, from the merged config: `publisher`, else
/// the second part of the identifier (tauri-bundler 2.10.3, `nsis/mod.rs`, `manufacturer`).
///
/// # Errors
/// A config with neither.
pub fn manufacturer(conf: &serde_json::Value) -> Result<String> {
    if let Some(publisher) = conf["bundle"]["publisher"].as_str() {
        return Ok(publisher.to_owned());
    }
    let id = conf["identifier"]
        .as_str()
        .context("tauri.conf.json has no identifier")?;
    Ok(id.split('.').nth(1).unwrap_or(id).to_owned())
}

/// Every file an install holds, relative to the folder given to the installer.
#[must_use]
pub fn expected_files(target: &str) -> Vec<PathBuf> {
    let mut files = vec![
        PathBuf::from(APP_EXE),
        PathBuf::from(APP_SYMBOLS),
        PathBuf::from(MEETING_NOTICE_IN_INSTALL),
        PathBuf::from("uninstall.exe"),
    ];
    let features = build::shipped_features(Some(target), &[]);
    files.extend(resource_dlls(&features).iter().map(PathBuf::from));
    if has_vulkan_summaries(&features) {
        files.push(PathBuf::from(VULKAN_LICENCE));
    }
    files
}

/// `(missing, unexpected)`: how `found` differs from `expected`, ignoring case as Windows does.
#[must_use]
pub fn file_set_differences(
    found: &[PathBuf],
    expected: &[PathBuf],
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let key = |p: &PathBuf| p.to_string_lossy().to_lowercase().replace('/', "\\");
    let not_in = |from: &[PathBuf], other: &[PathBuf]| -> Vec<PathBuf> {
        let keys: std::collections::BTreeSet<String> = other.iter().map(key).collect();
        from.iter()
            .filter(|p| !keys.contains(&key(p)))
            .cloned()
            .collect()
    };
    (not_in(expected, found), not_in(found, expected))
}

/// One verdict of `bundle-check`, printed as it is reached.
fn verdict(all: &mut bool, name: &str, pass: bool, detail: &str) {
    println!(
        "bundle-check: {} {name}: {detail}",
        if pass { "ok  " } else { "FAIL" }
    );
    *all &= pass;
}

/// Whether the install at `dir` is what was built: exactly the expected files, with the exe and
/// DLLs byte for byte.
fn check_install(all: &mut bool, dir: &Path, exe_dir: &Path, target: &str) -> Result<()> {
    let mut found = Vec::new();
    for file in files_under(dir)? {
        found.push(file.strip_prefix(dir)?.to_path_buf());
    }
    let (missing, unexpected) = file_set_differences(&found, &expected_files(target));
    verdict(
        all,
        "nsis: exactly the expected files",
        missing.is_empty() && unexpected.is_empty(),
        &format!(
            "{} files, missing: {missing:?}, unexpected: {unexpected:?}",
            found.len()
        ),
    );
    let mut names = vec![APP_EXE];
    let features = build::shipped_features(Some(target), &[]);
    names.extend(resource_dlls(&features));
    if has_vulkan_summaries(&features) {
        names.push(VULKAN_LICENCE);
    }
    let mut pairs: Vec<(&str, &str)> = names.into_iter().map(|n| (n, n)).collect();
    pairs.push((APP_SYMBOLS, APP_SYMBOLS_BUILT));
    for (name, built) in pairs {
        let installed = dir.join(name);
        let same =
            installed.is_file() && sha256_file(&installed)? == sha256_file(&exe_dir.join(built))?;
        verdict(all, &format!("nsis: {name}"), same, "same bytes as built");
    }
    let (scanned, findings) = paths::scan_tree(dir, &[], &paths::Needles::of_this_user()?)?;
    verdict(
        all,
        "nsis: no build machine's paths",
        scanned > 0 && findings.is_empty(),
        &format!(
            "{scanned} files scanned for the build user's name and home folder{}{}",
            if findings.is_empty() { "" } else { ": " },
            paths::describe(&findings)
        ),
    );
    Ok(())
}

/// Whether an installer's uncompressed bytes name the build machine. They say nothing about its
/// compressed contents, which only the scan of an install proves.
fn check_installer_paths(all: &mut bool, installer: &Path) -> Result<()> {
    let bytes =
        std::fs::read(installer).with_context(|| format!("read {}", installer.display()))?;
    let name = installer.file_name().map(PathBuf::from).unwrap_or_default();
    let found = paths::scan_bytes(&name, &bytes, &paths::Needles::of_this_user()?);
    verdict(
        all,
        "nsis: no build machine's paths in the installer's uncompressed bytes",
        found.is_none(),
        &found.map_or_else(
            || {
                format!(
                    "{} bytes; the install's scan is what proves the contents",
                    bytes.len()
                )
            },
            |f| paths::describe(&[f]),
        ),
    );
    Ok(())
}

/// Runs the NSIS installer silently into `dir`.
fn nsis_install(installer: &Path, dir: &Path) -> Result<()> {
    let mut cmd = Command::new(installer);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(nsis_install_args(dir));
    }
    let out = run_with_timeout(&mut cmd, INSTALL_TIMEOUT)
        .context("the silent install did not finish within the deadline")?;
    ensure!(out.success, "the silent install failed: {}", out.describe());
    Ok(())
}

/// Runs the uninstaller in `dir` silently and waits until it has finished.
///
/// `uninstall.exe` re-runs itself from a temp copy and returns at once, and it deletes its files
/// and folder before its registry work, so neither the process nor the folder says it is done: a
/// restore that trusted them could be undone by the uninstaller afterwards. Its last registry step
/// deletes the `Run` value, so the value is set to [`RUN_SENTINEL`] first (the guard already holds
/// the real one) and its disappearance is the signal. That also proves the deletion happens.
fn nsis_uninstall(dir: &Path, reg: &Reg, run_value: &str) -> Result<()> {
    reg.set_string(registry::RUN_KEY, run_value, RUN_SENTINEL)?;
    let out = run_with_timeout(
        Command::new(dir.join("uninstall.exe")).arg("/S"),
        INSTALL_TIMEOUT,
    )
    .context("the silent uninstall did not finish within the deadline")?;
    ensure!(out.success, "the uninstaller failed: {}", out.describe());
    let start = Instant::now();
    while reg.value_exists(registry::RUN_KEY, run_value)? {
        ensure!(
            start.elapsed() < UNINSTALL_WAIT,
            "the uninstaller of {} has not finished after {UNINSTALL_WAIT:?}",
            dir.display()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// `cargo xtask bundle-check`: installs the host's NSIS build into a folder under
/// `<repo>\target`, runs `accept m7`'s replay and judgement on the installed exe with nothing of
/// the repo or the build on `PATH`, and uninstalls. Returns whether everything passed. The replay proves the installed
/// exe and DLLs; its models and ONNX Runtime come from `EARSHOT_MODELS`, not from the install, so
/// it says nothing about a machine without them.
///
/// **The user's own state.** The installer runs with `currentUser` mode and `/NS`, so it makes no
/// shortcut and needs no elevation, and a running Earshot elsewhere is not stopped: its
/// `CheckIfAppIsRunning` asks Restart Manager about `$INSTDIR\earshot.exe` by path, and
/// `$INSTDIR` is under `target`.
///
/// - Saved first and restored on every path, then read again ([`registry`]; the run is refused
///   before anything is touched if the save cannot be proven): `Run\Earshot`,
///   `Uninstall\Earshot`, `Software\<manufacturer>\Earshot` and `Software\<manufacturer>`.
/// - Not restored: the uninstaller also clears the taskbar jump list of the app id
///   `earshot` (`DeleteAppUserModelId`), shared with an installed Earshot; the app
///   sets no jump list of its own, so at most Windows's recent items for it are lost.
/// - Not covered: a process killed mid-run restores nothing, and the next run refuses until the
///   saved files in `<repo>\target\bundle-check-registry` are dealt with. Until then `Run\Earshot`
///   may hold the sentinel, which fails harmlessly at logon.
///
/// An agent shell of the Claude desktop app sees an MSIX-virtualised registry, so a run there
/// proves the logic against that view only: what happens to the real `HKCU` needs a run from
/// The owner's own shell.
///
/// # Errors
/// An installer that is missing or cannot run, or a registry save or restore that fails.
pub fn check(root: &Path, target_dir: Option<&Path>) -> Result<bool> {
    let triple = effective_target(None).to_owned();
    let version = workspace_version(root)?;
    // Before anything is touched: a user the scan cannot judge stops the run here.
    paths::Needles::of_this_user()?;
    let dir = build::short_target_dir(root, target_dir, std::env::var_os("SystemDrive"), None);
    let exe_dir = dir.join("release");
    let nsis = exe_dir
        .join("bundle")
        .join(installer_path(&version, &triple)?);
    ensure!(
        nsis.is_file(),
        "{} is missing: run `cargo xtask build --bundle` with the same --target-dir",
        nsis.display()
    );
    let conf_path = root.join("apps/desktop/src-tauri/tauri.conf.json");
    let conf: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&conf_path)
            .with_context(|| format!("read {}", conf_path.display()))?,
    )
    .with_context(|| format!("parse {}", conf_path.display()))?;
    let product = conf["productName"]
        .as_str()
        .context("tauri.conf.json has no productName")?
        .to_owned();
    let vendor = manufacturer(&conf)?;

    let reg = Reg::system()?;
    let backup = root.join("target").join("bundle-check-registry");
    let guard = RegistryGuard::save(reg.clone(), registry::places(&vendor, &product), &backup)?;
    let names = Installed {
        reg,
        product,
        vendor,
    };
    let result = check_saved(root, &nsis, &exe_dir, &triple, &names);
    let restored = guard.finish();
    let mut all = with_restore_error(result, restored.as_ref().err())?;
    match restored {
        Ok(()) => verdict(
            &mut all,
            "registry restored",
            true,
            "the saved values and keys read back identical",
        ),
        Err(e) => verdict(&mut all, "registry restored", false, &format!("{e:#}")),
    }
    let work = root.join("target").join("bundle-check");
    if all {
        let _ = std::fs::remove_dir_all(&work);
    } else {
        println!("bundle-check: work folder kept at {}", work.display());
    }
    Ok(all)
}

/// The check's result, with a failed restore added to its error: a check that failed must not hide
/// that the user's registry was left changed.
///
/// # Errors
/// `result`'s error.
pub fn with_restore_error(result: Result<bool>, restored: Option<&anyhow::Error>) -> Result<bool> {
    match (result, restored) {
        (Err(e), Some(r)) => Err(e.context(format!("and the registry was not restored: {r:#}"))),
        (result, _) => result,
    }
}

/// The registry names of the install under test.
struct Installed {
    reg: Reg,
    product: String,
    vendor: String,
}

/// The check itself, with the registry saved by the caller.
fn check_saved(
    root: &Path,
    nsis: &Path,
    exe_dir: &Path,
    triple: &str,
    names: &Installed,
) -> Result<bool> {
    let work = root.join("target").join("bundle-check");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let mut all = true;

    check_installer_paths(&mut all, nsis)?;
    let install = work.join("install");
    nsis_install(nsis, &install)?;
    check_install(&mut all, &install, exe_dir, triple)?;

    // The names the guard saved are the ones the installer really used.
    let product_key = format!(r"HKCU\Software\{}\{}", names.vendor, names.product);
    let wrote = names.reg.default_value_text(&product_key)?;
    verdict(
        &mut all,
        "registry: the install wrote the saved product key",
        wrote
            .as_deref()
            .is_some_and(|t| t.contains(&install.display().to_string())),
        &product_key,
    );
    let uninstall_key = format!(r"{}\{}", registry::UNINSTALL_KEY, names.product);
    verdict(
        &mut all,
        "registry: the install wrote the saved uninstall key",
        names.reg.key_exists(&uninstall_key)?,
        &uninstall_key,
    );

    let installed = install.join(APP_EXE);
    let system_root = std::env::var_os("SystemRoot").context("SystemRoot is not set")?;
    let system_path = std::env::join_paths([
        Path::new(&system_root).join("System32"),
        PathBuf::from(&system_root),
    ])?;
    let empty_cwd = work.join("cwd");
    std::fs::create_dir_all(&empty_cwd)?;
    let isolated = |cmd: &mut Command| {
        cmd.env("PATH", &system_path).current_dir(&empty_cwd);
    };
    for c in m7::replay(root, &installed, &isolated)? {
        verdict(
            &mut all,
            &format!("replay: {}", c.name),
            c.pass,
            c.detail.as_deref().unwrap_or_default(),
        );
    }

    nsis_uninstall(&install, &names.reg, &names.product)?;
    verdict(
        &mut all,
        "nsis: uninstall leaves no folder",
        !install.exists(),
        &install.display().to_string(),
    );

    // The default per-user install folder is also the data folder (%LOCALAPPDATA%\Earshot).
    let shared = work.join("shared");
    nsis_install(nsis, &shared)?;
    let database = shared.join("earshot.db");
    let model = shared.join("models").join("kept.bin");
    // The browser engine's files sit there too: the identifier names the same folder.
    let webview = shared.join("EBWebView").join("kept.bin");
    std::fs::create_dir_all(shared.join("models"))?;
    std::fs::create_dir_all(shared.join("EBWebView"))?;
    std::fs::write(&database, "user data")?;
    std::fs::write(&model, "user data")?;
    std::fs::write(&webview, "user data")?;
    nsis_uninstall(&shared, &names.reg, &names.product)?;
    verdict(
        &mut all,
        "nsis: uninstall keeps data in the install folder",
        database.is_file()
            && model.is_file()
            && webview.is_file()
            && !shared.join(APP_EXE).exists(),
        "a database, a model and the browser engine's files beside the exe survive; the exe does not",
    );

    Ok(all)
}

#[cfg(test)]
mod tests;
