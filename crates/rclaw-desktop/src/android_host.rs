//! APK 宿主面（阶段 4）：单 .so（`librclaw_desktop.so`）对 Java 导出的 JNI 符号。
//!
//! `rclaw-android` 以 rlib 链入本 crate；Java 侧 `System.loadLibrary("rclaw_desktop")`
//! 后调用的 `Java_ai_rclaw_app_NativeBridge_*` 全部由本模块导出。符号集与旧
//! v0.2.0 工程一致（nativeStart/Stop/IsHealthy/ResolveAgentRoot），包名统一
//! `ai.rclaw.app`；JavaVM 在 `nativeStart` 经 `JNIEnv::get_java_vm` 取得并注入，
//! 不再依赖 `JNI_OnLoad`。

use std::sync::OnceLock;

use crate::state::RunningAgent;

/// 共享运行时 + 无头期遗留的 desktop 通道 outbound 接收端（GUI 采用时接桥）。
pub(crate) struct ApkRuntime {
    pub(crate) agent: RunningAgent,
    pub(crate) pending_outbound: Option<tokio::sync::mpsc::Receiver<rclaw_core::BusMessage>>,
}

/// 进程级共享运行时槽：服务（nativeStart，无 GUI）与 GUI（tauri setup）都经
/// `crate::agent::assemble_apk_agent` 装配；先装配的一方占槽，另一方采用。
pub(crate) static APK_RUNTIME: OnceLock<tokio::sync::Mutex<Option<ApkRuntime>>> =
    OnceLock::new();

pub(crate) fn apk_runtime_slot() -> &'static tokio::sync::Mutex<Option<ApkRuntime>> {
    APK_RUNTIME.get_or_init(|| tokio::sync::Mutex::new(None))
}

use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jstring, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;

