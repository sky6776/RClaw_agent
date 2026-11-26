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
  /** 打开失败的文案（按落点）：与「存进工作区失败」分开，打开成功只清这一份。 */
  openErrors: Record<string, string>;
  /** 下载错误对应的发起序号（按对象）：卡片只显示自己最后一次操作之后的下载错误。 */
  errorSeq: Record<string, number>;
  /** 每张卡最近一次操作的序号（按落点键）：放进订阅快照里，操作发起时触发重渲染
   *  （否则模块级序号变了、卡片不会重算要显示哪条错误）。 */
  cardOp: Record<string, number>;
  /** 打开错误实际用的落点：下载成功只清对应落点的那条。 */
  openErrorPath: Record<string, string>;
  /** 存进工作区之后的相对路径。 */
  saved: Record<string, string>;
}

const noCardState = (): CardState => ({
  busy: {},
  saving: {},
  errors: {},
  saveErrors: {},
  openErrors: {},
  errorSeq: {},
  cardOp: {},
  openErrorPath: {},
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
/** 清单取不到（重试完也失败）的会话：界面显示「核对附件…（点此重试）」，不拿旧清单
 *  冒充当前判断。 */
let failed: string | null = null;
/** 清单不是这个会话的 / 还没取到时给的空数组（引用要稳定，别每次渲染新建）。 */
const EMPTY: SyncAttachment[] = [];
/** `消息行 id:blob` → 上一份清单里的状态（看得出哪些附件是刚下载下来的）。 */
let known: Record<string, string> = {};
let cards: CardState = noCardState();
/** 读的时候发现「内容对不上」的条目（`blob:path`）。清单里的这一条还没轮到核对
 *  （`checked=false`）时，读的入口（预览/点开/播放）在这里记一笔，界面立刻按「对不上」
 *  出附件卡（带重新下载），不再内联一个每次点都失败的卡；重新下载/存进工作区成功之后
 *  这笔撤掉（内容已经是登记的那份了）。清单核对过（`checked=true`）的以清单为准。 */
const changedKeys = new Set<string>();

/** 读的时候发现内容对不上时，卡片上直接显示的说明。 */
const CHANGED_CARD_NOTE =
  "这台设备上的文件已经不是这条消息引用的那一份了（可能被就地改写过），点「重新下载」取回原来的内容";

/** 后端的「内容对不上」错误（被就地改写过）。文案出自 commands::sync / commands 的
 *  核对入口，改文案时要一起改这里；其他错误（不存在、太大、读不了）不算。 */
export function isAttachmentChangedError(e: unknown): boolean {
  return String(e).includes("已经不是这条消息引用的那一份");
}

/** 某个附件读出来发现内容对不上：界面按「已改写」处理（见 [`changedKeys`]），卡片上
 *  立刻显示错误与「重新下载」（本地条目的卡本来只显示「已下载到」，不加这条错误就得多
 *  点一次「打开」才看到失败）。 */
export function markAttachmentChanged(item: { path: string; blob: string }) {
  const key = `${item.blob}:${item.path}`;
  if (changedKeys.has(key)) return;
  changedKeys.add(key);
  // 清单数组换一个身份：订阅者（消息里的媒体卡、画廊）重算一遍可读性。
  all = [...all];
  cards = { ...cards, saveErrors: { ...cards.saveErrors, [key]: CHANGED_CARD_NOTE } };
  emit();
}

/** 某一次读成功（内容核对通过）：这条「对不上」的覆盖与卡片错误一并撤掉。 */
export function clearAttachmentChanged(item: { path: string; blob: string }) {
  const key = `${item.blob}:${item.path}`;
  if (!changedKeys.delete(key)) return;
  all = [...all];
  cards = { ...cards, saveErrors: without(cards.saveErrors, key) };
  emit();
}

/** 让当前会话的清单重新取一遍（装载器在的时候才有）。 */
let reload: (() => void) | null = null;
/** 作废正在路上的清单查询并重取：保存成功后，旧清单会按保存前的 `path_present`
 *  把刚写下的记录清掉。 */
let invalidateLoad: (() => void) | null = null;
/** 当前会话正文里已经加载的消息行 id（Chat 回报）。附件清单覆盖整个会话，正文首屏
 *  只加载最近 80 行：已保存的旧附件若不在正文里，顶栏那条就是它唯一的打开入口。 */
let loadedMsgIds = new Set<number>();
let loadedMsgSig = "";
let loadedMsgChat: string | null = null;
/** 装载器还没切过去时先攒着的上报（切会话时 Chat 的 effect 先于装载器跑，丢了
 *  的话正文里已有的消息会被当成没加载，保存过的附件一直留在顶栏）。 */
let pendingLoaded: { chatId: string; ids: number[]; sig: string } | null = null;

export function setLoadedAttachmentMessages(chatId: string, ids: number[]) {
  const sig = ids.join(",");
  if (chat !== chatId) {
    pendingLoaded = { chatId, ids, sig };
    return;
  }
  if (loadedMsgChat === chatId && sig === loadedMsgSig) return;
  loadedMsgChat = chatId;
  loadedMsgSig = sig;
  loadedMsgIds = new Set(ids);
  pendingLoaded = null;
  // 「已保存」的顶栏条目去留取决于消息卡在不在正文里：集合变了重取一次清单。
  reload?.();
}

/** 另存落点的存在性：清单只描述消息路径，`-快照` 这种另存名字要靠目录列举确认。
 *  按目录缓存几秒，避免每次清单刷新都为同一批保存记录打一轮 IPC。 */
/** 保存记录的版本：每次写入保存落点递增。探活/打开回来时凭它确认期间没有重新
 *  保存，否则会拿旧结果把刚写下的记录删掉。 */
const savedKeyGen: Record<string, number> = {};
/** 每张卡最近一次发起的操作（打开/保存）序号：异步回调回来时只有「最近一次
 *  操作」的失败才写进卡片——先发起、后返回的旧操作不能盖住新错误。 */
let opSeq = 0;
const cardOpSeq: Record<string, number> = {};
/** 这个对象最近一次「下载」的发起序号：下载结果回来时凭它判断是不是当前这次请求。 */
const downloadAt: Record<string, number> = {};
function beginOp(key: string): number {
  opSeq += 1;
  cardOpSeq[key] = opSeq;
  return opSeq;
}
const savedDirOf = (rel: string) => {
  const cut = rel.lastIndexOf("/");
  return cut < 0 ? "" : rel.slice(0, cut);
};

type SavedProbe = { key: string; rel: string; size?: number; gen: number };

/** 后端核对附件内容时允许的长度偏差（`local::SIZE_SLACK`）：登记长度是别的设备
 *  报来的，实际落盘长度可能差一点；探活的长度快筛用同样的容差，别误删有效副本。 */
const SIZE_SLACK = 1024 * 1024;

/** 另存落点的存在性：清单只描述消息路径，`-快照` 这种另存名字要靠目录列举确认。
 *  只返回结果、不写缓存——探活是异步的，缓存会把旧目录状态带进更新的一轮。
 *  结果按输入下标一一对应（同一路径可能被两条记录复用，不能按路径归并）。
 *  同名但长度超容差的按「不是当初存下的那份」处理（内容核对留给打开时）。 */
async function probeSavedPaths(
  paths: Array<{ rel: string; size?: number }>,
): Promise<boolean[]> {
  const present = new Array<boolean>(paths.length).fill(false);
  const byDir = new Map<string, Array<{ idx: number; rel: string; size?: number }>>();
  paths.forEach((item, idx) => {
    const dir = savedDirOf(item.rel);
    const entry = { idx, rel: item.rel, size: item.size };
    const bucket = byDir.get(dir);
    if (bucket) bucket.push(entry);
    else byDir.set(dir, [entry]);
  });
  for (const [dir, rels] of byDir) {
    let entries: Map<string, number | null> | null = null;
    try {
      entries = new Map(
        (await api.workspaceList(dir))
          .filter((item) => !item.is_dir)
          .map((item) => [item.name, item.size]),
      );
    } catch (e) {
      // 目录明确不存在 / 被同名文件顶掉：这个目录下的另存文件一定不在了。
      // 其他错误（权限、工作区暂不可用、越界）按「不知道」处理，别误删记录。
      const text = String(e);
      entries =
        text.startsWith("目录不存在") || text.startsWith("不是目录") ? new Map() : null;
    }
    for (const { idx, rel, size } of rels) {
      if (entries === null) {
        // 核不了：当它在，别把还能用的记录删掉。
        present[idx] = true;
        continue;
      }
      const name = rel.slice(rel.lastIndexOf("/") + 1);
      const onDisk = entries.get(name);
      if (onDisk === undefined) continue;
      // 读不到元数据（null）不参与长度比较；真实的 0 字节要照常比（被截断成 0 的
      // 副本不能一直算「还在」）。
      if (size !== undefined && onDisk !== null && Math.abs(onDisk - size) > SIZE_SLACK) {
        continue;
      }
      present[idx] = true;
    }
  }
  return present;
}

/** 找出现在已经不该算「已保存」的另存记录（只返回候选，调用方在核对时序后再删）。 */
async function probeSavedGone(chatId: string): Promise<SavedProbe[]> {
  if (chat !== chatId) return [];
  const alt: SavedProbe[] = [];
  for (const [key, rel] of Object.entries(cards.saved)) {
    if (rel === key.slice(key.indexOf(":") + 1)) continue; // 存回原路径的看清单
    const blob = key.slice(0, key.indexOf(":"));
    alt.push({
      key,
      rel,
      size: all.find((item) => item.blob === blob)?.size,
      gen: savedKeyGen[key] ?? 0,
    });
  }
  if (alt.length === 0) return [];
  const ok = await probeSavedPaths(alt.map(({ rel, size }) => ({ rel, size })));
  if (chat !== chatId) return [];
  return alt.filter((_, idx) => !ok[idx]);
}

/** 撤掉探活确认没了的保存记录（版本/落点在这期间变过的不动）。 */
function dropSavedGone(gone: SavedProbe[]) {
  if (gone.length === 0) return;
  setCards((prev) => {
    const keys = gone
      .filter(({ key, rel, gen }) => prev.saved[key] === rel && (savedKeyGen[key] ?? 0) === gen)
      .map(({ key }) => key);
    if (keys.length === 0) return prev;
    const saved = { ...prev.saved };
    const saveErrors = { ...prev.saveErrors };
    for (const key of keys) {
      delete saved[key];
      delete saveErrors[key];
    }
    return { ...prev, saved, saveErrors };
  });
}

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
    // 键带路径：同一条消息可能把同一份内容引用成两个路径（a.pdf、b.pdf），
    // 只按 blob 记会让后一个的可用性盖到前一个上。
    const id = `${item.message_id}:${item.blob}:${item.path}`;
    const before = known[id];
    const key = fileKey(item);
    // 文件的消息路径上已经是登记的那份内容（`path_present`）：它就在这台设备上，
    // 即便登记落点仍在同步缓存（「存进工作区」只写文件、不改登记，状态还是
    // `cached`）。工具结果（artifact）没有消息路径，仍按登记状态算。
    const here = item.state === "local" || (item.kind === "file" && item.path_present);
    // 记「可用性」而不是原始状态：cached + path_present 的条目上一轮就已经可用，
    // 相同的清单重取时不能当成「刚下载下来」。
    nextKnown[id] = here ? "local" : item.state;
    if (here) {
      // 上一份清单里它还不在原位置上：是刚下载下来的。
      const fresh = before !== undefined && before !== "local";
      if (fresh && !landed.some((other) => fileKey(other) === key)) landed.push(item);
      return;
    }
    // 已经存过、但存到了另一个名字（原路径被别的文件占着，`xxx-快照.pdf`）：消息
    // 路径上没有它，可用户手上已经有能打开的落点——不再按「不在这台设备上」计，
    // 留在这里保住「打开」入口（正文只加载最近 80 行，旧消息里的卡不一定在）。
    // 存到原路径、这一轮又核对出文件没了的不算（prune 随后也会撤掉那条记录）。
    const savedTo = cards.saved[key];
    const savedExactGone = savedTo === item.path && item.checked && !item.path_present;
    if (savedTo !== undefined && !savedExactGone) {
      if (!landed.some((other) => fileKey(other) === key)) landed.push(item);
      return;
    }
    if (!pendingKeys.has(key)) {
      pendingKeys.add(key);
      pending.push(item);
    }
  });
  known = nextKnown;
  // 存好的落点又对不上了（被改写/删除）：不能再给「打开 savedTo」，让用户重新存。
  // 只处理文件引用——工具结果全文的登记位置不是消息路径，不按它判断。
  const liveKeys = new Set(usable.map((item) => fileKey(item)));
  const presentKeys = new Set(
    usable
      .filter((item) => item.kind === "file" && item.path_present)
      .map((item) => fileKey(item)),
  );
  // 同一个（对象, 路径）可能被多条消息引用、这一轮只核对了其中几条：只要有一条
  // 核对过、确认消息路径上没有它，就按「不在」处理；全都没轮到核对时才当「不知道」。
  const checkedAbsent = new Set(
    usable
      .filter((item) => item.kind === "file" && item.checked && !item.path_present)
      .map((item) => fileKey(item)),
  );
  const prune = (map: Record<string, string>) => {
    let dropped = false;
    const next = { ...map };
    for (const key of Object.keys(next)) {
      // 引用已经从清单里消失了（消息被删/改）：保存记录没有意义了。工具结果
      // （artifact）也算引用，别把它的保存记录当成「已消失」。
      if (!liveKeys.has(key)) {
        delete next[key];
        dropped = true;
        continue;
      }
      if (presentKeys.has(key) || !checkedAbsent.has(key)) continue;
      // 存到了另一个名字（原位置被别的文件占着，`xxx-快照.pdf`）：那个文件不是
      // 消息路径，`path_present` 不描述它——它的存在由目录探活负责。
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
  // 下载下来之后又不见了的（被删了）会重新出现在 pending 里：不再算「已下载」；
  // 引用本身从清单里消失了的（消息被删/改）也一并丢掉，恢复就绪后不会又冒出来。
  // 已经「存进工作区」的也不再列：消息里那张卡（带已存到的落点）就是它的入口，
  // 留在顶栏只会让提示在保存成功后继续挂着。
  const stillLanded = landed.filter(
    (item) =>
      liveKeys.has(fileKey(item)) &&
      !pendingKeys.has(fileKey(item)) &&
      // 已保存的文件：消息卡（带「已存到 … / 打开」）在正文里就交给正文；不在
      // 正文里（首屏只加载最近 80 行）、或者根本没有正文卡（工具结果 artifact；
      // 只在工具调用参数里出现的引用）的，留着——顶栏是唯一的打开入口。
      !(
        item.kind === "file" &&
        saved[fileKey(item)] !== undefined &&
        loadedMsgIds.has(item.message_id)
      ),
  );
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
function settle(
  blob: string,
  error: string | null,
  placed: boolean,
  landed: string | null,
) {
  // 这次下载的发起序号：卡片渲染时按「自己的最后一次操作」决定要不要显示它。
  const seq = downloadAt[blob] ?? opSeq;
  if (error === null && placed) {
    // 只撤「这次实际落到的那个路径」上的覆盖：同一对象的其他路径上可能还是另一份
    // 内容（取回到同步缓存时，消息路径并没有被修好）。`error` 为空但没落盘的
    // （存上没有、记下「在等别的设备/取不回来」）什么都不能清。
    if (landed) clearAttachmentChanged({ path: landed, blob });
  }
  const timer = busyTimers[blob];
  if (timer !== undefined) {
    window.clearTimeout(timer);
    delete busyTimers[blob];
  }
  setCards((prev) => {
    const busy = without(prev.busy, blob);
    const errors =
      error === null ? without(prev.errors, blob) : { ...prev.errors, [blob]: error };
    // 下载成功：只清「实际落到的那个路径」的打开失败（别的落点可能仍然读不了；
    // 存进工作区失败是另一回事）。没给出落点时什么都没恢复，不清。
    let openErrors = prev.openErrors;
    let openErrorPath = prev.openErrorPath;
    if (error === null && placed && landed) {
      const prefix = `${blob}:`;
      const keys = Object.keys(openErrors).filter(
        (key) => key.startsWith(prefix) && openErrorPath[key] === landed,
      );
      if (keys.length > 0) {
        openErrors = { ...openErrors };
        openErrorPath = { ...openErrorPath };
        for (const key of keys) {
          delete openErrors[key];
          delete openErrorPath[key];
        }
      }
    }
    // 卡片上的「内容对不上／保存失败」是按落点记的：不在这里按整个对象清（成功落到的
    // 那个落点由上面的 clearAttachmentChanged 连说明一起收掉；别的落点可能仍然不对）。
    if (
      busy === prev.busy &&
      errors === prev.errors &&
      openErrors === prev.openErrors &&
      error === null &&
      prev.errorSeq[blob] === undefined
    ) {
      return prev;
    }
    return {
      ...prev,
      busy,
      errors,
      openErrors,
      openErrorPath,
      errorSeq:
        error === null
          ? without(prev.errorSeq, blob)
          : { ...prev.errorSeq, [blob]: seq },
    };
  });
}

/** 下载一个还在别的设备上的附件。不等结果：结果经 sync 事件回来（见装载器）。
 *  `key` 是卡片发起时给的落点键：下载也算这张卡的一次操作（旧打开/保存失败作废）。 */
function download(chatId: string, blob: string, key?: string) {
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
  // 下载也是一次操作：参与「最近一次操作」排序（稍后的保存/打开会让它的失败
  // 不再写进卡片错误）。
  opSeq += 1;
  const op = opSeq;
  downloadAt[blob] = op;
  if (key) {
    // 这张卡的下载也算一次操作：该落点的旧打开/保存失败作废（结果回来时还会写
    // 这次的错误）。
    cardOpSeq[key] = op;
    setCards((prev) => ({
      ...prev,
      saveErrors: without(prev.saveErrors, key),
      openErrors: without(prev.openErrors, key),
      openErrorPath: without(prev.openErrorPath, key),
      cardOp: { ...prev.cardOp, [key]: op },
    }));
  }
  api.syncFetchAttachment(chatId, blob).catch((e) => {
    // 会话切走、或这次请求已经被后来的下载顶掉了：迟到的失败不能结算到新请求上。
    if (chat !== chatId) return;
    if (downloadAt[blob] !== op) return;
    settle(blob, String(e), false, null);
  });
}

/** 把同步缓存里的一份文件快照存进工作区（存到这条消息写的那个路径上）。
 *
 *  同一份内容可能被几条消息引用到不同的路径：按对象 + 目标路径记状态、也按它发请求，
 *  否则从 `b.pdf` 那张卡存下去，后端会拿清单里第一条缓存记录写到 `a.pdf`。 */
function save(chatId: string, item: SyncAttachment) {
  const key = fileKey(item);
  if (cards.saving[key]) return;
  const op = beginOp(key);
  setCards((prev) => ({
    ...prev,
    saving: { ...prev.saving, [key]: true },
    saveErrors: without(prev.saveErrors, key),
    openErrors: without(prev.openErrors, key),
    openErrorPath: without(prev.openErrorPath, key),
    cardOp: { ...prev.cardOp, [key]: op },
  }));
  api
    .syncAttachmentSave(chatId, item.blob, item.path)
    .then(({ rel, verified, blob: savedBlob }) => {
      if (chat !== chatId) return;
      // 旧版明文登记保存成功后被归一：消息引用换成了新对象名，落点要记在刷新后用的
      // 键上（否则刷新后卡片按新名找不到「已存到」，又显示「存进工作区」）。
      const savedKey = `${savedBlob}:${item.path}`;
      // 只有「写下去的字节核对过 **且** 落点就是这条消息的路径」才撤「读出来对不上」
      // 的覆盖：目标被别的文件占着时存成了 `-快照`，消息路径上仍然不是这份内容，标记
      // 要留着（其余路径同理，按落点清而不是按对象整体清）。
      if (verified && rel === item.path) {
        clearAttachmentChanged({ path: item.path, blob: item.blob });
        if (savedBlob !== item.blob) {
          clearAttachmentChanged({ path: item.path, blob: savedBlob });
        }
      } else if (changedKeys.has(key) || changedKeys.has(savedKey)) {
        // 没核对、或者没存到这条消息的路径上：卡片上的说明要放回去（保存开始时按常规
        // 清过一次）。
        cards = {
          ...cards,
          saveErrors: { ...cards.saveErrors, [savedKey]: CHANGED_CARD_NOTE },
        };
      }
      setCards((prev) => {
        const saved = { ...prev.saved, [savedKey]: rel };
        // 归一之后旧对象名的键不再被卡片使用。
        if (savedBlob !== item.blob) delete saved[key];
        // 重新保存成功：上一次打开失败的文案也一并收掉。
        return { ...prev, saved, openErrors: without(prev.openErrors, savedKey) };
      });
      savedKeyGen[savedKey] = (savedKeyGen[savedKey] ?? 0) + 1;
      // 存进工作区之后清单里的 `path_present` 会变（消息路径上真有这份内容了）：先
      // 作废在途的旧清单（否则它按保存前的结论把刚写的记录清掉），再立刻重取。
      invalidateLoad?.();
      reload?.();
    })
    .catch((e) => {
      if (chat !== chatId) return;
      // 之后又发起了别的操作（比如点打开）：这条保存失败不写进卡片。
      if (cardOpSeq[key] !== op) return;
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
      failed = null;
      known = {};
      cards = noCardState();
      changedKeys.clear();
      // Chat 的上报可能先到（子组件 effect 先跑）：消费它，别丢。
      if (pendingLoaded?.chatId === chatId) {
        loadedMsgIds = new Set(pendingLoaded.ids);
        loadedMsgSig = pendingLoaded.sig;
        loadedMsgChat = chatId;
        pendingLoaded = null;
      } else {
        loadedMsgIds = new Set();
        loadedMsgSig = "";
        loadedMsgChat = null;
      }
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
        // 一直取不到：保持「未就绪」并把状态标成失败（界面给一个重试入口）。不能拿
        // 旧清单或者空清单冒充「核对好了」——那会让同步消息回退到工作区路径。
        // 之前取到过的旧清单也一并清掉：那一轮之后文件可能已经被改写，拿旧的
        // `path_present` 继续判断等于永远按旧结论走。
        failed = chatId;
        loaded = null;
        all = EMPTY;
        allSig = "";
        // 顶栏「已下载」那几条是这一会儿刚落到工作区的，与清单是否取得到无关：留着。
        bar = { pending: [], landed: bar.landed, sig: "" };
        emit();
        return;
      }
      if (!alive || mine !== seq) return;
      loadFails = 0;
      failed = null;
      // 另存的文件可能已经被删：清单不描述这些路径，先探一遍目录再归类。
      // 探活只返回候选——这期间可能已经有更新的一轮装载，顺序核对过再落。
      const staleSaved = await probeSavedGone(chatId);
      if (!alive || mine !== seq || chat !== chatId) return;
      dropSavedGone(staleSaved);
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
    // 用户点「重试」：退避重新算（一直失败之后的重试要真的再试几轮，不是立刻又
    // 判定失败）。
    const retry = () => {
      loadFails = 0;
      // 预算外的条目 / 补落点也要重新轮一遍（都是「看几次就停」，用户点了重试就再来一轮）。
      uncheckedTries = 0;
      aliasTries.clear();
      schedule();
    };
    const invalidate = () => {
      seq += 1;
      schedule();
    };
    invalidateLoad = invalidate;
    reload = retry;
    void load();
    const un = api.onSync((event) => {
      if (event.kind === "attachment") {
        settle(event.blob, event.error, event.placed, event.landed);
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
      if (reload === retry) reload = null;
      if (invalidateLoad === invalidate) invalidateLoad = null;
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
    () => chatId !== null && chat === chatId && loaded === chatId && failed !== chatId,
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

/** 这个会话的附件清单这次是不是取不到（重试完也失败）。 */
export function useChatAttachmentsFailed(chatId: string | null): boolean {
  return useSyncExternalStore(
    subscribe,
    () => chatId !== null && chat === chatId && failed === chatId,
  );
}

/** 一条消息登记的文件附件（只认消息里写的路径；工具结果全文不在此列）。 */
export function messageFileRefs(
  list: SyncAttachment[],
  msgId: number | undefined,
): SyncAttachment[] {
  if (msgId === undefined) return [];
  return indexByMessage(list).get(msgId) ?? [];
}

/** 这条引用能按消息里的路径直接读吗。`checked=false`（还没轮到核对）的按「能读」
 *  处理——读取前后端会再核一次；核对过且对不上的才出附件卡。 */
export function readableAtPath(item: SyncAttachment): boolean {
  if (item.kind !== "file") return false;
  // 核对过的看「消息路径上真有这份内容」（包括已存回原位置的快照）；没核对过的只在
  // 落点就是消息路径时才按能读处理——`local` 只说明登记的落点存在，可能根本不是这条
  // 消息写的那个路径。
  // 读的时候已经发现对不上的（预算外/还没轮到核对的那条）：按对不上处理，出附件卡
  // 给「重新下载」，不再内联一个每次点都失败的卡。这一条优先于清单的 `checked`：
  // 摘要有 60 秒的信任窗口，刚核对过又被改写的文件在清单里还是「核对过、在」。
  if (changedKeys.has(`${item.blob}:${item.path}`)) return false;
  if (item.checked) return item.path_present;
  return item.state === "local" && item.rel_path === item.path;
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
  const key = fileKey(item);
  // 下载错误按对象记；只有它发生在该卡最后一次操作之后才显示（之后点过保存/打开/
  // 重新下载的，旧的下载失败已经过时）。
  const downloadError =
    state.errorSeq[item.blob] !== undefined &&
    state.errorSeq[item.blob] >= (state.cardOp[key] ?? -1)
      ? state.errors[item.blob]
      : undefined;
  const error = downloadError ?? state.saveErrors[key] ?? state.openErrors[key];
  // 落点不支持原子发布是文件系统能力问题，重新下载改变不了它。
  const publishUnsupported =
    typeof error === "string" &&
    (error.includes("不支持原子发布") || error.includes("不支持无覆盖改名"));
  const open = (path: string) => {
    const key = fileKey(item);
    const genAt = savedKeyGen[key] ?? 0;
    const op = beginOp(key);
    // 用户刚点了打开：这张卡上一次「存进工作区失败」的文案先收掉，失败时显示
    // 这次打开的原因（最近一次操作的错误）。
    setCards((prev) => {
      // 只收掉这个落点上一次「存进工作区失败」：下载错误是按对象记的，别的落点
      // 可能还要看它（由各卡片按自己的操作序号决定显示）。
      return {
        ...prev,
        saveErrors: without(prev.saveErrors, key),
        cardOp: { ...prev.cardOp, [key]: op },
      };
    });
    // 点开前先让后端核对一下：工作区里这个路径上的文件可能已经被就地改写过，打开
    // 前的一次核对（只读这一个）是「不让用户看到另一个版本」的最后一道关。
    // 打开的是「已存到另一个名字」的副本时严格核对：核不了（同步密钥/登记不可用）
    // 就不要按老行为直接把路径交给系统——那条路径没有别的证据能证明是这份内容。
    const strict = state.saved[key] !== undefined && state.saved[key] === path;
    api
      .syncOpenAttachment(chatId, item.blob, path, strict)
      // 打开的是核对过的副本才算这次读通过；按**打开的这个落点**清覆盖（别把消息
      // 路径上的标记也清了，那条可能仍然对不上）。
      .then((verified) => {
        // 先核对会话/版本/落点，全部对得上才清状态：迟到的成功回调不能清掉别的
        // 会话或后来重新保存的那份记录。
        if (chat !== chatId) return;
        if ((savedKeyGen[key] ?? 0) !== genAt) return;
        if (cards.saved[key] !== undefined && cards.saved[key] !== path) return;
        if (cardOpSeq[key] !== op) return;
        // `verified=false` 是「按老行为打开、没核对」：文件确实打开了，上一次打开
        // 失败同样已经过去；只是不能清「内容对不上」的标记。
        if (verified) clearAttachmentChanged({ path, blob: item.blob });
        setCards((prev) =>
          prev.openErrors[key] === undefined
            ? prev
            : {
                ...prev,
                openErrors: without(prev.openErrors, key),
                openErrorPath: without(prev.openErrorPath, key),
              },
        );
      })
      .catch((e) => {
        if (chat !== chatId) return;
        // 这期间又保存过（含存到同一路径的新版本）：这次失败说的是上一份记录，
        // 整次状态更新都不作数（包括错误文案与刷新），别挂到新卡片上。
        if ((savedKeyGen[key] ?? 0) !== genAt) return;
        // 这期间已经保存到别的落点：同上，别挂到新卡片上。
        if (cards.saved[key] !== undefined && cards.saved[key] !== path) return;
        // 之后又发起了别的操作：这次失败是过期的，不写状态也不弹提示。
        if (cardOpSeq[key] !== op) return;
        onError(`打开文件失败: ${String(e)}`);
        // 只有「落点上的文件已经不是这份内容了」（含被删掉）才收回「打开」；内容
        // 核对通过之后临时副本/拉起系统程序失败不算文件没了，记录要留着。
        const gone = isAttachmentChangedError(e);
        setCards((prev) => ({
          ...prev,
          saved: gone && prev.saved[key] === path ? without(prev.saved, key) : prev.saved,
          openErrors: { ...prev.openErrors, [key]: String(e) },
          openErrorPath: { ...prev.openErrorPath, [key]: path },
        }));
        if (gone) reload?.();
      });
  };

  if (item.state === "local") {
    // 同一份内容被两条消息引用成不同的路径：登记只落到其中一条上，另一条消息的路径
    // 上没有它。给个入口把内容存过去（后端按核对过的字节复制，不覆盖已有不同文件）。
    // 工具结果（artifact）在消息里只有一个文件名，没有「这条消息的路径」可存，不算。
    const elsewhere =
      item.kind === "file" && item.rel_path !== undefined && item.rel_path !== item.path;
    const saving = Boolean(state.saving[fileKey(item)]);
    const savedTo = state.saved[fileKey(item)];
    return (
      <div>
        <div className={CARD}>
          <span className={`${CARD_ICON} bg-emerald-500/10 text-emerald-400`}>✓</span>
          <span className="min-w-0 flex-1">
            <span className={CARD_NAME}>{shown}</span>
            <span className={CARD_NOTE}>
              {formatBytes(item.size)} · 已下载到 {item.rel_path ?? item.path}
              {savedTo ? ` · 已存到 ${savedTo}` : ""}
            </span>
          </span>
          {elsewhere && !savedTo && (
            <button
              onClick={() => save(chatId, item)}
              disabled={inactive || saving}
              className={CARD_BTN}
            >
              {saving ? "保存中…" : "存到这条消息的路径"}
            </button>
          )}
          {/* 打开只按工作区相对路径：`open_media` 不收绝对路径（安卓与 Windows 都会拒）。
              工具结果（artifact）的登记落点就在这个会话的 artifact 目录下。 */}
          <button
            onClick={() => open(savedTo ?? item.rel_path ?? item.path)}
            disabled={inactive}
            className={CARD_BTN}
          >
            打开
          </button>
        </div>
        {/* 点开前的内容核对没过（文件被就地改写过）：给一条能走的路——重新下载，
            否则就只剩一个反复失败的「打开」。 */}
        {error && (
          <div className="mt-1 flex items-start justify-between gap-2">
            <div className={`min-w-0 flex-1 ${CARD_ERROR}`}>{error}</div>
            {!publishUnsupported && (
              <button
                onClick={() => download(chatId, item.blob, key)}
                disabled={inactive}
                className={CARD_BTN}
              >
                重新下载
              </button>
            )}
          </div>
        )}
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
                路径，由持有正确内容的设备核对摘要后重传。落点不支持原子发布是文件
                系统能力问题，重下解决不了。 */}
            {!publishUnsupported && (
              <button
                onClick={() => download(chatId, item.blob, key)}
                disabled={inactive}
                className={CARD_BTN}
              >
                重新下载
              </button>
            )}
          </div>
        )}
      </div>
    );
  }

  const busy = Boolean(state.busy[item.blob]);
  const away = awayText(item.state, where);
  const savedTo = state.saved[fileKey(item)];
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
        {savedTo && (
          <button onClick={() => open(savedTo)} disabled={inactive} className={CARD_BTN}>
            打开
          </button>
        )}
        <button
          onClick={() => download(chatId, item.blob, key)}
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
  // 预览失败：权限/IO 给重试入口；超过大小上限是定论，只说明限制。内容对不上的
  // 走「重新下载」，不在这里。
  const [failKind, setFailKind] = useState<"retry" | "limit" | null>(null);
  const [attempt, setAttempt] = useState(0);
  // 对象名或路径变了（换空间 rekey、旧版明文摘要归一）：换一份预览，别接着显示旧内容。
  useEffect(() => {
    setSrc(snapshotPreviews.get(cacheKey) ?? null);
    setFailKind(null);
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
          // 只有核对过的字节才进按对象名索引的跨打开缓存：核不了时读到的可能是
          // 另一份内容，下次打开又原样复用就成了「永远显示错的那张」。
          if (r.verified) rememberSnapshot(cacheKey, url);
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
        .catch((e) => {
          if (!alive) return;
          // 快照内容对不上：卡片换成「重新下载」，别只在预览处静默失败。
          if (isAttachmentChangedError(e)) {
            markAttachmentChanged({ path, blob });
            refreshSyncAttachments();
            return;
          }
          // 权限/IO 之类的临时失败：给个重试，别让卡片永远没有预览；超过上限
          // 是定论，重试没有意义。
          const text = String(e);
          setFailKind(
            text.includes("超过大小上限") || text.includes("超过预览解码限制") ? "limit" : "retry",
          );
        });
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
  }, [chatId, blob, path, cacheKey, src, attempt]);
  if (!src && failKind === "limit") {
    return <span className="mb-1 block text-[11px] text-gray-500">图片过大，不生成预览</span>;
  }
  if (!src && failKind === "retry") {
    return (
      <button
        onClick={() => {
          setFailKind(null);
          setAttempt((n) => n + 1);
        }}
        className="mb-1 rounded-md border border-[#2a2f3a] px-2 py-1 text-[11px] text-gray-400 transition hover:text-gray-200"
      >
        预览加载失败，点此重试
      </button>
    );
  }
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
  const ready = useChatAttachmentsReady(chatId);
  const [open, setOpen] = useState(false);
  // 用户手动关掉的提示按「当前清单签名」记：清单内容变了（新的缺附件 / 刚下载完）
  // 会重新出现；换会话复位。
  const [dismissed, setDismissed] = useState<string | null>(null);
  // 换会话时收起来。
  useEffect(() => {
    setOpen(false);
    setDismissed(null);
  }, [chatId]);
  // 清单从关闭时的样子变成别的样子（存回去又删掉之类）再变回来：旧的关闭记录
  // 作废，该提示就提示；签名一直没变的重复重取则保持关闭。清单取不到时 `bar`
  // 会被清成空签名（不代表内容变了），那种过渡不算。
  useEffect(() => {
    if (!ready) return;
    setDismissed((prev) => (prev !== null && prev !== view.sig ? null : prev));
  }, [view.sig, ready]);

  const running = runningOn !== undefined && runningOn.length > 0;
  // 清单没取到（或刚作废、正在重取）时 `bar` 还是上一轮的判断：不能拿它出下载/
  // 「已下载 · 打开」这类操作，只留「正在别的设备上运行」的提示。
  const total = ready ? view.pending.length + view.landed.length : 0;
  // 「收起」只折叠列表；要整条消失用右上角的 ✕（清单签名变了会再出现）。
  const showBar = total > 0 && dismissed !== view.sig;
  if (!running && !showBar) return null;

  const remote = view.pending.filter((item) => item.state === "remote");
  const idle = remote.filter((item) => !state.busy[item.blob]);
  return (
    <div className="shrink-0 border-b border-[#2a2f3a] bg-[#141821] px-4 py-2 max-md:px-3">
      {running && (
        <div
          className={`rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-1.5 text-xs leading-relaxed text-amber-300${
            showBar ? " mb-2" : ""
          }`}
        >
          这个会话正在「{runningOn.join("」「")}」上运行。
          现在在这里接着发消息的话，两边各写各的，同步之后会分成两个分支。
        </div>
      )}
      {showBar && (
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
            <button
              onClick={() => {
                setDismissed(view.sig);
                setOpen(false);
              }}
              title="关闭"
              aria-label="关闭附件提示"
              className="shrink-0 rounded px-1 py-0.5 text-xs text-gray-500 transition hover:text-gray-200"
            >
              ✕
            </button>
            {open && remote.length > 1 && (
              <button
                onClick={() =>
                  idle.forEach((item) => download(chatId, item.blob, fileKey(item)))
                }
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
