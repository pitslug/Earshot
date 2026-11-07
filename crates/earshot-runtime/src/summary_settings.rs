//! The Summaries page settings (T7.4g): who writes the summary, the endpoint of "your own
//! endpoint" and the live summaries switch. Device-local (`local_state`, never synced, no
//! migration), one key each, written only through [`set_summary_settings`] in one
//! transaction.
//!
//! - **Who writes the summary** ([`SummaryWriter`]): on this computer (the Models page's
//!   local model, the default), your own endpoint, Claude through the user's own Claude Code
//!   (T7.4ga; PLAN.md 3.9 exception: the prompt text is handed to the CLI, which sends it to
//!   Anthropic on the user's Claude.ai sign-in), OpenAI through the user's own Codex (T7.4gc;
//!   the same exception: Codex sends the text to OpenAI on the user's ChatGPT sign-in), or no
//!   summary.
//! - **The Claude Code model** (`settings/summary-claude-code-model`): what `--model` asks for
//!   (an alias such as `opus` or a full id), empty for Claude Code's own default; checked with
//!   [`earshot_llm::claude_code::check_model`].
//! - **The Codex model** (`settings/summary-codex-model`): what `-m` asks for (for example
//!   `gpt-5.6-luna`), empty for Codex's own default; checked with
//!   [`earshot_llm::codex::check_model`].
//! - **Your own endpoint** ([`EndpointProvider`], URL, model): checked with the rule a
//!   [`crate::summary::SummaryService`] applies ([`crate::summary::summary_allowlist`]: http
//!   only to loopback or a private LAN address, https otherwise, no credentials in the URL),
//!   plus no query or fragment (the address is shown on the page and stored as typed), and
//!   no part the client appends itself: Anthropic takes the API root only (no path), an
//!   OpenAI-compatible server its API base (usually `.../v1`) without `/chat/completions`. The
//!   endpoint is kept when another choice is made, so switching back needs no retyping.
//! - **The Anthropic API key** is never a setting: the app keeps it in the operating
//!   system's credential store and hands it over at use time through [`ApiKeyStore`]
//!   (PLAN T7.4g). Nothing here stores or prints it.
//! - **Summarise during the meeting**: `settings/live-summaries`
//!   ([`crate::live_summaries::LIVE_SUMMARIES_KEY`]), honoured only with a summary on this
//!   computer (`host::live_summaries_for`).
//!
//! A stored value this build does not know reads as the default and is reported
//! ([`UnknownSetting`]), as the Models page settings do; a stored endpoint the rules refuse
//! reads as no endpoint, and "your own endpoint" without a usable one reads as the default
//! (on this computer), reported.

use std::fmt;

use earshot_store::Store;

use crate::RuntimeError;
use crate::live_summaries::{LIVE_SUMMARIES_KEY, StoredSwitch, live_summaries_setting};
use crate::local::DeviceRecord;
pub use crate::model_settings::UnknownSetting;
use crate::summary::{SummaryConfig, summary_allowlist};
pub use earshot_llm::{ANTHROPIC_DEFAULT_URL, ApiKey};

#[cfg(test)]
#[path = "summary_settings_tests.rs"]
mod tests;

/// `local_state` key of who writes the summary.
pub const SUMMARY_WRITER_KEY: &str = "settings/summary-writer";
/// `local_state` key of the endpoint's provider.
pub const SUMMARY_PROVIDER_KEY: &str = "settings/summary-provider";
/// `local_state` key of the endpoint URL.
pub const SUMMARY_ENDPOINT_KEY: &str = "settings/summary-endpoint";
/// `local_state` key of the endpoint's model name.
pub const SUMMARY_ENDPOINT_MODEL_KEY: &str = "settings/summary-endpoint-model";
/// `local_state` key of the model Claude Code is asked for (T7.4ga).
pub const SUMMARY_CLAUDE_CODE_MODEL_KEY: &str = "settings/summary-claude-code-model";
/// `local_state` key of the model Codex is asked for (T7.4gc).
pub const SUMMARY_CODEX_MODEL_KEY: &str = "settings/summary-codex-model";

