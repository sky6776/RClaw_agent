package ai.rclaw.app;

import android.app.Activity;
import android.app.Instrumentation;
import android.content.Context;
import android.os.Bundle;
import android.system.Os;

import com.chaquo.python.Python;
import com.chaquo.python.android.AndroidPlatform;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.File;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.Base64;
import java.util.Map;
import java.util.regex.Pattern;

/** ADB-only test runner for exercising the APK's real embedded skill runtime. */
public final class SkillScriptInstrumentation extends Instrumentation {
    private static final Pattern RESULT_ID = Pattern.compile("[A-Za-z0-9._-]{1,80}");

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
        File isolatedConfigRoot = null;
        try {
            Context context = getTargetContext();
            RuntimeAssets.installBundledSkills(context);
            File agentRoot = new File(context.getFilesDir(), "agent").getCanonicalFile();
            String scriptRelative = required("script");
            prepareAgentSkill(context, agentRoot, scriptRelative);
            File script = resolveScript(agentRoot, scriptRelative);
            String resultId = required("result_id");
            if (!RESULT_ID.matcher(resultId).matches()) {
                throw new IllegalArgumentException("invalid result_id");
            }
            JSONArray args = decodeArgs(arguments.getString("args_b64", "W10="));

            isolatedConfigRoot = new File(
                    context.getCacheDir(), "rclaw-skill-test-configs").getCanonicalFile();
            recreateDirectory(isolatedConfigRoot, context.getCacheDir().getCanonicalFile());
            prepareSkillConfigs(context, isolatedConfigRoot);
            applyConfigOverride(context, isolatedConfigRoot);

            Os.setenv("RCLAW_SKILL_CONFIG_ROOT", isolatedConfigRoot.getAbsolutePath(), true);
            Os.setenv("RCLAW_ANDROID", "1", true);
            if (!Python.isStarted()) {
                Python.start(new AndroidPlatform(context));
            }
            Python.getInstance().getModule("os")
                    .callAttr("chdir", agentRoot.getAbsolutePath());

            long started = System.currentTimeMillis();
            String raw = new PythonRuntime(context).runScript(
                    script.getAbsolutePath(), args.toString(), agentRoot.getAbsolutePath(),
                    "{}", 0L, 65536L);
            long durationMs = System.currentTimeMillis() - started;

            JSONObject execution = new JSONObject(raw);
            execution.put("script", relativePath(agentRoot, script));
            execution.put("duration_ms", durationMs);
            execution.put("result_id", resultId);
            File resultFile = writeResult(agentRoot, resultId, execution);

            result.putString("result_path", resultFile.getAbsolutePath());
            result.putInt("exit_code", execution.optInt("exit_code", -1));
            result.putInt("stdout_chars", execution.optString("stdout").length());
            result.putInt("stderr_chars", execution.optString("stderr").length());
            result.putLong("duration_ms", durationMs);
            resultCode = Activity.RESULT_OK;
        } catch (Throwable error) {
            result.putString("harness_error", error.getClass().getSimpleName()
                    + ": " + String.valueOf(error.getMessage()));
        } finally {
            if (isolatedConfigRoot != null) {
                try {
                    deleteTree(isolatedConfigRoot, getTargetContext().getCacheDir().getCanonicalFile());
                } catch (IOException ignored) {
                    // The process exits after finish; cache cleanup can be retried by the next run.
                }
            }
            finish(resultCode, result);
        }
    }

    private String required(String key) {
        String value = arguments.getString(key, "");
        if (value.isEmpty()) {
            throw new IllegalArgumentException("missing " + key);
        }
        return value;
    }

    private static JSONArray decodeArgs(String encoded) throws JSONException {
        byte[] decoded = Base64.getDecoder().decode(encoded);
        return new JSONArray(new String(decoded, StandardCharsets.UTF_8));
    }

    private static File resolveScript(File agentRoot, String relative) throws IOException {
        if (!relative.startsWith("skills/")
                || !relative.endsWith(".py")
                || relative.contains("\\")
                || relative.contains("..")) {
            throw new IllegalArgumentException("script must be a safe skills/.../*.py path");
        }
        File script = new File(agentRoot, relative).getCanonicalFile();
        String root = agentRoot.getCanonicalPath() + File.separator;
        if (!script.getPath().startsWith(root) || !script.isFile()) {
            throw new IOException("skill script not found: " + relative);
        }
        return script;
    }

    private static void prepareAgentSkill(Context context, File agentRoot, String relative)
            throws IOException {
        if (!relative.startsWith("skills/")
                || !relative.endsWith(".py")
                || relative.contains("\\")
                || relative.contains("..")) {
            throw new IllegalArgumentException("script must be a safe skills/.../*.py path");
        }
        String remainder = relative.substring("skills/".length());
        int separator = remainder.indexOf('/');
        if (separator <= 0) {
            throw new IllegalArgumentException("script must identify a bundled skill");
        }
        String skillName = remainder.substring(0, separator);
        File bundledRoot = new File(
                context.getFilesDir(), "runtime/bundled-skills").getCanonicalFile();
        File source = new File(bundledRoot, skillName).getCanonicalFile();
        File destination = new File(agentRoot, "skills/" + skillName).getCanonicalFile();
        if (!source.getPath().startsWith(bundledRoot.getPath() + File.separator)
                || !source.isDirectory()) {
            throw new IOException("bundled skill not found: " + skillName);
        }
        if (!destination.getPath().startsWith(agentRoot.getPath() + File.separator)) {
            throw new IOException("test skill escapes agent root");
        }
        if (!destination.exists()) {
            copyTree(source, destination);
        }
    }

    private static void copyTree(File source, File destination) throws IOException {
        if (source.isDirectory()) {
            if (!destination.isDirectory() && !destination.mkdirs()) {
                throw new IOException("unable to create test skill directory");
            }
            File[] children = source.listFiles();
            if (children == null) {
                throw new IOException("unable to list bundled skill directory");
            }
            for (File child : children) {
                copyTree(child, new File(destination, child.getName()));
            }
            return;
        }
        Files.copy(source.toPath(), destination.toPath());
    }

    private static void prepareSkillConfigs(Context context, File destination) throws IOException {
        String root = destination.getCanonicalPath() + File.separator;
        for (Map.Entry<String, String> entry : AppConfig.loadSkillConfigs(context).entrySet()) {
            File target = new File(destination, entry.getKey()).getCanonicalFile();
            if (!target.getPath().startsWith(root)) {
                throw new IOException("skill config escapes isolated test root");
            }
            writePrivate(target, entry.getValue());
        }
    }

    private void applyConfigOverride(Context context, File destination) throws IOException {
        String path = arguments.getString("skill_config_path", "");
        String encoded = arguments.getString("skill_config_b64", "");
        if (path.isEmpty() && encoded.isEmpty()) {
            return;
        }
        if (path.isEmpty() || encoded.isEmpty()) {
            throw new IllegalArgumentException("skill config override requires path and content");
        }
        if (!AppConfig.loadSkillConfigs(context).containsKey(path)) {
            throw new IllegalArgumentException("unknown skill config path");
        }
        byte[] content = Base64.getDecoder().decode(encoded);
        if (content.length > RuntimeAssets.MAX_SKILL_CONFIG_BYTES) {
            throw new IllegalArgumentException("skill config override exceeds size limit");
        }
        File target = new File(destination, path).getCanonicalFile();
        String root = destination.getCanonicalPath() + File.separator;
        if (!target.getPath().startsWith(root)) {
            throw new IOException("skill config override escapes isolated test root");
        }
        writePrivate(target, new String(content, StandardCharsets.UTF_8));
    }

    private static void writePrivate(File destination, String content) throws IOException {
        File parent = destination.getParentFile();
        if (parent == null || (!parent.isDirectory() && !parent.mkdirs())) {
            throw new IOException("unable to create test config directory");
        }
        Files.write(destination.toPath(), content.getBytes(StandardCharsets.UTF_8));
    }

    private static File writeResult(File agentRoot, String resultId, JSONObject execution)
            throws IOException {
        File directory = new File(agentRoot, "tmp/skill-test-results").getCanonicalFile();
        String root = agentRoot.getCanonicalPath() + File.separator;
        if (!directory.getPath().startsWith(root)) {
            throw new IOException("test result directory escapes agent root");
        }
        if (!directory.isDirectory() && !directory.mkdirs()) {
            throw new IOException("unable to create skill test result directory");
        }
        File result = new File(directory, resultId + ".json").getCanonicalFile();
        if (!result.getPath().startsWith(directory.getPath() + File.separator)) {
            throw new IOException("test result path escapes result directory");
        }
        Files.write(result.toPath(), execution.toString().getBytes(StandardCharsets.UTF_8));
        return result;
    }

    private static String relativePath(File root, File child) {
        return root.toPath().relativize(child.toPath()).toString().replace('\\', '/');
    }

    private static void recreateDirectory(File target, File allowedRoot) throws IOException {
        if (target.exists()) {
            deleteTree(target, allowedRoot);
        }
        if (!target.mkdirs()) {
            throw new IOException("unable to create isolated test config root");
        }
    }

    private static void deleteTree(File target, File allowedRoot) throws IOException {
        File canonicalTarget = target.getCanonicalFile();
        String prefix = allowedRoot.getCanonicalPath() + File.separator;
        if (!canonicalTarget.getPath().startsWith(prefix)) {
            throw new IOException("refusing to delete outside test cache");
        }
        File[] children = canonicalTarget.listFiles();
        if (children != null) {
            for (File child : children) {
                deleteTree(child, allowedRoot);
            }
        }
        if (canonicalTarget.exists() && !canonicalTarget.delete()) {
            throw new IOException("unable to clean test cache: " + canonicalTarget.getName());
        }
    }
}
