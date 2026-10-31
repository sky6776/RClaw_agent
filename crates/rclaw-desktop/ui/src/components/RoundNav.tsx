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

/** 桌面轮次 rail（zcode ConversationTurnNavigator 同形态的极简版）：列表左缘
 *  一列小横条，一根对应一条用户提问；hover/focus 附近按距离衰减放大（山峰），
 *  当前轮（视口顶最近的上方向锚点）前景色常亮；点击平滑滚动到该轮。命中区
 *  16×32px（视觉条只有 2px 高，点的是透明按钮）；预览走原生 title tooltip。 */
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
  const activeIdx = rounds.findIndex((r) => r.id === activeId);
  return (
    <nav
      aria-label="对话提问导航"
      className="absolute inset-y-0 left-0 z-20 flex w-10 items-center"
    >
      <div className="flex max-h-[calc(100%-2rem)] w-full flex-col items-center gap-0.5 overflow-y-auto py-2 [scrollbar-width:none]">
        {rounds.map((r, i) => {
          const active = i === activeIdx;
          // 山峰视觉：hover/focus 项 2.6 倍，±1/±2 项按距离衰减；无指针悬停时
          // 仅当前轮适度放大（1.6）常亮。
          const dist = focusIdx === null ? Number.POSITIVE_INFINITY : Math.abs(i - focusIdx);
          const scale =
            dist === 0 ? 2.6 : dist === 1 ? 1.7 : dist === 2 ? 1.25 : focusIdx === null && active ? 1.6 : 1;
          const opacity =
            dist === 0 ? 1 : dist === 1 ? 0.86 : dist === 2 ? 0.72 : active ? 1 : 0.55;
          return (
            <button
              key={r.id}
              onClick={() => onJump(r.id)}
              onMouseEnter={() => setFocusIdx(i)}
              onMouseLeave={() => setFocusIdx(null)}
              onFocus={() => setFocusIdx(i)}
              onBlur={() => setFocusIdx(null)}
              title={r.preview}
              aria-label={`跳转到第 ${i + 1} 条提问`}
              aria-current={active ? "location" : undefined}
              className="flex h-4 w-8 shrink-0 items-center justify-center"
            >
              <span
                className={`h-0.5 w-4 origin-left rounded-full transition-[transform,opacity] duration-150 ${
                  active ? "bg-indigo-400" : "bg-gray-500"
                }`}
                style={{ transform: `scaleX(${scale})`, opacity }}
              />
            </button>
          );
        })}
      </div>
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
      className={`relative flex h-10 w-10 shrink-0 items-center justify-center rounded-lg transition hover:bg-[#1a2030] hover:text-indigo-300 ${
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
      <span aria-hidden className="absolute bottom-0 right-0.5 text-[10px] leading-none">
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
