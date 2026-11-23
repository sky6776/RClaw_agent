//! `file` 后端：一个本机目录。目录本身可以交给任意工具同步（网盘客户端、Syncthing、
//! 共享盘、U 盘）。
//!
//! 这类目录的特点（§10.1）：文件到达的先后没有保证，head 可能先于它引用的段出现；
//! 同步工具可能留下冲突副本。所以：对象没到齐时只等待（[`SyncTransport::arrival_grace`]
//! 给到 10 分钟）；不认识的文件名一律忽略；写入用临时文件加改名，别的设备看不到写了
//! 一半的对象。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;

use super::{
    check_key, is_object_name, oversize, too_many, ObjectMeta, SyncTransport, TransportResult,
    LIST_MAX,
};
use crate::error::TransportError;
use crate::ids;

pub struct FileTransport {
    root: PathBuf,
}

impl FileTransport {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FileTransport { root: root.into() }
    }

    async fn at<T: Send + 'static>(
        &self,
        key: &str,
        action: impl FnOnce(PathBuf) -> TransportResult<T> + Send + 'static,
    ) -> TransportResult<T> {
        check_key(key)?;
        let root = self.root.clone();
        let key = key.to_string();
        blocking(move || {
            let meta = std::fs::symlink_metadata(&root).map_err(|error| {
                TransportError::Other(format!("同步文件夹 {} 无法访问：{error}", root.display()))
            })?;
            if !meta.is_dir() || crate::wire::is_link(&meta) {
                return Err(TransportError::Other(format!(
                    "同步文件夹 {} 不是普通目录",
                    root.display()
                )));
            }
            let mut path = root.clone();
            path.extend(key.split('/').filter(|part| !part.is_empty()));
            crate::wire::check_no_links(&root, &path)
                .map_err(|error| io_error("检查路径", &path, error))?;
            action(path)
        })
        .await
    }

    pub(crate) async fn create_namespace(&self, name: &str) -> TransportResult<()> {
        self.at(name, |path| ensure_directory(&path)).await
    }
}

fn io_error(what: &str, path: &Path, e: std::io::Error) -> TransportError {
    if e.kind() == std::io::ErrorKind::NotFound {
        TransportError::NotFound(path.display().to_string())
    } else {
        TransportError::Other(format!("{what} {} 失败: {e}", path.display()))
    }
}

fn meta_of(name: String, meta: &std::fs::Metadata) -> ObjectMeta {
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().to_string());
    // 修改时间：变了就说明可能变了（立刻读），没变不能说明没变（同步工具复制、保留
    // 时间戳时两版等长内容会带着同一个值）——判重与回收依据都只把它当提示。
    let stable_version = modified.is_some();
    ObjectMeta {
        name,
        size: meta.len(),
        version: modified.unwrap_or_default(),
        stable_version,
        strong_version: false,
    }
}

fn list_blocking(path: PathBuf, is_dir: bool) -> TransportResult<Vec<ObjectMeta>> {
    if !is_dir {
        return match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Ok(vec![meta_of(name, &meta)])
            }
            Ok(_) => Ok(Vec::new()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(io_error("读取", &path, e)),
        };
    }
    let entries = match std::fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_error("列出", &path, e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_error("列出", &path, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // 冲突副本、本后端自己的临时文件、其他不认识的名字。
        if !is_object_name(&name) {
            continue;
        }
        // 条目可能在列目录与取元数据之间被同步工具移走：跳过即可。
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_file() && !crate::wire::is_link(&meta) {
            if out.len() >= LIST_MAX {
                return Err(too_many(&path.display().to_string()));
            }
            out.push(meta_of(name, &meta));
        }
    }
    Ok(out)
}

/// 临时文件与目标在同一目录，改名才是原子的。
fn temp_path(path: &Path) -> PathBuf {
    let name = format!(".rclaw-{}.tmp", ids::random_hex::<6>());
    path.with_file_name(name)
}

fn ensure_directory(path: &Path) -> TransportResult<()> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(io_error("创建目录", path, error)),
    }
}

