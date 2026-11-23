//! 配对码（§11.2）：`rclaw-pair-v1.<base64url(JSON)>`。
//!
//! 内含 `space_id` 和空间密钥，可选包含存储地址与凭据。**配对码等同于全部同步数据的
//! 钥匙**：它只出现在设置界面和 CLI 启动参数里，不进任何会话文本、日志或记忆
//! （`redact` 有针对这个格式的脱敏规则）。二维码的生成与识别在宿主（桌面 crate）里，
//! 这里只管文本。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::crypto::SpaceKey;
use crate::error::{Result, SyncError};
use crate::secrets::is_space_id;
use crate::transport::net::check_endpoint;

pub const PAIRING_PREFIX: &str = "rclaw-pair-v1.";
/// 配对码文本的长度上限（防粘贴进来一大段无关内容）。
const PAIRING_MAX_LEN: usize = 8 * 1024;

/// 随配对码带过去的存储设置。`file` 后端的目录是每台设备各自的本机路径，不带。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PairedStorage {
    Webdav {
        url: String,
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: String,
        #[serde(default)]
        allow_http: bool,
        #[serde(default)]
        pin: String,
    },
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        #[serde(default)]
        prefix: String,
        access_key_id: String,
        secret_key: String,
        #[serde(default)]
        path_style: bool,
        #[serde(default)]
        allow_http: bool,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode {
    #[serde(rename = "s")]
    pub space_id: String,
    /// 空间密钥（base64url）。
    #[serde(rename = "k")]
    pub key: String,
    #[serde(rename = "t", default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<PairedStorage>,
}

impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingCode")
            .field("space_id", &self.space_id)
            .field("has_storage", &self.storage.is_some())
            .finish_non_exhaustive()
    }
}

impl PairingCode {
    pub fn encode(&self) -> Result<String> {
        Ok(format!(
            "{PAIRING_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?)
        ))
    }

    /// 解析并校验一段配对码文本。复制粘贴带进来的空白与换行会被去掉。
    pub fn parse(text: &str) -> Result<PairingCode> {
        let bad = || SyncError::invalid("这不是有效的配对码");
        if text.len() > PAIRING_MAX_LEN {
            return Err(bad());
        }
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let body = compact.strip_prefix(PAIRING_PREFIX).ok_or_else(bad)?;
        let json = URL_SAFE_NO_PAD.decode(body).map_err(|_| bad())?;
        let code: PairingCode = serde_json::from_slice(&json).map_err(|_| bad())?;
        if !is_space_id(&code.space_id) {
            return Err(bad());
        }
        SpaceKey::from_base64(&code.key).map_err(|_| bad())?;
        // 配对码里的地址同样要过 HTTPS 规则：公网的明文地址在这里就拒绝，凭据不会被用上。
        match &code.storage {
            Some(PairedStorage::Webdav {
                url, allow_http, ..
            }) => {
                check_endpoint(url, *allow_http)?;
            }
            Some(PairedStorage::S3 {
                endpoint,
                allow_http,
                ..
            }) => {
                check_endpoint(endpoint, *allow_http)?;
            }
            None => {}
        }
        Ok(code)
    }

