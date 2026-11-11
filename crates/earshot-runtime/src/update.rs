//! In-app updates (T9.1b, D24): asks one host for a signed version file, and downloads the
//! installer it names. A request happens only when the user presses the button or has turned
//! automatic checks on, and asks for nothing but that file. Automatic checks run at the start and
//! on the user's [`UpdateSchedule`] (T12.22), measured from the last finished check ([`next_due`]).
//!
//! - **One destination.** [`UpdateSource::releases`] is fixed at build time: this repo's GitHub
//!   releases, with their download host for redirects. Nothing read from the network can name a
//!   host or a path: the installer's URL is built from the verified version and architecture.
//! - **Verified before use.** [`earshot_update::verify`] checks the signature against the key built
//!   into the app before anything in the file is read. An [`UpdateOffer`] is made only from a file
//!   that passed and a version newer than the running one, so holding one is the proof.
//! - **Downloaded by the model rules.** [`crate::download::download`]: streaming, `.part` resume,
//!   size and SHA-256 before the rename.
//! - **Checked again before it runs.** A file on disk can change after its download, so
//!   [`verify_installer`] reads it again at the moment of installing.
//!
//! Failures are [`UpdateError`], whose text never holds a URL's query or credentials. A failed
//! or refused update leaves the installed app and the user's data untouched: nothing here touches
//! either.

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_net::{Allowlist, AuditLog, NetClient, NetConfig, NetRequest, Url};
use earshot_store::{DataDir, Store};
use earshot_update::{Release, VerifyError, Version};
use sha2::{Digest, Sha256};

use crate::download::{DownloadError, Expected};
use crate::local::DeviceRecord;
use crate::{RuntimeError, SharedClock};

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;

/// The local-state key of "check automatically".
pub const AUTO_CHECK_KEY: &str = "settings/update-check";

/// The releases of this repo.
const RELEASES: &str = "https://github.com/pitslug/Earshot/releases";
/// The host of [`RELEASES`].
const RELEASE_HOST: &str = "github.com";
/// Where GitHub sends a release's files (seen 2026-11-08, following the redirects of the first
/// release's installer).
const RELEASE_ASSET_HOST: &str = "release-assets.githubusercontent.com";
/// The deadline of a check. The file is at most [`earshot_update::MAX_FILE_BYTES`].
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// The folder an update's installer waits in.
#[must_use]
pub fn updates_dir(data: &DataDir) -> PathBuf {
    data.root().join("updates")
}

/// How the installer runs: progress only, no questions; `/UPDATE` keeps what an uninstall would
/// remove, such as "Start with Windows"; `/R` starts the new version when it ends.
#[must_use]
pub const fn installer_args() -> [&'static str; 3] {
    ["/P", "/UPDATE", "/R"]
}

/// Whether Earshot checks by itself, when it starts and on its schedule ([`UpdateSchedule`]). Off
/// unless the user turned it on: this is the one consent, so off means no automatic request.
///
/// # Errors
/// Store errors.
pub fn auto_check(store: &Store) -> Result<bool, RuntimeError> {
    Ok(store.local_value(AUTO_CHECK_KEY)?.as_deref() == Some(b"on"))
}

/// Stores "check automatically".
///
/// # Errors
/// Store errors.
pub fn set_auto_check(store: &mut Store, on: bool) -> Result<(), RuntimeError> {
    let value: &[u8] = if on { b"on" } else { b"off" };
    crate::local::set(store, &DeviceRecord::UpdateCheck.key(), value).map_err(RuntimeError::from)
}

/// The local-state key of how often automatic checks run (T12.22).
pub const SCHEDULE_KEY: &str = "settings/update-schedule";
/// The local-state key of when the last check finished, in Unix milliseconds as decimal text.
pub const LAST_CHECK_KEY: &str = "update-last-check";
/// The local-state key of the last version a notification was shown for.
pub const NOTIFIED_KEY: &str = "update-notified";

/// How often Earshot checks by itself while it runs, when automatic checks are on. Off means no
/// request at all, whatever this says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpdateSchedule {
    /// Every hour.
    Hourly,
    /// Every 4 hours.
    FourHours,
    /// Every 12 hours.
    TwelveHours,
    /// Every day: the default.
    #[default]
    Daily,
    /// Every week.
    Weekly,
}

