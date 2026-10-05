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
//! - A proxy/stub pair named by an `.idl` interface (`Program::idl_interfaces`)
//!   is registered without a `SendRequest` call or handler bodies.

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
    if proxies.is_empty() {
        return Vec::new();
    }

    let mut stub_index: FxHashMap<&str, &Vec<FnId>> = FxHashMap::default();
    for (class, handlers) in &stubs {
        stub_index.insert(class.as_str(), handlers);
    }
    // By stub class, which is what a sender pairs with: the proxy the
    // interface names, and a `*Client` written by hand beside it. Indexing
    // records at most one fact per pair (the owner of its headers). Of
    // several, however a program came to hold them, the first names it.
    let mut descriptors: FxHashMap<&str, &str> = FxHashMap::default();
    for idl in &program.idl_interfaces {
        descriptors
            .entry(idl.stub.as_str())
            .or_insert(idl.descriptor.as_str());
    }

    // Indexed once a proxy pairs with a stub; ancestries per class, not per
    // method, since a proxy's methods share its ancestry.
    let mut hierarchy = None;
    let mut stub_interfaces: FxHashMap<&str, Vec<String>> = FxHashMap::default();
    let mut stub_classes: FxHashMap<&str, FxHashSet<String>> = FxHashMap::default();
    let mut compatible: FxHashMap<&str, bool> = FxHashMap::default();
    let mut bridges: Vec<IpcBridge> = Vec::new();
    for (proxy_class, method, proxy_method) in &proxies {
        let stub_class = derive_stub_class(proxy_class);
        let Some((&stub_class, handlers)) = stub_index.get_key_value(stub_class.as_str()) else {
            continue;
        };
        let hierarchy = hierarchy.get_or_insert_with(|| Hierarchy::new(program));
        let stub = stub_interfaces
            .entry(stub_class)
            .or_insert_with(|| hierarchy.interfaces(stub_class, "IRemoteStub"));
        let compatible = *compatible.entry(proxy_class).or_insert_with(|| {
            let proxy = hierarchy.interfaces(proxy_class, "IRemoteProxy");
            !interfaces_differ(program, &proxy, stub)
        });
        if !compatible {
            continue;
        }
        let descriptor = descriptors.get(stub_class).copied().unwrap_or_default();
        let matched_handlers = find_handlers(program, handlers, method);
        if !matched_handlers.is_empty() {
            bridges.extend(matched_handlers.into_iter().map(|stub_handler| IpcBridge {
                proxy_method: *proxy_method,
                stub_handler,
                descriptor: descriptor.to_string(),
            }));
            continue;
        }
        // Fallback: stub has no handler methods (only dispatcher + boilerplate).
        // The stub's OnRemoteRequest switch calls interface methods directly on
        // `this` (inherited from the parent interface). Match proxy methods
        // against external (interface) functions with the same simple name.
        let fallback = stub_classes
            .entry(stub_class)
            .or_insert_with(|| hierarchy.fallback_classes(stub_class));
        bridges.extend(
            find_interface_methods(program, stub_class, fallback, method)
                .into_iter()
                .map(|stub_handler| IpcBridge {
                    proxy_method: *proxy_method,
                    stub_handler,
                    descriptor: descriptor.to_string(),
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
            // `methods_by_class` is keyed by class, so each is visited once.
            if !handlers.is_empty() || has_dispatcher {
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

    if program.idl_interfaces.is_empty() {
        return (stubs, proxies);
    }
    // IDL-generated pairs (docs/ANALYSIS.md, "IDL-generated interfaces"): the
    // synthesized proxy and stub are declarations only, so neither shows a
    // `SendRequest` call or a handler body. The IDL itself declares them a
    // pair; its stub has no handlers, so pairing takes the interface fallback.
    let mut known_stubs: FxHashSet<String> = stubs.iter().map(|(c, _)| c.clone()).collect();
    let mut known_senders: FxHashSet<FnId> = proxies.iter().map(|(_, _, id)| *id).collect();
    for idl in &program.idl_interfaces {
        if known_stubs.insert(idl.stub.clone()) {
            stubs.push((idl.stub.clone(), Vec::new()));
        }
        for method in &idl.methods {
            let name = format!("{}::{}", idl.proxy, method.name);
            for id in program.symbols.resolve_function_candidates(&name, None) {
                if known_senders.insert(id) {
                    proxies.push((idl.proxy.clone(), method.name.clone(), id));
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
fn find_interface_methods(
    program: &Program,
    stub_class: &str,
    ancestors: &FxHashSet<String>,
    method_name: &str,
) -> Vec<FnId> {
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
    // happens to expose the same method.
    program
        .symbols
        .functions
        .iter()
        .filter(|f| {
            f.name
                .rsplit_once("::")
                .is_some_and(|(class, method)| method == method_name && ancestors.contains(class))
        })
        .map(|f| f.id)
        .collect()
}

/// Whether a proxy and the stub its name pairs with serve two interfaces:
/// both name declared classes in their `IRemoteProxy<I>` / `IRemoteStub<I>`
/// bases, and no proxy interface is a stub interface or derives from or to
/// one (docs/ANALYSIS.md, "OpenHarmony IPC bridges"). Dropping a bridge
/// takes more evidence than following one: unknown on either side, the pair
/// stays name-based.
fn interfaces_differ(program: &Program, proxy: &[String], stub: &[String]) -> bool {
    let related =
        |p: &String, s: &String| p == s || program.derives_from(p, s) || program.derives_from(s, p);
    !proxy.is_empty()
        && !stub.is_empty()
        && !proxy.iter().any(|p| stub.iter().any(|s| related(p, s)))
}

/// The class facts IPC pairing looks up, indexed once per detection.
struct Hierarchy<'p> {
    program: &'p Program,
    /// Built on the fallback's first use: most stubs declare handlers.
    method_owners: std::cell::OnceCell<FxHashSet<&'p str>>,
    /// Declared classes by their own or an alias's final name segment,
    /// built on first use.
    namesakes: std::cell::OnceCell<FxHashMap<&'p str, Vec<String>>>,
}

impl<'p> Hierarchy<'p> {
    fn new(program: &'p Program) -> Self {
        Self {
            program,
            method_owners: std::cell::OnceCell::new(),
            namesakes: std::cell::OnceCell::new(),
        }
    }

    /// The interfaces the `wrapper<I>` arguments (`IRemoteStub`,
    /// `IRemoteProxy`) on `class` or an ancestor may name: every lexical
    /// candidate that is a declared class or an alias of one, read as
    /// lowering reads a template argument (`TypeTable::declared_class`).
    /// Every reading, not only the nearest: the merged index cannot tell
    /// which of two declared namesakes a unit sees, and a bridge is dropped
    /// only when no reading matches. A bare spelling may also name a class or
    /// an alias a using-directive imports, which the index does not record,
    /// so every declared class of that final name, and every class an alias
    /// of that final name stands for, is a reading too. `::IFoo` is not bare.
    fn interfaces(&self, class: &str, wrapper: &str) -> Vec<String> {
        let mut interfaces = Vec::new();
        self.walk_remote_arguments(class, wrapper, |candidates, bare| {
            interfaces.extend(
                candidates
                    .iter()
                    .filter_map(|c| self.program.types.declared_class(c)),
            );
            // The written spelling is the last, unscoped candidate.
            if let Some(written) = candidates.last().filter(|_| bare) {
                let written = written.split('<').next().unwrap_or(written).trim();
                interfaces.extend(
                    self.namesakes()
                        .get(written)
                        .into_iter()
                        .flatten()
                        .map(|name| name.to_string()),
                );
            }
        });
        interfaces.sort();
        interfaces.dedup();
        interfaces
    }

    fn namesakes(&self) -> &FxHashMap<&'p str, Vec<String>> {
        self.namesakes.get_or_init(|| {
            let types = &self.program.types;
            let mut by_final: FxHashMap<&str, Vec<String>> = FxHashMap::default();
            let aliases = types
                .all_aliases()
                .keys()
                .filter_map(|alias| Some((alias.as_str(), types.declared_class(alias)?)));
            let classes = types
                .declared_class_names()
                .map(|name| (name, name.to_owned()));
            for (spelled, class) in classes.chain(aliases) {
                let last = spelled.rsplit("::").next().unwrap_or(spelled);
                by_final.entry(last).or_default().push(class);
            }
            by_final
        })
    }

    /// The classes the interface fallback searches for `stub_class`'s
    /// methods: its transitive bases, plus the interface of every
    /// `IRemoteStub<I>` base on it or on any class reached so far (a
    /// recovered interface is walked like a base), and their bases. The
    /// interface is the argument's nearest candidate the IR represents, read
    /// as a declared class (or an alias of one) when it is one, otherwise by
    /// a method or an edge; else its first candidate; `IRemoteStub<IFoo>` is
    /// an ordinary `IRemoteStub` inheritance edge plus a preserved
    /// template-base spelling, from which `IFoo` is recovered.
    fn fallback_classes(&self, stub_class: &str) -> FxHashSet<String> {
        let mut classes: FxHashSet<String> = FxHashSet::default();
        let mut pending = vec![stub_class.to_string()];
        let mut expanded = FxHashSet::default();
        while let Some(class) = pending.pop() {
            if !expanded.insert(class.clone()) {
                continue;
            }
            let mut reached = self.program.bases_of(&class);
            for fact in self
                .program
                .template_bases_of(&class)
                .into_iter()
                .filter(|f| !f.is_dependent)
            {
                let candidates = remote_interface_candidates(
                    &fact.spelling,
                    &fact.declaration_scope,
                    "IRemoteStub",
                );
                let followed = candidates
                    .iter()
                    .find_map(|c| {
                        self.program
                            .types
                            .declared_class(c)
                            .or_else(|| self.class_exists(c).then(|| c.clone()))
                    })
                    .or_else(|| candidates.first().cloned());
                reached.extend(followed);
            }
            for next in reached {
                if classes.insert(next.clone()) {
                    pending.push(next);
                }
            }
        }
        classes
    }

    /// Call `read` with the lexical candidates of every `wrapper<I>`
    /// argument written on `class` or an ancestor. A dependent base's
    /// argument is a template parameter, not an interface.
    /// `bare` is whether the argument was written without any qualifier.
    fn walk_remote_arguments(
        &self,
        class: &str,
        wrapper: &str,
        mut read: impl FnMut(&[String], bool),
    ) {
        let mut pending = vec![class.to_string()];
        let mut expanded = FxHashSet::default();
        while let Some(class) = pending.pop() {
            if !expanded.insert(class.clone()) {
                continue;
            }
            for fact in self
                .program
                .template_bases_of(&class)
                .into_iter()
                .filter(|f| !f.is_dependent)
            {
                let bare = remote_interface_argument(&fact.spelling, wrapper)
                    .is_some_and(|argument| !argument.contains("::"));
                read(
                    &remote_interface_candidates(&fact.spelling, &fact.declaration_scope, wrapper),
                    bare,
                );
            }
            pending.extend(self.program.bases_of(&class));
        }
    }

    /// Whether a lexical interface candidate is represented in the merged IR.
    /// Types are the primary signal; the other facts keep recovery working
    /// for incomplete/error-tolerant parses where a method or inheritance
    /// edge survived but the class tag did not.
    fn class_exists(&self, class: &str) -> bool {
        self.program
            .types
            .type_id_by_tag(class, TypeKind::Struct)
            .is_some()
            || self
                .method_owners
                .get_or_init(|| {
                    self.program
                        .symbols
                        .functions
                        .iter()
                        .filter_map(|function| {
                            function.name.rsplit_once("::").map(|(owner, _)| owner)
                        })
                        .collect()
                })
                .contains(class)
            || self.program.has_inheritance_edges(class)
    }
}

/// Recover the interface argument from an exact `wrapper<Interface>` base
/// spelling (`IRemoteStub`, `IRemoteProxy`), ordered by C++ lexical lookup
/// preference. Relative names, including `api::IFoo`, search from the derived
/// class's declaration scope outward; only a leading `::` forces global
/// lookup. The wrapper's own qualification does not affect the argument.
/// Nested templates in the first argument are preserved; later template
/// arguments are ignored.
/// The first template argument of `template_base` when it spells `wrapper`
/// (`IRemoteStub<api::IFoo, Policy>` gives `api::IFoo`), as written.
fn remote_interface_argument<'a>(template_base: &'a str, wrapper: &str) -> Option<&'a str> {
    let open = template_base.find('<')?;
    if template_base[..open].trim().rsplit("::").next() != Some(wrapper) {
        return None;
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
    let interface = template_base[open + 1..end?].trim();
    (!interface.is_empty()).then_some(interface)
}

fn remote_interface_candidates(
    template_base: &str,
    declaration_scope: &str,
    wrapper: &str,
) -> Vec<String> {
    let Some(interface) = remote_interface_argument(template_base, wrapper) else {
        return Vec::new();
    };
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
        program
            .symbols
            .push_synthetic_function(external_method(id, file, name))
    }

    /// As a header's declaration is: found by name.
    fn add_declared_method(program: &mut Program, file: trace_ir::FileId, name: &str) -> FnId {
        let id = program.symbols.alloc_fn_id();
        program
            .symbols
            .add_function(external_method(id, file, name))
    }

    fn external_method(id: FnId, file: trace_ir::FileId, name: &str) -> Function {
        Function {
            is_weak: false,
            target: None,
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
            explicit_arity: None,
            default_args: 0,
            reference_params: Vec::new(),
            owner_unresolved: false,
            variadic: false,
            defaulted_in_class: false,
            declared_in_class: false,
            is_static_member: false,
            is_virtual: true,
            is_final: false,
            is_cpp: true,
            c_linkage: false,
            tu: None,
        }
    }

    /// `detect_ipc_pairs` takes any `Program`: two facts naming one proxy
    /// class pair it once, under the first fact's descriptor.
    #[test]
    fn facts_naming_one_proxy_keep_the_first_descriptor() {
        let mut program = Program::new(PathBuf::from("/fixture"));
        let file = program.symbols.add_file(PathBuf::from("/fixture/atm.h"));
        let proxy = add_declared_method(&mut program, file, "p::AtmProxy::Run");
        let interface = add_declared_method(&mut program, file, "p::IAtm::Run");
        program.add_inheritance("p::AtmStub", "p::IAtm");
        for descriptor in ["p.IAtm", "p.Atm"] {
            program.idl_interfaces.push(trace_ir::IdlInterface {
                descriptor: descriptor.into(),
                interface: "p::IAtm".into(),
                proxy: "p::AtmProxy".into(),
                stub: "p::AtmStub".into(),
                methods: vec![trace_ir::IdlMethod {
                    name: "Run".into(),
                    ipccode: None,
                }],
            });
        }

        let bridges = detect_ipc_pairs(&program);

        let found: Vec<(FnId, FnId, &str)> = bridges
            .iter()
            .map(|b| (b.proxy_method, b.stub_handler, b.descriptor.as_str()))
            .collect();
        assert_eq!(found, vec![(proxy, interface, "p.IAtm")]);
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

        let ancestors = Hierarchy::new(&program).fallback_classes("svc::FooStub");
        let handlers = find_interface_methods(&program, "svc::FooStub", &ancestors, "Run");

        assert_eq!(handlers, vec![first, second]);
    }

    #[test]
    fn remote_template_uses_lexical_interface_candidates() {
        assert_eq!(
            remote_interface_candidates("OHOS::IRemoteStub<IFoo>", "outer::svc", "IRemoteStub"),
            vec!["outer::svc::IFoo", "outer::IFoo", "IFoo"]
        );
        assert_eq!(
            remote_interface_candidates(
                "OHOS::IRemoteStub<api::IFoo, Policy>",
                "outer::svc",
                "IRemoteStub"
            ),
            vec!["outer::svc::api::IFoo", "outer::api::IFoo", "api::IFoo"]
        );
        assert_eq!(
            remote_interface_candidates(
                "OHOS::IRemoteStub<::api::IFoo, Policy>",
                "outer::svc",
                "IRemoteStub"
            ),
            vec!["api::IFoo"]
        );
        assert_eq!(
            remote_interface_candidates("IRemoteProxy<IFoo>", "svc", "IRemoteProxy"),
            vec!["svc::IFoo", "IFoo"]
        );
        assert_eq!(
            remote_interface_candidates("IRemoteProxy<IFoo>", "svc", "IRemoteStub"),
            Vec::<String>::new()
        );
        assert_eq!(
            remote_interface_candidates("svc::Wrapper<IFoo>", "svc", "IRemoteStub"),
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
