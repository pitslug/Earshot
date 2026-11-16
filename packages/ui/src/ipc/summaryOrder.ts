// The order of summary writers (T12.63b, docs/summary-order.md): the Summaries page's one "Who
// writes the summary" section and the meeting header's re-summarise menu, kept apart from ipc.ts so
// `IpcCommands` gains them in one line. All copy comes from `fixtures/ui/summary_order_cases.json`
// (`ORDER_COPY`), the table the desktop's own tests pin to its Rust copy.

import table from "../../../../fixtures/ui/summary_order_cases.json";
import type {
  ResummariseItem,
  ResummariseView,
  StepChoice,
  StepView,
  SummaryOrderView,
  SummarySettingsValue,
  WriterChoice,
} from "./bindings";

export type { ResummariseItem, ResummariseView, StepChoice, StepView, SummaryOrderView };

/** The order's copy, shipped with the app (public copy only). */
export const ORDER_COPY = table;

/** `template` with each `{name}` filled literally (a name holding `$&` stays as written). */
export function fillCopy(template: string, pairs: Record<string, string>): string {
  return Object.entries(pairs).reduce((text, [k, v]) => text.split(`{${k}}`).join(v), template);
}

/** Whether two steps are the same writer or computer. */
export function sameStep(a: StepChoice, b: StepChoice): boolean {
  if (a.kind !== b.kind) return false;
  return a.kind !== "Computer" || (b.kind === "Computer" && a.device === b.device);
}

/** A step as a select's value (and back, through the offered list). */
export function stepKey(step: StepChoice): string {
  return step.kind === "Computer" ? `Computer:${step.device}` : step.kind;
}

/** Whether a step sends the transcript off this computer by itself (an endpoint or an agent CLI):
 * what a meeting kept on this computer never uses. */
export function isRemoteWriter(step: StepChoice): boolean {
  return step.kind === "Endpoint" || step.kind === "ClaudeCode" || step.kind === "Codex";
}

/** The old writer setting an order mirrors: its first writer step, or no summary. */
export function mirrorWriter(steps: StepChoice[]): WriterChoice {
  const first = steps.find((s) => s.kind !== "Computer");
  switch (first?.kind) {
    case "Local":
      return "OnThisComputer";
    case "Endpoint":
      return "OwnEndpoint";
    case "ClaudeCode":
      return "ClaudeCode";
    case "Codex":
      return "Codex";
    default:
      return "NoSummary";
  }
}

/** The order part of `IpcCommands`. Refusals are `invalid_setting` with the app's copy. */
export interface SummaryOrderCommands {
  /** The order of summary writers as the Summaries page shows it (asks the server for the other
   * computers when sync is on). */
  summaryOrder(): Promise<SummaryOrderView>;
  /** Stores the order of `steps` (empty: no summary), with the endpoint settings when the page
   * changes both (the endpoint form under a step). */
  setSummaryOrder(steps: StepChoice[], settings: SummarySettingsValue | null): Promise<SummaryOrderView>;
  /** The writers the re-summarise menu offers for a meeting: those that can run here now, the
   * remote ones disabled for a meeting kept on this computer. Computers come from
   * `summaryHelpers`. */
  resummariseWriters(meeting: string): Promise<ResummariseView>;
  /** Starts a re-summary by the one writer `step` names (no next step; `JobEvent`s follow). A
   * computer is asked through `askSummaryHelper`. */
  resummariseWith(meeting: string, step: StepChoice): Promise<void>;
}
