/**
* Bot Mode 面板（ADR-0006）：bot 花名册 + 编辑器 + 群聊房间。
 *
 * - 花名册：新建 / 编辑 / 删除 bot（persona、固定 provider、工具面收窄），
 *   「打开会话」进入该 bot 的专属聊天（普通会话，chat_id = bot-<name>）。
 * - 群聊：选择 2-6 个 bot 建群；房间视图 = 转录 + 活动流 + 输入框（@mention 芯片）。
 *   一条消息触发后端串行轮转编排（3 轮 / 10 条 / 补轮 2 次上限），增量经 group 事件刷新。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { api, type BotEntry, type GroupRoom } from "../lib/api";
import { useCompactLayout } from "../lib/compactLayout";
import GroupCreateDialog from "./GroupCreateDialog";

const PANEL =
  "flex h-full min-h-0 flex-col overflow-hidden rounded-xl border border-[#2a2f3a] bg-[#161a22]";
const BTN =
  "rounded-lg border border-[#2a2f3a] bg-[#1a1f29] px-3 py-1.5 text-xs text-gray-300 transition hover:border-indigo-500/50 hover:text-indigo-200 disabled:cursor-not-allowed disabled:opacity-40";
const BTN_PRIMARY =
  "rounded-lg bg-indigo-500/80 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-40";
const INPUT =
  "w-full rounded-lg border border-[#2a2f3a] bg-[#141821] px-3 py-1.5 text-xs text-gray-200 outline-none transition focus:border-indigo-500/60";

/** bot 编辑器草稿。 */
interface BotDraft {
  name: string;
  title: string;
  description: string;
  persona: string;
  provider: string;
  denied_tools: string;
}

const EMPTY_DRAFT: BotDraft = {
  name: "",
  title: "",
  description: "",
  persona: "",
  provider: "",
  denied_tools: "",
};

function draftOf(bot: BotEntry): BotDraft {
  return {
    name: bot.name,
    title: bot.title,
    description: bot.description,
    persona: bot.persona,
    provider: bot.provider ?? "",
    denied_tools: bot.denied_tools.join(", "),
  };
}

