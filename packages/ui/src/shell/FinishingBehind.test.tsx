// Meetings finishing behind the one the window follows keep their pages right, a failed start
// costs the previous meeting nothing, and a start that failed is said wherever the user is
// (T14.15).

import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import {
  type Ipc,
  IpcFailure,
  type MeetingEvent,
  type MeetingListItem,
  type MockIpcData,
  type ShellEvent,
  createMockIpc,
} from "../ipc";
import { renderApp } from "../test/app";
import { createManualTimer } from "../time";

const at = (hour: number) => new Date(2026, 10, 2, hour, 0).getTime();
const NOW = at(16);

const OLDER: MeetingListItem = { id: "m-older", title: "Paving quote", started_at_ms: at(14), duration_ms: 30 * 60_000, state: "Ready", folder_id: null, new_summary: false };
const OTHER: MeetingListItem = { id: "m-other", title: "Irrigation plan", started_at_ms: at(11), duration_ms: 20 * 60_000, state: "Ready", folder_id: null, new_summary: false };

/** A promise settled by the test, for an answer that arrives when the test says. */
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

async function open(over: (base: Ipc) => Partial<Ipc> = () => ({}), data: MockIpcData = {}) {
  const timer = createManualTimer(NOW);
  const base = createMockIpc({ timer, now: NOW, meetings: [OLDER, OTHER], ...data });
  let shell: ((event: ShellEvent) => void) | null = null;
  let live: ((event: MeetingEvent) => void) | null = null;
  /** The meetings whose page was read, in order. */
  const reads: string[] = [];
  const ipc: Ipc = {
    ...base,
    getMeeting: async (id) => {
      reads.push(id);
      return base.getMeeting(id);
    },
    onShellEvent: async (handler) => {
      shell = handler;
      return () => {};
    },
    onMeetingEvent: async (handler) => {
      live = handler;
      return () => {};
    },
    ...over(base),
  };
  renderApp(ipc, { timer, now: () => NOW });
  await screen.findByRole("heading", { level: 1, name: OLDER.title });
  await waitFor(() => {
    expect(shell).not.toBeNull();
    expect(live).not.toBeNull();
  });
  return {
    ipc,
    reads,
    readsOf: (id: string) => reads.filter((r) => r === id).length,
    send: (event: ShellEvent) => act(() => shell?.(event)),
    meeting: (event: MeetingEvent) => act(() => live?.(event)),
    user: userEvent.setup(),
  };
}

const started = (meeting: string): MeetingEvent => ({ type: "Started", meeting, mode: "Call", tracks: ["You", "Them"] });
const ended = (meeting: string): MeetingEvent => ({ type: "Ended", meeting, reason: "Stopped" });
const stored = (meeting: string): MeetingEvent => ({ type: "PostProgress", meeting, step: { kind: "Stored" }, done: 5, total: 6 });
const summary = (meeting: string): MeetingEvent => ({
  type: "Summary",
  meeting,
  result: { kind: "Stored", title: "Invented title", actions: 1, decisions: 1, voices_to_name: 0 },
});

const row = (item: MeetingListItem) =>
  within(screen.getByRole("list", { name: "Meetings" })).getByRole("button", { name: new RegExp(`^${item.title}`) });
const page = () => screen.getByRole("region", { name: "Meeting detail" });
const refused = (message: string) => new IpcFailure({ kind: "model", message, retryable: false });

