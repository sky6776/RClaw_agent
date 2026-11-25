import { useEffect, useRef, useState, useSyncExternalStore } from "react";
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

/** 附件清单一次没取到时的重试节奏：几秒内把临时故障熬过去；一直取不到就按「没有
 *  清单」处理（清掉旧清单，别拿过时的判断冒充新清单）。 */
const LOAD_RETRY_MS = [1000, 3000, 10_000, 30_000, 60_000];

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

/** 卡片上的临时状态。下载相关的按附件对象（blob）记；保存相关的按「对象:目标路径」
 *  记——同一份内容可能被几条消息引用到不同路径，一个落点失败不该显示在另一个上。 */
interface CardState {
  busy: Record<string, true>;
  saving: Record<string, true>;
  errors: Record<string, string>;
  saveErrors: Record<string, string>;
  /** 存进工作区之后的相对路径。 */
  saved: Record<string, string>;
}

const noCardState = (): CardState => ({
  busy: {},
  saving: {},
  errors: {},
  saveErrors: {},
  saved: {},
});

/** 清单属于哪个会话；换会话时整份换掉。 */
let chat: string | null = null;
let bar: BarView = NONE;
/** 当前会话的完整附件清单（含本机已有的）：消息里的媒体卡按路径查它，不在这台设备
 *  上的文件才出同步卡（[`useChatAttachments`]）。 */
let all: SyncAttachment[] = [];
let allSig = "";
/** 清单已经取到的会话（取失败也算：按「没有清单」处理）。区分「还没取到」与「取到
 *  是空的」——前者消息里的附件还没核对过，工作区路径不能当可用的打开入口。 */
let loaded: string | null = null;
/** 清单不是这个会话的 / 还没取到时给的空数组（引用要稳定，别每次渲染新建）。 */
const EMPTY: SyncAttachment[] = [];
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
  // 就绪状态本身也是订阅者关心的：清单内容没变（比如新消息只是文本）时也要通知，
  // 否则「核对附件…」的占位会一直停在那儿等下一次无关的重渲染。
  const wasLoaded = loaded === chatId;
  loaded = chatId;
  const usable = list.filter((item) => item.kind === "file" || item.kind === "artifact");
  const nextAllSig = JSON.stringify(usable);
  const changedAll = nextAllSig !== allSig;
  if (changedAll) {
    all = usable;
    allSig = nextAllSig;
  }
  const nextKnown: Record<string, string> = {};
  const pending: SyncAttachment[] = [];
  const pendingKeys = new Set<string>();
  const landed = [...bar.landed];
  usable.forEach((item) => {
    // 工具结果全文（artifact）也列出来：它落到同步缓存时，消息里原有的路径句柄指向的
    // 可能是工作区里的另一个同名文件，用户得能从这儿打开同步来的那份准确内容。
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
  // 存好的落点又对不上了（被改写/删除）：不能再给「打开 savedTo」，让用户重新存。
  // 只处理文件引用——工具结果全文的登记位置不是消息路径，不按它判断。
  const fileKeys = new Set(
    usable.filter((item) => item.kind === "file").map((item) => fileKey(item)),
  );
  const presentKeys = new Set(
    usable
      .filter((item) => item.kind === "file" && item.path_present)
      .map((item) => fileKey(item)),
  );
  const prune = (map: Record<string, string>) => {
    let dropped = false;
    const next = { ...map };
    for (const key of Object.keys(next)) {
      if (!fileKeys.has(key) || presentKeys.has(key)) continue;
      // 存到了另一个名字（原位置被别的文件占着，`xxx-快照.pdf`）：那个文件不是消息
      // 路径，`path_present` 不描述它——保留「打开」，别让用户再存一份。
      const original = key.slice(key.indexOf(":") + 1);
      if (next[key] !== original) continue;
      delete next[key];
      dropped = true;
    }
    return dropped ? next : map;
  };
  const saved = prune(cards.saved);
  const saveErrors = prune(cards.saveErrors);
  const cardsChanged = saved !== cards.saved || saveErrors !== cards.saveErrors;
  if (cardsChanged) cards = { ...cards, saved, saveErrors };
  const sig = JSON.stringify([pending, stillLanded]);
  if (sig === bar.sig && !changedAll && !cardsChanged && wasLoaded) return;
  bar = { pending, landed: stillLanded, sig };
  emit();
}

/** 清单要重取了（这个会话刚合进来别的设备的消息）：在取到之前，消息里的工作区路径
 *  不能当成「本机有」——新消息引用的可能正是别的设备上的文件，按路径打开会拿到一个
 *  同名、不同内容的本地文件。 */
function markStale(chatId: string) {
  if (chat !== chatId || loaded !== chatId) return;
  loaded = null;
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
    // 重新下载成功：这个对象在各个落点上的旧失败（缓存副本被改写过之类）一并清掉，
    // 修好的卡不该还挂着上一条错误。
    let saveErrors = prev.saveErrors;
    if (error === null) {
      const prefix = `${blob}:`;
      const stale = Object.keys(saveErrors).filter((key) => key.startsWith(prefix));
      if (stale.length > 0) {
        saveErrors = { ...saveErrors };
        for (const key of stale) delete saveErrors[key];
      }
    }
    if (busy === prev.busy && errors === prev.errors && saveErrors === prev.saveErrors) {
      return prev;
    }
    return { ...prev, busy, errors, saveErrors };
  });
}

