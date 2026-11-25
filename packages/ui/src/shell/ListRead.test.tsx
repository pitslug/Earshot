// A meeting list that could not be read keeps what was read before (T14.16).

import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { type Ipc, IpcFailure, type MeetingListItem, type SyncEvent, createMockIpc } from "../ipc";
import { renderApp } from "../test/app";
import { createManualTimer } from "../time";

const at = (hour: number) => new Date(2026, 10, 2, hour, 0).getTime();
const NOW = at(16);

const SHOWN: MeetingListItem = { id: "m-shown", title: "Drainage survey", started_at_ms: at(14), duration_ms: 30 * 60_000, state: "Ready", folder_id: null, new_summary: false };
const OTHER: MeetingListItem = { id: "m-other", title: "Mulch delivery", started_at_ms: at(11), duration_ms: 20 * 60_000, state: "Ready", folder_id: null, new_summary: false };

const BUSY = "The invented store was busy. Try again in a moment.";
const STALE_TITLE = "The meeting list could not be updated";
const FAILED_TITLE = "Meetings could not be loaded";

function open(failing: { meetings: boolean; folders: boolean }) {
  const timer = createManualTimer(NOW);
  const base = createMockIpc({ timer, now: NOW, meetings: [SHOWN, OTHER] });
  let sync: ((event: SyncEvent) => void) | null = null;
  const reads = { n: 0 };
  const fail = (): never => {
    throw new IpcFailure({ kind: "store", message: BUSY, retryable: true });
  };
  const ipc: Ipc = {
    ...base,
    listMeetings: async () => {
      reads.n += 1;
      if (failing.meetings) fail();
      return base.listMeetings();
    },
    listFolders: async () => {
      if (failing.folders) fail();
      return base.listFolders();
    },
    onSyncEvent: async (handler) => {
      sync = handler;
      return () => {};
    },
  };
  renderApp(ipc, { timer, now: () => NOW });
  return {
    ipc,
    reads,
    /** Something changed elsewhere: the window reads its list again. */
    readAgain: async () => {
      await waitFor(() => expect(sync).not.toBeNull());
      const before = reads.n;
      act(() => sync?.({ meetings: [], people: false }));
      await waitFor(() => expect(reads.n).toBe(before + 1));
      await act(async () => {});
    },
    user: userEvent.setup(),
  };
}

const list = () => screen.getByRole("list", { name: "Meetings" });
const rowNames = () =>
  within(list())
    .getAllByRole("button", { name: /^(?!Delete )/ })
    .filter((b) => b.getAttribute("aria-expanded") === null)
    .map((b) => b.textContent);

