//! 远端对象的读写：`space.json`、head、段、汇总包（§9.1）。
//!
//! 这里的传输都已经套在空间自己的子目录里（[`crate::transport::Scoped`]）。读到的东西
//! 一律先解密、校验，再交给上层。

use crate::codec::{
    self, Head, Rollup, Segment, SpaceFile, DEVICES_MAX, HEADS_DIR, HEAD_OBJECT_MAX,
    ROLLUP_OBJECT_MAX, ROLLUP_PLAIN_MAX, SEGMENT_OBJECT_MAX, SEGMENT_PLAIN_MAX, SPACE_CLOSED_FILE,
    SPACE_FILE, SPACE_FILE_MAX,
};
use std::sync::Arc;

use crate::blocking;
use crate::crypto::{sha256_hex, Keys};
use crate::error::{Result, SyncError};
use crate::ids;
use crate::transport::{self, ObjectMeta, SyncTransport};

pub async fn read_space(transport: &dyn SyncTransport) -> Result<Option<SpaceFile>> {
    match transport::get_opt(transport, SPACE_FILE, SPACE_FILE_MAX as u64).await? {
        Some(bytes) => Ok(Some(SpaceFile::decode(&bytes)?)),
        None => Ok(None),
    }
}

/// 读关闭墓碑：存在且能对上本空间的密钥，就说明这个空间已经被废弃。
///
/// 「仅创建」在部分 WebDAV 服务端上会被当成普通覆盖，`space.json` 里的关闭标记因此
/// 可能被迟到的创建改回开放；墓碑只有关闭路径写，读过 `space.json` 之后再查它一次，
/// 就能认出这种覆盖。
async fn read_closed(
    transport: &dyn SyncTransport,
    keys: &Keys,
    space_id: &str,
) -> Result<bool> {
    let Some(bytes) =
        transport::get_opt(transport, SPACE_CLOSED_FILE, SPACE_FILE_MAX as u64).await?
    else {
        return Ok(false);
    };
    let file = SpaceFile::decode(&bytes)?;
    file.verify(keys, space_id)?;
    Ok(file.closed)
}

/// 仅创建空间标记；目标已存在（另一个进程刚写入）时读回并核对，而不是把底层
/// 创建错误直接抛给调用方。
async fn create_marker(
    transport: &dyn SyncTransport,
    keys: &Keys,
    space_id: &str,
) -> Result<()> {
    let file = SpaceFile::new(keys, space_id);
    let written = transport::put_new_checked(transport, SPACE_FILE, file.encode()?).await;
    match written {
        Ok(()) => {
            // 墓碑在标记写入之后再查：最后一次观察留给只会从缺失变成存在的关闭证据。
            if read_closed(transport, keys, space_id).await? {
                return Err(SyncError::SpaceClosed);
            }
            Ok(())
        }
        Err(error) => {
            // 读回顺序不能反：标记可能被迟到的创建覆盖，先读标记、最后查墓碑，
            // 读到开放标记时关闭才确实还没完成。
            let existing = read_space(transport).await;
            let closed = read_closed(transport, keys, space_id).await?;
            match existing {
                Ok(Some(existing)) => {
                    existing.verify(keys, space_id)?;
                    if existing.closed || closed {
                        return Err(SyncError::SpaceClosed);
                    }
                    Ok(())
                }
                // 标记已经读不到了（另一设备的关闭把目录收了），墓碑在就是关闭。
                Ok(None) if closed => Err(SyncError::SpaceClosed),
                _ => Err(error.into()),
            }
        }
    }
}

