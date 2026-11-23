import { useEffect, useState, useSyncExternalStore } from "react";
import { api, type SyncAttachment } from "../lib/api";
import { formatBytes } from "../lib/syncUi";

/**
 * 多端同步：对话页顶部的一条（docs/multi-device-sync.md §7.4、§8.7）。
 *
 * 两样东西：
 * - 这个会话正在别的设备上运行时的提示；
 * - 这个会话引用的、不在这台设备上的文件，各有一张卡片：
 *   - 还在别的设备上的（remote）：「下载」；
 *   - 同步空间里没有的：向别的设备要了、在等的（asked），原来有它的设备说拿不出来了的
 *     （gone），等了很久没有设备传上来的（lost）。卡片上照实说，不说「会传上来」；
 *     都可以再试一次；
 *   - 取回之后只进了同步缓存的（cached，`media/`、`output/` 下的文件各端会重名，不往
 *     原位置写）：显示为「来自某设备的文件快照」，可以「存进工作区」；
 *   - 本机登记过、文件已经不在的（missing）：可以重新下载；
 *   - 就在工作区原位置上的（local）：不列，消息里照旧按路径显示。刚下载下来的会留在
 *     这里一会儿（到换会话为止），方便直接打开。
 *
 * 清单放在模块级的小仓库里：装载器（App 调用）和这一条（也由 App 渲染）是两个地方，
 * 卡片上的「下载中」之类的状态还要在这一条收起再展开之后留着。
 */

/** 下载请求发出后这么久还没有结果，就不再显示「下载中」（引擎换了连接之类的情况下，
 *  结果事件可能不来）；真的还在下的话，下完照样会刷新。 */
const FETCH_SHOWN_MS = 3 * 60 * 1000;

const IMAGE_RE = /\.(?:png|jpe?g|webp|gif)$/i;

/** 这一条上要显示的附件。对象只在内容变了时才换。 */
interface BarView {
  /** 不在工作区原位置上的（按消息先后；同一个文件被几条消息引用也只列一次）。 */
  pending: SyncAttachment[];
  /** 这个会话开着期间刚下载到工作区原位置上的。 */
  landed: SyncAttachment[];
  sig: string;
}

const NONE: BarView = { pending: [], landed: [], sig: "" };

/** 卡片上的临时状态，按附件对象（blob）记。 */
interface CardState {
  busy: Record<string, true>;
  saving: Record<string, true>;
  errors: Record<string, string>;
  /** 存进工作区之后的相对路径。 */
  saved: Record<string, string>;
}

const noCardState = (): CardState => ({ busy: {}, saving: {}, errors: {}, saved: {} });

/** 清单属于哪个会话；换会话时整份换掉。 */
let chat: string | null = null;
let bar: BarView = NONE;
/** `消息行 id:blob` → 上一份清单里的状态（看得出哪些附件是刚下载下来的）。 */
let known: Record<string, string> = {};
let cards: CardState = noCardState();
/** 让当前会话的清单重新取一遍（装载器在的时候才有）。 */
let reload: (() => void) | null = null;
const busyTimers: Record<string, number> = {};
const listeners = new Set<() => void>();

