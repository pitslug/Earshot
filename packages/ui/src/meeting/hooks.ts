import { useCallback, useEffect, useRef, useState } from "react";

import type { Ipc, MeetingEvent } from "../ipc";
import type { Timer } from "../time";
import { type LiveMeeting, dismiss, reduce } from "./liveMeeting";

/** The injected clock's time, refreshed every `everyMs`. */
export function useNow(timer: Timer, everyMs: number): number {
  const [now, setNow] = useState(() => timer.now());
  useEffect(() => {
    setNow(timer.now());
    return timer.setInterval(() => setNow(timer.now()), everyMs);
  }, [timer, everyMs]);
  return now;
}

export interface LiveMeetingHandle {
  /** The current (or last) meeting, `null` before the first `Started`. */
  meeting: LiveMeeting | null;
  /** Hides a banner. */
  dismissBanner(key: string): void;
  /** The offer to stop was answered "Keep recording". */
  closeCallEndOffer(): void;
  /** Forgets the last meeting (before starting the next). */
  reset(): void;
}

/** Follows the app's meeting events. `onEvent` hears each event after it is applied. */
export function useLiveMeeting(
  ipc: Ipc,
  timer: Timer,
  onEvent?: (event: MeetingEvent) => void,
): LiveMeetingHandle {
  const [meeting, setMeeting] = useState<LiveMeeting | null>(null);
  const onEventRef = useRef(onEvent);
  onEventRef.current = onEvent;
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let live = true;
    ipc
      .onMeetingEvent((event) => {
        if (!live) return;
        setMeeting((state) => reduce(state, event, timer.now()));
        onEventRef.current?.(event);
      })
      .then((stop) => {
        if (live) unlisten = stop;
        else stop();
      });
    return () => {
      live = false;
      unlisten?.();
    };
  }, [ipc, timer]);
  const dismissBanner = useCallback(
    (key: string) => setMeeting((state) => (state ? dismiss(state, key) : state)),
    [],
  );
  const closeCallEndOffer = useCallback(
    () => setMeeting((state) => (state ? { ...state, callEndOffer: false } : state)),
    [],
  );
  const reset = useCallback(() => setMeeting(null), []);
  return { meeting, dismissBanner, closeCallEndOffer, reset };
}
