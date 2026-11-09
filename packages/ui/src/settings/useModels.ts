// The model state of Settings: what the IPC says (hardware, models, settings), the running
// check, and the notices (MessageBars). This hook is the one subscriber to `ModelJobEvent`:
// `SettingsView` holds it for the System, Transcription and Summaries pages, and the first-run
// wizard holds its own for the whole of its life. They are views of `AppShell` and never both
// mounted, so only one subscription is ever live.

import { useCallback, useEffect, useRef, useState } from "react";

import {
  type HardwareView,
  IpcFailure,
  type ModelItem,
  type ModelJobEvent,
  type ModelSettingsValue,
  type ModelSettingsView,
  type ModelsView,
  type RunsOn,
  useIpc,
} from "../ipc";
import { formatSize, titleOf } from "./copy";

/** Where a notice is shown: the top of the page or under the check line. */
export type NoticeArea = "page" | "check";

/** A MessageBar; a later notice with the same key replaces it. */
export interface Notice {
  key: string;
  area: NoticeArea;
  intent: "error" | "warning" | "info" | "success";
  title: string;
  message: string;
}

export interface ModelsData {
  hardware: HardwareView;
  models: ModelsView;
  settings: ModelSettingsView;
}

export type ModelsLoad =
  | { status: "loading" }
  | { status: "failed"; message: string }
  | { status: "loaded"; data: ModelsData };

/** The running check: the trial in progress, if one started. */
export interface Checking {
  model: string | null;
  runs_on: RunsOn | null;
}

export interface ModelsPageState {
  load: ModelsLoad;
  checking: Checking | null;
  notices: Notice[];
  dismiss(key: string): void;
  /** Reads the models again (something outside this hook changed what runs). */
  refresh(): Promise<void>;
  save(change: Partial<ModelSettingsValue>): Promise<void>;
  download(model: string): Promise<void>;
  pause(model: string): Promise<void>;
  remove(model: string): Promise<void>;
  runCheck(): Promise<void>;
  cancelCheck(): Promise<void>;
}

/** The text of a failed IPC call. */
export function errorText(e: unknown): string {
  if (e instanceof IpcFailure) return e.error.message;
  if (e instanceof Error) return e.message;
  return String(e);
}

function patchItem(models: ModelsView, name: string, patch: (i: ModelItem) => ModelItem): ModelsView {
  const map = (list: ModelItem[]) => list.map((i) => (i.name === name ? patch(i) : i));
  return {
    ...models,
    helpers: models.helpers.name === name ? patch(models.helpers) : models.helpers,
    transcription: map(models.transcription),
    summary: map(models.summary),
    npu: map(models.npu),
  };
}

