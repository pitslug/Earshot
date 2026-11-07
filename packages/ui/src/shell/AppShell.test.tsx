import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { type Ipc, IpcFailure, IpcProvider, type MeetingListItem, createMockIpc } from "../ipc";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { AppShell } from "./AppShell";

const at = (day: number, hour: number, minute = 0) =>
  new Date(2026, 10, day, hour, minute).getTime();
const NOW = at(2, 16, 0);

const MEETINGS: MeetingListItem[] = [
  { id: "m4", title: "Design review", started_at_ms: at(2, 14, 0), duration_ms: 34 * 60_000, state: "Ready" },
  { id: "m3", title: "", started_at_ms: at(2, 9, 30), duration_ms: 47 * 60_000, state: "Ready" },
  { id: "m2", title: "Supplier call", started_at_ms: at(1, 11, 5), duration_ms: 22 * 60_000, state: "Ready" },
  { id: "m1", title: "Quarterly planning", started_at_ms: new Date(2026, 9, 27, 10, 0).getTime(), duration_ms: 95 * 60_000, state: "Ready" },
];

function renderShell(ipc: Ipc) {
  return render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell now={() => NOW} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("AppShell", () => {
  it("lists meetings grouped by day with their time and length", async () => {
    renderShell(createMockIpc({ meetings: MEETINGS }));
    const list = await screen.findByRole("list", { name: "Meetings" });
    const headings = within(list).getAllByRole("heading").map((h) => h.textContent);
    expect(headings).toEqual(["Today", "Yesterday", "Earlier"]);
    const today = within(list).getByRole("group", { name: "Today" });
    // Every row button has its trash can beside it.
    const rowsOnly = { name: /^(?!Delete )/ };
    const rows = within(today).getAllByRole("button", rowsOnly);
    expect(rows.map((r) => r.textContent)).toEqual([
      "Design review14:00 · 34 min",
      "Untitled meeting09:30 · 47 min",
    ]);
    const yesterday = within(list).getByRole("group", { name: "Yesterday" });
    expect(within(yesterday).getByRole("button", rowsOnly).textContent).toBe("Supplier call11:05 · 22 min");
    const earlier = within(list).getByRole("group", { name: "Earlier" });
    expect(within(earlier).getByRole("button", rowsOnly).textContent).toContain("Quarterly planning");
    expect(screen.getByRole("button", { name: "New meeting" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Settings" })).toBeInTheDocument();
  });

  it("shows the newest meeting first and switches on selection", async () => {
    const user = userEvent.setup();
    renderShell(createMockIpc({ meetings: MEETINGS }));
    const main = await screen.findByRole("main");
    await waitFor(() =>
      expect(within(main).getByRole("heading", { level: 1 }).textContent).toBe("Design review"),
    );
    const first = screen.getByRole("button", { name: /^Design review/ });
    expect(first).toHaveAttribute("aria-current", "true");

    await user.click(screen.getByRole("button", { name: /^Supplier call/ }));
    expect(within(main).getByRole("heading", { level: 1 }).textContent).toBe("Supplier call");
    expect(screen.getByRole("button", { name: /^Supplier call/ })).toHaveAttribute("aria-current", "true");
    expect(first).not.toHaveAttribute("aria-current");

    await user.click(screen.getByRole("button", { name: /^Untitled meeting/ }));
    expect(within(main).getByRole("heading", { level: 1 }).textContent).toBe("Untitled meeting");
  });

  it("shows the empty state when there are no meetings", async () => {
    renderShell(createMockIpc({ meetings: [] }));
    const main = await screen.findByRole("main");
    expect(
      await within(main).findByRole("heading", { name: "Record your first meeting" }),
    ).toBeInTheDocument();
    expect(within(main).getByRole("button", { name: "New meeting" })).toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "Meetings" })).toBeNull();
    // Copy rules (docs/ui-design.md): no em dashes, no "please", no exclamation marks.
    const text = document.body.textContent ?? "";
    expect(text).not.toMatch(/\u2014|please|!/i);
  });

  it("reports a failed load", async () => {
    const failing: Ipc = {
      ...createMockIpc(),
      listMeetings: () =>
        Promise.reject(new IpcFailure({ kind: "store", message: "store: disk full", retryable: false })),
    };
    renderShell(failing);
    expect(await screen.findByText("Meetings could not be loaded")).toBeInTheDocument();
    expect(screen.getByText("store: disk full")).toBeInTheDocument();
  });

  it("shows a notice when given one", async () => {
    render(
      <EarshotThemeProvider>
        <IpcProvider ipc={createMockIpc({ meetings: MEETINGS })}>
          <AppShell now={() => NOW} notice="Preview: sample data" />
        </IpcProvider>
      </EarshotThemeProvider>,
    );
    expect(await screen.findByText("Preview: sample data")).toBeInTheDocument();
  });
});
