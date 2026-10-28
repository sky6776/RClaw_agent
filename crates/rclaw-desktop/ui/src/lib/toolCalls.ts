/**
 * 工具 / Skill 调用条目的纯数据层：直播事件、快照 / 增量条目与展开详情之间的换算，
 * 以及折叠行的文案 / 目标摘要 / 行级 diff。UI 在 components/ToolCallRow.tsx。
 *
 * 口径：
 * - 一条 status 条目 = 一批 tool_calls（同一次 ReAct 迭代），`Entry.tools` 每项一个调用；
 * - 直播条目的文案与回放条目逐字一致（`toolBatchLabel`），增量批才能按文案对位盖章；
 * - 列表级入参是运行时裁剪过的（长字符串截断），展开详情按落库行 id 懒加载全文。
 */
import type { ToolCallSummary, ToolCallsEvent } from "./api";

export type ToolCallStatus = "running" | "done" | "error" | "interrupted";

export interface ToolCallView {
  /** ToolUse.id（同一批内唯一）。 */
  id: string;
  name: string;
  /** 列表级入参（已裁剪；`inputTruncated` 为 true 时展开需懒加载全文）。 */
  input: unknown;
  inputTruncated: boolean;
  /** Skill 归属（Manifest 专用工具所属 skill / read 激活的 skill）。 */
  skill: string | null;
  status: ToolCallStatus;
  /** 直播结束事件带回的结果预览（回放条目没有，展开时懒加载）。 */
  preview?: string;
  previewTruncated?: boolean;
  durationMs: number | null;
}

type BegunCalls = Extract<ToolCallsEvent, { phase: "begun" }>["batch"]["calls"];
type FinishedResults = Extract<ToolCallsEvent, { phase: "finished" }>["batch"]["results"];

/** 工具轮 status 条目文案：与后端 `tool_batch_label` 逐字一致。 */
export function toolBatchLabel(names: string[]): string {
  return `🔧 调用工具: ${names.join(", ")}`;
}

/** 快照 / 增量条目 → 视图（成败 / 中断来自侧表；无记录按已完成展示）。 */
export function viewsFromSummary(calls: ToolCallSummary[]): ToolCallView[] {
  return calls.map((c) => ({
    id: c.id,
    name: c.name,
    input: c.input,
    inputTruncated: c.input_truncated,
    skill: c.skill ?? null,
    status: c.aborted ? "interrupted" : c.is_error === true ? "error" : "done",
    durationMs: c.duration_ms ?? null,
  }));
}

/** 直播开始事件 → 运行中视图。 */
export function viewsFromBegun(calls: BegunCalls): ToolCallView[] {
  return calls.map((c) => ({
    id: c.id,
    name: c.name,
    input: c.input,
    inputTruncated: c.input_truncated,
    skill: c.skill,
    status: "running",
    durationMs: null,
  }));
}

/** 直播结束事件并入：按 id 配对写状态 / 预览 / 耗时 / skill；没配到结果的保持原状。
 *  取消清理补发的结束事件逐条带 aborted → 「已中断」（占位结果不当失败展示）。 */
export function applyFinished(views: ToolCallView[], results: FinishedResults): ToolCallView[] {
  return views.map((v) => {
    const r = results.find((x) => x.id === v.id);
    if (!r) return v;
    return {
      ...v,
      status: r.aborted ? "interrupted" : r.is_error ? "error" : "done",
      preview: r.preview,
      previewTruncated: r.truncated,
      durationMs: r.duration_ms ?? v.durationMs,
      skill: r.skill ?? v.skill,
    };
  });
}

/** 直播侧尚未拿到真实结果的状态：仍在运行，或 turn 收口时被就地标为「已中断」——
 *  后者只是前端的推断（取消确认 / 同会话另一轮的回复先到），库里的成败 / aborted
 *  侧表与随后到达的结束事件都是更权威的事实，必须能覆盖它。 */
export function isUnsettled(status: ToolCallStatus): boolean {
  return status === "running" || status === "interrupted";
}

/** 落库条目盖到直播条目上：未定局的（错过结束事件 / 被推断为中断）取库里的成败——
 *  链真被掐断时侧表 aborted 同样给出「已中断」；直播已有的预览与耗时保留，库里有
 *  耗时 / skill 时以库为准。 */
