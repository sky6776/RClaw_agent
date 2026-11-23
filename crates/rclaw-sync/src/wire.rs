//! 本机消息行 ↔ 线上消息（[`WireMsg`]）。发布（封段）、合入、会话包的导出与导入共用。
//!
//! 线上形态不含任何本机的东西：路径换成标记，`meta` 里的本机 id 换成全局 id，分叉告知行
//! 的正文换成规范文本（§7.2）。落库时再换成接收端自己的值。

use std::path::{Path, PathBuf};

use rclaw_core::{ContentBlock, Message};
use rclaw_store::session::artifact_chat_key;
use rclaw_store::session::raw::{self, RawMessageRow};
use rclaw_store::{sync_db, Db, SessionStore};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::crypto::{sha256_hex, sha256_reader, Keys};
use crate::db::{self, BLOB_FETCHED, BLOB_REMOTE};
use crate::error::{Result, SyncError};
use crate::ids;
use crate::map::{self, MsgMap, SessionMap};
use crate::op::{Artifact, FileRef, WireMsg};
use crate::portable::{self, LocalRoot};

/// 本机一侧的环境：会话库、agent 根与同步目录。
#[derive(Debug, Clone)]
pub struct Env {
    pub sessions: SessionStore,
    pub root: LocalRoot,
    /// `<数据根>/sync`。
    pub sync_root: PathBuf,
}

/// 附件相对路径里指向同步目录的前缀（其余都相对 agent 根）。
pub const SYNC_DIR_PREFIX: &str = "sync/";

impl Env {
    /// `data_root` 是 `rclaw.toml` 所在目录（同步目录是它下面的 `sync/`）。
    pub fn new(sessions: &SessionStore, data_root: &Path) -> Env {
        Env {
            root: LocalRoot::new(&sessions.layout().base),
            sessions: sessions.clone(),
            sync_root: data_root.join("sync"),
        }
    }

    pub fn db(&self) -> &Db {
        self.sessions.db()
    }

    /// agent 根（附件的相对路径都相对它）。
    pub fn base(&self) -> &Path {
        &self.sessions.layout().base
    }

    /// 附件的相对路径（正斜杠）→ 本机路径。`sync/…` 指同步目录（它在数据根下，与 agent
    /// 根并列），其余相对 agent 根。
    ///
    /// 结果一定落在那个根的下面。相对路径可能出自别的设备，各处的检查是第一道关，这里是
    /// 最后一道：每一段只当作一个普通的名字拼上去，本机上拼不得的段（见 [`local_part`]）
    /// 换成改写后的名字，而不是照拼。
    pub fn resolve(&self, rel_path: &str) -> PathBuf {
        self.resolve_parts(rel_path, |part| match local_part(part) {
            Some(part) => Some(part.to_string()),
            None => Some(rclaw_store::portable_file_name(part)),
        })
        .expect("每一段都给出了名字")
    }

    /// 同 [`Env::resolve`]，但只在每一段都能原样拼上去时给出路径。读本机文件用：改写过
    /// 的名字指向的是另一个文件。
    pub fn resolve_exact(&self, rel_path: &str) -> Option<PathBuf> {
        self.resolve_parts(rel_path, |part| local_part(part).map(str::to_string))
    }

    /// 访问附件前检查磁盘上的路径，避免工作区里的链接把读写重定向到别处。
    pub(crate) fn checked_path(&self, rel_path: &str) -> Result<PathBuf> {
        let path = self.resolve(rel_path);
        self.check_path(rel_path, &path)?;
        Ok(path)
    }

    pub(crate) fn check_path(&self, rel_path: &str, path: &Path) -> Result<()> {
        let root = if rel_path.starts_with(SYNC_DIR_PREFIX) {
            self.sync_root.parent().unwrap_or(&self.sync_root)
        } else {
            self.base()
        };
        check_no_links(root, path)
            .map_err(|error| SyncError::other(format!("无法安全访问附件 {rel_path}：{error}")))
    }

    fn resolve_parts(
        &self,
        rel_path: &str,
        name_of: impl Fn(&str) -> Option<String>,
    ) -> Option<PathBuf> {
        let (mut path, rest) = match rel_path.strip_prefix(SYNC_DIR_PREFIX) {
            Some(rest) => (self.sync_root.clone(), rest),
            None => (self.base().to_path_buf(), rel_path),
        };
        for part in rest.split('/').filter(|part| !part.is_empty()) {
            path.push(name_of(part)?);
        }
        Some(path)
    }
}

/// 根由本机设置；根以下每一级必须是普通目录或文件。Windows reparse point 也按链接处理。
pub(crate) fn check_no_links(root: &Path, path: &Path) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ErrorKind::InvalidInput)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                return Ok(())
            }
            Err(error) => return Err(error),
        };
        if is_link(&meta) || !(meta.is_dir() || meta.is_file()) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "路径经过链接或特殊文件",
            ));
        }
    }
    Ok(())
}

pub(crate) fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x0400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.is_symlink()
    }
}

/// 相对路径里的一段能不能原样当作本机路径的一段：拼上去之后还在原来的目录下面，指的
/// 也就是叫这个名字的那个文件。
///
/// Windows 上只有可移植的名字（[`rclaw_store::is_portable_file_name`]）可以：`C:x` 会把
/// 前面的目录整个换掉，`a:b` 是备用数据流，`NUL` 是设备，结尾的点和空格会被去掉。别的
/// 系统上这些都是普通文件名（本机自己的文件可以叫这些名字），只有 `.`、`..` 和带 NUL 的
/// 不行。
fn local_part(part: &str) -> Option<&str> {
    let plain = rclaw_store::is_portable_file_name(part)
        || (cfg!(not(windows)) && part != "." && part != ".." && !part.contains('\0'));
    plain.then_some(part)
}

/// 某会话 artifact 目录相对 agent 根的路径（带尾斜杠）。
pub fn artifact_rel_dir(channel: &str, chat_id: &str) -> String {
    format!("artifacts/{}/", artifact_chat_key(channel, chat_id))
}

/// 没有内联的工具结果全文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub name: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// `sync_msg.refs` 列的内容：这条消息引用的工作区文件，和没有内联的工具结果全文。
/// 列为空 = 还没提取过（本机产生、尚未封段的行）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRefs {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactRef>,
}

impl StoredRefs {
    pub fn parse(column: Option<&str>) -> Option<StoredRefs> {
        serde_json::from_str(column?).ok()
    }

