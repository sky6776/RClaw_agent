import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  wxbotContactsError,
  type WxbotContact,
  type WxbotContactsErrorDto,
  type WxbotRecentChat,
} from "../lib/api";

export type PickerKind = "groups" | "contacts";

interface PickerEntry {
  id: string;
  /** React 列表键：REST 条目 = wxId；本地条目 = account + convId（跨账号同群 id 是
   *  两行候选，但选中态/写入值都按裸 convId 共享——它们要写的就是同一个值）。 */
  key: string;
  /** 主显示名：群名 / 备注 ?? 昵称 / 微信号；本地兜底列表无名称时为 null。 */
  title: string | null;
  /** id 恒展示（写入 allow_* 的就是它）。 */
  idLine: string;
  meta?: string;
}

interface Props {
  kind: PickerKind;
  /** 当前字段值（草稿 ?? 基线，逗号分隔 id）。 */
  currentValue: string;
  apiBase: string;
  apiToken: string;
  /** 确认时回调（写回逗号分隔的 id 串）；取消不回调。 */
  onConfirm: (value: string) => void;
  onClose: () => void;
}

const HINTS: Record<PickerKind, string> = {
  groups: "空 = 所有群不回复（消息仍全量落库）",
  contacts: "空 = 放行全部私聊",
};

const LABELS: Record<PickerKind, { title: string; empty: string }> = {
  groups: { title: "选择群", empty: "WCX 未返回任何群" },
  contacts: { title: "选择联系人", empty: "WCX 未返回任何联系人" },
};

function splitIds(value: string): string[] {
  return value
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}

function formatTs(ts: number): string {
  if (!ts) return "时间未知";
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getMonth() + 1}-${d.getDate()} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function contactEntry(c: WxbotContact): PickerEntry {
  const title = c.remarkName || c.nickname || c.customWxId || null;
  const extra = c.customWxId && c.customWxId !== title ? ` · ${c.customWxId}` : "";
  return { id: c.wxId, key: c.wxId, title, idLine: c.wxId + extra };
}

function recentEntry(r: WxbotRecentChat): PickerEntry {
  const acct = r.account ? ` · 账号 ${r.account}` : "";
  return {
    id: r.convId,
    key: `${r.account}\n${r.convId}`,
    title: null,
    idLine: r.convId + acct,
    meta: `${r.msgCount} 条 · 最近 ${formatTs(r.lastTs)}`,
  };
}

/** wxbot 白名单点选弹层：REST 候选优先，失败自动降级本机最近消息（无名称，仅 id）。
 *  状态全部组件内自治——配置页 3s 轮询不会清掉弹层的错误/搜索/选中。 */
