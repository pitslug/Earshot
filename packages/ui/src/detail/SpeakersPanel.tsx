// The Speakers panel (docs/ui-design.md "Meeting detail", T12.19): it slides in beside the
// transcript. Voices that need a name come first, each with a name field, Save (name it,
// link it to that person and remember the voice, in one step) and Ignore. Named voices (the
// user, linked and recognised ones) sit in a collapsed list where a wrong name or match can
// still be fixed: the pencil changes the name and "Not <name>" unlinks it; "This is me" and
// "This isn't me" in an in-room meeting. Ignored voices keep their label and can be named
// after all. T12.44: a typed name that is someone's says "Links to <name>" before Save; after
// "This is me" a separate person with the user's name is offered for merging into the user;
// "This isn't me" waits on a voice the user's voiceprint named. T12.77: "Not <name>" puts a
// voice named or recognised as the wrong person back as an ignored "Speaker N". T12.80: the
// name field is one combobox that lists saved people as you type (any part of the name, case
// and accents ignored), tells persons with one name apart by how many meetings each is in, and
// offers "New person: <name>". T12.104: a named card holds only its name, the pencil and "Not
// <name>"; naming a voice after a person who already has another card here makes the two
// voices one (a merge), and "Manage people" opens Settings on the People page.

import {
  Accordion,
  AccordionHeader,
  AccordionItem,
  AccordionPanel,
  Badge,
  Body1Strong,
  Button,
  Caption1,
  Combobox,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  Option,
  Subtitle2,
  Tooltip,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { type KeyboardEvent, type ReactElement, type Ref, useEffect, useRef, useState } from "react";

import { RECOGNISED_ME_COPY, type LinkChoice, type MeetingDetail, type PersonItem, type SpeakerCard } from "../ipc";
import { recognised, recordingsGone, sourceLabel, speakerGroups } from "./detail";
import { meetingsText, personNameKey, personsMatching, personsNamed } from "./personFilter";
import { notThisPersonSavedHint } from "./voiceSampleCopy";

const useStyles = makeStyles({
  panel: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
    padding: tokens.spacingHorizontalM,
    borderLeft: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
    backgroundColor: tokens.colorNeutralBackground2,
    minWidth: 0,
  },
  top: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    gap: tokens.spacingHorizontalS,
  },
  heading: {
    margin: 0,
  },
  subheading: {
    margin: 0,
  },
  list: {
    listStyleType: "none",
    margin: 0,
    padding: 0,
  },
  row: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
    paddingTop: tokens.spacingVerticalS,
    paddingBottom: tokens.spacingVerticalS,
    borderTop: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
    minWidth: 0,
  },
  nameLine: {
    display: "flex",
    alignItems: "center",
    gap: tokens.spacingHorizontalXS,
    minWidth: 0,
  },
  name: {
    overflowWrap: "anywhere",
    minWidth: 0,
    flexGrow: 1,
  },
  nameInput: {
    flexGrow: 1,
    minWidth: 0,
  },
  muted: {
    color: tokens.colorNeutralForeground3,
    overflowWrap: "anywhere",
  },
  actions: {
    display: "flex",
    flexWrap: "wrap",
    columnGap: tokens.spacingHorizontalXS,
  },
  count: {
    color: tokens.colorNeutralForeground4,
  },
  // Read by assistive technology, not drawn (the usual clip pattern).
  srOnly: {
    position: "absolute",
    width: "1px",
    height: "1px",
    margin: "-1px",
    padding: 0,
    overflow: "hidden",
    clip: "rect(0 0 0 0)",
    whiteSpace: "nowrap",
    border: 0,
  },
  manage: {
    alignSelf: "flex-start",
  },
  section: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
  },
  accordionPanel: {
    marginLeft: 0,
    marginRight: 0,
  },
});

