//! `cargo xtask qnn-convert --model <name>` (T8.3): builds the NPU conversion of a Parakeet
//! TDT model into `<models>/parakeet-qnn/<name>/` (`EARSHOT_MODELS` or
//! `%LOCALAPPDATA%\Earshot\models`), where the runtime's QNN backend looks for it.
//!
//! 1. The source is the float model of the manifest: `cargo xtask fetch-models --set ci` for
//!    the default models (110M, 0.6B v2), `--set choice` for 0.6B v3 (all three are also in
//!    `eval`).
//! 2. Calibration pieces: [`calibration_plan`] picks LibriSpeech dev-clean utterances (never
//!    the pinned subset: its dev-clean half is the dev WER set, its test-clean half the
//!    acceptance set) and the `parakeet_calibration` example of `earshot-onnx` (release)
//!    writes their production NeMo features at the static encoder shape into
//!    `target/qnn-convert/cal-<n_mels>-j<joined>s<single>` (needs `cargo xtask
//!    fetch-fixtures --set librispeech`).
//! 3. A Python virtual environment in `target/qnn-convert/venv`, created with CPython 3.14 from
//!    PATH and filled with `pip install --require-hashes --no-deps -r
//!    xtask/python/requirements-qnn-convert.txt` (ONNX Runtime's quantisation tools exist only
//!    in its Python package; every wheel is hash-pinned).
//! 4. `xtask/python/qnn_convert.py` fixes the shapes, folds constants, quantises to QDQ with
//!    the [`Recipe`] (`--recipe`, default [`DEFAULT_RECIPE`]) and writes the model and
//!    `conversion.json` (recipe, versions, hashes).
//!
//! The result is hosted nowhere: shipping waits on the owner's hosting decision (Parakeet is
//! CC-BY-4.0) and the QNN redistribution question (T8.4).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::ci::cargo_command;
use crate::proc::run_with_timeout;

/// Feature frames of one 30 s piece (`earshot_onnx::parakeet::STATIC_FRAMES`).
pub const STATIC_FRAMES: usize = 3001;
/// The Python minor version the requirements are pinned for.
pub const PYTHON: (u32, u32) = (3, 14);
/// Deadline for building the calibration example and for writing the pieces.
const STEP_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Deadline for creating the virtual environment.
const VENV_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Deadline for installing the pinned wheels (about 50 MB).
const PIP_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Deadline for one conversion (the 0.6B encoders took about 65 min on the desktop).
const CONVERT_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
/// pip settings for every pip call: no version check against PyPI, never a prompt.
pub const PIP_ENV: [(&str, &str); 2] = [
    ("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
    ("PIP_NO_INPUT", "1"),
];
/// The file in the venv that records the SHA-256 of the requirements it was installed from.
pub const REQUIREMENTS_MARKER: &str = "earshot-requirements.sha256";

/// Whether the venv needs the pinned wheels installed: no marker, or a marker for other
/// requirements.
#[must_use]
pub fn needs_install(marker: Option<&str>, requirements_sha256: &str) -> bool {
    marker.is_none_or(|m| m.trim() != requirements_sha256)
}

/// The file in a finished calibration directory that records how many pieces it holds.
pub const PIECE_COUNT_FILE: &str = "pieces.txt";

/// A convertible model: name, float source directory (relative to the models directory) and
/// its mel feature count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Convertible {
    /// The model name (as `--model` and the runtime use it).
    pub name: &'static str,
    /// The float model's directory, relative to the models directory.
    pub source: &'static str,
    /// Mel features of its front end (`feat_dim`).
    pub n_mels: usize,
}

/// The TDT models that have an NPU conversion.
pub const CONVERTIBLE: [Convertible; 3] = [
    Convertible {
        name: "parakeet-tdt-110m",
        source: "parakeet/tdt-110m",
        n_mels: 80,
    },
    Convertible {
        name: "parakeet-tdt-0.6b-v2",
        source: "parakeet/tdt-0.6b-v2",
        n_mels: 128,
    },
    Convertible {
        name: "parakeet-tdt-0.6b-v3",
        source: "parakeet/tdt-0.6b-v3",
        n_mels: 128,
    },
];

/// The convertible model `name`.
///
/// # Errors
/// Any other name, with the list.
pub fn convertible(name: &str) -> Result<Convertible> {
    CONVERTIBLE
        .iter()
        .copied()
        .find(|c| c.name == name)
        .with_context(|| {
            format!(
                "no NPU conversion for {name}; convertible: {}",
                CONVERTIBLE.map(|c| c.name).join(", ")
            )
        })
}

/// The output directory of `name` below the models directory.
#[must_use]
pub fn output_dir(models: &Path, name: &str) -> PathBuf {
    models.join("parakeet-qnn").join(name)
}

/// Whether `python --version` output names the pinned [`PYTHON`] minor version.
#[must_use]
pub fn python_matches(version_output: &str) -> bool {
    let Some(v) = version_output.trim().strip_prefix("Python ") else {
        return false;
    };
    let mut parts = v.split('.').map(str::parse::<u32>);
    matches!(
        (parts.next(), parts.next()),
        (Some(Ok(major)), Some(Ok(minor))) if (major, minor) == PYTHON
    )
}

