// The Sync page (T11.3a), through the whole app and the mock: the two ways in, the refusals under
// the form that made them, the status and its switches, the pairing code, the computers and
// removal. Every address and code is invented.

import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/sync_copy_cases.json";
import { type SyncMockData, createMockIpc } from "../ipc";
import { MOCK_SYNC_CODE, MOCK_SYNC_SECRET } from "../ipc/mockSync";
import { openSettings, renderApp } from "../test/app";
import { createManualTimer } from "../time";
import { NOT_RUNNING, REMOVE_EXPLAINED, SYNC_INTRO, VOICEPRINTS_EXPLAINED, codeLifeText, seenText } from "./SyncPage";

async function setup(sync: SyncMockData = {}) {
  const calls: string[] = [];
  const timer = createManualTimer(1_000_000);
  renderApp(createMockIpc({ meetings: [], timer, sync: { onCall: (name) => calls.push(name), ...sync } }), { timer });
  const user = userEvent.setup();
  await openSettings(user, "Sync");
  await screen.findByRole("heading", { level: 1, name: "Sync" });
  return { user, calls, timer };
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
    expect(screen.queryByRole("button", { name: /^Save/ })).toBeNull();
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
});
