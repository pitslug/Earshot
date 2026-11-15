//! The container's health check (T11.6): the run loop writes the time to a heartbeat file every
//! [`HEARTBEAT_EVERY`]; `earshot-worker health` reads it and is healthy when it is at most
//! [`STALE_AFTER_S`] away from now. A sync thread that stopped ends the run with an error, so a fresh
//! heartbeat means the loop and its sync thread are both running. A relay that cannot be reached
//! is not unhealthy: the sync thread retries it with back-off, and the Sync page says so.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often the run loop writes the heartbeat.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
/// A heartbeat older (or newer) than this is stale: four missed beats (a judgement).
pub const STALE_AFTER_S: u64 = 120;

/// Why the worker is not healthy, as a fixed word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unhealthy {
    /// No heartbeat: the run loop is not running (or never started).
    NotRunning,
    /// The heartbeat is older than [`STALE_AFTER_S`], or that far in the future.
    Stale,
    /// The heartbeat does not hold a time.
    Unreadable,
}

impl Unhealthy {
    /// The word printed.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::NotRunning => "not_running",
            Self::Stale => "stale",
            Self::Unreadable => "unreadable",
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

/// The verdict on a heartbeat file's text (`None`: no file) at `now_s`.
///
/// # Errors
/// [`Unhealthy`].
pub fn verdict(text: Option<&str>, now_s: u64) -> Result<(), Unhealthy> {
    let text = text.ok_or(Unhealthy::NotRunning)?;
    let at: u64 = text.trim().parse().map_err(|_| Unhealthy::Unreadable)?;
    if at.abs_diff(now_s) > STALE_AFTER_S {
        return Err(Unhealthy::Stale);
    }
    Ok(())
}

/// Writes the heartbeat for `now_s` (a temporary file renamed into place).
///
/// # Errors
/// I/O errors.
pub fn beat(path: &Path, now_s: u64) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, beat_text(now_s))?;
    std::fs::rename(&tmp, path)
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
