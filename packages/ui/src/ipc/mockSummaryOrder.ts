// The order of summary writers in the mock `Ipc` (T12.63b, docs/summary-order.md): the order kept
// in memory with the Rust rules (`earshot_runtime::summary_order`: at most three steps, nothing
// twice, not this computer, an endpoint step needs its fields), the old writer setting mirrored
// from it, and every label, destination and refusal from `fixtures/ui/summary_order_cases.json`,
// the table the desktop's own tests pin to its Rust copy. The one online service (T14.72,
// docs/summary-order.md 12) follows `summary_order::{online_service, set_online_service}`: the
// pick read, the normalising and the provider switch are pinned by
// `fixtures/ui/online_service_cases.json`, which this mock's tests and the runtime's own run.

import table from "../../../../fixtures/ui/summary_order_cases.json";
import { IpcFailure } from "./ipc";
import { type SummariesMock, checkSummarySettings } from "./mockSummaries";
import type { SummaryHelpersView } from "./sync";
import type {
  OnlineRowView,
  ResummariseItem,
  ServiceChoice,
  StepChoice,
  StepView,
  SummaryOrderCommands,
  SummaryOrderView,
} from "./summaryOrder";
import { fillCopy, isRemoteWriter, mirrorWriter, sameStep } from "./summaryOrder";

export { mirrorWriter };
import type { ProviderChoice, SummarySettingsValue, WriterChoice } from "./bindings";

/** The most steps an order holds (`summary_order::MAX_STEPS`). */
export const MAX_ORDER_STEPS = 3;

/** A UUID as the app reads a device id. */
const UUID = /^(?:[0-9a-f]{8}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{12})$/i;

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

/** The writer step of the old writer setting (`WriterStep::of_writer`): the order the mock starts
 * with when none is given. */
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

/** An online writer's kind (`WriterStep::is_online`). */
type OnlineKind = "Endpoint" | "ClaudeCode" | "Codex";

/** The text `settings/online-service` holds for each online writer (`WriterStep::word`). */
const KEY_WORD: Record<OnlineKind, string> = { Endpoint: "endpoint", ClaudeCode: "claude-code", Codex: "codex" };

/** The service of online writer `kind`, the endpoint's by `provider` (`Service::of_step`). */
export function serviceOf(kind: OnlineKind, provider: ProviderChoice): ServiceChoice {
  if (kind === "Endpoint") return provider === "Anthropic" ? "AnthropicApi" : "OwnServer";
  return kind;
}

/** The writer step of `service` (`Service::step`). */
export function stepOfService(service: ServiceChoice): StepChoice {
  return service === "ClaudeCode" || service === "Codex" ? { kind: service } : { kind: "Endpoint" };
}

/** The endpoint provider `service` stores, null for Claude Code and Codex (`Service::provider`). */
export function providerOfService(service: ServiceChoice): ProviderChoice | null {
  return service === "AnthropicApi" ? "Anthropic" : service === "OwnServer" ? "OpenAiCompatible" : null;
}

/** `steps` with every online writer after the first dropped, and whether any was
 * (`SummaryOrder::normalised`). */
export function normalised(steps: StepChoice[]): { steps: StepChoice[]; dropped: boolean } {
  let seen = false;
  let dropped = false;
  const kept = steps.filter((s) => {
    if (!isRemoteWriter(s)) return true;
    const keep = !seen;
    seen = true;
    if (!keep) dropped = true;
    return keep;
  });
  return { steps: kept, dropped };
}

/** The online service picked, as read (`summary_order::online_service`): the order's online
 * writer (the endpoint's by `provider`), else the stored key, else none; a key this version does
 * not know reads as none with the `unknown_service` note. `steps` is the order as read. */
