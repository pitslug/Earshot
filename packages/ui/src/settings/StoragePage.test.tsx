import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import {
  type Ipc,
  IpcFailure,
  IpcProvider,
  type MockIpcData,
  RETENTION_CHOICES,
  SOME_LEFT_MESSAGE,
  createMockIpc,
} from "../ipc";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { RETENTION_DESCRIPTION, RETENTION_NEEDED } from "./StoragePage";

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

interface Setup {
  ipc: Ipc;
  user: ReturnType<typeof userEvent.setup>;
}

async function open(data: MockIpcData = {}): Promise<Setup> {
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
  await user.click(within(nav).getByRole("button", { name: "Storage" }));
  await screen.findByRole("heading", { level: 1, name: "Storage" });
  await screen.findByRole("combobox", { name: "Keep recordings for" });
  return { ipc, user };
}

/** The list. After a dialog closes the page can stay aria-hidden for a moment (tabster's
 * deferred pass), so checks after a dialog wait for it. */
const select = () => screen.getByRole<HTMLSelectElement>("combobox", { name: "Keep recordings for" });

beforeEach(() => {
  installMatchMedia(false);
});

describe("Settings, Storage", () => {
  it("lists every choice, shortest first, with 7 days chosen by default", async () => {
    await open();
    const options = within(select())
      .getAllByRole("option")
      .map((o) => o.textContent);
    expect(options).toEqual([
      "1 hour",
      "4 hours",
      "12 hours",
      "1 day",
      "3 days",
      "7 days",
      "14 days",
      "28 days",
      "3 months",
      "6 months",
      "1 year",
      "Forever",
    ]);
    expect(options).toEqual(RETENTION_CHOICES.map((c) => c.label));
    await waitFor(() => expect(select()).toHaveValue("7d"));
    expect(screen.getByText(RETENTION_DESCRIPTION)).toBeInTheDocument();
    expect(screen.getByText(RETENTION_NEEDED)).toBeInTheDocument();
    expect(document.body.textContent).not.toMatch(/—/);
  });

  it("asks before a shorter choice deletes recordings, and Cancel keeps them", async () => {
    const { ipc, user } = await open({ storage: { kept: [{ age: 2 * HOUR }, { age: 2 * DAY }, { age: 5 * DAY }] } });
    await user.selectOptions(select(), "1d");
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText("Delete recordings?")).toBeInTheDocument();
    expect(dialog).toHaveTextContent("Recordings of 2 meetings will be deleted now.");
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    await waitFor(() => expect(select()).toHaveValue("7d"));
    expect((await ipc.storageSettings()).retention).toBe("7d");
    expect(await ipc.previewRetention("1d")).toEqual({ meetings: 2, in_use: 0 });
  });

  it("deletes them on Delete and stores the choice", async () => {
    const { ipc, user } = await open({ storage: { kept: [{ age: 2 * HOUR }, { age: 2 * DAY }, { age: 5 * DAY }] } });
    await user.selectOptions(select(), "1h");
    const dialog = await screen.findByRole("alertdialog");
    expect(dialog).toHaveTextContent("Recordings of 3 meetings will be deleted now.");
    await user.click(within(dialog).getByRole("button", { name: "Delete" }));
    expect(await screen.findByText("Recordings of 3 meetings deleted.")).toBeInTheDocument();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    await waitFor(() => expect(select()).toHaveValue("1h"));
    expect((await ipc.storageSettings()).retention).toBe("1h");
    expect(await ipc.previewRetention("1h")).toEqual({ meetings: 0, in_use: 0 });
  });

  it("says 1 meeting in the singular and counts meetings in use apart", async () => {
    const { user } = await open({ storage: { kept: [{ age: 2 * DAY }, { age: 3 * DAY, inUse: true }] } });
    await user.selectOptions(select(), "1d");
    const dialog = await screen.findByRole("alertdialog");
    expect(dialog).toHaveTextContent(
      "Recordings of 1 meeting will be deleted now, and of 1 meeting more as soon as they are no longer in use.",
    );
  });

  it("shows the new choice and says some recordings are left when not all could be deleted", async () => {
    const { ipc, user } = await open({
      storage: { kept: [{ age: 2 * DAY }, { age: 3 * DAY, stuck: true }] },
    });
    await user.selectOptions(select(), "1d");
    const dialog = await screen.findByRole("alertdialog");
    await user.click(within(dialog).getByRole("button", { name: "Delete" }));
    expect(
      await screen.findByText(`Recordings of 1 meeting deleted. ${SOME_LEFT_MESSAGE}`),
    ).toBeInTheDocument();
    expect(screen.queryByText("This setting was not saved")).toBeNull();
    await waitFor(() => expect(select()).toHaveValue("1d"));
    expect((await ipc.storageSettings()).retention).toBe("1d");
  });

  it("shows the stored choice again after a save the app refused", async () => {
    const timer = createManualTimer(1_000_000);
    const base = createMockIpc({ meetings: [], timer, storage: { kept: [] } });
    let stored = "7d";
    const ipc: Ipc = {
      ...base,
      storageSettings: async () => ({ ...(await base.storageSettings()), retention: stored }),
      setStorageSettings: async () => {
        stored = "3d";
        throw new IpcFailure({ kind: "store", message: "the database is busy", retryable: true });
      },
    };
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
    await user.click(within(nav).getByRole("button", { name: "Storage" }));
    await screen.findByRole("combobox", { name: "Keep recordings for" });
    await user.selectOptions(select(), "forever");
    expect(await screen.findByText("This setting was not saved")).toBeInTheDocument();
    // Not the old value assumed: the value the app reports.
    await waitFor(() => expect(select()).toHaveValue("3d"));
  });

  it("shows the last automatic run's problem", async () => {
    await open({ storage: { problem: SOME_LEFT_MESSAGE } });
    expect(screen.getByText(SOME_LEFT_MESSAGE)).toBeInTheDocument();
  });

  it("saves a choice that deletes nothing without asking", async () => {
    const { ipc, user } = await open({ storage: { kept: [{ age: 2 * DAY }] } });
    await user.selectOptions(select(), "forever");
    await waitFor(async () => expect((await ipc.storageSettings()).retention).toBe("forever"));
    expect(screen.queryByRole("alertdialog")).toBeNull();
    await waitFor(() => expect(select()).toHaveValue("forever"));
    await user.selectOptions(select(), "3d");
    await waitFor(async () => expect((await ipc.storageSettings()).retention).toBe("3d"));
    expect(screen.queryByRole("alertdialog")).toBeNull();
  });

  it("shows why the settings could not be read", async () => {
    const timer = createManualTimer(1_000_000);
    const ipc = createMockIpc({ meetings: [], timer, storage: { error: "the database is locked" } });
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
    await user.click(within(nav).getByRole("button", { name: "Storage" }));
    expect(await screen.findByText("The storage settings could not be read")).toBeInTheDocument();
    expect(screen.getByText("the database is locked")).toBeInTheDocument();
  });
});
