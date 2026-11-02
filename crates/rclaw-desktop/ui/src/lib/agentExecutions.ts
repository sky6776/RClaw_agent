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
import { useCallback, useEffect, useState, useSyncExternalStore } from "react";
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
/** `removed` 事件的墓碑保留时长：拦住「删除前就已发出、删除后才返回」的全量列表把条目复活。 */
const REMOVED_TOMBSTONE_MS = 30_000;
/** 全量拉取期间经事件新入表的条目（created_at 晚于拉取发起时刻）保留；留点钟差余量。 */
const INFLIGHT_CREATED_SLACK_MS = 2_000;

/** 活跃统计（侧边栏徽标 / 概览四格）。 */
export interface ExecutionCounts {
  /** 本进程里真在跑的（排队 / 运行 / 等待 / 取消中，且有执行者）：侧边栏徽标数、秒表开关。 */
  active: number;
  /** 其中已开跑的（运行 / 等待 / 取消中，且有执行者）：徽标脉动、「N 个在运行」。 */
  running: number;
  queued: number;
  /** 等待中但没有执行者（进程重启后恢复，需 AgentResume）：不算在跑，单独提示。 */
  stalled: number;
  done: number;
  failed: number;
  finished: number;
  total: number;
}

const EMPTY_COUNTS: ExecutionCounts = {
  active: 0,
  running: 0,
  queued: 0,
  stalled: 0,
  done: 0,
  failed: 0,
  finished: 0,
  total: 0,
};

let state: ExecutionsState = { executions: [], loaded: false, error: null, truncated: false };
/** 列表的统计投影：只在数字变化时换引用，徽标订阅者不随每条快照事件重渲。 */
let counts: ExecutionCounts = EMPTY_COUNTS;
const listeners = new Set<() => void>();
const agentListeners = new Map<string, Set<(event: AgentExecEvent) => void>>();
/** 最近被 `removed` 事件清掉的 id → 清除时刻。 */
const tombstones = new Map<string, number>();

let started = false;
let refreshing = false;
let refreshQueued = false;

function emit() {
  for (const listener of listeners) listener();
}

function sameCounts(a: ExecutionCounts, b: ExecutionCounts): boolean {
  return (
    a.active === b.active &&
    a.running === b.running &&
    a.queued === b.queued &&
    a.stalled === b.stalled &&
    a.done === b.done &&
    a.failed === b.failed &&
    a.finished === b.finished &&
    a.total === b.total
  );
}

function setState(patch: Partial<ExecutionsState>) {
  state = { ...state, ...patch };
  if (patch.executions) {
    const next = countByState(state.executions);
    if (!sameCounts(counts, next)) counts = next;
  }
  emit();
}

function sortExecutions(list: AgentExecution[]): AgentExecution[] {
  return [...list].sort((a, b) => (a.created_at < b.created_at ? 1 : a.created_at > b.created_at ? -1 : 0));
}

function pruneTombstones(now: number) {
  for (const [id, at] of tombstones) {
    if (now - at > REMOVED_TOMBSTONE_MS) tombstones.delete(id);
  }
}

/** 全量拉取并与本地合并：本地更新时间更新的条目保留（事件先到、列表后到）；拉取期间
 *  被 `removed` 清掉的不复活；拉取期间新入表的（事件已带来、列表快照里还没有）保留。 */
