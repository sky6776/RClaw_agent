import { useCallback, useEffect, useState } from "react";
import { api, type EvolutionInfo, type Insights } from "../lib/api";

function Flag({ on, label }: { on: boolean; label: string }) {
  return (
    <div className="flex items-center gap-2 rounded-lg border border-[#2a2f3a] bg-[#141821] px-3 py-2">
      <span className={`h-2 w-2 rounded-full ${on ? "bg-emerald-500" : "bg-gray-600"}`} />
      <span className={`text-xs ${on ? "text-gray-300" : "text-gray-500"}`}>{label}</span>
      <span className={`ml-auto text-[10px] ${on ? "text-emerald-400" : "text-gray-600"}`}>
        {on ? "开启" : "关闭"}
      </span>
    </div>
  );
}

/** 水平条目条：label + 按占比渲染的条形 + 数值。 */
function BarRow({
  label,
  count,
  max,
  suffix,
}: {
  label: string;
  count: number;
  max: number;
  suffix?: string;
}) {
  const pct = max > 0 ? Math.max(4, Math.round((count / max) * 100)) : 4;
  return (
    <div className="flex items-center gap-3">
      <span className="w-28 shrink-0 truncate font-mono text-xs text-gray-300" title={label}>
        {label}
      </span>
      <div className="h-2 min-w-0 flex-1 overflow-hidden rounded-full bg-[#1a2030]">
        <div className="h-full rounded-full bg-indigo-500/70" style={{ width: `${pct}%` }} />
      </div>
      <span className="w-16 shrink-0 text-right text-[10px] text-gray-500">
        {count}
        {suffix ?? ""}
      </span>
    </div>
  );
}

