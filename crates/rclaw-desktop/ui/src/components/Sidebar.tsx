/** 侧栏：会话列表（最近 / 智能体 / 群聊 / 归档）+ 搜索 + 新建分裂菜单 + 设置。
 *
 * 桌面常驻（可收起为图标栏）；移动端放进抽屉（compact=true 时不渲染收起逻辑与用量卡
 * 之外的桌面噪音）。聊天/房间/菜单状态仍由 App 持有，这里只负责呈现与触发。
 */
import { useEffect, useMemo, useRef, useState } from "react";

import type { BotEntry, ChatMeta, GroupRoom, SyncSessionMark, UsageSnapshot } from "../lib/api";
import { useBackClose } from "../lib/backNav";
import { syncTitle } from "../lib/syncUi";
import { SyncMarkTags } from "./SyncBits";
import { BellIcon, BotIcon, GearIcon, GlobeIcon, SearchIcon, UsersIcon } from "./NavIcons";

export interface SidebarProps {
  compact: boolean;
  collapsed: boolean;
  onToggleCollapse?: () => void;
  chats: ChatMeta[];
  rooms: GroupRoom[];
  activeId: string;
  busyByChat: Record<string, boolean>;
  syncMarks: Record<string, SyncSessionMark>;
  syncPending: number;
  syncOn: boolean;
  importBusy: boolean;
  activityCount: number;
  activityPulse: boolean;
  usage: UsageSnapshot | null;
  homePath?: string;
  onOpenChat: (id: string) => void;
  onOpenRoom: (id: string) => void;
  onChatMenu: (chatId: string, title: string, recordId: number | undefined, x: number, y: number, trigger: HTMLElement) => void;
  onRoomMenu: (room: GroupRoom, x: number, y: number) => void;
  onRename: (chatId: string, title: string) => Promise<void>;
  onArchive: (chatId: string, recordId: number | undefined) => void;
  onRestore: (chatId: string, recordId: number | undefined) => void;
  onDeleteChat: (chatId: string, title: string, recordId: number | undefined) => void;
  onNewChat: () => void;
  onNewLocalChat: () => void;
  onImportBundle: (file: File) => void;
  onOpenBotChat: (name: string) => void;
  onOpenGroupCreate: () => void;
  onManageBots: () => void;
  onOpenSettings: () => void;
  onOpenActivity: () => void;
  onOpenBrowser?: () => void;
  loadBots: () => Promise<BotEntry[]>;
}

type GroupKey = "recent" | "bots" | "rooms" | "archived";

