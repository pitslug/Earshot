// The Speakers panel (docs/ui-design.md "Meeting detail", T12.19): it slides in beside the
// transcript. Voices that need a name come first, each with a name field, Save (name it,
// link it to that person and remember the voice, in one step) and Ignore. Named voices (the
// user, linked and recognised ones) sit in a collapsed list where a wrong name or match can
// still be fixed: rename here, rename person, link, merge and forget voice; "This is me" and
// "This isn't me" in an in-room meeting. Ignored voices keep their label and can be named
// after all. T12.44: a typed name that is someone's says "Links to <name>" before Save; after
// "This is me" a separate person with the user's name is offered for merging into the user;
// "This isn't me" waits on a voice the user's voiceprint named. T12.77: "Not <name>" puts a
// voice named or recognised as the wrong person back as an ignored "Speaker N". T12.80: the
// name of a voice that needs one, and Link on a named voice, are one combobox that lists saved
// people as you type (any part of the name, case and accents ignored), tells persons with one
// name apart by how many meetings each is in, and offers "New person: <name>".

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
  Dialog,
  DialogActions,
  DialogBody,
  DialogContent,
  DialogSurface,
  DialogTitle,
  Field,
  Input,
  Menu,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MenuButton,
  MenuItem,
  MenuList,
  MenuPopover,
  MenuTrigger,
  Option,
  Popover,
  PopoverSurface,
  PopoverTrigger,
  Subtitle2,
  Tooltip,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { type KeyboardEvent, type ReactElement, type Ref, useRef, useState } from "react";

import { RECOGNISED_ME_COPY, type LinkChoice, type MeetingDetail, type PersonItem, type SpeakerCard } from "../ipc";
import { recognised, recordingsGone, sourceLabel, speakerGroups } from "./detail";
import { meetingsText, personNameKey, personsMatching, personsNamed } from "./personFilter";

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
  linkSurface: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
    width: "240px",
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
  /** Renames the speaker in this meeting only. */
  onRename(speaker: string, name: string): Promise<void>;
  /** Renames a person, and so their speakers in every meeting. */
  onRenamePerson(person: string, name: string): Promise<void>;
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
  onMerge(keep: string, merge: string): Promise<void>;
  onForget(person: string): Promise<void>;
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

/** What "Not <name>" does (T12.77), for its tooltip. Until T12.77b, a voice saved for the
 * person stays in their saved voice, so the tooltip says so; `forgetShown`: the row shows
 * Forget voice (the person has a saved voice), which deletes that saved voice only. */
export function notThisPersonHint(name: string, forgetShown: boolean): string {
  const forget = forgetShown ? ` Forget voice deletes ${name}'s saved voice.` : "";
  return `Shows this voice as an unnamed speaker in this meeting and stops asking for its name. Later meetings may still name it ${name}, because ${name}'s saved voice may hold it.${forget}`;
}

/** Why the Speakers panel's edits wait while the meeting finishes. */
export const SPEAKERS_LOCKED_NOTE =
  "Name voices when the summary is done: the final transcript may change who is who.";

