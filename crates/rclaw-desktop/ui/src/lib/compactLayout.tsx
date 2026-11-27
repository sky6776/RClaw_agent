import { createContext, useContext, type ReactNode } from "react";

import { isAndroidDevice, useIsMobile } from "./useIsMobile";

/** 紧凑布局（侧栏抽屉 / 顶部栏 / 触屏布局）。
 *
 * - 桌面窄窗口（<768）与 Android 设备都走紧凑布局；
 * - Android 无论横屏/平板宽度都保持紧凑（避免出现 Windows 窗控 TitleBar 与触屏
 *   控件被 `max-md` 隐藏）；
 * - 通过 Context 下发，Chat/FilePanel/MediaPreviews/ModelThinkingPicker 等子组件
 *   不再各自调用 useIsMobile（否则 Android 宽屏会出现外壳紧凑、内部桌面布局）。
 */
const CompactLayoutContext = createContext<boolean | null>(null);

export function CompactLayoutProvider({
  value,
  children,
}: {
  value: boolean;
  children: ReactNode;
}) {
  return (
    <CompactLayoutContext.Provider value={value}>{children}</CompactLayoutContext.Provider>
  );
}

/** 读取紧凑布局值；无 Provider 时回落到当前视口/平台判断（防御性兜底）。 */
export function useCompactLayout(): boolean {
  const value = useContext(CompactLayoutContext);
  const isMobile = useIsMobile();
  if (value !== null) return value;
  return isMobile || isAndroidDevice();
}
