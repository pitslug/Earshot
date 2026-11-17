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
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_core::model::SummaryRequest;
use earshot_net::Url;
use earshot_runtime::keep_local::SummaryPrivacy;
use earshot_runtime::meeting_queue::{MeetingQueue, SummaryHold};
use earshot_runtime::offload::{
    ContinueAnswer, HelperWriter, OffloadHost, WriteHere, WriterKind, reason,
};
use earshot_runtime::summary::{LOCAL_MAX_OUTPUT_TOKENS, SummaryConfig, SummaryService};
use earshot_runtime::sync::{SyncClient, SyncProblem, SyncThread, SyncTiming, SyncVault};
use earshot_runtime::{Services, SharedClock, SystemClock};
use earshot_store::{DataDir, Store};

use crate::config::{Config, ConfigError};
use crate::endpoint::{Place, locate};
use crate::health;
use crate::vault::{FileVault, Presence, VaultRefused};

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
            Self::Vault(VaultRefused::Incomplete) => "vault_incomplete",
            Self::Vault(VaultRefused::Unreadable) => "vault_unreadable",
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
/// From before the code is spent until the device is named and offers summaries, the join
/// marker ([`FileVault::begin_join`]) is in the vault folder, so a run waiting beside this join
/// (`docker exec`) starts only once the whole join is done (T11.6d). The marker is removed
/// whether the join worked or not; only a crash leaves it.
///
/// # Errors
/// [`WorkerError::Config`] without a relay; [`WorkerError::VoiceprintsOn`];
/// [`WorkerError::Vault`] when the files or the marker cannot be written or the marker cannot
/// be removed; the pairing's [`SyncProblem`]s (a wrong or used code, a device already joined).
pub fn join(services: &Services, config: &Config, code: &str) -> Result<(), WorkerError> {
    let relay = config.relay()?;
    refuse_voiceprints(services)?;
    let vault = vault_of(config);
    vault.probe_writable()?;
    vault.begin_join(now_s())?;
    let joined = earshot_runtime::sync::join_with_code(services, &vault, relay, code)
        .map_err(WorkerError::from)
        .and_then(|()| present_as_worker(services, config));
    let ended = vault.end_join();
    joined?;
    Ok(ended?)
}

/// The system clock in Unix seconds (0 before 1970).
fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn refuse_voiceprints(services: &Services) -> Result<(), WorkerError> {
    let settings = earshot_runtime::sync::sync_settings(&services.store())?;
    if settings.voiceprints {
        return Err(WorkerError::VoiceprintsOn);
    }
    Ok(())
}

/// How often a run waiting to be joined looks for the vault files again (a judgement: a join
/// through `docker exec` is picked up within this).
pub const JOIN_POLL: Duration = Duration::from_secs(5);

/// What a waiting run tells the person, each at most once per run ([`wait_to_join`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// Not joined yet.
    Waiting,
    /// An earlier join stopped part way: run `join` again.
    Interrupted,
}

impl Notice {
    /// The fixed line printed.
    #[must_use]
    pub const fn line(self) -> &'static str {
        match self {
            Self::Waiting => "waiting to join",
            Self::Interrupted => "join interrupted: run `earshot-worker join` again",
        }
    }
}

/// How [`wait_to_join`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waited {
    /// Both vault files are there: run.
    Joined,
    /// A stop was asked for while waiting: end cleanly, sending nothing (the worker never
    /// joined, so it has no presence to take back).
    Stopped,
}

