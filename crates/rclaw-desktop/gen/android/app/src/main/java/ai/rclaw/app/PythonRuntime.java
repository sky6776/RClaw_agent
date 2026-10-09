package ai.rclaw.app;

import android.content.Context;

import com.chaquo.python.PyObject;
import com.chaquo.python.Python;
import com.chaquo.python.android.AndroidPlatform;

public final class PythonRuntime {
    private final Context context;

    PythonRuntime(Context context) {
        this.context = context.getApplicationContext();
        PdfRuntime.initialize(this.context);
    }

    /**
     * Thin pass-through to rclaw_runtime_bridge.run_script. The Rust
     * BridgePythonExecutor owns timeout/limit policy; Java only serializes
     * access (synchronized) so at most one skill script runs in-process.
     */
    public synchronized String runScript(
            String scriptPath, String argsJson, String cwd,
            String envJson, long timeoutMs, long limitBytes) {
        if (!Python.isStarted()) {
            Python.start(new AndroidPlatform(context));
        }
        PyObject bridge = Python.getInstance().getModule("rclaw_runtime_bridge");
        double timeoutSecs = timeoutMs > 0 ? timeoutMs / 1000.0 : 0.0;
        return bridge.callAttr(
                "run_script", scriptPath, argsJson, cwd, envJson,
                timeoutSecs > 0.0 ? timeoutSecs : null, limitBytes)
                .toString();
    }

    /**
     * Cooperative cancellation route B (stage 2.2): inject a KeyboardInterrupt
     * into the running script thread via the bridge. Deliberately NOT
     * synchronized with runScript — the cancellation caller must reach here
     * while a script is still running. Returns true when delivered.
     */
    public boolean interruptScript() {
        if (!Python.isStarted()) {
            return false;
        }
        PyObject bridge = Python.getInstance().getModule("rclaw_runtime_bridge");
        // callAttr 返回 PyObject，Boolean.TRUE.equals 恒 false——中断已成功
        // 送达也会被 Rust 侧判为失败，可能误升级处置（poisoned）。
        return bridge.callAttr("interrupt_main").toJava(Boolean.class);
    }
}
