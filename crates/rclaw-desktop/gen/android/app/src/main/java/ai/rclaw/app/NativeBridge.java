package ai.rclaw.app;

public final class NativeBridge {
    static {
        System.loadLibrary("rclaw_desktop");
    }

    /** init 时记录的 application context：Rust open_url 经此起 ACTION_VIEW。 */
    private static android.content.Context appContext;

    private NativeBridge() {}

    /** 用系统浏览器打开外部链接（Rust open_url 的 JNI 入口）。 */
    static boolean openUrl(String url) {
        android.content.Context context = appContext;
        if (context == null || url == null || url.isEmpty()) {
            return false;
        }
        try {
            android.content.Intent intent = new android.content.Intent(
                    android.content.Intent.ACTION_VIEW, android.net.Uri.parse(url));
            intent.addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK);
            context.startActivity(intent);
            return true;
        } catch (RuntimeException error) {
            android.util.Log.e("NativeBridge", "openUrl failed", error);
            return false;
        }
    }

    /** 应用内更新：拉起系统安装器安装已下载的 APK（Rust install_update 的 JNI 入口）。
     *  absPath 必须位于 FileProvider 暴露的 files/ 之下（下载命令落在 files/update/）。
     *  Android 8+ 首次会先引导到「未知来源安装」授权页，授权后重新点更新即可。 */
    static boolean installUpdate(String absPath) {
        android.content.Context context = appContext;
        if (context == null || absPath == null || absPath.isEmpty()) {
            return false;
        }
        try {
            java.io.File apk = new java.io.File(absPath);
            if (!apk.isFile() || apk.length() == 0) {
                return false;
            }
            android.net.Uri uri = androidx.core.content.FileProvider.getUriForFile(
                    context, context.getPackageName() + ".fileprovider", apk);
            android.content.Intent intent = new android.content.Intent(android.content.Intent.ACTION_VIEW);
            intent.setDataAndType(uri, "application/vnd.android.package-archive");
            intent.addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION
                    | android.content.Intent.FLAG_ACTIVITY_NEW_TASK);
            context.startActivity(intent);
            return true;
        } catch (RuntimeException error) {
            android.util.Log.e("NativeBridge", "installUpdate failed", error);
            return false;
        }
    }

    /** 会话内点开工作区媒体（视频/图片）：经 FileProvider 授只读 URI，
     *  拉起系统播放器/查看器（Rust open_media 的 JNI 入口）。
     *  relativePath 是相对 agent/ 的路径（如 media/skilltest_fish.mp4）。 */
    static boolean openMedia(String relativePath) {
        android.content.Context context = appContext;
        if (context == null || relativePath == null || relativePath.isEmpty()) {
            return false;
        }
        try {
            java.io.File root = new java.io.File(context.getFilesDir(), "agent");
            java.io.File file = new java.io.File(root, relativePath);
            if (!file.getCanonicalPath().startsWith(root.getCanonicalPath() + java.io.File.separator)
                    || !file.isFile()) {
                return false;
            }
            android.net.Uri uri = androidx.core.content.FileProvider.getUriForFile(
                    context, context.getPackageName() + ".fileprovider", file);
            String mime = sharedMime(context, file, uri);
            android.content.Intent intent = new android.content.Intent(android.content.Intent.ACTION_VIEW);
            intent.setDataAndType(uri, mime != null ? mime : "application/octet-stream");
            intent.addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION
                    | android.content.Intent.FLAG_ACTIVITY_NEW_TASK);
            context.startActivity(intent);
            return true;
        } catch (Exception error) {
            // getCanonicalPath 抛受检 IOException；统一按失败处理
            android.util.Log.e("NativeBridge", "openMedia failed", error);
            return false;
        }
    }

    /** 分享/打开共用 MIME：FileProvider 按 MimeTypeMap 反查，对 md/csv 等
     *  文本类扩展常回 null/octet-stream——系统查看器拒收、微信收成乱码文件。
     *  按扩展名显式补表，未命中的回落 ContentResolver 结果。 */
    private static String sharedMime(android.content.Context context,
            java.io.File file, android.net.Uri uri) {
        String name = file.getName().toLowerCase(java.util.Locale.ROOT);
        String ext = name.contains(".") ? name.substring(name.lastIndexOf('.') + 1) : "";
        switch (ext) {
            case "csv":
                return "text/csv";
            case "tsv":
                return "text/tab-separated-values";
            case "md": case "markdown": case "txt": case "log":
                return "text/plain";
            case "json":
                return "application/json";
            case "html": case "htm":
                return "text/html";
            case "pdf":
                return "application/pdf";
            case "zip":
                return "application/zip";
            default:
                String resolved = context.getContentResolver().getType(uri);
                return resolved != null ? resolved : "application/octet-stream";
        }
    }

    /** CDP 浏览器会话：启动（或复用）长驻调试 WebView，返回 {"ok":true,"pid":N}
     *  （Rust cdp_browser 据此拼 DevTools socket 名；BrowserSession.start 的桥）。 */
    static String browserSessionStart() {
        // BrowserSession.start 只捕获 InterruptedException——其余 RuntimeException
        // /Error 会穿透成 JNI "Java exception was thrown"，堆栈被 exception_clear
        // 丢弃、真因无从查起。这里全量捕获，把类名+堆栈带回工具错误串。
        try {
            return BrowserSession.start(appContext);
        } catch (Throwable t) {
            java.io.StringWriter stack = new java.io.StringWriter();
            t.printStackTrace(new java.io.PrintWriter(stack));
            android.util.Log.e("NativeBridge", "browserSessionStart failed", t);
            try {
                return "{\"ok\":false,\"error\":" + org.json.JSONObject.quote(
                        t.getClass().getName() + ": " + t.getMessage() + "\n" + stack) + "}";
            } catch (Throwable ignored) {
                return "{\"ok\":false,\"error\":\"browserSessionStart Throwable（堆栈序列化失败）\"}";
            }
        }
    }

    /** CDP 浏览器会话：销毁长驻 WebView（browser_close 工具的桥）。面板一并
     *  摘除——只销毁 WebView 会留下挡住聊天的空面板。stop 与 dismiss 全段入
     *  OVERLAY_SERIAL：两步之间若有线程展开并挂载新会话，收尾的 dismiss 会把
     *  新面板藏掉、留下 attached=true 的孤儿 WebView。 */
    static boolean browserSessionStop() {
        synchronized (OVERLAY_SERIAL) {
            boolean stopped = BrowserSession.stop();
            BrowserOverlay.dismiss(appContext);
            return stopped;
        }
    }

    /** CDP 浏览器会话：离屏截图到 PNG（View.draw 软件位图；不走 CDP——离屏
     *  WebView 无合成器帧）。 */
    static boolean browserSessionScreenshot(String absPath) {
        return BrowserSession.screenshot(absPath);
    }

    // ------------------------------------------------------------------
    // CDP 浏览器面板（应用内原生视图：Google 风控页人工过码 + 手动上网）。
    // 返回值约定：String 为 {"ok":bool,...} JSON；attached 为 boolean。
    // ------------------------------------------------------------------

    /**
     * 浏览器面板形态切换与 WebView reparent 的串行锁：expand/collapse/dismiss/
     * 自动展开（通知路径、工具路径、用户按钮路径）全部经它整体互斥——「已显示
     * 面板但还没 attach」的窗口期被收起穿插，会让 WebView 挂进已移除的面板并
     * 误置 attached=true（截图错轨）。锁序固定为 OVERLAY_SERIAL →
     * BrowserSession.LOCK，各处一致，无死锁面。
     */
    static final Object OVERLAY_SERIAL = new Object();

    /** 展开前的会话懒启动（panel 点击时可能尚无会话）。持锁调用。 */
    private static String ensureSessionStarted() {
        if (BrowserSession.isStarted()) {
            return "{\"ok\":true}";
        }
        return BrowserSession.start(appContext);
    }

    /** 展开浏览器面板并把 WebView 挂进容器（会话懒启动）。MainActivity 通知
     *  路径与 UI 侧边栏入口共用。 */
    static String browserOverlayExpand() {
        synchronized (OVERLAY_SERIAL) {
            try {
                String started = ensureSessionStarted();
                if (!started.contains("\"ok\":true")) {
                    return started;
                }
                if (!BrowserOverlay.ensureExpanded(appContext)) {
                    return "{\"ok\":false,\"error\":\"应用窗口未就绪，稍后重试\"}";
                }
                return BrowserSession.attach(BrowserOverlay.webviewHost());
            } catch (Throwable t) {
                return bridgeError("browserOverlayExpand", t);
            }
        }
    }

    /** 异步展开（MainActivity 通知路径：无需回执，结果进日志）。 */
    static void browserOverlayExpandAsync() {
        java.util.concurrent.Executors.newSingleThreadExecutor().execute(() -> {
            String reply = browserOverlayExpand();
            android.util.Log.i("NativeBridge", "browser overlay expand: " + reply);
        });
    }

    /** 会话内点开外链：展开浏览器面板、挂载 WebView 并导航到指定 URL
     *  （聊天面板链接默认应用内打开的 JNI 入口）。仅接受 http(s)。协议头
     *  大小写不敏感（markdown 允许 HTTPS:// 写法，前端/Rust 校验都不挑大
     *  小写，这里只把协议段小写化——路径段大小写有语义，不动）。 */
    static String browserOverlayOpenUrl(String url) {
        if (url == null) {
            return "{\"ok\":false,\"error\":\"仅支持 http(s) 链接\"}";
        }
        String lower = url.toLowerCase(java.util.Locale.ROOT);
        if (!(lower.startsWith("http://") || lower.startsWith("https://"))) {
            return "{\"ok\":false,\"error\":\"仅支持 http(s) 链接\"}";
        }
        int schemeEnd = lower.indexOf("://") + 3;
        String normalized = lower.substring(0, schemeEnd) + url.substring(schemeEnd);
        synchronized (OVERLAY_SERIAL) {
            try {
                String started = ensureSessionStarted();
                if (!started.contains("\"ok\":true")) {
                    return started;
                }
                if (!BrowserOverlay.ensureExpanded(appContext)) {
                    return "{\"ok\":false,\"error\":\"应用窗口未就绪，稍后重试\"}";
                }
                String attached = BrowserSession.attach(BrowserOverlay.webviewHost());
                BrowserSession.loadFromAddressBar(normalized);
                return attached;
            } catch (Throwable t) {
                return bridgeError("browserOverlayOpenUrl", t);
            }
        }
    }

    /** 收起面板：WebView 摘回离屏（agent 不受影响），聊天 UI 重新可见。 */
    static String browserOverlayCollapse() {
        synchronized (OVERLAY_SERIAL) {
            try {
                String detach = BrowserSession.detach();
                BrowserOverlay.collapse(appContext);
                return detach;
            } catch (Throwable t) {
                return bridgeError("browserOverlayCollapse", t);
            }
        }
    }

    /** 关闭面板（WebView 回离屏，不销毁会话）。 */
    static String browserOverlayDismiss() {
        synchronized (OVERLAY_SERIAL) {
            try {
                BrowserOverlay.dismiss(appContext);
                return BrowserSession.detach();
            } catch (Throwable t) {
                return bridgeError("browserOverlayDismiss", t);
            }
        }
    }

    /** WebView 当前是否挂在可见面板里（Rust 截图分流：attached 走 CDP 截图）。 */
    static boolean browserOverlayAttached() {
        return BrowserSession.isAttached() && BrowserOverlay.isExpanded();
    }

    /** Google 风控页命中：高优通知（点开进应用并展开面板）+ 前台时直接展开。 */
    static void notifyCaptchaDetected(String url) {
        try {
            BrowserOverlay.notifyCaptcha(appContext, url);
        } catch (Throwable t) {
            android.util.Log.e("NativeBridge", "notifyCaptchaDetected failed", t);
        }
        // 前台场景直接全屏展开，用户无需再点通知（应用在后台时面板也会挂好，
        // 点通知回来即见）。
        java.util.concurrent.Executors.newSingleThreadExecutor().execute(() -> {
            synchronized (OVERLAY_SERIAL) {
                try {
                    String started = ensureSessionStarted();
                    if (!started.contains("\"ok\":true")) {
                        return;
                    }
                    if (BrowserOverlay.ensureExpandedFullscreen(appContext)) {
                        BrowserSession.attach(BrowserOverlay.webviewHost());
                    }
                } catch (Throwable error) {
                    android.util.Log.e("NativeBridge", "auto expand failed", error);
                }
            }
        });
    }

    /** 悬浮窗/通知动作的统一 Throwable 序列化（类名 + 堆栈进 JSON 错误串）。 */
    private static String bridgeError(String entry, Throwable t) {
        java.io.StringWriter stack = new java.io.StringWriter();
        t.printStackTrace(new java.io.PrintWriter(stack));
        android.util.Log.e("NativeBridge", entry + " failed", t);
        try {
            return "{\"ok\":false,\"error\":" + org.json.JSONObject.quote(
                    t.getClass().getName() + ": " + t.getMessage() + "\n" + stack) + "}";
        } catch (Throwable ignored) {
            return "{\"ok\":false,\"error\":\"" + entry + " Throwable（堆栈序列化失败）\"}";
        }
    }

    /** 转发工作区媒体到其它应用（微信/QQ 等）：ACTION_SEND + FileProvider 只读
     *  URI，拉起系统分享面板（Rust share_media 的 JNI 入口）。 */
    static boolean shareFile(String relativePath) {
        android.content.Context context = appContext;
        if (context == null || relativePath == null || relativePath.isEmpty()) {
            return false;
        }
        try {
            java.io.File root = new java.io.File(context.getFilesDir(), "agent");
            java.io.File file = new java.io.File(root, relativePath);
            if (!file.getCanonicalPath().startsWith(root.getCanonicalPath() + java.io.File.separator)
                    || !file.isFile()) {
                return false;
            }
            android.net.Uri uri = androidx.core.content.FileProvider.getUriForFile(
                    context, context.getPackageName() + ".fileprovider", file);
            String mime = sharedMime(context, file, uri);
            android.content.Intent send = new android.content.Intent(android.content.Intent.ACTION_SEND);
            send.setType(mime != null ? mime : "application/octet-stream");
            send.putExtra(android.content.Intent.EXTRA_STREAM, uri);
            // clipData 让接收方（chooser 转发后）同样拿到读授权。
            send.setClipData(android.content.ClipData.newRawUri("", uri));
            send.addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION);
            android.content.Intent chooser =
                    android.content.Intent.createChooser(send, "转发到其它应用");
            chooser.addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION
                    | android.content.Intent.FLAG_ACTIVITY_NEW_TASK);
            context.startActivity(chooser);
            return true;
        } catch (Exception error) {
            android.util.Log.e("NativeBridge", "shareFile failed", error);
            return false;
        }
    }

    /** 把纯文本分享到其它应用（微信/QQ 收为聊天文字；Rust share_text 的 JNI 入口）。 */
    static boolean shareText(String title, String text) {
        android.content.Context context = appContext;
        if (context == null || text == null || text.isEmpty()) {
            return false;
        }
        try {
            android.content.Intent send = new android.content.Intent(android.content.Intent.ACTION_SEND);
            send.setType("text/plain");
            if (title != null && !title.isEmpty()) {
                send.putExtra(android.content.Intent.EXTRA_SUBJECT, title);
            }
            send.putExtra(android.content.Intent.EXTRA_TEXT, text);
            android.content.Intent chooser =
                    android.content.Intent.createChooser(send, "分享到其它应用");
            chooser.addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK);
            context.startActivity(chooser);
            return true;
        } catch (RuntimeException error) {
            android.util.Log.e("NativeBridge", "shareText failed", error);
            return false;
        }
    }

    // ------------------------------------------------------------------
    // 系统剪贴板（工作区面板的粘贴 / 复制到剪贴板）。
    // ------------------------------------------------------------------

    /** 剪贴板文件名清洗：与 publish_media.py 的 safe_name 同规则。 */
    private static String safeClipName(String name) {
        String cleaned = name.replaceAll("[^A-Za-z0-9._\\u4e00-\\u9fff-]+", "_");
        return cleaned.isEmpty() ? "clipboard-file" : cleaned;
    }

    /** 目录内唯一名：存在则「名(1).扩展」递增。 */
    private static java.io.File uniqueClipFile(java.io.File dir, String name) {
        java.io.File candidate = new java.io.File(dir, name);
        if (!candidate.exists()) return candidate;
        String stem = name, ext = "";
        int dot = name.lastIndexOf('.');
        if (dot > 0) {
            stem = name.substring(0, dot);
            ext = name.substring(dot);
        }
        for (int i = 1; i < 1000; i++) {
            candidate = new java.io.File(dir, stem + "(" + i + ")" + ext);
            if (!candidate.exists()) return candidate;
        }
        return null;
    }

    /** 读系统剪贴板落盘到 destDir：文本 →「剪贴板-时间戳.txt」；
     *  content:// URI（文件管理器的复制）→ 查 DISPLAY_NAME 流式拷贝（≤200MB）。
     *  返回 "ok:<文件名>" / "empty" / "error:<原因>"。 */
    static String readClipboardToFile(String destDir) {
        android.content.Context context = appContext;
        if (context == null || destDir == null || destDir.isEmpty()) {
            return "error:应用未就绪";
        }
        try {
            android.content.ClipboardManager cm = (android.content.ClipboardManager)
                    context.getSystemService(android.content.Context.CLIPBOARD_SERVICE);
            if (cm == null || !cm.hasPrimaryClip() || cm.getPrimaryClip() == null
                    || cm.getPrimaryClip().getItemCount() == 0) {
                return "empty";
            }
            android.content.ClipData.Item item = cm.getPrimaryClip().getItemAt(0);
            java.io.File dir = new java.io.File(destDir);
            if (!dir.isDirectory()) return "error:目标目录不存在";

            // 1) 纯文本 → 直接写 .txt。
            CharSequence textSeq = item.getText();
            if (textSeq != null) {
                String text = textSeq.toString();
                if (text.isEmpty()) return "empty";
                String stamp = new java.text.SimpleDateFormat("yyyyMMdd-HHmmss", java.util.Locale.US)
                        .format(new java.util.Date());
                java.io.File out = uniqueClipFile(dir, "剪贴板-" + stamp + ".txt");
                if (out == null) return "error:同名文件过多";
                try (java.io.FileOutputStream fos = new java.io.FileOutputStream(out)) {
                    fos.write(text.getBytes(java.nio.charset.StandardCharsets.UTF_8));
                }
                return "ok:" + out.getName();
            }

            // 2) content:// URI → 查显示名后流式拷贝。
            android.net.Uri uri = item.getUri();
            if (uri == null) return "empty";
            android.content.ContentResolver cr = context.getContentResolver();
            String name = null;
            try (android.database.Cursor cursor = cr.query(uri, null, null, null, null)) {
                if (cursor != null && cursor.moveToFirst()) {
                    int idx = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME);
                    if (idx >= 0) name = cursor.getString(idx);
                }
            } catch (RuntimeException ignored) {
                // 部分来源不支持 query，退到 lastPathSegment。
            }
            if (name == null || name.trim().isEmpty()) {
                String last = uri.getLastPathSegment();
                if (last != null) {
                    int slash = last.lastIndexOf('/');
                    name = slash >= 0 ? last.substring(slash + 1) : last;
                }
            }
            java.io.File out = uniqueClipFile(dir, safeClipName(name == null ? "" : name));
            if (out == null) return "error:同名文件过多";
            try (java.io.InputStream in = cr.openInputStream(uri);
                 java.io.FileOutputStream fos = new java.io.FileOutputStream(out)) {
                byte[] buf = new byte[65536];
                long total = 0;
                int n;
                while ((n = in.read(buf)) > 0) {
                    total += n;
                    if (total > 200L * 1024 * 1024) {
                        fos.close();
                        if (!out.delete()) android.util.Log.w("NativeBridge", "clip temp not deleted");
                        return "error:文件超过 200MB 上限";
                    }
                    fos.write(buf, 0, n);
                }
            } catch (java.io.FileNotFoundException missing) {
                if (!out.delete()) android.util.Log.w("NativeBridge", "clip temp not deleted");
                return "error:来源文件不可读（已失效或无权限）";
            }
            return "ok:" + out.getName();
        } catch (Exception error) {
            android.util.Log.e("NativeBridge", "readClipboard failed", error);
            return "error:" + error.getMessage();
        }
    }

    /** 把文件以 FileProvider URI 写入系统剪贴板（文件管理器长按粘贴可取出；
     *  微信/QQ 不接收系统文件剪贴板，用「转发到其它应用」）。 */
    static boolean copyFileToClipboard(String absPath) {
        android.content.Context context = appContext;
        if (context == null || absPath == null || absPath.isEmpty()) {
            return false;
        }
        try {
            java.io.File file = new java.io.File(absPath);
            if (!file.isFile()) return false;
            android.net.Uri uri = androidx.core.content.FileProvider.getUriForFile(
                    context, context.getPackageName() + ".fileprovider", file);
            android.content.ClipData clip = android.content.ClipData.newUri(
                    context.getContentResolver(), "RClaw 文件", uri);
            android.content.ClipboardManager cm = (android.content.ClipboardManager)
                    context.getSystemService(android.content.Context.CLIPBOARD_SERVICE);
            if (cm == null) return false;
            cm.setPrimaryClip(clip);
            return true;
        } catch (Exception error) {
            android.util.Log.e("NativeBridge", "copyFileToClipboard failed", error);
            return false;
        }
    }

    // ------------------------------------------------------------------
    // 采集（拍照/录视频/录音）与 whisper：Rust 命令的 JNI 入口。
    // 返回值约定：boolean=成败；String 约定 "ok"/"permission_requested"/数据/错误。
    // ------------------------------------------------------------------

    /** 拍照/录视频（kind: photo|video）：起系统相机，结果走 onActivityResult。
     *  返回 "ok" / "permission_requested" / "error:<原因>"。 */
    static String capture(String kind) {
        if ("photo".equals(kind)) return CaptureBridge.capturePhoto();
        if ("video".equals(kind)) return CaptureBridge.captureVideo();
        return "error:未知采集类型";
    }

    static String recordStart() {
        return CaptureBridge.recordStart();
    }

    static String recordStop() {
        return CaptureBridge.recordStop();
    }

    static boolean recordActive() {
        return CaptureBridge.recordActive();
    }

    static boolean recordCancel() {
        CaptureBridge.recordCancel();
        return true;
    }

    /** 保存到系统相册（MediaStore；kind: photo|video|audio）。 */
    static String saveToGallery(String absPath, String kind) {
        return CaptureBridge.saveToGallery(absPath, kind);
    }

    /** 文档保存到系统下载目录（MediaStore.Downloads；Rust save_media 的 file 分支）。 */
    static String saveToDownloads(String absPath) {
        return CaptureBridge.saveToDownloads(absPath);
    }

    /** 解码音频为 16kHz 单声道 WAV（whisper 输入）。返回 "ok" 或 "error:<原因>"。 */
    static String decodeToWav(String audioAbs, String wavAbs) {
        return CaptureBridge.decodeToWav(audioAbs, wavAbs);
    }

    /** 加载 whisper 推理库（librclaw_whisper.so；模型/推理入口在其中）。 */
    static boolean loadWhisperLib() {
        try {
            System.loadLibrary("rclaw_whisper");
            return true;
        } catch (UnsatisfiedLinkError error) {
            android.util.Log.e("NativeBridge", "loadWhisperLib failed", error);
            return false;
        }
    }

    /** whisper 转写（librclaw_whisper.so 内实现；调用方须在工作线程）。 */
    static native String whisperTranscribe(String modelPath, String wavPath, String language);

    /** 后台线程下载 whisper 模型（多镜像依次尝试，进度写状态文件）。 */
    static boolean downloadWhisperModel(String[] urls, String destName) {
        return CaptureBridge.downloadModel(urls, destName);
    }

    // ------------------------------------------------------------------
    // 运行状态桥（Rust android_status 推送）：前台通知的任务/空闲文案、
    // turn 完成提醒、GUI 前后台门闸。
    // ------------------------------------------------------------------

    private static final String DONE_CHANNEL_ID = "rclaw_done";
    private static final int DONE_NOTIFICATION_ID = 1002;

    /** MainActivity onResume/onPause 上报：GUI 在前台时 turn 完成不再发
     *  提醒通知（用户正看着聊天流，提醒只会打扰）。 */
    private static volatile boolean appInForeground;

    static void setForeground(boolean foreground) {
        appInForeground = foreground;
    }

    /** 应用 GUI 是否在前台（Rust 侧完成通知的门闸）。 */
    static boolean isAppForeground() {
        return appInForeground;
    }

    /** 前台服务通知状态更新：busy=true（任务运行中，绿色 accent + 进度
     *  动画）/ false（空闲：上下文占用 + 内存，文案由 Rust 计算后推送）。 */
    static void updateRuntimeStatus(String text, boolean busy) {
        RClawService.applyRuntimeStatus(text, busy);
    }

    /** turn 收尾提醒（独立「任务完成」通道，有声/横幅；autoCancel 点开进应用）。
     *  调用方（Rust）已按前台门闸过滤。 */
    static void notifyTurnDone(String text) {
        android.content.Context context = appContext;
        if (context == null || text == null || text.isEmpty()) {
            return;
        }
        try {
            android.app.NotificationManager manager =
                    context.getSystemService(android.app.NotificationManager.class);
            if (manager == null) {
                return;
            }
            manager.createNotificationChannel(new android.app.NotificationChannel(
                    DONE_CHANNEL_ID,
                    context.getString(R.string.done_channel),
                    android.app.NotificationManager.IMPORTANCE_DEFAULT));
            android.content.Intent open = new android.content.Intent(context, MainActivity.class);
            android.app.PendingIntent openIntent = android.app.PendingIntent.getActivity(
                    context,
                    2,
                    open,
                    android.app.PendingIntent.FLAG_UPDATE_CURRENT
                            | android.app.PendingIntent.FLAG_IMMUTABLE);
            android.app.Notification notification =
                    new android.app.Notification.Builder(context, DONE_CHANNEL_ID)
                            .setSmallIcon(R.drawable.ic_bagua)
                            .setContentTitle(context.getString(R.string.turn_done_title))
                            .setContentText(text)
                            .setStyle(new android.app.Notification.BigTextStyle().bigText(text))
                            .setContentIntent(openIntent)
                            .setAutoCancel(true)
                            .build();
            manager.notify(DONE_NOTIFICATION_ID, notification);
        } catch (RuntimeException error) {
            android.util.Log.e("NativeBridge", "notifyTurnDone failed", error);
        }
    }

    /** MainActivity 生命周期最早期调用：同步记录 filesDir 宿主上下文
     * （Rust 侧 home 解析与 GUI 装配依赖它；服务稍后的 nativeStart 会带全量配置覆盖）。 */
    static native String nativeInit(String filesDir);

    /** MainActivity.onCreate 最早期调用：持有 application context 供 openUrl 使用。 */
    static void setContext(android.content.Context context) {
        appContext = context.getApplicationContext();
    }

    static void init(java.io.File filesDir) {
        try {
            String error = nativeInit(filesDir.getAbsolutePath());
            if (!error.isEmpty()) {
                android.util.Log.e("NativeBridge", "nativeInit failed: " + error);
            }
        } catch (UnsatisfiedLinkError error) {
            android.util.Log.e("NativeBridge", "nativeInit threw", error);
        }
    }

    static native String nativeStart(String configJson, PythonRuntime pythonRuntime);

    static native String nativeStop();

    static native boolean nativeIsHealthy();

    static native String nativeResolveAgentRoot(String configJson);

}
