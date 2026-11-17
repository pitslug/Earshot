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
//! | `EARSHOT_WORKER_MODEL_SUBNET` | (required when the endpoint names the model service) | The model network's fixed private subnet (CIDR, the compose file's `172.31.250.0/24`): only addresses inside it can be the worker's own model |
//! | `EARSHOT_WORKER_LLM_MODEL` | `local` | The model name sent with each request and stored with the summary |
//! | `EARSHOT_WORKER_LLM_CONTEXT` | `16384` | The endpoint's context window in tokens (the compose file starts llama.cpp with the same) |
//! | `EARSHOT_WORKER_LLM_TIMEOUT_S` | `1800` | The longest one summary request may take |
//! | `EARSHOT_WORKER_NAME` | `Earshot server` | The name the other devices show |
//! | `EARSHOT_WORKER_MODEL_PLACE` | (none) | Where the model server runs, from the compose `.env` (a label set beside the profile): `cpu` or `gpu`, the profile's own word (`processor` and `graphics-card`, 0.3.11's words, still read), empty or unset for none; presence names it beside "local model" while the endpoint is the worker's own model (T11.6a). Any other value refuses to start |
//! | `EARSHOT_WORKER_KEY_FILE` | `<data>/vault/key` | The vault key file (a Docker secret's path may be given) |
//! | `EARSHOT_WORKER_TOKEN_FILE` | `<data>/vault/token` | The device token file (likewise) |

use std::path::PathBuf;
use std::time::Duration;

use earshot_net::Url;

use earshot_runtime::sync::ModelPlace;

