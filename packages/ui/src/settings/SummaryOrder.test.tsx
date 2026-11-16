// The Summaries page's order of summary writers (T12.63b, T12.68; docs/summary-order.md 2): one
// list of every writer this computer can use, the ticked writers first in their order with a rank
// and up and down buttons, at most three ticked, a writer that is not set up greyed with its
// reason, "No summary" above the list, the kept-local line under it, and where each writer sends
// the transcript.

import { act, renderHook, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import {
  type Ipc,
  IpcProvider,
  type MockIpcData,
  ORDER_COPY,
  type StepChoice,
  createMockIpc,
  fillCopy,
  mockDeviceId,
} from "../ipc";
import { useSummaries } from "./useSummaries";
import { openSettings, renderApp } from "../test/app";
import { createManualTimer } from "../time";

const STUDIO = mockDeviceId("e4a17d30");
const DESK: StepChoice = { kind: "Computer", device: STUDIO };
const PAGE = ORDER_COPY.page;
const LOCAL_LABEL = ORDER_COPY.steps.local;
const CLAUDE_LABEL = ORDER_COPY.steps.claude_code;
const CODEX_LABEL = ORDER_COPY.steps.codex;
const UNSET_LABEL = ORDER_COPY.steps.endpoint_unset;
const STUDIO_LABEL = fillCopy(ORDER_COPY.steps.computer, { device: "Studio", writer: "local model" });
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
    expect(listed()).toEqual([
      `1 ${LOCAL_LABEL}`,
      `2 ${CLAUDE_LABEL}`,
      `- ${UNSET_LABEL}`,
      `- ${CODEX_LABEL}`,
      `- ${fillCopy(ORDER_COPY.steps.computer, { device: "Laptop", writer: "Claude Code" })}`,
      `- ${STUDIO_LABEL}`,
    ]);
    // The rank shows as a number on a ticked row only.
    expect(within(rows()[0]).getByText("1")).toBeInTheDocument();
    expect(within(rows()[1]).getByText("2")).toBeInTheDocument();
    expect(within(rows()[3]).queryByText("3")).toBeNull();
    expect(tick(LOCAL_LABEL)).toBeChecked();
    expect(tick(CODEX_LABEL)).not.toBeChecked();
    for (const [i, text] of [
      [0, ORDER_COPY.destinations.local],
      [1, ORDER_COPY.destinations.claude_code],
      [3, ORDER_COPY.destinations.codex],
      [5, fillCopy(ORDER_COPY.destinations.computer, { device: "Studio", writer: "local model" })],
    ] as const) {
      expect(within(rows()[i]).getByText(text)).toBeInTheDocument();
    }
    // Up and down only on ticked rows; the first cannot go up, the last ticked cannot go down.
    expect(up(LOCAL_LABEL)).toBeDisabled();
    expect(down(LOCAL_LABEL)).toBeEnabled();
    expect(up(CLAUDE_LABEL)).toBeEnabled();
    expect(down(CLAUDE_LABEL)).toBeDisabled();
    expect(up(CODEX_LABEL)).toBeNull();
    expect(down(STUDIO_LABEL)).toBeNull();
    // The endpoint without its address cannot be ticked and says why.
    expect(tick(UNSET_LABEL)).toBeDisabled();
    expect(within(rows()[2]).getByText(PAGE.not_set_up)).toBeInTheDocument();
    expect(within(rows()[0]).queryByText(PAGE.not_set_up)).toBeNull();
    // The kept-local line sits under the list; the old step selects and the Another computer
    // section are gone.
    const kept = screen.getByText(PAGE.kept_local);
    expect(list().compareDocumentPosition(kept) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(screen.queryByRole("combobox", { name: "Step 2" })).toBeNull();
    expect(screen.queryByRole("heading", { name: "Another computer" })).toBeNull();
  });

  it("ticks add a writer last, unticks remove it, and up and down reorder, each saved at once", async () => {
    const s = await open();
    await waitFor(() => expect(tick(CLAUDE_LABEL)).toBeEnabled());
    await s.user.click(tick(CLAUDE_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }]));
    await s.user.click(tick(CODEX_LABEL));
    await waitFor(async () =>
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, { kind: "Codex" }]),
    );
    expect(listed().slice(0, 3)).toEqual([`1 ${LOCAL_LABEL}`, `2 ${CLAUDE_LABEL}`, `3 ${CODEX_LABEL}`]);
    // Claude Code's and Codex's cards show for the writers ticked.
    expect(screen.getByRole("group", { name: "Claude through Claude Code" })).toBeInTheDocument();
    expect(screen.getByRole("group", { name: "OpenAI through Codex" })).toBeInTheDocument();
    await s.user.click(up(CODEX_LABEL)!);
    await waitFor(async () =>
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "Codex" }, { kind: "ClaudeCode" }]),
    );
    await s.user.click(down(LOCAL_LABEL)!);
    await waitFor(async () =>
      expect(await stored(s.ipc)).toEqual([{ kind: "Codex" }, { kind: "Local" }, { kind: "ClaudeCode" }]),
    );
    // The writer the old setting mirrors follows the first writer.
    expect((await s.ipc.summarySettings()).value.writer).toBe("Codex");
    await waitFor(() =>
      expect(listed().slice(0, 3)).toEqual([`1 ${CODEX_LABEL}`, `2 ${LOCAL_LABEL}`, `3 ${CLAUDE_LABEL}`]),
    );
    // An untick removes the writer and its card; the rest keep their order.
    await s.user.click(tick(LOCAL_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Codex" }, { kind: "ClaudeCode" }]));
    await waitFor(() => expect(listed()[2]).toBe(`- ${LOCAL_LABEL}`));
    await s.user.click(tick(CLAUDE_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Codex" }]));
    await waitFor(() => expect(screen.queryByRole("group", { name: "Claude through Claude Code" })).toBeNull());
  });

  it("refuses a fourth tick with fixed copy and stores nothing", async () => {
    const s = await open({
      sync: { setUp: true },
      summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }, { kind: "Codex" }],
    });
    await waitFor(() => expect(tick(STUDIO_LABEL)).toBeEnabled());
    await s.user.click(tick(STUDIO_LABEL));
    expect(await screen.findByText(TOO_MANY)).toBeInTheDocument();
    expect(tick(STUDIO_LABEL)).not.toBeChecked();
    expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, { kind: "Codex" }]);
    // Unticking one makes room again, and the refusal goes.
    await s.user.click(tick(CODEX_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }]));
    expect(screen.queryByText(TOO_MANY)).toBeNull();
    await waitFor(() => expect(tick(STUDIO_LABEL)).toBeEnabled());
    await s.user.click(tick(STUDIO_LABEL));
    await waitFor(async () =>
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, DESK]),
    );
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
    expect(list()).not.toHaveAttribute("aria-disabled");
    expect(screen.queryByRole("group", { name: "Claude through Claude Code" })).toBeNull();
    // No writer's form under No summary: saving one would turn summaries back on unasked.
    expect(screen.queryByRole("group", { name: "Your own server" })).toBeNull();
    await s.user.click(writeRadio());
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]));
    await waitFor(() => expect(tick(CODEX_LABEL)).toBeEnabled());
    expect(screen.getByRole("group", { name: "Your own server" })).toBeInTheDocument();
    // Unticking the last writer is no summary.
    await s.user.click(tick(LOCAL_LABEL));
    await waitFor(async () => expect((await s.ipc.summaryOrder()).none).toBe(true));
    await waitFor(() => expect(noneRadio()).toBeChecked());
  });

  it("stores the endpoint ticked once its form is saved, keeping the first writer the old setting mirrors", async () => {
    const s = await open();
    const form = await screen.findByRole("group", { name: "Your own server" });
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
    const s = await open({ summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }, { kind: "Codex" }] });
    const form = await screen.findByRole("group", { name: "Your own server" });
    await s.user.type(within(form).getByLabelText("Address"), "http://192.168.1.20:8080/v1");
    await s.user.type(within(form).getByLabelText("Model"), "qwen");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    expect(await screen.findByText(TOO_MANY)).toBeInTheDocument();
    expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, { kind: "Codex" }]);
    expect((await s.ipc.summarySettings()).value.url).toBe("");
    expect(within(form).getByLabelText("Address")).toHaveValue("http://192.168.1.20:8080/v1");
  });

  it("shows a set-up endpoint's form without ticking it, and saves only its fields", async () => {
    const s = await open({
      summaryOrder: [{ kind: "Local" }],
      summaries: { settings: { url: "http://192.168.1.20:8080/v1", model: "qwen" } },
    });
    await waitFor(() => expect(tick(SERVER_LABEL)).toBeEnabled());
    expect(tick(SERVER_LABEL)).not.toBeChecked();
    const form = screen.getByRole("group", { name: "Your own server" });
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
    const s = await open({ noModels: true });
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
    // Claude Code and Codex stay tickable: their sign-in is asked only once their card shows.
    expect(tick(CODEX_LABEL)).toBeEnabled();
  });

  it("keeps the keyboard focus on the control used, on its row wherever it moved", async () => {
    const s = await open({ summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }] });
    await waitFor(() => expect(tick(CODEX_LABEL)).toBeEnabled());
    // The rank and where it goes reach a screen reader with the tick box.
    expect(tick(LOCAL_LABEL)).toHaveAccessibleDescription(
      `${fillCopy(PAGE.step, { n: "1" })} ${ORDER_COPY.destinations.local}`,
    );
    await s.user.click(tick(CODEX_LABEL));
    await waitFor(async () =>
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, { kind: "Codex" }]),
    );
    await waitFor(() => expect(document.activeElement).toBe(tick(CODEX_LABEL)));
    await s.user.click(up(CODEX_LABEL)!);
    await waitFor(async () =>
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "Codex" }, { kind: "ClaudeCode" }]),
    );
    await waitFor(() => expect(document.activeElement).toBe(up(CODEX_LABEL)));
    // Moved to the top: up cannot be used, so the focus goes to its down button.
    await s.user.click(up(CODEX_LABEL)!);
    await waitFor(async () =>
      expect(await stored(s.ipc)).toEqual([{ kind: "Codex" }, { kind: "Local" }, { kind: "ClaudeCode" }]),
    );
    await waitFor(() => expect(document.activeElement).toBe(down(CODEX_LABEL)));
    // From the keyboard too: Enter on the focused button moves it again, focus kept.
    await s.user.keyboard("{Enter}");
    await waitFor(async () =>
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "Codex" }, { kind: "ClaudeCode" }]),
    );
    await waitFor(() => expect(document.activeElement).toBe(down(CODEX_LABEL)));
    // An untick keeps the focus on the tick box, now in the unticked part of the list.
    await s.user.click(tick(LOCAL_LABEL));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Codex" }, { kind: "ClaudeCode" }]));
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
    const claude = tick(CLAUDE_LABEL);
    expect(claude).toBeDisabled();
    expect(within(list()).getAllByRole("checkbox").some((b) => (b as HTMLInputElement).checked)).toBe(false);
    expect(screen.getAllByRole("radio").some((r) => (r as HTMLInputElement).checked)).toBe(false);
    expect(noneRadio()).toBeDisabled();
    expect(screen.getByText(PAGE.reading)).toBeInTheDocument();
    // No writer's form is offered either (the endpoint's would store an order of its own).
    expect(screen.queryByRole("group", { name: "Your own server" })).toBeNull();
    await s.user.click(claude);
    expect(await stored(s.ipc)).toEqual([DESK, { kind: "Endpoint" }, { kind: "Local" }]);
    release();
    await waitFor(() => expect(claude).toBeEnabled());
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
    expect(await stored(base)).toEqual([{ kind: "ClaudeCode" }, { kind: "Local" }]);
  });
});
