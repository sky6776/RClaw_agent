import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  api,
  type AgentExecEvent,
  type AgentExecution,
  type AgentExecutionDetail as DetailDto,
  type HistoryMsg,
} from "../lib/api";
import {
  formatTokens,
  isLive,
  isTerminal,
  stateOf,
  subscribeAgentEvents,
  useAgentExecution,
  useNow,
} from "../lib/agentExecutions";
import {
  sameToolBatch,
  settleInterrupted,
  viewsFromBegun,
  viewsFromSummary,
  type ToolCallView,
} from "../lib/toolCalls";
import { isAndroidDevice } from "../lib/useIsMobile";
import ToolBatchRows, { transferToolRowOpenState, type ToolDetailLoader } from "./ToolCallRow";
import { ProgressBar, StateBadge, elapsedText } from "./ExecutionCard";

/**
 * 子 agent 执行详情页：实时头部（状态 / 阶段进度 / 运行时长 / token）、完整活动记录
 * （任务、思考块、正文、工具与 Skill 调用——与前台会话同一套渲染原语，展开可看完整
 * 入参 / 结果）、正在执行的工具行（直播）、交接 / 错误，以及与子 agent 对话的输入框
 * 与取消按钮。
 *
 * 数据流：列表级快照来自直播仓（`useAgentExecution`）；详情（事件尾、handoff、
 * inbox）在快照 `updated_at` 变化时重拉；活动记录首屏 `agentExecutionHistory`，之后按
 * `transcript` 事件游标增量续读；`tools_begun` 事件先以「进行中」行显示，落库行到达
 * 时按调用 id 序列对位替换，终态时仍未落库的标为「已中断」。
 */

/** 活动记录条目（与 Chat 的 Entry 同形的子集）。 */
interface TranscriptEntry {
  key: string;
  role: HistoryMsg["role"];
  content: string;
  /** 毫秒。 */
  ts: number;
  msgId?: number;
  tools?: ToolCallView[];
}

/** 直播中的工具批（尚未落库）。 */
interface PendingBatch {
  key: string;
  iter: number;
  ts: number;
  tools: ToolCallView[];
}

const INTERJECTION_HEAD = "## Message from the user";
const INTERJECTION_TAIL = "\n\nThe user is watching this execution live";
const NUDGE_HEAD = "Your reply has been shown to the user.";
const TEAM_HEAD = "## Incoming Team Messages";
/** 续跑分隔标记（运行时 `SUBAGENT_RESUME_MARKER`）：AgentResume 后任务书不重复落库，只落这条。 */
const RESUME_HEAD = "## Execution resumed";

/** 用户角色行的来源分类：任务书 / 用户插话 / 团队消息 / 续跑标记 / 系统提示（闸门反馈等）。 */
type UserKind = "task" | "interjection" | "team" | "resume" | "system";

function classifyUser(content: string, first: boolean): { kind: UserKind; body: string } {
  if (content.startsWith(INTERJECTION_HEAD)) {
    const tail = content.indexOf(INTERJECTION_TAIL);
    const body = content.slice(INTERJECTION_HEAD.length, tail === -1 ? undefined : tail).trim();
    return { kind: "interjection", body };
  }
  if (content.startsWith(TEAM_HEAD)) return { kind: "team", body: content };
  if (content.startsWith(RESUME_HEAD)) {
    return { kind: "resume", body: "进程重启后续跑（AgentResume）：子 agent 从顶部的任务书重新开始，上方是上一轮的记录。" };
  }
  if (first && !content.startsWith(NUDGE_HEAD)) return { kind: "task", body: content };
  return { kind: "system", body: content };
}

const USER_KIND_LABEL: Record<UserKind, string> = {
  task: "任务",
  interjection: "你的插话",
  team: "团队消息",
  resume: "续跑",
  system: "系统提示",
};

const MD_COMPONENTS = {
  a: ({ href, children }: { href?: string; children?: React.ReactNode }) => (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      className="text-indigo-400 underline decoration-indigo-400/40 underline-offset-2 hover:text-indigo-300"
      onClick={(e) => {
        e.preventDefault();
        if (href && /^https?:\/\//i.test(href)) {
          void api.browserOpenUrl(href).catch(() => undefined);
        }
      }}
    >
      {children}
    </a>
  ),
  table: ({ children }: { children?: React.ReactNode }) => (
    <div className="table-scroll">
      <table className="w-max min-w-full">{children}</table>
    </div>
  ),
};

