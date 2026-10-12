//! `WorkflowRun`：Layer 3 动态工作流的最小执行器（ZCode dynamic-workflow 同源移植）。
//!
//! 主代理提交一段 **确定性 JS 脚本**，引擎在嵌入的 QuickJS-ng 沙箱里执行：
//! 脚本经 `agent(name).ask(question)` facade 驱动子代理，每次 ask 调
//! [`AgentToolRuntime::run_task`] 并把问答写入 `agent/workflows/<run_id>.jsonl`
//! （含 FNV-1a **inputHash** 与事件序）。
//!
//! 确定性骨架（v1 边界）：
//! - 沙箱 prelude 把 `Date.now`/`Math.random` 固定并冻结 `Date`/`Math`；不注册任何
//!   模块加载器，动态 `import()` 直接失败；
//! - v1 ask 是阻塞语义（脚本串行扇出；`Promise.all` 并行需 async 运行时绑定，后续迭代）；
//! - **断点重放 = 重跑脚本**：resume 时按 (actor, inputHash) 从 journal 直接结算已答
//!   ask（provider 零调用），脚本 FNV hash 不一致则拒绝（amend 语义后续迭代）。
//!
//! 类型检查（facade .d.ts）与 set-order 重放为后续迭代项。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use rquickjs::{AsyncContext, AsyncRuntime, Context, Function, Runtime};
use rquickjs::prelude::Async as AsyncFn;
use serde_json::{json, Value};

use super::subagent::get_str;
use super::*;

/// 引擎沙箱的内存上限（QuickJS 级，防脚本失控）。
const ENGINE_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// 引擎墙钟上限：超时经中断处理器终止脚本。
const ENGINE_TIMEOUT: Duration = Duration::from_secs(120);
/// 并行 ask 上限（`Promise.all` 扇出的信号量许可数；default 4，硬顶 8）。
const DEFAULT_MAX_PARALLEL_ASKS: u32 = 4;
const MAX_MAX_PARALLEL_ASKS: u32 = 8;
/// 单次 ask 的子代理工具迭代上限（对齐计划步骤的收敛口径）。
const WORKFLOW_ASK_MAX_TOOL_ITER: usize = 64;
const MAX_SCRIPT_CHARS: usize = 100_000;
const MAX_QUESTION_CHARS: usize = 4_000;
const MAX_ANSWER_CHARS: usize = 4_000;

/// facade 声明（.d.ts 语义的文本形态）：脚本契约一览，随诊断一并回给模型。
pub(crate) const FACADE_DTS: &str = r#"
// WorkflowRun 脚本可用的全局 facade（确定性沙箱，无模块加载）。
// 可用：await / Promise.all / while / try-catch（脚本被包进 async IIFE）。
// 禁用：Date / Math.random 赋值 / import / eval / new Function / require /
//       setTimeout / setInterval / process —— 提交前静态检查直接拒绝。
declare function agent(name: string): {
  /** 向子代理提问并等待回答（promise）。问题文本即输入指纹（inputHash）。 */
  ask(question: string): Promise<string>;
  /** 同 ask，但答案按 JSON 解析并按 schema（JSON Schema 子集）校验后返回对象。 */
  askJson(question: string, schema: object): Promise<object>;
};
declare function result(text: string): void;
"#;

/// 一条 facade 诊断（行/列从 1 计）。
#[derive(Debug, Clone)]
pub(crate) struct FacadeDiagnostic {
    pub(crate) line: usize,
    pub(crate) col: usize,
    pub(crate) message: String,
}

/// 禁用模式：静态拒绝破坏确定性与沙箱逃逸面的写法（运行时钉死之外的二道防线）。
const BANNED_PATTERNS: &[(&str, &str)] = &[
    ("Date.now", "时间必须确定：脚本内禁用 Date.now（沙箱已钉死为 0）"),
    ("Date.prototype", "禁用 Date.prototype（沙箱已钉死时间源）"),
    ("new Date", "时间必须确定：脚本内禁用 new Date"),
    ("Math.random =", "禁用改写 Math.random（沙箱已钉死并冻结）"),
    ("eval(", "禁用 eval（沙箱逃逸面）"),
    ("new Function", "禁用 new Function（沙箱逃逸面）"),
    ("require(", "禁用 require（无模块加载器）"),
    ("import(", "禁用动态 import（无模块加载器，永不 resolve）"),
    ("setTimeout(", "禁用 setTimeout（无事件循环定时器）"),
    ("setInterval(", "禁用 setInterval（无事件循环定时器）"),
    ("process.", "禁用 process（无 Node 环境）"),
];

/// facade 静态检查：QuickJS 语法解析（AsyncFunction 只解析不执行）+ 禁用模式扫描；
/// `type_check=true` 且宿主 node/tsc 在位时叠加 **tsc 类型级检查**
/// （`--noEmit` 对脚本 + facade.d.ts），不可用则静默降级（两级共用
/// FacadeDiagnostic 格式）。带诊断的脚本在提交期直接拒绝（诊断只回给模型，不落用户）。
pub(crate) fn check_script(script: &str, type_check: bool) -> Vec<FacadeDiagnostic> {
    let mut diagnostics: Vec<FacadeDiagnostic> = Vec::new();

    // 1) 语法解析：new Function(body) 仅解析不执行；包装后 user script 是函数体。
    let syntax_probe = {
        let runtime = Runtime::new();
        match runtime {
            Ok(runtime) => {
                let context = Context::full(&runtime);
                match context {
                    Ok(context) => context.with(|ctx| {
                        let encoded = serde_json::to_string(script)
                            .unwrap_or_else(|_| "\"\"".to_string());
                        // 引擎把脚本包进 async IIFE：语法探针须用 AsyncFunction
                        //（普通 Function 体里 await 是语法错误，会误报）。
                        let probe = format!(
                            "const __AF = Object.getPrototypeOf(async function () {{}}).constructor;                             new __AF({encoded}); 1"
                        );
                        ctx.eval::<rquickjs::Value, _>(probe).is_err()
                    }),
                    Err(_) => false,
                }
            }
            Err(_) => false,
        }
    };
    if syntax_probe {
        diagnostics.push(FacadeDiagnostic {
            line: 1,
            col: 1,
            message: "语法错误：脚本无法被 QuickJS 解析（只允许普通 JS 语法；                      TypeScript 类型注解需宿主启用类型级检查）"
                .to_string(),
        });
    }

    // 2) 禁用模式扫描（真实行/列）。
    for (pattern, reason) in BANNED_PATTERNS {
        let mut search_from = 0usize;
        while let Some(found) = script[search_from..].find(pattern) {
            let absolute = search_from + found;
            let before = &script[..absolute];
            let line = before.matches('\n').count() + 1;
            let col = before
                .rsplit('\n')
                .next()
                .map(|line_text| line_text.chars().count() + 1)
                .unwrap_or(1);
            diagnostics.push(FacadeDiagnostic {
                line,
                col,
                message: format!("{reason}（{pattern}）"),
            });
            search_from = absolute + pattern.len();
        }
    }
    // 3) 类型级检查（可选增强）：node/tsc 在位时叠加；不可用静默降级——
    // 上面的语法+lint 结果照常生效（两级共用 FacadeDiagnostic）。
    if type_check && diagnostics.is_empty() {
        if let Some(mut tsc_diagnostics) = type_check_with_tsc(script) {
            diagnostics.append(&mut tsc_diagnostics);
        }
    }
    diagnostics
}

