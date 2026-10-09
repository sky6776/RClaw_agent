import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  type ConfigEdit,
  type UpdateInfo,
  type ConfigItem,
  type ConfigSection,
  type ConfigStateDto,
  type PermissionMode,
  type SloganPrefs,
} from "../lib/api";
import { isAndroidDevice } from "../lib/useIsMobile";
import SkillOpsCard from "./SkillOpsCard";

/** 示例默认值 → 输入框基线（str 去引号、数组转逗号串）。 */
function baselineOf(item: ConfigItem): string {
  if (item.current !== null) return item.current;
  const d = item.default_raw;
  if (item.kind === "str") return d.replace(/^"|"$/g, "");
  if (item.kind === "str_array")
    return d
      .replace(/^\[/, "")
      .replace(/\]$/, "")
      .split(",")
      .map((s) => s.trim().replace(/^"|"$/g, ""))
      .join(", ");
  return d;
}

function ItemRow({
  item,
  draft,
  onChange,
}: {
  item: ConfigItem;
  draft: string;
  onChange: (value: string) => void;
}) {
  const baseline = baselineOf(item);
  const dirty = draft !== baseline;
  const input =
    item.kind === "bool" ? (
      <input
        type="checkbox"
        checked={draft === "true"}
        onChange={(e) => onChange(e.target.checked ? "true" : "false")}
        className="h-4 w-4 accent-indigo-500"
      />
    ) : item.kind === "int" || item.kind === "float" ? (
      <input
        type="number"
        value={draft}
        step={item.kind === "float" ? "any" : "1"}
        onChange={(e) => onChange(e.target.value)}
        className="w-full rounded-md border border-[#2a2f3a] bg-[#0f131b] px-2 py-1 font-mono text-xs text-gray-200 focus:border-indigo-500/60 focus:outline-none"
      />
    ) : (
      <input
        type={item.secret ? "password" : "text"}
        value={draft}
        placeholder={item.kind === "str_array" ? "多个值用英文逗号分隔" : ""}
        onChange={(e) => onChange(e.target.value)}
        className="w-full rounded-md border border-[#2a2f3a] bg-[#0f131b] px-2 py-1 font-mono text-xs text-gray-200 focus:border-indigo-500/60 focus:outline-none"
      />
    );
  return (
    <div
      className={`rounded-lg border px-3 py-2.5 transition ${
        dirty ? "border-indigo-500/40 bg-indigo-500/5" : "border-transparent"
      }`}
    >
      <div className="flex items-start gap-3">
        <div className="w-40 shrink-0">
          <div className="break-all font-mono text-xs text-gray-200">
            {item.key}
            {item.commented && (
              <span className="ml-1.5 rounded bg-[#1a2030] px-1 text-[9px] text-gray-500">
                默认
              </span>
            )}
          </div>
          <div className="mt-0.5 text-[10px] text-gray-600">
            {item.current === null ? (
              <span title="未在 rclaw.toml 中显式配置，当前生效的是默认值">
                未配置 · 默认 {item.default_raw}
              </span>
            ) : (
              <span>已配置</span>
            )}
          </div>
        </div>
        <div className="min-w-0 flex-1 space-y-1.5">
          {input}
          {item.doc.length > 0 && (
            <p className="whitespace-pre-wrap text-[10px] leading-relaxed text-gray-500">
              {item.doc.join("\n")}
            </p>
          )}
        </div>
        {dirty && (
          <button
            onClick={() => onChange(baseline)}
            title="还原此项"
            className="shrink-0 rounded px-1 text-[10px] text-gray-500 transition hover:text-gray-300"
          >
            还原
          </button>
        )}
      </div>
    </div>
  );
}

