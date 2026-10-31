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

/** 移动端 / 窄窗轮次导航：消息区右下角「轮次 N/M」chip（不贴屏幕边，避开系统
 *  手势区与「回到底部」钮），点击弹底部 sheet——逐轮序号 + 预览 + 时间，当前轮
 *  高亮并自动滚入视口；点选跳转并收起，遮罩点击 / 系统返回键收起。行高 ≥44px
 *  （触控目标下限）。 */
export function RoundFab({
  rounds,
  activeId,
  onJump,
}: {
  rounds: RoundNavItem[];
  activeId: number | null;
  onJump: (id: number) => void;
}) {
  const [open, setOpen] = useState(false);
  const listRef = useRef<HTMLDivElement | null>(null);
  useBackClose(open, () => setOpen(false));
  const activeIdx = rounds.findIndex((r) => r.id === activeId);
  useEffect(() => {
    if (!open) return;
    listRef.current
      ?.querySelector<HTMLElement>("[data-active-round]")
      ?.scrollIntoView({ block: "center" });
  }, [open]);
  return (
    <>
      <button
        onClick={() => setOpen(true)}
        aria-label="轮次导航（按用户提问跳转，仅已加载消息）"
        title="按轮次跳转（仅已加载消息）"
        className="absolute bottom-3 right-3 z-20 flex h-11 items-center gap-1.5 rounded-full border border-[#2a2f3a] bg-[#141821]/95 px-3.5 text-xs text-gray-300 shadow-lg backdrop-blur transition hover:border-indigo-500/50 hover:text-indigo-300"
      >
        <span aria-hidden className="text-[13px] leading-none">☰</span>
        <span>
          {activeIdx >= 0 ? activeIdx + 1 : "–"}/{rounds.length}
        </span>
      </button>
      {open && (
        // 遮罩/面板挂在消息列表 wrapper 内（absolute inset-0）= 只覆盖消息区的
        // 底部面板：输入区保持可用（浏览轮次时还能继续打字），也不引入 fixed
        // 全屏层。要全屏 sheet 时改 fixed 即可。
        <div
          className="absolute inset-0 z-30 flex flex-col justify-end bg-black/40"
          onClick={() => setOpen(false)}
        >
          <div
            className="mx-2 mb-2 overflow-hidden rounded-2xl border border-[#2a2f3a] bg-[#141821] shadow-2xl"
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
                      setOpen(false);
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
      )}
    </>
  );
}
