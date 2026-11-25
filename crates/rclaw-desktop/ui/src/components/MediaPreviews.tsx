import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent,
  type TouchEvent,
} from "react";
import { api, type SyncAttachment } from "../lib/api";
import { createLongPressGuard, type LongPressGuard } from "../lib/longPress";
import { useIsMobile, isAndroidDevice } from "../lib/useIsMobile";
import {
  AttachmentCard,
  clearAttachmentChanged,
  isAttachmentChangedError,
  markAttachmentChanged,
  messageFileRefs,
  useChatAttachmentsFailed,
  readableAtPath,
  refreshSyncAttachments,
  useChatAttachments,
  useChatAttachmentsReady,
} from "./SyncAttachments";

/** 会话流内嵌媒体预览条目。 */
export interface PreviewItem {
  /** 去重键（传给后端的工作区路径）。 */
  key: string;
  /** 相对 agent 根的路径（如 media/out/<sha>/x.jpg），经 media_base64 / open_media 校验。 */
  path: string;
  /** 卡片上展示的文件名。 */
  display: string;
  /** image = 内嵌 <img>；open = 点击交系统应用打开（视频 / 文档）。 */
  kind: "image" | "open";
  /** true = 正文里只有裸文件名、按惯例猜成 `media/<名字>` 的条目（不是正文明确写出的
   *  路径）。同名登记存在时这种猜测要让位。 */
  inferred?: boolean;
}

/** 上抛给 Chat 统一菜单的媒体引用。 */
export interface MediaMenuPayload {
  path: string;
  display: string;
  kind: "image" | "open";
}

/** 媒体回执 marker 里的 path（设备绝对路径，或 agent/ 相对路径）→ 相对 agent 根
 *  的路径。publish_media.py 暂存在 <agent>/media/out/<sha>/<name>；内容寻址目录
 *  是 hex、文件名经 safe_name 清洗，路径里不会再出现 "agent/" 子串，取末段
 *  "agent/" 之后即工作区相对路径。 */
function toWorkspacePath(p: string): string {
  const i = p.lastIndexOf("/agent/");
  return (i >= 0 ? p.slice(i + "/agent/".length) : p).replace(/^\/+/, "");
}

/** 正文里是否真的写出了这个路径：两侧要有边界（`report.pdf` 不能命中
 *  `report.pdf.bak`，也不能命中 `output/media/x.png` 里的 `media/x.png` 尾巴）。
 *  补卡与画廊的清单兜底共用这一把尺子。 */
export function mentionsPath(content: string, path: string): boolean {
  let from = 0;
  for (;;) {
    const at = content.indexOf(path, from);
    if (at < 0) return false;
    const after = content[at + path.length] ?? "";
    // 句点必须单独判：后面还接着文件名成分（`pdf.bak`）就不是这个路径。
    const next = content[at + path.length + 1] ?? "";
    const okAfter =
      after === ""
        ? true
        : after === "."
          ? !/[A-Za-z0-9_]/.test(next)
          : !/[A-Za-z0-9_\-/\\]/.test(after);
    if (okAfter && boundaryBefore(content, at)) return true;
    from = at + 1;
  }
}

/** 匹配位置左边的边界：token 开头，或者紧跟在**工作区根**（`agent/`）之后。
 *  `output/media/x.png` 里的 `media/x.png` 是另一个路径的尾巴，不算——那会把它
 *  挂成一张不相干的卡。句读、空白、汉字都算边界。 */
function boundaryBefore(content: string, at: number): boolean {
  const startsToken = (index: number) =>
    index === 0 || !/[A-Za-z0-9_.\-/\\]/.test(content[index - 1]);
  if (at === 0) return true;
  const before = content[at - 1];
  if (!/[A-Za-z0-9_.\-/\\]/.test(before)) return true;
  // `./output/report.pdf`：点斜杠前缀也是 token 开头。
  if ((before === "/" || before === "\\") && content[at - 2] === ".") {
    return startsToken(at - 2);
  }
  if (before !== "/" && before !== "\\") return false;
  // 紧跟在一个分隔符后面：前一段必须是工作区根 `agent`（媒体回执行里的绝对路径
  // 就是这种写法）。
  const end = at - 1;
  let start = end;
  while (start > 0 && !/[/\\]/.test(content[start - 1])) start -= 1;
  if (content.slice(start, end).toLowerCase() !== "agent") return false;
  // `agent/` 必须属于一条绝对路径（`/…/agent/`、`C:/…/agent/`）：
  // `output/agent/…` 这种相对路径里的 `agent` 不是工作区根。
  let token = start;
  while (token > 0 && /[A-Za-z0-9_.\-/\\]/.test(content[token - 1])) {
    token -= 1;
  }
  const root = content.slice(token, start);
  return root.startsWith("/") || /^[A-Za-z]:[/\\]/.test(root);
}


