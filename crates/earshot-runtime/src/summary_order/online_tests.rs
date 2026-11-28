//! T14.72: the one online service (docs/summary-order.md section 12): the pick as read, an
//! order holding at most one online writer, changing the pick, the provider's one writer and
//! runs trying only the pick. Every name holds `summary_order`
//! (`cargo test -p earshot-runtime summary_order`).

use std::sync::atomic::{AtomicU32, Ordering};

use earshot_core::asr::CancelToken;
use earshot_core::identity::DeviceId;
use earshot_core::model::{RequestState, SummaryRequest, SummaryRequestRepo};
use earshot_llm_replay::mock::{MockResponse, MockServer};
use earshot_store::Store;

use super::*;
use crate::Services;
use crate::host::{SettingsWriters, SkipReason, SummaryResult};
use crate::local::DeviceRecord;
use crate::offload::HelperWriter;
use crate::summary_order::run::{
    Built, OutcomeWord, RunPlan, RunStart, StepResult, StepWriters, helper_writer,
    run_summary_order,
};
use crate::summary_settings::{
    SUMMARY_ENDPOINT_KEY, SUMMARY_ENDPOINT_MODEL_KEY, SUMMARY_PROVIDER_KEY, set_summary_settings,
    summary_settings,
};
use crate::testutil::{TempDir, meeting, services, store_order};

const DESK: DeviceId = DeviceId::from_bytes([0x3d; 16]);
const LOCAL: OrderStep = OrderStep::LOCAL;
const ENDPOINT: OrderStep = OrderStep::Writer(WriterStep::Endpoint);
const CLAUDE: OrderStep = OrderStep::Writer(WriterStep::ClaudeCode);
const CODEX: OrderStep = OrderStep::Writer(WriterStep::Codex);

fn order(steps: &[OrderStep]) -> SummaryOrder {
    SummaryOrder::new(steps.to_vec()).unwrap()
}

fn raw(store: &Store, key: &str) -> Option<String> {
    store
        .local_value(key)
        .unwrap()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn put(store: &mut Store, record: DeviceRecord, value: &str) {
    crate::local::set(store, &record.key(), value.as_bytes()).unwrap();
}

/// Stores `steps` as an order with its mirror, byte for byte as any build writes them (no rule
/// of this build checks them): what an older build, or an earlier version, left.
fn put_order(store: &mut Store, steps: &[OrderStep]) {
    let o = SummaryOrder::new(steps.to_vec()).unwrap();
    crate::local::set(store, &DeviceRecord::SummaryOrder.key(), &encode(&o)).unwrap();
    put(
        store,
        DeviceRecord::SummaryWriter,
        o.mirror().writer.as_str(),
    );
    put(store, DeviceRecord::SummaryOffloadDevice, "");
}

fn put_endpoint(store: &mut Store, provider: EndpointProvider, url: &str, model: &str) {
    put(store, DeviceRecord::SummaryProvider, provider.as_str());
    put(store, DeviceRecord::SummaryEndpoint, url);
    put(store, DeviceRecord::SummaryEndpointModel, model);
}

fn anthropic() -> SummarySettings {
    SummarySettings {
        provider: EndpointProvider::Anthropic,
        url: "https://api.anthropic.com".into(),
        model: "an-invented-model".into(),
        ..SummarySettings::default()
    }
}

fn own_server(url: &str) -> SummarySettings {
    SummarySettings {
        provider: EndpointProvider::OpenAiCompatible,
        url: url.into(),
        model: "an-invented-model".into(),
        ..SummarySettings::default()
    }
}

fn pick(store: &Store) -> Option<Service> {
    online_service(store).unwrap().0
}

/// Every key a pick, an order or a settings save may write.
fn snapshot(store: &Store) -> Vec<Option<String>> {
    [
        SUMMARY_ORDER_KEY,
        SUMMARY_WRITER_KEY,
        crate::offload::OFFLOAD_DEVICE_KEY,
        ONLINE_SERVICE_KEY,
        SUMMARY_PROVIDER_KEY,
        SUMMARY_ENDPOINT_KEY,
        SUMMARY_ENDPOINT_MODEL_KEY,
        crate::live_summaries::LIVE_SUMMARIES_KEY,
    ]
    .iter()
    .map(|k| raw(store, k))
    .collect()
}

// ---- the shared case table (fixtures/ui/online_service_cases.json) ----

fn table() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../fixtures/ui/online_service_cases.json"
    ))
    .unwrap()
}

fn step_of(v: &serde_json::Value) -> OrderStep {
    match v.as_str().unwrap() {
        "Local" => LOCAL,
        "Endpoint" => ENDPOINT,
        "ClaudeCode" => CLAUDE,
        "Codex" => CODEX,
        other => panic!("a step the table does not use: {other}"),
    }
}

fn steps_of(v: &serde_json::Value) -> Vec<OrderStep> {
    v.as_array().unwrap().iter().map(step_of).collect()
}

fn service_of(v: &serde_json::Value) -> Option<Service> {
    v.as_str().map(|s| match s {
        "ClaudeCode" => Service::ClaudeCode,
        "Codex" => Service::Codex,
        "AnthropicApi" => Service::AnthropicApi,
        "OwnServer" => Service::OwnServer,
        other => panic!("a service the table does not use: {other}"),
    })
}

fn provider_of(v: &serde_json::Value) -> EndpointProvider {
    match v.as_str().unwrap() {
        "OpenAiCompatible" => EndpointProvider::OpenAiCompatible,
        "Anthropic" => EndpointProvider::Anthropic,
        other => panic!("a provider the table does not use: {other}"),
    }
}

