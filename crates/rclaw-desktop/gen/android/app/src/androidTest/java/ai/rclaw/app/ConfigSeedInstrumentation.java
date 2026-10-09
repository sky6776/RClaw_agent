package ai.rclaw.app;

import android.app.Activity;
import android.app.Instrumentation;
import android.content.Context;
import android.os.Bundle;
import android.util.Log;

import java.io.File;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.StandardCopyOption;
import java.util.ArrayList;
import java.util.List;

/**
 * ADB-only config seeding (debug/androidTest builds only, never shipped).
 *
 * run-as is unavailable on some EMUI builds ("wrong owner"), so this
 * instrumentation copies config files into app-private storage from the
 * app's own external seed directory:
 *   getExternalFilesDir(null)/rclaw_seed/rclaw.toml          -> filesDir/rclaw.toml
 *   getExternalFilesDir(null)/rclaw_seed/files/<rel>         -> filesDir/<rel>
 *   getExternalFilesDir(null)/rclaw_seed/no_backup/<rel>     -> <dataDir>/no_backup/<rel>
 *
 * The app can always read its own external dir (no storage permission), and
 * adb can push there. Idempotent: existing destination files are overwritten.
 * Results are reported per file via logcat (tag ConfigSeed) and the result
 * bundle ("seeded" / "error").
 */
public final class ConfigSeedInstrumentation extends Instrumentation {
    private static final String TAG = "ConfigSeed";

    @Override
    public void onCreate(Bundle arguments) {
        super.onCreate(arguments);
        start();
    }

    @Override
    public void onStart() {
        Bundle result = new Bundle();
        int code = Activity.RESULT_CANCELED;
        try {
            Context context = getTargetContext();
            File seedRoot = new File(context.getExternalFilesDir(null), "rclaw_seed")
                    .getCanonicalFile();
            if (!seedRoot.isDirectory()) {
                throw new IllegalStateException("seed dir missing: " + seedRoot);
            }
            File filesDir = context.getFilesDir().getCanonicalFile();
            File dataDir = filesDir.getParentFile();
            if (dataDir == null) throw new IllegalStateException("no data dir parent");

            List<String> seeded = new ArrayList<>();
            File seedToml = new File(seedRoot, "rclaw.toml");
            if (seedToml.isFile()) {
                copyFile(seedToml, new File(filesDir, "rclaw.toml"));
                seeded.add("files/rclaw.toml");
            }
            copyTree(new File(seedRoot, "files"), filesDir, seeded);
            copyTree(new File(seedRoot, "no_backup"), new File(dataDir, "no_backup"), seeded);

            if (seeded.isEmpty()) {
                throw new IllegalStateException("nothing seeded - seed dir has no files");
            }
            result.putString("seeded", String.join("\n", seeded));
            code = Activity.RESULT_OK;
            Log.i(TAG, "seeded " + seeded.size() + " file(s)");
        } catch (Exception e) {
            Log.e(TAG, "seed failed", e);
            result.putString("error", String.valueOf(e));
        }
        finish(code, result);
    }

    /** Recursively copy seedSubtree into destRoot, mirroring relative paths. */
    private static void copyTree(File src, File dest, List<String> seeded) throws IOException {
        if (!src.exists()) {
            return;
        }
        File[] children = src.listFiles();
        if (children == null) {
            throw new IOException("list failed: " + src);
        }
        for (File child : children) {
            File target = new File(dest, child.getName());
            if (child.isDirectory()) {
                copyTree(child, target, seeded);
            } else if (child.isFile()) {
                copyFile(child, target);
                seeded.add(target.getPath());
            }
        }
    }

    private static void copyFile(File src, File dest) throws IOException {
        File parent = dest.getParentFile();
        if (parent != null && !parent.isDirectory() && !parent.mkdirs()) {
            throw new IOException("mkdirs failed: " + parent);
        }
        Files.copy(src.toPath(), dest.toPath(), StandardCopyOption.REPLACE_EXISTING);
        Log.i(TAG, "copied -> " + dest.getPath());
    }
}
