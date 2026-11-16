// The Summaries page's order of summary writers (T12.63b, docs/summary-order.md 2): step 1 among
// the writers, the other computers and "No summary"; steps 2 and 3 "If it can't"; where each step
// sends the transcript; the "Another computer" section gone.

import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { type Ipc, type MockIpcData, ORDER_COPY, createMockIpc, mockDeviceId } from "../ipc";
import { openSettings, renderApp } from "../test/app";
import { createManualTimer } from "../time";

const STUDIO = mockDeviceId("e4a17d30");

async function open(data: MockIpcData = {}): Promise<{ ipc: Ipc; user: ReturnType<typeof userEvent.setup> }> {
  const timer = createManualTimer(1_000_000);
  const ipc = createMockIpc({ meetings: [], timer, ...data });
  renderApp(ipc, { timer });
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
});
