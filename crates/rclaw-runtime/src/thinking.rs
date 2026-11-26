//! 会话页「思考强度」选择器的能力判定与状态。
//!
//! 选项表按 **live provider 的能力族 + 精确模型名** 判定，避免同名代理（例如
//! 自建 OpenAI 兼容端点也叫 `deepseek-flash`）被错误开放「关闭思考」：
//! - 只有 `ProviderFlavor::DeepSeek` 且模型属于 DeepSeek 家族的才提供「关闭」；
//! - 智谱只对 `glm-5.3-flash` / `glm-5.4` 开放 `low/high/max`（5.3-flash 强制思考）；
//! - 其它 provider / 模型 `supported = false`，UI 整体隐藏。

use serde::Serialize;

use rclaw_llm::{ProviderConfig, ProviderFlavor, SwappableProvider};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ThinkingOption {
    pub value: &'static str,
    pub label: &'static str,
}

const DISABLED: ThinkingOption = ThinkingOption {
    value: "disabled",
    label: "关闭",
};
const LOW: ThinkingOption = ThinkingOption {
    value: "low",
    label: "低",
};
const HIGH: ThinkingOption = ThinkingOption {
    value: "high",
    label: "高",
};
const MAX: ThinkingOption = ThinkingOption {
    value: "max",
    label: "最高",
};

/// 当前模型的思考强度状态（供 Tauri 命令 / UI 渲染）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThinkingState {
    pub supported: bool,
    pub provider: String,
    pub model: String,
    /// 当前档位；None = 未显式配置（服务端默认）。
    pub current: Option<String>,
    pub options: Vec<ThinkingOption>,
}

/// 模型选择器里的一个模型（含该模型可用的思考档位）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelOption {
    pub model: &'static str,
    pub label: &'static str,
    pub provider: &'static str,
    pub efforts: Vec<ThinkingOption>,
}

/// 模型选择器状态：当前 provider/model 与候选模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelState {
    pub supported: bool,
    pub provider: String,
    pub model: String,
    pub models: Vec<ModelOption>,
}

/// 一次下发「模型 + 思考强度」的完整状态（切换后前端一次更新，不出现半套状态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LlmState {
    /// 单调递增版本：GET / SET 回执 / 广播都按它排序，前端只应用更新的版本。
    pub revision: u64,
    pub model: ModelState,
    pub thinking: ThinkingState,
}

impl ThinkingState {
    /// agent 未启动 / 当前模型不支持时的空状态。
    pub fn unsupported(provider: String, model: String) -> Self {
        Self {
            supported: false,
            provider,
            model,
            current: None,
            options: Vec::new(),
        }
    }
}

fn is_deepseek_family(model: &str) -> bool {
    matches!(
        model,
        "deepseek-flash" | "deepseek-v4-flash" | "deepseek-v4-flash-vision-exp" | "deepseek-v4-pro"
    )
}

fn is_supported_glm(model: &str) -> bool {
    matches!(model, "glm-5.3-flash" | "glm-5.4")
}

/// 按 live 配置给出可选档位。
pub fn options_for(config: &ProviderConfig) -> Vec<ThinkingOption> {
    let model = config.default_model.trim().to_ascii_lowercase();
    match config.flavor {
        ProviderFlavor::DeepSeek if is_deepseek_family(&model) => {
            vec![DISABLED, LOW, HIGH, MAX]
        }
        ProviderFlavor::Zhipu if is_supported_glm(&model) => vec![LOW, HIGH, MAX],
        _ => Vec::new(),
    }
}

/// 按"将要切换到的模型"给出可选档位（模型选择器里展示每个候选模型的档位）。
pub fn options_for_model(model: &str) -> Vec<ThinkingOption> {
    let model = model.trim().to_ascii_lowercase();
    if is_deepseek_family(&model) {
        vec![DISABLED, LOW, HIGH, MAX]
    } else if is_supported_glm(&model) {
        vec![LOW, HIGH, MAX]
    } else {
        Vec::new()
    }
}

/// 当前档位：关闭（或 effort=none）必须映射到当前模型实际可选的档位；
/// 否则 UI 会显示「默认」而丢失真实状态。
pub fn current_for(config: &ProviderConfig) -> Option<String> {
    let options = options_for(config);
    let has = |value: &str| options.iter().any(|option| option.value == value);
    if config.reasoning.thinking == Some(false)
        || config.reasoning.effort.as_deref() == Some("none")
    {
        if has("disabled") {
            Some("disabled".to_string())
        } else if has("low") {
            // glm-5.4 的 disabled 实际落到 low；UI 不暴露「关闭」但必须显示有效档位。
            Some("low".to_string())
        } else {
            None
        }
    } else {
        config
            .reasoning
            .effort
            .clone()
            .filter(|effort| !effort.is_empty())
    }
}

/// 从一份 live 配置生成思考强度状态（模型/选项/当前档位必须来自同一快照）。
pub fn state_from_config(config: &ProviderConfig) -> ThinkingState {
    let options = options_for(config);
    ThinkingState {
        supported: !options.is_empty(),
        provider: config.name.clone(),
        model: config.default_model.clone(),
        current: current_for(config),
        options,
    }
}

/// 从 live provider 读取状态；读取失败时返回 unsupported（UI 隐藏，不报错）。
pub fn state_from(provider: &SwappableProvider) -> ThinkingState {
    match provider.current_config() {
        Ok(config) => state_from_config(&config),
        Err(_) => ThinkingState::unsupported(String::new(), String::new()),
    }
}

/// 切换档位后的 reasoning 配置。
///
/// - `disabled`：关闭思考，同时清空 effort（避免服务端仍按旧档位思考）；
/// - `low|high|max`：开启思考 + 对应 effort。
pub fn reasoning_for(value: &str) -> Option<rclaw_llm::ReasoningConfig> {
    match value {
        "disabled" => Some(rclaw_llm::ReasoningConfig {
            thinking: Some(false),
            effort: None,
        }),
        "low" | "high" | "max" => Some(rclaw_llm::ReasoningConfig {
            thinking: Some(true),
            effort: Some(value.to_string()),
        }),
        _ => None,
    }
}
