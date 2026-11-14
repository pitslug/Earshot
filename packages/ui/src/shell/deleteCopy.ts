// The words of a meeting deletion that cannot go now (T12.38), as `deletion.rs` says them. The one
// TypeScript copy: the delete question and the mock both read it, and `deleteQuestion.test.ts`
// pins it to `fixtures/ui/delete_copy_cases.json`, which the Rust tests pin too.

export const DELETE_COPY = {
  recording: "This meeting is still recording. Stop the recording first, then delete the meeting.",
  finishing:
    "This meeting is still being finished. Delete it again to stop that work and delete the meeting.",
  still_stopping:
    "Earshot is still stopping the work on this meeting, so it was not deleted yet. Try again in a minute.",
  needs_restart:
    "Earshot could not stop earlier work on this meeting, and that work still holds it. " +
    "Restart Earshot, then delete the meeting.",
  retranscribing: "This meeting is being transcribed again. Delete it when that has finished.",
  resummarising: "This meeting is being summarised again. Delete it when that has finished.",
} as const;