export interface SpeakersPanelProps {
  /** The panel's element id (the header button's `aria-controls`). */
  id: string;
  detail: MeetingDetail;
  persons: PersonItem[];
  /** Speaker ids whose sample audio is no longer kept. */
  noAudio: ReadonlySet<string>;
  /** Closes the panel. */
  onClose(): void;
  onPlay(speaker: string): void;
  /** Links a named voice to the person its changed name means (links only, remembers nothing). */
  onLink(speaker: string, choice: LinkChoice): Promise<void>;
  /** "Save": names the voice, links it to that person and remembers the voice (`name`: the
   * name it is saved as). */
  onSave(speaker: string, choice: LinkChoice, name: string): Promise<void>;
  /** "Ignore": the voice no longer needs a name. */
  onIgnore(speaker: string): Promise<void>;
  /** Undoes "Ignore". */
  onUnignore(speaker: string): Promise<void>;
  /** "Not <name>": the voice is not that person; back to an ignored "Speaker N". */
  onNotThisPerson(speaker: string): Promise<void>;
  /** Says this voice is the user's, in this meeting. */
  onMarkMe(speaker: string): Promise<void>;
  /** Puts the user's row back as a voice that needs a name. */
  onUnmarkMe(speaker: string): Promise<void>;
  /** Makes two voices one: `merge` joins `keep` (a name chosen for a voice whose person already
   * has another card here, [`mergeTarget`]). */
  onMerge(keep: string, merge: string): Promise<void>;
  /** "Manage people": opens Settings on the People page. */
  onManagePeople?(): void;
  /** Called with the name field of the first voice that needs a name (for "Name voices"). */
  firstNameField?: (input: HTMLInputElement | null) => void;
  /** The meeting is still finishing (T12.28): the voices can be seen and heard, but every edit
   * waits, because the final pass may change who is who and would drop it. */
  locked?: boolean;
  /** The meeting is busy as line edits see it (recording, finishing, or a job on it): the runtime
   * refuses "Not <name>" then (it holds the line-edit claim), so it waits too. */
  linesLocked?: boolean;
  /** A separate person named after the user, offered for merging into the user after "This is
   * me" (T12.44); `null` or absent when there is none. */
  mergeOffer?: PersonItem | null;
  /** Merges the offered person into the user. */
  onMergeIntoMe?(person: string): Promise<void>;
  /** Declines the offer: the person stays separate. */
  onKeepSeparate?(): void;
}

/** The user's person when `name` is the user's own name (not the default "Me"), case and
 * accents ignored as the person filter compares names; else null. */
export function userNamed(name: string, persons: PersonItem[]): PersonItem | null {
  const user = persons.find((p) => p.is_me);
  if (!user || user.name.trim() === "Me") return null;
  return personNameKey(user.name) === personNameKey(name) ? user : null;
}

/** Why typing the user's own name does not link the voice to them (T12.44). */
export function ownNameHint(user: PersonItem, canMarkMe: boolean): string {
  return canMarkMe
    ? `This is your name. Save makes a separate person called ${user.name}. To show this voice as you, choose "This is me".`
    : `This is your name. Save makes a separate person called ${user.name}, because only your own row can be you.`;
}

/** The offer after "This is me" when a separate person has the user's name (T12.44). */
export function mergeOfferText(person: PersonItem): string {
  const voice = person.has_voiceprint ? " Earshot will then recognise you by the voice saved for them." : "";
  return `There is also a person called ${person.name}. If that is you, merge them into you.${voice}`;
}

/** What "Not <name>" does (T12.77), for its tooltip. `savedHere`: this meeting's voice is saved
 * as a sample of `name`'s voice, so "Not" takes it out (T12.77b7, the agreed copy); otherwise
 * `name`'s saved voice may still hold it. */
export function notThisPersonHint(name: string, savedHere = false): string {
  if (savedHere) return notThisPersonSavedHint(name);
  return `Shows this voice as an unnamed speaker in this meeting and stops asking for its name. Later meetings may still name it ${name}, because ${name}'s saved voice may hold it.`;
}

/** The other card a name makes this voice one with (T12.104): when the person `pick` links to
 * already has another card in this meeting, saving merges this voice into that card instead of
 * linking it. Never for the user's own row, which cannot be merged away (the runtime refuses);
 * it links as before. The other card may be the user's row. */
export function mergeTarget(card: SpeakerCard, pick: PersonPick, speakers: SpeakerCard[]): SpeakerCard | null {
  if (card.kind === "Me" || pick.kind !== "existing") return null;
  return speakers.find((s) => s.id !== card.id && s.person?.id === pick.person.id) ?? null;
}

