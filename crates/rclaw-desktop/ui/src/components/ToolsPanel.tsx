import { useCallback, useEffect, useState } from "react";
import { api, type ToolEntry } from "../lib/api";

/** 工具面板：列出全部工具并可逐个使能/禁能。
 *  禁用立即拦截执行，并从下一个 turn 起移出模型的工具面；偏好跨重启保留。 */
export default function ToolsPanel({ agentReady }: { agentReady: boolean }) {
  const [tools, setTools] = useState<ToolEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState<string | null>(null);
  const [query, setQuery] = useState("");

  const refresh = useCallback(async () => {
    try {
      setTools(await api.toolsList());
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    if (agentReady) void refresh();
  }, [agentReady, refresh]);

  const toggle = async (tool: ToolEntry) => {
    setPending(tool.name);
    try {
      await api.setToolEnabled(tool.name, !tool.enabled);
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setPending(null);
    }
  };

  const filtered = tools.filter(
    (t) =>
      t.name.toLowerCase().includes(query.toLowerCase()) ||
      t.description.toLowerCase().includes(query.toLowerCase()),
  );

  const disabledCount = tools.filter((t) => !t.enabled).length;

  return (
    <div className="h-full overflow-y-auto p-6 max-md:p-3">
      <div className="mb-4 flex items-center justify-between">
        <h1 className="text-base font-semibold text-gray-100">
          工具 <span className="text-xs text-gray-500">共 {tools.length} 项 · 禁用 {disabledCount}</span>
        </h1>
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="搜索工具…"
          className="w-52 rounded-lg border border-[#2a2f3a] bg-[#161a22] px-3 py-1.5 text-xs text-gray-200 outline-none focus:border-indigo-500"
        />
      </div>

      <p className="mb-4 rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-xs text-gray-500">
        禁用立即生效：该工具从下一个回合起不再出现在模型的工具面，调用也会被拒绝。
        偏好会持久化（跨重启保留）。高危工具（如 shell、write、delete）可在此关停。
      </p>

      {error && (
        <div className="mb-4 rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-sm text-red-300">
          {error}
        </div>
      )}
      {!agentReady && (
        <div className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-sm text-gray-500">
          agent 启动后可用。
        </div>
      )}

      <div className="space-y-2">
        {filtered.map((t) => (
          <div
            key={t.name}
            className="flex items-center justify-between gap-3 rounded-xl border border-[#2a2f3a] bg-[#141821] px-4 py-3"
          >
            <div className="min-w-0">
              <div className="flex items-center gap-2">
                <span className="font-mono text-sm text-gray-200">{t.name}</span>
                {!t.enabled && (
                  <span className="rounded-full bg-gray-500/10 px-2 py-0.5 text-[10px] text-gray-500">
                    已禁用
                  </span>
                )}
              </div>
              <div className="mt-0.5 line-clamp-2 text-xs text-gray-500">{t.description}</div>
            </div>
            <button
              onClick={() => void toggle(t)}
              disabled={pending === t.name}
              title={t.enabled ? "禁用此工具" : "启用此工具"}
              className={`relative h-6 w-11 shrink-0 rounded-full transition ${
                t.enabled ? "bg-emerald-500/80" : "bg-gray-600"
              } ${pending === t.name ? "opacity-50" : ""}`}
            >
              <span
                className={`absolute top-0.5 h-5 w-5 rounded-full bg-white transition-all ${
                  t.enabled ? "left-[22px]" : "left-0.5"
                }`}
              />
            </button>
          </div>
        ))}
      </div>
    </div>
  );
}
