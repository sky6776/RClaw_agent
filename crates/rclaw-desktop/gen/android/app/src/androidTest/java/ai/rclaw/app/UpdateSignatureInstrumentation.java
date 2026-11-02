package ai.rclaw.app;

import android.app.Activity;
import android.app.Instrumentation;
import android.content.Context;
import android.os.Bundle;
import android.util.Log;

/**
 * ADB-only instrumentation: exercises NativeBridge.verifyUpdateApk on a real
 * device before release (the update-time signing gate). Not shipped.
 *
 * The installed app must be a debug/androidTest build signed with the same key
 * as this test APK. Usage:
 *
 *   # own APK (installed path) -> "ok"
 *   adb shell am instrument -w -e apk /data/app/.../base.apk \
 *     ai.rclaw.app.test/ai.rclaw.app.UpdateSignatureInstrumentation
 *   # a mismatched APK copied into the app's external files dir -> 签名不一致
 *   adb shell am instrument -w -e apk \
 *     /sdcard/Android/data/ai.rclaw.app/files/mismatch.apk \
 *     ai.rclaw.app.test/ai.rclaw.app.UpdateSignatureInstrumentation
 *
 * The verdict is written to the result bundle ("verdict"/"ok") and logcat tag
 * UpdateSignatureTest; RESULT_OK means "ok", RESULT_CANCELED means a blocking
 * verdict was produced (expected for the mismatch case).
 */
public final class UpdateSignatureInstrumentation extends Instrumentation {
    private static final String TAG = "UpdateSignatureTest";

    private Bundle arguments;

    @Override
    public void onCreate(Bundle arguments) {
        super.onCreate(arguments);
        this.arguments = arguments == null ? new Bundle() : arguments;
        start();
    }

    @Override
    public void onStart() {
        Bundle result = new Bundle();
        int resultCode = Activity.RESULT_CANCELED;
        try {
            Context context = getTargetContext();
            // NativeBridge.appContext 平时由 MainActivity / RClawService 写入；
            // instrumentation 进程里两者都不启动，这里反射注入（setContext 是
            // 包内可见，避免为测试面改动生产桥的可见性）。
            java.lang.reflect.Method setContext = NativeBridge.class.getDeclaredMethod(
                    "setContext", Context.class);
            setContext.setAccessible(true);
            setContext.invoke(null, context);
            String apk = arguments.getString("apk");
            if (apk == null || apk.isEmpty()) {
                apk = context.getPackageCodePath();
            }
            String verdict = NativeBridge.verifyUpdateApk(apk);
            boolean ok = "ok".equals(verdict);
            result.putString("verdict", verdict);
            result.putBoolean("ok", ok);
            Log.i(TAG, "verifyUpdateApk(" + apk + ") = " + verdict);
            resultCode = ok ? Activity.RESULT_OK : Activity.RESULT_CANCELED;
        } catch (Throwable error) {
            result.putString("error", String.valueOf(error));
            Log.e(TAG, "verifyUpdateApk instrumentation failed", error);
        }
        finish(resultCode, result);
    }
}
