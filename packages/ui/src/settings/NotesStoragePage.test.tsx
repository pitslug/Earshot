import { cleanup, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { type MockIpcData, createMockIpc } from "../ipc";
import { MOCK_CRASH_LOGS } from "../ipc/mockSettings";
import { openSettings, renderApp } from "../test/app";
import { createManualTimer } from "../time";
import { CRASH_LOGS_EXPLAINED, CRASH_LOGS_NO_FOLDER } from "./CrashLogsFolder";

async function open(data: MockIpcData = {}) {
  const timer = createManualTimer(1_000_000);
  const ipc = createMockIpc({ meetings: [], timer, ...data });
  renderApp(ipc, { timer });
  const user = userEvent.setup();
  await openSettings(user, "Notes and storage");
  await screen.findByRole("heading", { level: 1, name: "Notes and storage" });
}

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
