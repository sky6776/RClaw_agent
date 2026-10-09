use super::*;

pub(super) fn truncate_status_text(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut out = compact.chars().take(max_chars).collect::<String>();
    out.push('…');
    out
}

pub(super) fn short_fingerprint(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:08x}", hasher.finish() as u32)
}

pub(super) fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                out.insert(key.clone(), canonicalize_json(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_json).collect()),
        other => other.clone(),
    }
}

pub(super) fn tool_call_fingerprint(call: &rclaw_core::ToolCall) -> String {
    let normalized = serde_json::from_str::<Value>(&call.input)
        .map(|value| canonicalize_json(&value).to_string())
        .unwrap_or_else(|_| call.input.trim().to_string());
    format!("{}\0{normalized}", call.name)
}

pub(super) fn is_blocking_mailbox_wait(call: &rclaw_core::ToolCall) -> bool {
    call.name == "TaskList"
        && serde_json::from_str::<Value>(&call.input)
            .ok()
            .and_then(|input| input.get("wait_timeout_ms").and_then(Value::as_u64))
            .is_some_and(|timeout_ms| timeout_ms > 0)
}

pub(super) fn tool_effect_key(call: &rclaw_core::ToolCall, effect_base: &Path) -> String {
    let parsed_input = serde_json::from_str::<Value>(&call.input).ok();
    let effect_kind = match call.name.as_str() {
        "write" | "edit" | "delete" => "file_mutation".to_string(),
        "shell" => parsed_input
            .as_ref()
            .and_then(|input| {
                input
                    .get("command")
                    .and_then(Value::as_str)
                    .map(shell_effect_kind)
            })
            .unwrap_or_else(|| "shell:unknown".to_string()),
        other => other.to_string(),
    };
    let Some(input) = parsed_input else {
        return effect_kind;
    };
    let Some(object) = input.as_object() else {
        return effect_kind;
    };
    let target_fields = [
        "path",
        "file_path",
        "url",
        "to",
        "recipient",
        "channel",
        "chat_id",
        "taskId",
        "task_id",
        "agent_id",
        "name",
        "job_id",
        "team_name",
    ];
    let targets = target_fields
        .iter()
        .filter(|field| {
            // SendMessage 的真正副作用目标是收件人/通道。team_name 只是可选路由提示：
            // 首次使用错误团队名失败、随后省略或修正 team_name 成功时，应视为同一发送动作
            // 已经重试成功，而不是把旧错误永久留在完成门禁中。
            !(call.name.eq_ignore_ascii_case("SendMessage")
                && **field == "team_name"
                && (object.contains_key("to") || object.contains_key("recipient")))
        })
        .filter_map(|field| {
            object.get(*field).map(|value| {
                format!(
                    "{field}={}",
                    canonicalize_effect_target(&call.name, field, value, effect_base)
                )
            })
        })
        .collect::<Vec<_>>();
    if targets.is_empty() {
        effect_kind
    } else {
        format!("{effect_kind}\0{}", targets.join("\0"))
    }
}

pub(super) fn shell_effect_kind(command: &str) -> String {
    let command = command.to_ascii_lowercase();
    let mut actions = [
        ExternalAction::Create,
        ExternalAction::WriteUpdate,
        ExternalAction::Delete,
        ExternalAction::SourceControl,
        ExternalAction::Deploy,
        ExternalAction::Install,
    ]
    .into_iter()
    .filter(|action| shell_command_matches_action(&command, *action))
    .map(ExternalAction::label)
    .collect::<Vec<_>>();
    actions.sort_unstable();
    if actions.is_empty() {
        "shell:none".to_string()
    } else {
        format!("shell:{}", actions.join("+"))
    }
}

