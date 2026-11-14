//! Deleting a meeting (T12.38): what the delete question says, and the deletion itself, which may
//! first stop the meeting's finishing.
//!
//! A meeting whose final pass or summary runs or waits is deleted only after the user agreed to
//! stop that work: [`delete`] cancels it through [`MeetingDriver::cancel_finishing`] (T9.9's one
//! token), then tries `Services::delete_meeting` until the meeting's leases are gone, at most
//! [`STOP_WAIT`]. A thread left behind (T9.22) keeps its `Processing` lease until it ends or the
//! app restarts, so such a meeting is refused with [`NEEDS_RESTART`]: nothing records a deletion
//! for the next start. A meeting still recording is refused ([`RECORDING`]); so is one with a
//! re-transcription or re-summary running, since jobs have no cancel ([`job_copy`]).

use std::time::{Duration, Instant};

use earshot_core::identity::EntityId;
use earshot_runtime::{MeetingActivity, RuntimeError, Services};
use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::detail::parse_id;
use crate::ipc::{AppState, IpcError, blocking};
use crate::jobs::{JobKind, JobRunner};
use crate::meeting::MeetingDriver;

/// What deleting a meeting means now, for the delete question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum DeleteAsk {
    /// Nothing works on it: the plain question.
    Free,
    /// Its final pass or summary runs or waits: deleting stops that first.
    StopsFinishing,
    /// It is still recording: not deletable until the recording stops.
    Recording,
    /// A re-transcription runs on it: not deletable until it ends.
    Retranscribing,
    /// A re-summary runs on it: not deletable until it ends.
    Resummarising,
    /// Work Earshot gave up on (T9.22) still holds it: not deletable until Earshot restarts.
    NeedsRestart,
}

/// The longest [`delete`] waits for a cancelled finishing to let go of the meeting. A judgement:
/// work that answers its cancel ends in well under a second, but a model load cannot be
/// interrupted; past it the deletion is refused as retryable ([`STILL_STOPPING`]), the work
/// already cancelled.
pub const STOP_WAIT: Duration = Duration::from_secs(15);

/// How often [`delete`] tries again while the cancelled work lets go.
const RETRY: Duration = Duration::from_millis(50);

/// The refusal of a meeting still recording.
pub const RECORDING: &str = "This meeting is still recording. Stop the recording first, then \
     delete the meeting.";

/// The refusal of a meeting whose finishing runs, when the user did not agree to stop it.
pub const FINISHING: &str = "This meeting is still being finished. Delete it again to stop that \
     work and delete the meeting.";

/// The refusal of a meeting whose cancelled finishing has not let go within [`STOP_WAIT`].
pub const STILL_STOPPING: &str = "Earshot is still stopping the work on this meeting, so it was \
     not deleted yet. Try again in a minute.";

/// The refusal of a meeting a thread left behind still holds (T9.22).
pub const NEEDS_RESTART: &str = "Earshot could not stop earlier work on this meeting, and that \
     work still holds it. Restart Earshot, then delete the meeting.";

/// The refusal of a meeting with a job running.
#[must_use]
pub fn job_copy(job: JobKind) -> &'static str {
    match job {
        JobKind::Retranscribe => {
            "This meeting is being transcribed again. Delete it when that has finished."
        }
        JobKind::Resummarise => {
            "This meeting is being summarised again. Delete it when that has finished."
        }
    }
}

/// What deleting `meeting` means now. A deleted or unknown meeting is [`DeleteAsk::Free`]: the
/// deletion itself says so.
///
/// # Errors
/// Store errors.
pub fn ask(
    svc: &Services,
    driver: &MeetingDriver,
    jobs: &JobRunner,
    meeting: EntityId,
) -> Result<DeleteAsk, IpcError> {
    let activity = svc.meeting_activity(meeting)?;
    if activity == Some(MeetingActivity::Recording) {
        return Ok(DeleteAsk::Recording);
    }
    if let Some(job) = jobs.running_job(meeting) {
        return Ok(match job {
            JobKind::Retranscribe => DeleteAsk::Retranscribing,
            JobKind::Resummarise => DeleteAsk::Resummarising,
        });
    }
    if driver.is_finishing(&meeting.to_string()) {
        return Ok(DeleteAsk::StopsFinishing);
    }
    Ok(match activity {
        // No meeting thread and no job, yet something works on it: a thread left behind.
        Some(MeetingActivity::Processing | MeetingActivity::Waiting) => DeleteAsk::NeedsRestart,
        _ => DeleteAsk::Free,
    })
}

fn refused(kind: &str, message: &str, retryable: bool) -> IpcError {
    IpcError {
        kind: kind.into(),
        message: message.into(),
        retryable,
    }
}

