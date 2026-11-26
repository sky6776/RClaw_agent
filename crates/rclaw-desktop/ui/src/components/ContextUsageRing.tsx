import React, { useEffect, useLayoutEffect, useRef, useState } from "react";

import { useBackClose } from "../lib/backNav";
import type { UsageSnapshot } from "../lib/api";

function formatTokens(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return "0";
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}K`;
  return String(Math.round(value));
}

function ringColor(ratio: number): string {
  if (ratio >= 0.85) return "text-red-400";
  if (ratio >= 0.6) return "text-amber-400";
  return "text-indigo-400";
}

/**
 * 上下文使用率环（参考 ZCode 的 chat-context-usage 触发器）。
 *
 * - 24×24 viewBox、半径 10、线宽 4：底环 25% 透明，进度环 dasharray/dashoffset
 *   从 12 点方向顺时针增长（`-rotate-90`）。
 * - 点击弹出“上下文容量”：已用/总量/百分比、进度条，以及「历史消息 / 系统与工具」
 *   两个来源拆分（RClaw 的 usage_snapshot 提供 history_used 与 request_used）。
 * - 数据由 App 每 5s 轮询 `usage_snapshot` 传入；没有快照时整体隐藏。
 */
export default function ContextUsageRing({ usage }: { usage: UsageSnapshot | null }) {
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<{
    left: number;
    top?: number;
    bottom?: number;
    width: number;
    maxHeight: number;
  } | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  useBackClose(open, () => setOpen(false));

  // 弹层固定在视口内：优先向上展开，上方空间不足时改向下；左右各留 8px，窄屏
  // 收窄到视口宽度。监听 window 与 visualViewport（安卓软键盘只改后者）。
  useLayoutEffect(() => {
    if (!open) return;
    const update = () => {
      const rect = rootRef.current?.getBoundingClientRect();
      if (!rect) return;
      const vv = window.visualViewport;
      const viewportLeft = vv?.offsetLeft ?? 0;
      const viewportTop = vv?.offsetTop ?? 0;
      const viewportWidth = vv?.width ?? window.innerWidth;
      const viewportHeight = vv?.height ?? window.innerHeight;
      const width = Math.max(0, Math.min(256, viewportWidth - 16));
      // 锚点与可视视口求交：锚点完全不可见（或被软键盘压到屏外）时不再按
      // 锚点坐标推算方向，直接隐藏；监听器会在锚点重新可见时恢复。
      const anchorVisible =
        rect.bottom > viewportTop &&
        rect.top < viewportTop + viewportHeight &&
        rect.right > viewportLeft &&
        rect.left < viewportLeft + viewportWidth;
      if (!anchorVisible || width < 80) {
        setPos({
          left: viewportLeft + 8,
          top: viewportTop + 8,
          width,
          maxHeight: 0,
        });
        return;
      }
      const left = Math.min(
        Math.max(viewportLeft + 8, rect.right - width),
        Math.max(viewportLeft + 8, viewportLeft + viewportWidth - width - 8),
      );
      const above = rect.top - viewportTop - 12;
      const below = viewportTop + viewportHeight - rect.bottom - 12;
      const openUp = above >= below;
      const maxHeight = Math.max(
        0,
        Math.min(viewportHeight * 0.7, openUp ? above : below),
      );
      if (openUp) {
        setPos({ left, bottom: window.innerHeight - rect.top + 8, width, maxHeight });
      } else {
        setPos({ left, top: rect.bottom + 8, width, maxHeight });
      }
    };
    update();
    const vv = window.visualViewport;
    window.addEventListener("resize", update);
    window.addEventListener("scroll", update, true);
    vv?.addEventListener("resize", update);
    vv?.addEventListener("scroll", update);
    return () => {
      window.removeEventListener("resize", update);
      window.removeEventListener("scroll", update, true);
      vv?.removeEventListener("resize", update);
      vv?.removeEventListener("scroll", update);
    };
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: MouseEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onPointerDown);
    return () => document.removeEventListener("mousedown", onPointerDown);
  }, [open]);

  if (!usage || usage.context_length <= 0) return null;

  const used = Math.max(0, usage.request_used);
  const total = usage.context_length;
  // 文字显示真实估算比值（可能 >100%）；圆环和进度条才钳到 0..1。
  const rawRatio = total > 0 ? used / total : 0;
  const ratio = Math.min(Math.max(rawRatio, 0), 1);
  const percent = (rawRatio * 100).toFixed(1).replace(/\.0$/, "");
  const radius = 10;
  const circumference = 2 * Math.PI * radius;
  const history = Math.min(Math.max(usage.history_used, 0), used);
  const overhead = Math.max(used - history, 0);
  const historyPercent = total > 0 ? (history / total) * 100 : 0;
  const overheadPercent = total > 0 ? (overhead / total) * 100 : 0;

  return (
    <div ref={rootRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        title={`上下文已用 ${formatTokens(used)} / 总量 ${formatTokens(total)}（${percent}%）`}
        aria-label={`上下文已用 ${formatTokens(used)} / 总量 ${formatTokens(total)}（${percent}%）`}
        aria-haspopup="dialog"
        aria-expanded={open}
        className={`flex h-9 w-9 items-center justify-center rounded-lg transition hover:bg-[#1a2030] ${ringColor(ratio)}`}
      >
        <svg
          aria-hidden="true"
          focusable="false"
          viewBox="0 0 24 24"
          className="h-4 w-4 -rotate-90"
        >
          <circle
            cx="12"
            cy="12"
            r={radius}
            fill="none"
            opacity="0.25"
            stroke="currentColor"
            strokeWidth="4"
          />
          <circle
            cx="12"
            cy="12"
            r={radius}
            fill="none"
            opacity="0.9"
            stroke="currentColor"
            strokeDasharray={`${circumference} ${circumference}`}
            strokeDashoffset={circumference * (1 - ratio)}
            strokeLinecap="round"
            strokeWidth="4"
          />
        </svg>
      </button>
      {open && pos !== null && pos.maxHeight >= 24 && pos.width >= 80 && (
        <div
          role="dialog"
          aria-label="上下文容量"
          style={
            pos
              ? {
                  position: "fixed",
                  left: pos.left,
                  top: pos.top,
                  bottom: pos.bottom,
                  width: pos.width,
                  maxHeight: pos.maxHeight,
                }
              : {
                  position: "fixed",
                  left: 8,
                  bottom: 56,
                  width: Math.min(256, window.innerWidth - 16),
                }
          }
          className="z-50 overflow-y-auto rounded-xl border border-[#2a2f3a] bg-[#161a22] p-3 text-xs shadow-2xl"
        >
          <div className="mb-2 flex items-center gap-3">
            <span className="text-[13px] font-medium text-gray-200">上下文容量</span>
            <span className="ml-auto font-mono text-[11px] text-gray-400">
              {formatTokens(used)}/{formatTokens(total)}（{percent}%）
            </span>
          </div>
          <div className="h-2 overflow-hidden rounded-full bg-[#232937]">
            <div
              className={`h-full rounded-full ${
                ratio >= 0.85
                  ? "bg-red-400"
                  : ratio >= 0.6
                    ? "bg-amber-400"
                    : "bg-indigo-400"
              }`}
              style={{ width: `${Math.max(ratio * 100, 1)}%` }}
            />
          </div>
          <div className="mt-2 space-y-1">
            <div className="flex items-center gap-2 text-gray-400">
              <span className="h-2 w-2 shrink-0 rounded-sm bg-indigo-400" />
              <span className="min-w-0 truncate">历史消息</span>
              <span className="ml-auto shrink-0 font-mono text-gray-300">
                {formatTokens(history)}（{historyPercent.toFixed(1)}%）
              </span>
            </div>
            <div className="flex items-center gap-2 text-gray-400">
              <span className="h-2 w-2 shrink-0 rounded-sm bg-sky-400" />
              <span className="min-w-0 truncate">系统与工具</span>
              <span className="ml-auto shrink-0 font-mono text-gray-300">
                {formatTokens(overhead)}（{overheadPercent.toFixed(1)}%）
              </span>
            </div>
          </div>
          <p className="mt-2 border-t border-[#252b36] pt-2 text-[10px] leading-relaxed text-gray-500">
            提示词、工具调用和回复都会共享上下文窗口。容量是启动期预算（切换模型/重启后更新），数值为估算。
          </p>
        </div>
      )}
    </div>
  );
}
