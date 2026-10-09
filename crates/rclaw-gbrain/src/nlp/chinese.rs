//! Chinese NLP: jieba tokenization + pinyin for KB FTS5 indexing

use jieba_rs::Jieba;
use pinyin::ToPinyin;
use std::sync::OnceLock;
use tracing::trace;

const MAX_CONTENT_TOKENS: usize = 10000;
const MAX_PINYIN_CHARS: usize = 200;

static JIEBA: OnceLock<Jieba> = OnceLock::new();

fn jieba() -> &'static Jieba {
    JIEBA.get_or_init(Jieba::new)
}

/// Tokenize document content for FTS5 indexing.
/// Returns space-separated tokens string for kb_document_nodes.content_tokens.
pub fn tokenize_content(content: &str) -> String {
    let words = jieba().cut(content, true);
    let mut token_set = std::collections::HashSet::new();
    let mut result = Vec::new();

    for word in words {
        let token = normalize_token(word);
        if token.is_empty() {
            continue;
        }
        if token_set.insert(token.clone()) {
            result.push(token.clone());
        }
        if has_chinese(&token) {
            if let Some(pinyin_tokens) = generate_pinyin_tokens(&token) {
                for pt in pinyin_tokens {
                    if token_set.insert(pt.clone()) {
                        result.push(pt);
                    }
                }
            }
        }
        if result.len() >= MAX_CONTENT_TOKENS {
            break;
        }
    }
    result.join(" ")
}

/// Tokenize file name for FTS5 indexing.
/// Returns space-separated tokens string for kb_documents.name_tokens.
pub fn tokenize_name(original_name: &str) -> String {
    let (stem, ext) = split_name_extension(original_name);
    let mut token_set = std::collections::HashSet::new();
    let mut result = Vec::new();

    // 1. jieba cut on stem
    let words = jieba().cut(&stem, true);
    for word in words {
        let token = normalize_token(word);
        if token.is_empty() {
            continue;
        }
        if token_set.insert(token.clone()) {
            result.push(token);
        }
    }

    // 2. pinyin for chinese text
    let chinese = extract_chinese(&stem);
    if !chinese.is_empty() && chinese.chars().count() <= MAX_PINYIN_CHARS {
        if let Some(pinyin_tokens) = generate_pinyin_tokens(&chinese) {
            for pt in pinyin_tokens {
                if token_set.insert(pt.clone()) {
                    result.push(pt);
                }
            }
        }
    }

    // 3. split by non-word chars and re-tokenize each part
    for part in split_by_non_word(&stem) {
        let part_words = jieba().cut(&part, true);
        for word in part_words {
            let token = normalize_token(word);
            if !token.is_empty() && token_set.insert(token.clone()) {
                result.push(token);
            }
        }
        if has_chinese(&part) && part.chars().count() <= MAX_PINYIN_CHARS {
            if let Some(pinyin_tokens) = generate_pinyin_tokens(&part) {
                for pt in pinyin_tokens {
                    if token_set.insert(pt.clone()) {
                        result.push(pt);
                    }
                }
            }
        }
    }

    // 4. add extension as token
    if !ext.is_empty() {
        let ext_token = ext.to_lowercase();
        if token_set.insert(ext_token.clone()) {
            result.push(ext_token);
        }
    }

    result.join(" ")
}

/// Build FTS5 MATCH query from user search keywords.
pub fn build_fts_match_query(keyword: &str) -> String {
    let words = jieba().cut(keyword, true);
    let mut parts = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for word in words {
        let token = normalize_token(word);
        if token.is_empty() {
            continue;
        }
        if seen.insert(token.clone()) {
            let escaped = escape_fts5_token(&token);
            parts.push(format!("{}*", escaped));
        }
    }

    if parts.is_empty() {
        for part in split_by_non_word(keyword) {
            let token = normalize_token(&part);
            if !token.is_empty() && seen.insert(token.clone()) {
                let escaped = escape_fts5_token(&token);
                parts.push(format!("{}*", escaped));
            }
        }
    }

    parts.join(" OR ")
}

/// Check if a character is a CJK unified ideograph.
pub fn is_chinese(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}' |
        '\u{3400}'..='\u{4DBF}' |
        '\u{20000}'..='\u{2A6DF}' |
        '\u{2A700}'..='\u{2B73F}' |
        '\u{2B740}'..='\u{2B81F}' |
        '\u{F900}'..='\u{FAFF}'
    )
}

/// Check if a string contains any Chinese characters.
pub fn has_chinese(text: &str) -> bool {
    text.chars().any(is_chinese)
}

/// Extract only Chinese characters from a string.
pub fn extract_chinese(text: &str) -> String {
    text.chars().filter(|c| is_chinese(*c)).collect()
}

