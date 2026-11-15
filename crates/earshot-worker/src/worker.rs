//! The worker itself (T11.6): joining the vault, and the paired device that answers summary
//! requests.
//!
//! The worker is an ordinary paired device with no UI: the sync thread
//! ([`earshot_runtime::sync::SyncThread::start_offloading`]) pushes and pulls sealed rows and
//! sends presence (kind `server`), and runs [`earshot_runtime::offload::Offload::step`] after
//! every tick, which claims a request addressed to this device and hands it to [`WorkerHost`].
//! The host writes it with [`earshot_runtime::offload::write_for_request`]: the app's own
//! summary path (prompts, schema, validation, the store transaction that re-reads the request),
//! with the configured endpoint as its writer.
//!
//! It records nothing and holds no audio (audio rows never sync). It never applies voiceprints:
//! voiceprint sync stays off, nothing here can turn it on, and a store where it is on is refused
//! ([`WorkerError::VoiceprintsOn`]). It asks no other device for anything.

use std::path::Path;
use std::sync::Arc;

use earshot_core::asr::CancelToken;
use earshot_core::model::SummaryRequest;
use earshot_net::Url;
use earshot_runtime::keep_local::SummaryPrivacy;
use earshot_runtime::meeting_queue::{MeetingQueue, SummaryHold};
use earshot_runtime::offload::{HelperWriter, OffloadHost, WriteHere, WriterKind, reason};
use earshot_runtime::summary::{LOCAL_MAX_OUTPUT_TOKENS, SummaryConfig, SummaryService};
use earshot_runtime::sync::{SyncClient, SyncProblem, SyncThread, SyncTiming, SyncVault};
use earshot_runtime::{Services, SharedClock, SystemClock};
use earshot_store::{DataDir, Store};

use crate::config::{Config, ConfigError};
use crate::endpoint::{Place, locate};
use crate::vault::{FileVault, VaultRefused};

/// Why the worker cannot do what was asked, as fixed words. Never holds a key, a token, a code
/// or an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerError {
    /// A configuration value cannot be used.
    Config(ConfigError),
    /// No vault key and token are stored: run `join` first.
    NotJoined,
    /// Voiceprint sync is on in this store: the worker never applies voiceprints.
    VoiceprintsOn,
    /// The data folder or the database failed.
    Store,
    /// Sync failed with this problem.
    Sync(SyncProblem),
    /// A thread could not be started.
    Spawn,
    /// The vault files cannot be used ([`VaultRefused`]).
    Vault(VaultRefused),
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "{e}"),
            Self::NotJoined => f.write_str("not joined: run `earshot-worker join` first"),
            Self::VoiceprintsOn => {
                f.write_str("voiceprint sync is on; the worker never applies voiceprints")
            }
            Self::Store => f.write_str("the data folder or its database failed"),
            Self::Sync(SyncProblem::UnknownDevice) => f.write_str(
                "this worker was removed from the vault: wipe its volume and join again",
            ),
            Self::Sync(p) => write!(f, "sync: {}", p.word()),
            Self::Vault(r) => f.write_str(r.copy()),
            Self::Spawn => f.write_str("a thread could not be started"),
        }
    }
}

impl std::error::Error for WorkerError {}

impl From<ConfigError> for WorkerError {
    fn from(e: ConfigError) -> Self {
        Self::Config(e)
    }
}

impl WorkerError {
    /// The word the health check reports for a run that ended on this (a fixed word).
    #[must_use]
    pub const fn health_word(self) -> &'static str {
        match self {
            Self::Config(_) => "config",
            Self::NotJoined => "not_joined",
            Self::VoiceprintsOn => "voiceprints_on",
            Self::Store => "store",
            Self::Sync(SyncProblem::UnknownDevice) => "removed",
            Self::Sync(p) => p.word(),
            Self::Spawn => "spawn",
            Self::Vault(VaultRefused::NotWritable) => "vault_not_writable",
            Self::Vault(VaultRefused::ReadableByOthers) => "vault_readable_by_others",
        }
    }
}

impl From<VaultRefused> for WorkerError {
    fn from(r: VaultRefused) -> Self {
        Self::Vault(r)
    }
}

impl From<SyncProblem> for WorkerError {
    fn from(p: SyncProblem) -> Self {
        Self::Sync(p)
    }
}

/// The runtime services over the worker's data folder (created if missing), with the system
/// clock.
///
/// # Errors
/// [`WorkerError::Store`].
pub fn open_services(data: &Path) -> Result<Services, WorkerError> {
    let data = DataDir::new(data);
    data.create().map_err(|_| WorkerError::Store)?;
    let store = Store::open(&data.db_path()).map_err(|_| WorkerError::Store)?;
    let clock: SharedClock = Arc::new(SystemClock);
    Services::new(store, data, clock).map_err(|_| WorkerError::Store)
}

