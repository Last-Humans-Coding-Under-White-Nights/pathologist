use super::*;
use trace_ir::{CallSiteId, FnId, LinkingFacts};

fn fn_id(id: i64) -> Result<FnId> {
    Ok(FnId(
        u32::try_from(id).context("function ID exceeds linking range")?,
    ))
}
fn site_id(id: i64) -> Result<CallSiteId> {
    Ok(CallSiteId(
        u32::try_from(id).context("call-site ID exceeds linking range")?,
    ))
}

/// An analysis root is a path, not a snapshot identity: independent CI jobs
/// commonly index different repositories at the same mounted path. Qualify
/// first-time origins once; merged inputs retain these tokens across stages.
pub(super) fn scope_origins(facts: &mut LinkingFacts, input: Identity) {
    use std::fmt::Write;
    if facts.views.is_empty() {
        facts.views = facts.source_views().into_owned();
    }
    let mut origins: FxHashMap<String, String> = FxHashMap::default();
    let mut qualify = |origin: &mut String| {
        if let Some(scoped) = origins.get(origin.as_str()) {
            origin.clone_from(scoped);
            return;
        }
        let identity = Identity::hash(
            "trace-merge-linking-origin-v1",
            &[input.bytes(), origin.as_bytes()],
        );
        let mut scoped = String::from("analysis:");
        for byte in identity.bytes() {
            write!(&mut scoped, "{byte:02x}").unwrap();
        }
        let original = std::mem::replace(origin, scoped.clone());
        origins.insert(original, scoped);
    };
    for view in &mut facts.views {
        qualify(&mut view.origin);
    }
    for function in &mut facts.functions {
        for origin in &mut function.definition_origins {
            qualify(origin);
        }
    }
    for call in &mut facts.calls {
        qualify(&mut call.origin);
    }
}

// Serialize each structural key once, rather than formatting it on every
// comparison. Equal records coalesce; conflicting alias targets remain in a
// deterministic order for the shared alias resolver's last-wins behavior.
fn canonicalize<T: serde::Serialize + PartialEq>(facts: &mut Vec<T>) -> Result<()> {
    let mut keyed = std::mem::take(facts)
        .into_iter()
        .map(|fact| Ok((serde_json::to_string(&fact)?, fact)))
        .collect::<Result<Vec<_>>>()?;
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.dedup_by(|a, b| a.1 == b.1);
    *facts = keyed.into_iter().map(|(_, fact)| fact).collect();
    Ok(())
}