/** 从消息正文解析媒体引用条目（媒体回执行 + 工作区路径 + 裸文件名兜底）。
 *  顺序即出现顺序，同路径去重。除本组件出卡外，Chat 的全屏媒体画廊也用
 *  同一解析把全会话的图片/视频拼成可左右滑切换的时间线（口径必须一致，
 *  点击的卡片才保证能在画廊里对上号）。 */
export function parseMediaItems(content: string): PreviewItem[] {
  const found: PreviewItem[] = [];
  const seen = new Set<string>();
  const push = (
    path: string,
    display: string,
    kind?: "image" | "open",
    inferred?: boolean,
  ) => {
    if (seen.has(path)) return;
    seen.add(path);
    const isImg = /\.(?:png|jpe?g|webp|gif)$/i.test(path);
    found.push({
      key: path,
      path,
      display,
      kind: kind ?? (isImg ? "image" : "open"),
      inferred,
    });
  };

  let m: RegExpExecArray | null;
  // 1) 媒体回执行：agent 发布图片 / 文档 / 视频成功的凭据，取 path 渲染，
  //    name 作为卡片文件名（比沙箱路径友好）。
  const reMarker = /\[:[\w-]+-media\s+([^\]]*)\]/g;
  while ((m = reMarker.exec(content)) !== null) {
    const attrs = m[1];
    const pathMatch = attrs.match(/\bpath="([^"]+)"/);
    if (!pathMatch) continue;
    const path = toWorkspacePath(pathMatch[1].replace(/\\/g, "/"));
    const nameMatch = attrs.match(/\bname="([^"]+)"/);
    const typeMatch = attrs.match(/\btype="([^"]+)"/);
    push(
      path,
      nameMatch?.[1] ?? path.split("/").pop() ?? path,
      typeMatch?.[1] === "voice" ? "open" : undefined,
    );
  }

  // 2) 正文里的工作区路径（uploads/ 是 GUI 上传与系统分享的统一落盘目录，
  //    output/ 是 agent 生成产物目录，media/ 为媒体沙箱）。文件名保留中文
  //    原名（如 微信图片_*.jpg），字符集显式覆盖 CJK 与全角区；支持多级
  //    子目录（media/out/<sha>/x.jpg）。
  const fileName = "[A-Za-z0-9_\\-. ()\\u4e00-\\u9fff\\uff00-\\uffef]+";
  const reFull = new RegExp(
    `(?:media|uploads|output)/${fileName}(?:/${fileName})*\\.(?:png|jpe?g|webp|gif|mp4|webm|m4v|mov)`,
    "gi",
  );
  while ((m = reFull.exec(content)) !== null) {
    const p = m[0].replace(/[.,\s]+$/, "");
    push(p, p.split("/").pop() ?? p);
  }
  // 4) 文档引用：反引号包裹、带目录段的工作区相对路径（技能/agent 交付文档的
  //    固定书写式，如 `xianyu_output/<run>/items.csv`）——出「点开」卡，动作
  //    与媒体卡一致（系统打开 / 保存 / 转发）。正则放宽、循环里收紧：不含
  //    协议头；根目录段（首个 / 之前）必须是单一无点 token（`cat output/x.md`
  //    的首段是 "cat output"、"https://…" 的首段带冒号，都拒）；中间段允许
  //    空白——任务名可含空格（如 `xianyu_output/iPhone 15_…/items.csv`）。
  const reDoc =
    /`([^`\n]+\/[^`\n]+?\.(?:csv|tsv|md|markdown|txt|log|json|html?|pdf|zip|xlsx?|docx?|pptx?))`/gi;
  while ((m = reDoc.exec(content)) !== null) {
    const raw = m[1];
    if (raw.includes("://")) continue;
    const rootSeg = raw.replace(/\\/g, "/").split("/")[0];
    if (!rootSeg || /\s/.test(rootSeg) || rootSeg.includes(".") || rootSeg.includes(":")) {
      continue;
    }
    push(raw.replace(/\\/g, "/"), raw.split("/").pop() ?? raw, "open");
  }
  // 3) 裸文件名兜底：按惯例在 media/ 下找（agent 常只写文件名；找不到静默）。
  const reBare = /\b[A-Za-z0-9_\-]+\.(?:mp4|webm|m4v|mov|png|jpe?g|webp|gif)\b/gi;
  while ((m = reBare.exec(content)) !== null) {
    const name = m[0];
    if (found.some((it) => it.display === name || it.path.endsWith("/" + name))) continue;
    push("media/" + name, name, undefined, true);
  }
  return found;
}

