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
}
