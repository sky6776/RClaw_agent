import React, {
  Fragment,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { api, type PermissionMode, type PlanStatus } from "../lib/api";
import type { ToolCallView } from "../lib/toolCalls";
import { createLongPressGuard, type LongPressGuard } from "../lib/longPress";
import { useIsMobile, isAndroidDevice } from "../lib/useIsMobile";
import { useBackClose } from "../lib/backNav";
import MediaPreviews, { parseMediaItems } from "./MediaPreviews";
import WorkflowCard from "./WorkflowCard";
import MediaGallery, { type GalleryItem, type GallerySeed } from "./MediaGallery";
import ToolBatchRows from "./ToolCallRow";
import { RoundNavButton, RoundNavSheet, RoundRail, useWidePointer, type RoundNavItem } from "./RoundNav";
import { CheckIcon, CopyIcon, GearIcon, PencilIcon, ShieldIcon } from "./icons";

export type Role = "user" | "assistant" | "status" | "thinking";
export interface Entry {
  id: number;
  role: Role;
  content: string;
  /** 条目时间（毫秒）：直播条目 = 到达时间，历史恢复的条目 = 落库时间（秒×1000）；
   *  仅 ts ≤ 0 的极旧数据为 undefined。时间标签与思考块持续时长都用它。 */
  ts?: number;
  /** 持久化消息行 id：恢复条目自带，直播条目由 turn 收尾的增量回填原位盖章；
   *  「分叉到新会话」的锚点，没有它 = 该条目尚未落库。 */
  msgId?: number;
  /** 发送失败标记：这条消息没有落库（永远拿不到行 id），不得作为「分叉到新会话」
   *  的锚点。 */
  sendFailed?: boolean;
  /** 工具调用轮（role=status）的结构化调用列表：有它时渲染为可展开的工具 / Skill
   *  调用行（content 仍是「🔧 调用工具: …」文案，供对位盖章与导出）。 */
  tools?: ToolCallView[];
  /** 该批的 ReAct 迭代号（直播 begun / finished 事件配对用）。 */
  toolIter?: number;
}

/** 待随消息发送的上传附件（已落入 agent 工作区 uploads/）。 */
interface Attachment {
  rel: string;
  abs: string;
  name: string;
}

interface Props {
  entries: Entry[];
  busy: boolean;
  onSend: (content: string) => void;
  onCancel: () => void;
  /** 上传等非致命错误经此冒泡到 App 的 toast。 */
  onError?: (message: string) => void;
  /** 之后新到的思考块是否默认收起：由用户最近一次手动折叠（true）/ 展开（false）
   *  决定，只作为新块的初始状态，不改动已在屏上的块。 */
  futureThinkingCollapsed: boolean;
  /** 用户手动收起 / 展开某个思考块时回调（携带该块操作后的收起状态）。 */
  onToggleThinking: (collapsed: boolean) => void;
  /** 外部预填草稿（系统分享进来的附件引用+文本）：变化时一次性写入输入框。 */
  /** 输入框草稿（App 持有：Chat 随页面切换卸载时未发送草稿不丢）。 */
  draft: string;
  onDraftChange: (value: string) => void;
  /** 采集动作（拍照/拍视频/录音）：提供后附件按钮变为功能菜单（安卓）；
   *  缺省时保持纯文件上传（桌面）。 */
  onCapture?: (kind: "photo" | "video" | "audio") => void;
  /** 消息选择「发送到会话」：把打包文本交 App 弹目标会话选择器（并入草稿）。 */
  onForwardToChat?: (text: string) => void;
  /** 当前会话 id：变化时重置滚动跟随并直达新会话底部（Chat 不随会话重挂载）。 */
  sessionKey: string;
  /** 从该消息锚点分叉出新会话（复制可见历史 + 自动回滚之后的文件改动，原会话不动）。 */
  onFork?: (entry: Entry) => void;
  /** 最近的 PlanRun 计划状态（流程步骤卡片；完整列表，最多 6 条）。 */
  plans: PlanStatus[];
  /** 暂停 / 继续计划（步骤卡片展开时的 ⏸/▶ 按钮）。 */
  onPlanPause?: (planId: string, paused: boolean) => void;
  /** 当前会话的排队消息（App 级状态；Chat 随页面切换卸载，队列不能活在组件里）。 */
  queue: QueueItem[];
  /** 排队入队（agent 运行期间点发送）。 */
  onQueueEnqueue: (text: string) => void;
  /** 从队列移除一条（删除 / 编辑回填 / 立即投递前）。 */
  onQueueRemove: (id: number) => void;
}

/** 排队消息条目（App 级状态，按会话分桶存放——Chat 随页面切换会卸载，组件内
 *  state 会把排着队的文本一起丢掉）。 */
export interface QueueItem {
  id: number;
  text: string;
}

/** 消息选择模式操作按钮样式。 */
const ACTION_BTN =
  "rounded-lg border border-[#2a2f3a] px-2.5 py-1 text-[11px] text-gray-300 transition hover:text-gray-100 disabled:opacity-40";

/** 长按 / 右键统一消息菜单项样式（与媒体/会话菜单同款）。 */
const MENU_ITEM =
  "block w-full px-3 py-2 text-left text-xs text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100";

/** 统一消息菜单里的媒体引用（长按媒体卡时携带，提供下载/转发/打开等动作）。 */
interface MenuMedia {
  path: string;
  display: string;
  kind: "image" | "open";
}

/** 把秒数格式化为「N 秒 / M 分 S 秒」。 */
function formatDuration(secs: number): string {
  if (secs < 60) return `${secs} 秒`;
  return `${Math.floor(secs / 60)} 分 ${secs % 60} 秒`;
}

/** ReactMarkdown 的 components 配置（模块级常量）。内联字面量会让每次渲染产生
 *  新的组件类型，React 按类型不同直接重挂载子树——表格横向滚动位置丢失、触摸
 *  目标被摘除；稳定身份后与普通组件行为一致。 */
const MD_COMPONENTS = {
  a: ({ href, children }: { href?: string; children?: React.ReactNode }) => (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      className="text-indigo-400 underline decoration-indigo-400/40 underline-offset-2 hover:text-indigo-300"
      onClick={(e) => {
        // 只放行 http(s)：file:/mailto:/javascript: 等其它协议交给 WebView 默认
        // 行为会顶掉 GUI 或有安全面。
        if (!href || !/^https?:\/\//i.test(href)) {
          e.preventDefault();
          return;
        }
        e.preventDefault();
        void api.browserOpenUrl(href).catch(() => undefined);
      }}
    >
      {children}
    </a>
  ),
  // 表格放横向滚动容器里（zcode 同款）：宽表不再把单元格挤成一字一行的竖排，
  // 超出消息列宽度的部分左右滑动查看。
  table: ({ children }: { children?: React.ReactNode }) => (
    <div className="table-scroll">
      <table className="w-max min-w-full">{children}</table>
    </div>
  ),
};

/** 助手消息正文（memo 边界）：props 全为原始类型。流式思考增量会驱动消息列表
 *  整表重渲，没有这层边界时历史正文的 ReactMarkdown 全量 re-parse 每 chunk 都要
 *  重跑（真机 WebView 83 上可感知，表现为输出期间滑动卡顿）。stripMediaMarkers
 *  也放进边界内：内容不变时连清洗都不再重复执行。 */
const AssistantMarkdown = React.memo(function AssistantMarkdown({
  content,
  selected,
  selectable,
}: {
  content: string;
  selected: boolean;
  selectable: boolean;
}) {
  return (
    <div
      className={`md-body w-full text-sm text-gray-200 max-md:text-[15px] max-md:leading-relaxed ${
        selected ? "rounded-md ring-2 ring-indigo-400" : ""
      } ${selectable ? "text-selectable" : ""}`}
    >
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={MD_COMPONENTS}>
        {stripMediaMarkers(content)}
      </ReactMarkdown>
    </div>
  );
});

/** 思考块正文的展示文本：剥「💭 思考中...」流式占位前缀（ThinkingBody 渲染与
 *  菜单「选择文字」的空正文判断共用同一口径，避免两处正则漂移）。 */
function thinkingBodyText(content: string): string {
  return content.replace(/^💭\s*思考中\.\.\.\n?/, "").trim();
}

/** 思考块正文（memo 边界）：展开且内容不变时跳过重渲，流式期间只有正在增长的
 *  那一个块真正更新。清洗（剥「💭 思考中...」前缀）在边界内完成。selectable =
 *  文字选择模式选中了本块（安卓）：正文强制显示并恢复原生选择（收起状态的块
 *  被选中时若不强制展开，选区会落在不可见内容上——选了个寂寞）。 */
const ThinkingBody = React.memo(function ThinkingBody({
  content,
  open,
  selectable,
}: {
  content: string;
  open: boolean;
  selectable: boolean;
}) {
  if (!open && !selectable) return null;
  const body = thinkingBodyText(content);
  return (
    <div
      className={`md-body border-t border-[#2a2f3a] px-4 py-2 text-xs whitespace-pre-wrap text-gray-400 max-md:px-3 ${
        selectable ? "text-selectable" : ""
      }`}
    >
      {body}
    </div>
  );
});

/** 「已工作 X 分 X 秒」折叠条（zcode 同款交互，参考 ConversationTurnGroup）：
 *  完成态显示过程耗时（首/末条目时间差，固定不随时钟增长），运行态每秒跳动。
 *  时长是展示跨度（直播到达/落库时间差），不是运行时的权威工时。秒级 tick 只在
 *  本组件内 setState，不驱动消息列表重建。 */
function WorkSegmentBar({
  running,
  startMs,
  endMs,
  open,
  collapsible,
  onToggle,
}: {
  running: boolean;
  startMs?: number;
  endMs?: number;
  open: boolean;
  collapsible: boolean;
  onToggle: () => void;
}) {
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!running) return;
    const timer = window.setInterval(() => setTick((v) => v + 1), 1000);
    return () => window.clearInterval(timer);
  }, [running]);
  let label: string;
  if (running) {
    label = startMs ? `工作中 ${formatDuration(Math.max(1, Math.round((Date.now() - startMs) / 1000)))}` : "工作中";
  } else if (startMs && endMs && endMs > startMs) {
    label = `已工作 ${formatDuration(Math.max(1, Math.round((endMs - startMs) / 1000)))}`;
  } else {
    label = "已处理";
  }
  return (
    <button
      onClick={collapsible ? onToggle : undefined}
      disabled={!collapsible}
      title={collapsible ? (open ? "收起工作过程" : "展开工作过程") : undefined}
      aria-label={label}
      aria-expanded={collapsible ? open : undefined}
      className={`flex w-full items-center gap-2 rounded-lg px-2 py-1 text-left text-xs text-gray-500 transition ${
        collapsible ? "hover:bg-[#1a2030] hover:text-gray-300" : "cursor-default"
      }`}
    >
      <span>{label}</span>
      {collapsible && (
        <span className="text-[10px] text-gray-600">{open ? "▾" : "▸"}</span>
      )}
    </button>
  );
}

/** 工作过程容器壳：折叠条 + 展开内容。open 状态由外层按段管理；children 元素
 *  身份在上层 memo 中保持稳定——开合只重排本壳，不重建消息子树。 */
function ProcessGroupShell({
  header,
  open,
  children,
}: {
  header: React.ReactNode;
  open: boolean;
  children: React.ReactNode;
}) {
  return (
    <div className="min-w-0">
      {header}
      {open && (
        <div className="ml-1 space-y-2.5 border-l-2 border-[#2a2f3a] pl-2.5 max-md:pl-2">
          {children}
        </div>
      )}
    </div>
  );
}

/** 把文本写进剪贴板：Android WebView 非安全上下文 / 无焦点时 clipboard API 报
 *  NotAllowedError: Write permission denied，回退隐藏 textarea + execCommand。 */
async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    // 落到 execCommand 兜底。
  }
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.focus();
  ta.select();
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } finally {
    ta.remove();
  }
  if (!ok) throw new Error("复制失败（剪贴板不可用）");
}

/** 剥离运行时媒体回执行（`MEDIA_MARKER=…` / `[:…-media …]` 协议行）：它们由
 *  MediaPreviews 解析成媒体卡片，正文里不再原样展示这行文本。 */
function stripMediaMarkers(content: string): string {
  return content
    .split("\n")
    .filter(
      (line) =>
        !/^\s*(?:MEDIA_MARKER=|RCLAW_VALIDATED_MEDIA_MARKER=|RCLAW_MEDIA_PUBLISH_ATTEMPTED=)/.test(
          line,
        ) && !/^\s*\[:[\w-]+-media\s/.test(line),
    )
    .join("\n");
}

/** 消息时间标签格式：当天 HH:mm，跨天 MM-DD HH:mm（本地时区）。 */
function formatEntryTime(d: Date): string {
  const now = new Date();
  const two = (n: number) => String(n).padStart(2, "0");
  const hm = `${two(d.getHours())}:${two(d.getMinutes())}`;
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  return sameDay ? hm : `${two(d.getMonth() + 1)}-${two(d.getDate())} ${hm}`;
}

/** 消息正文下方的时间标签（小号、低对比度，跟随气泡左右对齐）。
 *  没有 ts 的条目（极旧数据）不渲染，也不留空行；无法构成有效日期的 ts（损坏
 *  行）同样不渲染——toISOString 对越界值会抛错，不能让一条坏数据把整个会话渲染打断。 */
function TimeTag({ ts }: { ts?: number }) {
  if (!ts) return null;
  const d = new Date(ts);
  if (!Number.isFinite(d.getTime())) return null;
  return (
    <time
      dateTime={d.toISOString()}
      className="mt-1 px-1 text-[10px] leading-none text-gray-500"
    >
      {formatEntryTime(d)}
    </time>
  );
}

/** 引用消息 → 发送时拼装的原文块：正文（剥媒体协议行）+ 末尾补媒体文件的工作区
 *  路径（模型可凭 `[引用媒体] path` 直接 read 被引用的图片/视频/文档）。
 *  超长截断，防一条引用撑爆上下文。 */
function buildQuoteText(content: string): string {
  const text = stripMediaMarkers(content).trim();
  const seen = new Set<string>();
  const paths: string[] = [];
  const addPath = (p: string) => {
    const ws = p.replace(/\\/g, "/");
    const rel = ws.includes("/agent/")
      ? ws.slice(ws.lastIndexOf("/agent/") + "/agent/".length)
      : ws.replace(/^\/+/, "");
    if (rel && !seen.has(rel)) {
      seen.add(rel);
      paths.push(rel);
    }
  };
  let m: RegExpExecArray | null;
  // 媒体回执行里的 path（设备绝对路径 → 工作区相对路径，同 MediaPreviews）。
  const reMarker = /\[:[\w-]+-media\s+([^\]]*)\]/g;
  while ((m = reMarker.exec(content)) !== null) {
    const p = m[1].match(/\bpath="([^"]+)"/)?.[1];
    if (p) addPath(p);
  }
  // 正文里内联的工作区路径（[附件] uploads/… 等）。
  const reFull = /(?:media|uploads|output)\/[A-Za-z0-9_\-.()\u4e00-\u9fff\uff00-\uffef]+\.[A-Za-z0-9]{2,5}/g;
  while ((m = reFull.exec(content)) !== null) addPath(m[0]);
  // 截断只作用于正文；[引用媒体] 行是模型要 read 的机器路径，半截路径比没有
  // 更糟，完整保留在截断后的正文之后。
  let q = text.length > 400 ? `${text.slice(0, 400)}…（正文过长已截断）` : text;
  if (paths.length > 0) {
    const mediaLines = paths.map((p) => `[引用媒体] ${p}`).join("\n");
    q = q ? `${q}\n${mediaLines}` : mediaLines;
  }
  return q;
}

