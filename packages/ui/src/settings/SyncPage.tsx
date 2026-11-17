// Settings, Sync (T11.3a, docs/sync.md): meetings kept the same on the user's computers through
// their own sync server, which holds them only encrypted. A computer that is not set up offers
// two ways in: set up the first computer with the server's setup secret, or join with a pairing
// code shown on another computer. A computer that is set up shows the status, a pairing code for
// adding another computer, the computers, voiceprint sync and removal. Every choice applies at
// once; there is no Save button on this page.

import {
  Body1,
  Button,
  Caption1,
  Dialog,
  DialogActions,
  DialogBody,
  DialogContent,
  DialogSurface,
  DialogTitle,
  Field,
  Input,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  Switch,
  Text,
  Title2,
  makeStyles,
  tokens,
  useRestoreFocusTarget,
} from "@fluentui/react-components";
import { type RefObject, useEffect, useRef, useState } from "react";

import table from "../../../../fixtures/ui/sync_copy_cases.json";
import type { SyncDeviceView, SyncView } from "../ipc";
import { clockTime, recordedDate } from "../shell/meetings";
import { type Timer, realTimer } from "../time";
import { NotLoaded, NoticeBar, Section, usePageStyles } from "./parts";
import { type SyncForm, type SyncPageState, useSync } from "./useSync";

const useStyles = makeStyles({
  heading: {
    margin: 0,
  },
  text: {
    maxWidth: "70ch",
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
  form: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
    maxWidth: "520px",
  },
  action: {
    alignSelf: "flex-start",
  },
  counts: {
    margin: 0,
    display: "grid",
    gridTemplateColumns: "max-content max-content",
    columnGap: tokens.spacingHorizontalL,
    rowGap: tokens.spacingVerticalXS,
  },
  row: {
    display: "contents",
  },
  term: {
    color: tokens.colorNeutralForeground3,
  },
  value: {
    margin: 0,
  },
  code: {
    fontFamily: tokens.fontFamilyMonospace,
    fontSize: tokens.fontSizeHero700,
    lineHeight: tokens.lineHeightHero700,
    letterSpacing: "0.04em",
    overflowWrap: "anywhere",
  },
  list: {
    listStyle: "none",
    margin: 0,
    padding: 0,
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
    maxWidth: "520px",
  },
  device: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    gap: tokens.spacingHorizontalM,
    padding: `${tokens.spacingVerticalXS} ${tokens.spacingHorizontalM}`,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke1}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorNeutralBackground1,
  },
  deviceText: {
    display: "flex",
    flexDirection: "column",
    minWidth: 0,
  },
  removeOther: {
    flexShrink: 0,
  },
});

/** What sync is, first on the page of a computer that is not set up. */
export const SYNC_INTRO =
  "Sync keeps your meetings the same on each of your computers, through a sync server you run yourself. The server stores them encrypted and cannot read them.";

/** What voiceprint sync means. */
export const VOICEPRINTS_EXPLAINED =
  "Voice samples are biometric data, so they stay on each computer unless you turn this on. Turning it on also brings in the ones your other computers saved earlier.";

/** What Dictionary sync means (T11.3e). */
export const DICTIONARY_EXPLAINED =
  "Your Dictionary words stay the same on each computer that syncs them. Turned off, this computer keeps its own list.";

/** What Status says when sync is on but its thread is not running. */
export const NOT_RUNNING = "Sync is not running. Choose Sync now to start it.";

/** What removing this computer does, in its dialog. */
export const REMOVE_EXPLAINED =
  "Your meetings stay on this computer. It stops syncing and forgets the sync server and its key. To sync again, set it up again or join with a pairing code.";

/** Removing another computer (T11.6c): the shared table's copy, which the desktop's tests also
 * read. */
export const REMOVE_OTHER = table.remove_other;

/** What "Write summaries for my other computers" means (T11.3db). */
export const OFFER_EXPLAINED =
  "When another of your computers asks, this computer writes the meeting's summary with its own summary writer. The other computer chooses it on its Summaries page.";

/** What the computer's name is for. */
export const NAME_EXPLAINED = "How your other computers show this one. Leave it empty to use the computer's own name.";

