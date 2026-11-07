// The who-is-who panel (docs/ui-design.md "Meeting detail"): one row per speaker with a
// play-sample button, the name (a name field for voices that need one), where it came from,
// share of talk time, a sample line, Merge, Link to person and Forget voice; "This is me" and
// "This isn't me" in an in-room meeting; and the opt-in "Remember these voices for next time".

import {
  Body1Strong,
  Button,
  Caption1,
  Checkbox,
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
import { linkablePersons, personNamed, recordingsGone, rememberable, sourceLabel } from "./detail";

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
  heading: {
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
  remember: {
    display: "flex",
    flexDirection: "column",
    alignItems: "flex-start",
    gap: tokens.spacingVerticalXS,
    paddingTop: tokens.spacingVerticalS,
    borderTop: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
  },
});

export interface WhoIsWhoProps {
  detail: MeetingDetail;
  persons: PersonItem[];
  /** Speaker ids whose sample audio is no longer kept. */
  noAudio: ReadonlySet<string>;
  onPlay(speaker: string): void;
  /** Renames the speaker in this meeting only. */
  onRename(speaker: string, name: string): Promise<void>;
  /** Renames a person, and so their speakers in every meeting. */
  onRenamePerson(person: string, name: string): Promise<void>;
  onLink(speaker: string, choice: LinkChoice): Promise<void>;
  /** Says this voice is the user's, in this meeting. */
  onMarkMe(speaker: string): Promise<void>;
  /** Puts the user's row back as a voice that needs a name. */
  onUnmarkMe(speaker: string): Promise<void>;
  onMerge(keep: string, merge: string): Promise<void>;
  onForget(person: string): Promise<void>;
  onRemember(speakers: string[]): Promise<void>;
  /** Called with the name field of the first voice that needs a name (for "Name voices"). */
  firstNameField?: (input: HTMLInputElement | null) => void;
}

/** The who-is-who panel. */
export function WhoIsWho(props: WhoIsWhoProps) {
  const styles = useStyles();
  const { detail } = props;
  const [remember, setRemember] = useState(false);
  const [saving, setSaving] = useState(false);
  const named = rememberable(detail);
  const firstUnnamed = detail.speakers.find((s) => s.needs_name)?.id;
  const save = () => {
    setSaving(true);
    props.onRemember(named.map((s) => s.id)).finally(() => setSaving(false));
  };
  return (
    <aside className={styles.panel} aria-label="Who is who">
      <Subtitle2 as="h2" className={styles.heading}>
        Who is who
      </Subtitle2>
      <ul className={styles.list}>
        {detail.speakers.map((s) => (
          <SpeakerRow
            key={s.id}
            card={s}
            {...props}
            nameRef={s.id === firstUnnamed ? props.firstNameField : undefined}
          />
        ))}
      </ul>
      <div className={styles.remember}>
        <Checkbox
          checked={remember}
          onChange={(_, d) => setRemember(d.checked === true)}
          label="Remember these voices for next time"
        />
        <Caption1 className={styles.muted}>
          Keeps a voiceprint of each named voice so later meetings name them. Your own voice is
          never saved here.
        </Caption1>
        <Button
          size="small"
          disabled={!remember || named.length === 0 || saving}
          onClick={save}
        >
          Save voices
        </Button>
      </div>
    </aside>
  );
}

interface SpeakerRowProps extends WhoIsWhoProps {
  card: SpeakerCard;
  nameRef?: (input: HTMLInputElement | null) => void;
}