impl UpdateSchedule {
    /// Every choice, in the order the page lists them.
    pub const ALL: [UpdateSchedule; 5] = [
        UpdateSchedule::Hourly,
        UpdateSchedule::FourHours,
        UpdateSchedule::TwelveHours,
        UpdateSchedule::Daily,
        UpdateSchedule::Weekly,
    ];

    /// The time between checks.
    #[must_use]
    pub const fn interval(self) -> Duration {
        const HOUR: u64 = 60 * 60;
        Duration::from_secs(match self {
            UpdateSchedule::Hourly => HOUR,
            UpdateSchedule::FourHours => 4 * HOUR,
            UpdateSchedule::TwelveHours => 12 * HOUR,
            UpdateSchedule::Daily => 24 * HOUR,
            UpdateSchedule::Weekly => 7 * 24 * HOUR,
        })
    }

    /// What is stored.
    #[must_use]
    pub const fn stored(self) -> &'static str {
        match self {
            UpdateSchedule::Hourly => "hourly",
            UpdateSchedule::FourHours => "4-hours",
            UpdateSchedule::TwelveHours => "12-hours",
            UpdateSchedule::Daily => "daily",
            UpdateSchedule::Weekly => "weekly",
        }
    }

    /// The choice stored as `text`, if this version knows it.
    #[must_use]
    pub fn from_stored(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.stored() == text)
    }
}

/// The stored schedule, and the stored text when this version does not know it (read as the
/// default, [`UpdateSchedule::Daily`]).
///
/// # Errors
/// Store errors.
pub fn update_schedule(store: &Store) -> Result<(UpdateSchedule, Option<String>), RuntimeError> {
    let Some(bytes) = store.local_value(SCHEDULE_KEY)? else {
        return Ok((UpdateSchedule::default(), None));
    };
    let text = String::from_utf8_lossy(&bytes);
    Ok(match UpdateSchedule::from_stored(&text) {
        Some(schedule) => (schedule, None),
        None => (UpdateSchedule::default(), Some(text.into_owned())),
    })
}

/// Stores how often automatic checks run.
///
/// # Errors
/// Store errors.
pub fn set_update_schedule(
    store: &mut Store,
    schedule: UpdateSchedule,
) -> Result<(), RuntimeError> {
    crate::local::set(
        store,
        &DeviceRecord::UpdateSchedule.key(),
        schedule.stored().as_bytes(),
    )
    .map_err(RuntimeError::from)
}

/// When the last check finished, in Unix milliseconds; `None` if never, or if what is stored
/// cannot be read (the next due check then runs and stores a good value).
///
/// # Errors
/// Store errors.
pub fn last_check(store: &Store) -> Result<Option<u64>, RuntimeError> {
    Ok(store
        .local_value(LAST_CHECK_KEY)?
        .and_then(|b| std::str::from_utf8(&b).ok()?.parse().ok()))
}

/// Stores that a check finished at `unix_ms`, whatever it found: a failed check counts too, so a
/// computer with no connection asks once per interval, not at every wake of the schedule.
///
/// # Errors
/// Store errors.
pub fn record_check(store: &mut Store, unix_ms: u64) -> Result<(), RuntimeError> {
    crate::local::set(
        store,
        &DeviceRecord::UpdateLastCheck.key(),
        unix_ms.to_string().as_bytes(),
    )
    .map_err(RuntimeError::from)
}

/// The last version a notification was shown for.
///
/// # Errors
/// Store errors.
pub fn notified_version(store: &Store) -> Result<Option<String>, RuntimeError> {
    Ok(store
        .local_value(NOTIFIED_KEY)?
        .map(|b| String::from_utf8_lossy(&b).into_owned()))
}

/// Stores that a notification was shown for `version`, so it is not shown again.
///
/// # Errors
/// Store errors.
pub fn set_notified_version(store: &mut Store, version: &str) -> Result<(), RuntimeError> {
    crate::local::set(
        store,
        &DeviceRecord::UpdateNotified.key(),
        version.as_bytes(),
    )
    .map_err(RuntimeError::from)
}

