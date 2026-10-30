import { useEffect, useState } from "react";
import { api, type AgentExecution } from "../lib/api";

const STATE_STYLE: Record<string, string> = {
  queued: "bg-gray-500/10 text-gray-400",
  running: "bg-amber-500/10 text-amber-400",
  waiting: "bg-sky-500/10 text-sky-400",
  cancelling: "bg-orange-500/10 text-orange-400",
  succeeded: "bg-emerald-500/10 text-emerald-400",
  failed: "bg-red-500/10 text-red-400",
  cancelled: "bg-gray-500/10 text-gray-500",
};

const STATE_LABEL: Record<string, string> = {
  queued: "排队中",
  running: "运行中",
  waiting: "等待中",
  cancelling: "正在取消",
  succeeded: "已完成",
  failed: "失败",
  cancelled: "已取消",
};

/** 已终态（可被「清除历史」清掉的记录）。 */
const TERMINAL_STATES = new Set(["succeeded", "failed", "cancelled"]);

/** Agent 状态面板：轮询后台子 agent 执行快照。
 *
 * 只覆盖后台/异步执行的子 agent（Agent 工具的 run_in_background / 团队成员）；
 * 前台并行子 agent 在父回合内同步运行，其进度体现在对话状态条中。
 */
export default function AgentsPanel({ agentReady }: { agentReady: boolean }) {
  const [executions, setExecutions] = useState<AgentExecution[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [clearing, setClearing] = useState(false);

  useEffect(() => {
    if (!agentReady) return;
    let stopped = false;
    const tick = async () => {
      try {
        const payload = await api.agentsList();
        if (!stopped) {
          setExecutions(payload.executions ?? []);
          setError(null);
        }
      } catch (e) {
        if (!stopped) setError(String(e));
      }
    };
    void tick();
    const timer = setInterval(tick, 2500);
    return () => {
      stopped = true;
      clearInterval(timer);
    };
  }, [agentReady]);

  const finishedCount = executions.filter((e) =>
    TERMINAL_STATES.has(e.state?.toLowerCase?.() ?? ""),
  ).length;

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
      setExecutions((prev) =>
        prev.filter((e) => !TERMINAL_STATES.has(e.state?.toLowerCase?.() ?? "")),
      );
    } catch (e) {
      setError(String(e));
    } finally {
      setClearing(false);
    }
  };

  return (
    <div className="h-full overflow-y-auto p-6 max-md:p-3">
      <div className="mb-4 flex items-center justify-between">
        <h1 className="text-base font-semibold text-gray-100">Agent 状态</h1>
        <div className="flex items-center gap-3">
          <button
            onClick={clearHistory}
            disabled={!agentReady || clearing || finishedCount === 0}
            title="清空已结束的后台任务记录（不影响会话与运行中的任务）"
            className="rounded-lg border border-[#2a2f3a] px-2.5 py-1 text-xs text-gray-400 transition-colors hover:bg-[#1c2230] disabled:cursor-not-allowed disabled:opacity-40"
          >
            {clearing ? "清除中…" : `清除历史${finishedCount > 0 ? `（${finishedCount}）` : ""}`}
          </button>
          <span className="text-xs text-gray-600">每 2.5 秒自动刷新</span>
        </div>
      </div>

      <p className="mb-4 rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-xs text-gray-500">
        这里展示后台 / 异步执行的子 agent（run_in_background、团队成员）。前台并行子
        agent 在当前回合内同步运行，进度直接体现在对话状态条里。
      </p>

      {error && (
        <div className="mb-4 rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-sm text-red-300">
          {error}
        </div>
      )}
      {!agentReady && (
        <div className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-sm text-gray-500">
          agent 启动后可用。
        </div>
      )}
      {agentReady && executions.length === 0 && (
        <div className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-sm text-gray-500">
          当前没有后台执行的子 agent。让 RClaw「并行研究 / 后台跑任务」时，这里能看到每个
          agent 的实时状态。
        </div>
      )}

      <div className="space-y-2">
        {executions.map((e) => {
          const state = e.state?.toLowerCase?.() ?? "";
          return (
            <div
              key={e.agent_id}
              className="rounded-xl border border-[#2a2f3a] bg-[#141821] px-4 py-3"
            >
              <div className="flex items-center justify-between">
                <div className="flex min-w-0 items-center gap-2">
                  <span className="truncate font-mono text-xs text-gray-400">{e.agent_id}</span>
                  <span
                    className={`rounded-full px-2 py-0.5 text-[10px] ${
                      STATE_STYLE[state] ?? "bg-gray-500/10 text-gray-400"
                    }`}
                  >
                    {STATE_LABEL[state] ?? e.state}
                  </span>
                </div>
                <span className="shrink-0 text-[10px] text-gray-600">
                  {e.progress?.completed_steps ?? 0}/{e.progress?.total_steps ?? 0} 步
                </span>
              </div>
              <div className="mt-1 truncate text-xs text-gray-500">
                {e.progress?.stage || "—"}
              </div>
              <div className="mt-1 truncate text-xs text-gray-400">{e.goal}</div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