/** 自进化信息面板：开关状态 + 学习洞察 + 长期记忆正文 + 技能库位置。 */
export default function EvolutionPanel({ agentReady }: { agentReady: boolean }) {
  const [info, setInfo] = useState<EvolutionInfo | null>(null);
  const [insights, setInsights] = useState<Insights | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setInfo(await api.evolutionInfo());
      setInsights(await api.insights());
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    if (agentReady) void refresh();
  }, [agentReady, refresh]);

  const maxDay = insights
    ? Math.max(1, ...insights.days.map((d) => d.user_msgs + d.assistant_msgs))
    : 1;
  const maxTool = insights?.top_tools[0]?.count ?? 1;

  return (
    <div className="h-full overflow-y-auto p-6 max-md:p-3">
      <div className="mb-4 flex items-center justify-between">
        <h1 className="text-base font-semibold text-gray-100">自进化</h1>
        <button
          onClick={() => void refresh()}
          className="rounded-lg border border-[#2a2f3a] px-3 py-1 text-xs text-gray-400 transition hover:border-[#3a4150] hover:text-gray-200"
        >
          刷新
        </button>
      </div>

      <p className="mb-4 rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-xs text-gray-500">
        RClaw 在每次对话后自动复盘：把偏好与事实沉淀进长期记忆（MEMORY.md / USER.md），
        把重复工作固化成技能，并由 curator 定期整理技能库。这些产物都是数据目录里的纯文本，
        可直接查看或手工修正。
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

      {info && (
        <div className="space-y-4">
          <div className="grid grid-cols-2 gap-2">
            <Flag on={info.auto_memory} label="自动记忆（每轮沉淀 MEMORY/USER）" />
            <Flag on={info.auto_evolve} label="自进化 review（沉淀技能改动）" />
            <Flag on={info.skill_curator} label="技能 curator（定期整理技能库）" />
            <Flag on={info.session_compression} label="会话自动压缩（上下文超限时摘要化）" />
          </div>

          {insights && (
            <div className="rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
              <h2 className="mb-1 text-sm font-medium text-gray-200">学习洞察</h2>
              <p className="mb-3 text-[10px] text-gray-600">
                会话 {insights.total_chats}（归档 {insights.archived_chats}）· 消息{" "}
                {insights.total_messages} · 记忆占用{" "}
                {Math.round((insights.memory_used / Math.max(1, insights.memory_limit)) * 100)}
                %（{insights.memory_used}/{insights.memory_limit}）
              </p>

              {/* 近 14 天活跃：竖条图 */}
              <div className="mb-4">
                <div className="mb-1 text-xs text-gray-400">近 14 天活跃</div>
                <div className="flex h-20 items-end gap-1">
                  {insights.days.map((d) => {
                    const total = d.user_msgs + d.assistant_msgs;
                    const h = Math.max(2, Math.round((total / maxDay) * 100));
                    return (
                      <div
                        key={d.date}
                        className="group relative flex h-full min-w-0 flex-1 flex-col justify-end"
                        title={`${d.date}：user ${d.user_msgs} · assistant ${d.assistant_msgs}`}
                      >
                        <div
                          className={`w-full rounded-t ${total > 0 ? "bg-indigo-500/70" : "bg-[#1a2030]"}`}
                          style={{ height: `${h}%` }}
                        />
                      </div>
                    );
                  })}
                </div>
                <div className="mt-1 flex gap-1 text-center text-[9px] text-gray-600">
                  {insights.days.map((d) => (
                    <span key={d.date} className="min-w-0 flex-1">
                      {d.date.slice(8)}
                    </span>
                  ))}
                </div>
              </div>

              {/* 工具调用 Top */}
              {insights.top_tools.length > 0 && (
                <div className="mb-4">
                  <div className="mb-2 text-xs text-gray-400">工具调用 Top {insights.top_tools.length}</div>
                  <div className="space-y-1.5">
                    {insights.top_tools.map((t) => (
                      <BarRow key={t.name} label={t.name} count={t.count} max={maxTool} />
                    ))}
                  </div>
                </div>
              )}

              {/* 技能使用 Top */}
              {insights.top_skills.length > 0 && (
                <div className="mb-4">
                  <div className="mb-2 text-xs text-gray-400">技能使用 Top {insights.top_skills.length}</div>
                  <div className="space-y-1.5">
                    {insights.top_skills.map((s) => (
                      <BarRow
                        key={s.name}
                        label={s.name}
                        count={s.use_count}
                        max={Math.max(1, ...insights.top_skills.map((x) => x.use_count))}
                        suffix={` · 改 ${s.patch_count}`}
                      />
                    ))}
                  </div>
                </div>
              )}

              {/* 最近技能变更（M2 台账） */}
              {insights.recent_mutations.length > 0 && (
                <div>
                  <div className="mb-2 text-xs text-gray-400">最近技能变更</div>
                  <div className="space-y-1 font-mono text-[10px] text-gray-500">
                    {insights.recent_mutations.map((m) => (
                      <div key={m.id} className="flex items-center gap-2">
                        <span className="text-gray-600">{m.ts}</span>
                        <span
                          className={
                            m.action === "rollback"
                              ? "text-amber-400"
                              : m.actor === "background_review"
                                ? "text-emerald-400"
                                : "text-sky-400"
                          }
                        >
                          {m.action}
                        </span>
                        <span className="truncate">{m.skill || "—"}</span>
                        {m.undo_of && <span className="text-gray-600">↩ {m.undo_of}</span>}
                      </div>
                    ))}
                  </div>
                </div>
              )}
            </div>
          )}

          {info.memory_md !== null && (
            <div className="rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
              <h2 className="mb-2 text-sm font-medium text-gray-200">MEMORY.md（长期记忆）</h2>
              <pre className="max-h-72 overflow-y-auto whitespace-pre-wrap text-xs leading-relaxed text-gray-400">
                {info.memory_md || "（空）"}
              </pre>
            </div>
          )}
          {info.user_md !== null && (
            <div className="rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
              <h2 className="mb-2 text-sm font-medium text-gray-200">USER.md（用户画像）</h2>
              <pre className="max-h-72 overflow-y-auto whitespace-pre-wrap text-xs leading-relaxed text-gray-400">
                {info.user_md || "（空）"}
              </pre>
            </div>
          )}
          <div className="rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
            <h2 className="mb-1 text-sm font-medium text-gray-200">技能库</h2>
            <p className="font-mono text-xs break-all text-gray-500">{info.skills_dir}</p>
            <p className="mt-1 text-xs text-gray-600">
              技能清单与禁能开关见「技能」页；归档技能位于其下 .archive/。
            </p>
          </div>
        </div>
      )}
    </div>
  );
}