/// The venv's interpreter.
#[must_use]
pub fn venv_python(venv: &Path) -> PathBuf {
    venv.join("Scripts").join("python.exe")
}

/// `pip` arguments installing exactly the hash-pinned requirements.
#[must_use]
pub fn pip_args(requirements: &Path) -> Vec<String> {
    [
        "-m",
        "pip",
        "install",
        "--require-hashes",
        "--no-deps",
        "--only-binary",
        ":all:",
        "-r",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .chain(std::iter::once(requirements.display().to_string()))
    .collect()
}

/// One conversion recipe (T8.3c): the calibration set and the quantisation settings. Every
/// measured variant stays here so its numbers can be reproduced (`--recipe`; docs/qnn.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe {
    /// The recipe name (`--recipe`, recorded in `conversion.json`).
    pub name: &'static str,
    /// The calibration pieces.
    pub calibration: CalibrationSet,
    /// Activation type: `u16` (A16) or `u8` (A8).
    pub activations: &'static str,
    /// 8-bit Conv weights per output channel instead of per tensor (ONNX Runtime's QNN
    /// configuration applies `per_channel` to Conv only; MatMul and Gemm stay per tensor).
    pub per_channel: bool,
    /// Activation calibration: `minmax`, `entropy` or `percentile`.
    pub method: &'static str,
    /// Mixed precision with 8-bit activations: the operator types whose activation inputs
    /// stay 16 bit (from the sensitivity sweep, `xtask/python/qnn_sensitivity.py`).
    pub a16_roles: &'static [&'static str],
}

/// The T8.3 settings with the T8.3c calibration fixes: A16W8, MinMax, weights per tensor,
/// two joined pieces per dev-clean speaker (80 pieces).
const BASE: Recipe = Recipe {
    name: "a16w8-minmax-j2",
    calibration: CalibrationSet {
        joined: 2,
        single: 0,
    },
    activations: "u16",
    per_channel: false,
    method: "minmax",
    a16_roles: &[],
};

/// The recipes, in the order T8.3c measured them (docs/qnn.md has every one's numbers).
pub const RECIPES: [Recipe; 7] = [
    BASE,
    Recipe {
        name: "a16w8-minmax-j4s4",
        calibration: CalibrationSet {
            joined: 4,
            single: 4,
        },
        ..BASE
    },
    Recipe {
        name: "a16w8pc-minmax-j2",
        per_channel: true,
        ..BASE
    },
    Recipe {
        name: "a16w8-entropy-j2",
        method: "entropy",
        ..BASE
    },
    Recipe {
        name: "a16w8-percentile-j2",
        method: "percentile",
        ..BASE
    },
    Recipe {
        name: "a8w8-minmax-j2",
        activations: "u8",
        ..BASE
    },
    Recipe {
        name: "a8w8pc-mixed-j2",
        activations: "u8",
        per_channel: true,
        a16_roles: &["LayerNormalization", "Add", "Softmax", "Reshape"],
        ..BASE
    },
];

/// The recipe `qnn-convert` uses unless told otherwise (T8.3c's choice, docs/qnn.md).
pub const DEFAULT_RECIPE: &str = "a16w8pc-minmax-j2";

/// The recipe `name`.
///
/// # Errors
/// Any other name, with the list.
pub fn recipe(name: &str) -> Result<Recipe> {
    RECIPES
        .iter()
        .copied()
        .find(|r| r.name == name)
        .with_context(|| {
            format!(
                "no recipe {name}; recipes: {}",
                RECIPES.map(|r| r.name).join(", ")
            )
        })
}

/// The converter's arguments.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn convert_args(
    script: &Path,
    c: &Convertible,
    r: &Recipe,
    source: &Path,
    calibration: &Path,
    pieces: usize,
    work: &Path,
    out: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-I".into(),
        script.display().to_string(),
        "--source".into(),
        source.display().to_string(),
        "--calibration".into(),
        calibration.display().to_string(),
        "--pieces".into(),
        pieces.to_string(),
        "--n-mels".into(),
        c.n_mels.to_string(),
        "--frames".into(),
        STATIC_FRAMES.to_string(),
        "--recipe".into(),
        r.name.into(),
        "--activations".into(),
        r.activations.into(),
        "--calibration-method".into(),
        r.method.into(),
        "--calibration-set".into(),
        format!(
            "LibriSpeech dev-clean minus the pinned subset: per speaker {} single-utterance \
             and {} joined pieces (xtask::qnn_convert::calibration_plan)",
            r.calibration.single, r.calibration.joined
        ),
    ];
    if r.per_channel {
        args.push("--per-channel".into());
    }
    if !r.a16_roles.is_empty() {
        args.push("--a16-roles".into());
        args.push(r.a16_roles.join(","));
    }
    args.extend([
        "--work".into(),
        work.display().to_string(),
        "--out".into(),
        out.display().to_string(),
    ]);
    args
}