    pub fn to_column(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

fn first_text(data: &str) -> Option<String> {
    let message: Message = serde_json::from_str(data).ok()?;
    message
        .content
        .iter()
        .find_map(|block| block.as_text())
        .map(str::to_string)
}

fn with_text(data: &str, text: String) -> Option<String> {
    let mut message: Message = serde_json::from_str(data).ok()?;
    message.id = 0;
    message.content = vec![ContentBlock::text(text)];
    serde_json::to_string(&message).ok()
}

/// 本机形态的 `meta` → 线上形态。锚点换不成全局 id 时返回 `None`（这一行在线上是普通行）。
fn local_meta_to_wire(
    conn: &Connection,
    session: &SessionMap,
    meta: &str,
) -> Result<Option<serde_json::Value>> {
    if let Some((head_until, tail_from)) = raw::compaction_anchors(meta) {
        let Some(tail) = map::gid_of(conn, session.session_id, tail_from)? else {
            return Ok(None);
        };
        let head = match head_until {
            Some(id) => match map::gid_of(conn, session.session_id, id)? {
                Some(gid) => Some(gid),
                None => return Ok(None),
            },
            None => None,
        };
        return Ok(Some(serde_json::json!({
            "compaction": { "head_until": head, "tail_from": tail }
        })));
    }
    if let Some((parent_chat, anchor)) = raw::fork_notice_anchor(meta) {
        let mut parent_sid = None;
        let mut anchor_gid = None;
        if let Some(parent_chat) = parent_chat {
            if let Some(parent) = map::session_map_by_key(conn, &session.channel, &parent_chat)? {
                if let Some(anchor) = anchor {
                    anchor_gid = map::gid_of(conn, parent.session_id, anchor)?;
                }
                parent_sid = Some(parent.sid);
            }
        }
        return Ok(Some(serde_json::json!({
            "fork_notice": { "parent": parent_sid, "anchor": anchor_gid }
        })));
    }
    Ok(None)
}

/// 本机已有内容的 artifact 对象（`sync_blob` 的登记）。
#[derive(Debug, Clone)]
pub struct StoredObject {
    /// 消息里引用它的文件名。
    pub name: String,
    /// 登记的对象名（可能是旧版本按明文摘要登记的内联缓存）。
    pub blob: String,
    /// 登记的本机落点（相对 agent 根）。
    pub rel_path: String,
    /// 登记的长度。
    pub size: u64,
}

/// 一行消息的线上形态（数据库阶段的产物）。artifact 全文还没读，见 [`fill_artifacts`]。
#[derive(Debug, Clone)]
pub struct RowWire {
    pub message_id: i64,
    pub channel: String,
    pub chat_id: String,
    pub msg: WireMsg,
    /// `sync_msg.refs` 里记着的；`None` = 还没提取过。
    pub stored: Option<StoredRefs>,
    /// 全文不在本机的 artifact（来自别的设备，还没取回）：照原样转述，不读占位文件。
    pub absent: Vec<ArtifactRef>,
    /// `refs` 里记着、本机已有内容的对象：按对象取，不按同名的落点猜。
    pub stored_objects: Vec<StoredObject>,
    /// 正文里引用、`refs` 里却没有记录的 artifact 在本机登记过的候选（`(文件名,
    /// [(对象名, 长度)]`）。旧消息缺的索引：占位文件里写着原长度，长度对得上才算同一个
    /// 对象；同名但内容不同的文件不能被当成它。
    pub unindexed: Vec<(String, Vec<(String, u64)>)>,
}

/// 读出一行消息的线上形态。在调用方的事务（或读快照）里执行，不做文件 IO。
pub fn row_to_wire(
    conn: &Connection,
    env: &Env,
    session: &SessionMap,
    row: &RawMessageRow,
    mapped: &MsgMap,
) -> Result<RowWire> {
    let wire_meta = match (&mapped.wire_meta, &row.meta) {
        // 来自远端的行：线上原件原样转述，各端的可移植转录才一致。
        (Some(original), _) => serde_json::from_str(original).ok(),
        (None, Some(local)) => local_meta_to_wire(conn, session, local)?,
        (None, None) => None,
    };
    let mut data = portable::publishable_data(&row.data, &row.role, row.ts);
    let is_fork_notice = wire_meta
        .as_ref()
        .is_some_and(|meta| meta.get("fork_notice").is_some());
    if is_fork_notice {
        // 正文里写着本机的 chat_id 与消息编号：换成规范文本。
        if let Some(text) = first_text(&data) {
            let canonical = raw::fork_notice_text(None, raw::fork_notice_files_note(&text));
            if let Some(rewritten) = with_text(&data, canonical) {
                data = rewritten;
            }
        }
    }
    let prefix = env
        .sessions
        .artifacts_dir_prefix(&session.channel, &session.chat_id);
    let data = portable::to_portable(&data, &env.root, &prefix);
    let stored = StoredRefs::parse(mapped.refs.as_deref());
    let mut absent = Vec::new();
    let mut stored_objects = Vec::new();
    if let Some(stored) = &stored {
        for artifact in &stored.artifacts {
            match &artifact.blob {
                Some(blob) => match db::blob_get(conn, blob)? {
                    Some(blob_row) if blob_row.state != BLOB_REMOTE => {
                        stored_objects.push(StoredObject {
                            name: artifact.name.clone(),
                            blob: blob.clone(),
                            rel_path: blob_row.rel_path,
                            size: blob_row.size,
                        });
                    }
                    _ => absent.push(artifact.clone()),
                },
                None => absent.push(artifact.clone()),
            }
        }
    }
    // 旧消息的 `refs` 可能是在 artifact 记录加进来之前写的：正文引用的文件名与按同一
    // 落点登记的对象能对上的，留给 [`fill_artifacts`] 核对占位文件的长度后补上。
    let mut unindexed = Vec::new();
    if stored.is_some() {
        let rel_dir = artifact_rel_dir(&session.channel, &session.chat_id);
        for name in portable::artifact_names(&data) {
            if stored
                .as_ref()
                .is_some_and(|stored| stored.artifacts.iter().any(|known| known.name == name))
            {
                continue;
            }
            let objects = db::blob_objects_at(conn, &format!("{rel_dir}{name}"))?;
            if !objects.is_empty() {
                unindexed.push((name, objects));
            }
        }
    }
    Ok(RowWire {
        message_id: row.id,
        channel: session.channel.clone(),
        chat_id: session.chat_id.clone(),
        msg: WireMsg {
            gid: mapped.gid.clone(),
            parent: mapped.parent.clone(),
            role: row.role.clone(),
            ts: row.ts,
            data,
            meta: wire_meta,
            tools: raw::tool_call_metas(conn, row.session_id, row.id)?,
            refs: stored
                .as_ref()
                .map(|stored| stored.files.clone())
                .unwrap_or_default(),
            artifacts: Vec::new(),
        },
        stored,
        absent,
        stored_objects,
        unindexed,
    })
}

/// artifact 全文怎么带。
#[derive(Clone, Copy)]
pub struct ArtifactPolicy<'a> {
    /// 单个全文内联的上限（字节）。
    pub inline_max: usize,
    /// 一条消息内联全文的总上限。
    pub message_max: usize,
    /// 有密钥时，超出上限的全文登记为附件对象；没有就只带名字与大小。
    pub keys: Option<&'a Keys>,
}

impl ArtifactPolicy<'static> {
    /// 手动导出的会话包与回收站：本机文件，全文都带上。
    pub const BUNDLE: ArtifactPolicy<'static> = ArtifactPolicy {
        inline_max: BUNDLE_ARTIFACT_MAX,
        message_max: usize::MAX,
        keys: None,
    };
}

