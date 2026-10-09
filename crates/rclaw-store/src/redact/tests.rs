use super::*;

#[test]
fn redacts_openai_key() {
    let s = "my key is sk-abcdefghijklmnopqrstuvwxyz plus more";
    let out = redact_sensitive_text(s);
    assert!(out.contains("[REDACTED]"));
    assert!(!out.contains("sk-abcdefghijklmnopqrstuvwxyz"));
}

#[test]
fn does_not_redact_short_sk_prefix() {
    // sk- + 短 run 不像合法 key，不动它（避免误伤形如 "sk-abc" 的普通串）。
    let s = "ref-sk-abc here";
    let out = redact_sensitive_text(s);
    assert_eq!(out, s);
}

#[test]
fn redacts_github_pat() {
    let s = "GITHUB_TOKEN=ghp_1234567890abcdefghij";
    let out = redact_sensitive_text(s);
    // keyword 形式会先匹配（value=ghp_... ≥8），整体替换 value。
    assert!(out.contains("[REDACTED]"));
    assert!(!out.contains("ghp_1234567890abcdefghij"));
}

#[test]
fn redacts_aws_access_key() {
    let s = "aws AKIAIOSFODNN7EXAMPLE done";
    let out = redact_sensitive_text(s);
    assert!(out.contains("[REDACTED]"));
    assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
}

#[test]
fn redacts_bearer_token_preserving_prefix() {
    let s = r#"Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig"#;
    let out = redact_sensitive_text(s);
    assert!(out.contains("Bearer [REDACTED]"));
    assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"));
}

/// 代码赋值（值是变量/表达式引用）不是密钥本体——keyword=value 规则不得脱敏。
/// （曾把 PowerShell 的 `$apiKey = $matches[1]` 改写成 `$apiKey = [REDACTED]`，
/// skill_manage 写闸把脱敏文本落盘，磁盘脚本被腐蚀。）
#[test]
fn keeps_keyword_assignment_with_reference_value_intact() {
    for s in [
        r#"$apiKey = $matches[1]; break"#.to_string(),
        r#"api_key = os.environ["ZHIPU_KEY"]"#.to_string(),
        r#"api_key = config["zhipu"]["api_key"]"#.to_string(),
        r#"self.api_key = get_key(cfg)"#.to_string(),
    ] {
        let out = redact_sensitive_text(&s);
        assert_eq!(out, s, "变量赋值不应被脱敏: {s}");
    }
    // 真实字面密钥仍然脱敏。
    let literal = "api_key = sk-abcdefghijklmnopqrstuvwxyz";
    let out = redact_sensitive_text(literal);
    assert!(out.contains("[REDACTED]"), "字面密钥应被脱敏");
}

/// `Bearer $VAR` 是脚本里的变量引用，不是凭证本体——原样保留。
/// （曾把 PowerShell 脚本的 `Authorization = "Bearer $ApiKey"` 脱敏成
/// `Bearer [REDACTED]`，模型读到后 patch 回写，磁盘脚本被腐蚀。）
#[test]
fn keeps_bearer_variable_references_intact() {
    for s in [
        r#"$Headers = @{ Authorization = "Bearer $ApiKey" }"#.to_string(),
        "curl -H \"Authorization: Bearer ${TOKEN}\" https://api.test".to_string(),
        "Invoke-RestMethod -Headers @{ Authorization = 'Bearer %ENV_KEY%' }".to_string(),
        "req.add_header(\"Authorization\", f\"Bearer {token}\")".to_string(),
        "\"Authorization\": f\"Bearer {api_key}\",".to_string(),
    ] {
        let out = redact_sensitive_text(&s);
        assert_eq!(out, s, "变量引用不应被脱敏: {s}");
    }
}