/** How long ago a computer last reached the server, in words. */
export function seenText(secondsAgo: number): string {
  const s = Math.max(0, Math.round(secondsAgo));
  if (s < 60) return "Last seen just now";
  const plural = (n: number, unit: string) => `Last seen ${n} ${unit}${n === 1 ? "" : "s"} ago`;
  if (s < 3_600) return plural(Math.floor(s / 60), "minute");
  if (s < 86_400) return plural(Math.floor(s / 3_600), "hour");
  return plural(Math.floor(s / 86_400), "day");
}

/** How long a pairing code works, in words. */
export function codeLifeText(seconds: number): string {
  const minutes = Math.round(seconds / 60);
  if (minutes < 1) return "Works for less than a minute, once.";
  return `Works for ${minutes} minute${minutes === 1 ? "" : "s"}, once.`;
}

/** A computer's name in the list: its own name from its presence, else its short id. */
export function deviceName(d: SyncDeviceView): string {
  if (d.this_device) return "This computer";
  return d.name ?? `Computer ${d.short_id}`;
}

/** A computer's state in the list: online (and recording) from its presence, else when it
 * last reached the server. */
export function deviceState(d: SyncDeviceView): string {
  if (d.this_device) return seenText(d.seen_s_ago);
  if (d.online) return d.recording ? "Online, recording" : "Online";
  return seenText(d.seen_s_ago);
}

/** What a computer that writes summaries for the others uses, in the list. */
export function writesText(d: SyncDeviceView): string | null {
  if (d.this_device || !d.offers_summaries) return null;
  return d.writer === null ? "Writes summaries" : `Writes summaries (${d.writer})`;
}

/** This computer's name, typed, saved with its own button (typed text needs Save). */
function NameForm({ view, state }: { view: SyncView; state: SyncPageState }) {
  const styles = useStyles();
  const [name, setName] = useState(view.device_name);
  const [error, setError] = useState<string | null>(null);
  return (
    <form
      aria-label="This computer's name"
      className={styles.form}
      onSubmit={(e) => {
        e.preventDefault();
        void state.saveDeviceName(name).then((refused) => {
          setError(refused);
          if (refused === null) setName((n) => n.trim());
        });
      }}
    >
      <Field label="This computer's name" hint={NAME_EXPLAINED} validationMessage={error ?? undefined}>
        <Input value={name} onChange={(_, d) => setName(d.value)} />
      </Field>
      <Button type="submit" className={styles.action} disabled={state.busy || name.trim() === view.device_name}>
        Save name
      </Button>
    </form>
  );
}

/** One of the two forms of a computer that is not set up. */
function JoinForm({
  title,
  secretLabel,
  secretType,
  button,
  form,
  onChange,
  onSubmit,
  busy,
}: {
  title: string;
  secretLabel: string;
  secretType: "password" | "text";
  button: string;
  form: SyncForm;
  onChange: (form: SyncForm) => void;
  onSubmit: () => void;
  busy: boolean;
}) {
  const styles = useStyles();
  return (
    <Section title={title}>
      <form
        aria-label={title}
        className={styles.form}
        onSubmit={(e) => {
          e.preventDefault();
          onSubmit();
        }}
      >
        <Field label="Server address">
          <Input
            value={form.address}
            placeholder="https://sync.example.com"
            onChange={(_, d) => onChange({ ...form, address: d.value })}
          />
        </Field>
        <Field label={secretLabel}>
          <Input
            type={secretType}
            value={form.secret}
            autoComplete="off"
            spellCheck={false}
            onChange={(_, d) => onChange({ ...form, secret: d.value })}
          />
        </Field>
        <Button type="submit" appearance="primary" className={styles.action} disabled={busy}>
          {button}
        </Button>
        {form.error !== null && (
          <MessageBar intent="error">
            <MessageBarBody>{form.error}</MessageBarBody>
          </MessageBar>
        )}
      </form>
    </Section>
  );
}

/** The props of the two forms of a computer that is not set up. `onSubmit` replaces the form's
 * own call (the first-run wizard's Next makes it, T12.79). */
