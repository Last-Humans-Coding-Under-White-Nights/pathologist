//! IPC proxy/stub bridge detection.
//!
//! OpenHarmony services communicate over Binder IPC: a proxy method calls
//! `remote->SendRequest(...)` and a stub dispatches in `OnRemoteRequest`.
//! When both proxy and stub live under the analyzed root, we can connect them
//! with a synthetic call edge so the call graph has no gap at the IPC
//! boundary.
//!
//! Detection is purely name-based — no control-flow / opcode analysis:
//!
//! - A **stub** class is identified by its name ending in `Stub`.
//! - A **proxy** class is identified by its name ending in `Proxy` or `Client` plus
//!   the presence of a call whose final callable segment is `SendRequest`.
//! - Bridges pair proxy methods to stub handlers by interface class name +
//!   method name correspondence (e.g. `FooProxy::Bar` → `FooStub::Bar`).

use rustc_hash::{FxHashMap, FxHashSet};
use trace_ir::{FnId, IpcBridge, Program, TypeKind};

/// `(qualified_class_name, handler FnIds)` for each detected stub class.
type StubClasses = Vec<(String, Vec<FnId>)>;
/// `(qualified_class_name, simple_method_name, method_fn)` for each
/// IPC-sending proxy method.
type ProxyMethods = Vec<(String, String, FnId)>;

/// Detect proxy/stub IPC bridges in a post-merge program.
///
/// Pure: reads `program` and returns the matched bridges. Runs after merge,
/// during PAG build. No `Program` mutation required.
pub fn detect_ipc_pairs(program: &Program) -> Vec<IpcBridge> {
    let (stubs, proxies) = scan(program);

    let mut stub_index: FxHashMap<String, &Vec<FnId>> = FxHashMap::default();
    for (class, handlers) in &stubs {
        stub_index.insert(class.clone(), handlers);
    }

    let mut bridges: Vec<IpcBridge> = Vec::new();
    for (proxy_class, method, proxy_method) in &proxies {
        let stub_class = derive_stub_class(proxy_class);
        let Some(handlers) = stub_index.get(&stub_class) else {
            continue;
        };

        // 1. Try Opcode-based symbolic/eval dispatch resolution first
        let opcode_matches = match_proxy_to_stub_by_opcode(
            program,
            *proxy_method,
            &stub_class,
            handlers,
        );
        if !opcode_matches.is_empty() {
            if std::env::var_os("TRACE_DEBUG_IPC").is_some() {
                let p_name = &program.symbols.function(*proxy_method).name;
                for h in &opcode_matches {
                    let h_name = &program.symbols.function(*h).name;
                    eprintln!("trace: ipc opcode bridge: {p_name} -> {h_name}");
                }
            }
            bridges.extend(opcode_matches.into_iter().map(|stub_handler| IpcBridge {
                proxy_method: *proxy_method,
                stub_handler,
                descriptor: String::new(),
            }));
            continue;
        }

        // 2. Name-based match fallback
        let matched_handlers = find_handlers(program, handlers, method);
        if !matched_handlers.is_empty() {
            bridges.extend(matched_handlers.into_iter().map(|stub_handler| IpcBridge {
                proxy_method: *proxy_method,
                stub_handler,
                descriptor: String::new(),
            }));
            continue;
        }

        // Fallback: stub has no handler methods (only dispatcher + boilerplate).
        // The stub's OnRemoteRequest switch calls interface methods directly on
        // `this` (inherited from the parent interface). Match proxy methods
        // against external (interface) functions with the same simple name.
        bridges.extend(
            find_interface_methods(program, &stub_class, method)
                .into_iter()
                .map(|stub_handler| IpcBridge {
                    proxy_method: *proxy_method,
                    stub_handler,
                    descriptor: String::new(),
                }),
        );
    }
    bridges
}

