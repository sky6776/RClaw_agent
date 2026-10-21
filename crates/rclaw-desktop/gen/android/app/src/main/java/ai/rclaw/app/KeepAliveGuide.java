package ai.rclaw.app;

import android.content.ActivityNotFoundException;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.net.Uri;
import android.os.Build;
import android.os.PowerManager;
import android.provider.Settings;
import android.util.Log;

import org.json.JSONException;
import org.json.JSONObject;

import java.util.Locale;

/**
 * 保活引导：电池优化白名单（AOSP Doze）与厂商自启动/后台管理页跳转。
 *
 * 前台服务 + 看门狗挡不住两件事：Doze 期间非白名单应用的 wake lock 被忽略、
 * 网络被切；国产 ROM（EMUI/MIUI/ColorOS/Funtouch）的自启动管理会连前台服务和
 * AlarmManager 一起拦。两者都只能由用户点一次系统设置，这里只负责查状态和
 * 把用户送到正确的页面。入口：GUI「关于与设置 → 后台保活」卡片（经
 * NativeBridge.keepAlive* JNI）、前台通知的未白名单提示。
 */
final class KeepAliveGuide {
    private static final String TAG = "KeepAliveGuide";

    private KeepAliveGuide() {}

    /** 厂商自启动/后台管理页候选（按 manufacturer 匹配，逐个 resolve 取首个可达）。
     *  组件名随 ROM 版本变动，表里同一厂商放多条，全部不可达时回退应用详情页。 */
    private static final String[][] OEM_COMPONENTS = {
            // Xiaomi / Redmi / POCO（MIUI / HyperOS）
            {"xiaomi", "com.miui.securitycenter",
                    "com.miui.permcenter.autostart.AutoStartManagementActivity"},
            {"redmi", "com.miui.securitycenter",
                    "com.miui.permcenter.autostart.AutoStartManagementActivity"},
            {"poco", "com.miui.securitycenter",
                    "com.miui.permcenter.autostart.AutoStartManagementActivity"},
            // Huawei / Honor（EMUI / HarmonyOS / MagicOS）
            {"huawei", "com.huawei.systemmanager",
                    "com.huawei.systemmanager.startupmgr.ui.StartupNormalAppListActivity"},
            {"huawei", "com.huawei.systemmanager",
                    "com.huawei.systemmanager.appcontrol.activity.StartupAppControlActivity"},
            {"huawei", "com.huawei.systemmanager",
                    "com.huawei.systemmanager.optimize.process.ProtectActivity"},
            {"honor", "com.hihonor.systemmanager",
                    "com.hihonor.systemmanager.startupmgr.ui.StartupNormalAppListActivity"},
            {"honor", "com.huawei.systemmanager",
                    "com.huawei.systemmanager.startupmgr.ui.StartupNormalAppListActivity"},
            {"honor", "com.huawei.systemmanager",
                    "com.huawei.systemmanager.appcontrol.activity.StartupAppControlActivity"},
            // OPPO / realme / OnePlus（ColorOS）
            {"oppo", "com.coloros.safecenter",
                    "com.coloros.safecenter.permission.startup.StartupAppListActivity"},
            {"oppo", "com.coloros.safecenter",
                    "com.coloros.safecenter.startupapp.StartupAppListActivity"},
            {"oppo", "com.oppo.safe",
                    "com.oppo.safe.permission.startup.StartupAppListActivity"},
            {"oppo", "com.coloros.oppoguardelf",
                    "com.coloros.powermanager.fuelgaue.PowerUsageModelActivity"},
            {"realme", "com.coloros.safecenter",
                    "com.coloros.safecenter.permission.startup.StartupAppListActivity"},
            {"realme", "com.coloros.safecenter",
                    "com.coloros.safecenter.startupapp.StartupAppListActivity"},
            {"oneplus", "com.oneplus.security",
                    "com.oneplus.security.chainlaunch.view.ChainLaunchAppListActivity"},
            {"oneplus", "com.coloros.safecenter",
                    "com.coloros.safecenter.permission.startup.StartupAppListActivity"},
            // vivo / iQOO（Funtouch OS / OriginOS）
            {"vivo", "com.vivo.permissionmanager",
                    "com.vivo.permissionmanager.activity.BgStartUpManagerActivity"},
            {"vivo", "com.iqoo.secure",
                    "com.iqoo.secure.ui.phoneoptimize.BgStartUpManager"},
            {"vivo", "com.iqoo.secure",
                    "com.iqoo.secure.ui.phoneoptimize.AddWhiteListActivity"},
            {"vivo", "com.vivo.permissionmanager",
                    "com.vivo.permissionmanager.activity.PurviewTabActivity"},
            {"iqoo", "com.vivo.permissionmanager",
                    "com.vivo.permissionmanager.activity.BgStartUpManagerActivity"},
            {"iqoo", "com.iqoo.secure",
                    "com.iqoo.secure.ui.phoneoptimize.BgStartUpManager"},
            // Samsung（One UI 电池 → 后台使用限制）
            {"samsung", "com.samsung.android.lool",
                    "com.samsung.android.sm.battery.ui.BatteryActivity"},
            {"samsung", "com.samsung.android.lool",
                    "com.samsung.android.sm.ui.battery.BatteryActivity"},
            // Meizu（Flyme）
            {"meizu", "com.meizu.safe", "com.meizu.safe.permission.SmartBGActivity"},
            {"meizu", "com.meizu.safe", "com.meizu.safe.security.SHOW_APPSEC"},
            // Lenovo / ZUK / Letv / Asus / Nokia（少量存量机型）
            {"lenovo", "com.lenovo.security",
                    "com.lenovo.security.purebackground.PureBackgroundActivity"},
            {"letv", "com.letv.android.letvsafe",
                    "com.letv.android.letvsafe.AutobootManageActivity"},
            {"asus", "com.asus.mobilemanager",
                    "com.asus.mobilemanager.autostart.AutoStartActivity"},
            {"nokia", "com.evenwell.powersaving.g3",
                    "com.evenwell.powersaving.g3.exception.PowerSaverExceptionActivity"},
    };

