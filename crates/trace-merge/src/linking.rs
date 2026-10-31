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
        let Some(input) = input else {
            out.complete = false;
            continue;
        };
        out.complete &= input.complete;
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
    out.functions = methods.into_values().collect();
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
    edges: &mut Vec<DbCallEdge>,
) -> Vec<i64> {
    let (members, bridges) = trace_analysis::link_graph_facts(facts);
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
            let mut reachable = vec![target];
            let mut at = 0;
            while at < reachable.len() {
                for &(from, to) in dependencies {
                    if from == reachable[at] && !reachable.contains(&to) {
                        reachable.push(to);
                    }
                }
                at += 1;
            }
            if b.target_id.is_none_or(|t| !reachable.contains(&t)) {
                continue;
            }
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
    handlers
}
