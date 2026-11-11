// Which word of a transcript line "Add to Dictionary" adds (T12.15f). The app's rule is
// `earshot_runtime::vocabulary::dictionary_word`, which does the adding; this copy only decides
// where the menu offers it, and `fixtures/ui/dictionary_word_cases.json` (run by both suites)
// keeps the two the same.

const WORD_CHAR = /[\p{Alphabetic}\p{N}']/u;
const LETTER = /\p{Alphabetic}/u;

/** `token` split into leading punctuation, its core (from the first to the last letter, digit
 * or apostrophe) and trailing punctuation; a token with no core is all leading punctuation. */
export function splitToken(token: string): [string, string, string] {
  const chars = [...token];
  const first = chars.findIndex((c) => WORD_CHAR.test(c));
  if (first < 0) return [token, "", ""];
  let last = chars.length - 1;
  while (!WORD_CHAR.test(chars[last])) last -= 1;
  return [chars.slice(0, first).join(""), chars.slice(first, last + 1).join(""), chars.slice(last + 1).join("")];
}

/** The word `token` adds: its core, when the core holds a letter; `null` for punctuation or a
 * number. */
export function dictionaryWord(token: string): string | null {
  const core = splitToken(token.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, ""))[1];
  return LETTER.test(core) ? core : null;
}

/** Whether `word` is an entry of the Dictionary (case ignored, as the Dictionary drops a
 * repeat in another case): such a word offers "Remove from Dictionary". */
export function inDictionary(dictionary: string[], word: string): boolean {
  const wanted = word.toLowerCase();
  return dictionary.some((e) => e.toLowerCase() === wanted);
}
