//! Tests of a meeting's summary kept on this computer (T12.13): the record, and the summary
//! plan a remote writer becomes. Every name contains `summary_kept_local`.

use std::cell::Cell;

use earshot_hardware::{Cpu, Hardware};

use super::*;
use crate::backends::{Accelerator, ModelDir};
use crate::host::{self, SkipReason, SummaryPlan, SummaryRequest};
use crate::summary_settings::{
    ApiKey, ApiKeyError, ApiKeyStore, EndpointProvider, SummarySettings, SummaryWriter,
    set_summary_settings,
};
use crate::testutil::{TempDir, meeting, services};

const CPU: &[Accelerator] = &[Accelerator::Cpu];

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

/// A key store that holds a key and counts its reads.
struct CountingKeys(Cell<u32>);

impl ApiKeyStore for CountingKeys {
    fn anthropic_key(&self) -> Result<Option<ApiKey>, ApiKeyError> {
        self.0.set(self.0.get() + 1);
        Ok(ApiKey::new("sk-ant-test-key-0123456789"))
    }
}

/// Every writer that would send the meeting off this computer, as the Summaries page stores it.
fn remote_writers() -> Vec<(&'static str, SummarySettings)> {
    let endpoint = |provider, url: &str| SummarySettings {
        writer: SummaryWriter::OwnEndpoint,
        provider,
        url: url.into(),
        model: "m".into(),
        ..SummarySettings::default()
    };
    vec![
        (
            "openai-compatible",
            endpoint(EndpointProvider::OpenAiCompatible, "http://127.0.0.1:9/v1"),
        ),
        (
            "anthropic",
            endpoint(EndpointProvider::Anthropic, "https://api.anthropic.com"),
        ),
        (
            "claude-code",
            SummarySettings {
                writer: SummaryWriter::ClaudeCode,
                claude_model: "sonnet".into(),
                ..SummarySettings::default()
            },
        ),
        (
            "codex",
            SummarySettings {
                writer: SummaryWriter::Codex,
                codex_model: "gpt-5.6-luna".into(),
                ..SummarySettings::default()
            },
        ),
    ]
}

/// Installs the summary model `id` in `models` as a sparse file of the manifest size.
fn install(models: &std::path::Path, id: &str) {
    let m = crate::summary_model::summary_model(id).unwrap();
    let path = models.join(m.file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::File::create(&path)
        .unwrap()
        .set_len(m.size)
        .unwrap();
}

#[test]
fn summary_kept_local_record_presence_is_the_answer() {
    let dir = TempDir::new("summary-kept-local-record");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "", 1);
    // No record: the meeting goes to the writer (every meeting from before T12.13).
    assert_eq!(summary_privacy(&svc.store(), m), SummaryPrivacy::Writer);
    crate::local::set(&mut svc.store(), &record_key(m), &encode()).unwrap();
    assert_eq!(summary_privacy(&svc.store(), m), SummaryPrivacy::KeptLocal);
    // A record this version cannot read still keeps the meeting here.
    for damaged in [&b""[..], b"\xff\xff", b"writer", &99u32.to_le_bytes()] {
        crate::local::set(&mut svc.store(), &record_key(m), damaged).unwrap();
        assert_eq!(
            summary_privacy(&svc.store(), m),
            SummaryPrivacy::KeptLocal,
            "{damaged:?}"
        );
    }
    // Allowing the writer deletes it, and only for this meeting.
    let other = meeting(&svc, "", 2);
    crate::local::set(&mut svc.store(), &record_key(other), &encode()).unwrap();
    allow_writer(&svc, m).unwrap();
    assert_eq!(summary_privacy(&svc.store(), m), SummaryPrivacy::Writer);
    assert_eq!(
        svc.store().local_value(record_key(m).as_str()).unwrap(),
        None
    );
    assert_eq!(
        summary_privacy(&svc.store(), other),
        SummaryPrivacy::KeptLocal
    );
    // Allowing again changes nothing; an unknown meeting is refused.
    allow_writer(&svc, m).unwrap();
    let unknown =
        EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 7]);
    assert!(matches!(
        allow_writer(&svc, unknown),
        Err(RuntimeError::Edit(crate::edit::EditError::MeetingNotFound(id))) if id == unknown
    ));
}

#[test]
fn summary_kept_local_record_is_version_then_marker() {
    let mut want = RECORD_VERSION.to_le_bytes().to_vec();
    want.extend_from_slice(b"local");
    assert_eq!(encode(), want);
    assert_eq!(
        record_key(EntityId::from_bytes([
            0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1
        ]))
        .as_str(),
        "summary-kept-local/01900000-0000-7000-8000-000000000001"
    );
}

