use std::collections::HashSet;

use serde_json::Value;

use super::effect::{publish_media_source, shell_tokens};
use super::run::ToolReceipt;

/// 显式重试型措辞：无论出现在动作关键词之前还是之后，都把动作定为新鲜工单
/// （「Retry the last deploy」「Run the last deploy again」，codex r11/r12）。
const RETRY_MARKERS: &[&str] = &[
    "retry",
    "retrying",
    "rerun",
    "rerunning",
    "re-run",
    "again",
    "重试",
    "重新",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ExternalAction {
    // Send is intentionally absent: this gate runs before channel delivery and therefore cannot
    // prove that email, another channel, or staged media was actually delivered.
    Create,
    WriteUpdate,
    Delete,
    SourceControl,
    Deploy,
    Install,
}

impl ExternalAction {
    /// 全部动作类别（跨 turn 账本归档回执时按此逐一冻结判定）。
    pub(super) const ALL: [ExternalAction; 6] = [
        ExternalAction::Create,
        ExternalAction::WriteUpdate,
        ExternalAction::Delete,
        ExternalAction::SourceControl,
        ExternalAction::Deploy,
        ExternalAction::Install,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            ExternalAction::Create => "create",
            ExternalAction::WriteUpdate => "write_or_update",
            ExternalAction::Delete => "delete",
            ExternalAction::SourceControl => "source_control",
            ExternalAction::Deploy => "deploy",
            ExternalAction::Install => "install",
        }
    }
}

pub(super) fn claimed_external_actions(text: &str) -> HashSet<ExternalAction> {
    let lower = text.to_ascii_lowercase();
    let mut actions = HashSet::new();
    const CREATE: &[&str] = &[
        "已创建",
        "已经创建",
        "successfully created",
        "created the file",
    ];
    const WRITE_UPDATE: &[&str] = &[
        "已写入",
        "已经写入",
        "已更新",
        "已经更新",
        "已保存",
        "已经保存",
        "已重命名",
        "已经重命名",
        "successfully updated",
        "successfully saved",
        "updated the file",
        "wrote the file",
        "saved the file",
    ];
    const DELETE: &[&str] = &[
        "已删除",
        "已经删除",
        "successfully deleted",
        "deleted the file",
    ];
    const SOURCE_CONTROL: &[&str] = &[
        "已提交代码",
        "已经提交代码",
        "代码已提交",
        "代码已经提交",
        "已提交更改",
        "已经提交更改",
        "更改已提交",
        "变更已提交",
        "已创建提交",
        "已推送代码",
        "已经推送代码",
        "代码已推送",
        "已推送分支",
        "分支已推送",
        "已推送提交",
        "提交已推送",
        "已合并 pr",
        "pr 已合并",
        "已合并拉取请求",
        "拉取请求已合并",
        "已合并分支",
        "分支已合并",
        "committed the changes",
        "pushed the changes",
        "merged the pull request",
    ];
    const DEPLOY: &[&str] = &[
        "已部署",
        "已经部署",
        "已上线",
        "successfully deployed",
        "deployed to production",
    ];
    const INSTALL: &[&str] = &[
        "已安装",
        "已经安装",
        "已卸载",
        "已经卸载",
        "successfully installed",
        "successfully uninstalled",
    ];
    for (patterns, action) in [
        (CREATE, ExternalAction::Create),
        (WRITE_UPDATE, ExternalAction::WriteUpdate),
        (DELETE, ExternalAction::Delete),
        (SOURCE_CONTROL, ExternalAction::SourceControl),
        (DEPLOY, ExternalAction::Deploy),
        (INSTALL, ExternalAction::Install),
    ] {
        if patterns
            .iter()
            .any(|pattern| contains_positive_claim_pattern(&lower, pattern))
        {
            actions.insert(action);
        }
    }
    actions
}

