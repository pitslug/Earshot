// The summary style commands (T7.4b) and the summary template (T12.17), kept apart from
// ipc.ts so `IpcCommands` gains them in one line.

import type { SummaryStyleView, SummaryTemplateOption } from "./bindings";

export type { SummaryStyleView, SummaryTemplateOption };

/** The summary style part of `IpcCommands`. */
export interface SummaryStyleCommands {
  /** The stored summary style ("" for none), the chosen template and a stored value this
   * version refuses. */
  summaryStyle(): Promise<SummaryStyleView>;
  /** Stores `style` ("" for none), trimmed. Rejects with `invalid_setting` (too long or
   * control characters; nothing stored). */
  setSummaryStyle(style: string): Promise<SummaryStyleView>;
  /** Stores the summary template (one of `SummaryStyleView.templates`). Rejects with
   * `invalid_setting` for a template this version does not offer (nothing stored). */
  setSummaryTemplate(template: string): Promise<SummaryStyleView>;
}
