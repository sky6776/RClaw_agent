import { useCallback, useEffect, useState } from "react";
import {
  api,
  type SkillApprovalItem,
  type SkillConfigEntry,
} from "../lib/api";

/** 技能脚本审批 + 技能配置编辑（安卓 APK 内可用；桌面无此闸门，接口报错即整卡隐藏）。 */
export default function SkillOpsCard() {
  const [available, setAvailable] = useState(true);
  const [pending, setPending] = useState<SkillApprovalItem[] | null>(null);
  const [configs, setConfigs] = useState<SkillConfigEntry[]>([]);
  const [editingPath, setEditingPath] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [dirty, setDirty] = useState(false);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);

  const refreshPending = useCallback(async () => {
    try {
      const items = await api.skillApprovalsPending();
      setPending(items);
      setAvailable(true);
    } catch {
      // 桌面端无技能闸门：整卡隐藏。
      setAvailable(false);
    }
  }, []);

  const refreshConfigs = useCallback(async () => {
    try {
      setConfigs(await api.skillConfigList());
    } catch {
      // 同上。
    }
  }, []);

  useEffect(() => {
    void refreshPending();
    void refreshConfigs();
    const timer = setInterval(() => void refreshPending(), 8000);
    return () => clearInterval(timer);
  }, [refreshPending, refreshConfigs]);

  const approve = async (script: string) => {
    setBusy(true);
    setMsg(null);
    try {
      await api.skillApprovalsApprove(script);
      await refreshPending();
      setMsg(`已批准 ${script}`);
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  const revoke = async (script: string) => {
    setBusy(true);
    setMsg(null);
    try {
      await api.skillApprovalsRevoke(script);
      await refreshPending();
      setMsg(`已撤销批准 ${script}（回退内置基线判定）`);
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  const openConfig = async (path: string) => {
    setMsg(null);
    try {
      setDraft(await api.skillConfigRead(path));
      setEditingPath(path);
      setDirty(false);
    } catch (e) {
      setMsg(String(e));
    }
  };

  const saveConfig = async () => {
    if (!editingPath) return;
    setBusy(true);
    setMsg(null);
    try {
      await api.skillConfigWrite(editingPath, draft);
      setDirty(false);
      setMsg(`已保存 ${editingPath}（相关脚本批准已自动摘除，重新批准后生效）`);
      await refreshConfigs();
      await refreshPending();
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  };

  if (!available) return null;

  return (
    <div className="mb-4 rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
      <h2 className="mb-1 text-sm font-medium text-gray-200">技能脚本审批</h2>
      <p className="mb-2 text-[10px] leading-relaxed text-gray-600">
        技能脚本被修改或新增后，执行前需要在此批准当前内容（批准按内容哈希记录）。
        {pending !== null && ` 待处理 ${pending.length} 项。`}
      </p>
      {pending === null ? (
        <p className="text-xs text-gray-600">加载中…</p>
      ) : pending.length === 0 ? (
        <p className="text-xs text-emerald-400">全部脚本均为已批准版本。</p>
      ) : (
        <div className="space-y-1.5">
          {pending.map((item) => (
            <div
              key={item.script}
              className="flex items-center justify-between gap-2 rounded-lg border border-[#2a2f3a] bg-[#0f131b] px-2.5 py-1.5"
            >
              <div className="min-w-0 flex-1">
                <div className="truncate font-mono text-[11px] text-amber-300">{item.script}</div>
                <div className="text-[10px] text-gray-600">
                  {item.has_baseline ? "与内置基线不一致" : "新脚本（无内置基线）"}
                </div>
              </div>
              <button
                disabled={busy}
                onClick={() => void approve(item.script)}
                className="shrink-0 rounded-lg bg-indigo-600 px-2.5 py-1 text-[11px] font-medium text-white transition hover:bg-indigo-500 disabled:opacity-40"
              >
                批准当前版本
              </button>
              {item.has_baseline && (
                <button
                  disabled={busy}
                  onClick={() => void revoke(item.script)}
                  className="shrink-0 rounded-lg border border-[#2a2f3a] px-2.5 py-1 text-[11px] text-gray-400 transition hover:text-gray-200 disabled:opacity-40"
                >
                  撤销批准
                </button>
              )}
            </div>
          ))}
        </div>
      )}

      <h2 className="mb-1 mt-4 text-sm font-medium text-gray-200">技能配置（API Key 等）</h2>
      <p className="mb-2 text-[10px] leading-relaxed text-gray-600">
        直接编辑 `skills/*/config.toml`：保存后相关脚本的批准会自动摘除，需重新批准。
      </p>
      {configs.length === 0 ? (
        <p className="text-xs text-gray-600">暂无配置文件。</p>
      ) : (
        <div className="flex flex-wrap gap-1.5">
          {configs.map((c) => (
            <button
              key={c.path}
              onClick={() => void openConfig(c.path)}
              className={`rounded-lg border px-2 py-1 font-mono text-[11px] transition ${
                editingPath === c.path
                  ? "border-indigo-500/60 bg-indigo-500/10 text-indigo-200"
                  : "border-[#2a2f3a] text-gray-400 hover:text-gray-200"
              }`}
            >
              {c.path}
            </button>
          ))}
        </div>
      )}
      {editingPath && (
        <div className="mt-2">
          <textarea
            value={draft}
            onChange={(e) => {
              setDraft(e.target.value);
              setDirty(true);
            }}
            rows={8}
            spellCheck={false}
            className="w-full rounded-md border border-[#2a2f3a] bg-[#0f131b] px-2 py-1.5 font-mono text-xs leading-relaxed text-gray-200 focus:border-indigo-500/60 focus:outline-none"
          />
          <div className="mt-1.5 flex items-center justify-end gap-2">
            {dirty && <span className="text-[10px] text-amber-400">有未保存修改</span>}
            <button
              disabled={busy || !dirty}
              onClick={() => void saveConfig()}
              className="rounded-lg bg-indigo-600 px-3 py-1 text-xs font-medium text-white transition hover:bg-indigo-500 disabled:opacity-40"
            >
              保存 {editingPath}
            </button>
          </div>
        </div>
      )}
      {msg && <p className="mt-2 text-[11px] text-gray-400">{msg}</p>}
    </div>
  );
}
