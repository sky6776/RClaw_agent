//! 秘密脱敏：把摘要/记忆里的密钥、令牌等敏感串替换为 `[REDACTED]`。
//!
//! 在把
//! 会话历史喂给摘要 LLM 前后、以及写入持久化记忆前，剥离可识别的凭证，避免
//! 密钥被摘要回显或长期留存于 `MEMORY.md`（包括 legacy `[自进化]` 条目）。
//!
//! 零依赖（不引入 `regex`）：所有模式用手写字节扫描实现，只在 UTF-8 字符
//! 边界推进，因此对中文等多字节文本安全。

/// 把文本中可识别的密钥/令牌替换为 `[REDACTED]`。
///
/// 覆盖的常见密钥形态：
/// - OpenAI 风格：`sk-` + ≥20 位 `[A-Za-z0-9_-]`
/// - GitHub PAT：`gh[pousr]_` + ≥20 位 `[A-Za-z0-9_]`
/// - AWS Access Key ID：`AKIA` + 恰好 16 位 `[A-Z0-9]`
/// - HTTP 凭证：Bearer 令牌、Basic Authorization、URL userinfo
/// - `keyword(:|=)value` / `"keyword":"value"` 形式：keyword 命中
///   password/secret/token/api_key/... 且 value ≥8 字节时，替换 value
///
/// 不匹配的文本原样返回（含中文）。
pub fn redact_sensitive_text(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some((consumed, replacement)) = match_redaction_at(text, bytes, i) {
            out.push_str(&replacement);
            i += consumed;
        } else {
            // 拷贝一个完整 UTF-8 字符（保持 i 在 char 边界上）。
            let ch = text[i..].chars().next().expect("i 始终在 char 边界上");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 脱敏 `sh -lc` / `cmd /C` 命令中的凭证。
///
/// 在通用文本规则之外识别 `--token value`、`-ApiKey value`、`/password:value`
/// 等 CLI 形式。CLI 规则只用于命令预览，不影响摘要和持久化记忆中的普通路径文本。
pub fn redact_sensitive_shell_command(text: &str) -> String {
    let dialect = if cfg!(windows) {
        ShellDialect::Cmd
    } else {
        ShellDialect::Posix
    };
    redact_sensitive_shell_command_for(text, dialect)
}

/// 在字节位置 `i`（保证是 char 边界）尝试匹配任一脱敏模式。
///
/// 返回 `(消费的字节数, 替换串)`。所有模式的前缀都是 ASCII，因此 byte 偏移
/// 与 char 偏移在这些前缀段内等价；value/token run 不会跨越 ASCII 空白（空白
/// 均为单字节），故消费量按字节计仍落在 char 边界上。
fn match_redaction_at(text: &str, bytes: &[u8], i: usize) -> Option<(usize, String)> {
    // 1. URL userinfo：保留 scheme 和主机；隐藏 `user:password@` 中的密码，或
    // `token@` 形式的整段凭证。
    if is_boundary_before(bytes, i) && bytes.get(i).is_some_and(u8::is_ascii_alphabetic) {
        let scheme_len = run_len(bytes, i, |b| {
            b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')
        });
        let authority_start = i + scheme_len + 3;
        if bytes
            .get(i + scheme_len..authority_start)
            .is_some_and(|separator| separator == b"://")
        {
            let authority_len = run_len(bytes, authority_start, |b| {
                !is_ws(b) && !is_quote(b) && !matches!(b, b'/' | b'?' | b'#')
            });
            let authority_end = authority_start + authority_len;
            if let Some(at) = bytes[authority_start..authority_end]
                .iter()
                .position(|&b| b == b'@')
                .map(|offset| authority_start + offset)
            {
                if let Some(colon) = bytes[authority_start..at]
                    .iter()
                    .position(|&b| b == b':')
                    .map(|offset| authority_start + offset)
                {
                    let password_start = colon + 1;
                    if password_start < at {
                        let prefix = &text[i..password_start];
                        return Some((at - i, format!("{prefix}[REDACTED]")));
                    }
                } else if authority_start < at {
                    let prefix = &text[i..authority_start];
                    return Some((at - i, format!("{prefix}[REDACTED]")));
                }
            }
        }
    }

    // 2. HTTP Authorization 头：Basic 凭证没有独立密钥前缀，需连同头名称识别。
    if is_boundary_before(bytes, i) {
        let header_len = if ci_starts_with(bytes, i, b"proxy-authorization") {
            19
        } else if ci_starts_with(bytes, i, b"authorization") {
            13
        } else {
            0
        };
        if header_len > 0 {
            let mut j = i + header_len;
            j += run_len(bytes, j, is_horizontal_ws);
            if bytes.get(j) == Some(&b':') {
                j += 1;
                j += run_len(bytes, j, is_horizontal_ws);
                if ci_starts_with(bytes, j, b"basic") {
                    j += 5;
                    let ws = run_len(bytes, j, is_horizontal_ws);
                    let token_start = j + ws;
                    let token_len = run_len(bytes, token_start, |b| !is_ws(b) && !is_quote(b));
                    if ws > 0 && token_len >= 4 {
                        let prefix = &text[i..token_start];
                        return Some((token_start + token_len - i, format!("{prefix}[REDACTED]")));
                    }
                }
            }
        }
    }

    // 3. Bearer <token>：保留 "Bearer " 前缀，仅替换 token。
    // `$VAR` / `%VAR%` / `${VAR}` / `{var}` 是脚本里的变量引用（shell/cmd/PowerShell/
    // f-string/Jinja），不是凭证本体——误替换会把示例代码教坏（模型读到什么就会
    // patch 回什么），且真实密钥没有以 `$ % {` 开头的形态。
    if is_boundary_before(bytes, i) && ci_starts_with(bytes, i, b"bearer") {
        let after = i + 6;
        let ws = run_len(bytes, after, is_ws);
        let tok_start = after + ws;
        let tok_len = run_len(bytes, tok_start, |b| !is_ws(b) && !is_quote(b));
        let is_var_ref = matches!(
            bytes.get(tok_start),
            Some(b'$') | Some(b'%') | Some(b'{')
        );
        if ws > 0 && tok_len >= 4 && !is_var_ref {
            let prefix = &text[i..tok_start];
            return Some((tok_start + tok_len - i, format!("{prefix}[REDACTED]")));
        }
    }

    // 4. OpenAI：`sk-` + ≥20 位；ElevenLabs：`sk_` + ≥10 位。
    if is_boundary_before(bytes, i) && ci_starts_with(bytes, i, b"sk-") {
        let run = run_len(bytes, i + 3, is_word);
        if run >= 20 {
            return Some((3 + run, "[REDACTED]".to_string()));
        }
    }
    if is_boundary_before(bytes, i) && ci_starts_with(bytes, i, b"sk_") {
        let run = run_len(bytes, i + 3, is_word_under);
        if run >= 10 {
            return Some((3 + run, "[REDACTED]".to_string()));
        }
    }

    // Hindsight API key：`hsk-` + ≥10 位 word 字符。
    if is_boundary_before(bytes, i) && ci_starts_with(bytes, i, b"hsk-") {
        let run = run_len(bytes, i + 4, is_word);
        if run >= 10 {
            return Some((4 + run, "[REDACTED]".to_string()));
        }
    }

    // 5. GitHub PAT：`gh[pousr]_` + ≥20 位 word_under 字符（大小写不敏感前缀）。
    if is_boundary_before(bytes, i)
        && bytes.len() >= i + 4
        && (bytes[i] == b'g' || bytes[i] == b'G')
        && (bytes[i + 1] == b'h' || bytes[i + 1] == b'H')
        && matches!(
            bytes[i + 2],
            b'p' | b'o' | b'u' | b's' | b'r' | b'P' | b'O' | b'U' | b'S' | b'R'
        )
        && bytes[i + 3] == b'_'
    {
        let run = run_len(bytes, i + 4, is_word_under);
        if run >= 20 {
            return Some((4 + run, "[REDACTED]".to_string()));
        }
    }

    // 6. AWS Access Key ID：`AKIA` + 恰好 16 位大写字母/数字（区分大小写）。
    if is_boundary_before(bytes, i) && bytes.len() >= i + 20 && &bytes[i..i + 4] == b"AKIA" {
        let run = run_len(bytes, i + 4, is_upper_or_digit);
        if run >= 16 {
            // 恰好替换 AKIA + 16 = 20 字节；其后多余大写字母单独看不再是合法 key。
            return Some((20, "[REDACTED]".to_string()));
        }
    }

    // 7. keyword(:|=)value / "keyword":"value"：keyword 取最长匹配，
    // value（未加引号时到空白或引号；加引号时到闭合引号）≥8 字节才替换。
    // 值为「变量/表达式形态」时不替换：`$apiKey = $matches[1]`、
    // `api_key = os.environ["ZHIPU_KEY"]` 是代码赋值而非密钥本体——真实密钥是
    // 字面 token（sk-/ghp_/JWT/hex 等），不会以 $ % { 开头，也不含 [ ( 引用语法。
    if is_boundary_before(bytes, i) {
        if let Some(kw_len) = match_keyword(bytes, i) {
            let after_kw = i + kw_len;
            // 关键词后必须不是字母数字（避免 "secret" 命中 "secretive"）。
            if after_kw >= bytes.len() || !bytes[after_kw].is_ascii_alphanumeric() {
                let mut j = after_kw;
                j += run_len(bytes, j, is_ws);
                if j < bytes.len() && is_quote(bytes[j]) {
                    j += 1;
                    j += run_len(bytes, j, is_ws);
                }
                if j < bytes.len() && (bytes[j] == b':' || bytes[j] == b'=') {
                    j += 1;
                    j += run_len(bytes, j, is_ws);
                    let quoted = j < bytes.len() && is_quote(bytes[j]);
                    if quoted {
                        j += 1;
                    }
                    let val_len = run_len(bytes, j, |b| {
                        if quoted { !is_quote(b) } else { !is_ws(b) && !is_quote(b) }
                    });
                    if val_len >= 8 && !looks_like_reference(&bytes[j..j + val_len]) {
                        let prefix = &text[i..j];
                        return Some((j + val_len - i, format!("{prefix}[REDACTED]")));
                    }
                    return None;
                }
            }
        }
    }

    None
}

/// 值是否为变量/表达式引用形态（非字面密钥）。
fn looks_like_reference(value: &[u8]) -> bool {
    let starts_variable = matches!(value.first(), Some(b'$') | Some(b'%') | Some(b'{'));
    starts_variable || value.iter().any(|b| matches!(b, b'[' | b']' | b'(' | b')'))
}

/// `i` 位置的前一个字节是否为「词边界」（非 ASCII 字母数字）。
///
/// 对中文等多字节字符：其 UTF-8 续接字节 ≥0x80，落在 `!is_ascii_alphanumeric`
/// 分支，因此中文后接英文关键词仍视为边界——符合直觉（"密钥是 password=..." 里
/// 的 "password" 前是中文，应被识别）。
fn is_boundary_before(bytes: &[u8], i: usize) -> bool {
    i == 0 || !bytes[i - 1].is_ascii_alphanumeric()
}

#[derive(Clone, Copy)]
enum SensitiveCliValue {
    NextWord,
    Inline {
        start: usize,
        end: usize,
        assignment: bool,
    },
}

#[derive(Clone, Copy)]
enum ShellDialect {
    Posix,
    Cmd,
}

fn redact_sensitive_shell_command_for(text: &str, dialect: ShellDialect) -> String {
    redact_sensitive_text(&redact_sensitive_cli_flags(text, dialect))
}

/// 只在 shell command 上运行的 CLI 参数脱敏器。值按完整 shell word 替换，避免
/// 转义空格或转义引号导致只隐藏前半段。
fn redact_sensitive_cli_flags(text: &str, dialect: ShellDialect) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied_until = 0usize;
    let mut i = 0usize;
    let mut cmd_command_start = true;
    let mut cmd_set_pending = false;

    while i < bytes.len() {
        if is_ws(bytes[i]) {
            if matches!(dialect, ShellDialect::Cmd) && matches!(bytes[i], b'\r' | b'\n') {
                cmd_command_start = true;
                cmd_set_pending = false;
            }
            i += 1;
            continue;
        }
        if is_shell_operator(bytes[i], dialect) {
            if matches!(dialect, ShellDialect::Cmd) {
                cmd_command_start = !matches!(bytes[i], b'<' | b'>');
                cmd_set_pending = false;
            }
            i += 1;
            continue;
        }

        let word_end = shell_word_end(bytes, i, dialect);
        let starts_cmd_set = matches!(dialect, ShellDialect::Cmd)
            && cmd_command_start
            && is_cmd_set_word(&bytes[i..word_end]);
        let flag = sensitive_cli_value(&bytes[i..word_end]);
        match flag {
            Some(SensitiveCliValue::Inline {
                start,
                mut end,
                assignment,
            }) => {
                let value_start = i + start;
                if matches!(dialect, ShellDialect::Cmd)
                    && cmd_set_pending
                    && assignment
                    && !has_matching_outer_quotes(&bytes[i..word_end])
                {
                    end = cmd_unquoted_set_value_end(bytes, value_start) - i;
                }
                let value_end = i + end;
                if value_start < value_end {
                    out.push_str(&text[copied_until..value_start]);
                    out.push_str("[REDACTED]");
                    copied_until = value_end;
                    i = value_end.max(word_end);
                } else {
                    i = word_end;
                }
            }
            Some(SensitiveCliValue::NextWord) => {
                let value_start = skip_shell_value_separators(bytes, word_end, dialect);

                if value_start < bytes.len()
                    && !is_ws(bytes[value_start])
                    && !is_shell_operator(bytes[value_start], dialect)
                {
                    let value_end = shell_word_end(bytes, value_start, dialect);
                    let (redact_start, redact_end) =
                        matching_quote_contents(bytes, value_start, value_end);
                    out.push_str(&text[copied_until..redact_start]);
                    out.push_str("[REDACTED]");
                    copied_until = redact_end;
                    i = value_end;
                } else {
                    i = word_end;
                }
            }
            _ => i = word_end,
        }

        if matches!(dialect, ShellDialect::Cmd) {
            cmd_set_pending = starts_cmd_set;
            cmd_command_start = false;
        }
    }

    out.push_str(&text[copied_until..]);
    out
}

fn skip_shell_value_separators(bytes: &[u8], start: usize, dialect: ShellDialect) -> usize {
    let mut i = start;
    loop {
        while i < bytes.len() && is_horizontal_ws(bytes[i]) {
            i += 1;
        }

        let continuation_end = match dialect {
            ShellDialect::Posix
                if bytes.get(i) == Some(&b'\\') && bytes.get(i + 1) == Some(&b'\n') =>
            {
                Some(i + 2)
            }
            ShellDialect::Cmd
                if bytes.get(i) == Some(&b'^') && bytes.get(i + 1) == Some(&b'\n') =>
            {
                Some(i + 2)
            }
            ShellDialect::Cmd
                if bytes.get(i) == Some(&b'^')
                    && bytes.get(i + 1) == Some(&b'\r')
                    && bytes.get(i + 2) == Some(&b'\n') =>
            {
                Some(i + 3)
            }
            _ => None,
        };
        if let Some(end) = continuation_end {
            i = end;
        } else {
            return i;
        }
    }
}

/// 返回敏感 CLI 参数、环境变量赋值或 header 的值位置：独立的下一 shell word，
/// 或当前 word 中 `=` / `:` 后的字节偏移。参数名比较忽略大小写、连字符和下划线。
fn sensitive_cli_value(word: &[u8]) -> Option<SensitiveCliValue> {
    let (content_start, content_end) = matching_quote_contents(word, 0, word.len());
    let content = &word[content_start..content_end];
    let (relative_name_start, is_cli_flag) = if content.starts_with(b"--") {
        (2, true)
    } else if matches!(content.first(), Some(b'-' | b'/')) {
        (1, true)
    } else {
        (0, false)
    };
    let name_start = content_start + relative_name_start;

    let mut name_end = name_start;
    while name_end < content_end
        && (word[name_end].is_ascii_alphanumeric() || matches!(word[name_end], b'-' | b'_'))
    {
        name_end += 1;
    }
    if name_end == name_start || !is_sensitive_cli_name(&word[name_start..name_end]) {
        return None;
    }

    if is_cli_flag && name_end == content_end {
        Some(SensitiveCliValue::NextWord)
    } else if name_end < content_end && matches!(word[name_end], b'=' | b':') {
        let separator = word[name_end];
        let mut value_start = name_end + 1;
        while value_start < content_end && is_horizontal_ws(word[value_start]) {
            value_start += 1;
        }
        let (start, end) = matching_quote_contents(word, value_start, content_end);
        Some(SensitiveCliValue::Inline {
            start,
            end,
            assignment: !is_cli_flag && separator == b'=',
        })
    } else {
        None
    }
}

fn matching_quote_contents(bytes: &[u8], start: usize, end: usize) -> (usize, usize) {
    if end > start + 1 && matches!(bytes[start], b'\'' | b'"') && bytes[end - 1] == bytes[start] {
        (start + 1, end - 1)
    } else {
        (start, end)
    }
}

fn has_matching_outer_quotes(word: &[u8]) -> bool {
    word.len() > 1 && matches!(word[0], b'\'' | b'"') && word[word.len() - 1] == word[0]
}

fn is_cmd_set_word(word: &[u8]) -> bool {
    word.strip_prefix(b"@")
        .unwrap_or(word)
        .eq_ignore_ascii_case(b"set")
}

/// 未使用 `set "NAME=value"` 形式时，cmd 会把命令运算符之前的空格也视为
/// 环境变量值的一部分；因此需要越过普通 shell word 的边界完整脱敏。
fn cmd_unquoted_set_value_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let mut quoted = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            quoted = !quoted;
            i += 1;
        } else if !quoted && b == b'^' && i + 1 < bytes.len() {
            i = cmd_caret_escape_end(bytes, i);
        } else if !quoted && (matches!(b, b'\r' | b'\n') || is_cmd_shell_operator(b)) {
            break;
        } else {
            i += 1;
        }
    }
    while i > start && is_horizontal_ws(bytes[i - 1]) {
        i -= 1;
    }
    i
}

