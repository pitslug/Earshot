// The audio check part of the mock `Ipc` (T7.4j): a scripted check on the timer, with the
// page's words of `audio_check::result_view` in the desktop app. Results are kept per
// microphone + output pair, by the devices a meeting would use now (the Audio mock says
// which), as the app stores them.

import type { AudioCheckEvent, AudioCheckOutcome, AudioCheckResultView, AudioCheckView, DeviceItem } from "./ipc";
import { IpcFailure } from "./ipc";
import type { Timer } from "../time";

/** How a scripted check ends. */
export type MockCheckEnd = AudioCheckOutcome | "Failed" | "TooQuiet" | "Clipping";

/** What the audio check mock starts with. */
export interface AudioCheckMockData {
  /** How the scripted check ends (default `Echo`). */
  end?: MockCheckEnd;
  /** `startAudioCheck` rejects with this (`busy` while a meeting runs, say). */
  startError?: { kind: string; message: string; retryable: boolean };
  /** A result stored before the page opens, for the devices in use. */
  stored?: AudioCheckResultView;
}

/** The check's usual length (`audio_check::check_length`: 3.2 s of sounds + 1.3 s). */
export const CHECK_TOTAL_MS = 4_500;
/** Progress step of the mock (`audio_check::PROGRESS_STEP`). */
export const CHECK_STEP_MS = 250;
/** The message of a scripted failure. */
export const MOCK_CHECK_FAILURE =
  "The audio check did not finish: the sounds could not be played: the output is in use by another app.";

/** The headline of each outcome (`audio_check::result_view`). */
export const CHECK_HEADLINE: Record<AudioCheckOutcome, string> = {
  Echo: "Your microphone hears your speakers.",
  Headphones:
    "Your microphone did not hear the sounds: you are probably using headphones, or the speaker volume is off.",
  NotPlayed: "No sound reached the output. Check that it is not muted, then run the check again.",
  MicSilent:
    "Your microphone sent only silence. Check that it is not muted or switched off, then run the check again.",
};

/** The line when the delay is used (`audio_check::result_view`). */
export const DELAY_USED_LINE =
  "Meetings with this microphone and output use the delay to start cancelling echo sooner.";
/** The too quiet line. */
export const TOO_QUIET_LINE = "Too quiet: the microphone heard the sounds only faintly, so the delay is not used.";
/** The clipping line. */
export const CLIPPING_LINE =
  "Clipping: the microphone is too loud and distorts. Lower its input level in the Windows sound settings.";

/** A result as the app would show it. */
export function mockResult(
  end: Exclude<MockCheckEnd, "Failed">,
  microphone: string,
  output: string,
  at: number,
): AudioCheckResultView {
  const outcome: AudioCheckOutcome = end === "TooQuiet" || end === "Clipping" ? "Echo" : end;
  const tooQuiet = end === "TooQuiet" || end === "MicSilent";
  const clipping = end === "Clipping";
  const echo = outcome === "Echo";
  const delay = echo ? 52 : null;
  const quieter = echo ? (tooQuiet ? 41 : 18) : null;
  const used = echo && !tooQuiet;
  const details: string[] = [];
  if (delay !== null && quieter !== null) {
    details.push(`Delay from speaker to microphone: ${delay} ms.`);
    details.push(`Echo level: ${quieter} dB quieter than the sound played.`);
  }
  if (echo && tooQuiet) details.push(TOO_QUIET_LINE);
  if (clipping) details.push(CLIPPING_LINE);
  if (used) details.push(DELAY_USED_LINE);
  return {
    microphone,
    output,
    checked_at_ms: at,
    outcome,
    delay_ms: delay,
    echo_quieter_db: quieter,
    too_quiet: tooQuiet,
    clipping,
    delay_used: used,
    headline: CHECK_HEADLINE[outcome],
    details,
  };
}

/** The audio check methods of the mock. */
export interface AudioCheckMock {
  audioCheck(): Promise<AudioCheckView>;
  startAudioCheck(): Promise<void>;
  cancelAudioCheck(): Promise<void>;
  onAudioCheckEvent(handler: (event: AudioCheckEvent) => void): Promise<() => void>;
}

/** The audio check part of the mock `Ipc`; `inUse` names the devices a meeting would use. */
export function createAudioCheckMock(
  data: AudioCheckMockData = {},
  timer: Timer,
  inUse: () => { microphone: DeviceItem | undefined; output: DeviceItem | undefined },
): AudioCheckMock {
  const results = new Map<string, AudioCheckResultView>();
  const key = () => {
    const { microphone, output } = inUse();
    return microphone === undefined || output === undefined ? null : `${microphone.id}|${output.id}`;
  };
  const first = key();
  if (data.stored !== undefined && first !== null) results.set(first, { ...data.stored });
  const listeners = new Set<(event: AudioCheckEvent) => void>();
  const emit = (event: AudioCheckEvent) => {
    for (const l of [...listeners]) l(event);
  };
  let running: (() => void) | null = null;
  const view = (): AudioCheckView => {
    const k = key();
    const r = k === null ? undefined : results.get(k);
    return { result: r === undefined ? null : { ...r, details: [...r.details] }, unreadable: false };
  };

  return {
    audioCheck: async () => view(),
    startAudioCheck: async () => {
      if (data.startError) throw new IpcFailure(data.startError);
      if (running !== null) throw new IpcFailure({ kind: "busy", message: "The audio check is already running.", retryable: true });
      let heard = 0;
      emit({ type: "Listening", heard_ms: 0, total_ms: CHECK_TOTAL_MS });
      running = timer.setInterval(() => {
        heard += CHECK_STEP_MS;
        emit({ type: "Listening", heard_ms: Math.min(heard, CHECK_TOTAL_MS), total_ms: CHECK_TOTAL_MS });
        if (heard < CHECK_TOTAL_MS) return;
        running?.();
        running = null;
        const end = data.end ?? "Echo";
        if (end === "Failed") {
          emit({ type: "Failed", message: MOCK_CHECK_FAILURE });
          return;
        }
        const { microphone, output } = inUse();
        const k = key();
        if (k !== null && microphone !== undefined && output !== undefined) {
          results.set(k, mockResult(end, microphone.name, output.name, timer.now()));
        }
        emit({ type: "Done", view: view(), note: null });
      }, CHECK_STEP_MS);
    },
    cancelAudioCheck: async () => {
      if (running === null) return;
      running();
      running = null;
      emit({ type: "Cancelled" });
    },
    onAudioCheckEvent: async (handler) => {
      listeners.add(handler);
      return () => {
        listeners.delete(handler);
      };
    },
  };
}
