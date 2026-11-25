//! 技能（§8.4）：`skills/<名字>/` 整个目录是一个版本，按技能名后写者胜。
//!
//! - 只同步用户技能与 agent 自建技能。内置技能随各端的程序下发，一个文件也不上传；归属、
//!   停用、固定这些判断都在工具层，经 [`SkillHost`] 问。
//! - 一个版本就是一份文件清单。小文件的内容直接写在 `skill.put` 里，大文件走附件对象
//!   （不论「同步附件」这个范围开没开都会传）。本机专有的文件不算在版本里，同步也不动
//!   它们：点文件与点目录、根上的 `config.toml`、密钥文件、解释器与包管理器的产物
//!   （[`is_local_only`]）。
//! - 远端的版本先在技能目录旁边的备料目录里备齐（[`SkillHost::staging_dir`]，和技能在
//!   同一个卷上）、逐个文件核对过摘要，再在没有 turn 运行的时候整个换上去（与技能
//!   curator 持同一道静默门）；本机原来的版本进 `skills/.archive/`。持着静默门的时候只
//!   改名，不复制。
//! - 换不上去（文件被占着、盘满之类）不是这个版本的问题：隔一阵再试，间隔逐次加长，
//!   连着失败才提示。[`REJECTED`] 只留给清单本身过不了检查的版本。
//! - 两种改动要用户在本机确认之后才落地：带来了新的或改过的可执行文件（脚本、二进制、
//!   声明入口的 `skill.toml`）的版本，以及动到本机固定的技能的任何改动。确认之前技能目录
//!   里看不到它。
//! - 写进模型上下文的那几份文件（`SKILL.md`、`EXPERIENCE.md`、`skill.toml`）不能带不可见
//!   的控制字符，与 `skill_manage` 的写入检查一致。文件内容原样传，不做脱敏：被改过内容
//!   的脚本再同步回去会把原件覆盖掉。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rusqlite::{params, Connection};

use super::{
    mark, mtime_ms, prepare, publish_prepared, put_remote, to_row, Cache, FilesReport, Row, Stamp,
    AWAITING, HANDLED, REJECTED,
};
use crate::crypto::{sha256_hex, sha256_reader, Keys};
use crate::db::{self, scope, BLOB_FETCHED, BLOB_PENDING, BLOB_REMOTE, BLOB_UPLOADED};
use crate::error::{Result, SyncError};
use crate::ids;
use crate::op::{Op, SkillFile};
use crate::publish::Settings;
use crate::status::{self, PendingItem};
use crate::wire::{self, LocalBlob};
use crate::{Ctx, Reload, SkillHost, SkillKind};

pub(crate) const KIND: &str = "skill";
/// 上次同步后本机版本的摘要：键是技能名。
const SCOPE_BASE: &str = "skill-base";
/// 用户拒绝过的远端版本：键是技能名，值是那一版的摘要（删除记为 [`REJ_DEL`]）。
const SCOPE_REJ: &str = "skill-rej";
/// 用户在本机确认过、还没装上的远端版本：键是技能名，值同上。
const SCOPE_OK: &str = "skill-ok";
/// 本机现在装着的这个版本是从哪台设备同步来的：键是技能名，值是那台设备的标识。装上
/// 远端的版本时记下，本机发布了自己的版本（改过、删了）就撤掉。只用来在技能列表里标出
/// 来源（§8.4：依赖 shell 的技能到了手机上不一定能跑，不做过滤，只标来源），不参与同步的
/// 任何判断。
const SCOPE_ORIGIN: &str = "skill-origin";
const REJ_DEL: &str = "del";
const CACHE_KEY: &str = "skills";
/// 本机同步不了的技能的提示。
const NOTICE_SKIPPED: &str = "skill-skipped";
/// 远端的版本一直换不上去的提示（键是技能名）。换上去了、或者不用换了就撤掉。
const NOTICE_RETRY: &str = "skill-retry";
/// 连着失败这么多次才提示：头一次多半是一时的（文件正被别的程序占着之类）。
const RETRY_NOTICE_AFTER: u32 = 2;
/// 远端版本里的大文件取不回来了：过这么久还是这样才提示。
const STUCK_NOTICE_AFTER_MS: i64 = 2 * 60 * 1000;

/// 内联进 `skill.put` 的单个文件上限；更大的走附件对象。
const INLINE_MAX: u64 = 256 * 1024;
/// 一条 `skill.put` 里内联内容的总预算；放不下的也走附件对象。
const INLINE_TOTAL: u64 = 4 * 1024 * 1024;
/// 一个技能最多这么多个文件、这么多层路径。
const FILES_MAX: usize = 2000;
const DEPTH_MAX: usize = 12;
const PATH_MAX: usize = 400;
/// 合入时的硬上限：超过的 `skill.put` 直接丢弃，不进寄存器。比发布端的预算宽得多，
/// 以后调整预算时旧版本的设备还收得下。
const RECORD_FILES_MAX: usize = 20_000;
const RECORD_INLINE_MAX: usize = 64 * 1024 * 1024;
/// 写进模型上下文的文件：必须是内联的文本，不能带不可见的控制字符。
const ROOT_TEXT: [&str; 3] = ["SKILL.md", "EXPERIENCE.md", "skill.toml"];

/// 按扩展名就算可执行内容的文件。
const EXECUTABLE_EXTS: &[&str] = &[
    "py", "pyw", "sh", "bash", "zsh", "fish", "ksh", "csh", "ps1", "psm1", "psd1", "bat", "cmd",
    "com", "exe", "dll", "so", "dylib", "msi", "scr", "vbs", "vbe", "wsf", "wsh", "js", "mjs",
    "cjs", "jse", "ts", "rb", "pl", "pm", "php", "lua", "tcl", "awk", "jar", "class", "apk", "bin",
    "elf", "wasm", "appimage", "deb", "rpm", "command", "run", "out",
];

/// 合入时记一条 `skill.put` / `skill.del`。这里只查技能名和大小的硬上限；清单本身到落地
/// 时才检查——这个版本的程序不接受的清单留在寄存器里，升级之后还能用上。
pub(crate) fn record(conn: &Connection, hlc: &str, op: &Op, stored: &str) -> Result<bool> {
    let name = match op {
        Op::SkillPut { name, files, .. } => {
            let inline: usize = files
                .iter()
                .map(|file| {
                    file.text.as_ref().map_or(0, String::len)
                        + file.b64.as_ref().map_or(0, String::len)
                })
                .sum();
            if files.len() > RECORD_FILES_MAX || inline > RECORD_INLINE_MAX {
                return Err(SyncError::invalid("技能版本过大"));
            }
            name
        }
        Op::SkillDel { name, .. } => name,
        _ => return Ok(false),
    };
    if !is_skill_name(name) {
        return Err(SyncError::invalid("技能名不合规"));
    }
    let won = put_remote(conn, scope::SKILL, name, hlc, stored)?;
    if let (true, Op::SkillPut { files, .. }) = (won, op) {
        // 一个新版本引用这些大文件：发布它的设备当时有这些内容。以前记下「取不回来」的
        // 不算数了（§8.7），不然同样内容的文件换个版本再发布也装不上。
        for blob in files.iter().filter_map(|file| file.blob.as_deref()) {
            if ids::is_digest(blob) {
                crate::attach::on_new_ref(conn, blob)?;
            }
        }
    }
    Ok(won)
}

/// 一个路径段在各个系统上都能原样建出来（Windows 的保留名、结尾的点和空格都不行）。
fn is_portable_component(part: &str) -> bool {
    part.len() <= 200 && !part.starts_with(['.', ' ']) && rclaw_store::is_portable_file_name(part)
}

/// 技能名：工具层的规则（小写字母、数字、`-_.`，字母或数字开头，不超过 64 个字符），
/// 外加各个系统上都建得出同名目录。
fn is_skill_name(name: &str) -> bool {
    name.len() <= 64
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
        && is_portable_component(name)
}

/// 把名字折叠成比较用的形态：在大小写不敏感的文件系统上是同一个名字的，折叠后相同。
///
/// - 逐字符先转大写再转小写。NTFS 按大写表比较名字，`ſ`（长 s）、`ı`（无点 i）大写之后
///   就是普通的 `S`、`I`；APFS 按小写折叠，`K`（开尔文符号）折叠后就是 `k`。只转一个
///   方向会漏掉另一半。转换结果不止一个字符的（`ß` → `SS`）不算：文件系统的大小写表是
///   一对一的。
/// - 去掉 HFS+ 比较名字时不看的那几个不可见字符（零宽连接符、方向控制符、BOM）。
///
/// 远端来的名字凡是要和「有特殊含义的名字」比的地方都用它：`Skill.toml`、`ſkill.toml` 在
/// Windows 和 macOS 上打开的就是 `skill.toml`。
fn fold(name: &str) -> String {
    fn single(mut mapped: impl Iterator<Item = char>, original: char) -> char {
        match (mapped.next(), mapped.next()) {
            (Some(c), None) => c,
            _ => original,
        }
    }
    name.chars()
        .filter(|c| {
            !matches!(c, '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}')
        })
        .map(|c| single(c.to_uppercase(), c))
        .map(|c| single(c.to_lowercase(), c))
        .collect()
}

/// `path` 是技能根上的文件、名字和某个契约文件（[`ROOT_TEXT`]）折叠后相同时，返回那个
/// 契约文件的名字。
fn root_contract(path: &str) -> Option<&'static str> {
    if path.contains('/') {
        return None;
    }
    let folded = fold(path);
    ROOT_TEXT.into_iter().find(|name| fold(name) == folded)
}

/// 本机专有、不随技能同步的文件或目录（参数是相对技能目录的路径，正斜杠）：点文件与
/// 点目录（固定标记、遥测、补丁备份、虚拟环境）、根上的 `config.toml`、密钥文件、解释器
/// 与包管理器的产物。宿主换上新版本时把它们从旧版本带过去（[`SkillHost::install`]）。
/// 不分大小写：只差大小写的名字在 Windows 和 macOS 上就是这些文件本身。
pub fn is_local_only(rel_path: &str) -> bool {
    let folded = fold(rel_path);
    let name = folded.rsplit('/').next().unwrap_or_default();
    folded == "config.toml"
        || name.ends_with("_key.toml")
        || name.ends_with(".pyc")
        || name.ends_with(".pyo")
        || folded
            .split('/')
            .any(|part| part.starts_with('.') || part == "__pycache__" || part == "node_modules")
}

/// 清单里的一个文件路径是否合规。
fn is_file_path(path: &str) -> bool {
    path.len() <= PATH_MAX
        && path.split('/').count() <= DEPTH_MAX
        && path.split('/').all(is_portable_component)
        && !is_local_only(path)
}

/// 是不是可执行内容：声明入口的 `skill.toml`、脚本和二进制的扩展名、`#!` 开头的脚本、
/// 可执行文件的魔数。`head` 是文件开头的几个字节，拿不到时只看路径。
fn is_executable(path: &str, head: Option<&[u8]>) -> bool {
    if root_contract(path) == Some("skill.toml") {
        return true;
    }
    let ext = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| fold(ext));
    if ext.is_some_and(|ext| EXECUTABLE_EXTS.contains(&ext.as_str())) {
        return true;
    }
    let Some(head) = head else {
        return false;
    };
    head.starts_with(b"#!")
        || head.starts_with(b"MZ")
        || head.starts_with(b"\x7fELF")
        || matches!(
            head.get(..4),
            Some(
                [0xfe, 0xed, 0xfa, 0xce]
                    | [0xfe, 0xed, 0xfa, 0xcf]
                    | [0xce, 0xfa, 0xed, 0xfe]
                    | [0xcf, 0xfa, 0xed, 0xfe]
                    | [0xca, 0xfe, 0xba, 0xbe]
            )
        )
}

/// 文本里有没有不可见的控制字符（开头的 BOM 不算）。
fn has_hidden_chars(text: &str) -> bool {
    rclaw_store::scan_skill_text(text.strip_prefix('\u{feff}').unwrap_or(text)).is_some()
}

/// 一个版本的摘要：各文件的 `(路径, 内容摘要)`，与清单的排列顺序无关。
fn version_hash<'a>(files: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let mut lines: Vec<String> = files
        .map(|(path, sha256)| format!("{path}\n{sha256}\n"))
        .collect();
    lines.sort();
    sha256_hex(lines.concat().as_bytes())
}

/// 远端清单的版本摘要。自己算，不看 op 里带的 `hash`。
fn files_hash(files: &[SkillFile]) -> String {
    version_hash(
        files
            .iter()
            .map(|file| (file.path.as_str(), file.sha256.as_str())),
    )
}