/** The line under a name field while the chosen person already has another card here. */
export function becomeOneText(person: string, other: string): string {
  return other === person
    ? `${person} already has a voice in this meeting: the two become one.`
    : `${person} is already ${other} here: the two voices become one.`;
}

/** Why the Speakers panel's edits wait while the meeting finishes. */
export const SPEAKERS_LOCKED_NOTE =
  "Name voices when the summary is done: the final transcript may change who is who.";

/** The Speakers panel. */
export function SpeakersPanel(props: SpeakersPanelProps) {
  const styles = useStyles();
  const { detail } = props;
  const { unnamed, named, ignored } = speakerGroups(detail);
  const panel = useRef<HTMLElement | null>(null);
  // After two voices become one the merged card is gone: focus goes to the card kept once the
  // panel shows it, else (its list is folded) to the panel's heading.
  const [focusAfterMerge, setFocusAfterMerge] = useState<string | null>(null);
  useEffect(() => {
    if (focusAfterMerge === null || panel.current === null) return;
    const kept = panel.current.querySelector<HTMLElement>(`[data-card="${focusAfterMerge}"]`);
    const heading = panel.current.querySelector<HTMLElement>("h2");
    (kept ?? heading)?.focus();
    setFocusAfterMerge(null);
  });
  const onMerge = async (keep: string, merge: string) => {
    await props.onMerge(keep, merge);
    setFocusAfterMerge(keep);
  };
  return (
    <aside id={props.id} ref={panel} className={styles.panel} aria-label="Speakers">
      <div className={styles.top}>
        <Subtitle2 as="h2" tabIndex={-1} className={styles.heading}>
          Speakers
        </Subtitle2>
        <Button size="small" appearance="subtle" aria-label="Close Speakers" onClick={props.onClose}>
          Close
        </Button>
      </div>
      {props.locked && (
        <MessageBar intent="info">
          <MessageBarBody>{SPEAKERS_LOCKED_NOTE}</MessageBarBody>
        </MessageBar>
      )}
      {props.mergeOffer && (
        <MessageBar intent="info" aria-label="Merge into you">
          <MessageBarBody>{mergeOfferText(props.mergeOffer)}</MessageBarBody>
          <MessageBarActions>
            <Button
              size="small"
              appearance="primary"
              disabled={props.locked}
              onClick={() => {
                if (props.mergeOffer) void props.onMergeIntoMe?.(props.mergeOffer.id);
              }}
            >
              Merge into me
            </Button>
            <Button size="small" onClick={() => props.onKeepSeparate?.()}>
              Keep separate
            </Button>
          </MessageBarActions>
        </MessageBar>
      )}
      {unnamed.length > 0 ? (
        <section className={styles.section} aria-label="Need a name">
          <Body1Strong as="h3" className={styles.subheading}>
            Need a name
          </Body1Strong>
          <Caption1 className={styles.muted}>
            Save names a voice and remembers it, so later meetings name it too. Names flow into
            the summary and export.
          </Caption1>
          <ul className={styles.list}>
            {unnamed.map((s, i) => (
              <UnnamedRow
                key={s.id}
                card={s}
                {...props}
                onMerge={onMerge}
                nameRef={i === 0 ? props.firstNameField : undefined}
              />
            ))}
          </ul>
        </section>
      ) : (
        <Caption1 className={styles.muted}>Every voice in this meeting has a name.</Caption1>
      )}
      <Accordion collapsible multiple>
        <AccordionItem value="named">
          <AccordionHeader size="small">Named ({named.length})</AccordionHeader>
          <AccordionPanel className={styles.accordionPanel}>
            <ul className={styles.list} aria-label="Named">
              {named.map((s) => (
                <NamedRow key={s.id} card={s} {...props} onMerge={onMerge} />
              ))}
            </ul>
          </AccordionPanel>
        </AccordionItem>
        {ignored.length > 0 && (
          <AccordionItem value="ignored">
            <AccordionHeader size="small">Ignored ({ignored.length})</AccordionHeader>
            <AccordionPanel className={styles.accordionPanel}>
              <ul className={styles.list} aria-label="Ignored">
                {ignored.map((s) => (
                  <IgnoredRow key={s.id} card={s} {...props} />
                ))}
              </ul>
            </AccordionPanel>
          </AccordionItem>
        )}
      </Accordion>
      <Caption1 className={styles.muted}>Your own voice is never saved here.</Caption1>
      {props.onManagePeople && (
        <Button
          size="small"
          appearance="transparent"
          className={styles.manage}
          onClick={() => props.onManagePeople?.()}
        >
          Manage people
        </Button>
      )}
    </aside>
  );
}

