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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use rclaw_core::ToolAccess;

use rquickjs::prelude::Async as AsyncFn;
use rquickjs::{AsyncContext, AsyncRuntime, Context, Function, Runtime};
use serde_json::{json, Value};

use super::workflow_effects::{BeginEffect, EffectTracker};
use super::workflow_paths::{self, resolve_under, WorkflowPaths};
use super::workflow_snapshot;
use super::workflow_store::{WorkflowStore, STOP_PERMISSION_REVOKED};
use super::workflow_world;
use super::*;

/// 引擎沙箱的内存上限（QuickJS 级，防脚本失控）。
const ENGINE_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// 单片 JS CPU 预算：两次宿主回调之间连续执行 JS 的上限（中断处理器轮询；ask/askJson
/// 结算、顺序门放行时重置）。整个 run 的时长另由墙钟预算 `max_run_secs` 约束。
const ENGINE_SLICE_BUDGET: Duration = Duration::from_secs(120);
/// 整体墙钟预算：默认 1h，入参 `max_run_secs` 可调（硬顶 24h），超时按 run_failed 收尾。
pub(crate) const DEFAULT_MAX_RUN_SECS: u64 = 60 * 60;
pub(crate) const MAX_MAX_RUN_SECS: u64 = 24 * 60 * 60;
/// 取消 / 超时后给子代理优雅收尾（经 cancel token 退出）的宽限期，到期置 halt 终止 JS。
const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// 片预算中断后留给包装层 reject 回调上报的宽限：之后不再发新片，引擎在下一个 job 边界停下
/// （否则脚本 `.catch` 掉中断错误再起一条紧循环，每条都能拿到整片预算，直到墙钟耗尽）。
const INTERRUPT_GRACE: Duration = Duration::from_secs(2);
/// 置 halt 后等中断处理器在下一个轮询点终止 JS 的上限。
const HALT_GRACE: Duration = Duration::from_secs(2);
/// 并行 ask 上限（`Promise.all` 扇出的信号量许可数；default 8，硬顶 16——第十二轮
/// 4/8 → 8/16：与 PlanRun 波次上限同一轮提升，大规模并行的直接闸门；ask 的子代理
/// 模型请求仍由 `subagent_request_stagger` 错峰）。
pub(crate) const DEFAULT_MAX_PARALLEL_ASKS: u32 = 8;
pub(crate) const MAX_MAX_PARALLEL_ASKS: u32 = 16;
/// 单次 ask 的子代理工具迭代上限（对齐计划步骤的收敛口径）。
const WORKFLOW_ASK_MAX_TOOL_ITER: usize = 64;
pub(crate) const MAX_SCRIPT_CHARS: usize = 100_000;
const MAX_QUESTION_CHARS: usize = 4_000;
/// `agent(name)` 的 actor 名上限（进每条 ask/answer 事件与调用点身份；与 hole 名同口径）。
const MAX_ACTOR_CHARS: usize = 100;
/// `agent(name, {system})` 的 persona 上限（与问题一起拼进子代理 task）。
const MAX_PERSONA_CHARS: usize = 8_000;
/// `agent(name, {model})` 的模型名上限。
const MAX_MODEL_CHARS: usize = 200;
/// 脚本返回值 / 抛错文本的上限（进 run_completed 与工具结果）。
const MAX_SCRIPT_RESULT_CHARS: usize = 256 * 1024;
/// 同时执行的 effect（files / git / world / artifact）上限：单个结果最大 2MiB，一次扇出
/// 1024 个 `files.read` 会把 2GiB 内容同时压在内存里。超出的在宿主侧排队等许可（引擎停驻，
/// 不计入片预算），不是报错。
const MAX_INFLIGHT_EFFECTS: usize = 32;
const MAX_ANSWER_CHARS: usize = 4_000;
/// 宿主回调入参（files/git/world/artifact 的 JSON payload、askJson 的 schema）上限：
/// 脚本把 2e7 字符的字符串塞进 payload，宿主侧 `from_str` + clone 会先把内存撑爆。
const MAX_HOST_PAYLOAD_BYTES: usize = 1024 * 1024;
/// 同一引擎在途（已发起、未结算）的 async 宿主调用上限。rquickjs 的 `Async` 包装在 JS 调用
/// 当场构造 future 排进 spawner，要到当前 job 结束后的扫描才首次 poll：不 `await` 的 `for`
/// 循环能在一片之内排出无界的 future 队列（每个都带一份入参）。超限的调用在同步段就被拒绝、
/// 不捕获入参；入参本身到首次 poll 才拷到 Rust 堆（排队期间只持 QuickJS 句柄，受 64MiB 限额）。
const MAX_PENDING_HOST_CALLS: usize = 1_024;
/// 单次执行里 `ask` 的发起次数上限：每次发起都是一条 journal 事件（≤4K 字符的问题），
/// `log` / `phase` / effect 都有同类上限；在占调用点编号之前拒绝，不影响重放编号。
const MAX_ASKS_PER_EXEC: u64 = 10_000;

/// facade 声明（.d.ts 语义的文本形态）：脚本契约一览，随诊断一并回给模型。
pub(crate) const FACADE_DTS: &str = r#"
// workflow 脚本可用的全局 facade（确定性沙箱，无模块加载）。
// 可用：await / Promise.all / while / try-catch（脚本被包进 async IIFE）。
// 禁用：Date.now / Math.random 赋值 / import / eval / Function / require /
//       setTimeout / setInterval / process（提交前静态检查 + 沙箱双层拒绝）。

declare const args: Readonly<Record<string, unknown>>;

declare function agent(name: string, persona?: { model?: string; system?: string }): {
  ask<T = string>(question: string): Promise<T>;
  askJson<T = any>(question: string, schema: object): Promise<T>;
};

declare function log(message: string): void;
declare function phase(name: string): void;
declare function report(item: unknown): void;
declare function result(text: string): void;

declare const files: {
  glob(pattern: string): Promise<string[]>;
  read(path: string): Promise<string>;
  grep(pattern: string, glob?: string): Promise<Array<{ path: string; line: number; text: string }>>;
};

declare const git: {
  status(): Promise<{ branch?: string; clean: boolean; staged: string[]; modified: string[]; untracked: string[] }>;
  diff(path?: string): Promise<string>;
};

declare const world: {
  run(command: string, opts?: { cwd?: string; idempotency_key?: string }): Promise<{
    exit_code: number; stdout: string; stderr: string; timed_out?: boolean;
  }>;
};

interface ArtifactOptions { title?: string; description?: string; primary?: boolean; contentType?: string }
interface ArtifactRef { id: string; version: number }

declare const artifact: {
  file(id: string, path: string, opts?: ArtifactOptions): Promise<ArtifactRef>;
  markdown(id: string, content: string, opts?: ArtifactOptions): Promise<ArtifactRef>;
  table(id: string, rows: unknown, opts?: ArtifactOptions): Promise<ArtifactRef>;
  chart(id: string, spec: { type?: "bar" | "line"; title?: string; data: Array<{ label?: string; value: number }> }, opts?: ArtifactOptions): Promise<ArtifactRef>;
  metrics(id: string, metrics: unknown, opts?: ArtifactOptions): Promise<ArtifactRef>;
  board(id: string, columns: Array<{ title: string; items: unknown[] }>, opts?: ArtifactOptions): Promise<ArtifactRef>;
};

declare function hole<T = string>(name: string, prompt?: string): Promise<T>;
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
    (
        "Date.now",
        "时间必须确定：脚本内禁用 Date.now（沙箱已钉死为固定纪元）",
    ),
    ("Date.prototype", "禁用 Date.prototype（沙箱已钉死时间源）"),
    (
        "Date(",
        "时间必须确定：脚本内禁用 Date() / new Date()（沙箱已钉死为固定纪元）",
    ),
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
pub(crate) fn run_with_timeout(
    mut command: std::process::Command,
    timeout: Duration,
) -> Option<std::process::Output> {
    use std::process::Stdio;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
/// 可为 `["T", "null"]` 数组）、const、enum、anyOf、allOf、object.properties/required、
/// array.items/prefixItems/minItems/maxItems。覆盖 facade 契约场景（含 `.ask<T>` 编译出的
/// 字面量 / 联合 / 元组 / 交叉 schema，第十二轮）；超出子集的字段忽略；未知类型名与
/// 超深嵌套（> 8）按错误处理。
/// JSON Schema 语义的值相等（递归）：数值按数学值比较（serde_json 的 `Value` 相等
/// 区分整数 / 浮点表示——`1` ≠ `1.0`，而 ask<1> 的答案序列化成 `1.0` / `1e0` 应放行）；
/// 对象 / 数组逐成员递归（`{"const":{"score":1}}` 接受 `{"score":1.0}`）；整数值跨
/// 表示比较走【精确整数】路径——f64 在 2^53 以上会合并相邻整数（9007199254740992
/// 与 ...993 转换后相等，const / enum 校验被静默绕过）；其余标量退回结构相等。
fn json_schema_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .get(key)
                        .is_some_and(|other| json_schema_equal(value, other))
                })
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left.iter().zip(right).all(|(m, n)| json_schema_equal(m, n))
        }
        _ => scalar_numeric_equal(a, b),
    }
}

/// 标量的 JSON Schema 数值相等（非数值退回结构相等）。整数值一律转 i128 精确比较
/// ——整数值的浮点（如 9007199254740992.0）同样精确转换，混合表示的比较绝不先做
/// f64 舍入（const 9007199254740993 曾接受答案 9007199254740992.0：常量转 f64 被
/// 舍入成 ...992，两个不同的数学值被判相等）。超出 i128 表示范围的大数（如 1e100）
/// 退回 f64 比较——生产端只有浮点精度可言。
fn scalar_numeric_equal(a: &Value, b: &Value) -> bool {
    if let (Some(left), Some(right)) = (a.as_i64(), b.as_i64()) {
        return left == right;
    }
    if a.is_number() && b.is_number() {
        let (Some(left), Some(right)) = (a.as_f64(), b.as_f64()) else {
            return a == b;
        };
        if left.fract() != 0.0 || right.fract() != 0.0 {
            return left == right;
        }
        // 两个都是整数值：精确整数比较（f64 整数值在 |f| < 2^126 内 as i128 精确；
        // 饱和转换会被上界检查挡住 → None）。
        let exact = |value: &Value| -> Option<i128> {
            if let Some(i) = value.as_i64() {
                return Some(i128::from(i));
            }
            if let Some(u) = value.as_u64() {
                return Some(i128::from(u));
            }
            if let Some(f) = value.as_f64() {
                if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e36 {
                    return Some(f as i128);
                }
            }
            None
        };
        if let (Some(left), Some(right)) = (exact(a), exact(b)) {
            return left == right;
        }
        return left == right;
    }
    a == b
}

