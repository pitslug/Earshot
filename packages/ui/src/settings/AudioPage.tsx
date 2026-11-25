// Settings, Audio (docs/ui-design.md "Settings", PLAN T7.4i): the microphone and the output a
// meeting records (the Windows default, followed when it changes, or one device by name),
// the capture mode a new meeting starts with, and whether the microphone is recorded with
// Windows' own sound processing or raw (T12.36, off by default). Choices save as they change. A chosen
// device that is not connected stays chosen and is shown as such; meetings then record the
// Windows default and say so. The audio check (T7.4j) plays short sounds through the output
// and shows what it found for the devices in use (AudioCheckSection).

import {
  Button,
  Caption1,
  Field,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  Radio,
  RadioGroup,
  Select,
  Spinner,
  Title2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { useId } from "react";

import type { AudioPageView, CaptureMode, ChosenDevice, DeviceItem, MicSignal } from "../ipc";
import { MODE_LABEL } from "../meeting/liveMeeting";
import { AudioCheckSection } from "./AudioCheckSection";
import { FIELD_WIDTH, NotLoaded, NoticeBar, Section, TEXT_WIDTH, UnknownSettings, usePageStyles } from "./parts";
import { type AudioPageState, useAudio } from "./useAudio";
import { checkRunning, useAudioCheck } from "./useAudioCheck";

const useStyles = makeStyles({
  heading: {
    margin: 0,
  },
  fields: {
    display: "grid",
    gap: tokens.spacingVerticalM,
    maxWidth: FIELD_WIDTH,
  },
  muted: {
    color: tokens.colorNeutralForeground2,
    maxWidth: TEXT_WIDTH,
  },
  row: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
});

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

/** The choices of the microphone's sound processing (T12.36, T12.36b). */
export const SIGNAL_LABEL: Record<MicSignal, string> = {
  Automatic: "Automatic: raw in the room, Windows' processing in calls (recommended)",
  Processed: "Always with Windows' sound processing",
  Raw: "Always raw, without Windows' sound processing",
};

/** What the sound processing setting does. */
export const SIGNAL_DESCRIPTION =
  "Some computers clean up the microphone before Earshot hears it. In a call that can leave echo of the other side under your name, and in a room it can treat people further away as noise. Raw skips that, where the microphone allows it. If you choose raw and the microphone does not allow it, it is recorded with the processing and the meeting says so. Run the audio check again after you change this.";

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
  const pageStyles = usePageStyles();
  const state = useAudio();
  const modeLabel = useId();
  const signalLabel = useId();
  const { load } = state;
  const chosen = load.status === "loaded" ? load.page.value : null;
  // The stored check is per microphone, output and sound processing.
  const check = useAudioCheck(
    `${chosen?.microphone?.id ?? "default"}|${chosen?.output?.id ?? "default"}|${chosen?.mic_signal ?? "Automatic"}`,
  );
  // The devices stay as they are while the check plays and listens on them.
  const deviceState = checkRunning(check.run) ? { ...state, busy: true } : state;

  if (load.status !== "loaded") {
    return (
      <NotLoaded load={load} loading="Reading the audio settings" failed="The audio settings could not be read" />
    );
  }
  const { page } = load;

  return (
    <div className={pageStyles.page}>
      <Title2 as="h1" className={styles.heading}>
        Audio
      </Title2>
      {state.notice !== null && (
        <NoticeBar title={state.notice.title} message={state.notice.message} onDismiss={state.dismiss} />
      )}
      <UnknownSettings unknown={page.unknown} />

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

      <Section title="Call or in the room">
        <span id={modeLabel} hidden>
          Call or in the room
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

      <Section title="Microphone sound processing">
        <span id={signalLabel} hidden>
          Microphone sound processing
        </span>
        <RadioGroup
          aria-labelledby={signalLabel}
          value={page.value.mic_signal}
          disabled={deviceState.busy}
          onChange={(_, d) => void state.save({ mic_signal: d.value as MicSignal })}
        >
          <Radio value="Automatic" label={SIGNAL_LABEL.Automatic} />
          <Radio value="Processed" label={SIGNAL_LABEL.Processed} />
          <Radio value="Raw" label={SIGNAL_LABEL.Raw} />
        </RadioGroup>
        <Caption1 className={styles.muted}>{SIGNAL_DESCRIPTION}</Caption1>
      </Section>

      <Section title="Audio check">
        <AudioCheckSection state={check} disabled={state.busy} />
      </Section>
    </div>
  );
}