function emit() {
  listeners.forEach((listener) => listener());
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

const fileKey = (item: SyncAttachment) => `${item.blob}:${item.path}`;

function without<T>(map: Record<string, T>, key: string): Record<string, T> {
  if (!(key in map)) return map;
  const next = { ...map };
  delete next[key];
  return next;
}

function setCards(update: (prev: CardState) => CardState) {
  const next = update(cards);
  if (next === cards) return;
  cards = next;
  emit();
}

/** 换上一份新取的清单。 */
function apply(chatId: string, list: SyncAttachment[]) {
  if (chat !== chatId) return;
  const nextKnown: Record<string, string> = {};
  const pending: SyncAttachment[] = [];
  const pendingKeys = new Set<string>();
  const landed = [...bar.landed];
  list.forEach((item) => {
    // 工具结果全文（artifact）也列出来：它落到同步缓存时，消息里原有的路径句柄指向的
    // 可能是工作区里的另一个同名文件，用户得能从这儿打开同步来的那份准确内容。
    if (item.kind !== "file" && item.kind !== "artifact") return;
    const id = `${item.message_id}:${item.blob}`;
    const before = known[id];
    nextKnown[id] = item.state;
    const key = fileKey(item);
    if (item.state === "local") {
      // 上一份清单里它还不在原位置上：是刚下载下来的。
      const fresh = before !== undefined && before !== "local";
      if (fresh && !landed.some((other) => fileKey(other) === key)) landed.push(item);
      return;
    }
    if (!pendingKeys.has(key)) {
      pendingKeys.add(key);
      pending.push(item);
    }
  });
  known = nextKnown;
  // 下载下来之后又不见了的（被删了）会重新出现在 pending 里：不再算「已下载」。
  const stillLanded = landed.filter((item) => !pendingKeys.has(fileKey(item)));
  const sig = JSON.stringify([pending, stillLanded]);
  if (sig === bar.sig) return;
  bar = { pending, landed: stillLanded, sig };
  emit();
}

/** 一个附件的下载有了结果（或者请求本身没发出去）。 */
function settle(blob: string, error: string | null) {
  const timer = busyTimers[blob];
  if (timer !== undefined) {
    window.clearTimeout(timer);
    delete busyTimers[blob];
  }
  setCards((prev) => {
    const busy = without(prev.busy, blob);
    const errors =
      error === null ? without(prev.errors, blob) : { ...prev.errors, [blob]: error };
    if (busy === prev.busy && errors === prev.errors) return prev;
    return { ...prev, busy, errors };
  });
}

/** 下载一个还在别的设备上的附件。不等结果：结果经 sync 事件回来（见装载器）。 */
function download(blob: string) {
  if (cards.busy[blob]) return;
  setCards((prev) => ({
    ...prev,
    busy: { ...prev.busy, [blob]: true },
    errors: without(prev.errors, blob),
  }));
  window.clearTimeout(busyTimers[blob]);
  busyTimers[blob] = window.setTimeout(() => {
    delete busyTimers[blob];
    setCards((prev) => (prev.busy[blob] ? { ...prev, busy: without(prev.busy, blob) } : prev));
    if (reload) reload();
  }, FETCH_SHOWN_MS);
  api.syncFetchAttachment(blob).catch((e) => settle(blob, String(e)));
}

/** 把同步缓存里的一份文件快照存进工作区。 */
function save(chatId: string, blob: string) {
  if (cards.saving[blob]) return;
  setCards((prev) => ({
    ...prev,
    saving: { ...prev.saving, [blob]: true },
    errors: without(prev.errors, blob),
  }));
  api
    .syncAttachmentSave(chatId, blob)
    .then((rel) => {
      if (chat !== chatId) return;
      setCards((prev) => ({ ...prev, saved: { ...prev.saved, [blob]: rel } }));
    })
    .catch((e) => {
      if (chat !== chatId) return;
      setCards((prev) => ({ ...prev, errors: { ...prev.errors, [blob]: String(e) } }));
    })
    .finally(() => {
      setCards((prev) =>
        prev.saving[blob] ? { ...prev, saving: without(prev.saving, blob) } : prev,
      );
    });
}

/**
 * 装载当前会话的附件清单，并跟着同步事件刷新。对话页开着的时候由 App 调用
 * （`chatId` 为 null = 不在对话页）。没开同步时后端给的是空清单，什么都不显示。
 */
export function useSyncAttachmentLoader(chatId: string | null) {
  useEffect(() => {
    if (!chatId) return;
    if (chat !== chatId) {
      // 换了会话：上一个会话的清单和卡片状态都不要了。同一个会话再进来时先沿用旧的，
      // 等新清单到了再换，不闪一下。
      chat = chatId;
      bar = NONE;
      known = {};
      cards = noCardState();
      emit();
    }
    let alive = true;
    let seq = 0;
    let timer: number | undefined;
    const load = async () => {
      const mine = ++seq;
      let list: SyncAttachment[];
      try {
        list = await api.syncAttachments(chatId);
      } catch {
        // agent 没启动、引擎不可用：当作没有清单，消息照旧按工作区路径显示。
        return;
      }
      if (alive && mine === seq) apply(chatId, list);
    };
    // 一次下载、一轮合入会连着来几条事件：合并成一次查询。
    const schedule = () => {
      window.clearTimeout(timer);
      timer = window.setTimeout(() => void load(), 150);
    };
    reload = schedule;
    void load();
    const un = api.onSync((event) => {
      if (event.kind === "attachment") {
        settle(event.blob, event.error);
        schedule();
      } else if (
        event.kind === "sessions" &&
        event.chats.some((c) => c.channel === "desktop" && c.chat_id === chatId)
      ) {
        // 别的设备的消息合进了这个会话：它们引用的文件多半不在本机。
        schedule();
      }
    });
    return () => {
      alive = false;
      window.clearTimeout(timer);
      if (reload === schedule) reload = null;
      void un.then((fn) => fn());
    };
  }, [chatId]);
}

const CARD =
  "flex items-center gap-2 rounded-xl border border-[#2a2f3a] bg-[#0f131b] px-3 py-2 text-left";
const CARD_ICON = "flex h-9 w-9 shrink-0 items-center justify-center rounded-full";
const CARD_BTN =
  "shrink-0 rounded-lg border border-[#2a2f3a] px-2.5 py-1 text-[11px] text-gray-300 transition hover:border-indigo-500/50 hover:text-indigo-300 disabled:opacity-40";
const CARD_NAME = "block truncate font-mono text-[11px] text-gray-300";
const CARD_NOTE = "block text-[10px] leading-relaxed text-gray-500 [overflow-wrap:anywhere]";
const CARD_ERROR = "px-1 text-[10px] leading-relaxed text-red-300 [overflow-wrap:anywhere]";

/** 不在这台设备上的附件：卡片上怎么说，按钮上写什么。 */
function awayText(state: string, where: string): { note: string; action: string } {
  switch (state) {
    case "remote":
      return { note: `还在${where}上，没有下载`, action: "下载" };
    case "asked":
      return {
        note: "同步空间里还没有这个文件。已经向其他设备要了，有设备传上来之后会自动取回",
        action: "再试一次",
      };
    // 这两种不是定论（别的设备后来也许又有了它）：引擎在打开会话时隔一天再去看一次，
    // 所以说「现在」。
    case "gone":
      return {
        note: "现在取不回来：同步空间里没有这个文件，原来有它的设备说它已经被删除或者改动过了",
        action: "重试",
      };
    case "lost":
      return {
        note: "现在取不回来：同步空间里没有这个文件，向其他设备要了很久也没有设备传上来",
        action: "重试",
      };
    default:
      return { note: "这台设备上已经没有这个文件了", action: "重新下载" };
  }
}

function AttachmentCard({
  chatId,
  item,
  onError,
}: {
  chatId: string;
  item: SyncAttachment;
  onError: (message: string) => void;
}) {
  const state = useSyncExternalStore(subscribe, () => cards);
  const name = item.path.split("/").pop() || item.path;
  const where = item.origin ? `「${item.origin}」` : "别的设备";
  const isImage = IMAGE_RE.test(item.path);
  const error = state.errors[item.blob];
  const open = (path: string) => {
    api.openMedia(path).catch((e) => onError(`打开文件失败: ${String(e)}`));
  };

  if (item.state === "local") {
    return (
      <div className={CARD}>
        <span className={`${CARD_ICON} bg-emerald-500/10 text-emerald-400`}>✓</span>
        <span className="min-w-0 flex-1">
          <span className={CARD_NAME}>{name}</span>
          <span className={CARD_NOTE}>
            {formatBytes(item.size)} · 已下载到 {item.path}
          </span>
        </span>
        {/* 打开只按工作区相对路径：`open_media` 不收绝对路径（安卓与 Windows 都会拒）。
            工具结果（artifact）的登记落点就在这个会话的 artifact 目录下。 */}
        <button onClick={() => open(item.rel_path ?? item.path)} className={CARD_BTN}>
          打开
        </button>
      </div>
    );
  }

  if (item.state === "cached") {
    const savedTo = state.saved[item.blob];
    const saving = Boolean(state.saving[item.blob]);
    return (
      <div>
        {isImage && <SnapshotImage chatId={chatId} blob={item.blob} alt={name} />}
        <div
          className={CARD}
          title="这是从别的设备取回的一份文件快照，放在同步缓存里（这台设备的工作区里可能另有同名文件，所以不往原位置写）。存进工作区之后，助手才能按路径读到它。"
        >
          <span className={`${CARD_ICON} bg-indigo-600 text-white`}>📄</span>
          <span className="min-w-0 flex-1">
            <span className={CARD_NAME}>{name}</span>
            <span className={CARD_NOTE}>
              {formatBytes(item.size)} · 来自{where}的文件快照
              {savedTo ? ` · 已存到 ${savedTo}` : "，存进工作区之后助手才读得到"}
            </span>
          </span>
          {savedTo ? (
            <button onClick={() => open(savedTo)} className={CARD_BTN}>
              打开
            </button>
          ) : (
            <button
              onClick={() => save(chatId, item.blob)}
              disabled={saving}
              className={CARD_BTN}
            >
              {saving ? "保存中…" : "存进工作区"}
            </button>
          )}
        </div>
        {error && (
          <div className="mt-1 flex items-start justify-between gap-2">
            <div className={`min-w-0 flex-1 ${CARD_ERROR}`}>{error}</div>
            {/* 快照核对不过（内容被本机改写过）或其他取用失败：重新下载走缺附件的
                路径，由持有正确内容的设备核对摘要后重传。 */}
            <button onClick={() => download(item.blob)} className={CARD_BTN}>
              重新下载
            </button>
          </div>
        )}
      </div>
    );
  }

  const busy = Boolean(state.busy[item.blob]);
  const away = awayText(item.state, where);
  return (
    <div>
      <div className={CARD}>
        <span className={`${CARD_ICON} bg-[#1a1f29] text-gray-300`}>{isImage ? "🖼" : "📎"}</span>
        <span className="min-w-0 flex-1">
          <span className={CARD_NAME}>{name}</span>
          <span className={CARD_NOTE}>
            {formatBytes(item.size)} · {away.note}
          </span>
        </span>
        <button onClick={() => download(item.blob)} disabled={busy} className={CARD_BTN}>
          {busy ? "下载中…" : away.action}
        </button>
      </div>
      {error && <div className={`mt-1 ${CARD_ERROR}`}>{error}</div>}
    </div>
  );
}

/** 同步缓存里一张图片快照的预览。出不了预览图就不显示，下面的卡片还在。 */
function SnapshotImage({ chatId, blob, alt }: { chatId: string; blob: string; alt: string }) {
  const [src, setSrc] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    setSrc(null);
    api
      .syncAttachmentPreview(chatId, blob)
      .then((r) => {
        if (alive) setSrc(`data:${r.mime};base64,${r.base64}`);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [chatId, blob]);
  if (!src) return null;
  return (
    <img
      src={src}
      alt={alt}
      loading="lazy"
      decoding="async"
      className="mb-1 block max-h-56 w-auto max-w-full rounded-xl border border-[#2a2f3a]"
    />
  );
}

/**
 * 对话页顶部的一条。没有要说的（没开同步、这个会话的文件都在本机、也没在别处运行）时
 * 什么都不渲染，对话页和原来一样。
 */
export function SyncChatBar({
  chatId,
  runningOn,
  onError,
}: {
  chatId: string;
  /** 这个会话正在这些设备上运行（会话角标里来的）。 */
  runningOn: string[] | undefined;
  onError: (message: string) => void;
}) {
  const view = useSyncExternalStore(subscribe, () => (chat === chatId ? bar : NONE));
  const state = useSyncExternalStore(subscribe, () => cards);
  const [open, setOpen] = useState(false);
  // 换会话时收起来。
  useEffect(() => {
    setOpen(false);
  }, [chatId]);

  const running = runningOn !== undefined && runningOn.length > 0;
  const total = view.pending.length + view.landed.length;
  if (!running && total === 0) return null;

  const remote = view.pending.filter((item) => item.state === "remote");
  const idle = remote.filter((item) => !state.busy[item.blob]);
  return (
    <div className="shrink-0 border-b border-[#2a2f3a] bg-[#141821] px-4 py-2 max-md:px-3">
      {running && (
        <div
          className={`rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-1.5 text-xs leading-relaxed text-amber-300${
            total > 0 ? " mb-2" : ""
          }`}
        >
          这个会话正在「{runningOn.join("」「")}」上运行。
          现在在这里接着发消息的话，两边各写各的，同步之后会分成两个分支。
        </div>
      )}
      {total > 0 && (
        <>
          <div className="flex items-center gap-2">
            <button
              onClick={() => setOpen((v) => !v)}
              aria-expanded={open}
              className="flex min-w-0 flex-1 items-center gap-2 text-left text-xs text-gray-300 transition hover:text-gray-100"
            >
              <span className="min-w-0 flex-1">
                {view.pending.length > 0
                  ? `这个会话引用的文件里，有 ${view.pending.length} 个不在这台设备上`
                  : `已下载 ${view.landed.length} 个文件`}
                {view.pending.length > 0 && view.landed.length > 0
                  ? `，刚下载了 ${view.landed.length} 个`
                  : ""}
              </span>
              <span className="shrink-0 text-gray-500">{open ? "收起 ▴" : "查看 ▾"}</span>
            </button>
            {open && remote.length > 1 && (
              <button
                onClick={() => idle.forEach((item) => download(item.blob))}
                disabled={idle.length === 0}
                className={CARD_BTN}
              >
                全部下载
              </button>
            )}
          </div>
          {open && (
            <div className="mt-2 max-h-64 space-y-2 overflow-y-auto pb-1">
              {view.landed.length > 0 && (
                <p className="text-[10px] leading-relaxed text-gray-500">
                  刚下载的文件可以在这里直接打开；消息里的预览图在下次打开这个会话时显示。
                </p>
              )}
              {view.pending.map((item) => (
                <AttachmentCard key={fileKey(item)} chatId={chatId} item={item} onError={onError} />
              ))}
              {view.landed.map((item) => (
                <AttachmentCard key={fileKey(item)} chatId={chatId} item={item} onError={onError} />
              ))}
            </div>
          )}
        </>
      )}
    </div>
  );
}
