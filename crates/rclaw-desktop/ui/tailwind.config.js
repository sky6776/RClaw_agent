/** @type {import('tailwindcss').Config} */
// Tailwind 3.4（v4 需要 Chrome 111+ 的 cascade layers/@property，真机 WebView 83
// 全部丢弃导致样式失效；v3 兼容 Chrome 60+）。类名与 v4 用法保持通用。
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  // hover 仅在真正支持悬停的设备生效：触屏 tap 后不残留 hover 底色。
  future: {
    hoverOnlyWhenSupported: true,
  },
  theme: {
    extend: {},
  },
  plugins: [],
};
