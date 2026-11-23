//! 远端对象的读写：`space.json`、head、段、汇总包（§9.1）。
//!
//! 这里的传输都已经套在空间自己的子目录里（[`crate::transport::Scoped`]）。读到的东西
//! 一律先解密、校验，再交给上层。

use crate::codec::{
    self, Head, Rollup, Segment, SpaceFile, DEVICES_MAX, HEADS_DIR, HEAD_OBJECT_MAX,
    ROLLUP_OBJECT_MAX, ROLLUP_PLAIN_MAX, SEGMENT_OBJECT_MAX, SEGMENT_PLAIN_MAX, SPACE_FILE,
    SPACE_FILE_MAX,
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

/// 仅创建空间标记；目标已存在（另一个进程刚写入）时读回并核对，而不是把底层
/// 创建错误直接抛给调用方。
async fn create_marker(
    transport: &dyn SyncTransport,
    keys: &Keys,
    space_id: &str,
) -> Result<()> {
    let file = SpaceFile::new(keys, space_id);
    if let Err(error) = transport::put_new_checked(transport, SPACE_FILE, file.encode()?).await {
        match read_space(transport).await {
            Ok(Some(existing)) => {
                existing.verify(keys, space_id)?;
                if existing.closed {
                    return Err(SyncError::SpaceClosed);
                }
                Ok(())
            }
            _ => Err(error.into()),
        }
    } else {
        Ok(())
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
                        if file.closed {
                            return Err(SyncError::SpaceClosed);
                        }
                        return Ok(());
                    }
                    Err(_) => {
                        tracing::warn!("空间标记损坏（{error}），按创建路径重建");
                        let file = SpaceFile::new(keys, space_id);
                        transport::put_checked(transport, SPACE_FILE, file.encode()?).await?;
                        return Ok(());
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
            if file.closed {
                return Err(SyncError::SpaceClosed);
            }
            Ok(())
        }
        None if create => create_marker(transport, keys, space_id).await,
        None => Err(SyncError::other(
            "存储目录里还没有这个同步空间（可能文件还没同步过来，或存储地址填的不是同一个目录）",
        )),
    }
}

/// 把空间标成已废弃（「重置同步空间」的最后一步）。
pub async fn close_space(transport: &dyn SyncTransport, keys: &Keys, space_id: &str) -> Result<()> {
    let mut file = SpaceFile::new(keys, space_id);
    file.closed = true;
    transport::put_checked(transport, SPACE_FILE, file.encode()?).await?;
    Ok(())
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