/// 会话包里单个 artifact 全文的上限。
pub const BUNDLE_ARTIFACT_MAX: usize = 64 * 1024 * 1024;
/// `msg` op 里内联全文的总上限。
pub const OP_ARTIFACTS_MAX: usize = 4 * 1024 * 1024;

/// 本机的一个待登记附件对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBlob {
    pub blob_id: String,
    pub rel_path: String,
    pub size: u64,
}

/// 内存里一段字节的 SHA-256 摘要。
fn digest_of(bytes: &[u8]) -> [u8; 32] {
    let mut reader = std::io::Cursor::new(bytes);
    sha256_reader(&mut reader).expect("内存读端不会出错").0
}

/// 内联全文写进同步缓存、登记成本机对象时用的对象名。
///
/// 有空间密钥时就是线上对象名（内容摘要的带密钥摘要），打开、保存、重传、重新封装都按
/// 普通附件对象处理。没有密钥（导入会话包时还没配对）只能按明文摘要登记：它只在本机
/// 有意义，不参与线上引用；以后封段重新封装时按文件内容换成线上对象名。
fn cached_text_id(keys: Option<&Keys>, text: &str) -> String {
    match keys {
        Some(keys) => keys.blob_id(&digest_of(text.as_bytes())),
        None => sha256_hex(text.as_bytes()),
    }
}

/// 登记的对象名是不是内容摘要的明文十六进制（旧版本内联缓存的登记方式）。
fn is_plain_digest(digest: [u8; 32], blob: &str) -> bool {
    ids::hex(&digest) == blob
}

/// 本机文件内容与登记的对象名做核对：`(线上对象名, 长度, 是否需要改登记)`。
///
/// 有密钥时按空间密钥算对象名；算出来与登记不符、但明文摘要与登记一致的是旧版本的内联
/// 缓存——对象名要换成带密钥的那个。没有密钥（导出会话包）只能核对明文摘要；带密钥的
/// 对象名验不了，照登记转述。
fn object_identity(
    keys: Option<&Keys>,
    digest: [u8; 32],
    len: u64,
    registered: &str,
) -> Option<(String, u64, bool)> {
    match keys {
        Some(keys) => {
            let current = keys.blob_id(&digest);
            if current == registered {
                Some((current, len, false))
            } else if is_plain_digest(digest, registered) {
                Some((current, len, true))
            } else {
                None
            }
        }
        // 没有密钥：明文摘要登记的内容核对得出来；带密钥的对象名验不了，按登记转述。
        None => Some((registered.to_string(), len, false)),
    }
}

/// 占位全文里写着的原长度。别的正文碰巧带这句话的，按完整格式再比一次。
fn placeholder_size(text: &str) -> Option<u64> {
    let rest = text.strip_prefix("（这段工具结果全文共 ")?;
    let size: u64 = rest.split_once(" 字节，")?.0.parse().ok()?;
    (text == placeholder_text(size)).then_some(size)
}

/// 登记过的对象用不了（不在本机、内容对不上）：照「还在别的设备上」转述，接收端以后
/// 还能按对象名去取；这一次不拿同名的文件充数。
fn as_absent(name: String, object: &StoredObject) -> Artifact {
    Artifact {
        name,
        text: None,
        size: object.size,
        blob: Some(object.blob.clone()),
    }
}

/// 旧登记归一成线上对象名之后，消息的附件清单跟着改：重新封装读到的就是同一个对象。
fn update_stored_artifact(row: &mut RowWire, name: &str, blob: &str, size: u64) {
    if let Some(stored) = &mut row.stored {
        if let Some(known) = stored.artifacts.iter_mut().find(|known| known.name == name) {
            known.blob = Some(blob.to_string());
            known.size = size;
        }
    }
}

