import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../lib/api";
import { isAndroidDevice, useIsMobile } from "../lib/useIsMobile";
import { useBackClose } from "../lib/backNav";

/** 工作区文件面板：安卓上 agent 目录（应用私有存储）文件管理器无法访问，
 *  这里提供应用内浏览 + 文本编辑（媒体类经系统应用打开 / 保存分享）。 */

interface Entry {
  name: string;
  is_dir: boolean;
  size: number;
  modified_ms: number;
  /** 主配置虚拟条目（rclaw.toml，实体在 home 根，保存走配置校验）。 */
  is_config?: boolean;
}

/** 可在应用内编辑的文本扩展名（与后端白名单同源；无扩展名由后端探测）。 */
const TEXT_EXTS = [
  "md", "txt", "json", "jsonl", "toml", "yaml", "yml", "py", "rs", "ts", "tsx", "js",
  "jsx", "css", "html", "xml", "csv", "log", "sh", "bat", "ps1", "ini", "cfg", "conf",
  "env", "gitignore", "prompt", "skill", "lock",
];

/** 图片/视频/音频等媒体：交给系统应用打开（open_media）。 */
const MEDIA_EXTS = [
  "png", "jpg", "jpeg", "webp", "gif", "bmp", "mp4", "webm", "m4v", "mov", "m4a",
  "aac", "wav", "mp3", "ogg", "opus", "flac", "pdf",
];