function SpeakerRow({
  card,
  detail,
  persons,
  noAudio,
  onPlay,
  onRename,
  onLink,
  onMarkMe,
  onUnmarkMe,
  onMerge,
  onForget,
  onRenamePerson,
  nameRef,
}: SpeakerRowProps) {
  const styles = useStyles();
  // Which rename is open: this meeting's row, or the person in every meeting.
  const [editing, setEditing] = useState<"row" | "person" | null>(null);
  const [forgetting, setForgetting] = useState(false);
  const others = detail.speakers.filter((s) => s.id !== card.id);
  const placeholder = `Name ${card.name.toLowerCase().startsWith("speaker") ? card.name.toLowerCase() : "this voice"}`;

  // Naming a voice makes a person (an existing one with that exact name), so only on Enter:
  // leaving the field keeps the draft.
  const name = async (value: string) => {
    const n = value.trim();
    if (n === "") return;
    const existing = personNamed(n, persons);
    await onLink(card.id, existing ? { kind: "Existing", person: existing.id } : { kind: "New", name: n });
  };
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
        <Button
          size="small"
          appearance="subtle"
          aria-label={`Play a sample of ${card.name}`}
          disabled={!detail.audio_kept}
          onClick={() => onPlay(card.id)}
        >
          Play
        </Button>
        {card.needs_name ? (
          <NameInput
            initial=""
            placeholder={placeholder}
            label={`Name of ${card.name}`}
            onCommit={name}
            inputRef={nameRef}
            enterOnly
          />
        ) : editing !== null ? (
          <NameInput
            initial={editing === "person" && card.person ? card.person.name : card.name}
            placeholder={placeholder}
            label={
              editing === "person"
                ? `New name of ${card.person?.name ?? card.name} in every meeting`
                : `Name of ${card.name} in this meeting`
            }
            onCommit={rename}
            autoFocus
          />
        ) : (
          <Body1Strong className={styles.name}>
            {card.kind === "Me" && card.person && card.person.name !== card.name
              ? `${card.name} (${card.person.name})`
              : card.name}
          </Body1Strong>
        )}
      </div>
      <Caption1 className={styles.muted}>
        {sourceLabel(card)} · {card.share_percent}% of talk time
      </Caption1>
      {card.sample_line !== "" && (
        <Caption1 className={styles.muted}>&quot;{card.sample_line}&quot;</Caption1>
      )}
      {editing === "person" && (
        <Caption1 className={styles.muted}>Renames this person in every meeting.</Caption1>
      )}
      {editing === "row" && (
        <Caption1 className={styles.muted}>Renames this speaker in this meeting only.</Caption1>
      )}
      {(noAudio.has(card.id) || recordingsGone(detail)) && (
        <Caption1 className={styles.muted}>Audio no longer kept</Caption1>
      )}
      <div className={styles.actions}>
        {card.kind === "Me" ? (
          <Tooltip
            content="Your own row cannot be merged away. Merge the other speaker into it."
            relationship="description"
          >
            <Button size="small" appearance="subtle" disabledFocusable>
              Merge
            </Button>
          </Tooltip>
        ) : (
          <Menu>
            <MenuTrigger disableButtonEnhancement>
              <MenuButton size="small" appearance="subtle" disabled={others.length === 0}>
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
        )}
        {card.can_mark_me && (
          <Tooltip
            content="Shows this voice as you in this meeting. No voiceprint is made from it."
            relationship="description"
          >
            <Button size="small" appearance="subtle" onClick={() => void onMarkMe(card.id)}>
              This is me
            </Button>
          </Tooltip>
        )}
        {card.can_unmark_me && (
          <Button size="small" appearance="subtle" onClick={() => void onUnmarkMe(card.id)}>
            This isn&apos;t me
          </Button>
        )}
        <LinkPopover card={card} persons={persons} onLink={onLink} />
        {!card.needs_name && card.kind === "Other" && editing === null && (
          <Button size="small" appearance="subtle" onClick={() => setEditing("row")}>
            Rename here
          </Button>
        )}
        {card.kind === "Other" && card.person !== null && editing === null && (
          <Button size="small" appearance="subtle" onClick={() => setEditing("person")}>
            Rename person
          </Button>
        )}
        {card.kind === "Other" && card.person?.has_voiceprint && (
          <Button size="small" appearance="subtle" onClick={() => setForgetting(true)}>
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

function NameInput({
  initial,
  placeholder,
  label,
  onCommit,
  inputRef,
  autoFocus,
  enterOnly = false,
}: {
  initial: string;
  placeholder: string;
  label: string;
  onCommit(value: string): Promise<void>;
  inputRef?: (input: HTMLInputElement | null) => void;
  autoFocus?: boolean;
  /** Commit on Enter only; leaving the field keeps the draft. */
  enterOnly?: boolean;
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
      autoFocus={autoFocus}
      ref={inputRef}
      onChange={(_, d) => setValue(d.value)}
      onBlur={enterOnly ? undefined : commit}
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
}: {
  card: SpeakerCard;
  persons: PersonItem[];
  onLink(speaker: string, choice: LinkChoice): Promise<void>;
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
        <Button size="small" appearance="subtle">
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