    /// 这段文本是不是一个还没给全的配对码：开头对，到这里为止的内容也都说得通，只是没完。
    ///
    /// 命令行读配对码时靠它决定要不要接着读下一行：粘贴进来的配对码可能被折成了几行，
    /// 只读第一行的话，剩下的几行会留在终端的输入里。
    pub fn is_incomplete(text: &str) -> bool {
        if text.len() > PAIRING_MAX_LEN || PairingCode::parse(text).is_ok() {
            return false;
        }
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let Some(body) = compact.strip_prefix(PAIRING_PREFIX) else {
            // 连开头都还没给全。
            return !compact.is_empty() && PAIRING_PREFIX.starts_with(compact.as_str());
        };
        // 末尾不满四个字符的那一组可能还没给全：解不出来就先不看它。（按字节切：正文里可能
        // 夹着不是配对码的多字节字符，按字符串切会切在字符中间。）
        let json = URL_SAFE_NO_PAD
            .decode(body)
            .or_else(|_| URL_SAFE_NO_PAD.decode(&body.as_bytes()[..body.len() / 4 * 4]));
        match json.map(|json| serde_json::from_slice::<serde_json::Value>(&json)) {
            Ok(Err(error)) => error.is_eof(),
            // 内容已经是完整的 JSON（只是不合规），或者夹着配对码里不会有的字符。
            Ok(Ok(_)) | Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::new_space_id;

    fn code(storage: Option<PairedStorage>) -> PairingCode {
        PairingCode {
            space_id: new_space_id(),
            key: SpaceKey::generate().to_base64(),
            storage,
        }
    }

    #[test]
    fn round_trips_with_and_without_storage() {
        let bare = code(None);
        let text = bare.encode().unwrap();
        assert!(text.starts_with(PAIRING_PREFIX));
        assert_eq!(PairingCode::parse(&text).unwrap(), bare);

        let full = code(Some(PairedStorage::Webdav {
            url: "https://dav.example.com/rclaw/".into(),
            username: "me".into(),
            password: "pw".into(),
            allow_http: false,
            pin: String::new(),
        }));
        let text = full.encode().unwrap();
        // 粘贴时被折行、带首尾空白。
        let wrapped = format!("  {}\n{}\n", &text[..20], &text[20..]);
        assert_eq!(PairingCode::parse(&wrapped).unwrap(), full);
        // Debug 不泄露密钥与密码。
        let shown = format!("{full:?}");
        assert!(!shown.contains(&full.key) && !shown.contains("pw"));
    }

    #[test]
    fn redaction_hides_pairing_codes() {
        // rclaw-store 的脱敏规则单独写了一份前缀，两边不能走样。
        assert_eq!(
            PAIRING_PREFIX.as_bytes(),
            rclaw_store::redact::PAIRING_CODE_PREFIX
        );
        let text = code(None).encode().unwrap();
        let wrapped = format!("配对码：{}\n{}\n完", &text[..40], &text[40..]);
        assert_eq!(
            rclaw_store::redact_sensitive_text(&wrapped),
            "配对码：[REDACTED]\n完"
        );
        // 带存储设置的（嵌套对象，密码里有花括号、引号、反斜杠）：写在 `token:` 后面、
        // 折成几行、带缩进和引用符、最后一行只剩几个字符，也整段遮掉；配对码结束之后的
        // 内容不受牵连。
        let full = code(Some(PairedStorage::Webdav {
            url: "https://dav.example.com/rclaw/".into(),
            username: "me".into(),
            password: "p}\"{w\\".into(),
            allow_http: false,
            pin: String::new(),
        }))
        .encode()
        .unwrap();
        let tail = full.len() - 5;
        let wrapped = format!(
            "token: {}\n>      {}\n{}\nabcdefghijklmnopqrstuvwxyz",
            &full[..60],
            &full[60..tail],
            &full[tail..]
        );
        assert_eq!(
            rclaw_store::redact_sensitive_text(&wrapped),
            "token: [REDACTED]\nabcdefghijklmnopqrstuvwxyz"
        );
    }

    #[test]
    fn rejects_garbage_and_tampering() {
        for bad in [
            "",
            "hello",
            "rclaw-pair-v1.",
            "rclaw-pair-v1.!!!!",
            "rclaw-pair-v2.e30",
            "rclaw-pair-v1.e30",
        ] {
            assert!(PairingCode::parse(bad).is_err(), "{bad}");
        }
        let mut short_key = code(None);
        short_key.key = "AAAA".into();
        assert!(PairingCode::parse(&short_key.encode().unwrap()).is_err());
        let mut bad_space = code(None);
        bad_space.space_id = "../../etc".into();
        assert!(PairingCode::parse(&bad_space.encode().unwrap()).is_err());
    }

    /// 命令行逐行读配对码时的判断：截断在哪儿都算「还没完」，给全了、或者明摆着不是配对码
    /// 的，不再等后面的行。
    #[test]
    fn a_code_cut_off_anywhere_is_incomplete_and_a_whole_or_wrong_one_is_not() {
        let webdav = |url: &str| {
            code(Some(PairedStorage::Webdav {
                url: url.into(),
                username: "我".into(),
                password: "p}\"{w\\".into(),
                allow_http: true,
                pin: String::new(),
            }))
        };
        for text in [
            code(None).encode().unwrap(),
            webdav("https://dav.example.com/rclaw/").encode().unwrap(),
        ] {
            for cut in 1..text.len() {
                assert!(PairingCode::is_incomplete(&text[..cut]), "{cut}");
            }
            assert!(!PairingCode::is_incomplete(&text));
            // 折了行、带着首尾空白的前半段。
            let wrapped = format!("  {}\n{}\n", &text[..30], &text[30..60]);
            assert!(PairingCode::is_incomplete(&wrapped));
        }
        // 内容完整但不合规的（公网 IP 的明文地址）：不是「没给全」。
        let refused = webdav("http://8.8.8.8/rclaw/").encode().unwrap();
        assert!(PairingCode::parse(&refused).is_err());
        assert!(!PairingCode::is_incomplete(&refused));
        for done in [
            "",
            "hello",
            "rclaw-pair-v2.e30",
            "rclaw-pair-v1.e30",
            "rclaw-pair-v1.!!!!",
            "rclaw-pair-v1.e30 后面跟着别的",
        ] {
            assert!(!PairingCode::is_incomplete(done), "{done}");
        }
    }

    #[test]
    fn storage_in_the_code_must_obey_the_https_rule() {
        let public_http = code(Some(PairedStorage::Webdav {
            url: "http://dav.example.com/rclaw/".into(),
            username: "me".into(),
            password: "pw".into(),
            allow_http: true,
            pin: String::new(),
        }));
        // 主机名走明文要到连接时才能按解析结果判定，这里放行；公网 IP 字面量当场拒绝。
        assert!(PairingCode::parse(&public_http.encode().unwrap()).is_ok());
        let public_ip = code(Some(PairedStorage::Webdav {
            url: "http://8.8.8.8/rclaw/".into(),
            username: "me".into(),
            password: "pw".into(),
            allow_http: true,
            pin: String::new(),
        }));
        assert!(PairingCode::parse(&public_ip.encode().unwrap()).is_err());
        let no_flag = code(Some(PairedStorage::Webdav {
            url: "http://192.168.1.2/rclaw/".into(),
            username: "me".into(),
            password: "pw".into(),
            allow_http: false,
            pin: String::new(),
        }));
        assert!(PairingCode::parse(&no_flag.encode().unwrap()).is_err());
    }
}
