// The mock's suggested words (T12.15e) follow `word_choices` and `edit::use_word`,
// `put_back_word` and `keep_word`. Every word is invented.

import { describe, expect, it } from "vitest";

import { IpcFailure, type MeetingDetail, type PersonItem } from "./ipc";
import { createMockIpc } from "./mock";
import { splitPunctuation, tokensOf, wordListOf } from "./mockWords";

const READY = {
  id: "m1",
  title: "Pumps",
  started_at_ms: 10,
  duration_ms: 60_000,
  state: "Ready" as const,
  folder_id: null,
};
const DRAFT = { ...READY, id: "m2", state: "Processing" as const };
const LINE8 = "5e000000-0000-7000-8000-000000000008";

async function kind(p: Promise<unknown>): Promise<string> {
  try {
    await p;
  } catch (e) {
    expect(e).toBeInstanceOf(IpcFailure);
    return (e as IpcFailure).error.kind;
  }
  throw new Error("expected a refusal");
}

const row = (d: MeetingDetail) => d.transcript.find((r) => r.segment_id === LINE8)!;

describe("mock suggested words (T12.15e)", () => {
  it("builds the list as word_choices::list_of does", () => {
    const person = (id: string, name: string, is_me = false): PersonItem => ({
      id,
      name,
      has_voiceprint: false,
      is_me,
    });
    expect(
      wordListOf(
        ["Zorbek", " Quillan  Marsh ", "vantrel"],
        [person("3", "Vantrel Oskarine"), person("1", "Me", true), person("2", "Brevik Tamsin")],
      ),
    ).toEqual(["Zorbek", "Quillan", "Marsh", "vantrel", "Brevik", "Tamsin", "Oskarine"]);
    // A user who set a name is listed like anyone else.
    expect(wordListOf([], [person("1", "Nilo Ardent", true)])).toEqual(["Nilo", "Ardent"]);
    expect(tokensOf(" a b  c ")).toEqual(["a", "b", "c"]);
    expect(splitPunctuation("(sorbeck's),")).toEqual(["(", "sorbeck's", "),"]);
    expect(splitPunctuation("--")).toEqual(["--", "", ""]);
  });

  it("serves the list and the sample suggestion on a final transcript only", async () => {
    const ipc = createMockIpc({ meetings: [READY, DRAFT], summaries: { vocabulary: ["Zorbek"] } });
    const d = await ipc.getMeeting("m1");
    expect(d.word_list).toEqual(["Zorbek", "Casey", "Priya", "Shah", "Tom", "Reid"]);
    expect(row(d).suggestions).toEqual([{ token: 8, decoded: "Reed", listed: "Reid" }]);
    expect(d.transcript.filter((r) => r.suggestions.length > 0)).toHaveLength(1);
    const draft = await ipc.getMeeting("m2");
    expect(draft.word_list).toEqual([]);
    expect(draft.transcript.every((r) => r.suggestions.length === 0 && r.changed_words.length === 0)).toBe(true);
    expect(await kind(ipc.useWord("m2", LINE8, 8, "Reid"))).toBe("draft_transcript");
    expect(await kind(ipc.keepWord("m2", LINE8, 8))).toBe("draft_transcript");
  });

  it("uses a word, another word, then puts the decoded word back", async () => {
    const ipc = createMockIpc({ meetings: [READY], summaries: { vocabulary: ["Reidar"] } });
    const before = row(await ipc.getMeeting("m1"));
    await ipc.useWord("m1", LINE8, 8, " reid ");
    let d = await ipc.getMeeting("m1");
    expect(row(d).text).toBe("Fine by us. I'll send it to Tom Reid in writing.");
    expect(row(d).edited).toBe(true);
    expect(row(d).suggestions).toEqual([]);
    expect(row(d).changed_words).toEqual([{ token: 8, decoded: "Reed", written: "Reid" }]);
    expect(d.summary_stale).toBe(true);
    // Another word from the list keeps the decoded word.
    await ipc.useWord("m1", LINE8, 8, "Reidar");
    d = await ipc.getMeeting("m1");
    expect(row(d).changed_words).toEqual([{ token: 8, decoded: "Reed", written: "Reidar" }]);
    await ipc.putBackWord("m1", LINE8, 8);
    d = await ipc.getMeeting("m1");
    expect(row(d).text).toBe(before.text);
    expect(row(d).changed_words).toEqual([]);
    expect(row(d).suggestions).toEqual(before.suggestions);
    expect(await kind(ipc.putBackWord("m1", LINE8, 8))).toBe("no_word_choice");
  });

  it("refuses an unlisted word, a missing word and a missing line", async () => {
    const ipc = createMockIpc({ meetings: [READY] });
    expect(await kind(ipc.useWord("m1", LINE8, 8, "Glimmerax"))).toBe("word_not_listed");
    expect(await kind(ipc.useWord("m1", LINE8, 99, "Reid"))).toBe("word_not_found");
    expect(await kind(ipc.keepWord("m1", LINE8, 99))).toBe("word_not_found");
    expect(await kind(ipc.useWord("m1", "5e000000-0000-7000-8000-999999999999", 0, "Reid"))).toBe(
      "segment_not_found",
    );
    expect(row(await ipc.getMeeting("m1")).text).toBe("Fine by us. I'll send it to Tom Reed in writing.");
  });

  it("keeps a word without changing the line, and a hand edit drops marks that moved", async () => {
    const ipc = createMockIpc({ meetings: [READY] });
    await ipc.keepWord("m1", LINE8, 8);
    let d = await ipc.getMeeting("m1");
    expect(row(d).suggestions).toEqual([]);
    expect(row(d).edited).toBe(false);
    expect(d.summary_stale).toBe(false);

    const fresh = createMockIpc({ meetings: [READY] });
    await fresh.useWord("m1", LINE8, 8, "Reid");
    await fresh.editSegment("m1", LINE8, "Fine by us. I'll send it to Tom Reid in writing today.");
    expect(row(await fresh.getMeeting("m1")).changed_words).toHaveLength(1);
    await fresh.editSegment("m1", LINE8, "Fine. I'll send it to Tom Reid in writing.");
    d = await fresh.getMeeting("m1");
    expect(row(d).changed_words).toEqual([]);
  });
});
