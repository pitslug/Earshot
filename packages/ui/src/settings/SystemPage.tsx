// Settings, System (docs/settings-design.md "Settings", T12.5b): what Earshot works out for this
// computer, for whoever wants to look: the hardware, the recommendation, the speech helpers, where
// each task runs, the model check, NVIDIA's CUDA libraries where offered, and the benchmark. The
// model parts share the `useModels` state Settings holds.

import { Title2, makeStyles, tokens } from "@fluentui/react-components";

import { BenchmarkSection } from "./BenchmarkSection";
import { CudaParts } from "./CudaSection";
import { CheckSection, HelpersSection, ModelNotices, ModelsNotLoaded, Section } from "./ModelSections";
import { HardwareSection, Recommendation, WhereItRuns } from "./ModelsParts";
import { useCuda } from "./useCuda";
import type { ModelsPageState } from "./useModels";

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
});

/** The model parts of the page, or why they cannot be shown. */
function SystemModels({ models }: { models: ModelsPageState }) {
  const cuda = useCuda(models.refresh);
  const { load } = models;
  if (load.status !== "loaded") return <ModelsNotLoaded load={load} />;
  const { hardware, models: view, settings } = load.data;
  return (
    <>
      <ModelNotices state={models} data={load.data} />
      <Section title="This is your hardware">
        <HardwareSection hardware={hardware} models={view} />
      </Section>
      <Recommendation models={view} />
      <HelpersSection state={models} data={load.data} />
      <Section title="Where it runs">
        <WhereItRuns
          hardware={hardware}
          models={view}
          transcription={settings.value.transcription_part}
          summary={settings.value.summary_part}
          onChange={(task, part) => void models.save({ [task]: part })}
        />
      </Section>
      <Section title="Model check">
        <CheckSection state={models} data={load.data} />
      </Section>
      {cuda.view?.offered === true && (
        <Section title="NVIDIA CUDA libraries">
          <CudaParts state={cuda} view={cuda.view} />
        </Section>
      )}
    </>
  );
}

export interface SystemPageProps {
  /** The model state Settings holds for its pages. */
  models: ModelsPageState;
}

/** The System page of Settings. */
export function SystemPage({ models }: SystemPageProps) {
  const styles = useStyles();
  return (
    <div className={styles.page}>
      <Title2 as="h1" className={styles.heading}>
        System
      </Title2>
      <SystemModels models={models} />
      <BenchmarkSection />
    </div>
  );
}