#[test]
fn summary_kept_local_remote_writer_becomes_the_local_model_and_reads_no_key() {
    for (name, settings) in remote_writers() {
        let dir = TempDir::new("summary-kept-local-plan");
        let mut db = Store::open(&dir.0.join("earshot.db")).unwrap();
        set_summary_settings(&mut db, &settings).unwrap();
        let keys = CountingKeys(Cell::new(0));
        let s = host::summary_from_settings(&db, CPU, &machine(), &keys, SummaryPrivacy::KeptLocal)
            .unwrap();
        assert_eq!(keys.0.get(), 0, "{name}: no API key is read");
        let r = &s.request;
        assert!(r.kept_local, "{name}");
        assert!(
            r.remote.is_none() && r.claude_code.is_none() && r.codex.is_none(),
            "{name}: {r:?}"
        );
        assert!(r.no_key.is_none() && !r.skip, "{name}");
        assert_eq!(s.writer, SummaryWriter::OnThisComputer, "{name}");

        // Without a local model: no summary, said, never the writer.
        let models = ModelDir::new(dir.0.join("models"));
        let plan = host::summary_plan(r, Some(16 << 30), false, &models).unwrap();
        let SummaryPlan::KeptLocalNoModel(choice) = &plan else {
            panic!("{name}: {plan:?}");
        };
        assert_eq!(
            host::skip_reason(&plan),
            Some(SkipReason::KeptLocalNoModel {
                model: choice.model.id
            })
        );
        let clock = std::sync::Arc::new(earshot_core::identity::ManualClock::new(0));
        assert!(
            host::prepare_summary(&plan, &models, clock, &[])
                .unwrap()
                .is_none(),
            "{name}: no service, nothing built"
        );

        // With the model this machine picks installed: that model.
        install(&dir.0.join("models"), choice.model.id);
        let plan = host::summary_plan(r, Some(16 << 30), false, &models).unwrap();
        assert!(
            matches!(&plan, SummaryPlan::Local(c) if c.model.id == choice.model.id),
            "{name}: {plan:?}"
        );
        assert_eq!(host::skip_reason(&plan), None);
    }
}

#[test]
fn summary_kept_local_changes_nothing_for_this_computer_or_no_summary() {
    let dir = TempDir::new("summary-kept-local-same");
    let mut db = Store::open(&dir.0.join("earshot.db")).unwrap();
    let keys = CountingKeys(Cell::new(0));
    for writer in [SummaryWriter::OnThisComputer, SummaryWriter::NoSummary] {
        set_summary_settings(
            &mut db,
            &SummarySettings {
                writer,
                ..SummarySettings::default()
            },
        )
        .unwrap();
        let read =
            |privacy| host::summary_from_settings(&db, CPU, &machine(), &keys, privacy).unwrap();
        let (kept, writer_side) = (
            read(SummaryPrivacy::KeptLocal),
            read(SummaryPrivacy::Writer),
        );
        assert!(!kept.request.kept_local, "{writer:?}");
        assert_eq!(kept.writer, writer);
        assert_eq!(
            format!("{:?}", kept.request),
            format!("{:?}", writer_side.request),
            "{writer:?}"
        );
    }
}

#[test]
fn summary_kept_local_request_never_plans_a_remote_writer() {
    // Whatever else a kept-local request carries (a bug elsewhere), the plan is local or none.
    let dir = TempDir::new("summary-kept-local-request");
    let models = ModelDir::new(dir.0.join("models"));
    let request = SummaryRequest {
        remote: Some(crate::summary::SummaryConfig::new(
            "http://127.0.0.1:9/v1",
            "m",
        )),
        claude_code: Some(crate::claude_code::ClaudeCodeSummaryConfig::default()),
        codex: Some(crate::codex::CodexSummaryConfig::default()),
        no_key: Some(ApiKeyError::Missing),
        kept_local: true,
        ..SummaryRequest::default()
    };
    let plan = host::summary_plan(&request, Some(16 << 30), false, &models).unwrap();
    assert!(matches!(plan, SummaryPlan::KeptLocalNoModel(_)), "{plan:?}");
    // The same request not kept local is the endpoint's (the key problem first): the field
    // is what decides.
    let open = SummaryRequest {
        kept_local: false,
        ..request
    };
    assert!(matches!(
        host::summary_plan(&open, Some(16 << 30), false, &models).unwrap(),
        SummaryPlan::NoApiKey(_)
    ));
}
