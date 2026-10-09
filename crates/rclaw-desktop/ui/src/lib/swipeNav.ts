import { useRef } from "react";

/** 画廊左右滑切换的共用轻扫检测（视频播放器 / 画廊取数遮罩用；图片查看器
 *  因与缩放平移手势耦合而自带实现）：容器 touchstart 记起点，touchend 按
 *  「水平主导 + 过阈值（或快速轻扫）」判定。filterStart 返回 true 的起点
 *  （如视频原生控制条）不参与。边界方向的回调缺省时，该方向滑动无效，
 *  由调用方 UI 弹回——画廊不循环。 */
export function useSwipeNav(
  onPrev: (() => void) | undefined,
  onNext: (() => void) | undefined,
  filterStart?: (x: number, y: number) => boolean,
) {
  const from = useRef<{ x: number; y: number; ts: number } | null>(null);
  return {
    onTouchStart(e: React.TouchEvent) {
      const t = e.touches[0];
      if (
        e.touches.length !== 1 ||
        !t ||
        (filterStart && filterStart(t.clientX, t.clientY))
      ) {
        from.current = null;
        return;
      }
      from.current = { x: t.clientX, y: t.clientY, ts: Date.now() };
    },
    onTouchEnd(e: React.TouchEvent) {
      const s = from.current;
      from.current = null;
      if (!s || (!onPrev && !onNext)) return;
      const t = e.changedTouches[0];
      if (!t) return;
      const dx = t.clientX - s.x;
      const dy = t.clientY - s.y;
      // 主导方向须水平：纵向手势（如调出视频控制条的下滑）不切换。
      if (Math.abs(dx) < Math.abs(dy) * 1.2) return;
      const quick = Date.now() - s.ts < 250 && Math.abs(dx) > 40;
      // 屏宽 20% 为阈值，下限 60px；上限 200px——触屏桌面/大屏平板不至于
      // 要拖半只手才触发。
      const threshold = Math.min(200, Math.max(60, window.innerWidth * 0.2));
      if (dx <= -threshold || (quick && dx < 0)) onNext?.();
      else if (dx >= threshold || (quick && dx > 0)) onPrev?.();
    },
    onTouchCancel() {
      from.current = null;
    },
  };
}
