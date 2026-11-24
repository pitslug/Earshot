// What a long transcript costs (T12.90a): a line's menu (More actions, and Choose on a line with
// no speaker) is a Fluent `Menu` with its own positioning and focus hooks, so a transcript of
// thousands of lines mounts none until one is opened. Menus are counted through Fluent's `Menu`,
// wrapped below, by the label of the button they open from.
import { screen, within } from "@testing-library/react";
import * as fluent from "@fluentui/react-components";
import { isValidElement, type ReactElement } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { type MeetingDetail, createMockIpc, sampleDetail } from "../ipc";
import { ITEM, setup } from "../test/meetingDetail";

vi.mock("@fluentui/react-components", async (original) => {
  const real = await original<typeof import("@fluentui/react-components")>();
  return { ...real, Menu: vi.fn(real.Menu) };
});

const menuSpy = vi.mocked(fluent.Menu);

/** The labels of the line buttons the rendered menus open from, one per `Menu` render. */
function lineMenuRenders(): string[] {
  const out: string[] = [];
  for (const [props] of menuSpy.mock.calls) {
    const children = (Array.isArray(props.children) ? props.children : [props.children]) as unknown[];
    for (const child of children) {
      if (!isValidElement(child)) continue;
      const button = (child as ReactElement<{ children?: unknown }>).props.children;
      if (!isValidElement(button)) continue;
      const label = (button as ReactElement<{ "aria-label"?: string }>).props["aria-label"];
      if (typeof label === "string" && /line \d+$/.test(label)) out.push(label);
    }
  }
  return out;
}

// Enough to show the cost does not follow the line count (each line would mount one or two
// menus before T12.90a); a few thousand rows is too slow for jsdom.
const LINES = 48;

/** The sample meeting with its final transcript repeated to `LINES` lines, the third line of
 * each eight left with no speaker (each such line has a Choose menu too). */
async function longDetail(): Promise<MeetingDetail> {
  const base = await createMockIpc({ meetings: [ITEM] }).getMeeting(ITEM.id);
  const sample = sampleDetail(ITEM).transcript;
  const transcript = Array.from({ length: LINES }, (_, i) => {
    const r = sample[i % sample.length];
    const id = `5e000000-0000-7000-8000-${String(i + 1).padStart(12, "0")}`;
    return i % 8 === 2
      ? { ...r, segment_id: id, start_ms: i * 5_000, speaker_id: null, speaker: "Unknown speaker", unknown_speaker: true }
      : { ...r, segment_id: id, start_ms: i * 5_000 };
  });
  return { ...base, transcript, summary: null };
}

beforeEach(() => {
  menuSpy.mockClear();
});

describe("render cost", () => {
  it("render cost: a long transcript mounts no line menu until one is opened", async () => {
    const long = await longDetail();
    const { user } = await setup({ details: { [ITEM.id]: long } });
    await user.click(await screen.findByRole("tab", { name: "Transcript" }));
    const table = await screen.findByRole("table", { name: "Transcript" });
    expect(within(table).getAllByRole("row")).toHaveLength(LINES);
    // Every line has its buttons, and no line's menu was rendered.
    expect(within(table).getAllByRole("button", { name: /^More actions for line/ })).toHaveLength(LINES);
    expect(within(table).getAllByRole("button", { name: /^Choose who said line/ })).toHaveLength(LINES / 8);
    expect(lineMenuRenders()).toEqual([]);

    // Opening one line's menu mounts that line's alone, open, with the runtime's offers.
    await user.click(within(table).getByRole("button", { name: "More actions for line 5" }));
    await user.click(await screen.findByRole("menuitem", { name: "Move to speaker" }));
    expect(await screen.findByRole("menuitem", { name: "New speaker" })).toBeInTheDocument();
    expect(new Set(lineMenuRenders())).toEqual(new Set(["More actions for line 5"]));

    await user.keyboard("{Escape}{Escape}");
    await user.click(within(table).getByRole("button", { name: "Choose who said line 3" }));
    expect(await screen.findByRole("menuitem", { name: "New speaker" })).toBeInTheDocument();
    expect(new Set(lineMenuRenders())).toEqual(new Set(["More actions for line 5", "Choose who said line 3"]));
  });
});