/// 从回复中提取被引用的回执号（格式 r{迭代}-{序}-{指纹}，如 r2-1-9a3bf1c0）。
/// 结构化 Evidence 试点（write/edit 先行）的引用通道：回执号由状态帧下发，
/// 模型引用它 = 精确的 ReportedFact（按 id 绑定真实回执，不经关键词类别猜测）。
pub(super) fn cited_receipt_ids(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'r' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            let start = i;
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let digits1_end = j;
            if j < bytes.len() && bytes[j] == b'-' {
                j += 1;
                let digits2_start = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > digits2_start && j < bytes.len() && bytes[j] == b'-' {
                    j += 1;
                    let fingerprint_start = j;
                    while j < bytes.len() && bytes[j].is_ascii_hexdigit() {
                        j += 1;
                    }
                    // 指纹段 = 恰好 8 位十六进制（short_fingerprint 格式）——
                    // 版本号「r2026-10-24」的 24 只有 2 位，不是引用（codex r39）。
                    if j - fingerprint_start == 8 {
                        out.push(text[start..j].to_string());
                        i = j;
                        continue;
                    }
                }
            }
            i = digits1_end.max(start + 1);
            continue;
        }
        i += 1;
    }
    out
}

pub(super) fn contains_positive_claim_pattern(text: &str, pattern: &str) -> bool {
    text.match_indices(pattern).any(|(index, _)| {
        let prefix = text[..index].chars().rev().take(40).collect::<String>();
        let context = prefix.chars().rev().collect::<String>();
        let context = context.trim_end();
        ![
            "not",
            "did not",
            "didn't",
            "was not",
            "wasn't",
            "failed to",
            "unable to",
            "could not",
            "couldn't",
        ]
        .iter()
        .any(|negation| context.ends_with(negation))
    })
}

pub(super) fn contains_generic_completion_claim(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "已完成",
        "已经完成",
        "完成了",
        "successfully completed",
        "has been completed",
        "is complete",
        "done.",
    ]
    .iter()
    .any(|pattern| contains_positive_claim_pattern(&lower, pattern))
}

pub(super) fn requested_external_actions(goal: &str) -> HashSet<ExternalAction> {
    let lower = goal.to_ascii_lowercase();
    let mut actions = HashSet::new();
    for (patterns, action) in [
        (
            &[
                "创建文件",
                "创建任务",
                "创建提醒",
                "create file",
                "create task",
                "create reminder",
            ][..],
            ExternalAction::Create,
        ),
        (
            &[
                "写入",
                "保存到",
                "更新文件",
                "修改文件",
                "重命名",
                "write file",
                "save file",
                "update file",
                "rename file",
            ][..],
            ExternalAction::WriteUpdate,
        ),
        (
            &["删除文件", "delete file", "remove file"][..],
            ExternalAction::Delete,
        ),
        (
            &[
                "提交代码",
                "推送",
                "合并",
                "commit",
                "push",
                "merge pull request",
            ][..],
            ExternalAction::SourceControl,
        ),
        (
            &["部署", "上线", "deploy", "redeploy", "publish site"][..],
            ExternalAction::Deploy,
        ),
        (
            &["安装", "卸载", "install", "reinstall", "uninstall"][..],
            ExternalAction::Install,
        ),
    ] {
        if patterns
            .iter()
            .any(|pattern| pattern_requested(&lower, pattern))
        {
            actions.insert(action);
        }
    }
    actions
}

