import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
} from "react";
import { api } from "../lib/api";
import { PanelRightIcon } from "./icons";

/** 标题栏右侧统一的图标按钮（主题切换 / 窗控三键）。 */
function BarButton({
  title,
  onClick,
  danger,
  children,
}: {
  title: string;
  onClick: () => void;
  danger?: boolean;
  children: ReactNode;
}) {
  return (
    <button
      title={title}
      onClick={onClick}
      className={`flex h-full w-11 items-center justify-center text-gray-400 transition hover:text-gray-200 ${
        danger ? "hover:bg-red-600 hover:text-white" : "hover:bg-[#1a1f29]"
      }`}
    >
      {children}
    </button>
  );
}

const icon = {
  width: 16,
  height: 16,
  viewBox: "0 0 16 16",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 1.3,
  strokeLinecap: "round",
  strokeLinejoin: "round",
} as const;

/** 自绘标题栏（窗口 decorations: false）：左侧轮播标语，右侧任务面板开关 + 主题切换 + 窗控三键。
 * 整条空白处可拖拽移动；双击最大化用 mousedown 时间差判定——start_dragging
 * 进入系统原生移动循环后 webview 收不到 dblclick 事件。 */
export default function TitleBar({
  title,
  theme,
  taskPanelOpen,
  onToggleTaskPanel,
  onToggleTheme,
}: {
  title: string;
  theme: "dark" | "light";
  taskPanelOpen: boolean;
  onToggleTaskPanel: () => void;
  onToggleTheme: () => void;
}) {
  const [maximized, setMaximized] = useState(false);
  const lastPress = useRef(0);

  // 最大化状态：启动查一次；窗口尺寸变化（含贴边分屏 / 双击标题栏）后重查。
  useEffect(() => {
    let timer: number | undefined;
    const sync = () => void api.windowMaximized().then(setMaximized).catch(() => {});
    sync();
    const onResize = () => {
      window.clearTimeout(timer);
      timer = window.setTimeout(sync, 200);
    };
    window.addEventListener("resize", onResize);
    return () => {
      window.removeEventListener("resize", onResize);
      window.clearTimeout(timer);
    };
  }, []);

  const onBarMouseDown = useCallback((e: ReactMouseEvent) => {
    if (e.button !== 0 || (e.target as HTMLElement).closest("button")) return;
    const now = Date.now();
    if (now - lastPress.current < 450) {
      lastPress.current = 0;
      void api.windowToggleMaximize().then(setMaximized).catch(() => {});
      return;
    }
    lastPress.current = now;
    void api.windowStartDrag().catch(() => {});
  }, []);

  const toggleMax = useCallback(() => {
    void api.windowToggleMaximize().then(setMaximized).catch(() => {});
  }, []);

  return (
    <div
      onMouseDown={onBarMouseDown}
      className="topbar-bg flex h-9 shrink-0 cursor-default select-none items-center border-b border-[#2a2f3a] bg-[#12151d]"
    >
      <span className="min-w-0 flex-1 truncate pl-4 text-xs text-gray-400" title={title}>
        {title}
      </span>
      <div className="flex h-full items-center">
        <BarButton title={taskPanelOpen ? "收起任务面板" : "展开任务面板"} onClick={onToggleTaskPanel}>
          <PanelRightIcon active={taskPanelOpen} />
        </BarButton>
        <BarButton
          title={theme === "dark" ? "切换到浅色主题" : "切换到深色主题"}
          onClick={onToggleTheme}
        >
          {theme === "dark" ? (
            <svg {...icon}>
              <circle cx="8" cy="8" r="2.7" />
              <path d="M8 1.5v1.7M8 12.8v1.7M1.5 8h1.7M12.8 8h1.7M3.4 3.4l1.2 1.2M11.4 11.4l1.2 1.2M12.6 3.4l-1.2 1.2M4.6 11.4l-1.2 1.2" />
            </svg>
          ) : (
            <svg {...icon}>
              <path d="M13.5 9.7A5.6 5.6 0 0 1 6.3 2.5a5.6 5.6 0 1 0 7.2 7.2z" />
            </svg>
          )}
        </BarButton>
        <BarButton title="最小化" onClick={() => void api.windowMinimize().catch(() => {})}>
          <svg {...icon}>
            <path d="M3.5 8h9" />
          </svg>
        </BarButton>
        <BarButton title={maximized ? "还原" : "最大化"} onClick={toggleMax}>
          {maximized ? (
            <svg {...icon}>
              <rect x="3" y="5.5" width="7.5" height="7.5" rx="1" />
              <path d="M5.5 3h7.5v7.5" />
            </svg>
          ) : (
            <svg {...icon}>
              <rect x="3.5" y="3.5" width="9" height="9" rx="1" />
            </svg>
          )}
        </BarButton>
        <BarButton
          danger
          title="关闭（隐藏到托盘）"
          onClick={() => void api.windowClose().catch(() => {})}
        >
          <svg {...icon}>
            <path d="M4.2 4.2l7.6 7.6M11.8 4.2l-7.6 7.6" />
          </svg>
        </BarButton>
      </div>
    </div>
  );
}