describe("a meeting finishing behind the one the window follows", () => {
  it("has its open page read again when its transcript and then its summary are stored", async () => {
    const s = await open();
    s.meeting(started(OLDER.id));
    s.meeting(ended(OLDER.id));
    await screen.findByText("Summary in progress");
    // A newer meeting starts: the window follows it, and the older one finishes behind it.
    s.meeting(started("m-newer"));
    await screen.findByRole("button", { name: "Stop meeting" });
    await s.user.click(row(OLDER));
    await screen.findByRole("heading", { level: 1, name: OLDER.title });
    // It opens on the transcript, as a meeting still finishing does.
    await waitFor(() => expect(within(page()).getByRole("tab", { name: "Transcript" })).toHaveAttribute("aria-selected", "true"));
    await waitFor(() => expect(s.readsOf(OLDER.id)).toBeGreaterThan(0));
    const opened = s.readsOf(OLDER.id);

    // Another meeting's news is not this page's.
    s.meeting(stored(OTHER.id));
    s.meeting(summary(OTHER.id));
    await act(async () => {});
    expect(s.readsOf(OLDER.id)).toBe(opened);

    s.meeting(stored(OLDER.id));
    await waitFor(() => expect(s.readsOf(OLDER.id)).toBe(opened + 1));
    s.meeting(summary(OLDER.id));
    await waitFor(() => expect(s.readsOf(OLDER.id)).toBe(opened + 2));
    // A stored summary shows itself.
    await waitFor(() => expect(within(page()).getByRole("tab", { name: "Summary" })).toHaveAttribute("aria-selected", "true"));
    // The newer meeting records on.
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    expect(await screen.findByRole("button", { name: "Stop meeting" })).toBeEnabled();
  });

  it("keeps its page's own Re-transcribe and Summarise off until its result", async () => {
    const s = await open();
    s.meeting(started(OLDER.id));
    s.meeting(ended(OLDER.id));
    s.meeting(started("m-newer"));
    await screen.findByRole("button", { name: "Stop meeting" });
    await s.user.click(row(OLDER));
    const retranscribe = await within(page()).findByRole("button", { name: "Re-transcribe" });
    await waitFor(() => expect(retranscribe).toHaveAttribute("aria-disabled", "true"));
    expect(within(page()).getByRole("button", { name: /^(Re-s|S)ummarise$/ })).toHaveAttribute("aria-disabled", "true");
    // A meeting that is not finishing keeps its buttons.
    await s.user.click(row(OTHER));
    const others = await within(page()).findByRole("button", { name: "Re-transcribe" });
    await waitFor(() => expect(others).not.toHaveAttribute("aria-disabled", "true"));
    await s.user.click(row(OLDER));
    await waitFor(() =>
      expect(within(page()).getByRole("button", { name: "Re-transcribe" })).toHaveAttribute("aria-disabled", "true"),
    );
    s.meeting(summary(OLDER.id));
    await waitFor(() =>
      expect(within(page()).getByRole("button", { name: "Re-transcribe" })).not.toHaveAttribute("aria-disabled", "true"),
    );
  });

  it("reads the followed meeting's page again too, and no other page for it", async () => {
    const s = await open();
    s.meeting(started(OLDER.id));
    s.meeting(ended(OLDER.id));
    await screen.findByText("Summary in progress");
    await waitFor(() => expect(s.readsOf(OLDER.id)).toBeGreaterThan(0));
    const opened = s.readsOf(OLDER.id);
    s.meeting(stored(OLDER.id));
    await waitFor(() => expect(s.readsOf(OLDER.id)).toBe(opened + 1));
    // Another meeting's page, opened meanwhile, is not read for this meeting's summary.
    await s.user.click(row(OTHER));
    await screen.findByRole("heading", { level: 1, name: OTHER.title });
    await waitFor(() => expect(s.readsOf(OTHER.id)).toBeGreaterThan(0));
    const other = s.readsOf(OTHER.id);
    s.meeting(summary(OLDER.id));
    await act(async () => {});
    expect(s.readsOf(OTHER.id)).toBe(other);
  });
});