/// Longest model name accepted (bytes).
pub const MAX_MODEL_NAME: usize = 200;
/// Longest endpoint URL accepted (bytes).
pub const MAX_URL: usize = 2048;

/// Who writes the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SummaryWriter {
    /// The local model of the Models page; nothing leaves this computer.
    #[default]
    OnThisComputer,
    /// The user's own endpoint ([`SummarySettings::provider`], `url`, `model`).
    OwnEndpoint,
    /// Claude through the user's installed Claude Code CLI on their Claude.ai sign-in
    /// ([`SummarySettings::claude_model`]; T7.4ga).
    ClaudeCode,
    /// OpenAI through the user's installed Codex CLI on their ChatGPT sign-in
    /// ([`SummarySettings::codex_model`]; T7.4gc).
    Codex,
    /// No summary: transcript and speakers only.
    NoSummary,
}

impl SummaryWriter {
    /// Every value.
    pub const ALL: [Self; 5] = [
        Self::OnThisComputer,
        Self::OwnEndpoint,
        Self::ClaudeCode,
        Self::Codex,
        Self::NoSummary,
    ];

    /// The stored name: `local`, `endpoint`, `claude-code`, `codex`, `none`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnThisComputer => "local",
            Self::OwnEndpoint => "endpoint",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::NoSummary => "none",
        }
    }

    /// Reads [`SummaryWriter::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|w| w.as_str() == name)
    }
}

/// Which API the endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EndpointProvider {
    /// An OpenAI-compatible server (`<url>/chat/completions`).
    #[default]
    OpenAiCompatible,
    /// The Anthropic Messages API (`<url>/v1/messages`) with the user's API key.
    Anthropic,
}

impl EndpointProvider {
    /// Every value.
    pub const ALL: [Self; 2] = [Self::OpenAiCompatible, Self::Anthropic];

    /// The stored name: `openai`, `anthropic`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai",
            Self::Anthropic => "anthropic",
        }
    }

    /// Reads [`EndpointProvider::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }
}

/// Every Summaries page setting.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SummarySettings {
    /// Who writes the summary.
    pub writer: SummaryWriter,
    /// The endpoint's provider (kept when another writer is chosen).
    pub provider: EndpointProvider,
    /// The endpoint URL as typed (trimmed), empty when none is set.
    pub url: String,
    /// The model name sent to the endpoint (trimmed), empty when none is set.
    pub model: String,
    /// Summarise during the meeting (honoured only on this computer).
    pub live: bool,
    /// The model Claude Code is asked for (trimmed), empty for Claude Code's own default.
    pub claude_model: String,
    /// The model Codex is asked for (trimmed), empty for Codex's own default (T7.4gc).
    pub codex_model: String,
}

impl SummarySettings {
    /// Whether the endpoint fields are filled in (not whether they pass the rules).
    #[must_use]
    pub fn has_endpoint(&self) -> bool {
        !self.url.is_empty() && !self.model.is_empty()
    }

    /// The endpoint's host as the page names it ("api.anthropic.com"), `None` without a
    /// usable URL.
    #[must_use]
    pub fn endpoint_host(&self) -> Option<String> {
        let url = earshot_net::validate_url(&self.url).ok()?;
        url.host_str()
            .map(|h| h.trim_start_matches('[').trim_end_matches(']').to_owned())
    }

    /// Whether the endpoint is this computer or the local network (no warning on the
    /// page); `false` without a usable URL.
    #[must_use]
    pub fn endpoint_is_local(&self) -> bool {
        earshot_net::validate_url(&self.url).is_ok_and(|u| earshot_net::is_local_url(&u))
    }

