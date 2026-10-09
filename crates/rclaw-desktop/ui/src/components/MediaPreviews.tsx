import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent,
  type TouchEvent,
} from "react";
import { api } from "../lib/api";
import { useIsMobile } from "../lib/useIsMobile";

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

/** 从消息正文解析媒体引用条目（媒体回执行 + 工作区路径 + 裸文件名兜底）。
 *  顺序即出现顺序，同路径去重。除本组件出卡外，Chat 的全屏媒体画廊也用
 *  同一解析把全会话的图片/视频拼成可左右滑切换的时间线（口径必须一致，
 *  点击的卡片才保证能在画廊里对上号）。 */
export function parseMediaItems(content: string): PreviewItem[] {
  const found: PreviewItem[] = [];
  const seen = new Set<string>();
  const push = (path: string, display: string, kind?: "image" | "open") => {
    if (seen.has(path)) return;
    seen.add(path);
    const isImg = /\.(?:png|jpe?g|webp|gif)$/i.test(path);
    found.push({
      key: path,
      path,
      display,
      kind: kind ?? (isImg ? "image" : "open"),
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
  // 3) 裸文件名兜底：按惯例在 media/ 下找（agent 常只写文件名；找不到静默）。
  const reBare = /\b[A-Za-z0-9_\-]+\.(?:mp4|webm|m4v|mov|png|jpe?g|webp|gif)\b/gi;
  while ((m = reBare.exec(content)) !== null) {
    const name = m[0];
    if (found.some((it) => it.display === name || it.path.endsWith("/" + name))) continue;
    push("media/" + name, name);
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
  onError,
  selectMode = false,
  onMediaMenu,
  onViewImage,
  onOpenVideo,
}: {
  content: string;
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
  }) => void;
  /** 视频：应用内播放器（本地流式 URL，边下边播）。 */
  onOpenVideo?: (payload: { url: string; path: string; display: string }) => void;
}) {
  const [urls, setUrls] = useState<Record<string, string>>({});
  const requested = useRef<Set<string>>(new Set());
  // 视频：本地流式 URL（127.0.0.1 Range 服务）。key=路径，value={url|""}，
  // ""=获取失败（降级为「点开」卡交给系统播放器）。
  const [videoUrls, setVideoUrls] = useState<Record<string, string>>({});
  const videoRequested = useRef<Set<string>>(new Set());
  const isMobile = useIsMobile();
  const pressTimer = useRef<number | null>(null);
  // 长按触发后吞掉紧随的 click（长按松手仍会派发 click，会误触「点开」/查看器）。
  const suppressClickRef = useRef(false);

  const items = useMemo(() => parseMediaItems(content), [content]);

  // 视频卡：拉本地流式 URL（含首帧由 <video preload=metadata> 显示）。
  useEffect(() => {
    for (const item of items) {
      if (item.kind !== "open") continue;
      if (!/\.(?:mp4|webm|m4v|mov)$/i.test(item.path)) continue; // 音频等仍走系统播放器
      if (videoRequested.current.has(item.key)) continue;
      videoRequested.current.add(item.key);
      api
        .mediaStreamUrl(item.path)
        .then((url) => setVideoUrls((prev) => ({ ...prev, [item.key]: url })))
        .catch(() => setVideoUrls((prev) => ({ ...prev, [item.key]: "" })));
    }
  }, [items]);

  useEffect(() => {
    for (const item of items) {
      if (item.kind !== "image") continue; // 视频/文档不预载：点卡片经系统应用打开
      if (requested.current.has(item.key)) continue;
      requested.current.add(item.key);
      void api
        // 卡片用屏幕级预览图（后端缩到长边 1440 并缓存）：不再传输/解码
        // 12MP 原图（9MB base64 + 全图解码是真机点开图片慢的源头）。
        .mediaPreview(item.path)
        .then((r) =>
          setUrls((prev) => ({ ...prev, [item.key]: `data:${r.mime};base64,${r.base64}` })),
        )
        .catch(() => setUrls((prev) => ({ ...prev, [item.key]: "" })));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [items]);

  // ---- 长按 / 右键上抛（菜单在 Chat 层统一渲染） ----

  // 长按起点：move 位移超 12px（touch slop）才取消——同坐标长按与手指微抖都
  // 会持续派发 touchmove，无条件取消会让长按永远不触发。
  const pressOrigin = useRef<{ x: number; y: number } | null>(null);

  const cancelPress = () => {
    pressOrigin.current = null;
    if (pressTimer.current !== null) {
      window.clearTimeout(pressTimer.current);
      pressTimer.current = null;
    }
  };

  // 长按 450ms 上抛（Chat 弹统一菜单，轻震动反馈）；选择模式下长按停用
  // （点击即选中切换）。
  const startPress = (item: PreviewItem, x: number, y: number) => {
    if (selectMode || !onMediaMenu) return;
    cancelPress();
    pressOrigin.current = { x, y };
    pressTimer.current = window.setTimeout(() => {
      pressTimer.current = null;
      suppressClickRef.current = true;
      window.setTimeout(() => (suppressClickRef.current = false), 600);
      if (navigator.vibrate) navigator.vibrate(15);
      onMediaMenu(
        { path: item.path, display: item.display, kind: item.kind },
        x,
        y,
        true,
      );
    }, 450);
  };

  const movePress = (x: number, y: number) => {
    const o = pressOrigin.current;
    if (o && Math.hypot(x - o.x, y - o.y) > 12) cancelPress();
  };

  // 组件卸载清定时器（返回 cancelPress 作 cleanup）。
  useEffect(() => cancelPress, []);

  const openItem = (item: PreviewItem) => {
    if (suppressClickRef.current) {
      suppressClickRef.current = false;
      return;
    }
    void api.openMedia(item.path).catch((e) => onError?.(`打开媒体失败: ${String(e)}`));
  };

  // 图片读取完成（无论成败）即出卡：失败（urls[p] === ""，如超 25MB 上限、
  // 文件已被清理）降级为「点开」卡交给系统应用，正文引用不再凭空消失。
  const rendered = items.filter((it) => it.kind === "open" || urls[it.key] !== undefined);
  if (rendered.length === 0) return null;
  return (
    // max-w-full 压住容器的 fit-content：长文件名的「点开」卡不受卡片自身
    // max-w-full 约束（百分比相对本容器，会循环撑开），把消息列撑出屏幕。
    <div className="mt-1 flex max-w-full flex-col gap-2">
      {rendered.map((it) => {
        // 长按 / 右键 handlers（各媒体卡共用）。选择模式下点击交给消息列的
        // 选中切换，本组件只拦截触摸冒泡（防消息气泡长按计时器叠加）。
        const handlers = {
          onTouchStart: (e: TouchEvent) => {
            if (!isMobile || e.touches.length === 0) return;
            // 阻止冒泡：消息气泡也有长按（统一菜单），媒体卡菜单不能同时触发。
            // 不在此处做任何图片解码预热：真机实测 WebView83 对 12MP data URL
            // 的 decode() 会阻塞主线程 1~2s，把点击响应整个挡住——查看器即时
            // 挂载 + decoding=async 已足够（遮罩先出、大图解码后光栅上屏）。
            e.stopPropagation();
            if (!selectMode) startPress(it, e.touches[0].clientX, e.touches[0].clientY);
          },
          onTouchEnd: cancelPress,
          onTouchMove: (e: TouchEvent) => {
            if (e.touches[0]) movePress(e.touches[0].clientX, e.touches[0].clientY);
          },
          onTouchCancel: cancelPress,
          onContextMenu: (e: MouseEvent) => {
            e.preventDefault();
            e.stopPropagation();
            // 右键即时弹（与消息气泡一致），不走 450ms 长按计时；fromTouch=false
            // 不布防 click 吞吐（吞吐只服务于长按抬指的合成 click）。
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
        const streamUrl = isVideoFile ? videoUrls[it.key] : undefined;
        if (isVideoFile && streamUrl) {
          return (
            <video
              key={it.key}
              src={`${streamUrl}#t=0.1`}
              preload="metadata"
              muted
              playsInline
              {...handlers}
              onError={() => {
                // 流断开 / 容器解不了（如个别 MOV）：降级为「点开」卡交给系统
                // 播放器，不留一块空黑板。
                setVideoUrls((prev) => ({ ...prev, [it.key]: "" }));
              }}
              onClick={() => {
                if (selectMode) return; // 冒泡到消息列的选中切换
                // 长按菜单触发后的合成 click 吞掉（与图片卡同款），否则菜单
                // 弹出的同一瞬间播放器跟着开。
                if (suppressClickRef.current) {
                  suppressClickRef.current = false;
                  return;
                }
                onOpenVideo?.({
                  url: streamUrl,
                  path: it.path,
                  display: it.display || it.path,
                });
              }}
              className="max-h-72 w-auto max-w-full cursor-pointer self-start rounded-xl border border-[#2a2f3a]"
              title={`${it.display || it.path}（点击播放；长按 / 右键更多操作）`}
            />
          );
        }
        return it.kind === "open" || (urls[it.key] ?? "") === "" ? (
          <button
            key={it.key}
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
            key={it.key}
            src={urls[it.key]}
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
                src: urls[it.key],
                path: it.path,
                display: it.display || it.path,
                card: e.currentTarget,
              });
            }}
            {...handlers}
          />
        );
      })}
    </div>
  );
}
