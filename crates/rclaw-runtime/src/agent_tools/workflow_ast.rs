//! 工作流脚本的 AST 级分析（oxc 解析，第十二轮）。
//!
//! 第十一轮的依赖图 / 静态洞察是语句级文本解析：字符串与注释里的 `.ask(` 会误报、
//! 变量提升的 `Promise.all` 扇出识别不了、inline `agent("x").ask()`（无句柄绑定）拿不到
//! actor。本模块用 oxc 把脚本解析成真 AST（零语义推断，只看语法形状），升级两件事：
//!
//! 1. **AST 依赖图** [`build_dependency_graph`]：ask 调用点 = `MemberExpression(ask |
//!    askJson)` 调用，receiver 经「句柄绑定 / 直接 agent() 调用」解析 actor；
//!    `Promise.all([...])` 的数组元素（直接 ask 或持有 ask promise 的标识符）合并成
//!    并行波；波与波之间按源序连「前波出口 → 后波入口」边（与第十一轮文本版同一套
//!    边语义）。解析失败（oxc 不接受的语法）返回 `None`，调用方降级回文本解析——
//!    分析永不阻塞提交。
//! 2. **`.ask<T>` 泛型到 schema 的编译** [`compile_typed_asks`]：`h.ask<{a: string}>("q")`
//!    的内联类型注解编译成 JSON Schema（子集），调用点重写为
//!    `h.askJson("q", <schema>)` 后交给 QuickJS 执行——执行引擎不变，答案按 schema
//!    校验。类型别名（`type X = {...}`，无泛型）在脚本内解析；不支持的类型给出带
//!    行列的诊断。script hash 仍按【作者原文】计算（resume 的逐字节一致语义不变）。
//!
//! 脚本在引擎里被包进 async IIFE 执行；本模块解析时同样包一层
//! （顶层 `await` / `return` 在函数体内才合法），行号减一、字节偏移去头。

use std::collections::HashMap;

use oxc_allocator::Allocator;
use oxc_allocator::CloneIn;
use oxc_ast::ast::*;
use oxc_ast_visit::walk::{walk_call_expression, walk_variable_declarator};
use oxc_ast_visit::Visit;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use serde_json::{json, Value};

use super::workflow::{DependencyGraph, DependencyNode, FacadeDiagnostic};

/// 包裹头：与执行引擎的 async IIFE 语义一致（顶层 await / return 合法）。
const WRAPPER_HEAD: &str = "(async function __wf__(){\n";
const WRAPPER_TAIL: &str = "\n})";
/// 类型嵌套深度上限（与 Rust 侧 schema 校验器的深度上限同源：8）。编译完成后按
/// 校验器口径核对展开后的 schema（[`schema_depth`]），这是唯一与校验器对齐的判定。
const MAX_TYPE_DEPTH: usize = 8;
/// 类型别名解析深度上限（防 `type A = B; type B = A;` 循环）。
const MAX_ALIAS_DEPTH: usize = 8;
/// schema 编译的语法递归护栏：每条 compile 递归边 +1（别名实例化的实参、体、括号、
/// 联合成员各占一层，并非 JSON 递归边）。只防失控递归，不承担「与校验器同口径」的
/// 职责——按 MAX_TYPE_DEPTH 直接卡语法层曾把 Box×8 这种校验器放行的嵌套多拒掉
///（8 层实参 + 体 + 属性 = 10 层语法递归）。取 4 倍留足别名 / 括号 / 联合的开销，
/// 展开后仍超 8 层的由 [`schema_depth`] 复核拒绝。
const MAX_COMPILE_RECURSION: usize = MAX_TYPE_DEPTH * 4;
/// 脚本嵌套深度上限（`([{` 一组、泛型 `<>` 一组）。oxc 是纯递归下降解析器，没有递归
/// 护栏；脚本来自模型（可被用户诱导），几 KB 的 `[[[[…]]]]` 就能在默认 2 MiB 的线程栈
/// 上溢出——Rust 栈溢出是 abort 不是 panic，整个进程退出。解析前线性预检拒绝超限脚本，
/// 解析本身再放到大栈专用线程（[`PARSE_STACK_BYTES`]）上，两道一起把递归深度钉死。
/// 正常脚本的嵌套远低于此（几层到十几层）。
const MAX_NESTING_DEPTH: usize = 256;
/// oxc 解析 + AST 遍历专用线程的栈大小（虚拟地址预留，按需提交物理页）。
const PARSE_STACK_BYTES: usize = 64 * 1024 * 1024;

/// 嵌套深度预检：字符串 / 注释遮蔽后，`([{` 与泛型 `<>` 的嵌套任一超过
/// [`MAX_NESTING_DEPTH`] 即返回带作者脚本行列的诊断。
///
/// `<>` 的计数是启发式（`<` 也是比较运算符）：`<=` / `>=` / `=>` 不计；`;` 把 `<>`
/// 计数清零——类型实参列表里不会出现 `;`（对象类型字面量的 `;` 伴随 `{}`，由括号计数
/// 兜住），而比较表达式总在语句结束前被清零，不会跨语句累加。`script` 必须是作者原文
///（不含包裹头尾），行列对应原脚本。
fn check_nesting_depth(script: &str) -> Result<(), FacadeDiagnostic> {
    let masked = super::workflow::mask_strings_and_comments(script);
    let bytes = masked.as_bytes();
    let (mut bracket, mut angle) = (0usize, 0usize);
    let mut i = 0usize;
    while i < bytes.len() {
        let exceeded = match bytes[i] {
            b'(' | b'[' | b'{' => {
                bracket += 1;
                bracket > MAX_NESTING_DEPTH
            }
            b')' | b']' | b'}' => {
                bracket = bracket.saturating_sub(1);
                false
            }
            b'<' if bytes.get(i + 1) == Some(&b'=') => {
                i += 1;
                false
            }
            b'<' => {
                angle += 1;
                angle > MAX_NESTING_DEPTH
            }
            b'>' if bytes.get(i + 1) == Some(&b'=') => {
                i += 1;
                false
            }
            b'>' => {
                if i == 0 || bytes[i - 1] != b'=' {
                    angle = angle.saturating_sub(1);
                }
                false
            }
            b';' => {
                angle = 0;
                false
            }
            _ => false,
        };
        if exceeded {
            let (line, col) = line_col(script, (i + WRAPPER_HEAD.len()) as u32);
            return Err(FacadeDiagnostic {
                line,
                col,
                message: format!(
                    "脚本嵌套过深：此处括号 / 泛型嵌套超过 {MAX_NESTING_DEPTH} 层，拆分表达式或类型后重新提交"
                ),
            });
        }
        i += 1;
    }
    Ok(())
}

/// 在大栈专用线程上执行 oxc 解析 + 遍历（见 [`PARSE_STACK_BYTES`]），结果带回当前线程。
/// 线程起不来（资源耗尽）返回 `None`，调用方按「无法分析」降级；闭包 panic 原样在当前
/// 线程重新抛出（与直接调用同语义）。
fn on_parse_stack<T: Send>(f: impl FnOnce() -> T + Send) -> Option<T> {
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("rclaw-workflow-ast".into())
            .stack_size(PARSE_STACK_BYTES)
            .spawn_scoped(scope, f)
            .ok()?;
        match handle.join() {
            Ok(value) => Some(value),
            Err(payload) => std::panic::resume_unwind(payload),
        }
    })
}

/// 解析包裹后的脚本（`wrapped` 由调用方持有，AST 与之同生命周期）。解析失败（含可恢复
/// 语法错误）与嵌套超限（[`check_nesting_depth`]）都返回 `None`：调用方降级（依赖图）
/// 或诊断（typed ask 编译在调用前已单独预检并给出带行列的诊断）。
fn parse<'a>(allocator: &'a Allocator, wrapped: &'a str) -> Option<Program<'a>> {
    // 预检只看作者原文：包裹头尾自带的 `(` `{` 会让恰在上限附近的脚本过了作者级预检
    //（compile_typed_asks 入口）却在这里被拒，两处口径不一致、诊断也丢了行列。
    let author = wrapped
        .strip_prefix(WRAPPER_HEAD)
        .and_then(|rest| rest.strip_suffix(WRAPPER_TAIL))
        .unwrap_or(wrapped);
    if check_nesting_depth(author).is_err() {
        return None;
    }
    let source_type = SourceType::mjs().with_typescript(true);
    let ret = Parser::new(allocator, wrapped, source_type).parse();
    if ret.panicked || !ret.errors.is_empty() {
        return None;
    }
    Some(ret.program)
}

/// 字节偏移（包裹文内）→ 作者脚本的 1 起行列。
fn line_col(script: &str, offset_in_wrapped: u32) -> (usize, usize) {
    let offset = (offset_in_wrapped as usize).saturating_sub(WRAPPER_HEAD.len());
    let before = &script[..offset.min(script.len())];
    let line = before.matches('\n').count() + 1;
    let col = before
        .rsplit('\n')
        .next()
        .map(|l| l.chars().count() + 1)
        .unwrap_or(1);
    (line, col)
}

// ---------------------------------------------------------------------------
// AST 依赖图
// ---------------------------------------------------------------------------

/// AST 版依赖图（第十一轮文本版的升级）。解析失败 / 嵌套超限 / 解析线程不可用都返回
/// `None`（调用方降级文本解析）。解析与遍历在大栈专用线程上执行（见 [`on_parse_stack`]）。
pub(crate) fn build_dependency_graph(script: &str) -> Option<DependencyGraph> {
    on_parse_stack(|| build_dependency_graph_inner(script)).flatten()
}

fn build_dependency_graph_inner(script: &str) -> Option<DependencyGraph> {
    let allocator = Allocator::default();
    let wrapped = format!("{WRAPPER_HEAD}{script}{WRAPPER_TAIL}");
    let program = parse(&allocator, &wrapped)?;
    let mut walker = GraphWalker {
        handles: HashMap::new(),
        promise_defs: HashMap::new(),
        nodes: Vec::new(),
        join_members: Vec::new(),
        cur_join: None,
        wrapped: &wrapped,
    };
    walker.visit_program(&program);

    // 分组：join 组（Promise.all 成员）按组内最小源偏移排，未入组的节点各自成串行组。
    let mut node_group: Vec<usize> = vec![usize::MAX; walker.nodes.len()];
    let mut groups: Vec<(Vec<usize>, bool)> = Vec::new(); // (成员下标, parallel)
    for members in &walker.join_members {
        // 复用的 promise（`const p = h.ask(..)` 进了两个 Promise.all）只归第一个组：
        // 同一 ask 节点进两组会产生组间自环边，Kahn 分层对环报不出波次。
        let mut unique: Vec<usize> = Vec::new();
        for &node_index in members {
            if node_index < node_group.len()
                && node_group[node_index] == usize::MAX
                && !unique.contains(&node_index)
            {
                unique.push(node_index);
            }
        }
        if unique.is_empty() {
            continue;
        }
        let group_index = groups.len();
        groups.push((unique.clone(), true));
        for &node_index in &unique {
            node_group[node_index] = group_index;
        }
    }
    for (index, group) in node_group.iter_mut().enumerate() {
        if *group == usize::MAX {
            *group = groups.len();
            groups.push((vec![index], false));
        }
    }
    // 组按成员最小【源偏移】稳定排列（单行脚本里所有组行号相同，按行排序会打平、
    // 插入序又把 join 组排到它前面的串行 ask 之前——偏移才是真实源序）。
    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by_key(|&g| {
        let (offset, line) = groups[g]
            .0
            .iter()
            .map(|&n| (walker.nodes[n].offset, walker.nodes[n].line))
            .min()
            .unwrap_or((0, 0));
        (offset, line)
    });

    // 边：相邻组全连（前组出口 → 后组入口）；join 组内部无边。
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut previous: Vec<String> = Vec::new();
    for &group_index in &order {
        let (members, _) = &groups[group_index];
        for &member in members {
            for from in &previous {
                edges.push((from.clone(), walker.nodes[member].id.clone()));
            }
        }
        previous = members
            .iter()
            .map(|&member| walker.nodes[member].id.clone())
            .collect();
    }

    // 波标记：join 组的成员 parallel=true；wave 序号按组序（观测用，边才是调度事实源）。
    for (group_index, (members, parallel)) in groups.iter().enumerate() {
        if *parallel {
            for &member in members {
                let node = &mut walker.nodes[member];
                node.parallel = true;
                node.wave = group_index + 1;
            }
        }
    }

    let waves = super::workflow::topo_waves(&walker.nodes, &edges);
    Some(DependencyGraph {
        nodes: walker.nodes,
        edges,
        waves,
    })
}

