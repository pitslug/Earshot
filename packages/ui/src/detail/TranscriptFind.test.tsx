// Find in the transcript (T12.98), through the app: the button and Ctrl+F open the bar, Esc and
// close clear and hide it, the count, previous and next, and what the lines show.
import { act, screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { sampleDetail } from "../ipc/mockDetail";
import { ITEM, setup } from "../test/meetingDetail";
import type { ManualTimer } from "../time";
import { FIND_PAUSE_MS, FIND_TOO_SHORT } from "./findMatches";

const table = () => screen.getByRole("table", { name: "Transcript" });
const rows = () => within(table()).getAllByRole("row");
const bar = () => screen.getByRole("search", { name: "Find in transcript" });
const findBox = () => within(bar()).getByRole("textbox", { name: "Text to find" });
const count = () => within(bar()).getByRole("status").textContent;
/** The text of each match: a match may be drawn in parts (a word's element, the space after
 * it), each part carrying its match's index on its line. */
function marks(): string[] {
  const out: string[] = [];
  let last: Element | null = null;
  for (const m of table().querySelectorAll("mark")) {
    const key = `${rowIndex(m)}:${m.getAttribute("data-find-index")}`;
    if (last !== null && `${rowIndex(last)}:${last.getAttribute("data-find-index")}` === key) {
      out[out.length - 1] += m.textContent;
    } else out.push(m.textContent ?? "");
    last = m;
  }
  return out;
}
/** The parts of the current match. */
const currentMarks = () => [...table().querySelectorAll('mark[data-find-match="current"]')];
const rowIndex = (el: Element) => rows().indexOf(el.closest('[role="row"]') as HTMLElement);

/** Types `text` into the find box and lets the typing pause pass, so find has looked. */
async function typeFind(user: UserEvent, timer: ManualTimer, text: string) {
  await user.type(findBox(), text);
  await act(async () => timer.advance(FIND_PAUSE_MS));
}

/** The detail on its Transcript tab with the bar open. */
async function openFind(data = {}) {
  const s = await setup(data);
  await s.user.click(await screen.findByRole("tab", { name: "Transcript" }));
  await s.user.click(screen.getByRole("button", { name: "Find in transcript" }));
  return s;
}

// jsdom has no scrolling; the scrolling tests install a recorder and this puts back what was
// there.
const scrollIntoViewBefore = Object.getOwnPropertyDescriptor(Element.prototype, "scrollIntoView");
afterEach(() => {
  if (scrollIntoViewBefore === undefined) delete (Element.prototype as Partial<Element>).scrollIntoView;
  else Object.defineProperty(Element.prototype, "scrollIntoView", scrollIntoViewBefore);
});

/** Records every element asked to scroll into the middle of the pane. */
function recordCentreScrolls(): Element[] {
  const scrolled: Element[] = [];
  Element.prototype.scrollIntoView = vi.fn(function (this: Element, arg?: boolean | ScrollIntoViewOptions) {
    if (typeof arg === "object" && arg.block === "center") scrolled.push(this);
  });
  return scrolled;
}

describe("find in the transcript", () => {
  it("opens from the search button with the text box focused", async () => {
    const { user } = await setup();
    await user.click(await screen.findByRole("tab", { name: "Transcript" }));
    const button = screen.getByRole("button", { name: "Find in transcript" });
    expect(button).toHaveAttribute("aria-expanded", "false");
    // The shortcut is named only in the tooltip, the button's description.
    expect(button).toHaveAccessibleDescription("Find in transcript (Ctrl+F)");
    await user.click(button);
    expect(findBox()).toHaveFocus();
    expect(button).toHaveAttribute("aria-expanded", "true");
    expect(count()).toBe("Pick a speaker or type");
    expect(bar().textContent).not.toContain("Ctrl");
    expect(marks()).toEqual([]);
  });

  it("opens with Ctrl+F, from the Summary tab too, and keeps the webview's own find closed", async () => {
    const { user, timer } = await setup();
    expect(await screen.findByRole("tab", { name: "Summary" })).toHaveAttribute("aria-selected", "true");
    const press = new KeyboardEvent("keydown", { key: "f", ctrlKey: true, bubbles: true, cancelable: true });
    act(() => {
      document.body.dispatchEvent(press);
    });
    expect(press.defaultPrevented).toBe(true);
    expect(screen.getByRole("tab", { name: "Transcript" })).toHaveAttribute("aria-selected", "true");
    expect(findBox()).toHaveFocus();
    // Again in the box: still the bar, its text selected.
    await typeFind(user, timer, "seals");
    await user.keyboard("{Control>}f{/Control}");
    expect(findBox()).toHaveFocus();
    const box = findBox() as HTMLInputElement;
    expect([box.selectionStart, box.selectionEnd]).toEqual([0, 5]);
  });

  it("Ctrl+F in a line being edited opens nothing, the webview's own find included", async () => {
    const { user } = await setup();
    await user.click(await screen.findByRole("tab", { name: "Transcript" }));
    await user.click(screen.getByRole("button", { name: "Edit line 1" }));
    const editor = screen.getByRole("textbox", { name: "Line 1" });
    editor.focus();
    const press = new KeyboardEvent("keydown", { key: "f", ctrlKey: true, bubbles: true, cancelable: true });
    act(() => {
      editor.dispatchEvent(press);
    });
    // The webview's find stays closed, and so does the bar: the line keeps its focus.
    expect(press.defaultPrevented).toBe(true);
    expect(screen.queryByRole("search", { name: "Find in transcript" })).not.toBeInTheDocument();
    expect(editor).toHaveFocus();
  });

  it("counts text matches, case ignored, and steps with Enter, Shift+Enter and the buttons, wrapping", async () => {
    const { user, timer } = await openFind();
    await typeFind(user, timer, "AIR FREIGHT");
    expect(count()).toBe("Match 1 of 2");
    expect(marks()).toEqual(["air freight", "air freight"]);
    expect(currentMarks().map((m) => m.textContent).join("")).toBe("air freight");
    expect(rowIndex(currentMarks()[0])).toBe(2);
    await user.keyboard("{Enter}");
    expect(count()).toBe("Match 2 of 2");
    expect(rowIndex(currentMarks()[0])).toBe(6);
    await user.keyboard("{Enter}");
    expect(count()).toBe("Match 1 of 2");
    await user.keyboard("{Shift>}{Enter}{/Shift}");
    expect(count()).toBe("Match 2 of 2");
    await user.click(within(bar()).getByRole("button", { name: "Next match" }));
    expect(count()).toBe("Match 1 of 2");
    await user.click(within(bar()).getByRole("button", { name: "Previous match" }));
    expect(count()).toBe("Match 2 of 2");
  });

  it("looks once typing pauses, and at once when the box is cleared", async () => {
    const { user, timer } = await openFind();
    await user.type(findBox(), "air freight");
    // Still typing: nothing found or marked yet, and no hint to type.
    expect(count()).toBe("");
    expect(marks()).toEqual([]);
    await act(async () => timer.advance(FIND_PAUSE_MS - 1));
    expect(marks()).toEqual([]);
    await act(async () => timer.advance(1));
    expect(count()).toBe("Match 1 of 2");
    expect(marks()).toEqual(["air freight", "air freight"]);
    // Another keystroke starts the pause again; the last look stays meanwhile.
    await user.type(findBox(), "x");
    expect(count()).toBe("Match 1 of 2");
    await act(async () => timer.advance(FIND_PAUSE_MS));
    expect(count()).toBe("No matches");
    // Clearing the box clears the marks without waiting.
    await typeFind(user, timer, "{Backspace}");
    expect(count()).toBe("Match 1 of 2");
    await user.clear(findBox());
    expect(count()).toBe("Pick a speaker or type");
    expect(marks()).toEqual([]);
  });

  it("Enter before the pause is over looks at once, at the first match", async () => {
    const { user } = await openFind();
    await user.type(findBox(), "air freight{Enter}");
    expect(count()).toBe("Match 1 of 2");
    expect(rowIndex(currentMarks()[0])).toBe(2);
    await user.keyboard("{Enter}");
    expect(count()).toBe("Match 2 of 2");
  });

  it("needs two letters of text, and none with a speaker chosen", async () => {
    const { user, timer } = await openFind();
    await user.type(findBox(), "a");
    expect(count()).toBe("");
    await act(async () => timer.advance(FIND_PAUSE_MS));
    expect(count()).toBe(FIND_TOO_SHORT);
    expect(marks()).toEqual([]);
    await typeFind(user, timer, "i");
    expect(count()).not.toBe(FIND_TOO_SHORT);
    expect(marks().length).toBeGreaterThan(0);
    // One character that is not a Latin letter (here a comma) is a real search.
    await user.clear(findBox());
    await typeFind(user, timer, ",");
    expect(count()).toMatch(/^Match 1 of \d+$/);
  });

  it("closing the bar during the pause looks for nothing afterwards", async () => {
    const { user, timer } = await openFind();
    await user.type(findBox(), "seals");
    await user.click(within(bar()).getByRole("button", { name: "Close find" }));
    await act(async () => timer.advance(FIND_PAUSE_MS));
    await user.click(screen.getByRole("button", { name: "Find in transcript" }));
    expect(count()).toBe("Pick a speaker or type");
    expect(marks()).toEqual([]);
  });

  it("scrolls the line it points at into the middle of the pane", async () => {
    const scrolled = recordCentreScrolls();
    const { user, timer } = await openFind();
    await typeFind(user, timer, "air freight");
    expect(scrolled.at(-1)).toBe(rows()[2]);
    await user.keyboard("{Enter}");
    expect(scrolled.at(-1)).toBe(rows()[6]);
  });

  it("does not scroll again when the meeting is read again after an edit", async () => {
    const scrolled = recordCentreScrolls();
    const { user, timer } = await openFind();
    await typeFind(user, timer, "air freight");
    const before = scrolled.length;
    expect(before).toBeGreaterThan(0);
    // Another line is edited with the bar open: the detail is read again.
    await user.click(screen.getByRole("button", { name: "Edit line 1" }));
    const editor = screen.getByRole("textbox", { name: "Line 1" });
    await user.clear(editor);
    await user.type(editor, "An invented line about the hedge.");
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(rows()[0].textContent).toContain("An invented line about the hedge."));
    expect(count()).toBe("Match 1 of 2");
    expect(scrolled).toHaveLength(before);
  });

  it("says no matches and turns previous and next off", async () => {
    const { user, timer } = await openFind();
    await typeFind(user, timer, "zebra");
    expect(count()).toBe("No matches");
    expect(within(bar()).getByRole("button", { name: "Next match" })).toBeDisabled();
    expect(within(bar()).getByRole("button", { name: "Previous match" })).toBeDisabled();
    expect(marks()).toEqual([]);
  });

  it("offers each speaker with lines and steps through a speaker's turns", async () => {
    // A speaker alone needs no typing, so no pause: the turns show at once.
    const { user } = await openFind();
    const shown = sampleDetail(ITEM).transcript;
    const names = [...new Set(rows().map((r) => within(r).getAllByRole("cell")[1].textContent ?? ""))];
    expect(names.length).toBeGreaterThan(1);
    await user.click(within(bar()).getByRole("combobox", { name: /^Speaker/ }));
    const options = screen.getAllByRole("option").map((o) => o.textContent);
    expect(options).toEqual(["Any speaker", ...names]);
    // The first line's speaker: their turns are the lines where they start talking.
    await user.click(screen.getByRole("option", { name: names[0] }));
    const theirs = rows().map((r) => within(r).getAllByRole("cell")[1].textContent === names[0]);
    const turns = theirs.flatMap((t, i) => (t && (i === 0 || !theirs[i - 1]) ? [i] : []));
    expect(turns.length).toBeGreaterThan(1);
    expect(count()).toBe(`Turn 1 of ${turns.length}`);
    expect(rows().filter((r) => r.hasAttribute("data-find-speaker"))).toHaveLength(theirs.filter(Boolean).length);
    const current = () => rows().findIndex((r) => r.getAttribute("data-find-turn") === "current");
    expect(current()).toBe(turns[0]);
    await user.click(within(bar()).getByRole("button", { name: "Next match" }));
    expect(current()).toBe(turns[1]);
    expect(shown.length).toBe(rows().length);
  });

  it("finds text only in the chosen speaker's lines", async () => {
    const { user, timer } = await openFind();
    await typeFind(user, timer, "seals");
    expect(count()).toBe("Match 1 of 2");
    const speakerOf = (i: number) => within(rows()[i]).getAllByRole("cell")[1].textContent ?? "";
    const owner = speakerOf(rowIndex(currentMarks()[0]));
    await user.click(within(bar()).getByRole("combobox", { name: /^Speaker/ }));
    await user.click(screen.getByRole("option", { name: owner }));
    expect(count()).toBe("Match 1 of 1");
    expect(marks()).toEqual(["seals"]);
    expect(speakerOf(rowIndex(currentMarks()[0]))).toBe(owner);
  });

  it("marks a match inside a suggested word's mark, leaving the mark a button", async () => {
    const { user, timer } = await openFind();
    await typeFind(user, timer, "ree");
    const mark = screen.getByRole("button", { name: "Suggestion for Reed" });
    expect(within(mark).getByText("Ree").tagName).toBe("MARK");
    expect(mark.textContent).toBe("Reed");
  });

  it("shows no highlights on a line being edited", async () => {
    const { user, timer } = await openFind();
    await typeFind(user, timer, "air freight");
    expect(marks()).toHaveLength(2);
    await user.click(screen.getByRole("button", { name: "Edit line 3" }));
    expect(marks()).toEqual(["air freight"]);
    expect(rowIndex(table().querySelector("mark") as Element)).toBe(6);
  });

  it("finds in a draft transcript", async () => {
    const base = sampleDetail(ITEM);
    const draft = { ...base, header: { ...base.header, transcript: "Draft" as const } };
    const { user, timer } = await openFind({ details: { [ITEM.id]: draft } });
    await typeFind(user, timer, "seals");
    expect(count()).toBe("Match 1 of 2");
    expect(marks()).toEqual(["seals", "seals"]);
  });

  it("Esc clears both fields, hides the bar and returns focus to the search button", async () => {
    const { user, timer } = await openFind();
    await typeFind(user, timer, "seals");
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("search", { name: "Find in transcript" })).not.toBeInTheDocument();
    expect(marks()).toEqual([]);
    const button = screen.getByRole("button", { name: "Find in transcript" });
    expect(button).toHaveFocus();
    await user.click(button);
    expect(findBox()).toHaveValue("");
    expect(count()).toBe("Pick a speaker or type");
  });

  it("Esc from elsewhere in the detail closes and clears the bar too", async () => {
    const { user, timer } = await openFind();
    await typeFind(user, timer, "seals");
    screen.getByRole("tab", { name: "Transcript" }).focus();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("search", { name: "Find in transcript" })).not.toBeInTheDocument();
    expect(marks()).toEqual([]);
    expect(screen.getByRole("button", { name: "Find in transcript" })).toHaveFocus();
  });

  it("Esc in a line being edited or on an open speaker list leaves the bar open", async () => {
    const { user, timer } = await openFind();
    await typeFind(user, timer, "seals");
    await user.click(within(bar()).getByRole("combobox", { name: /^Speaker/ }));
    expect(screen.getAllByRole("option").length).toBeGreaterThan(1);
    await user.keyboard("{Escape}");
    expect(screen.queryAllByRole("option")).toHaveLength(0);
    expect(bar()).toBeInTheDocument();
    expect(findBox()).toHaveValue("seals");
    await user.click(screen.getByRole("button", { name: "Edit line 1" }));
    await user.click(screen.getByRole("textbox", { name: "Line 1" }));
    await user.keyboard("{Escape}");
    expect(findBox()).toHaveValue("seals");
  });

  it("names the speaker choice with its chosen speaker, and the button closes an open bar", async () => {
    const { user, timer } = await openFind();
    // Fluent's pattern: named by the hidden label and by itself, so a screen reader reads the
    // chosen speaker with the name (jsdom's name computation reads only the label).
    const choice = within(bar()).getByRole("combobox", { name: /^Speaker/ });
    const [label, self] = (choice.getAttribute("aria-labelledby") ?? "").split(" ");
    expect(document.getElementById(label)?.textContent).toBe("Speaker");
    expect(self).toBe(choice.id);
    expect(choice.textContent).toBe("Any speaker");
    await user.click(choice);
    const name = screen.getAllByRole("option")[1].textContent ?? "";
    await user.click(screen.getByRole("option", { name }));
    expect(choice.textContent).toBe(name);
    await typeFind(user, timer, "seals");
    const button = screen.getByRole("button", { name: "Find in transcript" });
    expect(button).toHaveAttribute("aria-expanded", "true");
    await user.click(button);
    expect(screen.queryByRole("search", { name: "Find in transcript" })).not.toBeInTheDocument();
    expect(button).toHaveAttribute("aria-expanded", "false");
    expect(button).toHaveFocus();
    await user.click(button);
    expect(findBox()).toHaveValue("");
    expect(within(bar()).getByRole("combobox", { name: /^Speaker/ }).textContent).toBe("Any speaker");
  });

  it("close clears and hides the bar too, and leaving the tab closes it", async () => {
    const { user, timer } = await openFind();
    await typeFind(user, timer, "seals");
    await user.click(within(bar()).getByRole("button", { name: "Close find" }));
    expect(screen.queryByRole("search", { name: "Find in transcript" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Find in transcript" })).toHaveFocus();
    await user.click(screen.getByRole("button", { name: "Find in transcript" }));
    await typeFind(user, timer, "seals");
    await user.click(screen.getByRole("tab", { name: "Summary" }));
    expect(screen.queryByRole("search", { name: "Find in transcript" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Transcript" }));
    expect(marks()).toEqual([]);
  });
});
