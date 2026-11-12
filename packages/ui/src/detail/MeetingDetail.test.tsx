import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  type Ipc,
  IpcFailure,
  MISSING_SEGMENT,
  type MeetingListItem,
  type MockIpcData,
  POST_STEP_MS,
  SAMPLE_SPEAKERS,
  createMockIpc,
  sampleDetail,
  sampleMeetings,
  samplePersons,
} from "../ipc";
import { IpcProvider } from "../ipc/context";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { AppShell } from "../shell/AppShell";
import { COPIED_MS, handoverLine, savedText, summaryMarkdown } from "./detail";

const ITEM: MeetingListItem = { ...sampleMeetings()[1], started_at_ms: 2_000 };
/** An older meeting, so `ITEM` is shown first. */
const OTHER: MeetingListItem = { ...sampleMeetings()[2], started_at_ms: 1_000 };

/** The detail as the app shows it: inside `AppShell` (which follows job events), with
 * `ITEM` selected. */
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

const panel = () => screen.getByRole("complementary", { name: "Speakers" });
const speakerRow = (name: string) => within(panel()).getByRole("listitem", { name });
/** Opens the panel's collapsed list of named voices (T12.19). */
async function openNamed(user: ReturnType<typeof userEvent.setup>) {
  await user.click(within(panel()).getByRole("button", { name: /^Named/ }));
}
/** The list row of a meeting and its trash can, also while an open modal hides the page from
 * assistive technology. */
const listRow = (item: MeetingListItem) =>
  screen.getByRole("button", { name: new RegExp(`^${item.title}`), hidden: true });
const trashName = (item: MeetingListItem) => `Delete ${item.title}`;

/** Longer than tabster's deferred aria-hidden pass (`ModalizerAPI.hiddenUpdate`, 250 ms): a
 * dialog still accessible after this wait survived that pass. */
const TABSTER_HIDE_PASS_MS = 400;

/** Where focus is and whether the alert dialog is hidden, for a failure message. */
function dialogState(): string {
  const d = document.querySelector<HTMLElement>('[role="alertdialog"]');
  const f = document.activeElement;
  const focus = f
    ? `<${f.tagName.toLowerCase()} role=${f.getAttribute("role")} label=${f.getAttribute("aria-label")}> "${(f.textContent ?? "").slice(0, 30)}"`
    : "none";
  const hidden = d ? d.closest('[aria-hidden="true"]') : null;
  return `dialog in DOM: ${d !== null}, hidden by aria-hidden: ${hidden === null ? "no" : hidden === d ? "itself" : "an ancestor"}, focus: ${focus}, focus inside dialog: ${d?.contains(f) ?? false}`;
}

/** The alert dialog as assistive technology sees it; on a miss, says why (T7.3d: on the
 * Surface it stayed aria-hidden). */
