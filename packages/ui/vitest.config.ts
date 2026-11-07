import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
    // Each worker imports Fluent and sets up jsdom, so on a busy host many at once starve each
    // other past the 5 s test timeout. Two ran the whole suite faster than the default (82 s
    // against 117 s, 2026-11-07) and without those timeouts; the timeout itself stays.
    maxWorkers: 2,
  },
});