export default function WxbotContactPicker({
  kind,
  currentValue,
  apiBase,
  apiToken,
  onConfirm,
  onClose,
}: Props) {
  const label = LABELS[kind];
  // 打开瞬间的草稿值即「已配置全集」：候选外 id（手填/未加载/被截断）原样保留；
  // 手填重复 id 在入口去重（选中集本就是 Set，这里只为 preserved 列表的 key 稳定）。
  const initialRef = useRef<string[]>([...new Set(splitIds(currentValue))]);
  const [phase, setPhase] = useState<"loading" | "ready">("loading");
  const [source, setSource] = useState<"rest" | "recent">("rest");
  const [entries, setEntries] = useState<PickerEntry[]>([]);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<WxbotContactsErrorDto | null>(null);
  const [fatal, setFatal] = useState<WxbotContactsErrorDto | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set(initialRef.current));
  const [preserved, setPreserved] = useState<string[]>([]);
  const [search, setSearch] = useState("");
  const [selfWxId, setSelfWxId] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const initial = initialRef.current;
    // 换连接参数重跑时清空上一次的结果态：fatal/error/notice/账号残留会把新成功
    // 误关进「列表不可用」分支、把旧账号用于新连接的过滤（轮询/草稿变更都会触发重跑）。
    setPhase("loading");
    setError(null);
    setNotice(null);
    setFatal(null);
    setEntries([]);
    setSource("rest");
    setPreserved([]);
    setSelfWxId(null);
    // 账号行独立请求：失败（null）不影响列表；promise 本身供 loadRecent 兜底时
    // 确定性地 await（不靠 state 时序）。
    const selfPromise: Promise<string | null> = api
      .wxbotContactsSelf(apiBase, apiToken)
      .then((wx) => {
        if (!cancelled) setSelfWxId(wx);
        return wx;
      })
      .catch(() => null);
    const loadRecent = async (err: WxbotContactsErrorDto) => {
      // self 已知时按账号过滤本地兜底行，避免换号历史混入当前账号候选；
      // 未知（WCX 不可达时 self 请求通常也失败 → null）则不过滤，行内账号标注由 UI 展示。
      const wx = await selfPromise;
      const recents = await api.wxbotContactsRecent(kind === "groups", wx, 200);
      const listed = new Set(recents.map((r) => r.convId));
      if (cancelled) return;
      setEntries(recents.map(recentEntry));
      setPreserved(initial.filter((id) => !listed.has(id)));
      setSource("recent");
      // unsupported 的说明走 amber notice（与语义提示并列），不再重复挂红色 banner。
      setError(err.code === "unsupported_endpoint" ? null : err);
      setNotice(
        err.code === "unsupported_endpoint"
          ? "当前 WCX 版本不支持联系人接口，已切换到本机最近消息。"
          : null,
      );
      setPhase("ready");
    };
    (async () => {
      try {
        const contacts = await api.wxbotContactsList(
          apiBase,
          apiToken,
          kind === "groups" ? "groups" : "friends",
        );
        if (cancelled) return;
        const listed = new Set(contacts.map((c) => c.wxId));
        setEntries(contacts.map(contactEntry));
        setPreserved(initial.filter((id) => !listed.has(id)));
        setSource("rest");
        setPhase("ready");
      } catch (e) {
        const err = wxbotContactsError(e);
        if (cancelled) return;
        try {
          await loadRecent(err);
        } catch (e2) {
          if (cancelled) return;
          // 双失败：fatal = WCX 侧主因（用户最需要知道的），error = 本地兜底为何也不可用。
          setFatal(err);
          setError(wxbotContactsError(e2));
          setPreserved(initial);
          setPhase("ready");
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [kind, apiBase, apiToken]);

  const toggle = useCallback((id: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  }, []);

  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return entries;
    return entries.filter(
      (e) =>
        e.id.toLowerCase().includes(q) ||
        (e.title ?? "").toLowerCase().includes(q) ||
        (e.meta ?? "").toLowerCase().includes(q),
    );
  }, [entries, search]);

  // 候选外区块 = effect 记录的 preserved ∪ 「已勾选但不在当前候选」的 id——后者
  // 必须可见才能被取消勾选（换源/重跑不静默吞选中）。
  const preservedShown = useMemo(() => {
    const outside = [...selected].filter((id) => !entries.some((e) => e.id === id));
    return [...new Set([...preserved, ...outside])];
  }, [preserved, selected, entries]);

  const confirm = useCallback(() => {
    // 顺序：候选外（原草稿序在前）→ 候选内按展示序；按值去重（本地兜底列表跨账号
    // 可能出现同 convId 两行，写入值只有一个）。
    const ordered = [...preservedShown, ...entries.map((e) => e.id)];
    const final: string[] = [];
    for (const id of ordered) {
      if (selected.has(id) && !final.includes(id)) final.push(id);
    }
    if (
      kind === "contacts" &&
      initialRef.current.length > 0 &&
      final.length === 0 &&
      !window.confirm(
        "清空 allow_from 意味着放行全部私聊（而不是禁止全部）。确定清空？",
      )
    ) {
      return;
    }
    onConfirm(final.join(", "));
    onClose();
  }, [kind, preservedShown, entries, selected, onConfirm, onClose]);

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4">
      <div className="flex max-h-[80vh] w-full max-w-lg flex-col rounded-xl border border-[#2a2f3a] bg-[#141821] shadow-2xl">
        <div className="border-b border-[#2a2f3a] px-4 py-3">
          <div className="flex items-center justify-between">
            <h2 className="text-sm font-medium text-gray-100">{label.title}</h2>
            <button
              onClick={onClose}
              className="text-xs text-gray-500 transition hover:text-gray-300"
            >
              关闭
            </button>
          </div>
          <p className="mt-1 font-mono text-[10px] text-gray-600">
            WCX 当前登录账号：{selfWxId === null ? "未知（未登录或 WCX 未返回）" : selfWxId}
            {source === "recent" && " · 本机最近消息（不含名称）"}
          </p>
        </div>

        {phase === "loading" ? (
          <div className="flex flex-1 items-center justify-center py-10 text-sm text-gray-500">
            加载中…
          </div>
        ) : (
          <>
            {error && !fatal && (
              <div className="mx-4 mt-3 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-[11px] leading-relaxed text-red-300">
                {error.message}
              </div>
            )}
            {fatal ? (
              <>
                <div className="flex flex-1 flex-col justify-center gap-2 px-6 py-6 text-center">
                  <p className="text-xs leading-relaxed text-red-300 break-all">
                    {fatal.message}
                  </p>
                  {error && (
                    <p className="text-[10px] leading-relaxed text-gray-500 break-all">
                      本地兜底也不可用：{error.message}
                    </p>
                  )}
                  <p className="text-xs text-gray-500">
                    关闭弹层后可直接在输入框手工填写 id（已勾选的项会保留）。
                  </p>
                </div>
                <div className="border-t border-[#2a2f3a] px-4 py-3">
                  <div className="flex items-center justify-between">
                    <span className="text-[10px] text-gray-500">
                      已选 {selected.size} 项
                    </span>
                    <div className="flex items-center gap-2">
                      <button
                        onClick={confirm}
                        className="rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500"
                      >
                        写入选择
                      </button>
                      <button
                        onClick={onClose}
                        className="rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-300 transition hover:text-gray-100"
                      >
                        关闭
                      </button>
                    </div>
                  </div>
                </div>
              </>
            ) : (
              <>
                {entries.length > 0 && (
                  <div className="px-4 pt-3">
                    <input
                      value={search}
                      onChange={(e) => setSearch(e.target.value)}
                      placeholder={
                        source === "rest" ? "按名称 / id 搜索" : "按 id 搜索"
                      }
                      className="w-full rounded-md border border-[#2a2f3a] bg-[#0f131b] px-2 py-1.5 font-mono text-xs text-gray-200 focus:border-indigo-500/60 focus:outline-none"
                    />
                  </div>
                )}
                <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3">
                  {entries.length === 0 ? (
                    <p className="py-8 text-center text-xs text-gray-500">
                      {label.empty}
                      {source === "recent" && (
                        <span className="mt-1 block text-[10px] text-gray-600">
                          {selfWxId
                            ? "本机当前账号没有最近消息记录（其他账号/未知账号的记录已过滤）。"
                            : "本机宿主没有收到过 wxbot 消息记录（手机端部署的记录不在本机）。"}
                        </span>
                      )}
                    </p>
                  ) : (
                    <div className="space-y-1">
                      {filtered.map((e) => (
                        <label
                          key={e.key}
                          className="flex cursor-pointer items-start gap-2.5 rounded-lg border border-transparent px-2 py-1.5 transition hover:border-[#3a4150]"
                        >
                          <input
                            type="checkbox"
                            checked={selected.has(e.id)}
                            onChange={() => toggle(e.id)}
                            className="mt-0.5 h-3.5 w-3.5 accent-indigo-500"
                          />
                          <span className="min-w-0">
                            <span className="block truncate text-xs text-gray-200">
                              {e.title ?? "（无名称，仅 id）"}
                            </span>
                            <span className="block truncate font-mono text-[10px] text-gray-500">
                              {e.idLine}
                            </span>
                            {e.meta && (
                              <span className="block text-[10px] text-gray-600">{e.meta}</span>
                            )}
                          </span>
                        </label>
                      ))}
                      {filtered.length === 0 && (
                        <p className="py-4 text-center text-[10px] text-gray-600">
                          无匹配项（搜索只过滤已加载列表）
                        </p>
                      )}
                    </div>
                  )}
                  {preservedShown.length > 0 && (
                    <div className="mt-3 rounded-lg border border-[#2a2f3a] bg-[#0f131b] p-2.5">
                      <p className="text-[10px] text-gray-500">
                        已配置/已选中但不在以上列表（保持不变，取消勾选即移除）：
                      </p>
                      <div className="mt-1.5 space-y-1">
                        {preservedShown.map((id) => (
                          <label
                            key={id}
                            className="flex cursor-pointer items-center gap-2.5 px-1 py-0.5"
                          >
                            <input
                              type="checkbox"
                              checked={selected.has(id)}
                              onChange={() => toggle(id)}
                              className="h-3.5 w-3.5 accent-indigo-500"
                            />
                            <span className="truncate font-mono text-[10px] text-gray-400">
                              {id}
                            </span>
                          </label>
                        ))}
                      </div>
                    </div>
                  )}
                </div>
                <div className="border-t border-[#2a2f3a] px-4 py-3">
                  {notice && (
                    <p className="mb-1 text-[10px] text-amber-400">{notice}</p>
                  )}
                  <p className="text-[10px] text-gray-600">
                    {HINTS[kind]}。选择只改写本项草稿，点底部「保存到
                    rclaw.toml」后需重启应用生效。
                  </p>
                  <div className="mt-2 flex items-center justify-between">
                    <span className="text-[10px] text-gray-500">
                      已选 {selected.size} 项
                    </span>
                    <div className="flex items-center gap-2">
                      <button
                        onClick={onClose}
                        className="rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-400 transition hover:text-gray-200"
                      >
                        取消
                      </button>
                      <button
                        onClick={confirm}
                        className="rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500"
                      >
                        写入选择
                      </button>
                    </div>
                  </div>
                </div>
              </>
            )}
          </>
        )}
      </div>
    </div>
  );
}
