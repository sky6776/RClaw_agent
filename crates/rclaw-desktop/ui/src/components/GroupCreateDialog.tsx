/** 新建群聊对话框（2–6 个 bot）。
 *
 * 设置「智能体」与侧栏 `＋ ▾ → 发起群聊` 共用；创建成功后由调用方刷新 rooms 并打开房间。
 */
import { useState } from "react";
import { api, type BotEntry, type GroupRoom } from "../lib/api";

const BTN =
  "rounded-lg border border-[#2a2f3a] bg-[#1a1f29] px-3 py-1.5 text-xs text-gray-300 transition hover:border-indigo-500/50 hover:text-indigo-200 disabled:cursor-not-allowed disabled:opacity-40";
const BTN_PRIMARY =
  "rounded-lg bg-indigo-500/80 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-40";
const INPUT =
  "w-full rounded-lg border border-[#2a2f3a] bg-[#141821] px-3 py-1.5 text-xs text-gray-200 outline-none transition focus:border-indigo-500/60";

export default function GroupCreateDialog({
  bots,
  onClose,
  onCreated,
}: {
  bots: BotEntry[];
  onClose: () => void;
  onCreated: (room: GroupRoom) => void;
}) {
  const [name, setName] = useState("");
  const [members, setMembers] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const create = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const room = await api.groupCreate(name.trim(), members);
      onCreated(room);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      className="fixed bottom-0 left-0 right-0 top-0 z-50 flex items-center justify-center bg-black/50"
      onClick={onClose}
    >
      <div
        role="dialog"
        tabIndex={-1}
        aria-modal="true"
        aria-label="新建群聊"
        className="mx-4 w-[28rem] max-w-[calc(100vw-2rem)] rounded-xl border border-[#2a2f3a] bg-[#141821] p-4 shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-center justify-between">
          <span className="text-sm font-semibold text-gray-100">新建群聊</span>
          <button
            className="text-xs text-gray-500 hover:text-gray-300"
            onClick={onClose}
            disabled={busy}
          >
            关闭
          </button>
        </div>
        <label className="flex flex-col gap-1 text-[11px] text-gray-400">
          群聊名
          <input
            autoFocus
            className={INPUT}
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="投研小组"
            maxLength={40}
            disabled={busy}
          />
        </label>
        <div className="mt-3 text-[11px] text-gray-400">选择成员（2–6 个）</div>
        <div className="mt-1 flex max-h-56 flex-wrap gap-2 overflow-y-auto">
          {bots.map((b) => {
            const picked = members.includes(b.name);
            return (
              <button
                key={b.name}
                disabled={busy}
                className={`rounded-lg border px-2 py-1 text-xs transition ${
                  picked
                    ? "border-indigo-500/60 bg-indigo-500/10 text-indigo-200"
                    : "border-[#2a2f3a] bg-[#1a1f29] text-gray-400 hover:text-gray-200"
                }`}
                onClick={() =>
                  setMembers((prev) =>
                    picked ? prev.filter((m) => m !== b.name) : [...prev, b.name],
                  )
                }
              >
                🤖 {b.title} @{b.name}
              </button>
            );
          })}
          {bots.length === 0 && (
            <div className="px-1 py-2 text-xs text-gray-500">
              还没有智能体，先在设置 → 智能体里创建。
            </div>
          )}
        </div>
        {error && <div className="mt-2 text-xs text-red-400">{error}</div>}
        <div className="mt-3 flex justify-end gap-2">
          <button className={BTN} onClick={onClose} disabled={busy}>
            取消
          </button>
          <button
            className={BTN_PRIMARY}
            disabled={busy || name.trim() === "" || members.length < 2 || members.length > 6}
            onClick={() => void create()}
          >
            {busy ? "创建中…" : "创建"}
          </button>
        </div>
      </div>
    </div>
  );
}
