use std::sync::atomic::{AtomicU64, Ordering};

/// OCR 临时目录总字节使用量（全局原子计数器）。
static OCR_TEMP_DIR_BYTES: AtomicU64 = AtomicU64::new(0);

/// 返回当前 OCR 临时目录总字节使用量。
pub(crate) fn ocr_temp_dir_bytes_used() -> u64 {
    OCR_TEMP_DIR_BYTES.load(Ordering::Relaxed)
}

/// 尝试申请临时目录字节预算。
/// 若当前使用量 + 申请量未超过预算上限，则原子加法并返回 true；
/// 否则不修改计数器并返回 false。
pub(crate) fn ocr_temp_dir_try_reserve(bytes: u64, max_bytes: u64) -> bool {
    loop {
        let current = OCR_TEMP_DIR_BYTES.load(Ordering::Relaxed);
        let next = current.saturating_add(bytes);
        if next > max_bytes {
            return false;
        }
        if OCR_TEMP_DIR_BYTES
            .compare_exchange(current, next, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
    }
}

/// 释放临时目录字节预算（饱和减法，不会下溢）。
pub(crate) fn ocr_temp_dir_release(bytes: u64) {
    // 与 `ocr_temp_dir_try_reserve` 同款 CAS 循环：`fetch_update` 自 1.99 起弃用，
    // 替代的 `try_update` 要到 1.95 才稳定，MSRV 1.91 下两者都不宜直接用。
    loop {
        let current = OCR_TEMP_DIR_BYTES.load(Ordering::Relaxed);
        let next = current.saturating_sub(bytes);
        if OCR_TEMP_DIR_BYTES
            .compare_exchange(current, next, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
}

/// OCR 临时目录的 RAII 清理守卫。
/// 无论函数从哪条路径（正常返回、`?` 提前返回、panic）退出，
/// `Drop` 都会尝试递归删除临时目录，避免残留子 PDF 等文件。
pub struct TempOcrDir {
    path: std::path::PathBuf,
    /// 此目录写入的总字节数，在 drop 时从全局计数器释放。
    bytes_reserved: u64,
}

impl TempOcrDir {
    /// 创建 OCR 临时目录。
    /// 不再在创建时预留 estimated_bytes 预算，改为按实际写入逐文件计量，
    /// 避免递归拆分场景下预估偏差导致实际占用超过配置上限。
    pub fn create(prefix: &str, _estimated_bytes: u64, _max_bytes: u64) -> std::io::Result<Self> {
        static NEXT_DIR_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        for _ in 0..1000 {
            let id = NEXT_DIR_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("{}_{}_{}", prefix, std::process::id(), id));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        bytes_reserved: 0,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(e);
                }
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "unable to allocate a unique OCR temporary directory",
        ))
    }

    /// 返回临时目录路径。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 尝试为写入文件预留字节预算。成功时累加到内部计数器并返回 true。
    pub fn try_reserve(&mut self, bytes: u64, max_bytes: u64) -> bool {
        if bytes == 0 {
            return true;
        }
        if ocr_temp_dir_try_reserve(bytes, max_bytes) {
            self.bytes_reserved += bytes;
            true
        } else {
            false
        }
    }

    /// 释放已预留的字节预算（仅在文件删除成功后调用）。
    pub fn release(&mut self, bytes: u64) {
        if bytes > 0 {
            ocr_temp_dir_release(bytes);
            self.bytes_reserved = self.bytes_reserved.saturating_sub(bytes);
        }
    }
}

/// 清扫其它（已死）进程遗留的 OCR 临时目录。
///
/// 目录只靠 `Drop` 删除，进程被 Android LMK / 用户强杀时 Drop 不会执行，拆分出的子 PDF
/// （每次最多数百 MB）就永久残留在 APP 私有 tmp 目录里，跨重启无界增长。目录名形如
/// `{prefix}_{pid}_{id}`，取倒数第二段作为 pid：不等于当前进程且该进程已不存在（Linux /
/// Android 查 `/proc/{pid}`，其它平台退化为 mtime 超过 1 小时）的目录整体删除。
pub(crate) fn sweep_orphaned_dirs(prefix: &str) {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let self_pid = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(prefix) || !entry.path().is_dir() {
            continue;
        }
        let mut segs = name.rsplit('_');
        let (Some(_id), Some(pid)) = (segs.next(), segs.next()) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if pid == self_pid || !process_is_gone(pid, &entry.path()) {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => tracing::info!(dir = %name, "已清扫上次进程遗留的 OCR 临时目录"),
            Err(e) => tracing::warn!(dir = %name, error = %e, "清扫遗留 OCR 临时目录失败"),
        }
    }
}

fn process_is_gone(pid: u32, dir: &std::path::Path) -> bool {
    if cfg!(any(target_os = "linux", target_os = "android")) {
        return !std::path::Path::new(&format!("/proc/{pid}")).exists();
    }
    // 无法判断进程存活的平台：只清理 1 小时以上没有写入的目录
    dir.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age >= std::time::Duration::from_secs(3600))
}

impl Drop for TempOcrDir {
    fn drop(&mut self) {
        // 仅在目录删除成功后释放已占用预算。
        // 清理失败时保留预算，防止后续任务在磁盘空间未释放的情况下继续超额写入。
        match self.path.try_exists() {
            Ok(false) => {
                // 路径已确认不存在，不可能继续占用此目录下的磁盘空间。
                if self.bytes_reserved > 0 {
                    ocr_temp_dir_release(self.bytes_reserved);
                }
            }
            Ok(true) => match std::fs::remove_dir_all(&self.path) {
                Ok(()) => {
                    tracing::debug!("OCR temporary directory removed");
                    if self.bytes_reserved > 0 {
                        ocr_temp_dir_release(self.bytes_reserved);
                    }
                }
                Err(e) => {
                    // 清理失败时不释放预算，防止后续任务继续超额写入
                    tracing::warn!(
                        error = %e,
                        "OCR temporary directory cleanup failed"
                    );
                }
            },
            Err(e) => {
                // 无法确认目录是否存在时继续保留预算。
                tracing::warn!(
                    error = %e,
                    "OCR temporary directory existence check failed"
                );
            }
        }
    }
}
