import React, { useEffect, useRef, useState } from "react";
import { useBackClose } from "../lib/backNav";

/** 轮次快捷导航的一个条目（Chat 按用户消息推导，时间已格式化为展示文本）。 */
export interface RoundNavItem {
  /** 用户条目 id：消息行的 data-entry-id 锚点 + 跳转目标。 */
  id: number;
  /** 用户消息首行预览（剥媒体标记，截断）。 */
  preview: string;
  /** 展示用时间文本（无时间戳的极旧条目为 null）。 */
  time: string | null;
}

/** 桌面宽屏（≥864px 且指针精细）判定：rail 形态的开关。pointer: fine 在混合
 *  触屏设备（触屏笔记本）上可能为 false——此时落到 fab 形态，属预期降级。媒体
 *  查询监听带老 WebView83 回退（MediaQueryList 未继承 EventTarget，只有
 *  addListener——与 useIsMobile 同款，直接 addEventListener 在安卓上会挂载即
 *  抛错白屏）。 */
export function useWidePointer(): boolean {
  const QUERY = "(min-width: 864px) and (pointer: fine)";
  const [wide, setWide] = useState(() =>
    typeof window !== "undefined" ? window.matchMedia(QUERY).matches : false,
  );
  useEffect(() => {
    const mq = window.matchMedia(QUERY);
    const onChange = (e: MediaQueryListEvent) => setWide(e.matches);
    if (typeof mq.addEventListener === "function") {
      mq.addEventListener("change", onChange);
      return () => mq.removeEventListener("change", onChange);
    }
    const legacy = (e: MediaQueryList | MediaQueryListEvent) => setWide(e.matches);
    mq.addListener(legacy);
    return () => mq.removeListener(legacy);
  }, []);
  return wide;
}

/** 桌面轮次 rail（对齐 zcode 的 conversation turn navigator 规格）：会话面板左缘
 *  12px 处一条 24px 宽的窄轨，一根 2×12px 横条对应一条用户提问；hover/focus 附近
 *  按距离衰减放大（山峰，最大 2 倍），当前轮常亮；点击平滑滚动到该轮。
 *
 *  - 条目 10px 高、命中区 24×10px（视觉条只有 2px 高，点的是透明按钮）；
 *  - 轮次多时只渲染可视窗口 ± 6 条（固定 10px 行高，自己窗口化）；键盘焦点所在的
 *    那一行即使滚出窗口也保留挂载，别把焦点弄丢；
 *  - 预览是右侧浮层 tooltip（120ms 打开 / 80ms 关闭，可悬停、Esc 关闭），按视口坐标
 *    定位（滚动容器 `overflow-x-hidden` 会把内部浮层裁掉）；
 *  - 轮次 < 2 不渲染（Chat 的 navEnabled 也按同样条件收，rail/fab 一起消失）。 */