export async function refreshExecutions(): Promise<void> {
  if (refreshing) {
    refreshQueued = true;
    return;
  }
  refreshing = true;
  const startedAt = Date.now();
  try {
    const payload = await api.agentsList();
    const fetched = payload.executions ?? [];
    pruneTombstones(Date.now());
    const local = new Map(state.executions.map((e) => [e.agent_id, e] as const));
    const fetchedIds = new Set<string>();
    const merged: AgentExecution[] = [];
    for (const e of fetched) {
      fetchedIds.add(e.agent_id);
      if (tombstones.has(e.agent_id)) continue;
      const mine = local.get(e.agent_id);
      // 同一 updated_at 即同一份内容（每次变更都盖新时间戳）：保留本地对象，引用不变，
      // 选择器式订阅（useAgentExecution）不会因兜底轮询而白白重渲。
      merged.push(mine && mine.updated_at >= e.updated_at ? mine : e);
    }
    for (const e of state.executions) {
      if (fetchedIds.has(e.agent_id)) continue;
      const created = parseTs(e.created_at);
      if (created !== null && created >= startedAt - INFLIGHT_CREATED_SLACK_MS) merged.push(e);
    }
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

function fanOut(agentId: string, event: AgentExecEvent) {
  const subs = agentListeners.get(agentId);
  if (subs) for (const cb of subs) cb(event);
}

function applyEvent(event: AgentExecEvent) {
  switch (event.kind) {
    case "snapshot": {
      const next = event.execution;
      if (tombstones.has(next.agent_id)) return;
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
      const now = Date.now();
      pruneTombstones(now);
      for (const id of gone) tombstones.set(id, now);
      if (state.executions.some((e) => gone.has(e.agent_id))) {
        setState({ executions: state.executions.filter((e) => !gone.has(e.agent_id)) });
      }
      for (const id of gone) fanOut(id, event);
      return;
    }
    case "resync":
      // 宿主广播滞后：列表重拉；正在看的详情页同样要按游标补拉活动记录。
      void refreshExecutions();
      for (const [id] of agentListeners) fanOut(id, event);
      return;
    case "tools_begun":
    case "transcript":
      fanOut(event.agent_id, event);
      return;
    default:
      return;
  }
}

function ensureStarted() {
  if (started) return;
  started = true;
  // 先接事件再拉全量：listen 就绪前到达的事件没人收，只能等下一次兜底轮询补。
  let listening: Promise<unknown>;
  try {
    listening = api.onAgentExec(applyEvent);
  } catch {
    listening = Promise.resolve();
  }
  void listening
    .catch(() => {
      // 监听失败（非 Tauri 环境）：只剩兜底轮询。
    })
    .then(() => refreshExecutions());
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

function getCounts(): ExecutionCounts {
  return counts;
}

/** 只订阅统计数字（侧边栏徽标）：快照事件没改变计数时订阅者不重渲——App 根组件用它，
 *  避免每条进度 / token 事件都整树重渲。 */
export function useAgentExecutionCounts(enabled: boolean): ExecutionCounts {
  useEffect(() => {
    if (enabled) ensureStarted();
  }, [enabled]);
  return useSyncExternalStore(subscribe, getCounts, getCounts);
}

/** 单条执行的列表级快照（不存在 / 已清除时 undefined）。 */
export function useAgentExecution(agentId: string | null): AgentExecution | undefined {
  useEffect(() => {
    ensureStarted();
  }, []);
  // 选择器直接返回列表里的那个对象：其他执行的快照事件不改变它的引用，详情页不随之重渲。
  const select = useCallback(
    () => (agentId === null ? undefined : state.executions.find((e) => e.agent_id === agentId)),
    [agentId],
  );
  return useSyncExternalStore(subscribe, select, select);
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

/** 本地乐观更新：清除历史后立即过滤终态记录（事件 / 下次拉取对齐真实状态）。
 *  `truncated` 一并清掉：清除把窗口外的终态流也删了，剩下的活任务不会再被截断；
 *  真要是还截断，紧随其后的全量拉取会把它纠正回来。 */
export function dropTerminalLocally() {
  setState({
    executions: state.executions.filter((e) => !TERMINAL_STATES.has(stateOf(e))),
    truncated: false,
  });
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
export function countByState(list: AgentExecution[]): ExecutionCounts {
  let active = 0;
  let running = 0;
  let queued = 0;
  let stalled = 0;
  let done = 0;
  let failed = 0;
  let finished = 0;
  for (const e of list) {
    const s = stateOf(e);
    // 重启后恢复的 waiting 记录没有执行者：不是「在跑」，徽标不脉动、秒表不转。
    const live = isLive(e);
    if (ACTIVE_STATES.has(s)) {
      if (live) active += 1;
      else stalled += 1;
    }
    if ((s === "running" || s === "waiting" || s === "cancelling") && live) running += 1;
    if (s === "queued") queued += 1;
    if (s === "succeeded") done += 1;
    if (s === "failed") failed += 1;
    if (TERMINAL_STATES.has(s)) finished += 1;
  }
  return { active, running, queued, stalled, done, failed, finished, total: list.length };
}
