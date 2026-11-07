// The Audio page's state (T7.4i): the stored settings with the devices Windows lists now,
// and the refusals to show. Everything is saved through the IPC; whether a chosen device is
// connected is the app's answer (`microphone_missing`, `output_missing`), never decided here.

import { useCallback, useEffect, useRef, useState } from "react";

import { type AudioPageView, type AudioSettingsValue, useIpc } from "../ipc";
import { errorText } from "./useModels";

export type AudioLoad =
  | { status: "loading" }
  | { status: "failed"; message: string }
  | { status: "loaded"; page: AudioPageView };

/** A refusal or failure, shown as a MessageBar until dismissed or replaced. */
export interface AudioNotice {
  title: string;
  message: string;
}

export interface AudioPageState {
  load: AudioLoad;
  notice: AudioNotice | null;
  busy: boolean;
  dismiss(): void;
  /** Saves the stored settings with `change`; resolves true when stored. */
  save(change: Partial<AudioSettingsValue>): Promise<boolean>;
  /** Lists the devices again (after one was plugged in or out). */
  refresh(): Promise<void>;
}

/** Loads the Audio page and saves its changes. */
export function useAudio(): AudioPageState {
  const ipc = useIpc();
  const [load, setLoad] = useState<AudioLoad>({ status: "loading" });
  const [notice, setNotice] = useState<AudioNotice | null>(null);
  const [busy, setBusy] = useState(false);
  const current = useRef<AudioPageView | null>(null);
  const mounted = useRef(true);

  const show = (page: AudioPageView) => {
    current.current = page;
    if (mounted.current) setLoad({ status: "loaded", page });
  };

  useEffect(() => {
    mounted.current = true;
    void ipc.audioSettings().then(show, (e: unknown) => {
      if (mounted.current) setLoad({ status: "failed", message: errorText(e) });
    });
    return () => {
      mounted.current = false;
    };
  }, [ipc]);

  const save = useCallback(
    async (change: Partial<AudioSettingsValue>) => {
      const now = current.current;
      if (now === null) return false;
      setBusy(true);
      try {
        show(await ipc.setAudioSettings({ ...now.value, ...change }));
        setNotice(null);
        return true;
      } catch (e) {
        setNotice({ title: "This setting was not saved", message: errorText(e) });
        return false;
      } finally {
        if (mounted.current) setBusy(false);
      }
    },
    [ipc],
  );

  const refresh = useCallback(async () => {
    setBusy(true);
    try {
      show(await ipc.audioSettings());
      setNotice(null);
    } catch (e) {
      setNotice({ title: "The devices could not be listed", message: errorText(e) });
    } finally {
      if (mounted.current) setBusy(false);
    }
  }, [ipc]);

  return { load, notice, busy, dismiss: () => setNotice(null), save, refresh };
}