export function mergeCommitted(
  live: ToolCallView[] | undefined,
  committed: ToolCallView[],
): ToolCallView[] {
  if (!live || live.length === 0) return committed;
  return committed.map((c) => {
    const l = live.find((x) => x.id === c.id);
    if (!l) return c;
    return {
      ...l,
      status: isUnsettled(l.status) ? c.status : l.status,
      durationMs: c.durationMs ?? l.durationMs,
      skill: c.skill ?? l.skill,
      inputTruncated: l.inputTruncated || c.inputTruncated,
    };
  });
}

/** turn 收口（最终回复 / 取消 / 发送失败）：仍在运行的调用标为中断。 */
export function settleInterrupted(views: ToolCallView[]): ToolCallView[] {
  if (!views.some((v) => v.status === "running")) return views;
  return views.map((v) => (v.status === "running" ? { ...v, status: "interrupted" } : v));
}

/** 直播批次与落库条目是否同一批：调用 id 序列一致（熔断 / 中断占位的链同样成立）。 */
export function sameToolBatch(a: ToolCallView[] | undefined, b: ToolCallView[] | undefined): boolean {
  if (!a || !b || a.length !== b.length) return false;
  return a.every((v, i) => v.id === b[i].id && v.name === b[i].name);
}

/** 工具名 → 动作文案（进行中 / 已完成）。未收录的内置工具显示原名；Skill 专用工具
 *  （带 skill 归属且不在表内）显示「调用 Skill」。 */
const TOOL_VERBS: Record<string, [running: string, done: string]> = {
  read: ["正在读取", "读取"],
  write: ["正在写入", "写入"],
  edit: ["正在编辑", "编辑"],
  delete: ["正在删除", "删除"],
  grep: ["正在搜索", "搜索"],
  list: ["正在列出", "列出"],
  shell: ["正在执行", "执行"],
  web_search: ["正在联网搜索", "联网搜索"],
  web_fetch: ["正在抓取网页", "抓取网页"],
  browser_navigate: ["正在打开网页", "打开网页"],
  browser_snapshot: ["正在截取页面快照", "页面快照"],
  browser_click: ["正在点击", "点击"],
  browser_type: ["正在输入", "输入"],
  browser_scroll: ["正在滑动", "滑动"],
  browser_screenshot: ["正在截图", "页面截图"],
  browser_close: ["正在关闭页面", "关闭页面"],
  memory: ["正在读写记忆", "记忆"],
  skill_manage: ["正在管理技能", "管理技能"],
  cron_add: ["正在添加定时任务", "添加定时任务"],
  Agent: ["正在派发子代理", "子代理"],
  TaskList: ["正在查看任务", "任务列表"],
  AgentStatus: ["正在查看子代理", "子代理状态"],
  AgentWait: ["正在等待子代理", "等待子代理"],
  AgentResume: ["正在恢复子代理", "恢复子代理"],
  CreateWorkflow: ["正在创建工作流", "创建工作流"],
};

/** 内置工具动作文案查表：只认自有键（工具名来自模型侧，`toString` 之类原型键不算）。 */
function builtinVerbs(name: string): [running: string, done: string] | undefined {
  return Object.prototype.hasOwnProperty.call(TOOL_VERBS, name) ? TOOL_VERBS[name] : undefined;
}

export function toolVerb(call: Pick<ToolCallView, "name" | "skill" | "status">): string {
  const pair = builtinVerbs(call.name);
  const running = call.status === "running";
  if (pair) return running ? pair[0] : pair[1];
  if (call.skill) return running ? "正在调用 Skill" : "调用 Skill";
  return running ? `正在调用 ${call.name}` : call.name;
}

/** 是否为 Skill 专用（Manifest）工具：有 skill 归属且不是内置工具名。 */
export function isSkillTool(call: Pick<ToolCallView, "name" | "skill">): boolean {
  return !!call.skill && builtinVerbs(call.name) === undefined;
}

export function inputField(input: unknown, key: string): unknown {
  if (input && typeof input === "object" && !Array.isArray(input)) {
    return (input as Record<string, unknown>)[key];
  }
  return undefined;
}

export function inputString(input: unknown, key: string): string | null {
  const v = inputField(input, key);
  return typeof v === "string" ? v : null;
}