export function RoundRail({
  rounds,
  activeId,
  onJump,
}: {
  rounds: RoundNavItem[];
  activeId: number | null;
  onJump: (id: number) => void;
}) {
  const [focusIdx, setFocusIdx] = useState<number | null>(null);
  // 键盘焦点所在的条目：窗口移动时保留它，避免按钮被卸载后焦点掉到 body。
  const [focusedId, setFocusedId] = useState<number | null>(null);
  const [tip, setTip] = useState<{
    id: number;
    text: string;
    time: string | null;
    x: number;
    y: number;
  } | null>(null);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const [view, setView] = useState({ top: 0, height: 0 });
  const activeIdx = rounds.findIndex((r) => r.id === activeId);

  // 窗口化：固定行高 10px，可视窗口上下各多渲染 6 条（zcode 同款 overscan）。
  const ROW = 10;
  const OVERSCAN = 6;
  const total = rounds.length * ROW;
  const from = Math.max(0, Math.floor(view.top / ROW) - OVERSCAN);
  const to = Math.min(rounds.length, Math.ceil((view.top + view.height) / ROW) + OVERSCAN);
  const focusedIdx = focusedId === null ? -1 : rounds.findIndex((r) => r.id === focusedId);
  // 窗口外的焦点行也渲染（绝对定位，不影响布局）。
  const indices: number[] = [];
  for (let i = from; i < to; i++) indices.push(i);
  if (focusedIdx >= 0 && (focusedIdx < from || focusedIdx >= to)) {
    indices.push(focusedIdx);
    indices.sort((a, b) => a - b);
  }

  // 量可视窗口：首次挂载与尺寸变化（窗口缩放、面板收起）时更新。
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const sync = () => setView({ top: el.scrollTop, height: el.clientHeight });
    sync();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(sync);
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  // 当前轮滚进可视窗口（zcode 同样 align: auto：只保证可见，不居中）。行顶就是
  // `index*10`（容器没有纵向内边距），与窗口化用同一个坐标原点。
  useEffect(() => {
    const el = scrollRef.current;
    if (!el || activeIdx < 0 || view.height === 0) return;
    const top = activeIdx * ROW;
    if (top < el.scrollTop) el.scrollTop = top;
    else if (top + ROW > el.scrollTop + el.clientHeight) {
      el.scrollTop = top + ROW - el.clientHeight;
    }
  }, [activeIdx, view.height]);

  // tooltip 计时器：120ms 打开、80ms 关闭；卸载时清掉。
  const openTimer = useRef<number | undefined>(undefined);
  const closeTimer = useRef<number | undefined>(undefined);
  useEffect(
    () => () => {
      window.clearTimeout(openTimer.current);
      window.clearTimeout(closeTimer.current);
    },
    [],
  );
  const clearTip = () => {
    window.clearTimeout(openTimer.current);
    window.clearTimeout(closeTimer.current);
    setTip(null);
  };
  const hover = (i: number) => {
    setFocusIdx(i);
    window.clearTimeout(closeTimer.current);
  };
  const leave = () => {
    setFocusIdx(null);
    window.clearTimeout(openTimer.current);
    window.clearTimeout(closeTimer.current);
    closeTimer.current = window.setTimeout(() => setTip(null), 80);
  };
  const showTip = (el: HTMLElement, item: RoundNavItem) => {
    window.clearTimeout(openTimer.current);
    window.clearTimeout(closeTimer.current);
    // 换到另一行：旧的提示立刻收掉，别让第一行的预览滞留在屏幕上。
    setTip((prev) => (prev !== null && prev.id !== item.id ? null : prev));
    openTimer.current = window.setTimeout(() => {
      const rect = el.getBoundingClientRect();
      setTip({
        id: item.id,
        text: item.preview,
        time: item.time,
        // 右侧浮层；贴近右缘时回收一点，别出视口。
        x: Math.min(rect.right + 8, window.innerWidth - 16),
        y: rect.top + rect.height / 2,
      });
    }, 120);
  };

  // Esc 关闭（悬停出的提示没有焦点在按钮上也收得掉）。
  useEffect(() => {
    if (tip === null) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        window.clearTimeout(openTimer.current);
        window.clearTimeout(closeTimer.current);
        setTip(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [tip]);

  if (rounds.length < 2) return null;
  return (
    <nav aria-label="对话提问导航" className="absolute inset-y-0 left-3 z-20 w-6">
      <div
        ref={scrollRef}
        onScroll={() => {
          const el = scrollRef.current;
          if (el) setView({ top: el.scrollTop, height: el.clientHeight });
          setFocusIdx(null);
          // 滚动时提示立刻收掉（计时器也清）：留着会停在已经移走的那一行上。
          clearTip();
        }}
        onPointerLeave={leave}
        className="absolute top-1/2 max-h-[calc(100%-6rem)] w-6 -translate-y-1/2 overflow-x-hidden overflow-y-auto [scrollbar-width:none]"
      >
        <div
          role="list"
          aria-label="对话提问列表"
          className="rclaw-turn-navigator-enter relative w-6"
          style={{ height: total }}
        >
          {indices.map((i) => {
            const r = rounds[i];
            if (!r) return null;
            const active = i === activeIdx;
            // 山峰视觉：hover/focus 项 2 倍，按距离衰减到 ±3 之外；无指针悬停时
            // 仅当前轮适度放大（1.6）常亮。缩放上限 2（视觉条最长 24px = 轨道宽）。
            const dist =
              focusIdx === null ? Number.POSITIVE_INFINITY : Math.min(3, Math.abs(i - focusIdx));
            const scale =
              dist === 0 ? 2 : dist === 1 ? 1.6 : dist === 2 ? 1.25 : focusIdx === null && active ? 1.6 : 1;
            const opacity =
              dist === 0 ? 1 : dist === 1 ? 0.86 : dist === 2 ? 0.72 : active ? 1 : 0.55;
            return (
              <div
                key={r.id}
                role="listitem"
                aria-posinset={i + 1}
                aria-setsize={rounds.length}
                className="absolute left-0 top-0 h-2.5 w-6"
                style={{ transform: `translateY(${i * ROW}px)` }}
              >
                <button
                  type="button"
                  onClick={() => onJump(r.id)}
                  onMouseEnter={(e) => {
                    hover(i);
                    showTip(e.currentTarget, r);
                  }}
                  onMouseLeave={leave}
                  onFocus={(e) => {
                    hover(i);
                    setFocusedId(r.id);
                    showTip(e.currentTarget, r);
                  }}
                  onBlur={() => {
                    setFocusedId(null);
                    leave();
                  }}
                  aria-label={`跳转到第 ${i + 1} 条提问`}
                  aria-current={active ? "location" : undefined}
                  aria-describedby={tip?.id === r.id ? "rclaw-round-tip" : undefined}
                  className="flex h-2.5 w-6 items-center justify-start rounded-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-indigo-500/50 focus-visible:ring-inset"
                >
                  <span className="block" aria-hidden="true">
                    <span
                      className={`block h-0.5 w-3 origin-left rounded-full transition-[height,opacity,transform,background-color] duration-150 ease-out motion-reduce:transition-none ${
                        active ? "bg-indigo-400" : "bg-gray-500"
                      }`}
                      style={{ transform: `scaleX(${scale})`, opacity }}
                    />
                  </span>
                </button>
              </div>
            );
          })}
        </div>
      </div>
      {tip && (
        <div
          id="rclaw-round-tip"
          role="tooltip"
          onMouseEnter={() => {
            window.clearTimeout(openTimer.current);
            window.clearTimeout(closeTimer.current);
          }}
          onMouseLeave={leave}
          className="fixed z-50 w-max max-w-[16rem] -translate-y-1/2 rounded-md border border-[#2a2f3a] bg-[#0f131b] px-2 py-1 text-[11px] leading-relaxed text-gray-200 shadow-lg"
          style={{ left: tip.x, top: tip.y }}
        >
          <div className="truncate">{tip.text}</div>
          {tip.time && <div className="text-[10px] text-gray-500">{tip.time}</div>}
        </div>
      )}
    </nav>
  );
}

/** 移动端 / 窄窗轮次导航的入口钮：输入框下方工具条内（权限/附件/思考块展开
 *  同排），样式与兄弟钮完全同款（40px 网格 + hover 同款）——浅色/深色主题都走
 *  既有字面量覆盖，不自带深色底。右下角 10px 小计数显示「当前轮/总轮数」（已
 *  加载序号），无 bg 类、不需额外主题覆盖。 */
export function RoundNavButton({
  rounds,
  activeId,
  open,
  onToggle,
  triggerRef,
}: {
  rounds: RoundNavItem[];
  activeId: number | null;
  open: boolean;
  onToggle: () => void;
  /** 面板关闭时焦点返还用：Chat 持有的触发钮 ref。 */
  triggerRef?: React.RefObject<HTMLButtonElement>;
}) {
  const activeIdx = rounds.findIndex((r) => r.id === activeId);
  const count = `${activeIdx >= 0 ? activeIdx + 1 : "–"}/${rounds.length}`;
  const label =
    activeIdx >= 0
      ? `轮次导航（按用户提问跳转，仅已加载消息）：第 ${activeIdx + 1} 条，共 ${rounds.length} 条`
      : `轮次导航（按用户提问跳转，仅已加载消息）：当前轮未定位，共 ${rounds.length} 条`;
  return (
    <button
      ref={triggerRef}
      onClick={onToggle}
      title={label}
      aria-label={label}
      aria-haspopup="dialog"
      aria-expanded={open}
      className={`flex h-10 w-auto shrink-0 items-center justify-center gap-1 rounded-lg px-2 transition hover:bg-[#1a2030] hover:text-indigo-300 ${
        open ? "text-indigo-300" : "text-gray-500"
      }`}
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
        <path d="M4 6h16" />
        <path d="M4 12h16" />
        <path d="M4 18h10" />
      </svg>
      <span aria-hidden className="text-[10px] leading-none tabular-nums">
        {count}
      </span>
    </button>
  );
}

/** 移动端 / 窄窗轮次导航的底部面板：逐轮序号 + 预览 + 时间，当前轮高亮并自动
 *  滚入视口；点选跳转并收起，遮罩点击 / 系统返回键收起。行高 ≥44px（触控目标
 *  下限）。open 状态由 Chat 持有（触发钮在输入区工具条、面板在消息列表
 *  wrapper，分居两个子树）。 */
export function RoundNavSheet({
  open,
  onClose,
  rounds,
  activeId,
  onJump,
}: {
  open: boolean;
  onClose: () => void;
  rounds: RoundNavItem[];
  activeId: number | null;
  onJump: (id: number) => void;
}) {
  const listRef = useRef<HTMLDivElement | null>(null);
  const panelRef = useRef<HTMLDivElement | null>(null);
  useBackClose(open, onClose);
  const activeIdx = rounds.findIndex((r) => r.id === activeId);
  useEffect(() => {
    if (!open) return;
    listRef.current
      ?.querySelector<HTMLElement>("[data-active-round]")
      ?.scrollIntoView({ block: "center" });
    // 打开即把焦点移入面板：读屏用户能顺着 dialog 语义浏览轮次列表；ring 提供
    // 程序化焦点的可见反馈（focus-visible 对程序聚焦不触发，用常显 ring）。
    panelRef.current?.focus();
  }, [open]);
  if (!open) return null;
  return (
    // 遮罩/面板挂在消息列表 wrapper 内（absolute inset-0）= 只覆盖消息区的
    // 底部面板：输入区保持可用（浏览轮次时还能继续打字），也不引入 fixed
    // 全屏层。要全屏 sheet 时改 fixed 即可。非模态（输入区仍可交互），故
    // 不声明 aria-modal。
    <div
      className="absolute inset-0 z-30 flex flex-col justify-end bg-black/40"
      onClick={onClose}
    >
      <div
        ref={panelRef}
        tabIndex={-1}
        role="dialog"
        aria-label="轮次导航"
        className="mx-2 mb-2 overflow-hidden rounded-2xl border border-[#2a2f3a] bg-[#141821] shadow-2xl ring-2 ring-indigo-500/30 focus:outline-none"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="border-b border-[#2a2f3a] px-4 py-2.5 text-xs text-gray-400">
          按轮次跳转（仅已加载消息）
        </div>
        <div ref={listRef} className="max-h-[60vh] overflow-y-auto py-1">
          {rounds.map((r, i) => {
            const active = i === activeIdx;
            return (
              <button
                key={r.id}
                data-active-round={active ? "" : undefined}
                onClick={() => {
                  onClose();
                  onJump(r.id);
                }}
                className={`flex min-h-[44px] w-full items-baseline gap-2.5 px-4 py-2.5 text-left text-sm transition ${
                  active
                    ? "bg-indigo-500/15 text-indigo-200"
                    : "text-gray-300 hover:bg-[#1a2030]"
                }`}
              >
                <span
                  className={`w-8 shrink-0 text-right text-xs ${
                    active ? "text-indigo-300" : "text-gray-500"
                  }`}
                >
                  {i + 1}
                </span>
                <span className="min-w-0 flex-1 truncate">{r.preview}</span>
                {r.time && (
                  <span className="shrink-0 text-[10px] text-gray-500">{r.time}</span>
                )}
              </button>
            );
          })}
        </div>
      </div>
    </div>
  );
}
