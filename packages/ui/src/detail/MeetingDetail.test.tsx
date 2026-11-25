import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  type Ipc,
  IpcFailure,
  type JobKind,
  MISSING_SEGMENT,
  type MockIpcData,
  POST_STEP_MS,
  SAMPLE_SPEAKERS,
  SPEAKER_NOT_ON_TRACK_COPY,
  SPLIT_NEEDS_WORD_TIMES_COPY,
  SPLIT_OUT_OF_RANGE_COPY,
  UNKNOWN_SPEAKER,
  createMockIpc,
  sampleDetail,
  sampleMeetings,
  samplePersons,
  withUnknownLine,
} from "../ipc";
import { IpcProvider } from "../ipc/context";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { COPIED_MS, handoverLine, savedText, summaryMarkdown } from "./detail";
import { type JobView, MeetingDetailView } from "./MeetingDetailView";
import { ITEM, OTHER, setup, panel, speakerRow, openNamed } from "../test/meetingDetail";

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

  it("says a summary uses an earlier name when only a name changed (T12.82)", async () => {
    const detail = { ...sampleDetail(ITEM), summary_stale: true, summary_stale_names: true };
    const { user, calls } = await setup({ details: { [ITEM.id]: detail } });
    const note = await screen.findByText(/uses a speaker's earlier name/);
    expect(screen.queryByText(/refers to lines of an earlier transcript/)).not.toBeInTheDocument();
    const [, prompt] = screen.getAllByRole("button", { name: "Re-summarise" });
    expect(note.closest(".fui-MessageBar")).toContainElement(prompt);
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
      // The text before the last edit was itself edited, so the header still says so.
      expect(screen.getByText(/Summary by Qwen 3\.5 9B \(edited\)/)).toBeInTheDocument();
    });

    it("says in the header that the summary was edited, until undo brings back the writer's text", async () => {
      const { user } = await setup();
      await screen.findByText(/TL;DR/);
      expect(screen.getByText(/Summary by Qwen 3\.5 9B · /)).toBeInTheDocument();
      expect(screen.queryByText(/\(edited\)/)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Edit decision 1" }));
      const field = screen.getByRole("textbox", { name: "decision 1" });
      await user.clear(field);
      await user.type(field, "Split the pump order into two deliveries{Enter}");
      expect(await screen.findByText(/Summary by Qwen 3\.5 9B \(edited\) · /)).toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Switch to previous summary" }));
      expect(await screen.findByText("Split the pump order into two shipments")).toBeInTheDocument();
      expect(screen.queryByText(/\(edited\)/)).not.toBeInTheDocument();
      expect(screen.getByText(/Summary by Qwen 3\.5 9B · /)).toBeInTheDocument();
    });

    it("draws each summary edit as an icon that keeps its space while hidden, so hovering moves nothing (T12.105)", async () => {
      const { user } = await setup();
      await screen.findByText(/TL;DR/);
      for (const name of ["Edit TL;DR", "Edit decision 1", "Edit who does action 1", "Edit action 1", "Edit when action 1 is due"]) {
        const button = screen.getByRole("button", { name });
        // No words to appear on hover: an icon only.
        expect(button.textContent).toBe("");
        expect(button.querySelector("svg")).not.toBeNull();
        // Its width is fixed whether shown or not.
        expect(getComputedStyle(button).width).toBe("24px");
        expect(getComputedStyle(button).minWidth).toBe("24px");
      }
      // The due date has a calendar, the others a pencil.
      const pencil = screen.getByRole("button", { name: "Edit TL;DR" }).querySelector("path")?.getAttribute("d");
      const calendar = screen.getByRole("button", { name: "Edit when action 1 is due" }).querySelector("path")?.getAttribute("d");
      expect(calendar).not.toBe(pencil);
      // Hovering names what it edits.
      await user.hover(screen.getByRole("button", { name: "Edit decision 1" }));
      expect(await screen.findByRole("tooltip")).toHaveTextContent("Edit decision 1");
    });

    it("shows the due-date editor's label, since the action's text is not shown while it is open", async () => {
      const { user } = await setup();
      await screen.findByText(/TL;DR/);
      await user.click(screen.getByRole("button", { name: "Edit when action 1 is due" }));
      const due = screen.getByRole("textbox", { name: "when action 1 is due" });
      const shown = screen.getByText("When action 1 is due");
      expect(shown.tagName).toBe("LABEL");
      expect(shown).toHaveAttribute("for", due.id);
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

  it("shows what a re-transcription's final pass warned of with its result", async () => {
    const warning = "Some remembered voices could not be read.";
    const { user, timer } = await setup({ retranscribeWarnings: [warning] });
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("button", { name: "Re-transcribe" }));
    for (let i = 0; i < 12; i++) act(() => timer.advance(POST_STEP_MS));
    expect(await screen.findByText(`The meeting was transcribed again. ${warning}`)).toBeInTheDocument();
  });

  describe("cancelling a job (T12.41)", () => {
    it("cancels a re-summary from its notice, saying so while it stops and when it has", async () => {
      const { user, calls, ipc, timer } = await setup();
      await screen.findByText(/TL;DR/);
      let send = () => {};
      const real = ipc.cancelJob;
      ipc.cancelJob = async (meeting) => {
        await new Promise<void>((resolve) => {
          send = resolve;
        });
        await real(meeting);
      };
      await user.click(screen.getByRole("button", { name: "Re-summarise" }));
      expect(screen.getByText("Writing a new summary")).toBeInTheDocument();
      const cancel = screen.getByRole("button", { name: "Cancel the new summary" });
      expect(cancel).toHaveTextContent("Cancel");
      await user.click(cancel);
      // Until the job's last event: the bar says it stops, and the button holds.
      expect(screen.getByText("Stopping. This can take a moment if a model is still working or loading.")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Cancel the new summary" })).toHaveAttribute("aria-disabled", "true");
      await act(async () => send());
      expect(await screen.findByText("The new summary was cancelled. Nothing was changed.")).toBeInTheDocument();
      expect(calls).toContainEqual(["cancelJob", ITEM.id]);
      expect(screen.queryByRole("button", { name: /^Cancel the/ })).toBeNull();
      act(() => timer.advance(10 * POST_STEP_MS));
      expect(screen.queryByText(/New summary ready/)).toBeNull();
      // The old summary is still shown, and a new job can start.
      expect(screen.getByText(/TL;DR/)).toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Dismiss" }));
      await user.click(screen.getByRole("button", { name: "Re-summarise" }));
      expect(screen.getByRole("button", { name: "Cancel the new summary" })).not.toHaveAttribute("aria-disabled");
    });

    it("cancels a re-transcription during its steps", async () => {
      const { user, calls, timer } = await setup();
      await screen.findByText(/TL;DR/);
      await user.click(screen.getByRole("button", { name: "Re-transcribe" }));
      act(() => timer.advance(2 * POST_STEP_MS));
      expect(screen.getByText(/^Re-transcribing: /)).toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Cancel the re-transcription" }));
      expect(
        await screen.findByText("The re-transcription was cancelled. The transcript has not changed."),
      ).toBeInTheDocument();
      expect(calls).toContainEqual(["cancelJob", ITEM.id]);
      act(() => timer.advance(20 * POST_STEP_MS));
      expect(screen.queryByText("The meeting was transcribed again.")).toBeNull();
    });

    it("keeps the job running when the cancel could not be sent", async () => {
      const { user, ipc } = await setup();
      await screen.findByText(/TL;DR/);
      ipc.cancelJob = async () => {
        throw new Error("no");
      };
      await user.click(screen.getByRole("button", { name: "Re-summarise" }));
      await user.click(screen.getByRole("button", { name: "Cancel the new summary" }));
      await waitFor(() =>
        expect(screen.getByRole("button", { name: "Cancel the new summary" })).not.toHaveAttribute("aria-disabled"),
      );
      expect(screen.getByText("Writing a new summary")).toBeInTheDocument();
    });
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
    const field = await screen.findByRole("combobox", { name: "Name of Speaker 2" });
    await user.type(field, "Sam");
    await user.tab();
    expect(calls.some((c) => c[0] === "saveVoice" || c[0] === "linkSpeaker")).toBe(false);
    expect(within(panel()).getByRole("combobox", { name: "Name of Speaker 2" })).toHaveValue("Sam");
    await user.type(within(panel()).getByRole("combobox", { name: "Name of Speaker 2" }), " Lee{Enter}");
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
    await user.type(await screen.findByRole("combobox", { name: "Name of Speaker 2" }), "Jo Park{Enter}");
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
    // Line 7's last control is its More button (T12.90).
    screen.getByRole("button", { name: "More actions for line 7" }).focus();
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

  it("leaves the written word out of Another word whatever its case", async () => {
    // "Reid" was written while the list held it so; the Dictionary now lists it as "reid".
    const base = sampleDetail(ITEM);
    const detail = {
      ...base,
      transcript: base.transcript.map((r, i) =>
        i === 7
          ? {
              ...r,
              text: "Fine by us. I'll send it to Tom Reid in writing.",
              suggestions: [],
              changed_words: [{ token: 8, decoded: "Reed", written: "Reid" }],
            }
          : r,
      ),
    };
    const { user } = await transcript({
      details: { [ITEM.id]: detail },
      summaries: { vocabulary: ["reid", "Quillan"] },
    });
    await user.click(screen.getByRole("button", { name: "Changed word Reid" }));
    const dialog = await screen.findByRole("dialog", { name: "Changed word Reid" });
    await user.click(within(dialog).getByRole("combobox", { name: "Another word" }));
    expect(await screen.findByRole("option", { name: "Quillan" })).toBeInTheDocument();
    const offered = screen.getAllByRole("option").map((o) => o.textContent?.toLowerCase());
    expect(offered).not.toContain("reid");
  });

  it("keeps the word with Keep: the mark goes and the line is unchanged", async () => {
    const { user, calls } = await transcript();
    await user.click(screen.getByRole("button", { name: "Suggestion for Reed" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Keep" }));

    await waitFor(() => expect(marks()).toEqual([]));
    expect(calls).toContainEqual(["keepWord", ITEM.id, LINE8, 8]);
    // The popover just closed: tabster hides the page for its deferred pass, then shows it again.
    await waitFor(() => expect(lineCell().textContent).toBe(LINE8_TEXT));
    expect(screen.queryByRole("textbox", { name: "Line 8" })).not.toBeInTheDocument();
  });

  it("gives focus to the line's Edit button after Keep", async () => {
    const { user } = await transcript();
    // Held from before: the popover's modal hides the page from role queries for a moment.
    const edit = screen.getByRole("button", { name: "Edit line 8" });
    screen.getByRole("button", { name: "Suggestion for Reed" }).focus();
    await user.keyboard("{Enter}");
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Keep" }));
    await waitFor(() => expect(marks()).toEqual([]));
    await waitFor(() => expect(edit).toHaveFocus());
  });

  it("keeps focus on the word's mark after Use it", async () => {
    const { user } = await transcript();
    const mark = screen.getByRole("button", { name: "Suggestion for Reed" });
    mark.focus();
    await user.keyboard("{Enter}");
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Use it" }));
    const changed = await screen.findByRole("button", { name: "Changed word Reid" });
    expect(changed).toBe(mark);
    await waitFor(() => expect(changed).toHaveFocus());
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
    // The popover just closed: tabster hides the page for its deferred pass, then shows it again.
    await waitFor(() => expect(lineCell().textContent).toBe(LINE8_TEXT));
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

  // One way of moving on per test: each types a whole line key by key, and three in one test
  // ran past the test timeout on a slower machine.
  it("closes the bar on Not now, and ignoring it adds nothing", async () => {
    const { user, calls } = await transcript();
    await editLine1(user);
    expect(await screen.findByRole("group", { name: "Add to Dictionary" })).toBeInTheDocument();
    await user.click(within(bar()!).getByRole("button", { name: "Not now" }));
    expect(bar()).toBeNull();
    expect(word("Quillmarsh")?.className ?? "").toBe("");
    expect(calls.some((c) => c[0] === "addDictionaryWord")).toBe(false);
  });

  it("closes the bar when another line's edit starts, and adds nothing", async () => {
    const { user, calls } = await transcript();
    await editLine1(user, "So the lead time on the Velmora pump set is now looking like eleven weeks.");
    expect(await screen.findByRole("button", { name: 'Add "Velmora" to Dictionary' })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Edit line 2" }));
    expect(bar()).toBeNull();
    expect(calls.some((c) => c[0] === "addDictionaryWord")).toBe(false);
  });

  it("closes the bar with Escape inside it, and adds nothing", async () => {
    const { user, calls } = await transcript();
    await editLine1(user, "So the lead time on the Tarnwick pump set is now looking like eleven weeks.");
    const button = await screen.findByRole("button", { name: 'Add "Tarnwick" to Dictionary' });
    button.focus();
    await user.keyboard("{Escape}");
    expect(bar()).toBeNull();
    expect(calls.some((c) => c[0] === "addDictionaryWord")).toBe(false);
  });

  it("gives focus to the line's Edit button when the bar closes with Not now", async () => {
    const { user } = await transcript();
    await editLine1(user);
    const offer = await screen.findByRole("group", { name: "Add to Dictionary" });
    const edit = screen.getByRole("button", { name: "Edit line 1" });
    await user.click(within(offer).getByRole("button", { name: "Not now" }));
    expect(bar()).toBeNull();
    await waitFor(() => expect(edit).toHaveFocus());
  });

  it("gives focus to the line's Edit button when the bar closes with Escape", async () => {
    const { user } = await transcript();
    await editLine1(user);
    const edit = screen.getByRole("button", { name: "Edit line 1" });
    (await screen.findByRole("button", { name: 'Add "Quillmarsh" to Dictionary' })).focus();
    await user.keyboard("{Escape}");
    expect(bar()).toBeNull();
    await waitFor(() => expect(edit).toHaveFocus());
  });

  it("gives focus to the line's Edit button when a plain word's menu closes", async () => {
    const { user } = await transcript();
    // Held from before: the open menu hides the page from role queries for a moment.
    const edit = screen.getByRole("button", { name: "Edit line 1" });
    fireEvent.contextMenu(word("lead")!, { clientX: 40, clientY: 40 });
    await screen.findByRole("menuitem", { name: 'Add "lead" to Dictionary' });
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("menuitem")).toBeNull());
    await waitFor(() => expect(edit).toHaveFocus());
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

describe("MeetingDetailView while the meeting finishes (T12.28)", () => {
  function show(finishing: boolean, ipc: Ipc, job?: JobView) {
    return (
      <EarshotThemeProvider>
        <IpcProvider ipc={ipc}>
          <MeetingDetailView
            item={ITEM}
            onChanged={() => {}}
            onJobStarted={() => {}}
            onJobDismissed={() => {}}
            onJobCancel={() => {}}
            finishing={finishing}
            job={job}
          />
        </IpcProvider>
      </EarshotThemeProvider>
    );
  }

  it("shows the Speakers panel but keeps every edit off until a result, and says why", async () => {
    const calls: unknown[][] = [];
    const ipc = createMockIpc({ timer: createManualTimer(10_000), meetings: [ITEM, OTHER], onCall: (c) => calls.push(c) });
    const user = userEvent.setup();
    const view = render(show(true, ipc));
    await screen.findByRole("complementary", { name: "Speakers" });
    expect(
      within(panel()).getByText(
        "Name voices when the summary is done: the final transcript may change who is who.",
      ),
    ).toBeInTheDocument();
    const row = speakerRow("Speaker 3");
    const name = within(row).getByRole("combobox", { name: "Name of Speaker 3" });
    expect(name).toBeDisabled();
    expect(within(row).getByRole("button", { name: "Save Speaker 3" })).toBeDisabled();
    expect(within(row).getByRole("button", { name: "Ignore" })).toBeDisabled();
    expect(within(row).getByRole("button", { name: "Merge" })).toBeDisabled();
    // Playing a sample changes nothing, so it stays.
    expect(within(row).getByRole("button", { name: "Play a sample of Speaker 3" })).toBeEnabled();
    await openNamed(user);
    const named = within(panel()).getByRole("list", { name: "Named" });
    for (const button of within(named).getAllByRole("button")) {
      if (!/^Play a sample/.test(button.getAttribute("aria-label") ?? "")) {
        expect(button.hasAttribute("disabled") || button.getAttribute("aria-disabled") === "true").toBe(true);
      }
    }
    await user.click(within(row).getByRole("button", { name: "Ignore" }));
    await user.click(within(named).getAllByRole("button", { name: "Link to person" })[0]);
    await user.click(within(named).getByRole("button", { name: "Not Priya Shah" }));
    const edits = [
      "saveVoice",
      "ignoreSpeaker",
      "linkSpeaker",
      "renameSpeaker",
      "mergeSpeakers",
      "markMe",
      "unmarkMe",
      "notThisPerson",
    ];
    expect(calls.filter((c) => edits.includes(c[0] as string))).toEqual([]);
    expect(screen.queryByRole("dialog", { name: /Link .* to a person/ })).toBeNull();

    view.rerender(show(false, ipc));
    expect(within(panel()).queryByText(/Name voices when the summary is done/)).toBeNull();
    expect(within(speakerRow("Speaker 3")).getByRole("combobox", { name: "Name of Speaker 3" })).toBeEnabled();
    await user.click(within(speakerRow("Speaker 3")).getByRole("button", { name: "Ignore" }));
    expect(calls).toContainEqual(["ignoreSpeaker", ITEM.id, SAMPLE_SPEAKERS.third]);
  });

  it("keeps line edits and word choices off while the meeting is busy (TODO 95)", async () => {
    const calls: unknown[][] = [];
    const ipc = createMockIpc({
      timer: createManualTimer(10_000),
      meetings: [ITEM, OTHER],
      onCall: (c) => calls.push(c),
    });
    const user = userEvent.setup();
    const busy = (job: JobKind): JobView => ({ job, last: null, seq: 1 });
    const offWhile = async (finishing: boolean, job?: JobView) => {
      const view = render(show(finishing, ipc, job));
      await user.click(await screen.findByRole("tab", { name: "Transcript" }));
      const edit = screen.getByRole("button", { name: "Edit line 8" });
      expect(edit).toHaveAttribute("aria-disabled", "true");
      await user.click(edit);
      expect(screen.queryByRole("textbox", { name: "Line 8" })).toBeNull();
      // The mark still opens: its Dictionary action changes no line, its choices are off.
      await user.click(screen.getByRole("button", { name: "Suggestion for Reed" }));
      const dialog = await screen.findByRole("dialog", { name: "Suggestion for Reed" });
      expect(within(dialog).getByRole("button", { name: "Use it" })).toBeDisabled();
      expect(within(dialog).getByRole("button", { name: "Keep" })).toBeDisabled();
      expect(within(dialog).getByRole("button", { name: "Add to Dictionary" })).toBeEnabled();
      view.unmount();
    };
    await offWhile(true);
    await offWhile(false, busy("Retranscribe"));
    await offWhile(false, busy("Resummarise"));
    const edits = ["editSegment", "useWord", "keepWord", "putBackWord"];
    expect(calls.filter((c) => edits.includes(c[0] as string))).toEqual([]);

    // A finished job leaves them on.
    const failed: JobView = {
      job: "Retranscribe",
      last: { type: "Failed", meeting: ITEM.id, job: "Retranscribe", message: "Invented.", retryable: true },
      seq: 2,
    };
    render(show(false, ipc, failed));
    await user.click(await screen.findByRole("tab", { name: "Transcript" }));
    expect(screen.getByRole("button", { name: "Edit line 8" })).not.toHaveAttribute("aria-disabled");
  });

  it("keeps Not <name> off while a job runs, as the runtime refuses it, and on again after", async () => {
    const calls: unknown[][] = [];
    const ipc = createMockIpc({ timer: createManualTimer(10_000), meetings: [ITEM, OTHER], onCall: (c) => calls.push(c) });
    const user = userEvent.setup();
    const running: JobView = { job: "Resummarise", last: null, seq: 1 };
    const view = render(show(false, ipc, running));
    await screen.findByRole("complementary", { name: "Speakers" });
    await openNamed(user);
    const not = within(speakerRow("Priya Shah")).getByRole("button", { name: "Not Priya Shah" });
    expect(not).toBeDisabled();
    // Other speaker edits keep their own lock (finishing only).
    expect(within(speakerRow("Priya Shah")).getByRole("button", { name: "Link to person" })).toBeEnabled();
    await user.click(not);
    expect(calls.some((c) => c[0] === "notThisPerson")).toBe(false);

    const done: JobView = {
      job: "Resummarise",
      last: { type: "Failed", meeting: ITEM.id, job: "Resummarise", message: "Invented.", retryable: true },
      seq: 2,
    };
    view.rerender(show(false, ipc, done));
    expect(within(speakerRow("Priya Shah")).getByRole("button", { name: "Not Priya Shah" })).toBeEnabled();
  });
});

describe("MeetingDetailView, a line with an unknown speaker (T12.65)", () => {
  /** The sample with line 6 left without a speaker by the final pass. */
  const unknown = (transcript: "Final" | "Draft" = "Final") => {
    const d = withUnknownLine(sampleDetail(ITEM), 5);
    return { ...d, header: { ...d.header, transcript } };
  };
  const row = (n: number) => within(screen.getByRole("table", { name: "Transcript" })).getAllByRole("row")[n - 1];

  async function transcript(data: MockIpcData) {
    const s = await setup(data);
    await s.user.click(await screen.findByRole("tab", { name: "Transcript" }));
    return s;
  }

  it("shows the line as an unknown speaker and gives it a speaker chosen from the line", async () => {
    const { user, calls } = await transcript({ details: { [ITEM.id]: unknown() } });
    const speakerCell = within(row(6)).getAllByRole("cell")[1];
    expect(speakerCell).toHaveTextContent(UNKNOWN_SPEAKER);
    // Only the unknown line offers a choice.
    expect(screen.getAllByRole("button", { name: /^Choose who said line/ })).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "Choose who said line 6" }));
    const menu = await screen.findByRole("menu");
    // The far end's speakers (T12.90: Speaker 3, whose only line this is, by its far-end
    // card), then a new speaker.
    expect(within(menu).getAllByRole("menuitem").map((m) => m.textContent)).toEqual([
      "Priya Shah",
      "Speaker 2",
      "Speaker 3",
      "New speaker",
    ]);
    await user.click(within(menu).getByRole("menuitem", { name: "Speaker 2" }));
    await waitFor(() => expect(within(row(6)).getAllByRole("cell")[1]).toHaveTextContent("Speaker 2"));
    expect(within(row(6)).getAllByRole("cell")[1]).not.toHaveTextContent(UNKNOWN_SPEAKER);
    expect(row(6)).toHaveTextContent("(edited)");
    expect(screen.queryByRole("button", { name: /^Choose who said line/ })).toBeNull();
    expect(calls.filter((c) => c[0] === "moveLine")).toEqual([
      ["moveLine", ITEM.id, sampleDetail(ITEM).transcript[5].segment_id, SAMPLE_SPEAKERS.second],
    ]);
  });

  it("offers no choice on a draft transcript", async () => {
    await transcript({ details: { [ITEM.id]: unknown("Draft") } });
    expect(screen.queryByRole("button", { name: /^Choose who said line/ })).toBeNull();
  });

  it("keeps the choice off while the meeting is busy", async () => {
    const calls: unknown[][] = [];
    const ipc = createMockIpc({
      timer: createManualTimer(10_000),
      meetings: [ITEM, OTHER],
      details: { [ITEM.id]: unknown() },
      onCall: (c) => calls.push(c),
    });
    const user = userEvent.setup();
    render(
      <EarshotThemeProvider>
        <IpcProvider ipc={ipc}>
          <MeetingDetailView
            item={ITEM}
            onChanged={() => {}}
            onJobStarted={() => {}}
            onJobDismissed={() => {}}
            onJobCancel={() => {}}
            finishing
          />
        </IpcProvider>
      </EarshotThemeProvider>,
    );
    await user.click(await screen.findByRole("tab", { name: "Transcript" }));
    const choose = screen.getByRole("button", { name: "Choose who said line 6" });
    expect(choose).toHaveAttribute("aria-disabled", "true");
    await user.click(choose);
    expect(screen.queryByRole("menu")).toBeNull();
    // T12.90: every line's More menu is off too.
    const more = screen.getByRole("button", { name: "More actions for line 1" });
    expect(more).toHaveAttribute("aria-disabled", "true");
    await user.click(more);
    expect(screen.queryByRole("menu")).toBeNull();
    expect(calls.some((c) => c[0] === "moveLine" || c[0] === "splitLine")).toBe(false);
  });

  it("the mock refuses a speaker of the other track and an unknown speaker, as the runtime does", async () => {
    const ipc = createMockIpc({ meetings: [ITEM], details: { [ITEM.id]: unknown() } });
    const line1 = sampleDetail(ITEM).transcript[0].segment_id;
    await expect(ipc.moveLine(ITEM.id, line1, SAMPLE_SPEAKERS.me)).rejects.toMatchObject({
      error: { kind: "speaker_not_on_track", message: SPEAKER_NOT_ON_TRACK_COPY },
    });
    await expect(ipc.splitLine(ITEM.id, line1, 0)).rejects.toMatchObject({
      error: { kind: "split_out_of_range", message: SPLIT_OUT_OF_RANGE_COPY },
    });
    const line6 = sampleDetail(ITEM).transcript[5].segment_id;
    await expect(ipc.moveLine(ITEM.id, line6, "5a000000-0000-7000-8000-00000000ffff")).rejects.toMatchObject({
      error: { kind: "speaker_not_found" },
    });
  });
});

describe("MeetingDetailView, moving and splitting a line (T12.90)", () => {
  const row = (n: number) => within(screen.getByRole("table", { name: "Transcript" })).getAllByRole("row")[n - 1];
  const speakerOf = (n: number) => within(row(n)).getAllByRole("cell")[1];

  async function transcript(data: MockIpcData = {}) {
    const s = await setup(data);
    await s.user.click(await screen.findByRole("tab", { name: "Transcript" }));
    return s;
  }

  /** Opens line `n`'s "Move to speaker" list and returns its item names. */
  async function moveMenu(user: ReturnType<typeof userEvent.setup>, n: number) {
    await user.click(screen.getByRole("button", { name: `More actions for line ${n}` }));
    await user.click(await screen.findByRole("menuitem", { name: "Move to speaker" }));
    await screen.findByRole("menuitem", { name: "New speaker" });
    const menus = screen.getAllByRole("menu");
    return within(menus[menus.length - 1])
      .getAllByRole("menuitem")
      .map((m) => m.textContent);
  }

  it("moves a line to another speaker of its side of the call", async () => {
    const { user, calls } = await transcript();
    expect(await moveMenu(user, 1)).toEqual(["Priya Shah", "Speaker 3", "New speaker"]);
    await user.click(screen.getByRole("menuitem", { name: "Priya Shah" }));
    await waitFor(() => expect(speakerOf(1)).toHaveTextContent("Priya Shah"));
    expect(row(1)).toHaveTextContent("(edited)");
    expect(calls.filter((c) => c[0] === "moveLine")).toEqual([
      ["moveLine", ITEM.id, sampleDetail(ITEM).transcript[0].segment_id, SAMPLE_SPEAKERS.priya],
    ]);
  });

  it("moves a line to a new speaker", async () => {
    const { user, calls } = await transcript();
    await moveMenu(user, 1);
    await user.click(screen.getByRole("menuitem", { name: "New speaker" }));
    await waitFor(() => expect(speakerOf(1)).toHaveTextContent("Speaker 1"));
    expect(calls.filter((c) => c[0] === "moveLine")).toEqual([
      ["moveLine", ITEM.id, sampleDetail(ITEM).transcript[0].segment_id, null],
    ]);
    // The new voice is in the Speakers panel, needing a name.
    expect(screen.getByRole("button", { name: "Speakers, 3 need names" })).toBeInTheDocument();
  });

  it("offers a speaker whose last line moved away, so the move can be undone", async () => {
    const { user } = await transcript();
    // Line 6 is Speaker 3's only line.
    await moveMenu(user, 6);
    await user.click(screen.getByRole("menuitem", { name: "Priya Shah" }));
    await waitFor(() => expect(speakerOf(6)).toHaveTextContent("Priya Shah"));
    expect(await moveMenu(user, 6)).toEqual(["Speaker 2", "Speaker 3", "New speaker"]);
    await user.click(screen.getByRole("menuitem", { name: "Speaker 3" }));
    await waitFor(() => expect(speakerOf(6)).toHaveTextContent("Speaker 3"));
  });

  it("makes one new speaker however often New speaker is chosen", async () => {
    const { user, calls } = await transcript();
    await moveMenu(user, 1);
    await user.click(screen.getByRole("menuitem", { name: "New speaker" }));
    await waitFor(() => expect(speakerOf(1)).toHaveTextContent("Speaker 1"));
    await moveMenu(user, 1);
    await user.click(screen.getByRole("menuitem", { name: "New speaker" }));
    await waitFor(() => expect(calls.filter((c) => c[0] === "moveLine")).toHaveLength(2));
    expect(speakerOf(1)).toHaveTextContent("Speaker 1");
    expect(screen.getByRole("button", { name: "Speakers, 3 need names" })).toBeInTheDocument();
  });

  it("offers only the speakers heard on the line's own track", async () => {
    const { user } = await transcript();
    // "Me" is the only voice on the mic: the far end's speakers are not offered.
    expect(await moveMenu(user, 2)).toEqual(["New speaker"]);
  });

  it("splits a line before the chosen word", async () => {
    const { user, calls } = await transcript();
    await user.click(screen.getByRole("button", { name: "More actions for line 1" }));
    await user.click(await screen.findByRole("menuitem", { name: "Split line" }));
    const group = screen.getByRole("group", { name: "Split line 1" });
    // From the second word on, each word is a split point; the first takes focus.
    expect(within(group).queryByRole("button", { name: /^Split before word 1:/ })).toBeNull();
    expect(within(group).getByRole("button", { name: "Split before word 2: the" })).toHaveFocus();
    await user.click(within(group).getByRole("button", { name: "Split before word 3: lead" }));
    await waitFor(() => expect(screen.queryByRole("group", { name: "Split line 1" })).toBeNull());
    expect(calls.filter((c) => c[0] === "splitLine")).toEqual([
      ["splitLine", ITEM.id, sampleDetail(ITEM).transcript[0].segment_id, 2],
    ]);
    const table = screen.getByRole("table", { name: "Transcript" });
    expect(within(table).getAllByRole("row")).toHaveLength(9);
    expect(within(row(1)).getAllByRole("cell")[2].textContent).toBe("So the(edited)");
    expect(within(row(2)).getAllByRole("cell")[2].textContent).toMatch(/^lead time on the second pump set/);
    expect(speakerOf(2)).toHaveTextContent("Speaker 2");
    expect(screen.getByRole("button", { name: "Edit line 1" })).toHaveFocus();
  });

  it("keeps Cancel out of the word group and cancels by click", async () => {
    const { user, calls } = await transcript();
    await user.click(screen.getByRole("button", { name: "More actions for line 1" }));
    await user.click(await screen.findByRole("menuitem", { name: "Split line" }));
    const group = screen.getByRole("group", { name: "Split line 1" });
    expect(within(group).queryByRole("button", { name: "Cancel" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("group", { name: "Split line 1" })).toBeNull();
    expect(calls.some((c) => c[0] === "splitLine")).toBe(false);
    expect(screen.getByRole("button", { name: "Edit line 1" })).toHaveFocus();
  });

  it("tabs from the last word to Cancel and describes the group by its caption", async () => {
    const { user } = await transcript();
    await user.click(screen.getByRole("button", { name: "More actions for line 1" }));
    await user.click(await screen.findByRole("menuitem", { name: "Split line" }));
    const group = screen.getByRole("group", { name: "Split line 1" });
    expect(group).toHaveAccessibleDescription("Choose the word the new line starts with.");
    const buttons = within(group).getAllByRole("button");
    buttons[buttons.length - 1].focus();
    await user.tab();
    expect(screen.getByRole("button", { name: "Cancel" })).toHaveFocus();
  });

  it("leaves the split points with Escape without splitting", async () => {
    const { user, calls } = await transcript();
    await user.click(screen.getByRole("button", { name: "More actions for line 1" }));
    await user.click(await screen.findByRole("menuitem", { name: "Split line" }));
    expect(screen.getByRole("group", { name: "Split line 1" })).toBeInTheDocument();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("group", { name: "Split line 1" })).toBeNull();
    expect(calls.some((c) => c[0] === "splitLine")).toBe(false);
    expect(screen.getByRole("button", { name: "Edit line 1" })).toHaveFocus();
  });

  it("offers no split on a line whose words were edited", async () => {
    const { user, ipc } = await transcript();
    await user.click(screen.getByRole("button", { name: "More actions for line 1" }));
    expect(await screen.findByRole("menuitem", { name: "Split line" })).toBeInTheDocument();
    await user.keyboard("{Escape}");
    await user.click(screen.getByRole("button", { name: "Edit line 1" }));
    const field = screen.getByRole("textbox", { name: "Line 1" });
    await user.clear(field);
    await user.type(field, "So the lead time is now eleven weeks.");
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(row(1)).toHaveTextContent("(edited)"));
    await user.click(screen.getByRole("button", { name: "More actions for line 1" }));
    await screen.findByRole("menuitem", { name: "Move to speaker" });
    // "Split line" stays in the menu, disabled, saying why in its own text.
    const split = screen.getByRole("menuitem", { name: /^Split line/ });
    expect(split).toHaveAttribute("aria-disabled", "true");
    expect(split).toHaveTextContent(SPLIT_NEEDS_WORD_TIMES_COPY);
    await user.click(split);
    expect(screen.queryByRole("group", { name: "Split line 1" })).toBeNull();
    // The mock refuses it as the runtime does, with the same copy.
    await expect(ipc.splitLine(ITEM.id, sampleDetail(ITEM).transcript[0].segment_id, 1)).rejects.toMatchObject({
      error: { kind: "split_needs_word_times", message: SPLIT_NEEDS_WORD_TIMES_COPY },
    });
  });

  it("offers no split on a one-word line", async () => {
    const d = sampleDetail(ITEM);
    const oneWord = { ...d, transcript: d.transcript.map((r, i) => (i === 0 ? { ...r, text: "Okay." } : r)) };
    const { user } = await transcript({ details: { [ITEM.id]: oneWord } });
    await user.click(screen.getByRole("button", { name: "More actions for line 1" }));
    await screen.findByRole("menuitem", { name: "Move to speaker" });
    expect(screen.queryByRole("menuitem", { name: /^Split line/ })).toBeNull();
  });

  it("offers no move or split on a draft transcript", async () => {
    const draft = { ...sampleDetail(ITEM), header: { ...sampleDetail(ITEM).header, transcript: "Draft" as const } };
    await transcript({ details: { [ITEM.id]: draft } });
    expect(screen.queryByRole("button", { name: /^More actions for line/ })).toBeNull();
  });
});