//! 附件的上传与下载（§8.7）。
//!
//! 它们不占同步轮次：两个任务和轮次一样跑在引擎线程上，在轮次等网络、等磁盘的间隙里
//! 穿插进行，所以一个大附件不会拖住会话的同步。对网络类型没有要求。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use rclaw_store::sync_db;
use tokio::sync::mpsc;

use super::space::Live;
use super::Shared;
use crate::attach;
use crate::db;
use crate::error::{Result, SyncError, TransportError};
use crate::{blocking, SyncEvent};

/// 上传队列一批取这么多个；批与批之间看一眼是不是暂停了。
const UPLOAD_BATCH: usize = 8;
/// 存储够不着时，上传隔这么久再试（逐次翻倍到上限）。一轮同步成功也会提前叫醒它。
const UPLOAD_RETRY_MIN: Duration = Duration::from_secs(5);
const UPLOAD_RETRY_MAX: Duration = Duration::from_secs(600);
/// 存储限流时最多等这么久（和轮次用的上限一样）。
const UPLOAD_RATE_LIMIT_MAX: Duration = Duration::from_secs(900);
/// 同时进行的下载数。
const DOWNLOADS: usize = 3;

/// 一批上传没成之后等多久再试，以及这段等待能不能被提前叫醒。
///
/// 存储限流：照它给的时间等（没给就用退避的间隔），而且等满——这段时间里再发请求只会让
/// 限流续下去，所以一轮同步成功也不提前叫醒它。其余的失败：按退避的间隔等，一轮同步
/// 成功、恢复同步都可以提前叫醒。
fn upload_wait(error: &SyncError, retry: Duration) -> (Duration, bool) {
    match error {
        SyncError::Transport(TransportError::RateLimited { retry_after, .. }) => (
            retry_after
                .unwrap_or(retry)
                .clamp(UPLOAD_RETRY_MIN, UPLOAD_RATE_LIMIT_MAX),
            false,
        ),
        _ => (retry, true),
    }
}

/// 一个取回附件的请求。
pub(crate) struct FetchReq {
    pub blob: String,
    /// 用户点名要的（或者续聊要用）：排到队首，取不到要告诉界面。
    pub user: bool,
}

/// 上传任务：每次被叫醒（一轮同步结束、用户选了存量附件的范围、恢复同步）就把排着队的
/// 附件传完。传不传「同步附件」范围之外的东西不归它管——排进队列的都传（别的设备点名
/// 要的、技能里的大文件也走这条队列）。
pub(crate) async fn upload_task(shared: Arc<Shared>, live: Arc<Live>) {
    let mut retry = UPLOAD_RETRY_MIN;
    loop {
        shared.uploads.notified().await;
        loop {
            // 暂停了，或者上一轮同步没访问成存储（离线、密码不对、空间被重置）：等下一次
            // 被叫醒——恢复同步、一轮同步成功都会叫。
            if shared.paused.load(Ordering::Relaxed) || !shared.storage_ok.load(Ordering::Relaxed) {
                break;
            }
            let batch =
                attach::upload_pending(&shared.ctx, live.transport(), &live.keys, UPLOAD_BATCH)
                    .await;
            match batch {
                Ok(batch) if batch.taken == 0 => break,
                Ok(batch) => {
                    retry = UPLOAD_RETRY_MIN;
                    // 面板上「等着上传的附件数」变了。
                    shared.emit(SyncEvent::Status);
                    // 有附件本机已经拿不出来了：这句话（`blob.gone`）排进了待发队列，
                    // 现在就发出去，别的设备不用接着等。
                    if batch.said > 0 {
                        shared.kick.notify_one();
                    }
                }
                Err(error) => {
                    let (wait, wakeable) = upload_wait(&error, retry);
                    tracing::debug!("附件上传暂时不通（{wait:?} 后再试）: {error}");
                    if wakeable {
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => {}
                            _ = shared.uploads.notified() => {}
                        }
                    } else {
                        tokio::time::sleep(wait).await;
                    }
                    retry = (retry * 2).min(UPLOAD_RETRY_MAX);
                }
            }
        }
    }
}