fn is_sensitive_cli_name(name: &[u8]) -> bool {
    const NAMES: &[&[u8]] = &[
        b"refreshtoken",
        b"accesstoken",
        b"privatekey",
        b"secretkey",
        b"accesskey",
        b"password",
        b"authtoken",
        b"apikey",
        b"passwd",
        b"secret",
        b"token",
    ];

    if NAMES.iter().any(|expected| {
        name.iter()
            .filter(|&&b| !matches!(b, b'-' | b'_'))
            .map(u8::to_ascii_lowercase)
            .eq(expected.iter().copied())
    }) {
        return true;
    }

    let mut previous = None;
    for component in name
        .split(|b| matches!(b, b'-' | b'_'))
        .filter(|component| !component.is_empty())
    {
        if matches_ci(
            component,
            &[
                b"token",
                b"secret",
                b"password",
                b"passwd",
                b"credential",
                b"credentials",
            ],
        ) {
            return true;
        }
        if component.eq_ignore_ascii_case(b"key")
            && previous.is_some_and(|p| {
                matches_ci(
                    p,
                    &[
                        b"api",
                        b"access",
                        b"private",
                        b"secret",
                        b"client",
                        b"auth",
                        b"signing",
                        b"encryption",
                        b"ssh",
                    ],
                )
            })
        {
            return true;
        }
        previous = Some(component);
    }
    false
}

