//! Graph-only reconstruction from independently indexed declarations.
use rustc_hash::FxHashMap;
use trace_ir::*;

pub struct LinkedMember {
    pub site: CallSiteId,
    pub caller: FnId,
    pub callee: FnId,
}

// Keep source-specific guesses distinct from a namesake another provider
// actually declares. Stored facts retain the original spelling; private tags
// preserve that parameter's uncertainty for the shared matcher.
fn parameter_shape(t: &TypeDesc, guesses: &[String], types: &mut TypeTable) -> TypeDesc {
    match t {
        TypeDesc::Struct { name, fields } | TypeDesc::Union { name, fields }
            if guesses.contains(name) =>
        {
            let name = format!("@trace-linking-guess:{name}");
            types.note_guessed_class(&name);
            if matches!(t, TypeDesc::Struct { .. }) {
                TypeDesc::Struct {
                    name,
                    fields: fields.clone(),
                }
            } else {
                TypeDesc::Union {
                    name,
                    fields: fields.clone(),
                }
            }
        }
        TypeDesc::Ptr(t) => TypeDesc::Ptr(Box::new(parameter_shape(t, guesses, types))),
        TypeDesc::Array { elem, size } => TypeDesc::Array {
            elem: Box::new(parameter_shape(elem, guesses, types)),
            size: *size,
        },
        TypeDesc::FnPtr { ret, params } => TypeDesc::FnPtr {
            ret: Box::new(parameter_shape(ret, guesses, types)),
            params: params
                .iter()
                .map(|t| parameter_shape(t, guesses, types))
                .collect(),
        },
        t => t.clone(),
    }
}

/// No PAG or solver: the same class/method and IPC rules over a compact
/// declaration view. Callers apply their symbol/image ownership rules to the
/// returned ordinary member candidates; IPC bridges deliberately cross images.
pub fn link_graph_facts(facts: &LinkingFacts) -> (Vec<LinkedMember>, Vec<IpcBridge>) {
    link_graph_facts_in_targets(facts, &FxHashMap::default())
}