/// A usable address for `provider`, stored when an order holds the endpoint.
fn usable_url(provider: EndpointProvider) -> &'static str {
    match provider {
        EndpointProvider::Anthropic => "https://api.anthropic.com",
        EndpointProvider::OpenAiCompatible => "http://127.0.0.1:8080/v1",
    }
}

#[test]
fn summary_order_online_cases_match_the_shared_table() {
    let t = table();
    let mut wrong = Vec::new();
    for case in t["pick"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let dir = TempDir::new("summary-order-online-pick");
        let mut db = Store::open(&dir.0.join("earshot.db")).unwrap();
        let provider = provider_of(&case["provider"]);
        let steps = steps_of(&case["order"]);
        let url = if steps.contains(&ENDPOINT) {
            usable_url(provider)
        } else {
            ""
        };
        put_endpoint(
            &mut db,
            provider,
            url,
            if url.is_empty() { "" } else { "m" },
        );
        put_order(&mut db, &steps);
        if let Some(key) = case["key"].as_str() {
            put(&mut db, DeviceRecord::OnlineService, key);
        }
        let (got, notes) = online_service(&db).unwrap();
        let want_note = case["note"].as_str().map(|n| match n {
            "unknown_service" => UNKNOWN_SERVICE.to_owned(),
            other => panic!("a note the table does not use: {other}"),
        });
        if got != service_of(&case["pick"]) || notes != want_note.into_iter().collect::<Vec<_>>() {
            wrong.push(format!("pick {name}: {got:?} {notes:?}"));
        }
        if let Some(key) = case["key"].as_str()
            && notes.iter().any(|n| n.contains(key))
        {
            wrong.push(format!("pick {name}: the note shows the stored text"));
        }
    }
    for case in t["normalise"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let dir = TempDir::new("summary-order-online-normalise");
        let mut db = Store::open(&dir.0.join("earshot.db")).unwrap();
        if case["endpoint_set_up"].as_bool().unwrap() {
            put_endpoint(
                &mut db,
                EndpointProvider::OpenAiCompatible,
                usable_url(EndpointProvider::OpenAiCompatible),
                "m",
            );
        }
        let stored = steps_of(&case["stored"]);
        put_order(&mut db, &stored);
        let before = raw(&db, SUMMARY_ORDER_KEY);
        let (got, notes) = summary_order(&db).unwrap();
        let want_notes: Vec<String> = case["note"]
            .as_bool()
            .unwrap()
            .then(|| SECOND_ONLINE_DROPPED.to_owned())
            .into_iter()
            .collect();
        if got.steps() != steps_of(&case["reads"]).as_slice() || notes != want_notes {
            wrong.push(format!("normalise {name}: {got:?} {notes:?}"));
        }
        if raw(&db, SUMMARY_ORDER_KEY) != before {
            wrong.push(format!("normalise {name}: reading wrote"));
        }
    }
    for case in t["switch"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let dir = TempDir::new("summary-order-online-switch");
        let mut db = Store::open(&dir.0.join("earshot.db")).unwrap();
        put_endpoint(
            &mut db,
            provider_of(&case["provider"]),
            case["url"].as_str().unwrap(),
            case["model"].as_str().unwrap(),
        );
        put_order(&mut db, &steps_of(&case["order"]));
        let service = service_of(&case["service"]).unwrap();
        set_online_service(&mut db, service).unwrap();
        let after = &case["after"];
        let (s, _) = summary_settings(&db).unwrap();
        let got = (
            summary_order(&db).unwrap().0.steps().to_vec(),
            s.provider,
            s.url,
            s.model,
            pick(&db),
        );
        let want = (
            steps_of(&after["order"]),
            provider_of(&after["provider"]),
            after["url"].as_str().unwrap().to_owned(),
            after["model"].as_str().unwrap().to_owned(),
            service_of(&after["pick"]),
        );
        if got != want {
            wrong.push(format!("switch {name}: {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    // Public copy only: no em dash anywhere in the table.
    let text = include_str!("../../../../fixtures/ui/online_service_cases.json");
    assert!(!text.contains('\u{2014}'));
}

// ---- 12.13 item 1: the pick as read ----

#[test]
fn summary_order_online_pick_follows_the_order_then_the_key() {
    let dir = TempDir::new("summary-order-online-read");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    assert_eq!(
        online_service(&db).unwrap(),
        (None, vec![]),
        "nothing picked"
    );
    store_order(&mut db, &order(&[LOCAL, CLAUDE]), None);
    assert_eq!(pick(&db), Some(Service::ClaudeCode));
    assert_eq!(raw(&db, ONLINE_SERVICE_KEY).as_deref(), Some("claude-code"));
    // The endpoint, by its provider.
    store_order(&mut db, &order(&[ENDPOINT, LOCAL]), Some(&anthropic()));
    assert_eq!(pick(&db), Some(Service::AnthropicApi));
    // With none in the order: the key. Unticking leaves it.
    set_summary_order(&mut db, &order(&[LOCAL]), None).unwrap();
    assert_eq!(pick(&db), Some(Service::AnthropicApi));
    // An unknown word: none, with the note, never the stored text.
    put(
        &mut db,
        DeviceRecord::OnlineService,
        "an-invented-newer-service",
    );
    let (none, notes) = online_service(&db).unwrap();
    assert_eq!(none, None);
    assert_eq!(notes, vec![UNKNOWN_SERVICE.to_owned()]);
    assert!(!notes[0].contains("invented"));
}

#[test]
fn summary_order_online_old_keys_that_win_name_the_pick() {
    // The control of item 1: old keys an older build changed win (section 3), and the pick
    // follows the order they give, not the stored key.
    let dir = TempDir::new("summary-order-online-old-keys");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    store_order(&mut db, &order(&[LOCAL, CLAUDE]), None);
    assert_eq!(raw(&db, ONLINE_SERVICE_KEY).as_deref(), Some("claude-code"));
    put(&mut db, DeviceRecord::SummaryWriter, "codex");
    assert_eq!(summary_order(&db).unwrap().0, order(&[CODEX]));
    assert_eq!(pick(&db), Some(Service::Codex));
}

// ---- 12.13 item 2: normalising ----

#[test]
fn summary_order_online_a_second_online_writer_is_dropped_on_read_and_on_the_next_write() {
    let dir = TempDir::new("summary-order-online-normalise");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    put_order(&mut db, &[CLAUDE, LOCAL, CODEX]);
    let stored = raw(&db, SUMMARY_ORDER_KEY);
    assert_eq!(
        summary_order(&db).unwrap(),
        (
            order(&[CLAUDE, LOCAL]),
            vec![SECOND_ONLINE_DROPPED.to_owned()]
        )
    );
    assert_eq!(raw(&db, SUMMARY_ORDER_KEY), stored, "reading never writes");
    // The next write stores the normal form, and the note goes.
    drop(db);
    crate::offload::set_chosen_helper(&svc, None).unwrap_or_else(|e| panic!("{e}"));
    let db = svc.store();
    assert_eq!(
        decode(&db.local_value(SUMMARY_ORDER_KEY).unwrap().unwrap()),
        Some(order(&[CLAUDE, LOCAL]))
    );
    assert_eq!(summary_order(&db).unwrap().1, Vec::<String>::new());
}

#[test]
fn summary_order_online_one_online_writer_reads_unchanged() {
    let dir = TempDir::new("summary-order-online-one");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    put_order(&mut db, &[LOCAL, CODEX]);
    assert_eq!(
        summary_order(&db).unwrap(),
        (order(&[LOCAL, CODEX]), vec![])
    );
    // An unusable endpoint after the first online writer is dropped, not unreadable.
    put_order(&mut db, &[CLAUDE, ENDPOINT]);
    let (o, notes) = summary_order(&db).unwrap();
    assert_eq!(o, order(&[CLAUDE]));
    assert_eq!(notes, vec![SECOND_ONLINE_DROPPED.to_owned()]);
    assert!(!notes.contains(&UNREADABLE_ORDER.to_owned()));
}

// ---- 12.13 item 3: write refusals ----

#[test]
fn summary_order_online_writes_refuse_two_online_writers_and_one_not_picked() {
    let dir = TempDir::new("summary-order-online-refuse");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    // Nothing picked: no online writer can be stored.
    assert!(matches!(
        set_summary_order(&mut db, &order(&[CLAUDE]), None),
        Err(SummaryOrderError::NotThePick)
    ));
    store_order(&mut db, &order(&[LOCAL, CLAUDE]), None);
    let before = snapshot(&db);
    assert!(matches!(
        set_summary_order(&mut db, &order(&[CLAUDE, CODEX]), None),
        Err(SummaryOrderError::TwoOnline)
    ));
    assert!(matches!(
        set_summary_order(&mut db, &order(&[LOCAL, CODEX]), None),
        Err(SummaryOrderError::NotThePick)
    ));
    assert!(matches!(
        set_summary_order(
            &mut db,
            &order(&[ENDPOINT]),
            Some(&own_server("http://127.0.0.1:9/v1"))
        ),
        Err(SummaryOrderError::NotThePick)
    ));
    assert_eq!(snapshot(&db), before, "nothing stored");
    assert_eq!(
        SummaryOrderError::NotThePick.to_string(),
        NOT_THE_SERVICE,
        "fixed copy"
    );
    // The control: the pick at any position is accepted, and so is the in-place replacement.
    for steps in [
        &[CLAUDE, LOCAL][..],
        &[OrderStep::Computer(DESK), LOCAL, CLAUDE][..],
        &[LOCAL][..],
    ] {
        set_summary_order(&mut db, &order(steps), None).unwrap();
        assert_eq!(summary_order(&db).unwrap().0, order(steps));
    }
    set_summary_order(&mut db, &order(&[CLAUDE, LOCAL]), None).unwrap();
    assert_eq!(
        set_online_service(&mut db, Service::Codex).unwrap(),
        order(&[CODEX, LOCAL])
    );
}

// ---- 12.13 item 4: changing the pick in place ----

#[test]
fn summary_order_online_pick_change_replaces_the_step_in_place() {
    let dir = TempDir::new("summary-order-online-place");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    store_order(&mut db, &order(&[LOCAL, CLAUDE]), None);
    assert_eq!(
        set_online_service(&mut db, Service::Codex).unwrap(),
        order(&[LOCAL, CODEX])
    );
    assert_eq!(summary_order(&db).unwrap().0, order(&[LOCAL, CODEX]));
    assert_eq!(raw(&db, ONLINE_SERVICE_KEY).as_deref(), Some("codex"));
    assert_eq!(raw(&db, SUMMARY_WRITER_KEY).as_deref(), Some("local"));
    // Picking the service already picked stores nothing.
    let before = snapshot(&db);
    set_online_service(&mut db, Service::Codex).unwrap();
    assert_eq!(snapshot(&db), before);
}

#[test]
fn summary_order_online_pick_change_is_one_transaction() {
    let dir = TempDir::new("summary-order-online-fault");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    store_order(&mut db, &order(&[CLAUDE, LOCAL]), None);
    let before = snapshot(&db);
    fault::fail_after_order();
    assert!(matches!(
        set_online_service(&mut db, Service::Codex),
        Err(SummaryOrderError::Store(_))
    ));
    assert_eq!(
        snapshot(&db),
        before,
        "neither the order, the mirror nor the key"
    );
    assert_eq!(pick(&db), Some(Service::ClaudeCode));
    // The control: without the fault the order, mirror and key move together.
    set_online_service(&mut db, Service::Codex).unwrap();
    assert_eq!(raw(&db, SUMMARY_WRITER_KEY).as_deref(), Some("codex"));
    assert_eq!(raw(&db, ONLINE_SERVICE_KEY).as_deref(), Some("codex"));
}

#[test]
fn summary_order_online_pick_of_an_endpoint_not_set_up_unticks_it() {
    let dir = TempDir::new("summary-order-online-untick");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    store_order(
        &mut db,
        &order(&[LOCAL, CLAUDE, OrderStep::Computer(DESK)]),
        None,
    );
    assert_eq!(
        set_online_service(&mut db, Service::OwnServer).unwrap(),
        order(&[LOCAL, OrderStep::Computer(DESK)]),
        "the step leaves; the rest is unchanged"
    );
    assert_eq!(raw(&db, ONLINE_SERVICE_KEY).as_deref(), Some("endpoint"));
    assert_eq!(pick(&db), Some(Service::OwnServer));
}

#[test]
fn summary_order_online_pick_change_stores_the_order_as_read() {
    // An unreadable stored order: the order as read (the old keys) is stored in its place.
    let dir = TempDir::new("summary-order-online-unreadable");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    put(&mut db, DeviceRecord::SummaryWriter, "claude-code");
    crate::local::set(&mut db, &DeviceRecord::SummaryOrder.key(), b"not an order").unwrap();
    assert_eq!(summary_order(&db).unwrap().0, order(&[CLAUDE]));
    set_online_service(&mut db, Service::Codex).unwrap();
    assert_eq!(
        decode(&db.local_value(SUMMARY_ORDER_KEY).unwrap().unwrap()),
        Some(order(&[CODEX]))
    );
    assert_eq!(summary_order(&db).unwrap(), (order(&[CODEX]), vec![]));
    assert_eq!(pick(&db), Some(Service::Codex));
    // Old keys that win: the same.
    store_order(&mut db, &order(&[LOCAL, CODEX]), None);
    put(&mut db, DeviceRecord::SummaryWriter, "claude-code");
    assert_eq!(pick(&db), Some(Service::ClaudeCode));
    set_online_service(&mut db, Service::Codex).unwrap();
    assert_eq!(summary_order(&db).unwrap(), (order(&[CODEX]), vec![]));
    assert_eq!(
        pick(&db),
        Some(Service::Codex),
        "rule 1 cannot overrule it now"
    );
}

// ---- 12.13 item 5: the endpoint switch ----

#[test]
fn summary_order_online_endpoint_switch_clears_the_address_and_model() {
    let dir = TempDir::new("summary-order-online-endpoint");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    store_order(&mut db, &order(&[LOCAL, ENDPOINT]), Some(&anthropic()));
    set_online_service(&mut db, Service::OwnServer).unwrap();
    let (s, _) = summary_settings(&db).unwrap();
    assert_eq!(s.provider, EndpointProvider::OpenAiCompatible);
    assert_eq!((s.url.as_str(), s.model.as_str()), ("", ""));
    assert_eq!(summary_order(&db).unwrap().0, order(&[LOCAL]));

    // Anthropic API (set up), then Claude Code, then your own server: the clear at the third.
    let dir = TempDir::new("summary-order-online-endpoint-3");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    store_order(&mut db, &order(&[LOCAL, ENDPOINT]), Some(&anthropic()));
    set_online_service(&mut db, Service::ClaudeCode).unwrap();
    let (s, _) = summary_settings(&db).unwrap();
    assert_eq!(
        (s.provider, s.url.as_str(), s.model.as_str()),
        (
            EndpointProvider::Anthropic,
            "https://api.anthropic.com",
            "an-invented-model"
        ),
        "picking Claude Code changes no endpoint field"
    );
    assert_eq!(summary_order(&db).unwrap().0, order(&[LOCAL, CLAUDE]));
    // The provider write is one transaction with the clear.
    let before = snapshot(&db);
    fault::fail_after_provider();
    assert!(matches!(
        set_online_service(&mut db, Service::OwnServer),
        Err(SummaryOrderError::Store(_))
    ));
    assert_eq!(
        snapshot(&db),
        before,
        "provider, address and model as they were"
    );
    set_online_service(&mut db, Service::OwnServer).unwrap();
    let (s, _) = summary_settings(&db).unwrap();
    assert_eq!(
        (s.provider, s.url.as_str(), s.model.as_str()),
        (EndpointProvider::OpenAiCompatible, "", "")
    );
    assert_eq!(
        summary_order(&db).unwrap().0,
        order(&[LOCAL]),
        "the endpoint stays out"
    );
    // Picking Anthropic API over an empty address fills nothing in (the page's fill is a draft).
    set_online_service(&mut db, Service::AnthropicApi).unwrap();
    let (s, _) = summary_settings(&db).unwrap();
    assert_eq!(
        (s.provider, s.url.as_str()),
        (EndpointProvider::Anthropic, "")
    );
}

#[test]
fn summary_order_online_claude_code_to_codex_leaves_the_endpoint_as_stored() {
    let dir = TempDir::new("summary-order-online-cli-switch");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    store_order(&mut db, &order(&[LOCAL, ENDPOINT]), Some(&anthropic()));
    set_online_service(&mut db, Service::ClaudeCode).unwrap();
    let endpoint = |db: &Store| {
        (
            raw(db, SUMMARY_PROVIDER_KEY),
            raw(db, SUMMARY_ENDPOINT_KEY),
            raw(db, SUMMARY_ENDPOINT_MODEL_KEY),
        )
    };
    let before = endpoint(&db);
    set_online_service(&mut db, Service::Codex).unwrap();
    assert_eq!(endpoint(&db), before);
}

// ---- 12.13 item 6: an untick keeps the pick ----

#[test]
fn summary_order_online_untick_keeps_the_pick() {
    // Every upgrade: an order holding Claude Code and no key.
    let dir = TempDir::new("summary-order-online-untick-keep");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    put_order(&mut db, &[LOCAL, CLAUDE]);
    assert_eq!(raw(&db, ONLINE_SERVICE_KEY), None);
    set_summary_order(&mut db, &order(&[LOCAL]), None).unwrap();
    assert_eq!(pick(&db), Some(Service::ClaudeCode));
    // Key claude-code with an order an older build gave Codex: the order's writer is the pick,
    // and stays it after the untick.
    put_order(&mut db, &[LOCAL, CODEX]);
    put(&mut db, DeviceRecord::OnlineService, "claude-code");
    assert_eq!(pick(&db), Some(Service::Codex));
    set_summary_order(&mut db, &order(&[LOCAL]), None).unwrap();
    assert_eq!(pick(&db), Some(Service::Codex));
    // The control: picking another service changes it.
    set_online_service(&mut db, Service::ClaudeCode).unwrap();
    assert_eq!(pick(&db), Some(Service::ClaudeCode));
}

// ---- 12.13 item 7: a stale settings save ----

#[test]
fn summary_order_online_a_stale_settings_save_is_refused() {
    let dir = TempDir::new("summary-order-online-stale");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    store_order(&mut db, &order(&[LOCAL, ENDPOINT]), Some(&anthropic()));
    // The page's cache, read now.
    let cache = summary_settings(&db).unwrap().0;
    assert_eq!(cache.provider, EndpointProvider::Anthropic);
    set_online_service(&mut db, Service::OwnServer).unwrap();
    let before = snapshot(&db);
    let picked = pick(&db);
    // The live summaries switch from the stale cache.
    let e = set_summary_settings(
        &mut db,
        &SummarySettings {
            live: true,
            ..cache.clone()
        },
    )
    .unwrap_err();
    assert!(
        matches!(
            e,
            crate::summary_settings::SummarySettingsError::StaleProvider
        ),
        "{e}"
    );
    assert_eq!(e.to_string(), SETTINGS_CHANGED);
    // The endpoint form from the stale cache.
    assert!(matches!(
        set_summary_order(&mut db, &order(&[LOCAL, ENDPOINT]), Some(&cache)),
        Err(SummaryOrderError::StaleProvider)
    ));
    assert_eq!(snapshot(&db), before, "nothing stored");
    assert_eq!(pick(&db), picked);
    // The control: the same saves from a cache read again are stored.
    let fresh = summary_settings(&db).unwrap().0;
    set_summary_settings(
        &mut db,
        &SummarySettings {
            live: true,
            ..fresh.clone()
        },
    )
    .unwrap();
    assert!(summary_settings(&db).unwrap().0.live);
    let form = SummarySettings {
        url: "http://127.0.0.1:8080/v1".into(),
        model: "an-invented-model".into(),
        ..summary_settings(&db).unwrap().0
    };
    set_summary_order(&mut db, &order(&[LOCAL, ENDPOINT]), Some(&form)).unwrap();
    assert_eq!(summary_order(&db).unwrap().0, order(&[LOCAL, ENDPOINT]));
    assert_eq!(pick(&db), Some(Service::OwnServer));
}

#[test]
fn summary_order_online_settings_never_change_the_order_and_a_stale_writer_refuses_nothing() {
    let dir = TempDir::new("summary-order-online-writer-word");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    store_order(&mut db, &order(&[LOCAL, CLAUDE]), None);
    // A cache that still says "your own endpoint", with no endpoint fields: checked with the
    // order's mirror, so it does not refuse an unrelated save.
    let saved = set_summary_settings(
        &mut db,
        &SummarySettings {
            writer: SummaryWriter::OwnEndpoint,
            claude_model: "opus".into(),
            ..SummarySettings::default()
        },
    )
    .unwrap();
    assert_eq!(saved.writer, SummaryWriter::OnThisComputer);
    assert_eq!(summary_order(&db).unwrap().0, order(&[LOCAL, CLAUDE]));
    assert_eq!(summary_settings(&db).unwrap().0.claude_model, "opus");
}

// ---- 12.13 items 10, 10b and 12: runs and helpers try only the pick ----

/// An agent CLI probe that counts its calls and answers "not installed".
#[derive(Default)]
struct CountedCli(AtomicU32);

impl crate::claude_code::ClaudeCodeProbe for CountedCli {
    fn status(&self) -> crate::claude_code::ClaudeCodeStatus {
        self.0.fetch_add(1, Ordering::SeqCst);
        crate::claude_code::ClaudeCodeStatus::NotInstalled
    }
}

impl crate::codex::CodexProbe for CountedCli {
    fn status(&self) -> crate::codex::CodexStatus {
        self.0.fetch_add(1, Ordering::SeqCst);
        crate::codex::CodexStatus::NotInstalled
    }
}

fn count(n: &AtomicU32) -> u32 {
    n.load(Ordering::SeqCst)
}

fn production<'a>(
    models: &'a crate::backends::ModelDir,
    claude: &'a CountedCli,
    codex: &'a CountedCli,
    hardware: &'a earshot_hardware::Hardware,
) -> SettingsWriters<'a> {
    SettingsWriters {
        models,
        keys: &crate::summary_settings::NoApiKeys,
        claude,
        codex,
        dll_dirs: Vec::new(),
        order: vec![crate::backends::Accelerator::Cpu],
        hardware,
        computers: None,
    }
}

fn reply() -> MockResponse {
    let summary = earshot_core::summary::SummaryV1 {
        title: "Invented planning".into(),
        tldr: "The team agreed to ship on Friday.".into(),
        enhanced_notes: vec![],
        decisions: vec![],
        action_items: vec![],
        open_questions: vec![],
        topics: vec!["release".into()],
    };
    MockResponse::completion(&serde_json::to_string(&summary).unwrap(), "stop")
}

#[test]
fn summary_order_online_a_run_tries_only_the_pick_and_never_falls_back() {
    let dir = TempDir::new("summary-order-online-run");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Invented planning", 41);
    let models = crate::backends::ModelDir::new(dir.0.join("no-models"));
    let hardware = earshot_hardware::Hardware::default();
    // Steps a run copied before the update: a computer, then two online writers.
    let copied = order(&[OrderStep::Computer(DESK), CLAUDE, CODEX]);
    let run = |svc: &Services, claude: &CountedCli, codex: &CountedCli| {
        let writers = production(&models, claude, codex, &hardware);
        let plan = RunPlan {
            writers: &writers,
            local_not_now: &|| false,
            order: None,
        };
        run_summary_order(
            svc,
            m,
            RunStart::Order(copied.clone()),
            &plan,
            &CancelToken::new(),
        )
    };
    store_order(&mut svc.store(), &order(&[LOCAL, CLAUDE]), None);
    let (claude, codex) = (CountedCli::default(), CountedCli::default());
    let out = run(&svc, &claude, &codex);
    // Desk is not reachable (sync off), Claude Code is tried (not installed), Codex is skipped
    // as not chosen without ever being built.
    let words: Vec<_> = out.steps.iter().map(|v| v.result).collect();
    assert_eq!(
        words,
        vec![
            StepResult::Moved(OutcomeWord::Offline),
            StepResult::Moved(OutcomeWord::NotInstalled),
            StepResult::Moved(OutcomeWord::NotChosen),
        ]
    );
    assert_eq!((count(&claude.0), count(&codex.0)), (1, 0));
    // With Codex picked: Claude Code is skipped, Codex tried.
    set_online_service(&mut svc.store(), Service::Codex).unwrap();
    let (claude, codex) = (CountedCli::default(), CountedCli::default());
    let out = run(&svc, &claude, &codex);
    assert_eq!(
        out.steps[1].result,
        StepResult::Moved(OutcomeWord::NotChosen)
    );
    assert_eq!(
        out.steps[2].result,
        StepResult::Moved(OutcomeWord::NotInstalled)
    );
    assert_eq!((count(&claude.0), count(&codex.0)), (0, 1));
}

#[test]
fn summary_order_online_builder_skips_an_online_step_that_is_not_the_pick() {
    // Item 12: with Codex not picked no Codex step is ever built, from any start.
    let dir = TempDir::new("summary-order-online-builder");
    let (svc, _) = services(&dir);
    let models = crate::backends::ModelDir::new(dir.0.join("no-models"));
    let hardware = earshot_hardware::Hardware::default();
    let (claude, codex) = (CountedCli::default(), CountedCli::default());
    let writers = production(&models, &claude, &codex, &hardware);
    store_order(&mut svc.store(), &order(&[CLAUDE]), None);
    assert!(matches!(
        writers.writer(&svc, WriterStep::Codex),
        Built::Skip(SkipReason::NotChosen)
    ));
    assert!(matches!(
        writers.writer(&svc, WriterStep::Endpoint),
        Built::Skip(SkipReason::NotChosen)
    ));
    assert_eq!(count(&codex.0), 0, "Codex never probed");
    // A named run of Codex fails at its step and hands nothing over.
    let m = meeting(&svc, "Invented planning", 43);
    let plan = RunPlan {
        writers: &writers,
        local_not_now: &|| false,
        order: None,
    };
    let named = run_summary_order(&svc, m, RunStart::Named(CODEX), &plan, &CancelToken::new());
    assert!(
        matches!(
            named.summary,
            SummaryResult::Skipped {
                reason: SkipReason::NotChosen
            }
        ),
        "{:?}",
        named.summary
    );
    assert_eq!(count(&codex.0), 0);
    // The control: the pick is built (here probed, and not installed).
    assert!(matches!(
        writers.writer(&svc, WriterStep::ClaudeCode),
        Built::Skip(SkipReason::ClaudeCodeUnavailable(_))
    ));
    assert_eq!(count(&claude.0), 1);
}

/// Production writers that change the pick to `then` right before a step is built: a pick
/// changed between a helper's order read and its build.
struct PickChangesFirst<'a> {
    inner: SettingsWriters<'a>,
    then: Service,
}

impl StepWriters for PickChangesFirst<'_> {
    fn writer(&self, services: &Services, step: WriterStep) -> Built {
        set_online_service(&mut services.store(), self.then).unwrap();
        self.inner.writer(services, step)
    }

    fn computer(&self, services: &Services, device: DeviceId) -> crate::offload::OffloadChoice {
        self.inner.computer(services, device)
    }
}

