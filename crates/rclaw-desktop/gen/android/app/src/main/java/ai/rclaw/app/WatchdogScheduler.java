package ai.rclaw.app;

import android.app.AlarmManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.os.Build;
import android.os.SystemClock;
import android.util.Log;

final class WatchdogScheduler {
    private static final String TAG = "RClawWatchdog";
    private static final String ACTION_CHECK = "ai.rclaw.app.WATCHDOG_CHECK";
    private static final String KEY_LAST_HEARTBEAT = "watchdog_last_heartbeat_ms";
    private static final String KEY_FAILURE_COUNT = "watchdog_failure_count";
    private static final String KEY_LAST_FAILURE = "watchdog_last_failure_ms";
    private static final String KEY_RECOVERY_NOT_BEFORE = "watchdog_recovery_not_before_ms";
    static final long CHECK_INTERVAL_MS = 10 * 60 * 1000L;
    private static final long HEARTBEAT_STALE_MS = 3 * 60 * 1000L;
    private static final long FAILURE_WINDOW_MS = 30 * 60 * 1000L;
    private static final long STABLE_RESET_MS = 10 * 60 * 1000L;
    private static final long[] RECOVERY_DELAYS_MS = {
            10_000L,
            30_000L,
            2 * 60 * 1000L,
            10 * 60 * 1000L,
    };
    private static final long MAX_RECOVERY_DELAY_MS =
            RECOVERY_DELAYS_MS[RECOVERY_DELAYS_MS.length - 1];

    private WatchdogScheduler() {}

    static void schedulePeriodic(Context context) {
        AlarmManager alarms = context.getSystemService(AlarmManager.class);
        if (alarms == null) {
            Log.e(TAG, "AlarmManager is unavailable");
            return;
        }
        long triggerAt = System.currentTimeMillis() + CHECK_INTERVAL_MS;
        try {
            alarms.setAlarmClock(
                    new AlarmManager.AlarmClockInfo(triggerAt, alarmClockShowIntent(context)),
                    watchdogIntent(context));
        } catch (SecurityException error) {
            Log.w(TAG, "Alarm-clock scheduling was denied; falling back", error);
            alarms.setAndAllowWhileIdle(
                    AlarmManager.RTC_WAKEUP,
                    triggerAt,
                    watchdogIntent(context));
        }
    }

    static void scheduleAfter(Context context, long delayMs) {
        AlarmManager alarms = context.getSystemService(AlarmManager.class);
        if (alarms == null) {
            Log.e(TAG, "AlarmManager is unavailable");
            return;
        }
        long triggerAt = SystemClock.elapsedRealtime() + Math.max(1_000L, delayMs);
        scheduleIdleAlarm(alarms, triggerAt, watchdogIntent(context));
    }

    static void scheduleConfigApply(Context context) {
        AlarmManager alarms = context.getSystemService(AlarmManager.class);
        if (alarms == null) {
            throw new IllegalStateException("AlarmManager is unavailable");
        }
        scheduleIdleAlarm(
                alarms,
                SystemClock.elapsedRealtime() + 1_000L,
                configApplyIntent(context));
    }

    static long scheduleRecovery(Context context) {
        SharedPreferences prefs = AppConfig.preferences(context);
        long now = System.currentTimeMillis();
        long lastFailure = prefs.getLong(KEY_LAST_FAILURE, 0L);
        int failureCount = prefs.getInt(KEY_FAILURE_COUNT, 0);
        if (lastFailure == 0L || now - lastFailure > FAILURE_WINDOW_MS || now < lastFailure) {
            failureCount = 0;
        }
        failureCount++;
        long delay = RECOVERY_DELAYS_MS[Math.min(
                failureCount - 1,
                RECOVERY_DELAYS_MS.length - 1)];
        prefs.edit()
                .putInt(KEY_FAILURE_COUNT, failureCount)
                .putLong(KEY_LAST_FAILURE, now)
                .putLong(KEY_LAST_HEARTBEAT, 0L)
                .putLong(KEY_RECOVERY_NOT_BEFORE, now + delay)
                .commit();
        scheduleAfter(context, delay);
        return delay;
    }

    static void markHealthy(Context context, long stableForMs) {
        SharedPreferences.Editor editor = AppConfig.preferences(context)
                .edit()
                .putLong(KEY_LAST_HEARTBEAT, System.currentTimeMillis())
                .remove(KEY_RECOVERY_NOT_BEFORE);
        if (stableForMs >= STABLE_RESET_MS) {
            editor.remove(KEY_FAILURE_COUNT).remove(KEY_LAST_FAILURE);
        }
        editor.apply();
    }

