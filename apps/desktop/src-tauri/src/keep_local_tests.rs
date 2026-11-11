//! A meeting kept on this computer in the app (T12.13): the pre-flight's and the re-summary's
//! summary setup, the meeting detail and "Allow". Every name contains `meeting`.

use std::cell::Cell;
use std::sync::Arc;

use earshot_core::identity::{Clock, EntityId};
use earshot_core::model::{Meeting, Stamp};
use earshot_core::prompt::Template;
use earshot_runtime::claude_code::{ClaudeCodeProbe, ClaudeCodeStatus};
use earshot_runtime::codex::{CodexProbe, CodexStatus};
use earshot_runtime::host::SkipReason;
use earshot_runtime::summary_settings::{
    ApiKey, ApiKeyError, ApiKeyStore, EndpointProvider, SummarySettings, set_summary_settings,
};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::meeting::{StartRequest, summary_setup};
use crate::meeting_events::{CaptureMode, MeetingEvent, WarningKind, kept_local_banner};
use crate::testutil::{TempDir, services};

/// A Claude Code probe that must never be asked.
struct NeverClaude;
impl ClaudeCodeProbe for NeverClaude {
    fn status(&self) -> ClaudeCodeStatus {
        panic!("a meeting kept on this computer never checks Claude Code")
    }
}

/// A Codex probe that must never be asked.
struct NeverCodex;
impl CodexProbe for NeverCodex {
    fn status(&self) -> CodexStatus {
        panic!("a meeting kept on this computer never checks Codex")
    }
}

/// A key store holding a key, counting its reads.
struct Keys(Cell<u32>);
impl ApiKeyStore for Keys {
    fn anthropic_key(&self) -> Result<Option<ApiKey>, ApiKeyError> {
        self.0.set(self.0.get() + 1);
        Ok(ApiKey::new("sk-ant-keep-local-test-key-0123456789"))
    }
}

/// Every writer that would send a meeting off this computer.
fn remote_writers() -> Vec<SummarySettings> {
    vec![
        SummarySettings {
            writer: SummaryWriter::OwnEndpoint,
            provider: EndpointProvider::Anthropic,
            url: "https://api.anthropic.com".into(),
            model: "m".into(),
            ..SummarySettings::default()
        },
        SummarySettings {
            writer: SummaryWriter::OwnEndpoint,
            provider: EndpointProvider::OpenAiCompatible,
            url: "http://127.0.0.1:9/v1".into(),
            model: "m".into(),
            ..SummarySettings::default()
        },
        SummarySettings {
            writer: SummaryWriter::ClaudeCode,
            ..SummarySettings::default()
        },
        SummarySettings {
            writer: SummaryWriter::Codex,
            ..SummarySettings::default()
        },
    ]
}

fn set_writer(svc: &Services, settings: &SummarySettings) {
    set_summary_settings(&mut svc.store(), settings).unwrap();
}

