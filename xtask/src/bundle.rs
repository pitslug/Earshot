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
//! The installer also carries one hook of ours ([`NSIS_HOOKS`]), which takes the exe off the user's
//! crash report exclusion list on a real uninstall (T9.11).
//!
//! What an install holds: the exe (the benchmark meeting is compiled into it), llama.cpp's DLLs,
//! the exe's public symbols for crash logs ([`APP_SYMBOLS`]) and one notice crediting the meeting
//! and the common words compiled into the exe ([`install_notice`], T12.15ee). Nothing
//! in it names the build user ([`paths`]). No model, no ONNX Runtime (it lives in the models folder), no
//! CUDA library, no QNN library on x64 ([`refused_in_install`]), no updater. The ARM64 install also
//! holds Qualcomm's QNN execution provider and the libraries the manifest pins for it, with their
//! licences and a notice ([`qnn_install_files`], T8.4e: the owner approved bundling them), copied
//! from the verified extraction of `models/manifest.toml`'s [`QNN_ARTEFACT`]. WebView2 mode is `skip`: Tauri's
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
/// Where the install's notice sits: the meeting's attribution, then the common words'
/// ([`install_notice`]).
const MEETING_NOTICE_IN_INSTALL: &str = "resources/benchmark/NOTICE.md";
/// The install's notice as the bundle writes it beside the exe, before the bundler copies it to
/// [`MEETING_NOTICE_IN_INSTALL`].
pub const INSTALL_NOTICE_BUILT: &str = "earshot-notice.md";
/// The heading of the common words' attribution in the install's notice (T12.15ee).
pub const COMMON_WORDS_HEADING: &str = "## Common words";
/// The licence the common words' attribution must name.
pub const COMMON_WORDS_LICENCE_URL: &str = "https://creativecommons.org/licenses/by/4.0/";
/// What the install's notice must hold, checked by `bundle-check` ([`notice_missing_credits`]).
const NOTICE_CREDITS: &[&str] = &[
    "# Benchmark meeting",
    COMMON_WORDS_HEADING,
    "AMI Meeting Corpus manual annotations v1.6.2",
    "most frequent",
    COMMON_WORDS_LICENCE_URL,
];
/// The app's executable.
pub const APP_EXE: &str = "earshot.exe";
/// The uninstaller's hook (`bundle.windows.nsis.installerHooks`), relative to the repo root.
pub const NSIS_HOOKS: &str = "apps/desktop/src-tauri/nsis-hooks.nsh";
/// The app's public-symbols program database as the link makes it (the app's `build.rs`).
pub const APP_SYMBOLS_BUILT: &str = "earshot.stripped.pdb";
/// Its name in an install: the one the exe's debug record names, beside the exe, where a crash
/// log's backtrace finds it. It holds function names only; the full database never ships.
pub const APP_SYMBOLS: &str = "earshot.pdb";
/// Deadline for the bundler.
const BUNDLER_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Deadline for a silent install or an administrative extract.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Set as the `Run` value while an uninstaller runs: its deletion means the uninstaller has only
/// our hook's registry step left ([`nsis_uninstall`]). Not a real command, so a logon that finds
/// one left by a killed run fails to start it and nothing else.
const RUN_SENTINEL: &str = "earshot-bundle-check-sentinel";
/// How long a silent uninstall may take: the uninstaller re-runs itself from a temp copy and
/// returns before it has finished.
const UNINSTALL_WAIT: Duration = Duration::from_secs(90);
/// How long the hook gets after the `Run` deletion, which comes moments before it.
const HOOK_WAIT: Duration = Duration::from_secs(15);
/// A mode of the installed exe that lists it, then exits at once when given nothing else: the app's
/// own model check, which refuses to run without its `--languages` and `--models`.
const LISTING_MODE: &str = "--model-check";

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

/// The Visual C++ runtime an install holds beside the exe (T9.1w, the owner), because a PC without
/// Microsoft's redistributable could not start the app or load ONNX Runtime, and installing that
/// needs an admin prompt and a download. Exactly what the exe, llama.cpp, ggml and ONNX Runtime
/// import; the Universal CRT comes with Windows. Microsoft's terms allow these files for licensed
/// Visual Studio users (`VC\Redist` in its redistribution list). A new release carries a new copy,
/// because Windows Update does not service these.
pub const VC_RUNTIME_DLLS: [&str; 4] = [
    "msvcp140.dll",
    "msvcp140_1.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll",
];

/// [`VC_RUNTIME_DLLS`] for `target`. On ARM64 without `vcruntime140_1.dll`: that helper exists for
/// x64 code only, and the copy in Visual Studio's `arm64\Microsoft.VC*.CRT` folder is an x64 binary
/// (seen with 14.44.35112), which no ARM64 binary imports.
#[must_use]
pub fn vc_runtime_dlls(target: &str) -> &'static [&'static str] {
    if target == ARM64_WINDOWS {
        &VC_RUNTIME_DLLS[..3]
    } else {
        &VC_RUNTIME_DLLS
    }
}

/// Whether `name` is part of the Visual C++ runtime, which Windows may not have.
#[must_use]
pub fn is_vc_runtime_library(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["msvcp1", "vcruntime1", "concrt1", "vcomp1", "vccorlib1"]
        .iter()
        .any(|prefix| n.starts_with(prefix))
}