/// The vault of `config`.
#[must_use]
pub fn vault_of(config: &Config) -> FileVault {
    FileVault::new(&config.key_file, &config.token_file)
}

/// Joins the vault of the configured relay with a pairing code shown on a device already in it,
/// storing the key and token in the vault files, then names this device and offers summaries.
/// Both files' folders are proven writable first, so a code is never spent (and the device
/// registered at the relay) when the key could not be kept.
///
/// # Errors
/// [`WorkerError::Config`] without a relay; [`WorkerError::VoiceprintsOn`];
/// [`WorkerError::Vault`] when the files cannot be written; the pairing's [`SyncProblem`]s (a
/// wrong or used code, a device already joined).
pub fn join(services: &Services, config: &Config, code: &str) -> Result<(), WorkerError> {
    let relay = config.relay()?;
    refuse_voiceprints(services)?;
    let vault = vault_of(config);
    vault.probe_writable()?;
    earshot_runtime::sync::join_with_code(services, &vault, relay, code)?;
    present_as_worker(services, config)
}

fn refuse_voiceprints(services: &Services) -> Result<(), WorkerError> {
    let settings = earshot_runtime::sync::sync_settings(&services.store())?;
    if settings.voiceprints {
        return Err(WorkerError::VoiceprintsOn);
    }
    Ok(())
}

/// The name the other devices show, and "Write summaries for my other devices" on.
fn present_as_worker(services: &Services, config: &Config) -> Result<(), WorkerError> {
    earshot_runtime::sync::set_device_name(services, &config.name)?;
    earshot_runtime::sync::set_offer_summaries(services, true)?;
    Ok(())
}

/// Finds where the summary endpoint is ([`locate`] in production; tests give their own).
pub type Locate = dyn Fn(&Url) -> Place + Send + Sync;

/// The kind of writer the worker's presence names for `place`: its own local model, or an
/// OpenAI-compatible endpoint.
#[must_use]
pub const fn writer_kind(place: &Place) -> WriterKind {
    if place.is_own_network() {
        WriterKind::Local
    } else {
        WriterKind::OpenaiCompatible
    }
}

/// A joined worker, ready to run.
pub struct Worker {
    services: Services,
    config: Arc<Config>,
    vault: Arc<FileVault>,
    locate: Arc<Locate>,
    cancel: CancelToken,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").finish_non_exhaustive()
    }
}

impl Worker {
    /// The worker over `services` with the configured endpoint located by DNS ([`locate`],
    /// with the configured model service's name).
    ///
    /// # Errors
    /// As [`Worker::open_with`].
    pub fn open(services: Services, config: Config) -> Result<Self, WorkerError> {
        let own = config.own_model();
        Self::open_with(
            services,
            config,
            Arc::new(move |url: &Url| locate(url, own.as_ref())),
        )
    }

    /// The worker over `services`, its endpoint located by `locate`. Refuses a store with
    /// voiceprint sync on, and one without a vault key and token; names this device and offers
    /// summaries.
    ///
    /// # Errors
    /// [`WorkerError::Config`] without a relay; [`WorkerError::VoiceprintsOn`];
    /// [`WorkerError::NotJoined`]; [`WorkerError::Vault`] for a file others can read;
    /// [`WorkerError::Store`].
    pub fn open_with(
        services: Services,
        config: Config,
        locate: Arc<Locate>,
    ) -> Result<Self, WorkerError> {
        config.relay()?;
        refuse_voiceprints(&services)?;
        let vault = Arc::new(vault_of(&config));
        if !vault.has().map_err(|_| WorkerError::Store)? {
            return Err(WorkerError::NotJoined);
        }
        vault.check_private()?;
        present_as_worker(&services, &config)?;
        Ok(Self {
            services,
            config: Arc::new(config),
            vault,
            locate,
            cancel: CancelToken::new(),
        })
    }

    /// The services.
    #[must_use]
    pub fn services(&self) -> &Services {
        &self.services
    }

    /// The sync client for the configured relay, presenting as a summary worker. The writer
    /// kind is where the endpoint is at each presence send (located again every time), so a
    /// model container that starts after the worker is named correctly from its next beat.
    ///
    /// # Errors
    /// [`WorkerError::Sync`] with [`SyncProblem::Address`] for a refused relay address.
    pub fn client(&self) -> Result<SyncClient, WorkerError> {
        let vault: Arc<dyn SyncVault> = Arc::clone(&self.vault) as Arc<dyn SyncVault>;
        let (locate, url) = (Arc::clone(&self.locate), self.config.llm_url.clone());
        Ok(
            SyncClient::for_address(self.services.clone(), vault, self.config.relay()?)?
                .as_summary_worker(Arc::new(move || Some(writer_kind(&locate(&url))))),
        )
    }

