// T14.63: a re-export replaces the whole Obsidian note, so the page asks first, only for a
// meeting exported before; the copy is the shared table (`fixtures/ui/reexport_copy_cases.json`,
// also read by `earshot_runtime::export`'s tests).

import { act, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/reexport_copy_cases.json";
import { sampleDetail } from "../ipc";
import { ITEM, TABSTER_HIDE_PASS_MS, accessibleDialog, setup } from "../test/meetingDetail";
import { REEXPORT_COPY, reexportBatchQuestion } from "./reexportCopy";

const exportButton = () => screen.getByRole("button", { name: "Export to Obsidian" });
const exportCalls = (calls: unknown[][]) => calls.filter((c) => c[0] === "exportMeeting").length;

describe("re-export question", () => {
  it("is the shared table, word for word", () => {
    const { _comment: _, ...copy } = table;
    expect(REEXPORT_COPY).toEqual(copy);
    expect(REEXPORT_COPY.single).toBe(
      "This meeting has already been exported. Exporting again replaces the whole note, including anything you ticked, changed or added there.",
    );
    expect(REEXPORT_COPY.confirm).toBe("Export again");
    expect(REEXPORT_COPY.cancel).toBe("Cancel");
  });

  it("names how many of a batch were exported before, once", () => {
    expect(reexportBatchQuestion(0)).toBeNull();
    expect(reexportBatchQuestion(1)).toBe(table.batch_one);
    expect(reexportBatchQuestion(2)).toBe(table.batch_many.replace("{count}", "2"));
    expect(reexportBatchQuestion(37)).toMatch(/^37 of these meetings have already been exported\./);
  });

  it("exports a meeting never exported without asking", async () => {
    const { user, calls } = await setup({ vault: "C:\\vault" });
    await screen.findByText(/TL;DR/);
    await user.click(exportButton());
    expect(await screen.findByText(/^Exported to Meetings\//)).toBeInTheDocument();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(exportCalls(calls)).toBe(1);
  });

  it("asks before exporting a meeting exported before, and Cancel exports nothing", async () => {
    const { user, calls } = await setup({
      vault: "C:\\vault",
      details: { [ITEM.id]: { ...sampleDetail(ITEM), exported: true } },
    });
    await screen.findByText(/TL;DR/);
    await user.click(exportButton());
    const dialog = await accessibleDialog();
    // No title: the sentence names the dialog.
    expect(dialog).toHaveAccessibleName(REEXPORT_COPY.single);
    expect(within(dialog).queryByRole("heading")).toBeNull();
    expect(within(dialog).getByText(REEXPORT_COPY.single)).toBeInTheDocument();
    await act(() => new Promise((r) => setTimeout(r, TABSTER_HIDE_PASS_MS)));
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(exportCalls(calls)).toBe(0);
    expect(screen.queryByText(/^Exported to/)).toBeNull();
  });

  it("exports again once asked", async () => {
    const { user, calls } = await setup({
      vault: "C:\\vault",
      details: { [ITEM.id]: { ...sampleDetail(ITEM), exported: true } },
    });
    await screen.findByText(/TL;DR/);
    await user.click(exportButton());
    const dialog = await accessibleDialog();
    await act(() => new Promise((r) => setTimeout(r, TABSTER_HIDE_PASS_MS)));
    await user.click(within(dialog).getByRole("button", { name: "Export again" }));
    expect(await screen.findByText(/^Exported to Meetings\//)).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(exportCalls(calls)).toBe(1);
  });

  it("asks on the second export from the page, before the detail is read again", async () => {
    const { user, calls } = await setup({ vault: "C:\\vault" });
    await screen.findByText(/TL;DR/);
    await user.click(exportButton());
    expect(await screen.findByText(/^Exported to Meetings\//)).toBeInTheDocument();
    await user.click(exportButton());
    const dialog = await accessibleDialog();
    expect(within(dialog).getByText(REEXPORT_COPY.single)).toBeInTheDocument();
    expect(exportCalls(calls)).toBe(1);
  });
});