interface SetUpFormProps {
  state: SyncPageState;
  onSubmit?: () => void;
}

/** "Set up the first computer": the server's address and its setup secret. */
export function SetUpFirstForm({ state, onSubmit }: SetUpFormProps) {
  return (
    <JoinForm
      title="Set up the first computer"
      secretLabel="Setup secret"
      secretType="password"
      button="Set up"
      form={state.setup}
      onChange={state.setSetup}
      onSubmit={onSubmit ?? (() => void state.setUpFirst())}
      busy={state.busy}
    />
  );
}

/** "Join with a pairing code": the server's address and a code shown on another computer. */
export function JoinWithCodeForm({ state, onSubmit }: SetUpFormProps) {
  return (
    <JoinForm
      title="Join with a pairing code"
      secretLabel="Pairing code"
      secretType="text"
      button="Join"
      form={state.join}
      onChange={state.setJoin}
      onSubmit={onSubmit ?? (() => void state.joinWithCode())}
      busy={state.busy}
    />
  );
}

function NotSetUp({ state }: { state: SyncPageState }) {
  const styles = useStyles();
  return (
    <>
      <Body1 className={styles.text}>{SYNC_INTRO}</Body1>
      <SetUpFirstForm state={state} />
      <JoinWithCodeForm state={state} />
    </>
  );
}

function Status({ view, state }: { view: SyncView; state: SyncPageState }) {
  const styles = useStyles();
  const last =
    view.last_success_unix_ms === null
      ? "Not yet"
      : `${recordedDate(view.last_success_unix_ms)} ${clockTime(view.last_success_unix_ms)}`;
  const rows: [string, string][] = [
    ["Server", view.address ?? "Not usable"],
    ["Last sync", last],
    ["Waiting to send", String(view.waiting)],
    ["Held for later", String(view.held)],
    ["Could not be read", String(view.unreadable)],
  ];
  return (
    <Section title="Status">
      <Switch
        label="Sync is on"
        checked={view.on}
        disabled={state.busy}
        onChange={(_, d) => void state.setOn(d.checked)}
      />
      <dl className={styles.counts} aria-label="Sync status">
        {rows.map(([term, value]) => (
          <div key={term} className={styles.row}>
            <dt className={styles.term}>{term}</dt>
            <dd className={styles.value}>{value}</dd>
          </div>
        ))}
      </dl>
      {view.on && view.set_up && !view.running && <Body1 className={styles.text}>{NOT_RUNNING}</Body1>}
      {view.address_problem !== null && (
        <MessageBar intent="warning">
          <MessageBarBody>{view.address_problem}</MessageBarBody>
        </MessageBar>
      )}
      {view.problem !== null && (
        <MessageBar intent="warning">
          <MessageBarBody>
            <MessageBarTitle>
              {view.stopped ? "Sync is stopped until this is fixed" : "The last sync had a problem"}
            </MessageBarTitle>
            {view.problem}
          </MessageBarBody>
        </MessageBar>
      )}
      <Button className={styles.action} disabled={state.busy || !view.on} onClick={() => void state.syncNow()}>
        Sync now
      </Button>
    </Section>
  );
}

function AddComputer({ state }: { state: SyncPageState }) {
  const styles = useStyles();
  const showRef = useRef<HTMLButtonElement>(null);
  const hideRef = useRef<HTMLButtonElement>(null);
  // Focus follows the swap: to Hide when the code appears, back to Show when it goes (Hide or
  // its expiry), so it never falls to the page.
  const shown = state.code !== null;
  const wasShown = useRef(shown);
  useEffect(() => {
    if (shown === wasShown.current) return;
    wasShown.current = shown;
    (shown ? hideRef : showRef).current?.focus();
  }, [shown]);
  return (
    <Section title="Add a computer">
      <Body1 className={styles.text}>
        On the other computer, choose Join with a pairing code and type the code shown here.
      </Body1>
      {state.code === null ? (
        <Button
          ref={showRef}
          className={styles.action}
          disabled={state.busy}
          onClick={() => void state.showCode()}
        >
          Show a pairing code
        </Button>
      ) : (
        <>
          <Text className={styles.code}>{state.code.code}</Text>
          <Caption1 className={styles.muted}>{codeLifeText(state.code.expires_in_s)}</Caption1>
          <Button ref={hideRef} className={styles.action} onClick={state.hideCode}>
            Hide
          </Button>
        </>
      )}
      {state.codeError !== null && (
        <MessageBar intent="error">
          <MessageBarBody>{state.codeError}</MessageBarBody>
        </MessageBar>
      )}
    </Section>
  );
}

