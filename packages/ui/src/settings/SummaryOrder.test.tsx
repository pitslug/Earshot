// The Summaries page's order of summary writers (T12.63b, docs/summary-order.md 2): step 1 among
// the writers, the other computers and "No summary"; steps 2 and 3 "If it can't"; where each step
// sends the transcript; the "Another computer" section gone.

import { act, renderHook, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { type Ipc, IpcProvider, type MockIpcData, ORDER_COPY, createMockIpc, mockDeviceId } from "../ipc";
import { useSummaries } from "./useSummaries";
import { openSettings, renderApp } from "../test/app";
import { createManualTimer } from "../time";

const STUDIO = mockDeviceId("e4a17d30");

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
const laterGroup = () => screen.getByRole("group", { name: ORDER_COPY.page.if_cant });
const stepSelect = (n: number) => within(laterGroup()).getByRole("combobox", { name: `Step ${n}` });
const optionTexts = (select: HTMLElement) => [...select.querySelectorAll("option")].map((o) => o.textContent);

describe("Settings, Summaries, the order of summary writers", () => {
  it("adds steps 2 and 3, offering only writers not used, each saying where the transcript goes", async () => {
    const s = await open();
    // Step 1 is the local model; step 2 is offered, step 3 not yet.
    expect(optionTexts(stepSelect(2))).toEqual([
      ORDER_COPY.page.nothing_else,
      ORDER_COPY.steps.endpoint_unset,
      ORDER_COPY.steps.claude_code,
      ORDER_COPY.steps.codex,
    ]);
    expect(within(laterGroup()).queryByRole("combobox", { name: "Step 3" })).toBeNull();
    await s.user.selectOptions(stepSelect(2), ORDER_COPY.steps.claude_code);
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }]));
    expect(await screen.findByText(ORDER_COPY.destinations.claude_code, { selector: "[id]" })).toBeInTheDocument();
    // Step 3 offers what steps 1 and 2 do not hold.
    expect(optionTexts(await waitFor(() => stepSelect(3)))).toEqual([
      ORDER_COPY.page.nothing_else,
      ORDER_COPY.steps.endpoint_unset,
      ORDER_COPY.steps.codex,
    ]);
    await s.user.selectOptions(stepSelect(3), ORDER_COPY.steps.codex);
    await waitFor(async () =>
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, { kind: "Codex" }]),
    );
    // Claude Code's and Codex's cards show for the steps that hold them.
    expect(screen.getByRole("group", { name: "Claude through Claude Code" })).toBeInTheDocument();
    expect(screen.getByRole("group", { name: "OpenAI through Codex" })).toBeInTheDocument();
    // "Nothing else" at step 2 removes it and every later step.
    await s.user.selectOptions(stepSelect(2), ORDER_COPY.page.nothing_else);
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]));
    expect(screen.queryByRole("group", { name: "Claude through Claude Code" })).toBeNull();
  });

  it("puts a step 1 choice first and removes it from later steps", async () => {
    const s = await open({ summaryOrder: [{ kind: "Local" }, { kind: "ClaudeCode" }] });
    await waitFor(() => expect(stepSelect(2)).toHaveValue("ClaudeCode"));
    await s.user.click(screen.getByRole("radio", { name: /^Claude \(through Claude Code\)/ }));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "ClaudeCode" }]));
    expect((await s.ipc.summarySettings()).value.writer).toBe("ClaudeCode");
    // "No summary" clears the order.
    await s.user.click(screen.getByRole("radio", { name: /^No summary/ }));
    await waitFor(async () => expect((await s.ipc.summaryOrder()).none).toBe(true));
    expect(screen.queryByRole("group", { name: ORDER_COPY.page.if_cant })).toBeNull();
  });

  it("stores an endpoint as step 2 only once its form is saved", async () => {
    const s = await open();
    await s.user.selectOptions(stepSelect(2), ORDER_COPY.steps.endpoint_unset);
    const form = await screen.findByRole("group", { name: "Your own server" });
    expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]);
    await s.user.type(within(form).getByLabelText("Address"), "http://192.168.1.20:8080/v1");
    await s.user.type(within(form).getByLabelText("Model"), "qwen");
    await s.user.click(within(form).getByRole("button", { name: "Use this endpoint" }));
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "Endpoint" }]));
    // The writer the old setting mirrors stays step 1's.
    expect((await s.ipc.summarySettings()).value).toMatchObject({
      writer: "OnThisComputer",
      url: "http://192.168.1.20:8080/v1",
    });
    await waitFor(() =>
      expect(stepSelect(2)).toHaveDisplayValue(
        ORDER_COPY.steps.endpoint_server.replace("{host}", "192.168.1.20"),
      ),
    );
  });

  it("offers another computer as a step with where it sends the transcript, and has no Another computer section", async () => {
    const s = await open({ sync: { setUp: true } });
    const studio = await screen.findByRole("radio", { name: /^On Studio \(local model\)/ });
    expect(
      screen.getByText(
        ORDER_COPY.destinations.computer.replace("{device}", "Studio").replace("{writer}", "local model"),
      ),
    ).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Another computer" })).toBeNull();
    // Step 1 is what is chosen there; the local model can follow as step 2.
    await s.user.click(studio);
    await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Computer", device: STUDIO }]));
    const live = () => screen.getByRole("switch", { name: "Summarise during the meeting" });
    await waitFor(() => expect(live()).toBeDisabled());
    await s.user.selectOptions(stepSelect(2), ORDER_COPY.steps.local);
    await waitFor(async () =>
      expect(await stored(s.ipc)).toEqual([{ kind: "Computer", device: STUDIO }, { kind: "Local" }]),
    );
    // A computer then the local model: summarising during the meeting can run.
    await waitFor(() => expect(live()).toBeEnabled());
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
    const desk = { kind: "Computer" as const, device: STUDIO };
    const s = await open({ sync: { setUp: true }, summaryOrder: [desk, { kind: "Endpoint" }, { kind: "Local" }], summaries: { settings: { url: "http://192.168.1.20:8080/v1", model: "qwen" } } }, (ipc) => ({
      ...ipc,
      summaryOrder: async () => {
        await gate;
        return ipc.summaryOrder();
      },
    }));
    // While the order loads, step 1 is not offered and nothing is chosen.
    const claude = screen.getByRole("radio", { name: /^Claude \(through Claude Code\)/ });
    expect(claude).toBeDisabled();
    expect(screen.getAllByRole("radio").some((r) => (r as HTMLInputElement).checked)).toBe(false);
    expect(screen.getByText(ORDER_COPY.page.reading)).toBeInTheDocument();
    // No step's form is offered either (the endpoint's would store an order of its own).
    expect(screen.queryByRole("group", { name: "Your own server" })).toBeNull();
    await s.user.click(claude);
    expect(await stored(s.ipc)).toEqual([desk, { kind: "Endpoint" }, { kind: "Local" }]);
    release();
    await waitFor(() => expect(claude).toBeEnabled());
    await waitFor(() => expect(stepSelect(2)).toHaveValue("Endpoint"));
  });

  it("says when the order cannot be read and offers nothing that would replace it", async () => {
    const desk = { kind: "Computer" as const, device: STUDIO };
    const s = await open({ summaryOrder: [desk, { kind: "Local" }] }, (ipc) => ({
      ...ipc,
      summaryOrder: async () => {
        throw new Error("the order is unreadable here");
      },
    }));
    expect(await screen.findByText(ORDER_COPY.page.not_read)).toBeInTheDocument();
    const local = screen.getByRole("radio", { name: /^On this computer/ });
    expect(local).toBeDisabled();
    await s.user.click(screen.getByRole("radio", { name: /^No summary/ }));
    expect(await stored(s.ipc)).toEqual([desk, { kind: "Local" }]);
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
