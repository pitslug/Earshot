//! `cargo xtask build` (T2.6): release builds for the host or a cross target, with the
//! whisper.cpp (ggml) settings each target needs.
//!
//! - After the build, `llama.dll` and llama.cpp's `ggml*.dll` are copied next to
//!   `earshot-cli.exe` ([`crate::llama_dlls`], T6.21b), so the exe runs a local summary from
//!   any shell; the build runs with `--message-format=json-render-diagnostics` for that.
//!
//! - Without features, the whole workspace except xtask is built (`--workspace --exclude
//!   xtask`); with `--features`, the `earshot` CLI is built with `earshot-cli/<feature>` for
//!   each one (the CLI depends on every shipped crate, and workspace-wide feature flags need
//!   a package).
//! - `aarch64-pc-windows-msvc`: ggml refuses MSVC for ARM64 CPU code ("MSVC is not
//!   supported for ARM, use clang", `ggml/src/ggml-cpu/CMakeLists.txt`), so whisper.cpp is
//!   built with LLVM's clang-cl through the Ninja generator (as llama.cpp's
//!   `arm64-windows-llvm` preset does) for the [`ARM64_CPU_ARCH`] baseline
//!   (`GGML_CPU_ARM_ARCH`, also set in `.cargo/config.toml` `[env]` so that no ARM64 build
//!   falls back to plain ARMv8-A; `GGML_NATIVE` is off there). The Visual
//!   Studio generator cannot be used: the `cmake` crate always passes `-Thost=x64` to it, so
//!   no `ClangCL` toolset can be chosen from outside whisper-rs-sys. whisper-rs-sys forwards
//!   these `CMAKE_*`/`GGML_*` variables to whisper.cpp's CMake; the `cmake` crate reads
//!   `CMAKE_GENERATOR`. `ring` (TLS in `earshot-net`) needs `clang` itself for this target,
//!   so LLVM's `bin` directory ([`llvm_bin`]) goes first on PATH; `ninja` must be on PATH
//!   (`cargo xtask doctor`).
//! - Target directory: MSBuild's file tracker fails on paths over 260 characters, and ggml's
//!   Vulkan shader generator is a nested CMake project about 230 bytes below the profile
//!   root (`<target-dir>` for the host, `<target-dir>\<triple>` for a cross target): "FileTracker
//!   : error FTK1011", then "No CMAKE_C_COMPILER could be found". [`short_target_dir`] picks
//!   the first of `<repo>\target`, `%SystemDrive%\eb\host` and `%SystemDrive%\eb` whose
//!   profile root is at most [`MAX_PROFILE_ROOT_CHARS`] long (so an ARM64 build uses
//!   `%SystemDrive%\eb`); an explicit `--target-dir` wins. `%SystemDrive%\eb` is the only
//!   folder written at the drive root; `cargo xtask clean-build <name>` deletes `<eb>\<name>`.
//! - ARM64 with Vulkan from an x64 host: ggml builds its shader generator
//!   (`vulkan-shaders-gen`, run during the build) as a nested CMake project. When cross
//!   compiling it looks for a host compiler on PATH, which here is the ARM64-targeting `cl`
//!   of the build environment, so the generator cannot run on the host. The build writes a
//!   CMake toolchain file for the host's own MSVC x64 compiler and libraries
//!   ([`shaders_gen_toolchain`]) and passes it as `GGML_VULKAN_SHADERS_GEN_TOOLCHAIN`
//!   (forwarded to whisper.cpp's CMake like every `GGML_*` variable). whisper-rs-sys links
//!   `<VULKAN_SDK>\Lib\vulkan-1.lib` (x64), so `VULKAN_SDK` points at an ARM64 view of the SDK
//!   made of directory junctions ([`arm64_vulkan_sdk`]); rustflags are left alone.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::ci::cargo_command;
use crate::proc::run_with_timeout;

/// The ARM64 Windows target.
pub const ARM64_WINDOWS: &str = "aarch64-pc-windows-msvc";
/// ARM64 CPU baseline for whisper.cpp: ARMv8.2-A with the dot-product and int8 matrix
/// multiply extensions ggml's quantised kernels use (Snapdragon X and later; AGENTS.md).
pub const ARM64_CPU_ARCH: &str = "armv8.2-a+dotprod+i8mm";
/// Features the CLI can be built with.
pub const FEATURES: [&str; 5] = ["cuda", "onnx-cuda", "vulkan", "vulkan-summaries", "qnn"];
/// Longest profile root (`<target-dir>`, or `<target-dir>\<triple>` for a cross target;
/// bytes of the path, ASCII paths are one byte per character) that keeps ggml's nested
/// Vulkan build under the Windows path limit. Measured: the deepest nested file of a host
/// Vulkan build is 221 bytes below the target directory; ARM64 + Vulkan failed with
/// `C:\earshot-build\aarch64-pc-windows-msvc` (40 bytes) and built with
/// `C:\eb\aarch64-pc-windows-msvc` (29 bytes).
pub const MAX_PROFILE_ROOT_CHARS: usize = 30;

/// Length of the profile root of `dir` for `target`.
fn profile_root_len(dir: &Path, target: Option<&str>) -> usize {
    dir.as_os_str().len() + target.map_or(0, |t| t.len() + 1)
}

/// The one folder on the system drive that holds every short build directory
/// (`<system_drive>\eb`): `cargo xtask build` uses it and `<eb>\host`, sessions and lanes
/// use `<eb>\<name>`, and `cargo xtask clean-build <name>` deletes one. Nothing else is written to
/// the drive root.
#[must_use]
pub fn short_root(system_drive: Option<OsString>) -> PathBuf {
    let drive = system_drive.unwrap_or_else(|| OsString::from("C:"));
    PathBuf::from(format!(r"{}\eb", drive.to_string_lossy()))
}

/// The target directory for a build from `root` for `target` (`None`: the host): `explicit`
/// if given, else the first of `<root>\target`, `<eb>\host` and `<eb>` ([`short_root`])
/// whose profile root fits [`MAX_PROFILE_ROOT_CHARS`] (the last one if none does).
#[must_use]
pub fn short_target_dir(
    root: &Path,
    explicit: Option<&Path>,
    system_drive: Option<OsString>,
    target: Option<&str>,
) -> PathBuf {
    if let Some(d) = explicit {
        return d.to_path_buf();
    }
    let short = short_root(system_drive);
    let candidates = [root.join("target"), short.join("host"), short];
    let fits = |d: &PathBuf| profile_root_len(d, target) <= MAX_PROFILE_ROOT_CHARS;
    match candidates.iter().find(|d| fits(d)) {
        Some(d) => d.clone(),
        None => candidates[2].clone(),
    }
}

/// `cargo xtask clean-build <name>`: deletes the short build directory `<eb>\<name>`
/// ([`short_root`] on the system drive).
pub fn clean_build(name: &str) -> Result<bool> {
    let root = short_root(std::env::var_os("SystemDrive"));
    if clean_build_in(&root, name)? {
        println!("deleted {}", root.join(name).display());
    } else {
        println!("nothing to delete: {}", root.join(name).display());
    }
    Ok(true)
}

/// A build directory name: one path component of ASCII letters, digits, `-`, `_` and `.`,
/// not `.` or `..` and not starting with `.` (the prefix [`clean_build_in`] uses while
/// deleting).
pub fn check_build_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        bail!(
            "build name {name:?} must be one folder name of letters, digits, '-', '_' or '.' \
             (for example lane-t623c), not starting with '.'"
        )
    }
}

/// Deletes `root\<name>` and returns whether it existed. The folder is first renamed to
/// `root\.<name>.deleting`: Windows refuses to rename a folder with open files, so a
/// folder a running build is using is refused whole instead of being half deleted.
/// Junctions and symlinks inside are removed as links, never followed.
pub fn clean_build_in(root: &Path, name: &str) -> Result<bool> {
    check_build_name(name)?;
    let dir = root.join(name);
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
        Ok(_) => bail!("{} is not a plain folder; not deleting it", dir.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    }
    let doomed = root.join(format!(".{name}.deleting"));
    if doomed.exists() {
        std::fs::remove_dir_all(&doomed)
            .with_context(|| format!("removing an earlier {}", doomed.display()))?;
    }
    std::fs::rename(&dir, &doomed).with_context(|| {
        format!(
            "{} could not be moved aside, so it is probably in use by a running build; \
             nothing was deleted",
            dir.display()
        )
    })?;
    std::fs::remove_dir_all(&doomed).with_context(|| {
        format!(
            "{} was moved to {} but not fully deleted; run again to finish",
            dir.display(),
            doomed.display()
        )
    })?;
    Ok(true)
}