/// The newest `<redist>\<version>\<arch>\Microsoft.VC*.CRT` folder: a Visual Studio's folder
/// `VC\Redist\MSVC` holds one version per update, and other names beside them.
#[must_use]
pub fn newest_vc_runtime_folder(redist: &Path, arch: &str) -> Option<PathBuf> {
    let version = |name: &str| -> Option<Vec<u32>> {
        name.split('.').map(|part| part.parse().ok()).collect()
    };
    let mut found: Vec<(Vec<u32>, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(redist).ok()?.filter_map(Result::ok) {
        let Some(v) = version(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let Ok(inner) = std::fs::read_dir(entry.path().join(arch)) else {
            continue;
        };
        for crt in inner.filter_map(Result::ok) {
            let name = crt.file_name().to_string_lossy().to_ascii_lowercase();
            if name.starts_with("microsoft.vc") && name.ends_with(".crt") {
                found.push((v.clone(), crt.path()));
            }
        }
    }
    found.sort();
    found.pop().map(|(_, path)| path)
}

/// Copies [`vc_runtime_dlls`] for `triple` beside the exe, from the Visual Studio that built it.
fn place_vc_runtime(exe_dir: &Path, triple: &str) -> Result<()> {
    let arch = if triple.starts_with("aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let redist = build::visual_studio_install()?.join(r"VC\Redist\MSVC");
    let folder = newest_vc_runtime_folder(&redist, arch).with_context(|| {
        format!(
            "no Visual C++ runtime for {arch} under {}: add the Visual Studio component \
             Microsoft.VisualStudio.Component.VC.Redist.14.Latest",
            redist.display()
        )
    })?;
    for dll in vc_runtime_dlls(triple) {
        let (from, to) = (folder.join(dll), exe_dir.join(dll));
        std::fs::copy(&from, &to)
            .with_context(|| format!("copy {} to {}", from.display(), to.display()))?;
    }
    Ok(())
}

/// The DLLs an install for `target` holds beside the exe for `features`.
#[must_use]
pub fn resource_dlls(target: &str, features: &[String]) -> Vec<&'static str> {
    let mut dlls = llama_dlls::REQUIRED.to_vec();
    dlls.extend(vc_runtime_dlls(target));
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

/// Whether a file named `name` must not be in an install for `target` built with `features`: a
/// vendor library (except, on ARM64, the QNN files of [`qnn_install_files`]), or llama.cpp's CUDA
/// backend outside a CUDA build (a stale one would need NVIDIA's libraries).
#[must_use]
pub fn refused_in_install(name: &str, target: &str, features: &[String]) -> bool {
    (is_vendor_accelerator_library(name) && !is_shipped_qnn_file(name, target))
        || (name.eq_ignore_ascii_case("ggml-cuda.dll") && !features.iter().any(|f| f == "cuda"))
}

/// The ARM64 package of Qualcomm's QNN execution provider in `models/manifest.toml` (T8.3).
pub const QNN_ARTEFACT: &str = "qnn-ep-win-arm64";
/// Its `cargo xtask fetch-models` set.
pub const QNN_SET: &str = "qnn";
/// The QNN libraries an ARM64 install holds beside the exe (T8.4e): exactly the libraries the
/// manifest pins for the plugin, which `earshot_onnx::qnn::QNN_ARM64` checks before loading, and
/// the skels' signing catalogs (T12.12e: without them a context model compiled with them fails to
/// load, so the install's plugin folder matches the models folder's).
pub const QNN_LIBRARIES: [&str; 10] = [
    "onnxruntime_providers_qnn.dll",
    "QnnHtp.dll",
    "QnnHtpPrepare.dll",
    "QnnSystem.dll",
    "QnnHtpV73Stub.dll",
    "libQnnHtpV73Skel.so",
    "QnnHtpV81Stub.dll",
    "libQnnHtpV81Skel.so",
    "libqnnhtpv73.cat",
    "libqnnhtpv81.cat",
];
/// The plugin's MIT licence (`LICENSE` in the zip) in an install, renamed so it says whose it is.
pub const QNN_LICENSE_IN_INSTALL: &str = "QNN-LICENSE.txt";
/// ONNX Runtime's third-party notices as the plugin's zip carries them, in an install.
pub const QNN_NOTICES_IN_INSTALL: &str = "QNN-ThirdPartyNotices.txt";
/// The notice an ARM64 install holds beside the exe, naming the QNN files and their licences.
pub const QNN_NOTICE: &str = "NOTICE.md";

/// `(file in the extraction, file in the install)` for every QNN file an ARM64 install holds,
/// licences included; [`QNN_NOTICE`] is written by the bundle, not copied.
#[must_use]
pub fn qnn_install_files() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "onnxruntime_providers_qnn.dll",
            "onnxruntime_providers_qnn.dll",
        ),
        ("QnnHtp.dll", "QnnHtp.dll"),
        ("QnnHtpPrepare.dll", "QnnHtpPrepare.dll"),
        ("QnnSystem.dll", "QnnSystem.dll"),
        ("QnnHtpV73Stub.dll", "QnnHtpV73Stub.dll"),
        ("libQnnHtpV73Skel.so", "libQnnHtpV73Skel.so"),
        ("QnnHtpV81Stub.dll", "QnnHtpV81Stub.dll"),
        ("libQnnHtpV81Skel.so", "libQnnHtpV81Skel.so"),
        ("libqnnhtpv73.cat", "libqnnhtpv73.cat"),
        ("libqnnhtpv81.cat", "libqnnhtpv81.cat"),
        ("Qualcomm_LICENSE.pdf", "Qualcomm_LICENSE.pdf"),
        ("LICENSE", QNN_LICENSE_IN_INSTALL),
        ("ThirdPartyNotices.txt", QNN_NOTICES_IN_INSTALL),
    ]
}

/// Whether `name` is one of the QNN files an install for `target` holds (ARM64 only).
fn is_shipped_qnn_file(name: &str, target: &str) -> bool {
    target == ARM64_WINDOWS
        && qnn_install_files()
            .iter()
            .any(|(_, to)| to.eq_ignore_ascii_case(name))
}

