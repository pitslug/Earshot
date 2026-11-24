// Suggestions beside words in the mock IPC (T12.15e): the word list and the token rules of
// `earshot_runtime::word_choices` and `edit::use_word`, so a rule change edits both. The
// sound matcher itself is not mirrored: the sample's suggestions are fixed data.

import { dictionaryWord, splitToken, tokenPieces, tokensOf, withoutQuotesAndPunctuation } from "../detail/dictionaryWord";
import type { MeetingDetail, PersonItem } from "./ipc";

/** Rust's `to_ascii_lowercase`: only A-Z fold. */
function asciiLower(s: string): string {
  return s.replace(/[A-Z]/g, (c) => c.toLowerCase());
}

/** `word_choices::named_persons`: the saved people whose names count, by name with ASCII case
 * ignored (as `list_persons` orders them), the default "Me" left out. */
function namedPersons(persons: PersonItem[]): PersonItem[] {
  return persons
    .filter((p) => !(p.is_me && p.name === "Me"))
    .sort((a, b) => {
      const x = asciiLower(a.name);
      const y = asciiLower(b.name);
      if (x !== y) return x < y ? -1 : 1;
      return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
    });
}

/** `name` on one line (white space runs joined), or `null` when it is empty or holds a control
 * or invisible formatting character or more than 100 characters (`prompt::people::usable`). */
function usableName(name: string): string | null {
  if (/[\p{Cc}\p{Cf}]/u.test(name)) return null;
  const joined = name.split(/\p{White_Space}+/u).filter((w) => w !== "").join(" ");
  return joined === "" || [...joined].length > 100 ? null : joined;
}

/** `word_choices::people_left_out` (T12.91a, `prompt::People::left_out`): how many names of
 * `namedPersons` `peopleOf` leaves out for what they hold, never for being empty or a repeat. */
export function peopleLeftOut(persons: PersonItem[]): number {
  return namedPersons(persons).filter(
    (p) => p.name.split(/\p{White_Space}+/u).some((w) => w !== "") && usableName(p.name) === null,
  ).length;
}

/** `word_choices::people_of` (T12.91, `prompt::People::new`): the whole names of
 * `namedPersons`, white space joined, names that are empty, longer than 100 characters or hold
 * a control or invisible formatting character left out, and a name that repeats an earlier one
 * or a Dictionary entry in another case left out. */
export function peopleOf(dictionary: string[], persons: PersonItem[]): string[] {
  const seen = new Set(dictionary.map((w) => w.toLowerCase()));
  const out: string[] = [];
  for (const p of namedPersons(persons)) {
    const name = usableName(p.name);
    if (name === null) continue;
    const key = name.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(name);
  }
  return out;
}

/** `word_choices::list_of`: the Dictionary's words, then the words of saved people's names
 * (`namedPersons`), each dropped when it repeats an earlier word in another case. */
export function wordListOf(dictionary: string[], persons: PersonItem[]): string[] {
  const people = namedPersons(persons);
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
      // The token is replaced in place, the line's own white space kept (Rust `token_ranges`).
      const next = tokenPieces(r.text)
        .map(({ piece, token: at }) => {
          if (at !== token) return piece;
          const [pre, , post] = splitToken(piece);
          return `${pre}${word}${post}`;
        })
        .join("");
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
      const core = (i: number) => (tokens[i] === undefined ? undefined : splitToken(tokens[i])[1]);
      return {
        ...r,
        suggestions: r.suggestions.filter((s) => core(s.token) === s.decoded),
        changed_words: r.changed_words.filter((c) => core(c.token) === c.written),
      };
    }),
  };
}

/** The mock's common words (T12.15f): a short fixture in place of the matcher's 5,000 AMI
 * words (`earshot_core::vocabulary::suggest::is_common`), which the mock does not carry. */
export const MOCK_COMMON_WORDS = new Set(
  (
    "a about an and are as at be but by can do for from go had has have he her i i'll if in is it " +
    "its it's me my no not of ok okay on or our so that that's the their them then there they " +
    "think this to up us was we what when which who will with would yes you your"
  ).split(" "),
);

/** `vocabulary::words_to_offer`: each token of `after` whose word (`dictionaryWord`) is not the
 * core of a token of `before`, leaving out common words (`MOCK_COMMON_WORDS`), words on
 * `listed` (case ignored) and repeats in another case; in line order. */
export function wordsToOffer(before: string, after: string, listed: string[]): string[] {
  const was = new Set(tokensOf(before).map((t) => withoutQuotesAndPunctuation(splitToken(t)[1])));
  const onList = new Set(listed.map((w) => w.toLowerCase()));
  const seen = new Set<string>();
  const out: string[] = [];
  for (const t of tokensOf(after)) {
    const w = dictionaryWord(t);
    if (w === null || was.has(w) || MOCK_COMMON_WORDS.has(w.toLowerCase()) || onList.has(w.toLowerCase())) continue;
    if (seen.has(w.toLowerCase())) continue;
    seen.add(w.toLowerCase());
    out.push(w);
  }
  return out;
}