/** 消息引用小按钮：点击把该消息原文（含媒体路径）填进输入区引用条。 */
function QuoteButton({ onQuote }: { onQuote: () => void }) {
  return (
    <button
      onClick={onQuote}
      title="引用此消息"
      aria-label="引用"
      className="mt-0.5 flex items-center gap-1 rounded px-1 py-0.5 text-[10px] text-gray-500 transition hover:text-gray-200"
    >
      ❝ 引用
    </button>
  );
}

/** 消息一键复制小按钮：贴在消息正文下方，点击复制正文（助手消息为剥离媒体
 *  协议行后的文本，与消息展示一致），短暂显示对勾反馈。 */
function CopyButton({
  text,
  onError,
}: {
  text: string;
  onError?: (message: string) => void;
}) {
  const [copied, setCopied] = useState(false);
  const copiedTimer = useRef<number | undefined>(undefined);
  return (
    <button
      onClick={() => {
        copyText(text)
          .then(() => {
            setCopied(true);
            // 连续点击时重置反馈计时，避免上一次的定时器提前熄灭本次提示。
            window.clearTimeout(copiedTimer.current);
            copiedTimer.current = window.setTimeout(() => setCopied(false), 1500);
          })
          .catch((e) => onError?.(String(e)));
      }}
      title={copied ? "已复制" : "复制"}
      aria-label="复制"
      className="mt-0.5 flex items-center gap-1 rounded px-1 py-0.5 text-[10px] text-gray-500 transition hover:text-gray-200"
    >
      {copied ? <CheckIcon /> : <CopyIcon />}
      {copied && <span>已复制</span>}
    </button>
  );
}

/** 消息「分叉到新会话」小按钮（桌面内联形态）：从该消息（含）复制出一条新会话
 *  分支，并自动回滚锚点之后的文件改动，原会话不动（ZCode fork 同款语义）。
 *  移动端走长按菜单项。 */
function ForkButton({ onFork }: { onFork: () => void }) {
  return (
    <button
      onClick={onFork}
      title="从这里分叉出新会话（原会话保留）"
      aria-label="分叉到新会话"
      className="mt-0.5 flex items-center gap-1 rounded px-1 py-0.5 text-[10px] text-gray-500 transition hover:text-gray-200"
    >
      ⑂ 分叉
    </button>
  );
}

/** PlanRun 流程步骤卡片（可折叠）：当前最近的计划步骤清单钉在输入区上方，
 *  步骤状态随 plan_status 轮询刷新（zcode 的 todo 卡片同款信息位，第十二轮）。
 *  展开时提供暂停 / 继续按钮（ZCode 任务暂停同款：波次边界协作式暂停）。 */
