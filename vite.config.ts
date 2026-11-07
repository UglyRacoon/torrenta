import { defineConfig } from "vite";

// Tauri ожидает фиксированный порт и не должен мониторить src-tauri.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    host: host || false,
    hmr: host
      ? { protocol: "ws", host, port: 5174 }
      : undefined,
    // Сборка Rust идёт в общий `target/` в корне воркспейса: без этих
    // исключений dev-сервер падает с EBUSY на заблокированных .exe.
    watch: {
      ignored: ["**/src-tauri/**", "**/target/**", "**/dist/**", "**/bundle/**"],
    },
  },
  build: {
    // Tauri использует WebView2 (Chromium) — современный таргет безопасен.
    target: "es2021",
    minify: "esbuild",
    sourcemap: false,
    cssMinify: true,
    reportCompressedSize: false,
  },
});