describe("starting a meeting while the previous one finishes", () => {
  it("a start that fails leaves the previous meeting's progress and its Cancel", async () => {
    const answer = deferred<string>();
    const cancelled: string[] = [];
    const s = await open(() => ({
      startMeeting: () => answer.promise,
      cancelFinishing: async (id) => {
        cancelled.push(id);
      },
    }));
    s.meeting(started(OLDER.id));
    s.meeting(ended(OLDER.id));
    await screen.findByText("Summary in progress");
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await s.user.click(await screen.findByRole("button", { name: "Start" }));
    await act(async () => answer.reject(refused("The invented model is not on this computer.")));
    expect(await screen.findByText("The meeting could not start")).toBeInTheDocument();
    expect(screen.getByText("The invented model is not on this computer.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Start" })).toBeEnabled();
    // Back on the previous meeting's page, its finishing is as it was.
    await s.user.click(screen.getByRole("button", { name: "Cancel" }));
    const status = await screen.findByRole("region", { name: "Meeting status" });
    expect(within(status).getByText("Summary in progress")).toBeInTheDocument();
    await s.user.click(within(status).getByRole("button", { name: "Cancel" }));
    expect(cancelled).toEqual([OLDER.id]);
  });

  it("shows the start, not the previous meeting's page, between the app's answer and the first event", async () => {
    const s = await open(() => ({ startMeeting: async () => "m-newer" }));
    s.meeting(started(OLDER.id));
    s.meeting(ended(OLDER.id));
    await screen.findByText("Summary in progress");
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await s.user.click(await screen.findByRole("button", { name: "Start" }));
    expect(await screen.findByText("Starting the meeting")).toBeInTheDocument();
    expect(screen.queryByRole("heading", { level: 1, name: OLDER.title })).toBeNull();
    expect(screen.queryByRole("region", { name: "Meeting status" })).toBeNull();
    s.meeting(started("m-newer"));
    expect(await screen.findByRole("button", { name: "Stop meeting" })).toBeInTheDocument();
    expect(screen.queryByText("Starting the meeting")).toBeNull();
    // The previous meeting finishes behind it, with its Cancel.
    expect(screen.getByText("Your previous meeting is still finishing")).toBeInTheDocument();
    // A later meeting the tray starts shows its own screen: nothing is left waiting for a start.
    s.meeting(ended("m-newer"));
    s.meeting(started("m-from-the-tray"));
    expect(await screen.findByRole("button", { name: "Stop meeting" })).toBeInTheDocument();
    expect(screen.queryByText("Starting the meeting")).toBeNull();
  });

  it("shows the recording when its first event arrives before the app's answer", async () => {
    const answer = deferred<string>();
    const s = await open(() => ({ startMeeting: () => answer.promise }));
    s.meeting(started(OLDER.id));
    s.meeting(ended(OLDER.id));
    await screen.findByText("Summary in progress");
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await s.user.click(await screen.findByRole("button", { name: "Start" }));
    s.meeting(started("m-newer"));
    expect(await screen.findByRole("button", { name: "Stop meeting" })).toBeInTheDocument();
    await act(async () => answer.resolve("m-newer"));
    expect(screen.getByRole("button", { name: "Stop meeting" })).toBeInTheDocument();
    expect(screen.queryByText("Starting the meeting")).toBeNull();
    // The next meeting, started elsewhere, is not held up by the answered start.
    s.meeting(ended("m-newer"));
    s.meeting(started("m-from-the-tray"));
    expect(await screen.findByRole("button", { name: "Stop meeting" })).toBeInTheDocument();
    expect(screen.queryByText("Starting the meeting")).toBeNull();
  });
});

