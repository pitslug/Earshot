//! Tests of the Summaries page settings (T7.4g): stored values, validation and the host's
//! summary setup from them. Every name contains `summary_settings`.

use std::cell::Cell;

use earshot_hardware::{Cpu, Hardware};

use super::*;
use crate::backends::Accelerator;
use crate::host;
use crate::testutil::TempDir;

const CPU: &[Accelerator] = &[Accelerator::Cpu];
const SECRET: &str = "sk-ant-test-secret-0123456789";

fn store(dir: &TempDir) -> Store {
    Store::open(&dir.0.join("earshot.db")).unwrap()
}

fn machine() -> Hardware {
    Hardware {
        cpu: Cpu {
            name: Some("Test CPU".into()),
            physical_cores: Some(8),
            logical_cores: Some(8),
        },
        total_memory: Some(16 << 30),
        ..Hardware::default()
    }
}

fn endpoint(provider: EndpointProvider, url: &str, model: &str) -> SummarySettings {
    SummarySettings {
        writer: SummaryWriter::OwnEndpoint,
        provider,
        url: url.into(),
        model: model.into(),
        live: false,
        claude_model: String::new(),
        claude_effort: None,
        codex_model: String::new(),
    }
}

/// A key store that answers `answer` and counts its reads.
struct FakeKeys {
    answer: Result<Option<&'static str>, ApiKeyError>,
    reads: Cell<u32>,
}

impl FakeKeys {
    fn new(answer: Result<Option<&'static str>, ApiKeyError>) -> Self {
        Self {
            answer,
            reads: Cell::new(0),
        }
    }
}

impl ApiKeyStore for FakeKeys {
    fn anthropic_key(&self) -> Result<Option<ApiKey>, ApiKeyError> {
        self.reads.set(self.reads.get() + 1);
        self.answer.clone().map(|k| k.and_then(ApiKey::new))
    }
}

fn stored(db: &Store, key: &str) -> Option<String> {
    db.local_value(key)
        .unwrap()
        .map(|b| String::from_utf8(b).unwrap())
}

#[test]
fn summary_settings_default_to_this_computer_with_live_summaries_off() {
    let dir = TempDir::new("summary-settings-default");
    let db = store(&dir);
    let (s, unknown) = summary_settings(&db).unwrap();
    assert_eq!(s, SummarySettings::default());
    assert_eq!(s.writer, SummaryWriter::OnThisComputer);
    assert!(!s.live);
    assert!(unknown.is_empty());
}

#[test]
fn summary_settings_round_trip_in_one_write() {
    let dir = TempDir::new("summary-settings-round-trip");
    let mut db = store(&dir);
    let s = SummarySettings {
        url: "  https://api.anthropic.com  ".into(),
        model: " claude-sonnet-4-5 ".into(),
        live: false,
        ..endpoint(EndpointProvider::Anthropic, "", "")
    };
    let saved = set_summary_settings(&mut db, &s).unwrap();
    assert_eq!(saved.url, "https://api.anthropic.com", "trimmed");
    assert_eq!(saved.model, "claude-sonnet-4-5");
    let (read, unknown) = summary_settings(&db).unwrap();
    assert_eq!(read, saved);
    assert!(unknown.is_empty(), "{unknown:?}");
    assert_eq!(stored(&db, SUMMARY_WRITER_KEY).as_deref(), Some("endpoint"));
    assert_eq!(
        stored(&db, SUMMARY_PROVIDER_KEY).as_deref(),
        Some("anthropic")
    );
    // Each writer and provider round-trips; the endpoint is kept with another writer.
    for writer in SummaryWriter::ALL {
        for provider in EndpointProvider::ALL {
            let s = SummarySettings {
                writer,
                provider,
                live: writer == SummaryWriter::OnThisComputer,
                ..saved.clone()
            };
            assert_eq!(set_summary_settings(&mut db, &s).unwrap(), s);
            assert_eq!(
                summary_settings(&db).unwrap().0,
                s,
                "{writer:?} {provider:?}"
            );
        }
    }
}

#[test]
fn summary_settings_live_switch_is_the_live_summaries_key() {
    let dir = TempDir::new("summary-settings-live");
    let mut db = store(&dir);
    let on = SummarySettings {
        live: true,
        ..SummarySettings::default()
    };
    set_summary_settings(&mut db, &on).unwrap();
    assert_eq!(stored(&db, LIVE_SUMMARIES_KEY).as_deref(), Some("on"));
    assert!(host::choose_live_summaries(&mut db, None, false).unwrap().0);
    // The CLI's writer of the same key is read back here.
    crate::live_summaries::set_live_summaries(&mut db, false).unwrap();
    assert!(!summary_settings(&db).unwrap().0.live);
}

