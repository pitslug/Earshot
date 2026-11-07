import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { type Ipc, IpcFailure, IpcProvider, type MockIpcData, createMockIpc } from "../ipc";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { PICKED_VAULT } from "../ipc/mockSettings";
import { EXPORT_EXPLAINED, exportPlace } from "./ObsidianPage";

interface Setup {
  ipc: Ipc;
  user: ReturnType<typeof userEvent.setup>;
}

async function open(data: MockIpcData = {}): Promise<Setup> {
  const timer = createManualTimer(1_000_000);
  const ipc = createMockIpc({ timer, ...data });
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
  await user.click(within(nav).getByRole("button", { name: "Obsidian" }));
  await screen.findByRole("heading", { level: 1, name: "Obsidian" });
  await screen.findByLabelText("Vault folder");
  return { ipc, user };
}

const vault = () => screen.getByLabelText("Vault folder");
const folder = () => screen.getByLabelText("Subfolder");
const save = () => screen.getByRole("button", { name: "Save" });
const choose = () => screen.getByRole("button", { name: "Choose folder..." });

beforeEach(() => {
  installMatchMedia(false);
});

describe("Settings, Obsidian", () => {
  it("starts with no vault and the default subfolder, and saves a checked vault that export then uses", async () => {
    const s = await open();
    expect(screen.getByText(EXPORT_EXPLAINED)).toBeInTheDocument();
    expect(vault()).toHaveValue("");
    expect(folder()).toHaveValue("Meetings");
    expect(save()).toBeDisabled();
    const meeting = (await s.ipc.listMeetings())[0].id;
    await expect(s.ipc.exportMeeting(meeting)).rejects.toBeInstanceOf(IpcFailure);

    await s.user.type(vault(), "C:\\Users\\you\\Notes");
    await s.user.clear(folder());
    await s.user.type(folder(), "\\Work\\Calls\\");
    await s.user.click(save());
    const place = exportPlace("C:\\Users\\you\\Notes", "Work/Calls");
    expect(await screen.findByText(`Saved. ${place}`)).toBeInTheDocument();
    // The app's normalised subfolder is shown.
    expect(folder()).toHaveValue("Work/Calls");
    expect(save()).toBeDisabled();
    expect((await s.ipc.obsidianSettings()).value).toEqual({ vault: "C:\\Users\\you\\Notes", folder: "Work/Calls" });
    const done = await s.ipc.exportMeeting(meeting);
    expect(done.note_path.startsWith("Work/Calls/")).toBe(true);
  });

  it("shows why a vault or subfolder was refused and stores nothing", async () => {
    const s = await open({ settings: { missingFolders: ["D:\\Gone"] } });
    await s.user.type(vault(), "Notes");
    await s.user.click(save());
    expect(await screen.findByText("This setting was not saved")).toBeInTheDocument();
    expect(
      screen.getByText("The vault must be a full folder path, for example C:\\Users\\you\\Notes."),
    ).toBeInTheDocument();
    await s.user.clear(vault());
    await s.user.type(vault(), "D:\\Gone");
    await s.user.click(save());
    expect(await screen.findByText("This folder does not exist.")).toBeInTheDocument();
    await s.user.clear(vault());
    await s.user.type(vault(), "D:\\Notes");
    await s.user.clear(folder());
    await s.user.type(folder(), "../outside");
    await s.user.click(save());
    expect(
      await screen.findByText("The subfolder must be a folder inside the vault (dot component)."),
    ).toBeInTheDocument();
    expect((await s.ipc.obsidianSettings()).value).toEqual({ vault: "", folder: "Meetings" });
  });

  it("shows where notes go for a stored vault, and the top folder for an empty subfolder", async () => {
    const s = await open({ settings: { vault: "C:\\Vault", folder: "" } });
    expect(vault()).toHaveValue("C:\\Vault");
    expect(screen.getByText(exportPlace("C:\\Vault", ""))).toBeInTheDocument();
    expect(exportPlace("C:\\Vault", "")).toBe("Notes go to the top folder of C:\\Vault.");
    const meeting = (await s.ipc.listMeetings())[0].id;
    await waitFor(async () => expect((await s.ipc.exportMeeting(meeting)).note_path).not.toContain("/"));
  });

  it("fills the vault from the folder dialog, starting at the typed folder, and stores it only on Save", async () => {
    const starts: string[] = [];
    const s = await open({ settings: { vault: "C:\\Vault", onPickVault: (start) => starts.push(start) } });
    await s.user.click(choose());
    await waitFor(() => expect(vault()).toHaveValue(PICKED_VAULT));
    expect(starts).toEqual(["C:\\Vault"]);
    // Not stored until Save.
    expect((await s.ipc.obsidianSettings()).value.vault).toBe("C:\\Vault");
    expect(save()).toBeEnabled();
    await s.user.click(save());
    expect(await screen.findByText(`Saved. ${exportPlace(PICKED_VAULT, "Meetings")}`)).toBeInTheDocument();
    expect((await s.ipc.obsidianSettings()).value.vault).toBe(PICKED_VAULT);
  });

  it("leaves the vault as it was when the folder dialog is cancelled", async () => {
    const s = await open({ settings: { vault: "C:\\Vault", pickedVault: null } });
    await s.user.click(choose());
    await waitFor(() => expect(choose()).toBeEnabled());
    expect(vault()).toHaveValue("C:\\Vault");
    expect(save()).toBeDisabled();
  });

  it("says to type the folder when the folder dialog cannot open", async () => {
    const s = await open({ settings: { pickVaultFails: true } });
    await s.user.click(choose());
    expect(
      await screen.findByText("The folder window could not open. Type the vault folder instead."),
    ).toBeInTheDocument();
    expect(screen.queryByText("This setting was not saved")).toBeNull();
    await s.user.type(vault(), "C:\\Users\\you\\Notes");
    expect(vault()).toHaveValue("C:\\Users\\you\\Notes");
  });
});