describe("a start the app answered that never records", () => {
  const failed = (meeting: string, message: string): MeetingEvent => ({
    type: "Summary",
    meeting,
    result: { kind: "RecordingFailed", message },
  });

  it("stops saying it is starting and says why, when its result arrives with no first event", async () => {
    const s = await open(() => ({ startMeeting: async () => "m-newer" }));
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await s.user.click(await screen.findByRole("button", { name: "Start" }));
    expect(await screen.findByText("Starting the meeting")).toBeInTheDocument();
    s.meeting(failed("m-newer", "The invented microphone stopped answering."));
    await waitFor(() => expect(screen.queryByText("Starting the meeting")).toBeNull());
    expect(screen.getByText("The meeting could not start")).toBeInTheDocument();
    expect(screen.getByText("The invented microphone stopped answering.")).toBeInTheDocument();
    // The window is usable: a meeting's page shows, and a new start can be asked for.
    expect(await screen.findByRole("heading", { level: 1, name: OLDER.title })).toBeInTheDocument();
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    expect(await screen.findByRole("button", { name: "Start" })).toBeEnabled();
  });

  it("is left alone by another meeting's result, and stops saying it is starting at its own end", async () => {
    const s = await open(() => ({ startMeeting: async () => "m-newer" }));
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await s.user.click(await screen.findByRole("button", { name: "Start" }));
    await screen.findByText("Starting the meeting");
    s.meeting(failed(OTHER.id, "Invented words about another meeting."));
    expect(screen.getByText("Starting the meeting")).toBeInTheDocument();
    expect(screen.queryByText("The meeting could not start")).toBeNull();
    // Its own end without a first event: nothing is starting any more.
    s.meeting(ended("m-newer"));
    await waitFor(() => expect(screen.queryByText("Starting the meeting")).toBeNull());
    expect(await screen.findByRole("button", { name: "New meeting" })).toBeEnabled();
  });

  it("says why with fixed words when its result gives none", async () => {
    const s = await open(() => ({ startMeeting: async () => "m-newer" }));
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await s.user.click(await screen.findByRole("button", { name: "Start" }));
    await screen.findByText("Starting the meeting");
    s.meeting({ type: "Summary", meeting: "m-newer", result: { kind: "PostCancelled" } });
    await waitFor(() => expect(screen.queryByText("Starting the meeting")).toBeNull());
    expect(screen.getByText("The meeting could not start")).toBeInTheDocument();
    expect(screen.getByText("The meeting ended before it began recording. Start it again.")).toBeInTheDocument();
  });

  it("is read right when its result arrived before the app's answer", async () => {
    const answer = deferred<string>();
    const s = await open(() => ({ startMeeting: () => answer.promise }));
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await s.user.click(await screen.findByRole("button", { name: "Start" }));
    s.meeting(failed("m-newer", "The invented microphone stopped answering."));
    await act(async () => answer.resolve("m-newer"));
    expect(screen.queryByText("Starting the meeting")).toBeNull();
    expect(await screen.findByText("The meeting could not start")).toBeInTheDocument();
    expect(screen.getByText("The invented microphone stopped answering.")).toBeInTheDocument();
    // Still on the new-meeting screen, which can be used again.
    expect(screen.getByRole("button", { name: "Start" })).toBeEnabled();
  });

  it("a meeting that did record is never read as a failed start when its summary fails", async () => {
    const s = await open(() => ({ startMeeting: async () => "m-newer" }));
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await s.user.click(await screen.findByRole("button", { name: "Start" }));
    s.meeting(started("m-newer"));
    await screen.findByRole("button", { name: "Stop meeting" });
    s.meeting(ended("m-newer"));
    s.meeting({
      type: "Summary",
      meeting: "m-newer",
      result: { kind: "Failed", message: "Invented summary failure.", retryable: false },
    });
    await act(async () => {});
    expect(screen.queryByText("The meeting could not start")).toBeNull();
  });
});

