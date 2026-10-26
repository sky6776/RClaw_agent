use super::*;

/// 规范化可用于媒体标记的外部 channel id。
///
/// channel 会同时进入 marker 前缀和环境变量名，因此只接受有界 ASCII token，
/// 避免来源文本破坏 marker 语法或构造出意外的能力变量名。
pub(super) fn external_media_channel_id(channel: &str) -> Option<String> {
    let mut channel = channel.trim().to_ascii_lowercase();
    // 防御性旧名映射：技能脚本/历史消息可能回传 napcatqq，与 ChannelCaps 的规范化名对齐。
    // 适配器在接线期的 ChannelCaps.name 已是规范名，这里只兜底来源文本里的旧别名。
    if channel == "napcatqq" {
        channel = CHANNEL_NAPCAT.to_string();
    }
    let mut bytes = channel.bytes();
    if channel.len() > 64
        || matches!(channel.as_str(), "" | CHANNEL_CLI | CHANNEL_SYSTEM)
        || !bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Some(channel)
}

/// 当前 channel 是否声明了媒体能力。动态变量避免把 NapCatQQ/微信写死在分支里；
/// 将来新增 channel 只需声明能力（经 ChannelCaps）；`RCLAW_MEDIA_DIR` 仅作为
/// 子进程契约导出（见 rclaw-cli/main.rs），runtime 内部不读。
pub(crate) fn media_channel_available(
    channel: &str,
    info: &rclaw_core::RuntimeChannelInfo,
) -> bool {
    let Some(channel) = external_media_channel_id(channel) else {
        return false;
    };
    info.caps(&channel).is_some_and(|caps| caps.media)
}

/// 从成功工具 stdout 中提取可由当前 channel 实际消费的媒体回执。
///
/// 以前把 `MEDIA_MARKER=` 留给模型手工复制；模型即使已正确运行发布器，也可能在最终
/// 回复里漏掉 marker，形成“文件已暂存、文字声称已发送、通道却从未收到发送指令”的
/// 断链。这里把回执提升为运行时状态，但仍同时校验 channel、语法、真实文件和共享
/// media 沙箱，避免把普通工具输出、跨通道 marker 或伪造路径自动变成外发动作。
pub(crate) fn validated_media_markers_from_tool_result(
    content: &str,
    channel: &str,
    info: &rclaw_core::RuntimeChannelInfo,
) -> Vec<String> {
    // Shell 与 Manifest 动态工具都使用同一 stdout/stderr 分段。回执协议只允许
    // 发布器 stdout；若扫描整段文本，命令参数、包装元数据或子进程 stderr 都可能
    // 注入一条看似合法的 marker。
    let Some(stdout) = shell_stdout_content(content) else {
        return Vec::new();
    };
    validated_media_markers_from_runtime_content(stdout, channel, info)
}

pub(super) fn shell_stdout_content(content: &str) -> Option<&str> {
    content
        .split_once("\n[stdout]\n")
        .map(|(_, remainder)| remainder)
        .and_then(|remainder| {
            remainder
                .split_once("\n[stderr]\n")
                .map(|(stdout, _)| stdout)
        })
}

pub(super) fn validated_media_markers_from_runtime_content(
    content: &str,
    channel: &str,
    info: &rclaw_core::RuntimeChannelInfo,
) -> Vec<String> {
    if !media_channel_available(channel, info) {
        return Vec::new();
    }
    let Some(media_dir) = info
        .media_dir
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    else {
        return Vec::new();
    };
    validated_media_markers(content, channel, &media_dir)
}

