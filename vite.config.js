import { defineConfig } from "vite";

// Tauri expects a fixed dev port and no clearScreen so Rust logs stay visible.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: "es2021",
    // Vite 8's bundler is rolldown/oxc. Asking for the old esbuild minifier
    // here would pull esbuild back in as a separate dependency.
    minify: "oxc",
    sourcemap: false,
  },
});