/// 把消息引用到的 artifact 全文读进来（文件 IO，不碰数据库）。返回需要登记的附件对象。
pub fn fill_artifacts(env: &Env, row: &mut RowWire, policy: ArtifactPolicy<'_>) -> Vec<LocalBlob> {
    let mut blobs = Vec::new();
    let dir = env.sessions.artifacts_dir(&row.channel, &row.chat_id);
    let rel_dir = artifact_rel_dir(&row.channel, &row.chat_id);
    let mut inlined = 0usize;
    for name in portable::artifact_names(&row.msg.data)
        .into_iter()
        .take(256)
    {
        // 这条消息登记过、本机已有内容的对象：按对象取。内联全文撞名时它就在同步缓存
        // 里，artifact 目录里同名的那份是另一个对象，不能拿来充数。
        if let Some(object) = row
            .stored_objects
            .iter()
            .find(|object| object.name == name)
            .cloned()
        {
            let path = env.checked_path(&object.rel_path);
            let Ok(path) = path else {
                row.msg.artifacts.push(as_absent(name, &object));
                continue;
            };
            let Ok(meta) = std::fs::metadata(&path) else {
                row.msg.artifacts.push(as_absent(name, &object));
                continue;
            };
            if !meta.is_file() {
                row.msg.artifacts.push(as_absent(name, &object));
                continue;
            }
            if meta.len() <= policy.inline_max as u64 {
                let Ok(bytes) = std::fs::read(&path) else {
                    row.msg.artifacts.push(as_absent(name, &object));
                    continue;
                };
                let Some((blob, len, legacy)) = object_identity(
                    policy.keys,
                    digest_of(&bytes),
                    bytes.len() as u64,
                    &object.blob,
                ) else {
                    row.msg.artifacts.push(as_absent(name, &object));
                    continue;
                };
                let Ok(text) = String::from_utf8(bytes) else {
                    row.msg.artifacts.push(as_absent(name, &object));
                    continue;
                };
                if (len as usize) <= policy.message_max.saturating_sub(inlined) {
                    inlined += text.len();
                    row.msg.artifacts.push(Artifact {
                        name,
                        size: len,
                        text: Some(text),
                        blob: None,
                    });
                } else {
                    // 内联额度用完了：仍按对象转述，接收端以后取。这些对象来自内联正文的
                    // 本机缓存、从来没有上传过，必须一并排进上传队列（附件范围关着时由
                    // 调用方登记成 HELD）；旧登记还要把消息清单改成新的对象名。
                    row.msg.artifacts.push(Artifact {
                        name,
                        size: len,
                        text: None,
                        blob: Some(blob.clone()),
                    });
                    if legacy {
                        update_stored_artifact(row, &object.name, &blob, len);
                    }
                    blobs.push(LocalBlob {
                        blob_id: blob,
                        rel_path: object.rel_path.clone(),
                        size: len,
                    });
                }
                continue;
            }
            let Ok(mut file) = std::fs::File::open(&path) else {
                row.msg.artifacts.push(as_absent(name, &object));
                continue;
            };
            let Ok((digest, len)) = sha256_reader(&mut file) else {
                row.msg.artifacts.push(as_absent(name, &object));
                continue;
            };
            let Some((blob, len, legacy)) = object_identity(policy.keys, digest, len, &object.blob)
            else {
                row.msg.artifacts.push(as_absent(name, &object));
                continue;
            };
            row.msg.artifacts.push(Artifact {
                name,
                size: len,
                text: None,
                blob: Some(blob.clone()),
            });
            if legacy {
                update_stored_artifact(row, &object.name, &blob, len);
                blobs.push(LocalBlob {
                    blob_id: blob,
                    rel_path: object.rel_path.clone(),
                    size: len,
                });
            }
            continue;
        }
        if let Some(absent) = row.absent.iter().find(|artifact| artifact.name == name) {
            row.msg.artifacts.push(Artifact {
                name,
                text: None,
                size: absent.size,
                blob: absent.blob.clone(),
            });
            continue;
        }
        let path = dir.join(&name);
        if env.check_path(&format!("{rel_dir}{name}"), &path).is_err() {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let size = meta.len();
        // 旧消息的索引缺了这一条：占位文件里写着原长度，与本机按同一落点登记的对象
        // 长度对得上才算同一个对象；对不上的也不把占位正文当成工具结果发出去。
        if size <= 1024 {
            if let Some(placeholder) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| placeholder_size(&text))
            {
                if let Some((_, objects)) = row
                    .unindexed
                    .iter()
                    .find(|(candidate, _)| candidate == &name)
                {
                    if let Some(blob) = recovered_blob(objects, placeholder) {
                        let artifact = ArtifactRef {
                            name: name.clone(),
                            size: placeholder,
                            blob: Some(blob.clone()),
                        };
                        if let Some(stored) = &mut row.stored {
                            if !stored.artifacts.iter().any(|known| known.name == name) {
                                stored.artifacts.push(artifact);
                            }
                        }
                        row.msg.artifacts.push(Artifact {
                            name,
                            text: None,
                            size: placeholder,
                            blob: Some(blob),
                        });
                        continue;
                    }
                }
                row.msg.artifacts.push(Artifact {
                    name,
                    text: None,
                    size: placeholder,
                    blob: None,
                });
                continue;
            }
        }
        let fits = size <= policy.inline_max as u64
            && (size as usize) <= policy.message_max.saturating_sub(inlined);
        if fits {
            if let Ok(text) = std::fs::read_to_string(&path) {
                inlined += text.len();
                row.msg.artifacts.push(Artifact {
                    name,
                    size: text.len() as u64,
                    text: Some(text),
                    blob: None,
                });
            }
            continue;
        }
        let hashed = policy.keys.and_then(|keys| {
            let mut file = std::fs::File::open(&path).ok()?;
            let (digest, len) = sha256_reader(&mut file).ok()?;
            Some((keys.blob_id(&digest), len))
        });
        // 有对象名时，长度取算摘要时实际读到的字节数：名字和长度出自同一份内容。
        let size = hashed.as_ref().map_or(size, |(_, len)| *len);
        let blob = hashed.map(|(blob_id, _)| blob_id);
        if let Some(blob_id) = &blob {
            blobs.push(LocalBlob {
                blob_id: blob_id.clone(),
                rel_path: format!("{rel_dir}{name}"),
                size,
            });
        }
        row.msg.artifacts.push(Artifact {
            name,
            text: None,
            size,
            blob,
        });
    }
    blobs
}

/// 附件取回后的落点（相对 agent 根）。`uploads/` 按原路径；`media/`、`output/` 只进同步
/// 缓存，不覆盖工作区（§8.7）。
///
/// `uploads/` 下的路径也只有每一段都是可移植的名字时才按原路径落：别的系统上的文件名
/// 可以带 `:`、可以叫 `NUL`，在 Windows 上照着拼就不是那个文件了（甚至不在工作区里）。
/// 这样的附件同样进同步缓存，文件名改写过。规则不分平台，同一条引用在各台设备上的
/// 落点一致。
pub fn landing_path(path: &str, blob: &str) -> String {
    if path.starts_with("uploads/") && path.split('/').all(rclaw_store::is_portable_file_name) {
        return path.to_string();
    }
    cache_path(path, blob)
}

/// 同步缓存里的位置：`sync/files/<前两位>/<blob_id>/<文件名>`。文件名取路径的最后一段，
/// 改写成各个系统上都能原样存在的样子。
pub fn cache_path(path: &str, blob: &str) -> String {
    let name = rclaw_store::portable_file_name(path.rsplit('/').next().unwrap_or_default());
    format!("sync/files/{}/{blob}/{name}", &blob[..2.min(blob.len())])
}

pub(crate) fn placeholder_text(size: u64) -> String {
    format!(
        "（这段工具结果全文共 {size} 字节，保存在另一台设备上，还没有下载到本机。\
         可以在「多端同步」里下载附件后再读取。）\n"
    )
}

