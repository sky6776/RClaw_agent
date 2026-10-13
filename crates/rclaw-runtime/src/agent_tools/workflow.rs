//! `WorkflowRun`：Layer 3 动态工作流执行器（ZCode dynamic-workflow 同源移植）。
//!
//! 主代理提交一段 **确定性 JS 脚本**，引擎在嵌入的 QuickJS-ng 沙箱里执行：脚本被包进
//! async IIFE（顶层 await / `Promise.all` 可用），经 `agent(name).ask(question)` /
//! `askJson(question, schema)` facade 驱动子代理——每次 ask 启动一个**可取消**的子代理
//! （[`AgentToolRuntime::launch_background_task_impl`]，并发由信号量 `max_parallel_asks`
//! 限流），问答写入 `agent/state.sqlite` journal（`workflow/<run_id>` 事件流；FNV-1a
//! **inputHash** + 发起序 / 完成序）。
//!
//! 确定性骨架：
//! - 沙箱 prelude 钉死 `Date`（零参构造 / 函数式调用 = 固定纪元）、`Math.random`、
//!   `performance` 并冻结，拆除 `eval` / `Function` 构造；不注册模块加载器，动态 `import()`
//!   直接失败；单片 JS CPU 预算（宿主回调结算后重置）+ 整体墙钟预算（`max_run_secs`）；
//! - **断点重放 = 重跑脚本**（`resume_run_id`，脚本 FNV hash 须一致）：按
//!   (actor, inputHash, 第几次) 精确配对 journal 已答 ask（provider 零调用），并经 JS 顺序门
//!   按原完成序 resolve（set-order：Promise.race 保真）；引擎静止而门里仍有等待者时放行
//!   （release-on-settle，不会挂死）；
//! - **amend 热修**（`amend_run_id`，脚本可改）：按同一键导入前驱已完成 ask（不经顺序门），
//!   改写 / 新增的问题照常执行。
//!
//! 提交期 facade 检查：QuickJS 语法探针 + 禁用模式扫描（只扫代码，跳过字符串与注释）+
//! 可选 tsc 类型级检查（与引擎同一包装；宿主无 node/tsc 时静默降级）。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use rquickjs::{AsyncContext, AsyncRuntime, Context, Function, Runtime};
use rquickjs::prelude::Async as AsyncFn;
use serde_json::{json, Value};

use super::subagent::get_str;
use super::*;

/// 引擎沙箱的内存上限（QuickJS 级，防脚本失控）。
const ENGINE_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// 单片 JS CPU 预算：两次宿主回调之间连续执行 JS 的上限（中断处理器轮询；ask/askJson
/// 结算、顺序门放行时重置）。整个 run 的时长另由墙钟预算 `max_run_secs` 约束。
const ENGINE_SLICE_BUDGET: Duration = Duration::from_secs(120);
/// 整体墙钟预算：默认 1h，入参 `max_run_secs` 可调（硬顶 24h），超时按 run_failed 收尾。
const DEFAULT_MAX_RUN_SECS: u64 = 60 * 60;
const MAX_MAX_RUN_SECS: u64 = 24 * 60 * 60;
/// 取消 / 超时后给子代理优雅收尾（经 cancel token 退出）的宽限期，到期放弃引擎。
const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// 并行 ask 上限（`Promise.all` 扇出的信号量许可数；default 8，硬顶 16——第十二轮
/// 4/8 → 8/16：与 PlanRun 波次上限同一轮提升，大规模并行的直接闸门；ask 的子代理
/// 模型请求仍由 `subagent_request_stagger` 错峰）。
const DEFAULT_MAX_PARALLEL_ASKS: u32 = 8;
const MAX_MAX_PARALLEL_ASKS: u32 = 16;
/// 单次 ask 的子代理工具迭代上限（对齐计划步骤的收敛口径）。
const WORKFLOW_ASK_MAX_TOOL_ITER: usize = 64;
const MAX_SCRIPT_CHARS: usize = 100_000;
const MAX_QUESTION_CHARS: usize = 4_000;
const MAX_ANSWER_CHARS: usize = 4_000;

/// facade 声明（.d.ts 语义的文本形态）：脚本契约一览，随诊断一并回给模型。
pub(crate) const FACADE_DTS: &str = r#"
// WorkflowRun 脚本可用的全局 facade（确定性沙箱，无模块加载）。
// 可用：await / Promise.all / while / try-catch（脚本被包进 async IIFE）。
// 禁用：Date() 取当前时间 / Math.random 赋值 / import / eval / Function 构造 / require /
//       setTimeout / setInterval / process —— 提交前静态检查（只扫代码，不扫字符串与注释）直接拒绝。
declare function agent(name: string): {
  /** 向子代理提问并等待回答（promise）。问题文本即输入指纹（inputHash）。 */
  ask(question: string): Promise<string>;
  /**
   * 类型化 ask：内联类型注解编译成 JSON Schema 校验答案（答案按 JSON 解析后返回对象）。
   * 支持 string / number / boolean / null / 字面量 / 联合 / 数组（T[] 或 Array<T>）/
   * 对象字面量 / 顶层 type 别名（无泛型）；any 不允许（用具体类型或 unknown）。
   */
  ask<T = string>(question: string): Promise<T>;
  /** 同 ask<T>，但 schema 以 JSON Schema 子集作为第二参数显式传入。 */
  askJson<T = any>(question: string, schema: object): Promise<T>;
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

/// 禁用模式：静态拒绝破坏确定性与动态求值的写法（沙箱运行时钉死才是真正的执行面，这里是
/// 提交期的二道防线）。只在代码上匹配（字符串/注释已遮蔽），且要求模式前（以及以标识符
/// 结尾的模式后）不是标识符字符——`formatDate(` / `myeval(` 不会误报。
const BANNED_PATTERNS: &[(&str, &str)] = &[
    ("Date.now", "时间必须确定：脚本内禁用 Date.now（沙箱已钉死为固定纪元）"),
    ("Date.prototype", "禁用 Date.prototype（沙箱已钉死时间源）"),
    ("Date(", "时间必须确定：脚本内禁用 Date() / new Date()（沙箱已钉死为固定纪元）"),
    ("Math.random =", "禁用改写 Math.random（沙箱已钉死并冻结）"),
    ("eval", "禁用 eval（动态求值，沙箱已拆除）"),
    ("Function(", "禁用 Function 构造（动态求值，沙箱已拆除）"),
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

    // 2) 禁用模式扫描：只扫代码（字符串/注释遮蔽后字节位置不变，行/列按原脚本报告）。
    let code = mask_strings_and_comments(script);
    let code_bytes = code.as_bytes();
    for (pattern, reason) in BANNED_PATTERNS {
        let mut search_from = 0usize;
        while let Some(found) = code[search_from..].find(pattern) {
            let absolute = search_from + found;
            let after = absolute + pattern.len();
            search_from = after;
            // 标识符边界：模式前不能是标识符字符；以标识符结尾的模式（eval）后也不能是。
            let bounded_before = absolute == 0 || !is_identifier_byte(code_bytes[absolute - 1]);
            let ends_with_identifier = pattern
                .as_bytes()
                .last()
                .is_some_and(|byte| is_identifier_byte(*byte));
            let bounded_after = !ends_with_identifier
                || after >= code_bytes.len()
                || !is_identifier_byte(code_bytes[after]);
            if !bounded_before || !bounded_after {
                continue;
            }
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

/// JS 标识符字符（ASCII 字母数字 / `_` / `$`；非 ASCII 一律按标识符处理，宁可漏报不误报）。
fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

/// 把字符串字面量（单/双引号、模板串，含转义）与注释的内容替换成空格——字节长度与换行位置
/// 不变，禁用模式只在代码上匹配，行/列仍对应原脚本。模板串的 `${}` 一并视作字符串（经它
/// 拼出的动态求值由沙箱拆除的 eval / Function 兜底）。
pub(crate) fn mask_strings_and_comments(script: &str) -> String {
    let bytes = script.as_bytes();
    let mut masked = bytes.to_vec();
    fn blank(masked: &mut [u8], from: usize, to: usize) {
        for byte in &mut masked[from..to] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let end = bytes[i..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(bytes.len(), |offset| i + offset);
                blank(&mut masked, i, end);
                i = end;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let end = script[i + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |offset| i + 2 + offset + 2);
                blank(&mut masked, i, end);
                i = end;
            }
            quote @ (b'\'' | b'"' | b'`') => {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] != quote {
                    j += if bytes[j] == b'\\' { 2 } else { 1 };
                }
                let close = j.min(bytes.len());
                blank(&mut masked, i + 1, close);
                i = close + 1;
            }
            _ => i += 1,
        }
    }
    String::from_utf8(masked).unwrap_or_else(|_| script.to_string())
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

/// 在线程里持续读空一条子进程管道（子进程输出超过管道缓冲也不会卡死）。
fn drain_pipe<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    })
}

/// 带超时地运行外部命令；超时 kill 子进程并返回 None。
fn run_with_timeout(
    mut command: std::process::Command,
    timeout: Duration,
) -> Option<std::process::Output> {
    use std::process::Stdio;
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().ok()?;
    let stdout = drain_pipe(child.stdout.take());
    let stderr = drain_pipe(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    };
    Some(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// 探测宿主 `node` 可用性（`node --version`，2s 超时；结果进程级缓存）。
fn node_available() -> bool {
    static NODE: StdMutex<Option<Option<bool>>> = StdMutex::new(None);
    let mut cache = NODE.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(cached) = *cache {
        return cached.unwrap_or(false);
    }
    let mut command = std::process::Command::new("node");
    command.arg("--version");
    let available = run_with_timeout(command, Duration::from_secs(2))
        .map(|output| output.status.success())
        .unwrap_or(false);
    *cache = Some(Some(available));
    available
}

/// 解析 tsc 的 JS 入口（npm 全局安装：`<bin>/tsc(.cmd)` 旁的 node_modules/typescript/lib/tsc.js）。
/// 直接以 node 启动，超时 kill 的就是 tsc 进程本身（Windows 经 cmd.exe 垫片只会杀掉垫片）。
fn resolve_tsc_entry() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let entry = dir.join("node_modules/typescript/lib/tsc.js");
        let has_shim = dir.join("tsc.cmd").is_file() || dir.join("tsc").is_file();
        (has_shim && entry.is_file()).then_some(entry)
    })
}

/// `tsc <args>` 命令：node 入口可解析时直接 `node tsc.js`，否则按宿主习惯走 PATH。
fn tsc_command(args: &[&str]) -> std::process::Command {
    let mut command = match resolve_tsc_entry() {
        Some(entry) => {
            let mut command = std::process::Command::new("node");
            command.arg(entry);
            command
        }
        #[cfg(windows)]
        None => {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "tsc"]);
            command
        }
        #[cfg(not(windows))]
        None => std::process::Command::new("tsc"),
    };
    command.args(args);
    command
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
    let available = run_with_timeout(tsc_command(&["--version"]), Duration::from_secs(10))
        .map(|output| output.status.success())
        .unwrap_or(false);
    *cache = Some(Some(available));
    available
}

