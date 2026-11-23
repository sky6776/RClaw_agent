/**
 * 「多端同步」界面共用的小工具、文案与样式常量（docs/multi-device-sync.md）。
 *
 * 样式只用 index.css 里有浅色主题覆盖的那一组颜色，新面板在各套主题下都读得清。
 */
import type { SyncConfig, SyncSessionMark } from "./api";

/** 会话列表里显示的标题：并发续写分出来的分支、换了键落地的会话带一个本机显示用的后缀
 *  （docs/multi-device-sync.md §7.6）。后缀不入库、不同步；重命名时用的仍是原标题。 */
export function syncTitle(title: string, mark: SyncSessionMark | undefined): string {
  if (!mark) return title;
  if (mark.branch_of) return `${title}（${mark.branch_of}的分支）`;
  if (mark.landed_from) return `${title} · ${mark.landed_from}`;
  return title;
}

export const SYNC_CARD = "mb-4 rounded-xl border border-[#2a2f3a] bg-[#141821] p-4";
export const SYNC_INPUT =
  "w-full rounded-md border border-[#2a2f3a] bg-[#0f131b] px-2 py-1 font-mono text-xs text-gray-200 focus:border-indigo-500/60 focus:outline-none";
export const SYNC_BTN_PRIMARY =
  "rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500 disabled:opacity-40";
export const SYNC_BTN =
  "rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-300 transition hover:text-gray-100 disabled:opacity-40";
export const SYNC_BTN_DANGER =
  "rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-400 transition hover:border-red-500/50 hover:text-red-300 disabled:opacity-40";
export const SYNC_BTN_SMALL =
  "rounded-md border border-[#2a2f3a] px-2 py-1 text-[11px] text-gray-300 transition hover:text-gray-100 disabled:opacity-40";
export const SYNC_HINT = "text-[10px] leading-relaxed text-gray-500";
export const SYNC_LABEL = "mb-1 block text-[11px] text-gray-400";
export const SYNC_ERROR_BOX =
  "rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs leading-relaxed text-red-300";
export const SYNC_WARN_BOX =
  "rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs leading-relaxed text-amber-300";
export const SYNC_INFO_BOX =
  "rounded-lg border border-indigo-500/40 bg-indigo-500/10 px-3 py-2 text-xs leading-relaxed text-indigo-200";

/** 同步范围的名字（与后端 SCOPE_NAMES 同序）与界面说明。 */
export const SYNC_SCOPES: Array<{ name: string; label: string; hint: string }> = [
  { name: "sessions", label: "会话", hint: "对话内容、标题和归档状态。" },
  { name: "memory", label: "记忆与每日笔记", hint: "长期记忆、用户画像和每日笔记，按条目合并。" },
  {
    name: "persona",
    label: "人格",
    hint: "别的设备改了人格，要在这台设备上看过差异、确认之后才写入；重启后生效。",
  },
  {
    name: "attachments",
    label: "附件",
    hint: "会话里引用的图片和文件。小图自动下载，其余点一下再下载。需要同时同步会话。",
  },
  {
    name: "skills",
    label: "技能",
    hint: "自己装的和 agent 自建的技能。带可执行文件的改动要在这台设备上确认。",
  },
  { name: "bots", label: "智能体定义", hint: "放宽了工具限制的改动要在这台设备上确认。" },
  {
    name: "cron",
    label: "定时任务",
    hint: "各设备的任务互相看得到；任务仍在原来的设备上执行，可以移到这台设备。",
  },
];

