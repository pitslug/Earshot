// What the delete question says (T12.38), by what deleting the meeting means now
// (`deletion::ask` in Rust). A meeting that cannot go now gets the reason and a Close button only.

import type { DeleteAsk } from "../ipc/ipc";
import { DELETE_COPY } from "./deleteCopy";

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
      return { text: DELETE_COPY.recording, confirm: null };
    case "Retranscribing":
      return { text: DELETE_COPY.retranscribing, confirm: null };
    case "Resummarising":
      return { text: DELETE_COPY.resummarising, confirm: null };
    case "NeedsRestart":
      return { text: DELETE_COPY.needs_restart, confirm: null };
  }
}

/** What the dialog says while a deletion stops the meeting's work first. */
export const STOPPING_TEXT = "Stopping the work on this meeting, then deleting it.";
