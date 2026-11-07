import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { type Ipc, IpcProvider, type MockIpcData, createMockIpc } from "../ipc";
import { MOCK_CRASH_LOGS, MOCK_VERSION } from "../ipc/mockSettings";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider, THEME_KEY } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { CRASH_LOGS_EXPLAINED, CRASH_LOGS_NO_FOLDER } from "./CrashLogsFolder";
import { PREVIEWS_EXPLAINED, START_EXPLAINED, THEME_SYSTEM_EXPLAINED, TRAY_EXPLAINED, shortcutExplained } from "./GeneralPage";

async function open(data: MockIpcData = {}): Promise<{ ipc: Ipc; user: ReturnType<typeof userEvent.setup> }> {
  const timer = createManualTimer(1_000_000);
  const ipc = createMockIpc({ meetings: [], timer, ...data });
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell timer={timer} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: "Settings" }));
  const nav = await screen.findByRole("navigation", { name: "Settings sections" });
  await user.click(within(nav).getByRole("button", { name: "General" }));
  await screen.findByRole("heading", { level: 1, name: "General" });
  await screen.findByRole("switch", { name: "Start Earshot with Windows" });
  return { ipc, user };
}

const start = () => screen.getByRole("switch", { name: "Start Earshot with Windows" });

beforeEach(() => {
  installMatchMedia(false);
});

describe("Settings, General, provisional words", () => {
  const label = "Show words while someone is talking";
  const toggle = async () => (await screen.findByRole("switch", { name: label })) as HTMLInputElement;

  it("says what it does and stores a change", async () => {
    const s = await open({ settings: { previews: { on: true, can_change: true, note: null, unknown: null } } });
    const sw = await toggle();
    expect(sw.checked).toBe(true);
    expect(screen.getByText(PREVIEWS_EXPLAINED)).toBeTruthy();
    await s.user.click(sw);
    await waitFor(async () => expect((await s.ipc.previewsSettings()).on).toBe(false));
    expect((await toggle()).checked).toBe(false);
  });

  it("shows the note for a small machine, and lets it be turned on", async () => {
    const note = "A note about this computer.";
    const s = await open({ settings: { previews: { on: false, can_change: true, note, unknown: null } } });
    const sw = await toggle();
    expect(screen.getByText(note)).toBeTruthy();
    expect(sw.disabled).toBe(false);
    await s.user.click(sw);
    await waitFor(async () => expect((await s.ipc.previewsSettings()).on).toBe(true));
  });

  it("cannot be changed where nothing can run them", async () => {
    await open({ settings: { previews: { on: false, can_change: false, note: "Not here.", unknown: null } } });
    expect((await toggle()).disabled).toBe(true);
    expect(screen.getByText("Not here.")).toBeTruthy();
  });
});

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

  it("shows where the crash logs are, read-only, and says what they hold", async () => {
    await open();
    expect(screen.getByText(CRASH_LOGS_EXPLAINED)).toBeInTheDocument();
    const path = await screen.findByRole("textbox", { name: "Crash logs folder" });
    expect(path).toHaveValue(MOCK_CRASH_LOGS);
    expect(path).toHaveAttribute("readonly");
    cleanup();
    await open({ settings: { crashLogsFolder: null } });
    expect(await screen.findByText(CRASH_LOGS_NO_FOLDER)).toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "Crash logs folder" })).not.toBeInTheDocument();
  });

  it("disables the switch with the reason when the setting cannot be read", async () => {
    await open({ settings: { autostartError: "the Windows folder is unknown (SystemRoot)" } });
    expect(start()).toBeDisabled();
    expect(
      screen.getByText("This setting could not be read: the Windows folder is unknown (SystemRoot)"),
    ).toBeInTheDocument();
  });
});
