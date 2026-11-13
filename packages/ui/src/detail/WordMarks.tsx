// Suggestions beside words in the transcript (T12.15e, the owner's design): a word the
// word list suggests a listed word for has a dotted underline; a word changed from a
// suggestion keeps a quiet solid one. Each is a real button in the Tab order whose popover
// offers the choices; nothing changes until one is chosen. The rules (which tokens, what
// a choice writes) are the runtime's (`word_choices`, `edit::use_word`): this file only
// draws the marks the detail carries. Since T12.15f every other word of a final line is its
// own element carrying `WORD_ATTRIBUTE`, so a right-click finds its word, and a mark's popover
// also offers "Add to Dictionary" or "Remove from Dictionary" (`DictionaryWords.tsx`).

import {
  Body1,
  Body1Strong,
  Button,
  Combobox,
  Field,
  Option,
  Popover,
  PopoverSurface,
  PopoverTrigger,
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import { Fragment, type ReactNode, useState } from "react";

import type { TranscriptRow } from "../ipc";
import { dictionaryWord, inDictionary, splitToken } from "./dictionaryWord";

const useStyles = makeStyles({
  // A button that reads as the word it marks.
  mark: {
    margin: 0,
    padding: 0,
    border: "none",
    backgroundColor: "transparent",
    color: "inherit",
    fontFamily: "inherit",
    fontSize: "inherit",
    fontWeight: "inherit",
    lineHeight: "inherit",
    cursor: "pointer",
    textDecorationLine: "underline",
    textUnderlineOffset: "3px",
    borderRadius: tokens.borderRadiusSmall,
    ":focus-visible": {
      outline: `${tokens.strokeWidthThick} solid ${tokens.colorStrokeFocus2}`,
      outlineOffset: "1px",
    },
  },
  suggestion: {
    textDecorationStyle: "dotted",
    textDecorationThickness: tokens.strokeWidthThick,
    textDecorationColor: tokens.colorBrandForeground1,
  },
  // Quieter than a suggestion: thin, solid and neutral.
  changed: {
    textDecorationStyle: "solid",
    textDecorationThickness: tokens.strokeWidthThin,
    textDecorationColor: tokens.colorNeutralStroke1,
  },
  surface: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
    maxWidth: "280px",
  },
  buttons: {
    display: "flex",
    gap: tokens.spacingHorizontalS,
    flexWrap: "wrap",
  },
  // A word the Dictionary bar offers, while the bar is open (T12.15f, the owner: a soft
  // purple or green, never yellow).
  tinted: {
    backgroundColor: tokens.colorPaletteLavenderBackground2,
    borderRadius: tokens.borderRadiusSmall,
  },
});

/** What the marks of a line do; each resolves once the detail was read again. */
export interface WordActions {
  /** "Use it", or another word from the list: token `token` becomes `word`. */
  onUse(token: number, word: string): void;
  /** "Put back": token `token` reads as decoded again. */
  onPutBack(token: number): void;
  /** "Keep": the suggestion for token `token` goes; resolves once the detail was read again. */
  onKeep(token: number): void | Promise<unknown>;
}

/** The Dictionary as a line's words see it (T12.15f). */
export interface DictionaryState {
  /** The Dictionary's entries (`MeetingDetail.dictionary`). */
  entries: string[];
  /** "Add to Dictionary", or "Remove from Dictionary" for an entry, on word `word`. */
  onToggle(word: string): void;
}

/** The attribute that names a word of a final line (T12.15f): a right-click on an element
 * carrying it, or the Menu key or Shift+F10 on a mark, opens that word's Dictionary menu. */
export const WORD_ATTRIBUTE = "data-dictionary-word";

/** The text of a transcript line with its marks. `text` is split on runs of white space
 * exactly as the runtime numbers tokens; a mark wraps only the word inside its token, so the
 * punctuation around it stays plain text. A mark whose word is not in its token (a stale
 * detail) is not drawn. Every other word (`dictionaryWord`) is a plain span carrying
 * [`WORD_ATTRIBUTE`], tinted while it is in `tinted` (the words the bar under a just-edited
 * line offers, T12.15f). */