/// 线上形态的 `meta` → 本机形态。返回 `(meta, 分叉告知行要换成的正文)`。
fn wire_meta_to_local(
    conn: &Connection,
    session_id: i64,
    channel: &str,
    meta: &serde_json::Value,
    data: &str,
) -> Result<(Option<String>, Option<String>)> {
    if let Some(compaction) = meta.get("compaction") {
        let Some(tail) = compaction.get("tail_from").and_then(|v| v.as_str()) else {
            return Ok((None, None));
        };
        let Some(tail_from) = map::message_of(conn, session_id, tail)? else {
            return Ok((None, None));
        };
        let head_until = match compaction.get("head_until").and_then(|v| v.as_str()) {
            Some(head) => match map::message_of(conn, session_id, head)? {
                Some(id) => Some(id),
                None => return Ok((None, None)),
            },
            None => None,
        };
        // 按 `append_compaction` 同一套规则校验；不通过就降级为普通行。
        return Ok((
            raw::local_compaction_meta(conn, session_id, head_until, tail_from)?,
            None,
        ));
    }
    if let Some(notice) = meta.get("fork_notice") {
        let parent_sid = notice.get("parent").and_then(|v| v.as_str());
        let anchor_gid = notice.get("anchor").and_then(|v| v.as_str());
        let mut parent: Option<(String, i64)> = None;
        if let (Some(parent_sid), Some(anchor_gid)) = (parent_sid, anchor_gid) {
            for (parent_session, anchor_message) in map::find_gid(conn, parent_sid, anchor_gid)? {
                if let Some((parent_channel, parent_chat)) = raw::session_key(conn, parent_session)?
                {
                    if parent_channel == channel {
                        parent = Some((parent_chat, anchor_message));
                        break;
                    }
                }
            }
        }
        let parent_ref = parent.as_ref().map(|(chat, id)| (chat.as_str(), *id));
        let text = first_text(data).map(|canonical| {
            raw::fork_notice_text(parent_ref, raw::fork_notice_files_note(&canonical))
        });
        return Ok((Some(raw::fork_notice_meta(parent_ref)), text));
    }
    Ok((None, None))
}

/// 一条远端消息落到哪个本地会话。
#[derive(Debug, Clone, Copy)]
pub struct Landing<'a> {
    pub session_id: i64,
    pub channel: &'a str,
    pub chat_id: &'a str,
    /// 新行的 `sealed`：来自远端日志的是「已封段」，导入会话包的是「只分配了身份」。
    pub sealed: i64,
}

/// 把消息内联的 artifact 全文写进会话的 artifact 目录；没有内联的全文先放一个占位文件
/// （本机已有或以后取回时由 [`settle_aliases`] 换成全文）。内联全文撞上同名不同内容的
/// 文件时不覆盖，改放进按对象名命名的同步缓存——[`land_msg`] 会把它登记成附件，附件区
/// 按对象列出来。
///
/// `keys` 是空间密钥：有它时缓存按线上对象名（内容摘要的带密钥摘要）登记，之后打开、
/// 保存、重传、重新封装都走普通附件那条路；没有它（导入会话包时还没配对）只能按明文
/// 摘要登记成仅本机的缓存，重新发布时再归一。
///
/// 只做文件 IO，不碰数据库。调用方在开事务**之前**做完：事务里不做磁盘写入（库锁是
/// 会话读写共用的），而且提交之后消息里的句柄一定指向存在的文件。重复执行结果相同。
pub fn write_artifacts(
    env: &Env,
    channel: &str,
    chat_id: &str,
    msg: &WireMsg,
    keys: Option<&Keys>,
) -> Result<()> {
    if msg.artifacts.is_empty() {
        return Ok(());
    }
    let dir = env.sessions.artifacts_dir(channel, chat_id);
    let rel_dir = artifact_rel_dir(channel, chat_id);
    for artifact in &msg.artifacts {
        let path = dir.join(&artifact.name);
        env.check_path(&format!("{rel_dir}{}", artifact.name), &path)?;
        match &artifact.text {
            Some(text) => {
                // 落点冲突策略与取回下载一致（§8.7）：原位置已有内容不同的真实文件就
                // 不覆盖，全文放进按对象名命名的同步缓存，会话顶部的附件区列着它。
                let same = path.is_file()
                    && std::fs::read(&path)
                        .map(|bytes| bytes == text.as_bytes())
                        .unwrap_or(false);
                if path.exists() && !same {
                    let blob = cached_text_id(keys, text);
                    let rel = cache_path(&format!("{rel_dir}{}", artifact.name), &blob);
                    let full = env.checked_path(&rel)?;
                    if let Some(parent) = full.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    rclaw_store::atomic_write(&full, text.as_bytes())?;
                } else {
                    rclaw_store::atomic_write(&path, text.as_bytes())?;
                }
            }
            None if !path.exists() => {
                rclaw_store::atomic_write(&path, placeholder_text(artifact.size).as_bytes())?
            }
            // 原位置已有文件就不动：是我们写下的占位（等着取回的全文来替换），或是
            // 同一对象之前已经落过地；真实文件更不能覆盖。
            None => {}
        }
    }
    Ok(())
}

