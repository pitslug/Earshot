//! The sync relay (T11.2; docs/sync-spec.md sections 3 and 4).
//!
//! It stores what devices push and hands it to the others: sealed rows (newest per row key,
//! numbered by a server sequence), sealed presence blobs, and pairing slots. It holds no
//! vault key and cannot read a row, a blob or a wrapped key. What it can see is listed in
//! docs/sync.md section 4: counts, sizes, stamps, device ids and times.
//!
//! Logs hold a method, a path, a status and a duration. Never a body, a token, the setup
//! secret or a slot.

mod db;
mod limiter;

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, QueryRejection};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use earshot_sync::wire::{
    B64, EPOCH_LEN, ErrorBody, ErrorWord, FORMAT_HEADER, HealthResponse, MAX_PRESENCE, MAX_REQUEST,
    MAX_ROW_BODY, MAX_SEEN_SEQ, MAX_STAMP_AHEAD_MS, PULL_DEFAULT, PULL_MAX, PairingRequest,
    PairingResponse, PresenceRequest, PresenceResponse, PullResponse, PushRequest, PushResponse,
    RegisterRequest, RegisterResponse, RevokeRequest, SLOT_TTL_S, TakeRequest, TakeResponse,
    VaultResponse,
};
use earshot_sync::{KEY_CHECK_LEN, ROW_KEY_LEN, SLOT_LEN};
use serde::Deserialize;
use subtle::ConstantTimeEq;

pub use db::{CLAIM_EVERY_S, DbError, REFILE_MAX};

/// The shortest setup secret the relay starts with.
pub const MIN_SECRET_CHARS: usize = 24;

/// The largest wrapped vault key a pairing slot holds (a wrap is 73 bytes today).
const MAX_WRAPPED: usize = 256;

/// The largest body of a call that takes no token (registering, taking a pairing slot): 16 KiB,
/// a judgement. Such a body holds a device id, a key check and a setup secret or a slot, a few
/// hundred bytes; the limit leaves room for a long secret and nothing for a caller with no
/// token to make the relay hold (T14.46). Over it is `too_large`.
pub const MAX_OPEN_REQUEST: usize = 16 * 1024;

/// The relay's settings.
#[derive(Clone)]
pub struct Config {
    /// `EARSHOT_SYNC_SETUP_SECRET`.
    pub setup_secret: String,
    /// `EARSHOT_SYNC_DATA`: the folder holding `relay.db`.
    pub data: std::path::PathBuf,
    /// `EARSHOT_SYNC_LISTEN`.
    pub listen: SocketAddr,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("data", &self.data)
            .field("listen", &self.listen)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// The settings from the environment, read through `var`.
    ///
    /// # Errors
    /// A message naming the variable (never its value) when one is missing or unusable.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        Self::from_env_reading(var, |p| std::fs::read_to_string(p))
    }

    /// [`Config::from_env`] with the file reader given, for tests. The secret comes from
    /// `EARSHOT_SYNC_SETUP_SECRET` or from the file `EARSHOT_SYNC_SETUP_SECRET_FILE` names (a
    /// Docker secret), never both.
    ///
    /// # Errors
    /// As [`Config::from_env`]; a message never holds the secret or the file's text.
    pub fn from_env_reading(
        var: impl Fn(&str) -> Option<String>,
        read: impl Fn(&std::path::Path) -> std::io::Result<String>,
    ) -> Result<Self, String> {
        let setup_secret = match (
            var("EARSHOT_SYNC_SETUP_SECRET"),
            var("EARSHOT_SYNC_SETUP_SECRET_FILE"),
        ) {
            (Some(_), Some(_)) => {
                return Err(
                    "set EARSHOT_SYNC_SETUP_SECRET or EARSHOT_SYNC_SETUP_SECRET_FILE, \
                            not both"
                        .into(),
                );
            }
            (Some(s), None) => s,
            (None, Some(path)) => read(std::path::Path::new(&path))
                .map_err(|_| "EARSHOT_SYNC_SETUP_SECRET_FILE cannot be read".to_owned())?,
            (None, None) => {
                return Err(
                    "EARSHOT_SYNC_SETUP_SECRET or EARSHOT_SYNC_SETUP_SECRET_FILE is not set".into(),
                );
            }
        };
        if setup_secret.trim().chars().count() < MIN_SECRET_CHARS {
            return Err(format!(
                "the setup secret must be at least {MIN_SECRET_CHARS} characters"
            ));
        }
        let data = var("EARSHOT_SYNC_DATA").unwrap_or_else(|| "/data".into());
        let listen = listen_of(&var)?;
        Ok(Self {
            setup_secret,
            data: data.into(),
            listen,
        })
    }
}