function loadCollapsedGroups(): Record<string, boolean> {
  try {
    const raw = localStorage.getItem("rclaw_sidebar_groups");
    if (!raw) return {};
    const parsed = JSON.parse(raw) as Record<string, boolean>;
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch {
    return {};
  }
}

export default function Sidebar({
  compact,
  collapsed,
  onToggleCollapse,
  chats,
  rooms,
  activeId,
  busyByChat,
  syncMarks,
  syncPending,
  syncOn,
  importBusy,
  activityCount,
  activityPulse,
  usage,
  homePath,
  onOpenChat,
  onOpenRoom,
  onChatMenu,
  onRoomMenu,
  onRename,
  onArchive,
  onRestore,
  onDeleteChat,
  onNewChat,
  onNewLocalChat,
  onImportBundle,
  onOpenBotChat,
  onOpenGroupCreate,
  onManageBots,
  onOpenSettings,
  onOpenActivity,
  onOpenBrowser,
  loadBots,
}: SidebarProps) {
  const [search, setSearch] = useState("");
  const [collapsedGroups, setCollapsedGroups] =
    useState<Record<string, boolean>>(loadCollapsedGroups);
  const [newMenuOpen, setNewMenuOpen] = useState(false);
  const [bots, setBots] = useState<BotEntry[]>([]);
  const [botsError, setBotsError] = useState<string | null>(null);
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  const [archivedOpen, setArchivedOpen] = useState(
    () => localStorage.getItem("rclaw_sidebar_archived_open") === "1",
  );
  const toggleArchived = () => {
    setArchivedOpen((v) => {
      const next = !v;
      try {
        localStorage.setItem("rclaw_sidebar_archived_open", next ? "1" : "0");
      } catch {
        /* localStorage 不可用时仅本次生效 */
      }
      return next;
    });
  };
  const inputRef = useRef<HTMLInputElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const newBtnRef = useRef<HTMLButtonElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  useBackClose(newMenuOpen, () => setNewMenuOpen(false));

  useEffect(() => {
    if (!newMenuOpen) return;
    const onDown = (event: PointerEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (menuRef.current?.contains(target) || newBtnRef.current?.contains(target)) return;
      setNewMenuOpen(false);
    };
    document.addEventListener("pointerdown", onDown, true);
    return () => document.removeEventListener("pointerdown", onDown, true);
  }, [newMenuOpen]);

  useEffect(() => {
    if (!newMenuOpen) return;
    setBotsError(null);
    void loadBots()
      .then(setBots)
      .catch((e) => setBotsError(String(e)));
  }, [newMenuOpen, loadBots]);

  const toggleGroup = (key: GroupKey) => {
    setCollapsedGroups((prev) => {
      const next = { ...prev, [key]: !prev[key] };
      try {
        localStorage.setItem("rclaw_sidebar_groups", JSON.stringify(next));
      } catch {
        /* localStorage 不可用时仅本次生效 */
      }
      return next;
    });
  };

  const query = search.trim().toLowerCase();
  const match = (text: string) => !query || text.toLowerCase().includes(query);
  const bySearch = (c: ChatMeta) => match(c.title || c.chat_id) || match(c.chat_id);

  const { recent, botChats, archived } = useMemo(() => {
    const recent: ChatMeta[] = [];
    const botChats: ChatMeta[] = [];
    const archived: ChatMeta[] = [];
    for (const chat of chats) {
      if (chat.archived) {
        archived.push(chat);
      } else if (chat.chat_id.startsWith("bot-")) {
        botChats.push(chat);
      } else {
        recent.push(chat);
      }
    }
    return { recent, botChats, archived };
  }, [chats]);

  const visible = (list: ChatMeta[]) => (query ? list.filter(bySearch) : list);
  const visibleRooms = query
    ? rooms.filter((room) => match(room.name) || match(room.id))
    : rooms;

  const startRename = (chat: ChatMeta) => {
    setRenamingId(chat.chat_id);
    setRenameDraft(chat.title || chat.chat_id);
  };
  const commitRename = async () => {
    const id = renamingId;
    if (!id) return;
    const title = renameDraft.trim();
    setRenamingId(null);
    if (!title) return;
    try {
      await onRename(id, title);
    } catch {
      /* App 已提示 */
    }
  };

  const rowClass = (active: boolean) =>
    `group flex items-center rounded-lg transition ${
      active ? "bg-[#1a2030] text-indigo-200" : "text-gray-400 hover:bg-[#1a1f29]"
    }`;

  const chatRow = (chat: ChatMeta) => {
    const title = syncTitle(chat.title || chat.chat_id, syncMarks[chat.chat_id]);
    const busy = !!busyByChat[chat.chat_id];
    return (
      <div key={chat.chat_id} data-chat-row className={rowClass(activeId === chat.chat_id)}>
        {renamingId !== chat.chat_id &&
          (busy ? (
            <span
              title="Agent 工作中"
              className="ml-2.5 h-2.5 w-2.5 shrink-0 animate-spin rounded-full border-[1.5px] border-indigo-400 border-t-transparent"
            />
          ) : (
            <span title="空闲" className="ml-3 h-2 w-2 shrink-0 rounded-full bg-emerald-500" />
          ))}
        {renamingId === chat.chat_id ? (
          <input
            autoFocus
            value={renameDraft}
            onChange={(e) => setRenameDraft(e.target.value)}
            onClick={(e) => e.stopPropagation()}
            onKeyDown={(e) => {
              if (e.key === "Enter") void commitRename();
              else if (e.key === "Escape") setRenamingId(null);
            }}
            onBlur={() => void commitRename()}
            maxLength={120}
            placeholder={chat.chat_id}
            className="min-w-0 flex-1 rounded-md border border-indigo-500/50 bg-[#161a22] px-2 py-1 text-xs text-gray-200 outline-none"
          />
        ) : (
          <button
            onClick={() => onOpenChat(chat.chat_id)}
            onDoubleClick={() => startRename(chat)}
            onContextMenu={(e) => {
              e.preventDefault();
              onChatMenu(
                chat.chat_id,
                chat.title || chat.chat_id,
                chat.record_id,
                e.clientX,
                Math.min(e.clientY, window.innerHeight - 230),
                e.currentTarget,
              );
            }}
            className="min-w-0 flex-1 px-2 py-1.5 text-left max-md:py-2.5"
            title={`${title}（双击重命名，右键更多操作）`}
          >
            <div className="flex items-center gap-1.5">
              <span className="flex min-w-0 items-center gap-1 truncate text-xs max-md:text-[13px]">
                {chat.chat_id.startsWith("bot-") && (
                  <span className="shrink-0 text-gray-500">
                    <BotIcon />
                  </span>
                )}
                <span className="min-w-0 truncate">{title}</span>
              </span>
              <SyncMarkTags mark={syncMarks[chat.chat_id]} />
            </div>
          </button>
        )}
        {!busy && renamingId !== chat.chat_id && (
          <>
            <button
              onClick={(e) => {
                e.stopPropagation();
                const r = e.currentTarget.getBoundingClientRect();
                onChatMenu(
                  chat.chat_id,
                  chat.title || chat.chat_id,
                  chat.record_id,
                  Math.min(r.left, window.innerWidth - 176),
                  Math.min(r.bottom + 4, window.innerHeight - 230),
                  e.currentTarget,
                );
              }}
              title="会话操作"
              className="mr-0.5 hidden shrink-0 rounded px-1.5 py-0.5 text-xs leading-none text-gray-500 transition hover:text-gray-200 group-hover:block"
            >
              ⋯
            </button>
            <button
              onClick={() => onArchive(chat.chat_id, chat.record_id)}
              title="归档此会话"
              className="mr-1 hidden rounded px-1 text-[10px] text-gray-500 transition hover:text-amber-300 group-hover:block"
            >
              归档
            </button>
          </>
        )}
      </div>
    );
  };

  const roomRow = (room: GroupRoom) => (
    <div key={room.id} className={rowClass(false)}>
      <span
        title={room.running ? "编排进行中" : "空闲"}
        className={`ml-3 h-2 w-2 shrink-0 rounded-full ${
          room.running ? "animate-pulse bg-emerald-400" : "bg-emerald-500/60"
        }`}
      />
      <button
        onClick={() => onOpenRoom(room.id)}
        className="min-w-0 flex-1 px-2 py-1.5 text-left max-md:py-2.5"
        title={`${room.name}（${room.members.length} 个成员）`}
      >
        <span className="flex min-w-0 items-center gap-1 truncate text-xs max-md:text-[13px]">
          <span className="shrink-0 text-gray-500">
            <UsersIcon />
          </span>
          <span className="min-w-0 truncate">{room.name}</span>
        </span>
      </button>
      <button
        onClick={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          onRoomMenu(room, r.left, Math.min(r.bottom + 4, window.innerHeight - 160));
        }}
        title="群聊操作"
        className="mr-1 shrink-0 rounded px-1.5 py-0.5 text-xs leading-none text-gray-500 transition hover:text-gray-200"
      >
        ⋯
      </button>
    </div>
  );

  const groupHeader = (key: GroupKey, label: string, count: number) => (
    <button
      onClick={() => toggleGroup(key)}
      className="mt-2 flex w-full items-center justify-between rounded-lg px-2 py-1 text-[11px] text-gray-500 transition hover:text-gray-300"
    >
      <span>
        {label}
        {count > 0 ? `（${count}）` : ""}
      </span>
      <span>{collapsedGroups[key] ? "▸" : "▾"}</span>
    </button>
  );

  const showRecent = recent.length > 0 || visible(recent).length > 0;
  const showBots = botChats.length > 0;
  const showRooms = rooms.length > 0;
  const showArchived = archived.length > 0;

  if (collapsed && !compact) {
    return (
      <div className="relative flex h-full flex-col items-center gap-1 py-3">
        <button
          ref={newBtnRef}
          onClick={() => setNewMenuOpen((v) => !v)}
          title="新建"
          className="flex h-9 w-9 items-center justify-center rounded-lg text-lg text-gray-400 transition hover:bg-[#1a1f29] hover:text-indigo-300"
        >
          ＋
        </button>
        <button
          onClick={() => {
            onToggleCollapse?.();
            setTimeout(() => searchRef.current?.focus(), 60);
          }}
          title="搜索会话"
          className="flex h-9 w-9 items-center justify-center rounded-lg text-gray-400 transition hover:bg-[#1a1f29] hover:text-gray-200"
        >
          <SearchIcon />
        </button>
        <button
          onClick={onOpenActivity}
          title="活动"
          className="relative flex h-9 w-9 items-center justify-center rounded-lg text-gray-400 transition hover:bg-[#1a1f29] hover:text-gray-200"
        >
          <BellIcon />
          {activityCount > 0 && (
            <span className="absolute right-1 top-1 h-2 w-2 rounded-full bg-amber-400" />
          )}
        </button>
        <div className="flex-1" />
        <button
          onClick={onOpenSettings}
          title="设置"
          className="relative flex h-9 w-9 items-center justify-center rounded-lg text-gray-400 transition hover:bg-[#1a1f29] hover:text-gray-200"
        >
          <GearIcon />
          {syncPending > 0 && (
            <span className="absolute right-0.5 top-0.5 min-w-3 rounded-full bg-amber-500/90 px-0.5 text-[9px] leading-3 text-black">
              {syncPending}
            </span>
          )}
        </button>
        <button
          onClick={onToggleCollapse}
          title="展开侧栏"
          className="flex h-9 w-9 items-center justify-center rounded-lg text-xs text-gray-500 transition hover:bg-[#1a1f29] hover:text-gray-200"
        >
          »
        </button>
        {newMenuOpen && (
          <div ref={menuRef} className="absolute left-14 top-3 z-50">
            {newMenu()}
          </div>
        )}
      </div>
    );
  }

  function newMenu() {
    const item =
      "block w-full whitespace-nowrap rounded-md px-3 py-1.5 text-left text-xs text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100";
    return (
      <div className="min-w-44 rounded-xl border border-[#2a2f3a] bg-[#141821] py-1 shadow-2xl">
        <button
          className={item}
          onClick={() => {
            setNewMenuOpen(false);
            onNewChat();
          }}
        >
          新建对话
        </button>
        {syncOn && (
          <button
            className={item}
            onClick={() => {
              setNewMenuOpen(false);
              onNewLocalChat();
            }}
          >
            仅本机对话
          </button>
        )}
        <button
          className={item}
          disabled={importBusy}
          onClick={() => {
            setNewMenuOpen(false);
            inputRef.current?.click();
          }}
        >
          {importBusy ? "导入中…" : "导入会话包…"}
        </button>
        <div className="my-1 border-t border-[#252b36]" />
        <button
          className={item}
          onClick={() => {
            setNewMenuOpen(false);
            onOpenGroupCreate();
          }}
        >
          发起群聊…
        </button>
        {bots.length > 0 && (
          <>
            <div className="px-3 py-1 text-[10px] uppercase tracking-wide text-gray-600">
              与智能体对话
            </div>
            <div className="max-h-40 overflow-y-auto">
              {bots.map((bot) => (
                <button
                  key={bot.name}
                  className={item}
                  onClick={() => {
                    setNewMenuOpen(false);
                    onOpenBotChat(bot.name);
                  }}
                >
                  <span className="inline-flex items-center gap-1.5">
                    <BotIcon /> {bot.title}
                    <span className="text-gray-600">@{bot.name}</span>
                  </span>
                </button>
              ))}
            </div>
          </>
        )}
        {botsError && <div className="px-3 py-1 text-[10px] text-red-400">{botsError}</div>}
        <div className="my-1 border-t border-[#252b36]" />
        <button
          className={item}
          onClick={() => {
            setNewMenuOpen(false);
            onManageBots();
          }}
        >
          管理智能体…
        </button>
      </div>
    );
  }

  return (
    <div className="relative flex h-full min-h-0 flex-col">
      <input
        ref={inputRef}
        type="file"
        className="hidden"
        onChange={(e) => {
          const file = e.target.files && e.target.files[0];
          e.target.value = "";
          if (file) onImportBundle(file);
        }}
      />
      <div className={`flex items-center gap-2 pb-1 pt-3 ${compact ? "px-3" : "px-3"}`}>
        {!compact && (
          <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-indigo-600 text-sm font-bold text-white">
            R
          </div>
        )}
        {!compact && <span className="min-w-0 flex-1 truncate font-semibold text-gray-100">RClaw</span>}
        <button
          ref={newBtnRef}
          onClick={() => setNewMenuOpen((v) => !v)}
          title="新建"
          className="ml-auto flex h-8 items-center gap-0.5 rounded-lg px-2 text-base leading-none text-gray-400 transition hover:bg-[#1a1f29] hover:text-indigo-300"
        >
          ＋<span className="text-[10px]">▾</span>
        </button>
        {!compact && (
          <button
            onClick={onToggleCollapse}
            title="收起侧栏"
            className="rounded-md px-1.5 py-1.5 text-gray-500 transition hover:bg-[#1a1f29] hover:text-gray-200"
          >
            «
          </button>
        )}
      </div>
      {newMenuOpen && (
        <div ref={menuRef} className="absolute left-3 top-12 z-50">
          {newMenu()}
        </div>
      )}
      {compact && (
        <div className="flex items-center gap-2 px-3 pb-1">
          <button
            onClick={onOpenActivity}
            title="活动"
            className="relative flex h-9 flex-1 items-center justify-center rounded-lg border border-[#2a2f3a] text-sm text-gray-300"
          >
            <BellIcon /> 活动
            {activityCount > 0 && (
              <span className="ml-1 rounded-full bg-amber-500/15 px-1.5 text-[10px] text-amber-300">
                {activityCount}
              </span>
            )}
          </button>
          {onOpenBrowser && (
            <button
              onClick={onOpenBrowser}
              title="浏览器"
              className="flex h-9 items-center justify-center rounded-lg border border-[#2a2f3a] px-3 text-gray-300"
            >
              <GlobeIcon />
            </button>
          )}
        </div>
      )}
      <div className="px-3 pb-1">
        <input
          ref={searchRef}
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              e.stopPropagation();
              setSearch("");
              (e.target as HTMLInputElement).blur();
            }
          }}
          placeholder="搜索会话"
          className="w-full rounded-lg border border-[#2a2f3a] bg-[#141821] px-2.5 py-1.5 text-xs text-gray-200 outline-none transition focus:border-indigo-500/50"
        />
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-3 pb-2">
        {showRecent && (
          <>
            {groupHeader("recent", "最近", recent.length)}
            {!collapsedGroups.recent || query ? (
              <div className="space-y-0.5">{visible(recent).map(chatRow)}</div>
            ) : null}
          </>
        )}
        {showBots && (
          <>
            {groupHeader("bots", "智能体", botChats.length)}
            {!collapsedGroups.bots || query ? (
              <div className="space-y-0.5">{visible(botChats).map(chatRow)}</div>
            ) : null}
          </>
        )}
        {showRooms && (
          <>
            {groupHeader("rooms", "群聊", rooms.length)}
            {!collapsedGroups.rooms || query ? (
              <div className="space-y-0.5">{visibleRooms.map(roomRow)}</div>
            ) : null}
          </>
        )}
        {showArchived && (
          <>
            <button
              onClick={toggleArchived}
              className="mt-2 flex w-full items-center justify-between rounded-lg px-2 py-1 text-[11px] text-gray-500 transition hover:text-gray-300"
            >
              <span>已归档（{archived.length}）</span>
              <span>{archivedOpen || query ? "▾" : "▸"}</span>
            </button>
            {(archivedOpen || query) && (
              <div className="space-y-0.5">
                {visible(archived).map((chat) => (
                  <div key={chat.chat_id} className={rowClass(false)}>
                    <span className="min-w-0 flex-1 truncate px-2 py-1.5 text-xs" title={chat.title || chat.chat_id}>
                      {chat.title || chat.chat_id}
                    </span>
                    <button
                      onClick={() => onRestore(chat.chat_id, chat.record_id)}
                      title="恢复此会话"
                      className="mr-1 rounded px-1 text-[10px] text-gray-500 transition hover:text-emerald-300"
                    >
                      恢复
                    </button>
                    <button
                      onClick={() => onDeleteChat(chat.chat_id, chat.title || chat.chat_id, chat.record_id)}
                      title="永久删除此会话"
                      className="mr-1 rounded px-1 text-[10px] text-gray-500 transition hover:text-red-400"
                    >
                      删除
                    </button>
                  </div>
                ))}
              </div>
            )}
          </>
        )}
        {!showRecent && !showBots && !showRooms && !showArchived && (
          <div className="px-2 py-2 text-xs text-gray-600">
            {query ? "没有匹配的会话" : "暂无会话"}
          </div>
        )}
      </div>
      {compact && (
        <div className="mx-3 mb-2 flex items-center justify-between rounded-xl border border-[#2a2f3a] bg-[#141821] px-3 py-2 text-[11px] text-gray-500">
          <span className="flex items-center gap-1.5">
            <span
              className={`inline-block h-1.5 w-1.5 rounded-full ${
                usage && usage.context_length > 0 && usage.request_used / usage.context_length > 0.8
                  ? "bg-red-500"
                  : "bg-indigo-500"
              }`}
            />
            上下文
            {usage && usage.context_length > 0
              ? ` ${Math.round((usage.request_used / usage.context_length) * 100)}%`
              : " —"}
          </span>
          {usage && (
            <span className="tabular-nums">
              {Math.round(usage.request_used / 1000)}K / {Math.round(usage.context_length / 1000)}K
            </span>
          )}
        </div>
      )}
      {compact && homePath && (
        <div className="mx-3 mb-2 truncate text-[10px] text-gray-600" title={homePath}>
          {homePath}
        </div>
      )}
      <div className="border-t border-[#252b36] px-3 py-2">
        <button
          onClick={onOpenSettings}
          className="relative flex w-full items-center gap-2.5 rounded-lg px-2 py-2 text-left text-sm text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100 max-md:text-[15px]"
        >
          <span className="flex h-4 w-4 shrink-0 items-center justify-center text-gray-500">
            <GearIcon />
          </span>
          <span className="min-w-0 flex-1 truncate">设置</span>
          {syncPending > 0 && (
            <span
              title={`${syncPending} 件多端同步事项待处理`}
              className="inline-flex min-w-[1.25rem] items-center justify-center rounded-full bg-amber-500/15 px-1.5 py-px font-mono text-[10px] tabular-nums text-amber-300"
            >
              {syncPending}
            </span>
          )}
        </button>
      </div>
    </div>
  );
}
