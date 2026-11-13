//! `CodexClient` and `LlmSummarizer` against a fake `codex` CLI (T7.4gc).
//!
//! This test binary is its own fake: started as `<exe> --fake-codex <scenario dir> <args>`
//! it behaves like the CLI (module `fake`), so no script, interpreter or real CLI is needed
//! (std only). That needs `harness = false` (Cargo.toml): `main` runs the cases itself,
//! takes a name filter like libtest, and runs `#[ignore]`-like cases only with `--ignored`
//! or `--include-ignored`.
//!
//! The one case over a real Codex (`codex_real_cli_summarises_a_short_fixed_transcript`) is
//! ignored: it runs the CLI named by `EARSHOT_TEST_CODEX_CLI` (read only by that case), or
//! the installed one: `cargo test -p earshot-llm --test codex -- --ignored`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use earshot_core::asr::{CancelToken, MeetingTime};
use earshot_core::identity::{EntityId, ManualClock};
use earshot_core::prompt::{MeetingInfo, PromptSegment, SummaryInput, Template};
use earshot_core::summary::validate_summary;
use earshot_llm::claude_code::CliSearch;
use earshot_llm::codex::{CLEARED_ENV, MAX_CLI_OUTPUT, PREAMBLE, STATUS_TIMEOUT};
use earshot_llm::handover::{Handover, HandoverOutcome, HandoverSink};
use earshot_llm::{
    CodexCli, CodexClient, CodexConfig, CodexError, CodexSignIn, LlmError, LlmSummarizer,
    ResponseMode, SummaryRun,
};
use serde_json::json;

const T0: u64 = 1_790_000_000_000;

/// The environment variable naming a Codex CLI for the real run only.
const TEST_CLI_ENV: &str = "EARSHOT_TEST_CODEX_CLI";

/// Variables `main` sets in this process and the CLI must not see (T7.4gc, and T9.25: every
/// address, credential and proxy variable). Written out here, not read from the crate, so a
/// name dropped from the crate's list fails the test.
const MUST_CLEAR: [&str; 17] = [
    "OPENAI_BASE_URL",
    "OPENAI_API_BASE",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "OPENAI_FEDERATION_RULE_ID",
    "OPENAI_IDENTITY_TOKEN_FILE",
    "OPENAI_WORKLOAD_IDENTITY_CONTEXT",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "all_proxy",
    "no_proxy",
    "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
];