function SectionCard({
  section,
  drafts,
  onDraft,
  defaultOpen,
}: {
  section: ConfigSection;
  drafts: Record<string, string>;
  onDraft: (path: string, kind: string, value: string) => void;
  defaultOpen: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className="rounded-xl border border-[#2a2f3a] bg-[#141821]">
      <button
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center justify-between px-4 py-3 text-left"
      >
        <span className="flex items-center gap-2">
          <span className="font-mono text-xs text-indigo-300">{section.header}</span>
          {section.example_only && (
            <span className="rounded bg-[#1a2030] px-1.5 py-0.5 text-[9px] text-gray-500">
              示例 · 只读
            </span>
          )}
          {!section.example_only && (
            <span className="text-[10px] text-gray-600">{section.items.length} 项</span>
          )}
        </span>
        <span className="text-xs text-gray-500">{open ? "▾" : "▸"}</span>
      </button>
      {open && (
        <div className="space-y-2 border-t border-[#2a2f3a] px-4 py-3">
          {section.example_only ? (
            <pre className="overflow-x-auto rounded-lg bg-[#0f131b] p-3 font-mono text-[10px] leading-relaxed text-gray-400">
              {section.doc.join("\n")}
            </pre>
          ) : (
            <>
              {section.doc.length > 0 && (
                <p className="whitespace-pre-wrap border-l-2 border-[#2a2f3a] pl-3 text-[10px] leading-relaxed text-gray-500">
                  {section.doc.join("\n")}
                </p>
              )}
              {section.items.map((item) => (
                <ItemRow
                  key={item.path}
                  item={item}
                  draft={drafts[item.path] ?? baselineOf(item)}
                  onChange={(v) => onDraft(item.path, item.kind, v)}
                />
              ))}
            </>
          )}
        </div>
      )}
    </div>
  );
}

/** 配置面板：rclaw.toml 全量项可视化编辑（文档来自 rclaw.toml.example，保存为
 * 注释/键序保真的 toml_edit 手术，保存前做结构校验）。另含标题栏循环标语及其
 * 轮播周期的编辑——标语属界面偏好（desktop_prefs.json），不写入 rclaw.toml。 */
/** 检查更新卡片：GitHub releases 比对当前版本；有新版给出下载入口。 */
function UpdateCard() {
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const check = useCallback(async () => {
    setChecking(true);
    setError(null);
    try {
      setInfo(await api.updateCheck());
    } catch (e) {
      setError(String(e));
    } finally {
      setChecking(false);
    }
  }, []);

  const hasNewer = info?.latest != null;

  return (
    <div className="mb-4 rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
      <h2 className="mb-1 text-sm font-medium text-gray-200">检查更新</h2>
      <p className="mb-2 text-[10px] text-gray-600">
        当前版本 {info?.current ?? "…"}；对照 GitHub 发布页的最新 Release。
        应用本身不自动下载安装——有新版时前往发布页下载（Android 安装 APK，
        桌面运行安装包）。
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <button
          onClick={() => void check()}
          disabled={checking}
          className="rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-300 transition enabled:hover:border-indigo-500/50 enabled:hover:text-indigo-300 disabled:opacity-50"
        >
          {checking ? "检查中…" : "检查更新"}
        </button>
        {hasNewer && info?.url && (
          <button
            onClick={() => void api.openUrl(info.url!)}
            className="rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500"
          >
            前往下载 v{info.latest}
          </button>
        )}
        {!checking && info && !hasNewer && !info.error && (
          <span className="text-xs text-emerald-400">已是最新版本 ✓</span>
        )}
      </div>
      {info?.error && (
        <p className="mt-2 text-xs text-amber-400">{info.error}（稍后重试）</p>
      )}
      {error && <p className="mt-2 text-xs text-red-400">{error}</p>}
      {hasNewer && info?.notes && (
        <pre className="mt-2 max-h-40 overflow-y-auto whitespace-pre-wrap rounded-lg bg-[#0f131b] p-2 text-[10px] leading-relaxed text-gray-400">
          {info.notes}
        </pre>
      )}
    </div>
  );
}

