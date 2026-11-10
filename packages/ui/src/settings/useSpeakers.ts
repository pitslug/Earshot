// The Speakers page's state (T7.4h): the speaker detection setting, whether your voice is
// saved, and the voice sample as its events report it. The only subscriber to
// `VoiceEvent`. Everything is decided by the app; this only keeps what it said.

import { useCallback, useEffect, useState } from "react";

import { type SpeakerTimingChoice, type SpeakersView, type VoiceEvent, type VoiceResult, useIpc } from "../ipc";
import { errorText } from "./useModels";

export type SpeakersLoad =
  | { status: "loading" }
  | { status: "failed"; message: string }
  | { status: "loaded"; view: SpeakersView };

/** The voice sample as last reported. */
export type VoiceSample =
  | { status: "idle" }
  | { status: "starting" }
  | { status: "recording"; recordedMs: number; totalMs: number }
  | { status: "saving" }
  | { status: "done"; result: VoiceResult; message: string }
  | { status: "cancelled" }
  | { status: "failed"; message: string };

/** A refusal or failure, shown until dismissed or replaced. */
export interface SpeakersNotice {
  /** What it is about: the speaker timing setting, or your name and voice. */
  kind: "timing" | "voice";
  title: string;
  message: string;
}

export interface SpeakersPageState {
  load: SpeakersLoad;
  sample: VoiceSample;
  notice: SpeakersNotice | null;
  /** What forgetting said, until the next action. */
  forgotten: string | null;
  busy: boolean;
  dismiss(): void;
  setTiming(timing: SpeakerTimingChoice): Promise<void>;
  /** Stores the user's name; resolves true when stored. */
  setName(name: string): Promise<boolean>;
  /** Starts the sample (the user has agreed). */
  record(): Promise<void>;
  cancel(): Promise<void>;
  forget(): Promise<void>;
}

/** Whether a sample is under way (recording or saving). */
export function sampleRunning(s: VoiceSample): boolean {
  return s.status === "starting" || s.status === "recording" || s.status === "saving";
}

/** Loads the Speakers page and follows the voice sample. */
export function useSpeakers(): SpeakersPageState {
  const ipc = useIpc();
  const [load, setLoad] = useState<SpeakersLoad>({ status: "loading" });
  const [sample, setSample] = useState<VoiceSample>({ status: "idle" });
  const [notice, setNotice] = useState<SpeakersNotice | null>(null);
  const [forgotten, setForgotten] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const setView = (view: SpeakersView) => setLoad({ status: "loaded", view });

  useEffect(() => {
    let live = true;
    let stop: (() => void) | null = null;
    const onEvent = (e: VoiceEvent) => {
      if (!live) return;
      switch (e.type) {
        case "Recording":
          setSample({ status: "recording", recordedMs: e.recorded_ms, totalMs: e.total_ms });
          break;
        case "Saving":
          setSample({ status: "saving" });
          break;
        case "Done":
          setSample({ status: "done", result: e.result, message: e.message });
          setLoad((l) => (l.status === "loaded" ? { status: "loaded", view: { ...l.view, voice: e.voice } } : l));
          break;
        case "Cancelled":
          setSample({ status: "cancelled" });
          break;
        case "Failed":
          setSample({ status: "failed", message: e.message });
          break;
      }
    };
    void ipc.onVoiceEvent(onEvent).then((s) => {
      if (live) stop = s;
      else s();
    });
    void ipc.speakers().then(
      (view) => {
        if (live) setView(view);
      },
      (e: unknown) => {
        if (live) setLoad({ status: "failed", message: errorText(e) });
      },
    );
    return () => {
      live = false;
      stop?.();
    };
  }, [ipc]);

  const setTiming = useCallback(
    async (timing: SpeakerTimingChoice) => {
      setBusy(true);
      try {
        setView(await ipc.setSpeakerTiming(timing));
        setNotice(null);
      } catch (e) {
        setNotice({ kind: "timing", title: "This setting was not saved", message: errorText(e) });
      } finally {
        setBusy(false);
      }
    },
    [ipc],
  );

  const setName = useCallback(
    async (name: string) => {
      setBusy(true);
      try {
        setView(await ipc.setMyName(name));
        setNotice(null);
        return true;
      } catch (e) {
        setNotice({ kind: "voice", title: "Your name was not saved", message: errorText(e) });
        return false;
      } finally {
        setBusy(false);
      }
    },
    [ipc],
  );

  const record = useCallback(async () => {
    setForgotten(null);
    setNotice(null);
    setSample({ status: "starting" });
    try {
      await ipc.startVoiceSample();
    } catch (e) {
      setSample({ status: "idle" });
      setNotice({ kind: "voice", title: "The recording did not start", message: errorText(e) });
    }
  }, [ipc]);

  const cancel = useCallback(async () => {
    await ipc.cancelVoiceSample();
  }, [ipc]);

  const forget = useCallback(async () => {
    setBusy(true);
    setNotice(null);
    try {
      await ipc.forgetMyVoice();
      setView(await ipc.speakers());
      setSample({ status: "idle" });
      setForgotten("Your voice was forgotten. Earshot no longer recognises you from your voice.");
    } catch (e) {
      setNotice({ kind: "voice", title: "Your voice was not forgotten", message: errorText(e) });
    } finally {
      setBusy(false);
    }
  }, [ipc]);

  return {
    load,
    sample,
    notice,
    forgotten,
    busy,
    dismiss: () => setNotice(null),
    setTiming,
    setName,
    record,
    cancel,
    forget,
  };
}
