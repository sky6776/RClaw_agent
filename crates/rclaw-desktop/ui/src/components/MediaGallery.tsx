import { useEffect, useRef, useState } from "react";
import { api } from "../lib/api";
import { useSwipeNav } from "../lib/swipeNav";
import {
  isAttachmentChangedError,
  markAttachmentChanged,
  refreshSyncAttachments,
} from "./SyncAttachments";
import ImageViewer from "./ImageViewer";
import VideoPlayer from "./VideoPlayer";

/** 会话媒体画廊条目（Chat 从全部消息解析后传入，见 parseMediaItems）。 */
export interface GalleryItem {
  /** 条目的稳定身份：路径 + 期望的对象名（同一路径的不同版本是两条）。 */
  key: string;
  path: string;
  display: string;
  kind: "image" | "video";
  /** 这条消息引用的附件对象名：读取（预览/播放）前按它核对文件内容。 */
  blob?: string;
}

/** 打开画廊时的种子数据（点击的媒体卡已在手的资源，首屏免重复取数）。 */
export interface GallerySeed {
  path: string;
  /** 期望的对象名：只有点到的那一份用这个种子。 */
  blob?: string;
  /** 图片预览 data URL（卡片同款）。 */
  src?: string;
  /** 媒体卡 <img> 元素（查看器 canvas 即时预览用，只有点开的这一张有）。 */
  preview?: HTMLImageElement | null;
  /** 视频本地流式 URL。 */
  url?: string;
}

/** 全屏媒体画廊（微信行为）：把全屏图片查看器与应用内视频播放器串成一条
 *  按消息顺序排列的时间线，左右滑切换上一个/下一个（到首张/末张即止，不
 *  循环）。当前条目按需取数——图片 mediaPreview（后端有磁盘缓存）、视频
 *  mediaStreamUrl（流服务是常驻单例，重复取 URL 无泄漏）；取数期间黑底
 *  spinner，失败（如文件已被清理）给提示，两种画面都可左右滑走不卡死。
 *  条目切换用 key 强制重建查看器——缩放/平移状态不跨张携带；Chat 侧再以
 *  key={start} 挂载本组件，画廊开着时点另一张卡也能正确定位。 */
function seedMatches(seed: GallerySeed, item: GalleryItem): boolean {
  // 种子只给「同一路径、同一对象」的那一份用（`undefined` 只匹配未登记的那一份）。
  return seed.path === item.path && seed.blob === item.blob;
}

