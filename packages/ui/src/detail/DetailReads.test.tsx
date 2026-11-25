// What the meeting detail reads and when (T14.17): an answer older than the newest read is
// dropped, and the other computers are asked only when the Re-summarise menu opens.
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { type Ipc, type MeetingDetail, createMockIpc, sampleDetail } from "../ipc";
import { IpcProvider } from "../ipc/context";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { renderApp } from "../test/app";
import { ITEM, OTHER } from "../test/meetingDetail";
import { MeetingDetailView } from "./MeetingDetailView";

/** A detail whose header says `minutes` long. */
function lasting(minutes: number): MeetingDetail {
  const d = sampleDetail(ITEM);
  return { ...d, header: { ...d.header, duration_ms: minutes * 60_000 } };
}

describe("meeting detail reads", () => {
  it("drops an answer older than the newest read", async () => {
    const base = createMockIpc({ timer: createManualTimer(10_000), meetings: [ITEM, OTHER] });
    // Every read after the first waits until the test answers it.
    const pending: ((d: MeetingDetail) => void)[] = [];
    let first = true;
    const ipc: Ipc = {
      ...base,
      getMeeting: (id) => {
        if (first) {
          first = false;
          return Promise.resolve(lasting(5));
        }
        void id;
        return new Promise((resolve) => pending.push(resolve));
      },
    };
    const show = (refresh: number) => (
      <EarshotThemeProvider>
        <IpcProvider ipc={ipc}>
          <MeetingDetailView
            item={ITEM}
            refresh={refresh}
            onChanged={() => {}}
            onJobStarted={() => {}}
            onJobDismissed={() => {}}
            onJobCancel={() => {}}
          />
        </IpcProvider>
      </EarshotThemeProvider>
    );
    const view = render(show(0));
    expect(await screen.findByText(/· 5 min ·/)).toBeInTheDocument();
    // Two reads in flight; the newer answers first, then the older.
    view.rerender(show(1));
    view.rerender(show(2));
    await waitFor(() => expect(pending).toHaveLength(2));
    await act(async () => pending[1](lasting(9)));
    expect(await screen.findByText(/· 9 min ·/)).toBeInTheDocument();
    await act(async () => pending[0](lasting(3)));
    // The older answer changes nothing.
    expect(screen.getByText(/· 9 min ·/)).toBeInTheDocument();
    expect(screen.queryByText(/· 3 min ·/)).toBeNull();
  });

  it("asks for the other computers only when the Re-summarise menu opens", async () => {
    // Counted at the page's own command (the mock also reads the computers inside other
    // commands, which Rust does not).
    const timer = createManualTimer(10_000);
    const base = createMockIpc({ timer, machine: "desktop", meetings: [ITEM, OTHER] });
    let asked = 0;
    const ipc: Ipc = {
      ...base,
      summaryHelpers: () => {
        asked += 1;
        return base.summaryHelpers();
      },
    };
    renderApp(ipc, { timer });
    await screen.findByText(/TL;DR/);
    // The page's effects have run (it shows the summary): an ask on mount would be counted now.
    await act(async () => {});
    expect(asked).toBe(0);
    await userEvent.setup().click(screen.getByRole("button", { name: /: choose who writes it$/ }));
    await waitFor(() => expect(asked).toBe(1));
  });
});
