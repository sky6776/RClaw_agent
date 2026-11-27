/** 设置覆盖层：分区列表 / 分区详情（桌面双栏，移动端列表→详情两级）。
 *
 * Chat 保持挂载；只有当前分区挂载。error 修复模式只暴露 models/appearance/about
 * （section 的 clamp 由 App 的 openSettings 纯解析完成，这里只按传入值渲染）。
 */
import type { ReactNode } from "react";

import { useFocusTrap } from "../lib/focusTrap";

import {
  BotIcon,
  BrainIcon,
  ClockIcon,
  FolderIcon,
  InfoIcon,
  PaletteIcon,
  PuzzleIcon,
  SlidersIcon,
  SyncIcon,
  WrenchIcon,
} from "./NavIcons";

import type {
  ChatMeta,
  ConfigImportOutcome,
  GroupRoom,
  SloganPrefs,
} from "../lib/api";
import type { ThemeId } from "../lib/themeTypes";
import AppearanceSection, { type ThemeChoice } from "./AppearanceSection";
import BotsPanel from "./BotsPanel";
import ConfigPanel from "./ConfigPanel";
import CronPanel from "./CronPanel";
import EvolutionPanel from "./EvolutionPanel";
import FilePanel from "./FilePanel";
import SkillsPanel from "./SkillsPanel";
import SyncPanel from "./SyncPanel";
import ToolsPanel from "./ToolsPanel";

export type SettingsSection =
  | "appearance"
  | "models"
  | "bots"
  | "tools"
  | "skills"
  | "memory"
  | "cron"
  | "sync"
  | "files"
  | "about";

export interface SettingsState {
  view: "list" | "section";
  section: SettingsSection;
}

const SECTION_META: Array<{
  id: SettingsSection;
  label: string;
  desc: string;
  icon: () => ReactNode;
}> = [
  { id: "appearance", label: "通用与外观", desc: "主题、会话文字大小", icon: PaletteIcon },
  { id: "models", label: "模型与配置", desc: "provider、模型、搜索、压缩与主配置", icon: SlidersIcon },
  { id: "bots", label: "智能体", desc: "bot 花名册与群聊管理", icon: BotIcon },
  { id: "tools", label: "工具", desc: "工具开关与说明", icon: WrenchIcon },
  { id: "skills", label: "技能", desc: "技能列表与操作", icon: PuzzleIcon },
  { id: "memory", label: "记忆与自进化", desc: "长期记忆、自进化回顾", icon: BrainIcon },
  { id: "cron", label: "定时任务", desc: "cron 任务列表与编辑", icon: ClockIcon },
  { id: "sync", label: "多端同步", desc: "设备、空间与回收站", icon: SyncIcon },
  { id: "files", label: "运行目录", desc: "Agent 数据与产物（<home>/agent）", icon: FolderIcon },
  { id: "about", label: "关于", desc: "版本、路径与说明", icon: InfoIcon },
];