#[test]
fn summary_settings_unknown_stored_values_are_reported_and_the_default_used() {
    let dir = TempDir::new("summary-settings-unknown");
    let mut db = store(&dir);
    db.set_local_value(SUMMARY_WRITER_KEY, b"cloud").unwrap();
    db.set_local_value(SUMMARY_PROVIDER_KEY, b"gemini").unwrap();
    db.set_local_value(LIVE_SUMMARIES_KEY, b"sometimes")
        .unwrap();
    let (s, unknown) = summary_settings(&db).unwrap();
    assert_eq!(s, SummarySettings::default());
    let keys: Vec<&str> = unknown.iter().map(|u| u.key).collect();
    assert_eq!(
        keys,
        [SUMMARY_WRITER_KEY, SUMMARY_PROVIDER_KEY, LIVE_SUMMARIES_KEY]
    );
    assert!(
        unknown[0].to_string().contains("\"cloud\""),
        "{}",
        unknown[0]
    );
}

#[test]
fn summary_settings_a_refused_stored_endpoint_is_reported_without_its_address() {
    let dir = TempDir::new("summary-settings-bad-stored");
    let mut db = store(&dir);
    // Written by hand (or by a version with other rules): plain http to a public host.
    db.set_local_value(SUMMARY_WRITER_KEY, b"endpoint").unwrap();
    db.set_local_value(SUMMARY_ENDPOINT_KEY, b"http://llm.example.com/v1?token=abc")
        .unwrap();
    db.set_local_value(SUMMARY_ENDPOINT_MODEL_KEY, b"m")
        .unwrap();
    let (s, unknown) = summary_settings(&db).unwrap();
    assert_eq!(s.url, "", "a refused address is not used");
    assert_eq!(
        s.writer,
        SummaryWriter::OnThisComputer,
        "the default is used"
    );
    let text: Vec<String> = unknown.iter().map(ToString::to_string).collect();
    assert_eq!(unknown.len(), 2, "{text:?}");
    assert_eq!(unknown[0].key, SUMMARY_ENDPOINT_KEY);
    assert_eq!(unknown[1].key, SUMMARY_WRITER_KEY);
    for t in &text {
        assert!(!t.contains("example.com") && !t.contains("token"), "{t}");
    }
}

#[test]
fn summary_settings_validation_refuses_unusable_endpoints_and_stores_nothing() {
    let dir = TempDir::new("summary-settings-validate");
    let mut db = store(&dir);
    let openai = EndpointProvider::OpenAiCompatible;
    let cases: &[(SummarySettings, &str)] = &[
        (endpoint(openai, "", "m"), "enter the endpoint's address"),
        (
            endpoint(openai, "http://127.0.0.1:8080/v1", "  "),
            "enter the model name",
        ),
        (endpoint(openai, "not a url", "m"), "not a valid URL"),
        (endpoint(openai, "ftp://10.0.0.2/v1", "m"), "scheme `ftp`"),
        (
            endpoint(openai, "https://user:pw@llm.example.com/v1", "m"),
            "user name or password",
        ),
        (
            endpoint(openai, "http://llm.example.com/v1", "m"),
            "plain http is only allowed to this computer or the local network",
        ),
        (
            endpoint(EndpointProvider::Anthropic, "http://8.8.8.8", "m"),
            "plain http",
        ),
        (
            endpoint(openai, "https://llm.example.com/v1?key=1", "m"),
            "query",
        ),
        (
            endpoint(openai, "https://llm.example.com/v1#x", "m"),
            "fragment",
        ),
        (
            endpoint(openai, "https://llm.example.com/v1", "a\nb"),
            "line breaks",
        ),
        (
            endpoint(openai, "https://llm.example.com/v1", &"m".repeat(201)),
            "at most 200",
        ),
        // A path the client appends itself would be doubled (`/v1/v1/messages`).
        (
            endpoint(
                EndpointProvider::Anthropic,
                "https://api.anthropic.com/v1",
                "m",
            ),
            "use the address without a path, for example https://api.anthropic.com",
        ),
        (
            endpoint(
                EndpointProvider::Anthropic,
                "https://api.anthropic.com/v1/messages",
                "m",
            ),
            "without a path",
        ),
        (
            endpoint(openai, "http://127.0.0.1:8080/v1/chat/completions/", "m"),
            "use the address without /chat/completions, for example http://127.0.0.1:8080/v1",
        ),
        // A filled-in endpoint is checked whatever the writer.
        (
            SummarySettings {
                writer: SummaryWriter::OnThisComputer,
                ..endpoint(openai, "http://llm.example.com/v1", "m")
            },
            "plain http",
        ),
    ];
    for (s, want) in cases {
        let e = set_summary_settings(&mut db, s).unwrap_err();
        let text = e.to_string();
        assert!(
            matches!(
                e,
                SummarySettingsError::MissingUrl
                    | SummarySettingsError::MissingModel
                    | SummarySettingsError::BadUrl(_)
                    | SummarySettingsError::BadModel
            ),
            "{text}"
        );
        assert!(text.contains(want), "{s:?}: {text}");
        assert!(
            !text.contains("user:pw") && !text.contains("key=1"),
            "{text}"
        );
        assert_eq!(stored(&db, SUMMARY_WRITER_KEY), None, "nothing stored");
        assert_eq!(stored(&db, SUMMARY_ENDPOINT_KEY), None, "nothing stored");
    }
    // Accepted: https anywhere, http to loopback and the private LAN.
    for url in [
        "https://api.anthropic.com",
        "https://llm.example.com/v1",
        "http://127.0.0.1:8080/v1",
        "http://localhost:1234/v1",
        "http://192.168.1.20:8080/v1",
        "http://10.0.0.2/v1",
        "http://[::1]:8080/v1",
    ] {
        let s = endpoint(openai, url, "m");
        assert_eq!(set_summary_settings(&mut db, &s).unwrap(), s, "{url}");
    }
    // Anthropic takes the API root, with or without its trailing slash.
    for url in ["https://api.anthropic.com", "https://api.anthropic.com/"] {
        let s = endpoint(EndpointProvider::Anthropic, url, "m");
        assert_eq!(set_summary_settings(&mut db, &s).unwrap(), s, "{url}");
    }
    // No endpoint at all is fine for the other writers.
    for writer in [SummaryWriter::OnThisComputer, SummaryWriter::NoSummary] {
        let s = SummarySettings {
            writer,
            ..SummarySettings::default()
        };
        assert_eq!(set_summary_settings(&mut db, &s).unwrap(), s);
    }
}