/// The text of [`QNN_NOTICE`].
#[must_use]
pub fn qnn_notice_text() -> String {
    let mut text = String::from(
        "# Third-party files in this folder\n\
         \n\
         Earshot uses the Snapdragon NPU through Qualcomm's QNN execution provider for ONNX \
         Runtime, release 2.7.0 (https://github.com/onnxruntime/onnxruntime-qnn). These files \
         beside earshot.exe come unmodified from that release:\n\n",
    );
    for (_, to) in &qnn_install_files()[..QNN_LIBRARIES.len()] {
        text.push_str(&format!("- {to}\n"));
    }
    text.push_str(&format!(
        "\n\
         `onnxruntime_providers_qnn.dll` is Copyright (c) Qualcomm Technologies, Inc. and/or its \
         subsidiaries and Copyright (c) Microsoft Corporation, under the MIT licence in \
         `{QNN_LICENSE_IN_INSTALL}`. ONNX Runtime's third-party notices, as that release carries \
         them, are in `{QNN_NOTICES_IN_INSTALL}`.\n\
         \n\
         The other files are the Qualcomm AI Runtime libraries, distributed in object code as part \
         of this application under the Qualcomm AI Stack License in `Qualcomm_LICENSE.pdf`. Read \
         that licence for its terms; it does not permit distributing these files on their own.\n"
    ));
    text
}

/// Fetches [`QNN_ARTEFACT`] into the models folder (download and extraction checked against the
/// manifest, every extracted file re-hashed), copies [`qnn_install_files`] beside the exe, checks
/// each copy against its member pin, and writes [`QNN_NOTICE`].
fn place_qnn(root: &Path, exe_dir: &Path) -> Result<()> {
    let m = Manifest::load(&crate::models::manifest_path(root))?;
    let arts: Vec<&manifest::Artefact> = m
        .select(&[QNN_SET.to_owned()])?
        .into_iter()
        .filter(|a| a.id == QNN_ARTEFACT)
        .collect();
    ensure!(
        arts.len() == 1,
        "models/manifest.toml has no {QNN_ARTEFACT} in set {QNN_SET}"
    );
    let cache = crate::models::cache_dir()?;
    let reports = manifest::fetch_all(&arts, root, &cache, true, &m.policy())?;
    manifest::print_reports("bundle QNN files", &cache, &reports);
    let extract = arts[0]
        .extract
        .as_ref()
        .with_context(|| format!("{QNN_ARTEFACT} is not extracted"))?;
    let into = crate::download::jail_join(&cache, &extract.into)?;
    for (from, to) in qnn_install_files() {
        let member = extract
            .members
            .iter()
            .find(|mm| mm.path == *from)
            .with_context(|| format!("{from} is not a pinned member of {QNN_ARTEFACT}"))?;
        let (source, dest) = (crate::download::jail_join(&into, from)?, exe_dir.join(to));
        std::fs::copy(&source, &dest)
            .with_context(|| format!("copy {} to {}", source.display(), dest.display()))?;
        crate::download::verify_file(
            &dest,
            &crate::download::Expected {
                sha256: &member.sha256,
                size: member.size,
            },
        )
        .with_context(|| format!("{} after the copy", dest.display()))?;
    }
    let notice = exe_dir.join(QNN_NOTICE);
    std::fs::write(&notice, qnn_notice_text())
        .with_context(|| format!("write {}", notice.display()))
}

/// The QNN files and notice of an install for `target`, by their names in the install.
fn qnn_install_names(target: &str) -> Vec<&'static str> {
    if target != ARM64_WINDOWS {
        return Vec::new();
    }
    let mut names: Vec<&str> = qnn_install_files().iter().map(|(_, to)| *to).collect();
    names.push(QNN_NOTICE);
    names
}

/// [`NSIS_HOOKS`] below `root`, in the system's own separators: makensis finds no `!include` with
/// both kinds.
#[must_use]
pub fn nsis_hooks_path(root: &Path) -> PathBuf {
    let mut path = root.to_path_buf();
    path.extend(Path::new(NSIS_HOOKS).iter());
    path
}

/// The common words' attribution, from the `.NOTICE` beside the list in `earshot-core`
/// (`cargo xtask common-words` writes it): its source, licence and changes lines unchanged.
///
/// # Errors
/// A core notice without one of those lines.
pub fn common_words_credit(core_notice: &str) -> Result<String> {
    let field = |name: &str| -> Result<&str> {
        core_notice
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .with_context(|| format!("the common words' notice has no {name:?} line"))
    };
    let (source, licence, changes) = (field("Source:")?, field("Licence:")?, field("Changes:")?);
    let n = crate::common_words::COMMON_WORDS;
    Ok(format!(
        "\n{COMMON_WORDS_HEADING}\n\
         \n\
         The app holds a list of the {},{:03} most frequent words of the AMI Meeting Corpus \
         annotations, derived from them by counting words and keeping the most frequent. It is \
         compiled into `{APP_EXE}` and used to suggest your Dictionary's words beside a \
         transcript's words.\n\
         \n\
         - Source: {source}\n\
         - Licence: {licence}\n\
         - Changes: {changes}\n",
        n / 1000,
        n % 1000
    ))
}

/// The install's notice: the benchmark meeting's notice, then [`common_words_credit`].
///
/// # Errors
/// As [`common_words_credit`].
pub fn install_notice(meeting_notice: &str, core_notice: &str) -> Result<String> {
    Ok(format!(
        "{meeting_notice}{}",
        common_words_credit(core_notice)?
    ))
}

/// [`install_notice`] from this checkout's files.
///
/// # Errors
/// An unreadable notice, or as [`install_notice`].
pub fn install_notice_text(root: &Path) -> Result<String> {
    let read =
        |p: PathBuf| std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()));
    let meeting = read(root.join(MEETING_NOTICE))?;
    let core = read(
        root.join(crate::common_words::OUT_DIR)
            .join(crate::common_words::NOTICE_FILE),
    )?;
    install_notice(&meeting, &core)
}

/// Writes this checkout's [`install_notice_text`] beside the built exe as
/// [`INSTALL_NOTICE_BUILT`] and returns its path: the file [`bundle_config`] installs as
/// [`MEETING_NOTICE_IN_INSTALL`] (T12.15ee).
///
/// # Errors
/// As [`install_notice_text`]; a failed write.
pub fn write_install_notice(root: &Path, exe_dir: &Path) -> Result<PathBuf> {
    let notice = exe_dir.join(INSTALL_NOTICE_BUILT);
    std::fs::write(&notice, install_notice_text(root)?)
        .with_context(|| format!("write {}", notice.display()))?;
    Ok(notice)
}