/** 下载一个还在别的设备上的附件。不等结果：结果经 sync 事件回来（见装载器）。 */
function download(chatId: string, blob: string) {
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
  api.syncFetchAttachment(chatId, blob).catch((e) => settle(blob, String(e)));
}

/** 把同步缓存里的一份文件快照存进工作区（存到这条消息写的那个路径上）。
 *
 *  同一份内容可能被几条消息引用到不同的路径：按对象 + 目标路径记状态、也按它发请求，
 *  否则从 `b.pdf` 那张卡存下去，后端会拿清单里第一条缓存记录写到 `a.pdf`。 */
function save(chatId: string, item: SyncAttachment) {
  const key = fileKey(item);
  if (cards.saving[key]) return;
  setCards((prev) => ({
    ...prev,
    saving: { ...prev.saving, [key]: true },
    saveErrors: without(prev.saveErrors, key),
  }));
  api
    .syncAttachmentSave(chatId, item.blob, item.path)
    .then((rel) => {
      if (chat !== chatId) return;
      setCards((prev) => ({ ...prev, saved: { ...prev.saved, [key]: rel } }));
      // 存进工作区之后清单里的 `path_present` 会变（消息路径上真有这份内容了）：让
      // 消息里的卡立刻换回正常的图片/视频卡，不用等下一次同步动静。
      reload?.();
    })
    .catch((e) => {
      if (chat !== chatId) return;
      setCards((prev) => ({ ...prev, saveErrors: { ...prev.saveErrors, [key]: String(e) } }));
    })
    .finally(() => {
      setCards((prev) =>
        prev.saving[key] ? { ...prev, saving: without(prev.saving, key) } : prev,
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
      all = EMPTY;
      allSig = "";
      loaded = null;
      known = {};
      cards = noCardState();
      emit();
    }
    let alive = true;
    let seq = 0;
    let timer: number | undefined;
    // 「补落点」的复制发生在会话事件之后（`settle_aliases`），成功时不一定再有事件：
    // 清单里还有「这份内容本机有、但消息路径上没有」的条目时过几秒再看一眼。次数按
    // 对象记——别的对象早先冲突过，不该把这个对象的补看额度用掉；目标被另一份文件
    // 占着的，复制会一直不成功，看几次就停。
    let aliasTimer: number | undefined;
    const aliasTries = new Map<string, number>();
    // 预算外的条目（`checked=false`）每轮会把上一轮核对过的让出来，几轮就能轮完；
    // 一直核不完的（很大的老消息）看几次就停。
    let uncheckedTries = 0;
    // 取清单失败的重试节奏（毫秒）：一直取不到就不再假装就绪。
    let loadFails = 0;
    const load = async () => {
      const mine = ++seq;
      let list: SyncAttachment[];
      try {
        list = await api.syncAttachments(chatId);
      } catch {
        // 取不到清单：不能把旧清单（或者空清单）当成「核对好了」——那会让同步消息
        // 回退到工作区路径，正好是这次要修的那类问题。退避重试几次；一直取不到才
        // 按「没有清单」处理，并清掉旧清单（别拿过时的判断冒充新清单）。
        if (!alive || mine !== seq || chat !== chatId) return;
        if (loadFails < LOAD_RETRY_MS.length) {
          const delay = LOAD_RETRY_MS[loadFails];
          loadFails += 1;
          window.clearTimeout(aliasTimer);
          aliasTimer = window.setTimeout(() => void load(), delay);
          return;
        }
        all = EMPTY;
        allSig = "";
        loaded = chatId;
        emit();
        return;
      }
      if (!alive || mine !== seq) return;
      loadFails = 0;
      apply(chatId, list);
      const pendingAlias = list.filter(
        (item) => item.kind === "file" && item.state === "local" && !item.path_present,
      );
      let retry = false;
      for (const item of pendingAlias) {
        const tries = aliasTries.get(item.blob) ?? 0;
        if (tries < 3) {
          aliasTries.set(item.blob, tries + 1);
          retry = true;
        }
      }
      if (list.some((item) => item.kind === "file" && !item.checked) && uncheckedTries < 6) {
        uncheckedTries += 1;
        retry = true;
      }
      if (retry) {
        window.clearTimeout(aliasTimer);
        aliasTimer = window.setTimeout(() => void load(), 3000);
      }
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
      } else if (event.kind === "status") {
        // 每一轮同步的状态变化：别名补落点、登记回收都可能在这时完成，借机重取一次
        // 清单（下面 schedule 会合并）。
        schedule();
      } else if (
        event.kind === "sessions" &&
        event.chats.some((c) => c.channel === "desktop" && c.chat_id === chatId)
      ) {
        // 别的设备的消息合进了这个会话：它们引用的文件多半不在本机。已经开始的那次
        // 查询读到的可能是旧清单（那条应答晚一拍回来会把旧清单又标成就绪），先作废
        // 它；新消息可能已经渲染出来了，等重取到之前也别让工作区路径当可用的入口。
        seq += 1;
        markStale(chatId);
        schedule();
      }
    });
    return () => {
      alive = false;
      window.clearTimeout(timer);
      window.clearTimeout(aliasTimer);
      // 离开对话页（或换会话）：清单可能在这段时间里过时（App 照样在合入消息）。
      // 清单本身留着（同一个会话回来不闪），但标成「未核对」——回来时先按未就绪
      // 处理，等新清单到了再放开工作区路径。
      if (chat === chatId && loaded === chatId) {
        loaded = null;
        emit();
      }
      if (reload === schedule) reload = null;
      void un.then((fn) => fn());
    };
  }, [chatId]);
}

