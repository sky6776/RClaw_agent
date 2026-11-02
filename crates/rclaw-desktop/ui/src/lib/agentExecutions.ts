/**
 * 子 agent 执行登记表的前端直播仓（模块级单例）：侧边栏「Agent / 任务概览」徽标、
 * 右侧任务面板、Agent 状态页、任务概览页与执行详情页共用同一份数据。
 *
 * 数据流：首次订阅时拉一次全量（`agentsList`），之后靠运行时 `agent_exec` 事件逐条
 * 更新（snapshot / removed）；事件只是加速，仍保留低频兜底轮询（窗口可见时每 15s）
 * 与 `resync`（宿主广播滞后）触发的全量重拉。全量重拉按 `updated_at` 合并：事件已带来
 * 的更新快照不会被慢一步到达的旧列表盖回去。
 *
 * 详情页另需的逐执行事件（在跑的工具批 / 活动记录新行游标）经 [`subscribeAgentEvents`]
 * 分发，不进全局状态。
 */
import { useEffect, useState, useSyncExternalStore } from "react";
import { api, type AgentExecEvent, type AgentExecution } from "./api";

export interface ExecutionsState {
  /** 按创建时间倒序（与后端列表同序）。 */
  executions: AgentExecution[];
  /** 已成功拉到过至少一次全量。 */
  loaded: boolean;
  /** 最近一次全量拉取的错误（事件更新不清它；下一次成功拉取清零）。 */
  error: string | null;
  /** 后端列表被截断（>100 条）。 */
  truncated: boolean;
}

const FALLBACK_POLL_MS = 15_000;

let state: ExecutionsState = { executions: [], loaded: false, error: null, truncated: false };
const listeners = new Set<() => void>();
const agentListeners = new Map<string, Set<(event: AgentExecEvent) => void>>();

let started = false;
let refreshing = false;
let refreshQueued = false;

function emit() {
  for (const listener of listeners) listener();
}

function setState(patch: Partial<ExecutionsState>) {
  state = { ...state, ...patch };
  emit();
}

function sortExecutions(list: AgentExecution[]): AgentExecution[] {
  return [...list].sort((a, b) => (a.created_at < b.created_at ? 1 : a.created_at > b.created_at ? -1 : 0));
}

/** 全量拉取并与本地合并：本地更新时间更新的条目保留（事件先到、列表后到）。 */
export async function refreshExecutions(): Promise<void> {
  if (refreshing) {
    refreshQueued = true;
    return;
  }
  refreshing = true;
  try {
    const payload = await api.agentsList();
    const fetched = payload.executions ?? [];
    const local = new Map(state.executions.map((e) => [e.agent_id, e] as const));
    const merged = fetched.map((e) => {
      const mine = local.get(e.agent_id);
      return mine && mine.updated_at > e.updated_at ? mine : e;
    });
    setState({
      executions: sortExecutions(merged),
      loaded: true,
      error: null,
      truncated: payload.truncated === true,
    });
  } catch (e) {
    // agent 未启动等瞬时错误：保留上次快照。
    setState({ error: String(e) });
  } finally {
    refreshing = false;
    if (refreshQueued) {
      refreshQueued = false;
      void refreshExecutions();
    }
  }
}

function applyEvent(event: AgentExecEvent) {
  switch (event.kind) {
    case "snapshot": {
      const next = event.execution;
      const index = state.executions.findIndex((e) => e.agent_id === next.agent_id);
      if (index === -1) {
        setState({ executions: sortExecutions([...state.executions, next]) });
      } else if (state.executions[index].updated_at <= next.updated_at) {
        const list = state.executions.slice();
        list[index] = next;
        setState({ executions: list });
      }
      return;
    }
    case "removed": {
      const gone = new Set(event.agent_ids);
      if (state.executions.some((e) => gone.has(e.agent_id))) {
        setState({ executions: state.executions.filter((e) => !gone.has(e.agent_id)) });
      }
      for (const id of gone) {
        const subs = agentListeners.get(id);
        if (subs) for (const cb of subs) cb(event);
      }
      return;
    }
    case "resync":
      void refreshExecutions();
      return;
    case "tools_begun":
    case "transcript": {
      const subs = agentListeners.get(event.agent_id);
      if (subs) for (const cb of subs) cb(event);
      return;
    }
    default:
      return;
  }
}