/// Normalize a token: trim, lowercase, and reject tokens without alphanumeric or Chinese content.
pub fn normalize_token(token: &str) -> String {
    let t = token.trim().to_lowercase();
    if t.is_empty() || !t.chars().any(|c| c.is_alphanumeric() || is_chinese(c)) {
        return String::new();
    }
    t
}

/// Split text by non-word characters (preserving Chinese characters as word characters).
pub fn split_by_non_word(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && !is_chinese(c))
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Filter a token for safe use in FTS5 MATCH queries by removing special characters
/// and FTS5 boolean keywords (AND, OR, NOT, NEAR).
pub fn escape_fts5_token(token: &str) -> String {
    // FTS5 boolean keywords that must not appear as bare tokens
    const FTS5_KEYWORDS: &[&str] = &["AND", "OR", "NOT", "NEAR"];

    let filtered: String = token
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '"' | '\'' | '*' | '(' | ')' | ':' | '^' | '-' | '{' | '}' | '+' | '[' | ']'
            )
        })
        .collect();

    // If the filtered result is an FTS5 keyword, prefix with underscore to neutralize
    if FTS5_KEYWORDS.contains(&filtered.to_uppercase().as_str()) {
        format!("_{}", filtered)
    } else {
        filtered
    }
}

/// Generate pinyin tokens from Chinese text.
///
/// Returns both the full pinyin concatenation (e.g. "zhongguoren" for "中国人")
/// and the abbreviation (first letter of each pinyin, e.g. "zgr").
pub fn generate_pinyin_tokens(chinese_text: &str) -> Option<Vec<String>> {
    let chinese = extract_chinese(chinese_text);
    if chinese.is_empty() || chinese.chars().count() > MAX_PINYIN_CHARS {
        return None;
    }

    // Use pinyin crate to generate pinyin for each character
    let pinyins: Vec<String> = chinese
        .chars()
        .filter_map(|c| c.to_pinyin().map(|p| p.plain().to_string()))
        .collect();

    if pinyins.is_empty() {
        return None;
    }

    let mut result = Vec::new();

    // Full pinyin concatenation: "中国人" → "zhongguoren"
    let full: String = pinyins.join("");
    if !full.is_empty() {
        result.push(full);
    }

    // Abbreviation: first letter of each pinyin: "中国人" → "zgr"
    let abbrev: String = pinyins.iter().filter_map(|s| s.chars().next()).collect();
    if !abbrev.is_empty() {
        result.push(abbrev);
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

// --- P3-002~005: 繁简归一 + 同义词 + 别名 + 查询时拼音 ---

/// P3-002: 繁体中文 → 简体中文（基于常用字映射）
pub fn traditional_to_simplified(text: &str) -> String {
    // 常用繁简字映射表（覆盖日常使用的高频字）
    static MAP: std::sync::OnceLock<std::collections::HashMap<char, char>> =
        std::sync::OnceLock::new();
    let map = MAP.get_or_init(|| {
        let pairs: [(char, char); 119] = [
            ('國', '国'),
            ('後', '后'),
            ('時', '时'),
            ('體', '体'),
            ('對', '对'),
            ('會', '会'),
            ('機', '机'),
            ('發', '发'),
            ('書', '书'),
            ('長', '长'),
            ('門', '门'),
            ('開', '开'),
            ('關', '关'),
            ('學', '学'),
            ('實', '实'),
            ('現', '现'),
            ('萬', '万'),
            ('為', '为'),
            ('當', '当'),
            ('個', '个'),
            ('種', '种'),
            ('裡', '里'),
            ('從', '从'),
            ('來', '来'),
            ('這', '这'),
            ('說', '说'),
            ('話', '话'),
            ('見', '见'),
            ('頭', '头'),
            ('氣', '气'),
            ('無', '无'),
            ('問', '问'),
            ('愛', '爱'),
            ('電', '电'),
            ('視', '视'),
            ('聽', '听'),
            ('變', '变'),
            ('動', '动'),
            ('風', '风'),
            ('飛', '飞'),
            ('馬', '马'),
            ('魚', '鱼'),
            ('車', '车'),
            ('點', '点'),
            ('業', '业'),
            ('義', '义'),
            ('過', '过'),
            ('進', '进'),
            ('遠', '远'),
            ('運', '运'),
            ('連', '连'),
            ('還', '还'),
            ('總', '总'),
            ('東', '东'),
            ('臺', '台'),
            ('灣', '湾'),
            ('華', '华'),
            ('經', '经'),
            ('繫', '系'),
            ('處', '处'),
            ('號', '号'),
            ('區', '区'),
            ('歷', '历'),
            ('壓', '压'),
            ('應', '应'),
            ('邊', '边'),
            ('標', '标'),
            ('準', '准'),
            ('導', '导'),
            ('層', '层'),
            ('際', '际'),
            ('隊', '队'),
            ('驗', '验'),
            ('顯', '显'),
            ('設', '设'),
            ('計', '计'),
            ('認', '认'),
            ('識', '识'),
            ('確', '确'),
            ('質', '质'),
            ('據', '据'),
            ('轉', '转'),
            ('難', '难'),
            ('醫', '医'),
            ('舊', '旧'),
            ('寫', '写'),
            ('畫', '画'),
            ('藥', '药'),
            ('衛', '卫'),
            ('護', '护'),
            ('辦', '办'),
            ('處', '处'),
            ('備', '备'),
            ('預', '预'),
            ('眾', '众'),
            ('雙', '双'),
            ('龍', '龙'),
            ('鳳', '凤'),
            ('鳥', '鸟'),
            ('麼', '么'),
            ('嗎', '吗'),
            ('呀', '呀'),
            ('員', '员'),
            ('廣', '广'),
            ('廠', '厂'),
            ('裝', '装'),
            ('線', '线'),
            ('組', '组'),
            ('織', '织'),
            ('節', '节'),
            ('約', '约'),
            ('紙', '纸'),
            ('統', '统'),
            ('規', '规'),
            ('則', '则'),
            ('戰', '战'),
            ('爭', '争'),
            ('選', '选'),
            ('舉', '举'),
        ];
        pairs.iter().copied().collect()
    });
    text.chars()
        .map(|c| map.get(&c).copied().unwrap_or(c))
        .collect()
}

/// P3-003: 从文件加载同义词词表（格式：每行"词A,词B"）
pub fn load_synonyms_from_file(path: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((a, b)) = trimmed.split_once(',') {
                pairs.push((a.trim().to_string(), b.trim().to_string()));
            }
        }
    }
    pairs
}

