//! `earshot-cli benchmark` (T6.10c2): the first-run model check. Reads the hardware, runs the
//! bundled two-voice clip through the candidate models as a two-track load (fastest
//! answer first), prints each trial and the recommendation, and records the answer in the
//! data directory, where `earshot-cli record` and `replay` use it when no `--model` is given.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use earshot_core::asr::BackendKind;
use earshot_runtime::backends::{accelerator_order, model_fetch_set};
use earshot_runtime::host;
use earshot_runtime::model_bench::{CLIP_ATTRIBUTION, CheckProgress, run_check};
use earshot_runtime::model_check::{RTF_COMFORTABLE, RTF_LIMIT, Verdict, WER_LIMIT_PERCENT};
use earshot_store::{DataDir, Store};

use crate::session::model_dir;

/// `earshot-cli benchmark` arguments.
#[derive(Debug, Clone, Args)]
pub struct BenchmarkArgs {
    /// Earshot data directory to record the answer in (the one `record` and `replay` use;
    /// created if missing).
    #[arg(long)]
    pub out: PathBuf,
    /// Meeting languages, comma-separated (default: English). Other languages choose from
    /// the multilingual models; the clip is English, so for them it checks speed and that
    /// the backend works, not per-language quality.
    #[arg(long, value_delimiter = ',', default_value = "en")]
    pub languages: Vec<String>,
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

fn verdict_text(v: Verdict) -> &'static str {
    match v {
        Verdict::Pass { comfortable: true } => "pass, comfortably fast",
        Verdict::Pass { comfortable: false } => "pass",
        Verdict::TooSlow => "too slow",
        Verdict::Wrong => "fails (wrong output or does not run)",
    }
}

/// Runs the check and prints it to `out`.
///
/// # Errors
/// Setup failures (data directory, ONNX Runtime), output errors.
pub fn run(args: &BenchmarkArgs, out: &mut dyn Write) -> Result<()> {
    let data = DataDir::new(&args.out);
    data.create()
        .with_context(|| format!("cannot create {}", args.out.display()))?;
    let models = model_dir(&data);
    // A build with CUDA loads ONNX Runtime's CUDA library (it serves the CPU too), so
    // Parakeet can be tried on the GPU; an ARM64 build with QNN registers the QNN plugin
    // into the CPU library, so Parakeet can be tried on the NPU; otherwise the CPU library.
    let mut unavailable = None;
    let onnx = host::init_onnx_for_check(&models, &accelerator_order(), &mut |library, e| {
        unavailable = Some(format!(
            "onnx runtime: the {library:?} library is unavailable ({e})"
        ));
    });
    if let Some(line) = unavailable {
        writeln!(out, "{line}")?;
    }
    onnx.map_err(|e| anyhow::anyhow!("{e}"))?;
    let languages: Vec<&str> = args.languages.iter().map(String::as_str).collect();
    writeln!(
        out,
        "model check: two-track test clip, too slow above {RTF_LIMIT} of real time, \
         comfortably fast at or below {RTF_COMFORTABLE}, wrong above {WER_LIMIT_PERCENT}% WER"
    )?;
    let mut progress = |p: &CheckProgress| {
        let line = match p {
            CheckProgress::Testing { model, backend } => {
                format!("testing {model} on {backend:?} ...")
            }
            CheckProgress::Tested { report, verdict } => {
                let t = &report.trial;
                match &report.failure {
                    Some(why) => format!(
                        "  {} on {:?}: {} ({why})",
                        t.model,
                        t.backend,
                        verdict_text(*verdict)
                    ),
                    None => format!(
                        "  {} on {:?}: {:.3} of real time, WER {:.1}%, loaded in {:.1} s: {}\n    \
                         backend proven by {}",
                        t.model,
                        t.backend,
                        t.rtf,
                        t.wer_percent,
                        report.load.as_secs_f64(),
                        verdict_text(*verdict),
                        report
                            .evidence
                            .as_deref()
                            .unwrap_or("(no evidence recorded)")
                    ),
                }
            }
        };
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    };
    let outcome =
        run_check(&models, &languages, &mut progress).map_err(|e| anyhow::anyhow!("{e}"))?;
    let gpus: Vec<String> = earshot_asr_whisper::gpu_devices()
        .iter()
        .map(|d| {
            format!(
                "{} {} ({})",
                d.name,
                d.description,
                if d.integrated {
                    "integrated"
                } else {
                    "discrete"
                }
            )
        })
        .collect();
    writeln!(
        out,
        "whisper.cpp GPUs: {} (the first discrete one of a backend is used, else the first integrated one)",
        if gpus.is_empty() {
            "none in this build".to_owned()
        } else {
            gpus.join(", ")
        }
    )?;
    let f = &outcome.facts;
    let accels: Vec<String> = f
        .accelerators
        .iter()
        .map(|a| format!("{:?}", a.backend))
        .collect();
    writeln!(
        out,
        "hardware: {} cores, {} memory, accelerators: {}",
        f.cores,
        if f.memory == u64::MAX {
            "unknown".into()
        } else {
            gb(f.memory)
        },
        if accels.is_empty() {
            "none".into()
        } else {
            accels.join(", ")
        }
    )?;
    let Some(r) = &outcome.recommendation else {
        writeln!(
            out,
            "recommendation: none (no installed model works for {} on this machine)",
            args.languages.join(", ")
        )?;
        writeln!(out, "{CLIP_ATTRIBUTION}")?;
        return Ok(());
    };
    writeln!(
        out,
        "recommendation: {} on {:?}; fallback when a meeting falls behind: {}",
        r.model,
        r.backend,
        r.fallback.unwrap_or("none")
    )?;
    for m in &r.missing {
        let set = model_fetch_set(m.model).unwrap_or("desktop");
        writeln!(
            out,
            "  better and not installed: {} ({}): cargo xtask fetch-models --set {set}, then run the check again",
            m.model,
            gb(m.size)
        )?;
    }
    let mut store = Store::open(&data.db_path())
        .with_context(|| format!("cannot open the database in {}", args.out.display()))?;
    host::record_model_check(&mut store, &models, &languages, &outcome)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    writeln!(
        out,
        "recorded in {}: `earshot-cli record` and `replay` use {} when no --model is given",
        args.out.display(),
        r.model
    )?;
    if r.backend == BackendKind::Cpu && !f.accelerators.is_empty() {
        writeln!(
            out,
            "note: no GPU answer beat the CPU (a Vulkan answer is kept only when it is a more \
             accurate model or the same model faster); the CPU answer is used"
        )?;
    }
    writeln!(out, "{CLIP_ATTRIBUTION}")?;
    Ok(())
}