use crate::endpoint::{OwnModel, Subnet, service_name};

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
    /// The model network's fixed subnet ([`crate::endpoint`]); `None`: nothing is the
    /// worker's own model.
    pub model_subnet: Option<Subnet>,
    /// The model name.
    pub llm_model: String,
    /// The endpoint's context window in tokens.
    pub llm_context: usize,
    /// The longest one request may take.
    pub llm_timeout: Duration,
    /// The name the other devices show.
    pub name: String,
    /// Where the model server runs (the `.env` line beside the compose profile, T11.6a); `None`
    /// when not set, and presence then names no place.
    pub model_place: Option<ModelPlace>,
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
        let model_subnet = match get("EARSHOT_WORKER_MODEL_SUBNET") {
            Some(text) => Some(
                Subnet::parse_private(&text).ok_or(ConfigError("EARSHOT_WORKER_MODEL_SUBNET"))?,
            ),
            None => None,
        };
        // An endpoint naming the model service is meant to be the worker's own model, which
        // needs the subnet (without it, it would silently be remote).
        if model_subnet.is_none()
            && service_name(&llm_url).is_some_and(|n| n.eq_ignore_ascii_case(&llm_service))
        {
            return Err(ConfigError("EARSHOT_WORKER_MODEL_SUBNET"));
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
        let model_place = match get("EARSHOT_WORKER_MODEL_PLACE") {
            Some(word) => {
                Some(place_of_env(&word).ok_or(ConfigError("EARSHOT_WORKER_MODEL_PLACE"))?)
            }
            None => None,
        };
        Ok(Self {
            key_file: get("EARSHOT_WORKER_KEY_FILE")
                .map_or_else(|| data.join("vault").join("key"), PathBuf::from),
            token_file: get("EARSHOT_WORKER_TOKEN_FILE")
                .map_or_else(|| data.join("vault").join("token"), PathBuf::from),
            data,
            relay: get("EARSHOT_WORKER_RELAY"),
            llm_url,
            llm_service,
            model_subnet,
            llm_model: get("EARSHOT_WORKER_LLM_MODEL").unwrap_or_else(|| "local".into()),
            llm_context,
            llm_timeout: Duration::from_secs(timeout),
            name,
            model_place,
        })
    }

    /// The data folder alone, from the process environment (the health check needs nothing
    /// else, so it answers even when another variable is wrong).
    #[must_use]
    pub fn data_from_env() -> PathBuf {
        PathBuf::from(
            std::env::var("EARSHOT_WORKER_DATA")
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "/data".into()),
        )
    }

    /// What counts as the worker's own model; `None` without a subnet.
    #[must_use]
    pub fn own_model(&self) -> Option<OwnModel> {
        self.model_subnet.map(|subnet| OwnModel {
            service: self.llm_service.clone(),
            subnet,
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

/// The `.env` word for a model on the processor: the compose profile's own name.
pub const ENV_CPU: &str = "cpu";
/// The `.env` word for a model on the graphics card: the compose profile's own name.
pub const ENV_GPU: &str = "gpu";

/// The place an `EARSHOT_WORKER_MODEL_PLACE` value names: the profile words [`ENV_CPU`] and
/// [`ENV_GPU`] (the owner, 2026-11-17), or 0.3.11's presence words, which a `.env` written for
/// that release may still hold. Exact, lower case; `None` for anything else.
fn place_of_env(word: &str) -> Option<ModelPlace> {
    match word {
        ENV_CPU => Some(ModelPlace::Processor),
        ENV_GPU => Some(ModelPlace::GraphicsCard),
        other => ModelPlace::parse(other),
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
        let c = of(&[("EARSHOT_WORKER_MODEL_SUBNET", "172.31.250.0/24")]).unwrap();
        assert_eq!(c.model_subnet.unwrap().to_string(), "172.31.250.0/24");
        assert_eq!(c.own_model().unwrap().service, "llama");
        assert_eq!(c.data, PathBuf::from("/data"));
        assert_eq!(c.relay, None);
        assert_eq!(c.relay(), Err(ConfigError("EARSHOT_WORKER_RELAY")));
        assert_eq!(c.llm_url.as_str(), "http://llama:8080/v1");
        assert_eq!(c.llm_service, "llama");
        assert_eq!(c.llm_model, "local");
        assert_eq!(c.llm_context, 16_384);
        assert_eq!(c.llm_timeout, Duration::from_secs(1800));
        assert_eq!(c.name, "Earshot server");
        assert_eq!(c.model_place, None);
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
            ("EARSHOT_WORKER_MODEL_PLACE", "graphics card"),
            ("EARSHOT_WORKER_MODEL_PLACE", "Processor"),
            ("EARSHOT_WORKER_MODEL_PLACE", "GPU"),
            ("EARSHOT_WORKER_MODEL_PLACE", "cuda"),
        ] {
            let e = of(&[
                (name, value),
                ("EARSHOT_WORKER_MODEL_SUBNET", "172.31.250.0/24"),
            ])
            .unwrap_err();
            assert_eq!(e, ConfigError(name));
            assert!(!e.to_string().contains(value), "{e}");
        }
    }

    /// T11.6a: each place word is read (the profiles' `cpu` and `gpu`, and 0.3.11's presence
    /// words); the variable left out names no place.
    #[test]
    fn config_model_place_words_are_read() {
        let subnet = ("EARSHOT_WORKER_MODEL_SUBNET", "172.31.250.0/24");
        for (word, place) in [
            ("cpu", ModelPlace::Processor),
            ("gpu", ModelPlace::GraphicsCard),
            (" gpu ", ModelPlace::GraphicsCard),
            ("processor", ModelPlace::Processor),
            ("graphics-card", ModelPlace::GraphicsCard),
        ] {
            let c = of(&[subnet, ("EARSHOT_WORKER_MODEL_PLACE", word)]).unwrap();
            assert_eq!(c.model_place, Some(place), "{word}");
        }
        let c = of(&[subnet, ("EARSHOT_WORKER_MODEL_PLACE", "  ")]).unwrap();
        assert_eq!(c.model_place, None);
    }

    /// The compose file passes the place from `.env` through to the one worker service, empty
    /// when `.env` leaves it out (which `of` reads as no place), and documents both words.
    #[test]
    fn config_compose_passes_the_model_place_from_env() {
        let compose = include_str!("../docker-compose.yml").replace("\r\n", "\n");
        assert!(
            compose
                .contains("\n      EARSHOT_WORKER_MODEL_PLACE: ${EARSHOT_WORKER_MODEL_PLACE:-}\n"),
            "the worker service does not pass EARSHOT_WORKER_MODEL_PLACE"
        );
        assert_eq!(compose.matches("\n  earshot-worker:\n").count(), 1);
        let documented = format!(
            "#      EARSHOT_WORKER_MODEL_PLACE=<{}, or {}: the same word as the profile below>\n",
            ENV_CPU, ENV_GPU
        );
        assert!(
            compose.contains(&documented),
            "the .env line is not documented"
        );
        let subnet = ("EARSHOT_WORKER_MODEL_SUBNET", "172.31.250.0/24");
        let c = of(&[subnet, ("EARSHOT_WORKER_MODEL_PLACE", "")]).unwrap();
        assert_eq!(c.model_place, None);
    }

    #[test]
    fn config_the_model_service_needs_its_subnet() {
        // The default endpoint names the model service: without the subnet it would silently be
        // remote, so it is refused.
        assert_eq!(
            of(&[]).unwrap_err(),
            ConfigError("EARSHOT_WORKER_MODEL_SUBNET")
        );
        for bad in ["192.0.2.0/24", "172.31.250.1/24", "10.0.0.0/33", "lots"] {
            assert_eq!(
                of(&[("EARSHOT_WORKER_MODEL_SUBNET", bad)]).unwrap_err(),
                ConfigError("EARSHOT_WORKER_MODEL_SUBNET"),
                "{bad}"
            );
        }
        // Another endpoint needs none: nothing is then the worker's own model.
        let c = of(&[("EARSHOT_WORKER_LLM_URL", "https://api.example.com/v1")]).unwrap();
        assert_eq!(c.own_model(), None);
    }
}
