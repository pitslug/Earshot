//! Summary tests: a loopback mock endpoint only (earshot-llm-replay) for remote summaries,
//! a fake model behind the local backend, and one ignored test on the small `llm`-set
//! model. Every name contains `summary`.

use earshot_core::asr::CancelToken;
use earshot_core::model::MeetingRepo;
use earshot_core::summary::SummaryV1;
use earshot_llm_replay::mock::{MockResponse, MockServer};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::testutil::{TempDir, meeting, services};

fn reply(title: &str) -> MockResponse {
    let summary = SummaryV1 {
        title: title.into(),
        tldr: "The team agreed to ship the release on Friday.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec!["release".into()],
    };
    MockResponse::completion(&serde_json::to_string(&summary).unwrap(), "stop")
}

#[test]
fn summary_goes_only_to_the_configured_endpoint_and_is_stored() {
    let dir = TempDir::new("summary");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 1);
    let server = MockServer::start(vec![reply("Release planning")]).unwrap();
    let endpoint = server.endpoint();
    let service = SummaryService::new(
        &SummaryConfig::new(endpoint.as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let out = service
        .summarize(
            &svc,
            id,
            &mut ChaCha8Rng::seed_from_u64(2),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(out.summary.title, "Release planning");
    assert!(out.title_set);

    // Privacy: every network attempt went to the configured endpoint and nowhere else.
    let audit = service.audit_log().entries();
    assert!(!audit.is_empty());
    for e in &audit {
        assert_eq!(
            (e.host.as_str(), e.port),
            (endpoint.host_str().unwrap(), endpoint.port().unwrap()),
            "{e:?}"
        );
        assert_eq!(e.path, "/v1/chat/completions");
    }
    assert_eq!(audit.len(), server.requests().len());
    // What was sent: text (transcript with speaker names, the note), never audio.
    for r in server.requests() {
        let body = String::from_utf8(r.body.clone()).unwrap();
        assert!(body.contains("yes friday works for the release"));
        assert!(body.contains("Alice"));
        assert!(body.contains("check the release notes"));
        assert!(
            r.body.len() < 64 * 1024,
            "a transcript request, not a recording"
        );
    }

    let db = svc.store();
    let rows = db.summaries(id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, out.summary_id);
    assert_eq!(rows[0].model.as_deref(), Some("test-model"));
    assert_eq!(rows[0].summary, out.summary);
    assert_eq!(db.meeting(id).unwrap().unwrap().title, "Release planning");
}

/// Keeps `meeting`'s summary on this computer (T12.13), as a meeting started so carries it.
fn keep_local(svc: &Services, meeting: EntityId) {
    crate::local::set(
        &mut svc.store(),
        &crate::keep_local::record_key(meeting),
        &crate::keep_local::encode(),
    )
    .unwrap();
}

#[test]
fn summary_kept_local_meeting_opens_no_connection_with_an_endpoint_set() {
    let dir = TempDir::new("summary");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 1);
    keep_local(&svc, id);
    let server = MockServer::start(vec![reply("Release planning")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let summarize = || {
        service.summarize(
            &svc,
            id,
            &mut ChaCha8Rng::seed_from_u64(2),
            &CancelToken::new(),
        )
    };
    let e = summarize().unwrap_err();
    assert!(
        matches!(e, RuntimeError::SummaryKeptLocal(m) if m == id),
        "{e:?}"
    );
    assert!(!e.is_retryable());
    // Nothing was attempted, nothing arrived, nothing was stored.
    assert!(service.audit_log().is_empty());
    assert!(server.requests().is_empty());
    assert!(svc.store().summaries(id).unwrap().is_empty());
    // A record this version cannot read keeps it here too.
    crate::local::set(
        &mut svc.store(),
        &crate::keep_local::record_key(id),
        b"damaged",
    )
    .unwrap();
    assert!(matches!(
        summarize(),
        Err(RuntimeError::SummaryKeptLocal(_))
    ));
    assert!(server.requests().is_empty());
    // Control: once the user lets the writer have it, the same service sends it when asked.
    crate::keep_local::allow_writer(&svc, id).unwrap();
    assert!(server.requests().is_empty(), "allowing sends nothing");
    summarize().unwrap();
    assert_eq!(server.requests().len(), 1);
}

#[test]
#[cfg_attr(
    not(windows),
    ignore = "the Claude Code and Codex stand-ins are cmd.exe"
)]
fn summary_kept_local_meeting_spawns_no_agent_cli() {
    #[cfg(windows)]
    {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let summary = SummaryV1 {
            title: "Release planning".into(),
            tldr: "The team agreed to ship the release on Friday.".into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            topics: vec!["release".into()],
        };
        let claude = SummaryService::claude_code(
            &crate::claude_code::ClaudeCodeSummaryConfig {
                cli: Some(claude_code_stand_in(&dir, &summary)),
                ..Default::default()
            },
            svc.clock.clone(),
        )
        .unwrap();
        let events = [
            serde_json::json!({"type": "thread.started", "thread_id": "t"}),
            serde_json::json!({"type": "turn.started"}),
            serde_json::json!({"type": "item.completed", "item": {"id": "item_0", "type": "agent_message", "text": serde_json::to_string(&summary).unwrap()}}),
            serde_json::json!({"type": "turn.completed"}),
        ];
        let codex = SummaryService::codex(
            &crate::codex::CodexSummaryConfig {
                cli: Some(codex_printing(&dir, &events)),
                ..Default::default()
            },
            svc.clock.clone(),
        )
        .unwrap();
        for (seed, service) in [(1, &claude), (2, &codex)] {
            let id = meeting(&svc, "", seed);
            keep_local(&svc, id);
            let summarize = || {
                service.summarize(
                    &svc,
                    id,
                    &mut ChaCha8Rng::seed_from_u64(seed),
                    &CancelToken::new(),
                )
            };
            let e = summarize().unwrap_err();
            assert!(
                matches!(e, RuntimeError::SummaryKeptLocal(m) if m == id),
                "{e:?}"
            );
            // No handover was made or recorded: the CLI never ran.
            assert!(service.handovers().is_empty());
            assert!(service.audit_log().is_empty());
            let db = svc.store();
            assert_eq!(
                db.local_value(crate::claude_code::record_key(id).as_str())
                    .unwrap(),
                None,
                "no handover record"
            );
            assert!(db.summaries(id).unwrap().is_empty());
            drop(db);
            // Control: allowed, the same stand-in runs and its handover is recorded.
            crate::keep_local::allow_writer(&svc, id).unwrap();
            summarize().unwrap();
            assert_eq!(service.handovers().len(), 1);
            assert_eq!(
                crate::claude_code::summary_handovers(&svc.store(), id)
                    .unwrap()
                    .entries
                    .len(),
                1
            );
        }
    }
}

/// A stand-in for the Claude Code CLI: `cmd /c type <reply file> & rem <the provider's
/// arguments>` prints a Claude Code JSON result holding `summary` (the provider's own tests
/// in earshot-llm run a full fake CLI).
#[cfg(windows)]
fn claude_code_stand_in(dir: &TempDir, summary: &SummaryV1) -> earshot_llm::ClaudeCli {
    let envelope = serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "result": serde_json::to_string(summary).unwrap(), "stop_reason": "end_turn",
    });
    claude_code_printing(dir, &envelope.to_string())
}

/// The stand-in printing `stdout` as it is.
#[cfg(windows)]
fn claude_code_printing(dir: &TempDir, stdout: &str) -> earshot_llm::ClaudeCli {
    let path = dir.0.join("claude-reply.json");
    std::fs::write(&path, stdout).unwrap();
    earshot_llm::ClaudeCli {
        program: "cmd".into(),
        leading_args: vec![
            "/c".into(),
            "type".into(),
            path.into_os_string(),
            "&".into(),
            "rem".into(),
        ],
    }
}

/// The stored handover record of `meeting` holds none of the meeting's words (nor the CLI's
/// error text).
#[cfg(windows)]
fn assert_record_holds_no_text(db: &earshot_store::Store, meeting: EntityId) {
    let bytes = db
        .local_value(crate::claude_code::record_key(meeting).as_str())
        .unwrap()
        .expect("a handover record");
    let text = String::from_utf8_lossy(&bytes).to_lowercase();
    for word in [
        "friday",
        "works",
        "release",
        "alice",
        "check",
        "notes",
        "authenticate",
    ] {
        assert!(!text.contains(word), "{word} in {text}");
    }
}

#[test]
#[cfg_attr(not(windows), ignore = "the Claude Code stand-in is cmd.exe")]
fn summary_claude_code_is_stored_with_its_handovers_and_no_network() {
    #[cfg(windows)]
    {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 1);
        let summary = SummaryV1 {
            title: "Release planning".into(),
            tldr: "The team agreed to ship the release on Friday.".into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            topics: vec!["release".into()],
        };
        let config = crate::claude_code::ClaudeCodeSummaryConfig {
            model: Some("opus".into()),
            effort: None,
            cli: Some(claude_code_stand_in(&dir, &summary)),
            request_timeout: None,
        };
        let service = SummaryService::claude_code(&config, svc.clock.clone()).unwrap();
        assert!(service.is_claude_code());
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(3),
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(out.summary.title, "Release planning");
        assert!(matches!(&out.report, SummaryRunReport::ClaudeCode(r) if r.requests == 1));
        // No connection of Earshot's own; one handover with its size and no text.
        assert!(service.audit_log().is_empty());
        let handovers = service.handovers();
        assert_eq!(handovers.len(), 1);
        assert!(handovers[0].characters > 100);
        assert!(!format!("{handovers:?}").contains("friday"));
        let db = svc.store();
        let rows = db.summaries(id).unwrap();
        assert_eq!(rows[0].model.as_deref(), Some("claude-code/opus"));
        // Recorded locally with the summary: when and how much, never text.
        let record = crate::claude_code::summary_handovers(&db, id).unwrap();
        assert_eq!(
            record.entries,
            [crate::claude_code::HandoverEntry {
                at_unix_ms: handovers[0].at_unix_ms,
                characters: handovers[0].characters,
                cli: crate::claude_code::HandoverCli::ClaudeCode,
                outcome: crate::claude_code::HandoverKind::Answered,
            }]
        );
        assert_record_holds_no_text(&db, id);
    }
}