/// 解析 tsc 输出行（`--pretty false` 格式：`path(line,col): error TSxxxx: message`），
/// 只保留 script.ts 的条目（facade.d.ts 的报错属引擎自身问题，不归咎脚本）。
/// `line_offset` = 包装在脚本前占的行数，诊断行号回映射到用户脚本行。
fn parse_tsc_diagnostics(
    output: &str,
    script_file_name: &str,
    line_offset: usize,
) -> Vec<FacadeDiagnostic> {
    let mut diagnostics = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        // 先定位 "): error " / "): warning "（消息体本身可含括号，不能从行尾倒找）。
        let Some((marker_at, marker_len)) = ["): error ", "): warning "]
            .iter()
            .find_map(|marker| line.find(marker).map(|at| (at, marker.len())))
        else {
            continue;
        };
        let head = &line[..marker_at];
        let Some(open) = head.rfind('(') else {
            continue;
        };
        let file = &head[..open];
        let Some((line_no, col_no)) = head[open + 1..].split_once(',') else {
            continue;
        };
        let Ok(line_no) = line_no.parse::<usize>() else {
            continue;
        };
        let Ok(col_no) = col_no.parse::<usize>() else {
            continue;
        };
        // tail: "TS2304: Cannot find name 'x'."
        let tail = &line[marker_at + marker_len..];
        let file_stem = Path::new(file)
            .file_stem()
            .map(|stem| stem.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if file_stem != script_file_name {
            continue;
        }
        diagnostics.push(FacadeDiagnostic {
            line: line_no.saturating_sub(line_offset).max(1),
            col: col_no,
            message: format!("TS 检查：{tail}"),
        });
    }
    diagnostics
}