/** 会话流内嵌媒体预览：识别助手消息里的媒体引用——运行时媒体回执行
 *  （`[:desktop-media type=… path=…]`，publish_media.py 发布成功后由 runtime 校验
 *  并追加到出站消息末尾）与正文里的工作区路径（media/…、uploads/…、output/…）。
 *  图片经 media_base64 读为 data URL 内嵌渲染；视频 / 文档出卡，点击经
 *  open_media 交系统播放器 / 查看器打开。
 *  操作菜单 / 全屏查看器 / 保存转发动作都在 Chat 层统一处理（微信式长按菜单）：
 *  - onMediaMenu：长按（移动端）/ 右键（桌面）媒体卡上抛；
 *  - onViewImage：单击图片上抛（Chat 打开全屏查看器）；
 *  - selectMode：消息多选模式下点击媒体卡 = 选中切换（点击冒泡到消息列，
 *    本组件不拦截），长按菜单停用。
 *  加载失败静默留空（失败降级为「点开」卡交给系统应用）。 */
export default function MediaPreviews({
  content,
  chatId,
  msgId,
  onError,
  selectMode = false,
  onMediaMenu,
  onViewImage,
  onOpenVideo,
}: {
  content: string;
  /** 会话（多端同步的附件清单按它取）。没有就不查清单，全部按工作区路径出卡。 */
  chatId?: string;
  /** 消息行 id（= Entry.msgId）：同一路径被几条消息引用时优先本消息登记的那份。 */
  msgId?: number;
  onError?: (message: string) => void;
  selectMode?: boolean;
  onMediaMenu?: (media: MediaMenuPayload, x: number, y: number, fromTouch: boolean) => void;
  onViewImage?: (payload: {
    /** 预览图 data URL（卡片已加载的同一份）。 */
    src: string;
    /** 工作区路径（查看器放大时按需加载原图）。 */
    path: string;
    display: string;
    card?: HTMLImageElement;
    /** 这条消息引用的附件对象名（画廊按它定位到点的那一份）。 */
    blob?: string;
  }) => void;
  /** 视频：应用内播放器（本地流式 URL，边下边播）。 */
  onOpenVideo?: (payload: {
    url: string;
    path: string;
    display: string;
    blob?: string;
  }) => void;
}) {
  const [urls, setUrls] = useState<Record<string, string>>({});
  const requested = useRef<Set<string>>(new Set());
  // 视频：本地流式 URL（127.0.0.1 Range 服务）。key=路径，value={url|""}，
  // ""=获取失败（降级为「点开」卡交给系统播放器）。
  const [videoUrls, setVideoUrls] = useState<Record<string, string>>({});
  const videoRequested = useRef<Set<string>>(new Set());
  const isMobile = useIsMobile();
  // 长按触发后吞掉紧随的 click（长按松手仍会派发 click，会误触「点开」/查看器）。
  const suppressClickRef = useRef(false);
  // 组件还活着、还是同一个会话：点播/预览这类异步结果回来时再确认一次，别把上一个
  // 会话的画廊开到新会话上。
  const mountedRef = useRef(true);
  const chatRef = useRef(chatId);
  chatRef.current = chatId;
  useEffect(() => {
    // StrictMode 会「setup → cleanup → setup」各跑一次：setup 里必须复位，
    // 否则开发环境里点播的异步结果会被当成「组件已卸载」丢掉。
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const items = useMemo(() => parseMediaItems(content), [content]);

  // 同步来的会话：消息里引用的文件可能不在这台设备上（还在别的设备上，或者只在同步
  // 缓存里）。界面先查这个会话的附件清单，命中的按附件卡出卡（下载 / 存进工作区），
  // 不再出一个点了只报「媒体文件不存在」的「点开」卡；清单里没有（本机文件、没开同步）
  // 的照旧按工作区路径出卡。
  const synced = useChatAttachments(chatId ?? null);
  const attachedReady = useChatAttachmentsReady(chatId ?? null);
  const attachedFailed = useChatAttachmentsFailed(chatId ?? null);
  // 没有 chatId（不在会话里用这个组件）就不查清单：按工作区路径出卡，与文档一致。
  const ready = chatId === undefined ? true : attachedReady;
  // 只认这条消息自己登记的附件：同一个路径在不同消息里可能指向不同内容（文件改过），
  // 拿别的消息的对象来下载会把错的版本落到这条消息头上。消息还没落库（没有 msgId）
  // 时按工作区路径出卡，落库之后清单刷新再换成附件卡。
  const mine = useMemo(() => {
    const map = new Map<string, SyncAttachment>();
    if (!chatId) return map;
    for (const item of messageFileRefs(synced, msgId)) map.set(item.path, item);
    return map;
  }, [chatId, msgId, synced]);
  // 消息路径本机真有这个文件（包括同一对象的别名已经补到原位置）：照旧按工作区路径
  // 出卡，图片/视频照常内联。其余的按附件卡给出取回入口。
  const away = useMemo(() => {
    const map = new Map<string, SyncAttachment>();
    for (const [path, item] of mine) {
      // 核对过、消息路径上没有这份内容：出附件卡。还没核对过的按「能读」处理
      // （读取前后端再核一次），不然大文件会被预算一直挡在附件卡后面。
      if (!readableAtPath(item)) map.set(path, item);
    }
    return map;
  }, [mine]);
  const awayPaths = useMemo(() => new Set(away.keys()), [away]);
  // 预览/视频的请求与结果按「路径 + 期望的对象名」记：清单晚到时对象名才出现，同一
  // 条路径上的不同版本不能共用一次请求的结论。
  const keyFor = (item: PreviewItem) =>
    `${item.path}|${mine.get(item.path)?.blob ?? ""}`;
  // 文本解析没认出来、但这条消息确实登记了的附件（`attach::extract_refs` 认正文里
  // 裸写的 `output/report.pdf`，媒体卡的正则只认带反引号的文档）：补一张卡，否则
  // 用户在这台设备上没有取回入口。补齐按「这条消息登记的全部附件」算——只按缺卡的
  // 补的话，下载完成、文件落回原位置之后这张卡会凭空消失，而不是变成「点开」。
  const shown = useMemo(() => {
    // 正文里的裸文件名会被解析成 `media/<名字>` 的猜测；这条消息登记的是另一个完整
    // 路径（同名）时，猜测让位——否则本机恰好有 `media/<名字>` 时会显示一个不相干的
    // 文件（绕过了按对象的核对）。
    const refNames = new Set(
      [...mine.keys()].map((path) => path.split("/").pop() ?? path),
    );
    // 只有「解析器按裸文件名猜出来」的条目才让位；正文里明确写出的路径（哪怕也是
    // `media/cover.png`）照常出卡。
    const parsed = items.filter((item) => {
      if (mine.has(item.path) || !item.inferred) return true;
      const name = item.path.split("/").pop() ?? item.path;
      return !refNames.has(name);
    });
    if (mine.size === 0) return parsed;
    const seen = new Set(parsed.map((item) => item.path));
    const extra: PreviewItem[] = [];
    for (const path of mine.keys()) {
      if (seen.has(path)) continue;
      // 只补「这条消息的正文里真的写到了」的路径：附件清单是从整行的数据里抽的，
      // 工具调用的参数、思考里提到的文件也在里面，它们不该以图片/文件卡的形式挂到
      // 这条可见消息下面。
      if (!mentionsPath(content, path)) continue;
      seen.add(path);
      const image = /\.(?:png|jpe?g|webp|gif)$/i.test(path);
      extra.push({
        key: path,
        path,
        display: path.split("/").pop() || path,
        kind: image ? "image" : "open",
      });
    }
    return extra.length === 0 ? parsed : [...parsed, ...extra];
  }, [items, mine, content]);

  // 视频卡：拉本地流式 URL（含首帧由 <video preload=metadata> 显示）。
  useEffect(() => {
    for (const item of shown) {
      // 清单还没回来：这时按工作区路径取视频可能取到同名但不是这条消息引用的文件。
      if (!ready) break;
      if (awayPaths.has(item.path)) {
        // 清单比首屏晚到时，预览可能已经按本机路径试过一遍（文件不在，记成失败）。
        // 允许重新取：下载完成、清单刷新后这张卡要能自己换回视频/图片。
        videoRequested.current.delete(keyFor(item));
        continue;
      }
      if (item.kind !== "open") continue;
      if (!/\.(?:mp4|webm|m4v|mov)$/i.test(item.path)) continue; // 音频等仍走系统播放器
      const itemKey = keyFor(item);
      if (videoRequested.current.has(itemKey)) continue;
      videoRequested.current.add(itemKey);
      const blob = mine.get(item.path)?.blob;
      api
        // 海报帧用摘要缓存的结论核对（不整读整个视频）；点播时再直读核对一次。
        .mediaStreamUrl(item.path, chatId, blob, false)
        .then((r) => {
          if (!mountedRef.current || chatRef.current !== chatId) return;
          // 海报地址（钉版本 / 核不了时的原路径）不代表这一次核对通过：不清覆盖。
          if (r.verified && blob) clearAttachmentChanged({ path: item.path, blob });
          setVideoUrls((prev) => ({ ...prev, [itemKey]: r.url }));
        })
        .catch((e) => {
          if (!mountedRef.current || chatRef.current !== chatId) return;
          // 失败不占着去重集合：清单核对出这一条之后 effect 要能再试一次（否则
          // 预算外的视频永远出不来内联播放器）。
          videoRequested.current.delete(itemKey);
          // 内容对不上（缓存副本被改写过）：这张卡立刻换成附件卡（重新下载），
          // 不等清单下一轮核对。
          if (blob && isAttachmentChangedError(e)) {
            markAttachmentChanged({ path: item.path, blob });
          }
          setVideoUrls((prev) => ({ ...prev, [itemKey]: "" }));
        });
    }
  }, [shown, awayPaths, ready, mine, chatId]);

  useEffect(() => {
    for (const item of shown) {
      if (item.kind !== "image") continue; // 视频/文档不预载：点卡片经系统应用打开
      // 清单还没回来：先不按工作区路径取图（可能是同名却不是这条消息引用的那份）。
      if (!ready) break;
      if (awayPaths.has(item.path)) {
        // 不在这台设备上：附件卡自己出预览。首屏清单还没到时可能已按本机路径试过
        // 一次并记成失败，这里把去重记下，取回之后要能重新出图。
        requested.current.delete(keyFor(item));
        continue;
      }
      const itemKey = keyFor(item);
      if (requested.current.has(itemKey)) continue;
      requested.current.add(itemKey);
      const blob = mine.get(item.path)?.blob;
      void api
        // 卡片用屏幕级预览图（后端缩到长边 1440 并缓存）：不再传输/解码
        // 12MP 原图（9MB base64 + 全图解码是真机点开图片慢的源头）。
        .mediaPreview(item.path, chatId, blob)
        .then((r) => {
          if (!mountedRef.current || chatRef.current !== chatId) return;
          // 这次读的是核对过的字节才算通过（核不了时按老行为读路径出图，不清）。
          if (r.verified && blob) clearAttachmentChanged({ path: item.path, blob });
          setUrls((prev) => ({ ...prev, [itemKey]: `data:${r.mime};base64,${r.base64}` }));
        })
        .catch((e) => {
          // 组件已经卸载/切了会话：别把旧会话的结论写进当前会话的覆盖集。
          if (!mountedRef.current || chatRef.current !== chatId) return;
          // 一次暂时性读失败不该把这张图永久降级成「点开」：去重集合释放，清单
          // 下一次刷新时再试。
          requested.current.delete(itemKey);
          if (blob && isAttachmentChangedError(e)) {
            markAttachmentChanged({ path: item.path, blob });
          }
          setUrls((prev) => ({ ...prev, [itemKey]: "" }));
        });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shown, awayPaths, ready, mine, chatId]);

  // ---- 长按 / 右键上抛（菜单在 Chat 层统一渲染） ----

  // 触摸序列交给 lib/longPress 的 window capture 守卫（与消息行同一套 slop/
  // 计时/滚动漂移校验；媒体卡也会被流式重渲染替换 DOM，行级 move 跟踪同样会
  // 丢）。这里只记录待弹的媒体与滚动基线来源（最近的 .chat-msg-list）。选择
  // 模式下长按停用（点击即选中切换）。
  const pressGuardRef = useRef<LongPressGuard | null>(null);
  const pressItemRef = useRef<MediaMenuPayload | null>(null);
  const pressListElRef = useRef<Element | null>(null);
  // 守卫只创建一次，onFire 闭包里不能直接用渲染期的 onMediaMenu（每渲染都是
  // 新箭头、捕获当轮 props）——经 ref 取最新，避免流式期间菜单拿到旧回调。
  const onMediaMenuRef = useRef(onMediaMenu);
  onMediaMenuRef.current = onMediaMenu;
  const selectModeRef = useRef(selectMode);
  selectModeRef.current = selectMode;
  const getPressGuard = () => {
    // lazy init：useRef(createLongPressGuard(...)) 会在每次渲染新建实例。
    if (!pressGuardRef.current) {
      pressGuardRef.current = createLongPressGuard({
        getScrollTop: () =>
          pressListElRef.current instanceof HTMLElement
            ? pressListElRef.current.scrollTop
            : null,
        onFire: (x, y) => {
          const item = pressItemRef.current;
          // 按下后 450ms 内进入了选择模式：菜单让位（点击语义已变为选中切换）。
          if (selectModeRef.current) return;
          if (!item || !onMediaMenuRef.current) return;
          // 长按触发后的合成 click 必须吞掉（600ms 窗口），否则菜单弹出的
          // 同一瞬间图片查看器 / 视频播放器跟着开。
          suppressClickRef.current = true;
          window.setTimeout(() => (suppressClickRef.current = false), 600);
          if (navigator.vibrate) navigator.vibrate(15);
          onMediaMenuRef.current(item, x, y, true);
        },
      });
    }
    return pressGuardRef.current;
  };
  const startPress = (item: PreviewItem, el: Element, x: number, y: number) => {
    if (selectMode || !onMediaMenu) return;
    pressItemRef.current = { path: item.path, display: item.display, kind: item.kind };
    pressListElRef.current = el.closest(".chat-msg-list");
    getPressGuard().start(x, y);
  };

  // 组件卸载清计时器与 window 监听。
  useEffect(() => () => pressGuardRef.current?.cancel(), []);

  const openItem = (item: PreviewItem) => {
    if (suppressClickRef.current) {
      suppressClickRef.current = false;
      return;
    }
    // 这条消息登记过的附件：带上对象名打开，后端先核对内容——清单核过之后文件仍可能
    // 被改写，这是不让用户看到另一个版本的最后一道关。
    const attachment = mine.get(item.path);
    const open =
      attachment && chatId
        ? api.syncOpenAttachment(chatId, attachment.blob, item.path)
        : api.openMedia(item.path);
    void open.then((verified) => {
      // 打开的是核对过的副本才算这次读通过（核不了时按老行为打开原路径）。
      if (verified && attachment) clearAttachmentChanged(attachment);
    });
    void open.catch((e) => {
      onError?.(`打开媒体失败: ${String(e)}`);
      if (attachment && isAttachmentChangedError(e)) {
        // 核对没过（文件被就地改写过）：这条立刻按「对不上」出附件卡（带重新
        // 下载），不等清单下一轮核对（预算外的那条可能根本轮不到）。
        markAttachmentChanged(attachment);
      }
      // 重取清单：核对过的条目状态会跟着变。
      refreshSyncAttachments();
    });
  };

  // 图片读取完成（无论成败）即出卡：失败（urls[p] === ""，如超 25MB 上限、
  // 文件已被清理）降级为「点开」卡交给系统应用，正文引用不再凭空消失。不在这台设备
  // 上的附件（附件清单里有它）始终出卡——按清单状态给「下载」/「存进工作区」。
  const rendered = shown
    .map((it) => ({ it, away: away.get(it.path) }))
    .filter(
      ({ it, away }) =>
        // 清单还没就绪（或取不到）时，图片也要留在列表里——否则首屏失败后图片消息
        // 连「点此重试」都看不到。
        !ready ||
        away !== undefined ||
        it.kind === "open" ||
        urls[keyFor(it)] !== undefined,
    );
  if (rendered.length === 0) return null;
  return (
    // max-w-full 压住容器的 fit-content：长文件名的「点开」卡不受卡片自身
    // max-w-full 约束（百分比相对本容器，会循环撑开），把消息列撑出屏幕。
    <div className="mt-1 flex max-w-full flex-col gap-2">
      {rendered.map(({ it, away: awayItem }) => {
        const itemKey = keyFor(it);
        if (!ready) {
          // 清单还没核对好（刚合进新消息、正在重取）：这一步连已经加载过的图片/
          // 视频也不再出可点的卡——这时按工作区路径看到、打开或存下的可能是同名的
          // 另一个版本。清单到了再换回来。
          return (
            <div
              key={itemKey}
              role={attachedFailed ? "button" : undefined}
              onClick={attachedFailed ? () => refreshSyncAttachments() : undefined}
              className="flex max-w-full items-center gap-2 self-start rounded-xl border border-[#2a2f3a] bg-[#0f131b] px-3 py-2 text-left opacity-60"
            >
              <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-[#1a1f29] text-gray-400">
                …
              </span>
              <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-gray-400">
                {it.display || it.path}
              </span>
              <span className="shrink-0 text-[10px] text-gray-500">
                {attachedFailed ? "附件清单不可用 · 点此重试" : "核对附件…"}
              </span>
            </div>
          );
        }
        if (awayItem && chatId) {
          return (
            // 多选模式下点卡片 = 选中这条消息（与图片/视频卡一致）：卡片上的按钮
            // 不吃点击，事件落到消息行。
            <div key={itemKey} className={selectMode ? "pointer-events-none" : undefined}>
              <AttachmentCard
                chatId={chatId}
                item={awayItem}
                name={it.display}
                inactive={selectMode}
                onError={onError ?? (() => {})}
              />
            </div>
          );
        }
        // 长按 / 右键 handlers（各媒体卡共用）。选择模式下点击交给消息列的
        // 选中切换，本组件只拦截触摸冒泡（防消息气泡长按守卫同时上抛）。
        const handlers = {
          onTouchStart: (e: TouchEvent) => {
            if (!isMobile) return;
            if (e.touches.length !== 1) {
              // 双指（会话缩放）：捏合不是长按，取消媒体卡自己的守卫（按住不动也
              // 不该弹菜单）。
              pressGuardRef.current?.cancel();
              return;
            }
            // 阻止冒泡：消息气泡也有长按（统一菜单），媒体卡菜单不能同时触发。
            // 不在此处做任何图片解码预热：真机实测 WebView83 对 12MP data URL
            // 的 decode() 会阻塞主线程 1~2s，把点击响应整个挡住——查看器即时
            // 挂载 + decoding=async 已足够（遮罩先出、大图解码后光栅上屏）。
            e.stopPropagation();
            if (!selectMode) {
              startPress(
                it,
                e.currentTarget as Element,
                e.touches[0].clientX,
                e.touches[0].clientY,
              );
            }
          },
          onContextMenu: (e: MouseEvent) => {
            e.preventDefault();
            e.stopPropagation();
            // 安卓：系统长按 ~500ms 补发的原生 contextmenu 不弹应用菜单——应用
            // 菜单由长按守卫的 450ms 计时器负责（fromTouch=false 无 click 吞吐，
            // 从这里弹会被合成 click 瞬间关掉）。守卫顺带 cancel：正常路径计时
            // 器已触发（cancel 无害）；计时器被事件循环压迟到 contextmenu 之后
            // 时，避免迟到的菜单再弹出。安卓 + 鼠标右键随之牺牲（触摸长按是
            // 主路径）。桌面右键即时弹（不走 450ms 长按计时）。
            if (isAndroidDevice()) {
              pressGuardRef.current?.cancel();
              return;
            }
            if (!selectMode && onMediaMenu) {
              onMediaMenu(
                { path: it.path, display: it.display, kind: it.kind },
                e.clientX,
                e.clientY,
                false,
              );
            }
          },
        };
        // 视频容器：本地流式 URL 就绪 → 内嵌首帧视频卡（点击应用内播放）；
        // 未就绪/获取失败 → 旧的「点开」卡交给系统播放器。
        const isVideoFile = it.kind === "open" && /\.(?:mp4|webm|m4v|mov)$/i.test(it.path);
        const streamUrl = isVideoFile ? videoUrls[itemKey] : undefined;
        if (isVideoFile && streamUrl) {
          return (
            <video
              key={itemKey}
              src={`${streamUrl}#t=0.1`}
              preload="metadata"
              muted
              playsInline
              {...handlers}
              onError={() => {
                // 流断开 / 容器解不了（如个别 MOV），或者核对过的版本已经被改写
                // （409）：降级为「点开」卡交给系统播放器；去重记录也清掉，下次能重取。
                videoRequested.current.delete(itemKey);
                setVideoUrls((prev) => ({ ...prev, [itemKey]: "" }));
              }}
              onClick={() => {
                if (selectMode) return; // 冒泡到消息列的选中切换
                // 长按菜单触发后的合成 click 吞掉（与图片卡同款），否则菜单
                // 弹出的同一瞬间播放器跟着开。
                if (suppressClickRef.current) {
                  suppressClickRef.current = false;
                  return;
                }
                // 点播：重新取一个「直读核对过」的流地址（海报那个是按缓存结论核的）。
                const blob = mine.get(it.path)?.blob;
                api
                  .mediaStreamUrl(it.path, chatId, blob, true)
                  .then((r) => {
                    if (!mountedRef.current || chatRef.current !== chatId) return;
                    // 只有暂存副本（核对过的字节）才算这次读通过；核不了退回的
                    // 原路径不算。
                    if (r.verified && blob) clearAttachmentChanged({ path: it.path, blob });
                    onOpenVideo?.({
                      url: r.url,
                      path: it.path,
                      display: it.display || it.path,
                      blob,
                    });
                  })
                  .catch((e) => {
                    if (!mountedRef.current) return;
                    onError?.(`打开视频失败: ${String(e)}`);
                    // 核对没过（文件被改写）：这张卡立刻按「对不上」出附件卡，
                    // 并重取清单核对过的状态会跟着变。
                    if (blob && isAttachmentChangedError(e)) {
                      markAttachmentChanged({ path: it.path, blob });
                    }
                    refreshSyncAttachments();
                  });
              }}
              className="max-h-72 w-auto max-w-full cursor-pointer self-start rounded-xl border border-[#2a2f3a]"
              title={`${it.display || it.path}（点击播放；长按 / 右键更多操作）`}
            />
          );
        }
        return it.kind === "open" || (urls[itemKey] ?? "") === "" ? (
          <button
            key={itemKey}
            {...handlers}
            onClick={() => {
              if (selectMode) return; // 冒泡到消息列的选中切换
              openItem(it);
            }}
            className="flex max-w-full items-center gap-2 self-start rounded-xl border border-[#2a2f3a] bg-[#0f131b] px-3 py-2 text-left transition hover:border-indigo-500/50"
            title="点击用系统应用打开（长按 / 右键更多操作）"
          >
            <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-indigo-600 text-white">
              {/\.(?:mp4|webm|m4v|mov)$/i.test(it.path) ? "▶" : "📄"}
            </span>
            <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-gray-300">
              {it.display || it.path}
            </span>
            <span className="shrink-0 text-[10px] text-gray-500">点开</span>
          </button>
        ) : (
          <img
            key={itemKey}
            src={urls[itemKey]}
            alt={it.display || it.path}
            // 视口外不加载不解码（1440 预览也有数百 KB），列表滚动不卡顿。
            loading="lazy"
            decoding="async"
            title={`${it.path}（点击放大；长按 / 右键：保存 / 转发）`}
            className="max-h-72 w-auto max-w-full cursor-zoom-in self-start rounded-xl border border-[#2a2f3a]"
            onClick={(e) => {
              if (selectMode) return; // 冒泡到消息列的选中切换
              // 长按菜单触发后的合成 click 吞掉，只有干净轻点才进查看器。
              if (suppressClickRef.current) {
                suppressClickRef.current = false;
                return;
              }
              // 卡片元素一并上抛（已解码，查看器 canvas 即时预览用）；
              // src 即预览图，放大看原图时查看器再按 path 加载原图。
              onViewImage?.({
                src: urls[itemKey],
                path: it.path,
                display: it.display || it.path,
                card: e.currentTarget,
                blob: mine.get(it.path)?.blob,
              });
            }}
            {...handlers}
          />
        );
      })}
    </div>
  );
}