async function accessibleDialog(): Promise<HTMLElement> {
  try {
    return await screen.findByRole("alertdialog");
  } catch (e) {
    const now = dialogState();
    await act(() => new Promise((r) => setTimeout(r, 500)));
    throw new Error(`no accessible alertdialog: ${now}; 0.5 s later: ${dialogState()}`, { cause: e });
  }
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("MeetingDetailView", () => {
  it("shows the header, summary and the Speakers panel open on the voices that need names", async () => {
    const { user } = await setup();
    expect(await screen.findByText(/TL;DR/)).toBeInTheDocument();
    expect(screen.getByText(/Final transcript · Summary by Qwen 3.5 9B/)).toBeInTheDocument();
    const toggle = screen.getByRole("button", { name: "Speakers, 2 need names" });
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(toggle).toHaveTextContent("2 need names");
    expect(toggle.getAttribute("aria-controls")).toBe(panel().id);
    expect(within(panel()).getByText(/Names flow into the summary and export/)).toBeInTheDocument();
    expect(screen.getByText("Tick actions in the Obsidian note.")).toBeInTheDocument();
    const actions = screen.getByRole("table", { name: "Actions" });
    expect(within(actions).getAllByRole("row")[0].textContent).toContain("Casey");
    // The two voices that need a name, each with Save and Ignore; the named ones are collapsed.
    const needing = within(panel()).getByRole("region", { name: "Need a name" });
    expect(within(needing).getAllByRole("listitem").map((r) => r.getAttribute("aria-label"))).toEqual([
      "Speaker 2",
      "Speaker 3",
    ]);
    expect(within(needing).getAllByRole("button", { name: /^Save / })).toHaveLength(2);
    expect(within(needing).getAllByRole("button", { name: "Ignore" })).toHaveLength(2);
    expect(within(panel()).queryByRole("listitem", { name: "Priya Shah" })).toBeNull();
    await openNamed(user);
    expect(within(panel()).getAllByRole("listitem")).toHaveLength(4);
    expect(within(speakerRow("Priya Shah")).getByText(/Matched voiceprint · 22% of talk time/)).toBeInTheDocument();
    // The tickbox for every voice is gone: consent is per voice, by Save.
    expect(within(panel()).queryByRole("checkbox")).toBeNull();
    // Copy rules: no em dashes, no "please", no exclamation marks.
    expect(document.body.textContent ?? "").not.toMatch(/\u2014|please|!/i);
  });

  it("opens the cited line in the transcript and highlights it", async () => {
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("button", { name: "Show line 3 in the transcript" }));
    expect(screen.getByRole("tab", { name: "Transcript" })).toHaveAttribute("aria-selected", "true");
    const rows = within(screen.getByRole("table", { name: "Transcript" })).getAllByRole("row");
    expect(rows[2]).toHaveAttribute("aria-current", "true");
    expect(rows[2].textContent).toContain("We could split the order");
    expect(rows.filter((r) => r.getAttribute("aria-current") === "true")).toHaveLength(1);
  });

  describe("tabs", () => {
    afterEach(() => {
      delete (HTMLElement.prototype as Partial<HTMLElement>).scrollIntoView;
    });

    /** Records what the view scrolls to, since jsdom has no layout. */
    function watchScrolling() {
      const scrolled = vi.fn<(el: Element, options: unknown) => void>();
      HTMLElement.prototype.scrollIntoView = function (options) {
        scrolled(this, options);
      };
      return scrolled;
    }

    it("stay pinned with the meeting's header while the page scrolls", async () => {
      await setup();
      const tabs = await screen.findByRole("tablist");
      let pinned: HTMLElement | null = tabs;
      while (pinned !== null && getComputedStyle(pinned).position !== "sticky") {
        pinned = pinned.parentElement;
      }
      expect(pinned, "the tabs have a sticky ancestor").not.toBeNull();
      expect(pinned).toContainElement(screen.getByRole("heading", { level: 1, name: ITEM.title }));
      // Negative control: the content below the tabs scrolls.
      expect(pinned).not.toContainElement(panel());
    });

    it("open the summary at its top, however far down the transcript was", async () => {
      const scrolled = watchScrolling();
      const { user } = await setup();
      const view = screen.getByRole("region", { name: "Meeting detail" });
      await user.click(await screen.findByRole("tab", { name: "Transcript" }));
      scrolled.mockClear();
      await user.click(screen.getByRole("tab", { name: "Summary" }));
      expect(scrolled).toHaveBeenCalledWith(view, { block: "start" });
    });

    it("open the transcript at its top when no citation asks for a row", async () => {
      const scrolled = watchScrolling();
      const { user } = await setup();
      const view = screen.getByRole("region", { name: "Meeting detail" });
      const tab = await screen.findByRole("tab", { name: "Transcript" });
      scrolled.mockClear();
      await user.click(tab);
      expect(scrolled).toHaveBeenCalledWith(view, { block: "start" });
    });

    it("still take a citation to its row in the transcript", async () => {
      const scrolled = watchScrolling();
      const { user } = await setup();
      await screen.findByText(/TL;DR/);
      const view = screen.getByRole("region", { name: "Meeting detail" });
      scrolled.mockClear();
      await user.click(screen.getByRole("button", { name: "Show line 3 in the transcript" }));
      expect(screen.getByRole("tab", { name: "Transcript" })).toHaveAttribute("aria-selected", "true");
      expect(scrolled).toHaveBeenCalledWith(expect.anything(), { block: "center" });
      expect(scrolled).not.toHaveBeenCalledWith(view, { block: "start" });
    });
  });

  it("shows a citation of a replaced transcript as unavailable with the stale banner", async () => {
    const base = sampleDetail(ITEM);
    const summary = base.summary;
    if (summary === null) throw new Error("sample has a summary");
    const detail = {
      ...base,
      summary_stale: true,
      summary: {
        ...summary,
        decisions: [
          { text: "Keep the old supplier", citations: [{ segment_id: MISSING_SEGMENT, row: null }] },
        ],
      },
    };
    const { user, calls } = await setup({ details: { [ITEM.id]: detail } });
    expect(await screen.findByText("[unavailable]")).toBeInTheDocument();
    expect(screen.getByText(/refers to lines of an earlier transcript/)).toBeInTheDocument();
    // The header's button, then the stale prompt's own, which stays.
    const [header, prompt] = screen.getAllByRole("button", { name: "Re-summarise" });
    expect(screen.getByText(/refers to lines of an earlier transcript/).closest(".fui-MessageBar")).toContainElement(prompt);
    expect(header).not.toBe(prompt);
    await user.click(prompt);
    expect(calls).toContainEqual(["resummarise", ITEM.id]);
  });

  describe("editing the title and the transcript (T12.16)", () => {
    const LINE_1 = "So the lead time on the second pump set is now looking like eleven weeks.";

    it("renames the meeting with Enter and keeps the old title on Escape", async () => {
      const { user, calls } = await setup();
      await user.click(await screen.findByRole("button", { name: "Rename" }));
      const field = screen.getByRole("textbox", { name: "Meeting title" });
      expect(field).toHaveValue(ITEM.title);
      expect(field).toHaveFocus();
      await user.clear(field);
      await user.type(field, "Pump order follow-up{Escape}");
      expect(screen.queryByRole("textbox", { name: "Meeting title" })).not.toBeInTheDocument();
      expect(screen.getByRole("heading", { level: 1, name: ITEM.title })).toBeInTheDocument();
      expect(calls.some((c) => c[0] === "renameMeeting")).toBe(false);

      await user.click(screen.getByRole("button", { name: "Rename" }));
      const again = screen.getByRole("textbox", { name: "Meeting title" });
      await user.clear(again);
      await user.type(again, "  Pump order follow-up {Enter}");
      expect(await screen.findByRole("heading", { level: 1, name: "Pump order follow-up" })).toBeInTheDocument();
      expect(calls).toContainEqual(["renameMeeting", ITEM.id, "  Pump order follow-up "]);
      expect(screen.queryByRole("textbox", { name: "Meeting title" })).not.toBeInTheDocument();
    });

    it("keeps the title editor open with the refusal when the title is refused", async () => {
      const { user } = await setup();
      await user.click(await screen.findByRole("button", { name: "Rename" }));
      const field = screen.getByRole("textbox", { name: "Meeting title" });
      await user.clear(field);
      await user.type(field, "x".repeat(201));
      await user.click(screen.getByRole("button", { name: "Save" }));
      expect(await screen.findByText("Titles can be up to 200 characters long.")).toBeInTheDocument();
      expect(screen.getByText("The title was not saved")).toBeInTheDocument();
      expect(screen.getByRole("textbox", { name: "Meeting title" })).toBeInTheDocument();
    });

    it("corrects a line with Save, marks it edited and the summary out of date", async () => {
      const { user, calls } = await setup();
      await user.click(await screen.findByRole("tab", { name: "Transcript" }));
      expect(screen.queryByText(/refers to lines of an earlier transcript/)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Edit line 1" }));
      const field = screen.getByRole("textbox", { name: "Line 1" });
      expect(field).toHaveValue(LINE_1);
      expect(field).toHaveFocus();
      await user.clear(field);
      await user.type(field, "So the lead time on the second pump set is now about eleven weeks.");
      await user.click(screen.getByRole("button", { name: "Save" }));

      // The words cell (each word of a final line is its own element since T12.15f).
      const row = (
        await screen.findByText(
          (_, el) => el?.getAttribute("role") === "cell" && /now about eleven weeks/.test(el.textContent ?? ""),
        )
      ).closest('[role="row"]');
      if (!(row instanceof HTMLElement)) throw new Error("no row");
      expect(within(row).getByText("(edited)")).toBeInTheDocument();
      expect(screen.queryByRole("textbox", { name: "Line 1" })).not.toBeInTheDocument();
      expect(calls).toContainEqual([
        "editSegment",
        ITEM.id,
        sampleDetail(ITEM).transcript[0].segment_id,
        "So the lead time on the second pump set is now about eleven weeks.",
      ]);
      // Nothing is summarised by itself: the summary says it is out of date.
      expect(calls.some((c) => c[0] === "resummarise")).toBe(false);
      await user.click(screen.getByRole("tab", { name: "Summary" }));
      expect(screen.getByText(/refers to lines of an earlier transcript/)).toBeInTheDocument();
    });

    it("cancels a line with Escape and keeps it open on a refusal", async () => {
      const { user, calls } = await setup();
      await user.click(await screen.findByRole("tab", { name: "Transcript" }));
      await user.click(screen.getByRole("button", { name: "Edit line 2" }));
      await user.type(screen.getByRole("textbox", { name: "Line 2" }), " Really.{Escape}");
      expect(screen.queryByRole("textbox", { name: "Line 2" })).not.toBeInTheDocument();
      expect(calls.some((c) => c[0] === "editSegment")).toBe(false);

      await user.click(screen.getByRole("button", { name: "Edit line 2" }));
      await user.clear(screen.getByRole("textbox", { name: "Line 2" }));
      await user.keyboard("{Enter}");
      expect(await screen.findByText(/A line cannot be left empty/)).toBeInTheDocument();
      expect(screen.getByText("The line was not saved")).toBeInTheDocument();
      expect(screen.getByRole("textbox", { name: "Line 2" })).toBeInTheDocument();
    });

    it("corrects a decision and an action in the summary, and undo brings the old text back", async () => {
      const { user, calls } = await setup();
      await screen.findByText(/TL;DR/);
      expect(screen.queryByRole("button", { name: "Switch to previous summary" })).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Edit decision 1" }));
      const field = screen.getByRole("textbox", { name: "decision 1" });
      expect(field).toHaveValue("Split the pump order into two shipments");
      await user.clear(field);
      await user.type(field, "Split the pump order into two deliveries{Enter}");
      expect(await screen.findByText("Split the pump order into two deliveries")).toBeInTheDocument();
      const summaryId = sampleDetail(ITEM).summary?.id;
      expect(calls).toContainEqual([
        "editSummary",
        ITEM.id,
        summaryId,
        { kind: "Decision", index: 0 },
        "Split the pump order into two deliveries",
      ]);

      await user.click(screen.getByRole("button", { name: "Edit when action 1 is due" }));
      const due = screen.getByRole("textbox", { name: "when action 1 is due" });
      await user.clear(due);
      await user.click(screen.getByRole("button", { name: "Save" }));
      await waitFor(() => expect(screen.queryByText(/\(by Friday\)/)).not.toBeInTheDocument());
      expect(calls).toContainEqual(["editSummary", ITEM.id, summaryId, { kind: "ActionDue", index: 0 }, ""]);
      // A summary edit sends nothing to a summary writer.
      expect(calls.some((c) => c[0] === "resummarise")).toBe(false);

      await user.click(screen.getByRole("button", { name: "Switch to previous summary" }));
      expect(await screen.findByText(/\(by Friday\)/)).toBeInTheDocument();
      expect(screen.getByText("Split the pump order into two deliveries")).toBeInTheDocument();
    });

    it("cancels a summary edit with Escape and keeps it open on a refusal", async () => {
      const { user, calls } = await setup();
      await screen.findByText(/TL;DR/);
      await user.click(screen.getByRole("button", { name: "Edit TL;DR" }));
      await user.type(screen.getByRole("textbox", { name: "TL;DR" }), " More.{Escape}");
      expect(screen.queryByRole("textbox", { name: "TL;DR" })).not.toBeInTheDocument();
      expect(calls.some((c) => c[0] === "editSummary")).toBe(false);

      await user.click(screen.getByRole("button", { name: "Edit TL;DR" }));
      await user.clear(screen.getByRole("textbox", { name: "TL;DR" }));
      await user.keyboard("{Enter}");
      expect(await screen.findByText("Enter some words. Only a due date can be left empty.")).toBeInTheDocument();
      expect(screen.getByText("The summary was not changed")).toBeInTheDocument();
      expect(screen.getByRole("textbox", { name: "TL;DR" })).toBeInTheDocument();
    });

    it("offers no summary edit while a new summary is being written", async () => {
      const { user } = await setup();
      await screen.findByText(/TL;DR/);
      expect(screen.getByRole("button", { name: "Edit TL;DR" })).toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Re-summarise" }));
      await screen.findByText("Writing a new summary");
      expect(screen.queryByRole("button", { name: /^Edit / })).not.toBeInTheDocument();
    });

    it("offers no line edit on a draft transcript", async () => {
      const base = sampleDetail(ITEM);
      const draft = { ...base, header: { ...base.header, transcript: "Draft" as const } };
      const { user } = await setup({ details: { [ITEM.id]: draft } });
      await user.click(await screen.findByRole("tab", { name: "Transcript" }));
      expect(screen.getByText(LINE_1)).toBeInTheDocument();
      expect(screen.queryByRole("button", { name: /^Edit line/ })).not.toBeInTheDocument();
    });
  });

  describe("Copy summary", () => {
    /** A `navigator.clipboard` whose `writeText` records its text, or does what `write` says.
     * Installed after `setup`, which brings user-event's own clipboard stub. */
    function stubClipboard(write: (text: string) => Promise<void> = async () => {}) {
      const written: string[] = [];
      Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: {
          writeText: (text: string) => {
            written.push(text);
            return write(text);
          },
        },
      });
      return written;
    }

    const copyButton = () => screen.queryByRole("button", { name: "Copy summary" });

    it("copies the summary shown, says so for a while, then goes back", async () => {
      const { user, timer } = await setup();
      await screen.findByText(/TL;DR/);
      const written = stubClipboard();
      const summary = sampleDetail(ITEM).summary;
      if (summary === null) throw new Error("sample has a summary");

      await user.click(screen.getByRole("button", { name: "Copy summary" }));
      expect(await screen.findByRole("button", { name: "Copied" })).toBeInTheDocument();
      expect(written).toEqual([summaryMarkdown(summary)]);

      act(() => timer.advance(COPIED_MS - 1));
      expect(screen.getByRole("button", { name: "Copied" })).toBeInTheDocument();
      act(() => timer.advance(1));
      expect(screen.getByRole("button", { name: "Copy summary" })).toBeInTheDocument();
    });

    it("keeps saying so for the full time after a second copy", async () => {
      const { user, timer } = await setup();
      await screen.findByText(/TL;DR/);
      const written = stubClipboard();
      await user.click(screen.getByRole("button", { name: "Copy summary" }));
      await screen.findByRole("button", { name: "Copied" });
      act(() => timer.advance(COPIED_MS - 1));

      await user.click(screen.getByRole("button", { name: "Copied" }));
      await waitFor(() => expect(written).toHaveLength(2));
      act(() => timer.advance(COPIED_MS - 1));
      expect(await screen.findByRole("button", { name: "Copied" })).toBeInTheDocument();
      act(() => timer.advance(1));
      expect(screen.getByRole("button", { name: "Copy summary" })).toBeInTheDocument();
    });

    it("shows a fixed message when the copy fails, never the raw error, and a later copy clears it", async () => {
      const { user } = await setup();
      await screen.findByText(/TL;DR/);
      let refuse = true;
      stubClipboard(async () => {
        if (refuse) throw new Error("raw clipboard failure");
      });

      await user.click(screen.getByRole("button", { name: "Copy summary" }));
      expect(await screen.findByText("The summary could not be copied.")).toBeInTheDocument();
      expect(screen.queryByText(/raw clipboard failure/)).toBeNull();
      expect(screen.getByRole("button", { name: "Copy summary" })).toBeInTheDocument();

      refuse = false;
      await user.click(screen.getByRole("button", { name: "Copy summary" }));
      await screen.findByRole("button", { name: "Copied" });
      expect(screen.queryByText("The summary could not be copied.")).toBeNull();
    });

    it("shows the same message when the webview has no clipboard", async () => {
      const { user } = await setup();
      await screen.findByText(/TL;DR/);
      Object.defineProperty(navigator, "clipboard", { configurable: true, value: undefined });
      await user.click(screen.getByRole("button", { name: "Copy summary" }));
      expect(await screen.findByText("The summary could not be copied.")).toBeInTheDocument();
    });

    it("has no button without a summary", async () => {
      await setup({ details: { [ITEM.id]: { ...sampleDetail(ITEM), summary: null } } });
      await screen.findByRole("button", { name: "Summarise" });
      expect(copyButton()).toBeNull();
    });

    it("has no button on a summary with nothing in it", async () => {
      await setup({
        details: {
          [ITEM.id]: {
            ...sampleDetail(ITEM),
            summary: {
              id: "5a000000-0000-7000-8000-000000000009",
              title: "",
              tldr: "",
              decisions: [],
              actions: [],
              notes: [],
              open_questions: [],
            },
          },
        },
      });
      await screen.findByText(/TL;DR/);
      expect(copyButton()).toBeNull();
    });

    it("has no button while a new summary is being written, and offers the new one after", async () => {
      const { user, timer } = await setup();
      await screen.findByText(/TL;DR/);
      await user.click(screen.getByRole("button", { name: "Re-summarise" }));
      expect(screen.getByText("Writing a new summary")).toBeInTheDocument();
      expect(copyButton()).toBeNull();

      act(() => timer.advance(POST_STEP_MS));
      await screen.findByText(/The client hears the new date by Friday/);
      const written = stubClipboard();
      await user.click(screen.getByRole("button", { name: "Copy summary" }));
      await screen.findByRole("button", { name: "Copied" });
      expect(written[0]).toContain("The client hears the new date by Friday");
    });

    it("keeps the button while the meeting is transcribed again: its summary stays on screen", async () => {
      const { user } = await setup();
      await screen.findByText(/TL;DR/);
      await user.click(screen.getByRole("button", { name: "Re-transcribe" }));
      expect(screen.getByRole("button", { name: "Copy summary" })).toBeInTheDocument();
    });

    it("keeps the button when a re-summary is refused: the old summary stays", async () => {
      const { user, ipc } = await setup();
      await screen.findByText(/TL;DR/);
      // A refused start: no job runs, the old summary stays.
      ipc.resummarise = async () => {
        throw new IpcFailure({ kind: "busy", message: "busy", retryable: true });
      };
      await user.click(screen.getByRole("button", { name: "Re-summarise" }));
      await screen.findByText(/already being re-transcribed or re-summarised/);
      expect(screen.getByRole("button", { name: "Copy summary" })).toBeInTheDocument();
    });
  });

  it("offers This is me only in the room, and it and its undo follow through the transcript", async () => {
    const { user, calls } = await setup({ inRoom: true });
    await screen.findByRole("textbox", { name: "Name of Speaker 1" });
    expect(within(panel()).getAllByRole("button", { name: "This is me" })).toHaveLength(4);
    expect(within(panel()).queryByRole("button", { name: "This isn't me" })).toBeNull();

    await user.click(within(speakerRow("Speaker 2")).getByRole("button", { name: "This is me" }));
    expect(calls).toContainEqual(["markMe", ITEM.id, SAMPLE_SPEAKERS.priya]);
    // One Me row: the offer leaves every voice, and the user's name shows among the named.
    await waitFor(() => expect(within(panel()).queryByRole("listitem", { name: "Speaker 2" })).toBeNull());
    await openNamed(user);
    const mine = await within(panel()).findByRole("listitem", { name: "Casey" });
    expect(within(mine).getByRole("button", { name: "This isn't me" })).toBeInTheDocument();
    expect(within(panel()).queryByRole("button", { name: "This is me" })).toBeNull();
    expect(within(mine).queryByRole("textbox")).toBeNull();
    // Nothing saves a voice and no voiceprint call is made.
    expect(calls.some((c) => c[0] === "saveVoice")).toBe(false);
    await user.click(screen.getByRole("tab", { name: "Transcript" }));
    const table = screen.getByRole("table", { name: "Transcript" });
    expect(within(table).getAllByText("Casey").length).toBeGreaterThan(0);

    await user.click(within(mine).getByRole("button", { name: "This isn't me" }));
    expect(calls).toContainEqual(["unmarkMe", ITEM.id, SAMPLE_SPEAKERS.priya]);
    const back = await within(panel()).findByRole("listitem", { name: "Speaker 2" });
    expect(within(back).getByRole("textbox", { name: "Name of Speaker 2" })).toBeInTheDocument();
    expect(within(panel()).getAllByRole("button", { name: "This is me" })).toHaveLength(4);
    expect(within(table).queryByText("Casey")).toBeNull();
  });

  it("offers neither on a call, and the mock refuses like the runtime", async () => {
    const { ipc } = await setup();
    await screen.findByRole("textbox", { name: "Name of Speaker 2" });
    expect(within(panel()).queryByRole("button", { name: "This is me" })).toBeNull();
    expect(within(panel()).queryByRole("button", { name: "This isn't me" })).toBeNull();
    await expect(ipc.markMe(ITEM.id, SAMPLE_SPEAKERS.second)).rejects.toMatchObject({
      error: { kind: "not_in_room" },
    });
    await expect(ipc.unmarkMe(ITEM.id, SAMPLE_SPEAKERS.me)).rejects.toMatchObject({
      error: { kind: "not_in_room" },
    });
  });

  it("refuses a second voice in the mock with the runtime's copy", async () => {
    const { ipc } = await setup({ inRoom: true });
    await ipc.markMe(ITEM.id, SAMPLE_SPEAKERS.me);
    await expect(ipc.markMe(ITEM.id, SAMPLE_SPEAKERS.second)).rejects.toMatchObject({
      error: {
        kind: "meeting_has_me",
        message: 'One voice in this meeting is already you. Choose "This isn\'t me" on it first.',
      },
    });
    await expect(ipc.unmarkMe(ITEM.id, SAMPLE_SPEAKERS.second)).rejects.toMatchObject({
      error: { kind: "not_me" },
    });
  });

  it("saves a voice from its name field: named, linked and remembered in one step, and the transcript follows", async () => {
    const { user, calls } = await setup();
    const field = await screen.findByRole("textbox", { name: "Name of Speaker 2" });
    await user.type(field, "Sam Lee{Enter}");
    await waitFor(() => expect(within(panel()).queryByRole("textbox", { name: "Name of Speaker 2" })).toBeNull());
    expect(calls).toContainEqual(["saveVoice", ITEM.id, SAMPLE_SPEAKERS.second, { kind: "New", name: "Sam Lee" }]);
    expect(calls.filter((c) => c[0] === "saveVoice")).toHaveLength(1);
    expect(calls.some((c) => c[0] === "linkSpeaker")).toBe(false);
    expect(await screen.findByText("Saved Sam Lee. Later meetings will recognise this voice.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Speakers, 1 needs a name" })).toBeInTheDocument();
    // It moved to the named voices, where its voice can be forgotten.
    await openNamed(user);
    expect(within(speakerRow("Sam Lee")).getByRole("button", { name: "Forget voice" })).toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Transcript" }));
    const rows = within(screen.getByRole("table", { name: "Transcript" })).getAllByRole("row");
    expect(within(rows[0]).getAllByRole("cell")[1].textContent).toBe("Sam Lee");
  });

  it("renames a named speaker on Enter", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await openNamed(user);
    await user.click(within(speakerRow("Priya Shah")).getByRole("button", { name: "Rename here" }));
    expect(within(panel()).getByText("Renames this speaker in this meeting only.")).toBeInTheDocument();
    const field = within(panel()).getByRole("textbox", { name: "Name of Priya Shah in this meeting" });
    await user.clear(field);
    await user.type(field, "Priya S{Enter}");
    expect(await within(panel()).findByText("Priya S")).toBeInTheDocument();
    expect(calls).toContainEqual(["renameSpeaker", ITEM.id, SAMPLE_SPEAKERS.priya, "Priya S"]);
  });

  it("merges a speaker away from the panel", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(within(speakerRow("Speaker 3")).getByRole("button", { name: "Merge" }));
    await user.click(await screen.findByRole("menuitem", { name: "Into Priya Shah" }));
    await waitFor(() => expect(within(panel()).queryByRole("listitem", { name: "Speaker 3" })).toBeNull());
    expect(calls).toContainEqual(["mergeSpeakers", ITEM.id, SAMPLE_SPEAKERS.priya, SAMPLE_SPEAKERS.third]);
    await openNamed(user);
    expect(within(speakerRow("Priya Shah")).getByText(/29% of talk time/)).toBeInTheDocument();
    // The user's own row cannot be merged away.
    expect(within(speakerRow("Me")).getByRole("button", { name: "Merge" })).toHaveAttribute("aria-disabled", "true");
  });

  it("links a wrong match to a new person from the named voices", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await openNamed(user);
    await user.click(within(speakerRow("Priya Shah")).getByRole("button", { name: "Link to person" }));
    const dialog = await screen.findByRole("dialog", { name: "Link Priya Shah to a person" });
    await user.click(within(dialog).getByRole("combobox"));
    await user.click(await screen.findByRole("option", { name: "New person" }));
    await user.type(within(dialog).getByRole("textbox", { name: "New person's name" }), "Ana Diaz");
    await user.click(within(dialog).getByRole("button", { name: "Link" }));
    await waitFor(() => expect(within(panel()).getByRole("listitem", { name: "Ana Diaz" })).toBeInTheDocument());
    expect(calls).toContainEqual(["linkSpeaker", ITEM.id, SAMPLE_SPEAKERS.priya, { kind: "New", name: "Ana Diaz" }]);
    // Linking is not saving: no voice is remembered by it.
    expect(calls.some((c) => c[0] === "saveVoice")).toBe(false);
  });

  it("saves with the Save button, to an existing person by exact name, and never an empty name", async () => {
    const { user, calls } = await setup();
    const save = await screen.findByRole("button", { name: "Save Speaker 3" });
    expect(save).toBeDisabled();
    await user.type(within(panel()).getByRole("textbox", { name: "Name of Speaker 3" }), "  ");
    expect(save).toBeDisabled();
    await user.type(within(panel()).getByRole("textbox", { name: "Name of Speaker 3" }), "tom reid");
    await user.click(save);
    await waitFor(() =>
      expect(calls).toContainEqual([
        "saveVoice",
        ITEM.id,
        SAMPLE_SPEAKERS.third,
        { kind: "Existing", person: "9e000000-0000-7000-8000-000000000003" },
      ]),
    );
    expect(await screen.findByText("Saved Tom Reid. Later meetings will recognise this voice.")).toBeInTheDocument();
  });

  it("ignores a voice: it stops needing a name, keeps its label and can be named after all", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(within(speakerRow("Speaker 3")).getByRole("button", { name: "Ignore" }));
    expect(calls).toContainEqual(["ignoreSpeaker", ITEM.id, SAMPLE_SPEAKERS.third]);
    expect(await screen.findByRole("button", { name: "Speakers, 1 needs a name" })).toBeInTheDocument();
    const needing = within(panel()).getByRole("region", { name: "Need a name" });
    expect(within(needing).queryByRole("listitem", { name: "Speaker 3" })).toBeNull();
    // Nothing is saved or linked by ignoring.
    expect(calls.some((c) => c[0] === "saveVoice" || c[0] === "linkSpeaker")).toBe(false);
    // The label stays in the summary and the transcript.
    expect(within(screen.getByRole("table", { name: "Actions" })).getByText("Speaker 3")).toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Transcript" }));
    const table = screen.getByRole("table", { name: "Transcript" });
    expect(within(table).getAllByText("Speaker 3").length).toBeGreaterThan(0);

    await user.click(within(panel()).getByRole("button", { name: "Ignored (1)" }));
    await user.click(within(speakerRow("Speaker 3")).getByRole("button", { name: "Stop ignoring" }));
    expect(calls).toContainEqual(["unignoreSpeaker", ITEM.id, SAMPLE_SPEAKERS.third]);
    expect(await screen.findByRole("button", { name: "Speakers, 2 need names" })).toBeInTheDocument();
    expect(within(panel()).queryByRole("button", { name: /^Ignored/ })).toBeNull();
    expect(within(panel()).getByRole("textbox", { name: "Name of Speaker 3" })).toBeInTheDocument();
  });

  it("offers no Ignore for a voice with nothing stored, and the mock refuses like the runtime", async () => {
    const base = sampleDetail(ITEM);
    const detail = {
      ...base,
      speakers: base.speakers.map((s) => (s.id === SAMPLE_SPEAKERS.third ? { ...s, can_ignore: false } : s)),
    };
    const { ipc } = await setup({ details: { [ITEM.id]: detail } });
    await screen.findByText(/TL;DR/);
    expect(within(speakerRow("Speaker 3")).queryByRole("button", { name: "Ignore" })).toBeNull();
    expect(within(speakerRow("Speaker 2")).getByRole("button", { name: "Ignore" })).toBeInTheDocument();
    await expect(ipc.ignoreSpeaker(ITEM.id, SAMPLE_SPEAKERS.third)).rejects.toMatchObject({
      error: { kind: "no_stored_voice" },
    });
    await expect(ipc.ignoreSpeaker(ITEM.id, SAMPLE_SPEAKERS.priya)).rejects.toMatchObject({
      error: { kind: "not_unnamed", message: "Only a voice that needs a name can be ignored." },
    });
    await expect(ipc.saveVoice(ITEM.id, SAMPLE_SPEAKERS.me, { kind: "New", name: "Sky Moss" })).rejects.toMatchObject({
      error: { kind: "me_is_the_user" },
    });
    expect((await ipc.listPersons()).some((p) => p.name === "Sky Moss")).toBe(false);
  });

  it("stays closed when no voice needs a name, with recognised voices marked, and the header button toggles it", async () => {
    const base = sampleDetail(ITEM);
    const detail = {
      ...base,
      speakers: base.speakers.map((s) =>
        s.needs_name
          ? {
              ...s,
              name: "Tom Reid",
              person: { id: "9e000000-0000-7000-8000-000000000003", name: "Tom Reid", has_voiceprint: true },
              source: "Voiceprint" as const,
              needs_name: false,
              can_ignore: false,
            }
          : s,
      ),
    };
    const { user } = await setup({ details: { [ITEM.id]: detail } });
    await screen.findByText(/TL;DR/);
    const toggle = screen.getByRole("button", { name: "Speakers" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("complementary", { name: "Speakers" })).toBeNull();

    await user.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(within(panel()).getByText("Every voice in this meeting has a name.")).toBeInTheDocument();
    await openNamed(user);
    const recognisedRows = within(panel())
      .getAllByRole("listitem")
      .filter((r) => within(r).queryByText("Recognised") !== null);
    expect(recognisedRows.map((r) => r.getAttribute("aria-label"))).toEqual(["Priya Shah", "Tom Reid", "Tom Reid"]);
    expect(within(speakerRow("Me")).queryByText("Recognised")).toBeNull();

    await user.click(within(panel()).getByRole("button", { name: "Close Speakers" }));
    expect(screen.queryByRole("complementary", { name: "Speakers" })).toBeNull();
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    await user.click(toggle);
    expect(panel()).toBeInTheDocument();
    await user.click(toggle);
    expect(screen.queryByRole("complementary", { name: "Speakers" })).toBeNull();
  });

  it("opens by itself when a reload brings more voices that need a name, and never closes by itself", async () => {
    const base = sampleDetail(ITEM);
    const allNamed = {
      ...base,
      speakers: base.speakers.map((s) =>
        s.needs_name
          ? {
              ...s,
              name: "Tom Reid",
              person: { id: "9e000000-0000-7000-8000-000000000003", name: "Tom Reid", has_voiceprint: true },
              source: "Voiceprint" as const,
              needs_name: false,
              can_ignore: false,
            }
          : s,
      ),
    };
    const { user, ipc, timer } = await setup({ details: { [ITEM.id]: allNamed } });
    await screen.findByText(/TL;DR/);
    const closed = () => screen.queryByRole("complementary", { name: "Speakers" });
    expect(closed()).toBeNull();
    // A reload that brings no new voice to name leaves it closed.
    await user.click(screen.getByRole("button", { name: "Rename" }));
    await user.type(screen.getByRole("textbox", { name: "Meeting title" }), " again{Enter}");
    await screen.findByRole("heading", { level: 1, name: `${ITEM.title} again` });
    expect(closed()).toBeNull();

    // A re-transcription ends with two voices that need names: the panel opens by itself.
    ipc.getMeeting = async () => base;
    await user.click(screen.getByRole("button", { name: "Re-transcribe" }));
    for (let i = 0; i < 12; i++) act(() => timer.advance(POST_STEP_MS));
    expect(await screen.findByRole("complementary", { name: "Speakers" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Speakers, 2 need names" })).toHaveAttribute("aria-expanded", "true");

    // Closed by the user, it stays closed on a reload with no more voices to name than before.
    await user.click(within(panel()).getByRole("button", { name: "Close Speakers" }));
    await user.click(screen.getByRole("button", { name: "Rename" }));
    await user.type(screen.getByRole("textbox", { name: "Meeting title" }), "{Enter}");
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "Meeting title" })).toBeNull());
    expect(closed()).toBeNull();
  });

  it("returns focus to the Speakers button when the panel closes, and points at the panel only while it exists", async () => {
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    const toggle = screen.getByRole("button", { name: "Speakers, 2 need names" });
    expect(toggle.getAttribute("aria-controls")).toBe(panel().id);
    within(panel()).getByRole("button", { name: "Close Speakers" }).focus();
    await user.keyboard("{Enter}");
    expect(screen.queryByRole("complementary", { name: "Speakers" })).toBeNull();
    expect(toggle).toHaveFocus();
    expect(toggle).not.toHaveAttribute("aria-controls");
    await user.keyboard("{Enter}");
    expect(toggle.getAttribute("aria-controls")).toBe(panel().id);
  });

  it("puts the panel beside the summary and transcript, pushing them aside rather than covering them", async () => {
    await setup();
    await screen.findByText(/TL;DR/);
    const aside = panel();
    const main = screen.getByText(/TL;DR/).closest("div");
    if (main === null) throw new Error("no summary");
    // Both live in one grid row: the panel's column is beside the content's, in normal flow.
    let column: HTMLElement | null = aside;
    while (column !== null && column.parentElement !== null && !column.parentElement.contains(main)) {
      column = column.parentElement;
    }
    if (column === null || column.parentElement === null) throw new Error("no shared container");
    expect(getComputedStyle(column.parentElement).display).toBe("grid");
    for (let el: HTMLElement | null = aside; el !== null && el !== column.parentElement; el = el.parentElement) {
      expect(["absolute", "fixed"]).not.toContain(getComputedStyle(el).position);
    }
  });

  it("offers the previous summary only after a re-summary", async () => {
    const { user, timer } = await setup();
    await screen.findByText(/TL;DR/);
    expect(screen.queryByRole("button", { name: "Switch to previous summary" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Re-summarise" }));
    expect(screen.getByText("Writing a new summary")).toBeInTheDocument();
    act(() => timer.advance(POST_STEP_MS));
    const undo = await screen.findByRole("button", { name: "Switch to previous summary" });
    expect(screen.getByText(/The client hears the new date by Friday/)).toBeInTheDocument();
    await user.click(undo);
    await waitFor(() => expect(screen.queryByText(/The client hears the new date by Friday/)).toBeNull());
  });

  it("asks before deleting and names the meeting", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await user.pointer({ keys: "[MouseRight]", target: listRow(ITEM) });
    await user.click(await screen.findByRole("menuitem", { name: "Delete meeting..." }));
    const dialog = await accessibleDialog();
    expect(within(dialog).getByText(`Delete "${ITEM.title}"?`)).toBeInTheDocument();
    // Opened from the menu, the dialog must own focus on its first button: the menu hands focus
    // back to its trigger on close, and a modal whose own surface takes focus is never made
    // active by tabster, which aria-hides it once its hide timer runs (T7.3d, the Surface flake).
    expect(within(dialog).getByRole("button", { name: "Delete meeting" })).toHaveFocus();
    expect(listRow(ITEM)).not.toHaveFocus();
    await act(() => new Promise((r) => setTimeout(r, TABSTER_HIDE_PASS_MS)));
    expect(screen.getByRole("alertdialog")).toBe(dialog);
    // The modal is the active one: the page behind it is hidden, not the dialog.
    expect(screen.queryByRole("button", { name: trashName(ITEM) })).toBeNull();
    expect(calls.some((c) => c[0] === "deleteMeeting")).toBe(false);
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(calls.some((c) => c[0] === "deleteMeeting")).toBe(false);
    // The open modal aria-hid the page; tabster un-hides it on its deferred pass after close.
    await user.click(await screen.findByRole("button", { name: trashName(ITEM) }));
    const second = await accessibleDialog();
    // As a person does: the modal's hide pass has run (the page is aria-hidden) before Delete.
    // Deleting used to unmount the whole meeting view with its dialog; if a delete ever leaves no
    // tabster user, tabster is disposed without restoring the page, which then stays aria-hidden
    // for good.
    await act(() => new Promise((r) => setTimeout(r, TABSTER_HIDE_PASS_MS)));
    expect(screen.queryByRole("button", { name: trashName(ITEM) })).toBeNull();
    await user.click(within(second).getByRole("button", { name: "Delete meeting" }));
    await waitFor(() =>
      expect(screen.getByRole("heading", { level: 1 }).textContent).toBe(OTHER.title),
    );
    expect(screen.queryByRole("button", { name: new RegExp(`^${ITEM.title}`) })).toBeNull();
    expect(calls).toContainEqual(["deleteMeeting", ITEM.id]);
  });

  it("moves keyboard focus to the meeting now shown after a delete, or to New meeting", async () => {
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("button", { name: trashName(ITEM) }));
    await user.click(within(await accessibleDialog()).getByRole("button", { name: "Delete meeting" }));
    await waitFor(() => expect(listRow(OTHER)).toHaveFocus());
    // Past the closed modal's deferred passes, focus has stayed.
    await act(() => new Promise((r) => setTimeout(r, TABSTER_HIDE_PASS_MS)));
    expect(listRow(OTHER)).toHaveFocus();
    await user.click(screen.getByRole("button", { name: trashName(OTHER) }));
    await user.click(within(await accessibleDialog()).getByRole("button", { name: "Delete meeting" }));
    // The empty state has a New meeting button too: the list's own is the one that stays.
    await waitFor(() =>
      expect(document.activeElement).toBe(
        within(screen.getByRole("navigation", { hidden: true })).getByRole("button", { name: "New meeting", hidden: true }),
      ),
    );
    expect(await screen.findByRole("heading", { level: 1, name: "Record your first meeting" })).toBeInTheDocument();
  });

  it("deletes once on a double click, with the dialog's buttons held and no refusal shown", async () => {
    const { user, calls, ipc } = await setup();
    await screen.findByText(/TL;DR/);
    let finish = () => {};
    const real = ipc.deleteMeeting;
    let held = 0;
    ipc.deleteMeeting = async (meeting) => {
      held += 1;
      await new Promise<void>((resolve) => {
        finish = resolve;
      });
      await real(meeting);
    };
    await user.click(screen.getByRole("button", { name: trashName(ITEM) }));
    const dialog = await accessibleDialog();
    const del = within(dialog).getByRole("button", { name: "Delete meeting" });
    // Two clicks in one tick: the second comes before the buttons have re-rendered.
    act(() => {
      del.click();
      del.click();
    });
    expect(held).toBe(1);
    expect(del).toHaveAttribute("aria-disabled", "true");
    expect(within(dialog).getByRole("button", { name: "Cancel" })).toHaveAttribute("aria-disabled", "true");
    // Neither Cancel nor Escape closes a delete already on its way, and focus stays in the dialog.
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await user.keyboard("{Escape}");
    expect(screen.getByRole("alertdialog")).toBe(dialog);
    expect(dialog).toContainElement(document.activeElement as HTMLElement);
    await act(async () => finish());
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(calls.filter((c) => c[0] === "deleteMeeting")).toEqual([["deleteMeeting", ITEM.id]]);
    await screen.findByRole("heading", { level: 1, name: OTHER.title });
    expect(screen.queryByText("The meeting was not deleted")).toBeNull();
  });

  it("frees the dialog's buttons after a refusal, so the next delete can run", async () => {
    const { user, ipc } = await setup();
    await screen.findByText(/TL;DR/);
    const real = ipc.deleteMeeting;
    ipc.deleteMeeting = async () => {
      throw new IpcFailure({ kind: "meeting_busy", message: "The meeting is in use.", retryable: true });
    };
    await user.click(screen.getByRole("button", { name: trashName(ITEM) }));
    await user.click(within(await accessibleDialog()).getByRole("button", { name: "Delete meeting" }));
    expect(await screen.findByText("The meeting was not deleted")).toBeInTheDocument();
    ipc.deleteMeeting = real;
    await user.click(await screen.findByRole("button", { name: trashName(ITEM) }));
    const again = await accessibleDialog();
    expect(within(again).getByRole("button", { name: "Delete meeting" })).not.toHaveAttribute("aria-disabled", "true");
    await user.click(within(again).getByRole("button", { name: "Delete meeting" }));
    await screen.findByRole("heading", { level: 1, name: OTHER.title });
  });

  it("shows what a re-transcription's final pass warned of with its result", async () => {
    const warning = "Some remembered voices could not be read.";
    const { user, timer } = await setup({ retranscribeWarnings: [warning] });
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("button", { name: "Re-transcribe" }));
    for (let i = 0; i < 12; i++) act(() => timer.advance(POST_STEP_MS));
    expect(await screen.findByText(`The meeting was transcribed again. ${warning}`)).toBeInTheDocument();
  });

  it("deletes a meeting without opening it, and the one shown stays", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("button", { name: trashName(OTHER) }));
    const dialog = await accessibleDialog();
    expect(within(dialog).getByText(`Delete "${OTHER.title}"?`)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Delete meeting" }));
    await waitFor(() => expect(calls).toContainEqual(["deleteMeeting", OTHER.id]));
    await waitFor(() => expect(screen.queryByRole("button", { name: new RegExp(`^${OTHER.title}`) })).toBeNull());
    expect(screen.getByRole("heading", { level: 1, name: ITEM.title })).toBeInTheDocument();
  });

  it("opens the same delete question from the keyboard's context menu", async () => {
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    listRow(OTHER).focus();
    await user.keyboard("{Shift>}{F10}{/Shift}");
    // The menu's first item, "Move to folder" (T9.5c), takes focus; "Delete meeting..." is last.
    const item = await screen.findByRole("menuitem", { name: "Move to folder" });
    await waitFor(() => expect(item).toHaveFocus());
    expect(screen.getByRole("menuitem", { name: "Delete meeting..." })).toBeInTheDocument();
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("menuitem")).toBeNull());
    await waitFor(() => expect(listRow(OTHER)).toHaveFocus());
    // The Menu key does the same; the browser's own contextmenu event on the row opens it too.
    await user.keyboard("{ContextMenu}");
    const first = await screen.findByRole("menuitem", { name: "Move to folder" });
    await waitFor(() => expect(first).toHaveFocus());
    await user.keyboard("{ArrowDown}");
    await waitFor(() => expect(screen.getByRole("menuitem", { name: "Delete meeting..." })).toHaveFocus());
    await user.keyboard("{Enter}");
    const dialog = await accessibleDialog();
    expect(within(dialog).getByText(`Delete "${OTHER.title}"?`)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Delete meeting" })).toHaveFocus();
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(listRow(OTHER)).toHaveFocus());
    fireEvent.contextMenu(listRow(OTHER), { button: 0 });
    expect(await screen.findByRole("menuitem", { name: "Delete meeting..." })).toBeInTheDocument();
  });

  it("reaches the trash can by Tab from its row and returns there on Cancel", async () => {
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    listRow(ITEM).focus();
    await user.tab();
    const trash = screen.getByRole("button", { name: trashName(ITEM) });
    expect(trash).toHaveFocus();
    await user.keyboard("{Enter}");
    const dialog = await accessibleDialog();
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(trash).toHaveFocus());
  });

  it("says when a meeting being processed cannot be deleted, keeps it and leaves the page usable", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    // A job is running on the meeting: the app refuses, whichever way the delete started.
    await user.click(screen.getByRole("button", { name: "Re-summarise" }));
    expect(screen.getByText("Writing a new summary")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: trashName(ITEM) }));
    const dialog = await accessibleDialog();
    await user.click(within(dialog).getByRole("button", { name: "Delete meeting" }));
    expect(await screen.findByText("The meeting was not deleted")).toBeInTheDocument();
    expect(calls).toContainEqual(["deleteMeeting", ITEM.id]);
    // The closed modal's aria-hidden on the page lifts on tabster's deferred pass.
    expect(await screen.findByRole("button", { name: new RegExp(`^${ITEM.title}`) })).toBeInTheDocument();
    expect(screen.getByRole("heading", { level: 1, name: ITEM.title })).toBeInTheDocument();
    await user.click(screen.getAllByRole("button", { name: "Dismiss" })[0]);
    expect(screen.queryByText("The meeting was not deleted")).toBeNull();
  });

  it("puts the actions at the top, with no More menu", async () => {
    const { user, calls, timer } = await setup({
      details: { [ITEM.id]: { ...sampleDetail(ITEM), summary: null } },
    });
    const summarise = await screen.findByRole("button", { name: "Summarise" });
    expect(screen.queryByRole("button", { name: "More actions" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Re-summarise" })).toBeNull();
    expect(screen.getByRole("button", { name: "Export to Obsidian" })).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "Summarise" })).toHaveLength(1);
    await user.click(summarise);
    expect(calls).toContainEqual(["resummarise", ITEM.id]);
    act(() => timer.advance(POST_STEP_MS));
    await user.click(screen.getByRole("button", { name: "Re-transcribe" }));
    expect(calls).toContainEqual(["retranscribe", ITEM.id]);
  });

  it("explains that export needs a vault, and exports when there is one", async () => {
    const first = await setup();
    await screen.findByText(/TL;DR/);
    await first.user.click(screen.getByRole("button", { name: "Export to Obsidian" }));
    expect(await screen.findByText("Set your Obsidian vault in Settings to export.")).toBeInTheDocument();
  });

  it("reports an export to the vault", async () => {
    const { user } = await setup({ vault: "C:\\vault" });
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("button", { name: "Export to Obsidian" }));
    expect(await screen.findByText(/^Exported to Meetings\//)).toBeInTheDocument();
  });

  it("keeps time, speaker and text in separate transcript cells", async () => {
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("tab", { name: "Transcript" }));
    const rows = within(screen.getByRole("table", { name: "Transcript" })).getAllByRole("row");
    expect(rows).toHaveLength(8);
    for (const row of rows) {
      const cells = within(row).getAllByRole("cell");
      expect(cells).toHaveLength(3);
      expect(cells[2].textContent).not.toContain(cells[1].textContent ?? "");
    }
    const cells = within(rows[0]).getAllByRole("cell");
    expect(cells.map((c) => c.textContent)).toEqual([
      "12:04",
      "Speaker 2",
      "So the lead time on the second pump set is now looking like eleven weeks.",
    ]);
  });

  it("has no names banner: the Speakers button says how many voices need names", async () => {
    await setup();
    await screen.findByText(/TL;DR/);
    expect(screen.queryByRole("button", { name: "Name voices" })).toBeNull();
    expect(screen.queryByText(/voices need names\./)).toBeNull();
    expect(screen.getByRole("button", { name: "Speakers, 2 need names" })).toBeInTheDocument();
  });

  it("says when a sample's audio is no longer kept", async () => {
    const { user } = await setup({ sample: null });
    await screen.findByText(/TL;DR/);
    await openNamed(user);
    await user.click(within(speakerRow("Priya Shah")).getByRole("button", { name: "Play a sample of Priya Shah" }));
    expect(await within(speakerRow("Priya Shah")).findByText("Audio no longer kept")).toBeInTheDocument();
  });

  it("shows re-transcribe and samples as unavailable once the recordings are gone", async () => {
    const { user, calls } = await setup({
      details: { [ITEM.id]: { ...sampleDetail(ITEM), audio_kept: false } },
    });
    await screen.findByText(/TL;DR/);
    await openNamed(user);
    // The rest of the meeting reads as before.
    expect(within(panel()).getAllByText("Audio no longer kept").length).toBe(
      sampleDetail(ITEM).speakers.length,
    );
    const play = within(speakerRow("Priya Shah")).getByRole("button", { name: "Play a sample of Priya Shah" });
    expect(play).toBeDisabled();
    const item = screen.getByRole("button", { name: /Re-transcribe/ });
    expect(item).toHaveAttribute("aria-disabled", "true");
    expect(item).toHaveTextContent("Recording no longer kept");
    await user.click(item);
    expect(calls.some((c) => c[0] === "retranscribe")).toBe(false);
  });

  it("says no audio was saved for a meeting Earshot closed during, and offers no re-transcribe", async () => {
    const { user, calls } = await setup({
      meetings: [{ ...ITEM, state: "Interrupted", duration_ms: null }, OTHER],
      details: { [ITEM.id]: { ...sampleDetail(ITEM), audio_kept: false, state: "Interrupted" } },
    });
    await screen.findByText(/TL;DR/);
    const item = screen.getByRole("button", { name: /Re-transcribe/ });
    expect(item).toHaveAttribute("aria-disabled", "true");
    expect(item).toHaveTextContent("No usable audio from this recording");
    await user.click(item);
    expect(calls.some((c) => c[0] === "retranscribe")).toBe(false);
  });

  it("offers re-transcribe for a recovered meeting whose audio is kept", async () => {
    await setup({
      meetings: [{ ...ITEM, state: "Unprocessed" }, OTHER],
      details: { [ITEM.id]: { ...sampleDetail(ITEM), audio_kept: true, state: "Unprocessed" } },
    });
    await screen.findByText(/TL;DR/);
    expect(screen.getByRole("button", { name: /Re-transcribe/ })).not.toHaveAttribute("aria-disabled", "true");
  });

  it("offers re-transcribe and samples while the recordings are kept", async () => {
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    await openNamed(user);
    expect(within(panel()).queryByText("Audio no longer kept")).toBeNull();
    expect(
      within(speakerRow("Priya Shah")).getByRole("button", { name: "Play a sample of Priya Shah" }),
    ).toBeEnabled();
    const item = screen.getByRole("button", { name: /Re-transcribe/ });
    expect(item).not.toHaveAttribute("aria-disabled", "true");
    expect(item).not.toHaveTextContent("no longer kept");
  });

  it("asks before forgetting a voice", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await openNamed(user);
    await user.click(within(speakerRow("Priya Shah")).getByRole("button", { name: "Forget voice" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText("Forget Priya Shah's voice?")).toBeInTheDocument();
    expect(calls.some((c) => c[0] === "forgetPersonVoices")).toBe(false);
    await user.click(within(dialog).getByRole("button", { name: "Forget voice" }));
    await waitFor(() => expect(calls.some((c) => c[0] === "forgetPersonVoices")).toBe(true));
    await waitFor(() =>
      expect(within(speakerRow("Priya Shah")).queryByRole("button", { name: "Forget voice" })).toBeNull(),
    );
  });
});

