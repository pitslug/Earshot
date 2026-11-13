//! Tests of the summary style setting (T7.4b): stored values, validation, and the style
//! reaching the request that writes the summary. Every name contains `summary_style`.

use earshot_core::asr::CancelToken;
use earshot_core::prompt::MAX_STYLE_CHARS;
use earshot_core::summary::SummaryV1;
use earshot_llm_replay::mock::{MockResponse, MockServer};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use super::*;
use crate::summary::{SummaryConfig, SummaryService};
use crate::testutil::{TempDir, meeting, services};

fn store(dir: &TempDir) -> Store {
    Store::open(&dir.0.join("earshot.db")).unwrap()
}

#[test]
fn summary_style_is_empty_until_one_is_saved() {
    let dir = TempDir::new("summary-style");
    let s = store(&dir);
    assert_eq!(summary_style(&s).unwrap(), (SummaryStyle::default(), None));
}

#[test]
fn summary_style_saves_trimmed_text_and_empty_clears_it() {
    let dir = TempDir::new("summary-style");
    let mut s = store(&dir);
    let saved = set_summary_style(&mut s, "  Short, plain words.\r\nLead with risks.  ").unwrap();
    assert_eq!(saved.as_str(), "Short, plain words.\nLead with risks.");
    assert_eq!(summary_style(&s).unwrap(), (saved, None));
    assert_eq!(
        s.local_value(SUMMARY_STYLE_KEY).unwrap().as_deref(),
        Some("Short, plain words.\nLead with risks.".as_bytes())
    );
    let cleared = set_summary_style(&mut s, " \n ").unwrap();
    assert!(cleared.is_empty());
    assert_eq!(summary_style(&s).unwrap(), (SummaryStyle::default(), None));
}

#[test]
fn summary_style_refusals_store_nothing() {
    let dir = TempDir::new("summary-style");
    let mut s = store(&dir);
    set_summary_style(&mut s, "Keep it short.").unwrap();
    let long = "a".repeat(MAX_STYLE_CHARS + 1);
    let err = set_summary_style(&mut s, &long).unwrap_err();
    assert!(matches!(
        err,
        SummaryStyleError::Refused(StyleError::TooLong(_))
    ));
    assert_eq!(
        err.to_string(),
        "the summary style can be at most 1000 characters; this one has 1001"
    );
    let err = set_summary_style(&mut s, "a\u{7}b").unwrap_err();
    assert!(matches!(
        err,
        SummaryStyleError::Refused(StyleError::ControlCharacter)
    ));
    assert_eq!(summary_style(&s).unwrap().0.as_str(), "Keep it short.");
    // Exactly the limit is fine.
    set_summary_style(&mut s, &"a".repeat(MAX_STYLE_CHARS)).unwrap();
}

#[test]
fn summary_style_a_stored_value_this_version_refuses_reads_as_none_and_is_reported() {
    let dir = TempDir::new("summary-style");
    let mut s = store(&dir);
    let secret_ish = format!("private words {}", "x".repeat(MAX_STYLE_CHARS));
    s.set_local_value(SUMMARY_STYLE_KEY, secret_ish.as_bytes())
        .unwrap();
    let (style, refused) = summary_style(&s).unwrap();
    assert!(style.is_empty());
    let refused = refused.expect("reported");
    assert_eq!(refused.key, SUMMARY_STYLE_KEY);
    assert!(!refused.to_string().contains("private words"), "{refused}");
    assert!(refused.to_string().contains("1000"), "{refused}");

    s.set_local_value(SUMMARY_STYLE_KEY, &[0xff, 0xfe]).unwrap();
    let (style, refused) = summary_style(&s).unwrap();
    assert!(style.is_empty());
    assert!(refused.unwrap().stored.contains("not text"));
}

