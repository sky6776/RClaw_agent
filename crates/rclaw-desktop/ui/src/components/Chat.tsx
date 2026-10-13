import React, { useEffect, useMemo, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { api, parseToolStatusLine, type PermissionMode, type PlanStatus } from "../lib/api";
import { useIsMobile, isAndroidDevice } from "../lib/useIsMobile";
import { useBackClose } from "../lib/backNav";
import MediaPreviews, { parseMediaItems } from "./MediaPreviews";
import MediaGallery, { type GalleryItem, type GallerySeed } from "./MediaGallery";
import {
  CheckIcon,
  CopyIcon,
  GearIcon,
  PencilIcon,
  SearchIcon,
  ShieldIcon,
  TerminalIcon,
} from "./icons";

export type Role = "user" | "assistant" | "status" | "thinking";
export interface Entry {
  id: number;
  role: Role;
  content: string;
  /** 条目到达时间（毫秒）；历史恢复的条目没有，思考块据此显示持续时长。 */
  ts?: number;
  /** 持久化消息行 id（历史恢复路径携带；「分叉到新会话」的锚点，直播条目缺省）。 */
  msgId?: number;
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
  /** 从该消息锚点分叉出新会话（full = 携带完整转录；files = 同时回滚工作区文件）。 */
  onFork?: (entry: Entry, full?: boolean, files?: boolean) => void;
  /** 最近的 PlanRun 计划状态（流程步骤卡片；null = 无计划数据）。 */
  plan: PlanStatus | null;
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

/** 工具名 → 中文动作标签（未收录的工具显示原名）。 */
const TOOL_LABELS: Record<string, string> = {
  read: "读取",
  write: "写入",
  edit: "编辑",
  delete: "删除",
  grep: "搜索",
  list: "列出",
  shell: "命令",
  web_search: "联网搜索",
  web_browser: "网页浏览",
  memory: "记忆",
};

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

function toolIcon(tool: string) {
  if (tool === "shell") return <TerminalIcon />;
  if (tool === "edit" || tool === "write" || tool === "delete")
    return <PencilIcon />;
  if (tool === "read" || tool === "grep" || tool === "list" || tool === "web_search" || tool === "web_browser")
    return <SearchIcon />;
  return <GearIcon />;
}

/** 路径类摘要工具：尾部 +N/-N 计为行数增减，其余部分按路径拆文件名与目录。 */
const PATH_TOOLS = ["read", "write", "edit", "delete", "grep", "list"];

/** 拆分工具摘要：尾部连续的 +/- 行数计入增减，其余部分按路径拆文件名与目录。 */
function splitToolDetail(
  tool: string,
  detail: string,
): { main: string; dir: string | null; added: number | null; removed: number | null } {
  let rest = detail;
  const counts: number[] = [];
  let m: RegExpExecArray | null;
  // 仅路径类摘要做尾部 +/- 剥离；命令预览 / 检索词整段展示（文本可能以数字结尾）。
  const isPath = PATH_TOOLS.includes(tool);
  if (isPath) {
    while ((m = /\s+([+-]\d+)\s*$/.exec(rest)) !== null) {
      counts.unshift(Number(m[1]));
      rest = rest.slice(0, m.index);
    }
  }
  if (isPath) {
    const slash = rest.lastIndexOf("/");
    if (slash !== -1) {
      return {
        main: rest.slice(slash + 1),
        dir: rest.slice(0, slash + 1),
        added: counts[0] ?? null,
        removed: counts[1] ?? null,
      };
    }
  }
  return {
    main: rest,
    dir: null,
    added: counts[0] ?? null,
    removed: counts[1] ?? null,
  };
}

/** 工具动作流水行：图标 + 中文标签 + 文件路径（目录弱化）+ 行数增减。 */
function ToolStatusRow({
  tool,
  detail,
}: {
  tool: string;
  detail: string | null;
}) {
  const label = TOOL_LABELS[tool] ?? tool;
  const parts = detail ? splitToolDetail(tool, detail) : null;
  return (
    <div className="flex justify-start">
      <div
        className="flex max-w-[85%] items-center gap-2 rounded-xl bg-[#161a22] px-3 py-1.5 text-xs text-gray-400"
        title={detail ? `${tool} ${detail}` : tool}
      >
        <span className="shrink-0 text-gray-500">{toolIcon(tool)}</span>
        <span className="shrink-0 text-gray-300">{label}</span>
        {parts && (
          <>
            <span className="font-medium text-gray-200">{parts.main}</span>
            {parts.dir && <span className="min-w-0 truncate text-gray-500">{parts.dir}</span>}
            {parts.added !== null && (
              <span className="shrink-0 text-emerald-400">+{parts.added}</span>
            )}
            {parts.removed !== null && (
              <span className="shrink-0 text-red-400">-{parts.removed}</span>
            )}
          </>
        )}
      </div>
    </div>
  );
}

/** 把秒数格式化为「N 秒 / M 分 S 秒」。 */
function formatDuration(secs: number): string {
  if (secs < 60) return `${secs} 秒`;
  return `${Math.floor(secs / 60)} 分 ${secs % 60} 秒`;
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
 *  分支，原会话不动（ZCode fork 同款语义，第十二轮）。移动端走长按菜单项。 */
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
            {plan.completed
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
 *  新到块的默认状态，不影响任何已在屏上的块。 */
function ThinkingBlock({
  content,
  durationSecs,
  defaultOpen,
  onToggle,
}: {
  content: string;
  durationSecs: number | null;
  defaultOpen: boolean;
  onToggle?: (collapsed: boolean) => void;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const body = content.replace(/^💭\s*思考中\.\.\.\n?/, "").trim();
  return (
    <div className="flex justify-start">
      <div className="w-full overflow-hidden rounded-2xl rounded-bl-md border border-dashed border-[#3a4150] bg-[#141821]">
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
          <span className="text-[10px] text-gray-600">{open ? "▾" : "▸"}</span>
        </button>
        {open && (
          <div className="md-body border-t border-[#2a2f3a] px-4 py-2 text-xs whitespace-pre-wrap text-gray-400 max-md:px-3">
            {body}
          </div>
        )}
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
  plan,
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
  const noticeTimer = useRef<number | undefined>(undefined);
  const [exporting, setExporting] = useState(false);
  const [exportResult, setExportResult] = useState<string | null>(null);
  const [exportNotice, setExportNotice] = useState<string | null>(null);
  const selectPressTimer = useRef<number | null>(null);
  // 工具权限模式（null = 尚未加载到，隐藏选择器）。
  const [permMode, setPermMode] = useState<PermissionMode | null>(null);
  const [permOpen, setPermOpen] = useState(false);
  // 附件功能菜单（安卓：上传/拍照/拍视频/录音）。
  const [attachMenuOpen, setAttachMenuOpen] = useState(false);
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
  const [showJumpDown, setShowJumpDown] = useState(false);
  // 流式思考会让最后一条的 content 持续增长：把它加进依赖，流式期间跟随滚动。
  const lastContentLength = entries[entries.length - 1]?.content.length ?? 0;
  useEffect(() => {
    if (!stickToBottomRef.current) return;
    listRef.current?.scrollTo({ top: listRef.current.scrollHeight });
  }, [entries.length, lastContentLength, busy]);

  // 滚动时判定是否贴近底部（80px 容差）：决定后续输出是否继续跟随。
  const handleListScroll = () => {
    const el = listRef.current;
    if (!el) return;
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    stickToBottomRef.current = nearBottom;
    setShowJumpDown(!nearBottom);
  };

  // 回到底部并恢复跟随（发送消息 / 点击悬浮钮时调用）。
  const scrollListToBottom = (smooth = false) => {
    stickToBottomRef.current = true;
    setShowJumpDown(false);
    listRef.current?.scrollTo({
      top: listRef.current.scrollHeight,
      behavior: smooth ? "smooth" : "auto",
    });
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
    // 这里显示的只是当前会话的那一份，切回来还在。
    setAttachments([]);
    setHistoryIndex(null);
    setQuote(null);
    setSelectIds(null);
    setExportResult(null);
    draftRef.current = "";
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [sessionKey]);

  // 排队消息（第十二轮 · zcode 同款）：agent 运行期间发送的消息进入队列，显示在
  // 输入框上方，可删除 / 编辑 / 「立即」强制投递（走运行中的 turn 的插队通道）。
  // 队列本体活在 App（按会话分桶）：Chat 随页面切换（工作区/设置面板）会整体卸载，
  // 组件内 state 会把排着队的文本一起丢掉；逐条投递也由 App 的收尾监听统一驱动，
  // 人在哪个页面/会话都不影响排队消息按序发出。这里拿到的是当前会话的队列。

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

  const clearSelectPress = () => {
    if (selectPressTimer.current !== null) {
      window.clearTimeout(selectPressTimer.current);
      selectPressTimer.current = null;
    }
  };

  // 消息行的长按 / 右键 → 统一操作菜单（用户/助手/思考/工具行通用；媒体卡有
  // 独立入口且已 stopPropagation，不会同时触发）。touchmove 只在位移超过 12px
  // （touch slop）时取消计时：系统同坐标长按与手指微抖都会持续派发 move，
  // 无条件取消会让长按永远不触发。
  // 挂在整行（而非气泡内层）上：行内任意位置长按都可触发，包括气泡外空白。
  const bubblePressHandlers = (entry: Entry) => {
    let sx = 0;
    let sy = 0;
    return {
      onTouchStart: (e: React.TouchEvent) => {
        if (!isMobile || e.touches.length !== 1) return;
        const { clientX, clientY } = e.touches[0];
        sx = clientX;
        sy = clientY;
        clearSelectPress();
        selectPressTimer.current = window.setTimeout(() => {
          selectPressTimer.current = null;
          if (navigator.vibrate) navigator.vibrate(15);
          openMsgMenu({ entry, media: null, x: clientX, y: clientY }, true);
        }, 450);
      },
      onTouchEnd: clearSelectPress,
      onTouchMove: (e: React.TouchEvent) => {
        const t = e.touches[0];
        if (t && Math.hypot(t.clientX - sx, t.clientY - sy) > 12) clearSelectPress();
      },
      onTouchCancel: clearSelectPress,
      onContextMenu: (e: React.MouseEvent) => {
        e.preventDefault();
        openMsgMenu({ entry, media: null, x: e.clientX, y: e.clientY }, false);
      },
    };
  };
  // 卸载清定时器。
  useEffect(() => clearSelectPress, []);

  // 系统返回键关一层菜单 / 选择模式（与弹层栈共用管道）。
  useBackClose(msgMenu !== null, () => setMsgMenu(null));
  useBackClose(selectIds !== null, () => setSelectIds(null));
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
  const listItems = useMemo(
    () =>
      entries.map((e, i) => {
        if (e.role === "status") {
          const toolStatus = parseToolStatusLine(e.content);
          const selected = selectIds?.has(e.id) ?? false;
          if (toolStatus) {
            return (
              <div
                key={e.id}
                className={selectIds ? "flex items-start" : undefined}
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
                <div className={selectIds ? "min-w-0 flex-1" : undefined}>
                  <ToolStatusRow tool={toolStatus.tool} detail={toolStatus.detail} />
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
          const next = i + 1 < entries.length ? entries[i + 1] : undefined;
          const elapsed = e.ts && next?.ts ? Math.round((next.ts - e.ts) / 1000) : null;
          const durationSecs = elapsed !== null && elapsed >= 1 ? elapsed : null;
          const selected = selectIds?.has(e.id) ?? false;
          return (
            <div
              key={e.id}
              className={selectIds ? "flex cursor-pointer items-start" : undefined}
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
              <div className={selectIds ? "min-w-0 flex-1" : undefined}>
                <ThinkingBlock
                  content={e.content}
                  durationSecs={durationSecs}
                  defaultOpen={!futureThinkingCollapsed}
                  onToggle={onToggleThinking}
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
              className="flex justify-end"
              {...(selectIds ? undefined : bubblePressHandlers(e))}
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
                  }`}
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
            {...(selectIds ? undefined : bubblePressHandlers(e))}
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
              <div
                className={`md-body w-full text-sm text-gray-200 max-md:text-[15px] max-md:leading-relaxed ${
                  selectIds && selected ? "rounded-md ring-2 ring-indigo-400" : ""
                }`}
              >
                {/* 外部链接不在 WebView 内导航（会顶掉 GUI）：http(s) 一律交系统
                    浏览器打开（open_url：桌面 cmd start / Android ACTION_VIEW）。 */}
                <ReactMarkdown
                  remarkPlugins={[remarkGfm]}
                  components={{
                    a: ({ href, children }) => (
                      <a
                        href={href}
                        target="_blank"
                        rel="noreferrer"
                        className="text-indigo-400 underline decoration-indigo-400/40 underline-offset-2 hover:text-indigo-300"
                        onClick={(e) => {
                          // 只放行 http(s)：file:/mailto:/javascript: 等其它
                          // 协议交给 WebView 默认行为会顶掉 GUI 或有安全面。
                          if (!href || !/^https?:\/\//i.test(href)) {
                            e.preventDefault();
                            return;
                          }
                          e.preventDefault();
                          void api.openUrl(href).catch(() => undefined);
                        }}
                      >
                        {children}
                      </a>
                    ),
                    // 表格放横向滚动容器里（zcode 同款）：宽表不再把单元格挤成
                    // 一字一行的竖排，超出消息列宽度的部分左右滑动查看。
                    table: ({ children }) => (
                      <div className="table-scroll">
                        <table className="w-max min-w-full">{children}</table>
                      </div>
                    ),
                  }}
                >
                  {stripMediaMarkers(e.content)}
                </ReactMarkdown>
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
      }),
    // 行内闭包只触达 setState / ref（openMsgMenu、toggleSelected 等），不列入
    // 依赖是刻意的；onError 需要稳定身份（App 侧已 useCallback）；onFork 随会话
    // 变化（依赖 activeId），必须列入——否则分叉闭包捕获旧会话 id，切会话后
    // 点「分叉」会 fork 到上一个会话。
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [entries, selectIds, isMobile, futureThinkingCollapsed, onError, onFork],
  );

  return (
    <div className="chat-bg flex h-full flex-col">
      {/* 消息流（移动端收窄页边距，把宽度还给消息正文） */}
      <div
        ref={listRef}
        onScroll={handleListScroll}
        className="chat-msg-list flex-1 space-y-3 overflow-y-auto px-6 py-4 max-md:space-y-2.5 max-md:px-3 max-md:py-3"
      >
        {entries.length === 0 && (
          <div className="mt-24 text-center text-sm text-gray-600 max-md:mt-36 max-md:px-6">
            <p className="text-base text-gray-400">向 RClaw 发出第一条消息</p>
            <p className="mt-2">
              试试「帮我每天早上 8 点汇总一次待办」或输入 <code className="text-gray-500">/help</code>
            </p>
          </div>
        )}
        {listItems}
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
        {/* 向上翻阅历史时的「回到底部」钮：sticky 钉在滚动口底部，点击恢复跟随 */}
        {showJumpDown && (
          <div className="sticky bottom-2 z-20 flex justify-center">
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

      {/* 输入区（移动端贴底并让出手势条/安全区；桌面 p-4 不变）。 */}
      <div className="border-t border-[#2a2f3a] p-4 max-md:px-2.5 max-md:pt-2 max-md:pb-[calc(env(safe-area-inset-bottom)+0.5rem)]">

        {/* PlanRun 流程步骤卡片（可折叠）：最近的计划步骤清单，随 plan_status 轮询刷新。 */}
        {plan && plan.steps.length > 0 && (
          <PlanStepsCard plan={plan} onPlanPause={onPlanPause} />
        )}

        {/* 排队消息（zcode 同款，第十二轮）：agent 运行期间发送的消息在此排队，
            可删除 / 编辑 / 立即投递；会话收尾后由 App 逐条自动发出（App 级状态，
            页面切换不丢）。props.queue 已是当前会话的队列。 */}
        {queue.length > 0 && (
          <div className="mb-2 space-y-1.5">
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
                onClick={() => setSelectIds(null)}
                className="ml-auto rounded px-1.5 py-0.5 text-[11px] text-gray-400 transition hover:text-gray-200"
              >
                退出选择
              </button>
            </div>
            <div className="mt-2 flex flex-wrap gap-2">
              <button
                onClick={() => {
                  const text = buildPackageText();
                  setSelectIds(null);
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
            <input
              ref={fileRef}
              type="file"
              multiple
              className="hidden"
              onChange={(e) => void onFilesPicked(e.target.files)}
            />
            {/* 运行中且有草稿：排队发送按钮（桌面也可 Enter 入队；移动端无 Enter
                发送语义，这是唯一入队入口）。 */}
            {busy && draft.trim() && (
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
            {/* 分叉到新会话（用户/助手消息；从该消息复制出新会话分支，原会话不动）。
                full 变体携带被压缩折叠前的完整原文（存档式分叉）。 */}
            {onFork &&
              (msgMenu.entry.role === "user" || msgMenu.entry.role === "assistant") && (
                <>
                  <button
                    className={MENU_ITEM}
                    onClick={closeMenuAnd((m) => onFork(m.entry))}
                  >
                    ⑂ 从这里分叉到新会话
                  </button>
                  <button
                    className={MENU_ITEM}
                    onClick={closeMenuAnd((m) => onFork(m.entry, true))}
                  >
                    ⑂ 分叉（含完整历史）
                  </button>
                  <button
                    className={MENU_ITEM}
                    onClick={closeMenuAnd((m) => onFork(m.entry, false, true))}
                  >
                    ⑂ 分叉并回滚文件
                  </button>
                </>
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