fn matches_ci(value: &[u8], candidates: &[&[u8]]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn shell_word_end(bytes: &[u8], start: usize, dialect: ShellDialect) -> usize {
    match dialect {
        ShellDialect::Posix => posix_shell_word_end(bytes, start),
        ShellDialect::Cmd => cmd_shell_word_end(bytes, start),
    }
}

/// POSIX `sh` word：反斜杠仅在非单引号上下文转义；`$()`、`${}` 与反引号替换
/// 整体属于当前 word，内部空白和运算符不能提前结束凭证值。
fn posix_shell_word_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let mut quote = None;

    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if q != b'\'' && b == b'\\' && i + 1 < bytes.len() {
                i += 2;
            } else if b == q {
                quote = None;
                i += 1;
            } else if q == b'"' && b == b'`' {
                i = posix_backtick_end(bytes, i);
            } else if q == b'"' && b == b'$' && bytes.get(i + 1) == Some(&b'(') {
                i = posix_command_substitution_end(bytes, i + 1);
            } else if q == b'"' && b == b'$' && bytes.get(i + 1) == Some(&b'{') {
                i = posix_parameter_expansion_end(bytes, i + 1);
            } else {
                i += 1;
            }
        } else if matches!(b, b'\'' | b'"') {
            quote = Some(b);
            i += 1;
        } else if b == b'\\' && i + 1 < bytes.len() {
            i += 2;
        } else if b == b'`' {
            i = posix_backtick_end(bytes, i);
        } else if b == b'$' && bytes.get(i + 1) == Some(&b'(') {
            i = posix_command_substitution_end(bytes, i + 1);
        } else if b == b'$' && bytes.get(i + 1) == Some(&b'{') {
            i = posix_parameter_expansion_end(bytes, i + 1);
        } else if is_ws(b) || is_posix_shell_operator(b) {
            break;
        } else {
            i += 1;
        }
    }

    i
}

