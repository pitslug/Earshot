// The Sync page commands (T11.3a), kept apart from ipc.ts so `IpcCommands` gains them in one
// line. Refusals are `IpcError` kind `sync` (fixed copy by problem word,
// `fixtures/ui/sync_copy_cases.json`) or `invalid_setting` (an address or setup secret the rule
// refuses).

import type { SyncCodeView, SyncDeviceView, SyncView } from "./bindings";

export type { SyncCodeView, SyncDeviceView, SyncView };

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
  /** Asks for a sync now. */
  syncNow(): Promise<SyncView>;
  /** Removes this computer from sync: its meetings stay, the server and key are forgotten. */
  removeSyncDevice(): Promise<SyncView>;
}