describe("the bars about a start and a stop", () => {
  const stopRefused = () => ({
    stopMeeting: async () => {
      throw new IpcFailure({ kind: "internal", message: "The invented stop did not reach the app.", retryable: true });
    },
  });
  const barOf = async (title: string) => {
    const bar = (await screen.findByText(title)).closest<HTMLElement>(".fui-MessageBar");
    if (bar === null) throw new Error(`"${title}" is not in a message bar`);
    return bar;
  };

  it("are announced as the app's other error bars are", async () => {
    const s = await open(stopRefused);
    s.send({ type: "StartFailed", message: "The invented model is not installed." });
    expect(await barOf("The meeting could not start")).toHaveAttribute("role", "alert");
    s.meeting(started("m-live"));
    await s.user.click(await screen.findByRole("button", { name: "Stop meeting" }));
    expect(await barOf("The meeting could not be stopped")).toHaveAttribute("role", "alert");
  });

  it("hand focus to the row of the meeting shown when Dismiss takes the bar away", async () => {
    const s = await open();
    s.send({ type: "StartFailed", message: "The invented model is not installed." });
    const bar = await barOf("The meeting could not start");
    within(bar).getByRole("button", { name: "Dismiss" }).focus();
    await s.user.keyboard("{Enter}");
    expect(screen.queryByText("The meeting could not start")).toBeNull();
    expect(row(OLDER)).toHaveFocus();
  });

  it("hand focus to New meeting when the meeting shown has no row", async () => {
    const s = await open(stopRefused);
    s.meeting(started("m-live"));
    await s.user.click(await screen.findByRole("button", { name: "Stop meeting" }));
    const bar = await barOf("The meeting could not be stopped");
    // The group of the meeting shown is closed: its row is not on screen.
    await s.user.click(within(screen.getByRole("list", { name: "Meetings" })).getByRole("button", { name: "Today" }));
    expect(within(screen.getByRole("list", { name: "Meetings" })).queryByRole("button", { name: /^Paving quote/ })).toBeNull();
    within(bar).getByRole("button", { name: "Dismiss" }).focus();
    await s.user.keyboard("{Enter}");
    expect(screen.queryByText("The meeting could not be stopped")).toBeNull();
    // Never Stop meeting: a second Enter must not stop the recording.
    expect(screen.getByRole("button", { name: "New meeting" })).toHaveFocus();
  });

  it("hand focus to the screen's first control where there is no list (Settings)", async () => {
    const s = await open();
    await s.user.click(screen.getByRole("button", { name: "Settings" }));
    await screen.findByRole("navigation", { name: "Settings sections" });
    s.send({ type: "StartFailed", message: "The invented model is not installed." });
    const bar = await barOf("The meeting could not start");
    within(bar).getByRole("button", { name: "Dismiss" }).focus();
    await s.user.keyboard("{Enter}");
    expect(screen.queryByText("The meeting could not start")).toBeNull();
    expect(screen.getByRole("button", { name: "Back to meetings" })).toHaveFocus();
  });

  it("leave focus where it is when the bar goes while focus is elsewhere", async () => {
    const s = await open();
    s.send({ type: "StartFailed", message: "The invented model is not installed." });
    await screen.findByText("The meeting could not start");
    const search = screen.getByRole("searchbox", { name: "Search meetings" });
    await s.user.click(search);
    // A meeting starts: the bar goes by itself.
    s.meeting(started("m-live"));
    await screen.findByRole("button", { name: "Stop meeting" });
    expect(screen.queryByText("The meeting could not start")).toBeNull();
    expect(search).toHaveFocus();
  });
});