/// A CMake toolchain file for ggml's `vulkan-shaders-gen` on the build host: MSVC's x64
/// compiler `cl` and its libraries first on the linker's search path (the build
/// environment's `LIB` lists the target's libraries, which the linker searches after
/// `/LIBPATH`). Paths use forward slashes; the linker flags are one CMake string with each
/// directory quoted for the command line (they contain spaces).
#[must_use]
pub fn shaders_gen_toolchain(cl: &Path, lib_dirs: &[PathBuf]) -> String {
    let cmake = |p: &Path| p.display().to_string().replace('\\', "/");
    let libpaths = lib_dirs
        .iter()
        .map(|d| format!("/LIBPATH:\\\"{}\\\"", cmake(d)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "# Written by `cargo xtask build`: ggml's vulkan-shaders-gen runs on the build host.\n\
         set(CMAKE_C_COMPILER \"{cl}\")\n\
         set(CMAKE_CXX_COMPILER \"{cl}\")\n\
         set(CMAKE_EXE_LINKER_FLAGS_INIT \"{libpaths}\")\n\
         set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)\n\
         set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY NEVER)\n\
         set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE NEVER)\n",
        cl = cmake(cl)
    )
}

/// The directories of the ARM64 view of a Vulkan SDK: `(name, target)` for each directory
/// junction in the shim. whisper-rs-sys links `<VULKAN_SDK>\Lib\vulkan-1.lib`, which is the
/// x64 import library, so the shim's `Lib` is the SDK's `Lib-ARM64`; CMake's FindVulkan
/// reads `Include`, `Bin` (glslc, which runs on the host) and `Lib-ARM64` for an ARM64
/// target. No rustflags are set, so every rustflags source cargo reads stays in effect.
#[must_use]
pub fn arm64_vulkan_sdk_links(sdk: &Path) -> [(&'static str, PathBuf); 4] {
    [
        ("Include", sdk.join("Include")),
        ("Bin", sdk.join("Bin")),
        ("Lib", sdk.join("Lib-ARM64")),
        ("Lib-ARM64", sdk.join("Lib-ARM64")),
    ]
}

/// Creates (or repairs) the ARM64 view of `sdk` in `shim` as directory junctions
/// ([`arm64_vulkan_sdk_links`]) and returns `shim`. A junction needs no privilege.
///
/// # Errors
/// An SDK directory is missing, or a junction cannot be created.
pub fn arm64_vulkan_sdk(shim: &Path, sdk: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(shim).with_context(|| format!("create {}", shim.display()))?;
    for (name, target) in arm64_vulkan_sdk_links(sdk) {
        if !target.is_dir() {
            bail!(
                "no {} (the ARM64 build needs the Vulkan SDK's ARM64 component)",
                target.display()
            );
        }
        // A stale junction (another SDK version) is replaced.
        ensure_junction(&shim.join(name), &target)?;
    }
    Ok(shim.to_path_buf())
}

/// The folder of the latest Visual Studio with the MSVC x64 tools.
///
/// # Errors
/// vswhere cannot run or finds none.
pub fn visual_studio_install() -> Result<PathBuf> {
    let base = std::env::var_os("ProgramFiles(x86)").context("ProgramFiles(x86) is not set")?;
    let vswhere = Path::new(&base).join(r"Microsoft Visual Studio\Installer\vswhere.exe");
    let out = run_with_timeout(
        std::process::Command::new(&vswhere).args([
            "-latest",
            "-prerelease",
            "-products",
            "*",
            "-requires",
            crate::doctor::VC_X64,
            "-property",
            "installationPath",
        ]),
        Duration::from_secs(20),
    )
    .context("vswhere did not finish")?;
    if !out.success {
        bail!("vswhere failed: {}", out.describe());
    }
    Ok(PathBuf::from(
        out.stdout
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .context("vswhere found no Visual Studio with the MSVC x64 tools")?,
    ))
}

/// The host's MSVC x64 toolchain: `cl.exe` and the x64 library directories (MSVC, Windows
/// SDK `um` and `ucrt`) of the latest Visual Studio with the x64 tools.
///
/// # Errors
/// vswhere, the MSVC tools or the Windows SDK cannot be found.
pub fn host_msvc_x64() -> Result<(PathBuf, Vec<PathBuf>)> {
    let base = std::env::var_os("ProgramFiles(x86)").context("ProgramFiles(x86) is not set")?;
    let install = visual_studio_install()?;
    let version_file = install.join(r"VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt");
    let version = std::fs::read_to_string(&version_file)
        .with_context(|| format!("read {}", version_file.display()))?;
    let msvc = install.join(r"VC\Tools\MSVC").join(version.trim());
    let cl = msvc.join(r"bin\Hostx64\x64\cl.exe");
    if !cl.is_file() {
        bail!("no {}", cl.display());
    }
    let sdk = crate::doctor::windows_sdk().context("no Windows SDK (Windows Kits\\10\\Lib)")?;
    let kits = Path::new(&base).join(r"Windows Kits\10\Lib").join(sdk);
    let libs = vec![
        msvc.join(r"lib\x64"),
        kits.join(r"um\x64"),
        kits.join(r"ucrt\x64"),
    ];
    if let Some(missing) = libs.iter().find(|d| !d.is_dir()) {
        bail!("no {}", missing.display());
    }
    Ok((cl, libs))
}

/// Where a release build's crates are unpacked, below [`short_root`]. C and C++ asserts hold the
/// path of their source as `__FILE__`, and llama.cpp builds from the registry's copy, so with the
/// usual `CARGO_HOME` a shipped DLL names its builder (T9.1j, 300+ places in the CUDA set). Crates
/// are downloaded here once per machine.
pub const NEUTRAL_CARGO_HOME: &str = "cargo-home";

/// `CARGO_HOME` for a release build.
#[must_use]
pub fn neutral_cargo_home(system_drive: Option<OsString>) -> PathBuf {
    short_root(system_drive).join(NEUTRAL_CARGO_HOME)
}

/// What rustc must not write into a release build as it is on this machine: panic locations and
/// debug records. Workspace members already compile from relative paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeutralPaths {
    /// The toolchain as [`neutral_sysroot`] names it. The linker writes the path of every `std`
    /// rlib it reads into a program database, even a public-symbols one, so `--sysroot` is what
    /// keeps the real one out; the remap is for `std`'s generic code when `rust-src` is installed.
    pub sysroot: PathBuf,
    /// [`neutral_cargo_home`].
    pub cargo_home: PathBuf,
    /// The repo root.
    pub repo: PathBuf,
    /// The target directory, for `OUT_DIR` includes. May be inside the repo.
    pub target_dir: PathBuf,
}

impl NeutralPaths {
    /// rustc's flags. The last match wins, so the target directory comes after the repo that may
    /// hold it.
    #[must_use]
    pub fn rustc_flags(&self) -> Vec<String> {
        let mut flags = vec![format!("--sysroot={}", self.sysroot.display())];
        flags.extend(
            [
                (&self.sysroot, "/rust"),
                (&self.cargo_home, "/cargo"),
                (&self.repo, "/earshot"),
                (&self.target_dir, "/target"),
            ]
            .map(|(from, to)| format!("--remap-path-prefix={}={to}", from.display())),
        );
        flags
    }
}

/// Where a release build reads the toolchain at `real`: a junction below `short` with the
/// toolchain's own folder name, because cargo's rustc lives under the user's home folder.
///
/// # Errors
/// `real` has no folder name.
pub fn neutral_sysroot(short: &Path, real: &Path) -> Result<PathBuf> {
    let leaf = real
        .file_name()
        .with_context(|| format!("{} has no folder name", real.display()))?;
    let mut name = OsString::from("rust-");
    name.push(leaf);
    Ok(short.join(name))
}

/// Makes `link` a directory junction to `target`, replacing one that points elsewhere. A junction
/// needs no privilege, and removing one never touches its target.
fn ensure_junction(link: &Path, target: &Path) -> Result<()> {
    ensure_junction_with(link, target, |link, target| {
        let mut mklink = std::process::Command::new("cmd");
        mklink.args(["/c", "mklink", "/J"]).arg(link).arg(target);
        let out = run_with_timeout(&mut mklink, Duration::from_secs(20))
            .context("mklink did not finish")?;
        Ok((out.success, out.describe()))
    })
}

/// [`ensure_junction`], with `make` running `mklink` and saying whether it succeeded and what it
/// printed. A parallel build may make the same junction first, which fails this `mklink`: the
/// junction being what was wanted is what counts.
fn ensure_junction_with(
    link: &Path,
    target: &Path,
    make: impl FnOnce(&Path, &Path) -> Result<(bool, String)>,
) -> Result<()> {
    let points_there = || std::fs::read_link(link).is_ok_and(|t| t == target);
    if points_there() {
        return Ok(());
    }
    if link.symlink_metadata().is_ok() {
        std::fs::remove_dir(link).with_context(|| format!("remove {}", link.display()))?;
    }
    let (made, said) = make(link, target)?;
    ensure!(
        points_there() || (made && link.is_dir()),
        "cannot create junction {}: {said}",
        link.display(),
    );
    Ok(())
}

/// How `flags` reach rustc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RustFlags {
    /// `--config <this>`: cargo adds it to the config files' `build.rustflags`, which an
    /// environment value would replace (AGENTS.md).
    Config(String),
    /// `CARGO_ENCODED_RUSTFLAGS`: the shell already set rustflags, so cargo reads no config file's
    /// and ignores `--config`. Its own flags stay in front.
    Encoded(String),
}

