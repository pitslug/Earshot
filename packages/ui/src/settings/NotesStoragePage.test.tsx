import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { IpcProvider, type MockIpcData, createMockIpc } from "../ipc";
import { MOCK_CRASH_LOGS } from "../ipc/mockSettings";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { CRASH_LOGS_EXPLAINED, CRASH_LOGS_NO_FOLDER } from "./CrashLogsFolder";

async function open(data: MockIpcData = {}) {
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
  await user.click(within(nav).getByRole("button", { name: "Notes and storage" }));
  await screen.findByRole("heading", { level: 1, name: "Notes and storage" });
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("Settings, Notes and storage, crash logs", () => {
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
});