/** Loads the models and follows model job events. */
export function useModels(): ModelsPageState {
  const ipc = useIpc();
  const [load, setLoad] = useState<ModelsLoad>({ status: "loading" });
  const [checking, setChecking] = useState<Checking | null>(null);
  const [notices, setNotices] = useState<Notice[]>([]);
  const reloadSeq = useRef(0);
  // The latest models, for naming models in notices (`ModelItem.title`).
  const latest = useRef<ModelsLoad>(load);
  latest.current = load;
  const modelTitle = useCallback(
    (name: string) =>
      latest.current.status === "loaded" ? titleOf(latest.current.data.models, name) : name,
    [],
  );
  const warningSeq = useRef(0);

  const notify = useCallback((n: Notice) => {
    setNotices((all) => [...all.filter((o) => o.key !== n.key), n]);
  }, []);
  const dismiss = useCallback((key: string) => {
    setNotices((all) => all.filter((n) => n.key !== key));
  }, []);
  const clearCheckNotices = useCallback(() => {
    setNotices((all) => all.filter((n) => n.area !== "check"));
  }, []);

  const reload = useCallback(async () => {
    reloadSeq.current += 1;
    const seq = reloadSeq.current;
    try {
      const models = await ipc.models();
      if (seq !== reloadSeq.current) return;
      setLoad((l) => (l.status === "loaded" ? { status: "loaded", data: { ...l.data, models } } : l));
    } catch (e) {
      notify({
        key: "reload",
        area: "page",
        intent: "error",
        title: "The models could not be read",
        message: errorText(e),
      });
    }
  }, [ipc, notify]);

  useEffect(() => {
    let current = true;
    Promise.all([ipc.hardware(), ipc.models(), ipc.modelSettings()]).then(
      ([hardware, models, settings]) => {
        if (current) setLoad({ status: "loaded", data: { hardware, models, settings } });
      },
      (e: unknown) => {
        if (current) setLoad({ status: "failed", message: errorText(e) });
      },
    );
    return () => {
      current = false;
    };
  }, [ipc]);

  useEffect(() => {
    let live = true;
    let stop: (() => void) | null = null;
    const onEvent = (e: ModelJobEvent) => {
      if (!live) return;
      switch (e.type) {
        case "Progress":
          setLoad((l) =>
            l.status !== "loaded"
              ? l
              : {
                  status: "loaded",
                  data: {
                    ...l.data,
                    models: patchItem(l.data.models, e.model, (i) => ({
                      ...i,
                      held: e.done,
                      downloading: true,
                      installed: i.installed === "Yes" ? "Yes" : "Partly",
                    })),
                  },
                },
          );
          break;
        case "Downloaded":
        case "Cancelled":
          void reload();
          break;
        case "Warning":
          warningSeq.current += 1;
          notify({
            key: `warning-${warningSeq.current}`,
            area: e.model === null ? "check" : "page",
            intent: "warning",
            title: e.model === null ? "Model check" : modelTitle(e.model),
            message: e.message,
          });
          break;
        case "CheckProgress":
          setChecking({ model: e.model, runs_on: e.runs_on });
          break;
        case "Checked":
          setChecking(null);
          if (e.model === null) {
            notify({
              key: "check-result",
              area: "check",
              intent: "warning",
              title: "No installed model works here",
              message: "Download a lighter model, then check again.",
            });
          }
          void reload();
          break;
        case "CheckGaveWay":
          // A meeting started: nothing went wrong, and the page offers the check again.
          setChecking(null);
          void reload();
          break;
        case "Failed":
          if (e.model === null) {
            setChecking(null);
            notify({
              key: "check-failed",
              area: "check",
              intent: "error",
              title: "The check stopped",
              message: e.message,
            });
          } else {
            notify({
              key: `download-${e.model}`,
              area: "page",
              intent: "error",
              title: `${modelTitle(e.model)} could not be downloaded`,
              message: e.message,
            });
          }
          void reload();
          break;
      }
    };
    void ipc.onModelJobEvent(onEvent).then((s) => {
      if (live) stop = s;
      else s();
    });
    return () => {
      live = false;
      stop?.();
    };
  }, [ipc, notify, reload]);

  const save = useCallback(
    async (change: Partial<ModelSettingsValue>) => {
      if (load.status !== "loaded") return;
      const next = { ...load.data.settings.value, ...change };
      try {
        const settings = await ipc.setModelSettings(next);
        dismiss("setting");
        setLoad((l) => (l.status === "loaded" ? { status: "loaded", data: { ...l.data, settings } } : l));
        await reload();
      } catch (e) {
        notify({
          key: "setting",
          area: "page",
          intent: "error",
          title: "The setting was not saved",
          message: errorText(e),
        });
      }
    },
    [ipc, load, dismiss, notify, reload],
  );

  const download = useCallback(
    async (model: string) => {
      dismiss(`download-${model}`);
      try {
        await ipc.startDownload(model);
      } catch (e) {
        notify({
          key: `download-${model}`,
          area: "page",
          intent: "error",
          title: `${modelTitle(model)} could not be downloaded`,
          message: errorText(e),
        });
      }
      await reload();
    },
    [ipc, dismiss, notify, reload],
  );

  const pause = useCallback(
    async (model: string) => {
      try {
        await ipc.cancelDownload(model);
      } catch (e) {
        notify({
          key: `download-${model}`,
          area: "page",
          intent: "error",
          title: `${modelTitle(model)} could not be paused`,
          message: errorText(e),
        });
        await reload();
      }
    },
    [ipc, notify, reload],
  );

  const remove = useCallback(
    async (model: string) => {
      try {
        const done = await ipc.removeModel(model);
        const kept =
          done.kept_shared.length > 0
            ? ` Kept files another model uses: ${done.kept_shared.join(", ")}.`
            : "";
        notify({
          key: "remove",
          area: "page",
          intent: "success",
          title: `${modelTitle(model)} removed`,
          message: `${formatSize(done.bytes_freed)} freed.${kept}`,
        });
      } catch (e) {
        notify({
          key: "remove",
          area: "page",
          intent: "error",
          title: `${modelTitle(model)} was not removed`,
          message: errorText(e),
        });
      }
      await reload();
    },
    [ipc, notify, reload],
  );

  const runCheck = useCallback(async () => {
    clearCheckNotices();
    setChecking({ model: null, runs_on: null });
    try {
      await ipc.runCheck();
    } catch (e) {
      setChecking(null);
      notify({
        key: "check-failed",
        area: "check",
        intent: "error",
        title: "The check could not start",
        message: errorText(e),
      });
    }
  }, [ipc, clearCheckNotices, notify]);

  const cancelCheck = useCallback(async () => {
    try {
      await ipc.cancelCheck();
    } catch (e) {
      notify({
        key: "check-failed",
        area: "check",
        intent: "error",
        title: "The check could not be cancelled",
        message: errorText(e),
      });
    }
  }, [ipc, notify]);

  return { load, checking, notices, dismiss, refresh: reload, save, download, pause, remove, runCheck, cancelCheck };
}