#[test]
#[cfg_attr(not(windows), ignore = "the Claude Code stand-in is cmd.exe")]
fn summary_claude_code_says_when_an_unreadable_help_left_the_default_effort() {
    #[cfg(windows)]
    {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 1);
        let summary = SummaryV1 {
            title: "Invented planning".into(),
            tldr: "An invented summary.".into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            topics: vec![],
        };
        let reply = claude_code_stand_in(&dir, &summary);
        // A batch file that fails on `--help` and prints the reply for a run.
        let bat = dir.0.join("claude-stand-in.bat");
        let printed = reply.leading_args[2].clone();
        std::fs::write(
            &bat,
            format!(
                "@if \"%1\"==\"--help\" exit /b 1\r\n@type \"{}\"\r\n",
                std::path::Path::new(&printed).display()
            ),
        )
        .unwrap();
        let cli = earshot_llm::ClaudeCli {
            program: "cmd".into(),
            leading_args: vec!["/c".into(), "call".into(), bat.into_os_string()],
        };
        let run = |effort| {
            let config = crate::claude_code::ClaudeCodeSummaryConfig {
                model: None,
                effort,
                cli: Some(cli.clone()),
                request_timeout: None,
            };
            let service = SummaryService::claude_code(&config, svc.clock.clone()).unwrap();
            assert!(
                !service.claude_effort_unread(),
                "nothing asked before a run"
            );
            service
                .summarize(
                    &svc,
                    id,
                    &mut ChaCha8Rng::seed_from_u64(4),
                    &CancelToken::new(),
                )
                .unwrap();
            service.claude_effort_unread()
        };
        assert!(run(Some(earshot_llm::claude_code::Effort::High)));
        // No effort chosen: nothing to say.
        assert!(!run(None));
    }
}

#[test]
#[cfg_attr(not(windows), ignore = "the Claude Code stand-in is cmd.exe")]
fn summary_claude_code_failure_is_recorded_too() {
    #[cfg(windows)]
    {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 1);
        // An old meeting (or one never summarised through Claude Code) has no record.
        let none = crate::claude_code::summary_handovers(&svc.store(), id).unwrap();
        assert_eq!(none, crate::claude_code::HandoverRecord::default());
        let config = crate::claude_code::ClaudeCodeSummaryConfig {
            cli: Some(claude_code_printing(
                &dir,
                r#"{"type":"result","subtype":"success","is_error":true,"result":"Failed to authenticate: OAuth session expired and could not be refreshed","terminal_reason":"api_error"}"#,
            )),
            ..Default::default()
        };
        let service = SummaryService::claude_code(&config, svc.clock.clone()).unwrap();
        let e = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(9),
                &CancelToken::new(),
            )
            .unwrap_err();
        assert!(e.to_string().contains("sign-in"), "{e}");
        let db = svc.store();
        assert!(db.summaries(id).unwrap().is_empty());
        let record = crate::claude_code::summary_handovers(&db, id).unwrap();
        assert_eq!(record.entries.len(), 1);
        assert_eq!(
            record.entries[0].outcome,
            crate::claude_code::HandoverKind::SignInExpired
        );
        assert!(record.entries[0].characters > 100);
        assert_record_holds_no_text(&db, id);
        drop(db);
        // A second run appends.
        let _ = service.summarize(
            &svc,
            id,
            &mut ChaCha8Rng::seed_from_u64(10),
            &CancelToken::new(),
        );
        let record = crate::claude_code::summary_handovers(&svc.store(), id).unwrap();
        assert_eq!(record.entries.len(), 2);
    }
}

#[test]
#[cfg_attr(not(windows), ignore = "the Claude Code stand-in is cmd.exe")]
fn summary_claude_code_run_cut_short_leaves_its_started_entry_and_keeps_its_own_error() {
    #[cfg(windows)]
    {
        use crate::claude_code::{HandoverKind, summary_handovers};
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 1);
        // A CLI that runs for half a minute and never reads its input. `ping` by its full path:
        // under load cmd.exe's own search sometimes reports "'ping' is not recognized" (measured
        // 4 in 48 runs, 2026-11-06), goes on to `& rem` and exits 0 with no output.
        let ping = std::path::Path::new(&std::env::var_os("SystemRoot").unwrap())
            .join(r"System32\PING.EXE");
        let config = crate::claude_code::ClaudeCodeSummaryConfig {
            cli: Some(earshot_llm::ClaudeCli {
                program: "cmd".into(),
                leading_args: vec![
                    "/c".into(),
                    ping.into_os_string(),
                    "-n".into(),
                    "30".into(),
                    "127.0.0.1".into(),
                    ">nul".into(),
                    "&".into(),
                    "rem".into(),
                ],
            }),
            ..Default::default()
        };
        let service = SummaryService::claude_code(&config, svc.clock.clone()).unwrap();
        let cancel = CancelToken::new();
        // While the CLI runs, the record already holds `started` (what a crash would leave).
        let watcher = {
            let svc = svc.clone();
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
                loop {
                    let seen = summary_handovers(&svc.store(), id).unwrap().entries;
                    if !seen.is_empty() {
                        cancel.cancel();
                        return seen;
                    }
                    assert!(std::time::Instant::now() < until, "no started entry");
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })
        };
        let e = service
            .summarize(&svc, id, &mut ChaCha8Rng::seed_from_u64(11), &cancel)
            .unwrap_err();
        let mid_run = watcher.join().unwrap();
        assert_eq!(mid_run.len(), 1);
        assert_eq!(mid_run[0].outcome, HandoverKind::Started);
        assert!(mid_run[0].characters > 100);
        // The run's own error stays itself; the entry then takes the outcome.
        assert!(
            matches!(e, RuntimeError::Llm(earshot_llm::LlmError::Cancelled)),
            "{e}"
        );
        let after = summary_handovers(&svc.store(), id).unwrap().entries;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].outcome, HandoverKind::Cancelled);
        assert_eq!(after[0].at_unix_ms, mid_run[0].at_unix_ms);
        assert!(service.handover_record_failures().is_empty());
        assert_record_holds_no_text(&svc.store(), id);
    }
}

/// A stand-in for the Codex CLI: `cmd /c type <events file> & rem <the provider's
/// arguments>` prints `codex exec --json` events (the provider's own tests in earshot-llm run
/// a full fake CLI).
#[cfg(windows)]
fn codex_printing(dir: &TempDir, events: &[serde_json::Value]) -> earshot_llm::CodexCli {
    let path = dir.0.join("codex-events.jsonl");
    let text: Vec<String> = events.iter().map(ToString::to_string).collect();
    std::fs::write(&path, text.join("\n")).unwrap();
    earshot_llm::CodexCli {
        program: "cmd".into(),
        leading_args: vec![
            "/c".into(),
            "type".into(),
            path.into_os_string(),
            "&".into(),
            "rem".into(),
        ],
    }
}

#[test]
#[cfg_attr(not(windows), ignore = "the Codex stand-in is cmd.exe")]
fn summary_codex_is_stored_with_its_handovers_and_no_network() {
    #[cfg(windows)]
    {
        use crate::claude_code::{HandoverCli, HandoverKind, summary_handovers};
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 1);
        let summary = SummaryV1 {
            title: "Release planning".into(),
            tldr: "The team agreed to ship the release on Friday.".into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            topics: vec!["release".into()],
        };
        let events = [
            serde_json::json!({"type": "thread.started", "thread_id": "t"}),
            serde_json::json!({"type": "turn.started"}),
            serde_json::json!({"type": "item.completed", "item": {"id": "item_0", "type": "agent_message", "text": serde_json::to_string(&summary).unwrap()}}),
            serde_json::json!({"type": "turn.completed"}),
        ];
        let config = crate::codex::CodexSummaryConfig {
            model: Some("gpt-5.6-luna".into()),
            cli: Some(codex_printing(&dir, &events)),
            request_timeout: None,
        };
        let service = SummaryService::codex(&config, svc.clock.clone()).unwrap();
        assert!(service.is_codex() && !service.is_claude_code());
        assert_eq!(service.agent_cli(), Some(HandoverCli::Codex));
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(3),
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(out.summary.title, "Release planning");
        assert!(matches!(&out.report, SummaryRunReport::Codex(r) if r.requests == 1));
        assert!(service.audit_log().is_empty());
        let handovers = service.handovers();
        assert_eq!(handovers.len(), 1);
        assert_eq!(handovers[0].cli, HandoverCli::Codex);
        assert!(handovers[0].characters > 100);
        let db = svc.store();
        assert_eq!(
            db.summaries(id).unwrap()[0].model.as_deref(),
            Some("codex/gpt-5.6-luna")
        );
        let record = summary_handovers(&db, id).unwrap();
        assert_eq!(record.entries, handovers);
        assert_eq!(record.entries[0].outcome, HandoverKind::Answered);
        assert_record_holds_no_text(&db, id);
    }
}

#[test]
#[cfg_attr(not(windows), ignore = "the Codex stand-in is cmd.exe")]
fn summary_codex_failure_is_recorded_with_its_cli_and_no_text() {
    #[cfg(windows)]
    {
        use crate::claude_code::{HandoverCli, HandoverKind, summary_handovers};
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 1);
        let denied = r#"unexpected status 403 Forbidden: {"detail":{"message":"Unauthorized: Contact your ChatGPT workspace administrator for access.","code":"codex_workspace_access_denied"}}"#;
        let events = [
            serde_json::json!({"type": "turn.started"}),
            serde_json::json!({"type": "turn.failed", "error": {"message": denied}}),
        ];
        let config = crate::codex::CodexSummaryConfig {
            cli: Some(codex_printing(&dir, &events)),
            ..Default::default()
        };
        let service = SummaryService::codex(&config, svc.clock.clone()).unwrap();
        let e = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(9),
                &CancelToken::new(),
            )
            .unwrap_err();
        assert!(
            matches!(
                e,
                RuntimeError::Llm(earshot_llm::LlmError::Codex(
                    earshot_llm::CodexError::AccessDenied
                ))
            ),
            "{e}"
        );
        assert!(!e.to_string().contains("403"), "{e}");
        let db = svc.store();
        assert!(db.summaries(id).unwrap().is_empty());
        let record = summary_handovers(&db, id).unwrap();
        assert_eq!(record.entries.len(), 1);
        assert_eq!(record.entries[0].cli, HandoverCli::Codex);
        assert_eq!(record.entries[0].outcome, HandoverKind::AccessDenied);
        let bytes = db
            .local_value(crate::claude_code::record_key(id).as_str())
            .unwrap()
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).to_lowercase();
        assert!(!text.contains("workspace administrator"), "{text}");
        assert_record_holds_no_text(&db, id);
    }
}

