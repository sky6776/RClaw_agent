import { useCallback, useEffect, useRef, useState, type ChangeEvent } from "react";
import {
  api,
  type SyncConfig,
  type SyncPairingInfo,
  type SyncScan,
  type SyncStatus,
} from "../lib/api";
import { isAndroidDevice } from "../lib/useIsMobile";
import {
  SYNC_BTN_PRIMARY,
  SYNC_BTN_SMALL,
  SYNC_CARD,
  SYNC_ERROR_BOX,
  SYNC_HINT,
  SYNC_INFO_BOX,
  SYNC_INPUT,
  SYNC_LABEL,
  SYNC_WARN_BOX,
  fileToBase64,
  secretOrNull,
  storageProblem,
} from "../lib/syncUi";
import SyncStorageFields from "./SyncStorageFields";

const SCAN_POLL_MS = 1500;
/** 相机一直没回来（多半是没给相机权限）：这么久之后自己收场。 */
const SCAN_DEADLINE_MS = 10 * 60 * 1000;
/** 与后端识别图片时的上限一致。 */
const QR_IMAGE_MAX_BYTES = 25 * 1024 * 1024;

const TAB_ON =
  "rounded-lg border border-indigo-500/40 bg-indigo-500/15 px-3 py-1.5 text-xs text-indigo-200";
const TAB_OFF =
  "rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-400 transition hover:text-gray-200";

// ---- 拍照扫码的结果轮询 ----
//
// 后端第一次看到照片就把它读走、删掉，识别结果只给这一次调用。所以同一时刻只放一个轮询
// 在途；结果先记在这里，再交给当前挂着的表单——面板在轮询途中被切走，结果也不丢，下次
// 打开时接着用。这里记着的是配对码，只在内存里，不落盘、不打日志。
let scanEpoch = 0;
let scanInFlight: Promise<SyncScan> | null = null;
let scanParked: SyncScan | null = null;
/** 本轮扫码的 requestId：开始、轮询、取消都带它，旧一轮的结果不会被新一轮认领。 */
let scanRequestId: string | null = null;

/** 开始或取消一次扫码：此前在途的轮询、记着的结果都作废。 */
function resetScan() {
  scanEpoch += 1;
  scanInFlight = null;
  scanParked = null;
  scanRequestId = null;
}

function pollScan(requestId: string): Promise<SyncScan> {
  if (scanInFlight) return scanInFlight;
  const epoch = scanEpoch;
  const flight: Promise<SyncScan> = api.syncScanQrPoll(requestId).then(
    (result) => {
      if (scanInFlight === flight) scanInFlight = null;
      if (epoch === scanEpoch && (result.state === "done" || result.state === "failed")) {
        scanParked = result;
      }
      return result;
    },
    (error) => {
      if (scanInFlight === flight) scanInFlight = null;
      throw error;
    },
  );
  scanInFlight = flight;
  return flight;
}

function takeParkedScan(): SyncScan | null {
  const parked = scanParked;
  scanParked = null;
  return parked;
}

/**
 * 还没有同步空间时的两条路：创建一个，或者凭配对码加入已有的。
 *
 * 配对码等同于全部同步数据的钥匙：输入框默认遮住，不进日志、不进本地存储，用完就从
 * 界面状态里清掉。
 */
