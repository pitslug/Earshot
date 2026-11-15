//! The container's health check (T11.6): the run loop writes the time to a heartbeat file every
//! [`HEARTBEAT_EVERY`]; `earshot-worker health` reads it and is healthy when it is at most
//! [`STALE_AFTER_S`] away from now. A run that cannot start or ends on a problem (not joined,
//! removed from the vault, another vault's key) writes `stopped <word>` there instead, so the
//! health check reports that word while Docker's restart policy starts the worker again. A run
//! that is not joined yet waits and writes `waiting <time>` ([`waiting_text`]) there at every
//! look, so the health check reports `not_joined`, or `stale` once a waiting run has died
//! (T11.6d). A relay that cannot be reached is not unhealthy: the sync thread retries it with back-off, and
//! the Sync page says so.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often the run loop writes the heartbeat.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
/// A heartbeat older (or newer) than this is stale: four missed beats (a judgement).
pub const STALE_AFTER_S: u64 = 120;
/// The longest stopped word kept.
const MAX_WORD: usize = 32;

/// Why the worker is not healthy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unhealthy {
    /// No heartbeat: the run loop is not running (or never started).
    NotRunning,
    /// The heartbeat is older than [`STALE_AFTER_S`], or that far in the future.
    Stale,
    /// The heartbeat does not hold a time or a stopped word.
    Unreadable,
    /// The run is waiting to be joined (its record is at most [`STALE_AFTER_S`] old).
    NotJoined,
    /// The last run stopped on this problem (a fixed word: lower-case letters and `_`).
    Stopped(String),
}

impl Unhealthy {
    /// What is printed.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::NotRunning => "not_running".into(),
            Self::Stale => "stale".into(),
            Self::Unreadable => "unreadable".into(),
            Self::NotJoined => "not_joined".into(),
            Self::Stopped(word) => format!("stopped: {word}"),
        }
    }
}

/// The heartbeat file under the data folder.
#[must_use]
pub fn heartbeat_path(data: &Path) -> PathBuf {
    data.join("worker-heartbeat")
}

/// The heartbeat's text for `now_s` (Unix seconds).
#[must_use]
pub fn beat_text(now_s: u64) -> String {
    format!("{now_s}\n")
}

/// The text recording that a run is waiting to be joined at `now_s`.
#[must_use]
pub fn waiting_text(now_s: u64) -> String {
    format!("waiting {now_s}\n")
}

/// The text recording that a run stopped on `word` (anything but lower-case letters and `_`
/// is dropped, so nothing but a fixed word is ever written).
#[must_use]
pub fn stopped_text(word: &str) -> String {
    let word: String = word
        .chars()
        .filter(|c| c.is_ascii_lowercase() || *c == '_')
        .take(MAX_WORD)
        .collect();
    format!("stopped {word}\n")
}

/// The verdict on a heartbeat file's text (`None`: no file) at `now_s`.
///
/// # Errors
/// [`Unhealthy`].
pub fn verdict(text: Option<&str>, now_s: u64) -> Result<(), Unhealthy> {
    let text = text.ok_or(Unhealthy::NotRunning)?.trim();
    if let Some(at) = text.strip_prefix("waiting ") {
        let at: u64 = at.parse().map_err(|_| Unhealthy::Unreadable)?;
        return Err(if at.abs_diff(now_s) > STALE_AFTER_S {
            Unhealthy::Stale
        } else {
            Unhealthy::NotJoined
        });
    }
    if let Some(word) = text.strip_prefix("stopped ") {
        let ok = !word.is_empty()
            && word.len() <= MAX_WORD
            && word.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
        return Err(if ok {
            Unhealthy::Stopped(word.to_owned())
        } else {
            Unhealthy::Unreadable
        });
    }
    let at: u64 = text.parse().map_err(|_| Unhealthy::Unreadable)?;
    if at.abs_diff(now_s) > STALE_AFTER_S {
        return Err(Unhealthy::Stale);
    }
    Ok(())
}

fn write(path: &Path, text: &str) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Writes the heartbeat for `now_s` (a temporary file renamed into place).
///
/// # Errors
/// I/O errors.
pub fn beat(path: &Path, now_s: u64) -> std::io::Result<()> {
    write(path, &beat_text(now_s))
}

/// Records that the run is waiting to be joined at `now_s` ([`waiting_text`]).
///
/// # Errors
/// I/O errors.
pub fn record_waiting(path: &Path, now_s: u64) -> std::io::Result<()> {
    write(path, &waiting_text(now_s))
}

/// Records that the run stopped on `word`.
///
/// # Errors
/// I/O errors.
pub fn record_stopped(path: &Path, word: &str) -> std::io::Result<()> {
    write(path, &stopped_text(word))
}

