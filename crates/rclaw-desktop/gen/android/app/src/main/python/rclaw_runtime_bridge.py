"""RClaw APK 内嵌 Python 薄 runner（移植方案 D5/D3，阶段 2.3 削薄版）。

与 Rust 侧 `BridgePythonExecutor` 的契约：
- `run_script(script_path, args_json, cwd, limit_bytes=65536)` -> JSON 字符串
  `{"exit_code", "stdout", "stderr", "stdout_truncated", "stderr_truncated"}`；
- stdout/stderr 按字节分别限额截断（默认 64 KiB，与桌面 `skill_entrypoint`
  的 `MAX_OUTPUT_BYTES` 同值同语义：允许切在 UTF-8 字符中间、按 replace
  解码产生替换字符，不回退到字符边界）；
- 无任何进程/目录 monkeypatch（D3）：缺工具时让脚本得到自然报错；
- `_purge_skill_modules` 在每次执行前后清理 bundled skill 顶层模块，隔离技能间状态；
- 超时 watchdog 尽力注入 `KeyboardInterrupt`（`PyThreadState_SetAsyncExc`），
  ctypes 不可用时静默退化为无中断——路线结论以 0-E spike 2 真机验证为准；
  Java 侧也可经 `interrupt_main()` 触发同一注入（spike 2 路线 B 入口）。
"""

import contextlib
import importlib
import json
import os
import runpy
import sys
import tempfile
import threading
import traceback

try:
    import ctypes
except Exception:  # Chaquopy 环境可能缺失 ctypes：降级为无中断注入
    ctypes = None

_OUTPUT_LIMIT_BYTES = 64 * 1024
_WATCHDOG_RETRY_SECS = 0.5


class _BoundedBytesWriter:
    """按 UTF-8 字节计数的有界文本收集器（对齐桌面 read_limited 语义）。"""

    encoding = "utf-8"
    errors = "replace"

    def __init__(self, limit):
        self._limit = limit
        self._data = bytearray()
        self._discarded = 0

    def write(self, value):
        data = str(value).encode("utf-8", "replace")
        remaining = max(0, self._limit - len(self._data))
        if remaining:
            self._data.extend(data[:remaining])
        self._discarded += max(0, len(data) - remaining)
        return len(data)

    def flush(self):
        pass

    def writable(self):
        return True

    def isatty(self):
        return False

    def fileno(self):
        raise OSError("bounded skill capture stream has no file descriptor")

    def result(self):
        text = self._data.decode("utf-8", "replace")
        return text, self._discarded > 0


def _is_below(path, root):
    try:
        return os.path.commonpath((os.path.realpath(path), root)) == root
    except (OSError, TypeError, ValueError):
        return False


def _purge_skill_modules(skills_root):
    """Prevent top-level helper names from leaking between bundled skills."""
    for name, module in list(sys.modules.items()):
        module_path = getattr(module, "__file__", None)
        if module_path and _is_below(module_path, skills_root):
            sys.modules.pop(name, None)
    importlib.invalidate_caches()


def _async_raise(thread_id, exception):
    """跨线程注入异常（CPython C API；Chaquopy 下不可用则返回 False）。"""
    if ctypes is None:
        return False
    try:
        api = ctypes.pythonapi
        set_async_exc = api.PyThreadState_SetAsyncExc
        set_async_exc.argtypes = (ctypes.c_long, ctypes.py_object)
        set_async_exc.restype = ctypes.c_int
    except Exception:
        return False
    try:
        return set_async_exc(thread_id, exception) == 0
    except Exception:
        return False


def interrupt_main():
    """供 Java 侧调用的中断入口（spike 2 路线 B）：向主线程注入 KeyboardInterrupt。"""
    ident = threading.main_thread().ident
    return _async_raise(ident, KeyboardInterrupt)