#[test]
fn summary_codex_and_claude_code_handovers_share_the_record_without_mixing() {
    use earshot_llm::{ClaudeCodeError, CodexError, HandoverOutcome, HandoverSink};
    let dir = TempDir::new("summary");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 1);
    let sink = crate::claude_code::MeetingHandovers {
        services: svc.clone(),
        meeting: id,
        failures: Arc::default(),
    };
    // Both CLIs start at the same moment with the same size: each end finds its own entry.
    HandoverSink::<ClaudeCodeError>::started(&sink, 5, 20).unwrap();
    HandoverSink::<CodexError>::started(&sink, 5, 20).unwrap();
    sink.ended(&earshot_llm::Handover::<CodexError> {
        timestamp_ms: 5,
        characters: 20,
        outcome: HandoverOutcome::Failed(CodexError::NotSignedIn),
    });
    let entries = crate::claude_code::summary_handovers(&svc.store(), id)
        .unwrap()
        .entries;
    use crate::claude_code::{HandoverCli, HandoverKind};
    assert_eq!(
        entries
            .iter()
            .map(|e| (e.cli, e.outcome))
            .collect::<Vec<_>>(),
        [
            (HandoverCli::ClaudeCode, HandoverKind::Started),
            (HandoverCli::Codex, HandoverKind::NotSignedIn),
        ]
    );
}

#[test]
fn summary_handover_that_cannot_be_stored_is_reported_and_not_made() {
    use earshot_llm::{ClaudeCodeError, HandoverOutcome, HandoverSink};
    let dir = TempDir::new("summary");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 1);
    let failures = Arc::default();
    let sink = crate::claude_code::MeetingHandovers {
        services: svc.clone(),
        meeting: EntityId::from_bytes([7; 16]),
        failures: Arc::clone(&failures),
    };
    // No such meeting: `started` refuses (the CLI is then stopped unread) and says why.
    assert!(HandoverSink::<ClaudeCodeError>::started(&sink, 1, 10).is_err());
    let why = failures.lock().unwrap().clone();
    assert_eq!(why.len(), 1);
    assert!(why[0].contains("deleted"), "{why:?}");
    // An end for a meeting that is gone writes nothing and is no failure.
    sink.ended(&earshot_llm::Handover::<ClaudeCodeError> {
        timestamp_ms: 1,
        characters: 10,
        outcome: HandoverOutcome::Cancelled,
    });
    assert_eq!(failures.lock().unwrap().len(), 1);
    // A live meeting: started, then the outcome replaces it.
    let sink = crate::claude_code::MeetingHandovers {
        services: svc.clone(),
        meeting: id,
        failures: Arc::default(),
    };
    HandoverSink::<ClaudeCodeError>::started(&sink, 5, 20).unwrap();
    sink.ended(&earshot_llm::Handover::<ClaudeCodeError> {
        timestamp_ms: 5,
        characters: 20,
        outcome: HandoverOutcome::Answered { characters: 3 },
    });
    let entries = crate::claude_code::summary_handovers(&svc.store(), id)
        .unwrap()
        .entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].outcome,
        crate::claude_code::HandoverKind::Answered
    );
}

#[test]
fn summary_handover_records_of_another_version_or_damaged_read_as_unreadable() {
    use crate::claude_code::{
        HANDOVER_RECORD_VERSION, HandoverCli, HandoverEntry, HandoverKind, decode, encode,
    };
    let entries = [HandoverEntry {
        at_unix_ms: 1_793_000_000_000,
        characters: 5324,
        cli: HandoverCli::ClaudeCode,
        outcome: HandoverKind::Answered,
    }];
    let bytes = encode(&entries);
    assert_eq!(HANDOVER_RECORD_VERSION, 2);
    assert_eq!(&bytes[..4], &HANDOVER_RECORD_VERSION.to_le_bytes());
    assert_eq!(
        String::from_utf8_lossy(&bytes[4..]),
        r#"{"entries":[{"at_unix_ms":1793000000000,"characters":5324,"cli":"claude_code","outcome":"answered"}]}"#
    );
    let read = decode(&bytes);
    assert_eq!(read.entries, entries);
    assert_eq!(read.unreadable, None);
    // Version 1 (T7.4ga) records had no `cli`: every entry was Claude Code's.
    let mut v1 = 1u32.to_le_bytes().to_vec();
    v1.extend_from_slice(
        br#"{"entries":[{"at_unix_ms":1793000000000,"characters":5324,"outcome":"answered"}]}"#,
    );
    assert_eq!(decode(&v1), read);
    // A version 1 record naming Codex or its outcome word is not one any build wrote.
    let mut v1_codex = 1u32.to_le_bytes().to_vec();
    v1_codex.extend_from_slice(
        br#"{"entries":[{"at_unix_ms":1,"characters":2,"cli":"codex","outcome":"answered"}]}"#,
    );
    assert!(decode(&v1_codex).unreadable.is_some());
    let mut v1_denied = 1u32.to_le_bytes().to_vec();
    v1_denied.extend_from_slice(
        br#"{"entries":[{"at_unix_ms":1,"characters":2,"outcome":"access_denied"}]}"#,
    );
    assert!(decode(&v1_denied).unreadable.is_some());
    // Codex entries round-trip, a tool that ran included.
    let codex = [
        HandoverEntry {
            at_unix_ms: 7,
            characters: 8,
            cli: HandoverCli::Codex,
            outcome: HandoverKind::AccessDenied,
        },
        HandoverEntry {
            at_unix_ms: 9,
            characters: 10,
            cli: HandoverCli::Codex,
            outcome: HandoverKind::ToolRan,
        },
        HandoverEntry {
            at_unix_ms: 11,
            characters: 12,
            cli: HandoverCli::Codex,
            outcome: HandoverKind::UnknownEvent,
        },
    ];
    assert_eq!(decode(&encode(&codex)).entries, codex);
    assert!(String::from_utf8_lossy(&encode(&codex)).contains(r#""outcome":"tool_ran""#));
    assert!(String::from_utf8_lossy(&encode(&codex)).contains(r#""outcome":"unknown_event""#));
    let mut v1_tool = 1u32.to_le_bytes().to_vec();
    v1_tool.extend_from_slice(
        br#"{"entries":[{"at_unix_ms":1,"characters":2,"outcome":"tool_ran"}]}"#,
    );
    assert!(decode(&v1_tool).unreadable.is_some());
    let mut other = bytes.clone();
    other[0] = 9;
    assert!(decode(&other).unreadable.is_some() && decode(&other).entries.is_empty());
    assert!(decode(b"x").unreadable.is_some());
    let mut damaged = bytes;
    damaged.truncate(10);
    assert!(decode(&damaged).unreadable.is_some());
}

#[test]
fn summary_rerun_replaces_the_previous_one_and_keeps_the_users_title() {
    let dir = TempDir::new("summary");
    let (svc, clock) = services(&dir);
    let id = meeting(&svc, "Friday release", 3);
    let server = MockServer::start(vec![reply("First"), reply("Second")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let first = service
        .summarize(
            &svc,
            id,
            &mut ChaCha8Rng::seed_from_u64(4),
            &CancelToken::new(),
        )
        .unwrap();
    clock.advance_ms(1000);
    let second = service
        .summarize(
            &svc,
            id,
            &mut ChaCha8Rng::seed_from_u64(5),
            &CancelToken::new(),
        )
        .unwrap();
    assert!(!first.title_set && !second.title_set);
    let db = svc.store();
    let rows = db.summaries(id).unwrap();
    let live: Vec<_> = rows.iter().filter(|s| !s.meta.deleted).collect();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, second.summary_id);
    assert!(
        rows.iter()
            .any(|s| s.id == first.summary_id && s.meta.deleted)
    );
    assert_eq!(db.meeting(id).unwrap().unwrap().title, "Friday release");
}

#[test]
fn summary_failure_stores_nothing() {
    let dir = TempDir::new("summary");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 6);
    let server = MockServer::start(vec![MockResponse::status(500, "upstream down")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let err = service
        .summarize(
            &svc,
            id,
            &mut ChaCha8Rng::seed_from_u64(7),
            &CancelToken::new(),
        )
        .unwrap_err();
    assert!(matches!(err, RuntimeError::Llm(_)), "{err:?}");
    let db = svc.store();
    assert!(db.summaries(id).unwrap().is_empty());
    assert_eq!(db.meeting(id).unwrap().unwrap().title, "");
    // Cancelled before any request: nothing is sent.
    drop(db);
    let cancel = CancelToken::new();
    cancel.cancel();
    let before = service.audit_log().len();
    assert!(
        service
            .summarize(&svc, id, &mut ChaCha8Rng::seed_from_u64(8), &cancel)
            .is_err()
    );
    assert_eq!(service.audit_log().len(), before);
}

#[test]
fn summary_through_anthropic_goes_only_to_the_configured_host_and_is_stored() {
    const KEY: &str = "sk-ant-test-SECRET55";
    let dir = TempDir::new("summary");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 1);
    let summary = SummaryV1 {
        title: "Release planning".into(),
        tldr: "The team agreed to ship the release on Friday.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec!["release".into()],
    };
    let server = MockServer::start(vec![MockResponse::anthropic_text(
        &serde_json::to_string(&summary).unwrap(),
        "end_turn",
    )])
    .unwrap();
    let base = server.base_url();
    let config = SummaryConfig::anthropic(base.as_str(), "claude-test", ApiKey::new(KEY));
    assert!(!format!("{config:?}").contains("SECRET55"));
    let service = SummaryService::new(&config, svc.clock.clone()).unwrap();
    let out = service
        .summarize(
            &svc,
            id,
            &mut ChaCha8Rng::seed_from_u64(3),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(out.summary.title, "Release planning");

    // Privacy: every attempt went to the configured host, the key only in its header.
    let audit = service.audit_log().entries();
    assert_eq!(audit.len(), 1);
    for e in &audit {
        assert_eq!(
            (e.host.as_str(), e.port, e.path.as_str()),
            (
                base.host_str().unwrap(),
                base.port().unwrap(),
                "/v1/messages"
            ),
            "{e:?}"
        );
    }
    assert!(!format!("{audit:?} {service:?}").contains("SECRET55"));
    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].header("x-api-key"), Some(KEY));
    let body = String::from_utf8(reqs[0].body.clone()).unwrap();
    assert!(body.contains("yes friday works for the release"));
    assert!(
        body.len() < 64 * 1024,
        "a transcript request, not a recording"
    );

    let db = svc.store();
    let rows = db.summaries(id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model.as_deref(), Some("claude-test"));
}

#[test]
fn summary_anthropic_endpoint_must_be_allowlistable() {
    let clock: SharedClock = std::sync::Arc::new(earshot_core::identity::ManualClock::new(0));
    let key = ApiKey::new("k");
    assert!(
        SummaryService::new(
            &SummaryConfig::anthropic("http://api.anthropic.com", "m", key.clone()),
            clock.clone()
        )
        .is_err(),
        "plain http to a public host"
    );
    assert!(
        SummaryService::new(
            &SummaryConfig::anthropic(earshot_llm::ANTHROPIC_DEFAULT_URL, "m", key),
            clock
        )
        .is_ok()
    );
}

#[test]
fn summary_endpoint_must_be_allowlistable() {
    let clock: SharedClock = std::sync::Arc::new(earshot_core::identity::ManualClock::new(0));
    // Plain http to a public host is refused (earshot-net URL policy).
    assert!(
        SummaryService::new(
            &SummaryConfig::new("http://example.com/v1", "m"),
            clock.clone()
        )
        .is_err()
    );
    assert!(SummaryService::new(&SummaryConfig::new("not a url", "m"), clock.clone()).is_err());
    // https anywhere, http on loopback/LAN.
    assert!(
        SummaryService::new(
            &SummaryConfig::new("https://llm.example.com/v1", "m"),
            clock.clone()
        )
        .is_ok()
    );
    assert!(
        SummaryService::new(
            &SummaryConfig::new("http://192.168.1.20:8080/v1", "m"),
            clock
        )
        .is_ok()
    );
}

#[test]
fn summary_input_is_text_only_with_speaker_names_and_local_start() {
    let dir = TempDir::new("summary");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "Friday release", 9);
    let input = SummaryService::input(&svc, id).unwrap();
    assert_eq!(input.meeting.title.as_deref(), Some("Friday release"));
    assert_eq!(input.meeting.started.as_deref(), Some("2026-10-18 13:40"));
    assert_eq!(input.meeting.duration_ms, 60_000);
    assert_eq!(input.meeting.attendees, ["Me", "Alice"]);
    let speakers: Vec<&str> = input.segments.iter().map(|s| s.speaker.as_str()).collect();
    assert_eq!(speakers, ["Me", "Alice"]);
    assert_eq!(input.notes.len(), 1);
}

#[test]
fn summary_input_keeps_the_standard_rules_whatever_the_settings() {
    use earshot_core::prompt::{PromptVariant, SummaryRules};
    let dir = TempDir::new("summary");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "Friday release", 12);
    // The writer alone chooses brief rules (T12.17c); no production input carries them, so
    // the local model never gets them from a stored setting or a later experiment.
    let input = SummaryService::input(&svc, id).unwrap();
    assert_eq!(input.rules, SummaryRules::Standard);
    assert_eq!(input.prompt_variant, PromptVariant::Current);
    crate::summary_style::set_summary_style(&mut svc.store(), "Short bullets.").unwrap();
    crate::vocabulary::set_vocabulary(&mut svc.store(), &["Quillmarsh"]).unwrap();
    let input = SummaryService::input(&svc, id).unwrap();
    assert_eq!(input.rules, SummaryRules::Standard);
    assert_eq!(input.prompt_variant, PromptVariant::Current);
    let view = MeetingView::load(&**svc.store(), id).unwrap().unwrap();
    let eval = crate::summary::evaluation_input(&view);
    assert_eq!(eval.rules, SummaryRules::Standard);
    assert_eq!(eval.prompt_variant, PromptVariant::Current);
}

/// Starts a summary and runs `change` while the call is in flight, and returns the summary
/// result. The mock endpoint runs `change` when the request arrives, before it answers:
/// the summary input has been read and its result not yet written, however slow the host.
fn summarize_while(
    svc: &Services,
    meeting: earshot_core::identity::EntityId,
    change: impl FnOnce(&Services) + Send + 'static,
) -> Result<SummaryOutcome, RuntimeError> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let pending = std::sync::Mutex::new(Some((change, svc.clone())));
    let changed = std::sync::Arc::new(AtomicBool::new(false));
    let done = std::sync::Arc::clone(&changed);
    let server = MockServer::start_with(move |_| {
        let next = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some((change, services)) = next {
            change(&services);
            done.store(true, Ordering::SeqCst);
        }
        reply("Changed meanwhile")
    })
    .unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let result = service.summarize(
        svc,
        meeting,
        &mut ChaCha8Rng::seed_from_u64(40),
        &CancelToken::new(),
    );
    assert!(
        changed.load(Ordering::SeqCst),
        "the change ran while the call was in flight ({result:?})"
    );
    result
}

#[test]
fn summary_of_a_meeting_deleted_during_the_call_is_not_stored() {
    let dir = TempDir::new("summary");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 41);
    let result = summarize_while(&svc, id, move |s| delete_meeting_now(s, id));
    assert!(
        matches!(result, Err(RuntimeError::Conflict(_))),
        "{result:?}"
    );
    assert!(svc.store().summaries(id).unwrap().is_empty());
}

