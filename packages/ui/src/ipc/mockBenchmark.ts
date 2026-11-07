// The Benchmark part of the mock `Ipc` (T7.4c): a scripted run on the timer, with the page's
// words of `benchmark::result_view` in the desktop app. The mock replays the meeting faster
// than real time (each step is a few seconds of meeting) so the preview does not take
// minutes.

import type { BenchmarkEvent, BenchmarkResultView, BenchmarkRunning, BenchmarkView } from "./benchmark";
import { IpcFailure } from "./ipc";
import type { Timer } from "../time";

/** How a scripted run ends. */
export type MockBenchmarkEnd = "KeptUp" | "FellBehind" | "NoSummary" | "Failed";

/** What the benchmark mock starts with. */
export interface BenchmarkMockData {
  /** How the scripted run ends (default `KeptUp`). */
  end?: MockBenchmarkEnd;
  /** `startBenchmark` rejects with this (`busy` while a meeting runs, say). */
  startError?: { kind: string; message: string; retryable: boolean };
  /** A result stored before the page opens. */
  stored?: BenchmarkResultView;
  /** A stored result that this version cannot read. */
  unreadable?: boolean;
  /** A run already under way when the page opens (it stays there until cancelled). */
  running?: BenchmarkRunning;
}

/** The bundled meeting's length (2 min 30 s). */
export const BENCHMARK_MEETING_MS = 150_000;
/** The mock's timer step. */
export const BENCHMARK_STEP_MS = 250;
/** Meeting time replayed per mock step. */
const REPLAYED_PER_STEP_MS = 10_000;
/** The message of a scripted failure. */
export const MOCK_BENCHMARK_FAILURE =
  "A model these settings use is not downloaded. Download it on the Models page, then run the benchmark again.";
/** The meeting's attribution (`benchmark::MEETING_ATTRIBUTION`). */
export const BENCHMARK_ATTRIBUTION =
  "The benchmark meeting is an excerpt (9:30 to 12:00) of meeting ES2004a of the AMI Meeting Corpus, University of Edinburgh and the AMI Consortium, licensed under CC BY 4.0 (groups.inf.ed.ac.uk/ami/corpus).";

/** A result as the app would show it. */
export function mockBenchmarkResult(end: Exclude<MockBenchmarkEnd, "Failed">, at: number): BenchmarkResultView {
  const keptUp = end !== "FellBehind";
  const summary = end !== "NoSummary";
  return {
    ran_at_ms: at,
    app_version: "0.0.0-preview",
    kept_up: keptUp,
    headline: !keptUp
      ? "Live transcription fell behind. A lighter model, or another part on the Models page, may keep up."
      : summary
        ? "Live transcription kept up, and the summary was ready 48 seconds after the meeting ended."
        : "Live transcription kept up.",
    settings: [
      { label: "Transcription", value: "Parakeet 0.6B v2 on the NPU" },
      {
        label: "Summary",
        value: summary ? "Qwen 3.5 4B on the processor" : "None (Summaries is set to no summary)",
      },
      { label: "Summarising during the meeting", value: "Off" },
      { label: "Speaker detection", value: "During the call" },
    ],
    timings: [
      {
        label: "Transcript lag during the meeting (both tracks)",
        value: keptUp ? "Usually 4.3 seconds behind, at most 12 seconds" : "Usually 38 seconds behind, at most 71 seconds",
      },
      {
        label: "Live transcript",
        value: keptUp
          ? "Kept up: complete 2.1 seconds after the meeting ended"
          : "Fell behind: complete 74 seconds after the meeting ended",
      },
      { label: "Final transcript", value: "Ready 9.4 seconds after the meeting ended on the NPU" },
      { label: "Summary", value: summary ? "Ready 48 seconds after the meeting ended" : "Not run" },
      {
        label: "Model check",
        value: keptUp ? "Found Parakeet 0.6B v2 fast enough on the NPU" : "Has not tried Parakeet 0.6B v2 on the NPU",
      },
    ],
    notes: [],
  };
}

/** The Benchmark methods of the mock. */
export interface BenchmarkMock {
  benchmark(): Promise<BenchmarkView>;
  startBenchmark(): Promise<void>;
  cancelBenchmark(): Promise<void>;
  onBenchmarkEvent(handler: (event: BenchmarkEvent) => void): Promise<() => void>;
}

/** The Benchmark part of the mock `Ipc`. */
export function createBenchmarkMock(data: BenchmarkMockData = {}, timer: Timer): BenchmarkMock {
  let last: BenchmarkResultView | null = data.stored === undefined ? null : structuredClone(data.stored);
  let unreadable = data.unreadable === true;
  const listeners = new Set<(event: BenchmarkEvent) => void>();
  const emit = (event: BenchmarkEvent) => {
    for (const l of [...listeners]) l(event);
  };
  let running: (() => void) | null = data.running === undefined ? null : () => undefined;
  let current: BenchmarkRunning | null = data.running ?? null;
  const view = (): BenchmarkView => ({
    last: last === null ? null : structuredClone(last),
    unreadable,
    meeting_ms: BENCHMARK_MEETING_MS,
    attribution: BENCHMARK_ATTRIBUTION,
    running: current,
  });

  return {
    benchmark: async () => view(),
    startBenchmark: async () => {
      if (data.startError) throw new IpcFailure(data.startError);
      if (running !== null) {
        throw new IpcFailure({ kind: "busy", message: "The benchmark is already running.", retryable: true });
      }
      const end = data.end ?? "KeptUp";
      let heard = 0;
      let after = 0;
      const step = (phase: BenchmarkRunning["phase"], heardMs: number) => {
        current = { phase, heard_ms: heardMs, total_ms: BENCHMARK_MEETING_MS };
        emit({ type: "Progress", ...current });
      };
      step("Replaying", 0);
      running = timer.setInterval(() => {
        if (heard < BENCHMARK_MEETING_MS) {
          heard = Math.min(heard + REPLAYED_PER_STEP_MS, BENCHMARK_MEETING_MS);
          step("Replaying", heard);
          return;
        }
        after += 1;
        if (after === 1) {
          step("FinishingLive", BENCHMARK_MEETING_MS);
          return;
        }
        if (after === 2) {
          step("Transcribing", BENCHMARK_MEETING_MS);
          return;
        }
        if (after === 3 && end !== "NoSummary" && end !== "Failed") {
          step("Summarising", BENCHMARK_MEETING_MS);
          return;
        }
        running?.();
        running = null;
        current = null;
        if (end === "Failed") {
          emit({ type: "Failed", message: MOCK_BENCHMARK_FAILURE });
          return;
        }
        last = mockBenchmarkResult(end, timer.now());
        unreadable = false;
        emit({ type: "Done", view: view() });
      }, BENCHMARK_STEP_MS);
    },
    cancelBenchmark: async () => {
      if (running === null) return;
      running();
      running = null;
      current = null;
      emit({ type: "Cancelled" });
    },
    onBenchmarkEvent: async (handler) => {
      listeners.add(handler);
      return () => {
        listeners.delete(handler);
      };
    },
  };
}