/// A finished meeting row (distinct seeds per call).
fn meeting(svc: &Services, seed: u64) -> EntityId {
    let clock: Arc<dyn Clock + Send + Sync> = Arc::clone(&svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let mut m = Meeting::new("Invented review", 1_000, 600, Template::General, &mut stamp);
    m.duration_ms = Some(60_000);
    let id = m.id;
    earshot_core::model::MeetingRepo::put_meeting(store, m).unwrap();
    id
}

/// Marks `meeting` kept local with the record a meeting started so carries (its key and
/// bytes are pinned in `earshot_runtime::keep_local`'s tests).
fn keep_local(svc: &Services, meeting: EntityId, value: &[u8]) {
    svc.store()
        .set_local_value(&format!("summary-kept-local/{meeting}"), value)
        .unwrap();
}

fn record() -> Vec<u8> {
    let mut v = 1u32.to_le_bytes().to_vec();
    v.extend_from_slice(b"local");
    v
}

#[test]
fn meeting_start_request_keeps_the_summary_local_only_when_asked() {
    let request = |keep_local| StartRequest {
        mode: CaptureMode::Call,
        title: String::new(),
        keep_local,
    };
    assert_eq!(
        request(true).summary_privacy(),
        earshot_runtime::host::SummaryPrivacy::KeptLocal
    );
    assert_eq!(
        request(false).summary_privacy(),
        earshot_runtime::host::SummaryPrivacy::Writer
    );
}

#[test]
fn meeting_kept_local_setup_builds_nothing_remote_and_says_why() {
    for settings in remote_writers() {
        let dir = TempDir::new("keep-local-setup");
        let svc = services(&dir);
        let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
        set_writer(&svc, &settings);
        let keys = Keys(Cell::new(0));
        let s = summary_setup(
            &svc,
            &models,
            &keys,
            &NeverClaude,
            &NeverCodex,
            earshot_runtime::host::SummaryPrivacy::KeptLocal,
        )
        .unwrap();
        assert!(s.summary.is_none(), "{:?}", settings.writer);
        assert_eq!(keys.0.get(), 0, "no API key is read");
        let SkipReason::KeptLocalNoModel { model } = s.no_summary else {
            panic!("{:?}", s.no_summary);
        };
        assert!(
            s.local_chosen,
            "it is written on this computer or not at all"
        );
        let Some(MeetingEvent::Warning { key, kind, message }) = kept_local_banner(&s.no_summary)
        else {
            panic!("a banner");
        };
        assert_eq!(key, "summary-kept-local");
        assert_eq!(kind, WarningKind::Backend);
        assert!(message.starts_with("The summary will not run"), "{message}");
        assert!(
            message.contains(earshot_runtime::catalogue::display_name(model)),
            "{message}"
        );
        assert!(!message.contains(model), "no manifest id: {message}");
        assert!(!message.contains('\u{2014}'), "{message}");
        // The same settings for a meeting that is not kept local: the writer, as before.
        let open = summary_setup(
            &svc,
            &models,
            &keys,
            &earshot_runtime::claude_code::FixedClaudeCode(ClaudeCodeStatus::NotInstalled),
            &earshot_runtime::codex::FixedCodex(CodexStatus::NotInstalled),
            earshot_runtime::host::SummaryPrivacy::Writer,
        )
        .unwrap();
        assert!(!matches!(
            open.no_summary,
            SkipReason::KeptLocalNoModel { .. }
        ));
        assert!(kept_local_banner(&open.no_summary).is_none());
    }
}

#[test]
fn meeting_kept_local_resummarise_fails_and_sends_nothing() {
    let dir = TempDir::new("keep-local-job");
    let svc = services(&dir);
    let models = earshot_runtime::backends::ModelDir::new(dir.0.join("models"));
    for (seed, settings) in remote_writers().iter().enumerate() {
        set_writer(&svc, settings);
        let id = meeting(&svc, seed as u64 + 1);
        // A damaged record keeps the meeting here as well as a whole one.
        let value = if seed % 2 == 0 {
            record()
        } else {
            b"?".to_vec()
        };
        keep_local(&svc, id, &value);
        let keys = Keys(Cell::new(0));
        let mut warned = Vec::new();
        let e = crate::jobs::summarise_with(
            &svc,
            id,
            &models,
            &keys,
            &NeverClaude,
            &NeverCodex,
            &mut |w| warned.push(w),
        )
        .expect_err("no local summary model: the job fails");
        assert_eq!(e.kind, "summary_kept_local");
        assert!(!e.retryable);
        assert!(e.message.contains("kept on this computer"), "{}", e.message);
        assert!(e.message.contains("Settings, Summaries"), "{}", e.message);
        assert_eq!(keys.0.get(), 0);
        assert!(warned.is_empty());
        // No handover was recorded: no agent CLI ran.
        assert!(
            earshot_runtime::claude_code::summary_handovers(&svc.store(), id)
                .unwrap()
                .entries
                .is_empty()
        );
    }
}

#[test]
fn meeting_detail_shows_a_kept_local_meeting_until_the_writer_is_allowed() {
    let dir = TempDir::new("keep-local-detail");
    let svc = services(&dir);
    let open = meeting(&svc, 1);
    let kept = meeting(&svc, 2);
    keep_local(&svc, kept, &record());
    let view = |m| crate::detail::meeting_detail(&svc, m).unwrap().kept_local;
    assert_eq!(view(open), None);
    // The writer named is the one "Allow" lets summarise it; none while it is local.
    assert_eq!(view(kept), Some(KeptLocalView { writer: None }));
    for (settings, name) in remote_writers().into_iter().zip([
        "your own server",
        "your own server",
        "Claude Code",
        "Codex",
    ]) {
        set_writer(&svc, &settings);
        assert_eq!(
            view(kept),
            Some(KeptLocalView {
                writer: Some(name.into())
            })
        );
    }
    // Allowing changes this meeting only, and sends nothing (no handover, no summary).
    allow(&svc, &kept.to_string()).unwrap();
    assert_eq!(view(kept), None);
    assert!(
        earshot_core::model::MeetingRepo::summaries(&**svc.store(), kept)
            .unwrap()
            .is_empty()
    );
    keep_local(&svc, open, &record());
    assert!(view(open).is_some());
    // An unknown meeting is refused as the detail refuses edits.
    let e = allow(&svc, &EntityId::from_bytes([7; 16]).to_string()).unwrap_err();
    assert_eq!(
        e.kind,
        crate::detail::edit_error(&earshot_runtime::edit::EditError::MeetingNotFound(
            EntityId::from_bytes([7; 16])
        ))
        .kind
    );
    assert!(allow(&svc, "not an id").is_err());
}
