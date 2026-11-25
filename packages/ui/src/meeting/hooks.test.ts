// `useLiveMeeting` keeps the meters' meeting across a re-run of its event effect (T9.24 review).

import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { Ipc, MeetingEvent, MeetingSnapshot } from "../ipc";
import { createManualTimer } from "../time";
import { SNAPSHOT_WAIT_MS, useLiveMeeting } from "./hooks";

/** An Ipc with only the meeting events: `emit` reaches every live subscription. */
function events() {
  const handlers = new Set<(e: MeetingEvent) => void>();
  const ipc = {
    onMeetingEvent: async (h: (e: MeetingEvent) => void) => {
      handlers.add(h);
      return () => handlers.delete(h);
    },
  } as unknown as Ipc;
  const emit = (e: MeetingEvent) => act(() => [...handlers].forEach((h) => h(e)));
  return { ipc, emit };
}

describe("useLiveMeeting", () => {
  it("keeps the meters on the meeting when its event effect runs again mid-meeting", async () => {
    const { ipc, emit } = events();
    const first = createManualTimer(0);
    const { result, rerender } = renderHook(({ timer }) => useLiveMeeting(ipc, timer), {
      initialProps: { timer: first },
    });
    await act(async () => {});
    emit({ type: "Started", meeting: "m", mode: "Call", tracks: ["You", "Them"] });
    emit({ type: "Levels", you_dbfs: -20, them_dbfs: -30 });
    expect(result.current.levels.get()).toEqual({ You: -20, Them: -30 });

    // A new timer re-runs the effect: the meters still belong to the meeting.
    rerender({ timer: createManualTimer(0) });
    await act(async () => {});
    emit({ type: "Levels", you_dbfs: -10, them_dbfs: -15 });
    expect(result.current.levels.get()).toEqual({ You: -10, Them: -15 });
  });
});

/** `events()` with a snapshot read the test controls. */
function withSnapshot(read: () => Promise<MeetingSnapshot>) {
  const e = events();
  let reads = 0;
  (e.ipc as unknown as { meetingSnapshot: () => Promise<MeetingSnapshot> }).meetingSnapshot = () => {
    reads += 1;
    return read();
  };
  return { ...e, reads: () => reads };
}

const startedEvent: MeetingEvent = { type: "Started", meeting: "m", mode: "Call", tracks: ["You", "Them"] };
const line: MeetingEvent = { type: "Landed", track: "Them", start_ms: 1_000, end_ms: 2_000, text: "An invented line." };

describe("useLiveMeeting and the app's snapshot (T14.51)", () => {
  it("goes on without a snapshot that never answers, after its wait, and ignores a late answer", async () => {
    let answer: (s: MeetingSnapshot) => void = () => {};
    const s = withSnapshot(() => new Promise((resolve) => (answer = resolve)));
    const timer = createManualTimer(0);
    const { result } = renderHook(() => useLiveMeeting(s.ipc, timer));
    await act(async () => {});
    expect(s.reads()).toBe(1);
    s.emit(startedEvent);
    s.emit(line);
    // Held while the snapshot is awaited.
    expect(result.current.meeting).toBeNull();
    act(() => timer.advance(SNAPSHOT_WAIT_MS - 1));
    expect(result.current.meeting).toBeNull();
    act(() => timer.advance(1));
    expect(result.current.meeting?.meetingId).toBe("m");
    expect(result.current.meeting?.lines.map((l) => l.text)).toEqual(["An invented line."]);
    // A late answer changes nothing.
    await act(async () => answer({ followed: null, behind: [] }));
    expect(result.current.meeting?.lines).toHaveLength(1);
  });

  it("applies the held events when the snapshot read fails", async () => {
    const s = withSnapshot(() => Promise.reject(new Error("invented failure")));
    const timer = createManualTimer(0);
    const { result } = renderHook(() => useLiveMeeting(s.ipc, timer));
    await act(async () => {});
    s.emit(startedEvent);
    s.emit(line);
    expect(result.current.meeting?.lines.map((l) => l.text)).toEqual(["An invented line."]);
  });

  it("keeps the lines shown when its effect runs again, and reads the snapshot once", async () => {
    // The app says meeting "m" records: a second seed would replace what the window shows.
    const s = withSnapshot(async () => ({
      followed: {
        meeting: "m",
        mode: "Call",
        tracks: ["You", "Them"],
        elapsed_ms: 1_000,
        ended: null,
        banners: [],
        backends: [],
        words: 0,
        lag_seconds: null,
        transcript: "Live",
        call_end_offer: false,
        call_end_stops: false,
        progress: null,
      },
      behind: [],
    }));
    const { result, rerender } = renderHook(({ timer }) => useLiveMeeting(s.ipc, timer), {
      initialProps: { timer: createManualTimer(0) },
    });
    await act(async () => {});
    expect(result.current.meeting?.reloaded).toBe(true);
    s.emit(line);
    expect(result.current.meeting?.lines).toHaveLength(1);
    rerender({ timer: createManualTimer(0) });
    await act(async () => {});
    expect(result.current.meeting?.lines.map((l) => l.text)).toEqual(["An invented line."]);
    s.emit({ ...line, start_ms: 3_000, end_ms: 4_000, text: "A second invented line." } as MeetingEvent);
    expect(result.current.meeting?.lines).toHaveLength(2);
    expect(s.reads()).toBe(1);
  });
});