/// 格式化诊断列表（模型可读；含 facade 指引）。
pub(crate) fn format_diagnostics(diagnostics: &[FacadeDiagnostic]) -> String {
    let mut report = format!(
        "脚本未通过 facade 检查（{} 项诊断，未执行）。可用 facade：
{}
诊断：",
        diagnostics.len(),
        FACADE_DTS.trim()
    );
    for diagnostic in diagnostics {
        report.push_str(&format!(
            "
- L{}:C{} {}",
            diagnostic.line, diagnostic.col, diagnostic.message
        ));
    }
    report
}

// ---------------------------------------------------------------------------
// tsc 可选类型级检查（Node 工具链，自动降级）
// ---------------------------------------------------------------------------

/// 带超时地运行外部命令；超时 kill 子进程并返回 None。
fn run_with_timeout(
    mut command: std::process::Command,
    timeout: Duration,
) -> Option<std::process::Output> {
    use std::process::Stdio;
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child.wait_with_output().ok();
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

/// 探测宿主 `node` 可用性（`node --version`，2s 超时；结果进程级缓存）。
fn node_available() -> bool {
    static NODE: StdMutex<Option<Option<bool>>> = StdMutex::new(None);
    let mut cache = NODE.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(cached) = *cache {
        return cached.unwrap_or(false);
    }
    let available = run_with_timeout(
        {
            #[cfg(windows)]
            {
                let mut command = std::process::Command::new("cmd");
                command.args(["/C", "node", "--version"]);
                command
            }
            #[cfg(not(windows))]
            {
                std::process::Command::new("node").arg("--version")
            }
        },
        Duration::from_secs(2),
    )
    .map(|output| output.status.success())
    .unwrap_or(false);
    *cache = Some(Some(available));
    available
}

/// 探测宿主 `tsc`（全局 typescript）可用性：node 在位且 `tsc --version` 成功。
fn tsc_available() -> bool {
    if !node_available() {
        return false;
    }
    static TSC: StdMutex<Option<Option<bool>>> = StdMutex::new(None);
    let mut cache = TSC.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(cached) = *cache {
        return cached.unwrap_or(false);
    }
    let available = run_with_timeout(
        {
            #[cfg(windows)]
            {
                let mut command = std::process::Command::new("cmd");
                command.args(["/C", "tsc", "--version"]);
                command
            }
            #[cfg(not(windows))]
            {
                std::process::Command::new("tsc").arg("--version")
            }
        },
        Duration::from_secs(10),
    )
    .map(|output| output.status.success())
    .unwrap_or(false);
    *cache = Some(Some(available));
    available
}

/// 解析 tsc 输出行（`--pretty false` 格式：`path(line,col): error TSxxxx: message`），
/// 只保留 script.ts 的条目（facade.d.ts 的报错属引擎自身问题，不归咎脚本）。
fn parse_tsc_diagnostics(
    output: &str,
    script_file_name: &str,
) -> Vec<FacadeDiagnostic> {
    let mut diagnostics = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        let Some(open) = line.rfind('(') else {
            continue;
        };
        let (file, rest) = line.split_at(open);
        let Some(close) = rest.find(')') else {
            continue;
        };
        let position = &rest[1..close];
        let Some((line_no, col_no)) = position.split_once(',') else {
            continue;
        };
        let Ok(line_no) = line_no.parse::<usize>() else {
            continue;
        };
        let Ok(col_no) = col_no.parse::<usize>() else {
            continue;
        };
        let tail = &rest[close + 1..];
        // tail: " error TS2304: Cannot find name 'x'."
        let Some(tail) = tail.strip_prefix(" error ") else {
            continue;
        };
        let file_stem = Path::new(file)
            .file_stem()
            .map(|stem| stem.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if file_stem != script_file_name {
            continue;
        }
        diagnostics.push(FacadeDiagnostic {
            line: line_no,
            col: col_no,
            message: format!("TS 检查：{tail}"),
        });
    }
    diagnostics
}

/// 类型级检查（可选增强）：把脚本 + facade.d.ts 写进临时目录，spawn `tsc --noEmit`
/// 解析诊断。宿主无 node / tsc / 超时 → None（调用方静默降级为语法+lint 检查）。
fn type_check_with_tsc(script: &str) -> Option<Vec<FacadeDiagnostic>> {
    if !tsc_available() {
        return None;
    }
    let dir = std::env::temp_dir().join(format!(
        "rclaw-workflow-tsc-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let script_path = dir.join("script.ts");
    let facade_path = dir.join("facade.d.ts");
    std::fs::write(&script_path, script).ok()?;
    std::fs::write(&facade_path, FACADE_DTS).ok()?;
    let output = run_with_timeout(
        {
            #[cfg(windows)]
            {
                let mut command = std::process::Command::new("cmd");
                command.args([
                    "/C",
                    "tsc",
                    "--noEmit",
                    "--strict",
                    "--target",
                    "es2022",
                    "--module",
                    "esnext",
                    "--moduleResolution",
                    "bundler",
                    "--pretty",
                    "false",
                ]);
                command.arg(script_path.as_os_str());
                command.arg(facade_path.as_os_str());
                command
            }
            #[cfg(not(windows))]
            {
                let mut command = std::process::Command::new("tsc");
                command.args([
                    "--noEmit",
                    "--strict",
                    "--target",
                    "es2022",
                    "--module",
                    "esnext",
                    "--moduleResolution",
                    "bundler",
                    "--pretty",
                    "false",
                ]);
                command.arg(script_path.as_os_str());
                command.arg(facade_path.as_os_str());
                command
            }
        },
        Duration::from_secs(30),
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    Some(parse_tsc_diagnostics(&stdout, "script"))
}

// ---------------------------------------------------------------------------
// askJson：泛型 T→JSON Schema 的答案校验（schema 子集）
// ---------------------------------------------------------------------------

/// 极简 JSON Schema 子集校验（递归）：type（string/number/boolean/object/array）、
/// object.properties/required、array.items。覆盖 facade 契约场景；超出子集的字段忽略。
fn validate_json_schema(value: &Value, schema: &Value, depth: usize) -> Result<(), String> {
    if depth > 8 {
        return Ok(()); // 深度防御：schema 递归上限
    }
    let expected = schema.get("type").and_then(Value::as_str);
    match expected {
        Some(name) if name != "any" => {
            let matches = match name {
                "string" => value.is_string(),
                "number" => value.is_number(),
                "boolean" => value.is_boolean(),
                "object" => value.is_object(),
                "array" => value.is_array(),
                "null" => value.is_null(),
                _ => true,
            };
            if !matches {
                return Err(format!("类型不符：期望 {name}，实际 {value}"));
            }
        }
        _ => {}
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        if let Some(object) = value.as_object() {
            for key in required {
                if let Some(key) = key.as_str() {
                    if !object.contains_key(key) {
                        return Err(format!("缺少必填字段：{key}"));
                    }
                }
            }
        }
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        if let Some(object) = value.as_object() {
            for (key, subschema) in properties {
                if let Some(field) = object.get(key) {
                    validate_json_schema(field, subschema, depth + 1)
                        .map_err(|error| format!("{key}: {error}"))?;
                }
            }
        }
    }
    if let Some(items) = schema.get("items") {
        if let Some(array) = value.as_array() {
            for (index, item) in array.iter().enumerate() {
                validate_json_schema(item, items, depth + 1)
                    .map_err(|error| format!("[{index}]: {error}"))?;
            }
        }
    }
    Ok(())
}

/// 脚本静态洞察（文本级近似，非 AST）：actor 清单、ask 总数、是否含并行扇出。
/// 用于 run_started 事件与执行报告的可观测性——不是 ZCode 那种 AST 级依赖图。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct WorkflowInsight {
    pub(crate) actors: Vec<String>,
    pub(crate) ask_count: usize,
    pub(crate) parallel: bool,
    /// 依赖图（节点/边/波次）。
    pub(crate) dependency_graph: DependencyGraph,
}

/// 文本级静态分析：扫描 `agent("...")` 提取 actor；扫描 `.ask(` 计数；
/// `Promise.all` / `await Promise` 判定并行。
pub(crate) fn analyze_workflow_script(script: &str) -> WorkflowInsight {
    let mut actors: Vec<String> = Vec::new();
    let mut search_from = 0usize;
    while let Some(found) = script[search_from..].find("agent(") {
        let rest = &script[search_from + found + "agent(".len()..];
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix('"') {
            if let Some(close) = rest.find('"') {
                let name = &rest[..close];
                if !name.is_empty() && !actors.iter().any(|existing| existing == name) {
                    actors.push(name.to_string());
                }
            }
        }
        search_from += found + "agent(".len();
    }
    let ask_count = script.matches(".ask(").count();
    let parallel = script.contains("Promise.all");
    let dependency_graph = analyze_dependency_graph(script);
    WorkflowInsight {
        actors,
        ask_count,
        parallel,
        dependency_graph,
    }
}

// ---------------------------------------------------------------------------
// 依赖图静态分析（Layer 3：脚本 → DAG）
// ---------------------------------------------------------------------------

/// 依赖图节点：一个 ask 调用点。
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DependencyNode {
    /// 节点 id（`ask-<序号>`，按行序）。
    pub(crate) id: String,
    /// 发起 ask 的 actor 名。
    pub(crate) actor: String,
    /// 所在行号（1 起）。
    pub(crate) line: usize,
    /// 是否位于 Promise.all（并行波内）。
    pub(crate) parallel: bool,
}

/// 依赖图：节点 = ask 调用点，边 = 「必须后于」约束，waves = 拓扑层级。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct DependencyGraph {
    pub(crate) nodes: Vec<DependencyNode>,
    /// 边 (from, to)：from 完成后 to 才可运行。
    pub(crate) edges: Vec<(String, String)>,
    /// 拓扑波次：同一波内的节点可并行（无内部边）。
    pub(crate) waves: Vec<Vec<String>>,
}

/// 依赖图静态分析（语句级文本解析，QuickJS 兼容 JS 子集）：
/// - `const <handle> = agent("<actor>")` → 句柄绑定；
/// - `await <handle>.ask(...)` / `<handle>.ask(...)` → ask 节点；
/// - `Promise.all([...])` 同一句内的多个 ask → 并行波（内部无边）；
/// - 相邻语句的 ask → 顺序边（前一波 → 后一波）。
///
/// 这是文本级近似（无完整 AST）：不解析任意表达式形态，产出的是真实的
/// 节点/边/波次 DAG，供调度参照与可观测性。
pub(crate) fn analyze_dependency_graph(script: &str) -> DependencyGraph {
    let mut bindings: Vec<(String, String)> = Vec::new(); // (handle, actor)
    let mut nodes: Vec<DependencyNode> = Vec::new();

    for (idx, raw_line) in script.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        // 句柄绑定。
        if let Some(rest) = line.strip_prefix("const ") {
            if let Some((handle, actor)) = parse_agent_binding(rest) {
                bindings.push((handle, actor));
            }
        }
        // Promise.all 波：同一行内的多个 ask 标记并行（内部无边）。
        let in_promise_all = line.contains("Promise.all");
        for mut node in asks_on_line(line, &bindings, line_no) {
            if in_promise_all {
                node.parallel = true;
            }
            nodes.push(node);
        }
    }

    // 边：非并行的相邻 ask/wave 之间按行序连边（前 → 后）。
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut last_boundary: Option<String> = None; // 上一个「波/节点」的代表出口
    let mut i = 0usize;
    while i < nodes.len() {
        if nodes[i].parallel {
            // 收集连续的并行节点为同一波。
            let wave: Vec<usize> = {
                let mut group = vec![i];
                let line = nodes[i].line;
                let mut j = i + 1;
                while j < nodes.len() && nodes[j].parallel && nodes[j].line == line {
                    group.push(j);
                    j += 1;
                }
                group
            };
            let last = *wave.last().unwrap();
            if let Some(prev) = &last_boundary {
                for &member in &wave {
                    edges.push((prev.clone(), nodes[member].id.clone()));
                }
            }
            last_boundary = Some(nodes[last].id.clone());
            i = last + 1;
        } else {
            if let Some(prev) = &last_boundary {
                edges.push((prev.clone(), nodes[i].id.clone()));
            }
            last_boundary = Some(nodes[i].id.clone());
            i += 1;
        }
    }

    // 波次：Kahn 分层（入度 0 → 层 1，依边推进）。
    let waves = topo_waves(&nodes, &edges);

    DependencyGraph {
        nodes,
        edges,
        waves,
    }
}

/// 解析句柄绑定：`const <handle> = agent("<actor>")` → (handle, actor)。
fn parse_agent_binding(rest: &str) -> Option<(String, String)> {
    let handle_end = rest.find('=')?;
    let handle = rest[..handle_end].trim().to_string();
    if handle.is_empty() {
        return None;
    }
    let rhs = &rest[handle_end + 1..].trim_start();
    let rhs = rhs.strip_prefix("agent(")?;
    let actor_start = rhs.find('"')? + 1;
    let actor_end = rhs[actor_start..].find('"')? + actor_start;
    let actor = rhs[actor_start..actor_end].to_string();
    if actor.is_empty() {
        return None;
    }
    Some((handle, actor))
}

/// 解析一行内的 ask 调用点（`<handle>.ask(...)`，可多个，逗号/括号分隔均可——
/// 只提取句柄与行号，不解析参数）。
fn asks_on_line(
    line: &str,
    bindings: &[(String, String)],
    line_no: usize,
) -> Vec<DependencyNode> {
    let mut nodes = Vec::new();
    let mut search_from = 0usize;
    while let Some(found) = line[search_from..].find(".ask(") {
        let prefix = &line[..search_from + found];
        // 句柄 = .ask( 前最近的标识符。
        let handle: String = prefix
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let actor = bindings
            .iter()
            .rev()
            .find(|(bound, _)| *bound == handle)
            .map(|(_, actor)| actor.clone())
            .unwrap_or_else(|| handle.clone());
        nodes.push(DependencyNode {
            id: format!("ask-{}", line_no),
            actor,
            line: line_no,
            parallel: false,
        });
        search_from += found + ".ask(".len();
    }
    nodes
}

/// Kahn 分层：波次 = 拓扑层级（同层可并行）。
fn topo_waves(nodes: &[DependencyNode], edges: &[(String, String)]) -> Vec<Vec<String>> {
    let ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
    let mut indegree: HashMap<String, usize> = ids.iter().map(|id| (id.clone(), 0)).collect();
    let mut outgoing: HashMap<String, Vec<String>> = HashMap::new();
    for (from, to) in edges {
        *indegree.entry(to.clone()).or_insert(0) += 1;
        outgoing.entry(from.clone()).or_default().push(to.clone());
    }
    let mut waves = Vec::new();
    let mut frontier: Vec<String> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut processed = 0usize;
    while !frontier.is_empty() {
        frontier.sort();
        waves.push(frontier.clone());
        processed += frontier.len();
        let mut next_frontier = Vec::new();
        for id in &frontier {
            if let Some(targets) = outgoing.get(id) {
                for target in targets {
                    if let Some(degree) = indegree.get_mut(target) {
                        *degree = degree.saturating_sub(1);
                        if *degree == 0 {
                            next_frontier.push(target.clone());
                        }
                    }
                }
            }
        }
        frontier = next_frontier;
    }
    let _ = processed;
    waves
}

/// FNV-1a 64 位：ask 输入指纹（ZCode inputHash 同算法）。
fn fnv1a64(data: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// journal：run_started / ask / answer / run_completed（inputHash = FNV-1a64）
// ---------------------------------------------------------------------------

fn workflow_journal_path(layout: &Layout, run_id: &str) -> PathBuf {
    layout.base.join("workflows").join(format!("{run_id}.jsonl"))
}

fn append_workflow_event(path: &Path, event: &Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        tracing::warn!("workflow journal 打开失败: {}", path.display());
        return;
    };
    let _ = writeln!(file, "{event}");
}

/// 重放队列条目：答案 + 原完成序（answer 事件在 journal 中的序号，0 起）。
#[derive(Debug, Clone)]
struct ReplayAnswer {
    answer: String,
    order: u64,
}

/// journal 里的问答重放表：(actor, input_hash) → 答案队列（按原完成序排列）。
type ReplayTable = HashMap<(String, u64), VecDeque<ReplayAnswer>>;

/// 从 journal 重建 scriptHash 与重放表。
fn load_workflow_journal(path: &Path) -> Result<(u64, ReplayTable), String> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        format!(
            "读取 workflow journal 失败（{}）：{error}。resume 需要已存在的 run_id（见先前 WorkflowRun 结果）。",
            path.display()
        )
    })?;
    let mut script_hash: Option<u64> = None;
    let mut table: ReplayTable = HashMap::new();
    let mut open_questions: HashMap<u64, String> = HashMap::new();
    // 全局 answer 计数：order = 该 ask 在原完成顺序中的全局位次（0 起）。
    let mut answer_seen: u64 = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event.get("event").and_then(Value::as_str) {
            Some("run_started") | Some("run_resumed") => {
                if let Some(hash) = event
                    .get("script_hash")
                    .and_then(Value::as_str)
                    .and_then(|hash| u64::from_str_radix(hash, 16).ok())
                {
                    script_hash.get_or_insert(hash);
                }
            }
            Some("ask") => {
                if let (Some(actor), Some(hash)) = (
                    event.get("actor").and_then(Value::as_str),
                    event.get("input_hash").and_then(Value::as_str),
                ) {
                    if let Ok(hash) = u64::from_str_radix(hash, 16) {
                        open_questions.insert(hash, actor.to_string());
                    }
                }
            }
            Some("answer") => {
                let (Some(hash_text), Some(answer)) = (
                    event.get("input_hash").and_then(Value::as_str),
                    event.get("answer").and_then(Value::as_str),
                ) else {
                    continue;
                };
                let Ok(hash) = u64::from_str_radix(hash_text, 16) else {
                    continue;
                };
                let actor = open_questions.get(&hash).cloned().unwrap_or_default();
                // 文件序 = 完成序：answer_ord 即该 ask 在原完成顺序中的位次，
                // 重放时经 JS 顺序门按此位次延迟 resolve（Promise.race 保真）。
                let order = answer_seen;
                table
                    .entry((actor.clone(), hash))
                    .or_default()
                    .push_back(ReplayAnswer {
                        answer: answer.to_string(),
                        order,
                    });
                answer_seen += 1;
            }
            _ => {}
        }
    }
    let script_hash = script_hash.ok_or_else(|| {
        format!(
            "workflow journal（{}）里没有 run_started 记录，无法 resume",
            path.display()
        )
    })?;
    Ok((script_hash, table))
}