interface RowProps extends SpeakersPanelProps {
  card: SpeakerCard;
}

/** Play, where it came from, talk share, sample line and "Audio no longer kept". */
function RowFacts({ card, detail, noAudio }: RowProps) {
  const styles = useStyles();
  return (
    <>
      <Caption1 className={styles.muted}>
        {sourceLabel(card)} · {card.share_percent}% of talk time
      </Caption1>
      {card.sample_line !== "" && (
        <Caption1 className={styles.muted}>&quot;{card.sample_line}&quot;</Caption1>
      )}
      {(noAudio.has(card.id) || recordingsGone(detail)) && (
        <Caption1 className={styles.muted}>Audio no longer kept</Caption1>
      )}
    </>
  );
}

function PlayButton({ card, detail, onPlay }: RowProps) {
  return (
    <Button
      size="small"
      appearance="subtle"
      aria-label={`Play a sample of ${card.name}`}
      disabled={!detail.audio_kept}
      onClick={() => onPlay(card.id)}
    >
      Play
    </Button>
  );
}

function MarkMeButton({ card, onMarkMe, locked }: RowProps) {
  if (!card.can_mark_me) return null;
  return (
    <Tooltip
      content="Shows this voice as you in this meeting. No voiceprint is made from it."
      relationship="description"
    >
      <Button
        size="small"
        appearance="subtle"
        disabled={locked}
        onClick={() => void onMarkMe(card.id)}
      >
        This is me
      </Button>
    </Tooltip>
  );
}

/** A voice that needs a name: its name field with Save (Enter saves too) and Ignore. */
function UnnamedRow(props: RowProps & { nameRef?: (input: HTMLInputElement | null) => void }) {
  const styles = useStyles();
  const { card, persons, onSave, onMerge, onIgnore, nameRef, locked = false } = props;
  const [saving, setSaving] = useState(false);
  // Saving makes a person (or links the one chosen or named) and remembers the voice, so only
  // on Save or Enter: leaving the field keeps the draft.
  // A person who already has another card here makes the two voices one instead (T12.104).
  const save = async (pick: PersonPick) => {
    if (saving || locked) return;
    const target = linkOf(pick);
    if (target === null) return;
    const other = mergeTarget(card, pick, props.detail.speakers);
    setSaving(true);
    try {
      if (other !== null) await onMerge(other.id, card.id);
      else await onSave(card.id, target.choice, target.name);
    } finally {
      setSaving(false);
    }
  };
  const field = usePersonField({
    card,
    persons,
    speakers: props.detail.speakers,
    label: `Name of ${card.name}`,
    placeholder: `Name ${card.name.toLowerCase().startsWith("speaker") ? card.name.toLowerCase() : "this voice"}`,
    disabled: locked,
    inputRef: nameRef,
    onEnter: (pick) => void save(pick),
  });
  return (
    <li className={styles.row} aria-label={card.name} data-card={card.id} tabIndex={-1}>
      <div className={styles.nameLine}>
        <PlayButton {...props} />
        {field.element}
        <Button
          size="small"
          appearance="primary"
          aria-label={`Save ${card.name}`}
          disabled={linkOf(field.pick) === null || saving || locked}
          onClick={() => void save(field.pick)}
        >
          Save
        </Button>
      </div>
      {field.hint}
      <RowFacts {...props} />
      <div className={styles.actions}>
        {card.can_ignore && (
          <Tooltip
            content="Keeps this voice as it is labelled and stops asking for a name. Nothing is remembered."
            relationship="description"
          >
            <Button
              size="small"
              appearance="subtle"
              disabled={locked}
              onClick={() => void onIgnore(card.id)}
            >
              Ignore
            </Button>
          </Tooltip>
        )}
        <MarkMeButton {...props} />
      </div>
    </li>
  );
}