    /// The service configuration of the endpoint with `api_key` (Anthropic only; an
    /// OpenAI-compatible endpoint is sent none).
    #[must_use]
    pub fn endpoint_config(&self, api_key: Option<ApiKey>) -> SummaryConfig {
        match self.provider {
            EndpointProvider::OpenAiCompatible => SummaryConfig::new(&self.url, &self.model),
            EndpointProvider::Anthropic => {
                SummaryConfig::anthropic(&self.url, &self.model, api_key)
            }
        }
    }
}

/// Why a Summaries setting was refused (nothing was stored).
#[derive(Debug)]
pub enum SummarySettingsError {
    /// "Your own endpoint" without an address.
    MissingUrl,
    /// "Your own endpoint" without a model name.
    MissingModel,
    /// The address is not one Earshot sends to (the reason, never the address).
    BadUrl(String),
    /// The model name is too long or holds control characters.
    BadModel,
    /// The Claude Code model name cannot be passed to Claude Code (why).
    BadClaudeModel(&'static str),
    /// The Codex model name cannot be passed to Codex (why).
    BadCodexModel(&'static str),
    /// Reading or writing the store failed.
    Store(RuntimeError),
}

impl fmt::Display for SummarySettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingUrl => write!(f, "enter the endpoint's address"),
            Self::MissingModel => write!(f, "enter the model name the endpoint expects"),
            Self::BadUrl(why) => write!(f, "this address cannot be used: {why}"),
            Self::BadModel => write!(
                f,
                "the model name must be at most {MAX_MODEL_NAME} characters, without line breaks"
            ),
            Self::BadClaudeModel(why) => {
                write!(f, "this Claude Code model name cannot be used: {why}")
            }
            Self::BadCodexModel(why) => {
                write!(f, "this Codex model name cannot be used: {why}")
            }
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SummarySettingsError {}

impl From<RuntimeError> for SummarySettingsError {
    fn from(e: RuntimeError) -> Self {
        Self::Store(e)
    }
}

/// Why the API key could not be used or kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeyError {
    /// No key is saved (Anthropic needs one).
    Missing,
    /// This platform has no credential store Earshot uses.
    Unsupported,
    /// The key is not one Earshot can store (empty, too long, spaces or line breaks).
    Invalid(&'static str),
    /// The credential store failed (its own error text; never the key).
    Store(String),
}

impl fmt::Display for ApiKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => write!(
                f,
                "summaries go to Anthropic, but no API key is saved: add it in Settings, \
                 Summaries, or choose another way to summarise"
            ),
            Self::Unsupported => write!(
                f,
                "keeping an API key needs Windows Credential Manager, which this system does \
                 not have"
            ),
            Self::Invalid(why) => write!(f, "this API key cannot be saved: {why}"),
            Self::Store(e) => write!(f, "Windows Credential Manager failed: {e}"),
        }
    }
}

impl std::error::Error for ApiKeyError {}

/// Where the app keeps the Anthropic API key (Windows Credential Manager in the desktop
/// app, a fake in tests). Read only when a summary service is built.
pub trait ApiKeyStore {
    /// The saved key, `None` when there is none.
    ///
    /// # Errors
    /// The store failed ([`ApiKeyError::Store`], [`ApiKeyError::Unsupported`]).
    fn anthropic_key(&self) -> Result<Option<ApiKey>, ApiKeyError>;
}

/// No key anywhere (hosts without a credential store, and tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoApiKeys;

impl ApiKeyStore for NoApiKeys {
    fn anthropic_key(&self) -> Result<Option<ApiKey>, ApiKeyError> {
        Ok(None)
    }
}