/// 临时目录守卫：任何提前返回 / 超时路径都会清理。
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
    std::fs::create_dir_all(&dir).ok()?;
    let _cleanup = TempDirGuard(dir.clone());
    let script_path = dir.join("script.ts");
    let facade_path = dir.join("facade.d.ts");
    // 与引擎执行的包装一致（async IIFE）：顶层 await 合法（TS1375 只针对模块顶层）、顶层
    // 声明是函数作用域（不与 lib 全局冲突）；包装占 1 行，诊断行号回映射到用户脚本行。
    std::fs::write(&script_path, format!("(async () => {{\n{script}\n}})();\n")).ok()?;
    std::fs::write(&facade_path, FACADE_DTS).ok()?;
    let mut command = tsc_command(&[
        "--noEmit",
        "--strict",
        "--target",
        "es2022",
        "--lib",
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
    let output = run_with_timeout(command, Duration::from_secs(30))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Some(parse_tsc_diagnostics(&stdout, "script", 1))
}

// ---------------------------------------------------------------------------
// askJson：泛型 T→JSON Schema 的答案校验（schema 子集）
// ---------------------------------------------------------------------------

/// 极简 JSON Schema 子集校验（递归）：type（string/number/integer/boolean/object/array/null，
/// 可为 `["T", "null"]` 数组）、const、enum、anyOf、object.properties/required、array.items。
/// 覆盖 facade 契约场景（含 `.ask<T>` 编译出的字面量 / 联合 schema，第十二轮）；
/// 超出子集的字段忽略；未知类型名与超深嵌套（> 8）按错误处理。
fn validate_json_schema(value: &Value, schema: &Value, depth: usize) -> Result<(), String> {
    if depth > 8 {
        return Err("schema 嵌套超过深度上限 8".to_string());
    }
    // anyOf：任一分支通过即可（先于 type 校验——联合的每个分支自带 type/const）。
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        let mut failures = Vec::new();
        for variant in variants {
            match validate_json_schema(value, variant, depth + 1) {
                Ok(()) => return Ok(()),
                Err(error) => failures.push(error),
            }
        }
        return Err(format!("不满足任一联合分支：{}", failures.join("; ")));
    }
    if let Some(expected_const) = schema.get("const") {
        if value != expected_const {
            return Err(format!("常量不符：期望 {expected_const}，实际 {value}"));
        }
    }
    if let Some(variants) = schema.get("enum").and_then(Value::as_array) {
        if !variants.contains(value) {
            return Err(format!(
                "枚举不符：期望 {} 之一，实际 {value}",
                serde_json::to_string(variants).unwrap_or_default()
            ));
        }
    }
    let expected: Vec<&str> = match schema.get("type") {
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !expected.is_empty() && !expected.contains(&"any") {
        let mut matched = false;
        for name in &expected {
            matched |= match *name {
                "string" => value.is_string(),
                "number" => value.is_number(),
                "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
                "boolean" => value.is_boolean(),
                "object" => value.is_object(),
                "array" => value.is_array(),
                "null" => value.is_null(),
                other => return Err(format!("schema 含未知类型名：{other}")),
            };
        }
        if !matched {
            return Err(format!("类型不符：期望 {}，实际 {value}", expected.join("|")));
        }
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
    if let Some(prefix_items) = schema.get("prefixItems").and_then(Value::as_array) {
        if let Some(array) = value.as_array() {
            // 定长元组：前缀按位校验；min/maxItems 约束长度。
            for (index, (item, subschema)) in
                array.iter().zip(prefix_items.iter()).enumerate()
            {
                validate_json_schema(item, subschema, depth + 1)
                    .map_err(|error| format!("[{index}]: {error}"))?;
            }
        }
    }
    if let Some(items) = schema.get("items") {
        if let Some(array) = value.as_array() {
            let prefix = schema
                .get("prefixItems")
                .and_then(Value::as_array)
                .map(|prefix| prefix.len())
                .unwrap_or(0);
            for (index, item) in array.iter().enumerate().skip(prefix) {
                validate_json_schema(item, items, depth + 1)
                    .map_err(|error| format!("[{index}]: {error}"))?;
            }
        }
    }
    if let Some(min_items) = schema.get("minItems").and_then(Value::as_u64) {
        if let Some(array) = value.as_array() {
            if array.len() < min_items as usize {
                return Err(format!("数组长度不足：期望 ≥ {min_items}，实际 {}", array.len()));
            }
        }
    }
    if let Some(max_items) = schema.get("maxItems").and_then(Value::as_u64) {
        if let Some(array) = value.as_array() {
            if array.len() > max_items as usize {
                return Err(format!("数组超长：期望 ≤ {max_items}，实际 {}", array.len()));
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

/// 静态洞察（第十二轮起 AST 优先，文本解析降级）：actor 清单 / ask 计数 / 并行标记 /
/// 依赖图。AST 版（[`super::workflow_ast`]）不误报字符串与注释里的 `.ask(`、能识别变量
/// 提升的 `Promise.all` 扇出与 inline `agent("x").ask()`；解析失败降级回文本扫描，
/// 分析永不阻塞提交。
pub(crate) fn analyze_workflow_script(script: &str) -> WorkflowInsight {
    // AST 提取失败（None）才走文本扫描：AST 成功而零 actor 是合法结果（文本扫描反而
    // 会把字符串里的 agent( 当真）。
    let actors: Vec<String> = match super::workflow_ast::extract_actors(script) {
        Some(actors) => actors,
        None => {
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
            actors
        }
    };
    let ask_count = script.matches(".ask(").count() + script.matches(".askJson(").count();
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
    /// 节点 id（`ask-<行号>`；同一行第 n 个（n ≥ 2）为 `ask-<行号>-<n>`）。
    pub(crate) id: String,
    /// 发起 ask 的 actor 名。
    pub(crate) actor: String,
    /// 所在行号（1 起）。
    pub(crate) line: usize,
    /// 是否位于 Promise.all（并行波内）。
    pub(crate) parallel: bool,
    /// 所属 Promise.all 波编号（0 = 不在波内；可跨行）。
    #[serde(skip)]
    pub(crate) wave: usize,
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

/// 依赖图静态分析（第十二轮起 AST 优先，文本解析降级）：
/// - AST 版（[`super::workflow_ast::build_dependency_graph`]）：ask 调用点按语法识别
///   （字符串 / 注释里的 `.ask(` 不误报）、receiver 经句柄绑定或 inline `agent()`
///   调用解析 actor、`Promise.all` 数组元素（直接 ask 或持有 ask promise 的标识符）
///   合并成并行波；
/// - oxc 解析失败（None）降级回第十一轮的语句级文本解析：不解析任意表达式形态，
///   产出仍是真实的节点/边/波次 DAG，供调度参照与可观测性。
pub(crate) fn analyze_dependency_graph(script: &str) -> DependencyGraph {
    if let Some(graph) = super::workflow_ast::build_dependency_graph(script) {
        return graph;
    }
    text_parse_dependency_graph(script)
}

/// 第十一轮的语句级文本解析（AST 解析失败时的降级路径）。
fn text_parse_dependency_graph(script: &str) -> DependencyGraph {
    let mut bindings: Vec<(String, String)> = Vec::new(); // (handle, actor)
    let mut nodes: Vec<DependencyNode> = Vec::new();
    let mut wave_id = 0usize; // 当前 Promise.all 波编号
    let mut wave_depth = 0i32; // Promise.all( 之后的括号深度（> 0 表示波未闭合，可跨行）

    for (idx, raw_line) in script.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        // 句柄绑定。
        if let Some(rest) = line.strip_prefix("const ") {
            if let Some((handle, actor)) = parse_agent_binding(rest) {
                bindings.push((handle, actor));
            }
        }
        // Promise.all 波：从 `Promise.all(` 起按括号深度跟踪到闭合，期间的 ask 标记并行。
        let mut in_wave = wave_depth > 0;
        let mut depth_scope = line;
        if !in_wave {
            if let Some(at) = line.find("Promise.all(") {
                wave_id += 1;
                in_wave = true;
                depth_scope = &line[at + "Promise.all".len()..];
            }
        }
        if in_wave {
            wave_depth += depth_scope.matches('(').count() as i32
                - depth_scope.matches(')').count() as i32;
            wave_depth = wave_depth.max(0);
        }
        for mut node in asks_on_line(line, &bindings, line_no) {
            if in_wave {
                node.parallel = true;
                node.wave = wave_id;
            }
            nodes.push(node);
        }
    }

    // 边：相邻 ask/wave 之间按行序连边（前一波每个出口 → 后一波每个入口）。
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut last_boundary: Vec<String> = Vec::new(); // 上一个「波/节点」的全部出口
    let mut i = 0usize;
    while i < nodes.len() {
        let wave: Vec<usize> = if nodes[i].parallel {
            // 收集同一波的连续并行节点。
            let mut group = vec![i];
            let mut j = i + 1;
            while j < nodes.len() && nodes[j].parallel && nodes[j].wave == nodes[i].wave {
                group.push(j);
                j += 1;
            }
            group
        } else {
            vec![i]
        };
        for &member in &wave {
            for prev in &last_boundary {
                edges.push((prev.clone(), nodes[member].id.clone()));
            }
        }
        last_boundary = wave.iter().map(|&member| nodes[member].id.clone()).collect();
        i = wave.last().copied().unwrap_or(i) + 1;
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

/// 解析一行内的 ask 调用点（`<handle>.ask(...)` / `<handle>.askJson(...)`，可多个，
/// 逗号/括号分隔均可——只提取句柄与行号，不解析参数）。
fn asks_on_line(
    line: &str,
    bindings: &[(String, String)],
    line_no: usize,
) -> Vec<DependencyNode> {
    let mut nodes = Vec::new();
    let mut search_from = 0usize;
    while let Some(found) = line[search_from..].find(".ask") {
        let at = search_from + found;
        search_from = at + ".ask".len();
        let after = &line[search_from..];
        if !(after.starts_with('(') || after.starts_with("Json(")) {
            continue;
        }
        let prefix = &line[..at];
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
        // 同一行多个 ask：首个 `ask-<行>`，其后加行内序号，id 不冲突。
        let id = if nodes.is_empty() {
            format!("ask-{line_no}")
        } else {
            format!("ask-{line_no}-{}", nodes.len() + 1)
        };
        nodes.push(DependencyNode {
            id,
            actor,
            line: line_no,
            parallel: false,
            wave: 0,
        });
    }
    nodes
}

/// Kahn 分层：波次 = 拓扑层级（同层可并行）。
pub(crate) fn topo_waves(nodes: &[DependencyNode], edges: &[(String, String)]) -> Vec<Vec<String>> {
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
    while !frontier.is_empty() {
        frontier.sort();
        waves.push(frontier.clone());
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
// journal：run_started / ask / answer / ask_replayed / run_completed（inputHash = FNV-1a64）
// ---------------------------------------------------------------------------

/// 追加一条【非关键】journal 事件，写失败只告警（每条事件一行独立记录，并行 ask 的写入
/// 由事务串行化，不会交错）。`answer` / `ask_replayed` / `run_completed` 丢失的后果是下次
/// resume 多问一次——安全方向。`run_started` / `run_resumed` / `ask` 决定 resume 语义，
/// 直接用 [`EventLog::append`] 并处理错误。
fn append_workflow_event(journal: &EventLog, event: &Value) {
    journal.append_lossy(event);
}

/// 重放表条目：答案 + 原完成序（journal 中首见该调用点答案的全局位次，0 起；
/// amend 导入不经顺序门 → None）。
#[derive(Debug, Clone)]
struct ReplayAnswer {
    answer: String,
    order: Option<u64>,
}

/// 调用点键：(actor, inputHash, 本 run 内同键第几次提问)。
type ReplayKey = (String, u64, u32);

/// journal 里的问答重放表：调用点 → 答案。
type ReplayTable = HashMap<ReplayKey, ReplayAnswer>;

/// 只收每个调用点**首见**的答案（order 跨代稳定：重放一代按门序再发一遍，取首次）。
fn record_answer(table: &mut ReplayTable, order_next: &mut u64, key: ReplayKey, answer: &str) {
    if table.contains_key(&key) {
        return;
    }
    table.insert(
        key,
        ReplayAnswer {
            answer: answer.to_string(),
            order: Some(*order_next),
        },
    );
    *order_next += 1;
}

/// 从 journal 重建 scriptHash 与重放表。
///
/// 每一代（run_started / run_resumed 起始）内按发起序给同 (actor, inputHash) 的 ask 编号
/// （第几次），`answer` 经 `ask_seq`（= 发起事件在 journal 里的 seq，库内分配、跨进程唯一）
/// 指回发起事件精确配对；没有 `ask_seq` 或指不到本代未配对 ask 的 answer 跳过（宁可
/// 重问，不错配）。`ask_replayed` 自带答案（重放也是调用点）。字段残缺的事件跳过。
fn load_workflow_journal(journal: &EventLog) -> Result<(u64, ReplayTable), String> {
    let events = journal.load().map_err(|error| {
        format!("{error}。resume 需要已存在的 run_id（见先前 WorkflowRun 结果）。")
    })?;
    if events.is_empty() {
        return Err(format!(
            "workflow journal（{}）不存在。resume 需要已存在的 run_id（见先前 WorkflowRun 结果）。",
            journal.locator()
        ));
    }
    let mut script_hash: Option<u64> = None;
    let mut table = ReplayTable::new();
    let mut occurrences: HashMap<(String, u64), u32> = HashMap::new();
    // 本代已发起、尚未配对答案的 ask：(发起事件的 journal seq, 调用点)。
    let mut pending: Vec<(u64, ReplayKey)> = Vec::new();
    let mut order_next = 0u64;
    for (journal_seq, event) in events {
        let actor_and_hash = || -> Option<(String, u64)> {
            let actor = event.get("actor")?.as_str()?.to_string();
            let hash = u64::from_str_radix(event.get("input_hash")?.as_str()?, 16).ok()?;
            Some((actor, hash))
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
                // 新一代：调用点编号与未配对 ask 重置（上一代未答的 ask 本代会重新发起）。
                occurrences.clear();
                pending.clear();
            }
            Some(kind @ ("ask" | "ask_replayed")) => {
                let Some((actor, hash)) = actor_and_hash() else {
                    continue;
                };
                let index = occurrences.entry((actor.clone(), hash)).or_insert(0);
                let key = (actor, hash, *index);
                *index += 1;
                if kind == "ask" {
                    pending.push((journal_seq, key));
                } else if let Some(answer) = event.get("answer").and_then(Value::as_str) {
                    record_answer(&mut table, &mut order_next, key, answer);
                }
            }
            Some("answer") => {
                let Some(answer) = event.get("answer").and_then(Value::as_str) else {
                    continue;
                };
                // 精确配对：ask_seq 指回发起事件的 journal seq；指不到本代未配对的 ask
                // （残缺 / 跨代）就跳过，不按 hash 猜——同问两次完成顺序相反时会把答案换位。
                let Some(position) = event
                    .get("ask_seq")
                    .and_then(Value::as_u64)
                    .and_then(|ask_seq| pending.iter().position(|(seq, _)| *seq == ask_seq))
                else {
                    continue;
                };
                let (_, key) = pending.remove(position);
                record_answer(&mut table, &mut order_next, key, answer);
            }
            _ => {}
        }
    }
    let script_hash = script_hash.ok_or_else(|| {
        format!(
            "workflow journal（{}）里没有 run_started 记录，无法 resume",
            journal.locator()
        )
    })?;
    Ok((script_hash, table))
}

// ---------------------------------------------------------------------------
// 确定性沙箱（AsyncRuntime 异步驱动）+ facade
// ---------------------------------------------------------------------------

/// 确定性 prelude：时间 / 随机 / performance 钉死并冻结，动态求值入口拆除；不注册模块加载器
/// → 动态 import() 永不 resolve。之后是 set-order 顺序门与 facade。
const PRELUDE: &str = r#"
    (function () {
        var NativeDate = Date;
        var FIXED_EPOCH_MS = 0;
        // Date 包装：零参构造 / 函数式调用 = 固定纪元；显式参数构造保持原语义（纯函数）；
        // UTC / parse 保留。原生构造器只存活在闭包里，prototype.constructor 也指回包装。
        function PinnedDate() {
            if (!new.target) { return new NativeDate(FIXED_EPOCH_MS).toString(); }
            if (arguments.length === 0) { return new NativeDate(FIXED_EPOCH_MS); }
            return Reflect.construct(NativeDate, arguments, new.target);
        }
        PinnedDate.prototype = NativeDate.prototype;
        PinnedDate.UTC = NativeDate.UTC;
        PinnedDate.parse = NativeDate.parse;
        PinnedDate.now = function () { return FIXED_EPOCH_MS; };
        Object.defineProperty(NativeDate.prototype, "constructor",
            { value: PinnedDate, writable: false, configurable: false });
        Object.freeze(NativeDate.prototype);
        Object.freeze(PinnedDate);
        globalThis.Date = PinnedDate;
        Math.random = function () { return 0.5; };
        Object.freeze(Math);
        Object.defineProperty(globalThis, "performance", {
            value: Object.freeze({ now: function () { return 0; }, timeOrigin: 0 }),
            writable: false, configurable: false
        });
        // 动态求值：静态 lint 可被字符串拼接绕过（globalThis["eval"]），这里才是真正的执行面。
        var NativeFunction = Function;
        var noEval = function () { throw new Error("沙箱禁用 eval / Function 构造（动态求值）"); };
        [NativeFunction,
         Object.getPrototypeOf(async function () {}).constructor,
         Object.getPrototypeOf(function* () {}).constructor,
         Object.getPrototypeOf(async function* () {}).constructor].forEach(function (ctor) {
            Object.defineProperty(ctor.prototype, "constructor",
                { value: noEval, writable: false, configurable: false });
        });
        delete globalThis.eval;
        globalThis.Function = noEval;
    })();
    // Layer 3 set-order 顺序门：重放命中的 ask 按原完成序（order）延迟 resolve——
    // Promise.race / 交错 await 的解析顺序与原完成顺序一致。游标 __orderNext = 下一个
    // 待释放的 order；逐级微任务泵送：order N 的下游回调先于 order N+1 的解析入队（保序
    // 的关键）。迟到的旧序（< next）直接放行；新执行（非重放）与 amend 导入（无 order）
    // 的 ask 不经门。
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
    // release-on-settle（ZCode ReplaySettleOrder.open 同源）：引擎静止而脚本未结算时由宿主
    // 调用——门里等待者所等的位次已不可能被认领，按位次放行全部等待者并把游标推过它们。
    function __gate_open() {
        var indexes = Object.keys(__orderWaiters).map(Number).sort(function (a, b) { return a - b; });
        if (indexes.length === 0) { return false; }
        indexes.forEach(function (index) {
            var waiters = __orderWaiters[index];
            delete __orderWaiters[index];
            waiters.forEach(function (release) { release(); });
        });
        __orderNext = indexes[indexes.length - 1] + 1;
        return true;
    }
    // facade：宿主 envelope 结算（错误在 JS 侧抛出；重放命中且带 order 的经顺序门）。
    function __settle(raw, label) {
        var parsed = JSON.parse(raw);
        if (!parsed.ok) { throw new Error(parsed.error || (label + " 失败")); }
        if (parsed.replayed && typeof parsed.order === "number") {
            return __gate_wait(parsed.order).then(function () { return parsed.answer; });
        }
        return parsed.answer;
    }
    function agent(name) {
        name = String(name);
        if (!name) { throw new Error("agent() 需要 actor 名"); }
        return {
            name: name,
            ask: function (question) {
                return __ask_raw(name, String(question)).then(function (raw) {
                    return __settle(raw, "ask");
                });
            },
            askJson: function (question, schema) {
                return __ask_json_raw(name, String(question), JSON.stringify(schema)).then(function (raw) {
                    return __settle(raw, "askJson");
                });
            }
        };
    }
    function result(text) { __result(String(text)); }
"#;

/// 单片 JS CPU 预算（中断处理器轮询）：宿主 future 结算 / 顺序门放行时 `reset`，JS 续体获得
/// 新的一片；超预算时记录中断并重置（让包装层的 reject 回调能跑完上报），返回 true 终止当前片。
struct SliceBudget {
    started: Instant,
    slice: Duration,
    deadline_ms: AtomicU64,
    interrupted: AtomicBool,
}

impl SliceBudget {
    fn new() -> Self {
        Self::with_slice(ENGINE_SLICE_BUDGET)
    }

    fn with_slice(slice: Duration) -> Self {
        let budget = SliceBudget {
            started: Instant::now(),
            slice,
            deadline_ms: AtomicU64::new(0),
            interrupted: AtomicBool::new(false),
        };
        budget.reset();
        budget
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn reset(&self) {
        self.deadline_ms.store(
            self.elapsed_ms() + self.slice.as_millis() as u64,
            Ordering::Relaxed,
        );
    }

    fn poll_interrupt(&self) -> bool {
        if self.elapsed_ms() < self.deadline_ms.load(Ordering::Relaxed) {
            return false;
        }
        self.interrupted.store(true, Ordering::Relaxed);
        self.reset();
        true
    }

    fn interrupted(&self) -> bool {
        self.interrupted.load(Ordering::Relaxed)
    }
}

/// 脚本结算形态：async IIFE 正常返回（`undefined` → None，否则 `String(value)`）或抛错。
#[derive(Debug)]
enum ScriptOutcome {
    Returned(Option<String>),
    Threw(String),
}

/// 提取 JS 异常的真实消息（Error 对象取 message；其他值按字符串强转）。
fn js_error(ctx: &rquickjs::Ctx, error: rquickjs::Error) -> String {
    let caught = ctx.catch();
    let detail = caught
        .clone()
        .into_exception()
        .and_then(|exception| exception.message())
        .or_else(|| {
            caught
                .get::<rquickjs::Coerced<String>>()
                .ok()
                .map(|text| text.0)
        })
        .unwrap_or_else(|| "exception".to_string());
    format!("{error}: {detail}")
}

/// 沙箱 + 确定性 prelude（AsyncRuntime 异步驱动）；`register_hosts` 在 prelude 之后、
/// 用户脚本之前注册宿主函数。用户脚本被包进 async IIFE（顶层 await / Promise.all
/// 可用），返回值或抛错经 `__script_done` 回传；`None` = 脚本未结算（挂在永不 resolve
/// 的 promise 上）。
async fn eval_deterministic(
    script: String,
    budget: Arc<SliceBudget>,
    register_hosts: impl FnOnce(&rquickjs::Ctx) + Send + 'static,
) -> Result<Option<ScriptOutcome>, String> {
    let runtime =
        AsyncRuntime::new().map_err(|error| format!("QuickJS 运行时创建失败: {error}"))?;
    runtime.set_memory_limit(ENGINE_MEMORY_LIMIT).await;
    runtime.set_max_stack_size(1024 * 1024).await;
    let interrupt_budget = Arc::clone(&budget);
    runtime
        .set_interrupt_handler(Some(Box::new(move || interrupt_budget.poll_interrupt())))
        .await;
    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|error| format!("QuickJS 上下文创建失败: {error}"))?;
    let settled: Arc<StdMutex<Option<ScriptOutcome>>> = Arc::default();
    let settled_sink = Arc::clone(&settled);
    context
        .async_with(|ctx| Box::pin(async move {
            ctx.eval::<(), _>(PRELUDE)
                .map_err(|error| format!("prelude 执行失败: {}", js_error(&ctx, error)))?;
            register_hosts(&ctx);
            let done_fn = Function::new(
                ctx.clone(),
                move |ok: bool, text: Option<String>| -> String {
                    *settled_sink.lock().unwrap_or_else(|poison| poison.into_inner()) =
                        Some(if ok {
                            ScriptOutcome::Returned(text)
                        } else {
                            ScriptOutcome::Threw(text.unwrap_or_default())
                        });
                    "ok".to_string()
                },
            )
            .expect("注册 __script_done");
            ctx.globals()
                .set("__script_done", done_fn)
                .expect("set __script_done");
            // async IIFE：顶层 await / Promise.all 可用；返回值 / 抛错经 __script_done 回传
            //（未捕获异常不会丢：reject 分支上报；中断等不可捕获错误同样落到这里）。
            let wrapped = format!(
                "(async () => {{\n{script}\n}})().then(\n\
                 function (value) {{ __script_done(true, value === undefined ? undefined : String(value)); }},\n\
                 function (error) {{ __script_done(false, String(error)); }}\n);"
            );
            ctx.eval::<rquickjs::Value, _>(wrapped)
                .map_err(|error| format!("脚本执行失败: {}", js_error(&ctx, error)))?;
            Ok::<(), String>(())
        }))
        .await?;
    budget.reset();
    // 驱动全部 JS 任务与宿主 future（ask 子代理、Promise 链）到静止。引擎静止而脚本未结算
    // 时，若顺序门里还有等待者，其位次已不可能被认领（ZCode release-on-settle 同源）→
    // 放行后再驱动一轮；没有等待者则脚本确实挂在永不 resolve 的 promise 上，交调用方判失败。
    loop {
        runtime.idle().await;
        let is_settled = settled
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_some();
        if is_settled || budget.interrupted() {
            break;
        }
        let released = context
            .async_with(|ctx| {
                Box::pin(async move { ctx.eval::<bool, _>("__gate_open()").unwrap_or(false) })
            })
            .await;
        if !released {
            break;
        }
        budget.reset();
    }
    let outcome = settled
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take();
    Ok(outcome)
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
    actor: String,
    question: String,
) -> String {
    if question.is_empty() {
        return json!({"ok": false, "error": "ask 的问题为空"}).to_string();
    }
    // 取消后（父 turn /cancel、工具调用被放弃、墙钟超时）引擎还会在宽限期内跑一会儿：
    // 脚本若 try/catch 住失败的 ask 再补问，这里直接拒绝——不占调用点编号、不写 ask 事件、
    // 不登记转瞬即被取消的子代理执行。
    if shared.cancel.is_cancelled() {
        shared.budget.reset();
        return json!({"ok": false, "error": "workflow 已取消"}).to_string();
    }
    let input_hash = fnv1a64(&format!("{actor}\u{0}{question}"));
    let hash_hex = format!("{input_hash:016x}");
    let ask_seq = {
        let mut engine = shared.engine.lock().unwrap_or_else(|poison| poison.into_inner());
        engine.asks_total += 1;
        // 调用点身份 = (actor, inputHash, 本 run 内第几次)：同一问题问两次是两个调用点，
        // 重放按它精确配对；本 run 内不做去重（ZCode 亦无——缓存答案没有可认领的门序）。
        let occurrence = engine
            .occurrences
            .entry((actor.clone(), input_hash))
            .or_insert(0);
        let index = *occurrence;
        *occurrence += 1;
        if let Some(replayed) = engine
            .replay
            .remove(&(actor.clone(), input_hash, index))
        {
            engine.asks_replayed += 1;
            // 答案一并落本 run 的 journal：再次 resume 时本 journal 自足（不必追 amended_from）。
            append_workflow_event(
                &shared.journal,
                &json!({
                    "event": "ask_replayed",
                    "actor": actor,
                    "input_hash": hash_hex,
                    "answer": replayed.answer,
                }),
            );
            shared.budget.reset();
            // set-order：order = 原完成序（amend 导入为 null → 不经门）。JS 侧顺序门按它
            // 延迟 resolve，使 Promise.race / 交错 await 的解析顺序与原完成顺序一致。
            return json!({
                "ok": true,
                "answer": replayed.answer,
                "replayed": true,
                "order": replayed.order,
            })
            .to_string();
        }
        // journal 记录【发起序】（库内 seq 单调、跨进程唯一），answer 按【完成序】追加并经
        // ask_seq 指回。发起事件写不进去就没有可配对的答案，resume 只会重问：直接把错误
        // 交给脚本（ok:false），不假装发起成功。锁内写库：发起序与库内 seq 保持一致。
        match shared.journal.append(&json!({
            "event": "ask",
            "actor": actor,
            "question": truncate_chars(&question, MAX_QUESTION_CHARS),
            "input_hash": hash_hex,
        })) {
            Ok(seq) => seq,
            Err(error) => {
                shared.budget.reset();
                return json!({
                    "ok": false,
                    "error": truncate_chars(&format!("ask 未能写入 journal：{error}"), 500),
                })
                .to_string();
            }
        }
    };
    let _permit = tokio::select! {
        biased;
        _ = shared.cancel.cancelled() => {
            // 排队等许可期间被取消：ask 事件已写（resume 时该调用点只会重问），但不再
            // 启动子代理。
            shared.budget.reset();
            return json!({"ok": false, "error": "workflow 已取消"}).to_string();
        }
        permit = shared.permits.acquire() => permit.expect("workflow 信号量不会关闭"),
    };
    let answer = run_workflow_ask(runtime, shared, &actor, &question).await;
    // 宿主 future 结算 → JS 续体获得新的一片 CPU 预算。
    shared.budget.reset();
    match answer {
        Ok(answer) => {
            append_workflow_event(
                &shared.journal,
                &json!({
                    "event": "answer",
                    "ask_seq": ask_seq,
                    "actor": actor,
                    "input_hash": hash_hex,
                    "answer": truncate_chars(&answer, MAX_ANSWER_CHARS),
                }),
            );
            json!({"ok": true, "answer": answer, "replayed": false}).to_string()
        }
        Err(error) => json!({
            "ok": false,
            "error": truncate_chars(&error, 500),
        })
        .to_string(),
    }
}

/// ask 的子代理执行：经后台执行登记（AgentStatus 可见、AgentCancel 可控）+ 竞速通道取回
/// 结果（Agent 工具前台路径同款）；`done` 直到结果回来才触发，期间 workflow 取消（父 turn
/// /cancel、工具调用被放弃、墙钟超时）都会联动终止子代理。
async fn run_workflow_ask(
    runtime: &AgentToolRuntime,
    shared: &EngineShared,
    actor: &str,
    question: &str,
) -> Result<String, String> {
    let run_id = &shared.run_id;
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
    let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<AgentTaskResult>(1);
    let done = CancellationToken::new();
    // 本 future 被丢弃（引擎中止 / 超时后放弃）时也触发 done，watcher 不泄漏；
    // 中止路径上 shared.cancel 先于丢弃触发，子代理已被 watcher 联动取消。
    let _done_guard = done.clone().drop_guard();
    // 子代理的后台执行与取消 watcher 都要落在外层 runtime 上（见 EngineShared::outer），
    // 并带上发起方 turn 的 task-local 上下文；结果经 mpsc 跨 runtime 取回。
    let launch = {
        let runtime = runtime.clone();
        let description = format!("workflow {run_id} · {actor}");
        let linkage = Some((shared.cancel.clone(), done.clone()));
        shared.outer.spawn(shared.inherited.clone().run(async move {
            runtime
                .launch_background_task_impl(spec, description, Some(result_tx), linkage)
                .await
        }))
    };
    launch
        .await
        .map_err(|error| format!("子代理启动任务失败: {error}"))??;
    let result = result_rx.recv().await;
    // 结果已回：解除取消联动，watcher 退出（不泄漏）。
    done.cancel();
    let Some(result) = result else {
        return Err("子代理执行通道关闭，无结果返回".to_string());
    };
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
async fn ask_json_host(
    runtime: &AgentToolRuntime,
    shared: &EngineShared,
    actor: String,
    question: String,
    schema: Value,
) -> String {
    let raw = ask_host(runtime, shared, actor, question).await;
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
    // order 原样透传：重放命中的 askJson 同样经顺序门（否则会错认领 order 0）。
    json!({
        "ok": true,
        "answer": parsed,
        "replayed": envelope["replayed"],
        "order": envelope["order"],
    })
    .to_string()
}

/// 引擎共享态：宿主回调（并行 promise）、引擎驱动与收尾读取共用。
struct EngineShared {
    engine: StdMutex<EngineState>,
    /// 并行 ask 许可（`max_parallel_asks`）。
    permits: tokio::sync::Semaphore,
    /// 单片 JS CPU 预算（中断处理器与宿主回调共享）。
    budget: Arc<SliceBudget>,
    run_id: String,
    /// 本 run 的 journal 事件流（`workflow/<run_id>`）。
    journal: EventLog,
    /// 本 run 的取消令牌：工具 future 丢弃 / 父 turn 取消 / 墙钟超时触发，联动全部 ask 子代理。
    cancel: CancellationToken,
    /// 外层（多线程）runtime 句柄：ask 子代理的后台执行在它上面 spawn。引擎自己跑在
    /// spawn_blocking 里的内嵌 current_thread runtime 上，中止 / 超时后内嵌 runtime 随即
    /// 销毁——子代理若也在上面，会被连根丢弃而没有终态（登记表里永远 running）。
    outer: tokio::runtime::Handle,
    /// 发起方 turn 的 task-local 上下文（沙箱权限 / 会话键 / bot 身份），跨 runtime 带给子代理。
    inherited: super::InheritedTurnContext,
    /// 同 run 互斥守卫：挂在共享态上，随最后一个持有者（引擎线程 / 宿主回调）一起释放。
    /// 工具 future 被丢弃后引擎还要跑完取消宽限期，这段时间同一 run 仍不能被 resume——
    /// 否则宽限期内脚本补发的 ask 会落在新一代 run_resumed 之后、打乱调用点编号。
    _active_run: ActiveRunGuard,
}

#[derive(Default)]
struct EngineState {
    asks_total: u64,
    asks_replayed: u64,
    replay: ReplayTable,
    /// 本 run 内同 (actor, inputHash) 已发起的次数：调用点身份（第几次）。
    occurrences: HashMap<(String, u64), u32>,
    result: Option<String>,
}

/// 进程内「正在执行的 journal 流」登记：同一条流不允许并发执行。只有 resume 会复用 id，
/// 两个执行器同时往一条流写事件——workflow 的 ask / answer 调用点编号（occurrence）与
/// 重放配对会错乱，plan 的 plan_resumed 代标记会被另一方后写的覆盖、把未应答的提问误判
/// 成已过代。键带类型前缀（`workflow:` / `plan:`），两类 id 空间互不干扰。守卫 Drop 即释放。
///
/// 只覆盖本进程：CLI 与桌面端同时开同一个 `state.sqlite` 不在此列（没有跨进程租约）。
pub(super) struct ActiveRunGuard(String);

static ACTIVE_RUNS: StdMutex<Option<HashSet<String>>> = StdMutex::new(None);

impl ActiveRunGuard {
    /// workflow run：守卫随 [`EngineShared`] 存活，即引擎（含取消宽限期）真正退出才释放，
    /// 而不是工具 future 一丢弃就放行下一次 resume。
    fn acquire(run_id: &str) -> Option<Self> {
        Self::acquire_key(format!("workflow:{run_id}"))
    }

    /// 计划执行（`PlanRun`，按 plan_id）。
    pub(super) fn acquire_plan(plan_id: &str) -> Option<Self> {
        Self::acquire_key(format!("plan:{plan_id}"))
    }

    fn acquire_key(key: String) -> Option<Self> {
        let mut active = ACTIVE_RUNS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        active
            .get_or_insert_with(HashSet::new)
            .insert(key.clone())
            .then(|| ActiveRunGuard(key))
    }
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        if let Some(active) = ACTIVE_RUNS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_mut()
        {
            active.remove(&self.0);
        }
    }
}

/// 注册 facade 的宿主函数：`__ask_raw` / `__ask_json_raw`（async → promise）与 `__result`。
fn register_workflow_hosts(
    ctx: &rquickjs::Ctx,
    runtime: AgentToolRuntime,
    shared: Arc<EngineShared>,
) {
    let (ask_runtime, ask_shared) = (runtime.clone(), Arc::clone(&shared));
    let ask_fn = Function::new(
        ctx.clone(),
        AsyncFn(move |actor: String, question: String| {
            let (runtime, shared) = (ask_runtime.clone(), Arc::clone(&ask_shared));
            async move { ask_host(&runtime, &shared, actor, question).await }
        }),
    )
    .expect("注册 __ask_raw");
    ctx.globals().set("__ask_raw", ask_fn).expect("set __ask_raw");
    let result_shared = Arc::clone(&shared);
    let result_fn = Function::new(ctx.clone(), move |text: String| -> String {
        result_shared
            .engine
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .result = Some(truncate_chars(&text, 8_000));
        "ok".to_string()
    })
    .expect("注册 __result");
    ctx.globals().set("__result", result_fn).expect("set __result");
    let json_fn = Function::new(
        ctx.clone(),
        AsyncFn(move |actor: String, question: String, schema: String| {
            let (runtime, shared) = (runtime.clone(), Arc::clone(&shared));
            async move {
                let schema: Value = serde_json::from_str(&schema).unwrap_or(Value::Null);
                ask_json_host(&runtime, &shared, actor, question, schema).await
            }
        }),
    )
    .expect("注册 __ask_json_raw");
    ctx.globals()
        .set("__ask_json_raw", json_fn)
        .expect("set __ask_json_raw");
}

/// 在内嵌 runtime 上驱动引擎到结算，并与取消令牌、墙钟预算竞速：取消 / 超时先经 token 让
/// 子代理优雅收尾（宽限期），再放弃引擎。
async fn drive_engine(
    script: String,
    max_run: Duration,
    runtime: AgentToolRuntime,
    shared: Arc<EngineShared>,
) -> Result<Option<ScriptOutcome>, String> {
    let host_shared = Arc::clone(&shared);
    let drive = eval_deterministic(script, Arc::clone(&shared.budget), move |ctx| {
        register_workflow_hosts(ctx, runtime, host_shared)
    });
    tokio::pin!(drive);
    let abort_reason = tokio::select! {
        biased;
        _ = shared.cancel.cancelled() => "workflow 已取消（父 turn 取消或工具调用被放弃）".to_string(),
        driven = tokio::time::timeout(max_run, &mut drive) => match driven {
            Ok(outcome) => return outcome,
            Err(_) => format!("workflow 超过墙钟预算 {}s（max_run_secs）", max_run.as_secs()),
        },
    };
    shared.cancel.cancel();
    let _ = tokio::time::timeout(CANCEL_GRACE, &mut drive).await;
    Err(abort_reason)
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
         — answered asks replay from the run journal (agent/state.sqlite, workflow/<run_id>) at zero provider \
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
                "max_parallel_asks": {"type": "integer", "minimum": 1, "maximum": 16, "description": "Concurrency cap for parallel asks (default 8)."},
                "max_run_secs": {"type": "integer", "minimum": 1, "maximum": 86400, "description": "Overall wall-clock budget for the run in seconds (default 3600). Exceeding it fails the run; the journal is kept for resume."},
                "type_check": {"type": "boolean", "description": "Run tsc type-level check against facade.d.ts when the host has node+tsc (auto-downgrades to syntax+lint when unavailable). Default true."}
            },
            "required": ["script"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(script) = get_str(&input, "script")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            // CRLF → LF：hash 与执行都用规范化文本，跨平台重提同一脚本 resume 不被拒。
            .map(|s| s.replace("\r\n", "\n"))
        else {
            return ToolResult::err("script is required for WorkflowRun".to_string());
        };
        if script.chars().count() > MAX_SCRIPT_CHARS {
            return ToolResult::err(format!("script 超过 {MAX_SCRIPT_CHARS} 字符"));
        }
        // Layer 3 `.ask<T>` 泛型编译（第十二轮）：内联类型注解编译成 JSON schema，调用点
        // 重写为 askJson(question, schema)。script hash 仍按【作者原文】计算（resume 的
        // 逐字节一致语义不变）；语法 / lint 检查与引擎执行都走重写后的 exec_script。
        // 编译在 blocking 线程（oxc 解析是纯 CPU）。
        let compile_input = script.clone();
        let compiled = tokio::task::spawn_blocking(move || {
            if super::workflow_ast::has_typed_asks(&compile_input) {
                super::workflow_ast::compile_typed_asks(&compile_input)
                    .map(|transform| (transform.exec_script, transform.typed_asks))
            } else {
                Ok((compile_input.clone(), Vec::new()))
            }
        })
        .await
        .unwrap_or_else(|error| {
            tracing::warn!("typed ask 编译线程失败，按未重写脚本继续: {error}");
            Ok((script.clone(), Vec::new()))
        });
        let (exec_script, typed_asks) = match compiled {
            Ok(compiled) => compiled,
            Err(diagnostics) => {
                return ToolResult::err(format_diagnostics(&diagnostics));
            }
        };
        // Layer 3 类型/facade 检查：带诊断的脚本拒绝提交，诊断只回给模型不落用户。
        // tsc 类型级检查耗时（秒级）且为阻塞调用：放 blocking 线程。
        // 默认启用（不可用自动降级），显式 false 才跳过。
        let type_check = input
            .get("type_check")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        // tsc 类型级检查优先对【作者原文】跑（facade.d.ts 已声明 ask<T> 泛型，原文可
        // 直接检查出类型错误；重写后的 exec script 只剩 askJson，丢掉 ask<T> 的类型
        // 语境）。无 typed ask 时两者一致，仍用 exec script。语法探针与 lint 始终跑
        // exec script（QuickJS 只认重写后的纯 JS）。
        let tsc_input = if typed_asks.is_empty() {
            exec_script.clone()
        } else {
            script.clone()
        };
        let lint_script = exec_script.clone();
        let diagnostics = tokio::task::spawn_blocking(move || {
            let mut diagnostics = check_script(&lint_script, false);
            if type_check && diagnostics.is_empty() {
                if let Some(mut tsc_diagnostics) = type_check_with_tsc(&tsc_input) {
                    diagnostics.append(&mut tsc_diagnostics);
                }
            }
            diagnostics
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
        let max_run = Duration::from_secs(
            input
                .get("max_run_secs")
                .and_then(Value::as_u64)
                .map(|secs| secs.clamp(1, MAX_MAX_RUN_SECS))
                .unwrap_or(DEFAULT_MAX_RUN_SECS),
        );
        let resume_request = get_str(&input, "resume_run_id")
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let amend_request = get_str(&input, "amend_run_id")
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if resume_request.is_some() && amend_request.is_some() {
            return ToolResult::err("resume_run_id 与 amend_run_id 互斥：前者续跑同一 run（脚本须逐字节一致），后者按 actor 导入前驱已完成 ask 的修订热修（脚本可改）。".to_string());
        }
        // resume/amend 的 run_id 由调用方给出、将用于定位 journal 流：必须与 sanitize 结果
        // 逐字一致（`sanitize_agent_name` 对纯符号输入会回落成 "agent"，`is_empty()` 判不出来；
        // 大小写 / 空白变体也会悄悄指向另一条流）。与 ResolveWorkflowQuestion 同一准入口径。
        if let Some(raw_id) = resume_request.or(amend_request) {
            if sanitize_agent_name(raw_id) != raw_id {
                return ToolResult::err(format!(
                    "run_id '{raw_id}' 不合法（须与先前 WorkflowRun 结果里的 run_id 逐字一致）"
                ));
            }
        }
        let run_id = match (resume_request, amend_request) {
            (Some(raw_id), _) => raw_id.to_string(),
            (None, Some(_)) => next_execution_id(&format!("{run_name}-amend")),
            (None, None) => next_execution_id(&run_name),
        };
        // 同一 run_id 不允许并发执行（只有 resume 会复用 id）：两个引擎往同一条 journal
        // 交错写 ask / answer，重放配对与调用点编号都会错乱。守卫随后移交给 EngineShared，
        // 与引擎同寿命（见其字段说明）。
        let Some(active_run) = ActiveRunGuard::acquire(&run_id) else {
            return ToolResult::err(format!(
                "run '{run_id}' 正在执行中，不能并发 resume 同一个 run；等它结束后再续跑"
            ));
        };
        let journal = EventLog::workflow(&self.runtime.journal, &run_id);
        // Layer 3 静态洞察（AST 优先，文本降级）：actor/ask 数/并行/依赖图，
        // 入 run_started 与报告；typed ask 计数一并入事件。
        let insight = analyze_workflow_script(&exec_script);
        let mut replay: ReplayTable = HashMap::new();
        let mut amended_from: Option<String> = None;
        let mut imported_asks = 0usize;
        // run_started / run_resumed 是 resume 的锚点（script_hash 校验、新一代调用点编号的
        // 起点）：写不进 journal 就不能开跑——否则 run 结束后报告里的 resume 提示必然失败，
        // 而且已付费的 ask 答案没有任何一代能认领。
        let lifecycle_event: Value;
        if resume_request.is_some() {
            match load_workflow_journal(&journal) {
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
            lifecycle_event =
                json!({"event": "run_resumed", "script_hash": format!("{script_hash:016x}")});
        } else if let Some(predecessor) = amend_request {
            // Layer 3 Amend 热修（ZCode「按子代理名字导入前驱已完成工作」同源）：
            // 修订脚本按 actor 名 + inputHash 匹配前驱 journal——问答文本未变的 ask
            // 零调用继承答案；改写/新增的问题照常执行；脚本可以任意修改。
            let predecessor_journal = EventLog::workflow(&self.runtime.journal, predecessor);
            match load_workflow_journal(&predecessor_journal) {
                Ok((_predecessor_hash, mut table)) => {
                    // 导入不经顺序门：修订脚本可能改写/删除了前驱的某些 ask，原完成序里会出现
                    // 永不被认领的位次——保留 order 会让后面的重放 ask 在门里等到引擎静止。
                    for entry in table.values_mut() {
                        entry.order = None;
                    }
                    imported_asks = table.len();
                    replay = table;
                    amended_from = Some(predecessor.to_string());
                }
                Err(error) => {
                    return ToolResult::err(format!("amend 前驱加载失败: {error}"));
                }
            }
            lifecycle_event = json!({
                "event": "run_started",
                "script_hash": format!("{script_hash:016x}"),
                "amended_from": predecessor,
                "imported_asks": imported_asks,
            });
        } else {
            lifecycle_event = json!({
                "event": "run_started",
                "script_hash": format!("{script_hash:016x}"),
                "typed_asks": typed_asks.iter().map(|(line, _)| line).collect::<Vec<_>>(),
                "insight": insight,
            });
        }
        if let Err(error) = journal.append(&lifecycle_event) {
            return ToolResult::err(format!(
                "workflow 未启动：{error}。journal 写不进去的 run 无法 resume，拒绝执行以免白付 ask 费用"
            ));
        }

        // 取消：工具 future 被丢弃（父 turn /cancel、调用被放弃）→ 守卫触发 token → 引擎线程
        // 退出、全部 ask 子代理联动终止；顶层 turn 的取消令牌（在作用域内时）作为父令牌链入。
        let cancel = rclaw_tools::turn_state::turn_cancel()
            .map(|parent| parent.child_token())
            .unwrap_or_default();
        let _cancel_guard = cancel.clone().drop_guard();
        // 此处仍在外层 runtime、发起方 turn 的 task 里：句柄与 task-local 快照都在这里取。
        // 所有宿主（CLI / 桌面 / 安卓）都在 tokio 上跑 turn；万一不是，报错而不是 panic。
        let outer = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            Err(error) => {
                return ToolResult::err(format!("WorkflowRun 需要在 tokio 运行时内执行: {error}"));
            }
        };
        let shared = Arc::new(EngineShared {
            engine: StdMutex::new(EngineState {
                replay,
                ..EngineState::default()
            }),
            permits: tokio::sync::Semaphore::new(max_parallel_asks),
            budget: Arc::new(SliceBudget::new()),
            run_id: run_id.clone(),
            journal: journal.clone(),
            cancel,
            outer,
            inherited: super::InheritedTurnContext::capture(),
            _active_run: active_run,
        });
        let engine_runtime = self.runtime.clone();
        let engine_shared = Arc::clone(&shared);
        // rquickjs 的 async future 非 Send（持裸 JS 指针）：整段引擎放进
        // spawn_blocking + 内嵌 current_thread runtime 驱动——future 不跨线程，
        // JoinHandle（快照结果）是 Send。ask 的子代理执行 spawn 在外层 runtime 上
        //（EngineShared::outer），结果经 mpsc 回到内嵌 runtime 里的宿主回调。
        let executed = tokio::task::spawn_blocking(move || {
            let inner = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("workflow 内嵌 runtime 创建失败: {error}"))?;
            inner.block_on(drive_engine(exec_script, max_run, engine_runtime, engine_shared))
        })
        .await;
        let outcome = match executed {
            Ok(outcome) => outcome,
            Err(error) => {
                return ToolResult::err(format!("workflow 引擎线程失败: {error}"));
            }
        };
        let (asks_total, asks_replayed, final_result) = {
            let engine = shared.engine.lock().unwrap_or_else(|p| p.into_inner());
            (engine.asks_total, engine.asks_replayed, engine.result.clone())
        };
        // 结算判定：引擎错误 / 取消 / 超时 / CPU 片中断 / 脚本抛错 / 未结算 → run_failed
        //（journal 保留已完成问答，可 resume）；只有 IIFE 正常返回才算 completed。
        let returned = match outcome {
            Err(error) => Err(error),
            Ok(_) if shared.budget.interrupted() => Err(format!(
                "脚本单片 CPU 预算（{}s）耗尽被中断",
                ENGINE_SLICE_BUDGET.as_secs()
            )),
            Ok(None) => Err("脚本未结算（挂在永不 resolve 的 promise 上）".to_string()),
            Ok(Some(ScriptOutcome::Threw(error))) => Err(format!("ERROR: {error}")),
            Ok(Some(ScriptOutcome::Returned(value))) => Ok(value),
        };
        let returned = match returned {
            Ok(value) => value,
            Err(error) => {
                append_workflow_event(
                    &journal,
                    &json!({"event": "run_failed", "error": truncate_chars(&error, 500)}),
                );
                return ToolResult::err(format!(
                    "{error}\n（journal 已保留已完成问答：{}；同一脚本配 resume_run_id 重提可续跑）",
                    journal.locator()
                ));
            }
        };
        // result() 未调用而 IIFE 返回了非 undefined 值 → 以 String(返回值) 作为结果。
        let final_result = final_result.or(returned);
        append_workflow_event(
            &journal,
            &json!({"event": "run_completed", "asks_total": asks_total, "asks_replayed": asks_replayed}),
        );
        let mut report = json!({
            "status": "completed",
            "run_id": run_id,
            "script_hash": format!("{script_hash:016x}"),
            "asks_total": asks_total,
            "asks_replayed": asks_replayed,
            "result": final_result,
            "journal": journal.locator(),
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
    use async_trait::async_trait;

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

    #[test]
    fn dependency_graph_multiline_promise_all_unique_ids_and_ask_json() {
        // 跨行 Promise.all：三个 ask 分三行，仍是同一波；同一行两个 ask 的 id 不冲突；
        // askJson 也计为节点。
        let script = concat!(
            "const coder = agent(\"coder\");\n",
            "const prover = agent(\"prover\");\n",
            "const [a, b, c] = await Promise.all([\n",
            "  coder.ask(\"x\"), coder.ask(\"y\"),\n",
            "  prover.askJson(\"z\", { type: \"object\" }),\n",
            "]);\n",
            "const r = await prover.ask(\"合并\");\n"
        );
        let graph = analyze_dependency_graph(script);
        let ids: Vec<&str> = graph.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["ask-4", "ask-4-2", "ask-5", "ask-7"], "{ids:?}");
        assert!(graph.nodes[..3].iter().all(|n| n.parallel), "跨行波内节点应并行");
        assert!(!graph.nodes[3].parallel);
        assert_eq!(graph.waves.len(), 2, "{:?}", graph.waves);
        assert_eq!(graph.waves[0].len(), 3);
        assert_eq!(graph.edges.len(), 3, "波内每个节点 → 后续 ask: {:?}", graph.edges);
        assert_eq!(analyze_workflow_script(script).ask_count, 4);
    }

    /// 经 `__result` 回传脚本观测值（async IIFE + Promise 全链路）。
    async fn observe(script: &str) -> (Result<Option<ScriptOutcome>, String>, String) {
        let received: Arc<StdMutex<String>> = Arc::new(StdMutex::new(String::new()));
        let sink = Arc::clone(&received);
        let outcome = eval_deterministic(
            script.to_string(),
            Arc::new(SliceBudget::new()),
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
        let observed = received.lock().unwrap_or_else(|p| p.into_inner()).clone();
        (outcome, observed)
    }

    #[tokio::test]
    async fn sandbox_pins_nondeterminism() {
        // Date 的全部取时路径（Date.now / 零参 new / 函数式调用 / Reflect.construct /
        // toISOString）、Math.random、performance 都钉死；显式参数的 Date 保持原语义。
        let (outcome, observed) = observe(concat!(
            "var v = JSON.stringify([Date.now(), Math.random(), new Date().getTime(),\n",
            "  new Date().toISOString(), Date() === new Date(0).toString(),\n",
            "  Reflect.construct(Date, []).getTime(), new (Date)().valueOf(),\n",
            "  performance.now(), performance.timeOrigin, new Date(5).getTime(),\n",
            "  new Date(0).constructor === Date]);\n",
            "__result(v);"
        ))
        .await;
        assert!(matches!(outcome, Ok(Some(ScriptOutcome::Returned(None)))), "{outcome:?}");
        assert_eq!(
            observed,
            "[0,0.5,0,\"1970-01-01T00:00:00.000Z\",true,0,0,0,0,5,true]",
            "确定性钉死值必须生效"
        );
    }

    #[tokio::test]
    async fn sandbox_neutralizes_eval_bypass_that_lint_cannot_see() {
        // 静态 lint 看不见字符串里的 "eval" 与 Function.prototype.constructor 路径——
        // 运行时沙箱才是真正的执行面：eval 已拆除、Function 族构造器抛错。
        let script = concat!(
            "var e = globalThis[\"eval\"];\n",
            "var f; try { f = (function () {}).constructor(\"return 1\")(); } catch (err) { f = \"blocked\"; }\n",
            "var g; try { g = (async function () {}).constructor(\"return 2\"); } catch (err) { g = \"blocked\"; }\n",
            "__result(JSON.stringify([typeof e, f, g]));"
        );
        assert!(check_script(script, false).is_empty(), "lint 不应命中字符串内的 eval");
        let (outcome, observed) = observe(script).await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(observed, "[\"undefined\",\"blocked\",\"blocked\"]");
    }

    #[tokio::test]
    async fn iife_return_value_and_thrown_error_are_reported() {
        // result() 未调用时 IIFE 返回值作为结果；抛错按 Threw 回传真实消息。
        let (outcome, _) = observe("return \"done\";").await;
        assert!(
            matches!(&outcome, Ok(Some(ScriptOutcome::Returned(Some(value)))) if value == "done"),
            "{outcome:?}"
        );
        let (outcome, _) = observe("throw new Error(\"boom\");").await;
        assert!(
            matches!(&outcome, Ok(Some(ScriptOutcome::Threw(error))) if error.contains("boom")),
            "{outcome:?}"
        );
        // 永不 resolve 的 promise：引擎静止而未结算 → None（调用方判失败，不报 completed）。
        let (outcome, _) = observe("await new Promise(function () {});").await;
        assert!(matches!(outcome, Ok(None)), "{outcome:?}");
    }

    #[tokio::test]
    async fn slice_budget_interrupts_runaway_script_and_marks_failure() {
        let budget = Arc::new(SliceBudget::with_slice(Duration::from_millis(200)));
        let outcome = eval_deterministic(
            "while (true) {}".to_string(),
            Arc::clone(&budget),
            |_ctx| {},
        )
        .await;
        assert!(budget.interrupted(), "死循环必须被单片预算中断");
        assert!(
            !matches!(outcome, Ok(Some(ScriptOutcome::Returned(_)))),
            "被中断的脚本不得看起来像正常完成: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn stale_gate_order_is_released_when_engine_idles() {
        // 顺序门里等待一个永不会被认领的位次（order 1，而 order 0 无人认领）：引擎静止时放行，
        // 脚本照常结算而不是挂死。
        let (outcome, observed) =
            observe("await __gate_wait(1); __result(\"released\"); return 1;").await;
        assert!(
            matches!(&outcome, Ok(Some(ScriptOutcome::Returned(Some(value)))) if value == "1"),
            "{outcome:?}"
        );
        assert_eq!(observed, "released");
    }

    #[test]
    fn parse_tsc_diagnostics_handles_parenthesised_messages_and_line_offset() {
        let output = concat!(
            "script.ts(2,7): error TS2339: Property 'asq' does not exist on type '{ ask(question: string): Promise<string>; }'.\n",
            "facade.d.ts(3,1): error TS1005: ';' expected.\n",
            "script.ts(5,3): warning TS0000: a (b) c.\n"
        );
        let diagnostics = parse_tsc_diagnostics(output, "script", 1);
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert_eq!((diagnostics[0].line, diagnostics[0].col), (1, 7));
        assert!(
            diagnostics[0].message.ends_with("Promise<string>; }'."),
            "{}",
            diagnostics[0].message
        );
        assert!(diagnostics[0].message.contains("TS2339"));
        assert_eq!((diagnostics[1].line, diagnostics[1].col), (4, 3));
    }

    #[test]
    fn journal_loader_pairs_answers_by_journal_seq_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JournalStore::open(&Layout::new(dir.path().join("agent"))).unwrap();
        let hash = format!("{:016x}", fnv1a64("coder\u{0}same"));
        let key = |occurrence: u32| ("coder".to_string(), fnv1a64("coder\u{0}same"), occurrence);
        // 不存在的 run：明确报错而不是当空 journal。
        let missing = EventLog::workflow(&store, "nope");
        assert!(load_workflow_journal(&missing).unwrap_err().contains("不存在"));
        // 同问两次（Promise.all），完成顺序相反；answer 经 ask_seq（= 发起事件的库内 seq）
        // 精确配对，不按 hash 猜。
        let path = EventLog::workflow(&store, "run");
        path.append(&json!({"event": "run_started", "script_hash": "00000000000000ab"}))
            .unwrap();
        let ask1 = path
            .append(&json!({"event": "ask", "actor": "coder", "input_hash": hash}))
            .unwrap();
        let ask2 = path
            .append(&json!({"event": "ask", "actor": "coder", "input_hash": hash}))
            .unwrap();
        assert_eq!((ask1, ask2), (2, 3), "库内 seq 从 1 起、每条事件 +1");
        path.append(&json!({"event": "answer", "ask_seq": ask2, "actor": "coder", "input_hash": hash, "answer": "second"}))
            .unwrap();
        path.append(&json!({"event": "answer", "ask_seq": ask1, "actor": "coder", "input_hash": hash, "answer": "first"}))
            .unwrap();
        path.append(&json!("not an object")).unwrap();
        let (script_hash, table) = load_workflow_journal(&path).unwrap();
        assert_eq!(script_hash, 0xab);
        let (first, second) = (&table[&key(0)], &table[&key(1)]);
        assert_eq!((first.answer.as_str(), first.order), ("first", Some(1)));
        assert_eq!((second.answer.as_str(), second.order), ("second", Some(0)));
        // 没有 ask_seq 的 answer、指向上一代 ask 的 answer 都跳过（宁可重问不错配）；
        // ask_replayed 自带答案也是调用点；新一代从 run_resumed 起重新编号。
        let path = EventLog::workflow(&store, "resumed");
        path.append(&json!({"event": "run_started", "script_hash": "00000000000000ab"}))
            .unwrap();
        let stale_ask = path
            .append(&json!({"event": "ask", "actor": "coder", "input_hash": hash}))
            .unwrap();
        path.append(&json!({"event": "answer", "input_hash": hash, "answer": "no-ask-seq"}))
            .unwrap();
        path.append(&json!({"event": "run_resumed", "script_hash": "00000000000000ab"}))
            .unwrap();
        path.append(&json!({"event": "answer", "ask_seq": stale_ask, "actor": "coder", "input_hash": hash, "answer": "cross-generation"}))
            .unwrap();
        path.append(&json!({"event": "ask_replayed", "actor": "coder", "input_hash": hash, "answer": "replayed"}))
            .unwrap();
        let fresh_ask = path
            .append(&json!({"event": "ask", "actor": "coder", "input_hash": hash}))
            .unwrap();
        path.append(&json!({"event": "answer", "ask_seq": fresh_ask, "actor": "coder", "input_hash": hash, "answer": "fresh"}))
            .unwrap();
        let (_, table) = load_workflow_journal(&path).unwrap();
        assert_eq!(table.len(), 2, "{table:?}");
        assert_eq!(table[&key(0)].answer, "replayed");
        assert_eq!(table[&key(1)].answer, "fresh");
    }

    #[test]
    fn active_run_guard_rejects_concurrent_same_run_id() {
        let first = ActiveRunGuard::acquire("guard-test-run").expect("首个持有者");
        assert!(
            ActiveRunGuard::acquire("guard-test-run").is_none(),
            "同一 run_id 不能并发持有"
        );
        let _other = ActiveRunGuard::acquire("guard-test-run-2").expect("不同 run 互不影响");
        // plan 与 workflow 的 id 空间互不干扰：同名 plan 可与 workflow run 并存。
        let _plan = ActiveRunGuard::acquire_plan("guard-test-run").expect("plan 键带前缀，不与 workflow 冲突");
        assert!(ActiveRunGuard::acquire_plan("guard-test-run").is_none(), "同一 plan_id 不能并发持有");
        drop(first);
        assert!(ActiveRunGuard::acquire("guard-test-run").is_some(), "Drop 即释放");
    }

    // ---- 端到端（最小 runtime + 计数 provider）----

    /// 计数 provider：第 n 次调用答 "#n"——同问两次得到不同答案，可验证重放按调用点精确配对。
    struct CountingProvider {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl Provider for CountingProvider {
        fn name(&self) -> &str {
            "counting"
        }

        async fn chat(&self, _req: ChatRequest) -> rclaw_llm::Result<rclaw_core::ChatOutcome> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(rclaw_core::ChatOutcome {
                text: format!("#{n}"),
                reasoning: None,
                tool_calls: Vec::new(),
                stop: Some(rclaw_core::FinishReason::Stop),
                usage: None,
            })
        }

        async fn chat_stream(
            &self,
            req: ChatRequest,
            _on_text: rclaw_llm::OnText,
        ) -> rclaw_llm::Result<rclaw_core::ChatOutcome> {
            self.chat(req).await
        }
    }

    /// 永不回答的 provider：`started` 标记已进入推理，`dropped` 标记推理 future 被取消丢弃。
    struct HangingProvider {
        started: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Provider for HangingProvider {
        fn name(&self) -> &str {
            "hanging"
        }

        async fn chat(&self, _req: ChatRequest) -> rclaw_llm::Result<rclaw_core::ChatOutcome> {
            struct DropFlag(Arc<AtomicBool>);
            impl Drop for DropFlag {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _flag = DropFlag(Arc::clone(&self.dropped));
            self.started.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
            unreachable!("pending 永不完成")
        }

        async fn chat_stream(
            &self,
            req: ChatRequest,
            _on_text: rclaw_llm::OnText,
        ) -> rclaw_llm::Result<rclaw_core::ChatOutcome> {
            self.chat(req).await
        }
    }

    fn tool_with_provider(provider: Arc<dyn Provider>) -> WorkflowRunTool {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(dir.path().join("agent"));
        std::mem::forget(dir);
        let registry: RegistryLink = Arc::new(OnceLock::new());
        registry
            .set(Arc::new(ToolRegistry::new()))
            .unwrap_or_else(|_| panic!("registry 只初始化一次"));
        WorkflowRunTool::new(AgentToolRuntime::new(
            provider,
            MemoryStore::new(layout.clone()),
            layout.clone(),
            JournalStore::open(&layout).expect("open state.sqlite"),
            AgentOptions::default(),
            registry,
            rclaw_core::RuntimeChannelInfo::default(),
            None,
        ))
    }

    fn counting_tool() -> (WorkflowRunTool, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        let tool = tool_with_provider(Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
        }));
        (tool, calls)
    }

    async fn wait_flag(flag: &AtomicBool, what: &str) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !flag.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("等待「{what}」超时"));
    }

    #[tokio::test]
    async fn dropping_tool_future_cancels_inflight_asks() {
        // 工具 future 被丢弃（父 turn 取消 / 调用被放弃）→ 守卫触发取消令牌 → 正在推理的
        // ask 子代理被联动终止（provider future 被丢弃），不再后台空转。
        let started = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let tool = tool_with_provider(Arc::new(HangingProvider {
            started: Arc::clone(&started),
            dropped: Arc::clone(&dropped),
        }));
        let running = tokio::spawn(async move {
            tool.execute(json!({
                "script": "await agent(\"coder\").ask(\"挂起\"); result(\"x\");",
                "type_check": false
            }))
            .await
        });
        wait_flag(&started, "子代理进入推理").await;
        assert!(!dropped.load(Ordering::SeqCst));
        running.abort();
        let _ = running.await;
        wait_flag(&dropped, "子代理随工具 future 丢弃而取消").await;
    }

    /// 带宿主侧超时执行（挂死按失败而不是卡住测试）。
    async fn run_bounded(tool: &WorkflowRunTool, input: Value) -> Value {
        let outcome = tokio::time::timeout(Duration::from_secs(90), tool.execute(input))
            .await
            .expect("WorkflowRun 不得挂死");
        assert!(!outcome.is_error, "{}", outcome.content);
        serde_json::from_str(&outcome.content).unwrap()
    }

    #[tokio::test]
    async fn duplicate_question_in_one_run_completes_and_replays_by_call_site() {
        let (tool, calls) = counting_tool();
        let script = concat!(
            "const coder = agent(\"coder\");\n",
            "const a = await coder.ask(\"同一个问题\");\n",
            "const b = await coder.ask(\"同一个问题\");\n",
            "result(a + \"|\" + b);\n"
        );
        let first = run_bounded(&tool, json!({"script": script, "type_check": false})).await;
        assert_eq!(first["asks_total"], 2);
        assert_eq!(first["asks_replayed"], 0);
        assert_eq!(first["result"], "#1|#2", "同问两次是两个调用点，各自真实执行");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let run_id = first["run_id"].as_str().unwrap().to_string();
        let second = run_bounded(
            &tool,
            json!({"script": script, "resume_run_id": run_id, "type_check": false}),
        )
        .await;
        assert_eq!(second["asks_replayed"], 2);
        assert_eq!(second["result"], "#1|#2", "重放按调用点配对，答案不互换");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "重放零 provider 调用");
    }

    #[tokio::test]
    async fn amend_with_rewritten_earlier_ask_completes() {
        // 前驱先完成 q1（order 0）再完成 q2（order 1）；amend 改写 q1、保留 q2：
        // 导入的 q2 不经顺序门（否则等 order 0 永不到来而挂死）。
        let (tool, calls) = counting_tool();
        let original = concat!(
            "const coder = agent(\"coder\");\n",
            "const r1 = await coder.ask(\"问题一\");\n",
            "const r2 = await coder.ask(\"问题二\");\n",
            "result(r1 + \"|\" + r2);\n"
        );
        let first = run_bounded(&tool, json!({"script": original, "type_check": false})).await;
        assert_eq!(first["result"], "#1|#2");
        let predecessor = first["run_id"].as_str().unwrap().to_string();
        let amended = concat!(
            "const coder = agent(\"coder\");\n",
            "const r1 = await coder.ask(\"问题一（改）\");\n",
            "const r2 = await coder.ask(\"问题二\");\n",
            "result(r1 + \"|\" + r2);\n"
        );
        let second = run_bounded(
            &tool,
            json!({"script": amended, "amend_run_id": predecessor, "type_check": false}),
        )
        .await;
        assert_eq!(second["asks_replayed"], 1, "q2 应零调用导入");
        assert_eq!(second["imported_asks"], 2);
        assert_eq!(second["result"], "#3|#2");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
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

    #[test]
    fn lint_skips_strings_and_comments_but_catches_indirect_forms() {
        // 提示词文本里的 process. / eval / new Date / Date.now 不是代码，不得误报。
        let prompt_text = concat!(
            "const coder = agent(\"coder\");\n",
            "// Date.now() 在注释里 /* eval( */\n",
            "const r = await coder.ask(\"Review the build process. Then eval it; see `new Date()` docs\");\n",
            "const d = formatDate(1); function formatDate(x) { return x; }\n",
            "result(r + 'process.exit' + d);\n"
        );
        assert!(diagnose(prompt_text).is_empty(), "{:?}", diagnose(prompt_text));
        // 间接 eval / 函数式 Date() / Function 构造 / 字符串转义不打断遮蔽。
        let indirect = "const s = \"a\\\"b\"; const e = (0, eval)(s); const t = Date(); const f = new Function(\"x\");";
        let diagnostics = diagnose(indirect);
        assert!(diagnostics.iter().any(|d| d.message.contains("eval")), "{diagnostics:?}");
        assert!(diagnostics.iter().any(|d| d.message.contains("Date()")), "{diagnostics:?}");
        assert!(diagnostics.iter().any(|d| d.message.contains("Function")), "{diagnostics:?}");
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
    fn schema_validator_integer_type_arrays_unknown_names_and_depth() {
        let integer = json!({"type": "integer"});
        assert!(validate_json_schema(&json!(4), &integer, 0).is_ok());
        assert!(validate_json_schema(&json!(4.0), &integer, 0).is_ok());
        assert!(validate_json_schema(&json!(4.5), &integer, 0).is_err());
        let nullable = json!({"type": ["string", "null"]});
        assert!(validate_json_schema(&json!("x"), &nullable, 0).is_ok());
        assert!(validate_json_schema(&Value::Null, &nullable, 0).is_ok());
        assert!(validate_json_schema(&json!(1), &nullable, 0).is_err());
        let unknown = json!({"type": "strng"});
        assert!(validate_json_schema(&json!("x"), &unknown, 0).is_err(), "未知类型名必须报错");
        // 超过深度上限：报错而不是静默放行。
        let mut deep_schema = json!({"type": "number"});
        let mut deep_value = json!(1);
        for _ in 0..10 {
            deep_schema = json!({"type": "object", "properties": {"a": deep_schema}});
            deep_value = json!({"a": deep_value});
        }
        let error = validate_json_schema(&deep_value, &deep_schema, 0).unwrap_err();
        assert!(error.contains("深度"), "{error}");
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

#[cfg(test)]
mod validator_tests {
    use super::validate_json_schema;
    use serde_json::json;

    #[test]
    fn validator_supports_const_enum_and_anyof() {
        // const：字面量精确匹配。
        assert!(validate_json_schema(&json!("fast"), &json!({"const": "fast"}), 0).is_ok());
        assert!(validate_json_schema(&json!("slow"), &json!({"const": "fast"}), 0).is_err());
        assert!(validate_json_schema(&json!(8080), &json!({"const": 8080}), 0).is_ok());
        // enum：任一枚举值。
        let enum_schema = json!({"enum": ["fast", "slow"]});
        assert!(validate_json_schema(&json!("fast"), &enum_schema, 0).is_ok());
        assert!(validate_json_schema(&json!("mid"), &enum_schema, 0).is_err());
        // anyOf：任一分支通过。
        let union = json!({"anyOf": [{"type": "string"}, {"type": "number"}]});
        assert!(validate_json_schema(&json!("x"), &union, 0).is_ok());
        assert!(validate_json_schema(&json!(1), &union, 0).is_ok());
        assert!(validate_json_schema(&json!(true), &union, 0).is_err());
        // 编译产物的组合形态：对象内嵌枚举 + 数组元素 const。
        let compiled = json!({
            "type": "object",
            "properties": {
                "zone": {"enum": ["a", "b"]},
                "tags": {"type": "array", "items": {"const": "x"}}
            },
            "required": ["zone"]
        });
        assert!(validate_json_schema(
            &json!({"zone": "a", "tags": ["x", "x"]}),
            &compiled,
            0
        )
        .is_ok());
        assert!(validate_json_schema(&json!({"zone": "c"}), &compiled, 0).is_err());
        assert!(validate_json_schema(&json!({"zone": "a", "tags": ["y"]}), &compiled, 0).is_err());
    }
}

#[cfg(test)]
mod tuple_validator_tests {
    use super::validate_json_schema;
    use serde_json::json;

    #[test]
    fn validator_checks_tuple_positions_and_bounds() {
        let tuple = json!({
            "type": "array",
            "prefixItems": [{"type": "string"}, {"type": "number"}],
            "minItems": 2,
            "maxItems": 2
        });
        assert!(validate_json_schema(&json!(["a", 1]), &tuple, 0).is_ok());
        // 位置类型错。
        assert!(validate_json_schema(&json!([1, "a"]), &tuple, 0).is_err());
        // 长度不足 / 超长。
        assert!(validate_json_schema(&json!(["a"]), &tuple, 0).is_err());
        assert!(validate_json_schema(&json!(["a", 1, true]), &tuple, 0).is_err());
        // prefixItems 之后的元素由 items 接管（混合元组 + 变长尾）。
        let hybrid = json!({
            "type": "array",
            "prefixItems": [{"type": "string"}],
            "items": {"type": "number"}
        });
        assert!(validate_json_schema(&json!(["a", 1, 2]), &hybrid, 0).is_ok());
        assert!(validate_json_schema(&json!(["a", 1, "b"]), &hybrid, 0).is_err());
    }
}
