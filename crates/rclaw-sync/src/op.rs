//! op：一条变更记录（§9.2）。
//!
//! 段里每行一条 op 的 JSON：op 自己的字段，加上类型 `t` 与混合逻辑时钟 `hlc`。
//! **所有 op 都必须能重复、乱序重放而结果不变**：消息按 `gid` 去重，删除按墓碑，
//! 其余都是按 HLC 比较的后写者胜。
//!
//! 解析分两级（§9.4）：`t` 不认识 → 调用方停在该设备日志的那个位置并提示升级；
//! `t` 认识但内容不合规 → 只隔离所属线程。

use rclaw_store::ToolCallMetaRecord;
use serde::{Deserialize, Serialize};

use crate::error::{Result, SyncError};

/// 线上格式版本（`space.json` 的 `format`、段头与 head 的 `v`）。
pub const FORMAT_VERSION: u32 = 1;

/// 内联进 `msg` op 的 artifact 全文上限。更大的走附件通道（P2），或只写占位。
pub const ARTIFACT_INLINE_MAX: usize = 256 * 1024;
/// 单条消息 `data` 的上限。
pub const MESSAGE_DATA_MAX: usize = 8 * 1024 * 1024;

/// 分叉来源：`/fork` 产生的新线程指向父线程里的锚点消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkOf {
    pub sid: String,
    pub gid: String,
}

/// 被消息引用的工作区文件（§8.7）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    /// 相对 agent 根的路径（`uploads/…`、`media/…`、`output/…`），正斜杠。
    pub path: String,
    /// 附件对象名。
    pub blob: String,
    pub size: u64,
}

/// 工具结果全文（artifact）。`text` 有值 = 内联；否则全文在附件对象 `blob` 里
/// （没有 `blob` 说明发布端没开附件同步，接收端只能写占位）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// 文件名（不含目录）。落到接收端该会话自己的 artifact 目录。
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// 一条消息的线上形态。`msg` op 与会话包的消息行共用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireMsg {
    pub gid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub role: String,
    /// Unix 秒。
    pub ts: i64,
    /// 可移植化之后的 `message.data`（JSON 文本，原样携带，不重新序列化）。
    pub data: String,
    /// 线上形态的 `meta`：只有 compaction boundary 与分叉告知两种，id 都是全局的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolCallMetaRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<FileRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MsgOp {
    pub sid: String,
    #[serde(flatten)]
    pub msg: WireMsg,
}

