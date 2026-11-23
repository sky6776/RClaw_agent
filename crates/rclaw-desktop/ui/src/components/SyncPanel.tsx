import { useCallback, useEffect, useRef, useState } from "react";
import { api, type SyncBacklog, type SyncState } from "../lib/api";
import {
  SYNC_BTN,
  SYNC_BTN_DANGER,
  SYNC_BTN_PRIMARY,
  SYNC_BTN_SMALL,
  SYNC_CARD,
  SYNC_ERROR_BOX,
  SYNC_HINT,
  SYNC_INFO_BOX,
  SYNC_WARN_BOX,
  clearClipboard,
  formatAgo,
  formatBytes,
  syncStateLabel,
} from "../lib/syncUi";
import { SyncCardHead, SyncConfirm } from "./SyncBits";
import SyncDevices from "./SyncDevices";
import SyncPending from "./SyncPending";
import SyncSettings from "./SyncSettings";
import SyncSetup from "./SyncSetup";
import SyncTrash from "./SyncTrash";

/** 事件之外的兜底刷新间隔（面板开着、窗口可见时）。 */
const POLL_MS = 5000;

type Notice = { text: string; clipboard: boolean; error?: boolean };
type Danger = "off" | "leave" | "reset";

// ---- 面板上方的提示 ----
//
// 提示记在模块里，不只记在组件的状态里。创建 / 加入同步空间要等网络，用户在这期间切到
// 别的页面的话面板已经卸载：做完之后的提示（尤其是「请清空剪贴板」）、没做成的原因就没处
// 显示了。记在这里，下次打开面板时照样显示，直到用户点掉。这里只有给人看的话，没有配对码。
let parkedNotice: Notice | null = null;
let noticeSink: ((notice: Notice | null) => void) | null = null;

function postNotice(notice: Notice | null) {
  parkedNotice = notice;
  if (noticeSink) noticeSink(notice);
}

const DANGER_TEXT: Record<Danger, { title: string; confirm: string; body: string[] }> = {
  off: {
    title: "关闭多端同步",
    confirm: "关闭",
    body: [
      "这台设备停止同步。同步空间的密钥和设置都留着，随时可以再打开，打开后接着同步。",
      "关着的时候，这台设备上的新内容不会传出去，别的设备上的新内容也不会过来。",
    ],
  },
  leave: {
    title: "退出同步空间",
    confirm: "退出",
    body: [
      "从这台设备上清掉同步空间的密钥和存储密码，这台设备不再同步。",
      "会话、记忆等内容原样留在这台设备上；别的设备和存储上的数据不受影响。以后要回来，需要重新用配对码加入。",
    ],
  },
  reset: {
    title: "重置同步空间",
    confirm: "重置",
    body: [
      "设备丢失之后用。会换一个新的同步空间和新的密钥，并从这台设备把全部内容重新上传；旧空间作废。",
      "其余设备会停止同步，要用这台设备上新的配对码重新加入。丢失的设备还拿着旧密钥和存储密码：请同时修改存储账号的密码。",
    ],
  },
};

/**
 * 「多端同步」面板（docs/multi-device-sync.md §4.2、§11.2）。
 *
 * 状态以后端为准：收到 `sync` 事件、做完任何操作之后都重新读一次，另有一个慢轮询兜底。
 */
