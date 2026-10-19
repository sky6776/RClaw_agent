package ai.rclaw.app;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

public final class BootReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context context, Intent intent) {
        String action = intent.getAction();
        if (!Intent.ACTION_BOOT_COMPLETED.equals(action)
                && !Intent.ACTION_MY_PACKAGE_REPLACED.equals(action)) {
            return;
        }
        purgeUpdateApks(context);
        if (AppConfig.preferences(context).getBoolean("should_run", false)) {
            WatchdogScheduler.schedulePeriodic(context);
            Intent service = new Intent(context, RClawService.class)
                    .setAction(RClawService.ACTION_RECOVER);
            try {
                context.startForegroundService(service);
            } catch (RuntimeException error) {
                WatchdogScheduler.schedulePeriodic(context);
            }
        }
    }

    /** 清理自更新安装包残留（files/update/，220MB 级/个）。安装完成（
     *  MY_PACKAGE_REPLACED）后安装包即无用——此时应用刚被系统替换，正好清掉
     *  本次安装拉起的那个包；开机时兜底清历史残留。只删 update 目录里的普通
     *  文件，失败忽略（下次开机/更新下载前再清）。 */
    private static void purgeUpdateApks(Context context) {
        try {
            java.io.File dir = new java.io.File(context.getFilesDir(), "update");
            java.io.File[] files = dir.listFiles();
            if (files == null) {
                return;
            }
            for (java.io.File file : files) {
                if (file.isFile()) {
                    java.nio.file.Files.deleteIfExists(file.toPath());
                }
            }
        } catch (Exception error) {
            android.util.Log.w("BootReceiver", "purge update apks failed", error);
        }
    }
}