/** 文本是否带运行时裁剪标记（列表级入参里被截断的长字符串）。 */
export function isTruncatedText(text: string): boolean {
  return text.endsWith("…（已截断，展开查看全文）");
}

/** 行数口径与展开面板的 diff 一致：末尾换行不算一空行（`"a\nb\n"` 是 2 行）。 */
export function lineCount(text: string): number {
  return splitLines(text).length;
}

/** 折叠行目标摘要：主体（文件名 / 命令首行 / 检索词…）、弱化的目录、行数增减。 */
export interface ToolTarget {
  main: string | null;
  dir: string | null;
  added: number | null;
  removed: number | null;
}

const PATH_TOOLS = new Set(["read", "write", "edit", "delete", "list"]);

function splitPath(path: string): { main: string; dir: string | null } {
  const normalized = path.replace(/\\/g, "/").replace(/\/+$/, "");
  const slash = normalized.lastIndexOf("/");
  if (slash === -1) return { main: normalized, dir: null };
  return { main: normalized.slice(slash + 1), dir: normalized.slice(0, slash + 1) };
}

function firstLine(text: string, max = 60): string {
  const line = text.split("\n")[0] ?? "";
  const chars = [...line];
  const cut = chars.length > max ? `${chars.slice(0, max).join("")}…` : line;
  return text.includes("\n") && !cut.endsWith("…") ? `${cut}…` : cut;
}

export function toolTarget(call: Pick<ToolCallView, "name" | "input" | "skill">): ToolTarget {
  const input = call.input;
  const none: ToolTarget = { main: null, dir: null, added: null, removed: null };
  if (PATH_TOOLS.has(call.name)) {
    const path = inputString(input, "path");
    if (!path) return none;
    const { main, dir } = splitPath(path);
    let added: number | null = null;
    let removed: number | null = null;
    if (call.name === "write") {
      const content = inputString(input, "content");
      if (content !== null && !isTruncatedText(content)) added = lineCount(content);
    } else if (call.name === "edit") {
      const oldText = inputString(input, "old_string");
      const newText = inputString(input, "new_string");
      const oldFull = oldText !== null && !isTruncatedText(oldText);
      const newFull = newText !== null && !isTruncatedText(newText);
      if (oldFull && newFull) {
        // 与展开面板的对照头同口径：按行级 diff 计增减，而不是新旧片段的行数
        // （片段共有的上下文行不算改动）。
        const counts = diffCounts(lineDiff(splitLines(oldText), splitLines(newText)));
        added = counts.added;
        removed = counts.removed;
      } else {
        if (newFull) added = lineCount(newText);
        if (oldFull) removed = lineCount(oldText);
      }
    }
    return { main, dir, added, removed };
  }
  if (call.name === "grep") {
    const pattern = inputString(input, "pattern");
    const path = inputString(input, "path");
    return { main: pattern, dir: path, added: null, removed: null };
  }
  if (call.name === "shell") {
    const command = inputString(input, "command");
    return { main: command ? firstLine(command) : null, dir: null, added: null, removed: null };
  }
  if (call.name === "web_search") {
    // 工具 schema 的键是 `search_query`（与运行时 `brief_input` 同口径）。
    const query = inputString(input, "search_query") ?? inputString(input, "query");
    return { main: query, dir: null, added: null, removed: null };
  }
  if (call.name === "web_fetch") {
    return { main: inputString(input, "url"), dir: null, added: null, removed: null };
  }
  if (call.name === "browser_navigate") {
    // 搜索式导航带 `query`，直达带 `url`（与运行时 brief 同序）。
    const target = inputString(input, "query") ?? inputString(input, "url");
    return { main: target, dir: null, added: null, removed: null };
  }
  if (call.name === "memory") {
    return { main: inputString(input, "action"), dir: null, added: null, removed: null };
  }
  if (call.name === "Agent") {
    const desc = inputString(input, "description") ?? inputString(input, "prompt");
    return { main: desc ? firstLine(desc, 40) : null, dir: null, added: null, removed: null };
  }
  if (isSkillTool(call)) {
    // Skill 专用工具：主体 = 工具名，skill 徽标另显。
    return { main: call.name, dir: null, added: null, removed: null };
  }
  // 通用：首个字符串字段的首行。
  if (input && typeof input === "object" && !Array.isArray(input)) {
    for (const value of Object.values(input as Record<string, unknown>)) {
      if (typeof value === "string" && value.trim()) {
        return { main: firstLine(value, 40), dir: null, added: null, removed: null };
      }
    }
  }
  return none;
}