/** The Speakers panel. */
export function SpeakersPanel(props: SpeakersPanelProps) {
  const styles = useStyles();
  const { detail } = props;
  const { unnamed, named, ignored } = speakerGroups(detail);
  return (
    <aside id={props.id} className={styles.panel} aria-label="Speakers">
      <div className={styles.top}>
        <Subtitle2 as="h2" className={styles.heading}>
          Speakers
        </Subtitle2>
        <Button size="small" appearance="subtle" aria-label="Close Speakers" onClick={props.onClose}>
          Close
        </Button>
      </div>
      {props.locked && (
        <MessageBar intent="info" layout="multiline">
          <MessageBarBody>{SPEAKERS_LOCKED_NOTE}</MessageBarBody>
        </MessageBar>
      )}
      {props.mergeOffer && (
        <MessageBar intent="info" layout="multiline" aria-label="Merge into you">
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
                <NamedRow key={s.id} card={s} {...props} />
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

function MergeMenu({ card, detail, onMerge, locked }: RowProps) {
  const others = detail.speakers.filter((s) => s.id !== card.id);
  if (card.kind === "Me") {
    return (
      <Tooltip
        content="Your own row cannot be merged away. Merge the other speaker into it."
        relationship="description"
      >
        <Button size="small" appearance="subtle" disabledFocusable>
          Merge
        </Button>
      </Tooltip>
    );
  }
  return (
    <Menu>
      <MenuTrigger disableButtonEnhancement>
        <MenuButton size="small" appearance="subtle" disabled={others.length === 0 || locked}>
          Merge
        </MenuButton>
      </MenuTrigger>
      <MenuPopover>
        <MenuList>
          {others.map((o) => (
            <MenuItem key={o.id} onClick={() => void onMerge(o.id, card.id)}>
              Into {o.name}
            </MenuItem>
          ))}
        </MenuList>
      </MenuPopover>
    </Menu>
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
  const { card, persons, onSave, onIgnore, nameRef, locked = false } = props;
  const [saving, setSaving] = useState(false);
  // Saving makes a person (or links the one chosen or named) and remembers the voice, so only
  // on Save or Enter: leaving the field keeps the draft.
  const save = async (pick: PersonPick) => {
    if (saving || locked) return;
    const target = linkOf(pick);
    if (target === null) return;
    setSaving(true);
    try {
      await onSave(card.id, target.choice, target.name);
    } finally {
      setSaving(false);
    }
  };
  const field = usePersonField({
    card,
    persons,
    label: `Name of ${card.name}`,
    placeholder: `Name ${card.name.toLowerCase().startsWith("speaker") ? card.name.toLowerCase() : "this voice"}`,
    disabled: locked,
    inputRef: nameRef,
    onEnter: (pick) => void save(pick),
  });
  return (
    <li className={styles.row} aria-label={card.name}>
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
        <MergeMenu {...props} />
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
    <li className={styles.row} aria-label={card.name}>
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

/** A voice with a name: the user, a linked voice or a recognised one. */
function NamedRow(props: RowProps) {
  const styles = useStyles();
  const {
    card,
    persons,
    onRename,
    onLink,
    onUnmarkMe,
    onNotThisPerson,
    onForget,
    onRenamePerson,
    locked,
    linesLocked,
  } = props;
  // Which rename is open: this meeting's row, or the person in every meeting.
  const [editing, setEditing] = useState<"row" | "person" | null>(null);
  const [forgetting, setForgetting] = useState(false);
  const placeholder = `Name ${card.name.toLowerCase().startsWith("speaker") ? card.name.toLowerCase() : "this voice"}`;

  const rename = async (value: string) => {
    const n = value.trim();
    const kind = editing;
    setEditing(null);
    if (n === "") return;
    if (kind === "person" && card.person !== null) {
      if (n !== card.person.name) await onRenamePerson(card.person.id, n);
    } else if (n !== card.name) {
      await onRename(card.id, n);
    }
  };

  return (
    <li className={styles.row} aria-label={card.name}>
      <div className={styles.nameLine}>
        <PlayButton {...props} />
        {editing !== null ? (
          <NameInput
            initial={editing === "person" && card.person ? card.person.name : card.name}
            placeholder={placeholder}
            label={
              editing === "person"
                ? `New name of ${card.person?.name ?? card.name} in every meeting`
                : `Name of ${card.name} in this meeting`
            }
            onCommit={rename}
          />
        ) : (
          <Body1Strong className={styles.name}>
            {card.kind === "Me" && card.person && card.person.name !== card.name
              ? `${card.name} (${card.person.name})`
              : card.name}
          </Body1Strong>
        )}
        {recognised(card) && (
          <Badge appearance="tint" color="success" size="small">
            Recognised
          </Badge>
        )}
      </div>
      <RowFacts {...props} />
      {editing === "person" && (
        <Caption1 className={styles.muted}>Renames this person in every meeting.</Caption1>
      )}
      {editing === "row" && (
        <Caption1 className={styles.muted}>Renames this speaker in this meeting only.</Caption1>
      )}
      <div className={styles.actions}>
        <MergeMenu {...props} />
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
        <LinkPopover card={card} persons={persons} onLink={onLink} locked={locked} />
        {card.can_not_this_person && card.person !== null && editing === null && (
          <Tooltip
            content={notThisPersonHint(card.person.name, card.person.has_voiceprint)}
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
        {card.kind === "Other" && editing === null && (
          <Button size="small" appearance="subtle" disabled={locked} onClick={() => setEditing("row")}>
            Rename here
          </Button>
        )}
        {card.kind === "Other" && card.person !== null && editing === null && (
          <Button
            size="small"
            appearance="subtle"
            disabled={locked}
            onClick={() => setEditing("person")}
          >
            Rename person
          </Button>
        )}
        {card.kind === "Other" && card.person?.has_voiceprint && (
          <Button size="small" appearance="subtle" disabled={locked} onClick={() => setForgetting(true)}>
            Forget voice
          </Button>
        )}
      </div>
      {card.person !== null && (
        <Dialog modalType="alert" open={forgetting} onOpenChange={(_, d) => setForgetting(d.open)}>
          <DialogSurface>
            <DialogBody>
              <DialogTitle>Forget {card.person.name}&apos;s voice?</DialogTitle>
              <DialogContent>
                Earshot deletes the voiceprint it keeps for {card.person.name}. Later meetings
                will not recognise this voice until you remember it again. Names in this meeting
                stay as they are.
              </DialogContent>
              <DialogActions>
                <Button
                  appearance="primary"
                  onClick={() => {
                    setForgetting(false);
                    if (card.person) void onForget(card.person.id);
                  }}
                >
                  Forget voice
                </Button>
                <Button onClick={() => setForgetting(false)}>Cancel</Button>
              </DialogActions>
            </DialogBody>
          </DialogSurface>
        </Dialog>
      )}
    </li>
  );
}

/** A rename field: Enter or leaving it commits once. */
function NameInput({
  initial,
  placeholder,
  label,
  onCommit,
}: {
  initial: string;
  placeholder: string;
  label: string;
  onCommit(value: string): Promise<void>;
}) {
  const styles = useStyles();
  const [value, setValue] = useState(initial);
  // Enter and then blur commit once.
  const sent = useRef<string | null>(null);
  const commit = () => {
    if (sent.current === value) return;
    sent.current = value;
    void onCommit(value);
  };
  return (
    <Input
      className={styles.nameInput}
      size="small"
      value={value}
      placeholder={placeholder}
      aria-label={label}
      autoFocus
      onChange={(_, d) => setValue(d.value)}
      onBlur={commit}
      onKeyDown={(e: KeyboardEvent<HTMLInputElement>) => {
        if (e.key === "Enter") commit();
      }}
    />
  );
}

/** The value of the "New person: <name>" option. */
const NEW_PERSON = "__new__";

/** What a person field's text and choice mean (T12.80). */
type PersonPick =
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

/** The keys that move through the list: only after one of them does Enter or Tab choose the
 * option it reached; the highlight a keystroke leaves never decides by itself. */
const MOVE_KEYS = new Set(["ArrowDown", "ArrowUp", "Home", "End", "PageUp", "PageDown"]);

/** Starts every option's `text`, so no typed text ever starts an option's text. Fluent then
 * finds no option to highlight from the typed text (it would match the options of the render
 * before the keystroke, a stale list) and, once the new options render, highlights the first
 * one: by this field's order, the option Enter takes (the one person with the whole typed name,
 * else "New person"). Where Enter takes no option the highlight is turned off (`disableAutoFocus`).
 * The input never shows `text`: its value is controlled here. */
const OPTION_TEXT_PREFIX = "⁣";

/** The person combobox of a voice (T12.80): typing filters saved people by any part of the name,
 * case and accents ignored; each is listed with how many meetings they are in, the user
 * disabled (only the user's own row can be them), and "New person: <name>" for the typed name.
 * "New person: <name>" is offered only for a name no one other than the user has, so a click
 * never makes a second person with a saved name. Choosing an option (a click, or Enter or Tab
 * after moving through the list) picks it and shows what Save will do, saving nothing (Save is
 * the consent to remember the voice, T12.19); else the whole typed name decides: the one person
 * with it, several (the user must choose), or a new person. `onEnter` gets the pick of an Enter
 * that did not just choose an option. */
function usePersonField({
  card,
  persons,
  label,
  placeholder,
  disabled,
  inputRef,
  onEnter,
}: {
  card: SpeakerCard;
  persons: PersonItem[];
  label: string;
  placeholder: string;
  disabled: boolean;
  inputRef?: Ref<HTMLInputElement>;
  onEnter(pick: PersonPick): void;
}): { element: ReactElement; pick: PersonPick; hint: ReactElement | null } {
  const styles = useStyles();
  const [value, setValue] = useState("");
  // The chosen option, in state for rendering and in a ref for the Enter that chose it (Fluent
  // selects before this field's own Enter handler runs, in the same event).
  const [chosen, setChosen] = useState<PersonPick | null>(null);
  const chosenNow = useRef<PersonPick | null>(null);
  const navigated = useRef(false);
  // An Enter that chose an option (after moving through the list) chooses only.
  const choseOnEnter = useRef(false);
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

  const choose = (choice: PersonPick | null) => {
    chosenNow.current = choice;
    setChosen(choice);
  };

  // The user's own name makes a separate person (only the meeting's own row can be the user,
  // and the runtime refuses linking any other voice to them).
  const ownName = !own && pick.kind === "new" ? userNamed(pick.name, persons) : null;
  const hintText =
    pick.kind === "existing"
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
  // option is the person Enter links to (`OPTION_TEXT_PREFIX`).
  const named = own ? [] : personsNamed(typed, persons);
  const listed = personsMatching(typed, own ? persons.filter((p) => p.is_me) : persons);
  const ordered = [...named, ...listed.filter((p) => !named.includes(p))];
  const personOptions = ordered.map((p) => {
    const user = p.is_me && !own;
    return (
      <Option key={p.id} value={p.id} text={OPTION_TEXT_PREFIX + p.name} disabled={user}>
        {user ? `${p.name} (you)` : `${p.name} (${meetingsText(p.meetings)})`}
      </Option>
    );
  });
  const newOption =
    !own && typed !== "" && named.length === 0 ? (
      <Option key={NEW_PERSON} value={NEW_PERSON} text={OPTION_TEXT_PREFIX + typed}>
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
      aria-label={label}
      aria-describedby={hintText ? hintId : undefined}
      disabled={disabled}
      ref={inputRef}
      onChange={(e) => {
        setValue(e.target.value);
        choose(null);
        navigated.current = false;
      }}
      onOptionSelect={(e, d) => {
        if (d.optionValue === undefined) return;
        if (e.type === "keydown" && !navigated.current) return;
        navigated.current = false;
        if (e.type === "keydown" && "key" in e && e.key === "Enter") choseOnEnter.current = true;
        if (d.optionValue === NEW_PERSON) {
          if (typed !== "") choose({ kind: "new", name: typed });
          return;
        }
        const person = persons.find((p) => p.id === d.optionValue);
        if (!person || (person.is_me && !own)) return;
        choose({ kind: "existing", person });
        setValue(person.name);
      }}
      onKeyDown={(e: KeyboardEvent<HTMLInputElement>) => {
        if (MOVE_KEYS.has(e.key)) navigated.current = true;
        if (e.key !== "Enter") return;
        if (choseOnEnter.current) {
          choseOnEnter.current = false;
          return;
        }
        onEnter(pickOf(chosenNow.current));
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

/** Link on a named voice: the same person combobox, then Link (links only, remembers nothing). */
function LinkPopover({
  card,
  persons,
  onLink,
  locked = false,
}: {
  card: SpeakerCard;
  persons: PersonItem[];
  onLink(speaker: string, choice: LinkChoice): Promise<void>;
  locked?: boolean;
}) {
  const [open, setOpen] = useState(false);
  return (
    <Popover open={open} onOpenChange={(_, d) => setOpen(d.open)} trapFocus>
      <PopoverTrigger disableButtonEnhancement>
        <Button size="small" appearance="subtle" disabled={locked}>
          Link to person
        </Button>
      </PopoverTrigger>
      <PopoverSurface aria-label={`Link ${card.name} to a person`}>
        {open && (
          <LinkForm
            card={card}
            persons={persons}
            onLink={(choice) => {
              setOpen(false);
              void onLink(card.id, choice);
            }}
          />
        )}
      </PopoverSurface>
    </Popover>
  );
}

/** The popover's form, mounted only while it is open, so each opening starts empty. */
function LinkForm({
  card,
  persons,
  onLink,
}: {
  card: SpeakerCard;
  persons: PersonItem[];
  onLink(choice: LinkChoice): void;
}) {
  const styles = useStyles();
  const link = (pick: PersonPick) => {
    const target = linkOf(pick);
    if (target !== null) onLink(target.choice);
  };
  const field = usePersonField({
    card,
    persons,
    label: "Person",
    placeholder: card.kind === "Me" ? "Your name" : "Type a name",
    disabled: false,
    onEnter: link,
  });
  return (
    <div className={styles.linkSurface}>
      <Field label="Person">{field.element}</Field>
      {field.hint}
      <div>
        <Button appearance="primary" size="small" disabled={linkOf(field.pick) === null} onClick={() => link(field.pick)}>
          Link
        </Button>
      </div>
    </div>
  );
}
