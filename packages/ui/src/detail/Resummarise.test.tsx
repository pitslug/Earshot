// Re-summarise and the order of summary writers on the meeting detail (T12.63b,
// docs/summary-order.md 4.4 and 5): the split button's "Use my order" and writers, the kept-local
// reason, the last run's account and the job's steps banner.

import { act, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { ORDER_COPY, POST_STEP_MS } from "../ipc";
import { ITEM, setup } from "../test/meetingDetail";
import { STORED_TEXT, jobNotice } from "./MeetingDetailView";

const menuButton = () => screen.getByRole("button", { name: /: choose who writes it$/ });

describe("Meeting detail, re-summarise by the order or one writer", () => {
  it("offers Use my order and the writers that can run here, and runs a writer by name", async () => {
    const s = await setup({ machine: "desktop" });
    await s.user.click(menuButton());
    expect(await screen.findByRole("menuitem", { name: ORDER_COPY.resummarise.order })).toBeInTheDocument();
    // On the desktop sample Claude Code and Codex are installed; no endpoint is set.
    const claude = await screen.findByRole("menuitem", { name: ORDER_COPY.steps.claude_code });
    expect(screen.getByRole("menuitem", { name: ORDER_COPY.steps.local })).toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: ORDER_COPY.steps.endpoint_unset })).toBeNull();
    await s.user.click(claude);
    await waitFor(() =>
      expect(s.calls).toContainEqual(["resummariseWith", ITEM.id, { kind: "ClaudeCode" }]),
    );
    await act(async () => s.timer.advance(POST_STEP_MS));
  });

  it("Use my order runs the order", async () => {
    const s = await setup();
    await s.user.click(menuButton());
    await s.user.click(await screen.findByRole("menuitem", { name: ORDER_COPY.resummarise.order }));
    await waitFor(() => expect(s.calls).toContainEqual(["resummarise", ITEM.id]));
  });

  it("disables the writers that send a kept meeting elsewhere, with the reason", async () => {
    const s = await setup({ machine: "desktop", keptLocal: [ITEM.id] });
    await s.user.click(menuButton());
    const claude = await screen.findByRole("menuitem", { name: new RegExp(`^${ORDER_COPY.steps.claude_code.replace(/[()]/g, "\\$&")}`) });
    expect(claude).toHaveAttribute("aria-disabled", "true");
    expect(claude).toHaveTextContent(ORDER_COPY.resummarise.kept_local);
    expect(screen.getByRole("menuitem", { name: ORDER_COPY.steps.local })).not.toHaveAttribute(
      "aria-disabled",
      "true",
    );
  });

  it("shows the last run's account", async () => {
    const text = ORDER_COPY.named_hand_back.replace("{device}", "Studio").replace("{word}", "declined");
    await setup({ summaryRuns: { [ITEM.id]: text } });
    expect(await screen.findByText(text)).toBeInTheDocument();
  });
});

describe("Re-summary job notices of the order", () => {
  it("leaves the steps to the summary's run note when one was written, and says them when none was", () => {
    const note = ORDER_COPY.later_writer
      .replace("{writer}", ORDER_COPY.writers.local)
      .replace("{steps}", `${ORDER_COPY.writers.claude_code}: ${ORDER_COPY.words.not_signed_in}`);
    const stored = jobNotice({
      job: "Resummarise",
      seq: 1,
      last: {
        type: "Summary",
        meeting: "m",
        result: { kind: "Stored", title: "t", actions: 0, decisions: 0, voices_to_name: 0 },
        note,
      },
    });
    expect(stored).toMatchObject({ intent: "success", text: STORED_TEXT });
    const none = jobNotice({
      job: "Resummarise",
      seq: 1,
      last: {
        type: "Summary",
        meeting: "m",
        result: { kind: "Skipped", reason: "The summary model is not installed." },
        note: "No summary. This computer: not installed; Codex: not installed.",
      },
    });
    expect(none.text).toBe("No summary. This computer: not installed; Codex: not installed.");
    const asked = jobNotice({
      job: "Resummarise",
      seq: 1,
      last: { type: "Offloaded", meeting: "m", message: "Waiting for Studio to write the summary." },
    });
    expect(asked).toMatchObject({ intent: "info", text: "Waiting for Studio to write the summary." });
  });
});