fn write_blocking(
    root: &Path,
    path: PathBuf,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> TransportResult<()> {
    if let Some(parent) = path.parent() {
        let relative = parent
            .strip_prefix(root)
            .map_err(|error| TransportError::Other(error.to_string()))?;
        let mut current = root.to_path_buf();
        for part in relative.components() {
            current.push(part);
            ensure_directory(&current)?;
        }
    }
    let tmp = temp_path(&path);
    let mut file =
        std::fs::File::create_new(&tmp).map_err(|e| io_error("创建临时文件", &tmp, e))?;
    let result = (|| {
        let written = write(&mut file).and_then(|()| file.sync_all());
        drop(file);
        written?;
        std::fs::rename(&tmp, &path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(TransportError::Other(format!(
            "写入 {} 失败: {e}",
            path.display()
        )));
    }
    Ok(())
}

/// 把已经写满并 fsync 的临时文件以「目标存在即失败」的方式发布。
///
/// Windows 用同卷 `MoveFileExW`（不设 `MOVEFILE_REPLACE_EXISTING`）：重命名原子、
/// 目标已存在则失败，exFAT/FAT32 也支持。其它平台先试硬链接；卷不支持硬链接时退回
/// 「create_new + 复制」，复制失败会删掉刚创建的半成品，避免半个 space.json 卡住
/// 重置恢复。
#[cfg(windows)]
fn publish_new(tmp: &Path, path: &Path) -> std::io::Result<()> {
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    // Win32 的 MoveFileExW 默认受 MAX_PATH 限制：用扩展长度路径（本地盘
    // `\\?\C:...`、UNC `\\?\UNC\server...`）让深层目录也能发布（Rust 的临时文件/
    // 目录 API 已经走扩展路径）。
    fn wide_long(path: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        // 规范化父目录得到 verbatim（扩展长度）绝对路径，消掉正斜杠、`.`/`..` 与
        // 相对路径；目标文件名可能还不存在，只拼叶子。成功分支直接按 UTF-16
        // 编码（无损，含未配对代理单元也不会被替换）。
        if let Some(parent) = path.parent() {
            if let Ok(parent) = std::fs::canonicalize(parent) {
                let full = match path.file_name() {
                    Some(name) => parent.join(name),
                    None => parent,
                };
                return full
                    .as_os_str()
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect();
            }
        }
        // 规范化失败（SMB 权限、卷暂不可访问等）：先做词法归一化（扩展路径不解释
        // `.`/`..`），再按绝对盘符/UNC 形式补扩展前缀；全程 OsStr/UTF-16，
        // 不走 to_string_lossy。
        let normalized = lexical_normalize(path);
        let mut wide: Vec<u16> = Vec::new();
        let mut components = normalized.components();
        let prefix = components.next();
        // 宽字符前缀：\\?\UNC\ 与 \\?\（用码点常量，避免转义歧义）。
        const UNC_PREFIX: [u16; 8] = [0x5c, 0x5c, 0x3f, 0x5c, 0x55, 0x4e, 0x43, 0x5c];
        const DISK_PREFIX: [u16; 4] = [0x5c, 0x5c, 0x3f, 0x5c];
        let already_extended = matches!(
            prefix,
            Some(std::path::Component::Prefix(prefix))
                if matches!(
                    prefix.kind(),
                    std::path::Prefix::VerbatimDisk(_)
                        | std::path::Prefix::VerbatimUNC(_, _)
                        | std::path::Prefix::Verbatim(_)
                )
        );
        if !already_extended && path.is_absolute() {
            if let Some(std::path::Component::Prefix(prefix)) = prefix {
                if let std::path::Prefix::UNC(server, share) = prefix.kind() {
                    wide.extend(UNC_PREFIX);
                    wide.extend(server.encode_wide());
                    wide.push(0x5c);
                    wide.extend(share.encode_wide());
                    for component in components {
                        match component {
                            std::path::Component::RootDir
                            | std::path::Component::CurDir => {}
                            other => {
                                wide.push(0x5c);
                                wide.extend(other.as_os_str().encode_wide());
                            }
                        }
                    }
                    wide.push(0);
                    return wide;
                }
            }
            wide.extend(DISK_PREFIX);
        }
        wide.extend(normalized.as_os_str().encode_wide());
        wide.push(0);
        wide
    }

    /// 词法归一化：去掉 `.`、消解 `..`（扩展长度路径不解释点分量）。
    fn lexical_normalize(path: &Path) -> PathBuf {
        use std::os::windows::ffi::OsStrExt;
        use std::path::Component;
        let mut out = PathBuf::new();
        // 还没落到路径里的前导 `..`（`out` 里已经没有可弹出的组件时累积）。
        let mut leading_parents = 0usize;
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if leading_parents > 0 {
                        leading_parents += 1;
                    } else if !out.pop() {
                        // 根上再 `..`：Win32 的普通路径也会保留（不能丢，否则指向别处）。
                        leading_parents = 1;
                    }
                }
                other => {
                    for _ in 0..leading_parents {
                        out.push("..");
                    }
                    leading_parents = 0;
                    let name = other.as_os_str();
                    // 普通 Win32 会去掉每个分量结尾的点与空格；扩展路径不会——两边
                    // 混用时（Rust 创建临时文件、这里发布）必须自己按普通语义归一。
                    let trimmed = if matches!(other, Component::Normal(_)) {
                        trim_trailing_dots_and_spaces(name)
                    } else {
                        name.to_os_string()
                    };
                    out.push(trimmed);
                }
            }
        }
        for _ in 0..leading_parents {
            out.push("..");
        }
        out
    }

    /// 去掉文件名分量结尾的点与空格（Win32 普通路径语义），按 UTF-16 处理，保留
    /// 未配对代理单元。
    fn trim_trailing_dots_and_spaces(name: &std::ffi::OsStr) -> std::ffi::OsString {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let mut wide: Vec<u16> = name.encode_wide().collect();
        while matches!(wide.last(), Some(0x2e) | Some(0x20)) {
            wide.pop();
        }
        if wide.is_empty() {
            name.to_os_string()
        } else {
            std::ffi::OsString::from_wide(&wide)
        }
    }
    let from: Vec<u16> = wide_long(tmp);
    let to: Vec<u16> = wide_long(path);
    let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn publish_new(tmp: &Path, path: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let mut target = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            let copied = (|| {
                let mut source = std::fs::File::open(tmp)?;
                std::io::copy(&mut source, &mut target)?;
                target.sync_all()
            })();
            if copied.is_err() {
                // 正式路径上只留了一个刚创建的空/半截文件：删掉它，恢复「不存在」
                // 状态，下一次重试还能重新走仅创建。
                let _ = std::fs::remove_file(path);
            }
            copied
        }
    }
}