/** 入参 → 参数表行（展开面板「传了什么参数」）：对象按键展开，其它整体一行。 */
export function paramRows(input: unknown): Array<{ key: string; value: string }> {
  if (input && typeof input === "object" && !Array.isArray(input)) {
    return Object.entries(input as Record<string, unknown>).map(([key, value]) => ({
      key,
      value: typeof value === "string" ? value : JSON.stringify(value, null, 2),
    }));
  }
  if (input === undefined) return [];
  return [{ key: "input", value: typeof input === "string" ? input : JSON.stringify(input, null, 2) }];
}

export function formatMs(ms: number): string {
  if (ms < 1000) return `${ms} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} s`;
  const m = Math.floor(ms / 60_000);
  const s = Math.round((ms % 60_000) / 1000);
  return `${m} 分 ${s} 秒`;
}

// ---------------------------------------------------------------------------
// 行级 diff（write / edit 的新旧对照）
// ---------------------------------------------------------------------------

export interface DiffRow {
  kind: "same" | "add" | "del";
  text: string;
}

/** O(n·m) 上限：超过改用「整段删除 + 整段新增」的退化形态（仍可读，只是不配对）。 */
const DIFF_CELL_LIMIT = 4_000_000;

export function splitLines(text: string): string[] {
  if (text.length === 0) return [];
  const lines = text.split("\n");
  // 末尾换行不算一空行（与常见 diff 工具一致）。
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}

/** 最长公共子序列对齐的行级 diff；`before` 为 null 表示新建（全部新增）。 */
export function lineDiff(before: string[] | null, after: string[]): DiffRow[] {
  if (before === null || before.length === 0) {
    return after.map((text) => ({ kind: "add", text }));
  }
  if (after.length === 0) {
    return before.map((text) => ({ kind: "del", text }));
  }
  // 剥公共前后缀，缩小 DP 规模。
  let head = 0;
  while (head < before.length && head < after.length && before[head] === after[head]) head++;
  let tail = 0;
  while (
    tail < before.length - head &&
    tail < after.length - head &&
    before[before.length - 1 - tail] === after[after.length - 1 - tail]
  ) {
    tail++;
  }
  const b = before.slice(head, before.length - tail);
  const a = after.slice(head, after.length - tail);
  const rows: DiffRow[] = before.slice(0, head).map((text) => ({ kind: "same", text }));
  if (b.length * a.length > DIFF_CELL_LIMIT) {
    rows.push(...b.map((text) => ({ kind: "del" as const, text })));
    rows.push(...a.map((text) => ({ kind: "add" as const, text })));
  } else {
    // LCS 长度表（(b.length+1) × (a.length+1)），回溯产出对齐行。
    const w = a.length + 1;
    const dp = new Uint32Array((b.length + 1) * w);
    for (let i = b.length - 1; i >= 0; i--) {
      for (let j = a.length - 1; j >= 0; j--) {
        dp[i * w + j] =
          b[i] === a[j]
            ? dp[(i + 1) * w + j + 1] + 1
            : Math.max(dp[(i + 1) * w + j], dp[i * w + j + 1]);
      }
    }
    let i = 0;
    let j = 0;
    while (i < b.length && j < a.length) {
      if (b[i] === a[j]) {
        rows.push({ kind: "same", text: b[i] });
        i++;
        j++;
      } else if (dp[(i + 1) * w + j] >= dp[i * w + j + 1]) {
        rows.push({ kind: "del", text: b[i] });
        i++;
      } else {
        rows.push({ kind: "add", text: a[j] });
        j++;
      }
    }
    while (i < b.length) rows.push({ kind: "del", text: b[i++] });
    while (j < a.length) rows.push({ kind: "add", text: a[j++] });
  }
  rows.push(...before.slice(before.length - tail).map((text) => ({ kind: "same" as const, text })));
  return rows;
}

/** diff 的增减统计。 */
export function diffCounts(rows: DiffRow[]): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const r of rows) {
    if (r.kind === "add") added++;
    else if (r.kind === "del") removed++;
  }
  return { added, removed };
}