/// `open` 指向 `$(` 的左括号；返回匹配右括号之后的位置。未闭合时保守地消费到结尾。
fn posix_command_substitution_end(bytes: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    let mut depth = 1usize;
    let mut quote = None;

    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if q != b'\'' && b == b'\\' && i + 1 < bytes.len() {
                i += 2;
            } else if b == q {
                quote = None;
                i += 1;
            } else if q == b'"' && b == b'`' {
                i = posix_backtick_end(bytes, i);
            } else if q == b'"' && b == b'$' && bytes.get(i + 1) == Some(&b'{') {
                i = posix_parameter_expansion_end(bytes, i + 1);
            } else {
                i += 1;
            }
        } else if matches!(b, b'\'' | b'"') {
            quote = Some(b);
            i += 1;
        } else if b == b'\\' && i + 1 < bytes.len() {
            i += 2;
        } else if b == b'`' {
            i = posix_backtick_end(bytes, i);
        } else if b == b'$' && bytes.get(i + 1) == Some(&b'{') {
            i = posix_parameter_expansion_end(bytes, i + 1);
        } else if b == b'(' {
            depth += 1;
            i += 1;
        } else if b == b')' {
            depth -= 1;
            i += 1;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }

    bytes.len()
}

/// `open` 指向 `${` 的左花括号；参数展开中的空白、运算符和嵌套替换都属于
/// 当前 shell word。未闭合时保守地消费到结尾。
fn posix_parameter_expansion_end(bytes: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    let mut depth = 1usize;
    let mut quote = None;

    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if q != b'\'' && b == b'\\' && i + 1 < bytes.len() {
                i += 2;
            } else if b == q {
                quote = None;
                i += 1;
            } else if q == b'"' && b == b'`' {
                i = posix_backtick_end(bytes, i);
            } else if q == b'"' && b == b'$' && bytes.get(i + 1) == Some(&b'(') {
                i = posix_command_substitution_end(bytes, i + 1);
            } else if q == b'"' && b == b'$' && bytes.get(i + 1) == Some(&b'{') {
                depth += 1;
                i += 2;
            } else if b == b'}' && depth > 1 {
                depth -= 1;
                i += 1;
            } else {
                i += 1;
            }
        } else if matches!(b, b'\'' | b'"') {
            quote = Some(b);
            i += 1;
        } else if b == b'\\' && i + 1 < bytes.len() {
            i += 2;
        } else if b == b'`' {
            i = posix_backtick_end(bytes, i);
        } else if b == b'$' && bytes.get(i + 1) == Some(&b'(') {
            i = posix_command_substitution_end(bytes, i + 1);
        } else if b == b'$' && bytes.get(i + 1) == Some(&b'{') {
            depth += 1;
            i += 2;
        } else if b == b'}' {
            depth -= 1;
            i += 1;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }

    bytes.len()
}

