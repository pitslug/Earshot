// Suggestions beside words in the transcript (T12.15e, the owner's design): a word the
// word list suggests a listed word for has a dotted underline; a word changed from a
// suggestion keeps a quiet solid one. Each is a real button in the Tab order whose popover
// offers the choices; nothing changes until one is chosen. The rules (which tokens, what
// a choice writes) are the runtime's (`word_choices`, `edit::use_word`): this file only
// draws the marks the detail carries.

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
});

/** What the marks of a line do; each resolves once the detail was read again. */
export interface WordActions {
  /** "Use it", or another word from the list: token `token` becomes `word`. */
  onUse(token: number, word: string): void;
  /** "Put back": token `token` reads as decoded again. */
  onPutBack(token: number): void;
  /** "Keep": the suggestion for token `token` goes. */
  onKeep(token: number): void;
}

/** The text of a transcript line with its marks. `text` is split on runs of white space
 * exactly as the runtime numbers tokens; a mark wraps only the word inside its token, so the
 * punctuation around it stays plain text. A mark whose word is not in its token (a stale
 * detail) is not drawn. */
export function LineText({
  row,
  wordList,
  actions,
}: {
  row: TranscriptRow;
  wordList: string[];
  actions: WordActions;
}): ReactNode {
  if (row.suggestions.length === 0 && row.changed_words.length === 0) return row.text;
  let token = -1;
  return row.text.split(/(\p{White_Space}+)/u).map((piece, i) => {
    if (piece === "" || /^\p{White_Space}+$/u.test(piece)) return piece;
    token += 1;
    const t = token;
    const suggestion = row.suggestions.find((s) => s.token === t);
    const changed = row.changed_words.find((c) => c.token === t);
    const word = changed?.written ?? suggestion?.decoded;
    const at = word === undefined ? -1 : piece.indexOf(word);
    if (word === undefined || at < 0) return piece;
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
            onUse={(w) => actions.onUse(t, w)}
            onPutBack={() => actions.onPutBack(t)}
          />
        ) : (
          <WordMark
            kind="suggestion"
            word={word}
            listed={suggestion?.listed ?? ""}
            onUse={(w) => actions.onUse(t, w)}
            onKeep={() => actions.onKeep(t)}
          />
        )}
        {piece.slice(at + word.length)}
      </Fragment>
    );
  });
}

type MarkProps =
  | { kind: "suggestion"; word: string; listed: string; onUse(word: string): void; onKeep(): void }
  | {
      kind: "changed";
      word: string;
      decoded: string;
      wordList: string[];
      onUse(word: string): void;
      onPutBack(): void;
    };

function WordMark(props: MarkProps) {
  const styles = useStyles();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const name = props.kind === "suggestion" ? `Suggestion for ${props.word}` : `Changed word ${props.word}`;
  const choose = (f: () => void) => {
    setOpen(false);
    f();
  };
  const wanted = query.trim().toLowerCase();
  const others =
    props.kind === "changed"
      ? props.wordList.filter((w) => w !== props.word && w.toLowerCase().includes(wanted))
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
          aria-label={name}
          className={mergeClasses(styles.mark, props.kind === "suggestion" ? styles.suggestion : styles.changed)}
          // The word is not the line: a click here never starts a line edit.
          onClick={(e) => e.stopPropagation()}
        >
          {props.word}
        </button>
      </PopoverTrigger>
      <PopoverSurface aria-label={name}>
        {props.kind === "suggestion" ? (
          <div className={styles.surface}>
            <Body1>
              Your dictionary has <Body1Strong>{props.listed}</Body1Strong>
            </Body1>
            <div className={styles.buttons}>
              <Button appearance="primary" size="small" onClick={() => choose(() => props.onUse(props.listed))}>
                Use it
              </Button>
              <Button size="small" onClick={() => choose(props.onKeep)}>
                Keep
              </Button>
            </div>
          </div>
        ) : (
          <div className={styles.surface}>
            <Body1>
              Changed from <Body1Strong>{props.decoded}</Body1Strong>
            </Body1>
            <div className={styles.buttons}>
              <Button size="small" onClick={() => choose(props.onPutBack)}>
                Put back {props.decoded}
              </Button>
            </div>
            {props.wordList.some((w) => w !== props.word) && (
              <Field label="Another word" size="small">
                <Combobox
                  size="small"
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