/// Returns the stub classes and the IPC-sending proxy methods collected from
/// a post-merge program.
fn scan(program: &Program) -> (StubClasses, ProxyMethods) {
    // Index IPC-sending methods once. Scanning the entire call-site list for
    // every proxy method is quadratic on proxy-heavy trees.
    let senders: FxHashSet<FnId> = program
        .symbols
        .call_sites
        .iter()
        .filter(|cs| final_callable_segment(&cs.callee_name) == "SendRequest")
        .map(|cs| cs.caller)
        .collect();

    // Index all defined C++ methods by their qualified class.
    // (qualified_class → (simple_method_name, FnId)).
    let mut methods_by_class: FxHashMap<String, Vec<(String, FnId)>> = FxHashMap::default();
    for f in &program.symbols.functions {
        if !f.is_defined || !f.is_cpp {
            continue;
        }
        let Some((class, method)) = split_qualified(&f.name) else {
            continue;
        };
        methods_by_class
            .entry(class)
            .or_default()
            .push((method, f.id));
    }

    let mut stubs: StubClasses = Vec::new();
    let mut proxies: ProxyMethods = Vec::new();
    let mut seen_stub = std::collections::HashSet::new();

    for (class, methods) in &methods_by_class {
        if is_stub_class(class) {
            // A stub class: handlers are its methods that are not the
            // dispatcher/descriptor boilerplate.
            let handlers: Vec<FnId> = methods
                .iter()
                .filter(|(m, _)| !is_stub_entry(m) && !is_boilerplate(class, m))
                .map(|(_, id)| *id)
                .collect();
            // Register stubs with no handler methods too — the interface
            // fallback needs to find them. A stub must have either handler
            // methods OR OnRemoteRequest (the dispatcher) to be registered.
            let has_dispatcher = methods.iter().any(|(m, _)| is_stub_entry(m));
            if (!handlers.is_empty() || has_dispatcher) && seen_stub.insert(class.clone()) {
                stubs.push((class.clone(), handlers));
            }
        } else if is_proxy_class(class) {
            // A proxy class: its methods that call SendRequest are IPC sends.
            for (method, id) in methods {
                if senders.contains(id) {
                    proxies.push((class.clone(), method.clone(), *id));
                }
            }
        }
    }

    (stubs, proxies)
}

/// Derive the matching stub class name from a proxy/client class name.
/// `FooProxy` → `FooStub`, `FooClient` → `FooStub`.
fn derive_stub_class(proxy_class: &str) -> String {
    if let Some(base) = proxy_class.strip_suffix("Proxy") {
        return format!("{base}Stub");
    }
    if let Some(base) = proxy_class.strip_suffix("Client") {
        return format!("{base}Stub");
    }
    proxy_class.to_string()
}

/// Find stub handlers matching a proxy method name. Tries, in order:
/// exact name, a `Handle` prefix variant, then a `Stub` suffix variant
/// (the marshalling shim name used by some IDL generators).
/// Returns every overload at the first tier with any matches: name-based IPC
/// detection cannot distinguish overloads, so may-analysis retains them all.
/// Candidate resolution follows the symbol table's scope/overload rules and
/// keeps only definitions; declarations are left to the interface fallback.
fn find_handlers(program: &Program, handlers: &[FnId], method_name: &str) -> Vec<FnId> {
    for name in [
        method_name.to_string(),
        format!("Handle{method_name}"),
        format!("{method_name}Stub"),
    ] {
        let matching_entries: Vec<FnId> = handlers
            .iter()
            .copied()
            .filter(|&id| {
                program
                    .symbols
                    .function(id)
                    .name
                    .rsplit_once("::")
                    .is_some_and(|(_, method)| method == name)
            })
            .collect();
        let mut seen = FxHashSet::default();
        let mut matches = Vec::new();
        for id in matching_entries {
            let matched = program.symbols.function(id);
            for candidate in program
                .symbols
                .resolve_function_candidates(&matched.name, Some(matched.file))
            {
                let function = program.symbols.function(candidate);
                if function.is_defined && seen.insert(candidate) {
                    matches.push(candidate);
                }
            }
        }
        if !matches.is_empty() {
            return matches;
        }
    }
    Vec::new()
}

