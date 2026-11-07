import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { IpcProvider, type UpdatePhase } from "../ipc";
import { createMockIpc } from "../ipc/mock";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { RESTART_EXPLAINED, UPDATES_EXPLAINED, UpdatesSection } from "./UpdatesSection";

/** Opens the section over a mock that records its calls. */
async function open(update: { phase?: UpdatePhase; failInstall?: string } = {}) {
  const calls: string[] = [];
  const ipc = createMockIpc({ update, onCall: (c) => calls.push(String(c[0])) });
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <UpdatesSection />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  await screen.findByText(`Earshot ${(await ipc.updateStatus()).current}`);
  return { ipc, calls, user: userEvent.setup() };
}

const networkCalls = (calls: string[]) =>
  calls.filter((c) => ["checkForUpdate", "downloadUpdate", "installUpdate"].includes(c));

describe("Settings, General, updates", () => {
  it("asks nothing until the button is pressed, and says what a check sends", async () => {
    const s = await open();
    expect(networkCalls(s.calls)).toEqual([]);
    expect(screen.getByText(UPDATES_EXPLAINED)).toBeTruthy();
    const auto = screen.getByRole("switch", { name: "Check for updates when Earshot starts" }) as HTMLInputElement;
    expect(auto.checked).toBe(false);
  });

  it("goes from a check to a download to a confirmed restart, one press at a time", async () => {
    const s = await open();
    await s.user.click(screen.getByRole("button", { name: "Check for updates" }));
    await screen.findByText(/is available/);
    expect(networkCalls(s.calls)).toEqual(["checkForUpdate"]);

    await s.user.click(screen.getByRole("button", { name: "Download" }));
    const install = await screen.findByRole("button", { name: "Restart and install" });
    expect(networkCalls(s.calls)).toEqual(["checkForUpdate", "downloadUpdate"]);

    await s.user.click(install);
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText(RESTART_EXPLAINED)).toBeTruthy();
    expect(networkCalls(s.calls)).toEqual(["checkForUpdate", "downloadUpdate"]);
    await s.user.click(within(dialog).getByRole("button", { name: "Restart and install" }));
    await waitFor(() => expect(networkCalls(s.calls)).toContain("installUpdate"));
  });

  it("does not restart on Not now", async () => {
    const s = await open({ phase: { state: "Ready", version: "9.9.9" } });
    await s.user.click(await screen.findByRole("button", { name: "Restart and install" }));
    const dialog = await screen.findByRole("alertdialog");
    await s.user.click(within(dialog).getByRole("button", { name: "Not now" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(networkCalls(s.calls)).toEqual([]);
  });

  it("shows a refusal from the app and stays open", async () => {
    const s = await open({
      phase: { state: "Ready", version: "9.9.9" },
      failInstall: "Earshot is busy.",
    });
    await s.user.click(await screen.findByRole("button", { name: "Restart and install" }));
    const dialog = await screen.findByRole("alertdialog");
    await s.user.click(within(dialog).getByRole("button", { name: "Restart and install" }));
    await screen.findByText("Earshot is busy.");
  });

  it("shows a failure and lets it be tried again", async () => {
    await open({ phase: { state: "Failed", message: "It did not work." } });
    expect(await screen.findByText("It did not work.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Check for updates" })).toBeTruthy();
  });

  it("stores the automatic check", async () => {
    const s = await open();
    await s.user.click(screen.getByRole("switch", { name: "Check for updates when Earshot starts" }));
    await waitFor(async () => expect((await s.ipc.updateStatus()).auto_check).toBe(true));
    expect(networkCalls(s.calls)).toEqual([]);
  });

  it("can stop a download", async () => {
    const s = await open({ phase: { state: "Downloading", version: "9.9.9", done: 10, total: 100 } });
    await s.user.click(await screen.findByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(s.calls).toContain("cancelUpdate"));
    await screen.findByText(/is available/);
  });
});
