//! 路由策略模块 — 根据内容类型、意图、策略推断投影计划（设计文档 §6）
//!
//! 输入：content/path/url/text, intent, target_slug, file extension, mime type, caller context
//! 输出：to_kb, to_brain_shadow, to_brain_page, to_file, extract_links, extract_timeline,
//!       generate_review_changes, auto_apply_policy

use crate::artifact::types::{RoutePlan, UploadIntent};
use tracing::debug;

/// 根据意图、扩展名和 MIME 类型推断路由计划
///
/// 设计文档 §6.2 路由规则:
/// | 条件                                  | Artifact | KB | Shadow | File | Promotion |
/// |--------------------------------------|----------|----|--------|------|-----------|
/// | --intent attachment                  | yes      | no | no     | yes  | none      |
/// | --intent evidence                    | yes      | yes| yes    | no   | candidate |
/// | --intent memory                      | yes      | yes| yes    | no   | auto-low  |
/// | --intent promote                     | yes      | yes| yes    | no   | candidate |
/// | PDF/DOCX/XLS/XLSX/CSV/HTML/TXT auto | yes      | yes| yes    | no   | candidate |
/// | Raw Markdown with auto               | yes      | yes| yes    | no   | candidate |
/// | Markdown with gbrain frontmatter     | optional | no | no     | no   | direct put_page |
/// | Code file/repo with auto             | optional | no | no     | no   | code import/sync |
/// | Image/binary with auto               | yes      | no | no     | yes  | none      |
pub fn infer_route_plan(extension: &str, mime: &str, intent: &UploadIntent) -> RoutePlan {
    // 委托给 types.rs 中已有的 infer_route_plan 函数
    crate::artifact::types::infer_route_plan(extension, mime, intent)
}

/// 根据用户友好的 ArtifactIntent 推断路由计划
///
/// manual=true 时表示手动 put（直接写稳定页面），
/// manual=false 时表示上传文件（走 shadow 投影）。
pub fn infer_route_plan_from_artifact_intent(
    extension: &str,
    mime: &str,
    intent_str: &str,
    manual: bool,
) -> Result<RoutePlan, String> {
    // 先通过 FromStr 规范化 intent，再分支处理。
    // 修复前先做原始字符串比较，导致 FromStr 接受的别名（如 document）和
    // 大小写变体（如 Memory）绕过手动 put 路由，错误落入上传路由。
    if manual {
        let intent: UploadIntent = intent_str
            .parse()
            .map_err(|e| format!("手动 put 不支持该 intent: {}", e))?;
        match intent {
            UploadIntent::Memory => {
                return Ok(RoutePlan {
                    to_kb: true, // 由 config.artifact_manual_memory_to_kb 控制
                    to_brain: true,
                    to_shadow: false,
                    to_file: false,
                    promotion: crate::artifact::types::PromotionPolicy::AutoAcceptLowRisk,
                });
            }
            UploadIntent::Document => {
                // evidence 等效，仅 KB 证据，不写稳定页面
                return Ok(RoutePlan {
                    to_kb: true,
                    to_brain: false,
                    to_shadow: false,
                    to_file: false,
                    promotion: crate::artifact::types::PromotionPolicy::Candidate,
                });
            }
            UploadIntent::Promote => {
                return Ok(RoutePlan {
                    to_kb: true,
                    to_brain: true,
                    to_shadow: true,
                    to_file: false,
                    promotion: crate::artifact::types::PromotionPolicy::Candidate,
                });
            }
            // Auto/Attachment 等对手动 put 无意义
            other => {
                return Err(format!(
                    "手动 put 不支持 intent={}，有效值: memory/evidence/promote",
                    other
                ));
            }
        }
    }
    // 非手动 put（上传文件），走上传路由
    let intent: UploadIntent = intent_str.parse()?;
    let plan = infer_route_plan(extension, mime, &intent);
    debug!("infer_route_plan_from_artifact_intent: ext={}, mime={}, intent={}, manual={}, to_kb={} to_brain={} to_shadow={} to_file={} promotion={}",
        extension, mime, intent_str, manual, plan.to_kb, plan.to_brain, plan.to_shadow, plan.to_file, plan.promotion);
    Ok(plan)
}