/// 核对远端确实是本机所在的空间。`create` 为真时（本机创建了这个空间）不存在就写一个。
pub async fn open_space(
    transport: &dyn SyncTransport,
    keys: &Keys,
    space_id: &str,
    create: bool,
) -> Result<()> {
    let existing = match read_space(transport).await {
        Ok(existing) => existing,
        // 创建路径上读到损坏的标记（例如旧版「仅创建回退」在复制中途被打断留下的
        // 半个 space.json）。重建之前先重读并确认对象仍是刚读到的那份损坏内容：
        // 这期间另一设备可能刚写成一个可验证的关闭标记，不能被迟到重建覆盖。
        Err(SyncError::Corrupt(error)) if create => {
            let now = transport::get_opt(transport, SPACE_FILE, SPACE_FILE_MAX as u64).await?;
            match now {
                Some(bytes) => match SpaceFile::decode(&bytes) {
                    Ok(file) => {
                        file.verify(keys, space_id)?;
                        if file.closed || read_closed(transport, keys, space_id).await? {
                            return Err(SyncError::SpaceClosed);
                        }
                        return Ok(());
                    }
                    Err(_) => {
                        // 二次读仍损坏：不覆盖（另一设备可能刚写成有效的关闭标记，
                        // 无条件重建会把它改回开放）。保留损坏状态、给出恢复路径。
                        if read_closed(transport, keys, space_id).await? {
                            return Err(SyncError::SpaceClosed);
                        }
                        tracing::warn!("空间标记损坏（{error}）");
                        return Err(SyncError::other(format!(
                            "存储上的空间标记 {SPACE_FILE} 损坏（{error}）；请确认这不是别的设备正在写的标记后，删除或修复它再重试"
                        )));
                    }
                },
                None => {
                    // 已经被人删掉了：按缺失走仅创建（冲突时读回校验）。
                    return create_marker(transport, keys, space_id).await;
                }
            }
        }
        Err(error) => return Err(error),
    };
    match existing {
        Some(file) => {
            file.verify(keys, space_id)?;
            if file.closed || read_closed(transport, keys, space_id).await? {
                return Err(SyncError::SpaceClosed);
            }
            Ok(())
        }
        None if create => create_marker(transport, keys, space_id).await,
        None => {
            if read_closed(transport, keys, space_id).await? {
                return Err(SyncError::SpaceClosed);
            }
            Err(SyncError::other(
                "存储目录里还没有这个同步空间（可能文件还没同步过来，或存储地址填的不是同一个目录）",
            ))
        }
    }
}

/// 写一份关闭版墓碑。
async fn write_closed_tombstone(
    transport: &dyn SyncTransport,
    keys: &Keys,
    space_id: &str,
) -> Result<()> {
    let mut file = SpaceFile::new(keys, space_id);
    file.closed = true;
    transport::put_checked(transport, SPACE_CLOSED_FILE, file.encode()?).await?;
    Ok(())
}

/// 写一份关闭版兼容标记（旧版只认它）。
async fn write_closed_marker(
    transport: &dyn SyncTransport,
    keys: &Keys,
    space_id: &str,
) -> Result<()> {
    let mut file = SpaceFile::new(keys, space_id);
    file.closed = true;
    transport::put_checked(transport, SPACE_FILE, file.encode()?).await?;
    Ok(())
}