function ensureStarted() {
  if (started) return;
  started = true;
  void refreshExecutions();
  void api.onAgentExec(applyEvent).catch(() => {
    // 监听失败（非 Tauri 环境）：只剩兜底轮询。
  });
  setInterval(() => {
    if (typeof document === "undefined" || document.visibilityState !== "hidden") {
      void refreshExecutions();
    }
  }, FALLBACK_POLL_MS);
  if (typeof document !== "undefined") {
    // 后台期间事件可能被 WebView 节流 / 丢弃：回到前台立即对齐一次。
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible") void refreshExecutions();
    });
  }
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function getSnapshot(): ExecutionsState {
  return state;
}

/** 订阅执行列表；`enabled`（agent 已就绪）为 true 时启动直播仓（幂等）。 */
export function useAgentExecutions(enabled: boolean): ExecutionsState {
  useEffect(() => {
    if (enabled) ensureStarted();
  }, [enabled]);
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

/** 单条执行的列表级快照（不存在 / 已清除时 undefined）。 */
export function useAgentExecution(agentId: string | null): AgentExecution | undefined {
  const { executions } = useAgentExecutions(true);
  return agentId === null ? undefined : executions.find((e) => e.agent_id === agentId);
}

/** 订阅某条执行的逐执行事件（tools_begun / transcript / removed）。 */
export function subscribeAgentEvents(
  agentId: string,
  callback: (event: AgentExecEvent) => void,
): () => void {
  ensureStarted();
  let subs = agentListeners.get(agentId);
  if (!subs) {
    subs = new Set();
    agentListeners.set(agentId, subs);
  }
  subs.add(callback);
  return () => {
    const current = agentListeners.get(agentId);
    if (!current) return;
    current.delete(callback);
    if (current.size === 0) agentListeners.delete(agentId);
  };
}

/** 本地乐观更新：清除历史后立即过滤终态记录（事件 / 下次拉取对齐真实状态）。 */
export function dropTerminalLocally() {
  setState({ executions: state.executions.filter((e) => !TERMINAL_STATES.has(stateOf(e))) });
}

// ---------------------------------------------------------------------------
// 状态口径与格式化（各面板共用，避免文案 / 样式走散）
// ---------------------------------------------------------------------------

export const STATE_LABEL: Record<string, string> = {
  queued: "排队中",
  running: "运行中",
  waiting: "等待中",
  cancelling: "正在取消",
  succeeded: "已完成",
  failed: "失败",
  cancelled: "已取消",
};

export const STATE_STYLE: Record<string, string> = {
  queued: "bg-gray-500/10 text-gray-400",
  running: "bg-amber-500/10 text-amber-400",
  waiting: "bg-sky-500/10 text-sky-400",
  cancelling: "bg-orange-500/10 text-orange-400",
  succeeded: "bg-emerald-500/10 text-emerald-400",
  failed: "bg-red-500/10 text-red-400",
  cancelled: "bg-gray-500/10 text-gray-500",
};

/** 进度条颜色（按状态）。 */
export const BAR_STYLE: Record<string, string> = {
  queued: "bg-gray-500",
  running: "bg-amber-400",
  waiting: "bg-sky-400",
  cancelling: "bg-orange-400",
  succeeded: "bg-emerald-500",
  failed: "bg-red-500",
  cancelled: "bg-gray-600",
};

/** 已终态（可被「清除历史」清掉）。 */
export const TERMINAL_STATES = new Set(["succeeded", "failed", "cancelled"]);
/** 活跃（侧边栏徽标计数；running / waiting / cancelling 另带脉动）。 */
export const ACTIVE_STATES = new Set(["queued", "running", "waiting", "cancelling"]);

export function stateOf(e: Pick<AgentExecution, "state">): string {
  return e.state?.toLowerCase?.() ?? "";
}

export function isTerminal(e: Pick<AgentExecution, "state">): boolean {
  return TERMINAL_STATES.has(stateOf(e));
}

/** 本进程在跑：可取消 / 可插话；运行时长实时跳动。 */
export function isLive(e: AgentExecution): boolean {
  return e.live === true && !isTerminal(e);
}

/** 等待中但没有执行者（重启后恢复，需 AgentResume）。 */
export function isStalled(e: AgentExecution): boolean {
  return stateOf(e) === "waiting" && e.live !== true;
}

function parseTs(value: string | null | undefined): number | null {
  if (!value) return null;
  const ms = Date.parse(value);
  return Number.isNaN(ms) ? null : ms;
}

/** 运行时长（毫秒）：开跑 → 终态 / 现在（在跑）/ 最后更新（无执行者）。未开跑为 null。 */
export function executionElapsedMs(e: AgentExecution, now: number): number | null {
  const start = parseTs(e.started_at);
  if (start === null) return null;
  const end = parseTs(e.finished_at) ?? (isLive(e) ? now : (parseTs(e.updated_at) ?? now));
  return Math.max(0, end - start);
}

/** 排队时长（毫秒）：仅 queued 状态。 */
export function queuedMs(e: AgentExecution, now: number): number | null {
  if (stateOf(e) !== "queued") return null;
  const created = parseTs(e.created_at);
  return created === null ? null : Math.max(0, now - created);
}

/** 时长文案：秒级到分级到小时级。 */
export function formatElapsed(ms: number): string {
  const total = Math.floor(ms / 1000);
  if (total < 60) return `${total} 秒`;
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h === 0) return `${m} 分 ${String(s).padStart(2, "0")} 秒`;
  return `${h} 小时 ${String(m).padStart(2, "0")} 分`;
}