export default function ConfigPanel({
  agentReady,
  slogans,
  sloganInterval,
  onSlogansSaved,
}: {
  agentReady: boolean;
  slogans: string[];
  sloganInterval: number;
  onSlogansSaved: (prefs: SloganPrefs) => void;
}) {
  const [state, setState] = useState<ConfigStateDto | null>(null);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [showRaw, setShowRaw] = useState(false);
  // 安卓 App 专属：从手机存储导入整份 rclaw.toml 覆盖现有配置（桌面无此入口）。
  const android = isAndroidDevice();
  const fileRef = useRef<HTMLInputElement>(null);
  const [importing, setImporting] = useState(false);
  // 标语草稿（每行一条）与轮播周期（秒）；loaded 标记是否已从后端加载。
  const [sloganDraft, setSloganDraft] = useState<string>("");
  const [intervalDraft, setIntervalDraft] = useState<string>("");
  const [sloganLoaded, setSloganLoaded] = useState(false);
  const [sloganDirty, setSloganDirty] = useState(false);
  const [sloganMsg, setSloganMsg] = useState<string | null>(null);
  // 工具权限模式（desktop_prefs.json）：default = 工作区外需确认；full = 完全访问。
  const [permMode, setPermMode] = useState<PermissionMode>("default");
  const [permMsg, setPermMsg] = useState<string | null>(null);

  useEffect(() => {
    api
      .permissionModeGet()
      .then((m) => setPermMode(m))
      .catch(() => {});
  }, []);

  const changePermMode = useCallback(async (mode: PermissionMode) => {
    setPermMsg(null);
    const prev = permMode;
    setPermMode(mode); // 乐观更新，失败回滚。
    try {
      await api.permissionModeSet(mode);
      setPermMsg("已保存，对下一条消息生效");
      setTimeout(() => setPermMsg(null), 2500);
    } catch (e) {
      setPermMode(prev);
      setPermMsg(String(e));
    }
  }, [permMode]);

  useEffect(() => {
    setSloganDraft(slogans.join("\n"));
    setIntervalDraft(String(sloganInterval));
    setSloganLoaded(true);
    setSloganDirty(false);
  }, [slogans, sloganInterval]);

  const saveSlogans = useCallback(async () => {
    setSloganMsg(null);
    const interval = Number.parseInt(intervalDraft, 10);
    if (!Number.isFinite(interval) || interval < 1 || interval > 3600) {
      setSloganMsg("轮播周期须为 1-3600 的整数秒");
      return;
    }
    try {
      const next = sloganDraft.split("\n").map((s) => s.trim()).filter(Boolean);
      await api.slogansSet(next, interval);
      // 以服务端回读为准（空列表在读取侧回退默认标语，读取侧还做周期钳制）。
      const prefs = await api.slogansGet();
      onSlogansSaved(prefs);
      setSloganMsg("已保存");
      setTimeout(() => setSloganMsg(null), 2500);
    } catch (e) {
      setSloganMsg(String(e));
    }
  }, [sloganDraft, intervalDraft, onSlogansSaved]);

  const refresh = useCallback(async (opts?: { keepDrafts?: boolean }) => {
    try {
      const s = await api.configState();
      setState(s);
      // 轮询刷新保留未保存草稿：展示值 = 草稿 ?? 基线，外部改动刷新基线后，
      // 无草稿的键立即反映文件内容；保存后的刷新仍全量重置草稿。
      if (!opts?.keepDrafts) setDrafts({});
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    if (agentReady) void refresh();
  }, [agentReady, refresh]);

  // 导入整份配置文件：后端校验通过才落盘，成功后重读刷新整个面板。
  const importToml = async (file: File) => {
    setError(null);
    setImporting(true);
    try {
      await api.configImportRaw(await file.text());
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setImporting(false);
    }
  };

  // 外部编辑 rclaw.toml 的近实时同步：config_state 每次都从磁盘新读，这里仅在
  // 配置页挂载期间每 3s 拉一次（切页即卸载即停止）。
  useEffect(() => {
    if (!agentReady) return;
    const timer = setInterval(() => void refresh({ keepDrafts: true }), 3000);
    return () => clearInterval(timer);
  }, [agentReady, refresh]);

  const dirtyCount = useMemo(
    () =>
      state
        ? state.doc.sections
            .flatMap((sec) => sec.items)
            .filter((it) => (drafts[it.path] ?? baselineOf(it)) !== baselineOf(it)).length
        : 0,
    [state, drafts],
  );

  const onDraft = useCallback((path: string, _kind: string, value: string) => {
    setDrafts((prev) => ({ ...prev, [path]: value }));
  }, []);

  const save = useCallback(async () => {
    if (!state) return;
    const edits: ConfigEdit[] = state.doc.sections
      .flatMap((sec) => sec.items)
      .filter((it) => {
        const d = drafts[it.path];
        return d !== undefined && d !== baselineOf(it);
      })
      .map((it) => ({ path: it.path, kind: it.kind, value: drafts[it.path] }));
    setSaving(true);
    setError(null);
    try {
      await api.configSave(edits);
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  }, [state, drafts, refresh]);

  if (!state) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-gray-500">
        {error ?? "加载配置…"}
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      <div className="min-h-0 flex-1 overflow-y-auto p-6 max-md:p-3">
        <div className="mb-2 flex items-center justify-between">
          <h1 className="text-base font-semibold text-gray-100">配置</h1>
          <div className="flex items-center gap-3">
            {android && (
              <>
                <button
                  onClick={() => fileRef.current?.click()}
                  disabled={importing}
                  className="text-xs text-gray-500 transition hover:text-gray-300 disabled:opacity-50"
                >
                  {importing ? "导入中…" : "导入 rclaw.toml"}
                </button>
                <input
                  ref={fileRef}
                  type="file"
                  accept=".toml,text/plain,application/toml"
                  className="hidden"
                  onChange={(e) => {
                    const f = e.target.files?.[0];
                    e.target.value = "";
                    if (f) void importToml(f);
                  }}
                />
              </>
            )}
            <button
              onClick={() => setShowRaw((v) => !v)}
              className="text-xs text-gray-500 transition hover:text-gray-300"
            >
              {showRaw ? "收起原始文件" : "查看原始文件"}
            </button>
          </div>
        </div>
        <p className="mb-1 font-mono text-[10px] text-gray-600">{state.path}</p>
        <p className="mb-4 rounded-lg border border-[#2a2f3a] bg-[#141821] px-3 py-2 text-[11px] leading-relaxed text-gray-500">
          文档与默认值取自 rclaw.toml.example，逐键说明功能、取值含义与默认。
          点「保存」立即写入 rclaw.toml：注释/键序保真，先过结构与 provider 名校验，
          校验失败不落盘。运行中的 agent 在启动时加载配置，保存后需重启应用生效；
          web_search 的即时启停切换请用对话里的 /search 命令。
        </p>

        {state.doc.preamble.length > 0 && (
          <p className="mb-4 whitespace-pre-wrap text-[10px] text-gray-600">
            {state.doc.preamble.join("\n")}
          </p>
        )}

        {/* 检查更新：GitHub releases 对照 */}
        <SkillOpsCard />
        <UpdateCard />

        {/* 标题栏循环标语：界面偏好，不写 rclaw.toml */}
        <div className="mb-4 rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
          <h2 className="mb-1 text-sm font-medium text-gray-200">界面标语（标题栏循环显示）</h2>
          <p className="mb-2 text-[10px] text-gray-600">
            每行一条，窗口标题栏按「轮播周期」轮换；保存在界面偏好文件，不写入
            rclaw.toml。清空标语保存可恢复默认文案。
          </p>
          {sloganLoaded && (
            <>
              <textarea
                value={sloganDraft}
                onChange={(e) => {
                  setSloganDraft(e.target.value);
                  setSloganDirty(true);
                }}
                rows={5}
                spellCheck={false}
                className="w-full rounded-md border border-[#2a2f3a] bg-[#0f131b] px-2 py-1.5 font-mono text-xs leading-relaxed text-gray-200 focus:border-indigo-500/60 focus:outline-none"
              />
              <label className="mt-2 flex items-center gap-2 text-xs text-gray-400">
                轮播周期
                <input
                  type="number"
                  min={1}
                  max={3600}
                  value={intervalDraft}
                  onChange={(e) => {
                    setIntervalDraft(e.target.value);
                    setSloganDirty(true);
                  }}
                  className="w-20 rounded-md border border-[#2a2f3a] bg-[#0f131b] px-2 py-1 font-mono text-xs text-gray-200 focus:border-indigo-500/60 focus:outline-none"
                />
                秒（1-3600）
              </label>
              <div className="mt-2 flex items-center gap-3">
                <button
                  onClick={() => void saveSlogans()}
                  disabled={!sloganDirty}
                  className="rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500 disabled:opacity-40"
                >
                  保存标语
                </button>
                {sloganDirty && (
                  <button
                    onClick={() => {
                      setSloganDraft(slogans.join("\n"));
                      setIntervalDraft(String(sloganInterval));
                      setSloganDirty(false);
                      setSloganMsg(null);
                    }}
                    className="rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-400 transition hover:text-gray-200"
                  >
                    还原
                  </button>
                )}
                {sloganMsg && (
                  <span className={sloganMsg === "已保存" ? "text-xs text-emerald-400" : "text-xs text-red-400"}>
                    {sloganMsg}
                  </span>
                )}
              </div>
            </>
          )}
        </div>

        {/* 工具权限模式：界面偏好，不写 rclaw.toml */}
        <div className="mb-4 rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
          <h2 className="mb-1 text-sm font-medium text-gray-200">工具权限模式</h2>
          <p className="mb-3 text-[10px] text-gray-600">
            控制 agent 的文件 / 命令工具在工作区（agent/ 目录）之外读写时的授权方式。
          </p>
          <div className="space-y-2">
            {(
              [
                {
                  value: "default" as PermissionMode,
                  title: "默认权限（推荐）",
                  desc: "文件与命令工具仅限工作区内；访问工作区之外的目录 / 文件时弹窗询问，需你确认授权。",
                },
                {
                  value: "full" as PermissionMode,
                  title: "完全访问",
                  desc: "agent 可读写电脑上任意目录与文档，无需确认。请仅在信任任务来源时开启。",
                },
              ] as const
            ).map((opt) => (
              <button
                key={opt.value}
                onClick={() => void changePermMode(opt.value)}
                className={`flex w-full items-start gap-2.5 rounded-lg border px-3 py-2.5 text-left transition ${
                  permMode === opt.value
                    ? "border-indigo-500/60 bg-indigo-500/10"
                    : "border-[#2a2f3a] hover:border-[#3a4150]"
                }`}
              >
                <span
                  className={`mt-0.5 flex h-3.5 w-3.5 shrink-0 items-center justify-center rounded-full border ${
                    permMode === opt.value ? "border-indigo-400" : "border-gray-600"
                  }`}
                >
                  {permMode === opt.value && (
                    <span className="h-1.5 w-1.5 rounded-full bg-indigo-400" />
                  )}
                </span>
                <span className="min-w-0">
                  <span className="block text-xs font-medium text-gray-200">{opt.title}</span>
                  <span className="mt-0.5 block text-[10px] leading-relaxed text-gray-500">
                    {opt.desc}
                  </span>
                </span>
              </button>
            ))}
          </div>
          {permMsg && (
            <p
              className={`mt-2 text-[10px] ${
                permMsg.startsWith("已保存") ? "text-emerald-400" : "text-red-400"
              }`}
            >
              {permMsg}
            </p>
          )}
        </div>

        {showRaw && (
          <pre className="mb-4 max-h-72 overflow-auto rounded-xl border border-[#2a2f3a] bg-[#0f131b] p-3 font-mono text-[10px] leading-relaxed text-gray-400">
            {state.raw || "（rclaw.toml 不存在或为空）"}
          </pre>
        )}

        <div className="space-y-3">
          {state.doc.sections.map((sec) => (
            <SectionCard
              key={sec.header + sec.path}
              section={sec}
              drafts={drafts}
              onDraft={onDraft}
              defaultOpen={!sec.example_only}
            />
          ))}
        </div>
      </div>

      {dirtyCount > 0 && (
        <div className="flex items-center gap-3 border-t border-[#2a2f3a] bg-[#12151d] px-6 py-3">
          <span className="text-xs text-gray-400">{dirtyCount} 项待保存</span>
          <button
            onClick={() => void save()}
            disabled={saving}
            className="rounded-lg bg-indigo-600 px-4 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500 disabled:opacity-50"
          >
            {saving ? "保存中…" : "保存到 rclaw.toml"}
          </button>
          <button
            onClick={() => setDrafts({})}
            className="rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-400 transition hover:text-gray-200"
          >
            放弃修改
          </button>
          {error && <span className="min-w-0 flex-1 truncate text-xs text-red-400" title={error}>{error}</span>}
        </div>
      )}
      {dirtyCount === 0 && error && (
        <div className="border-t border-[#2a2f3a] bg-[#12151d] px-6 py-3 text-xs text-red-400">
          {error}
        </div>
      )}
    </div>
  );
}