/** A voice the user ignored: its label, and the way back. */
function IgnoredRow(props: RowProps) {
  const styles = useStyles();
  const { card, onUnignore, locked } = props;
  return (
    <li className={styles.row} aria-label={card.name} data-card={card.id} tabIndex={-1}>
      <div className={styles.nameLine}>
        <PlayButton {...props} />
        <Body1Strong className={styles.name}>{card.name}</Body1Strong>
      </div>
      <RowFacts {...props} />
      <div className={styles.actions}>
        <Button
          size="small"
          appearance="subtle"
          disabled={locked}
          onClick={() => void onUnignore(card.id)}
        >
          Stop ignoring
        </Button>
      </div>
    </li>
  );
}

/** A pencil, for "Change name of <name>". */
function PencilIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
      <path d="M10.5 2.5l3 3-8 8H2.5v-3l8-8zM9 4l3 3" />
    </svg>
  );
}

/** A voice with a name: the user, a linked voice or a recognised one. Its name, the pencil that
 * changes it (not on the user's own row) and "Not <name>"; in the room "This is me" and "This
 * isn't me" (T12.104). */
function NamedRow(props: RowProps) {
  const styles = useStyles();
  const { card, onUnmarkMe, onNotThisPerson, locked, linesLocked } = props;
  const [editing, setEditing] = useState(false);
  const pencil = useRef<HTMLButtonElement | null>(null);
  // Set when the field closes: the pencil it brought back takes focus once drawn.
  const refocus = useRef(false);
  const pencilRef = (el: HTMLButtonElement | null) => {
    pencil.current = el;
    if (el !== null && refocus.current) {
      refocus.current = false;
      el.focus();
    }
  };
  const close = () => {
    refocus.current = true;
    setEditing(false);
  };

  return (
    <li className={styles.row} aria-label={card.name} data-card={card.id} tabIndex={-1}>
      {editing ? (
        <NameChange {...props} onDone={close} />
      ) : (
        <div className={styles.nameLine}>
          <PlayButton {...props} />
          <Body1Strong className={styles.name}>
            {card.kind === "Me" && card.person && card.person.name !== card.name
              ? `${card.name} (${card.person.name})`
              : card.name}
          </Body1Strong>
          {recognised(card) && (
            <Badge appearance="tint" color="success" size="small">
              Recognised
            </Badge>
          )}
          {card.kind !== "Me" && (
            <Button
              ref={pencilRef}
              size="small"
              appearance="subtle"
              icon={<PencilIcon />}
              aria-label={`Change name of ${card.name}`}
              disabled={locked}
              onClick={() => setEditing(true)}
            />
          )}
        </div>
      )}
      <RowFacts {...props} />
      <div className={styles.actions}>
        <MarkMeButton {...props} />
        {card.can_unmark_me && (
          <Button
            size="small"
            appearance="subtle"
            disabled={locked}
            onClick={() => void onUnmarkMe(card.id)}
          >
            This isn&apos;t me
          </Button>
        )}
        {card.recognised_me && (
          <Tooltip content={RECOGNISED_ME_COPY} relationship="description">
            <Button size="small" appearance="subtle" disabledFocusable>
              This isn&apos;t me
            </Button>
          </Tooltip>
        )}
        {card.can_not_this_person && card.person !== null && !editing && (
          <Tooltip
            content={notThisPersonHint(card.person.name, card.voice_saved_here)}
            relationship="description"
          >
            <Button
              size="small"
              appearance="subtle"
              disabled={locked || linesLocked}
              onClick={() => void onNotThisPerson(card.id)}
            >
              Not {card.person.name}
            </Button>
          </Tooltip>
        )}
      </div>
    </li>
  );
}

/** The pencil's field, mounted only while it is open, so each opening starts from the name:
 * the same person field as a voice that needs a name, then Save (links, remembering nothing,
 * as Link did) or, for a person with another card here, the merge into that card. Escape or
 * Cancel closes it unchanged. */
