import type { AgentExecution } from "../lib/api";
import {
  BAR_STYLE,
  STATE_LABEL,
  STATE_STYLE,
  displayName,
  executionElapsedMs,
  formatElapsed,
  isLive,
  isStalled,
  progressPercent,
  queuedMs,
  stateOf,
  usageSummary,
} from "../lib/agentExecutions";

/** 状态徽标（列表卡片与详情页头部共用）：活跃状态带脉动点。 */
export function StateBadge({ e, size = "sm" }: { e: AgentExecution; size?: "sm" | "md" }) {
  const state = stateOf(e);
  const pulse = isLive(e) && (state === "running" || state === "waiting" || state === "cancelling");
  return (
    <span
      className={`inline-flex shrink-0 items-center gap-1 rounded-full ${
        size === "md" ? "px-2 py-0.5 text-[11px]" : "px-1.5 py-0.5 text-[10px]"
      } ${STATE_STYLE[state] ?? "bg-gray-500/10 text-gray-400"}`}
    >
      {pulse && <span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-current" />}
      {STATE_LABEL[state] ?? e.state}
      {isStalled(e) && <span className="text-gray-500">· 待恢复</span>}
    </span>
  );
}

/** 时长文案：在跑 → 实时跳动的运行时长；排队 → 排队时长；终态 → 总运行时长。 */
export function elapsedText(e: AgentExecution, now: number): string | null {
  const queued = queuedMs(e, now);
  if (queued !== null) return `排队 ${formatElapsed(queued)}`;
  const elapsed = executionElapsedMs(e, now);
  if (elapsed === null) return null;
  return isLive(e) ? `已运行 ${formatElapsed(elapsed)}` : `运行 ${formatElapsed(elapsed)}`;
}

/** 进度条（阶段步数 / 迭代上限）。 */
export function ProgressBar({ e, className = "" }: { e: AgentExecution; className?: string }) {
  const state = stateOf(e);
  const pct = progressPercent(e);
  const live = isLive(e);
  return (
    <div className={`h-1 overflow-hidden rounded-full bg-[#1a1f29] ${className}`}>
      <div
        className={`h-full rounded-full transition-[width] duration-500 ${BAR_STYLE[state] ?? "bg-gray-500"} ${
          live && pct < 100 ? "animate-pulse" : ""
        }`}
        style={{ width: `${Math.max(pct, live ? 3 : 0)}%` }}
      />
    </div>
  );
}

/** 一条后台执行的卡片：名字 / 状态 / 实时运行时长 / token / 目标 / 阶段 + 进度条；
 *  点击进入详情。`compact` 为右侧任务面板的窄版。 */
export default function ExecutionCard({
  e,
  now,
  index,
  compact = false,
  onOpen,
}: {
  e: AgentExecution;
  /** 当前时刻（父组件每秒一跳，所有卡片同步）。 */
  now: number;
  /** 列表序号（从 0 起；不传则不显示）。 */
  index?: number;
  compact?: boolean;
  onOpen: (agentId: string) => void;
}) {
  const elapsed = elapsedText(e, now);
  const tokens = usageSummary(e);
  const name = displayName(e);
  const showId = name !== e.agent_id;
  const failed = stateOf(e) === "failed";
  return (
    <button
      type="button"
      data-agent-id={e.agent_id}
      onClick={() => onOpen(e.agent_id)}
      title={e.goal}
      className={`block w-full text-left transition hover:border-[#3a4150] ${
        compact
          ? "rounded-lg border border-[#2a2f3a] bg-[#141821] px-2.5 py-2"
          : "rounded-xl border border-[#2a2f3a] bg-[#141821] px-4 py-3 max-md:px-3"
      }`}
    >
      <div className="flex items-center justify-between gap-2">
        <div className="flex min-w-0 items-center gap-1.5">
          {index !== undefined && (
            <span className="shrink-0 font-mono text-[10px] text-gray-600">
              {String(index + 1).padStart(2, "0")}
            </span>
          )}
          <span className={`truncate ${compact ? "text-[11px]" : "text-xs"} font-medium text-gray-200`}>
            {name}
          </span>
          {showId && !compact && (
            <span className="hidden min-w-0 truncate font-mono text-[10px] text-gray-600 md:inline">
              {e.agent_id}
            </span>
          )}
        </div>
        <StateBadge e={e} />
      </div>
      <div className={`mt-1 truncate ${compact ? "text-[11px]" : "text-xs"} text-gray-400`}>{e.goal}</div>
      <div className="mt-1.5 flex items-center justify-between gap-2 text-[10px] text-gray-500">
        <span className="min-w-0 truncate">
          {e.progress?.stage || "—"}
        </span>
        <span className="flex shrink-0 items-center gap-2 font-mono tabular-nums">
          {elapsed && <span className={isLive(e) ? "text-indigo-300" : ""}>{elapsed}</span>}
          {tokens && <span>{tokens}</span>}
          <span>
            {e.progress?.completed_steps ?? 0}/{e.progress?.total_steps ?? 0} 步
          </span>
        </span>
      </div>
      <ProgressBar e={e} className="mt-1.5" />
      {failed && e.error && (
        <div className="mt-1.5 truncate text-[10px] text-red-400/80" title={e.error}>
          {e.error}
        </div>
      )}
    </button>
  );
}