/// A variable `main` sets that the CLI must still see (the rest of the environment stays).
const KEPT: &str = "EARSHOT_FAKE_KEPT";

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

    fn event(out: &mut impl Write, v: &serde_json::Value) {
        writeln!(out, "{v}").unwrap();
    }

    /// Writes `NAME=value` (or `NAME=<unset>`) for each variable the tests check.
    fn record_env(path: &Path) {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| "<unset>".into());
        std::fs::write(
            path,
            std::iter::once(&super::KEPT)
                .chain(super::MUST_CLEAR.iter())
                .map(|k| format!("{k}={}", env(k)))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
    }

    /// `<exe> --fake-codex <dir> <args...>` (module docs of the test).
    pub fn main(dir: &Path, args: &[String]) -> ! {
        if args == ["--version"] {
            println!("codex-cli 9.9.9");
            std::process::exit(0);
        }
        if args == ["login", "status"] {
            bump(&dir.join("status-calls"));
            record_env(&dir.join("status.env"));
            if let Ok(ms) = std::fs::read_to_string(dir.join("status-sleep")) {
                std::thread::sleep(std::time::Duration::from_millis(ms.trim().parse().unwrap()));
            }
            // codex-cli 0.155.1 (this host, 2026-11-04): exit 0 signed in, 1 when not.
            if dir.join("signed-out").is_file() {
                eprintln!("Not logged in");
                std::process::exit(1);
            }
            if dir.join("status-garbage").is_file() {
                println!("hmm");
                std::process::exit(0);
            }
            println!("Logged in using ChatGPT");
            std::process::exit(0);
        }
        let n = bump(&dir.join("calls"));
        let call = |what: &str| dir.join(format!("call-{n}.{what}"));
        std::fs::write(call("args"), args.join("\u{1f}")).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let entries = std::fs::read_dir(&cwd).unwrap().count();
        std::fs::write(call("cwd"), format!("{}\n{entries}", cwd.display())).unwrap();
        std::fs::write(call("pid"), std::process::id().to_string()).unwrap();
        record_env(&call("env"));
        let mut stdin = String::new();
        std::io::stdin().read_to_string(&mut stdin).unwrap();
        std::fs::write(call("stdin"), &stdin).unwrap();
        let script = std::fs::read_to_string(dir.join(format!("script-{n}")))
            .or_else(|_| std::fs::read_to_string(dir.join("script")))
            .unwrap_or_default();
        let mut exit = 0;
        let mut out = std::io::stdout().lock();
        for line in script.lines() {
            let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
            match cmd {
                // A whole completed turn as codex-cli 0.155.1 prints it (an earlier app's
                // oneshot_pong.jsonl shape).
                "reply" => {
                    event(
                        &mut out,
                        &serde_json::json!({"type": "thread.started", "thread_id": "01a0bc77-f529-7f73-b8ed-de6c887297bf"}),
                    );
                    event(&mut out, &serde_json::json!({"type": "turn.started"}));
                    event(
                        &mut out,
                        &serde_json::json!({"type": "item.completed", "item": {"id": "item_0", "type": "error", "message": "Exceeded skills context budget."}}),
                    );
                    event(
                        &mut out,
                        &serde_json::json!({"type": "item.completed", "item": {"id": "item_1", "type": "agent_message", "text": rest}}),
                    );
                    event(
                        &mut out,
                        &serde_json::json!({"type": "turn.completed", "usage": {"input_tokens": 10, "output_tokens": 20}}),
                    );
                }
                "fail" => {
                    event(&mut out, &serde_json::json!({"type": "turn.started"}));
                    event(
                        &mut out,
                        &serde_json::json!({"type": "error", "message": rest}),
                    );
                    event(
                        &mut out,
                        &serde_json::json!({"type": "turn.failed", "error": {"message": rest}}),
                    );
                }
                "raw" => writeln!(out, "{rest}").unwrap(),
                // An event type this provider does not know, inside a turn that otherwise
                // completes with an answer.
                "unknown" => {
                    event(&mut out, &serde_json::json!({"type": "turn.started"}));
                    event(&mut out, &serde_json::json!({"type": rest}));
                    event(
                        &mut out,
                        &serde_json::json!({"type": "item.completed", "item": {"id": "item_2", "type": "agent_message", "text": "{}"}}),
                    );
                    event(&mut out, &serde_json::json!({"type": "turn.completed"}));
                }
                // A tool item of type `rest` (started and completed) inside a turn that
                // otherwise completes with an answer.
                "tool" => {
                    event(&mut out, &serde_json::json!({"type": "turn.started"}));
                    for kind in ["item.started", "item.completed"] {
                        event(
                            &mut out,
                            &serde_json::json!({"type": kind, "item": {"id": "item_1", "type": rest}}),
                        );
                    }
                    event(
                        &mut out,
                        &serde_json::json!({"type": "item.completed", "item": {"id": "item_2", "type": "agent_message", "text": "{}"}}),
                    );
                    event(&mut out, &serde_json::json!({"type": "turn.completed"}));
                }
                "big" => {
                    let chunk = "x".repeat(64 * 1024);
                    for _ in 0..rest.parse::<usize>().unwrap() / chunk.len() + 1 {
                        out.write_all(chunk.as_bytes()).unwrap();
                    }
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
        let left = std::fs::read_dir(&cwd).unwrap().count();
        std::fs::write(call("cwd-end"), left.to_string()).unwrap();
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
            "earshot-fake-codex-{}-{name}-{}",
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

    fn flag(&self, name: &str) {
        std::fs::write(self.dir.join(name), "").unwrap();
    }

    fn cli(&self) -> CodexCli {
        CodexCli {
            program: std::env::current_exe().unwrap(),
            leading_args: vec!["--fake-codex".into(), self.dir.clone().into_os_string()],
        }
    }

    fn client(&self, edit: impl FnOnce(&mut CodexConfig)) -> CodexClient {
        let mut config = CodexConfig::new(self.cli(), Some("gpt-5.6-luna".into())).unwrap();
        config.temp_root = self.temp_root.clone();
        edit(&mut config);
        CodexClient::new(config, Arc::new(ManualClock::new(T0)))
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
        style_preset: Default::default(),
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

fn summarise(client: &CodexClient, input: &SummaryInput, cancel: &CancelToken) -> SummaryRun {
    let summarizer = LlmSummarizer::new(client.clone());
    block_on(summarizer.summarize_with_report(input, cancel))
}

fn codex_error(e: &LlmError) -> &CodexError {
    match e {
        LlmError::Codex(c) => c,
        other => panic!("not a Codex error: {other}"),
    }
}

/// No argument of any run holds meeting text (the prompt goes only on standard input).
fn assert_args_hold_no_meeting_text(s: &Scenario, input: &SummaryInput) {
    for n in 1..=s.calls() {
        let joined = s.args(n).join(" ");
        assert!(!joined.contains(TITLE), "call {n}: {joined}");
        assert!(!joined.contains(PREAMBLE), "call {n}");
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

fn codex_valid_reply_is_a_summary_after_one_run() {
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
    assert_eq!(args[0], "exec");
    assert_eq!(args.last().unwrap(), "-");
    let at = |f: &str| {
        args.iter()
            .position(|a| a == f)
            .unwrap_or_else(|| panic!("{f}"))
    };
    assert_eq!(args[at("-s") + 1], "read-only");
    assert_eq!(args[at("-m") + 1], "gpt-5.6-luna");
    for f in [
        "--json",
        "--ephemeral",
        "--skip-git-repo-check",
        "--ignore-user-config",
        "--ignore-rules",
    ] {
        at(f);
    }
    // No reply file: nothing is written to disk, the reply comes from the stream; no
    // history either.
    assert!(!args.iter().any(|a| a == "-o"));
    assert_eq!(args[at("history.persistence=\"none\"") - 1], "-c");
    let cwd = s.read("call-1.cwd");
    let (path, entries) = cwd.split_once('\n').unwrap();
    assert_eq!(
        s.read("call-1.cwd-end"),
        "0",
        "the working folder stayed empty"
    );
    // The environment: no variable that could send the prompt elsewhere or bill another
    // account; the rest is kept (the sign-in is found through it).
    let env = s.read("call-1.env");
    let lines: Vec<&str> = env.lines().collect();
    for k in MUST_CLEAR {
        assert!(lines.contains(&format!("{k}=<unset>").as_str()), "{env}");
    }
    assert!(lines.contains(&"EARSHOT_FAKE_KEPT=1"), "{env}");
    // The crate's list holds every name this test requires (and may hold more).
    for k in MUST_CLEAR {
        assert!(CLEARED_ENV.contains(&k), "{k}");
    }
    assert_args_hold_no_meeting_text(&s, &input);

    // The prompt went on standard input: the preamble, the rules and the meeting.
    let stdin = s.read("call-1.stdin");
    assert!(stdin.starts_with(PREAMBLE));
    assert!(stdin.contains("===== system ====="));
    assert!(stdin.contains("===== user ====="));
    assert!(stdin.contains(&input.segments[2].text));
    assert!(stdin.contains(TITLE));

    // An empty working folder of its own, under the temp root, removed afterwards.
    assert!(Path::new(path).starts_with(&s.temp_root), "{path}");
    assert_eq!(entries, "0");
    s.assert_temp_root_empty();

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
    assert_eq!(s.read("status-calls"), "", "a good run asks no status");
}

fn codex_reply_is_the_last_agent_message_of_the_stream() {
    // Two agent messages: the last is the reply (no file is read).
    let s = Scenario::new("stream");
    s.script(
        None,
        &format!(
            "raw {}\nraw {}\nraw {}",
            json!({"type": "item.completed", "item": {"id": "item_1", "type": "agent_message", "text": "Let me think."}}),
            json!({"type": "item.completed", "item": {"id": "item_2", "type": "agent_message", "text": summary_json(&[1])}}),
            json!({"type": "turn.completed"})
        ),
    );
    let run = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new());
    run.result.unwrap();
    assert_eq!(run.report.requests, 1);
    s.assert_temp_root_empty();
}

fn codex_a_tool_item_fails_the_run_and_its_reply_is_never_used() {
    for kind in [
        "command_execution",
        "file_change",
        "mcp_tool_call",
        "web_search",
        "todo_list",
    ] {
        let s = Scenario::new("tool");
        s.script(None, &format!("tool {kind}"));
        let client = s.client(|_| {});
        let e = summarise(&client, &meeting(2), &CancelToken::new())
            .result
            .unwrap_err();
        assert_eq!(codex_error(&e), &CodexError::ToolRan, "{kind}: {e}");
        assert_eq!(s.calls(), 1, "{kind}: no repair round");
        assert_eq!(
            client.handovers().entries()[0].outcome,
            HandoverOutcome::Failed(CodexError::ToolRan),
            "{kind}: never answered"
        );
        assert_eq!(s.read("status-calls"), "", "{kind}: no sign-in question");
        s.assert_temp_root_empty();
    }
}

fn codex_an_unknown_event_fails_the_run_closed() {
    let s = Scenario::new("unknown-event");
    s.script(None, "unknown exec_command.begin");
    let client = s.client(|_| {});
    let e = summarise(&client, &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert_eq!(codex_error(&e), &CodexError::UnknownEvent, "{e}");
    assert_eq!(s.calls(), 1, "no repair round");
    assert_eq!(
        client.handovers().entries()[0].outcome,
        HandoverOutcome::Failed(CodexError::UnknownEvent),
        "never answered"
    );
    s.assert_temp_root_empty();
}

fn codex_invalid_json_is_repaired_by_another_restricted_run() {
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
    // Every round runs with the same restrictions.
    assert_eq!(s.args(1), s.args(2));
    let second = s.read("call-2.stdin");
    assert!(second.contains("===== your earlier reply ====="));
    assert!(second.contains("no JSON today"));
    assert_args_hold_no_meeting_text(&s, &input);
    assert_eq!(client.handovers().entries().len(), 2);
    s.assert_temp_root_empty();
}

fn codex_not_signed_in_is_reported_as_such() {
    // A 401 (captured with an empty CODEX_HOME) asks `login status` once: "Not logged in".
    let s = Scenario::new("signed-out");
    s.script(
        None,
        "fail unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: https://api.openai.com/v1/responses\nexit 1",
    );
    s.flag("signed-out");
    let client = s.client(|_| {});
    let e = summarise(&client, &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert_eq!(codex_error(&e), &CodexError::NotSignedIn, "{e}");
    assert!(codex_error(&e).needs_sign_in());
    assert_eq!(s.read("status-calls"), "1");
    assert_eq!(s.calls(), 1, "no repair round after a sign-in failure");
    assert_eq!(
        client.handovers().entries()[0].outcome,
        HandoverOutcome::Failed(CodexError::NotSignedIn)
    );

    // Signed in but refused: the sign-in did not work.
    let s = Scenario::new("expired");
    s.script(
        None,
        "fail unexpected status 401 Unauthorized: token expired\nexit 1",
    );
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert_eq!(codex_error(&e), &CodexError::SignInExpired, "{e}");
    assert_eq!(s.read("status-calls"), "1");

    // An unexplained failure asks too; a known one never does.
    let s = Scenario::new("unexplained");
    s.script(None, "fail something new\nexit 1");
    s.flag("signed-out");
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert_eq!(codex_error(&e), &CodexError::NotSignedIn, "{e}");
    let s = Scenario::new("denied");
    s.script(
        None,
        r#"fail unexpected status 403 Forbidden: {"detail":{"message":"Unauthorized: Contact your ChatGPT workspace administrator for access.","code":"codex_workspace_access_denied"}}, url: https://chatgpt.com/backend-api/codex/responses
exit 1"#,
    );
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert_eq!(codex_error(&e), &CodexError::AccessDenied, "{e}");
    assert_eq!(s.read("status-calls"), "");
}

fn codex_failures_never_show_cli_text() {
    let s = Scenario::new("turn-failed");
    s.script(
        None,
        "fail Point 0: we ship the beta https://x.example\nexit 1",
    );
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "Codex reported an error (exit code 1, turn_failed)"
    );
    assert_eq!(s.read("status-calls"), "1", "asked whether it is signed in");
    // Known messages on stderr without any event.
    for (stderr, want) in [
        (
            r#"Error: turn/start: turn/start failed: Input exceeds the maximum length of 1048576 characters. (code -32602), data: {"input_error_code":"input_too_large","max_chars":1048576,"actual_chars":9000000}"#,
            CodexError::PromptTooLong,
        ),
        (
            "error: unexpected argument '--bogus-flag' found",
            CodexError::UnsupportedVersion,
        ),
        (
            "Error: Unknown feature flag: bogus_feature_xyz",
            CodexError::UnsupportedVersion,
        ),
    ] {
        let s = Scenario::new("stderr");
        s.script(None, &format!("stderr {stderr}\nexit 1"));
        let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
            .result
            .unwrap_err();
        assert_eq!(codex_error(&e), &want, "{e}");
        assert!(!e.to_string().contains("1048576"), "{e}");
    }
    // Exit 0 without a completed turn is a bad output.
    let s = Scenario::new("no-turn");
    s.script(None, "raw hello");
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(matches!(codex_error(&e), CodexError::BadOutput(_)), "{e}");
}

fn codex_missing_cli_is_not_installed_and_nothing_is_handed_over() {
    let empty = Scenario::new("missing");
    let search = CliSearch {
        path: Some(empty.dir.clone().into_os_string()),
        home: Some(empty.dir.clone()),
        app_data: Some(empty.dir.clone()),
        local_app_data: None,
    };
    assert_eq!(CodexCli::find_with(&search), None);
    // A configured program that does not exist: the CLI never started, no handover.
    let mut config = CodexConfig::new(CodexCli::at(empty.dir.join("codex.exe")), None).unwrap();
    config.temp_root = empty.temp_root.clone();
    let client = CodexClient::new(config, Arc::new(ManualClock::new(T0)));
    let e = summarise(&client, &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(matches!(codex_error(&e), CodexError::Start(_)), "{e}");
    assert!(client.handovers().entries().is_empty());
    empty.assert_temp_root_empty();
    assert_eq!(
        CodexError::NotInstalled.to_string(),
        "Codex is not installed on this computer"
    );
}

fn codex_cli_search_resolves_npm_shims_to_the_native_exe() {
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
    let (package, triple) = if cfg!(target_arch = "aarch64") {
        ("codex-win32-arm64", "aarch64-pc-windows-msvc")
    } else {
        ("codex-win32-x64", "x86_64-pc-windows-msvc")
    };
    let native = |root: &Path| {
        root.join(package)
            .join("vendor")
            .join(triple)
            .join("bin")
            .join("codex.exe")
    };
    // A shim alone (and its PowerShell and bash siblings) runs nothing.
    let npm = s.dir.join("npm");
    touch(&npm.join("codex.cmd"));
    touch(&npm.join("codex.ps1"));
    touch(&npm.join("codex"));
    assert_eq!(CodexCli::find_with(&search(&[&npm])), None);
    // The hoisted platform package.
    let openai = npm.join("node_modules").join("@openai");
    touch(&native(&openai));
    assert_eq!(
        CodexCli::find_with(&search(&[&npm])),
        Some(CodexCli::at(native(&openai)))
    );
    // A global install's nested package is preferred.
    let nested = openai.join("codex").join("node_modules").join("@openai");
    touch(&native(&nested));
    assert_eq!(
        CodexCli::find_with(&search(&[&npm])),
        Some(CodexCli::at(native(&nested)))
    );
    // Among PATH folders, a native codex.exe in an earlier folder wins.
    let direct = s.dir.join("direct");
    touch(&direct.join("codex.exe"));
    assert_eq!(
        CodexCli::find_with(&search(&[&direct, &npm]))
            .unwrap()
            .program,
        direct.join("codex.exe")
    );
    // The known install folders come before PATH, even when PATH has a codex.exe too
    // (T9.25): npm's global folder before PATH...
    let app_data = s.dir.join("appdata");
    touch(&app_data.join("npm").join("codex.cmd"));
    touch(&native(
        &app_data.join("npm").join("node_modules").join("@openai"),
    ));
    let with_app_data = CliSearch {
        app_data: Some(app_data.clone()),
        ..search(&[&direct])
    };
    assert_eq!(
        CodexCli::find_with(&with_app_data).unwrap().program,
        native(&app_data.join("npm").join("node_modules").join("@openai"))
    );
    // ...and the standalone installer's folder before both.
    let local = s.dir.join("local");
    let standalone = local
        .join("Programs")
        .join("OpenAI")
        .join("Codex")
        .join("bin")
        .join("codex.exe");
    touch(&standalone);
    let with_standalone = CliSearch {
        local_app_data: Some(local.clone()),
        ..with_app_data
    };
    assert_eq!(
        CodexCli::find_with(&with_standalone).unwrap().program,
        standalone
    );
}

fn codex_timeout_kills_the_cli_and_its_children() {
    let s = Scenario::new("timeout");
    s.script(None, "grandchild\nsleep 60000");
    let client = s.client(|c| c.request_timeout = Duration::from_secs(2));
    let started = Instant::now();
    let e = summarise(&client, &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert!(matches!(codex_error(&e), CodexError::TimedOut(_)), "{e}");
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
        HandoverOutcome::Failed(CodexError::TimedOut(_))
    ));
}

fn codex_cancel_kills_the_cli_and_its_children() {
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

fn codex_cut_off_output_is_a_failure_never_a_partial_parse() {
    let s = Scenario::new("cut-off");
    s.script(None, &format!("big {}", MAX_CLI_OUTPUT + 1));
    let e = summarise(&s.client(|_| {}), &meeting(2), &CancelToken::new())
        .result
        .unwrap_err();
    assert_eq!(codex_error(&e), &CodexError::OutputIncomplete, "{e}");
    assert_eq!(s.calls(), 1);
    s.assert_temp_root_empty();
}

fn codex_version_and_sign_in_status_come_from_the_cli() {
    let s = Scenario::new("status");
    let cli = s.cli();
    assert_eq!(cli.version(STATUS_TIMEOUT).unwrap(), "codex-cli 9.9.9");
    assert_eq!(
        cli.sign_in(STATUS_TIMEOUT),
        CodexSignIn::SignedIn {
            method: Some("chatgpt")
        }
    );
    s.flag("signed-out");
    assert_eq!(cli.sign_in(STATUS_TIMEOUT), CodexSignIn::NotSignedIn);
    // The sign-in check sees the summary run's environment (T9.25): a sign-in that exists
    // only in a cleared variable reads as not signed in, never as one the run lacks.
    let env = s.read("status.env");
    let lines: Vec<&str> = env.lines().collect();
    assert!(lines.contains(&"EARSHOT_FAKE_KEPT=1"), "{env}");
    for k in MUST_CLEAR {
        assert!(lines.contains(&format!("{k}=<unset>").as_str()), "{env}");
    }
    std::fs::remove_file(s.dir.join("signed-out")).unwrap();
    s.flag("status-garbage");
    assert!(matches!(
        cli.sign_in(STATUS_TIMEOUT),
        CodexSignIn::Unknown(_)
    ));
    assert_eq!(s.calls(), 0, "status checks never start a summary run");
    let missing = CodexCli::at(s.dir.join("nope.exe"));
    assert!(matches!(
        missing.sign_in(STATUS_TIMEOUT),
        CodexSignIn::Unknown(_)
    ));
    assert!(missing.version(STATUS_TIMEOUT).is_err());
}

fn codex_sign_in_check_has_a_deadline() {
    let s = Scenario::new("status-deadline");
    std::fs::write(s.dir.join("status-sleep"), "60000").unwrap();
    let started = Instant::now();
    let sign_in = s.cli().sign_in(Duration::from_secs(1));
    assert!(matches!(sign_in, CodexSignIn::Unknown(_)), "{sign_in:?}");
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

impl HandoverSink<CodexError> for Sink {
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

    fn ended(&self, h: &Handover<CodexError>) {
        self.events.lock().unwrap().push(format!(
            "ended {} {} {}",
            h.timestamp_ms,
            h.characters,
            matches!(h.outcome, HandoverOutcome::Answered { .. })
        ));
    }
}

fn codex_sink_hears_each_handover_before_any_text_is_written() {
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
    assert_eq!(codex_error(&e), &CodexError::NotRecorded, "{e}");
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

/// The one real run (T7.4gc accept): a real Codex on this host's ChatGPT sign-in, a short
/// fixed transcript from the committed smoke clips' reference text (public LibriSpeech
/// text). `EARSHOT_TEST_CODEX_CLI` may name the CLI (this case only; production never reads
/// it), else the installed one is used.
fn codex_real_cli_summarises_a_short_fixed_transcript() {
    let cli = match std::env::var_os(TEST_CLI_ENV) {
        Some(p) => CodexCli::at(PathBuf::from(p)),
        None => CodexCli::find().expect("Codex is installed on this host"),
    };
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
    let config = CodexConfig::new(cli, None).unwrap();
    let client = CodexClient::new(config, Arc::new(earshot_net::SystemClock));
    let started = Instant::now();
    let run = summarise(&client, &input, &CancelToken::new());
    eprintln!("elapsed: {:?}", started.elapsed());
    eprintln!("report: {:?}", run.report);
    eprintln!("handovers: {:?}", client.handovers().entries());
    match &run.result {
        Ok(s) => eprintln!("summary: {} - {}", s.title, s.tldr),
        Err(e) => eprintln!("error: {e}"),
    }
    let summary = run.result.unwrap();
    assert!(validate_summary(&summary, &input.segment_ids()).is_empty());
}

// ---------------------------------------------------------------------------------------
// The harness.

type Case = (&'static str, fn(), bool);

const CASES: &[Case] = &[
    (
        "codex_valid_reply_is_a_summary_after_one_run",
        codex_valid_reply_is_a_summary_after_one_run,
        false,
    ),
    (
        "codex_reply_is_the_last_agent_message_of_the_stream",
        codex_reply_is_the_last_agent_message_of_the_stream,
        false,
    ),
    (
        "codex_an_unknown_event_fails_the_run_closed",
        codex_an_unknown_event_fails_the_run_closed,
        false,
    ),
    (
        "codex_a_tool_item_fails_the_run_and_its_reply_is_never_used",
        codex_a_tool_item_fails_the_run_and_its_reply_is_never_used,
        false,
    ),
    (
        "codex_invalid_json_is_repaired_by_another_restricted_run",
        codex_invalid_json_is_repaired_by_another_restricted_run,
        false,
    ),
    (
        "codex_not_signed_in_is_reported_as_such",
        codex_not_signed_in_is_reported_as_such,
        false,
    ),
    (
        "codex_failures_never_show_cli_text",
        codex_failures_never_show_cli_text,
        false,
    ),
    (
        "codex_missing_cli_is_not_installed_and_nothing_is_handed_over",
        codex_missing_cli_is_not_installed_and_nothing_is_handed_over,
        false,
    ),
    (
        "codex_cli_search_resolves_npm_shims_to_the_native_exe",
        codex_cli_search_resolves_npm_shims_to_the_native_exe,
        false,
    ),
    (
        "codex_timeout_kills_the_cli_and_its_children",
        codex_timeout_kills_the_cli_and_its_children,
        false,
    ),
    (
        "codex_cancel_kills_the_cli_and_its_children",
        codex_cancel_kills_the_cli_and_its_children,
        false,
    ),
    (
        "codex_cut_off_output_is_a_failure_never_a_partial_parse",
        codex_cut_off_output_is_a_failure_never_a_partial_parse,
        false,
    ),
    (
        "codex_version_and_sign_in_status_come_from_the_cli",
        codex_version_and_sign_in_status_come_from_the_cli,
        false,
    ),
    (
        "codex_sign_in_check_has_a_deadline",
        codex_sign_in_check_has_a_deadline,
        false,
    ),
    (
        "codex_sink_hears_each_handover_before_any_text_is_written",
        codex_sink_hears_each_handover_before_any_text_is_written,
        false,
    ),
    // Ignored: needs a signed-in Codex (sends six smoke-clip lines to OpenAI). Run:
    // cargo test -p earshot-llm --test codex -- --ignored
    (
        "codex_real_cli_summarises_a_short_fixed_transcript",
        codex_real_cli_summarises_a_short_fixed_transcript,
        true,
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--fake-codex") {
        fake::main(Path::new(&args[2]), &args[3..]);
    }
    if args.get(1).map(String::as_str) == Some("--fake-sleep") {
        fake::sleeper(&args[2]);
    }
    // Variables the provider must remove from the CLI's environment, and one it must keep.
    // Set before any thread starts.
    // SAFETY: single-threaded here (no case has started); nothing reads the environment
    // concurrently.
    unsafe {
        for k in MUST_CLEAR {
            std::env::set_var(k, "http://127.0.0.1:9/earshot-test");
        }
        std::env::set_var(KEPT, "1");
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