// ---------------------------------------------------------------------------
// 确定性沙箱（AsyncRuntime 异步驱动）+ facade
// ---------------------------------------------------------------------------

/// 沙箱 + 确定性 prelude（AsyncRuntime 异步驱动）；`register_hosts` 在 prelude 之后、
/// 用户脚本之前注册宿主函数。用户脚本被包进 async IIFE（顶层 await / Promise.all
/// 可用），完成或抛错经 `__script_done` 回传。
async fn eval_deterministic(
    script: String,
    register_hosts: impl FnOnce(&rquickjs::Ctx) + Send + 'static,
) -> Result<(), String> {
    eprintln!("[E1] async runtime creating");
    let runtime =
        AsyncRuntime::new().map_err(|error| format!("QuickJS 运行时创建失败: {error}"))?;
    runtime.set_memory_limit(ENGINE_MEMORY_LIMIT).await;
    runtime.set_max_stack_size(1024 * 1024).await;
    let deadline = Instant::now() + ENGINE_TIMEOUT;
    runtime
        .set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)))
        .await;
    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|error| format!("QuickJS 上下文创建失败: {error}"))?;
    let js_error = |ctx: &rquickjs::Ctx, error: rquickjs::Error| -> String {
        let caught = ctx.catch();
        let detail = caught
            .get::<String>()
            .unwrap_or_else(|_| "exception".to_string());
        format!("{error}: {detail}")
    };
    context
        .async_with(|ctx| Box::pin(async move {
            // 确定性 prelude：固定时间与随机源并冻结（篡改赋值被 QuickJS 抛
            // TypeError 或静默忽略，钉死值始终生效）；不注册模块加载器 →
            // 动态 import() 永不 resolve，模块内容不可得。
            let prelude = r#"
                Date.now = function () { return 0; };
                Date.prototype.getTime = function () { return 0; };
                Date.prototype.valueOf = function () { return 0; };
                Math.random = function () { return 0.5; };
                Object.freeze(Date);
                Object.freeze(Math);
                // Layer 3 set-order 顺序门：重放命中的 ask 按原完成序（order）延迟
                // resolve——Promise.race / 交错 await 的解析顺序与原完成顺序一致。
                // 游标 __orderNext = 下一个待释放的 order；逐级微任务泵送：
                // order N 的下游回调先于 order N+1 的解析入队（保序的关键）。
                // 迟到的旧序（< next）直接放行；新执行（非重放）的 ask 不经门。
                var __orderNext = 0;
                var __orderWaiters = {};
                function __gate_pump() {
                    var waiters = __orderWaiters[__orderNext];
                    if (!waiters) { return; }
                    delete __orderWaiters[__orderNext];
                    __orderNext += 1;
                    Promise.resolve().then(function () {
                        waiters.forEach(function (release) { release(); });
                        __gate_pump();
                    });
                }
                function __gate_wait(index) {
                    if (index < __orderNext) {
                        return Promise.resolve();
                    }
                    if (index === __orderNext) {
                        __orderNext += 1;
                        __gate_pump();
                        return Promise.resolve();
                    }
                    return new Promise(function (release) {
                        (__orderWaiters[index] = __orderWaiters[index] || []).push(release);
                    });
                }
                function agent(name) {
                    name = String(name);
                    if (!name) { throw new Error("agent() 需要 actor 名"); }
                    return {
                        name: name,
                        ask: function (question) {
                            return __ask_raw(name, String(question)).then(function (raw) {
                                var parsed = JSON.parse(raw);
                                if (!parsed.ok) { throw new Error(parsed.error || "ask 失败"); }
                                if (parsed.replayed) {
                                    return __gate_wait(parsed.order || 0).then(function () {
                                        return parsed.answer;
                                    });
                                }
                                return parsed.answer;
                            });
                        },
                        askJson: function (question, schema) {
                            return askJsonPair(name).askJson(question, schema);
                        }
                    };
                }
                function result(text) { __result(String(text)); }
                function askJsonPair(actor) {
                    return {
                        askJson: function (question, schema) {
                            return __ask_json_raw(
                                actor,
                                String(question),
                                JSON.stringify(schema)
                            ).then(function (raw) {
                                var parsed = JSON.parse(raw);
                                if (!parsed.ok) { throw new Error(parsed.error || "askJson 失败"); }
                                if (parsed.replayed) {
                                    return __gate_wait(parsed.order || 0).then(function () {
                                        return parsed.answer;
                                    });
                                }
                                return parsed.answer;
                            });
                        }
                    };
                }
            "#;
            ctx.eval::<(), _>(prelude)
                .map_err(|error| format!("prelude 执行失败: {}", js_error(&ctx, error)))?;
            register_hosts(&ctx);
            // async IIFE：顶层 await / Promise.all 可用；完成/抛错经 __script_done 回传
            //（未捕获异常不会丢：reject 分支把它转为 ERROR 标记）。
            let wrapped = format!(
                "(async () => {{\n{script}\n}})().then(\n\
                 function (value) {{ __script_done(String(value)); }},\n\
                 function (error) {{ __script_done(\"ERROR: \" + String(error)); }}\n);"
            );
            ctx.eval::<rquickjs::Value, _>(wrapped)
                .map_err(|error| format!("脚本执行失败: {}", js_error(&ctx, error)))?;
            Ok::<(), String>(())
        }))
        .await?;
    eprintln!("[E2] script evaluated; driving idle");
    // 驱动全部 JS 任务与宿主 future（ask 子代理、Promise 链）到静止。
    runtime.idle().await;
    eprintln!("[E3] idle done");
    Ok(())
}