#[test]
fn summary_order_online_a_helper_whose_pick_changed_declines_and_writes_nothing() {
    let dir = TempDir::new("summary-order-online-helper");
    let (svc, _) = services(&dir);
    let me = svc.store().device();
    let asker = DeviceId::from_bytes([5; 16]);
    let m = meeting(&svc, "Invented planning", 47);
    let server = MockServer::start(vec![reply()]).unwrap();
    store_order(
        &mut svc.store(),
        &order(&[ENDPOINT]),
        Some(&own_server(server.endpoint().as_str())),
    );
    let models = crate::backends::ModelDir::new(dir.0.join("no-models"));
    let hardware = earshot_hardware::Hardware::default();
    let (claude, codex) = (CountedCli::default(), CountedCli::default());
    let claimed = |seed: u8| {
        crate::edit::write(&svc, |tx, stamp| {
            let mut r = SummaryRequest::new(m, me, asker, seed.into(), stamp);
            r.state = RequestState::Claimed;
            r.meta.touch(stamp.hlc, stamp.clock);
            tx.put_summary_request(r.clone())?;
            Ok(r)
        })
        .unwrap()
    };
    let queue = crate::meeting_queue::MeetingQueue::new();
    // The pick moves to Claude Code after the helper read its order: it declines `no_writer`.
    let changing = PickChangesFirst {
        inner: production(&models, &claude, &codex, &hardware),
        then: Service::ClaudeCode,
    };
    let request = claimed(2);
    let done = crate::offload::write_for_request(
        &svc,
        &queue,
        &request,
        &|privacy| helper_writer(&svc, &changing, privacy),
        &CancelToken::new(),
    );
    assert_eq!(
        done,
        crate::offload::HelperOutcome::Declined(crate::offload::reason::NO_WRITER)
    );
    assert!(server.requests().is_empty(), "nothing sent");
    assert_eq!(count(&claude.0), 0, "nothing else tried");
    assert_eq!(
        svc.store()
            .summary_request(request.id)
            .unwrap()
            .unwrap()
            .state,
        RequestState::Declined
    );
    // The control: a helper whose order's online writer is the pick writes it.
    let server = MockServer::start(vec![reply()]).unwrap();
    set_online_service(&mut svc.store(), Service::OwnServer).unwrap();
    store_order(
        &mut svc.store(),
        &order(&[ENDPOINT]),
        Some(&own_server(server.endpoint().as_str())),
    );
    let writers = production(&models, &claude, &codex, &hardware);
    assert!(matches!(
        helper_writer(&svc, &writers, crate::keep_local::SummaryPrivacy::Writer),
        HelperWriter::Write(..)
    ));
    let request = claimed(3);
    let done = crate::offload::write_for_request(
        &svc,
        &queue,
        &request,
        &|privacy| helper_writer(&svc, &writers, privacy),
        &CancelToken::new(),
    );
    assert!(
        matches!(done, crate::offload::HelperOutcome::Done { .. }),
        "{done:?}"
    );
    assert_eq!(server.requests().len(), 1);
}

