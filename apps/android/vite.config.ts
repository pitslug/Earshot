import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// The Android app (T10.2a). Tauri loads http://localhost:1421 in development
// (tauri.conf.json `devUrl`; the desktop's preview has 1420) and the built `dist` otherwise.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1421,
    strictPort: true,
    host: "localhost",
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2022",
  },
});