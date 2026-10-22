import { defineConfig } from "vite";

// The UI lives in ui/; Tauri loads the build from dist/ (or the dev server).
export default defineConfig({
  root: "ui",
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    target: "es2022",
  },
});