/// AST 版 actor 提取：任意位置的 `agent("name")` 调用，按首现序去重。失败返回 `None`。
/// 解析与遍历在大栈专用线程上执行（见 [`on_parse_stack`]）。
pub(crate) fn extract_actors(script: &str) -> Option<Vec<String>> {
    on_parse_stack(|| extract_actors_inner(script)).flatten()
}

fn extract_actors_inner(script: &str) -> Option<Vec<String>> {
    let allocator = Allocator::default();
    let wrapped = format!("{WRAPPER_HEAD}{script}{WRAPPER_TAIL}");
    let program = parse(&allocator, &wrapped)?;
    let mut walker = ActorWalker {
        actors: Vec::new(),
        wrapped: &wrapped,
    };
    walker.visit_program(&program);
    Some(walker.actors)
}

/// `agent("name")` 调用的 actor 名（非字符串字面量参数 / 非该形状 → None）。
fn actor_of_agent_call(call: &CallExpression<'_>) -> Option<String> {
    if !matches!(
        &call.callee,
        Expression::Identifier(id) if id.name == "agent"
    ) {
        return None;
    }
    match call
        .arguments
        .first()
        .and_then(|argument| argument.as_expression())
    {
        Some(Expression::StringLiteral(literal)) => {
            let name = literal.value.as_str();
            (!name.is_empty()).then(|| name.to_string())
        }
        _ => None,
    }
}

/// 表达式主体是否就是一次 ask 调用（剥掉 await / 括号），返回该 CallExpression。
fn ask_call_of<'x, 'y>(expr: &'x Expression<'y>) -> Option<&'x CallExpression<'y>> {
    match expr {
        Expression::CallExpression(call) => Some(call),
        Expression::AwaitExpression(awaited) => ask_call_of(&awaited.argument),
        Expression::ParenthesizedExpression(paren) => ask_call_of(&paren.expression),
        _ => None,
    }
}

/// 调用是否是 `Promise.all(...)`（全局 Promise，静态成员 all / allSettled）。
fn is_promise_join(call: &CallExpression<'_>) -> bool {
    match &call.callee {
        Expression::StaticMemberExpression(member) => {
            let property = &member.property.name;
            if property != "all" && property != "allSettled" {
                return false;
            }
            matches!(&member.object, Expression::Identifier(id) if id.name == "Promise")
        }
        _ => false,
    }
}

struct GraphWalker<'a> {
    /// 句柄绑定：`const h = agent("x")` → h → x。
    handles: HashMap<String, String>,
    /// promise 绑定：`const p = h.ask(...)` → p → 节点下标（Promise.all 数组元素解析用）。
    promise_defs: HashMap<String, usize>,
    nodes: Vec<DependencyNode>,
    /// 每个 Promise.all 站点的成员节点下标。
    join_members: Vec<Vec<usize>>,
    /// 正在收集的 Promise.all 站点（嵌套表达式里的 ask 归入该站点）。
    cur_join: Option<Vec<usize>>,
    wrapped: &'a str,
}

impl<'a> GraphWalker<'a> {
    /// ask 调用点的 actor：句柄绑定 → 直接 agent() 调用 → 源文本回退（观测用）。
    fn actor_of_receiver(&self, object: &Expression<'_>) -> String {
        if let Expression::Identifier(id) = object {
            if let Some(actor) = self.handles.get(id.name.as_str()) {
                return actor.clone();
            }
            return id.name.as_str().to_string();
        }
        if let Expression::CallExpression(call) = object {
            if let Some(actor) = actor_of_agent_call(call) {
                return actor;
            }
        }
        let end = (object.span().end as usize).min(self.wrapped.len());
        let start = (object.span().start as usize).min(end);
        self.wrapped[start..end]
            .trim()
            .chars()
            .take(32)
            .collect::<String>()
    }

    fn try_ask_node(&mut self, call: &CallExpression<'a>) -> bool {
        let (property, object) = match &call.callee {
            Expression::StaticMemberExpression(member) => (&member.property.name, &member.object),
            _ => return false,
        };
        if property != "ask" && property != "askJson" {
            return false;
        }
        let actor = self.actor_of_receiver(object);
        let (line, offset) = {
            let source = &self.wrapped[WRAPPER_HEAD.len()..];
            let (line, _) = line_col(source, call.span.start);
            (
                line,
                (call.span.start as usize).saturating_sub(WRAPPER_HEAD.len()),
            )
        };
        let same_line = self.nodes.iter().filter(|n| n.line == line).count();
        let id = if same_line == 0 {
            format!("ask-{line}")
        } else {
            format!("ask-{line}-{}", same_line + 1)
        };
        let index = self.nodes.len();
        self.nodes.push(DependencyNode {
            id,
            actor,
            line,
            offset,
            parallel: false,
            wave: 0,
        });
        if let Some(members) = self.cur_join.as_mut() {
            members.push(index);
        }
        true
    }

    /// `Promise.all` 站点：数组元素逐个归集——标识符元素查 promise 绑定，
    /// 其余元素走 AST 收集（元素子树里的 ask 都属于本站点）。
    fn collect_join(&mut self, call: &CallExpression<'a>) {
        let mut members: Vec<usize> = Vec::new();
        let mut walk_targets: Vec<&Expression<'a>> = Vec::new();
        if let Some(expr) = call
            .arguments
            .first()
            .and_then(|argument| argument.as_expression())
        {
            match expr {
                Expression::ArrayExpression(array) => {
                    for element in &array.elements {
                        match element.as_expression() {
                            Some(Expression::Identifier(id)) => {
                                if let Some(&index) = self.promise_defs.get(id.name.as_str()) {
                                    members.push(index);
                                }
                            }
                            Some(other) => walk_targets.push(other),
                            None => {}
                        }
                    }
                }
                other => walk_targets.push(other),
            }
        }
        let previous = self.cur_join.replace(members);
        for target in walk_targets {
            self.visit_expression(target);
        }
        let members = self.cur_join.take().unwrap_or_default();
        self.cur_join = previous;
        // 其余参数（罕见）照常走查，不归属本站点。
        for argument in call.arguments.iter().skip(1) {
            if let Some(expr) = argument.as_expression() {
                self.visit_expression(expr);
            }
        }
        self.join_members.push(members);
    }
}

impl<'a> Visit<'a> for GraphWalker<'a> {
    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if is_promise_join(it) {
            self.collect_join(it);
            return;
        }
        let pushed = self.try_ask_node(it);
        let _ = pushed;
        walk_call_expression(self, it);
    }

    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        let nodes_before = self.nodes.len();
        walk_variable_declarator(self, it);
        let Some(init) = &it.init else { return };
        let Some(name) = (match &it.id {
            BindingPattern::BindingIdentifier(id) => Some(id.name.as_str().to_string()),
            _ => None,
        }) else {
            return;
        };
        // `const h = agent("x")` → 句柄绑定（await 一个 agent() 没有意义，只看直连形状）。
        if let Expression::CallExpression(call) = init {
            if let Some(actor) = actor_of_agent_call(call) {
                self.handles.insert(name, actor);
                return;
            }
        }
        // `const p = h.ask(...)` → promise 绑定（初始化式里恰好一个 ask 节点时才算——
        // 多个 ask 的表达式无法归一到单节点）。**带 await 的不算**：`const x = await
        // h.ask(...)` 是已完成的答案，不是待并行的 promise——并进 Promise.all 会把
        // 本来串行的两个 ask 凭空合并成一个并行波，拆掉顺序依赖。
        if !matches!(init, Expression::AwaitExpression(_)) && ask_call_of(init).is_some() {
            let pushed: Vec<usize> = (nodes_before..self.nodes.len()).collect();
            if pushed.len() == 1 {
                self.promise_defs.insert(name, pushed[0]);
            }
        }
    }
}

struct ActorWalker<'a> {
    actors: Vec<String>,
    #[allow(dead_code)]
    wrapped: &'a str,
}

impl<'a> Visit<'a> for ActorWalker<'a> {
    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if let Some(actor) = actor_of_agent_call(it) {
            if !self.actors.iter().any(|existing| existing == &actor) {
                self.actors.push(actor);
            }
        }
        walk_call_expression(self, it);
    }
}

// ---------------------------------------------------------------------------
// .ask<T> → JSON Schema 编译 + 调用点重写
// ---------------------------------------------------------------------------

/// typed ask 编译产物：重写后的可执行脚本（纯 JS）+ 各调用点的行号与 schema。
#[derive(Debug)]
pub(crate) struct TypedAskTransform {
    pub(crate) exec_script: String,
    /// (行号, schema)；行号按作者原文。
    pub(crate) typed_asks: Vec<(usize, Value)>,
}

/// schema 展开的节点数预算。正常 facade schema 远小于此（几十到几百节点）。
const MAX_SCHEMA_NODES: usize = 50_000;
/// 重写后可执行脚本的长度上限：每个调用点的 schema JSON 都内联进 exec_script，
/// 聚合体积要有自己的上限（与 [`MAX_SCHEMA_NODES`] 一起把编译期内存钉死）。
const MAX_EXEC_SCRIPT_CHARS: usize = 200_000;

/// 跨【全部 typed ask】共享的展开预算（每次编译一个）：按 ask 独立计数挡不住
/// 「很多个中等大小的 ask」加总爆内存（1500 个 4 层宽别名调用 ≈ 200 MB schema）。
#[derive(Default)]
struct Budget {
    nodes: usize,
    over: bool,
}

/// 脚本（字符串/注释遮蔽后）是否包含 `.ask<`——typed ask 的快速预判，无则零开销跳过。
///
/// **只作观测用途**（洞察 / 测试），不作编译开关：`.ask <T>`（空格）与模板串插值里的
/// typed ask 都不含 `.ask<` 字节序列，靠文本预判会漏——提交路径一律走
/// [`compile_typed_asks`]（AST 判定，无 typed ask 时原样返回）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn has_typed_asks(script: &str) -> bool {
    super::workflow::mask_strings_and_comments(script).contains(".ask<")
}

/// 把所有 `h.ask<T>(question)` 编译重写为 `h.askJson(question, <schema>)`。
/// 不支持的类型 / 非法形状 / 嵌套 typed ask 返回诊断列表（提交期拒绝，只回模型）。
/// oxc 拒绝的脚本（极端语法 / 语法错误）原样返回——重写层不设第二道语法门禁，
/// 语法由 `check_script` 的探针把关（typed ask 只会出现在 oxc 能解析的脚本里）。
/// 嵌套超限（[`check_nesting_depth`]）直接给带行列的诊断；解析与遍历在大栈专用线程上
/// 执行（见 [`on_parse_stack`]），线程起不来按「无 typed ask」原样放行（QuickJS 探针
/// 仍会把 TS 语法当语法错误拒绝，不会把未编译的 `ask<T>` 放进执行）。
pub(crate) fn compile_typed_asks(script: &str) -> Result<TypedAskTransform, Vec<FacadeDiagnostic>> {
    if let Err(diagnostic) = check_nesting_depth(script) {
        return Err(vec![diagnostic]);
    }
    match on_parse_stack(|| compile_typed_asks_inner(script)) {
        Some(result) => result,
        None => {
            tracing::warn!("workflow typed ask 编译线程无法启动，按无 typed ask 放行");
            Ok(TypedAskTransform {
                exec_script: script.to_string(),
                typed_asks: Vec::new(),
            })
        }
    }
}

