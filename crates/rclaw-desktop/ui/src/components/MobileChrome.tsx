import type { ReactNode } from "react";

const icon = {
  width: 18,
  height: 18,
  viewBox: "0 0 16 16",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 1.4,
  strokeLinecap: "round",
  strokeLinejoin: "round",
} as const;

/** 移动端顶栏（<768px）：汉堡开抽屉 + 标题 + 主题切换。
 * Android 无自绘窗控（系统提供返回/主页），标题栏不可拖拽移动窗口。 */
export default function MobileTopBar({
  title,
  theme,
  onOpenDrawer,
  onToggleTheme,
}: {
  title: string;
  theme: "dark" | "light";
  onOpenDrawer: () => void;
  onToggleTheme: () => void;
}) {
  return (
    <div
      className="topbar-bg flex h-14 shrink-0 items-center gap-1 border-b border-[#2a2f3a] bg-[#12151d] pl-1.5 pr-2"
      style={{ paddingTop: "env(safe-area-inset-top)" }}
    >
      <button
        onClick={onOpenDrawer}
        title="打开菜单"
        className="flex h-11 w-11 items-center justify-center rounded-full text-gray-300 transition active:bg-[#1a1f29]"
      >
        <svg {...icon}>
          <path d="M2.5 4h11M2.5 8h11M2.5 12h11" />
        </svg>
      </button>
      {/* 品牌图标不放在顶栏（用户偏好）：桌面 TitleBar 不受影响 */}
      <span className="min-w-0 flex-1 truncate px-2 text-[15px] font-medium text-gray-200">
        {title}
      </span>
      <button
        onClick={onToggleTheme}
        title={theme === "dark" ? "切换到浅色主题" : "切换到深色主题"}
        className="flex h-11 w-11 items-center justify-center rounded-full text-gray-400 transition active:bg-[#1a1f29]"
      >
        {theme === "dark" ? (
          <svg {...icon}>
            <circle cx="8" cy="8" r="2.7" />
            <path d="M8 1.5v1.7M8 12.8v1.7M1.5 8h1.7M12.8 8h1.7M3.4 3.4l1.2 1.2M11.4 11.4l1.2 1.2M12.6 3.4l-1.2 1.2M4.6 11.4l-1.2 1.2" />
          </svg>
        ) : (
          <svg {...icon}>
            <path d="M13.5 9.7A5.6 5.6 0 0 1 6.3 2.5a5.6 5.6 0 1 0 7.2 7.2z" />
          </svg>
        )}
      </button>
    </div>
  );
}

/** 抽屉遮罩。 */
export function DrawerScrim({ onClose }: { onClose: () => void }) {
  return (
    <div
      className="fixed inset-0 z-40 bg-black/60"
      style={{ top: "env(safe-area-inset-top)" }}
      onClick={onClose}
    />
  );
}

/** 移动端抽屉容器：左侧滑入，覆盖在主区之上。 */
export function Drawer({ children }: { children: ReactNode }) {
  return (
    <div
      className="sidebar-bg fixed inset-y-0 left-0 z-50 flex w-[17rem] max-w-[85vw] flex-col border-r border-[#2a2f3a] bg-[#12151d] shadow-2xl"
      style={{ paddingTop: "env(safe-area-inset-top)" }}
    >
      {children}
    </div>
  );
}
