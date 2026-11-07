// The audio check's state on the Audio page (T7.4j): the stored result for the devices a
// meeting would use now, and the running check as its events report it. The only
// subscriber to `AudioCheckEvent`. Everything is decided by the app; this keeps what it said.

import { useCallback, useEffect, useState } from "react";

import { type AudioCheckEvent, type AudioCheckView, useIpc } from "../ipc";
import { errorText } from "./useModels";

/** The check as last reported. */
export type CheckRun =
  | { status: "idle" }
  | { status: "starting" }
  | { status: "listening"; heardMs: number; totalMs: number }
  | { status: "done"; note: string | null }
  | { status: "cancelled" }
  | { status: "failed"; message: string };

export interface AudioCheckState {
  /** The stored result part (null while loading or when it could not be read). */
  view: AudioCheckView | null;
  run: CheckRun;
  /** Why the check did not start, until the next try. */
  notice: string | null;
  /** Starts the check (the user was warned that it plays sound). */
  start(): Promise<void>;
  cancel(): Promise<void>;
}

/** Whether a check is under way. */
export function checkRunning(r: CheckRun): boolean {
  return r.status === "starting" || r.status === "listening";
}

/** Follows the audio check; `devicesKey` changes when the chosen devices do (the stored
 * result shown is the one of the devices in use). */
export function useAudioCheck(devicesKey: string): AudioCheckState {
  const ipc = useIpc();
  const [view, setView] = useState<AudioCheckView | null>(null);
  const [run, setRun] = useState<CheckRun>({ status: "idle" });
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    let stop: (() => void) | null = null;
    const onEvent = (e: AudioCheckEvent) => {
      if (!live) return;
      switch (e.type) {
        case "Listening":
          setRun({ status: "listening", heardMs: e.heard_ms, totalMs: e.total_ms });
          break;
        case "Done":
          setView(e.view);
          setRun({ status: "done", note: e.note });
          break;
        case "Cancelled":
          setRun({ status: "cancelled" });
          break;
        case "Failed":
          setRun({ status: "failed", message: e.message });
          break;
      }
    };
    void ipc.onAudioCheckEvent(onEvent).then((s) => {
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
    void ipc.audioCheck().then(
      (v) => {
        if (live) setView(v);
      },
      () => {
        if (live) setView(null);
      },
    );
    return () => {
      live = false;
    };
  }, [ipc, devicesKey]);

  const start = useCallback(async () => {
    setNotice(null);
    setRun({ status: "starting" });
    try {
      await ipc.startAudioCheck();
    } catch (e) {
      setRun({ status: "idle" });
      setNotice(errorText(e));
    }
  }, [ipc]);

  const cancel = useCallback(async () => {
    await ipc.cancelAudioCheck();
  }, [ipc]);

  return { view, run, notice, start, cancel };
}
