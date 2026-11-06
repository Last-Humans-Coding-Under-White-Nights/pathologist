//! Compact facts retained for deterministic graph linking without value analysis.
use crate::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinkingFacts {
    pub complete: bool,
    #[serde(default)]
    pub views: Vec<LinkView>,
    pub classes: Vec<(String, bool, bool)>,
    pub aliases: Vec<(String, TypeDesc)>,
    pub bases: Vec<(String, String)>,
    pub template_bases: Vec<TemplateBase>,
    pub interfaces: Vec<IdlInterface>,
    pub functions: Vec<LinkFunction>,
    pub calls: Vec<LinkCall>,
    pub senders: Vec<FnId>,
}

/// Source-analysis view, retained across merges so final boundaries from one
/// provider do not constrain another provider with the same class names.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkView {
    pub origin: String,
    pub classes: Vec<(String, bool)>,
    pub methods: Vec<(FnId, bool)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkFunction {
    pub id: FnId,
    pub name: String,
    pub params: Option<Vec<TypeDesc>>,
    #[serde(default)]
    pub guessed_params: Vec<String>,
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

impl LinkCall {
    /// Member spelling shared by exported qualified names and legacy unresolved
    /// receiver expressions (`p->Run`, `obj.field.Run`, `ns::p->Run`).
    pub fn method_name(&self) -> &str {
        let tail = self.name.rsplit("::").next().unwrap();
        if tail.starts_with("operator") {
            return tail;
        }
        tail.rsplit("->")
            .next()
            .unwrap()
            .rsplit('.')
            .next()
            .unwrap()
    }
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

fn guessed_names(t: &TypeDesc, types: &TypeTable, legacy: bool, out: &mut Vec<String>) {
    match t {
        TypeDesc::Struct { name, .. } | TypeDesc::Union { name, .. }
            if types.is_guessed_class(t) || (legacy && types.is_undeclared_class(t)) =>
        {
            out.push(name.clone());
        }
        TypeDesc::Ptr(t) | TypeDesc::Array { elem: t, .. } => guessed_names(t, types, legacy, out),
        TypeDesc::FnPtr { ret, params } => {
            guessed_names(ret, types, legacy, out);
            for p in params {
                guessed_names(p, types, legacy, out);
            }
        }
        _ => {}
    }
}

impl LinkingFacts {
    /// Legacy exports have body origins but no source-view records. Preserve
    /// method boundaries where attributable; ambiguous class boundaries stay open.
    pub fn source_views(&self) -> std::borrow::Cow<'_, [LinkView]> {
        if !self.views.is_empty() {
            return std::borrow::Cow::Borrowed(&self.views);
        }
        let origins: std::collections::BTreeSet<_> = self
            .calls
            .iter()
            .map(|c| c.origin.clone())
            .chain(
                self.functions
                    .iter()
                    .flat_map(|f| f.definition_origins.iter().cloned()),
            )
            .collect();
        let single = origins.len() <= 1;
        let origins = if origins.is_empty() {
            vec![String::new()]
        } else {
            origins.into_iter().collect()
        };
        std::borrow::Cow::Owned(
            origins
                .into_iter()
                .map(|origin| LinkView {
                    classes: self
                        .classes
                        .iter()
                        .map(|(name, _, final_class)| (name.clone(), single && *final_class))
                        .collect(),
                    methods: self
                        .functions
                        .iter()
                        .filter(|f| !f.defined || f.definition_origins.contains(&origin))
                        .map(|f| (f.id, f.final_member && (single || f.defined)))
                        .collect(),
                    origin,
                })
                .collect(),
        )
    }

    /// Older payloads did not distinguish guessed parameter tags. Undeclared
    /// tags must stay uncertain rather than exclude scalar-typedef overrides.
    pub fn recover_legacy_parameter_guesses(&mut self) {
        if !self.views.is_empty() {
            return;
        }
        let mut types = TypeTable::default();
        for (name, _, _) in &self.classes {
            types.declare_struct(name);
        }
        for f in &mut self.functions {
            for t in f.params.iter().flatten() {
                guessed_names(t, &types, true, &mut f.guessed_params);
            }
            f.guessed_params.sort();
            f.guessed_params.dedup();
        }
    }

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
        let functions: Vec<_> = p
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
                guessed_params: {
                    let mut names = Vec::new();
                    if let Some(params) = p.symbols.explicit_params(f) {
                        for i in 0..params.len() {
                            if let Some(t) = params.get(i) {
                                guessed_names(&p.types.get(t).desc, &p.types, false, &mut names);
                            }
                        }
                    }
                    names.sort();
                    names.dedup();
                    names
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
                let mut call = LinkCall {
                    id: c.id,
                    source_site: *source_sites
                        .entry((c.caller, c.occurrence()))
                        .or_insert(c.id),
                    caller: c.caller,
                    callee: if implicit { None } else { c.callee_fn_id },
                    name: c.callee_name.to_string(),
                    receiver_declared: p.types.declares_class(&receiver),
                    receiver,
                    exact: c.exact_receiver,
                    origin: p.root.to_string_lossy().into_owned(),
                    implicit,
                };
                if implicit
                    || !call.name.contains("::")
                    || call.name.contains("->")
                    || call.name.contains('.')
                {
                    call.name = format!("{}::{}", call.receiver, call.method_name());
                }
                Some(call)
            })
            .collect();
        let mut senders: Vec<_> = p
            .symbols
            .call_sites
            .iter()
            .filter(|c| ipc_enabled && !c.unevaluated && crate::is_ipc_send(&c.callee_name))
            .map(|c| c.caller)
            .collect();
        senders.sort();
        senders.dedup();
        let views = vec![LinkView {
            origin: p.root.to_string_lossy().into_owned(),
            classes: classes
                .iter()
                .map(|(name, _, final_class)| (name.clone(), *final_class))
                .collect(),
            methods: functions.iter().map(|f| (f.id, f.final_member)).collect(),
        }];
        Self {
            complete: true,
            views,
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