/// `start` 指向起始反引号；返回闭合反引号之后的位置。未闭合时保守地消费到结尾。
fn posix_backtick_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
        } else if bytes[i] == b'`' {
            return i + 1;
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// Windows 实际执行后端是 `cmd /C`：双引号控制 word 边界，`^` 只在引号外
/// 转义下一字符；单引号、反斜杠和反引号均按普通字符处理。
fn cmd_shell_word_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let mut quoted = false;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            quoted = !quoted;
            i += 1;
        } else if !quoted && b == b'^' && i + 1 < bytes.len() {
            i = cmd_caret_escape_end(bytes, i);
        } else if !quoted && (is_ws(b) || is_cmd_shell_operator(b)) {
            break;
        } else {
            i += 1;
        }
    }

    i
}

fn cmd_caret_escape_end(bytes: &[u8], caret: usize) -> usize {
    if bytes.get(caret + 1) == Some(&b'\r') && bytes.get(caret + 2) == Some(&b'\n') {
        caret + 3
    } else {
        (caret + 2).min(bytes.len())
    }
}

#[inline]
fn is_horizontal_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t')
}

#[inline]
fn is_shell_operator(b: u8, dialect: ShellDialect) -> bool {
    match dialect {
        ShellDialect::Posix => is_posix_shell_operator(b),
        ShellDialect::Cmd => is_cmd_shell_operator(b),
    }
}