function extOf(name: string): string {
  const i = name.lastIndexOf(".");
  return i === -1 ? "" : name.slice(i + 1).toLowerCase();
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/** 相对 agent/ 的目录路径拼接。 */
function joinPath(dir: string, name: string): string {
  return dir ? `${dir}/${name}` : name;
}

/** 行图标：主配置 ⚙️，目录 📁，普通文件 📄。 */
function entryIcon(entry: Entry): string {
  if (entry.is_config) return "⚙️";
  return entry.is_dir ? "📁" : "📄";
}

export default function FilePanel({ onError }: { onError?: (m: string) => void }) {
  const [dir, setDir] = useState("");
  const [entries, setEntries] = useState<Entry[] | null>(null);
  const [loading, setLoading] = useState(false);
  // 编辑器载荷（content 为当前文本；dirty 标记未保存修改）。
  const [editor, setEditor] = useState<{ path: string; content: string; dirty: boolean } | null>(
    null,
  );
  const [saving, setSaving] = useState(false);
  // 新建文件行的输入态。
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  // 脏文件关闭二次确认（WebView 无原生 confirm）。
  const [confirmClose, setConfirmClose] = useState(false);
  // 行操作菜单（长按 450ms / 右键）：menu 记录目标与坐标。
  const [menu, setMenu] = useState<{ entry: Entry; x: number; y: number } | null>(null);
  // 长按打开后抬指的合成 click 会落在遮罩上把菜单瞬间关掉：一发式吞吐
  // （与 Chat 消息菜单同款），1 秒兜底复位。
  const menuClickGuard = useRef(false);
  const menuGuardTimer = useRef<number | undefined>(undefined);
  // 删除二次确认：待确认的目标路径（null = 无）。
  const [confirmDel, setConfirmDel] = useState<string | null>(null);
  // 新建文件夹行的输入态。
  const [creatingFolder, setCreatingFolder] = useState(false);
  const [newFolderName, setNewFolderName] = useState("");
  // 重命名行的输入态（path = 原路径，value = 当前输入）。
  const [renaming, setRenaming] = useState<{ path: string; value: string } | null>(null);
  // 操作成功反馈（小气泡，非错误通道）。
  const [notice, setNotice] = useState<string | null>(null);
  const noticeTimer = useRef<number | undefined>(undefined);
  const flashNotice = (text: string) => {
    setNotice(text);
    window.clearTimeout(noticeTimer.current);
    noticeTimer.current = window.setTimeout(() => setNotice(null), 2000);
  };
  const isMobile = useIsMobile();
  const pressTimer = useRef<number | null>(null);
  const suppressClickRef = useRef(false);
  // 系统返回键关行操作菜单 / 删除确认 / 全页编辑器 / 重命名与新建行（弹层栈
  // 后进先出：编辑器是覆盖整页的 fixed 层，不注册的话返回键会穿透到侧栏呼出）。
  useBackClose(menu !== null, () => setMenu(null));
  useBackClose(confirmDel !== null, () => setConfirmDel(null));
  useBackClose(editor !== null, () => {
    if (confirmClose) {
      setConfirmClose(false); // 最上层是「放弃修改」确认条：返回 = 继续编辑
      return;
    }
    if (editor?.dirty) {
      setConfirmClose(true); // 脏文件先出确认，与「关闭」按钮同一语义
      return;
    }
    setEditor(null);
  });
  useBackClose(renaming !== null, () => setRenaming(null));
  useBackClose(creating || creatingFolder, () => {
    setCreating(false);
    setCreatingFolder(false);
    setNewName("");
    setNewFolderName("");
  });

  const refresh = useCallback(
    (target: string) => {
      setLoading(true);
      api
        .workspaceList(target)
        .then(setEntries)
        .catch((e) => {
          setEntries([]);
          onError?.(String(e));
        })
        .finally(() => setLoading(false));
    },
    [onError],
  );

  useEffect(() => {
    refresh(dir);
  }, [dir, refresh]);

  const openEntry = (entry: Entry) => {
    if (entry.is_dir) {
      setDir(joinPath(dir, entry.name));
      return;
    }
    const ext = extOf(entry.name);
    const path = joinPath(dir, entry.name);
    if (MEDIA_EXTS.includes(ext)) {
      void api.openMedia(path).catch((e) => onError?.(`打开失败: ${String(e)}`));
      return;
    }
    if (!TEXT_EXTS.includes(ext) && ext !== "") {
      onError?.("该文件类型不支持应用内编辑（媒体可打开，其余请导出后处理）");
      return;
    }
    api
      .workspaceRead(path)
      .then((content) => setEditor({ path, content, dirty: false }))
      .catch((e) => onError?.(String(e)));
  };

  const saveEditor = () => {
    if (!editor) return;
    setSaving(true);
    api
      .workspaceWrite(editor.path, editor.content)
      .then(() => {
        setEditor((prev) => (prev ? { ...prev, dirty: false } : prev));
        // 主配置是冷配置：文件已更新，但运行中的 agent 要重启才按新值装配。
        if (editor.path === "rclaw.toml") {
          flashNotice("✓ 已保存主配置（重启应用后生效）");
        }
        refresh(dir);
      })
      .catch((e) => onError?.(String(e)))
      .finally(() => setSaving(false));
  };

  const createFile = () => {
    const name = newName.trim();
    setCreating(false);
    if (!name || name.includes("/") || name.includes("\\")) {
      if (name) onError?.("文件名不能包含路径分隔符");
      setNewName("");
      return;
    }
    const path = joinPath(dir, name);
    setNewName("");
    setEditor({ path, content: "", dirty: true });
  };

  // ---- 行长按（450ms）/ 右键操作菜单 ----

  // 长按起点：move 位移超 12px（touch slop）才取消——同坐标长按与手指微抖都
  // 会持续派发 touchmove，无条件取消会让长按永远不触发。
  const pressOrigin = useRef<{ x: number; y: number } | null>(null);

  const cancelPress = () => {
    pressOrigin.current = null;
    if (pressTimer.current !== null) {
      window.clearTimeout(pressTimer.current);
      pressTimer.current = null;
    }
  };

  const startPress = (entry: Entry, x: number, y: number) => {
    cancelPress();
    pressOrigin.current = { x, y };
    pressTimer.current = window.setTimeout(() => {
      pressTimer.current = null;
      suppressClickRef.current = true;
      window.setTimeout(() => (suppressClickRef.current = false), 600);
      menuClickGuard.current = true;
      window.clearTimeout(menuGuardTimer.current);
      menuGuardTimer.current = window.setTimeout(() => {
        menuClickGuard.current = false;
      }, 1000);
      setMenu({ entry, x, y });
      setConfirmDel(null);
      if (navigator.vibrate) navigator.vibrate(15);
    }, 450);
  };

  const movePress = (x: number, y: number) => {
    const o = pressOrigin.current;
    if (o && Math.hypot(x - o.x, y - o.y) > 12) cancelPress();
  };

  // 卸载清长按定时器。
  useEffect(() => cancelPress, []);

  // 切目录即收起菜单、删除确认与重命名行。
  useEffect(() => {
    setMenu(null);
    setConfirmDel(null);
    setRenaming(null);
  }, [dir]);

  const copyEntry = (entry: Entry) => {
    api
      .workspaceCopyFile(joinPath(dir, entry.name))
      .then(() => refresh(dir))
      .catch((e) => onError?.(String(e)));
  };

  const deleteEntry = (entry: Entry) => {
    api
      .workspaceDelete(joinPath(dir, entry.name))
      .then(() => {
        setMenu(null);
        refresh(dir);
      })
      .catch((e) => onError?.(String(e)));
  };

  const createFolder = () => {
    const name = newFolderName.trim();
    setCreatingFolder(false);
    setNewFolderName("");
    if (!name || name.includes("/") || name.includes("\\")) {
      if (name) onError?.("文件夹名不能包含路径分隔符");
      return;
    }
    api
      .workspaceNewFolder(dir, name)
      .then(() => refresh(dir))
      .catch((e) => onError?.(String(e)));
  };

  const applyRename = () => {
    if (!renaming) return;
    const value = renaming.value.trim();
    const path = renaming.path;
    const original = path.split("/").pop() ?? "";
    setRenaming(null);
    // 未改名（或清空）直接关闭，不惊动后端（后端对同名会报「已存在」）。
    if (!value || value === original) return;
    if (value.includes("/") || value.includes("\\")) {
      onError?.("名称不能包含路径分隔符");
      return;
    }
    api
      .workspaceRename(path, value)
      .then(() => refresh(dir))
      .catch((e) => onError?.(String(e)));
  };

  // 读系统剪贴板落盘到当前目录（安卓）：文本→.txt / 文件管理器复制的文件。
  const pasteFromClipboard = () => {
    api
      .workspacePasteFromClipboard(dir)
      .then((r) => {
        const name = r.startsWith("ok:") ? r.slice(3) : "剪贴板内容";
        flashNotice(`✓ 已粘贴 ${name}`);
        refresh(dir);
      })
      .catch((e) => onError?.(String(e)));
  };

  // 文件以 FileProvider URI 写系统剪贴板（文件管理器可粘贴；微信/QQ 请用转发）。
  const copyEntryToClipboard = (entry: Entry) => {
    api
      .workspaceCopyToClipboard(joinPath(dir, entry.name))
      .then(() => flashNotice("✓ 已复制到系统剪贴板"))
      .catch((e) => onError?.(String(e)));
  };

  const saveEntryToLocal = (entry: Entry) => {
    api
      .saveMedia(joinPath(dir, entry.name))
      .then((r) => {
        if (r === "permission_requested") {
          onError?.("请在系统授权弹窗允许后重试保存");
        } else {
          flashNotice("✓ 已保存到本地");
        }
      })
      .catch((e) => onError?.(`保存失败: ${String(e)}`));
  };

  const shareEntry = (entry: Entry) => {
    api.shareMedia(joinPath(dir, entry.name)).catch((e) => onError?.(`转发失败: ${String(e)}`));
  };

  // 面包屑：agent/ + 各级目录可点。
  const segments = dir ? dir.split("/") : [];

  return (
    <div className="flex h-full flex-col">
      {/* 顶部：面包屑 + 刷新/新建。 */}
      <div className="flex items-center gap-1 border-b border-[#2a2f3a] px-4 py-2.5 max-md:px-3">
        <button
          onClick={() => setDir("")}
          className={`rounded px-1.5 py-0.5 font-mono text-xs transition hover:text-indigo-300 ${
            dir ? "text-gray-400" : "text-indigo-300"
          }`}
        >
          agent/
        </button>
        {segments.map((seg, i) => (
          <span key={i} className="flex items-center">
            <button
              onClick={() => setDir(segments.slice(0, i + 1).join("/"))}
              className={`rounded px-1 py-0.5 font-mono text-xs transition hover:text-indigo-300 ${
                i === segments.length - 1 ? "text-indigo-300" : "text-gray-400"
              }`}
            >
              {seg}/
            </button>
          </span>
        ))}
        <div className="ml-auto flex items-center gap-1">
          {isAndroidDevice() && (
            <button
              onClick={pasteFromClipboard}
              title="粘贴系统剪贴板（文本/文件管理器复制的文件；微信/QQ 内容请用分享到 RClaw）"
              className="rounded-md px-2 py-1 text-xs text-gray-400 transition hover:bg-[#1a1f29] hover:text-gray-200"
            >
              粘贴
            </button>
          )}
          <button
            onClick={() => setCreatingFolder(true)}
            title="新建文件夹"
            className="rounded-md px-2 py-1 text-xs text-gray-400 transition hover:bg-[#1a1f29] hover:text-gray-200"
          >
            ＋夹
          </button>
          <button
            onClick={() => setCreating(true)}
            title="新建文本文件"
            className="rounded-md px-2 py-1 text-xs text-gray-400 transition hover:bg-[#1a1f29] hover:text-gray-200"
          >
            ＋文件
          </button>
          <button
            onClick={() => refresh(dir)}
            title="刷新"
            className="rounded-md px-2 py-1 text-xs text-gray-400 transition hover:bg-[#1a1f29] hover:text-gray-200"
          >
            ⟳
          </button>
        </div>
      </div>

      {/* 目录列表。 */}
      <div className="min-h-0 flex-1 overflow-y-auto px-2 py-2">
        {loading && entries === null && (
          <div className="px-2 py-1 text-xs text-gray-600">读取中…</div>
        )}
        {dir && (
          <button
            onClick={() => setDir(segments.slice(0, -1).join("/"))}
            className="flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left text-xs text-gray-500 transition hover:bg-[#1a1f29] hover:text-gray-300"
          >
            📁 ..
          </button>
        )}
        {creating && (
          <div className="flex items-center gap-2 px-2 py-1.5">
            <input
              autoFocus
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") createFile();
                else if (e.key === "Escape") {
                  setCreating(false);
                  setNewName("");
                }
              }}
              placeholder="文件名.md"
              maxLength={80}
              className="min-w-0 flex-1 rounded-md border border-indigo-500/50 bg-[#161a22] px-2 py-1 text-xs text-gray-200 outline-none"
            />
            <button onClick={createFile} className="text-xs text-indigo-300">
              创建
            </button>
            <button
              onClick={() => {
                setCreating(false);
                setNewName("");
              }}
              className="text-xs text-gray-500"
            >
              取消
            </button>
          </div>
        )}
        {creatingFolder && (
          <div className="flex items-center gap-2 px-2 py-1.5">
            <input
              autoFocus
              value={newFolderName}
              onChange={(e) => setNewFolderName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") createFolder();
                else if (e.key === "Escape") {
                  setCreatingFolder(false);
                  setNewFolderName("");
                }
              }}
              placeholder="文件夹名"
              maxLength={80}
              className="min-w-0 flex-1 rounded-md border border-indigo-500/50 bg-[#161a22] px-2 py-1 text-xs text-gray-200 outline-none"
            />
            <button onClick={createFolder} className="text-xs text-indigo-300">
              创建
            </button>
            <button
              onClick={() => {
                setCreatingFolder(false);
                setNewFolderName("");
              }}
              className="text-xs text-gray-500"
            >
              取消
            </button>
          </div>
        )}
        {entries?.length === 0 && !loading && !creating && !creatingFolder && !renaming && (
          <div className="px-2 py-1 text-xs text-gray-600">空目录</div>
        )}
        {entries?.map((entry) =>
          // 原位重命名：输入框直接替换被长按的那一行（顶部行在长列表里会
          // 落在屏幕外，看起来像「点了没反应」）。
          renaming && renaming.path === joinPath(dir, entry.name) ? (
            <div key={entry.name} className="flex items-center gap-2 rounded-lg px-2 py-1.5">
              <span className="shrink-0 text-sm">{entry.is_dir ? "📁" : "📄"}</span>
              <input
                autoFocus
                value={renaming.value}
                onChange={(e) => setRenaming({ ...renaming, value: e.target.value })}
                onKeyDown={(e) => {
                  if (e.key === "Enter") applyRename();
                  else if (e.key === "Escape") setRenaming(null);
                }}
                maxLength={120}
                className="min-w-0 flex-1 rounded-md border border-amber-500/50 bg-[#161a22] px-2 py-1 text-xs text-gray-200 outline-none"
              />
              <button onClick={applyRename} className="shrink-0 text-xs text-amber-300">
                确定
              </button>
              <button
                onClick={() => setRenaming(null)}
                className="shrink-0 text-xs text-gray-500"
              >
                取消
              </button>
            </div>
          ) : (
            <button
              key={entry.name}
              onClick={() => {
                if (suppressClickRef.current) {
                  suppressClickRef.current = false;
                  return;
                }
                openEntry(entry);
              }}
              onTouchStart={(e) => {
                if (!isMobile || e.touches.length === 0) return;
                startPress(entry, e.touches[0].clientX, e.touches[0].clientY);
              }}
              onTouchEnd={cancelPress}
              onTouchMove={(e) => {
                if (e.touches[0]) movePress(e.touches[0].clientX, e.touches[0].clientY);
              }}
              onTouchCancel={cancelPress}
              onContextMenu={(e) => {
                e.preventDefault();
                setMenu({ entry, x: e.clientX, y: e.clientY });
                setConfirmDel(null);
              }}
              className="flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left transition hover:bg-[#1a1f29]"
              title={
                entry.is_config
                  ? "主配置 rclaw.toml（保存即校验；重启应用后生效）"
                  : joinPath(dir, entry.name)
              }
            >
              <span className="shrink-0 text-sm">{entryIcon(entry)}</span>
              <span
                className={`min-w-0 flex-1 truncate text-xs ${
                  entry.is_config ? "text-indigo-300" : "text-gray-300"
                }`}
              >
                {entry.name}
                {entry.is_config && (
                  <span className="ml-1.5 text-[10px] text-gray-500">主配置</span>
                )}
              </span>
              {!entry.is_dir && (
                <span className="shrink-0 font-mono text-[10px] text-gray-600">
                  {formatSize(entry.size)}
                </span>
              )}
            </button>
          ),
        )}
        <p className="mt-3 px-2 text-[10px] leading-relaxed text-gray-600">
          agent 工作区（会话/记忆/技能/产物）都在这里；顶部的 ⚙️ rclaw.toml
          是主配置（与「配置」栏目同一文件，保存带校验，重启应用后生效）。{isAndroidDevice()
            ? "安卓上该目录是应用私有存储，文件管理器无法访问；文本可直接编辑，媒体点击用系统应用打开。"
            : ""}
          长按（或右键）可重命名/复制/删除；安卓「粘贴」收文本与文件管理器
          复制的文件（微信/QQ 内容请用「分享到 RClaw」导入）。
        </p>
      </div>

      {/* 行操作菜单：长按 / 右键唤起；删除走两次点击确认。 */}
      {menu && (
        <>
          <div
            className="fixed inset-0 z-50"
            onClick={() => {
              // 长按打开后抬指的合成 click 吞掉，防菜单瞬间自关。
              if (menuClickGuard.current) {
                menuClickGuard.current = false;
                return;
              }
              setMenu(null);
              setConfirmDel(null);
            }}
            onContextMenu={(e) => {
              e.preventDefault();
              // 长按打开菜单后 WebView 补发的 contextmenu 会命中遮罩：
              // 与抬指合成 click 同理由 guard 吞掉，防菜单瞬间自关。
              if (menuClickGuard.current) return;
              setMenu(null);
              setConfirmDel(null);
            }}
          />
          <div
            className="fixed z-50 w-44 overflow-hidden rounded-lg border border-[#2a2f3a] bg-[#141821] py-1 shadow-2xl"
            style={{
              left: Math.max(8, Math.min(menu.x, window.innerWidth - 190)),
              top: Math.max(8, Math.min(menu.y, window.innerHeight - 300)),
            }}
          >
            <div
              className="truncate px-3 py-1 text-[10px] text-gray-600"
              title={menu.entry.name}
            >
              {entryIcon(menu.entry)} {menu.entry.name}
            </div>
            <button
              onClick={() => {
                const target = menu.entry;
                setMenu(null);
                openEntry(target);
              }}
              className="block w-full px-3 py-1.5 text-left text-xs text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
            >
              {menu.entry.is_dir ? "进入目录" : "打开"}
            </button>
            {/* 主配置虚拟条目：不支持重命名/删除（实体是 home 根的 rclaw.toml）。 */}
            {!menu.entry.is_config && (
              <button
                onClick={() => {
                  const target = menu.entry;
                  setMenu(null);
                  setRenaming({
                    path: joinPath(dir, target.name),
                    value: target.name,
                  });
                }}
                className="block w-full px-3 py-1.5 text-left text-xs text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
              >
                重命名
              </button>
            )}
            {!menu.entry.is_dir && (
              <>
                <button
                  onClick={() => {
                    const target = menu.entry;
                    setMenu(null);
                    copyEntry(target);
                  }}
                  className="block w-full px-3 py-1.5 text-left text-xs text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
                >
                  复制副本
                </button>
                {isAndroidDevice() && (
                  <>
                    <button
                      onClick={() => {
                        const target = menu.entry;
                        setMenu(null);
                        copyEntryToClipboard(target);
                      }}
                      className="block w-full px-3 py-1.5 text-left text-xs text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
                    >
                      复制到剪贴板
                    </button>
                    <button
                      onClick={() => {
                        const target = menu.entry;
                        setMenu(null);
                        saveEntryToLocal(target);
                      }}
                      className="block w-full px-3 py-1.5 text-left text-xs text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
                    >
                      保存到本地
                    </button>
                    {/* 主配置不提供「转发」：Java 侧 shareFile 硬编码 agent/ 前缀。 */}
                    {!menu.entry.is_config && (
                      <button
                        onClick={() => {
                          const target = menu.entry;
                          setMenu(null);
                          shareEntry(target);
                        }}
                        className="block w-full px-3 py-1.5 text-left text-xs text-gray-300 transition hover:bg-[#1a1f29] hover:text-gray-100"
                      >
                        转发到其它应用
                      </button>
                    )}
                  </>
                )}
              </>
            )}
            {/* 主配置虚拟条目不可删除（后端同样拦截，这里不显示入口）。 */}
            {!menu.entry.is_config && (
              <button
                onClick={() => {
                  const path = joinPath(dir, menu.entry.name);
                  if (confirmDel !== path) {
                    setConfirmDel(path);
                    return;
                  }
                  deleteEntry(menu.entry);
                }}
                className={`block w-full px-3 py-1.5 text-left text-xs transition hover:bg-[#1a1f29] ${
                  confirmDel === joinPath(dir, menu.entry.name)
                    ? "font-medium text-red-400"
                    : "text-red-400/90"
                }`}
              >
                {confirmDel === joinPath(dir, menu.entry.name)
                  ? "确认删除？"
                  : menu.entry.is_dir
                    ? "删除（含全部内容）"
                    : "删除"}
              </button>
            )}
          </div>
        </>
      )}

      {/* 操作成功反馈小气泡。 */}
      {notice && (
        <div className="fixed bottom-24 left-1/2 z-50 -translate-x-1/2 rounded-full border border-[#2a2f3a] bg-[#141821] px-4 py-1.5 text-xs text-gray-300 shadow-xl">
          {notice}
        </div>
      )}

      {/* 文本编辑器（覆盖整页）。 */}
      {editor && (
        <div className="fixed inset-0 z-50 flex flex-col bg-[#0f1117]">
          <div
            className="flex items-center gap-2 border-b border-[#2a2f3a] px-3 py-2"
            style={{ paddingTop: "calc(env(safe-area-inset-top) + 0.5rem)" }}
          >
            <span
              className="min-w-0 flex-1 truncate font-mono text-xs text-gray-300"
              title={editor.path}
            >
              {editor.path}
              {editor.dirty && <span className="ml-1 text-amber-400">●未保存</span>}
            </span>
            <button
              onClick={saveEditor}
              disabled={saving}
              className="rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500 disabled:opacity-40"
            >
              保存
            </button>
            <button
              onClick={() => {
                if (!editor.dirty) {
                  setEditor(null);
                  return;
                }
                // WebView 无原生 confirm：脏文件走二次点击确认。
                setConfirmClose((v) => !v);
              }}
              className="rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-300 transition hover:text-gray-100"
            >
              {confirmClose && editor.dirty ? "确认放弃修改？" : "关闭"}
            </button>
          </div>
          {confirmClose && editor.dirty && (
            <div className="flex items-center justify-end gap-2 border-b border-[#2a2f3a] bg-[#141821] px-3 py-1.5 text-xs">
              <span className="mr-auto text-gray-500">有未保存的修改</span>
              <button
                onClick={() => {
                  setConfirmClose(false);
                  setEditor(null);
                }}
                className="rounded-lg bg-red-600 px-3 py-1 text-xs font-medium text-white transition hover:bg-red-500"
              >
                放弃修改
              </button>
              <button
                onClick={() => setConfirmClose(false)}
                className="rounded-lg border border-[#2a2f3a] px-3 py-1 text-xs text-gray-300"
              >
                继续编辑
              </button>
            </div>
          )}
          <textarea
            value={editor.content}
            onChange={(e) =>
              setEditor((prev) =>
                prev ? { ...prev, content: e.target.value, dirty: true } : prev,
              )
            }
            spellCheck={false}
            className="min-h-0 flex-1 resize-none bg-transparent px-4 py-3 font-mono text-xs leading-relaxed text-gray-200 outline-none"
          />
        </div>
      )}
    </div>
  );
}
