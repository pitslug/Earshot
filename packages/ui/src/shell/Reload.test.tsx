// A window that loads while a meeting records or finishes rebuilds the meeting screen from the
// app's snapshot (T14.51): it lands where the meeting is, with Stop, Cancel and the locks of a
// finishing meeting as before the reload.

import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import {
  type FollowedSnapshot,
  type Ipc,
  IpcFailure,
  type MeetingEvent,
  type MeetingListItem,
  type MeetingSnapshot,
  SCRIPT_WARNING_AT,
  createMockIpc,
} from "../ipc";
import { COMPACT_STOP_FAILED, RELOADED_NOTE } from "../meeting/MeetingViews";
import { renderApp } from "../test/app";
import { createManualTimer } from "../time";

const at = (hour: number) => new Date(2026, 10, 2, hour, 0).getTime();
const NOW = at(16);

const OLDER: MeetingListItem = {
  id: "m-older",
  title: "Paving quote",
  started_at_ms: at(14),
  duration_ms: 30 * 60_000,
  state: "Processing",
  folder_id: null,
  new_summary: false,
};
const NEWER: MeetingListItem = {
  id: "m-newer",
  title: "Fence quote",
  started_at_ms: at(15),
  duration_ms: null,
  state: "Recording",
  folder_id: null,
  new_summary: false,
};

function followed(meeting: string, over: Partial<FollowedSnapshot> = {}): FollowedSnapshot {
  return {
    meeting,
    mode: "Call",
    tracks: ["You", "Them"],
    elapsed_ms: 125_000,
    ended: null,
    banners: [],
    backends: [{ track: "You", label: "CPU" }],
    words: 42,
    lag_seconds: 3,
    transcript: "Live",
    call_end_offer: false,
    progress: null,
    ...over,
  };
}

/** A promise settled by the test. */
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

/** The app with `snapshot` as the app's answer and the meeting events under the test's hand. */
async function load(snapshot: Promise<MeetingSnapshot> | MeetingSnapshot, meetings: MeetingListItem[], over: Partial<Ipc> = {}) {
  const timer = createManualTimer(NOW);
  const base = createMockIpc({ timer, now: NOW, meetings });
  let live: ((event: MeetingEvent) => void) | null = null;
  const meetingSnapshot = vi.fn(() => Promise.resolve(snapshot));
  const ipc: Ipc = {
    ...base,
    onMeetingEvent: async (handler) => {
      live = handler;
      return () => {};
    },
    meetingSnapshot,
    ...over,
  };
  renderApp(ipc, { timer, now: () => NOW });
  await waitFor(() => expect(live).not.toBeNull());
  return {
    timer,
    meetingSnapshot,
    meeting: (event: MeetingEvent) => act(() => live?.(event)),
    user: userEvent.setup(),
  };
}

const page = () => screen.getByRole("region", { name: "Meeting detail" });