/// Reconstruct each provider image before applying final dispatch boundaries.
/// Targetless providers remain distinct through their source-analysis views.
pub fn link_graph_facts_in_targets(
    facts: &LinkingFacts,
    targets: &FxHashMap<FnId, TargetId>,
) -> (Vec<LinkedMember>, Vec<IpcBridge>) {
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
            .map(|t| {
                let shape = parameter_shape(t, &f.guessed_params, &mut p.types);
                p.types.intern(shape)
            })
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
            is_member_template: false,
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
    struct Provider<'a> {
        methods: FxHashMap<FnId, bool>,
        final_classes: std::collections::BTreeSet<&'a str>,
        images: std::collections::BTreeSet<Option<TargetId>>,
    }
    let source_views = facts.source_views();
    let defined: FxHashMap<_, _> = facts.functions.iter().map(|f| (f.id, f.defined)).collect();
    let mut providers = Vec::new();
    let mut source_by_origin = FxHashMap::default();
    for view in source_views.iter() {
        let images: std::collections::BTreeSet<_> = view
            .methods
            .iter()
            .filter(|(id, _)| defined.get(id) == Some(&true))
            .map(|(id, _)| targets.get(id).copied())
            .collect();
        source_by_origin.insert(view.origin.as_str(), providers.len());
        providers.push(Provider {
            methods: view.methods.iter().copied().collect(),
            images,
            final_classes: view
                .classes
                .iter()
                .filter(|(_, flag)| *flag)
                .map(|(name, _)| name.as_str())
                .collect(),
        });
    }
    let mut members = Vec::new();
    let mut member_targets: FxHashMap<(String, String, Option<bool>), Vec<FnId>> =
        FxHashMap::default();
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
        let method = call.method_name();
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
        let source = source_by_origin
            .get(call.origin.as_str())
            .map(|&i| &providers[i]);
        let source_class_final =
            source.is_some_and(|view| view.final_classes.contains(call.receiver.as_str()));
        let selected_final = call
            .callee
            .is_some_and(|id| source.is_some_and(|view| view.methods.get(&id) == Some(&true)));
        // An existing body binding is authoritative. Weak overrides, when
        // eligible, have already been handled by the ordinary linker.
        if !call.implicit
            && selected.is_some_and(|f| {
                f.is_defined
                    && (!f.is_virtual || selected_final || call.exact || source_class_final)
            })
        {
            continue;
        }
        // A selected nonvirtual overload must not inherit the dispatch mode
        // of a virtual namesake, including through the shared target cache.
        // An undeclared receiver's selected symbol may be a generated external
        // placeholder: its false virtual flag is not a nonvirtual declaration.
        let virtual_dispatch = if call.exact || source_class_final {
            Some(false)
        } else if call.receiver_declared {
            selected.map(|f| f.is_virtual && !selected_final)
        } else {
            None
        };
        let mut seen = std::collections::BTreeSet::new();
        for root in roots {
            let targets = member_targets
                .entry((root.clone(), method.to_string(), virtual_dispatch))
                .or_insert_with(|| {
                    let own = p
                        .ancestor_closure(root)
                        .into_iter()
                        .find_map(|(class, _)| {
                            let own = p.symbols.functions_named(&kind.name_on(class));
                            (!own.is_empty()).then_some(own)
                        })
                        .unwrap_or_else(|| crate::ipc::remote_stub_methods(&p, root, method));
                    let virtual_call = virtual_dispatch
                        .unwrap_or_else(|| own.iter().any(|&id| p.symbols.function(id).is_virtual));
                    if virtual_call {
                        let mut found = std::collections::BTreeSet::new();
                        for provider in &providers {
                            for target in &provider.images {
                                let visible = |id| {
                                    let f = p.symbols.function(id);
                                    !f.is_defined
                                        || (provider.methods.contains_key(&id)
                                            && targets.get(&id).copied() == *target)
                                };
                                let final_method = |id| {
                                    provider.methods.get(&id) == Some(&true)
                                        && (!p.symbols.function(id).is_defined
                                            || targets.get(&id).copied() == *target)
                                };
                                found.extend(p.method_targets_with_finality(
                                    root,
                                    &kind,
                                    &|_| true,
                                    &visible,
                                    &|class| provider.final_classes.contains(class),
                                    &final_method,
                                ));
                            }
                        }
                        if found.is_empty() {
                            own
                        } else {
                            found.into_iter().collect()
                        }
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
                        && f.explicit_arity.is_some()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn function(
        id: u32,
        name: &str,
        params: Option<Vec<TypeDesc>>,
        virtual_member: bool,
    ) -> LinkFunction {
        LinkFunction {
            id: FnId(id),
            name: name.into(),
            params,
            guessed_params: vec![],
            defined: true,
            virtual_member,
            final_member: false,
            static_member: false,
            variadic: false,
            definition_origins: vec!["provider".into()],
        }
    }

    fn call(id: u32, receiver: &str, callee: Option<FnId>) -> LinkCall {
        LinkCall {
            id: CallSiteId(id),
            source_site: CallSiteId(id),
            caller: FnId(0),
            callee,
            name: format!("{receiver}::Run"),
            receiver: receiver.into(),
            receiver_declared: true,
            exact: false,
            origin: "caller".into(),
            implicit: callee.is_none(),
        }
    }

    fn targets(facts: &LinkingFacts) -> BTreeSet<(CallSiteId, FnId)> {
        link_graph_facts(facts)
            .0
            .into_iter()
            .map(|m| (m.site, m.callee))
            .collect()
    }

    #[test]
    fn selected_overload_controls_virtual_dispatch_in_both_cache_orders() {
        let mut facts = LinkingFacts {
            classes: vec![
                ("Base".into(), true, false),
                ("Derived".into(), true, false),
            ],
            bases: vec![("Derived".into(), "Base".into())],
            functions: vec![
                function(0, "Client::use", Some(vec![]), false),
                function(1, "Base::Run", Some(vec![TypeDesc::Int]), true),
                function(2, "Base::Run", Some(vec![TypeDesc::Double]), false),
                function(3, "Derived::Run", Some(vec![TypeDesc::Int]), true),
                function(4, "Derived::Run", Some(vec![TypeDesc::Double]), false),
            ],
            calls: vec![
                call(0, "Base", Some(FnId(2))),
                call(1, "Base", Some(FnId(1))),
            ],
            ..Default::default()
        };
        let expected = BTreeSet::from([(CallSiteId(1), FnId(1)), (CallSiteId(1), FnId(3))]);
        assert_eq!(targets(&facts), expected);
        facts.calls.reverse();
        assert_eq!(targets(&facts), expected);
        facts.functions[1].final_member = true;
        assert!(!targets(&facts).contains(&(CallSiteId(1), FnId(3))));

        // A selected base virtual remains virtual when a nearer class also
        // declares a nonvirtual overload of the same name.
        facts.functions[1].final_member = false;
        facts.functions[3].name = "Leaf::Run".into();
        facts.classes.push(("Leaf".into(), true, false));
        facts.bases.push(("Leaf".into(), "Derived".into()));
        facts
            .calls
            .iter_mut()
            .find(|c| c.id == CallSiteId(1))
            .unwrap()
            .receiver = "Derived".into();
        assert!(targets(&facts).contains(&(CallSiteId(1), FnId(3))));
    }

    #[test]
    fn undeclared_receiver_placeholder_does_not_establish_nonvirtual_dispatch() {
        let mut placeholder = function(1, "missing::Base::Run", Some(vec![TypeDesc::Int]), false);
        placeholder.defined = false;
        let mut call = call(0, "Base", Some(placeholder.id));
        call.receiver_declared = false;
        let facts = LinkingFacts {
            classes: vec![
                ("Base".into(), true, false),
                ("Derived".into(), true, false),
            ],
            bases: vec![("Derived".into(), "Base".into())],
            functions: vec![
                function(0, "Client::use", Some(vec![]), false),
                placeholder,
                function(2, "Base::Run", Some(vec![TypeDesc::Int]), true),
                function(3, "Derived::Run", Some(vec![TypeDesc::Int]), true),
            ],
            calls: vec![call],
            ..Default::default()
        };
        assert!(targets(&facts).contains(&(CallSiteId(0), FnId(3))));
    }

    #[test]
    fn unknown_candidate_arity_remains_a_possible_override() {
        let facts = LinkingFacts {
            classes: vec![
                ("Base".into(), true, false),
                ("Derived".into(), true, false),
            ],
            bases: vec![("Derived".into(), "Base".into())],
            functions: vec![
                function(0, "Client::use", Some(vec![]), false),
                function(1, "Base::Run", Some(vec![TypeDesc::Int]), true),
                function(2, "Derived::Run", None, true),
                function(3, "Derived::Run", Some(vec![]), true),
                function(4, "Derived::Run", Some(vec![TypeDesc::Double]), true),
            ],
            calls: vec![call(0, "Base", Some(FnId(1)))],
            ..Default::default()
        };
        assert_eq!(
            targets(&facts),
            BTreeSet::from([(CallSiteId(0), FnId(1)), (CallSiteId(0), FnId(2)),])
        );
    }

    #[test]
    fn remote_stub_interface_body_survives_missing_dispatch_hierarchy() {
        let facts = LinkingFacts {
            classes: vec![
                ("IWorker".into(), true, false),
                ("WorkerStub".into(), true, false),
            ],
            bases: vec![("WorkerStub".into(), "IRemoteStub".into())],
            template_bases: vec![TemplateBase {
                derived: "WorkerStub".into(),
                spelling: "IRemoteStub<IWorker>".into(),
                declaration_scope: String::new(),
                is_dependent: false,
                anonymous_in: None,
            }],
            functions: vec![
                function(0, "WorkerStub::dispatch", Some(vec![]), false),
                function(1, "IWorker::Run", Some(vec![TypeDesc::Int]), true),
            ],
            calls: vec![call(0, "WorkerStub", None)],
            ..Default::default()
        };
        assert_eq!(targets(&facts), BTreeSet::from([(CallSiteId(0), FnId(1))]));
    }

    #[test]
    fn legacy_receiver_expressions_recover_their_member_name() {
        let mut facts = LinkingFacts {
            classes: vec![("Remote".into(), true, false)],
            functions: vec![
                function(0, "Client::use", Some(vec![]), false),
                function(1, "Remote::Run", Some(vec![]), false),
            ],
            calls: vec![call(0, "Remote", None)],
            ..Default::default()
        };
        for spelling in ["p->Run", "obj.field.Run", "ns::p->Run", "Remote::Run"] {
            facts.calls[0].name = spelling.into();
            facts.calls[0].implicit = false;
            assert_eq!(
                targets(&facts),
                BTreeSet::from([(CallSiteId(0), FnId(1))]),
                "{spelling}"
            );
        }
        for operator in ["operator->", "operator->*", "operator()"] {
            let name = format!("Remote::{operator}");
            facts.calls[0].name = name.clone();
            facts.functions[1].name = name;
            assert_eq!(
                targets(&facts),
                BTreeSet::from([(CallSiteId(0), FnId(1))]),
                "{operator}"
            );
        }
    }
    #[test]
    fn source_parameter_guess_is_not_erased_by_another_providers_class() {
        let named = TypeDesc::Struct {
            name: "MySize".into(),
            fields: vec![],
        };
        let mut guessed = function(1, "Base::Run", Some(vec![named.clone()]), true);
        guessed.defined = false;
        guessed.guessed_params = vec!["MySize".into()];
        let mut known = function(3, "Base::Run", Some(vec![named]), true);
        known.defined = false;
        let facts = LinkingFacts {
            classes: vec![
                ("Base".into(), true, false),
                ("Derived".into(), true, false),
                ("MySize".into(), true, false),
            ],
            bases: vec![("Derived".into(), "Base".into())],
            functions: vec![
                function(0, "Client::use", Some(vec![]), false),
                guessed,
                function(2, "Derived::Run", Some(vec![TypeDesc::Long]), true),
                known,
            ],
            calls: vec![
                call(0, "Base", Some(FnId(1))),
                call(1, "Base", Some(FnId(3))),
            ],
            ..Default::default()
        };
        assert_eq!(targets(&facts), BTreeSet::from([(CallSiteId(0), FnId(2))]));
    }
}