/// Waits until the worker has been joined (T11.6d), so `run` can stay up beside a `join` given
/// through `docker exec` instead of exiting and being restarted. Reads only the configuration,
/// the two vault files and the join marker ([`FileVault::presence`] at `now_s()`): the store is
/// not opened while waiting, so a `join` in the same container is the only process using it.
///
/// [`Presence::Present`] (both files, no marker) returns [`Waited::Joined`] at once. Not joined
/// yet, it keeps waiting, calling `pause` with [`JOIN_POLL`] between looks (`pause` returns true
/// when a stop was asked for), records what it waits on in `heartbeat` at every look (so a
/// waiting run that died reads `stale`), and calls `notice` at most once per [`Notice`] (the
/// caller prints its fixed line):
/// - [`Presence::Absent`], or [`Presence::Joining`] (a join is under way, also when the run
///   starts in the middle of one): [`Notice::Waiting`], [`health::waiting_text`] (`not_joined`).
/// - [`Presence::Interrupted`] (a stale marker): [`Notice::Interrupted`],
///   [`health::interrupted_text`] (`join_interrupted`). It does not exit: the container stays
///   up, so `join` can be run in it again, which overwrites the marker.
///
/// Anything else is refused at once: one file without the other and no marker, or a path that
/// is not a file.
///
/// # Errors
/// [`WorkerError::Config`] without a relay; [`WorkerError::Vault`].
pub fn wait_to_join(
    config: &Config,
    heartbeat: &Path,
    now_s: &dyn Fn() -> u64,
    notice: &mut dyn FnMut(Notice),
    pause: &mut dyn FnMut(Duration) -> bool,
) -> Result<Waited, WorkerError> {
    config.relay()?;
    let vault = vault_of(config);
    let mut said = Vec::new();
    let mut say = |n: Notice| {
        if !said.contains(&n) {
            said.push(n);
            notice(n);
        }
    };
    loop {
        let now = now_s();
        // A record that cannot be written leaves the health check unhealthy anyway (no
        // heartbeat, or a stale one).
        match vault.presence(now)? {
            Presence::Present => return Ok(Waited::Joined),
            Presence::Absent | Presence::Joining => {
                say(Notice::Waiting);
                let _ = health::record_waiting(heartbeat, now);
            }
            Presence::Interrupted => {
                say(Notice::Interrupted);
                let _ = health::record_interrupted(heartbeat, now);
            }
        }
        if pause(JOIN_POLL) {
            return Ok(Waited::Stopped);
        }
    }
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
    /// [`WorkerError::NotJoined`] with neither vault file; [`WorkerError::Vault`] for one file
    /// without the other, a path that is not a file, or a file others can read;
    /// [`WorkerError::Store`].
    pub fn open_with(
        services: Services,
        config: Config,
        locate: Arc<Locate>,
    ) -> Result<Self, WorkerError> {
        config.relay()?;
        refuse_voiceprints(&services)?;
        let vault = Arc::new(vault_of(&config));
        if vault.presence(now_s())? != Presence::Present {
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
    /// model container that starts after the worker is named correctly from its next beat. The
    /// place its model runs (the compose profile, T11.6a) goes with a `local` writer kind only.
    ///
    /// # Errors
    /// [`WorkerError::Sync`] with [`SyncProblem::Address`] for a refused relay address.
    pub fn client(&self) -> Result<SyncClient, WorkerError> {
        let vault: Arc<dyn SyncVault> = Arc::clone(&self.vault) as Arc<dyn SyncVault>;
        let (locate, url) = (Arc::clone(&self.locate), self.config.llm_url.clone());
        Ok(
            SyncClient::for_address(self.services.clone(), vault, self.config.relay()?)?
                .as_summary_worker(Arc::new(move || Some(writer_kind(&locate(&url)))))
                .with_model_place(self.config.model_place),
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
    fn continue_run(&self, _job: WriteHere) -> ContinueAnswer {
        ContinueAnswer::Refused
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
            model_place: None,
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

    fn wait_config(dir: &Path) -> Config {
        Config {
            data: dir.join("data"),
            relay: Some("https://sync.example.com".into()),
            llm_url: Url::parse("http://llama:8080/v1").unwrap(),
            llm_service: "llama".into(),
            model_subnet: None,
            llm_model: "test-model".into(),
            llm_context: 16_384,
            llm_timeout: Duration::from_secs(60),
            name: "Invented server".into(),
            model_place: None,
            key_file: dir.join("vault/key"),
            token_file: dir.join("vault/token"),
        }
    }

    fn wait_dir(tag: &str) -> Dir {
        let dir =
            Dir(std::env::temp_dir()
                .join(format!("earshot-worker-wait-{tag}-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir.0);
        std::fs::create_dir_all(&dir.0).unwrap();
        dir
    }

    fn write_key(config: &Config) {
        std::fs::create_dir_all(config.key_file.parent().unwrap()).unwrap();
        std::fs::write(&config.key_file, "ab".repeat(32)).unwrap();
    }

    fn write_token(config: &Config) {
        std::fs::create_dir_all(config.token_file.parent().unwrap()).unwrap();
        std::fs::write(&config.token_file, "an-invented-token").unwrap();
    }

    const NOW: u64 = 1_800_000_000;

    fn write_marker(config: &Config, at: u64) {
        std::fs::create_dir_all(config.key_file.parent().unwrap()).unwrap();
        std::fs::write(
            vault_of(config).marker_path(),
            crate::vault::marker_text(at),
        )
        .unwrap();
    }

    fn remove_marker(config: &Config) {
        std::fs::remove_file(vault_of(config).marker_path()).unwrap();
    }

    /// Nothing of the store exists while it waits: the data folder is not even created.
    fn assert_no_store(config: &Config) {
        assert!(!config.data.exists(), "the store was opened while waiting");
    }

    /// No vault files: one notice, the health check says `not_joined` while it waits, it
    /// looks again every [`JOIN_POLL`], and it runs as soon as both files are there.
    #[test]
    fn worker_wait_runs_once_joined_and_says_so_once() {
        let dir = wait_dir("joined");
        let config = wait_config(&dir.0);
        let heartbeat = health::heartbeat_path(&dir.0);
        let mut notices = 0;
        let mut pauses = Vec::new();
        let waited = wait_to_join(
            &config,
            &heartbeat,
            &|| NOW,
            &mut |_| notices += 1,
            &mut |d| {
                pauses.push(d);
                assert_eq!(
                    health::check(&dir.0, NOW),
                    Err(health::Unhealthy::NotJoined),
                    "while waiting"
                );
                assert_no_store(&config);
                if pauses.len() == 3 {
                    write_key(&config);
                    write_token(&config);
                }
                false
            },
        );
        assert_eq!(waited, Ok(Waited::Joined));
        assert_eq!(notices, 1, "one line, however long it waits");
        assert_eq!(pauses, vec![JOIN_POLL; 3]);
        assert_no_store(&config);
    }

    /// The waiting record carries the time of each look, so a waiting run that died reads
    /// stale rather than `not_joined` forever.
    #[test]
    fn worker_wait_refreshes_its_record_at_every_look() {
        let dir = wait_dir("refresh");
        let config = wait_config(&dir.0);
        let heartbeat = health::heartbeat_path(&dir.0);
        let clock = std::cell::Cell::new(NOW);
        let mut pauses = 0;
        let waited = wait_to_join(
            &config,
            &heartbeat,
            &|| clock.get(),
            &mut |_| {},
            &mut |d| {
                pauses += 1;
                assert_eq!(
                    std::fs::read_to_string(&heartbeat).unwrap(),
                    health::waiting_text(clock.get())
                );
                clock.set(clock.get() + d.as_secs());
                pauses == 3
            },
        );
        assert_eq!(waited, Ok(Waited::Stopped));
        // Read long after the last look: stale.
        assert_eq!(
            health::check(&dir.0, clock.get() + health::STALE_AFTER_S + 1),
            Err(health::Unhealthy::Stale)
        );
    }

    /// A stop while waiting ends it, with nothing joined, nothing written to the vault and no
    /// store opened.
    #[test]
    fn worker_wait_stops_cleanly_when_asked() {
        let dir = wait_dir("stopped");
        let config = wait_config(&dir.0);
        let heartbeat = health::heartbeat_path(&dir.0);
        let mut notices = 0;
        let mut pauses = 0;
        let waited = wait_to_join(
            &config,
            &heartbeat,
            &|| NOW,
            &mut |_| notices += 1,
            &mut |_| {
                pauses += 1;
                pauses == 2
            },
        );
        assert_eq!(waited, Ok(Waited::Stopped));
        assert_eq!((notices, pauses), (1, 2));
        assert!(!config.key_file.exists() && !config.token_file.exists());
        assert_no_store(&config);
    }

    /// Already joined: no notice, no pause, the heartbeat left to the run loop.
    #[test]
    fn worker_wait_returns_at_once_when_joined() {
        let dir = wait_dir("present");
        let config = wait_config(&dir.0);
        write_key(&config);
        write_token(&config);
        let heartbeat = health::heartbeat_path(&dir.0);
        let waited = wait_to_join(
            &config,
            &heartbeat,
            &|| NOW,
            &mut |_| panic!("no notice"),
            &mut |_| panic!("no pause"),
        );
        assert_eq!(waited, Ok(Waited::Joined));
        assert!(!heartbeat.exists());
    }

    /// A join through `docker exec` while it waits: the marker first, then the key, the
    /// token, the device set up, the marker gone. The run starts only after the marker is
    /// gone, however long the files have been there.
    #[test]
    fn worker_wait_holds_until_the_join_marker_is_gone() {
        let dir = wait_dir("join");
        let config = wait_config(&dir.0);
        let heartbeat = health::heartbeat_path(&dir.0);
        let mut notices = 0;
        let mut pauses = 0;
        let waited = wait_to_join(
            &config,
            &heartbeat,
            &|| NOW,
            &mut |_| notices += 1,
            &mut |_| {
                pauses += 1;
                match pauses {
                    1 => write_marker(&config, NOW),
                    2 => write_key(&config),
                    3 => write_token(&config),
                    4 => {} // the join is still setting up the device
                    5 => remove_marker(&config),
                    _ => panic!("joined by now"),
                }
                assert_no_store(&config);
                false
            },
        );
        assert_eq!(waited, Ok(Waited::Joined));
        assert_eq!((notices, pauses), (1, 5));
    }

    /// A run that starts in the middle of a join (marker and key there, no token yet) waits for
    /// it, rather than refusing the half vault; a stop during it is clean.
    #[test]
    fn worker_wait_a_run_starting_mid_join_waits_for_it() {
        let dir = wait_dir("mid-join");
        let config = wait_config(&dir.0);
        let heartbeat = health::heartbeat_path(&dir.0);
        write_marker(&config, NOW);
        write_key(&config);
        let mut notices = 0;
        let mut pauses = 0;
        let waited = wait_to_join(
            &config,
            &heartbeat,
            &|| NOW,
            &mut |_| notices += 1,
            &mut |_| {
                pauses += 1;
                if pauses == 1 {
                    write_token(&config);
                    remove_marker(&config);
                }
                false
            },
        );
        assert_eq!(waited, Ok(Waited::Joined));
        assert_eq!((notices, pauses), (1, 1));

        write_marker(&config, NOW);
        let waited = wait_to_join(&config, &heartbeat, &|| NOW, &mut |_| {}, &mut |_| true);
        assert_eq!(waited, Ok(Waited::Stopped));
    }

    /// A marker left by a crashed join keeps the run waiting (it never exits, so the container
    /// stays up for `join`) with `join_interrupted`, said once, whatever the files hold; a new
    /// join overwrites the marker and the wait goes on from there to running.
    #[test]
    fn worker_wait_keeps_waiting_behind_an_interrupted_join_until_a_new_join() {
        let dir = wait_dir("interrupted");
        let config = wait_config(&dir.0);
        let heartbeat = health::heartbeat_path(&dir.0);
        let stale = NOW + crate::vault::JOIN_STALE_S + 1;
        write_marker(&config, NOW);
        write_key(&config);
        write_token(&config);
        let mut notices = Vec::new();
        let mut pauses = 0;
        let waited = wait_to_join(
            &config,
            &heartbeat,
            &|| stale,
            &mut |n| notices.push(n),
            &mut |_| {
                pauses += 1;
                let verdict = health::check(&dir.0, stale);
                match pauses {
                    1..=3 => assert_eq!(verdict, Err(health::Unhealthy::JoinInterrupted)),
                    // `join` again: its marker replaces the stale one.
                    4 => {
                        assert_eq!(verdict, Err(health::Unhealthy::JoinInterrupted));
                        write_marker(&config, stale);
                    }
                    5 => {
                        assert_eq!(verdict, Err(health::Unhealthy::NotJoined));
                        remove_marker(&config);
                    }
                    _ => panic!("joined by now"),
                }
                assert_no_store(&config);
                false
            },
        );
        assert_eq!(waited, Ok(Waited::Joined));
        assert_eq!(pauses, 5);
        assert_eq!(notices, vec![Notice::Interrupted, Notice::Waiting]);
    }

    /// A join that hangs past the limit while the run waits turns into `join_interrupted`
    /// without ending the wait; a stop still ends it cleanly.
    #[test]
    fn worker_wait_a_join_that_hangs_too_long_reads_as_interrupted() {
        let dir = wait_dir("hang");
        let config = wait_config(&dir.0);
        let heartbeat = health::heartbeat_path(&dir.0);
        let stale = NOW + crate::vault::JOIN_STALE_S + 1;
        write_marker(&config, NOW);
        write_key(&config);
        let clock = std::cell::Cell::new(NOW);
        let mut notices = Vec::new();
        let mut pauses = 0;
        let waited = wait_to_join(
            &config,
            &heartbeat,
            &|| clock.get(),
            &mut |n| notices.push(n),
            &mut |_| {
                pauses += 1;
                clock.set(stale);
                pauses == 3
            },
        );
        assert_eq!(waited, Ok(Waited::Stopped));
        assert_eq!(notices, vec![Notice::Waiting, Notice::Interrupted]);
        assert_eq!(
            health::check(&dir.0, stale),
            Err(health::Unhealthy::JoinInterrupted)
        );
        // Fixed lines only.
        assert_eq!(Notice::Waiting.line(), "waiting to join");
        assert!(Notice::Interrupted.line().contains("earshot-worker join"));
    }

    /// One file without the other and no join under way is not a worker waiting to join: it
    /// is refused at once, as before T11.6d.
    #[test]
    fn worker_wait_refuses_a_half_present_vault() {
        let dir = wait_dir("half");
        let config = wait_config(&dir.0);
        let heartbeat = health::heartbeat_path(&dir.0);
        for half in [write_key as fn(&Config), write_token] {
            let _ = std::fs::remove_file(&config.key_file);
            let _ = std::fs::remove_file(&config.token_file);
            half(&config);
            let waited = wait_to_join(
                &config,
                &heartbeat,
                &|| NOW,
                &mut |_| panic!("not waiting"),
                &mut |_| panic!("no pause"),
            );
            assert_eq!(waited, Err(WorkerError::Vault(VaultRefused::Incomplete)));
        }
        assert_eq!(
            WorkerError::Vault(VaultRefused::Incomplete).health_word(),
            "vault_incomplete"
        );
    }

    /// A path that is not a file is refused at once; no relay address is a configuration
    /// error before anything is looked at.
    #[test]
    fn worker_wait_refuses_an_unusable_vault_or_configuration_at_once() {
        let dir = wait_dir("unusable");
        let config = wait_config(&dir.0);
        let heartbeat = health::heartbeat_path(&dir.0);
        std::fs::create_dir_all(&config.key_file).unwrap();
        let waited = wait_to_join(
            &config,
            &heartbeat,
            &|| NOW,
            &mut |_| panic!("not waiting"),
            &mut |_| panic!("no pause"),
        );
        assert_eq!(waited, Err(WorkerError::Vault(VaultRefused::Unreadable)));
        assert_eq!(
            WorkerError::Vault(VaultRefused::Unreadable).health_word(),
            "vault_unreadable"
        );
        let no_relay = Config {
            relay: None,
            ..wait_config(&dir.0)
        };
        assert!(matches!(
            wait_to_join(
                &no_relay,
                &heartbeat,
                &|| NOW,
                &mut |_| panic!("not waiting"),
                &mut |_| panic!("no pause"),
            ),
            Err(WorkerError::Config(_))
        ));
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
            WorkerError::Vault(VaultRefused::Incomplete),
            WorkerError::Vault(VaultRefused::Unreadable),
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
