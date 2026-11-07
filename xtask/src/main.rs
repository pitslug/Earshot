//! Project automation (`cargo xtask <command>`).
//!
//! Commands so far: ci, purity, public-guard, doctor, accept, fetch-fixtures, fetch-models, schema,
//! sum-eval, echo-nl, aec-heldout-baseline, bench.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Earshot project automation.
#[derive(Debug, Parser)]
#[command(name = "cargo xtask", bin_name = "cargo xtask", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
#[allow(
    clippy::large_enum_variant,
    reason = "parsed once per process; boxing the sum-eval options buys nothing; `allow`, because \
              whether the lint fires depends on the second-largest variant"
)]
enum Cmd {
    /// Run fmt --check, clippy -D warnings, tests, purity, the net check and the schema
    /// drift check; stop at the first failure.
    Ci,
    /// Check that earshot-core stays free of OS dependencies (PLAN.md 3.2).
    Purity,
    /// Check that a public repo holds no private details: a names list kept outside the
    /// repo, personal commit emails, recordings, LOG.md, gates/. Passes with one visible line
    /// in a repo that tracks no PUBLIC_REPO marker.
    PublicGuard {
        /// Fail when the names list is missing or empty, instead of warning.
        #[arg(long)]
        require_list: bool,
    },
    /// Release build for the host or a cross target (whisper.cpp settings per target).
    /// Without --features: the workspace; with: the earshot CLI with those features.
    Build {
        /// Target triple, for example aarch64-pc-windows-msvc (default: the host). An explicit
        /// x86_64-pc-windows-msvc ignores CMAKE_GENERATOR and CMAKE_*_COMPILER* in the shell,
        /// which on an ARM64 host name the ARM64 compiler.
        #[arg(long)]
        target: Option<String>,
        /// Features, comma-separated: cuda, vulkan (of the CLI, or of the app with --app).
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,
        /// Cargo target directory (default: <repo>\target when short enough for the
        /// Vulkan shader build, else %SystemDrive%\eb\host, else %SystemDrive%\eb,
        /// which a cross target needs).
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
        /// Build the desktop app as it ships, its front end compiled in, instead of the
        /// workspace or the CLI.
        #[arg(long)]
        app: bool,
        /// Build the app (as --app does) and its unsigned per-user NSIS installer.
        #[arg(long)]
        bundle: bool,
        /// Build the combined CUDA set of llama.cpp's DLLs into <target-dir>\\cuda-set.
        #[arg(long)]
        cuda_set: bool,
    },
    /// Install the host's NSIS build from `build --bundle` into <repo>\target, replay a meeting
    /// through the installed app, and uninstall it.
    BundleCheck {
        /// The --target-dir of the bundle build.
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
    },
    /// Delete one short build directory, %SystemDrive%\eb\<NAME> (refused whole while a
    /// build is using it).
    CleanBuild {
        /// Folder name under %SystemDrive%\eb, for example lane-t623c.
        name: String,
    },
    /// Report host toolchains and SDKs; fail only if a required item is missing.
    Doctor {
        /// Print machine-readable JSON instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Run a milestone's acceptance checks against acceptance/thresholds.toml.
    ///
    /// Writes target/acceptance/<milestone>.json and acceptance/results/<milestone>.json.
    Accept {
        /// Milestone to accept: m0 .. m11.
        milestone: String,
        /// Backend to run on; recorded in the report and used by milestones that support
        /// it. It never changes which thresholds apply.
        #[arg(long, value_parser = ["cpu", "cuda", "vulkan", "qnn"])]
        backend: Option<String>,
    },
    /// Download, check and extract fixture sets into EARSHOT_FIXTURES
    /// (default %LOCALAPPDATA%\earshot-dev\fixtures).
    FetchFixtures {
        /// Sets to fetch, comma-separated: smoke, librispeech, ami, all.
        #[arg(long, value_delimiter = ',', required = true)]
        set: Vec<String>,
        /// Re-hash existing downloads and re-derive the pinned subsets from the corpus.
        #[arg(long)]
        verify: bool,
        /// Regenerate fixtures/librispeech/subset.toml and the smoke set from the corpus.
        #[arg(long)]
        pin: bool,
    },
    /// Download and check models into EARSHOT_MODELS (default %LOCALAPPDATA%\Earshot\models).
    FetchModels {
        /// Sets to fetch, comma-separated: ci, desktop, all.
        #[arg(long, value_delimiter = ',', default_value = "ci")]
        set: Vec<String>,
        /// Only these models of the sets (manifest ids, comma-separated).
        #[arg(long, value_delimiter = ',')]
        model: Vec<String>,
        /// Re-hash models that are already present.
        #[arg(long)]
        verify: bool,
    },
    /// Check schemas/summary.v1.json against the Rust summary types (or regenerate it).
    Schema {
        /// Regenerate the committed schema instead of checking it.
        #[arg(long)]
        write: bool,
    },
    /// Evaluate the transcription models (T6.10e): WER, word timing, echo leak, invented
    /// words and speed per model on one backend. Writes docs/model-eval/<backend>.json.
    ModelEval {
        /// Backend: cpu, cuda, vulkan or qnn (default: cuda when doctor reports it). Vulkan
        /// evaluates the whisper models only (ONNX Runtime has no Vulkan provider); qnn is the
        /// NPU on Windows ARM64 and Qualcomm's HTP simulator on x64 (Parakeet only).
        #[arg(long, value_parser = ["cpu", "cuda", "vulkan", "qnn"])]
        backend: Option<String>,
        /// Candidate models, comma-separated (default: all).
        #[arg(long, value_delimiter = ',')]
        model: Vec<String>,
        /// Corpora, comma-separated: librispeech, ami, gate12, silence (the default), and
        /// speed (10 min of AMI, real-time factor only, for CPU and NPU runs), smoke (the 20
        /// committed clips), librispeech_dev (the pinned subset's dev-clean half, never NPU
        /// calibration data) and ami_sdm (the ami rule on the single distant microphone,
        /// informational).
        #[arg(long, value_delimiter = ',')]
        corpus: Vec<String>,
        /// Whisper decodings, comma-separated: greedy, beam (default: both).
        #[arg(long, value_delimiter = ',')]
        decoding: Vec<String>,
        /// Report path (default docs/model-eval/<backend>.json).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Build in this directory (a Vulkan build otherwise uses the short directory of
        /// `cargo xtask build`).
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
    },
    /// WER parity of whisper.cpp on Vulkan with the CPU (T8.2): every Vulkan ladder model
    /// through model-eval into docs/model-eval/vulkan-parity, judged from the recorded logs.
    VulkanParity {
        /// Judge the reports already there.
        #[arg(long)]
        skip_run: bool,
        /// Build in this directory (a Vulkan build otherwise uses the short directory of
        /// `cargo xtask build`).
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
    },
    /// Real-time factor, live chunk latency and peak memory of one part on one backend (T8.1),
    /// on the benchmark meeting. Writes acceptance/results/bench-<host>-<backend>.json.
    Bench {
        /// The part: asr, diar or aec (diar and aec run on the CPU only, as in production).
        #[arg(value_parser = ["asr", "diar", "aec"])]
        part: String,
        /// Backend: cpu, cuda, vulkan or qnn (default: cuda when doctor reports it).
        #[arg(long, value_parser = ["cpu", "cuda", "vulkan", "qnn"])]
        backend: Option<String>,
        /// asr only: another model than the production default for the backend.
        #[arg(long)]
        model: Option<String>,
        /// What else the host was doing; recorded beside the timings.
        #[arg(long)]
        note: Option<String>,
        /// Build in this directory (a Vulkan build otherwise uses the short directory of
        /// `cargo xtask build`).
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
    },
    /// Summary-quality evaluation over AMI meetings (T6.20): every candidate local model
    /// summarises the AMI reference transcripts and Earshot's own replayed transcripts,
    /// scored against the human summaries. Report in target/sum-eval.
    SumEval {
        /// Backend: cpu, cuda or vulkan (default: cuda when doctor reports it). Vulkan runs
        /// llama.cpp on a Vulkan GPU (T8.3h); it does not change Earshot-source replays.
        #[arg(long, value_parser = ["cpu", "cuda", "vulkan"])]
        backend: Option<String>,
        /// Candidate model ids from the llm-eval set, comma-separated (default: all). Or
        /// `claude-code` (`claude-code/<model>` for a Claude Code `--model`) alone, with
        /// `--source reference` (T7.4gb): Claude through the installed Claude Code CLI as
        /// the production provider summarises; with `--chunk-seconds` one map per chunk. Or
        /// `codex` (`codex/<model>`) alone the same way (T7.4gc): OpenAI through Codex.
        #[arg(long, value_delimiter = ',')]
        model: Vec<String>,
        /// Splits, comma-separated: dev, test (default: both).
        #[arg(long, value_delimiter = ',')]
        split: Vec<String>,
        /// Transcript sources, comma-separated: reference, earshot (default: both; earshot
        /// on the test split only).
        #[arg(long, value_delimiter = ',')]
        source: Vec<String>,
        /// Only the first N meetings of each split.
        #[arg(long)]
        limit: Option<usize>,
        /// Output directory (default target/sum-eval).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Replay the meetings through the CLI again instead of reusing cached inputs.
        #[arg(long)]
        rerun_earshot: bool,
        /// Only replay the test meetings for the Earshot source (no summaries).
        #[arg(long)]
        replay_only: bool,
        /// Re-score the finished result files in --out with the current scoring rules (no
        /// generation) and rewrite the report.
        #[arg(long)]
        rescore: bool,
        /// Write the exact Anthropic requests of every meeting into this directory (for a
        /// Claude ceiling run by Claude Code agents) instead of summarising.
        #[arg(long, value_name = "DIR")]
        export_prompts: Option<std::path::PathBuf>,
        /// Score the agents' `<meeting>.<source>.reply.json` files in this directory.
        #[arg(long, value_name = "DIR", requires = "label")]
        import_replies: Option<std::path::PathBuf>,
        /// Candidate name of the imported replies (e.g. claude-opus-5-5-medium-agent).
        #[arg(long)]
        label: Option<String>,
        /// Context window of the local models in tokens.
        #[arg(long, default_value_t = 16_384)]
        context: u32,
        /// Ceiling of any one reply budget in tokens (each request gets a budget sized to
        /// what it summarises, at most this).
        #[arg(long, default_value_t = 8192)]
        max_output: usize,
        /// CPU threads for the local models.
        #[arg(long)]
        threads: Option<i32>,
        /// Cargo target directory for the release builds.
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
        /// Forced chunking: a map request per this many seconds of meeting time, then the
        /// usual reduce (the T6.20 chunking experiment; rule in docs/summary-eval.md).
        #[arg(long)]
        chunk_seconds: Option<u32>,
        /// The shared prompt rules (T6.20b prompt tuning; docs/summary-eval.md): current
        /// (the production prompt), owners, decisions, items, lean, assignments, handed, checked or previous (the production prompt before T6.20f). Local runs only.
        #[arg(long, default_value = "current")]
        prompt_variant: earshot_core::prompt::PromptVariant,
        /// Two-call chunks (T6.20c, evaluation only): per chunk one request for the action
        /// items and one for everything else, then a merge that folds reworded actions.
        /// Needs --chunk-seconds.
        #[arg(long, requires = "chunk_seconds")]
        two_call: bool,
        /// Two-track chunks (T8.3ja, docs/live-summaries.md option A): chunks mapped from a
        /// "Me" / "Them" copy of the transcript so far, re-cited onto the real one, then the
        /// last chunk and the reduce. Needs --chunk-seconds and --source reference.
        #[arg(long, requires = "chunk_seconds", conflicts_with = "two_call")]
        two_track: bool,
        /// Only these meetings, comma-separated (each in a selected split).
        #[arg(long, value_delimiter = ',')]
        meeting: Vec<String>,
        /// The Codex CLI of a `--model codex` run (evaluation only, T7.4gc): without it
        /// Codex is found as the app finds it (PATH, then npm's global folder).
        #[arg(long, value_name = "PATH")]
        codex_cli: Option<std::path::PathBuf>,
    },
    /// Build the NPU conversion of a Parakeet TDT model (T8.3): static 30 s input shape,
    /// QDQ quantisation for Qualcomm's HTP backend, into <models>/parakeet-qnn/<name>.
    /// Needs CPython 3.14 on PATH (a hash-pinned venv in target/qnn-convert).
    QnnConvert {
        /// The model: parakeet-tdt-110m, parakeet-tdt-0.6b-v2 or parakeet-tdt-0.6b-v3.
        #[arg(long)]
        model: String,
        /// The conversion recipe (calibration set and quantisation settings; default: the
        /// one T8.3c chose, `xtask::qnn_convert::DEFAULT_RECIPE`).
        #[arg(long, default_value = xtask::qnn_convert::DEFAULT_RECIPE)]
        recipe: String,
        /// Output directory (default: <models>/parakeet-qnn/<model>, where the runtime
        /// looks).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Record what the QNN plugin sends to Windows event tracing (ETW, the telemetry
    /// channel) on this machine (T8.3): providers, keywords, event and field names (no
    /// values). Fails on any telemetry-keyword event or field not reviewed as system
    /// information, or on any field holding words of a transcribed smoke clip.
    QnnEtw,
    /// Measure loudspeaker-path nonlinearity of a raw recording and map it onto the call
    /// simulator's soft clip (T1.2a). Without --dir, prints only the calibration table.
    EchoNl {
        /// Raw recording directory (mic.wav, loopback.wav, timestamps.json).
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
        /// Print per-block levels and classes.
        #[arg(long)]
        verbose: bool,
    },
    /// Regenerate acceptance/aec_heldout_baseline.json from the current canceller on the
    /// held-out set (T1.6b). Every use must be logged in LOG.md with the reason.
    AecHeldoutBaseline,
    /// Generate the benchmark meeting shipped with the app (T7.4c) from the AMI fixtures;
    /// with --check, compare a fresh generation with the committed files.
    BenchMeeting {
        /// Compare with the committed files instead of writing them.
        #[arg(long)]
        check: bool,
    },
    /// Make the release key for in-app updates, once (T9.1b): prints its public half.
    UpdateKey {
        /// Where the key goes: outside the repo, never over an existing file.
        #[arg(long)]
        out: std::path::PathBuf,
    },
    /// Sign the installer `build --bundle` left, for an in-app update (T9.1b).
    SignUpdate {
        /// The release key `update-key` made.
        #[arg(long)]
        key: std::path::PathBuf,
        /// The installer's target (default: the host).
        #[arg(long)]
        target: Option<String>,
        /// The folder `build --bundle` used.
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
    },
    /// T6.13 measurements: generate the test meetings, time the single pass on a realtime
    /// replay, compare it with the whole-meeting pass. Reports in target/t613/.
    T613 {
        #[command(subcommand)]
        what: T613Cmd,
    },
}

/// `cargo xtask t613` subcommands.
#[derive(Debug, Subcommand)]
enum T613Cmd {
    /// Generate the meetings into the fixture cache (generated/t613/).
    Meeting {
        /// LibriSpeech meeting length in minutes.
        #[arg(long, default_value_t = 60)]
        minutes: u32,
        /// Also generate the AMI meetings.
        #[arg(long)]
        ami: bool,
        /// Generate again even when cached.
        #[arg(long)]
        regenerate: bool,
    },
    /// Replay the LibriSpeech meeting at real-time speed and time every stretch and the
    /// final transcript (run on a quiet host).
    Timing {
        /// Meeting length in minutes.
        #[arg(long, default_value_t = 60)]
        minutes: u32,
        /// Transcription model (default: the model check's answer on this host).
        #[arg(long)]
        model: Option<String>,
        /// Build backend (default: cuda when doctor reports it).
        #[arg(long, value_parser = ["cpu", "cuda"])]
        backend: Option<String>,
        /// Cargo target directory for the CLI build (short paths for CUDA builds).
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
        /// Generate the meeting again.
        #[arg(long)]
        regenerate: bool,
    },
    /// Compare the single pass with the whole-meeting pass (WER, DER, agreement).
    Quality {
        /// Inputs, comma-separated: librispeech, ami, gate12.
        #[arg(long, value_delimiter = ',', default_value = "librispeech,ami,gate12")]
        inputs: Vec<String>,
        /// LibriSpeech meeting length in minutes.
        #[arg(long, default_value_t = 10)]
        minutes: u32,
        /// Transcription model (default: the model check's answer on this host).
        #[arg(long)]
        model: Option<String>,
        /// Build backend (default: cuda when doctor reports it).
        #[arg(long, value_parser = ["cpu", "cuda"])]
        backend: Option<String>,
        /// Cargo target directory for the CLI build.
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
        /// Private fixtures directory with the GATE-12 recordings (default
        /// <repo>/fixtures/private).
        #[arg(long)]
        private: Option<std::path::PathBuf>,
        /// Replay at wall-clock speed (live speaker windows are then reused).
        #[arg(long)]
        realtime: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = xtask::repo_root();
    let result = match cli.command {
        Cmd::Ci => xtask::ci::run(&root).map(|()| true),
        Cmd::Purity => xtask::ci::purity(&root).map(|()| true),
        Cmd::PublicGuard { require_list } => xtask::public_guard::run_with(
            &root,
            &xtask::public_guard::Config::from_env(require_list),
        )
        .map(|()| true),
        Cmd::Build {
            target,
            features,
            target_dir,
            app,
            bundle,
            cuda_set,
        } => if cuda_set {
            xtask::build::cuda_set(&root, target_dir.as_deref()).map(|set| {
                println!("{}", set.display());
            })
        } else if bundle {
            xtask::bundle::run(&root, target.as_deref(), &features, target_dir.as_deref())
                .map(|_| ())
        } else if app {
            xtask::build::app(&root, target.as_deref(), &features, target_dir.as_deref()).map(
                |exe| {
                    println!("{}", exe.display());
                },
            )
        } else {
            xtask::build::run(&root, target.as_deref(), &features, target_dir.as_deref())
        }
        .map(|()| true),
        Cmd::BundleCheck { target_dir } => xtask::bundle::check(&root, target_dir.as_deref()),
        Cmd::CleanBuild { name } => xtask::build::clean_build(&name),
        Cmd::Doctor { json } => xtask::doctor::run_doctor(json),
        Cmd::Accept { milestone, backend } => {
            xtask::accept::run(&root, &milestone, backend.as_deref())
        }
        Cmd::FetchFixtures { set, verify, pin } => {
            xtask::fixtures::run(&root, &set, verify, pin).map(|()| true)
        }
        Cmd::FetchModels { set, model, verify } => {
            xtask::models::run(&root, &set, &model, verify).map(|()| true)
        }
        Cmd::Schema { write } => xtask::schema::run(&root, write).map(|()| true),
        Cmd::ModelEval {
            backend,
            model,
            corpus,
            decoding,
            out,
            target_dir,
        } => xtask::model_eval::run(
            &root,
            &xtask::model_eval::Options {
                backend,
                models: model,
                corpora: corpus,
                decodings: decoding,
                out,
                target_dir,
            },
        )
        .map(|()| true),
        Cmd::VulkanParity {
            skip_run,
            target_dir,
        } => xtask::vulkan_parity::run(&root, skip_run, target_dir),
        Cmd::Bench {
            part,
            backend,
            model,
            note,
            target_dir,
        } => xtask::bench::run(
            &root,
            &xtask::bench::Options {
                part,
                backend,
                model,
                note,
                target_dir,
            },
        )
        .map(|()| true),
        Cmd::SumEval {
            backend,
            model,
            split,
            source,
            limit,
            out,
            rerun_earshot,
            replay_only,
            rescore,
            export_prompts,
            import_replies,
            label,
            context,
            max_output,
            threads,
            target_dir,
            chunk_seconds,
            prompt_variant,
            two_call,
            two_track,
            meeting,
            codex_cli,
        } => xtask::sum_eval::run(
            &root,
            &xtask::sum_eval::Options {
                backend,
                models: model,
                splits: split,
                sources: source,
                limit,
                out,
                rerun_earshot,
                replay_only,
                rescore,
                export_prompts,
                import_replies,
                label,
                context,
                max_output,
                threads,
                target_dir,
                chunk_seconds,
                prompt_variant,
                two_call,
                two_track,
                meetings: meeting,
                codex_cli,
            },
        )
        .map(|()| true),
        Cmd::QnnConvert { model, recipe, out } => {
            xtask::qnn_convert::run(&root, &model, &recipe, out.as_deref()).map(|()| true)
        }
        Cmd::QnnEtw => xtask::qnn_etw::run(&root),
        Cmd::AecHeldoutBaseline => xtask::accept::m1::write_heldout_baseline(&root).map(|()| true),
        Cmd::UpdateKey { out } => xtask::update::make_key(&root, &out).map(|public| {
            println!("public key: {public}");
            println!("keep {} safe and out of the repo", out.display());
            true
        }),
        Cmd::SignUpdate {
            key,
            target,
            target_dir,
        } => xtask::update::run_sign(&root, &key, target.as_deref(), target_dir.as_deref())
            .map(|_| true),
        Cmd::BenchMeeting { check } => xtask::bench_meeting::run(&root, check).map(|()| true),
        Cmd::EchoNl { dir, verbose } => {
            xtask::echo_nl::run(&root, dir.as_deref(), verbose).map(|()| true)
        }
        Cmd::T613 { what } => run_t613(&root, what).map(|()| true),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_t613(root: &std::path::Path, what: T613Cmd) -> anyhow::Result<()> {
    use xtask::t613::{Build, meeting, quality, timing};
    match what {
        T613Cmd::Meeting {
            minutes,
            ami,
            regenerate,
        } => {
            let m = meeting::librispeech(root, minutes, regenerate)?;
            eprintln!("t613: {} in {}", m.truth.name, m.dir.display());
            if ami {
                for name in xtask::fixtures::AMI_MEETINGS {
                    let m = meeting::ami(root, name, regenerate)?;
                    eprintln!("t613: {} in {}", m.truth.name, m.dir.display());
                }
            }
        }
        T613Cmd::Timing {
            minutes,
            model,
            backend,
            target_dir,
            regenerate,
        } => {
            timing::run(
                root,
                &timing::TimingArgs {
                    minutes,
                    model,
                    build: Build {
                        backend,
                        target_dir,
                    },
                    regenerate,
                },
            )?;
        }
        T613Cmd::Quality {
            inputs,
            minutes,
            model,
            backend,
            target_dir,
            private,
            realtime,
        } => {
            quality::run(
                root,
                &quality::QualityArgs {
                    inputs,
                    minutes,
                    model,
                    build: Build {
                        backend,
                        target_dir,
                    },
                    private,
                    realtime,
                },
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn t613_parses_its_subcommands_with_defaults() {
        let cli = Cli::try_parse_from(["xtask", "t613", "timing"]).unwrap();
        match cli.command {
            Cmd::T613 {
                what:
                    T613Cmd::Timing {
                        minutes,
                        model,
                        backend,
                        ..
                    },
            } => {
                assert_eq!(minutes, 60, "the plan's 60-minute meeting by default");
                assert_eq!(model, None, "the model check's answer by default");
                assert_eq!(backend, None);
            }
            other => panic!("{other:?}"),
        }
        let cli =
            Cli::try_parse_from(["xtask", "t613", "quality", "--inputs", "ami,gate12"]).unwrap();
        assert!(matches!(
            cli.command,
            Cmd::T613 { what: T613Cmd::Quality { inputs, .. } } if inputs == ["ami", "gate12"]
        ));
        assert!(Cli::try_parse_from(["xtask", "t613", "timing", "--backend", "vulkan"]).is_err());
    }

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn accept_parses_milestone_and_backend() {
        let cli = Cli::try_parse_from(["xtask", "accept", "m2", "--backend", "cuda"]).unwrap();
        match cli.command {
            Cmd::Accept { milestone, backend } => {
                assert_eq!(milestone, "m2");
                assert_eq!(backend.as_deref(), Some("cuda"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn backend_is_restricted_to_known_values() {
        for b in ["cpu", "cuda", "vulkan", "qnn"] {
            assert!(
                Cli::try_parse_from(["xtask", "accept", "m2", "--backend", b]).is_ok(),
                "{b}"
            );
        }
        for b in ["gpu", "CPU", ""] {
            assert!(
                Cli::try_parse_from(["xtask", "accept", "m2", "--backend", b]).is_err(),
                "{b}"
            );
        }
    }

    #[test]
    fn fetch_fixtures_takes_comma_separated_sets() {
        let cli = Cli::try_parse_from([
            "xtask",
            "fetch-fixtures",
            "--set",
            "smoke,librispeech",
            "--verify",
        ])
        .unwrap();
        match cli.command {
            Cmd::FetchFixtures { set, verify, pin } => {
                assert_eq!(set, ["smoke", "librispeech"]);
                assert!(verify);
                assert!(!pin);
            }
            other => panic!("{other:?}"),
        }
        assert!(Cli::try_parse_from(["xtask", "fetch-fixtures"]).is_err());
    }

    #[test]
    fn fetch_models_defaults_to_the_ci_set() {
        let cli = Cli::try_parse_from(["xtask", "fetch-models", "--verify"]).unwrap();
        match cli.command {
            Cmd::FetchModels { set, model, verify } => {
                assert_eq!(set, ["ci"]);
                assert!(model.is_empty());
                assert!(verify);
            }
            other => panic!("{other:?}"),
        }
        let cli = Cli::try_parse_from(["xtask", "fetch-models", "--set", "ci,desktop"]).unwrap();
        assert!(matches!(cli.command, Cmd::FetchModels { set, .. } if set == ["ci", "desktop"]));
        let cli = Cli::try_parse_from([
            "xtask",
            "fetch-models",
            "--set",
            "llm-eval",
            "--model",
            "a,b",
        ])
        .unwrap();
        assert!(matches!(cli.command, Cmd::FetchModels { model, .. } if model == ["a", "b"]));
    }

    #[test]
    fn sum_eval_defaults_and_filters() {
        let cli = Cli::try_parse_from(["xtask", "sum-eval"]).unwrap();
        let Cmd::SumEval {
            backend,
            model,
            context,
            max_output,
            rerun_earshot,
            ..
        } = cli.command
        else {
            panic!("expected sum-eval");
        };
        assert_eq!(backend, None);
        assert!(model.is_empty());
        assert_eq!((context, max_output), (16_384, 8192));
        assert!(!rerun_earshot);
        let cli = Cli::try_parse_from([
            "xtask",
            "sum-eval",
            "--backend",
            "cpu",
            "--model",
            "qwen3-4b-q4_k_m,qwen3-8b-q4_k_m",
            "--split",
            "dev",
            "--limit",
            "2",
        ])
        .unwrap();
        let Cmd::SumEval {
            backend,
            model,
            split,
            limit,
            ..
        } = cli.command
        else {
            panic!("expected sum-eval");
        };
        assert_eq!(backend.as_deref(), Some("cpu"));
        assert_eq!(model.len(), 2);
        assert_eq!(split, ["dev"]);
        assert_eq!(limit, Some(2));
        // Vulkan summaries were added for T8.3h; a backend sum-eval cannot run is still refused.
        assert!(Cli::try_parse_from(["xtask", "sum-eval", "--backend", "vulkan"]).is_ok());
        assert!(Cli::try_parse_from(["xtask", "sum-eval", "--backend", "qnn"]).is_err());
    }

    #[test]
    fn echo_nl_takes_an_optional_recording_dir() {
        let cli = Cli::try_parse_from(["xtask", "echo-nl", "--dir", "rec", "--verbose"]).unwrap();
        match cli.command {
            Cmd::EchoNl { dir, verbose } => {
                assert_eq!(dir.as_deref(), Some(std::path::Path::new("rec")));
                assert!(verbose);
            }
            other => panic!("{other:?}"),
        }
        let cli = Cli::try_parse_from(["xtask", "echo-nl"]).unwrap();
        assert!(matches!(
            cli.command,
            Cmd::EchoNl {
                dir: None,
                verbose: false
            }
        ));
    }

    #[test]
    fn build_bundle_flag_and_bundle_check_parse() {
        let cli = Cli::try_parse_from([
            "xtask",
            "build",
            "--bundle",
            "--target",
            "aarch64-pc-windows-msvc",
        ])
        .unwrap();
        match cli.command {
            Cmd::Build { bundle, target, .. } => {
                assert!(bundle);
                assert_eq!(target.as_deref(), Some("aarch64-pc-windows-msvc"));
            }
            other => panic!("{other:?}"),
        }
        let cli = Cli::try_parse_from(["xtask", "bundle-check", "--target-dir", "t"]).unwrap();
        assert!(matches!(cli.command, Cmd::BundleCheck { .. }));
    }

    #[test]
    fn clean_build_takes_one_folder_name() {
        let cli = Cli::try_parse_from(["xtask", "clean-build", "lane-t623c"]).unwrap();
        match cli.command {
            Cmd::CleanBuild { name } => assert_eq!(name, "lane-t623c"),
            other => panic!("{other:?}"),
        }
        assert!(Cli::try_parse_from(["xtask", "clean-build"]).is_err());
    }

    #[test]
    fn only_planned_subcommands_exist() {
        let names: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "ci",
                "purity",
                "public-guard",
                "build",
                "bundle-check",
                "clean-build",
                "doctor",
                "accept",
                "fetch-fixtures",
                "fetch-models",
                "schema",
                "model-eval",
                "vulkan-parity",
                "bench",
                "sum-eval",
                "qnn-convert",
                "qnn-etw",
                "echo-nl",
                "aec-heldout-baseline",
                "bench-meeting",
                "update-key",
                "sign-update",
                "t613"
            ]
        );
    }
}
