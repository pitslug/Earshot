import { type ReactNode, createContext, useContext } from "react";

import type { Ipc } from "./ipc";

const IpcContext = createContext<Ipc | null>(null);

/** Provides the app's `Ipc` (Tauri or mock) to every component below it. */
export function IpcProvider({ ipc, children }: { ipc: Ipc; children: ReactNode }) {
  return <IpcContext.Provider value={ipc}>{children}</IpcContext.Provider>;
}

/** The `Ipc` of the nearest `IpcProvider`. */
export function useIpc(): Ipc {
  const ipc = useContext(IpcContext);
  if (!ipc) throw new Error("useIpc needs an IpcProvider above it");
  return ipc;
}