#[inline]
fn is_posix_shell_operator(b: u8) -> bool {
    matches!(b, b';' | b'|' | b'&' | b'(' | b')' | b'<' | b'>')
}

#[inline]
fn is_cmd_shell_operator(b: u8) -> bool {
    matches!(b, b'|' | b'&' | b'(' | b')' | b'<' | b'>')
}

/// 大小写不敏感地判断 `bytes[i..]` 是否以 `needle` 开头。
fn ci_starts_with(bytes: &[u8], i: usize, needle: &[u8]) -> bool {
    if i + needle.len() > bytes.len() {
        return false;
    }
    bytes[i..i + needle.len()]
        .iter()
        .zip(needle)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// 从 `i` 起连续满足 `pred` 的字节数。
fn run_len(bytes: &[u8], i: usize, pred: impl Fn(u8) -> bool) -> usize {
    let mut j = i;
    while j < bytes.len() && pred(bytes[j]) {
        j += 1;
    }
    j - i
}

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

#[inline]
fn is_quote(b: u8) -> bool {
    matches!(b, b'"' | b'\'')
}

#[inline]
fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

#[inline]
fn is_word_under(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[inline]
fn is_upper_or_digit(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}

/// 敏感关键词集合（小写）。`match_keyword` 取最长匹配以正确处理复合形式
/// （如 `secret_key` 优先于 `secret`）。
const KEYWORDS: &[&[u8]] = &[
    b"refresh_token",
    b"access_token",
    b"private_key",
    b"privatekey",
    b"secret_key",
    b"access_key",
    b"accesskey",
    b"password",
    b"auth_token",
    b"api_key",
    b"api-key",
    b"apikey",
    b"passwd",
    b"secret",
    b"token",
];

/// 返回 `bytes[i..]` 处匹配的最长关键词的字节长度（小写比较），未命中返回 `None`。
fn match_keyword(bytes: &[u8], i: usize) -> Option<usize> {
    let mut best = 0;
    for kw in KEYWORDS {
        if kw.len() > best && ci_starts_with(bytes, i, kw) {
            best = kw.len();
        }
    }
    if best > 0 {
        Some(best)
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