/// 技能目录里的一个文件。
#[derive(Debug, Clone)]
struct LocalFile {
    /// 相对技能目录的路径，正斜杠。
    path: String,
    abs: PathBuf,
    size: u64,
    mtime: i64,
}

struct Hashed {
    file: LocalFile,
    sha256: String,
    /// 是不是可执行内容（随清单发出去，供别的设备提前判断；对方不全信它）。
    runnable: bool,
}

/// 本机的一个版本：摘要都算好了的文件清单。
struct Local {
    files: Vec<Hashed>,
    hash: String,
}

/// 一个技能这一轮没法同步。
enum Unsynced {
    /// 它同步不了，原因给用户看；改好之前不发布，也不拿远端的版本覆盖它。
    Skip(String),
    /// 文件正在被改（读到一半变了）：下一轮再来。
    Busy,
}

/// 符号链接、Windows 上的目录联接：不跟进去，也不算在版本里。
fn is_redirect(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if meta.is_dir() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// 列出一个技能目录里算在版本里的文件，按路径排序。同步不了的原因给用户看。
fn list_skill(dir: &Path, max_size: u64) -> std::result::Result<Vec<LocalFile>, String> {
    let mut out: Vec<LocalFile> = Vec::new();
    let mut pending: Vec<(PathBuf, String, usize)> = vec![(dir.to_path_buf(), String::new(), 1)];
    while let Some((at, prefix, depth)) = pending.pop() {
        let unreadable = |error: std::io::Error| {
            let shown = if prefix.is_empty() {
                "."
            } else {
                prefix.trim_end_matches('/')
            };
            format!("读不了目录「{shown}」（{error}）")
        };
        for entry in std::fs::read_dir(&at).map_err(&unreadable)? {
            let entry = entry.map_err(&unreadable)?;
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                return Err(format!("「{prefix}」下有文件名不是合法的 UTF-8"));
            };
            let path = format!("{prefix}{name}");
            if is_local_only(&path) {
                continue;
            }
            let meta = entry.metadata().map_err(&unreadable)?;
            if is_redirect(&meta) {
                continue;
            }
            if !is_portable_component(name) || path.len() > PATH_MAX {
                return Err(format!("「{path}」这个名字在别的系统上建不出来"));
            }
            if meta.is_dir() {
                if depth >= DEPTH_MAX {
                    return Err(format!("目录超过 {DEPTH_MAX} 层"));
                }
                pending.push((entry.path(), format!("{path}/"), depth + 1));
            } else if meta.is_file() {
                if meta.len() > max_size {
                    return Err(format!("「{path}」超过了单个文件的大小上限"));
                }
                if out.len() >= FILES_MAX {
                    return Err(format!("文件超过 {FILES_MAX} 个"));
                }
                out.push(LocalFile {
                    path,
                    abs: entry.path(),
                    size: meta.len(),
                    mtime: mtime_ms(&meta),
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    // 只差大小写的两个名字在 Windows 和 macOS 上是同一个文件。
    let mut seen = BTreeSet::new();
    for file in &out {
        if !seen.insert(fold(&file.path)) {
            return Err(format!("「{}」与另一个文件只差大小写", file.path));
        }
        // 契约文件的名字必须一字不差：只差大小写的，在有的系统上就是契约文件本身（会被当成
        // 技能说明、入口声明来读），在别的系统上只是个普通文件——各设备上不是同一个技能。
        if let Some(contract) = root_contract(&file.path).filter(|contract| *contract != file.path)
        {
            return Err(format!(
                "「{}」与 {contract} 只差大小写，请把文件名改成 {contract}",
                file.path
            ));
        }
    }
    for name in ROOT_TEXT {
        if out
            .iter()
            .any(|file| file.path == name && file.size > INLINE_MAX)
        {
            return Err(format!("{name} 超过 {} KB", INLINE_MAX / 1024));
        }
    }
    if !out.iter().any(|file| file.path == "SKILL.md") {
        return Err("缺少 SKILL.md".into());
    }
    Ok(out)
}

/// 一个文件的 `(sha256, 长度, 开头几个字节)`。
fn digest_file(path: &Path) -> Result<(String, u64, Vec<u8>)> {
    let mut file = std::fs::File::open(path)?;
    let mut head = vec![0u8; 4];
    let mut filled = 0usize;
    while filled < head.len() {
        let read = file.read(&mut head[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    head.truncate(filled);
    let mut whole = std::io::Cursor::new(head.clone()).chain(file);
    let (digest, len) = sha256_reader(&mut whole)?;
    Ok((ids::hex(&digest), len, head))
}

/// 给列出来的文件算摘要。长度和修改时间都没变的文件用上次算的。
fn hash_local(
    cache: &mut Cache,
    name: &str,
    files: Vec<LocalFile>,
) -> std::result::Result<Local, Unsynced> {
    let mut hashed = Vec::with_capacity(files.len());
    for file in files {
        let key = format!("{name}/{}", file.path);
        let (sha256, runnable) = match cache.hash_of(&key, file.size, file.mtime) {
            Some(known) => known,
            None => {
                let (sha256, len, head) = digest_file(&file.abs).map_err(|_| Unsynced::Busy)?;
                if len != file.size {
                    return Err(Unsynced::Busy);
                }
                let runnable = is_executable(&file.path, Some(head.as_slice()));
                cache.put_hash(key, file.size, file.mtime, sha256.clone(), runnable);
                (sha256, runnable)
            }
        };
        hashed.push(Hashed {
            file,
            sha256,
            runnable,
        });
    }
    let hash = version_hash(
        hashed
            .iter()
            .map(|item| (item.file.path.as_str(), item.sha256.as_str())),
    );
    Ok(Local {
        files: hashed,
        hash,
    })
}

/// 把本机的版本做成一条 `skill.put`，连同要上传的附件对象。
fn build_put(
    keys: &Keys,
    name: &str,
    local: &Local,
) -> std::result::Result<(Op, Vec<LocalBlob>), Unsynced> {
    // 写进模型上下文的几份文件先占内联的预算；其余按路径顺序，放不下的走附件对象。
    let mut order: Vec<&Hashed> = local.files.iter().collect();
    order.sort_by_key(|item| !ROOT_TEXT.contains(&item.file.path.as_str()));
    let mut budget = INLINE_TOTAL;
    let mut files: BTreeMap<&str, SkillFile> = BTreeMap::new();
    let mut blobs = Vec::new();
    for item in order {
        let file = &item.file;
        let root_text = ROOT_TEXT.contains(&file.path.as_str());
        let mut entry = SkillFile {
            path: file.path.clone(),
            size: file.size,
            sha256: item.sha256.clone(),
            exec: item.runnable,
            text: None,
            b64: None,
            blob: None,
        };
        if file.size <= INLINE_MAX && (root_text || file.size * 4 / 3 + 4 <= budget) {
            let bytes = std::fs::read(&file.abs).map_err(|_| Unsynced::Busy)?;
            if bytes.len() as u64 != file.size || sha256_hex(&bytes) != item.sha256 {
                return Err(Unsynced::Busy);
            }
            match String::from_utf8(bytes) {
                Ok(text) => {
                    if root_text && has_hidden_chars(&text) {
                        return Err(Unsynced::Skip(format!(
                            "{} 里有不可见的控制字符",
                            file.path
                        )));
                    }
                    budget = budget.saturating_sub(text.len() as u64);
                    entry.text = Some(text);
                }
                Err(_) if root_text => {
                    return Err(Unsynced::Skip(format!("{} 不是 UTF-8 文本", file.path)));
                }
                Err(error) => {
                    let encoded = STANDARD.encode(error.into_bytes());
                    budget = budget.saturating_sub(encoded.len() as u64);
                    entry.b64 = Some(encoded);
                }
            }
        } else {
            let digest = ids::unhex(&item.sha256).ok_or(Unsynced::Busy)?;
            let blob_id = keys.blob_id(&digest);
            blobs.push(LocalBlob {
                blob_id: blob_id.clone(),
                rel_path: format!("skills/{name}/{}", file.path),
                size: file.size,
            });
            entry.blob = Some(blob_id);
        }
        files.insert(file.path.as_str(), entry);
    }
    let op = Op::SkillPut {
        name: name.to_string(),
        hash: local.hash.clone(),
        files: files.into_values().collect(),
    };
    Ok((op, blobs))
}

/// 内联文件的内容。
fn inline_bytes(file: &SkillFile) -> Option<Vec<u8>> {
    match (&file.text, &file.b64) {
        (Some(text), _) => Some(text.as_bytes().to_vec()),
        (None, Some(encoded)) => STANDARD.decode(encoded).ok(),
        (None, None) => None,
    }
}

/// 清单里的一个文件算不算可执行内容：发布端的标注，或者本机按路径和内联内容看出来的。
/// 大文件的内容这时还看不到，取回之后再按内容补判一次。
fn flagged(file: &SkillFile) -> bool {
    file.exec || is_executable(&file.path, inline_bytes(file).as_deref())
}

/// 远端来的一份文件清单在本机能不能用。不能用的原因给用户看。`max_size` 是本机设置的
/// 附件大小上限：比它大的文件取不回来（[`crate::attach::download`]），这个版本就装不上。
fn check_version(
    keys: &Keys,
    files: &[SkillFile],
    max_size: u64,
) -> std::result::Result<(), String> {
    if files.is_empty() || files.len() > FILES_MAX {
        return Err(format!("文件数不在 1 到 {FILES_MAX} 之间"));
    }
    let shown = |path: &str| path.chars().take(80).collect::<String>();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for file in files {
        if !is_file_path(&file.path) {
            return Err(format!("文件路径「{}」不合规", shown(&file.path)));
        }
        if file.size > max_size {
            return Err(format!(
                "「{}」超过了本机设置的附件大小上限（{} MB）",
                file.path,
                max_size / (1024 * 1024)
            ));
        }
        if !seen.insert(fold(&file.path)) {
            return Err(format!("「{}」与另一个文件重名", file.path));
        }
        if !ids::is_digest(&file.sha256) {
            return Err(format!("「{}」的摘要不合规", file.path));
        }
        match (&file.text, &file.b64, &file.blob) {
            (Some(_), None, None) | (None, Some(_), None) => {
                let Some(bytes) = inline_bytes(file) else {
                    return Err(format!("「{}」的内容解不出来", file.path));
                };
                if bytes.len() as u64 != file.size || sha256_hex(&bytes) != file.sha256 {
                    return Err(format!("「{}」的内容与摘要不符", file.path));
                }
            }
            (None, None, Some(blob)) => {
                let expected = ids::unhex(&file.sha256).map(|digest| keys.blob_id(&digest));
                if expected.as_deref() != Some(blob.as_str()) {
                    return Err(format!("「{}」的附件对象与摘要不符", file.path));
                }
            }
            _ => return Err(format!("「{}」没有内容，或有不止一份", file.path)),
        }
        if let Some(contract) = root_contract(&file.path) {
            // 只差大小写的名字，在 Windows 和 macOS 上打开的就是契约文件本身：加载器会把它
            // 当成入口声明、技能说明来读，而它既不算可执行内容、也没过下面这几项检查。
            if file.path != contract {
                return Err(format!("「{}」与 {contract} 只差大小写", shown(&file.path)));
            }
            match &file.text {
                Some(text) if !has_hidden_chars(text) => {}
                Some(_) => return Err(format!("{} 里有不可见的控制字符", file.path)),
                None => return Err(format!("{} 不是内联的文本", file.path)),
            }
        }
    }
    // 一个名字不能既是文件又是目录。
    for path in &seen {
        let mut at = path.as_str();
        while let Some((parent, _)) = at.rsplit_once('/') {
            if seen.contains(parent) {
                return Err(format!("「{parent}」既是文件又是目录"));
            }
            at = parent;
        }
    }
    if !files.iter().any(|file| file.path == "SKILL.md") {
        return Err("缺少 SKILL.md".into());
    }
    Ok(())
}

/// 在备料目录里新建一个文件。目录是这一次刚建的，里面不该有重名的：清单里两个名字在
/// 这个文件系统上要是指向同一个文件（Windows 的 8.3 短名 `SKILL~1.TOM` 之类，名字检查
/// 看不出来），后写的不能悄悄盖掉先写的——用户确认时看到的是先写的那一份。
fn create_staged(path: &Path) -> Result<std::fs::File> {
    Ok(std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?)
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create_staged(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// 把 `source` 复制到 `target` 并落盘，返回 `target` 的 `(sha256, 长度, 开头几个字节)`。
/// `source` 打不开时返回 `None`。
fn copy_synced(source: &Path, target: &Path) -> Result<Option<(String, u64, Vec<u8>)>> {
    let Ok(mut reader) = std::fs::File::open(source) else {
        return Ok(None);
    };
    let mut writer = create_staged(target)?;
    std::io::copy(&mut reader, &mut writer)?;
    writer.sync_all()?;
    drop(writer);
    digest_file(target).map(Some)
}

/// 备料用的临时目录，离开作用域时删掉（装好之后它已经被宿主移走，删不到也无妨）。
struct TmpDir(PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        // 备料目录在用户的技能目录里：空了就不留着。
        if let Some(root) = self.0.parent() {
            let _ = std::fs::remove_dir(root);
        }
    }
}

/// 在宿主给的备料目录（和技能目录在同一个卷上）下给一个要装的版本起一个目录名。目录
/// 本身随第一个文件建出来。
fn new_staging(root: &Path, name: &str) -> Result<TmpDir> {
    std::fs::create_dir_all(root)?;
    if is_redirect(&std::fs::symlink_metadata(root)?) {
        return Err(SyncError::other(
            "技能目录里的备料目录是符号链接，同步不往里写",
        ));
    }
    Ok(TmpDir(
        root.join(format!("{name}-{}", ids::random_hex::<4>())),
    ))
}

enum Staged {
    Ready {
        dir: TmpDir,
        /// 不是内联来的文件：`(清单里的序号, 开头几个字节)`，用来补判是不是可执行内容。
        heads: Vec<(usize, Vec<u8>)>,
        /// 从附件对象取来的文件：`(对象名, 装好之后相对 agent 根的路径)`。
        fetched: Vec<(String, String)>,
    },
    /// 还缺这些附件对象。
    Want(Vec<String>),
    /// 本机的文件在复制的时候变了。
    Busy,
}

/// 在备料目录 `root` 里把远端的版本备齐：内联的写出来，大文件从本机现有的版本或取回的
/// 附件对象复制过来，每个文件都核对摘要并落盘。清单已经过 [`check_version`]。
fn stage(
    ctx: &Ctx,
    root: &Path,
    name: &str,
    files: &[SkillFile],
    local: Option<&Local>,
) -> Result<Staged> {
    // 本机现有版本里内容相同的文件直接拿来用，不必再取一遍。
    let have: HashMap<&str, &Path> = local
        .map(|local| {
            local
                .files
                .iter()
                .map(|item| (item.sha256.as_str(), item.file.abs.as_path()))
                .collect()
        })
        .unwrap_or_default();
    let remote: Vec<(&SkillFile, &str)> = files
        .iter()
        .filter(|file| !have.contains_key(file.sha256.as_str()))
        .filter_map(|file| Some((file, file.blob.as_deref()?)))
        .collect();
    let mut sources: HashMap<&str, PathBuf> = HashMap::new();
    if !remote.is_empty() {
        let found = db::write(ctx.db(), |tx| {
            let mut found = Vec::with_capacity(remote.len());
            for (file, blob) in &remote {
                let landing = wire::cache_path(&file.path, blob);
                wire::expect_blob(tx, &ctx.env, blob, &landing, file.size)?;
                found.push(db::blob_get(tx, blob)?);
            }
            Ok(found)
        })?;
        let mut missing: Vec<String> = Vec::new();
        for ((_, blob), row) in remote.iter().zip(found) {
            match row.filter(|row| row.state != BLOB_REMOTE) {
                Some(row) => {
                    sources.insert(*blob, ctx.env.checked_path(&row.rel_path)?);
                }
                None => missing.push(blob.to_string()),
            }
        }
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            return Ok(Staged::Want(missing));
        }
    }

    let dir = new_staging(root, name)?;
    let mut heads = Vec::new();
    let mut fetched = Vec::new();
    let mut stale: Vec<(String, String)> = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let mut target = dir.0.clone();
        target.extend(file.path.split('/'));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(bytes) = inline_bytes(file) {
            write_synced(&target, &bytes)?;
            continue;
        }
        let blob = file.blob.as_deref().unwrap_or_default();
        let (source, from_local) = match have.get(file.sha256.as_str()) {
            Some(path) => (path.to_path_buf(), true),
            None => match sources.get(blob) {
                Some(path) => (path.clone(), false),
                None => return Err(SyncError::other("技能文件没有内容来源")),
            },
        };
        let copied = copy_synced(&source, &target)?
            .filter(|(sha256, len, _)| *sha256 == file.sha256 && *len == file.size);
        match (copied, from_local) {
            (Some((_, _, head)), _) => {
                heads.push((index, head));
                if !from_local {
                    fetched.push((blob.to_string(), format!("skills/{name}/{}", file.path)));
                }
            }
            (None, true) => return Ok(Staged::Busy),
            (None, false) => stale.push((blob.to_string(), wire::cache_path(&file.path, blob))),
        }
    }
    if !stale.is_empty() {
        // 登记说本机有的那一份对不上了（文件后来被改过、被删了）：改回「等待取回」。
        db::write(ctx.db(), |tx| {
            for (blob, landing) in &stale {
                db::blob_set(tx, blob, landing, BLOB_REMOTE)?;
            }
            Ok(())
        })?;
        return Ok(Staged::Want(
            stale.into_iter().map(|(blob, _)| blob).collect(),
        ));
    }
    Ok(Staged::Ready {
        dir,
        heads,
        fetched,
    })
}

/// 把本机的版本留一个副本：`<数据根>/sync/conflicts/skills/<名字>.<版本摘要的前 12 位>/`。
/// 目录名跟着内容走：这一轮没换成、下一轮再来的时候，同一个版本不会又复制一份。要把
/// 每个文件都复制一遍，所以不在静默门里调。留不下只记日志。
fn keep_version(ctx: &Ctx, name: &str, local: &Local) {
    let root = ctx.paths.conflicts.join("skills");
    let short = local.hash.get(..12).unwrap_or(&local.hash);
    let dir = root.join(format!("{name}.{short}"));
    if dir.is_dir() {
        return;
    }
    // 先复制到旁边，齐了再改名：留下来的副本要么没有，要么是完整的。
    let partial = root.join(format!("{name}.{short}.partial"));
    let _ = std::fs::remove_dir_all(&partial);
    let copy = || -> std::io::Result<()> {
        for item in &local.files {
            let mut target = partial.clone();
            target.extend(item.file.path.split('/'));
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&item.file.abs, &target)?;
        }
        std::fs::rename(&partial, &dir)
    };
    if let Err(error) = copy() {
        tracing::warn!("保留技能「{name}」的冲突副本失败: {error}");
        let _ = std::fs::remove_dir_all(&partial);
    }
}

/// 本机的版本从列清单到现在动过没有（只看有哪些文件、各自的长度和修改时间）。
fn unchanged(dir: &Path, local: &Local, max_size: u64) -> bool {
    list_skill(dir, max_size).is_ok_and(|files| {
        files.len() == local.files.len()
            && files.iter().zip(&local.files).all(|(now, then)| {
                now.path == then.file.path
                    && now.size == then.file.size
                    && now.mtime == then.file.mtime
            })
    })
}

fn set_base(conn: &Connection, name: &str, base: &Option<String>) -> Result<()> {
    match base {
        Some(hash) => db::state_put(conn, SCOPE_BASE, name, db::HLC_ZERO, None, Some(hash)),
        None => db::state_del(conn, SCOPE_BASE, name),
    }
}

/// 从别的设备同步来、现在装在本机的技能：`(技能名, 来源设备的展示名)`（§8.4）。本机自己
/// 的设备身份（包括换身份之前用过的）不算来源。
pub(crate) fn origins(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mine = rclaw_store::sync_db::device_ids(conn)?;
    let mut out = Vec::new();
    for (name, row) in db::state_scan(conn, SCOPE_ORIGIN, "")? {
        let Some(device) = row.value.filter(|device| !mine.contains(device)) else {
            continue;
        };
        out.push((name, db::device_label(conn, &device)?));
    }
    Ok(out)
}

/// 登记一个本机有、要传上去的附件对象（技能里的大文件；不看「同步附件」开没开）。登记
/// 一律指向技能目录里的这一份：它的内容刚核对过，就是这个对象。排进队列时给对象记上
/// 「技能依赖」这个上传原因：会话附件的范围收窄不能拦住它，不然接收端装不上同一个技能。
fn register_upload(conn: &Connection, blob: &LocalBlob) -> Result<()> {
    let Some(found) = db::blob_get(conn, &blob.blob_id)? else {
        db::blob_register(conn, &blob.blob_id, &blob.rel_path, blob.size, BLOB_PENDING)?;
        crate::attach::mark_skill_blob(conn, &blob.blob_id)?;
        return Ok(());
    };
    if found.state == BLOB_REMOTE
        && found.rel_path != blob.rel_path
        && !found.rel_path.starts_with("sync/files/")
    {
        // 别处还等着这个对象落过去（消息里的附件）：记下来，稍后从技能目录复制一份。
        db::state_put(
            conn,
            wire::SCOPE_BLOB_ALIAS,
            &format!("{}|{}", blob.blob_id, found.rel_path),
            db::HLC_ZERO,
            None,
            None,
        )?;
    }
    if found.state == BLOB_REMOTE {
        // 本机自己有这份内容了：不用再等别的设备传。
        crate::attach::forget(conn, &blob.blob_id)?;
    }
    // 远端已经有的不用再传；其余排进上传队列，记上「技能依赖」。
    let state = match found.state {
        BLOB_UPLOADED | BLOB_FETCHED => found.state,
        _ => BLOB_PENDING,
    };
    if state == BLOB_PENDING {
        crate::attach::mark_skill_blob(conn, &blob.blob_id)?;
    }
    db::blob_set(conn, &blob.blob_id, &blob.rel_path, state)
}

/// 寄存器、基线和各种记号的当前状态。
struct State {
    /// 寄存器里各技能的 `(hlc, 处理标记)`。不带内容：清单可能有几兆字节，用到哪条再读
    /// 哪条。
    marks: BTreeMap<String, (String, Option<String>)>,
    bases: BTreeMap<String, String>,
    rejected: BTreeMap<String, String>,
    approved: BTreeMap<String, String>,
}

impl State {
    fn load(conn: &Connection) -> Result<State> {
        let values = |scope: &str| -> Result<BTreeMap<String, String>> {
            Ok(db::state_scan(conn, scope, "")?
                .into_iter()
                .filter_map(|(key, row)| Some((key, row.value?)))
                .collect())
        };
        let marks = {
            let mut stmt =
                conn.prepare_cached("SELECT key, hlc, hash FROM sync_state WHERE scope = ?1")?;
            let found = stmt.query_map(params![scope::SKILL], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?),
                ))
            })?;
            found.collect::<rusqlite::Result<BTreeMap<_, _>>>()?
        };
        Ok(State {
            marks,
            bases: values(SCOPE_BASE)?,
            rejected: values(SCOPE_REJ)?,
            approved: values(SCOPE_OK)?,
        })
    }
}

/// 一条还没落到本机的远端版本。
struct Incoming<'a> {
    row: &'a Row,
    /// 刚到、还没看过（否则是看过了、在等确认或等条件）。
    is_new: bool,
    /// 远端的文件清单；`None` = 远端把这个技能删了。
    files: Option<&'a [SkillFile]>,
    /// 这个版本的摘要（删除记为 [`REJ_DEL`]）：拒绝和确认的记号都按它记。
    key: &'a str,
    /// 这个技能在本机是固定的。
    pinned: bool,
    /// 这个技能在本机还没同步过（刚加入同步空间的设备）。
    first: bool,
}

/// 寄存器里一条还没落地的记录：`(hlc, 是不是刚到)`。
type Pending = Option<(String, bool)>;

struct Run<'a> {
    ctx: &'a Ctx,
    s: &'a Settings,
    keys: &'a Keys,
    host: Arc<dyn SkillHost>,
    cache: &'a mut Cache,
    report: &'a mut FilesReport,
    state: State,
    /// 有技能这一轮没处理完，或者技能目录刚被这里改过：这一轮的指纹不留。
    unsettled: bool,
}

