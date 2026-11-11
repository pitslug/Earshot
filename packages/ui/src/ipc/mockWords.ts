// Suggestions beside words in the mock IPC (T12.15e): the word list and the token rules of
// `earshot_runtime::word_choices` and `edit::use_word`, so a rule change edits both. The
// sound matcher itself is not mirrored: the sample's suggestions are fixed data.

import type { MeetingDetail, PersonItem } from "./ipc";

/** The white-space tokens of `text` (Rust `split_whitespace`: Unicode White_Space). */
export function tokensOf(text: string): string[] {
  return text.split(/\p{White_Space}+/u).filter((t) => t !== "");
}

const WORD_CHAR = /[\p{Alphabetic}\p{N}']/u;

/** `token` split into leading punctuation, its core (first to last letter, digit or
 * apostrophe) and trailing punctuation, as core `vocabulary::suggest::split_punctuation`. */
export function splitPunctuation(token: string): [string, string, string] {
  const chars = [...token];
  const first = chars.findIndex((c) => WORD_CHAR.test(c));
  if (first < 0) return [token, "", ""];
  let last = chars.length - 1;
  while (!WORD_CHAR.test(chars[last])) last -= 1;
  return [chars.slice(0, first).join(""), chars.slice(first, last + 1).join(""), chars.slice(last + 1).join("")];
}

/** `word_choices::list_of`: the Dictionary's words, then the words of saved people's names
 * (by name, the default "Me" left out), each dropped when it repeats an earlier word in
 * another case. */
export function wordListOf(dictionary: string[], persons: PersonItem[]): string[] {
  const people = persons
    .filter((p) => !(p.is_me && p.name === "Me"))
    .sort((a, b) => {
      const x = a.name.toLowerCase();
      const y = b.name.toLowerCase();
      if (x !== y) return x < y ? -1 : 1;
      return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
    });
  const seen = new Set<string>();
  const out: string[] = [];
  for (const w of [...dictionary.flatMap(tokensOf), ...people.flatMap((p) => tokensOf(p.name))]) {
    const key = w.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(w);
  }
  return out;
}

/** Line `segment` with token `token` replaced by `word`, its punctuation kept, and the
 * line's marks: a changed word for the token unless `word` is the decoded word again. */
export function withWord(
  detail: MeetingDetail,
  segment: string,
  token: number,
  word: string,
  decoded: string,
): MeetingDetail {
  return {
    ...detail,
    transcript: detail.transcript.map((r) => {
      if (r.segment_id !== segment) return r;
      const tokens = tokensOf(r.text);
      const [pre, , post] = splitPunctuation(tokens[token]);
      const next = tokens.map((t, i) => (i === token ? `${pre}${word}${post}` : t)).join(" ");
      const changed = r.changed_words.filter((c) => c.token !== token);
      if (word !== decoded) changed.push({ token, decoded, written: word });
      changed.sort((a, b) => a.token - b.token);
      return {
        ...r,
        text: next,
        edited: r.edited || next !== r.text,
        suggestions: r.suggestions.filter((s) => s.token !== token),
        changed_words: changed,
      };
    }),
  };
}

/** After a hand edit of a line: only marks whose token still reads as recorded stay. */
export function marksAfterEdit(detail: MeetingDetail, segment: string): MeetingDetail {
  return {
    ...detail,
    transcript: detail.transcript.map((r) => {
      if (r.segment_id !== segment) return r;
      const tokens = tokensOf(r.text);
      const core = (i: number) => (tokens[i] === undefined ? undefined : splitPunctuation(tokens[i])[1]);
      return {
        ...r,
        suggestions: r.suggestions.filter((s) => core(s.token) === s.decoded),
        changed_words: r.changed_words.filter((c) => core(c.token) === c.written),
      };
    }),
  };
}
