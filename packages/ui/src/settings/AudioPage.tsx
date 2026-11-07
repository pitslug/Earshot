// Settings, Audio (docs/ui-design.md "Settings", PLAN T7.4i): the microphone and the output a
// meeting records (the Windows default, followed when it changes, or one device by name),
// and the capture mode a new meeting starts with. Choices save as they change. A chosen
// device that is not connected stays chosen and is shown as such; meetings then record the
// Windows default and say so. The audio check (T7.4j) plays short sounds through the output
// and shows what it found for the devices in use (AudioCheckSection).

import {
  Button,
  Caption1,
  Field,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
  Radio,
  RadioGroup,
  Select,
  Spinner,
  Subtitle2,
  Title2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { type ReactNode, useId } from "react";

import type { AudioPageView, CaptureMode, ChosenDevice, DeviceItem } from "../ipc";
import { MODE_LABEL } from "../meeting/liveMeeting";
import { AudioCheckSection } from "./AudioCheckSection";
import { type AudioPageState, useAudio } from "./useAudio";
import { checkRunning, useAudioCheck } from "./useAudioCheck";

const useStyles = makeStyles({
  page: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXL,
    maxWidth: "860px",
  },
  section: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
  },
  heading: {
    margin: 0,
  },
  fields: {
    display: "grid",
    gap: tokens.spacingVerticalM,
    maxWidth: "520px",
  },
  muted: {
    color: tokens.colorNeutralForeground2,
    maxWidth: "62ch",
  },
  row: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
});

function Section({ title, children }: { title: string; children: ReactNode }) {
  const styles = useStyles();
  const id = useId();
  return (
    <section className={styles.section} aria-labelledby={id}>
      <Subtitle2 as="h2" id={id} className={styles.heading}>
        {title}
      </Subtitle2>
      {children}
    </section>
  );
}

/** The select's value for the Windows default (endpoint ids start with `{`). */
export const DEFAULT_VALUE = "default";

/** What each device kind is called in sentences. */
const KIND = {
  microphone: { label: "Microphone", word: "microphone" },
  output: { label: "Output", word: "output" },
} as const;

type DeviceKind = keyof typeof KIND;

/** The warning for a Bluetooth headset in hands-free mode (the meeting banner's advice). */
export const HANDS_FREE_HINT =
  "This is a Bluetooth headset in hands-free mode, so Windows records it at phone quality. Use the computer's own mic and keep the headset for listening.";

/** What the output choice is for. */
export const OUTPUT_DESCRIPTION =
  "Earshot records everything that plays on this output as the other side of a call, so the call app needs to play through it.";

/** What the microphone choice is for. */
export const MICROPHONE_DESCRIPTION =
  "The Windows default follows the microphone set in Windows, also when it changes during a meeting. A device you choose here stays chosen.";

/** What the capture mode setting does. */
export const MODE_DESCRIPTION =
  "What a new meeting records. You can still pick the other one when you start a meeting.";

/** The option text of the Windows default, naming the device it is now. */
export function defaultLabel(devices: DeviceItem[]): string {
  const d = devices.find((x) => x.is_default);
  return d === undefined ? "Windows default (none now)" : `Windows default (${d.name})`;
}

/** The name of a chosen device that is not connected. */
function chosenName(kind: DeviceKind, chosen: ChosenDevice): string {
  return chosen.name.trim() === "" ? `The chosen ${KIND[kind].word}` : chosen.name;
}

/** The hint under a device select (null: none). */
export function deviceHint(kind: DeviceKind, page: AudioPageView): string | null {
  const devices = kind === "microphone" ? page.microphones : page.outputs;
  const chosen = kind === "microphone" ? page.value.microphone : page.value.output;
  const missing = kind === "microphone" ? page.microphone_missing : page.output_missing;
  const word = KIND[kind].word;
  if (missing && chosen !== null) {
    return `${chosenName(kind, chosen)} is not connected, so meetings record the Windows default ${word}. It is used again from the next meeting you start with it connected.`;
  }
  if (missing) return `Windows has no ${word} now. Connect one before you start a meeting.`;
  const used = chosen === null ? devices.find((d) => d.is_default) : devices.find((d) => d.id === chosen.id);
  if (kind === "microphone" && used?.hands_free) return HANDS_FREE_HINT;
  return null;
}

