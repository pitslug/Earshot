// The mark beside Settings when an update waits for the user.

import { act, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { type Ipc, IpcProvider, type UpdateEvent, type UpdatePhase, createMockIpc } from "../ipc";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { AppShell } from "./AppShell";

installMatchMedia(false);

const AVAILABLE: UpdatePhase = { state: "Available", version: "9.9.9", size: 1000 };
const DOWNLOADING: UpdatePhase = { state: "Downloading", version: "9.9.9", done: 1, total: 10 };
const READY: UpdatePhase = { state: "Ready", version: "9.9.9" };

function open(configure: (base: Ipc) => Partial<Ipc>, phase?: UpdatePhase) {
  const timer = createManualTimer(1_000_000);
  const base = createMockIpc({ timer, ...(phase ? { update: { phase } } : {}) });
  let handler: ((e: UpdateEvent) => void) | null = null;
  const unsubscribe = vi.fn();
  const ipc: Ipc = {
    ...base,
    onUpdateEvent: async (h) => {
      handler = h;
      return unsubscribe;
    },
    ...configure(base),
  };
  const view = render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell timer={timer} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  return { view, unsubscribe, send: (phase: UpdatePhase) => act(() => handler?.({ phase })), ready: () => waitFor(() => expect(handler).not.toBeNull()) };
}

describe("update mark", () => {
  it.each([{ state: "Idle" }, { state: "UpToDate" }, { state: "Failed", message: "invented" }] as UpdatePhase[])(
    "shows nothing for %j",
    async (phase) => {
      const s = open(() => ({}), phase);
      await screen.findByRole("button", { name: "Settings" });
      await s.ready();
      expect(screen.queryByTestId("update-mark")).toBeNull();
    },
  );

  it.each([AVAILABLE, DOWNLOADING, READY])("shows a mark on open for %j", async (phase) => {
    open(() => ({}), phase);
    await screen.findByRole("button", { name: "Settings, update available" });
    expect(screen.getByTestId("update-mark")).toBeTruthy();
  });

  it.each([AVAILABLE, DOWNLOADING, READY])("shows a mark from an event for %j", async (phase) => {
    const s = open(() => ({}));
    await screen.findByRole("button", { name: "Settings" });
    await s.ready();
    s.send(phase);
    await screen.findByRole("button", { name: "Settings, update available" });
  });

  it("clears the mark when the phase moves on", async () => {
    const s = open(() => ({}), AVAILABLE);
    await screen.findByRole("button", { name: "Settings, update available" });
    s.send({ state: "UpToDate" });
    await screen.findByRole("button", { name: "Settings" });
    expect(screen.queryByTestId("update-mark")).toBeNull();
  });

  it("removes its subscription on unmount", async () => {
    const s = open(() => ({}));
    await screen.findByRole("button", { name: "Settings" });
    await s.ready();
    s.view.unmount();
    await waitFor(() => expect(s.unsubscribe).toHaveBeenCalled());
  });

  it("shows nothing and keeps working when the status cannot be read", async () => {
    open(() => ({ updateStatus: () => Promise.reject(new Error("invented")) }));
    await screen.findByRole("button", { name: "Settings" });
    expect(screen.queryByTestId("update-mark")).toBeNull();
  });
});