export function pickOf(
  steps: StepChoice[],
  provider: ProviderChoice,
  key: string | null,
): { pick: ServiceChoice | null; note: "unknown_service" | null } {
  const online = steps.find(isRemoteWriter);
  if (online !== undefined) return { pick: serviceOf(online.kind as OnlineKind, provider), note: null };
  if (key === null) return { pick: null, note: null };
  const kind = (Object.keys(KEY_WORD) as OnlineKind[]).find((k) => KEY_WORD[k] === key.trim());
  return kind === undefined ? { pick: null, note: "unknown_service" } : { pick: serviceOf(kind, provider), note: null };
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
  /** The order now, as read (normalised). */
  steps(): Promise<StepChoice[]>;
  /** The online service picked, as read. */
  pick(): Promise<ServiceChoice | null>;
  /** Stores every Summaries setting with the order as read (`set_summary_settings` through
   * `set_settings_with_order`): never changes the order or the provider. */
  saveSettings(settings: SummarySettingsValue): Promise<void>;
  /** The order and key as stored (the shared case table checks that a read writes nothing). */
  stored(): { order: StepChoice[] | null; key: string | null };
}

/** The order of summary writers of the mock `Ipc`; `initial` overrides the order the writer
 * setting reads as, `initialKey` is the text stored at `settings/online-service` (null: none). */
