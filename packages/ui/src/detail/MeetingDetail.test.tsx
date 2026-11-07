import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

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
} from "../ipc";
import { IpcProvider } from "../ipc/context";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { AppShell } from "../shell/AppShell";
import { handoverLine, rememberText } from "./detail";

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

const panel = () => screen.getByRole("complementary", { name: "Who is who" });
const speakerRow = (name: string) => within(panel()).getByRole("listitem", { name });
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
  it("shows the header, summary and who-is-who panel with the names banner", async () => {
    await setup();
    expect(await screen.findByText(/TL;DR/)).toBeInTheDocument();
    expect(screen.getByText(/Final transcript · Summary by Qwen 3.5 9B/)).toBeInTheDocument();
    expect(screen.getByText("2 voices need names. Names flow into the summary and export.")).toBeInTheDocument();
    expect(screen.getByText("Tick actions in the Obsidian note.")).toBeInTheDocument();
    const actions = screen.getByRole("table", { name: "Actions" });
    expect(within(actions).getAllByRole("row")[0].textContent).toContain("Casey");
    expect(within(panel()).getAllByRole("listitem")).toHaveLength(4);
    expect(within(speakerRow("Priya Shah")).getByText(/Matched voiceprint · 22% of talk time/)).toBeInTheDocument();
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

  it("names a voice from its name field and the transcript follows", async () => {
    const { user, calls } = await setup();
    const field = await screen.findByRole("textbox", { name: "Name of Speaker 2" });
    await user.type(field, "Sam Lee{Enter}");
    await waitFor(() => expect(within(panel()).queryByRole("textbox", { name: "Name of Speaker 2" })).toBeNull());
    expect(calls).toContainEqual(["linkSpeaker", ITEM.id, SAMPLE_SPEAKERS.second, { kind: "New", name: "Sam Lee" }]);
    expect(calls.filter((c) => c[0] === "linkSpeaker")).toHaveLength(1);
    expect(screen.getByText("1 voice needs a name. Names flow into the summary and export.")).toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Transcript" }));
    const rows = within(screen.getByRole("table", { name: "Transcript" })).getAllByRole("row");
    expect(within(rows[0]).getAllByRole("cell")[1].textContent).toBe("Sam Lee");
  });

  it("renames a named speaker on Enter", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
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
    expect(within(speakerRow("Priya Shah")).getByText(/29% of talk time/)).toBeInTheDocument();
    // The user's own row cannot be merged away.
    expect(within(speakerRow("Me")).getByRole("button", { name: "Merge" })).toHaveAttribute("aria-disabled", "true");
  });

  it("links a voice to a new person", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(within(speakerRow("Speaker 3")).getByRole("button", { name: "Link to person" }));
    const dialog = await screen.findByRole("dialog", { name: "Link Speaker 3 to a person" });
    await user.click(within(dialog).getByRole("combobox"));
    await user.click(await screen.findByRole("option", { name: "New person" }));
    await user.type(within(dialog).getByRole("textbox", { name: "New person's name" }), "Ana Diaz");
    await user.click(within(dialog).getByRole("button", { name: "Link" }));
    await waitFor(() => expect(within(panel()).getByRole("listitem", { name: "Ana Diaz" })).toBeInTheDocument());
    expect(calls).toContainEqual(["linkSpeaker", ITEM.id, SAMPLE_SPEAKERS.third, { kind: "New", name: "Ana Diaz" }]);
  });

  it("remembers voices only when ticked, and only named voices other than yours", async () => {
    const remembered: string[][] = [];
    const { user } = await setup({ onRememberVoices: (_m, s) => remembered.push(s) });
    await screen.findByText(/TL;DR/);
    const box = within(panel()).getByRole("checkbox", { name: "Remember these voices for next time" });
    expect(box).not.toBeChecked();
    const save = within(panel()).getByRole("button", { name: "Save voices" });
    expect(save).toBeDisabled();
    await user.click(box);
    await user.click(save);
    await waitFor(() => expect(remembered).toEqual([[SAMPLE_SPEAKERS.priya]]));
    expect(await screen.findByText("1 voice remembered.")).toBeInTheDocument();
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
    const item = await screen.findByRole("menuitem", { name: "Delete meeting..." });
    await waitFor(() => expect(item).toHaveFocus());
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("menuitem")).toBeNull());
    await waitFor(() => expect(listRow(OTHER)).toHaveFocus());
    // The Menu key does the same; the browser's own contextmenu event on the row opens it too.
    await user.keyboard("{ContextMenu}");
    await screen.findByRole("menuitem", { name: "Delete meeting..." });
    await waitFor(() => expect(screen.getByRole("menuitem")).toHaveFocus());
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

  it("focuses the first name field from the names banner", async () => {
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("button", { name: "Name voices" }));
    expect(within(panel()).getByRole("textbox", { name: "Name of Speaker 2" })).toHaveFocus();
  });

  it("says when a sample's audio is no longer kept", async () => {
    const { user } = await setup({ sample: null });
    await screen.findByText(/TL;DR/);
    await user.click(within(speakerRow("Priya Shah")).getByRole("button", { name: "Play a sample of Priya Shah" }));
    expect(await within(speakerRow("Priya Shah")).findByText("Audio no longer kept")).toBeInTheDocument();
  });

  it("shows re-transcribe and samples as unavailable once the recordings are gone", async () => {
    const { user, calls } = await setup({
      details: { [ITEM.id]: { ...sampleDetail(ITEM), audio_kept: false } },
    });
    await screen.findByText(/TL;DR/);
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

  it("offers re-transcribe and samples while the recordings are kept", async () => {
    await setup();
    await screen.findByText(/TL;DR/);
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
  it("keeps a voice's draft name on blur and names it only on Enter", async () => {
    const { user, calls } = await setup();
    const field = await screen.findByRole("textbox", { name: "Name of Speaker 2" });
    await user.type(field, "Sam");
    await user.tab();
    expect(calls.some((c) => c[0] === "linkSpeaker")).toBe(false);
    expect(within(panel()).getByRole("textbox", { name: "Name of Speaker 2" })).toHaveValue("Sam");
    await user.type(within(panel()).getByRole("textbox", { name: "Name of Speaker 2" }), " Lee{Enter}");
    await waitFor(() =>
      expect(calls).toContainEqual(["linkSpeaker", ITEM.id, SAMPLE_SPEAKERS.second, { kind: "New", name: "Sam Lee" }]),
    );
  });

  it("renames a person in every meeting, apart from renaming the row", async () => {
    const { user, calls } = await setup();
    await screen.findByText(/TL;DR/);
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

  it("says why voices were not remembered", async () => {
    const { user } = await setup({
      rememberOutcomes: { [SAMPLE_SPEAKERS.priya]: "AlreadyRemembered" },
    });
    await screen.findByText(/TL;DR/);
    await user.click(within(panel()).getByRole("checkbox", { name: "Remember these voices for next time" }));
    await user.click(within(panel()).getByRole("button", { name: "Save voices" }));
    expect(
      await screen.findByText("0 voices remembered. Priya Shah was already remembered from this meeting."),
    ).toBeInTheDocument();
  });

  it("words every remember outcome by name", () => {
    const speakers = sampleDetail(ITEM).speakers;
    const r = (speaker: string, outcome: "Remembered" | "AlreadyRemembered" | "NotEnoughSpeech" | "NoStoredVoice" | "Unsupported") => ({ speaker, outcome });
    expect(
      rememberText(
        [
          r(SAMPLE_SPEAKERS.priya, "Remembered"),
          r(SAMPLE_SPEAKERS.second, "NotEnoughSpeech"),
          r(SAMPLE_SPEAKERS.third, "NoStoredVoice"),
        ],
        speakers,
      ),
    ).toBe(
      "1 voice remembered. Speaker 2 did not speak long enough to remember. There is no stored voice for Speaker 3 yet: voices are kept once the meeting has been processed.",
    );
    expect(
      rememberText([r(SAMPLE_SPEAKERS.priya, "Unsupported"), r(SAMPLE_SPEAKERS.second, "Unsupported")], speakers),
    ).toBe(
      "0 voices remembered. The speaker model in use cannot remember voices, so Priya Shah and Speaker 2 were not saved.",
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