fn compile_typed_asks_inner(script: &str) -> Result<TypedAskTransform, Vec<FacadeDiagnostic>> {
    let allocator = Allocator::default();
    let wrapped = format!("{WRAPPER_HEAD}{script}{WRAPPER_TAIL}");
    let Some(program) = parse(&allocator, &wrapped) else {
        return Ok(TypedAskTransform {
            exec_script: script.to_string(),
            typed_asks: Vec::new(),
        });
    };
    // 类型别名表（`type X = ...`，无泛型参数）。包裹脚本是单个表达式语句（括号包住的
    // async 函数），别名藏在函数体里——用 Visit 深入收集，不做语句形态假设。同名
    // 别名（不同作用域互相遮蔽）不支持：扁平表无法表达作用域，第二个声明报错，
    // 不静默覆盖（外层类型被内层同名声明偷换）。
    let mut alias_collector = AliasCollector {
        allocator: &allocator,
        aliases: HashMap::new(),
        spans: Vec::new(),
        diagnostics: Vec::new(),
    };
    alias_collector.visit_program(&program);
    let AliasCollector {
        aliases,
        spans,
        diagnostics: alias_diagnostics,
        ..
    } = alias_collector;
    if !alias_diagnostics.is_empty() {
        let source = &wrapped[WRAPPER_HEAD.len()..];
        return Err(alias_diagnostics
            .into_iter()
            .map(|(offset, message)| {
                let (line, col) = line_col(source, offset);
                FacadeDiagnostic { line, col, message }
            })
            .collect());
    }

    let mut walker = TypedAskWalker {
        replacements: Vec::new(),
        diagnostics: Vec::new(),
        typed_asks: Vec::new(),
        aliases: &aliases,
        wrapped: &wrapped,
        budget: Budget::default(),
    };
    walker.visit_program(&program);
    if !walker.diagnostics.is_empty() {
        return Err(walker.diagnostics);
    }
    // 展开后的 schema 深度按【校验器口径】统一核对（泛型代入会把实参子树插进别名
    // 体——源类型嵌套浅 + 实参子树深的组合曾编译通过，子代理执行完之后所有答案才
    // 被校验器的深度上限打回）。schema_depth 与 validate_json_schema 的递归边同口径。
    for (line, schema) in &walker.typed_asks {
        let depth = schema_depth(schema);
        if depth > MAX_TYPE_DEPTH {
            return Err(vec![FacadeDiagnostic {
                line: *line,
                col: 1,
                message: format!(
                    "类型展开后嵌套 {depth} 层，超过 {MAX_TYPE_DEPTH} 层上限：减少实例化嵌套层数"
                ),
            }]);
        }
    }
    // 重写生成的 `JSON.parse(...)` 依赖内置 JSON：脚本自己声明了 `JSON` 绑定时拒绝
    //（遮蔽内置对象，TDZ 场景重写体直接 ReferenceError——原脚本能过 tsc，重写后
    // 才炸）。
    if !walker.replacements.is_empty() {
        let mut bindings = BindingNameCollector {
            declares_json: false,
        };
        bindings.visit_program(&program);
        if bindings.declares_json {
            return Err(vec![FacadeDiagnostic {
                line: 1,
                col: 1,
                message:
                    "脚本声明了名为 `JSON` 的绑定：与 typed ask 重写依赖的内置 JSON.parse 冲突，请改名"
                        .to_string(),
            }]);
        }
    }
    // 别名声明是纯 TS：QuickJS 不认，必须从可执行脚本里抹掉（置空格保留换行，行列与
    // 偏移都不动，后按原文偏移应用 ask 替换不受影响——别名声明与 ask 调用的 span
    // 天然不相交）。
    let mut exec_bytes = script.as_bytes().to_vec();
    for (start, end) in spans {
        let (start, end) = (start.min(exec_bytes.len()), end.min(exec_bytes.len()));
        if start < end {
            for byte in &mut exec_bytes[start..end] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        }
    }
    let mut exec_script =
        String::from_utf8(exec_bytes).expect("span 按字符边界替换，UTF-8 不受破坏");
    // 按起始偏移降序应用替换（后面的先改，前面替换的偏移不受影响）。嵌套 typed ask
    // 已在收集期拒绝（见 TypedAskWalker），此处不再有重叠 span。
    let mut replacements = walker.replacements;
    replacements.sort_by_key(|replacement| std::cmp::Reverse(replacement.0));
    for (start, end, text) in replacements {
        if start <= end && end <= exec_script.len() {
            exec_script.replace_range(start..end, &text);
        }
    }
    // 聚合体积第二道闸：每个调用点的 schema JSON 都内联进 exec_script，节点预算挡
    // 不住「很多个中等大小的 ask」把重写后的脚本撑大（执行期 QuickJS 内存限制在
    // 这之前不生效）。
    if exec_script.chars().count() > MAX_EXEC_SCRIPT_CHARS {
        return Err(vec![FacadeDiagnostic {
            line: 1,
            col: 1,
            message: format!(
                "重写后的脚本超过 {MAX_EXEC_SCRIPT_CHARS} 字符上限：内联 schema 过大，\
                 请缩小类型宽度、嵌套层数或 ask 数量"
            ),
        }]);
    }
    Ok(TypedAskTransform {
        exec_script,
        typed_asks: walker.typed_asks,
    })
}

/// `type X = T;` 收集（带泛型参数的别名跳过——不支持）。Visit 的节点引用只在回调内
/// 有效，别名类型经 `CloneIn` 克隆进 arena 后跨回调保存；声明 span 一并记录（编译期
/// 从可执行脚本里抹除 TS 别名——QuickJS 不认 `type`）。
struct AliasCollector<'a> {
    allocator: &'a Allocator,
    aliases: HashMap<String, AliasDef<'a>>,
    /// 别名声明的作者脚本字节区间（已去包裹头）。
    spans: Vec<(usize, usize)>,
    /// 同名别名重复声明（作用域遮蔽不支持）——(wrapped 偏移, 消息)。
    diagnostics: Vec<(u32, String)>,
}

/// 一条类型别名定义：泛型参数名（按声明序，无泛型为空）+ 类型体（arena 克隆）。
/// 实例化（`Box<string>`）由 SchemaCompiler 做语法层代入。
pub(crate) struct AliasDef<'a> {
    pub(crate) params: Vec<String>,
    pub(crate) body: TSType<'a>,
}

impl<'a> Visit<'a> for AliasCollector<'a> {
    fn visit_ts_type_alias_declaration(&mut self, it: &TSTypeAliasDeclaration<'a>) {
        // 声明 span（去包裹头）记录下来，编译期从可执行脚本抹除（TS-only 语法）。
        let (start, end) = (it.span.start as usize, it.span.end as usize);
        if start >= WRAPPER_HEAD.len() && end >= WRAPPER_HEAD.len() {
            self.spans
                .push((start - WRAPPER_HEAD.len(), end - WRAPPER_HEAD.len()));
        }
        let params: Vec<String> = it
            .type_parameters
            .as_ref()
            .map(|parameters| {
                parameters
                    .params
                    .iter()
                    .map(|parameter| parameter.name.name.as_str().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let name = it.id.name.as_str().to_string();
        // 同名声明拒绝（保留首个）：别名表是扁平的，作用域化遮蔽（外层 `type X` +
        // 函数内 `type X`）表达不了——静默覆盖会让外层调用点用上内层的类型。
        if self.aliases.contains_key(&name) {
            self.diagnostics.push((
                it.span.start,
                format!(
                    "类型别名 `{name}` 重复声明（不同作用域的同名别名不支持）：请改名或合并声明"
                ),
            ));
            return;
        }
        self.aliases.insert(
            name,
            AliasDef {
                params,
                body: it.type_annotation.clone_in(self.allocator),
            },
        );
    }
}

/// 脚本里声明过的绑定名收集：`BindingIdentifier` 只出现在声明位置（引用是另一种
/// 节点 `IdentifierReference`），据此精确判断脚本是否声明了 `JSON`——typed ask
/// 重写生成的 `JSON.parse(...)` 会命中用户的同名绑定（遮蔽内置对象；TDZ 场景直接
/// ReferenceError），存在冲突时编译拒绝。
struct BindingNameCollector {
    declares_json: bool,
}

impl<'a> Visit<'a> for BindingNameCollector {
    fn visit_binding_identifier(&mut self, it: &BindingIdentifier<'a>) {
        if it.name.as_str() == "JSON" {
            self.declares_json = true;
        }
    }

    // 纯类型声明不产生运行时绑定：`type JSON = ...`（别名在执行前被抹除，遮蔽不到
    // 内置 JSON.parse）与 interface 不构成冲突——别把它们算进来，合法的类型别名
    // 名得继续可用。
    fn visit_ts_type_alias_declaration(&mut self, _it: &TSTypeAliasDeclaration<'a>) {}
    fn visit_ts_interface_declaration(&mut self, _it: &TSInterfaceDeclaration<'a>) {}
}

struct TypedAskWalker<'a> {
    /// (script 起始偏移, 结束偏移, 替换文本)。
    replacements: Vec<(usize, usize, String)>,
    diagnostics: Vec<FacadeDiagnostic>,
    typed_asks: Vec<(usize, Value)>,
    aliases: &'a HashMap<String, AliasDef<'a>>,
    wrapped: &'a str,
    /// 跨本次编译【全部 typed ask】共享的展开预算。
    budget: Budget,
}

impl<'a> TypedAskWalker<'a> {
    fn diag(&mut self, offset: u32, message: String) {
        let source = &self.wrapped[WRAPPER_HEAD.len()..];
        let (line, col) = line_col(source, offset);
        self.diagnostics
            .push(FacadeDiagnostic { line, col, message });
    }

    fn script_span(&self, span: Span) -> Option<(usize, usize)> {
        let start = span.start as usize;
        let end = span.end as usize;
        if start < WRAPPER_HEAD.len() || end < WRAPPER_HEAD.len() || end > self.wrapped.len() {
            return None;
        }
        Some((start - WRAPPER_HEAD.len(), end - WRAPPER_HEAD.len()))
    }

    fn slice(&self, span: Span) -> &str {
        let start = (span.start as usize).min(self.wrapped.len());
        let end = (span.end as usize).min(self.wrapped.len()).max(start);
        &self.wrapped[start..end]
    }
}

impl<'a> Visit<'a> for TypedAskWalker<'a> {
    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        let (property, object, member_optional) = match &it.callee {
            Expression::StaticMemberExpression(member) => {
                (&member.property.name, &member.object, member.optional)
            }
            _ => {
                walk_call_expression(self, it);
                return;
            }
        };
        let Some(type_arguments) = &it.type_arguments else {
            walk_call_expression(self, it);
            return;
        };
        if property != "ask" {
            if property == "askJson" {
                self.diag(
                    it.span.start,
                    "askJson 不接受类型注解：schema 以第二参数显式传入（askJson(question, schema)）"
                        .to_string(),
                );
            }
            walk_call_expression(self, it);
            return;
        }
        // h.ask<T>(question)：恰好一个表达式参数（问题文本）。
        let question = match it.arguments.as_slice() {
            [only] => match only.as_expression() {
                Some(expr) => expr,
                None => {
                    self.diag(it.span.start, "ask<T> 的问题参数不能是展开元素".to_string());
                    walk_call_expression(self, it);
                    return;
                }
            },
            _ => {
                self.diag(
                    it.span.start,
                    "ask<T> 只接受一个问题参数：ask<T>(question)".to_string(),
                );
                walk_call_expression(self, it);
                return;
            }
        };
        // 恰好一个类型实参：多余的实参（`ask<A, B>`）曾被静默丢弃——作者以为 B 参与
        // 了校验，实际只按 A 校验。
        let type_node = match type_arguments.params.as_slice() {
            [only] => only,
            [] => {
                self.diag(it.span.start, "ask<> 的类型参数列表为空".to_string());
                walk_call_expression(self, it);
                return;
            }
            _ => {
                self.diag(
                    it.span.start,
                    "ask<T> 只接受一个类型参数：多余的类型参数不参与校验，请删掉".to_string(),
                );
                walk_call_expression(self, it);
                return;
            }
        };
        let mut compiler = SchemaCompiler {
            aliases: self.aliases,
            type_args: Vec::new(),
            diagnostics: Vec::new(),
            budget: &mut self.budget,
        };
        let schema = compiler.compile(type_node, 0, 0);
        for (offset, message) in compiler.diagnostics {
            self.diag(offset, message);
        }
        if self.diagnostics.is_empty() {
            let receiver = self.slice(object.span()).trim();
            let question_src = self.slice(question.span());
            // 可选链（h?.ask<T>(..)）重写后必须保留：`?.` 与 `.` 混用会改变语义——
            // h 为空时合法跳过的调用变成 TypeError，且问题参数的副作用不再被短路。
            // 调用本身的可选标记（`ask?.<T>(..)`，flag 在 CallExpression 上）放在
            // 参数括号之前（`askJson?.(...)` 是 JS 的可选调用语法；`(...)?` 不是）。
            let chain = if member_optional { "?." } else { "." };
            let call_mark = if it.optional { "?." } else { "" };
            // `ask<string>` 与 facade 默认的 `ask(question): Promise<string>` 同义：答案本来
            // 就是整段文本，不该再要求子代理回 JSON 字符串字面量（`"..."`）——走 askJson
            // 会把朴素回答判成「不是合法 JSON」。只抹掉类型实参，保留普通 ask。
            let plain_string_ask = schema == json!({"type": "string"});
            let mut replacement = if plain_string_ask {
                format!("{receiver}{chain}ask{call_mark}({question_src}")
            } else {
                // schema 以 JSON 文本 → JS 字符串字面量 → JSON.parse 嵌入：直接内联对象
                // 字面量会把 `__proto__` 键变成原型设置而非自有属性（JSON.stringify 随即
                // 丢掉该约束，校验被静默放宽）；parse 路径的键语义与 JSON 完全一致。
                let schema_literal = serde_json::to_string(&schema.to_string()).unwrap_or_default();
                format!("{receiver}{chain}askJson{call_mark}({question_src}, JSON.parse({schema_literal})")
            };
            // 原调用跨多行（类型实参 / 参数各占一行）时替换文本会变短：补回被吃掉的换行，
            // 后续语句的行号与作者脚本一致——否则 QuickJS 运行期报错 / run_failed 的
            // 行号指到错误位置，模型按行号修改时改错地方。
            let original_newlines = self.slice(it.span).matches('\n').count();
            let replacement_newlines = replacement.matches('\n').count();
            for _ in replacement_newlines..original_newlines {
                replacement.push('\n');
            }
            replacement.push(')');
            if let Some((start, end)) = self.script_span(it.span) {
                // 嵌套 typed ask（`ask<T>(await ask<U>(...))` 的参数里又有一层）拒绝：
                // 外层替换的文本取自原文，内层重写无法嵌进外层的固定 span 里——支持它
                // 需要递归改写，误产出碎片比明确报错更糟；模型把内层存变量即可绕开。
                if self
                    .replacements
                    .iter()
                    .any(|(other_start, other_end, _)| start < *other_end && *other_start < end)
                {
                    self.diag(
                        it.span.start,
                        "ask<T> 嵌套不支持（参数里又出现一层 ask<T>）：先把内层结果存到变量，\
                         再把变量作为问题文本传入"
                            .to_string(),
                    );
                    walk_call_expression(self, it);
                    return;
                }
                self.replacements.push((start, end, replacement));
                let source = &self.wrapped[WRAPPER_HEAD.len()..];
                let (line, _) = line_col(source, it.span.start);
                self.typed_asks.push((line, schema));
            }
        }
        walk_call_expression(self, it);
    }
}

// ---------------------------------------------------------------------------
// TSType（语法层，无 checker）→ JSON Schema 子集
// ---------------------------------------------------------------------------

struct SchemaCompiler<'a> {
    aliases: &'a HashMap<String, AliasDef<'a>>,
    /// 当前正在实例化的别名的参数帧：参数名 → 实参【已编译 schema】。实参在
    /// 【调用方】环境解析后入栈，body 编译期间栈里【只有这一帧】——外层帧不得
    /// 泄漏进无参别名体（同名标识符是全局别名，不是调用方的参数）。
    /// compile_reference 命中参数名时查这帧；查不到落到全局别名表。
    type_args: Vec<HashMap<String, Value>>,
    diagnostics: Vec<(u32, String)>,
    /// 跨全部 typed ask 共享的展开预算（近似 schema 节点数，含从帧里 clone 出来的
    /// 实参子树）：深度上限 8 挡不住「宽别名 × 嵌套实例化」的指数展开，而编译发生
    /// 在 QuickJS 的内存/执行限制之前——必须自带预算，超限报错拒绝。
    budget: &'a mut Budget,
}

