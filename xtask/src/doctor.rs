//! `cargo xtask doctor`: reports which toolchains and SDKs this host has.
//!
//! Every external command runs with a deadline (see `proc`), and any failure counts as
//! missing. A probe that exited 0 but whose output was not captured completely is reported
//! as `unknown` with the captured output ([`Status::Incomplete`]), never as missing.
//! Windows-only probes report `n/a` on other hosts. The command fails only when a required
//! item is missing or unknown.

use std::env;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::proc::{CmdOutput, run_with_timeout};

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Probe result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Found and usable.
    Ok,
    /// Not found or not working.
    Missing,
    /// The probe exited 0 but its output was not captured completely, so whether the item
    /// is present is unknown (never read as missing or present).
    Incomplete,
    /// Not applicable on this host.
    #[serde(rename = "n/a")]
    Na,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Missing => "missing",
            Status::Incomplete => "unknown",
            Status::Na => "n/a",
        }
    }
}

/// Outcome of one probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Present, with a version or other detail.
    Found(String),
    /// Absent, failed, or reported nothing usable.
    Missing,
    /// A probe command exited 0 but its output was cut short; the detail names the
    /// command and carries [`CmdOutput::describe`].
    Incomplete(String),
}

impl From<Option<String>> for Probe {
    fn from(o: Option<String>) -> Self {
        o.map_or(Probe::Missing, Probe::Found)
    }
}

/// The probe outcome of a command run (`None`: could not start or timed out). A failed
/// exit is [`Probe::Missing`]; a successful exit with incomplete output is
/// [`Probe::Incomplete`], never parsed.
pub fn classify_probe(cmdline: &str, out: Option<CmdOutput>) -> Result<CmdOutput, Probe> {
    match out {
        Some(o) if !o.success => Err(Probe::Missing),
        Some(o) if !o.complete() => Err(Probe::Incomplete(format!(
            "probe output incomplete: `{cmdline}`: {}",
            o.describe()
        ))),
        Some(o) => Ok(o),
        None => Err(Probe::Missing),
    }
}

/// Applies `parse` to the stdout of a successful, complete probe run.
fn probe_with(r: Result<CmdOutput, Probe>, parse: impl FnOnce(&str) -> Option<String>) -> Probe {
    match r {
        Ok(o) => parse(&o.stdout).into(),
        Err(p) => p,
    }
}

/// One row of the doctor report.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    /// What was probed.
    pub name: String,
    /// Result.
    pub status: Status,
    /// Version or other detail (or why it is missing).
    pub detail: String,
    /// Whether this host needs it to build M0-M7.
    pub required: bool,
    /// Milestone or task that needs it.
    pub needed_for: String,
}

/// Full doctor report (the `--json` output).
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    /// Host target triple from `rustc -vV`.
    pub host: String,
    /// True when no required item is missing.
    pub ok: bool,
    /// Probe results.
    pub items: Vec<Item>,
}

// ---------------------------------------------------------------- parsing helpers

/// Extracts the `host:` triple from `rustc -vV` output.
pub fn parse_rustc_host(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("host:"))
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

/// First non-empty trimmed line (for `--version` outputs and `vswhere -property`).
pub fn first_line(out: &str) -> Option<String> {
    out.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Extracts `release X.Y` from `nvcc --version` output, as `X.Y`.
pub fn parse_nvcc_release(out: &str) -> Option<String> {
    let rest = out.split("release ").nth(1)?;
    let v: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!v.is_empty()).then_some(v)
}

/// One GPU from `nvidia-smi`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvidiaGpu {
    /// Product name.
    pub name: String,
    /// Driver version.
    pub driver: String,
    /// CUDA compute capability, for example `12.0`; `None` when the driver cannot report it
    /// (older drivers lack the `compute_cap` field or print `[N/A]`).
    pub compute_cap: Option<String>,
}

impl NvidiaGpu {
    /// The `CMAKE_CUDA_ARCHITECTURES` value for this GPU (`12.0` -> `120`), which the
    /// whisper.cpp CUDA build in `.cargo/config.toml` must name.
    #[must_use]
    pub fn cuda_arch(&self) -> Option<String> {
        self.compute_cap.as_ref().map(|c| c.replace('.', ""))
    }

    /// Doctor detail for this GPU.
    #[must_use]
    pub fn describe(&self) -> String {
        match (&self.compute_cap, self.cuda_arch()) {
            (Some(cap), Some(arch)) => format!(
                "{} (driver {}, compute capability {cap}: CMAKE_CUDA_ARCHITECTURES={arch})",
                self.name, self.driver
            ),
            _ => format!(
                "{} (driver {}, compute capability unknown)",
                self.name, self.driver
            ),
        }
    }
}

