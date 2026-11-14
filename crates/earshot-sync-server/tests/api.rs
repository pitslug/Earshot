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
use earshot_sync::wire::{FORMAT_HEADER, MAX_PRESENCE, MAX_REQUEST, MAX_ROW_BODY, SLOT_TTL_S};
use earshot_sync_server::{Clock, Config, Relay, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const SECRET: &str = "an-invented-setup-secret-for-tests";

/// A folder under the system temp folder, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "earshot-relay-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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
    {
        let c = rusqlite_open(&dir.0.join("relay.db"));
        c.pragma_update(None, "user_version", 2).unwrap();
    }
    let e = Relay::open(&config).err().unwrap();
    assert_eq!(e.kind(), "newer");
}

fn rusqlite_open(p: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(p).unwrap()
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

#[tokio::test]
async fn relay_pairing_code_never_takes_over_a_known_device() {
    let h = harness("takeover");
    let a = h.register(1, 0xC1).await;
    let b = h.register(2, 0xC1).await;
    h.post(
        "/v1/devices/revoke",
        Some(&a),
        &json!({"device_id": b64(&[2; 16])}),
    )
    .await;
    for (n, id) in [(0x61u8, 1u8), (0x62, 2)] {
        let slot = b64(&[n; 16]);
        h.post(
            "/v1/pairing",
            Some(&a),
            &json!({"slot": slot, "wrapped": b64(&[7; 73])}),
        )
        .await;
        h.post("/v1/pairing/take", None, &json!({"slot": slot}))
            .await;
        // A live device (1) or a revoked one (2): refused, and nothing changes.
        let (s, v) = h
            .post(
                "/v1/devices",
                None,
                &json!({"device_id": b64(&[id; 16]), "key_check": b64(&[0xC1; 32]), "slot": slot}),
            )
            .await;
        assert_eq!(
            (s, error_of(&v)),
            (StatusCode::CONFLICT, "device_exists"),
            "{id}"
        );
    }
    let (s, _) = h.call("GET", "/v1/vault", Some(&a), None, None).await;
    assert_eq!(s, StatusCode::OK, "device 1 keeps its token");
    let (s, _) = h.call("GET", "/v1/vault", Some(&b), None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "device 2 stays revoked");
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
