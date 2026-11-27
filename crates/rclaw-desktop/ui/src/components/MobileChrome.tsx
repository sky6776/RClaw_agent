import type { ReactNode } from "react";

import { ActivityIcon, ChatsIcon, GearIcon } from "./NavIcons";

/** 移动端顶栏：会话列表入口 + 标题 + 活动 + 设置。
 * Android 无自绘窗控（系统提供返回/主页），标题栏不可拖拽移动窗口。 */
export default function MobileTopBar({
  title,
  activityCount,
  activityPulse,
  activityHint,
  syncPending,
  onOpenDrawer,
  onOpenActivity,
  onOpenSettings,
}: {
  title: string;
  activityCount: number;
  activityPulse: boolean;
  activityHint?: string;
  syncPending?: number;
  onOpenDrawer: () => void;
  onOpenActivity: () => void;
  onOpenSettings: () => void;
}) {
  return (
    <div
      className="topbar-bg flex min-h-12 shrink-0 items-center gap-0.5 border-b border-[#2a2f3a] bg-[#12151d] pl-1 pr-1.5"
      style={{ paddingTop: "env(safe-area-inset-top)" }}
    >
      <button
        onClick={onOpenDrawer}
        title="打开会话列表"
        aria-label="打开会话列表"
        className="flex h-11 w-11 items-center justify-center rounded-full text-gray-300 transition active:bg-[#1a1f29]"
      >
        <ChatsIcon />
      </button>
      {/* 品牌图标不放在顶栏（用户偏好）：桌面 TitleBar 不受影响 */}
      <span className="min-w-0 flex-1 truncate px-1.5 text-[14px] font-medium text-gray-200">
        {title}
      </span>
      <button
        onClick={onOpenActivity}
        title={activityHint || "活动"}
        aria-label={activityHint || "活动"}
        className="relative flex h-11 w-11 items-center justify-center rounded-full text-gray-400 transition active:bg-[#1a1f29]"
      >
        <ActivityIcon />
        {activityCount > 0 && (
          <span
            className={`absolute right-1 top-1 min-w-3.5 rounded-full bg-amber-400 px-1 text-[9px] leading-[14px] text-black ${
              activityPulse ? "animate-pulse" : ""
            }`}
          >
            {activityCount}
          </span>
        )}
      </button>
      <button
        onClick={onOpenSettings}
        title={syncPending && syncPending > 0 ? `设置（${syncPending} 件多端同步事项待处理）` : "设置"}
        aria-label={
          syncPending && syncPending > 0 ? `设置，${syncPending} 件多端同步事项待处理` : "设置"
        }
        className="flex h-11 w-11 items-center justify-center rounded-full text-gray-400 transition active:bg-[#1a1f29]"
      >
        <span className="relative flex items-center">
          <GearIcon />
          {syncPending !== undefined && syncPending > 0 && (
            <span
              aria-hidden="true"
              className="absolute -right-2 -top-1 min-w-3 rounded-full bg-amber-400 px-0.5 text-[9px] leading-3 text-black"
            >
              {syncPending}
            </span>
          )}
        </span>
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
