// The Summaries page's state: the stored settings, whether an API key is stored (never the
// key), the model a summary on this computer uses (from the models view), Claude Code's
// and Codex's status (each asked only when its choice is shown: it runs the CLI's own checks)
// and the refusals to show. Everything is saved through the IPC; nothing is decided here.

import { useCallback, useEffect, useRef, useState } from "react";

import {
  type ClaudeCodeStatusView,
  type CodexStatusView,
  type ModelsView,
  type StepChoice,
  mirrorWriter,
  type SummaryOrderView,
  type SummarySettingsValue,
  type SummarySettingsView,
  errorText,
  useIpc,
} from "../ipc";

export type SummariesLoad =
  | { status: "loading" }
  | { status: "failed"; message: string }
  | { status: "loaded"; settings: SummarySettingsView; keySaved: boolean | null };

/** An agent CLI's status as last asked. */
export type CliCheck<V> =
  | { status: "idle" }
  | { status: "checking" }
  | { status: "done"; view: V }
  | { status: "failed"; message: string };

/** Claude Code's status as last asked (T7.4ga). */
export type ClaudeCodeCheck = CliCheck<ClaudeCodeStatusView>;

/** Codex's status as last asked (T7.4gc). */
export type CodexCheck = CliCheck<CodexStatusView>;

/** A refusal or failure, shown as a MessageBar until dismissed or replaced. */
export interface SummariesNotice {
  title: string;
  message: string;
}

export interface SummariesPageState {
  load: SummariesLoad;
  /** The models view for "On this computer" (null until read or when it failed). */
  models: ModelsView | null;
  /** The order of summary writers (T12.63b; null until read or when it failed). Nothing that
   * changes the order may be offered while it is null: a choice made then would replace an
   * order nobody has seen. */
  order: SummaryOrderView | null;
  /** Why the order could not be read (null while it loads or once read). */
  orderError: string | null;
  /** Stores the order of `steps` (empty: no summary), with the endpoint settings when they
   * change too; resolves true when stored. */
  saveOrder(steps: StepChoice[], settings?: Partial<SummarySettingsValue>): Promise<boolean>;
  /** Why the models view could not be read. */
  modelsError: string | null;
  notice: SummariesNotice | null;
  /** Why the key state is unknown (Credential Manager failed). */
  keyError: string | null;
  busy: boolean;
  dismiss(): void;
  /** Saves the stored settings with `change`; resolves true when stored. */
  save(change: Partial<SummarySettingsValue>): Promise<boolean>;
  saveKey(key: string): Promise<boolean>;
  removeKey(): Promise<void>;
  /** Claude Code's status (`idle` until `checkClaudeCode` first runs). */
  claudeCode: ClaudeCodeCheck;
  /** Asks the app whether Claude Code is installed and signed in. */
  checkClaudeCode(): Promise<void>;
  /** Codex's status (`idle` until `checkCodex` first runs). */
  codex: CodexCheck;
  /** Asks the app whether Codex is installed and signed in. */
  checkCodex(): Promise<void>;
}

