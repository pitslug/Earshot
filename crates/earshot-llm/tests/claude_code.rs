//! `ClaudeCodeClient` and `LlmSummarizer` against a fake `claude` CLI (T7.4ga).
//!
//! This test binary is its own fake: started as `<exe> --fake-claude <scenario dir> <args>`
//! it behaves like the CLI (module `fake`), so no script, interpreter or real CLI is needed
//! (std only). That needs `harness = false` (Cargo.toml): `main` runs the cases itself,
//! takes a name filter like libtest, and runs `#[ignore]`-like cases only with `--ignored`
//! or `--include-ignored`.
//!
//! The one case over the real Claude Code (`claude_code_real_cli_summarises_a_short_fixed_transcript`)
//! is ignored: `cargo test -p earshot-llm --test claude_code -- --ignored`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use earshot_core::asr::{CancelToken, MeetingTime};
use earshot_core::identity::{EntityId, ManualClock};
use earshot_core::prompt::{MeetingInfo, PromptSegment, SummaryInput, Template};
use earshot_core::summary::validate_summary;
use earshot_llm::claude_code::{CliSearch, Effort, MAX_CLI_OUTPUT, STATUS_TIMEOUT};
use earshot_llm::{
    ClaudeCli, ClaudeCodeClient, ClaudeCodeConfig, ClaudeCodeError, Handover, HandoverOutcome,
    HandoverSink, LlmError, LlmSummarizer, ResponseMode, SignIn, SummaryRun,
};
use serde_json::json;

const T0: u64 = 1_790_000_000_000;

// ---------------------------------------------------------------------------------------
// The fake CLI.

mod fake {
    use std::io::{Read, Write};
    use std::path::Path;

