import type { ReactNode } from "react";
import type { SyncSessionMark } from "../lib/api";
import { useBackClose } from "../lib/backNav";
import { SYNC_BTN, SYNC_BTN_PRIMARY, SYNC_HINT } from "../lib/syncUi";

/** 应用内确认框。Android WebView 里 window.confirm 不可靠，所以同步相关的确认都走这个。 */
export function SyncConfirm({
  title,
  children,
  confirmText,
  danger = false,
  busy = false,
  onConfirm,
  onCancel,
}: {
  title: string;
  children: ReactNode;
  confirmText: string;
  danger?: boolean;
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  useBackClose(true, () => {
    if (!busy) onCancel();
  });
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
      onClick={() => {
        if (!busy) onCancel();
      }}
    >
      <div
        className="mx-4 max-h-[85vh] w-[26rem] overflow-y-auto rounded-xl border border-[#2a2f3a] bg-[#141821] p-5 shadow-2xl max-md:w-[calc(100%-2rem)]"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="text-sm font-semibold text-gray-100">{title}</h2>
        <div className="mt-2 space-y-2 text-xs leading-relaxed text-gray-400">{children}</div>
        <div className="mt-4 flex justify-end gap-2">
          <button onClick={onCancel} disabled={busy} className={SYNC_BTN}>
            取消
          </button>
          <button
            onClick={onConfirm}
            disabled={busy}
            className={
              danger
                ? "rounded-lg bg-red-600 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-red-500 disabled:opacity-40"
                : SYNC_BTN_PRIMARY
            }
          >
            {busy ? "请稍候…" : confirmText}
          </button>
        </div>
      </div>
    </div>
  );
}

/** 带说明的勾选项（整行可点）。 */
export function SyncCheck({
  checked,
  onChange,
  label,
  hint,
  disabled = false,
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  label: ReactNode;
  hint?: ReactNode;
  disabled?: boolean;
}) {
  return (
    <label
      className={`flex items-start gap-2 ${disabled ? "cursor-default opacity-60" : "cursor-pointer"}`}
    >
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
        className="mt-0.5 h-4 w-4 shrink-0 accent-indigo-500"
      />
      <span className="min-w-0">
        <span className="block text-xs text-gray-200">{label}</span>
        {hint && <span className={`block ${SYNC_HINT}`}>{hint}</span>}
      </span>
    </label>
  );
}

/** 会话列表一行上的同步角标：仅本机 / 正在别的设备上运行 / 已同步。没有可显示的就不渲染。 */
export function SyncMarkTags({ mark }: { mark: SyncSessionMark | undefined }) {
  if (!mark) return null;
  const running = mark.running_on.length > 0;
  if (!mark.local_only && !mark.synced && !running) return null;
  return (
    <span className="flex shrink-0 items-center gap-1">
      {mark.local_only && (
        <span
          title="仅本机：这个会话不同步到其他设备"
          className="rounded border border-[#3a4150] px-1 text-[10px] leading-4 text-gray-400"
        >
          仅本机
        </span>
      )}
      {running && (
        <span
          title={`正在 ${mark.running_on.join("、")} 上运行`}
          className="max-w-[8rem] truncate rounded border border-amber-500/40 bg-amber-500/10 px-1 text-[10px] leading-4 text-amber-300"
        >
          {mark.running_on[0]}
          {mark.running_on.length > 1 ? ` 等 ${mark.running_on.length} 台` : ""}运行中
        </span>
      )}
      {mark.synced && (
        <span title="已同步到你的其他设备" aria-label="已同步" className="text-gray-500">
          <svg
            width="12"
            height="12"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d="M2.5 5.5h10M10.3 3.3l2.2 2.2-2.2 2.2" />
            <path d="M13.5 10.5h-10M5.7 8.3l-2.2 2.2 2.2 2.2" />
          </svg>
        </span>
      )}
    </span>
  );
}

/** 卡片标题行：左边标题，右边放操作。 */
export function SyncCardHead({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="mb-3 flex items-center justify-between gap-2">
      <h2 className="text-sm font-medium text-gray-200">{title}</h2>
      {children && <div className="flex shrink-0 items-center gap-2">{children}</div>}
    </div>
  );
}