function PlanStepsCard({
  plan,
  onPlanPause,
}: {
  plan: PlanStatus;
  onPlanPause?: (planId: string, paused: boolean) => void;
}) {
  const pending = plan.steps.filter((s) => s.status === "pending").length;
  // 未收尾的计划默认展开，收尾后默认收起（只留标题行当摘要）。
  const [open, setOpen] = useState(!plan.completed);
  // 计划切换（新 PlanRun 启动）时按新计划的收尾态重置折叠。
  const planKeyRef = useRef(plan.plan_id);
  useEffect(() => {
    if (planKeyRef.current !== plan.plan_id) {
      planKeyRef.current = plan.plan_id;
      setOpen(!plan.completed);
    }
  }, [plan.plan_id, plan.completed]);
  const icon = (status: string) => {
    if (status === "succeeded") return <span className="text-emerald-400">✓</span>;
    if (status === "failed") return <span className="text-red-400">✗</span>;
    if (status === "skipped") return <span className="text-gray-600">⤼</span>;
    if (status === "running")
      return <span className="inline-block animate-spin text-indigo-400">◌</span>;
    // 执行成功、等验证者裁决（Programmer/Prover 对抗环）——不是终态成功。
    if (status === "verifying") return <span className="text-amber-300">🔍</span>;
    return <span className="text-amber-400">⏳</span>;
  };
  return (
    <div className="mb-2 overflow-hidden rounded-xl border border-[#2a2f3a] bg-[#141821]">
      <button
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center justify-between gap-2 px-3 py-1.5 text-left text-xs text-gray-300 transition hover:text-gray-100"
      >
        <span className="flex min-w-0 items-center gap-2">
          <span className="shrink-0">📋 流程步骤</span>
          <span className="min-w-0 truncate text-[11px] text-gray-500">
            {plan.name}
            {plan.paused && " · ⏸ 已暂停"}
            {/* 已中止（取消 / 崩溃 / 进程被杀）没有 succeeded 计数，标成「已中止」而不是「完成 ?/N」。 */}
            {plan.aborted
              ? " · ⏹ 已中止"
              : plan.completed
                ? ` · 完成 ${plan.succeeded ?? "?"}/${plan.steps.length}`
                : ` · 待办 ${pending}/${plan.steps.length}`}
          </span>
        </span>
        <span className="shrink-0 text-[10px] text-gray-600">{open ? "▾" : "▸"}</span>
      </button>
      {/* 暂停 / 继续（展开时可见；未收尾的计划才有意义）——ZCode 任务暂停同款：
          当前波跑完后挂起，后续波次等待恢复；/cancel 取消语义不受影响。 */}
      {open && !plan.completed && onPlanPause && (
        <div className="flex justify-end border-t border-[#2a2f3a] px-3 py-1">
          <button
            onClick={() => onPlanPause(plan.plan_id, !plan.paused)}
            className={`rounded px-2 py-0.5 text-[11px] transition ${
              plan.paused
                ? "text-emerald-300 hover:bg-emerald-500/15"
                : "text-amber-300 hover:bg-amber-500/15"
            }`}
          >
            {plan.paused ? "▶ 继续" : "⏸ 暂停"}
          </button>
        </div>
      )}
      {open && (
        <ol className="border-t border-[#2a2f3a] px-4 py-2 text-xs text-gray-300 max-md:px-3">
          {plan.steps.map((step) => (
            <li key={step.id} className="flex items-start gap-2 py-0.5" title={step.summary}>
              <span className="mt-0.5 shrink-0 w-4 text-center">{icon(step.status)}</span>
              <span className="min-w-0">
                <span className="text-gray-500">{step.id}</span>
                {step.task && <span className="ml-1.5">{step.task}</span>}
                {step.summary && !step.task && (
                  <span className="ml-1.5 truncate text-gray-500">{step.summary}</span>
                )}
              </span>
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}

/** 可折叠的思考过程块：点击标题行（或右侧箭头）收起 / 展开。
 *  durationSecs = 下一条消息到达与本块到达的时间差（历史恢复无时间戳则不显示）。
 *  defaultOpen 只在本块挂载时生效；手动收起 / 展开经 onToggle 上报，作为之后
 *  新到块的默认状态，不影响任何已在屏上的块。selectable = 文字选择模式选中了
 *  本块（安卓）：正文强制显示、卡片放开裁剪（overflow-hidden 会裁掉选择手柄），
 *  并先把块滚入视口。标题箭头 / aria / 正文可见性统一按 expanded（手动开合 ∨
 *  selectable），避免「正文已显示、箭头还是收起」的状态矛盾；selectable 时标题
 *  仍可点（只改 open，正文因 expanded 保持显示，onToggle 上报语义不变）。 */
function ThinkingBlock({
  content,
  durationSecs,
  defaultOpen,
  onToggle,
  selectable,
}: {
  content: string;
  durationSecs: number | null;
  defaultOpen: boolean;
  onToggle?: (collapsed: boolean) => void;
  selectable: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const rootRef = useRef<HTMLDivElement | null>(null);
  // 进入选择模式后（正文已强制展开）再把块滚入视口：同步回调里正文还没展开，
  // 位置不准，layout effect 时机正合适。
  useLayoutEffect(() => {
    if (selectable) rootRef.current?.scrollIntoView({ block: "nearest" });
  }, [selectable]);
  const expanded = open || selectable;
  return (
    <div ref={rootRef} className="flex justify-start">
      <div
        className={`w-full rounded-2xl rounded-bl-md border border-dashed border-[#3a4150] bg-[#141821] ${
          selectable ? "overflow-visible" : "overflow-hidden"
        }`}
      >
        <button
          onClick={(ev) => {
            // 不冒泡：多选模式下点折叠标题不应切换整行选中。
            ev.stopPropagation();
            const nextOpen = !open;
            setOpen(nextOpen);
            // 上报操作后的收起状态（与展开状态相反），作为之后新到块的默认值。
            onToggle?.(!nextOpen);
          }}
          className="flex w-full items-center justify-between gap-3 px-4 py-1.5 text-left text-xs text-gray-400 transition hover:text-gray-200"
        >
          <span>
            💭 思考过程
            {durationSecs !== null && (
              <span className="ml-2 text-[10px] text-gray-500">
                持续了 {formatDuration(durationSecs)}
              </span>
            )}
          </span>
          <span className="text-[10px] text-gray-600">{expanded ? "▾" : "▸"}</span>
        </button>
        <ThinkingBody content={content} open={open} selectable={selectable} />
      </div>
    </div>
  );
}

/** 对话页（展示组件）：消息与忙碌状态由 App 按会话持有，切页 / 切会话不丢失。
 *  斜杠命令（/llmodel /context …）直接在输入框输入。 */
export default function Chat({
  entries,
  busy,
  onSend,
  onCancel,
  onError,
  futureThinkingCollapsed,
  onToggleThinking,
  draft,
  onDraftChange,
  onCapture,
  onForwardToChat,
  sessionKey,
  onFork,
  plans,
  onPlanPause,
  queue,
  onQueueEnqueue,
  onQueueRemove,
}: Props) {
  const listRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  // 上传附件（已写入工作区）与上传中标志；附件随消息发送后清空。
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [uploading, setUploading] = useState(false);
  const fileRef = useRef<HTMLInputElement>(null);
  // 输入历史（当前会话已发送的用户消息，按发送序）：↑↓ 遍历回填到输入框。
  // historyIndex 指向正在浏览的历史条目（null = 未在浏览）；draftRef 保存进入
  // 浏览前的草稿，↓ 越过最新一条时恢复。
  const inputHistory = useMemo(
    () => entries.filter((e) => e.role === "user").map((e) => e.content),
    [entries],
  );
  const [historyIndex, setHistoryIndex] = useState<number | null>(null);
  const draftRef = useRef("");
  // 引用条：待随下一条消息一并发送的被引消息原文（null = 未引用）。
  const [quote, setQuote] = useState<{ who: string; text: string } | null>(null);
  // 消息多选（null = 非选择模式；Set 为已选条目 id）：长按消息弹统一菜单后
  // 「多选」进入，支持打包发送到其它会话 / 导出文档 / 分享文本。
  const [selectIds, setSelectIds] = useState<Set<number> | null>(null);
  // 统一消息操作菜单（微信式）：长按 / 右键消息或媒体卡弹出，含引用 / 复制 /
  // 多选；长按媒体卡时附带媒体动作（放大 / 保存 / 转发 / 打开）。
  const [msgMenu, setMsgMenu] = useState<{
    entry: Entry;
    media: MenuMedia | null;
    x: number;
    y: number;
  } | null>(null);
  // 菜单打开时的一发式 click 吞吐：长按触发菜单后，抬指的合成 click 会落在
  // 菜单遮罩上把菜单瞬间关掉（真机/模拟器同病）。吞掉下一次 click，1 秒兜底
  // 复位（个别场景无合成 click 时不至于吞掉用户的真实点击太久）。
  const msgMenuClickGuard = useRef(false);
  const msgMenuGuardTimer = useRef<number | undefined>(undefined);
  const openMsgMenu = (
    payload: { entry: Entry; media: MenuMedia | null; x: number; y: number },
    fromTouch: boolean,
  ) => {
    if (fromTouch) {
      msgMenuClickGuard.current = true;
      window.clearTimeout(msgMenuGuardTimer.current);
      msgMenuGuardTimer.current = window.setTimeout(() => {
        msgMenuClickGuard.current = false;
      }, 1000);
    }
    setMsgMenu(payload);
  };
  // 全屏媒体画廊（图片查看器与视频播放器的统一入口）：媒体卡单击与菜单
  // 「放大查看」都从这里打开；打开时把全会话的图片/视频按消息顺序拼成
  // 时间线，左右滑切换上一个/下一个（微信行为）。seed = 点击的卡片已在手
  // 的资源（data URL / 流 URL / 卡片 <img> 元素——后者供查看器 canvas 即时
  // 预览，只有点开的这一张有）。
  const [gallery, setGallery] = useState<{
    items: GalleryItem[];
    start: string;
    seed?: GallerySeed | null;
  } | null>(null);
  // 媒体操作反馈气泡（保存成功 / 已复制；错误走 App toast）。
  const [notice, setNotice] = useState<string | null>(null);
  // 文字选择模式（安卓）：长按菜单「选择文字」进入——该条消息正文恢复原生
  // 选择（user-select:text），长按出手势选择手柄按范围复制；点「完成」退出。
  // 默认关着的原因见 index.css 的 android-app user-select 注释（原生选择
  // 手势会掐掉长按菜单的计时器，两者不能同时生效）。
  const [textSelectId, setTextSelectId] = useState<number | null>(null);
  // 切会话 / 新会话时退出文字选择模式（选区挂在旧会话的正文上，留着只会
  // 显示一个没有可选正文的「完成选择」钮）。
  useEffect(() => {
    setTextSelectId(null);
    window.getSelection?.()?.removeAllRanges();
  }, [sessionKey]);
  const noticeTimer = useRef<number | undefined>(undefined);
  const [exporting, setExporting] = useState(false);
  const [exportResult, setExportResult] = useState<string | null>(null);
  const [exportNotice, setExportNotice] = useState<string | null>(null);
  // 工具权限模式（null = 尚未加载到，隐藏选择器）。
  const [permMode, setPermMode] = useState<PermissionMode | null>(null);
  const [permOpen, setPermOpen] = useState(false);
  // 附件功能菜单（安卓：上传/拍照/拍视频/录音）。
  const [attachMenuOpen, setAttachMenuOpen] = useState(false);
  // 思考块批量开合的代次：每次一键展开/折叠 +1，强制所有 ThinkingBlock 按新
  // 默认态重挂载（defaultOpen 只在挂载时生效，仅改 prop 不影响已挂载的块）。
  const [thinkingGen, setThinkingGen] = useState(0);
  // 工作过程折叠段的显隐（按段 key = 段首条目 id）：缺省 = 收起（历史段）；运行中
  // 段强制展开不占此表；busy→false 完成时补一次 true（完成瞬间不收缩）。切会话清空。
  const [openSegs, setOpenSegs] = useState<Record<number, boolean>>({});
  // 运行段集合 ref（仅提交阶段写入，见 listBlocks 后的 useLayoutEffect）与上一
  // 提交的快照（完成段对比用）。busy→false 的首次渲染从 ref 读到「上次提交还在
  // 运行」——不等 effect 补账（否则过程先卸载再重挂：思考块局部状态丢失、跟随
  // effect 按收缩后的高度拉底，长过程把最终正文推出视口）。
  const runningKeysRef = useRef<Set<number>>(new Set());
  const prevRunningSegKeysRef = useRef<Set<number>>(new Set());
  // 工作段开合的结算代次：与 thinkingGen 共用同一套「动作前跟随态 + 滚动位置 →
  // 结算」机制（思考块批量开合引入，见下方结算 effect）。
  const [foldGen, setFoldGen] = useState(0);
  // 移动端无物理键盘：占位文案去掉 Enter/↑↓ 提示。
  const isMobile = useIsMobile();
  useEffect(() => {
    api
      .permissionModeGet()
      .then(setPermMode)
      .catch(() => {});
  }, []);

  const changePermMode = async (mode: PermissionMode) => {
    setPermOpen(false);
    if (mode === permMode) return;
    const prev = permMode;
    setPermMode(mode); // 乐观更新，失败回滚。
    try {
      await api.permissionModeSet(mode);
    } catch (e) {
      setPermMode(prev);
      onError?.(String(e));
    }
  };

  // 自动跟随滚动：仅当用户停在底部附近（含初始未滚动）时才跟随；用户向上翻阅
  // 历史时不再强制拉回底部，LLM 照常输出，重新滚回底部即恢复跟随。
  const stickToBottomRef = useRef(true);
  // 手指按在消息流上的手势状态。按住期间程序化跟随滚动必须让位：流式期间
  // 每个 chunk 都 scrollTo 底部，会把用户正在进行的上滑拖动原样压回去——
  // 位置被拉回底部、跟随标志又被滚动事件置真，表现为「和 agent 抢屏幕，
  // 要滑好几次才滑得动」。ids 按触点 identifier 跟踪：其它区域（输入框等）
  // 的手指会出现在 touches 里但事件不在列表上派发，手势是否结束只能凭 id 判。
  // start* 供松手时判定手势意图，多指按下时不覆写；moved 区分轻点与拖动。
  // zombie：手势已结算但场上还有未归属触点（e.touches.length>0）——可能是
  // 别的区域（输入框）也可能仍按在本列表上未移动的手指。抑制跟随滚动继续
  // 生效，直到该触点 move 到来被重新接管、ghost finish 落地或看门狗兜底，
  // 避免「标志已结算、effect 却跟残留手指抢屏」。
  const touchStateRef = useRef({
    active: false,
    zombie: false,
    ids: new Set<number>(),
    startX: 0,
    startY: 0,
    startScrollTop: 0,
    moved: false,
  });
  // 最近一次滚动事件里的 scrollTop：只有朝底部方向（scrollTop 严格增大）的
  // 滚动才允许恢复跟随——「向上翻」手势自己产生的滚动事件不能把刚脱离的跟随
  // 又打开（滚轮一格不足 80px 容差时尤其如此，否则慢速翻阅永远逃不出拉底）。
  // 程序化拉底后也要同步刷新它：scroll 通知是异步的，带着旧基线时，任何小于
  // 新位置的 scrollTop（如用户随即上滚）都会被误判成「朝底」。
  const lastScrollTopRef = useRef(0);
  // 最近一次触摸事件的时刻 + 手势看门狗。流式重渲染可能替换触摸目标（如
  // <p>→<h2> 换节点），后续 touchend 派发到已从文档摘除的节点、列表 capture
  // 收不到，ids 残留会把跟随压制卡死；而 follow effect 靠内容变化驱动，输出
  // 结束后不再重跑，兜不住这种「静默卡死」——必须用真定时器：长时间无触摸
  // 事件即强制复位（手指还活着则续期）。
  const lastTouchAtRef = useRef(0);
  // 松手 / 手势终止后的跟随重判（handleListTouchFinish 与看门狗共用）：
  // 无溢出、恰钉正底部、或贴近底部且无净上翻位移 → 恢复跟随并补拉底（按住
  // 期间内容可能已增长，跟随 effect 被按住跳过不会补跑）；否则保持脱离。
  // allowScroll=false 跳过补拉底（仅定标志）：用于场上还有未知归属触点时，
  // 避免跟仍按在本列表上的手指抢屏。
  const settleFollowAfterGesture = (
    st: { startScrollTop: number },
    el: HTMLDivElement,
    allowScroll = true,
  ) => {
    const noOverflow = el.scrollHeight - el.clientHeight <= 0;
    const atExactBottom = el.scrollTop === el.scrollHeight - el.clientHeight;
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    if (noOverflow || atExactBottom || (nearBottom && el.scrollTop >= st.startScrollTop)) {
      stickToBottomRef.current = true;
      setShowJumpDown(false);
      if (allowScroll) {
        el.scrollTo({ top: el.scrollHeight });
        lastScrollTopRef.current = el.scrollTop;
      }
    } else {
      stickToBottomRef.current = false;
      setShowJumpDown(true);
    }
  };
  const touchWatchdogRef = useRef<number | null>(null);
  const clearTouchWatchdog = () => {
    if (touchWatchdogRef.current !== null) {
      window.clearTimeout(touchWatchdogRef.current);
      touchWatchdogRef.current = null;
    }
  };
  const armTouchWatchdog = () => {
    clearTouchWatchdog();
    touchWatchdogRef.current = window.setTimeout(() => {
      touchWatchdogRef.current = null;
      const st = touchStateRef.current;
      if (!st.active && !st.zombie) return;
      if (performance.now() - lastTouchAtRef.current <= 10000) {
        armTouchWatchdog(); // 手指还在（10s 内有过触摸事件），继续等
        return;
      }
      st.active = false;
      st.zombie = false;
      st.ids.clear();
      clearZombieObserver();
      // 手势被丢失（目标节点被替换、finish 没送到）：按当前位置做一次与
      // 正常松手相同的结算，输出可能已结束、不会再有 chunk 触发 effect。
      const el = listRef.current;
      if (el) settleFollowAfterGesture(st, el);
    }, 12000);
  };
  // zombie 的外区触点观察：zombie 期间按在别处（输入框等）的手指抬起时，
  // touchend 发往其起始元素、不经过列表 capture，zombie 只能等看门狗超时
  // 解除（跟随被白抑制 ~12s，且期间内容增长会让兜底结算误判成脱离）。
  // zombie 期间挂 window capture 触摸监听（capture 自 window 下行，任何
  // 连接目标的 touchend 都能看到）：全部手指抬起即解除抑制，跟随标志维持
  // 手势结算时的结果。
  const zombieObserverRef = useRef<((e: TouchEvent) => void) | null>(null);
  const clearZombieObserver = () => {
    if (zombieObserverRef.current) {
      window.removeEventListener("touchend", zombieObserverRef.current, true);
      window.removeEventListener("touchcancel", zombieObserverRef.current, true);
      zombieObserverRef.current = null;
    }
  };
  const armZombieObserver = () => {
    clearZombieObserver();
    const onZombieTouchEnd = (e: TouchEvent) => {
      if (e.touches.length > 0) return; // 还有触点按着（不管在哪个区域）
      touchStateRef.current.zombie = false;
      clearTouchWatchdog();
      clearZombieObserver();
      // 解除抑制后按已结算的跟随标志补拉底：zombie 期间输出可能增长且已
      // 结束（follow effect 被抑制跳过、busy 翻转也已消费，不会再重跑），
      // 只清标志会把最后一段内容留在视口下方且无时机触发。
      const el = listRef.current;
      if (el && stickToBottomRef.current) {
        el.scrollTo({ top: el.scrollHeight });
        lastScrollTopRef.current = el.scrollTop;
      }
    };
    zombieObserverRef.current = onZombieTouchEnd;
    window.addEventListener("touchend", onZombieTouchEnd, {
      capture: true,
      passive: true,
    });
    window.addEventListener("touchcancel", onZombieTouchEnd, {
      capture: true,
      passive: true,
    });
  };
  const [showJumpDown, setShowJumpDown] = useState(false);
  // 流式思考会让最后一条的 content 持续增长：把它加进依赖，流式期间跟随滚动。
  const lastContentLength = entries[entries.length - 1]?.content.length ?? 0;
  useEffect(() => {
    if (!stickToBottomRef.current || touchStateRef.current.active || touchStateRef.current.zombie)
      return;
    const el = listRef.current;
    if (!el) return;
    el.scrollTo({ top: el.scrollHeight });
    lastScrollTopRef.current = el.scrollTop;
  }, [entries.length, lastContentLength, busy]);

  // 滚动时判定是否贴近底部（80px 容差）：决定后续输出是否继续跟随。
  const handleListScroll = () => {
    const el = listRef.current;
    if (!el) return;
    // 轮次导航的当前轮跟随滚动刷新（rAF 节流，见下方 computeActiveRound）。
    scheduleActiveRound();
    // 批量开合思考块的布局滚动抑制窗：钳位/结算复位产生的滚动事件（位置等于
    // 基线）只吞掉不改判——脱离态复原不被 atExactBottom / 无溢出分支翻转；
    // 位置与基线不同的滚动（键盘翻页 / 滚动条 / 滚轮等真实滚动）立即结束抑制
    // 并照常参与跟随判定（含回底恢复），不会被窗口吞掉。
    if (performance.now() < scrollSettleSuppressUntilRef.current) {
      if (el.scrollTop === lastScrollTopRef.current) {
        return;
      }
      scrollSettleSuppressUntilRef.current = 0;
    }
    if (touchStateRef.current.active) {
      lastScrollTopRef.current = el.scrollTop;
      return; // 拖动中的位置混着流式拉底，不据它改判跟随
    }
    // 内容全部可见（无溢出，scrollTop 被钳在 0）等价于停在底部：必须恢复
    // 跟随——否则放大窗口甩下的脱离态会卡死（严格增大条件不再满足，wheel
    // 又因无溢出提前返回，后续再溢出也不会跟随）。
    if (el.scrollHeight - el.clientHeight <= 0) {
      stickToBottomRef.current = true;
      lastScrollTopRef.current = el.scrollTop;
      setShowJumpDown(false);
      return;
    }
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    if (nearBottom) {
      // 严格增大才恢复：滚轮平滑动画首帧 scrollTop 可能尚未变化（相等），
      // 用 >= 会把同一格向上滚的滚动事件当成「朝底」把刚脱离的跟随接回去。
      // 例外：恰好钉在正底部（内容收缩/窗口放大把位置钳回底部，scrollTop
      // 是减小的）也算恢复——向上翻的滚动不可能停在正底部。
      const atExactBottom = el.scrollTop === el.scrollHeight - el.clientHeight;
      if (el.scrollTop > lastScrollTopRef.current || atExactBottom) {
        stickToBottomRef.current = true;
      }
    } else if (!isAndroidDevice()) {
      // 远离底部即脱离：让滚动条拖动 / PageUp / Home 这些不经 touch/wheel
      // 的路径也能脱离跟随。安卓 App 不走这条：触摸滚动全部经手势意图判定
      // （touchmove/松手结算），纯位置判定会被流式重排期的非输入滚动事件
      // （anchoring/软键盘重排等）误触发，把跟随无声甩掉（真机复现：流式
      // 中悬浮钮无故出现）。必须调用时求值：模块常量会在 main.tsx 挂上
      // android-app 类之前求值（静态 import 先行），把安卓错判成桌面。
      stickToBottomRef.current = false;
    }
    lastScrollTopRef.current = el.scrollTop;
    // 脱离跟随后可以仍停在容差内（如触控板在底部上滚 10px）：按钮此时
    // 承担「恢复跟随」入口，不能只按 !nearBottom 显示。
    setShowJumpDown(!nearBottom || !stickToBottomRef.current);
  };

  // 用户手势即时接管：触摸下滑超过 touch slop（12px，与长按菜单同值）即脱离
  // 跟随，不等 80px 容差——流式 chunk 间隙内根本拖不出容差（抢屏根因）；
  // 滚轮向上同理。列表完全无溢出时跳过：无可翻内容，脱离只会让悬浮钮常驻。
  // 触摸四事件挂 capture：媒体卡等子元素的 onTouchStart 会 stopPropagation
  // （防两套长按计时器叠加），bubble 阶段列表收不到起始事件，从卡片上开始的
  // 拖动会漏判；capture 从祖先往下先于目标元素执行，不受其影响。
  const handleListTouchStart = (e: React.TouchEvent) => {
    const st = touchStateRef.current;
    const first = e.changedTouches[0];
    const el = listRef.current;
    lastTouchAtRef.current = performance.now();
    if (!first || !el) return;
    if (!st.active) {
      st.active = true;
      st.zombie = false;
      clearZombieObserver();
      st.startX = first.clientX;
      st.startY = first.clientY;
      st.startScrollTop = el.scrollTop;
      st.moved = false;
    }
    for (let i = 0; i < e.changedTouches.length; i++) {
      st.ids.add(e.changedTouches[i].identifier);
    }
    armTouchWatchdog();
  };
  const handleListTouchMove = (e: React.TouchEvent) => {
    const st = touchStateRef.current;
    if (!st.active || st.zombie) {
      // 两种「无主手指」都会继续来 move：看门狗把仍按住的手指误清了（静止
      // 超时），或上一个手势结算时场上还有未归属触点（zombie）。在这里重新
      // 接管——touch 事件始终以触摸起始元素为目标，能到本列表 capture 的
      // move 必然起始自列表内。不接管的话后续拖动失去压制，流式 chunk 又会
      // 跟手指抢屏。
      const t = e.changedTouches[0];
      const el = listRef.current;
      if (!t || !el) return;
      st.active = true;
      st.zombie = false;
      clearZombieObserver();
      st.ids.clear();
      for (let i = 0; i < e.changedTouches.length; i++) {
        st.ids.add(e.changedTouches[i].identifier);
      }
      st.startX = t.clientX;
      st.startY = t.clientY;
      st.startScrollTop = el.scrollTop;
      st.moved = true; // 中途接管：松手按「拖动过」处理，不做轻点让位
      lastTouchAtRef.current = performance.now();
      armTouchWatchdog();
      return; // 起点刚重置，本事件不做脱离判定
    }
    lastTouchAtRef.current = performance.now();
    const el = listRef.current;
    if (!el || el.scrollHeight - el.clientHeight <= 0) return;
    for (let i = 0; i < e.changedTouches.length; i++) {
      const t = e.changedTouches[i];
      if (!st.ids.has(t.identifier)) continue;
      if (Math.hypot(t.clientX - st.startX, t.clientY - st.startY) > 12) {
        st.moved = true;
      }
      if (t.clientY - st.startY > 12 && stickToBottomRef.current) {
        stickToBottomRef.current = false;
        setShowJumpDown(true);
        break;
      }
    }
  };
  // touchend / touchcancel 共用：id 全部抬起才算手势结束，并按松手位置
  // 重判一次跟随——按住期间内容增长可能已把位置顶离底部（此时跟随效果
  // 被按住跳过，不会补跑），留在原地的「假跟随」必须在这里显式摘掉。
  const handleListTouchFinish = (e: React.TouchEvent) => {
    const st = touchStateRef.current;
    lastTouchAtRef.current = performance.now();
    for (let i = 0; i < e.changedTouches.length; i++) {
      st.ids.delete(e.changedTouches[i].identifier);
    }
    if (st.ids.size > 0) return; // 还有手指按在消息流上，手势未结束
    clearTouchWatchdog();
    const el = listRef.current;
    // 「安全结算」先行（无溢出 / 恰钉正底部）：不依赖手势起点、位置已在底部
    // 无拉底动作，任何路径（残余触点直接松开、切会话后迟到的 finish）在此
    // 恢复跟随都不会误伤。依赖起点的完整结算仍在下面按各路径 gate 执行。
    if (el) {
      const noOverflow = el.scrollHeight - el.clientHeight <= 0;
      const atExactBottom = el.scrollTop === el.scrollHeight - el.clientHeight;
      if (noOverflow || atExactBottom) {
        stickToBottomRef.current = true;
        setShowJumpDown(false);
      }
    }
    // zombie 期间的 ghost finish：未登记的列表触点直接抬起（没有 move、没
    // 经过重新接管）。位置分支照常结算；起点相关判定沿用现存 startScrollTop
    // （最可信的现存值）。仍有残余触点则继续 zombie。
    if (st.zombie) {
      st.zombie = false;
      clearZombieObserver();
      if (el) settleFollowAfterGesture(st, el, e.touches.length === 0);
      if (e.touches.length > 0) {
        st.zombie = true;
        armTouchWatchdog();
        armZombieObserver();
      }
      return;
    }
    // 切会话等场景已把手势复位：迟到的 finish 不许用旧起点改判新状态。
    if (!st.active) return;
    st.active = false;
    if (!el) return;
    // 点在「回到底部」钮上的轻点（未拖动）：不在此按位置重判——近底轻点会
    // 先恢复跟随并隐藏按钮，随后的合成 click 落在已卸载节点上落空，滚动不会
    // 发生，交给按钮自己的 click（scrollListToBottom）。拖动过（moved）视为
    // 普通手势，正常重判。
    if (!st.moved && e.target instanceof Element && e.target.closest("[data-jump-down]")) {
      return;
    }
    // 依赖起点的完整结算：有残留触点（e.touches.length>0，可能在别的区域，
    // 也可能是 watchdog 误清后仍按在本列表上未移动的手指）也要执行——外部
    // 触点不能吞掉本次手势的结论，否则松开列表手指后标志停留在过期值（无
    // 按钮却脱离 / 贴底却继续拉底）。但此时跳过补拉底：跟仍按在列表上的
    // 未知手指抢屏比晚一步拉底更伤。它随后的 move 会重新接管，松手时按它
    // 的起点再做带拉底的完整结算。
    settleFollowAfterGesture(st, el, e.touches.length === 0);
    if (e.touches.length > 0) {
      // 残留触点抑制跟随滚动继续生效，直到重新接管 / ghost finish / 外区
      // 触点全部抬起 / 看门狗兜底。
      st.zombie = true;
      armTouchWatchdog();
      armZombieObserver();
    }
  };
  const handleListWheel = (e: React.WheelEvent) => {
    const el = listRef.current;
    if (!el || el.scrollHeight - el.clientHeight <= 0) return;
    if (e.deltaY < 0) {
      stickToBottomRef.current = false;
      setShowJumpDown(true);
    }
  };
  // 窗口尺寸变化会改变溢出状态：放大到内容全部可见时 scrollTop 被钳回 0、
  // 不再派发 scroll 事件；放大到仍溢出时位置也会被钳到新底部（scrollTop
  // 减小着到达底部，被 handleListScroll 的严格增大条件拒绝）——两种都等价
  // 停在底部，须恢复跟随。只认「位置被钳动过」（scrollTop 与最近一次滚动
  // 事件记录不同）：用户停在半空的自由位置不因缩放窗口而被拉底。窄化放大
  // 溢出的场景交给后续滚动判定。
  useEffect(() => {
    const onResize = () => {
      const el = listRef.current;
      if (!el || touchStateRef.current.active) return;
      const noOverflow = el.scrollHeight - el.clientHeight <= 0;
      // 只认「位置被钳动过」（scrollTop 与最近一次滚动事件记录不同）：用户
      // 停在半空的自由位置不因缩放窗口而被拉底。无溢出例外——短列表贴顶时
      // scrollTop 恒为 0，放大窗口不产生位移，但「全部可见」本身等价于贴底。
      const clamped = el.scrollTop !== lastScrollTopRef.current;
      const atBottom = noOverflow || el.scrollHeight - el.scrollTop - el.clientHeight < 80;
      if (atBottom && (noOverflow || clamped)) {
        stickToBottomRef.current = true;
        setShowJumpDown(false);
        lastScrollTopRef.current = el.scrollTop;
      }
    };
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);

  // 回到底部并恢复跟随（发送消息 / 点击悬浮钮时调用）。
  const scrollListToBottom = (smooth = false) => {
    stickToBottomRef.current = true;
    setShowJumpDown(false);
    // 用户显式回底：zombie 的滚动抑制随即解除（抑制是为「未知手指还在列表
    // 上」，显式操作以用户意志优先）。
    touchStateRef.current.zombie = false;
    clearZombieObserver();
    const el = listRef.current;
    if (!el) return;
    el.scrollTo({
      top: el.scrollHeight,
      behavior: smooth ? "smooth" : "auto",
    });
    lastScrollTopRef.current = el.scrollTop;
  };

  // 切换会话：恢复跟随并把视图直达新会话最新消息（DOM 已随 props 更新完再滚）。
  const sessionKeyRef = useRef(sessionKey);
  useEffect(() => {
    if (sessionKeyRef.current === sessionKey) return;
    sessionKeyRef.current = sessionKey;
    stickToBottomRef.current = true;
    setShowJumpDown(false);
    // 切会话清附件与 ↑↓ 浏览态（附件发错对象/历史错位）；草稿清空由 App 的
    // openChat/newChat 入口同步完成——若在此清，分享确认「先切会话再并入」
    // 的文本会被这个后跑的 effect 抹掉。排队消息不清：队列在 App 按会话分桶，
    // 这里显示的只是当前会话的那一份，切回来还在。工作段折叠态的清空在提交
    // 阶段的会话边界 layout effect（下方）里做——这里清会把维护 effect 刚记录
    // 的运行集合抹掉且同次提交不再重跑。
    setAttachments([]);
    setHistoryIndex(null);
    setQuote(null);
    setSelectIds(null);
    setExportResult(null);
    draftRef.current = "";
    // 手势态一并复位：残留的 active/zombie/ids 会把跟随压制带到新会话。
    touchStateRef.current.active = false;
    touchStateRef.current.zombie = false;
    touchStateRef.current.ids.clear();
    clearTouchWatchdog();
    clearZombieObserver();
    const el = listRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
      lastScrollTopRef.current = el.scrollTop;
    }
  }, [sessionKey]);

  // 会话边界（提交阶段）：清工作段折叠态与运行集合 ref。必须声明在下方维护
  // effect 之前（同阶段按声明序执行：先重置、再按新会话记录）——否则「切到运行
  // 中的会话」时维护 effect 写入的集合会被这里清掉，且 listBlocks 未变时维护
  // effect 不再重跑，完成渲染会读到空集而折叠。
  useLayoutEffect(() => {
    // 本就为空时复用原对象：首帧 / 从未展开过的会话切换不多触发一次提交渲染。
    setOpenSegs((prev) => (Object.keys(prev).length > 0 ? {} : prev));
    runningKeysRef.current = new Set();
    prevRunningSegKeysRef.current = new Set();
  }, [sessionKey]);

  // 排队消息（第十二轮 · zcode 同款）：agent 运行期间发送的消息进入队列，显示在
  // 输入框上方，可删除 / 编辑 / 「立即」强制投递（走运行中的 turn 的插队通道）。
  // 队列本体活在 App（按会话分桶）：Chat 随页面切换（工作区/设置面板）会整体卸载，
  // 组件内 state 会把排着队的文本一起丢掉；逐条投递也由 App 的收尾监听统一驱动，
  // 人在哪个页面/会话都不影响排队消息按序发出。这里拿到的是当前会话的队列。

  // 思考块一键开合：按「之后新块的默认态」取反作为本次动作——当前默认展开则全部
  // 折叠（未来新块也默认折叠），反之全部展开。动作型 title/aria（不声明各块的
  // aria-expanded：混合开合时该属性既说不准也不必说）。
  const hasThinkingEntries = useMemo(
    () => entries.some((entry) => entry.role === "thinking"),
    [entries],
  );
  // 动作前的跟随状态与滚动位置（结算 effect 用）：折叠会让列表变矮，浏览器可能
  // 把超界的 scrollTop 钳到新底部并派发滚动事件，handleListScroll 的 atExactBottom
  // /无溢出分支会据此恢复跟随——不保存动作前状态的话，用户的脱离态会被这次布局
  // 翻转。滚动抑制窗：结算后一小段时间内，位置等于基线的滚动事件（钳位/复位
  // 布局产物）不改判跟随；真实滚动（位置变化）立即解除抑制并正常判定。
  const thinkingSettleRef = useRef<{ wasFollowing: boolean; scrollTop: number } | null>(
    null,
  );
  const scrollSettleSuppressUntilRef = useRef(0);
  const captureSettle = () => {
    thinkingSettleRef.current = {
      wasFollowing: stickToBottomRef.current,
      scrollTop: listRef.current?.scrollTop ?? 0,
    };
  };
  const toggleAllThinking = () => {
    const collapsed = !futureThinkingCollapsed;
    captureSettle();
    onToggleThinking(collapsed);
    setThinkingGen((gen) => gen + 1);
  };

  // 工作过程折叠段开合：与思考块批量开合同一套滚动结算（captureSettle + bump 代次）。
  const toggleSegment = (key: number) => {
    captureSettle();
    setOpenSegs((prev) => ({ ...prev, [key]: !(prev[key] ?? false) }));
    setFoldGen((gen) => gen + 1);
  };

  // 退出选择模式统一入口：先 capture 动作前的跟随态/位置并 bump 结算代次，再清
  // 选择。清空会让强制展开的段收回、高度骤减触发钳位——被动的 effect 在 DOM 收缩
  // 之后才跑，钳位滚动可能已把脱离态翻转（系统返回键等外部路径同样走这里）。
  const exitSelection = () => {
    if (selectIds === null) return;
    captureSettle();
    setFoldGen((gen) => gen + 1);
    setSelectIds(null);
  };

  // 批量开合（思考块 / 工作段）后的滚动结算：列表高度变了，但现有跟随 effect 只
  // 依赖条数/末条长度/busy，不会因此重跑。动作前脱离 → 复原脱离（把位置钳回保存
  // 值 + 开抑制窗，后续的钳位滚动事件不再改判跟随）；动作前跟随 → 补拉底（展开
  // 可能把最新消息顶出视口）。不做无条件拉底。
  useEffect(() => {
    if (thinkingGen === 0 && foldGen === 0) return;
    const settle = thinkingSettleRef.current;
    thinkingSettleRef.current = null;
    if (touchStateRef.current.active || touchStateRef.current.zombie) return;
    const el = listRef.current;
    if (!el) return;
    if (settle && !settle.wasFollowing) {
      const maxTop = el.scrollHeight - el.clientHeight;
      if (maxTop <= 0) {
        // 折叠后内容全部可见（无溢出）：与 handleListScroll / onResize 的无溢出分支
        // 同口径，等价于停在底部——恢复跟随、不开抑制窗。否则这里置脱离 + 抑制窗会
        // 吞掉钳位滚动事件，此后无溢出再无滚动事件、滚轮也提前返回，脱离态卡死：
        // 下一次流式输出在视口下方增长却不跟随（悬浮钮挂在没有滚动条的列表上）。
        stickToBottomRef.current = true;
        lastScrollTopRef.current = el.scrollTop;
        setShowJumpDown(false);
        return;
      }
      el.scrollTop = Math.min(settle.scrollTop, maxTop);
      stickToBottomRef.current = false;
      lastScrollTopRef.current = el.scrollTop;
      scrollSettleSuppressUntilRef.current = performance.now() + 600;
      setShowJumpDown(true);
      return;
    }
    if (stickToBottomRef.current) {
      el.scrollTo({ top: el.scrollHeight });
      lastScrollTopRef.current = el.scrollTop;
    } else {
      const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
      setShowJumpDown(!nearBottom || !stickToBottomRef.current);
    }
  }, [thinkingGen, foldGen]);

  const send = () => {
    const content = draft.trim();
    if (!content && attachments.length === 0 && !quote) return;
    if (uploading) return;
    // 运行中：入队等待（不再丢弃输入）；空闲：直接发送。
    if (busy) {
      const lines = attachments.map((a) => `[附件] ${a.rel}（已放入 agent 工作区）`);
      let body = content;
      if (quote) {
        const quoted = quote.text
          .split("\n")
          .map((l) => `> ${l}`)
          .join("\n");
        body = `> 引用${quote.who}消息：\n${quoted}\n\n${content}`;
      }
      const text = [...lines, body].filter(Boolean).join("\n");
      onQueueEnqueue(text);
      onDraftChange("");
      setAttachments([]);
      setHistoryIndex(null);
      setQuote(null);
      return;
    }
    // 附件以工作区相对路径逐行注入消息头，模型可以直接 read。
    const lines = attachments.map((a) => `[附件] ${a.rel}（已放入 agent 工作区）`);
    // 引用块：markdown 引用行拼在正文前，模型与会话历史文件都天然可读。
    let body = content;
    if (quote) {
      const quoted = quote.text
        .split("\n")
        .map((l) => `> ${l}`)
        .join("\n");
      body = `> 引用${quote.who}消息：\n${quoted}\n\n${content}`;
    }
    onDraftChange("");
    setAttachments([]);
    setHistoryIndex(null);
    setQuote(null);
    onSend([...lines, body].filter(Boolean).join("\n"));
    scrollListToBottom(); // 回到底部跟随新回复
  };

  /** 「立即」发送排队消息：绕过 busy 直接投递（运行时把消息作为插队并入当前 turn）。 */
  const sendQueuedNow = (item: QueueItem) => {
    onQueueRemove(item.id);
    onSend(item.text);
  };

  /** 编辑排队消息：回填输入框并从队列移除。 */
  const editQueued = (item: QueueItem) => {
    onQueueRemove(item.id);
    onDraftChange(item.text);
    inputRef.current?.focus();
  };

  // 历史回填：写入输入框并把光标移到末尾（否则光标停在原位，连续 ↑↓ 会被
  // 文本框默认的光标移动吃掉一次按键）。
  const setHistoryInput = (text: string) => {
    onDraftChange(text);
    requestAnimationFrame(() => {
      const el = inputRef.current;
      if (el) el.setSelectionRange(el.value.length, el.value.length);
    });
  };

  const pickFiles = () => fileRef.current?.click();

  const onFilesPicked = async (files: FileList | null) => {
    if (!files || files.length === 0) return;
    setUploading(true);
    for (const file of Array.from(files)) {
      try {
        const dataUrl = await new Promise<string>((resolve, reject) => {
          const reader = new FileReader();
          reader.onload = () => resolve(String(reader.result));
          reader.onerror = () => reject(reader.error ?? new Error("读取文件失败"));
          reader.readAsDataURL(file);
        });
        const base64 = dataUrl.slice(dataUrl.indexOf(",") + 1);
        const uploaded = await api.uploadFile(file.name, base64);
        setAttachments((prev) => [
          ...prev,
          { rel: uploaded.rel_path, abs: uploaded.abs_path, name: file.name },
        ]);
      } catch (e) {
        onError?.(String(e));
      }
    }
    setUploading(false);
    if (fileRef.current) fileRef.current.value = "";
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      // 安卓 App：Enter 一律换行不发送（软键盘没有 Shift，回车换行才符合
      // 手机输入习惯；发送走右侧发送钮）。外接硬件键盘用 Ctrl/⌘+Enter 发送。
      if (isAndroidDevice() && !(e.ctrlKey || e.metaKey)) return;
      e.preventDefault();
      send();
      return;
    }
    // 输入历史遍历：↑ 无条件进入浏览并从最近一条向更早回填（进入时保存当前
    // 正在编辑的草稿）；↓ 向后回退到更新，越过最新一条时恢复进入浏览前的草稿。
    // IME 组合输入中不接管（↑↓ 用于候选导航）。手动编辑草稿即退出浏览（见 onChange）。
    if (e.key !== "ArrowUp" && e.key !== "ArrowDown") return;
    if (e.nativeEvent.isComposing) return;
    if (inputHistory.length === 0) return;
    const browsing = historyIndex !== null && historyIndex < inputHistory.length;
    if (e.key === "ArrowUp") {
      e.preventDefault();
      const next =
        browsing && historyIndex !== null
          ? Math.max(0, historyIndex - 1)
          : inputHistory.length - 1;
      if (!browsing) draftRef.current = draft;
      setHistoryIndex(next);
      setHistoryInput(inputHistory[next]);
    } else if (browsing && historyIndex !== null) {
      e.preventDefault();
      if (historyIndex >= inputHistory.length - 1) {
        setHistoryIndex(null);
        setHistoryInput(draftRef.current);
      } else {
        setHistoryIndex(historyIndex + 1);
        setHistoryInput(inputHistory[historyIndex + 1]);
      }
    }
  };

  const stopOrSend = () => {
    if (busy) {
      onCancel();
    } else {
      send();
    }
  };

  // ---- 统一消息菜单（长按 / 右键）与媒体动作 ----

  // 多选覆盖全部条目类型（用户/助手/思考/工具行都只是文字信息）。
  const selectableEntries = entries;
  const selectedCount = selectIds?.size ?? 0;

  const toggleSelected = (id: number) => {
    setSelectIds((prev) => {
      if (!prev) return prev;
      const next = new Set(prev);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  };

  // 消息行的长按 / 右键 → 统一操作菜单（用户/助手/思考/工具行通用；媒体卡有
  // 独立入口且已 stopPropagation，不会同时触发）。触摸序列交给 lib/longPress
  // 的 window capture 守卫：流式重渲染会替换触摸目标（行级 onTouchMove 派发到
  // 已摘除的旧节点收不到，slop 取消失效——「正常滑动也弹长按」的根因），capture
  // 自 window 下行不受影响；触发前还校验列表滚动漂移（慢滚取消）与条目仍存在。
  // 同一时刻只有一个长按手势在途，全部行共享一个守卫是安全的。
  // 挂在整行（而非气泡内层）上：行内任意位置长按都可触发，包括气泡外空白。
  const pressGuardRef = useRef<LongPressGuard | null>(null);
  const pressEntryIdRef = useRef<number | null>(null);
  // 流式期间 entries 每个 chunk 都在变：触发时按 id 取最新条目，菜单复制到的
  // 是按下→触发之间增长后的完整内容。
  const entriesRef = useRef(entries);
  entriesRef.current = entries;
  const getPressGuard = () => {
    // lazy init：useRef(createLongPressGuard(...)) 会在每次渲染新建实例，
    // 在途手势容易失联。
    if (!pressGuardRef.current) {
      pressGuardRef.current = createLongPressGuard({
        getScrollTop: () => listRef.current?.scrollTop ?? null,
        isStillValid: () => {
          const id = pressEntryIdRef.current;
          return id === null || entriesRef.current.some((e) => e.id === id);
        },
        onFire: (x, y) => {
          const id = pressEntryIdRef.current;
          const entry =
            id === null ? undefined : entriesRef.current.find((e) => e.id === id);
          if (!entry) return;
          if (navigator.vibrate) navigator.vibrate(15);
          openMsgMenu({ entry, media: null, x, y }, true);
        },
      });
    }
    return pressGuardRef.current;
  };
  const bubblePressHandlers = (entry: Entry) => {
    return {
      onTouchStart: (e: React.TouchEvent) => {
        if (!isMobile || e.touches.length !== 1) return;
        const { clientX, clientY } = e.touches[0];
        pressEntryIdRef.current = entry.id;
        getPressGuard().start(clientX, clientY);
      },
      onContextMenu: (e: React.MouseEvent) => {
        e.preventDefault();
        // 安卓：系统长按 ~500ms 补发的原生 contextmenu 不弹应用菜单——应用菜
        // 单由 450ms 计时器负责（先于 contextmenu 打开，抬指合成 click 由
        // msgMenuClickGuard 吞掉）；fromTouch=false 不布防 click 吞吐，从这里
        // 弹会被瞬间关掉再重开。守卫顺带 cancel：正常路径计时器已触发（无害），
        // 被压迟到 contextmenu 之后时避免迟到的菜单再弹出。安卓 + 鼠标右键
        // 随之牺牲（触摸长按是主路径）。桌面右键照常即时弹。
        if (isAndroidDevice()) {
          pressGuardRef.current?.cancel();
          return;
        }
        openMsgMenu({ entry, media: null, x: e.clientX, y: e.clientY }, false);
      },
    };
  };
  // 卸载清计时器与 window 监听。
  useEffect(() => () => pressGuardRef.current?.cancel(), []);

  // 系统返回键关一层菜单 / 选择模式（与弹层栈共用管道）。
  useBackClose(msgMenu !== null, () => setMsgMenu(null));
  useBackClose(textSelectId !== null, () => {
    setTextSelectId(null);
    window.getSelection?.()?.removeAllRanges();
  });
  useBackClose(selectIds !== null, exitSelection);
  useBackClose(gallery !== null, () => setGallery(null));
  useBackClose(attachMenuOpen, () => setAttachMenuOpen(false));
  useBackClose(permOpen, () => setPermOpen(false));

  const flashNotice = (text: string) => {
    setNotice(text);
    // 连续操作时重置计时，避免上一条的定时器提前熄灭本次提示。
    window.clearTimeout(noticeTimer.current);
    noticeTimer.current = window.setTimeout(() => setNotice(null), 2000);
  };

  /** 关闭菜单并执行动作（闭包里的 msgMenu 是本次渲染的载荷，先取后清）。 */
  const closeMenuAnd = (fn: (menu: NonNullable<typeof msgMenu>) => void) => () => {
    if (!msgMenu) return;
    const menu = msgMenu;
    setMsgMenu(null);
    fn(menu);
  };

  // 打开全屏媒体画廊：从全部消息解析（与 MediaPreviews 出卡同一口径），图片
  // 与应用内可播的视频按出现顺序去重入列，start 定位到点击的那条。只扫
  // 用户/助手消息——工具行、思考行不出媒体卡，塞进画廊会出现「滑到一张
  // 聊天里没见过的图」。
  const openGallery = (start: string, seed?: GallerySeed | null) => {
    const items: GalleryItem[] = [];
    const seen = new Set<string>();
    for (const e of entries) {
      if (e.role !== "user" && e.role !== "assistant") continue;
      for (const it of parseMediaItems(e.content)) {
        if (seen.has(it.path)) continue;
        const isVideo = it.kind === "open" && /\.(?:mp4|webm|m4v|mov)$/i.test(it.path);
        if (it.kind !== "image" && !isVideo) continue;
        seen.add(it.path);
        items.push({
          path: it.path,
          display: it.display || it.path,
          kind: isVideo ? "video" : "image",
        });
      }
    }
    if (!seen.has(start)) return;
    setGallery({ items, start, seed: seed ?? null });
  };

  // 菜单媒体动作：保存 / 转发 / 系统打开 / 放大（放大按需拉 base64，媒体卡
  // 已加载的 data URL 不在 Chat 手里）。
  const menuSaveMedia = (path: string) => {
    api
      .saveMedia(path)
      .then((r) => {
        if (r === "permission_requested") {
          onError?.("请在系统授权弹窗允许后重试保存");
        } else {
          flashNotice("已保存到本地");
        }
      })
      .catch((e) => onError?.(`保存失败: ${String(e)}`));
  };
  const menuShareMedia = (path: string) => {
    api.shareMedia(path).catch((e) => onError?.(`转发失败: ${String(e)}`));
  };
  const menuOpenMedia = (path: string) => {
    api.openMedia(path).catch((e) => onError?.(`打开媒体失败: ${String(e)}`));
  };
  const menuViewImage = (path: string) => {
    void api
      .mediaPreview(path)
      .then((r) =>
        openGallery(path, { path, src: `data:${r.mime};base64,${r.base64}` }),
      )
      .catch((e) => onError?.(`读取图片失败: ${String(e)}`));
  };

  const selectedMessages = useMemo(
    () => (selectIds ? entries.filter((e) => selectIds.has(e.id)) : []),
    [entries, selectIds],
  );

  /** 打包选中的消息为可读文本（转发到会话 / 分享文本用）。 */
  const buildPackageText = () =>
    selectedMessages
      .map((e) => {
        const who =
          e.role === "user"
            ? "用户"
            : e.role === "assistant"
              ? "助手"
              : e.role === "thinking"
                ? "思考"
                : "工具";
        const text = stripMediaMarkers(e.content).trim() || "（媒体消息）";
        return `【${who}】\n${text}`;
      })
      .join("\n\n");

  const exportTitle = useMemo(() => {
    const first = selectedMessages[0];
    if (!first) return "会话导出";
    const text = stripMediaMarkers(first.content).trim();
    return (text.split("\n")[0] || "会话导出").slice(0, 20) || "会话导出";
  }, [selectedMessages]);

  const doExport = (fmt: "md" | "docx") => {
    if (selectedMessages.length === 0) return;
    setExporting(true);
    api
      .exportMessages(
        exportTitle,
        fmt,
        selectedMessages.map((e) => ({ role: e.role, content: e.content })),
      )
      .then((rel) => setExportResult(rel))
      .catch((e) => onError?.(`导出失败: ${String(e)}`))
      .finally(() => setExporting(false));
  };

  const doShareText = () => {
    let text = buildPackageText();
    if (text.length > 6000) text = `${text.slice(0, 6000)}\n…（已截断）`;
    api.shareText(exportTitle, text).catch((e) => onError?.(`分享失败: ${String(e)}`));
  };

  // 消息列表元素缓存：entries / 选择模式不变时，打开菜单、图片查看器、反馈
  // 气泡等局部状态变化不再整表重渲——每条助手消息的 markdown 都会重新解析，
  // 真机上可感知（曾表现为点图片开全屏查看器要顿 1~2s）。缓存的元素身份
  // 不变时 React 直接跳过对应子树。行内闭包只触达 setState 与 ref，沿用
  // 旧闭包无碍。
  // 输出为「展示段」块（zcode「已工作 X」同款）：每段 = user 条目 + 工作过程
  // （折叠容器内）+ 最终回复（容器外）。开合状态（openSegs）在外层读取，块内
  // 元素身份不随开合变化——折叠只重排壳，不重建消息子树。
  const listBlocks = useMemo(
    () => {
      const renderEntry = (e: Entry) => {
        if (e.role === "status") {
          const selected = selectIds?.has(e.id) ?? false;
          if (e.tools && e.tools.length > 0) {
            // 工具 / Skill 调用批次：逐调用一行，默认折叠、点箭头展开详情（直播与
            // 库回放同一组件；条目有行 id 时展开按行 id 懒加载全文）。
            return (
              <div
                key={e.id}
                className={selectIds ? "flex items-start" : "flex justify-start"}
                {...(selectIds
                  ? { onClick: () => toggleSelected(e.id) }
                  : bubblePressHandlers(e))}
              >
                {selectIds && (
                  <button
                    onClick={(ev) => {
                      ev.stopPropagation();
                      toggleSelected(e.id);
                    }}
                    title={selected ? "取消选择" : "选择"}
                    className={`mr-1.5 mt-1 flex h-5 w-5 shrink-0 items-center justify-center self-start rounded-full border text-[11px] transition ${
                      selected
                        ? "border-indigo-400 bg-indigo-600 text-white"
                        : "border-gray-500 text-transparent"
                    }`}
                  >
                    ✓
                  </button>
                )}
                <div className={selectIds ? "min-w-0 flex-1" : "min-w-0 w-full max-w-[92%]"}>
                  <ToolBatchRows
                    chatId={sessionKey}
                    entryId={e.id}
                    msgId={e.msgId}
                    calls={e.tools}
                  />
                </div>
              </div>
            );
          }
          return (
            <div
              key={e.id}
              className={`flex justify-start ${selectIds ? "cursor-pointer items-center" : ""}`}
              {...(selectIds
                ? { onClick: () => toggleSelected(e.id) }
                : bubblePressHandlers(e))}
            >
              {selectIds && (
                <button
                  onClick={(ev) => {
                    ev.stopPropagation();
                    toggleSelected(e.id);
                  }}
                  title={selected ? "取消选择" : "选择"}
                  className={`mr-1.5 flex h-5 w-5 shrink-0 items-center justify-center rounded-full border text-[11px] transition ${
                    selected
                      ? "border-indigo-400 bg-indigo-600 text-white"
                      : "border-gray-500 text-transparent"
                  }`}
                >
                  ✓
                </button>
              )}
              <span className="rounded-full bg-[#1a1f29] px-3 py-1 text-xs text-gray-500">
                {e.content}
              </span>
            </div>
          );
        }
        if (e.role === "thinking") {
          const durationSecs = durationById.get(e.id) ?? null;
          const selected = selectIds?.has(e.id) ?? false;
          return (
            <div
              key={e.id}
              className={selectIds ? "flex cursor-pointer items-start" : undefined}
              {...(selectIds
                ? { onClick: () => toggleSelected(e.id) }
                : textSelectId === e.id
                  ? undefined
                  : bubblePressHandlers(e))}
            >
              {selectIds && (
                <button
                  onClick={(ev) => {
                    ev.stopPropagation();
                    toggleSelected(e.id);
                  }}
                  title={selected ? "取消选择" : "选择"}
                  className={`mr-1.5 mt-1 flex h-5 w-5 shrink-0 items-center justify-center self-start rounded-full border text-[11px] transition ${
                    selected
                      ? "border-indigo-400 bg-indigo-600 text-white"
                      : "border-gray-500 text-transparent"
                  }`}
                >
                  ✓
                </button>
              )}
              <div className={selectIds ? "min-w-0 flex-1" : undefined}>
                <ThinkingBlock
                  key={`${e.id}:${thinkingGen}`}
                  content={e.content}
                  durationSecs={durationSecs}
                  defaultOpen={!futureThinkingCollapsed}
                  onToggle={onToggleThinking}
                  selectable={textSelectId === e.id}
                />
              </div>
            </div>
          );
        }
        if (e.role === "user") {
          const selected = selectIds?.has(e.id) ?? false;
          return (
            <div
              key={e.id}
              role="group"
              aria-label="用户消息"
              // 轮次导航锚点：RoundNav 按 data-entry-id 定位与判当前轮。
              data-entry-id={e.id}
              className="flex justify-end"
              {...(selectIds || textSelectId === e.id
                ? undefined
                : bubblePressHandlers(e))}
            >
              {selectIds && (
                <button
                  onClick={() => toggleSelected(e.id)}
                  title={selected ? "取消选择" : "选择"}
                  className={`mr-1.5 flex h-5 w-5 shrink-0 items-center justify-center self-center rounded-full border text-[11px] transition ${
                    selected
                      ? "border-indigo-400 bg-indigo-600 text-white"
                      : "border-gray-500 text-transparent"
                  }`}
                >
                  ✓
                </button>
              )}
              <div
                className={`flex flex-col items-end ${
                  selectIds
                    ? "max-w-[70%] cursor-pointer max-md:max-w-[78%]"
                    : // zcode 同款：窄屏（手机）用户消息可占满全宽，宽屏封顶 36rem。
                      "max-w-full md:max-w-xl"
                }`}
                {...(selectIds ? { onClick: () => toggleSelected(e.id) } : undefined)}
              >
                {/* zcode 同款角色提示（MessageContent is-user：rounded-lg + 次级底色 +
                    内边距，贴右收缩到内容宽）：无气泡全宽布局下用户消息一换行就与
                    助手消息分不开（同色同号、右对齐在满宽时消失），底色随换行块
                    延伸始终可见；浅色主题由 index.css 对 bg-[#1a1f29] 的定向覆盖接管。 */}
                <div
                  className={`whitespace-pre-wrap rounded-lg bg-[#1a1f29] px-4 py-2 text-sm text-gray-200 [overflow-wrap:anywhere] max-md:px-3.5 max-md:text-[15px] max-md:leading-relaxed ${
                    selectIds && selected ? "ring-2 ring-indigo-300" : ""
                  } ${textSelectId === e.id ? "text-selectable" : ""}`}
                >
                  {e.content}
                </div>
                <MediaPreviews
                  content={e.content}
                  onError={onError}
                  selectMode={!!selectIds}
                  onMediaMenu={(media, x, y, fromTouch) =>
                    openMsgMenu({ entry: e, media, x, y }, fromTouch)
                  }
                  onViewImage={({ src, path, card }) =>
                    openGallery(path, { path, src, preview: card ?? null })
                  }
                  onOpenVideo={({ url, path }) => openGallery(path, { path, url })}
                />
                <TimeTag ts={e.ts} />
                {!selectIds && (
                  <div className="flex items-center gap-1 max-md:hidden">
                    <CopyButton text={e.content} onError={onError} />
                    <QuoteButton
                      onQuote={() => setQuote({ who: "用户", text: buildQuoteText(e.content) })}
                    />
                    {onFork && <ForkButton onFork={() => onFork(e)} />}
                  </div>
                )}
              </div>
            </div>
          );
        }
        const selected = selectIds?.has(e.id) ?? false;
        return (
          <div
            key={e.id}
            role="group"
            aria-label="助手消息"
            className="flex justify-start"
            {...(selectIds || textSelectId === e.id
              ? undefined
              : bubblePressHandlers(e))}
          >
            {selectIds && (
              <button
                onClick={() => toggleSelected(e.id)}
                title={selected ? "取消选择" : "选择"}
                className={`mr-1.5 flex h-5 w-5 shrink-0 items-center justify-center self-center rounded-full border text-[11px] transition ${
                  selected
                    ? "border-indigo-400 bg-indigo-600 text-white"
                    : "border-gray-500 text-transparent"
                }`}
              >
                ✓
              </button>
            )}
            <div
              className={`flex flex-col items-start ${
                selectIds ? "max-w-[80%] cursor-pointer" : "w-full"
              }`}
              {...(selectIds ? { onClick: () => toggleSelected(e.id) } : undefined)}
            >
              <AssistantMarkdown
                content={e.content}
                selected={!!(selectIds && selected)}
                selectable={textSelectId === e.id}
              />
              <MediaPreviews
                content={e.content}
                onError={onError}
                selectMode={!!selectIds}
                onMediaMenu={(media, x, y, fromTouch) =>
                  openMsgMenu({ entry: e, media, x, y }, fromTouch)
                }
                onViewImage={({ src, path, card }) =>
                  openGallery(path, { path, src, preview: card ?? null })
                }
                onOpenVideo={({ url, path }) => openGallery(path, { path, url })}
              />
              <TimeTag ts={e.ts} />
              {!selectIds && (
                <div className="flex items-center gap-1 max-md:hidden">
                  <CopyButton text={stripMediaMarkers(e.content)} onError={onError} />
                  <QuoteButton
                    onQuote={() => setQuote({ who: "助手", text: buildQuoteText(e.content) })}
                  />
                  {onFork && <ForkButton onFork={() => onFork(e)} />}
                </div>
              )}
            </div>
          </div>
        );
      };

      // 思考块的持续时长（下一条目到达与本条到达的时间差，历史恢复无时间戳则不
      // 显示）：与原 entries[i+1] 平铺口径完全一致，只是预计算成表供 renderEntry 查。
      const durationById = new Map<number, number | null>();
      entries.forEach((e, i) => {
        if (e.role !== "thinking") return;
        const nxt = i + 1 < entries.length ? entries[i + 1] : undefined;
        const elapsed = e.ts && nxt?.ts ? Math.round((nxt.ts - e.ts) / 1000) : null;
        durationById.set(e.id, elapsed !== null && elapsed >= 1 ? elapsed : null);
      });

      // 展示段分段：user 条目开启一段（展示段 ≠ 后端 turn：「立即」投递并入运行中
      // 的 turn、排队出队开启新段）。快照只裁最近 80 条时可能没有 user 开头——该
      // 前缀段整段平铺（不做折叠壳）；发送失败段同样平铺（错误提示必须显眼）。
      const segs: Array<{ key: number; user: Entry | null; rows: Entry[] }> = [];
      let cur: { key: number; user: Entry | null; rows: Entry[] } | null = null;
      for (const e of entries) {
        if (e.role === "user") {
          cur = { key: e.id, user: e, rows: [] };
          segs.push(cur);
          continue;
        }
        if (!cur) {
          cur = { key: e.id, user: null, rows: [] };
          segs.push(cur);
        }
        cur.rows.push(e);
      }

      return segs.map((seg, si) => {
        const running = busy && si === segs.length - 1;
        const hasAssistant = seg.rows.some((r) => r.role === "assistant");
        const hasProcess = seg.rows.some(
          (r) => r.role === "status" || r.role === "thinking",
        );
        // 最终回复 = 完成段里最后一个 assistant 条目（启发式：Entry 不携带
        // intermediate 标记）。运行段不分离最终正文——运行中的最后 assistant
        // 仍可能是中间说明；无 assistant 的段（被取消/中断的轮）整体留在过程
        // 容器并强制展开，上下文不丢。
        let finalIdx = -1;
        if (!running && hasAssistant) {
          for (let ri = seg.rows.length - 1; ri >= 0; ri--) {
            if (seg.rows[ri].role === "assistant") {
              finalIdx = ri;
              break;
            }
          }
        }
        const processRows = finalIdx >= 0 ? seg.rows.slice(0, finalIdx) : seg.rows;
        const finalRow = finalIdx >= 0 ? seg.rows[finalIdx] : null;
        const followingRows = finalIdx >= 0 ? seg.rows.slice(finalIdx + 1) : [];
        // 过程行全在最终回复之后（中间说明先到、随后工具调用被取消）的段没有可折叠
        // 的「工作过程」——平铺，不渲染空的过程容器。
        const flat =
          seg.user === null ||
          !!seg.user?.sendFailed ||
          !hasProcess ||
          processRows.length === 0;
        return {
          key: seg.key,
          userEl: seg.user ? renderEntry(seg.user) : null,
          flat,
          flatRowsEls: flat ? seg.rows.map(renderEntry) : [],
          processEls: flat ? [] : processRows.map(renderEntry),
          finalEl: flat || !finalRow ? null : renderEntry(finalRow),
          followingEls: flat ? [] : followingRows.map(renderEntry),
          running,
          collapsible: !running && hasAssistant,
          startMs: processRows[0]?.ts,
          // 跨度终点 = 最终回复条目（无则最后一条过程行）；段尾 following 行不计入
          // 工作时长。
          endMs: (finalRow ?? processRows[processRows.length - 1])?.ts,
        };
      });
    },
    // 行内闭包只触达 setState / ref（openMsgMenu、toggleSelected 等），不列入
    // 依赖是刻意的；onError 需要稳定身份（App 侧已 useCallback）；onFork 随会话
    // 变化（依赖 activeId），必须列入——否则分叉闭包捕获旧会话 id，切会话后
    // 点「分叉」会 fork 到上一个会话。thinkingGen 参与 ThinkingBlock 的 key，
    // 变化时必须重算否则一键开合不会重挂载。busy 参与运行段判定（最终回复
    // 是否分离）。sessionKey 是工具行懒加载详情的会话键。
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [entries, selectIds, isMobile, futureThinkingCollapsed, onError, onFork, textSelectId, thinkingGen, busy, sessionKey],
  );

  // 运行段集合仅在提交阶段（useLayoutEffect）写入，渲染期只读上次提交的集合——
  // 渲染函数保持可重复（StrictMode 双调用 / 并发渲染被丢弃都不会污染 ref）。
  // busy→false 的完成渲染据此推导 open=true（上次提交还在运行），过程不卸载；
  // 同一提交阶段把决定固化进 openSegs，后续渲染从 state 读、可手动收起。
  // sessionKey 入依赖：会话边界 layout effect（上方）先清、本 effect 后按新会话
  // 重录运行集合（同阶段按声明序执行）。
  useLayoutEffect(() => {
    runningKeysRef.current = new Set(
      listBlocks.filter((b) => b.running).map((b) => b.key),
    );
    const finished = [...prevRunningSegKeysRef.current].filter(
      (k) => !runningKeysRef.current.has(k),
    );
    prevRunningSegKeysRef.current = runningKeysRef.current;
    if (finished.length === 0) return;
    setOpenSegs((prevOpen) => {
      const patch = { ...prevOpen };
      let changed = false;
      for (const k of finished) {
        if (patch[k] === undefined) {
          patch[k] = true;
          changed = true;
        }
      }
      return changed ? patch : prevOpen;
    });
  }, [listBlocks, sessionKey]);

  // 文字选择模式选中的思考块所在段：同样强制展开（历史过程段收起时思考块正文
  // 不可见，选择模式就是空选）。key 口径与下方分段一致（user 条目开段；无 user
  // 前缀段 = 段首条目 id）。
  const textSelectSegKey = useMemo(() => {
    if (textSelectId === null) return null;
    let curKey: number | null = null;
    for (const e of entries) {
      if (e.role === "user" || curKey === null) curKey = e.id;
      if (e.id === textSelectId) return curKey;
    }
    return null;
  }, [entries, textSelectId]);

  // 段开合状态在渲染期推导（只读上次提交的运行集合，本渲染不产生副作用——可
  // 被 StrictMode / 并发渲染安全地重复执行）。
  const segOpen = new Map<number, boolean>();
  const segForced = new Map<number, boolean>();
  for (const b of listBlocks) {
    // 选择模式（selectIds 非 null 即处于选择模式，空集也是）强制全部展开，保证
    // 每一行都可达；运行段/无 assistant 段强制展开且不可收起。
    const forcedOpen =
      selectIds !== null || b.running || !b.collapsible || b.key === textSelectSegKey;
    segForced.set(b.key, forcedOpen);
    segOpen.set(
      b.key,
      forcedOpen || (openSegs[b.key] ?? runningKeysRef.current.has(b.key)),
    );
  }

  // ── 轮次快捷导航（桌面 rail / 移动 chip+底部 sheet，组件在 RoundNav.tsx）──
  // 轮次 = 用户消息（发送失败的不算：没落库、带错误提示，导航意义小）。后端
  // 只保留最近 ~80 条目：编号按「已加载序号」，不是全会话轮次（面板有说明）。
  const rounds = useMemo<RoundNavItem[]>(
    () =>
      entries
        .filter((e) => e.role === "user" && !e.sendFailed)
        .map((e) => ({
          id: e.id,
          preview:
            stripMediaMarkers(e.content).split("\n")[0]?.trim().slice(0, 80) ||
            "（媒体消息）",
          time: e.ts && e.ts > 0 ? formatEntryTime(new Date(e.ts)) : null,
        })),
    [entries],
  );
  const widePointer = useWidePointer();
  // 选择模式 / 文字选择期间隐藏导航（选择手势与导航互抢）。
  const navEnabled = rounds.length >= 2 && selectIds === null && textSelectId === null;
  const railVisible = navEnabled && !isAndroidDevice() && widePointer;
  const fabVisible = navEnabled && !railVisible;
  // 轮次面板开合（安卓/窄窗形态）：触发钮在输入区工具条、面板在消息列表
  // wrapper，分居两个子树，状态只能放这里。fab 形态退场（切选择模式/切桌面
  // 宽屏/rounds<2）时必须复位，否则回到该形态时面板自动重开。
  const [navOpen, setNavOpen] = useState(false);
  // 面板关闭（遮罩/选行/返回键）后焦点返还触发钮，读屏用户不落回 body。
  const navBtnRef = useRef<HTMLButtonElement | null>(null);
  const closeNav = () => {
    setNavOpen(false);
    navBtnRef.current?.focus();
  };
  useEffect(() => {
    if (!fabVisible) setNavOpen(false);
  }, [fabVisible]);

  // 跳转到第 N 轮：锚点行顶对齐列表视口顶。最后一轮 = 最新对话，恢复跟随拉底
  // （跳到最新还显示「回到底部」是反直觉的）；其余轮次脱离跟随。zcode 同款坐
  // 标公式；prefers-reduced-motion 时全部 instant。
  const scrollToRound = (id: number) => {
    const el = listRef.current;
    if (!el) return;
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    if (rounds[rounds.length - 1]?.id === id) {
      scrollListToBottom(!reduce);
      return;
    }
    const row = el.querySelector<HTMLElement>(`[data-entry-id="${id}"]`);
    if (!row) return;
    stickToBottomRef.current = false;
    setShowJumpDown(true);
    el.scrollTo({
      top:
        el.scrollTop +
        row.getBoundingClientRect().top -
        el.getBoundingClientRect().top,
      behavior: reduce ? "auto" : "smooth",
    });
  };

  // 当前轮 = 视口顶以上最近的一条用户消息（zcode 同语义）；一条都不到线（视口
  // 停在第一条消息内部）才取第一条——底部不管多长的回复都不回退第一条。rAF
  // 节流：scroll 每 tick 至多一次，DOM 查询量 = 轮次数（≤80）。
  const [activeRoundId, setActiveRoundId] = useState<number | null>(null);
  const activeRoundRafRef = useRef<number | null>(null);
  // 最新实现经 ref 供 rAF 回调读取（见 scheduleActiveRound）；layout effect 写入
  // 而非渲染期——并发渲染丢弃的渲染不得更新 ref。
  const computeActiveRoundRef = useRef<() => void>(() => {});
  const computeActiveRound = () => {
    const el = listRef.current;
    if (!el || rounds.length === 0) {
      setActiveRoundId((v) => (v === null ? v : null));
      return;
    }
    const containerTop = el.getBoundingClientRect().top;
    let current: number | null = null;
    for (const r of rounds) {
      const row = el.querySelector<HTMLElement>(`[data-entry-id="${r.id}"]`);
      if (!row) continue;
      if (row.getBoundingClientRect().top <= containerTop + 8) current = r.id;
      else break;
    }
    const next = current ?? rounds[0].id;
    setActiveRoundId((v) => (v === next ? v : next));
  };
  const scheduleActiveRound = () => {
    // 经 ref 取最新 computeActiveRound：rAF 闭包若捕获调度帧的旧 rounds，会在
    // 流式 entries 更新后把高亮改回旧轮（layout effect 先按新值算、旧 rAF 后跑
    // 覆盖回去）。
    if (activeRoundRafRef.current !== null) return;
    activeRoundRafRef.current = window.requestAnimationFrame(() => {
      activeRoundRafRef.current = null;
      computeActiveRoundRef.current();
    });
  };
  // computeActiveRoundRef 每次 commit 后刷新（无依赖 layout effect：layout 同步
  // 于提交、先于 rAF 回调，rAF 永远读到刚提交的实现；并发渲染丢弃的渲染不会
  // 更新 ref，签名不变但 rounds 引用换新的边界场景也覆盖）。
  useLayoutEffect(() => {
    computeActiveRoundRef.current = computeActiveRound;
  });
  // 补算时机：滚动之外没有 scroll 事件的场景——切会话/首屏/发送/轮次集合变化/
  // 忙碌翻转。依赖收窄到轮次集合签名（长度+首尾 id）：流式每 chunk 变 entries
  // 不再逐 chunk 全量查 DOM（滚动路径的 rAF 兜住其余时机）。
  const roundsSignature = `${rounds.length}:${rounds[0]?.id ?? ""}:${rounds[rounds.length - 1]?.id ?? ""}`;
  useLayoutEffect(() => {
    computeActiveRound();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [roundsSignature, sessionKey, busy]);
  // 卸载取消挂着的 rAF。
  useEffect(
    () => () => {
      if (activeRoundRafRef.current !== null) {
        cancelAnimationFrame(activeRoundRafRef.current);
      }
    },
    [],
  );

  return (
    <div className="chat-bg flex h-full flex-col">
      {/* 消息流 wrapper：rail / fab 的定位基准必须是消息列表视口（rail 的
          top-1/2 若挂在 chat-bg 上会居中到整个聊天列、压到输入区）。 */}
      <div className="relative flex min-h-0 flex-1 flex-col">
        {railVisible && (
          <RoundRail rounds={rounds} activeId={activeRoundId} onJump={scrollToRound} />
        )}
        {/* 消息流（移动端收窄页边距，把宽度还给消息正文）；rail 可见时左侧
            让位 56px（rail 宽 40px + 边距），不压住正文。 */}
        <div
          ref={listRef}
          onScroll={handleListScroll}
          onTouchStartCapture={handleListTouchStart}
          onTouchMoveCapture={handleListTouchMove}
          onTouchEndCapture={handleListTouchFinish}
          onTouchCancelCapture={handleListTouchFinish}
          onWheel={handleListWheel}
          className={`chat-msg-list flex-1 space-y-3 overflow-y-auto py-4 max-md:space-y-2.5 max-md:px-3 max-md:py-3 ${
            railVisible ? "pl-14 pr-6" : "px-6"
          }`}
        >
        {entries.length === 0 && (
          <div className="mt-24 text-center text-sm text-gray-600 max-md:mt-36 max-md:px-6">
            <p className="text-base text-gray-400">向 RClaw 发出第一条消息</p>
            <p className="mt-2">
              试试「帮我每天早上 8 点汇总一次待办」或输入 <code className="text-gray-500">/help</code>
            </p>
          </div>
        )}
        {listBlocks.map((b) => {
          const forcedOpen = segForced.get(b.key) ?? false;
          const open = segOpen.get(b.key) ?? false;
          if (b.flat) {
            return (
              <Fragment key={b.key}>
                {b.userEl}
                {b.flatRowsEls}
              </Fragment>
            );
          }
          return (
            <Fragment key={b.key}>
              {b.userEl}
              <ProcessGroupShell
                header={
                  <WorkSegmentBar
                    running={b.running}
                    startMs={b.startMs}
                    endMs={b.endMs}
                    open={open}
                    collapsible={!forcedOpen}
                    onToggle={() => toggleSegment(b.key)}
                  />
                }
                open={open}
              >
                {b.processEls}
              </ProcessGroupShell>
              {b.finalEl}
              {b.followingEls}
            </Fragment>
          );
        })}
        {/* turn 进行中：消息流底部转圈齿轮，表示 agent 正在执行 */}
        {busy && (
          <div className="flex justify-start">
            <div className="flex items-center gap-2 rounded-full bg-[#1a1f29] px-3 py-1.5 text-xs text-gray-400">
              <span className="shrink-0 animate-spin text-indigo-400">
                <GearIcon />
              </span>
              正在执行…
            </div>
          </div>
        )}
        {/* 向上翻阅历史时的「回到底部」钮：sticky 钉在滚动口底部，点击恢复跟随。
            data-jump-down：列表触摸结束重判按它让位，见 handleListTouchFinish。 */}
        {showJumpDown && (
          <div className="sticky bottom-2 z-20 flex justify-center" data-jump-down>
            <button
              onClick={() => scrollListToBottom(true)}
              title="回到底部"
              aria-label="回到底部"
              className="flex h-9 w-9 items-center justify-center rounded-full border border-[#2a2f3a] bg-[#1a2030]/90 text-gray-300 shadow-lg backdrop-blur transition hover:border-indigo-500/50 hover:text-indigo-300"
            >
              <svg
                width="16"
                height="16"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <path d="M12 5v14" />
                <path d="m19 12-7 7-7-7" />
              </svg>
            </button>
          </div>
        )}
        </div>
        {/* 移动端 / 窄窗：轮次导航底部面板（入口在输入区工具条；rail 是桌面
            宽屏形态，两者互斥）。 */}
        {fabVisible && (
          <RoundNavSheet
            open={navOpen}
            onClose={closeNav}
            rounds={rounds}
            activeId={activeRoundId}
            onJump={scrollToRound}
          />
        )}
      </div>

      {/* 输入区（移动端贴底并让出手势条/安全区；桌面 p-4 不变）。 */}
      <div className="border-t border-[#2a2f3a] p-4 max-md:px-2.5 max-md:pt-2 max-md:pb-[calc(env(safe-area-inset-bottom)+0.5rem)]">

        {/* 工作流运行卡片（status/进度/问题/hole/副作用裁决）。 */}
        <WorkflowCard onError={onError} />

        {/* PlanRun 流程步骤卡片：全部【未收尾】计划各一张（随 plan_status 轮询刷新）
            ——只展示最新的会把被顶到后面的暂停计划藏没：暂停中的执行器在等恢复
            标志，Resume 按钮必须任何时候都够得着（暂停中的计划【全部渲染】，不受
            卡片上限约束）；没有未收尾计划时保留最新已收尾计划当摘要卡。非暂停的
            未收尾计划按 updated_at 序最多补 4 张。整块限高自滚动（页面 overflow-hidden
            且只有历史区滚动，多计划展开会把输入框 / 停止按钮顶出可视区）。计划无
            会话归属（journal 全局），在哪个会话都能操作。 */}
        {plans.length > 0 && (
          <div className="mb-2 max-h-60 space-y-1.5 overflow-y-auto">
            {(() => {
              const visible = plans.filter((p) => !p.completed && p.steps.length > 0);
              if (visible.length === 0 && plans[0]?.steps.length) {
                visible.push(plans[0]);
              }
              const pausedPlans = visible.filter((p) => p.paused);
              const others = visible
                .filter((p) => !p.paused)
                .slice(0, Math.max(0, 4 - pausedPlans.length));
              return [...pausedPlans, ...others].map((p) => (
                <PlanStepsCard key={p.plan_id} plan={p} onPlanPause={onPlanPause} />
              ));
            })()}
          </div>
        )}

        {/* 排队消息（zcode 同款，第十二轮）：agent 运行期间发送的消息在此排队，
            可删除 / 编辑 / 立即投递；会话收尾后由 App 逐条自动发出（App 级状态，
            页面切换不丢）。props.queue 已是当前会话的队列。限高自滚动：页面本身
            overflow-hidden 且只有历史区滚动，队列无界会把输入框 / 停止按钮顶出屏幕。 */}
        {queue.length > 0 && (
          <div className="mb-2 max-h-40 space-y-1.5 overflow-y-auto">
            {queue.map((item) => (
              <div
                key={item.id}
                className="flex items-center gap-1.5 rounded-xl border border-[#2a2f3a] bg-[#141821] px-3 py-1.5"
              >
                <span className="shrink-0 text-[10px] text-amber-400/80">⏳ 排队</span>
                <span className="min-w-0 flex-1 truncate text-xs text-gray-300" title={item.text}>
                  {item.text}
                </span>
                <button
                  onClick={() => sendQueuedNow(item)}
                  title="立即发送（作为插队消息并入当前任务）"
                  className="shrink-0 rounded px-1.5 py-0.5 text-[11px] text-indigo-300 transition hover:bg-indigo-500/15"
                >
                  立即
                </button>
                <button
                  onClick={() => editQueued(item)}
                  title="编辑（回填输入框）"
                  aria-label="编辑排队消息"
                  className="shrink-0 rounded px-1 py-0.5 text-gray-500 transition hover:text-gray-200"
                >
                  <PencilIcon />
                </button>
                <button
                  onClick={() => onQueueRemove(item.id)}
                  title="删除"
                  aria-label="删除排队消息"
                  className="shrink-0 rounded px-1.5 py-0.5 text-gray-500 transition hover:text-red-300"
                >
                  ✕
                </button>
              </div>
            ))}
          </div>
        )}

        {/* 选择模式操作栏：长按消息进入；打包发送到其它会话 / 导出文档 /
            分享文本（安卓）。 */}
        {selectIds && (
          <div className="mb-2 rounded-xl border border-[#2a2f3a] bg-[#141821] px-3 py-2">
            <div className="flex items-center gap-2 text-xs text-gray-300">
              <span>
                已选 {selectedCount} 条
                {exporting && (
                  <span className="ml-2 inline-block h-2.5 w-2.5 animate-spin rounded-full border-[1.5px] border-indigo-400 border-t-transparent align-[-2px]" />
                )}
              </span>
              <button
                onClick={() => {
                  const all = new Set(selectableEntries.map((entry) => entry.id));
                  setSelectIds((prev) =>
                    prev && prev.size === all.size && all.size > 0 ? new Set() : all,
                  );
                }}
                className="rounded px-1.5 py-0.5 text-[11px] text-gray-400 transition hover:text-gray-200"
              >
                {selectedCount === selectableEntries.length && selectableEntries.length > 0
                  ? "清除全选"
                  : "全选"}
              </button>
              <button
                onClick={exitSelection}
                className="ml-auto rounded px-1.5 py-0.5 text-[11px] text-gray-400 transition hover:text-gray-200"
              >
                退出选择
              </button>
            </div>
            <div className="mt-2 flex flex-wrap gap-2">
              <button
                onClick={() => {
                  const text = buildPackageText();
                  exitSelection();
                  if (text) onForwardToChat?.(text);
                }}
                disabled={selectedCount === 0}
                className={ACTION_BTN}
              >
                发送到会话
              </button>
              <button
                onClick={() => doExport("md")}
                disabled={selectedCount === 0 || exporting}
                className={ACTION_BTN}
              >
                导出文本文档(.md)
              </button>
              <button
                onClick={() => doExport("docx")}
                disabled={selectedCount === 0 || exporting}
                className={ACTION_BTN}
              >
                导出 Word(.docx 含图片)
              </button>
              {isAndroidDevice() && (
                <button
                  onClick={doShareText}
                  disabled={selectedCount === 0}
                  className={ACTION_BTN}
                >
                  分享文本到微信/QQ
                </button>
              )}
            </div>
          </div>
        )}

        {/* 导出结果条：产物在工作区 output/exports/，可保存到本地或转发。 */}
        {exportResult && (
          <div className="mb-2 rounded-xl border border-emerald-500/40 bg-emerald-500/10 px-3 py-2 text-xs text-gray-300">
            <div className="break-all font-mono text-[11px] text-emerald-300">
              已导出：{exportResult}
            </div>
            {exportNotice && <div className="mt-1 text-emerald-300">{exportNotice}</div>}
            <div className="mt-1.5 flex flex-wrap gap-2">
              <button
                onClick={() => {
                  setExportNotice(null);
                  api
                    .saveMedia(exportResult)
                    .then((r) => {
                      if (r === "permission_requested") {
                        onError?.("请在系统授权弹窗允许后重试保存");
                      } else {
                        setExportNotice("✓ 已保存到本地");
                      }
                    })
                    .catch((err) => onError?.(`保存失败: ${String(err)}`));
                }}
                className={ACTION_BTN}
              >
                保存到本地
              </button>
              {isAndroidDevice() && (
                <button
                  onClick={() => {
                    api.shareMedia(exportResult).catch((err) =>
                      onError?.(`转发失败: ${String(err)}`),
                    );
                  }}
                  className={ACTION_BTN}
                >
                  转发到其它应用
                </button>
              )}
              <button
                onClick={() => {
                  setExportResult(null);
                  setExportNotice(null);
                }}
                className={ACTION_BTN}
              >
                关闭
              </button>
            </div>
          </div>
        )}

        {/* 引用条（微信式缩略）：文本消息显首行；纯媒体消息显类型图标 + 文件名。
            全文见 title，随下一条消息发送或取消。 */}
        {quote && (
          <div className="mb-2 flex items-center gap-2 rounded-xl border border-indigo-500/40 bg-indigo-500/10 px-3 py-1.5">
            <span className="shrink-0 text-[11px] font-medium text-indigo-300">
              ❝ 引用{quote.who}
            </span>
            <span
              className="min-w-0 flex-1 truncate text-[11px] text-gray-400"
              title={quote.text}
            >
              {(() => {
                const first = quote.text.split("\n")[0] || "";
                const m = /^\[引用媒体\] (.+)$/.exec(first);
                if (!m) return first || "（媒体）";
                const p = m[1];
                const icon = /\.(?:mp4|webm|m4v|mov)$/i.test(p)
                  ? "🎬"
                  : /\.(?:png|jpe?g|webp|gif)$/i.test(p)
                    ? "🖼"
                    : "📄";
                return `${icon} ${p.split("/").pop() || p}`;
              })()}
            </span>
            <button
              onClick={() => setQuote(null)}
              title="取消引用"
              aria-label="取消引用"
              className="shrink-0 rounded px-1.5 text-[11px] text-gray-500 transition hover:text-gray-200"
            >
              ✕
            </button>
          </div>
        )}

        {(attachments.length > 0 || uploading) && (
          <div className="mb-2 flex flex-wrap items-center gap-2">
            {uploading && (
              <span className="flex items-center gap-1.5 rounded-full bg-[#1a2030] px-2.5 py-1 text-xs text-gray-400">
                <span className="h-2.5 w-2.5 animate-spin rounded-full border-[1.5px] border-indigo-400 border-t-transparent" />
                上传中…
              </span>
            )}
            {attachments.map((a, i) => (
              <span
                key={`${a.rel}-${i}`}
                title={a.abs}
                className="flex items-center gap-1.5 rounded-full bg-[#1a2030] px-2.5 py-1 text-xs text-gray-300"
              >
                📄 <span className="max-w-48 truncate">{a.name}</span>
                <button
                  onClick={() =>
                    setAttachments((prev) => prev.filter((_, idx) => idx !== i))
                  }
                  title="移除附件"
                  className="text-gray-500 transition hover:text-gray-200"
                >
                  ×
                </button>
              </span>
            ))}
          </div>
        )}
        {/* 输入卡片：输入框独占整行；底排左侧为权限/附件工具钮，右侧为圆形
            发送/停止钮（同排右下角，主流 IM 布局，不再悬在卡片外）。 */}
        <div className="min-w-0 flex-1 rounded-2xl border border-[#2a2f3a] bg-[#161a22] transition focus-within:border-indigo-500/70">
          <textarea
            ref={inputRef}
            value={draft}
            onChange={(e) => {
              onDraftChange(e.target.value);
              // 手动编辑草稿即退出历史浏览，↑↓ 恢复文本框默认的光标移动语义。
              if (
                historyIndex !== null &&
                e.target.value !== (inputHistory[historyIndex] ?? "")
              ) {
                setHistoryIndex(null);
              }
            }}
            onKeyDown={onKeyDown}
            rows={Math.min(5, draft.split("\n").length)}
            placeholder={
              busy
                ? isMobile
                  ? "RClaw 工作中…输入可排队，点「⏳ 排队」入队"
                  : "RClaw 工作中…输入将排队（Enter 入队，可选「立即」插队）"
                : isMobile
                  ? "输入消息…"
                  : isAndroidDevice()
                    ? "输入消息…（Enter 换行，↑↓ 历史，/ 开头为命令）"
                    : "输入消息…（Enter 发送，↑↓ 历史，/ 开头为命令）"
            }
            className="block w-full resize-none bg-transparent px-4 pb-1 pt-3 text-sm text-gray-200 outline-none placeholder:text-gray-600 max-md:px-3.5 max-md:text-[15px]"
          />
          <div className="flex items-center gap-0.5 px-2 pb-2">
            {/* 权限模式选择器：默认权限（工作区外需确认）/ 完全访问（免确认） */}
            {permMode && (
              <div className="relative">
                <button
                  onClick={() => setPermOpen((v) => !v)}
                  title={permMode === "full" ? "工具权限：完全访问" : "工具权限：默认权限"}
                  aria-label="工具权限模式"
                  className={`flex h-10 items-center gap-1.5 rounded-lg px-2 transition ${
                    permMode === "full"
                      ? "bg-amber-500/10 text-amber-300 hover:bg-amber-500/20"
                      : "text-gray-500 hover:bg-[#1a2030] hover:text-indigo-300"
                  }`}
                >
                  <ShieldIcon />
                  <span className="text-[11px] max-md:hidden">
                    {permMode === "full" ? "完全访问" : "默认权限"}
                  </span>
                </button>
                {permOpen && (
                  <>
                    <div
                      className="fixed inset-0 z-30"
                      onClick={() => setPermOpen(false)}
                    />
                    <div className="absolute bottom-full left-0 z-40 mb-2 w-60 rounded-lg border border-[#2a2f3a] bg-[#141821] p-1 shadow-xl">
                      <button
                        onClick={() => void changePermMode("default")}
                        className={`block w-full rounded-md px-3 py-2 text-left transition hover:bg-[#1a2030] ${
                          permMode === "default" ? "text-indigo-300" : "text-gray-300"
                        }`}
                      >
                        <span className="block text-xs font-medium">默认权限</span>
                        <span className="mt-0.5 block text-[10px] text-gray-500">
                          工具仅限工作区内，之外需你确认授权
                        </span>
                      </button>
                      <button
                        onClick={() => void changePermMode("full")}
                        className={`block w-full rounded-md px-3 py-2 text-left transition hover:bg-[#1a2030] ${
                          permMode === "full" ? "text-amber-300" : "text-gray-300"
                        }`}
                      >
                        <span className="block text-xs font-medium">完全访问</span>
                        <span className="mt-0.5 block text-[10px] text-gray-500">
                          可读写电脑任意目录与文档，免确认
                        </span>
                      </button>
                    </div>
                  </>
                )}
              </div>
            )}
            {/* 附件按钮：安卓提供上传/拍照/拍视频/录音菜单；桌面保持纯文件上传。
                文件经后端落入 agent 工作区 uploads/，随消息以路径引用发给模型。 */}
            <div className="relative">
              {onCapture ? (
                <>
                  <button
                    onClick={() => setAttachMenuOpen((v) => !v)}
                    disabled={busy || uploading}
                    title="附件 / 拍照 / 录音"
                    aria-label="附件功能菜单"
                    className="flex h-10 w-10 items-center justify-center rounded-lg text-gray-500 transition hover:bg-[#1a2030] hover:text-indigo-300 disabled:opacity-40"
                  >
                    <svg
                      width="18"
                      height="18"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="2"
                      strokeLinecap="round"
                      strokeLinejoin="round"
                    >
                      <path d="m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l8.57-8.57A4 4 0 1 1 18 8.84l-8.59 8.57a2 2 0 0 1-2.83-2.83l8.49-8.48" />
                    </svg>
                  </button>
                  {attachMenuOpen && (
                    <>
                      <div
                        className="fixed inset-0 z-30"
                        onClick={() => setAttachMenuOpen(false)}
                      />
                      <div className="absolute bottom-full left-0 z-40 mb-2 w-44 rounded-xl border border-[#2a2f3a] bg-[#141821] p-1 shadow-xl">
                        <button
                          onClick={() => {
                            setAttachMenuOpen(false);
                            pickFiles();
                          }}
                          className="block w-full rounded-lg px-3 py-2.5 text-left text-sm text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
                        >
                          📎 上传文件
                        </button>
                        <button
                          onClick={() => {
                            setAttachMenuOpen(false);
                            onCapture("photo");
                          }}
                          className="block w-full rounded-lg px-3 py-2.5 text-left text-sm text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
                        >
                          📷 拍照
                        </button>
                        <button
                          onClick={() => {
                            setAttachMenuOpen(false);
                            onCapture("video");
                          }}
                          className="block w-full rounded-lg px-3 py-2.5 text-left text-sm text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
                        >
                          🎬 拍视频
                        </button>
                        <button
                          onClick={() => {
                            setAttachMenuOpen(false);
                            onCapture("audio");
                          }}
                          className="block w-full rounded-lg px-3 py-2.5 text-left text-sm text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
                        >
                          🎙️ 录音
                        </button>
                      </div>
                    </>
                  )}
                </>
              ) : (
                <button
                  onClick={pickFiles}
                  disabled={busy || uploading}
                  title="上传文件到 agent 工作区"
                  aria-label="上传文件"
                  className="flex h-10 w-10 items-center justify-center rounded-lg text-gray-500 transition hover:bg-[#1a2030] hover:text-indigo-300 disabled:opacity-40"
                >
                  <svg
                    width="18"
                    height="18"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="2"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                  >
                    <path d="m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l8.57-8.57A4 4 0 1 1 18 8.84l-8.59 8.57a2 2 0 0 1-2.83-2.83l8.49-8.48" />
                  </svg>
                </button>
              )}
            </div>
            {/* 思考块一键开合（附件按钮右边）：会话里有思考条目才出现；动作 =
                当前新块默认态取反（默认展开 → 全部折叠，默认折叠 → 全部展开），
                并把默认态一并翻转，之后新到的块跟随。 */}
            {hasThinkingEntries && (
              <button
                onClick={toggleAllThinking}
                title={futureThinkingCollapsed ? "展开全部思考过程" : "折叠全部思考过程"}
                aria-label={futureThinkingCollapsed ? "展开全部思考过程" : "折叠全部思考过程"}
                className="flex h-10 w-10 items-center justify-center rounded-lg text-gray-500 transition hover:bg-[#1a2030] hover:text-indigo-300"
              >
                <svg
                  width="18"
                  height="18"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="2"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                >
                  <path d="m7 15 5 5 5-5" />
                  <path d="m7 9 5-5 5 5" />
                </svg>
              </button>
            )}
            {/* 轮次导航（安卓/窄窗形态；桌面宽屏用左缘 rail，互斥）：与权限/
                附件/思考块按钮同排，右下角小计数显示当前轮/总轮数（已加载
                序号），点开消息区底部面板逐轮跳转。 */}
            {fabVisible && (
              <RoundNavButton
                rounds={rounds}
                activeId={activeRoundId}
                open={navOpen}
                onToggle={() => setNavOpen((v) => !v)}
                triggerRef={navBtnRef}
              />
            )}
            <input
              ref={fileRef}
              type="file"
              multiple
              className="hidden"
              onChange={(e) => void onFilesPicked(e.target.files)}
            />
            {/* 运行中且有可发送内容：排队发送按钮（桌面也可 Enter 入队；移动端无
                Enter 发送语义，这是唯一入队入口）。准入条件与 send() 一致——纯附件 /
                纯引用（无正文）同样可排队，不必凑一句说明文字。 */}
            {busy && !uploading && (draft.trim() || attachments.length > 0 || quote) && (
              <button
                onClick={send}
                title="排队发送（任务结束后自动发出，可点「立即」插队）"
                className="flex h-10 items-center gap-1 rounded-lg px-2 text-[11px] text-amber-300 transition hover:bg-amber-500/15"
              >
                ⏳ 排队
              </button>
            )}
            {/* 卡片内右下角圆形按钮（与工具钮同排）：空闲 = ↑ 发送；工作中 = ■ 停止 */}
            <button
              onClick={stopOrSend}
              disabled={
                !busy && (uploading || (!draft.trim() && attachments.length === 0 && !quote))
              }
              title={busy ? "停止当前任务" : "发送"}
              aria-label={busy ? "停止" : "发送"}
              className={`ml-auto flex h-10 w-10 items-center justify-center rounded-full transition ${
                busy
                  ? "bg-gray-200 text-gray-900 hover:bg-white"
                  : "bg-indigo-600 text-white hover:bg-indigo-500 disabled:opacity-40"
              }`}
            >
              {busy ? (
                // 方块：停止
                <svg width="14" height="14" viewBox="0 0 14 14" fill="currentColor">
                  <rect x="1" y="1" width="12" height="12" rx="2" />
                </svg>
              ) : (
                // 向上箭头：发送
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                  <path d="M12 19V5" />
                  <path d="m5 12 7-7 7 7" />
                </svg>
              )}
            </button>
          </div>
        </div>
      </div>

      {/* 统一消息操作菜单（长按 / 右键弹出，微信式）：媒体卡附带下载/转发/打开，
          消息级动作 = 引用 / 复制 / 多选。 */}
      {msgMenu && (
        <div
          className="fixed inset-0 z-50"
          onClick={() => {
            // 长按打开后抬指的合成 click 吞掉，防菜单瞬间自关。
            if (msgMenuClickGuard.current) {
              msgMenuClickGuard.current = false;
              return;
            }
            setMsgMenu(null);
          }}
          onContextMenu={(e) => {
            e.preventDefault();
            // 长按打开菜单 ~50ms 后 WebView 会补发一次 contextmenu，命中目标
            // 已是刚挂载的遮罩——与抬指合成 click 同理，用 guard 吞掉，否则
            // 长按文本弹出的菜单瞬间自关（图片长按无此事件所以一直正常）。
            if (msgMenuClickGuard.current) return;
            setMsgMenu(null);
          }}
        >
          <div
            className="absolute min-w-44 rounded-lg border border-[#2a2f3a] bg-[#141821] py-1 shadow-xl"
            style={{
              left: Math.max(8, Math.min(msgMenu.x, window.innerWidth - 200)),
              top: Math.max(8, Math.min(msgMenu.y, window.innerHeight - 320)),
            }}
            onClick={(e) => e.stopPropagation()}
          >
            {msgMenu.media && (
              <>
                <div
                  className="truncate px-3 py-1 text-[10px] text-gray-600"
                  title={msgMenu.media.path}
                >
                  {msgMenu.media.display || msgMenu.media.path}
                </div>
                {msgMenu.media.kind === "image" && (
                  <button
                    className={MENU_ITEM}
                    onClick={closeMenuAnd((m) =>
                      m.media && menuViewImage(m.media.path),
                    )}
                  >
                    放大查看
                  </button>
                )}
                <button
                  className={MENU_ITEM}
                  onClick={closeMenuAnd((m) => m.media && menuSaveMedia(m.media.path))}
                >
                  保存到本地
                </button>
                {isAndroidDevice() && (
                  <button
                    className={MENU_ITEM}
                    onClick={closeMenuAnd((m) => m.media && menuShareMedia(m.media.path))}
                  >
                    转发到其它应用
                  </button>
                )}
                <button
                  className={MENU_ITEM}
                  onClick={closeMenuAnd((m) => m.media && menuOpenMedia(m.media.path))}
                >
                  用系统应用打开
                </button>
                <div className="my-1 border-t border-[#2a2f3a]" />
              </>
            )}
            <button
              className={MENU_ITEM}
              onClick={closeMenuAnd((m) => {
                // 与 buildPackageText 同一套角色称谓。
                const who =
                  m.entry.role === "user"
                    ? "用户"
                    : m.entry.role === "assistant"
                      ? "助手"
                      : m.entry.role === "thinking"
                        ? "思考"
                        : "工具";
                setQuote({ who, text: buildQuoteText(m.entry.content) });
              })}
            >
              引用
            </button>
            <button
              className={MENU_ITEM}
              onClick={closeMenuAnd((m) => {
                const text = stripMediaMarkers(m.entry.content).trim();
                if (!text) {
                  flashNotice("纯媒体消息，请长按媒体卡操作");
                  return;
                }
                copyText(text)
                  .then(() => flashNotice("已复制"))
                  .catch((err) => onError?.(String(err)));
              })}
            >
              复制
            </button>
            {/* 选择文字（安卓限定；桌面本来就可用原生拖选；用户/助手正文 + 思考
                块正文接了 text-selectable 渲染，工具/状态行没有）。进入该条消息的
                原生选择模式，长按出手势手柄按范围复制。空正文（纯媒体/空思考）
                没有可复制的字，不显示该项。 */}
            {isAndroidDevice() &&
              (msgMenu.entry.role === "user" ||
                msgMenu.entry.role === "assistant" ||
                (msgMenu.entry.role === "thinking" &&
                  thinkingBodyText(msgMenu.entry.content) !== "")) && (
                <button
                  className={MENU_ITEM}
                  onClick={closeMenuAnd((m) => {
                    setTextSelectId(m.entry.id);
                    flashNotice("长按文字拖动手柄选择范围");
                  })}
                >
                  选择文字
                </button>
              )}
            {/* 多选（全部条目类型可进选择模式：用户/助手/思考/工具行）。 */}
            <button
              className={MENU_ITEM}
              onClick={closeMenuAnd((m) => {
                setSelectIds(new Set([m.entry.id]));
                if (navigator.vibrate) navigator.vibrate(15);
              })}
            >
              多选
            </button>
            {/* 分叉到新会话（用户/助手消息；一次做完：复制可见历史到新会话分支 +
                自动回滚锚点之后的文件改动，原会话不动）。 */}
            {onFork &&
              (msgMenu.entry.role === "user" || msgMenu.entry.role === "assistant") && (
                <button
                  className={MENU_ITEM}
                  onClick={closeMenuAnd((m) => onFork(m.entry))}
                >
                  ⑂ 从这里分叉到新会话
                </button>
              )}
          </div>
        </div>
      )}
      {/* 媒体操作反馈气泡（保存成功 / 已复制）。 */}
      {notice && (
        <div className="fixed bottom-24 left-1/2 z-50 -translate-x-1/2 rounded-full border border-[#2a2f3a] bg-[#141821] px-4 py-1.5 text-xs text-gray-300 shadow-xl">
          {notice}
        </div>
      )}

      {/* 文字选择模式的退出钮：系统选择手柄在消息正文上，完成即恢复长按菜单。 */}
      {textSelectId !== null && (
        <button
          className="fixed bottom-24 right-1/2 z-50 translate-x-1/2 rounded-full border border-[#2a2f3a] bg-[#141821] px-5 py-1.5 text-xs text-gray-300 shadow-xl"
          onClick={() => {
            setTextSelectId(null);
            window.getSelection?.()?.removeAllRanges();
          }}
        >
          完成选择
        </button>
      )}

      {/* 全屏媒体画廊：图片查看器 / 视频播放器 + 左右滑切换上一个/下一个。 */}
      {gallery && (
        <MediaGallery
          key={gallery.start}
          items={gallery.items}
          start={gallery.start}
          seed={gallery.seed}
          onError={onError}
          onClose={() => setGallery(null)}
        />
      )}
    </div>
  );
}
