import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { type FolderListItem, type Ipc, IpcProvider, type MeetingListItem, createMockIpc } from "../ipc";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { AppShell } from "./AppShell";
import { SEARCH_DEBOUNCE_MS } from "./MeetingSearch";

const at = (day: number, hour: number) => new Date(2026, 10, day, hour, 0).getTime();
const NOW = at(2, 16);

// Invented names.
const ORCHARD: FolderListItem = { id: "f-orchard", name: "Orchard" };
const PEARS: FolderListItem = { id: "f-pears", name: "Pears" };

const row = (id: string, title: string, started: number, folder: string | null): MeetingListItem => ({
  id,
  title,
  started_at_ms: started,
  duration_ms: 30 * 60_000,
  state: "Ready",
  folder_id: folder,
});

const MEETINGS: MeetingListItem[] = [
  row("m4", "Pruning plan", at(2, 14), ORCHARD.id),
  row("m3", "Budget review", at(2, 9), null),
  row("m2", "Grafting notes", at(1, 11), ORCHARD.id),
  row("m1", "Pear harvest", at(1, 8), PEARS.id),
];

function setup(data: Parameters<typeof createMockIpc>[0] = {}) {
  const calls: unknown[][] = [];
  const timer = createManualTimer(NOW);
  const ipc = createMockIpc({
    meetings: MEETINGS,
    folders: [PEARS, ORCHARD],
    timer,
    onCall: (c) => calls.push(c),
    ...data,
  });
  const view = renderShell(ipc, timer);
  return { ipc, calls, timer, view, user: userEvent.setup() };
}

function renderShell(ipc: Ipc, timer = createManualTimer(NOW)) {
  return render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell now={() => NOW} timer={timer} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
}

const folderList = () => screen.findByRole("list", { name: "Folders" });
/** The folder buttons, in order. */
const folderNames = async () =>
  within(await folderList())
    .getAllByRole("button")
    .map((b) => b.textContent);
const folderButton = (name: string) => screen.getByRole("button", { name });
/** The titles of the meeting rows shown, grouped by their day heading. */
function shownByDay(): Record<string, string[]> {
  const list = screen.queryByRole("list", { name: "Meetings" });
  if (list === null) return {};
  const out: Record<string, string[]> = {};
  for (const group of within(list).getAllByRole("group")) {
    const label = within(group).getByRole("heading").textContent ?? "";
    out[label] = within(group)
      .getAllByRole("button", { name: /^(?!Delete )/ })
      .map((b) => b.querySelector(".earshot-row-title")?.textContent ?? "");
  }
  return out;
}
const meetingRow = (title: string) => screen.getByRole("button", { name: new RegExp(`^${title}`) });

