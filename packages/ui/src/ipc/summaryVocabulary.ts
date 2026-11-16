// The Dictionary commands (T12.15a): the user's own words for summaries, kept apart from
// ipc.ts so `IpcCommands` gains them in one line.

import type { SummaryVocabularyAdded, SummaryVocabularyView } from "./bindings";

export type { SummaryVocabularyAdded, SummaryVocabularyView };

/** The Dictionary part of `IpcCommands`. */
export interface SummaryVocabularyCommands {
  /** The stored words (none for an empty list) and a stored list this version refuses. */
  summaryVocabulary(): Promise<SummaryVocabularyView>;
  /** Adds one typed word (trimmed, inner spaces joined) at the end of the list, the stored
   * list read and written in one go (T11.3e: the page never sends a whole list). `added` is
   * false when it was already there. Rejects with `invalid_setting` (too many, too long, not
   * text, or some words not in use; nothing stored). */
  addSummaryVocabularyWord(text: string): Promise<SummaryVocabularyAdded>;
  /** Removes one word (case ignored) and resolves with the words left. */
  removeSummaryVocabularyWord(word: string): Promise<SummaryVocabularyView>;
}
