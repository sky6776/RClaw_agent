import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
} from "react";
import { flushSync } from "react-dom";
import { listen } from "@tauri-apps/api/event";
import {
  api,
  isStatusLine,
  isThinkingLine,
  type AgentEvent,
  type AppVersions,
  type ChatHistoryDelta,
  type ChatMeta,
  type HistoryMsg,
  type PermissionRequest,
  type PlanStatus,
  type Status,
  type UpdateInfo,
  type UsageSnapshot,
} from "./lib/api";
import Wizard from "./components/Wizard";
import Chat, { type Entry, type QueueItem } from "./components/Chat";
import AgentsPanel from "./components/AgentsPanel";
import BotsPanel from "./components/BotsPanel";
import ToolsPanel from "./components/ToolsPanel";
import SkillsPanel from "./components/SkillsPanel";
import FilePanel from "./components/FilePanel";
import EvolutionPanel from "./components/EvolutionPanel";
import CronPanel from "./components/CronPanel";
import ConfigPanel from "./components/ConfigPanel";
import TaskOverviewPanel from "./components/TaskOverviewPanel";
import TaskPanel from "./components/TaskPanel";
import TitleBar from "./components/TitleBar";
import MobileTopBar, { Drawer, DrawerScrim } from "./components/MobileChrome";
import PhotoEditor from "./components/PhotoEditor";
import CaptureDialog from "./components/CaptureDialog";
import KeepAliveCard from "./components/KeepAliveCard";
import { useIsMobile, isAndroidDevice } from "./lib/useIsMobile";
import { useEdgeSwipe } from "./lib/useEdgeSwipe";
import { installBackNavigation, useBackClose } from "./lib/backNav";
import {
  AboutIcon,
  AgentsIcon,
  BotsIcon,
  ChatIcon,
  CollapseIcon,
  ConfigIcon,
  CronIcon,
  EvolutionIcon,
  ExpandIcon,
  FolderIcon,
  SkillsIcon,
  TasksIcon,
  ToolsIcon,
} from "./components/icons";

type Page =
  | "chat"
  | "chats"
  | "bots"
  | "tools"
  | "skills"
  | "files"
  | "agents"
  | "evolution"
  | "cron"
  | "taskview"
  | "config"
  | "about";

/** 会话列表页的图标（对话气泡叠层）。 */
function SessionsIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round">
      <path d="M2 3.5h8.5v5H5L2 11V3.5z" />
      <path d="M6 12.5h7.5L16 15V8.2c0-.7-.4-1.2-1-1.2H14" />
    </svg>
  );
}

/** 导航项：页面 / 标签 / 图标。「对话」列在首位（收起后的图标栏也需要回到对话的入口）。 */
const NAV: Array<[Page, string, () => ReactNode]> = [
  ["chat", "对话", () => <ChatIcon />],
  ["chats", "会话", () => <SessionsIcon />],
  ["bots", "智能体", () => <BotsIcon />],
  ["tools", "工具", () => <ToolsIcon />],
  ["skills", "技能", () => <SkillsIcon />],
  ["files", "工作区", () => <FolderIcon />],
  ["agents", "Agent", () => <AgentsIcon />],
  ["evolution", "自进化", () => <EvolutionIcon />],
  ["cron", "定时任务", () => <CronIcon />],
  ["config", "配置", () => <ConfigIcon />],
  ["about", "关于", () => <AboutIcon />],
];

let next_entry_id = 1;

/** 默认（首个）会话 id。 */
const DEFAULT_CHAT = "main";

/** 本地新建会话的 id（「＋」新建与删空后的裸工作会话共用）。 */
const freshChatId = () =>
  `c-${Date.now().toString(36)}${Math.floor(Math.random() * 1e4).toString(36)}`;

/** 首条消息 → 本地乐观标题：首行、按码点截 60 字符（与后端派生标题同源）。 */
function deriveLocalTitle(content: string): string {
  return [...(content.trim().split("\n")[0] ?? "")].slice(0, 60).join("") || "新对话";
}

/** 历史行的落库时间（DTO 为 unix 秒）→ Entry.ts（毫秒）；≤0 = 旧数据缺失，
 *  不填 ts（界面不渲染时间标签）。 */
function historyEntryTs(secs: number): number | undefined {
  return secs > 0 ? secs * 1000 : undefined;
}

/** 思考文本归一化：剥直播快照前缀 + trim。持久化 reasoning 与直播思考块的对位比较
 *  两处共用（快照合并 + 增量盖章）——两边口径不一致就会留下双份思考块。 */
function normalizeThinkingText(text: string): string {
  return text.replace(/^💭\s*思考中\.\.\.\n?/, "").trim();
}

// ---------------------------------------------------------------------------
// 多主题：dark/light 之外的背景风格。base 决定复用哪套文字调色板
// （浅色系加挂 theme-light），聊天/侧边栏背景由 index.css 的 theme-* 层接管。
// ---------------------------------------------------------------------------

type ThemeId = "dark" | "light" | "starry" | "forest" | "sakura" | "cream";

const THEMES: Array<{
  id: ThemeId;
  name: string;
  base: "dark" | "light";
  desc: string;
  /** 选择器预览：聊天背景 / 侧边栏背景的代表色。 */
  swatch: [string, string];
}> = [
  { id: "dark", name: "深邃夜色", base: "dark", desc: "默认深色", swatch: ["#0f1219", "#12151d"] },
  { id: "light", name: "简约日间", base: "light", desc: "默认浅色", swatch: ["#f5f6f8", "#ffffff"] },
  { id: "starry", name: "浪漫星空", base: "dark", desc: "卡通星空深蓝紫", swatch: ["#171040", "#0a0e20"] },
  { id: "forest", name: "森林之夜", base: "dark", desc: "幽谷深绿", swatch: ["#0f1f18", "#0b1811"] },
  { id: "sakura", name: "樱花粉", base: "light", desc: "浅粉樱花", swatch: ["#fdf0f4", "#fff7fa"] },
  { id: "cream", name: "奶油暖阳", base: "light", desc: "暖阳奶油", swatch: ["#faf3e6", "#fffaf0"] },
];

const THEME_IDS: ThemeId[] = THEMES.map((t) => t.id);

/** 会话右键菜单项统一样式。 */
const CHAT_MENU_ITEM =
  "block w-full px-3 py-1.5 text-left text-xs text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100";

// 侧栏 / 右侧面板拖宽的夹取范围（px）。默认值对齐原 w-56 / w-72。
const LEFT_WIDTH_MIN = 160;
const LEFT_WIDTH_MAX = 480;
const RIGHT_WIDTH_MIN = 240;
const RIGHT_WIDTH_MAX = 560;

const clampWidth = (v: number, min: number, max: number) => Math.min(max, Math.max(min, v));

/** 从 localStorage 读宽度偏好（缺失 / 非法 / 越界时回退默认值，与写入侧同一夹取范围）。 */
function readStoredWidth(key: string, fallback: number, min: number, max: number): number {
  const v = Number.parseInt(localStorage.getItem(key) ?? "", 10);
  if (!Number.isFinite(v) || v <= 0) return fallback;
  return clampWidth(v, min, max);
}

/** 面板拖宽手柄：按住左右拖动，实时回调 clientX（App 据此算宽度）。 */
function ResizeHandle({ onDrag }: { onDrag: (clientX: number) => void }) {
  const onMouseDown = (e: ReactMouseEvent) => {
    e.preventDefault();
    const move = (ev: MouseEvent) => onDrag(ev.clientX);
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
  };
  return (
    <div
      onMouseDown={onMouseDown}
      title="拖动调整宽度"
      className="w-1 shrink-0 cursor-col-resize bg-transparent transition hover:bg-indigo-500/40"
    />
  );
}

/** 应用壳：向导模式 / agent 启动中 / 主界面三态。
 *
 * 会话转录与忙碌状态按 chat_id 提升到本层持有——切换页面 / 会话都不会丢失；
 * bus 事件按 chat_id 路由进对应会话的转录。 */
