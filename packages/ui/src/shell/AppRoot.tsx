// The window's whole tree, in the one order that works (T14.14). The desktop page renders this
// and nothing else, and the test of the order renders the same component.
//
// Above the error boundary sits only the theme provider: the boundary's fixed words are drawn
// with Fluent, so they need the theme, and a provider that failed would leave nothing to draw
// them with anyway. Everything else sits under the boundary, the provider that hands the app to
// the screens included: whatever throws while rendering, in the app or in a provider, leaves the
// fixed words, Stop recording and Reload instead of a blank window. The boundary is given the
// app directly, so it needs nothing from the tree it guards.

import type { ReactNode } from "react";

import { type Ipc, IpcProvider } from "../ipc";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import type { Timer } from "../time";
import { AppShell } from "./AppShell";
import { RootErrorBoundary } from "./RootErrorBoundary";

export interface AppRootProps {
  /** The app: the same one for the screens and for the error boundary. */
  ipc: Ipc;
  /** A small notice at the top of the meeting pane (the browser preview's badge). */
  notice?: ReactNode;
  /** The clock and timers (injected so tests never use real time). */
  timer?: Timer;
  /** Loads the window's page again, for the boundary's Reload (default: the webview's own). */
  onReload?: () => void;
  /** Shown under the boundary in place of the app: the preview's screen that throws. */
  children?: ReactNode;
}

/** The theme, then the error boundary, then the app's provider and the app window. */
export function AppRoot({ ipc, notice, timer, onReload, children }: AppRootProps) {
  return (
    <EarshotThemeProvider>
      <RootErrorBoundary ipc={ipc} timer={timer} onReload={onReload}>
        {children ?? (
          <IpcProvider ipc={ipc}>
            <AppShell notice={notice} timer={timer} />
          </IpcProvider>
        )}
      </RootErrorBoundary>
    </EarshotThemeProvider>
  );
}
