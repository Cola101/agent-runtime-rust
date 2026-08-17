import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Fixed port and no auto-open: Tauri points its webview here in development,
// so the port is part of the contract between the two halves rather than
// whatever happened to be free.
export default defineConfig({
  // Relative, not "/": the webview resolves these through Tauri's asset
  // handler rather than an HTTP server, and an absolute path there depends on
  // the handler normalising it the same way a server would.
  base: "./",
  plugins: [react()],
  clearScreen: false,
  server: { port: 5273, strictPort: true },
  build: { outDir: "dist", emptyOutDir: true, target: "safari15" },
});