/// The example writing the calibration pieces.
const CALIBRATION_EXAMPLE: &str = "parakeet_calibration";

/// One utterance of the calibration corpus: LibriSpeech id (`speaker-chapter-utterance`),
/// its path and its length in samples (16 kHz).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalUtterance {
    /// LibriSpeech id.
    pub id: String,
    /// The FLAC file.
    pub path: PathBuf,
    /// Samples (16 kHz mono).
    pub samples: u64,
}

/// How many calibration pieces each dev-clean speaker contributes (T8.3c): `single`
/// pieces of one utterance each (the lengths production pieces of short speech have), then
/// `joined` pieces of the speaker's next utterances joined with 0.5 s gaps up to 30 s (long
/// pieces, as the piece planner merges speech).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalibrationSet {
    /// Joined pieces per speaker.
    pub joined: usize,
    /// Single-utterance pieces per speaker.
    pub single: usize,
}

impl CalibrationSet {
    /// The calibration directory name for `n_mels` features (one per selection, so a
    /// changed selection never reuses old pieces).
    #[must_use]
    pub fn dir_name(&self, n_mels: usize) -> String {
        format!("cal-{n_mels}-j{}s{}", self.joined, self.single)
    }
}

/// Samples of the 0.5 s gap between joined utterances.
const CAL_GAP: u64 = 8_000;
/// Samples of one 30 s piece.
const CAL_MAX: u64 = 480_000;

/// The numeric sort key of a LibriSpeech id (`speaker-chapter-utterance`).
fn id_key(id: &str) -> Option<(u64, u64, u64)> {
    let mut p = id.split('-').map(str::parse::<u64>);
    match (p.next(), p.next(), p.next(), p.next()) {
        (Some(Ok(s)), Some(Ok(c)), Some(Ok(u)), None) => Some((s, c, u)),
        _ => None,
    }
}

/// The calibration pieces (each a list of utterance files, joined with 0.5 s gaps) of `set`
/// from the dev-clean `corpus`, leaving out every utterance in `exclude` (the pinned subset,
/// whose dev-clean half is the dev WER set). Fixed before any measurement (T8.3c): speakers
/// in ascending numeric order, each speaker's utterances in ascending (chapter, utterance)
/// order; the first `single` utterances are a piece each; the rest are joined greedily into
/// pieces of at most 30 s (a piece is closed when the next utterance would not fit; an
/// utterance longer than 30 s is cut to 30 s by the writer), of which the first `joined` are
/// kept.
///
/// # Errors
/// An id that is not `speaker-chapter-utterance`.
pub fn calibration_plan(
    corpus: &[CalUtterance],
    exclude: &std::collections::HashSet<String>,
    set: CalibrationSet,
) -> Result<Vec<Vec<PathBuf>>> {
    let mut utts: Vec<((u64, u64, u64), &CalUtterance)> = Vec::new();
    for u in corpus.iter().filter(|u| !exclude.contains(&u.id)) {
        let key = id_key(&u.id).with_context(|| format!("bad LibriSpeech id {}", u.id))?;
        utts.push((key, u));
    }
    utts.sort_by_key(|(k, _)| *k);
    let mut pieces = Vec::new();
    for speaker in utts.chunk_by(|a, b| a.0.0 == b.0.0) {
        let (singles, rest) = speaker.split_at(set.single.min(speaker.len()));
        pieces.extend(singles.iter().map(|(_, u)| vec![u.path.clone()]));
        let mut joined: Vec<Vec<PathBuf>> = Vec::new();
        let (mut piece, mut len) = (Vec::new(), 0u64);
        for (_, u) in rest {
            if joined.len() == set.joined {
                break;
            }
            let need = u.samples.min(CAL_MAX) + if piece.is_empty() { 0 } else { CAL_GAP };
            if !piece.is_empty() && len + need > CAL_MAX {
                joined.push(std::mem::take(&mut piece));
                if joined.len() == set.joined {
                    break;
                }
                piece.push(u.path.clone());
                len = u.samples.min(CAL_MAX);
            } else {
                piece.push(u.path.clone());
                len += need;
            }
        }
        if !piece.is_empty() && joined.len() < set.joined {
            joined.push(piece);
        }
        pieces.extend(joined);
    }
    Ok(pieces)
}