describe("MeetingDetailView, review fixes", () => {
  it("keeps a voice's draft name on blur and saves it only on Enter or Save", async () => {
    const { user, calls } = await setup();
    const field = await screen.findByRole("textbox", { name: "Name of Speaker 2" });
    await user.type(field, "Sam");
    await user.tab();
    expect(calls.some((c) => c[0] === "saveVoice" || c[0] === "linkSpeaker")).toBe(false);
    expect(within(panel()).getByRole("textbox", { name: "Name of Speaker 2" })).toHaveValue("Sam");
    await user.type(within(panel()).getByRole("textbox", { name: "Name of Speaker 2" }), " Lee{Enter}");
    await waitFor(() =>
      expect(calls).toContainEqual(["saveVoice", ITEM.id, SAMPLE_SPEAKERS.second, { kind: "New", name: "Sam Lee" }]),
    );
  });

  it("renames a person in every meeting, apart from renaming the row", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await openNamed(user);
    await user.click(within(speakerRow("Priya Shah")).getByRole("button", { name: "Rename person" }));
    expect(within(panel()).getByText("Renames this person in every meeting.")).toBeInTheDocument();
    const field = within(panel()).getByRole("textbox", { name: "New name of Priya Shah in every meeting" });
    await user.clear(field);
    await user.type(field, "Priya Shah-Rao{Enter}");
    await waitFor(() => expect(calls).toContainEqual(["renamePerson", "9e000000-0000-7000-8000-000000000002", "Priya Shah-Rao"]));
    expect(calls.some((c) => c[0] === "renameSpeaker")).toBe(false);
    expect(await within(panel()).findByText("Priya Shah-Rao")).toBeInTheDocument();
    // A voice without a person has no "Rename person".
    expect(within(speakerRow("Speaker 3")).queryByRole("button", { name: "Rename person" })).toBeNull();
  });

  it("says why a saved voice was not remembered", async () => {
    const { user } = await setup({
      rememberOutcomes: { [SAMPLE_SPEAKERS.second]: "Unsupported" },
    });
    await user.type(await screen.findByRole("textbox", { name: "Name of Speaker 2" }), "Jo Park{Enter}");
    expect(
      await screen.findByText("Saved Jo Park. The speaker model in use cannot remember voices, so no voiceprint was kept."),
    ).toBeInTheDocument();
    // Named all the same.
    expect(screen.getByRole("button", { name: "Speakers, 1 needs a name" })).toBeInTheDocument();
  });

  it("words every remember outcome of a save by name", () => {
    expect(savedText("Jo Park", "Remembered")).toBe("Saved Jo Park. Later meetings will recognise this voice.");
    expect(savedText("Jo Park", "AlreadyRemembered")).toBe(
      "Saved Jo Park. This voice was already remembered from this meeting.",
    );
    expect(savedText("Jo Park", "NotEnoughSpeech")).toBe(
      "Saved Jo Park. They did not speak long enough for their voice to be remembered, so later meetings will not recognise it yet.",
    );
    expect(savedText("Jo Park", "NoStoredVoice")).toBe(
      "Saved Jo Park. There is no stored voice for them yet: voices are kept once the meeting has been processed.",
    );
    expect(savedText("Jo Park", "Unsupported")).toBe(
      "Saved Jo Park. The speaker model in use cannot remember voices, so no voiceprint was kept.",
    );
  });

  it("shows a job's ending after showing another meeting meanwhile", async () => {
    const { user, timer } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("button", { name: "Re-summarise" }));
    expect(screen.getByText("Writing a new summary")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: new RegExp(`^${OTHER.title}`) }));
    await screen.findByRole("heading", { level: 1, name: OTHER.title });
    act(() => timer.advance(POST_STEP_MS));
    await user.click(screen.getByRole("button", { name: new RegExp(`^${ITEM.title}`) }));
    expect(await screen.findByText(/New summary ready/)).toBeInTheDocument();
    expect(await screen.findByText(/The client hears the new date by Friday/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Switch to previous summary" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText(/New summary ready/)).toBeNull();
  });
});