/// Why `url` (non-empty) cannot be an endpoint: the service's own rule for `provider`
/// ([`summary_allowlist`]) and no query or fragment. Never names the URL.
fn check_url(provider: EndpointProvider, url: &str) -> Result<(), String> {
    if url.len() > MAX_URL {
        return Err(format!("it is longer than {MAX_URL} characters"));
    }
    let parsed = earshot_net::validate_url(url).map_err(|e| e.to_string())?;
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("it must not contain a query (?) or a fragment (#)".into());
    }
    // The clients append their own path (`LlmConfig::request_url`): Anthropic
    // `<url>/v1/messages` onto the API root, OpenAI-compatible `<url>/chat/completions`
    // onto the server's API base (usually ending in `/v1`). An address that already holds
    // the appended part would double it.
    let path = parsed.path().trim_end_matches('/');
    match provider {
        EndpointProvider::Anthropic if !path.is_empty() => {
            return Err(format!(
                "use the address without a path, for example {ANTHROPIC_DEFAULT_URL}"
            ));
        }
        EndpointProvider::OpenAiCompatible if path.ends_with("/chat/completions") => {
            return Err(
                "use the address without /chat/completions, for example http://127.0.0.1:8080/v1"
                    .into(),
            );
        }
        _ => {}
    }
    // Model and key take no part in the address rule.
    let probe = SummarySettings {
        provider,
        url: url.to_owned(),
        model: "probe".to_owned(),
        ..SummarySettings::default()
    };
    summary_allowlist(&probe.endpoint_config(None))
        .map(|_| ())
        .map_err(|e| match e {
            RuntimeError::EndpointDenied(earshot_net::DenyReason::InsecureScheme) => {
                "plain http is only allowed to this computer or the local network; use https"
                    .to_owned()
            }
            RuntimeError::EndpointDenied(d) => d.to_string(),
            RuntimeError::Endpoint(u) | RuntimeError::Llm(earshot_llm::LlmError::Endpoint(u)) => {
                u.to_string()
            }
            other => other.to_string(),
        })
}

/// The settings `settings` would be stored as: URL and model trimmed, everything checked
/// (module docs). An endpoint that is filled in is checked whatever the writer, so nothing
/// unusable is ever stored; "your own endpoint" needs both fields.
///
/// # Errors
/// [`SummarySettingsError`] (never [`SummarySettingsError::Store`]).
pub fn validate(settings: &SummarySettings) -> Result<SummarySettings, SummarySettingsError> {
    let url = settings.url.trim().to_owned();
    let model = settings.model.trim().to_owned();
    if model.len() > MAX_MODEL_NAME || model.chars().any(char::is_control) {
        return Err(SummarySettingsError::BadModel);
    }
    let claude_model = settings.claude_model.trim().to_owned();
    earshot_llm::claude_code::check_model(&claude_model)
        .map_err(SummarySettingsError::BadClaudeModel)?;
    let codex_model = settings.codex_model.trim().to_owned();
    earshot_llm::codex::check_model(&codex_model).map_err(SummarySettingsError::BadCodexModel)?;
    if !url.is_empty() {
        check_url(settings.provider, &url).map_err(SummarySettingsError::BadUrl)?;
    }
    if settings.writer == SummaryWriter::OwnEndpoint {
        if url.is_empty() {
            return Err(SummarySettingsError::MissingUrl);
        }
        if model.is_empty() {
            return Err(SummarySettingsError::MissingModel);
        }
    }
    Ok(SummarySettings {
        url,
        model,
        claude_model,
        codex_model,
        ..settings.clone()
    })
}