pub(super) fn publish_media_source(command: &str) -> Option<String> {
    let tokens = shell_tokens(command);
    for script_index in tokens.iter().enumerate().filter_map(|(index, token)| {
        token
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("publish_media.py"))
            .then_some(index)
    }) {
        let segment_start = tokens[..script_index]
            .iter()
            .rposition(|token| matches!(token.as_str(), "&&" | "||" | ";" | "|" | "&"))
            .map(|index| index + 1)
            .unwrap_or(0);
        let segment_lower = tokens[segment_start..]
            .iter()
            .map(|token| token.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let segment = segment_lower.iter().map(String::as_str).collect::<Vec<_>>();
        let relative_script_index = script_index - segment_start;
        let invoked_by_python = ["python", "python3", "py"].iter().any(|program| {
            shell_program_index(&segment, program)
                .and_then(|index| python_script_index(&segment, index))
                == Some(relative_script_index)
        });
        let directly_invoked = shell_program_index(&segment, "publish_media.py")
            .is_some_and(|index| index == relative_script_index);
        if !invoked_by_python && !directly_invoked {
            continue;
        }
        let mut index = script_index + 1;
        while let Some(token) = tokens.get(index) {
            if matches!(token.as_str(), ";" | "&" | "|") {
                break;
            }
            if token == "--source-channel"
                || token == "--type"
                || token == "--name"
                || token == "--caption"
            {
                if tokens
                    .get(index + 1)
                    .is_none_or(|value| matches!(value.as_str(), ";" | "&" | "|"))
                {
                    break;
                }
                index += 2;
                continue;
            }
            if token.starts_with("--") && token.contains('=') {
                index += 1;
                continue;
            }
            if !token.starts_with('-') {
                return Some(token.clone());
            }
            break;
        }
    }
    None
}

pub(crate) fn shell_command_runs_media_publisher(
    command: &str,
    skills_dir: &Path,
    shell_cwd: Option<&str>,
) -> bool {
    // shell 工具会把 stdout 与 stderr 一并返回。`$(...)`、反引号和进程替换即使
    // 位于双引号参数中仍可能执行子命令，并从 stderr 注入看似来自发布器的 marker；
    // 受信发布规范不需要这些语法，因此在分词前整体拒绝。
    if command.contains("$(")
        || command.contains('`')
        || command.contains("<(")
        || command.contains(">(")
        || shell_command_has_unquoted_redirection(command)
    {
        return false;
    }
    let tokens = shell_tokens(command);
    if tokens
        .iter()
        .any(|token| matches!(token.as_str(), "&&" | "||" | ";" | "|" | "&"))
    {
        return false;
    }
    trusted_media_publisher_segment(&tokens, skills_dir, shell_cwd)
}

pub(super) fn shell_command_has_unquoted_redirection(command: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        let is_shell_escape = if cfg!(windows) { ch == '^' } else { ch == '\\' };
        if is_shell_escape && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if ch == '"' || (!cfg!(windows) && ch == '\'') {
            match quote {
                Some(active) if active == ch => quote = None,
                None => quote = Some(ch),
                _ => {}
            }
            continue;
        }
        if quote.is_none() && matches!(ch, '<' | '>') {
            return true;
        }
    }
    false
}

pub(crate) fn shell_command_attempts_media_publisher(command: &str) -> bool {
    let known_names = ["publish_media.py", "md2html.py", "export_report.py"];
    shell_tokens(command).iter().any(|token| {
        token
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|name| known_names.contains(&name.to_ascii_lowercase().as_str()))
    })
}

