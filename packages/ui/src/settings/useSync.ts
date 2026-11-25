// The Sync page's state (T11.3a): the view the app sends, the two set up forms, the pairing code
// shown for adding a computer and the list of computers. No setup secret or typed pairing code is
// kept once a call returns: the fields are cleared after every attempt. The one code kept is the
// one this computer shows, until Hide, the code's own expiry (on the injected timer) or the page
// closes.

import { useEffect, useRef, useState } from "react";

import { type SyncCodeView, type SyncDeviceView, type SyncView, errorText, useIpc } from "../ipc";
import { useWindowWatched } from "../shell/windowWatched";
import type { Timer } from "../time";
import { type Load, useLoad } from "./parts";

/** How often the open Sync page asks for the computers again while the window is watched, so one
 * that quits, sleeps or comes back shows without leaving the page (the 0.4.2 test, 2026-11-25). */
export const DEVICES_REFRESH_MS = 10_000;

/** A list of computers answered less than this long ago is not asked for again when the window is
 * watched again (a quick look away and back asks nothing). */
export const DEVICES_FRESH_MS = 2_000;

type DevicesAnswer = { ok: true; view: SyncDeviceView[] } | { ok: false; message: string };

/** One of the two forms for a computer that is not set up. */
export interface SyncForm {
  address: string;
  /** The setup secret or the pairing code (cleared after every attempt). */
  secret: string;
  /** The last refusal, shown under the form. */
  error: string | null;
}

const EMPTY_FORM: SyncForm = { address: "", secret: "", error: null };

/** Everything the Sync page shows and does. */
export interface SyncPageState {
  load: Load<SyncView>;
  busy: boolean;
  setup: SyncForm;
  setSetup: (form: SyncForm) => void;
  join: SyncForm;
  setJoin: (form: SyncForm) => void;
  /** A switch, Sync now or Remove that failed (fixed copy). */
  error: string | null;
  clearError: () => void;
  /** The pairing code shown, or why none could be shown. */
  code: SyncCodeView | null;
  codeError: string | null;
  /** The computers, once asked (null before the first ask). */
  devices: Load<SyncDeviceView[]> | null;
  /** Resolves true when this computer is now set up (a refusal shows under its form). */
  setUpFirst: () => Promise<boolean>;
  /** Resolves true when this computer joined (a refusal shows under its form). */
  joinWithCode: () => Promise<boolean>;
  setOn: (on: boolean) => Promise<void>;
  setVoiceprints: (on: boolean) => Promise<void>;
  setDictionary: (on: boolean) => Promise<void>;
  /** "Write summaries for my other computers" (T11.3db). */
  setOfferSummaries: (on: boolean) => Promise<void>;
  /** Saves this computer's name; resolves with the refusal's copy, or null when saved. */
  saveDeviceName: (name: string) => Promise<string | null>;
  syncNow: () => Promise<void>;
  showCode: () => Promise<void>;
  hideCode: () => void;
  remove: () => Promise<boolean>;
  /** Removes another computer (T11.6c); resolves true when it is gone from the list. A refusal
   * shows above the parts, as for the other changes. */
  removeOther: (id: string) => Promise<boolean>;
}