/// 从 Agent/Task 及其等待工具返回的运行时封装中恢复已验证媒体回执。
///
/// `RCLAW_VALIDATED_MEDIA_MARKER=` 只由子 Agent 运行时在真实发布器成功后写入；模型正文
/// 中的普通 `MEDIA_MARKER=` 不作为跨 Agent 凭据，避免“把一段文本转发一圈”绕过门禁。
pub(crate) fn validated_media_markers_from_agent_result(
    content: &str,
    channel: &str,
    info: &rclaw_core::RuntimeChannelInfo,
) -> Vec<String> {
    let tool_output = content
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("RCLAW_VALIDATED_MEDIA_MARKER=")
                .map(|marker| format!("MEDIA_MARKER={marker}"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    validated_media_markers_from_runtime_content(&tool_output, channel, info)
}

pub(crate) fn agent_result_reports_media_publish_attempt(content: &str) -> bool {
    content.lines().any(|line| {
        matches!(
            line.trim(),
            "RCLAW_MEDIA_PUBLISH_ATTEMPTED=1"
                | "RCLAW_VALIDATED_MEDIA_MARKER="
                | "MEDIA_MARKER="
                | "ARTIFACT="
        ) || line.trim().starts_with("RCLAW_VALIDATED_MEDIA_MARKER=")
            || line.trim().starts_with("MEDIA_MARKER=")
            || line.trim().starts_with("ARTIFACT=")
    })
}

/// 在最终出站前重新验证待投递 marker。
///
/// marker 首次出现后，后续工具仍可能删除文件或把路径改成越过沙箱的符号链接。通道真正
/// 读取文件发生在 turn 结束之后，因此不能把早先的一次校验永久当成发送凭据。
pub(crate) fn revalidated_pending_media_markers(
    markers: &[String],
    channel: &str,
    info: &rclaw_core::RuntimeChannelInfo,
) -> Vec<String> {
    let content = markers
        .iter()
        .map(|marker| format!("MEDIA_MARKER={marker}"))
        .collect::<Vec<_>>()
        .join("\n");
    validated_media_markers_from_runtime_content(&content, channel, info)
}

pub(super) fn validated_media_markers(
    content: &str,
    channel: &str,
    media_dir: &Path,
) -> Vec<String> {
    let Some(channel) = external_media_channel_id(channel) else {
        return Vec::new();
    };
    let prefix = format!("[:{channel}-media");
    let mut markers = Vec::new();
    let mut seen_paths = HashSet::new();

    for line in content.lines() {
        let Some(marker) = line.trim().strip_prefix("MEDIA_MARKER=") else {
            continue;
        };
        if marker.chars().any(char::is_control) {
            continue;
        }
        let Some(rest) = marker.strip_prefix(&prefix) else {
            continue;
        };
        // 防止 `[:napcat-media-forged ...]` 命中 `[:napcat-media` 的前缀。
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(inner) = rest.strip_suffix(']') else {
            continue;
        };
        if inner.contains(['[', ']']) {
            continue;
        }
        let Some(attributes) = parse_media_marker_attributes(inner) else {
            continue;
        };
        if !matches!(
            attributes.get("type").map(String::as_str),
            Some("file" | "image" | "video" | "voice")
        ) {
            continue;
        }
        let Some(path) = attributes.get("path") else {
            continue;
        };
        let Some(resolved_path) = canonical_sendable_media_path(media_dir, path) else {
            continue;
        };
        // 同一路径仅因 caption/name 不同而出现多条 marker 时仍只是一个文件，不能
        // 被计成多份附件来满足“两份报告已发送”之类的数量声明。
        if seen_paths.insert(resolved_path) {
            markers.push(marker.to_string());
        }
    }
    markers
}

pub(super) fn parse_media_marker_attributes(input: &str) -> Option<HashMap<String, String>> {
    let mut chars = input.chars().peekable();
    let mut attributes = HashMap::new();
    loop {
        while chars.next_if(|ch| ch.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return Some(attributes);
        }

        let mut key = String::new();
        while let Some(ch) = chars.next_if(|ch| !ch.is_whitespace() && *ch != '=') {
            key.push(ch);
        }
        if key.is_empty() || chars.next() != Some('=') {
            return None;
        }

        let mut value = String::new();
        if chars.next_if_eq(&'"').is_some() {
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some(ch) if !ch.is_control() => value.push(ch),
                    _ => return None,
                }
            }
            if chars.peek().is_some_and(|ch| !ch.is_whitespace()) {
                return None;
            }
        } else {
            while let Some(ch) = chars.next_if(|ch| !ch.is_whitespace()) {
                if ch.is_control() {
                    return None;
                }
                value.push(ch);
            }
        }
        if value.is_empty() || attributes.insert(key, value).is_some() {
            return None;
        }
    }
}

