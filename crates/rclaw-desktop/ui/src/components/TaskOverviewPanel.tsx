import { useState } from "react";
import { api } from "../lib/api";
import {
  countByState,
  dropTerminalLocally,
  refreshExecutions,
  useAgentExecutions,
  useNow,
} from "../lib/agentExecutions";
import ExecutionCard from "./ExecutionCard";

/** 任务概览详情页（移动端侧边栏入口）：概览四格 + 后台子 agent 执行列表（实时）。
 *
 * 与右侧任务面板 / Agent 状态页同一份直播仓；桌面侧仍由 TaskPanel 承担，本页面双向
 * 可用但入口只在移动端抽屉。点任一条进入执行详情。 */
export default function TaskOverviewPanel({
  agentReady,
  onOpen,
}: {
  agentReady: boolean;
  onOpen: (agentId: string) => void;
}) {
  const { executions, loaded, error } = useAgentExecutions(agentReady);
  const [clearing, setClearing] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const counts = countByState(executions);
  const now = useNow(counts.active > 0);

  const clearHistory = async () => {
    if (
      !window.confirm(
        "清除全部已结束的后台任务记录？（不影响会话；运行中 / 排队 / 等待中的任务保留）",
      )
    ) {
      return;
    }
    setClearing(true);
    try {
      await api.agentsClearHistory();
      dropTerminalLocally();
      void refreshExecutions();
    } catch (e) {
      setActionError(String(e));
    } finally {
      setClearing(false);
    }
  };

  return (
    <div className="h-full overflow-y-auto p-6 max-md:p-3">
      <div className="mb-4 flex items-center justify-between">
        <h1 className="text-base font-semibold text-gray-100">任务概览</h1>
        <div className="flex items-center gap-3">
          <button
            onClick={clearHistory}
            disabled={!agentReady || clearing || counts.finished === 0}
            title="清空已结束的后台任务记录（不影响会话与运行中的任务）"
            className="rounded-lg border border-[#2a2f3a] px-2.5 py-1 text-xs text-gray-400 transition-colors hover:bg-[#1c2230] disabled:cursor-not-allowed disabled:opacity-40"
          >
            {clearing ? "清除中…" : `清除历史${counts.finished > 0 ? `（${counts.finished}）` : ""}`}
          </button>
          <span className="flex items-center gap-1.5 text-xs text-gray-600">
            <span
              className={`inline-block h-1.5 w-1.5 rounded-full ${
                counts.running > 0 ? "animate-pulse bg-amber-400" : "bg-gray-600"
              }`}
            />
            实时
          </span>
        </div>
      </div>

      <div className="mb-4 grid grid-cols-4 gap-2">
        {(
          [
            ["已完成", counts.done, "text-emerald-400"],
            ["进行中", counts.running, "text-amber-400"],
            ["排队中", counts.queued, "text-sky-400"],
            ["失败", counts.failed, "text-red-400"],
          ] as Array<[string, number, string]>
        ).map(([label, count, color]) => (
          <div
            key={label}
            className="rounded-xl border border-[#2a2f3a] bg-[#141821] px-1 py-3 text-center"
          >
            <div className={`text-xl font-semibold ${count > 0 ? color : "text-gray-500"}`}>
              {count}
            </div>
            <div className="mt-0.5 text-[11px] text-gray-500">{label}</div>
          </div>
        ))}
      </div>

      {(error || actionError) && (
        <div className="mb-4 rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-sm text-red-300">
          {actionError ?? error}
        </div>
      )}

      {loaded && executions.length === 0 ? (
        <div className="rounded-xl border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-xs leading-relaxed text-gray-500">
          暂无后台任务。让 RClaw「后台 / 并行跑任务」时，这里能看到每个子 agent
          的实时状态（目标、当前步骤、运行时长、token）；点开可查看完整活动记录并与它对话。
        </div>
      ) : (
        <div className="space-y-2">
          {executions.map((e, i) => (
            <ExecutionCard key={e.agent_id} e={e} now={now} index={i} onOpen={onOpen} />
          ))}
        </div>
      )}
    </div>
  );
}