    /** 是否已加入电池优化白名单（Doze 期间 wake lock / 网络才真正生效）。 */
    static boolean isBatteryWhitelisted(Context context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) {
            return true;
        }
        PowerManager manager = context.getSystemService(PowerManager.class);
        if (manager == null) {
            return true;
        }
        try {
            return manager.isIgnoringBatteryOptimizations(context.getPackageName());
        } catch (RuntimeException error) {
            Log.w(TAG, "isIgnoringBatteryOptimizations failed", error);
            return true;
        }
    }

    /** 拉起系统「忽略电池优化」确认弹窗（REQUEST_IGNORE_BATTERY_OPTIMIZATIONS，
     *  Manifest 已声明）；个别 ROM 禁掉该弹窗时退到电池优化列表页。 */
    static boolean requestBatteryWhitelist(Context context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) {
            return false;
        }
        Intent direct = new Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS)
                .setData(Uri.parse("package:" + context.getPackageName()))
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        if (startSafely(context, direct)) {
            return true;
        }
        Intent list = new Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        return startSafely(context, list);
    }

    /** 厂商自启动/后台管理页：按 manufacturer 匹配候选组件，首个可达者即启动；
     *  全部不可达（原生 Android / 未知厂商）回退到本应用的应用详情页。 */
    static boolean openOemSettings(Context context) {
        PackageManager pm = context.getPackageManager();
        String manufacturer = manufacturer();
        for (String[] entry : OEM_COMPONENTS) {
            if (!manufacturer.contains(entry[0])) {
                continue;
            }
            Intent intent = new Intent()
                    .setComponent(new ComponentName(entry[1], entry[2]))
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
            if (pm.resolveActivity(intent, 0) == null) {
                continue;
            }
            if (startSafely(context, intent)) {
                return true;
            }
        }
        Intent details = new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS)
                .setData(Uri.parse("package:" + context.getPackageName()))
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        return startSafely(context, details);
    }

    /** 本机是否有可达的厂商自启动页（GUI 据此决定按钮文案：厂商页 / 应用详情）。 */
    static boolean hasOemSettings(Context context) {
        PackageManager pm = context.getPackageManager();
        String manufacturer = manufacturer();
        for (String[] entry : OEM_COMPONENTS) {
            if (!manufacturer.contains(entry[0])) {
                continue;
            }
            Intent intent = new Intent().setComponent(new ComponentName(entry[1], entry[2]));
            if (pm.resolveActivity(intent, 0) != null) {
                return true;
            }
        }
        return false;
    }

    /** GUI 保活卡片的状态快照（JSON，经 NativeBridge.keepAliveStatus 返回 Rust）。 */
    static String statusJson(Context context) {
        JSONObject json = new JSONObject();
        try {
            json.put("battery_whitelisted", isBatteryWhitelisted(context));
            json.put("manufacturer", Build.MANUFACTURER == null ? "" : Build.MANUFACTURER);
            json.put("oem_settings", hasOemSettings(context));
        } catch (JSONException error) {
            throw new IllegalStateException("Unable to serialize keep-alive status", error);
        }
        return json.toString();
    }

    private static String manufacturer() {
        String value = Build.MANUFACTURER == null ? "" : Build.MANUFACTURER;
        return value.toLowerCase(Locale.ROOT);
    }

    private static boolean startSafely(Context context, Intent intent) {
        try {
            context.startActivity(intent);
            return true;
        } catch (ActivityNotFoundException | SecurityException error) {
            Log.w(TAG, "settings activity unavailable: " + intent, error);
            return false;
        } catch (RuntimeException error) {
            Log.w(TAG, "settings activity launch failed: " + intent, error);
            return false;
        }
    }
}