// ---- 12.13 item 13: what an older build wrote ----

#[test]
fn summary_order_online_reads_what_an_older_build_wrote() {
    let dir = TempDir::new("summary-order-online-older");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    // A 0.9.2 build ticked a second online writer and switched its Provider select: the order
    // holds [Endpoint, Local, Codex] with the provider now Anthropic and an Anthropic address.
    put_endpoint(
        &mut db,
        EndpointProvider::Anthropic,
        "https://api.anthropic.com",
        "m",
    );
    put_order(&mut db, &[ENDPOINT, LOCAL, CODEX]);
    put(&mut db, DeviceRecord::OnlineService, "codex");
    let (o, notes) = summary_order(&db).unwrap();
    assert_eq!(o, order(&[ENDPOINT, LOCAL]), "the first online writer kept");
    assert_eq!(notes, vec![SECOND_ONLINE_DROPPED.to_owned()]);
    assert_eq!(
        pick(&db),
        Some(Service::AnthropicApi),
        "rule 1, by the provider"
    );
    // The provider select moved back: the endpoint service follows it.
    put(&mut db, DeviceRecord::SummaryProvider, "openai");
    put(
        &mut db,
        DeviceRecord::SummaryEndpoint,
        "http://127.0.0.1:8080/v1",
    );
    assert_eq!(pick(&db), Some(Service::OwnServer));
}

