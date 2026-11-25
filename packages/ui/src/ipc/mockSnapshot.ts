// The mock's copy of the app's record of what it sent the window (`meeting_events::SentRecord`,
// T14.51): every event the mock sends is folded in first, and `meetingSnapshot` answers from it,
// so a window that subscribes late sees what the app would show it. As in the app, each event
// comes with the number of the meeting that sent it (its start count): `Started`, `Ended`,
// `PostProgress` and `Summary` name their meeting, every other event belongs to the meeting of its
// number. Both copies run `fixtures/ui/meeting_snapshot_cases.json`, so they change together.

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
  seq: number;
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
  callEndStops: boolean;
  progress: ProgressSnapshot | null;
}

export interface SentRecord {
  /** Folds in `event` of meeting number `seq`, sent at `now` (the injected clock's ms). */
  apply(seq: number, event: MeetingEvent, now: number): void;
  /** The snapshot at `now`. */
  snapshot(now: number): MeetingSnapshot;
}

export function createSentRecord(): SentRecord {
  let meetings: Sent[] = [];
  // The number of the newest `Started`.
  let followed: number | null = null;
  const byId = (id: string) => meetings.find((m) => m.meeting === id);
  return {
    apply(seq, event, now) {
      switch (event.type) {
        case "Started":
          meetings = meetings.filter((m) => m.seq !== seq && m.meeting !== event.meeting);
          meetings.push({
            seq,
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
            callEndStops: false,
            progress: null,
          });
          followed = seq;
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
      const m = meetings.find((x) => x.seq === seq);
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
          // As last sent: re-sent open with `stops: false` when the setting is turned off.
          m.callEndOffer = event.open;
          m.callEndStops = event.stops;
          return;
        case "CallEndStopped":
          m.callEndOffer = false;
          return;
        default:
          // Levels, voice activity and transcript text are not kept.
          return;
      }
    },
    snapshot(now) {
      const f = followed === null ? undefined : meetings.find((m) => m.seq === followed);
      return {
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
                call_end_stops: f.callEndStops,
                progress: f.progress,
              },
        behind: meetings
          .filter((m) => m.seq !== followed && m.ended !== null)
          .sort((a, b) => a.seq - b.seq)
          .map((m) => ({ meeting: m.meeting, progress: m.progress })),
      };
    },
  };
}
