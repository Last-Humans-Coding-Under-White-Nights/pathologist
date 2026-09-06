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
        || program
            .inheritance
            .iter()
            .any(|(derived, base)| derived == class || base == class)
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