export default function BotsPanel({
  agentReady,
  rooms,
  onRoomsChanged,
  onDeleteRoom,
  onOpenChat,
  onOpenRoom,
}: {
  agentReady: boolean;
  /** App 持有的群房间快照（单一数据源）。 */
  rooms: GroupRoom[];
  /** 创建/删除群聊后刷新 rooms。 */
  onRoomsChanged: () => void;
  onDeleteRoom: (room: GroupRoom) => void;
  onOpenChat: (chatId: string) => void;
  onOpenRoom: (roomId: string) => void;
}) {
  const compact = useCompactLayout();
  const [bots, setBots] = useState<BotEntry[]>([]);
  const [providers, setProviders] = useState<string[]>([]);
  const [draft, setDraft] = useState<BotDraft | null>(null);
  const [creatingGroup, setCreatingGroup] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setBots(await api.botsList());
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    if (!agentReady) return;
    void refresh();
    void api
      .botProviders()
      .then(setProviders)
      .catch(() => {});
  }, [agentReady, refresh]);

  const saveBot = useCallback(async () => {
    if (!draft) return;
    try {
      await api.botSave({
        name: draft.name.trim(),
        title: draft.title,
        description: draft.description,
        persona: draft.persona,
        provider: draft.provider.trim() === "" ? null : draft.provider.trim(),
        deniedTools: draft.denied_tools
          .split(/[,\s]+/)
          .map((s) => s.trim())
          .filter(Boolean),
      });
      setDraft(null);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  }, [draft, refresh]);

  const deleteBot = useCallback(
    async (name: string) => {
      // 删除会连数据目录（独立记忆）一起清掉，先确认。
      if (!window.confirm(`删除 @${name}？其独立记忆与会话数据将一并清除（会话文件保留）。`)) {
        return;
      }
      try {
        await api.botDelete(name);
        await refresh();
      } catch (e) {
        setError(String(e));
      }
    },
    [refresh],
  );

  if (!agentReady) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-gray-500">
        agent 未就绪
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 gap-3 p-3 max-md:flex-col max-md:overflow-y-auto">
      {creatingGroup && (
        <GroupCreateDialog
          bots={bots}
          onClose={() => setCreatingGroup(false)}
          onCreated={() => {
            setCreatingGroup(false);
            onRoomsChanged();
          }}
        />
      )}
      {/* 左列：花名册 + 群聊列表。移动端进入编辑/建群/房间时整列让位（全屏化）。 */}
      <div
        className={`flex min-h-0 w-72 shrink-0 flex-col gap-3 max-md:w-auto ${
          draft || creatingGroup ? "max-md:hidden" : ""
        }`}
      >
        <div className={`${PANEL} max-md:h-72`}>
          <div className="flex items-center justify-between border-b border-[#2a2f3a] px-3 py-2">
            <span className="text-xs font-medium text-gray-200">🤖 智能体（{bots.length}）</span>
            <button
              className={BTN}
              onClick={() => {
                setDraft({ ...EMPTY_DRAFT });
                setError(null);
              }}
            >
              新建
            </button>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto p-2">
            {bots.length === 0 && (
              <div className="px-2 py-4 text-xs leading-relaxed text-gray-600">
                还没有 bot。新建一个研究员 / 审查员 / 编辑，或组建一个群聊让它们协作。
              </div>
            )}
            {bots.map((b) => (
              <div
                key={b.name}
                className="group rounded-lg px-2 py-2 transition hover:bg-[#1a1f29]"
              >
                <div className="flex items-center justify-between gap-2">
                  <button
                    className="min-w-0 flex-1 text-left"
                    onClick={() => onOpenChat(`bot-${b.name}`)}
                    title={`打开 @${b.name} 的专属会话`}
                  >
                    <div className="truncate text-xs font-medium text-gray-200">
                      🤖 {b.title}
                      <span className="ml-1 text-gray-500">@{b.name}</span>
                    </div>
                    <div className="truncate text-[11px] text-gray-500">
                      {b.description || (b.provider ? `模型：${b.provider}` : "未设置职责")}
                    </div>
                  </button>
                  {/* 触屏无 hover：编辑/删除改为常显（桌面保持 hover 出现）。 */}
                  <div className={`${compact ? "flex" : "hidden group-hover:flex"} shrink-0 gap-1`}>
                    <button
                      className="rounded px-1 text-[10px] text-gray-400 hover:text-indigo-300"
                      onClick={() => setDraft(draftOf(b))}
                      title="编辑"
                    >
                      编辑
                    </button>
                    <button
                      className="rounded px-1 text-[10px] text-gray-400 hover:text-red-400"
                      onClick={() => void deleteBot(b.name)}
                      title="删除 bot 定义与数据目录"
                    >
                      删除
                    </button>
                  </div>
                </div>
              </div>
            ))}
          </div>
        </div>

        <div className={`${PANEL} max-md:h-60`}>
          <div className="flex items-center justify-between border-b border-[#2a2f3a] px-3 py-2">
            <span className="text-xs font-medium text-gray-200">👥 群聊（{rooms.length}）</span>
            <button
              className={BTN}
              disabled={bots.length < 2}
              title={bots.length < 2 ? "至少需要 2 个 bot" : "新建群聊"}
              onClick={() => {
                setCreatingGroup(true);
                setError(null);
              }}
            >
              建群
            </button>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto p-2">
            {rooms.length === 0 && (
              <div className="px-2 py-3 text-xs text-gray-600">
                群聊 = 2-6 个 bot 的有界协作房间：一条消息触发串行轮转（最多 3 轮、10 条发言、
                未回应点名补 2 轮），全员 (pass) 提前收敛。
              </div>
            )}
            {rooms.map((g) => (
              <div
                key={g.id}
                className="group flex items-center justify-between gap-2 rounded-lg px-2 py-2 transition hover:bg-[#1a1f29]"
              >
                <button
                  className="min-w-0 flex-1 text-left"
                  onClick={() => onOpenRoom(g.id)}
                >
                  <div className="truncate text-xs font-medium text-gray-200">
                    {g.running ? "🟢 " : "👥 "}
                    {g.name}
                  </div>
                  <div className="truncate text-[11px] text-gray-500">
                    {g.members.map((m) => `@${m}`).join(" ")}
                  </div>
                </button>
                <button
                  className={`${compact ? "block" : "hidden group-hover:block"} rounded px-1 text-[10px] text-gray-400 hover:text-red-400`}
                  onClick={() => onDeleteRoom(g)}
                  title="删除群聊"
                >
                  删除
                </button>
              </div>
            ))}
          </div>
        </div>
      </div>

      {/* 右列：编辑器 / 建群 / 房间视图。 */}
      <div className="min-h-0 min-w-0 flex-1 max-md:min-h-0">
        {draft ? (
          <div className={`${PANEL} p-4 max-md:overflow-y-auto`}>
            <div className="mb-3 flex items-center justify-between">
              <span className="text-sm font-medium text-gray-100">
                {bots.some((b) => b.name === draft.name) ? `编辑 @${draft.name}` : "新建智能体"}
              </span>
              <button className="text-xs text-gray-500 hover:text-gray-300" onClick={() => setDraft(null)}>
                关闭
              </button>
            </div>
            <div className="grid grid-cols-2 gap-3 max-md:grid-cols-1">
              <label className="flex flex-col gap-1 text-[11px] text-gray-400">
                名字（唯一标识，创建后不可改）
                <input
                  className={INPUT}
                  value={draft.name}
                  disabled={bots.some((b) => b.name === draft.name)}
                  onChange={(e) => setDraft({ ...draft, name: e.target.value })}
                  placeholder="researcher（小写字母/数字/-/_，≤32 字符）"
                />
              </label>
              <label className="flex flex-col gap-1 text-[11px] text-gray-400">
                显示名
                <input
                  className={INPUT}
                  value={draft.title}
                  onChange={(e) => setDraft({ ...draft, title: e.target.value })}
                  placeholder="研究员"
                  maxLength={32}
                />
              </label>
              <label className="col-span-2 max-md:col-span-1 flex flex-col gap-1 text-[11px] text-gray-400">
                职责一句话（进花名册，给队友点名用）
                <input
                  className={INPUT}
                  value={draft.description}
                  onChange={(e) => setDraft({ ...draft, description: e.target.value })}
                  placeholder="负责公开信息检索与取证，输出带来源的结论"
                  maxLength={120}
                />
              </label>
              <label className="col-span-2 max-md:col-span-1 flex flex-1 flex-col gap-1 text-[11px] text-gray-400">
                角色设定（persona，作为该 bot 的系统提示人格段）
                <textarea
                  className={`${INPUT} min-h-40 flex-1 font-mono leading-relaxed`}
                  value={draft.persona}
                  onChange={(e) => setDraft({ ...draft, persona: e.target.value })}
                  placeholder={"你是一名严谨的行业研究员……\n（该 bot 拥有独立的记忆与会话；技能与文件沙箱与主控共享）"}
                />
              </label>
              <label className="flex flex-col gap-1 text-[11px] text-gray-400">
                固定 provider（可选）
                <select
                  className={INPUT}
                  value={draft.provider}
                  onChange={(e) => setDraft({ ...draft, provider: e.target.value })}
                >
                  <option value="">跟随主控</option>
                  {providers.map((p) => (
                    <option key={p} value={p}>
                      {p}
                    </option>
                  ))}
                </select>
              </label>
              <label className="flex flex-col gap-1 text-[11px] text-gray-400">
                禁用工具（逗号分隔，可选）
                <input
                  className={INPUT}
                  value={draft.denied_tools}
                  onChange={(e) => setDraft({ ...draft, denied_tools: e.target.value })}
                  placeholder="cron_add, shell"
                />
              </label>
            </div>
            {error && <div className="mt-2 text-xs text-red-400">{error}</div>}
            <div className="mt-3 flex justify-end gap-2">
              <button className={BTN} onClick={() => setDraft(null)}>
                取消
              </button>
              <button
                className={BTN_PRIMARY}
                disabled={
                  draft.name.trim() === "" ||
                  draft.title.trim() === "" ||
                  draft.persona.trim() === ""
                }
                onClick={() => void saveBot()}
              >
                保存（下一 turn 生效）
              </button>
            </div>
          </div>
        ) : (
          <div className="flex h-full items-center justify-center rounded-xl border border-dashed border-[#2a2f3a] text-xs leading-relaxed text-gray-600 max-md:hidden">
            <div className="max-w-md px-6 text-center">
              <div className="mb-2 text-sm text-gray-400">Bot Mode</div>
              <p>
                每个 bot 是一个隔离的智能体：独立人格、独立记忆与会话，可固定不同模型。
                在 bot 会话里用 <code className="text-indigo-300">@名字</code> 让它把任务交接给队友；
                或在群聊里发一条任务，看它们轮转协作。
              </p>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