#[test]
fn summary_settings_endpoint_host_and_locality_follow_the_net_rule() {
    let s = |url: &str| endpoint(EndpointProvider::OpenAiCompatible, url, "m");
    assert_eq!(
        s("https://api.anthropic.com").endpoint_host().as_deref(),
        Some("api.anthropic.com")
    );
    assert!(!s("https://api.anthropic.com").endpoint_is_local());
    assert!(s("http://127.0.0.1:8080/v1").endpoint_is_local());
    assert!(s("http://192.168.1.20/v1").endpoint_is_local());
    assert_eq!(
        s("http://[::1]:8080").endpoint_host().as_deref(),
        Some("::1")
    );
    assert!(s("http://[::1]:8080").endpoint_is_local());
    // A name other than localhost is never local (what it resolves to is not known).
    assert!(!s("https://llm.lan/v1").endpoint_is_local());
    assert_eq!(s("").endpoint_host(), None);
    assert!(!s("").endpoint_is_local());
}

#[test]
fn summary_settings_api_key_errors_never_carry_a_key() {
    for e in [
        ApiKeyError::Missing,
        ApiKeyError::Unsupported,
        ApiKeyError::Invalid("it holds spaces"),
        ApiKeyError::Store("Element not found. (0x80070490)".into()),
    ] {
        let text = RuntimeError::ApiKey(e.clone()).to_string();
        assert!(!text.contains(SECRET), "{text}");
        assert!(!format!("{e:?}").contains(SECRET));
    }
    assert!(!format!("{:?}", ApiKey::new(SECRET).unwrap()).contains(SECRET));
}

