use crate::constraints::{AbstractLocation, Constraint, ConstraintKind, LocKind};
use crate::ipc::detect_ipc_pairs;
use crate::summaries::{Effect, FnModelSet};
use indexmap::IndexMap;
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
use trace_ir::{
    FieldId, FlowConstraint, FnId, LocId, PagNodeId, Program, ReturnFlow, StorageClass, VarId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PagNodeKind {
    Var(VarId),
    Loc(LocId),
    CallTarget(trace_ir::CallSiteId),
}

#[derive(Debug, Clone)]
pub struct PagNode {
    pub id: PagNodeId,
    pub kind: PagNodeKind,
}

/// Adjacency lists for constraint propagation (built once after PAG construction).
#[derive(Debug, Default)]
pub struct SolverIndices {
    pub copy_src: FxHashMap<PagNodeId, Vec<usize>>,
    pub addr_of_dst: FxHashMap<PagNodeId, Vec<usize>>,
    pub load_src: FxHashMap<PagNodeId, Vec<usize>>,
    pub store_dst: FxHashMap<PagNodeId, Vec<usize>>,
    pub store_src: FxHashMap<PagNodeId, Vec<usize>>,
    pub gep_src: FxHashMap<PagNodeId, Vec<usize>>,
    pub dlsym_src: FxHashMap<PagNodeId, Vec<usize>>,
    /// `UnwrapPointer` constraints by source, each with its receiver's
    /// pointee declaration so propagation needs no further lookup.
    pub unwrap_src: FxHashMap<PagNodeId, Vec<(usize, trace_ir::TypeId)>>,
    pub indirect_by_target: FxHashMap<PagNodeId, Vec<trace_ir::CallSiteId>>,
}

/// Maximum nesting depth for instance-sensitive field locations. Deeper
/// accesses fold into instance-insensitive summaries (see `ensure_field_loc`).
const FIELD_LOC_DEPTH_CAP: u8 = 4;

#[derive(Debug, Default)]
pub struct Pag {
    pub nodes: Vec<PagNode>,
    pub constraints: Vec<Constraint>,
    pub locations: Vec<AbstractLocation>,
    pub var_node: IndexMap<VarId, PagNodeId, FxBuildHasher>,
    pub loc_node: IndexMap<LocId, PagNodeId, FxBuildHasher>,
    pub call_targets: IndexMap<trace_ir::CallSiteId, PagNodeId, FxBuildHasher>,
    pub fn_locations: IndexMap<FnId, LocId, FxBuildHasher>,
    pub var_location: IndexMap<VarId, LocId, FxBuildHasher>,
    /// Interned `StringConst` locations keyed by literal contents.
    pub string_locs: FxHashMap<String, LocId>,
    /// Field abstract locations keyed by (parent object location, field id).
    pub field_loc: IndexMap<(LocId, FieldId), LocId, FxBuildHasher>,
    /// Nesting depth of each synthesized field location (var-rooted = 0
    /// children start at 1). Bounds recursive `obj->next->next->...`
    /// location synthesis once interprocedural flow reaches chained structs.
    pub field_depth: FxHashMap<LocId, u8>,
    /// Per-(link target, struct type, field) summary location for
    /// instance-insensitive field flow. Separate images never share a mutable
    /// storage summary; without link metadata every key carries `None`.
    pub field_summary:
        IndexMap<(Option<trace_ir::TargetId>, trace_ir::TypeId, FieldId), LocId, FxBuildHasher>,
    pub field_loc_to_summary: IndexMap<LocId, LocId, FxBuildHasher>,
    /// `(struct type, field)` as a location records it → the declaration
    /// member its summary is keyed by. Memoized: resolving it compares tag
    /// and member names, and the GEP fallback asks on every pop.
    declared_member: FxHashMap<(trace_ir::TypeId, FieldId), (trace_ir::TypeId, FieldId)>,
    /// Only scoped locations have entries; also carries scope through heap
    /// fields and summary-rooted nested accesses that have no owning variable.
    location_targets: FxHashMap<LocId, trace_ir::TargetId>,
    /// Fn locations parked into an array var by `ArrayFnMember` inits
    /// (`{ {.., Fn}, .. }`); reachable through any element field load.
    pub array_fn_members: FxHashMap<VarId, Vec<LocId>>,
    /// Maps callee_var to the PAG nodes that should receive the
    /// return value of indirect calls whose function pointer is that var.
    pub indirect_return_dst: FxHashMap<VarId, Vec<PagNodeId>>,
    /// Detected IPC proxy→stub bridges (proxy method → stub handler).
    /// The solver emits a synthetic call edge for each bridge.
    pub ipc_bridges: Vec<trace_ir::IpcBridge>,
    pub indices: SolverIndices,
}

impl Pag {
    pub fn build(program: &Program) -> Self {
        Self::build_with_models(program, &FnModelSet::builtin())
    }

    pub fn build_with_models(program: &Program, models: &FnModelSet) -> Self {
        Self::build_with_models_and_ipc(program, models, true)
    }

    pub(crate) fn build_with_models_and_ipc(
        program: &Program,
        models: &FnModelSet,
        enable_ipc: bool,
    ) -> Self {
        let mut pag = Self::default();
        pag.build_variables(program);
        pag.build_function_locations(program);
        pag.build_flow_constraints(program, models);
        pag.build_dlsym_constraints(program, models);
        pag.build_call_constraints(program);
        pag.build_indices(program);
        if enable_ipc {
            pag.ipc_bridges = detect_ipc_pairs(program);
        }
        pag
    }

    fn alloc_node(&mut self, kind: PagNodeKind) -> PagNodeId {
        let id = PagNodeId(self.nodes.len() as u32);
        self.nodes.push(PagNode { id, kind });
        id
    }

    fn alloc_loc(&mut self, loc: AbstractLocation) -> LocId {
        let id = loc.id;
        let node_id = self.alloc_node(PagNodeKind::Loc(id));
        self.loc_node.insert(id, node_id);
        self.locations.push(loc);
        id
    }

    pub fn var_node_id(&mut self, var: VarId) -> PagNodeId {
        if let Some(&id) = self.var_node.get(&var) {
            return id;
        }
        let id = self.alloc_node(PagNodeKind::Var(var));
        self.var_node.insert(var, id);
        id
    }

    /// A node per variable, and a location per global or static that some
    /// fact names. A global no flow, return or call site names (a header's
    /// declaration repeated into every including unit) would be an isolated
    /// node holding only its own address; it gets none, and a later lookup
    /// that reaches it creates both through [`Pag::ensure_var_loc`]. Nodes
    /// keep variable order either way.
    fn build_variables(&mut self, program: &Program) {
        let named = named_statics(program);
        for var in &program.symbols.variables {
            let is_static = has_static_storage(var.storage);
            if is_static && !named.contains(&var.id) {
                continue;
            }
            self.var_node_id(var.id);
            if !is_static {
                continue;
            }
            let kind = match var.storage {
                StorageClass::Global => LocKind::Global,
                StorageClass::FileStatic => LocKind::FileStatic,
                StorageClass::FnStatic => LocKind::FnStatic,
                StorageClass::Local | StorageClass::Param => LocKind::Local,
            };
            let loc_id = LocId(self.locations.len() as u32);
            self.alloc_loc(AbstractLocation {
                id: loc_id,
                kind,
                var: Some(var.id),
                fn_id: var.fn_id,
                field: None,
                type_id: var.type_id,
                desc: var.name.clone(),
            });
            self.set_location_target(loc_id, var.target);
            self.var_location.insert(var.id, loc_id);
        }
    }

    /// `var`'s location, created on first use along with its node, so every
    /// variable with a location has a node.
    pub fn ensure_var_loc(&mut self, program: &Program, var: VarId) -> Option<LocId> {
        if let Some(&loc) = self.var_location.get(&var) {
            return Some(loc);
        }
        let v = program.symbols.variable_by_id(var)?;
        self.var_node_id(var);
        let kind = match v.storage {
            StorageClass::Global => LocKind::Global,
            StorageClass::FileStatic => LocKind::FileStatic,
            StorageClass::FnStatic => LocKind::FnStatic,
            StorageClass::Local | StorageClass::Param => LocKind::Local,
        };
        let loc_id = LocId(self.locations.len() as u32);
        self.alloc_loc(AbstractLocation {
            id: loc_id,
            kind,
            var: Some(var),
            fn_id: v.fn_id,
            field: None,
            type_id: v.type_id,
            desc: v.name.clone(),
        });
        self.set_location_target(loc_id, v.target);
        self.var_location.insert(var, loc_id);
        Some(loc_id)
    }

    /// Give a global or static reached after [`Pag::build_variables`] the
    /// location it would have had there.
    fn ensure_static_var(&mut self, program: &Program, var: VarId) {
        if program
            .symbols
            .variable_by_id(var)
            .is_some_and(|v| has_static_storage(v.storage))
        {
            self.ensure_var_loc(program, var);
        }
    }

    fn build_indices(&mut self, program: &Program) {
        for (i, c) in self.constraints.iter().enumerate() {
            match c.kind {
                ConstraintKind::Copy => {
                    self.indices.copy_src.entry(c.src).or_default().push(i);
                }
                ConstraintKind::AddrOf => {
                    self.indices.addr_of_dst.entry(c.dst).or_default().push(i);
                }
                ConstraintKind::Load => {
                    self.indices.load_src.entry(c.src).or_default().push(i);
                }
                ConstraintKind::Store => {
                    self.indices.store_dst.entry(c.dst).or_default().push(i);
                    self.indices.store_src.entry(c.src).or_default().push(i);
                }
                ConstraintKind::Gep => {
                    self.indices.gep_src.entry(c.src).or_default().push(i);
                }
                ConstraintKind::Dlsym => {
                    self.indices.dlsym_src.entry(c.src).or_default().push(i);
                }
                ConstraintKind::UnwrapPointer => {
                    // The receiver's type is the pointee's: its declaration,
                    // resolved once here, is what the solver admits by.
                    let PagNodeKind::Var(receiver) = self.nodes[c.dst.0 as usize].kind else {
                        unreachable!("an unwrap's destination is its receiver variable");
                    };
                    let pointee = program
                        .types
                        .tag_identity(program.symbols.variable(receiver).type_id);
                    self.indices
                        .unwrap_src
                        .entry(c.src)
                        .or_default()
                        .push((i, pointee));
                }
            }
        }
        for cs in &program.symbols.call_sites {
            if cs.is_direct {
                continue;
            }
            if let Some(&target) = self.call_targets.get(&cs.id) {
                self.indices
                    .indirect_by_target
                    .entry(target)
                    .or_default()
                    .push(cs.id);
            }
        }
    }

    /// Index constraints added after `build_indices` ran (e.g. from
    /// `expand_return_flows` during the solver worklist loop).
    pub(crate) fn index_new_constraints(&mut self, from: usize) -> Vec<PagNodeId> {
        let mut srcs = Vec::new();
        for i in from..self.constraints.len() {
            let c = &self.constraints[i];
            match c.kind {
                ConstraintKind::Copy => {
                    self.indices.copy_src.entry(c.src).or_default().push(i);
                    srcs.push(c.src);
                }
                ConstraintKind::AddrOf => {
                    self.indices.addr_of_dst.entry(c.dst).or_default().push(i);
                    srcs.push(c.src);
                }
                ConstraintKind::Load => {
                    self.indices.load_src.entry(c.src).or_default().push(i);
                    srcs.push(c.src);
                }
                ConstraintKind::Store => {
                    self.indices.store_dst.entry(c.dst).or_default().push(i);
                    self.indices.store_src.entry(c.src).or_default().push(i);
                    srcs.push(c.src);
                    srcs.push(c.dst);
                }
                ConstraintKind::Gep => {
                    self.indices.gep_src.entry(c.src).or_default().push(i);
                    srcs.push(c.src);
                }
                ConstraintKind::Dlsym => {
                    self.indices.dlsym_src.entry(c.src).or_default().push(i);
                    srcs.push(c.src);
                }
                ConstraintKind::UnwrapPointer => {
                    unreachable!(
                        "unwraps are lowered facts, built with the PAG, not added while solving"
                    )
                }
            }
        }
        srcs
    }

    fn set_location_target(&mut self, loc: LocId, target: Option<trace_ir::TargetId>) {
        if let Some(target) = target {
            self.location_targets.insert(loc, target);
        }
    }

    /// The link image a PAG node belongs to. The single answer to that
    /// question: consumers of points-to results must not each re-derive it,
    /// or they disagree about node kinds the way two hand-rolled matches did.
    pub fn node_target(&self, program: &Program, node: PagNodeId) -> Option<trace_ir::TargetId> {
        // Gated on the feature, not on `location_targets`: that map fills in
        // as the PAG is built, so an early query would otherwise read `None`
        // for a variable whose target the symbol table already knows — and a
        // heap location stamped `None` shares its field summaries with every
        // other image.
        if program.link_targets.is_empty() {
            return None;
        }
        match self.nodes[node.0 as usize].kind {
            PagNodeKind::Var(var) => program.symbols.variable_by_id(var).and_then(|v| v.target),
            PagNodeKind::Loc(loc) => self.location_target(loc),
            PagNodeKind::CallTarget(site) => program
                .symbols
                .call_site_by_id(site)
                .and_then(|cs| program.symbols.function(cs.caller).target),
        }
    }

    fn location_target(&self, loc: LocId) -> Option<trace_ir::TargetId> {
        if self.location_targets.is_empty() {
            return None;
        }
        self.location_targets.get(&loc).copied()
    }

    fn alloc_field_loc(
        &mut self,
        parent_loc: LocId,
        field: FieldId,
        field_type: trace_ir::TypeId,
        name: &str,
    ) -> LocId {
        if let Some(&loc) = self.field_loc.get(&(parent_loc, field)) {
            return loc;
        }
        let base_var = self.locations[parent_loc.0 as usize].var;
        let loc_id = LocId(self.locations.len() as u32);
        self.alloc_loc(AbstractLocation {
            id: loc_id,
            kind: LocKind::Field,
            var: base_var,
            fn_id: None,
            field: Some(field),
            type_id: field_type,
            desc: name.to_string(),
        });
        let depth = self.field_depth.get(&parent_loc).copied().unwrap_or(0) + 1;
        self.field_depth.insert(loc_id, depth);
        self.set_location_target(loc_id, self.location_target(parent_loc));
        self.field_loc.insert((parent_loc, field), loc_id);
        loc_id
    }

    /// Instance-sensitive child location for `parent.field`. Past
    /// `FIELD_LOC_DEPTH_CAP`, further nesting is folded into the
    /// instance-insensitive summary: unbounded recursive synthesis (linked
    /// structures reached through interprocedural flow) would otherwise
    /// diverge, while summaries stay bounded by (type, field).
    pub fn ensure_field_loc(
        &mut self,
        program: &Program,
        parent_loc: LocId,
        field: FieldId,
    ) -> Option<LocId> {
        if let Some(&loc) = self.field_loc.get(&(parent_loc, field)) {
            return Some(loc);
        }
        let parent_type = struct_type_for_loc(self, program, parent_loc)?;
        let target = self.location_target(parent_loc);
        let field_layout = program.types.get(parent_type).layout.fields.get(&field)?;
        if self.field_depth.get(&parent_loc).copied().unwrap_or(0) >= FIELD_LOC_DEPTH_CAP {
            return Some(self.ensure_field_summary_loc(
                program,
                parent_type,
                target,
                field,
                field_layout.type_id,
                &field_layout.name,
            ));
        }
        let field_loc =
            self.alloc_field_loc(parent_loc, field, field_layout.type_id, &field_layout.name);
        let summary = self.ensure_field_summary_loc(
            program,
            parent_type,
            target,
            field,
            field_layout.type_id,
            &field_layout.name,
        );
        self.field_loc_to_summary.insert(field_loc, summary);
        Some(field_loc)
    }

    fn ensure_field_summary_loc(
        &mut self,
        program: &Program,
        struct_type: trace_ir::TypeId,
        target: Option<trace_ir::TargetId>,
        field: FieldId,
        field_type: trace_ir::TypeId,
        name: &str,
    ) -> LocId {
        // One summary per declaration member, whichever snapshot of the
        // declaration the base's type records (docs/ANALYSIS.md, "Field
        // sensitivity"). A member the declaration's richest layout lacks keeps
        // the snapshot's own summary.
        let (struct_type, field) = *self
            .declared_member
            .entry((struct_type, field))
            .or_insert_with(|| {
                // The member's own name, read off the layout, so the answer
                // depends only on the memo's key.
                let Some(member) = program.types.get(struct_type).layout.fields.get(&field) else {
                    return (struct_type, field);
                };
                let member_name = member.name.as_str();
                let declared = program.types.tag_identity(struct_type);
                match program.types.field_id_by_name(declared, member_name) {
                    Some(member) if declared != struct_type => (declared, member),
                    _ => (struct_type, field),
                }
            });
        if let Some(&loc) = self.field_summary.get(&(target, struct_type, field)) {
            return loc;
        }
        let struct_name = match program.types.get(struct_type).desc.as_ref() {
            trace_ir::TypeDesc::Struct { name, .. } => name.clone(),
            trace_ir::TypeDesc::Union { name, .. } => name.clone(),
            _ => format!("type{}", struct_type.0),
        };
        let loc_id = LocId(self.locations.len() as u32);
        self.alloc_loc(AbstractLocation {
            id: loc_id,
            kind: LocKind::FieldSummary,
            var: None,
            fn_id: None,
            field: Some(field),
            type_id: field_type,
            desc: format!("summary:{struct_name}.{name}"),
        });
        self.field_summary
            .insert((target, struct_type, field), loc_id);
        self.set_location_target(loc_id, target);
        loc_id
    }

    pub fn summary_for_field_loc(&self, field_loc: LocId) -> Option<LocId> {
        self.field_loc_to_summary.get(&field_loc).copied()
    }

    /// Instance-insensitive summary location for `(struct type of var, field)`.
    #[must_use]
    pub fn ensure_field_summary_for_var(
        &mut self,
        program: &Program,
        var: trace_ir::VarId,
        field: FieldId,
    ) -> Option<LocId> {
        self.ensure_field_summary_for_var_named(program, var, field, None)
    }

    /// Instance-insensitive summary location for `(struct type of var, field)`,
    /// falling back to resolving `expected_name` if positional `field` does not match.
    #[must_use]
    pub fn ensure_field_summary_for_var_named(
        &mut self,
        program: &Program,
        var: trace_ir::VarId,
        field: FieldId,
        expected_name: Option<&str>,
    ) -> Option<LocId> {
        let v = program.symbols.variable_by_id(var)?;
        let struct_type = struct_type_from_type_id(program, v.type_id)?;
        let mut resolved_field = field;
        let positional = program.types.get(struct_type).layout.fields.get(&field);
        // Resolve by name only when the positional index does not already carry
        // the field the GEP named: a unioned layout can move a field off the
        // index the configuration that lowered the GEP gave it.
        let by_name = expected_name.and_then(|expected| {
            if expected.is_empty() || positional.is_some_and(|fl| fl.name == expected) {
                None
            } else {
                program.types.field_id_by_name(struct_type, expected)
            }
        });
        let field_layout = if let Some(fid) = by_name {
            resolved_field = fid;
            program.types.get(struct_type).layout.fields.get(&fid)?
        } else {
            // The name resolves nowhere in this layout — a base variable whose
            // static type simply has no such member. Keep the positional field
            // the baseline path would have used: merging a variant must only
            // ever add facts, never drop one the baseline had.
            positional?
        };
        Some(self.ensure_field_summary_loc(
            program,
            struct_type,
            v.target,
            resolved_field,
            field_layout.type_id,
            &field_layout.name,
        ))
    }

    pub fn field_loc_for_parent(&self, parent_loc: LocId, field: FieldId) -> Option<LocId> {
        self.field_loc.get(&(parent_loc, field)).copied()
    }

    /// Declared parameter count of the function-pointer slot `base.field`,
    /// used by the solver to keep signature-incompatible function values out
    /// of typed slots under wrong-type pointer flow. `None` = slot is not
    /// (known to be) a fn pointer.
    pub fn field_slot_arity(
        &self,
        program: &Program,
        base_loc: LocId,
        field: FieldId,
    ) -> Option<usize> {
        let parent_type = struct_type_for_loc(self, program, base_loc)?;
        let layout = program.types.get(parent_type).layout.fields.get(&field)?;
        match program.types.get(layout.type_id).desc.as_ref() {
            trace_ir::TypeDesc::FnPtr { params, .. } => Some(params.len()),
            _ => None,
        }
    }

    fn build_function_locations(&mut self, program: &Program) {
        for func in &program.symbols.functions {
            if self.fn_locations.contains_key(&func.id) {
                continue;
            }
            let loc_id = LocId(self.locations.len() as u32);
            self.alloc_loc(AbstractLocation {
                id: loc_id,
                kind: LocKind::Function,
                var: None,
                fn_id: Some(func.id),
                field: None,
                type_id: func.return_type,
                desc: func.name.clone(),
            });
            self.set_location_target(loc_id, func.target);
            self.fn_locations.insert(func.id, loc_id);
        }
    }

    fn build_flow_constraints(&mut self, program: &Program, models: &FnModelSet) {
        for flow in &program.flow {
            match flow {
                FlowConstraint::Copy { dst, src } => {
                    let dst_n = self.var_node_id(*dst);
                    let src_n = self.var_node_id(*src);
                    self.add_copy(dst_n, src_n);
                }
                FlowConstraint::AddrOfVar { dst, src } => {
                    let dst_n = self.var_node_id(*dst);
                    if let Some(loc) = self.ensure_var_loc(program, *src) {
                        let loc_n = self.loc_node[&loc];
                        self.add_addr_of(dst_n, loc_n);
                    }
                }
                FlowConstraint::AddrOfFn { dst, callee } => {
                    let dst_n = self.var_node_id(*dst);
                    // `callee` was resolved in TU scope during lowering and
                    // remapped at merge; a global name lookup here could bind
                    // an unrelated same-name function (e.g. file-`static`s).
                    if let Some(&fn_loc) = self.fn_locations.get(callee) {
                        let loc_n = self.loc_node[&fn_loc];
                        self.add_addr_of(dst_n, loc_n);
                    }
                }
                FlowConstraint::Load { dst, src } => {
                    let dst_n = self.var_node_id(*dst);
                    let src_n = self.var_node_id(*src);
                    self.add_load(dst_n, src_n);
                }
                FlowConstraint::Store { dst, src } => {
                    let dst_n = self.var_node_id(*dst);
                    let src_n = self.var_node_id(*src);
                    self.add_store(dst_n, src_n);
                }
                FlowConstraint::GepField {
                    dst,
                    base,
                    field,
                    field_name,
                } => {
                    let dst_n = self.var_node_id(*dst);
                    let base_n = self.var_node_id(*base);
                    self.add_gep(dst_n, base_n, *field, field_name.clone());
                }
                FlowConstraint::ArrayFnMember { array, callee } => {
                    let array_n = self.var_node_id(*array);
                    // Trust the merge-remapped FnId (see AddrOfFn above).
                    if let Some(&fn_loc) = self.fn_locations.get(callee) {
                        let loc_n = self.loc_node[&fn_loc];
                        self.add_addr_of(array_n, loc_n);
                        // Also record for element-field loads through
                        // pointers to the array (order-independent).
                        self.array_fn_members
                            .entry(*array)
                            .or_default()
                            .push(fn_loc);
                    }
                }
                FlowConstraint::CallReturn {
                    dst,
                    callee_name,
                    caller,
                } => {
                    let dst_n = self.var_node_id(*dst);
                    let mut visited = FxHashSet::default();
                    let candidates =
                        program
                            .symbols
                            .call_return_candidates(*caller, *dst, callee_name);
                    let mut any_real = false;
                    for callee in &candidates {
                        if self.expand_return_flows(program, dst_n, *callee, models, &mut visited) {
                            any_real = true;
                        }
                    }
                    // Modeled return effects fire only when no real return
                    // flow exists (bodyless callees: libc realloc, vendor
                    // allocators). Synthesized externals never enter the
                    // name-resolution maps, so the model is consulted by
                    // call name directly.
                    if !any_real {
                        if let Some(model) = models.get(callee_name) {
                            let params = candidates
                                .iter()
                                .find(|c| !program.symbols.function(**c).params.is_empty())
                                .map(|c| program.symbols.function(*c).params.clone());
                            self.apply_return_model(program, dst_n, model, params.as_deref());
                        }
                    }
                }
                FlowConstraint::CallReturnIndirect { dst, callee_var } => {
                    // Record the return destination so the solver can expand
                    // return flows when it resolves indirect call targets.
                    let dst_n = self.var_node_id(*dst);
                    let dsts = self.indirect_return_dst.entry(*callee_var).or_default();
                    if !dsts.contains(&dst_n) {
                        dsts.push(dst_n);
                    }
                }
                FlowConstraint::NewHeap { dst } => {
                    let dst_n = self.var_node_id(*dst);
                    let var = program.symbols.variable(*dst);
                    let type_id = var.type_id;
                    let loc = self.alloc_heap_loc("new heap".to_string());
                    self.set_location_target(loc, var.target);
                    let loc_n = self.loc_node[&loc];
                    self.locations[loc.0 as usize].type_id = type_id;
                    self.add_addr_of(dst_n, loc_n);
                }
                FlowConstraint::StringConst { dst, value } => {
                    let dst_n = self.var_node_id(*dst);
                    let loc = self.intern_string_loc(program, value);
                    let loc_n = self.loc_node[&loc];
                    self.add_addr_of(dst_n, loc_n);
                }
                FlowConstraint::UnwrapPointer { dst, src } => {
                    let dst_n = self.var_node_id(*dst);
                    let src_n = self.var_node_id(*src);
                    self.add_unwrap(dst_n, src_n);
                }
            }
        }
    }

    fn intern_string_loc(&mut self, program: &Program, value: &str) -> LocId {
        if let Some(&loc) = self.string_locs.get(value) {
            return loc;
        }
        let type_id = program
            .types
            .all()
            .iter()
            .find(|t| matches!(t.desc.as_ref(), trace_ir::TypeDesc::Char))
            .map(|t| t.id)
            .unwrap_or_else(|| program.types.void());
        let loc_id = LocId(self.locations.len() as u32);
        self.alloc_loc(AbstractLocation {
            id: loc_id,
            kind: LocKind::StringLit,
            var: None,
            fn_id: None,
            field: None,
            type_id,
            desc: value.to_string(),
        });
        self.string_locs.insert(value.to_string(), loc_id);
        loc_id
    }

    /// Persistent `Dlsym` edges: `pts(return_dst)` gains function locations
    /// named by string constants in the name-argument node. Wired here
    /// (not in `apply_fn_model`) so later-arriving string constants still fire.
    fn build_dlsym_constraints(&mut self, program: &Program, models: &FnModelSet) {
        for cs in &program.symbols.call_sites {
            let Some(model) = models.get_for_callee(&cs.callee_name) else {
                continue;
            };
            let Some(name_param) = model.effects.iter().find_map(|e| match e {
                Effect::Dlsym { name_param } => Some(*name_param),
                _ => None,
            }) else {
                continue;
            };
            let Some(dst_var) = cs.return_dst else {
                continue;
            };
            let Some((_, name_var)) = cs.var_args.iter().find(|(i, _)| *i == name_param) else {
                continue;
            };
            let dst_n = self.var_node_id(dst_var);
            let src_n = self.var_node_id(*name_var);
            self.add_dlsym(dst_n, src_n);
        }
    }

    pub(crate) fn expand_return_flows(
        &mut self,
        program: &Program,
        dst: PagNodeId,
        callee: FnId,
        models: &FnModelSet,
        visited: &mut FxHashSet<FnId>,
    ) -> bool {
        if !visited.insert(callee) {
            return false;
        }
        match program.fn_returns.get(&callee) {
            Some(flows) => {
                let mut applied = false;
                for flow in flows.clone() {
                    match flow {
                        ReturnFlow::AddrOfVar { src } => {
                            if let Some(loc) = self.ensure_var_loc(program, src) {
                                let loc_n = self.loc_node[&loc];
                                self.add_addr_of(dst, loc_n);
                                applied = true;
                            }
                        }
                        ReturnFlow::AddrOfFn { callee: fn_id } => {
                            // Trust the merge-remapped FnId (see AddrOfFn above).
                            if let Some(&fn_loc) = self.fn_locations.get(&fn_id) {
                                let loc_n = self.loc_node[&fn_loc];
                                self.add_addr_of(dst, loc_n);
                                applied = true;
                            }
                        }
                        ReturnFlow::Copy { src } => {
                            let src_n = self.var_node_id(src);
                            self.add_copy(dst, src_n);
                            applied = true;
                        }
                        ReturnFlow::Call { callee_name } => {
                            let inner_candidates =
                                program.symbols.return_flow_candidates(callee, &callee_name);
                            let mut inner_applied = false;
                            for inner in inner_candidates.iter().copied() {
                                if self.expand_return_flows(program, dst, inner, models, visited) {
                                    inner_applied = true;
                                }
                            }
                            // Bodyless leaf (`return realloc(p, n)` where
                            // realloc has no tree body): fall back to the
                            // modeled return effects.
                            if !inner_applied {
                                if let Some(model) = models.get(&callee_name) {
                                    let params = inner_candidates
                                        .iter()
                                        .find(|c| !program.symbols.function(**c).params.is_empty())
                                        .map(|c| program.symbols.function(*c).params.clone());
                                    self.apply_return_model(program, dst, model, params.as_deref());
                                    // Modeled heap/alias facts count as applied
                                    // so outer frames do not re-apply them.
                                    inner_applied = true;
                                }
                            }
                            if inner_applied {
                                applied = true;
                            }
                        }
                    }
                }
                applied
            }
            // No body under the analyzed root: no real return facts here.
            None => false,
        }
    }

    /// Apply `return_alias` / `return_heap` model effects to a `CallReturn`
    /// destination whose callee has no body under the analyzed root.
    fn apply_return_model(
        &mut self,
        program: &Program,
        dst: PagNodeId,
        model: &crate::summaries::FnModel,
        params: Option<&[VarId]>,
    ) {
        for effect in &model.effects {
            match effect {
                Effect::ReturnAlias { param } => {
                    if let Some(formal) = params.and_then(|ps| ps.get(*param as usize)) {
                        let formal_n = self.var_node_id(*formal);
                        self.add_copy(dst, formal_n);
                    }
                }
                Effect::ReturnHeap => {
                    let name = &model.name;
                    let loc = self.alloc_heap_loc(format!("{name}() storage"));
                    let target = self.node_target(program, dst);
                    self.set_location_target(loc, target);
                    let loc_n = self.loc_node[&loc];
                    self.add_addr_of(dst, loc_n);
                }
                _ => {}
            }
        }
    }

    /// Fresh anonymous storage location (malloc-family return summaries).
    fn alloc_heap_loc(&mut self, desc: String) -> LocId {
        let id = LocId(self.locations.len() as u32);
        self.alloc_loc(AbstractLocation {
            id,
            kind: LocKind::Heap,
            var: None,
            fn_id: None,
            field: None,
            type_id: trace_ir::TypeId(0),
            desc,
        });
        id
    }

    fn build_call_constraints(&mut self, program: &Program) {
        let mut fn_vars: FxHashMap<FnId, FxHashMap<String, VarId>> = FxHashMap::default();
        for var in &program.symbols.variables {
            if let Some(fn_id) = var.fn_id {
                fn_vars
                    .entry(fn_id)
                    .or_default()
                    .insert(var.name.clone(), var.id);
            }
        }
        for cs in &program.symbols.call_sites {
            if cs.is_direct {
                continue;
            }
            if let Some(var) = cs.callee_var {
                let call_target = self.call_target_node(cs.id);
                let var_node = self.var_node_id(var);
                if cs.callee_name.contains("->") || cs.callee_name.contains('.') {
                    self.add_copy(call_target, var_node);
                } else {
                    self.add_load(call_target, var_node);
                }
            } else if !callee_name_is_a_function(program, cs) {
                let call_target = self.call_target_node(cs.id);
                if let Some(v) = lookup_var_in_fn(&fn_vars, program, &cs.callee_name, cs.caller) {
                    // Found by name, so possibly a global no fact named.
                    self.ensure_static_var(program, v);
                    let var_node = self.var_node_id(v);
                    self.add_load(call_target, var_node);
                }
            }
        }
    }

    /// The variable whose address `node` was assigned (`node = &v`), if any.
    pub fn addressed_var(&self, node: PagNodeId) -> Option<VarId> {
        self.indices
            .addr_of_dst
            .get(&node)?
            .iter()
            .find_map(
                |&idx| match self.nodes[self.constraints[idx].src.0 as usize].kind {
                    PagNodeKind::Loc(loc) => self.locations[loc.0 as usize].var,
                    _ => None,
                },
            )
    }

    /// The variable argument `idx` of `cs` stands for: its actual, or for an
    /// `&x` argument ([`trace_ir::CallSite::addr_of_args`]) the `x` whose
    /// address the actual temporary holds.
    pub fn argument_var(&self, cs: &trace_ir::CallSite, idx: u32) -> Option<VarId> {
        let actual = cs.var_args.iter().find(|(j, _)| *j == idx)?.1;
        let addressed = cs
            .addr_of_args
            .contains(&idx)
            .then(|| {
                self.var_node
                    .get(&actual)
                    .and_then(|&n| self.addressed_var(n))
            })
            .flatten();
        Some(addressed.unwrap_or(actual))
    }

    pub fn call_target_node(&mut self, cs: trace_ir::CallSiteId) -> PagNodeId {
        if let Some(&id) = self.call_targets.get(&cs) {
            return id;
        }
        let id = self.alloc_node(PagNodeKind::CallTarget(cs));
        self.call_targets.insert(cs, id);
        id
    }

    pub fn add_copy(&mut self, dst: PagNodeId, src: PagNodeId) {
        self.constraints.push(Constraint {
            kind: crate::constraints::ConstraintKind::Copy,
            dst,
            src,
            field: None,
            field_name: None,
        });
    }

    fn add_unwrap(&mut self, dst: PagNodeId, src: PagNodeId) {
        self.constraints.push(Constraint {
            kind: crate::constraints::ConstraintKind::UnwrapPointer,
            dst,
            src,
            field: None,
            field_name: None,
        });
    }

    fn add_addr_of(&mut self, dst: PagNodeId, loc_node: PagNodeId) {
        self.constraints.push(Constraint {
            kind: crate::constraints::ConstraintKind::AddrOf,
            dst,
            src: loc_node,
            field: None,
            field_name: None,
        });
    }

    fn add_load(&mut self, dst: PagNodeId, src: PagNodeId) {
        self.constraints.push(Constraint {
            kind: crate::constraints::ConstraintKind::Load,
            dst,
            src,
            field: None,
            field_name: None,
        });
    }

    fn add_store(&mut self, dst: PagNodeId, src: PagNodeId) {
        self.constraints.push(Constraint {
            kind: crate::constraints::ConstraintKind::Store,
            dst,
            src,
            field: None,
            field_name: None,
        });
    }

    pub fn add_gep(&mut self, dst: PagNodeId, base: PagNodeId, field: FieldId, field_name: String) {
        self.constraints.push(Constraint {
            kind: crate::constraints::ConstraintKind::Gep,
            dst,
            src: base,
            field: Some(field),
            field_name: Some(field_name),
        });
    }

    fn add_dlsym(&mut self, dst: PagNodeId, src: PagNodeId) {
        self.constraints.push(Constraint {
            kind: crate::constraints::ConstraintKind::Dlsym,
            dst,
            src,
            field: None,
            field_name: None,
        });
    }
}

pub(crate) fn struct_type_for_loc(
    pag: &Pag,
    program: &Program,
    loc: LocId,
) -> Option<trace_ir::TypeId> {
    if let Some(var) = pag.locations[loc.0 as usize].var {
        return struct_type_from_type_id(program, program.symbols.variable_by_id(var)?.type_id);
    }
    type_id_of_loc(pag, program, loc)
}

fn inner_or_elem(desc: &trace_ir::TypeDesc) -> &trace_ir::TypeDesc {
    match desc {
        trace_ir::TypeDesc::Ptr(inner) | trace_ir::TypeDesc::Array { elem: inner, .. } => inner,
        other => other,
    }
}

fn type_id_of_loc(pag: &Pag, program: &Program, loc: LocId) -> Option<trace_ir::TypeId> {
    let mut type_id = pag.locations[loc.0 as usize].type_id;
    for _ in 0..4 {
        match program.types.get(type_id).desc.as_ref() {
            trace_ir::TypeDesc::Ptr(inner) => {
                type_id = program.types.resolve_type_id(inner);
            }
            // A table of structs: its elements' fields.
            trace_ir::TypeDesc::Array { elem, .. } => {
                type_id = program.types.resolve_type_id(inner_or_elem(elem));
            }
            _ => break,
        }
    }
    matches!(
        program.types.get(type_id).desc.as_ref(),
        trace_ir::TypeDesc::Struct { .. } | trace_ir::TypeDesc::Union { .. }
    )
    .then_some(type_id)
}

fn struct_type_from_type_id(
    program: &Program,
    mut type_id: trace_ir::TypeId,
) -> Option<trace_ir::TypeId> {
    for _ in 0..6 {
        match program.types.get(type_id).desc.as_ref() {
            trace_ir::TypeDesc::Ptr(inner) => {
                type_id = program
                    .types
                    .tag_declaration(inner)
                    .unwrap_or_else(|| program.types.resolve_type_id(inner));
            }
            // Arrays of structs: resolve fields against the element type.
            trace_ir::TypeDesc::Array { elem, .. } => {
                type_id = program.types.resolve_type_id(inner_or_elem(elem));
            }
            trace_ir::TypeDesc::Struct { fields, .. }
            | trace_ir::TypeDesc::Union { fields, .. } => {
                // Lowering numbers fields against the definition for an empty
                // forward declaration; nonempty snapshots keep their positions.
                let desc = program.types.get(type_id).desc.as_ref();
                return Some(if fields.is_empty() {
                    program.types.tag_declaration(desc).unwrap_or(type_id)
                } else {
                    type_id
                });
            }
            _ => return None,
        }
    }
    None
}

/// Whether the call's spelled name denotes a function rather than a
/// function-pointer variable. The companion `lookup_var_in_fn` below is
/// target-aware, so this guard has to be too: a function named `cb` in one
/// image must not stop another image's `cb` variable from feeding its own
/// call-target node. Without link metadata the historical whole-program
/// lookup is kept exactly.
fn callee_name_is_a_function(program: &Program, cs: &trace_ir::CallSite) -> bool {
    if program.link_targets.is_empty() {
        return program.symbols.resolve_function(&cs.callee_name).is_some();
    }
    program
        .symbols
        .resolve_function_in_scope_in_target(
            &cs.callee_name,
            Some(cs.scope_file()),
            program.symbols.function(cs.caller).target,
        )
        .is_some()
}

/// The globals and statics some flow constraint, return or call site
/// names: every static [`Pag::build_variables`] gives a node and location
/// up front.
fn named_statics(program: &Program) -> FxHashSet<VarId> {
    let mut named: FxHashSet<VarId> = program.flow.iter().flat_map(FlowConstraint::vars).collect();
    named.extend(
        program
            .fn_returns
            .values()
            .flatten()
            .filter_map(ReturnFlow::var),
    );
    for site in &program.symbols.call_sites {
        named.extend(site.callee_var);
        named.extend(site.return_dst);
        named.extend(site.var_args.iter().map(|&(_, v)| v));
    }
    named.retain(|&v| {
        program
            .symbols
            .variable_by_id(v)
            .is_some_and(|var| has_static_storage(var.storage))
    });
    named
}

/// Storage that outlives a call, and so has a location of its own.
fn has_static_storage(storage: StorageClass) -> bool {
    matches!(
        storage,
        StorageClass::Global | StorageClass::FileStatic | StorageClass::FnStatic
    )
}

fn lookup_var_in_fn(
    fn_vars: &FxHashMap<FnId, FxHashMap<String, VarId>>,
    program: &Program,
    name: &str,
    caller: FnId,
) -> Option<VarId> {
    fn_vars
        .get(&caller)
        .and_then(|m| m.get(name).copied())
        .or_else(|| {
            program
                .symbols
                .global_named(program.symbols.function(caller).target, name)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use trace_ir::{Span, TypeDesc, TypeId, Variable};

    /// Stage 6 (#133): a global no fact names gets neither a node nor a
    /// location; one a flow names gets both, in variable order.
    #[test]
    fn only_named_globals_get_nodes_and_locations() {
        let mut program = Program::new(".".into());
        let int = program.types.int();
        let global = |program: &mut Program, name: &str| {
            let id = program.symbols.alloc_var_id();
            program.symbols.add_variable(Variable {
                id,
                name: name.into(),
                type_id: int,
                storage: StorageClass::Global,
                fn_id: None,
                param_index: None,
                span: Span::new(trace_ir::FileId(0), 1, 1),
                is_pointer: false,
                is_defined: false,
                is_weak: false,
                target: None,
                is_namespaced: true,
                qualified_name: Some(format!("H::{name}")),
                c_linkage: false,
            });
            id
        };
        let unused = global(&mut program, "unused");
        let read = global(&mut program, "read");
        let out = global(&mut program, "out");
        program.flow.push(FlowConstraint::Copy {
            dst: out,
            src: read,
        });

        let mut pag = Pag::build(&program);
        assert!(!pag.var_node.contains_key(&unused));
        assert!(!pag.var_location.contains_key(&unused));
        for named in [read, out] {
            assert!(pag.var_node.contains_key(&named));
            assert!(pag.var_location.contains_key(&named));
        }
        assert!(pag.var_node[&read] < pag.var_node[&out], "variable order");

        // A later lookup gives the global both, as the build would have.
        pag.ensure_static_var(&program, unused);
        assert!(pag.var_node.contains_key(&unused));
        assert!(pag.var_location.contains_key(&unused));
    }

    #[test]
    fn field_summaries_share_within_targets_but_not_between_them() {
        let mut program = Program::new(".".into());
        let type_id = program.types.intern(TypeDesc::Struct {
            name: "Outer".into(),
            fields: vec![(
                "nested".into(),
                TypeDesc::Struct {
                    name: "Inner".into(),
                    fields: vec![("value".into(), TypeDesc::Int)],
                },
            )],
        });
        let mut vars = Vec::new();
        for target in [
            Some(trace_ir::TargetId(0)),
            Some(trace_ir::TargetId(0)),
            Some(trace_ir::TargetId(1)),
            None,
        ] {
            let id = program.symbols.alloc_var_id();
            program.symbols.add_variable(Variable {
                id,
                name: "object".into(),
                type_id,
                storage: StorageClass::Global,
                fn_id: None,
                param_index: None,
                span: Span::new(trace_ir::FileId(0), 1, 1),
                is_pointer: false,
                is_defined: true,
                is_weak: false,
                target,
                is_namespaced: false,
                qualified_name: None,
                c_linkage: false,
            });
            program.flow.push(FlowConstraint::NewHeap { dst: id });
            vars.push(id);
        }
        let mut pag = Pag::build(&program);
        let heaps: Vec<_> = pag
            .locations
            .iter()
            .filter(|loc| loc.kind == LocKind::Heap)
            .map(|loc| loc.id)
            .collect();
        let heap_a = pag
            .ensure_field_loc(&program, heaps[0], FieldId(0))
            .unwrap();
        let heap_b = pag
            .ensure_field_loc(&program, heaps[2], FieldId(0))
            .unwrap();
        assert_ne!(
            pag.summary_for_field_loc(heap_a),
            pag.summary_for_field_loc(heap_b)
        );
        let summaries: Vec<_> = vars
            .iter()
            .map(|v| {
                pag.ensure_field_summary_for_var(&program, *v, FieldId(0))
                    .unwrap()
            })
            .collect();
        assert_eq!(summaries[0], summaries[1]);
        assert_ne!(summaries[0], summaries[2]);
        assert_ne!(summaries[0], summaries[3]);
        let nested_a = pag
            .ensure_field_loc(&program, summaries[0], FieldId(0))
            .unwrap();
        let nested_b = pag
            .ensure_field_loc(&program, summaries[2], FieldId(0))
            .unwrap();
        assert_ne!(
            pag.summary_for_field_loc(nested_a),
            pag.summary_for_field_loc(nested_b)
        );
        let root_a = pag.ensure_var_loc(&program, vars[0]).unwrap();
        let field_a = pag.ensure_field_loc(&program, root_a, FieldId(0)).unwrap();
        assert_eq!(pag.summary_for_field_loc(field_a), Some(summaries[0]));
        let nested_field_a = pag.ensure_field_loc(&program, field_a, FieldId(0)).unwrap();
        let root_b = pag.ensure_var_loc(&program, vars[2]).unwrap();
        let field_b = pag.ensure_field_loc(&program, root_b, FieldId(0)).unwrap();
        let nested_field_b = pag.ensure_field_loc(&program, field_b, FieldId(0)).unwrap();
        assert_ne!(
            pag.summary_for_field_loc(nested_field_a),
            pag.summary_for_field_loc(nested_field_b)
        );
    }

    /// Issue #127 review: a store through `Outer.inner.GetDriver` and a load
    /// through `IDriverLoader *->GetDriver` share one summary even when the
    /// nested snapshot lacks a member the richest definition has (an
    /// `#ifdef`-only field). A member only the snapshot has keeps its own.
    #[test]
    fn field_summaries_follow_the_declaration_by_member_name() {
        let get = TypeDesc::FnPtr {
            ret: Box::new(TypeDesc::Int),
            params: Vec::new(),
        };
        let loader = |fields: Vec<(&str, TypeDesc)>| TypeDesc::Struct {
            name: "IDriverLoader".into(),
            fields: fields.into_iter().map(|(n, d)| (n.into(), d)).collect(),
        };
        let mut program = Program::new(".".into());
        let snapshot = program.types.intern(loader(vec![
            ("object", TypeDesc::Int),
            ("GetDriver", get.clone()),
            ("only_here", TypeDesc::Int),
        ]));
        let richest = loader(vec![
            ("object", TypeDesc::Int),
            ("debug_hook", TypeDesc::Int),
            ("GetDriver", get.clone()),
            ("ReclaimDriver", get),
        ]);
        program.types.intern(richest.clone());
        let through_ptr = program.types.intern(TypeDesc::Ptr(Box::new(richest)));
        let loader_ptr = program.symbols.alloc_var_id();
        program.symbols.add_variable(Variable {
            id: loader_ptr,
            name: "loader".into(),
            type_id: through_ptr,
            storage: StorageClass::Global,
            fn_id: None,
            param_index: None,
            span: Span::new(trace_ir::FileId(0), 1, 1),
            is_pointer: true,
            is_defined: true,
            is_weak: false,
            target: None,
            is_namespaced: false,
            qualified_name: None,
            c_linkage: false,
        });
        // The key follows the layout's own member name, not the caller's:
        // asked first with a wrong name, the member still maps by its own.
        let mut fresh = Pag::build(&program);
        let ty = program.types.get(snapshot).layout.fields[&FieldId(1)].type_id;
        let misnamed =
            fresh.ensure_field_summary_loc(&program, snapshot, None, FieldId(1), ty, "misnamed");
        let by_ptr = fresh
            .ensure_field_summary_for_var(&program, loader_ptr, FieldId(2))
            .unwrap();
        assert_eq!(misnamed, by_ptr);

        let mut pag = Pag::build(&program);
        let field = |pag: &mut Pag, id: u32, name: &str| {
            let ty = program.types.get(snapshot).layout.fields[&FieldId(id)].type_id;
            pag.ensure_field_summary_loc(&program, snapshot, None, FieldId(id), ty, name)
        };
        let stored = field(&mut pag, 1, "GetDriver");
        let loaded = pag
            .ensure_field_summary_for_var(&program, loader_ptr, FieldId(2))
            .unwrap();
        assert_eq!(stored, loaded, "GetDriver must share a summary");
        let own = field(&mut pag, 2, "only_here");
        assert_ne!(own, loaded, "only_here is not GetDriver");
        assert_ne!(
            own,
            pag.ensure_field_summary_for_var(&program, loader_ptr, FieldId(3))
                .unwrap()
        );
    }

    fn global_of(program: &mut Program, name: &str, type_id: TypeId) -> VarId {
        let id = program.symbols.alloc_var_id();
        program.symbols.add_variable(Variable {
            id,
            name: name.into(),
            type_id,
            storage: StorageClass::Global,
            fn_id: None,
            param_index: None,
            span: Span::new(trace_ir::FileId(0), 1, 1),
            is_pointer: false,
            is_defined: true,
            is_weak: false,
            target: None,
            is_namespaced: false,
            qualified_name: None,
            c_linkage: false,
        });
        id
    }

    /// Review of #127: a variable declared with a forward-declared tag
    /// (`struct Svc; extern struct Svc g;`, interned before any unit defines
    /// `Svc`) resolves its fields against the tag's definition, as lowering
    /// numbers them (`struct_type_for_var`).
    #[test]
    fn a_forward_declared_tag_variable_resolves_fields_against_its_definition() {
        let mut program = Program::new(".".into());
        let empty = program.types.intern(TypeDesc::Struct {
            name: "Svc".into(),
            fields: Vec::new(),
        });
        program.types.intern(TypeDesc::Struct {
            name: "Svc".into(),
            fields: vec![("cb".into(), TypeDesc::Int)],
        });
        let g = global_of(&mut program, "g", empty);
        let mut pag = Pag::build(&program);
        let loc = pag.ensure_var_loc(&program, g).unwrap();
        let concrete = pag.ensure_field_loc(&program, loc, FieldId(0)).unwrap();
        let summary = pag
            .ensure_field_summary_for_var(&program, g, FieldId(0))
            .expect("a forward-declared variable must resolve its field summary");
        assert_eq!(pag.field_loc_to_summary[&concrete], summary);
    }

    /// Review of #127: a scalar, however many pointers deep, has no struct.
    #[test]
    fn scalars_have_no_struct_type() {
        let mut program = Program::new(".".into());
        let mut desc = TypeDesc::Int;
        let mut deep = program.types.int();
        for _ in 0..4 {
            desc = TypeDesc::Ptr(Box::new(desc));
            deep = program.types.intern(desc.clone());
        }
        let p = global_of(&mut program, "p", deep);
        let mut pag = Pag::build(&program);
        let loc = pag.ensure_var_loc(&program, p).unwrap();
        assert_eq!(struct_type_for_loc(&pag, &program, loc), None);
    }

    #[test]
    fn empty_field_name_does_not_redirect_to_an_anonymous_member() {
        let mut program = Program::new(".".into());
        let type_id = program.types.intern(TypeDesc::Struct {
            name: "Fields".into(),
            fields: vec![
                (String::new(), TypeDesc::Int),
                ("named".into(), TypeDesc::Int),
            ],
        });
        let var = program.symbols.alloc_var_id();
        program.symbols.add_variable(Variable {
            is_defined: false,
            is_weak: false,
            target: None,
            is_namespaced: false,
            qualified_name: None,
            c_linkage: false,
            id: var,
            name: "object".into(),
            type_id,
            storage: StorageClass::Global,
            fn_id: None,
            param_index: None,
            span: Span::new(trace_ir::FileId(0), 1, 1),
            is_pointer: false,
        });
        let mut pag = Pag::default();
        let positional = pag
            .ensure_field_summary_for_var(&program, var, FieldId(1))
            .unwrap();
        assert_eq!(
            pag.ensure_field_summary_for_var_named(&program, var, FieldId(1), Some("")),
            Some(positional)
        );
        assert_eq!(
            pag.ensure_field_summary_for_var_named(&program, var, FieldId(2), Some("")),
            None
        );
    }
}