/// 把一条（已通过检查的）线上消息追加到本地会话末尾：消息行、工具元数据、映射、附件
/// 登记，并把分支末条推进到这条消息。在调用方的事务里执行，不做文件写入——artifact
/// 全文由调用方事先用 [`write_artifacts`] 写好，要复制的附件提交后由 [`settle_aliases`] 补。
///
/// `keys` 与 [`write_artifacts`] 传的是同一把：内联全文撞名落进同步缓存时，两边算出的
/// 必须是同一个对象名。
pub fn land_msg(
    conn: &Connection,
    env: &Env,
    landing: Landing<'_>,
    msg: &WireMsg,
    keys: Option<&Keys>,
) -> Result<i64> {
    let prefix = env
        .sessions
        .artifacts_dir_prefix(landing.channel, landing.chat_id);
    let mut data = portable::to_local(&msg.data, &env.root, &prefix);
    let mut meta = None;
    if let Some(wire_meta) = &msg.meta {
        let (local_meta, text) =
            wire_meta_to_local(conn, landing.session_id, landing.channel, wire_meta, &data)?;
        meta = local_meta;
        if let Some(rewritten) = text.and_then(|text| with_text(&data, text)) {
            data = rewritten;
        }
    }
    let message_id = raw::insert_raw_message(
        conn,
        landing.session_id,
        &msg.role,
        msg.ts,
        &data,
        meta.as_deref(),
        &msg.tools,
    )?;

    let mut stored = StoredRefs {
        files: msg.refs.clone(),
        artifacts: Vec::new(),
    };
    let rel_dir = artifact_rel_dir(landing.channel, landing.chat_id);
    for artifact in &msg.artifacts {
        if let Some(text) = &artifact.text {
            // 内联全文撞了名、被 [`write_artifacts`] 放进按对象名命名的同步缓存的：登记成
            // 本机的缓存附件（本机就有这份内容），附件区按对象列出来。没撞名的全文在
            // artifact 目录里，消息自己就能打开，不进附件清单。缓存文件在不在与
            // `write_artifacts` 的落点决定是同一个判据，重复执行结果相同。
            let blob = cached_text_id(keys, text);
            let rel = cache_path(&format!("{rel_dir}{}", artifact.name), &blob);
            if env.resolve(&rel).is_file() {
                // 登记的长度取真正写下的字节数：对象名和长度出自同一份内容。
                let size = text.len() as u64;
                db::blob_register(conn, &blob, &rel, size, BLOB_FETCHED)?;
                stored.artifacts.push(ArtifactRef {
                    name: artifact.name.clone(),
                    size,
                    blob: Some(blob),
                });
            }
            continue;
        }
        stored.artifacts.push(ArtifactRef {
            name: artifact.name.clone(),
            size: artifact.size,
            blob: artifact.blob.clone(),
        });
        if let Some(blob) = &artifact.blob {
            let rel_path = format!("{rel_dir}{}", artifact.name);
            expect_blob(conn, env, blob, &rel_path, artifact.size)?;
            // 一条新消息引用它：写下这条消息的设备当时有这份内容（§8.7）。
            crate::attach::on_new_ref(conn, blob)?;
        }
    }
    for file in &msg.refs {
        let target = landing_path(&file.path, &file.blob);
        expect_blob(conn, env, &file.blob, &target, file.size)?;
        crate::attach::on_new_ref(conn, &file.blob)?;
    }
    map::insert_msg_map(
        conn,
        message_id,
        landing.session_id,
        &msg.gid,
        msg.parent.as_deref(),
        landing.sealed,
        Some(&stored.to_column()),
        msg.meta.as_ref().map(|meta| meta.to_string()).as_deref(),
    )?;
    map::set_tip(conn, landing.session_id, Some(&msg.gid))?;
    Ok(message_id)
}

/// 占位文件能和哪个登记对上：只认长度唯一的那个；同一长度对上几个不同的对象就不猜。
fn recovered_blob(objects: &[(String, u64)], size: u64) -> Option<String> {
    let mut found: Option<&str> = None;
    for (blob, object_size) in objects {
        if *object_size != size {
            continue;
        }
        match found {
            Some(known) if known != blob => return None,
            Some(_) => {}
            None => found = Some(blob),
        }
    }
    found.map(str::to_string)
}

/// 已封段的历史行：`refs` 里缺的 artifact 记录补上（旧版本先有文件引用、后来才加
/// artifact 记录；那些行不会再进封段队列，只能在这里补）。每个会话清点一次。
///
/// 只补能准确重建的：落点上的占位文件写着原长度，本机 `sync_blob` 里按同一个落点登记的
/// 对象长度与它唯一对上，才认成同一个对象。对不上的不猜——同名但内容不同的文件不能当成
/// 它。返回补上的条数。
pub fn backfill_artifact_refs(env: &Env, channel: &str, chat_id: &str) -> Result<usize> {
    let rel_dir = artifact_rel_dir(channel, chat_id);
    let prefix = env.sessions.artifacts_dir_prefix(channel, chat_id);
    let (session_id, candidates) = db::read(env.db(), |conn| {
        if !sync_db::sync_tables_exist(conn)? {
            return Ok((None, Vec::new()));
        }
        let Some(session_id) = raw::find_session(conn, channel, chat_id)? else {
            return Ok((None, Vec::new()));
        };
        // 清点过的会话不再扫第二遍：这次补的是已封段的历史行，以后封段的行自带索引。
        if db::state_get(conn, SCOPE_REFS_INDEX, &session_id.to_string())?.is_some() {
            return Ok((None, Vec::new()));
        }
        let mut stmt = conn.prepare_cached(
            "SELECT message_id, data, refs FROM sync_msg WHERE session_id = ?1 AND sealed = ?2 \
             AND refs IS NOT NULL ORDER BY message_id",
        )?;
        let rows = stmt.query_map(params![session_id, sync_db::SEAL_SEALED], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (message_id, data, column) = row?;
            let Some(refs) = StoredRefs::parse(Some(&column)) else {
                continue;
            };
            let portable = portable::to_portable(&data, &env.root, &prefix);
            for name in portable::artifact_names(&portable) {
                if refs.artifacts.iter().any(|known| known.name == name) {
                    continue;
                }
                let objects = db::blob_objects_at(conn, &format!("{rel_dir}{name}"))?;
                if !objects.is_empty() {
                    out.push((message_id, name, objects));
                }
            }
        }
        Ok((Some(session_id), out))
    })?;
    let Some(session_id) = session_id else {
        return Ok(0);
    };
    // 文件 IO 在库锁之外。占位文件很短；读不出来（不是「文件不在」）就留着下次再扫。
    let mut updates: Vec<(i64, ArtifactRef)> = Vec::new();
    let mut retry = false;
    for (message_id, name, objects) in candidates {
        let path = env.checked_path(&format!("{rel_dir}{name}"));
        let Ok(path) = path else {
            continue;
        };
        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if !meta.is_file() || meta.len() > 1024 {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                retry |= e.kind() != std::io::ErrorKind::NotFound;
                continue;
            }
        };
        let Some(size) = placeholder_size(&text) else {
            continue;
        };
        if let Some(blob) = recovered_blob(&objects, size) {
            updates.push((
                message_id,
                ArtifactRef {
                    name,
                    size,
                    blob: Some(blob),
                },
            ));
        }
    }
    db::write(env.db(), |tx| {
        let mut written = 0usize;
        for (message_id, artifact) in &updates {
            // 清单可能在这期间被清掉（会话被删、换空间）：留着下一个事务重新看一眼。
            let Ok(column) = tx.query_row(
                "SELECT refs FROM sync_msg WHERE message_id = ?1",
                params![message_id],
                |row| row.get::<_, Option<String>>(0),
            ) else {
                continue;
            };
            let Some(mut stored) = StoredRefs::parse(column.as_deref()) else {
                continue;
            };
            if stored
                .artifacts
                .iter()
                .any(|known| known.name == artifact.name)
            {
                continue;
            }
            stored.artifacts.push(artifact.clone());
            tx.execute(
                "UPDATE sync_msg SET refs = ?2 WHERE message_id = ?1",
                params![message_id, stored.to_column()],
            )?;
            written += 1;
        }
        if !retry {
            db::state_put(
                tx,
                SCOPE_REFS_INDEX,
                &session_id.to_string(),
                db::HLC_ZERO,
                None,
                None,
            )?;
        }
        Ok(written)
    })
}