#[test]
fn summary_settings_host_no_summary_skips_without_reading_a_key() {
    let dir = TempDir::new("summary-settings-host-none");
    let mut db = store(&dir);
    set_summary_settings(
        &mut db,
        &SummarySettings {
            writer: SummaryWriter::NoSummary,
            ..endpoint(
                EndpointProvider::Anthropic,
                "https://api.anthropic.com",
                "m",
            )
        },
    )
    .unwrap();
    let keys = FakeKeys::new(Ok(Some(SECRET)));
    let s = host::summary_from_settings(
        &db,
        CPU,
        &machine(),
        &keys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(s.request.skip && s.request.remote.is_none());
    assert_eq!(s.writer, SummaryWriter::NoSummary);
    assert_eq!(keys.reads.get(), 0);
    let models = crate::backends::ModelDir::new(dir.0.join("models"));
    let plan = host::summary_plan(&s.request, None, false, &models).unwrap();
    assert!(matches!(plan, host::SummaryPlan::Skip));
    assert_eq!(
        host::skip_reason(&plan),
        Some(host::SkipReason::NotRequested)
    );
}

#[test]
fn summary_settings_host_on_this_computer_is_the_local_model() {
    let dir = TempDir::new("summary-settings-host-local");
    let mut db = store(&dir);
    set_summary_settings(
        &mut db,
        &SummarySettings {
            live: true,
            ..SummarySettings::default()
        },
    )
    .unwrap();
    let keys = FakeKeys::new(Ok(Some(SECRET)));
    let s = host::summary_from_settings(
        &db,
        CPU,
        &machine(),
        &keys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(!s.request.skip && s.request.remote.is_none());
    assert_eq!(s.request.local_model, None, "the memory rule picks");
    assert_eq!(s.writer, SummaryWriter::OnThisComputer);
    assert!(s.live);
    assert_eq!(keys.reads.get(), 0);
}

#[test]
fn summary_settings_host_openai_endpoint_allowlists_exactly_the_endpoint() {
    let dir = TempDir::new("summary-settings-host-openai");
    let mut db = store(&dir);
    set_summary_settings(
        &mut db,
        &SummarySettings {
            live: true,
            ..endpoint(
                EndpointProvider::OpenAiCompatible,
                "http://192.168.1.20:8080/v1",
                "qwen",
            )
        },
    )
    .unwrap();
    let keys = FakeKeys::new(Ok(Some(SECRET)));
    let s = host::summary_from_settings(
        &db,
        CPU,
        &machine(),
        &keys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert_eq!(
        keys.reads.get(),
        0,
        "an OpenAI-compatible endpoint is sent no key"
    );
    let config = s.request.remote.as_ref().expect("remote");
    assert_eq!(config.provider, earshot_llm::Provider::OpenAi);
    assert_eq!(config.endpoint, "http://192.168.1.20:8080/v1");
    assert_eq!(config.model, "qwen");
    assert!(config.api_key.is_none());
    assert!(
        s.live,
        "reported; live_summaries_for refuses it without a local model"
    );
    let allow = crate::summary::summary_allowlist(config).unwrap();
    assert_eq!(
        allow.entries().collect::<Vec<_>>(),
        [("192.168.1.20", 8080)]
    );
    assert_eq!(allow.redirect_entries().count(), 0);
    let models = crate::backends::ModelDir::new(dir.0.join("models"));
    let plan = host::summary_plan(&s.request, None, false, &models).unwrap();
    assert!(matches!(plan, host::SummaryPlan::Remote(_)));
    let prepared = host::prepare_summary(
        &plan,
        &models,
        std::sync::Arc::new(earshot_core::identity::ManualClock::new(0)),
        &[],
    )
    .unwrap();
    let prepared = prepared.expect("a service");
    assert!(prepared.local.is_none());
    assert_eq!(
        host::live_summaries_for(Some(&prepared), CPU).err(),
        Some(host::LIVE_SUMMARIES_NEED_LOCAL)
    );
}

#[test]
fn summary_settings_host_anthropic_reads_the_key_at_use_time() {
    let dir = TempDir::new("summary-settings-host-anthropic");
    let mut db = store(&dir);
    set_summary_settings(
        &mut db,
        &endpoint(
            EndpointProvider::Anthropic,
            "https://api.anthropic.com",
            "claude-sonnet-4-5",
        ),
    )
    .unwrap();
    // With a key: the remote config carries it, the allowlist is the endpoint alone.
    let keys = FakeKeys::new(Ok(Some(SECRET)));
    let s = host::summary_from_settings(
        &db,
        CPU,
        &machine(),
        &keys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert_eq!(keys.reads.get(), 1);
    let config = s.request.remote.as_ref().expect("remote");
    assert_eq!(config.provider, earshot_llm::Provider::Anthropic);
    assert_eq!(config.api_key, ApiKey::new(SECRET));
    assert!(
        !format!("{s:?}").contains(SECRET),
        "Debug never shows the key"
    );
    let allow = crate::summary::summary_allowlist(config).unwrap();
    assert_eq!(
        allow.entries().collect::<Vec<_>>(),
        [("api.anthropic.com", 443)]
    );
    // Without one: the meeting still runs; the summary is skipped with
    // the reason, never a local model or another provider. The key store is read once.
    let models = crate::backends::ModelDir::new(dir.0.join("models"));
    let none = FakeKeys::new(Ok(None));
    let s = host::summary_from_settings(
        &db,
        CPU,
        &machine(),
        &none,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert_eq!(none.reads.get(), 1);
    assert!(s.request.remote.is_none() && s.request.local_model.is_none());
    assert_eq!(s.request.no_key, Some(ApiKeyError::Missing));
    let plan = host::summary_plan(&s.request, None, false, &models).unwrap();
    assert!(
        matches!(plan, host::SummaryPlan::NoApiKey(ApiKeyError::Missing)),
        "{plan:?}"
    );
    assert_eq!(
        host::skip_reason(&plan),
        Some(host::SkipReason::NoApiKey(ApiKeyError::Missing))
    );
    let clock = std::sync::Arc::new(earshot_core::identity::ManualClock::new(0));
    assert!(
        host::prepare_summary(&plan, &models, clock.clone(), &[])
            .unwrap()
            .is_none(),
        "no service, nothing sent"
    );
    // A store failure is the reason as it is.
    let broken = FakeKeys::new(Err(ApiKeyError::Store("access denied".into())));
    let s = host::summary_from_settings(
        &db,
        CPU,
        &machine(),
        &broken,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert_eq!(broken.reads.get(), 1);
    assert_eq!(
        s.request.no_key,
        Some(ApiKeyError::Store("access denied".into()))
    );
    let plan = host::summary_plan(&s.request, None, false, &models).unwrap();
    assert_eq!(
        host::skip_reason(&plan),
        Some(host::SkipReason::NoApiKey(ApiKeyError::Store(
            "access denied".into()
        )))
    );
}

#[test]
fn summary_settings_host_notes_are_the_unknown_values_and_local_placement_only() {
    let dir = TempDir::new("summary-settings-host-notes");
    let mut db = store(&dir);
    // A "where it runs" note that only matters on this computer.
    db.set_local_value(crate::model_settings::SUMMARY_PART_KEY, b"gpu")
        .unwrap();
    let s = host::summary_from_settings(
        &db,
        CPU,
        &machine(),
        &NoApiKeys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert_eq!(s.notes.len(), 1, "{:?}", s.notes);
    assert!(s.placement.note.is_some());
    set_summary_settings(
        &mut db,
        &SummarySettings {
            writer: SummaryWriter::NoSummary,
            ..SummarySettings::default()
        },
    )
    .unwrap();
    db.set_local_value(SUMMARY_PROVIDER_KEY, b"gemini").unwrap();
    let s = host::summary_from_settings(
        &db,
        CPU,
        &machine(),
        &NoApiKeys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(s.placement.note.is_none());
    assert_eq!(s.notes.len(), 1, "{:?}", s.notes);
    assert!(s.notes[0].contains(SUMMARY_PROVIDER_KEY), "{:?}", s.notes);
}

#[test]
fn summary_settings_claude_code_model_round_trips_and_is_checked() {
    let dir = TempDir::new("summary-settings-claude-code");
    let mut db = store(&dir);
    let s = SummarySettings {
        writer: SummaryWriter::ClaudeCode,
        claude_model: "  opus ".into(),
        ..SummarySettings::default()
    };
    let saved = set_summary_settings(&mut db, &s).unwrap();
    assert_eq!(saved.claude_model, "opus", "trimmed");
    assert_eq!(
        stored(&db, SUMMARY_WRITER_KEY).as_deref(),
        Some("claude-code")
    );
    assert_eq!(
        stored(&db, SUMMARY_CLAUDE_CODE_MODEL_KEY).as_deref(),
        Some("opus")
    );
    assert_eq!(summary_settings(&db).unwrap(), (saved.clone(), Vec::new()));
    // Empty: Claude Code's own default; the endpoint fields are not needed.
    let default_model = SummarySettings {
        claude_model: String::new(),
        ..saved.clone()
    };
    assert_eq!(
        set_summary_settings(&mut db, &default_model).unwrap(),
        default_model
    );
    // A name that would read as an option or holds a space is refused; nothing is stored.
    for bad in ["--tools", "opus sonnet", "op\"us"] {
        let e = set_summary_settings(
            &mut db,
            &SummarySettings {
                claude_model: bad.into(),
                ..saved.clone()
            },
        )
        .unwrap_err();
        assert!(
            matches!(e, SummarySettingsError::BadClaudeModel(_)),
            "{bad}"
        );
        assert!(
            !e.to_string().contains(bad),
            "the refusal never quotes the name"
        );
    }
    assert_eq!(summary_settings(&db).unwrap().0, default_model);
    // A stored name this version refuses reads as the default, reported.
    db.set_local_value(SUMMARY_CLAUDE_CODE_MODEL_KEY, b"-p")
        .unwrap();
    let (read, unknown) = summary_settings(&db).unwrap();
    assert_eq!(read.claude_model, "");
    assert_eq!(read.writer, SummaryWriter::ClaudeCode);
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].key, SUMMARY_CLAUDE_CODE_MODEL_KEY);
}

#[test]
fn summary_settings_claude_code_effort_round_trips_and_an_unknown_one_reads_as_the_default() {
    use earshot_llm::claude_code::Effort;
    let dir = TempDir::new("summary-settings-claude-effort");
    let mut db = store(&dir);
    // Absent: Claude Code's own default.
    assert_eq!(summary_settings(&db).unwrap().0.claude_effort, None);
    for (effort, word) in [
        (Effort::Low, "low"),
        (Effort::Medium, "medium"),
        (Effort::High, "high"),
        (Effort::ExtraHigh, "xhigh"),
        (Effort::Max, "max"),
    ] {
        let s = SummarySettings {
            writer: SummaryWriter::ClaudeCode,
            claude_effort: Some(effort),
            ..SummarySettings::default()
        };
        let saved = set_summary_settings(&mut db, &s).unwrap();
        assert_eq!(saved.claude_effort, Some(effort));
        assert_eq!(
            stored(&db, SUMMARY_CLAUDE_CODE_EFFORT_KEY).as_deref(),
            Some(word)
        );
        assert_eq!(summary_settings(&db).unwrap(), (saved, Vec::new()));
    }
    // The default is stored as an empty value, read back as the default with nothing reported.
    let default = SummarySettings {
        writer: SummaryWriter::ClaudeCode,
        ..SummarySettings::default()
    };
    set_summary_settings(&mut db, &default).unwrap();
    assert_eq!(
        stored(&db, SUMMARY_CLAUDE_CODE_EFFORT_KEY).as_deref(),
        Some("")
    );
    assert_eq!(summary_settings(&db).unwrap(), (default, Vec::new()));
    // A stored word this version does not know reads as the default, reported by its key.
    for unknown_word in ["ultra", "High", "--model", "x high"] {
        db.set_local_value(SUMMARY_CLAUDE_CODE_EFFORT_KEY, unknown_word.as_bytes())
            .unwrap();
        let (read, unknown) = summary_settings(&db).unwrap();
        assert_eq!(read.claude_effort, None, "{unknown_word}");
        assert_eq!(read.writer, SummaryWriter::ClaudeCode);
        assert_eq!(unknown.len(), 1, "{unknown_word}");
        assert_eq!(unknown[0].key, SUMMARY_CLAUDE_CODE_EFFORT_KEY);
    }
}

/// A Claude Code probe with a fixed answer that counts its calls.
struct CountingProbe {
    status: crate::claude_code::ClaudeCodeStatus,
    calls: Cell<u32>,
}

impl crate::claude_code::ClaudeCodeProbe for CountingProbe {
    fn status(&self) -> crate::claude_code::ClaudeCodeStatus {
        self.calls.set(self.calls.get() + 1);
        self.status.clone()
    }
}

fn probe(status: crate::claude_code::ClaudeCodeStatus) -> CountingProbe {
    CountingProbe {
        status,
        calls: Cell::new(0),
    }
}

fn installed(sign_in: earshot_llm::SignIn) -> crate::claude_code::ClaudeCodeStatus {
    crate::claude_code::ClaudeCodeStatus::Installed {
        cli: earshot_llm::ClaudeCli {
            program: "C:\\fake\\claude.exe".into(),
            leading_args: Vec::new(),
        },
        version: Some("2.1.290 (Claude Code)".into()),
        sign_in,
        accepts_effort: None,
    }
}

#[test]
fn summary_settings_host_claude_code_is_checked_and_never_falls_back() {
    use crate::claude_code::ClaudeCodeStatus;
    use earshot_llm::{ClaudeCodeError, SignIn};
    let dir = TempDir::new("summary-settings-host-claude-code");
    let mut db = store(&dir);
    set_summary_settings(
        &mut db,
        &SummarySettings {
            writer: SummaryWriter::ClaudeCode,
            claude_model: "sonnet".into(),
            claude_effort: Some(earshot_llm::claude_code::Effort::High),
            // A filled-in endpoint is kept but not used.
            ..endpoint(
                EndpointProvider::Anthropic,
                "https://api.anthropic.com",
                "m",
            )
        },
    )
    .unwrap();
    let keys = FakeKeys::new(Ok(Some(SECRET)));
    let s = host::summary_from_settings(
        &db,
        CPU,
        &machine(),
        &keys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert_eq!(keys.reads.get(), 0, "no API key is read for Claude Code");
    assert!(s.request.remote.is_none() && s.request.local_model.is_none());
    assert!(!s.request.skip && s.request.no_key.is_none());
    let config = s.request.claude_code.clone().expect("claude code");
    assert_eq!(config.model.as_deref(), Some("sonnet"));
    assert_eq!(
        config.effort,
        Some(earshot_llm::claude_code::Effort::High),
        "the effort goes to the run, which passes it only when the CLI lists it"
    );
    let models = crate::backends::ModelDir::new(dir.0.join("models"));
    let plan = || host::summary_plan(&s.request, None, false, &models).unwrap();
    assert!(matches!(plan(), host::SummaryPlan::ClaudeCode(_)));
    assert_eq!(host::skip_reason(&plan()), None);

    // Not installed or not signed in: no summary, with the reason (a meeting still records).
    for (status, problem) in [
        (
            ClaudeCodeStatus::NotInstalled,
            ClaudeCodeError::NotInstalled,
        ),
        (installed(SignIn::NotSignedIn), ClaudeCodeError::NotSignedIn),
    ] {
        let p = probe(status);
        let checked = host::check_claude_code(plan(), &p);
        assert_eq!(p.calls.get(), 1);
        assert!(
            matches!(&checked, host::SummaryPlan::ClaudeCodeUnavailable(e) if *e == problem),
            "{checked:?}"
        );
        assert_eq!(
            host::skip_reason(&checked),
            Some(host::SkipReason::ClaudeCodeUnavailable(problem))
        );
        let clock = std::sync::Arc::new(earshot_core::identity::ManualClock::new(0));
        assert!(
            host::prepare_summary(&checked, &models, clock, &[])
                .unwrap()
                .is_none(),
            "no service: nothing is handed over"
        );
    }

    // Signed in (or a state the check could not read): the found CLI runs the summary.
    for sign_in in [
        SignIn::SignedIn {
            method: Some("claude.ai".into()),
        },
        SignIn::Unknown("timed out".into()),
    ] {
        let p = probe(installed(sign_in));
        let checked = host::check_claude_code(plan(), &p);
        let host::SummaryPlan::ClaudeCode(c) = &checked else {
            panic!("{checked:?}");
        };
        assert_eq!(
            c.cli.as_ref().map(|c| c.program.clone()),
            Some("C:\\fake\\claude.exe".into())
        );
        let clock = std::sync::Arc::new(earshot_core::identity::ManualClock::new(0));
        let prepared = host::prepare_summary(&checked, &models, clock, &[])
            .unwrap()
            .expect("a service");
        assert!(prepared.service.is_claude_code() && prepared.local.is_none());
        assert_eq!(prepared.service.model(), "claude-code/sonnet");
        assert!(prepared.service.audit_log().is_empty());
        assert_eq!(
            host::live_summaries_for(Some(&prepared), CPU).err(),
            Some(host::LIVE_SUMMARIES_NEED_LOCAL),
            "no live summaries through Claude Code"
        );
    }

    // Any other plan is not checked.
    let p = probe(ClaudeCodeStatus::NotInstalled);
    assert!(matches!(
        host::check_claude_code(host::SummaryPlan::Skip, &p),
        host::SummaryPlan::Skip
    ));
    assert_eq!(p.calls.get(), 0);
}

#[test]
fn summary_settings_codex_model_round_trips_and_is_checked() {
    let dir = TempDir::new("summary-settings-codex");
    let mut db = store(&dir);
    let s = SummarySettings {
        writer: SummaryWriter::Codex,
        codex_model: "  gpt-5.6-luna ".into(),
        ..SummarySettings::default()
    };
    let saved = set_summary_settings(&mut db, &s).unwrap();
    assert_eq!(saved.codex_model, "gpt-5.6-luna", "trimmed");
    assert_eq!(stored(&db, SUMMARY_WRITER_KEY).as_deref(), Some("codex"));
    assert_eq!(
        stored(&db, SUMMARY_CODEX_MODEL_KEY).as_deref(),
        Some("gpt-5.6-luna")
    );
    assert_eq!(summary_settings(&db).unwrap(), (saved.clone(), Vec::new()));
    assert_eq!(SummaryWriter::parse("codex"), Some(SummaryWriter::Codex));
    // Empty: Codex's own default; the endpoint fields are not needed.
    let default_model = SummarySettings {
        codex_model: String::new(),
        ..saved.clone()
    };
    assert_eq!(
        set_summary_settings(&mut db, &default_model).unwrap(),
        default_model
    );
    // A name that would read as an option, holds a space, a quote or an `=` is refused.
    for bad in ["-m", "gpt 5", "gp\"t", "a=b", "--sandbox"] {
        let e = set_summary_settings(
            &mut db,
            &SummarySettings {
                codex_model: bad.into(),
                ..saved.clone()
            },
        )
        .unwrap_err();
        assert!(matches!(e, SummarySettingsError::BadCodexModel(_)), "{bad}");
        assert!(
            !e.to_string().contains(bad),
            "the refusal never quotes the name"
        );
    }
    assert_eq!(summary_settings(&db).unwrap().0, default_model);
    // A stored name this version refuses reads as the default, reported.
    db.set_local_value(SUMMARY_CODEX_MODEL_KEY, b"-s").unwrap();
    let (read, unknown) = summary_settings(&db).unwrap();
    assert_eq!(read.codex_model, "");
    assert_eq!(read.writer, SummaryWriter::Codex);
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].key, SUMMARY_CODEX_MODEL_KEY);
}

/// A Codex probe with a fixed answer that counts its calls.
struct CountingCodexProbe {
    status: crate::codex::CodexStatus,
    calls: Cell<u32>,
}

impl crate::codex::CodexProbe for CountingCodexProbe {
    fn status(&self) -> crate::codex::CodexStatus {
        self.calls.set(self.calls.get() + 1);
        self.status.clone()
    }
}

fn codex_probe(status: crate::codex::CodexStatus) -> CountingCodexProbe {
    CountingCodexProbe {
        status,
        calls: Cell::new(0),
    }
}

fn codex_installed(sign_in: earshot_llm::CodexSignIn) -> crate::codex::CodexStatus {
    crate::codex::CodexStatus::Installed {
        cli: earshot_llm::CodexCli::at("C:\\fake\\codex.exe".into()),
        version: Some("codex-cli 0.155.1".into()),
        sign_in,
    }
}

#[test]
fn summary_settings_host_codex_is_checked_and_never_falls_back() {
    use crate::codex::CodexStatus;
    use earshot_llm::{CodexError, CodexSignIn};
    let dir = TempDir::new("summary-settings-host-codex");
    let mut db = store(&dir);
    set_summary_settings(
        &mut db,
        &SummarySettings {
            writer: SummaryWriter::Codex,
            codex_model: "gpt-5.6-luna".into(),
            claude_model: "opus".into(),
            // A filled-in endpoint is kept but not used.
            ..endpoint(
                EndpointProvider::Anthropic,
                "https://api.anthropic.com",
                "m",
            )
        },
    )
    .unwrap();
    let keys = FakeKeys::new(Ok(Some(SECRET)));
    let s = host::summary_from_settings(
        &db,
        CPU,
        &machine(),
        &keys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert_eq!(keys.reads.get(), 0, "no API key is read for Codex");
    assert!(s.request.remote.is_none() && s.request.local_model.is_none());
    assert!(s.request.claude_code.is_none(), "never Claude Code instead");
    assert!(!s.request.skip && s.request.no_key.is_none());
    let config = s.request.codex.clone().expect("codex");
    assert_eq!(config.model.as_deref(), Some("gpt-5.6-luna"));
    let models = crate::backends::ModelDir::new(dir.0.join("models"));
    let plan = || host::summary_plan(&s.request, None, false, &models).unwrap();
    assert!(matches!(plan(), host::SummaryPlan::Codex(_)));
    assert_eq!(host::skip_reason(&plan()), None);
    // The Claude Code check leaves a Codex plan alone.
    let claude = probe(crate::claude_code::ClaudeCodeStatus::NotInstalled);
    assert!(matches!(
        host::check_claude_code(plan(), &claude),
        host::SummaryPlan::Codex(_)
    ));
    assert_eq!(claude.calls.get(), 0);

    // Not installed or not signed in: no summary, with the reason (a meeting still records).
    for (status, problem) in [
        (CodexStatus::NotInstalled, CodexError::NotInstalled),
        (
            codex_installed(CodexSignIn::NotSignedIn),
            CodexError::NotSignedIn,
        ),
    ] {
        let p = codex_probe(status);
        let checked = host::check_codex(plan(), &p);
        assert_eq!(p.calls.get(), 1);
        assert!(
            matches!(&checked, host::SummaryPlan::CodexUnavailable(e) if *e == problem),
            "{checked:?}"
        );
        assert_eq!(
            host::skip_reason(&checked),
            Some(host::SkipReason::CodexUnavailable(problem))
        );
        let clock = std::sync::Arc::new(earshot_core::identity::ManualClock::new(0));
        assert!(
            host::prepare_summary(&checked, &models, clock, &[])
                .unwrap()
                .is_none(),
            "no service: nothing is handed over"
        );
    }

    // Signed in (or a state the check could not read): the found CLI runs the summary.
    for sign_in in [
        CodexSignIn::SignedIn {
            method: Some("chatgpt"),
        },
        CodexSignIn::Unknown("timed out".into()),
    ] {
        let p = codex_probe(codex_installed(sign_in));
        let checked = host::check_codex(plan(), &p);
        let host::SummaryPlan::Codex(c) = &checked else {
            panic!("{checked:?}");
        };
        assert_eq!(
            c.cli.as_ref().map(|c| c.program.clone()),
            Some("C:\\fake\\codex.exe".into())
        );
        let clock = std::sync::Arc::new(earshot_core::identity::ManualClock::new(0));
        let prepared = host::prepare_summary(&checked, &models, clock, &[])
            .unwrap()
            .expect("a service");
        assert!(prepared.service.is_codex() && prepared.local.is_none());
        assert_eq!(prepared.service.model(), "codex/gpt-5.6-luna");
        assert!(prepared.service.audit_log().is_empty());
        assert_eq!(
            host::live_summaries_for(Some(&prepared), CPU).err(),
            Some(host::LIVE_SUMMARIES_NEED_LOCAL),
            "no live summaries through Codex"
        );
    }

    // Any other plan is not checked.
    let p = codex_probe(CodexStatus::NotInstalled);
    assert!(matches!(
        host::check_codex(host::SummaryPlan::Skip, &p),
        host::SummaryPlan::Skip
    ));
    let claude_plan = host::SummaryPlan::ClaudeCode(Default::default());
    assert!(matches!(
        host::check_codex(claude_plan, &p),
        host::SummaryPlan::ClaudeCode(_)
    ));
    assert_eq!(p.calls.get(), 0);
}