fn validate_json_schema(value: &Value, schema: &Value, depth: usize) -> Result<(), String> {
    if depth > 8 {
        return Err("schema 嵌套超过深度上限 8".to_string());
    }
    // allOf：每个分支都必须通过（交叉类型的属性级交集走这里）。
    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for (index, branch) in branches.iter().enumerate() {
            validate_json_schema(value, branch, depth + 1)
                .map_err(|error| format!("allOf[{index}]: {error}"))?;
        }
    }
    // anyOf：任一分支通过即可。命中后【继续】走本层的 type / required / properties
    // 等同级约束——显式 askJson 的 schema 可以把 anyOf 与其它约束组合（编译产物是
    // 纯 anyOf 不受影响），提前 return 会把 `{"type":"object","required":["host"],
    // "anyOf":[...]}` 的 required 整个跳掉。
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        let mut failures = Vec::new();
        let mut matched = false;
        for variant in variants {
            match validate_json_schema(value, variant, depth + 1) {
                Ok(()) => {
                    matched = true;
                    break;
                }
                Err(error) => failures.push(error),
            }
        }
        if !matched {
            return Err(format!("不满足任一联合分支：{}", failures.join("; ")));
        }
    }
    if let Some(expected_const) = schema.get("const") {
        if !json_schema_equal(value, expected_const) {
            return Err(format!("常量不符：期望 {expected_const}，实际 {value}"));
        }
    }
    if let Some(variants) = schema.get("enum").and_then(Value::as_array) {
        if !variants
            .iter()
            .any(|variant| json_schema_equal(variant, value))
        {
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
            return Err(format!(
                "类型不符：期望 {}，实际 {value}",
                expected.join("|")
            ));
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
            for (index, (item, subschema)) in array.iter().zip(prefix_items.iter()).enumerate() {
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
                return Err(format!(
                    "数组长度不足：期望 ≥ {min_items}，实际 {}",
                    array.len()
                ));
            }
        }
    }
    if let Some(max_items) = schema.get("maxItems").and_then(Value::as_u64) {
        if let Some(array) = value.as_array() {
            if array.len() > max_items as usize {
                return Err(format!(
                    "数组超长：期望 ≤ {max_items}，实际 {}",
                    array.len()
                ));
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
    /// 源字节偏移（作者脚本内，0 起；文本降级路径不产 offset 恒 0）。组排序用它——
    /// 单行脚本里「串行 ask / Promise.all 波 / 串行 ask」行号相同，按行排序会打平，
    /// 插入序（join 组先发现）就把并行波排到前一个串行 ask 之前了。
    #[serde(skip)]
    pub(crate) offset: usize,
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
    /// 节点 / 边超过上限被截断（相邻两波全连是 |prev|×|next| 条边，100k 字符的脚本能
    /// 造出上亿条；洞察随 run_started 入 journal，必须有界）。截断后只保留前若干节点、不连边。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) truncated: bool,
}

/// 依赖图节点 / 边上限（观测用洞察，超限截断而不是拒绝脚本）。
pub(crate) const MAX_GRAPH_NODES: usize = 512;
pub(crate) const MAX_GRAPH_EDGES: usize = 8_192;

impl DependencyGraph {
    /// 节点超限：只保留前 `MAX_GRAPH_NODES` 个节点，不连边、不分层，标记截断。
    pub(crate) fn truncated(mut nodes: Vec<DependencyNode>) -> Self {
        nodes.truncate(MAX_GRAPH_NODES);
        DependencyGraph {
            nodes,
            edges: Vec::new(),
            waves: Vec::new(),
            truncated: true,
        }
    }
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
            wave_depth +=
                depth_scope.matches('(').count() as i32 - depth_scope.matches(')').count() as i32;
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

    if nodes.len() > MAX_GRAPH_NODES {
        return DependencyGraph::truncated(nodes);
    }
    // 边：相邻 ask/wave 之间按行序连边（前一波每个出口 → 后一波每个入口）。
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut truncated = false;
    let mut last_boundary: Vec<String> = Vec::new(); // 上一个「波/节点」的全部出口
    let mut i = 0usize;
    'edges: while i < nodes.len() {
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
                if edges.len() >= MAX_GRAPH_EDGES {
                    truncated = true;
                    break 'edges;
                }
                edges.push((prev.clone(), nodes[member].id.clone()));
            }
        }
        last_boundary = wave
            .iter()
            .map(|&member| nodes[member].id.clone())
            .collect();
        i = wave.last().copied().unwrap_or(i) + 1;
    }

    // 波次：Kahn 分层（入度 0 → 层 1，依边推进）。
    let waves = topo_waves(&nodes, &edges);

    DependencyGraph {
        nodes,
        edges,
        waves,
        truncated,
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
fn asks_on_line(line: &str, bindings: &[(String, String)], line_no: usize) -> Vec<DependencyNode> {
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
            offset: 0,
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
pub(crate) fn append_workflow_event(journal: &EventLog, event: &Value) {
    journal.append_lossy(event);
}

/// 重放表条目：答案 + 原完成序（journal 中首见该调用点答案的全局位次，0 起；
/// amend 导入不经顺序门 → None）。unresolved_disclosure 是答案子代理仍未解决的
/// 副作用工具错误的运行时披露块——与答案一起原样重放，resume 不丢披露。
#[derive(Debug, Clone)]
pub(crate) struct ReplayAnswer {
    pub(crate) answer: String,
    pub(crate) order: Option<u64>,
    pub(crate) unresolved_disclosure: String,
}

/// 调用点键：(actor, inputHash, 本 run 内同键第几次提问)。
type ReplayKey = (String, u64, u32);

/// journal 里的问答重放表：调用点 → 答案。
pub(crate) type ReplayTable = HashMap<ReplayKey, ReplayAnswer>;

/// 只收每个调用点**首见**的答案（order 跨代稳定：重放一代按门序再发一遍，取首次）。
fn record_answer(
    table: &mut ReplayTable,
    order_next: &mut u64,
    key: ReplayKey,
    answer: &str,
    unresolved_disclosure: &str,
) {
    if table.contains_key(&key) {
        return;
    }
    table.insert(
        key,
        ReplayAnswer {
            answer: answer.to_string(),
            order: Some(*order_next),
            unresolved_disclosure: unresolved_disclosure.to_string(),
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
pub(crate) fn load_workflow_journal(journal: &EventLog) -> Result<(u64, ReplayTable), String> {
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
                    // 取【最新】一代的 hash：hole fill / resume 后 run_started 的旧 hash
                    // 不应覆盖当前快照。
                    script_hash = Some(hash);
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
                    let disclosure = event
                        .get("unresolved_disclosure")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    record_answer(&mut table, &mut order_next, key, answer, disclosure);
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
                let disclosure = event
                    .get("unresolved_disclosure")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                record_answer(&mut table, &mut order_next, key, answer, disclosure);
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
    // 门的状态与函数封在闭包里、只以不可写不可配置的全局暴露：宿主每轮静止后会 eval
    // `__gate_open()` 决定是否再驱动一轮，脚本若能改写它（或 __orderWaiters）就能让宿主
    // 无限放行。
    (function () {
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
        Object.defineProperty(globalThis, "__gate_wait",
            { value: __gate_wait, writable: false, configurable: false, enumerable: false });
        Object.defineProperty(globalThis, "__gate_open",
            { value: __gate_open, writable: false, configurable: false, enumerable: false });
    })();
    // facade：宿主 envelope 结算（错误在 JS 侧抛出；重放命中且带 order 的经顺序门）。
    // 失败信封带 replayed/order 时同样先进顺序门再抛：重放代里慢调用的失败不得绕过
    // 顺序门抢先拒绝，否则 Promise.race 的赢家会随代次漂移（codex 第 6 轮 P1）。
    function __settle(raw, label) {
        var parsed = JSON.parse(raw);
        if (!parsed.ok) {
            var fail = function () { throw new Error(parsed.error || (label + " 失败")); };
            if (parsed.replayed && typeof parsed.order === "number") {
                return __gate_wait(parsed.order).then(fail);
            }
            return fail();
        }
        if (parsed.replayed && typeof parsed.order === "number") {
            return __gate_wait(parsed.order).then(function () { return parsed.answer; });
        }
        return parsed.answer;
    }
    // 通用信封结算（files/git/world/artifact/hole）：replay 不进顺序门。
    function __settle_value(raw, label) {
        var parsed = JSON.parse(raw);
        if (!parsed.ok) { throw new Error(parsed.error || (label + " 失败")); }
        return parsed.value;
    }
    function agent(name, persona) {
        name = String(name);
        if (!name) { throw new Error("agent() 需要 actor 名"); }
        var model = null;
        var system = null;
        if (persona != null) {
            if (typeof persona !== "object") {
                throw new Error("persona_unsupported: agent 的 persona 只接受 {model, system}");
            }
            if (persona.model != null) { model = String(persona.model); }
            if (persona.system != null) { system = String(persona.system); }
        }
        return {
            name: name,
            ask: function (question) {
                return __ask_raw(name, String(question), model, system).then(function (raw) {
                    return __settle(raw, "ask");
                });
            },
            askJson: function (question, schema) {
                return __ask_json_raw(name, String(question), JSON.stringify(schema), model, system).then(function (raw) {
                    return __settle(raw, "askJson");
                });
            }
        };
    }
    function log(message) { __wf_log(String(message)); }
    function report(item) { __wf_report(JSON.stringify(item === undefined ? null : item)); }
    function phase(name) { __wf_phase(String(name)); }
    function hole(name, prompt, site) {
        // 同步宿主：登记 hole 后返回错误信封 → 当前 async 链在这里中止（引擎把
        // shared.hole 存在解释为 HoleWaiting，而不是失败）。tail / 分支级 hole 同一路径。
        var raw = __wf_hole(
            String(name),
            prompt == null ? "" : String(prompt),
            site == null ? null : Number(site)
        );
        var parsed = JSON.parse(raw);
        if (!parsed.ok) { throw new Error(parsed.error || "hole 等待填充"); }
        return parsed.value;
    }
    var files = {
        glob: function (pattern) {
            return __wf_files("glob", JSON.stringify({pattern: String(pattern)})).then(function (raw) {
                return __settle_value(raw, "files.glob");
            });
        },
        read: function (path) {
            return __wf_files("read", JSON.stringify({path: String(path)})).then(function (raw) {
                return __settle_value(raw, "files.read");
            });
        },
        grep: function (pattern, glob) {
            return __wf_files("grep", JSON.stringify({pattern: String(pattern), glob: glob == null ? null : String(glob)})).then(function (raw) {
                return __settle_value(raw, "files.grep");
            });
        }
    };
    var git = {
        status: function () {
            return __wf_git("status", "{}").then(function (raw) {
                return __settle_value(raw, "git.status");
            });
        },
        diff: function (path) {
            return __wf_git("diff", JSON.stringify({path: path == null ? null : String(path)})).then(function (raw) {
                return __settle_value(raw, "git.diff");
            });
        }
    };
    var world = {
        run: function (command, opts) {
            return __wf_world(JSON.stringify({command: String(command), opts: opts || {}})).then(function (raw) {
                return __settle_value(raw, "world.run");
            });
        }
    };
    var artifact = {
        file: function (id, path, opts) {
            return __wf_artifact("file", JSON.stringify({id: String(id), path: String(path), opts: opts || {}})).then(function (raw) {
                return __settle_value(raw, "artifact.file");
            });
        },
        markdown: function (id, content, opts) {
            return __wf_artifact("markdown", JSON.stringify({id: String(id), content: String(content), opts: opts || {}})).then(function (raw) {
                return __settle_value(raw, "artifact.markdown");
            });
        },
        table: function (id, rows, opts) {
            return __wf_artifact("table", JSON.stringify({id: String(id), rows: rows, opts: opts || {}})).then(function (raw) {
                return __settle_value(raw, "artifact.table");
            });
        },
        chart: function (id, spec, opts) {
            return __wf_artifact("chart", JSON.stringify({id: String(id), spec: spec, opts: opts || {}})).then(function (raw) {
                return __settle_value(raw, "artifact.chart");
            });
        },
        metrics: function (id, metrics, opts) {
            return __wf_artifact("metrics", JSON.stringify({id: String(id), metrics: metrics, opts: opts || {}})).then(function (raw) {
                return __settle_value(raw, "artifact.metrics");
            });
        },
        board: function (id, columns, opts) {
            return __wf_artifact("board", JSON.stringify({id: String(id), columns: columns, opts: opts || {}})).then(function (raw) {
                return __settle_value(raw, "artifact.board");
            });
        }
    };
    function result(text) { __result(String(text)); }
    // args 在 register_hosts 阶段以 JSON.parse 注入并冻结（见 eval_deterministic）。
"#;

/// 单片 JS CPU 预算（中断处理器轮询）：宿主 future 结算 / 顺序门放行时 `reset`，JS 续体获得
/// 新的一片；超预算时记录中断并只留 `INTERRUPT_GRACE`（让包装层的 reject 回调能跑完上报），
/// 返回 true 终止当前片。中断是粘滞的：之后 `reset` 不再发新片，宽限一过第二次轮询再次
/// 返回 true，`drive_to_quiescence` 据此停下——run 结果此时已判失败，没必要再烧 CPU。
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
        // 已中断：不再发新片（宿主 future 结算 / 顺序门放行也只剩宽限期）。
        if self.interrupted() {
            return;
        }
        self.deadline_ms.store(
            self.elapsed_ms() + self.slice.as_millis() as u64,
            Ordering::Relaxed,
        );
    }

    fn poll_interrupt(&self) -> bool {
        if self.elapsed_ms() < self.deadline_ms.load(Ordering::Relaxed) {
            return false;
        }
        if !self.interrupted.swap(true, Ordering::Relaxed) {
            self.deadline_ms.store(
                self.elapsed_ms() + INTERRUPT_GRACE.as_millis() as u64,
                Ordering::Relaxed,
            );
        }
        // 宽限也用完：deadline 不再后推，之后每次轮询都返回 true——JS 再也跑不起来。
        true
    }

    fn interrupted(&self) -> bool {
        self.interrupted.load(Ordering::Relaxed)
    }
}

/// 第一次 poll 返回 Pending、再被唤醒即就绪：用于"等宿主 future 的任一唤醒"——它们在上一次
/// `execute_pending_job` 的 poll 里已把本任务的 waker 登记进了调度器，这里只需让出等被叫醒。
#[derive(Default)]
struct WakeOnce {
    polled: bool,
}

impl std::future::Future for WakeOnce {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if std::mem::replace(&mut self.polled, true) {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }
}

/// 把 JS job 队列与宿主 future 驱动到静止，替代 `AsyncRuntime::idle`：idle 在同一次 poll 里
/// 把 job 队列一直跑到空、中途不让出，而 QuickJS 的中断处理器只在循环回边 / 构造调用处轮询——
/// 一条不含循环、自我重排的 microtask 链（`then(loop)` 递归、`await` 递归）既不会触发中断
/// 处理器，也不会让 idle 返回：墙钟预算、取消令牌乃至外层硬超时后的收尾全部失效，阻塞线程
/// 被永久占满。这里逐个 job 驱动，job 之间按同一份片预算检查；没有 job 时等宿主 future 唤醒。
async fn drive_to_quiescence(runtime: &AsyncRuntime, budget: &SliceBudget, halt: &AtomicBool) {
    // 连续有进展（job / 立即结算的宿主 future）时周期性让出：否则这个 future 永不返回
    // Pending，外层 drive_engine 的 timeout / select!（max_run、取消宽限）得不到轮询。
    const YIELD_EVERY: u32 = 64;
    let mut since_yield: u32 = 0;
    loop {
        if halt.load(Ordering::SeqCst) {
            return;
        }
        match runtime.execute_pending_job().await {
            // 跑了一个 job（或宿主 future 有进展）：超出片预算即中断，与中断处理器同一判定。
            // job 抛出未捕获异常：与 idle 一致，丢弃后继续驱动（脚本主链的异常由 IIFE 的
            // reject 分支经 __script_done 上报，不会丢）。
            Ok(true) | Err(_) => {
                if budget.poll_interrupt() {
                    return;
                }
                since_yield += 1;
                if since_yield >= YIELD_EVERY {
                    since_yield = 0;
                    tokio::task::yield_now().await;
                }
            }
            Ok(false) => {
                // 没有可跑的 job 也没有待决的宿主 future → 引擎真正静止。
                if !runtime.is_job_pending().await {
                    return;
                }
                WakeOnce::default().await;
                // 从空闲被叫醒：期间没有 JS 在跑（在等 ask 许可 / world.run / files 这类宿主
                // future），这段墙钟不能算进 JS 片预算——否则并行 ask 超过 max_parallel_asks
                // 时排队 >120s、或一条命令跑 >120s，都会被当成“CPU 预算耗尽”把整个 run 判失败。
                // 自我重排的 microtask 链永远到不了这里，仍会被片预算中断。
                budget.reset();
            }
        }
    }
}

/// 脚本结算形态：async IIFE 正常返回（`undefined` → None，字符串原样，其余 JSON 化）或抛错。
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
    halt: Arc<AtomicBool>,
    args: Value,
    register_hosts: impl FnOnce(&rquickjs::Ctx) + Send + 'static,
) -> Result<Option<ScriptOutcome>, String> {
    let runtime =
        AsyncRuntime::new().map_err(|error| format!("QuickJS 运行时创建失败: {error}"))?;
    runtime.set_memory_limit(ENGINE_MEMORY_LIMIT).await;
    runtime.set_max_stack_size(1024 * 1024).await;
    let interrupt_budget = Arc::clone(&budget);
    let interrupt_halt = Arc::clone(&halt);
    // halt（取消 / 超时宽限期已过）优先：紧循环在下一个轮询点就被终止，不必等整片预算耗尽。
    runtime
        .set_interrupt_handler(Some(Box::new(move || {
            interrupt_halt.load(Ordering::SeqCst) || interrupt_budget.poll_interrupt()
        })))
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
            // args：保存/路径参数，冻结后脚本只读；无参数时为 {}。
            let args_json = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
            let args_literal = serde_json::to_string(&args_json).unwrap_or_else(|_| "\"{}\"".to_string());
            ctx.eval::<(), _>(format!(
                "globalThis.args = Object.freeze(JSON.parse({args_literal}));"
            ))
            .map_err(|error| format!("args 注入失败: {}", js_error(&ctx, error)))?;
            register_hosts(&ctx);
            let done_fn = Function::new(
                ctx.clone(),
                move |ok: bool, text: Option<String>| -> String {
                    // 返回值 / 抛错文本进 run_completed（journal + DB）并回给模型：与 Eval 的
                    // result 同一 256K 字符上限，`return "x".repeat(6e7)` 不能一次写进 60MB。
                    let text = text.map(|text| truncate_chars(&text, MAX_SCRIPT_RESULT_CHARS));
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
            // 返回值字符串化：字符串原样，其余走 JSON（ZCode 把顶层返回值按 JSON 值落库；
            // `return {ok: true}` 不能变成 "[object Object]"），JSON 化不了的再退回 String()。
            let wrapped = format!(
                "(async () => {{\n{script}\n}})().then(\n\
                 function (value) {{\n\
                   if (value === undefined) {{ __script_done(true, undefined); return; }}\n\
                   var text;\n\
                   if (typeof value === 'string') {{ text = value; }}\n\
                   else {{ try {{ text = JSON.stringify(value); }} catch (e) {{ text = undefined; }} }}\n\
                   __script_done(true, text === undefined ? String(value) : text);\n\
                 }},\n\
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
    // 放行轮数有上限、且不刷新片预算：放行是即时的、没有等待任何宿主 future，脚本若用
    // `__gate_wait(i++)` 自续等待者让每轮都"有放行"，只会在这里耗尽上限 / 片预算后结束，
    // 不能把阻塞线程变成无预算的自旋。
    const MAX_GATE_RELEASE_ROUNDS: u32 = 4_096;
    let mut release_rounds: u32 = 0;
    loop {
        drive_to_quiescence(&runtime, &budget, &halt).await;
        let is_settled = settled
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_some();
        if is_settled
            || budget.interrupted()
            || halt.load(Ordering::SeqCst)
            || release_rounds >= MAX_GATE_RELEASE_ROUNDS
        {
            break;
        }
        release_rounds += 1;
        let released = context
            .async_with(|ctx| {
                Box::pin(async move { ctx.eval::<bool, _>("__gate_open()").unwrap_or(false) })
            })
            .await;
        if !released {
            break;
        }
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
    model: Option<String>,
    persona: Option<String>,
) -> String {
    if question.is_empty() {
        return json!({"ok": false, "error": "ask 的问题为空"}).to_string();
    }
    // actor 名进每条 ask / answer / ask_replayed 事件与调用点身份：`agent("x".repeat(1e6))`
    // 会让 journal 每条事件都带上整段名字，与 hole 名同一上限。
    if actor.chars().count() > MAX_ACTOR_CHARS {
        return json!({
            "ok": false,
            "error": format!("agent 的 actor 名超过 {MAX_ACTOR_CHARS} 字符"),
        })
        .to_string();
    }
    // 与计划步骤的 MAX_STEP_TASK_CHARS 同口径：问题全文会成为子代理的 spec.task，并随每次
    // 执行状态变更整份克隆进 exec_log 事件（内存 + SQLite）。脚本里一句
    // `ask("x".repeat(2e7))` 循环几次就能把 APK 进程撑爆；journal 的 ask 事件只是截断记录，
    // 挡不住这条路，这里直接拒绝超长问题。
    if question.chars().count() > MAX_QUESTION_CHARS {
        return json!({
            "ok": false,
            "error": format!("ask 的问题超过 {MAX_QUESTION_CHARS} 字符；请把材料写入文件后让子代理自行读取"),
        })
        .to_string();
    }
    // persona.system 与问题一起进子代理的 task：同样受长度约束，否则 `agent(n, {system:
    // "x".repeat(2e7)})` 绕过上面的问题上限。
    if persona
        .as_deref()
        .is_some_and(|text| text.chars().count() > MAX_PERSONA_CHARS)
    {
        return json!({
            "ok": false,
            "error": format!("agent persona.system 超过 {MAX_PERSONA_CHARS} 字符"),
        })
        .to_string();
    }
    // 片预算：本函数的任何快速失败 / 重放命中路径都不 reset——预算只在引擎真正停驻等宿主
    // future 被唤醒时（drive_to_quiescence）刷新。否则 `while (true) { try { await
    // agent("a", {model: "x"}).ask("q") } catch (e) {} }` 每圈都刷新一片，120s CPU 预算形同虚设。
    //
    // 取消后（父 turn /cancel、工具调用被放弃、墙钟超时）引擎还会在宽限期内跑一会儿：
    // 脚本若 try/catch 住失败的 ask 再补问，这里直接拒绝——不占调用点编号、不写 ask 事件、
    // 不登记转瞬即被取消的子代理执行。
    if shared.cancel.is_cancelled() {
        return json!({"ok": false, "error": "workflow 已取消"}).to_string();
    }
    // §3.7 运行中权限变化：每次派发 ask 前重查发起会话此刻的权限，撤权立即停止派发。
    if let Err(error) = check_session_permission(shared) {
        return json!({"ok": false, "error": error}).to_string();
    }
    // 模型名进错误文案 / allowed_models 查找：`agent(n, {model: "x".repeat(2e7)})` 同样要在
    // 这里拦住。
    if model
        .as_deref()
        .is_some_and(|model| model.chars().count() > MAX_MODEL_CHARS)
    {
        return json!({
            "ok": false,
            "error": format!("agent 的 model 名超过 {MAX_MODEL_CHARS} 字符"),
        })
        .to_string();
    }
    // 模型优先级：agent(name,{model}) > execution_config.subagent_model > 会话模型。
    // 三者都必须落在 allowed_models 内（retune 只增不减）。在写 ask 事件 / 占调用点编号之前
    // 校验：不合法的模型名循环补问不能把 journal 刷成 GB 级的无答案 ask 事件。
    let model = model.or_else(|| {
        shared
            .subagent_model
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    });
    if let Some(model) = &model {
        let allowed = shared
            .allowed_models
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !allowed.contains(model) {
            return json!({
                "ok": false,
                "error": format!("model_not_allowed: '{model}' 不在 execution_config.allowed_models 内"),
            })
            .to_string();
        }
    }
    let input_hash = fnv1a64(&format!("{actor}\u{0}{question}"));
    let hash_hex = format!("{input_hash:016x}");
    let ask_occurrence: u32;
    let ask_seq = {
        let mut engine = shared
            .engine
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // 单次执行的发起次数上限：在占调用点编号 / 写 ask 事件之前拒绝（见 MAX_ASKS_PER_EXEC）。
        if engine.asks_total >= MAX_ASKS_PER_EXEC {
            return json!({
                "ok": false,
                "error": format!("ask 超过单次执行 {MAX_ASKS_PER_EXEC} 次上限"),
            })
            .to_string();
        }
        engine.asks_total += 1;
        shared.progress.asks_total.fetch_add(1, Ordering::SeqCst);
        // 调用点身份 = (actor, inputHash, 本 run 内第几次)：同一问题问两次是两个调用点，
        // 重放按它精确配对；本 run 内不做去重（ZCode 亦无——缓存答案没有可认领的门序）。
        let occurrence = engine
            .occurrences
            .entry((actor.clone(), input_hash))
            .or_insert(0);
        let index = *occurrence;
        *occurrence += 1;
        ask_occurrence = index;
        if let Some(replayed) = engine.replay.remove(&(actor.clone(), input_hash, index)) {
            // 答案一并落本 run 的 journal：再次 resume 时本 journal 自足（不必追 amended_from）。
            // 重放也是调用点，装载器按它给后续同 (actor, inputHash) 编号：写不进去就按失败交给
            // 脚本（与下面的 ask 同口径），不能让脚本拿着答案继续、journal 里却少一个调用点。
            if let Err(error) = shared.journal.append(&json!({
                "event": "ask_replayed",
                "actor": actor,
                "input_hash": hash_hex,
                "answer": replayed.answer,
                "unresolved_disclosure": replayed.unresolved_disclosure,
            })) {
                return json!({
                    "ok": false,
                    "error": truncate_chars(&format!("ask 未能写入 journal：{error}"), 500),
                })
                .to_string();
            }
            engine.asks_replayed += 1;
            shared.progress.asks_done.fetch_add(1, Ordering::SeqCst);
            // 重放路径同样累计（resume 代的披露重新进 run_completed 事件与投影）。
            if !replayed.unresolved_disclosure.is_empty() {
                let mut disclosures = shared
                    .unresolved_disclosures
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if !disclosures.contains(&replayed.unresolved_disclosure) {
                    disclosures.push(replayed.unresolved_disclosure.clone());
                }
            }
            // set-order：order = 原完成序（amend 导入为 null → 不经门）。JS 侧顺序门按它
            // 延迟 resolve，使 Promise.race / 交错 await 的解析顺序与原完成顺序一致。
            return json!({
                "ok": true,
                "answer": replayed.answer,
                "unresolved_disclosure": replayed.unresolved_disclosure,
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
            return json!({"ok": false, "error": "workflow 已取消"}).to_string();
        }
        permit = shared.permits.acquire(&shared.cancel) => match permit {
            Some(permit) => permit,
            // acquire 在轮询间隙看到取消先于上面的分支返回 None：同样按取消处理，不 panic。
            None => {
                return json!({"ok": false, "error": "workflow 已取消"}).to_string();
            }
        },
    };
    let question = match persona.filter(|text| !text.trim().is_empty()) {
        Some(persona) => format!("[persona]\n{persona}\n\n[task]\n{question}"),
        None => question,
    };
    let answer = run_workflow_ask(
        runtime,
        shared,
        &actor,
        &question,
        model.as_deref(),
        ask_occurrence,
        input_hash,
    )
    .await;
    // 子代理执行期间引擎停驻在 drive_to_quiescence 的 WakeOnce 上，被唤醒时那里已刷新片预算；
    // 这里不再 reset（见函数开头）。
    shared.progress.asks_done.fetch_add(1, Ordering::SeqCst);
    match answer {
        Ok((answer, unresolved_disclosure)) => {
            // 披露在宿主侧统一累计（先于 askJson 的 JSON/schema 校验）：校验失败被
            // 脚本 catch 后 run_completed 仍带披露（codex 第 5 轮 P1）。
            if !unresolved_disclosure.is_empty() {
                let mut disclosures = shared
                    .unresolved_disclosures
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if !disclosures.contains(&unresolved_disclosure) {
                    disclosures.push(unresolved_disclosure.clone());
                }
            }
            append_workflow_event(
                &shared.journal,
                &json!({
                    "event": "answer",
                    "ask_seq": ask_seq,
                    "actor": actor,
                    "input_hash": hash_hex,
                    "answer": truncate_chars(&answer, MAX_ANSWER_CHARS),
                    // 披露独立字段随 journal 落盘：resume 重放时原样还原（见重放信封）。
                    "unresolved_disclosure": unresolved_disclosure,
                }),
            );
            json!({
                "ok": true,
                "answer": answer,
                "unresolved_disclosure": unresolved_disclosure,
                "replayed": false
            })
            .to_string()
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
    model: Option<&str>,
    ask_occurrence: u32,
    input_hash: u64,
) -> Result<(String, String), String> {
    let _active_ask = ActiveAskGuard::new(Arc::clone(&shared.progress));
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
        model: model.map(str::to_string),
        plan_id: None,
        step_id: None,
        // 固定 shared_cas：candidate_patch 模式下子代理被禁止改共享工作区、只把补丁放进
        // handoff.artifacts，而 ask 只把 summary 交还脚本——"看起来像编码任务"的 ask 会拿到
        // 一句"已完成"却什么都没落盘。zcode 的 workflow 子代理同样直接在工作区干活；写操作
        // 的审计归 run 自身的 effects / 世界边界。
        isolation: "shared_cas".to_string(),
        max_tool_iter: WORKFLOW_ASK_MAX_TOOL_ITER,
        max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
    };
    let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<AgentTaskResult>(1);
    let done = CancellationToken::new();
    // 本 ask 自己的取消令牌（run 令牌的子令牌）：run 取消它随之取消；本 future 被丢弃
    // （引擎片预算中断 / 硬超时后放弃 / 运行时拆除）时由 drop guard 触发——子代理不能脱离
    // workflow 独立续跑：答案没人接收，还可能重复副作用。结果正常回来后 disarm，不误伤。
    let ask_cancel = shared.cancel.child_token();
    let ask_cancel_guard = ask_cancel.clone().drop_guard();
    // 子代理的后台执行与取消 watcher 都要落在外层 runtime 上（见 EngineShared::outer），
    // 并带上发起方 turn 的 task-local 上下文；结果经 mpsc 跨 runtime 取回。
    let launch = {
        let runtime = runtime.clone();
        let description = format!("workflow {run_id} · {actor}");
        let linkage = Some((ask_cancel.clone(), done.clone()));
        let mut inherited = shared.inherited.clone();
        inherited.workflow = Some(Arc::new(super::workflow_service::WorkflowAskContext {
            run_id: run_id.clone(),
            exec_generation: shared.exec_generation,
            actor: actor.to_string(),
            ask_identity: format!("{actor}:{input_hash:016x}:{ask_occurrence}"),
            store: shared.store.clone(),
            journal: shared.journal.clone(),
            cancel: shared.cancel.clone(),
            runtime: runtime.clone(),
            effects: Arc::clone(&shared.effects),
            next_seq: Arc::clone(&shared.question_seq),
            question_occurrence: Arc::new(AtomicU64::new(0)),
        }));
        shared.outer.spawn(inherited.run(async move {
            runtime
                .launch_background_task_impl(spec, description, Some(result_tx), linkage)
                .await
        }))
    };
    launch
        .await
        .map_err(|error| format!("子代理启动任务失败: {error}"))??;
    let result = result_rx.recv().await;
    // 结果已回：解除取消联动，watcher 退出（不泄漏）；本 ask 的令牌不再需要触发。
    done.cancel();
    let _ = ask_cancel_guard.disarm();
    let Some(result) = result else {
        return Err("子代理执行通道关闭，无结果返回".to_string());
    };
    if result.is_error || result.cancelled {
        return Err(truncate_chars(&result.content, 500));
    }
    let summary = result.handoff.summary;
    let answer = if summary.trim().is_empty() {
        truncate_chars(&result.content, MAX_ANSWER_CHARS)
    } else {
        truncate_chars(&summary, MAX_ANSWER_CHARS)
    };
    // 披露独立于答案返回：askJson 要把答案再过 JSON 解析，披露混进答案文本会让
    // 合法 JSON 答案解析失败；经信封独立字段承载（codex 第 3 轮 P1）。
    Ok((answer, result.unresolved_disclosure))
}

/// askJson 的宿主回调：答案 JSON 解析 + schema（子集）校验后返回对象。
/// 校验失败的答案按错误处理（重新 ask 或改写问题由脚本决定），不静默放行。
async fn ask_json_host(
    runtime: &AgentToolRuntime,
    shared: &EngineShared,
    actor: String,
    question: String,
    schema: Value,
    model: Option<String>,
    persona: Option<String>,
) -> String {
    let raw = ask_host(runtime, shared, actor, question, model, persona).await;
    let envelope: Value = match serde_json::from_str(&raw) {
        Ok(envelope) => envelope,
        Err(_) => return raw,
    };
    if envelope.get("ok").and_then(Value::as_bool) != Some(true) {
        return raw;
    }
    // 披露经信封独立字段透传：答案本身要过 JSON 解析，披露混进答案文本会把合法
    // JSON 答案误判为非法（codex 第 3 轮 P1）。
    let unresolved_disclosure = envelope
        .get("unresolved_disclosure")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let answer = envelope
        .get("answer")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // 校验失败信封保留 replayed/order：__settle 在顺序门放行后才抛错——否则重放代里
    // 先到的非法答案会绕过顺序门抢先拒绝，改变脚本的 race 分支（codex 第 6 轮 P1，
    // 既有缺陷顺带修复）。
    let order_gate = json!({
        "replayed": envelope["replayed"],
        "order": envelope["order"],
    });
    let parsed: Value = match serde_json::from_str(&answer) {
        Ok(parsed) => parsed,
        Err(error) => {
            return json!({
                "ok": false,
                "error": format!("askJson 答案不是合法 JSON: {error}；请重新 ask 并要求子代理只输出 JSON"),
                "replayed": order_gate["replayed"],
                "order": order_gate["order"],
            })
            .to_string()
        }
    };
    if let Err(error) = validate_json_schema(&parsed, &schema, 0) {
        return json!({
            "ok": false,
            "error": format!("askJson 答案未通过 schema 校验: {error}"),
            "replayed": order_gate["replayed"],
            "order": order_gate["order"],
        })
        .to_string();
    }
    // order 原样透传：重放命中的 askJson 同样经顺序门（否则会错认领 order 0）。
    json!({
        "ok": true,
        "answer": parsed,
        "unresolved_disclosure": unresolved_disclosure,
        "replayed": envelope["replayed"],
        "order": envelope["order"],
    })
    .to_string()
}

/// 运行中 retune 句柄（模式 A）：driver 注册到 service，Amend 直接改共享原子。
#[derive(Clone)]
pub(crate) struct RetuneHandle {
    permits: Arc<DynamicSemaphore>,
    subagent_model: Arc<StdMutex<Option<String>>>,
    allowed_models: Arc<StdMutex<HashSet<String>>>,
}

impl RetuneHandle {
    pub(crate) fn apply(
        &self,
        max_parallel_asks: Option<usize>,
        subagent_model: Option<Option<String>>,
        add_model: Option<&str>,
    ) {
        if let Some(limit) = max_parallel_asks {
            self.permits
                .set_limit(limit.clamp(1, MAX_MAX_PARALLEL_ASKS as usize));
        }
        if let Some(model) = subagent_model {
            *self
                .subagent_model
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = model;
        }
        if let Some(model) = add_model {
            self.allowed_models
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(model.to_string());
        }
    }
}

/// 心跳 `run_progress` 的实时计数（§3.4.1：asks_done / asks_total / active_agents）：
/// 引擎侧更新，driver 每 15s 读一次写进 journal。
#[derive(Debug, Default)]
pub(crate) struct ProgressCounters {
    /// 已发起的 ask（含重放命中）。
    pub asks_total: AtomicU64,
    /// 已结算的 ask（答案回来 / 失败 / 重放命中）。
    pub asks_done: AtomicU64,
    /// 在途子代理数。
    pub active_agents: AtomicUsize,
}

/// 运行中的 hole：脚本已停在 tail/branch hole，等待 FillWorkflowHole（名称 / 提示已在
/// 登记事务里落 `workflow_hole`，这里只带 id）。
#[derive(Debug, Clone)]
pub(crate) struct HoleWaiting {
    pub hole_id: String,
}

/// 动态并发闸门：`max_parallel_asks` 可在运行中被 retune 调整（只影响尚未开始的 ask）。
struct DynamicSemaphore {
    limit: AtomicUsize,
    used: StdMutex<usize>,
}

struct SemaphorePermit {
    semaphore: Arc<DynamicSemaphore>,
}

impl DynamicSemaphore {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(DynamicSemaphore {
            limit: AtomicUsize::new(limit.clamp(1, MAX_MAX_PARALLEL_ASKS as usize)),
            used: StdMutex::new(0),
        })
    }

    fn set_limit(&self, limit: usize) {
        self.limit.store(
            limit.clamp(1, MAX_MAX_PARALLEL_ASKS as usize),
            Ordering::SeqCst,
        );
    }

    fn limit(&self) -> usize {
        self.limit.load(Ordering::SeqCst)
    }

    async fn acquire(self: &Arc<Self>, cancel: &CancellationToken) -> Option<SemaphorePermit> {
        loop {
            if cancel.is_cancelled() {
                return None;
            }
            {
                let mut used = self
                    .used
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if *used < self.limit() {
                    *used += 1;
                    return Some(SemaphorePermit {
                        semaphore: Arc::clone(self),
                    });
                }
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return None,
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    }
}

impl Drop for SemaphorePermit {
    fn drop(&mut self) {
        let mut used = self
            .semaphore
            .used
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *used = used.saturating_sub(1);
    }
}

/// §3.7「运行中权限变化」：派发 ask / 实际写操作前重查发起会话此刻的 [`ToolAccess`]。
/// 宿主答得出（桌面按权限模式）且低于 run 启动时的快照 → run 转 `stopping(permission_revoked)`
/// 并取消执行令牌（引擎退出后 driver 写 `stopped`，§13.3 顺带作废未决问题 / hole）；宿主不认识
/// 该会话（None）沿用快照，不触发撤权。返回 Err 时调用方应立即拒绝本次操作。
fn check_session_permission(shared: &EngineShared) -> Result<(), String> {
    let Some((channel, chat_id)) = shared.inherited.chat() else {
        return Ok(());
    };
    let Some(current) = rclaw_tools::access::session_access(channel, chat_id) else {
        return Ok(());
    };
    if current >= shared.tool_access {
        return Ok(());
    }
    tracing::warn!(
        "工作流 {} 的发起会话权限已从 {:?} 降为 {:?}，撤销执行（permission_revoked）",
        shared.run_id,
        shared.tool_access,
        current
    );
    match shared.store.begin_stopping(
        &shared.run_id,
        shared.exec_generation,
        STOP_PERMISSION_REVOKED,
        STOP_PERMISSION_REVOKED,
    ) {
        Ok(_) => {}
        Err(error) => tracing::warn!("工作流撤权转 stopping 失败: {error}"),
    }
    shared.cancel.cancel();
    Err("permission_revoked: 发起会话的权限已被收回，workflow 已停止派发".to_string())
}

struct ActiveAskGuard(Arc<ProgressCounters>);

impl ActiveAskGuard {
    fn new(progress: Arc<ProgressCounters>) -> Self {
        progress.active_agents.fetch_add(1, Ordering::SeqCst);
        ActiveAskGuard(progress)
    }
}

impl Drop for ActiveAskGuard {
    fn drop(&mut self) {
        self.0.active_agents.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 引擎共享态：宿主回调（并行 promise）、引擎驱动与收尾读取共用。
struct EngineShared {
    engine: StdMutex<EngineState>,
    /// 并行 ask 许可（`max_parallel_asks`，运行中可 retune）。
    permits: Arc<DynamicSemaphore>,
    /// 单片 JS CPU 预算（中断处理器与宿主回调共享）。
    budget: Arc<SliceBudget>,
    /// hole 等待 / 取消等宿主侧提前结束驱动的信号。
    halt: Arc<AtomicBool>,
    run_id: String,
    /// 本 run 的 journal 事件流（`workflow/<run_id>`）。
    journal: EventLog,
    /// 本 run 的取消令牌（run 自有；后台模式不继承 turn_cancel、不挂 drop_guard）。
    cancel: CancellationToken,
    /// 外层（多线程）runtime 句柄：ask 子代理的后台执行在它上面 spawn。引擎自己跑在
    /// spawn_blocking 里的内嵌 current_thread runtime 上，中止 / 超时后内嵌 runtime 随即
    /// 销毁——子代理若也在上面，会被连根丢弃而没有终态（登记表里永远 running）。
    outer: tokio::runtime::Handle,
    /// 发起方 turn 的 task-local 上下文（沙箱权限 / 会话键 / bot 身份），跨 runtime 带给子代理。
    inherited: super::InheritedTurnContext,
    /// 同 run 互斥守卫：挂在共享态上，随最后一个持有者（引擎线程 / 宿主回调）一起释放。
    _active_run: ActiveRunGuard,
    // ---- P2-P5 facade ----
    store: WorkflowStore,
    effects: Arc<EffectTracker>,
    paths: WorkflowPaths,
    args: Value,
    project_root: PathBuf,
    tool_access: ToolAccess,
    allowed_models: Arc<StdMutex<HashSet<String>>>,
    subagent_model: Arc<StdMutex<Option<String>>>,
    script_generation: u32,
    exec_generation: u32,
    hole: StdMutex<Option<HoleWaiting>>,
    hole_seq: AtomicU64,
    hole_name_seq: StdMutex<HashMap<String, u32>>,
    /// 本 exec 已登记过的 hole_id：同一调用点循环到达不重复写 `hole_opened` / 通知。
    holes_opened: StdMutex<HashSet<String>>,
    question_seq: Arc<AtomicU64>,
    /// 心跳计数（在途 ask 子代理数也在这里：hole 暂停前等它们结算）。
    progress: Arc<ProgressCounters>,
    holes_allowed: bool,
    log_ring: StdMutex<VecDeque<Value>>,
    log_count: AtomicU64,
    log_bytes: AtomicU64,
    log_truncated: AtomicBool,
    report_bytes: AtomicU64,
    report_count: AtomicU64,
    phase: StdMutex<Option<String>>,
    phase_count: AtomicU64,
    phase_truncated: AtomicBool,
    artifact_versions: StdMutex<HashMap<String, u32>>,
    /// 在途 async 宿主调用计数（见 [`MAX_PENDING_HOST_CALLS`]）。
    pending_hosts: Arc<AtomicUsize>,
    /// 同时执行的 effect 闸门（见 [`MAX_INFLIGHT_EFFECTS`]）。
    effect_permits: Arc<tokio::sync::Semaphore>,
    /// 本 run 各 ask 子代理的未解决错误披露（首次出现顺序，去重）。宿主在 ask /
    /// askJson 结算时从信封独立字段累计；随 EngineOutcome::Completed 交给
    /// run_completed 事件与 WorkflowRun 投影——脚本只拿 answer（JSON 契约不变），
    /// 披露走宿主侧旁路（codex 第 4 轮 P1）。
    unresolved_disclosures: StdMutex<Vec<String>>,
}

/// 领取一个 effect 执行名额：排队期间引擎停驻在宿主 future 上；取消时放弃（返回 None）。
async fn acquire_effect_permit<'a>(
    permits: &'a tokio::sync::Semaphore,
    cancel: &CancellationToken,
) -> Option<tokio::sync::SemaphorePermit<'a>> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        permit = permits.acquire() => permit.ok(),
    }
}

/// 在途 async 宿主调用的名额：在 JS 调用的同步段领取，随 future 结算 / 丢弃归还。
struct HostSlot(Arc<AtomicUsize>);

impl HostSlot {
    fn acquire(pending: &Arc<AtomicUsize>) -> Option<Self> {
        if pending.fetch_add(1, Ordering::SeqCst) >= MAX_PENDING_HOST_CALLS {
            pending.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(HostSlot(Arc::clone(pending)))
    }
}

impl Drop for HostSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn host_backlog_error() -> String {
    err_envelope(format!(
        "在途宿主调用超过 {MAX_PENDING_HOST_CALLS} 个上限；请分批 await 后再发起"
    ))
}

/// JS 字符串入参到 future 首次被 poll 时才拷到 Rust 堆（见 [`MAX_PENDING_HOST_CALLS`]）；
/// 各自的长度 / 字节上限随后在宿主函数里照常检查。
fn js_string(value: &rquickjs::String<'_>) -> Result<String, String> {
    value
        .to_string()
        .map_err(|error| format!("宿主入参不是合法字符串: {error}"))
}

fn js_string_opt(value: Option<&rquickjs::String<'_>>) -> Result<Option<String>, String> {
    value.map(js_string).transpose()
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

    /// 某计划当前是否正在本进程执行（守卫在握）。桌面端 plan_status 用它把「日志停在
    /// 进行中但执行者已不在」的计划（进程崩溃 / 工具 future 被丢弃且未落 plan_aborted）
    /// 投影成已中止，而不是永远转圈；plan_pause 对不在执行的计划拒绝落暂停。
    pub(super) fn is_plan_active(plan_id: &str) -> bool {
        ACTIVE_RUNS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .is_some_and(|active| active.contains(&format!("plan:{plan_id}")))
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

/// 注册 facade 的宿主函数：ask / result / log / report / phase / files / git / world /
/// artifact / hole。所有返回值都是 JSON 信封字符串（`{ok, value|error}`）。
///
/// async 宿主的字符串入参一律取 `rquickjs::String` 句柄、在 future 里再转 Rust 字符串，并在
/// 同步段领取 [`HostSlot`]：被拒的调用不捕获入参（见 [`MAX_PENDING_HOST_CALLS`]）。
fn register_workflow_hosts<'js>(
    ctx: &rquickjs::Ctx<'js>,
    runtime: AgentToolRuntime,
    shared: Arc<EngineShared>,
) {
    let (ask_runtime, ask_shared) = (runtime.clone(), Arc::clone(&shared));
    let ask_fn = Function::new(
        ctx.clone(),
        AsyncFn(
            move |actor: rquickjs::String<'js>,
                  question: rquickjs::String<'js>,
                  model: Option<rquickjs::String<'js>>,
                  system: Option<rquickjs::String<'js>>| {
                let (runtime, shared) = (ask_runtime.clone(), Arc::clone(&ask_shared));
                let admitted = HostSlot::acquire(&shared.pending_hosts)
                    .map(|slot| (slot, actor, question, model, system));
                async move {
                    let Some((_slot, actor, question, model, system)) = admitted else {
                        return host_backlog_error();
                    };
                    let (actor, question, model, system) = match (
                        js_string(&actor),
                        js_string(&question),
                        js_string_opt(model.as_ref()),
                        js_string_opt(system.as_ref()),
                    ) {
                        (Ok(actor), Ok(question), Ok(model), Ok(system)) => {
                            (actor, question, model, system)
                        }
                        (Err(error), _, _, _)
                        | (_, Err(error), _, _)
                        | (_, _, Err(error), _)
                        | (_, _, _, Err(error)) => return err_envelope(error),
                    };
                    ask_host(&runtime, &shared, actor, question, model, system).await
                }
            },
        ),
    )
    .expect("注册 __ask_raw");
    ctx.globals()
        .set("__ask_raw", ask_fn)
        .expect("set __ask_raw");
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
    ctx.globals()
        .set("__result", result_fn)
        .expect("set __result");
    let json_shared = Arc::clone(&shared);
    let json_fn = Function::new(
        ctx.clone(),
        AsyncFn(
            move |actor: rquickjs::String<'js>,
                  question: rquickjs::String<'js>,
                  schema: rquickjs::String<'js>,
                  model: Option<rquickjs::String<'js>>,
                  system: Option<rquickjs::String<'js>>| {
                let (runtime, shared) = (runtime.clone(), Arc::clone(&json_shared));
                let admitted = HostSlot::acquire(&shared.pending_hosts)
                    .map(|slot| (slot, actor, question, schema, model, system));
                async move {
                    let Some((_slot, actor, question, schema, model, system)) = admitted else {
                        return host_backlog_error();
                    };
                    let (actor, question, schema, model, system) = match (
                        js_string(&actor),
                        js_string(&question),
                        js_string(&schema),
                        js_string_opt(model.as_ref()),
                        js_string_opt(system.as_ref()),
                    ) {
                        (Ok(actor), Ok(question), Ok(schema), Ok(model), Ok(system)) => {
                            (actor, question, schema, model, system)
                        }
                        (Err(error), _, _, _, _)
                        | (_, Err(error), _, _, _)
                        | (_, _, Err(error), _, _)
                        | (_, _, _, Err(error), _)
                        | (_, _, _, _, Err(error)) => return err_envelope(error),
                    };
                    let schema = match parse_host_payload(&schema) {
                        Ok(schema) => schema,
                        Err(error) => return err_envelope(format!("askJson schema {error}")),
                    };
                    ask_json_host(&runtime, &shared, actor, question, schema, model, system).await
                }
            },
        ),
    )
    .expect("注册 __ask_json_raw");
    ctx.globals()
        .set("__ask_json_raw", json_fn)
        .expect("set __ask_json_raw");

    // ---- log / report / phase / hole（同步宿主）----
    let log_shared = Arc::clone(&shared);
    let log_fn = Function::new(ctx.clone(), move |message: String| -> String {
        host_log(&log_shared, message);
        "ok".to_string()
    })
    .expect("注册 __wf_log");
    ctx.globals().set("__wf_log", log_fn).expect("set __wf_log");

    let report_shared = Arc::clone(&shared);
    let report_fn = Function::new(ctx.clone(), move |item: String| -> String {
        match host_report(&report_shared, &item) {
            Ok(()) => "ok".to_string(),
            Err(error) => json!({"ok": false, "error": error}).to_string(),
        }
    })
    .expect("注册 __wf_report");
    ctx.globals()
        .set("__wf_report", report_fn)
        .expect("set __wf_report");

    let phase_shared = Arc::clone(&shared);
    let phase_fn = Function::new(ctx.clone(), move |name: String| -> String {
        host_phase(&phase_shared, name);
        "ok".to_string()
    })
    .expect("注册 __wf_phase");
    ctx.globals()
        .set("__wf_phase", phase_fn)
        .expect("set __wf_phase");

    let hole_shared = Arc::clone(&shared);
    let hole_fn = Function::new(
        ctx.clone(),
        move |name: String, prompt: String, site: Option<u32>| -> String {
            host_hole(&hole_shared, name, prompt, site)
        },
    )
    .expect("注册 __wf_hole");
    ctx.globals()
        .set("__wf_hole", hole_fn)
        .expect("set __wf_hole");

    // ---- files / git / world / artifact（async 宿主）----
    let files_shared = Arc::clone(&shared);
    let files_fn = Function::new(
        ctx.clone(),
        AsyncFn(
            move |op: rquickjs::String<'js>, payload: rquickjs::String<'js>| {
                let shared = Arc::clone(&files_shared);
                let admitted =
                    HostSlot::acquire(&shared.pending_hosts).map(|slot| (slot, op, payload));
                async move {
                    let Some((_slot, op, payload)) = admitted else {
                        return host_backlog_error();
                    };
                    match (js_string(&op), js_string(&payload)) {
                        (Ok(op), Ok(payload)) => host_files(&shared, op, payload).await,
                        (Err(error), _) | (_, Err(error)) => err_envelope(error),
                    }
                }
            },
        ),
    )
    .expect("注册 __wf_files");
    ctx.globals()
        .set("__wf_files", files_fn)
        .expect("set __wf_files");

    let git_shared = Arc::clone(&shared);
    let git_fn = Function::new(
        ctx.clone(),
        AsyncFn(
            move |op: rquickjs::String<'js>, payload: rquickjs::String<'js>| {
                let shared = Arc::clone(&git_shared);
                let admitted =
                    HostSlot::acquire(&shared.pending_hosts).map(|slot| (slot, op, payload));
                async move {
                    let Some((_slot, op, payload)) = admitted else {
                        return host_backlog_error();
                    };
                    match (js_string(&op), js_string(&payload)) {
                        (Ok(op), Ok(payload)) => host_git(&shared, op, payload).await,
                        (Err(error), _) | (_, Err(error)) => err_envelope(error),
                    }
                }
            },
        ),
    )
    .expect("注册 __wf_git");
    ctx.globals().set("__wf_git", git_fn).expect("set __wf_git");

    let world_shared = Arc::clone(&shared);
    let world_fn = Function::new(
        ctx.clone(),
        AsyncFn(move |payload: rquickjs::String<'js>| {
            let shared = Arc::clone(&world_shared);
            let admitted = HostSlot::acquire(&shared.pending_hosts).map(|slot| (slot, payload));
            async move {
                let Some((_slot, payload)) = admitted else {
                    return host_backlog_error();
                };
                match js_string(&payload) {
                    Ok(payload) => host_world(&shared, payload).await,
                    Err(error) => err_envelope(error),
                }
            }
        }),
    )
    .expect("注册 __wf_world");
    ctx.globals()
        .set("__wf_world", world_fn)
        .expect("set __wf_world");

    let artifact_shared = Arc::clone(&shared);
    let artifact_fn = Function::new(
        ctx.clone(),
        AsyncFn(
            move |kind: rquickjs::String<'js>, payload: rquickjs::String<'js>| {
                let shared = Arc::clone(&artifact_shared);
                let admitted =
                    HostSlot::acquire(&shared.pending_hosts).map(|slot| (slot, kind, payload));
                async move {
                    let Some((_slot, kind, payload)) = admitted else {
                        return host_backlog_error();
                    };
                    match (js_string(&kind), js_string(&payload)) {
                        (Ok(kind), Ok(payload)) => host_artifact(&shared, kind, payload).await,
                        (Err(error), _) | (_, Err(error)) => err_envelope(error),
                    }
                }
            },
        ),
    )
    .expect("注册 __wf_artifact");
    ctx.globals()
        .set("__wf_artifact", artifact_fn)
        .expect("set __wf_artifact");
}

fn ok_envelope(value: Value) -> String {
    json!({"ok": true, "value": value}).to_string()
}

fn err_envelope(error: impl Into<String>) -> String {
    json!({"ok": false, "error": error.into()}).to_string()
}

/// 宿主回调的 JSON 入参：先卡字节上限再解析（非法 JSON 按 null 处理，各宿主自行报缺参）。
fn parse_host_payload(payload: &str) -> Result<Value, String> {
    if payload.len() > MAX_HOST_PAYLOAD_BYTES {
        return Err(format!("入参超过 {MAX_HOST_PAYLOAD_BYTES} 字节上限"));
    }
    Ok(serde_json::from_str(payload).unwrap_or(Value::Null))
}

/// 单代 journal 里 `log` 事件的条数 / 字节上限：超过后只进内存环（log_tail 仍能看到最新的），
/// 并记一条 `log_truncated`。journal 是全量 load 的，一个 `for` 里狂 `log()` 的脚本不能把
/// GetWorkflowRun / resume 的加载时间和内存一起撑爆。
const MAX_JOURNAL_LOGS: u64 = 2_000;
const MAX_JOURNAL_LOG_BYTES: u64 = 1024 * 1024;

fn host_log(shared: &EngineShared, message: String) {
    let message = truncate_chars(&message, 4_000);
    let event =
        json!({"event": "log", "message": message, "exec_generation": shared.exec_generation});
    let count = shared.log_count.fetch_add(1, Ordering::SeqCst) + 1;
    let bytes = shared
        .log_bytes
        .fetch_add(message.len() as u64, Ordering::SeqCst)
        + message.len() as u64;
    if count <= MAX_JOURNAL_LOGS && bytes <= MAX_JOURNAL_LOG_BYTES {
        append_workflow_event(&shared.journal, &event);
    } else if !shared.log_truncated.swap(true, Ordering::SeqCst) {
        append_workflow_event(
            &shared.journal,
            &json!({
                "event": "log_truncated",
                "exec_generation": shared.exec_generation,
                "message": format!(
                    "[workflow] 本代 log 超过 {MAX_JOURNAL_LOGS} 条 / {MAX_JOURNAL_LOG_BYTES} 字节，后续只保留在内存尾部"
                ),
            }),
        );
    }
    let mut ring = shared
        .log_ring
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if ring.len() >= 500 {
        ring.pop_front();
    }
    ring.push_back(event);
}

fn host_report(shared: &EngineShared, item: &str) -> Result<(), String> {
    if item.len() > 1024 * 1024 {
        return Err("report 单条超过 1MiB 上限".to_string());
    }
    let count = shared.report_count.fetch_add(1, Ordering::SeqCst) + 1;
    if count > 65_536 {
        shared.report_count.fetch_sub(1, Ordering::SeqCst);
        return Err("report 超过 65536 条上限".to_string());
    }
    let total = shared
        .report_bytes
        .fetch_add(item.len() as u64, Ordering::SeqCst)
        + item.len() as u64;
    if total > 64 * 1024 * 1024 {
        shared
            .report_bytes
            .fetch_sub(item.len() as u64, Ordering::SeqCst);
        shared.report_count.fetch_sub(1, Ordering::SeqCst);
        return Err("report 累计超过 64MiB 上限".to_string());
    }
    let parsed: Value = serde_json::from_str(item).unwrap_or(Value::Null);
    append_workflow_event(
        &shared.journal,
        &json!({
            "event": "report",
            "seq": count,
            "item": parsed,
            "exec_generation": shared.exec_generation,
        }),
    );
    Ok(())
}

/// 单代 journal 里 `phase` 事件的条数上限：规范里 phase 是顶层字面量语句（静态有界），但
/// 没有谁强制——`for (;;) { phase("a"); phase("b"); }` 是同步宿主、每条一笔事务，与 log 同样
/// 会把全量 load 的 journal 撑爆。超限后只更新内存里的当前阶段，并记一条 `phase_truncated`。
const MAX_JOURNAL_PHASES: u64 = 200;

fn host_phase(shared: &EngineShared, name: String) {
    let name = truncate_chars(name.trim(), 100);
    if name.is_empty() {
        return;
    }
    let mut current = shared
        .phase
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if current.as_deref() == Some(name.as_str()) {
        return;
    }
    *current = Some(name.clone());
    let count = shared.phase_count.fetch_add(1, Ordering::SeqCst) + 1;
    if count > MAX_JOURNAL_PHASES {
        if !shared.phase_truncated.swap(true, Ordering::SeqCst) {
            append_workflow_event(
                &shared.journal,
                &json!({
                    "event": "phase_truncated",
                    "exec_generation": shared.exec_generation,
                    "message": format!("[workflow] 本代 phase 超过 {MAX_JOURNAL_PHASES} 条，后续不再记录"),
                }),
            );
        }
        return;
    }
    append_workflow_event(
        &shared.journal,
        &json!({
            "event": "phase",
            "name": name,
            "at": chrono::Utc::now().timestamp_millis(),
            "exec_generation": shared.exec_generation,
        }),
    );
}

/// 单次执行内可登记的不同 hole 数上限（并行分支各自留白是正常用法，循环造新名字不是）。
const MAX_HOLES_PER_EXEC: usize = 64;

/// hole 名字进入 `hole_id` 的安全形态（trim、≤100 字符、只保留 `[A-Za-z0-9._-]`）。
/// `host_hole` 造 id 与 `FillWorkflowHole` 核对调用点都用它，保证两边同一口径。
pub(crate) fn safe_hole_name(name: &str) -> String {
    truncate_chars(name.trim(), 100)
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn host_hole(shared: &EngineShared, name: String, prompt: String, site: Option<u32>) -> String {
    if !shared.holes_allowed {
        return err_envelope("hole 不可用：当前阶段未启用 / 同步模式返回 background_required");
    }
    // 取消 / 超时宽限期内脚本仍可能走到 hole：不再登记留白、不再给主代理发通知。
    if shared.cancel.is_cancelled() {
        return err_envelope("workflow 已取消");
    }
    let name = truncate_chars(name.trim(), 100);
    // hole_id 末段 = prepare_script 注入的全局源码序号（0 起）；没有注入（扫描器没识别的
    // 写法）才退回“同名第几个”（1 起）。Fill 按同一规则定位调用点。
    let safe_name = safe_hole_name(&name);
    let seq = match site {
        Some(site) => site,
        None => {
            let mut counters = shared
                .hole_name_seq
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            // 别名调用（`const h = hole; h("n" + i)`）拿不到注入的调用点序号，每个新名字都会
            // 在这里留一项：同样受 MAX_HOLES_PER_EXEC 约束，否则 120s 一片能塞进千万个名字。
            if !counters.contains_key(&safe_name) && counters.len() >= MAX_HOLES_PER_EXEC {
                return err_envelope(format!("hole 超过单次执行 {MAX_HOLES_PER_EXEC} 个上限"));
            }
            let counter = counters.entry(safe_name.clone()).or_insert(0);
            *counter += 1;
            *counter
        }
    };
    let hole_id = format!("hole#{}-{}-{}", safe_name, shared.script_generation, seq);
    // 同一 exec 内同一 hole_id 只登记一次（同一调用点被 `for (;;) { try { hole() } catch {} }`
    // 反复到达）；不同 hole_id 也有上限——每次登记都是一笔事务 + 一条通知，不能无界。
    {
        let opened = shared
            .holes_opened
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if opened.contains(&hole_id) {
            return err_envelope(format!("__hole_pending__: {hole_id} 等待填充"));
        }
        if opened.len() >= MAX_HOLES_PER_EXEC {
            return err_envelope(format!("hole 超过单次执行 {MAX_HOLES_PER_EXEC} 个上限"));
        }
    }
    let _ = shared.hole_seq.fetch_add(1, Ordering::SeqCst);
    let prompt = truncate_chars(prompt.trim(), 2_000);
    // 通知在 hole 登记的同一事务入队（事务内不能再取连接锁，行先读出来）：主代理立刻
    // 得知留白，不必等旧 exec 的其它分支 / 在途 ask 结算后 driver 退出。id 带 exec 代次：
    // resume 后重新到达同一 hole 要再通知一次，同一 exec 内重复到达则去重。
    let notify_row = match shared.store.get_run(&shared.run_id) {
        Ok(row) => row,
        Err(error) => return err_envelope(error),
    };
    let event = json!({
        "event": "hole_opened",
        "hole_id": hole_id,
        "script_generation": shared.script_generation,
        "name": name,
        "prompt": prompt,
        "exec_generation": shared.exec_generation,
    });
    let outcome = shared.store.transaction(|tx| {
        shared
            .journal
            .append_tx(tx, &event)
            .map_err(|error| format!("journal hole 写入失败: {error}"))?;
        tx.execute(
            "INSERT OR REPLACE INTO workflow_hole (run_id, hole_id, script_generation, name, prompt, \
             state, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'waiting', ?6, ?6)",
            rusqlite::params![
                &shared.run_id,
                &hole_id,
                shared.script_generation as i64,
                &name,
                &prompt,
                rclaw_store::db::now_ms()
            ],
        )
        .map_err(|error| error.to_string())?;
        if let Some(row) = &notify_row {
            let (channel, chat_id) =
                super::workflow_service::split_session_key(&row.session_key);
            super::workflow_store::enqueue_notification_tx(
                tx,
                &format!(
                    "wf-note-{}-{}-hole-{}",
                    shared.run_id, shared.exec_generation, hole_id
                ),
                &shared.run_id,
                &row.session_key,
                &channel,
                &chat_id,
                &json!({
                    "kind": "hole",
                    "run_id": shared.run_id,
                    "label": row.label,
                    "hole_id": hole_id,
                    "name": name,
                }),
            )?;
        }
        Ok(())
    });
    if let Err(error) = outcome {
        return err_envelope(error);
    }
    shared
        .holes_opened
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert(hole_id.clone());
    // 不置 halt：留白只中止当前 async 链（下面的错误信封）。其它分支与在途 ask 继续跑到
    // 静止（答案照常落 journal，fill 后的新 exec 零成本重放）；FillWorkflowHole 需要停旧
    // exec 时取消 run 令牌即可（新 ask 被拒、在途 ask 联动取消）。规范 §3.5「旧 exec 继续跑」。
    *shared
        .hole
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(HoleWaiting {
        hole_id: hole_id.clone(),
    });
    err_envelope(format!("__hole_pending__: {hole_id} 等待填充"))
}

async fn host_files(shared: &Arc<EngineShared>, op: String, payload: String) -> String {
    let args = match parse_host_payload(&payload) {
        Ok(args) => args,
        Err(error) => return err_envelope(format!("files.{op} {error}")),
    };
    let op_name = format!("files.{op}");
    let Some(_effect_permit) = acquire_effect_permit(&shared.effect_permits, &shared.cancel).await
    else {
        return err_envelope("workflow 已取消");
    };
    let begin = match shared.effects.begin(&op_name, false, &args, None) {
        Ok(begin) => begin,
        Err(error) => return err_envelope(error),
    };
    let key = match begin {
        BeginEffect::Replay { value } => return ok_envelope(value),
        BeginEffect::Execute { key, .. } => key,
    };
    // 同步文件系统工作放到外层 runtime 的阻塞线程：引擎线程空转等待时 `drive_to_quiescence`
    // 会把这段墙钟从 JS 片预算里扣掉（见 WakeOnce 后的 reset）；在引擎线程上直接跑一次
    // 100s 的 grep 会被当成“CPU 预算耗尽”判失败。
    let root = shared.project_root.clone();
    let worker_op = op.clone();
    let result = shared
        .outer
        .spawn_blocking(move || match worker_op.as_str() {
            "glob" => {
                let pattern = args
                    .get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                workflow_world::files_glob(&root, pattern)
            }
            "read" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or_default();
                workflow_world::files_read(&root, path)
            }
            "grep" => {
                let pattern = args
                    .get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let glob = args.get("glob").and_then(Value::as_str);
                workflow_world::files_grep(&root, pattern, glob)
            }
            other => Err(format!("未知 files 操作: {other}")),
        })
        .await
        .unwrap_or_else(|error| Err(format!("files.{op} 工作线程失败: {error}")));
    settle_effect(shared, &key, result)
}

/// effect 终态落账并转成信封：结果累计超上限时 finish 不保留结果并返回 Err，脚本拿到的
/// 也必须是错误（不能一边记 failed 一边把值交给脚本，resume 重放会对不上）。
fn settle_effect(shared: &EngineShared, key: &str, result: Result<Value, String>) -> String {
    match result {
        Ok(value) => match shared.effects.finish(key, "completed", &value) {
            Ok(()) => ok_envelope(value),
            Err(error) => err_envelope(error),
        },
        Err(error) => {
            let _ = shared
                .effects
                .finish(key, "failed", &json!({"error": error}));
            err_envelope(error)
        }
    }
}

async fn host_git(shared: &Arc<EngineShared>, op: String, payload: String) -> String {
    let args = match parse_host_payload(&payload) {
        Ok(args) => args,
        Err(error) => return err_envelope(format!("git.{op} {error}")),
    };
    let op_name = format!("git.{op}");
    let Some(_effect_permit) = acquire_effect_permit(&shared.effect_permits, &shared.cancel).await
    else {
        return err_envelope("workflow 已取消");
    };
    let begin = match shared.effects.begin(&op_name, false, &args, None) {
        Ok(begin) => begin,
        Err(error) => return err_envelope(error),
    };
    let key = match begin {
        BeginEffect::Replay { value } => return ok_envelope(value),
        BeginEffect::Execute { key, .. } => key,
    };
    // 同步等 git 子进程（最长 60s）：与 files 同理放到外层阻塞线程，不占引擎片预算。
    let root = shared.project_root.clone();
    let worker_op = op.clone();
    let result = shared
        .outer
        .spawn_blocking(move || match worker_op.as_str() {
            "status" => workflow_world::git_status(&root),
            "diff" => {
                let path = args.get("path").and_then(Value::as_str);
                workflow_world::git_diff(&root, path)
            }
            other => Err(format!("未知 git 操作: {other}")),
        })
        .await
        .unwrap_or_else(|error| Err(format!("git.{op} 工作线程失败: {error}")));
    settle_effect(shared, &key, result)
}

async fn host_world(shared: &Arc<EngineShared>, payload: String) -> String {
    let args = match parse_host_payload(&payload) {
        Ok(args) => args,
        Err(error) => return err_envelope(format!("world.run {error}")),
    };
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if command.trim().is_empty() {
        return err_envelope("world.run 需要 command");
    }
    // 权限不够的调用在登记 effect 之前就拒绝（world_run 内部还会再查一次）：否则 Sandboxed
    // 会话经别名绕过静态扫描的 world.run 会留下一条 failed 写 effect，关掉 amend 缓存、
    // 把 Amend 挡在 UI 确认之后。
    if shared.tool_access != ToolAccess::Unrestricted {
        return err_envelope("forbidden_world: world.run 仅 Unrestricted 会话可用");
    }
    let opts = args.get("opts").cloned().unwrap_or_else(|| json!({}));
    let idempotency_key = opts
        .get("idempotency_key")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(_effect_permit) = acquire_effect_permit(&shared.effect_permits, &shared.cancel).await
    else {
        return err_envelope("workflow 已取消");
    };
    // §3.7：实际写操作前重查发起会话权限（撤权后不再登记 effect、不再起进程）——放在拿到
    // 执行名额之后，排队几分钟的调用不会带着过期的检查结果开跑。
    if let Err(error) = check_session_permission(shared) {
        return err_envelope(error);
    }
    let begin = match shared.effects.begin(
        "world.run",
        true,
        &json!({"command": command, "opts": opts}),
        idempotency_key.as_deref(),
    ) {
        Ok(begin) => begin,
        Err(error) => return err_envelope(error),
    };
    let key = match begin {
        BeginEffect::Replay { value } => return ok_envelope(value),
        BeginEffect::Execute { key, .. } => key,
    };
    let store = shared.store.clone();
    let run_id = shared.run_id.clone();
    let exec_generation = shared.exec_generation;
    let outcome = workflow_world::world_run(
        &shared.project_root,
        shared.tool_access,
        &command,
        &opts,
        &shared.cancel,
        Duration::from_secs(300),
        move |process_id, root_pid| {
            if let Err(error) = store.record_process(&run_id, exec_generation, process_id, root_pid)
            {
                tracing::warn!("记录 world.run 进程树失败: {error}");
            }
        },
    )
    .await;
    match outcome {
        Ok(world) => {
            if let Err(error) = shared.store.mark_process_exited(
                &shared.run_id,
                shared.exec_generation,
                &world.process_id,
            ) {
                tracing::warn!("更新 world.run 进程状态失败: {error}");
            }
            // 超时无法证明命令没有写入：按 unknown 处理，阻止自动 resume。
            if world
                .value
                .get("timed_out")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                shared
                    .effects
                    .mark_unknown(&key, "world.run timed out after process tree exit");
                return err_envelope("world_run_unknown: world.run 超时（需逐项裁决）");
            }
            settle_effect(shared, &key, Ok(world.value))
        }
        Err(error) => {
            if error.starts_with("world_run_unknown") {
                shared.effects.mark_unknown(&key, &error);
                return err_envelope(error);
            }
            settle_effect(shared, &key, Err(error))
        }
    }
}

async fn host_artifact(shared: &Arc<EngineShared>, kind: String, payload: String) -> String {
    let args = match parse_host_payload(&payload) {
        Ok(args) => args,
        Err(error) => return err_envelope(format!("artifact.{kind} {error}")),
    };
    let op_name = format!("artifact.{kind}");
    let Some(_effect_permit) = acquire_effect_permit(&shared.effect_permits, &shared.cancel).await
    else {
        return err_envelope("workflow 已取消");
    };
    // §3.7：artifact 也是落盘写操作，拿到执行名额后、登记 effect 前重查发起会话此刻的权限。
    if let Err(error) = check_session_permission(shared) {
        return err_envelope(error);
    }
    let begin = match shared.effects.begin(&op_name, true, &args, None) {
        Ok(begin) => begin,
        Err(error) => return err_envelope(error),
    };
    let key = match begin {
        BeginEffect::Replay { value } => return ok_envelope(value),
        BeginEffect::Execute { key, .. } => key,
    };
    // 最多 20MiB 的文件复制 + 原子写：同样放到外层阻塞线程。
    let worker_shared = Arc::clone(shared);
    let worker_kind = kind.clone();
    let written = shared
        .outer
        .spawn_blocking(move || write_artifact(&worker_shared, &worker_kind, &args))
        .await
        .unwrap_or_else(|error| Err(format!("artifact.{kind} 工作线程失败: {error}")));
    settle_effect(shared, &key, written)
}

fn write_artifact(shared: &EngineShared, kind: &str, args: &Value) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| "artifact 需要 id".to_string())?;
    let opts = args.get("opts").cloned().unwrap_or_else(|| json!({}));
    let title = opts
        .get("title")
        .and_then(Value::as_str)
        .map(|text| truncate_chars(text, 200))
        .unwrap_or_else(|| id.to_string());
    let primary = opts
        .get("primary")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let dir = workflow_world::artifact_target(&shared.project_root, &shared.run_id, id)?;
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建 artifact 目录失败: {error}"))?;
    let mut version = {
        let mut versions = shared
            .artifact_versions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let version = versions.entry(id.to_string()).or_insert(0);
        *version += 1;
        *version
    };
    let (extension, bytes) = match kind {
        "file" => {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "artifact.file 需要 path".to_string())?;
            let resolved = resolve_under(&shared.project_root, path, true)?;
            let metadata = std::fs::metadata(&resolved)
                .map_err(|error| format!("artifact.file 无法访问: {error}"))?;
            if !metadata.is_file() {
                return Err("artifact.file 目标不是文件".to_string());
            }
            if metadata.len() > 20 * 1024 * 1024 {
                return Err("artifact.file 超过 20MiB 上限".to_string());
            }
            let bytes = std::fs::read(&resolved)
                .map_err(|error| format!("artifact.file 读取失败: {error}"))?;
            let extension = resolved
                .extension()
                .and_then(|ext| ext.to_str())
                .filter(|ext| ext.len() <= 16)
                .unwrap_or("bin")
                .to_ascii_lowercase();
            (extension, bytes)
        }
        "markdown" => {
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "artifact.markdown 需要 content".to_string())?;
            if content.len() > 256 * 1024 {
                return Err("artifact.markdown 超过 256KB 上限".to_string());
            }
            ("md".to_string(), content.as_bytes().to_vec())
        }
        "table" => {
            let rows = args.get("rows").cloned().unwrap_or_else(|| json!([]));
            ("md".to_string(), render_table_markdown(&rows).into_bytes())
        }
        "metrics" => {
            let metrics = args.get("metrics").cloned().unwrap_or_else(|| json!({}));
            (
                "md".to_string(),
                render_metrics_markdown(&metrics).into_bytes(),
            )
        }
        "board" => {
            let columns = args.get("columns").cloned().unwrap_or_else(|| json!([]));
            (
                "md".to_string(),
                render_board_markdown(&columns).into_bytes(),
            )
        }
        "chart" => {
            let spec = args.get("spec").cloned().unwrap_or_else(|| json!({}));
            ("svg".to_string(), render_chart_svg(&spec).into_bytes())
        }
        other => return Err(format!("未知 artifact 类型: {other}")),
    };
    let mut path = dir.join(format!("v{version}.{extension}"));
    while path.exists() {
        version += 1;
        path = dir.join(format!("v{version}.{extension}"));
    }
    workflow_paths::write_atomic(&path, &bytes)?;
    if let Ok(mut versions) = shared.artifact_versions.lock() {
        let entry = versions.entry(id.to_string()).or_insert(0);
        *entry = (*entry).max(version);
    }
    let display = workflow_snapshot::display_path(&shared.paths, &path);
    append_workflow_event(
        &shared.journal,
        &json!({
            "event": "artifact",
            "id": id,
            "kind": kind,
            "title": title,
            "version": version,
            "primary": primary,
            "path": display,
            "content_type": opts
                .get("contentType")
                .and_then(Value::as_str)
                .map(|text| truncate_chars(text, 100)),
            "run_id": shared.run_id,
            "exec_generation": shared.exec_generation,
        }),
    );
    Ok(json!({
        "id": id,
        "version": version,
        "kind": kind,
        "title": title,
        "primary": primary,
        "path": display,
    }))
}

fn render_table_markdown(rows: &Value) -> String {
    let Some(rows) = rows.as_array() else {
        return "_(no rows)_\n".to_string();
    };
    if rows.is_empty() {
        return "_(no rows)_\n".to_string();
    }
    let escape = |value: &Value| -> String {
        let text = match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        text.replace('|', "\\|").replace('\n', " ")
    };
    let headers: Vec<String> = match &rows[0] {
        Value::Object(map) => map.keys().cloned().collect(),
        _ => (0..rows[0].as_array().map(Vec::len).unwrap_or(0))
            .map(|index| format!("col{index}"))
            .collect(),
    };
    let mut out = String::new();
    out.push_str(&format!("| {} |\n", headers.join(" | ")));
    out.push_str(&format!(
        "| {} |\n",
        headers
            .iter()
            .map(|_| "---")
            .collect::<Vec<_>>()
            .join(" | ")
    ));
    for row in rows {
        let cells: Vec<String> = match row {
            Value::Object(map) => headers
                .iter()
                .map(|header| escape(map.get(header).unwrap_or(&Value::Null)))
                .collect(),
            Value::Array(values) => values.iter().map(escape).collect(),
            other => vec![escape(other)],
        };
        out.push_str(&format!("| {} |\n", cells.join(" | ")));
    }
    out
}

fn render_metrics_markdown(metrics: &Value) -> String {
    match metrics {
        Value::Object(map) => {
            let mut out = String::from("| metric | value |\n| --- | --- |\n");
            for (key, value) in map {
                out.push_str(&format!(
                    "| {} | {} |\n",
                    key.replace('|', "\\|"),
                    value.to_string().replace('|', "\\|")
                ));
            }
            out
        }
        other => render_table_markdown(other),
    }
}

fn render_board_markdown(columns: &Value) -> String {
    let Some(columns) = columns.as_array() else {
        return "_(no columns)_\n".to_string();
    };
    let mut out = String::new();
    for column in columns {
        let title = column
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Column");
        out.push_str(&format!("## {title}\n\n"));
        if let Some(items) = column.get("items").and_then(Value::as_array) {
            for item in items {
                match item {
                    Value::String(text) => out.push_str(&format!("- {text}\n")),
                    other => out.push_str(&format!("- {}\n", other)),
                }
            }
        }
        out.push('\n');
    }
    out
}

fn render_chart_svg(spec: &Value) -> String {
    let title = spec.get("title").and_then(Value::as_str).unwrap_or("chart");
    let kind = spec.get("type").and_then(Value::as_str).unwrap_or("bar");
    let data = spec
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let values: Vec<f64> = data
        .iter()
        .filter_map(|point| match point {
            Value::Number(number) => number.as_f64(),
            Value::Object(map) => map.get("value").and_then(Value::as_f64),
            _ => None,
        })
        .collect();
    let max = values.iter().cloned().fold(0.0f64, f64::max).max(1.0);
    let (width, height, padding) = (640.0f64, 360.0f64, 32.0f64);
    let plot_width = width - padding * 2.0;
    let plot_height = height - padding * 2.0;
    let mut body = String::new();
    if kind == "line" {
        let step = if values.len() > 1 {
            plot_width / (values.len() - 1) as f64
        } else {
            0.0
        };
        let points: Vec<String> = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                format!(
                    "{:.1},{:.1}",
                    padding + index as f64 * step,
                    padding + plot_height - (value / max) * plot_height
                )
            })
            .collect();
        if !points.is_empty() {
            body.push_str(&format!(
                "<polyline fill=\"none\" stroke=\"#4a7\" stroke-width=\"2\" points=\"{}\"/>",
                points.join(" ")
            ));
        }
    } else {
        let slot = plot_width / values.len().max(1) as f64;
        for (index, value) in values.iter().enumerate() {
            let bar_height = (value / max) * plot_height;
            body.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"#4a7\"/>",
                padding + index as f64 * slot + slot * 0.1,
                padding + plot_height - bar_height,
                slot * 0.8,
                bar_height
            ));
        }
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" \
         viewBox=\"0 0 {width} {height}\"><rect width=\"100%\" height=\"100%\" fill=\"#fff\"/>\
         <text x=\"{padding}\" y=\"20\" font-family=\"sans-serif\" font-size=\"14\">{}</text>\
         <line x1=\"{padding}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#ccc\"/>\
         {body}</svg>",
        title.replace('<', "&lt;").replace('>', "&gt;"),
        padding + plot_height,
        width - padding,
        padding + plot_height
    )
}

