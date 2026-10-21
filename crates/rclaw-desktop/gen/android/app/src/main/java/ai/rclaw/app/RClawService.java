package ai.rclaw.app;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkRequest;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.os.IBinder;
import android.os.PowerManager;
import android.util.Log;

import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

public final class RClawService extends Service {
    private static final String TAG = "RClawService";
    static final String ACTION_START = "ai.rclaw.app.START";
    static final String ACTION_APPLY_CONFIG = "ai.rclaw.app.APPLY_CONFIG";
    static final String ACTION_STOP = "ai.rclaw.app.STOP";
    static final String ACTION_RECOVER = "ai.rclaw.app.RECOVER";
    static final String ACTION_KEEP_ALIVE = "ai.rclaw.app.KEEP_ALIVE";
    private static final String CHANNEL_ID = "rclaw_runtime";
    private static final String FATAL_PROCESS_RESTART = "FATAL_PROCESS_RESTART:";
    private static final int NOTIFICATION_ID = 1001;
    private static final long HEALTH_CHECK_INTERVAL_SECONDS = 60L;
    // wake lock 按需持有（此前 20 分钟租约每 10 分钟续 = CPU 永不休眠）：Doze 下
    // 非白名单应用的 wake lock 本就被系统忽略，常驻持锁只在非 Doze 时段实打实
    // 耗电，正是厂商「后台高耗电」杀手的识别特征。现在只在三个窗口持有：
    // 启动装配期（低端机 Python 导入可超 60s）、turn 执行期（Rust 推送 busy）、
    // 看门狗 tick（闹钟唤醒后把健康检查做完）。空闲期靠 WebSocket 入站包唤醒 CPU，
    // 配合电池白名单（KeepAliveGuide）网络在 Doze 里保持可用。
    private static final long STARTUP_WAKE_LOCK_MS = 5 * 60 * 1000L;
    private static final long TURN_WAKE_LOCK_MS = 15 * 60 * 1000L;
    private static final long TICK_WAKE_LOCK_MS = 10 * 1000L;
    private static final AtomicBoolean PROCESS_NATIVE_START_ATTEMPTED = new AtomicBoolean();

    private final AtomicBoolean startRequested = new AtomicBoolean();
    private final AtomicBoolean stopping = new AtomicBoolean();
    private final AtomicBoolean recoveryRequested = new AtomicBoolean();
    private final AtomicBoolean nativeStopRequested = new AtomicBoolean();
    private final ExecutorService runtimeExecutor = Executors.newSingleThreadExecutor();
    private final ScheduledExecutorService watchdogExecutor =
            Executors.newSingleThreadScheduledExecutor();
    // Rust 运行状态推送（NativeBridge.updateRuntimeStatus → applyRuntimeStatus）：
    // native 就绪后动态文案优先于服务生命周期文案；busy turn 用绿色 accent +
    // 不定进度动画表现「正在执行」，空闲显示上下文占用 + 内存。
    private static volatile RClawService activeInstance;
    private volatile String lifecycleText;
    private volatile String runtimeStatusText;
    private volatile boolean runtimeBusy;
    private final android.os.Handler mainHandler =
            new android.os.Handler(android.os.Looper.getMainLooper());
    private volatile boolean nativeStarted;
    private volatile long nativeStartedAtMs;
    private PowerManager.WakeLock startupWakeLock;
    private PowerManager.WakeLock turnWakeLock;
    private PowerManager.WakeLock tickWakeLock;
    private WifiManager.WifiLock wifiLock;
    private ConnectivityManager connectivityManager;
    private boolean networkRequested;
    private Thread.UncaughtExceptionHandler previousCrashHandler;
    private Thread.UncaughtExceptionHandler crashHandler;
    private final ConnectivityManager.NetworkCallback networkCallback =
            new ConnectivityManager.NetworkCallback() {
                @Override
                public void onAvailable(Network network) {
                    Log.i(TAG, "Active keep-alive network is available: " + network);
                }

                @Override
                public void onLost(Network network) {
                    Log.w(TAG, "Active keep-alive network was lost: " + network);
                }
            };

