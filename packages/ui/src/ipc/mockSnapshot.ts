// The mock's copy of the app's record of what it sent the window (`meeting_events::SentRecord`,
// T14.51): every event the mock sends is folded in first, and `meetingSnapshot` answers from it,
// so a window that subscribes late sees what the app would show it. Events that name no meeting
// belong to the newest one started, as the app sends them only for that one.

import type {
  BannerSnapshot,
  CaptureMode,
  EndedReason,
  LiveTrack,
  MeetingEvent,
  MeetingSnapshot,
  ProgressSnapshot,
  TrackBackend,
  TranscriptState,
} from "./bindings";

interface Sent {
  meeting: string;
  mode: CaptureMode;
  tracks: LiveTrack[];
  started: number;
  ended: { at: number; reason: EndedReason } | null;
  banners: BannerSnapshot[];
  backends: TrackBackend[];
  words: number;
  lagSeconds: number | null;
  transcript: TranscriptState;
  callEndOffer: boolean;
  progress: ProgressSnapshot | null;
}

export interface SentRecord {
  /** Folds in `event`, sent at `now` (the injected clock's ms). */
  apply(event: MeetingEvent, now: number): void;
  /** The snapshot at `now`. */
  snapshot(starting: boolean, now: number): MeetingSnapshot;
  /** Whether `meeting`'s `Started` was sent. */
  started(meeting: string): boolean;
}

export function createSentRecord(): SentRecord {
  // Oldest first; the last one is the meeting the screen follows, unless its result removed it.
  let meetings: Sent[] = [];
  let followed: string | null = null;
  const byId = (id: string) => meetings.find((m) => m.meeting === id);
  return {
    apply(event, now) {
      switch (event.type) {
        case "Started":
          meetings = meetings.filter((m) => m.meeting !== event.meeting);
          meetings.push({
            meeting: event.meeting,
            mode: event.mode,
            tracks: [...event.tracks],
            started: now,
            ended: null,
            banners: [],
            backends: [],
            words: 0,
            lagSeconds: null,
            transcript: "Live",
            callEndOffer: false,
            progress: null,
          });
          followed = event.meeting;
          return;
        case "Ended": {
          const m = byId(event.meeting);
          if (m) {
            m.ended = { at: now, reason: event.reason };
            m.callEndOffer = false;
          }
          return;
        }
        case "PostProgress": {
          const m = byId(event.meeting);
          if (m) m.progress = { step: event.step, done: event.done, total: event.total };
          return;
        }
        case "Summary":
          meetings = meetings.filter((m) => m.meeting !== event.meeting);
          return;
      }
      const m = followed === null ? undefined : byId(followed);
      if (!m) return;
      switch (event.type) {
        case "Backend":
          m.backends = [...m.backends.filter((b) => b.track !== event.track), { track: event.track, label: event.label }];
          return;
        case "Lag":
          m.lagSeconds = event.seconds;
          return;
        case "Words":
          m.words = event.total;
          return;
        case "Transcript":
          m.transcript = event.state;
          return;
        case "Warning":
          m.banners = [
            ...m.banners.filter((b) => b.key !== event.key),
            { key: event.key, kind: event.kind, message: event.message, after_end: m.ended !== null },
          ];
          return;
        case "WarningCleared":
          m.banners = m.banners.filter((b) => b.key !== event.key);
          return;
        case "CallEndOffer":
          m.callEndOffer = event.open;
          return;
        case "CallEndStopped":
          m.callEndOffer = false;
          return;
        default:
          // Levels, voice activity and transcript text are not kept.
          return;
      }
    },
    snapshot(starting, now) {
      const f = followed === null ? undefined : byId(followed);
      return {
        starting,
        followed:
          f === undefined
            ? null
            : {
                meeting: f.meeting,
                mode: f.mode,
                tracks: [...f.tracks],
                elapsed_ms: Math.max(0, (f.ended?.at ?? now) - f.started),
                ended: f.ended?.reason ?? null,
                banners: f.banners.map((b) => ({ ...b })),
                backends: f.backends.map((b) => ({ ...b })),
                words: f.words,
                lag_seconds: f.lagSeconds,
                transcript: f.transcript,
                call_end_offer: f.callEndOffer,
                progress: f.progress,
              },
        behind: meetings
          .filter((m) => m.meeting !== followed && m.ended !== null)
          .map((m) => ({ meeting: m.meeting, progress: m.progress })),
      };
    },
    started: (meeting) => byId(meeting) !== undefined,
  };
}
