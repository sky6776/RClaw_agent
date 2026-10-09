package ai.rclaw.app;

import android.content.Context;
import android.graphics.Bitmap;
import android.graphics.pdf.PdfRenderer;
import android.os.ParcelFileDescriptor;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.file.Files;
import java.util.Locale;

public final class PdfRuntime {
    private static final long MAX_PDF_BYTES = 100L * 1024L * 1024L;
    private static final long MAX_RENDERED_BYTES = 512L * 1024L * 1024L;
    private static volatile File filesRoot;

    private PdfRuntime() {}

    static void initialize(Context context) {
        filesRoot = context.getApplicationContext().getFilesDir();
    }

    public static String renderPdf(
            String pdfPath,
            String outputDirectory,
            int dpi,
            int maxPages) throws IOException, JSONException {
        if (dpi < 36 || dpi > 600) {
            throw new IOException("PDF dpi 必须在 36..600 之间");
        }
        if (maxPages < 1 || maxPages > 300) {
            throw new IOException("PDF maxPages 必须在 1..300 之间");
        }
        File pdf = requirePrivateFile(pdfPath, ".pdf");
        requireFileSize(pdf, MAX_PDF_BYTES, "PDF");
        File output = requirePrivateDirectory(outputDirectory);
        JSONArray pages = new JSONArray();
        long renderedBytes = 0L;
        try (ParcelFileDescriptor descriptor = ParcelFileDescriptor.open(
                        pdf,
                        ParcelFileDescriptor.MODE_READ_ONLY);
                PdfRenderer renderer = new PdfRenderer(descriptor)) {
            int count = Math.min(renderer.getPageCount(), maxPages);
            for (int index = 0; index < count; index++) {
                try (PdfRenderer.Page page = renderer.openPage(index)) {
                    File target = new File(
                            output,
                            String.format(Locale.US, "page_%03d.png", index + 1));
                    renderPage(page, target, output, dpi, index + 1);
                    renderedBytes += target.length();
                    if (renderedBytes > MAX_RENDERED_BYTES) {
                        if (!target.delete()) {
                            target.deleteOnExit();
                        }
                        // 本批此前已写盘的页一并清理：调用方拿不到 JSON（异常
                        // 返回），留下的 PNG 只会成为孤儿文件。
                        for (int i = 0; i < pages.length(); i++) {
                            try {
                                new java.io.File(pages.getJSONObject(i).getString("path")).delete();
                            } catch (JSONException | RuntimeException ignored) {
                            }
                        }
                        throw new IOException("PDF 页面输出超过 512 MiB 上限");
                    }
                    pages.put(new JSONObject()
                            .put("page", index + 1)
                            .put("path", target.getCanonicalPath()));
                }
            }
        }
        return pages.toString();
    }

    public static String renderPdfPage(
            String pdfPath,
            String outputPath,
            int dpi,
            int pageIndex) throws IOException {
        if (dpi < 36 || dpi > 600) {
            throw new IOException("PDF dpi 必须在 36..600 之间");
        }
        File pdf = requirePrivateFile(pdfPath, ".pdf");
        requireFileSize(pdf, MAX_PDF_BYTES, "PDF");
        File requested = new File(outputPath).getAbsoluteFile();
        if (!requested.getName().toLowerCase(Locale.ROOT).endsWith(".png")) {
            throw new IOException("PDF 单页输出必须是 PNG: " + outputPath);
        }
        File parent = requested.getParentFile();
        if (parent == null) {
            throw new IOException("PDF 单页输出缺少父目录: " + outputPath);
        }
        File output = requirePrivateDirectory(parent.getPath());
        requireSafeOutputFile(requested, output);
        try (ParcelFileDescriptor descriptor = ParcelFileDescriptor.open(
                        pdf,
                        ParcelFileDescriptor.MODE_READ_ONLY);
                PdfRenderer renderer = new PdfRenderer(descriptor)) {
            if (pageIndex < 0 || pageIndex >= renderer.getPageCount()) {
                throw new IOException("PDF pageIndex 超出范围: " + pageIndex);
            }
            try (PdfRenderer.Page page = renderer.openPage(pageIndex)) {
                renderPage(page, requested, output, dpi, pageIndex + 1);
            }
        }
        return requested.getCanonicalPath();
    }