/// `EARSHOT_SYNC_LISTEN` (default `0.0.0.0:8787`).
///
/// # Errors
/// A message naming the variable when it is not an address and port.
pub fn listen_of(var: &impl Fn(&str) -> Option<String>) -> Result<SocketAddr, String> {
    var("EARSHOT_SYNC_LISTEN")
        .unwrap_or_else(|| "0.0.0.0:8787".into())
        .parse()
        .map_err(|_| "EARSHOT_SYNC_LISTEN is not an address and port".to_owned())
}

/// Where `earshot-sync-server health` connects: the listening port on loopback when the relay
/// listens on every address, else the address itself. The health check opens a TCP connection
/// and closes it; it speaks no HTTP, so the image needs no HTTP client.
#[must_use]
pub fn health_target(listen: SocketAddr) -> SocketAddr {
    if listen.ip().is_unspecified() {
        let loopback: IpAddr = if listen.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        };
        SocketAddr::new(loopback, listen.port())
    } else {
        listen
    }
}

/// The relay's clock: Unix seconds.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The system clock.
#[must_use]
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    })
}

/// A running relay's state.
#[derive(Clone)]
pub struct Relay(Arc<Inner>);

struct Inner {
    db: Mutex<db::Db>,
    secret: String,
    clock: Clock,
    limiter: Mutex<limiter::Limiter>,
}

impl Relay {
    /// Opens (or creates) `relay.db` in `config.data` on the system clock.
    ///
    /// # Errors
    /// The folder or database cannot be opened, or it was written by a newer relay.
    pub fn open(config: &Config) -> Result<Self, DbError> {
        Self::open_with_clock(config, system_clock())
    }

    /// [`Relay::open`] on `clock`.
    ///
    /// # Errors
    /// As [`Relay::open`].
    pub fn open_with_clock(config: &Config, clock: Clock) -> Result<Self, DbError> {
        std::fs::create_dir_all(&config.data).map_err(DbError::Io)?;
        let db = db::Db::open(&config.data.join("relay.db"))?;
        Ok(Self(Arc::new(Inner {
            db: Mutex::new(db),
            secret: config.setup_secret.trim().to_owned(),
            clock,
            limiter: Mutex::new(limiter::Limiter::default()),
        })))
    }

    fn now(&self) -> u64 {
        (self.0.clock)()
    }

    fn db(&self) -> MutexGuard<'_, db::Db> {
        // A panic inside a transaction rolls it back; the connection itself stays usable.
        self.0
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn limiter(&self) -> MutexGuard<'_, limiter::Limiter> {
        self.0
            .limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What the relay holds, by count and size only (`earshot-sync-server inspect`; `accept m11`
/// checks that no stored row is over the limit). Never a body, a key or a device id.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Inspection {
    /// Rows stored.
    pub rows: u64,
    /// The largest stored row body, in bytes.
    pub largest_row_bytes: u64,
    /// Rows whose body is over the limit (the relay refuses them, so 0).
    pub rows_over_limit: u64,
    /// The row limit the count was taken against, in bytes.
    pub row_limit_bytes: u64,
    /// Devices registered (revoked ones included).
    pub devices: u64,
    /// The largest stored presence blob, in bytes.
    pub largest_presence_bytes: u64,
    /// Presence blobs over their limit.
    pub presence_over_limit: u64,
}

/// What the relay in `data` holds, by count and size.
///
/// # Errors
/// The database cannot be opened, or it was written by a newer relay.
pub fn inspect(data: &std::path::Path) -> Result<Inspection, DbError> {
    db::Db::open(&data.join("relay.db"))?.inspect()
}

/// A relay serving on a loopback port inside the calling process, on its own tokio runtime:
/// the in-process server that sync clients are tested against (T11.3). It is the relay's
/// own router, not a copy. Stops when dropped; drop it outside any async context.
pub struct LocalRelay {
    addr: SocketAddr,
    _runtime: tokio::runtime::Runtime,
}

impl LocalRelay {
    /// Serves `relay` on `127.0.0.1` at a free port.
    ///
    /// # Errors
    /// The runtime cannot start or the port cannot be bound.
    pub fn start(relay: Relay) -> std::io::Result<Self> {
        Self::start_on(relay, 0)
    }

