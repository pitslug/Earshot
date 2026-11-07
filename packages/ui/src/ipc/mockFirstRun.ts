// The first-run part of the mock `Ipc` (T9.1d): the record of `earshot_runtime::first_run`
// kept in memory. Whether the wizard shows at the start is given (the rule about stored
// meetings lives in Rust and is tested there).

import type { FirstRunCommands, FirstRunView } from "./firstRun";
import { IpcFailure } from "./ipc";
import { MOCK_DISPLAY_NAMES } from "./mockModels";

/** What the first-run mock starts with. */
export interface FirstRunMockData {
  /** The app opens on the wizard (default false). */
  show?: boolean;
  /** The models already answered "never". */
  declined?: string[];
  /** `firstRunState` rejects (the store cannot be read). */
  stateFails?: boolean;
  /** `finishFirstRun` rejects this many times (the store cannot be written). */
  finishFailures?: number;
  /** Called with the arguments of every command. */
  onCall?: (call: unknown[]) => void;
}

/** The first-run commands of the mock `Ipc`. */
export function createFirstRunMock(data: FirstRunMockData = {}): FirstRunCommands {
  let show = data.show ?? false;
  let declined = [...(data.declined ?? [])].sort();
  let failures = data.finishFailures ?? 0;
  const view = (): FirstRunView => ({ show, declined_models: [...declined] });
  return {
    firstRunState: async () => {
      if (data.stateFails) {
        throw new IpcFailure({ kind: "store", message: "the database could not be read", retryable: false });
      }
      return view();
    },
    finishFirstRun: async () => {
      data.onCall?.(["finishFirstRun"]);
      if (failures > 0) {
        failures -= 1;
        throw new IpcFailure({ kind: "store", message: "the database could not be written", retryable: true });
      }
      show = false;
      return view();
    },
    setModelDeclined: async (model, no) => {
      data.onCall?.(["setModelDeclined", model, no]);
      if (!(model in MOCK_DISPLAY_NAMES)) {
        throw new IpcFailure({
          kind: "unknown_model",
          message: `${model} is not a model of this version`,
          retryable: false,
        });
      }
      declined = declined.filter((m) => m !== model);
      if (no) declined = [...declined, model].sort();
      return view();
    },
  };
}
