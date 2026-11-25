// Renaming a meeting from its row in the list (T12.102): the row menu's Rename..., its dialog,
// what a refusal looks like and where focus goes. Every title is invented.

import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { ITEM, OTHER, listRow, setup } from "../test/meetingDetail";

const meetingsList = () => screen.getByRole("list", { name: "Meetings" });

async function openRename(user: Awaited<ReturnType<typeof setup>>["user"], row: HTMLElement): Promise<HTMLElement> {
  await user.pointer({ keys: "[MouseRight]", target: row });
  await user.click(await screen.findByRole("menuitem", { name: "Rename..." }));
  return screen.findByRole("dialog", { name: "Rename meeting" });
}

describe("renaming a meeting from the list", () => {
  it("renames from the row menu, shows the new title in the list and the open meeting", async () => {
    const { user, calls } = await setup();
    const dialog = await openRename(user, listRow(ITEM));
    const field = within(dialog).getByRole("textbox", { name: "Meeting title" });
    await waitFor(() => expect(field).toHaveFocus());
    expect(field).toHaveValue(ITEM.title);
    await user.clear(field);
    await user.type(field, "Quarterly planning{Enter}");
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Rename meeting" })).toBeNull());
    expect(calls).toContainEqual(["renameMeeting", ITEM.id, "Quarterly planning"]);
    await waitFor(() =>
      expect(within(meetingsList()).getByRole("button", { name: /^Quarterly planning/ })).toBeInTheDocument(),
    );
    // The open meeting is this one: its heading follows.
    await waitFor(() => expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("Quarterly planning"));
    // Focus goes back to the renamed row.
    await waitFor(() =>
      expect(within(meetingsList()).getByRole("button", { name: /^Quarterly planning/ })).toHaveFocus(),
    );
  });

  it("renames a meeting that is not the one open, leaving the open one as it is", async () => {
    const { user, calls } = await setup();
    const dialog = await openRename(user, listRow(OTHER));
    const field = within(dialog).getByRole("textbox", { name: "Meeting title" });
    await user.clear(field);
    await user.type(field, "Budget review");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls).toContainEqual(["renameMeeting", OTHER.id, "Budget review"]));
    await waitFor(() =>
      expect(within(meetingsList()).getByRole("button", { name: /^Budget review/ })).toBeInTheDocument(),
    );
    expect(screen.getByRole("heading", { level: 1 }).textContent).toBe(ITEM.title);
  });

  it("changes nothing on Cancel or Escape", async () => {
    const { user, calls } = await setup();
    let dialog = await openRename(user, listRow(ITEM));
    await user.type(within(dialog).getByRole("textbox", { name: "Meeting title" }), " changed");
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Rename meeting" })).toBeNull());
    dialog = await openRename(user, listRow(ITEM));
    // Opened again, it starts from the stored title, not what was typed before.
    expect(within(dialog).getByRole("textbox", { name: "Meeting title" })).toHaveValue(ITEM.title);
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Rename meeting" })).toBeNull());
    expect(calls.some((c) => c[0] === "renameMeeting")).toBe(false);
  });

  it("keeps the dialog and what was typed when the title is refused, and says why", async () => {
    const { user } = await setup();
    const dialog = await openRename(user, listRow(ITEM));
    const field = within(dialog).getByRole("textbox", { name: "Meeting title" });
    const long = "a".repeat(201);
    await user.clear(field);
    await user.click(field);
    await user.paste(long);
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    expect(await within(dialog).findByText("Titles can be up to 200 characters long.")).toBeInTheDocument();
    expect(field).toHaveValue(long);
    expect(screen.getByRole("dialog", { name: "Rename meeting" })).toBe(dialog);
    expect(listRow(ITEM)).toBeInTheDocument();
  });
});
