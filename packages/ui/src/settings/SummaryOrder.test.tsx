// The Summaries page's order of summary writers (T12.63b, T12.68; docs/summary-order.md 2): one
// list of every writer this computer can use, the ticked writers first in their order with a rank
// and up and down buttons, at most three ticked, a writer that is not set up greyed with its
// reason, "No summary" above the list, the kept-local line under it, and where each writer sends
// the transcript. Since T14.72 (docs/summary-order.md 12) the online writers are one Online
// service row with a picker: tests that ticked Claude Code, Codex and the endpoint as three rows
// were rewritten to the one row (the third writer is now another computer).

import { act, renderHook, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import {
  type Ipc,
  IpcProvider,
  type MockIpcData,
  ORDER_COPY,
  type ServiceChoice,
  type StepChoice,
  createMockIpc,
  fillCopy,
  mockDeviceId,
} from "../ipc";
import { MOCK_ANTHROPIC_URL } from "../ipc/mockSummaries";
import { CLAUDE_CODE_PRIVACY, CODEX_PRIVACY, ONLINE_ROW_KEY, SETUP_COPY } from "./SummariesPage";
import { useSummaries } from "./useSummaries";
import { openSettings, openSetup, renderApp, setupToggle } from "../test/app";
import { createManualTimer } from "../time";

const STUDIO = mockDeviceId("e4a17d30");
const LAPTOP = mockDeviceId("c5e09b72");
const DESK: StepChoice = { kind: "Computer", device: STUDIO };
const LAP: StepChoice = { kind: "Computer", device: LAPTOP };
const PAGE = ORDER_COPY.page;
const LOCAL_LABEL = ORDER_COPY.steps.local;
const CLAUDE_LABEL = ORDER_COPY.steps.claude_code;
const CODEX_LABEL = ORDER_COPY.steps.codex;
const STUDIO_LABEL = fillCopy(ORDER_COPY.steps.computer, { device: "Studio", writer: "local model" });
const LAPTOP_LABEL = fillCopy(ORDER_COPY.steps.computer, { device: "Laptop", writer: "Claude Code" });
const SERVER_LABEL = fillCopy(ORDER_COPY.steps.endpoint_server, { host: "192.168.1.20" });
const TOO_MANY = fillCopy(PAGE.too_many, { n: "3" });

async function open(
  data: MockIpcData = {},
  wrap: (ipc: Ipc) => Ipc = (ipc) => ipc,
): Promise<{ ipc: Ipc; user: ReturnType<typeof userEvent.setup> }> {
  const timer = createManualTimer(1_000_000);
  const ipc = createMockIpc({ meetings: [], timer, ...data });
  renderApp(wrap(ipc), { timer });
  const user = userEvent.setup();
  await openSettings(user, "Summaries");
  await screen.findByRole("radiogroup", { name: "Who writes the summary" });
  return { ipc, user };
}

const stored = async (ipc: Ipc) => (await ipc.summaryOrder()).steps.map((s) => s.step);
const list = () => screen.getByRole("list", { name: PAGE.list });
const rows = () => within(list()).getAllByRole("listitem");
const tick = (writer: string) => screen.getByRole("checkbox", { name: fillCopy(PAGE.use, { writer }) });
const up = (writer: string) => screen.queryByRole("button", { name: fillCopy(PAGE.move_up, { writer }) });
const down = (writer: string) => screen.queryByRole("button", { name: fillCopy(PAGE.move_down, { writer }) });
const writeRadio = () => screen.getByRole("radio", { name: new RegExp(`^${PAGE.write}`) });
const noneRadio = () => screen.getByRole("radio", { name: /^No summary/ });
/** The Online service row's picker. */
const picker = () => screen.getByRole("combobox", { name: PAGE.service_label });
/** The Online service rows (exactly one, always). */
const onlineRows = () => rows().filter((r) => r.dataset.step === ONLINE_ROW_KEY);
/** Picks `service` with the Online service row's picker. */
const pick = (user: ReturnType<typeof userEvent.setup>, service: ServiceChoice) =>
  user.selectOptions(picker(), service);
/** Each row as its rank ("-" when not ticked) and the writer its tick box names. */
const listed = () =>
  rows().map((row) => {
    const name = within(row).getByRole("checkbox").getAttribute("aria-label") ?? "";
    return `${row.getAttribute("data-rank") ?? "-"} ${name.replace(fillCopy(PAGE.use, { writer: "" }), "")}`;
  });

describe("Settings, Summaries, the order of summary writers", () => {
  it("lists every writer, the ticked ones first with their rank, each saying where the transcript goes", async () => {
    await open({ sync: { setUp: true }, summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }] });
    await waitFor(() => expect(tick(LOCAL_LABEL)).toBeEnabled());
    expect(listed()).toEqual([`1 ${LOCAL_LABEL}`, `2 ${CLAUDE_LABEL}`, `- ${LAPTOP_LABEL}`, `- ${STUDIO_LABEL}`]);
    // The rank shows as a number on a ticked row only.
    expect(within(rows()[0]).getByText("1")).toBeInTheDocument();
    expect(within(rows()[1]).getByText("2")).toBeInTheDocument();
    expect(within(rows()[3]).queryByText("3")).toBeNull();
    expect(tick(LOCAL_LABEL)).toBeChecked();
    expect(tick(STUDIO_LABEL)).not.toBeChecked();
    for (const [i, text] of [
      [0, ORDER_COPY.destinations.local],
      [1, ORDER_COPY.destinations.claude_code],
      [3, fillCopy(ORDER_COPY.destinations.computer, { device: "Studio", writer: "local model" })],
    ] as const) {
      expect(within(rows()[i]).getByText(text)).toBeInTheDocument();
    }
    // Up and down only on ticked rows; the first cannot go up, the last ticked cannot go down.
    expect(up(LOCAL_LABEL)).toBeDisabled();
    expect(down(LOCAL_LABEL)).toBeEnabled();
    expect(up(CLAUDE_LABEL)).toBeEnabled();
    expect(down(CLAUDE_LABEL)).toBeDisabled();
    expect(up(STUDIO_LABEL)).toBeNull();
    expect(down(STUDIO_LABEL)).toBeNull();
    // One Online service row: the picked service, no row for any other online writer.
    expect(onlineRows()).toHaveLength(1);
    expect(onlineRows()[0]).toBe(rows()[1]);
    expect(picker()).toHaveValue("ClaudeCode");
    for (const other of [CODEX_LABEL, ORDER_COPY.steps.endpoint_unset, ORDER_COPY.steps.endpoint_anthropic]) {
      expect(screen.queryByRole("checkbox", { name: fillCopy(PAGE.use, { writer: other }) })).toBeNull();
    }
    // The picker names the four services by their step names.
    expect([...picker().querySelectorAll("option")].map((o) => o.textContent)).toEqual([
      CLAUDE_LABEL,
      CODEX_LABEL,
      ORDER_COPY.steps.endpoint_anthropic,
      ORDER_COPY.steps.endpoint_unset,
    ]);
    // The kept-local line sits under the list; the old step selects and the Another computer
    // section are gone.
    const kept = screen.getByText(PAGE.kept_local);
    expect(list().compareDocumentPosition(kept) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(screen.queryByRole("combobox", { name: "Step 2" })).toBeNull();
    expect(screen.queryByRole("heading", { name: "Another computer" })).toBeNull();
  });

  it("ticks add a writer last, unticks remove it, and up and down reorder, each saved at once", async () => {
    const s = await open({ sync: { setUp: true }, onlineService: "claude-code" });
    await waitFor(() => expect(tick(CLAUDE_LABEL)).toBeEnabled());
    await s.user.click(tick(CLAUDE_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }]));
    await waitFor(() => expect(tick(STUDIO_LABEL)).toBeEnabled());
    await s.user.click(tick(STUDIO_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, DESK]));
    await waitFor(() =>
      expect(listed().slice(0, 3)).toEqual([`1 ${LOCAL_LABEL}`, `2 ${CLAUDE_LABEL}`, `3 ${STUDIO_LABEL}`]),
    );
    // The online row's up and down move it like any writer (12.2).
    await s.user.click(up(CLAUDE_LABEL)!);
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "ClaudeCode" }, { kind: "Local" }, DESK]));
    await waitFor(() => expect(down(CLAUDE_LABEL)).toBeEnabled());
    await s.user.click(down(CLAUDE_LABEL)!);
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, DESK]));
    await waitFor(() => expect(up(STUDIO_LABEL)).toBeEnabled());
    await s.user.click(up(STUDIO_LABEL)!);
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, DESK, { kind: "ClaudeCode" }]));
    await s.user.click(down(LOCAL_LABEL)!);
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([DESK, { kind: "Local" }, { kind: "ClaudeCode" }]));
    // The writer the old setting mirrors follows the first writer step.
    expect((await s.ipc.summarySettings()).value.writer).toBe("OnThisComputer");
    await waitFor(() =>
      expect(listed().slice(0, 3)).toEqual([`1 ${STUDIO_LABEL}`, `2 ${LOCAL_LABEL}`, `3 ${CLAUDE_LABEL}`]),
    );
    // An untick removes the writer; the rest keep their order, and Claude Code stays picked.
    await s.user.click(tick(LOCAL_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([DESK, { kind: "ClaudeCode" }]));
    await s.user.click(tick(CLAUDE_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([DESK]));
    await waitFor(() => expect(tick(CLAUDE_LABEL)).not.toBeChecked());
    expect(picker()).toHaveValue("ClaudeCode");
    expect((await s.ipc.summaryOrder()).online.service).toBe("ClaudeCode");
  });

  it("refuses a fourth tick with fixed copy and stores nothing", async () => {
    const s = await open({ sync: { setUp: true }, summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }, DESK] });
    await waitFor(() => expect(tick(LAPTOP_LABEL)).toBeEnabled());
    await s.user.click(tick(LAPTOP_LABEL));
    expect(await screen.findByText(TOO_MANY)).toBeInTheDocument();
    expect(tick(LAPTOP_LABEL)).not.toBeChecked();
    expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, DESK]);
    // Unticking one makes room again, and the refusal goes.
    await s.user.click(tick(STUDIO_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }]));
    expect(screen.queryByText(TOO_MANY)).toBeNull();
    await waitFor(() => expect(tick(LAPTOP_LABEL)).toBeEnabled());
    await s.user.click(tick(LAPTOP_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, LAP]));
  });

  it("No summary clears the order and greys the list; Write summaries starts with this computer", async () => {
    const s = await open({ summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }] });
    await waitFor(() => expect(tick(CLAUDE_LABEL)).toBeEnabled());
    expect(writeRadio()).toBeChecked();
    await s.user.click(noneRadio());
    await waitFor(async () => expect((await s.ipc.summaryOrder()).none).toBe(true));
    await waitFor(() => expect(tick(LOCAL_LABEL)).toBeDisabled());
    for (const box of within(list()).getAllByRole("checkbox")) {
      expect(box).toBeDisabled();
      expect(box).not.toBeChecked();
    }
    // The picker is off too.
    expect(picker()).toBeDisabled();
    expect(list()).not.toHaveAttribute("aria-disabled");
    // No writer's form under No summary: saving one would turn summaries back on unasked.
    expect(screen.queryByRole("group", { name: "Claude through Claude Code" })).toBeNull();
    await s.user.click(writeRadio());
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]));
    await waitFor(() => expect(tick(CLAUDE_LABEL)).toBeEnabled());
    expect(picker()).toBeEnabled();
    // The service picked keeps its setup section.
    expect(screen.getByRole("group", { name: "Claude through Claude Code" })).toBeInTheDocument();
    // Unticking the last writer is no summary.
    await s.user.click(tick(LOCAL_LABEL));
    await waitFor(async () => expect((await s.ipc.summaryOrder()).none).toBe(true));
    await waitFor(() => expect(noneRadio()).toBeChecked());
  });

  it("stores the endpoint ticked once its form is saved, keeping the first writer the old setting mirrors", async () => {
    const s = await open();
    await pick(s.user, "OwnServer");
    const form = await openSetup(s.user, "Your own server");
    expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]);
    await s.user.type(within(form).getByLabelText("Address"), "http://192.168.1.20:8080/v1");
    await s.user.type(within(form).getByLabelText("Model"), "qwen");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "Endpoint" }]));
    expect((await s.ipc.summarySettings()).value).toMatchObject({
      writer: "OnThisComputer",
      url: "http://192.168.1.20:8080/v1",
    });
    await waitFor(() => expect(tick(SERVER_LABEL)).toBeChecked());
    expect(listed()[1]).toBe(`2 ${SERVER_LABEL}`);
    expect(screen.queryByText(PAGE.not_set_up)).toBeNull();
  });

  it("refuses to tick a newly set-up endpoint into a full order, keeping what was typed", async () => {
    const s = await open({ sync: { setUp: true }, summaryOrder: [{ kind: "Local" }, DESK, LAP] });
    await waitFor(() => expect(picker()).toBeEnabled());
    await pick(s.user, "OwnServer");
    const form = await openSetup(s.user, "Your own server");
    await s.user.type(within(form).getByLabelText("Address"), "http://192.168.1.20:8080/v1");
    await s.user.type(within(form).getByLabelText("Model"), "qwen");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    expect(await screen.findByText(TOO_MANY)).toBeInTheDocument();
    expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, DESK, LAP]);
    expect((await s.ipc.summarySettings()).value.url).toBe("");
    expect(within(form).getByLabelText("Address")).toHaveValue("http://192.168.1.20:8080/v1");
  });

  it("shows a set-up endpoint's form without ticking it, and saves only its fields", async () => {
    const s = await open({
      summaryOrder: [{ kind: "Local" }],
      onlineService: "endpoint",
      summaries: { settings: { url: "http://192.168.1.20:8080/v1", model: "qwen" } },
    });
    await waitFor(() => expect(tick(SERVER_LABEL)).toBeEnabled());
    expect(tick(SERVER_LABEL)).not.toBeChecked();
    const form = await openSetup(s.user, "Your own server");
    const model = within(form).getByLabelText("Model");
    expect(model).toHaveValue("qwen");
    await s.user.clear(model);
    await s.user.type(model, "qwen-big");
    await s.user.click(within(form).getByRole("button", { name: "Save endpoint" }));
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.model).toBe("qwen-big"));
    expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]);
    expect(tick(SERVER_LABEL)).not.toBeChecked();
  });

  it("greys this computer as not downloaded while no summary model is installed", async () => {
    const s = await open({ noModels: true, onlineService: "claude-code" });
    await waitFor(() => expect(within(rows()[0]).getByText(PAGE.not_downloaded)).toBeInTheDocument());
    // Ticked by default: it stays and can be unticked, but not ticked again until a model is in.
    expect(tick(LOCAL_LABEL)).toBeChecked();
    await s.user.click(tick(LOCAL_LABEL));
    await waitFor(async () => expect((await s.ipc.summaryOrder()).none).toBe(true));
    await s.user.click(writeRadio());
    await waitFor(() => expect(tick(CLAUDE_LABEL)).toBeEnabled());
    await s.user.click(tick(CLAUDE_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }]));
    await s.user.click(tick(LOCAL_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "ClaudeCode" }]));
    await waitFor(() => expect(tick(LOCAL_LABEL)).not.toBeChecked());
    expect(tick(LOCAL_LABEL)).toBeDisabled();
    // Its model can be downloaded right here.
    expect(screen.getByRole("group", { name: "Which model writes the summary" })).toBeInTheDocument();
    // The online service stays tickable: its sign-in is asked only once its card shows.
    expect(tick(CLAUDE_LABEL)).toBeEnabled();
  });

  it("says on the online row that the agent CLI is not signed in, and leaves it tickable", async () => {
    const s = await open({ summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }] });
    // The sample Surface's Claude Code is installed but signed out; its card asks once it shows.
    await waitFor(() => expect(within(onlineRows()[0]).getByText(SETUP_COPY.notSignedIn)).toBeInTheDocument());
    // Still tickable both ways, and the row keeps saying it once unticked.
    await s.user.click(tick(CLAUDE_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]));
    expect(tick(CLAUDE_LABEL)).toBeEnabled();
    expect(within(onlineRows()[0]).getByText(SETUP_COPY.notSignedIn)).toBeInTheDocument();
    await s.user.click(tick(CLAUDE_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }]));
  });

  it("says nothing on a signed-in agent CLI's row", async () => {
    await open({ machine: "desktop", summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }] });
    const row = tick(CLAUDE_LABEL).closest("li") as HTMLElement;
    // Wait for the check to answer (its card's header says so), then look at the row.
    await waitFor(() => expect(screen.getAllByText(new RegExp(SETUP_COPY.signedIn)).length).toBeGreaterThan(0));
    expect(within(row).queryByText(SETUP_COPY.notSignedIn)).toBeNull();
    expect(within(row).queryByText(SETUP_COPY.signedIn)).toBeNull();
  });

  it("keeps the keyboard focus on the control used, on its row wherever it moved", async () => {
    const s = await open({ sync: { setUp: true }, summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }] });
    await waitFor(() => expect(tick(STUDIO_LABEL)).toBeEnabled());
    // The rank and where it goes reach a screen reader with the tick box.
    expect(tick(LOCAL_LABEL)).toHaveAccessibleDescription(
      `${fillCopy(PAGE.step, { n: "1" })} ${ORDER_COPY.destinations.local}`,
    );
    await s.user.click(tick(STUDIO_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, DESK]));
    await waitFor(() => expect(document.activeElement).toBe(tick(STUDIO_LABEL)));
    await s.user.click(up(STUDIO_LABEL)!);
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, DESK, { kind: "ClaudeCode" }]));
    await waitFor(() => expect(document.activeElement).toBe(up(STUDIO_LABEL)));
    // Moved to the top: up cannot be used, so the focus goes to its down button.
    await s.user.click(up(STUDIO_LABEL)!);
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([DESK, { kind: "Local" }, { kind: "ClaudeCode" }]));
    await waitFor(() => expect(document.activeElement).toBe(down(STUDIO_LABEL)));
    // From the keyboard too: Enter on the focused button moves it again, focus kept.
    await s.user.keyboard("{Enter}");
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, DESK, { kind: "ClaudeCode" }]));
    await waitFor(() => expect(document.activeElement).toBe(down(STUDIO_LABEL)));
    // The online row's own buttons keep the focus the same way.
    await s.user.click(up(CLAUDE_LABEL)!);
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, DESK]));
    await waitFor(() => expect(document.activeElement).toBe(up(CLAUDE_LABEL)));
    // An untick keeps the focus on the tick box, now in the unticked part of the list.
    await s.user.click(tick(LOCAL_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "ClaudeCode" }, DESK]));
    await waitFor(() => expect(document.activeElement).toBe(tick(LOCAL_LABEL)));
  });

  it("offers another computer as a writer, and summarising during the meeting follows the order", async () => {
    const s = await open({ sync: { setUp: true } });
    await waitFor(() => expect(tick(STUDIO_LABEL)).toBeEnabled());
    const live = () => screen.getByRole("switch", { name: "Summarise during the meeting" });
    await s.user.click(tick(STUDIO_LABEL));
    await waitFor(() => expect(up(STUDIO_LABEL)).toBeEnabled());
    await s.user.click(up(STUDIO_LABEL)!);
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([DESK, { kind: "Local" }]));
    // A computer then the local model: summarising during the meeting can run.
    await waitFor(() => expect(live()).toBeEnabled());
    await s.user.click(tick(LOCAL_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([DESK]));
    await waitFor(() => expect(live()).toBeDisabled());
  });

  it("refuses a fourth step with the app's copy", async () => {
    const s = await open();
    await expect(
      s.ipc.setSummaryOrder(
        [{ kind: "Local" }, { kind: "ClaudeCode" }, { kind: "Codex" }, { kind: "Endpoint" }],
        null,
      ),
    ).rejects.toMatchObject({ error: { kind: "invalid_setting", message: ORDER_COPY.refusals.too_many_steps } });
    await expect(s.ipc.setSummaryOrder([{ kind: "Local" }, { kind: "Local" }], null)).rejects.toMatchObject({
      error: { message: ORDER_COPY.refusals.twice },
    });
  });

  it("offers no change to an order that has not been read yet", async () => {
    let release: () => void = () => {};
    const gate = new Promise<void>((r) => {
      release = r;
    });
    const s = await open(
      {
        sync: { setUp: true },
        summaryOrder: [DESK, { kind: "Endpoint" }, { kind: "Local" }],
        summaries: { settings: { url: "http://192.168.1.20:8080/v1", model: "qwen" } },
      },
      (ipc) => ({
        ...ipc,
        summaryOrder: async () => {
          await gate;
          return ipc.summaryOrder();
        },
      }),
    );
    // While the order loads, no writer is offered and nothing is ticked or chosen.
    const local = tick(LOCAL_LABEL);
    expect(local).toBeDisabled();
    expect(tick(PAGE.online_service)).toBeDisabled();
    expect(picker()).toBeDisabled();
    expect(within(list()).getAllByRole("checkbox").some((b) => (b as HTMLInputElement).checked)).toBe(false);
    expect(screen.getAllByRole("radio").some((r) => (r as HTMLInputElement).checked)).toBe(false);
    expect(noneRadio()).toBeDisabled();
    expect(screen.getByText(PAGE.reading)).toBeInTheDocument();
    // No writer's form is offered either (the endpoint's would store an order of its own).
    expect(screen.queryByRole("group", { name: "Your own server" })).toBeNull();
    await s.user.click(local);
    expect(await stored(s.ipc)).toEqual([DESK, { kind: "Endpoint" }, { kind: "Local" }]);
    release();
    await waitFor(() => expect(local).toBeEnabled());
    expect(listed().slice(0, 3)).toEqual([`1 ${STUDIO_LABEL}`, `2 ${SERVER_LABEL}`, `3 ${LOCAL_LABEL}`]);
  });

  it("says when the order cannot be read and offers nothing that would replace it", async () => {
    const s = await open({ summaryOrder: [DESK, { kind: "Local" }] }, (ipc) => ({
      ...ipc,
      summaryOrder: async () => {
        throw new Error("the order is unreadable here");
      },
    }));
    expect(await screen.findByText(PAGE.not_read)).toBeInTheDocument();
    expect(tick(LOCAL_LABEL)).toBeDisabled();
    expect(picker()).toBeDisabled();
    expect(noneRadio()).toBeDisabled();
    await s.user.click(tick(LOCAL_LABEL));
    await s.user.click(noneRadio());
    expect(await stored(s.ipc)).toEqual([DESK, { kind: "Local" }]);
  });

  it("never sends a writer the order no longer holds, even when the settings cannot be read again", async () => {
    let reads = 0;
    const base = createMockIpc({ meetings: [], summaryOrder: [{ kind: "ClaudeCode" }, { kind: "Local" }] });
    const ipc: Ipc = {
      ...base,
      summarySettings: async () => {
        reads += 1;
        if (reads > 1) throw new Error("the settings cannot be read now");
        return base.summarySettings();
      },
    };
    const { result } = renderHook(() => useSummaries(), {
      wrapper: ({ children }) => <IpcProvider ipc={ipc}>{children}</IpcProvider>,
    });
    await waitFor(() => expect(result.current.order).not.toBeNull());
    let saved = false;
    await act(async () => {
      saved = await result.current.saveOrder([{ kind: "Local" }]);
    });
    // The order was saved: no "not saved" for the failed read that followed.
    expect(saved).toBe(true);
    expect(result.current.notice).toBeNull();
    expect(await stored(base)).toEqual([{ kind: "Local" }]);
    // A later save of another setting keeps the order (the old writer would put Claude back).
    await act(async () => {
      expect(await result.current.save({ live: true })).toBe(true);
    });
    expect((await base.summarySettings()).value.live).toBe(true);
    expect(await stored(base)).toEqual([{ kind: "Local" }]);
  });

  it("saves nothing over an order that was not read", async () => {
    const base = createMockIpc({ meetings: [], summaryOrder: [{ kind: "ClaudeCode" }, { kind: "Local" }] });
    const ipc: Ipc = {
      ...base,
      summaryOrder: async () => {
        throw new Error("the order is unreadable here");
      },
    };
    const { result } = renderHook(() => useSummaries(), {
      wrapper: ({ children }) => <IpcProvider ipc={ipc}>{children}</IpcProvider>,
    });
    await waitFor(() => expect(result.current.orderError).not.toBeNull());
    await waitFor(() => expect(result.current.load.status).toBe("loaded"));
    let saved = true;
    await act(async () => {
      saved = await result.current.saveOrder([{ kind: "Codex" }]);
    });
    expect(saved).toBe(false);
    let picked: unknown = "not asked";
    await act(async () => {
      picked = await result.current.pickService("Codex");
    });
    expect(picked).toBeNull();
    expect(await stored(base)).toEqual([{ kind: "ClaudeCode" }, { kind: "Local" }]);
    expect((await base.summaryOrder()).online.service).toBe("ClaudeCode");
  });
});