/// Adds `flags` to a build without changing which other rustflags it gets, whichever source that
/// is. `encoded` and `plain` are `CARGO_ENCODED_RUSTFLAGS` and `RUSTFLAGS`, which cargo reads in
/// that order. A variable set to nothing still counts: cargo then takes no flags at all from
/// anywhere else, `--config` included, so only the variable can carry `flags`.
#[must_use]
pub fn rust_flags(flags: &[String], encoded: Option<&str>, plain: Option<&str>) -> RustFlags {
    const SEPARATOR: &str = "\u{1f}";
    let inherited: Option<Vec<&str>> = match (encoded, plain) {
        (Some(""), _) => Some(Vec::new()),
        (Some(e), _) => Some(e.split(SEPARATOR).collect()),
        (None, Some(p)) => Some(p.split_whitespace().collect()),
        (None, None) => None,
    };
    match inherited {
        Some(mut all) => {
            all.extend(flags.iter().map(String::as_str));
            RustFlags::Encoded(all.join(SEPARATOR))
        }
        None => {
            let items: Vec<String> = flags
                .iter()
                .map(|f| toml::Value::String(f.clone()).to_string())
                .collect();
            RustFlags::Config(format!("build.rustflags=[{}]", items.join(",")))
        }
    }
}

/// The toolchain cargo will use from `root`.
fn sysroot(root: &Path) -> Result<PathBuf> {
    let out = run_with_timeout(
        std::process::Command::new("rustc")
            .args(["--print", "sysroot"])
            .current_dir(root),
        Duration::from_secs(60),
    )
    .context("rustc --print sysroot did not finish")?;
    ensure!(out.success, "rustc --print sysroot: {}", out.describe());
    out.require_complete("rustc's sysroot")?;
    let line = out.stdout.trim();
    ensure!(!line.is_empty(), "rustc printed no sysroot");
    Ok(PathBuf::from(line))
}

/// Gives `cmd`, a release build with `args`, neutral paths (T9.1j).
fn apply_neutral_paths(
    cmd: &mut std::process::Command,
    root: &Path,
    target_dir: &Path,
) -> Result<()> {
    let short = short_root(std::env::var_os("SystemDrive"));
    let cargo_home = neutral_cargo_home(std::env::var_os("SystemDrive"));
    let real = sysroot(root)?;
    let toolchain = neutral_sysroot(&short, &real)?;
    std::fs::create_dir_all(&short).with_context(|| format!("create {}", short.display()))?;
    ensure_junction(&toolchain, &real)?;
    let flags = NeutralPaths {
        sysroot: toolchain,
        cargo_home: cargo_home.clone(),
        repo: root.to_path_buf(),
        target_dir: target_dir.to_path_buf(),
    }
    .rustc_flags();
    cmd.env("CARGO_HOME", cargo_home);
    let encoded = std::env::var("CARGO_ENCODED_RUSTFLAGS").ok();
    let plain = std::env::var("RUSTFLAGS").ok();
    match rust_flags(&flags, encoded.as_deref(), plain.as_deref()) {
        RustFlags::Config(value) => {
            cmd.args(["--config", &value]);
        }
        RustFlags::Encoded(value) => {
            cmd.env("CARGO_ENCODED_RUSTFLAGS", value);
        }
    }
    Ok(())
}

/// The feature an x64 app is built with when none is named (D23, T8.4ac).
pub const SHIPPED_X64_FEATURE: &str = "vulkan-summaries";
/// ONNX Runtime's CUDA provider, also in an x64 app (T8.4ad). It adds no import to the exe
/// (`bundle_shipped_exe_imports_no_cuda_dll`), and nothing runs on CUDA until the app chooses it.
pub const SHIPPED_X64_ONNX_FEATURE: &str = "onnx-cuda";

/// The features of the app for `target`: those named, else what ships. An x64 app built on x64
/// runs summaries on Vulkan, and Parakeet on CUDA if the app chooses it. An x64 app cross-built on
/// ARM64 has none, because llama.cpp's Vulkan
/// build needs Ninja and the x64 tools ([`needs_msvc_x64_env`]): it is for tests there, not for
/// release. ARM64 stays on the CPU for summaries.
#[must_use]
pub fn shipped_features(target: Option<&str>, features: &[String]) -> Vec<String> {
    if !features.is_empty() {
        return features.to_vec();
    }
    let builds_x64 = target != Some(ARM64_WINDOWS) && cfg!(target_arch = "x86_64");
    if builds_x64 {
        vec![
            SHIPPED_X64_FEATURE.to_owned(),
            SHIPPED_X64_ONNX_FEATURE.to_owned(),
        ]
    } else {
        Vec::new()
    }
}

/// The generations of the combined CUDA set (docs/cuda-install.md): 7.5 for RTX
/// 20 and GTX 16, 8.6 for RTX 30 and, by NVIDIA's binary rules, RTX 40, 12.0 for RTX 50.
pub const CUDA_SET_ARCHITECTURES: &str = "75-real;86-real;120a-real";
/// The toolkit it is built with: the newest cuDNN 9.28 lists, also with 13.5 installed beside it.
pub const CUDA_SET_TOOLKIT: &str = "v13.4";

/// The toolkit folder below `program_files`.
#[must_use]
pub fn cuda_set_toolkit(program_files: &Path) -> PathBuf {
    program_files
        .join("NVIDIA GPU Computing Toolkit")
        .join("CUDA")
        .join(CUDA_SET_TOOLKIT)
}

/// Cargo's arguments for the set.
#[must_use]
pub fn cuda_set_args(dir: &Path) -> Vec<String> {
    [
        "build",
        "--release",
        "-p",
        "earshot-llm-local",
        "--features",
        "cuda",
        "--target-dir",
    ]
    .map(Into::into)
    .into_iter()
    .chain([
        dir.display().to_string(),
        "--message-format=json-render-diagnostics".into(),
    ])
    .collect()
}