/// schema 树的近似节点数（对象 / 数组的值逐个计入）。
fn schema_size(value: &Value) -> usize {
    match value {
        Value::Object(map) => 1 + map.values().map(schema_size).sum::<usize>(),
        Value::Array(items) => 1 + items.iter().map(schema_size).sum::<usize>(),
        _ => 1,
    }
}

/// schema 树的嵌套深度，按【校验器递归边】计：只有 properties 的值、items、
/// additionalProperties、prefixItems、allOf、anyOf 会消耗校验器的 depth+1；
/// `type` / `required` / `const` 等标量字段与 JSON 表示容器（properties 键本身）
/// 不算——按 JSON 容器层计会把合法嵌套在编译期提前拒掉（校验器其实只看到一半深度）。
/// 叶子记 0：校验器从 depth 0 校验根 schema，每条递归边 +1，`depth > 8` 才拒绝——
/// 返回值即「最深叶子被校验时的 depth」，与 `MAX_TYPE_DEPTH` 直接比较口径一致
/// （叶子记 1 曾把校验器放行的 8 层嵌套在编译期多拒掉一层）。
fn schema_depth(value: &Value) -> usize {
    let mut depth = 0;
    if let Some(properties) = value.get("properties").and_then(Value::as_object) {
        for child in properties.values() {
            depth = depth.max(1 + schema_depth(child));
        }
    }
    for edge in ["items", "additionalProperties"] {
        if let Some(child) = value.get(edge) {
            if child.is_object() || child.is_array() {
                depth = depth.max(1 + schema_depth(child));
            }
        }
    }
    for edge in ["prefixItems", "allOf", "anyOf"] {
        if let Some(children) = value.get(edge).and_then(Value::as_array) {
            for child in children {
                depth = depth.max(1 + schema_depth(child));
            }
        }
    }
    depth
}

impl<'a> SchemaCompiler<'a> {
    /// 数字常量的 schema 形态：整数值且能在 i64 里【精确】表示才落整数（json!(f64)
    /// 会产出 `1.0`，而类型注解 `<1>` 的自然预期是 `1`；校验端两种表示都按数值相等
    /// 接受）。上界用 2^63 严格比较——`i64::MAX as f64` 会舍入成 2^63，包含性比较
    /// 加饱和转换曾把 9223372036854775808 错写成 9223372036854775807。
    fn number_const(value: f64) -> Value {
        if value.is_finite()
            && value.fract() == 0.0
            && value >= i64::MIN as f64
            && value < 9_223_372_036_854_775_808.0
        {
            json!(value as i64)
        } else {
            json!(value)
        }
    }

    /// 预算入账 + 超限判定：超限只报一次诊断，之后所有 compile 调用 O(1) 快速返回。
    fn charge(&mut self, nodes: usize, offset: u32) {
        self.budget.nodes = self.budget.nodes.saturating_add(nodes);
        if self.budget.nodes > MAX_SCHEMA_NODES && !self.budget.over {
            self.budget.over = true;
            self.diagnostics.push((
                offset,
                format!(
                    "类型展开超过 {MAX_SCHEMA_NODES} 节点上限：缩小别名宽度、实例化层数或 ask 数量"
                ),
            ));
        }
    }
}

impl<'a> SchemaCompiler<'a> {
    fn compile(&mut self, ty: &'a TSType<'a>, depth: usize, alias_depth: usize) -> Value {
        self.charge(1, ty.span().start);
        if self.budget.over {
            return json!({});
        }
        if depth > MAX_COMPILE_RECURSION {
            self.diagnostics.push((
                ty.span().start,
                format!(
                    "类型语法嵌套超过 {MAX_COMPILE_RECURSION} 层上限：拆分别名或减少实例化层数"
                ),
            ));
            return json!({});
        }
        match ty {
            TSType::TSStringKeyword(_) => json!({"type": "string"}),
            TSType::TSNumberKeyword(_) => json!({"type": "number"}),
            TSType::TSBooleanKeyword(_) => json!({"type": "boolean"}),
            TSType::TSNullKeyword(_) => json!({"type": "null"}),
            TSType::TSUnknownKeyword(_) => json!({}),
            TSType::TSAnyKeyword(_) => {
                self.diagnostics.push((
                    ty.span().start,
                    "any 不允许：答案无法校验；用具体类型或 unknown（不校验）".to_string(),
                ));
                json!({})
            }
            TSType::TSLiteralType(literal) => self.compile_literal(&literal.literal),
            TSType::TSUnionType(union) => {
                let variants: Vec<Value> = union
                    .types
                    .iter()
                    .map(|member| self.compile(member, depth + 1, alias_depth))
                    .collect();
                // 全字面量 → enum；否则 anyOf（校验器两者都支持）。
                if variants.iter().all(|v| v.get("const").is_some()) {
                    json!({"enum": variants.iter().filter_map(|v| v.get("const").cloned()).collect::<Vec<_>>()})
                } else {
                    json!({"anyOf": variants})
                }
            }
            TSType::TSArrayType(array) => {
                json!({"type": "array", "items": self.compile(&array.element_type, depth + 1, alias_depth)})
            }
            TSType::TSTupleType(tuple) => {
                // 定长元组 → prefixItems（按位校验）+ min/maxItems；可选 / rest 元素
                // 不支持（校验器无对应语义，报错优于近似）。
                let mut items = Vec::new();
                for element in &tuple.element_types {
                    // 普通类型元素（@inherit TSType）直接编译；可选 / rest 元素
                    // 校验器无对应语义，报错优于近似。
                    if let Some(element_type) = element.as_ts_type() {
                        items.push(self.compile(element_type, depth + 1, alias_depth));
                    } else {
                        self.diagnostics.push((
                            element.span().start,
                            "元组元素必须是普通类型（可选 / rest 元素不支持）".to_string(),
                        ));
                        return json!({});
                    }
                }
                let len = items.len();
                json!({"type": "array", "prefixItems": items, "minItems": len, "maxItems": len})
            }
            TSType::TSIntersectionType(intersection) => {
                // 交叉 = 对象合并：properties 合并、required 并集；非对象分量不支持。
                // 同名属性在各成员都有约束 = TS 语义的属性级交集（`{v:"ok"} & {v:string}`
                // 里 v 必须同时满足两者）——合并成 allOf 让校验器逐分支核对，直接覆盖
                // 会把字面量约束悄悄放宽。
                let mut properties = serde_json::Map::new();
                let mut required: Vec<String> = Vec::new();
                for member in &intersection.types {
                    let schema = self.compile(member, depth + 1, alias_depth);
                    let Some(object) = schema.get("properties").and_then(Value::as_object) else {
                        self.diagnostics.push((
                            member.span().start,
                            "交叉类型的成员必须是对象类型（对象字面量 / 别名）".to_string(),
                        ));
                        continue;
                    };
                    for (key, value) in object {
                        match properties.entry(key.clone()) {
                            serde_json::map::Entry::Occupied(mut occupied) => {
                                // 同名属性已有约束 → 属性级 allOf（存量已是 allOf 则追加分支）。
                                let existing = occupied.get().clone();
                                let branches = match existing.get("allOf") {
                                    Some(Value::Array(earlier)) => {
                                        let mut branches = earlier.clone();
                                        branches.push(value.clone());
                                        branches
                                    }
                                    _ => vec![existing, value.clone()],
                                };
                                occupied.insert(json!({"allOf": branches}));
                            }
                            serde_json::map::Entry::Vacant(vacant) => {
                                vacant.insert(value.clone());
                            }
                        }
                    }
                    if let Some(member_required) = schema.get("required").and_then(Value::as_array)
                    {
                        for key in member_required.iter().filter_map(Value::as_str) {
                            if !required.iter().any(|existing: &String| existing == key) {
                                required.push(key.to_string());
                            }
                        }
                    }
                }
                let mut schema = json!({"type": "object", "properties": properties});
                if !required.is_empty() {
                    schema["required"] = json!(required);
                }
                schema
            }
            TSType::TSTypeLiteral(literal) => {
                self.compile_type_literal(literal, depth, alias_depth)
            }
            TSType::TSTypeReference(reference) => {
                // Box<'a, TSTypeReference<'a>> 解引用为 &'a（实例化栈要存 'a 引用）。
                self.compile_reference(reference, ty.span().start, depth, alias_depth)
            }
            TSType::TSParenthesizedType(paren) => {
                self.compile(&paren.type_annotation, depth + 1, alias_depth)
            }
            other => {
                self.diagnostics.push((
                    other.span().start,
                    "类型不受支持：可用 string / number / boolean / null / 字面量 / 联合 / \
                     数组（T[] 或 Array<T>）/ 对象字面量 / 脚本顶层 type 别名（可带泛型参数）"
                        .to_string(),
                ));
                json!({})
            }
        }
    }