    /// The host the sync thread hands requests to.
    #[must_use]
    pub fn host(&self) -> Arc<WorkerHost> {
        Arc::new(WorkerHost {
            services: self.services.clone(),
            config: Arc::clone(&self.config),
            locate: Arc::clone(&self.locate),
            queue: MeetingQueue::new(),
            cancel: self.cancel.clone(),
        })
    }

    /// Starts the sync thread with summary offload.
    ///
    /// # Errors
    /// [`WorkerError::Sync`]; [`WorkerError::Spawn`].
    pub fn start(&self, timing: SyncTiming) -> Result<SyncThread, WorkerError> {
        let host: Arc<dyn OffloadHost> = self.host();
        SyncThread::start_offloading(self.client()?, timing, host).map_err(|_| WorkerError::Spawn)
    }

    /// Cancels summaries in progress (the worker is stopping): a cancelled request stays
    /// claimed and is handed again at the next start, or withdrawn by its asker.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// The worker's [`OffloadHost`]: each request is written on its own thread
/// (`earshot-worker-offload`) in the worker's meeting queue, one at a time.
pub struct WorkerHost {
    services: Services,
    config: Arc<Config>,
    locate: Arc<Locate>,
    queue: MeetingQueue,
    cancel: CancelToken,
}

impl WorkerHost {
    /// The writer for a meeting of `privacy`, the endpoint located now: its own model writes
    /// any meeting; a remote endpoint writes only a meeting not kept local, and a kept-local one
    /// is declined `kept_local_no_model` before anything is sent.
    #[must_use]
    pub fn writer(&self, privacy: SummaryPrivacy) -> HelperWriter {
        let place = (self.locate)(&self.config.llm_url);
        if !place.is_own_network() && privacy == SummaryPrivacy::KeptLocal {
            return HelperWriter::Decline(reason::KEPT_LOCAL_NO_MODEL);
        }
        let config = summary_config(&self.config, place.url());
        let clock = Arc::clone(&self.services.clock);
        let service = if place.is_own_network() {
            SummaryService::own_model_endpoint(&config, clock)
        } else {
            SummaryService::new(&config, clock)
        };
        match service {
            // The model is in another container: nothing of this computer is held for it.
            Ok(service) => HelperWriter::Write(Box::new(service), SummaryHold::OffThisComputer),
            Err(_) => HelperWriter::Decline(reason::NO_WRITER),
        }
    }
}

/// The summary settings for the endpoint at `url`.
fn summary_config(config: &Config, url: &Url) -> SummaryConfig {
    SummaryConfig {
        budget: Some(earshot_core::prompt::ContextBudget {
            context_window: config.llm_context,
            max_output_tokens: LOCAL_MAX_OUTPUT_TOKENS.min(config.llm_context / 2),
        }),
        request_timeout: Some(config.llm_timeout),
        ..SummaryConfig::new(url.as_str(), config.llm_model.clone())
    }
}

impl OffloadHost for WorkerHost {
    /// The worker asks no other device, so nothing is ever handed back to it to write.
    fn write_here(&self, _job: WriteHere) -> bool {
        false
    }

