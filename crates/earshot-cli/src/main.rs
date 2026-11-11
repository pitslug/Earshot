//! Headless Earshot driver: transcribe, diarise, summarise, export, record and bench.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use earshot_capture_wasapi::{LoopbackRequest, MicRole};

mod benchmark;
mod events;
mod record;
mod record_raw;
mod replay;
mod session;
mod transcribe;

/// Headless Earshot driver.
#[derive(Debug, Parser)]
#[command(name = "earshot-cli", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Record the default microphone and the far end (a call app's audio via process
    /// loopback, or the loopback of the default speakers) to mic.wav (mono),
    /// loopback.wav (the loopback's channels, normally stereo; both 32-bit float at the
    /// native rate) and timestamps.json. Follows default-device changes. No echo
    /// cancellation, no resampling.
    RecordRaw {
        /// Recording length in seconds.
        #[arg(long)]
        seconds: f64,
        /// Output directory (created if missing).
        #[arg(long)]
        out: PathBuf,
        /// Which default capture device to use as the mic. Default-device loopback
        /// uses the console default render device, so `console` pairs devices of the
        /// same role.
        #[arg(long, value_enum, default_value_t = MicRoleArg::Console)]
        mic_role: MicRoleArg,
        /// Loopback source: `default-device` (the whole speaker output, the default and the
        /// reliable choice), `auto` (a call app with an active audio session, else the
        /// default speakers), `follow-call` (the default speakers, then the output a known call
        /// app plays on: on trial, T6.15), or a process name (`zoom.exe`) or pid. Per-app capture
        /// is not yet proven on call apps (T6.15).
        /// Names and pids never fall back to the default device.
        #[arg(
            long,
            default_value = "default-device",
            value_name = "auto|NAME|PID|default-device|follow-call"
        )]
        loopback_process: LoopbackRequest,
    },
    /// Transcribe a WAV or FLAC file with whisper.cpp: one batch decode (beam 5), or with
    /// `--stream` through the VAD-driven streaming chunker (greedy). Models come from
    /// EARSHOT_MODELS (default %LOCALAPPDATA%\Earshot\models).
    Transcribe(transcribe::TranscribeArgs),
    /// Replay a recorded mic (and optionally the far-end loopback) through the runtime as
    /// a live meeting: live transcript with the backend per track and AEC stats, then the
    /// final pass with diarisation, a summary only with --llm-url, and an Obsidian export
    /// only with --export-vault. Everything is stored in --out.
    Replay(replay::ReplayArgs),
    /// List the default microphones and the default render device (the loopback) with
    /// their mix formats, and the render audio sessions --loopback-process can name.
    Devices,
    /// Record a live meeting from the default mic and the far end through the runtime:
    /// live transcript with the backend per track and AEC stats until --seconds or Ctrl+C
    /// (a second Ctrl+C ends at once), then the final pass with diarisation, a summary only
    /// with --llm-url, and an Obsidian export only with --export-vault.
    Record(record::RecordArgs),
    /// Find the best transcription model this machine can run (the first-run model check):
    /// a short two-voice test clip through the candidate models as a two-track load, best
    /// first. Records the answer in --out, where `record` and `replay` use it when no
    /// --model is given.
    Benchmark(benchmark::BenchmarkArgs),
}

/// `--mic-role` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum MicRoleArg {
    /// Default console capture device.
    Console,
    /// Default communications capture device.
    Communications,
}

impl From<MicRoleArg> for MicRole {
    fn from(r: MicRoleArg) -> Self {
        match r {
            MicRoleArg::Console => MicRole::Console,
            MicRoleArg::Communications => MicRole::Communications,
        }
    }
}