    fn compile_literal(&mut self, literal: &TSLiteral<'a>) -> Value {
        match literal {
            TSLiteral::StringLiteral(text) => json!({"const": text.value.as_str()}),
            TSLiteral::NumericLiteral(number) => json!({"const": Self::number_const(number.value)}),
            TSLiteral::BooleanLiteral(flag) => json!({"const": flag.value}),
            TSLiteral::UnaryExpression(unary) => {
                // 负数字面量（`ask<-1 | 0>` 的 -1）：oxc 把类型位置的一元负号解析成
                // UnaryExpression 而不是 NumericLiteral——按符号值给 const。
                if unary.operator == UnaryOperator::UnaryNegation {
                    if let Expression::NumericLiteral(number) = &unary.argument {
                        return json!({"const": Self::number_const(-number.value)});
                    }
                }
                self.diagnostics.push((
                    literal.span().start,
                    "只支持 string / number / boolean 字面量类型（负号仅限数字字面量）".to_string(),
                ));
                json!({})
            }
            other => {
                self.diagnostics.push((
                    other.span().start,
                    "只支持 string / number / boolean 字面量类型".to_string(),
                ));
                json!({})
            }
        }
    }

    fn compile_type_literal(
        &mut self,
        literal: &'a TSTypeLiteral<'a>,
        depth: usize,
        alias_depth: usize,
    ) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required: Vec<String> = Vec::new();
        for member in &literal.members {
            let property = match member {
                TSSignature::TSPropertySignature(property) => property,
                other => {
                    self.diagnostics.push((
                        other.span().start,
                        "对象类型里只支持属性签名（方法 / 索引签名等不支持）".to_string(),
                    ));
                    continue;
                }
            };
            if property.computed {
                self.diagnostics.push((
                    property.span.start,
                    "对象类型的键必须是标识符或字符串字面量（不支持计算键）".to_string(),
                ));
                continue;
            }
            let key = match &property.key {
                PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
                other => match other.as_expression() {
                    Some(Expression::StringLiteral(text)) => text.value.as_str().to_string(),
                    _ => {
                        self.diagnostics.push((
                            property.span.start,
                            "对象类型的键必须是标识符或字符串字面量".to_string(),
                        ));
                        continue;
                    }
                },
            };
            match &property.type_annotation {
                Some(annotation) => {
                    properties.insert(
                        key.clone(),
                        self.compile(&annotation.type_annotation, depth + 1, alias_depth),
                    );
                }
                None => {
                    self.diagnostics
                        .push((property.span.start, format!("属性 `{key}` 缺少类型注解")));
                    continue;
                }
            }
            if !property.optional {
                required.push(key);
            }
        }
        let mut schema = json!({"type": "object", "properties": properties});
        if !required.is_empty() {
            schema["required"] = json!(required);
        }
        schema
    }

    fn compile_reference(
        &mut self,
        reference: &'a TSTypeReference<'a>,
        span_start: u32,
        depth: usize,
        alias_depth: usize,
    ) -> Value {
        let TSTypeName::IdentifierReference(id) = &reference.type_name else {
            self.diagnostics.push((
                span_start,
                "类型引用必须是简单标识符（不支持限定名）".to_string(),
            ));
            return json!({});
        };
        let name = id.name.as_str();
        // 泛型实例化帧最优先：别名 body 内引用参数名（T / U…，含遮蔽内置 Array 的
        // 参数名）→ 返回调用方环境里已解析的实参 schema。clone 的是整棵子树，其尺寸
        // 计入展开预算——否则「body 里 N 处引用 × 实参子树大小」的乘法绕过构造期
        // 计数。展开后的总深度在编译完成后按校验器口径统一核对（见 compile_typed_asks）。
        let hit = self
            .type_args
            .iter()
            .rev()
            .find_map(|frame| frame.get(name).cloned());
        if let Some(schema) = hit {
            self.charge(schema_size(&schema), span_start);
            return if self.budget.over { json!({}) } else { schema };
        }
        // Array<T>（内置特判在参数帧之后——参数名可以遮蔽内置名）。
        if name == "Array" && self.aliases.get(name).is_none() {
            let Some(params) = &reference.type_arguments else {
                self.diagnostics
                    .push((span_start, "Array 需要元素类型参数".to_string()));
                return json!({});
            };
            let Some(element) = params.params.first() else {
                self.diagnostics
                    .push((span_start, "Array<> 的类型参数列表为空".to_string()));
                return json!({});
            };
            return json!({"type": "array", "items": self.compile(element, depth + 1, alias_depth)});
        }
        // 类型别名（脚本顶层 `type X = ...`，可带泛型参数，引用点实例化）。
        let Some(alias) = self.aliases.get(name) else {
            self.diagnostics.push((
                span_start,
                format!("未知类型 `{name}`：只支持内联类型或脚本顶层 type 别名"),
            ));
            return json!({});
        };
        if alias_depth > MAX_ALIAS_DEPTH {
            self.diagnostics
                .push((span_start, "类型别名循环引用".to_string()));
            return json!({});
        }
        // 实参个数与声明对齐：无参数别名不允许带实参引用；带参数别名必须给全。
        let arguments: Vec<&TSType<'a>> = reference
            .type_arguments
            .as_ref()
            .map(|params| params.params.iter().collect())
            .unwrap_or_default();
        if arguments.len() != alias.params.len() {
            self.diagnostics.push((
                span_start,
                format!(
                    "类型 `{name}` 需要 {} 个类型参数，实给 {} 个",
                    alias.params.len(),
                    arguments.len()
                ),
            ));
            return json!({});
        }
        // 语法层代入：实参先在【调用方】环境（当前栈）里解析成 schema，再以参数名
        // 入栈；body 只在本别名的【参数帧】里编译（TS 作用域语义）——带着外层帧编
        // body 会让无参别名体里的同名标识符被调用方参数偷换（`type T = number;
        // type Box = { v: T }; type Outer<T> = Box` 的 Outer<string> 会把全局
        // T=number 错编成 string）。
        let mut frame: HashMap<String, Value> = HashMap::new();
        for (param, argument) in alias.params.iter().zip(arguments) {
            frame.insert(
                param.clone(),
                self.compile(argument, depth + 1, alias_depth),
            );
        }
        let outer_frames = std::mem::take(&mut self.type_args);
        self.type_args = vec![frame];
        let compiled = self.compile(&alias.body, depth + 1, alias_depth + 1);
        self.type_args = outer_frames;
        compiled
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn node_ids(graph: &DependencyGraph) -> Vec<String> {
        graph.nodes.iter().map(|n| n.id.clone()).collect()
    }

    fn actors(graph: &DependencyGraph) -> Vec<String> {
        graph.nodes.iter().map(|n| n.actor.clone()).collect()
    }

    #[test]
    fn ast_graph_serial_chain_and_promise_all_wave() {
        let script = "const coder = agent(\"coder\");\n\
                      const r1 = await coder.ask(\"设计\");\n\
                      const reviewer = agent(\"reviewer\");\n\
                      const r2 = await reviewer.ask(\"评审\");\n\
                      const r3 = await coder.ask(\"修复\");\n";
        let graph = build_dependency_graph(script).expect("AST 解析应成功");
        assert_eq!(node_ids(&graph), vec!["ask-2", "ask-4", "ask-5"]);
        assert_eq!(actors(&graph), vec!["coder", "reviewer", "coder"]);
        assert_eq!(graph.edges.len(), 2, "{:?}", graph.edges);
        // 三层波（串行链）。
        assert_eq!(graph.waves.len(), 3, "{:?}", graph.waves);

        let parallel = "const a = agent(\"a\");\nconst b = agent(\"b\");\n\
                        const [x, y] = await Promise.all([a.ask(\"x\"), b.ask(\"y\")]);\n";
        let graph = build_dependency_graph(parallel).expect("AST 解析应成功");
        assert_eq!(node_ids(&graph), vec!["ask-3", "ask-3-2"]);
        assert!(graph.nodes.iter().all(|n| n.parallel));
        assert!(graph.edges.is_empty(), "波内无边: {:?}", graph.edges);
        assert_eq!(graph.waves.len(), 1, "{:?}", graph.waves);
    }

    #[test]
    fn ast_graph_ignores_asks_in_strings_and_comments() {
        // 文本解析会把注释/字符串里的 .ask( 计入；AST 不会。
        let script = "const a = agent(\"a\");\n\
                      // const dead = agent(\"dead\"); dead.ask(\"注释里的\")\n\
                      const note = \".ask(伪装)\";\n\
                      const r = await a.ask(\"真实\");\n";
        let graph = build_dependency_graph(script).expect("AST 解析应成功");
        assert_eq!(node_ids(&graph), vec!["ask-4"]);
        assert_eq!(actors(&graph), vec!["a"]);
    }

    #[test]
    fn single_line_groups_sort_by_source_offset_not_line() {
        // 回归：单行脚本「串行 ask → Promise.all 波 → 串行 ask」里所有组行号相同，
        // 按行排序会打平，插入序（join 组先发现）把并行波排到前一个串行 ask 之前。
        let script = "const a = agent(\"a\"); \
                      const first = await a.ask(\"先\"); \
                      const [x, y] = await Promise.all([a.ask(\"并1\"), a.ask(\"并2\")]); \
                      const last = await a.ask(\"后\");";
        let graph = build_dependency_graph(script).expect("AST 解析应成功");
        assert_eq!(
            node_ids(&graph),
            vec!["ask-1", "ask-1-2", "ask-1-3", "ask-1-4"]
        );
        assert!(!graph.nodes[0].parallel, "首个串行 ask 不在波内");
        assert!(graph.nodes[1].parallel && graph.nodes[2].parallel);
        assert!(!graph.nodes[3].parallel);
        // 边：串行出口 → 波内每个成员 → 末尾串行（4 条：1→2、1→3、2→4、3→4）。
        assert_eq!(graph.edges.len(), 4, "{:?}", graph.edges);
        assert!(graph.edges.contains(&("ask-1".into(), "ask-1-2".into())));
        assert!(graph.edges.contains(&("ask-1".into(), "ask-1-3".into())));
        assert!(
            !graph.edges.contains(&("ask-1-2".into(), "ask-1".into())),
            "{:?}",
            graph.edges
        );
    }

    #[test]
    fn ast_graph_resolves_inline_agent_calls_and_hoisted_promises() {
        // inline agent() 调用（无句柄绑定）与变量提升的 Promise.all 扇出。
        let script = "const a = agent(\"a\");\n\
                      const b = agent(\"b\");\n\
                      const p1 = a.ask(\"x\");\n\
                      const p2 = b.ask(\"y\");\n\
                      const rs = await Promise.all([p1, p2]);\n\
                      const r3 = await a.ask(\"汇总\" + rs.length);\n";
        let graph = build_dependency_graph(script).expect("AST 解析应成功");
        assert_eq!(node_ids(&graph), vec!["ask-3", "ask-4", "ask-6"]);
        assert!(graph.nodes[0].parallel && graph.nodes[1].parallel);
        assert!(!graph.nodes[2].parallel, "波后的串行 ask 不标并行");
        // 波（p1,p2）→ 汇总：两条边，波内无边。
        assert_eq!(graph.edges.len(), 2, "{:?}", graph.edges);
        assert!(graph.edges.contains(&("ask-3".into(), "ask-6".into())));
        assert!(graph.edges.contains(&("ask-4".into(), "ask-6".into())));
        assert_eq!(graph.waves.len(), 2, "{:?}", graph.waves);

        // 回归：`const x = await a.ask(..)` 是已完成的答案，不是待并行的 promise——
        // 之后把它们列进 Promise.all 不得把两个串行 ask 合并成并行波。
        let awaited = "const a = agent(\"a\");\n\
                       const b = agent(\"b\");\n\
                       const x = await a.ask(\"先\");\n\
                       const y = await b.ask(\"后\");\n\
                       const rs = await Promise.all([x, y]);\n";
        let graph = build_dependency_graph(awaited).expect("AST 解析应成功");
        assert_eq!(node_ids(&graph), vec!["ask-3", "ask-4"]);
        assert!(
            !graph.nodes.iter().all(|n| n.parallel),
            "await 过的 ask 不得进并行波"
        );
        assert!(
            graph.edges.contains(&("ask-3".into(), "ask-4".into())),
            "顺序依赖保留: {:?}",
            graph.edges
        );
        assert_eq!(graph.waves.len(), 2, "{:?}", graph.waves);

        // inline agent("x").ask(...)：无绑定也能解析 actor。
        let inline = "const r = await agent(\"scout\").ask(\"探路\");\n";
        let graph = build_dependency_graph(inline).expect("AST 解析应成功");
        assert_eq!(actors(&graph), vec!["scout"]);
    }

    #[test]
    fn ast_graph_falls_back_on_unparsable_script() {
        // oxc 不接受的语法（这里用未闭合花括号）→ None（调用方降级文本解析）。
        assert!(build_dependency_graph("const a = agent(\"a\").ask(\"x\").then(").is_none());
    }

    #[test]
    fn typed_ask_compiles_inline_object_schema_and_rewrites_to_askjson() {
        let script = "const scout = agent(\"scout\");\n\
                      const r = await scout.ask<{ host: string, port: number, tags: string[] }>(\"找一个主机\");\n\
                      result(r.host);\n";
        let transform = compile_typed_asks(script).expect("编译应成功");
        // serde_json 的键序按字母排（默认 BTreeMap）：不依赖键序，断言片段各自存在。
        // schema 经 JSON.parse 字符串嵌入（对象字面量的 __proto__ 键会被当原型设置）。
        assert!(
            transform
                .exec_script
                .contains("scout.askJson(\"找一个主机\", JSON.parse("),
            "{}",
            transform.exec_script
        );
        assert!(transform
            .exec_script
            .contains("\\\"type\\\":\\\"object\\\""));
        assert!(!transform.exec_script.contains(".ask<"));
        assert_eq!(transform.typed_asks.len(), 1);
        let (_, schema) = &transform.typed_asks[0];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["port"]["type"], "number");
        assert_eq!(schema["properties"]["tags"]["items"]["type"], "string");
        assert_eq!(schema["required"], json!(["host", "port", "tags"]));
        // 重写后仍是合法 JS（AST 依赖图能解析）。
        assert!(build_dependency_graph(&transform.exec_script).is_some());
    }

    #[test]
    fn typed_ask_supports_literals_unions_aliases_and_arrays() {
        let script = "type Host = { name: string, zone: \"a\" | \"b\", optional?: number };\n\
                      const h = agent(\"h\");\n\
                      const r1 = await h.ask<Host>(\"主机\");\n\
                      const r2 = await h.ask<Array<Host>>(\"列表\");\n\
                      const r3 = await h.ask<\"fast\" | \"slow\">(\"档位\");\n\
                      const r4 = await h.ask<number[]>(\"端口\");\n\
                      result(r1.name);\n";
        let transform = compile_typed_asks(script).expect("编译应成功");
        assert_eq!(transform.typed_asks.len(), 4);
        let (_, alias_schema) = &transform.typed_asks[0];
        assert_eq!(
            alias_schema["properties"]["zone"]["enum"],
            json!(["a", "b"])
        );
        assert_eq!(alias_schema["required"], json!(["name", "zone"]));
        let (_, array_schema) = &transform.typed_asks[1];
        assert_eq!(array_schema["type"], "array");
        assert_eq!(array_schema["items"]["type"], "object");
        let (_, union_schema) = &transform.typed_asks[2];
        assert_eq!(union_schema["enum"], json!(["fast", "slow"]));
        let (_, num_array) = &transform.typed_asks[3];
        assert_eq!(num_array["items"]["type"], "number");
    }

    #[test]
    fn typed_ask_rejects_unsupported_types_with_diagnostics() {
        let bad_any = "const h = agent(\"h\");\nawait h.ask<any>(\"任意\");\n";
        let diagnostics = compile_typed_asks(bad_any).unwrap_err();
        assert!(
            diagnostics.iter().any(|d| d.message.contains("any")),
            "{diagnostics:?}"
        );

        let bad_ref = "const h = agent(\"h\");\nawait h.ask<Date>(\"时间\");\n";
        let diagnostics = compile_typed_asks(bad_ref).unwrap_err();
        assert!(
            diagnostics.iter().any(|d| d.message.contains("未知类型")),
            "{diagnostics:?}"
        );

        let bad_args = "const h = agent(\"h\");\nawait h.ask<{a: string}>(\"q\", \"多余\");\n";
        let diagnostics = compile_typed_asks(bad_args).unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("一个问题参数")),
            "{diagnostics:?}"
        );

        let bad_json = "const h = agent(\"h\");\nawait h.askJson<{a: string}>(\"q\", {});\n";
        let diagnostics = compile_typed_asks(bad_json).unwrap_err();
        assert!(
            diagnostics.iter().any(|d| d.message.contains("askJson")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn typed_ask_detection_scans_code_only() {
        assert!(has_typed_asks("h.ask<{a: string}>(\"q\")"));
        assert!(!has_typed_asks("// h.ask<...>(\"q\")"));
        assert!(!has_typed_asks("const s = \".ask<{a}>\"; h.ask(\"q\")"));
        assert!(!has_typed_asks("h.ask(\"q\")"));
    }

    #[test]
    fn typed_ask_rewrite_preserves_plain_asks_and_multiple_sites() {
        let script = "const h = agent(\"h\");\n\
                      const a = await h.ask(\"普通\");\n\
                      const b = await h.ask<{ n: number }>(\"类型\");\n\
                      const c = await h.ask(\"再来\");\n\
                      result(a + b.n + c);\n";
        let transform = compile_typed_asks(script).expect("编译应成功");
        assert_eq!(transform.typed_asks.len(), 1);
        assert!(transform.exec_script.contains("h.ask(\"普通\")"));
        assert!(transform.exec_script.contains("h.ask(\"再来\")"));
        assert!(transform.exec_script.contains("h.askJson(\"类型\""));
    }
}

#[cfg(test)]
mod extended_type_tests {
    use super::*;

    #[test]
    fn tuple_compiles_to_prefix_items() {
        let script = "const h = agent(\"h\");\n\
                      const r = await h.ask<[string, number, boolean]>(\"三元组\");\n\
                      result(String(r.length));\n";
        let transform = compile_typed_asks(script).expect("编译应成功");
        let (_, schema) = &transform.typed_asks[0];
        assert_eq!(schema["type"], "array");
        let prefix = schema["prefixItems"].as_array().unwrap();
        assert_eq!(prefix.len(), 3);
        assert_eq!(prefix[0]["type"], "string");
        assert_eq!(prefix[1]["type"], "number");
        assert_eq!(prefix[2]["type"], "boolean");
        assert_eq!(schema["minItems"], 3);
        assert_eq!(schema["maxItems"], 3);
        // 可选 / rest 元素拒绝。
        let bad =
            compile_typed_asks("const h = agent(\"h\");\nawait h.ask<[string, number?]>(\"q\");\n")
                .unwrap_err();
        assert!(bad.iter().any(|d| d.message.contains("可选")), "{bad:?}");
    }

    #[test]
    fn intersection_merges_object_schemas() {
        let script = concat!(
            "type Base = { id: string, note?: string };\n",
            "const h = agent(\"h\");\n",
            "const r = await h.ask<Base & { score: number }>(\"交叉\");\n",
            "result(r.id);\n"
        );
        let transform = compile_typed_asks(script).expect("编译应成功");
        let (_, schema) = &transform.typed_asks[0];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["id"]["type"], "string");
        assert_eq!(schema["properties"]["score"]["type"], "number");
        assert_eq!(schema["properties"]["note"]["type"], "string");
        // required = 并集（可选字段不进）。
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "id"));
        assert!(required.iter().any(|v| v == "score"));
        assert!(!required.iter().any(|v| v == "note"));
        // 非对象分量拒绝。
        let bad = compile_typed_asks(
            "const h = agent(\"h\");\nawait h.ask<{ a: string } & string[]>(\"q\");\n",
        )
        .unwrap_err();
        assert!(bad.iter().any(|d| d.message.contains("交叉")), "{bad:?}");
    }

    #[test]
    fn generic_alias_instantiates_type_arguments() {
        let script = concat!(
            "type Box<T> = { value: T };\n",
            "type Pair<A, B> = { first: A, second: B };\n",
            "const h = agent(\"h\");\n",
            "const r = await h.ask<Box<number>>(\"单参\");\n",
            "const p = await h.ask<Pair<string, boolean>>(\"双参\");\n",
            "result(String(r.value) + String(p.first));\n"
        );
        let transform = compile_typed_asks(script).expect("编译应成功");
        assert_eq!(transform.typed_asks.len(), 2);
        let (_, box_schema) = &transform.typed_asks[0];
        assert_eq!(box_schema["properties"]["value"]["type"], "number");
        let (_, pair_schema) = &transform.typed_asks[1];
        assert_eq!(pair_schema["properties"]["first"]["type"], "string");
        assert_eq!(pair_schema["properties"]["second"]["type"], "boolean");

        // 实参个数不匹配 → 诊断。
        let bad = compile_typed_asks(
            "type Box<T> = { value: T };\nconst h = agent(\"h\");\nawait h.ask<Box>(\"q\");\n",
        )
        .unwrap_err();
        assert!(
            bad.iter().any(|d| d.message.contains("1 个类型参数")),
            "{bad:?}"
        );
        // 嵌套实例化：Box<Box<string>>。
        let nested = compile_typed_asks(
            "type Box<T> = { value: T };\nconst h = agent(\"h\");\nawait h.ask<Box<Box<string>>>(\"q\");\n",
        )
        .expect("嵌套实例化应成功");
        let (_, schema) = &nested.typed_asks[0];
        assert_eq!(
            schema["properties"]["value"]["properties"]["value"]["type"],
            "string"
        );

        // 回归：别名链上实参本身是上层参数名（`type Outer<T> = Inner<T>`，ask<
        // Outer<string>>）——实参必须先在调用方环境解析再入栈，否则 Inner 帧把
        // 参数 T 绑到「自己的 T」，body 引用时同帧自吞尾巴直到深度上限。
        let chained = compile_typed_asks(
            "type Inner<T> = { v: T };\n\
             type Outer<T> = Inner<T>;\n\
             const h = agent(\"h\");\n\
             const r = await h.ask<Outer<string>>(\"链式\");\n\
             result(r.v);\n",
        )
        .expect("别名链实例化应成功");
        let (_, schema) = &chained.typed_asks[0];
        assert_eq!(schema["properties"]["v"]["type"], "string", "{schema}");
    }

    #[test]
    fn alias_declarations_are_erased_from_exec_script() {
        // 回归：别名只参与 schema 编译，声明本身是 TS 语法——留在可执行脚本里会被
        // QuickJS 探针整只拒绝。
        let script = concat!(
            "type Host = { name: string };\n",
            "const h = agent(\"h\");\n",
            "const r = await h.ask<Host>(\"问\");\n",
            "result(r.name);\n"
        );
        let transform = compile_typed_asks(script).expect("编译应成功");
        assert!(
            !transform.exec_script.contains("type Host"),
            "别名声明必须抹除: {}",
            transform.exec_script
        );
        assert!(transform.exec_script.contains(".askJson("));
        // 抹除置空格不换行：askJson 行号 = 原文行号（诊断/定位不漂移）。
        let rewrite_line = transform
            .exec_script
            .lines()
            .position(|line| line.contains(".askJson("))
            .expect("重写行存在");
        let original_line = script
            .lines()
            .position(|line| line.contains("ask<Host>"))
            .expect("原行存在");
        assert_eq!(rewrite_line, original_line);
        assert!(
            transform.exec_script.contains("result(r.name);"),
            "后续代码原样保留"
        );
    }

    #[test]
    fn nested_typed_asks_are_rejected_with_diagnostic() {
        // 回归：嵌套 typed ask 的替换 span 相互包含，按原文 span 重写会产出残缺 JS——
        // 编译期明确拒绝（模型把内层存变量即可绕开）。
        let script = "const h = agent(\"h\");\n\
                      const r = await h.ask<{ v: string }>(`外层 ${await h.ask<string>(\"内层\")}`);\n";
        let diagnostics = compile_typed_asks(script).unwrap_err();
        assert!(
            diagnostics.iter().any(|d| d.message.contains("嵌套不支持")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn typed_asks_are_found_regardless_of_formatting() {
        // 回归：`.ask <T>`（空格）与模板串插值里的 typed ask 都不含 `.ask<` 字节序列，
        // 文本预判曾漏掉它们（TS 语法漏到 QuickJS 才报错）。编译路径现在始终走 AST。
        let spaced = "const h = agent(\"h\");\nconst r = await h.ask <number>(\"间距\");\nresult(String(r));\n";
        let transform = compile_typed_asks(spaced).expect("空格形态应编译");
        assert_eq!(transform.typed_asks.len(), 1);
        assert_eq!(transform.typed_asks[0].1["type"], "number");
        assert!(transform.exec_script.contains(".askJson("));

        // 模板串 ${} 插值里的 typed ask：遮蔽函数把整个模板视作字符串，文本预判
        // 曾漏掉——AST 路径必须能找到并重写插值内的调用。
        let interpolated = "const h = agent(\"h\");\n\
                            const tag = `值 ${await h.ask<number>(\"几\")}`;\n\
                            result(tag);\n";
        let transform = compile_typed_asks(interpolated).expect("插值内调用应编译");
        assert_eq!(transform.typed_asks.len(), 1);
        assert_eq!(transform.typed_asks[0].1["type"], "number");
        assert!(
            transform.exec_script.contains("${await h.askJson("),
            "插值内调用应被重写: {}",
            transform.exec_script
        );
    }

    #[test]
    fn intersection_overlapping_properties_intersect_via_allof() {
        // 回归：`{v: "ok"} & {v: string}` 的 v 曾被第二个成员覆盖（字面量约束被悄悄
        // 放宽）——同名属性必须合成 allOf，答案要同时满足两个分支。
        let script = concat!(
            "const h = agent(\"h\");\n",
            "const r = await h.ask<{ v: \"ok\" } & { v: string }>(\"交叉\");\n",
            "result(r.v);\n"
        );
        let transform = compile_typed_asks(script).expect("编译应成功");
        let (_, schema) = &transform.typed_asks[0];
        let v = &schema["properties"]["v"];
        let branches = v["allOf"].as_array().expect("同名属性合成 allOf");
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0]["const"], "ok");
        assert_eq!(branches[1]["type"], "string");
    }

    #[test]
    fn alias_bodies_compile_in_their_own_parameter_scope() {
        // 回归：别名 body 曾带着调用方的参数帧编译——`type T = number; type Box =
        // { v: T }; type Outer<T> = Box` 的 Outer<string> 会把全局 T=number 偷换成
        // string（合法数值答案被拒、字符串答案被放行）。body 只见本别名自己的帧。
        let script = concat!(
            "type T = number;\n",
            "type Box = { v: T };\n",
            "type Outer<U> = Box;\n",
            "const h = agent(\"h\");\n",
            "const r = await h.ask<Outer<string>>(\"作用域\");\n",
            "result(r.v);\n"
        );
        let transform = compile_typed_asks(script).expect("编译应成功");
        let (_, schema) = &transform.typed_asks[0];
        assert_eq!(
            schema["properties"]["v"]["type"], "number",
            "Box 体里的 T 是全局别名，不受调用方 U=string 影响: {schema}"
        );

        // 对照：真正的泛型透传（Outer<U> = Inner<U>）仍然在调用方环境解析实参。
        let passthrough = compile_typed_asks(
            "type Inner<T> = { v: T };\n\
             type Outer<U> = Inner<U>;\n\
             const h = agent(\"h\");\n\
             const r = await h.ask<Outer<string>>(\"透传\");\n",
        )
        .expect("泛型透传应成功");
        let (_, schema) = &passthrough.typed_asks[0];
        assert_eq!(schema["properties"]["v"]["type"], "string", "{schema}");
    }

    #[test]
    fn optional_chaining_survives_the_rewrite() {
        // 回归：`h?.ask<T>(..)` 的 `?.` 曾被重写成 `.`——h 为空时合法跳过的调用变成
        // TypeError，问题参数的副作用也不再被短路。
        let script = "const h = agent(\"h\");\n\
                      const r = await h?.ask<number>(\"可选\");\n\
                      result(String(r));\n";
        let transform = compile_typed_asks(script).expect("编译应成功");
        assert!(
            transform.exec_script.contains("h?.askJson("),
            "{}",
            transform.exec_script
        );
        assert!(!transform.exec_script.contains(".ask<"));
        // 普通点号调用不受影响。
        let plain = compile_typed_asks(
            "const h = agent(\"h\");\nconst r = await h.ask<number>(\"普通\");\n",
        )
        .expect("编译应成功");
        assert!(
            plain.exec_script.contains("h.askJson("),
            "{}",
            plain.exec_script
        );
        // 调用本身的可选标记（`ask?.<T>(..)`，flag 在 CallExpression 上）同样保留——
        // 位置在参数括号前（`askJson?.(...)` 是 JS 可选调用语法；`(...)?` 不是 JS）。
        let optional_call = compile_typed_asks(
            "const h = agent(\"h\");\nconst r = await h.ask?.<number>(\"可省\");\nresult(String(r));\n",
        )
        .expect("编译应成功");
        assert!(
            optional_call.exec_script.contains("askJson?.("),
            "调用级可选标记应放在参数括号前: {}",
            optional_call.exec_script
        );
    }

    #[test]
    fn type_alias_named_json_does_not_conflict() {
        // 回归：`type JSON = ...` 是纯类型声明（别名执行前被抹除，遮蔽不到运行时的
        // 内置 JSON.parse）——曾把类型别名标识符也当值绑定误拒。
        let script = concat!(
            "type JSON = { ok: boolean };\n",
            "const h = agent(\"h\");\n",
            "const r = await h.ask<JSON>(\"别名\");\n",
            "result(String(r.ok));\n"
        );
        let transform = compile_typed_asks(script).expect("类型别名 JSON 不构成冲突");
        assert_eq!(
            transform.typed_asks[0].1["properties"]["ok"]["type"],
            "boolean"
        );
        assert!(transform.exec_script.contains("JSON.parse("));
    }

    #[test]
    fn schema_expansion_is_bounded() {
        // 回归：宽别名 × 嵌套实例化是指数展开（16 字段别名嵌 5 层 ≈ 10^6 叶子），
        // 编译发生在 QuickJS 内存限制之前——必须自带节点预算，超限报错而不是耗尽
        // 宿主内存。
        let fields = (0..16)
            .map(|index| format!("f{index}: T"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut script = format!("type Wide<T> = {{ {fields} }};\n");
        script.push_str("const h = agent(\"h\");\n");
        script.push_str("const r = await h.ask<Wide<Wide<Wide<Wide<Wide<number>>>>>>(\"爆\");\n");
        let diagnostics = compile_typed_asks(&script).unwrap_err();
        assert!(
            diagnostics.iter().any(|d| d.message.contains("展开超过")),
            "{diagnostics:?}"
        );

        // 预算内的正常展开不受影响。
        let small = compile_typed_asks(
            "type Pair<T> = { a: T, b: T };\nconst h = agent(\"h\");\nconst r = await h.ask<Pair<Pair<number>>>(\"正常\");\n",
        )
        .expect("预算内展开应成功");
        assert_eq!(
            small.typed_asks[0].1["properties"]["a"]["properties"]["b"]["type"],
            "number"
        );
    }

    #[test]
    fn expansion_budget_is_shared_across_asks() {
        // 回归：预算曾按 ask 重置——很多个「中等大小」的 ask 加总照样爆内存
        //（输入远在 100k 字符限内）。全部 ask 共享同一预算（或聚合脚本长度上限），
        // 超限整体拒绝。
        let mut script = String::from("type Pair<T> = { a: T, b: T };\nconst h = agent(\"h\");\n");
        for index in 0..3000 {
            script.push_str(&format!("await h.ask<Pair<Pair<number>>>(\"q{index}\");\n"));
        }
        let diagnostics = compile_typed_asks(&script).unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("展开超过") || d.message.contains("字符上限")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn negative_numeric_literals_compile_to_signed_consts() {
        let script = "const h = agent(\"h\");\n\
                      const r = await h.ask<-1 | 0>(\"符号\");\n\
                      result(String(r));\n";
        let transform = compile_typed_asks(script).expect("负数字面量应编译");
        assert_eq!(transform.typed_asks[0].1["enum"], json!([-1, 0]));
        // 负号作用在非数字上（`-'x'`）只允许两种结果：oxc 解析失败 → 编译按「原样返回」
        // 降级（不产出任何 typed ask，语法交给 check_script 的探针把关）；或 oxc 解析成
        // UnaryExpression → 明确的「负号仅限数字字面量」诊断。绝不静默编出错误的 schema，
        // 也不得因别的原因报错。
        match compile_typed_asks("const h = agent(\"h\");\nconst r = await h.ask<-'x'>(\"q\");\n") {
            Ok(transform) => assert!(
                transform.typed_asks.is_empty(),
                "解析失败的类型不得编出 schema: {:?}",
                transform.typed_asks
            ),
            Err(diagnostics) => assert!(
                diagnostics.iter().any(|d| d.message.contains("负号")),
                "非数字负号应给出字面量诊断: {diagnostics:?}"
            ),
        }
    }

    #[test]
    fn json_binding_conflicts_are_rejected() {
        // 回归：用户声明的 `JSON` 绑定会遮蔽重写依赖的内置 JSON.parse——原脚本能过
        // tsc，重写体在 TDZ 场景直接 ReferenceError。存在 typed ask 时明确拒绝。
        let script = concat!(
            "const h = agent(\"h\");\n",
            "const JSON = await h.ask<{ v: string }>(\"问\");\n",
            "result(JSON.v);\n"
        );
        let diagnostics = compile_typed_asks(script).unwrap_err();
        assert!(
            diagnostics.iter().any(|d| d.message.contains("JSON")),
            "{diagnostics:?}"
        );
        // 没有 typed ask 时不拦（脚本不重写，用户自己的 JSON 用法与自己负责）。
        let plain = "const JSON = { parse: () => 1 };\nresult(String(JSON.parse()));\n";
        let transform = compile_typed_asks(plain).expect("无 typed ask 时不拒绝");
        assert!(transform.typed_asks.is_empty());
    }

    #[test]
    fn numeric_boundaries_keep_exact_values() {
        // 回归：i64 边界上的常量曾走饱和转换——ask<9223372036854775808>（2^63，
        // i64::MAX as f64 恰好舍入到它）被错写成 i64::MAX（...997），正确的 JSON
        // 答案被拒、不同的答案被放行。超出 i64 的保留浮点表示。
        let script = concat!(
            "const h = agent(\"h\");\n",
            "const r = await h.ask<9223372036854775808>(\"边界\");\n",
            "result(String(r));\n"
        );
        let transform = compile_typed_asks(script).expect("编译应成功");
        let (_, schema) = &transform.typed_asks[0];
        assert_eq!(
            schema["const"],
            json!(9223372036854775808.0f64),
            "2^63 保留浮点形态（不是饱和成 i64::MAX）: {schema}"
        );
        // i64 范围内的整数值仍落整数表示。
        let inside = compile_typed_asks(
            "const h = agent(\"h\");\nconst r = await h.ask<9007199254740992>(\"内\");\n",
        )
        .expect("编译应成功");
        assert_eq!(inside.typed_asks[0].1["const"], json!(9007199254740992i64));
    }

    #[test]
    fn alias_parameter_shadows_builtin_array() {
        // 回归：参数名可以遮蔽内置 Array——`type Box<Array> = { value: Array }` 的
        // body 里 Array 是本别名的参数，指向调用方实参；内置特判曾先于参数帧查找
        // 执行，把这个合法工作流误拒成「Array 需要元素类型参数」。
        let shadowed = compile_typed_asks(
            "type Box<Array> = { value: Array };\nconst h = agent(\"h\");\nconst r = await h.ask<Box<string>>(\"q\");\n",
        )
        .expect("参数遮蔽内置名应按实参解析");
        assert_eq!(
            shadowed.typed_asks[0].1["properties"]["value"]["type"],
            "string"
        );
        // 泛型代入仍要过展开预算：深实参经遮蔽内置名的参数名展开时同样受深度上限
        // 约束（编译完成后统一核对）。尖括号用 format! 构造，避免手写失衡被解析器
        // 吃成比较运算后静默降级。
        let opens = "Array<".repeat(8);
        let closes = ">".repeat(9);
        let deep = compile_typed_asks(&format!(
            "type Box<Array> = {{ value: Array }};\nconst h = agent(\"h\");\nconst r = await h.ask<Box<{opens}string{closes}>(\"深\");\n"
        ));
        assert!(deep.is_err(), "超展开深度应被拒绝");
    }

    #[test]
    fn expanded_schema_depth_is_checked_before_execution() {
        // 回归：泛型代入会把实参子树插进别名体——源类型嵌套浅 + 实参子树深的组合
        // 曾编译通过，子代理执行完之后所有答案才被校验器的深度上限打回。编译完成后
        // 按【校验器口径】（properties 递归边，别名层不再 +1）核对总深度并拒绝。
        let opens = "Box<".repeat(9);
        let closes = ">".repeat(9);
        let script = format!(
            "type Box<T> = {{ value: T }};\nconst h = agent(\"h\");\nconst r = await h.ask<{opens}string{closes}>(\"深\");\nresult(r.value);\n"
        );
        let diagnostics = compile_typed_asks(&script).unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("超过") && d.message.contains("层")),
            "{diagnostics:?}"
        );
        // 校验器口径内的嵌套不受影响（Box9 展开 = 9 条 properties 递归边，string 叶在
        // depth 9 被校验，> 8 拒绝；Box8 的叶在 depth 8，校验器放行，编译期也必须放行——
        // 叶子记 1 的旧算法曾在这里多拒一层；语法递归护栏按 MAX_TYPE_DEPTH 直接卡
        // 也会多拒：Box8 是 8 层实参 + 体 + 属性 = 10 层语法递归）。
        let ok = compile_typed_asks(
            "type Box<T> = { value: T };\nconst h = agent(\"h\");\nconst r = await h.ask<Box<Box<Box<Box<string>>>>>(\"内\");\n",
        )
        .expect("深度预算内应编译");
        let mut cursor = &ok.typed_asks[0].1;
        for _ in 0..4 {
            cursor = &cursor["properties"]["value"];
        }
        assert_eq!(cursor["type"], "string");
        let opens = "Box<".repeat(8);
        let closes = ">".repeat(8);
        let boundary = format!(
            "type Box<T> = {{ value: T }};\nconst h = agent(\"h\");\nconst r = await h.ask<{opens}string{closes}>(\"界\");\n"
        );
        let ok = compile_typed_asks(&boundary).expect("8 层嵌套与校验器同口径放行");
        assert_eq!(schema_depth(&ok.typed_asks[0].1), MAX_TYPE_DEPTH);
    }

    #[test]
    fn deeply_nested_scripts_are_rejected_before_parsing() {
        // 回归：oxc 递归下降无护栏，几 KB 的 `[[[[…]]]]` 曾把解析线程的栈打穿（Rust 栈
        // 溢出是 abort，整个进程退出）。预检在解析前拒绝，三处入口都不进 oxc。
        let depth = MAX_NESTING_DEPTH + 10;
        let bracket_bomb = format!(
            "const h = agent(\"h\");\nconst x = {}1{};\nresult(String(x));\n",
            "[".repeat(depth),
            "]".repeat(depth)
        );
        let diagnostics = compile_typed_asks(&bracket_bomb).unwrap_err();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].line, 2, "{diagnostics:?}");
        assert!(
            diagnostics[0].message.contains("嵌套过深"),
            "{diagnostics:?}"
        );
        assert!(build_dependency_graph(&bracket_bomb).is_none());
        assert!(extract_actors(&bracket_bomb).is_none());
        // 泛型嵌套同样受限（`<>` 不进括号计数）。
        let generic_bomb = format!(
            "type Box<T> = {{ value: T }};\nconst h = agent(\"h\");\nconst r = await h.ask<{}string{}>(\"深\");\n",
            "Box<".repeat(depth),
            ">".repeat(depth)
        );
        let diagnostics = compile_typed_asks(&generic_bomb).unwrap_err();
        assert!(
            diagnostics[0].message.contains("嵌套过深"),
            "{diagnostics:?}"
        );
        // 字符串 / 注释里的括号不计；比较运算符与箭头函数不触发泛型计数。
        let benign = format!(
            "const h = agent(\"h\");\n// {}\nconst s = \"{}\";\nconst f = (a, b) => a < b && a <= b && b >= a && b > a;\nfor (let i = 0; i < 3; i++) {{ h.ask(`x${{i}}`); }}\nresult(s);\n",
            "(".repeat(depth),
            "{".repeat(depth)
        );
        compile_typed_asks(&benign).expect("遮蔽后的括号与运算符不触发深度预检");
        assert!(build_dependency_graph(&benign).is_some());
        // 上限之内的真实嵌套照常解析（大栈线程兜住 oxc 的递归）。
        let within = format!(
            "const h = agent(\"h\");\nconst x = {}1{};\nresult(String(x));\n",
            "[".repeat(MAX_NESTING_DEPTH - 4),
            "]".repeat(MAX_NESTING_DEPTH - 4)
        );
        compile_typed_asks(&within).expect("上限内的嵌套应解析");
        assert!(build_dependency_graph(&within).is_some());
    }

    #[test]
    fn multiline_typed_asks_keep_following_line_numbers() {
        // 回归：跨多行的 `ask<T>(…)` 重写成单行后，后续语句整体上移——运行期错误行号
        // 与作者脚本对不上。替换体补回被吃掉的换行。
        let script = "const h = agent(\"h\");\n\
                      const r = await h.ask<{\n  name: string;\n  age: number;\n}>(\n  \"多行\"\n);\n\
                      result(r.name);\n";
        let transform = compile_typed_asks(script).expect("多行 typed ask 应编译");
        assert_eq!(
            transform.exec_script.lines().count(),
            script.lines().count(),
            "{}",
            transform.exec_script
        );
        let original_line = script
            .lines()
            .position(|line| line.contains("result(r.name);"))
            .expect("原行存在");
        let rewritten_line = transform
            .exec_script
            .lines()
            .position(|line| line.contains("result(r.name);"))
            .expect("重写后仍在");
        assert_eq!(rewritten_line, original_line);
    }

    #[test]
    fn extra_type_arguments_are_rejected() {
        // 回归：`ask<A, B>` 曾只取 A、静默丢掉 B——作者以为 B 参与了校验。
        let script = "const h = agent(\"h\");\nconst r = await h.ask<string, number>(\"多参\");\n";
        let diagnostics = compile_typed_asks(script).unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("只接受一个类型参数")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn string_typed_ask_stays_plain_ask() {
        // `ask<string>` 与默认的 ask(question): Promise<string> 同义：不该要求子代理回
        // JSON 字符串字面量，只抹掉类型实参。
        let script =
            "const h = agent(\"h\");\nconst r = await h?.ask<string>(\"文本\");\nresult(r);\n";
        let transform = compile_typed_asks(script).expect("ask<string> 应编译");
        assert!(
            transform.exec_script.contains("h?.ask(\"文本\")"),
            "{}",
            transform.exec_script
        );
        assert!(
            !transform.exec_script.contains("askJson"),
            "{}",
            transform.exec_script
        );
    }

    #[test]
    fn duplicate_alias_declarations_are_rejected() {
        // 回归：扁平别名表无法表达作用域，函数内同名声明曾静默覆盖顶层——外层调用
        // 点用上内层类型。现在明确拒绝（不猜测作用域）。
        let script = concat!(
            "type Answer = string;\n",
            "function helper() { type Answer = number; }\n",
            "const h = agent(\"h\");\n",
            "const r = await h.ask<Answer>(\"问\");\n",
            "result(r);\n"
        );
        let diagnostics = compile_typed_asks(script).unwrap_err();
        assert!(
            diagnostics.iter().any(|d| d.message.contains("重复声明")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn schema_is_embedded_via_json_parse_not_object_literal() {
        // 回归：schema 直接内联 JS 对象字面量时 `__proto__` 键变成原型设置而非自有
        // 属性（JSON.stringify 随即丢掉该约束）——必须经 JSON.parse 字符串嵌入。
        let script = concat!(
            "const h = agent(\"h\");\n",
            "const r = await h.ask<{ \"__proto__\": string }>(\"原型键\");\n",
            "result(JSON.stringify(r));\n"
        );
        let transform = compile_typed_asks(script).expect("编译应成功");
        assert!(
            transform.exec_script.contains("JSON.parse("),
            "{}",
            transform.exec_script
        );
        // schema 本体保留该属性（校验在 Rust 侧按 Value 走，不受嵌入形态影响）。
        let (_, schema) = &transform.typed_asks[0];
        assert!(schema["properties"].get("__proto__").is_some(), "{schema}");
    }

    #[test]
    fn reused_promise_joins_do_not_create_self_edges() {
        // 回归：同一个 promise 进两个 Promise.all 时节点曾重复入组——组间连边出现
        // 自环，Kahn 分层报不出波次。
        let script = "const a = agent(\"a\");\n\
                      const p = a.ask(\"只问一次\");\n\
                      const [x] = await Promise.all([p]);\n\
                      const [y] = await Promise.all([p]);\n";
        let graph = build_dependency_graph(script).expect("AST 解析应成功");
        let ids: Vec<String> = graph.nodes.iter().map(|n| n.id.clone()).collect();
        assert_eq!(ids, vec!["ask-2"], "复用 promise 只产生一个 ask 节点");
        assert!(
            !graph.edges.iter().any(|(from, to)| from == to),
            "不得有自环边: {:?}",
            graph.edges
        );
        assert!(!graph.waves.is_empty(), "无环才能分层: {:?}", graph.waves);
    }
}
