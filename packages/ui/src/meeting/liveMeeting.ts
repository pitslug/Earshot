// The meeting screen's state, built from the app's `MeetingEvent`s by a pure reducer, and
// the copy helpers the views share.

import type {
  CaptureMode,
  EndedReason,
  LiveTrack,
  MeetingEvent,
  PostStep,
  SummaryEvent,
  TranscriptState,
  WarningKind,
} from "../ipc";

export type Phase = "recording" | "processing" | "done";

export interface LandedLine {
  /** Arrival order (React key). */
  seq: number;
  track: LiveTrack;
  startMs: number;
  text: string;
}

/** A word still being guessed at (T9.1m): shown until the real decode covers it, never stored. */
export interface ProvisionalWord {
  startMs: number;
  text: string;
}

/** A provisional word starting up to this long before the end of settled text is kept: word times
 * move by this much between decodes (the runtime's `COVERED_SLACK`). */
export const COVERED_SLACK_MS = 100;

export interface Banner {
  key: string;
  kind: WarningKind;
  message: string;
  /** Arrival order of the latest warning with this key (newest first in the view). */
  seq: number;
}

export interface LiveMeeting {
  phase: Phase;
  meetingId: string;
  mode: CaptureMode;
  tracks: LiveTrack[];
  /** When `Started` arrived (the injected clock's ms), for the elapsed clock. */
  startedAt: number;
  /** When the recording ended (the injected clock's ms), freezing the clock. */
  endedAt: number | null;
  speaking: Record<LiveTrack, boolean>;
  backends: Partial<Record<LiveTrack, string>>;
  words: number;
  lagSeconds: number | null;
  /** Where the live transcript is; `Live` shows no banner. */
  transcript: TranscriptState;
  lines: LandedLine[];
  /** Per track, the unfinished stretch's provisional words, after its settled lines. */
  provisional: Record<LiveTrack, ProvisionalWord[]>;
  /** Live banners by key; dismissed ones are removed until their condition comes back. */
  banners: Banner[];
  /** The call seems to have ended and the question "stop recording?" is open. Only the user's
   * answer stops anything. */
  callEndOffer: boolean;
  /** The event count at the end of the recording: later banners are about what came after. */
  endedSeq: number | null;
  endedReason: EndedReason | null;
  progress: { step: PostStep; done: number; total: number | null } | null;
  summary: SummaryEvent | null;
  seq: number;
}

/** The state after `Started` (`now` from the injected clock). */
export function startedMeeting(
  meetingId: string,
  mode: CaptureMode,
  tracks: LiveTrack[],
  now: number,
): LiveMeeting {
  return {
    phase: "recording",
    meetingId,
    mode,
    tracks,
    startedAt: now,
    endedAt: null,
    speaking: { You: false, Them: false },
    backends: {},
    words: 0,
    lagSeconds: null,
    transcript: "Live",
    lines: [],
    provisional: { You: [], Them: [] },
    banners: [],
    callEndOffer: false,
    endedSeq: null,
    endedReason: null,
    progress: null,
    summary: null,
    seq: 0,
  };
}

/** The state after `event` at `now`. `Started` begins a new meeting; any other event before
 * one is ignored. */
export function reduce(
  state: LiveMeeting | null,
  event: MeetingEvent,
  now: number,
): LiveMeeting | null {
  if (event.type === "Started") return startedMeeting(event.meeting, event.mode, event.tracks, now);
  if (state === null) return null;
  // An older meeting finishing behind this one: only the list hears about it.
  if (
    (event.type === "Ended" || event.type === "PostProgress" || event.type === "Summary") &&
    event.meeting !== state.meetingId
  ) {
    return state;
  }
  const seq = state.seq + 1;
  switch (event.type) {
    case "Backend":
      return { ...state, backends: { ...state.backends, [event.track]: event.label } };
    case "Levels":
      // Not meeting state: the meters read levels from `useLiveMeeting`'s own store, so 10
      // events a second never render the meeting screen (T9.24).
      return state;
    case "VoiceActivity":
      return { ...state, speaking: { ...state.speaking, [event.track]: event.speaking } };
    case "Lag":
      return { ...state, lagSeconds: event.seconds };
    case "Words":
      return { ...state, words: event.total };
    case "Landed":
      return {
        ...state,
        seq,
        lines: [
          ...state.lines,
          { seq, track: event.track, startMs: event.start_ms, text: event.text },
        ],
        // In one step with the line, so a word is never shown twice or missing in between.
        provisional: {
          ...state.provisional,
          [event.track]: state.provisional[event.track].filter(
            (w) => w.startMs + COVERED_SLACK_MS >= event.end_ms,
          ),
        },
      };
    case "Provisional":
      return {
        ...state,
        provisional: {
          ...state.provisional,
          [event.track]: event.words.map((w) => ({ startMs: w.start_ms, text: w.text })),
        },
      };
    case "Transcript":
      return { ...state, transcript: event.state };
    case "Warning": {
      const banner: Banner = { key: event.key, kind: event.kind, message: event.message, seq };
      return { ...state, seq, banners: [...state.banners.filter((b) => b.key !== event.key), banner] };
    }
    case "CallEndOffer":
      return { ...state, callEndOffer: event.open };
    case "CallEndStopped":
      return { ...state, callEndOffer: false };
    case "Ended":
      return {
        ...state,
        phase: "processing",
        callEndOffer: false,
        endedAt: now,
        endedSeq: state.seq,
        endedReason: event.reason,
        speaking: { You: false, Them: false },
        provisional: { You: [], Them: [] },
      };
    case "PostProgress":
      return { ...state, progress: { step: event.step, done: event.done, total: event.total } };
    case "Summary":
      return { ...state, phase: "done", summary: event.result, endedAt: state.endedAt ?? now };
  }
}