/// 读兼容标记的关闭状态：已验证关闭 `Some(true)`、已验证开放 `Some(false)`，
/// 缺失或解析不了 `None`（都不算依据）。身份不符、读取失败照常传播。
pub async fn marker_closed_state(
    transport: &dyn SyncTransport,
    keys: &Keys,
    space_id: &str,
) -> Result<Option<bool>> {
    match read_space(transport).await {
        Ok(Some(file)) => {
            file.verify(keys, space_id)?;
            Ok(Some(file.closed))
        }
        Ok(None) | Err(SyncError::Corrupt(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// 确认关闭墓碑在：兼容标记已经是关闭版、墓碑却没写成（上一代关闭或半次关闭）时补写。
///
/// 只凭「标记已关闭」就往下走的话，迟到创建把标记覆盖回开放后，新版也会重新接受这个
/// 旧空间；墓碑才是迟到覆盖拿不掉的关闭证据，提交重置前必须在。
pub async fn ensure_closed(
    transport: &dyn SyncTransport,
    keys: &Keys,
    space_id: &str,
) -> Result<()> {
    match read_closed(transport, keys, space_id).await {
        Ok(true) => Ok(()),
        Ok(false) => write_closed_tombstone(transport, keys, space_id).await,
        // 上一次 PUT 断开留下的半截墓碑（§10 允许非原子 PUT）：这里要写的就是关闭版，
        // 覆盖一个解析不了的对象不会把关闭变成开放。
        Err(SyncError::Corrupt(error)) => {
            tracing::warn!("关闭墓碑损坏，重写: {error}");
            write_closed_tombstone(transport, keys, space_id).await
        }
        // 读不出来（权限、网络、身份不符）也不能挡住关闭：到这里来的调用点都已经确认
        // 这个空间该关。要写的本来就是关闭版，不会把开放变封闭。
        Err(error) => {
            tracing::warn!("关闭墓碑读不出来，直接写: {error}");
            write_closed_tombstone(transport, keys, space_id).await
        }
    }
}

/// 把空间标成已废弃（「重置同步空间」的最后一步）。
pub async fn close_space(transport: &dyn SyncTransport, keys: &Keys, space_id: &str) -> Result<()> {
    // 先写墓碑、再改兼容标记：墓碑才是迟到创建拿不掉的关闭证据，放第一步保证任何
    // 半途失败都已经留下它；旧版的兼容标记随后补，写不成也不影响新版判关闭。
    // 已经有效的墓碑不重写：非原子 PUT 重复写可能把好的墓碑截断，而它正是关闭的依据。
    ensure_closed(transport, keys, space_id).await?;
    write_closed_marker(transport, keys, space_id).await
}

/// 只在远端确实显示「关闭已经开始」时把关闭补完：兼容标记已关闭，或墓碑已经写下。
///
/// 重置的记录可能在本机退出时被丢掉，之后靠一条独立的收尾记录调用这里；重置也可能
/// 压根没走到关闭这一步，那时不能替用户关掉一个还在用的旧空间。`known_closed` 用于
/// 本机在关闭现场确认过标记是关闭版、但墓碑没补上的情况：这个事实已经落盘，迟到的
/// 创建把标记改回开放也不影响，直接补完。返回是否补了关闭。
pub async fn ensure_closed_if_started(
    transport: &dyn SyncTransport,
    keys: &Keys,
    space_id: &str,
    known_closed: bool,
) -> Result<bool> {
    if known_closed {
        // 本机在关闭现场确认过标记已是关闭版：迟到创建可能把它改回开放，但「关闭已经
        // 开始」这个事实不受影响，直接补完。标记还在就核对身份，缺失或坏了都重写成
        // 关闭版（写的本来就是关闭，不会把开放变封闭）。
        match read_space(transport).await {
            Ok(Some(file)) => file.verify(keys, space_id)?,
            Ok(None) | Err(SyncError::Corrupt(_)) => {}
            Err(error) => return Err(error),
        }
        close_space(transport, keys, space_id).await?;
        return Ok(true);
    }
    let marker_closed = match read_space(transport).await {
        Ok(Some(file)) => {
            file.verify(keys, space_id)?;
            file.closed
        }
        // 标记缺失或坏了都不算关闭证据；坏标记后面由墓碑证据决定要不要重写成关闭版。
        Ok(None) | Err(SyncError::Corrupt(_)) => false,
        Err(error) => return Err(error),
    };
    if marker_closed {
        ensure_closed(transport, keys, space_id).await?;
        return Ok(true);
    }
    let started = match read_closed(transport, keys, space_id).await {
        Ok(closed) => closed,
        // 解析不了的墓碑（半截 PUT、超限、身份字段缺失）分不清是写坏的关闭还是别处
        // 误放的残片：不据此关掉一个可能还在用的空间。标记已关闭的分支上面已经补过墓碑。
        Err(SyncError::Corrupt(error)) => {
            tracing::warn!("关闭墓碑损坏，不据此补关闭: {error}");
            false
        }
        Err(error) => return Err(error),
    };
    if !started {
        return Ok(false);
    }
    // 墓碑已经读成有效的了：只补兼容标记，不再重复读、写墓碑。
    write_closed_marker(transport, keys, space_id).await?;
    Ok(true)
}

/// 列出各设备的 head（一次空闲轮询就是这一个请求）。
///
/// 存储端不可信：`heads/` 里可以被放进任意多个名字像设备 id 的文件，每个名字都要本机去读
/// 一次。多过 [`DEVICES_MAX`] 就整轮不做——这么多名字不可能是这个空间的设备写出来的，挑
/// 一部分读又分不出哪些是真的。
pub async fn list_heads(transport: &dyn SyncTransport) -> Result<Vec<ObjectMeta>> {
    let heads: Vec<ObjectMeta> = transport
        .list(HEADS_DIR)
        .await?
        .into_iter()
        .filter(|meta| ids::is_device_id(&meta.name) && meta.size as usize <= HEAD_OBJECT_MAX)
        .collect();
    check_head_count(heads.len())?;
    Ok(heads)
}

fn check_head_count(count: usize) -> Result<()> {
    if count > DEVICES_MAX {
        return Err(SyncError::other(format!(
            "存储上这个同步空间的设备文件多得不正常（{count} 个，上限 {DEVICES_MAX}）：\
             heads 目录可能被别的东西写过。请检查存储目录；确认无误的话，用「重置同步空间」换一个新的空间"
        )));
    }
    Ok(())
}

pub async fn read_head(
    transport: &dyn SyncTransport,
    keys: &Keys,
    device: &str,
) -> Result<Option<Head>> {
    let path = codec::head_path(device);
    match transport::get_opt(transport, &path, HEAD_OBJECT_MAX as u64).await? {
        Some(bytes) => Ok(Some(Head::decode(keys, device, &bytes)?)),
        None => Ok(None),
    }
}

/// 写 head，然后读回来核对（§9.3 第 6 步）。读回的不是刚写的那一份，说明另有写者。
pub async fn write_head(transport: &dyn SyncTransport, keys: &Keys, head: &Head) -> Result<()> {
    let path = codec::head_path(&head.dev);
    transport.put(&path, head.encode(keys)?).await?;
    let back = transport.get(&path, HEAD_OBJECT_MAX as u64).await?;
    let read = Head::decode(keys, &head.dev, &back)?;
    if &read != head {
        return Err(SyncError::other(
            "写出的 head 读回来不一样：可能有另一份数据在用同一个设备身份写入",
        ));
    }
    Ok(())
}

/// 取一个段：下载、解密、解压，核对摘要，解析。段可以有上百兆字节，下载之后的几步放到
/// 阻塞线程池上做。
pub async fn fetch_segment(
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    device: &str,
    name: &str,
    digest: &str,
) -> Result<Segment> {
    let path = codec::segment_path(device, name);
    let object = transport.get(&path, SEGMENT_OBJECT_MAX).await?;
    let (keys, device, digest) = (keys.clone(), device.to_string(), digest.to_string());
    blocking(move || {
        let plain = codec::open_log_object(&keys, &path, &object, SEGMENT_PLAIN_MAX)?;
        drop(object);
        if sha256_hex(&plain) != digest {
            return Err(SyncError::corrupt(format!(
                "{path} 的摘要与 head 里记的不符"
            )));
        }
        codec::parse_segment(&plain, &device)
    })
    .await
}

/// 取一个汇总包，核对指针里记的摘要。
pub async fn fetch_rollup(
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    device: &str,
    name: &str,
    digest: &str,
) -> Result<Rollup> {
    let path = codec::rollup_path(device, name);
    let object = transport.get(&path, ROLLUP_OBJECT_MAX).await?;
    let (keys, device, digest) = (keys.clone(), device.to_string(), digest.to_string());
    blocking(move || {
        let plain = codec::open_log_object(&keys, &path, &object, ROLLUP_PLAIN_MAX)?;
        drop(object);
        if sha256_hex(&plain) != digest {
            return Err(SyncError::corrupt(format!("{path} 的摘要与指针里记的不符")));
        }
        codec::parse_rollup(&plain, &device)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SpaceKey;
    use crate::transport::file::FileTransport;
    use crate::error::TransportError;
    use crate::transport::TransportResult;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn space_file_is_created_once_and_checked_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Keys::derive(&SpaceKey::generate());
        let space_id = crate::secrets::new_space_id();
        // 加入方不创建。
        assert!(open_space(&transport, &keys, &space_id, false)
            .await
            .is_err());
        open_space(&transport, &keys, &space_id, true)
            .await
            .unwrap();
        open_space(&transport, &keys, &space_id, false)
            .await
            .unwrap();
        // 另一把密钥对不上。
        let other = Keys::derive(&SpaceKey::generate());
        assert!(open_space(&transport, &other, &space_id, false)
            .await
            .is_err());
        // 废弃之后一律拒绝。
        close_space(&transport, &keys, &space_id).await.unwrap();
        assert!(matches!(
            open_space(&transport, &keys, &space_id, false).await,
            Err(SyncError::SpaceClosed)
        ));
    }

    #[tokio::test]
    async fn a_late_marker_overwrite_does_not_reopen_a_closed_space() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Keys::derive(&SpaceKey::generate());
        let space_id = crate::secrets::new_space_id();
        open_space(&transport, &keys, &space_id, true)
            .await
            .unwrap();
        close_space(&transport, &keys, &space_id).await.unwrap();
        // 不支持 `If-None-Match` 的服务端上，迟到的创建会把 space.json 无条件改回
        // 开放；墓碑还在，读标记之后必须仍判为已关闭。
        transport
            .put(SPACE_FILE, SpaceFile::new(&keys, &space_id).encode().unwrap())
            .await
            .unwrap();
        assert!(matches!(
            open_space(&transport, &keys, &space_id, false).await,
            Err(SyncError::SpaceClosed)
        ));
        assert!(matches!(
            open_space(&transport, &keys, &space_id, true).await,
            Err(SyncError::SpaceClosed)
        ));
    }

    #[tokio::test]
    async fn creating_over_a_tombstone_is_refused_after_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Keys::derive(&SpaceKey::generate());
        let space_id = crate::secrets::new_space_id();
        // 另一设备已经关了这个空间（最后写的是墓碑），本机的创建还没出发：既没有
        // 标记可读，「仅创建」也不会失败，只有写完再查墓碑能拦住。
        let mut closed = SpaceFile::new(&keys, &space_id);
        closed.closed = true;
        transport
            .put(SPACE_CLOSED_FILE, closed.encode().unwrap())
            .await
            .unwrap();
        assert!(matches!(
            create_marker(&transport, &keys, &space_id).await,
            Err(SyncError::SpaceClosed)
        ));
    }

    #[tokio::test]
    async fn a_half_done_close_is_completed_before_the_reset_is_committed() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Keys::derive(&SpaceKey::generate());
        let space_id = crate::secrets::new_space_id();
        open_space(&transport, &keys, &space_id, true)
            .await
            .unwrap();
        // 上一次关闭只写成了兼容标记（墓碑那次 PUT 失败）。
        let mut closed = SpaceFile::new(&keys, &space_id);
        closed.closed = true;
        transport
            .put(SPACE_FILE, closed.encode().unwrap())
            .await
            .unwrap();
        ensure_closed(&transport, &keys, &space_id).await.unwrap();
        // 幂等：墓碑已经在就不重复写。
        ensure_closed(&transport, &keys, &space_id).await.unwrap();
        // 补上墓碑之后，迟到创建把标记覆盖回开放也拒绝打开。
        transport
            .put(SPACE_FILE, SpaceFile::new(&keys, &space_id).encode().unwrap())
            .await
            .unwrap();
        assert!(matches!(
            open_space(&transport, &keys, &space_id, false).await,
            Err(SyncError::SpaceClosed)
        ));
    }

    #[tokio::test]
    async fn the_marker_state_is_read_only_when_it_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Keys::derive(&SpaceKey::generate());
        let space_id = crate::secrets::new_space_id();
        assert_eq!(
            marker_closed_state(&transport, &keys, &space_id)
                .await
                .unwrap(),
            None
        );
        open_space(&transport, &keys, &space_id, true)
            .await
            .unwrap();
        assert_eq!(
            marker_closed_state(&transport, &keys, &space_id)
                .await
                .unwrap(),
            Some(false)
        );
        close_space(&transport, &keys, &space_id).await.unwrap();
        assert_eq!(
            marker_closed_state(&transport, &keys, &space_id)
                .await
                .unwrap(),
            Some(true)
        );
    }

    #[tokio::test]
    async fn a_truncated_tombstone_does_not_block_the_close_from_being_completed() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Keys::derive(&SpaceKey::generate());
        let space_id = crate::secrets::new_space_id();
        open_space(&transport, &keys, &space_id, true)
            .await
            .unwrap();
        // 关闭的第一次 PUT 断了，留下半截墓碑（§10 允许非原子 PUT）。
        transport
            .put(SPACE_CLOSED_FILE, b"{\"format\":1,\"space".to_vec())
            .await
            .unwrap();
        ensure_closed(&transport, &keys, &space_id).await.unwrap();
        // 墓碑被重写成完整的一份：开放标记 + 有效墓碑仍然判关闭。
        assert!(matches!(
            open_space(&transport, &keys, &space_id, false).await,
            Err(SyncError::SpaceClosed)
        ));
    }

    #[tokio::test]
    async fn the_leftover_close_is_only_finished_when_it_has_started() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Keys::derive(&SpaceKey::generate());
        let space_id = crate::secrets::new_space_id();
        // 还没开始关闭：开放标记、没有墓碑 → 什么都不做。
        open_space(&transport, &keys, &space_id, true)
            .await
            .unwrap();
        assert!(
            !ensure_closed_if_started(&transport, &keys, &space_id, false)
                .await
                .unwrap()
        );
        // 关闭开了个头（墓碑先写成、兼容标记没写成）→ 补完两件。
        let mut closed = SpaceFile::new(&keys, &space_id);
        closed.closed = true;
        transport
            .put(SPACE_CLOSED_FILE, closed.encode().unwrap())
            .await
            .unwrap();
        assert!(
            ensure_closed_if_started(&transport, &keys, &space_id, false)
                .await
                .unwrap()
        );
        assert!(matches!(
            open_space(&transport, &keys, &space_id, false).await,
            Err(SyncError::SpaceClosed)
        ));
        // 幂等。
        assert!(
            ensure_closed_if_started(&transport, &keys, &space_id, false)
                .await
                .unwrap()
        );
        // 旧版关闭只写了兼容标记：补墓碑。
        transport.delete(SPACE_CLOSED_FILE).await.unwrap();
        assert!(
            ensure_closed_if_started(&transport, &keys, &space_id, false)
                .await
                .unwrap()
        );
        assert!(matches!(
            open_space(&transport, &keys, &space_id, false).await,
            Err(SyncError::SpaceClosed)
        ));
        // 关闭在现场确认过：迟到创建把标记改回开放也照样补完。
        transport
            .put(SPACE_FILE, SpaceFile::new(&keys, &space_id).encode().unwrap())
            .await
            .unwrap();
        transport.delete(SPACE_CLOSED_FILE).await.unwrap();
        assert!(
            ensure_closed_if_started(&transport, &keys, &space_id, true)
                .await
                .unwrap()
        );
        assert!(matches!(
            open_space(&transport, &keys, &space_id, false).await,
            Err(SyncError::SpaceClosed)
        ));
    }

    #[tokio::test]
    async fn unreadable_close_evidence_does_not_close_a_live_space() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Keys::derive(&SpaceKey::generate());
        let space_id = crate::secrets::new_space_id();
        open_space(&transport, &keys, &space_id, true)
            .await
            .unwrap();
        // 墓碑键上是解析不了的残片（可能来自别处），不是写坏的关闭：不能据此关掉
        // 一个还在用的空间。
        transport
            .put(SPACE_CLOSED_FILE, b"{\"format\":1,\"space".to_vec())
            .await
            .unwrap();
        assert!(
            !ensure_closed_if_started(&transport, &keys, &space_id, false)
                .await
                .unwrap()
        );
        assert!(!read_space(&transport).await.unwrap().unwrap().closed);
    }

    /// 测试存储：不支持条件写入；标记被读过之前墓碑不可见（模拟关闭正好落在仅创建
    /// 失败之后的读回中途）；`put_new` 一律报错（本机的创建没落到存储上）。
    struct LateClose {
        inner: FileTransport,
        marker_reads: AtomicUsize,
        closed: Vec<u8>,
    }

    #[async_trait::async_trait]
    impl SyncTransport for LateClose {
        async fn list(&self, prefix: &str) -> TransportResult<Vec<ObjectMeta>> {
            self.inner.list(prefix).await
        }

        async fn get(&self, key: &str, max: u64) -> TransportResult<Vec<u8>> {
            if key == SPACE_CLOSED_FILE {
                if self.marker_reads.load(Ordering::SeqCst) == 0 {
                    return Err(TransportError::NotFound(SPACE_CLOSED_FILE.into()));
                }
                return Ok(self.closed.clone());
            }
            if key == SPACE_FILE {
                self.marker_reads.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.get(key, max).await
        }

        async fn put(&self, key: &str, body: Vec<u8>) -> TransportResult<()> {
            self.inner.put(key, body).await
        }

        async fn put_new(&self, _key: &str, _body: Vec<u8>) -> TransportResult<()> {
            Err(TransportError::Other("写入在存储生效前失败".into()))
        }

        async fn delete(&self, key: &str) -> TransportResult<()> {
            self.inner.delete(key).await
        }

        fn kind(&self) -> &'static str {
            "test"
        }
    }

    #[tokio::test]
    async fn a_close_that_lands_during_the_readback_is_not_missed() {
        let dir = tempfile::tempdir().unwrap();
        let inner = FileTransport::new(dir.path());
        let keys = Keys::derive(&SpaceKey::generate());
        let space_id = crate::secrets::new_space_id();
        // 迟到的创建已经把 space.json 改回开放，本机的仅创建还没落到存储上。
        inner
            .put(SPACE_FILE, SpaceFile::new(&keys, &space_id).encode().unwrap())
            .await
            .unwrap();
        let mut closed = SpaceFile::new(&keys, &space_id);
        closed.closed = true;
        let transport = LateClose {
            inner,
            marker_reads: AtomicUsize::new(0),
            closed: closed.encode().unwrap(),
        };
        // 读回开放标记之后才看到墓碑：必须按已关闭处理，先查墓碑的旧顺序会把这次
        // 关闭漏掉。
        assert!(matches!(
            create_marker(&transport, &keys, &space_id).await,
            Err(SyncError::SpaceClosed)
        ));
    }

    #[tokio::test]
    async fn head_round_trips_and_missing_head_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Keys::derive(&SpaceKey::generate());
        let device = ids::new_device_id();
        assert!(read_head(&transport, &keys, &device)
            .await
            .unwrap()
            .is_none());
        let head = Head {
            v: 1,
            dev: device.clone(),
            instance: ids::new_instance_id(),
            last_seq: 0,
            last_seg: None,
            segs: Vec::new(),
            rollup: None,
            floor: 0,
            acks: Default::default(),
            running: Vec::new(),
            snap: None,
            ts: 1,
        };
        write_head(&transport, &keys, &head).await.unwrap();
        assert_eq!(
            read_head(&transport, &keys, &device).await.unwrap(),
            Some(head)
        );
        let listed = list_heads(&transport).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, device);
    }

    #[test]
    fn an_absurd_number_of_heads_stops_the_round() {
        check_head_count(DEVICES_MAX).unwrap();
        assert!(check_head_count(DEVICES_MAX + 1).is_err());
    }
}
