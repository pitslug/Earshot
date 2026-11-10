//! Tests of the Summaries page IPC (T7.4g): settings, the API key flow, and the meeting's
//! summary setup from them. Every name contains `summaries`.

use std::cell::Cell;

use earshot_core::identity::EntityId;
use earshot_runtime::claude_code::{ClaudeCodeError, ClaudeCodeStatus, FixedClaudeCode, SignIn};
use earshot_runtime::codex::{CodexError, CodexSignIn, CodexStatus, FixedCodex};
use earshot_runtime::host::{SkipReason, SummaryResult};
use earshot_runtime::summary_settings::{ApiKey, ApiKeyStore};

use crate::meeting_events::{
    MeetingEvent, SummaryEvent, WarningKind, claude_code_banner, codex_banner, no_key_banner,
    summary_result_event,
};

use super::*;
use crate::credentials::MemoryVault;
use crate::meeting::{live_summaries, summary_setup};
use crate::testutil::{TempDir, services};

const SECRET: &str = "sk-ant-api03-summaries-test-secret-42";

fn value(
    writer: WriterChoice,
    provider: ProviderChoice,
    url: &str,
    model: &str,
) -> SummarySettingsValue {
    SummarySettingsValue {
        writer,
        provider,
        url: url.into(),
        model: model.into(),
        live: false,
        claude_model: String::new(),
        claude_effort: None,
        codex_model: String::new(),
    }
}

/// A Claude Code probe that must not matter (no Claude Code choice).
fn no_claude() -> FixedClaudeCode {
    FixedClaudeCode(ClaudeCodeStatus::NotInstalled)
}

/// A Codex probe that must not matter (no Codex choice).
fn no_codex() -> FixedCodex {
    FixedCodex(CodexStatus::NotInstalled)
}

/// Codex installed with `sign_in`.
fn codex_installed(sign_in: CodexSignIn) -> FixedCodex {
    FixedCodex(CodexStatus::Installed {
        cli: earshot_runtime::codex::CodexCli::at("C:\\fake\\codex.exe".into()),
        version: Some("codex-cli 0.155.1".into()),
        sign_in,
    })
}

/// Claude Code installed with `sign_in`, its help listing `--effort`.
fn claude_installed(sign_in: SignIn) -> FixedClaudeCode {
    claude_installed_with(sign_in, Some(true))
}

