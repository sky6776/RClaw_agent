use std::collections::HashSet;

use serde_json::Value;

use super::effect::{publish_media_source, shell_tokens};
use super::run::ToolReceipt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ExternalAction {
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
            &["部署", "上线", "deploy", "publish site"][..],
            ExternalAction::Deploy,
        ),
        (
            &["安装", "卸载", "install", "uninstall"][..],
            ExternalAction::Install,
        ),
    ] {
        if patterns
            .iter()
            .any(|pattern| contains_required_request_pattern(&lower, pattern))
        {
            actions.insert(action);
        }
    }
    actions
}

pub(super) fn contains_required_request_pattern(text: &str, pattern: &str) -> bool {
    text.match_indices(pattern).any(|(index, _)| {
        let prefix = text[..index].chars().rev().take(48).collect::<String>();
        let prefix = prefix.chars().rev().collect::<String>();
        if request_prefix_is_negated(&prefix) {
            return false;
        }

        true
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
        .rsplit(['。', '！', '？', '；', ';', '，', ',', '\n', '\r'])
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