/// The stored settings (absent values are the defaults) and every stored value this build
/// does not know or refuses (the default is used; a refused URL is reported by its reason,
/// never by its text).
///
/// # Errors
/// Store errors.
pub fn summary_settings(
    store: &Store,
) -> Result<(SummarySettings, Vec<UnknownSetting>), RuntimeError> {
    let mut unknown = Vec::new();
    let mut out = SummarySettings::default();
    let text = |key: &str| -> Result<Option<String>, RuntimeError> {
        Ok(store
            .local_value(key)?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    };
    let mut writer = None;
    if let Some(t) = text(SUMMARY_WRITER_KEY)? {
        match SummaryWriter::parse(&t) {
            Some(w) => writer = Some(w),
            None => unknown.push(UnknownSetting {
                key: SUMMARY_WRITER_KEY,
                stored: t,
            }),
        }
    }
    if let Some(t) = text(SUMMARY_PROVIDER_KEY)? {
        match EndpointProvider::parse(&t) {
            Some(p) => out.provider = p,
            None => unknown.push(UnknownSetting {
                key: SUMMARY_PROVIDER_KEY,
                stored: t,
            }),
        }
    }
    if let Some(t) = text(SUMMARY_ENDPOINT_MODEL_KEY)? {
        if t.len() <= MAX_MODEL_NAME && !t.chars().any(char::is_control) {
            out.model = t;
        } else {
            unknown.push(UnknownSetting {
                key: SUMMARY_ENDPOINT_MODEL_KEY,
                stored: "a model name this version refuses".into(),
            });
        }
    }
    if let Some(t) = text(SUMMARY_CLAUDE_CODE_MODEL_KEY)? {
        match earshot_llm::claude_code::check_model(&t) {
            Ok(()) => out.claude_model = t,
            Err(why) => unknown.push(UnknownSetting {
                key: SUMMARY_CLAUDE_CODE_MODEL_KEY,
                stored: format!("a Claude Code model name this version refuses: {why}"),
            }),
        }
    }
    if let Some(t) = text(SUMMARY_CODEX_MODEL_KEY)? {
        match earshot_llm::codex::check_model(&t) {
            Ok(()) => out.codex_model = t,
            Err(why) => unknown.push(UnknownSetting {
                key: SUMMARY_CODEX_MODEL_KEY,
                stored: format!("a Codex model name this version refuses: {why}"),
            }),
        }
    }
    if let Some(t) = text(SUMMARY_ENDPOINT_KEY)?.filter(|t| !t.is_empty()) {
        match check_url(out.provider, &t) {
            Ok(()) => out.url = t,
            Err(why) => unknown.push(UnknownSetting {
                key: SUMMARY_ENDPOINT_KEY,
                stored: format!("an address this version refuses: {why}"),
            }),
        }
    }
    out.writer = match writer {
        Some(SummaryWriter::OwnEndpoint) if !out.has_endpoint() => {
            unknown.push(UnknownSetting {
                key: SUMMARY_WRITER_KEY,
                stored: "your own endpoint, without a usable address and model".into(),
            });
            SummaryWriter::default()
        }
        Some(w) => w,
        None => SummaryWriter::default(),
    };
    match live_summaries_setting(store)? {
        StoredSwitch::Set(on) => out.live = on,
        StoredSwitch::Absent => {}
        StoredSwitch::Unknown(t) => unknown.push(UnknownSetting {
            key: LIVE_SUMMARIES_KEY,
            stored: t,
        }),
    }
    Ok((out, unknown))
}

/// Checks and stores every setting (replacing earlier ones) in one transaction; returns
/// what was stored.
///
/// # Errors
/// [`SummarySettingsError`]: nothing is stored on a refusal.
pub fn set_summary_settings(
    store: &mut Store,
    settings: &SummarySettings,
) -> Result<SummarySettings, SummarySettingsError> {
    let s = validate(settings)?;
    let values: [(DeviceRecord, &str); 7] = [
        (DeviceRecord::SummaryWriter, s.writer.as_str()),
        (DeviceRecord::SummaryProvider, s.provider.as_str()),
        (DeviceRecord::SummaryEndpoint, s.url.as_str()),
        (DeviceRecord::SummaryEndpointModel, s.model.as_str()),
        (
            DeviceRecord::SummaryClaudeCodeModel,
            s.claude_model.as_str(),
        ),
        (DeviceRecord::SummaryCodexModel, s.codex_model.as_str()),
        (
            DeviceRecord::LiveSummaries,
            if s.live { "on" } else { "off" },
        ),
    ];
    store
        .transaction(|tx| -> Result<(), earshot_store::StoreError> {
            for (record, value) in values {
                crate::local::set_tx(tx, &record.key(), value.as_bytes())?;
            }
            Ok(())
        })
        .map_err(RuntimeError::from)?;
    Ok(s)
}
