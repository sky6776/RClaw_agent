package ai.rclaw.app;

import android.graphics.Bitmap;
import android.media.MediaMetadataRetriever;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.util.Locale;

public final class MediaRuntime {
    private static final long MAX_VIDEO_BYTES = 250L * 1024L * 1024L;

    private MediaRuntime() {}

    public static String extractVideoFrames(
            String videoPath,
            String outputDirectory,
            int frameLimit) throws IOException, JSONException {
        // 上限 8 与实现一致（下方按视频秒数再收紧到 ≤8）：glm5v 抽帧设计上限
        // 就是 8 帧，校验放 64 只会让 >8 的请求被静默缩水而非显式报错。
        if (frameLimit < 1 || frameLimit > 8) {
            throw new IOException("video frameLimit 必须在 1..8 之间");
        }
        File video = PdfRuntime.requirePrivateFile(videoPath, "");
        PdfRuntime.requireFileSize(video, MAX_VIDEO_BYTES, "视频");
        File output = PdfRuntime.requirePrivateDirectory(outputDirectory);
        MediaMetadataRetriever retriever = new MediaMetadataRetriever();
        JSONArray frames = new JSONArray();
        try {
            retriever.setDataSource(video.getCanonicalPath());
            String durationText = retriever.extractMetadata(
                    MediaMetadataRetriever.METADATA_KEY_DURATION);
            long durationMs = Long.parseLong(durationText == null ? "0" : durationText);
            if (durationMs <= 0L) {
                throw new IOException("无法读取视频时长");
            }
            int count = Math.min(frameLimit, Math.max(1, Math.min(8, (int) Math.ceil(durationMs / 1000.0))));
            for (int index = 0; index < count; index++) {
                double timestamp = (durationMs / 1000.0) * (index + 0.5) / count;
                Bitmap frame = retriever.getFrameAtTime(
                        Math.round(timestamp * 1_000_000.0),
                        MediaMetadataRetriever.OPTION_CLOSEST_SYNC);
                if (frame == null) {
                    continue;
                }
                Bitmap rendered = frame;
                try {
                    int width = frame.getWidth();
                    int height = frame.getHeight();
                    long pixels = (long) width * height;
                    if (width > 1600 || height > 1600 || pixels > 2_560_000L) {
                        float dimensionScale = Math.min(1600.0f / width, 1600.0f / height);
                        float pixelScale = (float) Math.sqrt(2_560_000.0 / pixels);
                        float scale = Math.min(1.0f, Math.min(dimensionScale, pixelScale));
                        width = Math.max(1, Math.round(width * scale));
                        height = Math.max(1, Math.round(height * scale));
                        rendered = Bitmap.createScaledBitmap(frame, width, height, true);
                    }
                    File target = new File(
                            output,
                            String.format(Locale.US, "frame_%03d.jpg", index + 1));
                    PdfRuntime.requireSafeOutputFile(target, output);
                    try (FileOutputStream stream = new FileOutputStream(target, false)) {
                        if (!rendered.compress(Bitmap.CompressFormat.JPEG, 90, stream)) {
                            throw new IOException("视频帧 JPEG 编码失败");
                        }
                        stream.getFD().sync();
                    }
                    frames.put(new JSONObject()
                            .put("path", target.getCanonicalPath())
                            .put("timestamp", timestamp));
                } finally {
                    if (rendered != frame) {
                        rendered.recycle();
                    }
                    frame.recycle();
                }
            }
        } catch (NumberFormatException error) {
            throw new IOException("视频时长元数据无效", error);
        } finally {
            retriever.release();
        }
        if (frames.length() == 0) {
            throw new IOException("Android 媒体运行时未能提取视频帧");
        }
        return frames.toString();
    }
}