/// The credits the install's notice `text` lacks ([`NOTICE_CREDITS`]).
#[must_use]
pub fn notice_missing_credits(text: &str) -> Vec<&'static str> {
    NOTICE_CREDITS
        .iter()
        .copied()
        .filter(|c| !text.contains(c))
        .collect()
}

/// `bundle-check`'s verdict on an installed notice: every credit present and the text what this
/// checkout assembles (`expected`), with the detail to print.
#[must_use]
pub fn notice_check(installed: &str, expected: &str) -> (bool, String) {
    let missing = notice_missing_credits(installed);
    let same = installed == expected;
    (
        missing.is_empty() && same,
        format!(
            "missing: {missing:?}, {}",
            if same {
                "same text as this checkout assembles"
            } else {
                "differs from what this checkout assembles"
            }
        ),
    )
}

/// The settings laid over `tauri.conf.json` for one bundle run. Every path is a whole one: the CLI
/// canonicalises `installerHooks` from its own working folder, not from this file's.
#[must_use]
pub fn bundle_config(
    exe_dir: &Path,
    notice: &Path,
    hooks: &Path,
    formats: &[&str],
    target: &str,
    features: &[String],
) -> serde_json::Value {
    let mut resources = serde_json::Map::new();
    for name in resource_dlls(target, features)
        .into_iter()
        .chain(qnn_install_names(target))
    {
        resources.insert(exe_dir.join(name).display().to_string(), json!(name));
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
                "nsis": {
                    "installMode": "currentUser",
                    "installerHooks": hooks.display().to_string(),
                },
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
    // The exe loads the QNN provider at run time, so even on ARM64 it imports no vendor library.
    let refused: Vec<&String> = pe
        .imports
        .iter()
        .filter(|i| {
            refused_in_install(i, target, features)
                || (name.eq_ignore_ascii_case(APP_EXE) && is_vendor_accelerator_library(i))
        })
        .collect();
    ensure!(
        refused.is_empty(),
        "{name} imports {refused:?}, which no install holds or the exe must not import (a CUDA \
         build's install is T8.4a's)"
    );
    // Without the loader such an exe does not start where drivers provide none (T8.4ac).
    ensure!(
        !pe.imports
            .iter()
            .any(|i| i.eq_ignore_ascii_case(VULKAN_LOADER))
            || resource_dlls(target, features).contains(&VULKAN_LOADER),
        "{name} imports {VULKAN_LOADER}, which an install built with {features:?} does not hold"
    );
    // A runtime library the list misses would start here, where Visual Studio is installed, and
    // nowhere else.
    let missing: Vec<&String> = pe
        .imports
        .iter()
        .filter(|i| {
            is_vc_runtime_library(i)
                && !vc_runtime_dlls(target)
                    .iter()
                    .any(|dll| dll.eq_ignore_ascii_case(i))
        })
        .collect();
    ensure!(
        missing.is_empty(),
        "{name} imports {missing:?}, which the install for {target} does not hold"
    );
    Ok(())
}

/// The silent NSIS command line. `/D=` must come last and unquoted (NSIS reads to the end of the
/// line); `/NS` leaves the user's Desktop and Start menu alone.
#[must_use]
pub fn nsis_install_args(dir: &Path) -> String {
    format!("/S /NS /D={}", dir.display())
}

/// [`nsis_install_args`] for an update: the installer's `/UPDATE`, which `earshot_runtime::update`
/// also passes, without its window and restart.
#[must_use]
pub fn nsis_update_install_args(dir: &Path) -> String {
    format!("/S /NS /UPDATE /D={}", dir.display())
}

/// The uninstaller's silent update command line, as `installer.nsi` builds it: `_?=` must come last
/// and unquoted, and runs it in place, so its process ends when it has finished. It then cannot
/// delete itself or its folder.
#[must_use]
pub fn nsis_update_uninstall_args(dir: &Path) -> String {
    format!("/S /UPDATE _?={}", dir.display())
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
    place_vc_runtime(exe_dir, &triple)?;
    if triple == ARM64_WINDOWS {
        place_qnn(root, exe_dir)?;
    }
    ensure!(
        exe_dir.join(APP_SYMBOLS_BUILT).is_file(),
        "the build left no {APP_SYMBOLS_BUILT} beside {APP_EXE}"
    );
    check_binary(APP_EXE, &read_pe(&exe)?, &triple, features)?;
    for dll in resource_dlls(&triple, features) {
        let path = exe_dir.join(dll);
        check_binary(dll, &read_pe(&path)?, &triple, features)?;
    }
    // The Hexagon skels are not Windows binaries.
    for name in qnn_install_names(&triple)
        .into_iter()
        .filter(|n| n.ends_with(".dll"))
    {
        check_binary(name, &read_pe(&exe_dir.join(name))?, &triple, features)?;
    }
    let notice = write_install_notice(root, exe_dir)?;
    let config = dir.join(format!("earshot-bundle-{triple}.json"));
    let overlay = bundle_config(
        exe_dir,
        &notice,
        &nsis_hooks_path(root),
        wanted,
        &triple,
        features,
    );
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
    files.extend(resource_dlls(target, &features).iter().map(PathBuf::from));
    if has_vulkan_summaries(&features) {
        files.push(PathBuf::from(VULKAN_LICENCE));
    }
    files.extend(qnn_install_names(target).into_iter().map(PathBuf::from));
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

/// Refuses a `target` whose install this host cannot run: `bundle-check` installs the app and runs
/// a meeting through it, so it judges only builds for the host's own architecture. An x64 build
/// could run on ARM64 under emulation, but would prove nothing about x64 hardware, so that is
/// refused too.
///
/// # Errors
/// A target with no installer, or one that is not the host's.
pub fn refuse_unless_runnable(target: &str, host: &str) -> Result<()> {
    formats(target)?;
    if target == host {
        return Ok(());
    }
    if target == ARM64_WINDOWS {
        bail!(
            "bundle-check installs and runs the app, so an {target} build must be checked on an \
             ARM64 Windows machine, not this {host} one: copy its installer there and run \
             `cargo xtask bundle-check --installer <path>` from the repo (it checks only the host's \
             own architecture; nothing was installed)"
        );
    }
    bail!(
        "bundle-check checks only the host's own architecture ({host}), not {target}: an x64 \
         build under emulation on ARM64 is not judged; run it on an x64 Windows machine \
         (nothing was installed)"
    )
}

/// `(version, target)` from an installer's file name, `Earshot_<version>_<arch>-setup.exe` as
/// [`installer_path`] makes it.
///
/// # Errors
/// A name of another shape or architecture.
pub fn installer_name_target(path: &Path) -> Result<(String, &'static str)> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{} has no file name", path.display()))?;
    let parsed = name
        .strip_prefix("Earshot_")
        .and_then(|rest| rest.strip_suffix("-setup.exe"))
        .and_then(|rest| rest.rsplit_once('_'))
        .filter(|(version, _)| !version.is_empty());
    let Some((version, arch)) = parsed else {
        bail!("{name} is not named Earshot_<version>_<x64|arm64>-setup.exe as the bundle names it");
    };
    let target = match arch {
        "x64" => X64_WINDOWS,
        "arm64" => ARM64_WINDOWS,
        other => bail!("{name} names architecture {other}, not x64 or arm64"),
    };
    Ok((version.to_owned(), target))
}

/// The target whose COFF machine is `machine` ([`machine_of`] reversed).
#[must_use]
pub fn target_of_machine(machine: u16) -> Option<&'static str> {
    [X64_WINDOWS, ARM64_WINDOWS]
        .into_iter()
        .find(|t| machine_of(t).ok() == Some(machine))
}

/// Where `cargo xtask build --bundle` leaves the exe (and `bundle\`) for `target`, in the order
/// to look: no target is the host's plain build; a named target is cargo's `<dir>\<triple>\release`
/// in the folder [`build::short_target_dir`] chose for it, then, for the host's own triple, a
/// plain build's folder too.
#[must_use]
pub fn built_release_dirs(
    root: &Path,
    explicit: Option<&Path>,
    system_drive: Option<std::ffi::OsString>,
    target: Option<&str>,
    host: &str,
) -> Vec<PathBuf> {
    let Some(t) = target else {
        return vec![build::short_target_dir(root, explicit, system_drive, None).join("release")];
    };
    let mut dirs = vec![
        build::short_target_dir(root, explicit, system_drive.clone(), Some(t))
            .join(t)
            .join("release"),
    ];
    if t == host {
        dirs.push(build::short_target_dir(root, explicit, system_drive, None).join("release"));
    }
    dirs
}

/// `(installed, built)`: the files of an install for `target` compared with the build byte for byte.
fn built_comparisons(target: &str) -> Vec<(&'static str, &'static str)> {
    let mut names = vec![APP_EXE];
    let features = build::shipped_features(Some(target), &[]);
    names.extend(resource_dlls(target, &features));
    if has_vulkan_summaries(&features) {
        names.push(VULKAN_LICENCE);
    }
    names.extend(qnn_install_names(target));
    let mut pairs: Vec<(&str, &str)> = names.into_iter().map(|n| (n, n)).collect();
    pairs.push((APP_SYMBOLS, APP_SYMBOLS_BUILT));
    pairs
}

/// What replaces a byte comparison with the build when only an installer is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instead {
    /// A QNN file: its manifest pin (size and SHA-256).
    Pin,
    /// The QNN notice: it names every QNN file.
    Notice,
    /// A Windows binary: its machine type and imports ([`check_binary`]).
    Binary,
    /// Nothing: no build and no pin to compare it with.
    Skipped,
}

