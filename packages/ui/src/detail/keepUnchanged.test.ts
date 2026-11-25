import { describe, expect, it } from "vitest";

import { sampleDetail, sampleMeetings } from "../ipc";
import { keepUnchanged, sameValue } from "./keepUnchanged";

const ITEM = sampleMeetings()[1];
/** The detail as a new IPC answer: the same contents, every object new. */
const read = () => structuredClone(sampleDetail(ITEM));

describe("keepUnchanged", () => {
  it("compares IPC values by contents", () => {
    expect(sameValue({ a: [1, "x", null], b: { c: true } }, { b: { c: true }, a: [1, "x", null] })).toBe(true);
    expect(sameValue({ a: [1] }, { a: [2] })).toBe(false);
    expect(sameValue({ a: 1 }, { a: 1, b: undefined })).toBe(false);
    expect(sameValue([1], { 0: 1 })).toBe(false);
    expect(sameValue(null, {})).toBe(false);
  });

  it("keeps every unchanged object of the read before", () => {
    const prev = read();
    const next = keepUnchanged(prev, read());
    expect(next.transcript).toBe(prev.transcript);
    expect(next.word_list).toBe(prev.word_list);
    expect(next.dictionary).toBe(prev.dictionary);
    expect(next.movable_by_track).toBe(prev.movable_by_track);
  });

  it("gives a changed row its new object and keeps the others", () => {
    const prev = read();
    const fresh = read();
    const edited = fresh.transcript[2];
    edited.text = "An invented line about the hedge.";
    const next = keepUnchanged(prev, fresh);
    expect(next.transcript).not.toBe(prev.transcript);
    expect(next.transcript[2]).toBe(edited);
    next.transcript.forEach((r, i) => {
      if (i !== 2) expect(r).toBe(prev.transcript[i]);
    });
  });

  it("gives a row whose speaker, marks or splitting changed its new object", () => {
    const prev = read();
    for (const change of [
      (d: ReturnType<typeof read>) => (d.transcript[0].speaker = "Someone else"),
      (d: ReturnType<typeof read>) => (d.transcript[0].unknown_speaker = !d.transcript[0].unknown_speaker),
      (d: ReturnType<typeof read>) =>
        (d.transcript[0].suggestions =
          d.transcript[0].suggestions.length > 0 ? [] : [{ token: 0, word: "lead", suggested: "Lede" } as never]),
      (d: ReturnType<typeof read>) => (d.transcript[0].edited = !d.transcript[0].edited),
      (d: ReturnType<typeof read>) => (d.transcript[0].splittable = !d.transcript[0].splittable),
    ]) {
      const fresh = read();
      change(fresh);
      const next = keepUnchanged(prev, fresh);
      expect(next.transcript[0]).toBe(fresh.transcript[0]);
      expect(next.transcript[1]).toBe(prev.transcript[1]);
    }
  });

  it("gives a changed track's movable list its new object", () => {
    const movable = {
      mic: [{ id: "s-1", name: "Speaker 1" }],
      far: [{ id: "s-2", name: "Speaker 2" }],
    };
    const prev = { ...read(), movable_by_track: structuredClone(movable) };
    const same = keepUnchanged(prev, { ...read(), movable_by_track: structuredClone(movable) });
    expect(same.movable_by_track).toBe(prev.movable_by_track);
    const fresh = { ...read(), movable_by_track: structuredClone(movable) };
    fresh.movable_by_track.mic = [...fresh.movable_by_track.mic, { id: "s-new", name: "Speaker 9" }];
    const next = keepUnchanged(prev, fresh);
    expect(next.movable_by_track).not.toBe(prev.movable_by_track);
    expect(next.movable_by_track.mic).toBe(fresh.movable_by_track.mic);
    expect(next.movable_by_track.far).toBe(prev.movable_by_track.far);
  });

  it("follows a split: the new line is new and the lines after it keep theirs", () => {
    const prev = read();
    const fresh = read();
    const added = { ...fresh.transcript[1], segment_id: "split-part", text: "the rest" };
    fresh.transcript.splice(2, 0, added);
    const next = keepUnchanged(prev, fresh);
    expect(next.transcript[2]).toBe(added);
    expect(next.transcript[3]).toBe(prev.transcript[2]);
  });

  it("keeps nothing of another meeting", () => {
    const prev = read();
    const other = structuredClone(sampleDetail(sampleMeetings()[2]));
    expect(keepUnchanged(prev, other)).toBe(other);
  });
});