// ---------------------------------------------------------------------------
// ask 宿主回调 / 子代理执行
// ---------------------------------------------------------------------------

/// 一次 ask 的宿主回调（async，经 rquickjs 包装为 promise）：重放命中直接结算；
/// 否则取并发许可、驱动子代理并写 journal。并行扇出由 Promise.all 表达，
/// 信号量（`max_parallel_asks`）限流。
async fn ask_host(
    runtime: &AgentToolRuntime,
    shared: &EngineShared,
    journal_path: &Path,
    actor: String,
    question: String,
) -> String {
    if question.is_empty() {
        return json!({"ok": false, "error": "ask 的问题为空"}).to_string();
    }
    let input_hash = fnv1a64(&format!("{actor}\u{0}{question}"));
    {
        let mut engine = shared.engine.lock().unwrap_or_else(|poison| poison.into_inner());
        engine.asks_total += 1;
        if let Some(replay_answer) = engine
            .replay
            .get_mut(&(actor.clone(), input_hash))
            .and_then(VecDeque::pop_front)
        {
            engine.asks_replayed += 1;
            engine.seq += 1;
            append_workflow_event(
                journal_path,
                &json!({
                    "event": "ask_replayed",
                    "seq": engine.seq,
                    "actor": actor,
                    "input_hash": format!("{input_hash:016x}"),
                }),
            );
            // Layer 3 set-order：order = 原完成序。JS 侧顺序门按它延迟 resolve，
            // 使 Promise.race/交错 await 的解析顺序与原完成顺序一致。
            return json!({
                "ok": true,
                "answer": replay_answer.answer,
                "replayed": true,
                "order": replay_answer.order,
            })
            .to_string();
        }
        // set-order：journal 记录【发起序】（seq 单调），answer 按【完成序】追加——
        // 重放按 (actor, input_hash) FIFO 结算，结果确定且与完成顺序无关
        //（沙箱无世界交互，完成顺序对脚本不可观测）。
        engine.seq += 1;
        append_workflow_event(
            journal_path,
            &json!({
                "event": "ask",
                "seq": engine.seq,
                "actor": actor,
                "question": truncate_chars(&question, MAX_QUESTION_CHARS),
                "input_hash": format!("{input_hash:016x}"),
            }),
        );
    }
    let _permit = shared
        .permits
        .acquire()
        .await
        .expect("workflow 信号量不会关闭");
    eprintln!("[A] ask_host running: {actor}");
    let answer = run_workflow_ask(runtime, &actor, &question).await;
    eprintln!("[B] ask_host done: {actor}");
    let mut engine = shared.engine.lock().unwrap_or_else(|poison| poison.into_inner());
    match answer {
        Ok(answer) => {
            engine.seq += 1;
            append_workflow_event(
                journal_path,
                &json!({
                    "event": "answer",
                    "seq": engine.seq,
                    "input_hash": format!("{input_hash:016x}"),
                    "answer": truncate_chars(&answer, MAX_ANSWER_CHARS),
                }),
            );
            let order = engine.seq;
            engine
                .replay
                .entry((actor, input_hash))
                .or_default()
                .push_back(ReplayAnswer {
                    // 本轮内同 hash 重复 ask 的完成序 = 当前完成计数（engine.seq）。
                    answer: answer.clone(),
                    order,
                });
            json!({"ok": true, "answer": answer, "replayed": false}).to_string()
        }
        Err(error) => json!({
            "ok": false,
            "error": truncate_chars(&error, 500),
        })
        .to_string(),
    }
}