/// How `name`, in an install for `target`, is judged without the build.
#[must_use]
pub fn instead_of_build(name: &str, target: &str) -> Instead {
    let lower = name.to_ascii_lowercase();
    if is_shipped_qnn_file(name, target) {
        Instead::Pin
    } else if target == ARM64_WINDOWS && name.eq_ignore_ascii_case(QNN_NOTICE) {
        Instead::Notice
    } else if lower.ends_with(".dll") || lower.ends_with(".exe") {
        Instead::Binary
    } else {
        Instead::Skipped
    }
}

/// The byte comparisons with the build that nothing replaces without it, with the reason.
#[must_use]
pub fn skipped_without_build(target: &str) -> Vec<(&'static str, &'static str)> {
    built_comparisons(target)
        .into_iter()
        .filter(|(name, _)| instead_of_build(name, target) == Instead::Skipped)
        .map(|(name, _)| {
            (
                name,
                "skipped: no build folder to compare with and no pin for it",
            )
        })
        .collect()
}

/// One line per byte comparison with the build that an installer-only check cannot run, saying what
/// replaces it or that it is skipped.
#[must_use]
pub fn skipped_lines(target: &str) -> Vec<String> {
    built_comparisons(target)
        .into_iter()
        .map(|(name, _)| {
            let how = match instead_of_build(name, target) {
                Instead::Pin => "checked against its manifest pin (size and SHA-256) instead",
                Instead::Notice => "checked that it names every QNN file instead",
                Instead::Binary => "checked by machine type and imports instead",
                Instead::Skipped => "skipped: no build folder to compare with and no pin for it",
            };
            format!("bundle-check: no build: {name}: same bytes as built not compared; {how}")
        })
        .collect()
}

/// The line printed first: which installer is checked, by its SHA-256.
#[must_use]
pub fn installer_digest_line(installer: &Path, sha256: &str) -> String {
    format!(
        "bundle-check: installer {} sha256 {sha256}",
        installer.display()
    )
}