/** A `remove_other` template (`fixtures/ui/sync_copy_cases.json`) with the row's name. */
export function removeOtherText(template: string, d: SyncDeviceView): string {
  // A function replacer: a name is text, never a replacement pattern (`$&`, `$1`).
  return template.replace("{device}", () => deviceName(d));
}

/** One other computer's Remove; the dialog hands focus back to it on Cancel. */
function RemoveOther({ device, state, onRemove }: { device: SyncDeviceView; state: SyncPageState; onRemove: () => void }) {
  const styles = useStyles();
  const restoreFocus = useRestoreFocusTarget();
  return (
    <Button
      {...restoreFocus}
      size="small"
      className={styles.removeOther}
      aria-label={removeOtherText(REMOVE_OTHER.button, device)}
      disabled={state.busy}
      onClick={onRemove}
    >
      {REMOVE_OTHER.confirm}
    </Button>
  );
}

function Computers({
  state,
  listRef,
  onRemoveOther,
}: {
  state: SyncPageState;
  listRef: RefObject<HTMLUListElement | null>;
  onRemoveOther: (d: SyncDeviceView) => void;
}) {
  const styles = useStyles();
  const { devices } = state;
  return (
    <Section title="Computers">
      {devices === null || devices.status !== "loaded" ? (
        <NotLoaded
          load={devices ?? { status: "loading" }}
          size="small"
          loading="Asking the sync server"
          failed="The computers could not be listed"
        />
      ) : (
        <ul ref={listRef} tabIndex={-1} className={styles.list} aria-label="Computers">
          {devices.view.map((d) => (
            <li key={d.id} className={styles.device}>
              <div className={styles.deviceText}>
                <Body1>{deviceName(d)}</Body1>
                <Caption1 className={styles.muted}>{deviceState(d)}</Caption1>
                {writesText(d) !== null && <Caption1 className={styles.muted}>{writesText(d)}</Caption1>}
              </div>
              {!d.this_device && <RemoveOther device={d} state={state} onRemove={() => onRemoveOther(d)} />}
            </li>
          ))}
        </ul>
      )}
    </Section>
  );
}

function SetUp({
  view,
  state,
  onRemove,
  listRef,
  onRemoveOther,
}: {
  view: SyncView;
  state: SyncPageState;
  onRemove: () => void;
  listRef: RefObject<HTMLUListElement | null>;
  onRemoveOther: (d: SyncDeviceView) => void;
}) {
  const styles = useStyles();
  const restoreFocus = useRestoreFocusTarget();
  return (
    <>
      {state.error !== null && (
        <NoticeBar title="This was not changed" message={state.error} onDismiss={state.clearError} />
      )}
      <Status view={view} state={state} />
      <AddComputer state={state} />
      <Computers state={state} listRef={listRef} onRemoveOther={onRemoveOther} />
      <Section title="Summaries for your other computers">
        <Switch
          label="Write summaries for my other computers"
          checked={view.offers_summaries}
          disabled={state.busy}
          onChange={(_, d) => void state.setOfferSummaries(d.checked)}
        />
        <Caption1 className={styles.muted}>{OFFER_EXPLAINED}</Caption1>
      </Section>
      <Section title="This computer">
        <NameForm view={view} state={state} />
      </Section>
      <Section title="Dictionary">
        <Switch
          label="Sync Dictionary words"
          checked={view.dictionary}
          disabled={state.busy}
          onChange={(_, d) => void state.setDictionary(d.checked)}
        />
        <Caption1 className={styles.muted}>{DICTIONARY_EXPLAINED}</Caption1>
      </Section>
      <Section title="Voiceprints">
        <Switch
          label="Sync voiceprints"
          checked={view.voiceprints}
          disabled={state.busy}
          onChange={(_, d) => void state.setVoiceprints(d.checked)}
        />
        <Caption1 className={styles.muted}>{VOICEPRINTS_EXPLAINED}</Caption1>
      </Section>
      <Section title="Remove this computer">
        <Button
          {...restoreFocus}
          className={styles.action}
          disabled={state.busy}
          onClick={onRemove}
        >
          Remove this computer
        </Button>
      </Section>
    </>
  );
}