/// 引擎没有正常结算的原因：driver 据此区分 stopped（取消）与 errored(budget_exhausted)
/// （墙钟耗尽）——两者都会取消引擎自己的令牌，不能靠令牌状态倒推。
enum EngineAbort {
    /// 外部取消（Cancel / amend 取代 / 同步模式父 turn）。
    Cancelled(String),
    /// 墙钟预算 `max_run_secs` 耗尽。
    Budget(String),
    /// 脚本 / 引擎自身错误。
    Script(String),
}

/// 在内嵌 runtime 上驱动引擎到结算，并与取消令牌、墙钟预算竞速：取消 / 超时先经 token 让
/// 子代理优雅收尾（宽限期），再置 halt 终止 JS。
async fn drive_engine(
    script: String,
    max_run: Duration,
    runtime: AgentToolRuntime,
    shared: Arc<EngineShared>,
) -> Result<Option<ScriptOutcome>, EngineAbort> {
    let host_shared = Arc::clone(&shared);
    let drive = eval_deterministic(
        script,
        Arc::clone(&shared.budget),
        Arc::clone(&shared.halt),
        shared.args.clone(),
        move |ctx| register_workflow_hosts(ctx, runtime, host_shared),
    );
    tokio::pin!(drive);
    let abort = tokio::select! {
        biased;
        _ = shared.cancel.cancelled() => EngineAbort::Cancelled(
            "workflow 已取消（父 turn 取消或工具调用被放弃）".to_string(),
        ),
        driven = tokio::time::timeout(max_run, &mut drive) => match driven {
            Ok(outcome) => return outcome.map_err(EngineAbort::Script),
            Err(_) => EngineAbort::Budget(format!(
                "workflow 超过墙钟预算 {}s（max_run_secs）",
                max_run.as_secs()
            )),
        },
    };
    // 引擎令牌是 run 令牌的子令牌（见 driver）：这里取消只波及引擎内部（在途 ask / world.run），
    // driver 看到的 run 令牌不变，墙钟耗尽才能被正确记成 errored 而不是 stopped。
    shared.cancel.cancel();
    if tokio::time::timeout(CANCEL_GRACE, &mut drive)
        .await
        .is_err()
    {
        shared.halt.store(true, Ordering::SeqCst);
        let _ = tokio::time::timeout(HALT_GRACE, &mut drive).await;
    }
    Err(abort)
}

