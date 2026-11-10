// The Dictionary commands (T12.15a): the user's own words for summaries, kept apart from
// ipc.ts so `IpcCommands` gains them in one line.

import type { SummaryVocabularyView } from "./bindings";

export type { SummaryVocabularyView };

/** The Dictionary part of `IpcCommands`. */
export interface SummaryVocabularyCommands {
  /** The stored words (none for an empty list) and a stored list this version refuses. */
  summaryVocabulary(): Promise<SummaryVocabularyView>;
  /** Stores `words` as the whole list (empty for none): each trimmed, empty ones and repeats
   * in another case dropped. Rejects with `invalid_setting` (too many, too long or not text;
   * nothing stored). */
  setSummaryVocabulary(words: string[]): Promise<SummaryVocabularyView>;
}
