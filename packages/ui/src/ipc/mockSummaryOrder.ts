// The order of summary writers in the mock `Ipc` (T12.63b, docs/summary-order.md): the order kept
// in memory with the Rust rules (`earshot_runtime::summary_order`: at most three steps, nothing
// twice, not this computer, an endpoint step needs its fields), the old writer setting mirrored
// from it, and every label, destination and refusal from `fixtures/ui/summary_order_cases.json`,
// the table the desktop's own tests pin to its Rust copy.

import table from "../../../../fixtures/ui/summary_order_cases.json";
import { IpcFailure } from "./ipc";
import type { SummariesMock } from "./mockSummaries";
import type { SummaryHelpersView } from "./sync";
import type {
  ResummariseItem,
  StepChoice,
  StepView,
  SummaryOrderCommands,
  SummaryOrderView,
} from "./summaryOrder";
import { fillCopy, isRemoteWriter, mirrorWriter, sameStep } from "./summaryOrder";

export { mirrorWriter };
import type { SummarySettingsValue, WriterChoice } from "./bindings";

/** The most steps an order holds (`summary_order::MAX_STEPS`). */
export const MAX_ORDER_STEPS = 3;

/** A UUID as the app reads a device id. */
const UUID = /^(?:[0-9a-f]{8}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{12})$/i;

/** The writer step of the old writer setting (`WriterStep::of_writer`). */
function stepOfWriter(writer: WriterChoice): StepChoice | null {
  switch (writer) {
    case "OnThisComputer":
      return { kind: "Local" };
    case "OwnEndpoint":
      return { kind: "Endpoint" };
    case "ClaudeCode":
      return { kind: "ClaudeCode" };
    case "Codex":
      return { kind: "Codex" };
    case "NoSummary":
      return null;
  }
}

/** `steps` with `writer` as its first writer step (`SummaryOrder::with_writer`). */
export function withWriter(steps: StepChoice[], writer: WriterChoice): StepChoice[] {
  const w = stepOfWriter(writer);
  if (w === null) return steps.filter((s) => s.kind === "Computer");
  const out: StepChoice[] = [];
  let placed = false;
  for (const s of steps) {
    if (s.kind !== "Computer" && !placed) {
      out.push(w);
      placed = true;
    } else if (!sameStep(s, w)) out.push(s);
  }
  if (!placed) {
    if (out.length >= MAX_ORDER_STEPS) out.length = MAX_ORDER_STEPS - 1;
    out.push(w);
  }
  return out;
}

/** Whether summarising during the meeting runs with `steps` (`live_summaries_allowed`). */
export function liveAllowed(steps: StepChoice[]): boolean {
  const [first, second] = steps;
  if (first?.kind === "Local") return true;
  return first?.kind === "Computer" && second?.kind === "Local";
}

/** What a step's copy needs. */
interface Context {
  provider: SummarySettingsValue["provider"];
  host: string | null;
  computers: Map<string, { name: string | null; writer: string | null }>;
}

type Texts = (typeof table)["steps"];

function textOf(texts: Texts, step: StepChoice, cx: Context): string {
  switch (step.kind) {
    case "Local":
      return texts.local;
    case "Endpoint":
      if (cx.host === null) return texts.endpoint_unset;
      return cx.provider === "Anthropic" ? texts.endpoint_anthropic : fillCopy(texts.endpoint_server, { host: cx.host });
    case "ClaudeCode":
      return texts.claude_code;
    case "Codex":
      return texts.codex;
    case "Computer": {
      const c = cx.computers.get(step.device);
      const device = c?.name !== null && c?.name !== undefined && c.name.trim() !== "" ? c.name : table.unnamed;
      return c?.writer
        ? fillCopy(texts.computer, { device, writer: c.writer })
        : fillCopy(texts.computer_no_writer, { device });
    }
  }
}

/** A step's label, as the app gives it. */
export function mockStepLabel(step: StepChoice, cx: Context): string {
  return textOf(table.steps, step, cx);
}

/** Where a step sends the transcript, as the app says it. */
export function mockDestination(step: StepChoice, cx: Context): string {
  return textOf(table.destinations, step, cx);
}

function hostOf(url: string): string | null {
  try {
    return new URL(url).hostname.replace(/^\[|\]$/g, "");
  } catch {
    return null;
  }
}

function invalid(message: string): never {
  throw new IpcFailure({ kind: "invalid_setting", message, retryable: false });
}

/** What the order mock reads from the rest of the mock. */
export interface SummaryOrderDeps {
  summaries: SummariesMock;
  /** The other computers (`summaryHelpers`). */
  helpers(): Promise<SummaryHelpersView>;
  /** This computer's device id, refused as a step. */
  thisDevice: string | null;
  /** Whether a meeting is kept on this computer. */
  keptLocal(meeting: string): boolean;
}