/** The Sync page of Settings. The removal dialogs live here, not in the set up part, because a
 * removal of this computer replaces that part with the forms while the dialog closes, and a
 * removal of another computer takes away the row that opened it. */
export function SyncPage({ timer = realTimer }: { timer?: Timer }) {
  const styles = useStyles();
  const page = usePageStyles();
  const state = useSync(timer);
  const [removing, setRemoving] = useState(false);
  // The other computer whose removal is being confirmed (kept while the dialog closes).
  const [other, setOther] = useState<SyncDeviceView | null>(null);
  const [otherOpen, setOtherOpen] = useState(false);
  const listRef = useRef<HTMLUListElement>(null);
  const removeOther = async (d: SyncDeviceView) => {
    setOtherOpen(false);
    // Removed: its row goes, and the Remove that opened the dialog with it. Refused: that Remove
    // is disabled while the call ran, so the dialog's hand-back found nothing to focus. Either
    // way focus moves to the list rather than staying on the page.
    await state.removeOther(d.id);
    listRef.current?.focus();
  };
  const { load } = state;
  // After set up, join or removal swaps the page's parts, focus goes to the heading rather than
  // falling to the page. The first read is not a swap.
  const headingRef = useRef<HTMLHeadingElement>(null);
  const setUp = load.status === "loaded" ? load.view.set_up : null;
  const wasSetUp = useRef(setUp);
  useEffect(() => {
    if (setUp === null) return;
    const before = wasSetUp.current;
    wasSetUp.current = setUp;
    if (before !== null && before !== setUp) headingRef.current?.focus();
  }, [setUp]);
  return (
    <div className={page.page}>
      <h1 ref={headingRef} tabIndex={-1} className={styles.heading}>
        <Title2>Sync</Title2>
      </h1>
      {load.status !== "loaded" ? (
        <NotLoaded load={load} loading="Reading the sync settings" failed="The sync settings could not be read" />
      ) : load.view.set_up ? (
        <SetUp
          view={load.view}
          state={state}
          onRemove={() => setRemoving(true)}
          listRef={listRef}
          onRemoveOther={(d) => {
            setOther(d);
            setOtherOpen(true);
          }}
        />
      ) : (
        <NotSetUp state={state} />
      )}
      <Dialog modalType="alert" open={removing} onOpenChange={(_, d) => setRemoving(d.open)}>
        <DialogSurface>
          <DialogBody>
            <DialogTitle>Remove this computer from sync?</DialogTitle>
            <DialogContent>{REMOVE_EXPLAINED}</DialogContent>
            <DialogActions>
              <Button
                appearance="primary"
                onClick={() => {
                  setRemoving(false);
                  void state.remove();
                }}
              >
                Remove
              </Button>
              <Button onClick={() => setRemoving(false)}>Cancel</Button>
            </DialogActions>
          </DialogBody>
        </DialogSurface>
      </Dialog>
      <Dialog modalType="alert" open={otherOpen} onOpenChange={(_, d) => setOtherOpen(d.open)}>
        <DialogSurface>
          <DialogBody>
            <DialogTitle>{other === null ? "" : removeOtherText(REMOVE_OTHER.title, other)}</DialogTitle>
            <DialogContent>{REMOVE_OTHER.explained}</DialogContent>
            <DialogActions>
              <Button
                appearance="primary"
                onClick={() => {
                  if (other !== null) void removeOther(other);
                }}
              >
                {REMOVE_OTHER.confirm}
              </Button>
              <Button onClick={() => setOtherOpen(false)}>Cancel</Button>
            </DialogActions>
          </DialogBody>
        </DialogSurface>
      </Dialog>
    </div>
  );
}
