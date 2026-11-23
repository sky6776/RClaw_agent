import { useCallback, useEffect, useRef, useState } from "react";
import { api, type SyncTrashItem, type SyncTrashMemory } from "../lib/api";
import {
  SYNC_BTN_SMALL,
  SYNC_CARD,
  SYNC_HINT,
  formatAgo,
  formatBytes,
} from "../lib/syncUi";
import { SyncCardHead, SyncConfirm } from "./SyncBits";

const REMOVE_BTN =
  "rounded-md border border-[#2a2f3a] px-2 py-1 text-[11px] text-gray-400 transition hover:border-red-500/50 hover:text-red-300 disabled:opacity-40";
const ROW = "rounded-lg border border-[#2a2f3a] px-3 py-2";

/** 提示里最多列出这么多条没放回去的记忆。 */
const REJECTED_SHOWN = 3;

type Removing =
  | { kind: "session"; item: SyncTrashItem }
  | { kind: "memory"; item: SyncTrashMemory };

/**
 * 回收站：别的设备删掉的、以及在这台设备上删掉的已同步会话，先在这里留若干天；因为同步而
 * 从这台设备的记忆文件里删掉的条目也留在这里。
 *
 * 「恢复」把会话作为一个新会话放回来，「放回」把记忆条目写回原来的记忆文件；同步开着的话，
 * 两者随后都会重新同步给其他设备。回收站只在这台设备上，不依赖同步开没开：关闭同步、退出
 * 同步空间之后，里面的东西照样看得到、恢复得了。
 */