/// When the next automatic check is due, in Unix milliseconds, `interval` after the last finished
/// check (`last`) as read at `now`. Clock-free, so every case is tested without waiting:
///
/// - never checked: due now;
/// - the wall clock moved back (`last` after `now`): never more than one interval from `now`. A
///   caller that asks again later stores `now` as the last check first ([`record_check`]), or the
///   check would move one interval further away at each ask;
/// - the wall clock moved forward, or the computer woke from sleep: an overdue check is due now,
///   once. The check stores a new `last`, so the next one is a whole interval later: no burst for
///   the intervals that were slept through.
#[must_use]
pub fn next_due(last: Option<u64>, interval: Duration, now: u64) -> u64 {
    let Some(last) = last else {
        return now;
    };
    let step = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX);
    last.saturating_add(step).min(now.saturating_add(step))
}

/// Why an update did not happen.
#[derive(Debug)]
pub enum UpdateError {
    /// This build has no release key, so it can verify nothing.
    NoKey,
    /// The network client could not be built.
    Setup(String),
    /// The request failed or was refused before a response.
    Network(String),
    /// The host answers that there is no such file: the latest release has none for this
    /// architecture.
    NotPublished,
    /// Any other status.
    Status(u16),
    /// The file was refused.
    Refused(VerifyError),
    /// The installer's download failed.
    Download(DownloadError),
    /// The installer on disk is not what the release says.
    InstallerChanged,
    /// The installer on disk cannot be read.
    Unreadable(String),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoKey => f.write_str("this build has no release key"),
            Self::Setup(e) => write!(f, "the update client could not start: {e}"),
            Self::Network(e) => write!(f, "the update check failed: {e}"),
            Self::NotPublished => f.write_str("the latest release has no update file"),
            Self::Status(s) => write!(f, "the update host answered {s}"),
            Self::Refused(e) => write!(f, "{e}"),
            Self::Download(e) => write!(f, "{e}"),
            Self::InstallerChanged => {
                f.write_str("the downloaded installer is not what the release says")
            }
            Self::Unreadable(e) => write!(f, "the downloaded installer cannot be read: {e}"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// Where updates come from, and the key they must be signed with.
#[derive(Debug, Clone)]
pub struct UpdateSource {
    /// The version file.
    version_url: Url,
    /// What the installer's URL starts with: `<downloads>/v<version>/<installer>`.
    downloads: String,
    /// All the client may contact.
    allowlist: Allowlist,
    /// The release key's public half.
    key: [u8; 32],
    /// This build's architecture.
    arch: String,
}

impl UpdateSource {
    /// This repo's releases and the key built into the app.
    ///
    /// # Errors
    /// [`UpdateError::NoKey`] in a build without one.
    pub fn releases() -> Result<Self, UpdateError> {
        Self::releases_with_key(earshot_update::RELEASE_PUBLIC_KEY.ok_or(UpdateError::NoKey)?)
    }

    /// This repo's releases and `key`.
    fn releases_with_key(key: [u8; 32]) -> Result<Self, UpdateError> {
        let arch = earshot_update::this_arch();
        let bad = |e: earshot_net::UrlError| UpdateError::Setup(e.to_string());
        let mut allowlist = Allowlist::new();
        allowlist.allow_https_host(RELEASE_HOST).map_err(bad)?;
        allowlist
            .allow_redirect_host(RELEASE_ASSET_HOST)
            .map_err(bad)?;
        let name = earshot_update::version_file_name(arch);
        let version_url = Url::parse(&format!("{RELEASES}/latest/download/{name}"))
            .map_err(|e| UpdateError::Setup(e.to_string()))?;
        Ok(Self {
            version_url,
            downloads: format!("{RELEASES}/download"),
            allowlist,
            key,
            arch: arch.to_owned(),
        })
    }

    /// The installer's URL: everything comes from the verified release's checked fields.
    fn installer_url(&self, release: &Release) -> Result<Url, UpdateError> {
        Url::parse(&format!(
            "{}/v{}/{}",
            self.downloads, release.version, release.installer
        ))
        .map_err(|e| UpdateError::Setup(e.to_string()))
    }
}

/// A verified release newer than the running version. Made only by [`Updater::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOffer {
    release: Release,
}

impl UpdateOffer {
    /// The new version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.release.version
    }

    /// The installer's size in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.release.size
    }

    /// Where the installer goes in `dir`. The name is checked: it holds no path.
    #[must_use]
    pub fn installer_path(&self, dir: &Path) -> PathBuf {
        dir.join(&self.release.installer)
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    /// The latest release is the running version or an older one.
    UpToDate,
    /// A newer release.
    Available(UpdateOffer),
}