const Markdown = React.memo(function Markdown({ content }: { content: string }) {
  return (
    <div className="md-body w-full text-sm text-gray-200 max-md:text-[15px] max-md:leading-relaxed">
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={MD_COMPONENTS}>
        {content}
      </ReactMarkdown>
    </div>
  );
});

function formatClock(ms: number): string {
  const d = new Date(ms);
  if (!Number.isFinite(d.getTime())) return "";
  const two = (n: number) => String(n).padStart(2, "0");
  return `${two(d.getHours())}:${two(d.getMinutes())}:${two(d.getSeconds())}`;
}

function formatIso(value: string | null | undefined): string {
  if (!value) return "—";
  const ms = Date.parse(value);
  if (Number.isNaN(ms)) return value;
  const d = new Date(ms);
  const two = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${two(d.getMonth() + 1)}-${two(d.getDate())} ${two(d.getHours())}:${two(d.getMinutes())}:${two(d.getSeconds())}`;
}

function entryFrom(m: HistoryMsg): TranscriptEntry {
  return {
    key: `m${m.id}:${m.role}`,
    role: m.role,
    content: m.content,
    ts: m.ts > 0 ? m.ts * 1000 : 0,
    msgId: m.id,
    ...(m.tool_calls ? { tools: viewsFromSummary(m.tool_calls) } : {}),
  };
}

/** 首屏窗口上限（与后端 `agent_execution_history` 的 retain_last_rows 同值）：拉满即视为
 *  被截断，最早的行（任务书）不在窗口里。 */
const HISTORY_WINDOW_ROWS = 200;

/** 落库行与直播批对位的时间余量：行 ts 是落库时刻取整到秒，一定不早于该批开始；
 *  比直播批开始时刻还早超过这个余量的行是更早的一批（provider 复用调用 id 时同序）。 */
const COMMIT_MATCH_SLACK_MS = 1_500;

/** 落库的工具批到达：撤掉对应的直播行（调用 id 序列一致且落库不早于该批开始；落库行带
 *  权威成败），并把用户展开着的行的开合状态搬到落库行上。幂等。 */
function withoutCommitted(
  pending: PendingBatch[],
  fresh: TranscriptEntry[],
  chatId: string,
): PendingBatch[] {
  const committed = fresh.filter((e) => e.role === "status" && e.tools && e.tools.length > 0);
  if (committed.length === 0 || pending.length === 0) return pending;
  const next = pending.filter((p) => {
    const match = committed.find(
      (c) => sameToolBatch(p.tools, c.tools) && (c.ts === 0 || c.ts >= p.ts - COMMIT_MATCH_SLACK_MS),
    );
    if (!match) return true;
    if (match.msgId !== undefined) {
      transferToolRowOpenState(
        chatId,
        -p.iter,
        match.msgId,
        p.tools.map((t) => t.id),
      );
    }
    return false;
  });
  return next.length === pending.length ? pending : next;
}

/** 思考块（折叠式；默认展开最后一块，其余收起）。 */
function ThinkingBlock({ content, defaultOpen }: { content: string; defaultOpen: boolean }) {
  const [open, setOpen] = useState(defaultOpen);
  const body = content.replace(/^💭\s*思考中\.\.\.\n?/, "").trim();
  return (
    <div className="flex justify-start">
      <div className="w-full overflow-hidden rounded-2xl rounded-bl-md border border-dashed border-[#3a4150] bg-[#141821]">
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          className="flex w-full items-center justify-between gap-3 px-4 py-1.5 text-left text-xs text-gray-400 transition hover:text-gray-200"
        >
          <span>💭 思考过程</span>
          <span className="text-[10px] text-gray-600">{open ? "▾" : "▸"}</span>
        </button>
        {open && body && (
          <div className="whitespace-pre-wrap px-4 pb-3 text-xs leading-relaxed text-gray-400 [overflow-wrap:anywhere]">
            {body}
          </div>
        )}
      </div>
    </div>
  );
}

/** 交接（handoff）卡片。 */
function HandoffCard({ handoff }: { handoff: NonNullable<DetailDto["execution"]["handoff"]> }) {
  const [open, setOpen] = useState(true);
  const section = (title: string, items: string[]) =>
    items.length > 0 && (
      <div>
        <div className="mb-0.5 text-[11px] text-gray-500">{title}</div>
        <ul className="list-disc space-y-0.5 pl-4 text-xs text-gray-300">
          {items.map((item, i) => (
            <li key={i} className="[overflow-wrap:anywhere]">
              {item}
            </li>
          ))}
        </ul>
      </div>
    );
  return (
    <div className="rounded-xl border border-emerald-500/30 bg-emerald-500/5">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center justify-between px-4 py-2 text-left text-xs font-medium text-emerald-300"
      >
        <span>📦 交接结果</span>
        <span className="text-[10px] text-gray-500">{open ? "▾" : "▸"}</span>
      </button>
      {open && (
        <div className="space-y-2 px-4 pb-3">
          {handoff.goal && (
            <div>
              <div className="mb-0.5 text-[11px] text-gray-500">目标</div>
              <div className="text-xs text-gray-300 [overflow-wrap:anywhere]">{handoff.goal}</div>
            </div>
          )}
          <div>
            <div className="mb-0.5 text-[11px] text-gray-500">摘要</div>
            <Markdown content={handoff.summary} />
          </div>
          {section("证据", handoff.evidence)}
          {section("产物", handoff.artifacts)}
          {section("未决风险", handoff.open_risks)}
          {section("验证", handoff.validation)}
        </div>
      )}
    </div>
  );
}

/** 状态变迁时间线（折叠）。 */
function EventsTimeline({ events }: { events: DetailDto["events"] }) {
  const [open, setOpen] = useState(false);
  if (events.length === 0) return null;
  return (
    <div className="rounded-xl border border-[#2a2f3a] bg-[#141821]">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center justify-between px-4 py-2 text-left text-xs text-gray-400"
      >
        <span>状态变迁（{events.length}）</span>
        <span className="text-[10px] text-gray-600">{open ? "▾" : "▸"}</span>
      </button>
      {open && (
        <ul className="space-y-1 px-4 pb-3 text-[11px] text-gray-500">
          {events.map((ev, i) => (
            <li key={i} className="flex flex-wrap items-center gap-x-2">
              <span className="font-mono tabular-nums text-gray-600">{formatIso(ev.timestamp)}</span>
              <span className="text-gray-400">{ev.event}</span>
              <span>→ {ev.state}</span>
              {ev.progress?.stage && <span className="truncate text-gray-600">· {ev.progress.stage}</span>}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

export default function AgentExecutionDetail({
  agentId,
  onBack,
}: {
  agentId: string;
  /** 返回列表。 */
  onBack: () => void;
}) {
  const summary = useAgentExecution(agentId);
  const [detail, setDetail] = useState<DetailDto | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [removed, setRemoved] = useState(false);
  const [entries, setEntries] = useState<TranscriptEntry[]>([]);
  const [pending, setPending] = useState<PendingBatch[]>([]);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [actionNotice, setActionNotice] = useState<string | null>(null);
  /** 首屏窗口被截断（最早的行不在屏上）：首行不再当「任务」标注。 */
  const [truncated, setTruncated] = useState(false);
  /** 列表是否在跟随底部（只用于显示「回到底部」按钮；判定本身在 ref 里，不触发重渲）。 */
  const [following, setFollowing] = useState(true);
  const cursorRef = useRef(0);
  /** 首屏快照已到（之后 transcript 事件才按游标续拉；之前到达的事件在首屏后补拉一次）。 */
  const loadedRef = useRef(false);
  const syncingRef = useRef(false);
  const resyncQueuedRef = useRef(false);
  /** 加载代际：切换执行 / 卸载时递增，在途的拉取回来后发现代际变了就丢弃结果。 */
  const genRef = useRef(0);
  const listRef = useRef<HTMLDivElement | null>(null);
  const stickToBottomRef = useRef(true);
  const lastScrollTopRef = useRef(0);
  const inputRef = useRef<HTMLTextAreaElement | null>(null);

  const live = summary ? isLive(summary) : false;
  const terminal = summary ? isTerminal(summary) : detail ? isTerminal(detail.execution) : false;
  const now = useNow(live || stateOf(summary ?? { state: "" }) === "queued");
  const transcriptKey = `agent:${agentId}`;

  // 详情（事件尾 / handoff / inbox / cancellable）：首次 + 快照每次更新时重拉。
  const updatedAt = summary?.updated_at;
  useEffect(() => {
    let cancelled = false;
    api
      .agentExecutionDetail(agentId)
      .then((d) => {
        if (cancelled) return;
        setDetail(d);
        setDetailError(null);
      })
      .catch((e) => {
        if (!cancelled) setDetailError(String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [agentId, updatedAt]);

  // 活动记录：首屏快照 + 游标增量（串行化：同时只跑一个拉取，期间到达的事件合并为一次续拉；
  // 代际校验：切换执行后在途的旧拉取不得把旧记录的行与游标写进新记录）。
  const pullDelta = useCallback(async () => {
    const gen = genRef.current;
    if (syncingRef.current) {
      resyncQueuedRef.current = true;
      return;
    }
    syncingRef.current = true;
    try {
      for (;;) {
        const delta = await api.agentExecutionHistorySince(agentId, cursorRef.current);
        if (gen !== genRef.current) return;
        if (delta.cursor > cursorRef.current) cursorRef.current = delta.cursor;
        if (delta.entries.length > 0) {
          const fresh = delta.entries.map(entryFrom);
          setEntries((prev) => {
            const have = new Set(prev.map((e) => e.key));
            const add = fresh.filter((e) => !have.has(e.key));
            return add.length > 0 ? [...prev, ...add] : prev;
          });
          setPending((prev) => withoutCommitted(prev, fresh, transcriptKey));
        }
        setHistoryError(null);
        if (!delta.more) break;
      }
    } catch (e) {
      if (gen === genRef.current) setHistoryError(String(e));
    } finally {
      if (gen === genRef.current) {
        syncingRef.current = false;
        if (resyncQueuedRef.current) {
          resyncQueuedRef.current = false;
          void pullDelta();
        }
      }
    }
  }, [agentId, transcriptKey]);

  useEffect(() => {
    let cancelled = false;
    genRef.current += 1;
    const gen = genRef.current;
    cursorRef.current = 0;
    loadedRef.current = false;
    syncingRef.current = false;
    resyncQueuedRef.current = false;
    setEntries([]);
    setPending([]);
    setRemoved(false);
    setDetail(null);
    setHistoryError(null);
    setTruncated(false);
    stickToBottomRef.current = true;
    setFollowing(true);
    // 首屏拉取期间到达的 transcript 事件先记下，首屏到位后按游标补拉一次（排队中的执行
    // 首屏可能为空、游标为 0，所以不能拿游标是否为 0 当「已加载」）。
    let eventSeen = false;
    const unsubscribe = subscribeAgentEvents(agentId, (event: AgentExecEvent) => {
      if (cancelled) return;
      if (event.kind === "transcript") {
        if (!loadedRef.current) {
          eventSeen = true;
        } else if (event.cursor > cursorRef.current) {
          void pullDelta();
        }
      } else if (event.kind === "tools_begun") {
        const tools = viewsFromBegun(event.batch.calls);
        setPending((prev) => [
          ...prev.filter((p) => p.iter !== event.batch.iter),
          { key: `p${event.batch.iter}:${Date.now()}`, iter: event.batch.iter, ts: Date.now(), tools },
        ]);
      } else if (event.kind === "removed") {
        setRemoved(true);
      } else if (event.kind === "resync") {
        // 宿主广播滞后：中间的 transcript 事件可能已丢，按游标补拉对齐。
        if (loadedRef.current) void pullDelta();
        else eventSeen = true;
      }
    });
    // 后台期间 WebView 事件可能被节流 / 丢弃：回到前台按游标补拉一次。
    const onVisibility = () => {
      if (document.visibilityState === "visible" && loadedRef.current && !cancelled) {
        void pullDelta();
      }
    };
    document.addEventListener("visibilitychange", onVisibility);
    api
      .agentExecutionHistory(agentId)
      .then((snapshot) => {
        if (cancelled || gen !== genRef.current) return;
        cursorRef.current = Math.max(cursorRef.current, snapshot.cursor);
        loadedRef.current = true;
        const fresh = snapshot.entries.map(entryFrom);
        setEntries(fresh);
        setPending((prev) => withoutCommitted(prev, fresh, transcriptKey));
        setTruncated(snapshot.entries.length >= HISTORY_WINDOW_ROWS);
        setHistoryError(null);
        if (eventSeen) void pullDelta();
      })
      .catch((e) => {
        if (cancelled || gen !== genRef.current) return;
        // 首屏失败也进入「已加载」：之后的事件 / 重试按游标 0 续拉能拿回全部行。
        loadedRef.current = true;
        setHistoryError(String(e));
      });
    return () => {
      cancelled = true;
      // 让在途的拉取作废（卸载 / 切换执行）。
      genRef.current += 1;
      document.removeEventListener("visibilitychange", onVisibility);
      unsubscribe();
    };
  }, [agentId, pullDelta, transcriptKey]);

  // 终态：仍在「进行中」的直播工具行标为已中断（没有落库行会来替换它们）；
  // 同时再按游标补拉一次，兜底收尾阶段可能错过的 transcript 事件。
  useEffect(() => {
    if (!terminal) return;
    setPending((prev) => prev.map((p) => ({ ...p, tools: settleInterrupted(p.tools) })));
    if (loadedRef.current) void pullDelta();
  }, [terminal, pullDelta]);

  // 自动跟随到底部。只有用户主动向上滚才解除跟随（软键盘弹出等布局变化引起的滚动事件
  // 不会让 scrollTop 变小）；回到底部附近 / 内容没溢出时恢复跟随。
  const handleScroll = () => {
    const el = listRef.current;
    if (!el) return;
    const distance = el.scrollHeight - el.scrollTop - el.clientHeight;
    const atBottom = distance < 48 || el.scrollHeight <= el.clientHeight;
    const scrolledUp = el.scrollTop < lastScrollTopRef.current - 1;
    lastScrollTopRef.current = el.scrollTop;
    if (atBottom) {
      if (!stickToBottomRef.current) {
        stickToBottomRef.current = true;
        setFollowing(true);
      }
    } else if (scrolledUp && stickToBottomRef.current) {
      stickToBottomRef.current = false;
      setFollowing(false);
    }
  };
  const jumpToBottom = () => {
    stickToBottomRef.current = true;
    setFollowing(true);
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  };
  useEffect(() => {
    const el = listRef.current;
    if (el && stickToBottomRef.current) el.scrollTop = el.scrollHeight;
  }, [entries, pending, detail?.execution.handoff]);

  const detailLoader = useMemo<ToolDetailLoader>(
    () => (msgId: number) => api.agentExecutionToolDetail(agentId, msgId),
    [agentId],
  );

  // 活动记录 + 直播批按时间合并（直播批总在最后：它们一定晚于已落库的行）。
  const lastThinkingKey = useMemo(() => {
    for (let i = entries.length - 1; i >= 0; i -= 1) {
      if (entries[i].role === "thinking") return entries[i].key;
    }
    return null;
  }, [entries]);

  const exec = detail?.execution;
  const usage = summary?.usage ?? exec?.usage;
  const stage = summary?.progress?.stage ?? exec?.progress?.stage ?? "";
  const name = summary?.name?.trim() || exec?.spec.name?.trim() || agentId;
  const elapsed = summary ? elapsedText(summary, now) : null;
  const canSend = live && !removed;
  const inboxPending = detail?.inbox_pending ?? summary?.inbox_pending ?? 0;

  const send = async () => {
    const text = draft.trim();
    if (!text || sending || !canSend) return;
    setSending(true);
    setActionError(null);
    try {
      await api.agentExecutionSend(agentId, text);
      setDraft("");
      setActionNotice("已发送：子 agent 将在下一个安全点读到这条消息并回应。");
      jumpToBottom();
      // inbox_pending 由随之而来的快照事件（updated_at 推进 → 详情重拉）带回，不在这里
      // 另发一次详情请求——两路响应乱序会让旧详情盖掉新的。
    } catch (e) {
      setActionError(String(e));
    } finally {
      setSending(false);
      inputRef.current?.focus();
    }
  };

  const cancel = async () => {
    if (cancelling) return;
    if (!window.confirm(`取消子 agent「${name}」的执行？当前正在进行的工具调用会被中断。`)) return;
    setCancelling(true);
    setActionError(null);
    try {
      await api.agentExecutionCancel(agentId);
      setActionNotice("已请求取消，等待子 agent 在安全点停下。");
    } catch (e) {
      setActionError(String(e));
    } finally {
      setCancelling(false);
    }
  };

  useEffect(() => {
    if (!actionNotice) return;
    const timer = setTimeout(() => setActionNotice(null), 4000);
    return () => clearTimeout(timer);
  }, [actionNotice]);

  const headerSnapshot: AgentExecution | undefined =
    summary ??
    (exec
      ? {
          agent_id: exec.agent_id,
          name: exec.spec.name,
          state: exec.state,
          progress: exec.progress,
          goal: exec.spec.task,
          created_at: exec.created_at,
          updated_at: exec.updated_at,
          started_at: exec.started_at,
          finished_at: exec.finished_at,
          usage: exec.usage,
          live: detail?.cancellable,
        }
      : undefined);

  return (
    <div className="flex h-full min-h-0 flex-col">
      {/* 头部：返回 / 名字 / 状态 / 取消 */}
      <div className="border-b border-[#2a2f3a] px-6 pb-3 pt-4 max-md:px-3 max-md:pt-3">
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={onBack}
            className="flex h-7 w-7 shrink-0 items-center justify-center rounded-lg text-gray-400 transition hover:bg-[#1a1f29] hover:text-gray-200"
            title="返回列表"
            aria-label="返回列表"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round">
              <path d="m15 6-6 6 6 6" />
            </svg>
          </button>
          <div className="min-w-0 flex-1">
            <div className="flex min-w-0 items-center gap-2">
              <h1 className="truncate text-base font-semibold text-gray-100">{name}</h1>
              {headerSnapshot && <StateBadge e={headerSnapshot} size="md" />}
            </div>
            <div className="truncate font-mono text-[10px] text-gray-600" title={agentId}>
              {agentId}
            </div>
          </div>
          {detail?.cancellable && !terminal && (
            <button
              type="button"
              onClick={cancel}
              disabled={cancelling || stateOf(summary ?? { state: "" }) === "cancelling"}
              className="shrink-0 rounded-lg border border-red-500/40 px-2.5 py-1 text-xs text-red-300 transition hover:bg-red-500/10 disabled:cursor-not-allowed disabled:opacity-40"
            >
              {cancelling ? "取消中…" : "取消执行"}
            </button>
          )}
        </div>

        {/* 实时指标 */}
        {headerSnapshot && (
          <div className="mt-3">
            <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-gray-400">
              <span className="min-w-0 truncate" title={stage}>
                阶段：<span className="text-gray-200">{stage || "—"}</span>
              </span>
              <span className="font-mono tabular-nums">
                {headerSnapshot.progress?.completed_steps ?? 0}/{headerSnapshot.progress?.total_steps ?? 0} 步
              </span>
              {elapsed && (
                <span className={`font-mono tabular-nums ${live ? "text-indigo-300" : ""}`}>{elapsed}</span>
              )}
              <span
                className="font-mono tabular-nums"
                title={
                  usage && usage.model_calls > 0
                    ? `输入 ${usage.prompt_tokens.toLocaleString()} · 输出 ${usage.completion_tokens.toLocaleString()} · 模型调用 ${usage.model_calls} 次${
                        usage.unreported_calls > 0 ? `（其中 ${usage.unreported_calls} 次未上报用量）` : ""
                      }`
                    : "尚无模型调用"
                }
              >
                {usage && usage.model_calls > 0
                  ? `${usage.unreported_calls > 0 ? "≥" : ""}${formatTokens(usage.total_tokens)} tokens（↑${formatTokens(usage.prompt_tokens)} ↓${formatTokens(usage.completion_tokens)}）· ${usage.model_calls} 次调用`
                  : "0 tokens"}
              </span>
            </div>
            <ProgressBar e={headerSnapshot} className="mt-2" />
            <div className="mt-2 flex flex-wrap gap-x-4 gap-y-0.5 text-[11px] text-gray-500">
              {exec?.spec.role && <span>角色：{exec.spec.role}</span>}
              {(summary?.model ?? exec?.spec.model) && <span>模型：{summary?.model ?? exec?.spec.model}</span>}
              {(summary?.isolation ?? exec?.spec.isolation) && (
                <span>隔离：{summary?.isolation ?? exec?.spec.isolation}</span>
              )}
              {(summary?.team_name ?? exec?.spec.team_name) && (
                <span>团队：{summary?.team_name ?? exec?.spec.team_name}</span>
              )}
              {(summary?.origin_chat_id ?? exec?.spec.origin_chat_id) && (
                <span>
                  来源会话：{summary?.source_channel ?? exec?.spec.source_channel ?? ""}/
                  {summary?.origin_chat_id ?? exec?.spec.origin_chat_id}
                </span>
              )}
              <span>创建：{formatIso(headerSnapshot.created_at)}</span>
              {headerSnapshot.started_at && <span>开跑：{formatIso(headerSnapshot.started_at)}</span>}
              {headerSnapshot.finished_at && <span>结束：{formatIso(headerSnapshot.finished_at)}</span>}
            </div>
          </div>
        )}
        {removed && (
          <div className="mt-3 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-300">
            这条执行记录已被清除（活动记录随之删除）。
          </div>
        )}
        {detailError && !removed && (
          <div className="mt-3 rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-300">
            详情加载失败：{detailError}
          </div>
        )}
      </div>

      {/* 活动记录 */}
      <div className="relative min-h-0 flex-1">
      <div ref={listRef} onScroll={handleScroll} className="h-full space-y-3 overflow-y-auto px-6 py-4 max-md:px-3">
        {exec?.error && (
          <div className="whitespace-pre-wrap rounded-xl border border-red-500/40 bg-red-500/10 px-4 py-2 text-xs text-red-300 [overflow-wrap:anywhere]">
            {stateOf(exec) === "cancelled" ? "取消原因：" : "失败原因："}
            {exec.error}
          </div>
        )}
        {exec?.recovered && !live && !terminal && (
          <div className="rounded-xl border border-sky-500/40 bg-sky-500/10 px-4 py-2 text-xs text-sky-300">
            这条执行是进程重启后从登记表恢复的，当前没有执行者；需要在对话里用 AgentResume 续跑。
          </div>
        )}
        {historyError && (
          <div className="rounded-xl border border-red-500/40 bg-red-500/10 px-4 py-2 text-xs text-red-300">
            活动记录加载失败：{historyError}
            <button
              type="button"
              className="ml-2 rounded border border-red-500/40 px-1.5 py-px text-red-200"
              onClick={() => void pullDelta()}
            >
              重试
            </button>
          </div>
        )}
        {entries.length === 0 && pending.length === 0 && !historyError && (
          <div className="text-xs text-gray-500">
            {stateOf(summary ?? { state: "" }) === "queued" ? "排队中，尚未开始执行。" : "暂无活动记录。"}
          </div>
        )}
        {entries.map((e, index) => {
          if (e.role === "status") {
            if (e.tools && e.tools.length > 0) {
              return (
                <div key={e.key} className="flex justify-start">
                  <div className="min-w-0 w-full max-w-[92%]">
                    <ToolBatchRows
                      chatId={transcriptKey}
                      entryId={e.msgId ?? index}
                      msgId={e.msgId}
                      calls={e.tools}
                      detailLoader={detailLoader}
                    />
                  </div>
                </div>
              );
            }
            return (
              <div key={e.key} className="flex justify-start">
                <span className="rounded-full bg-[#1a1f29] px-3 py-1 text-xs text-gray-500">{e.content}</span>
              </div>
            );
          }
          if (e.role === "thinking") {
            return <ThinkingBlock key={e.key} content={e.content} defaultOpen={e.key === lastThinkingKey} />;
          }
          if (e.role === "user") {
            // 首屏窗口被截断时最早的行不在屏上，首行不一定是任务书。
            const { kind, body } = classifyUser(e.content, index === 0 && !truncated);
            if (kind === "system" || kind === "resume") {
              return (
                <div key={e.key} className="flex justify-start">
                  <div className="max-w-[92%] whitespace-pre-wrap rounded-xl border border-dashed border-[#2a2f3a] px-3 py-1.5 text-[11px] text-gray-500 [overflow-wrap:anywhere]">
                    <span className="mr-1.5 text-gray-600">{USER_KIND_LABEL[kind]}</span>
                    {body}
                  </div>
                </div>
              );
            }
            return (
              <div key={e.key} className={`flex ${kind === "interjection" ? "justify-end" : "justify-start"}`}>
                <div className={`flex max-w-full flex-col md:max-w-xl ${kind === "interjection" ? "items-end" : "items-start"}`}>
                  <div
                    className={`whitespace-pre-wrap rounded-lg px-4 py-2 text-sm [overflow-wrap:anywhere] max-md:px-3.5 max-md:text-[15px] max-md:leading-relaxed ${
                      kind === "interjection"
                        ? "bg-indigo-500/15 text-gray-100"
                        : "bg-[#1a1f29] text-gray-200"
                    }`}
                  >
                    <div className="mb-1 text-[10px] text-gray-500">{USER_KIND_LABEL[kind]}</div>
                    {body}
                  </div>
                  {e.ts > 0 && <span className="mt-1 px-1 text-[10px] text-gray-500">{formatClock(e.ts)}</span>}
                </div>
              </div>
            );
          }
          return (
            <div key={e.key} className="flex justify-start">
              <div className="flex w-full flex-col items-start">
                <Markdown content={e.content} />
                {e.ts > 0 && <span className="mt-1 px-1 text-[10px] text-gray-500">{formatClock(e.ts)}</span>}
              </div>
            </div>
          );
        })}
        {pending.map((p) => (
          <div key={p.key} className="flex justify-start">
            <div className="min-w-0 w-full max-w-[92%]">
              <ToolBatchRows chatId={transcriptKey} entryId={-p.iter} calls={p.tools} detailLoader={detailLoader} />
            </div>
          </div>
        ))}
        {exec?.unresolved_disclosure && (
          <div className="whitespace-pre-wrap rounded-xl border border-amber-500/40 bg-amber-500/10 px-4 py-2 text-xs text-amber-200 [overflow-wrap:anywhere]">
            {exec.unresolved_disclosure}
          </div>
        )}
        {exec?.handoff && <HandoffCard handoff={exec.handoff} />}
        {detail && <EventsTimeline events={detail.events} />}
      </div>
      {!following && (
        <button
          type="button"
          onClick={jumpToBottom}
          className="absolute bottom-3 right-6 rounded-full border border-[#2a2f3a] bg-[#141821]/95 px-3 py-1 text-[11px] text-gray-300 shadow-lg transition hover:text-gray-100 max-md:right-3"
        >
          ↓ 回到底部
        </button>
      )}
      </div>

      {/* 对话输入 */}
      <div className="border-t border-[#2a2f3a] px-6 py-3 max-md:px-3">
        {(actionError || actionNotice) && (
          <div
            className={`mb-2 rounded-lg px-3 py-1.5 text-xs ${
              actionError
                ? "border border-red-500/40 bg-red-500/10 text-red-300"
                : "border border-emerald-500/30 bg-emerald-500/10 text-emerald-300"
            }`}
          >
            {actionError ?? actionNotice}
          </div>
        )}
        <div className="flex items-end gap-2">
          <textarea
            ref={inputRef}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key !== "Enter" || e.shiftKey || e.nativeEvent.isComposing) return;
              // 安卓 App：Enter 一律换行不发送（与对话页同规则：软键盘没有 Shift，发送走
              // 右侧按钮）；外接硬件键盘 Ctrl/⌘+Enter 发送。
              if (isAndroidDevice() && !(e.ctrlKey || e.metaKey)) return;
              e.preventDefault();
              void send();
            }}
            disabled={!canSend}
            rows={2}
            placeholder={
              canSend
                ? isAndroidDevice()
                  ? "给这个子 agent 发消息（追问进展、补充要求、纠正方向）… 回车换行，点右侧发送"
                  : "给这个子 agent 发消息（追问进展、补充要求、纠正方向）… Enter 发送，Shift+Enter 换行"
                : removed
                  ? "记录已清除"
                  : terminal
                    ? "执行已结束，不能再对话"
                    : "当前没有执行者（重启后恢复的记录需先 AgentResume），不能对话"
            }
            className="min-h-[2.5rem] flex-1 resize-none rounded-xl border border-[#2a2f3a] bg-[#141821] px-3 py-2 text-sm text-gray-200 placeholder:text-gray-600 focus:border-indigo-500/60 focus:outline-none disabled:cursor-not-allowed disabled:opacity-50 max-md:text-[15px]"
          />
          <button
            type="button"
            onClick={() => void send()}
            disabled={!canSend || sending || draft.trim().length === 0}
            className="shrink-0 rounded-xl bg-indigo-600 px-4 py-2 text-sm text-white transition hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-40"
          >
            {sending ? "发送中…" : "发送"}
          </button>
        </div>
        <div className="mt-1.5 flex items-center justify-between text-[10px] text-gray-600">
          <span>
            {canSend
              ? "消息在子 agent 的下一个安全点（模型调用前后）注入，回应会出现在上方活动记录里。"
              : "只有本进程正在运行的子 agent 能接收消息。"}
          </span>
          {inboxPending > 0 && <span className="text-indigo-300">{inboxPending} 条消息待子 agent 读取</span>}
        </div>
      </div>
    </div>
  );
}