/// 同 [`write_blocking`]，但目标已存在时失败：临时文件写满并 fsync 后用平台原语
/// 原子落位（目标已存在则失败），迟到的「创建空间」写入不能覆盖已有的关闭标记。
fn write_blocking_new(
    root: &Path,
    path: PathBuf,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> TransportResult<()> {
    if let Some(parent) = path.parent() {
        let relative = parent
            .strip_prefix(root)
            .map_err(|error| TransportError::Other(error.to_string()))?;
        let mut current = root.to_path_buf();
        for part in relative.components() {
            current.push(part);
            ensure_directory(&current)?;
        }
    }
    let tmp = temp_path(&path);
    let mut file =
        std::fs::File::create_new(&tmp).map_err(|e| io_error("创建临时文件", &tmp, e))?;
    let result = (|| {
        let written = write(&mut file).and_then(|()| file.sync_all());
        drop(file);
        written?;
        publish_new(&tmp, &path)
    })();
    match result {
        Ok(()) => {
            let _ = std::fs::remove_file(&tmp);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(TransportError::Other(format!(
                "创建 {} 失败: {e}",
                path.display()
            )))
        }
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> TransportResult<T> + Send + 'static,
) -> TransportResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| TransportError::Other(format!("文件操作被中断: {e}")))?
}

#[async_trait]
impl SyncTransport for FileTransport {
    async fn list(&self, prefix: &str) -> TransportResult<Vec<ObjectMeta>> {
        let is_dir = prefix.ends_with('/');
        self.at(prefix, move |path| list_blocking(path, is_dir))
            .await
    }

