import { useCallback, useEffect, useState } from "react";
import { api, type SkillEntry } from "../lib/api";

/** 技能面板：列出全部技能并可逐个禁能/启用。
 *  禁能 = 归档到 skills/.archive/（退出技能目录扫描与系统提示）；
 *  启用 = 从 .archive/ 移回。与 skill_manage 工具的 archive/restore 同一机制。 */
export default function SkillsPanel({ agentReady }: { agentReady: boolean }) {
  const [skills, setSkills] = useState<SkillEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState<string | null>(null);
  const [query, setQuery] = useState("");

  const refresh = useCallback(async () => {
    try {
      const payload = await api.skillsList();
      setSkills(payload.skills ?? []);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    if (agentReady) void refresh();
  }, [agentReady, refresh]);

  const toggle = async (skill: SkillEntry) => {
    setPending(skill.name);
    try {
      await api.skillSetEnabled(skill.name, !isEnabled(skill));
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setPending(null);
    }
  };

  const isEnabled = (s: SkillEntry) => s.state !== "archived" && s.enabled !== false;

  const filtered = skills.filter(
    (s) =>
      s.name.toLowerCase().includes(query.toLowerCase()) ||
      (s.description ?? "").toLowerCase().includes(query.toLowerCase()),
  );

  const disabledCount = skills.filter((s) => !isEnabled(s)).length;

  return (
    <div className="h-full overflow-y-auto p-6 max-md:p-3">
      <div className="mb-4 flex items-center justify-between">
        <h1 className="text-base font-semibold text-gray-100">
          技能 <span className="text-xs text-gray-500">共 {skills.length} 项 · 禁用 {disabledCount}</span>
        </h1>
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="搜索技能…"
          className="w-52 rounded-lg border border-[#2a2f3a] bg-[#161a22] px-3 py-1.5 text-xs text-gray-200 outline-none focus:border-indigo-500"
        />
      </div>

      <p className="mb-4 rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-xs text-gray-500">
        禁用的技能会归档到 <code>skills/.archive/</code>——退出系统提示与技能清单，模型不再使用；
        启用即移回。自进化（curator / skill_manage）维护的同一套归档机制，归档技能在关于页所列的
        数据目录中始终可见。
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
      {agentReady && skills.length === 0 && (
        <div className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-sm text-gray-500">
          技能库为空。在对话里让 RClaw「把这类工作沉淀成技能」，自进化会自动创建。
        </div>
      )}

      <div className="space-y-2">
        {filtered.map((s) => {
          const enabled = isEnabled(s);
          return (
            <div
              key={s.name}
              className="flex items-center justify-between gap-3 rounded-xl border border-[#2a2f3a] bg-[#141821] px-4 py-3"
            >
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <span className="font-mono text-sm text-gray-200">{s.name}</span>
                  {!enabled && (
                    <span className="rounded-full bg-gray-500/10 px-2 py-0.5 text-[10px] text-gray-500">
                      已禁用
                    </span>
                  )}
                  {s.pinned && (
                    <span className="rounded-full bg-amber-500/10 px-2 py-0.5 text-[10px] text-amber-400">
                      pinned
                    </span>
                  )}
                </div>
                {s.description && (
                  <div className="mt-0.5 line-clamp-2 text-xs text-gray-500">{s.description}</div>
                )}
              </div>
              <button
                onClick={() => void toggle(s)}
                disabled={pending === s.name}
                title={enabled ? "禁用此技能" : "启用此技能"}
                className={`relative h-6 w-11 shrink-0 rounded-full transition ${
                  enabled ? "bg-emerald-500/80" : "bg-gray-600"
                } ${pending === s.name ? "opacity-50" : ""}`}
              >
                <span
                  className={`absolute top-0.5 h-5 w-5 rounded-full bg-white transition-all ${
                    enabled ? "left-[22px]" : "left-0.5"
                  }`}
                />
              </button>
            </div>
          );
        })}
      </div>
    </div>
  );
}