/// The health check: the heartbeat under `data` at `now_s`.
///
/// # Errors
/// [`Unhealthy`].
pub fn check(data: &Path, now_s: u64) -> Result<(), Unhealthy> {
    let text = match std::fs::read_to_string(heartbeat_path(data)) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(Unhealthy::Unreadable),
    };
    verdict(text.as_deref(), now_s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_a_fresh_heartbeat_is_healthy_and_anything_else_is_not() {
        let now = 1_800_000_000;
        assert_eq!(verdict(Some(&beat_text(now)), now), Ok(()));
        assert_eq!(verdict(Some(&beat_text(now - STALE_AFTER_S)), now), Ok(()));
        assert_eq!(verdict(Some(&beat_text(now + STALE_AFTER_S)), now), Ok(()));
        assert_eq!(
            verdict(Some(&beat_text(now - STALE_AFTER_S - 1)), now),
            Err(Unhealthy::Stale)
        );
        assert_eq!(
            verdict(Some(&beat_text(now + STALE_AFTER_S + 1)), now),
            Err(Unhealthy::Stale)
        );
        assert_eq!(verdict(None, now), Err(Unhealthy::NotRunning));
        assert_eq!(verdict(Some("yesterday"), now), Err(Unhealthy::Unreadable));
        assert_eq!(verdict(Some(""), now), Err(Unhealthy::Unreadable));
        // Four missed beats.
        assert_eq!(STALE_AFTER_S, 4 * HEARTBEAT_EVERY.as_secs());
    }

    #[test]
    fn health_a_stopped_run_reports_its_word_and_nothing_else() {
        let now = 1_800_000_000;
        assert_eq!(
            verdict(Some(&stopped_text("not_joined")), now),
            Err(Unhealthy::Stopped("not_joined".into()))
        );
        assert_eq!(
            Unhealthy::Stopped("removed".into()).describe(),
            "stopped: removed"
        );
        // Only a fixed word is ever written or read back.
        assert_eq!(stopped_text("Key: ab12/x"), "stopped eyabx\n");
        assert_eq!(verdict(Some("stopped "), now), Err(Unhealthy::Unreadable));
        assert_eq!(
            verdict(Some("stopped a-token-value"), now),
            Err(Unhealthy::Unreadable)
        );
    }

    #[test]
    fn health_the_check_reads_the_heartbeat_the_loop_writes() {
        let dir =
            std::env::temp_dir().join(format!("earshot-worker-health-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(check(&dir, 100), Err(Unhealthy::NotRunning));
        beat(&heartbeat_path(&dir), 1_000).unwrap();
        assert_eq!(check(&dir, 1_030), Ok(()));
        assert_eq!(check(&dir, 1_200), Err(Unhealthy::Stale));
        beat(&heartbeat_path(&dir), 1_200).unwrap();
        assert_eq!(check(&dir, 1_200), Ok(()));
        record_stopped(&heartbeat_path(&dir), "not_joined").unwrap();
        assert_eq!(
            check(&dir, 1_200),
            Err(Unhealthy::Stopped("not_joined".into()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn health_a_run_waiting_to_be_joined_is_unhealthy_with_a_fixed_word() {
        let now = 1_800_000_000;
        assert_eq!(waiting_text(now), "waiting 1800000000\n");
        assert_eq!(
            verdict(Some(&waiting_text(now)), now),
            Err(Unhealthy::NotJoined)
        );
        assert_eq!(
            verdict(Some(&waiting_text(now - STALE_AFTER_S)), now),
            Err(Unhealthy::NotJoined)
        );
        assert_eq!(Unhealthy::NotJoined.describe(), "not_joined");
        // A waiting run that died reads stale, like a running one, either way round.
        assert_eq!(
            verdict(Some(&waiting_text(now - STALE_AFTER_S - 1)), now),
            Err(Unhealthy::Stale)
        );
        assert_eq!(
            verdict(Some(&waiting_text(now + STALE_AFTER_S + 1)), now),
            Err(Unhealthy::Stale)
        );
        // Nothing else that starts the same way reads as waiting.
        assert_eq!(
            verdict(Some("waiting something"), now),
            Err(Unhealthy::Unreadable)
        );
        assert_eq!(verdict(Some("waiting "), now), Err(Unhealthy::Unreadable));
        let dir =
            std::env::temp_dir().join(format!("earshot-worker-waiting-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        record_waiting(&heartbeat_path(&dir), now).unwrap();
        assert_eq!(check(&dir, now + 5), Err(Unhealthy::NotJoined));
        // The first heartbeat after joining replaces it.
        beat(&heartbeat_path(&dir), now).unwrap();
        assert_eq!(check(&dir, now), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
