// The Benchmark page's state (T7.4c): the stored result and the running benchmark as its
// events report it. The only subscriber to `BenchmarkEvent`. The app measures and words the
// result; this keeps what it said.

import { useCallback, useEffect, useState } from "react";

import { type BenchmarkEvent, type BenchmarkPhase, type BenchmarkView, useIpc } from "../ipc";
import { errorText } from "./useModels";

/** The run as last reported. */
export type BenchmarkRun =
  | { status: "idle" }
  | { status: "starting" }
  | { status: "running"; phase: BenchmarkPhase; heardMs: number; totalMs: number }
  | { status: "done" }
  | { status: "cancelling" }
  | { status: "cancelled" }
  | { status: "failed"; message: string };

/** The page's data. */
export type BenchmarkLoad =
  | { status: "loading" }
  | { status: "failed"; message: string }
  | { status: "ready"; view: BenchmarkView };

export interface BenchmarkState {
  load: BenchmarkLoad;
  run: BenchmarkRun;
  /** Why the benchmark did not start, until the next try. */
  notice: string | null;
  start(): Promise<void>;
  cancel(): Promise<void>;
}

/** Whether a run is under way. */
export function benchmarkRunning(r: BenchmarkRun): boolean {
  return r.status === "starting" || r.status === "running" || r.status === "cancelling";
}

/** Follows the benchmark. */
export function useBenchmark(): BenchmarkState {
  const ipc = useIpc();
  const [load, setLoad] = useState<BenchmarkLoad>({ status: "loading" });
  const [run, setRun] = useState<BenchmarkRun>({ status: "idle" });
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    let stop: (() => void) | null = null;
    const onEvent = (e: BenchmarkEvent) => {
      if (!live) return;
      switch (e.type) {
        case "Progress":
          setRun((r) =>
            r.status === "cancelling"
              ? r
              : {
                  status: "running",
                  phase: e.phase,
                  heardMs: e.heard_ms,
                  totalMs: e.total_ms,
                },
          );
          break;
        case "Done":
          setLoad({ status: "ready", view: e.view });
          setRun({ status: "done" });
          break;
        case "Cancelled":
          setRun({ status: "cancelled" });
          break;
        case "Failed":
          setRun({ status: "failed", message: e.message });
          break;
      }
    };
    void ipc.onBenchmarkEvent(onEvent).then((s) => {
      if (live) stop = s;
      else s();
    });
    return () => {
      live = false;
      stop?.();
    };
  }, [ipc]);

  useEffect(() => {
    let live = true;
    void ipc.benchmark().then(
      (view) => {
        if (!live) return;
        setLoad({ status: "ready", view });
        // A page opened during a run shows it, and can cancel it.
        const now = view.running;
        if (now !== null) {
          setRun((r) =>
            r.status === "idle"
              ? { status: "running", phase: now.phase, heardMs: now.heard_ms, totalMs: now.total_ms }
              : r,
          );
        }
      },
      (e: unknown) => {
        if (live) setLoad({ status: "failed", message: errorText(e) });
      },
    );
    return () => {
      live = false;
    };
  }, [ipc]);

  const start = useCallback(async () => {
    setNotice(null);
    setRun({ status: "starting" });
    try {
      await ipc.startBenchmark();
    } catch (e) {
      setRun({ status: "idle" });
      setNotice(errorText(e));
    }
  }, [ipc]);

  const cancel = useCallback(async () => {
    setRun((r) => (benchmarkRunning(r) ? { status: "cancelling" } : r));
    await ipc.cancelBenchmark();
  }, [ipc]);

  return { load, run, notice, start, cancel };
}