    fn write_for(&self, request: SummaryRequest) {
        let host = Self {
            services: self.services.clone(),
            config: Arc::clone(&self.config),
            locate: Arc::clone(&self.locate),
            queue: self.queue.clone(),
            cancel: self.cancel.clone(),
        };
        let id = request.id;
        let spawned = std::thread::Builder::new()
            .name("earshot-worker-offload".into())
            .spawn(move || {
                // One outcome whatever happens: a panic declines `failed`, so the asker writes it.
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    earshot_runtime::offload::write_for_request(
                        &host.services,
                        &host.queue,
                        &request,
                        &|privacy| host.writer(privacy),
                        &host.cancel,
                    )
                }));
                if ran.is_err() {
                    let _ = earshot_runtime::offload::decline(&host.services, id, reason::FAILED);
                }
            });
        if spawned.is_err() {
            let _ = earshot_runtime::offload::decline(&self.services, id, reason::FAILED);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    struct Dir(std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_host(tag: &str, llm: &str, locate: Arc<Locate>) -> (WorkerHost, Dir) {
        let dir =
            Dir(std::env::temp_dir()
                .join(format!("earshot-worker-host-{tag}-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir.0);
        let services = open_services(&dir.0.join("data")).unwrap();
        let config = Config {
            data: dir.0.join("data"),
            relay: Some("https://sync.example.com".into()),
            llm_url: Url::parse(llm).unwrap(),
            llm_service: "llama".into(),
            model_subnet: crate::endpoint::Subnet::parse_private("172.31.250.0/24"),
            llm_model: "test-model".into(),
            llm_context: 16_384,
            llm_timeout: Duration::from_secs(60),
            name: "Invented server".into(),
            key_file: dir.0.join("vault/key"),
            token_file: dir.0.join("vault/token"),
        };
        let host = WorkerHost {
            services,
            config: Arc::new(config),
            locate,
            queue: MeetingQueue::new(),
            cancel: CancelToken::new(),
        };
        (host, dir)
    }

    /// The production rule with `service` as the model service, the compose file's subnet and
    /// a stub lookup that answers every name with an address inside that subnet, so only the
    /// rule's own checks can make a name remote.
    fn stub_resolving(service: &'static str) -> Arc<Locate> {
        let own = crate::endpoint::OwnModel {
            service: service.into(),
            subnet: crate::endpoint::Subnet::parse_private("172.31.250.0/24").unwrap(),
        };
        Arc::new(move |url: &Url| {
            crate::endpoint::locate_with(url, Some(&own), &|_, _| {
                vec!["172.31.250.3".parse().unwrap()]
            })
        })
    }

    /// The production rule with the system's lookup.
    fn production(service: &'static str) -> Arc<Locate> {
        let own = crate::endpoint::OwnModel {
            service: service.into(),
            subnet: crate::endpoint::Subnet::parse_private("172.31.250.0/24").unwrap(),
        };
        Arc::new(move |url: &Url| locate(url, Some(&own)))
    }

    fn declined(writer: &HelperWriter) -> Option<&'static str> {
        match writer {
            HelperWriter::Decline(word) => Some(word),
            HelperWriter::Write(..) => None,
        }
    }

    /// The production rule on a bare name that is not the model service (or does not resolve):
    /// remote, so plain http to it is refused by the client and nothing is ever sent; a
    /// kept-local meeting is declined before a writer is even built.
    #[test]
    fn worker_host_a_name_the_rule_refuses_is_never_written_to() {
        // Control: the stub lookup puts the model service inside its subnet, so the configured
        // name is the worker's own model and writes.
        let (host, _dir) = make_host("control", "http://llama:8080/v1", stub_resolving("llama"));
        assert_eq!(declined(&host.writer(SummaryPrivacy::KeptLocal)), None);
        // The same lookup for a name that is not the configured service: only the exact-name
        // rule makes it remote.
        let (host, _dir) = make_host(
            "refused",
            "http://llama:8080/v1",
            stub_resolving("not-this-one"),
        );
        assert_eq!(
            declined(&host.writer(SummaryPrivacy::Writer)),
            Some(reason::NO_WRITER)
        );
        assert_eq!(
            declined(&host.writer(SummaryPrivacy::KeptLocal)),
            Some(reason::KEPT_LOCAL_NO_MODEL)
        );
        // The configured name itself, where no such service resolves (this test host): the same.
        let (host, _dir) = make_host(
            "unresolved",
            "http://earshot-worker-no-such-model:8080/v1",
            production("earshot-worker-no-such-model"),
        );
        assert_eq!(
            declined(&host.writer(SummaryPrivacy::Writer)),
            Some(reason::NO_WRITER)
        );
    }

    /// The own model at a checked private address builds a writer for any meeting.
    #[test]
    fn worker_host_its_own_model_at_a_private_address_writes_kept_local_meetings() {
        let checked = Url::parse("http://172.18.0.3:8080/v1").unwrap();
        let place = Place::OwnNetwork {
            url: checked.clone(),
        };
        let (host, _dir) = make_host(
            "own",
            "http://llama:8080/v1",
            Arc::new(move |_: &Url| place.clone()),
        );
        for privacy in [SummaryPrivacy::Writer, SummaryPrivacy::KeptLocal] {
            assert_eq!(declined(&host.writer(privacy)), None, "{privacy:?}");
        }
        // And the presence label follows the place.
        assert_eq!(
            writer_kind(&Place::OwnNetwork { url: checked }),
            WriterKind::Local
        );
        assert_eq!(
            writer_kind(&Place::Remote {
                url: Url::parse("https://api.example.com/v1").unwrap()
            }),
            WriterKind::OpenaiCompatible
        );
    }

    #[test]
    fn worker_errors_say_what_to_do_in_fixed_words() {
        assert_eq!(WorkerError::NotJoined.health_word(), "not_joined");
        assert_eq!(
            WorkerError::Sync(SyncProblem::UnknownDevice).health_word(),
            "removed"
        );
        assert!(
            WorkerError::Sync(SyncProblem::UnknownDevice)
                .to_string()
                .contains("join again")
        );
        for e in [
            WorkerError::NotJoined,
            WorkerError::Sync(SyncProblem::KeyMismatch),
            WorkerError::Vault(VaultRefused::NotWritable),
            WorkerError::Vault(VaultRefused::ReadableByOthers),
        ] {
            assert!(
                e.health_word()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{e:?}"
            );
        }
    }
}
