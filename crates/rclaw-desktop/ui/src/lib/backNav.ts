import { useEffect, useRef } from "react";

/** 安卓返回键接管：MainActivity 在 onWebViewCreate 里注册了优先于 wry 内建
 *  回调的 OnBackPressedCallback，把返回键直接翻译成一次 `window.__rclawBack()`
 *  调用——不走 WebView 历史导航（华为 WebView 的原生 goBack() 对 pushState
 *  同文档历史条目会原生崩溃，crashpad 真机复现）。JS 侧只负责装载决策函数：
 *  有关弹层先关最上层，没有时交给 onBack（抽屉开 = 退出应用，否则呼出侧栏）。
 *  EMUI 手势导航的「侧边内滑 = 系统返回」同样经 dispatcher 进来，一并接管。 */

type Handler = () => void;

/** 弹层关闭回调栈（后进先出）。 */
const stack: Handler[] = [];

/** 弹层在打开期间注册返回键关闭回调；active=false 时不注册（幂等，可常驻调用）。
 *  回调闭包每次渲染刷新，注册/退订只随 active 变化。 */
export function useBackClose(active: boolean, close: Handler) {
  const ref = useRef(close);
  ref.current = close;
  useEffect(() => {
    if (!active) return;
    const fn = () => ref.current();
    stack.push(fn);
    return () => {
      const i = stack.indexOf(fn);
      if (i >= 0) stack.splice(i, 1);
    };
  }, [active]);
}

/** 栈顶弹层响应返回键（有则消费），返回是否消费。 */
function closeTopmost(): boolean {
  const fn = stack[stack.length - 1];
  if (!fn) return false;
  fn();
  return true;
}

/** 装载返回键决策函数（仅安卓 APP 主界面调用；未装载时返回键由原生侧走
 *  系统默认退出，向导/启动期即此行为）。 */
export function installBackNavigation(onBack: () => boolean) {
  (window as unknown as Record<string, unknown>).__rclawBack = () => {
    closeTopmost() || onBack();
  };
}