    @Override
    public void onCreate() {
        super.onCreate();
        activeInstance = this;
        NotificationManager manager = getSystemService(NotificationManager.class);
        manager.createNotificationChannel(new NotificationChannel(
                CHANNEL_ID,
                getString(R.string.runtime_channel),
                NotificationManager.IMPORTANCE_LOW));
        PowerManager power = getSystemService(PowerManager.class);
        if (power == null) {
            Log.e(TAG, "PowerManager is unavailable");
        } else {
            startupWakeLock = newWakeLock(power, "rclaw:startup");
            turnWakeLock = newWakeLock(power, "rclaw:turn");
            tickWakeLock = newWakeLock(power, "rclaw:tick");
        }
        installCrashHandler();
        watchdogExecutor.scheduleWithFixedDelay(
                this::checkNativeRuntime,
                HEALTH_CHECK_INTERVAL_SECONDS,
                HEALTH_CHECK_INTERVAL_SECONDS,
                TimeUnit.SECONDS);
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && ACTION_STOP.equals(intent.getAction())) {
            AppConfig.preferences(this).edit().putBoolean("should_run", false).apply();
            WatchdogScheduler.cancel(this);
            stopSelf();
            return START_NOT_STICKY;
        }

        String action = intent == null ? null : intent.getAction();
        boolean userStart = ACTION_START.equals(action);
        boolean configApply = ACTION_APPLY_CONFIG.equals(action);
        boolean keepAliveStart = ACTION_KEEP_ALIVE.equals(action);
        boolean explicitStart = userStart || configApply;
        boolean recoveryStart = intent == null
                || ACTION_RECOVER.equals(action)
                || keepAliveStart;
        if (!explicitStart
                && !AppConfig.preferences(this).getBoolean("should_run", false)) {
            WatchdogScheduler.cancel(this);
            stopSelf();
            return START_NOT_STICKY;
        }
        if (explicitStart) {
            WatchdogScheduler.resetForExplicitStart(this);
            // 显式启动即表达运行意图：万一 nativeStart 挂死被启动截止杀进程，
            // 恢复闹钟不因 should_run=false 短路（真失败仍由 handleStartupFailure
            // 置 false 停止，另有 6 次熔断兜底）。
            AppConfig.preferences(this).edit().putBoolean("should_run", true).apply();
        } else {
            long remainingDelay = WatchdogScheduler.remainingRecoveryDelay(this);
            if (remainingDelay > 0L) {
                showForegroundNotification(getString(
                        R.string.runtime_restarting,
                        Math.max(1L, remainingDelay / 1000L)));
                WatchdogScheduler.scheduleAfter(this, remainingDelay);
                stopSelf();
                return START_NOT_STICKY;
            }
        }

        if (keepAliveStart && startRequested.get()) {
            showForegroundNotification(getString(nativeStarted
                    ? R.string.runtime_running
                    : R.string.runtime_starting));
            maintainKeepAliveResources();
            if (nativeStarted) {
                // 闹钟唤醒的健康检查排在 runtimeExecutor 上异步跑：系统为广播
                // 持有的 wake lock 到 onStartCommand 返回就没了，这里自己兜 10s。
                acquireTimed(tickWakeLock, TICK_WAKE_LOCK_MS);
                runtimeExecutor.execute(this::checkNativeRuntime);
            }
            return START_STICKY;
        }

