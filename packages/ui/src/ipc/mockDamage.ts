// The damaged-items part of the mock `Ipc` (T14.65, docs/sync-spec.md 17): the banner, Details
// and Repair from sync, with the app's rules (`apps/desktop/src-tauri/src/damage.rs`). The mock
// runs no scan: the counts are fixture data, and a Repair heals everything outside the
// switched-off kinds. Copy comes only from `fixtures/ui/damaged_copy_cases.json`.

import { DAMAGED_COPY, NO_DAMAGE, moreInAnyKind, totalDamaged } from "./damagedCopy";
import type { DamageCountsView, DamageEvent, DamagedView, KindsOff, RepairView, SyncView } from "./bindings";
import { IpcFailure } from "./ipc";
import type { Timer } from "../time";

/** The one mock option for damaged items on this computer. */
export interface DamageMockData {
  /** What the last scan found, by kind (missing kinds: 0). */
  counts?: Partial<DamageCountsView>;
  /** Sync is set up and on, so Repair is offered (default: what the sync mock says). */
  syncOn?: boolean;
  /** Whether each kind's own sync switch is off (default: what the sync mock says). */
  switchesOff?: Partial<KindsOff>;
  /** Where a Repair is when the window opens (default idle). */
  repair?: RepairView;
  /** The banner was dismissed at these counts (default: not dismissed). */
  dismissedAt?: Partial<DamageCountsView>;
}

/** How long each step of a mock Repair takes. */
export const MOCK_REPAIR_STEP_MS = 1500;

/** The damaged-items commands and event of the mock. */
export interface DamageMock {
  damagedItems(): Promise<DamagedView>;
  repairFromSync(): Promise<void>;
  damageBanner(): Promise<boolean>;
  dismissDamageBanner(): Promise<void>;
  onDamageEvent(handler: (event: DamageEvent) => void): Promise<() => void>;
  /** What is damaged now. */
  counts(): DamageCountsView;
}

export function createDamageMock(
  data: DamageMockData | undefined,
  syncView: () => Promise<SyncView>,
  timer: Timer,
): DamageMock {
  let counts: DamageCountsView = { ...NO_DAMAGE, ...data?.counts };
  let dismissed: DamageCountsView | null = data?.dismissedAt === undefined ? null : { ...NO_DAMAGE, ...data.dismissedAt };
  let repair: RepairView = data?.repair ?? { state: "idle" };
  const listeners = new Set<(event: DamageEvent) => void>();

  // `damaged::DamagedItems::shown`: something found and not dismissed at counts as high.
  const shown = () => totalDamaged(counts) > 0 && (dismissed === null || moreInAnyKind(counts, dismissed));
  const emit = () => {
    const event: DamageEvent = { shown: shown(), repair };
    for (const l of [...listeners]) l(event);
  };
  const sync = async () => {
    const v = await syncView();
    return {
      on: data?.syncOn ?? (v.set_up && v.on),
      voiceOff: data?.switchesOff?.voice ?? !v.voiceprints,
      wordsOff: data?.switchesOff?.words ?? !v.dictionary,
    };
  };
  const kindsOff = async (c: DamageCountsView): Promise<KindsOff> => {
    const s = await sync();
    return { voice: c.voice > 0 && s.voiceOff, words: c.words > 0 && s.wordsOff };
  };

  return {
    counts: () => ({ ...counts }),
    damagedItems: async () => ({
      counts: { ...counts },
      kinds_off: await kindsOff(counts),
      sync_repairs: (await sync()).on,
      repair,
    }),
    repairFromSync: async () => {
      const s = await sync();
      if (!s.on) throw new IpcFailure({ kind: "invalid_setting", message: DAMAGED_COPY.no_repair_sync_off, retryable: false });
      if (repair.state === "reading" || repair.state === "stopped") {
        throw new IpcFailure({ kind: "busy", message: DAMAGED_COPY.busy, retryable: true });
      }
      repair = { state: "reading", percent: null };
      emit();
      timer.setTimeout(() => {
        repair = { state: "reading", percent: 50 };
        emit();
        timer.setTimeout(() => {
          // Only the switched-off kinds stay damaged: the server had good copies of the rest.
          counts = { ...NO_DAMAGE, voice: s.voiceOff ? counts.voice : 0, words: s.wordsOff ? counts.words : 0 };
          if (totalDamaged(counts) === 0) dismissed = null;
          repair = { state: "done", left: { ...counts } };
          emit();
        }, MOCK_REPAIR_STEP_MS);
      }, MOCK_REPAIR_STEP_MS);
    },
    damageBanner: async () => shown(),
    dismissDamageBanner: async () => {
      dismissed = { ...counts };
      emit();
    },
    onDamageEvent: async (handler) => {
      listeners.add(handler);
      return () => {
        listeners.delete(handler);
      };
    },
  };
}