/// 12.9a: a run record holding `not chosen` is read here through the production reader; an
/// older build does not know the word, which is what this build meets in a record holding a
/// word newer than itself: the same reader then reads it as unreadable (fails closed, 4.3).
#[test]
fn summary_order_online_a_run_record_with_a_word_its_reader_does_not_know_fails_closed() {
    let dir = TempDir::new("summary-order-online-record");
    let (svc, _) = services(&dir);
    let m = meeting(&svc, "Invented planning", 53);
    let models = crate::backends::ModelDir::new(dir.0.join("no-models"));
    let hardware = earshot_hardware::Hardware::default();
    let (claude, codex) = (CountedCli::default(), CountedCli::default());
    let writers = production(&models, &claude, &codex, &hardware);
    store_order(&mut svc.store(), &order(&[CLAUDE]), None);
    let plan = RunPlan {
        writers: &writers,
        local_not_now: &|| false,
        order: None,
    };
    let out = run_summary_order(
        &svc,
        m,
        RunStart::Order(order(&[CODEX, CLAUDE])),
        &plan,
        &CancelToken::new(),
    );
    assert_eq!(
        out.steps[0].result,
        StepResult::Moved(OutcomeWord::NotChosen)
    );
    let key = crate::summary_order::run::record_key(m);
    let bytes = svc.store().local_value(key.as_str()).unwrap().unwrap();
    // This build reads the record and the word in it.
    let crate::summary_order::run::RunRecord::Read(run) =
        crate::summary_order::run::decode(Some(&bytes))
    else {
        panic!("the record this build wrote reads");
    };
    assert_eq!(run.outcome_of(0), Some(OutcomeWord::NotChosen));
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(text.contains("\"not_chosen\""), "the word as stored");
    // The same record holding a word its reader does not know reads as unreadable.
    let newer = text
        .replace("\"not_chosen\"", "\"a_newer_word\"")
        .into_bytes();
    assert_eq!(
        crate::summary_order::run::decode(Some(&newer)),
        crate::summary_order::run::RunRecord::Unreadable
    );
    crate::local::set(&mut svc.store(), &key, &newer).unwrap();
    assert_eq!(
        crate::summary_order::run::read(&svc.store(), m).unwrap(),
        crate::summary_order::run::RunRecord::Unreadable
    );
}

