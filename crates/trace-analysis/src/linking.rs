//! Graph-only reconstruction from independently indexed declarations.
use rustc_hash::FxHashMap;
use trace_ir::*;

pub struct LinkedMember {
    pub site: CallSiteId,
    pub caller: FnId,
    pub callee: FnId,
}

/// No PAG or solver: the same class/method and IPC rules over a compact
/// declaration view. Callers apply their symbol/image ownership rules to the
/// returned ordinary member candidates; IPC bridges deliberately cross images.
pub fn link_graph_facts(facts: &LinkingFacts) -> (Vec<LinkedMember>, Vec<IpcBridge>) {
    let mut p = Program::new("/linking".into());
    let file = p.symbols.add_file("/linking".into());
    for (class, defined, final_class) in &facts.classes {
        p.types.declare_struct(class);
        if *defined {
            p.types.define_struct(class);
        }
        if *final_class {
            p.mark_class_final(class);
        }
    }
    for (name, desc) in &facts.aliases {
        p.types.register_alias(name, desc.clone());
    }
    for (derived, base) in &facts.bases {
        p.add_inheritance(derived, base);
    }
    for base in &facts.template_bases {
        p.add_template_base_fact(base);
    }
    p.idl_interfaces.clone_from(&facts.interfaces);
    for f in &facts.functions {
        let params: Vec<_> = f
            .params
            .iter()
            .flatten()
            .map(|t| p.types.intern(t.clone()))
            .collect();
        p.symbols.restore_linking_function(Function {
            id: f.id,
            name: f.name.clone(),
            linkage: Linkage::External,
            return_type: p.types.unknown(),
            params: vec![],
            locals: vec![],
            span: Span::new(file, 1, 1),
            end_line: 1,
            file,
            is_defined: f.defined,
            is_weak: false,
            target: None,
            param_type_ids: params,
            explicit_arity: f.params.as_ref().map(|p| p.len() as u32),
            owner_unresolved: false,
            variadic: f.variadic,
            defaulted_in_class: false,
            declared_in_class: true,
            is_static_member: f.static_member,
            default_args: 0,
            reference_params: vec![],
            is_virtual: f.virtual_member,
            is_final: f.final_member,
            is_cpp: true,
            c_linkage: false,
            tu: None,
        });
    }
    for (i, &sender) in facts.senders.iter().enumerate() {
        if p.symbols.function_by_id(sender).is_none() {
            continue;
        }
        p.symbols.call_sites.push(CallSite {
            id: CallSiteId(i as u32),
            caller: sender,
            callee_name: "SendRequest".into(),
            callee_var: None,
            callee_fn_id: None,
            var_args: vec![],
            details: None,
            args_bound_past_this: false,
            span: Span::new(file, 1, 1),
            expansion_span: None,
            is_direct: false,
            in_loop: false,
            unevaluated: false,
            receiver_class: None,
            exact_receiver: false,
            return_dst: None,
            receiver: None,
            tu: None,
        });
    }
    let mut members = Vec::new();
    let mut member_targets: FxHashMap<(String, String, bool), Vec<FnId>> = FxHashMap::default();
    // A declaration imported by another input must not turn the source's
    // guessed receiver namespace into a certain one during name comparison.
    let unknown_receiver_types = TypeTable::default();
    let definitions: FxHashMap<_, _> = facts
        .functions
        .iter()
        .map(|f| (f.id, &f.definition_origins))
        .collect();
    let mut roots: FxHashMap<(&str, bool), Vec<String>> = FxHashMap::default();
    for call in &facts.calls {
        let Some((_, method)) = call.name.rsplit_once("::") else {
            continue;
        };
        if method == call.receiver.rsplit("::").next().unwrap_or("") || method.starts_with('~') {
            continue;
        }
        let roots = roots
            .entry((&call.receiver, call.receiver_declared))
            .or_insert_with(|| {
                if call.receiver_declared {
                    return vec![call.receiver.clone()];
                }
                let named = TypeDesc::Struct {
                    name: call.receiver.clone(),
                    fields: vec![],
                };
                facts
                    .classes
                    .iter()
                    .filter(|(name, _, _)| {
                        spelled_alike(
                            &unknown_receiver_types,
                            &named,
                            &TypeDesc::Struct {
                                name: name.clone(),
                                fields: vec![],
                            },
                            SpellingTolerance::Redeclaration,
                        )
                    })
                    .map(|(name, _, _)| name.clone())
                    .collect()
            });
        let kind = MethodKind::Named(method.to_string());
        let selected = call.callee.and_then(|id| p.symbols.function_by_id(id));
        let mut seen = std::collections::BTreeSet::new();
        for root in roots {
            let targets = member_targets
                .entry((root.clone(), method.to_string(), call.exact))
                .or_insert_with(|| {
                    let own = p
                        .ancestor_closure(root)
                        .into_iter()
                        .find_map(|(class, _)| {
                            let own = p.symbols.functions_named(&kind.name_on(class));
                            (!own.is_empty()).then_some(own)
                        })
                        .unwrap_or_else(|| crate::ipc::remote_stub_methods(&p, root, method));
                    let virtual_call =
                        !call.exact && own.iter().any(|&id| p.symbols.function(id).is_virtual);
                    if virtual_call {
                        p.method_targets(root, &kind)
                    } else {
                        own
                    }
                });
            for &id in targets.iter() {
                let f = p.symbols.function(id);
                if !f.is_defined || !seen.insert(id) {
                    continue;
                }
                if !call.implicit
                    && definitions
                        .get(&id)
                        .is_some_and(|origins| origins.contains(&call.origin))
                {
                    continue;
                }
                if selected.is_some_and(|selected| {
                    selected.params.is_empty()
                        && selected.explicit_arity.is_some()
                        && p.symbols.explicit_params_alike(
                            selected,
                            f,
                            &p.types,
                            SpellingTolerance::Override,
                        ) == Some(false)
                }) {
                    continue;
                }
                members.push(LinkedMember {
                    site: call.source_site,
                    caller: call.caller,
                    callee: id,
                });
            }
        }
    }
    (members, crate::detect_ipc_pairs(&p))
}