class _Watchdog:
    """到期后反复向主线程注入 KeyboardInterrupt，直到 stop()。

    注入对 C 扩展/死循环无效是已知限制；处置策略（放弃等待 + poisoned
    标记）在 Rust 侧 `BridgePythonExecutor` 实现，这里只负责尽力中断。
    """

    def __init__(self, timeout_secs):
        self._deadline = timeout_secs
        self._stop = threading.Event()
        self._thread = None

    def start(self):
        main_ident = threading.main_thread().ident
        self._thread = threading.Thread(
            target=self._run, args=(main_ident,), daemon=True
        )
        self._thread.start()

    def _run(self, main_ident):
        if self._stop.wait(self._deadline):
            return
        while not self._stop.is_set():
            if _async_raise(main_ident, KeyboardInterrupt):
                break
            self._stop.wait(_WATCHDOG_RETRY_SECS)

    def stop(self):
        self._stop.set()
        if self._thread is not None:
            self._thread = None


def run_script(script_path, args_json, cwd, env_json=None,
               timeout_secs=None, limit_bytes=_OUTPUT_LIMIT_BYTES):
    script_path = os.path.realpath(script_path)
    cwd = os.path.realpath(cwd)
    if os.path.realpath(os.getcwd()) != cwd:
        return json.dumps(
            {
                "exit_code": 1,
                "stdout": "",
                "stderr": "Embedded Python cwd does not match the RClaw workspace",
                "stdout_truncated": False,
                "stderr_truncated": False,
            },
            ensure_ascii=False,
        )

    args = json.loads(args_json)
    extra_env = dict(json.loads(env_json)) if env_json else {}
    skills_root = os.path.realpath(os.path.join(cwd, "skills"))
    old_argv = sys.argv[:]
    old_path = sys.path[:]
    old_executable = sys.executable
    old_environment = dict(os.environ)
    old_tempdir = tempfile.tempdir
    stdout = _BoundedBytesWriter(limit_bytes)
    stderr = _BoundedBytesWriter(limit_bytes)
    exit_code = 0
    watchdog = _Watchdog(timeout_secs) if timeout_secs is not None else None
    try:
        _purge_skill_modules(skills_root)
        sys.argv = [script_path, *[str(value) for value in args]]
        sys.executable = "python"
        sys.path.insert(0, os.path.dirname(script_path))
        if cwd not in sys.path:
            sys.path.insert(1, cwd)
        private_tmp = os.path.join(cwd, "tmp")
        private_config = os.path.join(cwd, ".config")
        private_cache = os.path.join(cwd, ".cache")
        os.makedirs(private_tmp, exist_ok=True)
        os.makedirs(private_config, exist_ok=True)
        os.makedirs(private_cache, exist_ok=True)
        os.environ["RCLAW_ANDROID"] = "1"
        os.environ["PYTHONUTF8"] = "1"
        os.environ["HOME"] = cwd
        os.environ["TMPDIR"] = private_tmp
        os.environ["XDG_CONFIG_HOME"] = private_config
        os.environ["XDG_CACHE_HOME"] = private_cache
        tempfile.tempdir = private_tmp
        mpl_config = os.path.join(private_cache, "matplotlib")
        os.makedirs(mpl_config, exist_ok=True)
        os.environ["MPLCONFIGDIR"] = mpl_config
        # 宿主注入的可信执行环境（如 RCLAW_SOURCE_CHANNEL）：先摘除继承来源，
        # 再按传入值设置（与桌面子进程 env_remove + env 语义对齐）。
        os.environ.pop("RCLAW_SOURCE_CHANNEL", None)
        for key, value in extra_env.items():
            os.environ[str(key)] = str(value)

        if watchdog is not None:
            watchdog.start()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            try:
                runpy.run_path(script_path, run_name="__main__")
            except SystemExit as error:
                if error.code is None:
                    exit_code = 0
                elif isinstance(error.code, int):
                    exit_code = error.code
                else:
                    exit_code = 1
                    print(str(error.code), file=sys.stderr)
            except BaseException:
                exit_code = 1
                traceback.print_exc(file=sys.stderr)
    finally:
        if watchdog is not None:
            watchdog.stop()
        sys.argv = old_argv
        sys.path[:] = old_path
        sys.executable = old_executable
        os.environ.clear()
        os.environ.update(old_environment)
        tempfile.tempdir = old_tempdir
        _purge_skill_modules(skills_root)

    stdout_text, stdout_truncated = stdout.result()
    stderr_text, stderr_truncated = stderr.result()
    return json.dumps(
        {
            "exit_code": exit_code,
            "stdout": stdout_text,
            "stderr": stderr_text,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
        },
        ensure_ascii=False,
    )