export default function SettingsOverlay({
  compact,
  state,
  agentState,
  themes,
  theme,
  onTheme,
  chatFont,
  onFont,
  fontMin,
  fontMax,
  fontDefault,
  slogans,
  sloganInterval,
  onSlogansSaved,
  onImportRaw,
  rooms,
  onRoomsChanged,
  onDeleteRoom,
  onOpenChat,
  onOpenRoom,
  syncPending,
  captureBusy,
  onError,
  onSyncOpenChat,
  onEditorDirtyChange,
  onRequestLeave,
  home,
  about,
  onSection,
  onBack,
  onClose,
}: {
  compact: boolean;
  state: SettingsState;
  agentState: "idle" | "starting" | "ready" | "error";
  themes: ThemeChoice[];
  theme: ThemeId;
  onTheme: (id: ThemeId) => void;
  chatFont: number;
  onFont: (value: number) => void;
  fontMin: number;
  fontMax: number;
  fontDefault: number;
  slogans: string[];
  sloganInterval: number;
  onSlogansSaved: (prefs: SloganPrefs) => void;
  onImportRaw: (content: string) => Promise<ConfigImportOutcome>;
  rooms: GroupRoom[];
  onRoomsChanged: () => void;
  onDeleteRoom: (room: GroupRoom) => void;
  onOpenChat: (chatId: string) => void;
  onOpenRoom: (roomId: string) => void;
  syncPending: number;
  captureBusy: boolean;
  onError: (message: string) => void;
  onSyncOpenChat: (chatId: string) => void;
  onEditorDirtyChange: (dirty: boolean) => void;
  onRequestLeave: (after: () => void) => void;
  home: string;
  about: ReactNode;
  onSection: (section: SettingsSection) => void;
  onBack: () => void;
  onClose: () => void;
}) {
  const ready = agentState === "ready";
  const repairMode = agentState === "error";
  const rootRef = useFocusTrap<HTMLDivElement>(`${state.view}:${state.section}`);
  const meta = SECTION_META.find((item) => item.id === state.section) ?? SECTION_META[0];

  const renderSection = () => {
    switch (state.section) {
      case "appearance":
        return (
          <div className="h-full overflow-y-auto">
          <AppearanceSection
            themes={themes}
            theme={theme}
            onTheme={onTheme}
            chatFont={chatFont}
            onFont={onFont}
            fontMin={fontMin}
            fontMax={fontMax}
            fontDefault={fontDefault}
          />
          </div>
        );
      case "models":
        return (
          <ConfigPanel
            agentReady={ready}
            repairMode={repairMode}
            slogans={slogans}
            sloganInterval={sloganInterval}
            onSlogansSaved={onSlogansSaved}
            onImportRaw={onImportRaw}
          />
        );
      case "bots":
        return ready ? (
          <BotsPanel
            agentReady
            rooms={rooms}
            onRoomsChanged={onRoomsChanged}
            onDeleteRoom={onDeleteRoom}
            onOpenChat={onOpenChat}
            onOpenRoom={onOpenRoom}
          />
        ) : null;
      case "tools":
        return ready ? <ToolsPanel agentReady /> : null;
      case "skills":
        return ready ? <SkillsPanel agentReady /> : null;
      case "memory":
        return ready ? <EvolutionPanel agentReady /> : null;
      case "cron":
        return ready ? <CronPanel agentReady /> : null;
      case "sync":
        return ready ? (
          <SyncPanel
            agentReady
            captureBusy={captureBusy}
            onError={onError}
            onOpenChat={onSyncOpenChat}
          />
        ) : null;
      case "files":
        return ready ? (
          <FilePanel
            onError={onError}
            onEditorDirtyChange={onEditorDirtyChange}
            onRequestLeave={onRequestLeave}
          />
        ) : null;
      case "about":
        return <div className="h-full overflow-y-auto">{about}</div>;
      default:
        return null;
    }
  };

  const list = (
    <div className="min-h-0 flex-1 overflow-y-auto p-3">
      <div className="space-y-1">
        {SECTION_META.map((item) => (
          <button
            key={item.id}
            onClick={() => onSection(item.id)}
            className={`flex w-full items-center gap-3 rounded-xl border px-3 py-2.5 text-left transition ${
              state.view === "section" && state.section === item.id
                ? "border-indigo-400/50 bg-indigo-500/10"
                : "border-transparent hover:bg-[#1a1f29]"
            }`}
          >
            <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-[#1a1f29] text-gray-400">
              <item.icon />
            </span>
            <span className="min-w-0 flex-1">
              <span className="flex items-center gap-1.5">
                <span className="truncate text-sm text-gray-200">{item.label}</span>
                {item.id === "sync" && syncPending > 0 && (
                  <span className="rounded-full bg-amber-500/15 px-1.5 text-[10px] text-amber-300">
                    {syncPending}
                  </span>
                )}
              </span>
              <span className="block truncate text-[11px] text-gray-500">{item.desc}</span>
            </span>
            <span className="text-gray-600">›</span>
          </button>
        ))}
      </div>
    </div>
  );

  const chrome = (body: ReactNode, title: string, showBack: boolean) => (
    <div
      ref={rootRef}
      tabIndex={-1}
      className={`fixed left-0 right-0 bottom-0 z-40 flex min-h-0 flex-col bg-[#0f131b] ${
        compact ? "top-0" : "top-9"
      }`}
      role="dialog"
      aria-label="设置"
      aria-modal={compact ? "true" : undefined}
      style={compact ? { paddingTop: "env(safe-area-inset-top)" } : undefined}
    >
      <div className="flex h-11 shrink-0 items-center gap-2 border-b border-[#2a2f3a] px-3">
        {showBack ? (
          <button
            onClick={onBack}
            title="返回"
            className="flex h-8 w-8 items-center justify-center rounded-lg text-gray-400 hover:bg-[#1a1f29] hover:text-gray-100"
          >
            ←
          </button>
        ) : (
          compact && (
            <button
              onClick={onClose}
              title="关闭设置"
              className="flex h-8 w-8 items-center justify-center rounded-lg text-gray-400 hover:bg-[#1a1f29] hover:text-gray-100"
            >
              ✕
            </button>
          )
        )}
        <span className="min-w-0 flex-1 truncate text-sm font-semibold text-gray-100">
          {title}
        </span>
        {!compact && (
          <button
            onClick={onClose}
            title="关闭设置（Esc）"
            className="flex h-8 w-8 items-center justify-center rounded-lg text-gray-400 hover:bg-[#1a1f29] hover:text-gray-100"
          >
            ✕
          </button>
        )}
      </div>
      <div className="min-h-0 flex-1 overflow-hidden">{body}</div>
    </div>
  );

  if (compact) {
    if (state.view === "list") return chrome(list, "设置", false);
    return chrome(
      <div className="flex h-full min-h-0 flex-col">
        <div className="min-h-0 flex-1 overflow-hidden">{renderSection()}</div>
      </div>,
      meta.label,
      true,
    );
  }

  return chrome(
    <div className="flex h-full min-h-0">
      <div className="flex w-56 shrink-0 flex-col border-r border-[#2a2f3a] bg-[#12151d]">
        <div className="px-3 py-2 text-[11px] uppercase tracking-wide text-gray-600">设置</div>
        <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-2">
          {SECTION_META.map((item) => (
            <button
              key={item.id}
              onClick={() => onSection(item.id)}
              className={`mb-0.5 flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-xs transition ${
                state.section === item.id
                  ? "bg-indigo-500/10 text-indigo-200"
                  : "text-gray-400 hover:bg-[#1a1f29] hover:text-gray-200"
              }`}
            >
              <span className="flex w-4 shrink-0 items-center justify-center">
                <item.icon />
              </span>
              <span className="min-w-0 flex-1 truncate">{item.label}</span>
              {item.id === "sync" && syncPending > 0 && (
                <span className="rounded-full bg-amber-500/15 px-1.5 text-[10px] text-amber-300">
                  {syncPending}
                </span>
              )}
            </button>
          ))}
        </div>
      </div>
      <div className="min-h-0 min-w-0 flex-1 overflow-hidden">{renderSection()}</div>
    </div>,
    meta.label,
    false,
  );
}
