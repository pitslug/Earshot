// The Sync page commands (T11.3a) and summary offload's (T11.3db), kept apart from ipc.ts so
// `IpcCommands` gains them in one line. Refusals are `IpcError` kind `sync` (fixed copy by problem
// word, `fixtures/ui/sync_copy_cases.json`), `invalid_setting` (an address, setup secret or name
// the rule refuses) or `offload` (a computer that would not be asked,
// `fixtures/ui/offload_copy_cases.json`).

import type {
  SummaryHelperView,
  SummaryHelpersView,
  SyncCodeView,
  SyncDeviceView,
  SyncRemovedView,
  SyncView,
} from "./bindings";

export type { SummaryHelperView, SummaryHelpersView, SyncCodeView, SyncDeviceView, SyncRemovedView, SyncView };

/** The Sync part of `IpcCommands`. */
export interface SyncCommands {
  /** What the Sync page shows. Never a key, token, setup secret or used pairing code. */
  syncView(): Promise<SyncView>;
  /** The vault's devices (asks the server). */
  syncDevices(): Promise<SyncDeviceView[]>;
  /** Sets this computer up as the first device with the server's setup secret. */
  setUpSync(address: string, setupSecret: string): Promise<SyncView>;
  /** A pairing code for adding another device: works once, for `expires_in_s`. */
  offerSyncCode(): Promise<SyncCodeView>;
  /** Joins the vault with a pairing code shown on another device. */
  joinSync(address: string, code: string): Promise<SyncView>;
  /** Turns sync on or off. */
  setSyncOn(on: boolean): Promise<SyncView>;
  /** Turns voiceprint sync on or off. */
  setSyncVoiceprints(on: boolean): Promise<SyncView>;
  /** Turns Dictionary sync on or off (T11.3e; on by default once sync is on). */
  setSyncDictionary(on: boolean): Promise<SyncView>;
  /** Asks for a sync now. */
  syncNow(): Promise<SyncView>;
  /** Removes this computer from sync: its meetings stay, the server and key are forgotten. */
  removeSyncDevice(): Promise<SyncView>;
  /** Removes another computer from sync by its `SyncDeviceView.id` (T11.6c): the server stops
   * taking its requests and its synced meetings stay. Resolves with the computers left, or no list
   * when they could not be listed after the removal (which still happened). */
  removeOtherSyncDevice(id: string): Promise<SyncRemovedView>;
  /** Turns "Write summaries for my other computers" on or off (T11.3db). */
  setSyncOfferSummaries(on: boolean): Promise<SyncView>;
  /** Sets this computer's name as the other computers show it; empty: the computer's own. */
  setSyncDeviceName(name: string): Promise<SyncView>;
  /** The Summaries page's other computers that write summaries, and the chosen one (asks the
   * server). */
  summaryHelpers(): Promise<SummaryHelpersView>;
  /** Chooses the computer this one asks to write its summaries (null: none). */
  setSummaryHelper(device: string | null): Promise<SummaryHelpersView>;
  /** Asks another computer to write a meeting's summary now; resolves with the meeting's waiting
   * line. */
  askSummaryHelper(meeting: string, device: string): Promise<string>;
  /** Stops a meeting's summary waiting on another computer (T14.66): null when it stopped (read
   * the meeting again), else fixed copy saying why nothing was stopped. */
  stopWaitingSummary(meeting: string): Promise<string | null>;
}
