import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { IpcProvider } from "../ipc";
import { createMockIpc } from "../ipc/mock";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import {
  AUTO_CHECK_LABEL,
  RESTART_EXPLAINED,
  SCHEDULE_EXPLAINED,
  SCHEDULE_OFF_EXPLAINED,
  SCHEDULE_OPTIONS,
  UPDATES_EXPLAINED,
  UpdatesSection,
  scheduleUnknownText,
} from "./UpdatesSection";

type MockUpdate = NonNullable<NonNullable<Parameters<typeof createMockIpc>[0]>["update"]>;

/** Opens the section over a mock that records its calls. */
async function open(update: MockUpdate = {}) {
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
    const auto = screen.getByRole("switch", { name: AUTO_CHECK_LABEL }) as HTMLInputElement;
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
    await s.user.click(screen.getByRole("switch", { name: AUTO_CHECK_LABEL }));
    await waitFor(async () => expect((await s.ipc.updateStatus()).auto_check).toBe(true));
    expect(networkCalls(s.calls)).toEqual([]);
  });

  it("offers the schedule under the switch, every day by default, applied at once with no check", async () => {
    const s = await open();
    const list = screen.getByRole("combobox", { name: "How often" }) as HTMLSelectElement;
    expect(list.value).toBe("daily");
    expect([...list.options].map((o) => o.textContent)).toEqual(SCHEDULE_OPTIONS.map((o) => o.label));
    expect([...list.options].map((o) => o.textContent)).toEqual([
      "Every hour",
      "Every 4 hours",
      "Every 12 hours",
      "Every day",
      "Every week",
    ]);
    // Off: the list still applies, and the copy says nothing is asked.
    expect(screen.getByText(SCHEDULE_OFF_EXPLAINED)).toBeTruthy();
    expect(list.disabled).toBe(false);
    await s.user.selectOptions(list, "four_hours");
    await waitFor(async () => expect((await s.ipc.updateStatus()).schedule).toBe("four_hours"));
    expect(list.value).toBe("four_hours");
    expect(networkCalls(s.calls)).toEqual([]);
    // No Save button beside it: a choice applies at once.
    expect(screen.queryByRole("button", { name: /^Save/ })).toBeNull();
  });

  it("says the schedule runs when automatic checks are on", async () => {
    await open({ autoCheck: true, schedule: "weekly" });
    const list = (await screen.findByRole("combobox", { name: "How often" })) as HTMLSelectElement;
    await waitFor(() => expect(list.value).toBe("weekly"));
    expect(screen.getByText(SCHEDULE_EXPLAINED)).toBeTruthy();
    expect(screen.queryByText(SCHEDULE_OFF_EXPLAINED)).toBeNull();
  });

  it("reports a stored schedule this version does not know, and reads it as every day", async () => {
    const s = await open({ scheduleUnknown: "fortnightly" });
    expect(await screen.findByText(scheduleUnknownText("fortnightly"))).toBeTruthy();
    const list = screen.getByRole("combobox", { name: "How often" }) as HTMLSelectElement;
    expect(list.value).toBe("daily");
    await s.user.selectOptions(list, "hourly");
    await waitFor(() => expect(screen.queryByText(scheduleUnknownText("fortnightly"))).toBeNull());
  });

  it("shows an offer an automatic check found while the page is open", async () => {
    const s = await open({ autoCheck: true });
    expect(screen.getByRole("button", { name: "Check for updates" })).toBeTruthy();
    // The app's schedule reports through the same event as the button's check.
    await s.ipc.checkForUpdate();
    expect(await screen.findByText(/Earshot 9\.9\.9 is available/)).toBeTruthy();
  });

  it("can stop a download", async () => {
    const s = await open({ phase: { state: "Downloading", version: "9.9.9", done: 10, total: 100 } });
    await s.user.click(await screen.findByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(s.calls).toContain("cancelUpdate"));
    await screen.findByText(/is available/);
  });
});