/// Every utterance of the extracted LibriSpeech dev-clean corpus under `fixtures`, with its
/// length from the FLAC header.
///
/// # Errors
/// A missing corpus (with the fetch instruction) or an unreadable file.
pub fn dev_clean_corpus(fixtures: &Path) -> Result<Vec<CalUtterance>> {
    let root = fixtures.join("librispeech/dev-clean/LibriSpeech/dev-clean");
    let fetch = "run `cargo xtask fetch-fixtures --set librispeech`";
    let mut out = Vec::new();
    let list = |d: &Path| -> Result<Vec<PathBuf>> {
        let mut v = Vec::new();
        for e in std::fs::read_dir(d).with_context(|| format!("list {}; {fetch}", d.display()))? {
            v.push(e?.path());
        }
        Ok(v)
    };
    for speaker in list(&root)?.into_iter().filter(|p| p.is_dir()) {
        for chapter in list(&speaker)?.into_iter().filter(|p| p.is_dir()) {
            for f in list(&chapter)? {
                if f.extension().and_then(|e| e.to_str()) != Some("flac") {
                    continue;
                }
                let id = f
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .with_context(|| format!("bad file name {}", f.display()))?
                    .to_owned();
                let info = crate::flac::read(&f)?;
                ensure!(
                    info.sample_rate == 16_000 && info.channels == 1,
                    "{}: not 16 kHz mono",
                    f.display()
                );
                // STREAMINFO 0 means "unknown": the planner needs real lengths.
                ensure!(
                    info.total_samples > 0,
                    "{}: no length in its FLAC header",
                    f.display()
                );
                out.push(CalUtterance {
                    id,
                    path: f,
                    samples: info.total_samples,
                });
            }
        }
    }
    ensure!(
        !out.is_empty(),
        "no dev-clean utterances in {}; {fetch}",
        root.display()
    );
    Ok(out)
}

/// Every utterance id of the pinned subset (`fixtures/librispeech/subset.toml`, both
/// splits): never calibration data (its dev-clean half is the dev WER set, its test-clean
/// half the acceptance WER set).
///
/// # Errors
/// An unreadable subset file.
pub fn subset_ids(root: &Path) -> Result<std::collections::HashSet<String>> {
    let path = root.join("fixtures/librispeech/subset.toml");
    let doc: toml::Table = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?
        .parse()
        .with_context(|| format!("parse {}", path.display()))?;
    let utts = doc
        .get("utterance")
        .and_then(toml::Value::as_array)
        .with_context(|| format!("{} lists no utterances", path.display()))?;
    utts.iter()
        .map(|u| {
            u.get("id")
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
                .with_context(|| format!("{}: utterance without id", path.display()))
        })
        .collect()
}

/// The plan file the calibration example reads: one piece per line, its files separated by
/// tabs.
#[must_use]
pub fn plan_text(plan: &[Vec<PathBuf>]) -> String {
    plan.iter()
        .map(|p| {
            let files: Vec<String> = p.iter().map(|f| f.display().to_string()).collect();
            files.join("\t") + "\n"
        })
        .collect()
}

/// The executable of example `name` in cargo's JSON build messages.
#[must_use]
pub fn example_executable(messages: &str, name: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == name)
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
}

/// Runs `cmd` under `timeout` (`proc::run_with_timeout`), prints its output and fails
/// unless it succeeded.
fn run_step(cmd: &mut Command, what: &str, timeout: Duration) -> Result<()> {
    let done = run_with_timeout(cmd, timeout)
        .with_context(|| format!("{what} did not finish within {} s", timeout.as_secs()))?;
    eprint!("{}{}", done.stdout, done.stderr);
    ensure!(done.success, "{what} failed: {}", done.describe());
    Ok(())
}

/// Calibration pieces (`cal_NNNN.npy`, not the `_len` files) in `dir`.
///
/// # Errors
/// An unreadable directory.
pub fn count_pieces(dir: &Path) -> Result<usize> {
    let mut n = 0;
    for e in std::fs::read_dir(dir).with_context(|| format!("list {}", dir.display()))? {
        let name = e?.file_name().to_string_lossy().into_owned();
        if name.starts_with("cal_") && name.ends_with(".npy") && !name.ends_with("_len.npy") {
            n += 1;
        }
    }
    Ok(n)
}

/// The plan a calibration directory was written from ([`plan_text`]).
pub const PLAN_FILE: &str = "plan.txt";

/// Whether `dir` is a finished calibration directory written from exactly `plan` (its
/// [`plan_text`]): a changed selection, corpus or fixtures path writes the pieces again.
#[must_use]
pub fn calibration_is_current(dir: &Path, plan: &str) -> bool {
    finished_pieces(dir).is_some()
        && std::fs::read_to_string(dir.join(PLAN_FILE)).is_ok_and(|p| p == plan)
}

