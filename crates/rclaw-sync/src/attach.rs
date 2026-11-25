//! 附件（§8.7）：被消息引用的工作区文件，和放不进 op 的大工具结果全文。
//!
//! 附件对象内容寻址：对象名是内容摘要的带密钥摘要，同名即同内容，重复写无害。本机的
//! 登记在 `sync_blob`：没上传的、等上传的、已上传的、在别的设备上等取回的、已取回的。
//! 「同步附件」这个范围只决定传不传；消息里的引用清单只要会话在同步就照常记录，以后
//! 打开附件同步时存量附件才有清单可依。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::params;

use crate::codec;
use crate::crypto::{sealed_blob_len, sha256_reader, HashingReader, Keys, BLOB_CHUNK};
use crate::db::{
    self, scope, BlobRow, BLOB_FETCHED, BLOB_HELD, BLOB_PENDING, BLOB_REMOTE, BLOB_UPLOADED,
};
use crate::error::{Result, SyncError, TransportError};
use crate::ids;
use crate::op::{FileRef, Op};
use crate::portable::{self, MARK_ROOT};
use crate::transport::{self, SyncTransport};
use crate::wire::{self, Env, LocalBlob};
use crate::{offload, Commit, Ctx};

/// 一条消息最多登记这么多个被引用的文件。
const MAX_REFS_PER_MESSAGE: usize = 64;
/// 正文里一处路径最多试这么多种截法。
const MAX_CUTS: usize = 12;

const REF_DIRS: [&str; 3] = ["uploads/", "media/", "output/"];

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '/' | '_' | '-' | '.' | '(' | ')' | ' ' | '+' | '@' | '~' | ',' | '[' | ']'
        )
        || ('\u{4e00}'..='\u{9fff}').contains(&c)
        || ('\u{ff00}'..='\u{ffef}').contains(&c)
}

/// 一段正文里可能是工作区文件的相对路径：媒体回执行里的 `path="…"`，以及正文里写出来
/// 的 `uploads/…`、`media/…`、`output/…`。每处给出从长到短的几种截法（文件名里可以有
/// 空格和中文，路径后面又往往紧跟着别的字），由调用方挑出真实存在的那一个。
fn path_candidates(text: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let normalize = |raw: &str| -> Option<String> {
        let path = raw.replace('\\', "/");
        let path = match path.rfind(MARK_ROOT) {
            Some(at) => path[at + MARK_ROOT.len()..].to_string(),
            None => path,
        };
        let path = path.trim().trim_start_matches("./").to_string();
        portable::is_ref_path(&path).then_some(path)
    };
    // 1) 媒体回执行。
    let mut rest = text;
    while let Some(start) = rest.find("path=\"") {
        let tail = &rest[start + 6..];
        let Some(end) = tail.find('"') else { break };
        if let Some(path) = normalize(&tail[..end]) {
            out.push(vec![path]);
        }
        rest = &tail[end..];
    }
    // 2) 正文里的相对路径。
    for dir in REF_DIRS {
        let mut from = 0usize;
        while let Some(found) = text[from..].find(dir) {
            let at = from + found;
            from = at + dir.len();
            let boundary = text[..at]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.'));
            if !boundary {
                continue;
            }
            let tail: String = text[at..]
                .chars()
                .take_while(|c| is_path_char(*c))
                .take(300)
                .collect();
            // 截法：整段，然后在每个空格、右括号、逗号、中文与西文的交界处往回截。
            let mut cuts: Vec<String> = Vec::new();
            let chars: Vec<(usize, char)> = tail.char_indices().collect();
            let mut ends: Vec<usize> = vec![tail.len()];
            for window in chars.windows(2).rev() {
                let ((_, a), (pos, b)) = (window[0], window[1]);
                let soft = matches!(b, ' ' | ')' | ',' | '(' | '[' | ']')
                    || (a.is_ascii() && !b.is_ascii())
                    || (b == '.' && !a.is_ascii());
                if soft {
                    ends.push(pos);
                }
            }
            for end in ends {
                let cut = tail[..end].trim_end_matches(|c: char| {
                    c.is_whitespace() || matches!(c, '.' | ',' | ')' | '(' | '[' | ']')
                });
                if let Some(path) = normalize(cut) {
                    if !cuts.contains(&path) {
                        cuts.push(path);
                    }
                }
                if cuts.len() >= MAX_CUTS {
                    break;
                }
            }
            if !cuts.is_empty() {
                out.push(cuts);
            }
        }
    }
    out
}

/// 从一条（可移植化之后的）消息里找出它引用的工作区文件，算出各自的附件对象名。
/// 只做文件 IO，不碰数据库。超过 `max_size` 的文件不登记。
pub fn extract_refs(
    env: &Env,
    keys: &Keys,
    portable_data: &str,
    max_size: u64,
) -> Vec<(FileRef, LocalBlob)> {
    let mut out: Vec<(FileRef, LocalBlob)> = Vec::new();
    for text in portable::text_leaves(portable_data) {
        if !REF_DIRS.iter().any(|dir| text.contains(dir)) {
            continue;
        }
        for cuts in path_candidates(&text) {
            if out.len() >= MAX_REFS_PER_MESSAGE {
                return out;
            }
            // 只认本机上原样存在的文件（`resolve_exact`）：名字被改写过的路径指向另一个
            // 文件，不能算在这条引用头上。
            let Some((path, local)) = cuts.into_iter().find_map(|path| {
                let local = env.resolve_exact(&path)?;
                env.check_path(&path, &local).ok()?;
                std::fs::metadata(&local)
                    .is_ok_and(|meta| meta.is_file() && meta.len() > 0 && meta.len() <= max_size)
                    .then_some((path, local))
            }) else {
                continue;
            };
            if out.iter().any(|(file, _)| file.path == path) {
                continue;
            }
            // 登记的长度取算摘要时实际读到的字节数，不取刚才看的那一眼：文件可能正在被
            // 写。对象名和长度就此出自同一份内容，别的设备按它核对对象的长度。
            let Some((blob_id, size)) = hash_file_len(keys, &local) else {
                continue;
            };
            if size == 0 || size > max_size {
                continue;
            }
            out.push((
                FileRef {
                    path: path.clone(),
                    blob: blob_id.clone(),
                    size,
                },
                LocalBlob {
                    blob_id,
                    rel_path: path,
                    size,
                },
            ));
        }
    }
    out
}

/// 一个本机文件在这把密钥下的附件对象名；读不了返回 `None`。
pub(crate) fn hash_file(keys: &Keys, path: &Path) -> Option<String> {
    hash_file_len(keys, path).map(|(blob_id, _)| blob_id)
}

/// 同 [`hash_file`]，连同算摘要时读到的字节数：这个名字对应的内容就是这么长。
pub(crate) fn hash_file_len(keys: &Keys, path: &Path) -> Option<(String, u64)> {
    let mut file = std::fs::File::open(path).ok()?;
    let (digest, len) = sha256_reader(&mut file).ok()?;
    Some((keys.blob_id(&digest), len))
}

/// 取回一个登记长度为 `size`（明文）的附件对象时，对象最长可以有多长。
///
/// 对象的长度本来是定死的（[`sealed_blob_len`]）；这里多给一块的余量。上限的用处是不让
/// 存储端拿一个没完没了的对象把本机磁盘写满，宽这一块不碍事；而登记的长度是别的设备
/// 报来的，万一差了一点，不该让一个内容完好的附件就此取不回来。
fn download_max(size: u64) -> u64 {
    sealed_blob_len(size).saturating_add(sealed_blob_len(BLOB_CHUNK as u64))
}

fn tmp_file(ctx: &Ctx, tag: &str) -> PathBuf {
    ctx.paths
        .tmp
        .join(format!("{tag}-{}", ids::random_hex::<8>()))
}

/// 本次任务写下的临时文件：任务被取消（换空间、清理）时 `Drop` 就把它删掉，不等调用
/// 方走到那一步的去删。明文与密文都用它兜底。
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::debug!("清理临时文件 {} 失败: {e}", self.0.display()),
        }
    }
}

/// 换空间 / 清理时这次附件任务的提交作废。
fn abandoned() -> SyncError {
    SyncError::other("同步空间已经切换或清理，这次附件的提交作废")
}

/// 本机登记的一个附件对象现在是什么情况。打开、存入工作区、取回、重新封装共用这一份
/// 判据：登记的对象名可能出自旧版本（内联缓存按明文 SHA-256 登记），这时以本机文件内容
/// 为准换成正确的对象名，而不是拿「带密钥摘要 vs 明文摘要」直接判坏。
pub(crate) enum LocalObject {
    /// 没有这个登记，或它登记在别的设备上：这里不做判定。
    Unknown,
    /// 登记说本机有，文件却不在了。
    Absent,
    /// 文件在，内容对不上登记：不能当作这个对象。
    Mismatch,
    /// 文件就是登记的那一份；旧版明文摘要登记已经归一成线上对象名。
    Intact(BlobRow),
}

/// 按对象名看本机的登记。`blob_id` 可能是旧版本按明文摘要登记的内联缓存：内容对得上
/// 就当场归一（登记与消息清单一起改），返回的登记里已经是线上对象名。
pub(crate) fn check_local_object(ctx: &Ctx, keys: &Keys, blob_id: &str) -> Result<LocalObject> {
    let row = db::read(ctx.db(), |conn| db::blob_get(conn, blob_id))?;
    let Some(row) = row else {
        return Ok(LocalObject::Unknown);
    };
    if row.state == BLOB_REMOTE {
        return Ok(LocalObject::Unknown);
    }
    check_local_row(ctx, keys, row)
}

fn check_local_row(ctx: &Ctx, keys: &Keys, row: BlobRow) -> Result<LocalObject> {
    // 路径不安全（经过链接、拼不出本机的名字）：按「验不了」处理，由别处照常兜底。
    let Ok(path) = ctx.env.checked_path(&row.rel_path) else {
        return Ok(LocalObject::Unknown);
    };
    match std::fs::metadata(&path) {
        Ok(meta) if !meta.is_file() => return Ok(LocalObject::Mismatch),
        Ok(_) => {}
        Err(e) if is_absent(&e) => return Ok(LocalObject::Absent),
        Err(e) => return Err(e.into()),
    }
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if is_absent(&e) => return Ok(LocalObject::Absent),
        Err(e) => return Err(e.into()),
    };
    let (digest, len) = sha256_reader(&mut file)?;
    let current = keys.blob_id(&digest);
    if current == row.blob_id {
        return Ok(LocalObject::Intact(row));
    }
    if crate::ids::hex(&digest) != row.blob_id {
        return Ok(LocalObject::Mismatch);
    }
    // 旧版本按明文摘要登记的内联缓存：内容对得上，对象名按空间密钥重算并改登记。
    let normalized = normalize_registration(ctx, &row.blob_id, &current, &row.rel_path, len)?;
    Ok(LocalObject::Intact(normalized))
}

/// 把旧版本按明文摘要登记的本机缓存归一成线上对象名：登记换成新的，消息的附件清单一起
/// 改写。新对象名已经登记过（另一条消息先归一过）时保留它原来的落点。
fn normalize_registration(
    ctx: &Ctx,
    old: &str,
    new: &str,
    rel_path: &str,
    size: u64,
) -> Result<BlobRow> {
    db::write(ctx.db(), |tx| {
        // 新对象名已经登记过（另一条消息先归一过，或同一份内容从别的设备取回过）：
        // 保留它的状态，落点换成本次核对过的这一份——两份内容相同。
        match db::blob_get(tx, new)? {
            Some(existing) => {
                let state = if existing.state == BLOB_REMOTE {
                    BLOB_FETCHED
                } else {
                    existing.state
                };
                if existing.rel_path != rel_path || state != existing.state {
                    db::blob_set(tx, new, rel_path, state)?;
                }
            }
            None => {
                db::blob_register(tx, new, rel_path, size, BLOB_FETCHED)?;
            }
        }
        replace_refs_blob(tx, old, new)?;
        tx.execute("DELETE FROM sync_blob WHERE blob_id = ?1", params![old])?;
        db::blob_get(tx, new)?.ok_or_else(|| SyncError::other("附件登记刚刚被清理"))
    })
}

