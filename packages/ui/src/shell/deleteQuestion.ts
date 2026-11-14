// What the delete question says (T12.38), by what deleting the meeting means now
// (`deletion::ask` in Rust). A meeting that cannot go now gets the reason and a Close button only.

import type { DeleteAsk } from "../ipc/ipc";

/** The question's text and its confirming button (`null`: nothing to confirm). */
export type DeleteQuestion = { text: string; confirm: string | null };

const PLAIN =
  "This deletes the recording, transcript, summary and speaker names of this meeting from this " +
  "computer. The note in your Obsidian vault stays.";

/** The delete question for `ask`. */
export function deleteQuestion(ask: DeleteAsk): DeleteQuestion {
  switch (ask) {
    case "Free":
      return { text: PLAIN, confirm: "Delete meeting" };
    case "StopsFinishing":
      return {
        text:
          "Earshot is still finishing this meeting's transcript and summary. Deleting the meeting " +
          "stops that work first, then deletes its recording, transcript, summary and speaker " +
          "names from this computer. The note in your Obsidian vault stays.",
        confirm: "Stop and delete",
      };
    case "Recording":
      return {
        text: "This meeting is still recording. Stop the recording first, then delete the meeting.",
        confirm: null,
      };
    case "Retranscribing":
      return {
        text: "This meeting is being transcribed again. Delete it when that has finished.",
        confirm: null,
      };
    case "Resummarising":
      return {
        text: "This meeting is being summarised again. Delete it when that has finished.",
        confirm: null,
      };
    case "NeedsRestart":
      return {
        text:
          "Earshot could not stop earlier work on this meeting, and that work still holds it. " +
          "Restart Earshot, then delete the meeting.",
        confirm: null,
      };
  }
}

/** What the dialog says while a deletion stops the meeting's work first. */
export const STOPPING_TEXT = "Stopping the work on this meeting, then deleting it.";