export default function SyncTrash({
  refresh,
  trashDays,
  quiet,
  onError,
  onNotice,
  onOpenChat,
}: {
  /** 变一下就重新读列表（面板在收到同步事件时递增它）。 */
  refresh: number;
  trashDays: number;
  /** 这台设备现在不在同步（没开、还没创建或加入空间、引擎没起来）：回收站是空的就整个
   *  不显示，有东西才显示。 */
  quiet: boolean;
  onError: (message: string) => void;
  onNotice: (text: string) => void;
  onOpenChat: (chatId: string) => void;
}) {
  const [sessions, setSessions] = useState<SyncTrashItem[]>([]);
  const [memory, setMemory] = useState<SyncTrashMemory[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [removing, setRemoving] = useState<Removing | null>(null);
  const [viewing, setViewing] = useState<string | null>(null);
  const seq = useRef(0);

  const load = useCallback(async () => {
    const mine = ++seq.current;
    try {
      const next = await api.syncTrashList();
      if (mine !== seq.current) return;
      setSessions(next.sessions);
      setMemory(next.memory);
    } catch {
      // 列表读不出来不打扰用户：下一次刷新再试。
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load, refresh]);

  const restore = async (item: SyncTrashItem) => {
    setBusy(item.id);
    try {
      const imported = await api.syncTrashRestore(item.id);
      await load();
      if (imported.channel === "desktop") {
        onOpenChat(imported.chat_id);
      } else {
        // 不是桌面通道的会话（比如别的入口产生的）：这里的会话列表里没有它，说一声。
        onNotice(
          `「${item.title || item.chat_id}」已经恢复成一个新会话。它属于「${imported.channel}」通道，不在这里的会话列表里。`,
        );
      }
    } catch (e) {
      onError(String(e));
      void load();
    } finally {
      setBusy(null);
    }
  };

  const restoreMemory = async (item: SyncTrashMemory) => {
    setBusy(item.id);
    try {
      const result = await api.syncTrashMemoryRestore(item.id);
      if (result.rejected.length === 0) {
        onNotice(
          `${result.restored} 条记忆已经放回${result.into}。同步开着的话，它们随后会回到其他设备上。`,
        );
      } else {
        const shown = result.rejected
          .slice(0, REJECTED_SHOWN)
          .map(([preview, reason]) => `「${preview}」：${reason}`)
          .join("；");
        const more = result.rejected.length > REJECTED_SHOWN ? " 等" : "";
        onNotice(
          `${result.restored} 条记忆已经放回${result.into}；另有 ${result.rejected.length} 条没有通过这台设备的检查，没有放回（${shown}${more}）。这一份还留在回收站里，可以查看。`,
        );
      }
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(null);
      void load();
    }
  };

  const remove = async (target: Removing) => {
    setBusy(target.item.id);
    try {
      if (target.kind === "session") await api.syncTrashRemove(target.item.id);
      else await api.syncTrashMemoryRemove(target.item.id);
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(null);
      setRemoving(null);
      void load();
    }
  };

  const total = sessions.length + memory.length;
  if (quiet && total === 0) return null;

  return (
    <div className={SYNC_CARD}>
      <SyncCardHead title={total ? `回收站（${total}）` : "回收站"} />
      <p className={`mb-2 ${SYNC_HINT}`}>
        {trashDays <= 0
          ? "回收站现在设成了不保留：被同步删掉的会话和记忆条目不会留在这里。"
          : quiet
            ? `这台设备参与同步期间被删掉的会话和记忆条目留在这里，从删除那天起保留 ${trashDays} 天。现在没有在同步，也照样可以恢复。`
            : `已经同步过的会话被删除时（不管在哪台设备上删的），这台设备先在这里留一份；因为同步而从这台设备的记忆里删掉的条目也留在这里。保留 ${trashDays} 天。「彻底删除」的会话不进回收站。`}
      </p>
      {total === 0 && <div className={SYNC_HINT}>现在是空的。</div>}
      <div className="space-y-2">
        {sessions.map((item) => (
          <div key={item.id} className={`flex items-center justify-between gap-3 ${ROW}`}>
            <div className="min-w-0">
              <div className="truncate text-xs text-gray-200">{item.title || item.chat_id}</div>
              <div className="mt-0.5 text-[10px] text-gray-500">
                {formatAgo(item.deleted_at_ms)}删除 · {item.messages} 条消息 · {formatBytes(item.size)}
              </div>
            </div>
            <div className="flex shrink-0 items-center gap-2">
              <button
                disabled={busy !== null}
                onClick={() => void restore(item)}
                className={SYNC_BTN_SMALL}
              >
                {busy === item.id ? "请稍候…" : "恢复"}
              </button>
              <button
                disabled={busy !== null}
                onClick={() => setRemoving({ kind: "session", item })}
                className={REMOVE_BTN}
              >
                清除
              </button>
            </div>
          </div>
        ))}
        {memory.map((item) => (
          <div key={item.id} className={ROW}>
            <div className="flex items-center justify-between gap-3">
              <div className="min-w-0">
                <div className="break-words text-xs text-gray-200">
                  {item.what || "被同步删掉的记忆条目"}
                </div>
                <div className="mt-0.5 text-[10px] text-gray-500">
                  {formatAgo(item.deleted_at_ms)}删除 · {item.entries.length} 条记忆
                </div>
              </div>
              <div className="flex shrink-0 items-center gap-2">
                <button
                  onClick={() => setViewing(viewing === item.id ? null : item.id)}
                  className={SYNC_BTN_SMALL}
                >
                  {viewing === item.id ? "收起" : "查看"}
                </button>
                {item.target && (
                  <button
                    disabled={busy !== null}
                    onClick={() => void restoreMemory(item)}
                    className={SYNC_BTN_SMALL}
                  >
                    {busy === item.id ? "请稍候…" : "放回"}
                  </button>
                )}
                <button
                  disabled={busy !== null}
                  onClick={() => setRemoving({ kind: "memory", item })}
                  className={REMOVE_BTN}
                >
                  清除
                </button>
              </div>
            </div>
            {viewing === item.id && (
              <div className="mt-2 max-h-56 space-y-1 overflow-y-auto rounded-md border border-[#2a2f3a] bg-[#0f131b] px-2 py-1.5">
                {item.entries.map((entry, index) => (
                  <div
                    key={index}
                    className="whitespace-pre-wrap break-words text-[11px] leading-relaxed text-gray-400"
                  >
                    - {entry}
                  </div>
                ))}
                {!item.target && (
                  <div className={SYNC_HINT}>
                    这一份没有记下条目原来在哪个记忆文件里，不能直接放回；需要的话把上面的内容抄回去。
                  </div>
                )}
              </div>
            )}
          </div>
        ))}
      </div>
      {removing && (
        <SyncConfirm
          title="从回收站清除"
          confirmText="清除"
          danger
          busy={busy !== null}
          onConfirm={() => void remove(removing)}
          onCancel={() => setRemoving(null)}
        >
          {removing.kind === "session" ? (
            <p>
              「{removing.item.title || removing.item.chat_id}」在这台设备上留的这一份会被清除，之后不能再恢复。
            </p>
          ) : (
            <p>
              这 {removing.item.entries.length} 条记忆在这台设备上留的这一份会被清除，之后不能再放回。
            </p>
          )}
        </SyncConfirm>
      )}
    </div>
  );
}
