//! Helpers for OpenAI-compatible chat completion requests.

/// Some chat APIs enable thinking by default. Most gbrain call sites expect the
/// final answer in `choices[0].message.content`, JSON, or tool calls, so disable
/// thinking unless a caller explicitly builds a reasoning request.
pub fn is_deepseek_chat_api(base_url: &str, model: &str) -> bool {
    base_url.to_ascii_lowercase().contains("api.deepseek.com")
        || model.to_ascii_lowercase().starts_with("deepseek-")
}

pub fn is_glm_thinking_chat_api(_base_url: &str, model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("glm")
}

pub fn should_disable_thinking_for_chat(base_url: &str, model: &str) -> bool {
    is_deepseek_chat_api(base_url, model) || is_glm_thinking_chat_api(base_url, model)
}

pub fn apply_deepseek_chat_options(body: &mut serde_json::Value, base_url: &str, model: &str) {
    if !should_disable_thinking_for_chat(base_url, model) {
        return;
    }

    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "thinking".to_string(),
            serde_json::json!({ "type": "disabled" }),
        );
    }
}

pub fn terminal_finish_reason(data: &serde_json::Value) -> Option<&str> {
    let reason = data
        .get("choices")?
        .as_array()?
        .first()?
        .get("finish_reason")?
        .as_str()?;

    match reason {
        "length" | "content_filter" | "insufficient_system_resource" => Some(reason),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn disables_thinking_for_glm_models() {
        assert!(super::should_disable_thinking_for_chat(
            "https://open.bigmodel.cn/api/coding/paas/v4",
            "glm-5.2"
        ));
        assert!(super::should_disable_thinking_for_chat(
            "https://open.bigmodel.cn/api/paas/v4",
            "GLM-4.5"
        ));
    }

    #[test]
    fn applies_disabled_thinking_to_glm_body() {
        let mut body = serde_json::json!({
            "model": "glm-5.2",
            "messages": []
        });

        super::apply_deepseek_chat_options(
            &mut body,
            "https://open.bigmodel.cn/api/coding/paas/v4",
            "glm-5.2",
        );

        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn leaves_regular_openai_models_unchanged() {
        let mut body = serde_json::json!({
            "model": "gpt-4o-mini",
            "messages": []
        });

        super::apply_deepseek_chat_options(&mut body, "https://api.openai.com/v1", "gpt-4o-mini");

        assert!(body.get("thinking").is_none());
    }
}