/// ask 的子代理执行（复用 PlanRun 步骤同款 run_task 门禁）。
async fn run_workflow_ask(
    runtime: &AgentToolRuntime,
    actor: &str,
    question: &str,
) -> Result<String, String> {
    let run_id = current_workflow_run().unwrap_or_default();
    let spec = AgentTaskSpec {
        name: sanitize_agent_name(&format!("{run_id}-{actor}")),
        role: String::new(),
        task: question.to_string(),
        context: format!("deterministic workflow '{run_id}'"),
        team_name: None,
        subagent_type: None,
        split_reason: Some("independent_evidence".to_string()),
        source_channel: String::new(),
        origin_chat_id: String::new(),
        model: None,
        plan_id: None,
        step_id: None,
        isolation: if looks_like_complex_coding_task(question) {
            "candidate_patch".to_string()
        } else {
            "shared_cas".to_string()
        },
        max_tool_iter: WORKFLOW_ASK_MAX_TOOL_ITER,
        max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
    };
    let result = runtime.run_task(spec, None, None).await;
    if result.is_error || result.cancelled {
        return Err(truncate_chars(&result.content, 500));
    }
    let summary = result.handoff.summary;
    Ok(if summary.trim().is_empty() {
        truncate_chars(&result.content, MAX_ANSWER_CHARS)
    } else {
        truncate_chars(&summary, MAX_ANSWER_CHARS)
    })
}