    fn bump(path: &Path) -> u32 {
        let n = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| t.trim().parse::<u32>().ok())
            .unwrap_or(0)
            + 1;
        std::fs::write(path, n.to_string()).unwrap();
        n
    }

    /// `<exe> --fake-sleep <pid file>`: records its pid and sleeps (a grandchild).
    pub fn sleeper(pid_file: &str) -> ! {
        std::fs::write(pid_file, std::process::id().to_string()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(120));
        std::process::exit(0)
    }

    /// `<exe> --fake-claude <dir> <args...>` (module docs of the test).
    pub fn main(dir: &Path, args: &[String]) -> ! {
        if args == ["--version"] {
            println!("9.9.9 (Claude Code)");
            std::process::exit(0);
        }
        if args == ["--help"] {
            bump(&dir.join("help-calls"));
            // `help.txt` stands in for another version's help; the default lists `--effort`
            // as Claude Code 2.1.292 does.
            let help = std::fs::read_to_string(dir.join("help.txt")).unwrap_or_else(|_| {
                "Usage: claude [options]

Options:
  --effort <level>  Effort level for the current session
                    (low, medium, high, xhigh, max)
  --model <model>   Model for the current session
"
                .into()
            });
            print!("{help}");
            std::process::exit(0);
        }
        if args.len() >= 2 && args[0] == "auth" && args[1] == "status" {
            bump(&dir.join("auth-calls"));
            if let Ok(ms) = std::fs::read_to_string(dir.join("auth-sleep")) {
                std::thread::sleep(std::time::Duration::from_millis(ms.trim().parse().unwrap()));
            }
            let status = std::fs::read_to_string(dir.join("auth.json"))
                .unwrap_or_else(|_| r#"{"loggedIn": true, "authMethod": "claude.ai"}"#.into());
            print!("{status}");
            // Claude Code 2.1.290 exits 1 when not signed in (captured 2026-11-04).
            std::process::exit(i32::from(status.contains("\"loggedIn\": false")));
        }
        let n = bump(&dir.join("calls"));
        let call = |what: &str| dir.join(format!("call-{n}.{what}"));
        std::fs::write(call("args"), args.join("\u{1f}")).unwrap();
        let mut stdin = String::new();
        std::io::stdin().read_to_string(&mut stdin).unwrap();
        std::fs::write(call("stdin"), &stdin).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let entries = std::fs::read_dir(&cwd).unwrap().count();
        std::fs::write(call("cwd"), format!("{}\n{entries}", cwd.display())).unwrap();
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| "<unset>".into());
        std::fs::write(
            call("env"),
            format!(
                "DISABLE_TELEMETRY={}\nCLAUDECODE={}\nCLAUDE_CODE_SIMPLE={}",
                env("DISABLE_TELEMETRY"),
                env("CLAUDECODE"),
                env("CLAUDE_CODE_SIMPLE")
            ),
        )
        .unwrap();
        std::fs::write(call("pid"), std::process::id().to_string()).unwrap();
        let script = std::fs::read_to_string(dir.join(format!("script-{n}")))
            .or_else(|_| std::fs::read_to_string(dir.join("script")))
            .unwrap_or_default();
        let mut exit = 0;
        let mut out = std::io::stdout().lock();
        for line in script.lines() {
            let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
            match cmd {
                "reply" => {
                    let envelope = serde_json::json!({
                        "type": "result", "subtype": "success", "is_error": false,
                        "result": rest, "stop_reason": "end_turn", "num_turns": 1,
                        "usage": {"input_tokens": 10, "output_tokens": 20},
                    });
                    write!(out, "{envelope}").unwrap();
                }
                "raw" => write!(out, "{rest}").unwrap(),
                "big" => {
                    write!(out, r#"{{"type":"result","is_error":false,"result":""#).unwrap();
                    let chunk = "x".repeat(64 * 1024);
                    for _ in 0..rest.parse::<usize>().unwrap() / chunk.len() + 1 {
                        out.write_all(chunk.as_bytes()).unwrap();
                    }
                    write!(out, r#""}}"#).unwrap();
                }
                "stderr" => eprintln!("{rest}"),
                "exit" => exit = rest.parse().unwrap(),
                "grandchild" => {
                    let pid_file = call("grandchild-pid");
                    // Never waited for on purpose: the run under test must end it (its job
                    // object), which the case checks by the recorded pid.
                    #[expect(
                        clippy::zombie_processes,
                        reason = "the provider's job object ends this grandchild"
                    )]
                    std::process::Command::new(std::env::current_exe().unwrap())
                        .args(["--fake-sleep", &pid_file.display().to_string()])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .unwrap();
                    while !pid_file.is_file() {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
                "sleep" => {
                    std::thread::sleep(std::time::Duration::from_millis(rest.parse().unwrap()))
                }
                "" => {}
                other => panic!("unknown fake command {other}"),
            }
        }
        out.flush().unwrap();
        std::process::exit(exit)
    }
}

// ---------------------------------------------------------------------------------------
// Helpers.

/// A scenario folder (and the temp root of the runs) removed at the end.
struct Scenario {
    dir: PathBuf,
    temp_root: PathBuf,
}

impl Scenario {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "earshot-fake-claude-{}-{name}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = base.join("scenario");
        let temp_root = base.join("runs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&temp_root).unwrap();
        Self { dir, temp_root }
    }

    fn script(&self, n: Option<u32>, text: &str) {
        let name = n.map_or("script".to_owned(), |n| format!("script-{n}"));
        std::fs::write(self.dir.join(name), text).unwrap();
    }

    fn cli(&self) -> ClaudeCli {
        ClaudeCli {
            program: std::env::current_exe().unwrap(),
            leading_args: vec!["--fake-claude".into(), self.dir.clone().into_os_string()],
        }
    }

    fn client(&self, edit: impl FnOnce(&mut ClaudeCodeConfig)) -> ClaudeCodeClient {
        let mut config = ClaudeCodeConfig::new(self.cli(), Some("opus".into())).unwrap();
        config.temp_root = self.temp_root.clone();
        edit(&mut config);
        ClaudeCodeClient::new(config, Arc::new(ManualClock::new(T0)))
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    fn calls(&self) -> u32 {
        self.read("calls").trim().parse().unwrap_or(0)
    }

    fn args(&self, n: u32) -> Vec<String> {
        self.read(&format!("call-{n}.args"))
            .split('\u{1f}')
            .map(str::to_owned)
            .collect()
    }

    fn pid(&self, name: &str) -> Option<u32> {
        self.read(name).trim().parse().ok()
    }

    /// No run left a working folder behind.
    fn assert_temp_root_empty(&self) {
        let left: Vec<_> = std::fs::read_dir(&self.temp_root).unwrap().collect();
        assert!(left.is_empty(), "working folders left: {left:?}");
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        if let Some(base) = self.dir.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

/// Whether process `pid` still runs (`tasklist`, a query only).
fn running(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

/// Waits up to 10 s for `pid` to be gone.
fn assert_gone(pid: u32) {
    let until = Instant::now() + Duration::from_secs(10);
    while running(pid) {
        assert!(Instant::now() < until, "process {pid} still runs");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn seg_id(i: usize) -> EntityId {
    EntityId::from_bytes(((i as u128) | (0x0189_0a5d_ac96_7000u128 << 64)).to_be_bytes())
}

const TITLE: &str = "Beta planning";

fn meeting(n: usize) -> SummaryInput {
    SummaryInput {
        meeting: MeetingInfo {
            title: Some(TITLE.into()),
            started: Some("2026-10-17 14:00".into()),
            duration_ms: n as u64 * 10_000,
            attendees: vec!["Ana".into(), "Ben".into()],
        },
        segments: (0..n)
            .map(|i| PromptSegment {
                id: seg_id(i),
                start: MeetingTime::from_millis(i as u64 * 10_000),
                speaker: if i % 2 == 0 { "Ana" } else { "Ben" }.into(),
                text: format!("Point {i}: we ship the beta on Friday and Ana writes the notes."),
            })
            .collect(),
        notes: vec![],
        template: Template::General,
        prompt_variant: Default::default(),
        style: Default::default(),
        vocabulary: Default::default(),
        user_name: None,
    }
}

fn summary_json(numbers: &[usize]) -> String {
    json!({
        "title": "Beta planning",
        "tldr": "The beta ships on Friday.",
        "enhanced_notes": [{"heading": "Beta", "bullets": [{"text": "Ship Friday", "source_segment_ids": numbers}]}],
        "decisions": [{"text": "Ship the beta on Friday", "source_segment_ids": numbers}],
        "action_items": [{"owner": "Ana", "task": "Write release notes", "due": "Thursday", "source_segment_ids": numbers}],
        "open_questions": [],
        "topics": ["beta"]
    })
    .to_string()
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

fn summarise(client: &ClaudeCodeClient, input: &SummaryInput, cancel: &CancelToken) -> SummaryRun {
    let summarizer = LlmSummarizer::new(client.clone());
    block_on(summarizer.summarize_with_report(input, cancel))
}

/// No argument of any run holds meeting text (the prompt goes only on standard input).
fn assert_args_hold_no_meeting_text(s: &Scenario, input: &SummaryInput) {
    for n in 1..=s.calls() {
        let joined = s.args(n).join(" ");
        assert!(!joined.contains(TITLE), "call {n}: {joined}");
        for seg in &input.segments {
            assert!(!joined.contains(&seg.text), "call {n}");
        }
        for a in &input.meeting.attendees {
            assert!(!s.args(n).iter().any(|x| x == a), "call {n}");
        }
    }
}

// ---------------------------------------------------------------------------------------
// Cases.

fn claude_code_valid_reply_is_a_summary_after_one_run() {
    let s = Scenario::new("valid");
    s.script(None, &format!("reply {}", summary_json(&[1, 2])));
    let client = s.client(|_| {});
    let input = meeting(3);
    let run = summarise(&client, &input, &CancelToken::new());
    let summary = run.result.unwrap();
    assert!(validate_summary(&summary, &input.segment_ids()).is_empty());
    assert_eq!(run.report.requests, 1);
    assert_eq!(run.report.repairs, 0);
    assert_eq!(run.report.mode, ResponseMode::Text);
    assert_eq!(s.calls(), 1);

    // The restrictions, the model, and nothing of the meeting on the command line.
    let args = s.args(1);
    assert_eq!(&args[..2], ["--print", "--output-format"]);
    let at = |f: &str| {
        args.iter()
            .position(|a| a == f)
            .unwrap_or_else(|| panic!("{f}"))
    };
    assert_eq!(args[at("--tools") + 1], "");
    assert_eq!(args[at("--model") + 1], "opus");
    for f in [
        "--strict-mcp-config",
        "--disable-slash-commands",
        "--no-session-persistence",
        "--safe-mode",
    ] {
        at(f);
    }
    assert_args_hold_no_meeting_text(&s, &input);

    // The prompt went on standard input: the rules and the meeting.
    let stdin = s.read("call-1.stdin");
    assert!(stdin.contains("===== system ====="));
    assert!(stdin.contains("===== user ====="));
    assert!(stdin.contains(&input.segments[2].text));
    assert!(stdin.contains(TITLE));

    // An empty working folder of its own, under the temp root, removed afterwards.
    let cwd = s.read("call-1.cwd");
    let (path, entries) = cwd.split_once('\n').unwrap();
    assert!(Path::new(path).starts_with(&s.temp_root), "{path}");
    assert_eq!(entries, "0");
    s.assert_temp_root_empty();

    // The environment: telemetry off, nested-session and bare-mode markers removed.
    let env = s.read("call-1.env");
    assert!(env.contains("DISABLE_TELEMETRY=1"), "{env}");
    assert!(env.contains("CLAUDECODE=<unset>"), "{env}");
    assert!(env.contains("CLAUDE_CODE_SIMPLE=<unset>"), "{env}");

    // The handover record: when and how much, never the text.
    let log = client.handovers().entries();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].timestamp_ms, T0);
    assert_eq!(log[0].characters, stdin.chars().count());
    assert!(matches!(log[0].outcome, HandoverOutcome::Answered { characters } if characters > 100));
    let printed = format!("{log:?}");
    assert!(
        !printed.contains("Point") && !printed.contains(TITLE),
        "{printed}"
    );
}

fn claude_code_invalid_json_is_repaired_by_another_restricted_run() {
    let s = Scenario::new("repair");
    s.script(
        Some(1),
        "reply Here are the notes you asked for, sorry, no JSON today.",
    );
    s.script(Some(2), &format!("reply {}", summary_json(&[1])));
    let client = s.client(|_| {});
    let input = meeting(2);
    let run = summarise(&client, &input, &CancelToken::new());
    run.result.unwrap();
    assert_eq!(run.report.requests, 2);
    assert_eq!(run.report.repairs, 1);
    assert_eq!(s.calls(), 2);
    assert_eq!(
        s.args(1),
        s.args(2),
        "every round runs with the same restrictions"
    );
    let second = s.read("call-2.stdin");
    assert!(second.contains("===== your earlier reply ====="));
    assert!(second.contains("no JSON today"));
    assert_args_hold_no_meeting_text(&s, &input);
    assert_eq!(client.handovers().entries().len(), 2);
    s.assert_temp_root_empty();
}

fn claude_code_not_signed_in_is_reported_as_such() {
    // An unexplained failure asks `auth status` once; loggedIn false decides.
    let s = Scenario::new("signed-out");
    s.script(
        None,
        r#"raw {"type":"result","subtype":"success","is_error":true,"result":"Something the provider does not know","terminal_reason":"api_error"}
exit 1"#,
    );
    std::fs::write(s.dir.join("auth.json"), r#"{"loggedIn": false}"#).unwrap();
    let client = s.client(|_| {});
    let run = summarise(&client, &meeting(2), &CancelToken::new());
    let e = run.result.unwrap_err();
    assert!(
        matches!(e, LlmError::ClaudeCode(ClaudeCodeError::NotSignedIn)),
        "{e}"
    );
    assert_eq!(s.read("auth-calls"), "1");
    assert_eq!(s.calls(), 1, "no repair round after a sign-in failure");
    let log = client.handovers().entries();
    assert_eq!(
        log[0].outcome,
        HandoverOutcome::Failed(ClaudeCodeError::NotSignedIn)
    );

    // The captured message of an expired sign-in (2.1.290) needs no status check.
    let s = Scenario::new("expired");
    s.script(
        None,
        r#"raw {"type":"result","subtype":"success","is_error":true,"result":"Failed to authenticate: OAuth session expired and could not be refreshed","terminal_reason":"api_error","stop_reason":"stop_sequence"}
exit 1"#,
    );
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(
        matches!(e, LlmError::ClaudeCode(ClaudeCodeError::SignInExpired)),
        "{e}"
    );
    assert!(e.to_string().contains("sign-in"));
    assert_eq!(s.read("auth-calls"), "");
}

fn claude_code_an_is_error_result_never_shows_its_text() {
    let s = Scenario::new("is-error");
    s.script(
        None,
        r#"raw {"type":"result","subtype":"error_during_execution","is_error":true,"result":"Point 0: we ship the beta","terminal_reason":"api_error"}
exit 1"#,
    );
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "Claude Code reported an error (exit code 1, api_error)"
    );
    assert!(matches!(
        e,
        LlmError::ClaudeCode(ClaudeCodeError::Failed {
            exit_code: Some(1),
            ..
        })
    ));
    assert_eq!(s.read("auth-calls"), "1", "asked whether it is signed in");

    // Exit 0 with is_error true is still a failure; exit 0 with no JSON is a bad output.
    let s = Scenario::new("is-error-0");
    s.script(
        None,
        r#"raw {"type":"result","is_error":true,"result":"x"}"#,
    );
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(
        matches!(e, LlmError::ClaudeCode(ClaudeCodeError::Failed { .. })),
        "{e}"
    );
    let s = Scenario::new("not-json");
    s.script(None, "raw hello");
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(
        matches!(e, LlmError::ClaudeCode(ClaudeCodeError::BadOutput(_))),
        "{e}"
    );
    // Known messages on stderr without a result.
    let s = Scenario::new("too-long");
    s.script(None, "stderr Error: piped stdin input exceeds 10MB. Pass large content as a file path in your prompt instead.\nexit 1");
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(
        matches!(e, LlmError::ClaudeCode(ClaudeCodeError::PromptTooLong)),
        "{e}"
    );
    let s = Scenario::new("unknown-option");
    s.script(None, "stderr error: unknown option '--safe-mode'\nexit 1");
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(
        matches!(e, LlmError::ClaudeCode(ClaudeCodeError::UnsupportedVersion)),
        "{e}"
    );
    let s = Scenario::new("refresh");
    s.script(None, "raw Failed to refresh OAuth token: another Claude Code process is refreshing it or exited mid-refresh. This is usually transient; retry in a minute, or sign in again\nexit 1");
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(
        matches!(e, LlmError::ClaudeCode(ClaudeCodeError::SignInBusy)),
        "{e}"
    );
}

fn claude_code_missing_cli_is_not_installed_and_nothing_is_handed_over() {
    let empty = Scenario::new("missing");
    let search = CliSearch {
        path: Some(empty.dir.clone().into_os_string()),
        home: Some(empty.dir.clone()),
        app_data: Some(empty.dir.clone()),
        local_app_data: None,
    };
    assert_eq!(ClaudeCli::find_with(&search), None);
    // A configured program that does not exist: the CLI never started, no handover.
    let mut config = ClaudeCodeConfig::new(
        ClaudeCli {
            program: empty.dir.join("claude.exe"),
            leading_args: Vec::new(),
        },
        None,
    )
    .unwrap();
    config.temp_root = empty.temp_root.clone();
    let client = ClaudeCodeClient::new(config, Arc::new(ManualClock::new(T0)));
    let e = summarise(&client, &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(
        matches!(e, LlmError::ClaudeCode(ClaudeCodeError::Start(_))),
        "{e}"
    );
    assert!(client.handovers().entries().is_empty());
    empty.assert_temp_root_empty();
    assert_eq!(
        ClaudeCodeError::NotInstalled.to_string(),
        "Claude Code is not installed on this computer"
    );
}

fn claude_code_cli_search_resolves_npm_shims_and_prefers_the_exe() {
    let s = Scenario::new("search");
    let touch = |p: &Path| {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"").unwrap();
    };
    let search = |dirs: &[&Path]| CliSearch {
        path: Some(std::env::join_paths(dirs).unwrap()),
        home: None,
        app_data: None,
        local_app_data: None,
    };
    let package = |npm: &Path| {
        npm.join("node_modules")
            .join("@anthropic-ai")
            .join("claude-code")
    };
    // A shim alone runs nothing.
    let npm = s.dir.join("npm");
    touch(&npm.join("claude.cmd"));
    assert_eq!(ClaudeCli::find_with(&search(&[&npm])), None);
    // An older install: node plus cli.js, node from the searched folders.
    let node_dir = s.dir.join("node");
    touch(&node_dir.join("node.exe"));
    touch(&package(&npm).join("cli.js"));
    assert_eq!(
        ClaudeCli::find_with(&search(&[&npm, &node_dir])),
        Some(ClaudeCli {
            program: node_dir.join("node.exe"),
            leading_args: vec![OsString::from(package(&npm).join("cli.js"))],
        })
    );
    // Claude Code 2: the package's own claude.exe.
    touch(&package(&npm).join("bin").join("claude.exe"));
    assert_eq!(
        ClaudeCli::find_with(&search(&[&npm, &node_dir])),
        Some(ClaudeCli {
            program: package(&npm).join("bin").join("claude.exe"),
            leading_args: Vec::new(),
        })
    );
    // A native claude.exe in an earlier folder wins; the native installer's folder is
    // searched after PATH.
    let native = s.dir.join("native");
    touch(&native.join("claude.exe"));
    assert_eq!(
        ClaudeCli::find_with(&search(&[&native, &npm]))
            .unwrap()
            .program,
        native.join("claude.exe")
    );
    let home = s.dir.join("home");
    touch(&home.join(".local").join("bin").join("claude.exe"));
    let with_home = CliSearch {
        home: Some(home.clone()),
        ..search(&[&s.dir.join("nothing")])
    };
    assert_eq!(
        ClaudeCli::find_with(&with_home).unwrap().program,
        home.join(".local").join("bin").join("claude.exe")
    );
}

fn claude_code_timeout_kills_the_cli_and_its_children() {
    let s = Scenario::new("timeout");
    s.script(None, "grandchild\nsleep 60000");
    let client = s.client(|c| c.request_timeout = Duration::from_secs(2));
    let started = Instant::now();
    let e = summarise(&client, &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(
        matches!(e, LlmError::ClaudeCode(ClaudeCodeError::TimedOut(_))),
        "{e}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(40),
        "{:?}",
        started.elapsed()
    );
    assert_gone(s.pid("call-1.pid").unwrap());
    assert_gone(s.pid("call-1.grandchild-pid").unwrap());
    s.assert_temp_root_empty();
    assert_eq!(s.calls(), 1, "a timeout is not repaired");
    assert!(matches!(
        &client.handovers().entries()[0].outcome,
        HandoverOutcome::Failed(ClaudeCodeError::TimedOut(_))
    ));
}

fn claude_code_cancel_kills_the_cli_and_its_children() {
    let s = Scenario::new("cancel");
    s.script(None, "grandchild\nsleep 60000");
    let client = s.client(|_| {});
    let cancel = CancelToken::new();
    let setter = cancel.clone();
    let pid_file = s.dir.join("call-1.grandchild-pid");
    let t = std::thread::spawn(move || {
        // Cancel once the CLI is running (its grandchild exists).
        let until = Instant::now() + Duration::from_secs(30);
        while !pid_file.is_file() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        setter.cancel();
    });
    let started = Instant::now();
    let e = summarise(&client, &meeting(2), &cancel).result.unwrap_err();
    t.join().unwrap();
    assert!(matches!(e, LlmError::Cancelled), "{e}");
    assert!(started.elapsed() < Duration::from_secs(40));
    assert_gone(s.pid("call-1.pid").unwrap());
    assert_gone(s.pid("call-1.grandchild-pid").unwrap());
    s.assert_temp_root_empty();
    assert_eq!(
        client.handovers().entries()[0].outcome,
        HandoverOutcome::Cancelled
    );
    // Already cancelled: nothing runs.
    let e = summarise(&client, &meeting(2), &cancel).result.unwrap_err();
    assert!(matches!(e, LlmError::Cancelled));
    assert_eq!(s.calls(), 1);
}

fn claude_code_cut_off_output_is_a_failure_never_a_partial_parse() {
    let s = Scenario::new("cut-off");
    s.script(None, &format!("big {}", MAX_CLI_OUTPUT + 1));
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(
        matches!(e, LlmError::ClaudeCode(ClaudeCodeError::OutputIncomplete)),
        "{e}"
    );
    assert_eq!(s.calls(), 1);
    s.assert_temp_root_empty();
}

fn claude_code_effort_is_passed_only_when_the_cli_lists_it() {
    let input = meeting(3);
    let effort_of = |args: &[String]| {
        args.iter()
            .position(|a| a == "--effort")
            .map(|at| args[at + 1].clone())
    };
    // Listed: `--effort high` on every run (here an invalid reply and its repair), the help
    // asked once for both.
    let s = Scenario::new("effort-listed");
    s.script(Some(1), "reply not json");
    s.script(Some(2), &format!("reply {}", summary_json(&[1])));
    let client = s.client(|c| c.effort = Some(Effort::High));
    summarise(&client, &input, &CancelToken::new())
        .result
        .unwrap();
    assert_eq!(s.calls(), 2);
    for n in 1..=2 {
        let args = s.args(n);
        assert_eq!(effort_of(&args).as_deref(), Some("high"), "call {n}");
        assert_eq!(
            args[args.iter().position(|a| a == "--model").unwrap() + 1],
            "opus"
        );
    }
    assert_eq!(s.read("help-calls").trim(), "1", "the help is asked once");
    assert_args_hold_no_meeting_text(&s, &input);

    // An older CLI whose help has no `--effort`: the run goes without it.
    let s = Scenario::new("effort-older");
    std::fs::write(
        s.dir.join("help.txt"),
        "Options:
  --model <model>  Model for the current session
",
    )
    .unwrap();
    s.script(None, &format!("reply {}", summary_json(&[1])));
    let client = s.client(|c| c.effort = Some(Effort::Max));
    summarise(&client, &input, &CancelToken::new())
        .result
        .unwrap();
    assert_eq!(effort_of(&s.args(1)), None);
    assert_eq!(s.read("help-calls").trim(), "1");

    // No effort set: the help is never asked and no `--effort` is passed.
    let s = Scenario::new("effort-none");
    s.script(None, &format!("reply {}", summary_json(&[1])));
    let client = s.client(|_| {});
    summarise(&client, &input, &CancelToken::new())
        .result
        .unwrap();
    assert_eq!(effort_of(&s.args(1)), None);
    assert_eq!(s.read("help-calls"), "", "the help is not asked");
}

fn claude_code_version_and_sign_in_status_come_from_the_cli() {
    let s = Scenario::new("status");
    let cli = s.cli();
    assert_eq!(cli.version(STATUS_TIMEOUT).unwrap(), "9.9.9 (Claude Code)");
    assert_eq!(
        cli.sign_in(STATUS_TIMEOUT),
        SignIn::SignedIn {
            method: Some("claude.ai".into())
        }
    );
    std::fs::write(s.dir.join("auth.json"), r#"{"loggedIn": false}"#).unwrap();
    assert_eq!(cli.sign_in(STATUS_TIMEOUT), SignIn::NotSignedIn);
    std::fs::write(s.dir.join("auth.json"), "garbage").unwrap();
    assert!(matches!(cli.sign_in(STATUS_TIMEOUT), SignIn::Unknown(_)));
    assert_eq!(cli.accepts_effort(STATUS_TIMEOUT), Some(true));
    std::fs::write(
        s.dir.join("help.txt"),
        "Options:
  --model <model>
",
    )
    .unwrap();
    assert_eq!(cli.accepts_effort(STATUS_TIMEOUT), Some(false));
    assert_eq!(s.calls(), 0, "status checks never start a summary run");
    let missing = ClaudeCli {
        program: s.dir.join("nope.exe"),
        leading_args: Vec::new(),
    };
    assert!(matches!(
        missing.sign_in(STATUS_TIMEOUT),
        SignIn::Unknown(_)
    ));
    assert!(missing.version(STATUS_TIMEOUT).is_err());
    assert_eq!(missing.accepts_effort(STATUS_TIMEOUT), None);
}

/// The one real run (T7.4ga accept): the installed Claude Code on this host's sign-in, a
/// short fixed transcript from the committed smoke clips' reference text.
fn claude_code_real_cli_summarises_a_short_fixed_transcript() {
    let cli = ClaudeCli::find().expect("Claude Code is installed on this host");
    eprintln!("cli: {cli:?}");
    eprintln!("version: {:?}", cli.version(STATUS_TIMEOUT));
    eprintln!("sign-in: {:?}", cli.sign_in(STATUS_TIMEOUT));
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/smoke/transcripts.txt");
    let lines: Vec<String> = std::fs::read_to_string(root)
        .unwrap()
        .lines()
        .take(6)
        .map(|l| l.split_once(' ').unwrap().1.to_lowercase())
        .collect();
    let mut input = meeting(0);
    input.meeting.title = None;
    input.meeting.duration_ms = lines.len() as u64 * 5_000;
    input.segments = lines
        .iter()
        .enumerate()
        .map(|(i, text)| PromptSegment {
            id: seg_id(i),
            start: MeetingTime::from_millis(i as u64 * 5_000),
            speaker: if i % 2 == 0 { "Ana" } else { "Ben" }.into(),
            text: text.clone(),
        })
        .collect();
    let config = ClaudeCodeConfig::new(cli, None).unwrap();
    let client = ClaudeCodeClient::new(config, Arc::new(earshot_net::SystemClock));
    let run = summarise(&client, &input, &CancelToken::new());
    eprintln!("report: {:?}", run.report);
    eprintln!("handovers: {:?}", client.handovers().entries());
    match &run.result {
        Ok(s) => eprintln!("summary: {} - {}", s.title, s.tldr),
        Err(e) => eprintln!("error: {e}"),
    }
    let summary = run.result.unwrap();
    assert!(validate_summary(&summary, &input.segment_ids()).is_empty());
}

fn claude_code_sign_in_check_has_a_deadline() {
    // The meeting-start check asks only `auth status`, with a short deadline: a CLI that
    // hangs reads as unknown (the run reports its own error later), never a blocked start.
    let s = Scenario::new("auth-deadline");
    std::fs::write(s.dir.join("auth-sleep"), "60000").unwrap();
    let started = Instant::now();
    let sign_in = s.cli().sign_in(Duration::from_secs(1));
    assert!(matches!(sign_in, SignIn::Unknown(_)), "{sign_in:?}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
}

/// A sink that keeps what it hears and answers `started` with `accept`.
struct Sink {
    accept: bool,
    events: std::sync::Mutex<Vec<String>>,
}

impl HandoverSink for Sink {
    fn started(&self, timestamp_ms: u64, characters: usize) -> Result<(), String> {
        self.events
            .lock()
            .unwrap()
            .push(format!("started {timestamp_ms} {characters}"));
        if self.accept {
            Ok(())
        } else {
            Err("store busy".into())
        }
    }

    fn ended(&self, h: &Handover) {
        self.events.lock().unwrap().push(format!(
            "ended {} {} {}",
            h.timestamp_ms,
            h.characters,
            matches!(h.outcome, HandoverOutcome::Answered { .. })
        ));
    }
}

fn claude_code_sink_hears_each_handover_before_any_text_is_written() {
    let s = Scenario::new("sink");
    s.script(None, &format!("reply {}", summary_json(&[1])));
    let client = s.client(|_| {});
    let sink = Arc::new(Sink {
        accept: true,
        events: std::sync::Mutex::new(Vec::new()),
    });
    client.set_sink(Some(sink.clone()));
    summarise(&client, &meeting(2), &CancelToken::new())
        .result
        .unwrap();
    let chars = s.read("call-1.stdin").chars().count();
    assert_eq!(
        *sink.events.lock().unwrap(),
        [
            format!("started {T0} {chars}"),
            format!("ended {T0} {chars} true")
        ]
    );

    // A handover that cannot be recorded is not made: the CLI is stopped unread.
    let s = Scenario::new("sink-refused");
    s.script(None, &format!("reply {}", summary_json(&[1])));
    let client = s.client(|_| {});
    let refusing = Arc::new(Sink {
        accept: false,
        events: std::sync::Mutex::new(Vec::new()),
    });
    client.set_sink(Some(refusing.clone()));
    let e = summarise(&client, &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(
        matches!(e, LlmError::ClaudeCode(ClaudeCodeError::NotRecorded)),
        "{e}"
    );
    assert!(
        !s.dir.join("call-1.stdin").exists(),
        "no text reached the CLI"
    );
    assert_eq!(
        refusing.events.lock().unwrap().len(),
        1,
        "no end for a refused run"
    );
    assert!(client.handovers().entries().is_empty());
    if let Some(pid) = s.pid("call-1.pid") {
        assert_gone(pid);
    }
    s.assert_temp_root_empty();
}

// ---------------------------------------------------------------------------------------
// The harness.

type Case = (&'static str, fn(), bool);

const CASES: &[Case] = &[
    (
        "claude_code_valid_reply_is_a_summary_after_one_run",
        claude_code_valid_reply_is_a_summary_after_one_run,
        false,
    ),
    (
        "claude_code_invalid_json_is_repaired_by_another_restricted_run",
        claude_code_invalid_json_is_repaired_by_another_restricted_run,
        false,
    ),
    (
        "claude_code_not_signed_in_is_reported_as_such",
        claude_code_not_signed_in_is_reported_as_such,
        false,
    ),
    (
        "claude_code_an_is_error_result_never_shows_its_text",
        claude_code_an_is_error_result_never_shows_its_text,
        false,
    ),
    (
        "claude_code_missing_cli_is_not_installed_and_nothing_is_handed_over",
        claude_code_missing_cli_is_not_installed_and_nothing_is_handed_over,
        false,
    ),
    (
        "claude_code_cli_search_resolves_npm_shims_and_prefers_the_exe",
        claude_code_cli_search_resolves_npm_shims_and_prefers_the_exe,
        false,
    ),
    (
        "claude_code_timeout_kills_the_cli_and_its_children",
        claude_code_timeout_kills_the_cli_and_its_children,
        false,
    ),
    (
        "claude_code_cancel_kills_the_cli_and_its_children",
        claude_code_cancel_kills_the_cli_and_its_children,
        false,
    ),
    (
        "claude_code_cut_off_output_is_a_failure_never_a_partial_parse",
        claude_code_cut_off_output_is_a_failure_never_a_partial_parse,
        false,
    ),
    (
        "claude_code_effort_is_passed_only_when_the_cli_lists_it",
        claude_code_effort_is_passed_only_when_the_cli_lists_it,
        false,
    ),
    (
        "claude_code_version_and_sign_in_status_come_from_the_cli",
        claude_code_version_and_sign_in_status_come_from_the_cli,
        false,
    ),
    (
        "claude_code_sign_in_check_has_a_deadline",
        claude_code_sign_in_check_has_a_deadline,
        false,
    ),
    (
        "claude_code_sink_hears_each_handover_before_any_text_is_written",
        claude_code_sink_hears_each_handover_before_any_text_is_written,
        false,
    ),
    // Ignored: needs the installed, signed-in Claude Code (sends six smoke-clip lines to
    // Anthropic). Run: cargo test -p earshot-llm --test claude_code -- --ignored
    (
        "claude_code_real_cli_summarises_a_short_fixed_transcript",
        claude_code_real_cli_summarises_a_short_fixed_transcript,
        true,
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--fake-claude") {
        fake::main(Path::new(&args[2]), &args[3..]);
    }
    if args.get(1).map(String::as_str) == Some("--fake-sleep") {
        fake::sleeper(&args[2]);
    }
    // The markers the provider must remove from the CLI's environment. Set before any
    // thread starts.
    // SAFETY: single-threaded here (no case has started); nothing reads the environment
    // concurrently.
    unsafe {
        std::env::set_var("CLAUDECODE", "1");
        std::env::set_var("CLAUDE_CODE_SIMPLE", "1");
    }
    let mut filters = Vec::new();
    let (mut ignored, mut include_ignored, mut list) = (false, false, false);
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--ignored" => ignored = true,
            "--include-ignored" => include_ignored = true,
            "--list" => list = true,
            "--test-threads" | "--skip" | "--format" | "--color" | "-Z" | "--logfile" => {
                it.next();
            }
            f if f.starts_with('-') => {}
            f => filters.push(f.to_owned()),
        }
    }
    let selected: Vec<&Case> = CASES
        .iter()
        .filter(|(name, _, _)| {
            filters.is_empty() || filters.iter().any(|f| name.contains(f.as_str()))
        })
        .collect();
    if list {
        for (name, _, _) in &selected {
            println!("{name}: test");
        }
        return;
    }
    let (mut passed, mut failed, mut skipped) = (0, 0, 0);
    let mut failures = Vec::new();
    println!("\nrunning {} tests", selected.len());
    for (name, case, is_ignored) in selected {
        let run = if *is_ignored {
            ignored || include_ignored
        } else {
            !ignored
        };
        if !run {
            println!("test {name} ... ignored");
            skipped += 1;
            continue;
        }
        if !cfg!(windows) {
            // The fake's process checks use tasklist; the provider runs on Windows.
            println!("test {name} ... ignored (Windows only)");
            skipped += 1;
            continue;
        }
        match std::panic::catch_unwind(case) {
            Ok(()) => {
                println!("test {name} ... ok");
                passed += 1;
            }
            Err(_) => {
                println!("test {name} ... FAILED");
                failures.push(*name);
                failed += 1;
            }
        }
    }
    let verdict = if failed == 0 { "ok" } else { "FAILED" };
    println!("\ntest result: {verdict}. {passed} passed; {failed} failed; {skipped} ignored\n");
    if failed > 0 {
        for f in failures {
            println!("    {f}");
        }
        std::process::exit(101);
    }
}
