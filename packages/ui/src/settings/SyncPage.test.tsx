// The Sync page (T11.3a), through the whole app and the mock: the two ways in, the refusals under
// the form that made them, the status and its switches, the pairing code, the computers and
// removal. Every address and code is invented.

import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/sync_copy_cases.json";
import { type SyncDeviceView, type SyncMockData, createMockIpc } from "../ipc";
import { mockDeviceId } from "../ipc/mockSync";
import { MOCK_SYNC_CODE, MOCK_SYNC_SECRET } from "../ipc/mockSync";
import { openSettings, renderApp } from "../test/app";
import { setWindowWatched } from "../test/windowWatched";
import { createManualTimer } from "../time";
import offload from "../../../../fixtures/ui/offload_copy_cases.json";
import { DEVICE_NAME_TOO_LONG } from "../ipc/mockSync";
import {
  DEVICES_REFRESH_MS,
  NAME_EXPLAINED,
  NOT_SYNCING,
  NOT_RUNNING,
  OFFER_EXPLAINED,
  REMOVE_EXPLAINED,
  SYNC_INTRO,
  VOICEPRINTS_EXPLAINED,
  DICTIONARY_EXPLAINED,
  codeLifeText,
  seenText,
} from "./SyncPage";

async function setup(sync: SyncMockData = {}) {
  const calls: string[] = [];
  const timer = createManualTimer(1_000_000);
  renderApp(createMockIpc({ meetings: [], timer, sync: { onCall: (name) => calls.push(name), ...sync } }), { timer });
  const user = userEvent.setup();
  await openSettings(user, "Sync");
  await screen.findByRole("heading", { level: 1, name: "Sync" });
  return { user, calls, timer };
}

/** One computer of the list, offline and named by presence later. */
function device(shortId: string, thisDevice: boolean): SyncDeviceView {
  return {
    this_device: thisDevice,
    name: null,
    online: false,
    recording: false,
    offers_summaries: false,
    writer: null,
    id: mockDeviceId(shortId),
    short_id: shortId,
    joined_unix_s: 1_793_000_000,
    seen_s_ago: 60,
  };
}

/** A `remove_other` template with the row's name. */
function fillDevice(template: string, name: string): string {
  return template.replace("{device}", () => name);
}

function setUpForm(): HTMLElement {
  return screen.getByRole("form", { name: "Set up the first computer" });
}

function joinForm(): HTMLElement {
  return screen.getByRole("form", { name: "Join with a pairing code" });
}

async function fill(user: ReturnType<typeof userEvent.setup>, form: HTMLElement, address: string, secret: string) {
  const fields = within(form);
  await user.clear(fields.getByLabelText("Server address"));
  await user.type(fields.getByLabelText("Server address"), address);
  const second = fields.queryByLabelText("Setup secret") ?? fields.getByLabelText("Pairing code");
  if (secret !== "") await user.type(second, secret);
}

