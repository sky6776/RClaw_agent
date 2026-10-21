import { useCallback, useEffect, useState } from "react";
import { api, type KeepAliveStatus } from "../lib/api";

/** 厂商名 → 系统设置里该页面的常见叫法（按钮文案 / 指引用）。 */
function oemLabel(manufacturer: string): string {
  const m = manufacturer.toLowerCase();
  if (m.includes("xiaomi") || m.includes("redmi") || m.includes("poco")) return "自启动管理";
  if (m.includes("huawei") || m.includes("honor")) return "应用启动管理";
  if (m.includes("vivo") || m.includes("iqoo")) return "后台高耗电 / 自启动";
  if (m.includes("oppo") || m.includes("realme") || m.includes("oneplus")) return "自启动 / 后台冻结";
  if (m.includes("samsung")) return "后台使用限制";
  if (m.includes("meizu")) return "后台管理";
  return "自启动 / 后台管理";
}

/** 后台保活卡片（仅安卓渲染）：电池白名单一键申请 + 厂商自启动页跳转 + 手动项指引。
 *  用户去系统设置再回来时（visibilitychange / focus）自动重查状态。 */
export default function KeepAliveCard() {
  const [status, setStatus] = useState<KeepAliveStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setStatus(await api.keepAliveStatus());
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    const onVisible = () => {
      if (document.visibilityState === "visible") void refresh();
    };
    document.addEventListener("visibilitychange", onVisible);
    window.addEventListener("focus", onVisible);
    return () => {
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("focus", onVisible);
    };
  }, [refresh]);

  const run = useCallback(
    async (action: () => Promise<void>) => {
      setBusy(true);
      setError(null);
      try {
        await action();
      } catch (e) {
        setError(String(e));
      } finally {
        setBusy(false);
      }
    },
    [],
  );

  if (status && !status.supported) return null;

  const whitelisted = status?.battery_whitelisted ?? true;
  const label = oemLabel(status?.manufacturer ?? "");

  return (
    <div className="rounded-xl border border-[#2a2f3a] bg-[#141821] p-4">
      <div className="mb-1 flex items-center justify-between gap-3">
        <h2 className="text-sm font-medium text-gray-200">后台保活</h2>
        {status && (
          <span
            className={`rounded-full px-2 py-0.5 text-[10px] ${
              whitelisted
                ? "bg-emerald-500/15 text-emerald-400"
                : "bg-amber-500/15 text-amber-400"
            }`}
          >
            {whitelisted ? "电池白名单已开启" : "未加入电池白名单"}
          </span>
        )}
      </div>
      <p className="mb-3 text-[10px] leading-relaxed text-gray-600">
        RClaw 靠前台服务常驻接收微信 / QQ 等通道消息。息屏后系统会进入省电状态：未加入电池白名单时网络被切、唤醒被忽略，
        国产手机的自启动管理还会把前台服务一起停掉。这两项都只能由你在系统设置里点一次开启。
      </p>

      <div className="space-y-2">
        <div className="flex items-start gap-3 rounded-lg border border-[#2a2f3a] px-3 py-2.5">
          <span className="mt-0.5 text-xs">{whitelisted ? "✅" : "1️⃣"}</span>
          <div className="min-w-0 flex-1">
            <div className="text-xs font-medium text-gray-200">电池优化白名单</div>
            <div className="mt-0.5 text-[10px] leading-relaxed text-gray-500">
              系统弹窗选「允许」。这是 Doze 省电模式下保持网络与唤醒的唯一开关。
            </div>
          </div>
          {!whitelisted && (
            <button
              onClick={() => void run(api.keepAliveRequestBatteryWhitelist)}
              disabled={busy}
              className="shrink-0 rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-indigo-500 disabled:opacity-50"
            >
              去开启
            </button>
          )}
        </div>

        <div className="flex items-start gap-3 rounded-lg border border-[#2a2f3a] px-3 py-2.5">
          <span className="mt-0.5 text-xs">2️⃣</span>
          <div className="min-w-0 flex-1">
            <div className="text-xs font-medium text-gray-200">
              {label}
              {status?.manufacturer ? `（${status.manufacturer}）` : ""}
            </div>
            <div className="mt-0.5 text-[10px] leading-relaxed text-gray-500">
              {status?.oem_settings
                ? "在列表里找到 RClaw，允许自启动 / 后台运行（或改为「手动管理」并全部允许）。"
                : "本机没有可识别的厂商管理页，将打开 RClaw 的应用详情；请在「电池」或「权限」里允许后台运行与自启动。"}
            </div>
          </div>
          <button
            onClick={() => void run(api.keepAliveOpenOemSettings)}
            disabled={busy}
            className="shrink-0 rounded-lg border border-[#2a2f3a] px-3 py-1.5 text-xs text-gray-300 transition enabled:hover:border-indigo-500/50 enabled:hover:text-indigo-300 disabled:opacity-50"
          >
            打开设置
          </button>
        </div>

        <div className="flex items-start gap-3 rounded-lg border border-[#2a2f3a] px-3 py-2.5">
          <span className="mt-0.5 text-xs">3️⃣</span>
          <div className="min-w-0 flex-1">
            <div className="text-xs font-medium text-gray-200">手动两步</div>
            <div className="mt-0.5 text-[10px] leading-relaxed text-gray-500">
              多任务界面下拉 RClaw 卡片点「锁定」，避免一键清理时被杀；Wi-Fi 高级设置里「休眠时保持 Wi-Fi 连接」选「始终」。
            </div>
          </div>
        </div>
      </div>

      <div className="mt-2 flex items-center gap-3">
        <button
          onClick={() => void refresh()}
          className="text-xs text-gray-500 underline decoration-dotted hover:text-gray-300"
        >
          重新检测
        </button>
        {error && <span className="min-w-0 flex-1 truncate text-xs text-red-400" title={error}>{error}</span>}
      </div>
    </div>
  );
}