/// The piece count recorded in a finished calibration directory, `None` if it is not one.
#[must_use]
pub fn finished_pieces(dir: &Path) -> Option<usize> {
    std::fs::read_to_string(dir.join(PIECE_COUNT_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The converter's virtual environment, `target/qnn-convert/venv` below `root`.
#[must_use]
pub fn venv_dir(root: &Path) -> PathBuf {
    root.join("target").join("qnn-convert").join("venv")
}

/// Creates the converter's virtual environment ([`venv_dir`]) from CPython [`PYTHON`] on
/// PATH and installs the hash-pinned requirements, unless they are already installed (the
/// requirements' SHA-256 is recorded in the venv after a successful install, so a changed
/// pin is installed again; pip is idempotent). Returns the venv's interpreter.
///
/// # Errors
/// No or another Python on PATH, or a failed venv creation or install.
pub fn ensure_venv(root: &Path) -> Result<PathBuf> {
    let venv = venv_dir(root);
    if let Some(parent) = venv.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let python = venv_python(&venv);
    let requirements = root.join("xtask/python/requirements-qnn-convert.txt");
    let wanted = crate::download::sha256_file(&requirements)?;
    let marker = venv.join(REQUIREMENTS_MARKER);
    if !python.is_file() {
        let mut probe = Command::new("python");
        probe.arg("--version");
        let v = run_with_timeout(&mut probe, Duration::from_secs(60))
            .context("`python --version` did not answer")?;
        let version = format!("{}{}", v.stdout, v.stderr);
        if !v.success || !v.complete() || !python_matches(&version) {
            bail!(
                "qnn-convert needs CPython {}.{} on PATH (found: {}); the requirements are \
                 pinned for it",
                PYTHON.0,
                PYTHON.1,
                version.trim()
            );
        }
        eprintln!("qnn-convert: creating {}", venv.display());
        run_step(
            Command::new("python").arg("-m").arg("venv").arg(&venv),
            "python -m venv",
            VENV_TIMEOUT,
        )?;
    }
    if needs_install(std::fs::read_to_string(&marker).ok().as_deref(), &wanted) {
        let _ = std::fs::remove_file(&marker);
        run_step(
            Command::new(&python)
                .args(pip_args(&requirements))
                .envs(PIP_ENV),
            "pip install (hash-pinned)",
            PIP_TIMEOUT,
        )?;
        std::fs::write(&marker, format!("{wanted}\n"))
            .with_context(|| format!("write {}", marker.display()))?;
    }
    Ok(python)
}

/// Builds the conversion of `name` (module docs).
///
/// # Errors
/// An unknown model, missing sources or fixtures, a missing or wrong Python, or a failed step.
pub fn run(root: &Path, name: &str, recipe_name: &str, out: Option<&Path>) -> Result<()> {
    let c = convertible(name)?;
    let r = recipe(recipe_name)?;
    let models = crate::models::cache_dir()?;
    let fixtures = crate::fixtures::cache_dir()?;
    let source = models.join(c.source);
    for f in ["encoder.onnx", "decoder.onnx", "joiner.onnx", "tokens.txt"] {
        if !source.join(f).is_file() {
            bail!(
                "{} not found; run `cargo xtask fetch-models --set ci` (parakeet-tdt-0.6b-v3: \
                 `--set choice`)",
                source.join(f).display()
            );
        }
    }
    let base = root.join("target").join("qnn-convert");
    let calibration = base.join(r.calibration.dir_name(c.n_mels));
    let work = base.join(format!("{}-{}", c.name, r.name));
    let out = out.map_or_else(|| output_dir(&models, c.name), Path::to_path_buf);
    std::fs::create_dir_all(&base).with_context(|| format!("create {}", base.display()))?;

    // 1. Calibration pieces (production features of dev-clean speech), written to
    // `<dir>.part` and renamed only once complete, with their count and plan recorded. A
    // finished directory is reused only when its plan is the one computed now.
    let plan = calibration_plan(
        &dev_clean_corpus(&fixtures)?,
        &subset_ids(root)?,
        r.calibration,
    )?;
    ensure!(!plan.is_empty(), "the calibration plan is empty");
    let text = plan_text(&plan);
    if !calibration_is_current(&calibration, &text) {
        let part = calibration.with_extension("part");
        for d in [&part, &calibration] {
            if d.exists() {
                std::fs::remove_dir_all(d).with_context(|| format!("remove {}", d.display()))?;
            }
        }
        std::fs::create_dir_all(&part).with_context(|| format!("create {}", part.display()))?;
        let plan_file = part.join(PLAN_FILE);
        std::fs::write(&plan_file, &text)
            .with_context(|| format!("write {}", plan_file.display()))?;
        eprintln!(
            "qnn-convert: writing {} calibration pieces to {}",
            plan.len(),
            calibration.display()
        );
        // Built first and run directly: the deadline kills only the direct child.
        let mut build = cargo_command();
        build
            .args([
                "build",
                "--release",
                "-p",
                "earshot-onnx",
                "--example",
                CALIBRATION_EXAMPLE,
                "--message-format=json-render-diagnostics",
            ])
            .current_dir(root);
        let built = run_with_timeout(&mut build, STEP_TIMEOUT)
            .context("building parakeet_calibration did not finish within the deadline")?;
        if !built.success {
            bail!("building parakeet_calibration failed: {}", built.describe());
        }
        built.require_complete("building parakeet_calibration")?;
        let exe = example_executable(&built.stdout, CALIBRATION_EXAMPLE)
            .context("cargo reported no parakeet_calibration executable")?;
        let mut cmd = Command::new(exe);
        cmd.arg(c.n_mels.to_string())
            .arg(&plan_file)
            .arg(&part)
            .current_dir(root);
        run_step(&mut cmd, "parakeet_calibration", STEP_TIMEOUT)?;
        let n = count_pieces(&part)?;
        ensure!(
            n == plan.len(),
            "parakeet_calibration wrote {n} pieces, the plan has {}",
            plan.len()
        );
        std::fs::write(part.join(PIECE_COUNT_FILE), format!("{n}\n"))?;
        std::fs::rename(&part, &calibration)
            .with_context(|| format!("rename {} into place", part.display()))?;
    }
    let pieces = finished_pieces(&calibration).context("calibration pieces missing")?;
    ensure!(
        count_pieces(&calibration)? == pieces,
        "{} holds {} pieces, {} recorded; delete it and run again",
        calibration.display(),
        count_pieces(&calibration)?,
        pieces
    );

    // 2. The pinned Python environment.
    let python = ensure_venv(root)?;

    // 3. The conversion.
    let script = root.join("xtask/python/qnn_convert.py");
    eprintln!(
        "qnn-convert: converting {} with recipe {} into {}",
        c.name,
        r.name,
        out.display()
    );
    run_step(
        Command::new(&python)
            .args(convert_args(
                &script,
                &c,
                &r,
                &source,
                &calibration,
                pieces,
                &work,
                &out,
            ))
            .envs(PIP_ENV)
            .current_dir(root),
        "qnn_convert.py",
        CONVERT_TIMEOUT,
    )?;
    eprintln!(
        "qnn-convert: done; {} records the versions and hashes",
        out.join("conversion.json").display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qnn_convert_models_are_the_manifest_tdt_models() {
        let m = crate::manifest::Manifest::load(&crate::models::manifest_path(&crate::repo_root()))
            .unwrap();
        for c in CONVERTIBLE {
            let entry = m
                .models
                .iter()
                .find(|e| e.name == c.name)
                .unwrap_or_else(|| panic!("{} is not a manifest [[model]]", c.name));
            assert_eq!(entry.engine, "parakeet-tdt", "{}", c.name);
            // Its source directory is where the manifest puts its encoder.
            let encoder = format!("{}/encoder.onnx", c.source);
            assert!(
                m.artefacts.iter().any(|a| a.path == encoder),
                "{encoder} is not a manifest artefact"
            );
        }
        assert!(
            convertible("parakeet-ctc-110m").is_err(),
            "CTC is not converted"
        );
        assert!(convertible("base.en").is_err());
        assert_eq!(convertible("parakeet-tdt-110m").unwrap().n_mels, 80);
    }

    fn utt(id: &str, seconds: u64) -> CalUtterance {
        CalUtterance {
            id: id.into(),
            path: PathBuf::from(format!("{id}.flac")),
            samples: seconds * 16_000,
        }
    }

    fn names(plan: &[Vec<PathBuf>]) -> Vec<Vec<String>> {
        plan.iter()
            .map(|p| p.iter().map(|f| f.display().to_string()).collect())
            .collect()
    }

    #[test]
    fn qnn_convert_calibration_plan_follows_the_fixed_selection() {
        // Unsorted input; speaker 84 sorts before 174 numerically (not as text), chapter
        // 2 before 10, and the excluded (dev WER) utterance never appears.
        let corpus = [
            utt("174-50561-0000", 10),
            utt("84-10-0000", 12),
            utt("84-2-0001", 5),
            utt("84-2-0000", 20),
            utt("84-2-0002", 14),
            utt("84-2-0003", 14),
            utt("84-10-0001", 40),
            utt("84-10-0002", 3),
            utt("174-50561-0001", 9),
        ];
        let exclude: std::collections::HashSet<String> = ["84-2-0001".to_owned()].into();
        let plan = calibration_plan(
            &corpus,
            &exclude,
            CalibrationSet {
                joined: 2,
                single: 1,
            },
        )
        .unwrap();
        assert_eq!(
            names(&plan),
            vec![
                vec!["84-2-0000.flac"],
                // 14 + 0.5 + 14 = 28.5 s fits; + 0.5 + 12 would not.
                vec!["84-2-0002.flac", "84-2-0003.flac"],
                // 12 s, then the 40 s utterance (cut to 30 s) does not fit: closed. Only two
                // joined pieces are kept, so 84-10-0001 and 84-10-0002 are left out.
                vec!["84-10-0000.flac"],
                vec!["174-50561-0000.flac"],
                vec!["174-50561-0001.flac"],
            ]
        );
        assert!(
            calibration_plan(
                &[utt("x", 1)],
                &exclude,
                CalibrationSet {
                    joined: 1,
                    single: 0
                }
            )
            .is_err()
        );
        let text = plan_text(&plan);
        assert_eq!(text.lines().count(), 5);
        assert_eq!(text.lines().nth(1), Some("84-2-0002.flac\t84-2-0003.flac"));
    }

    /// A FLAC STREAMINFO header (as `flac::tests::head`): 16 kHz mono 16-bit, `total` samples.
    fn flac_head(total: u64) -> Vec<u8> {
        let rate = 16_000u32;
        let mut v = b"fLaC".to_vec();
        v.extend_from_slice(&[0x80, 0, 0, 34]);
        let mut si = [0u8; 34];
        si[10] = (rate >> 12) as u8;
        si[11] = (rate >> 4) as u8;
        si[12] = ((rate & 0xf) << 4) as u8;
        si[13] = (15 << 4) | ((total >> 32) as u8 & 0xf);
        si[14..18].copy_from_slice(&(total as u32).to_be_bytes());
        v.extend_from_slice(&si);
        v
    }

    #[test]
    fn qnn_convert_dev_clean_corpus_reads_lengths_and_refuses_unknown_ones() {
        let base = std::env::temp_dir().join(format!("earshot-qnn-corpus-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let chapter = base.join("librispeech/dev-clean/LibriSpeech/dev-clean/84/121123");
        std::fs::create_dir_all(&chapter).unwrap();
        std::fs::write(chapter.join("84-121123-0000.flac"), flac_head(32_000)).unwrap();
        std::fs::write(
            chapter.join("84-121123.trans.txt"),
            "84-121123-0000 HELLO\n",
        )
        .unwrap();
        let corpus = dev_clean_corpus(&base).unwrap();
        assert_eq!(corpus.len(), 1, "{corpus:?}");
        assert_eq!(corpus[0].id, "84-121123-0000");
        assert_eq!(corpus[0].samples, 32_000);
        // A header without a length (0, "unknown") is refused, never planned as empty.
        std::fs::write(chapter.join("84-121123-0001.flac"), flac_head(0)).unwrap();
        let e = dev_clean_corpus(&base).unwrap_err().to_string();
        assert!(e.contains("no length"), "{e}");
        let _ = std::fs::remove_dir_all(&base);
        assert!(dev_clean_corpus(&base).is_err(), "a missing corpus fails");
    }

    #[test]
    fn qnn_convert_reuses_calibration_pieces_only_for_the_same_plan() {
        let dir = std::env::temp_dir().join(format!("earshot-qnn-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plan = "a.flac\tb.flac\nc.flac\n";
        std::fs::write(dir.join(PLAN_FILE), plan).unwrap();
        // Not finished (no piece count): not current.
        assert!(!calibration_is_current(&dir, plan));
        std::fs::write(dir.join(PIECE_COUNT_FILE), "2\n").unwrap();
        assert!(calibration_is_current(&dir, plan));
        // Another selection or corpus is another plan.
        assert!(!calibration_is_current(&dir, "a.flac\nc.flac\n"));
        std::fs::remove_file(dir.join(PLAN_FILE)).unwrap();
        assert!(!calibration_is_current(&dir, plan), "no recorded plan");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn qnn_convert_calibration_dir_names_the_selection() {
        let s = CalibrationSet {
            joined: 4,
            single: 4,
        };
        assert_eq!(s.dir_name(80), "cal-80-j4s4");
    }

    #[test]
    fn qnn_convert_subset_ids_list_both_halves_of_the_pinned_subset() {
        // The exclusion itself is `calibration_plan`'s (tested above with an excluded id).
        let ids = subset_ids(&crate::repo_root()).unwrap();
        // Both halves: 196 dev-clean (the dev WER set) and 196 test-clean (acceptance).
        assert_eq!(ids.len(), 392);
        assert!(ids.contains("61-70968-0000"));
    }

    #[test]
    fn qnn_convert_static_frames_are_one_30_s_piece() {
        // 1 + 30 s * 16 kHz / 160-sample hop (earshot_onnx::parakeet::STATIC_FRAMES).
        assert_eq!(STATIC_FRAMES, 1 + 30 * 16_000 / 160);
    }

    #[test]
    fn qnn_convert_reinstalls_when_the_pins_change() {
        assert!(needs_install(None, "abc"));
        assert!(needs_install(Some("old\n"), "abc"));
        assert!(!needs_install(Some("abc\n"), "abc"));
    }

    #[test]
    fn qnn_convert_pip_never_asks_or_phones_home() {
        assert!(PIP_ENV.contains(&("PIP_DISABLE_PIP_VERSION_CHECK", "1")));
        assert!(PIP_ENV.contains(&("PIP_NO_INPUT", "1")));
    }

    #[test]
    fn qnn_convert_calibration_counts_only_finished_pieces() {
        let dir = std::env::temp_dir().join(format!("earshot-qnn-cal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in [
            "cal_0000.npy",
            "cal_0000_len.npy",
            "cal_0001.npy",
            "cal_0001_len.npy",
        ] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        assert_eq!(count_pieces(&dir).unwrap(), 2);
        // Without the count file the directory is not finished (an interrupted run).
        assert_eq!(finished_pieces(&dir), None);
        std::fs::write(dir.join(PIECE_COUNT_FILE), "2\n").unwrap();
        assert_eq!(finished_pieces(&dir), Some(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn qnn_convert_needs_the_pinned_python() {
        assert!(python_matches("Python 3.14.7\n"));
        assert!(python_matches("Python 3.14.0"));
        assert!(!python_matches("Python 3.13.9"));
        assert!(!python_matches("Python 3.140.1"));
        assert!(!python_matches("python 3.14.1"));
        assert!(!python_matches(""));
    }

    #[test]
    fn qnn_convert_installs_only_hash_pinned_wheels() {
        let args = pip_args(Path::new("req.txt"));
        for a in ["--require-hashes", "--no-deps", "--only-binary", ":all:"] {
            assert!(args.iter().any(|x| x == a), "{a}");
        }
        assert_eq!(args.last().map(String::as_str), Some("req.txt"));
        let text = std::fs::read_to_string(
            crate::repo_root().join("xtask/python/requirements-qnn-convert.txt"),
        )
        .unwrap();
        let pins: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .collect();
        assert!(pins.len() >= 3);
        for l in &pins {
            assert!(l.contains("==") && l.contains("--hash=sha256:"), "{l}");
        }
        assert!(pins.iter().any(|l| l.starts_with("onnxruntime==1.31.0 ")));
    }

    #[test]
    fn qnn_convert_writes_where_the_runtime_looks() {
        // earshot_runtime::backends::ParakeetModel::qnn_dir is `parakeet-qnn/<name>`.
        assert_eq!(
            output_dir(Path::new("M"), "parakeet-tdt-110m"),
            Path::new("M")
                .join("parakeet-qnn")
                .join("parakeet-tdt-110m")
        );
        let c = convertible("parakeet-tdt-0.6b-v2").unwrap();
        let p = Path::new("p");
        let r = recipe(DEFAULT_RECIPE).unwrap();
        let args = convert_args(p, &c, &r, p, p, 89, p, p).join(" ");
        assert!(args.contains("--n-mels 128"));
        assert!(args.contains("--pieces 89"));
        assert!(args.contains("--frames 3001"));
        assert!(args.contains(&format!("--recipe {DEFAULT_RECIPE}")));
        assert!(args.contains("--activations u16 --calibration-method minmax"));
        assert!(args.contains("--per-channel"));
        assert!(!args.contains("--a16-roles"));
        assert!(args.starts_with("-I "), "isolated mode");
        let mixed = recipe("a8w8pc-mixed-j2").unwrap();
        let args = convert_args(p, &c, &mixed, p, p, 80, p, p).join(" ");
        assert!(args.contains("--activations u8"));
        assert!(args.contains("--a16-roles LayerNormalization,Add,Softmax,Reshape"));
    }

    #[test]
    fn qnn_convert_script_source_uses_strided_calibration_and_length_overrides() {
        // A source pin only (T8.3c: ONNX Runtime 1.31's `CalibMaxIntermediateOutputs` drops
        // buffered outputs without reducing them, so T8.3 calibrated on 1 of 89 pieces). What
        // the converter computes is checked by
        // `qnn_convert_selftest_calibrates_every_piece_and_keeps_lengths_exact`.
        let script =
            std::fs::read_to_string(crate::repo_root().join("xtask/python/qnn_convert.py"))
                .unwrap();
        let code: String = script
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!code.contains("\"CalibMaxIntermediateOutputs\""));
        assert!(code.contains("stride=1"));
        assert!(code.contains("init_overrides=overrides"));
        assert!(code.contains("lengths = length_path_tensors(model_in)"));
    }

    #[test]
    #[ignore = "needs CPython 3.14 on PATH and the hash-pinned wheels (installed from PyPI into \
                target/qnn-convert/venv on first use): cargo test -p xtask --lib \
                qnn_convert_selftest -- --ignored"]
    fn qnn_convert_selftest_calibrates_every_piece_and_keeps_lengths_exact() {
        // Runs the real converter on a synthetic encoder in the pinned venv
        // (xtask/python/qnn_convert_selftest.py): the calibration range equals a single pass
        // over all nine pieces, every length-path tensor has scale 1/16 and zero point 0, and
        // the QDQ model's encoded lengths equal the float model's for every length 1..=3001.
        let root = crate::repo_root();
        let python = ensure_venv(&root).unwrap();
        let mut cmd = Command::new(python);
        cmd.arg("-I")
            .arg(root.join("xtask/python/qnn_convert_selftest.py"))
            .arg("--work")
            .arg(root.join("target/qnn-convert/selftest"))
            .envs(PIP_ENV)
            .current_dir(&root);
        let done = run_with_timeout(&mut cmd, Duration::from_secs(20 * 60)).unwrap();
        done.require_complete("qnn_convert_selftest.py").unwrap();
        assert!(
            done.success && done.stdout.contains("qnn_convert selftest: ok"),
            "{}\n{}\n{}",
            done.describe(),
            done.stdout,
            done.stderr
        );
    }

    #[test]
    fn qnn_convert_recipes_are_unique_and_the_default_exists() {
        let mut names: Vec<&str> = RECIPES.iter().map(|r| r.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), RECIPES.len());
        assert!(recipe(DEFAULT_RECIPE).is_ok());
        assert!(recipe("nope").is_err());
        for r in RECIPES {
            assert!(["u8", "u16"].contains(&r.activations), "{}", r.name);
            assert!(
                ["minmax", "entropy", "percentile"].contains(&r.method),
                "{}",
                r.name
            );
            // Mixed precision only lifts parts of an 8-bit graph.
            assert!(
                r.a16_roles.is_empty() || r.activations == "u8",
                "{}",
                r.name
            );
        }
    }
}