/// Review advisory 1: a settings save from a page read before two switches (Anthropic API, then
/// your own server, then Anthropic API again, so the provider matches) never writes back the
/// address and model the clear removed; the endpoint form's save still stores them.
#[test]
fn summary_order_online_a_stale_settings_save_never_writes_back_the_address() {
    let dir = TempDir::new("summary-order-online-two-switches");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    store_order(&mut db, &order(&[LOCAL, ENDPOINT]), Some(&anthropic()));
    let cache = summary_settings(&db).unwrap().0;
    set_online_service(&mut db, Service::OwnServer).unwrap();
    set_online_service(&mut db, Service::AnthropicApi).unwrap();
    let saved = set_summary_settings(
        &mut db,
        &SummarySettings {
            live: true,
            ..cache.clone()
        },
    )
    .unwrap();
    let (s, _) = summary_settings(&db).unwrap();
    assert!(s.live, "the switch itself is stored");
    assert_eq!((s.url.as_str(), s.model.as_str()), ("", ""));
    assert_eq!((saved.url.as_str(), saved.model.as_str()), ("", ""));
    assert_eq!(summary_order(&db).unwrap().0, order(&[LOCAL]));
    // The control: the endpoint form's save stores them.
    set_summary_order(&mut db, &order(&[LOCAL, ENDPOINT]), Some(&cache)).unwrap();
    let (s, _) = summary_settings(&db).unwrap();
    assert_eq!(
        (s.url.as_str(), s.model.as_str()),
        ("https://api.anthropic.com", "an-invented-model")
    );
}

