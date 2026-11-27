/** 设置 → 通用与外观：主题与会话文字大小（从原主题弹层迁移）。 */
import type { ThemeId } from "../lib/themeTypes";

export interface ThemeChoice {
  id: ThemeId;
  name: string;
  base: "dark" | "light";
  desc: string;
  swatch: [string, string];
}

export default function AppearanceSection({
  themes,
  theme,
  onTheme,
  chatFont,
  onFont,
  fontMin,
  fontMax,
  fontDefault,
}: {
  themes: ThemeChoice[];
  theme: ThemeId;
  onTheme: (id: ThemeId) => void;
  chatFont: number;
  onFont: (value: number) => void;
  fontMin: number;
  fontMax: number;
  fontDefault: number;
}) {
  return (
    <div className="mx-auto w-full max-w-2xl space-y-5 p-4">
      <section>
        <h3 className="mb-2 text-sm font-semibold text-gray-100">主题</h3>
        <div className="grid grid-cols-[repeat(auto-fill,minmax(8.5rem,1fr))] gap-2">
          {themes.map((t) => (
            <button
              key={t.id}
              onClick={() => onTheme(t.id)}
              className={`rounded-lg border p-2 text-left transition ${
                theme === t.id
                  ? "border-indigo-400 bg-indigo-500/10"
                  : "border-[#2a2f3a] hover:border-indigo-500/50"
              }`}
            >
              <div className="flex h-10 overflow-hidden rounded-md border border-black/20">
                <div className="h-full w-1/3" style={{ backgroundColor: t.swatch[1] }} />
                <div className="h-full flex-1" style={{ backgroundColor: t.swatch[0] }} />
              </div>
              <span className="mt-1.5 block text-xs font-medium text-gray-200">
                {theme === t.id ? "✓ " : ""}
                {t.name}
              </span>
              <span className="block text-[10px] text-gray-500">{t.desc}</span>
            </button>
          ))}
        </div>
      </section>
      <section className="border-t border-[#2a2f3a] pt-4">
        <h3 className="mb-1.5 text-sm font-semibold text-gray-100">会话文字大小</h3>
        <div className="flex items-center gap-1.5">
          <button
            onClick={() => onFont(chatFont - 5)}
            disabled={chatFont <= fontMin}
            className="rounded-lg border border-[#2a2f3a] px-2.5 py-1 text-xs text-gray-300 transition hover:border-indigo-500/50 hover:text-indigo-300 disabled:opacity-40"
            aria-label="缩小文字"
          >
            A−
          </button>
          <span className="min-w-0 flex-1 text-center text-xs tabular-nums text-gray-300">
            {chatFont}%
          </span>
          <button
            onClick={() => onFont(chatFont + 5)}
            disabled={chatFont >= fontMax}
            className="rounded-lg border border-[#2a2f3a] px-2.5 py-1 text-xs text-gray-300 transition hover:border-indigo-500/50 hover:text-indigo-300 disabled:opacity-40"
            aria-label="放大文字"
          >
            A+
          </button>
          <button
            onClick={() => onFont(fontDefault)}
            className="rounded-lg border border-[#2a2f3a] px-2 py-1 text-[11px] text-gray-400 transition hover:border-indigo-500/50 hover:text-indigo-300"
          >
            重置
          </button>
        </div>
        <p className="mt-1.5 text-[10px] leading-relaxed text-gray-500">
          桌面上还可以 Ctrl + 鼠标滚轮；手机在会话里双指捏合。只影响消息文字，不动图片/视频。
        </p>
      </section>
    </div>
  );
}