/// 单个动作模式的意图判定：扫描它的每一次出现，以【最后一次】出现的语境为准
/// ——后来的否定/回顾（插话撤销、改为总结）压过先前的肯定（codex r23：
/// 「deploy service」+ 插话「Do not deploy; summarize the last deploy instead」
/// 不得再要求本 turn 回执）。出现分类：否定前缀 → 否定；回顾前缀且后缀无重试
/// 标记 → 回顾（不改状态）；其余（含重试后缀）→ 肯定。
/// 单个动作模式是否构成新鲜请求：存在任一「肯定」出现即成立。
/// 出现分类：否定前缀 → 否定；回顾前缀且后缀无重试标记 → 回顾（不计肯定）；
/// 其余（含后缀重试标记）→ 肯定。
///
/// 语义边界（刻意保持简单，codex r23-r28 实证：更精细的最后出现为准/
/// 宾语比对模型每修一个措辞就破两个新的，已回退）：插话撤销
/// （「Do not deploy; summarize instead」）不被识别——此类目标仍要求当轮回执，
/// 模型可以真实执行或用无动作断言的措辞如实汇报；不引入新的误收。
fn pattern_requested(text: &str, pattern: &str) -> bool {
    if pattern.is_ascii() {
        // ASCII 模式整词匹配：uninstall 里的 install 不是 install 请求
        // （codex r39——「Summarize your last uninstall」的误判）。
        let bytes = text.as_bytes();
        return text.match_indices(pattern).any(|(index, _)| {
            let before_ok = index == 0 || !bytes[index - 1].is_ascii_alphanumeric();
            let prefix = text[..index].chars().rev().take(48).collect::<String>();
            let prefix = prefix.chars().rev().collect::<String>();
            let retrospective = request_prefix_is_retrospective(&prefix)
                && !request_suffix_is_retry(text, index + pattern.len());
            // 屈折容差：deploying/committed 等变体仍算命中（codex r44）——
            // ascii_inflected_after 同时承担严格边界与屈折后缀两种词尾判定。
            before_ok
                && ascii_inflected_after(text, index + pattern.len(), pattern)
                && !request_prefix_is_negated(&prefix)
                && !retrospective
        });
    }
    text.match_indices(pattern).any(|(index, _)| {
        let prefix = text[..index].chars().rev().take(48).collect::<String>();
        let prefix = prefix.chars().rev().collect::<String>();
        if request_prefix_is_negated(&prefix) {
            return false;
        }
        if request_prefix_is_retrospective(&prefix)
            && !request_suffix_is_retry(text, index + pattern.len())
        {
            return false;
        }
        true
    })
}

/// 关键词后缀（到下一子句分隔为止）是否带显式重试标记——回顾出现因此翻转为
/// 肯定（「Run the last deploy again」）。
/// ASCII 请求词的屈折容差：词后接 s/es/ed/ing（允许一次双写末辅音，如
/// commit→committing）后到达边界，视为同一动词的变体（codex r44：
/// deploying/committing 仍是请求动词，不得被整词检查误杀）。
fn ascii_inflected_after<'a>(hay: &'a str, end: usize, pattern: &str) -> bool {
    let tail = &hay[end..];
    if tail.is_empty() || !tail.as_bytes()[0].is_ascii_alphanumeric() {
        return true; // 普通词边界
    }
    // 允许一次双写末辅音（commit + t + ing）；tail 保持借用切片，
    // 无 String 重建（codex r45：O(n²) 拷贝已消除）。
    let mut tails: Vec<&'a str> = vec![tail];
    if let Some(last) = pattern.chars().last() {
        if tail.starts_with(last) {
            tails.push(&tail[1..]);
        }
    }
    // 名词派生（deployment/installation）也算动作域提及（codex r45b：
    // 「Finish the deployment」是请求，不得从 requested_actions 脱落）。
    // 循环剥离（≤2 轮）支持复合派生：deploy + ment + s = deployments
    // （codex r46：复数名词派生同样算动作域提及）。
    tails.iter().any(|rest0| {
        let mut rest = *rest0;
        for _ in 0..2 {
            let mut progressed = false;
            for suffix in ["s", "es", "ed", "ing", "ment", "ation"] {
                if let Some(stripped) = rest.strip_prefix(suffix) {
                    rest = stripped;
                    progressed = true;
                    break;
                }
            }
            if !progressed {
                break;
            }
        }
        rest.is_empty() || !rest.as_bytes()[0].is_ascii_alphanumeric()
    })
}

