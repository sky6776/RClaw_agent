import React, { useEffect, useMemo, useState } from "react";
import { api, type ToolCallDetail, type ToolChainDetail } from "../lib/api";
import {
  diffCounts,
  formatMs,
  inputField,
  inputString,
  isSkillTool,
  isTruncatedText,
  lineDiff,
  paramRows,
  splitLines,
  toolTarget,
  toolVerb,
  type DiffRow,
  type ToolCallView,
} from "../lib/toolCalls";
import { GearIcon, PencilIcon, SearchIcon, TerminalIcon } from "./icons";

/**
 * 工具 / Skill 调用行（zcode ToolLayout 同款）：默认折叠成一行「动作 · 目标 · 状态 ›」，
 * 点小箭头展开详情——read 看读的哪个文件与返回内容，write / edit 看新旧对照，shell 看
 * 命令与输出，Skill 看用了哪个 skill 的哪个工具、传了什么参数、返回什么。
 *
 * 数据来源两路、同一渲染：直播条目拿事件里的列表级入参 + 结果预览；落库后（条目带
 * 行 id）展开时按行 id 懒加载完整入参 / 结果 / 写入前快照——重启、重装后从库里重建的
 * 条目走的就是这一路，看到的和当时一样。
 */

/** 展开状态按 (会话, 条目, 调用) 记在模块级：Chat 随页面切换卸载重挂时不丢。 */
const openState = new Map<string, boolean>();

/** 详情缓存（按会话 + 行 id）：同一行多次展开不重复读库；失败的请求不缓存，可重试。 */
const detailCache = new Map<string, Promise<ToolChainDetail>>();

function loadDetail(chatId: string, msgId: number): Promise<ToolChainDetail> {
  const key = `${chatId}:${msgId}`;
  let pending = detailCache.get(key);
  if (!pending) {
    pending = api.chatToolDetail(chatId, msgId);
    detailCache.set(key, pending);
    pending.catch(() => detailCache.delete(key));
  }
  return pending;
}

function toolIcon(call: ToolCallView) {
  if (call.name === "shell") return <TerminalIcon />;
  if (call.name === "edit" || call.name === "write" || call.name === "delete") return <PencilIcon />;
  if (
    call.name === "read" ||
    call.name === "grep" ||
    call.name === "list" ||
    call.name === "web_search" ||
    call.name === "web_fetch" ||
    call.name === "browser_navigate"
  ) {
    return <SearchIcon />;
  }
  if (isSkillTool(call)) return <span className="text-[11px] leading-none">🧩</span>;
  return <GearIcon />;
}

function StatusGlyph({ status }: { status: ToolCallView["status"] }) {
  if (status === "running") {
    return (
      <span className="inline-flex shrink-0 animate-spin text-indigo-400" aria-label="进行中">
        <GearIcon />
      </span>
    );
  }
  if (status === "error") {
    return (
      <span className="shrink-0 text-[11px] text-red-400" aria-label="失败">
        ✕
      </span>
    );
  }
  if (status === "interrupted") {
    return (
      <span className="shrink-0 text-[11px] text-amber-400" aria-label="已中断">
        ⏹
      </span>
    );
  }
  return (
    <span className="shrink-0 text-[11px] text-emerald-400" aria-label="完成">
      ✓
    </span>
  );
}

function Chevron({ open }: { open: boolean }) {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2.2"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={`shrink-0 transition-transform ${open ? "rotate-90" : ""}`}
      aria-hidden
    >
      <path d="m9 6 6 6-6 6" />
    </svg>
  );
}

/** 一批工具调用（一条 status 条目）：逐调用一行。 */
export default function ToolBatchRows({
  chatId,
  entryId,
  msgId,
  calls,
}: {
  chatId: string;
  entryId: number;
  msgId?: number;
  calls: ToolCallView[];
}) {
  return (
    <div className="flex w-full flex-col gap-1">
      {calls.map((call, index) => (
        <ToolCallRow
          key={`${call.id}:${index}`}
          chatId={chatId}
          entryId={entryId}
          msgId={msgId}
          call={call}
        />
      ))}
    </div>
  );
}