/** token 数文案：1234 → 1.2K，1234567 → 1.23M。 */
export function formatTokens(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}K`;
  return `${(n / 1_000_000).toFixed(2)}M`;
}

/** 累计 token 的一行摘要（没有任何模型调用时 null）。 */
export function usageSummary(e: Pick<AgentExecution, "usage">): string | null {
  const u = e.usage;
  if (!u || u.model_calls === 0) return null;
  const approx = u.unreported_calls > 0 ? "≥" : "";
  return `${approx}${formatTokens(u.total_tokens)} tokens`;
}

/** 进度百分比（0–100）。 */
export function progressPercent(e: AgentExecution): number {
  const total = e.progress?.total_steps ?? 0;
  const done = e.progress?.completed_steps ?? 0;
  if (total <= 0) return 0;
  return Math.min(100, Math.max(0, (done / total) * 100));
}

/** 每秒一跳的当前时间（只在 `enabled` 时跑定时器；关闭时返回挂载时刻）。 */
export function useNow(enabled: boolean, intervalMs = 1000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!enabled) return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(timer);
  }, [enabled, intervalMs]);
  return now;
}

/** 列表里的显示名：名字优先，退回 agent_id。 */
export function displayName(e: Pick<AgentExecution, "name" | "agent_id">): string {
  const name = e.name?.trim();
  return name ? name : e.agent_id;
}

/** 活跃统计（徽标 / 概览四格）。 */
export function countByState(list: AgentExecution[]) {
  let active = 0;
  let running = 0;
  let queued = 0;
  let done = 0;
  let failed = 0;
  let finished = 0;
  for (const e of list) {
    const s = stateOf(e);
    if (ACTIVE_STATES.has(s)) active += 1;
    if (s === "running" || s === "waiting" || s === "cancelling") running += 1;
    if (s === "queued") queued += 1;
    if (s === "succeeded") done += 1;
    if (s === "failed") failed += 1;
    if (TERMINAL_STATES.has(s)) finished += 1;
  }
  return { active, running, queued, done, failed, finished, total: list.length };
}
