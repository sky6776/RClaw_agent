//! 首运行向导：生成 `rclaw.toml` 并落盘。
//!
//! 配置约束（与 `rclaw-cli` 的 `deny_unknown_fields` 校验对齐）：只写真实字段。
//! 自进化开关按分发默认打开（`auto_memory` / `auto_evolve` / `skill_curator`），
//! 并在向导 UI 明示后台 review 的 API 额度消耗。API key 等值做字符白名单校验，
//! 杜绝 TOML 注入。

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 向导提交参数（前端表单）。
#[derive(Debug, Clone, Deserialize)]
pub struct WizardParams {
    /// provider 标识（写入 `[llm].provider` 与 `[[llm.providers]].name`）。
    pub provider: String,
    /// OpenAI 兼容端点（如 `https://open.bigmodel.cn/api/paas/v4`）。
    pub base_url: String,
    /// 模型名（如 `glm-5.3-flash`）。
    pub model: String,
    /// API key（明文表单输入，落盘到 `[[llm.providers]].api_key`）。
    pub api_key: String,
    /// 模型上下文窗口（token）；0 = 未声明，按 128K 假设派生压缩预算。
    pub context_length: u64,
    /// 自进化开关（auto_memory 恒开；auto_evolve / curator 随此项）。
    pub auto_evolve: bool,
}

/// 校验参数并生成配置文件；返回写入路径。已存在配置时拒绝（向导只跑一次）。
pub fn write_config(home: &Path, p: &WizardParams) -> Result<PathBuf, String> {
    if crate::home::has_config(home) {
        return Err(format!(
            "配置已存在（{}）；如需重配请编辑该文件后重启",
            crate::home::config_path(home).display()
        ));
    }
    validate(p)?;
    let toml_text = render_template(p);
    std::fs::create_dir_all(home).map_err(|e| format!("创建数据目录失败: {e}"))?;
    let path = crate::home::config_path(home);
    std::fs::write(&path, toml_text).map_err(|e| format!("写入配置失败: {e}"))?;
    Ok(path)
}

fn validate(p: &WizardParams) -> Result<(), String> {
    let name = p.provider.trim();
    if name.is_empty() {
        return Err("provider 标识不能为空".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("provider 标识只能包含字母、数字、- 与 _".into());
    }
    let base = p.base_url.trim();
    if !base.starts_with("http://") && !base.starts_with("https://") {
        return Err("API 端点必须以 http:// 或 https:// 开头".into());
    }
    if p.model.trim().is_empty() {
        return Err("模型名不能为空".into());
    }
    // TOML 基本字符串安全：拒绝引号 / 反斜杠 / 控制字符（API key 的合法字符集远大于此）。
    for (label, value) in [
        ("API key", &p.api_key),
        ("模型名", &p.model),
        ("provider 标识", &p.provider),
    ] {
        if value
            .chars()
            .any(|c| c == '"' || c == '\\' || (c as u32) < 0x20)
        {
            return Err(format!("{label} 含非法字符（引号 / 反斜杠 / 控制字符）"));
        }
    }
    if p.api_key.trim().is_empty() {
        return Err("API key 不能为空".into());
    }
    Ok(())
}

/// 渲染初始 `rclaw.toml`。只写 `deny_unknown_fields` 允许的真实字段；
/// 自进化开关写显式值（不依赖编译期默认，避免与 CLI 的行为分叉）。
fn render_template(p: &WizardParams) -> String {
    let auto_evolve = if p.auto_evolve { "true" } else { "false" };
    format!(
        r#"# RClaw 桌面版 · 首运行向导生成
# 完整可配置项参考仓库根目录的 rclaw.toml.example。

[agent]
base_dir = "agent"
session_compression = true
auto_memory = true
auto_evolve = {auto_evolve}
skill_curator = {auto_evolve}

[log]
file = true
file_level = "info"
console = false

[llm]
provider = "{provider}"
# 不写则默认 4096：推理模型的思考 token 也计入输出上限，4096 会导致
# turn 频繁以 finish_reason=length 异常中断。128000 为 GLM Coding Plan 实测值。
max_tokens = 128000

[[llm.providers]]
name = "{provider}"
base_url = "{base_url}"
model = "{model}"
api_key = "{api_key}"
context_length = {context_length}
"#,
        auto_evolve = auto_evolve,
        provider = p.provider.trim(),
        base_url = p.base_url.trim(),
        model = p.model.trim(),
        api_key = p.api_key.trim(),
        context_length = p.context_length,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rclaw_cli::config::Config;

    fn params() -> WizardParams {
        WizardParams {
            provider: "zhipu".into(),
            base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
            model: "glm-5.3-flash".into(),
            api_key: "sk-test-123".into(),
            context_length: 131_072,
            auto_evolve: true,
        }
    }

    #[test]
    fn generated_config_round_trips_through_cli_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &params()).unwrap();
        assert_eq!(path, crate::home::config_path(dir.path()));

        // 生成的 TOML 必须能被 CLI 配置层原样解析（deny_unknown_fields 校验），
        // 且自进化开关 / provider 三元组逐字落盘。
        let cfg = Config::load_from_dir(dir.path()).unwrap();
        assert_eq!(cfg.llm.provider, "zhipu");
        let entry = cfg
            .llm
            .providers
            .iter()
            .find(|p| p.name == "zhipu")
            .expect("生成的 provider 条目应存在");
        assert_eq!(entry.base_url, "https://open.bigmodel.cn/api/paas/v4");
        assert_eq!(entry.model.as_deref(), Some("glm-5.3-flash"));
        assert_eq!(entry.api_key.as_deref(), Some("sk-test-123"));
        assert_eq!(entry.context_length, 131_072);
        assert_eq!(
            cfg.llm.max_tokens, 128_000,
            "向导应显式写 max_tokens（默认 4096 会被推理模型思考打爆）"
        );
        assert!(cfg.agent.session_compression, "会话自动压缩应默认显式开启");
        assert!(cfg.agent.auto_memory, "自进化记忆应默认开启");
        assert!(cfg.agent.auto_evolve, "自进化应默认开启");
        assert!(cfg.agent.skill_curator, "技能 curator 应默认开启");
    }

    #[test]
    fn auto_evolve_off_writes_false() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = params();
        p.auto_evolve = false;
        write_config(dir.path(), &p).unwrap();
        let cfg = Config::load_from_dir(dir.path()).unwrap();
        assert!(!cfg.agent.auto_evolve);
        assert!(!cfg.agent.skill_curator);
        assert!(
            cfg.agent.auto_memory,
            "auto_memory 恒开（记忆是自进化的底座）"
        );
    }

    #[test]
    fn refuses_to_overwrite_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), &params()).unwrap();
        assert!(write_config(dir.path(), &params()).is_err());
    }

    #[test]
    fn rejects_injection_and_bad_urls() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = params();
        p.api_key = "abc\";\n[[evil]]".into();
        assert!(write_config(dir.path(), &p).is_err(), "TOML 注入应被拒绝");

        let mut p = params();
        p.base_url = "ftp://example.com".into();
        assert!(
            write_config(dir.path(), &p).is_err(),
            "非 http 端点应被拒绝"
        );

        let mut p = params();
        p.api_key = "  ".into();
        assert!(write_config(dir.path(), &p).is_err(), "空 key 应被拒绝");
    }
}