/// 技能目录里的一个文件（§8.4）。小文件内联（文本用 `text`，二进制用 `b64`），
/// 大文件走附件对象。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillFile {
    /// 相对技能目录的路径，正斜杠。
    pub path: String,
    pub size: u64,
    pub sha256: String,
    /// 是否可执行内容（脚本 / 二进制）：含这类文件的变更要在本机确认后才生效。
    #[serde(default)]
    pub exec: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Op {
    #[serde(rename = "device.hello")]
    DeviceHello {
        name: String,
        platform: String,
        version: String,
        /// 所取代的旧 `device_id`（换身份时声明，§9.6）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replaces: Option<String>,
    },
    #[serde(rename = "session.open")]
    SessionOpen {
        sid: String,
        channel: String,
        chat_id: String,
        /// Unix 毫秒。
        created_at: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fork_of: Option<ForkOf>,
    },
    #[serde(rename = "msg")]
    Msg(MsgOp),
    #[serde(rename = "session.set")]
    SessionSet {
        sid: String,
        /// 设置时该分支末条消息的 `gid`；空 = 对线程的所有分支生效。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<String>,
        /// `title` / `archived` / `goal`。
        field: String,
        value: serde_json::Value,
    },
    #[serde(rename = "session.delete")]
    SessionDelete {
        sid: String,
        #[serde(default)]
        tip: Option<String>,
        #[serde(default)]
        purge: bool,
        /// 被删的那条分支从开头到 `tip` 各消息 `gid` 的随机尾巴，按先后连成一个串
        /// （§7.5）。手里留着这条分支更早样子的设备靠它认出来。没有消息时为空。
        #[serde(default, skip_serializing_if = "String::is_empty")]
        chain: String,
    },
    #[serde(rename = "session.want")]
    SessionWant {
        sid: String,
        /// 悬空链的第一条。
        gid: String,
        /// 请哪台设备应答。
        to: String,
    },
    #[serde(rename = "session.bundle")]
    SessionBundle {
        sid: String,
        /// 一条分支的会话包（JSONL 文本）。
        bundle: String,
    },
    #[serde(rename = "mem.add")]
    MemAdd {
        /// `memory` / `user` / `bot:<名字>:memory` / `bot:<名字>:user`。
        target: String,
        id: String,
        text: String,
    },
    #[serde(rename = "mem.del")]
    MemDel {
        target: String,
        id: String,
        text: String,
    },
    #[serde(rename = "daily.append")]
    DailyAppend {
        /// `YYYY-MM-DD`。
        date: String,
        id: String,
        text: String,
        /// 没有本机基线时发布的存量，可能与别的设备已有的行重叠。
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        initial: bool,
    },
    #[serde(rename = "file.put")]
    FilePut {
        /// 白名单内的相对路径（相对 agent 根）。
        path: String,
        hash: String,
        content: String,
    },
    #[serde(rename = "file.del")]
    FileDel { path: String, hash: String },
    #[serde(rename = "blob.want")]
    BlobWant { blob: String },
    /// 对 `blob.want` 的回话（也可以不等人问就说）：本机登记过这个附件，现在拿不出来了
    /// ——文件没了，或者内容已经不是那一份。
    #[serde(rename = "blob.gone")]
    BlobGone { blob: String },
    #[serde(rename = "skill.put")]
    SkillPut {
        name: String,
        hash: String,
        files: Vec<SkillFile>,
    },
    #[serde(rename = "skill.del")]
    SkillDel { name: String, hash: String },
    #[serde(rename = "bot.put")]
    BotPut {
        name: String,
        hash: String,
        /// bot 定义文件原文（TOML）。
        def: String,
    },
    #[serde(rename = "bot.del")]
    BotDel { name: String, hash: String },
    #[serde(rename = "cron.put")]
    CronPut {
        /// 归属设备。
        owner: String,
        id: String,
        /// 任务定义（`CronJob` 的 JSON，不含本机的触发状态）。
        job: serde_json::Value,
        /// 投递目标会话的线程 id；空 = 目标不在同步范围内，只能镜像查看。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    #[serde(rename = "cron.del")]
    CronDel { owner: String, id: String },
    #[serde(rename = "cron.transfer")]
    CronTransfer {
        id: String,
        from: String,
        to: String,
        /// 说的是任务的哪一版：原归属设备发布那份定义（`cron.put`）时的 HLC。任务在原
        /// 设备上改过之后，针对旧版的请求、回执就都不算数了。
        rev: String,
        /// 强制接管：不等原归属设备的回执。
        #[serde(default)]
        force: bool,
        /// 原归属设备的回执：它已把任务从自己的任务表里移除。
        #[serde(default)]
        ack: bool,
        /// 落定：`to` 已经把任务放进了自己的任务表，这次交接到此为止。接手的设备看到
        /// 回执后发；原归属设备等不到对方落定、把任务收回去时也发（这时 `to` 就是
        /// `from`）。`force`、`ack`、`done` 至多有一个为真。
        #[serde(default)]
        done: bool,
    },
}

/// 本版本认识的全部 op 类型。
pub const KNOWN_TYPES: &[&str] = &[
    "device.hello",
    "session.open",
    "msg",
    "session.set",
    "session.delete",
    "session.want",
    "session.bundle",
    "mem.add",
    "mem.del",
    "daily.append",
    "file.put",
    "file.del",
    "blob.want",
    "blob.gone",
    "skill.put",
    "skill.del",
    "bot.put",
    "bot.del",
    "cron.put",
    "cron.del",
    "cron.transfer",
];

impl Op {
    /// 所属线程（会话类 op）。
    pub fn sid(&self) -> Option<&str> {
        match self {
            Op::SessionOpen { sid, .. }
            | Op::SessionSet { sid, .. }
            | Op::SessionDelete { sid, .. }
            | Op::SessionWant { sid, .. }
            | Op::SessionBundle { sid, .. } => Some(sid),
            Op::Msg(msg) => Some(&msg.sid),
            _ => None,
        }
    }

    /// 序列化成段里的一行（带 `hlc`）。
    pub fn to_line(&self, hlc: &str) -> Result<String> {
        let mut value = serde_json::to_value(self)?;
        value
            .as_object_mut()
            .ok_or_else(|| SyncError::other("op 不是 JSON 对象"))?
            .insert("hlc".into(), serde_json::Value::String(hlc.to_string()));
        Ok(value.to_string())
    }
}

/// 一行 op 的外层：类型与 HLC 已取出，正文尚未按类型解析。
#[derive(Debug, Clone)]
pub struct OpLine {
    pub kind: String,
    pub hlc: String,
    /// 所属线程（会话类 op 的 `sid` 字段；非会话类为空）。
    pub sid: Option<String>,
    pub value: serde_json::Value,
}

impl OpLine {
    /// 解析外层。行本身不是 JSON 对象、缺 `t` / `hlc`、HLC 格式不对都是段损坏。
    pub fn parse(line: &str) -> Result<OpLine> {
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| SyncError::corrupt(format!("op 不是合法 JSON: {e}")))?;
        let kind = value
            .get("t")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SyncError::corrupt("op 缺少类型"))?
            .to_string();
        let hlc = value
            .get("hlc")
            .and_then(|v| v.as_str())
            .filter(|hlc| rclaw_store::sync_db::remote_hlc_parts(hlc).is_some())
            .ok_or_else(|| SyncError::corrupt("op 缺少合法的 hlc"))?
            .to_string();
        let sid = value
            .get("sid")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(OpLine {
            kind,
            hlc,
            sid,
            value,
        })
    }

    /// 本版本是否认识这个类型。
    pub fn is_known(&self) -> bool {
        KNOWN_TYPES.contains(&self.kind.as_str())
    }

    /// 按类型解析正文。失败 = 已知类型的 op 内容不合规。
    pub fn decode(&self) -> Result<Op> {
        serde_json::from_value::<Op>(self.value.clone())
            .map_err(|e| SyncError::invalid(format!("{} 的内容不合规: {e}", self.kind)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HLC: &str = "1763438400000-0001-0123456789abcdef";

    fn sample_msg() -> Op {
        Op::Msg(MsgOp {
            sid: "a".repeat(24),
            msg: WireMsg {
                gid: "0123456789abcdef.0123456789abcdef".into(),
                parent: None,
                role: "user".into(),
                ts: 1_763_438_400,
                data: r#"{"role":"user","content":[{"type":"text","text":"hi"}],"ts":1763438400}"#
                    .into(),
                meta: None,
                tools: Vec::new(),
                refs: Vec::new(),
                artifacts: Vec::new(),
            },
        })
    }

    #[test]
    fn op_round_trips_through_a_line() {
        let op = sample_msg();
        let line = op.to_line(HLC).unwrap();
        let parsed = OpLine::parse(&line).unwrap();
        assert_eq!(parsed.kind, "msg");
        assert_eq!(parsed.hlc, HLC);
        assert_eq!(parsed.sid.as_deref(), op.sid());
        assert!(parsed.is_known());
        assert_eq!(parsed.decode().unwrap(), op);
    }

    #[test]
    fn every_known_type_has_a_variant() {
        // 每个已知类型名都能被枚举识别（类型名与变体的对应不会悄悄漂移）。
        for kind in KNOWN_TYPES {
            let probe = serde_json::json!({ "t": kind });
            let err = serde_json::from_value::<Op>(probe).unwrap_err().to_string();
            assert!(
                !err.contains("unknown variant"),
                "{kind} 没有对应的枚举变体: {err}"
            );
        }
    }

    #[test]
    fn unknown_type_is_reported_as_unknown_not_invalid() {
        let line = format!(r#"{{"t":"future.op","hlc":"{HLC}","x":1}}"#);
        let parsed = OpLine::parse(&line).unwrap();
        assert!(!parsed.is_known());
    }

    #[test]
    fn known_type_with_bad_body_is_invalid() {
        let line = format!(r#"{{"t":"session.delete","hlc":"{HLC}"}}"#);
        let parsed = OpLine::parse(&line).unwrap();
        assert!(parsed.is_known());
        assert!(matches!(parsed.decode(), Err(SyncError::Invalid(_))));
    }

    #[test]
    fn a_delete_carries_its_chain_only_when_there_is_one() {
        let sid = "a".repeat(24);
        let tip = "0123456789abcdef.00000000000000a2".to_string();
        let with_chain = Op::SessionDelete {
            sid: sid.clone(),
            tip: Some(tip.clone()),
            purge: false,
            chain: "00000000000000a100000000000000a2".into(),
        };
        let line = with_chain.to_line(HLC).unwrap();
        assert!(line.contains(r#""chain":"00000000000000a100000000000000a2""#));
        assert_eq!(OpLine::parse(&line).unwrap().decode().unwrap(), with_chain);
        // 没有消息的会话：不带这个字段，读的时候按空的算。
        let bare = Op::SessionDelete {
            sid: sid.clone(),
            tip: None,
            purge: true,
            chain: String::new(),
        };
        let line = bare.to_line(HLC).unwrap();
        assert!(!line.contains("chain"));
        assert_eq!(OpLine::parse(&line).unwrap().decode().unwrap(), bare);
    }

    #[test]
    fn malformed_lines_are_corrupt() {
        for line in [
            "",
            "not json",
            "[]",
            r#"{"t":"msg"}"#,
            r#"{"hlc":"1763438400000-0001-x"}"#,
            r#"{"t":"msg","hlc":"bad"}"#,
        ] {
            assert!(
                matches!(OpLine::parse(line), Err(SyncError::Corrupt(_))),
                "{line}"
            );
        }
    }
}
