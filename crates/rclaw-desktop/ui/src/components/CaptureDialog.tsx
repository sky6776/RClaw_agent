import { useEffect, useRef, useState } from "react";
import { api } from "../lib/api";

/** 采集结果对话框：拍完视频/录完音后的三选。
 *  视频：发送到当前会话 / 保存到本地 / 取消
 *  音频：发送到当前会话 / 转为文字（whisper）/ 取消
 *  「发送/转文字」成功都以草稿预填收尾（onDraft），文件保留在工作区。 */
export default function CaptureDialog({
  kind,
  rel,
  onDraft,
  onClose,
  onError,
}: {
  kind: "video" | "audio";
  rel: string;
  /** 草稿预填（附件引用行 + 可选转写文本）。 */
  onDraft: (draft: string) => void;
  onClose: () => void;
  onError: (message: string) => void;
}) {
  const [transcribeState, setTranscribeState] = useState<
    "idle" | "downloading" | "transcribing"
  >("idle");
  const [progress, setProgress] = useState(0);
  const [notice, setNotice] = useState<string | null>(null);
  const pollRef = useRef<number | null>(null);

  useEffect(
    () => () => {
      if (pollRef.current !== null) window.clearInterval(pollRef.current);
    },
    [],
  );

  const attachmentLine = `[附件] ${rel}（已放入 agent 工作区）`;

  const send = () => {
    onDraft(attachmentLine);
  };

  const discard = () => {
    void api.discardCapture(rel).catch(() => {});
    onClose();
  };

  const saveLocal = async () => {
    try {
      const result = await api.saveToGallery(rel, kind);
      if (result === "permission_requested") {
        setNotice("请在系统弹窗中授权存储权限后重试");
      } else {
        onClose();
      }
    } catch (e) {
      onError(String(e));
    }
  };

  /** whisper 转写：模型缺失先下载（轮询进度），完成后推理。 */
  const transcribe = async () => {
    try {
      const status = await api.whisperModelStatus();
      if (!status.installed) {
        setTranscribeState("downloading");
        setNotice("首次使用需下载语音模型（约 57MB）");
        await api.whisperModelDownload();
        await new Promise<void>((resolve, reject) => {
          if (pollRef.current !== null) window.clearInterval(pollRef.current);
          // 下载启动有延迟（Java 线程尚未写状态文件）：连续 5 拍既未安装也
          // 不在下载才判失败，避免把"还没开始"误判成"失败了"。
          let idleTicks = 0;
          pollRef.current = window.setInterval(() => {
            void api
              .whisperModelStatus()
              .then((s) => {
                setProgress(
                  s.total > 0 ? Math.round((s.received / s.total) * 100) : 0,
                );
                if (s.installed) {
                  if (pollRef.current !== null) window.clearInterval(pollRef.current);
                  resolve();
                  return;
                }
                idleTicks = s.downloading ? 0 : idleTicks + 1;
                if (idleTicks >= 5) {
                  if (pollRef.current !== null) window.clearInterval(pollRef.current);
                  reject(new Error("模型下载失败（断网或全部镜像不可用）"));
                }
              })
              .catch(reject);
          }, 1000);
        });
      }
      setTranscribeState("transcribing");
      setNotice("识别中，短语音约几秒到一分钟…");
      const text = await api.whisperTranscribe(rel, "zh");
      setTranscribeState("idle");
      onDraft(`${attachmentLine}\n语音识别结果：\n${text}`);
    } catch (e) {
      setTranscribeState("idle");
      setNotice(null);
      onError(String(e));
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
      <div className="mx-4 w-[20rem] rounded-xl border border-[#2a2f3a] bg-[#141821] p-4 shadow-2xl max-md:w-[calc(100%-2rem)]">
        <h2 className="text-sm font-semibold text-gray-100">
          {kind === "video" ? "视频已就绪" : "录音已完成"}
        </h2>
        <p className="mt-1 truncate font-mono text-[11px] text-gray-500" title={rel}>
          {rel}
        </p>
        {notice && (
          <div className="mt-3">
            <p className="text-[11px] leading-relaxed text-indigo-300">{notice}</p>
            {transcribeState === "downloading" && (
              <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-[#0f131b]">
                <div
                  className="h-full rounded-full bg-indigo-500 transition-all"
                  style={{ width: `${Math.max(4, progress)}%` }}
                />
              </div>
            )}
          </div>
        )}
        <div className="mt-4 space-y-2">
          <button
            onClick={send}
            disabled={transcribeState === "transcribing"}
            className="block w-full rounded-lg bg-indigo-500/10 px-3 py-2 text-left text-sm text-indigo-200 transition hover:bg-indigo-500/20 disabled:opacity-40"
          >
            ➡️ 发送到当前会话
          </button>
          {kind === "video" ? (
            <button
              onClick={() => void saveLocal()}
              className="block w-full rounded-lg px-3 py-2 text-left text-sm text-gray-300 transition hover:bg-[#1a1f29]"
            >
              💾 保存到本地（相册）
            </button>
          ) : (
            <button
              onClick={() => void transcribe()}
              disabled={transcribeState !== "idle"}
              className="block w-full rounded-lg px-3 py-2 text-left text-sm text-gray-300 transition hover:bg-[#1a1f29] disabled:opacity-40"
            >
              {transcribeState === "idle"
                ? "🎙️ 转为文字（whisper）"
                : transcribeState === "downloading"
                  ? `⏳ 模型下载中 ${progress}%`
                  : "⏳ 语音识别中…"}
            </button>
          )}
          <button
            onClick={discard}
            disabled={transcribeState === "transcribing"}
            className="block w-full rounded-lg border border-[#2a2f3a] px-3 py-2 text-left text-xs text-gray-400 transition hover:text-gray-200 disabled:opacity-40"
          >
            取消（丢弃本次录制）
          </button>
        </div>
      </div>
    </div>
  );
}
