//! 全局共享 Tokio 运行时模块
//!
//! P0-3: 由 current_thread 改为 multi_thread,以支持:
//! - embedding 批次并发(`embed_batch_concurrent`)并行请求外部 API
//! - 后续多 worker 拆分的索引任务并行执行
//!
//! 线程数由环境变量 `GBRAIN_ASYNC_WORKER_THREADS` 控制(默认 4,最小 2)。

use std::sync::OnceLock;

/// 全局共享的 Tokio multi_thread 运行时
static SHARED_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// 获取全局共享的 Tokio 运行时实例。
/// 首次调用时初始化,后续调用返回同一实例。
///
/// 线程数取自环境变量 `GBRAIN_ASYNC_WORKER_THREADS`(解析失败或为 0 时回退到 4,
/// 最终结果会在 [2, usize::MAX] 区间内,以避免单线程退化)。
pub fn shared_runtime() -> &'static tokio::runtime::Runtime {
    SHARED_RUNTIME.get_or_init(|| {
        let threads = std::env::var("GBRAIN_ASYNC_WORKER_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(4)
            .max(2);

        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .enable_all()
            .build()
            .expect("初始化共享 Tokio 运行时失败")
    })
}