/// Review advisory 7: picking the service already picked by rule 1 writes the order as read when
/// the stored order does not read as itself; the control stores nothing when it does.
#[test]
fn summary_order_online_picking_the_same_service_writes_the_order_as_read_only_when_needed() {
    // An unreadable stored order with the old keys naming Claude Code.
    let dir = TempDir::new("summary-order-online-same-unreadable");
    let (svc, _) = services(&dir);
    let mut db = svc.store();
    put(&mut db, DeviceRecord::SummaryWriter, "claude-code");
    crate::local::set(&mut db, &DeviceRecord::SummaryOrder.key(), b"not an order").unwrap();
    assert_eq!(pick(&db), Some(Service::ClaudeCode));
    set_online_service(&mut db, Service::ClaudeCode).unwrap();
    assert_eq!(
        decode(&db.local_value(SUMMARY_ORDER_KEY).unwrap().unwrap()),
        Some(order(&[CLAUDE]))
    );
    assert_eq!(summary_order(&db).unwrap(), (order(&[CLAUDE]), vec![]));
    assert_eq!(raw(&db, ONLINE_SERVICE_KEY).as_deref(), Some("claude-code"));
    // Old keys that win.
    store_order(&mut db, &order(&[LOCAL, CODEX]), None);
    put(&mut db, DeviceRecord::SummaryWriter, "claude-code");
    assert_eq!(pick(&db), Some(Service::ClaudeCode));
    set_online_service(&mut db, Service::ClaudeCode).unwrap();
    assert_eq!(
        decode(&db.local_value(SUMMARY_ORDER_KEY).unwrap().unwrap()),
        Some(order(&[CLAUDE]))
    );
    assert_eq!(summary_order(&db).unwrap(), (order(&[CLAUDE]), vec![]));
    // The control: the stored order reads as itself, so the same pick stores nothing.
    let before = snapshot(&db);
    set_online_service(&mut db, Service::ClaudeCode).unwrap();
    assert_eq!(snapshot(&db), before);
}
