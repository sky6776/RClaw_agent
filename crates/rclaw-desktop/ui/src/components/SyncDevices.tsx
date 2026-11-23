import { useEffect, useState } from "react";
import { api, type SyncDevice, type SyncPairingCode, type SyncStatus } from "../lib/api";
import {
  SYNC_BTN,
  SYNC_BTN_SMALL,
  SYNC_CARD,
  SYNC_ERROR_BOX,
  SYNC_HINT,
  SYNC_WARN_BOX,
  clearClipboard,
  formatAgo,
  writeClipboard,
} from "../lib/syncUi";
import { SyncCardHead, SyncCheck, SyncConfirm } from "./SyncBits";

/** 配对码在屏幕上最多留这么久，然后自己收起来。 */
const CODE_VISIBLE_MS = 2 * 60 * 1000;

const PLATFORM_LABEL: Record<string, string> = {
  windows: "Windows",
  macos: "macOS",
  linux: "Linux",
  android: "Android",
  termux: "Termux",
};

function DeviceRow({
  device,
  busy,
  onMark,
}: {
  device: SyncDevice;
  busy: boolean;
  onMark: (device: SyncDevice) => void;
}) {
  const about = [PLATFORM_LABEL[device.platform] || device.platform, device.version]
    .filter(Boolean)
    .join(" · ");
  return (
    <div className="flex items-center justify-between gap-3 rounded-lg border border-[#2a2f3a] px-3 py-2">
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <span className="truncate text-xs font-medium text-gray-200">{device.name}</span>
          {device.me && (
            <span className="shrink-0 rounded-full bg-indigo-500/15 px-2 py-0.5 text-[10px] text-indigo-300">
              这台设备
            </span>
          )}
          {device.lost && (
            <span className="shrink-0 rounded-full bg-red-500/10 px-2 py-0.5 text-[10px] text-red-300">
              已标为丢失
            </span>
          )}
          {!device.lost && device.stale && (
            <span className="shrink-0 rounded-full bg-amber-500/10 px-2 py-0.5 text-[10px] text-amber-300">
              很久没出现
            </span>
          )}
        </div>
        <div className="mt-0.5 text-[10px] leading-relaxed text-gray-500">
          {about && <span>{about} · </span>}
          <span>
            {device.me ? "上次写出" : "上次有新内容"}：{formatAgo(device.seen_ms)}
          </span>
          {device.behind > 0 && <span> · 还有 {device.behind} 条在处理</span>}
        </div>
      </div>
      {!device.me && (
        <button disabled={busy} onClick={() => onMark(device)} className={`shrink-0 ${SYNC_BTN_SMALL}`}>
          {device.lost ? "取消丢失标记" : "标为已丢失"}
        </button>
      )}
    </div>
  );
}

/**
 * 设备列表与「添加设备」。
 *
 * 配对码等同于全部同步数据的钥匙：点了才向后端取，默认遮住文字，过一会儿自动收起；
 * 不写日志、不进本地存储。
 */