#[test]
fn summary_of_a_transcript_replaced_during_the_call_is_not_stored() {
    let dir = TempDir::new("summary");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "Weekly", 42);
    let result = summarize_while(&svc, id, move |s| replace_transcript_now(s, id, 43));
    assert!(
        matches!(result, Err(RuntimeError::Conflict(_))),
        "{result:?}"
    );
    assert!(svc.store().summaries(id).unwrap().is_empty());
    assert_eq!(svc.store().meeting(id).unwrap().unwrap().title, "Weekly");
}

/// Marks the meeting deleted, as a deletion on another thread would.
fn delete_meeting_now(s: &Services, id: EntityId) {
    let clock = std::sync::Arc::clone(&s.clock);
    let mut db = s.store();
    let mut m = db.meeting(id).unwrap().unwrap();
    let (store, hlc) = db.parts();
    m.meta.mark_deleted(hlc, &*clock);
    store.put_meeting(m).unwrap();
}

/// A re-transcription: replaces the final segments with copies under new ids.
fn replace_transcript_now(s: &Services, id: EntityId, seed: u64) {
    let clock = std::sync::Arc::clone(&s.clock);
    let mut db = s.store();
    let rows: Vec<_> = db.segments(id).unwrap();
    let (store, hlc) = db.parts();
    // One generator for every new id (ids from the same seed and clock collide).
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    for mut row in rows {
        row.meta.mark_deleted(hlc, &*clock);
        store.put_segment(row.clone()).unwrap();
        let mut seg = row.segment.clone();
        seg.id = EntityId::generate(&*clock, &mut rng);
        let fresh = earshot_core::model::SegmentRow::new(
            id,
            earshot_core::model::TranscriptPass::Final,
            seg,
            hlc,
            &*clock,
        );
        store.put_segment(fresh).unwrap();
    }
}

mod local {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use earshot_core::prompt::{ChatMessage, Role, estimate_messages};
    use earshot_llm_local::{Generation, LlmError, Stop};

    use super::*;

    /// Runs once, on the summary thread, while the first request generates.
    type OnCall = Box<dyn FnOnce(&CancelToken) + Send>;

    /// What a fake model saw and did, shared with the test.
    #[derive(Default)]
    struct Shared {
        loads: AtomicUsize,
        live: AtomicUsize,
        /// The `gpu` setting of every load attempt.
        load_gpu: Mutex<Vec<bool>>,
        requests: Mutex<Vec<Vec<ChatMessage>>>,
        on_call: Mutex<Option<OnCall>>,
    }