export function LineText({
  row,
  wordList,
  actions,
  locked = false,
  dictionary,
  tinted = [],
}: {
  row: TranscriptRow;
  wordList: string[];
  actions: WordActions;
  /** The meeting is busy: the marks show, but no choice changes the line. */
  locked?: boolean;
  dictionary: DictionaryState;
  tinted?: string[];
}): ReactNode {
  const styles = useStyles();
  let token = -1;
  return row.text.split(/(\p{White_Space}+)/u).map((piece, i) => {
    if (piece === "" || /^\p{White_Space}+$/u.test(piece)) return piece;
    token += 1;
    const t = token;
    const suggestion = row.suggestions.find((s) => s.token === t);
    const changed = row.changed_words.find((c) => c.token === t);
    const word = changed?.written ?? suggestion?.decoded;
    const at = word === undefined ? -1 : piece.indexOf(word);
    if (word === undefined || at < 0) {
      const plain = dictionaryWord(piece);
      if (plain === null) return piece;
      const [pre, core, post] = splitToken(piece);
      return (
        <Fragment key={i}>
          {pre}
          <span
            {...{ [WORD_ATTRIBUTE]: plain }}
            className={tinted.includes(plain) ? styles.tinted : undefined}
          >
            {core}
          </span>
          {post}
        </Fragment>
      );
    }
    const listed = inDictionary(dictionary.entries, word);
    return (
      // Keyed by position, so a suggestion that becomes a changed word keeps its button and
      // the focus on it.
      <Fragment key={i}>
        {piece.slice(0, at)}
        {changed !== undefined ? (
          <WordMark
            kind="changed"
            word={word}
            decoded={changed.decoded}
            wordList={wordList}
            locked={locked}
            listed={listed}
            onDictionary={() => dictionary.onToggle(word)}
            onUse={(w) => actions.onUse(t, w)}
            onPutBack={() => actions.onPutBack(t)}
          />
        ) : (
          <WordMark
            kind="suggestion"
            word={word}
            suggested={suggestion?.listed ?? ""}
            locked={locked}
            listed={listed}
            onDictionary={() => dictionary.onToggle(word)}
            onUse={(w) => actions.onUse(t, w)}
            onKeep={() => actions.onKeep(t)}
          />
        )}
        {piece.slice(at + word.length)}
      </Fragment>
    );
  });
}

/** What both kinds of mark share: the word and its Dictionary action (T12.15f). */
interface MarkBase {
  word: string;
  /** The word is a Dictionary entry: the popover offers Remove rather than Add. */
  listed: boolean;
  /** The meeting is busy: the choices that change the line are off. */
  locked: boolean;
  onDictionary(): void;
}

type MarkProps = MarkBase &
  (
    | { kind: "suggestion"; suggested: string; onUse(word: string): void; onKeep(): void }
    | {
        kind: "changed";
        decoded: string;
        wordList: string[];
        onUse(word: string): void;
        onPutBack(): void;
      }
  );

function WordMark(props: MarkProps) {
  const styles = useStyles();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const name = props.kind === "suggestion" ? `Suggestion for ${props.word}` : `Changed word ${props.word}`;
  const choose = (f: () => void) => {
    setOpen(false);
    f();
  };
  // T12.15f: the same action as the word's menu.
  const dictionaryButton = (
    <Button size="small" appearance="subtle" onClick={() => choose(props.onDictionary)}>
      {props.listed ? "Remove from Dictionary" : "Add to Dictionary"}
    </Button>
  );
  const wanted = query.trim().toLowerCase();
  // The list is deduplicated without case, so the written word is left out the same way.
  const written = props.word.toLowerCase();
  const others =
    props.kind === "changed"
      ? props.wordList.filter((w) => w.toLowerCase() !== written && w.toLowerCase().includes(wanted))
      : [];
  return (
    <Popover
      open={open}
      onOpenChange={(_, d) => {
        setOpen(d.open);
        if (d.open) setQuery("");
      }}
      positioning="below-start"
      withArrow
      trapFocus
    >
      <PopoverTrigger disableButtonEnhancement>
        <button
          type="button"
          data-word-mark={props.kind}
          {...{ [WORD_ATTRIBUTE]: props.word }}
          aria-label={name}
          className={mergeClasses(styles.mark, props.kind === "suggestion" ? styles.suggestion : styles.changed)}
        >
          {props.word}
        </button>
      </PopoverTrigger>
      <PopoverSurface aria-label={name}>
        {props.kind === "suggestion" ? (
          <div className={styles.surface}>
            <Body1>
              Your dictionary has <Body1Strong>{props.suggested}</Body1Strong>
            </Body1>
            <div className={styles.buttons}>
              <Button
                appearance="primary"
                size="small"
                disabled={props.locked}
                onClick={() => choose(() => props.onUse(props.suggested))}
              >
                Use it
              </Button>
              <Button size="small" disabled={props.locked} onClick={() => choose(props.onKeep)}>
                Keep
              </Button>
              {dictionaryButton}
            </div>
          </div>
        ) : (
          <div className={styles.surface}>
            <Body1>
              Changed from <Body1Strong>{props.decoded}</Body1Strong>
            </Body1>
            <div className={styles.buttons}>
              <Button size="small" disabled={props.locked} onClick={() => choose(props.onPutBack)}>
                Put back {props.decoded}
              </Button>
              {dictionaryButton}
            </div>
            {props.wordList.some((w) => w.toLowerCase() !== written) && (
              <Field label="Another word" size="small">
                <Combobox
                  size="small"
                  disabled={props.locked}
                  placeholder="Find a word"
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  onOptionSelect={(_, d) => {
                    const w = d.optionValue;
                    if (w !== undefined) choose(() => props.onUse(w));
                  }}
                >
                  {others.map((w) => (
                    <Option key={w} value={w}>
                      {w}
                    </Option>
                  ))}
                </Combobox>
              </Field>
            )}
          </div>
        )}
      </PopoverSurface>
    </Popover>
  );
}
