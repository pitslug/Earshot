// The CUDA section's state: what the IPC says, its notices, and the download events. Everything it
// shows and refuses comes from Rust (`cuda_settings`); a failure's text comes through `plain_text`.

import { useCallback, useEffect, useState } from "react";

import { type CudaPartChoice, type CudaSwitchValue, type CudaTerms, type CudaView, useIpc } from "../ipc";
import { formatSize } from "./copy";
import { type Notice, errorText } from "./useModels";

const NAME: Record<CudaPartChoice, string> = {
  Summaries: "Summaries on CUDA",
  Transcription: "Transcription on CUDA",
};

export interface CudaState {
  /** `null` until read, and when it could not be read (a notice says so). */
  view: CudaView | null;
  notices: Notice[];
  dismiss(key: string): void;
  setSwitch(value: CudaSwitchValue): Promise<void>;
  download(part: CudaPartChoice, acceptedTerms: boolean): Promise<void>;
  pause(part: CudaPartChoice): Promise<void>;
  remove(part: CudaPartChoice): Promise<void>;
  openTerms(terms: CudaTerms): Promise<void>;
  restart(): Promise<void>;
}

/** Loads the CUDA section and follows its events. `onStartChecked` hears the start finish, which
 * changes what the rest of the Models page shows. */
export function useCuda(onStartChecked?: () => Promise<void>): CudaState {
  const ipc = useIpc();
  const [view, setView] = useState<CudaView | null>(null);
  const [notices, setNotices] = useState<Notice[]>([]);

  const notify = useCallback((key: string, intent: Notice["intent"], title: string, message: string) => {
    const n: Notice = { key, area: "page", intent, title, message };
    setNotices((all) => [...all.filter((o) => o.key !== key), n]);
  }, []);
  const dismiss = useCallback((key: string) => {
    setNotices((all) => all.filter((n) => n.key !== key));
  }, []);

  const reload = useCallback(async () => {
    try {
      setView(await ipc.cudaSettings());
      dismiss("read");
    } catch (e) {
      notify("read", "error", "The CUDA settings could not be read", errorText(e));
    }
  }, [ipc, dismiss, notify]);

  useEffect(() => {
    void reload();
  }, [reload]);

  useEffect(() => {
    let live = true;
    let stop: (() => void) | null = null;
    void ipc
      .onCudaEvent((e) => {
        if (!live) return;
        switch (e.type) {
          case "StartChecked":
            void reload();
            void onStartChecked?.();
            break;
          case "Progress":
            setView((v) => {
              if (v === null) return v;
              const key = e.part === "Summaries" ? "summaries" : "transcription";
              return {
                ...v,
                [key]: {
                  ...v[key],
                  held: e.done,
                  downloading: true,
                  installed: v[key].installed === "Yes" ? "Yes" : "Partly",
                },
              };
            });
            break;
          case "Warning":
            notify(`warning-${e.part}`, "warning", NAME[e.part], e.message);
            break;
          case "Failed":
            notify(`download-${e.part}`, "error", `${NAME[e.part]} could not be downloaded`, e.message);
            void reload();
            break;
          default:
            void reload();
        }
      })
      .then((s) => {
        if (live) {
          stop = s;
          // The start may have finished between the first read and this subscription.
          void reload();
          void onStartChecked?.();
        } else s();
      });
    return () => {
      live = false;
      stop?.();
    };
  }, [ipc, notify, reload, onStartChecked]);

  const setSwitch = useCallback(
    async (value: CudaSwitchValue) => {
      try {
        await ipc.setCudaSettings(value);
        dismiss("setting");
      } catch (e) {
        notify("setting", "error", "The setting was not saved", errorText(e));
      }
      await reload();
    },
    [ipc, dismiss, notify, reload],
  );

  const download = useCallback(
    async (part: CudaPartChoice, acceptedTerms: boolean) => {
      dismiss(`download-${part}`);
      try {
        // What the user ticked; the app refuses a download without it.
        await ipc.startCudaDownload(part, acceptedTerms);
      } catch (e) {
        notify(`download-${part}`, "error", `${NAME[part]} could not be downloaded`, errorText(e));
      }
      await reload();
    },
    [ipc, dismiss, notify, reload],
  );

  const pause = useCallback(
    async (part: CudaPartChoice) => {
      try {
        await ipc.cancelCudaDownload(part);
      } catch (e) {
        notify(`download-${part}`, "error", `${NAME[part]} could not be paused`, errorText(e));
        await reload();
      }
    },
    [ipc, notify, reload],
  );

  const remove = useCallback(
    async (part: CudaPartChoice) => {
      try {
        const done = await ipc.removeCudaPart(part);
        const left =
          done.left_in_folder.length > 0
            ? ` Files Earshot does not recognise are still in the folder: ${done.left_in_folder.join(", ")}.`
            : "";
        notify("remove", "success", `${NAME[part]} removed`, `${formatSize(done.bytes_freed)} freed.${left}`);
      } catch (e) {
        notify("remove", "error", `${NAME[part]} was not removed`, errorText(e));
      }
      await reload();
    },
    [ipc, notify, reload],
  );

  const openTerms = useCallback(
    async (terms: CudaTerms) => {
      try {
        await ipc.openCudaTerms(terms);
      } catch (e) {
        notify("terms", "error", "The page could not be opened", errorText(e));
      }
    },
    [ipc, notify],
  );

  const restart = useCallback(async () => {
    try {
      await ipc.restartApp();
    } catch (e) {
      notify("restart", "warning", "Earshot did not restart", errorText(e));
      await reload();
    }
  }, [ipc, notify, reload]);

  return { view, notices, dismiss, setSwitch, download, pause, remove, openTerms, restart };
}