    static boolean heartbeatIsFresh(Context context) {
        long last = lastHeartbeatAt(context);
        if (last == 0L) {
            return false;
        }
        long now = System.currentTimeMillis();
        return now >= last && now - last <= HEARTBEAT_STALE_MS;
    }

    static long lastHeartbeatAt(Context context) {
        return AppConfig.preferences(context).getLong(KEY_LAST_HEARTBEAT, 0L);
    }

    static long remainingRecoveryDelay(Context context) {
        SharedPreferences prefs = AppConfig.preferences(context);
        long notBefore = prefs.getLong(KEY_RECOVERY_NOT_BEFORE, 0L);
        long lastFailure = prefs.getLong(KEY_LAST_FAILURE, 0L);
        long now = System.currentTimeMillis();
        long remaining = notBefore - now;
        if ((lastFailure != 0L && now < lastFailure)
                || remaining > MAX_RECOVERY_DELAY_MS) {
            prefs.edit().remove(KEY_RECOVERY_NOT_BEFORE).apply();
            return 0L;
        }
        return Math.max(0L, remaining);
    }

    /** 连续恢复失败是否达到熔断阈值（计数由 scheduleRecovery 维护，窗口
     *  30 分钟；显式启动 resetForExplicitStart 会清零重新计数）。 */
    static boolean recoveryExhausted(Context context) {
        return AppConfig.preferences(context).getInt(KEY_FAILURE_COUNT, 0) >= 6;
    }

    static void resetForExplicitStart(Context context) {
        AppConfig.preferences(context).edit()
                .remove(KEY_FAILURE_COUNT)
                .remove(KEY_LAST_FAILURE)
                .remove(KEY_RECOVERY_NOT_BEFORE)
                .putLong(KEY_LAST_HEARTBEAT, 0L)
                .apply();
    }

    static void cancel(Context context) {
        AlarmManager alarms = context.getSystemService(AlarmManager.class);
        if (alarms != null) {
            alarms.cancel(watchdogIntent(context));
            alarms.cancel(configApplyIntent(context));
        }
        AppConfig.preferences(context).edit()
                .remove(KEY_FAILURE_COUNT)
                .remove(KEY_LAST_FAILURE)
                .remove(KEY_RECOVERY_NOT_BEFORE)
                .remove(KEY_LAST_HEARTBEAT)
                .apply();
    }

    static void startRecoveryService(Context context) {
        startForegroundService(context, RClawService.ACTION_RECOVER);
    }

    static void startKeepAliveService(Context context) {
        startForegroundService(context, RClawService.ACTION_KEEP_ALIVE);
    }

    private static void startForegroundService(Context context, String action) {
        Intent service = new Intent(context, RClawService.class)
                .setAction(action);
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(service);
            } else {
                context.startService(service);
            }
        } catch (RuntimeException error) {
            Log.e(TAG, "Unable to start recovery foreground service", error);
            schedulePeriodic(context);
        }
    }

    private static PendingIntent watchdogIntent(Context context) {
        Intent intent = new Intent(context, WatchdogReceiver.class).setAction(ACTION_CHECK);
        return PendingIntent.getBroadcast(
                context,
                2001,
                intent,
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
    }

    private static PendingIntent alarmClockShowIntent(Context context) {
        Intent intent = new Intent(context, MainActivity.class)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TOP);
        return PendingIntent.getActivity(
                context,
                2003,
                intent,
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
    }

    private static void scheduleIdleAlarm(
            AlarmManager alarms,
            long triggerAt,
            PendingIntent operation) {
        boolean exactAllowed = Build.VERSION.SDK_INT < 31 || alarms.canScheduleExactAlarms();
        if (exactAllowed) {
            try {
                alarms.setExactAndAllowWhileIdle(
                        AlarmManager.ELAPSED_REALTIME_WAKEUP,
                        triggerAt,
                        operation);
                return;
            } catch (SecurityException error) {
                Log.w(TAG, "Exact alarm permission changed; falling back", error);
            }
        }
        alarms.setAndAllowWhileIdle(
                AlarmManager.ELAPSED_REALTIME_WAKEUP,
                triggerAt,
                operation);
    }

    private static PendingIntent configApplyIntent(Context context) {
        Intent intent = new Intent(context, RClawService.class)
                .setAction(RClawService.ACTION_APPLY_CONFIG);
        return PendingIntent.getForegroundService(
                context,
                2002,
                intent,
                PendingIntent.FLAG_UPDATE_CURRENT
                        | PendingIntent.FLAG_IMMUTABLE
                        | PendingIntent.FLAG_ONE_SHOT);
    }
}