/// askJson 的宿主回调：答案 JSON 解析 + schema（子集）校验后返回对象。
/// 校验失败的答案按错误处理（重新 ask 或改写问题由脚本决定），不静默放行。
#[allow(clippy::too_many_arguments)]
async fn ask_json_host(
    runtime: &AgentToolRuntime,
    shared: &EngineShared,
    journal_path: &Path,
    actor: String,
    question: String,
    schema: Value,
) -> String {
    let raw = ask_host(runtime, shared, journal_path, actor.clone(), question).await;
    let envelope: Value = match serde_json::from_str(&raw) {
        Ok(envelope) => envelope,
        Err(_) => return raw,
    };
    if envelope.get("ok").and_then(Value::as_bool) != Some(true) {
        return raw;
    }
    let answer = envelope
        .get("answer")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let parsed: Value = match serde_json::from_str(&answer) {
        Ok(parsed) => parsed,
        Err(error) => {
            return json!({
                "ok": false,
                "error": format!("askJson 答案不是合法 JSON: {error}；请重新 ask 并要求子代理只输出 JSON")
            })
            .to_string()
        }
    };
    if let Err(error) = validate_json_schema(&parsed, &schema, 0) {
        return json!({
            "ok": false,
            "error": format!("askJson 答案未通过 schema 校验: {error}")
        })
        .to_string();
    }
    json!({"ok": true, "answer": parsed, "replayed": envelope["replayed"]}).to_string()
}

tokio::task_local! {
    static CURRENT_WORKFLOW_RUN: Option<String>;
}

/// 当前 workflow run_id（ask 子代理命名/上下文用）。
fn current_workflow_run() -> Option<String> {
    CURRENT_WORKFLOW_RUN
        .try_with(std::clone::Clone::clone)
        .ok()
        .flatten()
}

/// 引擎共享态：AsyncRuntime 的宿主回调（并行 promise）与收尾读取共用。
struct EngineShared {
    engine: StdMutex<EngineState>,
    permits: Arc<tokio::sync::Semaphore>,
}

#[derive(Default)]
struct EngineState {
    seq: u64,
    asks_total: u64,
    asks_replayed: u64,
    replay: ReplayTable,
    result: Option<String>,
    script_done: Option<String>,
}

// ---------------------------------------------------------------------------
// WorkflowRun 工具
// ---------------------------------------------------------------------------

pub struct WorkflowRunTool {
    runtime: AgentToolRuntime,
}

impl WorkflowRunTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        WorkflowRunTool { runtime }
    }
}

#[async_trait]
impl Tool for WorkflowRunTool {
    fn name(&self) -> &str {
        "WorkflowRun"
    }

    fn description(&self) -> &str {
        "Run a DETERMINISTIC JS script that orchestrates sub-agents (Layer 3 engine). \
         Sandbox: QuickJS with Date.now/Math.random pinned and Date/Math frozen, no \
         module loading. Facade: agent(name).ask(question) returns a PROMISE (use await \
         or Promise.all for parallel fan-out; up to max_parallel_asks sub-agents run \
         concurrently); result(text) records the final output. Every ask runs a bounded \
         sub-agent and is journaled with an FNV-1a inputHash. Resume: pass resume_run_id \
         — answered asks replay from agent/workflows/<run_id>.jsonl at zero provider \
         cost; the script must be byte-identical (script hash checked). AMEND: pass          amend_run_id with a REVISED script — completed asks import by actor name +          question hash (zero cost), changed/new questions run fresh."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Short run name for logs (default 'workflow')."},
                "script": {"type": "string", "description": "Deterministic JS orchestration script (<=100k chars), wrapped in an async IIFE (top-level await and Promise.all allowed). Globals: agent(name).ask(question) (Promise), result(text). No Date.now/Math.random/import (pinned/blocked)."},
                "resume_run_id": {"type": "string", "description": "Resume a previous run: answered asks are replayed from the journal at zero cost; the script must be identical (hash checked)."},
                "amend_run_id": {"type": "string", "description": "AMEND a previous run with a REVISED script: completed asks are imported from the predecessor journal by actor name + question hash (identical question text = zero-cost answer; changed/new questions run fresh). Mutually exclusive with resume_run_id; creates a new run id with lineage recorded."},
                "max_parallel_asks": {"type": "integer", "minimum": 1, "maximum": 8, "description": "Concurrency cap for parallel asks (default 4)."},
                "type_check": {"type": "boolean", "description": "Run tsc type-level check against facade.d.ts when the host has node+tsc (auto-downgrades to syntax+lint when unavailable). Default true."}
            },
            "required": ["script"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(script) = get_str(&input, "script")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            return ToolResult::err("script is required for WorkflowRun".to_string());
        };
        if script.chars().count() > MAX_SCRIPT_CHARS {
            return ToolResult::err(format!("script 超过 {MAX_SCRIPT_CHARS} 字符"));
        }
        // Layer 3 类型/facade 检查：带诊断的脚本拒绝提交，诊断只回给模型不落用户。
        // tsc 类型级检查耗时（秒级）且为阻塞调用：放 blocking 线程。
        let type_check = get_bool(&input, "type_check") || true; // 默认启用（不可用自动降级）
        let type_check_script = script.clone();
        let diagnostics = tokio::task::spawn_blocking(move || {
            check_script(&type_check_script, type_check)
        })
        .await
        .unwrap_or_default();
        if !diagnostics.is_empty() {
            return ToolResult::err(format_diagnostics(&diagnostics));
        }
        let script_hash = fnv1a64(&script);
        let run_name = sanitize_agent_name(
            get_str(&input, "name")
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("workflow"),
        );
        let max_parallel_asks = input
            .get("max_parallel_asks")
            .and_then(Value::as_u64)
            .map(|value| value.clamp(1, MAX_MAX_PARALLEL_ASKS as u64) as usize)
            .unwrap_or(DEFAULT_MAX_PARALLEL_ASKS as usize);
        let resume_request = get_str(&input, "resume_run_id")
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let amend_request = get_str(&input, "amend_run_id")
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if resume_request.is_some() && amend_request.is_some() {
            return ToolResult::err("resume_run_id 与 amend_run_id 互斥：前者续跑同一 run（脚本须逐字节一致），后者按 actor 导入前驱已完成 ask 的修订热修（脚本可改）。".to_string());
        }
        let run_id = match (resume_request, amend_request) {
            (Some(raw_id), _) => sanitize_agent_name(raw_id),
            (None, Some(_)) => next_execution_id(&format!("{run_name}-amend")),
            (None, None) => next_execution_id(&run_name),
        };
        let journal_path = workflow_journal_path(&self.runtime.layout, &run_id);
        // Layer 3 静态洞察（文本级近似）：actor/ask 数/并行，入 run_started 与报告。
        let insight = analyze_workflow_script(&script);
        let mut replay: ReplayTable = HashMap::new();
        let mut amended_from: Option<String> = None;
        let mut imported_asks = 0usize;
        if let Some(raw_id) = resume_request {
            let run_id = sanitize_agent_name(raw_id);
            match load_workflow_journal(&journal_path) {
                Ok((journal_script_hash, table)) => {
                    if journal_script_hash != script_hash {
                        return ToolResult::err(format!(
                            "脚本与 run '{run_id}' 的 journal 不一致（script hash {script_hash:016x} ≠ {journal_script_hash:016x}）。\
                             续跑（resume_run_id）要求脚本逐字节一致；如需修改编排，请改用 amend_run_id 热修：\
                             按 actor 名导入前驱已完成 ask，只重跑未完成的部分。"
                        ));
                    }
                    replay = table;
                }
                Err(error) => return ToolResult::err(error),
            }
            append_workflow_event(
                &journal_path,
                &json!({"event": "run_resumed", "script_hash": format!("{script_hash:016x}")}),
            );
        } else if let Some(raw_id) = amend_request {
            // Layer 3 Amend 热修（ZCode「按子代理名字导入前驱已完成工作」同源）：
            // 修订脚本按 actor 名 + inputHash 匹配前驱 journal——问答文本未变的 ask
            // 零调用继承答案；改写/新增的问题照常执行；脚本可以任意修改。
            let predecessor = sanitize_agent_name(raw_id);
            let predecessor_path = workflow_journal_path(&self.runtime.layout, &predecessor);
            match load_workflow_journal(&predecessor_path) {
                Ok((_predecessor_hash, table)) => {
                    imported_asks = table.len();
                    replay = table;
                    amended_from = Some(predecessor.clone());
                }
                Err(error) => {
                    return ToolResult::err(format!("amend 前驱加载失败: {error}"));
                }
            }
            append_workflow_event(
                &journal_path,
                &json!({
                    "event": "run_started",
                    "script_hash": format!("{script_hash:016x}"),
                    "amended_from": predecessor,
                    "imported_asks": imported_asks,
                }),
            );
        } else {
            append_workflow_event(
                &journal_path,
                &json!({
                    "event": "run_started",
                    "script_hash": format!("{script_hash:016x}"),
                    "insight": insight,
                }),
            );
        }

