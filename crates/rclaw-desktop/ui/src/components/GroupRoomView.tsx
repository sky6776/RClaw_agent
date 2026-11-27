/** 群聊房间视图：转录 + 活动流 + 输入框（@mention 芯片插入）。
 *
 * 从 BotsPanel 抽出：设置里的「智能体」管理与 App 级群聊前景共用同一个房间视图。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { api, type BotEntry, type GroupRoom, type LlmState } from "../lib/api";
import ModelThinkingPicker from "./ModelThinkingPicker";

const PANEL =
  "flex h-full min-h-0 flex-col overflow-hidden rounded-xl border border-[#2a2f3a] bg-[#161a22]";
const BTN =
  "rounded-lg border border-[#2a2f3a] bg-[#1a1f29] px-3 py-1.5 text-xs text-gray-300 transition hover:border-indigo-500/50 hover:text-indigo-200 disabled:cursor-not-allowed disabled:opacity-40";
const BTN_PRIMARY =
  "rounded-lg bg-indigo-500/80 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-40";
const INPUT =
  "w-full rounded-lg border border-[#2a2f3a] bg-[#141821] px-3 py-1.5 text-xs text-gray-200 outline-none transition focus:border-indigo-500/60";

const ACTIVITY_LABEL: Record<string, string> = {
  queued: "已受理",
  working: "思考中",
  passed: "弃权 (pass)",
  settled: "已收敛",
  capped: "达到上限",
  failed: "失败",
  timeout: "超时",
  cancelled: "已取消",
};

/** 群聊房间视图：转录 + 活动流 + 输入框（@mention 芯片插入）。 */
export default function GroupRoomView({
  id,
  bots,
  onError,
  onClose,
}: {
  id: string;
  bots: BotEntry[];
  onError: (msg: string | null) => void;
  onClose: () => void;
}) {
  const [room, setRoom] = useState<GroupRoom | null>(null);
  const [failed, setFailed] = useState(false);
  const [text, setText] = useState("");
  const transcriptRef = useRef<HTMLDivElement>(null);
  // 群房间的模型选择：每个成员的实际轮次是独立的 grp-<gid>-<member> 会话，
  // 这里按成员分别读写同一个会话级选择器。
  const [modelMember, setModelMember] = useState("");
  const [llm, setLlm] = useState<{ chatId: string; state: LlmState } | null>(null);
  const llmSeq = useRef(0);
  const llmRevision = useRef(0);
  const memberChatId =
    room && room.id === id && room.members.includes(modelMember)
      ? `grp-${id}-${modelMember}`
      : "";
  const activeMemberRef = useRef(memberChatId);
  activeMemberRef.current = memberChatId;
  const memberGen = useRef(0);

  const refresh = useCallback(async () => {
    try {
      setRoom(await api.groupHistory(id));
      setFailed(false);
    } catch (e) {
      // 首次加载失败必须可感知：room 保持 null 时界面只会停在「加载中」，
      // 记录失败态给出重试入口（运行期刷新失败仍走父级 toast）。
      setFailed((prev) => prev || room === null);
      onError(String(e));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id, onError]);

  useEffect(() => {
    void refresh();
    const un = api.onGroup((payload) => {
      if (payload.id === id) void refresh();
    });
    return () => {
      void un.then((fn) => fn());
    };
  }, [id, refresh]);

  useEffect(() => {
    if (!room) return;
    if (!room.members.includes(modelMember)) {
      setModelMember(room.members[0] ?? "");
    }
  }, [room, modelMember]);

  const applyLlm = useCallback((chatId: string, next: LlmState) => {
    if (chatId !== activeMemberRef.current) return;
    if (next.revision < llmRevision.current) return;
    llmRevision.current = next.revision;
    setLlm({ chatId, state: next });
  }, []);
  const refreshLlm = useCallback(() => {
    if (!memberChatId || activeMemberRef.current !== memberChatId) {
      setLlm(null);
      return;
    }
    const gen = memberGen.current;
    const seq = ++llmSeq.current;
    api
      .chatLlmState(memberChatId)
      .then((next) => {
        if (
          gen === memberGen.current &&
          seq === llmSeq.current &&
          activeMemberRef.current === memberChatId
        ) {
          applyLlm(memberChatId, next);
        }
      })
      .catch(() => {
        /* 保留最近一次有效快照 */
      });
  }, [applyLlm, memberChatId]);
  useEffect(() => {
    memberGen.current += 1;
    llmSeq.current += 1;
    llmRevision.current = 0;
    setLlm(null);
    void refreshLlm();
  }, [memberChatId, refreshLlm]);
  useEffect(
    () => () => {
      memberGen.current += 1;
      llmSeq.current += 1;
    },
    [],
  );
  useEffect(() => {
    if (!memberChatId) return;
    let active = true;
    const pending = api.onLlmStateChanged((payload) => {
      if (
        active &&
        activeMemberRef.current === memberChatId &&
        payload.chat_id === memberChatId
      ) {
        applyLlm(memberChatId, payload.state);
      }
    });
    // 订阅完成后再补读一次：首读与订阅建立之间的提交不会漏收。
    void pending.then((unlisten) => {
      if (!active) {
        unlisten();
        return;
      }
      void refreshLlm();
    });
    return () => {
      active = false;
      void pending.then((unlisten) => unlisten());
    };
  }, [applyLlm, memberChatId, refreshLlm]);

  // 新消息时滚动到底部。
  useEffect(() => {
    const el = transcriptRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [room?.lines.length, room?.activities.length]);

  const send = useCallback(async () => {
    const body = text.trim();
    if (!body || room?.running) return;
    try {
      await api.groupSend(id, body);
      setText("");
    } catch (e) {
      // 失败（如仍有编排在跑）时保留输入，用户不必重敲。
      onError(String(e));
    }
  }, [text, room, id, onError]);

  if (!room) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 text-sm text-gray-500">
        {failed ? "群聊加载失败" : "加载中…"}
        <div className="flex gap-2">
          {failed && (
            <button
              onClick={() => void refresh()}
              className="rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-300 transition hover:text-gray-100"
            >
              重试
            </button>
          )}
          <button
            onClick={onClose}
            className="rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-300 transition hover:text-gray-100"
          >
            返回列表
          </button>
        </div>
      </div>
    );
  }

  const titleOf = (name: string) =>
    bots.find((b) => b.name === name)?.title ?? name;

  return (
    <div className={`${PANEL} p-0`}>
      <div className="flex items-center justify-between border-b border-[#2a2f3a] px-4 py-2">
        <div className="flex min-w-0 items-center gap-1">
          <button
            onClick={onClose}
            title="返回列表"
            aria-label="返回群列表"
            className="mr-1 flex h-7 w-7 shrink-0 items-center justify-center rounded-lg text-gray-400 transition hover:bg-[#1a2030] hover:text-gray-100"
          >
            ←
          </button>
          <div className="min-w-0">
          <div className="truncate text-sm font-medium text-gray-100">
            👥 {room.name}
            {room.running && <span className="ml-2 text-xs text-emerald-400">编排进行中…</span>}
          </div>
          <div className="truncate text-[11px] text-gray-500">
            {room.members.map((m) => `@${m}`).join("  ")}
          </div>
          </div>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <select
            value={modelMember}
            onChange={(e) => setModelMember(e.target.value)}
            className="rounded-lg border border-[#2a2f3a] bg-[#1a1f29] px-1.5 py-1 text-[10px] text-gray-300 outline-none"
            aria-label="选择要设置模型的群成员"
          >
            {room.members.map((m) => (
              <option key={m} value={m}>
                @{m}
              </option>
            ))}
          </select>
          {llm && llm.chatId === memberChatId && (
            <ModelThinkingPicker
              key={memberChatId}
              state={llm.state}
              busy={room.running}
              compact={false}
              onOpen={refreshLlm}
              onModelSelect={async (model, expected) => {
                const gen = memberGen.current;
                const chat = memberChatId;
                try {
                  const next = await api.chatSetModel(
                    chat,
                    model,
                    expected.model.provider,
                    expected.model.model,
                    expected.revision,
                  );
                  if (gen === memberGen.current && activeMemberRef.current === chat) {
                    applyLlm(chat, next);
                  }
                } catch (e) {
                  if (gen === memberGen.current && activeMemberRef.current === chat) {
                    onError(String(e));
                    refreshLlm();
                  }
                }
              }}
              onThinkingSelect={async (value, expected) => {
                const gen = memberGen.current;
                const chat = memberChatId;
                try {
                  const next = await api.chatSetThinking(
                    chat,
                    value,
                    expected.model.provider,
                    expected.model.model,
                    expected.revision,
                  );
                  if (gen === memberGen.current && activeMemberRef.current === chat) {
                    applyLlm(chat, next);
                  }
                } catch (e) {
                  if (gen === memberGen.current && activeMemberRef.current === chat) {
                    onError(String(e));
                    refreshLlm();
                  }
                }
              }}
            />
          )}
          {room.running && (
            <button className={BTN} onClick={() => void api.groupCancel(id).catch((e) => onError(String(e)))}>
              取消编排
            </button>
          )}
        </div>
      </div>

      <div ref={transcriptRef} className="min-h-0 flex-1 space-y-2 overflow-y-auto px-4 py-3">
        {room.lines.length === 0 && (
          <div className="py-6 text-center text-xs text-gray-600">
            还没有消息。发一条任务试试，比如「@researcher 先查资料，@critic 复核」。
          </div>
        )}
        {room.lines.map((line) => {
          if (line.who === "user") {
            return (
              <div key={line.seq} className="flex justify-end">
                <div className="max-w-[80%] rounded-xl rounded-br-sm bg-indigo-500/20 px-3 py-2 text-xs leading-relaxed whitespace-pre-wrap text-indigo-100">
                  {line.text}
                </div>
              </div>
            );
          }
          const bot = line.who.replace(/^bot:/, "");
          return (
            <div key={line.seq} className="flex justify-start">
              <div>
                <div className="mb-0.5 text-[10px] text-gray-500">
                  🤖 {titleOf(bot)} @{bot}
                </div>
                <div className="max-w-[80%] rounded-xl rounded-bl-sm border border-[#2a2f3a] bg-[#1a1f29] px-3 py-2 text-xs leading-relaxed whitespace-pre-wrap text-gray-200">
                  {line.text}
                </div>
              </div>
            </div>
          );
        })}
        {room.running && (
          <div className="px-1 text-[11px] text-gray-500">
            {(() => {
              const last = room.activities[room.activities.length - 1];
              if (!last) return "编排进行中…";
              if (last.kind === "working") return `@${last.who} 正在思考…`;
              return ACTIVITY_LABEL[last.kind] ?? last.kind;
            })()}
          </div>
        )}
      </div>

      {/* 活动流（最近 5 条）。 */}
      <div className="max-h-24 overflow-y-auto border-t border-[#2a2f3a] px-4 py-1.5">
        {room.activities.slice(-5).map((a, i) => (
          <div key={i} className="truncate text-[10px] text-gray-600">
            {a.who ? `@${a.who} ` : ""}
            {ACTIVITY_LABEL[a.kind] ?? a.kind}
            {a.detail ? `：${a.detail}` : ""}
          </div>
        ))}
      </div>

      <div className="border-t border-[#2a2f3a] px-4 py-2">
        <div className="mb-1.5 flex flex-wrap gap-1">
          {room.members.map((m) => (
            <button
              key={m}
              className="rounded border border-[#2a2f3a] px-1.5 py-0.5 text-[10px] text-gray-400 transition hover:border-indigo-500/50 hover:text-indigo-200"
              onClick={() => setText((t) => `${t}@${m} `)}
            >
              @{m}
            </button>
          ))}
        </div>
        <div className="flex items-center gap-2">
          <textarea
            className={`${INPUT} min-h-10 flex-1 resize-none`}
            rows={2}
            value={text}
            disabled={room.running}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                void send();
              }
            }}
            placeholder={
              room.running
                ? "编排进行中，收敛或取消后可继续…"
                : "输入任务（Enter 发送，@名字 点名成员，Shift+Enter 换行）"
            }
          />
          <button className={BTN_PRIMARY} disabled={room.running || text.trim() === ""} onClick={() => void send()}>
            发送
          </button>
        </div>
      </div>
    </div>
  );
}
