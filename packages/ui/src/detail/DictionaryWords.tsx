// Adding words to the Dictionary from the transcript (T12.15f, the owner's design): never
// automatic, a word joins only from a click on that word. Two ways in: the bar under a line just
// edited ("Add to Dictionary:" with one button per word the edit wrote, as `editSegment`
// returns them), and a word's menu (right-click, the Menu key or Shift+F10). A typo is
// easy to take back: the confirmation's Undo removes exactly that word, and a listed word offers
// Remove. The rules (which words, the limits and refusals) are the runtime's
// (`vocabulary::add_word`, `words_to_offer`); this file only draws them.

import {
  Body1,
  Button,
  Caption1,
  Menu,
  MenuItem,
  MenuList,
  MenuPopover,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { type KeyboardEvent, type Ref } from "react";

import type { TranscriptRow } from "../ipc";
import { inDictionary } from "./dictionaryWord";

const useStyles = makeStyles({
  bar: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
    marginTop: tokens.spacingVerticalXS,
  },
  // The same soft tint as the line's words the bar offers (`WordMarks` `tinted`).
  word: {
    backgroundColor: tokens.colorPaletteLavenderBackground2,
    ":hover": { backgroundColor: tokens.colorPaletteLavenderBorderActive },
  },
  error: {
    color: tokens.colorPaletteRedForeground1,
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
});

/** What the note under a line says after a Dictionary action. */
export type DictionaryNote =
  | { segment: string; kind: "added"; word: string }
  | { segment: string; kind: "already"; word: string }
  | { segment: string; kind: "removed"; word: string }
  | { segment: string; kind: "error"; text: string };

/** How many lines other than `segment` carry a suggestion of `word` (T12.15f: an add makes the
 * new word suggested on other lines once the detail is read again). */
export function otherLinesSuggesting(transcript: TranscriptRow[], segment: string, word: string): number {
  const wanted = word.toLowerCase();
  return transcript.filter(
    (r) => r.segment_id !== segment && r.suggestions.some((s) => s.listed.toLowerCase() === wanted),
  ).length;
}

/** The bar under a line just edited: one button per offered word not yet in the Dictionary.
 * Nothing shows once every word is listed. */
export function DictionaryBar({
  words,
  dictionary,
  onAdd,
  onClose,
}: {
  words: string[];
  dictionary: string[];
  onAdd(word: string): void;
  onClose(): void;
}) {
  const styles = useStyles();
  const left = words.filter((w) => !inDictionary(dictionary, w));
  if (left.length === 0) return null;
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      onClose();
    }
  };
  return (
    <div role="group" aria-label="Add to Dictionary" className={styles.bar} onKeyDown={onKeyDown}>
      <Body1>Add to Dictionary:</Body1>
      {left.map((w) => (
        <Button
          key={w}
          size="small"
          className={styles.word}
          data-offered-word={w}
          aria-label={`Add "${w}" to Dictionary`}
          onClick={() => onAdd(w)}
        >
          {w}
        </Button>
      ))}
      <Button size="small" appearance="subtle" onClick={onClose}>
        Not now
      </Button>
    </div>
  );
}

/** The note under a line after a Dictionary action: "Added "<word>". Undo", with how many other
 * lines now carry a suggestion of it; a refusal in the Dictionary page's own words. It stays
 * until the user moves on. */
export function DictionaryNoteLine({
  note,
  others,
  undoRef,
  onUndo,
}: {
  note: DictionaryNote;
  others: number;
  undoRef: Ref<HTMLButtonElement>;
  onUndo(): void;
}) {
  const styles = useStyles();
  return (
    <div role="status" className={styles.bar}>
      {note.kind === "added" && (
        <>
          <Body1>Added "{note.word}".</Body1>
          <Button size="small" appearance="subtle" ref={undoRef} onClick={onUndo}>
            Undo
          </Button>
          {others > 0 && (
            <Caption1 className={styles.muted}>
              {others === 1 ? "1 other line now has a suggestion." : `${others} other lines now have a suggestion.`}
            </Caption1>
          )}
        </>
      )}
      {note.kind === "already" && <Body1>"{note.word}" is already in your Dictionary.</Body1>}
      {note.kind === "removed" && <Body1>Removed "{note.word}" from your Dictionary.</Body1>}
      {note.kind === "error" && (
        <Body1 className={styles.error}>
          The Dictionary was not changed. {note.text}
        </Body1>
      )}
    </div>
  );
}

/** Where a word's menu opens: the word's element, or a point (a right-click). */
export type MenuTarget = HTMLElement | { getBoundingClientRect(): DOMRect };

/** A point as a positioning target. */
export function pointTarget(x: number, y: number): MenuTarget {
  return { getBoundingClientRect: () => new DOMRect(x, y, 0, 0) };
}

/** The Dictionary menu of one word, or of every word of a line (the Menu key on a line's Edit
 * button, where no single word has focus). No "Edit line" item: a line edit starts only from
 * its Edit button. */
export function WordMenu({
  words,
  dictionary,
  target,
  onToggle,
  onClose,
}: {
  words: string[];
  dictionary: string[];
  target: MenuTarget | null;
  onToggle(word: string): void;
  onClose(): void;
}) {
  return (
    <Menu
      open={target !== null && words.length > 0}
      onOpenChange={(_, d) => {
        if (!d.open) onClose();
      }}
      positioning={{ target: target ?? undefined, position: "below", align: "start" }}
    >
      <MenuPopover>
        <MenuList>
          {words.map((w) => (
            <MenuItem key={w} onClick={() => onToggle(w)}>
              {inDictionary(dictionary, w) ? `Remove "${w}" from Dictionary` : `Add "${w}" to Dictionary`}
            </MenuItem>
          ))}
        </MenuList>
      </MenuPopover>
    </Menu>
  );
}
