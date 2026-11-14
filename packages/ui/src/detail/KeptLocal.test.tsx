// A meeting kept on this computer (T12.13): the start screen's box and the detail's bar, with
// "Allow" behind a confirm.

import { act, cleanup, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { type MockIpcData } from "../ipc";
import { MOCK_STILL_WORKING_COPY } from "../ipc/mock";
import { KEEP_LOCAL_HINT } from "../meeting/MeetingViews";
import { ITEM, OTHER, setup } from "../test/meetingDetail";
import { KEPT_LOCAL_TEXT, allowWriterText } from "./KeptLocal";

const claudeCode: MockIpcData["summaries"] = { settings: { writer: "ClaudeCode" } };

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
    await user.click(await screen.findByRole("button", { name: "Let Claude Code summarise it" }));
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

  it("refuses the writer while the meeting is still worked on, with the reason", async () => {
    const { calls, ipc, user } = await setup({ keptLocal: [ITEM.id], summaries: claudeCode });
    await ipc.resummarise(ITEM.id);
    await user.click(await screen.findByRole("button", { name: "Let Claude Code summarise it" }));
    const dialog = await screen.findByRole("alertdialog");
    await user.click(within(dialog).getByRole("button", { name: "Allow" }));
    expect(await screen.findByText(MOCK_STILL_WORKING_COPY)).toBeInTheDocument();
    expect(screen.getByText("The meeting could not be changed")).toBeInTheDocument();
    expect(calls.filter((c) => c[0] === "allowSummaryWriter")).toEqual([["allowSummaryWriter", ITEM.id]]);
    expect((await ipc.getMeeting(ITEM.id)).kept_local).not.toBeNull();
    expect(screen.getByText("Kept on this computer")).toBeInTheDocument();
    expect(MOCK_STILL_WORKING_COPY).not.toMatch(/—|please|!/i);
    await act(() => new Promise((r) => setTimeout(r, 400)));
  });

  it("is not shown on a meeting that is not kept", async () => {
    await setup({ keptLocal: [OTHER.id], summaries: claudeCode });
    await screen.findByText(/TL;DR/);
    expect(screen.queryByText("Kept on this computer")).toBeNull();
  });
});