/// Fallback for stubs with no handler methods: find an inherited interface
/// function whose simple name matches the proxy method. The stub's
/// `OnRemoteRequest` switch calls these interface methods directly on `this`
/// (inherited from the parent interface class).
fn find_interface_methods(program: &Program, stub_class: &str, method_name: &str) -> Vec<FnId> {
    // Prefer concrete in-tree implementations deriving from the stub. This
    // keeps reachability alive beyond the IPC boundary when the stub itself
    // only declares the interface methods.
    let mut concrete = Vec::new();
    let mut seen = FxHashSet::default();
    for class in program.subclass_closure(stub_class).into_iter().skip(1) {
        let name = format!("{class}::{method_name}");
        for id in program.symbols.resolve_function_candidates(&name, None) {
            if program.symbols.function(id).is_defined && seen.insert(id) {
                concrete.push(id);
            }
        }
    }
    if !concrete.is_empty() {
        return concrete;
    }

    // Restrict ancestor fallbacks to actual base classes of the stub. A
    // namespace/name heuristic can otherwise select an unrelated class that
    // happens to expose the same method. `IRemoteStub<IFoo>` is represented
    // by the ordinary `IRemoteStub` inheritance edge plus a preserved
    // template-base spelling, from which we recover `IFoo`.
    let mut interface_classes = FxHashSet::default();
    let mut pending = vec![stub_class.to_string()];
    let mut expanded = FxHashSet::default();
    while let Some(class) = pending.pop() {
        if !expanded.insert(class.clone()) {
            continue;
        }
        for base in program.bases_of(&class) {
            if interface_classes.insert(base.clone()) {
                pending.push(base);
            }
        }
        for template_base in program.template_bases_of(&class) {
            let candidates = remote_stub_interface_candidates(
                &template_base.spelling,
                &template_base.declaration_scope,
            );
            if let Some(interface) = candidates
                .iter()
                .find(|candidate| interface_class_exists(program, candidate))
                .or_else(|| candidates.first())
                .cloned()
            {
                if interface_classes.insert(interface.clone()) {
                    pending.push(interface);
                }
            }
        }
    }

    program
        .symbols
        .functions
        .iter()
        .filter(|f| {
            f.name
                .rsplit_once("::")
                .is_some_and(|(_, method)| method == method_name)
                && {
                    let class_part = f.name.rsplit_once("::").map(|(c, _)| c).unwrap_or("");
                    interface_classes.contains(class_part)
                }
        })
        .map(|f| f.id)
        .collect()
}