        let shared = Arc::new(EngineShared {
            engine: StdMutex::new(EngineState {
                seq: 0,
                asks_total: 0,
                asks_replayed: 0,
                replay,
                result: None,
                script_done: None,
            }),
            permits: Arc::new(tokio::sync::Semaphore::new(max_parallel_asks)),
        });
        let engine_journal = journal_path.clone();
        let engine_runtime = self.runtime.clone();
        // rquickjs 的 async future 非 Send（持裸 JS 指针）：整段引擎放进
        // spawn_blocking + 内嵌 current_thread runtime 驱动——future 不跨线程，
        // JoinHandle（快照结果）是 Send。ask 的 run_task 由内嵌 runtime 驱动
        //（provider HTTP / 信号量全部内闭环）。
        let executed = tokio::task::spawn_blocking(move || -> Result<_, String> {
            let inner = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("workflow 内嵌 runtime 创建失败: {error}"))?;
            inner.block_on(async move {
                let inner_shared = Arc::clone(&shared);
                let inner_runtime = engine_runtime.clone();
                let inner_journal = engine_journal.clone();
                let inner_shared_for_hosts = Arc::clone(&inner_shared);
                let inner_runtime_for_hosts = inner_runtime.clone();
                let inner_journal_for_hosts = inner_journal.clone();
                let outcome_result = eval_deterministic(script, move |ctx| {
                    let ask_state = Arc::clone(&inner_shared_for_hosts);
                    let ask_runtime = inner_runtime_for_hosts.clone();
                    let ask_journal = inner_journal_for_hosts.clone();
                    let ask_fn = Function::new(
                        ctx.clone(),
                        AsyncFn(move |actor: String, question: String| {
                            let ask_runtime = ask_runtime.clone();
                            let ask_state = ask_state.clone();
                            let ask_journal = ask_journal.clone();
                            async move {
                                ask_host(&ask_runtime, &ask_state, &ask_journal, actor, question)
                                    .await
                            }
                        }),
                    )
                    .expect("注册 __ask_raw");
                    ctx.globals().set("__ask_raw", ask_fn).expect("set __ask_raw");
                    let result_state = Arc::clone(&inner_shared_for_hosts);
                    let result_fn = Function::new(ctx.clone(), move |text: String| -> String {
                        let mut engine = result_state
                            .engine
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        engine.result = Some(truncate_chars(&text, 8_000));
                        "ok".to_string()
                    })
                    .expect("注册 __result");
                    ctx.globals().set("__result", result_fn).expect("set __result");
                    let done_state = Arc::clone(&inner_shared_for_hosts);
                    let done_fn = Function::new(ctx.clone(), move |text: String| -> String {
                        let mut engine = done_state
                            .engine
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        engine.script_done = Some(text);
                        "ok".to_string()
                    })
                    .expect("注册 __script_done");
                    ctx.globals()
                        .set("__script_done", done_fn)
                        .expect("set __script_done");
                    let json_state = Arc::clone(&inner_shared);
                    let json_runtime = inner_runtime.clone();
                    let json_journal = inner_journal.clone();
                    let json_fn = Function::new(
                        ctx.clone(),
                        AsyncFn(move |actor: String, question: String, schema: String| {
                            let json_runtime = json_runtime.clone();
                            let json_state = json_state.clone();
                            let json_journal = json_journal.clone();
                            async move {
                                let schema: Value =
                                    serde_json::from_str(&schema).unwrap_or(Value::Null);
                                ask_json_host(
                                    &json_runtime,
                                    &json_state,
                                    &json_journal,
                                    actor,
                                    question,
                                    schema,
                                )
                                .await
                            }
                        }),
                    )
                    .expect("注册 __ask_json_raw");
                    ctx.globals()
                        .set("__ask_json_raw", json_fn)
                        .expect("set __ask_json_raw");
                })
                .await;
                let snapshot = {
                    let engine = shared.engine.lock().unwrap_or_else(|p| p.into_inner());
                    (
                        engine.asks_total,
                        engine.asks_replayed,
                        engine.result.clone(),
                        engine.script_done.clone(),
                    )
                };
                Ok((outcome_result, snapshot))
            })
        })
        .await;
        let (outcome, snapshot) = match executed {
            Ok(Ok(pair)) => pair,
            Ok(Err(error)) => {
                append_workflow_event(
                    &journal_path,
                    &json!({"event": "run_failed", "error": truncate_chars(&error, 500)}),
                );
                return ToolResult::err(format!(
                    "{error}
（journal 已保留已完成问答：{}；同一脚本配 resume_run_id 重提可续跑）",
                    journal_path.display()
                ));
            }
            Err(error) => {
                return ToolResult::err(format!("workflow 引擎线程失败: {error}"));
            }
        };
        let (asks_total, asks_replayed, final_result, script_done) = snapshot;

        if let Err(error) = outcome {
            append_workflow_event(
                &journal_path,
                &json!({"event": "run_failed", "error": truncate_chars(&error, 500)}),
            );
            return ToolResult::err(format!(
                "{error}\n（journal 已保留已完成问答：{journal}；同一脚本配 resume_run_id 重提可续跑）",
                journal = journal_path.display()
            ));
        }
        if let Some(done) = script_done.filter(|done| done.starts_with("ERROR:")) {
            append_workflow_event(
                &journal_path,
                &json!({"event": "run_failed", "error": truncate_chars(&done, 500)}),
            );
            return ToolResult::err(format!(
                "{done}\n（journal 已保留已完成问答：{journal}；同一脚本配 resume_run_id 重提可续跑）",
                journal = journal_path.display()
            ));
        }
        append_workflow_event(
            &journal_path,
            &json!({"event": "run_completed", "asks_total": asks_total, "asks_replayed": asks_replayed}),
        );
        let mut report = json!({
            "status": "completed",
            "run_id": run_id,
            "script_hash": format!("{script_hash:016x}"),
            "asks_total": asks_total,
            "asks_replayed": asks_replayed,
            "result": final_result,
            "journal": journal_path.display().to_string(),
            "note": "断点续跑：把同一脚本配 resume_run_id 重提，已回答的 ask 零调用重放；                     修订编排请配 amend_run_id（按 actor 导入已完成 ask，只重跑未完成部分）。"
        });
        if let Some(predecessor) = &amended_from {
            report["amended_from"] = json!(predecessor);
            report["imported_asks"] = json!(imported_asks);
        }
        report["insight"] = json!({
            "actors": insight.actors,
            "ask_count": insight.ask_count,
            "parallel": insight.parallel,
            "dependency_graph": {
                "nodes": insight.dependency_graph.nodes,
                "edges": insight.dependency_graph.edges,
                "waves": insight.dependency_graph.waves,
            },
        });
        ToolResult::ok(report.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_matches_known_vectors() {
        assert_eq!(fnv1a64(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64("foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn dependency_graph_sequential_chain_and_parallel_wave() {
        // 串行链：a → b → c 三层波。
        let sequential = concat!(
            "const coder = agent(\"coder\");
",
            "const r1 = await coder.ask(\"设计\");
",
            "const reviewer = agent(\"reviewer\");
",
            "const r2 = await reviewer.ask(\"评审 r1\");
",
            "const r3 = await coder.ask(\"按评审修复\");
"
        );
        let graph = analyze_dependency_graph(sequential);
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.waves.len(), 3, "串行链应为三层波: {:?}", graph.waves);
        assert_eq!(graph.edges.len(), 2);
        // 并行波：两个 ask 同一行（Promise.all）→ 同波、无边。
        let parallel = concat!(
            "const coder = agent(\"coder\");
",
            "const reviewer = agent(\"reviewer\");
",
            "const [a, b] = await Promise.all([coder.ask(\"x\"), reviewer.ask(\"y\")]);
"
        );
        let graph = analyze_dependency_graph(parallel);
        assert_eq!(graph.nodes.len(), 2);
        assert!(graph.nodes.iter().all(|n| n.parallel), "波内节点应标记并行");
        assert_eq!(graph.waves.len(), 1, "同波节点应同一层: {:?}", graph.waves);
        assert!(graph.edges.is_empty(), "波内不应有顺序边");
    }

    #[test]
    fn dependency_graph_mixed_wave_then_chain() {
        // 并行波之后接串行 ask：波整体 → 后续节点有边。
        let script = concat!(
            "const coder = agent(\"coder\");
",
            "const reviewer = agent(\"reviewer\");
",
            "const [a, b] = await Promise.all([coder.ask(\"x\"), reviewer.ask(\"y\")]);
",
            "const fixer = agent(\"fixer\");
",
            "const r3 = await fixer.ask(\"合并修复\");
"
        );
        let graph = analyze_dependency_graph(script);
        assert_eq!(graph.nodes.len(), 3);
        // 波 → fixer 的边。
        assert!(
            graph.edges.iter().any(|(from, to)| from == "ask-3" && to == "ask-5"),
            "并行波末节点 → 后续 ask 应有边: {:?}",
            graph.edges
        );
        // 波次：x/y 第一波，合并修复第二波。
        assert_eq!(graph.waves.len(), 2);
    }

    #[tokio::test]
    async fn sandbox_pins_nondeterminism() {
        // 钉死值经 __result 回传验证（async IIFE + Promise 全链路）。
        let received: Arc<StdMutex<String>> = Arc::new(StdMutex::new(String::new()));
        let sink = Arc::clone(&received);
        let outcome = eval_deterministic(
            "var v = JSON.stringify([Date.now(), Math.random()]); __result(v);".to_string(),
            move |ctx| {
                let sink = Arc::clone(&sink);
                let result_fn = Function::new(ctx.clone(), move |text: String| -> String {
                    *sink.lock().unwrap_or_else(|p| p.into_inner()) = text;
                    "ok".to_string()
                })
                .expect("注册 __result");
                ctx.globals().set("__result", result_fn).expect("set __result");
            },
        )
        .await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(
            *received.lock().unwrap_or_else(|p| p.into_inner()),
            "[0,0.5]",
            "确定性钉死值必须生效"
        );
    }
}

#[cfg(test)]
mod check_tests {
    use super::*;

    fn diagnose(script: &str) -> Vec<FacadeDiagnostic> {
        // 语法+lint 层（type_check=false：不依赖宿主 tsc，测试确定性）。
        check_script(script, false)
    }

    #[tokio::test]
    async fn clean_async_script_passes_check() {
        let script = concat!(
            "const coder = agent(\"coder\");\n",
            "const [a, b] = await Promise.all([coder.ask(\"q1\"), coder.ask(\"q2\")]);\n",
            "result(a + b);\n"
        );
        assert!(
            diagnose(script).is_empty(),
            "干净脚本不应有诊断: {:?}",
            diagnose(script)
        );
    }

    #[test]
    fn syntax_error_is_rejected_with_diagnostic() {
        let diagnostics = diagnose("const x = {;");
        assert!(!diagnostics.is_empty(), "语法错误应有诊断");
        assert!(diagnostics[0].message.contains("语法错误"));
    }

    #[test]
    fn banned_patterns_reported_with_line_and_col() {
        let script = "const t = Date.now();\nconst r = eval('1');";
        let diagnostics = diagnose(script);
        assert!(diagnostics.iter().any(|d| d.message.contains("Date.now")), "{diagnostics:?}");
        let eval_diag = diagnostics
            .iter()
            .find(|d| d.message.contains("eval"))
            .expect("eval 应有诊断");
        assert_eq!(eval_diag.line, 2, "eval 在第 2 行");
        assert!(eval_diag.col > 1);
    }

    #[test]
    fn dynamic_import_is_rejected_statically() {
        let diagnostics = diagnose("var m = import('fs');");
        assert!(diagnostics.iter().any(|d| d.message.contains("import")));
    }
}

#[cfg(test)]
mod schema_and_insight_tests {
    use super::*;

    #[test]
    fn schema_validator_enforces_types_and_required() {
        let schema = json!({
            "type": "object",
            "required": ["verdict"],
            "properties": {
                "verdict": {"type": "boolean"},
                "score": {"type": "number"},
                "notes": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}}
            }
        });
        let good = json!({"verdict": true, "score": 3.5, "tags": ["a"], "extra": 1});
        assert!(validate_json_schema(&good, &schema, 0).is_ok());
        let bad_type = json!({"verdict": "yes"});
        assert!(validate_json_schema(&bad_type, &schema, 0).is_err());
        let missing = json!({"score": 1});
        assert!(validate_json_schema(&missing, &schema, 0).is_err());
        let bad_item = json!({"verdict": true, "tags": [1, 2]});
        assert!(validate_json_schema(&bad_item, &schema, 0).is_err());
    }

    #[test]
    fn insight_extracts_actors_and_parallel() {
        let script = concat!(
            "const coder = agent(\"coder\");\n",
            "const reviewer = agent(\"reviewer\");\n",
            "const [a, b] = await Promise.all([coder.ask(\"x\"), reviewer.ask(\"y\")]);\n",
            "result(a + b);\n"
        );
        let insight = analyze_workflow_script(script);
        assert_eq!(insight.actors, vec!["coder".to_string(), "reviewer".to_string()]);
        assert_eq!(insight.ask_count, 2);
        assert!(insight.parallel);
    }

}