describe("a start that failed is said wherever the user is", () => {
  it("on another meeting's page, when the user left the new-meeting screen while it started", async () => {
    const answer = deferred<string>();
    const s = await open(() => ({ startMeeting: () => answer.promise }));
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await s.user.click(await screen.findByRole("button", { name: "Start" }));
    await s.user.click(row(OTHER));
    await screen.findByRole("heading", { level: 1, name: OTHER.title });
    await act(async () => answer.reject(refused("The invented microphone could not be opened.")));
    const title = await screen.findByText("The meeting could not start");
    expect(screen.getByText("The invented microphone could not be opened.")).toBeInTheDocument();
    // Still on the page the user went to.
    expect(screen.getByRole("heading", { level: 1, name: OTHER.title })).toBeInTheDocument();
    const bar = title.closest<HTMLElement>(".fui-MessageBar");
    if (bar === null) throw new Error("the start failure is not in a message bar");
    await s.user.click(within(bar).getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText("The meeting could not start")).toBeNull();
    expect(screen.getByRole("heading", { level: 1, name: OTHER.title })).toBeInTheDocument();
  });

  it("in Settings, when the user went there while it started", async () => {
    const answer = deferred<string>();
    const s = await open(() => ({ startMeeting: () => answer.promise }));
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await s.user.click(await screen.findByRole("button", { name: "Start" }));
    await s.user.click(screen.getByRole("button", { name: "Settings" }));
    await screen.findByRole("navigation", { name: "Settings sections" });
    await act(async () => answer.reject(refused("The invented microphone could not be opened.")));
    expect(await screen.findByText("The meeting could not start")).toBeInTheDocument();
    expect(screen.getByText("The invented microphone could not be opened.")).toBeInTheDocument();
    expect(screen.getByRole("navigation", { name: "Settings sections" })).toBeInTheDocument();
  });

  it("is said once on the new-meeting screen, and goes when the next start is pressed", async () => {
    let answer = deferred<string>();
    const s = await open(() => ({ startMeeting: () => answer.promise }));
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await s.user.click(await screen.findByRole("button", { name: "Start" }));
    await act(async () => answer.reject(refused("The invented microphone could not be opened.")));
    await screen.findByText("The meeting could not start");
    expect(screen.getAllByText("The meeting could not start")).toHaveLength(1);
    expect(screen.getAllByText("The invented microphone could not be opened.")).toHaveLength(1);
    answer = deferred<string>();
    await s.user.click(screen.getByRole("button", { name: "Start" }));
    expect(screen.queryByText("The meeting could not start")).toBeNull();
  });

  it("a tray or shortcut start that failed leaves the user in Settings, with the reason there", async () => {
    const s = await open();
    await s.user.click(screen.getByRole("button", { name: "Settings" }));
    const nav = await screen.findByRole("navigation", { name: "Settings sections" });
    s.send({ type: "StartFailed", message: "The invented model is not installed." });
    const title = await screen.findByText("The meeting could not start");
    expect(screen.getByText("The invented model is not installed.")).toBeInTheDocument();
    // The same Settings screen: nothing was remounted.
    expect(screen.getByRole("navigation", { name: "Settings sections" })).toBe(nav);
    const bar = title.closest<HTMLElement>(".fui-MessageBar");
    if (bar === null) throw new Error("the start failure is not in a message bar");
    await s.user.click(within(bar).getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText("The meeting could not start")).toBeNull();
    expect(screen.getByRole("navigation", { name: "Settings sections" })).toBe(nav);
  });

  it("a tray or shortcut start that failed leaves the new-meeting screen and what was typed", async () => {
    const s = await open();
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    const title = await screen.findByRole("textbox", { name: /^Title/ });
    await s.user.type(title, "Invented retaining wall call");
    s.send({ type: "StartFailed", message: "The invented model is not installed." });
    expect(await screen.findByText("The meeting could not start")).toBeInTheDocument();
    expect(screen.getByRole("textbox", { name: /^Title/ })).toBe(title);
    expect(title).toHaveValue("Invented retaining wall call");
  });
});

describe("a stop that failed", () => {
  it("says why until dismissed", async () => {
    const s = await open(() => ({
      stopMeeting: async () => {
        throw new IpcFailure({ kind: "internal", message: "The invented stop did not reach the app.", retryable: true });
      },
    }));
    s.meeting(started("m-live"));
    await s.user.click(await screen.findByRole("button", { name: "Stop meeting" }));
    const title = await screen.findByText("The meeting could not be stopped");
    expect(screen.getByText("The invented stop did not reach the app.")).toBeInTheDocument();
    const bar = title.closest<HTMLElement>(".fui-MessageBar");
    if (bar === null) throw new Error("the stop failure is not in a message bar");
    await s.user.click(within(bar).getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText("The meeting could not be stopped")).toBeNull();
    // The recording goes on, and can be stopped again.
    expect(screen.getByRole("button", { name: "Stop meeting" })).toBeEnabled();
  });
});
