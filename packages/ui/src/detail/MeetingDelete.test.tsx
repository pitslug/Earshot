// Deleting a meeting from the app (T9.1o, T12.38, T12.41): the question, its dialog and what
// follows, moved out of MeetingDetail.test.tsx unchanged (T12.55).

import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { IpcFailure, POST_STEP_MS } from "../ipc";
import { DELETE_COPY } from "../shell/deleteCopy";
import { ITEM, OTHER, setup, listRow, trashName, TABSTER_HIDE_PASS_MS, accessibleDialog } from "../test/meetingDetail";

describe("MeetingDetailView", () => {
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

  it("keeps a meeting with a job running, and the job, when the delete question is cancelled", async () => {
    const { user, calls, timer } = await setup();
    await screen.findByText(/TL;DR/);
    // A job is running on the meeting: the question offers to stop it first (T12.41).
    await user.click(screen.getByRole("button", { name: "Re-summarise" }));
    expect(screen.getByText("Writing a new summary")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: trashName(ITEM) }));
    const dialog = await accessibleDialog();
    expect(within(dialog).getByText(/^Earshot is writing a new summary of this meeting\./)).toBeInTheDocument();
    expect(within(dialog).queryByRole("button", { name: "Delete meeting" })).toBeNull();
    const stop = within(dialog).getByRole("button", { name: "Stop and delete" });
    expect(stop).toHaveFocus();
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(calls.some((c) => c[0] === "deleteMeeting" || c[0] === "cancelJob")).toBe(false);
    // The closed modal's aria-hidden on the page lifts on tabster's deferred pass.
    expect(await screen.findByRole("button", { name: new RegExp(`^${ITEM.title}`) })).toBeInTheDocument();
    expect(screen.getByRole("heading", { level: 1, name: ITEM.title })).toBeInTheDocument();
    // The job goes on to its own end.
    act(() => timer.advance(POST_STEP_MS));
    expect(await screen.findByText(/New summary ready/)).toBeInTheDocument();
  });

  it("still shows a refusal that comes only at the delete, with its own words", async () => {
    const { user, ipc } = await setup();
    await screen.findByText(/TL;DR/);
    const refusal = "Earshot is still stopping the work on this meeting, so it was not deleted yet. Try again in a minute.";
    ipc.deleteMeeting = async () => {
      throw new IpcFailure({ kind: "meeting_stopping", message: refusal, retryable: true });
    };
    await user.click(screen.getByRole("button", { name: trashName(ITEM) }));
    await user.click(within(await accessibleDialog()).getByRole("button", { name: "Delete meeting" }));
    expect(await screen.findByText("The meeting was not deleted")).toBeInTheDocument();
    expect(screen.getByText(refusal)).toBeInTheDocument();
    await user.click(screen.getAllByRole("button", { name: "Dismiss" })[0]);
    expect(screen.queryByText("The meeting was not deleted")).toBeNull();
  });

  it("asks before stopping a meeting that is still finishing, then stops it and deletes it", async () => {
    const { user, calls, ipc } = await setup();
    await screen.findByText(/TL;DR/);
    // The mock finishes only meetings it recorded (`mockDelete.test.ts` covers that flow).
    ipc.meetingDeletion = async () => "StopsFinishing";
    let finish = () => {};
    const real = ipc.deleteMeeting;
    ipc.deleteMeeting = async (meeting, stop) => {
      await new Promise<void>((resolve) => {
        finish = resolve;
      });
      await real(meeting, stop);
    };
    await user.click(screen.getByRole("button", { name: trashName(ITEM) }));
    const dialog = await accessibleDialog();
    expect(within(dialog).getByText(`Delete "${ITEM.title}"?`)).toBeInTheDocument();
    expect(
      within(dialog).getByText(
        "Earshot is still finishing this meeting's transcript and summary. Deleting the meeting stops that work first, then deletes its recording, transcript, summary and speaker names from this computer. The note in your Obsidian vault stays.",
      ),
    ).toBeInTheDocument();
    const stop = within(dialog).getByRole("button", { name: "Stop and delete" });
    expect(stop).toHaveFocus();
    expect(within(dialog).queryByRole("button", { name: "Delete meeting" })).toBeNull();
    // Cancel stops nothing.
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(calls.some((c) => c[0] === "deleteMeeting")).toBe(false);

    await user.click(await screen.findByRole("button", { name: trashName(ITEM) }));
    const again = await accessibleDialog();
    await user.click(within(again).getByRole("button", { name: "Stop and delete" }));
    // While the work stops, the dialog says so and holds its buttons.
    expect(within(again).getByRole("status")).toHaveTextContent("Stopping the work on this meeting, then deleting it.");
    expect(within(again).getByRole("button", { name: "Cancel" })).toHaveAttribute("aria-disabled", "true");
    await act(async () => finish());
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(calls).toContainEqual(["deleteMeeting", ITEM.id, true]);
    await screen.findByRole("heading", { level: 1, name: OTHER.title });
    expect(screen.queryByText("The meeting was not deleted")).toBeNull();
  });

  it.each([
    ["Recording", "This meeting is still recording. Stop the recording first, then delete the meeting."],
    [
      "NeedsRestart",
      "Earshot could not stop earlier work on this meeting, and that work still holds it. Restart Earshot, then delete the meeting.",
    ],
  ] as const)("says why a meeting cannot be deleted now (%s) and deletes nothing", async (ask, text) => {
    const { user, calls, ipc } = await setup();
    await screen.findByText(/TL;DR/);
    ipc.meetingDeletion = async () => ask;
    await user.click(screen.getByRole("button", { name: trashName(ITEM) }));
    const dialog = await accessibleDialog();
    expect(within(dialog).getByText(text)).toBeInTheDocument();
    expect(within(dialog).getAllByRole("button").map((b) => b.textContent)).toEqual(["Close"]);
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(calls.some((c) => c[0] === "deleteMeeting")).toBe(false);
    expect(await screen.findByRole("button", { name: new RegExp(`^${ITEM.title}`) })).toBeInTheDocument();
  });

  it("asks before stopping a re-summary, then stops it and deletes the meeting (T12.41)", async () => {
    const { user, calls, timer } = await setup();
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("button", { name: "Re-summarise" }));
    await user.click(screen.getByRole("button", { name: trashName(ITEM) }));
    const dialog = await accessibleDialog();
    expect(
      within(dialog).getByText(
        "Earshot is writing a new summary of this meeting. Deleting the meeting stops that work first, then deletes its recording, transcript, summary and speaker names from this computer. The note in your Obsidian vault stays.",
      ),
    ).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Stop and delete" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(calls).toContainEqual(["deleteMeeting", ITEM.id, true]);
    await screen.findByRole("heading", { level: 1, name: OTHER.title });
    expect(screen.queryByText("The meeting was not deleted")).toBeNull();
    // The cancelled job never ends on its own afterwards.
    act(() => timer.advance(10 * POST_STEP_MS));
    expect(screen.queryByText(/New summary ready/)).toBeNull();
  });

  it("shows the job as stopping while a Stop and delete waits for it, also after a still-stopping refusal", async () => {
    const { user, ipc } = await setup();
    await screen.findByText(/TL;DR/);
    let refuse = () => {};
    ipc.deleteMeeting = async () => {
      await new Promise<void>((resolve) => {
        refuse = resolve;
      });
      throw new IpcFailure({ kind: "meeting_stopping", message: DELETE_COPY.still_stopping, retryable: true });
    };
    await user.click(screen.getByRole("button", { name: "Re-summarise" }));
    await user.click(screen.getByRole("button", { name: trashName(ITEM) }));
    await user.click(within(await accessibleDialog()).getByRole("button", { name: "Stop and delete" }));
    const stopping = "Stopping. This can take a moment if a model is still working or loading.";
    expect(screen.getByText(stopping)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Cancel the new summary", hidden: true })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
    await act(async () => refuse());
    expect(await screen.findByText("The meeting was not deleted")).toBeInTheDocument();
    // The job was cancelled and has not ended yet: still stopping.
    expect(screen.getByText(stopping)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Cancel the new summary" })).toHaveAttribute("aria-disabled", "true");
  });

  it("puts the job back as running when a Stop and delete is refused for another reason", async () => {
    const { user, ipc } = await setup();
    await screen.findByText(/TL;DR/);
    ipc.deleteMeeting = async () => {
      throw new IpcFailure({ kind: "store", message: "The meeting could not be deleted.", retryable: true });
    };
    await user.click(screen.getByRole("button", { name: "Re-summarise" }));
    await user.click(screen.getByRole("button", { name: trashName(ITEM) }));
    await user.click(within(await accessibleDialog()).getByRole("button", { name: "Stop and delete" }));
    expect(await screen.findByText("The meeting was not deleted")).toBeInTheDocument();
    expect(screen.getByText("Writing a new summary")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Cancel the new summary" })).not.toHaveAttribute("aria-disabled");
  });
});
