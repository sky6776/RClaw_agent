/** 导航/设置的单色线条图标（不用彩色 emoji，风格对齐 ZCode / Cursor）。
 *
 * 全部 stroke=currentColor：默认灰、激活态跟随文字色（indigo）。
 */
import type { ReactNode } from "react";

const base = {
  width: 16,
  height: 16,
  viewBox: "0 0 16 16",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 1.4,
  strokeLinecap: "round",
  strokeLinejoin: "round",
} as const;

export function PaletteIcon() {
  return (
    <svg {...base}>
      <path d="M8 2.2a5.8 5.8 0 1 0 0 11.6c.9 0 1.3-.6 1.3-1.2 0-.4-.2-.7-.5-1-.2-.3-.4-.6-.4-1 0-.7.5-1.2 1.2-1.2h1.2A3 3 0 0 0 14 6.5C14 4 11.3 2.2 8 2.2z" />
      <circle cx="5.2" cy="6" r="0.8" fill="currentColor" stroke="none" />
      <circle cx="7.6" cy="4.6" r="0.8" fill="currentColor" stroke="none" />
      <circle cx="10.2" cy="5.6" r="0.8" fill="currentColor" stroke="none" />
    </svg>
  );
}

export function SlidersIcon() {
  return (
    <svg {...base}>
      <path d="M3 4.5h10M3 8h10M3 11.5h10" />
      <circle cx="6" cy="4.5" r="1.5" fill="currentColor" stroke="none" />
      <circle cx="10" cy="8" r="1.5" fill="currentColor" stroke="none" />
      <circle cx="5" cy="11.5" r="1.5" fill="currentColor" stroke="none" />
    </svg>
  );
}

export function BotIcon() {
  return (
    <svg {...base}>
      <rect x="3" y="5" width="10" height="8" rx="2" />
      <path d="M8 5V2.8M6.4 2.8h3.2" />
      <circle cx="6" cy="9" r="0.9" fill="currentColor" stroke="none" />
      <circle cx="10" cy="9" r="0.9" fill="currentColor" stroke="none" />
    </svg>
  );
}

export function WrenchIcon() {
  return (
    <svg {...base}>
      <path d="M9.8 3.2a3 3 0 0 0-3.6 3.9L3 10.3a1.6 1.6 0 0 0 2.3 2.3l3.2-3.2a3 3 0 0 0 3.9-3.6l-2 2-1.8-.4-.4-1.8 1.6-1.4z" />
    </svg>
  );
}

export function PuzzleIcon() {
  return (
    <svg {...base}>
      <path d="M6 2.8h2.2v1.4a1.2 1.2 0 1 0 2.4 0V2.8H13v3.4h-1.3a1.2 1.2 0 1 0 0 2.4H13v3.4H9.4v-1.3a1.2 1.2 0 1 0-2.4 0v1.3H3V8.6h1.5a1.2 1.2 0 1 0 0-2.4H3V2.8h3z" />
    </svg>
  );
}

export function BrainIcon() {
  return (
    <svg {...base}>
      <path d="M6.2 2.6a2 2 0 0 0-2 2 2.1 2.1 0 0 0-1.4 2c0 .6.3 1.2.7 1.6-.2.4-.4.8-.4 1.3A2.4 2.4 0 0 0 5.5 12c.3.8 1 1.4 1.9 1.4.5 0 .9-.2 1.2-.5V3.4a1.8 1.8 0 0 0-1.2-.8z" />
      <path d="M9.8 2.6a2 2 0 0 1 2 2 2.1 2.1 0 0 1 1.4 2c0 .6-.3 1.2-.7 1.6.2.4.4.8.4 1.3a2.4 2.4 0 0 1-2.4 2.5c-.3.8-1 1.4-1.9 1.4-.5 0-.9-.2-1.2-.5V3.4a1.8 1.8 0 0 1 1.2-.8z" />
    </svg>
  );
}

export function ClockIcon() {
  return (
    <svg {...base}>
      <circle cx="8" cy="8" r="5.7" />
      <path d="M8 4.6V8l2.2 1.4" />
    </svg>
  );
}