/// Recover the interface argument from an exact `IRemoteStub<Interface>`
/// base spelling, ordered by C++ lexical lookup preference. Relative names,
/// including `api::IFoo`, search from the derived class's declaration scope
/// outward; only a leading `::` forces global lookup. The wrapper's own
/// qualification does not affect the argument. Nested templates in the first
/// argument are preserved; later template arguments are ignored.
fn remote_stub_interface_candidates(template_base: &str, declaration_scope: &str) -> Vec<String> {
    let Some(open) = template_base.find('<') else {
        return Vec::new();
    };
    let wrapper = template_base[..open].trim();
    if wrapper.rsplit("::").next() != Some("IRemoteStub") {
        return Vec::new();
    }

    let mut depth = 0_u32;
    let mut end = None;
    for (offset, ch) in template_base[open + 1..].char_indices() {
        match ch {
            '<' => depth += 1,
            '>' if depth == 0 => {
                end = Some(open + 1 + offset);
                break;
            }
            '>' => depth -= 1,
            ',' if depth == 0 => {
                end = Some(open + 1 + offset);
                break;
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return Vec::new();
    };
    let interface = template_base[open + 1..end].trim();
    if interface.is_empty() {
        return Vec::new();
    }
    if let Some(global) = interface.strip_prefix("::") {
        return vec![global.to_string()];
    }

    let scope_segments: Vec<&str> = declaration_scope
        .split("::")
        .filter(|segment| !segment.is_empty())
        .collect();
    let mut candidates = Vec::with_capacity(scope_segments.len() + 1);
    for len in (0..=scope_segments.len()).rev() {
        let candidate = if len == 0 {
            interface.to_string()
        } else {
            format!("{}::{interface}", scope_segments[..len].join("::"))
        };
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

/// Whether a lexical interface candidate is represented in the merged IR.
/// Types are the primary signal; the other facts keep recovery working for
/// incomplete/error-tolerant parses where a method or inheritance edge
/// survived but the class tag did not.
fn interface_class_exists(program: &Program, class: &str) -> bool {
    program
        .types
        .type_id_by_tag(class, TypeKind::Struct)
        .is_some()
        || program.symbols.functions.iter().any(|function| {
            function
                .name
                .rsplit_once("::")
                .is_some_and(|(owner, _)| owner == class)
        })
        || program.has_inheritance_edges(class)
}

fn is_stub_class(class: &str) -> bool {
    class.ends_with("Stub")
}

fn is_stub_entry(method: &str) -> bool {
    method == "OnRemoteRequest"
}

fn is_boilerplate(class: &str, method: &str) -> bool {
    let class_name = class.rsplit("::").next().unwrap_or(class);
    method == class_name || method == "GetDescriptor" || method.starts_with('~')
}

fn is_proxy_class(class: &str) -> bool {
    class.ends_with("Proxy") || class.ends_with("Client")
}

/// Final callable segment of either a qualified name or a member expression.
/// Lowering may retain `remote->` / `remote.` when the receiver type cannot
/// be resolved, so those separators have to be handled alongside `::`.
fn final_callable_segment(name: &str) -> &str {
    name.rsplit([':', '>', '.']).next().unwrap_or(name)
}

/// Split a qualified C++ function name into `(class, method)`.
/// Returns `None` for plain/non-member functions and destructors.
fn split_qualified(name: &str) -> Option<(String, String)> {
    let mut parts: Vec<&str> = name.split("::").collect();
    if parts.len() < 2 {
        return None;
    }
    let method = parts.pop().unwrap().to_string();
    if method.starts_with('~') {
        return None;
    }
    let class = parts.join("::");
    Some((class, method))
}

/// Match a proxy method to stub handler(s) using symbolic opcode equality.
fn match_proxy_to_stub_by_opcode(
    program: &Program,
    proxy_method: FnId,
    stub_class: &str,
    handlers: &[FnId],
) -> Vec<FnId> {
    let sends: Vec<&trace_ir::IpcSend> = program
        .ipc_sends
        .iter()
        .filter(|s| s.proxy_method == proxy_method)
        .collect();
    if sends.is_empty() {
        return Vec::new();
    }

    let mut stub_classes = vec![stub_class.to_string()];
    for base in program.bases_of(stub_class) {
        if is_stub_class(&base) {
            stub_classes.push(base);
        }
    }

    let dispatches: Vec<&trace_ir::IpcDispatch> = program
        .ipc_dispatches
        .iter()
        .filter(|d| {
            let name = &program.symbols.function(d.stub_fn).name;
            stub_classes.iter().any(|cls| {
                name.starts_with(cls)
                    && name.get(cls.len()..).is_some_and(|rest| rest.starts_with("::"))
            })
        })
        .collect();
    if dispatches.is_empty() {
        return Vec::new();
    }

    let mut matched = Vec::new();
    let mut seen = FxHashSet::default();

    for send in &sends {
        for disp in &dispatches {
            if opcodes_match(send, disp, program) {
                for callee in &disp.callee_names {
                    let candidates = find_handlers(program, handlers, callee);
                    if !candidates.is_empty() {
                        for h in candidates {
                            if seen.insert(h) {
                                matched.push(h);
                            }
                        }
                    } else {
                        let iface_candidates = find_interface_methods(program, stub_class, callee);
                        for h in iface_candidates {
                            if seen.insert(h) {
                                matched.push(h);
                            }
                        }
                    }
                }
            }
        }
    }

    matched
}

fn opcodes_match(
    send: &trace_ir::IpcSend,
    disp: &trace_ir::IpcDispatch,
    program: &Program,
) -> bool {
    // 1. Literal integer evaluation match
    let v1 = send
        .opcode_val
        .or_else(|| eval_opcode_expr(&send.opcode_normalized, &program.defines, &program.enum_constants));
    let v2 = disp
        .opcode_val
        .or_else(|| eval_opcode_expr(&disp.opcode_normalized, &program.defines, &program.enum_constants));
    if let (Some(v1), Some(v2)) = (v1, v2) {
        return v1 == v2;
    }
    if v1.is_some() && v2.is_some() {
        return false;
    }


    // 2. Normalized expression textual match
    if !send.opcode_normalized.is_empty() && send.opcode_normalized == disp.opcode_normalized {
        return true;
    }

    // 3. Qualified vs unqualified terminal identifier match
    let p_short = send
        .opcode_normalized
        .rsplit("::")
        .next()
        .unwrap_or(&send.opcode_normalized);
    let s_short = disp
        .opcode_normalized
        .rsplit("::")
        .next()
        .unwrap_or(&disp.opcode_normalized);
    if !p_short.is_empty()
        && p_short == s_short
        && p_short.chars().all(|c| c.is_alphanumeric() || c == '_')
    {
        return true;
    }

    // 4. SMT BitVector symbolic solving
    #[cfg(feature = "smt")]
    {
        if smt_ipc::solve_opcode_equality_smt(
            &send.opcode_normalized,
            &disp.opcode_normalized,
            program,
        ) {
            return true;
        }
    }

    false
}

fn eval_opcode_expr(
    expr: &str,
    defines: &indexmap::IndexMap<String, String>,
    enums: &rustc_hash::FxHashMap<String, u64>,
) -> Option<u64> {
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }
    if let Some(v) = parse_int_literal(expr) {
        return Some(v as u64);
    }
    if let Some(&v) = enums.get(expr) {
        return Some(v);
    }
    let short = expr.rsplit("::").next().unwrap_or(expr);
    if let Some(&v) = enums.get(short) {
        return Some(v);
    }
    if let Some(def_val) = defines.get(expr).or_else(|| defines.get(short)) {
        let def_clean = normalize_opcode_expr(def_val);
        if let Some(v) = parse_int_literal(&def_clean) {
            return Some(v as u64);
        }
        if let Some(&v) = enums
            .get(&def_clean)
            .or_else(|| enums.get(def_clean.rsplit("::").next().unwrap_or(&def_clean)))
        {
            return Some(v);
        }
    }
    if let Some((left, right)) = expr.rsplit_once('+') {
        let left_v = eval_opcode_expr(left.trim(), defines, enums)?;
        let right_v = eval_opcode_expr(right.trim(), defines, enums)?;
        return Some(left_v.wrapping_add(right_v));
    }
    if let Some((left, right)) = expr.rsplit_once('-') {
        let left_v = eval_opcode_expr(left.trim(), defines, enums)?;
        let right_v = eval_opcode_expr(right.trim(), defines, enums)?;
        return Some(left_v.wrapping_sub(right_v));
    }
    None
}

/// Strip `static_cast<...>(...)`, `(type)(...)`, outer parentheses, and collapse whitespace.
fn normalize_opcode_expr(mut s: &str) -> String {

    s = s.trim();
    loop {
        if let Some(rest) = s
            .strip_prefix("static_cast")
            .or_else(|| s.strip_prefix("reinterpret_cast"))
            .or_else(|| s.strip_prefix("const_cast"))
        {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('<') {
                if let Some(pos) = rest.find('>') {
                    let after = rest[pos + 1..].trim_start();
                    if after.starts_with('(') && after.ends_with(')') {
                        s = after[1..after.len() - 1].trim();
                        continue;
                    }
                }
            }
        }
        if let Some(rest) = s
            .strip_prefix("(uint32_t)")
            .or_else(|| s.strip_prefix("(uint32)"))
            .or_else(|| s.strip_prefix("(int32_t)"))
            .or_else(|| s.strip_prefix("(int)"))
            .or_else(|| s.strip_prefix("(uint64_t)"))
            .or_else(|| s.strip_prefix("(unsigned int)"))
        {
            s = rest.trim();
            continue;
        }
        if s.starts_with('(') && s.ends_with(')') {
            let mut depth = 0;
            let mut balanced_at_end = false;
            for (i, c) in s.char_indices() {
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                    if depth == 0 && i == s.len() - 1 {
                        balanced_at_end = true;
                    } else if depth == 0 {
                        break;
                    }
                }
            }
            if balanced_at_end {
                s = s[1..s.len() - 1].trim();
                continue;
            }
        }
        break;
    }
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.replace(" :: ", "::").replace(":: ", "::").replace(" ::", "::")
}

fn parse_int_literal(s: &str) -> Option<i64> {
    let s = s.trim();
    let (neg, s) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest.trim())
    } else if let Some(rest) = s.strip_prefix('+') {
        (false, rest.trim())
    } else {
        (false, s)
    };
    let val = if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(rest, 16).ok()?
    } else if let Some(rest) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        i64::from_str_radix(rest, 2).ok()?
    } else if s.len() > 1 && s.starts_with('0') && s.chars().all(|c| ('0'..='7').contains(&c)) {
        i64::from_str_radix(&s[1..], 8).ok()?
    } else {
        s.parse::<i64>().ok()?
    };
    Some(if neg { -val } else { val })
}