    /// The first segment number a request shows: a transcript line `[n] ...` or, in a
    /// merge request, the first citation of a partial summary.
    fn first_number(messages: &[ChatMessage]) -> u64 {
        let user = &messages
            .iter()
            .find(|m| m.role == Role::User)
            .unwrap()
            .content;
        for line in user.lines() {
            if let Some(n) = line
                .strip_prefix('[')
                .and_then(|r| r.split(']').next())
                .and_then(|n| n.parse().ok())
            {
                return n;
            }
        }
        let key = "\"source_segment_ids\":[";
        let at = user.find(key).expect("a number") + key.len();
        user[at..]
            .split([',', ']'])
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Answers every request with a valid summary citing the first number it shows.
    struct FakeModel(Arc<Shared>);

    impl Drop for FakeModel {
        fn drop(&mut self) {
            self.0.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Generate for FakeModel {
        fn context_tokens(&self) -> usize {
            16_384
        }

        fn prompt_tokens(&self, m: &[ChatMessage], prefix: &str) -> Result<usize, LlmError> {
            Ok(estimate_messages(m) + prefix.len())
        }

        fn generate(
            &mut self,
            m: &[ChatMessage],
            _prefix: &str,
            _grammar: &str,
            _max_tokens: usize,
            cancel: &CancelToken,
        ) -> Result<Generation, LlmError> {
            self.0.requests.lock().unwrap().push(m.to_vec());
            let hook = self.0.on_call.lock().unwrap().take();
            if let Some(hook) = hook {
                hook(cancel);
            }
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }
            let n = first_number(m);
            Ok(Generation {
                text: format!(
                    r#"{{"title":"Release planning","tldr":"Ship on Friday.","enhanced_notes":[],"decisions":[{{"text":"Ship on Friday","source_segment_ids":[{n}]}}],"action_items":[],"open_questions":[],"topics":["release"]}}"#
                ),
                stop: Stop::EndOfGeneration,
                prompt_tokens: 100,
                generated_tokens: 50,
                prompt_time: Duration::from_millis(1),
                generate_time: Duration::from_millis(2),
            })
        }
    }

    /// Loads a [`FakeModel`] and reports `backend`, whatever the config asked for; with
    /// `refuse_gpu` a GPU load fails as llama.cpp's backend check does; with `missing`
    /// every load fails as a missing model file does.
    struct FakeLoader {
        backend: LlamaBackend,
        refuse_gpu: bool,
        missing: bool,
        /// Every load fails as llama.cpp's load of a corrupt file does.
        unloadable: bool,
        shared: Arc<Shared>,
    }

    impl LoadLocal for FakeLoader {
        fn load(&self, config: &LocalSummaryConfig) -> Result<LoadedModel, RuntimeError> {
            self.shared.load_gpu.lock().unwrap().push(config.gpu);
            if self.missing {
                return Err(RuntimeError::ModelMissing {
                    path: config.model_path.clone(),
                    fetch_set: None,
                });
            }
            if self.unloadable {
                let on = if config.gpu { "gpu" } else { "cpu" };
                return Err(RuntimeError::LocalSummary(LocalSummaryError::Load(
                    LlmError::Load(format!("corrupt file ({on} load)")),
                )));
            }
            if self.refuse_gpu && config.gpu {
                return Err(RuntimeError::LocalSummary(LocalSummaryError::Load(
                    LlmError::Backend("asked for the GPU, the load log shows the CPU".into()),
                )));
            }
            self.shared.loads.fetch_add(1, Ordering::SeqCst);
            self.shared.live.fetch_add(1, Ordering::SeqCst);
            Ok(LoadedModel {
                heartbeat: Heartbeat::default(),
                model: Box::new(FakeModel(Arc::clone(&self.shared))),
                backend: self.backend,
                load_time: Duration::from_millis(5),
            })
        }
    }

    fn fake_service(config: &LocalSummaryConfig) -> (SummaryService, Arc<Shared>) {
        // Not what the config asks for (CPU): the outcome must show the loader's.
        fake_service_with(config, LlamaBackend::Vulkan, false)
    }

    fn fake_service_with(
        config: &LocalSummaryConfig,
        backend: LlamaBackend,
        refuse_gpu: bool,
    ) -> (SummaryService, Arc<Shared>) {
        let shared = Arc::new(Shared::default());
        let loader = FakeLoader {
            backend,
            refuse_gpu,
            missing: false,
            unloadable: false,
            shared: Arc::clone(&shared),
        };
        let service = SummaryService::local_with(config, Box::new(loader)).unwrap();
        (service, shared)
    }

    fn config() -> LocalSummaryConfig {
        LocalSummaryConfig::new("unused.gguf", "fake-local-model")
    }

    /// Lets a model stuck in [`StuckModel`] go when the test ends, so its thread ends too.
    #[derive(Clone, Default)]
    struct Stuck(Arc<(Mutex<bool>, std::sync::Condvar)>, Arc<AtomicBool>);

    impl Stuck {
        fn hold(&self) {
            self.1.store(true, Ordering::SeqCst);
            let (lock, wake) = &*self.0;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
        }

        fn release(&self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }

    /// A model on the GPU whose first request never returns and never reports progress: what a
    /// call stuck inside llama.cpp looks like from outside. Cancelling does not reach it.
    struct StuckModel(Stuck, FakeModel);

    impl Generate for StuckModel {
        fn context_tokens(&self) -> usize {
            self.1.context_tokens()
        }

        fn prompt_tokens(&self, m: &[ChatMessage], prefix: &str) -> Result<usize, LlmError> {
            self.1.prompt_tokens(m, prefix)
        }

        fn generate(
            &mut self,
            _m: &[ChatMessage],
            _prefix: &str,
            _grammar: &str,
            _max_tokens: usize,
            _cancel: &CancelToken,
        ) -> Result<Generation, LlmError> {
            self.0.hold();
            Err(LlmError::Cancelled)
        }
    }

    /// A slow model on the GPU that keeps reporting progress for longer than the limit.
    struct SlowModel(Heartbeat, Duration, FakeModel);

    impl Generate for SlowModel {
        fn context_tokens(&self) -> usize {
            self.2.context_tokens()
        }

        fn prompt_tokens(&self, m: &[ChatMessage], prefix: &str) -> Result<usize, LlmError> {
            self.2.prompt_tokens(m, prefix)
        }

        fn generate(
            &mut self,
            m: &[ChatMessage],
            prefix: &str,
            grammar: &str,
            max_tokens: usize,
            cancel: &CancelToken,
        ) -> Result<Generation, LlmError> {
            let started = std::time::Instant::now();
            while started.elapsed() < self.1 {
                self.0.beat();
                std::thread::sleep(Duration::from_millis(5));
            }
            self.2.generate(m, prefix, grammar, max_tokens, cancel)
        }
    }

    /// What a GPU load gives in these tests; a CPU load is always the ordinary fake.
    enum OnGpu {
        Stuck(Stuck),
        StuckLoad(Stuck),
        Slow(Duration),
    }

    struct StallLoader(OnGpu, Arc<Shared>);

    impl LoadLocal for StallLoader {
        fn load(&self, config: &LocalSummaryConfig) -> Result<LoadedModel, RuntimeError> {
            self.1.load_gpu.lock().unwrap().push(config.gpu);
            let fake = FakeModel(Arc::clone(&self.1));
            let heartbeat = Heartbeat::default();
            let (model, backend): (Box<dyn Generate>, _) = match &self.0 {
                _ if !config.gpu => (Box::new(fake), LlamaBackend::Cpu),
                OnGpu::Stuck(s) => (Box::new(StuckModel(s.clone(), fake)), LlamaBackend::Vulkan),
                OnGpu::StuckLoad(s) => {
                    s.hold();
                    (Box::new(fake), LlamaBackend::Vulkan)
                }
                OnGpu::Slow(d) => (
                    Box::new(SlowModel(heartbeat.clone(), *d, fake)),
                    LlamaBackend::Vulkan,
                ),
            };
            Ok(LoadedModel {
                heartbeat,
                model,
                backend,
                load_time: Duration::from_millis(5),
            })
        }
    }

    /// A GPU run with short limits, its own session flag and `on_gpu`.
    fn stall_service(on_gpu: OnGpu) -> (SummaryService, Arc<Shared>, &'static AtomicBool) {
        stall_service_within(on_gpu, Duration::from_millis(300))
    }

    /// [`stall_service`] with its limits at `limit`.
    fn stall_service_within(
        on_gpu: OnGpu,
        limit: Duration,
    ) -> (SummaryService, Arc<Shared>, &'static AtomicBool) {
        let mut cfg = config();
        cfg.gpu = true;
        cfg.cpu_fallback = true;
        cfg.stall_limit = limit;
        cfg.load_stall_limit = limit;
        let shared = Arc::new(Shared::default());
        let flag: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let service = SummaryService::local_flagged(
            &cfg,
            Box::new(StallLoader(on_gpu, Arc::clone(&shared))),
            flag,
        )
        .unwrap();
        (service, shared, flag)
    }

    /// Gives the speaker rows of `id` these names, Me first.
    fn name_rows(svc: &Services, id: EntityId, me: &str, other: &str) {
        crate::edit::write(svc, |tx, stamp| {
            for mut s in tx.speakers(id)? {
                s.display_name = if s.role == earshot_core::diar::SpeakerRole::Me {
                    me
                } else {
                    other
                }
                .to_owned();
                s.meta.touch(stamp.hlc, stamp.clock);
                tx.put_speaker(s)?;
            }
            Ok(())
        })
        .unwrap();
    }

    fn sent_to_the_model(svc: &Services, id: EntityId, seed: u64) -> String {
        let (service, shared) = fake_service(&config());
        service
            .summarize(
                svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(seed),
                &CancelToken::new(),
            )
            .unwrap();
        let requests = shared.requests.lock().unwrap();
        requests[0]
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn summary_names_the_user_once_they_have_a_name() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 190);
        // The default: the prompt says nothing about who the user is.
        let plain = sent_to_the_model(&svc, id, 191);
        assert!(!plain.contains("appears in the transcript as"), "{plain}");

        name_rows(&svc, id, "Casey Parker", "Alice");
        let named = sent_to_the_model(&svc, id, 192);
        assert!(
            named.contains("appears in the transcript as Casey Parker."),
            "{named}"
        );
        assert!(named.contains("Casey Parker:"), "{named}");
        assert!(named.contains("Attendees: Casey Parker, Alice"), "{named}");
    }

    #[test]
    fn summary_tells_two_people_with_one_first_name_apart() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 193);
        name_rows(&svc, id, "Casey Parker", "Casey Mason");
        let sent = sent_to_the_model(&svc, id, 194);
        assert!(
            sent.contains("appears in the transcript as Casey P."),
            "{sent}"
        );
        assert!(sent.contains("Attendees: Casey P, Casey M"), "{sent}");
        assert!(!sent.contains("Casey Mason"), "{sent}");
        // With no last names, numbers; the stored rows keep their names throughout.
        name_rows(&svc, id, "Casey", "Casey");
        let sent = sent_to_the_model(&svc, id, 195);
        assert!(
            sent.contains("appears in the transcript as Casey (1)."),
            "{sent}"
        );
        assert!(sent.contains("Attendees: Casey (1), Casey (2)"), "{sent}");
        let view = crate::view::MeetingView::load(&**svc.store(), id)
            .unwrap()
            .unwrap();
        assert!(view.speakers.iter().all(|s| s.display_name == "Casey"));
    }