/// 「换不上去、等着再试」在 [`Cache`] 里的键。
fn retry_key(name: &str) -> String {
    format!("skill/{name}")
}

impl Run<'_> {
    /// 这一轮做不成，过一会儿再来。
    fn later(&mut self) {
        self.report.retry = true;
        self.unsettled = true;
    }

    fn drop_item(&mut self, name: &str) {
        self.report.status_changed |= status::remove(&self.ctx.paths, &status::item_id(KIND, name));
    }

    /// 这个技能没有等着重试的事了：忘掉失败的记录，撤掉提示。
    fn calm(&mut self, name: &str) {
        self.cache.settled(&retry_key(name));
        self.report.status_changed |= status::clear_notice(&self.ctx.paths, NOTICE_RETRY, name);
    }

    /// 上次换不上去，还没到再试的时候。
    fn backing_off(&mut self, name: &str) -> bool {
        if !self
            .cache
            .backing_off(&retry_key(name), crate::attach::now_ms())
        {
            return false;
        }
        // 这一轮的指纹不留，之后每一轮都再来看一眼到没到时候；但不催着马上再来。
        self.unsettled = true;
        true
    }

    /// 远端的版本没问题，只是这一次没换上去（归档改名失败、文件被占着、盘满……）：留在
    /// 「待落地」，隔一阵再试，间隔逐次加长。连着失败才提示，说的是真实的原因。
    fn failed(&mut self, name: &str, row: &Row, removing: bool, reason: &str) -> Result<()> {
        let attempts = self.cache.failed(&retry_key(name), crate::attach::now_ms());
        tracing::warn!("技能「{name}」的远端版本没换上去（第 {attempts} 次）: {reason}");
        self.unsettled = true;
        if attempts >= RETRY_NOTICE_AFTER {
            let label = db::read(self.ctx.db(), |conn| db::device_label(conn, &row.origin))?;
            let title = match removing {
                true => format!("技能「{name}」还没从本机移走"),
                false => format!("技能「{name}」的新版本还没装上"),
            };
            let id = status::item_id(NOTICE_RETRY, name);
            if status::get(&self.ctx.paths, &id).is_none() {
                status::notice(
                    &self.ctx.paths,
                    NOTICE_RETRY,
                    name,
                    &title,
                    &format!(
                        "「{label}」的这次改动往技能目录里落的时候出了错（{reason}）。同步会隔一阵\
                         自动再试，成功之后这条提示会自己消失。"
                    ),
                );
                self.report.status_changed = true;
            }
        }
        Ok(())
    }

    /// 远端版本里有大文件取不回来了（[`crate::attach::Lost`]）：这个版本装不上，也不再
    /// 去取。那台设备再发布一版（哪怕引用的还是同样内容的文件）之后会重新去取。
    ///
    /// 刚听说取不回来的时候先不提示：多半是那台设备上这个技能又改了（所以旧版本的文件
    /// 没了），新版本马上就到。
    fn stuck(&mut self, name: &str, row: &Row, lost: &crate::attach::Lost) -> Result<()> {
        let since = crate::attach::now_ms() - lost.at;
        if (0..STUCK_NOTICE_AFTER_MS).contains(&since) {
            return Ok(());
        }
        let id = status::item_id(NOTICE_RETRY, name);
        if status::get(&self.ctx.paths, &id).is_some() {
            return Ok(());
        }
        let label = db::read(self.ctx.db(), |conn| db::device_label(conn, &row.origin))?;
        let why = match lost.gone {
            true => "这个版本里有文件在那台设备上已经被删除或者改动过",
            false => "这个版本里有文件一直没有传到同步空间",
        };
        status::notice(
            &self.ctx.paths,
            NOTICE_RETRY,
            name,
            &format!("技能「{name}」的新版本还没装上"),
            &format!(
                "「{label}」发布的这个版本本机装不上：{why}，取不回来。在那台设备上再改动一次\
                 这个技能，新的版本会重新同步过来；装上之后这条提示会自己消失。"
            ),
        );
        self.report.status_changed = true;
        Ok(())
    }

    /// 这条远端版本现在落不了（本机的技能停用了，或者本机这份同步不了）：记成「看过了」，
    /// 等本机的文件有变化时再看，不必每一轮都来读它。
    fn park(&mut self, name: &str, pending: &Pending) -> Result<()> {
        if let Some((hlc, true)) = pending {
            db::write(self.ctx.db(), |tx| {
                mark(tx, scope::SKILL, name, hlc, AWAITING)
            })?;
        }
        Ok(())
    }

    /// 本机的这个技能同步不了：提示原因，远端的版本先放着。
    fn skip(&mut self, name: &str, pending: &Pending, reason: &str) -> Result<()> {
        let id = status::item_id(NOTICE_SKIPPED, name);
        if status::get(&self.ctx.paths, &id).is_none() {
            status::notice(
                &self.ctx.paths,
                NOTICE_SKIPPED,
                name,
                &format!("技能「{name}」没有同步"),
                &format!("{reason}。改好之后会自动同步。"),
            );
            self.report.status_changed = true;
        }
        self.park(name, pending)
    }

    fn one(
        &mut self,
        name: &str,
        listing: Option<std::result::Result<Vec<LocalFile>, String>>,
    ) -> Result<()> {
        let kind = self.host.kind(name);
        let pending: Pending =
            self.state
                .marks
                .get(name)
                .and_then(|(hlc, mark)| match mark.as_deref() {
                    None => Some((hlc.clone(), true)),
                    Some(AWAITING) => Some((hlc.clone(), false)),
                    Some(_) => None,
                });
        let base = self.state.bases.get(name).cloned();
        match kind {
            SkillKind::Builtin => {
                // 这个名字在本机是内置技能：别的设备上的同名技能不落到本机，本机的也不发布。
                if pending.is_some() || base.is_some() {
                    db::write(self.ctx.db(), |tx| {
                        if let Some((hlc, _)) = &pending {
                            mark(tx, scope::SKILL, name, hlc, HANDLED)?;
                        }
                        db::state_del(tx, SCOPE_ORIGIN, name)?;
                        db::state_del(tx, SCOPE_BASE, name)
                    })?;
                    self.drop_item(name);
                }
                return Ok(());
            }
            // 本机把它停用了：同步不动它，也不把停用当成删除发出去；恢复之后再继续。
            SkillKind::Archived => return self.park(name, &pending),
            SkillKind::Live { .. } | SkillKind::Absent => {}
        }
        let pinned = matches!(kind, SkillKind::Live { pinned: true });
        let local = match (listing, kind) {
            (Some(Ok(files)), _) => match hash_local(self.cache, name, files) {
                Ok(local) => Some(local),
                Err(Unsynced::Busy) => {
                    self.later();
                    return Ok(());
                }
                Err(Unsynced::Skip(reason)) => return self.skip(name, &pending, &reason),
            },
            (Some(Err(reason)), _) => return self.skip(name, &pending, &reason),
            (None, SkillKind::Absent) => None,
            // 列完技能名之后它才出现：下一轮再看。
            (None, _) => {
                self.later();
                return Ok(());
            }
        };
        let local_hash = local.as_ref().map(|local| local.hash.clone());
        let changed = local_hash != base;
        let Some((hlc, is_new)) = pending.clone() else {
            // 没有待落地的远端版本：本机有变化就发布（没同步过的存量也在这里发）。
            if changed {
                self.publish_local(name, local.as_ref(), base.as_deref(), &pending)?;
            } else if local.is_some() {
                self.report.status_changed |=
                    status::clear_notice(&self.ctx.paths, NOTICE_SKIPPED, name);
            }
            return Ok(());
        };
        // 清单的内容在库锁之外解析。
        let raw = db::read(self.ctx.db(), |conn| {
            db::state_get(conn, scope::SKILL, name)
        })?;
        let Some(row) = raw
            .and_then(|raw| to_row(name.to_string(), raw))
            .filter(|row| row.hlc == hlc)
        else {
            return Ok(());
        };
        let remote = match &row.op {
            Op::SkillPut { files, .. } => Some(files.as_slice()),
            Op::SkillDel { .. } => None,
            _ => return Ok(()),
        };
        let remote_hash = remote.map(files_hash);
        let remote_key = remote_hash.clone().unwrap_or_else(|| REJ_DEL.to_string());
        let local_newer = match &base {
            // 上次同步之后本机也改过：本机的更新。
            Some(_) => changed,
            // 从没同步过的本机技能遇上一条早先的删除记录：那条删除说的不是它。
            None => remote.is_none() && local.is_some(),
        };
        if remote_hash == local_hash {
            return self.settle(name, &row, Some(local_hash));
        }
        if local_newer {
            return self.publish_local(name, local.as_ref(), base.as_deref(), &pending);
        }
        if self.state.rejected.get(name) == Some(&remote_key) {
            // 用户拒绝过的那一版又来了：不再问。
            let base = base.is_none().then_some(local_hash);
            return self.settle(name, &row, base);
        }
        let incoming = Incoming {
            row: &row,
            is_new,
            files: remote,
            key: &remote_key,
            pinned,
            first: base.is_none(),
        };
        self.apply(name, &incoming, local.as_ref())
    }

    /// 远端版本与本机相同，或是用户拒绝过的那一版：记为已处理。`base` 是要记下的基线
    /// （`None` = 不动基线）。
    fn settle(&mut self, name: &str, row: &Row, base: Option<Option<String>>) -> Result<()> {
        db::write(self.ctx.db(), |tx| {
            mark(tx, scope::SKILL, name, &row.hlc, HANDLED)?;
            if let Some(base) = &base {
                set_base(tx, name, base)?;
            }
            db::state_del(tx, SCOPE_OK, name)
        })?;
        self.calm(name);
        self.drop_item(name);
        Ok(())
    }

    /// 把本机的版本（或本机的删除）发布出去。
    fn publish_local(
        &mut self,
        name: &str,
        local: Option<&Local>,
        base: Option<&str>,
        pending: &Pending,
    ) -> Result<()> {
        let (op, blobs) = match (local, base) {
            (Some(local), _) => match build_put(self.keys, name, local) {
                Ok(built) => built,
                Err(Unsynced::Busy) => {
                    self.later();
                    return Ok(());
                }
                Err(Unsynced::Skip(reason)) => return self.skip(name, pending, &reason),
            },
            (None, Some(base)) => {
                let op = Op::SkillDel {
                    name: name.to_string(),
                    hash: base.to_string(),
                };
                (op, Vec::new())
            }
            (None, None) => return Ok(()),
        };
        let s = self.s;
        let new_base = local.map(|local| local.hash.clone());
        // 清单可能有几兆字节：在库锁之外序列化好，锁里只是把文本写进去。
        let prepared = prepare(s, &op)?;
        drop(op);
        db::write(self.ctx.db(), |tx| {
            for blob in &blobs {
                register_upload(tx, blob)?;
            }
            publish_prepared(tx, s, scope::SKILL, name, &prepared)?;
            set_base(tx, name, &new_base)?;
            // 现在的版本是本机自己的（或者本机把它删了）：不再算从别的设备来的。
            db::state_del(tx, SCOPE_ORIGIN, name)?;
            db::state_del(tx, SCOPE_OK, name)
        })?;
        self.report.published += 1;
        self.report.status_changed |= status::clear_notice(&self.ctx.paths, NOTICE_SKIPPED, name);
        // 本机的版本压过了那条没换上去的远端版本：不用再试了。
        self.calm(name);
        self.drop_item(name);
        Ok(())
    }

    /// 把远端的版本落到本机：要确认的先问，要取的大文件先取，然后在没有 turn 运行的
    /// 时候换上去。
    fn apply(&mut self, name: &str, incoming: &Incoming<'_>, local: Option<&Local>) -> Result<()> {
        let ctx = self.ctx;
        let host = self.host.clone();
        let approved = self.state.approved.get(name).map(String::as_str) == Some(incoming.key);
        let live = ctx.layout.skills_dir.join(name);
        let max_size = self.s.attachment_max;
        // 占门之前再看一眼：列完清单之后本机的版本又被改过的话，这一轮不换——换了，那些
        // 改动就跟着旧版本进了归档，再也发布不出去。下一轮它们算本机的新版本。
        let untouched =
            |local: Option<&Local>| local.is_none_or(|local| unchanged(&live, local, max_size));
        let Some(files) = incoming.files else {
            // 别的设备删了这个技能：本机的移进归档。
            if incoming.pinned && !approved {
                return self.ask(name, incoming, local.is_some(), &[]);
            }
            if self.backing_off(name) {
                return Ok(());
            }
            if !untouched(local) {
                self.later();
                return Ok(());
            }
            let mut removed = None;
            ctx.gate.when_quiet(&mut || {
                if !host.in_use() {
                    removed = Some(host.remove(name));
                }
            });
            return match removed {
                Some(Ok(_)) => self.applied(name, incoming.row, None, &[]),
                Some(Err(reason)) => self.failed(name, incoming.row, true, &reason),
                // 有 turn 在跑，或者后台任务正在用技能：下一轮再来。
                None => {
                    self.later();
                    Ok(())
                }
            };
        };
        if let Err(reason) = check_version(self.keys, files, max_size) {
            return self.reject(name, incoming.row, &reason);
        }
        // 这个版本带来的新内容（本机同一路径上没有同样内容的文件）里有没有可执行的东西。
        let have: HashMap<&str, &str> = local
            .map(|local| {
                local
                    .files
                    .iter()
                    .map(|item| (item.file.path.as_str(), item.sha256.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let fresh = |file: &SkillFile| have.get(file.path.as_str()) != Some(&file.sha256.as_str());
        let mut runnable: Vec<String> = files
            .iter()
            .filter(|file| fresh(file) && flagged(file))
            .map(|file| file.path.clone())
            .collect();
        if (incoming.pinned || !runnable.is_empty()) && !approved {
            return self.ask(name, incoming, local.is_some(), &runnable);
        }
        if self.backing_off(name) {
            return Ok(());
        }
        if !ctx.gate.when_quiet(&mut || {}) {
            // 有 turn 在跑：不白备一遍料，下一轮再来。
            self.later();
            return Ok(());
        }
        let staged = match stage(ctx, &host.staging_dir(), name, files, local) {
            Ok(staged) => staged,
            // 备料目录是新建的，里面却已经有同名的文件：清单里有两个名字在本机的文件系统
            // 上指向同一个文件（名字检查看不出来的那种，比如 Windows 的 8.3 短名）。
            Err(SyncError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return self.reject(
                    name,
                    incoming.row,
                    "清单里有两个文件名在这台设备的文件系统上指向同一个文件",
                );
            }
            Err(error) => return Err(error),
        };
        let (dir, fetched) = match staged {
            Staged::Want(blobs) => {
                self.unsettled = true;
                // 要取的大文件里有已经收了场的（等了很久没有设备传上来，或者发布它的设备
                // 说拿不出来了，§8.7）：这个版本装不上，不再去取。
                let lost = db::read(ctx.db(), |conn| crate::attach::lost_among(conn, &blobs))?;
                if let Some(lost) = lost {
                    return self.stuck(name, incoming.row, &lost);
                }
                self.report.want_blobs.extend(blobs);
                return Ok(());
            }
            Staged::Busy => {
                self.later();
                return Ok(());
            }
            Staged::Ready {
                dir,
                heads,
                fetched,
            } => {
                // 大文件的内容现在看得到了：按内容补判一次。
                for (index, head) in &heads {
                    let file = &files[*index];
                    if fresh(file)
                        && is_executable(&file.path, Some(head.as_slice()))
                        && !runnable.contains(&file.path)
                    {
                        runnable.push(file.path.clone());
                    }
                }
                if !runnable.is_empty() && !approved {
                    return self.ask(name, incoming, local.is_some(), &runnable);
                }
                (dir, fetched)
            }
        };
        // 刚加入的设备上与空间里同名的技能：以空间里的为准，本机原来的留一个副本。副本在
        // 占门之前留好——持着门的时候只改名，不复制。
        if let Some(local) = local.filter(|_| incoming.first) {
            keep_version(ctx, name, local);
        }
        if !untouched(local) {
            self.later();
            return Ok(());
        }
        let mut installed = None;
        ctx.gate.when_quiet(&mut || {
            if !host.in_use() {
                installed = Some(host.install(name, &dir.0));
            }
        });
        match installed {
            Some(Ok(())) => {
                self.applied(name, incoming.row, Some(incoming.key.to_string()), &fetched)?;
                // 归档里被换下来的旧版本只留最近几份（要删文件，所以在门外）。
                host.prune(name);
                Ok(())
            }
            Some(Err(reason)) => self.failed(name, incoming.row, false, &reason),
            // 有 turn 在跑，或者后台任务正在用技能：下一轮再来。
            None => {
                self.later();
                Ok(())
            }
        }
    }

    /// 远端的版本已经落到本机。
    fn applied(
        &mut self,
        name: &str,
        row: &Row,
        base: Option<String>,
        fetched: &[(String, String)],
    ) -> Result<()> {
        let cached = db::write(self.ctx.db(), |tx| {
            mark(tx, scope::SKILL, name, &row.hlc, HANDLED)?;
            set_base(tx, name, &base)?;
            // 装上的是哪台设备发布的版本（落地的是删除就没有这一说）。
            match &base {
                Some(_) => db::state_put(
                    tx,
                    SCOPE_ORIGIN,
                    name,
                    db::HLC_ZERO,
                    None,
                    Some(&row.origin),
                )?,
                None => db::state_del(tx, SCOPE_ORIGIN, name)?,
            }
            db::state_del(tx, SCOPE_REJ, name)?;
            db::state_del(tx, SCOPE_OK, name)?;
            // 取回来的大文件已经装进技能目录：登记改指向那里，同步缓存里的那一份不用留。
            let mut cached = Vec::new();
            for (blob, installed) in fetched {
                if let Some(found) = db::blob_get(tx, blob)? {
                    if found.state != BLOB_REMOTE
                        && found.rel_path.starts_with(wire::SYNC_DIR_PREFIX)
                    {
                        db::blob_set(tx, blob, installed, found.state)?;
                        cached.push(found.rel_path);
                    }
                }
            }
            Ok(cached)
        })?;
        for rel_path in cached {
            let Ok(path) = self.ctx.env.checked_path(&rel_path) else {
                continue;
            };
            let _ = std::fs::remove_file(&path);
            if let Some(parent) = path.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
        self.report.applied += 1;
        self.report.reload.insert(Reload::Skills);
        self.unsettled = true;
        self.calm(name);
        self.drop_item(name);
        Ok(())
    }

    /// 远端的版本本机用不了：清单过不了检查。程序重启（多半是升级）之后会再试一次。换不
    /// 上去不走这里（[`Run::failed`]）——那不是版本的问题。
    fn reject(&mut self, name: &str, row: &Row, reason: &str) -> Result<()> {
        let label = db::write(self.ctx.db(), |tx| {
            mark(tx, scope::SKILL, name, &row.hlc, REJECTED)?;
            db::state_del(tx, SCOPE_OK, name)?;
            db::device_label(tx, &row.origin)
        })?;
        status::notice(
            &self.ctx.paths,
            "skill-rejected",
            &format!("{name}/{}", row.hlc),
            &format!("来自「{label}」的技能「{name}」没有同步到本机"),
            &format!(
                "这个版本本机用不了（{reason}）。如果那台设备上的 RClaw 版本更新，升级本机并\
                 重启之后会再试一次。"
            ),
        );
        self.calm(name);
        self.drop_item(name);
        self.report.status_changed = true;
        Ok(())
    }

    /// 要用户在本机确认。`runnable` 是这个版本带来的可执行文件。
    fn ask(
        &mut self,
        name: &str,
        incoming: &Incoming<'_>,
        has_local: bool,
        runnable: &[String],
    ) -> Result<()> {
        let id = status::item_id(KIND, name);
        if !incoming.is_new && status::get(&self.ctx.paths, &id).is_some() {
            // 已经问过了，还在等。
            return Ok(());
        }
        let row = incoming.row;
        let label = db::write(self.ctx.db(), |tx| {
            mark(tx, scope::SKILL, name, &row.hlc, AWAITING)?;
            db::device_label(tx, &row.origin)
        })?;
        let deleting = incoming.files.is_none();
        let verb = match (deleting, has_local) {
            (true, _) => "删除",
            (false, true) => "修改",
            (false, false) => "新增",
        };
        let detail = if deleting {
            "这个技能在本机是固定的，删除它要你在这台设备上确认。接受之后它会移进归档，可以恢复。"
                .to_string()
        } else if runnable.is_empty() {
            "这个技能在本机是固定的，别的设备对它的改动要你在这台设备上确认之后才会落地。"
                .to_string()
        } else {
            let mut shown = runnable
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join("、");
            if runnable.len() > 8 {
                shown.push_str(&format!(" 等 {} 个", runnable.len()));
            }
            format!(
                "这个版本带来了新的或改过的可执行文件：{shown}。含可执行文件的改动要你在这台设备上\
                 确认之后才会装进技能目录；拒绝则这台设备保持原样。"
            )
        };
        let item = PendingItem {
            id,
            kind: KIND.into(),
            title: format!("「{label}」{verb}了技能「{name}」"),
            detail,
            origin: Some(label),
            at_ms: crate::attach::now_ms(),
            actions: vec!["accept".into(), "reject".into()],
            payload: serde_json::json!({
                "name": name,
                "hash": incoming.key,
                "hlc": row.hlc,
                "exec": runnable,
                "pinned": incoming.pinned,
            }),
        };
        status::put(&self.ctx.paths, &item)?;
        self.report.status_changed = true;
        Ok(())
    }
}

pub(crate) fn reconcile(
    ctx: &Ctx,
    s: &Settings,
    keys: &Keys,
    cache: &mut Cache,
    report: &mut FilesReport,
) -> Result<()> {
    let Some(host) = ctx.skills.clone() else {
        return Ok(());
    };
    let root = &ctx.layout.skills_dir;
    if !root.is_dir() {
        // 技能目录不在（所在的盘没挂上之类）：什么都不做，更不能当成「全删了」。
        return Ok(());
    }
    // 上次换版本换到一半被打断的技能先恢复成完整的旧版本，再看本机有哪些技能。
    if host.recover() {
        report.reload.insert(Reload::Skills);
        cache.forget(CACHE_KEY);
    }
    let names: BTreeSet<String> = host
        .syncable()
        .map_err(SyncError::other)?
        .into_iter()
        .collect();
    // 各技能的文件清单，和这一轮的指纹。
    let mut listings: BTreeMap<String, std::result::Result<Vec<LocalFile>, String>> =
        BTreeMap::new();
    let mut stamp: Stamp = Vec::new();
    for name in &names {
        let listing = if is_skill_name(name) {
            list_skill(&root.join(name), s.attachment_max)
        } else {
            Err("这个技能名在别的系统上用不了".to_string())
        };
        match &listing {
            Ok(files) => stamp.extend(
                files
                    .iter()
                    .map(|file| (format!("{name}/{}", file.path), file.size, file.mtime)),
            ),
            Err(reason) => stamp.push((format!("{name}/!{reason}"), 0, 0)),
        }
        listings.insert(name.clone(), listing);
    }
    let state = db::read(ctx.db(), State::load)?;
    if !state.marks.values().any(|(_, mark)| mark.is_none()) && cache.is_current(CACHE_KEY, &stamp)
    {
        return Ok(());
    }
    let all: BTreeSet<String> = names
        .iter()
        .chain(state.marks.keys())
        .chain(state.bases.keys())
        .cloned()
        .collect();
    let mut run = Run {
        ctx,
        s,
        keys,
        host,
        cache: &mut *cache,
        report: &mut *report,
        state,
        unsettled: false,
    };
    for name in &all {
        let listing = listings.remove(name);
        if let Err(error) = run.one(name, listing) {
            tracing::warn!("同步技能「{name}」失败: {error}");
            run.unsettled = true;
        }
    }
    let unsettled = run.unsettled;
    cache.retain_hashes(|key| {
        key.split('/')
            .next()
            .is_some_and(|name| names.contains(name))
    });
    if unsettled {
        cache.forget(CACHE_KEY);
    } else {
        cache.put(CACHE_KEY, stamp);
    }
    Ok(())
}

/// 待处理事项对应的寄存器行，和那个版本的摘要；事项已经过期（来了更新的远端版本，或已经
/// 处理过）时把它清掉并报错，界面刷新后会看到最新的。
fn pending_row(ctx: &Ctx, item: &PendingItem) -> Result<(String, Row, String)> {
    let name = item.payload["name"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let raw = db::read(ctx.db(), |conn| db::state_get(conn, scope::SKILL, &name))?;
    if let Some(row) = raw.and_then(|raw| to_row(name.clone(), raw)) {
        if row.is_unapplied() && Some(row.hlc.as_str()) == item.payload["hlc"].as_str() {
            let key = match &row.op {
                Op::SkillPut { files, .. } => files_hash(files),
                _ => REJ_DEL.to_string(),
            };
            return Ok((name, row, key));
        }
    }
    status::remove(&ctx.paths, &item.id);
    Err(SyncError::invalid("这项改动已经有了更新的版本，请重新查看"))
}

/// 差异对比：这个版本相对本机现有的版本新增、修改、去掉了哪些文件。能当文本看的小文件
/// 带上两边的内容。
pub(crate) fn detail(ctx: &Ctx, item: &PendingItem) -> Result<serde_json::Value> {
    let (name, row, _) = pending_row(ctx, item)?;
    let local: BTreeMap<String, LocalFile> =
        list_skill(&ctx.layout.skills_dir.join(&name), u64::MAX)
            .unwrap_or_default()
            .into_iter()
            .map(|file| (file.path.clone(), file))
            .collect();
    let text_of = |file: &LocalFile| -> Option<String> {
        if file.size > INLINE_MAX {
            return None;
        }
        String::from_utf8(std::fs::read(&file.abs).ok()?).ok()
    };
    let (remote, deleting): (&[SkillFile], bool) = match &row.op {
        Op::SkillPut { files, .. } => (files.as_slice(), false),
        _ => (&[][..], true),
    };
    let mut files = Vec::new();
    let mut unchanged = 0usize;
    for file in remote {
        let before = local.get(&file.path);
        let same = before.is_some_and(|before| {
            before.size == file.size
                && digest_file(&before.abs).is_ok_and(|(sha256, _, _)| sha256 == file.sha256)
        });
        if same {
            unchanged += 1;
            continue;
        }
        let status = if before.is_some() {
            "modified"
        } else {
            "added"
        };
        let local_text = before.and_then(&text_of);
        files.push(serde_json::json!({
            "path": file.path,
            "status": status,
            "exec": flagged(file),
            "size": file.size,
            "local": local_text,
            "remote": file.text,
        }));
    }
    for (path, file) in &local {
        if remote.iter().any(|remote| remote.path == *path) {
            continue;
        }
        let local_text = if deleting { None } else { text_of(file) };
        files.push(serde_json::json!({
            "path": path,
            "status": "removed",
            "exec": is_executable(path, None),
            "size": file.size,
            "local": local_text,
            "remote": null,
        }));
    }
    let op = if deleting { "del" } else { "put" };
    Ok(serde_json::json!({
        "name": name,
        "op": op,
        "files": files,
        "unchanged": unchanged,
    }))
}

/// 用户在本机接受或拒绝了一个技能版本。接受只是记下「确认过了」：下一轮对比把它装上——
/// 要取的大文件、要等没有 turn 在跑，都在那里处理。
pub(crate) fn decide(
    ctx: &Ctx,
    _s: &Settings,
    item: &PendingItem,
    accept: bool,
) -> Result<Option<Reload>> {
    let (name, row, key) = pending_row(ctx, item)?;
    // 拒绝、而这个技能还没有基线（刚加入的设备）：基线记成本机现在的版本，免得反过来把
    // 本机的版本推给别的设备。之后本机再改，照常发布。
    let local_hash = if accept {
        None
    } else {
        list_skill(&ctx.layout.skills_dir.join(&name), u64::MAX)
            .ok()
            .and_then(|files| hash_local(&mut Cache::default(), &name, files).ok())
            .map(|local| local.hash)
    };
    db::write(ctx.db(), |tx| {
        if accept {
            db::state_put(tx, SCOPE_OK, &name, db::HLC_ZERO, None, Some(&key))?;
            tx.execute(
                "UPDATE sync_state SET hash = NULL WHERE scope = ?1 AND key = ?2 AND hlc = ?3",
                params![scope::SKILL, name, row.hlc],
            )?;
        } else {
            mark(tx, scope::SKILL, &name, &row.hlc, HANDLED)?;
            db::state_put(tx, SCOPE_REJ, &name, db::HLC_ZERO, None, Some(&key))?;
            db::state_del(tx, SCOPE_OK, &name)?;
            if db::state_get(tx, SCOPE_BASE, &name)?.is_none() {
                set_base(tx, &name, &local_hash)?;
            }
        }
        Ok(())
    })?;
    status::remove(&ctx.paths, &item.id);
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{self, Cache};
    use crate::testing::{self, Device};
    use crate::{NoGate, SessionGate};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// 测试用的技能目录入口：归属、固定、停用都记在内存里。
    struct Dir {
        root: PathBuf,
        builtin: Vec<String>,
        pinned: Mutex<BTreeSet<String>>,
        archived: Mutex<BTreeSet<String>>,
        /// 有后台任务正在用技能。
        busy: AtomicBool,
        /// 换版本、移走技能时报这个错（模拟文件被占着之类）。
        broken: Mutex<Option<String>>,
        /// 清理旧版本被调了几次。
        pruned: Mutex<Vec<String>>,
    }

    impl Dir {
        fn archive(&self, name: &str) -> std::result::Result<PathBuf, String> {
            let to = self
                .root
                .join(".archive")
                .join(format!("{name}-{}", ids::random_hex::<4>()));
            std::fs::create_dir_all(self.root.join(".archive")).map_err(|e| e.to_string())?;
            std::fs::rename(self.root.join(name), &to).map_err(|e| e.to_string())?;
            Ok(to)
        }
    }

    impl SkillHost for Dir {
        fn syncable(&self) -> std::result::Result<Vec<String>, String> {
            let mut names = Vec::new();
            for entry in std::fs::read_dir(&self.root).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.path().join("SKILL.md").is_file()
                    && !name.starts_with('.')
                    && !self.builtin.contains(&name)
                {
                    names.push(name);
                }
            }
            Ok(names)
        }

        fn kind(&self, name: &str) -> SkillKind {
            if self.builtin.iter().any(|builtin| builtin == name) {
                SkillKind::Builtin
            } else if self.archived.lock().unwrap().contains(name) {
                SkillKind::Archived
            } else if self.root.join(name).join("SKILL.md").is_file() {
                SkillKind::Live {
                    pinned: self.pinned.lock().unwrap().contains(name),
                }
            } else {
                SkillKind::Absent
            }
        }

        fn in_use(&self) -> bool {
            self.busy.load(Ordering::SeqCst)
        }

        fn staging_dir(&self) -> PathBuf {
            self.root.join(".sync-staging")
        }

        fn prune(&self, name: &str) {
            self.pruned.lock().unwrap().push(name.to_string());
        }

        fn install(&self, name: &str, staged: &Path) -> std::result::Result<(), String> {
            if let Some(reason) = self.broken.lock().unwrap().clone() {
                return Err(reason);
            }
            // 引擎把新版本备在备料目录里：换上去只是改名。
            assert_eq!(staged.parent(), Some(self.staging_dir().as_path()));
            let live = self.root.join(name);
            let old = match live.exists() {
                true => Some(self.archive(name)?),
                false => None,
            };
            std::fs::rename(staged, &live).map_err(|e| e.to_string())?;
            // 本机专有的文件带到新版本里（测试里只看技能根上的）。
            if let Some(old) = old {
                for entry in std::fs::read_dir(&old)
                    .map_err(|e| e.to_string())?
                    .flatten()
                {
                    let file = entry.file_name().to_string_lossy().into_owned();
                    if is_local_only(&file) {
                        std::fs::rename(entry.path(), live.join(&file))
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
            Ok(())
        }

        fn remove(&self, name: &str) -> std::result::Result<bool, String> {
            if let Some(reason) = self.broken.lock().unwrap().clone() {
                return Err(reason);
            }
            if !self.root.join(name).exists() {
                return Ok(false);
            }
            self.archive(name).map(|_| true)
        }
    }

    /// 可以切换「有 turn 在跑」的静默门。
    struct Busy(AtomicBool);

    impl SessionGate for Busy {
        fn begin_merge(&self, _channel: &str, _chat_id: &str) -> Option<Box<dyn Send>> {
            Some(Box::new(()))
        }

        fn is_busy(&self, _channel: &str, _chat_id: &str) -> bool {
            false
        }

        fn running_sessions(&self) -> Vec<(String, String)> {
            Vec::new()
        }

        fn session_changed(&self, _channel: &str, _chat_id: &str, _deleted: bool) {}

        fn when_quiet(&self, f: &mut dyn FnMut()) -> bool {
            if self.0.load(Ordering::SeqCst) {
                return false;
            }
            f();
            true
        }
    }

    fn device_with(gate: Arc<dyn SessionGate>, builtin: &[&str]) -> (Device, Arc<Dir>) {
        let mut device = Device::with_gate(gate);
        let host = Arc::new(Dir {
            root: device.ctx.layout.skills_dir.clone(),
            builtin: builtin.iter().map(|name| name.to_string()).collect(),
            pinned: Mutex::default(),
            archived: Mutex::default(),
            busy: AtomicBool::new(false),
            broken: Mutex::default(),
            pruned: Mutex::default(),
        });
        device.ctx.skills = Some(host.clone());
        (device, host)
    }

    fn device() -> (Device, Arc<Dir>) {
        device_with(Arc::new(NoGate), &[])
    }

    fn sync_files(device: &Device) -> files::FilesReport {
        files::reconcile(
            &device.ctx,
            &device.settings,
            &device.keys,
            &mut Cache::default(),
            true,
        )
    }

    fn exchange(from: &Device, to: &Device) -> files::FilesReport {
        sync_files(from);
        testing::seal_all(from);
        testing::ship(from, to);
        to.merge();
        sync_files(to)
    }

    fn at(device: &Device, rel: &str) -> PathBuf {
        device.ctx.layout.skills_dir.join(rel)
    }

    fn write(device: &Device, rel: &str, body: impl AsRef<[u8]>) {
        let path = at(device, rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn read(device: &Device, rel: &str) -> Option<String> {
        std::fs::read_to_string(at(device, rel)).ok()
    }

    fn pending(device: &Device) -> Vec<PendingItem> {
        status::list(&device.ctx.paths)
            .into_iter()
            .filter(|item| item.kind == KIND)
            .collect()
    }

    fn notices(device: &Device, kind: &str) -> usize {
        status::list(&device.ctx.paths)
            .iter()
            .filter(|item| item.kind == kind)
            .count()
    }

    /// 待发队列里的 `skill.put`。
    fn puts(device: &Device) -> Vec<serde_json::Value> {
        testing::outbox(device)
            .into_iter()
            .filter(|op| op["t"] == "skill.put")
            .collect()
    }

    fn archived(device: &Device) -> usize {
        std::fs::read_dir(at(device, ".archive")).map_or(0, |dir| dir.count())
    }

    #[test]
    fn names_local_only_files_and_executables_are_recognised() {
        for path in [
            "config.toml",
            ".pinned",
            ".SKILL.md.patch.bak",
            ".venv/bin/python",
            "scripts/__pycache__/run.cpython-312.pyc",
            "node_modules/left-pad/index.js",
            "ark_key.toml",
            "sub/my_key.toml",
        ] {
            assert!(is_local_only(path), "{path}");
        }
        for path in [
            "SKILL.md",
            "templates/config.toml",
            "scripts/run.py",
            "references/keys.md",
        ] {
            assert!(!is_local_only(path), "{path}");
        }
        // 只差大小写的名字在 Windows 和 macOS 上就是那个文件本身（`ı` 的大写是 `I`）。
        for path in [
            "Config.TOML",
            "conf\u{131}g.toml",
            "scripts/__PyCache__/x",
            "Node_Modules/a.js",
        ] {
            assert!(is_local_only(path), "{path}");
        }
        // 长 s（大写是 S）、开尔文符号（小写是 k）、HFS+ 不看的零宽字符。
        for path in [
            "Skill.toml",
            "SKILL.TOML",
            "\u{17f}kill.toml",
            "s\u{212a}ill.toml",
            "skill\u{200c}.toml",
        ] {
            assert_eq!(root_contract(path), Some("skill.toml"), "{path}");
            assert!(is_executable(path, None), "{path}");
        }
        // 一对多的大小写转换不算：文件系统不把 `ß` 和 `ss` 当成同一个名字。
        assert_ne!(fold("stra\u{df}e.md"), fold("strasse.md"));
        assert_eq!(root_contract("skill.md"), Some("SKILL.md"));
        assert_eq!(root_contract("SKILL.md"), Some("SKILL.md"));
        assert_eq!(root_contract("templates/skill.toml"), None);
        assert_eq!(root_contract("skills.toml"), None);
        assert!(is_executable("scripts/run.\u{17f}h", None));
        assert!(is_executable("skill.toml", None));
        assert!(is_executable("scripts/run.PY", None));
        assert!(is_executable("bin/tool", Some(b"\x7fELF".as_slice())));
        assert!(is_executable("bin/tool", Some(b"#!/b".as_slice())));
        assert!(!is_executable(
            "references/guide.md",
            Some(b"# hi".as_slice())
        ));
        assert!(!is_executable("templates/skill.toml", None));
        for name in ["a", "notes", "pdf-tools_2.1"] {
            assert!(is_skill_name(name), "{name}");
        }
        for name in ["", "Notes", ".hidden", "con", "nul.txt", "a.", "a/b"] {
            assert!(!is_skill_name(name), "{name}");
        }
        // 版本摘要与清单的排列顺序无关。
        let a = version_hash([("b.md", "22"), ("a.md", "11")].into_iter());
        let b = version_hash([("a.md", "11"), ("b.md", "22")].into_iter());
        assert_eq!(a, b);
        assert_ne!(a, version_hash([("a.md", "11")].into_iter()));
    }

    #[test]
    fn a_remote_file_list_is_checked_before_use() {
        let keys = testing::keys();
        let file = |path: &str, text: &str| SkillFile {
            path: path.into(),
            size: text.len() as u64,
            sha256: sha256_hex(text.as_bytes()),
            exec: false,
            text: Some(text.into()),
            b64: None,
            blob: None,
        };
        let good = vec![file("SKILL.md", "# 笔记\n"), file("references/a.md", "a")];
        let check = |files: &[SkillFile]| check_version(&keys, files, 1 << 20);
        assert!(check(&good).is_ok());

        let with = |extra: SkillFile| {
            let mut files = good.clone();
            files.push(extra);
            files
        };
        // 跳出技能目录、本机专有的名字、只差大小写的重名、既是文件又是目录。
        assert!(check(&with(file("../a.md", "a"))).is_err());
        assert!(check(&with(file("config.toml", "a"))).is_err());
        assert!(check(&with(file("References/A.md", "a"))).is_err());
        assert!(check(&with(file("references/a.md/b.md", "b"))).is_err());
        // 在 Windows 上不是普通文件名的路径段：盘符、备用数据流、设备名、结尾的点。
        for path in [
            "C:a.md",
            "references/a.md:x",
            "references/NUL.md",
            "COM1/a.md",
            "a./b.md",
        ] {
            assert!(check(&with(file(path, "a"))).is_err(), "{path}");
        }
        // 内容与摘要不符。
        let mut wrong = file("references/b.md", "b");
        wrong.sha256 = sha256_hex(b"other");
        assert!(check(&with(wrong)).is_err());
        // 没有 SKILL.md；SKILL.md 里有不可见的控制字符。
        assert!(check(&good[1..]).is_err());
        assert!(check(&[file("SKILL.md", "# 笔\u{202e}记\n")]).is_err());
        // 契约文件的名字必须一字不差：只差大小写的，在 Windows 和 macOS 上会被加载器当成
        // 入口声明 / 技能说明来读，却躲过了「可执行内容要确认」和这里的检查。
        assert!(check(&with(file("skill.toml", "name = \"notes\"\n"))).is_ok());
        for path in [
            "Skill.toml",
            "SKILL.TOML",
            "\u{17f}kill.toml",
            "experience.md",
            "Skill.md",
        ] {
            assert!(check(&with(file(path, "x = 1\n"))).is_err(), "{path}");
        }
        // 附件对象必须就是摘要对应的那一个。
        let mut big = file("assets/big.dat", "x");
        big.text = None;
        big.blob = Some("0".repeat(64));
        assert!(check(&with(big.clone())).is_err());
        big.blob = Some(keys.blob_id(&ids::unhex(&big.sha256).unwrap()));
        assert!(check(&with(big.clone())).is_ok());
        // 比本机的附件大小上限还大的文件取不回来，这个版本用不了。
        big.size = (1 << 20) + 1;
        assert!(check(&with(big)).is_err());
    }

    #[test]
    fn a_skill_travels_whole_and_local_only_files_stay_home() {
        let (a, _) = device();
        let (b, _) = device();
        let logo = [0x89u8, b'P', b'N', b'G', 0xff, 0x00];
        write(&a, "notes/SKILL.md", "# 笔记\n\n记笔记的办法。\n");
        write(&a, "notes/references/guide.md", "细则\n");
        write(&a, "notes/assets/logo.png", logo);
        write(&a, "notes/config.toml", "token = \"本机的\"\n");
        write(&a, "notes/.pinned", "");
        write(&a, "notes/my_key.toml", "key = \"k\"\n");
        write(
            &a,
            "notes/scripts/__pycache__/run.cpython-312.pyc",
            [0u8, 1, 2],
        );
        let report = exchange(&a, &b);
        assert!(report.reload.contains(&Reload::Skills));
        assert_eq!(
            read(&b, "notes/SKILL.md").as_deref(),
            Some("# 笔记\n\n记笔记的办法。\n")
        );
        assert_eq!(
            read(&b, "notes/references/guide.md").as_deref(),
            Some("细则\n")
        );
        assert_eq!(
            std::fs::read(at(&b, "notes/assets/logo.png")).unwrap(),
            logo
        );
        for local_only in [
            "notes/config.toml",
            "notes/.pinned",
            "notes/my_key.toml",
            "notes/scripts",
        ] {
            assert!(!at(&b, local_only).exists(), "{local_only}");
        }
        // 落下来的版本不会被当成本机的改动再发出去。
        sync_files(&b);
        assert!(puts(&b).is_empty());

        // B 上改一处、删一个文件，传回 A：A 本机专有的文件还在，原来的版本进了归档。
        write(&b, "notes/SKILL.md", "# 笔记\n\n改过的办法。\n");
        std::fs::remove_file(at(&b, "notes/references/guide.md")).unwrap();
        exchange(&b, &a);
        assert_eq!(
            read(&a, "notes/SKILL.md").as_deref(),
            Some("# 笔记\n\n改过的办法。\n")
        );
        assert!(read(&a, "notes/references/guide.md").is_none());
        assert_eq!(
            read(&a, "notes/config.toml").as_deref(),
            Some("token = \"本机的\"\n")
        );
        assert_eq!(archived(&a), 1);
        sync_files(&a);
        assert!(puts(&a).is_empty());
    }

    #[test]
    fn a_synced_skill_is_marked_with_its_source_until_it_is_changed_here() {
        fn sources(device: &Device) -> Vec<(String, String)> {
            db::read(device.ctx.db(), origins).unwrap()
        }
        fn label(on: &Device, of: &Device) -> String {
            db::read(on.ctx.db(), |conn| {
                db::device_label(conn, &of.settings.device)
            })
            .unwrap()
        }
        let (a, _) = device();
        let (b, _) = device();
        write(&a, "notes/SKILL.md", "# 笔记\n");
        write(&a, "todo/SKILL.md", "# 待办\n");
        exchange(&a, &b);
        // 发布的一方：自己写的技能没有来源；收到的一方：两个都记着来自 A。
        assert!(sources(&a).is_empty());
        let from_a = label(&b, &a);
        assert_eq!(
            sources(&b),
            vec![
                ("notes".to_string(), from_a.clone()),
                ("todo".to_string(), from_a.clone())
            ]
        );
        // 落下来的版本不算本机的改动：来源留着。
        sync_files(&b);
        assert_eq!(sources(&b).len(), 2);

        // B 改了其中一个：它现在是 B 自己的版本；A 收到之后记着来自 B。
        write(&b, "notes/SKILL.md", "# 笔记\n\n改过。\n");
        exchange(&b, &a);
        assert_eq!(sources(&b), vec![("todo".to_string(), from_a)]);
        assert_eq!(sources(&a), vec![("notes".to_string(), label(&a, &b))]);

        // A 删掉另一个：B 上随之移走，来源也不再留着。
        std::fs::remove_dir_all(at(&a, "todo")).unwrap();
        exchange(&a, &b);
        assert!(!at(&b, "todo").exists());
        assert!(sources(&b).is_empty());
    }

    #[test]
    fn built_in_skills_never_leave_and_are_never_replaced() {
        let (a, _) = device_with(Arc::new(NoGate), &["pdf"]);
        let (b, _) = device_with(Arc::new(NoGate), &["notes"]);
        write(&a, "pdf/SKILL.md", "# PDF\n");
        write(&a, "notes/SKILL.md", "# 笔记\n");
        write(&b, "notes/SKILL.md", "# 内置的笔记\n");
        sync_files(&a);
        let sent = puts(&a);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["name"], "notes");
        // 这个名字在 B 上是内置技能：A 的同名技能不落到 B，B 的也不发布。
        exchange(&a, &b);
        assert_eq!(
            read(&b, "notes/SKILL.md").as_deref(),
            Some("# 内置的笔记\n")
        );
        assert!(!at(&b, "pdf").exists());
        assert!(puts(&b).is_empty());
        assert!(pending(&b).is_empty());
    }

    #[test]
    fn executables_wait_for_local_confirmation() {
        let (a, _) = device();
        let (b, _) = device();
        write(&a, "notes/SKILL.md", "# 笔记\n");
        exchange(&a, &b);
        assert!(pending(&b).is_empty());

        // 带来了脚本的版本：确认之前技能目录里看不到它的任何改动。
        write(&a, "notes/SKILL.md", "# 笔记\n\n用脚本整理。\n");
        write(&a, "notes/scripts/run.py", "print('v1')\n");
        exchange(&a, &b);
        assert_eq!(read(&b, "notes/SKILL.md").as_deref(), Some("# 笔记\n"));
        assert!(!at(&b, "notes/scripts").exists());
        let items = pending(&b);
        assert_eq!(items.len(), 1);
        assert!(items[0].title.contains("修改"));
        assert_eq!(
            items[0].payload["exec"],
            serde_json::json!(["scripts/run.py"])
        );
        let detail = files::detail(&b.ctx, &items[0].id).unwrap();
        let script = detail["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == "scripts/run.py")
            .unwrap();
        assert_eq!(script["status"], "added");
        assert_eq!(script["exec"], true);
        assert_eq!(script["remote"], "print('v1')\n");
        // 接受：下一轮对比装上。
        files::decide(
            &b.ctx,
            &b.settings,
            &items[0].id,
            "accept",
            items[0].version(),
        )
        .unwrap();
        let report = sync_files(&b);
        assert!(report.reload.contains(&Reload::Skills));
        assert_eq!(
            read(&b, "notes/scripts/run.py").as_deref(),
            Some("print('v1')\n")
        );
        assert!(pending(&b).is_empty());

        // 只改说明、脚本没动：直接生效。
        write(&a, "notes/SKILL.md", "# 笔记\n\n用脚本整理（改）。\n");
        exchange(&a, &b);
        assert_eq!(
            read(&b, "notes/SKILL.md").as_deref(),
            Some("# 笔记\n\n用脚本整理（改）。\n")
        );
        assert!(pending(&b).is_empty());

        // 脚本改了：再问。拒绝之后本机保持原样，也不把本机的版本推回去。
        write(&a, "notes/scripts/run.py", "print('v2')\n");
        exchange(&a, &b);
        let items = pending(&b);
        assert_eq!(items.len(), 1);
        files::decide(
            &b.ctx,
            &b.settings,
            &items[0].id,
            "reject",
            items[0].version(),
        )
        .unwrap();
        sync_files(&b);
        assert_eq!(
            read(&b, "notes/scripts/run.py").as_deref(),
            Some("print('v1')\n")
        );
        assert!(pending(&b).is_empty());
        assert!(puts(&b).is_empty());
    }

    #[test]
    fn a_joining_device_takes_the_space_version_and_keeps_a_copy() {
        let (a, _) = device();
        let (b, _) = device();
        write(&a, "notes/SKILL.md", "# 空间里的版本\n");
        write(&b, "notes/SKILL.md", "# 本机原来的版本\n");
        write(&b, "todo/SKILL.md", "# 只在本机的技能\n");
        exchange(&a, &b);
        assert_eq!(
            read(&b, "notes/SKILL.md").as_deref(),
            Some("# 空间里的版本\n")
        );
        let kept: Vec<_> = std::fs::read_dir(b.ctx.paths.conflicts.join("skills"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            std::fs::read_to_string(kept[0].path().join("SKILL.md")).unwrap(),
            "# 本机原来的版本\n"
        );
        // 本机独有的技能作为存量发布出去。
        exchange(&b, &a);
        assert_eq!(
            read(&a, "todo/SKILL.md").as_deref(),
            Some("# 只在本机的技能\n")
        );
    }

    #[test]
    fn deletions_archive_and_pinned_or_disabled_skills_are_left_alone() {
        let (a, _) = device();
        let (b, host) = device();
        for name in ["notes", "todo", "old"] {
            write(&a, &format!("{name}/SKILL.md"), "# 第一版\n");
        }
        exchange(&a, &b);
        // 删除：对方的移进归档。
        std::fs::remove_dir_all(at(&a, "notes")).unwrap();
        exchange(&a, &b);
        assert!(!at(&b, "notes").exists());
        assert_eq!(archived(&b), 1);

        // 固定的技能：别的设备的修改和删除都先问。
        host.pinned.lock().unwrap().insert("todo".into());
        write(&a, "todo/SKILL.md", "# 第二版\n");
        exchange(&a, &b);
        assert_eq!(read(&b, "todo/SKILL.md").as_deref(), Some("# 第一版\n"));
        let items = pending(&b);
        assert_eq!(items.len(), 1);
        files::decide(
            &b.ctx,
            &b.settings,
            &items[0].id,
            "reject",
            items[0].version(),
        )
        .unwrap();
        std::fs::remove_dir_all(at(&a, "todo")).unwrap();
        exchange(&a, &b);
        let items = pending(&b);
        assert_eq!(items.len(), 1);
        assert!(items[0].title.contains("删除"));
        files::decide(
            &b.ctx,
            &b.settings,
            &items[0].id,
            "accept",
            items[0].version(),
        )
        .unwrap();
        sync_files(&b);
        assert!(!at(&b, "todo").exists());
        assert!(pending(&b).is_empty());

        // 本机停用（归档）的技能：不当成删除发出去，远端的新版本也先不落；恢复之后再继续。
        host.archived.lock().unwrap().insert("old".into());
        let parked = b.ctx.paths.root.join("parked");
        std::fs::rename(at(&b, "old"), &parked).unwrap();
        write(&a, "old/SKILL.md", "# 第二版\n");
        exchange(&a, &b);
        assert!(testing::outbox(&b).iter().all(|op| op["t"] != "skill.del"));
        assert!(!at(&b, "old").exists());
        host.archived.lock().unwrap().clear();
        std::fs::rename(&parked, at(&b, "old")).unwrap();
        sync_files(&b);
        assert_eq!(read(&b, "old/SKILL.md").as_deref(), Some("# 第二版\n"));
    }

    #[test]
    fn the_swap_waits_until_no_turn_is_running() {
        let gate = Arc::new(Busy(AtomicBool::new(true)));
        let (a, _) = device();
        let (b, _) = device_with(gate.clone(), &[]);
        write(&a, "notes/SKILL.md", "# 笔记\n");
        let report = exchange(&a, &b);
        assert!(report.retry);
        assert_eq!(report.applied, 0);
        assert!(!at(&b, "notes").exists());
        gate.0.store(false, Ordering::SeqCst);
        let report = sync_files(&b);
        assert_eq!(report.applied, 1);
        assert_eq!(read(&b, "notes/SKILL.md").as_deref(), Some("# 笔记\n"));
    }

    #[test]
    fn a_swap_that_fails_is_retried_later_and_not_held_against_the_version() {
        let (a, _) = device();
        let (b, host) = device();
        write(&a, "notes/SKILL.md", "# 第一版\n");
        exchange(&a, &b);
        // 引擎每一轮用的是同一个缓存：失败之后隔多久再试记在里面。
        let mut cache = Cache::default();
        let round =
            |cache: &mut Cache| files::reconcile(&b.ctx, &b.settings, &b.keys, cache, false);
        let mark = || {
            db::read(b.ctx.db(), |conn| files::row(conn, scope::SKILL, "notes"))
                .unwrap()
                .unwrap()
                .mark
        };
        let retry_notices = || -> Vec<PendingItem> {
            status::list(&b.ctx.paths)
                .into_iter()
                .filter(|item| item.kind == NOTICE_RETRY)
                .collect()
        };

        // 换不上去（文件被别的程序占着之类）：不是这个版本的问题。
        write(&a, "notes/SKILL.md", "# 第二版\n");
        *host.broken.lock().unwrap() = Some("归档移动失败: 拒绝访问".into());
        sync_files(&a);
        testing::seal_all(&a);
        testing::ship(&a, &b);
        b.merge();
        let report = round(&mut cache);
        assert_eq!(report.applied, 0);
        assert!(!report.retry, "不催着马上再来一轮");
        assert_eq!(mark(), None, "还是待落地，没有被记成「这个版本用不了」");
        assert_eq!(notices(&b, "skill-rejected"), 0);
        assert!(retry_notices().is_empty(), "头一次失败不提示");
        assert!(
            !at(&b, ".sync-staging").exists(),
            "没用上的料不留在技能目录里"
        );

        // 还没到再试的时候：不去碰它，也不把本机的旧版本当成本机的改动发出去。
        *host.broken.lock().unwrap() = None;
        let report = round(&mut cache);
        assert_eq!(report.applied, 0);
        assert_eq!(read(&b, "notes/SKILL.md").as_deref(), Some("# 第一版\n"));
        assert!(puts(&b).is_empty());

        // 到时候了，又失败一次：这回提示，说的是真实的原因。
        *host.broken.lock().unwrap() = Some("归档移动失败: 拒绝访问".into());
        cache.retry_now(&retry_key("notes"));
        round(&mut cache);
        let shown = retry_notices();
        assert_eq!(shown.len(), 1);
        assert!(shown[0].detail.contains("拒绝访问"), "{}", shown[0].detail);
        assert_eq!(mark(), None);

        // 再到时候，换上去了：提示自己撤掉。
        *host.broken.lock().unwrap() = None;
        cache.retry_now(&retry_key("notes"));
        let report = round(&mut cache);
        assert_eq!(report.applied, 1);
        assert_eq!(read(&b, "notes/SKILL.md").as_deref(), Some("# 第二版\n"));
        assert!(retry_notices().is_empty());
        assert_eq!(mark().as_deref(), Some(HANDLED));
        // 两次装上（第一版、第二版）之后各清理过一次归档里的旧版本。
        assert_eq!(*host.pruned.lock().unwrap(), ["notes", "notes"]);
    }

    #[test]
    fn the_copy_of_the_local_version_is_made_once_and_outside_the_swap() {
        let (a, _) = device();
        let (b, host) = device();
        write(&a, "notes/SKILL.md", "# 空间里的版本\n");
        write(&b, "notes/SKILL.md", "# 本机原来的版本\n");
        // 后台任务正在用技能：料备好了也换不了，下一轮再来。
        host.busy.store(true, Ordering::SeqCst);
        let report = exchange(&a, &b);
        assert!(report.retry);
        assert_eq!(report.applied, 0);
        let report = sync_files(&b);
        assert_eq!(report.applied, 0);
        assert_eq!(
            read(&b, "notes/SKILL.md").as_deref(),
            Some("# 本机原来的版本\n")
        );
        assert!(
            !at(&b, ".sync-staging").exists(),
            "没用上的料不留在技能目录里"
        );
        host.busy.store(false, Ordering::SeqCst);
        let report = sync_files(&b);
        assert_eq!(report.applied, 1);
        assert_eq!(
            read(&b, "notes/SKILL.md").as_deref(),
            Some("# 空间里的版本\n")
        );
        // 三轮都走到了「留副本」那一步，留下来的只有一份，而且是完整的。
        let kept: Vec<_> = std::fs::read_dir(b.ctx.paths.conflicts.join("skills"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            std::fs::read_to_string(kept[0].path().join("SKILL.md")).unwrap(),
            "# 本机原来的版本\n"
        );
    }

    #[test]
    fn a_local_edit_made_while_the_new_version_was_being_prepared_is_not_archived_away() {
        let (a, _) = device();
        let (b, _) = device();
        write(&a, "notes/SKILL.md", "# 第一版\n");
        exchange(&a, &b);
        write(&a, "notes/SKILL.md", "# 第二版\n");
        sync_files(&a);
        testing::seal_all(&a);
        testing::ship(&a, &b);
        b.merge();
        // 列清单、算摘要时本机还是第一版；之后（备料期间）本机改了它。
        let files = list_skill(&at(&b, "notes"), u64::MAX).unwrap();
        let seen = hash_local(&mut Cache::default(), "notes", files)
            .ok()
            .unwrap();
        write(&b, "notes/SKILL.md", "# 第一版，本机刚加了一行\n");
        assert!(!unchanged(&at(&b, "notes"), &seen, u64::MAX));
        // 下一轮它算本机的新版本：留下并发布，不被远端的版本换进归档。
        sync_files(&b);
        assert_eq!(
            read(&b, "notes/SKILL.md").as_deref(),
            Some("# 第一版，本机刚加了一行\n")
        );
        assert_eq!(puts(&b).len(), 1);
        assert_eq!(archived(&b), 0);
    }

    #[test]
    fn unsafe_versions_are_refused_on_both_ends() {
        let (a, _) = device();
        let (b, _) = device();
        // 发布端：写进模型上下文的文件里有不可见的控制字符，不发布，给出提示。
        write(&a, "hidden/SKILL.md", "# 隐藏\u{200b}的指令\n");
        sync_files(&a);
        assert!(puts(&a).is_empty());
        assert_eq!(notices(&a, NOTICE_SKIPPED), 1);
        // 改好之后照常发布，提示撤掉。
        write(&a, "hidden/SKILL.md", "# 不再隐藏的指令\n");
        sync_files(&a);
        assert_eq!(puts(&a).len(), 1);
        assert_eq!(notices(&a, NOTICE_SKIPPED), 0);

        // 接收端：清单里的路径跳出了技能目录，不落地；程序重启之后会再试一次。
        testing::seal_all(&a);
        testing::ship_tampered(&a, &b, |line| {
            line.replace("\"path\":\"SKILL.md\"", "\"path\":\"../SKILL.md\"")
        });
        b.merge();
        sync_files(&b);
        assert!(!at(&b, "hidden").exists());
        assert!(!at(&b, "SKILL.md").exists());
        assert_eq!(notices(&b, "skill-rejected"), 1);
        db::write(b.ctx.db(), |tx| files::retry_rejected(tx)).unwrap();
        let again = db::read(b.ctx.db(), |conn| files::row(conn, scope::SKILL, "hidden"))
            .unwrap()
            .unwrap();
        assert!(again.is_new());
    }

    #[tokio::test]
    async fn big_files_ride_as_attachment_objects() {
        use crate::transport::file::FileTransport;

        let (a, _) = device();
        let (b, _) = device();
        let storage = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(storage.path());
        let keys = Arc::new(testing::keys());
        let big: Vec<u8> = (0..300 * 1024u32).map(|i| (i * 31 % 251) as u8).collect();
        write(&a, "notes/SKILL.md", "# 笔记\n");
        write(&a, "notes/assets/data.dat", &big);
        sync_files(&a);
        let put = puts(&a).remove(0);
        let blob = put["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == "assets/data.dat")
            .unwrap()["blob"]
            .as_str()
            .unwrap()
            .to_string();
        // 不论「同步附件」开没开，技能里的大文件都排进了上传队列。
        let batch = crate::attach::upload_pending(&a.ctx, &transport, &keys, 10)
            .await
            .unwrap();
        assert_eq!(batch.done, 1);
        testing::seal_all(&a);
        testing::ship(&a, &b);
        b.merge();
        // 大文件还没取回：先不装，报出要取的对象。
        let report = sync_files(&b);
        assert_eq!(report.want_blobs, std::slice::from_ref(&blob));
        assert!(!at(&b, "notes").exists());
        crate::attach::download(&b.ctx, &transport, &keys, &blob, 1 << 20)
            .await
            .unwrap();
        let report = sync_files(&b);
        assert_eq!(report.applied, 1);
        assert_eq!(std::fs::read(at(&b, "notes/assets/data.dat")).unwrap(), big);
        // 登记改指向技能目录，同步缓存里不留第二份。
        let row = db::read(b.ctx.db(), |conn| db::blob_get(conn, &blob))
            .unwrap()
            .unwrap();
        assert_eq!(row.rel_path, "skills/notes/assets/data.dat");
        assert!(!b
            .ctx
            .env
            .resolve(&wire::cache_path("assets/data.dat", &blob))
            .exists());

        // 之后只改说明：大文件不用再取一遍。
        write(&a, "notes/SKILL.md", "# 笔记（改）\n");
        let report = exchange(&a, &b);
        assert!(report.want_blobs.is_empty());
        assert_eq!(
            read(&b, "notes/SKILL.md").as_deref(),
            Some("# 笔记（改）\n")
        );
        assert_eq!(std::fs::read(at(&b, "notes/assets/data.dat")).unwrap(), big);
    }
}
