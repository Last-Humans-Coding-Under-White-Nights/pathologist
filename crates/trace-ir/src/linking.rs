//! Compact facts retained for deterministic graph linking without value analysis.
use crate::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinkingFacts {
    pub complete: bool,
    pub classes: Vec<(String, bool, bool)>,
    pub aliases: Vec<(String, TypeDesc)>,
    pub bases: Vec<(String, String)>,
    pub template_bases: Vec<TemplateBase>,
    pub interfaces: Vec<IdlInterface>,
    pub functions: Vec<LinkFunction>,
    pub calls: Vec<LinkCall>,
    pub senders: Vec<FnId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkFunction {
    pub id: FnId,
    pub name: String,
    pub params: Option<Vec<TypeDesc>>,
    pub defined: bool,
    pub virtual_member: bool,
    pub final_member: bool,
    pub static_member: bool,
    pub variadic: bool,
    pub definition_origins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkCall {
    pub id: CallSiteId,
    /// First site of this invocation, shared by its overload/CHA alternatives.
    pub source_site: CallSiteId,
    pub caller: FnId,
    pub callee: Option<FnId>,
    pub name: String,
    pub receiver: String,
    /// Declaration certainty belongs to the source analysis, not the union of
    /// databases: another repository may declare the class it really names.
    pub receiver_declared: bool,
    pub exact: bool,
    pub origin: String,
    pub implicit: bool,
}

fn shape(t: &TypeDesc) -> TypeDesc {
    match t {
        TypeDesc::Struct { name, .. } => TypeDesc::Struct {
            name: name.clone(),
            fields: vec![],
        },
        TypeDesc::Union { name, .. } => TypeDesc::Union {
            name: name.clone(),
            fields: vec![],
        },
        TypeDesc::Ptr(t) => TypeDesc::Ptr(Box::new(shape(t))),
        TypeDesc::Array { elem, size } => TypeDesc::Array {
            elem: Box::new(shape(elem)),
            size: *size,
        },
        TypeDesc::FnPtr { ret, params } => TypeDesc::FnPtr {
            ret: Box::new(shape(ret)),
            params: params.iter().map(shape).collect(),
        },
        t => t.clone(),
    }
}

impl LinkingFacts {
    pub fn collect(p: &Program, ipc_enabled: bool) -> Self {
        let mut classes: Vec<_> = p
            .types
            .declared_class_names()
            .filter(|c| !p.anonymous_classes.contains_key(*c))
            .map(|c| {
                (
                    c.to_owned(),
                    p.types.is_struct_defined(c),
                    p.class_is_final(c),
                )
            })
            .collect();
        classes.sort();
        let aliases = p
            .types
            .all_aliases()
            .iter()
            .filter(|(_, t)| t.is_class_like())
            .map(|(name, t)| (name.clone(), shape(t)))
            .collect();
        let functions = p
            .symbols
            .functions
            .iter()
            .filter(|f| f.linkage == Linkage::External && f.name.contains("::"))
            .map(|f| LinkFunction {
                id: f.id,
                name: f.name.clone(),
                defined: f.is_defined,
                virtual_member: f.is_virtual,
                final_member: f.is_final,
                static_member: f.is_static_member,
                variadic: f.variadic,
                definition_origins: if f.is_defined {
                    vec![p.root.to_string_lossy().into_owned()]
                } else {
                    vec![]
                },
                params: (f.explicit_arity.is_some() || !f.params.is_empty())
                    .then(|| p.symbols.explicit_params(f))
                    .flatten()
                    .and_then(|params| {
                        (0..params.len())
                            .map(|i| params.get(i).map(|t| shape(&p.types.get(t).desc)))
                            .collect()
                    }),
            })
            .collect();
        let mut source_sites = rustc_hash::FxHashMap::default();
        let calls = p
            .symbols
            .call_sites
            .iter()
            .filter(|c| !c.unevaluated)
            .filter_map(|c| {
                let mut implicit = false;
                let receiver = c.receiver_class.clone().or_else(|| {
                    if c.callee_var.is_some()
                        || !c
                            .callee_name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        || !c.callee_fn_id.is_some_and(|id| {
                            let f = p.symbols.function(id);
                            !f.is_defined && f.span.line == 0
                        })
                    {
                        return None;
                    }
                    let caller = p.symbols.function(c.caller);
                    let (owner, _) = caller.name.rsplit_once("::")?;
                    if !caller.is_cpp || !p.types.declares_class(owner) {
                        return None;
                    }
                    implicit = true;
                    Some(owner.to_string())
                })?;
                Some(LinkCall {
                    id: c.id,
                    source_site: *source_sites
                        .entry((c.caller, c.occurrence()))
                        .or_insert(c.id),
                    caller: c.caller,
                    callee: if implicit { None } else { c.callee_fn_id },
                    name: if implicit {
                        format!("{receiver}::{}", c.callee_name)
                    } else {
                        c.callee_name.to_string()
                    },
                    receiver_declared: p.types.declares_class(&receiver),
                    receiver,
                    exact: c.exact_receiver,
                    origin: p.root.to_string_lossy().into_owned(),
                    implicit,
                })
            })
            .collect();
        let mut senders: Vec<_> = p
            .symbols
            .call_sites
            .iter()
            .filter(|c| {
                ipc_enabled
                    && !c.unevaluated
                    && c.callee_name.rsplit([':', '>', '.']).next() == Some("SendRequest")
            })
            .map(|c| c.caller)
            .collect();
        senders.sort();
        senders.dedup();
        Self {
            complete: true,
            classes,
            aliases,
            bases: p.inheritance().to_vec(),
            template_bases: p.template_bases().to_vec(),
            interfaces: if ipc_enabled {
                p.idl_interfaces.clone()
            } else {
                vec![]
            },
            functions,
            calls,
            senders,
        }
    }
}
