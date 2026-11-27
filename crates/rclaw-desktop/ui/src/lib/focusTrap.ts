import { useEffect, useRef } from "react";

/** 全屏前景（设置/活动）的键盘焦点限制：打开时把焦点移进覆盖层，Tab 在层内循环；
 *  视图切换（分区/标签/详情）后重新落点；卸载时还给触发元素。
 *  背景内容由 App 的 inert/几何遮挡隔离；授权弹窗等更高层对话框在根外，不抢它的 Tab。 */
export function useFocusTrap<T extends HTMLElement>(activeKey?: unknown) {
  const ref = useRef<T | null>(null);
  /** 上一次看到的根节点：用于区分「打开/关闭」与「同一根内切视图」。 */
  const lastRoot = useRef<HTMLElement | null>(null);
  const previousFocus = useRef<HTMLElement | null>(null);

  useEffect(() => {
    const root = ref.current;
    if (root && lastRoot.current !== root) {
      // 覆盖层刚打开：记住打开前的焦点，关闭时还回去。
      previousFocus.current = document.activeElement as HTMLElement | null;
      lastRoot.current = root;
    } else if (!root && lastRoot.current) {
      // 覆盖层关闭（组件卸载把 ref 置空）：把焦点还给打开前的元素。
      lastRoot.current = null;
      previousFocus.current?.focus?.();
      previousFocus.current = null;
      return;
    }
    if (!root) return;

    const visible = (el: HTMLElement) =>
      el.getClientRects().length > 0 &&
      getComputedStyle(el).visibility !== "hidden" &&
      el.closest('[aria-hidden="true"]') === null;
    const focusables = () =>
      Array.from(
        root.querySelectorAll<HTMLElement>(
          'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
        ),
      ).filter(visible);
    const place = () => {
      const active = document.activeElement as HTMLElement | null;
      if (!active || active === document.body || !root.contains(active) || !visible(active)) {
        (focusables()[0] ?? root).focus();
      }
    };

    place();
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Tab") return;
      const active = document.activeElement as HTMLElement | null;
      // 焦点在根外的另一个对话框（如授权弹窗）时不要抢它的 Tab。
      if (active && active !== document.body && !root.contains(active)) return;
      const list = focusables();
      if (list.length === 0) {
        event.preventDefault();
        root.focus();
        return;
      }
      const first = list[0];
      const last = list[list.length - 1];
      if (!root.contains(active)) {
        event.preventDefault();
        (event.shiftKey ? last : first).focus();
      } else if (event.shiftKey && active === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && active === last) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("keydown", onKey);
      // 组件真正卸载（ref 已被置空）时才回焦；同一根内切视图不打断焦点。
      if (ref.current !== root) {
        lastRoot.current = null;
        const prev = previousFocus.current;
        previousFocus.current = null;
        const fallback = () => {
          // 原触发控件已卸载/不可聚焦：落到当前可见层（设置/活动/建群/采集）的首个
          // 可操作项，避免焦点停到 body。
          const dialogs = Array.from(
            document.querySelectorAll<HTMLElement>('[role="dialog"]'),
          ).filter((el) => el.getClientRects().length > 0);
          const target = dialogs[dialogs.length - 1];
          const first = target?.querySelector<HTMLElement>(
            'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
          );
          (first ?? target)?.focus();
        };
        if (prev?.isConnected) {
          prev.focus();
          if (document.activeElement !== prev) fallback();
        } else {
          fallback();
        }
      }
    };
  }, [activeKey]);

  return ref;
}