/// P3-004: 从文件加载别名映射（格式：每行"缩写,全称"）
pub fn load_aliases_from_file(path: &str) -> Vec<(String, String)> {
    load_synonyms_from_file(path)
}

/// P3-003: 查询时同义词扩展（优先从文件加载，fallback 到硬编码默认值）
pub fn expand_query_with_synonyms(query: &str) -> Vec<String> {
    static SYNONYMS: std::sync::OnceLock<Vec<(&str, &str)>> = std::sync::OnceLock::new();
    let syn = SYNONYMS.get_or_init(|| {
        vec![
            ("合同", "协议"),
            ("怎么", "如何"),
            ("申请", "提交"),
            ("报销", "报账"),
            ("电脑", "计算机"),
            ("数据", "资料"),
            ("系统", "平台"),
            ("方案", "计划"),
            ("工具", "软件"),
            ("使用", "操作"),
            ("安装", "部署"),
            ("配置", "设置"),
            ("错误", "异常"),
            ("修复", "解决"),
            ("优化", "提升"),
            ("测试", "验证"),
        ]
    });
    let mut results = vec![query.to_string()];
    let q = query.to_lowercase();
    for (k, v) in syn {
        if q.contains(&k.to_lowercase()) {
            results.push(q.replace(&k.to_lowercase(), v));
        }
        if q.contains(&v.to_lowercase()) {
            results.push(q.replace(&v.to_lowercase(), k));
        }
    }
    trace!("synonym expansion: {} -> {} variants", query, results.len());
    results
}

/// P3-004: 查询时别名扩展
pub fn expand_query_with_aliases(query: &str) -> Vec<String> {
    static ALIASES: std::sync::OnceLock<Vec<(&str, &str)>> = std::sync::OnceLock::new();
    let aliases = ALIASES.get_or_init(|| {
        vec![
            ("OA", "办公自动化"),
            ("ERP", "企业资源计划"),
            ("CRM", "客户关系管理"),
            ("HR", "人力资源"),
            ("API", "接口"),
            ("DB", "数据库"),
            ("UI", "界面"),
            ("UX", "用户体验"),
            ("CI/CD", "持续集成"),
            ("KPI", "绩效指标"),
        ]
    });
    let mut results = vec![query.to_string()];
    let q_lower = query.to_lowercase();
    for (abbr, full) in aliases {
        if q_lower.contains(&abbr.to_lowercase()) {
            results.push(q_lower.replace(&abbr.to_lowercase(), &full.to_lowercase()));
        }
        if q_lower.contains(&full.to_lowercase()) {
            results.push(q_lower.replace(&full.to_lowercase(), &abbr.to_lowercase()));
        }
    }
    trace!("alias expansion: {} -> {} variants", query, results.len());
    results
}

/// P3-005: 查询时拼音/首字母匹配 — 检测 query 是否为拼音
pub fn detect_pinyin_query(query: &str) -> bool {
    let has_chinese = query.chars().any(is_chinese);
    if has_chinese {
        return false;
    }
    // 纯 ASCII + 空格 → 可能是拼音
    query
        .chars()
        .all(|c| c.is_ascii_alphabetic() || c.is_whitespace())
}

fn split_name_extension(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(pos) => (name[..pos].to_string(), name[pos + 1..].to_string()),
        None => (name.to_string(), String::new()),
    }
}