describe("a meeting list that could not be read", () => {
  it("keeps the list and the open meeting, says so, and reads again on Try again", async () => {
    const failing = { meetings: false, folders: false };
    const s = open(failing);
    const heading = await screen.findByRole("heading", { level: 1, name: SHOWN.title });
    await s.user.click(await screen.findByRole("tab", { name: "Transcript" }));
    const table = await screen.findByRole("table", { name: "Transcript" });
    const rows = rowNames();
    expect(rows).toHaveLength(2);

    failing.meetings = true;
    await s.readAgain();
    const title = await screen.findByText(STALE_TITLE);
    expect(screen.getByText(BUSY)).toBeInTheDocument();
    // The list as it was, and the same open page: nothing was unmounted, so nothing typed or
    // scrolled on it is lost.
    expect(rowNames()).toEqual(rows);
    expect(screen.getByRole("heading", { level: 1, name: SHOWN.title })).toBe(heading);
    expect(screen.getByRole("table", { name: "Transcript" })).toBe(table);
    expect(screen.queryByText(FAILED_TITLE)).toBeNull();

    // Trying again while it still fails keeps everything, the reason included.
    const bar = title.closest<HTMLElement>(".fui-MessageBar");
    if (bar === null) throw new Error("the failure is not in a message bar");
    const before = s.reads.n;
    await s.user.click(within(bar).getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(s.reads.n).toBe(before + 1));
    await act(async () => {});
    expect(screen.getByText(STALE_TITLE)).toBeInTheDocument();
    expect(rowNames()).toEqual(rows);

    // Once it reads, the bar goes and the list is the new one.
    failing.meetings = false;
    await s.ipc.renameMeeting(OTHER.id, "Mulch delivery, second load");
    await s.user.click(screen.getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(screen.queryByText(STALE_TITLE)).toBeNull());
    expect(await within(list()).findByRole("button", { name: /^Mulch delivery, second load/ })).toBeInTheDocument();
    expect(screen.getByRole("heading", { level: 1, name: SHOWN.title })).toBe(heading);
  });

  it("keeps the list when only the folders could not be read", async () => {
    const failing = { meetings: false, folders: false };
    const s = open(failing);
    await screen.findByRole("heading", { level: 1, name: SHOWN.title });
    const rows = rowNames();
    failing.folders = true;
    await s.readAgain();
    expect(await screen.findByText(STALE_TITLE)).toBeInTheDocument();
    expect(rowNames()).toEqual(rows);
    expect(screen.getByRole("heading", { level: 1, name: SHOWN.title })).toBeInTheDocument();
  });

  it("a later read that works clears the bar by itself", async () => {
    const failing = { meetings: false, folders: false };
    const s = open(failing);
    await screen.findByRole("heading", { level: 1, name: SHOWN.title });
    failing.meetings = true;
    await s.readAgain();
    await screen.findByText(STALE_TITLE);
    failing.meetings = false;
    await s.readAgain();
    await waitFor(() => expect(screen.queryByText(STALE_TITLE)).toBeNull());
    expect(rowNames()).toHaveLength(2);
  });

  it("is announced as the app's other error bars are, and hands focus to the row of the meeting shown once Try again worked", async () => {
    const failing = { meetings: false, folders: false };
    const s = open(failing);
    await screen.findByRole("heading", { level: 1, name: SHOWN.title });
    failing.meetings = true;
    await s.readAgain();
    const bar = (await screen.findByText(STALE_TITLE)).closest<HTMLElement>(".fui-MessageBar");
    if (bar === null) throw new Error("the failure is not in a message bar");
    expect(bar).toHaveAttribute("role", "alert");
    const again = within(bar).getByRole("button", { name: "Try again" });
    // While it still fails the bar stays, and so does focus on its button.
    again.focus();
    let before = s.reads.n;
    await s.user.keyboard("{Enter}");
    await waitFor(() => expect(s.reads.n).toBe(before + 1));
    await act(async () => {});
    expect(screen.getByText(STALE_TITLE)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Try again" })).toHaveFocus();
    // Once it reads, the bar goes and focus is on the row of the meeting shown.
    failing.meetings = false;
    before = s.reads.n;
    await s.user.keyboard("{Enter}");
    await waitFor(() => expect(screen.queryByText(STALE_TITLE)).toBeNull());
    expect(within(list()).getByRole("button", { name: new RegExp(`^${SHOWN.title}`) })).toHaveFocus();
  });

  it("leaves focus where it is when the bar goes by itself", async () => {
    const failing = { meetings: false, folders: false };
    const s = open(failing);
    await screen.findByRole("heading", { level: 1, name: SHOWN.title });
    failing.meetings = true;
    await s.readAgain();
    await screen.findByText(STALE_TITLE);
    const search = screen.getByRole("searchbox", { name: "Search meetings" });
    await s.user.click(search);
    failing.meetings = false;
    await s.readAgain();
    await waitFor(() => expect(screen.queryByText(STALE_TITLE)).toBeNull());
    expect(search).toHaveFocus();
  });

  it("hands focus to New meeting once Try again worked after a first read that failed", async () => {
    const failing = { meetings: true, folders: false };
    const s = open(failing);
    const bar = (await screen.findByText(FAILED_TITLE)).closest<HTMLElement>(".fui-MessageBar");
    if (bar === null) throw new Error("the failure is not in a message bar");
    expect(bar).toHaveAttribute("role", "alert");
    failing.meetings = false;
    within(bar).getByRole("button", { name: "Try again" }).focus();
    await s.user.keyboard("{Enter}");
    expect(await screen.findByRole("heading", { level: 1, name: SHOWN.title })).toBeInTheDocument();
    // No row was on screen when the bar went: New meeting, by the same rule.
    expect(screen.getByRole("button", { name: "New meeting" })).toHaveFocus();
  });

  it("offers Try again when the very first read failed, and then shows the meetings", async () => {
    const failing = { meetings: true, folders: false };
    const s = open(failing);
    expect(await screen.findByText(FAILED_TITLE)).toBeInTheDocument();
    expect(screen.getByText(BUSY)).toBeInTheDocument();
    expect(screen.queryByText(STALE_TITLE)).toBeNull();
    failing.meetings = false;
    await s.user.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { level: 1, name: SHOWN.title })).toBeInTheDocument();
    expect(screen.queryByText(FAILED_TITLE)).toBeNull();
    expect(rowNames()).toHaveLength(2);
  });
});
