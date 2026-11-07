import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Tauri loads http://localhost:1420 in development (tauri.conf.json `devUrl`) and the
// built `dist` otherwise.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: "localhost",
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2022",
  },
});
