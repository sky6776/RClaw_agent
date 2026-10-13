import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
} from "react";
import {
  api,
  isStatusLine,
  isThinkingLine,
  type AgentEvent,
  type AppVersions,
  type ChatMeta,
  type PermissionRequest,
  type Status,
  type UsageSnapshot,
} from "./lib/api";
import Wizard from "./components/Wizard";
import Chat, { type Entry } from "./components/Chat";
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

/** 导航项：页面 / 标签 / 图标。「对话」列在首位（收起后的图标栏也需要回到对话的入口）。 */
const NAV: Array<[Page, string, () => ReactNode]> = [
  ["chat", "对话", () => <ChatIcon />],
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
  // 之后新到的思考块是否默认收起（按会话）：由用户最近一次手动折叠（true）/
  // 展开（false）决定，只作为新块的初始状态，不改动已在屏上的块。
  const [futureThinkingCollapsed, setFutureThinkingCollapsed] = useState<Record<string, boolean>>({});
  // 已从持久化历史恢复过转录的会话（避免重复拉取）。
  const restoredChats = useRef<Set<string>>(new Set());
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
        // turn 收尾时会话记录已落库（先 append 后 outbound）：刷新列表，
        // 让新会话（bot 收件箱、首次对话）与派生标题及时出现在侧栏。
        void refreshChats();
      }
    });
    return () => {
      unAgent.then((fn) => fn());
      unBus.then((fn) => fn());
    };
  }, [appendEntries, mergeThinking, refreshChats, applyStatus]);

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
      if (restoredChats.current.has(chatId)) return;
      restoredChats.current.add(chatId);
      setEntriesByChat((prev) => ({ ...prev, [chatId]: prev[chatId] ?? [] }));
      try {
        const history = await api.chatHistory(chatId);
        setEntriesByChat((prev) => {
          // 事件可能先于历史返回：只填补空缺，不覆盖已有内容。
          if ((prev[chatId]?.length ?? 0) > 0) return prev;
          return {
            ...prev,
            [chatId]: history.map((m) => ({ id: next_entry_id++, role: m.role, content: m.content })),
          };
        });
      } catch {
        // agent 未启动等瞬时失败：回滚已恢复标记，下次打开还能重试；
        // 否则该会话永远保持空转录。
        restoredChats.current.delete(chatId);
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
        } else if (list.length > 0 && !restoredChats.current.has(activeId)) {
          // 初始定位到最近会话（默认 main 也在列表里）。
          void openChat(activeId);
        }
        return list;
      });
    });
    // 仅在就绪时执行一次。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentState]);

  const busyFor = (chatId: string) => busyByChat[chatId] ?? false;

  const handleSend = useCallback(
    (content: string) => {
      const chatId = activeId;
      appendEntries(chatId, [{ role: "user", content }]);
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
        setBusyByChat((prev) => ({ ...prev, [chatId]: false }));
      });
      void refreshChats();
    },
    [activeId, appendEntries, refreshChats],
  );

  const handleCancel = useCallback(() => {
    void api.cancel(activeId).catch(() => {
      // 取消确认会以 bus 消息回来；这里静默。
    });
  }, [activeId]);

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
        className={`sidebar-bg flex shrink-0 flex-col border-r border-[#2a2f3a] bg-[#12151d] ${
          isMobile
            ? drawerOpen
              ? "fixed inset-y-0 left-0 z-50 w-[17rem] max-w-[85vw] overflow-y-auto shadow-2xl"
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
          <button
            onClick={toggleSidebar}
            title={sidebarCollapsed ? "展开侧栏" : "收起侧栏"}
            className="rounded-md px-1.5 py-1.5 text-gray-500 transition hover:bg-[#1a1f29] hover:text-gray-200"
          >
            {sidebarCollapsed ? <ExpandIcon /> : <CollapseIcon />}
          </button>
        </div>

        <nav className={`space-y-1 ${sidebarCollapsed ? "px-2" : "px-3"}`}>
          {/* 任务概览仅移动端抽屉展示（桌面由右侧 TaskPanel 承担）：
              与其他栏目同级，点击进入专门的概览详情页。 */}
          {(isMobile
            ? ([
                ...NAV.slice(0, 7),
                ["taskview", "任务概览", () => <TasksIcon />],
                ...NAV.slice(7),
              ] as Array<[Page, string, () => ReactNode]>)
            : NAV
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
        </nav>

        {/* 会话列表（收起为图标栏时隐藏）；移动端保证最小可见高度，
            超出部分由侧栏整体滚动承接（导航/状态块不再挤占会话区）。 */}
        {!sidebarCollapsed && (
        <div className="mt-5 flex min-h-0 flex-1 flex-col px-3 max-md:mt-2">
          <div className="mb-1 flex items-center justify-between px-1">
            <span className="text-xs font-medium text-gray-500">会话</span>
            <button
              onClick={() => {
                newChat();
                setDrawerOpen(false);
              }}
              title="新建对话"
              className="rounded-md px-1.5 text-sm text-gray-400 transition hover:bg-[#1a1f29] hover:text-indigo-300"
            >
              ＋
            </button>
          </div>
          <div className="min-h-0 flex-1 space-y-0.5 overflow-y-auto pb-2 max-md:min-h-[9rem]">
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
                    activeId === c.chat_id && page === "chat"
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
                      onClick={() => void openChat(c.chat_id)}
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