describe("the Sync page", () => {
  it("offers both ways in on a computer that is not set up", async () => {
    await setup();
    expect(await screen.findByRole("form", { name: "Set up the first computer" })).toBeInTheDocument();
    expect(joinForm()).toBeInTheDocument();
    expect(screen.getByText(SYNC_INTRO)).toBeInTheDocument();
    expect(within(setUpForm()).getByLabelText("Setup secret")).toHaveAttribute("type", "password");
    expect(within(setUpForm()).getByRole("button", { name: "Set up" })).toBeInTheDocument();
    expect(within(joinForm()).getByRole("button", { name: "Join" })).toBeInTheDocument();
    expect(screen.queryByRole("switch", { name: "Sync is on" })).toBeNull();
  });

  it("sets up the first computer with the server's secret and shows the status", async () => {
    const { user, calls } = await setup();
    await screen.findByRole("form", { name: "Set up the first computer" });
    await fill(user, setUpForm(), "https://sync.example.com", MOCK_SYNC_SECRET);
    await user.click(within(setUpForm()).getByRole("button", { name: "Set up" }));
    expect(await screen.findByRole("switch", { name: "Sync is on" })).toBeChecked();
    expect(calls).toContain("setUpSync");
    expect(screen.getByText("https://sync.example.com")).toBeInTheDocument();
    expect(screen.getByText("Not yet")).toBeInTheDocument();
    for (const term of ["Waiting to send", "Held for later", "Could not be read"]) {
      expect(screen.getByText(term)).toBeInTheDocument();
    }
    expect(screen.queryByRole("form", { name: "Set up the first computer" })).toBeNull();
    const list = await screen.findByRole("list", { name: "Computers" });
    expect(within(list).getByText("This computer")).toBeInTheDocument();
  });

  it("shows a wrong secret's copy under its form and clears the field", async () => {
    const { user } = await setup();
    await screen.findByRole("form", { name: "Set up the first computer" });
    await fill(user, setUpForm(), "https://sync.example.com", "not the secret");
    await user.click(within(setUpForm()).getByRole("button", { name: "Set up" }));
    expect(await within(setUpForm()).findByText(table.command_problems.bad_secret)).toBeInTheDocument();
    expect(within(setUpForm()).getByLabelText("Setup secret")).toHaveValue("");
    expect(within(setUpForm()).getByLabelText("Server address")).toHaveValue("https://sync.example.com");
    expect(within(joinForm()).queryByText(table.command_problems.bad_secret)).toBeNull();
  });

  it("shows an address refusal's copy", async () => {
    const { user } = await setup();
    await screen.findByRole("form", { name: "Set up the first computer" });
    await fill(user, setUpForm(), "http://sync.example.com", MOCK_SYNC_SECRET);
    await user.click(within(setUpForm()).getByRole("button", { name: "Set up" }));
    const copy = table.address_cases.find((c) => c.input === "http://sync.example.com")?.copy ?? "";
    expect(copy).not.toBe("");
    expect(await within(setUpForm()).findByText(copy)).toBeInTheDocument();
    expect(within(setUpForm()).getByLabelText("Setup secret")).toHaveValue("");
  });

  it("joins with a pairing code, after a code the server does not hold", async () => {
    const { user } = await setup();
    await screen.findByRole("form", { name: "Join with a pairing code" });
    await fill(user, joinForm(), "https://sync.example.com", "00000-11111-22222-33333-444444");
    await user.click(within(joinForm()).getByRole("button", { name: "Join" }));
    expect(await within(joinForm()).findByText(table.command_problems.no_slot)).toBeInTheDocument();
    expect(within(joinForm()).getByLabelText("Pairing code")).toHaveValue("");
    await user.type(within(joinForm()).getByLabelText("Pairing code"), MOCK_SYNC_CODE);
    await user.click(within(joinForm()).getByRole("button", { name: "Join" }));
    expect(await screen.findByRole("switch", { name: "Sync is on" })).toBeChecked();
    const list = await screen.findByRole("list", { name: "Computers" });
    expect(within(list).getByText("Computer b71d04a8")).toBeInTheDocument();
    // Named and online from presence; a computer without one is shown by its id and last seen.
    expect(within(list).getByText("Laptop")).toBeInTheDocument();
    expect(within(list).getByText("Online, recording")).toBeInTheDocument();
    expect(within(list).getByText("Last seen 2 hours ago")).toBeInTheDocument();
    expect(within(list).getByText(seenText(7_200))).toBeInTheDocument();
  });

  it("syncs Dictionary words by default and turns that off and on at once (T11.3e)", async () => {
    const { user, calls } = await setup({ setUp: true });
    const dictionary = await screen.findByRole("switch", { name: "Sync Dictionary words" });
    expect(dictionary).toBeChecked();
    expect(screen.getByText(DICTIONARY_EXPLAINED)).toBeInTheDocument();
    // Beside the voiceprint switch, before it.
    const voice = screen.getByRole("switch", { name: "Sync voiceprints" });
    expect(dictionary.compareDocumentPosition(voice) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    await user.click(dictionary);
    await waitFor(() => expect(dictionary).not.toBeChecked());
    expect(voice).not.toBeChecked();
    await user.click(dictionary);
    await waitFor(() => expect(dictionary).toBeChecked());
    expect(calls.filter((c) => c === "setSyncDictionary")).toHaveLength(2);
  });

  it("says damaged items were skipped only when the last sync skipped some (T12.77b8)", async () => {
    await setup({ setUp: true, unsendable: true });
    await screen.findByRole("switch", { name: "Sync is on" });
    expect(screen.getByText(table.unsendable_note)).toBeInTheDocument();
  });

  it("says nothing about damaged items when none were skipped", async () => {
    await setup({ setUp: true, waiting: 2 });
    await screen.findByRole("switch", { name: "Sync is on" });
    expect(screen.queryByText(table.unsendable_note)).toBeNull();
  });

  it("says how many items could not be synced, with the count (T14.43)", async () => {
    await setup({ setUp: true, refused: 3 });
    await screen.findByRole("switch", { name: "Sync is on" });
    const note = table.refused_note.many.replace("{count}", () => "3");
    expect(note).toContain("3 items");
    expect(note).not.toContain("{count}");
    expect(screen.getByText(note)).toBeInTheDocument();
    expect(screen.queryByText(table.refused_note.one)).toBeNull();
  });

  it("words one item that could not be synced in the singular", async () => {
    await setup({ setUp: true, refused: 1 });
    await screen.findByRole("switch", { name: "Sync is on" });
    expect(table.refused_note.one.startsWith("1 item ")).toBe(true);
    expect(screen.getByText(table.refused_note.one)).toBeInTheDocument();
  });

  it("says nothing about items that could not be synced when there are none", async () => {
    await setup({ setUp: true, waiting: 2 });
    await screen.findByRole("switch", { name: "Sync is on" });
    expect(screen.queryByText(/cannot be synced/)).toBeNull();
  });

  it("turns sync off and voiceprints on at once", async () => {
    const { user, calls } = await setup({ setUp: true, waiting: 3 });
    const on = await screen.findByRole("switch", { name: "Sync is on" });
    const voice = screen.getByRole("switch", { name: "Sync voiceprints" });
    expect(voice).not.toBeChecked();
    expect(screen.getByText(VOICEPRINTS_EXPLAINED)).toBeInTheDocument();
    await user.click(on);
    await waitFor(() => expect(on).not.toBeChecked());
    expect(screen.getByRole("button", { name: "Sync now" })).toBeDisabled();
    await user.click(voice);
    await waitFor(() => expect(voice).toBeChecked());
    expect(calls).toEqual(expect.arrayContaining(["setSyncOn", "setSyncVoiceprints"]));
    // Choices apply at once; the one Save is the typed name's, in its own form with no choice
    // beside it (T11.3db).
    const saves = screen.getAllByRole("button", { name: /^Save/ });
    expect(saves.map((b) => b.textContent)).toEqual(["Save name"]);
    const form = saves[0].closest("form");
    expect(form).toHaveAccessibleName("This computer's name");
    expect(form?.querySelectorAll("input[type='checkbox'], input[type='radio'], select, [role='switch']")).toHaveLength(0);
  });

  it("asks for the computers again while the page is open, and stops when it closes", async () => {
    // The other computer quits: the server's next answer says so, and the open page shows it
    // without being left and opened again.
    const other = { ...device("b71d04a8", false), name: "Laptop", online: true, seen_s_ago: 5 };
    let answer: SyncDeviceView[] = [{ ...device("3f2a9c1e", true), seen_s_ago: 0 }, other];
    const asked: number[] = [];
    const timer = createManualTimer(1_000_000);
    const ipc = createMockIpc({ meetings: [], timer, sync: { setUp: true } });
    renderApp(
      {
        ...ipc,
        syncDevices: async () => {
          asked.push(asked.length);
          return answer.map((d) => ({ ...d }));
        },
      },
      { timer },
    );
    const user = userEvent.setup();
    await openSettings(user, "Sync");
    const list = await screen.findByRole("list", { name: "Computers" });
    expect(within(list).getAllByText("Online")).toHaveLength(2);
    answer = [answer[0], { ...other, online: false, seen_s_ago: 3 }];
    act(() => timer.advance(DEVICES_REFRESH_MS - 1));
    expect(asked).toHaveLength(1);
    act(() => timer.advance(1));
    await waitFor(() => expect(within(list).getByText("Last seen just now")).toBeInTheDocument());
    expect(within(list).queryByText("Online, recording")).toBeNull();
    // Back online on the next look.
    answer = [answer[0], other];
    act(() => timer.advance(DEVICES_REFRESH_MS));
    await waitFor(() => expect(asked).toHaveLength(3));
    // This computer reads "Online" while sync runs, the other one too.
    await waitFor(() => expect(within(list).getAllByText("Online")).toHaveLength(2));
    // Leaving the page stops asking.
    await user.click(screen.getByRole("button", { name: "General" }));
    await waitFor(() => expect(screen.queryByRole("list", { name: "Computers" })).toBeNull());
    const before = asked.length;
    act(() => timer.advance(DEVICES_REFRESH_MS * 3));
    expect(asked).toHaveLength(before);
  });

  it("asks for the computers only while the window is watched, and at once when it is again (T14.50)", async () => {
    const answer = [{ ...device("3f2a9c1e", true), seen_s_ago: 0 }, { ...device("b71d04a8", false), name: "Laptop" }];
    const asked: number[] = [];
    const timer = createManualTimer(1_000_000);
    const ipc = createMockIpc({ meetings: [], timer, sync: { setUp: true } });
    renderApp(
      {
        ...ipc,
        syncDevices: async () => {
          asked.push(asked.length);
          return answer.map((d) => ({ ...d }));
        },
      },
      { timer },
    );
    const user = userEvent.setup();
    await openSettings(user, "Sync");
    const list = await screen.findByRole("list", { name: "Computers" });
    expect(within(list).getByText("Laptop")).toBeInTheDocument();
    expect(asked).toHaveLength(1);
    // The ask is made in the same turn the timer fires in, so a count read right after one
    // says whether it was made.
    act(() => timer.advance(DEVICES_REFRESH_MS));
    expect(asked).toHaveLength(2);
    // Hidden to the tray: nothing is asked, however long the page stays open there, and the
    // list stays as it was.
    act(() => setWindowWatched(false, false, true));
    act(() => timer.advance(DEVICES_REFRESH_MS * 5));
    expect(asked).toHaveLength(2);
    expect(within(list).getByText("Laptop")).toBeInTheDocument();
    // Watched again: asked at once, not at the next interval, and then every interval as before.
    act(() => setWindowWatched(true, true, true));
    expect(asked).toHaveLength(3);
    act(() => timer.advance(DEVICES_REFRESH_MS - 1));
    expect(asked).toHaveLength(3);
    act(() => timer.advance(1));
    expect(asked).toHaveLength(4);
    // Visible but behind another window (not focused) is not watched either.
    act(() => setWindowWatched(true, false, true));
    act(() => timer.advance(DEVICES_REFRESH_MS * 3));
    expect(asked).toHaveLength(4);
    act(() => setWindowWatched(true, true, true));
    expect(asked).toHaveLength(5);
    await waitFor(() => expect(within(list).getByText("Laptop")).toBeInTheDocument());
  });

  it("asks for nothing when the page opens in a window that is not watched, until it is", async () => {
    setWindowWatched(false, false);
    const { calls } = await setup({ setUp: true });
    await screen.findByRole("switch", { name: "Sync is on" });
    expect(calls).toContain("syncView");
    expect(calls).not.toContain("syncDevices");
    act(() => setWindowWatched(true, true, true));
    expect(calls.filter((c) => c === "syncDevices")).toHaveLength(1);
    const list = await screen.findByRole("list", { name: "Computers" });
    expect(within(list).getByText("This computer")).toBeInTheDocument();
  });

  it("says this computer is online while sync runs, and not syncing when it does not", async () => {
    await setup({ setUp: true });
    const list = await screen.findByRole("list", { name: "Computers" });
    const mine = within(list).getByText("This computer").closest("li") as HTMLElement;
    expect(within(mine).getByText("Online")).toBeInTheDocument();
    expect(within(mine).queryByText(/Last seen/)).toBeNull();
  });

  it("says this computer is not syncing while sync is off or not running", async () => {
    await setup({ setUp: true, running: false });
    const list = await screen.findByRole("list", { name: "Computers" });
    const mine = within(list).getByText("This computer").closest("li") as HTMLElement;
    expect(within(mine).getByText(NOT_SYNCING)).toBeInTheDocument();
    expect(within(mine).queryByText("Online")).toBeNull();
  });

  it("syncs now and asks for the computers again", async () => {
    const { user, calls } = await setup({ setUp: true, waiting: 3 });
    await screen.findByRole("list", { name: "Computers" });
    expect(screen.getByText("3")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Sync now" }));
    await waitFor(() => expect(calls.filter((c) => c === "syncDevices")).toHaveLength(2));
    expect(screen.queryByText("Not yet")).toBeNull();
  });

  it("shows a pairing code, then hides it", async () => {
    const { user } = await setup({ setUp: true });
    await user.click(await screen.findByRole("button", { name: "Show a pairing code" }));
    expect(await screen.findByText(MOCK_SYNC_CODE)).toBeInTheDocument();
    expect(screen.getByText(codeLifeText(600))).toBeInTheDocument();
    expect(codeLifeText(600)).toBe("Works for 10 minutes, once.");
    expect(screen.queryByRole("button", { name: /copy/i })).toBeNull();
    await waitFor(() => expect(screen.getByRole("button", { name: "Hide" })).toHaveFocus());
    await user.click(screen.getByRole("button", { name: "Hide" }));
    expect(screen.queryByText(MOCK_SYNC_CODE)).toBeNull();
    const show = screen.getByRole("button", { name: "Show a pairing code" });
    await waitFor(() => expect(show).toHaveFocus());
  });

  it("takes the shown code away when it stops working", async () => {
    const { user, timer } = await setup({ setUp: true });
    await user.click(await screen.findByRole("button", { name: "Show a pairing code" }));
    expect(await screen.findByText(MOCK_SYNC_CODE)).toBeInTheDocument();
    act(() => timer.advance(599_000));
    expect(screen.getByText(MOCK_SYNC_CODE)).toBeInTheDocument();
    act(() => timer.advance(1_000));
    expect(screen.queryByText(MOCK_SYNC_CODE)).toBeNull();
    expect(screen.getByRole("button", { name: "Show a pairing code" })).toBeInTheDocument();
  });

  it("removes this computer through the dialog and goes back to the forms", async () => {
    const { user, calls } = await setup({ setUp: true });
    await user.click(await screen.findByRole("button", { name: "Remove this computer" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Remove this computer from sync?" });
    expect(within(dialog).getByText(REMOVE_EXPLAINED)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(await screen.findByRole("form", { name: "Set up the first computer" })).toBeInTheDocument();
    expect(calls).toContain("removeSyncDevice");
    const heading = screen.getByRole("heading", { level: 1, name: "Sync" });
    await waitFor(() => expect(heading).toHaveFocus());
  });

  it("removes another computer from its row through a dialog that says the key is not taken back (T11.6c)", async () => {
    const { user, calls } = await setup({ setUp: true });
    const list = await screen.findByRole("list", { name: "Computers" });
    // Every other computer has a Remove; this one does not (it has its own section).
    const buttons = within(list).getAllByRole("button");
    expect(buttons.map((b) => b.getAttribute("aria-label"))).toEqual(
      ["Computer b71d04a8", "Laptop", "Studio"].map((n) => fillDevice(table.remove_other.button, n)),
    );
    expect(buttons.every((b) => b.textContent === table.remove_other.confirm)).toBe(true);
    await user.click(within(list).getByRole("button", { name: fillDevice(table.remove_other.button, "Laptop") }));
    const dialog = await screen.findByRole("alertdialog", { name: fillDevice(table.remove_other.title, "Laptop") });
    expect(within(dialog).getByText(table.remove_other.explained)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: table.remove_other.confirm }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    await waitFor(() => expect(within(list).queryByText("Laptop")).toBeNull());
    expect(calls).toContain("removeOtherSyncDevice");
    expect(within(list).getByText("Studio")).toBeInTheDocument();
    // Focus does not fall to the page when the row goes.
    await waitFor(() => expect(list).toHaveFocus());
    // This computer is still set up.
    expect(screen.getByRole("switch", { name: "Sync is on" })).toBeChecked();
  });

  it("keeps the computer on Cancel; its Remove is where the dialog gives focus back", async () => {
    const { user, calls } = await setup({ setUp: true });
    const list = await screen.findByRole("list", { name: "Computers" });
    const remove = within(list).getByRole("button", { name: fillDevice(table.remove_other.button, "Computer b71d04a8") });
    // A programmatic Fluent Dialog hands focus back only to a restore target (jsdom fires no
    // focusout when the dialog unmounts, so the hand-back itself is checked in the preview).
    expect(remove.getAttribute("data-tabster")).toContain("restorer");
    await user.click(remove);
    const dialog = await screen.findByRole("alertdialog", {
      name: fillDevice(table.remove_other.title, "Computer b71d04a8"),
    });
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(calls).not.toContain("removeOtherSyncDevice");
    expect(within(list).getByText("Computer b71d04a8")).toBeInTheDocument();
  });

  it("shows a refused removal of another computer with the command copy and keeps the row", async () => {
    const { user } = await setup({ setUp: true, refuse: { removeOtherSyncDevice: "unreachable" } });
    const list = await screen.findByRole("list", { name: "Computers" });
    await user.click(within(list).getByRole("button", { name: fillDevice(table.remove_other.button, "Studio") }));
    const dialog = await screen.findByRole("alertdialog");
    await user.click(within(dialog).getByRole("button", { name: table.remove_other.confirm }));
    expect(await screen.findByText(table.command_problems.unreachable)).toBeInTheDocument();
    expect(screen.getByText("This was not changed")).toBeInTheDocument();
    expect(within(list).getByText("Studio")).toBeInTheDocument();
    // The Remove was disabled while the call ran: focus goes to the list, not the page.
    await waitFor(() => expect(list).toHaveFocus());
  });

  it("lists a summary worker with where its local model runs (T11.6a)", async () => {
    const devices: SyncDeviceView[] = [
      { ...device("3f2a9c1e", true), name: "Desk" },
      {
        ...device("a1b2c3d4", false),
        name: "Invented rack",
        online: true,
        offers_summaries: true,
        writer: offload.local_model_places.graphics_card,
      },
      {
        ...device("b2c3d4e5", false),
        name: "Invented shelf",
        online: true,
        offers_summaries: true,
        writer: offload.local_model_places.processor,
      },
    ];
    await setup({ setUp: true, devices });
    const list = await screen.findByRole("list", { name: "Computers" });
    expect(within(list).getByText("Writes summaries (local model, graphics card)")).toBeInTheDocument();
    expect(within(list).getByText("Writes summaries (local model, processor)")).toBeInTheDocument();
  });

  it("names a computer whose name holds replacement patterns as it is", async () => {
    const odd = "Desk $& $1 $$ $` co";
    const devices: SyncDeviceView[] = [
      { ...device("3f2a9c1e", true), name: "Desk" },
      { ...device("a1b2c3d4", false), name: odd },
    ];
    const { user } = await setup({ setUp: true, devices });
    const list = await screen.findByRole("list", { name: "Computers" });
    const remove = within(list).getByRole("button", { name: `Remove ${odd}` });
    await user.click(remove);
    expect(await screen.findByRole("alertdialog", { name: `Remove ${odd} from sync?` })).toBeInTheDocument();
  });

  it("reads voiceprints off after a removal and a new set up", async () => {
    const { user } = await setup({ setUp: true, voiceprints: true });
    expect(await screen.findByRole("switch", { name: "Sync voiceprints" })).toBeChecked();
    await user.click(screen.getByRole("button", { name: "Remove this computer" }));
    const dialog = await screen.findByRole("alertdialog");
    await user.click(within(dialog).getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    await screen.findByRole("form", { name: "Set up the first computer" });
    await fill(user, setUpForm(), "https://sync.example.com", MOCK_SYNC_SECRET);
    await user.click(within(setUpForm()).getByRole("button", { name: "Set up" }));
    expect(await screen.findByRole("switch", { name: "Sync voiceprints" })).not.toBeChecked();
    await waitFor(() => expect(screen.getByRole("heading", { level: 1, name: "Sync" })).toHaveFocus());
  });

  it("says sync is not running and starts it on Sync now", async () => {
    const { user, calls } = await setup({ setUp: true, running: false });
    expect(await screen.findByText(NOT_RUNNING)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Sync now" }));
    await waitFor(() => expect(screen.queryByText(NOT_RUNNING)).toBeNull());
    expect(calls).toContain("syncNow");
  });

  it("says nothing about running while sync is on and running, or off", async () => {
    const { user } = await setup({ setUp: true });
    await screen.findByRole("switch", { name: "Sync is on" });
    expect(screen.queryByText(NOT_RUNNING)).toBeNull();
    await user.click(screen.getByRole("switch", { name: "Sync is on" }));
    await waitFor(() => expect(screen.getByRole("switch", { name: "Sync is on" })).not.toBeChecked());
    expect(screen.queryByText(NOT_RUNNING)).toBeNull();
  });

  it("names a problem with its copy and says when it stopped sync", async () => {
    await setup({ setUp: true, problem: "key_mismatch" });
    expect(await screen.findByText(table.problems.key_mismatch)).toBeInTheDocument();
    expect(screen.getByText("Sync is stopped until this is fixed")).toBeInTheDocument();
  });

  it("names a passing problem without saying sync stopped", async () => {
    await setup({ setUp: true, problem: "unreachable" });
    expect(await screen.findByText(table.problems.unreachable)).toBeInTheDocument();
    expect(screen.getByText("The last sync had a problem")).toBeInTheDocument();
    expect(screen.queryByText("Sync is stopped until this is fixed")).toBeNull();
  });

  it("shows a failed list of computers in its own part", async () => {
    await setup({ setUp: true, refuse: { syncDevices: "unreachable" } });
    expect(await screen.findByText("The computers could not be listed")).toBeInTheDocument();
    expect(screen.getByText(table.command_problems.unreachable)).toBeInTheDocument();
    expect(screen.getByRole("switch", { name: "Sync is on" })).toBeInTheDocument();
  });

  it("writes summaries for the other computers when switched on, and lists who does (T11.3db)", async () => {
    const { user, calls } = await setup({ setUp: true });
    const offer = await screen.findByRole("switch", { name: "Write summaries for my other computers" });
    expect(offer).not.toBeChecked();
    expect(screen.getByText(OFFER_EXPLAINED)).toBeInTheDocument();
    await user.click(offer);
    await waitFor(() => expect(offer).toBeChecked());
    expect(calls).toContain("setSyncOfferSummaries");
    const list = await screen.findByRole("list", { name: "Computers" });
    expect(within(list).getByText(`Writes summaries (${offload.writers.local})`)).toBeInTheDocument();
    expect(within(list).getByText(`Writes summaries (${offload.writers.claude_code})`)).toBeInTheDocument();
  });

  it("saves this computer's name with its own button and shows a refusal under it", async () => {
    const { user, calls } = await setup({ setUp: true, deviceName: "Desk" });
    const form = await screen.findByRole("form", { name: "This computer's name" });
    const field = within(form).getByRole("textbox", { name: "This computer's name" });
    expect(field).toHaveValue("Desk");
    expect(within(form).getByText(NAME_EXPLAINED)).toBeInTheDocument();
    const save = within(form).getByRole("button", { name: "Save name" });
    expect(save).toBeDisabled();
    await user.clear(field);
    await user.type(field, "x".repeat(65));
    await user.click(save);
    expect(await within(form).findByText(DEVICE_NAME_TOO_LONG)).toBeInTheDocument();
    await user.clear(field);
    await user.type(field, "Invented laptop ");
    await user.click(within(form).getByRole("button", { name: "Save name" }));
    await waitFor(() => expect(field).toHaveValue("Invented laptop"));
    expect(within(form).queryByText(DEVICE_NAME_TOO_LONG)).toBeNull();
    expect(calls.filter((c) => c === "setSyncDeviceName")).toHaveLength(2);
  });
});
