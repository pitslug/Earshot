// The summary style commands (T7.4b), kept apart from ipc.ts so `IpcCommands` gains them in
// one line.

import type { SummaryStyleView } from "./bindings";

export type { SummaryStyleView };

/** The summary style part of `IpcCommands`. */
export interface SummaryStyleCommands {
  /** The stored summary style ("" for none) and a stored one this version refuses. */
  summaryStyle(): Promise<SummaryStyleView>;
  /** Stores `style` ("" for none), trimmed. Rejects with `invalid_setting` (too long or
   * control characters; nothing stored). */
  setSummaryStyle(style: string): Promise<SummaryStyleView>;
}