function NameChange(props: RowProps & { onDone(): void }) {
  const styles = useStyles();
  const { card, persons, detail, onLink, onMerge, onDone, locked = false } = props;
  const [saving, setSaving] = useState(false);
  const unchanged = (pick: PersonPick) => pick.kind === "existing" && pick.person.id === card.person?.id;
  const save = async (pick: PersonPick) => {
    if (saving || locked) return;
    if (unchanged(pick)) {
      onDone();
      return;
    }
    const target = linkOf(pick);
    if (target === null) return;
    const other = mergeTarget(card, pick, detail.speakers);
    setSaving(true);
    try {
      if (other !== null) await onMerge(other.id, card.id);
      else await onLink(card.id, target.choice);
    } finally {
      setSaving(false);
    }
    onDone();
  };
  const field = usePersonField({
    card,
    persons,
    speakers: detail.speakers,
    initial: card.person?.name ?? card.name,
    label: `Name of ${card.name}`,
    placeholder: "Type a name",
    disabled: locked,
    autoFocus: true,
    onEnter: (pick) => void save(pick),
    onEscape: onDone,
  });
  return (
    <>
      <div className={styles.nameLine}>
        <PlayButton {...props} />
        {field.element}
      </div>
      {field.hint}
      <div className={styles.actions}>
        <Button
          size="small"
          appearance="primary"
          aria-label={`Save name of ${card.name}`}
          disabled={linkOf(field.pick) === null || unchanged(field.pick) || saving || locked}
          onClick={() => void save(field.pick)}
        >
          Save
        </Button>
        <Button size="small" appearance="subtle" onClick={onDone}>
          Cancel
        </Button>
      </div>
    </>
  );
}

/** The value of the "New person: <name>" option. */
const NEW_PERSON = "__new__";

/** What a person field's text and choice mean (T12.80). */
export type PersonPick =
  /** Nothing typed, or (on the user's own row) a name that is not the user's. */
  | { kind: "none" }
  /** A saved person: chosen from the list, or the only one with the whole typed name. */
  | { kind: "existing"; person: PersonItem }
  /** A new person with this name: chosen, or a name no one other than the user has. */
  | { kind: "new"; name: string }
  /** Several people have the whole typed name and none was chosen. */
  | { kind: "several"; name: string; count: number };

/** The link a pick makes and the name it is saved as; null when there is nothing to link. */
function linkOf(pick: PersonPick): { choice: LinkChoice; name: string } | null {
  switch (pick.kind) {
    case "existing":
      return { choice: { kind: "Existing", person: pick.person.id }, name: pick.person.name };
    case "new":
      return { choice: { kind: "New", name: pick.name }, name: pick.name };
    default:
      return null;
  }
}

/** Starts every option's `text`, so no typed text ever starts an option's text. Fluent then
 * finds no option to highlight from the typed text (it would match the options of the render
 * before the keystroke, a stale list) and, once the new options render, highlights the first
 * one: by this field's order, the option the typed name means (the one person with the whole
 * typed name, else "New person"). Where the typed name means no option the highlight is turned
 * off (`disableAutoFocus`). The input never shows `text`: its value is controlled here. */
const OPTION_TEXT_PREFIX = "⁣";

/** The attribute naming an option's value on its element, read back by [`keyAction`]. */
const OPTION_VALUE_ATTRIBUTE = "data-person-option";

/** Whether two picks link the same way. */
function samePick(a: PersonPick, b: PersonPick): boolean {
  if (a.kind === "existing" && b.kind === "existing") return a.person.id === b.person.id;
  if (a.kind === "new" && b.kind === "new") return a.name === b.name;
  return false;
}

/** What Enter or Tab does in a person field: the one rule, decided from the option highlighted
 * in the open list as the user saw it, never from key history.
 * - A highlighted disabled option (the user, on another voice's row): nothing.
 * - A highlighted option that is not what the hint says: choose it (the hint then says it);
 *   nothing is saved, because Save is the consent to remember the voice (T12.19).
 * - Otherwise (no highlighted option, or the one the hint already says): Enter does what the
 *   hint says (link to the one person, or a new person); several people or no name: nothing.
 *   Tab never saves. */
