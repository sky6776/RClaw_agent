import React, { useEffect, useLayoutEffect, useRef, useState } from "react";

import { useBackClose } from "../lib/backNav";
import type { LlmState } from "../lib/api";

/** 档位短标签：窄屏底栏只放 1 个汉字，完整文案进无障碍标签。 */
function shortEffort(value: string | null): string {
  switch (value) {
    case "disabled":
      return "关";
    case "low":
      return "低";
    case "high":
      return "高";
    case "max":
      return "最";
    default:
      return "默认";
  }
}

/**
 * 模型短名：底部按钮在安卓窄屏要同时容纳模型 + 档位，`deepseek-flash` 太长。
 * 完整名称仍在菜单、title 与 aria-label 里。
 */
function shortModelLabel(model: string): string {
  switch (model) {
    case "glm-5.3-flash":
      return "GLM-5.3F";
    case "glm-5.4":
      return "GLM-5.4";
    case "deepseek-flash":
      return "dp-flash";
    default:
      return model.startsWith("deepseek-")
        ? `dp-${model.slice("deepseek-".length)}`
        : model;
  }
}

/**
 * 会话页「模型 + 思考强度」合并控件（参考 ZCode 的底部模型选择）。
 *
 * - 一个按钮同时显示当前模型与思考档位，点开先选模型、再选该模型的思考强度；
 *   底栏空间紧张时不额外占位（合并成一个控件）。
 * - 选项/可选性完全由后端 `chat_llm_state` 给出（按 live provider 能力族 + 精确模型），
 *   前端不自己按模型名猜。
 * - 提交带 expected provider/model：菜单打开后模型被另一窗口切走时，后端拒绝旧档位。
 * - 桌面挂在「收放思考块」按钮右侧，安卓挂在轮次跳转按钮右侧（由 Chat 决定位置）。
 */
export default function ModelThinkingPicker({
  state,
  busy,
  onModelSelect,
  onThinkingSelect,
  onOpen,
}: {
  state: LlmState;
  busy?: boolean;
  onModelSelect: (model: string, expected: LlmState) => Promise<void>;
  onThinkingSelect: (value: string, expected: LlmState) => Promise<void>;
  onOpen?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [pending, setPending] = useState(false);
  const [pos, setPos] = useState<{
    left: number;
    top?: number;
    bottom?: number;
    width: number;
    maxHeight: number;
  } | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  useBackClose(open, () => setOpen(false));

  // 菜单固定在视口内：与按钮右缘对齐、向左展开；窄屏收窄。上方空间不足时
  // 改向下展开；两侧都紧张时按可用空间限高（菜单自身滚动）。监听 window 与
  // visualViewport（安卓软键盘只改后者）。
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

  // 会话进入忙碌时关掉菜单：进行中的 turn 已冻结旧 provider，此时提交只会让用户
  // 误以为本 turn 生效。
  useEffect(() => {
    if (busy) setOpen(false);
  }, [busy]);

  if (!state.model.supported) return null;

  const currentModel =
    state.model.models.find((option) => option.model === state.model.model) ?? null;
  const fullModelLabel = currentModel?.label ?? state.model.model;
  const displayModelLabel = shortModelLabel(state.model.model);
  const effortLabel = state.thinking.supported
    ? (state.thinking.options.find((option) => option.value === state.thinking.current)
        ?.label ?? "默认")
    : "";
  const effortOptions = state.thinking.supported ? state.thinking.options : [];

  return (
    <div ref={rootRef} className="relative">
      <button
        type="button"
        onClick={() => {
          if (!open) onOpen?.();
          setOpen(!open);
        }}
        disabled={busy || pending}
        title={`模型与思考强度：${fullModelLabel}${effortLabel ? ` · ${effortLabel}` : ""}`}
        aria-label={`模型 ${fullModelLabel}${effortLabel ? `，思考强度 ${effortLabel}` : ""}`}
        aria-haspopup="menu"
        aria-expanded={open}
        className="flex h-10 max-w-[110px] items-center gap-1 rounded-lg px-1.5 text-[10px] text-gray-400 transition hover:bg-[#1a2030] hover:text-indigo-300 disabled:opacity-40"
      >
        <span aria-hidden="true" className="text-[13px] leading-none">
          🧠
        </span>
        <span className="min-w-0 truncate">{displayModelLabel}</span>
        {effortLabel && (
          <span className="shrink-0 rounded bg-[#232937] px-1 py-0.5 text-[10px] leading-none text-gray-300">
            {shortEffort(state.thinking.current)}
          </span>
        )}
      </button>
      {open && pos !== null && pos.maxHeight >= 24 && pos.width >= 80 && (
        <div
          role="menu"
          aria-label="模型与思考强度"
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
                  maxHeight: window.innerHeight * 0.7,
                }
          }
          className="z-50 overflow-y-auto rounded-xl border border-[#2a2f3a] bg-[#161a22] py-1 shadow-2xl"
        >
          <div className="px-3 py-1 text-[10px] uppercase tracking-wide text-gray-500">模型</div>
          {state.model.models.map((option) => {
            const selected = option.model === state.model.model;
            return (
              <button
                key={option.model}
                type="button"
                role="menuitemradio"
                aria-checked={selected}
                disabled={busy || pending}
                onClick={async () => {
                  if (selected) return;
                  setPending(true);
                  try {
                    await onModelSelect(option.model, state);
                  } finally {
                    setPending(false);
                  }
                }}
                className={`flex w-full items-center justify-between px-3 py-2 text-left transition ${
                  selected
                    ? "bg-indigo-500/10 text-indigo-200"
                    : "text-gray-300 hover:bg-[#1a2030] hover:text-gray-100"
                } disabled:opacity-50`}
              >
                <span className="min-w-0">
                  <span className="block truncate text-sm">{option.label}</span>
                  <span className="block truncate text-[10px] text-gray-500">
                    {option.model}
                  </span>
                </span>
                {selected && <span aria-hidden="true">✓</span>}
              </button>
            );
          })}
          {effortOptions.length > 1 && (
            <>
              <div className="my-1 border-t border-[#252b36]" />
              <div className="px-3 py-1 text-[10px] uppercase tracking-wide text-gray-500">
                思考强度
              </div>
              {effortOptions.map((option) => {
                const selected = option.value === state.thinking.current;
                return (
                  <button
                    key={option.value}
                    type="button"
                    role="menuitemradio"
                    aria-checked={selected}
                    disabled={busy || pending}
                    onClick={async () => {
                      if (selected) {
                        setOpen(false);
                        return;
                      }
                      setPending(true);
                      try {
                        await onThinkingSelect(option.value, state);
                        setOpen(false);
                      } finally {
                        setPending(false);
                      }
                    }}
                    className={`flex w-full items-center justify-between px-3 py-2 text-left text-sm transition ${
                      selected
                        ? "bg-indigo-500/10 text-indigo-200"
                        : "text-gray-300 hover:bg-[#1a2030] hover:text-gray-100"
                    } disabled:opacity-50`}
                  >
                    <span>{option.label}</span>
                    {selected && <span aria-hidden="true">✓</span>}
                  </button>
                );
              })}
            </>
          )}
        </div>
      )}
    </div>
  );
}