export function createSummaryOrderMock(
  deps: SummaryOrderDeps,
  initial?: StepChoice[],
  initialKey: string | null = null,
): SummaryOrderMock {
  let order: StepChoice[] | null = initial ? [...initial] : null;
  let key: string | null = initialKey;
  const settingsNow = async () => (await deps.summaries.summarySettings()).value;
  // The stored order, before the read normalises it.
  const raw = async (): Promise<StepChoice[]> => {
    if (order === null) {
      const w = stepOfWriter((await settingsNow()).writer);
      order = w === null ? [] : [w];
    }
    return order;
  };
  const read = async () => normalised(await raw());
  const steps = async (): Promise<StepChoice[]> => (await read()).steps;
  const readPick = async () => pickOf(await steps(), (await settingsNow()).provider, key);
  const context = async (): Promise<Context> => {
    const settings = await settingsNow();
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
  // The Online service row (`summary_order::online_row`).
  const online = async (): Promise<OnlineRowView> => {
    const settings = await settingsNow();
    const { pick } = await readPick();
    if (pick === null)
      return { service: null, step: null, reason: table.page.choose_service, label: null, destination: null };
    const step = stepOfService(pick);
    const cx = await context();
    // An Anthropic pick with no address yet is still Anthropic (12.8): the step rule names any
    // endpoint without a host "Your own server".
    const anthropicUnset = pick === "AnthropicApi" && cx.host === null;
    const label = anthropicUnset ? table.steps.endpoint_anthropic : mockStepLabel(step, cx);
    const destination = anthropicUnset ? table.destinations.endpoint_anthropic : mockDestination(step, cx);
    const ready = step.kind !== "Endpoint" || (settings.url !== "" && settings.model !== "");
    return ready
      ? { service: pick, step, reason: null, label, destination }
      : { service: pick, step: null, reason: table.page.not_set_up, label, destination };
  };
  const view = async (): Promise<SummaryOrderView> => {
    const { steps: now, dropped } = await read();
    const { note } = await readPick();
    const cx = await context();
    const offered: StepChoice[] = [{ kind: "Local" }];
    for (const id of cx.computers.keys()) offered.push({ kind: "Computer", device: id });
    // The online writer is the Online service row, never an offered step.
    for (const s of now) if (!offered.some((o) => sameStep(o, s)) && !isRemoteWriter(s)) offered.push(s);
    const notes: string[] = [];
    if (dropped) notes.push(table.page.second_online_dropped);
    if (note !== null) notes.push(table.page[note]);
    return {
      none: now.length === 0,
      steps: now.map((s) => stepView(s, cx)),
      offered: offered.map((s) => stepView(s, cx)),
      online: await online(),
      notes,
      max_steps: MAX_ORDER_STEPS,
    };
  };
  // `write_order`: the checks in the Rust order, then the order, its mirror, the key and (for a
  // new endpoint service) the provider with its clear, all or nothing.
  const write = async (wanted: StepChoice[], settings: SummarySettingsValue | null, newPick: ServiceChoice | null) => {
    for (const s of wanted) {
      // Rust's copy (`offload::device_id`), not in a table.
      if (s.kind === "Computer" && !UUID.test(s.device)) invalid("That computer is not one Earshot knows.");
    }
    if (wanted.length > MAX_ORDER_STEPS) invalid(table.refusals.too_many_steps);
    if (wanted.some((s, i) => wanted.slice(0, i).some((t) => sameStep(s, t)))) invalid(table.refusals.twice);
    const stored = await settingsNow();
    const { pick: readP } = await readPick();
    // The provider has one writer (12.7a).
    if (settings !== null && settings.provider !== stored.provider) invalid(table.page.settings_changed);
    const onlines = wanted.filter(isRemoteWriter);
    if (onlines.length > 1) invalid(table.page.not_the_service);
    const pick = newPick ?? readP;
    if (onlines.length === 1 && (pick === null || !sameStep(stepOfService(pick), onlines[0]))) {
      invalid(table.page.not_the_service);
    }
    if (wanted.some((s) => s.kind === "Computer" && s.device === deps.thisDevice)) invalid(table.refusals.this_device);
    const provider = newPick === null ? null : providerOfService(newPick);
    const providerChange = provider !== null && provider !== stored.provider ? provider : null;
    const base = settings ?? stored;
    const candidate: SummarySettingsValue =
      providerChange === null ? { ...base } : { ...base, provider: providerChange, url: "", model: "" };
    const writer = mirrorWriter(wanted);
    // The endpoint rules wherever the step sits, then every setting with the mirrored writer:
    // the Summaries mock's own checks, which store nothing on a refusal.
    if (wanted.some((s) => s.kind === "Endpoint")) checkSummarySettings({ ...candidate, writer: "OwnEndpoint" });
    checkSummarySettings({ ...candidate, writer });
    await deps.summaries.setSummarySettings({ ...candidate, writer });
    order = wanted.map((s) => ({ ...s }));
    // The key follows the order's online writer, else the new pick, else the pick as read: an
    // untick never changes the pick (12.4).
    const keyStep = onlines[0] ?? (pick === null ? null : stepOfService(pick));
    if (keyStep !== null) key = KEY_WORD[keyStep.kind as OnlineKind];
  };
  return {
    steps,
    pick: async () => (await readPick()).pick,
    stored: () => ({ order: order === null ? null : order.map((s) => ({ ...s })), key }),
    saveSettings: async (settings) => {
      await write(await steps(), settings, null);
    },
    summaryOrder: async () => view(),
    setSummaryOrder: async (wanted, settings) => {
      await write(wanted, settings, null);
      return view();
    },
    // `set_online_service` (12.6, 12.7).
    setOnlineService: async (service) => {
      const now = await steps();
      const { pick } = await readPick();
      if (pick !== service) {
        const stored = await settingsNow();
        const provider = providerOfService(service);
        const after =
          provider !== null && provider !== stored.provider ? { ...stored, provider, url: "", model: "" } : stored;
        let canStep = true;
        if (service === "AnthropicApi" || service === "OwnServer") {
          try {
            checkSummarySettings({ ...after, writer: "OwnEndpoint" });
          } catch {
            canStep = false;
          }
        }
        const next = [...now];
        const i = next.findIndex(isRemoteWriter);
        if (i >= 0) {
          if (canStep) next[i] = stepOfService(service);
          else next.splice(i, 1);
        }
        await write(next, null, service);
      }
      return { settings: await deps.summaries.summarySettings(), order: await view() };
    },
    // `resummarise_view` (12.9): the local model, and the one service picked when it can run.
    resummariseWriters: async (meeting) => {
      const settings = await settingsNow();
      const cx = await context();
      const kept = deps.keptLocal(meeting);
      const here: StepChoice[] = [{ kind: "Local" }];
      const { pick } = await readPick();
      const runs =
        pick === "ClaudeCode"
          ? (await deps.summaries.claudeCodeStatus()).state !== "NotInstalled"
          : pick === "Codex"
            ? (await deps.summaries.codexStatus()).state !== "NotInstalled"
            : pick !== null &&
              settings.url !== "" &&
              settings.model !== "" &&
              (settings.provider !== "Anthropic" || (await deps.summaries.hasApiKey()));
      if (pick !== null && runs) here.push(stepOfService(pick));
      const writers: ResummariseItem[] = here.map((step) => ({
        step,
        label: mockStepLabel(step, cx),
        disabled: kept && isRemoteWriter(step) ? table.resummarise.kept_local : null,
      }));
      return { writers };
    },
  };
}