/** Removes a banner (it comes back if its condition is reported again). */
export function dismiss(state: LiveMeeting, key: string): LiveMeeting {
  return { ...state, banners: state.banners.filter((b) => b.key !== key) };
}

/** Banners, newest first. */
export function bannersNewestFirst(state: LiveMeeting): Banner[] {
  return [...state.banners].sort((a, b) => b.seq - a.seq);
}

/** The banners about the work after the recording, newest first. Those from during it are done
 * with: the recording view that showed them has gone. */
export function afterMeetingBanners(state: LiveMeeting): Banner[] {
  const ended = state.endedSeq;
  return ended === null ? [] : bannersNewestFirst(state).filter((b) => b.seq > ended);
}

/** "m:ss", or "h:mm:ss" from an hour. */
export function clock(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const ss = String(s).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}

export const MODE_LABEL: Record<CaptureMode, string> = {
  Call: "Call: mic + computer audio",
  InRoom: "In the room: mic only",
};

/** The lowest level a meter shows (anything quieter is an empty meter). */
export const METER_FLOOR_DB = -60;

/** A meter's fill in 0..1 for a level in dBFS (`null`: no level yet). */
export function meterValue(dbfs: number | null): number {
  if (dbfs === null || !Number.isFinite(dbfs)) return 0;
  return Math.min(1, Math.max(0, (dbfs - METER_FLOOR_DB) / -METER_FLOOR_DB));
}

/** The banner at the top of the transcript: what is happening to it, `null` while it keeps up. */
export function transcriptBanner(
  state: TranscriptState,
): { title: string | null; body: string } | null {
  const recording = "Audio is recording.";
  switch (state) {
    case "Live":
      return null;
    case "WaitingForPreviousMeeting":
      return {
        title: recording,
        body: "Processing your previous meeting. Transcription will begin soon.",
      };
    case "WaitingForStart":
      return { title: recording, body: "Transcription will begin soon." };
    case "CatchingUp":
      return { title: null, body: "Catching up on the transcript" };
    case "Stopped":
      return {
        title: recording,
        body: "The live transcript has stopped for this meeting. The full transcript will be ready after the meeting ends.",
      };
  }
}

/** The "Running on" tile: one label, or one per track when they differ. */
export function runningOn(state: LiveMeeting): string {
  const labels = state.tracks
    .map((t) => [t, state.backends[t]] as const)
    .filter((e): e is readonly [LiveTrack, string] => e[1] !== undefined);
  if (labels.length === 0) {
    const waiting =
      state.transcript === "WaitingForPreviousMeeting" || state.transcript === "WaitingForStart";
    return waiting ? "Waiting" : "Starting";
  }
  if (labels.every(([, l]) => l === labels[0][1])) return labels[0][1];
  return labels.map(([t, l]) => `${t}: ${l}`).join(", ");
}

/** Words for the tile: "3,412". */
export function wordCount(n: number): string {
  return n.toLocaleString("en-AU");
}

/** Transcript lag for the tile: "38 s", or a dash before the first report. */
export function lagText(seconds: number | null): string {
  return seconds === null ? "-" : `${Math.round(seconds)} s`;
}

function whose(track: LiveTrack): string {
  return track === "You" ? "you" : "them";
}

/** One line of copy for a post-meeting step. */
export function stepLabel(step: PostStep): string {
  switch (step.kind) {
    case "Loading":
      return "Reading the recordings";
    case "Starting":
      return "Loading the models";
    case "Transcribed":
      return `Finished the transcript for ${whose(step.track)}`;
    case "Diarised":
      return `Found the voices for ${whose(step.track)}`;
    case "EchoChecked":
      return "Removed echo from your mic";
    case "Stored":
      return "Saved the final transcript";
    case "Summarising":
      return "Writing the summary";
  }
}

/** How close to the end of the transcript still counts as being at it (px). */
export const FOLLOW_SLACK = 24;

/** Whether a scrolled transcript is at its end, so new lines should keep it there. */
export function atTheEnd(scrollTop: number, clientHeight: number, scrollHeight: number): boolean {
  return scrollHeight - scrollTop - clientHeight <= FOLLOW_SLACK;
}