describe("handoverLine", () => {
  it("says when and how much the last summary run handed to Claude Code or Codex, and nothing without one", () => {
    const item = sampleMeetings(Date.UTC(2026, 10, 2, 9, 30))[0];
    const d = sampleDetail(item);
    expect(handoverLine(d)).toBeNull();
    const at = new Date(2026, 10, 4, 14, 2).getTime();
    const line = handoverLine({
      ...d,
      handovers: [
        { cli: "claude_code", at_ms: at - 60_000, characters: 10, outcome: "sign_in_expired" },
        { cli: "claude_code", at_ms: at, characters: 5324, outcome: "answered" },
      ],
    });
    expect(line).toBe("Handed to Claude Code: 5,324 characters at 14:02");
    const codex = handoverLine({
      ...d,
      handovers: [
        { cli: "claude_code", at_ms: at - 60_000, characters: 10, outcome: "answered" },
        { cli: "codex", at_ms: at, characters: 5400, outcome: "access_denied" },
      ],
    });
    expect(codex).toBe("Handed to Codex: 5,400 characters at 14:02");
  });
});

describe("MeetingDetailView, suggested words (T12.15e)", () => {
  /** The sample line with the suggestion: "Reed" sounds like the saved person Tom Reid. */
  const LINE8 = sampleDetail(ITEM).transcript[7].segment_id;
  const LINE8_TEXT = "Fine by us. I'll send it to Tom Reed in writing.";
  const marks = () => screen.queryAllByRole("button", { name: /^(Suggestion for|Changed word) / });
  /** The words cell of the last line. */
  const lineCell = () => {
    const rows = within(screen.getByRole("table", { name: "Transcript" })).getAllByRole("row");
    return within(rows[7]).getAllByRole("cell")[2];
  };

  async function transcript(data: MockIpcData = {}) {
    const s = await setup(data);
    await s.user.click(await screen.findByRole("tab", { name: "Transcript" }));
    return s;
  }

  it("underlines the suggested word only, and Tab reaches it", async () => {
    const { user } = await transcript();
    const mark = screen.getByRole("button", { name: "Suggestion for Reed" });
    expect(marks()).toEqual([mark]);
    // The word alone is the control; the line reads as before.
    expect(mark).toHaveTextContent(/^Reed$/);
    expect(mark).toHaveAttribute("data-word-mark", "suggestion");
    expect(lineCell().textContent).toBe(LINE8_TEXT);
    screen.getByRole("button", { name: "Edit line 7" }).focus();
    await user.tab();
    expect(mark).toHaveFocus();
  });

  it("opens with Enter, and Use it changes the word and leaves a changed mark", async () => {
    const { user, calls } = await transcript();
    screen.getByRole("button", { name: "Suggestion for Reed" }).focus();
    await user.keyboard("{Enter}");
    const dialog = await screen.findByRole("dialog", { name: "Suggestion for Reed" });
    expect(dialog).toHaveTextContent("Your dictionary has Reid");
    await user.click(within(dialog).getByRole("button", { name: "Use it" }));

    await waitFor(() =>
      expect(lineCell().textContent).toBe("Fine by us. I'll send it to Tom Reid in writing.(edited)"),
    );
    expect(calls).toContainEqual(["useWord", ITEM.id, LINE8, 8, "Reid"]);
    const changed = screen.getByRole("button", { name: "Changed word Reid" });
    expect(changed).toHaveAttribute("data-word-mark", "changed");
    expect(marks()).toEqual([changed]);
    expect(screen.queryByRole("textbox", { name: "Line 8" })).not.toBeInTheDocument();
  });

  it("puts a changed word back from its own mark", async () => {
    const { user, calls } = await transcript();
    await user.click(screen.getByRole("button", { name: "Suggestion for Reed" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Use it" }));
    await user.click(await screen.findByRole("button", { name: "Changed word Reid" }));
    const dialog = await screen.findByRole("dialog", { name: "Changed word Reid" });
    expect(dialog).toHaveTextContent("Changed from Reed");
    await user.click(within(dialog).getByRole("button", { name: "Put back Reed" }));

    await waitFor(() => expect(lineCell().textContent).toBe(`${LINE8_TEXT}(edited)`));
    expect(calls).toContainEqual(["putBackWord", ITEM.id, LINE8, 8]);
    expect(screen.queryByRole("button", { name: "Changed word Reid" })).not.toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "Line 8" })).not.toBeInTheDocument();
  });

  it("writes another word chosen from the list", async () => {
    const { user, calls } = await transcript({ summaries: { vocabulary: ["Reidar", "Quillan"] } });
    await user.click(screen.getByRole("button", { name: "Suggestion for Reed" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Use it" }));
    await user.click(await screen.findByRole("button", { name: "Changed word Reid" }));
    const dialog = await screen.findByRole("dialog", { name: "Changed word Reid" });
    const list = within(dialog).getByRole("combobox", { name: "Another word" });
    await user.type(list, "reida");
    // The typed text narrows the list.
    expect(await screen.findByRole("option", { name: "Reidar" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "Quillan" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("option", { name: "Reidar" }));

    await waitFor(() =>
      expect(lineCell().textContent).toBe("Fine by us. I'll send it to Tom Reidar in writing.(edited)"),
    );
    expect(calls).toContainEqual(["useWord", ITEM.id, LINE8, 8, "Reidar"]);
    expect(screen.getByRole("button", { name: "Changed word Reidar" })).toBeInTheDocument();
  });

  it("keeps the word with Keep: the mark goes and the line is unchanged", async () => {
    const { user, calls } = await transcript();
    await user.click(screen.getByRole("button", { name: "Suggestion for Reed" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Keep" }));

    await waitFor(() => expect(marks()).toEqual([]));
    expect(calls).toContainEqual(["keepWord", ITEM.id, LINE8, 8]);
    expect(lineCell().textContent).toBe(LINE8_TEXT);
    expect(screen.queryByRole("textbox", { name: "Line 8" })).not.toBeInTheDocument();
  });

  it("opens the popover and never a line edit from the word; line edits start only from Edit", async () => {
    const { user } = await transcript();
    await user.click(screen.getByRole("button", { name: "Suggestion for Reed" }));
    expect(await screen.findByRole("dialog", { name: "Suggestion for Reed" })).toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "Line 8" })).not.toBeInTheDocument();
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());

    // A click on the line's text opens no editor, as before.
    await user.click(lineCell());
    expect(screen.queryByRole("textbox", { name: "Line 8" })).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Edit line 8" }));
    const field = screen.getByRole("textbox", { name: "Line 8" });
    expect(field).toHaveValue(LINE8_TEXT);
    // While the line is edited its marks are not shown.
    expect(marks()).toEqual([]);
  });

  it("closes with Escape and gives focus back to the word", async () => {
    const { user } = await transcript();
    const mark = screen.getByRole("button", { name: "Suggestion for Reed" });
    await user.click(mark);
    await screen.findByRole("dialog", { name: "Suggestion for Reed" });
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(mark).toHaveFocus();
  });

  it("shows a refusal as a line edit's refusal is shown", async () => {
    // Without the saved person the word is no longer on the list.
    const { user } = await transcript({ persons: samplePersons().filter((p) => p.name !== "Tom Reid") });
    await user.click(screen.getByRole("button", { name: "Suggestion for Reed" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Use it" }));
    expect(await screen.findByText("The word was not changed")).toBeInTheDocument();
    expect(screen.getByText(/not in your dictionary or among the people you saved/)).toBeInTheDocument();
    expect(lineCell().textContent).toBe(LINE8_TEXT);
  });

  it("shows no marks on a draft transcript", async () => {
    const base = sampleDetail(ITEM);
    const draft = { ...base, header: { ...base.header, transcript: "Draft" as const } };
    await transcript({ details: { [ITEM.id]: draft } });
    expect(draft.transcript[7].suggestions).not.toEqual([]);
    expect(screen.getByText(LINE8_TEXT)).toBeInTheDocument();
    expect(marks()).toEqual([]);
  });
});

describe("MeetingDetailView, words to the Dictionary (T12.15f)", () => {
  const LINE1 = sampleDetail(ITEM).transcript[0].segment_id;
  const LINE1_EDITED = "So the lead time on the Quillmarsh pump set is now looking like eleven weeks.";
  const word = (w: string) => document.querySelector<HTMLElement>(`[data-dictionary-word="${w}"]`);
  const bar = () => screen.queryByRole("group", { name: "Add to Dictionary" });

  async function transcript(data: MockIpcData = {}) {
    const s = await setup(data);
    await s.user.click(await screen.findByRole("tab", { name: "Transcript" }));
    return s;
  }

  async function editLine1(user: ReturnType<typeof userEvent.setup>, text = LINE1_EDITED) {
    await user.click(screen.getByRole("button", { name: "Edit line 1" }));
    const field = screen.getByRole("textbox", { name: "Line 1" });
    await user.clear(field);
    await user.type(field, text);
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "Line 1" })).not.toBeInTheDocument());
  }

  it("offers the words an edit wrote, tinted in the line, and adds one only on a click", async () => {
    const { user, calls, ipc } = await transcript();
    await editLine1(user);
    const offered = await within(bar()!).findByRole("button", { name: 'Add "Quillmarsh" to Dictionary' });
    // Only the written word: every other word was in the line before.
    expect(within(bar()!).getAllByRole("button").map((b) => b.textContent)).toEqual(["Quillmarsh", "Not now"]);
    expect(word("Quillmarsh")?.className).not.toBe("");
    expect(word("pump")?.className ?? "").toBe("");
    // Nothing was added by the edit itself.
    expect(calls.some((c) => c[0] === "addDictionaryWord")).toBe(false);
    expect((await ipc.summaryVocabulary()).words).toEqual([]);

    await user.click(offered);
    const status = await screen.findByRole("status");
    expect(status).toHaveTextContent('Added "Quillmarsh".');
    expect(calls).toContainEqual(["addDictionaryWord", "Quillmarsh"]);
    expect((await ipc.summaryVocabulary()).words).toEqual(["Quillmarsh"]);
    // The bar has no word left to offer, and the listed word loses its tint; Undo has the focus.
    expect(bar()).toBeNull();
    expect(word("Quillmarsh")?.className ?? "").toBe("");
    const undo = within(status).getByRole("button", { name: "Undo" });
    await waitFor(() => expect(undo).toHaveFocus());
    // The line's text is unchanged by the add.
    expect(word("Quillmarsh")?.closest('[role="cell"]')?.textContent).toBe(`${LINE1_EDITED}(edited)`);
  });

  it("Undo removes exactly the word added and offers it again", async () => {
    const { user, calls, ipc } = await transcript({ summaries: { vocabulary: ["QXR-7"] } });
    await editLine1(user);
    await user.click(await screen.findByRole("button", { name: 'Add "Quillmarsh" to Dictionary' }));
    await user.click(within(await screen.findByRole("status")).getByRole("button", { name: "Undo" }));

    await waitFor(() => expect(screen.queryByRole("status")).not.toBeInTheDocument());
    expect(calls).toContainEqual(["removeDictionaryWord", "Quillmarsh"]);
    expect((await ipc.summaryVocabulary()).words).toEqual(["QXR-7"]);
    const again = await screen.findByRole("button", { name: 'Add "Quillmarsh" to Dictionary' });
    await waitFor(() => expect(again).toHaveFocus());
  });

  it("says how many other lines now carry a suggestion of the word", async () => {
    const base = sampleDetail(ITEM);
    const detail = {
      ...base,
      transcript: base.transcript.map((r, i) =>
        i === 3 ? { ...r, suggestions: [{ token: 0, decoded: "Agreed", listed: "Quillmarsh" }] } : r,
      ),
    };
    const { user } = await transcript({ details: { [ITEM.id]: detail } });
    await editLine1(user);
    await user.click(await screen.findByRole("button", { name: 'Add "Quillmarsh" to Dictionary' }));
    const status = await screen.findByRole("status");
    expect(status).toHaveTextContent('Added "Quillmarsh".');
    expect(status).toHaveTextContent("1 other line now has a suggestion.");
  });

  it("closes the bar when the user moves on, and ignoring it adds nothing", async () => {
    const { user, calls } = await transcript();
    await editLine1(user);
    expect(await screen.findByRole("group", { name: "Add to Dictionary" })).toBeInTheDocument();
    await user.click(within(bar()!).getByRole("button", { name: "Not now" }));
    expect(bar()).toBeNull();
    expect(word("Quillmarsh")?.className ?? "").toBe("");

    // Again, then another line's edit: the bar goes.
    await editLine1(user, "So the lead time on the Velmora pump set is now looking like eleven weeks.");
    expect(await screen.findByRole("button", { name: 'Add "Velmora" to Dictionary' })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Edit line 2" }));
    expect(bar()).toBeNull();
    // Escape inside the bar closes it too.
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    await editLine1(user, "So the lead time on the Tarnwick pump set is now looking like eleven weeks.");
    const button = await screen.findByRole("button", { name: 'Add "Tarnwick" to Dictionary' });
    button.focus();
    await user.keyboard("{Escape}");
    expect(bar()).toBeNull();
    expect(calls.some((c) => c[0] === "addDictionaryWord")).toBe(false);
  });

  it("keeps the bar while another line's menu is open, and closes it when that menu adds a word", async () => {
    const { user } = await transcript();
    await editLine1(user);
    await screen.findByRole("group", { name: "Add to Dictionary" });
    fireEvent.contextMenu(word("commissioning")!, { clientX: 40, clientY: 40 });
    const item = await screen.findByRole("menuitem", { name: 'Add "commissioning" to Dictionary' });
    expect(bar()).not.toBeNull();
    await user.click(item);
    const status = await screen.findByRole("status");
    expect(status).toHaveTextContent('Added "commissioning".');
    expect(status.closest('[role="row"]')).toBe(word("commissioning")!.closest('[role="row"]'));
    expect(bar()).toBeNull();
  });

  it("offers no bar when the edit wrote only common or listed words", async () => {
    const { user, calls } = await transcript({ summaries: { vocabulary: ["Quillmarsh"] } });
    await editLine1(user);
    await editLine1(user, "So the lead time on the Quillmarsh pump set is now looking like eleven weeks, we think.");
    expect(calls.filter((c) => c[0] === "editSegment")).toHaveLength(2);
    expect(bar()).toBeNull();
  });

  it("opens a word's menu on right-click: Add for a new word, Remove for a listed one, no Edit line", async () => {
    const { user, calls, ipc } = await transcript({ summaries: { vocabulary: ["Pump"] } });
    fireEvent.contextMenu(word("lead")!, { clientX: 40, clientY: 40 });
    const add = await screen.findByRole("menuitem", { name: 'Add "lead" to Dictionary' });
    expect(screen.getAllByRole("menuitem")).toEqual([add]);
    expect(screen.queryByRole("menuitem", { name: /Edit/ })).toBeNull();
    await user.click(add);
    expect(await screen.findByRole("status")).toHaveTextContent('Added "lead".');
    expect(calls).toContainEqual(["addDictionaryWord", "lead"]);

    // "pump" is listed as "Pump": case ignored.
    fireEvent.contextMenu(word("pump")!, { clientX: 40, clientY: 40 });
    await user.click(await screen.findByRole("menuitem", { name: 'Remove "pump" from Dictionary' }));
    expect(await screen.findByText('Removed "pump" from your Dictionary.')).toBeInTheDocument();
    expect(calls).toContainEqual(["removeDictionaryWord", "pump"]);
    expect((await ipc.summaryVocabulary()).words).toEqual(["lead"]);
    // Nothing changed the line.
    expect(calls.some((c) => c[0] === "editSegment")).toBe(false);
  });

  it("opens the menu from the keyboard: a marked word's own, or every word of the line from Edit", async () => {
    const { user, calls } = await transcript();
    const mark = screen.getByRole("button", { name: "Suggestion for Reed" });
    mark.focus();
    await user.keyboard("{Shift>}{F10}{/Shift}");
    const item = await screen.findByRole("menuitem", { name: 'Add "Reed" to Dictionary' });
    expect(screen.getAllByRole("menuitem")).toEqual([item]);
    await waitFor(() => expect(item).toHaveFocus());
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("menuitem")).toBeNull());
    await waitFor(() => expect(mark).toHaveFocus());

    screen.getByRole("button", { name: "Edit line 8" }).focus();
    await user.keyboard("{ContextMenu}");
    await screen.findAllByRole("menuitem");
    expect(screen.getAllByRole("menuitem").map((m) => m.textContent)).toEqual(
      ["Fine", "by", "us", "I'll", "send", "it", "to", "Tom", "Reed", "in", "writing"].map(
        (w) => `Add "${w}" to Dictionary`,
      ),
    );
    await user.click(screen.getByRole("menuitem", { name: 'Add "Tom" to Dictionary' }));
    await waitFor(() => expect(calls).toContainEqual(["addDictionaryWord", "Tom"]));
    // The menu opened no line edit.
    expect(screen.queryByRole("textbox", { name: "Line 8" })).not.toBeInTheDocument();
  });

  it("gives a marked word's popover the same action, and the popover's clicks open no menu", async () => {
    const { user, calls } = await transcript({ summaries: { vocabulary: ["reed"] } });
    // "Reed" is listed: the popover offers Remove.
    await user.click(screen.getByRole("button", { name: "Suggestion for Reed" }));
    const dialog = await screen.findByRole("dialog", { name: "Suggestion for Reed" });
    fireEvent.contextMenu(within(dialog).getByRole("button", { name: "Keep" }));
    expect(screen.queryByRole("menuitem")).toBeNull();
    await user.click(within(dialog).getByRole("button", { name: "Remove from Dictionary" }));
    await waitFor(() => expect(calls).toContainEqual(["removeDictionaryWord", "Reed"]));
    await user.click(await screen.findByRole("button", { name: "Suggestion for Reed" }));
    const again = await screen.findByRole("dialog", { name: "Suggestion for Reed" });
    await user.click(within(again).getByRole("button", { name: "Add to Dictionary" }));
    await waitFor(() => expect(calls).toContainEqual(["addDictionaryWord", "Reed"]));
  });

  it("shows a refusal in place, in the Dictionary page's own words", async () => {
    const full = Array.from({ length: 100 }, (_, i) => `w${i}`);
    const { user, ipc } = await transcript({ summaries: { vocabulary: full } });
    fireEvent.contextMenu(word("lead")!, { clientX: 40, clientY: 40 });
    await user.click(await screen.findByRole("menuitem", { name: 'Add "lead" to Dictionary' }));
    const status = await screen.findByRole("status");
    expect(status).toHaveTextContent("The list can hold at most 100 words; this one has 101.");
    expect(within(status).queryByRole("button", { name: "Undo" })).toBeNull();
    expect((await ipc.summaryVocabulary()).words).toEqual(full);
    // In place: under the line, not the page's notice.
    expect(status.closest('[role="row"]')).toBe(word("lead")!.closest('[role="row"]'));
  });

  it("offers nothing on a draft transcript", async () => {
    const base = sampleDetail(ITEM);
    const draft = { ...base, header: { ...base.header, transcript: "Draft" as const } };
    await transcript({ details: { [ITEM.id]: draft } });
    expect(word("lead")).toBeNull();
    fireEvent.contextMenu(screen.getByText(base.transcript[0].text));
    expect(screen.queryByRole("menuitem")).toBeNull();
  });

  it("the mock's offer follows words_to_offer", async () => {
    const ipc = createMockIpc({ meetings: [ITEM], summaries: { vocabulary: ["Zorbek"] } });
    const offer = await ipc.editSegment(
      ITEM.id,
      LINE1,
      "So the lead time on the Quillmarsh (zorbek) pump set is now about 11 weeks, quillmarsh.",
    );
    expect(offer).toEqual(["Quillmarsh"]);
    expect(await ipc.editSegment(ITEM.id, LINE1, "So the lead time on the Quillmarsh (zorbek) pump set is now about 11 weeks, quillmarsh.")).toEqual([]);
  });
});