async fn fetch_one(shared: Arc<Shared>, live: Arc<Live>, blob: String) -> (String, Result<()>) {
    let max_size = shared.attachment_max.load(Ordering::Relaxed);
    let result = attach::download(&shared.ctx, live.transport(), &live.keys, &blob, max_size)
        .await
        .map(|_| ());
    (blob, result)
}

/// 下载任务：按请求取回附件，同时最多 [`DOWNLOADS`] 个，用户点名要的插到队首。
pub(crate) async fn download_task(
    shared: Arc<Shared>,
    live: Arc<Live>,
    mut requests: mpsc::UnboundedReceiver<FetchReq>,
) {
    let mut queue: VecDeque<String> = VecDeque::new();
    // 排着队的和正在取的：是不是用户点名要的。
    let mut wanted: HashMap<String, bool> = HashMap::new();
    let mut running = FuturesUnordered::new();
    loop {
        while running.len() < DOWNLOADS {
            let Some(blob) = queue.pop_front() else {
                break;
            };
            running.push(fetch_one(shared.clone(), live.clone(), blob));
        }
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else {
                    break;
                };
                match wanted.get_mut(&request.blob) {
                    // 已经在排队或正在取。用户又点了它：往前挪。
                    Some(user) => {
                        if request.user && !*user {
                            *user = true;
                            if let Some(at) = queue.iter().position(|blob| *blob == request.blob) {
                                queue.remove(at);
                                queue.push_front(request.blob);
                            }
                        }
                    }
                    None => {
                        wanted.insert(request.blob.clone(), request.user);
                        if request.user {
                            queue.push_front(request.blob);
                        } else {
                            queue.push_back(request.blob);
                        }
                    }
                }
            }
            Some((blob, result)) = running.next(), if !running.is_empty() => {
                let user = wanted.remove(&blob).unwrap_or(false);
                finish(&shared, blob, user, result).await;
            }
        }
    }
}

