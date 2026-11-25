// Find in the transcript (T12.98, the owner's design after two mockups): a bar under the
// Summary/Transcript tabs, opened by the search button at the end of the tab row or Ctrl+F.
// It holds a speaker choice, a text box, the count, previous and next, and close; close or Esc
// clears both and hides it. What it finds is `findMatches.ts`; the lines draw the result
// (`LineFind`, `markRanges`).

import {
  Button,
  Caption1,
  Dropdown,
  Input,
  Option,
  Tooltip,
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import { type ReactNode, type Ref, forwardRef, useId } from "react";

import type { FindSpeaker } from "./findMatches";

/** The current match's frame. */
const FRAME = tokens.colorPaletteDarkOrangeBorderActive;

const useStyles = makeStyles({
  bar: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    columnGap: tokens.spacingHorizontalS,
    rowGap: tokens.spacingVerticalXS,
    paddingTop: tokens.spacingVerticalXS,
    paddingBottom: tokens.spacingVerticalS,
  },
  speaker: {
    minWidth: "170px",
  },
  // Read by assistive technology only: the speaker choice's name.
  hiddenLabel: {
    position: "absolute",
    width: "1px",
    height: "1px",
    margin: "-1px",
    padding: 0,
    overflow: "hidden",
    clipPath: "inset(50%)",
    whiteSpace: "nowrap",
    border: 0,
  },
  text: {
    flexGrow: 1,
    flexBasis: "180px",
    maxWidth: "320px",
  },
  count: {
    color: tokens.colorNeutralForeground3,
    fontVariantNumeric: "tabular-nums",
    // Wide enough for the longest fixed text ("Pick a speaker or type", the too-short hint), so
    // nothing after it moves when the count changes (T14.17).
    minWidth: "12.5em",
    whiteSpace: "nowrap",
  },
  // A text match: a soft yellow behind the words, readable in light and dark.
  mark: {
    backgroundColor: tokens.colorPaletteYellowBackground2,
    color: "inherit",
    borderRadius: tokens.borderRadiusSmall,
  },
  // The match the count points at: a frame drawn side by side, so a match cut into parts (a
  // word's own element, the space after it) reads as one frame.
  current: {
    boxShadow: `inset 0 2px 0 ${FRAME}, inset 0 -2px 0 ${FRAME}`,
  },
  currentFirst: {
    boxShadow: `inset 0 2px 0 ${FRAME}, inset 0 -2px 0 ${FRAME}, inset 2px 0 0 ${FRAME}`,
  },
  currentLast: {
    boxShadow: `inset 0 2px 0 ${FRAME}, inset 0 -2px 0 ${FRAME}, inset -2px 0 0 ${FRAME}`,
  },
  currentWhole: {
    boxShadow: `inset 0 0 0 2px ${FRAME}`,
  },
});

/** How find shows on one transcript line; `undefined` for a line it leaves alone, so a line's
 * props change only when its own highlight does (T9.24). */
export interface LineFind {
  /** The line is the chosen speaker's: tinted. */
  speaker: boolean;
  /** The line is the current turn's start (a speaker alone): outlined. */
  currentTurn: boolean;
  /** Text matches in the line, code-unit ranges of its text. */
  matches: readonly (readonly [number, number])[];
  /** The index in `matches` of the current match, -1 when it is on another line. */
  current: number;
}

/** Whether two line states draw the same. */
export function sameLineFind(a: LineFind | undefined, b: LineFind | undefined): boolean {
  if (a === b) return true;
  if (a === undefined || b === undefined) return false;
  return (
    a.speaker === b.speaker &&
    a.currentTurn === b.currentTurn &&
    a.current === b.current &&
    a.matches.length === b.matches.length &&
    a.matches.every(([s, e], i) => b.matches[i][0] === s && b.matches[i][1] === e)
  );
}

/** Draws find's text marks: `(text, offset, find)` gives `text`, which starts at code unit
 * `offset` of its line, with the parts inside `find`'s matches wrapped in `<mark>`, and `text`
 * itself when none falls in it (so a line find leaves alone renders as before). A match
 * crossing the end of `text` is cut there: the rest is marked by the next piece. */
