//! The relay's calls (docs/sync-spec.md section 3), over one `earshot-net` client whose
//! allowlist is exactly the sync server.
//!
//! A failure becomes a fixed [`SyncProblem`]: the relay's error word when it sent one this
//! build knows, else [`SyncProblem::Server`]; nothing the server sent reaches a caller.

use std::time::Duration;

use earshot_net::{Allowlist, AuditLog, Method, NetClient, NetConfig, NetError, NetRequest, Url};
use earshot_sync::ENVELOPE_FORMAT;
use earshot_sync::wire::{
    ErrorBody, ErrorWord, FORMAT_HEADER, PairingRequest, PairingResponse, PresenceRequest,
    PresenceResponse, PullResponse, PushRequest, PushResponse, RegisterRequest, RegisterResponse,
    RevokeRequest, TakeRequest, TakeResponse, VaultResponse,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{DeviceToken, SyncProblem};
use crate::services::SharedClock;

/// How long one request may take (a page is at most 4 MiB).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a presence request may take: it is small and advisory, and a stop waits for the
/// one that says gone.
const PRESENCE_TIMEOUT: Duration = Duration::from_secs(5);

/// The relay at one address.
pub(crate) struct Relay {
    net: NetClient,
    runtime: tokio::runtime::Runtime,
    base: Url,
    /// The envelope format every call announces: [`ENVELOPE_FORMAT`], except in a test that
    /// stands in for an older build.
    format: u32,
}

fn problem_of_net(e: &NetError) -> SyncProblem {
    match e {
        NetError::NotAllowed { .. } => SyncProblem::Address,
        NetError::Transport(_) => SyncProblem::Unreachable,
        NetError::Build(_)
        | NetError::InvalidHeader(_)
        | NetError::TooManyRedirects
        | NetError::BadRedirect(_)
        | NetError::BodyTooLarge { .. }
        | NetError::StreamFailed => SyncProblem::Server,
    }
}

/// The pull's query parameters, in order. A record that holds only a cursor (what a 0.4.2
/// client stores) gives exactly the two a 0.4.2 client sends, `after` and `limit`.
pub(crate) fn pull_query(
    after: u64,
    limit: u32,
    seen: &super::Cursor,
) -> Vec<(&'static str, String)> {
    let mut query = vec![("after", after.to_string()), ("limit", limit.to_string())];
    if seen.seen() > after {
        query.push(("seen", seen.seen().to_string()));
    }
    if let Some(epoch) = &seen.epoch {
        query.push(("epoch", epoch.text()));
    }
    query
}

/// The problem for a relay error word.
pub(crate) fn problem_of_word(word: ErrorWord) -> SyncProblem {
    match word {
        ErrorWord::UnknownDevice => SyncProblem::UnknownDevice,
        ErrorWord::BadSecret => SyncProblem::BadSecret,
        ErrorWord::NoSlot => SyncProblem::NoSlot,
        ErrorWord::KeyMismatch => SyncProblem::KeyMismatch,
        ErrorWord::UpdateNeeded => SyncProblem::UpdateNeeded,
        ErrorWord::DeviceExists => SyncProblem::DeviceExists,
        ErrorWord::TooLarge => SyncProblem::TooLarge,
        ErrorWord::ClockAhead => SyncProblem::ClockAhead,
        ErrorWord::SlowDown => SyncProblem::SlowDown,
        ErrorWord::BadRequest | ErrorWord::NotFound | ErrorWord::Internal => SyncProblem::Server,
    }
}

impl Relay {
    /// A client for the relay at `base` (an address [`super::check_address`] accepted).
    pub(crate) fn new(base: Url, clock: SharedClock) -> Result<Self, SyncProblem> {
        let mut allow = Allowlist::new();
        allow.allow_url(&base).map_err(|_| SyncProblem::Address)?;
        let net = NetClient::new(NetConfig::new(allow, clock).timeout(REQUEST_TIMEOUT))
            .map_err(|_| SyncProblem::Internal)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| SyncProblem::Internal)?;
        Ok(Self {
            net,
            runtime,
            base,
            format: ENVELOPE_FORMAT,
        })
    }

    /// This client announcing `format` on every call, as a build that writes that envelope
    /// format does (0.4.2: 2).
    #[cfg(test)]
    pub(crate) fn announcing(mut self, format: u32) -> Self {
        self.format = format;
        self
    }

    /// Every request made, one entry per hop.
    pub(crate) fn audit_log(&self) -> AuditLog {
        self.net.audit_log()
    }

    fn url(&self, path: &str) -> Result<Url, SyncProblem> {
        self.base.join(path).map_err(|_| SyncProblem::Address)
    }

    fn call<Resp: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        token: Option<&DeviceToken>,
        body: Option<Vec<u8>>,
    ) -> Result<Resp, SyncProblem> {
        self.call_within(method, url, token, body, REQUEST_TIMEOUT)
    }

    fn call_within<Resp: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        token: Option<&DeviceToken>,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Resp, SyncProblem> {
        let mut req = NetRequest::new(method, url)
            .header(FORMAT_HEADER, self.format.to_string())
            .timeout(timeout);
        if let Some(token) = token {
            req = req.header("Authorization", format!("Bearer {}", token.as_str()));
        }
        if let Some(body) = body {
            req = req.header("Content-Type", "application/json").body(body);
        }
        let resp = self
            .runtime
            .block_on(self.net.send(req))
            .map_err(|e| problem_of_net(&e))?;
        if (200..300).contains(&resp.status) {
            return serde_json::from_slice(&resp.body).map_err(|_| SyncProblem::Server);
        }
        Err(serde_json::from_slice::<ErrorBody>(&resp.body)
            .ok()
            .and_then(|b| ErrorWord::from_word(&b.error))
            .map_or(SyncProblem::Server, problem_of_word))
    }

    fn post<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        path: &str,
        token: Option<&DeviceToken>,
        body: &Req,
    ) -> Result<Resp, SyncProblem> {
        let bytes = serde_json::to_vec(body).map_err(|_| SyncProblem::Internal)?;
        self.call(Method::POST, self.url(path)?, token, Some(bytes))
    }

    /// `POST /v1/devices`.
    pub(crate) fn register(&self, req: &RegisterRequest) -> Result<DeviceToken, SyncProblem> {
        let resp: RegisterResponse = self.post("v1/devices", None, req)?;
        Ok(DeviceToken::new(resp.token))
    }

    /// `POST /v1/pairing`.
    pub(crate) fn offer_slot(
        &self,
        token: &DeviceToken,
        req: &PairingRequest,
    ) -> Result<PairingResponse, SyncProblem> {
        self.post("v1/pairing", Some(token), req)
    }

    /// `POST /v1/pairing/take`.
    pub(crate) fn take_slot(&self, req: &TakeRequest) -> Result<TakeResponse, SyncProblem> {
        self.post("v1/pairing/take", None, req)
    }

    /// `POST /v1/rows`.
    pub(crate) fn push(
        &self,
        token: &DeviceToken,
        req: &PushRequest,
    ) -> Result<PushResponse, SyncProblem> {
        self.post("v1/rows", Some(token), req)
    }

    /// `GET /v1/rows?after=<after>&limit=<limit>` (`PULL_DEFAULT` outside tests), with what
    /// this device has seen of the vault when it has seen more than its cursor says (`seen`,
    /// `epoch`; T14.45). A relay from before those ignores them.
    pub(crate) fn pull(
        &self,
        token: &DeviceToken,
        after: u64,
        limit: u32,
        seen: &super::Cursor,
    ) -> Result<PullResponse, SyncProblem> {
        let mut url = self.url("v1/rows")?;
        {
            let mut query = url.query_pairs_mut();
            for (name, value) in pull_query(after, limit, seen) {
                query.append_pair(name, &value);
            }
        }
        self.call(Method::GET, url, Some(token), None)
    }

    /// `POST /v1/devices/revoke`.
    pub(crate) fn revoke(
        &self,
        token: &DeviceToken,
        req: &RevokeRequest,
    ) -> Result<(), SyncProblem> {
        let _: serde_json::Value = self.post("v1/devices/revoke", Some(token), req)?;
        Ok(())
    }

    /// `POST /v1/presence`.
    pub(crate) fn put_presence(
        &self,
        token: &DeviceToken,
        req: &PresenceRequest,
    ) -> Result<(), SyncProblem> {
        let bytes = serde_json::to_vec(req).map_err(|_| SyncProblem::Internal)?;
        let _: serde_json::Value = self.call_within(
            Method::POST,
            self.url("v1/presence")?,
            Some(token),
            Some(bytes),
            PRESENCE_TIMEOUT,
        )?;
        Ok(())
    }

    /// `GET /v1/presence`.
    pub(crate) fn presence(&self, token: &DeviceToken) -> Result<PresenceResponse, SyncProblem> {
        self.call(Method::GET, self.url("v1/presence")?, Some(token), None)
    }

    /// `GET /v1/vault`.
    pub(crate) fn vault(&self, token: &DeviceToken) -> Result<VaultResponse, SyncProblem> {
        self.call(Method::GET, self.url("v1/vault")?, Some(token), None)
    }
}