// ---------------------------------------------------------------------------
// 引擎入口（后台 driver / 同步模式 / EvalSnippet 共用）
// ---------------------------------------------------------------------------

/// 一次执行的完整上下文（由 service/driver 从快照 + execution_config 构建）。
pub(crate) struct EngineRequest {
    pub run_id: String,
    /// 已编译的 exec script（`.ask<T>` 已重写）。
    pub script: String,
    /// 作者原文的 FNV hash（journal / snapshot 校验用）。
    pub script_hash: u64,
    pub script_generation: u32,
    pub exec_generation: u32,
    pub journal: EventLog,
    pub cancel: CancellationToken,
    pub max_run: Duration,
    pub max_parallel_asks: usize,
    pub subagent_model: Option<String>,
    pub allowed_models: HashSet<String>,
    pub args: Value,
    pub project_root: PathBuf,
    pub tool_access: ToolAccess,
    pub store: WorkflowStore,
    pub paths: WorkflowPaths,
    pub effects: Arc<EffectTracker>,
    pub replay: ReplayTable,
    pub holes_allowed: bool,
    pub inherited: super::InheritedTurnContext,
    /// driver 在引擎就绪后写入 retune 句柄（Amend 模式 A 用）。
    pub retune_slot: Arc<StdMutex<Option<RetuneHandle>>>,
    /// 心跳计数（driver 创建并持有，引擎侧更新）。
    pub progress: Arc<ProgressCounters>,
}