/// The last line: the verdict with the installer's SHA-256, so the file signed can be matched to
/// the file checked. `without_build` counts the comparisons with the build replaced or skipped.
#[must_use]
pub fn installer_summary(
    pass: bool,
    installer: &Path,
    sha256: &str,
    without_build: usize,
) -> String {
    let verdict = if pass { "PASS" } else { "FAIL" };
    let mut line = format!(
        "bundle-check: {verdict} installer {} sha256 {sha256}",
        installer.display()
    );
    if without_build > 0 {
        let s = if without_build == 1 { "" } else { "s" };
        line.push_str(&format!(
            " ({without_build} comparison{s} with the build replaced or skipped, listed at the start)"
        ));
    }
    line
}

/// A QNN file's pin, by its name in the install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QnnPin {
    /// Name in the install.
    pub name: &'static str,
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
}

/// The pins of every QNN file an ARM64 install holds, from [`QNN_ARTEFACT`]'s members in
/// `models/manifest.toml`, in [`qnn_install_files`] order.
///
/// # Errors
/// An unreadable manifest, or a QNN file that is not a pinned member.
pub fn qnn_pins(root: &Path) -> Result<Vec<QnnPin>> {
    let m = Manifest::load(&crate::models::manifest_path(root))?;
    let extract = m
        .artefacts
        .iter()
        .find(|a| a.id == QNN_ARTEFACT)
        .and_then(|a| a.extract.as_ref())
        .with_context(|| format!("models/manifest.toml has no extracted {QNN_ARTEFACT}"))?;
    qnn_install_files()
        .iter()
        .map(|(from, to)| {
            let member = extract
                .members
                .iter()
                .find(|mm| mm.path == *from)
                .with_context(|| format!("{from} is not a pinned member of {QNN_ARTEFACT}"))?;
            Ok(QnnPin {
                name: to,
                sha256: member.sha256.clone(),
                size: member.size,
            })
        })
        .collect()
}

/// Each pin with whether the file of its name in `dir` matches it.
#[must_use]
pub fn qnn_pin_results<'a>(dir: &Path, pins: &'a [QnnPin]) -> Vec<(&'a QnnPin, Result<()>)> {
    pins.iter()
        .map(|pin| {
            let result = crate::download::verify_file(
                &dir.join(pin.name),
                &crate::download::Expected {
                    sha256: &pin.sha256,
                    size: pin.size,
                },
            );
            (pin, result)
        })
        .collect()
}

/// The QNN files the notice `text` does not name.
#[must_use]
pub fn notice_missing_names(text: &str) -> Vec<&'static str> {
    qnn_install_files()
        .iter()
        .map(|(_, to)| *to)
        .filter(|to| !text.contains(to))
        .collect()
}

/// What an install is judged against.
enum Reference<'a> {
    /// The build folder `build --bundle` made it from: the exe and DLLs byte for byte.
    Built(&'a Path),
    /// Only the installer: pins, machine types and imports ([`instead_of_build`]).
    Installer,
}

