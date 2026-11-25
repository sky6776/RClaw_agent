/** 内联 SVG 图标集：16×16、stroke currentColor、strokeWidth 1.3，与 TitleBar 同风格。
 *
 * 侧栏导航 / 面板开关等处使用；新增图标保持同一 viewBox 与线条语言。 */
import type { ReactNode } from "react";

const icon = {
  width: 16,
  height: 16,
  viewBox: "0 0 16 16",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 1.3,
  strokeLinecap: "round",
  strokeLinejoin: "round",
} as const;

function Svg({ children }: { children: ReactNode }) {
  return <svg {...icon}>{children}</svg>;
}

/** 对话（气泡）。 */
export function ChatIcon() {
  return (
    <Svg>
      <path d="M2.5 3.5h11v7h-6l-3 2.6V10.5h-2z" />
    </Svg>
  );
}

/** 工具（扳手）。 */
export function ToolsIcon() {
  return (
    <Svg>
      <path d="M9.5 3a3 3 0 0 0-3.9 3.9L2.5 10v3.5H6l3.1-3.1A3 3 0 0 0 13 6.5L10.8 8.7 8.3 7.2 6.8 4.7 9.5 3z" />
    </Svg>
  );
}

/** 技能（闪电）。 */
export function SkillsIcon() {
  return (
    <Svg>
      <path d="M8.8 1.8 3.5 9h3.4l-.7 5.2L11.5 7H8.1z" />
    </Svg>
  );
}

/** 文件（文件夹）。 */
export function FolderIcon() {
  return (
    <Svg>
      <path d="M1.8 4.2V12a1 1 0 0 0 1 1h10.4a1 1 0 0 0 1-1V6a1 1 0 0 0-1-1H8L6.6 3.2a1 1 0 0 0-.7-.3H2.8a1 1 0 0 0-1 1z" />
    </Svg>
  );
}

/** Agent（机器人头）。 */
/** Bot Mode（双人协作）。 */
export function BotsIcon() {
  return (
    <Svg>
      <circle cx="6" cy="8" r="2.6" />
      <circle cx="12.5" cy="8" r="2.2" />
      <path d="M2.8 13.5c.6-1.8 1.8-2.6 3.2-2.6s2.6.8 3.2 2.6M10.5 13.2c.5-1.3 1.3-1.9 2-1.9 1 0 1.9.7 2.4 1.9" />
    </Svg>
  );
}

export function AgentsIcon() {
  return (
    <Svg>
      <rect x="3" y="5" width="10" height="8" rx="1.6" />
      <path d="M8 5V2.5M5.5 8.2h.01M10.5 8.2h.01M6 10.8h4" />
    </Svg>
  );
}

/** 自进化（循环箭头）。 */
export function EvolutionIcon() {
  return (
    <Svg>
      <path d="M13 8a5 5 0 1 1-1.5-3.5M13 2.5V5h-2.5" />
    </Svg>
  );
}

/** 定时任务（时钟）。 */
export function CronIcon() {
  return (
    <Svg>
      <circle cx="8" cy="8" r="5.8" />
      <path d="M8 4.8V8l2.2 1.6" />
    </Svg>
  );
}

/** 多端同步（两个相对的箭头，各自带箭头尖）。 */
export function SyncIcon() {
  return (
    <Svg>
      <path d="M2.5 5.5h10M10.3 3.3l2.2 2.2-2.2 2.2" />
      <path d="M13.5 10.5h-10M5.7 8.3l-2.2 2.2 2.2 2.2" />
    </Svg>
  );
}

/** 任务概览（清单板）。 */
export function TasksIcon() {
  return (
    <Svg>
      <rect x="2.8" y="2.2" width="10.4" height="11.6" rx="1.6" />
      <path d="M5.4 6.2h5.2M5.4 9h5.2M5.4 11.8h3" />
    </Svg>
  );
}

/** 配置（齿轮）。 */
export function ConfigIcon() {
  return (
    <Svg>
      <circle cx="8" cy="8" r="2" />
      <path d="M8 1.8v2M8 12.2v2M1.8 8h2M12.2 8h2M3.6 3.6l1.4 1.4M11 11l1.4 1.4M12.4 3.6 11 5M5 11l-1.4 1.4" />
    </Svg>
  );
}