    async fn get(&self, key: &str, max: u64) -> TransportResult<Vec<u8>> {
        let object_name = key.to_string();
        self.at(key, move |path| {
            let file = std::fs::File::open(&path).map_err(|e| io_error("读取", &path, e))?;
            // 先看长度：过大的文件根本不读。
            let len = file
                .metadata()
                .map_err(|e| io_error("读取", &path, e))?
                .len();
            if len > max {
                return Err(oversize(&object_name, max));
            }
            // 文件在看过长度之后还可能变长（同步工具正在写）：读的时候再拦一次。
            let mut body = Vec::with_capacity(len as usize);
            file.take(max.saturating_add(1))
                .read_to_end(&mut body)
                .map_err(|e| io_error("读取", &path, e))?;
            if body.len() as u64 > max {
                return Err(oversize(&object_name, max));
            }
            Ok(body)
        })
        .await
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> TransportResult<()> {
        let root = self.root.clone();
        self.at(key, move |path| {
            write_blocking(&root, path, |file| file.write_all(&body))
        })
        .await
    }

    async fn put_new(&self, key: &str, body: Vec<u8>) -> TransportResult<()> {
        let root = self.root.clone();
        self.at(key, move |path| {
            write_blocking_new(&root, path, |file| file.write_all(&body))
        })
        .await
    }

    async fn delete(&self, key: &str) -> TransportResult<()> {
        self.at(key, move |path| match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error("删除", &path, e)),
        })
        .await
    }

    async fn put_file(&self, key: &str, source: &Path) -> TransportResult<()> {
        let root = self.root.clone();
        let source = source.to_path_buf();
        self.at(key, move |path| {
            let mut input =
                std::fs::File::open(&source).map_err(|e| io_error("读取", &source, e))?;
            write_blocking(&root, path, |file| {
                std::io::copy(&mut input, file).map(|_| ())
            })
        })
        .await
    }

    async fn get_file(&self, key: &str, dest: &Path, max: u64) -> TransportResult<()> {
        let dest = dest.to_path_buf();
        let object_name = key.to_string();
        self.at(key, move |path| {
            let source = std::fs::File::open(&path).map_err(|e| io_error("读取", &path, e))?;
            let len = source
                .metadata()
                .map_err(|e| io_error("读取", &path, e))?
                .len();
            if len > max {
                return Err(oversize(&object_name, max));
            }
            let write_error = |e: std::io::Error| {
                TransportError::Other(format!("写入 {} 失败: {e}", dest.display()))
            };
            let mut output = std::fs::File::create(&dest).map_err(write_error)?;
            // 最多拷 `max + 1` 字节：拷出了第 `max + 1` 个字节就是源文件在这期间变长了。
            let copied = std::io::copy(&mut source.take(max.saturating_add(1)), &mut output)
                .map_err(|e| TransportError::Other(format!("复制 {} 失败: {e}", path.display())))?;
            if copied > max {
                return Err(oversize(&object_name, max));
            }
            output.flush().map_err(write_error)
        })
        .await
    }

    fn arrival_grace(&self) -> Duration {
        Duration::from_secs(600)
    }

    fn kind(&self) -> &'static str {
        "file"
    }
}

#[cfg(test)]
mod tests {
    use super::super::{check_len, get_opt, put_checked, stat};
    use super::*;

