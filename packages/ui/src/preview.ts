// @earshot/ui/preview: what the browser preview needs of the mock `Ipc` (T12.54). The desktop app
// loads it with a dynamic `import()` only outside Tauri, so the app's own bundle starts without it.

export { createMockIpc } from "./ipc/mock";
export { samplePersons } from "./ipc/mockDetail";
