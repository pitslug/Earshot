// The first-run wizard's commands (T9.1d), kept apart from ipc.ts so `IpcCommands` gains them
// in one line. Everything the wizard's steps do goes through the Settings pages' commands.

import type { FirstRunView } from "./bindings";

export type { FirstRunView };

/** The first-run part of `IpcCommands`. */
export interface FirstRunCommands {
  /** Whether the app opens on the wizard, and its "never" answers. */
  firstRunState(): Promise<FirstRunView>;
  /** Marks the wizard done, whichever steps were skipped. */
  finishFirstRun(): Promise<FirstRunView>;
  /** Records or clears "never" for an optional model. Rejects with `unknown_model`. */
  setModelDeclined(model: string, declined: boolean): Promise<FirstRunView>;
}