export default function App() {
  const [status, setStatus] = useState<Status | null>(null);
  const [agentState, setAgentState] = useState<"idle" | "starting" | "ready" | "error">("idle");
  const [agentError, setAgentError] = useState<string | null>(null);
  const [page, setPage] = useState<Page>("chat");

  // 标题栏循环标语（界面偏好，非 rclaw.toml）。加载失败或为空时回退应用名。
  const [slogans, setSlogans] = useState<string[]>([]);
  const [sloganInterval, setSloganInterval] = useState(5);
  const [sloganIdx, setSloganIdx] = useState(0);
  useEffect(() => {
    void api
      .slogansGet()
      .then((p) => {
        setSlogans(p.slogans);
        setSloganInterval(p.interval_secs);
      })
      .catch(() => {});
  }, []);
  // world.run 逐命令确认（[agent] workflow_command_confirm=true 时后端弹窗）。
  useEffect(() => {
    // cleanup 可能先于 listen 的 promise 完成（StrictMode 双挂载）：那时直接在 then 里解除，
    // 否则监听器泄漏、同一确认弹两次。
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<{ id: string; command: string; cwd: string }>(
      "workflow_command_confirm",
      (event) => {
        const allowed = window.confirm(
          `允许执行命令？
${event.payload.command}

工作目录：${event.payload.cwd}`,
        );
        api.workflowAnswerCommandConfirm(event.payload.id, allowed).catch(() => {});
      },
    ).then((fn) => {
      if (disposed) {
        fn();
      } else {
        unlisten = fn;
      }
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);
  useEffect(() => {
    if (slogans.length <= 1) return;
    const ms = Math.max(1, sloganInterval) * 1000;
    const timer = setInterval(() => setSloganIdx((i) => (i + 1) % slogans.length), ms);
    return () => clearInterval(timer);
  }, [slogans, sloganInterval]);
  // 标语轮播到原生窗口标题栏（「最上面」的 RClaw 处）。
  const sloganTitle = slogans.length ? slogans[sloganIdx % slogans.length] : "RClaw";
  // 移动端（<768px）：侧栏改为抽屉，任务面板/窗控隐藏；桌面布局零影响。
  const isMobile = useIsMobile();
  const [drawerOpen, setDrawerOpen] = useState(false);
  // 安卓 App：左缘右滑呼出抽屉 / 抽屉内左滑收起（与内部横向滚动互斥判定
  // 见 hook）；向导/启动页不启用。桌面版（含窄窗口）不挂任何监听。
  const showWizard = status?.wizard_needed ?? false;
  useEdgeSwipe({
    open: drawerOpen,
    onOpen: () => setDrawerOpen(true),
    onClose: () => setDrawerOpen(false),
    enabled: isAndroidDevice() && isMobile && !showWizard,
  });

  // 安卓返回键导航（原生侧 MainActivity 把返回键翻译成 __rclawBack 调用）：
  // 弹层开着 = 关一层；抽屉开 = 退出应用；否则 = 呼出侧边栏（「返回回到侧边
  // 栏，再按退出」）。向导/启动期不装载（走系统默认退出）。
  const drawerOpenRef = useRef(drawerOpen);
  drawerOpenRef.current = drawerOpen;
  const backNavArmed = isAndroidDevice() && isMobile && !!status && !status.wizard_needed;
  useEffect(() => {
    if (!backNavArmed) return;
    installBackNavigation(() => {
      if (drawerOpenRef.current) {
        void api.appExit().catch(() => {});
        return true;
      }
      setDrawerOpen(true);
      return true;
    });
  }, [backNavArmed]);
  useEffect(() => {
    void api.setWindowTitle(sloganTitle).catch(() => {});
  }, [sloganTitle]);

  // 界面主题：dark/light 之外新增四套带背景的风格（星空/森林 = 暗色系，
  // 樱花/奶油 = 浅色系，复用 theme-light 的文字覆盖）。存界面偏好；localStorage
  // 同步缓存，避免非默认主题用户每次启动先闪一屏深色。类名挂在 html 根。
  const [theme, setTheme] = useState<ThemeId>(() => {
    const saved = localStorage.getItem("rclaw_theme") as ThemeId | null;
    return saved && THEME_IDS.includes(saved) ? saved : "dark";
  });
  useEffect(() => {
    void api
      .themeGet()
      .then((t) => {
        if (THEME_IDS.includes(t as ThemeId)) {
          setTheme(t as ThemeId);
          localStorage.setItem("rclaw_theme", t);
        }
      })
      .catch(() => {});
  }, []);
  useEffect(() => {
    const root = document.documentElement.classList;
    for (const id of THEME_IDS) root.remove(`theme-${id}`);
    root.add(`theme-${theme}`);
    // 浅色系主题复用浅色调色板（文字/面板覆盖），背景由各自主题层接管。
    const base = THEMES.find((t) => t.id === theme)?.base ?? "dark";
    root.toggle("theme-light", base === "light");
  }, [theme]);
  const applyTheme = useCallback((next: ThemeId) => {
    setTheme(next);
    localStorage.setItem("rclaw_theme", next);
    void api.themeSet(next).catch((e) => setErrorToast(`主题保存失败: ${String(e)}`));
    setThemePickerOpen(false);
  }, []);
  const [themePickerOpen, setThemePickerOpen] = useState(false);
  const toggleTheme = useCallback(() => {
    setThemePickerOpen((v) => !v);
  }, []);

  // 侧栏收放（图标栏）与右侧任务面板开关：界面偏好，存 localStorage。
  const [sidebarCollapsed, setSidebarCollapsed] = useState(
    () => localStorage.getItem("rclaw_sidebar") === "collapsed",
  );
  const toggleSidebar = useCallback(() => {
    setSidebarCollapsed((v) => {
      localStorage.setItem("rclaw_sidebar", v ? "expanded" : "collapsed");
      return !v;
    });
  }, []);
  const [taskPanelOpen, setTaskPanelOpen] = useState(
    () => localStorage.getItem("rclaw_task_panel") !== "hidden",
  );
  const toggleTaskPanel = useCallback(() => {
    setTaskPanelOpen((v) => {
      localStorage.setItem("rclaw_task_panel", v ? "hidden" : "visible");
      return !v;
    });
  }, []);

  // 侧栏 / 右侧任务面板拖宽：界面偏好，存 localStorage（收起 / 关闭时手柄不渲染）。
  const [leftWidth, setLeftWidth] = useState(() =>
    readStoredWidth("rclaw_sidebar_width", 224, LEFT_WIDTH_MIN, LEFT_WIDTH_MAX),
  );
  const [rightWidth, setRightWidth] = useState(() =>
    readStoredWidth("rclaw_task_width", 288, RIGHT_WIDTH_MIN, RIGHT_WIDTH_MAX),
  );
  useEffect(() => {
    localStorage.setItem("rclaw_sidebar_width", String(leftWidth));
  }, [leftWidth]);
  useEffect(() => {
    localStorage.setItem("rclaw_task_width", String(rightWidth));
  }, [rightWidth]);
  const resizeLeft = useCallback(
    (clientX: number) => setLeftWidth(clampWidth(clientX, LEFT_WIDTH_MIN, LEFT_WIDTH_MAX)),
    [],
  );
  const resizeRight = useCallback(
    (clientX: number) =>
      setRightWidth(clampWidth(window.innerWidth - clientX, RIGHT_WIDTH_MIN, RIGHT_WIDTH_MAX)),
    [],
  );

  // 多会话状态（提升到 App：切页 / 切会话不丢历史）。
  const [chats, setChats] = useState<ChatMeta[]>([]);
  const [activeId, setActiveId] = useState<string>(DEFAULT_CHAT);
  const [entriesByChat, setEntriesByChat] = useState<Record<string, Entry[]>>({});
  const [busyByChat, setBusyByChat] = useState<Record<string, boolean>>({});
  // 事件回调（分叉等）只在点击时才需要最新的转录 / 忙碌态：经 ref 读取，不把这两个
  // 随每个流式 token 变化的对象放进 useCallback 依赖——否则回调身份每个 token 变一次，
  // Chat 里依赖它的 listItems useMemo 跟着整段重算。
  const entriesByChatRef = useRef(entriesByChat);
  entriesByChatRef.current = entriesByChat;
  const busyByChatRef = useRef(busyByChat);
  busyByChatRef.current = busyByChat;
  // 之后新到的思考块是否默认收起（按会话）：由用户最近一次手动折叠（true）/
  // 展开（false）决定，只作为新块的初始状态，不改动已在屏上的块。
  const [futureThinkingCollapsed, setFutureThinkingCollapsed] = useState<Record<string, boolean>>({});
  // 已从持久化历史恢复过转录的会话（避免重复拉取）。
  const restoredChats = useRef<Set<string>>(new Set());
  // 快照拉取进行中的会话：游标尚未落定，增量拉取必须等它完成（否则从 0 起步会把
  // 尾窗之外的历史整段拉进视图，破坏首屏有界加载）。
  const snapshotLoading = useRef<Set<string>>(new Set());
  // 行级游标（每会话）：已消费到哪条持久化消息行。快照落定 / 增量入账时推进；
  // turn 收尾与窗口恢复可见时按它续读增量（重连不重拉全量）。
  const cursorByChat = useRef<Record<string, number>>({});
  // 本地新建、后端尚未确认的会话 id（「+」路径）：其占位标题可被首条消息更新。
  const localChatIds = useRef<Set<string>>(new Set());

  const refreshChats = useCallback(async () => {
    try {
      const backend = await api.chatList();
      // 后端已确认的会话不再是本地占位（标题此后以服务端为准）。
      backend.forEach((c) => localChatIds.current.delete(c.chat_id));
      // 后端只列出已有消息记录的会话；本地刚新建、尚未发首条消息的会话还没有记录，
      // 直接整体替换会让空新会话从侧栏消失——保留仍是当前会话的本地未持久化项。
      setChats((prev) => {
        const ids = new Set(backend.map((c) => c.chat_id));
        const locals = prev.filter(
          (c) => !ids.has(c.chat_id) && !c.archived && c.chat_id === activeId,
        );
        return [...locals, ...backend].sort((a, b) => b.updated_at - a.updated_at);
      });
    } catch {
      // agent 未启动 / 首次运行时静默。
    }
  }, [activeId]);

  // 思考消息流式合并：快照（「💭 思考中」前缀，轮末整段权威内容）覆盖当前思考块；
  // 流式增量（「💭\n」前缀，载荷为原始分片）追加进当前思考块；当前块不存在
  // （最后一条不是思考）时新建。切会话 / 用户消息 / 工具行都会开启新块。
  const THINKING_SNAPSHOT_PREFIX = "💭 思考中";
  const THINKING_DELTA_PREFIX = "💭\n";
  const mergeThinking = useCallback((chatId: string, content: string) => {
    const isSnapshot = content.startsWith(THINKING_SNAPSHOT_PREFIX);
    const delta = isSnapshot ? null : content.slice(THINKING_DELTA_PREFIX.length);
    setEntriesByChat((prev) => {
      const existing = prev[chatId] ?? [];
      const last = existing[existing.length - 1];
      if (last?.role === "thinking") {
        const next = existing.slice();
        next[next.length - 1] = {
          ...last,
          content: delta === null ? content : last.content + delta,
        };
        return { ...prev, [chatId]: next };
      }
      const initial = delta === null ? content : `💭 思考中...\n${delta}`;
      return {
        ...prev,
        [chatId]: [
          ...existing,
          { id: next_entry_id++, ts: Date.now(), role: "thinking" as const, content: initial },
        ],
      };
    });
  }, []);

  // 把转录条目追加进指定会话（懒初始化空会话）。
  const appendEntries = useCallback(
    (chatId: string, items: Omit<Entry, "id">[]) => {
      setEntriesByChat((prev) => {
        const existing = prev[chatId] ?? [];
        const appended = items.map((item) => ({
          id: next_entry_id++,
          ts: Date.now(),
          ...item,
        }));
        return { ...prev, [chatId]: [...existing, ...appended] };
      });
    },
    [],
  );

  // 已落库条目并入会话视图（行级增量的装配步），两阶段保证幂等与顺序：
  // 1) 按 (行 id, 角色) 去重（同一持久化行可产出 thinking/status/text 多个条目）；
  //    再把同角色同文的【未盖章】直播条目一对一原位盖上行 id——顺序与对象身份都
  //    不动，fork 菜单持有的条目引用因此拿到锚点。思考条目按归一化文本匹配、且
  //    只在「owner user 到下一条 user」的窗口内找（纯文本轮的孤儿思考块永不落库，
  //    跨轮同文不能错盖）；匹配不上就追加，宁偶有重复不错盖。
  // 2) 没有直播孪生的条目按 (行 id, 同行角色序) 插入——乱序到达的中插行不会落到
  //    最终回复之后，同行内保持 thinking → status → assistant。
  const applyCommitted = useCallback((chatId: string, items: HistoryMsg[]) => {
    if (items.length === 0) return;
    // 条目在同行内的展示序：同一持久化行可产出 thinking/status/text 三种条目
    // （共享行 id），插入定位时按此序排同行先后。
    const roleRank = (role: Entry["role"]) =>
      role === "thinking" ? 0 : role === "status" ? 1 : role === "assistant" ? 2 : 0;
    setEntriesByChat((prev) => {
      const existing = prev[chatId] ?? [];
      const next = existing.slice();
      // 去重身份 = (行 id, 角色)：按行 id 去重会把同行的正文吞掉（thinking 入账后
      // 同 id 的 assistant 条目被跳过，正文丢失也拿不到分叉锚点）。
      const have = new Set(
        existing.flatMap((e) =>
          e.msgId === undefined ? [] : [`${e.msgId}:${e.role}`],
        ),
      );
      // 两阶段：先完成本批全部身份对位（盖章），再统一插入未匹配条目。单遍混合时
      // 「先到的 thinking 追加到尾部、后到的 assistant 原位盖章」会让同行顺序倒置
      // （thinking 排到正文后面）——对位完成后比较器才能看到同行已盖章条目。
      const ownerUserOf = (rowId: number) => {
        // 思考行必然晚于其 user 行落库：owner = 最后一条已盖章且行 id 更小的 user。
        // 只认已盖章的：未盖章 user 的行 id 未知，可能属于更晚的轮。
        for (let i = next.length - 1; i >= 0; i--) {
          const e = next[i];
          if (e.role === "user" && e.msgId !== undefined && e.msgId < rowId) return e.id;
        }
        return null;
      };
      const windowFor = (anchorId: number | null) => {
        // 窗口 = [owner user 之后, 下一条 user 之前)：双边界把跨轮同文的孤儿块和
        // 下一轮的直播块都排除在外。
        if (anchorId === null) return { from: 0, to: next.length };
        const at = next.findIndex((e) => e.id === anchorId);
        if (at < 0) return { from: 0, to: next.length };
        let to = next.length;
        for (let i = at + 1; i < next.length; i++) {
          if (next[i].role === "user") {
            to = i;
            break;
          }
        }
        return { from: at + 1, to };
      };
      // 阶段一：身份对位（盖章）。批内遇到 user 条目即以其为窗口锚（盖章→可用；
      // 追加→直播条目整体缺席，本段无从盖章）；批内还没遇到 user 时按行 id 回溯
      // 最近一条已盖章 user（思考行必然晚于其 user 行落库）。
      let ownerAnchorId: number | null = null;
      let anchorFromBatch = false;
      let anchorUsable = false;
      const unmatched: Array<{ m: HistoryMsg; rank: number }> = [];
      for (const m of items) {
        const key = `${m.id}:${m.role}`;
        if (have.has(key)) continue;
        let idx = -1;
        if (m.role === "thinking") {
          const anchor = anchorFromBatch ? ownerAnchorId : ownerUserOf(m.id);
          const usable = anchorFromBatch ? anchorUsable : anchor !== null;
          const { from, to } = usable ? windowFor(anchor) : { from: 0, to: 0 };
          if (from < to) {
            idx = next.findIndex(
              (e, i) =>
                i >= from &&
                i < to &&
                e.msgId === undefined &&
                !e.sendFailed &&
                e.role === "thinking" &&
                normalizeThinkingText(e.content) ===
                  normalizeThinkingText(m.content),
            );
          }
        } else {
          idx = next.findIndex(
            (e) =>
              e.msgId === undefined &&
              !e.sendFailed &&
              e.role === m.role &&
              e.content.trim() === m.content.trim(),
          );
        }
        if (idx >= 0) {
          next[idx] = { ...next[idx], msgId: m.id };
        }
        if (m.role === "user") {
          anchorFromBatch = true;
          anchorUsable = idx >= 0;
          ownerAnchorId = idx >= 0 ? next[idx].id : null;
        }
        if (idx < 0) {
          unmatched.push({ m, rank: roleRank(m.role) });
        }
        have.add(key);
      }
      // 阶段二：统一定位插入。按 (行 id, 同行角色序) 全序比较——乱序行落在第一个
      // 更大行 id 之前，同行内保持 thinking → status → assistant。
      for (const { m, rank } of unmatched) {
        const pos = next.findIndex(
          (e) =>
            e.msgId !== undefined &&
            (e.msgId > m.id || (e.msgId === m.id && roleRank(e.role) > rank)),
        );
        const entry: Entry = {
          id: next_entry_id++,
          ts: historyEntryTs(m.ts),
          role: m.role,
          content: m.content,
          msgId: m.id,
        };
        if (pos >= 0) next.splice(pos, 0, entry);
        else next.push(entry);
      }
      return { ...prev, [chatId]: next };
    });
  }, []);

  // 行级增量拉取：把游标之后落库的行补进视图并回填锚点。turn 收尾 / 窗口恢复可见 /
  // fork 前兜底共用。两个竞态都在这里收口：
  // - 快照还在装（snapshotLoading）：不是丢弃触发——快照读库之后落库的行要等它落定
  //   后靠这里的增量补上（snapshotReady 在 openChat 成功/失败时都会解除）。
  // - 同会话在飞期间再触发 ≠ 已覆盖：那次请求可能已读过库，新提交的行不在其响应里。
  //   记一笔尾随重跑，作业收尾前从最新游标再读一轮，直到没有尾随触发为止。
  const resyncInflight = useRef<Record<string, Promise<void>>>({});
  const resyncPending = useRef<Record<string, boolean>>({});
  const snapshotReady = useRef<Record<string, Promise<void>>>({});
  const resyncChat = useCallback(
    (chatId: string): Promise<void> => {
      const inflight = resyncInflight.current[chatId];
      if (inflight) {
        resyncPending.current[chatId] = true;
        return inflight;
      }
      const run = (async () => {
        for (;;) {
          resyncPending.current[chatId] = false;
          await snapshotReady.current[chatId];
          if (!restoredChats.current.has(chatId)) return;
          let after = cursorByChat.current[chatId] ?? 0;
          for (let page = 0; page < 20; page++) {
            let delta: ChatHistoryDelta;
            try {
              delta = await api.chatHistorySince(chatId, after);
            } catch {
              return; // agent 未启动等瞬时失败：下次收尾再补。
            }
            cursorByChat.current[chatId] = Math.max(after, delta.cursor);
            if (delta.entries.length > 0) applyCommitted(chatId, delta.entries);
            if (!delta.more) break;
            after = delta.cursor;
          }
          if (!resyncPending.current[chatId]) return;
        }
      })();
      resyncInflight.current[chatId] = run;
      void run.finally(() => {
        delete resyncInflight.current[chatId];
        resyncPending.current[chatId] = false;
      });
      return run;
    },
    [applyCommitted],
  );

  // 由 get_status 结果推导界面状态（挂载查询与「启动中」轮询共用同一份逻辑）。
  const applyStatus = useCallback((s: Status) => {
    setStatus(s);
    setAgentState(
      s.agent_ready ? "ready" : s.wizard_needed ? "idle" : s.agent_error ? "error" : "starting",
    );
    if (s.agent_error) setAgentError(s.agent_error);
  }, []);

  useEffect(() => {
    void api.getStatus().then(applyStatus);
    const unAgent = api.onAgent((e: AgentEvent) => {
      setAgentState(e.state);
      if (e.error) setAgentError(e.error);
      if (e.state === "ready") {
        void api.getStatus().then(setStatus);
        void refreshChats();
      }
    });
    // bus 事件按 chat_id 路由；思考消息走流式合并（快照覆盖 / 增量追加），
    // 正式回复解除忙碌态。「🟢 Agent工作中...」不进转录——忙碌状态由侧栏指示点
    // 与消息流底部的执行中齿轮行承担。
    const unBus = api.onBus((payload) => {
      if (payload.content === "🟢 Agent工作中...") {
        return;
      }
      const thinking = isThinkingLine(payload.content);
      if (thinking) {
        mergeThinking(payload.chat_id, payload.content);
        return;
      }
      const statusEntry = isStatusLine(payload.content);
      appendEntries(payload.chat_id, [
        {
          role: statusEntry ? "status" : "assistant",
          content: payload.content,
        },
      ]);
      // 仅 turn 的最终回复 / 收尾确认（非 intermediate 的普通消息）解除忙碌态：
      // 工具轮之间的过程文本（intermediate）不再误判为收尾——修复「调用 skill
      // 期间侧栏绿点、齿轮行消失」的问题。
      if (!statusEntry && !payload.intermediate) {
        setBusyByChat((prev) => ({ ...prev, [payload.chat_id]: false }));
        // turn 收尾时会话记录已落库（先 append 后 outbound）：按游标增量拉一轮，
        // 给本轮直播条目回填持久化行 id（fork 锚点）；错过的事件也在此时自愈。
        void resyncChat(payload.chat_id);
        // 刷新列表，让新会话（bot 收件箱、首次对话）与派生标题及时出现在侧栏。
        void refreshChats();
      }
    });
    return () => {
      unAgent.then((fn) => fn());
      unBus.then((fn) => fn());
    };
  }, [appendEntries, mergeThinking, refreshChats, applyStatus, resyncChat]);

  // 窗口重新可见（移动端切回 / 桌面最小化恢复）时给当前会话增量补一轮：隐藏期间
  // 收尾的 turn、后台会话的 outbound 事件可能被系统丢弃，按游标续读自愈。
  useEffect(() => {
    const onVisible = () => {
      if (document.visibilityState === "visible") void resyncChat(activeId);
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => document.removeEventListener("visibilitychange", onVisible);
  }, [resyncChat, activeId]);

  // 「启动中」轮询兜底：agent ready/error 事件可能在事件监听器注册完成前发出
  // （首启动 webview 冷启动时窗口尤其窄），错过事件会永远停在启动动画。轮询
  // get_status 直到状态翻转（ready / error / 向导），由 agentState 变化自动停表。
  useEffect(() => {
    if (agentState !== "starting") return;
    const timer = setInterval(() => {
      void api.getStatus().then(applyStatus);
    }, 800);
    return () => clearInterval(timer);
  }, [agentState, applyStatus]);

  // 打开会话：内存没有转录时从持久化历史恢复（重启后会话历史仍在）。
  const openChat = useCallback(
    async (chatId: string) => {
      setActiveId(chatId);
      setPage("chat");
      setDrawerOpen(false);
      // 会话边界清草稿（入口同步清，不依赖 Chat 挂载后的 effect 时序——
      // 分享确认等"先切会话再并入草稿"的路径会被后跑的清空 effect 抵消）。
      setChatDraft("");
      if (restoredChats.current.has(chatId) || snapshotLoading.current.has(chatId)) return;
      snapshotLoading.current.add(chatId);
      // 快照就绪闸门：装完（成功或失败都算解除）前到达的增量拉取在这里等，
      // 落定后从最新游标补读快照读库与安装之间落库的行。
      let resolveReady!: () => void;
      snapshotReady.current[chatId] = new Promise((resolve) => {
        resolveReady = resolve;
      });
      setEntriesByChat((prev) => ({ ...prev, [chatId]: prev[chatId] ?? [] }));
      try {
        // 快照装配：已落库条目是权威投影，整体重建；此后视图 = 快照 + 按游标续读
        // 的增量（resyncChat），不存在「数第 N 次出现」式的文本对位。
        const history = await api.chatHistory(chatId);
        cursorByChat.current[chatId] = history.cursor;
        snapshotLoading.current.delete(chatId);
        restoredChats.current.add(chatId);
        resolveReady();
        setEntriesByChat((prev) => {
          const committed: Entry[] = history.entries.map((m) => ({
            id: next_entry_id++,
            role: m.role,
            content: m.content,
            msgId: m.id,
            ts: historyEntryTs(m.ts),
          }));
          // 事件可能先于快照返回：每条快照条目一对一消费一条同角色同文的直播孪生
          // （成员资格判断会误伤重复文本——快照之后新发的乐观「继续」、永远无法从
          // 库里恢复的发送失败条目都会被整批错杀）。两边都按时间序，用前进指针从
          // 上次消费位继续找：跨轮同文的思考块不会被更早的孤儿直播块顶替消费。
          // 思考块比较走归一化（直播侧带「💭 思考中...」前缀/流式差异）。没被消费
          // 的直播条目与思考块保留在尾部，等增量入账盖章。
          const live = [...(prev[chatId] ?? [])];
          let searchFrom = 0;
          for (const c of committed) {
            const rel = live.findIndex(
              (e, i) =>
                i >= searchFrom &&
                !e.sendFailed &&
                e.role === c.role &&
                (c.role === "thinking"
                  ? normalizeThinkingText(e.content) === normalizeThinkingText(c.content)
                  : e.content.trim() === c.content.trim()),
            );
            if (rel >= 0) {
              live.splice(rel, 1);
              searchFrom = rel;
            }
          }
          return { ...prev, [chatId]: [...committed, ...live] };
        });
      } catch {
        // agent 未启动等瞬时失败：回滚加载标记，下次打开还能重试；
        // 否则该会话永远保持空转录。
        snapshotLoading.current.delete(chatId);
        resolveReady();
      }
    },
    [],
  );

  // agent 就绪后拉取会话列表，并定位最近一个会话。
  useEffect(() => {
    if (agentState !== "ready") return;
    void refreshChats().then(() => {
      setChats((list) => {
        if (!list.some((c) => c.chat_id === activeId) && list.length > 0) {
          void openChat(list[0].chat_id);
        } else if (!restoredChats.current.has(activeId)) {
          // 初始定位到最近会话；列表为空（全新库）也要初始化默认会话——
          // 否则它的首批消息永远拿不到游标与 fork 锚点。
          void openChat(activeId);
        }
        return list;
      });
    });
    // 仅在就绪时执行一次。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentState]);

  const busyFor = (chatId: string) => busyByChat[chatId] ?? false;

  /** 向指定会话发送消息（排队投递也走它——收尾时用户可能在别的会话/页面）。 */
  const sendTo = useCallback(
    (chatId: string, content: string) => {
      const userEntry: Entry = { id: next_entry_id++, role: "user", content };
      appendEntries(chatId, [userEntry]);
      setBusyByChat((prev) => ({ ...prev, [chatId]: true }));
      // 首条消息发出时后端会话记录尚未落库，chat_list 扫不到：先本地补一个
      // 会话条目让侧栏立即显示（否则整个 turn 期间是「暂无会话」，直到收尾
      // 刷新才出现）。「+」新建的本地占位会话（标题还是「新对话」）则用首条
      // 消息更新缩略标题；已确认的会话条目不动。
      setChats((prev) => {
        if (!prev.some((c) => c.chat_id === chatId)) {
          return [
            {
              chat_id: chatId,
              title: deriveLocalTitle(content),
              updated_at: Date.now(),
              archived: false,
            },
            ...prev,
          ];
        }
        if (!localChatIds.current.has(chatId)) {
          return prev;
        }
        // 仅更新仍是「新对话」占位标题的条目：用户已重命名的不覆盖，
        // 后续消息也不改写首条消息派生的标题（与后端「首条消息派生」同规则）。
        return prev.map((c) =>
          c.chat_id === chatId && c.title === "新对话"
            ? { ...c, title: deriveLocalTitle(content), updated_at: Date.now() }
            : c,
        );
      });
      void api.send(chatId, content).catch((e) => {
        appendEntries(chatId, [{ role: "assistant", content: `发送失败：${String(e)}` }]);
        // 标记这条用户消息发送失败（未落库）：fork 锚点解析不得把它配到前面同文
        // 的旧消息上——空闲也不证明它已持久化。
        setEntriesByChat((prev) => ({
          ...prev,
          [chatId]: (prev[chatId] ?? []).map((existing) =>
            existing.id === userEntry.id ? { ...existing, sendFailed: true } : existing,
          ),
        }));
        setBusyByChat((prev) => ({ ...prev, [chatId]: false }));
      });
      void refreshChats();
    },
    [appendEntries, refreshChats],
  );

  const handleSend = useCallback(
    (content: string) => sendTo(activeId, content),
    [activeId, sendTo],
  );

  // ---- 排队消息（第十二轮 · zcode 同款）----
  // 队列按会话分桶放在 App：Chat 随页面切换（工作区/设置面板）会整体卸载，组件内
  // state 会把排着队的文本一起丢掉；投递由下面的收尾监听统一驱动——某会话空闲且
  // 队非空就出队一条发送，人在哪个页面/会话都不影响。
  const [queueByChat, setQueueByChat] = useState<Record<string, QueueItem[]>>({});
  const queueIdRef = useRef(0);
  const enqueueQueued = useCallback((chatId: string, text: string) => {
    queueIdRef.current += 1;
    const item = { id: queueIdRef.current, text };
    setQueueByChat((prev) => ({ ...prev, [chatId]: [...(prev[chatId] ?? []), item] }));
  }, []);
  const removeQueued = useCallback((chatId: string, id: number) => {
    setQueueByChat((prev) => ({
      ...prev,
      [chatId]: (prev[chatId] ?? []).filter((queued) => queued.id !== id),
    }));
  }, []);
  // 逐条投递：一次 effect 只出队一条（发送把该会话 busy 翻回 true，下一轮收尾再发
  // 下一条）——一次全发会让后续消息变成运行中 turn 的插队，排队语义就没了。
  useEffect(() => {
    for (const [chatId, items] of Object.entries(queueByChat)) {
      const next = items[0];
      if (!next || busyByChat[chatId]) continue;
      removeQueued(chatId, next.id);
      sendTo(chatId, next.text);
      break;
    }
  }, [queueByChat, busyByChat, removeQueued, sendTo]);

  const handleCancel = useCallback(() => {
    void api.cancel(activeId).catch(() => {
      // 取消确认会以 bus 消息回来；这里静默。
    });
  }, [activeId]);

  // ---- PlanRun 流程步骤 / todo（第十二轮 · zcode 同款）----
  // plan_status 轮询：有未收尾计划或任一会话忙碌时 2.5s 高频，否则 12s 低频
  // （计划事件量小，全量投影每次都读；收尾后卡片默认收起只留标题）。
  const [plans, setPlans] = useState<PlanStatus[]>([]);
  const latestPlan = plans[0] ?? null;
  // 高频条件：存在计划且最新未收尾，或任一会话忙碌（PlanRun 通常在忙碌期推进）。
  // 从未用过 PlanRun 的部署保持 12s 低频空轮询（首次 tick 立即执行补状态）。
  const planNeedsFresh = plans.length > 0 && !latestPlan?.completed;
  const anyBusy = Object.values(busyByChat).some(Boolean);
  useEffect(() => {
    if (agentState !== "ready") return;
    let stopped = false;
    const tick = async () => {
      try {
        const next = await api.planStatus();
        if (!stopped) setPlans(next);
      } catch {
        // agent 未启动等瞬时失败：静默（下次轮询再试）。
      }
    };
    void tick();
    const interval = planNeedsFresh || anyBusy ? 2500 : 12000;
    const timer = setInterval(tick, interval);
    return () => {
      stopped = true;
      clearInterval(timer);
    };
  }, [agentState, planNeedsFresh, anyBusy]);

  // 新建会话的公共部分：重置草稿/转录/占位标记并切换到对话页。是否在侧栏
  // 列表补条目由调用方决定（「＋」新建要补；删空后的裸工作会话不补）。
  const prepareChat = useCallback((id: string) => {
    setChatDraft("");
    restoredChats.current.add(id);
    localChatIds.current.add(id);
    setEntriesByChat((prev) => ({ ...prev, [id]: [] }));
    setBusyByChat((prev) => ({ ...prev, [id]: false }));
    setActiveId(id);
    setPage("chat");
  }, []);

  const newChat = useCallback(() => {
    const id = freshChatId();
    prepareChat(id);
    setChats((prev) => [
      { chat_id: id, title: "新对话", updated_at: Date.now(), archived: false },
      ...prev.filter((c) => c.chat_id !== id),
    ]);
  }, [prepareChat]);

  const archiveChat = useCallback(
    async (chatId: string) => {
      // 正在工作的会话禁止归档：turn 收尾还会向原路径追加（文件会被重建），先停止再归档。
      if (busyByChat[chatId]) {
        setErrorToast("会话正在工作中，请先停止任务再归档");
        return;
      }
      try {
        await api.chatArchive(chatId);
      } catch (e) {
        setErrorToast(String(e));
        return;
      }
      await refreshChats();
      if (chatId === activeId) {
        setChats((list) => {
          const next = list.find((c) => !c.archived);
          if (next) {
            void openChat(next.chat_id);
          } else {
            newChat();
          }
          return list;
        });
      }
    },
    [activeId, busyByChat, newChat, openChat, refreshChats],
  );

  const restoreChat = useCallback(
    async (chatId: string) => {
      try {
        await api.chatRestore(chatId);
      } catch (e) {
        setErrorToast(String(e));
        return;
      }
      await refreshChats();
    },
    [refreshChats],
  );

  // 删除会话（不可恢复，入口先弹确认）：正在工作的会话禁止删除（turn 收尾会
  // 向原路径追加，文件会被重建）。本地转录与恢复标记一并清掉，防止同 id 会话
  // 重开时拼回已删除的历史。
  const deleteChat = useCallback(
    async (chatId: string) => {
      if (busyByChat[chatId]) {
        setErrorToast("会话正在工作中，请先停止任务再删除");
        return;
      }
      try {
        await api.chatDelete(chatId);
      } catch (e) {
        setErrorToast(String(e));
        return;
      }
      restoredChats.current.delete(chatId);
      snapshotLoading.current.delete(chatId);
      delete cursorByChat.current[chatId];
      localChatIds.current.delete(chatId);
      setEntriesByChat((prev) => {
        const next = { ...prev };
        delete next[chatId];
        return next;
      });
      // 先把被删会话从列表摘除再刷新/切换：refreshChats 会保留「仍是当前会话
      // 的本地未持久化条目」，若带着被删的 activeId 去刷新，它会以幽灵条目
      // 复活（表现为「删掉最后一个会话后侧栏出现两条、永远删不空」）。
      setChats((prev) => prev.filter((c) => c.chat_id !== chatId));
      if (chatId === activeId) {
        const next = chats.find((c) => !c.archived && c.chat_id !== chatId);
        if (next) {
          void openChat(next.chat_id);
        } else {
          // 全部删空：不再自动补「新对话」条目，侧栏显示「暂无会话」；主区挂
          // 一个不进列表的空白工作会话，发出首条消息时 handleSend 会补进列表。
          prepareChat(freshChatId());
        }
      }
      await refreshChats();
    },
    [activeId, busyByChat, chats, openChat, prepareChat, refreshChats],
  );

  // 删除确认弹窗载荷（WebView 无原生 confirm，不可恢复操作走应用内弹窗）。
  const [deleteConfirm, setDeleteConfirm] = useState<{
    chatId: string;
    title: string;
  } | null>(null);

  const [toast, setToast] = useState<string | null>(null);
  const toastTimer = useRef<number | undefined>(undefined);
  // 身份保持稳定（useCallback）：它作为 onError 传给 Chat 等面板，Chat 用它
  // 作消息列表 memo 的依赖——身份抖动会让 memo 失效（App 每 5s 刷新用量快照
  // 都会触发重渲，进而让整个消息流反复重挂 markdown 解析）。
  const setErrorToast = useCallback((msg: string) => {
    setToast(msg);
    // 连续报错时重置计时，避免上一条的定时器提前熄灭这一条。
    window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToast(null), 4000);
  }, []);

  // 暂停 / 继续计划（步骤卡片 ⏸/▶ 按钮）：写标志与事件后立即本地翻转投影
  // （2.5s 轮询会带回权威状态），失败回滚并提示。
  const handlePlanPause = useCallback(
    (planId: string, paused: boolean) => {
      setPlans((prev) =>
        prev.map((p) => (p.plan_id === planId ? { ...p, paused } : p)),
      );
      api.planPause(planId, paused).catch((e) => {
        setPlans((prev) =>
          prev.map((p) => (p.plan_id === planId ? { ...p, paused: !paused } : p)),
        );
        setErrorToast(`暂停/继续失败: ${String(e)}`);
      });
    },
    [setErrorToast],
  );

  // 消息「分叉到新会话」（ZCode fork 同款，第十二轮）：解析该消息的持久化行 id，
  // fork 后切到新会话。锚点一律是权威行 id：恢复条目自带，直播条目由 turn 收尾的
  // 增量回填原位盖章——没有行 id 就说明它还没落库，不做任何按内容反查的猜测。
  const handleFork = useCallback(
    async (entry: Entry) => {
      // 发送失败的条目没有落库，永远拿不到行 id——直接拒绝。
      if (entry.sendFailed) {
        setErrorToast("该消息发送失败、未落库，无法作为分叉锚点");
        return;
      }
      let anchorId = entry.msgId ?? null;
      if (anchorId === null) {
        // 直播条目还没有持久化行 id。忙碌期该消息确实尚未落库（运行时按会话串行，
        // 排队中的消息没写进库）；空闲期则多半是收尾的增量回填还没跑完——补拉一次，
        // 同一 Entry 对象被原位盖章（对象身份不变），按 Entry.id 重新查表拿锚点。
        if (busyByChatRef.current[activeId]) {
          setErrorToast(
            "会话运行中，该消息可能尚未落库：请等任务收尾后再分叉，或用 /fork 查看已落库的锚点",
          );
          return;
        }
        await resyncChat(activeId);
        // applyCommitted 只是把盖章排进了 state 队列；并发渲染下 await 返回不代表
        // 已提交。强制冲刷一轮再读 ref，否则第一次 fork 会拿到旧的未盖章条目、
        // 误报「没有持久化记录」。
        flushSync(() => {});
        anchorId =
          entriesByChatRef.current[activeId]?.find((e) => e.id === entry.id)?.msgId ?? null;
      }
      if (anchorId === null) {
        setErrorToast("未找到该消息的持久化记录（可能尚未落库），请稍后重试");
        return;
      }
      try {
        // 统一分叉语义：复制可见历史 + 自动回滚锚点之后的文件改动（runtime 侧持
        // 静默门，任务运行中会拒绝；无文件改动是正常路径）。
        const fork = await api.chatFork(activeId, anchorId);
        await refreshChats();
        setErrorToast(
          `✅ 已分叉出新会话（复制 ${fork.copied} 条历史${
            fork.restore_failed
              ? "，⚠️ 文件回滚未成功"
              : fork.restored_files > 0
                ? `，回滚 ${fork.restored_files} 个文件`
                : "，无可回滚的文件改动"
          }）`,
        );
        void openChat(fork.chat_id);
      } catch (e) {
        setErrorToast(`分叉失败: ${String(e)}`);
      }
    },
    [activeId, openChat, refreshChats, setErrorToast, resyncChat],
  );

  // 工作区外访问授权（默认权限下工具触达工作区之外时后端推送）：
  // 请求排队逐个弹窗，允许 / 拒绝后回填后端，超时由后端按拒绝兜底。
  const [permQueue, setPermQueue] = useState<PermissionRequest[]>([]);
  useEffect(() => {
    const un = api.onPermissionRequest((req) =>
      setPermQueue((q) => [...q, req]),
    );
    return () => {
      un.then((fn) => fn());
    };
  }, []);
  const answerPermission = useCallback(
    (allowed: boolean) => {
      const head = permQueue[0];
      if (!head) return;
      setPermQueue((q) => q.slice(1));
      // 副作用放在 updater 之外：StrictMode 下 updater 可能双调，回填必须只发一次。
      void api.permissionRespond(head.id, allowed).catch(() => {});
    },
    [permQueue],
  );

  // 状态条数据目录：单击复制路径、双击在文件管理器打开。单击先挂 400ms 定时器，
  // 期间收到 onDoubleClick 即取消复制（避免双击被当成两次单击先误复制）。
  // 400ms：贴近 Windows 默认双击间隔（500ms）但不拖太长，慢双击多数落在窗口内。
  const [homeHint, setHomeHint] = useState<string | null>(null);
  const homeClickTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const homeHintTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const flashHomeHint = (text: string) => {
    setHomeHint(text);
    if (homeHintTimer.current) clearTimeout(homeHintTimer.current);
    homeHintTimer.current = setTimeout(() => setHomeHint(null), 1500);
  };
  const handleHomeClick = () => {
    if (homeClickTimer.current) return; // 双击的第一下，交给 handleHomeDoubleClick
    homeClickTimer.current = setTimeout(() => {
      homeClickTimer.current = null;
      api
        .copyHomePath()
        .then(() => flashHomeHint("✓ 路径已复制"))
        .catch((e) => setErrorToast(String(e)));
    }, 400);
  };
  const handleHomeDoubleClick = () => {
    if (homeClickTimer.current) {
      clearTimeout(homeClickTimer.current);
      homeClickTimer.current = null;
    }
    api.openHomeDir().catch((e) => setErrorToast(String(e)));
  };

  // 会话重命名（双击 / 右键菜单的「重命名」触发行内编辑）：Enter 或失焦提交，Esc 取消；
  // 空标题提交 = 恢复自动派生标题（首条用户消息截断）。
  // cancelRef 挡住 Esc 卸载 input 时误触发的 onBlur 提交（blur 闭包里的状态是旧的）。
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  const renameCancelled = useRef(false);
  const startRename = useCallback((chatId: string, current: string) => {
    renameCancelled.current = false;
    setRenamingId(chatId);
    setRenameDraft(current);
  }, []);
  const commitRename = useCallback(async () => {
    if (renameCancelled.current) {
      renameCancelled.current = false;
      return;
    }
    const chatId = renamingId;
    if (!chatId) return;
    const title = renameDraft.trim().slice(0, 120);
    setRenamingId(null);
    // 乐观更新本地标题，失败时经 refreshChats 以服务端为准纠正。
    setChats((prev) =>
      prev.map((c) => (c.chat_id === chatId ? { ...c, title: title || c.title } : c)),
    );
    try {
      await api.chatRename(chatId, title);
    } catch (e) {
      setErrorToast(String(e));
    }
    void refreshChats();
  }, [renamingId, renameDraft, refreshChats]);

  // 会话右键菜单：记录目标会话与弹出行位置。
  const [chatMenu, setChatMenu] = useState<{
    chatId: string;
    title: string;
    x: number;
    y: number;
  } | null>(null);
  // 菜单外按下或 Esc 关闭；菜单内点击交给菜单项自己的 onClick。
  // 在会话行上右键时不关闭——行处理器会把菜单移到新位置。
  useEffect(() => {
    if (!chatMenu) return;
    const onDown = (e: MouseEvent) => {
      const t = e.target as Element | null;
      if (t?.closest?.("[data-chat-menu]")) return;
      setChatMenu(null);
    };
    const onCtx = (e: MouseEvent) => {
      const t = e.target as Element | null;
      if (t?.closest?.("[data-chat-menu]") || t?.closest?.("[data-chat-row]")) return;
      setChatMenu(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setChatMenu(null);
    };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("contextmenu", onCtx);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("contextmenu", onCtx);
      window.removeEventListener("keydown", onKey);
    };
  }, [chatMenu]);

  // 导出完整聊天记录：后端返回不裁剪的 Markdown，经 Blob 触发 .md 下载。
  const exportChat = useCallback(async (chatId: string, title: string) => {
    try {
      const md = await api.chatExport(chatId);
      const blob = new Blob([md], { type: "text/markdown;charset=utf-8" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `${(title || chatId).replace(/[\\/:*?"<>|]/g, "_")}.md`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      setErrorToast(String(e));
    }
  }, []);

  // 移动端抽屉的上下文/内存状态（桌面由右侧 TaskPanel 承担，互不影响）。
  // 任务概览不内嵌抽屉（具体任务状态展示不下），改为侧边栏同级入口 +
  // 专门详情页（TaskOverviewPanel）。
  const [mobileUsage, setMobileUsage] = useState<UsageSnapshot | null>(null);
  // 会话输入草稿（App 持有：Chat 随页面切换卸载重挂载时未发送内容不丢，
  // 分享/采集预填也直接写入这里）。
  const [chatDraft, setChatDraft] = useState("");
  // 系统分享待消费内容（选择目标期间的暂存，确认后并入 chatDraft）。
  const [shareDraft, setShareDraft] = useState<string | null>(null);
  const shareDraftRef = useRef<string | null>(null);
  useEffect(() => {
    shareDraftRef.current = shareDraft;
  }, [shareDraft]);
  const [shareChooserOpen, setShareChooserOpen] = useState(false);
  useEffect(() => {
    if (!isMobile || agentState !== "ready") return;
    let stopped = false;
    const tickUsage = async () => {
      try {
        const snapshot = await api.usageSnapshot(activeId);
        if (!stopped) setMobileUsage(snapshot);
      } catch {
        // agent 未启动等瞬时错误：保留上次快照。
      }
    };
    void tickUsage();
    const usageTimer = setInterval(tickUsage, 5000);
    return () => {
      stopped = true;
      clearInterval(usageTimer);
    };
  }, [isMobile, agentState, activeId]);

  // 系统分享消费：其他应用分享进来的文件已由 Java 侧落盘 uploads/。
  // 轮询消费（应用运行中分享经 onNewIntent 也会写入）；消费后弹目标选择：
  // 多会话选一个，或发到新会话（用户要求）。
  useEffect(() => {
    if (agentState !== "ready") return;
    const check = async () => {
      const share = await api.consumePendingShare().catch(() => null);
      if (!share) return;
      const lines = share.files.map(
        (f) => `[附件] ${f}（已放入 agent 工作区）`,
      );
      const draft = [...lines, share.text ?? ""].filter(Boolean).join("\n");
      if (!draft) return;
      // 未消费的旧草稿（选择框还开着）合并而非覆盖：附件引用不丢。
      setShareDraft((prev) => (prev ? prev + "\n" + draft : draft));
      setShareChooserOpen(true);
      setPage("chat");
    };
    void check();
    const timer = setInterval(check, 5000);
    return () => clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentState]);

  const placeShare = useCallback(
    (target: "new" | string) => {
      setShareChooserOpen(false);
      const text = shareDraftRef.current;
      setShareDraft(null);
      // 先切会话（入口会清空草稿），再并入分享文本：同一事件批处理中
      // 函数式更新按序应用，并入最终生效。
      if (target === "new") {
        newChat();
      } else {
        void openChat(target);
      }
      if (text) setChatDraft((prev) => (prev ? prev + "\n" + text : text));
    },
    [newChat, openChat],
  );

  // 消息选择「发送到会话」：与系统分享同款目标选择器；选中后把打包文本并入
  // 目标会话草稿（可检查/编辑后发送，不直接触发 turn）。
  const forwardDraftRef = useRef<string | null>(null);
  const [forwardChooserOpen, setForwardChooserOpen] = useState(false);
  const forwardToChat = useCallback((text: string) => {
    forwardDraftRef.current = text;
    setForwardChooserOpen(true);
  }, []);
  const placeForward = useCallback(
    (target: "new" | string) => {
      setForwardChooserOpen(false);
      const text = forwardDraftRef.current;
      forwardDraftRef.current = null;
      if (target === "new") {
        newChat();
      } else {
        void openChat(target);
      }
      if (text) setChatDraft((prev) => (prev ? prev + "\n" + text : text));
    },
    [newChat, openChat],
  );

  // ---------------------------------------------------------------------------
  // 会话采集（拍照/拍视频/录音 → 编辑/三选 → 草稿预填）与 whisper 转写。
  // 采集期间轮询 capture_result.json（相机返回后 Java 落盘），结果分发：
  // photo → 编辑页；video/audio → 三选对话框；cancelled → 静默复位。
  // ---------------------------------------------------------------------------
  const [captureInFlight, setCaptureInFlight] = useState(false);
  const [photoEditor, setPhotoEditor] = useState<{ dataUrl: string; rel: string } | null>(null);
  const [captureDialog, setCaptureDialog] = useState<{
    kind: "video" | "audio";
    rel: string;
  } | null>(null);
  const [recording, setRecording] = useState<{ startedAt: number } | null>(null);
  const [recordingSeconds, setRecordingSeconds] = useState(0);
  const capturePollRef = useRef<number | null>(null);

  useEffect(() => {
    if (!recording) return;
    setRecordingSeconds(0);
    const timer = setInterval(() => {
      setRecordingSeconds((s) => s + 1);
    }, 1000);
    return () => clearInterval(timer);
  }, [recording]);

  const stopCapturePolling = useCallback(() => {
    if (capturePollRef.current !== null) {
      clearInterval(capturePollRef.current);
      capturePollRef.current = null;
    }
  }, []);

  // captureInFlight 的同步镜像（连点守卫读它，避免 setState 异步窗口）。
  const captureInFlightRef = useRef(false);
  useEffect(() => {
    captureInFlightRef.current = captureInFlight;
  }, [captureInFlight]);

  const startCapture = useCallback(
    async (kind: "photo" | "video") => {
      // 连点守卫用同步 ref：setState 是异步的且 useCallback 闭包捕获的是
      // 渲染时的旧值，两次快速点击都会读到 false 直接放行。
      if (captureInFlightRef.current) return;
      captureInFlightRef.current = true;
      setCaptureInFlight(true);
      try {
        const status = await api.captureStart(kind);
        if (status.startsWith("error:")) {
          setErrorToast(status.slice(6));
          captureInFlightRef.current = false;
          setCaptureInFlight(false);
          return;
        }
        // "ok" 或 "permission_requested"（系统弹窗授权后原生侧自动续跑本轮采集，
        // 结果都会写 capture_result.json，所以两种都要启动轮询）。
        if (status === "permission_requested") {
          setErrorToast("请在系统弹窗中允许相机权限");
        }
        stopCapturePolling();
        // 视频拍摄常态超 90s，且相机在前台会冻结 WebView 定时器——deadline
        // 只作为"原生永远不产结果"（拒绝权限等）的兜底，取宽 10 分钟。
        const deadline = Date.now() + 600_000;
        const dispatchResult = (result: { kind: string; file: string | null } | null) => {
          if (!result?.file) return; // 用户在系统相机里放弃了
          if (result.kind === "photo") {
            void api
              .mediaBase64(result.file)
              .then((payload) => {
                setPhotoEditor({
                  dataUrl: `data:${payload.mime};base64,${payload.base64}`,
                  rel: result.file as string,
                });
              })
              .catch((e) => {
                // 拍照读取失败不能静默：用户拍了照却什么都看不到。
                setErrorToast(`读取拍摄结果失败: ${String(e)}`);
              });
          } else {
            setCaptureDialog({ kind: "video", rel: result.file });
          }
        };
        capturePollRef.current = window.setInterval(() => {
          if (Date.now() > deadline) {
            // 超时收摊前先消费一次：定时器冻结会让超时判断先于结果落盘执行。
            stopCapturePolling();
            setCaptureInFlight(false);
            void api
              .consumeCaptureResult()
              .then(dispatchResult)
              .catch(() => {});
            return;
          }
          void api
            .consumeCaptureResult()
            .then((result) => {
              if (!result) return;
              stopCapturePolling();
              setCaptureInFlight(false);
              dispatchResult(result);
            })
            .catch(() => {});
        }, 1500);
      } catch (e) {
        setErrorToast(String(e));
        captureInFlightRef.current = false;
        setCaptureInFlight(false);
      }
    },
    [stopCapturePolling, setErrorToast],
  );

  const startRecording = useCallback(async () => {
    try {
      const status = await api.recordStart();
      if (status === "ok") {
        setRecording({ startedAt: Date.now() });
      } else if (status === "permission_requested") {
        setErrorToast("请在系统弹窗中允许麦克风权限");
        // 授权后原生侧自动开录：轮询感知，亮出录音条（30s 内未开录则放弃）。
        const deadline = Date.now() + 30_000;
        const poll = window.setInterval(() => {
          void api
            .recordActive()
            .then((active) => {
              if (active) {
                window.clearInterval(poll);
                setRecording({ startedAt: Date.now() });
              }
            })
            .catch(() => {});
          if (Date.now() > deadline) window.clearInterval(poll);
        }, 800);
      } else {
        setErrorToast(status.startsWith("error:") ? status.slice(6) : status);
      }
    } catch (e) {
      setErrorToast(String(e));
    }
  }, [setErrorToast]);

  const stopRecording = useCallback(async () => {
    try {
      const rel = await api.recordStop();
      setRecording(null);
      if (rel) setCaptureDialog({ kind: "audio", rel });
      else setErrorToast("录音失败或时长过短");
    } catch (e) {
      setRecording(null);
      setErrorToast(String(e));
    }
  }, [setErrorToast]);

  const cancelRecording = useCallback(async () => {
    try {
      await api.recordCancel();
    } catch {
      // 尽力而为
    }
    setRecording(null);
  }, []);

  /** 采集/编辑产物 → 并入当前会话草稿（App 持有，切页不丢）。 */
  const prefillCapture = useCallback((draft: string) => {
    setCaptureDialog(null);
    setPhotoEditor(null);
    setChatDraft((prev) => (prev ? prev + "\n" + draft : draft));
    setPage("chat");
  }, []);

  /** 采集产物保存到系统相册（编辑页「保存到本地」）。 */
  const saveCaptureLocal = useCallback(
    (rel: string, kind: "photo" | "video" | "audio") => {
      void api
        .saveToGallery(rel, kind)
        .then((result) => {
          if (result === "permission_requested") {
            setErrorToast("请在系统弹窗中授权存储权限后重试");
          } else {
            setErrorToast(
              `已保存到相册（${kind === "photo" ? "图片" : kind === "video" ? "视频" : "音乐"}/RClaw）`,
            );
          }
        })
        .catch((e) => setErrorToast(String(e)));
      setPhotoEditor(null);
    },
    [setErrorToast],
  );

  /** 丢弃编辑（取消）：删除采集原图。 */
  const discardPhotoEditor = useCallback(() => {
    if (photoEditor) {
      void api.discardCapture(photoEditor.rel).catch(() => {});
    }
    setPhotoEditor(null);
  }, [photoEditor]);

  /** 附件菜单动作分发：拍照/拍视频走系统相机，录音走应用内录音条。 */
  const handleCaptureAction = useCallback(
    (kind: "photo" | "video" | "audio") => {
      if (kind === "audio") {
        void startRecording();
      } else {
        void startCapture(kind);
      }
    },
    [startRecording, startCapture],
  );

  // 系统返回键关一层弹层（与视觉层级无关，按打开先后响应；未注册的弹层
  // 返回键落到抽屉逻辑）。授权弹窗返回 = 拒绝（不响应会卡住队列，后端超时
  // 同样按拒绝兜底）。
  useBackClose(themePickerOpen, () => setThemePickerOpen(false));
  useBackClose(shareChooserOpen, () => setShareChooserOpen(false));
  useBackClose(forwardChooserOpen, () => {
    forwardDraftRef.current = null;
    setForwardChooserOpen(false);
  });
  useBackClose(chatMenu !== null, () => setChatMenu(null));
  useBackClose(deleteConfirm !== null, () => setDeleteConfirm(null));
  useBackClose(permQueue.length > 0, () => answerPermission(false));
  useBackClose(photoEditor !== null, discardPhotoEditor);
  useBackClose(captureDialog !== null, () => setCaptureDialog(null));
  useBackClose(recording !== null, () => void cancelRecording());

  let body: ReactNode;
  if (!status) {
    body = <div className="flex h-full items-center justify-center text-sm text-gray-500">加载中…</div>;
  } else if (status.wizard_needed) {
    // 首运行：配置向导。
    body = (
      <Wizard
        home={status.home}
        onDone={() => {
          setAgentState("starting");
          setStatus({ ...status, wizard_needed: false });
        }}
      />
    );
  } else {
    const activeBusy = busyFor(activeId);
    const activeEntries = entriesByChat[activeId] ?? [];
    body = (
      <div className="flex h-full">
      {isMobile && drawerOpen && <DrawerScrim onClose={() => setDrawerOpen(false)} />}
      {/* 侧边栏：展开 = 图标+文字（宽度可拖拽调整）；收起 = 图标栏（悬浮 title 提示）。 */}
      <div
        className={`sidebar-bg flex shrink-0 flex-col overflow-hidden border-r border-[#2a2f3a] bg-[#12151d] ${
          isMobile
            ? drawerOpen
              ? "fixed inset-y-0 left-0 z-50 w-[17rem] max-w-[85vw] shadow-2xl"
              : "hidden"
            : sidebarCollapsed
              ? "w-14"
              : ""
        }`}
        style={
          isMobile
            ? { paddingTop: "env(safe-area-inset-top)" }
            : sidebarCollapsed
              ? undefined
              : { width: leftWidth }
        }
      >
        <div
          className={`flex items-center gap-2 py-4 max-md:py-2.5 ${
            sidebarCollapsed ? "flex-col px-0" : "px-3"
          }`}
        >
          <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-indigo-600 text-sm font-bold text-white">
            R
          </div>
          {!sidebarCollapsed && (
            <span className="min-w-0 flex-1 truncate font-semibold text-gray-100">RClaw</span>
          )}
          {/* 新建对话快捷按钮（RClaw 右侧；新建即回到对话页）。 */}
          <button
            onClick={() => {
              newChat();
              setPage("chat");
              setDrawerOpen(false);
            }}
            title="新建对话"
            className="flex h-7 w-7 shrink-0 items-center justify-center rounded-md text-base leading-none text-gray-400 transition hover:bg-[#1a1f29] hover:text-indigo-300"
          >
            ＋
          </button>
          <button
            onClick={toggleSidebar}
            title={sidebarCollapsed ? "展开侧栏" : "收起侧栏"}
            className="rounded-md px-1.5 py-1.5 text-gray-500 transition hover:bg-[#1a1f29] hover:text-gray-200"
          >
            {sidebarCollapsed ? <ExpandIcon /> : <CollapseIcon />}
          </button>
        </div>

        {/* 侧边栏顶部常驻入口：浏览器（应用内面板，手动上网/人工过码）。
            桌面版入口在标题栏右上角（地球图标），此处仅移动端抽屉展示。 */}
        {isMobile && (
          <div className="px-3 pb-1">
            <button
              onClick={() => {
                void api.browserToggleVisible().catch(() => {});
                setDrawerOpen(false);
              }}
              className="flex w-full items-center gap-2.5 rounded-lg px-3 py-2 text-sm text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100 max-md:py-2"
            >
              <span className="flex h-4 w-4 shrink-0 items-center justify-center">
                <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round">
                  <circle cx="8" cy="8" r="6.2" />
                  <path d="M1.8 8h12.4M8 1.8c1.9 1.7 2.9 3.8 2.9 6.2S9.9 12.5 8 14.2C6.1 12.5 5.1 10.4 5.1 8S6.1 3.5 8 1.8z" />
                </svg>
              </span>
              浏览器
            </button>
          </div>
        )}

        {/* 导航区自滚（条目多时上下滑动，不再把底部会话区挤走）。 */}
        <nav className={`min-h-0 flex-1 space-y-1 overflow-y-auto ${sidebarCollapsed ? "px-2" : "px-3"}`}>
          {/* 任务概览仅移动端抽屉展示（桌面由右侧 TaskPanel 承担）：
              与其他栏目同级，点击进入专门的概览详情页。
              「关于」固定置底：先渲染除它以外的栏目，安卓的主题入口垫在
              「配置」之后，最后渲染「关于」（NAV 末项）。 */}
          {(
            isMobile
              ? ([
                  ...NAV.slice(0, 7),
                  ["taskview", "任务概览", () => <TasksIcon />],
                  ...NAV.slice(7, -1),
                ] as Array<[Page, string, () => ReactNode]>)
              : NAV.slice(0, -1)
          ).map(([key, label, Icon]) => (
            <button
              key={key}
              onClick={() => {
                setPage(key);
                setDrawerOpen(false);
              }}
              title={label}
              className={`flex w-full items-center rounded-lg text-sm transition max-md:text-[15px] ${
                sidebarCollapsed ? "justify-center py-2" : "gap-2.5 px-3 py-2 text-left max-md:py-2"
              } ${
                page === key
                  ? "bg-indigo-500/10 text-indigo-300"
                  : "text-gray-400 hover:bg-[#1a1f29] hover:text-gray-200"
              }`}
            >
              <span className="flex h-4 w-4 shrink-0 items-center justify-center">
                <Icon />
              </span>
              {!sidebarCollapsed && label}
            </button>
          ))}
          {/* 安卓 App：主题选择入口（顶栏已移除，收进侧栏栏目；仅手机布局）。 */}
          {isAndroidDevice() && isMobile && !sidebarCollapsed && (
            <button
              onClick={() => {
                setDrawerOpen(false);
                setThemePickerOpen(true);
              }}
              title="主题"
              className="flex w-full items-center gap-2.5 rounded-lg px-3 py-2 text-left text-sm text-gray-400 transition hover:bg-[#1a1f29] hover:text-gray-200 max-md:py-2 max-md:text-[15px]"
            >
              <span className="flex h-4 w-4 shrink-0 items-center justify-center text-[13px]">
                🎨
              </span>
              主题
            </button>
          )}
          {NAV.slice(-1).map(([key, label, Icon]) => (
            <button
              key={key}
              onClick={() => {
                setPage(key);
                setDrawerOpen(false);
              }}
              title={label}
              className={`flex w-full items-center rounded-lg text-sm transition max-md:text-[15px] ${
                sidebarCollapsed ? "justify-center py-2" : "gap-2.5 px-3 py-2 text-left max-md:py-2"
              } ${
                page === key
                  ? "bg-indigo-500/10 text-indigo-300"
                  : "text-gray-400 hover:bg-[#1a1f29] hover:text-gray-200"
              }`}
            >
              <span className="flex h-4 w-4 shrink-0 items-center justify-center">
                <Icon />
              </span>
              {!sidebarCollapsed && label}
            </button>
          ))}
        </nav>

        {/* 移动端：上下文窗口 / 内存占用（数据路径上方；桌面在 TaskPanel）。
            单行紧凑卡：小圆点颜色表达上下文压力（>80% 变红，替代桌面进度条）。 */}
        {isMobile && (
          <div className="mx-3 mb-2 flex items-center justify-between rounded-xl border border-[#2a2f3a] bg-[#141821] px-3 py-2 text-[11px] text-gray-500">
            <span className="flex items-center gap-1.5">
              <span
                className={`inline-block h-1.5 w-1.5 rounded-full ${
                  mobileUsage &&
                  mobileUsage.context_length > 0 &&
                  mobileUsage.request_used / mobileUsage.context_length > 0.8
                    ? "bg-red-500"
                    : "bg-indigo-500"
                }`}
              />
              上下文{" "}
              <span className="font-mono text-gray-300">
                {mobileUsage && mobileUsage.context_length > 0
                  ? `${(mobileUsage.request_used / 1000).toFixed(1)}K/${(mobileUsage.context_length / 1000).toFixed(1)}K`
                  : "—"}
              </span>
            </span>
            <span>
              内存{" "}
              <span className="font-mono text-gray-300">
                {mobileUsage && mobileUsage.rss_bytes > 0
                  ? `${(mobileUsage.rss_bytes / 1024 / 1024).toFixed(0)} MB`
                  : "—"}
              </span>
            </span>
          </div>
        )}

        {!sidebarCollapsed && (
          <div className="px-5 py-3 text-[10px] leading-relaxed text-gray-600">
            <button
              type="button"
              title="单击复制路径，双击在文件管理器打开"
              onClick={handleHomeClick}
              onDoubleClick={handleHomeDoubleClick}
              className="block max-w-full cursor-pointer truncate text-left transition hover:text-gray-300"
            >
              {homeHint ?? `数据：${status.home}`}
            </button>
          </div>
        )}
      </div>

      {/* 侧栏拖宽手柄（收起为图标栏时无意义，隐藏）。 */}
      {!isMobile && !sidebarCollapsed && <ResizeHandle onDrag={resizeLeft} />}

      {/* 主区 */}
      <div className="min-w-0 flex-1">
        {agentState === "starting" && (
          <div className="flex h-full flex-col items-center justify-center gap-3 text-sm text-gray-400">
            <div className="h-8 w-8 animate-spin rounded-full border-2 border-indigo-500 border-t-transparent" />
            正在启动 RClaw…
            {agentError && <div className="text-red-400">{agentError}</div>}
          </div>
        )}
        {agentState === "error" && (
          <div className="flex h-full flex-col items-center justify-center gap-2 px-8 text-center">
            <div className="text-sm text-red-400">agent 启动失败</div>
            <div className="max-w-lg text-xs text-gray-500">{agentError}</div>
            <div className="text-xs text-gray-600">
              请检查 {status.home}/rclaw.toml 的 provider 配置后重启应用。
            </div>
          </div>
        )}
        {agentState === "ready" && page === "chats" && (
          <div className="h-full overflow-y-auto bg-[#0f131b] px-5 py-4 max-md:px-3">
        {/* 会话列表（收起为图标栏时隐藏）；移动端保证最小可见高度，
            超出部分由侧栏整体滚动承接（导航/状态块不再挤占会话区）。 */}
        <div className="mb-3 flex items-center justify-between">
          <h2 className="text-base font-semibold text-gray-100">会话</h2>
          <button
            onClick={() => {
              newChat();
              setPage("chat");
            }}
            className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-3 py-1.5 text-xs text-gray-300 transition hover:border-indigo-500/50 hover:text-indigo-300"
          >
            ＋ 新建对话
          </button>
        </div>
          {/* todo（zcode 会话面板同款信息位）：最近一个未收尾计划的步骤清单，
              与消息流的「流程步骤」卡片同源数据。 */}
          {latestPlan && !latestPlan.completed && latestPlan.steps.length > 0 && (
            <div className="mb-2 rounded-lg border border-[#2a2f3a] bg-[#141821] px-2.5 py-2">
              <div className="flex items-center justify-between text-[11px] text-gray-500">
                <span className="truncate">📋 {latestPlan.name}</span>
                <span>
                  {latestPlan.steps.filter((s) => s.status === "pending").length} 项待办
                </span>
              </div>
              <ul className="mt-1 space-y-0.5">
                {latestPlan.steps.slice(0, 8).map((step) => (
                  <li key={step.id} className="flex items-start gap-1.5 text-[11px] leading-snug">
                    <span
                      className={
                        step.status === "succeeded"
                          ? "text-emerald-400"
                          : step.status === "failed"
                            ? "text-red-400"
                            : step.status === "skipped"
                              ? "text-gray-600"
                              : step.status === "running"
                                ? "text-indigo-400"
                                : step.status === "verifying"
                                  ? "text-amber-300"
                                  : "text-amber-400"
                      }
                    >
                      {step.status === "succeeded"
                        ? "✓"
                        : step.status === "failed"
                          ? "✗"
                          : step.status === "skipped"
                            ? "⤼"
                            : step.status === "running"
                              ? "◌"
                              : step.status === "verifying"
                                ? "🔍"
                                : "☐"}
                    </span>
                    <span
                      className="min-w-0 truncate text-gray-400"
                      title={step.task || step.summary}
                    >
                      {step.id}
                      {step.task ? ` · ${step.task}` : ""}
                    </span>
                  </li>
                ))}
                {latestPlan.steps.length > 8 && (
                  <li className="pl-4 text-[10px] text-gray-600">
                    … 共 {latestPlan.steps.length} 步
                  </li>
                )}
              </ul>
            </div>
          )}
          <div className="min-h-0 flex-1 space-y-0.5 overflow-y-auto pb-2">
            {chats.filter((c) => !c.archived).length === 0 && (
              <div className="px-2 py-1 text-xs text-gray-600">暂无会话</div>
            )}
            {chats
              .filter((c) => !c.archived)
              .map((c) => (
                <div
                  key={c.chat_id}
                  data-chat-row
                  className={`group flex items-center rounded-lg transition ${
                    activeId === c.chat_id
                      ? "bg-[#1a2030] text-indigo-200"
                      : "text-gray-400 hover:bg-[#1a1f29]"
                  }`}
                >
                  {/* 会话工作状态：agent 工作中 = 转圈，turn 结束 = 绿点。 */}
                  {renamingId !== c.chat_id &&
                    (busyByChat[c.chat_id] ? (
                      <span
                        title="Agent 工作中"
                        className="ml-2.5 h-2.5 w-2.5 shrink-0 animate-spin rounded-full border-[1.5px] border-indigo-400 border-t-transparent"
                      />
                    ) : (
                      <span
                        title="空闲"
                        className="ml-3 h-2 w-2 shrink-0 rounded-full bg-emerald-500"
                      />
                    ))}
                  {renamingId === c.chat_id ? (
                    <input
                      autoFocus
                      value={renameDraft}
                      onChange={(e) => setRenameDraft(e.target.value)}
                      onClick={(e) => e.stopPropagation()}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") {
                          void commitRename();
                        } else if (e.key === "Escape") {
                          renameCancelled.current = true;
                          setRenamingId(null);
                        }
                      }}
                      onBlur={() => void commitRename()}
                      maxLength={120}
                      placeholder={c.chat_id}
                      className="min-w-0 flex-1 rounded-md border border-indigo-500/50 bg-[#161a22] px-2 py-1 text-xs text-gray-200 outline-none"
                    />
                  ) : (
                    <button
                      onClick={() => {
                        void openChat(c.chat_id);
                        setPage("chat");
                      }}
                      onDoubleClick={() => startRename(c.chat_id, c.title || c.chat_id)}
                      onContextMenu={(e) => {
                        e.preventDefault();
                        setChatMenu({
                          chatId: c.chat_id,
                          title: c.title || c.chat_id,
                          x: e.clientX,
                          // 靠底行向上弹，避免菜单溢出窗口。
                          y: Math.min(e.clientY, window.innerHeight - 170),
                        });
                      }}
                      className="min-w-0 flex-1 px-2 py-1.5 text-left max-md:py-2.5"
                      title={`${c.title || c.chat_id}（双击重命名，右键更多操作）`}
                    >
                      <div className="truncate text-xs max-md:text-[13px]">
                        {c.chat_id.startsWith("bot-") ? "🤖 " : ""}
                        {c.title || c.chat_id}
                      </div>
                    </button>
                  )}
                  {!busyByChat[c.chat_id] && renamingId !== c.chat_id && (
                    <>
                      {/* 移动端操作入口（触屏无右键/hover）：弹出重命名/导出/归档菜单 */}
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          const r = e.currentTarget.getBoundingClientRect();
                          setChatMenu({
                            chatId: c.chat_id,
                            title: c.title || c.chat_id,
                            x: Math.min(r.left, window.innerWidth - 176),
                            y: Math.min(r.bottom + 4, window.innerHeight - 170),
                          });
                        }}
                        title="会话操作"
                        className="mr-0.5 hidden shrink-0 rounded px-1.5 py-0.5 text-xs leading-none text-gray-500 transition hover:text-gray-200 max-md:block"
                      >
                        ⋯
                      </button>
                      <button
                        onClick={() => void archiveChat(c.chat_id)}
                        title="归档此会话"
                        className="mr-1 hidden rounded px-1 text-[10px] text-gray-500 transition hover:text-amber-300 group-hover:block"
                      >
                        归档
                      </button>
                    </>
                  )}
                </div>
              ))}
            {/* 已归档（折叠展示，可恢复/删除） */}
            {chats.some((c) => c.archived) && (
              <ArchivedSection
                items={chats.filter((c) => c.archived)}
                onRestore={(id) => void restoreChat(id)}
                onDelete={(id, title) => setDeleteConfirm({ chatId: id, title })}
              />
            )}
          </div>
          </div>
        )}

        {agentState === "ready" && page === "chat" && (
          <Chat
            entries={activeEntries}
            busy={activeBusy}
            onSend={handleSend}
            onCancel={handleCancel}
            onError={setErrorToast}
            draft={chatDraft}
            onDraftChange={setChatDraft}
            onCapture={isMobile ? handleCaptureAction : undefined}
            onForwardToChat={forwardToChat}
            sessionKey={activeId}
            onFork={handleFork}
            plans={plans}
            onPlanPause={handlePlanPause}
            queue={queueByChat[activeId] ?? []}
            onQueueEnqueue={(text) => enqueueQueued(activeId, text)}
            onQueueRemove={(id) => removeQueued(activeId, id)}
            futureThinkingCollapsed={futureThinkingCollapsed[activeId] ?? false}
            onToggleThinking={(collapsed) =>
              setFutureThinkingCollapsed((prev) => ({ ...prev, [activeId]: collapsed }))
            }
          />
        )}
        {agentState === "ready" && page === "bots" && (
          <BotsPanel agentReady onOpenChat={(id) => void openChat(id)} />
        )}
        {agentState === "ready" && page === "tools" && <ToolsPanel agentReady />}
        {agentState === "ready" && page === "skills" && <SkillsPanel agentReady />}
        {agentState === "ready" && page === "files" && <FilePanel onError={setErrorToast} />}
        {agentState === "ready" && page === "agents" && <AgentsPanel agentReady />}
        {agentState === "ready" && page === "evolution" && <EvolutionPanel agentReady />}
        {agentState === "ready" && page === "cron" && <CronPanel agentReady />}
        {agentState === "ready" && page === "taskview" && <TaskOverviewPanel agentReady />}
        {agentState === "ready" && page === "config" && (
          <ConfigPanel
            agentReady
            slogans={slogans}
            sloganInterval={sloganInterval}
            onSlogansSaved={(prefs) => {
              setSlogans(prefs.slogans);
              setSloganInterval(prefs.interval_secs);
            }}
          />
        )}
        {agentState === "ready" && page === "about" && <About home={status.home} />}
      </div>

      {/* 右侧任务面板：任务概览 + 上下文/内存状态（宽度可拖拽调整）。移动端
          不渲染（信息在「Agent」页），避免挤压会话区。 */}
      {taskPanelOpen && agentState === "ready" && !isMobile && (
        <>
          <ResizeHandle onDrag={resizeRight} />
          <TaskPanel
            agentReady
            activeChatId={activeId}
            width={rightWidth}
            onOpenAgents={() => setPage("agents")}
          />
        </>
      )}
      </div>
    );
  }

  return (
    <div
      className="flex h-full flex-col"
      style={
        // 安卓 App 无顶栏：让出系统状态栏/刘海安全区（其余平台由顶栏承担）。
        isAndroidDevice() && isMobile && status && !status.wizard_needed
          ? { paddingTop: "env(safe-area-inset-top)" }
          : undefined
      }
    >
      {!isMobile && (
        <TitleBar
          title={sloganTitle}
          theme={THEMES.find((t) => t.id === theme)?.base ?? "dark"}
          taskPanelOpen={taskPanelOpen}
          onToggleTaskPanel={toggleTaskPanel}
          onToggleTheme={toggleTheme}
        />
      )}
      {/* 安卓 App 去掉顶栏（侧栏由左缘右滑呼出，主题入口在侧栏栏目里）；
          其他移动端（桌面窄窗口模拟）保持顶栏不变。 */}
      {isMobile && !isAndroidDevice() && status && !status.wizard_needed && (
        <MobileTopBar
          title={sloganTitle}
          theme={THEMES.find((t) => t.id === theme)?.base ?? "dark"}
          onOpenDrawer={() => setDrawerOpen(true)}
          onToggleTheme={toggleTheme}
        />
      )}
      <div className="min-h-0 flex-1">{body}</div>
      {toast && (
        <div className="fixed bottom-4 left-1/2 -translate-x-1/2 rounded-lg border border-red-500/40 bg-red-500/10 px-4 py-2 text-sm text-red-300">
          {toast}
        </div>
      )}
      {/* 系统分享目标选择：多会话时选择发送到哪个会话或新会话。 */}
      {shareChooserOpen && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="mx-4 w-[20rem] rounded-xl border border-[#2a2f3a] bg-[#141821] p-4 shadow-2xl max-md:w-[calc(100%-2rem)]">
            <h2 className="text-sm font-semibold text-gray-100">分享到 RClaw</h2>
            <p className="mt-1 text-[11px] text-gray-500">选择发送到哪个会话：</p>
            <div className="mt-3 max-h-64 space-y-1 overflow-y-auto">
              <button
                onClick={() => placeShare("new")}
                className="block w-full rounded-lg bg-indigo-500/10 px-3 py-2 text-left text-sm text-indigo-200 transition hover:bg-indigo-500/20"
              >
                ➕ 新会话
              </button>
              {chats
                .filter((c) => !c.archived)
                .map((c) => (
                  <button
                    key={c.chat_id}
                    onClick={() => placeShare(c.chat_id)}
                    className="block w-full truncate rounded-lg px-3 py-2 text-left text-sm text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
                    title={c.chat_id}
                  >
                    {busyByChat[c.chat_id] ? "🔄 " : "💬 "}
                    {c.title || c.chat_id}
                  </button>
                ))}
            </div>
            <button
              onClick={() => setShareChooserOpen(false)}
              className="mt-3 w-full rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-400 transition hover:text-gray-200"
            >
              取消（文件保留在 uploads/）
            </button>
          </div>
        </div>
      )}
      {/* 消息选择「发送到会话」目标选择器：并入目标会话草稿（可编辑后发送）。 */}
      {forwardChooserOpen && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="mx-4 w-[20rem] rounded-xl border border-[#2a2f3a] bg-[#141821] p-4 shadow-2xl max-md:w-[calc(100%-2rem)]">
            <h2 className="text-sm font-semibold text-gray-100">发送到会话</h2>
            <p className="mt-1 text-[11px] text-gray-500">
              选中的消息将并入目标会话的输入框（可编辑后发送）：
            </p>
            <div className="mt-3 max-h-64 space-y-1 overflow-y-auto">
              <button
                onClick={() => placeForward("new")}
                className="block w-full rounded-lg bg-indigo-500/10 px-3 py-2 text-left text-sm text-indigo-200 transition hover:bg-indigo-500/20"
              >
                ➕ 新会话
              </button>
              {chats
                .filter((c) => !c.archived)
                .map((c) => (
                  <button
                    key={c.chat_id}
                    onClick={() => placeForward(c.chat_id)}
                    className="block w-full truncate rounded-lg px-3 py-2 text-left text-sm text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
                    title={c.chat_id}
                  >
                    {busyByChat[c.chat_id] ? "🔄 " : "💬 "}
                    {c.title || c.chat_id}
                  </button>
                ))}
            </div>
            <button
              onClick={() => {
                forwardDraftRef.current = null;
                setForwardChooserOpen(false);
              }}
              className="mt-3 w-full rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-400 transition hover:text-gray-200"
            >
              取消
            </button>
          </div>
        </div>
      )}
      {/* 工作区外访问授权弹窗（默认权限下工具请求触达工作区之外时出现）。 */}
      {permQueue.length > 0 && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="mx-4 w-[26rem] rounded-xl border border-[#2a2f3a] bg-[#141821] p-5 shadow-2xl max-md:mx-4 max-md:w-[calc(100%-2rem)]">
            <h2 className="text-sm font-semibold text-gray-100">工具授权请求</h2>
            <p className="mt-2 text-xs leading-relaxed text-gray-400">
              Agent 正在尝试访问工作区以外的路径（默认权限下需要你的确认）：
            </p>
            <p
              className="mt-2 max-h-24 overflow-y-auto rounded-lg bg-[#0f131b] px-3 py-2 font-mono text-xs break-all text-indigo-300"
            >
              {permQueue[0].path}
            </p>
            <p className="mt-2 text-[10px] text-gray-600">
              允许后本次运行内对该文件的同类操作不再重复询问（其它操作、其它文件仍会询问）；拒绝会让 agent 收到访问被拒提示。也可在
              「配置」页切换为完全访问模式（任意路径免确认）。
              注意：对某个工作区外文件的首次放行若是写入 / 删除，这一次不记回滚点（放行前不读取它的原文），/rewind 还原不了它；该文件放行之后的写入 / 删除才有回滚点。
            </p>
            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => answerPermission(false)}
                className="rounded-lg border border-[#2a2f3a] px-4 py-1.5 text-xs text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
              >
                拒绝
              </button>
              <button
                onClick={() => answerPermission(true)}
                className="rounded-lg bg-indigo-600 px-4 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500"
              >
                允许
              </button>
            </div>
          </div>
        </div>
      )}
      {/* 会话右键菜单（重命名 / 刷新 / 导出 / 归档 / 删除）。 */}
      {chatMenu && (
        <div
          data-chat-menu
          className="fixed z-50 min-w-40 rounded-lg border border-[#2a2f3a] bg-[#141821] py-1 shadow-xl"
          style={{ left: chatMenu.x, top: chatMenu.y }}
        >
          <button
            className={CHAT_MENU_ITEM}
            onClick={() => {
              const m = chatMenu;
              setChatMenu(null);
              startRename(m.chatId, m.title);
            }}
          >
            重命名
          </button>
          <button
            className={CHAT_MENU_ITEM}
            onClick={() => {
              setChatMenu(null);
              void refreshChats();
            }}
          >
            刷新列表
          </button>
          <button
            className={CHAT_MENU_ITEM}
            onClick={() => {
              const m = chatMenu;
              setChatMenu(null);
              void exportChat(m.chatId, m.title);
            }}
          >
            导出完整聊天记录
          </button>
          <button
            className={CHAT_MENU_ITEM}
            onClick={() => {
              const m = chatMenu;
              setChatMenu(null);
              void archiveChat(m.chatId);
            }}
          >
            归档
          </button>
          <button
            className={`${CHAT_MENU_ITEM} text-red-400 hover:bg-red-500/10 hover:text-red-300`}
            onClick={() => {
              const m = chatMenu;
              setChatMenu(null);
              setDeleteConfirm({ chatId: m.chatId, title: m.title });
            }}
          >
            删除
          </button>
        </div>
      )}

      {/* 删除会话确认（不可恢复；区别于可撤销的「归档」）。 */}
      {deleteConfirm && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="mx-4 w-[24rem] rounded-xl border border-[#2a2f3a] bg-[#141821] p-5 shadow-2xl max-md:w-[calc(100%-2rem)]">
            <h2 className="text-sm font-semibold text-gray-100">删除会话</h2>
            <p className="mt-2 text-xs leading-relaxed text-gray-400">
              将永久删除「{deleteConfirm.title}」的完整聊天记录，不可恢复。
              如只是暂时收起，请改用「归档」。
            </p>
            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => setDeleteConfirm(null)}
                className="rounded-lg border border-[#2a2f3a] px-4 py-1.5 text-xs text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
              >
                取消
              </button>
              <button
                onClick={() => {
                  const m = deleteConfirm;
                  setDeleteConfirm(null);
                  void deleteChat(m.chatId);
                }}
                className="rounded-lg bg-red-600 px-4 py-1.5 text-xs font-medium text-white transition hover:bg-red-500"
              >
                删除
              </button>
            </div>
          </div>
        </div>
      )}

      {/* 录音中指示条：计时 + 完成 / 取消。 */}
      {recording && (
        <div className="fixed bottom-24 left-1/2 z-50 flex -translate-x-1/2 items-center gap-3 rounded-full border border-red-500/40 bg-[#141821] px-4 py-2 shadow-xl">
          <span className="flex items-center gap-2 text-sm text-red-400">
            <span className="inline-block h-2 w-2 animate-pulse rounded-full bg-red-500" />
            录音中 {String(Math.floor(recordingSeconds / 60)).padStart(2, "0")}:
            {String(recordingSeconds % 60).padStart(2, "0")}
          </span>
          <button
            onClick={() => void stopRecording()}
            className="rounded-full bg-indigo-600 px-4 py-1 text-xs font-medium text-white transition hover:bg-indigo-500"
          >
            完成
          </button>
          <button
            onClick={() => void cancelRecording()}
            className="rounded-full border border-[#2a2f3a] px-4 py-1 text-xs text-gray-400 transition hover:text-gray-200"
          >
            取消
          </button>
        </div>
      )}

      {/* 相机处理中指示（含等待授权；非阻塞角标，授权后原生自动续跑）。 */}
      {captureInFlight && (
        <div className="fixed left-1/2 top-16 z-40 -translate-x-1/2 rounded-full border border-[#2a2f3a] bg-[#141821] px-4 py-1.5 text-xs text-gray-300 shadow-lg">
          📷 等待相机 / 处理拍摄结果…
        </div>
      )}

      {/* 拍照编辑页：旋转/画笔后三选（发送/保存/取消）。 */}
      {photoEditor && (
        <PhotoEditor
          dataUrl={photoEditor.dataUrl}
          rel={photoEditor.rel}
          onClose={discardPhotoEditor}
          onSent={(rel) =>
            prefillCapture(`[附件] ${rel}（已放入 agent 工作区）`)
          }
          onSaved={(rel) => saveCaptureLocal(rel, "photo")}
          onError={setErrorToast}
        />
      )}

      {/* 视频/录音三选对话框（音频含 whisper 转文字）。 */}
      {captureDialog && (
        <CaptureDialog
          kind={captureDialog.kind}
          rel={captureDialog.rel}
          onDraft={prefillCapture}
          onClose={() => setCaptureDialog(null)}
          onError={setErrorToast}
        />
      )}

      {/* 主题选择器：一键切换，含聊天/侧边栏背景预览。 */}
      {themePickerOpen && (
        <div className="fixed inset-0 z-50" onClick={() => setThemePickerOpen(false)}>
          <div
            className="absolute right-2 w-64 rounded-xl border border-[#2a2f3a] bg-[#141821] p-3 shadow-2xl max-md:left-1/2 max-md:right-auto max-md:-translate-x-1/2"
            style={{
              top: isAndroidDevice()
                ? "calc(env(safe-area-inset-top) + 0.75rem)"
                : "calc(env(safe-area-inset-top) + 3.75rem)",
            }}
            onClick={(e) => e.stopPropagation()}
          >
            <h2 className="mb-2 text-sm font-semibold text-gray-100">主题</h2>
            <div className="grid grid-cols-2 gap-2">
              {THEMES.map((t) => (
                <button
                  key={t.id}
                  onClick={() => applyTheme(t.id)}
                  className={`rounded-lg border p-2 text-left transition ${
                    theme === t.id
                      ? "border-indigo-400 bg-indigo-500/10"
                      : "border-[#2a2f3a] hover:border-indigo-500/50"
                  }`}
                >
                  <div className="flex h-10 overflow-hidden rounded-md border border-black/20">
                    <div className="h-full w-1/3" style={{ backgroundColor: t.swatch[1] }} />
                    <div className="h-full flex-1" style={{ backgroundColor: t.swatch[0] }} />
                  </div>
                  <span className="mt-1.5 block text-xs font-medium text-gray-200">
                    {theme === t.id ? "✓ " : ""}
                    {t.name}
                  </span>
                  <span className="block text-[10px] text-gray-500">{t.desc}</span>
                </button>
              ))}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

/** 已归档会话（折叠区，可恢复 / 删除）。 */
function ArchivedSection({
  items,
  onRestore,
  onDelete,
}: {
  items: ChatMeta[];
  onRestore: (chatId: string) => void;
  onDelete: (chatId: string, title: string) => void;
}) {
  const [open, setOpen] = useState(false);
  return (
    <div className="mt-2">
      <button
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center justify-between rounded-lg px-2 py-1 text-xs text-gray-500 transition hover:text-gray-300"
      >
        <span>已归档（{items.length}）</span>
        <span>{open ? "▾" : "▸"}</span>
      </button>
      {open && (
        <div className="space-y-0.5">
          {items.map((c) => (
            <div
              key={c.chat_id}
              className="group flex items-center rounded-lg text-gray-500 transition hover:bg-[#1a1f29]"
            >
              <span className="min-w-0 flex-1 truncate px-2 py-1.5 text-xs" title={c.title || c.chat_id}>
                {c.title || c.chat_id}
              </span>
              <button
                onClick={() => onRestore(c.chat_id)}
                title="恢复此会话"
                className="mr-1 hidden rounded px-1 text-[10px] text-gray-500 transition hover:text-emerald-300 group-hover:block max-md:block"
              >
                恢复
              </button>
              <button
                onClick={() => onDelete(c.chat_id, c.title || c.chat_id)}
                title="永久删除此会话"
                className="mr-1 hidden rounded px-1 text-[10px] text-gray-500 transition hover:text-red-400 group-hover:block max-md:block"
              >
                删除
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** 检查更新卡片（关于页）：GitHub releases 比对当前版本；有新版可一键下载并
 *  触发安装——Windows 换单文件 exe 后自动重启，安卓拉起系统安装器。进度经
 *  update_progress 事件推送。 */
function UpdateCard({ currentVersion }: { currentVersion?: string | null }) {
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [phase, setPhase] = useState<"idle" | "download" | "install">("idle");
  const [progress, setProgress] = useState<{ received: number; total: number } | null>(null);

  useEffect(() => {
    const un = api.onUpdateProgress((p) => {
      if (p.phase === "install") {
        setPhase("install");
        return;
      }
      setPhase("download");
      setProgress({ received: p.received, total: p.total });
    });
    return () => {
      un.then((fn) => fn());
    };
  }, []);

  const check = useCallback(async () => {
    setChecking(true);
    setError(null);
    try {
      setInfo(await api.updateCheck());
    } catch (e) {
      setError(String(e));
    } finally {
      setChecking(false);
    }
  }, []);

  const install = useCallback(async () => {
    setPhase("download");
    setProgress(null);
    setError(null);
    try {
      await api.updateDownloadInstall();
      // 命令返回 = 交付完成，不等于安装完成：Windows 进程即将退出（保持
      // install 态无所谓）；安卓只是拉起了系统安装器——用户取消安装/先去授权
      // 「未知来源」都会回到本界面，15s 后复位状态让「更新到 vX」可重试。
      if (isAndroidDevice()) {
        window.setTimeout(() => setPhase((cur) => (cur === "install" ? "idle" : cur)), 15000);
      }
    } catch (e) {
      setError(String(e));
      setPhase("idle");
    }
  }, []);

  const hasNewer = info?.latest != null;
  const busy = phase !== "idle";
  const pct =
    progress && progress.total > 0
      ? Math.min(100, Math.round((progress.received / progress.total) * 100))
      : null;

  return (
    <div className="rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
      <h2 className="mb-1 text-sm font-medium text-gray-200">检查更新</h2>
      <p className="mb-2 text-[10px] text-gray-600">
        当前版本 {currentVersion ?? info?.current ?? "…"}；对照 GitHub 发布页的最新 Release。发现新版后
        {isAndroidDevice() ? "下载 APK 并拉起系统安装器" : "下载新程序并自动替换重启"}。
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <button
          onClick={() => void check()}
          disabled={checking || busy}
          className="rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-300 transition enabled:hover:border-indigo-500/50 enabled:hover:text-indigo-300 disabled:opacity-50"
        >
          {checking ? "检查中…" : "检查更新"}
        </button>
        {hasNewer && !busy && (
          <button
            onClick={() => void install()}
            className="rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500"
          >
            更新到 v{info!.latest}
          </button>
        )}
        {!checking && info && !hasNewer && !info.error && (
          <span className="text-xs text-emerald-400">已是最新版本 ✓</span>
        )}
        {hasNewer && info?.url && !busy && (
          <button
            onClick={() => void api.openUrl(info.url!)}
            className="text-xs text-gray-500 underline decoration-dotted hover:text-gray-300"
          >
            前往发布页
          </button>
        )}
      </div>
      {phase === "download" && (
        <div className="mt-2">
          <div className="mb-1 text-[10px] text-gray-400">
            下载中…{pct != null ? ` ${pct}%` : ""}
            {progress ? `（${(progress.received / 1048576).toFixed(1)} MB）` : ""}
          </div>
          <div className="h-1.5 overflow-hidden rounded-full bg-[#0f131b]">
            <div
              className="h-full bg-indigo-500 transition-all"
              style={{ width: `${pct ?? 5}%` }}
            />
          </div>
        </div>
      )}
      {phase === "install" && (
        <p className="mt-2 text-xs text-indigo-300">
          {isAndroidDevice()
            ? "下载完成，已拉起系统安装器——请在弹窗中确认安装。"
            : "下载完成，正在准备替换重启…"}
        </p>
      )}
      {info?.error && (
        <p className="mt-2 text-xs text-amber-400">{info.error}（稍后重试）</p>
      )}
      {error && <p className="mt-2 text-xs text-red-400">{error}</p>}
      {hasNewer && info?.notes && (
        <pre className="mt-2 max-h-40 overflow-y-auto whitespace-pre-wrap rounded-lg bg-[#0f131b] p-2 text-[10px] leading-relaxed text-gray-400">
          {info.notes}
        </pre>
      )}
    </div>
  );
}

/** 关于页：斜杠命令与配置说明（设置类操作复用既有 slash 管道）。 */
function About({ home }: { home: string }) {
  const [versions, setVersions] = useState<AppVersions | null>(null);
  useEffect(() => {
    api.appVersions().then(setVersions).catch(() => {});
  }, []);
  // WebView 实际内核版本（安卓上随系统 WebView 更新，与构建期的 wry 无关）。
  const webviewVer = /Chrome\/([\d.]+)/.exec(navigator.userAgent)?.[1] ?? null;

  const versionRows: Array<[string, string | null | undefined]> = versions
    ? [
        ["应用", versions.app],
        ["Agent 内核 (rclaw-runtime)", versions.agent_runtime],
        ["核心类型 (rclaw-core)", versions.agent_core],
        ["LLM 接入 (rclaw-llm)", versions.agent_llm],
        ["工具集 (rclaw-tools)", versions.agent_tools],
        ["会话存储 (rclaw-store)", versions.agent_store],
        ["CLI 装配 (rclaw-cli)", versions.agent_cli],
        ...(versions.agent_channels
          ? [["通道接入 (rclaw-channels)", versions.agent_channels] as [string, string]]
          : []),
        ["Tauri", versions.tauri],
        ["WebView 内核", webviewVer],
        ["Rust 编译器", versions.rustc],
        ["构建提交", versions.git],
        ["构建时间", new Date(versions.build_time * 1000).toLocaleString()],
      ]
    : [];

  return (
    <div className="h-full overflow-y-auto p-6 max-md:p-4">
      <h1 className="mb-4 text-base font-semibold text-gray-100">关于与设置</h1>
      <div className="space-y-4 text-sm text-gray-400">
        <div className="rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
          <h2 className="mb-2 text-sm font-medium text-gray-200">版本信息</h2>
          {versions ? (
            <div className="space-y-1">
              {versionRows.map(([label, value]) => (
                <div key={label} className="flex items-start justify-between gap-3">
                  <span className="shrink-0 text-xs text-gray-500">{label}</span>
                  <span
                    className="min-w-0 break-all text-right font-mono text-xs text-gray-300"
                    title={value ?? undefined}
                  >
                    {value ?? "—"}
                  </span>
                </div>
              ))}
            </div>
          ) : (
            <div className="text-xs text-gray-500">读取中…</div>
          )}
        </div>
        {/* 检查更新（原在配置页）：版本信息紧跟着给更新入口。 */}
        <UpdateCard currentVersion={versions?.app} />
        {/* 后台保活（仅安卓）：电池白名单 + 厂商自启动页引导。 */}
        {isAndroidDevice() && <KeepAliveCard />}
        <div className="rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
          <h2 className="mb-2 text-sm font-medium text-gray-200">在对话里直接用命令</h2>
          <ul className="space-y-1 text-xs leading-relaxed">
            <li>
              <code className="text-indigo-300">/context</code> — 查看上下文窗口占用与压缩预算
            </li>
            <li>
              <code className="text-indigo-300">/llmodel</code> — 查看 / 切换模型（持久化到
              rclaw.toml）
            </li>
            <li>
              <code className="text-indigo-300">/search</code> — 搜索通道启停（已配置
              web_search 时）
            </li>
            <li>
              <code className="text-indigo-300">/cancel</code> — 停止当前任务（也可以点对话页的
              「停止」按钮）
            </li>
          </ul>
        </div>
        <div className="rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
          <h2 className="mb-2 text-sm font-medium text-gray-200">自进化</h2>
          <p className="text-xs leading-relaxed">
            RClaw 默认开启自动记忆与自进化：每次对话后后台 review 会把偏好写进 MEMORY.md /
            USER.md，把重复工作固化成技能（agent/skills/），并定期归档过时技能。全程可见——记忆与技能都是
            数据目录里的纯文本文件。
          </p>
        </div>
        <div className="rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
          <h2 className="mb-2 text-sm font-medium text-gray-200">数据目录</h2>
          <p className="font-mono text-xs break-all text-gray-500">{home}</p>
          <p className="mt-1 text-xs text-gray-600">
            rclaw.toml（配置）、agent/（会话、记忆、技能、日志）都在这里；会话、回滚点与
            归档标记都存在 agent/state.sqlite（SQLite 单库），导出用 /export 或侧栏「导出」。
            关闭窗口仅隐藏到托盘，RClaw 保持常驻。
          </p>
        </div>
      </div>
    </div>
  );
}