/// 识别所有会生成外部媒体标记的已知发布入口，统一套用可信命令校验。
pub(super) fn shell_command_invokes_known_media_publisher(command: &str) -> bool {
    let tokens = shell_tokens(command);
    tokens
        .split(|token| matches!(token.as_str(), "&&" | "||" | ";" | "|" | "&"))
        .any(|segment| {
            if segment.is_empty() {
                return false;
            }
            let lower_owned = segment
                .iter()
                .map(|token| token.to_ascii_lowercase())
                .collect::<Vec<_>>();
            let lower = lower_owned.iter().map(String::as_str).collect::<Vec<_>>();
            let known_scripts = ["publish_media.py", "md2html.py", "export_report.py"];
            if ["python", "python3", "py"].iter().any(|program| {
                shell_program_index(&lower, program)
                    .and_then(|index| python_script_index(&lower, index))
                    .and_then(|index| lower.get(index))
                    .and_then(|token| token.rsplit(['/', '\\']).next())
                    .is_some_and(|name| known_scripts.contains(&name))
            }) {
                return true;
            }
            if shell_program_index(&lower, "publish_media.py").is_some() {
                return true;
            }
            if ["bash", "sh"].iter().any(|program| {
                let Some(index) = shell_program_index(&lower, program) else {
                    return false;
                };
                let runner_index = if lower.get(index + 1) == Some(&"--") {
                    index + 2
                } else {
                    index + 1
                };
                lower
                    .get(runner_index)
                    .and_then(|token| token.rsplit(['/', '\\']).next())
                    .is_some_and(|name| name == "run.sh")
                    && lower
                        .get(runner_index + 1)
                        .and_then(|token| token.rsplit(['/', '\\']).next())
                        .is_some_and(|name| matches!(name, "md2html.py" | "export_report.py"))
            }) {
                return true;
            }
            ["powershell", "powershell.exe", "pwsh", "pwsh.exe"]
                .iter()
                .any(|program| {
                    let Some(program_index) = shell_program_index(&lower, program) else {
                        return false;
                    };
                    powershell_file_runner_index(&lower)
                        .and_then(|option_index| lower.get(option_index + 1))
                        .and_then(|token| token.rsplit(['/', '\\']).next())
                        .is_some_and(|name| name == "run.ps1")
                        && lower
                            .get(powershell_file_runner_index(&lower).unwrap_or(program_index) + 2)
                            .and_then(|token| token.rsplit(['/', '\\']).next())
                            .is_some_and(|name| matches!(name, "md2html.py" | "export_report.py"))
                })
        })
}

pub(crate) fn untrusted_publish_media_command_error(
    command: &str,
    skills_dir: &Path,
    shell_cwd: Option<&str>,
) -> Option<&'static str> {
    if shell_command_invokes_known_media_publisher(command)
        && !shell_command_runs_media_publisher(command, skills_dir, shell_cwd)
    {
        return Some(
            "MEDIA_PUBLISH_COMMAND_UNTRUSTED: 媒体发布脚本必须作为唯一 shell 命令、按 Skill 文档中的受信路径直接执行；请通过 shell 的 cwd 字段指定工作目录，不要使用 cd、&&、管道、重定向、环境变量包装、临时副本或附加命令。stdout/stderr 已由 shell 工具捕获，无需添加 2>&1。",
        );
    }
    None
}

pub(super) fn trusted_media_publisher_segment(
    tokens: &[String],
    skills_dir: &Path,
    shell_cwd: Option<&str>,
) -> bool {
    if tokens.is_empty() {
        return false;
    }
    let lower_owned = tokens
        .iter()
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let lower = lower_owned.iter().map(String::as_str).collect::<Vec<_>>();
    let invoked_directly = ["python", "python3", "py"].iter().any(|program| {
        shell_program_index(&lower, program)
            .filter(|index| {
                *index == 0
                    && trusted_command_program(
                        &tokens[*index],
                        &["python", "python3", "py"],
                        skills_dir,
                        shell_cwd,
                    )
            })
            .and_then(|index| {
                // 发布器命令不需要 Python 启动参数；禁止 PYTHONPATH/env wrapper、
                // `-X` 等在受信任脚本运行前注入其它 Python 代码。
                (index + 1 < lower.len()).then_some(index + 1)
            })
            .is_some_and(|script_index| {
                TRUSTED_MEDIA_PUBLISHER_SCRIPTS.iter().any(|relative| {
                    token_is_installed_skill_path(
                        &tokens[script_index],
                        skills_dir,
                        relative,
                        shell_cwd,
                    )
                })
            })
    });
    if invoked_directly {
        return true;
    }

    // stock-analyst 通过受信任的跨平台 runner 启动具体脚本，而不是直接把
    // md2html.py 交给 Python。只接受 runner 的真实安装路径和紧随其后的允许脚本名；
    // 单纯在 echo/日志文本中出现同名文件不能成为发布回执来源。
    let delegated_scripts = ["md2html.py", "export_report.py"];
    let bash_runner = ["bash", "sh"].iter().any(|program| {
        let Some(program_index) = shell_program_index(&lower, program) else {
            return false;
        };
        if program_index != 0
            || !trusted_command_program(
                &tokens[program_index],
                &["bash", "sh"],
                skills_dir,
                shell_cwd,
            )
        {
            return false;
        }
        let runner_index = if lower.get(program_index + 1) == Some(&"--") {
            program_index + 2
        } else {
            program_index + 1
        };
        let Some(runner_token) = tokens.get(runner_index) else {
            return false;
        };
        token_is_installed_skill_path(
            runner_token,
            skills_dir,
            "glmv-stock-analyst/scripts/run.sh",
            shell_cwd,
        ) && tokens
            .get(runner_index + 1)
            .and_then(|token| token.rsplit(['/', '\\']).next())
            .is_some_and(|name| delegated_scripts.contains(&name.to_ascii_lowercase().as_str()))
    });
    if bash_runner {
        return true;
    }

    ["powershell", "powershell.exe", "pwsh", "pwsh.exe"]
        .iter()
        .any(|program| {
            let Some(program_index) = shell_program_index(&lower, program) else {
                return false;
            };
            if program_index != 0
                || !trusted_command_program(
                    &tokens[program_index],
                    &["powershell", "pwsh"],
                    skills_dir,
                    shell_cwd,
                )
            {
                return false;
            }
            let Some((runner_token, delegated_token)) = powershell_file_runner_index(&lower)
                .and_then(|file_option| file_option.checked_add(1))
                .and_then(|runner_index| {
                    Some((
                        tokens.get(runner_index)?,
                        tokens.get(runner_index.checked_add(1)?)?,
                    ))
                })
            else {
                return false;
            };
            token_is_installed_skill_path(
                runner_token,
                skills_dir,
                "glmv-stock-analyst/scripts/run.ps1",
                shell_cwd,
            ) && delegated_token
                .rsplit(['/', '\\'])
                .next()
                .is_some_and(|name| delegated_scripts.contains(&name.to_ascii_lowercase().as_str()))
        })
}