/// 引擎执行结果。
pub(crate) enum EngineOutcome {
    Completed {
        result: Option<String>,
        asks_total: u64,
        asks_replayed: u64,
        /// 本 run 各 ask 子代理的未解决错误披露（宿主累计，见
        /// [`EngineShared::unresolved_disclosures`]）。
        unresolved_disclosures: Vec<String>,
    },
    HoleWaiting {
        hole: HoleWaiting,
    },
    Failed {
        error: String,
        /// 墙钟预算（max_run_secs）耗尽：driver 记 `errored(budget_exhausted)`。
        budget_exhausted: bool,
    },
}

/// 提交期脚本准备：typed ask 编译 + 语法/lint/tsc。
pub(crate) struct PreparedScript {
    /// 作者原文（规范化为 LF）——hash 与快照都按它。
    pub script: String,
    /// 编译后的 exec script（`.ask<T>` 已重写为 askJson）。
    pub exec_script: String,
    pub script_hash: u64,
    pub typed_asks: usize,
    pub insight: WorkflowInsight,
}

pub(crate) async fn prepare_script(
    script: &str,
    type_check: bool,
) -> Result<PreparedScript, String> {
    let script = workflow_snapshot::normalize_script(script);
    // 注入静态 hole 调用点序号（快照/hash 基于注入后的文本）。注入是幂等的：已有序号原位
    // 改写，所以 resume 重新 prepare 同一快照 hash 不变；fill 后正文里新出现的 hole 会让
    // 后续调用点重新编号（hole_id 带 script_generation，旧代序号不会被误用）。
    let (mut script, _) = super::workflow_saved::annotate_hole_sites(&script);
    if !script.contains("/* rclaw-hole-sites */") {
        script.push_str("\n/* rclaw-hole-sites */\n");
    }
    if script.chars().count() > MAX_SCRIPT_CHARS {
        return Err(format!("script 超过 {MAX_SCRIPT_CHARS} 字符"));
    }
    let compile_input = script.clone();
    let compiled = tokio::task::spawn_blocking(move || {
        super::workflow_ast::compile_typed_asks(&compile_input)
            .map(|transform| (transform.exec_script, transform.typed_asks))
    })
    .await
    .unwrap_or_else(|error| {
        tracing::warn!("typed ask 编译线程失败，按未重写脚本继续: {error}");
        Ok((script.clone(), Vec::new()))
    });
    let (exec_script, typed_asks) = match compiled {
        Ok(compiled) => compiled,
        Err(diagnostics) => return Err(format_diagnostics(&diagnostics)),
    };
    let tsc_input = if typed_asks.is_empty() {
        exec_script.clone()
    } else {
        script.clone()
    };
    let lint_script = exec_script.clone();
    // 静态洞察（依赖图）与 lint 同在阻塞线程上算：解析 + 分组对大脚本是实打实的 CPU 工作，
    // 不占 async 工作线程。
    let (diagnostics, insight) = tokio::task::spawn_blocking(move || {
        let mut diagnostics = check_script(&lint_script, false);
        if type_check && diagnostics.is_empty() {
            if let Some(mut tsc_diagnostics) = type_check_with_tsc(&tsc_input) {
                diagnostics.append(&mut tsc_diagnostics);
            }
        }
        let insight = diagnostics
            .is_empty()
            .then(|| analyze_workflow_script(&lint_script));
        (diagnostics, insight)
    })
    .await
    .unwrap_or_default();
    if !diagnostics.is_empty() {
        return Err(format_diagnostics(&diagnostics));
    }
    let script_hash = fnv1a64(&script);
    let insight = insight.unwrap_or_else(|| analyze_workflow_script(&exec_script));
    Ok(PreparedScript {
        script,
        exec_script,
        script_hash,
        typed_asks: typed_asks.len(),
        insight,
    })
}

