import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";

import type { Ipc, LiveTrack, MeetingEvent, MeetingSnapshot } from "../ipc";
import type { Timer } from "../time";
import { clearDismissed, dismissedOn, forgetDismissed, rememberDismissed } from "./dismissedBanners";
import { type LiveMeeting, dismiss, fromSnapshot, reduce } from "./liveMeeting";

/** The injected clock's time, refreshed every `everyMs`. Only a leaf that shows the time
 * should hold it: every tick renders the component that calls this (T9.24). */
export function useNow(timer: Timer, everyMs: number): number {
  const [now, setNow] = useState(() => timer.now());
  useEffect(() => {
    setNow(timer.now());
    return timer.setInterval(() => setNow(timer.now()), everyMs);
  }, [timer, everyMs]);
  return now;
}

/** The local calendar day of `now()` (its midnight), checked once a minute: a component holding
 * it renders when the day changes, not on every tick of the clock (T9.24). */
export function useLocalDay(timer: Timer, now: () => number): number {
  const [day, setDay] = useState(() => dayOf(now()));
  const nowRef = useRef(now);
  nowRef.current = now;
  useEffect(() => {
    const check = () => setDay(dayOf(nowRef.current()));
    check();
    return timer.setInterval(check, DAY_CHECK_MS);
  }, [timer]);
  return day;
}

/** How often [`useLocalDay`] looks at the clock. */
export const DAY_CHECK_MS = 60_000;

/** The local midnight starting the day of `ms`. */
function dayOf(ms: number): number {
  const d = new Date(ms);
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
}

/** Each track's latest level in dBFS (`null`: none yet, or the recording ended). */
export type Levels = Record<LiveTrack, number | null>;

const NO_LEVELS: Levels = { You: null, Them: null };

/** The meters' levels, kept outside React state: 10 `Levels` events a second render only the
 * meters that subscribe ([`useLevel`]), never the meeting screen (T9.24). */
export interface LiveLevels {
  get(): Levels;
  subscribe(listener: () => void): () => void;
}