/** 引擎状态 → 界面上的说法与颜色。 */
export function syncStateLabel(state: string): { text: string; tone: string } {
  switch (state) {
    case "off":
      return { text: "没有开启", tone: "text-gray-500" };
    case "setup":
      return { text: "还没有创建或加入同步空间", tone: "text-amber-300" };
    case "standby":
      return { text: "另一个 RClaw 正在同步", tone: "text-amber-300" };
    case "confirm":
      return { text: "等你确认", tone: "text-amber-300" };
    case "paused":
      return { text: "已暂停", tone: "text-amber-300" };
    case "syncing":
      return { text: "正在同步", tone: "text-indigo-300" };
    case "idle":
      return { text: "已同步", tone: "text-emerald-400" };
    case "offline":
      return { text: "连不上存储", tone: "text-amber-300" };
    case "error":
      return { text: "出错了", tone: "text-red-400" };
    default:
      return { text: state, tone: "text-gray-400" };
  }
}

/** 这台设备已经在一个同步空间里（创建过或加入过）。 */
export function syncInSpace(state: string): boolean {
  return state !== "off" && state !== "setup" && state !== "standby";
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

/** 「多久以前」；0 或负数 = 还没有过。 */
export function formatAgo(ms: number, now: number = Date.now()): string {
  if (!ms || ms <= 0) return "还没有";
  const secs = Math.max(0, Math.round((now - ms) / 1000));
  if (secs < 10) return "刚刚";
  if (secs < 60) return `${secs} 秒前`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins} 分钟前`;
  const hours = Math.floor(mins / 60);
  if (hours < 48) return `${hours} 小时前`;
  return `${Math.floor(hours / 24)} 天前`;
}

/** 本地时间 `MM-DD HH:mm`；无效时间返回空串。 */
export function formatDateTime(ms: number): string {
  const d = new Date(ms);
  if (!ms || !Number.isFinite(d.getTime())) return "";
  const two = (n: number) => String(n).padStart(2, "0");
  return `${two(d.getMonth() + 1)}-${two(d.getDate())} ${two(d.getHours())}:${two(d.getMinutes())}`;
}

/** 文件内容 → base64（分块拼接，大文件不爆调用栈）。 */
export async function fileToBase64(file: Blob): Promise<string> {
  const buf = new Uint8Array(await file.arrayBuffer());
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < buf.length; i += chunk) {
    binary += String.fromCharCode(...buf.subarray(i, i + chunk));
  }
  return btoa(binary);
}

/** 把文本写进剪贴板。Android WebView 里 clipboard API 常被拒绝，退回隐藏文本框 + 复制命令。 */
export async function writeClipboard(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    // 落到下面的兜底。
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
  if (!ok) throw new Error("剪贴板不可用");
}

/** 用一个空格盖掉剪贴板里的内容（配对码用完之后）。空串在兜底路径上复制不出去，所以用空格。 */
export function clearClipboard(): Promise<void> {
  return writeClipboard(" ");
}

/** 存储设置填得够不够（只查必填项；地址对不对由后端在测试连接时报）。返回缺什么。 */
export function storageProblem(config: SyncConfig): string | null {
  switch (config.transport) {
    case "webdav":
      return config.webdav.url.trim() ? null : "请填写 WebDAV 地址";
    case "file":
      return config.file.dir.trim() ? null : "请填写共享文件夹的路径";
    case "s3":
      if (!config.s3.endpoint.trim()) return "请填写对象存储的地址";
      if (!config.s3.bucket.trim()) return "请填写存储桶的名字";
      if (!config.s3.access_key_id.trim()) return "请填写 Access Key ID";
      return null;
    default:
      return "请选择存储类型";
  }
}

/** 当前存储的一句话描述（地址不含密码）。 */
export function storageSummary(config: SyncConfig): string {
  switch (config.transport) {
    case "webdav":
      return `WebDAV · ${config.webdav.url || "（没填地址）"}`;
    case "file":
      return `共享文件夹 · ${config.file.dir || "（没填路径）"}`;
    case "s3":
      return `对象存储 · ${config.s3.endpoint || "（没填地址）"} / ${config.s3.bucket || "（没填存储桶）"}`;
    default:
      return config.transport;
  }
}

/** 界面上填的密码：没填 = 不动本机已经存着的。 */
export function secretOrNull(secret: string): string | null {
  return secret ? secret : null;
}