/// Builds the combined CUDA set of llama.cpp's DLLs (T8.4ada) into `<target-dir>\cuda-set` and
/// returns that folder. It holds none of NVIDIA's libraries: those come from NVIDIA (T8.4ae).
///
/// # Errors
/// No 13.4 toolkit, a failed build, or a build whose output cannot be read.
pub fn cuda_set(root: &Path, target_dir: Option<&Path>) -> Result<PathBuf> {
    let program_files = std::env::var_os("ProgramFiles").context("ProgramFiles is not set")?;
    let toolkit = cuda_set_toolkit(Path::new(&program_files));
    ensure!(
        toolkit.join("bin").join("nvcc.exe").is_file(),
        "no CUDA toolkit at {}: the set is built with {CUDA_SET_TOOLKIT}",
        toolkit.display()
    );
    let dir = target_dir.map_or_else(
        || short_root(std::env::var_os("SystemDrive")).join("cuda-set"),
        Path::to_path_buf,
    );
    let mut cmd = cargo_command();
    cmd.args(cuda_set_args(&dir)).current_dir(root);
    apply_msvc_x64_env(&mut cmd)?;
    let path = cmd
        .get_envs()
        .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
        .and_then(|(_, v)| v.map(OsString::from))
        .unwrap_or_default();
    let mut paths = vec![toolkit.join("bin")];
    paths.extend(std::env::split_paths(&path));
    cmd.env("PATH", std::env::join_paths(paths)?)
        .env("CUDA_PATH", &toolkit)
        .env("CMAKE_CUDA_ARCHITECTURES", CUDA_SET_ARCHITECTURES)
        .env(
            "CARGO_HOME",
            neutral_cargo_home(std::env::var_os("SystemDrive")),
        );
    eprintln!(
        "xtask build: the CUDA set, {CUDA_SET_ARCHITECTURES}, in {}",
        dir.display()
    );
    let out = run_with_timeout(&mut cmd, BUILD_TIMEOUT)
        .context("the CUDA set's build did not finish within the deadline")?;
    if !out.success {
        bail!("the CUDA set's build failed: {}", out.describe());
    }
    out.require_complete("the build's cargo messages")?;
    let bins = crate::llama_dlls::bin_dirs(&out.stdout);
    let [bin] = bins.as_slice() else {
        bail!("cargo reported {} llama.cpp builds, not one", bins.len());
    };
    let set = dir.join("cuda-set");
    std::fs::create_dir_all(&set).with_context(|| format!("create {}", set.display()))?;
    for dll in crate::llama_dlls::dlls_in(bin)? {
        let to = set.join(dll.file_name().context("a DLL without a name")?);
        std::fs::copy(&dll, &to).with_context(|| format!("copy {}", dll.display()))?;
    }
    ensure!(
        set.join("ggml-cuda.dll").is_file(),
        "the build left no ggml-cuda.dll"
    );
    Ok(set)
}

/// Whether a build needs Ninja and the Visual Studio x64 environment: llama.cpp's Vulkan shader
/// generator fails under the Visual Studio generator on x64 (T8.4c). ARM64 has its own
/// toolchain ([`target_env`]).
#[must_use]
pub fn needs_msvc_x64_env(target: Option<&str>, features: &[String]) -> bool {
    target != Some(ARM64_WINDOWS)
        && cfg!(target_arch = "x86_64")
        && features.iter().any(|f| f == "vulkan-summaries")
}

/// The variables `vcvars64.bat` sets that a build reads, from the output of `set` (names are
/// matched as Windows does, without regard to case).
#[must_use]
pub fn msvc_env_from_set(output: &str) -> Vec<(String, String)> {
    const KEYS: [&str; 11] = [
        "PATH",
        "INCLUDE",
        "LIB",
        "LIBPATH",
        "VCINSTALLDIR",
        "VCToolsInstallDir",
        "VCToolsVersion",
        "VSINSTALLDIR",
        "WindowsSdkDir",
        "WindowsSDKVersion",
        "UCRTVersion",
    ];
    output
        .lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| KEYS.iter().any(|want| want.eq_ignore_ascii_case(k)))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// Sets `CMAKE_GENERATOR=Ninja` and the Visual Studio x64 environment on `cmd`, read by running
/// `vcvars64.bat` from the Visual Studio install that has the x64 tools.
///
/// # Errors
/// Visual Studio or Ninja cannot be found, or `vcvars64.bat` fails.
pub fn apply_msvc_x64_env(cmd: &mut std::process::Command) -> Result<()> {
    let (cl, _) = host_msvc_x64()?;
    // <install>\VC\Tools\MSVC\<version>\bin\Hostx64\x64\cl.exe
    let install = cl
        .ancestors()
        .nth(8)
        .context("cl.exe is not in a Visual Studio install")?;
    let bat = install.join(r"VC\Auxiliary\Build\vcvars64.bat");
    if !bat.is_file() {
        bail!("no {}", bat.display());
    }
    let out = run_with_timeout(
        std::process::Command::new("cmd")
            .args(["/d", "/c", "call"])
            .arg(&bat)
            .args([">nul", "&&", "set"]),
        Duration::from_secs(60),
    )
    .context("vcvars64.bat did not finish")?;
    if !out.success {
        bail!("vcvars64.bat failed: {}", out.describe());
    }
    out.require_complete("vcvars64.bat's environment")?;
    let vars = msvc_env_from_set(&out.stdout);
    if !vars.iter().any(|(k, _)| k.eq_ignore_ascii_case("INCLUDE")) {
        bail!("vcvars64.bat set no INCLUDE");
    }
    for (k, v) in vars {
        cmd.env(k, v);
    }
    cmd.env("CMAKE_GENERATOR", "Ninja");
    Ok(())
}

/// Deadline for one build (a cold CUDA build of whisper.cpp takes about 4 minutes; a cold
/// ARM64 or Vulkan build compiles every ggml shader and kernel).
const BUILD_TIMEOUT: Duration = Duration::from_secs(90 * 60);

