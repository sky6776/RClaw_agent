import { useRef } from "react";
import { useBackClose } from "../lib/backNav";
import { useSwipeNav } from "../lib/swipeNav";

/** 应用内视频播放器（本地流式 URL，边下边播）：
 * - 原生 controls（播放/暂停/进度/全屏），WebView 直接支持；
 * - 关闭：右上 ✕ 或系统返回键（经 backNav 弹层栈接管）；
 * - 媒体画廊模式：水平轻扫切上一个/下一个（onPrev/onNext 缺省 = 单视频），
 *   原生控制条区域（进度条拖拽）不参与；
 * - 参考开源播放器（Fossify Gallery/Aves）的极简全屏形态：黑底 + 居中视频。 */

interface Props {
  /** 本地流式 URL（127.0.0.1 Range 服务）。 */
  url: string;
  display: string;
  onClose: () => void;
  /** 画廊切换回调（见 ImageViewer 同名 props）。 */
  onPrev?: () => void;
  onNext?: () => void;
}

export default function VideoPlayer({ url, display, onClose, onPrev, onNext }: Props) {
  useBackClose(true, onClose);
  const videoRef = useRef<HTMLVideoElement | null>(null);
  // 原生控制条区域的横向拖动是进度条拖拽，不算切换手势（控制条贴着视频元素
  // 底边，取底边上方 56px 屏蔽带；视频元素以下的黑边不受限）。
  const swipe = useSwipeNav(onPrev, onNext, (x, y) => {
    const v = videoRef.current;
    if (!v) return false;
    const r = v.getBoundingClientRect();
    return y >= r.bottom - 56 && y <= r.bottom && x >= r.left && x <= r.right;
  });

  return (
    <div
      className="fixed inset-0 z-[60] flex touch-none flex-col bg-black"
      onTouchStart={swipe.onTouchStart}
      onTouchEnd={swipe.onTouchEnd}
      onTouchCancel={swipe.onTouchCancel}
      onContextMenu={(e) => e.preventDefault()}
    >
      <div
        className="absolute left-1/2 top-0 z-10 flex -translate-x-1/2 items-center gap-2 px-3 py-1 text-[11px] text-gray-400"
        style={{ marginTop: "calc(env(safe-area-inset-top) + 0.5rem)" }}
      >
        <span className="max-w-[60vw] truncate font-mono" title={display}>
          {display}
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
      <video
        ref={videoRef}
        src={url}
        controls
        autoPlay
        playsInline
        className="max-h-full max-w-full object-contain"
        style={{ marginTop: "auto", marginBottom: "auto" }}
      />
    </div>
  );
}
