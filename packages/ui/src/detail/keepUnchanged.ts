// A detail read again keeps the objects of the one before wherever their contents did not change
// (T14.18): every IPC answer is a new object tree, so without this each edit gave every
// transcript line new props and the memoised lines all rendered again. Pure.

import type { MeetingDetail, MovableSpeaker, TranscriptRow } from "../ipc";

/** Whether two IPC values (JSON: null, booleans, numbers, strings, arrays, plain objects) hold
 * the same contents. */
export function sameValue(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Array.isArray(a)) {
    return Array.isArray(b) && a.length === b.length && a.every((x, i) => sameValue(x, b[i]));
  }
  if (Array.isArray(b)) return false;
  const ka = Object.keys(a);
  const kb = Object.keys(b);
  if (ka.length !== kb.length) return false;
  const ob = b as Record<string, unknown>;
  const oa = a as Record<string, unknown>;
  return ka.every((k) => Object.prototype.hasOwnProperty.call(ob, k) && sameValue(oa[k], ob[k]));
}

/** `next` with each part that holds the same contents as in `prev` replaced by `prev`'s object:
 * transcript rows (by segment id), the word list, the Dictionary and each track's movable
 * speakers. A row whose text, speaker, marks or anything else changed keeps its new object. */
export function keepUnchanged(prev: MeetingDetail | null, next: MeetingDetail): MeetingDetail {
  if (prev === null || prev.id !== next.id) return next;
  const before = new Map(prev.transcript.map((r) => [r.segment_id, r]));
  const rows: TranscriptRow[] = next.transcript.map((r) => {
    const old = before.get(r.segment_id);
    return old !== undefined && sameValue(old, r) ? old : r;
  });
  const transcript =
    rows.length === prev.transcript.length && rows.every((r, i) => r === prev.transcript[i]) ? prev.transcript : rows;
  const movable: Record<string, MovableSpeaker[]> = {};
  for (const [track, list] of Object.entries(next.movable_by_track)) {
    const old = prev.movable_by_track[track];
    movable[track] = old !== undefined && sameValue(old, list) ? old : list;
  }
  const movableKeys = Object.keys(movable);
  const movable_by_track =
    movableKeys.length === Object.keys(prev.movable_by_track).length &&
    movableKeys.every((k) => movable[k] === prev.movable_by_track[k])
      ? prev.movable_by_track
      : movable;
  return {
    ...next,
    transcript,
    movable_by_track,
    word_list: sameValue(prev.word_list, next.word_list) ? prev.word_list : next.word_list,
    dictionary: sameValue(prev.dictionary, next.dictionary) ? prev.dictionary : next.dictionary,
  };
}