/// The cargo arguments for `target` (`None`: the host) and `features`.
///
/// # Errors
/// An unknown feature.
pub fn build_args(target: Option<&str>, features: &[String]) -> Result<Vec<String>> {
    let mut args: Vec<String> = vec!["build".into(), "--release".into()];
    if features.is_empty() {
        args.extend(["--workspace", "--exclude", "xtask"].map(Into::into));
    } else {
        for f in features {
            if !FEATURES.contains(&f.as_str()) {
                bail!("unknown feature `{f}` (known: {})", FEATURES.join(", "));
            }
        }
        args.extend(["-p".into(), "earshot-cli".into(), "--features".into()]);
        args.push(
            features
                .iter()
                .map(|f| format!("earshot-cli/{f}"))
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    if let Some(t) = target {
        args.extend(["--target".into(), t.to_owned()]);
    }
    Ok(args)
}

/// The cargo arguments of the desktop app's release build for `target` (`None`: the host), with
/// its front end compiled in: what `tauri build --no-bundle` runs. Without
/// `tauri/custom-protocol` the window would look for the dev server on localhost.
#[must_use]
pub fn app_build_args(target: Option<&str>, features: &[String]) -> Vec<String> {
    let mut wanted = vec!["tauri/custom-protocol".to_owned()];
    wanted.extend(features.iter().cloned());
    let mut args: Vec<String> = ["build", "--release", "-p", "earshot-desktop", "--features"]
        .map(Into::into)
        .to_vec();
    args.push(wanted.join(","));
    if let Some(t) = target {
        args.extend(["--target".into(), t.to_owned()]);
    }
    args
}

/// CMake settings that a build for `target` must not inherit: on an ARM64 host the shell usually
/// carries the ARM64 compiler and generator (`target_env`), which would compile an x64 build for
/// ARM64 and fail when it links against x64 libraries.
#[must_use]
pub fn inherited_env_to_clear(target: Option<&str>) -> &'static [&'static str] {
    if target == Some("x86_64-pc-windows-msvc") {
        &[
            "CMAKE_GENERATOR",
            "CMAKE_C_COMPILER",
            "CMAKE_CXX_COMPILER",
            "CMAKE_C_COMPILER_TARGET",
            "CMAKE_CXX_COMPILER_TARGET",
        ]
    } else {
        &[]
    }
}

/// Extra environment for `target`'s whisper.cpp build.
#[must_use]
pub fn target_env(target: Option<&str>) -> Vec<(&'static str, &'static str)> {
    match target {
        Some(ARM64_WINDOWS) => vec![
            ("CMAKE_GENERATOR", "Ninja"),
            ("CMAKE_C_COMPILER", "clang-cl"),
            ("CMAKE_CXX_COMPILER", "clang-cl"),
            ("CMAKE_C_COMPILER_TARGET", "aarch64-pc-windows-msvc"),
            ("CMAKE_CXX_COMPILER_TARGET", "aarch64-pc-windows-msvc"),
            ("GGML_CPU_ARM_ARCH", ARM64_CPU_ARCH),
            // The `cmake` crate replaces CMake's default C++ flags (which include `/EHsc`)
            // with cc's; under Ninja nothing adds it back, and whisper.cpp uses exceptions.
            ("CXXFLAGS_aarch64_pc_windows_msvc", "/EHsc"),
        ],
        _ => Vec::new(),
    }
}

/// LLVM's `bin` directory: `LIBCLANG_PATH` (doctor's LLVM/libclang row) when it holds
/// `clang.exe`, else the default install under Program Files.
#[must_use]
pub fn llvm_bin(
    libclang_path: Option<OsString>,
    program_files: Option<OsString>,
) -> Option<PathBuf> {
    let candidates = [
        libclang_path.map(PathBuf::from),
        program_files.map(|p| PathBuf::from(p).join("LLVM").join("bin")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|d| d.join("clang.exe").is_file())
}

/// Runs the build, printing cargo's output when it fails.
///
/// # Errors
/// An unknown feature, a failed or timed-out build.
pub fn run(
    root: &Path,
    target: Option<&str>,
    features: &[String],
    target_dir: Option<&Path>,
) -> Result<()> {
    let args = build_args(target, features)?;
    built(root, target, features, target_dir, args, "earshot-cli").map(|_| ())
}

/// Builds the desktop app as it ships for `target` (`None`: the host), front end first, llama.cpp's
/// DLLs beside the executable, and returns its path.
///
/// # Errors
/// A failed build of the front end or the app.
pub fn app(
    root: &Path,
    target: Option<&str>,
    features: &[String],
    target_dir: Option<&Path>,
) -> Result<PathBuf> {
    let features = shipped_features(target, features);
    let pnpm_args = ["-r", "build"];
    let mut pnpm = crate::ui::pnpm_command(root, &pnpm_args);
    let out = run_with_timeout(&mut pnpm, BUILD_TIMEOUT)
        .context("the front end's build did not finish within the deadline")?;
    crate::ui::evaluate(&pnpm_args, &out)?;
    built(
        root,
        target,
        &features,
        target_dir,
        app_build_args(target, &features),
        "earshot",
    )
}

/// Removes every 0-byte `.obj` under `<profile>/build/<package>/out`, returning what it removed.
/// A native compile that stopped part-way can leave one behind; MSBuild then treats it as up to
/// date and every later build fails at the link with LNK1136 (T7.6a, ggml-cuda's `sumrows.obj`).
/// A compiler never leaves a valid object empty, so removing one only makes it compile again.
///
/// # Errors
/// An empty object that cannot be removed. An unreadable folder is skipped: the build reports
/// anything that matters.
pub fn remove_empty_objects(profile: &Path) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let Ok(packages) = std::fs::read_dir(profile.join("build")) else {
        return Ok(removed);
    };
    let mut pending: Vec<PathBuf> = packages.flatten().map(|p| p.path().join("out")).collect();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("obj"))
                && entry.metadata().is_ok_and(|m| m.len() == 0)
            {
                std::fs::remove_file(&path)
                    .with_context(|| format!("remove the empty object {}", path.display()))?;
                removed.push(path);
            }
        }
    }
    removed.sort();
    Ok(removed)
}

/// The first line of a failed build's output that names a compiler, linker or MSBuild error, which
/// cargo's own "command did not execute successfully" hides among the rest.
#[must_use]
pub fn first_native_error(output: &str) -> Option<&str> {
    output.lines().map(str::trim).find(|line| {
        line.contains("fatal error")
            || line.split_whitespace().any(|w| {
                let code = w.trim_end_matches(':');
                let letters = code.trim_end_matches(|c: char| c.is_ascii_digit());
                w.ends_with(':')
                    && letters != code
                    && matches!(letters, "C" | "LNK" | "MSB" | "FTK")
            }) && line.contains("error")
    })
}