/** A store of levels that the meeting's events write. */
export function createLevels(): LiveLevels & { set(levels: Levels): void } {
  let current = NO_LEVELS;
  const listeners = new Set<() => void>();
  return {
    get: () => current,
    set(levels) {
      if (levels.You === current.You && levels.Them === current.Them) return;
      current = levels;
      for (const l of [...listeners]) l();
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

/** Levels that never move (a view shown without a meeting's events). */
export const QUIET_LEVELS: LiveLevels = {
  get: () => NO_LEVELS,
  subscribe: () => () => {},
};

/** `track`'s latest level; renders the caller only when it changes. */
export function useLevel(levels: LiveLevels, track: LiveTrack): number | null {
  return useSyncExternalStore(
    levels.subscribe,
    () => levels.get()[track],
    () => levels.get()[track],
  );
}

export interface LiveMeetingHandle {
  /** The current (or last) meeting, `null` before the first `Started`. */
  meeting: LiveMeeting | null;
  /** The meters' levels, apart from `meeting` so they render only the meters. */
  levels: LiveLevels;
  /** Hides a banner. */
  dismissBanner(key: string): void;
  /** The offer to stop was answered "Keep recording". */
  closeCallEndOffer(): void;
  /** Forgets the last meeting (before starting the next). */
  reset(): void;
}

/** Follows the app's meeting events. `onEvent` hears each event after it is applied.
 *
 * A window can load while a meeting records or finishes (a reload, T14.51), so the hook starts
 * from the app's snapshot: it subscribes first, then reads `meetingSnapshot`, holding the events
 * it hears meanwhile; it seeds the meeting from the snapshot (`fromSnapshot`), tells
 * `onSnapshot`, then applies every held event in order. The app folds each event into its
 * snapshot before sending it, so nothing sent before the subscription is missed; an event both in
 * the snapshot and held sets again what it already set (every snapshot field is set by
 * assignment, a banner by its key, and lines are not in the snapshot). A snapshot that cannot be
 * read seeds nothing: the held events still apply. */
export function useLiveMeeting(
  ipc: Ipc,
  timer: Timer,
  onEvent?: (event: MeetingEvent) => void,
  onSnapshot?: (snapshot: MeetingSnapshot) => void,
): LiveMeetingHandle {
  const [meeting, setMeeting] = useState<LiveMeeting | null>(null);
  const [levels] = useState(createLevels);
  const onEventRef = useRef(onEvent);
  onEventRef.current = onEvent;
  const onSnapshotRef = useRef(onSnapshot);
  onSnapshotRef.current = onSnapshot;
  // The meeting the meters belong to: an older meeting's end leaves them alone. A ref, so a
  // re-run of the effect below mid-meeting keeps it.
  const metering = useRef<string | null>(null);
  // The meeting the screen follows, whose dismissed banners are remembered across a reload.
  const followed = useRef<string | null>(null);
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let live = true;
    // Events heard before the snapshot is read, in order; `null` once it was.
    let held: MeetingEvent[] | null = [];
    const apply = (event: MeetingEvent) => {
      if (event.type === "Levels") {
        // Not meeting state: only the meters hear it (T9.24).
        if (metering.current !== null) levels.set({ You: event.you_dbfs, Them: event.them_dbfs });
        onEventRef.current?.(event);
        return;
      }
      if (event.type === "Started") {
        metering.current = event.meeting;
        levels.set(NO_LEVELS);
        followed.current = event.meeting;
        clearDismissed(event.meeting);
      }
      // Sent again: it shows, also after a later reload.
      if (event.type === "Warning" && followed.current !== null) forgetDismissed(followed.current, event.key);
      if (event.type === "Summary" && event.meeting === followed.current) clearDismissed();
      if (event.type === "Ended" && event.meeting === metering.current) {
        metering.current = null;
        levels.set(NO_LEVELS);
      }
      setMeeting((state) => reduce(state, event, timer.now()));
      onEventRef.current?.(event);
    };
    const seed = (snapshot: MeetingSnapshot | null) => {
      if (!live) return;
      const f = snapshot?.followed ?? null;
      if (snapshot !== null && f !== null) {
        const now = timer.now();
        metering.current = f.ended === null ? f.meeting : null;
        followed.current = f.meeting;
        // Banners the user dismissed before the reload stay dismissed.
        const dismissed = dismissedOn(f.meeting);
        clearDismissed(f.meeting);
        const shown = { ...f, banners: f.banners.filter((b) => !dismissed.has(b.key)) };
        // A start heard meanwhile is newer than the snapshot: it replaces this when applied.
        setMeeting(fromSnapshot(shown, now));
      } else if (snapshot !== null) {
        // Nothing in progress: what was dismissed is about a meeting that has ended.
        clearDismissed();
      }
      if (snapshot !== null) onSnapshotRef.current?.(snapshot);
      const events = held ?? [];
      held = null;
      for (const event of events) apply(event);
    };
    ipc
      .onMeetingEvent((event) => {
        if (!live) return;
        if (held !== null) {
          held.push(event);
          return;
        }
        apply(event);
      })
      .then((stop) => {
        if (!live) {
          stop();
          return;
        }
        unlisten = stop;
        // Read only once subscribed: whatever is sent from here on is heard.
        Promise.resolve()
          .then(() => ipc.meetingSnapshot())
          .then(seed, () => seed(null));
      });
    return () => {
      live = false;
      unlisten?.();
    };
  }, [ipc, timer, levels]);
  const dismissBanner = useCallback((key: string) => {
    if (followed.current !== null) rememberDismissed(followed.current, key);
    setMeeting((state) => (state ? dismiss(state, key) : state));
  }, []);
  const closeCallEndOffer = useCallback(
    () => setMeeting((state) => (state ? { ...state, callEndOffer: false } : state)),
    [],
  );
  const reset = useCallback(() => setMeeting(null), []);
  return { meeting, levels, dismissBanner, closeCallEndOffer, reset };
}