fn result_string(env: JNIEnv<'_>, result: anyhow::Result<()>) -> jstring {
    let message = result.err().map(|e| e.to_string()).unwrap_or_default();
    env.new_string(message)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_ai_rclaw_app_NativeBridge_nativeInit(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    files_dir: JString<'_>,
) -> jstring {
    let result = (|| -> anyhow::Result<()> {
        let vm = env.get_java_vm().map_err(anyhow::Error::from)?;
        rclaw_android::set_java_vm(vm);
        let dir = env
            .get_string(&files_dir)
            .map(|s| s.to_string_lossy().into_owned())
            .map_err(anyhow::Error::from)?;
        if dir.trim().is_empty() {
            anyhow::bail!("filesDir 不能为空");
        }
        // 最小启动上下文：全量配置由服务的 nativeStart 稍后覆盖（见 nativeStart）。
        let minimal = serde_json::json!({
            "files_dir": dir.trim(),
        })
        .to_string();
        rclaw_android::record_host_context(
            std::path::PathBuf::from(dir.trim()),
            minimal,
        );
        Ok(())
    })();
    result_string(env, result)
}

#[no_mangle]
pub extern "system" fn Java_ai_rclaw_app_NativeBridge_nativeStart(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
    python_bridge: jni::objects::JObject<'_>,
) -> jstring {
    let config = (|| -> anyhow::Result<()> {
        let vm = env.get_java_vm().map_err(anyhow::Error::from)?;
        rclaw_android::set_java_vm(vm);
        rclaw_android::set_python_bridge(&env, python_bridge)?;
        // 本导出跑在 Java 线程上：此刻缓存 NativeBridge 类引用，attached 原生
        // 线程（tauri 命令）后续的 JNI 调用才能拿到应用类（否则 FATAL）。
        rclaw_android::jni_bridge::cache_native_bridge_class(&mut env)?;
        let json = env
            .get_string(&config_json)
            .map(|s| s.to_string_lossy().into_owned())
            .map_err(anyhow::Error::from)?;
        let launch: rclaw_android::AndroidConfig = serde_json::from_str(&json)?;
        let files_dir = std::path::PathBuf::from(launch.files_dir.trim());
        rclaw_android::record_host_context(files_dir.clone(), json.clone());
        // v2 语义：不再走 v0.2.0 的 rclaw_android::start 独立运行时；GUI 场景由
        // tauri setup 装配。服务无头场景（GUI 从未打开）：filesDir/rclaw.toml 已
        // 存在时在独立线程装配共享运行时（napcat/weixin 通道随即可用）。
        // 同进程「停止 → 再启动」：槽内运行时仍在跑，重新亮起健康信号即可，不再新建
        // 一个（故意泄漏的）runtime 去发现「已就绪」——每次重启都泄漏一套线程。
        if apk_runtime_slot()
            .try_lock()
            .is_ok_and(|slot| slot.is_some())
        {
            rclaw_android::set_runtime_healthy(true);
            tracing::info!("APK 共享运行时已就绪，nativeStart 复用");
            return Ok(());
        }
        if files_dir.join("rclaw.toml").is_file() {
            std::thread::Builder::new()
                .name("rclaw-apk-headless".into())
                .spawn(move || {
                    // 故意泄漏：block_on 返回（共享运行时入槽即返回）后 runtime
                    // 若被 drop，agent/心跳/cron 等全部 tokio 任务随之取消——
                    // 心跳停喂，Java 看门狗 150s 后判死杀进程（偶发前台退出的
                    // 真因）。该 runtime 本就是进程级全局运行时，随进程共存亡。
                    let runtime: &'static tokio::runtime::Runtime = Box::leak(
                        Box::new(
                            tokio::runtime::Builder::new_multi_thread()
                                .worker_threads(2)
                                .enable_all()
                                .build()
                                .expect("构建 APK 无头 tokio runtime 失败"),
                        ),
                    );
                    runtime.block_on(async move {
                        let mut slot = apk_runtime_slot().lock().await;
                        if slot.is_some() {
                            // 同进程「停止 → 再启动」：既有运行时（agent loop / 通道 /
                            // cron / 心跳任务）仍在泄漏的 runtime 上跑，直接重新亮起
                            // 健康信号即可，不能再装配一套（否则双 agent 同时消费
                            // 同一通道与 cron 表：重复回复、定时任务双发）。
                            rclaw_android::set_runtime_healthy(true);
                            tracing::info!("APK 共享运行时已就绪，无头启动跳过");
                            return;
                        }
                        match crate::agent::assemble_apk_agent(&json, None).await {
                            Ok((agent, pending_outbound)) => {
                                *slot = Some(ApkRuntime {
                                    agent,
                                    pending_outbound,
                                });
                                tracing::info!("APK 无头运行时已就绪（等待 GUI 采用）");
                            }
                            Err(error) => {
                                tracing::error!("APK 无头装配失败: {error}");
                                crate::android_boot_log(&format!("无头装配失败: {error}"));
                            }
                        }
                    });
                })
                .map_err(anyhow::Error::from)?;
        }
        Ok(())
    })();
    result_string(env, config)
}

#[no_mangle]
pub extern "system" fn Java_ai_rclaw_app_NativeBridge_nativeStop(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    // v0.2.0 独立 host 线程（rclaw_android::start/stop）已删除：nativeStop
    // 只做健康信号清除（GUI 若在前台，其句柄仍有效直到进程退出）。
    // 槽内运行时不清：它跑在故意泄漏的进程级 runtime 上，清槽停不掉任何任务，
    // 只会让同进程的下一次 nativeStart / GUI 启动再装配一套并行的 agent。
    rclaw_android::set_runtime_healthy(false);
    result_string(env, Ok(()))
}

#[no_mangle]
pub extern "system" fn Java_ai_rclaw_app_NativeBridge_nativeIsHealthy(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jboolean {
    if rclaw_android::is_healthy() {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[no_mangle]
pub extern "system" fn Java_ai_rclaw_app_NativeBridge_nativeResolveAgentRoot(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
) -> jstring {
    let result = env
        .get_string(&config_json)
        .map(|json| json.to_string_lossy().into_owned())
        .map_err(anyhow::Error::from)
        .and_then(|json| rclaw_android::resolve_agent_root(&json));
    let payload = match result {
        Ok(path) => serde_json::json!({"ok": true, "path": path.display().to_string()}),
        Err(error) => serde_json::json!({"ok": false, "error": error.to_string()}),
    };
    env.new_string(payload.to_string())
        .map(|value| value.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