/** Loads the Summaries page and saves its changes. */
export function useSummaries(): SummariesPageState {
  const ipc = useIpc();
  const [load, setLoad] = useState<SummariesLoad>({ status: "loading" });
  const [models, setModels] = useState<ModelsView | null>(null);
  const [order, setOrderState] = useState<SummaryOrderView | null>(null);
  const [orderError, setOrderError] = useState<string | null>(null);
  const orderRef = useRef<SummaryOrderView | null>(null);
  const setOrder = (o: SummaryOrderView) => {
    orderRef.current = o;
    setOrderState(o);
    setOrderError(null);
  };
  const [modelsError, setModelsError] = useState<string | null>(null);
  const [notice, setNotice] = useState<SummariesNotice | null>(null);
  const [keyError, setKeyError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [claudeCode, setClaudeCode] = useState<ClaudeCodeCheck>({ status: "idle" });
  const [codex, setCodex] = useState<CodexCheck>({ status: "idle" });
  const current = useRef<SummarySettingsView | null>(null);

  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const settings = await ipc.summarySettings();
        let keySaved: boolean | null = null;
        try {
          keySaved = await ipc.hasApiKey();
        } catch (e) {
          if (live) setKeyError(errorText(e));
        }
        if (!live) return;
        current.current = settings;
        setLoad({ status: "loaded", settings, keySaved });
      } catch (e) {
        if (live) setLoad({ status: "failed", message: errorText(e) });
      }
    })();
    void ipc.summaryOrder().then(
      (o) => {
        if (live) setOrder(o);
      },
      (e: unknown) => {
        if (live) setOrderError(errorText(e));
      },
    );
    void ipc.models().then(
      (m) => {
        if (live) setModels(m);
      },
      (e: unknown) => {
        if (live) setModelsError(errorText(e));
      },
    );
    return () => {
      live = false;
    };
  }, [ipc]);

  const setSettings = (settings: SummarySettingsView) => {
    current.current = settings;
    setLoad((l) => (l.status === "loaded" ? { ...l, settings } : l));
  };
  const setKeySaved = (keySaved: boolean) =>
    setLoad((l) => (l.status === "loaded" ? { ...l, keySaved } : l));

  const save = useCallback(
    async (change: Partial<SummarySettingsValue>) => {
      const now = current.current;
      if (now === null) return false;
      setBusy(true);
      try {
        setSettings(await ipc.setSummarySettings({ ...now.value, ...change }));
        setNotice(null);
        // The steps' labels name the endpoint's host.
        try {
          setOrder(await ipc.summaryOrder());
        } catch {
          // Kept as read: the setting itself was stored.
        }
        return true;
      } catch (e) {
        setNotice({ title: "This setting was not saved", message: errorText(e) });
        return false;
      } finally {
        setBusy(false);
      }
    },
    [ipc],
  );

  const saveOrder = useCallback(
    async (steps: StepChoice[], change?: Partial<SummarySettingsValue>) => {
      const now = current.current;
      // Never over an order that was not read.
      if (now === null || orderRef.current === null) return false;
      setBusy(true);
      try {
        const stored = await ipc.setSummaryOrder(steps, change === undefined ? null : { ...now.value, ...change });
        setOrder(stored);
        // The writer the old setting mirrors follows the order at once, so no later save sends a
        // writer the order no longer holds (it would put that writer back first).
        const writer = mirrorWriter(stored.steps.map((v) => v.step));
        const value = { ...now.value, ...change, writer };
        setSettings({ ...now, value });
        setNotice(null);
        // The live switch and the endpoint's host as stored; the order is saved either way.
        try {
          setSettings(await ipc.summarySettings());
        } catch {
          // Kept as patched above.
        }
        return true;
      } catch (e) {
        setNotice({ title: "This setting was not saved", message: errorText(e) });
        return false;
      } finally {
        setBusy(false);
      }
    },
    [ipc],
  );

  const saveKey = useCallback(
    async (key: string) => {
      setBusy(true);
      try {
        await ipc.setApiKey(key);
        setKeySaved(true);
        setKeyError(null);
        setNotice(null);
        return true;
      } catch (e) {
        setNotice({ title: "The API key was not saved", message: errorText(e) });
        return false;
      } finally {
        setBusy(false);
      }
    },
    [ipc],
  );

  const removeKey = useCallback(async () => {
    setBusy(true);
    try {
      await ipc.removeApiKey();
      setKeySaved(false);
      setKeyError(null);
      setNotice(null);
    } catch (e) {
      setNotice({ title: "The API key was not removed", message: errorText(e) });
    } finally {
      setBusy(false);
    }
  }, [ipc]);

  const checkClaudeCode = useCallback(async () => {
    setClaudeCode({ status: "checking" });
    try {
      setClaudeCode({ status: "done", view: await ipc.claudeCodeStatus() });
    } catch (e) {
      setClaudeCode({ status: "failed", message: errorText(e) });
    }
  }, [ipc]);

  const checkCodex = useCallback(async () => {
    setCodex({ status: "checking" });
    try {
      setCodex({ status: "done", view: await ipc.codexStatus() });
    } catch (e) {
      setCodex({ status: "failed", message: errorText(e) });
    }
  }, [ipc]);

  return {
    load,
    models,
    order,
    orderError,
    saveOrder,
    modelsError,
    notice,
    keyError,
    busy,
    dismiss: () => setNotice(null),
    save,
    saveKey,
    removeKey,
    claudeCode,
    checkClaudeCode,
    codex,
    checkCodex,
  };
}