export default function SyncPanel({
  agentReady,
  captureBusy,
  onError,
  onOpenChat,
}: {
  agentReady: boolean;
  /** 会话那边正在等一次拍摄的结果（与拍照扫码共用采集通道）。 */
  captureBusy: boolean;
  onError: (message: string) => void;
  onOpenChat: (chatId: string) => void;
}) {
  const [data, setData] = useState<SyncState | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [nowBusy, setNowBusy] = useState(false);
  const [notice, showNotice] = useState<Notice | null>(parkedNotice);
  const [danger, setDanger] = useState<Danger | null>(null);
  const [trashTick, setTrashTick] = useState(0);
  const seq = useRef(0);

  // 提示经 `postNotice` 走：先记进模块，再交给此刻挂着的面板（见上面的说明）。
  const setNotice = postNotice;
  useEffect(() => {
    noticeSink = showNotice;
    // 从取初值到订阅上之间到的提示。
    showNotice(parkedNotice);
    return () => {
      if (noticeSink === showNotice) noticeSink = null;
    };
  }, []);

  const load = useCallback(async () => {
    const mine = ++seq.current;
    try {
      const next = await api.syncStatus();
      if (mine !== seq.current) return;
      setData(next);
      setLoadError(null);
    } catch (e) {
      if (mine !== seq.current) return;
      setLoadError(String(e));
    }
  }, []);

  useEffect(() => {
    if (!agentReady) return;
    void load();
    let timer = 0;
    const un = api.onSync((event) => {
      if (event.kind === "status") {
        window.clearTimeout(timer);
        timer = window.setTimeout(() => void load(), 150);
      } else if (event.kind === "sessions" || (event.kind === "reload" && event.what === "memory")) {
        // 同步删了会话、删了记忆条目：回收站里可能多了东西。
        setTrashTick((n) => n + 1);
      }
    });
    const poll = window.setInterval(() => {
      if (document.visibilityState === "visible") void load();
    }, POLL_MS);
    return () => {
      window.clearTimeout(timer);
      window.clearInterval(poll);
      void un.then((fn) => fn());
    };
  }, [agentReady, load]);

  /** 做一件事，然后重新读状态；出错弹提示。返回成没成。 */
  const act = async (key: string, run: () => Promise<unknown>): Promise<boolean> => {
    setBusy(key);
    try {
      await run();
      return true;
    } catch (e) {
      onError(String(e));
      return false;
    } finally {
      setBusy(null);
      void load();
    }
  };

  /** 「立即同步」要等这一轮跑完，首次同步可能很久：它不占着别的按钮。 */
  const syncNow = async () => {
    setNowBusy(true);
    try {
      await api.syncNow();
    } catch (e) {
      onError(String(e));
    } finally {
      setNowBusy(false);
      void load();
    }
  };

  const wipeClipboard = async () => {
    try {
      await clearClipboard();
      setNotice(null);
    } catch (e) {
      onError(`清空剪贴板失败：${String(e)}`);
    }
  };

  const header = (
    <div className="mb-4 flex items-center justify-between">
      <h1 className="text-base font-semibold text-gray-100">多端同步</h1>
    </div>
  );

  if (!agentReady || !data) {
    return (
      <div className="h-full overflow-y-auto p-6 max-md:p-3">
        {header}
        {loadError ? (
          <div className={SYNC_ERROR_BOX}>{loadError}</div>
        ) : (
          <div className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-sm text-gray-500">
            {agentReady ? "正在读取…" : "agent 启动后可用。"}
          </div>
        )}
      </div>
    );
  }

  const { status, config } = data;
  const state = status.state;
  const label = syncStateLabel(state);
  const inSpace = status.space_id !== null;
  // 引擎连着一个同步空间（哪怕此刻暂停、离线或出错）：显示同步的全部内容。
  const active = inSpace && state !== "off" && state !== "standby" && state !== "setup";

  const confirmDanger = async () => {
    if (!danger) return;
    const kind = danger;
    const ok = await act(kind, () => {
      if (kind === "off") return api.syncSaveConfig({ ...config, enabled: false });
      if (kind === "leave") return api.syncLeaveSpace();
      return api.syncResetSpace();
    });
    setDanger(null);
    if (ok && kind === "reset") {
      setNotice({
        text: "同步空间已经重置。其余设备要用下面「添加设备」里新的配对码重新加入；别忘了修改存储账号的密码。",
        clipboard: false,
      });
    }
  };

  const chooseBacklog = (choice: SyncBacklog) => void act("backlog", () => api.syncSetBacklog(choice));

  const turnOff = (
    <button
      disabled={busy !== null}
      onClick={() => void act("off", () => api.syncSaveConfig({ ...config, enabled: false }))}
      className={SYNC_BTN}
    >
      关闭多端同步
    </button>
  );

  return (
    <div className="h-full overflow-y-auto p-6 max-md:p-3">
      {header}
      {loadError && <div className={`mb-4 ${SYNC_ERROR_BOX}`}>{loadError}</div>}
      {notice && (
        <div className={`mb-4 ${notice.error ? SYNC_ERROR_BOX : SYNC_INFO_BOX}`}>
          <div>{notice.text}</div>
          <div className="mt-2 flex items-center gap-2">
            {notice.clipboard && (
              <button onClick={() => void wipeClipboard()} className={SYNC_BTN_SMALL}>
                清空剪贴板
              </button>
            )}
            <button onClick={() => setNotice(null)} className={SYNC_BTN_SMALL}>
              知道了
            </button>
          </div>
        </div>
      )}

      {state === "off" && (
        <div className={SYNC_CARD}>
          <h2 className="text-sm font-medium text-gray-200">让你的几台设备保持一致</h2>
          <div className="mt-2 space-y-1.5 text-xs leading-relaxed text-gray-400">
            <p>开启后，会话、记忆、人格等内容在你的各台设备之间自动同步：电脑上聊到一半，拿起手机可以接着聊。</p>
            <p>
              数据先在这台设备上加密，再存到你自己指定的存储上——WebDAV
              网盘、自建的文件服务器、共享文件夹或对象存储。存储方看不到内容；不需要一台一直开着的电脑，也不用在服务器上装
              RClaw。
            </p>
            <p>
              同步在后台进行，不占用聊天。只有一处会让你等：发消息前先看一眼别的设备有没有续写这个会话，最多
              1.5 秒，可以关掉。
            </p>
            <p>没有开启时，RClaw 不会为同步做任何事。</p>
          </div>
          <button
            disabled={busy !== null}
            onClick={() => void act("on", () => api.syncSaveConfig({ ...config, enabled: true }))}
            className={`mt-3 ${SYNC_BTN_PRIMARY}`}
          >
            {busy === "on" ? "正在开启…" : "开启多端同步"}
          </button>
        </div>
      )}

      {state === "standby" && (
        <div className={SYNC_CARD}>
          <SyncCardHead title={label.text} />
          <div className={SYNC_WARN_BOX}>
            {status.message || "另一个 RClaw 进程正在用这个数据目录做同步。"}
          </div>
          <p className={`mt-2 ${SYNC_HINT}`}>
            刚刚关掉又打开同步时，这个状态会停留一两秒，然后自己恢复。一直这样，就是真的另有一个 RClaw
            在运行。
          </p>
          <div className="mt-3">{turnOff}</div>
        </div>
      )}

      {state === "setup" && (
        <>
          <SyncSetup
            config={config}
            status={status}
            captureBusy={captureBusy}
            onCreated={() => {
              setNotice({
                text: "同步空间已经创建。在下面的「添加设备」里显示配对码，让其他设备加入。",
                clipboard: false,
              });
              void load();
            }}
            onJoined={(warning) => {
              setNotice({
                text: `${warning || "已经加入同步空间，正在把空间里的内容拉下来。"} 配对码就是密钥：如果它是复制粘贴过来的，请清空剪贴板。`,
                clipboard: true,
              });
              void load();
            }}
            onFailedAway={(text) => setNotice({ text, clipboard: false, error: true })}
            onChanged={() => void load()}
          />
          <div className="mb-4 flex items-center gap-3">
            {turnOff}
            <span className={SYNC_HINT}>还不想用的话可以先关掉，填过的设置会留着。</span>
          </div>
        </>
      )}

      {state !== "off" && state !== "standby" && state !== "setup" && !inSpace && (
        <div className={SYNC_CARD}>
          <SyncCardHead title="同步没有启动" />
          <div className={SYNC_ERROR_BOX}>{status.message || "同步引擎没有准备好。"}</div>
          <div className="mt-3 flex items-center gap-2">
            <button
              disabled={busy !== null}
              onClick={() => void act("retry", () => api.syncSaveConfig(config))}
              className={SYNC_BTN_PRIMARY}
            >
              {busy === "retry" ? "正在重试…" : "重试"}
            </button>
            {turnOff}
          </div>
        </div>
      )}

      {active && (
        <>
          <div className={SYNC_CARD}>
            <div className={`text-sm font-medium ${label.tone}`}>{label.text}</div>
            <div className="mt-1 text-[11px] leading-relaxed text-gray-500">
              上次同步：{formatAgo(status.last_sync_ms)}
              {status.outbox > 0 && ` · ${status.outbox} 条改动等着上传`}
              {status.inbox > 0 && ` · ${status.inbox} 条改动等着合入`}
              {status.uploads > 0 && ` · ${status.uploads} 个附件等着上传`}
            </div>
            {status.message && (
              <div className={`mt-2 ${state === "error" ? SYNC_ERROR_BOX : SYNC_WARN_BOX}`}>
                {status.message}
                {status.message.indexOf("同步空间已被重置") === 0 &&
                  " 请先在下面「退出同步空间」，再用新的配对码加入。"}
              </div>
            )}
            <div className="mt-3 flex items-center gap-2">
              {state === "confirm" ? (
                <button
                  disabled={busy !== null}
                  onClick={() => void act("confirm", () => api.syncConfirmMetered())}
                  className={SYNC_BTN_PRIMARY}
                >
                  {busy === "confirm" ? "请稍候…" : "继续同步"}
                </button>
              ) : (
                <button
                  disabled={nowBusy || state === "paused"}
                  onClick={() => void syncNow()}
                  className={SYNC_BTN_PRIMARY}
                >
                  {nowBusy ? "正在同步…" : "立即同步"}
                </button>
              )}
              <button
                disabled={busy !== null}
                onClick={() =>
                  void act("pause", () => api.syncSetPaused(state !== "paused"))
                }
                className={SYNC_BTN}
              >
                {state === "paused" ? "继续同步" : "暂停"}
              </button>
            </div>
            <div className={`mt-2 ${SYNC_HINT}`}>
              这台设备叫「{status.device_name}」· 同步空间 {(status.space_id || "").slice(0, 8)}
            </div>
          </div>

          <SyncPending
            items={status.pending}
            onError={onError}
            onNotice={(text) => setNotice({ text, clipboard: false })}
            onChanged={() => void load()}
          />

          {status.backlog_bytes > 0 && (
            <div className={SYNC_CARD}>
              <SyncCardHead title="以前的附件要不要上传" />
              <p className="text-xs leading-relaxed text-gray-400">
                这台设备上已有的会话里引用了图片和文件，共 {formatBytes(status.backlog_bytes)}{" "}
                还没有上传。之后新产生的附件会照常同步，这里只问以前的。
              </p>
              <div className="mt-3 flex flex-col items-start gap-2">
                <button
                  disabled={busy !== null}
                  onClick={() => chooseBacklog({ days: 30 })}
                  className={SYNC_BTN_PRIMARY}
                >
                  只传最近 30 天的（推荐）
                </button>
                <button disabled={busy !== null} onClick={() => chooseBacklog("all")} className={SYNC_BTN}>
                  全部上传
                </button>
                <button disabled={busy !== null} onClick={() => chooseBacklog("none")} className={SYNC_BTN}>
                  都不上传
                </button>
              </div>
              <p className={`mt-2 ${SYNC_HINT}`}>
                没上传的附件，别的设备打开对应的消息时会来要，这台设备在线时再补传。
              </p>
            </div>
          )}

          <SyncDevices status={status} onError={onError} onChanged={() => void load()} />
          <SyncSettings
            config={config}
            status={status}
            onError={onError}
            onChanged={() => void load()}
          />
        </>
      )}

      {/* 回收站不跟着同步的状态走：关闭同步、退出同步空间之后，里面的东西照样恢复得了。
          不在同步的时候它是空的就不显示。 */}
      <SyncTrash
        refresh={trashTick}
        trashDays={config.trash_days}
        quiet={!active}
        onError={onError}
        onNotice={(text) => setNotice({ text, clipboard: false })}
        onOpenChat={onOpenChat}
      />

      {active && (
        <div className={SYNC_CARD}>
          <SyncCardHead title="停用与重置" />
          {(["off", "leave", "reset"] as Danger[]).map((kind) => (
            <div key={kind} className="flex items-center justify-between gap-3 py-1.5">
              <div className="min-w-0">
                <div className="text-xs text-gray-200">{DANGER_TEXT[kind].title}</div>
                <div className={SYNC_HINT}>{DANGER_TEXT[kind].body[0]}</div>
              </div>
              <button
                disabled={busy !== null}
                onClick={() => setDanger(kind)}
                className={`shrink-0 ${SYNC_BTN_DANGER}`}
              >
                {DANGER_TEXT[kind].confirm}
              </button>
            </div>
          ))}
        </div>
      )}

      {danger && (
        <SyncConfirm
          title={DANGER_TEXT[danger].title}
          confirmText={DANGER_TEXT[danger].confirm}
          danger={danger !== "off"}
          busy={busy !== null}
          onConfirm={() => void confirmDanger()}
          onCancel={() => setDanger(null)}
        >
          {DANGER_TEXT[danger].body.map((line) => (
            <p key={line}>{line}</p>
          ))}
        </SyncConfirm>
      )}
    </div>
  );
}