/** The order part of the mock, plus what the rest of the mock reads. */
export interface SummaryOrderMock extends Omit<SummaryOrderCommands, "resummariseWith"> {
  /** The order now. */
  steps(): Promise<StepChoice[]>;
  /** The writer setting was stored through the Summaries page's own save: the order follows it
   * (`set_summary_settings` through `SummaryOrder::with_writer`). */
  writerSaved(writer: WriterChoice): Promise<void>;
}

/** The order of summary writers of the mock `Ipc`; `initial` overrides the order the writer
 * setting reads as. */
export function createSummaryOrderMock(deps: SummaryOrderDeps, initial?: StepChoice[]): SummaryOrderMock {
  let order: StepChoice[] | null = initial ? [...initial] : null;
  const steps = async (): Promise<StepChoice[]> => {
    if (order === null) {
      const w = stepOfWriter((await deps.summaries.summarySettings()).value.writer);
      order = w === null ? [] : [w];
    }
    return order;
  };
  const context = async (): Promise<Context> => {
    const settings = (await deps.summaries.summarySettings()).value;
    const helpers = await deps.helpers();
    return {
      provider: settings.provider,
      host: settings.url !== "" && settings.model !== "" ? hostOf(settings.url) : null,
      computers: new Map(helpers.helpers.map((h) => [h.id, { name: h.name, writer: h.writer }])),
    };
  };
  const stepView = (step: StepChoice, cx: Context): StepView => ({
    step: { ...step },
    label: mockStepLabel(step, cx),
    destination: mockDestination(step, cx),
  });
  const view = async (): Promise<SummaryOrderView> => {
    const now = await steps();
    const cx = await context();
    const offered: StepChoice[] = [{ kind: "Local" }, { kind: "Endpoint" }, { kind: "ClaudeCode" }, { kind: "Codex" }];
    for (const id of cx.computers.keys()) offered.push({ kind: "Computer", device: id });
    for (const s of now) if (!offered.some((o) => sameStep(o, s))) offered.push(s);
    return {
      none: now.length === 0,
      steps: now.map((s) => stepView(s, cx)),
      offered: offered.map((s) => stepView(s, cx)),
      notes: [],
      max_steps: MAX_ORDER_STEPS,
    };
  };
  return {
    steps,
    writerSaved: async (writer) => {
      order = withWriter(await steps(), writer);
    },
    summaryOrder: async () => view(),
    setSummaryOrder: async (wanted, settings) => {
      // `SummaryOrder::new`, then `validate`, in the Rust order.
      for (const s of wanted) {
        // Rust's copy (`offload::device_id`), not in a table.
        if (s.kind === "Computer" && !UUID.test(s.device)) invalid("That computer is not one Earshot knows.");
      }
      if (wanted.length > MAX_ORDER_STEPS) invalid(table.refusals.too_many_steps);
      if (wanted.some((s, i) => wanted.slice(0, i).some((t) => sameStep(s, t)))) invalid(table.refusals.twice);
      if (wanted.some((s) => s.kind === "Computer" && s.device === deps.thisDevice)) invalid(table.refusals.this_device);
      const stored = (await deps.summaries.summarySettings()).value;
      const candidate = settings ?? stored;
      if (wanted.some((s) => s.kind === "Endpoint")) {
        // The endpoint rules wherever the step sits: the Summaries mock's own checks.
        await deps.summaries.setSummarySettings({ ...candidate, writer: "OwnEndpoint" });
      }
      await deps.summaries.setSummarySettings({ ...candidate, writer: mirrorWriter(wanted) });
      order = wanted.map((s) => ({ ...s }));
      return view();
    },
    resummariseWriters: async (meeting) => {
      const settings = (await deps.summaries.summarySettings()).value;
      const cx = await context();
      const kept = deps.keptLocal(meeting);
      const here: StepChoice[] = [{ kind: "Local" }];
      if (
        settings.url !== "" &&
        settings.model !== "" &&
        (settings.provider !== "Anthropic" || (await deps.summaries.hasApiKey()))
      ) {
        here.push({ kind: "Endpoint" });
      }
      if ((await deps.summaries.claudeCodeStatus()).state !== "NotInstalled") here.push({ kind: "ClaudeCode" });
      if ((await deps.summaries.codexStatus()).state !== "NotInstalled") here.push({ kind: "Codex" });
      const writers: ResummariseItem[] = here.map((step) => ({
        step,
        label: mockStepLabel(step, cx),
        disabled: kept && isRemoteWriter(step) ? table.resummarise.kept_local : null,
      }));
      return { writers };
    },
  };
}