/// Remap each source fact before combining it; retain the compact metadata
/// in the output so a subsequent merge can add another provider.
pub(super) fn combine(
    inputs: Vec<Option<LinkingFacts>>,
    functions: &[FxHashMap<i64, i64>],
    sites: &[FxHashMap<i64, i64>],
) -> Result<LinkingFacts> {
    let mut out = LinkingFacts {
        complete: true,
        ..Default::default()
    };
    let mut methods = BTreeMap::new();
    let mut calls = FxHashSet::default();
    for (db, input) in inputs.into_iter().enumerate() {
        let Some(mut input) = input else {
            out.complete = false;
            continue;
        };
        out.complete &= input.complete;
        if input.views.is_empty() {
            input.views = input.source_views().into_owned();
        }
        for mut view in input.views {
            view.methods = view
                .methods
                .into_iter()
                .filter_map(|(f, flag)| {
                    functions[db]
                        .get(&i64::from(f.0))
                        .copied()
                        .map(|id| Ok((fn_id(id)?, flag)))
                })
                .collect::<Result<Vec<_>>>()?;
            out.views.push(view);
        }
        out.classes.extend(input.classes);
        out.aliases.extend(input.aliases);
        out.bases.extend(input.bases);
        out.template_bases.extend(input.template_bases);
        out.interfaces.extend(input.interfaces);
        for mut f in input.functions {
            let Some(&id) = functions[db].get(&i64::from(f.id.0)) else {
                continue;
            };
            f.id = fn_id(id)?;
            match methods.entry(f.id) {
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let held: &mut trace_ir::LinkFunction = entry.get_mut();
                    held.guessed_params.extend(f.guessed_params);
                    held.virtual_member |= f.virtual_member;
                    held.final_member |= f.final_member;
                    held.defined |= f.defined;
                    for origin in f.definition_origins {
                        if !held.definition_origins.contains(&origin) {
                            held.definition_origins.push(origin);
                        }
                    }
                    if held.params.is_none() {
                        held.params = f.params;
                    }
                }
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(f);
                }
            }
        }
        for mut call in input.calls {
            let (Some(&id), Some(&caller)) = (
                sites[db].get(&i64::from(call.id.0)),
                functions[db].get(&i64::from(call.caller.0)),
            ) else {
                continue;
            };
            call.source_site =
                site_id(*sites[db].get(&i64::from(call.source_site.0)).unwrap_or(&id))?;
            call.id = site_id(id)?;
            // Overlapping inputs can retain the same inherited source site.
            // Its immutable linking fact belongs to the unified site once.
            if !calls.insert(call.id) {
                continue;
            }
            call.caller = fn_id(caller)?;
            call.callee = call
                .callee
                .and_then(|id| functions[db].get(&i64::from(id.0)))
                .copied()
                .map(fn_id)
                .transpose()?;
            out.calls.push(call);
        }
        for sender in input.senders {
            if let Some(&id) = functions[db].get(&i64::from(sender.0)) {
                out.senders.push(fn_id(id)?);
            }
        }
    }
    // IDs are unique after remapping/deduplication; no stable-sort buffer is needed.
    out.calls.sort_unstable_by_key(|call| call.id);
    out.functions = methods.into_values().collect();
    for function in &mut out.functions {
        function.guessed_params.sort();
        function.guessed_params.dedup();
        function.definition_origins.sort();
        function.definition_origins.dedup();
    }
    let mut views: BTreeMap<String, trace_ir::LinkView> = BTreeMap::new();
    for view in std::mem::take(&mut out.views) {
        let held = views
            .entry(view.origin.clone())
            .or_insert_with(|| trace_ir::LinkView {
                origin: view.origin.clone(),
                classes: vec![],
                methods: vec![],
            });
        held.classes.extend(view.classes);
        held.methods.extend(view.methods);
    }
    for mut view in views.into_values() {
        let mut classes = BTreeMap::new();
        for (name, flag) in view.classes {
            classes
                .entry(name)
                .and_modify(|held| *held &= flag)
                .or_insert(flag);
        }
        view.classes = classes.into_iter().collect();
        let mut methods = BTreeMap::new();
        for (id, flag) in view.methods {
            methods
                .entry(id)
                .and_modify(|held| *held &= flag)
                .or_insert(flag);
        }
        view.methods = methods.into_iter().collect();
        out.views.push(view);
    }
    canonicalize(&mut out.aliases)?;
    canonicalize(&mut out.template_bases)?;
    canonicalize(&mut out.interfaces)?;
    out.classes.sort();
    out.classes.dedup();
    out.bases.sort();
    out.bases.dedup();
    out.senders.sort();
    out.senders.dedup();
    Ok(out)
}

