// The Speakers panel (docs/ui-design.md "Meeting detail", T12.19): it slides in beside the
// transcript. Voices that need a name come first, each with a name field, Save (name it,
// link it to that person and remember the voice, in one step) and Ignore. Named voices (the
// user, linked and recognised ones) sit in a collapsed list where a wrong name or match can
// still be fixed: rename here, rename person, link, merge and forget voice; "This is me" and
// "This isn't me" in an in-room meeting. Ignored voices keep their label and can be named
// after all.

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
import { type KeyboardEvent, useRef, useState } from "react";

import type { LinkChoice, MeetingDetail, PersonItem, SpeakerCard } from "../ipc";
import { linkablePersons, personNamed, recognised, recordingsGone, sourceLabel, speakerGroups } from "./detail";

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
  const [value, setValue] = useState("");
  const [saving, setSaving] = useState(false);
  const placeholder = `Name ${card.name.toLowerCase().startsWith("speaker") ? card.name.toLowerCase() : "this voice"}`;
  const typed = value.trim();
  // Saving makes a person (an existing one with exactly that name) and remembers the voice,
  // so only on Save or Enter: leaving the field keeps the draft.
  const save = async () => {
    if (typed === "" || saving || locked) return;
    const existing = personNamed(typed, persons);
    setSaving(true);
    try {
      await onSave(
        card.id,
        existing ? { kind: "Existing", person: existing.id } : { kind: "New", name: typed },
        existing ? existing.name : typed,
      );
    } finally {
      setSaving(false);
    }
  };
  return (
    <li className={styles.row} aria-label={card.name}>
      <div className={styles.nameLine}>
        <PlayButton {...props} />
        <Input
          className={styles.nameInput}
          size="small"
          value={value}
          placeholder={placeholder}
          aria-label={`Name of ${card.name}`}
          disabled={locked}
          ref={nameRef}
          onChange={(_, d) => setValue(d.value)}
          onKeyDown={(e: KeyboardEvent<HTMLInputElement>) => {
            if (e.key === "Enter") void save();
          }}
        />
        <Button
          size="small"
          appearance="primary"
          aria-label={`Save ${card.name}`}
          disabled={typed === "" || saving || locked}
          onClick={() => void save()}
        >
          Save
        </Button>
      </div>
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
  const { card, persons, onRename, onLink, onUnmarkMe, onForget, onRenamePerson, locked } = props;
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
        <LinkPopover card={card} persons={persons} onLink={onLink} locked={locked} />
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

const NEW_PERSON = "__new__";

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
  const styles = useStyles();
  const [open, setOpen] = useState(false);
  const [chosen, setChosen] = useState<string | null>(null);
  const [newName, setNewName] = useState("");
  const options = linkablePersons(card, persons);
  const canLink = chosen !== null && (chosen !== NEW_PERSON || newName.trim() !== "");
  const link = () => {
    if (chosen === null) return;
    const choice: LinkChoice =
      chosen === NEW_PERSON ? { kind: "New", name: newName.trim() } : { kind: "Existing", person: chosen };
    setOpen(false);
    void onLink(card.id, choice);
  };
  return (
    <Popover
      open={open}
      onOpenChange={(_, d) => {
        setOpen(d.open);
        if (d.open) {
          setChosen(null);
          setNewName("");
        }
      }}
      trapFocus
    >
      <PopoverTrigger disableButtonEnhancement>
        <Button size="small" appearance="subtle" disabled={locked}>
          Link to person
        </Button>
      </PopoverTrigger>
      <PopoverSurface aria-label={`Link ${card.name} to a person`}>
        <div className={styles.linkSurface}>
          <Field label="Person">
            <Combobox
              placeholder="Choose a person"
              onOptionSelect={(_, d) => setChosen(d.optionValue ?? null)}
            >
              {options.map((p) => (
                <Option key={p.id} value={p.id}>
                  {p.name}
                </Option>
              ))}
              {card.kind !== "Me" && (
                <Option value={NEW_PERSON} text="New person">
                  New person
                </Option>
              )}
            </Combobox>
          </Field>
          {chosen === NEW_PERSON && (
            <Field label="New person's name">
              <Input value={newName} onChange={(_, d) => setNewName(d.value)} />
            </Field>
          )}
          <div>
            <Button appearance="primary" size="small" disabled={!canLink} onClick={link}>
              Link
            </Button>
          </div>
        </div>
      </PopoverSurface>
    </Popover>
  );
}