describe("a window loaded while a meeting is in progress", () => {
  it("lands on the recording screen, with its clock, banners and Stop working", async () => {
    // The mock's meeting has been recording 95 s; what it sent until now reached no window.
    const timer = createManualTimer(NOW);
    const ipc = createMockIpc({ timer, now: NOW, meetings: [OLDER], recordingBeforeLoad: 95_000 });
    const stopMeeting = vi.spyOn(ipc, "stopMeeting");
    renderApp(ipc, { timer, now: () => NOW });
    const stop = await screen.findByRole("button", { name: "Stop meeting" });
    expect(screen.getByLabelText("Elapsed")).toHaveTextContent("1:35");
    expect(SCRIPT_WARNING_AT).toBeLessThan(95_000);
    expect(screen.getByText(/Bluetooth headset in hands-free mode/)).toBeInTheDocument();
    expect(screen.getByText(RELOADED_NOTE)).toBeInTheDocument();
    expect(within(screen.getByRole("group", { name: "Running on" })).getByText("GPU (CUDA)")).toBeInTheDocument();
    const user = userEvent.setup();
    await user.click(stop);
    expect(stopMeeting).toHaveBeenCalledTimes(1);
    // Its end shows its page, finishing.
    expect(await screen.findByText("Summary in progress")).toBeInTheDocument();
  });

  it("applies the events heard while it read the snapshot on top of it, in order", async () => {
    const answer = deferred<MeetingSnapshot>();
    const s = await load(answer.promise, [NEWER]);
    await waitFor(() => expect(s.meetingSnapshot).toHaveBeenCalledTimes(1));
    // Heard before the answer: the snapshot was read after some of these and holds them already.
    s.meeting({ type: "Warning", key: "far-end", kind: "Audio", message: "Invented far-end words." });
    s.meeting({ type: "WarningCleared", key: "far-end" });
    s.meeting({ type: "Warning", key: "speakers", kind: "SpeakersOff", message: "Invented speaker words." });
    s.meeting({ type: "Landed", track: "Them", start_ms: 120_000, end_ms: 122_000, text: "An invented line after the reload." });
    s.meeting({ type: "Words", total: 49 });
    expect(screen.queryByRole("button", { name: "Stop meeting" })).toBeNull();
    await act(async () =>
      answer.resolve({
        starting: false,
        followed: followed(NEWER.id, {
          banners: [{ key: "speakers", kind: "SpeakersOff", message: "Invented speaker words.", after_end: false }],
          words: 44,
        }),
        behind: [],
      }),
    );
    expect(await screen.findByRole("button", { name: "Stop meeting" })).toBeEnabled();
    expect(screen.getAllByText("Invented speaker words.")).toHaveLength(1);
    expect(screen.queryByText("Invented far-end words.")).toBeNull();
    expect(screen.getByText("An invented line after the reload.")).toBeInTheDocument();
    expect(within(screen.getByRole("group", { name: "Words so far" })).getByText("49")).toBeInTheDocument();
    expect(screen.getByLabelText("Elapsed")).toHaveTextContent("2:05");
    // An event after the answer is applied at once.
    s.meeting({ type: "WarningCleared", key: "speakers" });
    expect(screen.queryByText("Invented speaker words.")).toBeNull();
  });

  it("shows a meeting that is finishing as finishing, its edits and Re-transcribe locked", async () => {
    await load(
      {
        starting: false,
        followed: followed(OLDER.id, {
          ended: "Stopped",
          progress: { step: { kind: "Stored" }, done: 5, total: 6 },
        }),
        behind: [],
      },
      [OLDER],
    );
    expect(await screen.findByText("Summary in progress")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Stop meeting" })).toBeNull();
    const retranscribe = await within(page()).findByRole("button", { name: "Re-transcribe" });
    await waitFor(() => expect(retranscribe).toHaveAttribute("aria-disabled", "true"));
    expect(within(page()).getByRole("button", { name: /^(Re-s|S)ummarise$/ })).toHaveAttribute("aria-disabled", "true");
  });

  it("puts a meeting finishing behind the recording one in the bar, with its Cancel", async () => {
    const cancelFinishing = vi.fn(async () => {});
    const s = await load(
      {
        starting: false,
        followed: followed(NEWER.id),
        behind: [{ meeting: OLDER.id, progress: { step: { kind: "Starting" }, done: 0, total: 6 } }],
      },
      [OLDER, NEWER],
      { cancelFinishing },
    );
    await screen.findByRole("button", { name: "Stop meeting" });
    expect(screen.getByText("Your previous meeting is still finishing")).toBeInTheDocument();
    await s.user.click(screen.getByRole("button", { name: "Cancel finishing" }));
    expect(cancelFinishing).toHaveBeenCalledWith(OLDER.id);
    // Its page, opened from the list, is locked as one finishing behind.
    await s.user.click(
      within(screen.getByRole("list", { name: "Meetings" })).getByRole("button", { name: new RegExp(`^${OLDER.title}`) }),
    );
    const retranscribe = await within(page()).findByRole("button", { name: "Re-transcribe" });
    await waitFor(() => expect(retranscribe).toHaveAttribute("aria-disabled", "true"));
  });

  it("with nothing in progress shows the meeting list as always", async () => {
    await load({ starting: false, followed: null, behind: [] }, [OLDER]);
    expect(await screen.findByRole("heading", { level: 1, name: OLDER.title })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Stop meeting" })).toBeNull();
    expect(screen.queryByText("Your previous meeting is still finishing")).toBeNull();
  });
});

describe("banners dismissed before a reload", () => {
  const banners: FollowedSnapshot["banners"] = [
    { key: "far-end", kind: "Audio", message: "Invented far-end words.", after_end: false },
    { key: "speakers", kind: "SpeakersOff", message: "Invented speaker words.", after_end: false },
  ];

  /** The window as it loads: a fresh tree on the same session storage. */
  function loadWindow(snapshot: MeetingSnapshot) {
    const timer = createManualTimer(NOW);
    let live: ((event: MeetingEvent) => void) | null = null;
    const ipc: Ipc = {
      ...createMockIpc({ timer, now: NOW, meetings: [NEWER] }),
      onMeetingEvent: async (handler) => {
        live = handler;
        return () => {};
      },
      meetingSnapshot: async () => snapshot,
    };
    const view = renderApp(ipc, { timer, now: () => NOW });
    return { view, meeting: (event: MeetingEvent) => act(() => live?.(event)) };
  }

  it("stay dismissed after a reload until the app sends them again", async () => {
    const snapshot: MeetingSnapshot = { starting: false, followed: followed(NEWER.id, { banners }), behind: [] };
    const first = loadWindow(snapshot);
    const user = userEvent.setup();
    const bar = (await screen.findByText("Invented far-end words.")).closest<HTMLElement>(".fui-MessageBar");
    if (bar === null) throw new Error("the banner is not a message bar");
    await user.click(within(bar).getByRole("button", { name: /Dismiss/ }));
    expect(screen.queryByText("Invented far-end words.")).toBeNull();
    first.view.unmount();

    // The reload: the app still has both banners in force.
    const second = loadWindow(snapshot);
    expect(await screen.findByText("Invented speaker words.")).toBeInTheDocument();
    expect(screen.queryByText("Invented far-end words.")).toBeNull();
    // Sent again after the reload: it shows, and stays shown across another reload.
    second.meeting({ type: "Warning", key: "far-end", kind: "Audio", message: "Invented far-end words." });
    expect(screen.getByText("Invented far-end words.")).toBeInTheDocument();
    second.view.unmount();
    loadWindow(snapshot);
    expect(await screen.findByText("Invented far-end words.")).toBeInTheDocument();
  });

  it("are forgotten when the meeting ends or another one is followed", async () => {
    const snapshot: MeetingSnapshot = { starting: false, followed: followed(NEWER.id, { banners }), behind: [] };
    const first = loadWindow(snapshot);
    const user = userEvent.setup();
    const bar = (await screen.findByText("Invented far-end words.")).closest<HTMLElement>(".fui-MessageBar");
    if (bar === null) throw new Error("the banner is not a message bar");
    await user.click(within(bar).getByRole("button", { name: /Dismiss/ }));
    first.meeting({ type: "Summary", meeting: NEWER.id, result: { kind: "Cancelled" } });
    first.view.unmount();
    // The same id followed again (an invented replay) shows everything: nothing is remembered.
    loadWindow(snapshot);
    expect(await screen.findByText("Invented far-end words.")).toBeInTheDocument();
    expect(window.sessionStorage.length).toBe(0);
  });
});

describe("the compact recording card", () => {
  it("says why a Stop did not work", async () => {
    const stopMeeting = vi.fn(async () => {
      throw new IpcFailure({ kind: "not_recording", message: "Invented refusal.", retryable: false });
    });
    const s = await load({ starting: false, followed: followed(NEWER.id), behind: [] }, [NEWER], {
      stopMeeting,
    });
    await s.user.click(await screen.findByRole("button", { name: "Compact" }));
    await s.user.click(await screen.findByRole("button", { name: "Stop" }));
    expect(stopMeeting).toHaveBeenCalledTimes(1);
    expect(await screen.findByRole("alert")).toHaveTextContent(`${COMPACT_STOP_FAILED} Invented refusal.`);
    // The card stays, with Stop to try again.
    expect(screen.getByRole("button", { name: "Stop" })).toBeEnabled();
  });
});