#[test]
fn redacts_basic_authorization_preserving_quotes() {
    let s = r#"curl -H "Authorization: Basic dXNlcjpwYXNz" https://example.test"#;
    let out = redact_sensitive_shell_command_for(s, ShellDialect::Posix);
    assert!(out.contains(r#"Authorization: Basic [REDACTED]""#));
    assert!(!out.contains("dXNlcjpwYXNz"));
}

#[test]
fn shell_redacts_short_api_key_header() {
    let posix = r#"curl -H 'X-API-Key: abc' https://example.test"#;
    let out = redact_sensitive_shell_command_for(posix, ShellDialect::Posix);
    assert_eq!(
        out,
        r#"curl -H 'X-API-Key: [REDACTED]' https://example.test"#
    );

    let cmd = r#"curl -H "X-API-Key: abc" https://example.test"#;
    let out = redact_sensitive_shell_command_for(cmd, ShellDialect::Cmd);
    assert_eq!(
        out,
        r#"curl -H "X-API-Key: [REDACTED]" https://example.test"#
    );
}

#[test]
fn redacts_password_in_url_userinfo() {
    let s = "DATABASE_URL=postgresql://db-user:short@db.example.test/app";
    let out = redact_sensitive_shell_command_for(s, ShellDialect::Posix);
    assert_eq!(
        out,
        "DATABASE_URL=postgresql://db-user:[REDACTED]@db.example.test/app"
    );

    let token_url = "git clone https://opaque-token@git.example.test/repo.git";
    let out = redact_sensitive_shell_command_for(token_url, ShellDialect::Posix);
    assert_eq!(
        out,
        "git clone https://[REDACTED]@git.example.test/repo.git"
    );
}

#[test]
fn redacts_keyword_value_form() {
    let s = "config password = hunter2_long_value";
    let out = redact_sensitive_text(s);
    assert!(out.contains("password = [REDACTED]"));
    assert!(!out.contains("hunter2_long_value"));
}

#[test]
fn redacts_quoted_keyword_value_form() {
    let s = r#"config token = "abcdef1234567890""#;
    let out = redact_sensitive_text(s);
    assert!(out.contains(r#"token = "[REDACTED]""#));
    assert!(!out.contains("abcdef1234567890"));
}

#[test]
fn redacts_sensitive_cli_flag_values() {
    let cases = [
        ("tool --token abc", "abc"),
        (r#"tool -ApiKey "short""#, "short"),
        ("tool /password hunter2", "hunter2"),
        ("tool --secret=xyz", "xyz"),
        ("tool --access-token /abc123secret", "/abc123secret"),
        ("tool --password -secret", "-secret"),
        ("tool --token --verbose", "--verbose"),
        ("tool --client-secret oauth_value", "oauth_value"),
        ("tool --github-token github_value", "github_value"),
        ("tool --db-password database_value", "database_value"),
    ];

    for (input, secret) in cases {
        let out = redact_sensitive_shell_command(input);
        assert!(out.contains("[REDACTED]"), "未脱敏: {input}");
        assert!(!out.contains(secret), "凭证仍然可见: {out}");
    }
}

#[test]
fn shell_cli_redaction_consumes_escaped_complete_word() {
    let posix_cases = [
        (r#"tool --token "abc\"remaining" --verbose"#, "remaining"),
        (r"tool --password pa\ tail_posix --verbose", "tail_posix"),
    ];

    for (input, leaked_suffix) in posix_cases {
        let out = redact_sensitive_shell_command_for(input, ShellDialect::Posix);
        assert!(!out.contains(leaked_suffix), "凭证后半段仍然可见: {out}");
        assert!(out.contains("[REDACTED]"), "未脱敏: {input}");
        assert!(
            out.to_ascii_lowercase().contains("verbose"),
            "不应吞掉凭证后的参数: {out}"
        );
    }

    let cmd = r"tool /password pa^ tail_cmd /verbose";
    let out = redact_sensitive_shell_command_for(cmd, ShellDialect::Cmd);
    assert!(!out.contains("tail_cmd"), "凭证后半段仍然可见: {out}");
    assert!(out.contains("/verbose"), "不应吞掉凭证后的参数: {out}");

    let continued_cmd = "tool /password pa^\r\ntail_cmd /verbose";
    let out = redact_sensitive_shell_command_for(continued_cmd, ShellDialect::Cmd);
    assert!(!out.contains("tail_cmd"), "续行凭证后半段仍然可见: {out}");
    assert!(out.contains("/verbose"), "不应吞掉续行后的参数: {out}");

    let posix_separated = "tool --token \\\n    continued_secret --verbose";
    let out = redact_sensitive_shell_command_for(posix_separated, ShellDialect::Posix);
    assert!(!out.contains("continued_secret"), "续行凭证仍然可见: {out}");
    assert!(out.contains("--verbose"), "不应吞掉续行后的参数: {out}");

    let cmd_separated = "tool /password ^\r\n    continued_secret /verbose";
    let out = redact_sensitive_shell_command_for(cmd_separated, ShellDialect::Cmd);
    assert!(!out.contains("continued_secret"), "续行凭证仍然可见: {out}");
    assert!(out.contains("/verbose"), "不应吞掉续行后的参数: {out}");
}

#[test]
fn posix_shell_redaction_consumes_compound_expansions() {
    let cases = [
        ("tool --token $(printf secret_sub) --verbose", "secret_sub"),
        ("tool --token `printf secret_tick` --verbose", "secret_tick"),
        (
            "tool --token ${TOKEN:-fallback secret_value} --verbose",
            "secret_value",
        ),
    ];

    for (input, leaked_secret) in cases {
        let out = redact_sensitive_shell_command_for(input, ShellDialect::Posix);
        assert!(!out.contains(leaked_secret), "凭证仍然可见: {out}");
        assert!(out.contains("--verbose"), "不应吞掉后续参数: {out}");
    }
}

#[test]
fn posix_single_quote_does_not_escape_with_backslash() {
    let input = r"tool --password 'abc\' --verbose echo visible";
    let out = redact_sensitive_shell_command_for(input, ShellDialect::Posix);
    assert!(!out.contains("abc"), "凭证仍然可见: {out}");
    assert!(
        out.contains("--verbose echo visible"),
        "不应吞掉凭证后的命令: {out}"
    );
}

#[test]
fn shell_redacts_sensitive_environment_assignments() {
    let posix_cases = [
        (
            r"PASSWORD=pa\ assignment_tail tool --verbose",
            "assignment_tail",
        ),
        ("GITHUB_TOKEN=short command --verbose", "short"),
        (r#"API_KEY="abc\"remaining" curl --verbose"#, "remaining"),
    ];

    for (input, leaked_secret) in posix_cases {
        let out = redact_sensitive_shell_command_for(input, ShellDialect::Posix);
        assert!(!out.contains(leaked_secret), "环境变量凭证仍然可见: {out}");
        assert!(out.contains("[REDACTED]"), "未脱敏: {input}");
        assert!(out.contains("verbose"), "不应吞掉后续命令: {out}");
    }

    let cmd = r"set DB_PASSWORD=pa^ assignment_tail & tool /verbose";
    let out = redact_sensitive_shell_command_for(cmd, ShellDialect::Cmd);
    assert!(
        !out.contains("assignment_tail"),
        "环境变量凭证仍然可见: {out}"
    );
    assert!(out.contains("tool /verbose"), "不应吞掉后续命令: {out}");

    let quoted_cmd = r#"set "PASSWORD=short value" & tool /verbose"#;
    let out = redact_sensitive_shell_command_for(quoted_cmd, ShellDialect::Cmd);
    assert_eq!(
        out, r#"set "PASSWORD=[REDACTED]" & tool /verbose"#,
        "应保留 cmd 赋值的外层引号并脱敏完整值"
    );

    let unquoted_cmd = "@set PASSWORD=short value;still-secret & tool /verbose";
    let out = redact_sensitive_shell_command_for(unquoted_cmd, ShellDialect::Cmd);
    assert_eq!(
        out, "@set PASSWORD=[REDACTED] & tool /verbose",
        "cmd 的分号不是命令分隔符，未加引号的 set 值应脱敏到真正的运算符"
    );

    let spaced_cmd = "set PASSWORD= short value & tool /verbose";
    let out = redact_sensitive_shell_command_for(spaced_cmd, ShellDialect::Cmd);
    assert_eq!(out, "set PASSWORD=[REDACTED] & tool /verbose");
}

#[test]
fn shell_redaction_preserves_value_quotes() {
    let cases = [
        (
            ShellDialect::Posix,
            r#"tool --token="short value" --verbose"#,
            r#"tool --token="[REDACTED]" --verbose"#,
        ),
        (
            ShellDialect::Cmd,
            r#"tool "--client-secret=short value" /verbose"#,
            r#"tool "--client-secret=[REDACTED]" /verbose"#,
        ),
        (
            ShellDialect::Cmd,
            r#"tool /password "short value" /verbose"#,
            r#"tool /password "[REDACTED]" /verbose"#,
        ),
        (
            ShellDialect::Cmd,
            "tool /password value;still-secret /verbose",
            "tool /password [REDACTED] /verbose",
        ),
    ];

    for (dialect, input, expected) in cases {
        assert_eq!(redact_sensitive_shell_command_for(input, dialect), expected);
    }
}

#[test]
fn generic_redaction_ignores_cli_like_prose() {
    let s = "Use design-token blue, then visit /password reset endpoint";
    assert_eq!(redact_sensitive_text(s), s);
}

#[test]
fn redacts_json_keyword_value_form() {
    let s = r#"{"api_key":"abcdef1234567890","safe":"ok"}"#;
    let out = redact_sensitive_text(s);
    assert!(out.contains(r#""api_key":"[REDACTED]""#));
    assert!(out.contains(r#""safe":"ok""#));
    assert!(!out.contains("abcdef1234567890"));
}

#[test]
fn redacts_sk_underscore_key() {
    let s = "tts key sk_1234567890abcdefghij";
    let out = redact_sensitive_text(s);
    assert!(out.contains("[REDACTED]"));
    assert!(!out.contains("sk_1234567890abcdefghij"));
}

#[test]
fn does_not_redact_short_value() {
    // value < 8 字节：可能是普通配置项而非密钥，保留。
    let s = "level=info token=abc";
    let out = redact_sensitive_text(s);
    assert_eq!(out, s);
}

#[test]
fn does_not_redact_substring_words() {
    // "secretive" 不应被 "secret" 命中（词后是字母 i）。
    let s = "use secretive mode here";
    let out = redact_sensitive_text(s);
    assert_eq!(out, s);
}

#[test]
fn preserves_chinese_text_and_redacts_embedded_secret() {
    let s = "我的密钥是 sk-abcdefghijklmnopqrstuvwxyz 结束";
    let out = redact_sensitive_text(s);
    assert!(out.contains("我的密钥是 [REDACTED]"));
    assert!(out.contains("结束"));
}

#[test]
fn compound_keyword_preferred_over_prefix() {
    // secret_key=... 应整体识别为 secret_key（value 被替换），而非卡在 secret。
    let s = "secret_key=AKIAIOSFODNN7EXAMPL";
    let out = redact_sensitive_text(s);
    assert!(out.contains("secret_key=[REDACTED]"));
}

#[test]
fn leaves_innocent_text_untouched() {
    let s = "请用 read_file 读取 agent/config/SOUL.md，再用 shell 跑 npm test。";
    let out = redact_sensitive_text(s);
    assert_eq!(out, s);
}