export default function SyncSetup({
  config,
  status,
  captureBusy,
  onCreated,
  onJoined,
  onFailedAway,
  onChanged,
}: {
  config: SyncConfig;
  status: SyncStatus;
  /** 会话那边正在等一次拍摄的结果：这时不能再开相机扫码（两边共用一条采集通道）。 */
  captureBusy: boolean;
  onCreated: () => void;
  /** 加入成功。`warning`：加入了，但还有要用户处理的事（设置没写回文件）。 */
  onJoined: (warning: string | null) => void;
  /** 创建 / 加入没成功，而这张表单已经不在屏幕上了（用户切到了别的页面）：原因交给面板
   *  留着，下次打开时显示。表单还在的话原因就显示在表单里，不走这里。 */
  onFailedAway: (message: string) => void;
  /** 没成功，但设置可能已经存下了：让面板重新读一次。 */
  onChanged: () => void;
}) {
  const android = isAndroidDevice();
  // 这张表单还在不在屏幕上。创建 / 加入要等网络，用户可能在这期间切走。
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  const [tab, setTab] = useState<"create" | "join">(android ? "join" : "create");
  const [draft, setDraft] = useState<SyncConfig>(config);
  const draftRef = useRef(draft);
  draftRef.current = draft;
  const [secret, setSecret] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [code, setCode] = useState("");
  const [showCode, setShowCode] = useState(false);
  const [info, setInfo] = useState<SyncPairingInfo | null>(null);
  const [infoError, setInfoError] = useState<string | null>(null);
  const [scanning, setScanning] = useState(false);
  const startingScan = useRef(false);
  const [scanNote, setScanNote] = useState<string | null>(null);
  const [reading, setReading] = useState(false);
  const fileRef = useRef<HTMLInputElement>(null);

  const secretSaved = status.secret_saved && status.transport === draft.transport;

  // 配对码一变就让后端看一眼：格式对不对、带没带存储设置、还要不要问密码。
  useEffect(() => {
    const trimmed = code.trim();
    setInfo(null);
    setInfoError(null);
    if (!trimmed) return;
    let cancelled = false;
    const timer = window.setTimeout(() => {
      api.syncPairingInfo(trimmed, draftRef.current).then(
        (result) => {
          if (!cancelled) setInfo(result);
        },
        (e) => {
          if (!cancelled) setInfoError(String(e));
        },
      );
    }, 400);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [code, draft.transport, status.secret_saved]);

  const applyScan = useCallback((result: SyncScan) => {
    if (result.state === "waiting") {
      setScanning(true);
      return;
    }
    setScanning(false);
    if (result.state === "done" && result.code) {
      setCode(result.code);
      setTab("join");
      setError(null);
      setScanNote(
        result.warning
          ? `已经识别出配对码。${result.warning}`
          : "已经识别出配对码，拍下的照片已经删除。",
      );
    } else if (result.state === "failed") {
      setScanNote(result.warning);
      setError(result.error || "没有识别出配对码");
    } else if (result.state === "cancelled") {
      // 用户自己点「取消」时提示已经被清掉、轮次也作废了；能走到这里的多半是相机权限被拒。
      setScanNote("这次扫码已取消。如果是相机权限被拒绝，请允许后重新扫码。");
    }
  }, []);

  // 打开面板时接上没走完的那一次扫码（界面被系统回收过、或者刚才切到别的页去了）。
  useEffect(() => {
    const parked = takeParkedScan();
    if (parked) {
      applyScan(parked);
      return;
    }
    if (!android) return;
    const requestId = scanRequestId;
    if (!requestId) return;
    let stopped = false;
    const epoch = scanEpoch;
    pollScan(requestId).then(
      (result) => {
        if (stopped || epoch !== scanEpoch) return;
        applyScan(takeParkedScan() || result);
      },
      () => {},
    );
    return () => {
      stopped = true;
    };
  }, [android, applyScan]);

  useEffect(() => {
    if (!scanning) return;
    const requestId = scanRequestId;
    if (!requestId) return;
    let stopped = false;
    let timer = 0;
    const epoch = scanEpoch;
    const deadline = Date.now() + SCAN_DEADLINE_MS;
    const tick = () => {
      if (Date.now() > deadline) {
        resetScan();
        setScanning(false);
        setScanNote("等了很久相机也没有返回，这次扫码已经取消。");
        api.syncScanQrCancel(requestId).catch(() => {});
        return;
      }
      pollScan(requestId).then(
        (polled) => {
          if (stopped || epoch !== scanEpoch) return;
          const result = takeParkedScan() || polled;
          applyScan(result);
          if (result.state === "waiting") timer = window.setTimeout(tick, SCAN_POLL_MS);
        },
        (e) => {
          if (stopped || epoch !== scanEpoch) return;
          setScanning(false);
          setError(String(e));
        },
      );
    };
    timer = window.setTimeout(tick, SCAN_POLL_MS);
    return () => {
      stopped = true;
      window.clearTimeout(timer);
    };
  }, [scanning, applyScan]);

  const startScan = async () => {
    // 连点两下只拉起一次相机：按钮要等下一次渲染才变灰，这里先同步挡住。
    if (startingScan.current) return;
    if (captureBusy) {
      setError("正在等另一次拍摄的结果，等它结束再扫码。");
      return;
    }
    startingScan.current = true;
    setError(null);
    setScanNote(null);
    resetScan();
    try {
      const started = await api.syncScanQrStart();
      scanRequestId = started.request_id;
      setScanNote(
        started.status === "permission_requested"
          ? "请先允许 RClaw 使用相机，同意之后相机会自动打开。没有同意的话，点「取消」。"
          : "用相机对准另一台设备上的二维码，拍一张照片。",
      );
      setScanning(true);
    } catch (e) {
      setError(String(e));
    } finally {
      startingScan.current = false;
    }
  };

  const cancelScan = async () => {
    const requestId = scanRequestId;
    resetScan();
    setScanning(false);
    setScanNote(null);
    if (!requestId) return;
    try {
      await api.syncScanQrCancel(requestId);
    } catch (e) {
      setError(String(e));
    }
  };

  const pickImage = async (e: ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files && e.target.files[0];
    e.target.value = "";
    if (!file) return;
    if (file.size > QR_IMAGE_MAX_BYTES) {
      setError("这张图片太大了（超过 25 MB）。");
      return;
    }
    setReading(true);
    setError(null);
    setScanNote(null);
    try {
      const found = await api.syncScanQrImage(await fileToBase64(file));
      setCode(found);
      setScanNote("已经从图片里识别出配对码。这张图片等同于配对码，用完请删掉。");
    } catch (err) {
      setError(String(err));
    } finally {
      setReading(false);
    }
  };

  const create = async () => {
    const problem = storageProblem(draft);
    if (problem) {
      setError(problem);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await api.syncCreateSpace(draft, secretOrNull(secret));
      setSecret("");
      onCreated();
    } catch (e) {
      const message = String(e);
      setError(message);
      if (!alive.current) onFailedAway(`创建同步空间没有成功：${message}`);
      onChanged();
    } finally {
      setBusy(false);
    }
  };

  const join = async () => {
    const trimmed = code.trim();
    if (!trimmed) {
      setError("请先粘贴配对码。");
      return;
    }
    if (infoError) {
      setError(infoError);
      return;
    }
    if (info && !info.has_storage) {
      const problem = storageProblem(draft);
      if (problem) {
        setError(`这个配对码不带存储设置：${problem}`);
        return;
      }
      if (info.needs_secret && !secret) {
        setError("请填写存储的密码。");
        return;
      }
    }
    setBusy(true);
    setError(null);
    try {
      await api.syncJoinSpace(trimmed, draft, secretOrNull(secret));
      setCode("");
      setSecret("");
      setScanNote(null);
      onJoined(null);
    } catch (e) {
      const message = String(e);
      // 密钥已经存好、引擎也连上了，只是设置没写回文件：按「已加入」处理，提醒照样给。
      if (message.indexOf("已经加入同步空间") === 0) {
        setCode("");
        setSecret("");
        setScanNote(null);
        onJoined(message);
      } else {
        setError(message);
        if (!alive.current) onFailedAway(`加入同步空间没有成功：${message}`);
        onChanged();
      }
    } finally {
      setBusy(false);
    }
  };

  const deviceName = (
    <div>
      <label className={SYNC_LABEL}>这台设备的名字</label>
      <input
        type="text"
        value={draft.device_name}
        disabled={busy}
        maxLength={40}
        placeholder={status.device_name || "例如：书房的电脑"}
        onChange={(e) => setDraft({ ...draft, device_name: e.target.value })}
        className={SYNC_INPUT}
      />
      <p className={`mt-1 ${SYNC_HINT}`}>别的设备上会看到这个名字。不填就用上面灰色的那个。</p>
    </div>
  );

  const storageFields = (
    <SyncStorageFields
      config={draft}
      onChange={setDraft}
      secret={secret}
      onSecret={setSecret}
      secretSaved={secretSaved}
      disabled={busy}
    />
  );

  return (
    <div className={SYNC_CARD}>
      <div className="mb-3 flex gap-2">
        <button
          onClick={() => {
            setTab("create");
            setError(null);
          }}
          className={tab === "create" ? TAB_ON : TAB_OFF}
        >
          创建同步空间
        </button>
        <button
          onClick={() => {
            setTab("join");
            setError(null);
          }}
          className={tab === "join" ? TAB_ON : TAB_OFF}
        >
          加入同步空间
        </button>
      </div>
      <p className={`mb-4 ${SYNC_HINT}`}>
        第一台设备「创建」，其余设备凭它给出的配对码「加入」。建议在电脑上创建、手机上加入：手机可以直接拍电脑屏幕上的二维码。
      </p>

      {tab === "create" && (
        <div className="space-y-4">
          {deviceName}
          {storageFields}
          {error && <div className={SYNC_ERROR_BOX}>{error}</div>}
          <div className="flex items-center gap-3">
            <button disabled={busy} onClick={() => void create()} className={SYNC_BTN_PRIMARY}>
              {busy ? "正在创建…" : "创建同步空间"}
            </button>
            <span className={SYNC_HINT}>创建之后，这里会给出配对码。</span>
          </div>
        </div>
      )}

      {tab === "join" && (
        <div className="space-y-4">
          <div>
            <label className={SYNC_LABEL}>配对码</label>
            <div className="flex items-center gap-2">
              <input
                type={showCode ? "text" : "password"}
                value={code}
                disabled={busy}
                autoComplete="off"
                autoCapitalize="off"
                autoCorrect="off"
                spellCheck={false}
                placeholder="在已经加入的设备上：多端同步 → 添加设备"
                onChange={(e) => setCode(e.target.value)}
                className={SYNC_INPUT}
              />
              <button
                onClick={() => setShowCode((shown) => !shown)}
                className={`shrink-0 ${SYNC_BTN_SMALL}`}
              >
                {showCode ? "遮住" : "显示"}
              </button>
            </div>
            <div className="mt-2 flex items-center gap-2">
              {android && (
                <button
                  disabled={busy || scanning || reading}
                  onClick={() => void startScan()}
                  className={SYNC_BTN_SMALL}
                >
                  拍照扫码
                </button>
              )}
              <button
                disabled={busy || scanning || reading}
                onClick={() => fileRef.current && fileRef.current.click()}
                className={SYNC_BTN_SMALL}
              >
                {reading ? "识别中…" : "从图片识别"}
              </button>
              <input
                ref={fileRef}
                type="file"
                accept="image/*"
                className="hidden"
                onChange={(e) => void pickImage(e)}
              />
            </div>
            {scanning && (
              <div className="mt-2 flex items-center gap-2">
                <span className="text-[11px] text-gray-400">在等相机返回…</span>
                <button onClick={() => void cancelScan()} className={SYNC_BTN_SMALL}>
                  取消
                </button>
              </div>
            )}
            {scanNote && <p className="mt-2 text-[11px] leading-relaxed text-gray-400">{scanNote}</p>}
            <p className={`mt-2 ${SYNC_HINT}`}>
              配对码就是全部同步数据的钥匙：不要经聊天软件转发，也不要发给别人。
            </p>
          </div>

          {infoError && <div className={SYNC_ERROR_BOX}>{infoError}</div>}
          {info && info.has_storage && (
            <div className={SYNC_INFO_BOX}>
              这个配对码自带存储设置（{info.transport === "s3" ? "对象存储" : "WebDAV"}）：
              <span className="break-all font-mono">{info.endpoint}</span>
              。加入之后，这台设备的数据会加密后传到这个地址，请确认它是你自己的存储。
            </div>
          )}
          {info && !info.has_storage && (
            <>
              <div className={SYNC_WARN_BOX}>
                这个配对码不带存储设置：请在下面填写与其他设备相同的存储。
              </div>
              {storageFields}
            </>
          )}

          {deviceName}
          {error && <div className={SYNC_ERROR_BOX}>{error}</div>}
          <div className="flex items-center gap-3">
            <button
              disabled={busy || !code.trim()}
              onClick={() => void join()}
              className={SYNC_BTN_PRIMARY}
            >
              {busy ? "正在加入…" : "加入同步空间"}
            </button>
            <span className={SYNC_HINT}>加入后先把空间里的内容拉下来，再上传这台设备上的。</span>
          </div>
        </div>
      )}
    </div>
  );
}