    private static void renderPage(
            PdfRenderer.Page page,
            File target,
            File output,
            int dpi,
            int pageNumber) throws IOException {
        float scale = dpi / 72.0f;
        int width = Math.max(1, Math.round(page.getWidth() * scale));
        int height = Math.max(1, Math.round(page.getHeight() * scale));
        long pixels = (long) width * height;
        if (width > 3072 || height > 3072 || pixels > 8_000_000L) {
            float dimensionScale = Math.min(3072.0f / width, 3072.0f / height);
            float pixelScale = (float) Math.sqrt(8_000_000.0 / pixels);
            float safeScale = Math.min(dimensionScale, pixelScale);
            width = Math.max(1, Math.round(width * safeScale));
            height = Math.max(1, Math.round(height * safeScale));
        }
        Bitmap bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888);
        try {
            bitmap.eraseColor(0xFFFFFFFF);
            page.render(bitmap, null, null, PdfRenderer.Page.RENDER_MODE_FOR_DISPLAY);
            requireSafeOutputFile(target, output);
            try (FileOutputStream stream = new FileOutputStream(target, false)) {
                if (!bitmap.compress(Bitmap.CompressFormat.PNG, 100, stream)) {
                    throw new IOException("PDF 页面 PNG 编码失败: " + pageNumber);
                }
                stream.getFD().sync();
            }
        } finally {
            bitmap.recycle();
        }
    }

    public static int pageCount(String pdfPath) throws IOException {
        File pdf = requirePrivateFile(pdfPath, ".pdf");
        requireFileSize(pdf, MAX_PDF_BYTES, "PDF");
        try (ParcelFileDescriptor descriptor = ParcelFileDescriptor.open(
                        pdf,
                        ParcelFileDescriptor.MODE_READ_ONLY);
                PdfRenderer renderer = new PdfRenderer(descriptor)) {
            return renderer.getPageCount();
        }
    }

    static File requirePrivateFile(String path, String extension) throws IOException {
        File file = new File(path).getCanonicalFile();
        requireInsideFiles(file);
        if (!file.isFile()
                || (!extension.isEmpty()
                        && !file.getName().toLowerCase(Locale.ROOT).endsWith(extension))) {
            throw new IOException("无效的 app 私有文件: " + path);
        }
        return file;
    }

    static File requirePrivateDirectory(String path) throws IOException {
        File requested = new File(path).getAbsoluteFile();
        File existing = requested;
        while (existing != null && !existing.exists()) {
            existing = existing.getParentFile();
        }
        if (existing == null) {
            throw new IOException("输出目录没有可验证的父目录: " + path);
        }
        requireInsideFiles(existing.getCanonicalFile());
        if (!requested.isDirectory() && !requested.mkdirs()) {
            throw new IOException("无法创建 app 私有输出目录: " + path);
        }
        File directory = requested.getCanonicalFile();
        requireInsideFiles(directory);
        return directory;
    }

    static void requireFileSize(File file, long maxBytes, String kind) throws IOException {
        if (file.length() > maxBytes) {
            throw new IOException(kind + " 文件超过大小限制: " + maxBytes + " bytes");
        }
    }

    static void requireSafeOutputFile(File target, File outputDirectory) throws IOException {
        if (Files.isSymbolicLink(target.toPath())) {
            throw new IOException("输出文件不允许是符号链接: " + target);
        }
        File canonicalParent = target.getCanonicalFile().getParentFile();
        if (canonicalParent == null
                || !canonicalParent.equals(outputDirectory.getCanonicalFile())) {
            throw new IOException("输出文件超出目标目录: " + target);
        }
    }

    private static void requireInsideFiles(File path) throws IOException {
        File root = filesRoot;
        if (root == null) {
            throw new IOException("PDF runtime 尚未初始化");
        }
        String rootPath = root.getCanonicalPath();
        String candidate = path.getCanonicalPath();
        if (!candidate.equals(rootPath)
                && !candidate.startsWith(rootPath + File.separator)) {
            throw new IOException("PDF 路径超出 app 私有目录: " + candidate);
        }
    }
}
