/** 活动覆盖层：任务概览 / Agent 执行（留在设置之外，随会话实时变化）。
 *
 * 详情状态与标签绑定（切标签清空 selectedId）；桌面 Esc / 移动返回由 App 统一调度
 * （detail→list→关闭）。Chat 保持挂载。
 */
import { useEffect } from "react";

import { useFocusTrap } from "../lib/focusTrap";

import AgentsPanel from "./AgentsPanel";
import ExecutionPage from "./ExecutionPage";
import TaskOverviewPanel from "./TaskOverviewPanel";

export type ActivityTab = "tasks" | "agents";

export interface ActivityState {
  tab: ActivityTab;
  /** detail 时 selectedId 必填。 */
  view: "list" | "detail";
  selectedId?: string;
}

export default function ActivityOverlay({
  compact,
  state,
  agentReady,
  onTab,
  onOpenDetail,
  onBack,
  onClose,
}: {
  compact: boolean;
  state: ActivityState;
  agentReady: boolean;
  onTab: (tab: ActivityTab) => void;
  onOpenDetail: (tab: ActivityTab, agentId: string) => void;
  onBack: () => void;
  onClose: () => void;
}) {
  const selected = state.view === "detail" ? state.selectedId ?? null : null;
  const rootRef = useFocusTrap<HTMLDivElement>(`${state.tab}:${state.view}:${state.selectedId ?? ""}`);

  useEffect(() => {
    // 切标签/返回时把焦点移到覆盖层容器，避免焦点留在已卸载的卡片上。
    // 具体列表卡片焦点由 ExecutionPage 负责恢复。
  }, [state.tab, state.view]);

  const panel = (tab: ActivityTab) => {
    if (tab === "tasks") {
      return (
        <TaskOverviewPanel
          agentReady={agentReady}
          hidden={selected !== null}
          onOpen={(id) => onOpenDetail("tasks", id)}
        />
      );
    }
    return (
      <AgentsPanel
        agentReady={agentReady}
        hidden={selected !== null}
        onOpen={(id) => onOpenDetail("agents", id)}
      />
    );
  };

  return (
    <div
      ref={rootRef}
      tabIndex={-1}
      className={`flex h-full min-h-0 flex-col bg-[#0f131b] ${
        compact ? "fixed top-0 right-0 bottom-0 left-0 z-40" : "absolute top-0 right-0 bottom-0 left-0 z-40"
      }`}
      role="dialog"
      aria-label="活动"
      aria-modal={compact ? "true" : undefined}
      style={compact ? { paddingTop: "env(safe-area-inset-top)" } : undefined}
    >
      <div className="flex h-11 shrink-0 items-center gap-2 border-b border-[#2a2f3a] px-3">
        {compact && (
          <button
            onClick={onClose}
            title="关闭"
            className="flex h-8 w-8 items-center justify-center rounded-lg text-gray-400 hover:bg-[#1a1f29] hover:text-gray-100"
          >
            ←
          </button>
        )}
        <span className="text-sm font-semibold text-gray-100">活动</span>
        <div className="ml-2 flex gap-1">
          {(
            [
              ["tasks", "任务概览"],
              ["agents", "Agent 执行"],
            ] as Array<[ActivityTab, string]>
          ).map(([tab, label]) => (
            <button
              key={tab}
              onClick={() => onTab(tab)}
              className={`rounded-lg px-2.5 py-1 text-xs transition ${
                state.tab === tab
                  ? "bg-indigo-500/15 text-indigo-200"
                  : "text-gray-400 hover:bg-[#1a1f29] hover:text-gray-200"
              }`}
            >
              {label}
            </button>
          ))}
        </div>
        <div className="flex-1" />
        {!compact && (
          <button
            onClick={onClose}
            title="关闭（Esc）"
            className="flex h-8 w-8 items-center justify-center rounded-lg text-gray-400 hover:bg-[#1a1f29] hover:text-gray-100"
          >
            ✕
          </button>
        )}
      </div>
      <div className="min-h-0 flex-1 overflow-hidden p-2">
        <ExecutionPage selectedAgentId={selected} onBack={onBack}>
          {(hidden) => (
            <div className={hidden ? "invisible" : "h-full"}>{panel(state.tab)}</div>
          )}
        </ExecutionPage>
      </div>
    </div>
  );
}
