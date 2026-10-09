import { useEffect, useState } from "react";
import { api, type AgentExecution } from "../lib/api";

const STATE_LABEL: Record<string, string> = {
  queued: "排队中",
  running: "运行中",
  waiting: "等待输入",
  cancelling: "正在取消",
  succeeded: "已完成",
  failed: "失败",
  cancelled: "已取消",
};

const STATE_STYLE: Record<string, string> = {
  queued: "bg-gray-500/10 text-gray-400",
  running: "bg-amber-500/10 text-amber-400",
  waiting: "bg-sky-500/10 text-sky-400",
  cancelling: "bg-orange-500/10 text-orange-400",
  succeeded: "bg-emerald-500/10 text-emerald-400",
  failed: "bg-red-500/10 text-red-400",
  cancelled: "bg-gray-500/10 text-gray-500",
};

function stateOf(e: AgentExecution): string {
  return e.state?.toLowerCase?.() ?? "";
}

/** 任务概览详情页（移动端侧边栏入口）：概览四格 + 后台子 agent 执行列表。
 *
 * 与右侧任务面板同源数据（agentsList，2.5s 轮询）；桌面侧仍由 TaskPanel
 * 承担，本页面双向可用但入口只在移动端抽屉。 */
export default function TaskOverviewPanel({ agentReady }: { agentReady: boolean }) {
  const [executions, setExecutions] = useState<AgentExecution[]>([]);
  const [error, setError] = useState<string | null>(null);

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

  const total = executions.length;
  const done = executions.filter((e) => stateOf(e) === "succeeded").length;
  const active = executions.filter((e) =>
    ["running", "waiting", "cancelling"].includes(stateOf(e)),
  ).length;
  const queued = executions.filter((e) => stateOf(e) === "queued").length;
  const failed = executions.filter((e) => stateOf(e) === "failed").length;

  return (
    <div className="h-full overflow-y-auto p-6 max-md:p-3">
      <div className="mb-4 flex items-center justify-between">
        <h1 className="text-base font-semibold text-gray-100">任务概览</h1>
        <span className="text-xs text-gray-600">每 2.5 秒自动刷新</span>
      </div>

      <div className="mb-4 grid grid-cols-4 gap-2">
        {(
          [
            ["已完成", done, "text-emerald-400"],
            ["进行中", active, "text-amber-400"],
            ["排队中", queued, "text-sky-400"],
            ["失败", failed, "text-red-400"],
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

      {error && (
        <div className="mb-4 rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-sm text-red-300">
          {error}
        </div>
      )}

      {total === 0 ? (
        <div className="rounded-xl border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-xs leading-relaxed text-gray-500">
          暂无后台任务。让 RClaw「后台 / 并行跑任务」时，这里能看到每个子 agent
          的实时状态（目标、当前步骤、进度）。
        </div>
      ) : (
        <div className="space-y-2">
          {executions.map((e, i) => {
            const state = stateOf(e);
            return (
              <div
                key={e.agent_id}
                className="rounded-xl border border-[#2a2f3a] bg-[#141821] px-4 py-3"
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="truncate font-mono text-[10px] text-gray-500">
                    {String(i + 1).padStart(2, "0")} · {e.agent_id}
                  </span>
                  <span
                    className={`shrink-0 rounded-full px-2 py-0.5 text-[10px] ${
                      STATE_STYLE[state] ?? "bg-gray-500/10 text-gray-400"
                    }`}
                  >
                    {STATE_LABEL[state] ?? e.state}
                  </span>
                </div>
                <div className="mt-1 text-xs text-gray-400">{e.goal}</div>
                <div className="mt-1 truncate text-[11px] text-gray-600">
                  {e.progress?.stage || "—"} · {e.progress?.completed_steps ?? 0}/
                  {e.progress?.total_steps ?? 0} 步
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