/// 从 journal 恢复 artifact 最新版本（resume / fill 后不覆盖旧版本）。
fn load_artifact_versions(journal: &EventLog) -> HashMap<String, u32> {
    let mut versions: HashMap<String, u32> = HashMap::new();
    let Ok(events) = journal.load() else {
        return versions;
    };
    for (_, event) in events {
        if event.get("event").and_then(Value::as_str) != Some("artifact") {
            continue;
        }
        let Some(id) = event.get("id").and_then(Value::as_str) else {
            continue;
        };
        let version = event.get("version").and_then(Value::as_u64).unwrap_or(0) as u32;
        let entry = versions.entry(id.to_string()).or_insert(0);
        *entry = (*entry).max(version);
    }
    versions
}

/// 执行一段脚本，返回结构化结果（不写 run 终态；driver 负责事务收尾）。
pub(crate) async fn run_engine(request: EngineRequest, runtime: AgentToolRuntime) -> EngineOutcome {
    let EngineRequest {
        run_id,
        script,
        script_hash,
        script_generation,
        exec_generation,
        journal,
        cancel,
        max_run,
        max_parallel_asks,
        subagent_model,
        allowed_models,
        args,
        project_root,
        tool_access,
        store,
        paths,
        effects,
        replay,
        holes_allowed,
        inherited,
        retune_slot,
        progress,
    } = request;
    let Some(active_run) = ActiveRunGuard::acquire(&run_id) else {
        return EngineOutcome::Failed {
            error: format!("run '{run_id}' 正在执行中，不能并发执行同一个 run"),
            budget_exhausted: false,
        };
    };
    let permit_limit = max_parallel_asks.clamp(1, MAX_MAX_PARALLEL_ASKS as usize);
    let artifact_versions = load_artifact_versions(&journal);
    let _ = script_hash;
    let shared = Arc::new(EngineShared {
        engine: StdMutex::new(EngineState {
            replay,
            ..EngineState::default()
        }),
        permits: DynamicSemaphore::new(permit_limit),
        budget: Arc::new(SliceBudget::new()),
        halt: Arc::new(AtomicBool::new(false)),
        run_id: run_id.clone(),
        journal,
        cancel,
        outer: tokio::runtime::Handle::current(),
        inherited,
        _active_run: active_run,
        store,
        effects,
        paths,
        args,
        project_root,
        tool_access,
        allowed_models: Arc::new(StdMutex::new(allowed_models)),
        subagent_model: Arc::new(StdMutex::new(subagent_model)),
        script_generation,
        exec_generation,
        hole: StdMutex::new(None),
        hole_seq: AtomicU64::new(0),
        hole_name_seq: StdMutex::new(HashMap::new()),
        holes_opened: StdMutex::new(HashSet::new()),
        question_seq: Arc::new(AtomicU64::new(0)),
        progress,
        holes_allowed,
        log_ring: StdMutex::new(VecDeque::new()),
        log_count: AtomicU64::new(0),
        log_bytes: AtomicU64::new(0),
        log_truncated: AtomicBool::new(false),
        report_bytes: AtomicU64::new(0),
        report_count: AtomicU64::new(0),
        phase: StdMutex::new(None),
        phase_count: AtomicU64::new(0),
        phase_truncated: AtomicBool::new(false),
        artifact_versions: StdMutex::new(artifact_versions),
        pending_hosts: Arc::new(AtomicUsize::new(0)),
        effect_permits: Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_EFFECTS)),
        unresolved_disclosures: StdMutex::new(Vec::new()),
    });
    *retune_slot
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(RetuneHandle {
        permits: Arc::clone(&shared.permits),
        subagent_model: Arc::clone(&shared.subagent_model),
        allowed_models: Arc::clone(&shared.allowed_models),
    });
    let engine_runtime = runtime.clone();
    let engine_shared = Arc::clone(&shared);
    let engine_task = tokio::task::spawn_blocking(move || {
        let inner = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                EngineAbort::Script(format!("workflow 内嵌 runtime 创建失败: {error}"))
            })?;
        inner.block_on(drive_engine(script, max_run, engine_runtime, engine_shared))
    });
    // 外层取消（Cancel / amend 取代 / 同步模式父 turn）到达引擎令牌后：宽限期让在途子代理经
    // 令牌收尾、让 try/catch 住失败 ask 的脚本走完；到期置 halt，中断处理器在下一个轮询点
    // 终止 JS——内嵌 runtime 里的 select 在紧循环期间得不到 poll，这个看门狗必须在外层。
    let halt_watch = {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            shared.cancel.cancelled().await;
            tokio::time::sleep(CANCEL_GRACE).await;
            shared.halt.store(true, Ordering::SeqCst);
        })
    };
    let hard_deadline = max_run + ENGINE_SLICE_BUDGET + CANCEL_GRACE;
    let executed = match tokio::time::timeout(hard_deadline, engine_task).await {
        Ok(executed) => executed,
        Err(_elapsed) => {
            shared.cancel.cancel();
            shared.halt.store(true, Ordering::SeqCst);
            Ok(Err(EngineAbort::Budget(format!(
                "workflow 引擎超过墙钟预算 {}s 后仍未返回（脚本可能卡在不可中断的同步操作，\
                 如灾难性回溯的正则），已放弃等待",
                max_run.as_secs()
            ))))
        }
    };
    halt_watch.abort();
    let outcome = match executed {
        Ok(outcome) => outcome,
        Err(error) => Err(EngineAbort::Script(format!(
            "workflow 引擎线程失败: {error}"
        ))),
    };
    let (asks_total, asks_replayed, final_result, hole) = {
        let engine = shared.engine.lock().unwrap_or_else(|p| p.into_inner());
        let hole = shared
            .hole
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        (
            engine.asks_total,
            engine.asks_replayed,
            engine.result.clone(),
            hole,
        )
    };
    // 墙钟耗尽压过 hole：预算已用完的 run 不能停在 hole 上装作还能续（Fill 后新代次一启动
    // 就 budget_exhausted）。取消则仍按 HoleWaiting 交给 driver——Fill 回退旧 exec 就是靠
    // 取消令牌让它退出，run 要继续停在 hole 上。
    let budget_exhausted = matches!(outcome, Err(EngineAbort::Budget(_)));
    if let (Some(hole), false) = (hole, budget_exhausted) {
        // hole 暂停前等在途 ask 结算（子代理不能跨 exec 代次继续跑）。
        for _ in 0..200 {
            if shared.progress.active_agents.load(Ordering::SeqCst) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        return EngineOutcome::HoleWaiting { hole };
    }
    let returned = match outcome {
        Err(
            EngineAbort::Cancelled(error) | EngineAbort::Budget(error) | EngineAbort::Script(error),
        ) => Err(error),
        Ok(_) if shared.budget.interrupted() => Err(format!(
            "脚本单片 CPU 预算（{}s）耗尽被中断",
            ENGINE_SLICE_BUDGET.as_secs()
        )),
        Ok(None) => Err("脚本未结算（挂在永不 resolve 的 promise 上）".to_string()),
        Ok(Some(ScriptOutcome::Threw(error))) => Err(format!("ERROR: {error}")),
        Ok(Some(ScriptOutcome::Returned(value))) => Ok(value),
    };
    match returned {
        Ok(value) => EngineOutcome::Completed {
            result: final_result.or(value),
            asks_total,
            asks_replayed,
            unresolved_disclosures: shared
                .unresolved_disclosures
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone(),
        },
        Err(error) => EngineOutcome::Failed {
            error: format!(
                "{error}\n（journal 已保留已完成问答：{}；同一脚本配 resume_run_id 重提可续跑）",
                shared.journal.locator()
            ),
            budget_exhausted,
        },
    }
}