/** 关于（圆 i）。 */
export function AboutIcon() {
  return (
    <Svg>
      <circle cx="8" cy="8" r="5.8" />
      <path d="M8 7.2v3.4M8 5.1h.01" />
    </Svg>
  );
}

/** 收起到左侧（双箭头向左）。 */
export function CollapseIcon() {
  return (
    <Svg>
      <path d="M6.5 4 3 8l3.5 4M13 4 9.5 8 13 12" />
    </Svg>
  );
}

/** 展开（双箭头向右）。 */
export function ExpandIcon() {
  return (
    <Svg>
      <path d="M9.5 4 13 8l-3.5 4M3 4l3.5 4L3 12" />
    </Svg>
  );
}

/** 右侧面板开关。 */
export function PanelRightIcon({ active }: { active?: boolean }) {
  return (
    <svg {...icon} fill={active ? "currentColor" : "none"} stroke="currentColor" strokeWidth={1.3}>
      <rect x="2.5" y="3" width="11" height="10" rx="1.4" />
      <path d="M9.5 3v10" />
    </svg>
  );
}

/** 工具动作流水行用小图标（读取 / 搜索）。 */
export function SearchIcon() {
  return (
    <Svg>
      <circle cx="7" cy="7" r="4" />
      <path d="m10.2 10.2 3.3 3.3" />
    </Svg>
  );
}

/** 工具动作流水行用小图标（编辑）。 */
export function PencilIcon() {
  return (
    <Svg>
      <path d="M11.3 2.9a1.6 1.6 0 0 1 2.3 2.3L5.5 13.3l-3 .8.8-3z" />
    </Svg>
  );
}

/** 工具动作流水行用小图标（命令执行）。 */
export function TerminalIcon() {
  return (
    <Svg>
      <rect x="2" y="3" width="12" height="10" rx="1.4" />
      <path d="m5 6.5 2.2 2L5 10.5M9 10.5h2.5" />
    </Svg>
  );
}

/** 执行中齿轮（配合 animate-spin 旋转）。 */
export function GearIcon() {
  return (
    <svg {...icon}>
      <circle cx="8" cy="8" r="2.2" />
      <path d="M8 1.8v2M8 12.2v2M1.8 8h2M12.2 8h2M3.6 3.6l1.4 1.4M11 11l1.4 1.4M12.4 3.6 11 5M5 11l-1.4 1.4" />
    </svg>
  );
}

/** 权限模式（盾牌）。 */
export function ShieldIcon() {
  return (
    <Svg>
      <path d="M8 1.8 13 3.6v4c0 3.2-2.1 5.4-5 6.6-2.9-1.2-5-3.4-5-6.6v-4z" />
    </Svg>
  );
}

/** 消息复制（双叠矩形）。 */
export function CopyIcon() {
  return (
    <Svg>
      <rect x="5.5" y="5.5" width="8" height="8" rx="1.4" />
      <path d="M10.5 3.5h-7a1 1 0 0 0-1 1v7" />
    </Svg>
  );
}

/** 复制成功（对勾）。 */
export function CheckIcon() {
  return (
    <Svg>
      <path d="m3.5 8.5 3 3 6-7" />
    </Svg>
  );
}

/** 引用消息（回复箭头：左指箭头 + 折回正文的曲线）。 */
export function QuoteIcon() {
  return (
    <Svg>
      <path d="M6.4 5.4 2.6 8.4l3.8 3" />
      <path d="M2.6 8.4h6.2a3.6 3.6 0 0 1 3.6 3.6v1.2" />
    </Svg>
  );
}

/** 分叉到新会话（一条主线分出东北 / 东南两支，缓弧过渡 + 实心箭头）。 */
export function ForkIcon() {
  return (
    <Svg>
      {/* 主干：左下 → 分叉点，轻微外弓 */}
      <path d="M2.4 13.6Q4.7 12.7 6 10" />
      {/* 上支：分叉点 → 东北（向上缓弧） */}
      <path d="M6 10Q8.6 6.6 11.9 4.1" />
      {/* 下支：分叉点 → 东南（向下缓弧） */}
      <path d="M6 10Q9.2 11.9 11.9 12.1" />
      <path d="M10.4 2.4h3.2v3.2z" fill="currentColor" stroke="none" />
      <path d="M10.4 13.6h3.2v-3.2z" fill="currentColor" stroke="none" />
    </Svg>
  );
}