/// `nvidia-smi` query with the compute capability (newer drivers).
pub const NVIDIA_SMI_QUERY: &str = "--query-gpu=name,driver_version,compute_cap";
/// Fallback query for drivers without the `compute_cap` field.
pub const NVIDIA_SMI_QUERY_OLD: &str = "--query-gpu=name,driver_version";

fn valid_cap(cap: &str) -> bool {
    cap.split('.').count() == 2
        && cap
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Parses `nvidia-smi <query> --format=csv,noheader`: `name, driver, compute_cap` with
/// `with_cap` ([`NVIDIA_SMI_QUERY`]), else `name, driver` ([`NVIDIA_SMI_QUERY_OLD`]). A
/// capability that is not `<major>.<minor>` (for example `[N/A]`) is unknown.
pub fn parse_nvidia_smi(out: &str, with_cap: bool) -> Vec<NvidiaGpu> {
    out.lines()
        .filter_map(|l| {
            let mut fields = l.trim().rsplitn(if with_cap { 3 } else { 2 }, ',');
            let cap = if with_cap {
                Some(fields.next()?.trim()).filter(|c| valid_cap(c))
            } else {
                None
            };
            let (driver, name) = (fields.next()?.trim(), fields.next()?.trim());
            (!name.is_empty() && !driver.is_empty()).then(|| NvidiaGpu {
                name: name.into(),
                driver: driver.into(),
                compute_cap: cap.map(str::to_owned),
            })
        })
        .collect()
}

/// The GPUs `query` reports: the [`NVIDIA_SMI_QUERY`] first, the old two-field query when
/// that fails or lists nothing, so an older driver never hides its GPU (and with it the
/// CUDA tests and `accept m2`'s CUDA run). `query` returns nvidia-smi's stdout on success
/// or the probe outcome otherwise. An incomplete capture of either query is returned as
/// `Err` with its detail: the GPU list is then unknown, not empty.
pub fn nvidia_gpus(
    query: impl Fn(&str) -> Result<String, Probe>,
) -> Result<Vec<NvidiaGpu>, String> {
    let parse = |q: &str, with_cap: bool| match query(q) {
        Ok(o) => Ok(parse_nvidia_smi(&o, with_cap)),
        Err(Probe::Incomplete(d)) => Err(d),
        Err(_) => Ok(Vec::new()),
    };
    let gpus = parse(NVIDIA_SMI_QUERY, true)?;
    if !gpus.is_empty() {
        return Ok(gpus);
    }
    parse(NVIDIA_SMI_QUERY_OLD, false)
}

fn version_key(s: &str) -> Option<Vec<u64>> {
    let parts: Option<Vec<u64>> = s.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| p.len() >= 2)
}

/// Highest dotted numeric version among `names` (for example Windows SDK or NDK
/// directory names). Names that are not dotted numbers are ignored.
pub fn latest_version<I, S>(names: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    names
        .into_iter()
        .filter_map(|n| version_key(n.as_ref()).map(|k| (k, n.as_ref().to_string())))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, n)| n)
}

/// Whether a `node --version` string (for example `v22.11.0`) is on an LTS line
/// (even major version).
pub fn node_is_lts(version: &str) -> bool {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|m| m.parse::<u64>().ok())
        .is_some_and(|m| m % 2 == 0)
}

/// True when no required item is missing or unknown (optional and `n/a` items never fail
/// doctor).
pub fn report_ok(items: &[Item]) -> bool {
    !items
        .iter()
        .any(|i| i.required && matches!(i.status, Status::Missing | Status::Incomplete))
}

/// vswhere component id of the MSVC x64 tools.
pub const VC_X64: &str = "Microsoft.VisualStudio.Component.VC.Tools.x86.x64";
/// vswhere component id of the MSVC ARM64 tools.
pub const VC_ARM64: &str = "Microsoft.VisualStudio.Component.VC.Tools.ARM64";

/// The Ninja row for a host triple: `(required, needed_for)`. ggml refuses MSVC for ARM64
/// CPU code, so whisper.cpp for ARM64 is built with LLVM's clang-cl through Ninja (T2.6,
/// `xtask::build`): required on an ARM64 host, optional on x64 (ARM64 cross builds).
#[must_use]
pub fn ninja_role(host: &str) -> (bool, &'static str) {
    if host.starts_with("aarch64") {
        (true, "M2 (ARM64)")
    } else {
        (false, "T2.6 (ARM64)")
    }
}