pub(super) fn reconstruct(
    facts: &LinkingFacts,
    functions: &[DbFunction],
    dependencies: &[(i64, i64)],
    definitions: &FxHashMap<String, Vec<DefRecord>>,
    edges: &mut Vec<DbCallEdge>,
) -> Result<Vec<i64>> {
    let targets = functions
        .iter()
        .filter_map(|f| {
            f.target_id.map(|target| {
                Ok((
                    fn_id(f.id)?,
                    trace_ir::TargetId(
                        u32::try_from(target).context("target ID exceeds linking range")?,
                    ),
                ))
            })
        })
        .collect::<Result<FxHashMap<_, _>>>();
    let targets = targets?;
    let (members, bridges) = trace_analysis::link_graph_facts_in_targets(facts, &targets);
    let functions: FxHashMap<_, _> = functions.iter().map(|f| (f.id, f)).collect();
    let mut emitted: FxHashSet<_> = edges
        .iter()
        .filter_map(|e| e.call_site_id.map(|s| (s, e.callee_fn_id)))
        .collect();
    let mut ipc: FxHashSet<_> = edges
        .iter()
        .filter(|e| e.resolution == "ipc")
        .map(|e| (e.caller_fn_id, e.callee_fn_id))
        .collect();
    let mut dependencies_by_target: FxHashMap<i64, Vec<i64>> = FxHashMap::default();
    for &(from, to) in dependencies {
        dependencies_by_target.entry(from).or_default().push(to);
    }
    let mut closures: FxHashMap<i64, FxHashSet<i64>> = FxHashMap::default();
    let mut choices: FxHashMap<(Option<i64>, &str, &str), FxHashSet<i64>> = FxHashMap::default();
    for member in members {
        let (caller, callee, site) = (
            i64::from(member.caller.0),
            i64::from(member.callee.0),
            i64::from(member.site.0),
        );
        let (Some(a), Some(b)) = (functions.get(&caller), functions.get(&callee)) else {
            continue;
        };
        if let Some(target) = a.target_id {
            let reachable = closures.entry(target).or_insert_with(|| {
                let mut reachable = FxHashSet::default();
                reachable.insert(target);
                let mut pending = vec![target];
                while let Some(from) = pending.pop() {
                    for &to in dependencies_by_target.get(&from).into_iter().flatten() {
                        if reachable.insert(to) {
                            pending.push(to);
                        }
                    }
                }
                reachable
            });
            if b.target_id.is_some_and(|t| !reachable.contains(&t)) {
                continue;
            }
        }
        let chosen = choices
            .entry((a.target_id, b.name.as_str(), b.signature.as_str()))
            .or_insert_with(|| {
                let Some(defs) = definitions.get(&b.name) else {
                    return FxHashSet::default();
                };
                let (pool, fallback) = matching_definitions(defs, &b.signature);
                // Apply the ordinary linker's precedence to its full pool.
                // Endpoint visibility must not resurrect a weak definition
                // that ordinary linking already replaced with a strong one.
                let pool = prefer_definitions(pool, a.target_id, fallback);
                pool.into_iter().map(|d| d.new_fn_id).collect()
            });
        if !chosen.contains(&callee) {
            continue;
        }
        if !emitted.insert((site, callee)) {
            continue;
        }
        edges.push(DbCallEdge {
            id: (edges.len() + 1) as i64,
            call_site_id: Some(site),
            caller_fn_id: caller,
            callee_fn_id: callee,
            resolution: "direct".into(),
        });
    }
    let mut handlers = Vec::new();
    for bridge in bridges {
        let (caller, callee) = (
            i64::from(bridge.proxy_method.0),
            i64::from(bridge.stub_handler.0),
        );
        if !ipc.insert((caller, callee)) {
            continue;
        }
        edges.push(DbCallEdge {
            id: (edges.len() + 1) as i64,
            call_site_id: None,
            caller_fn_id: caller,
            callee_fn_id: callee,
            resolution: "ipc".into(),
        });
        handlers.push(callee);
    }
    handlers.sort_unstable();
    handlers.dedup();
    Ok(handlers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use trace_ir::{IdlInterface, IdlMethod, TemplateBase, TypeDesc};

    fn function(id: u32, name: &str) -> trace_ir::LinkFunction {
        trace_ir::LinkFunction {
            id: FnId(id),
            name: name.into(),
            params: Some(vec![]),
            guessed_params: vec![],
            defined: true,
            virtual_member: true,
            final_member: false,
            static_member: false,
            variadic: false,
            definition_origins: vec!["provider".into()],
        }
    }

    fn call(id: u32) -> trace_ir::LinkCall {
        trace_ir::LinkCall {
            id: CallSiteId(id),
            source_site: CallSiteId(id),
            caller: FnId(1),
            callee: Some(FnId(2)),
            name: "Base::Run".into(),
            receiver: "Base".into(),
            receiver_declared: true,
            exact: false,
            origin: "caller".into(),
            implicit: false,
        }
    }

    #[test]
    fn member_visibility_preserves_unknown_targets_and_filters_known_images() {
        let facts = LinkingFacts {
            classes: vec![
                ("Base".into(), true, false),
                ("Derived".into(), true, false),
            ],
            bases: vec![("Derived".into(), "Base".into())],
            functions: vec![
                function(1, "Client::use"),
                function(2, "Base::Run"),
                function(3, "Derived::Run"),
            ],
            calls: vec![call(1), call(2)],
            ..Default::default()
        };
        for (target, admitted) in [
            (None, true),
            (Some(1), true),
            (Some(2), true),
            (Some(3), false),
        ] {
            let functions: Vec<_> = facts
                .functions
                .iter()
                .map(|f| DbFunction {
                    id: i64::from(f.id.0),
                    name: f.name.clone(),
                    file_id: 1,
                    line_start: 1,
                    line_end: 1,
                    linkage: "external".into(),
                    signature: "Run()".into(),
                    is_defined: 1,
                    is_dep: 0,
                    is_weak: 0,
                    target_id: if f.id == FnId(3) { target } else { Some(1) },
                })
                .collect();
            let mut edges = Vec::new();
            let mut defs: FxHashMap<String, Vec<DefRecord>> = FxHashMap::default();
            for f in &functions {
                defs.entry(f.name.clone()).or_default().push(DefRecord {
                    new_fn_id: f.id,
                    db_idx: 0,
                    signature: f.signature.clone(),
                    file_id: f.file_id,
                    file_path: String::new(),
                    line_start: f.line_start,
                    is_weak: f.is_weak != 0,
                    target_id: f.target_id,
                });
            }
            reconstruct(&facts, &functions, &[(1, 2), (2, 1)], &defs, &mut edges).unwrap();
            let recovered = edges.iter().filter(|e| e.callee_fn_id == 3).count();
            assert_eq!(recovered, if admitted { 2 } else { 0 }, "{target:?}");
        }
    }

    #[test]
    fn visibility_cannot_resurrect_a_weak_body_after_ordinary_selection() {
        let mut query = call(1);
        query.callee = Some(FnId(4));
        query.receiver = "Api".into();
        query.name = "Api::Run".into();
        let mut methods = vec![
            function(1, "Client::use"),
            function(2, "Api::Run"),
            function(3, "Api::Run"),
            function(4, "Api::Run"),
        ];
        for f in &mut methods {
            f.virtual_member = false;
        }
        methods[3].defined = false;
        let facts = LinkingFacts {
            classes: vec![("Api".into(), true, false)],
            functions: methods,
            calls: vec![query],
            ..Default::default()
        };
        let functions: Vec<_> = facts
            .functions
            .iter()
            .map(|f| DbFunction {
                id: i64::from(f.id.0),
                name: f.name.clone(),
                file_id: i64::from(f.id.0),
                line_start: 1,
                line_end: 1,
                linkage: "external".into(),
                signature: "Run()".into(),
                is_defined: i64::from(f.defined),
                is_dep: 0,
                is_weak: i64::from(f.id == FnId(2)),
                target_id: Some(if f.id == FnId(3) { 2 } else { 1 }),
            })
            .collect();
        let pool = functions
            .iter()
            .filter(|f| f.is_defined != 0 && f.name == "Api::Run")
            .map(|f| DefRecord {
                new_fn_id: f.id,
                db_idx: 0,
                signature: f.signature.clone(),
                file_id: f.file_id,
                file_path: String::new(),
                line_start: 1,
                is_weak: f.is_weak != 0,
                target_id: f.target_id,
            })
            .collect();
        let defs = FxHashMap::from_iter([("Api::Run".into(), pool)]);
        let mut edges = vec![DbCallEdge {
            id: 1,
            call_site_id: Some(1),
            caller_fn_id: 1,
            callee_fn_id: 3,
            resolution: "direct".into(),
        }];
        reconstruct(&facts, &functions, &[], &defs, &mut edges).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].callee_fn_id, 3);
    }

    #[test]
    fn calls_follow_remapped_site_order_across_input_permutations() {
        let input = |name: &str| {
            let mut call = call(0);
            call.name = format!("{name}::Run");
            call.caller = FnId(0);
            call.callee = None;
            LinkingFacts {
                calls: vec![call],
                ..Default::default()
            }
        };
        let a = input("A");
        let b = input("B");
        let functions = vec![FxHashMap::from_iter([(0, 1)]); 2];
        let sites = vec![
            FxHashMap::from_iter([(0, 9)]),
            FxHashMap::from_iter([(0, 2)]),
        ];
        let forward = combine(vec![Some(a.clone()), Some(b.clone())], &functions, &sites).unwrap();
        let reverse = combine(
            vec![Some(b), Some(a)],
            &functions,
            &[sites[1].clone(), sites[0].clone()],
        )
        .unwrap();
        assert_eq!(
            forward.calls.iter().map(|c| c.id).collect::<Vec<_>>(),
            [CallSiteId(2), CallSiteId(9)]
        );
        assert_eq!(
            serde_json::to_value(&forward.calls).unwrap(),
            serde_json::to_value(&reverse.calls).unwrap()
        );
    }

    #[test]
    fn structural_facts_are_canonical_across_reordering_and_overlap() {
        let input = |name: &str| LinkingFacts {
            complete: true,
            aliases: vec![(
                "Handle".into(),
                TypeDesc::Struct {
                    name: name.into(),
                    fields: vec![],
                },
            )],
            template_bases: vec![TemplateBase {
                derived: name.into(),
                spelling: "Wrapper<Handle>".into(),
                declaration_scope: String::new(),
                is_dependent: false,
                anonymous_in: None,
            }],
            interfaces: vec![IdlInterface {
                composed_stub: false,
                descriptor: format!("test.I{name}"),
                interface: format!("I{name}"),
                proxy: format!("{name}Proxy"),
                stub: format!("{name}Stub"),
                methods: vec![IdlMethod {
                    name: "Run".into(),
                    ipccode: Some(1),
                }],
            }],
            ..Default::default()
        };
        let a = input("A");
        let b = input("B");
        let combine_facts = |inputs: Vec<LinkingFacts>| {
            let maps = vec![FxHashMap::default(); inputs.len()];
            combine(inputs.into_iter().map(Some).collect(), &maps, &maps).unwrap()
        };
        let flat = combine_facts(vec![b.clone(), a.clone(), a.clone()]);
        let reverse = combine_facts(vec![a.clone(), b]);
        let overlap = combine_facts(vec![flat.clone(), a]);
        for facts in [reverse, overlap] {
            assert_eq!(flat.aliases, facts.aliases);
            assert_eq!(flat.template_bases, facts.template_bases);
            assert_eq!(flat.interfaces, facts.interfaces);
        }
        assert_eq!(
            flat.aliases.len(),
            2,
            "conflicting alias targets are retained"
        );
        assert_eq!(flat.template_bases.len(), 2);
        assert_eq!(flat.interfaces.len(), 2);
    }
}