export function keyAction(
  key: "Enter" | "Tab",
  highlighted: { disabled: true } | { disabled: false; pick: PersonPick } | null,
  pick: PersonPick,
): { kind: "nothing" } | { kind: "choose"; pick: PersonPick } | { kind: "save"; pick: PersonPick } {
  if (highlighted?.disabled) return { kind: "nothing" };
  if (highlighted && !samePick(highlighted.pick, pick)) return { kind: "choose", pick: highlighted.pick };
  if (key === "Enter" && linkOf(pick) !== null) return { kind: "save", pick };
  return { kind: "nothing" };
}

/** The person combobox of a voice (T12.80): typing filters saved people by any part of the name,
 * case and accents ignored; each is listed with how many meetings they are in, the user
 * disabled (only the user's own row can be them), and "New person: <name>" for a name no one
 * other than the user has, so a click never makes a second person with a saved name. A click on
 * an option, or Enter or Tab on a highlighted option the hint does not already say, chooses it
 * and shows what Save will do; else the whole typed name decides: the one person with it,
 * several (the user must choose), or a new person ([`keyAction`]). `onEnter` gets the pick an
 * Enter saves; `onEscape`, when given, is Escape with the list closed. A person who already has
 * another of `speakers` here says the two voices become one ([`mergeTarget`]). */
