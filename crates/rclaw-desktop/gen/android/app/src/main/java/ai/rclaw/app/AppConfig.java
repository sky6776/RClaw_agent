package ai.rclaw.app;

import android.content.Context;
import android.content.SharedPreferences;

import java.io.File;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.HashMap;
import java.util.Map;

import org.json.JSONObject;

/**
 * v2 轻量配置壳：配置内容回归 filesDir/rclaw.toml（桌面 GUI 向导/设置页编辑，
 * 旧版 SharedPreferences 逐项配置层已随 v0.2.0 UI 一起删除）。这里只保留：
 * - 保活链路的本地偏好（should_run / watchdog 退避状态，与 rclaw.toml 解耦）；
 * - nativeStart 所需的 AndroidConfig 启动 JSON 组装（读固定路径，缺配置即待配置态）。
 */
public final class AppConfig {
    private AppConfig() {}

    public static SharedPreferences preferences(Context context) {
        return context.getSharedPreferences("rclaw_service", Context.MODE_PRIVATE);
    }

    /** 生命周期取证：vivo 裁剪 logcat，杀进程/健康检查的判定只能落盘才可回溯
     *  （files/lifecycle_trace.txt，run-as 读取）。追加写，单行一条；超过
     *  256KB 直接清空重来（取证日志可丢弃，防无限增长占满 files）。 */
    public static void trace(Context context, String message) {
        try {
            File file = new File(context.getFilesDir(), "lifecycle_trace.txt");
            if (file.length() > 256 * 1024L) {
                file.delete();
            }
            String line = String.format("%1$tF %1$tT.%1$tL pid=%2$d %3$s%n",
                    System.currentTimeMillis(), android.os.Process.myPid(), message);
            Files.write(file.toPath(),
                    line.getBytes(StandardCharsets.UTF_8),
                    java.nio.file.StandardOpenOption.CREATE,
                    java.nio.file.StandardOpenOption.APPEND);
        } catch (IOException ignored) {
        }
    }

    /** 组装 Rust AndroidConfig 启动 JSON；rclaw.toml 缺失时 toml_config 为 null（待配置态）。 */
    public static String toJson(Context context) {
        File filesDir = context.getFilesDir();
        File noBackup = new File(filesDir.getParentFile(), "no_backup");
        File skillConfigsDir = new File(noBackup, "rclaw-skill-configs");
        JSONObject json = new JSONObject();
        try {
            json.put("files_dir", filesDir.getAbsolutePath());
            json.put("workspace_dir", new File(filesDir, "rclaw").getAbsolutePath());
            json.put("bundled_skills_dir",
                    new File(filesDir, "runtime/bundled-skills").getAbsolutePath());
            json.put("skill_configs_dir", skillConfigsDir.getAbsolutePath());
            File tomlFile = new File(filesDir, "rclaw.toml");
            if (tomlFile.isFile()) {
                String toml = new String(
                        Files.readAllBytes(tomlFile.toPath()), StandardCharsets.UTF_8);
                json.put("toml_config", toml);
            }
            JSONObject configs = new JSONObject();
            for (Map.Entry<String, String> entry : readSkillConfigs(skillConfigsDir).entrySet()) {
                configs.put(entry.getKey(), entry.getValue());
            }
            json.put("skill_configs", configs);
            // 审批表由 Rust 侧（rclaw-skill-approvals.json）持久化，无需经启动 JSON 传递。
            json.put("approved_skill_scripts", new JSONObject());
        } catch (org.json.JSONException | IOException error) {
            throw new IllegalStateException("Unable to assemble launch config", error);
        }
        return json.toString();
    }

    /** 兼容旧 instrumentation 的读取入口（与 readSkillConfigs 同义）。 */
    public static Map<String, String> loadSkillConfigs(Context context) {
        File skillConfigsDir = new File(
                new File(context.getFilesDir().getParentFile(), "no_backup"),
                "rclaw-skill-configs");
        return readSkillConfigs(skillConfigsDir);
    }

    private static Map<String, String> readSkillConfigs(File root) {
        Map<String, String> result = new HashMap<>();
        File[] entries = root.listFiles();
        if (entries == null) {
            return result;
        }
        for (File entry : entries) {
            if (entry.isFile() && entry.getName().endsWith(".toml")) {
                try {
                    result.put(entry.getName(),
                            new String(Files.readAllBytes(entry.toPath()), StandardCharsets.UTF_8));
                } catch (IOException ignored) {
                    // 单个配置读失败跳过；Rust 侧对未知键有校验。
                }
            } else if (entry.isDirectory()) {
                for (File inner : entry.listFiles() == null ? new File[0] : entry.listFiles()) {
                    if (inner.isFile() && inner.getName().endsWith(".toml")) {
                        try {
                            result.put(entry.getName() + "/" + inner.getName(),
                                    new String(Files.readAllBytes(inner.toPath()),
                                            StandardCharsets.UTF_8));
                        } catch (IOException ignored) {
                        }
                    }
                }
            }
        }
        return result;
    }
}