function DeviceField({ kind, state, page }: { kind: DeviceKind; state: AudioPageState; page: AudioPageView }) {
  const devices = kind === "microphone" ? page.microphones : page.outputs;
  const chosen = kind === "microphone" ? page.value.microphone : page.value.output;
  const missing = kind === "microphone" ? page.microphone_missing : page.output_missing;
  const hint = deviceHint(kind, page);
  const hintIsWarning = hint !== null && (missing || hint === HANDS_FREE_HINT);
  const choose = (value: string) => {
    if (value === DEFAULT_VALUE) {
      void state.save({ [kind]: null });
      return;
    }
    const d = devices.find((x) => x.id === value);
    // The chosen device that is not connected is already stored.
    if (d === undefined) return;
    void state.save({ [kind]: { id: d.id, name: d.name } });
  };
  return (
    <Field
      label={KIND[kind].label}
      validationState={hintIsWarning ? "warning" : "none"}
      validationMessage={hintIsWarning ? hint : undefined}
      hint={hintIsWarning ? undefined : (hint ?? undefined)}
    >
      <Select
        value={chosen === null ? DEFAULT_VALUE : chosen.id}
        disabled={state.busy}
        onChange={(_, d) => choose(d.value)}
      >
        <option value={DEFAULT_VALUE}>{defaultLabel(devices)}</option>
        {devices.map((d) => (
          <option key={d.id} value={d.id}>
            {d.name}
          </option>
        ))}
        {chosen !== null && !devices.some((d) => d.id === chosen.id) && (
          <option value={chosen.id}>{`${chosenName(kind, chosen)} (not connected)`}</option>
        )}
      </Select>
    </Field>
  );
}

/** The Audio page of Settings. */
export function AudioPage() {
  const styles = useStyles();
  const state = useAudio();
  const modeLabel = useId();
  const { load } = state;
  const chosen = load.status === "loaded" ? load.page.value : null;
  const check = useAudioCheck(`${chosen?.microphone?.id ?? "default"}|${chosen?.output?.id ?? "default"}`);
  // The devices stay as they are while the check plays and listens on them.
  const deviceState = checkRunning(check.run) ? { ...state, busy: true } : state;

  if (load.status === "loading") return <Spinner label="Reading the audio settings" />;
  if (load.status === "failed") {
    return (
      <MessageBar intent="error">
        <MessageBarBody>
          <MessageBarTitle>The audio settings could not be read</MessageBarTitle>
          {load.message}
        </MessageBarBody>
      </MessageBar>
    );
  }
  const { page } = load;

  return (
    <div className={styles.page}>
      <Title2 as="h1" className={styles.heading}>
        Audio
      </Title2>
      {state.notice !== null && (
        <MessageBar intent="error">
          <MessageBarBody>
            <MessageBarTitle>{state.notice.title}</MessageBarTitle>
            {state.notice.message}
          </MessageBarBody>
          <MessageBarActions
            containerAction={
              <Button size="small" appearance="transparent" onClick={state.dismiss}>
                Dismiss
              </Button>
            }
          />
        </MessageBar>
      )}
      {page.unknown.length > 0 && (
        <MessageBar intent="warning">
          <MessageBarBody>
            <MessageBarTitle>Some stored settings are not known to this version</MessageBarTitle>
            {page.unknown.join(" ")}
          </MessageBarBody>
        </MessageBar>
      )}

      <Section title="Devices">
        {page.devices_error !== null && (
          <MessageBar intent="warning">
            <MessageBarBody>
              <MessageBarTitle>The devices could not be listed</MessageBarTitle>
              {page.devices_error}
            </MessageBarBody>
          </MessageBar>
        )}
        <div className={styles.fields}>
          <DeviceField kind="microphone" state={deviceState} page={page} />
          <Caption1 className={styles.muted}>{MICROPHONE_DESCRIPTION}</Caption1>
          <DeviceField kind="output" state={deviceState} page={page} />
          <Caption1 className={styles.muted}>{OUTPUT_DESCRIPTION}</Caption1>
        </div>
        <div className={styles.row}>
          <Button disabled={state.busy} onClick={() => void state.refresh()}>
            Refresh devices
          </Button>
          {state.busy && <Spinner size="tiny" label="Saving" />}
        </div>
      </Section>

      <Section title="Capture mode">
        <span id={modeLabel} hidden>
          Capture mode
        </span>
        <RadioGroup
          aria-labelledby={modeLabel}
          value={page.value.capture_mode}
          disabled={state.busy}
          onChange={(_, d) => void state.save({ capture_mode: d.value as CaptureMode })}
        >
          <Radio value="Call" label={MODE_LABEL.Call} />
          <Radio value="InRoom" label={MODE_LABEL.InRoom} />
        </RadioGroup>
        <Caption1 className={styles.muted}>{MODE_DESCRIPTION}</Caption1>
      </Section>

      <Section title="Audio check">
        <AudioCheckSection state={check} disabled={state.busy} />
      </Section>
    </div>
  );
}