/// MSVC tool rows for a host triple, as `(name, component, required, needed_for)`:
/// the tools for the host's own architecture are required, the other one is optional.
/// Order is always x64 then ARM64.
pub fn msvc_roles(host: &str) -> [(&'static str, &'static str, bool, &'static str); 2] {
    if host.starts_with("aarch64") {
        [
            ("MSVC x64 tools", VC_X64, false, "x64 cross builds"),
            ("MSVC ARM64 tools", VC_ARM64, true, "M0-M7"),
        ]
    } else {
        [
            ("MSVC x64 tools", VC_X64, true, "M0-M7"),
            ("MSVC ARM64 tools", VC_ARM64, false, "T2.6"),
        ]
    }
}

// ---------------------------------------------------------------- probes

/// Runs a probe (see [`classify_probe`]): doctor reads versions and device lists from its
/// output, so a cut-off capture is reported as unknown, never parsed and never "missing".
fn run(program: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> Result<CmdOutput, Probe> {
    let program = program.as_ref();
    let cmdline = format!("{} {}", program.to_string_lossy(), args.join(" "));
    classify_probe(
        &cmdline,
        run_with_timeout(Command::new(program).args(args), PROBE_TIMEOUT),
    )
}

/// Doctor item names whose presence means CUDA builds and runs on this host.
pub const CUDA_ITEMS: [&str; 2] = ["CUDA toolkit", "NVIDIA GPU"];

/// Whether the report shows CUDA: the toolkit and an NVIDIA GPU both found. `accept m2`
/// runs whisper.cpp on CUDA exactly when this holds (PLAN.md T2.5).
pub fn reports_cuda(report: &DoctorReport) -> bool {
    CUDA_ITEMS.iter().all(|name| {
        report
            .items
            .iter()
            .any(|i| i.name == *name && i.status == Status::Ok)
    })
}

/// Doctor item name of the Vulkan SDK.
pub const VULKAN_SDK_ITEM: &str = "Vulkan SDK";

/// Whether the report shows the Vulkan SDK (whisper.cpp's `vulkan` build needs it, T2.6;
/// `cargo xtask model-eval --backend vulkan` requires it, T6.10d).
pub fn reports_vulkan_sdk(report: &DoctorReport) -> bool {
    report
        .items
        .iter()
        .any(|i| i.name == VULKAN_SDK_ITEM && i.status == Status::Ok)
}

/// Whether this host has CUDA for an automatic or `cuda` backend choice ([`reports_cuda`]).
/// A CUDA probe whose output was cut short is an error naming it: a starved `nvidia-smi`
/// must not quietly turn an automatic choice into a CPU build. `requested` is the
/// `--backend` value; an explicit non-CUDA backend does not depend on the probes.
///
/// # Errors
/// A CUDA item is [`Status::Incomplete`] and the choice could be CUDA.
pub fn cuda_for_backend_choice(report: &DoctorReport, requested: Option<&str>) -> Result<bool> {
    if requested.is_some_and(|b| b != "cuda") {
        return Ok(reports_cuda(report));
    }
    if let Some(i) = report
        .items
        .iter()
        .find(|i| CUDA_ITEMS.contains(&i.name.as_str()) && i.status == Status::Incomplete)
    {
        anyhow::bail!(
            "the doctor probe for {} did not capture its output completely, so whether this \
             host has CUDA is unknown; refusing to fall back to a CPU build (re-run, or pass \
             --backend explicitly):\n{}",
            i.name,
            i.detail
        );
    }
    Ok(reports_cuda(report))
}

fn item(name: &str, required: bool, needed_for: &str, found: impl Into<Probe>, why: &str) -> Item {
    let (status, detail) = match found.into() {
        Probe::Found(d) => (Status::Ok, d),
        Probe::Missing => (Status::Missing, why.to_string()),
        Probe::Incomplete(d) => (Status::Incomplete, d),
    };
    Item {
        name: name.into(),
        status,
        detail,
        required,
        needed_for: needed_for.into(),
    }
}

fn na(name: &str, required: bool, needed_for: &str) -> Item {
    Item {
        name: name.into(),
        status: Status::Na,
        detail: "n/a on this host (Windows only)".into(),
        required,
        needed_for: needed_for.into(),
    }
}

fn env_dir(vars: &[&str]) -> Option<PathBuf> {
    vars.iter()
        .filter_map(env::var_os)
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}

fn subdir_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn vswhere(component: &str) -> Probe {
    let Some(base) = env::var_os("ProgramFiles(x86)") else {
        return Probe::Missing;
    };
    let exe = Path::new(&base).join(r"Microsoft Visual Studio\Installer\vswhere.exe");
    let out = run(
        &exe,
        &[
            "-latest",
            "-prerelease",
            "-products",
            "*",
            "-requires",
            component,
            "-property",
            "installationVersion",
        ],
    );
    probe_with(out, |s| first_line(s).map(|v| format!("Visual Studio {v}")))
}

/// The latest Windows SDK version with `Lib\<version>\um` (also `build`'s host toolchain).
pub(crate) fn windows_sdk() -> Option<String> {
    let base = env::var_os("ProgramFiles(x86)")?;
    let lib = Path::new(&base).join(r"Windows Kits\10\Lib");
    let names: Vec<String> = subdir_names(&lib)
        .into_iter()
        .filter(|n| lib.join(n).join("um").is_dir())
        .collect();
    latest_version(&names)
}

fn libclang() -> Probe {
    if let Some(p) = env::var_os("LIBCLANG_PATH").map(PathBuf::from) {
        let dll = if p.is_dir() {
            p.join("libclang.dll")
        } else {
            p.clone()
        };
        if dll.is_file() {
            return Probe::Found(format!("LIBCLANG_PATH -> {}", dll.display()));
        }
    }
    if cfg!(windows) {
        let default = Path::new(r"C:\Program Files\LLVM\bin\libclang.dll");
        if default.is_file() {
            return Probe::Found(format!("{} (default LLVM install)", default.display()));
        }
    }
    probe_with(run("clang", &["--version"]), |s| {
        first_line(s).map(|v| format!("clang on PATH: {v}"))
    })
}

fn node_version() -> Probe {
    probe_with(run("node", &["--version"]), first_line)
}

/// The doctor row for Node: PLAN T0.2 requires Node LTS, so a version on an odd
/// (non-LTS) major counts as missing.
pub fn node_item(version: Option<&str>) -> Item {
    let found = version
        .filter(|v| node_is_lts(v))
        .map(|v| format!("{v} (LTS line)"));
    let why = match version {
        Some(v) => format!("{v} is not an LTS line (odd major); install a Node LTS release"),
        None => "node not on PATH".to_string(),
    };
    item("Node LTS", true, "M7", found, &why)
}

fn pnpm() -> Probe {
    let out = if cfg!(windows) {
        run("cmd", &["/c", "pnpm", "--version"])
    } else {
        run("pnpm", &["--version"])
    };
    probe_with(out, first_line)
}

fn cuda() -> Probe {
    let Some(root) = env::var_os("CUDA_PATH").map(PathBuf::from) else {
        return Probe::Missing;
    };
    let nvcc = root
        .join("bin")
        .join(if cfg!(windows) { "nvcc.exe" } else { "nvcc" });
    probe_with(run(&nvcc, &["--version"]), |s| {
        parse_nvcc_release(s).map(|rel| format!("CUDA {rel} at {}", root.display()))
    })
}

fn nvidia_gpu() -> Probe {
    match nvidia_gpus(|q| run("nvidia-smi", &[q, "--format=csv,noheader"]).map(|o| o.stdout)) {
        Err(detail) => Probe::Incomplete(detail),
        Ok(gpus) => (!gpus.is_empty())
            .then(|| {
                gpus.iter()
                    .map(NvidiaGpu::describe)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .into(),
    }
}

fn android_ndk(sdk: Option<&Path>) -> Option<String> {
    if let Some(p) = env_dir(&["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT"]) {
        return Some(p.display().to_string());
    }
    let ndk = sdk?.join("ndk");
    let v = latest_version(subdir_names(&ndk))?;
    Some(format!("{v} at {}", ndk.join(&v).display()))
}

/// Runs every probe.
pub fn collect() -> DoctorReport {
    let windows = cfg!(windows);
    let rustc = run("rustc", &["-vV"]);
    let host = rustc
        .as_ref()
        .ok()
        .and_then(|o| parse_rustc_host(&o.stdout))
        .unwrap_or_else(|| "unknown".into());

    let mut items = vec![item(
        "Rust toolchain",
        true,
        "all",
        probe_with(rustc, |s| {
            Some(format!(
                "{} (host {})",
                first_line(s)?,
                parse_rustc_host(s)?
            ))
        }),
        "rustc -vV failed",
    )];

    let msvc = msvc_roles(&host);
    if windows {
        for (name, component, required, needed_for) in msvc {
            let short = component.trim_start_matches("Microsoft.VisualStudio.Component.");
            items.push(item(
                name,
                required,
                needed_for,
                vswhere(component),
                &format!("vswhere found no install with {short}"),
            ));
        }
        items.push(item(
            "Windows SDK",
            true,
            "M0-M7",
            windows_sdk(),
            "no Windows Kits\\10\\Lib\\<version>\\um",
        ));
    } else {
        for (name, _, required, needed_for) in msvc {
            items.push(na(name, required, needed_for));
        }
        items.push(na("Windows SDK", true, "M0-M7"));
    }

    items.push(item(
        "CMake",
        true,
        "M2 (whisper.cpp)",
        probe_with(run("cmake", &["--version"]), first_line),
        "cmake not on PATH",
    ));
    items.push(item(
        "LLVM/libclang",
        true,
        "M2 (bindgen)",
        libclang(),
        "no LIBCLANG_PATH with libclang, no default LLVM install, no clang on PATH",
    ));
    let (required, needed_for) = ninja_role(&host);
    items.push(item(
        "Ninja",
        required,
        needed_for,
        probe_with(run("ninja", &["--version"]), first_line),
        "ninja not on PATH (winget install Ninja-build.Ninja)",
    ));
    items.push(match node_version() {
        Probe::Incomplete(d) => item("Node LTS", true, "M7", Probe::Incomplete(d), ""),
        Probe::Found(v) => node_item(Some(&v)),
        Probe::Missing => node_item(None),
    });
    items.push(item("pnpm", true, "M7", pnpm(), "pnpm not on PATH"));
    items.push(item(
        CUDA_ITEMS[0],
        false,
        "T2.6, M8",
        cuda(),
        "CUDA_PATH unset or nvcc --version failed",
    ));
    items.push(item(
        CUDA_ITEMS[1],
        false,
        "T2.6, M8",
        nvidia_gpu(),
        "nvidia-smi failed or reported no GPU",
    ));
    items.push(item(
        VULKAN_SDK_ITEM,
        false,
        "T2.6, T6.10d",
        env_dir(&["VULKAN_SDK"]).map(|p| p.display().to_string()),
        "VULKAN_SDK unset or not a directory",
    ));
    items.push(item(
        "Docker",
        false,
        "M11",
        probe_with(run("docker", &["--version"]), first_line),
        "docker not on PATH",
    ));
    let sdk = env_dir(&["ANDROID_HOME", "ANDROID_SDK_ROOT"]);
    items.push(item(
        "Android SDK",
        false,
        "M10",
        sdk.as_ref().map(|p| p.display().to_string()),
        "ANDROID_HOME / ANDROID_SDK_ROOT unset or not a directory",
    ));
    items.push(item(
        "Android NDK",
        false,
        "M10",
        android_ndk(sdk.as_deref()),
        "no ANDROID_NDK_HOME and no <sdk>/ndk/<version>",
    ));
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    items.push(item(
        "cargo-ndk",
        false,
        "M10",
        probe_with(run(&cargo, &["ndk", "--version"]), first_line),
        "cargo ndk --version failed",
    ));

    let ok = report_ok(&items);
    DoctorReport { host, ok, items }
}

/// Human-readable table.
pub fn render(report: &DoctorReport) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "host: {}\n", report.host);
    let _ = writeln!(
        s,
        "{:<18} {:<8} {:<9} {:<17} detail",
        "item", "status", "required", "needed for"
    );
    for i in &report.items {
        let _ = writeln!(
            s,
            "{:<18} {:<8} {:<9} {:<17} {}",
            i.name,
            i.status.as_str(),
            if i.required { "yes" } else { "no" },
            i.needed_for,
            i.detail
        );
    }
    let _ = writeln!(
        s,
        "\ndoctor: {}",
        if report.ok {
            "all required items present"
        } else {
            "required items missing"
        }
    );
    s
}

/// Runs doctor and prints the table or JSON. Returns whether every required item is present.
pub fn run_doctor(json: bool) -> Result<bool> {
    let report = collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render(&report));
    }
    Ok(report.ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_reports_cuda_only_with_toolkit_and_gpu() {
        let report = |toolkit: bool, gpu: bool| DoctorReport {
            host: "x86_64-pc-windows-msvc".into(),
            ok: true,
            items: vec![
                item(
                    CUDA_ITEMS[0],
                    false,
                    "T2.6",
                    toolkit.then(|| "CUDA 13.4".to_string()),
                    "no",
                ),
                item(
                    CUDA_ITEMS[1],
                    false,
                    "T2.6",
                    gpu.then(|| "RTX 5080".to_string()),
                    "no",
                ),
            ],
        };
        assert!(reports_cuda(&report(true, true)));
        assert!(!reports_cuda(&report(true, false)));
        assert!(!reports_cuda(&report(false, true)));
        assert!(!reports_cuda(&DoctorReport {
            host: String::new(),
            ok: true,
            items: Vec::new(),
        }));
    }

    fn cuda_report(toolkit: Probe, gpu: Probe) -> DoctorReport {
        let items = vec![
            item(CUDA_ITEMS[0], false, "T2.6", toolkit, "no toolkit"),
            item(CUDA_ITEMS[1], false, "T2.6", gpu, "no gpu"),
        ];
        DoctorReport {
            host: "x86_64-pc-windows-msvc".into(),
            ok: report_ok(&items),
            items,
        }
    }

    fn found(s: &str) -> Probe {
        Probe::Found(s.into())
    }

    #[test]
    fn doctor_incomplete_probe_is_unknown_not_missing() {
        let r = cuda_report(
            found("CUDA 13.4"),
            Probe::Incomplete("probe output incomplete: `nvidia-smi ...`: exit code 0".into()),
        );
        let gpu = &r.items[1];
        assert_eq!(gpu.status, Status::Incomplete);
        assert!(
            gpu.detail.contains("probe output incomplete"),
            "{}",
            gpu.detail
        );
        assert!(!gpu.detail.contains("no gpu"));
        assert!(render(&r).contains("unknown"));
        assert!(!reports_cuda(&r));
        // A required item whose probe was cut short fails doctor like a missing one.
        let required = item(
            "CMake",
            true,
            "M2",
            Probe::Incomplete("x".into()),
            "missing",
        );
        assert!(!report_ok(&[required]));
    }

    #[test]
    fn doctor_classifies_probe_runs() {
        let out = |success: bool, complete: bool| CmdOutput {
            success,
            code: Some(i32::from(!success)),
            stdout: "NVIDIA GeForce RTX 5080, 581.57, 12.0\n".into(),
            stderr: String::new(),
            stdout_complete: complete,
            stderr_complete: true,
        };
        assert!(classify_probe("nvidia-smi", Some(out(true, true))).is_ok());
        assert_eq!(
            classify_probe("nvidia-smi", Some(out(false, true))).unwrap_err(),
            Probe::Missing
        );
        assert_eq!(
            classify_probe("nvidia-smi", None).unwrap_err(),
            Probe::Missing
        );
        let Err(Probe::Incomplete(d)) = classify_probe("nvidia-smi --q", Some(out(true, false)))
        else {
            panic!("an incomplete successful probe must be Incomplete");
        };
        assert!(
            d.contains("`nvidia-smi --q`") && d.contains("exit code 0"),
            "{d}"
        );
        assert!(d.contains("stdout (INCOMPLETE)"), "{d}");
    }

    #[test]
    fn doctor_incomplete_cuda_probe_blocks_an_automatic_backend_choice() {
        let incomplete = || Probe::Incomplete("probe output incomplete: `nvidia-smi`".into());
        for r in [
            cuda_report(found("CUDA 13.4"), incomplete()),
            cuda_report(incomplete(), found("RTX 5080")),
        ] {
            for requested in [None, Some("cuda")] {
                let err = format!("{:#}", cuda_for_backend_choice(&r, requested).unwrap_err());
                assert!(
                    err.contains("refusing to fall back to a CPU build"),
                    "{err}"
                );
                assert!(err.contains("probe output incomplete"), "{err}");
            }
            // An explicit non-CUDA backend does not depend on the CUDA probes.
            for requested in ["cpu", "vulkan"] {
                assert!(!cuda_for_backend_choice(&r, Some(requested)).unwrap());
            }
        }
        let both = cuda_report(found("CUDA 13.4"), found("RTX 5080"));
        assert!(cuda_for_backend_choice(&both, None).unwrap());
        let none = cuda_report(found("CUDA 13.4"), Probe::Missing);
        assert!(!cuda_for_backend_choice(&none, None).unwrap());
    }

    #[test]
    fn rustc_host() {
        let out = "rustc 1.99.1 (abc 2026-09-01)\nbinary: rustc\ncommit-hash: abc\nhost: x86_64-pc-windows-msvc\nrelease: 1.99.1\nLLVM version: 21.1.0\n";
        assert_eq!(
            parse_rustc_host(out).as_deref(),
            Some("x86_64-pc-windows-msvc")
        );
        let crlf = out.replace('\n', "\r\n");
        assert_eq!(
            parse_rustc_host(&crlf).as_deref(),
            Some("x86_64-pc-windows-msvc")
        );
        assert_eq!(parse_rustc_host("rustc 1.0\n"), None);
    }

    #[test]
    fn first_line_skips_blanks() {
        assert_eq!(
            first_line("\r\n  17.14.36310.24 \r\n").as_deref(),
            Some("17.14.36310.24")
        );
        assert_eq!(
            first_line("cmake version 3.31.2\n\nCMake suite").as_deref(),
            Some("cmake version 3.31.2")
        );
        assert_eq!(first_line(""), None);
        assert_eq!(first_line(" \r\n\n"), None);
    }

    #[test]
    fn nvcc_release() {
        let out = "nvcc: NVIDIA (R) Cuda compiler driver\r\nCopyright (c) 2005-2025 NVIDIA Corporation\r\nBuilt on Wed_Jan_15_19:38:46_Pacific_Standard_Time_2025\r\nCuda compilation tools, release 12.8, V12.8.61\r\nBuild cuda_12.8.r12.8/compiler.35404655_0\r\n";
        assert_eq!(parse_nvcc_release(out).as_deref(), Some("12.8"));
        assert_eq!(parse_nvcc_release("garbage"), None);
    }

    #[test]
    fn nvidia_smi_csv() {
        let out = "NVIDIA GeForce RTX 5080, 581.57, 12.0\r\n";
        let gpus = parse_nvidia_smi(out, true);
        assert_eq!(
            gpus,
            vec![NvidiaGpu {
                name: "NVIDIA GeForce RTX 5080".into(),
                driver: "581.57".into(),
                compute_cap: Some("12.0".into()),
            }]
        );
        assert_eq!(gpus[0].cuda_arch().as_deref(), Some("120"));
        assert!(gpus[0].describe().contains("CMAKE_CUDA_ARCHITECTURES=120"));
        let two = "NVIDIA A, 1.0, 8.6\nNVIDIA B, 2.0, 8.9\n\n";
        assert_eq!(parse_nvidia_smi(two, true).len(), 2);
        assert_eq!(
            parse_nvidia_smi(two, true)[1].cuda_arch().as_deref(),
            Some("89")
        );
        // [N/A] is an unknown capability, not a missing GPU.
        let na = parse_nvidia_smi("NVIDIA X, 1.0, [N/A]", true);
        assert_eq!(na.len(), 1);
        assert_eq!(na[0].compute_cap, None);
        assert!(na[0].describe().contains("compute capability unknown"));
        // The old two-field query.
        let old = parse_nvidia_smi("NVIDIA GeForce RTX 5080, 581.57", false);
        assert_eq!(old[0].name, "NVIDIA GeForce RTX 5080");
        assert_eq!(old[0].driver, "581.57");
        assert_eq!(old[0].compute_cap, None);
        assert!(parse_nvidia_smi("", true).is_empty());
        assert!(parse_nvidia_smi("No devices were found", true).is_empty());
    }

    #[test]
    fn doctor_falls_back_to_the_old_nvidia_smi_query() {
        // A driver that rejects compute_cap still reports its GPU.
        let old_driver = |q: &str| {
            (q == NVIDIA_SMI_QUERY_OLD)
                .then(|| "NVIDIA T, 470.1\n".to_owned())
                .ok_or(Probe::Missing)
        };
        let gpus = nvidia_gpus(old_driver).unwrap();
        assert_eq!(gpus.len(), 1);
        assert!(gpus[0].describe().contains("compute capability unknown"));
        // A current driver uses the first query only.
        let new_driver = |q: &str| {
            assert_eq!(q, NVIDIA_SMI_QUERY, "no fallback needed");
            Ok("NVIDIA GeForce RTX 5080, 581.57, 12.0\n".to_owned())
        };
        assert_eq!(
            nvidia_gpus(new_driver).unwrap()[0].compute_cap.as_deref(),
            Some("12.0")
        );
        assert!(nvidia_gpus(|_| Err(Probe::Missing)).unwrap().is_empty());
    }

    #[test]
    fn doctor_incomplete_nvidia_smi_is_unknown_not_no_gpu() {
        // Cut short on the first query: no fallback guess, the list is unknown.
        let first_cut = |q: &str| {
            assert_eq!(
                q, NVIDIA_SMI_QUERY,
                "an incomplete first query must not fall back"
            );
            Err(Probe::Incomplete("cut".into()))
        };
        assert_eq!(nvidia_gpus(first_cut).unwrap_err(), "cut");
        // First query fails cleanly, the fallback query is cut short.
        let old_cut = |q: &str| {
            if q == NVIDIA_SMI_QUERY {
                Err(Probe::Missing)
            } else {
                Err(Probe::Incomplete("old cut".into()))
            }
        };
        assert_eq!(nvidia_gpus(old_cut).unwrap_err(), "old cut");
    }

    /// `.cargo/config.toml` builds whisper.cpp for a portable x86-64 baseline (AVX2, not the
    /// build host's CPU) and names the CUDA architecture with the rebuild note.
    #[test]
    fn doctor_cargo_config_pins_portable_whisper_builds() {
        let text = std::fs::read_to_string(crate::repo_root().join(".cargo/config.toml")).unwrap();
        let doc: toml::Table = text.parse().unwrap();
        let env = doc["env"].as_table().unwrap();
        assert_eq!(env["GGML_NATIVE"].as_str(), Some("OFF"));
        for flag in [
            "GGML_AVX",
            "GGML_AVX2",
            "GGML_FMA",
            "GGML_F16C",
            "GGML_BMI2",
        ] {
            assert_eq!(env[flag].as_str(), Some("ON"), "{flag}");
        }
        assert_eq!(env["GGML_AVX512"].as_str(), Some("OFF"));
        assert!(env.contains_key("CMAKE_CUDA_ARCHITECTURES"));
        assert!(text.contains("cargo clean -p whisper-rs-sys"));
    }

    #[test]
    fn latest_version_is_numeric_not_lexical() {
        let names = ["10.0.19041.0", "10.0.26100.0", "10.0.22621.0", "wdf"];
        assert_eq!(latest_version(names).as_deref(), Some("10.0.26100.0"));
        assert_eq!(
            latest_version(["9.0.0", "10.0.0"]).as_deref(),
            Some("10.0.0")
        );
        assert_eq!(
            latest_version(["27.2.12479018", "26.3.11579264"]).as_deref(),
            Some("27.2.12479018")
        );
        assert_eq!(latest_version(Vec::<String>::new()), None);
        assert_eq!(latest_version(["include", "x"]), None);
    }

    #[test]
    fn node_item_marks_non_lts_missing() {
        let lts = node_item(Some("v22.11.0"));
        assert_eq!(lts.status, Status::Ok);
        assert!(lts.required);
        assert!(lts.detail.contains("v22.11.0"), "{}", lts.detail);
        let odd = node_item(Some("v23.0.0"));
        assert_eq!(odd.status, Status::Missing);
        assert!(odd.detail.contains("v23.0.0"), "{}", odd.detail);
        assert!(odd.detail.contains("LTS"), "{}", odd.detail);
        assert!(!report_ok(&[odd]));
        assert_eq!(node_item(None).status, Status::Missing);
        assert_eq!(node_item(Some("garbage")).status, Status::Missing);
    }

    #[test]
    fn node_lts_is_even_major() {
        assert!(node_is_lts("v22.11.0"));
        assert!(node_is_lts("v24.1.0\n"));
        assert!(!node_is_lts("v23.0.0"));
        assert!(!node_is_lts("garbage"));
    }

    #[test]
    fn report_ok_only_fails_on_required_missing() {
        let mk = |required, status| Item {
            name: "x".into(),
            status,
            detail: String::new(),
            required,
            needed_for: String::new(),
        };
        assert!(report_ok(&[
            mk(false, Status::Missing),
            mk(true, Status::Ok),
            mk(true, Status::Na)
        ]));
        assert!(!report_ok(&[
            mk(true, Status::Ok),
            mk(true, Status::Missing)
        ]));
        assert!(report_ok(&[]));
    }

    #[test]
    fn msvc_requirement_follows_host_arch() {
        let x64 = msvc_roles("x86_64-pc-windows-msvc");
        assert_eq!(x64[0].0, "MSVC x64 tools");
        assert!(x64[0].2, "x64 tools required on x64 host");
        assert_eq!(x64[1].0, "MSVC ARM64 tools");
        assert!(!x64[1].2, "ARM64 tools optional on x64 host");
        assert_eq!(x64[1].3, "T2.6");

        let arm = msvc_roles("aarch64-pc-windows-msvc");
        assert_eq!(arm[1].0, "MSVC ARM64 tools");
        assert!(arm[1].2, "ARM64 tools required on ARM64 host");
        assert!(!arm[0].2, "x64 tools optional on ARM64 host");
        assert_eq!(arm[1].1, VC_ARM64);
        assert_eq!(arm[0].1, VC_X64);
    }

    #[test]
    fn ninja_is_required_only_on_arm64_hosts() {
        assert!(ninja_role("aarch64-pc-windows-msvc").0);
        assert!(!ninja_role("x86_64-pc-windows-msvc").0);
    }

    #[test]
    fn json_uses_lowercase_status_and_na() {
        let report = DoctorReport {
            host: "h".into(),
            ok: true,
            items: vec![Item {
                name: "x".into(),
                status: Status::Na,
                detail: "d".into(),
                required: false,
                needed_for: "M10".into(),
            }],
        };
        let v: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(v["items"][0]["status"], "n/a");
        assert_eq!(serde_json::to_value(Status::Missing).unwrap(), "missing");
        assert_eq!(serde_json::to_value(Status::Ok).unwrap(), "ok");
        for key in ["name", "status", "detail", "required", "needed_for"] {
            assert!(v["items"][0].get(key).is_some(), "{key}");
        }
    }
}