/** Opens a row's menu from the keyboard (Shift+F10), as a person would. */
async function openMenu(user: ReturnType<typeof userEvent.setup>, button: HTMLElement) {
  button.focus();
  await user.keyboard("{Shift>}{F10}{/Shift}");
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("folders", () => {
  it("opens on All meetings, and a folder shows only its meetings, still by day", async () => {
    const { user } = setup();
    expect(await folderNames()).toEqual(["All meetings", "Orchard", "Pears"]);
    expect(folderButton("All meetings")).toHaveAttribute("aria-current", "true");
    await waitFor(() =>
      expect(shownByDay()).toEqual({
        Today: ["Pruning plan", "Budget review"],
        Yesterday: ["Grafting notes", "Pear harvest"],
      }),
    );

    await user.click(folderButton("Orchard"));
    expect(folderButton("Orchard")).toHaveAttribute("aria-current", "true");
    expect(folderButton("All meetings")).not.toHaveAttribute("aria-current");
    expect(shownByDay()).toEqual({ Today: ["Pruning plan"], Yesterday: ["Grafting notes"] });

    await user.click(folderButton("All meetings"));
    expect(Object.values(shownByDay()).flat()).toHaveLength(4);
  });

  it("opens on All meetings every time, whatever folder was shown before", async () => {
    const { ipc, user, view } = setup();
    await user.click(await screen.findByRole("button", { name: "Pears" }));
    expect(folderButton("Pears")).toHaveAttribute("aria-current", "true");
    view.unmount();
    renderShell(ipc);
    await folderList();
    expect(folderButton("All meetings")).toHaveAttribute("aria-current", "true");
    await waitFor(() => expect(Object.values(shownByDay()).flat()).toHaveLength(4));
  });

  it("with no folders shows only a quiet New folder", async () => {
    setup({ folders: [] });
    expect(await screen.findByRole("button", { name: "New folder" })).toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "Folders" })).toBeNull();
    expect(screen.queryByRole("button", { name: "All meetings" })).toBeNull();
    expect(screen.queryByRole("heading", { name: "Folders" })).toBeNull();
  });

  it("an empty folder says so", async () => {
    const { user } = setup({ folders: [ORCHARD, PEARS, { id: "f-empty", name: "Quinces" }] });
    await user.click(await screen.findByRole("button", { name: "Quinces" }));
    expect(screen.getByText("No meetings in this folder.")).toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "Meetings" })).toBeNull();
  });

  it("creates a folder, and a refused name shows fixed copy under the input", async () => {
    const { user, calls } = setup();
    await user.click(await screen.findByRole("button", { name: "New folder" }));
    const input = screen.getByRole("textbox", { name: "New folder name" });
    expect(input).toHaveFocus();

    await user.type(input, "orchard{Enter}");
    expect(await screen.findByText("Another folder already has this name.")).toBeInTheDocument();
    await user.clear(input);
    await user.type(input, "   {Enter}");
    expect(await screen.findByText("Enter a name.")).toBeInTheDocument();
    await user.clear(input);
    await user.type(input, "Walnuts");
    // Typing clears the refusal.
    expect(screen.queryByText("Enter a name.")).toBeNull();
    await user.keyboard("{Enter}");

    await waitFor(() => expect(screen.queryByRole("textbox", { name: "New folder name" })).toBeNull());
    expect(await folderNames()).toEqual(["All meetings", "Orchard", "Pears", "Walnuts"]);
    await waitFor(() => expect(folderButton("Walnuts")).toHaveFocus());
    expect(calls.filter((c) => c[0] === "createFolder")).toEqual([
      ["createFolder", "orchard"],
      ["createFolder", "   "],
      ["createFolder", "Walnuts"],
    ]);
  });

  it("Escape closes the new folder input and creates nothing", async () => {
    const { user, calls } = setup();
    await user.click(await screen.findByRole("button", { name: "New folder" }));
    await user.type(screen.getByRole("textbox", { name: "New folder name" }), "Walnuts{Escape}");
    await waitFor(() => expect(screen.getByRole("button", { name: "New folder" })).toHaveFocus());
    expect(calls.filter((c) => c[0] === "createFolder")).toEqual([]);
  });

  it("renames a folder from its menu, refusing a name another folder has", async () => {
    const { user } = setup();
    await folderList();
    await openMenu(user, folderButton("Pears"));
    await user.click(await screen.findByRole("menuitem", { name: "Rename" }));
    const input = await screen.findByRole("textbox", { name: "Folder name" });
    expect(input).toHaveValue("Pears");

    await user.clear(input);
    await user.type(input, "ORCHARD{Enter}");
    expect(await screen.findByText("Another folder already has this name.")).toBeInTheDocument();
    await user.clear(input);
    await user.type(input, "Pear trees");
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "Folder name" })).toBeNull());
    expect(await folderNames()).toEqual(["All meetings", "Orchard", "Pear trees"]);
    await waitFor(() => expect(folderButton("Pear trees")).toHaveFocus());
  });

  it("the folder menu opens on a right-click too", async () => {
    setup();
    await folderList();
    fireEvent.contextMenu(folderButton("Orchard"), { button: 0 });
    expect(await screen.findByRole("menuitem", { name: "Rename" })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: "Delete..." })).toBeInTheDocument();
  });

  it("moves a meeting to a folder and out again from the row menu", async () => {
    const { user, calls } = setup();
    await folderList();
    await openMenu(user, meetingRow("Budget review"));
    await user.click(await screen.findByRole("menuitem", { name: "Move to folder" }));
    // The submenu lists the folders by name, then New folder and No folder; the current one is
    // checked.
    const choices = await screen.findAllByRole("menuitemradio");
    expect(choices.map((c) => c.textContent)).toEqual(["Orchard", "Pears", "No folder"]);
    expect(screen.getByRole("menuitemradio", { name: "No folder" })).toHaveAttribute("aria-checked", "true");
    expect(screen.getByRole("menuitem", { name: "New folder..." })).toBeInTheDocument();
    await user.click(screen.getByRole("menuitemradio", { name: "Pears" }));
    await waitFor(() => expect(calls).toContainEqual(["moveToFolder", "m3", PEARS.id]));

    await user.click(folderButton("Pears"));
    await waitFor(() => expect(Object.values(shownByDay()).flat()).toEqual(["Budget review", "Pear harvest"]));

    // Moved out of the folder shown, the row goes and focus does not fall to the page.
    await openMenu(user, meetingRow("Budget review"));
    await user.click(await screen.findByRole("menuitem", { name: "Move to folder" }));
    await user.click(await screen.findByRole("menuitemradio", { name: "No folder" }));
    await waitFor(() => expect(calls).toContainEqual(["moveToFolder", "m3", null]));
    await waitFor(() => expect(Object.values(shownByDay()).flat()).toEqual(["Pear harvest"]));
    await waitFor(() => expect(document.activeElement).not.toBe(document.body));
  });

  it("New folder in the row menu creates the folder and moves the meeting into it", async () => {
    const { user, calls } = setup();
    await folderList();
    await openMenu(user, meetingRow("Budget review"));
    await user.click(await screen.findByRole("menuitem", { name: "Move to folder" }));
    await user.click(await screen.findByRole("menuitem", { name: "New folder..." }));
    const input = await screen.findByRole("textbox", { name: "New folder name (the meeting moves into it)" });
    await waitFor(() => expect(input).toHaveFocus());
    await user.type(input, "Budgets{Enter}");
    await waitFor(() => expect(input).not.toBeInTheDocument());
    const created = calls.findIndex((c) => c[0] === "createFolder");
    expect(calls[created]).toEqual(["createFolder", "Budgets"]);
    const moved = calls.find((c) => c[0] === "moveToFolder");
    expect(moved?.[1]).toBe("m3");
    expect(typeof moved?.[2]).toBe("string");
    await user.click(await screen.findByRole("button", { name: "Budgets" }));
    expect(Object.values(shownByDay()).flat()).toEqual(["Budget review"]);
  });

  it("a recording meeting cannot be moved", async () => {
    setup({ meetings: [{ ...row("m9", "Live talk", at(2, 15), null), state: "Recording", duration_ms: null }] });
    const user = userEvent.setup();
    await folderList();
    await openMenu(user, meetingRow("Live talk"));
    expect(await screen.findByRole("menuitem", { name: "Move to folder" })).toHaveAttribute("aria-disabled", "true");
  });

  it("the delete dialog says how many meetings the folder holds and keeps them by default", async () => {
    const { user, calls } = setup();
    await user.click(await screen.findByRole("button", { name: "Orchard" }));
    await openMenu(user, folderButton("Orchard"));
    await user.click(await screen.findByRole("menuitem", { name: "Delete..." }));
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText('Delete the folder "Orchard"?')).toBeInTheDocument();
    expect(within(dialog).getByText("This folder holds 2 meetings.")).toBeInTheDocument();
    expect(within(dialog).getByRole("radio", { name: "Keep the meetings. They stay in All meetings." })).toBeChecked();
    expect(within(dialog).getByRole("radio", { name: /^Delete the meetings too/ })).not.toBeChecked();
    await user.click(within(dialog).getByRole("button", { name: "Delete folder" }));

    await waitFor(() => expect(calls).toContainEqual(["deleteFolderKeepingMeetings", ORCHARD.id]));
    expect(calls.some((c) => c[0] === "deleteFolderWithMeetings" || c[0] === "deleteMeeting")).toBe(false);
    // The folder shown went: All meetings again, with every meeting still there.
    await waitFor(() => expect(screen.queryByRole("button", { name: "Orchard" })).toBeNull());
    expect(folderButton("All meetings")).toHaveAttribute("aria-current", "true");
    await waitFor(() => expect(Object.values(shownByDay()).flat()).toHaveLength(4));
    await waitFor(() => expect(folderButton("All meetings")).toHaveFocus());
  });

  it("an empty folder's dialog offers no choice", async () => {
    const { user, calls } = setup({ folders: [ORCHARD, PEARS, { id: "f-empty", name: "Quinces" }] });
    await folderList();
    await openMenu(user, folderButton("Quinces"));
    await user.click(await screen.findByRole("menuitem", { name: "Delete..." }));
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText("The folder is empty, so only the folder goes.")).toBeInTheDocument();
    expect(within(dialog).queryByRole("radio")).toBeNull();
    await user.click(within(dialog).getByRole("button", { name: "Delete folder" }));
    await waitFor(() => expect(calls).toContainEqual(["deleteFolderKeepingMeetings", "f-empty"]));
  });

  it("deleting a folder with its meetings that leaves a busy one says so with counts", async () => {
    const { ipc, user, calls } = setup();
    // A re-transcription that never finishes on the manual clock keeps "Grafting notes" busy.
    await ipc.retranscribe("m2");
    await folderList();
    await openMenu(user, folderButton("Orchard"));
    await user.click(await screen.findByRole("menuitem", { name: "Delete..." }));
    const dialog = await screen.findByRole("alertdialog");
    await user.click(within(dialog).getByRole("radio", { name: /^Delete the meetings too/ }));
    await user.click(within(dialog).getByRole("button", { name: "Delete folder and meetings" }));

    await waitFor(() => expect(calls).toContainEqual(["deleteFolderWithMeetings", ORCHARD.id]));
    expect(await screen.findByText("The folder was not deleted")).toBeInTheDocument();
    expect(
      screen.getByText("Deleted 1 meeting. 1 could not be deleted because it is busy, so the folder stays."),
    ).toBeInTheDocument();
    expect(folderButton("Orchard")).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByRole("button", { name: /^Pruning plan/ })).toBeNull());
    await user.click(folderButton("Orchard"));
    expect(Object.values(shownByDay()).flat()).toEqual(["Grafting notes"]);
  });

  it("search finds meetings in every folder whatever folder is shown", async () => {
    const timer = createManualTimer(NOW);
    renderShell(createMockIpc({ meetings: MEETINGS, folders: [PEARS, ORCHARD], timer }), timer);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Pears" }));
    await user.type(screen.getByRole("searchbox", { name: "Search meetings" }), "budget");
    await act(async () => timer.advance(SEARCH_DEBOUNCE_MS));
    const results = await screen.findByRole("list", { name: "Search results" });
    expect(within(results).getByRole("button", { name: /^Budget review/ })).toBeInTheDocument();
    // The folder group gives way to the results while searching.
    expect(screen.queryByRole("list", { name: "Folders" })).toBeNull();
  });

  it("a database saved by a newer Earshot shows its own screen with the update check in reach", async () => {
    const user = userEvent.setup();
    renderShell(createMockIpc({ newerData: true }));
    expect(await screen.findByRole("heading", { level: 1, name: "Earshot needs updating" })).toBeInTheDocument();
    expect(
      screen.getByText("This data was saved by a newer version of Earshot. Install the latest version to open it."),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "New meeting" })).toBeNull();
    expect(screen.queryByText("Meetings could not be loaded")).toBeNull();
    const text = document.body.textContent ?? "";
    expect(text).not.toMatch(/\u2014|please|!/i);

    await user.click(screen.getByRole("button", { name: "Check for updates" }));
    // Settings opens on General, where the update check is.
    expect(await screen.findByRole("button", { name: "Check for updates" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Back to meetings" })).toBeInTheDocument();
  });
});