export function useFindMarks(): (text: string, offset: number, find: LineFind | undefined) => ReactNode {
  const styles = useStyles();
  return (text, offset, find) => {
    if (find === undefined || find.matches.length === 0 || text === "") return text;
    const end = offset + text.length;
    const out: ReactNode[] = [];
    let at = offset;
    find.matches.forEach(([s, e], i) => {
      const from = Math.max(s, at);
      const to = Math.min(e, end);
      if (from >= to) return;
      if (from > at) out.push(text.slice(at - offset, from - offset));
      const first = from === s;
      const last = to === e;
      const frame =
        i !== find.current
          ? undefined
          : first && last
            ? styles.currentWhole
            : first
              ? styles.currentFirst
              : last
                ? styles.currentLast
                : styles.current;
      out.push(
        <mark
          key={i}
          data-find-match={i === find.current ? "current" : "match"}
          data-find-index={i}
          className={mergeClasses(styles.mark, frame)}
        >
          {text.slice(from - offset, to - offset)}
        </mark>,
      );
      at = to;
    });
    if (out.length === 0) return text;
    if (at < end) out.push(text.slice(at - offset));
    return out;
  };
}

/** The search button at the end of the tab row. */
export const FindButton = forwardRef(function FindButton(
  { expanded, controls, onClick }: { expanded: boolean; controls: string | undefined; onClick(): void },
  ref: Ref<HTMLButtonElement>,
) {
  return (
    <Tooltip content="Find in transcript (Ctrl+F)" relationship="description">
      <Button
        ref={ref}
        appearance="subtle"
        icon={<SearchIcon />}
        aria-label="Find in transcript"
        aria-keyshortcuts="Control+F"
        aria-expanded={expanded}
        aria-controls={controls}
        onClick={onClick}
      />
    </Tooltip>
  );
});

/** The find bar. Everything it shows is the caller's state. */
export function TranscriptFind({
  id,
  speakers,
  speaker,
  text,
  count,
  countText,
  inputRef,
  onSpeaker,
  onText,
  onStep,
  onClose,
}: {
  id: string;
  speakers: FindSpeaker[];
  /** The chosen speaker's key, `null` for any. */
  speaker: string | null;
  text: string;
  /** How many places previous and next step through. */
  count: number;
  countText: string;
  inputRef: Ref<HTMLInputElement>;
  onSpeaker(key: string | null): void;
  onText(text: string): void;
  onStep(by: 1 | -1): void;
  onClose(): void;
}) {
  const styles = useStyles();
  const chosen = speakers.find((s) => s.key === speaker);
  const labelId = useId();
  const dropdownId = useId();
  return (
    <div
      id={id}
      role="search"
      aria-label="Find in transcript"
      className={styles.bar}
      onKeyDown={(e) => {
        if (e.key !== "Escape" || !e.currentTarget.contains(e.target as Node)) return;
        // Escape on an open speaker list closes the list only.
        if ((e.target as Element).getAttribute("aria-expanded") === "true") return;
        e.preventDefault();
        onClose();
      }}
    >
      <span id={labelId} className={styles.hiddenLabel}>
        Speaker
      </span>
      {/* Named by its label and its own text, so the chosen speaker is read with the name. */}
      <Dropdown
        id={dropdownId}
        className={styles.speaker}
        aria-labelledby={`${labelId} ${dropdownId}`}
        value={chosen?.name ?? "Any speaker"}
        selectedOptions={[speaker ?? ""]}
        onOptionSelect={(_, d) => onSpeaker(d.optionValue === undefined || d.optionValue === "" ? null : d.optionValue)}
      >
        <Option value="">Any speaker</Option>
        {speakers.map((s) => (
          <Option key={s.key} value={s.key}>
            {s.name}
          </Option>
        ))}
      </Dropdown>
      <Input
        ref={inputRef}
        className={styles.text}
        aria-label="Text to find"
        placeholder="Find in transcript"
        value={text}
        onChange={(_, d) => onText(d.value)}
        onKeyDown={(e) => {
          if (e.key !== "Enter") return;
          e.preventDefault();
          onStep(e.shiftKey ? -1 : 1);
        }}
      />
      <Caption1 role="status" className={styles.count}>
        {countText}
      </Caption1>
      <Button
        appearance="subtle"
        icon={<UpIcon />}
        aria-label="Previous match"
        disabled={count === 0}
        onClick={() => onStep(-1)}
      />
      <Button
        appearance="subtle"
        icon={<DownIcon />}
        aria-label="Next match"
        disabled={count === 0}
        onClick={() => onStep(1)}
      />
      <Button appearance="subtle" icon={<CloseIcon />} aria-label="Close find" onClick={onClose} />
    </div>
  );
}

function SearchIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
      <circle cx="6.75" cy="6.75" r="4.25" />
      <path d="M10 10l3.75 3.75" strokeLinecap="round" />
    </svg>
  );
}

function UpIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
      <path d="M4 10l4-4 4 4" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

function DownIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
      <path d="M4 6l4 4 4-4" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

function CloseIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
      <path d="M4 4l8 8M12 4l-8 8" strokeLinecap="round" />
    </svg>
  );
}
