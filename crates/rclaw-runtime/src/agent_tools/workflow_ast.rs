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
use oxc_ast::ast::*;
use oxc_allocator::CloneIn;
use oxc_ast_visit::walk::{walk_call_expression, walk_variable_declarator};
use oxc_ast_visit::Visit;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use serde_json::{json, Value};

use super::workflow::{DependencyGraph, DependencyNode, FacadeDiagnostic};

/// 包裹头：与执行引擎的 async IIFE 语义一致（顶层 await / return 合法）。
const WRAPPER_HEAD: &str = "(async function __wf__(){\n";
const WRAPPER_TAIL: &str = "\n})";
/// 类型嵌套深度上限（与 Rust 侧 schema 校验器的深度上限同源：8）。
const MAX_TYPE_DEPTH: usize = 8;
/// 类型别名解析深度上限（防 `type A = B; type B = A;` 循环）。
const MAX_ALIAS_DEPTH: usize = 8;

/// 解析包裹后的脚本（`wrapped` 由调用方持有，AST 与之同生命周期）。解析失败（含可恢复
/// 语法错误）返回 `None`：调用方降级（依赖图）或诊断（typed ask 编译）。
fn parse<'a>(allocator: &'a Allocator, wrapped: &'a str) -> Option<Program<'a>> {
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

/// AST 版依赖图（第十一轮文本版的升级）。解析失败返回 `None`（调用方降级文本解析）。
pub(crate) fn build_dependency_graph(script: &str) -> Option<DependencyGraph> {
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
pub(crate) fn extract_actors(script: &str) -> Option<Vec<String>> {
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
    match call.arguments.first().and_then(|argument| argument.as_expression()) {
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
        if let Some(expr) = call.arguments.first().and_then(|argument| argument.as_expression()) {
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
        // `const p = h.ask(...)` / `const p = await h.ask(...)` → promise 绑定
        //（初始化式里恰好一个 ask 节点时才算——多个 ask 的表达式无法归一到单节点）。
        if let Some(_ask) = ask_call_of(init) {
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
pub(crate) fn compile_typed_asks(script: &str) -> Result<TypedAskTransform, Vec<FacadeDiagnostic>> {
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
    };
    walker.visit_program(&program);
    if !walker.diagnostics.is_empty() {
        return Err(walker.diagnostics);
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
        let (start, end) = (
            it.span.start as usize,
            it.span.end as usize,
        );
        if start >= WRAPPER_HEAD.len() && end >= WRAPPER_HEAD.len() {
            self.spans.push((start - WRAPPER_HEAD.len(), end - WRAPPER_HEAD.len()));
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

struct TypedAskWalker<'a> {
    /// (script 起始偏移, 结束偏移, 替换文本)。
    replacements: Vec<(usize, usize, String)>,
    diagnostics: Vec<FacadeDiagnostic>,
    typed_asks: Vec<(usize, Value)>,
    aliases: &'a HashMap<String, AliasDef<'a>>,
    wrapped: &'a str,
}

impl<'a> TypedAskWalker<'a> {
    fn diag(&mut self, offset: u32, message: String) {
        let source = &self.wrapped[WRAPPER_HEAD.len()..];
        let (line, col) = line_col(source, offset);
        self.diagnostics.push(FacadeDiagnostic { line, col, message });
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
        let (property, object) = match &it.callee {
            Expression::StaticMemberExpression(member) => (&member.property.name, &member.object),
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
        let Some(type_node) = type_arguments.params.first() else {
            self.diag(it.span.start, "ask<> 的类型参数列表为空".to_string());
            walk_call_expression(self, it);
            return;
        };
        let mut compiler = SchemaCompiler {
            aliases: self.aliases,
            type_args: Vec::new(),
            diagnostics: Vec::new(),
        };
        let schema = compiler.compile(type_node, 0, 0);
        for (offset, message) in compiler.diagnostics {
            self.diag(offset, message);
        }
        if self.diagnostics.is_empty() {
            let schema_json = schema.to_string();
            let receiver = self.slice(object.span()).trim();
            let question_src = self.slice(question.span());
            // schema 以 JSON 文本 → JS 字符串字面量 → JSON.parse 嵌入：直接内联对象
            // 字面量会把 `__proto__` 键变成原型设置而非自有属性（JSON.stringify 随即
            // 丢掉该约束，校验被静默放宽）；parse 路径的键语义与 JSON 完全一致。
            let schema_literal = serde_json::to_string(&schema_json).unwrap_or_default();
            let replacement = format!("{receiver}.askJson({question_src}, JSON.parse({schema_literal}))");
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
    /// 泛型实例化栈：别名参数名 → 实参【已编译 schema】。实参在【调用方】的类型
    /// 环境里先解析再入栈——若按原始引用入栈，`type Outer<T> = Inner<T>` 的 Inner
    /// 帧会把参数 T 绑到「自己的 T」，body 里再引用就在同一帧里自吞尾巴（反复命中
    /// 直到深度上限，合法脚本被误拒）。compile_reference 命中参数名时栈顶向下查。
    type_args: Vec<HashMap<String, Value>>,
    diagnostics: Vec<(u32, String)>,
}

impl<'a> SchemaCompiler<'a> {
    fn compile(&mut self, ty: &'a TSType<'a>, depth: usize, alias_depth: usize) -> Value {
        if depth > MAX_TYPE_DEPTH {
            self.diagnostics.push((
                ty.span().start,
                format!("类型嵌套超过 {MAX_TYPE_DEPTH} 层上限"),
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
                     数组（T[] 或 Array<T>）/ 对象字面量 / 顶层 type 别名（无泛型）"
                        .to_string(),
                ));
                json!({})
            }
        }
    }

    fn compile_literal(&mut self, literal: &TSLiteral<'a>) -> Value {
        match literal {
            TSLiteral::StringLiteral(text) => json!({"const": text.value.as_str()}),
            TSLiteral::NumericLiteral(number) => json!({"const": number.value}),
            TSLiteral::BooleanLiteral(flag) => json!({"const": flag.value}),
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
                    self.diagnostics.push((
                        property.span.start,
                        format!("属性 `{key}` 缺少类型注解"),
                    ));
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
        // Array<T>。
        if name == "Array" && self.aliases.get(name).is_none() {
            let Some(params) = &reference.type_arguments else {
                self.diagnostics.push((span_start, "Array 需要元素类型参数".to_string()));
                return json!({});
            };
            let Some(element) = params.params.first() else {
                self.diagnostics.push((span_start, "Array<> 的类型参数列表为空".to_string()));
                return json!({});
            };
            return json!({"type": "array", "items": self.compile(element, depth + 1, alias_depth)});
        }
        // 泛型实例化栈优先：别名 body 内引用参数名（T / U…）→ 返回调用方环境里
        // 已解析的实参 schema。
        for frame in self.type_args.iter().rev() {
            if let Some(schema) = frame.get(name) {
                return schema.clone();
            }
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
            self.diagnostics.push((span_start, "类型别名循环引用".to_string()));
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
        // 入栈编译 body（body 里引用参数名的 TSTypeReference 在上方栈查找命中已解析
        // 的 schema，不会再撞上 callee 自己的同名参数）。
        let mut frame: HashMap<String, Value> = HashMap::new();
        for (param, argument) in alias.params.iter().zip(arguments) {
            frame.insert(param.clone(), self.compile(argument, depth + 1, alias_depth));
        }
        self.type_args.push(frame);
        let compiled = self.compile(&alias.body, depth + 1, alias_depth + 1);
        self.type_args.pop();
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
        assert_eq!(node_ids(&graph), vec!["ask-1", "ask-1-2", "ask-1-3", "ask-1-4"]);
        assert!(!graph.nodes[0].parallel, "首个串行 ask 不在波内");
        assert!(graph.nodes[1].parallel && graph.nodes[2].parallel);
        assert!(!graph.nodes[3].parallel);
        // 边：串行出口 → 波内每个成员 → 末尾串行（4 条：1→2、1→3、2→4、3→4）。
        assert_eq!(graph.edges.len(), 4, "{:?}", graph.edges);
        assert!(graph.edges.contains(&("ask-1".into(), "ask-1-2".into())));
        assert!(graph.edges.contains(&("ask-1".into(), "ask-1-3".into())));
        assert!(!graph.edges.contains(&("ask-1-2".into(), "ask-1".into())), "{:?}", graph.edges);
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
        assert!(transform.exec_script.contains("\\\"type\\\":\\\"object\\\""));
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
        assert_eq!(alias_schema["properties"]["zone"]["enum"], json!(["a", "b"]));
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
        assert!(diagnostics.iter().any(|d| d.message.contains("any")), "{diagnostics:?}");

        let bad_ref = "const h = agent(\"h\");\nawait h.ask<Date>(\"时间\");\n";
        let diagnostics = compile_typed_asks(bad_ref).unwrap_err();
        assert!(diagnostics.iter().any(|d| d.message.contains("未知类型")), "{diagnostics:?}");

        let bad_args = "const h = agent(\"h\");\nawait h.ask<{a: string}>(\"q\", \"多余\");\n";
        let diagnostics = compile_typed_asks(bad_args).unwrap_err();
        assert!(
            diagnostics.iter().any(|d| d.message.contains("一个问题参数")),
            "{diagnostics:?}"
        );

        let bad_json = "const h = agent(\"h\");\nawait h.askJson<{a: string}>(\"q\", {});\n";
        let diagnostics = compile_typed_asks(bad_json).unwrap_err();
        assert!(diagnostics.iter().any(|d| d.message.contains("askJson")), "{diagnostics:?}");
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
        let bad = compile_typed_asks(
            "const h = agent(\"h\");\nawait h.ask<[string, number?]>(\"q\");\n",
        )
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
        assert!(bad.iter().any(|d| d.message.contains("1 个类型参数")), "{bad:?}");
        // 嵌套实例化：Box<Box<string>>。
        let nested = compile_typed_asks(
            "type Box<T> = { value: T };\nconst h = agent(\"h\");\nawait h.ask<Box<Box<string>>>(\"q\");\n",
        )
        .expect("嵌套实例化应成功");
        let (_, schema) = &nested.typed_asks[0];
        assert_eq!(schema["properties"]["value"]["properties"]["value"]["type"], "string");

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
        assert!(transform.exec_script.contains("result(r.name);"), "后续代码原样保留");
    }

    #[test]
    fn nested_typed_asks_are_rejected_with_diagnostic() {
        // 回归：嵌套 typed ask 的替换 span 相互包含，按原文 span 重写会产出残缺 JS——
        // 编译期明确拒绝（模型把内层存变量即可绕开）。
        let script = "const h = agent(\"h\");\n\
                      const r = await h.ask<{ v: string }>(`外层 ${await h.ask<string>(\"内层\")}`);\n";
        let diagnostics = compile_typed_asks(script).unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("嵌套不支持")),
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
