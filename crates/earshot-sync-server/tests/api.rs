//! The relay's calls end to end through its router (T11.2; docs/sync-spec.md sections 3 and
//! 4). In process: requests go through `tower::ServiceExt::oneshot`, never a socket.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use earshot_sync::wire::{
    FORMAT_HEADER, MAX_PRESENCE, MAX_REQUEST, MAX_ROW_BODY, MAX_STAMP_AHEAD_MS, SLOT_TTL_S,
};
use earshot_sync_server::{Clock, Config, Relay, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const SECRET: &str = "an-invented-setup-secret-for-tests";

/// A folder under the system temp folder, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        // The counter keeps two tests in one clock tick apart (T12.70).
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "earshot-relay-{name}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    app: Router,
    now: Arc<AtomicU64>,
    _dir: TempDir,
}

fn harness(name: &str) -> Harness {
    let dir = TempDir::new(name);
    let now = Arc::new(AtomicU64::new(1_800_000_000));
    let clock: Clock = {
        let now = Arc::clone(&now);
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let config = Config {
        setup_secret: SECRET.into(),
        data: dir.0.clone(),
        listen: "127.0.0.1:0".parse().unwrap(),
    };
    let relay = Relay::open_with_clock(&config, clock).unwrap();
    let app = router(relay).layer(MockConnectInfo(SocketAddr::from(([192, 0, 2, 7], 4000))));
    Harness {
        app,
        now,
        _dir: dir,
    }
}

fn b64(b: &[u8]) -> String {
    STANDARD.encode(b)
}

impl Harness {
    fn advance(&self, s: u64) {
        self.now.fetch_add(s, Ordering::SeqCst);
    }

    /// One call: `token` adds the bearer header, `format` the format header.
    async fn call(
        &self,
        method: &str,
        uri: &str,
        token: Option<&str>,
        format: Option<u32>,
        body: Option<Vec<u8>>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if let Some(f) = format {
            req = req.header(FORMAT_HEADER, f.to_string());
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn post(&self, uri: &str, token: Option<&str>, body: &Value) -> (StatusCode, Value) {
        self.call(
            "POST",
            uri,
            token,
            Some(1),
            Some(serde_json::to_vec(body).unwrap()),
        )
        .await
    }

    /// Registers device `n` with the setup secret and key check `check`; returns its token.
    async fn register(&self, n: u8, check: u8) -> String {
        let (s, v) = self
            .post(
                "/v1/devices",
                None,
                &json!({"device_id": b64(&[n; 16]), "key_check": b64(&[check; 32]),
                        "setup_secret": SECRET}),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["token"].as_str().unwrap().to_owned()
    }
}

fn row(key: u8, wall_ms: u64, logical: u32, body: &[u8]) -> Value {
    json!({"key": b64(&[key; 16]), "wall_ms": wall_ms, "logical": logical,
           "node": b64(&[9; 16]), "body": b64(body)})
}

fn error_of(v: &Value) -> &str {
    v["error"].as_str().unwrap_or("")
}

#[tokio::test]
async fn relay_health_answers_without_a_token() {
    let h = harness("health");
    let (s, v) = h.call("GET", "/v1/health", None, None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["ok"], true);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn relay_registers_with_the_secret_and_refuses_everything_else() {
    let h = harness("register");
    // Wrong secret, malformed fields, both or neither of secret and slot.
    let bad = [
        (
            json!({"device_id": b64(&[1; 16]), "key_check": b64(&[1; 32]),
                "setup_secret": "not-the-secret-at-all-no"}),
            StatusCode::FORBIDDEN,
            "bad_secret",
        ),
        (
            json!({"device_id": b64(&[1; 15]), "key_check": b64(&[1; 32]),
                "setup_secret": SECRET}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            json!({"device_id": b64(&[1; 16]), "key_check": b64(&[1; 32])}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            json!({"device_id": b64(&[1; 16]), "key_check": b64(&[1; 32]),
                "setup_secret": SECRET, "slot": b64(&[1; 16])}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            json!({"device_id": "not base64!", "key_check": b64(&[1; 32]),
                "setup_secret": SECRET}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
    ];
    for (body, status, word) in bad {
        let (s, v) = h.post("/v1/devices", None, &body).await;
        assert_eq!((s, error_of(&v)), (status, word), "{body}");
    }
    let (s, v) = h
        .call(
            "POST",
            "/v1/devices",
            None,
            None,
            Some(b"{not json".to_vec()),
        )
        .await;
    assert_eq!((s, error_of(&v)), (StatusCode::BAD_REQUEST, "bad_request"));

    let token = h.register(1, 0xC1).await;
    assert_eq!(token.len(), 64);
    let (s, v) = h.call("GET", "/v1/vault", Some(&token), None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["devices"].as_array().unwrap().len(), 1);
    // No token, or one the relay never issued.
    for t in [None, Some("0".repeat(64))] {
        let (s, v) = h.call("GET", "/v1/vault", t.as_deref(), None, None).await;
        assert_eq!(
            (s, error_of(&v)),
            (StatusCode::UNAUTHORIZED, "unknown_device")
        );
    }
    // The secret with another key: this is not the vault's key.
    let (s, v) = h
        .post(
            "/v1/devices",
            None,
            &json!({"device_id": b64(&[2; 16]), "key_check": b64(&[0xC2; 32]),
                    "setup_secret": SECRET}),
        )
        .await;
    assert_eq!((s, error_of(&v)), (StatusCode::CONFLICT, "key_mismatch"));
    // The same key: a second device the owner sets up with the secret.
    let second = h.register(2, 0xC1).await;
    assert_ne!(second, token);
    // A device registering again gets a new token; the old one stops working.
    let again = h.register(1, 0xC1).await;
    let (s, _) = h.call("GET", "/v1/vault", Some(&token), None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = h.call("GET", "/v1/vault", Some(&again), None, None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn relay_pairing_slot_gives_its_key_once_and_one_registration() {
    let h = harness("pairing");
    let a = h.register(1, 0xC1).await;
    let slot = b64(&[0x51; 16]);
    let (s, v) = h
        .post(
            "/v1/pairing",
            Some(&a),
            &json!({"slot": slot, "wrapped": b64(&[7; 73])}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["expires_in_s"], SLOT_TTL_S);
    // Only a registered device may offer a slot.
    let (s, _) = h
        .post(
            "/v1/pairing",
            None,
            &json!({"slot": slot, "wrapped": b64(&[7; 73])}),
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // A slot must be registered before it is taken: registering on it now fails.
    let join = json!({"device_id": b64(&[2; 16]), "key_check": b64(&[0xC1; 32]), "slot": slot});
    let (s, v) = h.post("/v1/devices", None, &join).await;
    assert_eq!((s, error_of(&v)), (StatusCode::NOT_FOUND, "no_slot"));
    // Taken once, the key comes back; a second take finds nothing.
    let (s, v) = h
        .post("/v1/pairing/take", None, &json!({"slot": slot}))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["wrapped"], b64(&[7; 73]));
    let (s, v) = h
        .post("/v1/pairing/take", None, &json!({"slot": slot}))
        .await;
    assert_eq!((s, error_of(&v)), (StatusCode::NOT_FOUND, "no_slot"));
    // The taken slot allows exactly one registration, with the vault's key.
    let wrong = json!({"device_id": b64(&[2; 16]), "key_check": b64(&[0xC2; 32]), "slot": slot});
    let (s, v) = h.post("/v1/devices", None, &wrong).await;
    assert_eq!((s, error_of(&v)), (StatusCode::CONFLICT, "key_mismatch"));
    let (s, v) = h.post("/v1/devices", None, &join).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = h.post("/v1/devices", None, &join).await;
    assert_eq!((s, error_of(&v)), (StatusCode::NOT_FOUND, "no_slot"));

    // A slot expires after its time, taken or not.
    let other = b64(&[0x52; 16]);
    h.post(
        "/v1/pairing",
        Some(&a),
        &json!({"slot": other, "wrapped": b64(&[8; 73])}),
    )
    .await;
    h.advance(SLOT_TTL_S);
    let (s, _) = h
        .post("/v1/pairing/take", None, &json!({"slot": other}))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn relay_keeps_the_newest_row_per_key_and_pages_by_sequence() {
    let h = harness("rows");
    let t = h.register(1, 0xC1).await;
    let push = |rows: Vec<Value>| json!({"format": 1, "rows": rows});
    let (s, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![row(1, 100, 0, b"one"), row(2, 100, 0, b"two")]),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        (v["accepted"].as_u64(), v["seq"].as_u64()),
        (Some(2), Some(2))
    );
    // The same stamp or an older one is superseded and changes nothing.
    let (_, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![row(1, 100, 0, b"same"), row(2, 99, 5, b"old")]),
        )
        .await;
    assert_eq!(v["accepted"], 0);
    assert_eq!(v["superseded"].as_array().unwrap().len(), 2);
    // A later stamp (by logical, and by node at an equal clock) replaces it with a new number.
    let mut later_node = row(2, 100, 0, b"two by a later node");
    later_node["node"] = json!(b64(&[0xFF; 16]));
    let (_, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![row(1, 100, 1, b"one again"), later_node]),
        )
        .await;
    assert_eq!(
        (v["accepted"].as_u64(), v["seq"].as_u64()),
        (Some(2), Some(4))
    );

    let (s, v) = h
        .call("GET", "/v1/rows?after=0", Some(&t), Some(1), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "one row per key");
    assert_eq!(rows[0]["seq"], 3);
    assert_eq!(rows[0]["body"], b64(b"one again"));
    assert_eq!(rows[1]["seq"], 4);
    assert_eq!(v["more"], false);
    // After the cursor: nothing new.
    let (_, v) = h
        .call("GET", "/v1/rows?after=4", Some(&t), Some(1), None)
        .await;
    assert_eq!(v["rows"].as_array().unwrap().len(), 0);

    // Paging by count.
    let many: Vec<Value> = (10..20).map(|k| row(k, 200, 0, b"x")).collect();
    h.post("/v1/rows", Some(&t), &push(many)).await;
    let (_, v) = h
        .call("GET", "/v1/rows?after=4&limit=3", Some(&t), Some(1), None)
        .await;
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [5, 6, 7]
    );
    assert_eq!(v["more"], true);
    // A limit out of range is clamped, not refused.
    let (s, v) = h
        .call("GET", "/v1/rows?after=0&limit=0", Some(&t), Some(1), None)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["rows"].as_array().unwrap().len(), 1);
    let (s, _) = h
        .call("GET", "/v1/rows?after=minus", Some(&t), Some(1), None)
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn relay_refuses_a_device_older_than_the_vault_format() {
    let h = harness("format");
    let t = h.register(1, 0xC1).await;
    // The format header is required on rows calls.
    let (s, v) = h.call("GET", "/v1/rows", Some(&t), None, None).await;
    assert_eq!((s, error_of(&v)), (StatusCode::BAD_REQUEST, "bad_request"));
    let body = serde_json::to_vec(&json!({"format": 2, "rows": [row(1, 1, 0, b"x")]})).unwrap();
    let (s, _) = h
        .call("POST", "/v1/rows", Some(&t), Some(2), Some(body))
        .await;
    assert_eq!(s, StatusCode::OK);
    for (method, body) in [
        ("GET", None),
        (
            "POST",
            Some(serde_json::to_vec(&json!({"format": 1, "rows": []})).unwrap()),
        ),
    ] {
        let (s, v) = h.call(method, "/v1/rows", Some(&t), Some(1), body).await;
        assert_eq!(
            (s, error_of(&v)),
            (StatusCode::CONFLICT, "update_needed"),
            "{method}"
        );
    }
    let (_, v) = h.call("GET", "/v1/rows", Some(&t), Some(2), None).await;
    assert_eq!(v["vault_format"], 2);
}

/// T12.93: envelope format 3 (the ignore on a speaker row). Once a format-3 device has pushed,
/// a format-2 device (0.4.0 to 0.4.2) is refused on both calls, so it can never write a speaker
/// row back without the ignore; the relay itself needed no change (it keeps the highest format).
#[tokio::test]
async fn relay_refuses_a_format_two_device_at_vault_three() {
    let h = harness("format3");
    let t = h.register(1, 0xC1).await;
    let push = |format: u32| serde_json::to_vec(&json!({"format": format, "rows": []})).unwrap();
    let (s, _) = h
        .call("POST", "/v1/rows", Some(&t), Some(2), Some(push(2)))
        .await;
    assert_eq!(s, StatusCode::OK, "a format-2 vault takes format 2");
    let body = serde_json::to_vec(&json!({"format": 3, "rows": [row(1, 1, 0, b"x")]})).unwrap();
    let (s, _) = h
        .call("POST", "/v1/rows", Some(&t), Some(3), Some(body))
        .await;
    assert_eq!(s, StatusCode::OK);
    for (method, body) in [("GET", None), ("POST", Some(push(2)))] {
        let (s, v) = h.call(method, "/v1/rows", Some(&t), Some(2), body).await;
        assert_eq!(
            (s, error_of(&v)),
            (StatusCode::CONFLICT, "update_needed"),
            "{method}"
        );
    }
    let (s, v) = h.call("GET", "/v1/rows", Some(&t), Some(3), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["vault_format"], 3);
}

/// T12.92: envelope format 4 (the final-pass run on a segment row). Once a format-4 device has
/// pushed, a format-3 device is refused on both calls, so it can never write a line back without
/// its run or the edit order; the relay itself needed no change (it keeps the highest format).
#[tokio::test]
async fn relay_refuses_a_format_three_device_at_vault_four() {
    let h = harness("format4");
    let t = h.register(1, 0xC1).await;
    let push = |format: u32| serde_json::to_vec(&json!({"format": format, "rows": []})).unwrap();
    let (s, _) = h
        .call("POST", "/v1/rows", Some(&t), Some(3), Some(push(3)))
        .await;
    assert_eq!(s, StatusCode::OK, "a format-3 vault takes format 3");
    let body = serde_json::to_vec(&json!({"format": 4, "rows": [row(1, 1, 0, b"x")]})).unwrap();
    let (s, _) = h
        .call("POST", "/v1/rows", Some(&t), Some(4), Some(body))
        .await;
    assert_eq!(s, StatusCode::OK);
    for (method, body) in [("GET", None), ("POST", Some(push(3)))] {
        let (s, v) = h.call(method, "/v1/rows", Some(&t), Some(3), body).await;
        assert_eq!(
            (s, error_of(&v)),
            (StatusCode::CONFLICT, "update_needed"),
            "{method}"
        );
    }
    let (s, v) = h.call("GET", "/v1/rows", Some(&t), Some(4), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["vault_format"], 4);
}

#[tokio::test]
async fn relay_refuses_oversized_rows_requests_and_presence() {
    let h = harness("limits");
    let t = h.register(1, 0xC1).await;
    let push = |rows: Vec<Value>| json!({"format": 1, "rows": rows});
    let (s, _) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![row(1, 1, 0, &vec![0; MAX_ROW_BODY])]),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "a row at the limit is taken");
    let (s, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![row(2, 1, 0, &vec![0; MAX_ROW_BODY + 1])]),
        )
        .await;
    assert_eq!(
        (s, error_of(&v)),
        (StatusCode::PAYLOAD_TOO_LARGE, "too_large")
    );
    let (s, v) = h
        .call(
            "POST",
            "/v1/rows",
            Some(&t),
            Some(1),
            Some(vec![b' '; MAX_REQUEST + 1]),
        )
        .await;
    assert_eq!(
        (s, error_of(&v)),
        (StatusCode::PAYLOAD_TOO_LARGE, "too_large")
    );
    let (s, v) = h
        .post(
            "/v1/presence",
            Some(&t),
            &json!({"blob": b64(&vec![1; MAX_PRESENCE + 1])}),
        )
        .await;
    assert_eq!(
        (s, error_of(&v)),
        (StatusCode::PAYLOAD_TOO_LARGE, "too_large")
    );
    // An empty body is not a row.
    let (s, _) = h
        .post("/v1/rows", Some(&t), &push(vec![row(3, 1, 0, b"")]))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn relay_presence_ages_by_its_own_clock_and_revoked_devices_leave() {
    let h = harness("presence");
    let a = h.register(1, 0xC1).await;
    let b = h.register(2, 0xC1).await;
    h.post("/v1/presence", Some(&a), &json!({"blob": b64(b"sealed-a")}))
        .await;
    h.advance(40);
    h.post("/v1/presence", Some(&b), &json!({"blob": b64(b"sealed-b")}))
        .await;
    h.advance(5);
    let (s, v) = h.call("GET", "/v1/presence", Some(&a), None, None).await;
    assert_eq!(s, StatusCode::OK);
    let devices = v["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2);
    let age = |id: u8| {
        devices
            .iter()
            .find(|d| d["device_id"] == b64(&[id; 16]))
            .unwrap()["age_s"]
            .as_u64()
            .unwrap()
    };
    assert_eq!((age(1), age(2)), (45, 5));
    // A device removes another: its token stops working and it leaves the lists.
    let (s, _) = h
        .post(
            "/v1/devices/revoke",
            Some(&a),
            &json!({"device_id": b64(&[2; 16])}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = h.call("GET", "/v1/presence", Some(&b), None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (_, v) = h.call("GET", "/v1/presence", Some(&a), None, None).await;
    assert_eq!(v["devices"].as_array().unwrap().len(), 1);
    let (_, v) = h.call("GET", "/v1/vault", Some(&a), None, None).await;
    assert_eq!(v["devices"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn relay_a_device_removed_by_another_keeps_its_rows_and_cannot_act() {
    // T11.6c: any device of the vault may remove any other (docs/sync-spec.md section 3).
    let h = harness("remove-other");
    let a = h.register(1, 0xC1).await;
    let b = h.register(2, 0xC1).await;
    let (s, _) = h
        .post(
            "/v1/rows",
            Some(&b),
            &json!({"format": 1, "rows": [row(9, 100, 0, b"sealed-by-b")]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    h.post("/v1/presence", Some(&b), &json!({"blob": b64(b"sealed-b")}))
        .await;
    let revoke = |id: u8| json!({"device_id": b64(&[id; 16])});
    let (s, v) = h.post("/v1/devices/revoke", Some(&a), &revoke(2)).await;
    assert_eq!((s, v), (StatusCode::OK, json!({})));
    // Every call of the removed device is refused, its own removal of another included.
    for (method, uri) in [
        ("GET", "/v1/vault"),
        ("GET", "/v1/presence"),
        ("GET", "/v1/rows"),
    ] {
        let (s, v) = h.call(method, uri, Some(&b), Some(1), None).await;
        assert_eq!(
            (s, error_of(&v)),
            (StatusCode::UNAUTHORIZED, "unknown_device"),
            "{uri}"
        );
    }
    let (s, _) = h.post("/v1/devices/revoke", Some(&b), &revoke(1)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = h.call("GET", "/v1/vault", Some(&a), None, None).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "the removed device could not remove the other"
    );
    // Its rows stay; its presence is gone.
    let (_, v) = h
        .call("GET", "/v1/rows?after=0", Some(&a), Some(1), None)
        .await;
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["body"], b64(b"sealed-by-b"));
    let (_, v) = h.call("GET", "/v1/presence", Some(&a), None, None).await;
    assert!(v["devices"].as_array().unwrap().is_empty(), "{v}");
    // Removing it again, or an id the relay does not know, changes nothing and revives nobody.
    for id in [2, 7] {
        let (s, _) = h.post("/v1/devices/revoke", Some(&a), &revoke(id)).await;
        assert_eq!(s, StatusCode::OK);
    }
    let (s, _) = h.call("GET", "/v1/vault", Some(&b), None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (_, v) = h.call("GET", "/v1/vault", Some(&a), None, None).await;
    let ids: Vec<&Value> = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| &d["device_id"])
        .collect();
    assert_eq!(ids, [&json!(b64(&[1; 16]))]);
}

#[tokio::test]
async fn relay_slows_an_address_after_ten_failures_for_a_minute() {
    let h = harness("limiter");
    let wrong = json!({"device_id": b64(&[1; 16]), "key_check": b64(&[1; 32]),
                       "setup_secret": "not-the-secret-at-all-no"});
    for _ in 0..10 {
        let (s, _) = h.post("/v1/devices", None, &wrong).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }
    // Even the right secret waits now, and so does taking a slot.
    let right = json!({"device_id": b64(&[1; 16]), "key_check": b64(&[1; 32]),
                       "setup_secret": SECRET});
    let (s, v) = h.post("/v1/devices", None, &right).await;
    assert_eq!(
        (s, error_of(&v)),
        (StatusCode::TOO_MANY_REQUESTS, "slow_down")
    );
    let (s, _) = h
        .post("/v1/pairing/take", None, &json!({"slot": b64(&[1; 16])}))
        .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    h.advance(60);
    let (s, _) = h.post("/v1/devices", None, &right).await;
    assert_eq!(s, StatusCode::OK);
}

#[test]
fn relay_config_needs_a_long_secret_and_never_prints_it() {
    let env = |secret: Option<&'static str>| {
        move |k: &str| match k {
            "EARSHOT_SYNC_SETUP_SECRET" => secret.map(str::to_owned),
            _ => None,
        }
    };
    let e = Config::from_env(env(None)).unwrap_err();
    assert!(e.contains("EARSHOT_SYNC_SETUP_SECRET"), "{e}");
    let short = "short-secret";
    let e = Config::from_env(env(Some("short-secret"))).unwrap_err();
    assert!(!e.contains(short), "{e}");
    let c = Config::from_env(env(Some(SECRET))).unwrap();
    assert_eq!(c.data, PathBuf::from("/data"));
    assert_eq!(c.listen, "0.0.0.0:8787".parse::<SocketAddr>().unwrap());
    assert!(!format!("{c:?}").contains(SECRET));
}

#[test]
fn relay_config_reads_the_secret_from_a_docker_secret_file() {
    let file_env = |k: &str| match k {
        "EARSHOT_SYNC_SETUP_SECRET_FILE" => Some("/run/secrets/earshot_sync_setup_secret".into()),
        _ => None,
    };
    // A Docker secret file usually ends in a newline: it is not part of the secret.
    let c = Config::from_env_reading(file_env, |p| {
        assert_eq!(
            p,
            std::path::Path::new("/run/secrets/earshot_sync_setup_secret")
        );
        Ok(format!("{SECRET}\n"))
    })
    .unwrap();
    assert_eq!(c.setup_secret.trim(), SECRET);
    // A file that cannot be read, or one too short: refused without its text.
    let e = Config::from_env_reading(file_env, |_| Err(std::io::ErrorKind::NotFound.into()))
        .unwrap_err();
    assert!(e.contains("EARSHOT_SYNC_SETUP_SECRET_FILE"), "{e}");
    let e = Config::from_env_reading(file_env, |_| Ok("short-in-file".into())).unwrap_err();
    assert!(!e.contains("short-in-file"), "{e}");
    // Both set is ambiguous.
    let both = |k: &str| match k {
        "EARSHOT_SYNC_SETUP_SECRET" => Some(SECRET.to_owned()),
        "EARSHOT_SYNC_SETUP_SECRET_FILE" => Some("/run/secrets/x".into()),
        _ => None,
    };
    assert!(Config::from_env_reading(both, |_| Ok(SECRET.into())).is_err());
}

#[test]
fn relay_health_check_connects_to_loopback_when_listening_everywhere() {
    use earshot_sync_server::health_target;
    let at = |s: &str| s.parse::<SocketAddr>().unwrap();
    assert_eq!(health_target(at("0.0.0.0:8787")), at("127.0.0.1:8787"));
    assert_eq!(health_target(at("[::]:8787")), at("[::1]:8787"));
    assert_eq!(health_target(at("192.0.2.4:9000")), at("192.0.2.4:9000"));
}

#[test]
fn relay_reopens_its_database_and_refuses_a_newer_one() {
    let dir = TempDir::new("reopen");
    let config = Config {
        setup_secret: SECRET.into(),
        data: dir.0.clone(),
        listen: "127.0.0.1:0".parse().unwrap(),
    };
    drop(Relay::open(&config).unwrap());
    drop(Relay::open(&config).unwrap());
    let written: u32 = rusqlite_open(&dir.0.join("relay.db"))
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        written, 2,
        "the schema this build writes (T14.45: the epoch)"
    );
    {
        let c = rusqlite_open(&dir.0.join("relay.db"));
        c.pragma_update(None, "user_version", written + 1).unwrap();
    }
    let e = Relay::open(&config).err().unwrap();
    assert_eq!(e.kind(), "newer");
}

fn rusqlite_open(p: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(p).unwrap()
}

/// The relay's database as every relay up to 0.4.2 made it (schema 1, copied from
/// `v0.4.2:crates/earshot-sync-server/src/db.rs`), holding a vault with two devices, three rows
/// and a pairing slot. Every value is invented; device 1's token is `TOKEN_V1`.
const RELAY_V1_SQL: &str = "
CREATE TABLE vault (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    format INTEGER NOT NULL,
    key_check BLOB,
    next_seq INTEGER NOT NULL
) STRICT;
INSERT INTO vault VALUES (1, 0, NULL, 1);
CREATE TABLE device (
    device_id BLOB PRIMARY KEY NOT NULL,
    token_sha256 BLOB NOT NULL UNIQUE,
    created_unix_s INTEGER NOT NULL,
    last_seen_unix_s INTEGER NOT NULL,
    presence BLOB,
    presence_unix_s INTEGER,
    revoked INTEGER NOT NULL
) STRICT;
CREATE TABLE row (
    key BLOB PRIMARY KEY NOT NULL,
    wall_ms INTEGER NOT NULL,
    logical INTEGER NOT NULL,
    node BLOB NOT NULL,
    seq INTEGER NOT NULL UNIQUE,
    device_id BLOB NOT NULL,
    body BLOB NOT NULL
) STRICT;
CREATE TABLE pairing (
    slot BLOB PRIMARY KEY NOT NULL,
    wrapped BLOB,
    expires_unix_s INTEGER NOT NULL,
    taken INTEGER NOT NULL
) STRICT;
UPDATE vault SET format = 2, key_check = zeroblob(32), next_seq = 6 WHERE id = 1;
INSERT INTO device VALUES (x'01010101010101010101010101010101', ?1, 1700000000, 1700000100,
    x'A1A2', 1700000050, 0);
INSERT INTO device VALUES (x'02020202020202020202020202020202', zeroblob(32), 1700000001,
    1700000101, NULL, NULL, 1);
INSERT INTO row VALUES (x'0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A0A', 100, 0,
    x'09090909090909090909090909090909', 2, x'01010101010101010101010101010101', x'6F6E65');
INSERT INTO row VALUES (x'0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B', 101, 0,
    x'09090909090909090909090909090909', 4, x'01010101010101010101010101010101', x'74776F');
INSERT INTO row VALUES (x'0C0C0C0C0C0C0C0C0C0C0C0C0C0C0C0C', 102, 3,
    x'09090909090909090909090909090909', 5, x'02020202020202020202020202020202', x'7468726565');
INSERT INTO pairing VALUES (x'0D0D0D0D0D0D0D0D0D0D0D0D0D0D0D0D', x'EE', 99999999999, 0);
PRAGMA user_version = 1;
";

const TOKEN_V1: &str = "an-invented-token-from-before-the-epoch";

/// A harness over the relay database in `dir` (opened, and so brought up to this build's
/// schema, here).
fn harness_in(dir: TempDir) -> Harness {
    let now = Arc::new(AtomicU64::new(1_800_000_000));
    let clock: Clock = {
        let now = Arc::clone(&now);
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let config = Config {
        setup_secret: SECRET.into(),
        data: dir.0.clone(),
        listen: "127.0.0.1:0".parse().unwrap(),
    };
    let relay = Relay::open_with_clock(&config, clock).unwrap();
    let app = router(relay).layer(MockConnectInfo(SocketAddr::from(([192, 0, 2, 7], 4000))));
    Harness {
        app,
        now,
        _dir: dir,
    }
}

fn relay_v1_dir(name: &str) -> TempDir {
    use sha2::Digest as _;
    let dir = TempDir::new(name);
    let c = rusqlite_open(&dir.0.join("relay.db"));
    let hash: [u8; 32] = sha2::Sha256::digest(TOKEN_V1.as_bytes()).into();
    // The one statement with a parameter is run by itself; the rest as a batch.
    for statement in RELAY_V1_SQL.split(';') {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }
        if statement.contains("?1") {
            c.execute(statement, [&hash[..]]).unwrap();
        } else {
            c.execute_batch(statement).unwrap();
        }
    }
    dir
}

/// T14.45: a database from a relay before the epoch (schema 1, with devices, rows and a slot
/// in it) is brought to schema 2 on opening: the vault gets an epoch, nothing else changes, the
/// devices' tokens still work, and the epoch is the same after a restart.
#[tokio::test]
async fn relay_gives_a_vault_from_before_the_epoch_one_and_keeps_it_across_restarts() {
    let dir = relay_v1_dir("epoch-migrate");
    let db = dir.0.join("relay.db");
    let dump = |p: &std::path::Path| -> Vec<String> {
        let c = rusqlite_open(p);
        let mut out = Vec::new();
        for (table, cols) in [
            ("vault", "id, format, hex(key_check), next_seq"),
            (
                "device",
                "hex(device_id), hex(token_sha256), created_unix_s, hex(presence), \
                 presence_unix_s, revoked",
            ),
            (
                "row",
                "hex(key), wall_ms, logical, hex(node), seq, hex(device_id), hex(body)",
            ),
            ("pairing", "hex(slot), hex(wrapped), expires_unix_s, taken"),
        ] {
            let width = cols.split(", ").count();
            let mut st = c
                .prepare(&format!("SELECT {cols} FROM {table} ORDER BY 1"))
                .unwrap();
            let rows = st
                .query_map([], |r| {
                    Ok((0..width)
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect::<Vec<_>>()
                        .join("|"))
                })
                .unwrap();
            for row in rows {
                out.push(format!("{table}: {}", row.unwrap()));
            }
        }
        out
    };
    let before = dump(&db);
    assert_eq!(before.len(), 1 + 2 + 3 + 1);

    let h = harness_in(dir);
    // Everything that was there is there, unchanged; the version is this build's.
    assert_eq!(dump(&db), before);
    let c = rusqlite_open(&db);
    let version: u32 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
    let stored: Vec<u8> = c
        .query_row("SELECT epoch FROM vault WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored.len(), 16);
    assert_ne!(stored, vec![0u8; 16], "a random epoch, not a blank one");
    drop(c);

    // The old token works; the pull carries the rows, the epoch and the highest number.
    let (s, v) = h
        .call("GET", "/v1/rows?after=0", Some(TOKEN_V1), Some(2), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let seqs: Vec<u64> = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, [2, 4, 5]);
    assert_eq!(v["epoch"], b64(&stored));
    assert_eq!(v["max_seq"], 5);
    assert_eq!(v["vault_format"], 2);
    // A push answers with the same epoch and numbers on from where the old relay stopped.
    let (s, v) = h
        .call(
            "POST",
            "/v1/rows",
            Some(TOKEN_V1),
            Some(2),
            Some(
                serde_json::to_vec(&json!({"format": 2, "rows": [row(0x1F, 200, 0, b"new")]}))
                    .unwrap(),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        (v["seq"].as_u64(), v["max_seq"].as_u64()),
        (Some(6), Some(5))
    );
    assert_eq!(v["epoch"], b64(&stored));

    // A restart (the same data folder opened again) keeps the epoch.
    let Harness { app, now, _dir } = h;
    drop((app, now));
    let h = harness_in(_dir);
    let (_, v) = h
        .call("GET", "/v1/rows?after=6", Some(TOKEN_V1), Some(2), None)
        .await;
    assert_eq!(v["epoch"], b64(&stored), "the epoch survives a restart");
    assert_eq!(v["max_seq"], 6);
}

/// T14.45: a device that has seen a sequence number the vault has not reached proves the relay
/// lost rows (it was restored from an older copy). The relay then takes a new epoch and moves
/// its numbering past what the device saw, once: the same claim again changes nothing.
#[tokio::test]
async fn relay_restored_from_an_older_copy_takes_a_new_epoch_and_numbers_past_what_was_seen() {
    let h = harness("epoch-rollback");
    let t = h.register(1, 0xC1).await;
    let push = |rows: Vec<Value>, seen: Option<u64>, epoch: Option<&str>| {
        let mut body = json!({"format": 1, "rows": rows});
        if let Some(seen) = seen {
            body["seen_seq"] = json!(seen);
        }
        if let Some(epoch) = epoch {
            body["epoch"] = json!(epoch);
        }
        body
    };
    let (_, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![row(1, 100, 0, b"a"), row(2, 100, 0, b"b")], None, None),
        )
        .await;
    let first = v["epoch"].as_str().unwrap().to_owned();
    assert_eq!(STANDARD.decode(&first).unwrap().len(), 16);
    assert_eq!(
        (v["seq"].as_u64(), v["max_seq"].as_u64()),
        (Some(2), Some(0))
    );

    // Nothing lost: what the device has seen is what the vault holds. Nothing changes.
    let (_, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![row(3, 100, 0, b"c")], Some(2), Some(&first)),
        )
        .await;
    assert_eq!(v["epoch"], first.as_str());
    assert_eq!(
        (v["seq"].as_u64(), v["max_seq"].as_u64()),
        (Some(3), Some(2))
    );
    let (_, v) = h
        .call(
            "GET",
            &format!(
                "/v1/rows?after=3&seen=3&epoch={}",
                first
                    .replace('+', "%2B")
                    .replace('=', "%3D")
                    .replace('/', "%2F")
            ),
            Some(&t),
            Some(1),
            None,
        )
        .await;
    assert_eq!(v["epoch"], first.as_str());
    assert_eq!(v["max_seq"], 3);

    // The device has seen number 40, with this epoch: the relay holds less than it once did.
    let (s, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![row(4, 100, 0, b"d")], Some(40), Some(&first)),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let second = v["epoch"].as_str().unwrap().to_owned();
    assert_ne!(second, first, "a new epoch");
    assert_eq!(
        v["max_seq"], 3,
        "the highest number before the claim was heard"
    );
    assert_eq!(
        v["seq"], 41,
        "the pushed row is numbered past what the device saw"
    );
    // The same claim again, from a device that still holds the old epoch (another device of
    // the vault, or this one before it took the new epoch in): nothing more happens.
    let (_, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![row(5, 100, 0, b"e")], Some(40), Some(&first)),
        )
        .await;
    assert_eq!(v["epoch"], second.as_str());
    assert_eq!(
        (v["seq"].as_u64(), v["max_seq"].as_u64()),
        (Some(42), Some(41))
    );
    // A device that holds the old epoch and saw even more, long after: nothing changes for the
    // vault (that device saw another history and reads from the start by itself when it sees
    // this epoch; the devices of this epoch were told when it was made).
    h.advance(3_600);
    let (_, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![row(6, 100, 0, b"f")], Some(60), Some(&first)),
        )
        .await;
    assert_eq!(v["epoch"], second.as_str());
    assert_eq!(
        (v["seq"].as_u64(), v["max_seq"].as_u64()),
        (Some(43), Some(42))
    );
    // The rows are all there, in order, and a pull says the same epoch.
    let (_, v) = h
        .call("GET", "/v1/rows?after=0", Some(&t), Some(1), None)
        .await;
    let seqs: Vec<u64> = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, [1, 2, 3, 41, 42, 43]);
    assert_eq!(v["epoch"], second.as_str());
    assert_eq!(v["max_seq"], 43);
}

fn seqs_of(v: &Value) -> Vec<u64> {
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["seq"].as_u64().unwrap())
        .collect()
}

/// T14.45 review: one restore is one epoch. After a device of the epoch has made the relay take
/// a new one, the rows filed since (its own, sent again, and another device's) are numbered
/// from where that device's cursor was. A client from before the epoch (it says only where
/// its cursor is, and its cursor only moves forward) would never pull those numbered at or
/// below its own cursor, whether its cursor is above everything the vault holds or the
/// vault's numbers have passed it meanwhile. The relay takes no further epoch for it and
/// makes it wait for nothing: at its first pull since the epoch those rows are filed again
/// above everything, in order, so the pull brings every row filed since the restore. That
/// happens once for each such device in an epoch. A device of the epoch sees the same epoch
/// and a higher number, nothing to start over for.
#[tokio::test]
async fn relay_files_rows_again_for_old_clients_after_a_restore_without_a_new_epoch() {
    let h = harness("epoch-refile");
    // A and B know of epochs; C, D and E are clients from before them (they name none).
    let a = h.register(1, 0xC1).await;
    let b = h.register(2, 0xC1).await;
    let c = h.register(3, 0xC1).await;
    let d = h.register(4, 0xC1).await;
    let e = h.register(5, 0xC1).await;
    let plain = |rows: Vec<Value>| json!({"format": 1, "rows": rows});
    let pull = |token: &str, after: u64| {
        let (h, token) = (&h, token.to_owned());
        async move {
            let (s, v) = h
                .call(
                    "GET",
                    &format!("/v1/rows?after={after}&limit=200"),
                    Some(&token),
                    Some(1),
                    None,
                )
                .await;
            assert_eq!(s, StatusCode::OK, "{v}");
            v
        }
    };
    let bodies = |v: &Value| -> Vec<String> {
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["body"].as_str().unwrap().to_owned())
            .collect()
    };
    // The relay as restored: three rows, numbers 1 to 3. Every device has called before.
    let (_, v) = h
        .post(
            "/v1/rows",
            Some(&a),
            &plain(vec![
                row(1, 100, 0, b"a"),
                row(2, 100, 0, b"b"),
                row(3, 100, 0, b"c"),
            ]),
        )
        .await;
    let first = v["epoch"].as_str().unwrap().to_owned();
    for token in [&b, &c, &d] {
        assert_eq!(seqs_of(&pull(token, 0).await), [1, 2, 3]);
    }
    // A had seen number 100: a new epoch, and its two rows land from 101.
    let mut again = plain(vec![
        row(10, 100, 0, b"lost one"),
        row(11, 100, 0, b"lost two"),
    ]);
    again["seen_seq"] = json!(100);
    again["epoch"] = json!(first);
    let (_, v) = h.post("/v1/rows", Some(&a), &again).await;
    let second = v["epoch"].as_str().unwrap().to_owned();
    assert_ne!(second, first);
    assert_eq!(
        (v["seq"].as_u64(), v["max_seq"].as_u64()),
        (Some(102), Some(3))
    );

    // Half a minute later, well inside the ten minutes in which no further epoch is taken, B
    // (which holds the old epoch) sends its rows again: they land from 103, past C's cursor.
    h.advance(30);
    let mut from_b = plain(vec![
        row(20, 100, 0, b"b one"),
        row(21, 100, 0, b"b two"),
        row(22, 100, 0, b"b three"),
    ]);
    from_b["seen_seq"] = json!(130);
    from_b["epoch"] = json!(first);
    let (_, v) = h.post("/v1/rows", Some(&b), &from_b).await;
    assert_eq!(v["epoch"], second.as_str(), "B starts over by itself");
    assert_eq!(v["seq"], 105);

    // C, a client from before the epoch whose cursor is 104: the vault's numbers have passed
    // it (105), so it shows nothing by being above them. It is served all the same, at its
    // first pull since the epoch, with no wait and no new epoch: the four rows filed since
    // the restore at or below its cursor are filed again above everything, in their order,
    // and it is sent every row filed since.
    let v = pull(&c, 104).await;
    assert_eq!(v["epoch"], second.as_str(), "one restore, one epoch");
    assert_eq!(seqs_of(&v), [105, 106, 107, 108, 109]);
    assert_eq!(
        bodies(&v),
        [
            b"b three" as &[u8],
            b"lost one",
            b"lost two",
            b"b one",
            b"b two"
        ]
        .map(b64)
    );
    assert_eq!(v["max_seq"], 105);
    // D, another such client with a cursor above everything (150), served at once too: the
    // rows are moved above its cursor.
    let v = pull(&d, 150).await;
    assert_eq!(v["epoch"], second.as_str());
    assert_eq!(seqs_of(&v), [151, 152, 153, 154, 155]);
    assert_eq!(
        bodies(&v),
        [
            b"b three" as &[u8],
            b"lost one",
            b"lost two",
            b"b one",
            b"b two"
        ]
        .map(b64)
    );
    // Each is served once in an epoch: the same pull again moves nothing (a device cannot
    // make the relay renumber its rows over and over), and what was filed again for D is
    // further on for C.
    for _ in 0..3 {
        let v = pull(&d, 150).await;
        assert_eq!(
            (seqs_of(&v), v["max_seq"].as_u64()),
            (vec![151, 152, 153, 154, 155], Some(155))
        );
        let v = pull(&c, 109).await;
        assert_eq!(
            (seqs_of(&v), v["max_seq"].as_u64()),
            (vec![151, 152, 153, 154, 155], Some(155))
        );
    }
    // E, which had read nothing yet, needs nothing moved.
    let v = pull(&e, 0).await;
    assert_eq!(seqs_of(&v), [1, 2, 3, 151, 152, 153, 154, 155]);
    assert_eq!(v["max_seq"], 155);

    // The devices of the epoch see the same epoch and a higher highest number than they saw:
    // nothing to start over for. A's new row is numbered on from there.
    let mut more = plain(vec![row(12, 100, 0, b"new")]);
    more["seen_seq"] = json!(102);
    more["epoch"] = json!(second);
    let (_, v) = h.post("/v1/rows", Some(&a), &more).await;
    assert_eq!(v["epoch"], second.as_str());
    assert_eq!(
        (v["seq"].as_u64(), v["max_seq"].as_u64()),
        (Some(156), Some(155))
    );
    // A served old client that later shows a cursor above the vault's rows saw numbers of
    // this epoch that the vault no longer holds: a second restore, and (the ten minutes being
    // over) a new epoch.
    h.advance(601);
    let v = pull(&c, 400).await;
    assert_ne!(v["epoch"], second.as_str());
    assert_eq!(v["max_seq"], 156);
}

/// T14.45 review: the relay acts on a claim at most once in ten minutes per vault, so a device
/// that sends ever higher claims cannot make every other device send its store again, or the
/// relay renumber its rows, over and over. Inside the limit a claim changes nothing at all;
/// after it the next one is acted on.
#[tokio::test]
async fn relay_acts_on_claims_at_most_once_in_ten_minutes() {
    let h = harness("epoch-limit");
    let t = h.register(1, 0xC1).await;
    let claim =
        |seen: u64, epoch: &str| json!({"format": 1, "rows": [], "seen_seq": seen, "epoch": epoch});
    let (_, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &json!({"format": 1, "rows": [row(1, 100, 0, b"a")]}),
        )
        .await;
    let first = v["epoch"].as_str().unwrap().to_owned();
    // The first claim above the vault's rows: a new epoch.
    let (_, v) = h.post("/v1/rows", Some(&t), &claim(50, &first)).await;
    let second = v["epoch"].as_str().unwrap().to_owned();
    assert_ne!(second, first);
    assert_eq!(
        (v["seq"].as_u64(), v["max_seq"].as_u64()),
        (Some(50), Some(1))
    );
    // Ever higher claims naming the current epoch, for the next ten minutes: nothing changes,
    // not the epoch and not the numbering.
    for (step, seen) in [(1u64, 1_000u64), (300, 2_000), (298, 3_000)] {
        h.advance(step);
        let (s, v) = h.post("/v1/rows", Some(&t), &claim(seen, &second)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["epoch"], second.as_str(), "inside the limit: {seen}");
        assert_eq!(
            (v["seq"].as_u64(), v["max_seq"].as_u64()),
            (Some(50), Some(50))
        );
        // An old client's cursor above the rows is not acted on either.
        let (_, v) = h
            .call(
                "GET",
                &format!("/v1/rows?after={seen}"),
                Some(&t),
                Some(1),
                None,
            )
            .await;
        assert_eq!(v["epoch"], second.as_str());
        assert_eq!(v["max_seq"], 50);
    }
    // Ten minutes after the claim that was acted on: the next one is.
    h.advance(1);
    let (_, v) = h.post("/v1/rows", Some(&t), &claim(4_000, &second)).await;
    let third = v["epoch"].as_str().unwrap().to_owned();
    assert_ne!(third, second);
    assert_eq!(
        (v["seq"].as_u64(), v["max_seq"].as_u64()),
        (Some(4_000), Some(50))
    );
    assert_eq!(earshot_sync_server::CLAIM_EVERY_S, 600);
}

/// T14.45: a client from before the epoch says only where its cursor is. A cursor past
/// everything the vault holds is the same proof: a new epoch and numbering past the cursor,
/// once. Its next pulls from the same cursor change nothing, and rows pushed afterwards are
/// numbered above the cursor, so even that client pulls them.
#[tokio::test]
async fn relay_hears_an_old_clients_cursor_past_its_rows_once() {
    let h = harness("epoch-old-client");
    let t = h.register(1, 0xC1).await;
    let (_, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &json!({"format": 1, "rows": [row(1, 100, 0, b"a")]}),
        )
        .await;
    let first = v["epoch"].as_str().unwrap().to_owned();
    // A cursor at the vault's highest number is no proof.
    let (_, v) = h
        .call("GET", "/v1/rows?after=1", Some(&t), Some(1), None)
        .await;
    assert_eq!(v["epoch"], first.as_str());
    // A cursor past it is.
    let (s, v) = h
        .call("GET", "/v1/rows?after=25", Some(&t), Some(1), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let second = v["epoch"].as_str().unwrap().to_owned();
    assert_ne!(second, first);
    assert_eq!(v["rows"].as_array().unwrap().len(), 0);
    assert_eq!(v["max_seq"], 1);
    for _ in 0..3 {
        let (_, v) = h
            .call("GET", "/v1/rows?after=25", Some(&t), Some(1), None)
            .await;
        assert_eq!(
            v["epoch"],
            second.as_str(),
            "the same cursor is not news twice"
        );
        assert_eq!(v["max_seq"], 25);
    }
    // A row pushed now is above that cursor.
    let (_, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &json!({"format": 1, "rows": [row(2, 100, 0, b"b")]}),
        )
        .await;
    assert_eq!(v["seq"], 26);
    let (_, v) = h
        .call("GET", "/v1/rows?after=25", Some(&t), Some(1), None)
        .await;
    assert_eq!(v["rows"].as_array().unwrap().len(), 1);
    assert_eq!(v["rows"][0]["seq"], 26);

    // An epoch is 16 bytes.
    let (s, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &json!({"format": 1, "rows": [], "epoch": b64(&[1; 8])}),
        )
        .await;
    assert_eq!((s, error_of(&v)), (StatusCode::BAD_REQUEST, "bad_request"));
}

/// T14.45 review: no claim can put the vault's numbering out of reach. A device that says it
/// has seen the largest number there is (a fault, or a token in the wrong hands) is not
/// refused, and is not followed further than `MAX_CLAIM_JUMP` above the vault's own rows:
/// every other device goes on pushing and pulling, with numbers nowhere near what the
/// database can count to, and what they then say they have seen is accepted in turn.
#[tokio::test]
async fn relay_a_hostile_claim_leaves_the_vault_usable() {
    let h = harness("epoch-hostile");
    let hostile = h.register(1, 0xC1).await;
    let honest = h.register(2, 0xC1).await;
    let jump = u64::try_from(earshot_sync_server::MAX_CLAIM_JUMP).unwrap();
    assert_eq!(jump, 1 << 30);
    let (_, v) = h
        .post(
            "/v1/rows",
            Some(&honest),
            &json!({"format": 1, "rows": [row(1, 100, 0, b"a")]}),
        )
        .await;
    let first = v["epoch"].as_str().unwrap().to_owned();
    // The largest number there is, on a push and on a pull, with and without the epoch.
    for body in [
        json!({"format": 1, "rows": [], "seen_seq": u64::MAX, "epoch": first}),
        json!({"format": 1, "rows": [], "seen_seq": u64::MAX}),
        json!({"format": 1, "rows": [], "seen_seq": i64::MAX as u64}),
    ] {
        let (s, v) = h.post("/v1/rows", Some(&hostile), &body).await;
        assert_eq!(s, StatusCode::OK, "not refused: {v}");
        assert!(v["seq"].as_u64().unwrap() <= 1 + jump, "{v}");
    }
    for uri in [
        format!("/v1/rows?after={}", u64::MAX),
        format!("/v1/rows?after=0&seen={}", u64::MAX),
    ] {
        let (s, v) = h.call("GET", &uri, Some(&hostile), Some(1), None).await;
        assert_eq!(s, StatusCode::OK, "not refused: {v}");
    }
    // The other device goes on: its row is stored, numbered at most the jump above the rows,
    // and its pull brings both rows.
    let (s, v) = h
        .post(
            "/v1/rows",
            Some(&honest),
            &json!({"format": 1, "rows": [row(2, 100, 0, b"b")]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let seq = v["seq"].as_u64().unwrap();
    assert_eq!(
        seq,
        1 + jump + 1,
        "one jump past the vault's one row, then this row"
    );
    let epoch = v["epoch"].as_str().unwrap().to_owned();
    assert_ne!(
        epoch, first,
        "the first claim was taken for a restore: one new epoch"
    );
    let (_, v) = h
        .call("GET", "/v1/rows?after=0", Some(&honest), Some(1), None)
        .await;
    assert_eq!(seqs_of(&v), [1, seq]);
    // What the honest device has now seen is a number it may send back: it is accepted, and
    // changes nothing.
    let (s, v) = h
        .post(
            "/v1/rows",
            Some(&honest),
            &json!({"format": 1, "rows": [row(3, 100, 0, b"c")], "seen_seq": seq, "epoch": epoch}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        (v["seq"].as_u64(), v["epoch"].as_str()),
        (Some(seq + 1), Some(epoch.as_str()))
    );
    // More of the same from the hostile device, at once and later: at most one more jump each
    // ten minutes, never a refusal, and the honest device is never stopped.
    for round in 0..3u64 {
        h.advance(601);
        let (s, _) = h
            .post(
                "/v1/rows",
                Some(&hostile),
                &json!({"format": 1, "rows": [], "seen_seq": u64::MAX, "epoch": epoch}),
            )
            .await;
        assert_eq!(s, StatusCode::OK);
        let (s, v) = h
            .post(
                "/v1/rows",
                Some(&honest),
                &json!({"format": 1, "rows": [row(10 + u8::try_from(round).unwrap(), 100, 0, b"d")]}),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert!(v["seq"].as_u64().unwrap() < (round + 3) * (jump + 8), "{v}");
    }
    assert!(
        (jump + 8) * 6 < u64::MAX / 1_000_000_000,
        "far from the top"
    );
}

#[tokio::test]
async fn relay_superseded_rows_leave_the_stored_row_and_its_number_alone() {
    let h = harness("superseded");
    let t = h.register(1, 0xC1).await;
    let push = |rows: Vec<Value>| json!({"format": 1, "rows": rows});
    h.post(
        "/v1/rows",
        Some(&t),
        &push(vec![row(1, 100, 0, b"one"), row(2, 100, 0, b"two")]),
    )
    .await;
    h.post(
        "/v1/rows",
        Some(&t),
        &push(vec![row(1, 100, 0, b"same"), row(2, 99, 5, b"old")]),
    )
    .await;
    let (_, v) = h
        .call("GET", "/v1/rows?after=0", Some(&t), Some(1), None)
        .await;
    let rows = v["rows"].as_array().unwrap();
    let got: Vec<(u64, &str)> = rows
        .iter()
        .map(|r| (r["seq"].as_u64().unwrap(), r["body"].as_str().unwrap()))
        .collect();
    let (one, two) = (b64(b"one"), b64(b"two"));
    assert_eq!(got, [(1, one.as_str()), (2, two.as_str())]);
}

#[tokio::test]
async fn relay_pull_stops_at_the_byte_cap_but_always_sends_a_row() {
    let h = harness("bytecap");
    let t = h.register(1, 0xC1).await;
    // 17 rows at the row limit: 4.25 MiB of bodies, past the 4 MiB page.
    for batch in 0..6u8 {
        let rows: Vec<Value> = (0..3u8)
            .map(|i| batch * 3 + i)
            .filter(|k| *k < 17)
            .map(|k| row(k + 1, 1, 0, &vec![k; MAX_ROW_BODY]))
            .collect();
        let (s, v) = h
            .post("/v1/rows", Some(&t), &json!({"format": 1, "rows": rows}))
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    let (_, v) = h
        .call("GET", "/v1/rows?after=0&limit=500", Some(&t), Some(1), None)
        .await;
    let n = v["rows"].as_array().unwrap().len();
    assert_eq!(
        n,
        MAX_REQUEST / MAX_ROW_BODY,
        "16 rows fill the page exactly"
    );
    assert_eq!(v["more"], true);
    let (_, v) = h
        .call(
            "GET",
            "/v1/rows?after=16&limit=500",
            Some(&t),
            Some(1),
            None,
        )
        .await;
    assert_eq!(v["rows"].as_array().unwrap().len(), 1);
    assert_eq!(v["more"], false);
}

#[tokio::test]
async fn relay_push_must_announce_the_format_it_carries() {
    let h = harness("format-match");
    let t = h.register(1, 0xC1).await;
    // A body format above the header would raise the vault past what the device writes.
    let body = serde_json::to_vec(&json!({"format": 4_000_000_000u32, "rows": []})).unwrap();
    let (s, v) = h
        .call("POST", "/v1/rows", Some(&t), Some(1), Some(body))
        .await;
    assert_eq!((s, error_of(&v)), (StatusCode::BAD_REQUEST, "bad_request"));
    let (_, v) = h.call("GET", "/v1/rows", Some(&t), Some(1), None).await;
    assert_eq!(v["vault_format"], 0, "the vault's format did not move");
}

/// T14.44: a push carrying a stamp more than a day past the relay's own clock is refused whole
/// with a fixed word and stores nothing, so a row from a far future never shadows later edits
/// from computers whose clocks are right. At the limit it is accepted, and so is the same row
/// once the relay's clock has caught up.
#[tokio::test]
async fn relay_refuses_a_push_stamped_more_than_a_day_past_its_clock() {
    let h = harness("clock-ahead");
    let t = h.register(1, 0xC1).await;
    let now_ms = h.now.load(Ordering::SeqCst) * 1000;
    let limit = now_ms + MAX_STAMP_AHEAD_MS;
    assert_eq!(MAX_STAMP_AHEAD_MS, 24 * 60 * 60 * 1000);
    let push = |rows: Vec<Value>| json!({"format": 1, "rows": rows});
    // One row of the push is too far ahead: none of its rows is stored.
    let (s, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![
                row(1, now_ms, 0, b"now"),
                row(2, limit + 1, 0, b"far ahead"),
            ]),
        )
        .await;
    assert_eq!((s, error_of(&v)), (StatusCode::CONFLICT, "clock_ahead"));
    assert_eq!(
        v,
        json!({"error": "clock_ahead"}),
        "the word and nothing else"
    );
    let (_, v) = h
        .call("GET", "/v1/rows?after=0", Some(&t), Some(1), None)
        .await;
    assert_eq!(v["rows"].as_array().unwrap().len(), 0, "nothing was stored");
    // Exactly at the limit, and anything earlier, is accepted.
    let (s, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![
                row(1, now_ms, 0, b"now"),
                row(3, limit, 0, b"at the limit"),
            ]),
        )
        .await;
    assert_eq!(
        (s, v["accepted"].as_u64()),
        (StatusCode::OK, Some(2)),
        "{v}"
    );
    // The relay's clock moves on a second: the refused row is now inside the window.
    h.advance(1);
    let (s, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![row(2, limit + 1, 0, b"far ahead")]),
        )
        .await;
    assert_eq!(
        (s, v["accepted"].as_u64()),
        (StatusCode::OK, Some(1)),
        "{v}"
    );
    // A stamp at the far end of what the wire can carry is refused the same way, not as a
    // malformed request (an older client treats either as a failed push and keeps its rows).
    let (s, v) = h
        .post(
            "/v1/rows",
            Some(&t),
            &push(vec![row(4, i64::MAX as u64, 0, b"the far end")]),
        )
        .await;
    assert_eq!((s, error_of(&v)), (StatusCode::CONFLICT, "clock_ahead"));
}

/// Offers and takes pairing slot `n` from device `token`; returns the slot to register with.
async fn taken_slot(h: &Harness, token: &str, n: u8) -> String {
    let slot = b64(&[n; 16]);
    let (s, _) = h
        .post(
            "/v1/pairing",
            Some(token),
            &json!({"slot": slot, "wrapped": b64(&[7; 73])}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = h
        .post("/v1/pairing/take", None, &json!({"slot": slot}))
        .await;
    assert_eq!(s, StatusCode::OK);
    slot
}

/// A pairing code never takes over a live device (T11.2). Changed by the owner on 2026-11-16
/// (T11.6c): a revoked device comes back with a valid code; before, a code refused it too.
#[tokio::test]
async fn relay_pairing_code_never_takes_over_a_live_device_but_brings_back_a_removed_one() {
    let h = harness("takeover");
    let a = h.register(1, 0xC1).await;
    let b = h.register(2, 0xC1).await;
    let join = |id: u8, slot: &str| json!({"device_id": b64(&[id; 16]), "key_check": b64(&[0xC1; 32]), "slot": slot});
    // A live device: refused, and nothing changes (the code stays usable for its own device).
    let slot = taken_slot(&h, &a, 0x61).await;
    let (s, v) = h.post("/v1/devices", None, &join(1, &slot)).await;
    assert_eq!((s, error_of(&v)), (StatusCode::CONFLICT, "device_exists"));
    let (s, _) = h.call("GET", "/v1/vault", Some(&a), None, None).await;
    assert_eq!(s, StatusCode::OK, "device 1 keeps its token");

    h.post("/v1/presence", Some(&b), &json!({"blob": b64(b"sealed-b")}))
        .await;
    h.post(
        "/v1/devices/revoke",
        Some(&a),
        &json!({"device_id": b64(&[2; 16])}),
    )
    .await;
    // A revoked device with a code that is wrong, never taken, or used: refused, still revoked.
    let (s, v) = h
        .post("/v1/devices", None, &join(2, &b64(&[0x70; 16])))
        .await;
    assert_eq!((s, error_of(&v)), (StatusCode::NOT_FOUND, "no_slot"));
    let untaken = b64(&[0x71; 16]);
    h.post(
        "/v1/pairing",
        Some(&a),
        &json!({"slot": untaken, "wrapped": b64(&[7; 73])}),
    )
    .await;
    let (s, _) = h.post("/v1/devices", None, &join(2, &untaken)).await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "a slot not taken registers nobody"
    );
    let (s, _) = h.call("GET", "/v1/vault", Some(&b), None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "device 2 stays revoked");
    let (_, v) = h.call("GET", "/v1/vault", Some(&a), None, None).await;
    assert_eq!(v["devices"].as_array().unwrap().len(), 1);

    // A revoked device with a valid code: registered again with a fresh token; the old token
    // stays dead and its presence starts empty.
    let slot = taken_slot(&h, &a, 0x62).await;
    let (s, v) = h.post("/v1/devices", None, &join(2, &slot)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let fresh = v["token"].as_str().unwrap().to_owned();
    assert_ne!(fresh, b);
    let (s, _) = h.call("GET", "/v1/vault", Some(&b), None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "the old token stays dead");
    let (s, v) = h.call("GET", "/v1/vault", Some(&fresh), None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["devices"].as_array().unwrap().len(), 2);
    let (_, v) = h.call("GET", "/v1/presence", Some(&a), None, None).await;
    assert!(v["devices"].as_array().unwrap().is_empty(), "{v}");
    // The code is used up: it cannot bring it back a second time, and the device is live now.
    let (s, _) = h.post("/v1/devices", None, &join(2, &slot)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let slot = taken_slot(&h, &a, 0x63).await;
    let (s, v) = h.post("/v1/devices", None, &join(2, &slot)).await;
    assert_eq!((s, error_of(&v)), (StatusCode::CONFLICT, "device_exists"));
    let (s, _) = h.call("GET", "/v1/vault", Some(&fresh), None, None).await;
    assert_eq!(s, StatusCode::OK, "a live device is never registered over");
}

#[tokio::test]
async fn relay_answers_unknown_calls_with_a_fixed_word() {
    let h = harness("notfound");
    for (method, uri) in [("GET", "/v1/nothing"), ("GET", "/"), ("DELETE", "/v1/rows")] {
        let (s, v) = h.call(method, uri, None, None, None).await;
        assert_eq!(
            (s, error_of(&v)),
            (StatusCode::NOT_FOUND, "not_found"),
            "{method} {uri}"
        );
    }
    // Every word round-trips and carries its status.
    for w in earshot_sync::wire::ErrorWord::ALL {
        assert_eq!(earshot_sync::wire::ErrorWord::from_word(w.word()), Some(w));
        assert!(StatusCode::from_u16(w.status()).unwrap().is_client_error() || w.status() == 500);
    }
}

/// T12.77b8: a format-1 pull racing the format-2 push that raises the vault either sees the
/// vault as it was before the push (none of its rows) or is refused `update_needed`; it never
/// receives a row of the push (the format is read in the pull's own transaction).
///
/// The interleaving cannot be forced from outside: the format check and the row read happen
/// inside one call under the relay's database lock, with no seam between them to stop at, so the
/// test races real tasks on several threads and prints how the pulls fell. A round where every
/// pull landed on one side still checks the rule; it just did not exercise the race.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_a_format_one_pull_racing_the_raise_never_gets_a_pushed_row() {
    let raised = b64(&[9; 16]);
    let (mut before, mut refused) = (0, 0);
    for round in 0..20u8 {
        let h = Arc::new(harness(&format!("race-{round}")));
        let t = h.register(1, 0xC1).await;
        let old =
            serde_json::to_vec(&json!({"format": 1, "rows": [row(1, 1, 0, b"old")]})).unwrap();
        let (s, _) = h
            .call("POST", "/v1/rows", Some(&t), Some(1), Some(old))
            .await;
        assert_eq!(s, StatusCode::OK);
        let mut pulls = Vec::new();
        for _ in 0..8 {
            let (h, t) = (Arc::clone(&h), t.clone());
            pulls.push(tokio::spawn(async move {
                h.call("GET", "/v1/rows", Some(&t), Some(1), None).await
            }));
        }
        let new = serde_json::to_vec(
            &json!({"format": 2, "rows": [row(9, 2, 0, b"new"), row(8, 3, 0, b"new")]}),
        )
        .unwrap();
        let (s, _) = h
            .call("POST", "/v1/rows", Some(&t), Some(2), Some(new))
            .await;
        assert_eq!(s, StatusCode::OK);
        for p in pulls {
            let (s, v) = p.await.unwrap();
            if s == StatusCode::OK {
                let rows = v["rows"].as_array().unwrap();
                assert!(
                    rows.iter().all(|r| r["key"] != raised.as_str()),
                    "round {round}: {v}"
                );
                assert_eq!(rows.len(), 1, "round {round}: only the old row");
                assert_eq!(v["vault_format"], 1, "round {round}");
                before += 1;
            } else {
                assert_eq!(
                    (s, error_of(&v)),
                    (StatusCode::CONFLICT, "update_needed"),
                    "round {round}"
                );
                refused += 1;
            }
        }
        // After the push every format-1 pull is refused.
        let (s, v) = h.call("GET", "/v1/rows", Some(&t), Some(1), None).await;
        assert_eq!((s, error_of(&v)), (StatusCode::CONFLICT, "update_needed"));
    }
    println!("format-1 pulls: {before} before the raise, {refused} refused update_needed");
}

// ---- T14.46: relay hardening ----

/// A relay whose clock counts how often it is read. Checking a token reads it once (to record
/// when the device was seen), so the count tells whether the token was checked yet.
fn counting_harness(name: &str) -> (Harness, Arc<AtomicU64>) {
    let dir = TempDir::new(name);
    let reads = Arc::new(AtomicU64::new(0));
    let clock: Clock = {
        let reads = Arc::clone(&reads);
        Arc::new(move || {
            reads.fetch_add(1, Ordering::SeqCst);
            1_800_000_000
        })
    };
    let config = Config {
        setup_secret: SECRET.into(),
        data: dir.0.clone(),
        listen: "127.0.0.1:0".parse().unwrap(),
    };
    let relay = Relay::open_with_clock(&config, clock).unwrap();
    let app = router(relay).layer(MockConnectInfo(SocketAddr::from(([192, 0, 2, 7], 4000))));
    let harness = Harness {
        app,
        now: Arc::new(AtomicU64::new(1_800_000_000)),
        _dir: dir,
    };
    (harness, reads)
}

/// Sends `body` to `uri` with `token` and returns the answer and how often the relay's clock
/// had been read at the moment the relay took the body off the request (`u64::MAX`: it never
/// took it).
async fn send_watching_the_body(
    h: &Harness,
    reads: &Arc<AtomicU64>,
    uri: &str,
    token: &str,
    body: Vec<u8>,
) -> (StatusCode, Value, u64) {
    let at_body = Arc::new(AtomicU64::new(u64::MAX));
    let watched = {
        let (reads, at_body) = (Arc::clone(reads), Arc::clone(&at_body));
        // The frame is handed over only when the relay reads the request's body.
        http_body_util::Full::new(axum::body::Bytes::from(body)).map_frame(move |frame| {
            let _ = at_body.compare_exchange(
                u64::MAX,
                reads.load(Ordering::SeqCst),
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            frame
        })
    };
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header(FORMAT_HEADER, "1")
        .header("content-type", "application/json")
        .body(Body::new(watched))
        .unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        at_body.load(Ordering::SeqCst),
    )
}

/// The token is checked before the body is read: on every call that takes a token and a body,
/// the relay has looked the token up (which reads its clock) by the time it takes the body, for
/// a device it knows and for a caller it refuses alike. A refused caller's body is never parsed:
/// the answer is `unknown_device` whatever it holds.
#[tokio::test]
async fn relay_checks_the_token_before_it_reads_the_body() {
    let (h, reads) = counting_harness("auth-first");
    let token = h.register(1, 0xC1).await;
    let presence = serde_json::to_vec(&json!({"blob": b64(b"sealed presence")})).unwrap();
    let rows = serde_json::to_vec(&json!({"format": 1, "rows": [row(1, 100, 0, b"one")]})).unwrap();
    let slot =
        serde_json::to_vec(&json!({"slot": b64(&[7; 16]), "wrapped": b64(&[1; 73])})).unwrap();
    let revoke = serde_json::to_vec(&json!({"device_id": b64(&[9; 16])})).unwrap();
    for (uri, body) in [
        ("/v1/presence", presence),
        ("/v1/rows", rows),
        ("/v1/pairing", slot),
        ("/v1/devices/revoke", revoke),
    ] {
        // A caller the relay refuses.
        let before = reads.load(Ordering::SeqCst);
        let (s, v, at_body) =
            send_watching_the_body(&h, &reads, uri, "not-a-token-the-relay-gave", body.clone())
                .await;
        assert_eq!(
            (s, error_of(&v)),
            (StatusCode::UNAUTHORIZED, "unknown_device"),
            "{uri}"
        );
        assert!(
            at_body == u64::MAX || at_body > before,
            "{uri}: the body was read before the token was checked"
        );
        // The same from a refused caller with a body the call could never take.
        let (s, v, _) = send_watching_the_body(
            &h,
            &reads,
            uri,
            "not-a-token-the-relay-gave",
            b"{ not json at all".to_vec(),
        )
        .await;
        assert_eq!(
            (s, error_of(&v)),
            (StatusCode::UNAUTHORIZED, "unknown_device"),
            "{uri}"
        );
        // A device it knows: the token is looked up first, then the body is read and used.
        let before = reads.load(Ordering::SeqCst);
        let (s, v, at_body) = send_watching_the_body(&h, &reads, uri, &token, body).await;
        assert_eq!(s, StatusCode::OK, "{uri}: {v}");
        assert!(
            at_body != u64::MAX && at_body > before,
            "{uri}: the body was read before the token was checked"
        );
    }
    // A call with no token at all is refused the same way.
    let (s, v) = h
        .call("POST", "/v1/rows", None, Some(1), Some(b"{}".to_vec()))
        .await;
    assert_eq!(
        (s, error_of(&v)),
        (StatusCode::UNAUTHORIZED, "unknown_device")
    );
}

/// A caller the relay refuses gets its answer whole even while it is sending a large body, over
/// a real connection: the relay takes what is sent and drops it rather than closing on unread
/// bytes (which can lose the answer on the way, leaving a removed computer to read "could not
/// be reached" for "this computer was removed").
#[test]
fn relay_answers_a_refused_caller_whole_while_it_sends_a_large_body() {
    use std::io::{Read as _, Write as _};
    let dir = TempDir::new("auth-large-body");
    let config = Config {
        setup_secret: SECRET.into(),
        data: dir.0.clone(),
        listen: "127.0.0.1:0".parse().unwrap(),
    };
    let relay = earshot_sync_server::LocalRelay::start(Relay::open(&config).unwrap()).unwrap();
    for size in [0usize, 64 * 1024, 3 * 1024 * 1024] {
        let mut s = std::net::TcpStream::connect(relay.addr()).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(60)))
            .unwrap();
        write!(
            s,
            "POST /v1/rows HTTP/1.1\r\nHost: relay.invalid\r\nAuthorization: Bearer \
             not-a-token-the-relay-gave\r\n{FORMAT_HEADER}: 1\r\nContent-Type: application/json\r\n\
             Content-Length: {size}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        s.write_all(&vec![b'x'; size]).unwrap();
        let mut answer = Vec::new();
        s.read_to_end(&mut answer).unwrap();
        let answer = String::from_utf8_lossy(&answer);
        assert!(
            answer.starts_with("HTTP/1.1 401 "),
            "{size} bytes: {answer}"
        );
        assert!(
            answer.ends_with(r#"{"error":"unknown_device"}"#),
            "{size} bytes: {answer}"
        );
    }
}

/// The calls that take no token hold little for a caller: a body over 16 KiB is refused as too
/// large, where a push may be megabytes. A registration of ordinary size, with a long secret,
/// still fits.
#[tokio::test]
async fn relay_holds_little_for_a_caller_with_no_token() {
    let h = harness("open-limit");
    let limit = earshot_sync_server::MAX_OPEN_REQUEST;
    assert_eq!(limit, 16 * 1024);
    for uri in ["/v1/devices", "/v1/pairing/take"] {
        let padded =
            serde_json::to_vec(&json!({"slot": b64(&[1; 16]), "pad": "x".repeat(limit)})).unwrap();
        assert!(padded.len() > limit && padded.len() < MAX_REQUEST);
        let (s, v) = h.call("POST", uri, None, Some(1), Some(padded)).await;
        assert_eq!(
            (s, error_of(&v)),
            (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
            "{uri}"
        );
    }
    // Under the limit the calls read their body as before (a wrong long secret is a wrong
    // secret, an unknown slot is no slot).
    let (s, v) = h
        .post(
            "/v1/devices",
            None,
            &json!({"device_id": b64(&[1; 16]), "key_check": b64(&[0xC1; 32]),
                    "setup_secret": "s".repeat(8 * 1024)}),
        )
        .await;
    assert_eq!((s, error_of(&v)), (StatusCode::FORBIDDEN, "bad_secret"));
    let (s, v) = h
        .post("/v1/pairing/take", None, &json!({"slot": b64(&[1; 16])}))
        .await;
    assert_eq!((s, error_of(&v)), (StatusCode::NOT_FOUND, "no_slot"));
    // A call with a token still takes a body of megabytes.
    let t = h.register(1, 0xC1).await;
    let rows: Vec<Value> = (0..8u8)
        .map(|k| row(k + 1, 1, 0, &vec![k; MAX_ROW_BODY]))
        .collect();
    let (s, v) = h
        .post("/v1/rows", Some(&t), &json!({"format": 1, "rows": rows}))
        .await;
    assert_eq!(
        (s, v["accepted"].as_u64()),
        (StatusCode::OK, Some(8)),
        "{v}"
    );
}

/// The setup secret is compared whole, whatever part of it a guess gets right: a prefix of it,
/// it with more after it, and a guess of its length that differs in one place are all refused
/// the same way; only the secret itself (spaces around it aside, as before) registers.
#[tokio::test]
async fn relay_refuses_every_near_miss_of_the_setup_secret_and_the_key_check() {
    let h = harness("near-miss");
    let mut one_off = SECRET.to_owned().into_bytes();
    *one_off.last_mut().unwrap() ^= 1;
    let guesses = [
        SECRET[..SECRET.len() - 1].to_owned(),
        format!("{SECRET}x"),
        String::from_utf8(one_off).unwrap(),
        String::new(),
        SECRET.to_uppercase(),
    ];
    for guess in guesses {
        let (s, v) = h
            .post(
                "/v1/devices",
                None,
                &json!({"device_id": b64(&[1; 16]), "key_check": b64(&[0xC1; 32]),
                        "setup_secret": guess}),
            )
            .await;
        assert_eq!(
            (s, error_of(&v)),
            (StatusCode::FORBIDDEN, "bad_secret"),
            "{guess:?}"
        );
    }
    let (s, _) = h
        .post(
            "/v1/devices",
            None,
            &json!({"device_id": b64(&[1; 16]), "key_check": b64(&[0xC1; 32]),
                    "setup_secret": format!("  {SECRET}\n")}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    // The vault's key check is compared whole too: one bit off anywhere is another vault.
    for flip in [0usize, 15, 31] {
        let mut check = [0xC1u8; 32];
        check[flip] ^= 0x80;
        let (s, v) = h
            .post(
                "/v1/devices",
                None,
                &json!({"device_id": b64(&[2; 16]), "key_check": b64(&check),
                        "setup_secret": SECRET}),
            )
            .await;
        assert_eq!(
            (s, error_of(&v)),
            (StatusCode::CONFLICT, "key_mismatch"),
            "byte {flip}"
        );
    }
    let (s, _) = h
        .post(
            "/v1/devices",
            None,
            &json!({"device_id": b64(&[2; 16]), "key_check": b64(&[0xC1; 32]),
                    "setup_secret": SECRET}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
}