/// 从 journal 读取某 run 的脚本 hash 与重放表（driver 启动前调用）。
pub(crate) fn journal_replay(journal: &EventLog) -> Result<(u64, ReplayTable), String> {
    load_workflow_journal(journal)
}

/// `EvalWorkflowSnippet`：与真实 run 同一编译器 / 沙箱，但无 agent/report/artifact/hole/phase，
/// 不落 journal、不建 run。`world.run` 仅 Unrestricted。
pub(crate) async fn eval_snippet(
    code: String,
    timeout: Duration,
    tool_access: ToolAccess,
    project_root: PathBuf,
) -> Result<Value, String> {
    // 取消令牌挂在父 turn 上：`/cancel` 要能停掉片段里的 world.run；外层再加硬超时——内嵌
    // runtime 里不可中断的同步操作（灾难性回溯的正则、挂住的 git）会让里面的 timeout 永远
    // 得不到 poll，没有外层超时这个工具调用就永久挂起（线程泄漏与 run_engine 同口径）。
    let cancel = rclaw_tools::turn_state::turn_cancel()
        .map(|parent| parent.child_token())
        .unwrap_or_default();
    let worker_cancel = cancel.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let inner = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("片段内嵌 runtime 创建失败: {error}"))?;
        inner.block_on(eval_snippet_inner(
            code,
            timeout,
            tool_access,
            project_root,
            worker_cancel,
        ))
    });
    match tokio::time::timeout(timeout + CANCEL_GRACE + Duration::from_secs(5), worker).await {
        Ok(joined) => joined.map_err(|error| format!("片段执行线程失败: {error}"))?,
        Err(_elapsed) => {
            cancel.cancel();
            Err(format!(
                "片段超过 {}ms 仍未结算（可能卡在不可中断的同步操作上），已放弃",
                timeout.as_millis()
            ))
        }
    }
}

async fn eval_snippet_inner(
    code: String,
    timeout: Duration,
    tool_access: ToolAccess,
    project_root: PathBuf,
    cancel: CancellationToken,
) -> Result<Value, String> {
    if code.chars().count() > MAX_SCRIPT_CHARS {
        return Err(format!("code 超过 {MAX_SCRIPT_CHARS} 字符"));
    }
    // §5.10：与真实 run 同一编译器——语法 / 禁用模式诊断按 `{result:null, logs:[], diagnostics}`
    // 结构化回给模型（不是一段错误文本），不进入执行。
    let diagnostics = check_script(&code, false);
    if !diagnostics.is_empty() {
        return Ok(json!({
            "result": Value::Null,
            "logs": [],
            "diagnostics": diagnostics
                .iter()
                .map(|diagnostic| json!({
                    "line": diagnostic.line,
                    "col": diagnostic.col,
                    "message": diagnostic.message,
                }))
                .collect::<Vec<_>>(),
        }));
    }
    // `world.run` 仅 Unrestricted：静态可见的调用直接 `forbidden_world`，不跑片段；经别名绕过
    // 静态扫描的在运行期被 world_run 拒绝，脚本抛出的错误同样映射成该码（见 Threw 分支）。
    if tool_access != ToolAccess::Unrestricted
        && mask_strings_and_comments(&code).contains("world.run")
    {
        return Err("forbidden_world: world.run 仅 Unrestricted 会话可用".to_string());
    }
    let budget = Arc::new(SliceBudget::with_slice(timeout.max(Duration::from_secs(1))));
    let halt = Arc::new(AtomicBool::new(false));
    let result_slot: Arc<StdMutex<Option<String>>> = Arc::default();
    let logs: Arc<StdMutex<Vec<String>>> = Arc::default();
    let outer_cancel = cancel.clone();
    let hosts = EvalHosts {
        result: Arc::clone(&result_slot),
        logs: Arc::clone(&logs),
        root: project_root.clone(),
        cancel: cancel.clone(),
        tool_access,
        world_timeout: timeout.min(Duration::from_secs(300)),
        pending: Arc::new(AtomicUsize::new(0)),
        effect_permits: Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_EFFECTS)),
    };
    let drive = eval_deterministic(code, Arc::clone(&budget), halt, json!({}), move |ctx| {
        register_eval_hosts(ctx, hosts)
    });
    let outcome = tokio::select! {
        biased;
        _ = outer_cancel.cancelled() => return Err("片段已随 turn 取消".to_string()),
        driven = tokio::time::timeout(timeout + CANCEL_GRACE, drive) => driven
            .map_err(|_| format!("片段超过 {}ms 未结算", timeout.as_millis()))??,
    };
    if let Some(ScriptOutcome::Threw(error)) = &outcome {
        if error.contains("forbidden_world") {
            return Err(format!("forbidden_world: {error}"));
        }
    }
    let result = result_slot
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
        .or_else(|| match outcome {
            Some(ScriptOutcome::Returned(value)) => value,
            Some(ScriptOutcome::Threw(error)) => Some(format!("ERROR: {error}")),
            None => None,
        });
    let result = result
        .map(|text| Value::String(truncate_chars(&text, 256 * 1024)))
        .unwrap_or(Value::Null);
    let logs = logs
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    Ok(json!({
        "result": result,
        "logs": logs,
        "diagnostics": [],
    }))
}

/// `EvalWorkflowSnippet` 的宿主面：result / log / files / git / world，其余 facade 返回不支持。
struct EvalHosts {
    result: Arc<StdMutex<Option<String>>>,
    logs: Arc<StdMutex<Vec<String>>>,
    root: PathBuf,
    cancel: CancellationToken,
    tool_access: ToolAccess,
    world_timeout: Duration,
    /// 在途 async 宿主调用计数（与正式引擎同一上限，见 [`MAX_PENDING_HOST_CALLS`]）。
    pending: Arc<AtomicUsize>,
    /// 同时执行的 files / git / world 上限（与正式引擎同一上限，见 [`MAX_INFLIGHT_EFFECTS`]）。
    effect_permits: Arc<tokio::sync::Semaphore>,
}