#[test]
fn summary_style_a_refused_stored_value_is_a_meeting_note() {
    use earshot_hardware::{Cpu, Hardware};
    let dir = TempDir::new("summary-style");
    let mut s = store(&dir);
    s.set_local_value(SUMMARY_STYLE_KEY, b"bad\x07").unwrap();
    let hw = Hardware {
        cpu: Cpu {
            name: Some("Test CPU".into()),
            physical_cores: Some(8),
            logical_cores: Some(8),
        },
        total_memory: Some(16 << 30),
        ..Hardware::default()
    };
    let cpu = [crate::backends::Accelerator::Cpu];
    let setup = crate::host::summary_from_settings(
        &s,
        &cpu,
        &hw,
        &crate::summary_settings::NoApiKeys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(
        setup.notes.iter().any(|n| n.contains(SUMMARY_STYLE_KEY)),
        "{:?}",
        setup.notes
    );
    // A readable style is no note.
    set_summary_style(&mut s, "Short.").unwrap();
    let setup = crate::host::summary_from_settings(
        &s,
        &cpu,
        &hw,
        &crate::summary_settings::NoApiKeys,
        crate::keep_local::SummaryPrivacy::Writer,
    )
    .unwrap();
    assert!(
        !setup.notes.iter().any(|n| n.contains(SUMMARY_STYLE_KEY)),
        "{:?}",
        setup.notes
    );
}

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
fn summary_style_reaches_the_summary_request_and_a_resummarise_uses_the_current_one() {
    let dir = TempDir::new("summary-style");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 1);
    assert!(SummaryService::input(&svc, id).unwrap().style.is_empty());
    set_summary_style(&mut svc.store(), "Use our team's codename Bluebird.").unwrap();
    assert_eq!(
        SummaryService::input(&svc, id).unwrap().style.as_str(),
        "Use our team's codename Bluebird."
    );

    let server = MockServer::start(vec![reply("Release planning"), reply("Again")]).unwrap();
    let endpoint = server.endpoint();
    let service = SummaryService::new(
        &SummaryConfig::new(endpoint.as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    let run = |svc: &crate::Services| {
        service
            .summarize(
                svc,
                id,
                &mut ChaCha8Rng::seed_from_u64(2),
                &CancelToken::new(),
            )
            .unwrap()
    };
    run(&svc);
    set_summary_style(&mut svc.store(), "Formal tone, name the Kestrel project.").unwrap();
    run(&svc);
    let bodies: Vec<String> = server
        .requests()
        .iter()
        .map(|r| String::from_utf8(r.body.clone()).unwrap())
        .collect();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains("BEGIN USER STYLE"));
    assert!(bodies[0].contains("Bluebird"));
    assert!(!bodies[1].contains("Bluebird"));
    assert!(bodies[1].contains("Kestrel"));
    // Only the configured endpoint was contacted.
    for e in service.audit_log().entries() {
        assert_eq!(
            (e.host.as_str(), e.port),
            (endpoint.host_str().unwrap(), endpoint.port().unwrap())
        );
    }
}

// T12.17: the summary template beside the style.

#[test]
fn summary_style_template_is_standard_until_one_is_chosen_and_each_is_stored_by_name() {
    let dir = TempDir::new("summary-style");
    let mut s = store(&dir);
    assert_eq!(summary_template(&s).unwrap(), (StylePreset::Standard, None));
    for p in StylePreset::ALL {
        set_summary_template(&mut s, p).unwrap();
        assert_eq!(summary_template(&s).unwrap(), (p, None));
        assert_eq!(
            s.local_value(SUMMARY_TEMPLATE_KEY).unwrap().as_deref(),
            Some(p.name().as_bytes())
        );
    }
}

#[test]
fn summary_style_template_an_unknown_stored_word_reads_as_standard_and_is_reported() {
    let dir = TempDir::new("summary-style");
    let mut s = store(&dir);
    for bad in [&b"terse"[..], &[0xff, 0xfe][..], &b""[..]] {
        s.set_local_value(SUMMARY_TEMPLATE_KEY, bad).unwrap();
        let (p, refused) = summary_template(&s).unwrap();
        assert_eq!(p, StylePreset::Standard);
        let refused = refused.expect("reported");
        assert_eq!(refused.key, SUMMARY_TEMPLATE_KEY);
        assert!(!refused.to_string().contains("terse"), "{refused}");
    }
}

#[test]
fn summary_style_template_reaches_the_summary_request_before_the_style() {
    let dir = TempDir::new("summary-style");
    let (svc, _) = services(&dir);
    let id = meeting(&svc, "", 1);
    assert_eq!(
        SummaryService::input(&svc, id).unwrap().style_preset,
        StylePreset::Standard
    );
    set_summary_template(&mut svc.store(), StylePreset::Decisions).unwrap();
    set_summary_style(&mut svc.store(), "Name the Kestrel project.").unwrap();
    assert_eq!(
        SummaryService::input(&svc, id).unwrap().style_preset,
        StylePreset::Decisions
    );
    let server = MockServer::start(vec![reply("Release planning")]).unwrap();
    let service = SummaryService::new(
        &SummaryConfig::new(server.endpoint().as_str(), "test-model"),
        svc.clock.clone(),
    )
    .unwrap();
    service
        .summarize(
            &svc,
            id,
            &mut ChaCha8Rng::seed_from_u64(2),
            &CancelToken::new(),
        )
        .unwrap();
    let body = String::from_utf8(server.requests()[0].body.clone()).unwrap();
    let preset = body
        .find(&StylePreset::Decisions.text()[..40])
        .expect("template text");
    let style = body.find("Kestrel").expect("style text");
    assert!(preset < style);
    assert!(body.find("BEGIN USER STYLE").unwrap() < preset);
}

#[test]
fn summary_style_template_unknown_is_a_meeting_note() {
    use earshot_hardware::{Cpu, Hardware};
    let dir = TempDir::new("summary-style");
    let mut s = store(&dir);
    s.set_local_value(SUMMARY_TEMPLATE_KEY, b"terse").unwrap();
    let hw = Hardware {
        cpu: Cpu {
            name: Some("Test CPU".into()),
            physical_cores: Some(8),
            logical_cores: Some(8),
        },
        total_memory: Some(16 << 30),
        ..Hardware::default()
    };
    let cpu = [crate::backends::Accelerator::Cpu];
    let notes = |s: &Store| {
        crate::host::summary_from_settings(
            s,
            &cpu,
            &hw,
            &crate::summary_settings::NoApiKeys,
            crate::keep_local::SummaryPrivacy::Writer,
        )
        .unwrap()
        .notes
    };
    assert!(notes(&s).iter().any(|n| n.contains(SUMMARY_TEMPLATE_KEY)));
    set_summary_template(&mut s, StylePreset::Brief).unwrap();
    assert!(!notes(&s).iter().any(|n| n.contains(SUMMARY_TEMPLATE_KEY)));
}