function ToolCallRow({
  chatId,
  entryId,
  msgId,
  call,
}: {
  chatId: string;
  entryId: number;
  msgId?: number;
  call: ToolCallView;
}) {
  const stateKey = `${chatId}:${entryId}:${call.id}`;
  const [open, setOpen] = useState(() => openState.get(stateKey) ?? false);
  const target = useMemo(() => toolTarget(call), [call]);
  const verb = toolVerb(call);
  const skillTool = isSkillTool(call);
  const title = [verb, target.main, target.dir, call.skill ? `Skill: ${call.skill}` : null]
    .filter(Boolean)
    .join(" ");
  return (
    <div className="flex min-w-0 flex-col items-start">
      <button
        type="button"
        onClick={(ev) => {
          // 不冒泡：多选模式下点展开不应切换整行选中。
          ev.stopPropagation();
          const next = !open;
          openState.set(stateKey, next);
          setOpen(next);
        }}
        aria-expanded={open}
        title={title}
        className={`flex max-w-full items-center gap-2 rounded-xl bg-[#161a22] px-3 py-1.5 text-left text-xs text-gray-400 transition hover:bg-[#1a2030] hover:text-gray-200 ${
          open ? "rounded-b-none" : ""
        }`}
      >
        <span className="shrink-0 text-gray-500">{toolIcon(call)}</span>
        <span className="shrink-0 text-gray-300">{verb}</span>
        {call.skill && (
          <span
            className="shrink-0 rounded-md bg-indigo-500/15 px-1.5 py-px text-[10px] text-indigo-300"
            title={`Skill: ${call.skill}`}
          >
            {call.skill}
          </span>
        )}
        {target.main && (
          <span className={`min-w-0 truncate ${skillTool ? "font-mono text-gray-300" : "font-medium text-gray-200"}`}>
            {target.main}
          </span>
        )}
        {target.dir && <span className="min-w-0 truncate text-gray-500">{target.dir}</span>}
        {target.added !== null && <span className="shrink-0 text-emerald-400">+{target.added}</span>}
        {target.removed !== null && <span className="shrink-0 text-red-400">-{target.removed}</span>}
        {call.durationMs !== null && call.status !== "running" && (
          <span className="shrink-0 text-[10px] text-gray-600">{formatMs(call.durationMs)}</span>
        )}
        <StatusGlyph status={call.status} />
        <span className="shrink-0 text-gray-600">
          <Chevron open={open} />
        </span>
      </button>
      {open && (
        <div className="w-full max-w-full rounded-b-xl rounded-tr-xl border border-[#242a36] bg-[#11151d] px-3 py-2 text-xs text-gray-300">
          <ToolCallDetailPanel chatId={chatId} msgId={msgId} call={call} />
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 展开详情
// ---------------------------------------------------------------------------

/** 合并后的详情数据（直播或落库任一来源）。 */
interface Resolved {
  input: unknown;
  /** 入参仍是裁剪版（全文尚不可得：直播期或详情未到）。 */
  inputPartial: boolean;
  result: { content: string; is_error: boolean } | null;
  /** 结果仍是预览（全文尚不可得）。 */
  resultPartial: boolean;
  /**
   * 调用被中断：库里的结果是 aborted 占位（is_error = true），返回区按「已中断」而不是
   * 「错误」标注，与状态行一致。
   */
  interrupted: boolean;
  checkpoint: ToolCallDetail["checkpoint"] | null;
  /** 库里的详情已到：此后 checkpoint 为空才是权威的「没有写入前快照」。 */
  detailLoaded: boolean;
  startedMs: number | null;
  durationMs: number | null;
}

function ToolCallDetailPanel({
  chatId,
  msgId,
  call,
}: {
  chatId: string;
  msgId?: number;
  call: ToolCallView;
}) {
  const [detail, setDetail] = useState<ToolCallDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [retry, setRetry] = useState(0);

  useEffect(() => {
    if (msgId === undefined) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    loadDetail(chatId, msgId)
      .then((chain) => {
        if (cancelled) return;
        const found = chain.calls.find((c) => c.id === call.id) ?? null;
        setDetail(found);
        if (!found) setError("库里没有这条调用的记录");
      })
      .catch((e) => {
        if (!cancelled) setError(`详情加载失败: ${String(e)}`);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [chatId, msgId, call.id, retry]);

  const resolved: Resolved = {
    input: detail ? detail.input : call.input,
    inputPartial: !detail && call.inputTruncated,
    result: detail?.result
      ? detail.result
      : call.preview !== undefined
        ? { content: call.preview, is_error: call.status === "error" }
        : null,
    resultPartial: !detail?.result && !!call.previewTruncated,
    interrupted: call.status === "interrupted",
    checkpoint: detail?.checkpoint ?? null,
    detailLoaded: detail !== null,
    startedMs: detail?.started_ms ?? null,
    durationMs: detail?.duration_ms ?? call.durationMs,
  };

  return (
    <div className="flex flex-col gap-2">
      <MetaLine call={call} resolved={resolved} loading={loading} />
      {error && (
        <div className="flex items-center gap-2 text-[11px] text-amber-400">
          <span>{error}</span>
          <button
            type="button"
            className="rounded border border-[#2a2f3a] px-1.5 py-px text-gray-300 hover:text-gray-100"
            onClick={() => setRetry((n) => n + 1)}
          >
            重试
          </button>
        </div>
      )}
      <KindBody call={call} resolved={resolved} />
    </div>
  );
}

function MetaLine({
  call,
  resolved,
  loading,
}: {
  call: ToolCallView;
  resolved: Resolved;
  loading: boolean;
}) {
  const statusText =
    call.status === "running"
      ? "进行中"
      : call.status === "error"
        ? "失败"
        : call.status === "interrupted"
          ? resolved.result
            ? "已中断（turn 被取消或异常结束）"
            : "已中断（turn 被取消或异常结束，没有拿到结果）"
          : "完成";
  const started = resolved.startedMs ? new Date(resolved.startedMs) : null;
  const two = (n: number) => String(n).padStart(2, "0");
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-gray-500">
      <span>
        工具 <span className="font-mono text-gray-300">{call.name}</span>
      </span>
      {call.skill && (
        <span>
          Skill <span className="text-indigo-300">{call.skill}</span>
        </span>
      )}
      <span>{statusText}</span>
      {resolved.durationMs !== null && <span>耗时 {formatMs(resolved.durationMs)}</span>}
      {started && !Number.isNaN(started.getTime()) && (
        <span>
          {two(started.getHours())}:{two(started.getMinutes())}:{two(started.getSeconds())} 开始
        </span>
      )}
      {loading && <span className="text-gray-600">加载详情…</span>}
    </div>
  );
}

function KindBody({ call, resolved }: { call: ToolCallView; resolved: Resolved }) {
  switch (call.name) {
    case "read":
      return <ReadBody resolved={resolved} />;
    case "write":
      return <WriteBody resolved={resolved} />;
    case "edit":
      return <EditBody resolved={resolved} />;
    case "delete":
      return <DeleteBody resolved={resolved} />;
    case "shell":
      return <ShellBody resolved={resolved} />;
    default:
      return <GenericBody call={call} resolved={resolved} />;
  }
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="flex min-w-0 flex-col gap-1">
      <div className="text-[10px] uppercase tracking-wide text-gray-600">{title}</div>
      {children}
    </div>
  );
}

function PathLine({ path, extra }: { path: string | null; extra?: string | null }) {
  if (!path) return null;
  return (
    <div className="flex min-w-0 flex-wrap items-center gap-x-2 font-mono text-[11px] text-gray-300">
      <span className="break-all">{path}</span>
      {extra && <span className="text-gray-500">{extra}</span>}
    </div>
  );
}

/** 文本块（结果 / 命令 / 文件内容）：等宽、可滚动、超长只渲染前 20k 字符。 */
const TEXT_RENDER_LIMIT = 20_000;

function TextBlock({
  text,
  tone,
  partial,
  emptyHint,
}: {
  text: string | null;
  tone?: "error" | "normal";
  /** 文本是预览 / 裁剪版（全文待加载或不可得）。 */
  partial?: boolean;
  emptyHint?: string;
}) {
  if (text === null) {
    return emptyHint ? <div className="text-[11px] text-gray-600">{emptyHint}</div> : null;
  }
  const over = text.length > TEXT_RENDER_LIMIT;
  const shown = over ? text.slice(0, TEXT_RENDER_LIMIT) : text;
  return (
    <div className="min-w-0">
      <pre
        className={`max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-lg border border-[#242a36] bg-[#0d1117] px-2.5 py-2 font-mono text-[11px] leading-relaxed ${
          tone === "error" ? "text-red-300" : "text-gray-300"
        }`}
      >
        {shown.length === 0 ? <span className="text-gray-600">（空）</span> : shown}
      </pre>
      {(over || partial) && (
        <div className="mt-0.5 text-[10px] text-gray-600">
          {over ? `仅显示前 ${TEXT_RENDER_LIMIT.toLocaleString()} 字符（共 ${text.length.toLocaleString()}）` : "预览已截断"}
          {partial && !over ? "，完成落库后展开可看全文" : ""}
        </div>
      )}
    </div>
  );
}

function ResultSection({ resolved, title = "返回" }: { resolved: Resolved; title?: string }) {
  const r = resolved.result;
  const failed = !!r?.is_error && !resolved.interrupted;
  const suffix = resolved.interrupted && r ? "（已中断）" : failed ? "（错误）" : "";
  return (
    <Section title={`${title}${suffix}`}>
      <TextBlock
        text={r ? r.content : null}
        tone={failed ? "error" : "normal"}
        partial={resolved.resultPartial}
        emptyHint="尚无结果"
      />
    </Section>
  );
}

function ReadBody({ resolved }: { resolved: Resolved }) {
  const path = inputString(resolved.input, "path");
  const offset = inputField(resolved.input, "offset");
  const limit = inputField(resolved.input, "limit");
  // read 的 offset 是 0 基行号（工具 schema：「起始行号（0 基，默认 0）」），展示按
  // 人类习惯换成 1 基。
  const firstLine = (typeof offset === "number" ? offset : 0) + 1;
  const range =
    typeof offset === "number" || typeof limit === "number"
      ? `第 ${firstLine} 行起${typeof limit === "number" ? `，共 ${limit} 行` : ""}`
      : null;
  return (
    <>
      <Section title="文件">
        <PathLine path={path} extra={range} />
      </Section>
      <ResultSection resolved={resolved} title="读到的内容" />
    </>
  );
}

/** 行级 diff 视图（上限 400 行）。 */
const DIFF_ROW_LIMIT = 400;

function DiffView({ rows }: { rows: DiffRow[] }) {
  const { added, removed } = diffCounts(rows);
  const shown = rows.slice(0, DIFF_ROW_LIMIT);
  return (
    <div className="min-w-0">
      <div className="mb-1 flex items-center gap-2 text-[10px] text-gray-500">
        <span className="text-emerald-400">+{added}</span>
        <span className="text-red-400">-{removed}</span>
      </div>
      <div className="max-h-80 overflow-auto rounded-lg border border-[#242a36] bg-[#0d1117] font-mono text-[11px] leading-relaxed">
        {shown.map((row, i) => (
          <div
            key={i}
            className={`whitespace-pre-wrap break-words px-2.5 ${
              row.kind === "add"
                ? "bg-emerald-500/10 text-emerald-200"
                : row.kind === "del"
                  ? "bg-red-500/10 text-red-200"
                  : "text-gray-400"
            }`}
          >
            <span className="mr-2 select-none text-gray-600">
              {row.kind === "add" ? "+" : row.kind === "del" ? "-" : " "}
            </span>
            {row.text.length === 0 ? " " : row.text}
          </div>
        ))}
      </div>
      {rows.length > shown.length && (
        <div className="mt-0.5 text-[10px] text-gray-600">仅显示前 {DIFF_ROW_LIMIT} 行（共 {rows.length} 行）</div>
      )}
    </div>
  );
}

function WriteBody({ resolved }: { resolved: Resolved }) {
  const path = inputString(resolved.input, "path");
  const content = inputString(resolved.input, "content");
  const contentPartial = content !== null && isTruncatedText(content);
  const before = resolved.checkpoint ? resolved.checkpoint.before : null;
  const rows = useMemo(
    () => (content === null || contentPartial ? null : lineDiff(before === null ? null : splitLines(before), splitLines(content))),
    [before, content, contentPartial],
  );
  return (
    <>
      <Section title="文件">
        <PathLine
          path={path}
          extra={
            resolved.checkpoint
              ? resolved.checkpoint.before === null
                ? "新建"
                : "覆盖写入（与写入前对照）"
              : resolved.detailLoaded && resolved.result && !resolved.result.is_error
                ? "（无写入前快照：未改动 / 工作区外 / 超限）"
                : null
          }
        />
      </Section>
      <Section title={resolved.checkpoint && resolved.checkpoint.before !== null ? "新旧对照" : "写入内容"}>
        {rows ? (
          <DiffView rows={rows} />
        ) : (
          <TextBlock
            text={content}
            partial={contentPartial}
            emptyHint="入参里没有 content"
          />
        )}
      </Section>
      <ResultSection resolved={resolved} />
    </>
  );
}

function EditBody({ resolved }: { resolved: Resolved }) {
  const path = inputString(resolved.input, "path");
  const oldText = inputString(resolved.input, "old_string");
  const newText = inputString(resolved.input, "new_string");
  const replaceAll = inputField(resolved.input, "replace_all") === true;
  const partial = (oldText !== null && isTruncatedText(oldText)) || (newText !== null && isTruncatedText(newText));
  const rows = useMemo(
    () => (oldText === null || newText === null || partial ? null : lineDiff(splitLines(oldText), splitLines(newText))),
    [oldText, newText, partial],
  );
  return (
    <>
      <Section title="文件">
        <PathLine path={path} extra={replaceAll ? "替换全部匹配" : null} />
      </Section>
      <Section title="新旧对照">
        {rows ? (
          <DiffView rows={rows} />
        ) : (
          <div className="flex flex-col gap-2">
            <div>
              <div className="mb-0.5 text-[10px] text-red-300">旧文本</div>
              <TextBlock text={oldText} partial={oldText !== null && isTruncatedText(oldText)} emptyHint="（无）" />
            </div>
            <div>
              <div className="mb-0.5 text-[10px] text-emerald-300">新文本</div>
              <TextBlock text={newText} partial={newText !== null && isTruncatedText(newText)} emptyHint="（无）" />
            </div>
          </div>
        )}
      </Section>
      <ResultSection resolved={resolved} />
    </>
  );
}

function DeleteBody({ resolved }: { resolved: Resolved }) {
  const path = inputString(resolved.input, "path");
  const before = resolved.checkpoint?.before ?? null;
  return (
    <>
      <Section title="文件">
        <PathLine path={path} extra={resolved.checkpoint ? "已删除（有写入前快照，可回滚）" : null} />
      </Section>
      {before !== null && (
        <Section title="删除前内容">
          <TextBlock text={before} />
        </Section>
      )}
      <ResultSection resolved={resolved} />
    </>
  );
}

function ShellBody({ resolved }: { resolved: Resolved }) {
  const command = inputString(resolved.input, "command");
  const cwd = inputString(resolved.input, "cwd");
  const timeout = inputField(resolved.input, "timeout_sec") ?? inputField(resolved.input, "timeout");
  return (
    <>
      <Section title="命令">
        <TextBlock text={command} emptyHint="入参里没有 command" />
        {(cwd || typeof timeout === "number") && (
          <div className="flex flex-wrap gap-x-3 text-[11px] text-gray-500">
            {cwd && (
              <span>
                目录 <span className="font-mono text-gray-300">{cwd}</span>
              </span>
            )}
            {typeof timeout === "number" && <span>超时 {timeout}s</span>}
          </div>
        )}
      </Section>
      <ResultSection resolved={resolved} title="输出" />
    </>
  );
}

function ParamTable({ input, partial }: { input: unknown; partial: boolean }) {
  const rows = paramRows(input);
  if (rows.length === 0) return <div className="text-[11px] text-gray-600">（无参数）</div>;
  return (
    <div className="min-w-0 overflow-hidden rounded-lg border border-[#242a36]">
      {rows.map((row) => (
        <div key={row.key} className="flex min-w-0 border-b border-[#242a36] last:border-b-0">
          <div className="w-28 shrink-0 bg-[#0d1117] px-2 py-1 font-mono text-[11px] text-indigo-300 max-md:w-20">
            {row.key}
          </div>
          <pre className="min-w-0 flex-1 whitespace-pre-wrap break-words px-2 py-1 font-mono text-[11px] text-gray-300">
            {row.value}
          </pre>
        </div>
      ))}
      {partial && <div className="px-2 py-1 text-[10px] text-gray-600">参数已截断，完成落库后展开可看全文</div>}
    </div>
  );
}

function GenericBody({ call, resolved }: { call: ToolCallView; resolved: Resolved }) {
  const skillTool = isSkillTool(call);
  return (
    <>
      {skillTool && (
        <div className="flex flex-wrap items-center gap-2 text-[11px]">
          <span className="rounded-md bg-indigo-500/15 px-1.5 py-px text-indigo-300">🧩 Skill · {call.skill}</span>
          <span className="text-gray-500">
            使用其工具 <span className="font-mono text-gray-300">{call.name}</span>
          </span>
        </div>
      )}
      <Section title="参数">
        <ParamTable input={resolved.input} partial={resolved.inputPartial} />
      </Section>
      <ResultSection resolved={resolved} />
    </>
  );
}
