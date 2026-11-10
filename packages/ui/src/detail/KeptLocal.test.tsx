// A meeting kept on this computer (T12.13): the start screen's box and the detail's bar, with
// "Allow" behind a confirm.

import { act, cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { type Ipc, type MeetingListItem, type MockIpcData, createMockIpc, sampleMeetings } from "../ipc";
import { IpcProvider } from "../ipc/context";
import { KEEP_LOCAL_HINT } from "../meeting/MeetingViews";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { AppShell } from "../shell/AppShell";
import { KEPT_LOCAL_TEXT, allowWriterText } from "./KeptLocal";

const ITEM: MeetingListItem = { ...sampleMeetings()[1], started_at_ms: 2_000 };
const OTHER: MeetingListItem = { ...sampleMeetings()[2], started_at_ms: 1_000 };

async function setup(data: MockIpcData = {}) {
  const calls: unknown[][] = [];
  const timer = createManualTimer(10_000);
  const ipc: Ipc = createMockIpc({
    timer,
    meetings: [ITEM, OTHER],
    onCall: (c) => calls.push(c),
    ...data,
  });
  const user = userEvent.setup();
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell timer={timer} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  await screen.findByRole("heading", { level: 1, name: ITEM.title });
  return { calls, timer, ipc, user };
}

const claudeCode: MockIpcData["summaries"] = { settings: { writer: "ClaudeCode" } };

beforeEach(() => {
  installMatchMedia(false);
});

describe("keeping a meeting on this computer", () => {
  it("is off on the start screen and follows the box into the started meeting", async () => {
    for (const tick of [false, true]) {
      const { ipc, user, timer } = await setup();
      const nav = await screen.findByRole("navigation");
      await user.click(within(nav).getByRole("button", { name: "New meeting" }));
      const box = screen.getByRole("checkbox", { name: "Keep this meeting on this computer" });
      expect(box).not.toBeChecked();
      expect(screen.getByText(KEEP_LOCAL_HINT)).toBeInTheDocument();
      if (tick) await user.click(box);
      await user.click(screen.getByRole("button", { name: "Start" }));
      await act(async () => {
        timer.advance(0);
      });
      await screen.findByRole("region", { name: "Meeting" });
      const id = (await ipc.listMeetings()).find((m) => m.state === "Recording")!.id;
      expect((await ipc.getMeeting(id)).kept_local !== null).toBe(tick);
      cleanup();
    }
  });

  it("shows the bar on a kept meeting only, with no way back while the writer is local", async () => {
    await setup({ keptLocal: [ITEM.id] });
    expect(await screen.findByText("Kept on this computer")).toBeInTheDocument();
    expect(screen.getByText(KEPT_LOCAL_TEXT)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /summarise it$/ })).toBeNull();
    expect(document.body.textContent ?? "").not.toMatch(/—|please|!/i);
  });

  it("lets the writer have it only after the confirm, and sends nothing", async () => {
    const { calls, user } = await setup({ keptLocal: [ITEM.id], summaries: claudeCode });
    const allow = await screen.findByRole("button", { name: "Let Claude Code summarise it" });
    // Cancel changes nothing.
    await user.click(allow);
    let dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText(allowWriterText("Claude Code"))).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(calls.filter((c) => c[0] === "allowSummaryWriter")).toEqual([]);
    expect(screen.getByText("Kept on this computer")).toBeInTheDocument();
    // Allow: the bar goes, and no summary was asked for.
    await user.click(screen.getByRole("button", { name: "Let Claude Code summarise it" }));
    dialog = await screen.findByRole("alertdialog");
    await user.click(within(dialog).getByRole("button", { name: "Allow" }));
    await screen.findByRole("heading", { level: 1, name: ITEM.title });
    await act(async () => {});
    expect(screen.queryByText("Kept on this computer")).toBeNull();
    expect(calls.filter((c) => c[0] === "allowSummaryWriter")).toEqual([["allowSummaryWriter", ITEM.id]]);
    expect(calls.filter((c) => c[0] === "resummarise")).toEqual([]);
    // The page stays usable after the dialog closes (tabster's hide pass).
    await act(() => new Promise((r) => setTimeout(r, 400)));
    expect(screen.getByRole("tab", { name: "Summary" })).toBeInTheDocument();
  });

  it("is not shown on a meeting that is not kept", async () => {
    await setup({ keptLocal: [OTHER.id], summaries: claudeCode });
    await screen.findByText(/TL;DR/);
    expect(screen.queryByText("Kept on this computer")).toBeNull();
  });
});
