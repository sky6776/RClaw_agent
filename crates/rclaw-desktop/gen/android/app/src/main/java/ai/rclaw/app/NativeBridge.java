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
            String mime = context.getContentResolver().getType(uri);
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
            String mime = context.getContentResolver().getType(uri);
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

    /** 无头浏览器取页（Rust web_browser 工具的 JNI 入口；调用方须在工作线程）。 */
    static String browse(String url, int timeoutMs) {
        return HeadlessBrowser.browse(appContext, url, timeoutMs);
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