pub(super) fn canonical_sendable_media_path(media_dir: &Path, candidate: &str) -> Option<PathBuf> {
    if media_dir.as_os_str().is_empty() || candidate.is_empty() {
        return None;
    }
    let base = PathBuf::from(media_dir.to_string_lossy().replace('\\', "/"));
    let raw = PathBuf::from(candidate.replace('\\', "/"));
    if raw
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    // 各外部 channel 共享同一媒体沙箱，也必须共享同一条路径契约。尤其微信只接受
    // marker 中词法上已位于 media_dir 下的路径；若运行时额外放行 `out/x.pdf`
    // 并自行拼接 media_dir，就会形成“验证通过、通道拒绝”的假回执。
    // 词法前缀不一致并不总意味着越区：Android 上 /data/user/0 是 /data/data 的
    // 符号链接，发布脚本一旦 resolve() 了沙箱路径，marker 前缀就与 media_dir
    // 词法分叉（物理仍在沙箱内）。因此词法命中走快路径；未命中时以 canonicalize
    // 后的物理包含关系兜底，真实安全边界始终是下面的 canonical 前缀校验。
    let Ok(canonical_base) = base.canonicalize() else {
        return None;
    };
    raw.canonicalize().ok().filter(|resolved| {
        resolved.starts_with(&canonical_base)
            && resolved
                .metadata()
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
    })
}

pub(crate) fn is_media_control_line(line: &str) -> bool {
    let value = line.trim_start();
    // `MEDIA_MARKER=` 是工具与运行时之间的保留协议。即使后面的 marker 格式错误，
    // 也不能把内部协议行当普通正文展示给用户。
    if value.starts_with("MEDIA_MARKER=")
        || value.starts_with("RCLAW_VALIDATED_MEDIA_MARKER=")
        || value.starts_with("RCLAW_MEDIA_PUBLISH_ATTEMPTED=")
    {
        return true;
    }
    let Some(rest) = value.strip_prefix("[:") else {
        return false;
    };
    rest.split(|ch: char| ch.is_whitespace() || ch == ']')
        .next()
        // 整个 `*-media*` 命名空间均保留给运行时。通道解析器历史上曾错误接受
        // `[:weixin-media-forged ...]` 这类前缀变体；若这里只清理精确 `-media`
        // 结尾，模型手写变体就可能绕过运行时验证后被下游通道消费。
        .is_some_and(|name| name.contains("-media"))
}