export default function SyncDevices({
  status,
  onError,
  onChanged,
}: {
  status: SyncStatus;
  onError: (message: string) => void;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [marking, setMarking] = useState<SyncDevice | null>(null);
  const [includeStorage, setIncludeStorage] = useState(true);
  const [pairing, setPairing] = useState<SyncPairingCode | null>(null);
  const [loading, setLoading] = useState(false);
  const [pairError, setPairError] = useState<string | null>(null);
  const [showText, setShowText] = useState(false);
  const [copied, setCopied] = useState(false);
  const canCarryStorage = status.transport !== "file";

  const hide = () => {
    setPairing(null);
    setShowText(false);
  };

  // 配对码不长时间留在屏幕上。到没到点看的是时刻，不只靠定时器：页面转到后台（手机切到
  // 别的应用、锁屏、窗口最小化）时定时器会被系统暂停或放慢，所以回到前台时再核对一次，
  // 过了点就在用户看到之前收起来。
  useEffect(() => {
    if (!pairing) return;
    const deadline = Date.now() + CODE_VISIBLE_MS;
    const check = () => {
      if (Date.now() >= deadline) hide();
    };
    const timer = window.setTimeout(hide, CODE_VISIBLE_MS);
    document.addEventListener("visibilitychange", check);
    window.addEventListener("focus", check);
    window.addEventListener("pageshow", check);
    return () => {
      window.clearTimeout(timer);
      document.removeEventListener("visibilitychange", check);
      window.removeEventListener("focus", check);
      window.removeEventListener("pageshow", check);
    };
  }, [pairing]);

  const reveal = async () => {
    setLoading(true);
    setPairError(null);
    setCopied(false);
    try {
      setPairing(await api.syncPairingCode(canCarryStorage && includeStorage));
    } catch (e) {
      setPairError(String(e));
    } finally {
      setLoading(false);
    }
  };

  const copy = async () => {
    if (!pairing) return;
    try {
      await writeClipboard(pairing.code);
      setCopied(true);
    } catch (e) {
      onError(`复制失败：${String(e)}`);
    }
  };

  const wipeClipboard = async () => {
    try {
      await clearClipboard();
      setCopied(false);
    } catch (e) {
      onError(`清空剪贴板失败：${String(e)}`);
    }
  };

  const mark = async (device: SyncDevice, lost: boolean) => {
    setBusy(true);
    try {
      await api.syncMarkLost(device.id, lost);
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(false);
      setMarking(null);
      onChanged();
    }
  };

  return (
    <div className={SYNC_CARD}>
      <SyncCardHead title={`设备（${status.devices.length}）`} />
      <div className="space-y-2">
        {status.devices.map((device) => (
          <DeviceRow
            key={device.id}
            device={device}
            busy={busy}
            onMark={(target) => {
              if (target.lost) void mark(target, false);
              else setMarking(target);
            }}
          />
        ))}
      </div>

      <div className="mt-4 border-t border-[#2a2f3a] pt-3">
        <div className="text-xs font-medium text-gray-200">添加设备</div>
        <p className={`mt-1 ${SYNC_HINT}`}>
          在新设备上打开 RClaw：多端同步 → 加入同步空间，然后拍下这里的二维码，或者粘贴配对码。
        </p>
        {!pairing && (
          <div className="mt-2 space-y-2">
            {canCarryStorage && (
              <SyncCheck
                checked={includeStorage}
                onChange={setIncludeStorage}
                label="配对码里带上存储的地址和密码"
                hint="新设备扫一下就能用，不用再填一遍。不勾的话，新设备上要自己填写相同的存储。"
              />
            )}
            <button disabled={loading} onClick={() => void reveal()} className={SYNC_BTN}>
              {loading ? "正在生成…" : "显示配对码"}
            </button>
            {pairError && <div className={SYNC_ERROR_BOX}>{pairError}</div>}
          </div>
        )}
        {pairing && (
          <div className="mt-2 space-y-2">
            <div className={SYNC_WARN_BOX}>
              配对码就是全部同步数据的钥匙{canCarryStorage && includeStorage ? "，里面还带着存储的密码" : ""}
              。只给你自己的设备看：不要截图，不要经聊天软件转发。约两分钟后自动收起。
            </div>
            {pairing.qr ? (
              <img
                src={pairing.qr}
                alt="配对二维码"
                className="w-72 max-w-full rounded-lg bg-white"
                style={{ imageRendering: "pixelated" }}
              />
            ) : (
              <div className={SYNC_HINT}>{pairing.qr_error || "配对码太长，画不成二维码；请复制文字。"}</div>
            )}
            <div className="break-all rounded-md border border-[#2a2f3a] bg-[#0f131b] px-2 py-1.5 font-mono text-[10px] leading-relaxed text-gray-300">
              {showText ? pairing.code : "•••••••••••••••••••••••• （已遮住）"}
            </div>
            <div className="flex items-center gap-2">
              <button onClick={() => setShowText((shown) => !shown)} className={SYNC_BTN_SMALL}>
                {showText ? "遮住文字" : "显示文字"}
              </button>
              <button onClick={() => void copy()} className={SYNC_BTN_SMALL}>
                复制
              </button>
              <button onClick={hide} className={SYNC_BTN_SMALL}>
                收起
              </button>
            </div>
          </div>
        )}
        {copied && (
          <div className="mt-2 flex items-center gap-2">
            <span className="text-[11px] leading-relaxed text-amber-300">
              配对码在剪贴板里。在新设备上粘贴之后，请清空剪贴板。
            </span>
            <button onClick={() => void wipeClipboard()} className={`shrink-0 ${SYNC_BTN_SMALL}`}>
              清空剪贴板
            </button>
          </div>
        )}
      </div>

      {marking && (
        <SyncConfirm
          title={`把「${marking.name}」标为已丢失`}
          confirmText="标为已丢失"
          danger
          busy={busy}
          onConfirm={() => void mark(marking, true)}
          onCancel={() => setMarking(null)}
        >
          <p>标记之后，它名下的定时任务可以被别的设备强制接管，不用等它确认。</p>
          <p>
            这并不能收回它已经拿到的密钥。设备真的丢了，请用下面的「重置同步空间」，并修改存储账号的密码。
          </p>
        </SyncConfirm>
      )}
    </div>
  );
}
