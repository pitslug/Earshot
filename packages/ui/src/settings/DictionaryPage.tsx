// Settings, Dictionary (T12.15a; the owner, 2026-11-10: its own page, after Summaries): the
// user's own words, such as names, acronyms and company or site names, given to every summary
// request so the summary spells them right (`earshot_runtime::vocabulary`). An Add field above
// the words in alphabetical order (the owner, 2026-11-11: a long list never pushes Add off the
// page), each word with its own Delete; both apply at once (docs/ui-design.md
// "Settings"). Below the words, the saved people's names (T12.91), read only: they come from
// the People page, and each summary request is given only the names that match what was said.

import {
  Body1,
  Button,
  Caption1,
  Field,
  Input,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  Subtitle2,
  Title2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { useId } from "react";

import { FIELD_WIDTH, NotLoaded, TEXT_WIDTH, UnknownSettings, usePageStyles } from "./parts";
import { useDictionary } from "./useDictionary";

const useStyles = makeStyles({
  heading: {
    margin: 0,
  },
  text: {
    maxWidth: TEXT_WIDTH,
  },
  section: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
  },
  list: {
    listStyle: "none",
    margin: 0,
    padding: 0,
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
    // Short words: at the page's width each Remove button would sit far from its word.
    maxWidth: FIELD_WIDTH,
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
    maxWidth: FIELD_WIDTH,
  },
  field: {
    flexGrow: 1,
  },
  muted: {
    color: tokens.colorNeutralForeground3,
    maxWidth: TEXT_WIDTH,
  },
});

/** What the page is for, first on the page. */
export const DICTIONARY_INTRO =
  "Words Earshot should spell your way: names, acronyms, and company or site names. Summaries use them to spell these right, and transcripts suggest them where they hear something similar. They are sent with the meeting to whatever writes your summary.";

/** The Add field's label. */
export const ADD_LABEL = "Add a word";

/** What the saved people's names are, above them (T12.91). */
export const SAVED_PEOPLE_TEXT =
  "Names of the people you saved on the People page. Change them there. A summary is given only the names that match what was said.";

/** The words as the page lists them: alphabetical, ignoring case and accents (the stored order,
 * which every summary prompt uses, is unchanged). */
export function sortedWords(words: readonly string[]): string[] {
  return [...words].sort((a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }) || (a < b ? -1 : 1));
}

/** The Dictionary page. */
export function DictionaryPage() {
  const styles = useStyles();
  const page = usePageStyles();
  const state = useDictionary();
  const headingId = useId();
  const peopleId = useId();
  const { load } = state;

  return (
    <div className={page.page}>
      <Title2 as="h1" className={styles.heading}>
        Dictionary
      </Title2>
      <Body1 className={styles.text}>{DICTIONARY_INTRO}</Body1>
      <section className={styles.section} aria-labelledby={headingId}>
        <Subtitle2 as="h2" id={headingId} className={styles.heading}>
          Your words
        </Subtitle2>
        {load.status !== "loaded" ? (
          <NotLoaded
            load={load}
            size="small"
            loading="Reading your dictionary"
            failed="Your dictionary could not be read"
          />
        ) : (
          <>
            <UnknownSettings unknown={load.view.unknown} title="The stored dictionary is not used" />
            {load.view.not_in_use_note !== null && (
              <MessageBar intent="warning" aria-label="Words not in use">
                <MessageBarBody>{load.view.not_in_use_note}</MessageBarBody>
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
      {load.status === "loaded" && (
        <section className={styles.section} aria-labelledby={peopleId}>
          <Subtitle2 as="h2" id={peopleId} className={styles.heading}>
            Saved people
          </Subtitle2>
          <Body1 className={styles.text}>{SAVED_PEOPLE_TEXT}</Body1>
          {load.view.people.length > 0 ? (
            <ul className={styles.list} aria-label="Saved people">
              {sortedWords(load.view.people).map((name) => (
                <li key={name} className={styles.word}>
                  <Body1>{name}</Body1>
                </li>
              ))}
            </ul>
          ) : (
            load.view.people_left_out_note === null && (
              <Caption1 className={styles.muted}>No saved people yet.</Caption1>
            )
          )}
          {load.view.people_left_out_note !== null && (
            <Caption1 className={styles.muted}>{load.view.people_left_out_note}</Caption1>
          )}
        </section>
      )}
    </div>
  );
}