/// 附件索引已清点过的会话（`sync_state` 的 `refs-index`）。
const SCOPE_REFS_INDEX: &str = "refs-index";

/// artifact 的占位文件可以被取回的全文覆盖；工作区里的文件不覆盖（§8.7）。可覆盖的
/// 只有同步自己写下的占位（内容一字不差）：用户改写的、别的版本落过地的都是真实文件。
/// 读不出来（权限、被占用）返回 `Err`：不能当成「不是占位」销账，留着重试。
fn may_overwrite(rel_path: &str, target: &std::path::Path, size: u64) -> Result<bool> {
    if !rel_path.starts_with("artifacts/") {
        return Ok(false);
    }
    std::fs::read(target)
        .map(|bytes| bytes == placeholder_text(size).as_bytes())
        .map_err(Into::into)
}

/// 别名落点补全的结果。
enum Copied {
    /// 复制成功。
    Done,
    /// 目标已经是真实文件（不是占位）：不覆盖。对象在同步缓存里有完整的一份，附件入口
    /// 按对象找得到——这条落点销掉。
    Conflict,
    /// 暂时写不进去或读不了（权限、文件被占）：留着下轮再试。
    Retry,
}

fn copy_into_place(env: &Env, source_rel: &str, target_rel: &str, size: u64) -> Copied {
    let (Ok(source), Ok(target)) = (env.checked_path(source_rel), env.checked_path(target_rel))
    else {
        return Copied::Retry;
    };
    if target.exists() {
        match may_overwrite(target_rel, &target, size) {
            Ok(true) => {}
            Ok(false) => return Copied::Conflict,
            // 读不了不是「有冲突」：销了账这份落点就永远缺着，留下轮再试。
            Err(_) => return Copied::Retry,
        }
    }
    if let Some(parent) = target.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return Copied::Retry;
        }
    }
    // 先写同目录的临时文件再改名：读者看不到复制了一半的文件。
    let staging = target.with_extension(format!("part-{}", crate::ids::random_hex::<4>()));
    if std::fs::copy(source, &staging).is_err() {
        let _ = std::fs::remove_file(&staging);
        return Copied::Retry;
    }
    if std::fs::rename(&staging, &target).is_err() {
        let _ = std::fs::remove_file(&staging);
        return Copied::Retry;
    }
    Copied::Done
}

/// 登记一个本机应当有的附件对象，落点是 `rel_path`。本机还没有这个对象（内容寻址）就
/// 登记为「等待取回」；已经有了、但在别的位置，就记下「还要落到这里」，由
/// [`settle_aliases`] 在事务之外复制。这里不做文件写入。
pub(crate) fn expect_blob(
    conn: &Connection,
    env: &Env,
    blob: &str,
    rel_path: &str,
    size: u64,
) -> Result<()> {
    db::blob_register(conn, blob, rel_path, size, BLOB_REMOTE)?;
    let Some(existing) = db::blob_get(conn, blob)? else {
        return Ok(());
    };
    let local = existing.state != BLOB_REMOTE;
    if local && !env.resolve(&existing.rel_path).is_file() {
        // 登记说本机有，文件却不在了（会话被删、用户清理）：改回「等待取回」。
        db::blob_set(conn, blob, rel_path, BLOB_REMOTE)?;
        return Ok(());
    }
    if existing.rel_path == rel_path {
        return Ok(());
    }
    // 落点在同步缓存里、而本机已有这个对象时不必再复制一份：界面按对象找到已有的文件。
    if local && rel_path.starts_with(SYNC_FILES_PREFIX) {
        return Ok(());
    }
    db::state_put(
        conn,
        SCOPE_BLOB_ALIAS,
        &format!("{blob}|{rel_path}"),
        db::HLC_ZERO,
        None,
        None,
    )
}

const SYNC_FILES_PREFIX: &str = "sync/files/";

/// 同一个附件对象的其他落点（`sync_state`，键 `<blob_id>|<相对路径>`）。
pub const SCOPE_BLOB_ALIAS: &str = "blob-alias";

