// Settings, Dictionary (T12.15a; the owner, 2026-11-10: its own page, after Summaries): the
// user's own words, such as names, acronyms and company or site names, given to every summary
// request so the summary spells them right (`earshot_runtime::vocabulary`). An Add field above
// the words in alphabetical order (the owner, 2026-11-11: a long list never pushes Add off the
// page), each word with its own Delete; both apply at once (docs/ui-design.md
// "Settings"). Transcripts do not use the words yet (T12.15b).

import {
  Body1,
  Button,
  Caption1,
  Field,
  Input,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  Spinner,
  Subtitle2,
  Title2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { useId } from "react";

import { useDictionary } from "./useDictionary";

const useStyles = makeStyles({
  page: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXL,
    maxWidth: "860px",
  },
  heading: {
    margin: 0,
  },
  text: {
    maxWidth: "70ch",
  },
  section: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
    maxWidth: "520px",
  },
  list: {
    listStyle: "none",
    margin: 0,
    padding: 0,
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
  },
  word: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    gap: tokens.spacingHorizontalS,
    padding: `${tokens.spacingVerticalXS} ${tokens.spacingHorizontalM}`,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke1}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorNeutralBackground1,
    overflowWrap: "anywhere",
  },
  add: {
    display: "flex",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
  field: {
    flexGrow: 1,
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
});

/** What the page is for, first on the page. */
export const DICTIONARY_INTRO =
  "Words Earshot should spell your way: names, acronyms, and company or site names. Summaries use them to spell these right, and transcripts will use them later. They are sent with the meeting to whatever writes your summary.";

/** The Add field's label. */
export const ADD_LABEL = "Add a word";

/** The words as the page lists them: alphabetical, ignoring case and accents (the stored order,
 * which every summary prompt uses, is unchanged). */
export function sortedWords(words: readonly string[]): string[] {
  return [...words].sort((a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }) || (a < b ? -1 : 1));
}

/** The Dictionary page. */
export function DictionaryPage() {
  const styles = useStyles();
  const state = useDictionary();
  const headingId = useId();
  const { load } = state;

  return (
    <div className={styles.page}>
      <Title2 as="h1" className={styles.heading}>
        Dictionary
      </Title2>
      <Body1 className={styles.text}>{DICTIONARY_INTRO}</Body1>
      <section className={styles.section} aria-labelledby={headingId}>
        <Subtitle2 as="h2" id={headingId} className={styles.heading}>
          Your words
        </Subtitle2>
        {load.status === "loading" ? (
          <Spinner size="small" label="Reading your dictionary" />
        ) : load.status === "failed" ? (
          <MessageBar intent="error">
            <MessageBarBody>
              <MessageBarTitle>Your dictionary could not be read</MessageBarTitle>
              {load.message}
            </MessageBarBody>
          </MessageBar>
        ) : (
          <>
            {load.view.unknown.length > 0 && (
              <MessageBar intent="warning">
                <MessageBarBody>
                  <MessageBarTitle>The stored dictionary is not used</MessageBarTitle>
                  {load.view.unknown.join(" ")}
                </MessageBarBody>
              </MessageBar>
            )}
            <form
              aria-label={ADD_LABEL}
              onSubmit={(e) => {
                e.preventDefault();
                void state.add();
              }}
            >
              <Field
                label={ADD_LABEL}
                hint={`${load.view.words.length} of ${load.view.max_words} words, each up to ${load.view.max_word_chars} characters.`}
                validationState={state.addMessage?.intent ?? "none"}
                validationMessage={state.addMessage?.text}
              >
                <div className={styles.add}>
                  <Input
                    className={styles.field}
                    value={state.draft}
                    placeholder="For example: a product name or an acronym"
                    onChange={(_, d) => state.setDraft(d.value)}
                  />
                  <Button type="submit" appearance="primary" disabled={state.busy || state.draft.trim() === ""}>
                    Add
                  </Button>
                </div>
              </Field>
            </form>
            {load.view.words.length === 0 ? (
              <Caption1 className={styles.muted}>No words yet.</Caption1>
            ) : (
              <ul className={styles.list} aria-label="Your words">
                {sortedWords(load.view.words).map((word) => (
                  <li key={word} className={styles.word}>
                    <Body1>{word}</Body1>
                    <Button
                      size="small"
                      aria-label={`Delete ${word}`}
                      disabled={state.busy}
                      onClick={() => void state.remove(word)}
                    >
                      Delete
                    </Button>
                  </li>
                ))}
              </ul>
            )}
            {state.deleteError !== null && (
              <MessageBar intent="error">
                <MessageBarBody>
                  <MessageBarTitle>The word could not be deleted</MessageBarTitle>
                  {state.deleteError}
                </MessageBarBody>
              </MessageBar>
            )}
          </>
        )}
      </section>
    </div>
  );
}