describe("Settings, Summaries, the one Online service row (T14.72, docs/summary-order.md 12.8)", () => {
  const CLAUDE_GROUP = "Claude through Claude Code";
  const CODEX_GROUP = "OpenAI through Codex";
  const ENDPOINT_GROUPS = [ORDER_COPY.steps.endpoint_unset, ORDER_COPY.steps.endpoint_anthropic];
  const groupOf = (name: string) => screen.queryByRole("group", { name });

  it("reads Online service and Choose a service, greyed and not tickable, until a service is picked", async () => {
    const s = await open();
    await waitFor(() => expect(picker()).toBeEnabled());
    const row = onlineRows()[0];
    expect(within(row).getByText(PAGE.online_service)).toBeInTheDocument();
    // "Choose a service" is said once: by the picker, never also as a badge beside the name.
    expect(within(row).getAllByText(PAGE.choose_service).map((e) => e.tagName)).toEqual(["OPTION"]);
    expect(tick(PAGE.online_service)).toBeDisabled();
    // Greyed: the row carries a style the tickable row does not.
    const greyed = row.className;
    expect(picker()).toHaveDisplayValue(PAGE.choose_service);
    // Nothing picked: no setup section of any online service.
    for (const g of [CLAUDE_GROUP, CODEX_GROUP, ...ENDPOINT_GROUPS]) expect(groupOf(g)).toBeNull();
    // Control: once picked, the row is named by the service and can be ticked.
    await pick(s.user, "Codex");
    await waitFor(() => expect(tick(CODEX_LABEL)).toBeEnabled());
    expect(within(onlineRows()[0]).queryByText(PAGE.choose_service)).toBeNull();
    expect(screen.queryByRole("option", { name: PAGE.choose_service })).toBeNull();
    expect(onlineRows()[0].className).not.toBe(greyed);
    // Control for the badge: a reason that is not the picker's own still shows beside the name.
    await pick(s.user, "OwnServer");
    await waitFor(() => expect(within(onlineRows()[0]).getByText(PAGE.not_set_up)).toBeInTheDocument());
  });

  it("applies the picker at once, with no Save, and picking alone ticks nothing", async () => {
    const s = await open();
    const set = vi.spyOn(s.ipc, "setOnlineService");
    await waitFor(() => expect(picker()).toBeEnabled());
    await pick(s.user, "ClaudeCode");
    await waitFor(async () => expect((await s.ipc.summaryOrder()).online.service).toBe("ClaudeCode"));
    expect(set).toHaveBeenCalledTimes(1);
    expect(set).toHaveBeenCalledWith("ClaudeCode");
    expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]);
    // Control: picking the service already picked asks nothing.
    await pick(s.user, "ClaudeCode");
    expect(set).toHaveBeenCalledTimes(1);
  });

  it("keeps one row with the constant key, its picker keeping the focus across a change", async () => {
    const s = await open({ summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }] });
    await waitFor(() => expect(picker()).toBeEnabled());
    const row = onlineRows()[0];
    expect(row).toHaveAttribute("data-step", ONLINE_ROW_KEY);
    const select = picker();
    select.focus();
    await pick(s.user, "Codex");
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "Codex" }]));
    await waitFor(() => expect(tick(CODEX_LABEL)).toBeChecked());
    // The same row and the same picker (the key did not change with the step), still focused.
    expect(onlineRows()).toHaveLength(1);
    expect(onlineRows()[0]).toBe(row);
    expect(picker()).toBe(select);
    expect(document.activeElement).toBe(select);
    // Control: the step's own key would have changed: the row is no longer keyed by it.
    expect(row).not.toHaveAttribute("data-step", "Codex");
  });

  it("renders only the setup section of the service picked", async () => {
    const s = await open({ machine: "desktop", summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }] });
    await screen.findByRole("group", { name: CLAUDE_GROUP });
    for (const g of [CODEX_GROUP, ...ENDPOINT_GROUPS]) expect(groupOf(g)).toBeNull();
    await pick(s.user, "Codex");
    await screen.findByRole("group", { name: CODEX_GROUP });
    for (const g of [CLAUDE_GROUP, ...ENDPOINT_GROUPS]) expect(groupOf(g)).toBeNull();
    await pick(s.user, "AnthropicApi");
    await screen.findByRole("group", { name: ORDER_COPY.steps.endpoint_anthropic });
    for (const g of [CLAUDE_GROUP, CODEX_GROUP, ORDER_COPY.steps.endpoint_unset]) expect(groupOf(g)).toBeNull();
    // The form has no provider: the picker is the provider.
    const form = await openSetup(s.user, ORDER_COPY.steps.endpoint_anthropic);
    expect(within(form).queryByRole("combobox", { name: "Provider" })).toBeNull();
    expect((await s.ipc.summarySettings()).value.provider).toBe("Anthropic");
    await pick(s.user, "OwnServer");
    await screen.findByRole("group", { name: ORDER_COPY.steps.endpoint_unset });
    expect((await s.ipc.summarySettings()).value.provider).toBe("OpenAiCompatible");
    for (const g of [CLAUDE_GROUP, CODEX_GROUP, ORDER_COPY.steps.endpoint_anthropic]) expect(groupOf(g)).toBeNull();
  });

  it("shows the picked service's destination and privacy line before it is ticked", async () => {
    const s = await open({ machine: "desktop" });
    await waitFor(() => expect(picker()).toBeEnabled());
    // Control: no service picked, no privacy line.
    expect(screen.queryByText(CLAUDE_CODE_PRIVACY)).toBeNull();
    expect(screen.queryByText(CODEX_PRIVACY)).toBeNull();
    await pick(s.user, "Codex");
    await waitFor(() => expect(tick(CODEX_LABEL)).toBeEnabled());
    expect(tick(CODEX_LABEL)).not.toBeChecked();
    expect(within(onlineRows()[0]).getByText(ORDER_COPY.destinations.codex)).toBeInTheDocument();
    expect(within(onlineRows()[0]).getByText(CODEX_PRIVACY)).toBeInTheDocument();
    expect(tick(CODEX_LABEL)).toHaveAccessibleDescription(`${ORDER_COPY.destinations.codex} ${CODEX_PRIVACY}`);
    expect(screen.queryByText(CLAUDE_CODE_PRIVACY)).toBeNull();
    await pick(s.user, "ClaudeCode");
    await waitFor(() => expect(within(onlineRows()[0]).getByText(CLAUDE_CODE_PRIVACY)).toBeInTheDocument());
    expect(screen.queryByText(CODEX_PRIVACY)).toBeNull();
    // An endpoint service has its destination and no agent CLI privacy line.
    await pick(s.user, "AnthropicApi");
    await waitFor(() =>
      expect(within(onlineRows()[0]).getByText(ORDER_COPY.destinations.endpoint_anthropic)).toBeInTheDocument(),
    );
    expect(tick(ORDER_COPY.steps.endpoint_anthropic)).toBeDisabled();
    expect(screen.queryByText(CLAUDE_CODE_PRIVACY)).toBeNull();
  });

  it("opens the endpoint's section, focus kept on the picker, when a pick leaves it unticked", async () => {
    const s = await open({ summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }] });
    await waitFor(() => expect(picker()).toBeEnabled());
    picker().focus();
    await pick(s.user, "OwnServer");
    // Not set up: it cannot be a step, so it leaves the order (12.6) and its section opens.
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]));
    const form = await screen.findByRole("group", { name: ORDER_COPY.steps.endpoint_unset });
    await waitFor(() => expect(setupToggle(form)).toHaveAttribute("aria-expanded", "true"));
    expect(document.activeElement).toBe(picker());
  });

  it("leaves the endpoint's section closed when the pick unticked nothing (the control)", async () => {
    const s = await open();
    await waitFor(() => expect(picker()).toBeEnabled());
    await pick(s.user, "OwnServer");
    const form = await screen.findByRole("group", { name: ORDER_COPY.steps.endpoint_unset });
    expect(setupToggle(form)).toHaveAttribute("aria-expanded", "false");
  });

  it("fills Anthropic's address as a draft, storing nothing, when Anthropic API is picked", async () => {
    const s = await open();
    await waitFor(() => expect(picker()).toBeEnabled());
    await pick(s.user, "AnthropicApi");
    const form = await openSetup(s.user, ORDER_COPY.steps.endpoint_anthropic);
    expect(within(form).getByLabelText("Address")).toHaveValue(MOCK_ANTHROPIC_URL);
    expect((await s.ipc.summarySettings()).value).toMatchObject({ provider: "Anthropic", url: "" });
    // The row is still not set up: the fill is not stored.
    expect(tick(ORDER_COPY.steps.endpoint_anthropic)).toBeDisabled();
    expect(within(onlineRows()[0]).getByText(PAGE.not_set_up)).toBeInTheDocument();
  });

  it("replaces the cached settings with the pick's, so a later save sends the new provider", async () => {
    const s = await open();
    await waitFor(() => expect(picker()).toBeEnabled());
    const save = vi.spyOn(s.ipc, "setSummarySettings");
    await pick(s.user, "AnthropicApi");
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.provider).toBe("Anthropic"));
    const live = screen.getByRole("switch", { name: "Summarise during the meeting" });
    await waitFor(() => expect(live).toBeEnabled());
    await s.user.click(live);
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.live).toBe(true));
    // Sent with the provider the pick stored, never the one the page started with.
    expect(save).toHaveBeenCalledTimes(1);
    expect(save.mock.calls[0][0].provider).toBe("Anthropic");
    expect(screen.queryByText("This setting was not saved")).toBeNull();
  });
});
