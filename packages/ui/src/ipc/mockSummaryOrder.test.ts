// The mock's order of summary writers against the shared copy table and the Rust rules it mirrors
// (`live_summaries_allowed`, the refusals), and the one online service (T14.72) against the
// shared case table the runtime runs too (`fixtures/ui/online_service_cases.json`).

import { describe, expect, it } from "vitest";

import onlineCases from "../../../../fixtures/ui/online_service_cases.json";
import table from "../../../../fixtures/ui/summary_order_cases.json";
import type { ProviderChoice, ServiceChoice } from "./bindings";
import { IpcFailure } from "./ipc";
import { createMockIpc } from "./mock";
import { createSummariesMock } from "./mockSummaries";
import { createSummaryOrderMock, liveAllowed, mirrorWriter, mockDestination, mockStepLabel } from "./mockSummaryOrder";
import type { StepChoice } from "./summaryOrder";
import { fillCopy } from "./summaryOrder";

const LOCAL: StepChoice = { kind: "Local" };
const CLAUDE: StepChoice = { kind: "ClaudeCode" };
const DESK: StepChoice = { kind: "Computer", device: "0f3a91c2-0000-7000-8000-000000000000" };

/** The table's step kinds as steps (it uses no computers). */
const stepsOf = (kinds: string[]): StepChoice[] => kinds.map((kind) => ({ kind }) as StepChoice);

/** A usable address for `provider`, stored when an order holds the endpoint (as the runtime's
 * runner of the same table does). */
const usableUrl = (provider: ProviderChoice) =>
  provider === "Anthropic" ? "https://api.anthropic.com" : "http://127.0.0.1:8080/v1";

/** An order mock over a store holding exactly these values. */
function orderMockOf(
  settings: { provider: ProviderChoice; url: string; model: string },
  steps: StepChoice[],
  key: string | null,
) {
  const summaries = createSummariesMock({ settings: { ...settings, writer: mirrorWriter(steps) } });
  const mock = createSummaryOrderMock(
    {
      summaries,
      helpers: async () => ({ sync_ready: false, chosen: null, helpers: [], problem: null }),
      thisDevice: null,
      keptLocal: () => false,
    },
    steps,
    key,
  );
  return { summaries, mock };
}

async function refusal(run: () => Promise<unknown>): Promise<string | null> {
  try {
    await run();
    return null;
  } catch (e) {
    if (e instanceof IpcFailure) return e.error.message;
    throw e;
  }
}

describe("mock order of summary writers", () => {
  it("labels each step and its destination from the shared table", () => {
    const cx = {
      provider: "OpenAiCompatible" as const,
      host: "models.example.test",
      computers: new Map([[DESK.kind === "Computer" ? DESK.device : "", { name: "Invented desk", writer: "local model" }]]),
    };
    expect(mockStepLabel(LOCAL, cx)).toBe(table.steps.local);
    expect(mockDestination(LOCAL, cx)).toBe(table.destinations.local);
    expect(mockStepLabel({ kind: "Endpoint" }, cx)).toBe("Model server (models.example.test)");
    expect(mockDestination({ kind: "Endpoint" }, { ...cx, provider: "Anthropic" })).toBe(
      table.destinations.endpoint_anthropic,
    );
    expect(mockStepLabel({ kind: "Endpoint" }, { ...cx, host: null })).toBe(table.steps.endpoint_unset);
    expect(mockStepLabel(DESK, cx)).toBe("On Invented desk (local model)");
    expect(mockDestination(DESK, cx)).toBe("Goes to Invented desk, which writes it with its local model");
    expect(mockStepLabel(DESK, { ...cx, computers: new Map() })).toBe(
      fillCopy(table.steps.computer_no_writer, { device: table.unnamed }),
    );
  });

  it("follows the Rust rules for the old writer setting and live summaries", () => {
    expect(mirrorWriter([DESK, LOCAL])).toBe("OnThisComputer");
    expect(mirrorWriter([DESK])).toBe("NoSummary");
    expect(mirrorWriter([DESK, CLAUDE, LOCAL])).toBe("ClaudeCode");
    expect(liveAllowed([LOCAL, CLAUDE])).toBe(true);
    expect(liveAllowed([DESK, LOCAL])).toBe(true);
    expect(liveAllowed([CLAUDE, LOCAL])).toBe(false);
    expect(liveAllowed([DESK, CLAUDE, LOCAL])).toBe(false);
    expect(liveAllowed([])).toBe(false);
  });

  it("keeps public copy only: no em dash", () => {
    expect(JSON.stringify(table)).not.toContain("—");
    expect(JSON.stringify(onlineCases)).not.toContain("—");
  });
});