#[cfg(feature = "smt")]
mod smt_ipc {
    use rustc_hash::FxHashMap;
    use trace_ir::Program;
    use z3::ast::{Ast, Bool, BV};
    use z3::{Config, Context, SatResult, Solver};

    use super::{normalize_opcode_expr, parse_int_literal};

    fn find_top_level_ternary(s: &str) -> Option<(usize, usize)> {
        let mut depth: u32 = 0;
        let mut q_pos = None;
        for (i, c) in s.char_indices() {
            match c {
                '(' | '<' => depth += 1,
                ')' | '>' => depth = depth.saturating_sub(1),
                '?' if depth == 0 && q_pos.is_none() => {
                    q_pos = Some(i);
                }
                ':' if depth == 0 && q_pos.is_some() => {
                    return Some((q_pos.unwrap(), i));
                }
                _ => {}
            }
        }
        None
    }

    fn find_last_top_level_op<'a>(s: &str, ops: &[&'a str]) -> Option<(usize, &'a str)> {
        let mut depth: u32 = 0;
        let mut last = None;
        let bytes = s.as_bytes();

        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'(' | b'<' => depth += 1,
                b')' | b'>' => depth = depth.saturating_sub(1),
                _ if depth == 0 => {
                    for &op in ops {
                        if s[i..].starts_with(op) {
                            if op == "+" && s[i..].starts_with("++") {
                                continue;
                            }
                            if op == "-" && (s[i..].starts_with("--") || s[i..].starts_with("->")) {
                                continue;
                            }
                            if op == "&" && s[i..].starts_with("&&") {
                                continue;
                            }
                            if op == "|" && s[i..].starts_with("||") {
                                continue;
                            }
                            last = Some((i, op));
                            break;
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
        last
    }

    fn lower_to_bv<'ctx>(
        ctx: &'ctx Context,
        mut expr: &str,
        program: &Program,
        symbols: &mut FxHashMap<String, BV<'ctx>>,
    ) -> Option<BV<'ctx>> {
        expr = expr.trim();
        while expr.starts_with('(') && expr.ends_with(')') {
            let mut depth = 0;
            let mut balanced_at_end = false;
            for (i, c) in expr.char_indices() {
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                    if depth == 0 && i == expr.len() - 1 {
                        balanced_at_end = true;
                    } else if depth == 0 {
                        break;
                    }
                }
            }
            if balanced_at_end {
                expr = expr[1..expr.len() - 1].trim();
            } else {
                break;
            }
        }

        if let Some((q, col)) = find_top_level_ternary(expr) {
            let cond_str = expr[..q].trim();
            let then_str = expr[q + 1..col].trim();
            let else_str = expr[col + 1..].trim();
            let then_bv = lower_to_bv(ctx, then_str, program, symbols)?;
            let else_bv = lower_to_bv(ctx, else_str, program, symbols)?;
            let cond_bool = Bool::new_const(ctx, cond_str);
            return Some(cond_bool.ite(&then_bv, &else_bv));
        }

        for ops in &[
            &["|"][..],
            &["^"],
            &["&"],
            &["<<", ">>"],
            &["+", "-"],
            &["*"],
        ] {
            if let Some((pos, op)) = find_last_top_level_op(expr, ops) {
                if (op == "+" || op == "-") && pos == 0 {
                    continue;
                }
                let left_str = expr[..pos].trim();
                let right_str = expr[pos + op.len()..].trim();
                if left_str.is_empty() || right_str.is_empty() {
                    continue;
                }
                let left_bv = lower_to_bv(ctx, left_str, program, symbols)?;
                let right_bv = lower_to_bv(ctx, right_str, program, symbols)?;
                return match op {
                    "|" => Some(left_bv.bvor(&right_bv)),
                    "^" => Some(left_bv.bvxor(&right_bv)),
                    "&" => Some(left_bv.bvand(&right_bv)),
                    "<<" => Some(left_bv.bvshl(&right_bv)),
                    ">>" => Some(left_bv.bvlshr(&right_bv)),
                    "+" => Some(left_bv.bvadd(&right_bv)),
                    "-" => Some(left_bv.bvsub(&right_bv)),
                    "*" => Some(left_bv.bvmul(&right_bv)),
                    _ => None,
                };
            }
        }

        if let Some(v) = parse_int_literal(expr) {
            return Some(BV::from_u64(ctx, v as u64, 32));
        }

        if let Some(&v) = program.enum_constants.get(expr) {
            return Some(BV::from_u64(ctx, v, 32));
        }
        let short = expr.rsplit("::").next().unwrap_or(expr);
        if let Some(&v) = program.enum_constants.get(short) {
            return Some(BV::from_u64(ctx, v, 32));
        }

        if let Some(def_val) = program.defines.get(expr).or_else(|| program.defines.get(short)) {
            let def_clean = normalize_opcode_expr(def_val);
            if let Some(v) = parse_int_literal(&def_clean) {
                return Some(BV::from_u64(ctx, v as u64, 32));
            }
            if let Some(&v) = program.enum_constants.get(&def_clean).or_else(|| {
                program
                    .enum_constants
                    .get(def_clean.rsplit("::").next().unwrap_or(&def_clean))
            }) {
                return Some(BV::from_u64(ctx, v, 32));
            }
        }

        if !short.is_empty() && short.chars().all(|c| c.is_alphanumeric() || c == '_') {
            let bv = symbols
                .entry(short.to_string())
                .or_insert_with(|| BV::new_const(ctx, short, 32))
                .clone();
            return Some(bv);
        }


        None
    }

    pub fn solve_opcode_equality_smt(expr1: &str, expr2: &str, program: &Program) -> bool {
        let mut cfg = Config::new();
        cfg.set_param_value("timeout", "50");
        let ctx = Context::new(&cfg);
        let solver = Solver::new(&ctx);

        let mut symbols: FxHashMap<String, BV> = FxHashMap::default();
        let bv1 = match lower_to_bv(&ctx, expr1, program, &mut symbols) {
            Some(b) => b,
            None => return false,
        };
        let bv2 = match lower_to_bv(&ctx, expr2, program, &mut symbols) {
            Some(b) => b,
            None => return false,
        };

        let symbol_vec: Vec<_> = symbols.values().collect();
        for i in 0..symbol_vec.len() {
            for j in (i + 1)..symbol_vec.len() {
                solver.assert(&symbol_vec[i]._eq(symbol_vec[j]).not());
            }
        }

        solver.assert(&bv1._eq(&bv2));

        matches!(solver.check(), SatResult::Sat)
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use trace_ir::{Function, Linkage, Span, TypeId};

    fn add_external_method(program: &mut Program, file: trace_ir::FileId, name: &str) -> FnId {
        let id = program.symbols.alloc_fn_id();
        program.symbols.push_synthetic_function(Function {
            id,
            name: name.to_string(),
            linkage: Linkage::External,
            return_type: TypeId(0),
            params: Vec::new(),
            locals: Vec::new(),
            span: Span::new(file, 1, 1),
            end_line: 1,
            file,
            is_defined: false,
            param_type_ids: Vec::new(),
            is_virtual: true,
            is_final: false,
            is_cpp: true,
        });
        id
    }

    #[test]
    fn interface_fallback_retains_all_matching_overloads() {
        let mut program = Program::new(PathBuf::from("/fixture"));
        let file = program
            .symbols
            .add_file(PathBuf::from("/fixture/interface.cpp"));
        let first = add_external_method(&mut program, file, "svc::IFoo::Run");
        let second = add_external_method(&mut program, file, "svc::IFoo::Run");
        add_external_method(&mut program, file, "svc::Foo::Run");
        add_external_method(&mut program, file, "other::IFoo::Run");
        program.add_inheritance("svc::FooStub", "svc::IFoo");

        let handlers = find_interface_methods(&program, "svc::FooStub", "Run");

        assert_eq!(handlers, vec![first, second]);
    }

    #[test]
    fn remote_stub_template_uses_lexical_interface_candidates() {
        assert_eq!(
            remote_stub_interface_candidates("OHOS::IRemoteStub<IFoo>", "outer::svc"),
            vec!["outer::svc::IFoo", "outer::IFoo", "IFoo"]
        );
        assert_eq!(
            remote_stub_interface_candidates("OHOS::IRemoteStub<api::IFoo, Policy>", "outer::svc"),
            vec!["outer::svc::api::IFoo", "outer::api::IFoo", "api::IFoo"]
        );
        assert_eq!(
            remote_stub_interface_candidates(
                "OHOS::IRemoteStub<::api::IFoo, Policy>",
                "outer::svc"
            ),
            vec!["api::IFoo"]
        );
        assert_eq!(
            remote_stub_interface_candidates("svc::Wrapper<IFoo>", "svc"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn send_request_match_uses_exact_member_name() {
        assert_eq!(final_callable_segment("remote->SendRequest"), "SendRequest");
        assert_eq!(final_callable_segment("Remote::SendRequest"), "SendRequest");
        assert_eq!(final_callable_segment("remote.SendRequest"), "SendRequest");
        assert_ne!(
            final_callable_segment("remote->SendRequestAsync"),
            "SendRequest"
        );
    }
}