/// Runs `cargo <args>` with `target`'s environment and returns the executable `exe_name`.
fn built(
    root: &Path,
    target: Option<&str>,
    features: &[String],
    target_dir: Option<&Path>,
    mut args: Vec<String>,
    exe_name: &str,
) -> Result<PathBuf> {
    let dir = short_target_dir(root, target_dir, std::env::var_os("SystemDrive"), target);
    args.extend(["--target-dir".into(), dir.display().to_string()]);
    // The executable and llama.cpp's DLL directory come from cargo's JSON messages.
    args.push("--message-format=json-render-diagnostics".into());
    let mut cmd = cargo_command();
    cmd.args(&args).current_dir(root);
    apply_neutral_paths(&mut cmd, root, &dir)?;
    for k in inherited_env_to_clear(target) {
        cmd.env_remove(k);
    }
    for (k, v) in target_env(target) {
        cmd.env(k, v);
    }
    // `.cargo/config.toml`'s C/C++ release flags are MSVC's with the dynamic CRT; any other
    // toolchain or the static CRT gets its own (T2.2c).
    let rustflags = [
        std::env::var("RUSTFLAGS").unwrap_or_default(),
        std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default(),
    ]
    .join(" ");
    for (k, v) in crate::native_flags::release_flags_env(
        crate::native_flags::is_msvc(target),
        crate::native_flags::crt_static(&rustflags),
    ) {
        cmd.env(k, v);
    }
    if needs_msvc_x64_env(target, features) {
        apply_msvc_x64_env(&mut cmd)?;
    }
    if target == Some(ARM64_WINDOWS) {
        // Checked here: a missing ninja otherwise fails deep inside whisper.cpp's CMake.
        let ninja = std::process::Command::new("ninja")
            .arg("--version")
            .output();
        if !ninja.is_ok_and(|o| o.status.success()) {
            bail!(
                "the ARM64 build needs ninja on PATH (winget install Ninja-build.Ninja; \
                 `cargo xtask doctor` checks it)"
            );
        }
        let bin = llvm_bin(
            std::env::var_os("LIBCLANG_PATH"),
            std::env::var_os("ProgramFiles"),
        )
        .context(r"no clang.exe in LIBCLANG_PATH or Program Files\LLVM\bin: install LLVM")?;
        let mut path = vec![bin];
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        cmd.env("PATH", std::env::join_paths(path)?);
        if features.iter().any(|f| f == "vulkan") && cfg!(target_arch = "x86_64") {
            let (cl, libs) = host_msvc_x64()
                .context("the ARM64 Vulkan build needs the host's MSVC x64 tools")?;
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            let file = dir.join("vulkan-shaders-gen-host.cmake");
            std::fs::write(&file, shaders_gen_toolchain(&cl, &libs))
                .with_context(|| format!("write {}", file.display()))?;
            eprintln!(
                "xtask build: GGML_VULKAN_SHADERS_GEN_TOOLCHAIN={}",
                file.display()
            );
            cmd.env("GGML_VULKAN_SHADERS_GEN_TOOLCHAIN", file);
            // whisper-rs-sys links `<VULKAN_SDK>\Lib\vulkan-1.lib`, the x64 import library:
            // point VULKAN_SDK at an ARM64 view of the SDK (`Lib` is `Lib-ARM64`).
            let sdk = std::env::var_os("VULKAN_SDK").context("VULKAN_SDK is not set")?;
            let shim = arm64_vulkan_sdk(&dir.join("vulkan-sdk-arm64"), Path::new(&sdk))?;
            eprintln!("xtask build: VULKAN_SDK={}", shim.display());
            cmd.env("VULKAN_SDK", shim);
        }
    }
    let profile = target
        .map_or_else(|| dir.clone(), |t| dir.join(t))
        .join("release");
    for path in remove_empty_objects(&profile)? {
        eprintln!(
            "xtask build: removed {}, left empty by a build that stopped",
            path.display()
        );
    }
    eprintln!("xtask build: cargo {}", args.join(" "));
    let out = run_with_timeout(&mut cmd, BUILD_TIMEOUT)
        .context("the build did not finish within the deadline")?;
    // A failed build is judged on the exit status (an incomplete capture is marked in the
    // message by `describe`). A successful build is also judged on its captured JSON
    // messages (the executable and llama.cpp's DLLs), which must then be complete.
    if !out.success {
        // Cargo prints a failed build script's output inside the JSON messages on stdout too.
        let native = first_native_error(&out.stderr)
            .or_else(|| first_native_error(&out.stdout))
            .map(|line| format!("first native error: {line}\n"))
            .unwrap_or_default();
        bail!("the build failed: {native}{}", out.describe());
    }
    let custom_dir = (dir != root.join("target")).then_some(dir.as_path());
    let n = crate::native_flags::check_optimised(
        &profile,
        &crate::native_flags::clean_args(target, custom_dir),
    )?;
    // llama.cpp's DLLs beside earshot-cli.exe (T6.21b): read from the captured messages, so
    // they must be complete.
    out.require_complete("the build's cargo messages")?;
    let exe = crate::accept::m6::executable_named(&out.stdout, exe_name)
        .with_context(|| format!("cargo reported no {exe_name} executable"))?;
    let dlls = crate::llama_dlls::install_beside(&out.stdout, &exe)?;
    eprintln!(
        "xtask build: {} with {} beside it",
        exe.display(),
        dlls.join(", ")
    );
    eprintln!("xtask build: ok ({n} C/C++ builds optimised)");
    Ok(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_cuda_set_targets_rtx_20_to_50_with_the_toolkit_cudnn_lists() {
        assert_eq!(CUDA_SET_ARCHITECTURES, "75-real;86-real;120a-real");
        assert!(cuda_set_toolkit(Path::new("C:/PF")).ends_with("CUDA/v13.4"));
        let args = cuda_set_args(Path::new("C:/eb/cu3"));
        assert!(args.windows(2).any(|w| w == ["-p", "earshot-llm-local"]));
        assert!(args.windows(2).any(|w| w == ["--features", "cuda"]));
        // Only llama.cpp: whisper's CUDA code stays out of everything that ships.
        assert!(
            !args
                .iter()
                .any(|a| a.contains("earshot-desktop") || a.contains("whisper"))
        );
    }

    fn neutral() -> NeutralPaths {
        NeutralPaths {
            sysroot: PathBuf::from(r"C:\eb\rust-1.99"),
            cargo_home: PathBuf::from(r"C:\eb\cargo-home"),
            repo: PathBuf::from(r"C:\Users\Build Person\Earshot"),
            target_dir: PathBuf::from(r"C:\Users\Build Person\Earshot\target"),
        }
    }

    #[test]
    fn build_release_paths_are_remapped_with_the_target_dir_after_the_repo_that_holds_it() {
        let flags = neutral().rustc_flags();
        assert_eq!(
            flags,
            [
                r"--sysroot=C:\eb\rust-1.99",
                r"--remap-path-prefix=C:\eb\rust-1.99=/rust",
                r"--remap-path-prefix=C:\eb\cargo-home=/cargo",
                r"--remap-path-prefix=C:\Users\Build Person\Earshot=/earshot",
                r"--remap-path-prefix=C:\Users\Build Person\Earshot\target=/target",
            ]
        );
        assert_eq!(
            neutral_cargo_home(Some("D:".into())),
            PathBuf::from(r"D:\eb\cargo-home")
        );
        assert_eq!(
            neutral_sysroot(
                Path::new(r"C:\eb"),
                Path::new(r"C:\Users\x\.rustup\toolchains\1.99.1-x86_64-pc-windows-msvc")
            )
            .unwrap(),
            PathBuf::from(r"C:\eb\rust-1.99.1-x86_64-pc-windows-msvc")
        );
    }

    #[test]
    fn build_release_paths_join_rustflags_from_config_files_when_the_shell_sets_none() {
        let flags = neutral().rustc_flags();
        let RustFlags::Config(value) = rust_flags(&flags, None, None) else {
            panic!("no rustflags in the shell");
        };
        // What cargo reads: one TOML key with the flags exact, backslashes and spaces too.
        let parsed: toml::Table = value.parse().unwrap();
        let got = parsed["build"]["rustflags"].as_array().unwrap();
        let got: Vec<&str> = got.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(got, flags);
    }

    #[test]
    fn build_release_paths_survive_a_rustflags_variable_set_to_nothing() {
        // Cargo reads such a variable as "no flags" and ignores `--config`, as measured.
        let flags = neutral().rustc_flags();
        let ours = RustFlags::Encoded(flags.join("\u{1f}"));
        for (encoded, plain) in [
            (Some(""), None),
            (None, Some("")),
            (None, Some("   ")),
            (Some(""), Some("")),
            (Some(""), Some("-Cignored")),
        ] {
            assert_eq!(
                rust_flags(&flags, encoded, plain),
                ours,
                "{encoded:?} {plain:?}"
            );
        }
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "directory junctions are Windows-only")]
    fn build_junction_made_first_by_a_parallel_build_is_not_a_failure() {
        let root = std::env::temp_dir().join(format!("earshot-junction-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let target = root.join("toolchain");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let link = root.join("link");
        // The other build wins between the look and this `mklink`, which then fails.
        ensure_junction_with(&link, &target, |link, target| {
            ensure_junction(link, target).unwrap();
            Ok((false, "already exists".to_owned()))
        })
        .unwrap();
        assert!(std::fs::read_link(&link).is_ok_and(|t| t == target));
        // The negative controls: another build's different junction, and no junction at all.
        let other = root.join("other");
        let e = ensure_junction_with(&other, &target, |link, _| {
            ensure_junction(link, &elsewhere).unwrap();
            Ok((false, "already exists".to_owned()))
        })
        .unwrap_err();
        assert!(e.to_string().contains("already exists"), "{e}");
        let none = root.join("none");
        assert!(
            ensure_junction_with(&none, &target, |_, _| Ok((false, "denied".to_owned()))).is_err()
        );
        std::fs::remove_dir(&link).unwrap();
        std::fs::remove_dir(&other).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn build_release_paths_join_rustflags_from_the_shell_in_the_variable_cargo_reads() {
        let flags = vec!["--remap-path-prefix=C:\\a b=/x".to_owned()];
        let one = "\u{1f}";
        assert_eq!(
            rust_flags(&flags, None, Some("-C target-feature=+crt-static")),
            RustFlags::Encoded(["-C", "target-feature=+crt-static", &flags[0]].join(one))
        );
        // Cargo prefers the encoded variable, so its value is the one that counts.
        assert_eq!(
            rust_flags(&flags, Some(&format!("-Ca=1{one}-Cb=2")), Some("-Cignored")),
            RustFlags::Encoded(["-Ca=1", "-Cb=2", &flags[0]].join(one))
        );
    }

    #[test]
    fn build_an_x64_app_ships_with_vulkan_summaries_unless_told_otherwise() {
        let on_x64 = cfg!(target_arch = "x86_64");
        let vulkan = vec![
            SHIPPED_X64_FEATURE.to_owned(),
            SHIPPED_X64_ONNX_FEATURE.to_owned(),
        ];
        let none: Vec<String> = Vec::new();
        let expected = if on_x64 { &vulkan } else { &none };
        assert_eq!(&shipped_features(None, &[]), expected);
        assert_eq!(
            &shipped_features(Some("x86_64-pc-windows-msvc"), &[]),
            expected
        );
        // ARM64 stays on the CPU for summaries, and a named feature is what it says.
        assert!(shipped_features(Some(ARM64_WINDOWS), &[]).is_empty());
        let cuda = vec!["cuda".to_owned()];
        assert_eq!(shipped_features(None, &cuda), cuda);
        assert_eq!(shipped_features(Some(ARM64_WINDOWS), &cuda), cuda);
    }

    #[test]
    fn build_only_vulkan_summaries_on_x64_need_the_visual_studio_environment() {
        let f = |s: &[&str]| s.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        let vs = f(&["vulkan-summaries"]);
        assert_eq!(needs_msvc_x64_env(None, &vs), cfg!(target_arch = "x86_64"));
        assert!(!needs_msvc_x64_env(Some(ARM64_WINDOWS), &vs));
        assert!(!needs_msvc_x64_env(None, &f(&["vulkan"])));
        assert!(!needs_msvc_x64_env(None, &f(&["cuda"])));
        assert!(!needs_msvc_x64_env(None, &[]));
    }

    #[test]
    fn build_reads_the_visual_studio_environment_from_set() {
        let set = "ALLUSERSPROFILE=C:\\ProgramData\r\nINCLUDE=C:\\inc;C:\\more\r\nLib=C:\\lib\r\n\
                   Path=C:\\VS;C:\\a=b\r\nno equals sign\r\nLIBPATH=\r\n";
        let vars = msvc_env_from_set(set);
        let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("INCLUDE"), Some(r"C:\inc;C:\more"));
        assert_eq!(get("Lib"), Some(r"C:\lib"));
        // Only the first `=` splits, and a value may be empty.
        assert_eq!(get("Path"), Some(r"C:\VS;C:\a=b"));
        assert_eq!(get("LIBPATH"), Some(""));
        assert_eq!(get("ALLUSERSPROFILE"), None);
    }

    #[test]
    fn build_removes_only_empty_objects_in_native_build_folders() {
        let root = std::env::temp_dir().join(format!("earshot-empty-obj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let deep = root.join("build/llama-cpp-sys-2-abc/out/build/ggml/Release");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("empty.obj"), b"").unwrap();
        std::fs::write(deep.join("EMPTY2.OBJ"), b"").unwrap();
        std::fs::write(deep.join("good.obj"), b"x").unwrap();
        std::fs::write(deep.join("empty.log"), b"").unwrap();
        // Outside a package's `out`: not ours to touch.
        let outside = root.join("deps");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("other.obj"), b"").unwrap();

        let removed = remove_empty_objects(&root).unwrap();

        assert_eq!(removed.len(), 2, "{removed:?}");
        assert!(!deep.join("empty.obj").exists());
        assert!(!deep.join("EMPTY2.OBJ").exists());
        assert!(deep.join("good.obj").exists());
        assert!(deep.join("empty.log").exists());
        assert!(outside.join("other.obj").exists());
        assert!(remove_empty_objects(&root).unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn build_empty_object_sweep_accepts_a_profile_never_built() {
        let root = std::env::temp_dir().join("earshot-empty-obj-never-built");
        assert!(remove_empty_objects(&root).unwrap().is_empty());
    }

    #[test]
    fn build_names_the_first_native_error() {
        let linker = "  ggml-cpu.vcxproj -> ggml-cpu.dll\n\
            ggml-cuda.dir\\Release\\sumrows.obj : fatal error LNK1136: invalid or corrupt file\n\
            later.obj : fatal error LNK1000: other";
        assert!(first_native_error(linker).unwrap().contains("LNK1136"));
        let compiler = "a.cpp(3,1): error C2065: 'x': undeclared identifier";
        assert!(first_native_error(compiler).unwrap().contains("C2065"));
        let msbuild = "x.targets(5,5): error MSB3073: The command exited with code 1.";
        assert!(first_native_error(msbuild).unwrap().contains("MSB3073"));
        for clean in [
            "warning C4100: unreferenced parameter",
            "command did not execute successfully, got: exit code: 1",
            "0 Error(s)",
        ] {
            assert_eq!(first_native_error(clean), None, "{clean}");
        }
    }

    #[test]
    fn build_without_features_is_the_workspace() {
        let a = build_args(None, &[]).unwrap();
        assert_eq!(
            a,
            ["build", "--release", "--workspace", "--exclude", "xtask"]
        );
        let arm = build_args(Some(ARM64_WINDOWS), &[]).unwrap();
        assert!(arm.ends_with(&["--target".into(), ARM64_WINDOWS.into()]));
    }

    #[test]
    fn build_for_x64_does_not_inherit_the_arm64_compiler() {
        let x = inherited_env_to_clear(Some("x86_64-pc-windows-msvc"));
        assert!(x.contains(&"CMAKE_C_COMPILER_TARGET") && x.contains(&"CMAKE_GENERATOR"));
        assert!(inherited_env_to_clear(None).is_empty());
        assert!(inherited_env_to_clear(Some(ARM64_WINDOWS)).is_empty());
        // What is cleared is never what the target sets.
        assert!(target_env(Some("x86_64-pc-windows-msvc")).is_empty());
    }

    #[test]
    fn build_app_compiles_its_front_end_in() {
        let a = app_build_args(None, &[]);
        assert!(a.windows(2).any(|w| w == ["-p", "earshot-desktop"]));
        assert!(
            a.windows(2)
                .any(|w| w == ["--features", "tauri/custom-protocol"])
        );
        assert!(!a.iter().any(|x| x == "--target"));
        let x = app_build_args(Some("x86_64-pc-windows-msvc"), &[]);
        assert!(x.ends_with(&["--target".into(), "x86_64-pc-windows-msvc".into()]));
        let c = app_build_args(None, &["cuda".into()]);
        assert!(
            c.windows(2)
                .any(|w| w == ["--features", "tauri/custom-protocol,cuda"])
        );
    }

    #[test]
    fn build_features_go_to_the_cli() {
        let a = build_args(None, &["vulkan".into(), "cuda".into()]).unwrap();
        assert!(a.windows(4).any(|w| w
            == [
                "-p",
                "earshot-cli",
                "--features",
                "earshot-cli/vulkan,earshot-cli/cuda"
            ]));
        assert!(build_args(None, &["metal".into()]).is_err());
        // T8.3: the QNN plugin for the Snapdragon NPU, built for ARM64.
        let q = build_args(Some(ARM64_WINDOWS), &["qnn".into()]).unwrap();
        assert!(
            q.windows(4)
                .any(|w| w == ["-p", "earshot-cli", "--features", "earshot-cli/qnn"])
        );
    }

    #[test]
    fn build_llvm_bin_needs_clang_exe() {
        let root = std::env::temp_dir().join(format!("earshot-build-llvm-{}", std::process::id()));
        let lib = root.join("libclang-only");
        let pf = root.join("pf");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(pf.join("LLVM").join("bin")).unwrap();
        // LIBCLANG_PATH without clang.exe is skipped; Program Files/LLVM/bin without it too.
        assert_eq!(
            llvm_bin(Some(lib.clone().into()), Some(pf.clone().into())),
            None
        );
        std::fs::write(pf.join("LLVM").join("bin").join("clang.exe"), b"").unwrap();
        assert_eq!(
            llvm_bin(Some(lib.clone().into()), Some(pf.clone().into())),
            Some(pf.join("LLVM").join("bin"))
        );
        std::fs::write(lib.join("clang.exe"), b"").unwrap();
        assert_eq!(
            llvm_bin(Some(lib.clone().into()), Some(pf.into())),
            Some(lib)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn build_target_dir_is_short_enough_for_the_vulkan_shader_build() {
        let deep = Path::new(r"C:\Users\Someone\Documents\Projects\Earshot");
        assert_eq!(
            short_target_dir(deep, None, Some("D:".into()), None),
            PathBuf::from(r"D:\eb\host")
        );
        let shallow = Path::new(r"C:\e");
        assert_eq!(
            short_target_dir(shallow, None, None, None),
            shallow.join("target")
        );
        let explicit = Path::new(r"X:\t");
        assert_eq!(
            short_target_dir(deep, Some(explicit), None, Some(ARM64_WINDOWS)),
            explicit
        );
        // A cross target adds `\<triple>` to the profile root: a folder below `eb` is too
        // deep for ARM64, so `eb` itself is used.
        assert_eq!(
            short_target_dir(deep, None, Some("C:".into()), Some(ARM64_WINDOWS)),
            PathBuf::from(r"C:\eb")
        );
        assert_eq!(
            short_target_dir(shallow, None, None, Some(ARM64_WINDOWS)),
            PathBuf::from(r"C:\eb")
        );
        for target in [None, Some(ARM64_WINDOWS)] {
            let d = short_target_dir(deep, None, Some("C:".into()), target);
            assert!(
                profile_root_len(&d, target) <= MAX_PROFILE_ROOT_CHARS,
                "{d:?}"
            );
        }
        assert!(
            profile_root_len(Path::new(r"C:\earshot-build"), Some(ARM64_WINDOWS))
                > MAX_PROFILE_ROOT_CHARS
        );
        // Every default lives under the one short root.
        for target in [None, Some(ARM64_WINDOWS)] {
            let d = short_target_dir(deep, None, Some("C:".into()), target);
            assert!(d.starts_with(short_root(Some("C:".into()))), "{d:?}");
        }
    }

    #[test]
    fn build_clean_build_deletes_only_the_named_folder() {
        let root = std::env::temp_dir().join(format!("earshot-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(r"lane-a\release\deep")).unwrap();
        std::fs::write(root.join(r"lane-a\release\deep\x.rlib"), b"x").unwrap();
        std::fs::create_dir_all(root.join(r"lane-b\release")).unwrap();
        std::fs::write(root.join("CACHEDIR.TAG"), b"x").unwrap();

        assert!(clean_build_in(&root, "lane-a").unwrap());
        assert!(!root.join("lane-a").exists());
        assert!(!root.join(".lane-a.deleting").exists());
        assert!(root.join(r"lane-b\release").is_dir());
        assert!(root.join("CACHEDIR.TAG").is_file());
        // A missing folder is nothing to delete.
        assert!(!clean_build_in(&root, "lane-a").unwrap());
        // A file is not a build folder.
        assert!(clean_build_in(&root, "CACHEDIR.TAG").is_err());
        assert!(root.join("CACHEDIR.TAG").is_file());

        // An open file inside makes the rename fail on Windows: nothing is deleted.
        #[cfg(windows)]
        {
            std::fs::write(root.join(r"lane-b\release\open.bin"), b"x").unwrap();
            let file = std::fs::File::open(root.join(r"lane-b\release\open.bin")).unwrap();
            assert!(clean_build_in(&root, "lane-b").is_err());
            assert!(root.join(r"lane-b\release\open.bin").is_file());
            drop(file);
            assert!(clean_build_in(&root, "lane-b").unwrap());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn build_clean_build_refuses_names_that_leave_the_short_root() {
        for bad in [
            "", ".", "..", r"..\x", "a/b", r"a\b", "C:", ".hidden", "a b",
        ] {
            assert!(check_build_name(bad).is_err(), "{bad:?}");
        }
        for good in ["lane-t623c", "main-cu", "vk", "host", "x.1_2"] {
            check_build_name(good).unwrap();
        }
    }

    #[test]
    fn build_arm64_vulkan_sdk_view_links_the_arm64_libraries() {
        let sdk = Path::new(r"C:\Program Files\VulkanSDK\1.4");
        let links = arm64_vulkan_sdk_links(sdk);
        let get = |n: &str| links.iter().find(|(k, _)| *k == n).map(|(_, t)| t.clone());
        assert_eq!(get("Lib"), Some(sdk.join("Lib-ARM64")));
        assert_eq!(get("Lib-ARM64"), Some(sdk.join("Lib-ARM64")));
        assert_eq!(get("Include"), Some(sdk.join("Include")));
        // glslc runs on the build host: the x64 Bin.
        assert_eq!(get("Bin"), Some(sdk.join("Bin")));
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "directory junctions are Windows-only")]
    fn build_arm64_vulkan_sdk_view_is_made_of_junctions_and_repaired() {
        let root = std::env::temp_dir().join(format!("earshot-vk-shim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sdk = root.join("sdk");
        for d in ["Include", "Bin", "Lib", "Lib-ARM64"] {
            std::fs::create_dir_all(sdk.join(d)).unwrap();
        }
        std::fs::write(sdk.join("Lib-ARM64").join("vulkan-1.lib"), b"arm64").unwrap();
        std::fs::write(sdk.join("Lib").join("vulkan-1.lib"), b"x64").unwrap();
        let shim = root.join("shim");
        arm64_vulkan_sdk(&shim, &sdk).unwrap();
        assert_eq!(
            std::fs::read(shim.join("Lib").join("vulkan-1.lib")).unwrap(),
            b"arm64"
        );
        // Idempotent, and a junction to another SDK is replaced without touching its target.
        let other = root.join("other");
        std::fs::create_dir_all(other.join("Lib-ARM64")).unwrap();
        std::fs::write(other.join("Lib-ARM64").join("keep"), b"k").unwrap();
        for d in ["Include", "Bin"] {
            std::fs::create_dir_all(other.join(d)).unwrap();
        }
        arm64_vulkan_sdk(&shim, &other).unwrap();
        assert!(shim.join("Lib").join("keep").is_file());
        arm64_vulkan_sdk(&shim, &sdk).unwrap();
        assert_eq!(
            std::fs::read(shim.join("Lib").join("vulkan-1.lib")).unwrap(),
            b"arm64"
        );
        assert!(other.join("Lib-ARM64").join("keep").is_file());
        // Removing the shim never follows a junction into the SDK: a file reached only
        // through a junction (`shim\Include` -> `sdk\Include`) survives.
        std::fs::write(sdk.join("Include").join("marker.h"), b"m").unwrap();
        assert!(shim.join("Include").join("marker.h").is_file());
        std::fs::remove_dir_all(&shim).unwrap();
        assert!(!shim.exists());
        assert!(sdk.join("Include").join("marker.h").is_file());
        assert!(sdk.join("Lib-ARM64").join("vulkan-1.lib").is_file());
        // No ARM64 component: a clear error, on a fresh shim.
        std::fs::remove_dir_all(sdk.join("Lib-ARM64")).unwrap();
        let e = arm64_vulkan_sdk(&root.join("shim2"), &sdk).unwrap_err();
        assert!(e.to_string().contains("ARM64 component"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn build_shaders_gen_toolchain_uses_the_host_compiler_and_libraries() {
        let cl = Path::new(r"C:\VS\VC\Tools\MSVC\14.50\bin\Hostx64\x64\cl.exe");
        let libs = [
            PathBuf::from(r"C:\VS\VC\Tools\MSVC\14.50\lib\x64"),
            PathBuf::from(r"C:\Kits\Lib\10.0.26100.0\um\x64"),
        ];
        let t = shaders_gen_toolchain(cl, &libs);
        assert!(t.contains(
            "set(CMAKE_C_COMPILER \"C:/VS/VC/Tools/MSVC/14.50/bin/Hostx64/x64/cl.exe\")"
        ));
        assert!(t.contains(
            "set(CMAKE_CXX_COMPILER \"C:/VS/VC/Tools/MSVC/14.50/bin/Hostx64/x64/cl.exe\")"
        ));
        assert!(t.contains(
            r#"set(CMAKE_EXE_LINKER_FLAGS_INIT "/LIBPATH:\"C:/VS/VC/Tools/MSVC/14.50/lib/x64\" /LIBPATH:\"C:/Kits/Lib/10.0.26100.0/um/x64\"")"#
        ));
        // Only escaped quotes use a backslash; paths are CMake's forward-slash form.
        assert!(!t.replace("\\\"", "").contains('\\'), "{t}");
    }

    #[test]
    fn build_arm64_uses_clang_and_the_baseline() {
        let env = target_env(Some(ARM64_WINDOWS));
        assert!(env.contains(&("CMAKE_GENERATOR", "Ninja")));
        assert!(env.contains(&("CMAKE_C_COMPILER", "clang-cl")));
        assert!(env.contains(&("CMAKE_C_COMPILER_TARGET", ARM64_WINDOWS)));
        assert!(env.contains(&("CXXFLAGS_aarch64_pc_windows_msvc", "/EHsc")));
        assert!(env.contains(&("GGML_CPU_ARM_ARCH", ARM64_CPU_ARCH)));
        assert!(target_env(None).is_empty());
        assert!(target_env(Some("x86_64-pc-windows-msvc")).is_empty());
    }

    // A plain `cargo build --target aarch64-pc-windows-msvc` (or a native build on the
    // Surface) must not silently produce a baseline-only ARMv8-A whisper.cpp: the workspace
    // `[env]` carries the same ARM baseline as `cargo xtask build`.
    #[test]
    fn build_arm64_baseline_is_in_the_workspace_cargo_env() {
        let path = crate::repo_root().join(".cargo").join("config.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        let config: toml::Table = text.parse().unwrap();
        assert_eq!(
            config["env"]
                .get("GGML_CPU_ARM_ARCH")
                .and_then(|v| v.as_str()),
            Some(ARM64_CPU_ARCH),
            "{} [env] must set GGML_CPU_ARM_ARCH = \"{ARM64_CPU_ARCH}\"",
            path.display()
        );
    }
}
