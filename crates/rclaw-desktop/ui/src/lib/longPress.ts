/**
 * 长按守卫（消息行 / 媒体卡共用工厂）：统一 450ms 计时 + 12px touch slop +
 * window capture 触摸监听，调用方只管 start/cancel。
 *
 * 为什么挂 window capture：流式重渲染会替换触摸目标（如 <p>→<h2> 换节点），
 * 行级 onTouchMove 派发到已从文档摘除的旧节点就收不到了——slop 取消失效，
 * 用户明明在滑动、菜单却照样弹出（「正常滑动触发长按」的根因）。capture 自
 * window 下行，只要触点还连着文档就必然经过这里。
 *
 * 为什么是 450ms 不拉长：WebView 在长按 ~500ms 会补发原生 contextmenu（Chat/
 * FilePanel 均有记载）。计时器与原生文字选择（约 400ms 接管）同量级，并抢在
 * contextmenu 之前打开应用菜单（抬指合成 click 由调用方的 click 吞吐兜住）；
 * 「长按太容易误触」的降敏来自修掉漏取消，不是拉长计时。
 *
 * 滚动取消：scroll 不冒泡，不挂 scroll 监听；到期触发前读 `getScrollTop` 与
 * 按下基线比对，漂移 >4px 即取消——慢滚（每次 move 位移 <slop）在触发前也能
 * 拦住。4px 容差放行 anchoring/软键盘重排等布局微动，避免误杀静止长按。
 */

/** 触摸位移超过该值（px）即取消长按：与消息列表手势的 touch slop 同值。 */
const LONG_PRESS_SLOP_PX = 12;
/** 触发前允许的列表滚动漂移（px）：超过即视为用户在滚动，取消长按。 */
const LONG_PRESS_SCROLL_PX = 4;

export interface LongPressGuard {
  /** 开始一次长按计时（已在途的先取消）。x/y 为触发时回传的菜单坐标。 */
  start: (x: number, y: number) => void;
  /** 取消当前计时（无在途时无害）。 */
  cancel: () => void;
}

export function createLongPressGuard(options: {
  /** 计时到期且校验通过后的回调（菜单坐标）。 */
  onFire: (x: number, y: number) => void;
  /** 触发前滚动基线读取（null = 不做滚动校验，如不在可滚动列表内的场景）。 */
  getScrollTop?: () => number | null;
  /** 触发前额外校验（false = 取消）。调用方在此检查目标是否仍在文档等。 */
  isStillValid?: () => boolean;
}): LongPressGuard {
  const { onFire, getScrollTop, isStillValid } = options;
  let timer: number | null = null;
  let origin: { x: number; y: number } | null = null;
  let scrollBase: number | null = null;
  // 在途手势的 window capture 监听卸载器（cancel/fire 时统一执行）。
  let cleanups: (() => void)[] = [];

  const runCleanups = () => {
    for (const fn of cleanups) fn();
    cleanups = [];
  };
  const addWindowListener = (type: string, fn: EventListener) => {
    // 添加/移除 capture 标志必须一致，否则移除失效、监听越挂越多。
    window.addEventListener(type, fn, { capture: true, passive: true });
    cleanups.push(() =>
      window.removeEventListener(type, fn, { capture: true }),
    );
  };

  const cancel = () => {
    if (timer !== null) {
      window.clearTimeout(timer);
      timer = null;
    }
    origin = null;
    scrollBase = null;
    runCleanups();
  };

  const start = (x: number, y: number) => {
    cancel();
    origin = { x, y };
    scrollBase = getScrollTop?.() ?? null;
    timer = window.setTimeout(() => {
      timer = null;
      const o = origin;
      // ① 滚动漂移校验：慢滚（逐次 move 都 <slop）在这里拦住。
      if (scrollBase !== null) {
        const now = getScrollTop?.() ?? null;
        if (now === null || Math.abs(now - scrollBase) > LONG_PRESS_SCROLL_PX) {
          cancel();
          return;
        }
      }
      // ② 调用方校验（目标行仍在文档等）。
      if (isStillValid && !isStillValid()) {
        cancel();
        return;
      }
      origin = null;
      scrollBase = null;
      runCleanups();
      if (o) onFire(o.x, o.y);
    }, 450);
    addWindowListener("touchmove", (e) => {
      const t = (e as TouchEvent).touches[0];
      if (!t || !origin) return;
      if (Math.hypot(t.clientX - origin.x, t.clientY - origin.y) > LONG_PRESS_SLOP_PX) {
        cancel();
      }
    });
    addWindowListener("touchstart", (e) => {
      // 第二根手指落下：非长按意图（双指捏合/多指操作），取消。
      if ((e as TouchEvent).touches.length > 1) cancel();
    });
    addWindowListener("touchend", cancel);
    addWindowListener("touchcancel", cancel);
    // 页面切后台 / 失焦：触摸序列不可信，直接取消（抬起事件可能收不到）。
    addWindowListener("visibilitychange", () => {
      if (document.hidden) cancel();
    });
    addWindowListener("blur", cancel);
  };

  return { start, cancel };
}