/// Deletes `meeting`; when its finishing runs and `stop_finishing` is set, cancels that first and
/// waits up to `wait` for it to let go (production: [`STOP_WAIT`]).
///
/// # Errors
/// `meeting_recording`, `meeting_job` and `meeting_needs_restart` as [`ask`] says;
/// `meeting_finishing` (retryable) while its finishing runs and `stop_finishing` is not set;
/// `meeting_stopping` (retryable) when the cancelled work has not let go within `wait`; anything
/// `Services::delete_meeting` refuses.
pub fn delete(
    svc: &Services,
    driver: &MeetingDriver,
    jobs: &JobRunner,
    meeting: EntityId,
    stop_finishing: bool,
    wait: Duration,
) -> Result<(), IpcError> {
    match ask(svc, driver, jobs, meeting)? {
        DeleteAsk::Free => {
            svc.delete_meeting(meeting)?;
            Ok(())
        }
        DeleteAsk::Recording => Err(refused("meeting_recording", RECORDING, false)),
        DeleteAsk::Retranscribing => Err(refused(
            "meeting_job",
            job_copy(JobKind::Retranscribe),
            true,
        )),
        DeleteAsk::Resummarising => {
            Err(refused("meeting_job", job_copy(JobKind::Resummarise), true))
        }
        DeleteAsk::NeedsRestart => Err(refused("meeting_needs_restart", NEEDS_RESTART, false)),
        DeleteAsk::StopsFinishing if !stop_finishing => {
            Err(refused("meeting_finishing", FINISHING, true))
        }
        DeleteAsk::StopsFinishing => {
            let id = meeting.to_string();
            driver.cancel_finishing(&id);
            let started = Instant::now();
            loop {
                match svc.delete_meeting(meeting) {
                    Ok(_) => return Ok(()),
                    Err(RuntimeError::MeetingBusy(_)) => {}
                    Err(e) => return Err(e.into()),
                }
                // The meeting's thread has ended (it lets go of the meeting before it ends), so
                // work that still holds it was left behind: no need to wait any longer.
                if !driver.is_finishing(&id)
                    && matches!(
                        svc.meeting_activity(meeting)?,
                        Some(MeetingActivity::Processing | MeetingActivity::Waiting)
                    )
                {
                    return Err(refused("meeting_needs_restart", NEEDS_RESTART, false));
                }
                if started.elapsed() >= wait {
                    return Err(refused("meeting_stopping", STILL_STOPPING, true));
                }
                std::thread::sleep(RETRY);
            }
        }
    }
}

/// What deleting `meeting` means now, for the delete question.
#[tauri::command]
#[specta::specta]
pub async fn meeting_deletion(
    state: State<'_, AppState>,
    driver: State<'_, MeetingDriver>,
    jobs: State<'_, JobRunner>,
    meeting: String,
) -> Result<DeleteAsk, IpcError> {
    let services = state.services()?;
    let meeting = parse_id(&meeting)?;
    let (driver, jobs) = (driver.inner().clone(), jobs.inner().clone());
    blocking(move || ask(&services, &driver, &jobs, meeting)).await
}

/// Deletes `meeting` with its audio, transcript and summaries. With `stop_finishing`, a final
/// pass or summary that runs on it is stopped first (`meeting_finishing` without it). Refused
/// while it records (`meeting_recording`), while a job runs on it (`meeting_job`), and while work
/// given up on still holds it (`meeting_needs_restart`).
#[tauri::command]
#[specta::specta]
pub async fn delete_meeting(
    state: State<'_, AppState>,
    driver: State<'_, MeetingDriver>,
    jobs: State<'_, JobRunner>,
    meeting: String,
    stop_finishing: bool,
) -> Result<(), IpcError> {
    let services = state.services()?;
    let meeting = parse_id(&meeting)?;
    let (driver, jobs) = (driver.inner().clone(), jobs.inner().clone());
    blocking(move || {
        delete(
            &services,
            &driver,
            &jobs,
            meeting,
            stop_finishing,
            STOP_WAIT,
        )
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_copy_matches_the_shared_table() {
        // The UI's question and the mock check the same table (`deleteQuestion.test.ts`).
        let table: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../fixtures/ui/delete_copy_cases.json"
        ))
        .unwrap();
        let copy = |key: &str| table[key].as_str().unwrap().to_owned();
        assert_eq!(copy("recording"), RECORDING);
        assert_eq!(copy("finishing"), FINISHING);
        assert_eq!(copy("still_stopping"), STILL_STOPPING);
        assert_eq!(copy("needs_restart"), NEEDS_RESTART);
        assert_eq!(copy("retranscribing"), job_copy(JobKind::Retranscribe));
        assert_eq!(copy("resummarising"), job_copy(JobKind::Resummarise));
    }
}
