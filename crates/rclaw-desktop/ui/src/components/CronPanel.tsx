import { useCallback, useEffect, useState } from "react";
import { api, type CronEntry } from "../lib/api";

/** 定时任务面板：列表 + 删除；新增任务直接在对话里让 RClaw 自己加（cron_add 工具）。 */
export default function CronPanel({ agentReady }: { agentReady: boolean }) {
  const [entries, setEntries] = useState<CronEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setEntries(await api.cronList());
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoaded(true);
    }
  }, []);

  useEffect(() => {
    if (agentReady) void refresh();
  }, [agentReady, refresh]);

  const remove = async (id: string) => {
    try {
      await api.cronRemove(id);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="h-full overflow-y-auto p-6 max-md:p-3">
      <div className="mb-4 flex items-center justify-between">
        <h1 className="text-base font-semibold text-gray-100">定时任务</h1>
        <button
          onClick={() => void refresh()}
          className="rounded-lg border border-[#2a2f3a] px-3 py-1 text-xs text-gray-400 transition hover:border-[#3a4150] hover:text-gray-200"
        >
          刷新
        </button>
      </div>

      <p className="mb-4 rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-xs text-gray-500">
        新增任务无需表单：直接在对话里说「每天早上 8 点提醒我…」，RClaw 会自己调用 cron
        工具登记。这里查看与删除已登记的任务。
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
      {agentReady && loaded && entries.length === 0 && (
        <div className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-sm text-gray-500">
          暂无定时任务。
        </div>
      )}
      <div className="space-y-2">
        {entries.map((e) => (
          <div
            key={e.id}
            className="flex items-center justify-between rounded-xl border border-[#2a2f3a] bg-[#141821] px-4 py-3"
          >
            <div className="min-w-0">
              <div className="flex items-center gap-2">
                <span className="truncate text-sm font-medium text-gray-200">
                  {e.name || e.id}
                </span>
                <span
                  className={`rounded-full px-2 py-0.5 text-[10px] ${
                    e.enabled
                      ? "bg-emerald-500/10 text-emerald-400"
                      : "bg-gray-500/10 text-gray-500"
                  }`}
                >
                  {e.enabled ? "启用中" : "已停用"}
                </span>
              </div>
              <div className="mt-0.5 truncate text-xs text-gray-500">
                {e.schedule} · {e.kind}
              </div>
              <div className="mt-1 truncate text-xs text-gray-400">{e.message}</div>
            </div>
            <button
              onClick={() => void remove(e.id)}
              className="ml-3 shrink-0 rounded-lg border border-[#2a2f3a] px-3 py-1 text-xs text-gray-400 transition hover:border-red-500/50 hover:text-red-300"
            >
              删除
            </button>
          </div>
        ))}
      </div>
    </div>
  );
}