/// 与 [`register_workflow_hosts`] 同一套入参纪律：字符串句柄到 future 里再转、同步段领名额。
fn register_eval_hosts<'js>(ctx: &rquickjs::Ctx<'js>, hosts: EvalHosts) {
    let EvalHosts {
        result,
        logs,
        root,
        cancel,
        tool_access,
        world_timeout,
        pending,
        effect_permits,
    } = hosts;
    let result_fn = Function::new(ctx.clone(), move |text: String| -> String {
        *result.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(text);
        "ok".to_string()
    })
    .expect("注册 __result");
    ctx.globals()
        .set("__result", result_fn)
        .expect("set __result");
    let log_fn = Function::new(ctx.clone(), move |message: String| -> String {
        let mut logs = logs.lock().unwrap_or_else(|poison| poison.into_inner());
        if logs.len() < 500 {
            logs.push(truncate_chars(&message, 4_000));
        }
        "ok".to_string()
    })
    .expect("注册 __wf_log");
    ctx.globals().set("__wf_log", log_fn).expect("set __wf_log");
    let unsupported = Function::new(ctx.clone(), move || -> String {
        json!({"ok": false, "error": "EvalWorkflowSnippet 不支持该 facade"}).to_string()
    })
    .expect("注册 unsupported");
    for name in [
        "__ask_raw",
        "__ask_json_raw",
        "__wf_report",
        "__wf_phase",
        "__wf_artifact",
        "__wf_hole",
    ] {
        ctx.globals()
            .set(name, unsupported.clone())
            .expect("set unsupported");
    }
    let files_root = root.clone();
    let files_pending = Arc::clone(&pending);
    let files_permits = Arc::clone(&effect_permits);
    let files_cancel = cancel.clone();
    let files_fn = Function::new(
        ctx.clone(),
        AsyncFn(
            move |op: rquickjs::String<'js>, payload: rquickjs::String<'js>| {
                let root = files_root.clone();
                let permits = Arc::clone(&files_permits);
                let cancel = files_cancel.clone();
                let admitted = HostSlot::acquire(&files_pending).map(|slot| (slot, op, payload));
                async move {
                    let Some((_slot, op, payload)) = admitted else {
                        return host_backlog_error();
                    };
                    let (op, payload) = match (js_string(&op), js_string(&payload)) {
                        (Ok(op), Ok(payload)) => (op, payload),
                        (Err(error), _) | (_, Err(error)) => return err_envelope(error),
                    };
                    let args = match parse_host_payload(&payload) {
                        Ok(args) => args,
                        Err(error) => return err_envelope(format!("files.{op} {error}")),
                    };
                    let Some(_permit) = acquire_effect_permit(&permits, &cancel).await else {
                        return err_envelope("片段已随 turn 取消");
                    };
                    let result = match op.as_str() {
                        "glob" => workflow_world::files_glob(
                            &root,
                            args.get("pattern")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        ),
                        "read" => workflow_world::files_read(
                            &root,
                            args.get("path").and_then(Value::as_str).unwrap_or_default(),
                        ),
                        "grep" => workflow_world::files_grep(
                            &root,
                            args.get("pattern")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            args.get("glob").and_then(Value::as_str),
                        ),
                        other => Err(format!("未知 files 操作: {other}")),
                    };
                    match result {
                        Ok(value) => ok_envelope(value),
                        Err(error) => err_envelope(error),
                    }
                }
            },
        ),
    )
    .expect("注册 __wf_files");
    ctx.globals()
        .set("__wf_files", files_fn)
        .expect("set __wf_files");
    let git_root = root.clone();
    let git_pending = Arc::clone(&pending);
    let git_permits = Arc::clone(&effect_permits);
    let git_cancel = cancel.clone();
    let git_fn = Function::new(
        ctx.clone(),
        AsyncFn(
            move |op: rquickjs::String<'js>, payload: rquickjs::String<'js>| {
                let root = git_root.clone();
                let permits = Arc::clone(&git_permits);
                let cancel = git_cancel.clone();
                let admitted = HostSlot::acquire(&git_pending).map(|slot| (slot, op, payload));
                async move {
                    let Some((_slot, op, payload)) = admitted else {
                        return host_backlog_error();
                    };
                    let (op, payload) = match (js_string(&op), js_string(&payload)) {
                        (Ok(op), Ok(payload)) => (op, payload),
                        (Err(error), _) | (_, Err(error)) => return err_envelope(error),
                    };
                    let args = match parse_host_payload(&payload) {
                        Ok(args) => args,
                        Err(error) => return err_envelope(format!("git.{op} {error}")),
                    };
                    let Some(_permit) = acquire_effect_permit(&permits, &cancel).await else {
                        return err_envelope("片段已随 turn 取消");
                    };
                    let result = match op.as_str() {
                        "status" => workflow_world::git_status(&root),
                        "diff" => workflow_world::git_diff(
                            &root,
                            args.get("path").and_then(Value::as_str),
                        ),
                        other => Err(format!("未知 git 操作: {other}")),
                    };
                    match result {
                        Ok(value) => ok_envelope(value),
                        Err(error) => err_envelope(error),
                    }
                }
            },
        ),
    )
    .expect("注册 __wf_git");
    ctx.globals().set("__wf_git", git_fn).expect("set __wf_git");
    let world_fn = Function::new(
        ctx.clone(),
        AsyncFn(move |payload: rquickjs::String<'js>| {
            let root = root.clone();
            let cancel = cancel.clone();
            let permits = Arc::clone(&effect_permits);
            let admitted = HostSlot::acquire(&pending).map(|slot| (slot, payload));
            async move {
                let Some((_slot, payload)) = admitted else {
                    return host_backlog_error();
                };
                let payload = match js_string(&payload) {
                    Ok(payload) => payload,
                    Err(error) => return err_envelope(error),
                };
                let args = match parse_host_payload(&payload) {
                    Ok(args) => args,
                    Err(error) => return err_envelope(format!("world.run {error}")),
                };
                let command = args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let opts = args.get("opts").cloned().unwrap_or_else(|| json!({}));
                let Some(_permit) = acquire_effect_permit(&permits, &cancel).await else {
                    return err_envelope("片段已随 turn 取消");
                };
                match workflow_world::world_run(
                    &root,
                    tool_access,
                    &command,
                    &opts,
                    &cancel,
                    world_timeout,
                    |_, _| {},
                )
                .await
                {
                    Ok(outcome) => ok_envelope(outcome.value),
                    Err(error) => err_envelope(error),
                }
            }
        }),
    )
    .expect("注册 __wf_world");
    ctx.globals()
        .set("__wf_world", world_fn)
        .expect("set __wf_world");
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
            graph
                .edges
                .iter()
                .any(|(from, to)| from == "ask-3" && to == "ask-5"),
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
        assert!(
            graph.nodes[..3].iter().all(|n| n.parallel),
            "跨行波内节点应并行"
        );
        assert!(!graph.nodes[3].parallel);
        assert_eq!(graph.waves.len(), 2, "{:?}", graph.waves);
        assert_eq!(graph.waves[0].len(), 3);
        assert_eq!(
            graph.edges.len(),
            3,
            "波内每个节点 → 后续 ask: {:?}",
            graph.edges
        );
        assert_eq!(analyze_workflow_script(script).ask_count, 4);
    }

    /// 经 `__result` 回传脚本观测值（async IIFE + Promise 全链路）。
    async fn observe(script: &str) -> (Result<Option<ScriptOutcome>, String>, String) {
        let received: Arc<StdMutex<String>> = Arc::new(StdMutex::new(String::new()));
        let sink = Arc::clone(&received);
        let outcome = eval_deterministic(
            script.to_string(),
            Arc::new(SliceBudget::new()),
            Arc::new(AtomicBool::new(false)),
            json!({}),
            move |ctx| {
                let sink = Arc::clone(&sink);
                let result_fn = Function::new(ctx.clone(), move |text: String| -> String {
                    *sink.lock().unwrap_or_else(|p| p.into_inner()) = text;
                    "ok".to_string()
                })
                .expect("注册 __result");
                ctx.globals()
                    .set("__result", result_fn)
                    .expect("set __result");
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
        assert!(
            matches!(outcome, Ok(Some(ScriptOutcome::Returned(None)))),
            "{outcome:?}"
        );
        assert_eq!(
            observed, "[0,0.5,0,\"1970-01-01T00:00:00.000Z\",true,0,0,0,0,5,true]",
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
        assert!(
            check_script(script, false).is_empty(),
            "lint 不应命中字符串内的 eval"
        );
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
            Arc::new(AtomicBool::new(false)),
            json!({}),
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
    async fn self_rescheduling_microtask_chain_is_interrupted() {
        // 不含循环的自我重排 microtask 链：不经过中断处理器的轮询点，靠 job 之间的片预算检查中断。
        let budget = Arc::new(SliceBudget::with_slice(Duration::from_millis(200)));
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            eval_deterministic(
                "function tick() { Promise.resolve().then(tick); } tick(); await new Promise(function () {});"
                    .to_string(),
                Arc::clone(&budget),
                Arc::new(AtomicBool::new(false)),
                json!({}),
                |_ctx| {},
            ),
        )
        .await
        .expect("自我重排的 microtask 链必须在片预算内被中断，而不是让引擎永不返回");
        assert!(budget.interrupted(), "microtask 链必须被单片预算中断");
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

    #[tokio::test]
    async fn top_level_return_value_is_json_for_objects_and_raw_for_strings() {
        // 顶层 `return {..}` 以 JSON 落库（ZCode 同源），而不是 "[object Object]"；字符串原样。
        let (outcome, _) = observe("return { ok: true, answer: 42 };").await;
        assert!(
            matches!(&outcome, Ok(Some(ScriptOutcome::Returned(Some(value)))) if value == r#"{"ok":true,"answer":42}"#),
            "{outcome:?}"
        );
        let (outcome, _) = observe("return 'plain text';").await;
        assert!(
            matches!(&outcome, Ok(Some(ScriptOutcome::Returned(Some(value)))) if value == "plain text"),
            "{outcome:?}"
        );
        let (outcome, _) = observe("return;").await;
        assert!(
            matches!(&outcome, Ok(Some(ScriptOutcome::Returned(None)))),
            "{outcome:?}"
        );
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
        assert!(load_workflow_journal(&missing)
            .unwrap_err()
            .contains("不存在"));
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
        let _plan = ActiveRunGuard::acquire_plan("guard-test-run")
            .expect("plan 键带前缀，不与 workflow 冲突");
        assert!(
            ActiveRunGuard::acquire_plan("guard-test-run").is_none(),
            "同一 plan_id 不能并发持有"
        );
        drop(first);
        assert!(
            ActiveRunGuard::acquire("guard-test-run").is_some(),
            "Drop 即释放"
        );
    }

    // ---- 端到端（最小 runtime + 计数 provider）----

    /// 计数 provider：第 n 次调用答 "#n"——同问两次得到不同答案，可验证重放按调用点精确配对。
    /// `hang_once` 给定时，问题含「挂起」的首次调用不计数、挂住直到被取消并置位该标记
    /// （用来制造 stopped run）。
    struct CountingProvider {
        calls: Arc<AtomicU64>,
        hang_once: Option<(Arc<AtomicBool>, AtomicBool)>,
    }

    #[async_trait]
    impl Provider for CountingProvider {
        fn name(&self) -> &str {
            "counting"
        }

        async fn chat(&self, req: ChatRequest) -> rclaw_llm::Result<rclaw_core::ChatOutcome> {
            if let Some((hung, armed)) = &self.hang_once {
                let question = req
                    .messages
                    .iter()
                    .find(|message| message.role == rclaw_core::Role::User)
                    .and_then(|message| {
                        message.content.iter().find_map(|block| match block {
                            rclaw_core::ContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                    })
                    .unwrap_or_default();
                if question.contains("挂起") && armed.swap(false, Ordering::SeqCst) {
                    hung.store(true, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                }
            }
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
        WorkflowRunTool::new(service_with_provider(provider))
    }

    fn service_with_provider(provider: Arc<dyn Provider>) -> super::super::WorkflowService {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(dir.path().join("agent"));
        std::mem::forget(dir);
        let registry: RegistryLink = Arc::new(OnceLock::new());
        registry
            .set(Arc::new(ToolRegistry::new()))
            .unwrap_or_else(|_| panic!("registry 只初始化一次"));
        let runtime = AgentToolRuntime::new(
            provider,
            MemoryStore::new(layout.clone()),
            layout.clone(),
            JournalStore::open(&layout).expect("open state.sqlite"),
            AgentOptions::default(),
            registry,
            rclaw_core::RuntimeChannelInfo::default(),
            None,
        );
        // 同步模式 + 替 `cli/console` 测试会话预登记技能门（等价于完整读过 SKILL.md）。
        let project_root = super::super::workflow_paths::canonical_project_root(&layout.base);
        super::super::workflow_skill::ensure_skill_file(&project_root).expect("安装技能文件");
        let hash = super::super::workflow_skill::file_hash(
            &super::super::workflow_skill::skill_path(&project_root),
        )
        .expect("技能文件 hash");
        runtime
            .journal_store()
            .kv_set(
                "workflow-skill/cli/console",
                &json!({
                    "content_sha256": hash,
                    "contract_version": super::super::workflow_skill::CONTRACT_VERSION,
                    "session_key": "cli/console",
                    "read_at": rclaw_store::db::now_ms(),
                }),
            )
            .expect("登记技能门");
        super::super::WorkflowService::new(
            runtime,
            &layout,
            super::super::WorkflowOptions {
                background: false,
                legacy_tool: true,
                max_active: 4,
                command_confirm: false,
                ..Default::default()
            },
        )
    }

    fn counting_tool() -> (WorkflowRunTool, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        let tool = tool_with_provider(Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
            hang_once: None,
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
        assert_eq!(
            first["result"], "#1|#2",
            "同问两次是两个调用点，各自真实执行"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // 重放按调用点配对：同问两次真实执行（#1、#2）后在第三问挂住时取消 → stopped；
        // resume 时 a=#1、b=#2 不互换，第三问真实执行（#3）。
        let calls = Arc::new(AtomicU64::new(0));
        let hung = Arc::new(AtomicBool::new(false));
        let service = service_with_provider(Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
            hang_once: Some((Arc::clone(&hung), AtomicBool::new(true))),
        }));
        let script = concat!(
            "const coder = agent(\"coder\");\n",
            "const a = await coder.ask(\"同一个问题\");\n",
            "const b = await coder.ask(\"同一个问题\");\n",
            "const c = await coder.ask(\"挂起\");\n",
            "result(a + \"|\" + b + \"|\" + c);\n"
        );
        let run_id = super::super::tests::stop_run_at_hang(
            &service,
            json!({"script": script, "type_check": false}),
            &hung,
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let tool = WorkflowRunTool::new(service);
        let second =
            run_bounded(&tool, json!({"resume_run_id": run_id, "type_check": false})).await;
        assert_eq!(second["asks_replayed"], 2);
        assert_eq!(second["result"], "#1|#2|#3", "重放按调用点配对，答案不互换");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "重放的问答零 provider 调用"
        );
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
        assert!(
            diagnostics.iter().any(|d| d.message.contains("Date.now")),
            "{diagnostics:?}"
        );
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
        assert!(
            diagnose(prompt_text).is_empty(),
            "{:?}",
            diagnose(prompt_text)
        );
        // 间接 eval / 函数式 Date() / Function 构造 / 字符串转义不打断遮蔽。
        let indirect = "const s = \"a\\\"b\"; const e = (0, eval)(s); const t = Date(); const f = new Function(\"x\");";
        let diagnostics = diagnose(indirect);
        assert!(
            diagnostics.iter().any(|d| d.message.contains("eval")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics.iter().any(|d| d.message.contains("Date()")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics.iter().any(|d| d.message.contains("Function")),
            "{diagnostics:?}"
        );
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
        assert!(
            validate_json_schema(&json!("x"), &unknown, 0).is_err(),
            "未知类型名必须报错"
        );
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
        assert_eq!(
            insight.actors,
            vec!["coder".to_string(), "reviewer".to_string()]
        );
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
        assert!(
            validate_json_schema(&json!({"zone": "a", "tags": ["x", "x"]}), &compiled, 0).is_ok()
        );
        assert!(validate_json_schema(&json!({"zone": "c"}), &compiled, 0).is_err());
        assert!(validate_json_schema(&json!({"zone": "a", "tags": ["y"]}), &compiled, 0).is_err());
    }

    #[test]
    fn validator_requires_every_allof_branch() {
        // allOf：交叉类型的属性级交集（`{v:"ok"} & {v:string}`）——每个分支都要通过，
        // 只满足其一不放行。
        let intersection = json!({
            "type": "object",
            "properties": {
                "v": {"allOf": [{"const": "ok"}, {"type": "string"}]}
            },
            "required": ["v"]
        });
        assert!(validate_json_schema(&json!({"v": "ok"}), &intersection, 0).is_ok());
        assert!(
            validate_json_schema(&json!({"v": "wrong"}), &intersection, 0).is_err(),
            "只满足 string 分支不得放行（字面量约束不能被放宽）"
        );
        // 嵌套对象里的 allOf 照样逐分支核对。
        let nested = json!({
            "type": "object",
            "properties": {
                "inner": {"allOf": [{"type": "object", "properties": {"a": {"const": 1}}, "required": ["a"]}]}
            }
        });
        assert!(validate_json_schema(&json!({"inner": {"a": 1}}), &nested, 0).is_ok());
        assert!(validate_json_schema(&json!({"inner": {"a": 2}}), &nested, 0).is_err());
    }

    #[test]
    fn anyof_success_still_checks_sibling_constraints() {
        // 回归：anyOf 命中后提前 return，把同级 type / required / properties 整个跳掉
        // ——`{"type":"object","required":["host"],"anyOf":[{}]}` 曾放行 `{}`。
        let schema = json!({
            "type": "object",
            "required": ["host"],
            "properties": {"host": {"type": "string"}},
            "anyOf": [{"type": "object"}]
        });
        assert!(validate_json_schema(&json!({"host": "h"}), &schema, 0).is_ok());
        assert!(
            validate_json_schema(&json!({}), &schema, 0).is_err(),
            "anyOf 分支通过不等于整个 schema 通过"
        );
        assert!(
            validate_json_schema(&json!({"host": 1}), &schema, 0).is_err(),
            "同级 properties 约束仍然生效"
        );
        // 无分支通过照旧整体拒绝。
        assert!(validate_json_schema(&json!("标量"), &schema, 0).is_err());
    }

    #[test]
    fn numeric_consts_and_enums_compare_by_value() {
        // 回归：serde_json 的 Value 相等区分整数 / 浮点表示（1 ≠ 1.0），JSON Schema
        // 的数值相等是数学值——ask<1> 的答案序列化成 1.0 / 1e0 曾被误拒。
        assert!(validate_json_schema(&json!(1.0), &json!({"const": 1}), 0).is_ok());
        assert!(validate_json_schema(&json!(1e0), &json!({"const": 1}), 0).is_ok());
        assert!(validate_json_schema(&json!(2.0), &json!({"const": 1}), 0).is_err());
        let enum_schema = json!({"enum": [1, 2]});
        assert!(validate_json_schema(&json!(2.0), &enum_schema, 0).is_ok());
        assert!(validate_json_schema(&json!(3.0), &enum_schema, 0).is_err());
        // 字符串等非数值不受影响（"1" 仍不等 1）。
        assert!(validate_json_schema(&json!("1"), &json!({"const": 1}), 0).is_err());
        // 回归：2^53 以上的整数走精确比较——f64 转换曾把相邻大整数合并成相等，
        // const / enum 校验被静默绕过。
        let big_const = json!({"const": 9007199254740992i64});
        assert!(validate_json_schema(&json!(9007199254740992i64), &big_const, 0).is_ok());
        assert!(
            validate_json_schema(&json!(9007199254740993i64), &big_const, 0).is_err(),
            "相邻大整数不得被 f64 转换合并"
        );
        // 跨表示的同一整数值（i64 vs 浮点形态）仍按数学值相等放行。
        assert!(
            validate_json_schema(&json!(9007199254740992.0f64), &big_const, 0).is_ok(),
            "整数与它的浮点形态相等"
        );
        // 回归：混合表示（精确整数常量 vs 整数值浮点答案）同样精确——常量转 f64 曾
        // 被舍入，两个不同的数学值被判相等。
        let bigger_const = json!({"const": 9007199254740993i64});
        assert!(
            validate_json_schema(&json!(9007199254740992.0f64), &bigger_const, 0).is_err(),
            "常量 ...993 不得接受答案 ...992.0（常量侧转 f64 曾被舍入成 ...992）"
        );
    }

    #[test]
    fn compound_consts_compare_members_recursively() {
        // 回归：嵌套 const / enum 曾退回结构相等——{"score":1} 拒绝 {"score":1.0}，
        // 违反 JSON Schema 的数值相等语义；逐成员递归用同一数值规则。
        let schema = json!({
            "type": "object",
            "properties": {
                "result": {"const": {"score": 1, "tags": ["a", 2]}}
            },
            "required": ["result"]
        });
        assert!(validate_json_schema(
            &json!({"result": {"score": 1.0, "tags": ["a", 2.0]}}),
            &schema,
            0
        )
        .is_ok());
        assert!(validate_json_schema(
            &json!({"result": {"score": 2, "tags": ["a", 2]}}),
            &schema,
            0
        )
        .is_err());
        assert!(
            validate_json_schema(&json!({"result": {"score": 1, "tags": ["a"]}}), &schema, 0)
                .is_err(),
            "数组长度不同不得相等"
        );
        // 键集合不同不得相等。
        assert!(validate_json_schema(&json!({"result": {"score": 1}}), &schema, 0).is_err());
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