fn main() -> anyhow::Result<()> {
    // The NPU compile helper (T12.12b): this exe in a hidden mode, so a run never closes an NPU
    // session it compiled with.
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if let Some(code) = earshot_runtime::npu_compiled::child_main(&args) {
        std::process::exit(code);
    }
    earshot_runtime::npu_compiled::use_this_exe();
    match Cli::parse().command {
        Command::RecordRaw {
            seconds,
            out,
            mic_role,
            loopback_process,
        } => {
            let o = record_raw::run(seconds, mic_role.into(), &loopback_process, &out)?;
            println!("{}", o.mic.display());
            println!("{}", o.loopback.display());
            println!("{}", o.timestamps.display());
        }
        Command::Transcribe(args) => transcribe::run(&args)?,
        Command::Replay(args) => {
            replay::run(&args, &mut std::io::stdout().lock())?;
        }
        Command::Devices => record::devices(&mut std::io::stdout().lock())?,
        Command::Record(args) => {
            record::run(&args, &mut std::io::stdout().lock())?;
        }
        Command::Benchmark(args) => {
            benchmark::run(&args, &mut std::io::stdout().lock())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_record_raw() {
        let cli =
            Cli::try_parse_from(["earshot", "record-raw", "--seconds", "5", "--out", "x"]).unwrap();
        let Command::RecordRaw {
            seconds,
            out,
            mic_role,
            loopback_process,
        } = cli.command
        else {
            panic!("expected record-raw");
        };
        assert_eq!(seconds, 5.0);
        assert_eq!(out, PathBuf::from("x"));
        assert_eq!(mic_role, MicRoleArg::Console, "console is the default");
        assert_eq!(
            loopback_process,
            LoopbackRequest::DefaultDevice,
            "the whole speaker output is the default (T6.14)"
        );
    }

    #[test]
    fn parses_loopback_process() {
        let parse = |v: &str| {
            Cli::try_parse_from([
                "earshot",
                "record-raw",
                "--seconds",
                "1",
                "--out",
                "x",
                "--loopback-process",
                v,
            ])
            .map(|c| {
                let Command::RecordRaw {
                    loopback_process, ..
                } = c.command
                else {
                    panic!("expected record-raw");
                };
                loopback_process
            })
        };
        let default =
            Cli::try_parse_from(["earshot", "record-raw", "--seconds", "1", "--out", "x"]).map(
                |c| match c.command {
                    Command::RecordRaw {
                        loopback_process, ..
                    } => loopback_process,
                    _ => panic!("expected record-raw"),
                },
            );
        assert_eq!(
            default.unwrap(),
            LoopbackRequest::DefaultDevice,
            "the whole speaker output by default (T6.14)"
        );
        assert_eq!(parse("auto").unwrap(), LoopbackRequest::Auto);
        assert_eq!(
            parse("follow-call").unwrap(),
            LoopbackRequest::FollowCallOutput
        );
        assert_eq!(
            parse("default-device").unwrap(),
            LoopbackRequest::DefaultDevice
        );
        assert_eq!(parse("1234").unwrap(), LoopbackRequest::Pid(1234));
        assert_eq!(
            parse("ms-teams.exe").unwrap(),
            LoopbackRequest::ProcessName("ms-teams.exe".into())
        );
        assert!(parse("").is_err());
        assert!(parse("0").is_err());
    }

    #[test]
    fn parses_mic_role() {
        let parse = |role: &str| {
            Cli::try_parse_from([
                "earshot",
                "record-raw",
                "--seconds",
                "1",
                "--out",
                "x",
                "--mic-role",
                role,
            ])
            .map(|c| {
                let Command::RecordRaw { mic_role, .. } = c.command else {
                    panic!("expected record-raw");
                };
                MicRole::from(mic_role)
            })
        };
        assert_eq!(parse("console").unwrap(), MicRole::Console);
        assert_eq!(parse("communications").unwrap(), MicRole::Communications);
        assert!(parse("multimedia").is_err());
    }

    #[test]
    fn parses_transcribe() {
        let cli = Cli::try_parse_from(["earshot", "transcribe", "a.wav", "--json"]).unwrap();
        let Command::Transcribe(args) = cli.command else {
            panic!("expected transcribe");
        };
        assert_eq!(args.input, PathBuf::from("a.wav"));
        assert!(args.json);
    }

    #[test]
    fn parses_replay_with_defaults_and_every_option() {
        let cli =
            Cli::try_parse_from(["earshot", "replay", "--mic", "a.wav", "--out", "d"]).unwrap();
        let Command::Replay(args) = cli.command else {
            panic!("expected replay");
        };
        assert_eq!(args.mic, PathBuf::from("a.wav"));
        assert_eq!(args.reference, None);
        assert_eq!(args.meeting.out, PathBuf::from("d"));
        assert_eq!(args.meeting.model, None);
        assert!(!args.meeting.no_post);
        assert_eq!(args.meeting.llm_url, None, "no summary endpoint by default");
        assert_eq!(args.meeting.export_vault, None, "no export by default");
        assert_eq!(
            args.meeting.summary_input, None,
            "no summary input file by default"
        );

        let cli = Cli::try_parse_from([
            "earshot",
            "replay",
            "--summary-input",
            "s.json",
            "--mic",
            "a.wav",
            "--ref",
            "b.wav",
            "--out",
            "d",
            "--model",
            "tiny.en",
            "--title",
            "Sync",
            "--no-post",
            "--llm-url",
            "http://127.0.0.1:8080/v1",
            "--llm-model",
            "m",
            "--export-vault",
            "v",
        ])
        .unwrap();
        let Command::Replay(args) = cli.command else {
            panic!("expected replay");
        };
        assert_eq!(args.reference, Some(PathBuf::from("b.wav")));
        assert_eq!(args.meeting.model.as_deref(), Some("tiny.en"));
        assert_eq!(args.meeting.title, "Sync");
        assert!(args.meeting.no_post);
        assert_eq!(
            args.meeting.llm_url.as_deref(),
            Some("http://127.0.0.1:8080/v1")
        );
        assert_eq!(args.meeting.llm_model.as_deref(), Some("m"));
        assert_eq!(args.meeting.export_vault, Some(PathBuf::from("v")));
        assert_eq!(args.meeting.summary_input, Some(PathBuf::from("s.json")));
    }

    #[test]
    fn replay_needs_mic_and_out_and_the_endpoint_with_its_model() {
        let base = ["earshot", "replay", "--mic", "a.wav", "--out", "d"];
        assert!(Cli::try_parse_from(["earshot", "replay", "--mic", "a.wav"]).is_err());
        assert!(Cli::try_parse_from(["earshot", "replay", "--out", "d"]).is_err());
        let with = |extra: &[&str]| {
            let mut v = base.to_vec();
            v.extend_from_slice(extra);
            Cli::try_parse_from(v)
        };
        // The summary settings are checked together (before anything is recorded).
        let summary = |extra: &[&str], key: Option<&str>| {
            let Command::Replay(args) = with(extra).unwrap().command else {
                panic!("expected replay");
            };
            let key = key.map(str::to_string);
            session::summary_config(&args.meeting, move |name| {
                assert_eq!(
                    name, "ANTHROPIC_API_KEY",
                    "the key comes only from this variable"
                );
                key.clone()
            })
        };
        assert!(
            summary(&[], None).unwrap().is_none(),
            "no summary by default"
        );
        assert!(summary(&["--llm-url", "http://127.0.0.1:1/v1"], None).is_err());
        assert!(summary(&["--llm-model", "m"], None).is_err());
        assert!(summary(&["--llm-provider", "openai"], None).is_err());
        let c = summary(
            &["--llm-url", "http://127.0.0.1:1/v1", "--llm-model", "m"],
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(c.provider, earshot_llm::Provider::OpenAi);
        assert_eq!(
            (c.endpoint.as_str(), c.model.as_str()),
            ("http://127.0.0.1:1/v1", "m")
        );
        assert!(c.api_key.is_none());

        // Anthropic: opt-in by provider, defaults for URL and model, key from the
        // environment only (there is no key argument).
        let err = summary(&["--llm-provider", "anthropic"], None).unwrap_err();
        assert!(format!("{err}").contains("ANTHROPIC_API_KEY"), "{err}");
        assert!(summary(&["--llm-provider", "anthropic"], Some("  ")).is_err());
        let c = summary(&["--llm-provider", "anthropic"], Some("sk-ant-x"))
            .unwrap()
            .unwrap();
        assert_eq!(c.provider, earshot_llm::Provider::Anthropic);
        assert_eq!(
            (c.endpoint.as_str(), c.model.as_str()),
            ("https://api.anthropic.com", "claude-sonnet-5-5")
        );
        assert_eq!(c.api_key, earshot_llm::ApiKey::new("sk-ant-x"));
        let c = summary(
            &[
                "--llm-provider",
                "anthropic",
                "--llm-url",
                "http://127.0.0.1:9",
                "--llm-model",
                "claude-x",
            ],
            Some("k"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (c.endpoint.as_str(), c.model.as_str()),
            ("http://127.0.0.1:9", "claude-x")
        );
        assert!(with(&["--llm-provider", "claude"]).is_err());
        assert!(with(&["--llm-key", "k"]).is_err(), "no key argument");

        // Claude Code (T7.4ga): no endpoint, no key, the model from --llm-model.
        let claude = |extra: &[&str]| {
            let Command::Replay(args) = with(extra).unwrap().command else {
                panic!("expected replay");
            };
            session::claude_code_config(&args.meeting)
        };
        assert!(
            summary(&["--llm-provider", "claude-code"], None)
                .unwrap()
                .is_none()
        );
        assert_eq!(claude(&[]).unwrap(), None);
        let c = claude(&["--llm-provider", "claude-code"]).unwrap().unwrap();
        assert_eq!(c.model, None, "Claude Code's own default");
        let c = claude(&["--llm-provider", "claude-code", "--llm-model", "opus"])
            .unwrap()
            .unwrap();
        assert_eq!(c.model.as_deref(), Some("opus"));
        assert!(claude(&["--llm-provider", "claude-code", "--llm-model=--tools"]).is_err());
        assert!(claude(&["--llm-provider", "claude-code", "--llm-model", "op us"]).is_err());
        assert!(
            claude(&[
                "--llm-provider",
                "claude-code",
                "--llm-url",
                "http://127.0.0.1:1"
            ])
            .is_err()
        );
    }

    #[test]
    fn cli_codex_flag_plan_and_its_skip_lines() {
        use earshot_llm::CodexError;
        use earshot_runtime::backends::ModelDir;
        use session::SummaryPlan;
        let parse = |extra: &[&str]| {
            let mut argv = vec!["earshot", "replay", "--mic", "a.wav", "--out", "d"];
            argv.extend_from_slice(extra);
            let Command::Replay(args) = Cli::try_parse_from(argv).unwrap().command else {
                panic!("expected replay");
            };
            args
        };
        // Codex (T7.4gc): no endpoint, no key, the model from --llm-model.
        let codex = |extra: &[&str]| session::codex_config(&parse(extra).meeting);
        assert_eq!(codex(&[]).unwrap(), None);
        assert_eq!(codex(&["--llm-provider", "claude-code"]).unwrap(), None);
        let c = codex(&["--llm-provider", "codex"]).unwrap().unwrap();
        assert_eq!(c.model, None, "Codex's own default");
        let c = codex(&["--llm-provider", "codex", "--llm-model", "gpt-5.6-luna"])
            .unwrap()
            .unwrap();
        assert_eq!(c.model.as_deref(), Some("gpt-5.6-luna"));
        assert!(codex(&["--llm-provider", "codex", "--llm-model=-s"]).is_err());
        assert!(codex(&["--llm-provider", "codex", "--llm-model", "gpt 5"]).is_err());
        assert!(codex(&["--llm-provider", "codex", "--llm-url", "http://127.0.0.1:1"]).is_err());
        assert!(
            session::summary_config(&parse(&["--llm-provider", "codex"]).meeting, |_| None)
                .unwrap()
                .is_none(),
            "no endpoint for Codex"
        );
        assert!(
            session::claude_code_config(&parse(&["--llm-provider", "codex"]).meeting)
                .unwrap()
                .is_none(),
            "never Claude Code instead"
        );

        let models = ModelDir::new(std::env::temp_dir().join("earshot-cli-codex-none"));
        let args = parse(&["--llm-provider", "codex", "--llm-model", "gpt-5.6-luna"]);
        let plan = session::summary_plan(&args.meeting, |_| None, None, false, &models).unwrap();
        assert!(
            matches!(&plan, SummaryPlan::Codex(c) if c.model.as_deref() == Some("gpt-5.6-luna")),
            "{plan:?}"
        );
        assert!(session::no_summary_lines(&plan).is_none());
        let (line, network) =
            session::no_summary_lines(&SummaryPlan::CodexUnavailable(CodexError::NotSignedIn))
                .unwrap();
        assert!(line.contains("Codex is not signed in"), "{line}");
        assert_eq!(network, "Codex cannot run");
    }

    #[test]
    fn cli_claude_code_plan_and_its_skip_lines() {
        use earshot_llm::ClaudeCodeError;
        use earshot_runtime::backends::ModelDir;
        use session::SummaryPlan;
        let models = ModelDir::new(std::env::temp_dir().join("earshot-cli-claude-code-none"));
        let Command::Replay(args) = Cli::try_parse_from([
            "earshot",
            "replay",
            "--mic",
            "a.wav",
            "--out",
            "d",
            "--llm-provider",
            "claude-code",
            "--llm-model",
            "sonnet",
        ])
        .unwrap()
        .command
        else {
            panic!("expected replay");
        };
        let plan = session::summary_plan(&args.meeting, |_| None, None, false, &models).unwrap();
        assert!(
            matches!(&plan, SummaryPlan::ClaudeCode(c) if c.model.as_deref() == Some("sonnet")),
            "{plan:?}"
        );
        assert!(session::no_summary_lines(&plan).is_none());
        let (line, network) = session::no_summary_lines(&SummaryPlan::ClaudeCodeUnavailable(
            ClaudeCodeError::NotSignedIn,
        ))
        .unwrap();
        assert!(line.contains("Claude Code is not signed in"), "{line}");
        assert_eq!(network, "Claude Code cannot run");
    }

    /// A temp models directory removed on drop (std only, AGENTS.md).
    struct TempModels(PathBuf);

    impl TempModels {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!(
                "earshot-cli-summary-models-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }

        /// Installs `id` as a sparse file of the manifest size.
        fn install(&self, id: &str) {
            let m = earshot_runtime::summary_model::summary_model(id).unwrap();
            let path = self.0.join(m.file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::File::create(&path)
                .unwrap()
                .set_len(m.size)
                .unwrap();
        }
    }

    impl Drop for TempModels {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn replay_summarises_locally_by_default_unless_told_otherwise() {
        use earshot_runtime::backends::ModelDir;
        use earshot_runtime::summary_model::ChoiceReason;
        use session::SummaryPlan;
        const GIB: u64 = 1 << 30;
        let base = ["earshot", "replay", "--mic", "a.wav", "--out", "d"];
        let with = |extra: &[&str]| {
            let mut v = base.to_vec();
            v.extend_from_slice(extra);
            Cli::try_parse_from(v)
        };
        // On the GPU, where the memory rule picks the model; `cpu_plan` on the CPU (T8.3f).
        let on = |extra: &[&str], memory: Option<u64>, gpu: bool, models: &ModelDir| {
            let Command::Replay(args) = with(extra).unwrap().command else {
                panic!("expected replay");
            };
            session::summary_plan(&args.meeting, |_| None, memory, gpu, models)
        };
        let plan = |extra: &[&str], memory: Option<u64>, models: &ModelDir| {
            on(extra, memory, true, models)
        };
        let Command::Replay(args) = with(&[]).unwrap().command else {
            panic!("expected replay");
        };
        assert!(!args.meeting.no_summary);
        assert_eq!(args.meeting.summary_model, None);

        let dir = TempModels::new("plan");
        let models = ModelDir::new(&dir.0);
        // Nothing installed: one line and no summary, never an endpoint.
        let p = plan(&[], Some(64 * GIB), &models).unwrap();
        let SummaryPlan::NotInstalled(choice, why) = &p else {
            panic!("{p:?}");
        };
        assert_eq!(choice.model.id, "qwen3.5-9b-q4_k_m");
        assert!(why.contains("fetch-models --set summary"), "{why}");
        // Its line names the fetch command once, and the network line says why there is no
        // summary (not "no summary endpoint configured").
        let (line, network) = session::no_summary_lines(&p).unwrap();
        assert_eq!(
            line.matches("fetch-models --set summary").count(),
            1,
            "{line}"
        );
        assert!(line.contains("qwen3.5-9b-q4_k_m"), "{line}");
        assert_eq!(network, "no local summary model installed");
        let (line, network) = session::no_summary_lines(&SummaryPlan::Skip).unwrap();
        assert_eq!(line, "summary: skipped (--no-summary)");
        assert_eq!(network, "summary skipped");
        // The 9B where memory allows (a 16 GB machine reports a little less), else the 4B.
        dir.install("qwen3.5-9b-q4_k_m");
        let p = plan(&[], Some(15 * GIB), &models).unwrap();
        assert!(
            matches!(&p, SummaryPlan::Local(c) if c.model.id == "qwen3.5-9b-q4_k_m"),
            "{p:?}"
        );
        // A 12 GB machine wants the 4B, which is not installed.
        let p = plan(&[], Some(12 * GIB), &models).unwrap();
        assert!(
            matches!(&p, SummaryPlan::NotInstalled(c, _) if c.model.id == "qwen3.5-4b-q4_k_m"),
            "{p:?}"
        );
        // On the CPU even a 64 GB machine wants the 4B (not installed yet), and says why.
        let p = on(&[], Some(64 * GIB), false, &models).unwrap();
        assert!(
            matches!(&p, SummaryPlan::NotInstalled(c, _)
                if c.model.id == "qwen3.5-4b-q4_k_m" && c.reason == ChoiceReason::OnTheCpu),
            "{p:?}"
        );
        dir.install("qwen3.5-4b-q4_k_m");
        let p = plan(&[], None, &models).unwrap();
        assert!(
            matches!(&p, SummaryPlan::Local(c)
                if c.model.id == "qwen3.5-4b-q4_k_m" && c.reason == ChoiceReason::MemoryUnknown),
            "{p:?}"
        );
        // --summary-model picks a table model whatever the memory.
        let p = plan(
            &["--summary-model", "qwen3.5-4b-q4_k_m"],
            Some(64 * GIB),
            &models,
        )
        .unwrap();
        assert!(
            matches!(&p, SummaryPlan::Local(c)
                if c.model.id == "qwen3.5-4b-q4_k_m" && c.reason == ChoiceReason::Requested),
            "{p:?}"
        );
        // An unknown id, or a named model that is not installed, fails before recording.
        let e = plan(
            &["--summary-model", "gemma-4-12b-it-qat-q4_0"],
            None,
            &models,
        )
        .unwrap_err();
        assert!(format!("{e}").contains("qwen3.5-9b-q4_k_m"), "{e}");
        let empty = ModelDir::new(dir.0.join("empty"));
        let e = plan(&["--summary-model", "qwen3.5-4b-q4_k_m"], None, &empty).unwrap_err();
        assert!(format!("{e}").contains("fetch-models --set summary"), "{e}");
        // --no-summary: nothing, even with a model installed.
        assert!(matches!(
            plan(&["--no-summary"], Some(64 * GIB), &models).unwrap(),
            SummaryPlan::Skip
        ));
        // An endpoint wins over the local model.
        assert!(matches!(
            plan(
                &["--llm-url", "http://127.0.0.1:1/v1", "--llm-model", "m"],
                Some(64 * GIB),
                &models
            )
            .unwrap(),
            SummaryPlan::Remote(_)
        ));
        // Contradictory settings do not parse.
        for bad in [
            &["--no-summary", "--llm-url", "http://127.0.0.1:1/v1"][..],
            &["--no-summary", "--llm-provider", "anthropic"],
            &["--no-summary", "--summary-model", "qwen3.5-4b-q4_k_m"],
            &["--summary-model", "qwen3.5-4b-q4_k_m", "--llm-model", "m"],
            &[
                "--summary-model",
                "qwen3.5-4b-q4_k_m",
                "--llm-provider",
                "openai",
            ],
        ] {
            assert!(with(bad).is_err(), "{bad:?}");
        }
        // record takes the same options.
        let Command::Record(args) =
            Cli::try_parse_from(["earshot", "record", "--out", "d", "--no-summary"])
                .unwrap()
                .command
        else {
            panic!("expected record");
        };
        assert!(args.meeting.no_summary);
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "off-Windows behaviour; the Windows case is its own test"
    )]
    fn local_summary_dlls_are_never_missing_off_windows() {
        let dir = TempModels::new("dlls-off");
        assert!(session::missing_llama_dlls(std::slice::from_ref(&dir.0)).is_empty());
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "llama.cpp DLLs are a Windows concern")]
    fn local_summary_dlls_are_looked_for_beside_the_exe_and_on_path() {
        let dir = TempModels::new("dlls");
        let (a, b) = (dir.0.join("a"), dir.0.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let dirs = [a.clone(), b.clone()];
        let missing = session::missing_llama_dlls(&dirs);
        assert_eq!(missing, session::LLAMA_DLLS);
        assert!(session::LLAMA_DLLS.contains(&"llama.dll"));
        // Spread over the directories counts.
        std::fs::write(a.join("llama.dll"), "").unwrap();
        std::fs::write(b.join("ggml.dll"), "").unwrap();
        let missing = session::missing_llama_dlls(&dirs);
        assert!(!missing.contains(&"llama.dll") && !missing.contains(&"ggml.dll"));
        assert!(missing.contains(&"ggml-base.dll"), "{missing:?}");
        // The CUDA runtime is found by name prefix (its major version follows the build).
        let prefixes = ["cublas64_", "cublasLt64_"];
        assert_eq!(
            session::missing_dll_prefixes(&dirs, &prefixes),
            ["cublas64_*.dll", "cublasLt64_*.dll"]
        );
        std::fs::write(b.join("CUBLAS64_13.DLL"), "").unwrap();
        std::fs::write(a.join("cublasLt64_13.lib"), "").unwrap();
        assert_eq!(
            session::missing_dll_prefixes(&dirs, &prefixes),
            ["cublasLt64_*.dll"],
            "case is ignored; a .lib is not the DLL"
        );
        std::fs::write(a.join("cublasLt64_13.dll"), "").unwrap();
        assert!(session::missing_dll_prefixes(&dirs, &prefixes).is_empty());
        assert_eq!(
            session::CUDA_RUNTIME_DLL_PREFIXES.is_empty(),
            !cfg!(feature = "cuda"),
            "checked only in a cuda build"
        );
        for n in session::LLAMA_DLLS {
            std::fs::write(b.join(n), "").unwrap();
        }
        assert!(session::missing_llama_dlls(&dirs).is_empty());
        // A directory of that name is not the DLL.
        let c = dir.0.join("c");
        std::fs::create_dir_all(c.join("llama.dll")).unwrap();
        assert!(session::missing_llama_dlls(&[c]).contains(&"llama.dll"));
    }

    #[test]
    fn replay_whole_pass_and_event_log_are_off_by_default_and_parse() {
        let base = ["earshot", "replay", "--mic", "a.wav", "--out", "d"];
        let Command::Replay(args) = Cli::try_parse_from(base).unwrap().command else {
            panic!("expected replay");
        };
        assert!(!args.meeting.whole_pass, "the cleanup reuses the live work");
        assert_eq!(args.events_json, None);
        let mut v = base.to_vec();
        v.extend_from_slice(&["--whole-pass", "--events-json", "e.jsonl"]);
        let Command::Replay(args) = Cli::try_parse_from(v).unwrap().command else {
            panic!("expected replay");
        };
        assert!(args.meeting.whole_pass);
        assert_eq!(args.events_json, Some(PathBuf::from("e.jsonl")));
        // A whole pass without a cleanup makes no sense.
        let mut v = base.to_vec();
        v.extend_from_slice(&["--whole-pass", "--no-post"]);
        assert!(Cli::try_parse_from(v).is_err());
    }

    #[test]
    fn parses_record_and_devices() {
        let cli = Cli::try_parse_from(["earshot", "record", "--out", "d"]).unwrap();
        let Command::Record(args) = cli.command else {
            panic!("expected record");
        };
        assert_eq!(args.seconds, None, "until Ctrl+C by default");
        assert_eq!(args.mic_role, MicRoleArg::Console);
        assert_eq!(
            args.loopback_process,
            LoopbackRequest::DefaultDevice,
            "the whole speaker output by default (T6.14)"
        );
        assert_eq!(args.meeting.out, PathBuf::from("d"));
        assert_eq!(args.meeting.export_vault, None, "no export by default");
        assert_eq!(args.meeting.llm_url, None, "no summary endpoint by default");

        let cli = Cli::try_parse_from([
            "earshot",
            "record",
            "--seconds",
            "30",
            "--out",
            "d",
            "--mic-role",
            "communications",
            "--loopback-process",
            "zoom.exe",
            "--model",
            "tiny.en",
            "--no-post",
        ])
        .unwrap();
        let Command::Record(args) = cli.command else {
            panic!("expected record");
        };
        assert_eq!(args.seconds, Some(30.0));
        assert_eq!(args.mic_role, MicRoleArg::Communications);
        assert_eq!(
            args.loopback_process,
            LoopbackRequest::ProcessName("zoom.exe".into())
        );
        assert!(args.meeting.no_post);
        assert!(Cli::try_parse_from(["earshot", "record", "--seconds", "5"]).is_err());

        let cli = Cli::try_parse_from(["earshot", "devices"]).unwrap();
        assert!(matches!(cli.command, Command::Devices));
    }

    #[test]
    fn parses_the_live_summaries_flag_on_record_and_replay() {
        use session::OnOff;
        for base in [
            vec!["earshot", "record", "--out", "d"],
            vec!["earshot", "replay", "--mic", "a.wav", "--out", "d"],
        ] {
            let parse = |extra: &[&str]| {
                let mut v = base.clone();
                v.extend_from_slice(extra);
                Cli::try_parse_from(v).map(|cli| match cli.command {
                    Command::Record(a) => a.meeting,
                    Command::Replay(a) => a.meeting,
                    _ => panic!("expected record or replay"),
                })
            };
            let m = parse(&[]).unwrap();
            assert_eq!((m.live_summaries, m.save_live_summaries), (None, false));
            let m = parse(&["--live-summaries", "on"]).unwrap();
            assert_eq!(
                (m.live_summaries, m.save_live_summaries),
                (Some(OnOff::On), false)
            );
            let m = parse(&["--live-summaries", "off", "--save-live-summaries"]).unwrap();
            assert_eq!(
                (m.live_summaries, m.save_live_summaries),
                (Some(OnOff::Off), true)
            );
            assert!(parse(&["--live-summaries", "yes"]).is_err());
            assert!(
                parse(&["--save-live-summaries"]).is_err(),
                "nothing to save"
            );
        }
    }

    #[test]
    fn parses_the_speaker_detection_flag_on_record_and_replay() {
        use session::SpeakersArg;
        for base in [
            vec!["earshot", "record", "--out", "d"],
            vec!["earshot", "replay", "--mic", "a.wav", "--out", "d"],
        ] {
            let parse = |extra: &[&str]| {
                let mut v = base.clone();
                v.extend_from_slice(extra);
                Cli::try_parse_from(v).map(|cli| match cli.command {
                    Command::Record(a) => a.meeting,
                    Command::Replay(a) => a.meeting,
                    _ => panic!("expected record or replay"),
                })
            };
            let m = parse(&[]).unwrap();
            assert_eq!(
                (m.speakers, m.save_speakers),
                (None, false),
                "stored setting"
            );
            for (name, value) in [
                ("during", SpeakersArg::During),
                ("after", SpeakersArg::After),
                ("after-on-battery", SpeakersArg::AfterOnBattery),
            ] {
                let m = parse(&["--speakers", name]).unwrap();
                assert_eq!((m.speakers, m.save_speakers), (Some(value), false));
                let m = parse(&["--speakers", name, "--save-speakers"]).unwrap();
                assert_eq!((m.speakers, m.save_speakers), (Some(value), true));
            }
            assert!(parse(&["--speakers", "never"]).is_err());
            assert!(parse(&["--save-speakers"]).is_err(), "nothing to save");
        }
    }

    #[test]
    fn speaker_detection_help_carries_the_after_the_call_warning() {
        for sub in ["record", "replay"] {
            let mut cmd = Cli::command();
            let help = cmd
                .find_subcommand_mut(sub)
                .unwrap()
                .render_long_help()
                .to_string();
            let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
            let warning = earshot_runtime::speaker_timing::AFTER_CALL_WARNING
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(flat.contains(&warning), "{sub}: {help}");
            assert!(flat.contains("after-on-battery"), "{sub}");
        }
    }

    #[test]
    fn parses_record_in_room_and_refuses_it_with_a_loopback() {
        use earshot_capture_wasapi::CaptureMode;

        let cli = Cli::try_parse_from(["earshot", "record", "--out", "d"]).unwrap();
        let Command::Record(args) = cli.command else {
            panic!("expected record");
        };
        assert!(!args.in_room, "a call by default");
        assert_eq!(
            args.capture_mode(),
            CaptureMode::Call(LoopbackRequest::DefaultDevice)
        );
        let cli = Cli::try_parse_from(["earshot", "record", "--out", "d", "--in-room"]).unwrap();
        let Command::Record(args) = cli.command else {
            panic!("expected record");
        };
        assert!(args.in_room);
        assert_eq!(args.capture_mode(), CaptureMode::InRoom);
        assert!(
            Cli::try_parse_from([
                "earshot",
                "record",
                "--out",
                "d",
                "--in-room",
                "--loopback-process",
                "zoom.exe",
            ])
            .is_err(),
            "no loopback in the room"
        );
    }

    #[test]
    fn record_raw_requires_both_args() {
        assert!(Cli::try_parse_from(["earshot", "record-raw", "--seconds", "5"]).is_err());
        assert!(Cli::try_parse_from(["earshot", "record-raw", "--out", "x"]).is_err());
    }
}