/// 把登记过的「其他落点」补上：对象已经在本机的，复制到位并销掉登记；还在别的设备上的
/// 留着，等取回之后再补。文件复制在库锁之外做。`blob` 给定时只看这一个对象。返回补上的
/// 个数。
///
/// `still_valid` 是这次工作的提交许可（换空间期间的作废判据）：复制与销账都要在许可
/// 之内——许可没了（旧世代已被收尾）就不能再改名进旧落点、销掉旧账目；给 `None` 的
/// 调用方不受约束（引擎轮次内的补落点，不在换空间的提交边界里）。
///
/// 成功、源目标相同、登记没了才销账。目标已是真实文件（重名冲突）也销：对象在同步缓存
/// 里有完整的一份，附件入口按对象打开的是缓存。暂时写不进去的留着，下一轮再补。
pub fn settle_aliases(
    env: &Env,
    blob: Option<&str>,
    still_valid: Option<&dyn Fn() -> bool>,
) -> Result<usize> {
    let allowed = || still_valid.is_none_or(|valid| valid());
    let prefix = blob.map(|blob| format!("{blob}|")).unwrap_or_default();
    let todo: Vec<(String, String, String, u64)> = db::read(env.db(), |conn| {
        let mut todo = Vec::new();
        for (key, _) in db::state_scan(conn, SCOPE_BLOB_ALIAS, &prefix)? {
            let Some((blob_id, target)) = key.split_once('|') else {
                continue;
            };
            let Some(row) = db::blob_get(conn, blob_id)? else {
                // 对象的登记没了（附件被回收）：这条落点也不用留。
                todo.push((key.clone(), String::new(), String::new(), 0));
                continue;
            };
            if row.state != BLOB_REMOTE {
                todo.push((key.clone(), row.rel_path, target.to_string(), row.size));
            }
        }
        Ok(todo)
    })?;
    if todo.is_empty() {
        return Ok(0);
    }
    let mut done = 0usize;
    let mut settled: Vec<&String> = Vec::new();
    for (key, source, target, size) in &todo {
        if source.is_empty() || source == target {
            settled.push(key);
            continue;
        }
        // 复制挂在慢盘上时可能正赶上换空间收尾：收尾之后这份许可就没了，不能再把文件
        // 改名进旧落点。这一条留着，等下一轮（新世代里登记还在的话）再补。
        if !allowed() {
            break;
        }
        match copy_into_place(env, source, target, *size) {
            Copied::Done => {
                done += 1;
                settled.push(key);
            }
            Copied::Conflict => settled.push(key),
            Copied::Retry => tracing::debug!("附件没有补到 {target}（写入失败，下轮再试）"),
        }
    }
    if !settled.is_empty() && allowed() {
        db::write(env.db(), |tx| {
            for key in &settled {
                db::state_del(tx, SCOPE_BLOB_ALIAS, key)?;
            }
            Ok(())
        })?;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landing_paths_follow_the_source_directory() {
        let blob = "ab".repeat(32);
        assert_eq!(landing_path("uploads/a b.png", &blob), "uploads/a b.png");
        assert_eq!(
            landing_path("output/报告.docx", &blob),
            format!("sync/files/ab/{blob}/报告.docx")
        );
        assert_eq!(
            landing_path("media/2026/x:y?.jpg", &blob),
            format!("sync/files/ab/{blob}/x_y_.jpg")
        );
    }

    /// `uploads/` 下的引用只有每一段都是可移植的名字才落回工作区；在 Windows 上会逃出
    /// 工作区、指向设备或备用数据流的，一律进同步缓存，文件名改写过。
    #[test]
    fn non_portable_upload_paths_land_in_the_cache() {
        let blob = "cd".repeat(32);
        for (path, name) in [
            ("uploads/D:evil/run.bat", "run.bat"),
            ("uploads/C:version.dll", "C_version.dll"),
            ("uploads/C:..", "C_"),
            ("uploads/12:30.png", "12_30.png"),
            ("uploads/a.png:stream", "a.png_stream"),
            ("uploads/NUL", "_NUL"),
            ("uploads/con.txt", "_con.txt"),
            ("uploads/COM1/x.png", "x.png"),
            ("uploads/a./x.png", "x.png"),
            ("uploads/x.png.", "x.png"),
            ("uploads/x.png ", "x.png"),
        ] {
            assert_eq!(
                landing_path(path, &blob),
                format!("sync/files/cd/{blob}/{name}"),
                "{path}"
            );
        }
        for path in [
            "uploads/a.png",
            "uploads/子目录/报告 (1).docx",
            "uploads/.hidden/x",
        ] {
            assert_eq!(landing_path(path, &blob), path);
        }
    }

    /// 不管相对路径写成什么样，解析结果都在根的下面。
    #[test]
    fn resolve_never_leaves_its_root() {
        let (dir, ctx) = crate::testing::ctx();
        let env = &ctx.env;
        let base = env.base().to_path_buf();
        let sync_root = dir.path().join("sync");
        for rel in [
            "uploads/D:evil/run.bat",
            "uploads/C:version.dll",
            "uploads/C:",
            "uploads/../../etc/passwd",
            "uploads/./x",
            "uploads/..",
            "uploads/NUL",
            "uploads/a\0b",
            "D:x",
            "..",
            "sync/../x",
            "sync/C:x",
        ] {
            let path = env.resolve(rel);
            let root = if rel.starts_with(SYNC_DIR_PREFIX) {
                &sync_root
            } else {
                &base
            };
            assert!(path.starts_with(root), "{rel} → {}", path.display());
            let inside = path.strip_prefix(root).unwrap();
            assert!(
                inside
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))),
                "{rel} → {}",
                path.display()
            );
            assert_eq!(
                inside.components().count(),
                rel.strip_prefix(SYNC_DIR_PREFIX)
                    .unwrap_or(rel)
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .count(),
                "{rel} → {}",
                path.display()
            );
        }
        // 普通的路径原样拼上去，两种解析一致。
        let plain = env.resolve("uploads/子目录/a b.png");
        assert_eq!(plain, base.join("uploads").join("子目录").join("a b.png"));
        assert_eq!(env.resolve_exact("uploads/子目录/a b.png"), Some(plain));
        // 拼不得的段：`resolve_exact` 不给路径。
        assert_eq!(env.resolve_exact("uploads/../x"), None);
        assert_eq!(env.resolve_exact("uploads/a\0b"), None);
        #[cfg(windows)]
        for rel in [
            "uploads/C:x",
            "uploads/12:30.png",
            "uploads/NUL",
            "uploads/x.",
        ] {
            assert_eq!(env.resolve_exact(rel), None, "{rel}");
        }
        #[cfg(not(windows))]
        assert_eq!(
            env.resolve_exact("uploads/12:30.png"),
            Some(base.join("uploads").join("12:30.png"))
        );
    }

    #[test]
    fn stored_refs_round_trip_and_tolerate_garbage() {
        let refs = StoredRefs {
            files: vec![FileRef {
                path: "uploads/a.png".into(),
                blob: "a".repeat(64),
                size: 3,
            }],
            artifacts: vec![ArtifactRef {
                name: "1-shell.txt".into(),
                size: 9,
                blob: None,
            }],
        };
        assert_eq!(StoredRefs::parse(Some(&refs.to_column())), Some(refs));
        assert_eq!(StoredRefs::parse(Some("{}")), Some(StoredRefs::default()));
        assert_eq!(StoredRefs::parse(Some("garbage")), None);
        assert_eq!(StoredRefs::parse(None), None);
    }

    #[test]
    fn fork_notice_text_is_rewritten_without_touching_the_message_shape() {
        let local = raw::fork_notice_text(Some(("c-parent", 42)), "（文件已回滚）");
        let data = serde_json::to_string(&Message::plain(rclaw_core::Role::User, local)).unwrap();
        let text = first_text(&data).unwrap();
        let canonical = raw::fork_notice_text(None, raw::fork_notice_files_note(&text));
        let rewritten = with_text(&data, canonical.clone()).unwrap();
        assert!(!rewritten.contains("c-parent"));
        assert!(rewritten.contains("（文件已回滚）"));
        assert_eq!(first_text(&rewritten).unwrap(), canonical);
        portable::check_message_data(&rewritten, "user").unwrap();
    }
}