/** The Sync page's state. */
export function useSync(timer: Timer): SyncPageState {
  const ipc = useIpc();
  const [load, setLoad] = useLoad((ipc) => ipc.syncView());
  const [busy, setBusy] = useState(false);
  const [setup, setSetup] = useState<SyncForm>(EMPTY_FORM);
  const [join, setJoin] = useState<SyncForm>(EMPTY_FORM);
  const [error, setError] = useState<string | null>(null);
  const [code, setCode] = useState<SyncCodeView | null>(null);
  const [codeError, setCodeError] = useState<string | null>(null);
  const [devices, setDevices] = useState<Load<SyncDeviceView[]> | null>(null);
  // Bumped to ask for the computers again (after a sync now).
  const [devicesAsked, setDevicesAsked] = useState(0);

  // The shown code goes when it stops working.
  useEffect(() => {
    if (code === null) return;
    return timer.setTimeout(() => setCode(null), code.expires_in_s * 1_000);
  }, [code, timer]);

  const setUp = load.status === "loaded" && load.view.set_up;
  // The server is asked only while the window is watched (T14.50): hidden in the tray, minimised
  // or behind another window, the open page asks nothing, and it asks at once when it is watched
  // again (this effect runs anew), so the list is never older than the last look at it. A return
  // within DEVICES_FRESH_MS of the last shown answer asks nothing, and no second ask starts while
  // one runs: a run that finds one waits for its answer instead.
  const watched = useWindowWatched();
  const asking = useRef<Promise<DevicesAnswer> | null>(null);
  const shownAt = useRef<number | null>(null);
  const lastAsked = useRef(devicesAsked);
  useEffect(() => {
    if (!setUp) {
      setDevices(null);
      shownAt.current = null;
      return;
    }
    if (!watched) return;
    // A bump (the interval, Sync now) always asks; anything else is a return to the window.
    const bumped = devicesAsked !== lastAsked.current;
    lastAsked.current = devicesAsked;
    const fresh = shownAt.current !== null && timer.now() - shownAt.current < DEVICES_FRESH_MS;
    if (!bumped && asking.current === null && fresh) return;
    let live = true;
    setDevices((d) => d ?? { status: "loading" });
    let ask = asking.current;
    if (ask === null) {
      const started = ipc.syncDevices().then(
        (view): DevicesAnswer => ({ ok: true, view }),
        (e: unknown): DevicesAnswer => ({ ok: false, message: errorText(e) }),
      );
      asking.current = started;
      void started.then(() => {
        if (asking.current === started) asking.current = null;
      });
      ask = started;
    }
    void ask.then((answer) => {
      if (!live) return;
      if (answer.ok) {
        setDevices({ status: "loaded", view: answer.view });
        shownAt.current = timer.now();
      } else {
        setDevices({ status: "failed", message: answer.message });
      }
    });
    return () => {
      live = false;
    };
  }, [ipc, setUp, watched, devicesAsked, timer]);

  // While the page is open and the window is watched the computers are asked for again, so one
  // that quits, sleeps or comes back shows without leaving the page; leaving the page, or the
  // window no longer being watched, stops it.
  useEffect(() => {
    if (!setUp || !watched) return;
    return timer.setInterval(() => setDevicesAsked((n) => n + 1), DEVICES_REFRESH_MS);
  }, [setUp, watched, timer]);

  const shown = (view: SyncView) => setLoad({ status: "loaded", view });

  // Runs a set up or join; the secret field is cleared whatever happens.
  const attempt = async (
    form: SyncForm,
    put: (form: SyncForm) => void,
    work: () => Promise<SyncView>,
  ): Promise<boolean> => {
    setBusy(true);
    try {
      const view = await work();
      put({ ...EMPTY_FORM, address: form.address });
      shown(view);
      return view.set_up;
    } catch (e) {
      put({ address: form.address, secret: "", error: errorText(e) });
      return false;
    } finally {
      setBusy(false);
    }
  };

  // Runs a change on a computer that is set up; a refusal shows above the parts.
  const change = async (work: () => Promise<SyncView>): Promise<boolean> => {
    setBusy(true);
    try {
      shown(await work());
      setError(null);
      return true;
    } catch (e) {
      setError(errorText(e));
      return false;
    } finally {
      setBusy(false);
    }
  };

  return {
    load,
    busy,
    setup,
    setSetup,
    join,
    setJoin,
    error,
    clearError: () => setError(null),
    code,
    codeError,
    devices,
    setUpFirst: () => attempt(setup, setSetup, () => ipc.setUpSync(setup.address, setup.secret)),
    joinWithCode: () => attempt(join, setJoin, () => ipc.joinSync(join.address, join.secret)),
    setOn: async (on) => {
      await change(() => ipc.setSyncOn(on));
    },
    setVoiceprints: async (on) => {
      await change(() => ipc.setSyncVoiceprints(on));
    },
    setDictionary: async (on) => {
      await change(() => ipc.setSyncDictionary(on));
    },
    setOfferSummaries: async (on) => {
      await change(() => ipc.setSyncOfferSummaries(on));
    },
    saveDeviceName: async (name) => {
      setBusy(true);
      try {
        shown(await ipc.setSyncDeviceName(name));
        return null;
      } catch (e) {
        return errorText(e);
      } finally {
        setBusy(false);
      }
    },
    syncNow: async () => {
      if (await change(() => ipc.syncNow())) setDevicesAsked((n) => n + 1);
    },
    showCode: async () => {
      setBusy(true);
      try {
        setCode(await ipc.offerSyncCode());
        setCodeError(null);
      } catch (e) {
        setCode(null);
        setCodeError(errorText(e));
      } finally {
        setBusy(false);
      }
    },
    hideCode: () => setCode(null),
    remove: async () => {
      const removed = await change(() => ipc.removeSyncDevice());
      if (removed) {
        setCode(null);
        setCodeError(null);
        setSetup(EMPTY_FORM);
        setJoin(EMPTY_FORM);
      }
      return removed;
    },
    removeOther: async (id) => {
      setBusy(true);
      try {
        const { devices } = await ipc.removeOtherSyncDevice(id);
        if (devices === null) {
          // Removed, but the list could not be read after it: ask again.
          setDevicesAsked((n) => n + 1);
        } else {
          setDevices({ status: "loaded", view: devices });
        }
        setError(null);
        return true;
      } catch (e) {
        setError(errorText(e));
        return false;
      } finally {
        setBusy(false);
      }
    },
  };
}
