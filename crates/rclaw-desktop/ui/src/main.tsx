// 必须最先执行：垫片要在依赖模块图求值前就位（模块按 import 顺序执行）。
import "./polyfills";
import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./index.css";

// 安卓 App 宿主标记（WebView UA 含 Android）：CSS 据此隐藏滚动条等
// 移动专属样式；桌面版不挂类，规则零命中。
if (/Android/.test(navigator.userAgent)) {
  document.documentElement.classList.add("android-app");
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
