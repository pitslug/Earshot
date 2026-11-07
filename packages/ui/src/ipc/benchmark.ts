// The Benchmark page commands (T7.4c), kept apart from ipc.ts so `IpcCommands` gains them in
// one line.

import type {
  BenchmarkEvent,
  BenchmarkPhase,
  BenchmarkResultView,
  BenchmarkRow,
  BenchmarkRunning,
  BenchmarkView,
} from "./bindings";

export type { BenchmarkEvent, BenchmarkPhase, BenchmarkResultView, BenchmarkRow, BenchmarkRunning, BenchmarkView };

/** The Benchmark page part of `IpcCommands`. */
export interface BenchmarkCommands {
  /** The latest benchmark result, the benchmark meeting's length, its attribution and the
   * run under way. */
  benchmark(): Promise<BenchmarkView>;
  /** Starts the benchmark with the current settings; `BenchmarkEvent`s follow. Rejects with
   * `busy` while a meeting, a meeting job, the model check, the voice sample, the audio check
   * or another benchmark runs. */
  startBenchmark(): Promise<void>;
  /** Cancels the benchmark; nothing is stored. */
  cancelBenchmark(): Promise<void>;
}