/// 消息的附件清单里，旧对象名换成新的。分批扫，每个事务都短。
fn replace_refs_blob(conn: &rusqlite::Connection, old: &str, new: &str) -> Result<()> {
    let pattern = format!("%{old}%");
    let mut after = 0i64;
    loop {
        let batch: Vec<(i64, String)> = {
            let mut stmt = conn.prepare_cached(
                "SELECT message_id, refs FROM sync_msg WHERE message_id > ?1 AND refs LIKE ?2 \
                 ORDER BY message_id LIMIT 500",
            )?;
            let rows = stmt.query_map(params![after, &pattern], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let Some((last, _)) = batch.last() else {
            break;
        };
        after = *last;
        for (message_id, column) in batch {
            let Some(mut refs) = wire::StoredRefs::parse(Some(&column)) else {
                continue;
            };
            let mut changed = false;
            for file in &mut refs.files {
                if file.blob == old {
                    file.blob = new.to_string();
                    changed = true;
                }
            }
            for artifact in &mut refs.artifacts {
                if artifact.blob.as_deref() == Some(old) {
                    artifact.blob = Some(new.to_string());
                    changed = true;
                }
            }
            if changed {
                conn.execute(
                    "UPDATE sync_msg SET refs = ?2 WHERE message_id = ?1",
                    params![message_id, refs.to_column()],
                )?;
            }
        }
    }
    Ok(())
}

/// 存储上有一份「长度对、内容不对」的对象时的修复登记（`sync_state` 的 `blob-repair`）：
/// 取回时发现内容与对象名不符记下的。下一次轮到上传这个对象时不再被「同长度即已有」
/// 挡住，重传一遍把坏的那份盖掉。
const SCOPE_REPAIR: &str = "blob-repair";

fn repair_get(conn: &rusqlite::Connection, blob_id: &str) -> Result<bool> {
    if !rclaw_store::sync_db::sync_tables_exist(conn)? {
        return Ok(false);
    }
    Ok(db::state_get(conn, SCOPE_REPAIR, blob_id)?.is_some())
}

/// 标记一个对象「存储上的那份要重传覆盖」。
fn mark_repair(conn: &rusqlite::Connection, blob_id: &str) -> Result<()> {
    // 没开过同步的库（测试里的裸库）没有同步表：没有要修的。
    if !rclaw_store::sync_db::sync_tables_exist(conn)? {
        return Ok(());
    }
    if db::state_get(conn, SCOPE_REPAIR, blob_id)?.is_none() {
        db::state_put(conn, SCOPE_REPAIR, blob_id, db::HLC_ZERO, None, None)?;
    }
    Ok(())
}

fn repair_del(conn: &rusqlite::Connection, blob_id: &str) -> Result<()> {
    if !rclaw_store::sync_db::sync_tables_exist(conn)? {
        return Ok(());
    }
    db::state_del(conn, SCOPE_REPAIR, blob_id)
}

/// 这个 IO 错误说的是「没有这个文件」，不是「看不了」。
fn is_absent(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{NotADirectory, NotFound};
    matches!(error.kind(), NotFound | NotADirectory)
}

/// 上传一个本机登记着的附件对象。文件已经不在、或内容变了（对象名对不上）时返回
/// `Ok(false)`，登记改回「没上传」之外的状态由调用方决定。
pub async fn upload(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    blob: &BlobRow,
) -> Result<bool> {
    let key = codec::blob_path(&blob.blob_id);
    // 两种情况不能拿「远端长度对」当「远端内容对」：取回时发现长度对、内容不对的
    // （登记过要修的），以及别的设备点名重要过、对象被重新排队的（收到重复请求时
    // 校验存储副本）——都重新封装并核对摘要之后真的传一遍。
    let repair = db::read(ctx.db(), |conn| {
        // 还没开过同步的库（测试里的裸库）没有同步表：没有要修的，也没有上传原因。
        if !rclaw_store::sync_db::sync_tables_exist(conn)? {
            return Ok(false);
        }
        Ok(repair_get(conn, &blob.blob_id)?
            || db::state_get(conn, SCOPE_NAMED, &blob.blob_id)?.is_some()
            || db::state_get(conn, SCOPE_SKILL_BLOB, &blob.blob_id)?.is_some())
    })?;
    // 同名即同内容：远端已经有了完整的一份就不再传。「完整」要看长度——连接中断时有的
    // 服务端会留下截断的对象，只看「在不在」的话它就永远留在那里，谁也取不回来。长度
    // 对不上的照常重传一遍，把它盖掉。取回时发现「长度对、内容不对」的（登记过要修的）
    // 也重传一遍。
    if !repair {
        if let Some(meta) = transport::stat(transport, &key).await? {
            if meta.size == sealed_blob_len(blob.size) {
                return Ok(true);
            }
        }
    }
    let sealed = Scratch(tmp_file(ctx, "up"));
    let keys_for_task = keys.clone();
    let (env, rel_path) = (ctx.env.clone(), blob.rel_path.clone());
    let (blob_id, sealed_for_task, key_for_task, registered_size) = (
        blob.blob_id.clone(),
        sealed.0.clone(),
        key.clone(),
        blob.size,
    );
    // 提交阶段挂号：换空间时 `teardown` 先关入口、等在途的阶段走完，等不到才换代。
    let commit = ctx.space.begin();
    let token = commit.token();
    let prepared = tokio::task::spawn_blocking(move || -> Result<Option<u64>> {
        let source = env.checked_path(&rel_path)?;
        // 登记说本机有这个文件，其实不在了（被删、被换成了目录）：拿不出来，和以前一样
        // 报「没有」，由调用方说 blob.gone。
        match std::fs::metadata(&source) {
            Ok(meta) if !meta.is_file() => return Ok(None),
            Ok(_) => {}
            Err(e) if is_absent(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let mut reader = match std::fs::File::open(&source) {
            Ok(file) => HashingReader::new(file),
            Err(e) if is_absent(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // 摘要与封装读同一次：核对的就是真正被封装的那些字节，封装期间文件变了也不会
        // 以旧对象名传出错的内容。
        if !token.valid() {
            return Err(abandoned());
        }
        let mut writer = std::io::BufWriter::new(std::fs::File::create(&sealed_for_task)?);
        let len = keys_for_task.seal_blob(&key_for_task, &mut reader, &mut writer)?;
        let (digest, plain_len) = reader.finish();
        if keys_for_task.blob_id(&digest) != blob_id || plain_len != registered_size {
            return Ok(None);
        }
        Ok(Some(len))
    })
    .await
    .map_err(|e| SyncError::other(format!("附件加密任务失败: {e}")))?;
    let result = async {
        let Some(len) = prepared? else {
            return Ok(false);
        };
        if !commit.valid() {
            return Err(abandoned());
        }
        transport.put_file(&key, &sealed.0).await?;
        transport::check_len(transport, &key, len).await?;
        Ok(true)
    }
    .await;
    drop(sealed);
    // 修完了：把标记摘掉，以后同名同长度照常跳过。换了空间就不动了。
    if repair && matches!(&result, Ok(true)) && commit.valid() {
        db::write(ctx.db(), |tx| repair_del(tx, &blob.blob_id))?;
    }
    drop(commit);
    result
}

/// 取回之前先看本机的结果。
enum Lookup {
    /// 文件就在本机登记的地方（内容核对过）：不用取。
    Here(PathBuf),
    /// 要去存储上取。
    Fetch(BlobRow),
}

/// 取回一个附件之前先看本机：有没有它的登记，文件是不是已经在本机了。本机的那一份要
/// 内容核对得上——被人原地改过的不能拿去充当这个对象，改去存储上取原来的那份。旧版本
/// 按明文摘要登记的内联缓存在这里一并归一，不判坏。
fn lookup(ctx: &Ctx, keys: &Keys, blob_id: &str) -> Result<Lookup> {
    let (row, was_mine) = db::read(ctx.db(), |conn| {
        let row = db::blob_get(conn, blob_id)?;
        let was_mine = row.is_none() && db::state_get(conn, scope::BLOB_GONE, blob_id)?.is_some();
        Ok((row, was_mine))
    })?;
    let Some(row) = row else {
        // 本机登记过、上传的时候发现拿不出来了的（[`announce_gone`] 留了底）：照实说。
        return Err(SyncError::invalid(if was_mine {
            "这个文件原本在这台设备上，后来被删除或者改动过，同步空间里当时也没有它的副本，\
             所以取不回来。把它放回原处的话，其他设备再来要的时候会重新上传"
        } else {
            "没有这个附件的登记"
        }));
    };
    if row.state != BLOB_REMOTE {
        match check_local_row(ctx, keys, row.clone())? {
            LocalObject::Intact(row) => {
                return Ok(Lookup::Here(ctx.env.checked_path(&row.rel_path)?))
            }
            // 内容变了：不当它还在，去存储上取（取回后按占位规则落回）。文件没了也一样。
            LocalObject::Absent | LocalObject::Mismatch => {}
            // 登记在、也不在别的设备上，却判不了（路径不安全）：去存储上取。
            LocalObject::Unknown => {}
        }
    }
    Ok(Lookup::Fetch(row))
}

/// 取回一个附件对象到它登记的落点。成功后登记为「已取回」，并补上同一对象的其他落点。
/// 落点上已有内容不同的文件时不覆盖，改放进同步缓存（§8.7）。
///
/// `max_size` 是本机设置的附件大小上限（明文字节）：登记的大小超过它的不取（§11.3）。
/// 登记的大小是别的设备报来的，实际取回多少字节另由 [`download_max`] 卡住。
pub async fn download(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    blob_id: &str,
    max_size: u64,
) -> Result<PathBuf> {
    let found = {
        let blob_id = blob_id.to_string();
        let keys = keys.clone();
        offload(ctx, move |ctx| lookup(&ctx, &keys, &blob_id)).await?
    };
    let row = match found {
        // 本机就有也不用白拿一趟：同一个对象登记过的其他落点（占位）顺手补上。补别名会写
        // 文件，和落盘一样在提交边界之内（换空间期间作废）；文件复制放到阻塞线程池。
        Lookup::Here(path) => {
            let commit = ctx.space.begin();
            if !commit.valid() {
                return Err(abandoned());
            }
            // 最终写入段（本地命中的别名复制也算）：许可活着时换代不会发生；teardown
            // 等不到在途阶段退场就不换代，所以这里的 valid() 检查可信。
            let permit = ctx.space.enter();
            if !permit.ok() {
                return Err(abandoned());
            }
            let token = commit.token();
            let blob = blob_id.to_string();
            offload(ctx, move |ctx| {
                if !token.valid() {
                    return Err(abandoned());
                }
                let valid = || token.valid();
                wire::settle_aliases(&ctx.env, Some(&blob), Some(&valid))
            })
            .await?;
            drop(permit);
            return Ok(path);
        }
        Lookup::Fetch(row) => row,
    };
    if row.size > max_size {
        const MB: u64 = 1024 * 1024;
        return Err(SyncError::other(format!(
            "这个附件有 {} MB，超过了本机设置的附件大小上限（{} MB）。\
             要取回它，先在多端同步的设置里把上限调大",
            row.size.div_ceil(MB),
            max_size / MB
        )));
    }
    let key = codec::blob_path(blob_id);
    let sealed = Scratch(tmp_file(ctx, "down"));
    let plain = Scratch(tmp_file(ctx, "plain"));
    // 提交阶段挂号：换空间 / 清理时 teardown 先换代、再等它结束；落到文件和记账之前
    // 都要核对代，失配就丢掉这次结果（明文临时文件由 Scratch 收掉）。
    let commit = ctx.space.begin();
    // 暂存文件的守卫交给阻塞闭包自己拿：异步一侧取消（换空间超时收尾）不会提前删掉
    // 闭包正在读写的文件、也不会留下它重新写出来的明文；闭包跑完把守卫还回来，谁最后
    // 拿到谁收尾。
    let fetched: Result<(Scratch, Scratch, Commit)> = async move {
        transport
            .get_file(&key, &sealed.0, download_max(row.size))
            .await?;
        let keys = keys.clone();
        let key = key.clone();
        let blob_id = blob_id.to_string();
        let repair_ctx = ctx.clone();
        let token = commit.token();
        let (result, sealed, plain) = tokio::task::spawn_blocking(
            move || -> (Result<()>, Scratch, Scratch) {
                let outcome = (|| -> Result<()> {
                    let mut reader = std::io::BufReader::new(std::fs::File::open(&sealed.0)?);
                    let mut writer = std::io::BufWriter::new(std::fs::File::create(&plain.0)?);
                    keys.open_blob(&key, &mut reader, &mut writer)?;
                    drop(writer);
                    // 解密通过只说明它是这个空间里的对象；内容还得就是这个名字对应的那一份。
                    if hash_file(&keys, &plain.0).as_deref() != Some(blob_id.as_str()) {
                        // 存储上有一份长度对、内容不对的对象：登记要修，下一次上传把它盖掉。
                        // 这是记账：期间换了空间就不写了（那是旧空间的登记）。
                        let _ = db::write(repair_ctx.db(), |tx| {
                            // 写事务可能排在换空间之后：拿到锁之后在事务里再核对一次。
                            if token.valid() {
                                mark_repair(tx, &blob_id)
                            } else {
                                Ok(())
                            }
                        });
                        return Err(SyncError::corrupt("附件内容与对象名不符"));
                    }
                    Ok(())
                })();
                (outcome, sealed, plain)
            },
        )
        .await
        .map_err(|e| SyncError::other(format!("附件解密任务失败: {e}")))?;
        result?;
        Ok((sealed, plain, commit))
    }
    .await;
    let (sealed, plain, commit) = match fetched {
        Ok(guards) => guards,
        Err(error) => return Err(error),
    };
    if !commit.valid() {
        return Err(abandoned());
    }
    let ctx_for_task = ctx.clone();
    let blob_id = blob_id.to_string();
    tokio::task::spawn_blocking(move || {
        // 明文的守卫随闭包走（见上）；落盘把慢复制与最终提交分开，见 `place`。
        let placed = place(&ctx_for_task, &commit, &blob_id, &plain.0);
        drop(plain);
        drop(sealed);
        placed
    })
    .await
    .map_err(|e| SyncError::other(format!("附件落盘任务失败: {e}")))?
}

fn same_content(a: &Path, b: &Path) -> bool {
    let (Ok(meta_a), Ok(meta_b)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    if meta_a.len() != meta_b.len() {
        return false;
    }
    let digest = |path: &Path| {
        std::fs::File::open(path)
            .ok()
            .and_then(|mut file| sha256_reader(&mut file).ok())
            .map(|(digest, _)| digest)
    };
    digest(a).is_some() && digest(a) == digest(b)
}

/// 把取回的明文放到登记的落点，并更新登记。落点上已有的文件只有「同步写下的占位」才
/// 原位替换；内容不同的真实文件不覆盖，取回的全文改放进按摘要命名的同步缓存（§8.7），
/// 附件入口按对象找到它。
///
/// `commit` 是这次任务的提交阶段：换空间之后登记与落点都不是本机的了，写文件、写库之前
/// 各核对一次，失配就丢掉结果。
fn place(ctx: &Ctx, commit: &Commit, blob_id: &str, plain: &Path) -> Result<PathBuf> {
    let row = db::read(ctx.db(), |conn| db::blob_get(conn, blob_id))?
        .ok_or_else(|| SyncError::invalid("没有这个附件的登记"))?;
    if !commit.valid() {
        return Err(abandoned());
    }
    let mut rel_path = row.rel_path.clone();
    let original = ctx.env.checked_path(&rel_path);
    let is_artifact = rel_path.starts_with("artifacts/");
    let target = if original.as_ref().is_ok_and(|target| {
        !target.exists()
            || target.is_file()
                && (is_artifact && is_placeholder(target, row.size)
                    || same_content(target, plain))
    }) {
        original?
    } else {
        // 本机同名文件内容不同：不覆盖，放进同步缓存。
        rel_path = wire::cache_path(&rel_path, blob_id);
        ctx.env.checked_path(&rel_path)?
    };
    // 落点上原本就有一份内容相同的（或登记的地方就是同步缓存里那一份）时直接复用——
    // 那是用户或以前的同步留下的，不属于这次任务，中途作废也不动它；但登记仍要更新
    // （这次取回的结论就是「本机有它了」）。
    let needs_write = !(target.is_file() && same_content(&target, plain));
    let staging = if needs_write {
        if !commit.valid() {
            return Err(abandoned());
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // 慢复制与最终提交分开：先把明文复制进目标目录里的暂存文件（可能在慢盘上挂
        // 很久，不占提交许可），改名之前再取许可。哪一步失败都不把暂存文件留在用户的
        // 目录里。
        let staging = target.with_extension(format!("part-{}", ids::random_hex::<4>()));
        if let Err(e) = std::fs::copy(plain, &staging) {
            let _ = std::fs::remove_file(&staging);
            return Err(e.into());
        }
        Some(staging)
    } else {
        None
    };
    // 最终改名与记账都在最终写入段里：许可活着时换代不会发生（见
    // [`SpaceEpoch::enter`]）。teardown 关入口之后拿到的许可无效——旧世代只把暂存
    // 文件收掉，不再改名进落点、不再记账。
    let permit = ctx.space.enter();
    if !permit.ok() || !commit.valid() {
        if let Some(staging) = &staging {
            let _ = std::fs::remove_file(staging);
        }
        return Err(abandoned());
    }
    if let Some(staging) = &staging {
        if let Err(e) = std::fs::rename(staging, &target) {
            let _ = std::fs::remove_file(staging);
            drop(permit);
            return Err(e.into());
        }
    }
    // 记账：事务里再核对一次代，失配就让事务回滚——旧空间的登记不会留下来。
    let stored = db::write(ctx.db(), |tx| {
        if !permit.ok() || !commit.valid() {
            return Err(abandoned());
        }
        db::blob_set(tx, blob_id, &rel_path, BLOB_FETCHED)?;
        // 取回成功：这条登记不再是「重新下载」的临时登记，撤掉标记（以后清单/回收都
        // 按常规登记对待）。
        tx.execute(
            "DELETE FROM sync_state WHERE scope = ?1 AND key = ?2",
            params![scope::BLOB_PROBE, blob_id],
        )?;
        forget(tx, blob_id)
    });
    drop(permit);
    if let Err(error) = stored {
        // 只收回这次任务自己写下的落点：内容还是这次复制进去的那一份才删。路径
        // 可能已被新空间的对象复用，不能按路径硬删。
        if needs_write && same_content(&target, plain) {
            let _ = std::fs::remove_file(&target);
        }
        return Err(error);
    }
    // 同一个对象登记过的其他落点（别的会话的 artifact 目录等）一并补上。登记提交之后
    // 才换的代：落点与登记都在切换之前落定，是一致的；补别名会再写文件，与改名、记账
    // 一样要提交许可。
    let permit = ctx.space.enter();
    if permit.ok() && commit.valid() {
        let valid = || permit.ok() && commit.valid();
        wire::settle_aliases(&ctx.env, Some(blob_id), Some(&valid))?;
    }
    drop(permit);
    Ok(target)
}

/// 这个文件是不是同步写下的占位（内容一字不差）：只有它才能被取回的全文原位替换。
fn is_placeholder(path: &Path, size: u64) -> bool {
    std::fs::read(path).is_ok_and(|bytes| bytes == wire::placeholder_text(size).as_bytes())
}

/// 「同步已有附件」的选择（§8.8）：全部 / 最近若干天 / 都不传。记在 kv 里的写法是
/// `"all"`、`{"days": 30}`、`"none"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backlog {
    All,
    Days(u64),
    None,
}

/// 按选择把本机「没上传」的附件排进上传队列。返回 `(排进去的个数, 总字节数)`。
pub fn queue_backlog(ctx: &Ctx, choice: Backlog) -> Result<(u64, u64)> {
    let cutoff = match choice {
        Backlog::None => return Ok((0, 0)),
        Backlog::All => None,
        Backlog::Days(days) => {
            Some(SystemTime::now() - Duration::from_secs(days.saturating_mul(86_400)))
        }
    };
    let held = db::read(ctx.db(), |conn| {
        db::blobs_in_state(conn, BLOB_HELD, 100_000)
    })?;
    let mut picked: Vec<String> = Vec::new();
    let mut bytes = 0u64;
    for blob in held {
        let Ok(path) = ctx.env.checked_path(&blob.rel_path) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        let recent = match cutoff {
            None => true,
            Some(cutoff) => meta.modified().map(|at| at >= cutoff).unwrap_or(true),
        };
        if recent {
            bytes += blob.size;
            picked.push(blob.blob_id);
        }
    }
    db::write(ctx.db(), |tx| {
        for blob_id in &picked {
            tx.execute(
                "UPDATE sync_blob SET state = ?2 WHERE blob_id = ?1 AND state = ?3",
                params![blob_id, BLOB_PENDING, BLOB_HELD],
            )?;
        }
        Ok(())
    })?;
    Ok((picked.len() as u64, bytes))
}

/// 本机「没上传」的附件：`(个数, 总字节数)`。打开附件同步时用来提示。
pub fn backlog_size(ctx: &Ctx) -> Result<(u64, u64)> {
    db::read(ctx.db(), |conn| {
        Ok(conn.query_row(
            "SELECT count(*), COALESCE(sum(size), 0) FROM sync_blob WHERE state = ?1",
            params![BLOB_HELD],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )?)
    })
}

/// 一批上传的结果。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UploadBatch {
    /// 从队列里取出来、有了着落的个数：传成的、文件已经不在的、退回「没上传」的。
    pub taken: usize,
    /// 其中传成的。
    pub done: usize,
    /// 其中本机已经拿不出来、并且告诉了别的设备的（排了 `blob.gone`，等下一轮发出去）。
    pub said: usize,
}

/// 存储是通的、同一个附件却连着这么多次没传上去，就不再自动重试它：每试一次都要把整个
/// 文件再发一遍。
const UPLOAD_TRIES: u32 = 3;

/// 本机登记过、后来拿不出来了的附件留的底（`sync_state` 的 `blob-gone`）：原来登记的
/// 路径与大小。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct GoneNote {
    path: String,
    size: u64,
}

/// 一个排着队的附件本机拿不出来了（文件没了，或内容变了），登记刚被删掉：留个底，并
/// 告诉别的设备（`blob.gone`）——它们的消息里还引用着它，没有这句话就只能一直等
/// （§8.7）。在调用方的写事务里执行；返回话有没有排进待发队列。
fn announce_gone(conn: &rusqlite::Connection, blob: &BlobRow) -> Result<bool> {
    db::state_put_json(
        conn,
        scope::BLOB_GONE,
        &blob.blob_id,
        db::HLC_ZERO,
        &GoneNote {
            path: blob.rel_path.clone(),
            size: blob.size,
        },
    )?;
    let Some(device) = rclaw_store::sync_db::device_id(conn)? else {
        return Ok(false);
    };
    crate::map::enqueue(
        conn,
        &device,
        None,
        &Op::BlobGone {
            blob: blob.blob_id.clone(),
        },
    )?;
    Ok(true)
}

/// 队列里一个附件这一次的着落。
enum Settled {
    /// 传上去了。
    Uploaded,
    /// 文件没了或内容变了：这个对象本机已经拿不出来，留着登记也没用。
    Gone,
    /// 这个附件传不了：退回「没上传」。
    Held,
    /// 这一次没传成，而存储是通的：记一次，挪到队尾，以后再试。带着失败的原因。
    Later(String),
}

fn megabytes(size: u64) -> f64 {
    size as f64 / (1024.0 * 1024.0)
}

/// 一个附件没传上去之后：是存储够不着了，还是只有它传不上去？
///
/// 限流、被拒绝、配置不对是整个存储的事，返回 `None`——整批停下，过一阵再来。其余的再
/// 问存储一次这个对象：问不到也是 `None`；对象其实已经完整地在那里了（回应丢在了路上）
/// 就算传成；否则这次失败记在这个附件头上，不拦着排在它后面的。
async fn after_failure(
    transport: &dyn SyncTransport,
    blob: &BlobRow,
    error: &TransportError,
) -> Option<Settled> {
    if !matches!(
        error,
        TransportError::Timeout(_) | TransportError::Other(_) | TransportError::NotFound(_)
    ) {
        return None;
    }
    match transport::stat(transport, &codec::blob_path(&blob.blob_id)).await {
        Err(_) => None,
        Ok(Some(meta)) if meta.size == sealed_blob_len(blob.size) => Some(Settled::Uploaded),
        Ok(_) => Some(Settled::Later(error.to_string())),
    }
}

/// 不受「同步附件」开关约束的上传原因（`sync_state` 的 `blob-named`）。别的设备点名要过
/// （`blob.want`）——明确的请求，本机答应过要传（§8.8 的界面说明也是这么写的）；范围
/// 收窄只过滤掉「会话自动」那一种原因，点名的照传。
const SCOPE_NAMED: &str = "blob-named";

/// 技能发布登记的大文件（`sync_state` 的 `blob-skill`）：技能同步有自己的范围开关，
/// 与会话附件无关。技能范围开着时，发布技能登记的这些对象必须能传上去，不然接收端
/// 装不了同一个技能；范围关着时不消化过去排的队（它们跟着 `skill.put` 一起暂缓）。
const SCOPE_SKILL_BLOB: &str = "blob-skill";

fn mark_named(conn: &rusqlite::Connection, blob_id: &str) -> Result<()> {
    if db::state_get(conn, SCOPE_NAMED, blob_id)?.is_none() {
        db::state_put(conn, SCOPE_NAMED, blob_id, db::HLC_ZERO, None, None)?;
    }
    Ok(())
}

/// 技能发布登记的大文件：给队列里的对象补上「技能依赖」这个上传原因（旧队列里可能
/// 已经有它——先前自动排队过，或者上一版技能同步留下的）。
pub(crate) fn mark_skill_blob(conn: &rusqlite::Connection, blob_id: &str) -> Result<()> {
    if db::state_get(conn, SCOPE_SKILL_BLOB, blob_id)?.is_none() {
        db::state_put(conn, SCOPE_SKILL_BLOB, blob_id, db::HLC_ZERO, None, None)?;
    }
    Ok(())
}

/// 上传排着队的附件，最多 `limit` 个。存储够不着时返回错误——没传的留在队列里，调用方
/// 过一阵再来；只跟某一个附件有关的失败（存储不收这么大的、本机读不了、存储是通的但它
/// 就是传不上去）不拦着后面的。
///
/// 「同步附件」范围收窄之后：自动排队的（本机发布时登记的）不传，有别的上传原因的照传
/// ——别的设备点名要的、技能发布登记的大文件。范围重新打开时自动的那部分接着传。
pub async fn upload_pending(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    limit: usize,
) -> Result<UploadBatch> {
    let (attachments_on, skills_on, named, skill_marked) = offload(ctx, move |ctx| {
        db::read(ctx.db(), |conn| {
            let range = db::kv_json::<db::Range>(conn, db::KV_RANGE)?;
            let attachments = range.as_ref().is_none_or(|range| range.attachments);
            let skills = range.as_ref().is_none_or(|range| range.skills);
            let named: HashSet<String> = db::state_scan(conn, SCOPE_NAMED, "")?
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            // 技能依赖的原因要一直读出来：「同步附件」开着时也得靠它把技能范围关着的
            // 对象筛掉（技能定义还暂缓着，先传字节等于绕过技能范围）。
            let skill_marked: HashSet<String> = db::state_scan(conn, SCOPE_SKILL_BLOB, "")?
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            Ok((attachments, skills, named, skill_marked))
        })
    })
    .await?;
    let skill_deps: HashSet<String> = if skills_on {
        skill_marked.clone()
    } else {
        HashSet::new()
    };
    let pending = offload(ctx, move |ctx| {
        db::read(ctx.db(), |conn| {
            let mut rows = Vec::new();
            if attachments_on {
                // 技能依赖的对象在技能范围关着时不传（点名的除外）：它的 `skill.put`
                // 还暂缓着，先传字节等于绕过技能范围。过滤放在 SQL 里做——先取前 N 个
                // 再在内存里跳过，队首一堆暂停项会把后面的普通附件永远挡住。
                return db::blobs_for_upload(
                    conn,
                    !skills_on,
                    SCOPE_SKILL_BLOB,
                    SCOPE_NAMED,
                    limit,
                );
            }
            // 「同步附件」关着：只传有别的上传原因的——别的设备点名要的（维护，照传），
            // 以及技能范围开着时技能发布登记的大文件。
            for blob_id in named.iter().chain(skill_deps.iter()) {
                if rows.len() >= limit {
                    break;
                }
                if let Some(row) = db::blob_get(conn, blob_id)? {
                    if row.state == BLOB_PENDING {
                        rows.push(row);
                    }
                }
            }
            Ok(rows)
        })
    })
    .await?;
    let mut batch = UploadBatch::default();
    for blob in pending {
        // 一个附件从上传到记账是一个提交阶段：换空间期间作废——入口关着时拿到的阶段
        // 一开始就无效，传完发现代变了也不动登记（那是旧空间的账目）。
        let commit = ctx.space.begin();
        if !commit.valid() {
            break;
        }
        let mut too_large = false;
        let settled = match upload(ctx, transport, keys, &blob).await {
            Ok(true) => Settled::Uploaded,
            Ok(false) => Settled::Gone,
            Err(SyncError::Transport(TransportError::TooLarge(_))) => {
                tracing::warn!("附件 {} 太大，存储不接收", blob.rel_path);
                too_large = true;
                Settled::Held
            }
            Err(SyncError::Transport(error)) => {
                tracing::warn!("附件 {} 上传失败: {error}", blob.rel_path);
                match after_failure(transport, &blob, &error).await {
                    Some(settled) => settled,
                    None => return Err(error.into()),
                }
            }
            Err(error) => {
                tracing::warn!("附件 {} 没能上传: {error}", blob.rel_path);
                Settled::Held
            }
        };
        if !commit.valid() {
            break;
        }
        let done = matches!(settled, Settled::Uploaded);
        // 记账、给用户留话都在阻塞线程池上：这个任务和引擎共用一条线程。
        let (taken, said) = offload(ctx, move |ctx| {
            if !commit.valid() {
                return Err(abandoned());
            }
            if too_large {
                crate::status::notice(
                    &ctx.paths,
                    "blob-too-large",
                    "",
                    "有附件太大，存储不接收",
                    &format!(
                        "「{}」（{:.1} MB）超过了存储允许的单个文件大小，没有上传，其他设备上\
                         打不开它。可以调大存储一侧的上限；之后其他设备点开这个附件时会重新\
                         请本机上传。",
                        blob.rel_path,
                        megabytes(blob.size)
                    ),
                );
            }
            let (taken, gave_up, said) = db::write(ctx.db(), |tx| {
                // 写事务可能排在换空间之后：拿到锁之后在事务里再核对一次，旧空间的
                // 上传结果不写进新空间的账目。
                if !commit.valid() {
                    return Err(abandoned());
                }
                settle(tx, &blob, &settled)
            })?;
            if let (true, Settled::Later(reason)) = (gave_up, &settled) {
                crate::status::notice(
                    &ctx.paths,
                    "blob-upload-failed",
                    "",
                    "有附件一直传不上去",
                    &format!(
                        "「{}」（{:.1} MB）连着 {UPLOAD_TRIES} 次没能上传（最后一次：{reason}），\
                         先不再自动重试，其他设备上暂时打不开它。之后其他设备点开这个附件时会\
                         重新请本机上传；也可以在多端同步的设置里重新选一次「同步已有附件」的范围。",
                        blob.rel_path,
                        megabytes(blob.size)
                    ),
                );
            }
            Ok((taken, said))
        })
        .await?;
        if said {
            batch.said += 1;
        }
        if taken {
            batch.taken += 1;
        }
        if done {
            batch.done += 1;
        }
    }
    Ok(batch)
}

/// 队列里一个附件有了着落之后的记账（一个事务）。上传期间登记被别处改过（重置空间、
/// 退出空间）就不动它。返回：这个附件是不是有了着落（从队列里出去了），是不是因为试够
/// 了次数才退回去的，以及有没有排一条 `blob.gone`。
fn settle(
    tx: &rusqlite::Connection,
    blob: &BlobRow,
    settled: &Settled,
) -> Result<(bool, bool, bool)> {
    let set_state = |state: i64| {
        tx.execute(
            "UPDATE sync_blob SET state = ?2 WHERE blob_id = ?1 AND state = ?3",
            params![blob.blob_id, state, BLOB_PENDING],
        )
    };
    let outcome = match settled {
        Settled::Uploaded => {
            set_state(BLOB_UPLOADED)?;
            // 以前拿不出来过、这次又传成了（内容变回来了）：那个底不用留了。
            db::state_del(tx, scope::BLOB_GONE, &blob.blob_id)?;
            (true, false, false)
        }
        Settled::Gone => {
            let removed = tx.execute(
                "DELETE FROM sync_blob WHERE blob_id = ?1 AND state = ?2",
                params![blob.blob_id, BLOB_PENDING],
            )?;
            let said = removed > 0 && announce_gone(tx, blob)?;
            (true, false, said)
        }
        Settled::Held => {
            set_state(BLOB_HELD)?;
            (true, false, false)
        }
        // 登记已经不是「等待上传」：不归这里管了，也就不在队列里了。
        Settled::Later(_) if !db::blob_requeue(tx, &blob.blob_id)? => (true, false, false),
        Settled::Later(_) => {
            let tries = db::state_get(tx, scope::BLOB_FAIL, &blob.blob_id)?
                .and_then(|row| row.value?.parse::<u32>().ok())
                .unwrap_or(0)
                .saturating_add(1);
            if tries < UPLOAD_TRIES {
                db::state_put(
                    tx,
                    scope::BLOB_FAIL,
                    &blob.blob_id,
                    db::HLC_ZERO,
                    None,
                    Some(&tries.to_string()),
                )?;
                return Ok((false, false, false));
            }
            set_state(BLOB_HELD)?;
            (true, true, false)
        }
    };
    db::state_del(tx, scope::BLOB_FAIL, &blob.blob_id)?;
    // 「点名要」与「技能依赖」的登记跟着它出队列一起了结；之后再有设备来问（或技能
    // 重新登记）会重新记上。
    db::state_del(tx, SCOPE_NAMED, &blob.blob_id)?;
    db::state_del(tx, SCOPE_SKILL_BLOB, &blob.blob_id)?;
    Ok(outcome)
}

/// 别的设备点名要某个附件（`blob.want`）：本机有这个文件，就排进上传队列——不论本机
/// 有没有开「同步已有附件」。登记成「远端也有」的同样重新排队：对象可能已经被它的
/// 上传者回收（§9.5）；真的还在的话，上传前的那次核对会发现，不会再传一遍。
///
/// 本机登记过、后来拿不出来了的（[`announce_gone`] 留了底）：照着留的底把登记放回去
/// 再排一次队。上传前会重新核对文件——内容又变回来了就照常传，还是拿不出来就再说一遍
/// `blob.gone`。问的那台设备也许没看到上一次说的（它是后来加入的，那段日志已经回收），
/// 不能让它干等。一条 `blob.want` 至多换来一条 `blob.gone`，`blob.want` 自己是限流的。
pub fn on_blob_want(conn: &rusqlite::Connection, blob_id: &str) -> Result<()> {
    // 「重新下载」临时补的登记（带 `blob-probe` 标记）：刚点下去、下载可能还在跑，
    // 这时不能撤（`place` 落盘还要这条登记）；只有陈旧的（超过 10 分钟，不可能还有
    // 在途下载）才撤掉，让下面照常走 `BLOB_GONE` 的恢复路径。
    if let Some(marker) = db::state_get(conn, scope::BLOB_PROBE, blob_id)? {
        let started = marker
            .value
            .as_deref()
            .and_then(|value| value.rsplit_once('|'))
            .and_then(|(_, ts)| ts.parse::<i64>().ok())
            .unwrap_or(0);
        if now_ms().saturating_sub(started) > 10 * 60 * 1000 {
            if let Some(row) = db::blob_get(conn, blob_id)? {
                if row.state == BLOB_REMOTE {
                    db::blob_delete(conn, blob_id)?;
                }
            }
            conn.execute(
                "DELETE FROM sync_state WHERE scope = ?1 AND key = ?2",
                params![scope::BLOB_PROBE, blob_id],
            )?;
        } else {
            // 在途：照常点名，但别动登记。
            mark_named(conn, blob_id)?;
            return Ok(());
        }
    }
    let requeued = conn.execute(
        "UPDATE sync_blob SET state = ?2 WHERE blob_id = ?1 AND state IN (?3, ?4, ?5)",
        params![
            blob_id,
            BLOB_PENDING,
            BLOB_HELD,
            BLOB_UPLOADED,
            BLOB_FETCHED
        ],
    )?;
    if requeued > 0 {
        // 点名要的：不受「同步附件」范围影响，记下来。已经在队列里的（本来是自动排队
        // 的）也补上这个标记：点名之后它就是明确请求，范围收窄也照传。
        mark_named(conn, blob_id)?;
        return Ok(());
    }
    if db::blob_get(conn, blob_id)?.is_some() {
        // 已经在队列里（比如自动排队的）：点名同样是明确请求，补标记让它不受范围影响。
        mark_named(conn, blob_id)?;
        return Ok(());
    }
    let note =
        db::state_get(conn, scope::BLOB_GONE, blob_id)?.and_then(|row| row.json::<GoneNote>());
    if let Some(note) = note {
        db::blob_register(conn, blob_id, &note.path, note.size, BLOB_PENDING)?;
        mark_named(conn, blob_id)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 存储上没有的附件：向别的设备要，要不到就收场（§8.7）
// ---------------------------------------------------------------------------

const DAY_MS: i64 = 24 * 3600 * 1000;
/// 用户自己点了下载、存储上还是没有：隔这么久才再发一次 `blob.want`。自动去取的（小图、
/// 打开会话时的预取、隔一阵的重试）间隔逐次加倍，见 [`ask_interval`]。
const ASK_EVERY_MS: i64 = DAY_MS;
/// 要了这么久还没有设备传上来，就不再自动去取（和悬空 op 的保留期一样，§7.5）。
const WANT_GIVE_UP_MS: i64 = 30 * DAY_MS;
/// 有设备说它拿不出来之后再等这么久：别的设备可能也留着同样内容的一份。
const GONE_GRACE_MS: i64 = 3600 * 1000;
/// 收了场的附件，用户打开会话、接着聊的时候隔这么久才再去存储上看一次。
const PROBE_EVERY_MS: i64 = DAY_MS;

/// 自动去取的第 `asks` 次之后，隔多久再发下一条 `blob.want`：1、2、4、8、8…天。等满
/// [`WANT_GIVE_UP_MS`] 一共发 6 条。
fn ask_interval(asks: u32) -> i64 {
    DAY_MS << asks.saturating_sub(1).min(3)
}

/// 向别的设备要过、还在等的附件（`sync_state` 的 `blob-want`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Want {
    /// 第一次去要的时刻（Unix 毫秒）。
    pub first: i64,
    /// 最近一次发 `blob.want` 的时刻；0 = 下次取不到马上再发。
    pub asked: i64,
    /// 发过几条 `blob.want`。
    pub asks: u32,
    /// 有设备说它已经拿不出这份内容了（`blob.gone`）：本机听到的时刻。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gone: Option<i64>,
}

impl Want {
    /// 等够了：有设备说过拿不出来、宽限也过了；或者从第一次要算起已经太久。
    pub(crate) fn expired(&self, now: i64) -> bool {
        match self.gone {
            Some(at) => now - at >= GONE_GRACE_MS,
            None => now - self.first >= WANT_GIVE_UP_MS,
        }
    }

    /// 记下的时刻在将来（时钟被往回拨过）的，当作现在：不然「过了多久」永远是负数，
    /// 这个附件就永远等下去。返回有没有改动。
    fn clamp(&mut self, now: i64) -> bool {
        let before = *self;
        self.first = self.first.min(now);
        self.asked = self.asked.min(now);
        self.gone = self.gone.map(|at| at.min(now));
        *self != before
    }
}

/// 收了场的附件（`sync_state` 的 `blob-lost`）：后台不再去取，也不再催。用户用到它的
/// 时候照样去存储上看——说不定后来有设备传上来了：点它的时候每次都看，打开会话、接着聊
/// 的时候隔 [`PROBE_EVERY_MS`] 看一次（[`worth_fetching`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Lost {
    /// 什么时候收的场。
    pub at: i64,
    /// 最近一次发 `blob.want` 的时刻。0 = 本机没要过：只是听别的设备说它拿不出来了
    /// （[`on_blob_gone`]），或者要过之后又有新的引用、还没来得及再要。
    pub asked: i64,
    /// 有设备说过它已经拿不出这份内容；否则只是等了很久没有设备传上来。
    pub gone: bool,
    /// 收场之后最近一次去存储上看、还是没有的时刻（没看过是 0）。
    pub probed: i64,
}

fn want_get(conn: &rusqlite::Connection, blob_id: &str) -> Result<Option<Want>> {
    Ok(db::state_get(conn, scope::BLOB_WANT, blob_id)?.and_then(|row| row.json()))
}

fn want_put(conn: &rusqlite::Connection, blob_id: &str, want: &Want) -> Result<()> {
    db::state_put_json(conn, scope::BLOB_WANT, blob_id, db::HLC_ZERO, want)
}

pub(crate) fn lost_get(conn: &rusqlite::Connection, blob_id: &str) -> Result<Option<Lost>> {
    Ok(db::state_get(conn, scope::BLOB_LOST, blob_id)?.and_then(|row| row.json()))
}

fn lost_put(conn: &rusqlite::Connection, blob_id: &str, lost: &Lost) -> Result<()> {
    db::state_put_json(conn, scope::BLOB_LOST, blob_id, db::HLC_ZERO, lost)
}

/// 这个附件到手了（或者本机自己有了同样内容的一份）：不用再记着要过它。
pub(crate) fn forget(conn: &rusqlite::Connection, blob_id: &str) -> Result<()> {
    db::state_del(conn, scope::BLOB_WANT, blob_id)?;
    db::state_del(conn, scope::BLOB_LOST, blob_id)
}

/// 去取一个附件、存储上却没有这个对象时，本机接下来怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// 在等别的设备传上来。`asked`：这一次发了一条 `blob.want`（要马上跑一轮把它发
    /// 出去）。`gone`：已经有设备说它拿不出来了，只是再等一阵看别的设备。
    Waiting { asked: bool, gone: bool },
    /// 不再自动去取了。`gone`：有设备说过它已经拿不出这份内容；否则是等了很久没有设备
    /// 传上来。
    Lost { gone: bool },
}

/// 去取一个附件，存储上没有这个对象：向别的设备要（`blob.want`），要了很久没有、或者
/// 持有它的设备说拿不出来了，就收场——后台不再去取，也不再发 `blob.want`。在调用方的
/// 写事务里执行。
///
/// `user`：用户点名要的（或者续聊要用）。收了场的附件只有这时候才重新要一次，而且
/// 距上一条 `blob.want` 要满 [`ASK_EVERY_MS`]。例外是本机还没亲自要过的（只是听别的设备
/// 说它拿不出来了）：第一次用到它时不论是谁要的都要一次——说那句话的设备之外，也许还有
/// 别的设备留着它，而它们未必见过当初那条 `blob.want`。
pub fn on_missing(
    conn: &rusqlite::Connection,
    device: &str,
    blob_id: &str,
    user: bool,
    now: i64,
) -> Result<Missing> {
    let ask = || {
        crate::map::enqueue(
            conn,
            device,
            None,
            &Op::BlobWant {
                blob: blob_id.to_string(),
            },
        )
    };
    let Some(row) = db::blob_get(conn, blob_id)? else {
        // 登记已经不在了（去取的这一会儿，上传那边发现本机的文件没了、把登记删了；或者
        // 空间被重置了）：没有地方可放，也就没有可等的。
        let gone = db::state_get(conn, scope::BLOB_GONE, blob_id)?.is_some();
        return Ok(Missing::Lost { gone });
    };
    // 登记说本机有、文件却不在了（[`download`] 看过；用户点了「重新下载」才会走到这里）：
    // 从此按「还在别的设备上」算，以后的重试才轮得到它。
    let mut mine = false;
    if row.state != BLOB_REMOTE {
        db::blob_set_state(conn, blob_id, BLOB_REMOTE)?;
        forget(conn, blob_id)?;
        if matches!(row.state, BLOB_HELD | BLOB_PENDING) {
            // 本机登记着、还没传上去的：别的设备多半正等着本机传。告诉它们本机拿不出来
            // 了；本机自己也照「有设备说过拿不出来」那样等——只有别的设备凑巧留着同样
            // 内容的一份，才会有人传上来。
            crate::map::enqueue(
                conn,
                device,
                None,
                &Op::BlobGone {
                    blob: blob_id.to_string(),
                },
            )?;
            mine = true;
        }
    }
    if let Some(lost) = lost_get(conn, blob_id)? {
        let due = lost.asked > now || now - lost.asked >= ASK_EVERY_MS;
        if !(due && (user || lost.asked == 0)) {
            // 刚去存储上看过、还是没有：记下时刻，打开会话时的预取隔一阵才再来看。
            if lost.probed != now {
                lost_put(
                    conn,
                    blob_id,
                    &Lost {
                        probed: now,
                        ..lost
                    },
                )?;
            }
            return Ok(Missing::Lost { gone: lost.gone });
        }
        ask()?;
        db::state_del(conn, scope::BLOB_LOST, blob_id)?;
        // 说过拿不出来的那台设备多半还是拿不出来：这一次只等宽限那么久。
        let want = Want {
            first: now,
            asked: now,
            asks: 1,
            gone: lost.gone.then_some(now),
        };
        want_put(conn, blob_id, &want)?;
        return Ok(Missing::Waiting {
            asked: true,
            gone: lost.gone,
        });
    }
    let Some(mut want) = want_get(conn, blob_id)? else {
        ask()?;
        let want = Want {
            first: now,
            asked: now,
            asks: 1,
            gone: mine.then_some(now),
        };
        want_put(conn, blob_id, &want)?;
        return Ok(Missing::Waiting {
            asked: true,
            gone: mine,
        });
    };
    let mut changed = want.clamp(now);
    let gone = want.gone.is_some();
    if want.expired(now) {
        db::state_del(conn, scope::BLOB_WANT, blob_id)?;
        let lost = Lost {
            at: now,
            asked: want.asked,
            gone,
            probed: now,
        };
        lost_put(conn, blob_id, &lost)?;
        return Ok(Missing::Lost { gone });
    }
    // 有设备说过拿不出来之后不再催：要过的那一条各设备都看得到，留着同样内容的自己会传。
    let wait = if user {
        ASK_EVERY_MS
    } else {
        ask_interval(want.asks)
    };
    let asked = !gone && now - want.asked >= wait;
    if asked {
        ask()?;
        want.asked = now;
        want.asks = want.asks.saturating_add(1);
        changed = true;
    }
    if changed {
        want_put(conn, blob_id, &want)?;
    }
    Ok(Missing::Waiting { asked, gone })
}

/// 别的设备说它拿不出某个附件了（`blob.gone`）。本机也没有、正指望别的设备的才记：
/// 在等的记下这句话（再等 [`GONE_GRACE_MS`] 就收场）；还没要过的直接照收场算——后台
/// 不去取它（小图的自动下载、技能都跳过），界面上照实说。
///
/// 这句话只代表说话的那台设备，而且合入的先后不等于写下的先后：别的设备后来又引用了
/// 同一份内容的，那条引用可能比这句话先合入。所以照收场算的也不是定论——用户第一次用到
/// 它时本机照样去存储上看一次、要一次（[`on_missing`]），之后隔一阵再看（[`worth_fetching`]）。
pub fn on_blob_gone(conn: &rusqlite::Connection, blob_id: &str, now: i64) -> Result<()> {
    if !db::blob_get(conn, blob_id)?.is_some_and(|row| row.state == BLOB_REMOTE) {
        return Ok(());
    }
    if let Some(mut want) = want_get(conn, blob_id)? {
        if want.gone.is_none() {
            want.gone = Some(now);
            want_put(conn, blob_id, &want)?;
        }
        return Ok(());
    }
    match lost_get(conn, blob_id)? {
        Some(lost) if lost.gone => Ok(()),
        Some(lost) => lost_put(conn, blob_id, &Lost { gone: true, ..lost }),
        None => lost_put(
            conn,
            blob_id,
            &Lost {
                at: now,
                asked: 0,
                gone: true,
                probed: 0,
            },
        ),
    }
}

/// 新合入了一处对这个附件的引用（别的设备的一条消息，或者一个技能版本）：写下引用的
/// 那台设备当时有这份内容。以前记下的「取不回来」不算数了；在等、又听说过「拿不出来」
/// 的也不再那样算，下次取不到马上再问一遍（新的持有者没见过上一条 `blob.want`）。
///
/// 没人说过拿不出来、只是还在等的不动：不然持有方一直传不上去（存储不收这么大的）、
/// 又不断有新消息引用它的时候，每来一条消息就多催一次，对方就多白传一次。
pub fn on_new_ref(conn: &rusqlite::Connection, blob_id: &str) -> Result<()> {
    db::state_del(conn, scope::BLOB_LOST, blob_id)?;
    if let Some(mut want) = want_get(conn, blob_id)? {
        if want.gone.is_some() {
            want.gone = None;
            want.asked = 0;
            want_put(conn, blob_id, &want)?;
        }
    }
    Ok(())
}

/// 一个还不在本机的附件现在的情况，给界面看：`remote`（没取过）、`asked`（存储上还
/// 没有，向别的设备要了，在等）、`gone`（持有它的设备说拿不出来了）、`lost`（等了很久
/// 没有设备传上来）。后两种本机不再自动去取。
pub(crate) fn remote_state(
    conn: &rusqlite::Connection,
    blob_id: &str,
    now: i64,
) -> Result<&'static str> {
    let ended = |gone: bool| if gone { "gone" } else { "lost" };
    if let Some(lost) = lost_get(conn, blob_id)? {
        return Ok(ended(lost.gone));
    }
    Ok(match want_get(conn, blob_id)? {
        // 已经等够了，只是还没轮到下一次重试去收场：界面上先照收场了的说。
        Some(want) if want.expired(now) => ended(want.gone.is_some()),
        Some(_) => "asked",
        None => "remote",
    })
}

/// `blobs` 里第一个已经收了场的（[`Lost`]）。
pub(crate) fn lost_among(conn: &rusqlite::Connection, blobs: &[String]) -> Result<Option<Lost>> {
    for blob in blobs {
        if let Some(lost) = lost_get(conn, blob)? {
            return Ok(Some(lost));
        }
    }
    Ok(None)
}

/// 这个附件本机还没向别的设备要过，也没收过场。
pub(crate) fn untried(conn: &rusqlite::Connection, blob_id: &str) -> Result<bool> {
    Ok(want_get(conn, blob_id)?.is_none() && lost_get(conn, blob_id)?.is_none())
}

/// 用户打开会话、接着聊的时候，这个还不在本机的附件要不要去取。没收场的都要（在等的
/// 也要：轮次里的重试隔得越来越久，而这时候正是要用它的时候）。收了场的隔
/// [`PROBE_EVERY_MS`] 去存储上看一次：收场之后才有设备把它传上来的，这样才取得到，
/// 用户不用自己去点「重试」。
pub(crate) fn worth_fetching(conn: &rusqlite::Connection, blob_id: &str, now: i64) -> Result<bool> {
    Ok(match lost_get(conn, blob_id)? {
        Some(lost) => lost.probed > now || now - lost.probed >= PROBE_EVERY_MS,
        None => true,
    })
}

/// 向别的设备要过、还在等的附件，给隔一阵去取一次的重试用。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Waiting {
    /// `(对象名, 有没有设备说过它拿不出来)`。
    pub blobs: Vec<(String, bool)>,
    /// 记着要过、登记却已经不是「等着取回」的（到手了，本机自己有了同样的内容，或者
    /// 登记没了）：调用方另开写事务把它们忘掉（[`forget_stale`]）。
    pub stale: Vec<String>,
}

pub(crate) fn waiting(conn: &rusqlite::Connection) -> Result<Waiting> {
    let mut out = Waiting::default();
    for (blob, row) in db::state_scan(conn, scope::BLOB_WANT, "")? {
        if !db::blob_get(conn, &blob)?.is_some_and(|row| row.state == BLOB_REMOTE) {
            out.stale.push(blob);
            continue;
        }
        let gone = row.json::<Want>().is_some_and(|want| want.gone.is_some());
        out.blobs.push((blob, gone));
    }
    Ok(out)
}

/// 见 [`Waiting::stale`]。在写事务里再看一眼登记：列出来之后它可能又变回「等着取回」了。
pub(crate) fn forget_stale(conn: &rusqlite::Connection, blobs: &[String]) -> Result<()> {
    for blob in blobs {
        if !db::blob_get(conn, blob)?.is_some_and(|row| row.state == BLOB_REMOTE) {
            forget(conn, blob)?;
        }
    }
    Ok(())
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SpaceKey;
    use crate::testing::Device;
    use crate::transport::file::FileTransport;
    use crate::transport::{ObjectMeta, TransportResult};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn write(ctx: &Ctx, rel: &str, body: &[u8]) {
        let path = ctx.env.resolve(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn finds_marker_paths_and_inline_paths_with_trailing_text() {
        let (_g, ctx) = crate::testing::ctx();
        let keys = Keys::derive(&SpaceKey::generate());
        write(&ctx, "uploads/报告 最终版.docx", b"docx");
        write(&ctx, "media/a.png", b"png");
        write(&ctx, "output/chart(1).svg", b"svg");
        let data = serde_json::json!({
            "role": "user",
            "content": [{
                "type": "text",
                "text": "请看 uploads/报告 最终版.docx 这个文件，还有 output/chart(1).svg。\n\
                         [:image-media path=\"<<RCLAW_AGENT_ROOT>>/media/a.png\"]\n\
                         不存在的 media/none.png 不算，foomedia/a.png 也不算"
            }]
        })
        .to_string();
        let refs = extract_refs(&ctx.env, &keys, &data, 1 << 20);
        let mut paths: Vec<&str> = refs.iter().map(|(file, _)| file.path.as_str()).collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                "media/a.png",
                "output/chart(1).svg",
                "uploads/报告 最终版.docx"
            ]
        );
        for (file, blob) in &refs {
            assert!(ids::is_digest(&file.blob));
            assert_eq!(file.blob, blob.blob_id);
            assert_eq!(file.path, blob.rel_path);
        }
        // 超过上限的不登记。
        assert!(extract_refs(&ctx.env, &keys, &data, 2).is_empty());
    }

    /// 正文里用反斜杠写出来的本机绝对路径（Windows 上工具输出里常见）也认得出是附件。
    #[cfg(windows)]
    #[test]
    fn backslash_paths_under_the_agent_root_are_found() {
        let (_g, ctx) = crate::testing::ctx();
        let keys = Keys::derive(&SpaceKey::generate());
        write(&ctx, "output/sub/b.txt", b"txt");
        let native = ctx.env.resolve("output/sub/b.txt");
        assert!(native.to_string_lossy().contains('\\'));
        let data = serde_json::json!({
            "role": "user",
            "content": [{ "type": "text", "text": format!("已写入 {}", native.display()) }]
        })
        .to_string();
        let portable = portable::to_portable(&data, &ctx.env.root, "");
        let refs = extract_refs(&ctx.env, &keys, &portable, 1 << 20);
        assert_eq!(refs.len(), 1, "{portable}");
        assert_eq!(refs[0].0.path, "output/sub/b.txt");
    }

    #[tokio::test]
    async fn upload_then_download_lands_per_directory_rules() {
        let (_ga, a) = crate::testing::ctx();
        let (_gb, b) = crate::testing::ctx();
        let remote = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(remote.path());
        let keys = Arc::new(Keys::derive(&SpaceKey::generate()));
        write(&a, "uploads/a.txt", b"hello");
        write(&a, "output/report.md", b"# report");
        let data = serde_json::json!({
            "role": "user",
            "content": [{ "type": "text", "text": "uploads/a.txt 和 output/report.md" }]
        })
        .to_string();
        let refs = extract_refs(&a.env, &keys, &data, 1 << 20);
        assert_eq!(refs.len(), 2);
        db::write(a.db(), |tx| {
            db::ensure_device(tx)?;
            for (_, blob) in &refs {
                db::blob_register(tx, &blob.blob_id, &blob.rel_path, blob.size, BLOB_HELD)?;
            }
            Ok(())
        })
        .unwrap();
        // 没选「同步已有附件」：什么都不传。
        assert_eq!(
            upload_pending(&a, &transport, &keys, 10)
                .await
                .unwrap()
                .taken,
            0
        );
        assert_eq!(queue_backlog(&a, Backlog::All).unwrap().0, 2);
        let batch = upload_pending(&a, &transport, &keys, 10).await.unwrap();
        assert_eq!((batch.taken, batch.done), (2, 2));

        // 接收端：uploads/ 按原路径；output/ 只进同步缓存。本机同名文件内容不同则不覆盖。
        write(&b, "uploads/a.txt", b"local version");
        db::write(b.db(), |tx| {
            db::ensure_device(tx)?;
            for (file, _) in &refs {
                let target = wire::landing_path(&file.path, &file.blob);
                db::blob_register(tx, &file.blob, &target, file.size, BLOB_REMOTE)?;
            }
            Ok(())
        })
        .unwrap();
        for (file, _) in &refs {
            let path = download(&b, &transport, &keys, &file.blob, 1 << 20)
                .await
                .unwrap();
            assert!(path.starts_with(&b.env.sync_root), "{}", path.display());
        }
        assert_eq!(
            std::fs::read(b.env.resolve("uploads/a.txt")).unwrap(),
            b"local version"
        );
        let row = db::read(b.db(), |c| db::blob_get(c, &refs[0].0.blob))
            .unwrap()
            .unwrap();
        assert_eq!(row.state, BLOB_FETCHED);
        assert_eq!(
            std::fs::read(b.env.resolve(&row.rel_path)).unwrap(),
            b"hello"
        );
        assert!(!b.env.resolve("output/report.md").exists());
    }

    /// 存储上的附件对象短了（上次没传完）就重传盖掉；长出一截的不是它，取回时不收；登记
    /// 的大小超过本机上限的不取。
    #[tokio::test]
    async fn remote_objects_of_the_wrong_length_are_replaced_or_refused() {
        let (_ga, a) = crate::testing::ctx();
        let (_gb, b) = crate::testing::ctx();
        let remote = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(remote.path());
        let keys = Arc::new(Keys::derive(&SpaceKey::generate()));
        write(&a, "uploads/a.txt", b"hello");
        let (blob_id, size) = hash_file_len(&keys, &a.env.resolve("uploads/a.txt")).unwrap();
        let row = BlobRow {
            blob_id: blob_id.clone(),
            rel_path: "uploads/a.txt".into(),
            size,
            state: BLOB_PENDING,
        };
        let object = remote.path().join(codec::blob_path(&blob_id));

        // 远端留着半截对象：不算传过，重传之后是完整的。
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(&object, b"cut").unwrap();
        assert!(upload(&a, &transport, &keys, &row).await.unwrap());
        assert_eq!(
            std::fs::metadata(&object).unwrap().len(),
            sealed_blob_len(size)
        );
        // 已经完整：不再传——本机的文件拿走了也照样算传过。
        std::fs::remove_file(a.env.resolve("uploads/a.txt")).unwrap();
        assert!(upload(&a, &transport, &keys, &row).await.unwrap());

        db::write(b.db(), |tx| {
            db::ensure_device(tx)?;
            db::blob_register(tx, &blob_id, "uploads/a.txt", size, BLOB_REMOTE)?;
            Ok(())
        })
        .unwrap();
        let good = std::fs::read(&object).unwrap();
        // 对象比应有的长度长出不止一块：拒收，本机不留任何东西。
        let mut bloated = good.clone();
        bloated.resize(download_max(size) as usize + 1, 0);
        std::fs::write(&object, &bloated).unwrap();
        let error = download(&b, &transport, &keys, &blob_id, 1 << 20)
            .await
            .unwrap_err();
        assert!(matches!(error, SyncError::Corrupt(_)), "{error}");
        assert!(!b.env.resolve("uploads/a.txt").exists());
        assert_eq!(std::fs::read_dir(&b.paths.tmp).unwrap().count(), 0);

        // 登记的大小超过本机的附件大小上限：不取。上限够了就取得回来。
        std::fs::write(&object, &good).unwrap();
        assert!(download(&b, &transport, &keys, &blob_id, size - 1)
            .await
            .is_err());
        assert!(!b.env.resolve("uploads/a.txt").exists());
        let path = download(&b, &transport, &keys, &blob_id, size)
            .await
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"hello");
    }

    /// 接在本机目录上的存储，外加几种毛病：整个够不着；某个对象的上传每次都失败；或者
    /// 传上去了、回应却没回来。
    struct Faulty {
        inner: FileTransport,
        /// 这个对象的上传出毛病。
        key: String,
        offline: AtomicBool,
        /// 出毛病的那个对象其实传上去了，只是回应丢了。
        ack_lost: AtomicBool,
    }

    #[async_trait::async_trait]
    impl SyncTransport for Faulty {
        async fn list(&self, prefix: &str) -> TransportResult<Vec<ObjectMeta>> {
            if self.offline.load(Ordering::Relaxed) {
                return Err(TransportError::Other("连不上".into()));
            }
            self.inner.list(prefix).await
        }

        async fn get(&self, key: &str, max: u64) -> TransportResult<Vec<u8>> {
            self.inner.get(key, max).await
        }

        async fn put(&self, key: &str, body: Vec<u8>) -> TransportResult<()> {
            self.inner.put(key, body).await
        }

        async fn delete(&self, key: &str) -> TransportResult<()> {
            self.inner.delete(key).await
        }

        async fn put_file(&self, key: &str, source: &Path) -> TransportResult<()> {
            if key != self.key {
                return self.inner.put_file(key, source).await;
            }
            if self.ack_lost.load(Ordering::Relaxed) {
                self.inner.put_file(key, source).await?;
            }
            Err(TransportError::Timeout("没有回应".into()))
        }

        fn kind(&self) -> &'static str {
            "test"
        }
    }

    /// 存储是通的、某个附件却传不上去：它不拦着后面的，试够了次数就退回「没上传」并留一条
    /// 提示。存储够不着时什么都不记。回应丢了、对象其实已经在存储上的，算传成。
    #[tokio::test]
    async fn an_attachment_that_keeps_failing_does_not_block_the_queue() {
        let (_g, a) = crate::testing::ctx();
        let remote = tempfile::tempdir().unwrap();
        let keys = Arc::new(Keys::derive(&SpaceKey::generate()));
        write(&a, "uploads/stuck.bin", b"stuck");
        write(&a, "uploads/fine.txt", b"fine");
        let (stuck, stuck_len) = hash_file_len(&keys, &a.env.resolve("uploads/stuck.bin")).unwrap();
        let (fine, fine_len) = hash_file_len(&keys, &a.env.resolve("uploads/fine.txt")).unwrap();
        db::write(a.db(), |tx| {
            db::ensure_device(tx)?;
            db::blob_register(tx, &stuck, "uploads/stuck.bin", stuck_len, BLOB_PENDING)?;
            db::blob_register(tx, &fine, "uploads/fine.txt", fine_len, BLOB_PENDING)?;
            Ok(())
        })
        .unwrap();
        let transport = Faulty {
            inner: FileTransport::new(remote.path()),
            key: codec::blob_path(&stuck),
            offline: AtomicBool::new(false),
            ack_lost: AtomicBool::new(false),
        };
        let state = |blob: &str| {
            db::read(a.db(), |conn| {
                Ok(db::blob_get(conn, blob)?.map(|row| row.state))
            })
            .unwrap()
        };
        let queue = || -> Vec<String> {
            db::read(a.db(), |conn| db::blobs_in_state(conn, BLOB_PENDING, 10))
                .unwrap()
                .into_iter()
                .map(|row| row.blob_id)
                .collect()
        };
        let failures = || {
            db::read(a.db(), |conn| db::state_get(conn, scope::BLOB_FAIL, &stuck))
                .unwrap()
                .and_then(|row| row.value)
        };

        // 存储够不着：整批停下，队列原样，失败不记在哪个附件头上。
        transport.offline.store(true, Ordering::Relaxed);
        assert!(upload_pending(&a, &transport, &keys, 10).await.is_err());
        assert_eq!(queue(), [stuck.clone(), fine.clone()]);
        assert_eq!(failures(), None);
        transport.offline.store(false, Ordering::Relaxed);

        // 存储是通的：传不上去的那个记一次、挪到队尾。
        let batch = upload_pending(&a, &transport, &keys, 1).await.unwrap();
        assert_eq!((batch.taken, batch.done), (0, 0));
        assert_eq!(queue(), [fine.clone(), stuck.clone()]);
        assert_eq!(failures().as_deref(), Some("1"));
        // 后面的照常传。
        let batch = upload_pending(&a, &transport, &keys, 10).await.unwrap();
        assert_eq!((batch.taken, batch.done), (1, 1));
        assert_eq!(state(&fine), Some(BLOB_UPLOADED));
        assert_eq!(queue(), [stuck.clone()]);
        assert_eq!(failures().as_deref(), Some("2"));
        // 第三次还是不行：不再自动重试，留一条提示。
        let batch = upload_pending(&a, &transport, &keys, 10).await.unwrap();
        assert_eq!((batch.taken, batch.done), (1, 0));
        assert_eq!(state(&stuck), Some(BLOB_HELD));
        assert_eq!(failures(), None);
        assert!(crate::status::list(&a.paths)
            .iter()
            .any(|item| item.kind == "blob-upload-failed"));

        // 别的设备点名要它：重新排队。这次传上去了，只是回应没回来——算传成。
        db::write(a.db(), |tx| on_blob_want(tx, &stuck)).unwrap();
        transport.ack_lost.store(true, Ordering::Relaxed);
        let batch = upload_pending(&a, &transport, &keys, 10).await.unwrap();
        assert_eq!((batch.taken, batch.done), (1, 1));
        assert_eq!(state(&stuck), Some(BLOB_UPLOADED));
        assert_eq!(failures(), None);
    }

    /// 待发队列里某一类 op 有几条。
    fn queued(device: &Device, kind: &str) -> usize {
        crate::testing::outbox(device)
            .iter()
            .filter(|op| op["t"] == kind)
            .count()
    }

    fn register(device: &Device, blob: &str, rel_path: &str, size: u64, state: i64) {
        db::write(device.ctx.db(), |tx| {
            db::blob_register(tx, blob, rel_path, size, state)?;
            Ok(())
        })
        .unwrap();
    }

    /// 登记一个「还在别的设备上」的附件，返回它的对象名。
    fn remote_blob(device: &Device, tag: &str) -> String {
        let blob = tag.repeat(32);
        register(
            device,
            &blob,
            &format!("sync/files/{tag}/x.bin"),
            9,
            BLOB_REMOTE,
        );
        blob
    }

    fn blob_row(device: &Device, blob: &str) -> Option<BlobRow> {
        db::read(device.ctx.db(), |conn| db::blob_get(conn, blob)).unwrap()
    }

    fn missing(device: &Device, blob: &str, user: bool, at: i64) -> Missing {
        let me = device.settings.device.clone();
        db::write(device.ctx.db(), |tx| on_missing(tx, &me, blob, user, at)).unwrap()
    }

    fn shown(device: &Device, blob: &str, at: i64) -> &'static str {
        db::read(device.ctx.db(), |conn| remote_state(conn, blob, at)).unwrap()
    }

    const HOUR: i64 = 3600 * 1000;
    const T0: i64 = 1_700_000_000_000;

    /// 排着队的附件，本机的文件没了或者内容变了：登记删掉，告诉别的设备一声。别的设备
    /// 再来要，照着留的底重新核对——还是没有就再说一遍，文件放回来了就照常传。
    #[tokio::test]
    async fn a_file_that_is_gone_is_said_so_and_checked_again_when_asked() {
        let a = Device::new();
        let remote = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(remote.path());
        let keys = Arc::new(crate::testing::keys());
        write(&a.ctx, "uploads/a.txt", b"hello");
        let (blob, size) = hash_file_len(&keys, &a.ctx.env.resolve("uploads/a.txt")).unwrap();
        register(&a, &blob, "uploads/a.txt", size, BLOB_PENDING);
        let noted = || {
            db::read(a.ctx.db(), |conn| {
                db::state_get(conn, scope::BLOB_GONE, &blob)
            })
            .unwrap()
            .is_some()
        };

        // 内容变了：对象名对不上，同样算拿不出来。
        write(&a.ctx, "uploads/a.txt", b"changed");
        let batch = upload_pending(&a.ctx, &transport, &keys, 10).await.unwrap();
        assert_eq!((batch.taken, batch.done, batch.said), (1, 0, 1));
        assert!(blob_row(&a, &blob).is_none() && noted());
        assert_eq!(queued(&a, "blob.gone"), 1);
        // 没有人再来要：不重复说。
        let batch = upload_pending(&a.ctx, &transport, &keys, 10).await.unwrap();
        assert_eq!((batch.taken, batch.said), (0, 0));
        assert_eq!(queued(&a, "blob.gone"), 1);

        // 别的设备又来要（它也许没看到上一次说的）：重新核对，还是拿不出来，再说一遍。
        std::fs::remove_file(a.ctx.env.resolve("uploads/a.txt")).unwrap();
        db::write(a.ctx.db(), |tx| on_blob_want(tx, &blob)).unwrap();
        let row = blob_row(&a, &blob).unwrap();
        assert_eq!(
            (row.rel_path.as_str(), row.size, row.state),
            ("uploads/a.txt", size, BLOB_PENDING)
        );
        let batch = upload_pending(&a.ctx, &transport, &keys, 10).await.unwrap();
        assert_eq!((batch.taken, batch.done, batch.said), (1, 0, 1));
        assert!(blob_row(&a, &blob).is_none());
        assert_eq!(queued(&a, "blob.gone"), 2);

        // 本机的用户点它：照实说取不回来，不去存储上白找。
        let error = download(&a.ctx, &transport, &keys, &blob, 1 << 20)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("取不回来"), "{error}");

        // 文件放回来了：下次有人要就照常传，留的底不用了。
        write(&a.ctx, "uploads/a.txt", b"hello");
        db::write(a.ctx.db(), |tx| on_blob_want(tx, &blob)).unwrap();
        let batch = upload_pending(&a.ctx, &transport, &keys, 10).await.unwrap();
        assert_eq!((batch.taken, batch.done, batch.said), (1, 1, 0));
        assert_eq!(
            blob_row(&a, &blob).map(|row| row.state),
            Some(BLOB_UPLOADED)
        );
        assert!(!noted());
        assert_eq!(queued(&a, "blob.gone"), 2);

        // 没登记过、也没留过底的：别的设备要也不理。
        let unknown = "0".repeat(64);
        db::write(a.ctx.db(), |tx| on_blob_want(tx, &unknown)).unwrap();
        assert!(blob_row(&a, &unknown).is_none());
    }

    /// 文件在、只是这会儿读不了（被别的程序独占着）：不算拿不出来，不告诉别的设备，退回
    /// 「没上传」，下次有人要再试。
    #[cfg(windows)]
    #[tokio::test]
    async fn a_file_that_cannot_be_read_right_now_is_not_reported_as_gone() {
        use std::os::windows::fs::OpenOptionsExt;
        let a = Device::new();
        let remote = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(remote.path());
        let keys = Arc::new(crate::testing::keys());
        write(&a.ctx, "uploads/busy.bin", b"busy");
        let path = a.ctx.env.resolve("uploads/busy.bin");
        let (blob, size) = hash_file_len(&keys, &path).unwrap();
        register(&a, &blob, "uploads/busy.bin", size, BLOB_PENDING);

        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let batch = upload_pending(&a.ctx, &transport, &keys, 10).await.unwrap();
        assert_eq!((batch.taken, batch.done, batch.said), (1, 0, 0));
        assert_eq!(blob_row(&a, &blob).map(|row| row.state), Some(BLOB_HELD));
        assert_eq!(queued(&a, "blob.gone"), 0);
        drop(held);

        // 放开之后有人来要：照常传。
        db::write(a.ctx.db(), |tx| on_blob_want(tx, &blob)).unwrap();
        let batch = upload_pending(&a.ctx, &transport, &keys, 10).await.unwrap();
        assert_eq!((batch.taken, batch.done, batch.said), (1, 1, 0));
    }

    /// 存储上一直没有的附件：向别的设备要，隔得越来越久地再要几次，等满 30 天就收场——
    /// 不再自动去取，也不再要。用户自己点它，才隔一天再要一次。
    #[test]
    fn a_missing_attachment_is_asked_for_a_few_times_and_then_given_up() {
        let b = Device::new();
        let blob = remote_blob(&b, "a1");
        assert_eq!(shown(&b, &blob, T0), "remote");
        assert!(db::read(b.ctx.db(), |conn| untried(conn, &blob)).unwrap());

        assert_eq!(
            missing(&b, &blob, false, T0),
            Missing::Waiting {
                asked: true,
                gone: false
            }
        );
        assert_eq!(queued(&b, "blob.want"), 1);
        assert_eq!(shown(&b, &blob, T0), "asked");
        assert!(!db::read(b.ctx.db(), |conn| untried(conn, &blob)).unwrap());

        // 每小时自动去取一次，连着 30 天：一共只要 6 次，在第 0、1、3、7、15、23 天。
        let mut asked_on = vec![0];
        for hour in 1..30 * 24 {
            match missing(&b, &blob, false, T0 + hour * HOUR) {
                Missing::Waiting {
                    asked: true,
                    gone: false,
                } => asked_on.push(hour / 24),
                Missing::Waiting {
                    asked: false,
                    gone: false,
                } => {}
                other => panic!("第 {hour} 小时：{other:?}"),
            }
        }
        assert_eq!(asked_on, [0, 1, 3, 7, 15, 23]);
        assert_eq!(queued(&b, "blob.want"), 6);

        // 等满了：收场。界面上是「等了很久没有设备传上来」，自动去取的不再要。
        let end = T0 + WANT_GIVE_UP_MS;
        assert_eq!(shown(&b, &blob, end), "lost");
        assert_eq!(
            missing(&b, &blob, false, end),
            Missing::Lost { gone: false }
        );
        assert_eq!(
            missing(&b, &blob, false, end + DAY_MS),
            Missing::Lost { gone: false }
        );
        assert_eq!(queued(&b, "blob.want"), 6);
        assert!(db::read(b.ctx.db(), |conn| want_get(conn, &blob))
            .unwrap()
            .is_none());
        let lost = db::read(b.ctx.db(), |conn| {
            lost_among(conn, std::slice::from_ref(&blob))
        })
        .unwrap()
        .unwrap();
        assert_eq!((lost.at, lost.gone), (end, false));

        // 用户点「重试」：距上一次要满一天了，再要一次，重新开始等。
        assert_eq!(
            missing(&b, &blob, true, end + DAY_MS),
            Missing::Waiting {
                asked: true,
                gone: false
            }
        );
        assert_eq!(queued(&b, "blob.want"), 7);
        assert_eq!(shown(&b, &blob, end + DAY_MS), "asked");
        // 一天之内再点不再要；满一天才要。
        assert_eq!(
            missing(&b, &blob, true, end + DAY_MS + HOUR),
            Missing::Waiting {
                asked: false,
                gone: false
            }
        );
        assert_eq!(
            missing(&b, &blob, true, end + 2 * DAY_MS),
            Missing::Waiting {
                asked: true,
                gone: false
            }
        );
        assert_eq!(queued(&b, "blob.want"), 8);
    }

    /// 持有它的设备说拿不出来了：在等的再等一小时（别的设备可能留着同样的一份）就收场；
    /// 还没要过的先照收场算，第一次用到时才去要一遍。本机自己有的不受影响。
    #[test]
    fn a_holder_saying_it_is_gone_ends_the_wait_after_a_grace() {
        let b = Device::new();
        let waiting = remote_blob(&b, "a1");
        let fresh = remote_blob(&b, "b2");
        let own = "c3".repeat(32);
        register(&b, &own, "uploads/own.bin", 3, BLOB_HELD);
        let hear = |blob: &str, at: i64| {
            db::write(b.ctx.db(), |tx| on_blob_gone(tx, blob, at)).unwrap();
        };

        // 在等的。
        assert_eq!(
            missing(&b, &waiting, false, T0),
            Missing::Waiting {
                asked: true,
                gone: false
            }
        );
        let heard = T0 + 5 * 60 * 1000;
        hear(&waiting, heard);
        assert_eq!(shown(&b, &waiting, heard), "asked");
        // 宽限里照常去取，但不再催；听到第二遍不重新计时。
        hear(&waiting, heard + HOUR / 2);
        assert_eq!(
            missing(&b, &waiting, true, heard + GONE_GRACE_MS - 1),
            Missing::Waiting {
                asked: false,
                gone: true
            }
        );
        // 宽限过了：界面上先照收场了的说，下一次取不到就收场。
        assert_eq!(shown(&b, &waiting, heard + GONE_GRACE_MS), "gone");
        assert_eq!(
            missing(&b, &waiting, false, heard + GONE_GRACE_MS),
            Missing::Lost { gone: true }
        );
        assert_eq!(shown(&b, &waiting, heard + GONE_GRACE_MS), "gone");
        assert_eq!(queued(&b, "blob.want"), 1);

        // 还没要过的：先照收场算，后台不去取它（小图的自动下载、技能只取 `untried` 的）。
        let due = |blob: &str, at: i64| {
            db::read(b.ctx.db(), |conn| worth_fetching(conn, blob, at)).unwrap()
        };
        hear(&fresh, T0);
        assert_eq!(shown(&b, &fresh, T0), "gone");
        assert!(!db::read(b.ctx.db(), |conn| untried(conn, &fresh)).unwrap());
        // 这句话只代表说话的那台设备：第一次用到它（哪怕只是打开会话）照样去存储上看
        // 一次、要一次，只等宽限那么久。
        assert!(due(&fresh, T0));
        assert_eq!(
            missing(&b, &fresh, false, T0),
            Missing::Waiting {
                asked: true,
                gone: true
            }
        );
        assert_eq!(queued(&b, "blob.want"), 2);
        assert_eq!(
            missing(&b, &fresh, false, T0 + GONE_GRACE_MS),
            Missing::Lost { gone: true }
        );
        // 收场之后：用户点它，一天之内不再要；满一天才再要一次。
        assert_eq!(
            missing(&b, &fresh, true, T0 + 2 * HOUR),
            Missing::Lost { gone: true }
        );
        assert_eq!(queued(&b, "blob.want"), 2);
        assert_eq!(
            missing(&b, &fresh, true, T0 + DAY_MS),
            Missing::Waiting {
                asked: true,
                gone: true
            }
        );
        assert_eq!(queued(&b, "blob.want"), 3);
        let ended = T0 + DAY_MS + GONE_GRACE_MS;
        assert_eq!(
            missing(&b, &fresh, false, ended),
            Missing::Lost { gone: true }
        );
        // 打开会话时的预取：刚看过的不看，隔一天去存储上看一次，不再要。
        assert!(!due(&fresh, ended + HOUR));
        assert!(due(&fresh, ended + DAY_MS));
        assert_eq!(
            missing(&b, &fresh, false, ended + DAY_MS),
            Missing::Lost { gone: true }
        );
        assert!(!due(&fresh, ended + DAY_MS + HOUR));
        assert_eq!(queued(&b, "blob.want"), 3);
        // 时钟被往回拨过（上次看的时刻在将来）：不拦着再看。
        assert!(due(&fresh, T0));

        // 本机自己有的：别的设备拿不出来与本机无关。
        hear(&own, T0);
        assert!(db::read(b.ctx.db(), |conn| untried(conn, &own)).unwrap());
        assert!(due(&own, T0));
    }

    /// 新合入的消息（或技能版本）又引用了它：写下引用的设备当时有这份内容，以前记下的
    /// 「取不回来」不算数了。只是还在等、没人说过拿不出来的不动。
    #[test]
    fn a_new_reference_reopens_an_attachment_that_was_given_up() {
        let b = Device::new();
        let renew = |blob: &str| db::write(b.ctx.db(), |tx| on_new_ref(tx, blob)).unwrap();
        let hear = |blob: &str, at: i64| {
            db::write(b.ctx.db(), |tx| on_blob_gone(tx, blob, at)).unwrap();
        };

        // 收了场的：从头再来。
        let lost = remote_blob(&b, "a1");
        hear(&lost, T0);
        assert_eq!(shown(&b, &lost, T0), "gone");
        renew(&lost);
        assert_eq!(shown(&b, &lost, T0), "remote");
        assert!(db::read(b.ctx.db(), |conn| untried(conn, &lost)).unwrap());

        // 在等、又听说过拿不出来的：不再那样算，下次取不到马上再问一遍。
        let told = remote_blob(&b, "b2");
        assert_eq!(
            missing(&b, &told, false, T0),
            Missing::Waiting {
                asked: true,
                gone: false
            }
        );
        hear(&told, T0 + HOUR / 2);
        renew(&told);
        assert_eq!(shown(&b, &told, T0 + 3 * HOUR), "asked");
        assert_eq!(
            missing(&b, &told, false, T0 + 3 * HOUR),
            Missing::Waiting {
                asked: true,
                gone: false
            }
        );

        // 只是在等的：不多催。
        let plain = remote_blob(&b, "c3");
        assert_eq!(
            missing(&b, &plain, false, T0),
            Missing::Waiting {
                asked: true,
                gone: false
            }
        );
        renew(&plain);
        assert_eq!(
            missing(&b, &plain, false, T0 + HOUR),
            Missing::Waiting {
                asked: false,
                gone: false
            }
        );
        assert_eq!(queued(&b, "blob.want"), 3);
    }

    /// 时钟被往回拨过：记下的时刻在将来的当作现在，等待照样有尽头。
    #[test]
    fn a_clock_set_back_does_not_make_the_wait_endless() {
        let b = Device::new();
        let blob = remote_blob(&b, "a1");
        let ahead = T0 + 400 * DAY_MS;
        assert_eq!(
            missing(&b, &blob, false, ahead),
            Missing::Waiting {
                asked: true,
                gone: false
            }
        );
        // 现在的时刻比记下的早了 400 天。
        assert_eq!(
            missing(&b, &blob, false, T0),
            Missing::Waiting {
                asked: false,
                gone: false
            }
        );
        let want = db::read(b.ctx.db(), |conn| want_get(conn, &blob))
            .unwrap()
            .unwrap();
        assert_eq!((want.first, want.asked), (T0, T0));
        assert_eq!(
            missing(&b, &blob, false, T0 + WANT_GIVE_UP_MS),
            Missing::Lost { gone: false }
        );
        // 收场之后也一样：上一次要的时刻在将来，不拦着用户再要。
        db::write(b.ctx.db(), |tx| {
            let lost = Lost {
                at: ahead,
                asked: ahead,
                gone: false,
                probed: ahead,
            };
            lost_put(tx, &blob, &lost)
        })
        .unwrap();
        assert_eq!(
            missing(&b, &blob, true, T0),
            Missing::Waiting {
                asked: true,
                gone: false
            }
        );
    }

    /// 登记说本机有、文件却不在了，存储上也没有（用户点了「重新下载」）：从此按「还在
    /// 别的设备上」算。本机还没传上去的，顺带告诉别的设备不用等本机了。
    #[test]
    fn a_local_registration_whose_file_is_gone_turns_into_a_wait() {
        let a = Device::new();
        // 还没传上去的。
        let held = "a1".repeat(32);
        register(&a, &held, "uploads/held.bin", 3, BLOB_HELD);
        assert_eq!(
            missing(&a, &held, true, T0),
            Missing::Waiting {
                asked: true,
                gone: true
            }
        );
        assert_eq!(blob_row(&a, &held).map(|row| row.state), Some(BLOB_REMOTE));
        assert_eq!((queued(&a, "blob.gone"), queued(&a, "blob.want")), (1, 1));
        assert_eq!(
            missing(&a, &held, false, T0 + GONE_GRACE_MS),
            Missing::Lost { gone: true }
        );

        // 传上去过的（对象后来不在存储上了）：别的设备取回过的话还留着，只管要。
        let uploaded = "b2".repeat(32);
        register(&a, &uploaded, "uploads/up.bin", 3, BLOB_UPLOADED);
        assert_eq!(
            missing(&a, &uploaded, true, T0),
            Missing::Waiting {
                asked: true,
                gone: false
            }
        );
        assert_eq!(
            blob_row(&a, &uploaded).map(|row| row.state),
            Some(BLOB_REMOTE)
        );
        assert_eq!((queued(&a, "blob.gone"), queued(&a, "blob.want")), (1, 2));

        // 登记已经不在了：没有可等的，什么也不发。
        let unknown = "c3".repeat(32);
        assert_eq!(
            missing(&a, &unknown, true, T0),
            Missing::Lost { gone: false }
        );
        assert!(db::read(a.ctx.db(), |conn| untried(conn, &unknown)).unwrap());
        assert_eq!((queued(&a, "blob.gone"), queued(&a, "blob.want")), (1, 2));
    }

    /// 轮次里的重试照着这份清单来：在等的列出来（带上有没有设备说过拿不出来），登记
    /// 已经不是「等着取回」的顺手清掉。
    #[test]
    fn the_retry_list_holds_live_waits_and_drops_stale_ones() {
        let b = Device::new();
        let plain = remote_blob(&b, "a1");
        let told = remote_blob(&b, "b2");
        let arrived = remote_blob(&b, "c3");
        for blob in [&plain, &told, &arrived] {
            assert_eq!(
                missing(&b, blob, false, T0),
                Missing::Waiting {
                    asked: true,
                    gone: false
                }
            );
        }
        db::write(b.ctx.db(), |tx| {
            on_blob_gone(tx, &told, T0)?;
            // 本机自己有了同样内容的一份（比如从技能目录登记的）。
            db::blob_set_state(tx, &arrived, BLOB_UPLOADED)
        })
        .unwrap();
        let listed = db::read(b.ctx.db(), waiting).unwrap();
        assert_eq!(listed.blobs, [(plain.clone(), false), (told.clone(), true)]);
        assert_eq!(listed.stale, [arrived.clone()]);
        db::write(b.ctx.db(), |tx| forget_stale(tx, &listed.stale)).unwrap();
        assert_eq!(
            db::read(b.ctx.db(), waiting).unwrap().stale,
            Vec::<String>::new()
        );
        assert!(db::read(b.ctx.db(), |conn| untried(conn, &arrived)).unwrap());
    }

    /// 两台设备走一遍：乙要一个甲已经拿不出来的附件，甲答一声，乙记下、等一阵就收场。
    #[tokio::test]
    async fn a_want_for_a_file_that_is_gone_is_answered_across_devices() {
        let (a, b) = (Device::new(), Device::new());
        let remote = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(remote.path());
        let keys = Arc::new(crate::testing::keys());
        write(&a.ctx, "uploads/a.txt", b"hello");
        let (blob, size) = hash_file_len(&keys, &a.ctx.env.resolve("uploads/a.txt")).unwrap();
        // 甲登记着、没上传（没开附件同步）；乙从消息里知道有这个附件。
        register(&a, &blob, "uploads/a.txt", size, BLOB_HELD);
        register(&b, &blob, "uploads/a.txt", size, BLOB_REMOTE);
        std::fs::remove_file(a.ctx.env.resolve("uploads/a.txt")).unwrap();

        // 乙去取，存储上没有：向别的设备要。
        let error = download(&b.ctx, &transport, &keys, &blob, 1 << 20)
            .await
            .unwrap_err();
        assert!(error.is_not_found(), "{error}");
        let asked_at = now_ms();
        assert_eq!(
            missing(&b, &blob, true, asked_at),
            Missing::Waiting {
                asked: true,
                gone: false
            }
        );
        crate::testing::ship(&b, &a);
        a.merge();
        assert_eq!(blob_row(&a, &blob).map(|row| row.state), Some(BLOB_PENDING));

        // 甲排队去传，发现文件没了：答一声。
        let batch = upload_pending(&a.ctx, &transport, &keys, 10).await.unwrap();
        assert_eq!((batch.taken, batch.done, batch.said), (1, 0, 1));
        crate::testing::ship(&a, &b);
        b.merge();

        // 乙记下了：宽限里照「要过」算，过了就是「取不回来」，不再去取。
        let heard = db::read(b.ctx.db(), |conn| want_get(conn, &blob))
            .unwrap()
            .and_then(|want| want.gone)
            .expect("乙记下了甲的话");
        assert!(heard >= asked_at);
        assert_eq!(shown(&b, &blob, heard + GONE_GRACE_MS - 1), "asked");
        assert_eq!(shown(&b, &blob, heard + GONE_GRACE_MS), "gone");
        assert_eq!(
            missing(&b, &blob, false, heard + GONE_GRACE_MS),
            Missing::Lost { gone: true }
        );
        assert_eq!(queued(&b, "blob.want"), 0, "要过的那一条已经封段发走了");
    }

    /// 在 Windows 上不是普通文件名的路径不当作引用（那里不可能有这样的本机文件；照着拼
    /// 会读到别的文件）；别的系统上它们是普通文件，照常登记，接收端放进同步缓存。
    #[test]
    fn refs_only_point_at_files_that_exist_under_that_very_name() {
        let (_g, ctx) = crate::testing::ctx();
        let keys = Keys::derive(&SpaceKey::generate());
        write(&ctx, "uploads/12_30.png", b"png");
        let data = serde_json::json!({
            "role": "user",
            "content": [{
                "type": "text",
                "text": "[:image-media path=\"<<RCLAW_AGENT_ROOT>>/uploads/12:30.png\"]"
            }]
        })
        .to_string();
        let refs = extract_refs(&ctx.env, &keys, &data, 1 << 20);
        #[cfg(windows)]
        assert!(refs.is_empty(), "{refs:?}");
        #[cfg(not(windows))]
        {
            assert!(refs.is_empty(), "{refs:?}");
            write(&ctx, "uploads/12:30.png", b"png");
            let refs = extract_refs(&ctx.env, &keys, &data, 1 << 20);
            assert_eq!(refs.len(), 1);
            assert_eq!(refs[0].0.path, "uploads/12:30.png");
            assert!(wire::landing_path(&refs[0].0.path, &refs[0].0.blob).starts_with("sync/files/"));
        }
    }
}
