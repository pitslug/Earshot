// What the delete question says (T12.38), by what deleting the meeting means now
// (`deletion::ask` in Rust). A meeting that cannot go now gets the reason and a Close button only;
// one with work running on it (its finishing, or a re-transcription or re-summary since T12.41)
// is asked whether to stop that work and delete it.

import type { DeleteAsk } from "../ipc/ipc";
import { DELETE_COPY } from "./deleteCopy";

/** The question's text and its confirming button (`null`: nothing to confirm). */
export type DeleteQuestion = { text: string; confirm: string | null };

const PLAIN =
  "This deletes the recording, transcript, summary and speaker names of this meeting from this " +
  "computer. The note in your Obsidian vault stays.";

/** What deleting a meeting with work running on it does, after saying what runs. */
const STOPS_FIRST =
  "Deleting the meeting stops that work first, then deletes its recording, transcript, summary " +
  "and speaker names from this computer. The note in your Obsidian vault stays.";

/** Whether deleting the meeting stops work on it first (the deletion's `stopFinishing`). */
export function stopsWork(ask: DeleteAsk): boolean {
  return ask === "StopsFinishing" || ask === "Retranscribing" || ask === "Resummarising";
}

/** The delete question for `ask`. */
export function deleteQuestion(ask: DeleteAsk): DeleteQuestion {
  switch (ask) {
    case "Free":
      return { text: PLAIN, confirm: "Delete meeting" };
    case "StopsFinishing":
      return {
        text: `Earshot is still finishing this meeting's transcript and summary. ${STOPS_FIRST}`,
        confirm: "Stop and delete",
      };
    case "Retranscribing":
      return {
        text: `Earshot is transcribing this meeting again. ${STOPS_FIRST}`,
        confirm: "Stop and delete",
      };
    case "Resummarising":
      return {
        text: `Earshot is writing a new summary of this meeting. ${STOPS_FIRST}`,
        confirm: "Stop and delete",
      };
    case "Recording":
      return { text: DELETE_COPY.recording, confirm: null };
    case "NeedsRestart":
      return { text: DELETE_COPY.needs_restart, confirm: null };
  }
}

/** What the dialog says while a deletion stops the meeting's work first. */
export const STOPPING_TEXT = "Stopping the work on this meeting, then deleting it.";
