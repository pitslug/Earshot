// Keyboard reach of the meeting detail (T14.19): the Speakers button moves focus into the panel
// it opens (an automatic open does not), and the transcript is one tab stop whose lines are
// reached with the arrow keys and whose buttons are reached from their line.
import { act, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { type Ipc, createMockIpc, sampleDetail } from "../ipc";
import { IpcProvider } from "../ipc/context";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { ITEM, OTHER, panel, setup } from "../test/meetingDetail";
import { MeetingDetailView } from "./MeetingDetailView";

const table = () => screen.getByRole("table", { name: "Transcript" });
const rows = () => within(table()).getAllByRole("row");
const LINES = sampleDetail(ITEM).transcript.length;

/** The detail on its Transcript tab. */
async function transcript() {
  const s = await setup();
  await s.user.click(await screen.findByRole("tab", { name: "Transcript" }));
  await screen.findByRole("table", { name: "Transcript" });
  return s;
}

describe("Speakers panel focus", () => {
  it("an automatic open leaves focus where it is", async () => {
    await setup();
    await screen.findByText(/TL;DR/);
    // The panel opened by itself (voices need names).
    expect(panel()).toBeInTheDocument();
    expect(panel().contains(document.activeElement)).toBe(false);
  });

  it("opening it from its button moves focus into it, and Close gives it back", async () => {
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(within(panel()).getByRole("button", { name: "Close Speakers" }));
    const button = screen.getByRole("button", { name: /^Speakers/ });
    await waitFor(() => expect(button).toHaveFocus());
    await user.keyboard("{Enter}");
    await waitFor(() => expect(within(panel()).getByRole("heading", { name: "Speakers" })).toHaveFocus());
    await user.click(within(panel()).getByRole("button", { name: "Close Speakers" }));
    await waitFor(() => expect(button).toHaveFocus());
  });
});

describe("Transcript lines from the keyboard", () => {
  it("are one tab stop: Tab enters on a line and the next Tab leaves the transcript", async () => {
    const { user } = await transcript();
    expect(rows().filter((r) => r.tabIndex === 0)).toEqual([rows()[0]]);
    // No line's buttons are tab stops of their own.
    for (const b of within(table()).getAllByRole("button")) expect(b.tabIndex).toBe(-1);
    screen.getByRole("button", { name: "Find in transcript" }).focus();
    await user.tab();
    expect(rows()[0]).toHaveFocus();
    await user.tab();
    expect(table().contains(document.activeElement)).toBe(false);
  });

  it("move between lines with the arrow keys, Home and End, and the tab stop follows", async () => {
    const { user } = await transcript();
    rows()[0].focus();
    await user.keyboard("{ArrowDown}");
    expect(rows()[1]).toHaveFocus();
    await user.keyboard("{ArrowDown}{ArrowDown}{ArrowUp}");
    expect(rows()[2]).toHaveFocus();
    expect(rows().filter((r) => r.tabIndex === 0)).toEqual([rows()[2]]);
    await user.keyboard("{End}");
    expect(rows()[LINES - 1]).toHaveFocus();
    await user.keyboard("{ArrowDown}");
    expect(rows()[LINES - 1]).toHaveFocus();
    await user.keyboard("{Home}");
    expect(rows()[0]).toHaveFocus();
    await user.keyboard("{ArrowUp}");
    expect(rows()[0]).toHaveFocus();
  });

  it("reach a line's Edit and menu buttons from the line, and come back to it", async () => {
    const { user } = await transcript();
    rows()[0].focus();
    await user.keyboard("{Enter}");
    const edit = screen.getByRole("button", { name: "Edit line 1" });
    expect(edit).toHaveFocus();
    // Enter on the line only moves to its Edit button: an edit starts from the button.
    expect(screen.queryByRole("textbox", { name: "Line 1" })).toBeNull();
    await user.keyboard("{ArrowRight}");
    expect(screen.getByRole("button", { name: "More actions for line 1" })).toHaveFocus();
    await user.keyboard("{ArrowRight}");
    expect(screen.getByRole("button", { name: "More actions for line 1" })).toHaveFocus();
    await user.keyboard("{ArrowLeft}{ArrowLeft}");
    expect(rows()[0]).toHaveFocus();
    // From a button, up and down go to the neighbouring lines.
    await user.keyboard("{ArrowRight}");
    expect(edit).toHaveFocus();
    await user.keyboard("{Enter}");
    expect(screen.getByRole("textbox", { name: "Line 1" })).toHaveFocus();
  });

  it("reach a line's word marks from the line, and a mark still opens its popover", async () => {
    const { user } = await transcript();
    const mark = screen.getByRole("button", { name: "Suggestion for Reed" });
    const line = mark.closest('[role="row"]') as HTMLElement;
    line.focus();
    await user.keyboard("{ArrowRight}");
    expect(mark).toHaveFocus();
    await user.keyboard("{Enter}");
    expect(await screen.findByRole("dialog", { name: "Suggestion for Reed" })).toBeInTheDocument();
    // Arrow keys inside the popover are its own.
    await user.keyboard("{ArrowDown}");
    expect(line.contains(document.activeElement)).toBe(false);
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(mark).toHaveFocus();
  });

  it("Shift+F10 on a line opens the menu of its words and gives focus back to the line", async () => {
    const { user } = await transcript();
    rows()[0].focus();
    await user.keyboard("{Shift>}{F10}{/Shift}");
    expect((await screen.findAllByRole("menuitem")).length).toBeGreaterThan(1);
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("menuitem")).toBeNull());
    await waitFor(() => expect(rows()[0]).toHaveFocus());
  });

  it("give focus back to the line when its editor closes by Save, Escape or Cancel", async () => {
    const { user } = await transcript();
    await user.click(screen.getByRole("button", { name: "Edit line 2" }));
    await user.clear(screen.getByRole("textbox", { name: "Line 2" }));
    await user.type(screen.getByRole("textbox", { name: "Line 2" }), "An invented line about the hedge.");
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(rows()[1].textContent).toContain("An invented line about the hedge."));
    await waitFor(() => expect(rows()[1]).toHaveFocus());

    await user.click(screen.getByRole("button", { name: "Edit line 3" }));
    await user.keyboard("{Escape}");
    await waitFor(() => expect(rows()[2]).toHaveFocus());

    await user.click(screen.getByRole("button", { name: "Edit line 4" }));
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(rows()[3]).toHaveFocus());
  });

  it("keep the field and its focus on a refused save", async () => {
    const { user } = await transcript();
    await user.click(screen.getByRole("button", { name: "Edit line 2" }));
    const field = screen.getByRole("textbox", { name: "Line 2" });
    await user.clear(field);
    await user.keyboard("{Enter}");
    expect(await screen.findByText(/A line cannot be left empty/)).toBeInTheDocument();
    expect(screen.getByRole("textbox", { name: "Line 2" })).toBeInTheDocument();
    expect(rows()[1].contains(document.activeElement)).toBe(true);
  });

  it("after a split, focus is on the first part", async () => {
    const { user } = await transcript();
    rows()[0].focus();
    await user.keyboard("{Enter}{ArrowRight}{Enter}");
    await user.click(await screen.findByRole("menuitem", { name: "Split line" }));
    await user.click(screen.getByRole("button", { name: "Split before word 2: the" }));
    await waitFor(() => expect(rows()).toHaveLength(LINES + 1));
    await waitFor(() => expect(rows()[0].contains(document.activeElement)).toBe(true));
  });

  it("go to the line now at its place when the focused line is gone", async () => {
    const base = createMockIpc({ timer: createManualTimer(10_000), meetings: [ITEM, OTHER] });
    const third = sampleDetail(ITEM).transcript[2];
    let removed = false;
    const ipc: Ipc = {
      ...base,
      getMeeting: async (id) => {
        const d = await base.getMeeting(id);
        return removed ? { ...d, transcript: d.transcript.filter((r) => r.segment_id !== third.segment_id) } : d;
      },
    };
    const show = (refresh: number) => (
      <EarshotThemeProvider>
        <IpcProvider ipc={ipc}>
          <MeetingDetailView
            item={ITEM}
            initialTab="transcript"
            refresh={refresh}
            onChanged={() => {}}
            onJobStarted={() => {}}
            onJobDismissed={() => {}}
            onJobCancel={() => {}}
          />
        </IpcProvider>
      </EarshotThemeProvider>
    );
    const view = render(show(0));
    await screen.findByRole("table", { name: "Transcript" });
    act(() => rows()[2].focus());
    const fourth = rows()[3];
    // The meeting is read again (another computer's re-transcription) without that line.
    removed = true;
    view.rerender(show(1));
    await waitFor(() => expect(rows()).toHaveLength(LINES - 1));
    await waitFor(() => expect(fourth).toHaveFocus());
  });

  it("leave typing keys to a line being edited", async () => {
    const { user } = await transcript();
    await user.click(screen.getByRole("button", { name: "Edit line 2" }));
    const field = screen.getByRole("textbox", { name: "Line 2" });
    expect(field).toHaveFocus();
    await user.keyboard("{Home}{ArrowDown}{ArrowUp}{End}x");
    expect(field).toHaveFocus();
    expect((field as HTMLTextAreaElement).value.endsWith("x")).toBe(true);
  });
});