    #[tokio::test]
    async fn an_unavailable_storage_root_does_not_look_empty_or_get_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent");
        let transport = FileTransport::new(&missing);
        assert!(matches!(
            transport.list("heads/").await,
            Err(TransportError::Other(_))
        ));
        assert!(matches!(
            stat(&transport, "space.json").await,
            Err(TransportError::Other(_))
        ));
        assert!(transport.put("heads/a", b"data".to_vec()).await.is_err());
        assert!(!missing.exists());
    }

    #[tokio::test]
    async fn put_get_list_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());

        assert!(transport.list("heads/").await.unwrap().is_empty());
        assert!(get_opt(&transport, "heads/a", 1024)
            .await
            .unwrap()
            .is_none());

        put_checked(&transport, "heads/a", b"one".to_vec())
            .await
            .unwrap();
        put_checked(&transport, "heads/b", b"three".to_vec())
            .await
            .unwrap();
        let mut names: Vec<(String, u64)> = transport
            .list("heads/")
            .await
            .unwrap()
            .into_iter()
            .map(|m| (m.name, m.size))
            .collect();
        names.sort();
        assert_eq!(names, vec![("a".into(), 3), ("b".into(), 5)]);

        // 覆盖写：内容换掉，目录里不留临时文件。
        transport.put("heads/a", b"second".to_vec()).await.unwrap();
        assert_eq!(transport.get("heads/a", 1024).await.unwrap(), b"second");
        // 长度正好等于上限的读得出来，多一个字节就拒绝，而且不算「不存在」。
        assert_eq!(transport.get("heads/a", 6).await.unwrap(), b"second");
        let too_big = transport.get("heads/a", 5).await.unwrap_err();
        assert!(matches!(too_big, TransportError::Oversize(_)), "{too_big}");
        assert!(matches!(
            get_opt(&transport, "heads/a", 5).await,
            Err(TransportError::Oversize(_))
        ));
        check_len(&transport, "heads/a", 6).await.unwrap();
        assert!(check_len(&transport, "heads/a", 7).await.is_err());
        let on_disk: Vec<String> = std::fs::read_dir(dir.path().join("heads"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(on_disk.len(), 2, "{on_disk:?}");

        assert_eq!(stat(&transport, "heads/a").await.unwrap().unwrap().size, 6);
        assert!(stat(&transport, "heads/zz").await.unwrap().is_none());

        transport.delete("heads/a").await.unwrap();
        transport.delete("heads/a").await.unwrap();
        assert!(transport
            .get("heads/a", 1024)
            .await
            .unwrap_err()
            .is_not_found());
    }

    #[tokio::test]
    async fn listing_ignores_conflict_copies_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let heads = dir.path().join("heads");
        std::fs::create_dir_all(heads.join("subdir")).unwrap();
        std::fs::write(heads.join("0123456789abcdef"), b"x").unwrap();
        std::fs::write(heads.join("0123456789abcdef (冲突副本)"), b"x").unwrap();
        std::fs::write(
            heads.join("0123456789abcdef.sync-conflict-20260101-000000-ABCDEFG"),
            b"x",
        )
        .unwrap();
        std::fs::write(heads.join(".rclaw-000000000000.tmp"), b"x").unwrap();

        let transport = FileTransport::new(dir.path());
        let names: Vec<String> = transport
            .list("heads/")
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, vec!["0123456789abcdef".to_string()]);
    }

    #[tokio::test]
    async fn rejects_keys_that_escape_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path().join("space"));
        assert!(transport.put("../escape", b"x".to_vec()).await.is_err());
        assert!(transport.get("a/../../escape", 1024).await.is_err());
        assert!(!dir.path().join("escape").exists());
    }

    #[tokio::test]
    async fn file_variants_copy_content() {
        let dir = tempfile::tempdir().unwrap();
        FileTransport::new(dir.path())
            .create_namespace("space")
            .await
            .unwrap();
        let transport = FileTransport::new(dir.path().join("space"));
        let source = dir.path().join("source.bin");
        std::fs::write(&source, vec![7u8; 100_000]).unwrap();
        transport
            .put_file("blobs/ab/abcdef", &source)
            .await
            .unwrap();
        let dest = dir.path().join("dest.bin");
        transport
            .get_file("blobs/ab/abcdef", &dest, 100_000)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), vec![7u8; 100_000]);
        assert!(transport
            .get_file("blobs/ab/missing", &dest, 100_000)
            .await
            .unwrap_err()
            .is_not_found());
        // 比上限大的对象：不拷，落点上什么都不写。
        let refused = dir.path().join("refused.bin");
        assert!(matches!(
            transport
                .get_file("blobs/ab/abcdef", &refused, 99_999)
                .await,
            Err(TransportError::Oversize(_))
        ));
        assert!(!refused.exists());
    }
}