pub(super) fn powershell_file_runner_index(tokens: &[&str]) -> Option<usize> {
    let mut index = 1;
    while let Some(option) = tokens.get(index).copied() {
        match option {
            "-nologo" | "-noprofile" | "-noninteractive" => index += 1,
            "-executionpolicy" | "-ep" => {
                let policy = tokens.get(index + 1)?;
                if policy.starts_with('-') {
                    return None;
                }
                index += 2;
            }
            "-file" | "-f" => return Some(index),
            // 尤其不能同时接受 -Command/-EncodedCommand：这会让受信任 runner 只作为
            // 普通文本参数出现，而 stdout 实际来自另一段任意 PowerShell 代码。
            _ => return None,
        }
    }
    None
}

pub(super) fn trusted_command_program(
    token: &str,
    allowed_names: &[&str],
    skills_dir: &Path,
    shell_cwd: Option<&str>,
) -> bool {
    let lower = token.to_ascii_lowercase();
    let file_name = lower.rsplit(['/', '\\']).next().unwrap_or(&lower);
    let normalized_name = file_name.strip_suffix(".exe").unwrap_or(file_name);
    if !allowed_names.contains(&normalized_name) {
        return false;
    }
    if !token.contains(['/', '\\']) {
        return !bare_program_is_shadowed(token, skills_dir, shell_cwd);
    }
    // Skill 工作区可由 Agent 修改，不能把其中任意名为 python 的文件提升为回执
    // 信任根。发布规范使用裸系统 Python；需要虚拟环境的股票技能必须经过下方已安装
    // run.sh/run.ps1 runner，由 runner 自己固定目标脚本和解释器。
    false
}

pub(super) fn bare_program_is_shadowed(
    token: &str,
    skills_dir: &Path,
    shell_cwd: Option<&str>,
) -> bool {
    // Windows cmd 会先从当前目录解析裸 `python`/`powershell`，而 agent 对 workspace
    // 有写权限。若本地同名 exe/cmd/bat 存在，stdout 可能根本不来自预期解释器；
    // 因此宁可不接纳 marker，也不能把同名伪启动器当作发布器来源。
    let Ok(canonical_skills_dir) = skills_dir.canonicalize() else {
        return true;
    };
    let Some(workspace_dir) = canonical_skills_dir.parent() else {
        return true;
    };
    let Some(effective_cwd) = effective_shell_cwd(&canonical_skills_dir, shell_cwd) else {
        return true;
    };

    let file_name = token.rsplit(['/', '\\']).next().unwrap_or(token);
    let lower_name = file_name.to_ascii_lowercase();
    let mut candidates = vec![file_name.to_string()];
    if ![".com", ".exe", ".bat", ".cmd"]
        .iter()
        .any(|suffix| lower_name.ends_with(suffix))
    {
        candidates.extend(
            [".com", ".exe", ".bat", ".cmd"]
                .into_iter()
                .map(|suffix| format!("{file_name}{suffix}")),
        );
    }
    for directory in [&effective_cwd, workspace_dir] {
        if candidates
            .iter()
            .any(|candidate| directory.join(candidate).is_file())
        {
            return true;
        }
    }
    false
}

