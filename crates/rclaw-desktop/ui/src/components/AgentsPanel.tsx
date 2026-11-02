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

/** Agent 状态面板：后台 / 异步执行的子 agent 实时列表（直播仓驱动，事件到达即更新）。
 *
 * 覆盖登记在册的子 agent：Agent 工具的后台任务、自动转后台的前台调用、团队成员。
 * 点任一条进入详情页（实时阶段 / 时长 / token、完整活动记录、与它对话、取消）。 */
export default function AgentsPanel({
  agentReady,
  hidden = false,
  onOpen,
}: {
  agentReady: boolean;
  /** 被详情页盖住（仍挂载以保住滚动位置）：停掉秒表，不白白每秒重渲。 */
  hidden?: boolean;
  /** 打开某条执行的详情。 */
  onOpen: (agentId: string) => void;
}) {
  const { executions, loaded, error } = useAgentExecutions(agentReady);
  const [clearing, setClearing] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const counts = countByState(executions);
  // 有在跑的任务才每秒刷新时长；全是终态 / 面板被盖住时不跑定时器。
  const now = useNow(!hidden && counts.active > 0);

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
        <h1 className="text-base font-semibold text-gray-100">Agent 状态</h1>
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
            {counts.running > 0 ? `${counts.running} 个在运行 · 实时` : "实时"}
            {counts.stalled > 0 && (
              <span className="text-sky-500" title="进程重启后恢复的记录，没有执行者；在对话里用 AgentResume 续跑">
                · {counts.stalled} 个待恢复
              </span>
            )}
          </span>
        </div>
      </div>

      <p className="mb-4 rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-xs text-gray-500">
        这里展示登记在册的子 agent（后台任务、自动转后台的调用、团队成员）的实时进度。
        点开任一条可查看完整活动记录（思考、工具与 Skill 调用）、累计 token 与运行时长，并能直接和它对话或取消。
      </p>

      {(error || actionError) && (
        <div className="mb-4 rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-sm text-red-300">
          {actionError ?? error}
        </div>
      )}
      {!agentReady && (
        <div className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-sm text-gray-500">
          agent 启动后可用。
        </div>
      )}
      {agentReady && loaded && executions.length === 0 && (
        <div className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-sm text-gray-500">
          当前没有后台执行的子 agent。让 RClaw「并行研究 / 后台跑任务」时，这里能看到每个
          agent 的实时状态。
        </div>
      )}

      <div className="space-y-2">
        {executions.map((e, i) => (
          <ExecutionCard key={e.agent_id} e={e} now={now} index={i} onOpen={onOpen} />
        ))}
      </div>
    </div>
  );
}
