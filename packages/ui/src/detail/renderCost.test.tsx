// What an edit costs on a long transcript (T14.18): the detail read again after an edit keeps
// the rows that did not change, so only the edited line renders. Lines are counted through
// `rowTime`, which each line calls once per render (as shell/renderCost.test.tsx does).
import { act, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { type MeetingDetail, sampleDetail } from "../ipc";
import { ITEM, setup } from "../test/meetingDetail";
import * as detailModule from "./detail";

vi.mock("./detail", async (original) => {
  const real = await original<typeof import("./detail")>();
  return { ...real, rowTime: vi.fn(real.rowTime) };
});
const rowTimeSpy = vi.mocked(detailModule.rowTime);

const LINES = 50;
const startOf = (i: number) => 60_000 + i * 6_000;

/** The sample meeting with an invented transcript of `LINES` lines. */
function longDetail(): MeetingDetail {
  const base = sampleDetail(ITEM);
  const transcript = Array.from({ length: LINES }, (_, i) => {
    const like = base.transcript[i % base.transcript.length];
    return {
      ...like,
      segment_id: `0190aaaa-0000-7000-8000-${String(i).padStart(12, "0")}`,
      start_ms: startOf(i),
      text: `Invented line number ${i} about the gravel.`,
      edited: false,
      unknown_speaker: false,
      suggestions: [],
      changed_words: [],
    };
  });
  return { ...base, transcript };
}

beforeEach(() => rowTimeSpy.mockClear());

describe("render cost of a long transcript", () => {
  it("render cost: an edit on a 50-line transcript renders only the edited line", async () => {
    const s = await setup({ details: { [ITEM.id]: longDetail() } });
    await s.user.click(await screen.findByRole("tab", { name: "Transcript" }));
    const table = await screen.findByRole("table", { name: "Transcript" });
    expect(within(table).getAllByRole("row")).toHaveLength(LINES);
    // Focus moves the transcript's one tab stop to the line first (T14.19), which renders it
    // and the line it leaves; the edit is counted from there.
    await act(async () => within(table).getAllByRole("row")[10].focus());
    rowTimeSpy.mockClear();
    await s.user.click(screen.getByRole("button", { name: "Edit line 11" }));
    const editor = screen.getByRole("textbox", { name: "Line 11" });
    await s.user.clear(editor);
    await s.user.type(editor, "An invented line about the hedge.");
    await s.user.click(screen.getByRole("button", { name: "Save" }));
    // Each word of a final line is its own element: read the row's text.
    await waitFor(() =>
      expect(within(table).getAllByRole("row")[10].textContent).toContain("An invented line about the hedge."),
    );
    await act(async () => {});
    // The edited line rendered (into the editor, out of it, with its new text); no other did.
    const rendered = new Set(rowTimeSpy.mock.calls.map(([ms]) => ms));
    expect(rendered).toEqual(new Set([startOf(10)]));
  });

  it("render cost: moving between lines renders only the line left and the line reached", async () => {
    const s = await setup({ details: { [ITEM.id]: longDetail() } });
    await s.user.click(await screen.findByRole("tab", { name: "Transcript" }));
    const table = await screen.findByRole("table", { name: "Transcript" });
    await act(async () => within(table).getAllByRole("row")[20].focus());
    rowTimeSpy.mockClear();
    await s.user.keyboard("{ArrowDown}");
    expect(within(table).getAllByRole("row")[21]).toHaveFocus();
    expect(new Set(rowTimeSpy.mock.calls.map(([ms]) => ms))).toEqual(new Set([startOf(20), startOf(21)]));
  });

  it("render cost: a read with nothing changed renders no line", async () => {
    const s = await setup({ details: { [ITEM.id]: longDetail() } });
    await s.user.click(await screen.findByRole("tab", { name: "Transcript" }));
    await screen.findByRole("table", { name: "Transcript" });
    rowTimeSpy.mockClear();
    // A meeting name change reads the detail again; no line changes.
    await s.user.click(screen.getByRole("button", { name: "Rename" }));
    const title = screen.getByRole("textbox", { name: "Meeting title" });
    await s.user.clear(title);
    await s.user.type(title, "An invented title");
    await s.user.click(screen.getByRole("button", { name: "Save" }));
    await act(async () => {});
    expect(s.calls.some(([c]) => c === "renameMeeting")).toBe(true);
    expect(rowTimeSpy).not.toHaveBeenCalled();
  });
});