/// 只剥离媒体控制行，不回填 marker：完成闸门对【模型正文】做声明、回执号引用与
/// 泄露帧检查时用——已验证 marker 是运行时附加的交付物，其内容（如文件名
/// `r9-1-….md`）不得参与回执号解析，否则 cited_ids 非空会把 result 通道收窄为
/// 只认被引用回执，类回执 token 会过滤掉真实成功凭据，制造误拒（codex 第 2 轮）。
pub(crate) fn strip_media_control_lines(text: &str) -> String {
    text.lines()
        .filter(|line| !is_media_control_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 只保留运行时从真实成功工具结果中验证过的 marker，并自动补到最终回复。
///
/// 这既消除了模型漏抄回执的问题，也让模型手写、从历史复制或写错 channel 的 marker
/// 无法绕过当前 turn 的工具结果与沙箱校验。
pub(crate) fn finalize_media_reply(text: &str, markers: &[String]) -> String {
    let mut reply = strip_media_control_lines(text);
    let mut seen = HashSet::new();
    for marker in markers {
        if !seen.insert(marker) {
            continue;
        }
        if !reply.is_empty() {
            reply.push('\n');
        }
        reply.push_str(marker);
    }
    reply
}

pub(crate) fn claims_media_delivery(text: &str) -> bool {
    required_media_markers_for_claim(text) > 0
}

pub(crate) fn required_media_markers_for_claim(text: &str) -> usize {
    let lower = text.to_ascii_lowercase();
    let failure_phrases = [
        "发送失败",
        "未发送",
        "没有发送",
        "没发送",
        "未能发送",
        "无法发送",
        "不发送",
        "不再发送",
        "不会发送",
        "不会再发送",
        "不是已发送",
        "没有成功发送",
        "没有发送成功",
        "not sent",
        "not been sent",
        "failed to send",
        "could not send",
        "unable to send",
        "will not send",
        "won't send",
    ];
    let uncertainty_phrases = [
        "是否已发送",
        "是否已经发送",
        "有没有发送",
        "无法确认",
        "不能确认",
        "尚不确定",
        "不确定是否",
        "whether it was sent",
        "whether the file was sent",
        "cannot confirm",
        "can't confirm",
        "not sure whether",
    ];
    let quoted_or_meta_phrases = [
        "agent说",
        "agent 说",
        "agent声称",
        "agent 声称",
        "日志中",
        "日志显示",
        "错误地声称",
        "误称",
        "表述错误",
        "说法错误",
        "claimed",
        "the log says",
        "the log shows",
    ];
    let later_success_phrases = [
        "失败后已发送",
        "失败后已经发送",
        "重试后已发送",
        "重试后已经发送",
        "随后已发送",
        "随后已上传",
        "最终已发送",
        "最终发送成功",
        "最终已上传",
        "最终已投递",
        "但已发送",
        "但已经发送",
        "重试成功",
        "重试后已上传",
        "finally sent",
        "then sent",
        "but was sent",
        "sent after retry",
        "retry succeeded",
    ];
    let media_phrases = [
        "文件",
        "附件",
        "报告",
        "图片",
        "图像",
        "照片",
        "视频",
        "音频",
        "语音",
        "qq",
        "微信",
        "file",
        "attachment",
        "report",
        "image",
        "photo",
        "video",
        "audio",
    ];
    let success_phrases = [
        "已发送",
        "已经发送",
        "发送成功",
        "已发到",
        "已经发到",
        "已发给",
        "已经发给",
        "发给你了",
        "发给您了",
        "给你发了",
        "给您发了",
        "发过去了",
        "已上传",
        "已经上传",
        "已投递",
        "已经投递",
        "已附上",
        "已经附上",
        "请查收",
        "见附件",
        "通过qq发",
        "通过微信发",
        "发到qq",
        "发到微信",
        "has been sent",
        "have been sent",
        "was sent",
        "were sent",
        "successfully sent",
        "sent successfully",
        "sent you",
        "sent the file",
        "sent the report",
        "sent the attachment",
        "attached the file",
        "please find attached",
    ];
    let hedged_delivery_claim = |value: &str| {
        let chinese_hedge = ["应该", "可能", "大概"]
            .iter()
            .any(|needle| value.contains(needle));
        let chinese_delivery = [
            "已发送",
            "已经发送",
            "已发到",
            "已经发到",
            "已发给",
            "已经发给",
            "已上传",
            "已经上传",
            "已投递",
            "已经投递",
            "已附上",
            "已经附上",
        ]
        .iter()
        .any(|needle| value.contains(needle));
        (chinese_hedge && chinese_delivery)
            || [
                "should have been sent",
                "probably sent",
                "may have been sent",
            ]
            .iter()
            .any(|needle| value.contains(needle))
    };

    // 按标点和转折词逐句判断，不能让“一份文件发送失败”把同一回复中“另一份已发送”
    // 的成功声明整体豁免，也不能把“报告已生成，普通文字消息已发送”拼成媒体成功。
    let clauses = re_split_media_claim_clauses(&lower);
    let mut required_markers = 0;
    for clause in clauses {
        let reports_failure = failure_phrases.iter().any(|needle| clause.contains(needle));
        let reports_uncertainty = uncertainty_phrases
            .iter()
            .any(|needle| clause.contains(needle));
        let is_quoted_or_meta = quoted_or_meta_phrases
            .iter()
            .any(|needle| clause.contains(needle));
        let mentions_media = media_phrases.iter().any(|needle| clause.contains(needle));
        let claims_success = success_phrases.iter().any(|needle| clause.contains(needle))
            || hedged_delivery_claim(clause);
        if mentions_media
            && claims_success
            && !reports_failure
            && !reports_uncertainty
            && !is_quoted_or_meta
        {
            required_markers = required_markers.max(explicit_media_count(clause).unwrap_or(1));
        }
    }

    // “报告发送失败但最终已发送”可能把媒体名只写在转折前；显式的重试/最终成功
    // 仍视为成功声明，而单纯失败说明继续允许诚实降级。
    let later_success = media_phrases.iter().any(|needle| lower.contains(needle))
        && !uncertainty_phrases
            .iter()
            .any(|needle| lower.contains(needle))
        && !quoted_or_meta_phrases
            .iter()
            .any(|needle| lower.contains(needle))
        && later_success_phrases
            .iter()
            .any(|needle| lower.contains(needle));
    if later_success {
        required_markers = required_markers.max(explicit_media_count(&lower).unwrap_or(1));
    }
    // “文件已经发布，应该已经发给你”会被标点拆开，媒体名与模糊交付声明
    // 不一定在同一分句；这种说法仍暗示交付成功，必须有真实 marker 支撑。
    let hedged_delivery = media_phrases.iter().any(|needle| lower.contains(needle))
        && !failure_phrases.iter().any(|needle| lower.contains(needle))
        && !uncertainty_phrases
            .iter()
            .any(|needle| lower.contains(needle))
        && !quoted_or_meta_phrases
            .iter()
            .any(|needle| lower.contains(needle))
        && hedged_delivery_claim(&lower);
    if hedged_delivery {
        required_markers = required_markers.max(explicit_media_count(&lower).unwrap_or(1));
    }
    required_markers
}

pub(super) fn explicit_media_count(text: &str) -> Option<usize> {
    let compact = text
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    let media_nouns = [
        "份报告",
        "个报告",
        "份文件",
        "个文件",
        "份附件",
        "个附件",
        "张图片",
        "张图像",
        "张照片",
        "段视频",
        "个视频",
        "段音频",
        "个音频",
        "条语音",
        "reports",
        "files",
        "attachments",
        "images",
        "photos",
        "videos",
        "audiofiles",
    ];
    media_nouns
        .iter()
        .filter_map(|noun| {
            compact
                .match_indices(noun)
                .filter_map(|(index, _)| trailing_count(&compact[..index]))
                .max()
        })
        .max()
}

pub(super) fn trailing_count(prefix: &str) -> Option<usize> {
    let digits = prefix
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    if !digits.is_empty() {
        let count_start = prefix.len() - digits.len();
        if prefix[..count_start].ends_with('第') {
            return None;
        }
        return digits.parse().ok().filter(|count| *count > 0);
    }
    if let Some((_, ordinal)) = prefix.rsplit_once('第') {
        if !ordinal.is_empty()
            && ordinal
                .chars()
                .all(|ch| "一二两三四五六七八九十百".contains(ch))
        {
            return None;
        }
    }
    [
        ("twenty", 20),
        ("twelve", 12),
        ("eleven", 11),
        ("three", 3),
        ("seven", 7),
        ("eight", 8),
        ("four", 4),
        ("five", 5),
        ("nine", 9),
        ("six", 6),
        ("two", 2),
        ("ten", 10),
        ("二十", 20),
        ("十二", 12),
        ("十一", 11),
        ("两", 2),
        ("二", 2),
        ("三", 3),
        ("四", 4),
        ("五", 5),
        ("六", 6),
        ("七", 7),
        ("八", 8),
        ("九", 9),
        ("十", 10),
    ]
    .into_iter()
    .find_map(|(word, count)| {
        prefix
            .strip_suffix(word)
            .filter(|before| !before.ends_with('第'))
            .map(|_| count)
    })
}

pub(super) fn re_split_media_claim_clauses(text: &str) -> Vec<&str> {
    text.split(['\n', '。', '！', '？', '；', ';', '，', ',', '!', '?'])
        .flat_map(|clause| clause.split("但是"))
        .flat_map(|clause| clause.split('但'))
        .flat_map(|clause| clause.split("however"))
        .flat_map(|clause| clause.split(" but "))
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .collect()
}

/// 把日志文件名收敛为可安全放入 marker 属性的文件名。
///
/// 通道解析器以 `]` 结束 marker、以 `"` 切换引号，换行还会拆成另一条消息；
/// 复制时同步使用净化后的名称，确保 marker 中声明的路径就是沙箱内的真实文件。
pub(super) fn media_marker_file_name(file_name: &str) -> String {
    let sanitized = file_name
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '"' | '[' | ']') {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>();
    if sanitized == file_name {
        return sanitized;
    }

    // 不同危险字符可能净化成同一名称；加原名哈希，避免并发 `/log get`
    // 覆盖彼此的沙箱副本并把另一份日志内容发给用户。
    let mut hasher = DefaultHasher::new();
    file_name.hash(&mut hasher);
    format!("{:016x}_{sanitized}", hasher.finish())
}

/// 把日志复制进共享 media 沙箱（`media_dir`，由调用方经 RuntimeChannelInfo 提供），
/// 返回当前外部 channel 的文件媒体标记。
///
/// 通道的媒体发送要求文件落在 `agent/media` 沙箱内（见各通道 `resolve_media_path`），
/// 而日志在 `agent/logs`，故先复制一份到同一 media 目录，再按来源 channel 生成 marker。
/// 调用方必须先用 [`media_channel_available`] 确认该 channel 已实现媒体解析。
///
/// 返回 `None`：非外部通道、或 media 目录不可用 / 复制失败（调用方回退文本回显）。
/// 复制的文件留在 media 目录（通道异步上传），可由 operator 清理。
pub(super) fn log_media_marker(
    channel: &str,
    media_dir: &Path,
    src: &Path,
    file_name: &str,
    size: u64,
) -> Option<String> {
    let channel = external_media_channel_id(channel)?;
    let prefix = format!("{channel}-media");
    let file_name = media_marker_file_name(file_name);
    std::fs::create_dir_all(media_dir).ok()?;
    let dest = media_dir.join(&file_name);
    std::fs::copy(src, &dest).ok()?;
    let path_str = dest.to_string_lossy().replace('\\', "/");
    Some(format!(
        "[:{prefix} type=file path=\"{path_str}\" name=\"{file_name}\" caption=\"{file_name} ({})\"]",
        format_bytes(size)
    ))
}

/// 当 skill 属于名称不同的技能包时，返回 `skills` 根目录下的首级目录名。
///
/// `ai-berkshire/skills/news-pulse.md` 返回 `ai-berkshire`；普通的
/// `glm5v-multimodal/SKILL.md` 与根目录单文件均返回 `None`。仅把明确包含包内 `skills`
/// 层级的路径视为技能包，避免把名称不同的普通 skill 目录误判为包。
pub(crate) fn skill_package_label(
    layout: &Layout,
    skill_path: &str,
    skill_name: &str,
) -> Option<String> {
    let root = layout.skills_dir.to_string_lossy().replace('\\', "/");
    let root = root.trim_end_matches('/');
    let prefix = format!("{root}/");
    let relative = skill_path.strip_prefix(&prefix)?;

    let mut parts = relative.split('/').filter(|part| !part.is_empty());
    let first = parts.next()?;
    if parts.next()? != "skills" {
        return None;
    }

    let package = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if package.is_empty() || package.eq_ignore_ascii_case(skill_name) {
        None
    } else {
        Some(package)
    }
}