    /// Serves `relay` on `127.0.0.1:<port>` (0: a free port). A test that stops a relay and
    /// starts it again on the same port uses this.
    ///
    /// # Errors
    /// The runtime cannot start or the port cannot be bound.
    pub fn start_on(relay: Relay, port: u16) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("earshot-local-relay")
            .enable_all()
            .build()?;
        let bound = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))?;
        bound.set_nonblocking(true)?;
        let addr = bound.local_addr()?;
        let listener = {
            let _entered = runtime.enter();
            tokio::net::TcpListener::from_std(bound)?
        };
        let app = router(relay).into_make_service_with_connect_info::<SocketAddr>();
        runtime.spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            addr,
            _runtime: runtime,
        })
    }

    /// Where it listens.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Its address as a client is given it: `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// The relay's routes (docs/sync-spec.md section 3).
///
/// The calls that take a token are behind [`authenticate`], which runs before any of their
/// handler's arguments are made, the body among them (T14.46): a caller the relay does not know
/// is answered without its body ever being held in memory. The layer covers only the calls
/// themselves (`route_layer`), so an unknown path or method on them is still `not_found`, with
/// or without a token.
pub fn router(relay: Relay) -> Router {
    let with_token = Router::new()
        .route("/v1/devices/revoke", post(revoke))
        .route("/v1/pairing", post(offer_slot))
        .route("/v1/rows", post(push).get(pull))
        .route("/v1/presence", post(put_presence).get(get_presence))
        .route("/v1/vault", get(vault))
        .route_layer(middleware::from_fn_with_state(relay.clone(), authenticate));
    // The two calls anyone may make take a few short fields: a body past MAX_OPEN_REQUEST is
    // refused before it is read, rather than held up to the limit a push may reach.
    let open = Router::new()
        .route("/v1/devices", post(register))
        .route("/v1/pairing/take", post(take_slot))
        .layer(DefaultBodyLimit::max(MAX_OPEN_REQUEST));
    Router::new()
        .route("/v1/health", get(health))
        .merge(open)
        .merge(with_token)
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_REQUEST))
        .layer(middleware::from_fn(log))
        .with_state(relay)
}

/// The device a call came from, as [`authenticate`] found it from the bearer token.
#[derive(Debug, Clone, Copy)]
struct Caller([u8; 16]);

/// How long the relay goes on taking a refused caller's body before it answers anyway (10
/// seconds, a judgement: longer than a slow link needs for the few megabytes a push may be,
/// short enough that a caller who sends nothing does not hold the call open).
const DISCARD_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Checks the bearer token before the call's handler runs (T14.46). A device the relay knows
/// goes on to the handler with its id ([`Caller`]). Anyone else is answered `unknown_device`:
/// what they were sending is taken off the connection and dropped, piece by piece, never
/// collected, so the relay holds nothing for a caller it does not know, and the answer still
/// reaches a caller in the middle of a large push (a connection closed with unread bytes on it
/// can lose the answer, and a removed computer must read that it was removed).
async fn authenticate(State(relay): State<Relay>, mut req: Request, next: Next) -> Response {
    match caller(&relay, req.headers()) {
        Ok(device) => {
            req.extensions_mut().insert(Caller(device));
            next.run(req).await
        }
        Err(fail) => {
            discard(req.into_body()).await;
            fail.into_response()
        }
    }
}

/// Takes what is left of a request's body and drops it as it comes: at most [`MAX_REQUEST`]
/// bytes and for at most [`DISCARD_WAIT`]. Past either the call is answered anyway and the
/// connection is the server's to close.
async fn discard(mut body: axum::body::Body) {
    use axum::body::HttpBody as _;
    let taking = async {
        let mut left = MAX_REQUEST;
        while let Some(Ok(frame)) =
            std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await
        {
            let Some(data) = frame.data_ref() else {
                continue;
            };
            if data.len() > left {
                return;
            }
            left -= data.len();
        }
    };
    let _ = tokio::time::timeout(DISCARD_WAIT, taking).await;
}

/// One line per request: method, path (no query), status, milliseconds.
async fn log(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let started = std::time::Instant::now();
    let res = next.run(req).await;
    println!(
        "{method} {path} {} {}ms",
        res.status().as_u16(),
        started.elapsed().as_millis()
    );
    res
}