/// 一个下载有了结果：通知界面。存储上没有这个对象的，向别的设备要——要了很久没有、
/// 或者持有它的设备说拿不出来了，就收场（[`attach::on_missing`]）。
async fn finish(shared: &Arc<Shared>, blob: String, user: bool, result: Result<()>) {
    let error = match result {
        Ok(()) => None,
        // 对象不在存储上，与对象在但内容对不上（长度相同却解不开/摘要不符）一样：这本机
        // 都拿不到它。按「缺」走同一套簿记——向持有它的设备发一条有界的 `blob.want`，
        // 持有方重新排队上传时会核对明文摘要、用正确的内容盖掉坏的。
        Err(error) if error.is_not_found() || matches!(error, SyncError::Corrupt(_)) => {
            // 记账是一个提交阶段：换空间期间作废，不往新空间的库里写旧空间的等待记录。
            let commit = shared.ctx.space.begin();
            if !commit.valid() {
                return;
            }
            // 这条登记是不是「重新下载」临时补的？是才在失败时撤掉它（保留 `gone`
            // 记录，文件放回原处后别的设备来要仍能重新上传）。
            let probe = shared
                .probes
                .lock()
                .map(|mut probes| probes.remove(&blob))
                .unwrap_or(false);
            let (ctx, wanted) = (shared.ctx.clone(), blob.clone());
            let missing = blocking(move || {
                if !commit.valid() {
                    return Ok(None);
                }
                db::write(ctx.db(), |tx| {
                    // 取的写事务可能排在换空间后面：拿到锁之后在事务里再核对一次，
                    // 旧空间的等待记录不写进新空间的库。
                    if !commit.valid() {
                        return Ok(None);
                    }
                    // 这次是为「重新下载」临时补的登记、存储上又确实没有：撤掉它（保留
                    // `gone` 记录——文件放回原处之后别的设备来要时还能重新上传）。
                    // 不是这次补的登记（别的消息正常登记进来的）走下面的常规路径。
                    if probe && db::state_get(tx, crate::db::scope::BLOB_GONE, &wanted)?.is_some() {
                        db::blob_delete(tx, &wanted)?;
                        return Ok(None);
                    }
                    let Some(device) = sync_db::device_id(tx)? else {
                        return Ok(None);
                    };
                    attach::on_missing(tx, &device, &wanted, user, attach::now_ms()).map(Some)
                })
            })
            .await;
            match missing {
                Ok(Some(missing)) => {
                    let waiting = matches!(missing, attach::Missing::Waiting { .. });
                    let asked = matches!(missing, attach::Missing::Waiting { asked: true, .. });
                    if waiting {
                        // 别的设备传上来之后没有通知：以后的轮次隔一阵再来取。
                        shared.wants.store(true, Ordering::Relaxed);
                    }
                    if asked {
                        // 刚排进队列的 `blob.want` 现在就发出去。
                        shared.kick.notify_one();
                    }
                    // 没取到不算出错：这个附件现在的情况（在等别的设备 / 取不回来了）记
                    // 在本机，界面重新查一遍就看得到，说法由界面按情况给。隔一阵自动
                    // 重试、情况又没变的不通知。
                    if !user && waiting && !asked {
                        return;
                    }
                    None
                }
                // 本机不在同步空间里了：没有可记的，界面重新查一遍就是空的。
                Ok(None) => None,
                Err(error) => {
                    tracing::debug!("登记要取的附件 {blob} 失败: {error}");
                    Some(format!("取回附件失败：{error}"))
                }
            }
        }
        Err(error) => {
            tracing::debug!("取回附件 {blob} 失败: {error}");
            // 这次是「重新下载」临时补的登记、又没取成（连接断了/超时）：撤掉它，别
            // 挡住「文件放回原处后重新上传」的那条路。
            let probe = shared
                .probes
                .lock()
                .map(|mut probes| probes.remove(&blob))
                .unwrap_or(false);
            if probe {
                let (ctx, wanted) = (shared.ctx.clone(), blob.clone());
                let cleanup = blocking(move || {
                    crate::engine::drop_remote_registration(&ctx, &wanted);
                    Ok(())
                });
                cleanup.await.ok();
            }
            Some(format!("取回附件失败：{error}"))
        }
    };
    match error {
        None => shared.emit(SyncEvent::Attachment { blob, error: None }),
        // 自动取的（小图、打开会话时预取的、隔一阵的重试）失败了不打扰用户，占位照旧。
        Some(error) if user => shared.emit(SyncEvent::Attachment {
            blob,
            error: Some(error),
        }),
        Some(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limited(retry_after: Option<u64>) -> SyncError {
        SyncError::Transport(TransportError::RateLimited {
            retry_after: retry_after.map(Duration::from_secs),
            message: "429".into(),
        })
    }

    #[test]
    fn a_rate_limited_upload_waits_out_what_the_storage_asked_for() {
        let retry = Duration::from_secs(20);
        // 限流：照存储给的时间等满，不被提前叫醒；给得离谱的收到上限以内。
        assert_eq!(
            upload_wait(&limited(Some(1800)), retry),
            (UPLOAD_RATE_LIMIT_MAX, false)
        );
        assert_eq!(
            upload_wait(&limited(Some(120)), retry),
            (Duration::from_secs(120), false)
        );
        assert_eq!(
            upload_wait(&limited(Some(0)), retry),
            (UPLOAD_RETRY_MIN, false)
        );
        // 没给时间：用退避的间隔，同样等满。
        assert_eq!(upload_wait(&limited(None), retry), (retry, false));
        // 别的失败：退避的间隔，一轮同步成功可以提前叫醒。
        let offline = SyncError::Transport(TransportError::Timeout("x".into()));
        assert_eq!(upload_wait(&offline, retry), (retry, true));
    }
}
