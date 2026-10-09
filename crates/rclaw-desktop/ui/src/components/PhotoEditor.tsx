import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../lib/api";

/** 笔迹：原图归一化坐标（0..1）+ 样式。 */
interface Stroke {
  color: string;
  width: number;
  points: { x: number; y: number }[];
}

/** 拍照编辑页：旋转 / 画笔标注 / 撤销，完成后三选——发送到当前会话 /
 *  保存到本地 / 取消（丢弃原图）。画布坐标系：笔迹按「原图归一化坐标」
 *  存储，旋转时整体重绘，因此撤销与旋转可任意组合。 */
export default function PhotoEditor({
  dataUrl,
  rel,
  onClose,
  onSent,
  onSaved,
  onError,
}: {
  dataUrl: string;
  /** 原始采集文件（相对 agent/）：取消时删除。 */
  rel: string;
  onClose: () => void;
  /** 发送到当前会话：返回编辑后落盘的工作区路径。 */
  onSent: (rel: string) => void;
  /** 保存到本地：返回编辑后落盘的工作区路径。 */
  onSaved: (rel: string) => void;
  onError: (message: string) => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const imageRef = useRef<HTMLImageElement | null>(null);
  const [rotation, setRotation] = useState(0);
  const [penOn, setPenOn] = useState(false);
  const [penColor, setPenColor] = useState("#ff4d6d");
  const [penWidth, setPenWidth] = useState(4);
  const [strokes, setStrokes] = useState<Stroke[]>([]);
  const [busy, setBusy] = useState(false);
  const drawingRef = useRef<{ x: number; y: number }[] | null>(null);
  // 加载失败时禁掉导出/发送：画布是空的，导出只会得到全黑 JPEG。
  const [loadFailed, setLoadFailed] = useState(false);

  useEffect(() => {
    const image = new Image();
    image.onload = () => {
      imageRef.current = image;
      redraw(0, []);
    };
    image.onerror = () => {
      setLoadFailed(true);
      onError("照片加载失败");
    };
    image.src = dataUrl;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dataUrl]);

  /** 画布尺寸：原图限到 2048（导出 base64 体积可控），随旋转换边。 */
  const dims = useCallback((rotation: number) => {
    const image = imageRef.current;
    const w = image?.naturalWidth ?? 1;
    const h = image?.naturalHeight ?? 1;
    const scale = Math.min(1, 2048 / Math.max(w, h));
    const sw = Math.round(w * scale);
    const sh = Math.round(h * scale);
    return rotation % 180 === 0 ? { w: sw, h: sh, sw, sh } : { w: sh, h: sw, sw, sh };
  }, []);

  const redraw = useCallback(
    (rotation: number, strokes: Stroke[]) => {
      const canvas = canvasRef.current;
      const image = imageRef.current;
      if (!canvas || !image) return;
      const { w, h, sw, sh } = dims(rotation);
      canvas.width = w;
      canvas.height = h;
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.save();
      // 原图坐标系（0..sw × 0..sh）经旋转变换映射到画布，图片与笔迹同空间绘制。
      if (rotation === 90) {
        ctx.translate(w, 0);
        ctx.rotate(Math.PI / 2);
      } else if (rotation === 180) {
        ctx.translate(w, h);
        ctx.rotate(Math.PI);
      } else if (rotation === 270) {
        ctx.translate(0, h);
        ctx.rotate((3 * Math.PI) / 2);
      }
      ctx.drawImage(image, 0, 0, sw, sh);
      // 笔迹存储在原图归一化坐标（0..1），乘回缩放尺寸后与图片同一坐标系。
      for (const stroke of strokes) {
        ctx.strokeStyle = stroke.color;
        ctx.lineWidth = Math.max(2, (stroke.width * sw) / 1000);
        ctx.lineCap = "round";
        ctx.lineJoin = "round";
        ctx.beginPath();
        stroke.points.forEach((p, i) => {
          const x = p.x * sw;
          const y = p.y * sh;
          if (i === 0) ctx.moveTo(x, y);
          else ctx.lineTo(x, y);
        });
        if (stroke.points.length === 1) {
          const p = stroke.points[0];
          ctx.lineTo(p.x * sw + 0.5, p.y * sh);
        }
        ctx.stroke();
      }
      ctx.restore();
    },
    [dims],
  );

  useEffect(() => {
    if (imageRef.current) redraw(rotation, strokes);
  }, [rotation, strokes, redraw]);

  /** 显示坐标 → 原图归一化坐标（逆旋转变换）。 */
  const toImageCoords = (event: React.PointerEvent<HTMLCanvasElement>) => {
    const canvas = canvasRef.current;
    if (!canvas) return null;
    const rect = canvas.getBoundingClientRect();
    const cx = ((event.clientX - rect.left) / rect.width) * canvas.width;
    const cy = ((event.clientY - rect.top) / rect.height) * canvas.height;
    const { sw, sh } = dims(rotation);
    let ox = 0;
    let oy = 0;
    if (rotation === 0) {
      ox = cx;
      oy = cy;
    } else if (rotation === 90) {
      // 正变换: (ox,oy)→(sw−oy, ox)
      oy = sw - cx;
      ox = cy;
    } else if (rotation === 180) {
      ox = sw - cx;
      oy = sh - cy;
    } else {
      // 270: (ox,oy)→(oy, sh−ox)
      oy = cx;
      ox = sh - cy;
    }
    return { x: ox / sw, y: oy / sh };
  };

  const onPointerDown = (event: React.PointerEvent<HTMLCanvasElement>) => {
    if (!penOn) return;
    event.currentTarget.setPointerCapture(event.pointerId);
    const point = toImageCoords(event);
    if (point) drawingRef.current = [point];
  };

  const onPointerMove = (event: React.PointerEvent<HTMLCanvasElement>) => {
    if (!penOn || !drawingRef.current) return;
    const point = toImageCoords(event);
    if (point) drawingRef.current.push(point);
    const canvas = canvasRef.current;
    const image = imageRef.current;
    if (!canvas || !image) return;
    // 实时预览：重绘已定笔迹 + 正在画的一条。
    const live = drawingRef.current;
    const all = [...strokes, { color: penColor, width: penWidth, points: live }];
    redraw(rotation, all);
  };

  const onPointerUp = () => {
    if (!penOn || !drawingRef.current) return;
    const points = drawingRef.current;
    drawingRef.current = null;
    if (points.length > 0) {
      setStrokes((prev) => [...prev, { color: penColor, width: penWidth, points }]);
    }
  };

  /** 画布导出为 JPEG base64 并落盘，返回工作区相对路径。 */
  const exportPhoto = async (): Promise<string> => {
    const canvas = canvasRef.current;
    if (!canvas) throw new Error("画布未就绪");
    const dataUrl = canvas.toDataURL("image/jpeg", 0.9);
    return api.saveEditedPhoto(dataUrl.slice(dataUrl.indexOf(",") + 1));
  };

  const sendToChat = async () => {
    setBusy(true);
    try {
      const saved = await exportPhoto();
      onSent(saved);
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const saveLocal = async () => {
    setBusy(true);
    try {
      const saved = await exportPhoto();
      onSaved(saved);
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-[60] flex flex-col bg-[#0b0e14]">
      {/* 顶栏：编辑工具 */}
      <div className="flex shrink-0 items-center gap-2 border-b border-[#2a2f3a] bg-[#12151d] px-3 py-2 max-md:px-2">
        <span className="text-sm font-medium text-gray-200 max-md:hidden">编辑照片</span>
        <div className="flex-1" />
        <button
          onClick={() => setRotation((r) => (r + 270) % 360)}
          title="向左旋转"
          className="flex h-10 w-10 items-center justify-center rounded-full text-gray-300 transition active:bg-[#1a1f29]"
        >
          ↺
        </button>
        <button
          onClick={() => setRotation((r) => (r + 90) % 360)}
          title="向右旋转"
          className="flex h-10 w-10 items-center justify-center rounded-full text-gray-300 transition active:bg-[#1a1f29]"
        >
          ↻
        </button>
        <button
          onClick={() => {
            setPenOn((v) => !v);
          }}
          title="画笔标注"
          className={`flex h-10 items-center gap-1 rounded-full border px-3 text-xs transition ${
            penOn ? "border-indigo-500 text-indigo-300" : "border-[#2a2f3a] text-gray-300"
          }`}
        >
          ✏️ 画笔
        </button>
        <button
          onClick={() => setStrokes((prev) => prev.slice(0, -1))}
          disabled={strokes.length === 0}
          title="撤销上一笔"
          className="flex h-10 w-10 items-center justify-center rounded-full text-gray-300 transition active:bg-[#1a1f29] disabled:opacity-30"
        >
          ⟲
        </button>
      </div>
      {penOn && (
        <div className="flex shrink-0 items-center gap-3 border-b border-[#2a2f3a] bg-[#141821] px-3 py-2">
          <div className="flex gap-2">
            {["#ff4d6d", "#ffd166", "#4dd4ac", "#4d8dff", "#ffffff"].map((color) => (
              <button
                key={color}
                onClick={() => setPenColor(color)}
                className={`h-7 w-7 rounded-full border-2 transition ${
                  penColor === color ? "border-indigo-400" : "border-transparent"
                }`}
                style={{ backgroundColor: color }}
                aria-label={`画笔颜色 ${color}`}
              />
            ))}
          </div>
          <div className="flex gap-1">
            {[2, 4, 8].map((width) => (
              <button
                key={width}
                onClick={() => setPenWidth(width)}
                className={`flex h-7 w-9 items-center justify-center rounded-md border text-[10px] transition ${
                  penWidth === width
                    ? "border-indigo-400 text-indigo-300"
                    : "border-[#2a2f3a] text-gray-500"
                }`}
              >
                {width === 2 ? "细" : width === 4 ? "中" : "粗"}
              </button>
            ))}
          </div>
        </div>
      )}
      {/* 画布区 */}
      <div className="flex min-h-0 flex-1 items-center justify-center p-2">
        <canvas
          ref={canvasRef}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerUp}
          className={`max-h-full max-w-full rounded-lg ${penOn ? "cursor-crosshair" : ""}`}
          style={{ touchAction: "none" }}
        />
      </div>
      {/* 底部三操作 */}
      <div className="flex shrink-0 items-center justify-center gap-3 border-t border-[#2a2f3a] bg-[#12151d] px-3 py-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]">
        <button
          onClick={onClose}
          disabled={busy}
          className="rounded-full border border-[#2a2f3a] px-5 py-2 text-sm text-gray-300 transition hover:text-gray-100 disabled:opacity-40"
        >
          取消
        </button>
        <button
          onClick={() => void saveLocal()}
          disabled={busy || loadFailed}
          className="rounded-full border border-[#2a2f3a] px-5 py-2 text-sm text-gray-300 transition hover:text-gray-100 disabled:opacity-40"
        >
          保存到本地
        </button>
        <button
          onClick={() => void sendToChat()}
          disabled={busy || loadFailed}
          className="rounded-full bg-indigo-600 px-6 py-2 text-sm font-medium text-white transition hover:bg-indigo-500 disabled:opacity-40"
        >
          {busy ? "处理中…" : "发送到当前会话"}
        </button>
      </div>
    </div>
  );
}