function usePersonField({
  card,
  persons,
  speakers,
  initial = "",
  label,
  placeholder,
  disabled,
  autoFocus = false,
  inputRef,
  onEnter,
  onEscape,
}: {
  card: SpeakerCard;
  persons: PersonItem[];
  speakers: SpeakerCard[];
  initial?: string;
  label: string;
  placeholder: string;
  disabled: boolean;
  autoFocus?: boolean;
  inputRef?: Ref<HTMLInputElement>;
  onEnter(pick: PersonPick): void;
  onEscape?(): void;
}): { element: ReactElement; pick: PersonPick; hint: ReactElement | null } {
  const styles = useStyles();
  const [value, setValue] = useState(initial);
  const [chosen, setChosen] = useState<PersonPick | null>(null);
  const own = card.kind === "Me";
  const typed = value.trim();

  const pickOf = (choice: PersonPick | null): PersonPick => {
    if (choice !== null) return choice;
    if (typed === "") return { kind: "none" };
    if (own) {
      const user = persons.find((p) => p.is_me && personNameKey(p.name) === personNameKey(typed));
      return user ? { kind: "existing", person: user } : { kind: "none" };
    }
    const named = personsNamed(typed, persons);
    if (named.length === 1) return { kind: "existing", person: named[0] };
    if (named.length > 1) return { kind: "several", name: typed, count: named.length };
    return { kind: "new", name: typed };
  };
  const pick = pickOf(chosen);

  // The pick of an option's value; null for the user on another voice's row (disabled).
  const pickOfOption = (optionValue: string): PersonPick | null => {
    if (optionValue === NEW_PERSON) return typed === "" ? null : { kind: "new", name: typed };
    const person = persons.find((p) => p.id === optionValue);
    if (!person || (person.is_me && !own)) return null;
    return { kind: "existing", person };
  };
  const choose = (choice: PersonPick) => {
    setChosen(choice);
    if (choice.kind === "existing") setValue(choice.person.name);
  };

  // The user's own name makes a separate person (only the meeting's own row can be the user,
  // and the runtime refuses linking any other voice to them).
  const ownName = !own && pick.kind === "new" ? userNamed(pick.name, persons) : null;
  const other = mergeTarget(card, pick, speakers);
  const hintText =
    other !== null && pick.kind === "existing"
      ? becomeOneText(pick.person.name, other.name)
      : pick.kind === "existing"
      ? `Links to ${pick.person.name} (${meetingsText(pick.person.meetings)})`
      : pick.kind === "several"
        ? `${pick.count} people are called ${pick.name}. Choose one from the list.`
        : ownName
          ? ownNameHint(ownName, card.can_mark_me)
          : pick.kind === "new"
            ? `New person: ${pick.name}`
            : null;
  const hintId = `${card.id}-links-to`;

  // Persons with the whole typed name (case, accents and spaces ignored) first, so the first
  // option is the person the typed name means (`OPTION_TEXT_PREFIX`).
  const named = own ? [] : personsNamed(typed, persons);
  const listed = personsMatching(typed, own ? persons.filter((p) => p.is_me) : persons);
  const ordered = [...named, ...listed.filter((p) => !named.includes(p))];
  const personOptions = ordered.map((p) => {
    const user = p.is_me && !own;
    return (
      <Option
        key={p.id}
        value={p.id}
        text={OPTION_TEXT_PREFIX + p.name}
        disabled={user}
        {...{ [OPTION_VALUE_ATTRIBUTE]: p.id }}
      >
        {user ? (
          `${p.name} (you)`
        ) : (
          <>
            {`${p.name} `}
            {/* The count is short so a long name stays on one line; the whole words are read out. */}
            <span className={styles.count} aria-hidden="true">
              {`(${p.meetings})`}
            </span>
            <span className={styles.srOnly} data-sr-only="">
              {`(${meetingsText(p.meetings)})`}
            </span>
          </>
        )}
      </Option>
    );
  });
  const newOption =
    !own && typed !== "" && named.length === 0 ? (
      <Option
        key={NEW_PERSON}
        value={NEW_PERSON}
        text={OPTION_TEXT_PREFIX + typed}
        {...{ [OPTION_VALUE_ATTRIBUTE]: NEW_PERSON }}
      >
        {`New person: ${typed}`}
      </Option>
    ) : null;
  // A name someone has lists them first; any other name lists "New person" first.
  const options = newOption === null ? personOptions : [newOption, ...personOptions];
  const selected =
    pick.kind === "existing" && chosen !== null ? [pick.person.id] : pick.kind === "new" && chosen !== null ? [NEW_PERSON] : [];

  const element = (
    <Combobox
      className={styles.nameInput}
      size="small"
      freeform
      value={value}
      selectedOptions={selected}
      disableAutoFocus={pick.kind !== "existing" && pick.kind !== "new"}
      placeholder={placeholder}
      autoFocus={autoFocus}
      aria-label={label}
      aria-describedby={hintText ? hintId : undefined}
      disabled={disabled}
      ref={inputRef}
      onChange={(e) => {
        setValue(e.target.value);
        setChosen(null);
      }}
      onOptionSelect={(e, d) => {
        // Keys are decided by `keyAction` in `onKeyDown`; only a click chooses here.
        if (e.type === "keydown" || d.optionValue === undefined) return;
        const choice = pickOfOption(d.optionValue);
        if (choice !== null) choose(choice);
      }}
      onKeyDown={(e: KeyboardEvent<HTMLInputElement>) => {
        // Escape first closes an open list (Fluent); with the list closed it cancels.
        if (e.key === "Escape" && onEscape && e.currentTarget.getAttribute("aria-expanded") !== "true") {
          e.preventDefault();
          onEscape();
          return;
        }
        if ((e.key !== "Enter" && e.key !== "Tab") || e.nativeEvent.isComposing) return;
        // The list and highlight as last rendered, which is what the user saw: Fluent's own
        // handler has already run for this key, but nothing it changed is drawn yet.
        const input = e.currentTarget;
        const id = input.getAttribute("aria-expanded") === "true" ? input.getAttribute("aria-activedescendant") : null;
        const option = id ? input.ownerDocument.getElementById(id) : null;
        const optionValue = option?.getAttribute(OPTION_VALUE_ATTRIBUTE) ?? null;
        let highlighted: Parameters<typeof keyAction>[1] = null;
        if (option !== null && optionValue !== null) {
          const choice = option.getAttribute("aria-disabled") === "true" ? null : pickOfOption(optionValue);
          highlighted = choice === null ? { disabled: true } : { disabled: false, pick: choice };
        }
        const action = keyAction(e.key === "Enter" ? "Enter" : "Tab", highlighted, pick);
        if (action.kind === "choose") choose(action.pick);
        if (action.kind === "save") onEnter(action.pick);
      }}
    >
      {options}
    </Combobox>
  );
  const hint = hintText ? (
    <Caption1 id={hintId} className={styles.muted}>
      {hintText}
    </Caption1>
  ) : null;
  return { element, pick, hint };
}
