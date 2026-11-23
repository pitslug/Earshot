// The meeting detail as its tests render it (T12.55): inside `AppShell`, with `ITEM` selected,
// and the helpers the detail, Speakers panel, delete and kept-local tests share.

import { act, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { type Ipc, type MeetingListItem, type MockIpcData, createMockIpc, sampleMeetings } from "../ipc";
import { renderApp } from "./app";
import { createManualTimer } from "../time";

export const ITEM: MeetingListItem = { ...sampleMeetings()[1], started_at_ms: 2_000 };
/** An older meeting, so `ITEM` is shown first. */
export const OTHER: MeetingListItem = { ...sampleMeetings()[2], started_at_ms: 1_000 };

/** The detail as the app shows it: inside `AppShell` (which follows job events), with
 * `ITEM` selected. */
export async function setup(data: MockIpcData = {}) {
  const calls: unknown[][] = [];
  const timer = createManualTimer(10_000);
  const ipc: Ipc = createMockIpc({
    timer,
    meetings: [ITEM, OTHER],
    onCall: (c) => calls.push(c),
    ...data,
  });
  const user = userEvent.setup();
  renderApp(ipc, { timer });
  await screen.findByRole("heading", { level: 1, name: ITEM.title });
  return { calls, timer, ipc, user };
}

export const panel = () => screen.getByRole("complementary", { name: "Speakers" });
export const speakerRow = (name: string) => within(panel()).getByRole("listitem", { name });
/** Opens the panel's collapsed list of named voices (T12.19). */
export async function openNamed(user: ReturnType<typeof userEvent.setup>) {
  await user.click(within(panel()).getByRole("button", { name: /^Named/ }));
}
/** The list row of a meeting and its trash can, also while an open modal hides the page from
 * assistive technology. The role query runs inside the meeting list only: over the whole page it
 * took longer than `waitFor`'s interval on the Surface, so a `waitFor` around it starved the very
 * render it waited for (the T12.76 status-dot test failed there every run). */
export const listRow = (item: MeetingListItem) => {
  const list = document.querySelector<HTMLElement>('ul[aria-label="Meetings"]');
  if (list === null) throw new Error("the meeting list is not on the page");
  return within(list).getByRole("button", { name: new RegExp(`^${item.title}`), hidden: true });
};
export const trashName = (item: MeetingListItem) => `Delete ${item.title}`;

/** Longer than tabster's deferred aria-hidden pass (`ModalizerAPI.hiddenUpdate`, 250 ms): a
 * dialog still accessible after this wait survived that pass. */
export const TABSTER_HIDE_PASS_MS = 400;

/** Where focus is and whether the alert dialog is hidden, for a failure message. */
export function dialogState(): string {
  const d = document.querySelector<HTMLElement>('[role="alertdialog"]');
  const f = document.activeElement;
  const focus = f
    ? `<${f.tagName.toLowerCase()} role=${f.getAttribute("role")} label=${f.getAttribute("aria-label")}> "${(f.textContent ?? "").slice(0, 30)}"`
    : "none";
  const hidden = d ? d.closest('[aria-hidden="true"]') : null;
  return `dialog in DOM: ${d !== null}, hidden by aria-hidden: ${hidden === null ? "no" : hidden === d ? "itself" : "an ancestor"}, focus: ${focus}, focus inside dialog: ${d?.contains(f) ?? false}`;
}

/** The alert dialog as assistive technology sees it; on a miss, says why (T7.3d: on the
 * Surface it stayed aria-hidden). */
export async function accessibleDialog(): Promise<HTMLElement> {
  try {
    return await screen.findByRole("alertdialog");
  } catch (e) {
    const now = dialogState();
    await act(() => new Promise((r) => setTimeout(r, 500)));
    throw new Error(`no accessible alertdialog: ${now}; 0.5 s later: ${dialogState()}`, { cause: e });
  }
}
