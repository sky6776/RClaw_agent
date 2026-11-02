import { useEffect, useState } from "react";
import { api, type UsageSnapshot } from "../lib/api";
import {
  countByState,
  dropTerminalLocally,
  refreshExecutions,
  useAgentExecutions,
  useNow,
} from "../lib/agentExecutions";
import ExecutionCard from "./ExecutionCard";

/** 右侧任务面板：任务概览（后台子 agent 执行，实时）+ 底部状态（上下文窗口 / 内存）。
 *
 * 任务列表与 AgentsPanel 同一份直播仓（事件驱动 + 低频兜底）；状态区 5s 轮询。
 * 仅在 agentReady 且面板可见时挂载（由 App 控制），卸载即停止轮询。 */
export default function TaskPanel({
  agentReady,
  activeChatId,
  onOpenAgents,
  width,
}: {
  agentReady: boolean;
  activeChatId: string;
  /** 打开 Agent 页；带 agentId 时直接进入该条执行的详情。 */
  onOpenAgents: (agentId?: string) => void;
  /** 面板宽度（px；App 侧拖拽手柄调整并持久化）。 */
  width: number;
}) {
  const { executions } = useAgentExecutions(agentReady);
  const [usage, setUsage] = useState<UsageSnapshot | null>(null);
  const [clearing, setClearing] = useState(false);
  const counts = countByState(executions);
  const now = useNow(counts.active > 0);

  // 上下文 / 内存轮询（估算要扫技能目录，频率放宽）。
  useEffect(() => {
    if (!agentReady) return;
    let stopped = false;
    const tick = async () => {
      try {
        const snapshot = await api.usageSnapshot(activeChatId);
        if (!stopped) setUsage(snapshot);
      } catch {
        // agent 未启动等瞬时错误：保留上次快照。
      }
    };
    void tick();
    const timer = setInterval(tick, 5000);
    return () => {
      stopped = true;
      clearInterval(timer);
    };
  }, [agentReady, activeChatId]);

  // 概览四格：succeeded / running+waiting+cancelling / queued / failed。
  const { total, done, running: active, queued, failed, finished: finishedCount } = counts;

  // 清除历史（与 Agent 状态页 / 任务概览页同源命令）：只清已终态的任务记录，
  // 不影响任何会话；活任务保留。本地立即过滤，removed 事件 / 重拉对齐。
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
    } catch {
      // 下一次拉取带回真实状态。
    } finally {
      setClearing(false);
    }
  };

  const capacity = usage?.context_length ?? 0;
  const used = usage?.request_used ?? 0;
  const pct = capacity > 0 ? Math.min(100, (used / capacity) * 100) : 0;
  const rssMb = usage ? usage.rss_bytes / 1024 / 1024 : 0;

  return (
    <div className="flex shrink-0 flex-col border-l border-[#2a2f3a] bg-[#12151d]" style={{ width }}>
      {/* 任务概览 */}
      <div className="flex items-center justify-between px-4 pt-4 pb-2">
        <span className="text-xs font-medium text-gray-400">任务概览</span>
        <div className="flex items-center gap-1">
          <button
            onClick={clearHistory}
            disabled={!agentReady || clearing || finishedCount === 0}
            title="清空已结束的后台任务记录（不影响会话与运行中的任务）"
            className="rounded-md px-1.5 text-xs text-gray-500 transition hover:bg-[#1a1f29] hover:text-indigo-300 disabled:cursor-not-allowed disabled:opacity-40"
          >
            {clearing ? "清除中…" : `清除${finishedCount > 0 ? `（${finishedCount}）` : ""}`}
          </button>
          <button
            onClick={() => onOpenAgents()}
            title="打开 Agent 状态页"
            className="rounded-md px-1.5 text-xs text-gray-500 transition hover:bg-[#1a1f29] hover:text-indigo-300"
          >
            详情
          </button>
        </div>
      </div>
      <div className="px-4">
        <div className="mb-2 flex items-baseline gap-1">
          <span className="text-2xl font-semibold text-gray-100">{done}</span>
          <span className="text-xs text-gray-500">/ {total}</span>
        </div>
        <div className="grid grid-cols-4 gap-1.5">
          {(
            [
              ["已完成", done, "text-emerald-400"],
              ["进行中", active, "text-amber-400"],
              ["等待中", queued, "text-sky-400"],
              ["失败", failed, "text-red-400"],
            ] as Array<[string, number, string]>
          ).map(([label, count, color]) => (
            <div
              key={label}
              className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-1 py-2 text-center"
            >
              <div className={`text-base font-semibold ${count > 0 ? color : "text-gray-500"}`}>
                {count}
              </div>
              <div className="mt-0.5 text-[10px] text-gray-500">{label}</div>
            </div>
          ))}
        </div>
      </div>

      {/* 任务列表 */}
      <div className="mt-3 min-h-0 flex-1 overflow-y-auto px-4 pb-2">
        {total === 0 ? (
          <div className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-3 py-2.5 text-xs leading-relaxed text-gray-500">
            暂无后台任务。让 RClaw「后台 / 并行跑任务」时，这里能看到每个 agent 的实时状态。
          </div>
        ) : (
          <div className="space-y-1.5">
            {executions.map((e, i) => (
              <ExecutionCard
                key={e.agent_id}
                e={e}
                now={now}
                index={i}
                compact
                onOpen={(id) => onOpenAgents(id)}
              />
            ))}
          </div>
        )}
      </div>

      {/* 状态：上下文窗口 + 内存占用 */}
      <div className="border-t border-[#2a2f3a] px-4 py-3">
        <div className="mb-1.5 text-xs font-medium text-gray-400">状态</div>
        <div className="space-y-1.5 text-xs">
          <div>
            <div className="flex items-center justify-between text-gray-500">
              <span>上下文窗口</span>
              <span className="font-mono">
                {capacity > 0
                  ? `${(used / 1000).toFixed(1)}K/${(capacity / 1000).toFixed(1)}K（${pct.toFixed(1)}%）`
                  : "—"}
              </span>
            </div>
            <div className="mt-1 h-1 overflow-hidden rounded-full bg-[#1a1f29]">
              <div
                className={`h-full rounded-full ${pct > 80 ? "bg-red-500" : "bg-indigo-500"}`}
                style={{ width: `${Math.max(pct, used > 0 ? 2 : 0)}%` }}
              />
            </div>
          </div>
          <div className="flex items-center justify-between text-gray-500">
            <span>内存占用</span>
            <span className="font-mono">{rssMb > 0 ? `${rssMb.toFixed(1)} MB` : "—"}</span>
          </div>
        </div>
      </div>
    </div>
  );
}