/// ASCII 标记的整词匹配（前后界均非字母数字）。
fn contains_ascii_word(hay: &str, needle: &str) -> bool {
    let bytes = hay.as_bytes();
    let mut start = 0;
    while let Some(offset) = hay[start..].find(needle) {
        let abs = start + offset;
        let after = abs + needle.len();
        let before_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn request_suffix_is_retry(text: &str, from: usize) -> bool {
    let after = &text[from..];
    // 后缀边界与前缀扫描同一套子句分隔（含 ASCII . ! ?——codex r13）。
    // 扫描窗口先截 200 字节再拷贝，防长目标上的二次方开销（codex r46；
    // r50——截断必须先于分隔符查找，否则 find 仍对全后缀线性扫描）。
    // 字节上限必须落在字符边界：无分隔符的 CJK 长后缀会把 200 落进多字节
    // 字符中间，直接切片 panic（codex r49）——向下取整到最近边界。
    let mut window_end = after.len().min(200);
    while !after.is_char_boundary(window_end) {
        window_end -= 1;
    }
    let bounded = &after[..window_end];
    let postfix_end = bounded
        .find([
            '。', '！', '？', '；', ';', '，', ',', '.', '!', '?', '\n', '\r',
        ])
        .unwrap_or(bounded.len());
    let mut postfix = bounded[..postfix_end].to_ascii_lowercase();
    // 软连词切后缀：and/then/然后/并/接着 之后的 again 属于后半句（codex r14）；
    // 「再」不切——把 X 再重新执行 的 重新 属于本动词（codex r21）；「后」只认
    // 多字连词形态（之后/然后/最后/以后/随后），单字会吞掉 后台 的重试标记
    // （codex r20）。
    let mut cut = postfix.len();
    for separator in [
        " and ", " then ", "然后", "之后", "最后", "以后", "随后", "并", "接着",
    ] {
        if let Some(pos) = postfix.find(separator) {
            cut = cut.min(pos);
        }
    }
    postfix.truncate(cut);
    // ASCII 重试标记整词匹配：against 里的 again 不是重试标记（codex r41）。
    RETRY_MARKERS.iter().any(|marker| {
        if marker.is_ascii() {
            contains_ascii_word(&postfix, marker)
        } else {
            postfix.contains(marker)
        }
    })
}

pub(super) fn request_prefix_is_negated(prefix: &str) -> bool {
    let prefix = prefix.trim_end();
    if [
        "不",
        "不通过",
        "不要",
        "不要再",
        "别",
        "别再",
        "禁止",
        "无需",
        "不需要",
        "不能",
        "不可",
        "切勿",
        "请勿",
        "donot",
        "don't",
        "not",
        "never",
    ]
    .iter()
    .any(|negation| prefix.ends_with(negation))
    {
        return true;
    }

    let clause = prefix
        .rsplit([
            '。', '！', '？', '；', ';', '，', ',', '.', '!', '?', '\n', '\r',
        ])
        .next()
        .unwrap_or(prefix);
    [
        "不要",
        "别再",
        "禁止",
        "无需",
        "不需要",
        "不能",
        "不可",
        "切勿",
        "请勿",
        "do not",
        "don't",
        "never",
    ]
    .iter()
    .any(|negation| clause.contains(negation))
}

/// 动作关键词近邻的回顾式标记：「上次 commit」「Summarize your last commit」里的
/// commit 指向既成事实而非新工单。
///
/// 作用域是【当前子句】：中英句读（。！？；，,.!?后）与软连词（and / then / 然后 /
/// 并 / 再 / 接着）都开新子句——「Summarize your last change. Commit the changes.」
/// 后半句是新工单，不得被前半句的 last 压制（codex r6/r7/r8）。
/// 判据：子句里有【报告动词】（summarize/总结/查看…——动作关键词是报告的宾语而非
/// 请求）；或子句【以时间指称收尾】直接邻接关键词——中文时间词（上次/之前…）直接
/// 可用；英文 last/previous 需限定词搭配（your/the last…，r8）——「Make one last
/// commit」的 last 是「最后一次」的新工单，隔着实词的「已经」（把已经写好的报告
/// 提交）也不算回顾（codex r7/r8 收紧）。全部按子串匹配、不分词。
pub(super) fn request_prefix_is_retrospective(prefix: &str) -> bool {
    // 仅 CJK 报告动词参与回顾判定（子句任意位置）：英文报告词与指令共存无法
    // 区分（「Follow the review checklist to deploy」的 review 会让 deploy 被误
    // 压制，codex r10）——英文回顾一律靠「限定词 + 时间词」邻接判定。
    const CJK_REPORTING: &[&str] = &["总结", "汇总", "查看", "看看", "列出", "回顾", "查询"];
    // 不含 之前/此前/以前： deadline 型措辞（「请在今晚之前部署」）里它们是
    // 截止修饰而非回顾指称（codex r18）——宁可让回顾型提问变严格，不让截止型
    // 新工单靠旧回执蒙混。
    const CJK_TIME: &[&str] = &["上次", "上一次", "以往", "过去", "历史", "已经"];
    // 不含不定冠词 a：「Make a last commit」的 a last 是「最后一次」的新工单
    // （codex r16）；定冠词 the 指称既成事实，保留。
    const EN_POSSESSIVES: &[&str] = &[
        "your", "my", "our", "his", "her", "their", "the", "this", "that",
    ];
    const EN_TIME: &[&str] = &["last", "previous", "recent", "past", "earlier"];
    fn last_clause(prefix: &str) -> &str {
        // 中文连词不带空格（「总结上次修改然后提交代码」的 然后 必须切开，r8）。
        // 单字 后 覆盖 然后/之后/最后 等时间连接（「总结后提交」的后一个动作是新工单）。
        const SOFT: &[&str] = &[" and ", " then ", "然后", "并", "再", "接着"];
        let mut cut = 0usize;
        for (index, ch) in prefix.char_indices() {
            if matches!(
                ch,
                '。' | '！' | '？' | '；' | ';' | '，' | ',' | '\n' | '\r' | '.' | '!' | '?' | '后'
            ) {
                cut = index + ch.len_utf8();
            }
        }
        let clause = &prefix[cut..];
        let mut soft_cut = 0usize;
        for separator in SOFT {
            if let Some(pos) = clause.rfind(separator) {
                soft_cut = soft_cut.max(pos + separator.len());
            }
        }
        &clause[soft_cut..]
    }
    let clause = last_clause(prefix);
    let lower = clause.trim_end().to_ascii_lowercase();
    // 显式重试型措辞是新鲜工单：「Retry the last deploy」虽有 the last，动作
    // 仍要求现在执行——时间指称不得压制（codex r11）。ASCII 标记整词匹配：
    // against 里的 again 不是重试标记（codex r42）。
    if RETRY_MARKERS.iter().any(|marker| {
        if marker.is_ascii() {
            contains_ascii_word(&lower, marker)
        } else {
            lower.contains(marker)
        }
    }) {
        return false;
    }
    // 时间词必须与动作关键词直接相邻（子句恰好以时间指称结尾）：「把已经写好的
    // 报告提交」里的 已经 隔着 实词，不算回顾——提交仍是新工单。
    let cjk_time_adjacent = CJK_TIME.iter().any(|marker| lower.ends_with(marker));
    let en_time_adjacent = EN_TIME.iter().any(|time| {
        lower.ends_with(time)
            && lower[..lower.len() - time.len()]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric())
            && lower[..lower.len() - time.len()]
                .trim_end()
                .rsplit(' ')
                .next()
                .is_some_and(|qualifier| EN_POSSESSIVES.contains(&qualifier))
    });
    // CJK 报告动词也要求紧邻关键词（codex r15）：「按查看到的步骤部署」里的
    // 查看 是修饰语，隔着实词——与时间词同一邻接口径。
    let reporting_hit = CJK_REPORTING.iter().any(|marker| lower.ends_with(marker));
    reporting_hit || cjk_time_adjacent || en_time_adjacent
}

pub(super) fn tool_receipt_matches_action(receipt: &ToolReceipt, action: ExternalAction) -> bool {
    let tool = receipt.tool.to_ascii_lowercase();
    let input = serde_json::from_str::<Value>(&receipt.input).unwrap_or(Value::Null);

    if tool == "shell" {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        return shell_command_matches_action(&command, action);
    }
    if tool == "memory" {
        return memory_input_matches_action(&input, action);
    }
    if tool == "skill_manage" {
        let requested = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return match action {
            ExternalAction::Create => requested == "create",
            ExternalAction::WriteUpdate => {
                matches!(requested, "create" | "patch" | "edit" | "write_file")
            }
            ExternalAction::Delete => matches!(requested, "delete" | "remove_file"),
            _ => false,
        };
    }

    if tool == "download_media" {
        // 下载 = 把远程内容写进工作区媒体沙箱（「已下载/已保存」类声明）。
        return matches!(action, ExternalAction::WriteUpdate | ExternalAction::Create);
    }

    let contains_any = |needles: &[&str]| needles.iter().any(|needle| tool.contains(needle));
    match action {
        ExternalAction::Create => {
            matches!(tool.as_str(), "agent" | "task")
                || contains_any(&["create", "add", "write", "upload", "put", "cron_add"])
        }
        ExternalAction::WriteUpdate => {
            contains_any(&["write", "edit", "update", "save", "upload", "put"])
        }
        ExternalAction::Delete => contains_any(&["delete", "remove"]),
        ExternalAction::SourceControl => contains_any(&[
            "commit",
            "push",
            "merge",
            "create_pull_request",
            "create_pr",
        ]),
        ExternalAction::Deploy => contains_any(&["deploy", "publish"]),
        ExternalAction::Install => contains_any(&["install", "uninstall"]),
    }
}

pub(super) fn memory_input_matches_action(input: &Value, action: ExternalAction) -> bool {
    let matches_operation = |operation: &Value| {
        let requested = operation
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match action {
            ExternalAction::WriteUpdate => matches!(requested, "add" | "replace"),
            ExternalAction::Delete => requested == "remove",
            _ => false,
        }
    };
    if let Some(operations) = input.get("operations").and_then(Value::as_array) {
        return operations.iter().any(&matches_operation);
    }
    matches_operation(input)
}

pub(super) fn shell_command_matches_action(command: &str, action: ExternalAction) -> bool {
    let contains_any = |needles: &[&str]| needles.iter().any(|needle| command.contains(needle));
    match action {
        ExternalAction::Create => {
            contains_any(&[
                "mkdir ",
                "mkdir\n",
                "md ",
                "new-item",
                "touch ",
                "create table",
                "create file",
            ]) || shell_command_writes_file(command)
        }
        ExternalAction::WriteUpdate => {
            shell_command_writes_file(command)
                || contains_any(&[
                    "set-content",
                    "add-content",
                    "copy-item",
                    "move-item",
                    "sed -i",
                    "perl -pi",
                ])
        }
        ExternalAction::Delete => contains_any(&[
            "remove-item",
            "rm ",
            "rm\t",
            "rmdir ",
            "del ",
            "erase ",
            "unlink ",
            "drop table",
        ]),
        ExternalAction::SourceControl => shell_has_source_control_mutation(command),
        ExternalAction::Deploy => {
            publish_media_source(command).is_none()
                && contains_any(&[
                    " deploy", "deploy ", " publish", "publish ", "vercel ", "netlify ",
                ])
        }
        ExternalAction::Install => contains_any(&[
            " install ",
            " uninstall ",
            "npm i ",
            "npm add ",
            "npm remove ",
            "cargo add ",
            "cargo install ",
            "cargo uninstall ",
            "pip install ",
            "pip uninstall ",
        ]),
    }
}

pub(super) fn shell_has_source_control_mutation(command: &str) -> bool {
    let tokens = shell_tokens(command);
    tokens
        .split(|token| matches!(token.as_str(), ";" | "&" | "|"))
        .any(|segment| {
            let tokens = segment
                .iter()
                .map(|token| token.trim_matches(['(', ')']))
                .filter(|token| !token.is_empty())
                .collect::<Vec<_>>();
            if let Some(git_index) = shell_program_index(&tokens, "git") {
                let git_args = &tokens[git_index + 1..];
                return git_subcommand(git_args).is_some_and(|mutation| {
                    matches!(mutation, "commit" | "push" | "merge")
                        && !git_args.contains(&"--dry-run")
                        && !(mutation == "push" && git_args.contains(&"-n"))
                });
            }
            shell_program_index(&tokens, "gh").is_some_and(|gh_index| {
                let gh_args = &tokens[gh_index + 1..];
                !gh_args.contains(&"--dry-run")
                    && gh_subcommands(gh_args).is_some_and(|(group, action)| {
                        group == "pr" && matches!(action, "create" | "merge")
                    })
            })
        })
}

pub(super) fn git_subcommand<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut index = 0;
    while let Some(argument) = args.get(index).copied() {
        if argument == "--" {
            return args.get(index + 1).copied();
        }
        if !argument.starts_with('-') || argument == "-" {
            return Some(argument);
        }
        let option_takes_value = matches!(
            argument,
            "-C" | "-c"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--super-prefix"
                | "--config-env"
        );
        index += if option_takes_value { 2 } else { 1 };
    }
    None
}