export function SyncIcon() {
  return (
    <svg {...base}>
      <path d="M3.2 6.4A5 5 0 0 1 12 4.2M12.8 9.6A5 5 0 0 1 4 11.8" />
      <path d="M10.8 2.6l1.6 1.6-1.6 1.6M5.2 13.4l-1.6-1.6 1.6-1.6" />
    </svg>
  );
}

export function FolderIcon() {
  return (
    <svg {...base}>
      <path d="M2.8 4.2c0-.7.5-1.2 1.2-1.2h2.5l1.2 1.4H12c.7 0 1.2.5 1.2 1.2v5.6c0 .7-.5 1.2-1.2 1.2H4c-.7 0-1.2-.5-1.2-1.2V4.2z" />
    </svg>
  );
}

export function InfoIcon() {
  return (
    <svg {...base}>
      <circle cx="8" cy="8" r="5.7" />
      <path d="M8 7.2v3.4" />
      <circle cx="8" cy="5.2" r="0.8" fill="currentColor" stroke="none" />
    </svg>
  );
}

export function SearchIcon() {
  return (
    <svg {...base}>
      <circle cx="7" cy="7" r="4.2" />
      <path d="M10.2 10.2L13.4 13.4" />
    </svg>
  );
}

/** 会话：带文本行的对话气泡（顶栏打开会话抽屉）。 */
export function ChatsIcon() {
  return (
    <svg {...base}>
      <path d="M13.6 4.4c0-.8-.7-1.5-1.5-1.5H3.9c-.8 0-1.5.7-1.5 1.5v4.1c0 .8.7 1.5 1.5 1.5h.8v2.6L7.5 10h4.6c.8 0 1.5-.7 1.5-1.5V4.4z" />
      <path d="M5.2 5.4h5.6M5.2 7.4h3.5" />
    </svg>
  );
}

export function PlusIcon() {
  return (
    <svg {...base}>
      <path d="M8 3.2v9.6M3.2 8h9.6" />
    </svg>
  );
}

/** 导入：箭头落入托盘。 */
export function ImportIcon() {
  return (
    <svg {...base}>
      <path d="M8 2.6v6.2M5.4 6.4L8 9l2.6-2.6" />
      <path d="M3.2 10.6v1.6c0 .7.5 1.2 1.2 1.2h7.2c.7 0 1.2-.5 1.2-1.2v-1.6" />
    </svg>
  );
}

export function BellIcon() {
  return (
    <svg {...base}>
      <path d="M4 11.2h8l-1-1.6V6.8a3 3 0 0 0-6 0v2.8l-1 1.6z" />
      <path d="M6.6 12.6a1.5 1.5 0 0 0 2.8 0" />
    </svg>
  );
}

export function GearIcon() {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.7"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
      <circle cx="12" cy="12" r="3" />
    </svg>
  );
}

export function UsersIcon() {
  return (
    <svg {...base}>
      <circle cx="6" cy="6" r="2.2" />
      <path d="M2.6 12.6c0-1.9 1.5-3.2 3.4-3.2s3.4 1.3 3.4 3.2" />
      <path d="M10.6 4.2a2 2 0 0 1 0 3.8M11.2 9.6c1.3.3 2.2 1.4 2.2 3" />
    </svg>
  );
}

export function ActivityIcon() {
  return (
    <svg {...base}>
      <path d="M2.6 8.4h2.2l1.4-3.2 2 6.2 1.4-3h3.8" />
    </svg>
  );
}

export type IconRenderer = () => ReactNode;

export function GlobeIcon() {
  return (
    <svg {...base}>
      <circle cx="8" cy="8" r="5.7" />
      <path d="M2.4 8h11.2M8 2.3c1.7 1.6 2.6 3.5 2.6 5.7S9.7 12.1 8 13.7C6.3 12.1 5.4 10.2 5.4 8S6.3 3.9 8 2.3z" />
    </svg>
  );
}