/// Claude Code installed with `sign_in`; `accepts_effort` as its help answered.
fn claude_installed_with(sign_in: SignIn, accepts_effort: Option<bool>) -> FixedClaudeCode {
    FixedClaudeCode(ClaudeCodeStatus::Installed {
        cli: earshot_runtime::claude_code::ClaudeCli {
            program: "C:\\fake\\claude.exe".into(),
            leading_args: Vec::new(),
        },
        version: Some("2.1.290 (Claude Code)".into()),
        sign_in,
        accepts_effort,
    })
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

#[test]
fn summaries_settings_default_and_round_trip() {
    let dir = TempDir::new("summaries-round-trip");
    let svc = services(&dir);
    let v = settings_view(&svc).unwrap();
    assert_eq!(
        v.value,
        SummarySettingsValue {
            writer: WriterChoice::OnThisComputer,
            provider: ProviderChoice::OpenAiCompatible,
            url: String::new(),
            model: String::new(),
            live: false,
            claude_model: String::new(),
            claude_effort: None,
            codex_model: String::new(),
        }
    );
    assert!(v.unknown.is_empty());
    assert_eq!(
        v.live_unavailable, None,
        "on this computer the switch is usable"
    );
    assert_eq!(v.endpoint_host, None);
    assert_eq!(v.anthropic_url, "https://api.anthropic.com");
    let want = SummarySettingsValue {
        live: true,
        ..value(
            WriterChoice::OwnEndpoint,
            ProviderChoice::Anthropic,
            " https://api.anthropic.com ",
            "claude-sonnet-4-5",
        )
    };
    let stored = set_settings(&svc, &want).unwrap();
    assert_eq!(stored.value.url, "https://api.anthropic.com");
    assert_eq!(stored.endpoint_host.as_deref(), Some("api.anthropic.com"));
    assert!(!stored.endpoint_local);
    assert_eq!(
        stored.live_unavailable.as_deref(),
        Some("Summarising during the meeting needs a local summary model.")
    );
    assert_eq!(settings_view(&svc).unwrap(), stored);
    // A LAN endpoint needs no warning.
    let lan = set_settings(
        &svc,
        &value(
            WriterChoice::OwnEndpoint,
            ProviderChoice::OpenAiCompatible,
            "http://192.168.1.20:8080/v1",
            "qwen",
        ),
    )
    .unwrap();
    assert!(lan.endpoint_local);
    let none = set_settings(
        &svc,
        &value(
            WriterChoice::NoSummary,
            ProviderChoice::OpenAiCompatible,
            "",
            "",
        ),
    )
    .unwrap();
    assert!(none.live_unavailable.is_some());
}

#[test]
fn summaries_refused_settings_are_invalid_setting_and_store_nothing() {
    let dir = TempDir::new("summaries-refused");
    let svc = services(&dir);
    for (v, want) in [
        (
            value(
                WriterChoice::OwnEndpoint,
                ProviderChoice::OpenAiCompatible,
                "",
                "m",
            ),
            "Enter the endpoint's address.",
        ),
        (
            value(
                WriterChoice::OwnEndpoint,
                ProviderChoice::OpenAiCompatible,
                "https://llm.example.com/v1",
                "",
            ),
            "Enter the model name the endpoint expects.",
        ),
        (
            value(
                WriterChoice::OwnEndpoint,
                ProviderChoice::OpenAiCompatible,
                "http://llm.example.com/v1",
                "m",
            ),
            "This address cannot be used: plain http is only allowed to this computer or the \
             local network; use https.",
        ),
        (
            value(
                WriterChoice::OwnEndpoint,
                ProviderChoice::Anthropic,
                "https://api.anthropic.com/v1",
                "claude-opus-5-5",
            ),
            "This address cannot be used: use the address without a path, for example \
             https://api.anthropic.com.",
        ),
    ] {
        let e = set_settings(&svc, &v).unwrap_err();
        assert_eq!(e.kind, "invalid_setting");
        assert_eq!(e.message, want);
        assert!(!e.retryable);
        assert_eq!(
            settings_view(&svc).unwrap().value.writer,
            WriterChoice::OnThisComputer
        );
    }
}

#[test]
fn summaries_api_key_flow_never_returns_the_key() {
    let vault = MemoryVault::default();
    let mut responses = Vec::new();
    responses.push(json(&has_api_key(&vault).unwrap()));
    assert!(!has_api_key(&vault).unwrap());
    set_api_key(&vault, &format!("  {SECRET}\n")).unwrap();
    responses.push(json(&set_api_key(&vault, SECRET)));
    assert!(has_api_key(&vault).unwrap());
    responses.push(json(&has_api_key(&vault)));
    assert_eq!(
        vault.anthropic_key().unwrap(),
        ApiKey::new(SECRET),
        "stored trimmed"
    );
    // Something that is not a key is refused without quoting it.
    let e = set_api_key(&vault, &format!("{SECRET} and more")).unwrap_err();
    assert_eq!(e.kind, "invalid_setting");
    assert_eq!(
        e.message,
        "This API key cannot be saved: it holds spaces or line breaks; paste only the key."
    );
    responses.push(json(&e));
    assert_eq!(vault.anthropic_key().unwrap(), ApiKey::new(SECRET), "kept");
    remove_api_key(&vault).unwrap();
    responses.push(json(&remove_api_key(&vault)));
    assert!(!has_api_key(&vault).unwrap());
    // A failing store is told as it is, never with the key.
    let broken = MemoryVault::broken("access denied");
    let e = set_api_key(&broken, SECRET).unwrap_err();
    assert_eq!(e.kind, "api_key");
    assert_eq!(
        e.message,
        "Windows Credential Manager failed: access denied."
    );
    responses.push(json(&e));
    responses.push(json(&has_api_key(&broken)));
    responses.push(json(&remove_api_key(&broken)));
    for r in &responses {
        assert!(!r.contains(SECRET) && !r.contains("summaries-test"), "{r}");
    }
}

#[test]
fn summaries_settings_responses_never_carry_the_key() {
    let dir = TempDir::new("summaries-no-key");
    let svc = services(&dir);
    let vault = MemoryVault::default();
    set_api_key(&vault, SECRET).unwrap();
    let v = set_settings(
        &svc,
        &value(
            WriterChoice::OwnEndpoint,
            ProviderChoice::Anthropic,
            "https://api.anthropic.com",
            "claude-sonnet-4-5",
        ),
    )
    .unwrap();
    for text in [json(&v), json(&settings_view(&svc).unwrap())] {
        assert!(!text.contains(SECRET), "{text}");
    }
    // The meeting's setup reads it and keeps it out of everything it reports.
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    let setup = summary_setup(
        &svc,
        &models,
        &vault,
        &no_claude(),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    let prepared = setup.summary.as_ref().expect("a remote service");
    assert!(prepared.local.is_none());
    assert!(!format!("{prepared:?}").contains(SECRET));
    assert!(setup.notes.iter().all(|n| !n.contains(SECRET)));
}

#[test]
fn summaries_meeting_setup_follows_who_writes_the_summary() {
    let dir = TempDir::new("summaries-setup");
    let svc = services(&dir);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    let vault = MemoryVault::default();
    // No summary: nothing runs, the existing skip reason.
    set_settings(
        &svc,
        &value(
            WriterChoice::NoSummary,
            ProviderChoice::OpenAiCompatible,
            "",
            "",
        ),
    )
    .unwrap();
    let s = summary_setup(
        &svc,
        &models,
        &vault,
        &no_claude(),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(s.summary.is_none());
    assert_eq!(s.no_summary, SkipReason::NotRequested);
    assert!(!s.local_chosen);
    // On this computer without the model installed: the not-installed skip.
    set_settings(
        &svc,
        &value(
            WriterChoice::OnThisComputer,
            ProviderChoice::OpenAiCompatible,
            "",
            "",
        ),
    )
    .unwrap();
    let s = summary_setup(
        &svc,
        &models,
        &vault,
        &no_claude(),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(s.summary.is_none());
    assert!(
        matches!(s.no_summary, SkipReason::NotInstalled { .. }),
        "{:?}",
        s.no_summary
    );
    assert!(s.local_chosen);
    // An OpenAI-compatible endpoint: a remote service, no key needed.
    set_settings(
        &svc,
        &value(
            WriterChoice::OwnEndpoint,
            ProviderChoice::OpenAiCompatible,
            "http://127.0.0.1:8080/v1",
            "qwen",
        ),
    )
    .unwrap();
    let s = summary_setup(
        &svc,
        &models,
        &vault,
        &no_claude(),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    let prepared = s.summary.expect("remote");
    assert!(prepared.local.is_none());
    assert_eq!(prepared.service.model(), "qwen");
    assert_eq!(
        prepared.service.audit_log().entries().len(),
        0,
        "nothing sent yet"
    );
    // Anthropic without a key: no service and the skip reason (the meeting still runs),
    // never a local model or another provider.
    set_settings(
        &svc,
        &value(
            WriterChoice::OwnEndpoint,
            ProviderChoice::Anthropic,
            "https://api.anthropic.com",
            "claude-sonnet-4-5",
        ),
    )
    .unwrap();
    let s = summary_setup(
        &svc,
        &models,
        &vault,
        &no_claude(),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(s.summary.is_none());
    assert_eq!(s.no_summary, SkipReason::NoApiKey(ApiKeyError::Missing));
    // With one: a remote service.
    set_api_key(&vault, SECRET).unwrap();
    let s = summary_setup(
        &svc,
        &models,
        &vault,
        &no_claude(),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert_eq!(
        s.summary.expect("remote").service.model(),
        "claude-sonnet-4-5"
    );
}

#[test]
fn summaries_live_switch_is_honoured_only_on_this_computer() {
    let dir = TempDir::new("summaries-live");
    let svc = services(&dir);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    let vault = MemoryVault::default();
    let order = [earshot_runtime::backends::Accelerator::Cpu];
    // Off: nothing, nothing said.
    let s = summary_setup(
        &svc,
        &models,
        &vault,
        &no_claude(),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(matches!(live_summaries(&s, &order), (None, None)));
    // On with a local choice whose model is not installed: off for this meeting, told.
    set_settings(
        &svc,
        &SummarySettingsValue {
            live: true,
            ..value(
                WriterChoice::OnThisComputer,
                ProviderChoice::OpenAiCompatible,
                "",
                "",
            )
        },
    )
    .unwrap();
    let s = summary_setup(
        &svc,
        &models,
        &vault,
        &no_claude(),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    let (l, note) = live_summaries(&s, &order);
    assert!(l.is_none());
    assert_eq!(
        note.as_deref(),
        Some(
            "Summarising during the meeting is off for this meeting: summarising during the \
             meeting needs a local summary model."
        )
    );
    // On with an endpoint: not used, and the page already says why.
    set_settings(
        &svc,
        &SummarySettingsValue {
            live: true,
            ..value(
                WriterChoice::OwnEndpoint,
                ProviderChoice::OpenAiCompatible,
                "http://127.0.0.1:8080/v1",
                "qwen",
            )
        },
    )
    .unwrap();
    let s = summary_setup(
        &svc,
        &models,
        &vault,
        &no_claude(),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(s.live);
    assert!(matches!(live_summaries(&s, &order), (None, None)));
}

/// A key store that answers `answer` and counts its reads.
struct Counting {
    answer: Result<Option<&'static str>, ApiKeyError>,
    reads: Cell<u32>,
}

impl ApiKeyStore for Counting {
    fn anthropic_key(&self) -> Result<Option<ApiKey>, ApiKeyError> {
        self.reads.set(self.reads.get() + 1);
        self.answer.clone().map(|k| k.and_then(ApiKey::new))
    }
}

fn anthropic(svc: &earshot_runtime::Services) {
    set_settings(
        svc,
        &value(
            WriterChoice::OwnEndpoint,
            ProviderChoice::Anthropic,
            "https://api.anthropic.com",
            "claude-opus-5-5",
        ),
    )
    .unwrap();
}

#[test]
fn summaries_no_api_key_skips_the_meetings_summary_with_a_banner() {
    let dir = TempDir::new("summaries-no-key-meeting");
    let svc = services(&dir);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    anthropic(&svc);
    let keys = Counting {
        answer: Ok(None),
        reads: Cell::new(0),
    };
    // The setup succeeds, so the meeting starts.
    let s = summary_setup(
        &svc,
        &models,
        &keys,
        &no_claude(),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert_eq!(keys.reads.get(), 1, "the key store is read once");
    assert!(s.summary.is_none());
    assert_eq!(s.no_summary, SkipReason::NoApiKey(ApiKeyError::Missing));
    assert_eq!(
        no_key_banner(&s.no_summary),
        Some(MeetingEvent::Warning {
            key: "summary-key".into(),
            kind: WarningKind::Backend,
            message: "The summary will not run: no Anthropic API key is saved. Add it in \
                      Settings, Summaries, then re-summarise this meeting."
                .into(),
        })
    );
    assert_eq!(no_key_banner(&SkipReason::NotRequested), None);
    // The meeting's final Summary event carries the reason.
    let event = summary_result_event(
        &SummaryResult::Skipped {
            reason: s.no_summary.clone(),
        },
        0,
    );
    assert_eq!(
        event,
        SummaryEvent::Skipped {
            reason: "The summary did not run: no Anthropic API key is saved. Add it in \
                     Settings, Summaries, then re-summarise this meeting."
                .into()
        }
    );
    // A Credential Manager failure is a skip too, with its text.
    let broken = Counting {
        answer: Err(ApiKeyError::Store("access denied".into())),
        reads: Cell::new(0),
    };
    let s = summary_setup(
        &svc,
        &models,
        &broken,
        &no_claude(),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert_eq!(broken.reads.get(), 1);
    let Some(MeetingEvent::Warning { message, .. }) = no_key_banner(&s.no_summary) else {
        panic!("{:?}", s.no_summary);
    };
    assert_eq!(
        message,
        "The summary will not run: Windows Credential Manager could not be read (access \
         denied). Check the key in Settings, Summaries, then re-summarise this meeting."
    );
    assert!(!message.contains('\u{2014}'), "{message}");
}

/// Chooses a summary model by name in the Models page's settings, through its own IPC path.
fn choose_summary_model(svc: &earshot_runtime::Services, model: &str) {
    let mut value = crate::models::settings_view(svc).unwrap().value;
    value.summary = Some(model.to_owned());
    crate::models::set_settings(svc, &value).unwrap();
}

fn assert_summary_model_copy(message: &str, model: &str) {
    assert!(
        message.contains(earshot_runtime::catalogue::display_name(model)),
        "{message}"
    );
    assert!(message.contains("Settings, Summaries"), "{message}");
    assert!(!message.contains("speech helpers"), "{message}");
    assert!(!message.contains("transcription"), "{message}");
    assert!(
        !crate::plain_text::names_developer_tool(message),
        "{message}"
    );
    assert_eq!(earshot_runtime::catalogue::manifest_id_in(message), None);
}

/// The meeting's pre-flight (`HostPrepare::prepare` calls this `summary_setup`) with a summary
/// model chosen by name and not downloaded: a summary setting never costs a recording (decided
/// by the owner, T9.1q). The meeting runs, no summary is prepared, and the setup carries what the
/// start banner and an explicit re-summarise say.
#[test]
fn summaries_a_chosen_summary_model_not_downloaded_skips_the_summary_naming_that_model() {
    let dir = TempDir::new("summaries-chosen-missing");
    let svc = services(&dir);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    let model = earshot_runtime::summary_model::LOW_MEMORY_SUMMARY_MODEL;
    choose_summary_model(&svc, model);
    let s = summary_setup(
        &svc,
        &models,
        &MemoryVault::default(),
        &no_claude(),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .map_err(|e| e.message)
    .expect("a chosen model that is not there never refuses the meeting");
    assert!(s.summary.is_none());
    assert!(s.local_chosen);
    assert!(
        matches!(s.no_summary, SkipReason::ChosenNotInstalled { model: m } if m == model),
        "{:?}",
        s.no_summary
    );
    // The banner at the start, and the skipped Summary event at the end.
    let Some(MeetingEvent::Warning { key, message, .. }) =
        crate::meeting_events::chosen_model_banner(&s.no_summary)
    else {
        panic!("{:?}", s.no_summary);
    };
    assert_eq!(key, "summary-model");
    assert!(message.starts_with("The summary will not run"), "{message}");
    assert_summary_model_copy(&message, model);
    assert_summary_model_copy(&crate::meeting_events::skip_copy(&s.no_summary), model);
    // What an explicit re-summarise fails with.
    let e = s.chosen_missing.expect("the job's refusal");
    assert_eq!(e.kind, "model");
    assert!(!e.retryable);
    assert_summary_model_copy(&e.message, model);
}

/// An automatic model that is not installed gets no banner from this rule: only a choice the
/// user made is reported ahead of the meeting.
#[test]
fn summaries_an_automatic_model_not_installed_is_not_a_chosen_model_banner() {
    let reason = SkipReason::NotInstalled {
        model: earshot_runtime::summary_model::LOW_MEMORY_SUMMARY_MODEL,
        why: String::new(),
    };
    assert!(crate::meeting_events::chosen_model_banner(&reason).is_none());
    assert!(crate::meeting_events::chosen_model_banner(&SkipReason::NotRequested).is_none());
}

/// The re-summarise job, the same way: the user asked for this summary, so it fails.
#[test]
fn summaries_resummarise_with_a_chosen_summary_model_not_downloaded_fails_naming_that_model() {
    let dir = TempDir::new("summaries-chosen-missing-job");
    let svc = services(&dir);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    let model = earshot_runtime::summary_model::DEFAULT_SUMMARY_MODEL;
    choose_summary_model(&svc, model);
    let mut warned = Vec::new();
    let e = crate::jobs::summarise_with(
        &svc,
        EntityId::from_bytes([1; 16]),
        &models,
        &MemoryVault::default(),
        &no_claude(),
        &no_codex(),
        &mut |w| warned.push(w),
    )
    .expect_err("the job fails");
    assert_eq!(e.kind, "model");
    assert_summary_model_copy(&e.message, model);
    assert!(warned.is_empty());
}

#[test]
fn summaries_resummarise_without_an_api_key_fails_with_the_api_key_message() {
    let dir = TempDir::new("summaries-no-key-job");
    let svc = services(&dir);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    anthropic(&svc);
    let vault = MemoryVault::default();
    let mut warned = Vec::new();
    let meeting = EntityId::from_bytes([1; 16]);
    let e = crate::jobs::summarise_with(
        &svc,
        meeting,
        &models,
        &vault,
        &no_claude(),
        &no_codex(),
        &mut |w| {
            warned.push(w);
        },
    )
    .expect_err("the job fails");
    assert_eq!(e.kind, "api_key");
    assert!(e.message.contains("no API key is saved"), "{}", e.message);
    assert!(!e.retryable);
    assert!(warned.is_empty());
}

fn claude_code(svc: &earshot_runtime::Services, model: &str) {
    set_settings(
        svc,
        &SummarySettingsValue {
            claude_model: model.into(),
            ..value(
                WriterChoice::ClaudeCode,
                ProviderChoice::OpenAiCompatible,
                "",
                "",
            )
        },
    )
    .unwrap();
}

#[test]
fn summaries_claude_code_choice_round_trips_with_its_model() {
    let dir = TempDir::new("summaries-claude-code-settings");
    let svc = services(&dir);
    claude_code(&svc, " opus ");
    let v = settings_view(&svc).unwrap();
    assert_eq!(v.value.writer, WriterChoice::ClaudeCode);
    assert_eq!(v.value.claude_model, "opus");
    assert_eq!(
        v.live_unavailable.as_deref(),
        Some("Summarising during the meeting needs a local summary model.")
    );
    let e = set_settings(
        &svc,
        &SummarySettingsValue {
            claude_model: "--tools".into(),
            ..v.value.clone()
        },
    )
    .unwrap_err();
    assert_eq!(e.kind, "invalid_setting");
    assert!(
        e.message
            .starts_with("This Claude Code model name cannot be used"),
        "{}",
        e.message
    );
    assert!(!e.message.contains("--tools"));
    assert!(!e.message.contains('\u{2014}'));
    assert_eq!(
        settings_view(&svc).unwrap().value.claude_model,
        "opus",
        "nothing stored"
    );
}

#[test]
fn summaries_claude_code_effort_round_trips_as_a_choice() {
    let dir = TempDir::new("summaries-claude-code-effort");
    let svc = services(&dir);
    claude_code(&svc, "opus");
    assert_eq!(settings_view(&svc).unwrap().value.claude_effort, None);
    for (choice, word) in [
        (EffortChoice::Low, "Low"),
        (EffortChoice::Medium, "Medium"),
        (EffortChoice::High, "High"),
        (EffortChoice::ExtraHigh, "ExtraHigh"),
        (EffortChoice::Max, "Max"),
    ] {
        let mut v = settings_view(&svc).unwrap().value;
        v.claude_effort = Some(choice);
        let stored = set_settings(&svc, &v).unwrap();
        assert_eq!(stored.value.claude_effort, Some(choice));
        assert_eq!(settings_view(&svc).unwrap(), stored);
        assert!(
            json(&stored).contains(&format!("\"claude_effort\":\"{word}\"")),
            "{}",
            json(&stored)
        );
        // The model is kept; the effort changes nothing else.
        assert_eq!(stored.value.claude_model, "opus");
    }
    let mut v = settings_view(&svc).unwrap().value;
    v.claude_effort = None;
    assert_eq!(set_settings(&svc, &v).unwrap().value.claude_effort, None);
    // A stored word this version does not know reads as the default, reported.
    svc.store()
        .set_local_value("settings/summary-claude-code-effort", b"ultra")
        .unwrap();
    let v = settings_view(&svc).unwrap();
    assert_eq!(v.value.claude_effort, None);
    assert_eq!(v.unknown.len(), 1, "{:?}", v.unknown);
}

#[test]
fn summaries_claude_code_status_says_when_effort_needs_a_newer_claude_code() {
    let signed_in = || SignIn::SignedIn {
        method: Some("claude.ai".into()),
    };
    let s = claude_code_status(&claude_installed_with(signed_in(), Some(true)));
    assert_eq!(s.effort_note, None);
    // The help could not be read: nothing is claimed.
    let s = claude_code_status(&claude_installed_with(signed_in(), None));
    assert_eq!(s.effort_note, None);
    let s = claude_code_status(&FixedClaudeCode(ClaudeCodeStatus::NotInstalled));
    assert_eq!(s.effort_note, None);
    // An older Claude Code: the page says effort needs a newer one.
    let s = claude_code_status(&claude_installed_with(signed_in(), Some(false)));
    let note = s.effort_note.expect("a note");
    assert!(note.contains("newer Claude Code"), "{note}");
    assert!(!note.contains('\u{2014}'), "{note}");
    assert!(note.ends_with('.'), "{note}");
    // The sign-in state reads as before.
    assert_eq!(s.state, ClaudeCodeState::SignedIn);
}

#[test]
fn summaries_claude_code_status_says_how_to_fix_it() {
    let s = claude_code_status(&FixedClaudeCode(ClaudeCodeStatus::NotInstalled));
    assert_eq!(s.state, ClaudeCodeState::NotInstalled);
    assert_eq!(s.message, "Claude Code is not installed on this computer.");
    assert!(s.fix.is_some());
    let s = claude_code_status(&claude_installed(SignIn::SignedIn {
        method: Some("claude.ai".into()),
    }));
    assert_eq!(s.state, ClaudeCodeState::SignedIn);
    assert_eq!(s.version.as_deref(), Some("2.1.290 (Claude Code)"));
    assert_eq!(s.message, "Claude Code 2.1.290 is installed and signed in.");
    assert_eq!(s.fix, None);
    let s = claude_code_status(&claude_installed(SignIn::NotSignedIn));
    assert_eq!(s.state, ClaudeCodeState::NotSignedIn);
    assert_eq!(
        s.message,
        "Claude Code 2.1.290 is installed but not signed in."
    );
    assert!(s.fix.as_deref().unwrap().contains("/login"));
    let s = claude_code_status(&claude_installed(SignIn::Unknown("timed out".into())));
    assert_eq!(s.state, ClaudeCodeState::Unknown);
    for state in [
        claude_code_status(&FixedClaudeCode(ClaudeCodeStatus::NotInstalled)),
        claude_code_status(&claude_installed(SignIn::NotSignedIn)),
    ] {
        let text = json(&state);
        assert!(!text.contains('\u{2014}'), "{text}");
        assert!(!text.contains("fake"), "no path is shown: {text}");
    }
}

#[test]
fn summaries_claude_code_not_signed_in_skips_the_meetings_summary_with_a_banner() {
    let dir = TempDir::new("summaries-claude-code-meeting");
    let svc = services(&dir);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    claude_code(&svc, "");
    let keys = MemoryVault::default();
    // Signed in: a Claude Code service with the CLI found by the check, no network.
    let s = summary_setup(
        &svc,
        &models,
        &keys,
        &claude_installed(SignIn::SignedIn { method: None }),
        &no_codex(),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    let prepared = s.summary.expect("a Claude Code service");
    assert!(prepared.service.is_claude_code());
    assert_eq!(prepared.service.model(), "claude-code");
    assert!(!s.local_chosen);
    // Not signed in (or not installed): the meeting starts; the summary is skipped with a
    // banner at the start and the reason at the end; never another provider.
    for (probe, banner, end) in [
        (
            claude_installed(SignIn::NotSignedIn),
            "The summary will not run: Claude Code is not signed in. Open Claude Code and \
             sign in, then re-summarise this meeting.",
            "The summary did not run: Claude Code is not signed in. Open Claude Code and \
             sign in, then re-summarise this meeting.",
        ),
        (
            no_claude(),
            "The summary will not run: Claude Code is not installed on this computer. \
             Install Claude Code and sign in, then re-summarise this meeting.",
            "The summary did not run: Claude Code is not installed on this computer. \
             Install Claude Code and sign in, then re-summarise this meeting.",
        ),
    ] {
        let s = summary_setup(
            &svc,
            &models,
            &keys,
            &probe,
            &no_codex(),
            earshot_runtime::host::SummaryPrivacy::Writer,
        )
        .unwrap();
        assert!(s.summary.is_none());
        assert!(matches!(s.no_summary, SkipReason::ClaudeCodeUnavailable(_)));
        assert_eq!(
            claude_code_banner(&s.no_summary),
            Some(MeetingEvent::Warning {
                key: "summary-claude-code".into(),
                kind: WarningKind::Backend,
                message: banner.into(),
            })
        );
        assert_eq!(no_key_banner(&s.no_summary), None);
        assert_eq!(
            summary_result_event(
                &SummaryResult::Skipped {
                    reason: s.no_summary.clone()
                },
                0
            ),
            SummaryEvent::Skipped { reason: end.into() }
        );
    }
    assert_eq!(claude_code_banner(&SkipReason::NotRequested), None);
}

#[test]
fn summaries_resummarise_through_claude_code_not_signed_in_fails_with_the_reason() {
    let dir = TempDir::new("summaries-claude-code-job");
    let svc = services(&dir);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    claude_code(&svc, "opus");
    let meeting = EntityId::from_bytes([1; 16]);
    let e = crate::jobs::summarise_with(
        &svc,
        meeting,
        &models,
        &MemoryVault::default(),
        &claude_installed(SignIn::NotSignedIn),
        &no_codex(),
        &mut |_| {},
    )
    .expect_err("the job fails");
    assert_eq!(e.kind, "claude_code");
    assert_eq!(
        e.message,
        "The summary did not run: Claude Code is not signed in. Open Claude Code and sign in, \
         then re-summarise this meeting."
    );
    assert!(!e.retryable);
    let busy = crate::ipc::IpcError::from(earshot_runtime::claude_code::runtime_error(
        ClaudeCodeError::SignInBusy,
    ));
    assert!(busy.retryable, "a sign-in clash can be retried");
}

#[test]
fn summaries_handover_record_failure_is_a_warning_without_meeting_text() {
    let e = crate::meeting_events::handover_record_warning("store: database is locked");
    assert_eq!(
        e,
        MeetingEvent::Warning {
            key: "summary-handover-record".into(),
            kind: WarningKind::Backend,
            message: "Earshot could not store its record of what it handed to Claude Code or \
                      Codex (store: database is locked)."
                .into(),
        }
    );
}

fn codex(svc: &earshot_runtime::Services, model: &str) {
    set_settings(
        svc,
        &SummarySettingsValue {
            codex_model: model.into(),
            ..value(
                WriterChoice::Codex,
                ProviderChoice::OpenAiCompatible,
                "",
                "",
            )
        },
    )
    .unwrap();
}

#[test]
fn summaries_codex_choice_round_trips_with_its_model() {
    let dir = TempDir::new("summaries-codex-settings");
    let svc = services(&dir);
    codex(&svc, " gpt-5.6-luna ");
    let v = settings_view(&svc).unwrap();
    assert_eq!(v.value.writer, WriterChoice::Codex);
    assert_eq!(v.value.codex_model, "gpt-5.6-luna");
    assert_eq!(
        v.value.claude_model, "",
        "the Claude Code model is its own field"
    );
    assert_eq!(
        v.live_unavailable.as_deref(),
        Some("Summarising during the meeting needs a local summary model.")
    );
    let e = set_settings(
        &svc,
        &SummarySettingsValue {
            codex_model: "--sandbox".into(),
            ..v.value.clone()
        },
    )
    .unwrap_err();
    assert_eq!(e.kind, "invalid_setting");
    assert!(
        e.message
            .starts_with("This Codex model name cannot be used"),
        "{}",
        e.message
    );
    assert!(!e.message.contains("--sandbox"));
    assert!(!e.message.contains('\u{2014}'));
    assert_eq!(
        settings_view(&svc).unwrap().value.codex_model,
        "gpt-5.6-luna",
        "nothing stored"
    );
}

#[test]
fn summaries_codex_status_says_how_to_fix_it() {
    let s = codex_status(&FixedCodex(CodexStatus::NotInstalled));
    assert_eq!(s.state, CodexState::NotInstalled);
    assert_eq!(s.message, "Codex is not installed on this computer.");
    assert!(
        s.fix
            .as_deref()
            .unwrap()
            .contains("npm install -g @openai/codex")
    );
    let s = codex_status(&codex_installed(CodexSignIn::SignedIn {
        method: Some("chatgpt"),
    }));
    assert_eq!(s.state, CodexState::SignedIn);
    assert_eq!(s.version.as_deref(), Some("codex-cli 0.155.1"));
    assert_eq!(
        s.message,
        "Codex 0.155.1 is installed and signed in with ChatGPT."
    );
    assert_eq!(s.fix, None);
    let s = codex_status(&codex_installed(CodexSignIn::SignedIn {
        method: Some("api_key"),
    }));
    assert_eq!(
        s.message,
        "Codex 0.155.1 is installed and signed in with an API key."
    );
    let s = codex_status(&codex_installed(CodexSignIn::SignedIn { method: None }));
    assert_eq!(s.message, "Codex 0.155.1 is installed and signed in.");
    let s = codex_status(&codex_installed(CodexSignIn::NotSignedIn));
    assert_eq!(s.state, CodexState::NotSignedIn);
    assert_eq!(s.message, "Codex 0.155.1 is installed but not signed in.");
    assert!(s.fix.as_deref().unwrap().contains("codex login"));
    let s = codex_status(&codex_installed(CodexSignIn::Unknown("timed out".into())));
    assert_eq!(s.state, CodexState::Unknown);
    for state in [
        codex_status(&FixedCodex(CodexStatus::NotInstalled)),
        codex_status(&codex_installed(CodexSignIn::NotSignedIn)),
    ] {
        let text = json(&state);
        assert!(!text.contains('\u{2014}'), "{text}");
        assert!(!text.contains("fake"), "no path is shown: {text}");
    }
}

#[test]
fn summaries_codex_not_signed_in_skips_the_meetings_summary_with_a_banner() {
    let dir = TempDir::new("summaries-codex-meeting");
    let svc = services(&dir);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    codex(&svc, "");
    let keys = MemoryVault::default();
    // Signed in: a Codex service with the CLI found by the check, no network.
    let s = summary_setup(
        &svc,
        &models,
        &keys,
        &no_claude(),
        &codex_installed(CodexSignIn::SignedIn { method: None }),
        earshot_runtime::host::SummaryPrivacy::Writer,
    )
    .unwrap();
    let prepared = s.summary.expect("a Codex service");
    assert!(prepared.service.is_codex());
    assert_eq!(prepared.service.model(), "codex");
    assert!(!s.local_chosen);
    // Not signed in (or not installed): the meeting starts; the summary is skipped with a
    // banner at the start and the reason at the end; never another provider, even with a
    // signed-in Claude Code.
    for (probe, banner, end) in [
        (
            codex_installed(CodexSignIn::NotSignedIn),
            "The summary will not run: Codex is not signed in. Sign in to Codex (run codex \
             login), then re-summarise this meeting.",
            "The summary did not run: Codex is not signed in. Sign in to Codex (run codex \
             login), then re-summarise this meeting.",
        ),
        (
            no_codex(),
            "The summary will not run: Codex is not installed on this computer. Install \
             Codex and sign in, then re-summarise this meeting.",
            "The summary did not run: Codex is not installed on this computer. Install \
             Codex and sign in, then re-summarise this meeting.",
        ),
    ] {
        let s = summary_setup(
            &svc,
            &models,
            &keys,
            &claude_installed(SignIn::SignedIn { method: None }),
            &probe,
            earshot_runtime::host::SummaryPrivacy::Writer,
        )
        .unwrap();
        assert!(s.summary.is_none(), "never Claude Code instead");
        assert!(matches!(s.no_summary, SkipReason::CodexUnavailable(_)));
        assert_eq!(
            codex_banner(&s.no_summary),
            Some(MeetingEvent::Warning {
                key: "summary-codex".into(),
                kind: WarningKind::Backend,
                message: banner.into(),
            })
        );
        assert_eq!(claude_code_banner(&s.no_summary), None);
        assert_eq!(no_key_banner(&s.no_summary), None);
        assert_eq!(
            summary_result_event(
                &SummaryResult::Skipped {
                    reason: s.no_summary.clone()
                },
                0
            ),
            SummaryEvent::Skipped { reason: end.into() }
        );
    }
    assert_eq!(codex_banner(&SkipReason::NotRequested), None);
}

#[test]
fn summaries_resummarise_through_codex_not_signed_in_fails_with_the_reason() {
    let dir = TempDir::new("summaries-codex-job");
    let svc = services(&dir);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    codex(&svc, "gpt-5.6-luna");
    let meeting = EntityId::from_bytes([1; 16]);
    let e = crate::jobs::summarise_with(
        &svc,
        meeting,
        &models,
        &MemoryVault::default(),
        &no_claude(),
        &codex_installed(CodexSignIn::NotSignedIn),
        &mut |_| {},
    )
    .expect_err("the job fails");
    assert_eq!(e.kind, "codex");
    assert_eq!(
        e.message,
        "The summary did not run: Codex is not signed in. Sign in to Codex (run codex login), \
         then re-summarise this meeting."
    );
    assert!(!e.retryable);
    // A workspace that does not allow Codex says what to do, without Codex's own text.
    let denied = crate::ipc::IpcError::from(earshot_runtime::codex::runtime_error(
        CodexError::AccessDenied,
    ));
    assert_eq!(denied.kind, "codex");
    assert_eq!(
        denied.message,
        "The summary did not run: your ChatGPT workspace does not allow Codex; ask its \
         administrator for access. Choose another way to write summaries on the Summaries \
         page, or ask your ChatGPT workspace administrator to allow Codex."
    );
    assert!(!denied.retryable);
    // A version that refuses an option, and a tool that ran: fixed copy, no Codex text.
    let version = crate::ipc::IpcError::from(earshot_runtime::codex::runtime_error(
        CodexError::UnsupportedVersion,
    ));
    assert_eq!(
        version.message,
        "The summary did not run: this Codex version and Earshot do not agree on an option. \
         Check that Earshot supports your Codex version."
    );
    let tool =
        crate::ipc::IpcError::from(earshot_runtime::codex::runtime_error(CodexError::ToolRan));
    assert!(
        tool.message
            .starts_with("The summary did not run: Codex used a tool"),
        "{}",
        tool.message
    );
    assert!(!tool.message.contains('\u{2014}'));
    let timed_out = crate::ipc::IpcError::from(earshot_runtime::codex::runtime_error(
        CodexError::TimedOut(std::time::Duration::from_secs(600)),
    ));
    assert!(timed_out.retryable, "a timeout can be retried");
}
