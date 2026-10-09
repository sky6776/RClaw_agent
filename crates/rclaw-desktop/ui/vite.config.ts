import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 桌面壳的前端构建：产物输出 dist/，由 tauri 在编译期内嵌进 exe。
// Tailwind 走 PostCSS（v3），不经 vite 插件——兼容真机 WebView 83。
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    outDir: "dist",
    // 真机（vivo, Android 10）系统 WebView 83：ES2021 语法（??=、.at() 等）
    // 部分不支持；降到 chrome83 让 esbuild 全量转译（含 node_modules 依赖）。
    target: "chrome83",
  },
});
