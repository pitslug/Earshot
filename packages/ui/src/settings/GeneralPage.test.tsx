import { act, cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { type Ipc, IpcFailure, type MockIpcData, createMockIpc } from "../ipc";
import { MOCK_VERSION } from "../ipc/mockSettings";
import { openSettings, renderApp } from "../test/app";
import { THEME_KEY } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { CALL_END_STOP_EXPLAINED, CALL_START_EXPLAINED, START_EXPLAINED, THEME_SYSTEM_EXPLAINED, TRAY_EXPLAINED, shortcutExplained } from "./GeneralPage";

async function open(data: MockIpcData = {}): Promise<{ ipc: Ipc; user: ReturnType<typeof userEvent.setup> }> {
  const timer = createManualTimer(1_000_000);
  const ipc = createMockIpc({ meetings: [], timer, ...data });
  renderApp(ipc, { timer });
  const user = userEvent.setup();
  await openSettings(user, "General");
  await screen.findByRole("heading", { level: 1, name: "General" });
  await screen.findByRole("switch", { name: "Start Earshot with Windows" });
  return { ipc, user };
}

const start = () => screen.getByRole("switch", { name: "Start Earshot with Windows" });

describe("Settings, General", () => {
  it("turns starting with Windows on and off", async () => {
    const s = await open();
    expect(start()).not.toBeChecked();
    expect(screen.getByText(START_EXPLAINED)).toBeInTheDocument();
    expect(screen.getByText(`Earshot ${MOCK_VERSION}`)).toBeInTheDocument();
    await s.user.click(start());
    await waitFor(async () => expect((await s.ipc.generalSettings()).start_with_windows).toBe(true));
    expect(start()).toBeChecked();
    await s.user.click(start());
    await waitFor(async () => expect((await s.ipc.generalSettings()).start_with_windows).toBe(false));
    expect(start()).not.toBeChecked();
  });

  it("offers System, Light and Dark, applied at once, with the title bar following", async () => {
    const calls: unknown[][] = [];
    const s = await open({ onCall: (c) => calls.push(c) });
    const theme = () => document.querySelector("[data-theme]:not(html)")?.getAttribute("data-theme");
    const group = screen.getByRole("radiogroup", { name: "Theme" });
    expect(within(group).getAllByRole("radio").map((r) => (r as HTMLInputElement).labels?.[0].textContent)).toEqual([
      "System",
      "Light",
      "Dark",
    ]);
    expect(within(group).getByRole("radio", { name: "System" })).toBeChecked();
    expect(screen.getByText(THEME_SYSTEM_EXPLAINED)).toBeInTheDocument();
    // The old line pointing at Windows Settings is gone.
    expect(screen.queryByText(/Personalisation/)).toBeNull();
    expect(calls).toContainEqual(["setWindowTheme", null]);
    expect(theme()).toBe("light");

    await s.user.click(within(group).getByRole("radio", { name: "Dark" }));
    expect(theme()).toBe("dark");
    expect(window.localStorage.getItem(THEME_KEY)).toBe("dark");
    await waitFor(() => expect(calls).toContainEqual(["setWindowTheme", "dark"]));

    await s.user.click(within(group).getByRole("radio", { name: "Light" }));
    expect(theme()).toBe("light");
    await waitFor(() => expect(calls).toContainEqual(["setWindowTheme", "light"]));
    await s.user.click(within(group).getByRole("radio", { name: "System" }));
    expect(window.localStorage.getItem(THEME_KEY)).toBe("system");
    await waitFor(() => expect(calls.at(-1)).toEqual(["setWindowTheme", null]));
  });

  it("says how the tray and the shortcut work, and when the shortcut is taken", async () => {
    await open();
    expect(screen.getByText(TRAY_EXPLAINED)).toBeInTheDocument();
    expect(screen.getByText(shortcutExplained("Ctrl+Alt+Shift+R"))).toBeInTheDocument();
    cleanup();
    await open({ settings: { shortcutError: "Ctrl+Alt+Shift+R is in use by another app." } });
    expect(screen.getByText("Ctrl+Alt+Shift+R is in use by another app.")).toBeInTheDocument();
  });


  it("disables the switch with the reason when the setting cannot be read", async () => {
    await open({ settings: { autostartError: "the Windows folder is unknown (SystemRoot)" } });
    expect(start()).toBeDisabled();
    expect(
      screen.getByText("This setting could not be read: the Windows folder is unknown (SystemRoot)"),
    ).toBeInTheDocument();
  });
});

describe("Settings, General, the offer to record a call (T12.86)", () => {
  const offer = () => screen.getByRole("switch", { name: "Offer to record when a call starts" });

  it("shows the stored value, on by default, with what it does", async () => {
    await open();
    expect(await screen.findByRole("switch", { name: "Offer to record when a call starts" })).toBeChecked();
    expect(screen.getByText(CALL_START_EXPLAINED)).toBeInTheDocument();
    cleanup();
    await open({ callStartOffer: false });
    expect(await screen.findByRole("switch", { name: "Offer to record when a call starts" })).not.toBeChecked();
  });

  it("applies a change at once and shows the value the app stored", async () => {
    const calls: unknown[][] = [];
    const s = await open({ onCall: (c) => calls.push(c) });
    await s.user.click(await screen.findByRole("switch", { name: "Offer to record when a call starts" }));
    await waitFor(() => expect(offer()).not.toBeChecked());
    expect(calls).toContainEqual(["setCallStartOffer", false]);
    expect(await s.ipc.callStartOffer()).toBe(false);
    await s.user.click(offer());
    await waitFor(() => expect(offer()).toBeChecked());
    expect(calls).toContainEqual(["setCallStartOffer", true]);
  });

  it("shows what the app stored even when it differs from the press", async () => {
    const timer = createManualTimer(1_000_000);
    const base = createMockIpc({ meetings: [], timer });
    // An app that keeps the offer on whatever is asked.
    renderApp({ ...base, setCallStartOffer: async () => true }, { timer });
    const user = userEvent.setup();
    await openSettings(user, "General");
    await user.click(await screen.findByRole("switch", { name: "Offer to record when a call starts" }));
    await waitFor(() => expect(offer()).toBeEnabled());
    expect(offer()).toBeChecked();
  });

  it("writes nothing and shows no switch before the stored value is read", async () => {
    const timer = createManualTimer(1_000_000);
    const base = createMockIpc({ meetings: [], timer });
    let release: (on: boolean) => void = () => {};
    const writes: boolean[] = [];
    renderApp(
      {
        ...base,
        callStartOffer: () => new Promise<boolean>((resolve) => (release = resolve)),
        setCallStartOffer: async (on) => {
          writes.push(on);
          return on;
        },
      },
      { timer },
    );
    const user = userEvent.setup();
    await openSettings(user, "General");
    await screen.findByRole("switch", { name: "Start Earshot with Windows" });
    expect(screen.queryByRole("switch", { name: "Offer to record when a call starts" })).toBeNull();
    expect(screen.getByText("Reading the call setting")).toBeInTheDocument();
    act(() => release(false));
    expect(await screen.findByRole("switch", { name: "Offer to record when a call starts" })).not.toBeChecked();
    expect(writes).toEqual([]);
  });

  it("says when the change was refused and keeps the stored value", async () => {
    const timer = createManualTimer(1_000_000);
    const base = createMockIpc({ meetings: [], timer });
    renderApp(
      {
        ...base,
        setCallStartOffer: async () => {
          throw new IpcFailure({ kind: "internal", message: "The setting could not be stored.", retryable: false });
        },
      },
      { timer },
    );
    const user = userEvent.setup();
    await openSettings(user, "General");
    await user.click(await screen.findByRole("switch", { name: "Offer to record when a call starts" }));
    expect(await screen.findByText("The setting could not be stored.")).toBeInTheDocument();
    expect(offer()).toBeChecked();
    expect(offer()).toBeEnabled();
  });
});

describe("Settings, General, stopping by itself when a call ends (T14.52)", () => {
  const name = "Stop recording by itself when a call ends";
  const stop = () => screen.getByRole("switch", { name });

  it("is off by default, beside the offer to record, with what on and off do", async () => {
    await open();
    expect(await screen.findByRole("switch", { name })).not.toBeChecked();
    expect(screen.getByText(CALL_END_STOP_EXPLAINED)).toBeInTheDocument();
    expect(CALL_END_STOP_EXPLAINED).toMatch(/after 3 minutes with no answer/);
    expect(CALL_END_STOP_EXPLAINED).toMatch(/keeps recording until you stop it/);
    // In the Calls section, after the offer to record.
    const calls = screen.getByRole("switch", { name: "Offer to record when a call starts" });
    expect(calls.compareDocumentPosition(stop()) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("applies a change at once and shows the value the app stored", async () => {
    const calls: unknown[][] = [];
    const s = await open({ onCall: (c) => calls.push(c) });
    await s.user.click(await screen.findByRole("switch", { name }));
    await waitFor(() => expect(stop()).toBeChecked());
    expect(calls).toContainEqual(["setCallEndStop", true]);
    expect((await s.ipc.callEndStop()).on).toBe(true);
    await s.user.click(stop());
    await waitFor(() => expect(stop()).not.toBeChecked());
    expect(calls).toContainEqual(["setCallEndStop", false]);
  });

  it("says when a stored value is not known, and when a change was refused", async () => {
    const timer = createManualTimer(1_000_000);
    const base = createMockIpc({ meetings: [], timer });
    renderApp(
      {
        ...base,
        callEndStop: async () => ({ on: false, unknown: ["Not known: off is used."] }),
        setCallEndStop: async () => {
          throw new IpcFailure({ kind: "internal", message: "The setting could not be stored.", retryable: false });
        },
      },
      { timer },
    );
    const user = userEvent.setup();
    await openSettings(user, "General");
    expect(await screen.findByText("Not known: off is used.")).toBeInTheDocument();
    await user.click(await screen.findByRole("switch", { name }));
    expect(await screen.findByText("The setting could not be stored.")).toBeInTheDocument();
    expect(stop()).not.toBeChecked();
    expect(stop()).toBeEnabled();
  });
});