pub(super) fn effective_shell_cwd(
    canonical_skills_dir: &Path,
    shell_cwd: Option<&str>,
) -> Option<PathBuf> {
    match shell_cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) {
        // 与 shell 工具保持一致：显式 cwd 按进程 cwd 解释，默认 cwd 才是 agent
        // workspace。不能无条件按 workspace 解析命令里的相对脚本，否则模型只需把
        // shell.cwd 指向可写目录，就能让同名伪脚本冒充已安装发布器并伪造回执。
        Some(cwd) => PathBuf::from(cwd).canonicalize().ok(),
        None => canonical_skills_dir.parent().map(Path::to_path_buf),
    }
}

pub(super) fn token_is_installed_skill_path(
    token: &str,
    skills_dir: &Path,
    relative: &str,
    shell_cwd: Option<&str>,
) -> bool {
    let Ok(canonical_skills_dir) = skills_dir.canonicalize() else {
        return false;
    };
    let token_path = PathBuf::from(token.replace('\\', "/"));
    let candidate = if token_path.is_absolute() {
        token_path
    } else {
        let Some(effective_cwd) = effective_shell_cwd(&canonical_skills_dir, shell_cwd) else {
            return false;
        };
        effective_cwd.join(token_path)
    };
    let expected = canonical_skills_dir.join(relative);
    candidate
        .canonicalize()
        .ok()
        .is_some_and(|candidate| candidate == expected && candidate.is_file())
}

pub(super) fn python_script_index(tokens: &[&str], python_index: usize) -> Option<usize> {
    let mut index = python_index + 1;
    while let Some(argument) = tokens.get(index).copied() {
        if matches!(argument, ";" | "&" | "|") {
            return None;
        }
        if argument == "--" {
            let script = tokens.get(index + 1).copied()?;
            return (!matches!(script, ";" | "&" | "|")).then_some(index + 1);
        }
        // `-c` and `-m` execute code/module names; later tokens are arguments, not scripts.
        if matches!(argument, "-c" | "-m")
            || argument
                .strip_prefix("-c")
                .is_some_and(|value| !value.is_empty())
            || argument
                .strip_prefix("-m")
                .is_some_and(|value| !value.is_empty())
        {
            return None;
        }
        let takes_value = matches!(argument, "-W" | "-X" | "--check-hash-based-pycs");
        if argument.starts_with('-') {
            index += if takes_value { 2 } else { 1 };
            continue;
        }
        return Some(index);
    }
    None
}