/** 让当前会话的清单立刻重取一遍（一轮对话结束、工具可能改写过文件时用）。 */
export function refreshSyncAttachments() {
  reload?.();
}

/**
 * 当前会话的完整附件清单（同一份清单，[`SyncChatBar`] 与消息里的媒体卡共用）。
 * 不在对话页、或者清单还没取到：空列表。消息里的媒体卡按路径在这份清单里查——
 * 文件在别的设备上（或只在同步缓存里）时，MediaPreviews 改出附件卡，而不是一个
 * 点了只报「媒体文件不存在」的「点开」卡。
 */
export function useChatAttachments(chatId: string | null): SyncAttachment[] {
  return useSyncExternalStore(subscribe, () => (chatId !== null && chat === chatId ? all : EMPTY));
}

/**
 * 这个会话的附件清单取到了没有（取失败也算取到——按「没有清单」处理）。还没取到
 * 时消息里的工作区路径不能当作可用的打开入口：那可能是别的设备上的文件，只是清单
 * 还没回来。
 */
export function useChatAttachmentsReady(chatId: string | null): boolean {
  return useSyncExternalStore(
    subscribe,
    () => chatId !== null && chat === chatId && loaded === chatId,
  );
}

/** 清单按消息行索引（同一份清单只建一次：消息组件很多，别每个都扫一遍清单）。 */
const byMessage = new WeakMap<SyncAttachment[], Map<number, SyncAttachment[]>>();