    #[test]
    fn summary_stall_limits_are_the_documented_ones() {
        let c = config();
        assert_eq!(c.stall_limit, Duration::from_secs(180));
        assert_eq!(c.load_stall_limit, Duration::from_secs(600));
        assert_eq!(c.cpu_load_limit, Duration::from_secs(600));
        let limit = Duration::from_secs(180);
        assert!(!has_stalled(Duration::from_secs(179), limit, false));
        assert!(has_stalled(limit, limit, false));
        // A cancelled run does not make the user wait out the limit.
        assert!(!has_stalled(Duration::from_secs(29), limit, true));
        assert!(has_stalled(Duration::from_secs(30), limit, true));
    }

    #[test]
    fn summary_a_stalled_gpu_moves_to_the_cpu_and_says_so() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 170);
        let stuck = Stuck::default();
        let (service, shared, flag) = stall_service(OnGpu::Stuck(stuck.clone()));
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(171),
                &CancelToken::new(),
            )
            .unwrap();
        let r = local_report(&out);
        assert_eq!(r.backend, LlamaBackend::Cpu);
        assert_eq!(
            r.gpu_refused.as_deref(),
            Some(&LocalSummaryError::GpuStalled)
        );
        assert_eq!(*shared.load_gpu.lock().unwrap(), [true, false]);
        assert_eq!(svc.store().summaries(id).unwrap().len(), 1);
        assert!(flag.load(Ordering::Acquire));

        // The next summary of the session does not try the GPU, and still says why.
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(172),
                &CancelToken::new(),
            )
            .unwrap();
        let r = local_report(&out);
        assert_eq!(r.backend, LlamaBackend::Cpu);
        assert_eq!(
            r.gpu_refused.as_deref(),
            Some(&LocalSummaryError::GpuStalled)
        );
        assert_eq!(*shared.load_gpu.lock().unwrap(), [true, false, false]);
        stuck.release();
    }

    #[test]
    fn summary_a_stuck_gpu_load_moves_to_the_cpu_too() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 173);
        let stuck = Stuck::default();
        let (service, shared, _) = stall_service(OnGpu::StuckLoad(stuck.clone()));
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(174),
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(local_report(&out).backend, LlamaBackend::Cpu);
        assert_eq!(*shared.load_gpu.lock().unwrap(), [true, false]);
        stuck.release();
    }

    /// What a CPU try does in the T9.22b tests.
    enum OnCpu {
        /// The load never returns until released.
        StuckLoad(Stuck),
        /// The load returns at once; each request then takes this long with no progress reported.
        QuietRun(Duration),
    }

    /// A model whose requests each take a while and report nothing meanwhile.
    struct QuietModel(Duration, FakeModel);

    impl Generate for QuietModel {
        fn context_tokens(&self) -> usize {
            self.1.context_tokens()
        }

        fn prompt_tokens(&self, m: &[ChatMessage], prefix: &str) -> Result<usize, LlmError> {
            self.1.prompt_tokens(m, prefix)
        }

        fn generate(
            &mut self,
            m: &[ChatMessage],
            prefix: &str,
            grammar: &str,
            max_tokens: usize,
            cancel: &CancelToken,
        ) -> Result<Generation, LlmError> {
            std::thread::sleep(self.0);
            self.1.generate(m, prefix, grammar, max_tokens, cancel)
        }
    }

    struct CpuLoader(OnCpu, Arc<Shared>);

    impl LoadLocal for CpuLoader {
        fn load(&self, config: &LocalSummaryConfig) -> Result<LoadedModel, RuntimeError> {
            self.1.load_gpu.lock().unwrap().push(config.gpu);
            let fake = FakeModel(Arc::clone(&self.1));
            let model: Box<dyn Generate> = match &self.0 {
                OnCpu::StuckLoad(s) => {
                    s.hold();
                    Box::new(fake)
                }
                OnCpu::QuietRun(d) => Box::new(QuietModel(*d, fake)),
            };
            Ok(LoadedModel {
                heartbeat: Heartbeat::default(),
                model,
                backend: LlamaBackend::Cpu,
                load_time: Duration::from_millis(5),
            })
        }
    }

    /// A CPU summary with all its limits at 300 ms.
    fn cpu_stall_service(on_cpu: OnCpu) -> SummaryService {
        let mut cfg = config();
        cfg.stall_limit = Duration::from_millis(300);
        cfg.load_stall_limit = Duration::from_millis(300);
        cfg.cpu_load_limit = Duration::from_millis(300);
        let flag: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        SummaryService::local_flagged(
            &cfg,
            Box::new(CpuLoader(on_cpu, Arc::new(Shared::default()))),
            flag,
        )
        .unwrap()
    }

    #[test]
    fn summary_a_cpu_load_that_never_returns_fails_the_summary_and_is_left() {
        // T9.22b: a load polls nothing, so it has its own bound; past it the thread is left behind
        // and the summary fails, so the meeting's turn passes to the next.
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 181);
        let stuck = Stuck::default();
        let service = cpu_stall_service(OnCpu::StuckLoad(stuck.clone()));
        let result = service.summarize(
            &svc,
            id,
            &mut ChaCha8Rng::seed_from_u64(182),
            &CancelToken::new(),
        );
        assert!(
            matches!(
                result,
                Err(RuntimeError::LocalSummary(LocalSummaryError::LoadStalled))
            ),
            "{result:?}"
        );
        assert!(stuck.1.load(Ordering::SeqCst), "it was in the load");
        assert!(svc.store().summaries(id).unwrap().is_empty());
        stuck.release();
    }

    #[test]
    fn summary_a_slow_cpu_run_is_never_cut() {
        // Negative control: each request is quiet for three times the limits, and only the load
        // is watched on the CPU.
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 183);
        let service = cpu_stall_service(OnCpu::QuietRun(Duration::from_millis(900)));
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(184),
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(local_report(&out).backend, LlamaBackend::Cpu);
    }

    #[test]
    fn summary_a_slow_gpu_that_keeps_going_is_not_a_stall() {
        // Negative control: well past the limit in time, but reporting progress throughout.
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 175);
        let (service, shared, flag) = stall_service(OnGpu::Slow(Duration::from_millis(900)));
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(176),
                &CancelToken::new(),
            )
            .unwrap();
        let r = local_report(&out);
        assert_eq!(r.backend, LlamaBackend::Vulkan);
        assert_eq!(r.gpu_refused, None);
        assert_eq!(*shared.load_gpu.lock().unwrap(), [true]);
        assert!(!flag.load(Ordering::Acquire));
    }

    #[test]
    fn summary_a_cancel_during_a_stall_is_a_cancel_not_a_cpu_run() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 177);
        let stuck = Stuck::default();
        // Only the cancel can end this wait: a stall limit that fired first on a busy host would
        // be a CPU run, the other case's answer.
        let (service, shared, flag) =
            stall_service_within(OnGpu::Stuck(stuck.clone()), Duration::from_secs(600));
        let cancel = CancelToken::new();
        let later = cancel.clone();
        let inside = Arc::clone(&stuck.1);
        // Once the run is in the stuck call: a cancel before it is a different path.
        let canceller = std::thread::spawn(move || {
            while !inside.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            later.cancel();
        });
        let err = service
            .summarize(&svc, id, &mut ChaCha8Rng::seed_from_u64(178), &cancel)
            .unwrap_err();
        canceller.join().unwrap();
        assert!(
            matches!(
                &err,
                RuntimeError::LocalSummary(LocalSummaryError::Run(SummaryFailure::Cancelled))
            ),
            "{err:?}"
        );
        assert_eq!(*shared.load_gpu.lock().unwrap(), [true]);
        assert!(svc.store().summaries(id).unwrap().is_empty());
        assert!(flag.load(Ordering::Acquire));
        stuck.release();
    }

    #[test]
    fn summary_a_cpu_run_is_never_watched() {
        // A CPU try with silence longer than any limit still ends with its answer.
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 179);
        let mut cfg = config();
        cfg.stall_limit = Duration::from_millis(1);
        cfg.load_stall_limit = Duration::from_millis(1);
        let (service, shared) = fake_service_with(&cfg, LlamaBackend::Cpu, false);
        *shared.on_call.lock().unwrap() = Some(Box::new(|_: &CancelToken| {
            std::thread::sleep(Duration::from_millis(200))
        }));
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(180),
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(local_report(&out).gpu_refused, None);
    }

    fn local_report(out: &SummaryOutcome) -> &LocalRunReport {
        match &out.report {
            SummaryRunReport::Local(r) => r,
            SummaryRunReport::Remote(r)
            | SummaryRunReport::ClaudeCode(r)
            | SummaryRunReport::Codex(r) => {
                panic!("a remote report: {r:?}")
            }
        }
    }

    #[test]
    fn summary_kept_local_meeting_is_summarised_by_the_local_model() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 53);
        super::keep_local(&svc, id);
        let (service, shared) = fake_service(&config());
        service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(54),
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(shared.loads.load(Ordering::SeqCst), 1);
        assert!(service.audit_log().is_empty());
        assert_eq!(svc.store().summaries(id).unwrap().len(), 1);
    }

    #[test]
    fn summary_local_is_stored_with_no_network_and_the_loaded_backend() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 50);
        let (service, shared) = fake_service(&config());
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(51),
                &CancelToken::new(),
            )
            .unwrap();
        // No connection exists for a local summary.
        assert!(service.audit_log().entries().is_empty());
        assert!(format!("{service:?}").contains("local"));
        let r = local_report(&out);
        assert_eq!(
            r.backend,
            LlamaBackend::Vulkan,
            "from the load, not the config"
        );
        assert_eq!(r.load_time, Duration::from_millis(5));
        // The test meeting is 6 s long: one 600 s window, the ordinary single request.
        assert_eq!((r.chunks, r.run.requests, r.run.map_parts), (1, 1, 0));
        assert_eq!(r.run.steps.len(), 1);
        // Loaded for this call and freed before it returned.
        assert_eq!(shared.loads.load(Ordering::SeqCst), 1);
        assert_eq!(shared.live.load(Ordering::SeqCst), 0);
        // What the model saw: text with speaker names and the note.
        let requests = shared.requests.lock().unwrap().clone();
        let user = &requests[0][1].content;
        assert!(user.contains("yes friday works for the release"), "{user}");
        assert!(user.contains("Alice") && user.contains("check the release notes"));

        assert!(out.title_set);
        let db = svc.store();
        let rows = db.summaries(id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, out.summary_id);
        assert_eq!(rows[0].model.as_deref(), Some("fake-local-model"));
        assert_eq!(rows[0].summary, out.summary);
        // Segment number 1 was mapped back to the first segment's id.
        let first = MeetingView::load(&**db, id).unwrap().unwrap().segments[0].id;
        assert_eq!(out.summary.decisions[0].source_segment_ids, [first]);
        assert_eq!(db.meeting(id).unwrap().unwrap().title, "Release planning");
        drop(db);

        // A second summary loads the model again and replaces the first.
        let again = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(52),
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(shared.loads.load(Ordering::SeqCst), 2);
        assert!(!again.title_set);
        let live: Vec<_> = svc
            .store()
            .summaries(id)
            .unwrap()
            .into_iter()
            .filter(|s| !s.meta.deleted)
            .collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, again.summary_id);
    }

    #[test]
    fn summary_local_gpu_refusal_is_retried_once_on_the_cpu_and_reported() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 70);
        let mut cfg = config();
        cfg.gpu = true;
        cfg.cpu_fallback = true;
        let (service, shared) = fake_service_with(&cfg, LlamaBackend::Cpu, true);
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(71),
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(*shared.load_gpu.lock().unwrap(), [true, false]);
        let r = local_report(&out);
        assert_eq!(r.backend, LlamaBackend::Cpu);
        let refused = r.gpu_refused.as_ref().expect("the refusal is reported");
        assert!(refused.to_string().contains("load log"), "{refused}");
        assert_eq!(svc.store().summaries(id).unwrap().len(), 1);

        // Without the fallback the GPU refusal is the error: no silent CPU run.
        cfg.cpu_fallback = false;
        let (service, shared) = fake_service_with(&cfg, LlamaBackend::Cpu, true);
        let err = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(72),
                &CancelToken::new(),
            )
            .unwrap_err();
        assert!(
            matches!(
                &err,
                RuntimeError::LocalSummary(LocalSummaryError::Load(LlmError::Backend(_)))
            ),
            "{err:?}"
        );
        assert_eq!(*shared.load_gpu.lock().unwrap(), [true]);

        // A GPU load that works is not retried and reports no refusal.
        cfg.cpu_fallback = true;
        let (service, shared) = fake_service_with(&cfg, LlamaBackend::Cuda, false);
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(73),
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(*shared.load_gpu.lock().unwrap(), [true]);
        let r = local_report(&out);
        assert_eq!(r.backend, LlamaBackend::Cuda);
        assert!(r.gpu_refused.is_none());
    }

    #[test]
    fn summary_local_missing_model_is_not_retried_on_the_cpu() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 74);
        let mut cfg = LocalSummaryConfig::new(dir.0.join("missing.gguf"), "m");
        cfg.gpu = true;
        cfg.cpu_fallback = true;
        // Only a llama.cpp load refusal is retried: a missing file is loaded exactly once.
        let shared = Arc::new(Shared::default());
        let loader = FakeLoader {
            backend: LlamaBackend::Cpu,
            refuse_gpu: false,
            missing: true,
            unloadable: false,
            shared: Arc::clone(&shared),
        };
        let service = SummaryService::local_with(&cfg, Box::new(loader)).unwrap();
        let err = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(75),
                &CancelToken::new(),
            )
            .unwrap_err();
        assert!(matches!(err, RuntimeError::ModelMissing { .. }), "{err:?}");
        assert_eq!(*shared.load_gpu.lock().unwrap(), [true], "no CPU retry");
        assert!(svc.store().summaries(id).unwrap().is_empty());
    }

    #[test]
    fn summary_local_cpu_retry_that_fails_too_keeps_both_errors() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 76);
        let mut cfg = config();
        cfg.gpu = true;
        cfg.cpu_fallback = true;
        // A file llama.cpp cannot load on any backend (a corrupt GGUF): the GPU error is
        // not dropped when the CPU retry fails too.
        let shared = Arc::new(Shared::default());
        let loader = FakeLoader {
            backend: LlamaBackend::Cpu,
            refuse_gpu: false,
            missing: false,
            unloadable: true,
            shared: Arc::clone(&shared),
        };
        let service = SummaryService::local_with(&cfg, Box::new(loader)).unwrap();
        let err = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(77),
                &CancelToken::new(),
            )
            .unwrap_err();
        assert_eq!(*shared.load_gpu.lock().unwrap(), [true, false]);
        let RuntimeError::LocalSummary(LocalSummaryError::LoadRetried { gpu, cpu }) = &err else {
            panic!("{err:?}");
        };
        assert!(
            matches!(**gpu, LocalSummaryError::Load(LlmError::Load(ref m)) if m.contains("gpu")),
            "{gpu:?}"
        );
        assert!(
            matches!(**cpu, LocalSummaryError::Load(LlmError::Load(ref m)) if m.contains("cpu")),
            "{cpu:?}"
        );
        let text = err.to_string();
        assert!(text.contains("gpu") && text.contains("cpu"), "{text}");
        assert!(svc.store().summaries(id).unwrap().is_empty());
    }

    #[test]
    fn summary_local_meeting_longer_than_a_window_is_summarised_in_time_chunks() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "Weekly", 53);
        // Segments start at 1 s and 5 s: 3 s windows put them in two chunks.
        let mut cfg = config();
        cfg.chunk_seconds = 3;
        let (service, shared) = fake_service(&cfg);
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(54),
                &CancelToken::new(),
            )
            .unwrap();
        let r = local_report(&out);
        assert_eq!(r.chunks, 2);
        assert_eq!(
            (r.run.map_parts, r.run.reduce_steps, r.run.requests),
            (2, 1, 3)
        );
        let steps: Vec<&str> = r.run.steps.iter().map(|s| s.step).collect();
        assert_eq!(steps, ["map", "map", "reduce"]);
        // Each map request holds its own window's segment only.
        let requests = shared.requests.lock().unwrap().clone();
        assert!(requests[0][1].content.contains("shall we ship on friday"));
        assert!(!requests[0][1].content.contains("yes friday works"));
        assert!(requests[1][1].content.contains("yes friday works"));
        assert!(!requests[1][1].content.contains("shall we ship on friday"));
        assert_eq!(svc.store().summaries(id).unwrap().len(), 1);
    }

    /// Copies `id`'s final segments as a live draft without speakers ("Me" / "Them"), as a
    /// live session leaves it; returns the live ids in meeting order.
    fn live_draft(svc: &Services, id: EntityId, seed: u64) -> Vec<EntityId> {
        use earshot_core::model::{SegmentRow, TranscriptPass, live_segments};
        let clock = Arc::clone(&svc.clock);
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut db = svc.store();
        let mut fin: Vec<_> = live_segments(&**db, id, TranscriptPass::Final)
            .unwrap()
            .into_iter()
            .map(|r| r.segment)
            .collect();
        fin.sort_by_key(|s| s.start);
        let (store, hlc) = db.parts();
        fin.into_iter()
            .map(|mut s| {
                s.id = EntityId::generate(&*clock, &mut rng);
                s.speaker_id = None;
                let live = s.id;
                store
                    .put_segment(SegmentRow::new(id, TranscriptPass::Live, s, hlc, &*clock))
                    .unwrap();
                live
            })
            .collect()
    }

    /// A partial for window 0 citing `cites`, owned by "Me".
    fn live_partial(cites: &[EntityId]) -> SummaryV1 {
        SummaryV1 {
            title: String::new(),
            tldr: "Made during the meeting.".into(),
            enhanced_notes: Vec::new(),
            decisions: vec![earshot_core::summary::Decision {
                text: "Recited decision".into(),
                source_segment_ids: cites.to_vec(),
            }],
            action_items: vec![earshot_core::summary::ActionItem {
                owner: "Me".into(),
                task: "Draft the release note".into(),
                due: None,
                source_segment_ids: cites.to_vec(),
            }],
            open_questions: Vec::new(),
            topics: Vec::new(),
        }
    }

    fn store_live_record(svc: &Services, id: EntityId, model: &str, cites: &[EntityId]) {
        let record = crate::live_summaries::LiveSummaryRecord {
            model: model.into(),
            seconds: 3,
            partials: vec![crate::live_summaries::LivePartial {
                window: 0,
                summary: live_partial(cites),
            }],
            stopped: None,
            vocabulary: Some(Vec::new()),
        };
        crate::live_summaries::record_live_summaries(&mut svc.store(), id, &record).unwrap();
    }

    fn live_record_of(svc: &Services, id: EntityId) -> bool {
        crate::live_summaries::live_summary_record(&svc.store(), id)
            .unwrap()
            .is_some()
    }

    #[test]
    fn summary_local_reuses_a_live_summary_partial_recited_onto_the_final_transcript() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "Weekly", 120);
        let live = live_draft(&svc, id, 121);
        // Window 0 (segment at 1 s, 3 s windows) was summarised during the meeting.
        store_live_record(&svc, id, "fake-local-model", &live[..1]);
        let mut cfg = config();
        cfg.chunk_seconds = 3;
        let (service, shared) = fake_service(&cfg);
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(122),
                &CancelToken::new(),
            )
            .unwrap();
        let r = local_report(&out);
        assert_eq!(
            *r.live,
            crate::live_summaries::LiveReuse {
                recorded: 1,
                reused: 1,
                ..Default::default()
            }
        );
        // Only window 1 is mapped at hang-up, then the reduce over both partials.
        assert_eq!((r.chunks, r.run.reused_parts, r.run.requests), (2, 1, 2));
        let steps: Vec<&str> = r.run.steps.iter().map(|s| s.step).collect();
        assert_eq!(steps, ["map", "reduce"]);
        let requests = shared.requests.lock().unwrap().clone();
        assert!(requests[0][1].content.contains("yes friday works"));
        assert!(!requests[0][1].content.contains("shall we ship on friday"));
        // The reduce sees the re-cited partial (its text, a final segment number).
        assert!(requests[1][1].content.contains("Recited decision"));
        // Stored, and the partials (content) went with the summary's write.
        assert_eq!(svc.store().summaries(id).unwrap().len(), 1);
        assert!(!live_record_of(&svc, id));
    }

    #[test]
    fn summary_local_redoes_a_live_summary_partial_that_cannot_be_recited() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "Weekly", 130);
        live_draft(&svc, id, 131);
        let mut cfg = config();
        cfg.chunk_seconds = 3;
        // It cites a segment the live draft does not have.
        let stranger =
            EntityId::from_bytes([1, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 7]);
        store_live_record(&svc, id, "fake-local-model", &[stranger]);
        let (service, _) = fake_service(&cfg);
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(132),
                &CancelToken::new(),
            )
            .unwrap();
        let r = local_report(&out);
        assert_eq!((r.live.recorded, r.live.recite_failed), (1, 1));
        assert_eq!(
            (r.run.reused_parts, r.run.requests),
            (0, 3),
            "both windows mapped"
        );
        assert!(!live_record_of(&svc, id));

        // Another model's partials are not used.
        let id = meeting(&svc, "Weekly", 133);
        let live = live_draft(&svc, id, 134);
        store_live_record(&svc, id, "another-model", &live[..1]);
        let (service, _) = fake_service(&cfg);
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(135),
                &CancelToken::new(),
            )
            .unwrap();
        let r = local_report(&out);
        assert!(
            r.live
                .not_used
                .as_deref()
                .unwrap()
                .contains("another-model")
        );
        assert_eq!(r.run.requests, 3);
    }

    #[test]
    fn summary_local_failure_keeps_the_live_summary_partials_for_a_retry() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "Weekly", 140);
        let live = live_draft(&svc, id, 141);
        store_live_record(&svc, id, "fake-local-model", &live[..1]);
        let mut cfg = config();
        cfg.chunk_seconds = 3;
        let (service, _) = fake_service(&cfg);
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = service
            .summarize(&svc, id, &mut ChaCha8Rng::seed_from_u64(142), &cancel)
            .unwrap_err();
        assert!(matches!(err, RuntimeError::LocalSummary(e) if e.is_cancelled()));
        assert!(live_record_of(&svc, id), "nothing stored, nothing deleted");
        // A remote summary's write deletes them too (the record belongs to the meeting,
        // not the backend).
        let view = MeetingView::load(&**svc.store(), id).unwrap().unwrap();
        crate::summary::store_summary(
            &svc,
            &view,
            "remote".into(),
            &live_partial(&[view.segments[0].id]),
            &mut ChaCha8Rng::seed_from_u64(143),
        )
        .unwrap();
        assert!(!live_record_of(&svc, id));
    }

    /// Summarises `id` with a fake model that runs `change` while the first request
    /// generates (the input has been read, the result not yet stored).
    fn summarize_local_while(
        svc: &Services,
        id: EntityId,
        change: impl FnOnce(&Services) + Send + 'static,
    ) -> Result<SummaryOutcome, RuntimeError> {
        let (service, shared) = fake_service(&config());
        let other = svc.clone();
        *shared.on_call.lock().unwrap() = Some(Box::new(move |_| change(&other)));
        let result = service.summarize(
            svc,
            id,
            &mut ChaCha8Rng::seed_from_u64(55),
            &CancelToken::new(),
        );
        assert!(shared.on_call.lock().unwrap().is_none(), "the change ran");
        assert_eq!(shared.live.load(Ordering::SeqCst), 0);
        result
    }

    #[test]
    fn summary_local_of_a_transcript_replaced_during_the_call_is_not_stored() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 56);
        let result = summarize_local_while(&svc, id, move |s| replace_transcript_now(s, id, 57));
        assert!(
            matches!(result, Err(RuntimeError::Conflict(_))),
            "{result:?}"
        );
        assert!(svc.store().summaries(id).unwrap().is_empty());
        assert_eq!(svc.store().meeting(id).unwrap().unwrap().title, "");
    }

    #[test]
    fn summary_local_of_a_meeting_deleted_during_the_call_is_not_stored() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 58);
        let result = summarize_local_while(&svc, id, move |s| delete_meeting_now(s, id));
        assert!(
            matches!(result, Err(RuntimeError::Conflict(_))),
            "{result:?}"
        );
        assert!(svc.store().summaries(id).unwrap().is_empty());
    }

    #[test]
    fn summary_local_cancelled_stores_nothing() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 59);
        // Cancelled while the first request generates.
        let (service, shared) = fake_service(&config());
        *shared.on_call.lock().unwrap() = Some(Box::new(CancelToken::cancel));
        let err = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(60),
                &CancelToken::new(),
            )
            .unwrap_err();
        let RuntimeError::LocalSummary(e) = &err else {
            panic!("{err:?}");
        };
        assert!(e.is_cancelled(), "{e:?}");
        assert_eq!(shared.requests.lock().unwrap().len(), 1);
        assert_eq!(shared.live.load(Ordering::SeqCst), 0);
        // Cancelled before the call: the model is not even loaded.
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = service
            .summarize(&svc, id, &mut ChaCha8Rng::seed_from_u64(61), &cancel)
            .unwrap_err();
        assert!(
            matches!(&err, RuntimeError::LocalSummary(e) if e.is_cancelled()),
            "{err:?}"
        );
        assert_eq!(shared.loads.load(Ordering::SeqCst), 1);
        let db = svc.store();
        assert!(db.summaries(id).unwrap().is_empty());
        assert_eq!(db.meeting(id).unwrap().unwrap().title, "");
        assert!(service.audit_log().entries().is_empty());
    }

    #[test]
    fn summary_local_settings_and_model_files_are_checked() {
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 62);
        // A zero window is refused when the service is made.
        let mut cfg = config();
        cfg.chunk_seconds = 0;
        assert!(matches!(
            SummaryService::local(&cfg),
            Err(RuntimeError::LocalSummary(LocalSummaryError::Chunks(
                TimeChunkError::ZeroWindow
            )))
        ));
        // A missing model file is ModelMissing, with its path.
        let missing = dir.0.join("no-such-model.gguf");
        let service = SummaryService::local(&LocalSummaryConfig::new(&missing, "m")).unwrap();
        let err = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(63),
                &CancelToken::new(),
            )
            .unwrap_err();
        assert!(
            matches!(&err, RuntimeError::ModelMissing { path, .. } if *path == missing),
            "{err:?}"
        );
        // A file llama.cpp cannot load is a load error (llama.cpp ran and refused it).
        let bad = dir.0.join("not-a-model.gguf");
        std::fs::write(&bad, b"this is not a gguf file").unwrap();
        let service = SummaryService::local(&LocalSummaryConfig::new(&bad, "m")).unwrap();
        let err = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(64),
                &CancelToken::new(),
            )
            .unwrap_err();
        assert!(
            matches!(
                &err,
                RuntimeError::LocalSummary(LocalSummaryError::Load(
                    earshot_llm_local::LlmError::Load(_)
                ))
            ),
            "{err:?}"
        );
        assert!(svc.store().summaries(id).unwrap().is_empty());
        assert!(service.audit_log().entries().is_empty());
    }

    #[test]
    #[ignore = "needs the summary model set: cargo xtask fetch-models --set summary; run: cargo test -p earshot-runtime summary -- --include-ignored"]
    fn summary_local_with_the_low_memory_model_on_the_cpu_is_stored_with_no_network() {
        // The shipped low-memory model with its production settings (PLAN D22: tests load
        // only default models; the `llm` set's Qwen2.5 1.5B until then), on the CPU.
        let data = earshot_store::DataDir::from_local_app_data()
            .expect("LOCALAPPDATA is not set; set EARSHOT_MODELS to the models directory");
        let models = crate::backends::ModelDir::from_env(&data);
        let choice = crate::summary_model::choose_summary_model(
            Some(crate::summary_model::LOW_MEMORY_SUMMARY_MODEL),
            None,
            false,
        )
        .unwrap();
        let config = crate::summary_model::local_summary_config(&models, &choice, false)
            .unwrap_or_else(|e| panic!("{e}"));
        let dir = TempDir::new("summary");
        let (svc, _) = services(&dir);
        let id = meeting(&svc, "", 65);
        let service = SummaryService::local(&config).unwrap();
        let out = service
            .summarize(
                &svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(66),
                &CancelToken::new(),
            )
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(service.audit_log().entries().is_empty());
        let r = local_report(&out);
        assert_eq!(r.backend, LlamaBackend::Cpu);
        assert_eq!(r.chunks, 1);
        assert!(r.run.requests >= 1, "{r:?}");
        let db = svc.store();
        let rows = db.summaries(id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].model.as_deref(),
            Some(crate::summary_model::LOW_MEMORY_SUMMARY_MODEL)
        );
        assert_eq!(rows[0].summary, out.summary);
        assert!(!out.summary.title.trim().is_empty());
        // Every citation is a segment of the meeting.
        let ids: Vec<EntityId> = MeetingView::load(&**db, id)
            .unwrap()
            .unwrap()
            .segments
            .iter()
            .map(|s| s.id)
            .collect();
        for d in &out.summary.decisions {
            assert!(
                d.source_segment_ids.iter().all(|s| ids.contains(s)),
                "{d:?}"
            );
        }
    }
}

#[test]
fn summary_a_load_on_an_integrated_gpu_counts_as_refused() {
    let radeon = "AMD Radeon(TM) Graphics";
    let refuse = vec![radeon.to_owned()];
    assert_eq!(
        refused_gpu(LlamaBackend::Vulkan, Some(radeon), &refuse),
        Some(radeon)
    );
    // Another GPU, no named GPU, nothing to refuse, and a CPU load of a GPU build, which names
    // a device it did not use.
    assert_eq!(
        refused_gpu(
            LlamaBackend::Vulkan,
            Some("NVIDIA GeForce RTX 5080"),
            &refuse
        ),
        None
    );
    assert_eq!(refused_gpu(LlamaBackend::Vulkan, None, &refuse), None);
    assert_eq!(refused_gpu(LlamaBackend::Vulkan, Some(radeon), &[]), None);
    assert_eq!(refused_gpu(LlamaBackend::Cpu, Some(radeon), &refuse), None);
}