pub(super) fn shell_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        let is_shell_escape = if cfg!(windows) { ch == '^' } else { ch == '\\' };
        if is_shell_escape && quote != Some('\'') {
            escaped = true;
            continue;
        }
        // Windows shell 工具实际使用 cmd.exe；单引号在那里只是普通字符，不能
        // 屏蔽 `&`/`|`。若检测器却把它当引号，`'caption & echo forged'` 会被误判
        // 为单一受信命令，而 cmd 会真的执行后半段。POSIX sh 才同时识别单双引号。
        if ch == '"' || (!cfg!(windows) && ch == '\'') {
            match quote {
                Some(active) if active == ch => quote = None,
                None => quote = Some(ch),
                _ => current.push(ch),
            }
            continue;
        }
        if quote.is_none() && matches!(ch, ';' | '|' | '&') {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            tokens.push(ch.to_string());
            continue;
        }
        if quote.is_none() && matches!(ch, '\n' | '\r') {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            tokens.push(";".to_string());
            continue;
        }
        if ch.is_whitespace() && quote.is_none() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if escaped {
        current.push(if cfg!(windows) { '^' } else { '\\' });
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

pub(super) fn canonicalize_effect_target(
    tool: &str,
    field: &str,
    value: &Value,
    effect_base: &Path,
) -> Value {
    // 文件工具把裸相对路径解析到 agent 存储根，并兼容显式的 `agent/` 前缀；完成门禁必须
    // 使用相同语义建立副作用身份。skill_manage 的 file_path 是相对 skill 包的逻辑路径，
    // 若在这里按 agent 根绝对化会改变其目标语义。
    if matches!(tool, "write" | "edit" | "delete") && matches!(field, "path" | "file_path") {
        if let Some(path) = value.as_str() {
            let raw = Path::new(path);
            let resolved = if raw.is_absolute() || raw.starts_with(effect_base) {
                raw.to_path_buf()
            } else if effect_base
                .file_name()
                .is_some_and(|base_name| raw.starts_with(base_name))
            {
                match effect_base.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => parent.join(raw),
                    _ => raw.to_path_buf(),
                }
            } else {
                effect_base.join(raw)
            };
            let absolute = if resolved.is_absolute() {
                resolved
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(&resolved))
                    .unwrap_or(resolved)
            };
            let normalized = absolute.components().collect::<PathBuf>();
            let normalized = normalized.to_string_lossy().replace('\\', "/");
            #[cfg(windows)]
            let normalized = normalized.to_lowercase();
            return Value::String(normalized);
        }
    }
    if tool.eq_ignore_ascii_case("SendMessage") && matches!(field, "to" | "recipient") {
        if let Some(recipient) = value.as_str() {
            return Value::String(recipient.trim().trim_start_matches('@').to_string());
        }
    }
    if matches!(field, "taskId" | "task_id" | "agent_id")
        || (field == "name" && matches!(tool, "Agent" | "Task"))
    {
        if let Some(identifier) = value.as_str() {
            return Value::String(identifier.trim().to_string());
        }
    }
    if field == "team_name"
        && matches!(
            tool,
            "TeamCreate"
                | "TeamDelete"
                | "TaskCreate"
                | "TaskUpdate"
                | "TaskList"
                | "TaskGet"
                | "SendMessage"
                | "Agent"
                | "Task"
        )
    {
        if let Some(team_name) = value.as_str() {
            return Value::String(crate::agent_tools::sanitize_agent_name(team_name));
        }
    }
    canonicalize_json(value)
}

pub(super) fn tool_is_effectful(name: &str) -> bool {
    !matches!(
        name,
        "read"
            | "list"
            | "grep"
            | "web_search"
            | "zhipu_search"
            | "tavily_search"
            | "doubao_custom_search"
            | "doubao_global_search"
            | "tavily_extract"
            | "cron_list"
            | "TaskList"
            | "TaskGet"
            | "AgentStatus"
            | "AgentWait"
    )
}

pub(super) fn tool_call_is_effectful(call: &rclaw_core::ToolCall) -> bool {
    if call.name != "shell" {
        return tool_is_effectful(&call.name);
    }
    serde_json::from_str::<Value>(&call.input)
        .ok()
        .and_then(|input| {
            input
                .get("command")
                .and_then(Value::as_str)
                .map(shell_effect_kind)
        })
        .is_some_and(|kind| kind != "shell:none")
}

pub(super) fn classify_tool_error(content: &str) -> &'static str {
    let lower = content.to_ascii_lowercase();
    if lower.contains("loop_circuit_open") || lower.contains("重复工具调用") {
        "loop_repetition"
    } else if lower.contains("permission")
        || lower.contains("权限")
        || lower.contains("拒绝")
        || lower.contains("sandbox")
    {
        "permission"
    } else if lower.contains("timeout") || lower.contains("超时") {
        "timeout"
    } else if lower.contains("not found") || lower.contains("不存在") || lower.contains("未找到")
    {
        "not_found"
    } else if lower.contains("invalid")
        || lower.contains("必须")
        || lower.contains("缺少")
        || lower.contains("非法")
    {
        "validation"
    } else {
        "tool_execution"
    }
}