        showForegroundNotification(getString(R.string.runtime_starting));
        maintainKeepAliveResources();
        // v2：配置改动经 GUI 热启动生效，不再走「杀进程重载」。已尝试过启动的
        // 进程收到 keep-alive 类 intent 时只维持前台状态（装配失败由 GUI 展示，
        // 用户修改配置后经 ACTION_APPLY_CONFIG 显式重试）。
        if (PROCESS_NATIVE_START_ATTEMPTED.get() && !startRequested.get() && !explicitStart) {
            showForegroundNotification(getString(nativeStarted
                    ? R.string.runtime_running
                    : R.string.runtime_starting));
            maintainKeepAliveResources();
            return START_STICKY;
        }
        if (startRequested.compareAndSet(false, true)) {
            scheduleStartDeadline();
            runtimeExecutor.execute(() -> startNativeRuntime(recoveryStart));
        } else if (userStart) {
            // v2：用户显式启动但运行时已在装配/已就绪——只维持前台状态。
            // （v0.2.0 在此杀进程以重载配置；v2 配置经 GUI 热启动生效。）
            showForegroundNotification(getString(nativeStarted
                    ? R.string.runtime_running
                    : R.string.runtime_starting));
            maintainKeepAliveResources();
        }
        return START_STICKY;
    }

    private void startNativeRuntime(boolean recoveryStart) {
        // 启动装配期持锁：nativeStart 秒回但装配（Python 导入、通道连接）在后台
        // 线程继续，息屏后不能被挂起。Rust 首次推送运行状态即装配完成，届时
        // 在 updateTurnLocks 释放；失败路径在 handleStartupFailure 释放；最晚
        // 5 分钟（= 启动截止）自动到期。
        acquireTimed(startupWakeLock, STARTUP_WAKE_LOCK_MS);
        try {
            RuntimeAssets.installBundledSkills(this);
            PROCESS_NATIVE_START_ATTEMPTED.set(true);
            AppConfig.trace(this, "native-start begin recovery=" + recoveryStart);
            String error = NativeBridge.nativeStart(
                    AppConfig.toJson(this),
                    new PythonRuntime(getApplicationContext()));
            AppConfig.trace(this, "native-start returned empty=" + error.isEmpty());
            if (error.isEmpty()) {
                if (stopping.get()) {
                    stopNativeRuntimeWithDeadline();
                    return;
                }
                nativeStarted = true;
                nativeStartedAtMs = System.currentTimeMillis();
                AppConfig.preferences(this).edit().putBoolean("should_run", true).apply();
                WatchdogScheduler.markHealthy(this, 0L);
                showForegroundNotification(getString(R.string.runtime_running));
            } else {
                Log.e(TAG, "Native runtime failed to start: " + error);
                if (error.startsWith(FATAL_PROCESS_RESTART)) {
                    if (stopping.get()) {
                        terminateProcess(0);
                        return;
                    }
                    boolean recoverySaved = AppConfig.preferences(this)
                            .edit()
                            .putBoolean("should_run", true)
                            .commit();
                    if (recoverySaved) {
                        requestProcessRecovery(error, null);
                    } else {
                        handleStartupFailure("无法保存进程恢复状态", false);
                    }
                } else {
                    handleStartupFailure(error, recoveryStart);
                }
            }
        } catch (Exception | UnsatisfiedLinkError error) {
            Log.e(TAG, "Native runtime startup threw", error);
            handleStartupFailure(String.valueOf(error.getMessage()), recoveryStart);
        }
    }

    private void handleStartupFailure(String error, boolean recoveryStart) {
        releaseLock(startupWakeLock);
        if (recoveryStart
                && AppConfig.preferences(this).getBoolean("should_run", false)) {
            long delay = WatchdogScheduler.scheduleRecovery(this);
            if (WatchdogScheduler.recoveryExhausted(this)) {
                // 熔断：坏配置/持续崩溃会让退避封顶后仍每 10 分钟杀一次进程
                //（连同前台 GUI）。暂停自动恢复，用户显式打开 App 才重新计数。
                AppConfig.preferences(this).edit().putBoolean("should_run", false).apply();
                WatchdogScheduler.cancel(this);
                showForegroundNotification(getString(R.string.start_failed,
                        "连续启动失败已达上限，自动恢复已暂停；打开 App 手动启动可重新开始"));
                PROCESS_NATIVE_START_ATTEMPTED.set(false);
                startRequested.set(false);
                stopSelf();
                return;
            }
            showForegroundNotification(getString(
                    R.string.runtime_restarting,
                    Math.max(1L, delay / 1000L)));
        } else {
            showForegroundNotification(getString(R.string.start_failed, error));
            AppConfig.preferences(this).edit().putBoolean("should_run", false).apply();
            WatchdogScheduler.cancel(this);
        }
        PROCESS_NATIVE_START_ATTEMPTED.set(false);
        startRequested.set(false);
        stopSelf();
    }

    private void checkNativeRuntime() {
        if (stopping.get() || !nativeStarted) {
            return;
        }
        try {
            boolean healthy = NativeBridge.nativeIsHealthy();
            AppConfig.trace(this, "health-check healthy=" + healthy
                    + " uptimeMs=" + (System.currentTimeMillis() - nativeStartedAtMs));
            if (healthy) {
                WatchdogScheduler.markHealthy(
                        this,
                        System.currentTimeMillis() - nativeStartedAtMs);
                return;
            }
            requestProcessRecovery("native runtime heartbeat expired", null);
        } catch (RuntimeException | UnsatisfiedLinkError error) {
            AppConfig.trace(this, "health-check threw: " + error);
            requestProcessRecovery("native runtime health check failed", error);
        }
    }

    private void requestProcessRecovery(String reason, Throwable error) {
        if (stopping.get() || !recoveryRequested.compareAndSet(false, true)) {
            AppConfig.trace(this, "recovery-suppressed " + reason);
            return;
        }
        AppConfig.trace(this, "RECOVERY-KILL " + reason);
        long delay = WatchdogScheduler.scheduleRecovery(this);
        if (error == null) {
            Log.e(TAG, reason + "; restarting in " + delay + " ms");
        } else {
            Log.e(TAG, reason + "; restarting in " + delay + " ms", error);
        }
        showForegroundNotification(getString(
                R.string.runtime_restarting,
                Math.max(1L, delay / 1000L)));
        terminateProcess(10);
    }

    private static void terminateProcess(int status) {
        android.os.Process.killProcess(android.os.Process.myPid());
        System.exit(status);
    }

    /** 启动截止：nativeStart 挂起（装配死锁/解压卡住）时，健康检查以
     * nativeStarted 为前提空转，onDestroy 提交的 stop 又排在同一 executor 的
     * 挂死任务之后——5 分钟未见就绪即走进程恢复（杀进程由看门狗按退避重启）。 */
    private void scheduleStartDeadline() {
        watchdogExecutor.schedule(() -> {
            if (!nativeStarted && !stopping.get() && startRequested.get()) {
                AppConfig.trace(this, "native-start deadline exceeded");
                requestProcessRecovery("native start deadline exceeded", null);
            }
        }, 5, TimeUnit.MINUTES);
    }

    private void stopNativeRuntimeWithDeadline() {
        if (!nativeStopRequested.compareAndSet(false, true)) {
            return;
        }
        AppConfig.trace(this, "stop-native-runtime begin");
        AtomicBoolean completed = new AtomicBoolean();
        Thread deadline = new Thread(() -> {
            try {
                Thread.sleep(10_000L);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
                return;
            }
            if (!completed.get()) {
                AppConfig.trace(this, "STOP-DEADLINE-KILL native stop > 10s");
                Log.e(TAG, "Native runtime did not stop within 10 seconds; terminating process");
                terminateProcess(0);
            }
        }, "rclaw-stop-deadline");
        deadline.setDaemon(true);
        deadline.start();
        try {
            String error = NativeBridge.nativeStop();
            if (!error.isEmpty()) {
                AppConfig.trace(this, "STOP-ERROR-KILL nativeStop=" + error);
                Log.e(TAG, "Native runtime stop failed: " + error);
                terminateProcess(0);
            }
        } catch (RuntimeException | UnsatisfiedLinkError error) {
            AppConfig.trace(this, "STOP-THREW-KILL " + error);
            Log.e(TAG, "Native runtime stop threw; terminating process", error);
            terminateProcess(0);
        } finally {
            completed.set(true);
            AppConfig.trace(this, "stop-native-runtime done");
        }
        // 停止完成后复位进程级启动标志：ACTION_STOP（或启动成功即停止）之后
        // 同进程内可以再次启动——此前 static 短路会让「停止→再启动」永远失败；
        // nativeStopRequested 一并复位，否则下次 stop 会被 CAS 短路直接跳过。
        PROCESS_NATIVE_START_ATTEMPTED.set(false);
        startRequested.set(false);
        nativeStopRequested.set(false);
    }

    private void installCrashHandler() {
        previousCrashHandler = Thread.getDefaultUncaughtExceptionHandler();
        crashHandler = (thread, error) -> {
            try {
                AppConfig.trace(this, "JAVA-CRASH thread=" + thread.getName()
                        + " " + error);
                if (AppConfig.preferences(this).getBoolean("should_run", false)) {
                    WatchdogScheduler.scheduleRecovery(this);
                }
            } catch (RuntimeException schedulingError) {
                Log.e(TAG, "Unable to schedule crash recovery", schedulingError);
            }
            if (previousCrashHandler != null) {
                previousCrashHandler.uncaughtException(thread, error);
            } else {
                android.os.Process.killProcess(android.os.Process.myPid());
                System.exit(10);
            }
        };
        Thread.setDefaultUncaughtExceptionHandler(crashHandler);
    }

    @Override
    public void onDestroy() {
        if (activeInstance == this) {
            activeInstance = null;
        }
        AppConfig.trace(this, "service-onDestroy should_run="
                + AppConfig.preferences(this).getBoolean("should_run", false));
        stopping.set(true);
        watchdogExecutor.shutdownNow();
        if (Thread.getDefaultUncaughtExceptionHandler() == crashHandler) {
            Thread.setDefaultUncaughtExceptionHandler(previousCrashHandler);
        }
        if (!AppConfig.preferences(this).getBoolean("should_run", false)) {
            WatchdogScheduler.cancel(this);
        }
        releaseActiveNetworkRequest();
        releaseWifiLock();
        releaseLock(startupWakeLock);
        releaseLock(turnWakeLock);
        releaseLock(tickWakeLock);
        runtimeExecutor.execute(() -> {
            try {
                if (nativeStarted || PROCESS_NATIVE_START_ATTEMPTED.get()) {
                    stopNativeRuntimeWithDeadline();
                    nativeStarted = false;
                }
            } finally {
                runtimeExecutor.shutdown();
            }
        });
        stopForeground(STOP_FOREGROUND_REMOVE);
        super.onDestroy();
    }

    /** 常驻资源：钉住物理网络 + 看门狗周期闹钟。wake lock / Wi-Fi 锁不在此列
     *  （按需持有，见 STARTUP/TURN/TICK_WAKE_LOCK_MS 与 updateTurnLocks）。 */
    private void maintainKeepAliveResources() {
        requestActiveNetwork();
        WatchdogScheduler.schedulePeriodic(this);
    }

    /** Rust 运行状态推送驱动的 turn 期锁：busy 期间持 CPU + Wi-Fi 锁（每次推送
     *  续期，租约兜底），回空闲即释放。首次推送也意味着装配完成，启动期锁一并放掉。 */
    private void updateTurnLocks(boolean busy) {
        releaseLock(startupWakeLock);
        if (busy) {
            acquireTimed(turnWakeLock, TURN_WAKE_LOCK_MS);
            acquireWifiLock();
        } else {
            releaseLock(turnWakeLock);
            releaseWifiLock();
        }
    }

    private static PowerManager.WakeLock newWakeLock(PowerManager power, String tag) {
        PowerManager.WakeLock lock = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, tag);
        // 非引用计数：重复 acquire(timeout) 只是把到期时间往后推（续期），release
        // 一次即放。
        lock.setReferenceCounted(false);
        return lock;
    }

    private static void acquireTimed(PowerManager.WakeLock lock, long timeoutMs) {
        if (lock == null) {
            return;
        }
        try {
            lock.acquire(timeoutMs);
        } catch (RuntimeException error) {
            Log.e(TAG, "Unable to acquire wake lock " + lock, error);
        }
    }

    private static void releaseLock(PowerManager.WakeLock lock) {
        if (lock == null) {
            return;
        }
        try {
            if (lock.isHeld()) {
                lock.release();
            }
        } catch (RuntimeException error) {
            Log.w(TAG, "Wake lock was already revoked: " + lock, error);
        }
    }

    @SuppressWarnings("deprecation")
    private void acquireWifiLock() {
        if (wifiLock == null) {
            WifiManager manager = getApplicationContext().getSystemService(WifiManager.class);
            if (manager == null) {
                Log.e(TAG, "WifiManager is unavailable");
                return;
            }
            wifiLock = manager.createWifiLock(
                    WifiManager.WIFI_MODE_FULL_HIGH_PERF,
                    "rclaw:wifi-keepalive");
            wifiLock.setReferenceCounted(false);
        }
        try {
            if (!wifiLock.isHeld()) {
                wifiLock.acquire();
            }
        } catch (RuntimeException error) {
            Log.e(TAG, "Unable to acquire Wi-Fi keep-alive lock", error);
        }
    }

    private void releaseWifiLock() {
        if (wifiLock == null) {
            return;
        }
        try {
            if (wifiLock.isHeld()) {
                wifiLock.release();
            }
        } catch (RuntimeException error) {
            Log.w(TAG, "Wi-Fi keep-alive lock was already revoked", error);
        }
    }

    private void requestActiveNetwork() {
        if (networkRequested) {
            return;
        }
        if (connectivityManager == null) {
            connectivityManager = getSystemService(ConnectivityManager.class);
        }
        if (connectivityManager == null) {
            Log.e(TAG, "ConnectivityManager is unavailable");
            return;
        }
        NetworkRequest request = new NetworkRequest.Builder()
                .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
                .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
                .build();
        try {
            connectivityManager.requestNetwork(request, networkCallback);
            networkRequested = true;
            Log.i(TAG, "Requested Android to keep a physical Internet network available");
        } catch (RuntimeException error) {
            Log.e(TAG, "Unable to request an active network", error);
        }
    }

    private void releaseActiveNetworkRequest() {
        if (!networkRequested || connectivityManager == null) {
            return;
        }
        try {
            connectivityManager.unregisterNetworkCallback(networkCallback);
        } catch (RuntimeException error) {
            Log.w(TAG, "Active network callback was already removed", error);
        } finally {
            networkRequested = false;
        }
    }

    /** Rust android_status 推送入口（JNI 线程调用）：更新前台通知的任务/
     *  空闲文案；服务未运行（理论上 native 就绪时不会）则丢弃。 */
    static void applyRuntimeStatus(String text, boolean busy) {
        RClawService service = activeInstance;
        if (service == null || text == null || text.isEmpty()) {
            return;
        }
        service.runtimeStatusText = text;
        service.runtimeBusy = busy;
        service.mainHandler.post(() -> {
            service.updateTurnLocks(busy);
            service.renderForegroundNotification();
        });
    }

    private void showForegroundNotification(String text) {
        lifecycleText = text;
        renderForegroundNotification();
    }

    /** 合成前台通知：native 就绪且 Rust 推送过状态 → 动态文案（busy turn =
     *  绿色 accent + 不定进度动画）；否则回退服务生命周期文案（启动中/恢复
     *  中/失败，静态）。文案合并放主线程（JNI 线程与生命周期路径并发写）。 */
    private void renderForegroundNotification() {
        boolean dynamic = nativeStarted && runtimeStatusText != null;
        String text = dynamic
                ? runtimeStatusText
                : (lifecycleText != null ? lifecycleText : getString(R.string.runtime_starting));
        boolean busy = dynamic && runtimeBusy;
        // 未加入电池白名单：Doze 下网络被切、wake lock 被忽略，后台通道实际不可用。
        // 提示只进展开态大字（折叠行保持状态文案），引导到 GUI 保活卡片一键开启。
        String bigText = KeepAliveGuide.isBatteryWhitelisted(this)
                ? text
                : text + "\n" + getString(R.string.keepalive_hint);

        Intent open = new Intent(this, MainActivity.class);
        PendingIntent openIntent = PendingIntent.getActivity(
                this,
                0,
                open,
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);

        Intent stop = new Intent(this, RClawService.class).setAction(ACTION_STOP);
        PendingIntent stopIntent = PendingIntent.getService(
                this,
                1,
                stop,
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);

        Notification.Builder builder = new Notification.Builder(this, CHANNEL_ID)
                .setSmallIcon(R.drawable.ic_bagua)
                .setContentTitle(getString(R.string.app_name))
                .setContentText(text)
                .setStyle(new Notification.BigTextStyle().bigText(bigText))
                .setContentIntent(openIntent)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .addAction(new Notification.Action.Builder(
                        null,
                        getString(R.string.stop),
                        stopIntent).build());
        if (busy) {
            // 任务执行中：绿色 accent + 不定进度条（下拉通知栏动画；AOD 熄屏
            // 由系统统一灰度静态渲染八卦图标，动态/着色是平台限制）。
            builder.setColor(0xFF2E9E44);
            builder.setProgress(0, 0, true);
        }
        Notification notification = builder.build();

        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(
                    NOTIFICATION_ID,
                    notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        } else {
            startForeground(NOTIFICATION_ID, notification);
        }
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
