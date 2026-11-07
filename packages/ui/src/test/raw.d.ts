// Vite's `?raw` imports, used by tests that read a file as text.
declare module "*?raw" {
  const text: string;
  export default text;
}
