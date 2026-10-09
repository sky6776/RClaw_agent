import { useEffect, useRef } from "react";

/** 安卓边缘滑动手势（仅安卓 APP 启用，桌面不受影响）：
 * - 抽屉关：从屏幕左缘（≤28px）向右滑 ≥80px 且横向占优 → onOpen（呼出侧栏）；
 * - 抽屉开：起点在抽屉范围内向左滑 ≥70px 且横向占优 → onClose（收起侧栏）。
 *
 * 与内部横向滚动区（代码块、表格、横向轮播等）的关系：起点祖先存在可横向
 * 滚动且确有溢出的元素时**不直接放弃**——只在其 scrollLeft 于手势期间被改变
 * （用户确实在横向平移内容）时才让位；竖向占优的滑动同样不触发。只判「存在
 * 溢出」会误杀：消息列表因长路径/URL 溢出时 overflowX 计算值也是 auto，
 * 呼出手势会被整条判死（侧栏呼不出的根因）。监听全程 passive，不拦截默认行为。 */
export function useEdgeSwipe(options: {
  open: boolean;
  onOpen: () => void;
  onClose: () => void;
  enabled: boolean;
}) {
  const ref = useRef(options);
  ref.current = options;
  const enabled = options.enabled;
  useEffect(() => {
    if (!enabled) return;
    const EDGE_PX = 28;
    const OPEN_DX = 80;
    const CLOSE_DX = -70;
    let sx = 0;
    let sy = 0;
    let tracking = false;
    let horiz = false;
    // 起点祖先里第一个横向可滚且确有溢出的元素 + 起始 scrollLeft：
    // 手势期间 scrollLeft 变了 = 内容在消费这次触摸，手势让位。
    let hScroller: Element | null = null;
    let hScrollLeft = 0;
    const findHScroller = (target: EventTarget | null) => {
      hScroller = null;
      let node = target instanceof Element ? target : null;
      while (node) {
        const style = window.getComputedStyle(node);
        if (
          (style.overflowX === "auto" || style.overflowX === "scroll") &&
          node.scrollWidth > node.clientWidth + 1
        ) {
          hScroller = node;
          hScrollLeft = node.scrollLeft;
          return;
        }
        node = node.parentElement;
      }
    };
    const drawerWidth = () => Math.min(17 * 16, window.innerWidth * 0.85);
    const onStart = (e: TouchEvent) => {
      tracking = false;
      if (e.touches.length !== 1) return;
      const t = e.touches[0];
      if (ref.current.open) {
        // 抽屉开：起点在抽屉范围（含遮罩边缘附近）即跟踪，用于左滑收起。
        tracking = t.clientX <= drawerWidth() + 16;
      } else {
        tracking = t.clientX <= EDGE_PX;
      }
      if (tracking) findHScroller(e.target);
      sx = t.clientX;
      sy = t.clientY;
      horiz = false;
    };
    const onMove = (e: TouchEvent) => {
      if (!tracking || e.touches.length !== 1) return;
      // 内部横向滚动被内容消费（代码块平移等）：立即让位。
      if (hScroller && hScroller.scrollLeft !== hScrollLeft) {
        tracking = false;
        return;
      }
      const t = e.touches[0];
      const dx = t.clientX - sx;
      const dy = t.clientY - sy;
      if (!horiz) {
        if (Math.abs(dx) > 10 || Math.abs(dy) > 10) {
          horiz = Math.abs(dx) > Math.abs(dy) * 1.4;
        }
        // 竖向占优（列表滚动）即解除跟踪，不再参与判定。
        if (!horiz && Math.abs(dy) > 14) tracking = false;
      }
    };
    const onEnd = (e: TouchEvent) => {
      if (!tracking) return;
      tracking = false;
      const t = e.changedTouches[0];
      if (!horiz) return;
      const dx = t.clientX - sx;
      const state = ref.current;
      if (!state.open && dx >= OPEN_DX) state.onOpen();
      else if (state.open && dx <= CLOSE_DX) state.onClose();
    };
    const onCancel = () => {
      tracking = false;
    };
    window.addEventListener("touchstart", onStart, { passive: true });
    window.addEventListener("touchmove", onMove, { passive: true });
    window.addEventListener("touchend", onEnd, { passive: true });
    window.addEventListener("touchcancel", onCancel, { passive: true });
    return () => {
      window.removeEventListener("touchstart", onStart);
      window.removeEventListener("touchmove", onMove);
      window.removeEventListener("touchend", onEnd);
      window.removeEventListener("touchcancel", onCancel);
    };
  }, [enabled]);
}
