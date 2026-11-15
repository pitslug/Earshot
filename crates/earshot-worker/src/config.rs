//! The worker's configuration, from environment variables (as the relay's). None of them holds
//! a secret: the vault key and token are files ([`crate::vault`]), and nothing reads them from
//! the environment.
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `EARSHOT_WORKER_DATA` | `/data` | The data folder: the worker's database, its vault files and its heartbeat |
//! | `EARSHOT_WORKER_RELAY` | (required for `join` and `run`) | The relay's address, as the other devices use it (`https://...`) |
//! | `EARSHOT_WORKER_LLM_URL` | `http://llama:8080/v1` | The OpenAI-compatible summary endpoint ([`crate::endpoint`] decides whether it is the worker's own model) |
//! | `EARSHOT_WORKER_LLM_SERVICE` | `llama` | The model service's name on the compose network: the only bare name that can count as the worker's own model |
//! | `EARSHOT_WORKER_LLM_MODEL` | `local` | The model name sent with each request and stored with the summary |
//! | `EARSHOT_WORKER_LLM_CONTEXT` | `16384` | The endpoint's context window in tokens (the compose file starts llama.cpp with the same) |
//! | `EARSHOT_WORKER_LLM_TIMEOUT_S` | `1800` | The longest one summary request may take |
//! | `EARSHOT_WORKER_NAME` | `Earshot server` | The name the other devices show |
//! | `EARSHOT_WORKER_KEY_FILE` | `<data>/vault/key` | The vault key file (a Docker secret's path may be given) |
//! | `EARSHOT_WORKER_TOKEN_FILE` | `<data>/vault/token` | The device token file (likewise) |

use std::path::PathBuf;
use std::time::Duration;

use earshot_net::Url;

/// The configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The data folder.
    pub data: PathBuf,
    /// The relay's address (checked when used).
    pub relay: Option<String>,
    /// The summary endpoint.
    pub llm_url: Url,
    /// The model service's name on the compose network ([`crate::endpoint`]).
    pub llm_service: String,
    /// The model name.
    pub llm_model: String,
    /// The endpoint's context window in tokens.
    pub llm_context: usize,
    /// The longest one request may take.
    pub llm_timeout: Duration,
    /// The name the other devices show.
    pub name: String,
    /// The vault key file.
    pub key_file: PathBuf,
    /// The device token file.
    pub token_file: PathBuf,
}

/// A configuration value that cannot be used, by the variable's name. Never holds the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigError(pub &'static str);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is not usable", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// The smallest context accepted: room for the prompt's fixed rules and a reply.
pub const MIN_CONTEXT: usize = 4096;

impl Config {
    /// The configuration from the process environment.
    ///
    /// # Errors
    /// [`ConfigError`].
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// The configuration from `var` (a variable's value by name; tests pass their own).
    ///
    /// # Errors
    /// [`ConfigError`].
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |name: &str| {
            var(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let data = PathBuf::from(get("EARSHOT_WORKER_DATA").unwrap_or_else(|| "/data".into()));
        let llm_url = Url::parse(
            &get("EARSHOT_WORKER_LLM_URL").unwrap_or_else(|| "http://llama:8080/v1".into()),
        )
        .map_err(|_| ConfigError("EARSHOT_WORKER_LLM_URL"))?;
        let llm_service = get("EARSHOT_WORKER_LLM_SERVICE").unwrap_or_else(|| "llama".into());
        let label = !llm_service.is_empty()
            && llm_service.len() <= 63
            && !llm_service.eq_ignore_ascii_case("localhost")
            && llm_service
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !label {
            return Err(ConfigError("EARSHOT_WORKER_LLM_SERVICE"));
        }
        let number = |name: &'static str, default: u64| -> Result<u64, ConfigError> {
            get(name).map_or(Ok(default), |v| v.parse().map_err(|_| ConfigError(name)))
        };
        let llm_context = usize::try_from(number("EARSHOT_WORKER_LLM_CONTEXT", 16_384)?)
            .map_err(|_| ConfigError("EARSHOT_WORKER_LLM_CONTEXT"))?;
        if llm_context < MIN_CONTEXT {
            return Err(ConfigError("EARSHOT_WORKER_LLM_CONTEXT"));
        }
        let timeout = number("EARSHOT_WORKER_LLM_TIMEOUT_S", 1800)?;
        if timeout == 0 {
            return Err(ConfigError("EARSHOT_WORKER_LLM_TIMEOUT_S"));
        }
        let name = earshot_runtime::sync::clean_name(
            &get("EARSHOT_WORKER_NAME").unwrap_or_else(|| "Earshot server".into()),
        )
        .ok_or(ConfigError("EARSHOT_WORKER_NAME"))?;
        Ok(Self {
            key_file: get("EARSHOT_WORKER_KEY_FILE")
                .map_or_else(|| data.join("vault").join("key"), PathBuf::from),
            token_file: get("EARSHOT_WORKER_TOKEN_FILE")
                .map_or_else(|| data.join("vault").join("token"), PathBuf::from),
            data,
            relay: get("EARSHOT_WORKER_RELAY"),
            llm_url,
            llm_service,
            llm_model: get("EARSHOT_WORKER_LLM_MODEL").unwrap_or_else(|| "local".into()),
            llm_context,
            llm_timeout: Duration::from_secs(timeout),
            name,
        })
    }

