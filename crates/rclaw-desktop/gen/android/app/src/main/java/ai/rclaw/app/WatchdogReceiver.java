package ai.rclaw.app;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

public final class WatchdogReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context context, Intent intent) {
        if (!AppConfig.preferences(context).getBoolean("should_run", false)) {
            WatchdogScheduler.cancel(context);
            return;
        }

        long remainingDelay = WatchdogScheduler.remainingRecoveryDelay(context);
        if (remainingDelay > 0L) {
            WatchdogScheduler.scheduleAfter(context, remainingDelay);
            return;
        }

        WatchdogScheduler.schedulePeriodic(context);
        WatchdogScheduler.startKeepAliveService(context);
    }
}