/// A failed call.
#[derive(Debug)]
pub enum Fail {
    /// An error the client knows ([`ErrorWord`]).
    Word(ErrorWord),
    /// The relay's own problem (its database): 500, `{"error":"internal"}`.
    Internal,
}

impl From<ErrorWord> for Fail {
    fn from(w: ErrorWord) -> Self {
        Self::Word(w)
    }
}

impl From<DbError> for Fail {
    fn from(e: DbError) -> Self {
        // The kind only: a database message can quote SQL, never a value, but stays out.
        eprintln!("earshot-sync-server: database error ({})", e.kind());
        Self::Internal
    }
}

/// An unknown path or method: `{"error":"not_found"}`.
async fn not_found() -> Fail {
    Fail::Word(ErrorWord::NotFound)
}

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        let (status, word) = match self {
            Self::Word(w) => (
                StatusCode::from_u16(w.status()).unwrap_or(StatusCode::BAD_REQUEST),
                w.word(),
            ),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorWord::Internal.word(),
            ),
        };
        (
            status,
            Json(ErrorBody {
                error: word.to_owned(),
            }),
        )
            .into_response()
    }
}

type Answer<T> = Result<Json<T>, Fail>;

/// The JSON body of a call, with the relay's own errors: over the size limit is
/// `too_large`, anything else unreadable is `bad_request`.
fn body<T: for<'de> Deserialize<'de>>(bytes: Result<Bytes, BytesRejection>) -> Result<T, Fail> {
    let bytes = bytes.map_err(|r| {
        if r.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ErrorWord::TooLarge
        } else {
            ErrorWord::BadRequest
        }
    })?;
    serde_json::from_slice(&bytes).map_err(|_| ErrorWord::BadRequest.into())
}

/// `bytes` as exactly `N` bytes, else `bad_request`.
fn exact<const N: usize>(b: &B64) -> Result<[u8; N], Fail> {
    b.0.as_slice()
        .try_into()
        .map_err(|_| ErrorWord::BadRequest.into())
}

/// The calling device, from its bearer token. Records that it was seen.
fn caller(relay: &Relay, headers: &HeaderMap) -> Result<[u8; 16], Fail> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(ErrorWord::UnknownDevice)?;
    let now = relay.now();
    relay
        .db()
        .device_by_token(&db::token_hash(token), now)?
        .ok_or_else(|| ErrorWord::UnknownDevice.into())
}

