use super::*;

/// 无媒体参与的普通文本：闸门应直接通过三层验证。
#[test]
fn plain_text_without_media_claims_passes() {
    let mut run = Run::new("回答一个问题", 8);
    let mut markers = Vec::new();

    let (finalized, gate) = run.evaluate_final_gate(
        "答案是 42。",
        None,
        &mut markers,
        "cli",
        &rclaw_core::RuntimeChannelInfo::default(),
        false,
    );

    assert!(gate.accepted());
    assert_eq!(finalized, "答案是 42。");
}

/// 文本含媒体控制行且声称已交付，但没有任何已验证 marker：
/// 即使通道不支持媒体，控制行本身也构成「声称已发送」的触发条件，必须拒绝。
#[test]
fn media_control_line_without_validated_marker_is_rejected() {
    let mut run = Run::new("发送报告", 8);
    let mut markers = Vec::new();
    let text = "报告已发送到你的微信。\n[:napcat-media type=file path=x]";

    let (_, gate) = run.evaluate_final_gate(
        text,
        None,
        &mut markers,
        "napcat",
        &rclaw_core::RuntimeChannelInfo::default(),
        false,
    );

    assert!(!gate.accepted());
    let reason = gate.reason_text();
    assert!(
        reason.contains("媒体交付验证失败"),
        "应走媒体认领闸门拒绝：{reason}"
    );
    assert!(reason.contains("声称已交付"), "不得夸大发送数量：{reason}");
}

/// reasoning 参与验证文本：text 本身无声明但 reasoning 声称了外部动作（写入）且无回执，
/// 应被结果验证拒绝——与拆分前 evaluate_reply_completion_with_reasoning 语义一致。
#[test]
fn reasoning_claim_without_receipt_is_rejected() {
    let mut run = Run::new("写入报告文件", 8);
    let mut markers = Vec::new();

    let (_, gate) = run.evaluate_final_gate(
        "完成。",
        Some("报告已写入文件。"),
        &mut markers,
        "cli",
        &rclaw_core::RuntimeChannelInfo::default(),
        false,
    );

    assert!(!gate.accepted());
    assert!(
        gate.reason_text().contains("结果验证失败"),
        "外部动作声明无回执应被结果验证拒绝：{}",
        gate.reason_text()
    );
}

/// 空文本（剥离 marker 后为空）走媒体交付验证分支拒绝。
#[test]
fn empty_text_is_rejected() {
    let mut run = Run::new("回答一个问题", 8);
    let mut markers = Vec::new();

    let (_, gate) = run.evaluate_final_gate(
        "",
        None,
        &mut markers,
        "cli",
        &rclaw_core::RuntimeChannelInfo::default(),
        false,
    );

    assert!(!gate.accepted());
    assert!(
        gate.reason_text().contains("媒体交付验证失败"),
        "空最终回复必须被拒绝：{}",
        gate.reason_text()
    );
}