function indexByMessage(list: SyncAttachment[]): Map<number, SyncAttachment[]> {
  const cached = byMessage.get(list);
  if (cached) return cached;
  const index = new Map<number, SyncAttachment[]>();
  for (const item of list) {
    if (item.kind !== "file") continue;
    const bucket = index.get(item.message_id);
    if (bucket) bucket.push(item);
    else index.set(item.message_id, [item]);
  }
  byMessage.set(list, index);
  return index;
}

/** 一条消息登记的文件附件（只认消息里写的路径；工具结果全文不在此列）。 */
export function messageFileRefs(
  list: SyncAttachment[],
  msgId: number | undefined,
): SyncAttachment[] {
  if (msgId === undefined) return [];
  return indexByMessage(list).get(msgId) ?? [];
}

/** 一条消息登记的文件里，消息路径上并没有这份内容的（按路径索引）：界面出附件卡，
 *  画廊也要跳过——按工作区路径取会拿到同名却不是这份内容的文件。 */
export function awayAttachments(
  list: SyncAttachment[],
  msgId: number | undefined,
): Map<string, SyncAttachment> {
  const away = new Map<string, SyncAttachment>();
  for (const item of messageFileRefs(list, msgId)) {
    if (!item.path_present) away.set(item.path, item);
  }
  return away;
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
    // 这台设备上的同名文件被就地改写过，已经不是这条消息引用的那一份：重新下载会取回
    // 登记的内容（落到同步缓存，不覆盖现有文件）。
    case "changed":
      return {
        note: "这台设备上的同名文件已经不是这条消息引用的那一份",
        action: "重新下载",
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

/** 一个不在本机原位置上的附件（下载 / 存进工作区 / 重试）。会话顶部的一条与消息里
 *  的媒体卡共用：同一种状态在两处说法一致，点按的行为也一致。 */
export function AttachmentCard({
  chatId,
  item,
  name,
  inactive = false,
  onError,
}: {
  chatId: string;
  item: SyncAttachment;
  /** 卡片上展示的文件名（消息里媒体回执行给了 name 时用那个；缺省取路径末段）。 */
  name?: string;
  /** 消息多选模式：卡片的按钮不能用（指针事件已由外层让开，键盘回车也不能触发）。 */
  inactive?: boolean;
  onError: (message: string) => void;
}) {
  const state = useSyncExternalStore(subscribe, () => cards);
  const shown = name ?? (item.path.split("/").pop() || item.path);
  const where = item.origin ? `「${item.origin}」` : "别的设备";
  const isImage = IMAGE_RE.test(item.path);
  const error = state.errors[item.blob] ?? state.saveErrors[fileKey(item)];
  const open = (path: string) => {
    // 点开前先让后端核对一下：工作区里这个路径上的文件可能已经被就地改写过，打开
    // 前的一次核对（只读这一个）是「不让用户看到另一个版本」的最后一道关。
    api
      .syncOpenAttachment(chatId, item.blob, path)
      .catch((e) => {
        onError(`打开文件失败: ${String(e)}`);
        // 核对没过（文件被改写/删掉）：这次保存的落点不能再「打开」，收回去让用户
        // 重新「存进工作区」；错误按落点记，只有这张卡显示。
        const key = fileKey(item);
        setCards((prev) => ({
          ...prev,
          saved: without(prev.saved, key),
          saveErrors: { ...prev.saveErrors, [key]: String(e) },
        }));
        reload?.();
      });
  };

  if (item.state === "local") {
    return (
      <div className={CARD}>
        <span className={`${CARD_ICON} bg-emerald-500/10 text-emerald-400`}>✓</span>
        <span className="min-w-0 flex-1">
          <span className={CARD_NAME}>{shown}</span>
          <span className={CARD_NOTE}>
            {formatBytes(item.size)} · 已下载到 {item.rel_path ?? item.path}
          </span>
        </span>
        {/* 打开只按工作区相对路径：`open_media` 不收绝对路径（安卓与 Windows 都会拒）。
            工具结果（artifact）的登记落点就在这个会话的 artifact 目录下。 */}
        <button
          onClick={() => open(item.rel_path ?? item.path)}
          disabled={inactive}
          className={CARD_BTN}
        >
          打开
        </button>
      </div>
    );
  }

  if (item.state === "cached") {
    const savedTo = state.saved[fileKey(item)];
    const saving = Boolean(state.saving[fileKey(item)]);
    return (
      <div>
        {isImage && (
          <SnapshotImage chatId={chatId} blob={item.blob} path={item.path} alt={shown} />
        )}
        <div
          className={CARD}
          title="这是从别的设备取回的一份文件快照，放在同步缓存里（这台设备的工作区里可能另有同名文件，所以不往原位置写）。存进工作区之后，助手才能按路径读到它。"
        >
          <span className={`${CARD_ICON} bg-indigo-600 text-white`}>📄</span>
          <span className="min-w-0 flex-1">
            <span className={CARD_NAME}>{shown}</span>
            <span className={CARD_NOTE}>
              {formatBytes(item.size)} · 来自{where}的文件快照
              {savedTo ? ` · 已存到 ${savedTo}` : "，存进工作区之后助手才读得到"}
            </span>
          </span>
          {savedTo ? (
            <button onClick={() => open(savedTo)} disabled={inactive} className={CARD_BTN}>
              打开
            </button>
          ) : (
            <button
              onClick={() => save(chatId, item)}
              disabled={inactive || saving}
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
            <button
              onClick={() => download(chatId, item.blob)}
              disabled={inactive}
              className={CARD_BTN}
            >
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
          <span className={CARD_NAME}>{shown}</span>
          <span className={CARD_NOTE}>
            {formatBytes(item.size)} · {away.note}
          </span>
        </span>
        <button
          onClick={() => download(chatId, item.blob)}
          disabled={inactive || busy}
          className={CARD_BTN}
        >
          {busy ? "下载中…" : away.action}
        </button>
      </div>
      {error && <div className={`mt-1 ${CARD_ERROR}`}>{error}</div>}
    </div>
  );
}

/** 已生成的快照预览（对象名 → data URL）。对象名是内容摘要：同一份内容在多条消息
 *  里出现只经一次 IPC；打开一个同步来的会话，缓存里的图往往不只一张。
 *
 *  按字节数限额：GIF 与不超上限的小图是原样返回的（最大 25 MB）再转 base64，按条数
 *  限几十条就能留住上 GB；超过总预算时先丢最早的，单张超过上限的不记（这张卡自己
 *  照样显示，只是离开会话再回来要重取）。 */
const snapshotPreviews = new Map<string, string>();
let snapshotPreviewBytes = 0;
const SNAPSHOT_PREVIEWS_BUDGET = 8 * 1024 * 1024;
const SNAPSHOT_PREVIEW_ONE = 2 * 1024 * 1024;
/** 正在取的（同一对象多张卡一起进视口时只发一次 IPC，失败后不留在表里）。 */
const snapshotPending = new Map<string, Promise<string>>();

function rememberSnapshot(key: string, src: string) {
  if (src.length > SNAPSHOT_PREVIEW_ONE || snapshotPreviews.has(key)) return;
  while (
    snapshotPreviews.size > 0 &&
    snapshotPreviewBytes + src.length > SNAPSHOT_PREVIEWS_BUDGET
  ) {
    const oldest = snapshotPreviews.keys().next().value;
    if (oldest === undefined) break;
    snapshotPreviewBytes -= snapshotPreviews.get(oldest)?.length ?? 0;
    snapshotPreviews.delete(oldest);
  }
  snapshotPreviews.set(key, src);
  snapshotPreviewBytes += src.length;
}

/** 同步缓存里一张图片快照的预览。出不了预览图就不显示，下面的卡片还在。
 *
 *  卡片快进视口时才经 IPC 生成预览：`sync_attachment_preview` 每次都要重建这个会话
 *  的附件清单并核对摘要，而一个会话可能同时挂出几十张快照卡——都在挂载时请求的话，
 *  打开会话要白等一长串 IPC（`loading="lazy"` 只管字节，不管这次调用）。 */
function SnapshotImage({
  chatId,
  blob,
  path,
  alt,
}: {
  chatId: string;
  blob: string;
  /** 这条消息里的路径（扩展名决定这份内容能不能出预览）。 */
  path: string;
  alt: string;
}) {
  // 预览的样子取决于按哪个路径取（同一个对象引用成 `copy.png` 与 `animation.gif`，
  // 后者原样返回动画、前者是静态缩略图）：缓存键带上格式。
  const format = (path.split(".").pop() ?? "").toLowerCase();
  const cacheKey = `${blob}:${format}`;
  const [src, setSrc] = useState<string | null>(
    () => snapshotPreviews.get(cacheKey) ?? null,
  );
  // 对象名或路径变了（换空间 rekey、旧版明文摘要归一）：换一份预览，别接着显示旧内容。
  useEffect(() => {
    setSrc(snapshotPreviews.get(cacheKey) ?? null);
  }, [cacheKey]);
  const box = useRef<HTMLSpanElement | null>(null);
  useEffect(() => {
    if (src) return;
    let alive = true;
    const load = () => {
      // 别的卡可能已经取到了（或者正在取）：直接用，不再发一遍。
      const known = snapshotPreviews.get(cacheKey);
      if (known) {
        setSrc(known);
        return;
      }
      // 在途请求按「对象 + 路径」记：同一份内容引用成不同的名字时，能不能预览取决于
      // 各自那个名字的扩展名，不能把一次失败的结论借给另一张卡。
      const pending = `${blob}:${path}`;
      let request = snapshotPending.get(pending);
      if (!request) {
        const started = api.syncAttachmentPreview(chatId, blob, path).then((r) => {
          const url = `data:${r.mime};base64,${r.base64}`;
          rememberSnapshot(cacheKey, url);
          return url;
        });
        request = started;
        snapshotPending.set(pending, started);
        void started
          .catch(() => {})
          .then(() => {
            if (snapshotPending.get(pending) === started) snapshotPending.delete(pending);
          });
      }
      void request
        .then((url) => {
          if (alive) setSrc(url);
        })
        .catch(() => {});
    };
    const el = box.current;
    if (!el || typeof IntersectionObserver === "undefined") {
      load();
      return () => {
        alive = false;
      };
    }
    const observer = new IntersectionObserver(
      (entries) => {
        if (!entries.some((entry) => entry.isIntersecting)) return;
        observer.disconnect();
        load();
      },
      { rootMargin: "300px" },
    );
    observer.observe(el);
    return () => {
      alive = false;
      observer.disconnect();
    };
  }, [chatId, blob, path, cacheKey, src]);
  // 没加载出来时不占位，只留一个 1px 的观察点（在卡片正上方，随卡片滚进视口）。
  if (!src) return <span ref={box} className="block h-1 w-full" aria-hidden="true" />;
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
                onClick={() => idle.forEach((item) => download(chatId, item.blob))}
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
