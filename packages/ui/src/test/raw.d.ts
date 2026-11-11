// Vite's `?raw` imports, used by tests that read a file as text.
declare module "*?raw" {
  const text: string;
  export default text;
}

// Vite's `import.meta.glob`, as tests use it: every matching file's text, read when the test loads.
interface ImportMeta {
  glob(
    pattern: string | string[],
    options: { query: "?raw"; import: "default"; eager: true },
  ): Record<string, string>;
}