describe("the mock's one online service against the shared case table (T14.72)", () => {
  it("reads the pick as the app does", async () => {
    expect(onlineCases.pick.length).toBeGreaterThanOrEqual(8);
    const wrong: string[] = [];
    for (const c of onlineCases.pick) {
      const provider = c.provider as ProviderChoice;
      const steps = stepsOf(c.order);
      const url = steps.some((s) => s.kind === "Endpoint") ? usableUrl(provider) : "";
      const { mock } = orderMockOf({ provider, url, model: url === "" ? "" : "m" }, steps, c.key);
      const view = await mock.summaryOrder();
      const notes = c.note === null ? [] : [table.page[c.note as "unknown_service"]];
      if (view.online.service !== c.pick || JSON.stringify(view.notes) !== JSON.stringify(notes)) {
        wrong.push(`${c.name}: ${String(view.online.service)} ${JSON.stringify(view.notes)}`);
      }
      // A note never shows the stored text.
      if (c.key !== null && view.notes.some((n) => n.includes(c.key as string))) wrong.push(`${c.name}: shows the key`);
    }
    expect(wrong).toEqual([]);
  });

  it("keeps the first online writer of a stored order, and reading writes nothing", async () => {
    expect(onlineCases.normalise.length).toBeGreaterThanOrEqual(4);
    const wrong: string[] = [];
    for (const c of onlineCases.normalise) {
      const set = c.endpoint_set_up;
      const stored = stepsOf(c.stored);
      const { mock } = orderMockOf(
        { provider: "OpenAiCompatible", url: set ? usableUrl("OpenAiCompatible") : "", model: set ? "m" : "" },
        stored,
        null,
      );
      const view = await mock.summaryOrder();
      const notes = c.note ? [table.page.second_online_dropped] : [];
      if (
        JSON.stringify(view.steps.map((v) => v.step)) !== JSON.stringify(stepsOf(c.reads)) ||
        JSON.stringify(view.notes) !== JSON.stringify(notes)
      ) {
        wrong.push(`${c.name}: ${JSON.stringify(view.steps.map((v) => v.step))} ${JSON.stringify(view.notes)}`);
      }
      if (JSON.stringify(mock.stored().order) !== JSON.stringify(stored)) wrong.push(`${c.name}: reading wrote`);
    }
    expect(wrong).toEqual([]);
  });

  it("changes the pick as the app does, clearing the endpoint when its provider changes", async () => {
    expect(onlineCases.switch.length).toBeGreaterThanOrEqual(6);
    const wrong: string[] = [];
    for (const c of onlineCases.switch) {
      const { summaries, mock } = orderMockOf(
        { provider: c.provider as ProviderChoice, url: c.url, model: c.model },
        stepsOf(c.order),
        null,
      );
      await mock.setOnlineService(c.service as ServiceChoice);
      const s = (await summaries.summarySettings()).value;
      const got = {
        order: (await mock.steps()).map((x) => x.kind),
        provider: s.provider,
        url: s.url,
        model: s.model,
        pick: await mock.pick(),
      };
      if (JSON.stringify(got) !== JSON.stringify(c.after)) wrong.push(`${c.name}: ${JSON.stringify(got)}`);
    }
    expect(wrong).toEqual([]);
  });

  it("keeps the pick across an untick, and moves it only when another service is picked", async () => {
    // No key stored (every upgrade): the pick is the order's writer, and an untick keeps it.
    const { mock } = orderMockOf({ provider: "OpenAiCompatible", url: "", model: "" }, [LOCAL, CLAUDE], null);
    await mock.setSummaryOrder([LOCAL], null);
    expect(await mock.pick()).toBe("ClaudeCode");
    expect(mock.stored().key).toBe("claude-code");
    // Control: picking another service changes it.
    await mock.setOnlineService("Codex");
    expect(await mock.pick()).toBe("Codex");
    expect(mock.stored()).toEqual({ order: [LOCAL], key: "codex" });
  });

  it("refuses two online writers, a writer that is not the pick, and stale settings, storing nothing", async () => {
    const { summaries, mock } = orderMockOf({ provider: "Anthropic", url: "", model: "" }, [LOCAL, CLAUDE], null);
    const before = JSON.stringify([mock.stored(), (await summaries.summarySettings()).value]);
    expect(await refusal(() => mock.setSummaryOrder([CLAUDE, { kind: "Codex" }], null))).toBe(
      table.page.not_the_service,
    );
    expect(await refusal(() => mock.setSummaryOrder([LOCAL, { kind: "Codex" }], null))).toBe(
      table.page.not_the_service,
    );
    const settings = (await summaries.summarySettings()).value;
    expect(await refusal(() => mock.setSummaryOrder([LOCAL, CLAUDE], { ...settings, provider: "OpenAiCompatible" }))).toBe(
      table.page.settings_changed,
    );
    expect(JSON.stringify([mock.stored(), (await summaries.summarySettings()).value])).toBe(before);
    // Controls: the pick at any position, and the same settings, are stored.
    expect(await refusal(() => mock.setSummaryOrder([CLAUDE, LOCAL], { ...settings, live: true }))).toBeNull();
    expect((await mock.steps()).map((s) => s.kind)).toEqual(["ClaudeCode", "Local"]);
  });

  it("labels an Anthropic pick with no address as Anthropic, as the app does", async () => {
    const { mock } = orderMockOf({ provider: "OpenAiCompatible", url: "", model: "" }, [LOCAL], null);
    await mock.setOnlineService("AnthropicApi");
    const row = (await mock.summaryOrder()).online;
    expect(row).toEqual({
      service: "AnthropicApi",
      step: null,
      reason: table.page.not_set_up,
      label: table.steps.endpoint_anthropic,
      destination: table.destinations.endpoint_anthropic,
    });
    // Control: your own server with no address is "Your own server".
    await mock.setOnlineService("OwnServer");
    const own = (await mock.summaryOrder()).online;
    expect([own.label, own.destination]).toEqual([table.steps.endpoint_unset, table.destinations.endpoint_unset]);
    // And with no pick, the row cannot be ticked and says to choose.
    const none = orderMockOf({ provider: "OpenAiCompatible", url: "", model: "" }, [LOCAL], null).mock;
    expect((await none.summaryOrder()).online).toEqual({
      service: null,
      step: null,
      reason: table.page.choose_service,
      label: null,
      destination: null,
    });
  });

  it("offers only the pick in Re-summarise and refuses another online writer by name", async () => {
    const ipc = createMockIpc({
      machine: "desktop",
      summaryOrder: [LOCAL],
      onlineService: "claude-code",
    });
    const meeting = (await ipc.listMeetings()).meetings[0]?.id ?? "";
    const offered = (await ipc.resummariseWriters(meeting)).writers.map((w) => w.step.kind);
    // Codex is installed on the sample desktop, but it is not the pick.
    expect(offered).toEqual(["Local", "ClaudeCode"]);
    await expect(ipc.resummariseWith(meeting, { kind: "Codex" })).rejects.toMatchObject({
      error: { kind: "invalid_setting", message: table.page.not_the_service },
    });
    // Control: with Codex picked it is offered and accepted.
    await ipc.setOnlineService("Codex");
    expect((await ipc.resummariseWriters(meeting)).writers.map((w) => w.step.kind)).toEqual(["Local", "Codex"]);
    await expect(ipc.resummariseWith(meeting, { kind: "Codex" })).resolves.toBeUndefined();
  });

  it("refuses an unpicked online writer before the kept-local refusal, as the app does", async () => {
    const probe = createMockIpc({ machine: "desktop" });
    const meeting = (await probe.listMeetings()).meetings[0]?.id ?? "";
    const ipc = createMockIpc({ machine: "desktop", onlineService: "claude-code", keptLocal: [meeting] });
    // Codex is not the pick: `not_the_service`, though the meeting is kept here too.
    await expect(ipc.resummariseWith(meeting, { kind: "Codex" })).rejects.toMatchObject({
      error: { kind: "invalid_setting", message: table.page.not_the_service },
    });
    // Control: the picked writer on the kept meeting still gets the kept-local refusal.
    await expect(ipc.resummariseWith(meeting, { kind: "ClaudeCode" })).rejects.toMatchObject({
      error: { kind: "summary_kept_local", message: table.resummarise.kept_local },
    });
  });

  it("has every T14.72 copy key the page, the mock and the runs read", () => {
    const keys: [string, string][] = [
      [table.page.online_service, "Online service"],
      [table.page.choose_service, "Choose a service"],
      [table.page.service_label, "Service"],
      [
        table.page.second_online_dropped,
        "Your order of summary writers held more than one online service, so only the first is used.",
      ],
      [table.page.unknown_service, "The stored online service is not one this version knows, so none is chosen."],
      [table.page.not_the_service, "Only the online service you chose can write summaries."],
      [table.page.settings_changed, "The summary settings changed. Check them and try again."],
      [table.words.not_chosen, "not chosen"],
    ];
    for (const [got, want] of keys) expect(got).toBe(want);
  });
});