export default function MediaGallery({
  items,
  start,
  seed,
  chatId,
  startBlob,
  onError,
  onClose,
}: {
  items: GalleryItem[];
  /** 打开时点击的条目路径（时间线定位起点）。 */
  start: string;
  /** 点击的那一份的对象名（同一路径有多个版本时定位到点的那一份）。 */
  startBlob?: string;
  seed?: GallerySeed | null;
  /** 会话（按对象核对附件读取用）。 */
  chatId?: string;
  onError?: (message: string) => void;
  onClose: () => void;
}) {
  const startIndex = Math.max(
    0,
    // 对象名按原样比（`undefined` 也是一种身份：未登记的那一份）。
    items.findIndex((it) => it.path === start && it.blob === startBlob),
  );
  const [index, setIndex] = useState(startIndex);
  const item = items[index];
  // 图片预览 data URL：null=取数中，""=失败。
  // 本次画廊会话里已经看过的条目：种子里的流地址只在**第一次**进这条时用（点播刚
  // 核对过）；划走再划回来时暂存副本可能已被回收，重新取一个核对过的新地址。
  const visitedKeys = useRef<Set<string>>(new Set());
  const [imgSrc, setImgSrc] = useState<string | null>(null);
  // 视频本地流式 URL：null=取数中，""=失败。
  const [videoUrl, setVideoUrl] = useState<string | null>(null);

  useEffect(() => {
    setImgSrc(null);
    setVideoUrl(null);
    if (!item) return;
    let alive = true;
    if (item.kind === "video") {
      const first = !visitedKeys.current.has(item.key);
      visitedKeys.current.add(item.key);
      if (first && seed && seedMatches(seed, item) && seed.url) {
        // 点播刚核对并复制过：首屏直接用这个地址，不再复制第二份。
        setVideoUrl(seed.url);
        return;
      }
      api
        .mediaStreamUrl(item.path, chatId, item.blob, true)
        .then((u) => {
          if (alive) setVideoUrl(u);
        })
        .catch((e) => {
          if (!alive) return;
          // 读出来发现内容对不上：消息里的内联卡也换成附件卡（带重新下载），别让
          // 用户只能在画廊里看到一个失败画面。
          if (item.blob && isAttachmentChangedError(e)) {
            markAttachmentChanged({ path: item.path, blob: item.blob });
            refreshSyncAttachments();
          }
          setVideoUrl("");
        });
    } else {
      if (seed && seedMatches(seed, item) && seed.src) {
        setImgSrc(seed.src);
        return;
      }
      api
        .mediaPreview(item.path, chatId, item.blob)
        .then((r) => {
          if (alive) setImgSrc(`data:${r.mime};base64,${r.base64}`);
        })
        .catch((e) => {
          if (!alive) return;
          if (item.blob && isAttachmentChangedError(e)) {
            markAttachmentChanged({ path: item.path, blob: item.blob });
            refreshSyncAttachments();
          }
          setImgSrc("");
        });
    }
    return () => {
      alive = false;
    };
  }, [item, seed, chatId]);

  // 边界方向不传回调：查看器把该方向的滑动当普通拖拽，松手弹回（不循环）。
  const onPrev = index > 0 ? () => setIndex(index - 1) : undefined;
  const onNext = index < items.length - 1 ? () => setIndex(index + 1) : undefined;
  // 取数/失败画面也接轻扫——滑进一条加载失败（如文件已被清理）的媒体后仍能
  // 左右滑走，不会卡死在失败遮罩里（hook 须在任何条件 return 之前调用）。
  const fallbackSwipe = useSwipeNav(onPrev, onNext);

  if (!item) return null;

  if (item.kind === "image" && imgSrc) {
    return (
      <ImageViewer
        key={item.key}
        src={imgSrc}
        path={item.path}
        chatId={chatId}
        blob={item.blob}
        display={item.display}
        preview={seed && seedMatches(seed, item) ? seed.preview ?? null : null}
        onError={onError}
        onClose={onClose}
        onPrev={onPrev}
        onNext={onNext}
      />
    );
  }
  if (item.kind === "video" && videoUrl) {
    return (
      <VideoPlayer
        key={item.key}
        url={videoUrl}
        display={item.display}
        onClose={onClose}
        onPrev={onPrev}
        onNext={onNext}
      />
    );
  }
  // 取数中 / 失败：黑底 spinner 或失败提示（关闭常驻，返回键走 Chat 的注册）。
  const failed = item.kind === "image" ? imgSrc === "" : videoUrl === "";
  return (
    <div
      className="fixed inset-0 z-[60] flex touch-none select-none items-center justify-center bg-black/95"
      onTouchStart={fallbackSwipe.onTouchStart}
      onTouchEnd={fallbackSwipe.onTouchEnd}
      onTouchCancel={fallbackSwipe.onTouchCancel}
    >
      <div
        className="absolute left-1/2 top-0 z-10 flex -translate-x-1/2 items-center gap-2 px-3 py-1 text-[11px] text-gray-400"
        style={{ marginTop: "calc(env(safe-area-inset-top) + 0.5rem)" }}
      >
        <span className="max-w-[60vw] truncate font-mono" title={item.display}>
          {item.display}
        </span>
      </div>
      <button
        onClick={onClose}
        aria-label="关闭"
        className="absolute right-3 z-10 flex h-10 w-10 items-center justify-center rounded-full bg-white/10 text-xl text-white transition active:bg-white/20"
        style={{ top: "calc(env(safe-area-inset-top) + 0.5rem)" }}
      >
        ✕
      </button>
      {failed ? (
        <span className="text-sm text-gray-400">媒体加载失败</span>
      ) : (
        <span className="block h-8 w-8 animate-spin rounded-full border-2 border-white/70 border-t-transparent" />
      )}
    </div>
  );
}