/// The format the caller names in [`FORMAT_HEADER`].
fn format_header(headers: &HeaderMap) -> Result<u32, Fail> {
    Ok(headers
        .get(FORMAT_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .ok_or(ErrorWord::BadRequest)?)
}

/// The format the caller names in [`FORMAT_HEADER`]; `update_needed` when it is below the
/// vault's.
fn check_format(relay: &Relay, headers: &HeaderMap) -> Result<u32, Fail> {
    let format = format_header(headers)?;
    if format < relay.db().vault_format()? {
        return Err(ErrorWord::UpdateNeeded.into());
    }
    Ok(format)
}

/// Whether `given` is the setup secret, taking the same time whichever byte differs and
/// whatever the lengths (T14.46): the two SHA-256 digests are compared in constant time, so a
/// guess learns nothing from how long the answer took, not even how long the secret is (a
/// direct compare of the texts stops at the first difference in length).
fn same_secret(given: &str, secret: &str) -> bool {
    use sha2::{Digest as _, Sha256};
    let given = Sha256::digest(given.as_bytes());
    let secret = Sha256::digest(secret.as_bytes());
    given.as_slice().ct_eq(secret.as_slice()).into()
}

/// Refuses an address with too many recent failures.
fn not_slowed(relay: &Relay, ip: IpAddr) -> Result<(), Fail> {
    if relay.limiter().blocked(ip, relay.now()) {
        return Err(ErrorWord::SlowDown.into());
    }
    Ok(())
}

/// Counts a failure against `ip` and returns `fail`.
fn failed(relay: &Relay, ip: IpAddr, fail: ErrorWord) -> Fail {
    relay.limiter().record(ip, relay.now());
    fail.into()
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}

async fn register(
    State(relay): State<Relay>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    bytes: Result<Bytes, BytesRejection>,
) -> Answer<RegisterResponse> {
    let ip = addr.ip();
    not_slowed(&relay, ip)?;
    let req: RegisterRequest = body(bytes)?;
    let device = exact::<16>(&req.device_id)?;
    let check = exact::<KEY_CHECK_LEN>(&req.key_check)?;
    let now = relay.now();
    let slot = match (&req.setup_secret, &req.slot) {
        (Some(secret), None) => {
            if !same_secret(secret.trim(), &relay.0.secret) {
                return Err(failed(&relay, ip, ErrorWord::BadSecret));
            }
            None
        }
        (None, Some(slot)) => {
            let slot = exact::<SLOT_LEN>(slot)?;
            if !relay.db().slot_taken_and_live(&slot, now)? {
                return Err(failed(&relay, ip, ErrorWord::NoSlot));
            }
            Some(slot)
        }
        _ => return Err(ErrorWord::BadRequest.into()),
    };
    let token = db::new_token().map_err(|()| Fail::Internal)?;
    match relay
        .db()
        .register(&device, &check, &db::token_hash(&token), slot.as_ref(), now)?
    {
        db::Registered::Yes => Ok(Json(RegisterResponse { token })),
        db::Registered::KeyMismatch => Err(ErrorWord::KeyMismatch.into()),
        db::Registered::SlotGone => Err(failed(&relay, ip, ErrorWord::NoSlot)),
        db::Registered::DeviceExists => Err(ErrorWord::DeviceExists.into()),
    }
}

async fn revoke(
    State(relay): State<Relay>,
    Extension(Caller(_)): Extension<Caller>,
    bytes: Result<Bytes, BytesRejection>,
) -> Answer<serde_json::Value> {
    let req: RevokeRequest = body(bytes)?;
    let device = exact::<16>(&req.device_id)?;
    relay.db().revoke(&device)?;
    Ok(Json(serde_json::json!({})))
}

async fn offer_slot(
    State(relay): State<Relay>,
    Extension(Caller(_)): Extension<Caller>,
    bytes: Result<Bytes, BytesRejection>,
) -> Answer<PairingResponse> {
    let req: PairingRequest = body(bytes)?;
    let slot = exact::<SLOT_LEN>(&req.slot)?;
    if req.wrapped.0.is_empty() || req.wrapped.0.len() > MAX_WRAPPED {
        return Err(ErrorWord::BadRequest.into());
    }
    let now = relay.now();
    relay
        .db()
        .offer_slot(&slot, &req.wrapped.0, now + SLOT_TTL_S, now)?;
    Ok(Json(PairingResponse {
        expires_in_s: SLOT_TTL_S,
    }))
}

async fn take_slot(
    State(relay): State<Relay>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    bytes: Result<Bytes, BytesRejection>,
) -> Answer<TakeResponse> {
    let ip = addr.ip();
    not_slowed(&relay, ip)?;
    let req: TakeRequest = body(bytes)?;
    let slot = exact::<SLOT_LEN>(&req.slot)?;
    match relay.db().take_slot(&slot, relay.now())? {
        Some(wrapped) => Ok(Json(TakeResponse {
            wrapped: B64(wrapped),
        })),
        None => Err(failed(&relay, ip, ErrorWord::NoSlot)),
    }
}

async fn push(
    State(relay): State<Relay>,
    Extension(Caller(device)): Extension<Caller>,
    headers: HeaderMap,
    bytes: Result<Bytes, BytesRejection>,
) -> Answer<PushResponse> {
    // Refused early when the device is plainly behind the vault; the check that decides is
    // made again inside the transaction that stores the rows (T14.46), since another device
    // may raise the vault's format between this read and that write.
    let announced = check_format(&relay, &headers)?;
    let req: PushRequest = body(bytes)?;
    // The format in the body is the one the vault's format rises to: it must be the one the
    // device announces, so one bad push cannot lock every device out with a format nobody
    // writes (the header is already checked against the vault).
    if req.format != announced {
        return Err(ErrorWord::BadRequest.into());
    }
    // A stamp far past the relay's clock would be kept over every later edit of its row from a
    // computer whose clock is right (the latest stamp wins here), so the push that carries one
    // is refused whole and nothing of it is stored (T14.44). The device keeps its rows queued.
    let latest_ms = relay
        .now()
        .saturating_mul(1000)
        .saturating_add(MAX_STAMP_AHEAD_MS);
    let mut rows = Vec::with_capacity(req.rows.len());
    for r in &req.rows {
        if r.body.0.is_empty() {
            return Err(ErrorWord::BadRequest.into());
        }
        if r.body.0.len() > MAX_ROW_BODY {
            return Err(ErrorWord::TooLarge.into());
        }
        if r.wall_ms > latest_ms {
            return Err(ErrorWord::ClockAhead.into());
        }
        rows.push(db::NewRow {
            key: exact::<ROW_KEY_LEN>(&r.key)?,
            stamp: db::Stamp {
                wall_ms: i64::try_from(r.wall_ms).map_err(|_| ErrorWord::BadRequest)?,
                logical: r.logical,
                node: exact::<16>(&r.node)?,
            },
            body: &r.body.0,
        });
    }
    let claim = claim_of(&relay, req.seen_seq, req.epoch.as_ref())?;
    let Some(pushed) = relay.db().push(&device, req.format, &rows, &claim)? else {
        return Err(ErrorWord::UpdateNeeded.into());
    };
    Ok(Json(PushResponse {
        accepted: pushed.accepted,
        superseded: pushed
            .superseded
            .into_iter()
            .map(|k| B64(k.to_vec()))
            .collect(),
        seq: pushed.seq,
        epoch: Some(B64(pushed.epoch.to_vec())),
        max_seq: Some(pushed.max_seq_before),
    }))
}

/// What a device says it has seen of the vault (T14.45): `bad_request` for a sequence number
/// past [`MAX_SEEN_SEQ`] or an epoch that is not [`EPOCH_LEN`] bytes.
fn claim_of(relay: &Relay, seen: Option<u64>, epoch: Option<&B64>) -> Result<db::Claim, Fail> {
    if seen.is_some_and(|s| s > MAX_SEEN_SEQ) {
        return Err(ErrorWord::BadRequest.into());
    }
    Ok(db::Claim {
        seen,
        epoch: epoch.map(exact::<EPOCH_LEN>).transpose()?,
        now_s: relay.now(),
    })
}

/// `GET /v1/rows` parameters. `seen` and `epoch` are the pull's [`PushRequest::seen_seq`] and
/// [`PushRequest::epoch`] (T14.45); a client from before them sends neither.
#[derive(Debug, Deserialize)]
struct PullQuery {
    #[serde(default)]
    after: u64,
    limit: Option<u32>,
    seen: Option<u64>,
    epoch: Option<B64>,
}

async fn pull(
    State(relay): State<Relay>,
    Extension(Caller(_)): Extension<Caller>,
    headers: HeaderMap,
    query: Result<Query<PullQuery>, QueryRejection>,
) -> Answer<PullResponse> {
    let format = format_header(&headers)?;
    let Query(q) = query.map_err(|_| ErrorWord::BadRequest)?;
    let limit = q.limit.unwrap_or(PULL_DEFAULT).clamp(1, PULL_MAX);
    // The cursor is itself something the device has seen (a client from before T14.45 sends
    // nothing else); one past the limit is no claim, and reads as it always did.
    let mut claim = claim_of(&relay, q.seen, q.epoch.as_ref())?;
    if q.after <= MAX_SEEN_SEQ {
        claim.seen = Some(claim.seen.map_or(q.after, |s| s.max(q.after)));
    }
    // The format is checked in the same read as the rows (T12.77b8): a push that raises the
    // vault's format commits with its rows, so a device below it never receives one of them.
    match relay
        .db()
        .pull(format, q.after, limit, MAX_REQUEST, &claim)?
    {
        Some(page) => Ok(Json(page)),
        None => Err(ErrorWord::UpdateNeeded.into()),
    }
}

async fn put_presence(
    State(relay): State<Relay>,
    Extension(Caller(device)): Extension<Caller>,
    bytes: Result<Bytes, BytesRejection>,
) -> Answer<serde_json::Value> {
    let req: PresenceRequest = body(bytes)?;
    if req.blob.0.is_empty() {
        return Err(ErrorWord::BadRequest.into());
    }
    if req.blob.0.len() > MAX_PRESENCE {
        return Err(ErrorWord::TooLarge.into());
    }
    let now = relay.now();
    relay.db().put_presence(&device, &req.blob.0, now)?;
    Ok(Json(serde_json::json!({})))
}

async fn get_presence(
    State(relay): State<Relay>,
    Extension(Caller(_)): Extension<Caller>,
) -> Answer<PresenceResponse> {
    let now = relay.now();
    Ok(Json(relay.db().presence(now)?))
}

async fn vault(
    State(relay): State<Relay>,
    Extension(Caller(_)): Extension<Caller>,
) -> Answer<VaultResponse> {
    let now = relay.now();
    Ok(Json(relay.db().vault(now)?))
}
