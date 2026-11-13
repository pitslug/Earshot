// `useLiveMeeting` keeps the meters' meeting across a re-run of its event effect (T9.24 review).

import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { Ipc, MeetingEvent } from "../ipc";
import { createManualTimer } from "../time";
import { useLiveMeeting } from "./hooks";

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
