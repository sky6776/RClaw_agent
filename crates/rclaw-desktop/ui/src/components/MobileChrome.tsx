import type { ReactNode } from "react";

import { BellIcon, GearIcon } from "./NavIcons";

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

/** 移动端顶栏：汉堡开抽屉 + 标题 + 活动 + 设置。
 * Android 无自绘窗控（系统提供返回/主页），标题栏不可拖拽移动窗口。 */
export default function MobileTopBar({
  title,
  activityCount,
  activityPulse,
  activityHint,
  onOpenDrawer,
  onOpenActivity,
  onOpenSettings,
}: {
  title: string;
  activityCount: number;
  activityPulse: boolean;
  activityHint?: string;
  onOpenDrawer: () => void;
  onOpenActivity: () => void;
  onOpenSettings: () => void;
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
        onClick={onOpenActivity}
        title={activityHint || "活动"}
        className="relative flex h-11 w-11 items-center justify-center rounded-full text-gray-400 transition active:bg-[#1a1f29]"
      >
        <BellIcon />
        {activityCount > 0 && (
          <span
            className={`absolute right-1.5 top-1.5 min-w-3.5 rounded-full bg-amber-400 px-1 text-[9px] leading-3.5 text-black ${
              activityPulse ? "animate-pulse" : ""
            }`}
          >
            {activityCount}
          </span>
        )}
      </button>
      <button
        onClick={onOpenSettings}
        title="设置"
        className="flex h-11 w-11 items-center justify-center rounded-full text-gray-400 transition active:bg-[#1a1f29]"
      >
        <GearIcon />
      </button>
    </div>
  );
}

/** 抽屉遮罩。 */
export function DrawerScrim({ onClose }: { onClose: () => void }) {
  return (
    <div
      className="fixed bottom-0 left-0 right-0 top-0 z-40 bg-black/60"
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
