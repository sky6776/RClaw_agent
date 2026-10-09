import { useEffect, useState } from "react";

/** 移动端判定：<768px 视口（Android 手机宽度）。
 * 桌面窗口宽度 >768 恒为 false，样式/布局分支对桌面零影响。 */
export function useIsMobile(): boolean {
  const [mobile, setMobile] = useState(() =>
    typeof window !== "undefined"
      ? window.matchMedia("(max-width: 767px)").matches
      : false,
  );
  useEffect(() => {
    const mq = window.matchMedia("(max-width: 767px)");
    const onChange = (e: MediaQueryListEvent) => setMobile(e.matches);
    // 老 WebView（83 内核）的 MediaQueryList 未继承 EventTarget，只有
    // addListener/removeListener；直接 addEventListener 会挂载即抛错白屏。
    if (typeof mq.addEventListener === "function") {
      mq.addEventListener("change", onChange);
      return () => mq.removeEventListener("change", onChange);
    }
    const legacy = (e: MediaQueryListEvent | MediaQueryList) => setMobile(e.matches);
    mq.addListener(legacy);
    return () => mq.removeListener(legacy);
  }, []);
  return mobile;
}

/** 安卓 App 判定：Android WebView 的 UA 必含 "Android"，桌面端（Windows/macOS/
 * Linux Tauri）不含。用于只影响安卓 App 的 UI 分支（如配置导入入口），
 * 桌面版零影响。 */
export function isAndroidDevice(): boolean {
  return typeof navigator !== "undefined" && /Android/.test(navigator.userAgent);
}
