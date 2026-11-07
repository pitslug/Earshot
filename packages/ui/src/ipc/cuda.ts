// The CUDA section's commands (T8.4af), kept apart from ipc.ts so `IpcCommands` gains them in one
// line.

import type {
  CudaEvent,
  CudaPartChoice,
  CudaPartView,
  CudaRemoved,
  CudaRunView,
  CudaSwitchValue,
  CudaTerms,
  CudaView,
  TermsLink,
} from "./bindings";

export type { CudaEvent, CudaPartChoice, CudaPartView, CudaRemoved, CudaRunView, CudaSwitchValue, CudaTerms, CudaView, TermsLink };

/** The CUDA part of `IpcCommands`. */
export interface CudaCommands {
  /** Whether this computer is offered NVIDIA's libraries, and the state of both parts. */
  cudaSettings(): Promise<CudaView>;
  /** Stores the two settings; they apply at the next start. Rejects with `invalid_setting`. */
  setCudaSettings(value: CudaSwitchValue): Promise<CudaSwitchValue>;
  /** Starts a part's download; `CudaEvent`s follow. Rejects unless `acceptedTerms`. */
  startCudaDownload(part: CudaPartChoice, acceptedTerms: boolean): Promise<void>;
  /** Cancels a part's download; what arrived is kept. Rejects with `not_downloading`. */
  cancelCudaDownload(part: CudaPartChoice): Promise<void>;
  /** Removes a part's files. Rejects with `part_in_use`, or `busy` while either part downloads, the model check, a meeting or a meeting job runs. */
  removeCudaPart(part: CudaPartChoice): Promise<CudaRemoved>;
  /** Opens NVIDIA's terms in the browser. Takes a choice, never an address. */
  openCudaTerms(terms: CudaTerms): Promise<void>;
  /** Restarts the app, so a changed switch applies. Rejects with `busy` while something runs. */
  restartApp(): Promise<void>;
}