/// Whether the install at `dir` is what was built: exactly the expected files, with the exe and
/// DLLs byte for byte against the build, or, with only the installer, against what replaces that
/// ([`instead_of_build`]). Every Windows binary's machine and imports are checked either way, and
/// on ARM64 the QNN files against their pins and the notice for their names.
fn check_install(
    all: &mut bool,
    dir: &Path,
    reference: &Reference,
    target: &str,
    qnn: &[QnnPin],
    notice: &str,
) -> Result<()> {
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
    let pairs = built_comparisons(target);
    if let Reference::Built(exe_dir) = reference {
        for (name, built) in &pairs {
            let installed = dir.join(name);
            let same = installed.is_file()
                && sha256_file(&installed)? == sha256_file(&exe_dir.join(built))?;
            verdict(all, &format!("nsis: {name}"), same, "same bytes as built");
        }
    }
    let features = build::shipped_features(Some(target), &[]);
    for (name, _) in &pairs {
        let lower = name.to_ascii_lowercase();
        if !(lower.ends_with(".dll") || lower.ends_with(".exe")) {
            continue;
        }
        let checked = read_pe(&dir.join(name))
            .and_then(|pe| check_binary(name, &pe, target, &features))
            .map_err(|e| format!("{e:#}"));
        verdict(
            all,
            &format!("nsis: {name} machine and imports"),
            checked.is_ok(),
            checked
                .as_ref()
                .err()
                .map_or("built for this target, every import held", String::as_str),
        );
    }
    let installed =
        std::fs::read_to_string(dir.join(MEETING_NOTICE_IN_INSTALL)).unwrap_or_default();
    let (credited, detail) = notice_check(&installed, notice);
    verdict(
        all,
        &format!(
            "nsis: {MEETING_NOTICE_IN_INSTALL} credits the benchmark meeting and the common words"
        ),
        credited,
        &detail,
    );
    if target == ARM64_WINDOWS {
        for (pin, result) in qnn_pin_results(dir, qnn) {
            let detail = match &result {
                Ok(()) => format!("{} bytes, sha256 {} as pinned", pin.size, pin.sha256),
                Err(e) => format!("{e:#}"),
            };
            verdict(
                all,
                &format!("nsis: {} matches its manifest pin", pin.name),
                result.is_ok(),
                &detail,
            );
        }
        let notice = std::fs::read_to_string(dir.join(QNN_NOTICE)).unwrap_or_default();
        let missing = notice_missing_names(&notice);
        verdict(
            all,
            &format!("nsis: {QNN_NOTICE} names every QNN file"),
            !notice.is_empty() && missing.is_empty(),
            &format!("missing: {missing:?}"),
        );
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

/// Runs `exe` with the raw command line `args`, which NSIS reads to the end of the line.
fn run_raw(exe: &Path, args: &str, what: &str) -> Result<()> {
    let mut cmd = Command::new(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(args);
    }
    #[cfg(not(windows))]
    let _ = args;
    let out = run_with_timeout(&mut cmd, INSTALL_TIMEOUT)
        .with_context(|| format!("the {what} did not finish within the deadline"))?;
    ensure!(out.success, "the {what} failed: {}", out.describe());
    Ok(())
}

/// Runs the NSIS installer silently into `dir`.
fn nsis_install(installer: &Path, dir: &Path) -> Result<()> {
    run_raw(installer, &nsis_install_args(dir), "silent install")
}

/// Makes sure the exe is on the crash report exclusion list, leaving an entry the app wrote as it
/// is, with an invented neighbour beside it. The guard holds what was there.
fn fill_exclusion_list(reg: &Reg) -> Result<()> {
    if !reg.value_exists(registry::WER_EXCLUDED_KEY, APP_EXE)? {
        reg.set_dword(registry::WER_EXCLUDED_KEY, APP_EXE, 1)?;
    }
    reg.set_dword(registry::WER_EXCLUDED_KEY, registry::WER_NEIGHBOUR, 1)
}

/// Runs the uninstaller in `dir` silently and waits until it has finished. Returns whether the exe
/// came off the crash report exclusion list, with [`fill_exclusion_list`] done first.
///
/// `uninstall.exe` re-runs itself from a temp copy and returns at once, and it deletes its files
/// and folder before its registry work, so neither the process nor the folder says it is done: a
/// restore that trusted them could be undone by the uninstaller afterwards. Its template's last
/// registry step deletes the `Run` value, so the value is set to [`RUN_SENTINEL`] first (the guard
/// already holds the real one) and its disappearance is the signal. That also proves the deletion
/// happens. Our hook comes after it and is the real last step, so its deletion is awaited too;
/// `false` after [`HOOK_WAIT`] means it did not run, as in an installer built without the hook.
fn nsis_uninstall(dir: &Path, reg: &Reg, run_value: &str) -> Result<bool> {
    fill_exclusion_list(reg)?;
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
    let start = Instant::now();
    while reg.value_exists(registry::WER_EXCLUDED_KEY, APP_EXE)? {
        if start.elapsed() >= HOOK_WAIT {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(true)
}

/// [`nsis_uninstall`], judged: the exe's value is gone and only that.
fn uninstall_and_judge(all: &mut bool, dir: &Path, names: &Installed) -> Result<()> {
    let reg = &names.reg;
    let gone = nsis_uninstall(dir, reg, &names.product)?;
    let neighbour = reg.value_exists(registry::WER_EXCLUDED_KEY, registry::WER_NEIGHBOUR)?;
    verdict(
        all,
        "nsis: uninstall takes the exe off the crash report list and nothing else",
        gone && neighbour,
        &format!("{APP_EXE} gone: {gone}, a neighbouring value kept: {neighbour}"),
    );
    Ok(())
}

/// The real app, in `installed`, writes the name the hook removes: the list is emptied of it and
/// the exe run in a mode that adds it. `prepare` is the replay's isolation.
fn check_app_lists_itself(
    all: &mut bool,
    installed: &Path,
    prepare: &dyn Fn(&mut Command),
    reg: &Reg,
) -> Result<()> {
    if reg.value_exists(registry::WER_EXCLUDED_KEY, APP_EXE)? {
        reg.delete_value(registry::WER_EXCLUDED_KEY, APP_EXE)?;
    }
    let mut cmd = Command::new(installed);
    prepare(&mut cmd);
    cmd.arg(LISTING_MODE);
    let out = run_with_timeout(&mut cmd, INSTALL_TIMEOUT)
        .context("the installed exe's model check did not finish within the deadline")?;
    let listed = reg.value_exists(registry::WER_EXCLUDED_KEY, APP_EXE)?;
    verdict(
        all,
        "nsis: the installed exe lists itself under the name the uninstaller removes",
        listed,
        &format!("{APP_EXE} listed: {listed}, exit code {:?}", out.code),
    );
    Ok(())
}

/// An update keeps the exe on the crash report list: through the installer's `/UPDATE`, which is
/// what the app runs and which does not run the uninstaller at all, and through the uninstaller's,
/// which is the branch of the hook. The uninstall key's deletion shows that uninstaller did run and
/// the `Run` value that it read `/UPDATE`.
fn check_update(all: &mut bool, nsis: &Path, work: &Path, names: &Installed) -> Result<()> {
    let reg = &names.reg;
    let dir = work.join("update");
    nsis_install(nsis, &dir)?;
    fill_exclusion_list(reg)?;
    reg.set_string(registry::RUN_KEY, &names.product, RUN_SENTINEL)?;
    let listed = || -> Result<bool> {
        Ok(reg.value_exists(registry::WER_EXCLUDED_KEY, APP_EXE)?
            && reg.value_exists(registry::WER_EXCLUDED_KEY, registry::WER_NEIGHBOUR)?)
    };

    run_raw(nsis, &nsis_update_install_args(&dir), "update install")?;
    let kept = listed()?;
    verdict(
        all,
        "nsis: an install with /UPDATE keeps the crash report list",
        kept,
        &format!("{APP_EXE} and its neighbour kept: {kept}"),
    );

    let uninstall_key = format!(r"{}\{}", registry::UNINSTALL_KEY, names.product);
    run_raw(
        &dir.join("uninstall.exe"),
        &nsis_update_uninstall_args(&dir),
        "update uninstall",
    )?;
    let ran = !reg.key_exists(&uninstall_key)?;
    let read_update = reg.value_exists(registry::RUN_KEY, &names.product)?;
    let kept = listed()?;
    verdict(
        all,
        "nsis: an uninstall with /UPDATE keeps the crash report list",
        ran && read_update && kept,
        &format!(
            "it ran: {ran}, it read /UPDATE: {read_update}, {APP_EXE} and its neighbour kept: \
             {kept}"
        ),
    );
    Ok(())
}

/// `cargo xtask bundle-check`: installs the NSIS build for `target` (default the host's), or with
/// `installer` that file (T8.4h: the Surface checks the exact ARM64 installer the desktop built),
/// into a folder under `<repo>\target`, runs `accept m7`'s replay and judgement on the installed exe with nothing of
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
///   `Uninstall\Earshot`, `Software\<manufacturer>\Earshot`, `Software\<manufacturer>` and, on the
///   crash report list, the value `earshot.exe` and the check's invented neighbour. Other values
///   on that list are neither saved nor changed.
/// - Not restored: the uninstaller also clears the taskbar jump list of the app id
///   `earshot` (`DeleteAppUserModelId`), shared with an installed Earshot; the app
///   sets no jump list of its own, so at most Windows's recent items for it are lost.
/// - Not covered: a process killed mid-run restores nothing, and the next run refuses until the
///   saved files in `<repo>\target\bundle-check-registry` are dealt with. Until then `Run\Earshot`
///   may hold the sentinel, which fails harmlessly at logon, and the owner's `earshot.exe` may be
///   off the crash report list until the app's next start.
/// - A real Earshot running during the check puts `earshot.exe` back whenever it starts a child
///   process, which can fail a crash report verdict: run the check with the app closed.
///
/// An agent shell of the Claude desktop app sees an MSIX-virtualised registry, so a run there
/// proves the logic against that view only: what happens to the real `HKCU` needs a run from
/// The owner's own shell.
///
/// Only the host's own architecture is checked ([`refuse_unless_runnable`], before anything is
/// touched). The installer's SHA-256 is printed first and in the last line. With `installer` there
/// is no build folder, so the byte comparisons with the build are replaced by pins, machine types
/// and imports, or skipped, each listed at the start ([`skipped_lines`]); its name gives the
/// architecture and version, and the installed exe's machine must agree.
///
/// # Errors
/// A target this host cannot run, an installer that is missing or cannot run, or a registry save
/// or restore that fails.
pub fn check(
    root: &Path,
    target_dir: Option<&Path>,
    target: Option<&str>,
    installer: Option<&Path>,
) -> Result<bool> {
    // The host is the architecture this xtask runs as: run it from the machine's own toolchain.
    let host = effective_target(None);
    let version = workspace_version(root)?;
    let (nsis, exe_dir, triple) = if let Some(path) = installer {
        let (named, triple) = installer_name_target(path)?;
        refuse_unless_runnable(triple, host)?;
        ensure!(
            named == version,
            "{} is version {named} but this repo is {version}: check out the commit it was built \
             from, so the expected files and the replay are the ones it was built with",
            path.display()
        );
        ensure!(path.is_file(), "{} is not a file", path.display());
        (path.to_path_buf(), None, triple)
    } else {
        let triple = effective_target(target);
        refuse_unless_runnable(triple, host)?;
        let name = installer_path(&version, triple)?;
        let dirs = built_release_dirs(
            root,
            target_dir,
            std::env::var_os("SystemDrive"),
            target,
            host,
        );
        let found = dirs
            .iter()
            .find(|d| d.join("bundle").join(&name).is_file())
            .cloned();
        let Some(exe_dir) = found else {
            bail!(
                "{} is missing: run `cargo xtask build --bundle{}` with the same --target-dir",
                dirs[0].join("bundle").join(&name).display(),
                target.map(|t| format!(" --target {t}")).unwrap_or_default()
            );
        };
        (exe_dir.join("bundle").join(&name), Some(exe_dir), triple)
    };
    // Before anything is touched: a user the scan cannot judge stops the run here.
    paths::Needles::of_this_user()?;
    let qnn = if triple == ARM64_WINDOWS {
        qnn_pins(root)?
    } else {
        Vec::new()
    };
    let notice = install_notice_text(root)?;
    let digest = sha256_file(&nsis)?;
    println!("{}", installer_digest_line(&nsis, &digest));
    let without_build = if exe_dir.is_none() {
        let lines = skipped_lines(triple);
        for line in &lines {
            println!("{line}");
        }
        lines.len()
    } else {
        0
    };
    let reference = exe_dir
        .as_deref()
        .map_or(Reference::Installer, Reference::Built);
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
    let result = check_saved(root, &nsis, &reference, triple, &qnn, &notice, &names);
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
    println!("{}", installer_summary(all, &nsis, &digest, without_build));
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
    reference: &Reference,
    triple: &str,
    qnn: &[QnnPin],
    notice: &str,
    names: &Installed,
) -> Result<bool> {
    let work = root.join("target").join("bundle-check");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let mut all = true;

    check_installer_paths(&mut all, nsis)?;
    let install = work.join("install");
    nsis_install(nsis, &install)?;
    // The installed exe says what the installer is for; its name must agree.
    let machine = read_pe(&install.join(APP_EXE)).map(|pe| pe.machine);
    let built_for = machine.as_ref().ok().copied().and_then(target_of_machine);
    verdict(
        &mut all,
        "nsis: the installed exe is for the installer's architecture",
        built_for == Some(triple),
        &match &machine {
            Ok(m) => format!("{APP_EXE} machine {m:#x} ({built_for:?}), expected {triple}"),
            Err(e) => format!("{e:#}"),
        },
    );
    check_install(&mut all, &install, reference, triple, qnn, notice)?;

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

    check_app_lists_itself(&mut all, &installed, &isolated, &names.reg)?;

    uninstall_and_judge(&mut all, &install, names)?;
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
    uninstall_and_judge(&mut all, &shared, names)?;
    verdict(
        &mut all,
        "nsis: uninstall keeps data in the install folder",
        database.is_file()
            && model.is_file()
            && webview.is_file()
            && !shared.join(APP_EXE).exists(),
        "a database, a model and the browser engine's files beside the exe survive; the exe does not",
    );

    check_update(&mut all, nsis, &work, names)?;

    Ok(all)
}

#[cfg(test)]
mod tests;