pub(super) fn gh_subcommands<'a>(args: &[&'a str]) -> Option<(&'a str, &'a str)> {
    let mut index = 0;
    while let Some(argument) = args.get(index).copied() {
        if argument == "--" {
            index += 1;
            break;
        }
        if !argument.starts_with('-') || argument == "-" {
            break;
        }
        let option_takes_value = matches!(argument, "-R" | "--repo" | "--hostname");
        index += if option_takes_value { 2 } else { 1 };
    }
    Some((*args.get(index)?, *args.get(index + 1)?))
}

pub(super) fn shell_program_index(tokens: &[&str], program: &str) -> Option<usize> {
    let mut index = 0;
    while tokens
        .get(index)
        .is_some_and(|token| token.contains('=') && !token.starts_with('-'))
    {
        index += 1;
    }
    let wrapper = tokens.get(index).map(|token| token.trim_start_matches('@'));
    if wrapper == Some("command") {
        index += 1;
        if tokens
            .get(index)
            .is_some_and(|option| matches!(*option, "-v" | "-V"))
        {
            return None;
        }
        while tokens
            .get(index)
            .is_some_and(|token| token.starts_with('-'))
        {
            index += 1;
        }
    } else if wrapper == Some("call") {
        index += 1;
    } else if wrapper == Some("env") {
        index += 1;
        while let Some(argument) = tokens.get(index).copied() {
            if argument.contains('=') && !argument.starts_with('-') {
                index += 1;
                continue;
            }
            if !argument.starts_with('-') || argument == "-" {
                break;
            }
            let takes_value = matches!(
                argument,
                "-u" | "--unset" | "-C" | "--chdir" | "-S" | "--split-string"
            );
            index += if takes_value { 2 } else { 1 };
        }
    } else if wrapper == Some("sudo") {
        index += 1;
        while let Some(option) = tokens.get(index).copied() {
            if !option.starts_with('-') || option == "-" {
                break;
            }
            let takes_value = matches!(
                option,
                "-u" | "--user"
                    | "-g"
                    | "--group"
                    | "-h"
                    | "--host"
                    | "-p"
                    | "--prompt"
                    | "-C"
                    | "--chdir"
                    | "-T"
                    | "--command-timeout"
            );
            index += if takes_value { 2 } else { 1 };
        }
    }
    let token = tokens.get(index)?.trim_start_matches('@');
    let executable = token.rsplit(['/', '\\']).next().unwrap_or(token);
    (executable == program || executable.strip_suffix(".exe") == Some(program)).then_some(index)
}

pub(super) fn shell_command_writes_file(command: &str) -> bool {
    let chars = command.char_indices().collect::<Vec<_>>();
    let mut single_quoted = false;
    let mut double_quoted = false;
    let mut escaped = false;
    for (index, (_, ch)) in chars.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        let is_shell_escape = if cfg!(windows) { ch == '^' } else { ch == '\\' };
        if is_shell_escape && !single_quoted {
            escaped = true;
            continue;
        }
        if ch == '\'' && !cfg!(windows) && !double_quoted {
            single_quoted = !single_quoted;
            continue;
        }
        if ch == '"' && !single_quoted {
            double_quoted = !double_quoted;
            continue;
        }
        if ch != '>' || single_quoted || double_quoted {
            continue;
        }
        let next = chars.get(index + 1).map(|(_, ch)| *ch);
        // `>&1` / `2>&1` only duplicates a file descriptor, while `>=` is
        // an expression operator in commands such as Python snippets.
        if !matches!(next, Some('&' | '=')) {
            return true;
        }
    }
    false
}