    /// The relay's address.
    ///
    /// # Errors
    /// [`ConfigError`] when none is set.
    pub fn relay(&self) -> Result<&str, ConfigError> {
        self.relay
            .as_deref()
            .ok_or(ConfigError("EARSHOT_WORKER_RELAY"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::from_vars(|name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        })
    }

    #[test]
    fn config_defaults_name_the_compose_layout() {
        let c = of(&[]).unwrap();
        assert_eq!(c.data, PathBuf::from("/data"));
        assert_eq!(c.relay, None);
        assert_eq!(c.relay(), Err(ConfigError("EARSHOT_WORKER_RELAY")));
        assert_eq!(c.llm_url.as_str(), "http://llama:8080/v1");
        assert_eq!(c.llm_service, "llama");
        assert_eq!(c.llm_model, "local");
        assert_eq!(c.llm_context, 16_384);
        assert_eq!(c.llm_timeout, Duration::from_secs(1800));
        assert_eq!(c.name, "Earshot server");
        assert_eq!(c.key_file, PathBuf::from("/data").join("vault").join("key"));
        assert_eq!(
            c.token_file,
            PathBuf::from("/data").join("vault").join("token")
        );
    }

    #[test]
    fn config_values_are_read_and_bad_ones_named_without_their_text() {
        let c = of(&[
            ("EARSHOT_WORKER_DATA", "/srv/worker"),
            ("EARSHOT_WORKER_RELAY", " https://sync.example.com "),
            ("EARSHOT_WORKER_LLM_URL", "http://llama-gpu:8080/v1"),
            ("EARSHOT_WORKER_LLM_CONTEXT", "32768"),
            ("EARSHOT_WORKER_NAME", "  Invented rack  "),
            ("EARSHOT_WORKER_KEY_FILE", "/run/secrets/earshot_vault_key"),
        ])
        .unwrap();
        assert_eq!(c.relay().unwrap(), "https://sync.example.com");
        assert_eq!(c.llm_context, 32_768);
        assert_eq!(c.name, "Invented rack");
        assert_eq!(c.key_file, PathBuf::from("/run/secrets/earshot_vault_key"));
        assert_eq!(
            c.token_file,
            PathBuf::from("/srv/worker").join("vault").join("token")
        );
        for (name, value) in [
            ("EARSHOT_WORKER_LLM_URL", "not a url"),
            ("EARSHOT_WORKER_LLM_CONTEXT", "lots"),
            ("EARSHOT_WORKER_LLM_CONTEXT", "1024"),
            ("EARSHOT_WORKER_LLM_TIMEOUT_S", "0"),
            ("EARSHOT_WORKER_LLM_SERVICE", "llama.lan"),
            ("EARSHOT_WORKER_LLM_SERVICE", "localhost"),
            ("EARSHOT_WORKER_NAME", "\u{7}"),
        ] {
            let e = of(&[(name, value)]).unwrap_err();
            assert_eq!(e, ConfigError(name));
            assert!(!e.to_string().contains(value), "{e}");
        }
    }
}
