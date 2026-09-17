import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 期望固定的端口与严格模式；dev server 仅供 Tauri 内嵌窗口使用。
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: {
      // 不要监听 src-tauri 下的 Rust 源码，避免触发前端热重载
      ignored: ["**/src-tauri/**"],
    },
  },
  // Tauri 使用环境变量做编译期注入，保留前缀
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "es2021",
    minify: "esbuild",
    sourcemap: false,
  },
});