/// Checks and downloads, through one client and its audit log.
pub struct Updater {
    source: UpdateSource,
    client: NetClient,
    runtime: tokio::runtime::Runtime,
}

impl Updater {
    /// An updater for `source`.
    ///
    /// # Errors
    /// [`UpdateError::Setup`] if the client or its runtime cannot be built.
    pub fn new(source: UpdateSource, clock: SharedClock) -> Result<Self, UpdateError> {
        let config = NetConfig::new(source.allowlist.clone(), clock)
            .timeout(CHECK_TIMEOUT)
            .max_body(earshot_update::MAX_FILE_BYTES);
        let client = NetClient::new(config).map_err(|e| UpdateError::Setup(e.to_string()))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| UpdateError::Setup(e.to_string()))?;
        Ok(Self {
            source,
            client,
            runtime,
        })
    }

    /// Every request this updater made.
    #[must_use]
    pub fn audit_log(&self) -> AuditLog {
        self.client.audit_log()
    }

    /// Asks for the version file and verifies it. `current` is the running version.
    ///
    /// # Errors
    /// [`UpdateError`]: nothing is offered then.
    pub fn check(&self, current: &str) -> Result<CheckResult, UpdateError> {
        let request = NetRequest::get(self.source.version_url.clone());
        let response = self
            .runtime
            .block_on(self.client.send(request))
            .map_err(|e| UpdateError::Network(e.to_string()))?;
        match response.status {
            200 => {}
            404 => return Err(UpdateError::NotPublished),
            other => return Err(UpdateError::Status(other)),
        }
        let release = earshot_update::verify(&response.body, &self.source.key, &self.source.arch)
            .map_err(UpdateError::Refused)?;
        let newer = match (
            release.version.parse::<Version>(),
            current.parse::<Version>(),
        ) {
            (Ok(new), Ok(running)) => new > running,
            // A running version with no reading, such as a developer's: never offer over it.
            _ => false,
        };
        Ok(if newer {
            CheckResult::Available(UpdateOffer { release })
        } else {
            CheckResult::UpToDate
        })
    }

    /// Downloads `offer`'s installer into `dir`, or finds it already there, verified either way.
    /// Returns its path. `progress(done, total)` follows the bytes.
    ///
    /// # Errors
    /// [`UpdateError::Download`]; a cut transfer keeps its `.part` for the next try.
    pub fn download(
        &self,
        offer: &UpdateOffer,
        dir: &Path,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<PathBuf, UpdateError> {
        std::fs::create_dir_all(dir).map_err(|e| UpdateError::Unreadable(e.to_string()))?;
        let dest = offer.installer_path(dir);
        let url = self.source.installer_url(&offer.release)?;
        let expected = Expected {
            sha256: &offer.release.sha256,
            size: offer.release.size,
        };
        self.runtime
            .block_on(crate::download::download(
                &self.client,
                &url,
                &dest,
                &expected,
                cancel,
                progress,
            ))
            .map_err(UpdateError::Download)?;
        Ok(dest)
    }
}

/// Reads the installer at `path` again and compares it with `offer`, right before it runs.
///
/// # Errors
/// [`UpdateError::InstallerChanged`], or [`UpdateError::Unreadable`].
pub fn verify_installer(offer: &UpdateOffer, path: &Path) -> Result<(), UpdateError> {
    let mut file = std::fs::File::open(path).map_err(|e| UpdateError::Unreadable(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| UpdateError::Unreadable(e.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    let digest = earshot_update::to_hex(&hasher.finalize());
    if size == offer.release.size && digest == offer.release.sha256 {
        Ok(())
    } else {
        Err(UpdateError::InstallerChanged)
    }
}

/// Removes what earlier updates left in `dir`: installers and partial downloads, nothing else.
/// Best effort, at each start, before a new check can begin.
pub fn remove_leftovers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        let ours = name.starts_with("earshot_")
            && (name.ends_with("-setup.exe") || name.ends_with("-setup.exe.part"));
        if ours && entry.file_type().is_ok_and(|t| t.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    // Only succeeds when empty.
    let _ = std::fs::remove_dir(dir);
}
