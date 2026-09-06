use crate::constraints::{AbstractLocation, Constraint, ConstraintKind, LocKind};
use crate::ipc::detect_ipc_pairs;
use crate::summaries::{Effect, FnModelSet};
use indexmap::IndexMap;
use rustc_hash::{FxHashMap, FxHashSet};
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
    pub var_node: IndexMap<VarId, PagNodeId>,
    pub loc_node: IndexMap<LocId, PagNodeId>,
    pub call_targets: IndexMap<trace_ir::CallSiteId, PagNodeId>,
    pub fn_locations: IndexMap<FnId, LocId>,
    pub var_location: IndexMap<VarId, LocId>,
    /// Interned `StringConst` locations keyed by literal contents.
    pub string_locs: FxHashMap<String, LocId>,
    /// Field abstract locations keyed by (parent object location, field id).
    pub field_loc: IndexMap<(LocId, FieldId), LocId>,
    /// Nesting depth of each synthesized field location (var-rooted = 0
    /// children start at 1). Bounds recursive `obj->next->next->...`
    /// location synthesis once interprocedural flow reaches chained structs.
    pub field_depth: FxHashMap<LocId, u8>,
    /// Per-(struct type, field) summary location for instance-insensitive field flow.
    pub field_summary: IndexMap<(trace_ir::TypeId, FieldId), LocId>,
    pub field_loc_to_summary: IndexMap<LocId, LocId>,
    /// Fn locations parked into an array var by `ArrayFnMember` inits
    /// (`{ {.., Fn}, .. }`); reachable through any element field load.
    pub array_fn_members: FxHashMap<VarId, Vec<LocId>>,
    /// Maps callee_var load-var to the PAG node that should receive the
    /// return value of indirect calls whose function pointer is that var.
    pub indirect_return_dst: FxHashMap<VarId, PagNodeId>,
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

    fn build_variables(&mut self, program: &Program) {
        for var in &program.symbols.variables {
            self.var_node_id(var.id);
            if !matches!(
                var.storage,
                StorageClass::Global | StorageClass::FileStatic | StorageClass::FnStatic
            ) {
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
            self.var_location.insert(var.id, loc_id);
        }
    }

    pub fn ensure_var_loc(&mut self, program: &Program, var: VarId) -> Option<LocId> {
        if let Some(&loc) = self.var_location.get(&var) {
            return Some(loc);
        }
        let v = program.symbols.variable_by_id(var)?;
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
        self.var_location.insert(var, loc_id);
        Some(loc_id)
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
            }
        }
        srcs
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
        self.field_loc.insert((parent_loc, field), loc_id);
        loc_id
    }

    /// Instance-sensitive child location for `parent.field`. Past
    /// [`FIELD_LOC_DEPTH_CAP`], further nesting is folded into the
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
        let field_layout = program.types.get(parent_type).layout.fields.get(&field)?;
        if self.field_depth.get(&parent_loc).copied().unwrap_or(0) >= FIELD_LOC_DEPTH_CAP {
            return Some(self.ensure_field_summary_loc(
                program,
                parent_type,
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
        field: FieldId,
        field_type: trace_ir::TypeId,
        name: &str,
    ) -> LocId {
        if let Some(&loc) = self.field_summary.get(&(struct_type, field)) {
            return loc;
        }
        let struct_name = match &program.types.get(struct_type).desc {
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
        self.field_summary.insert((struct_type, field), loc_id);
        loc_id
    }

    pub fn summary_for_field_loc(&self, field_loc: LocId) -> Option<LocId> {
        self.field_loc_to_summary.get(&field_loc).copied()
    }

    /// Instance-insensitive summary location for `(struct type of var, field)`.
    pub fn ensure_field_summary_for_var(
        &mut self,
        program: &Program,
        var: trace_ir::VarId,
        field: FieldId,
    ) -> Option<LocId> {
        let v = program.symbols.variable_by_id(var)?;
        let struct_type = struct_type_from_type_id(program, v.type_id)?;
        let field_layout = program.types.get(struct_type).layout.fields.get(&field)?;
        Some(self.ensure_field_summary_loc(
            program,
            struct_type,
            field,
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
        match &program.types.get(layout.type_id).desc {
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
                FlowConstraint::CallReturn { dst, callee_name } => {
                    let dst_n = self.var_node_id(*dst);
                    let file = program
                        .symbols
                        .variable(*dst)
                        .fn_id
                        .map(|f| program.symbols.function(f).file);
                    // May-approximation: a merged name may bind to the
                    // query file's `static` def, the external def, or both.
                    let mut visited = FxHashSet::default();
                    let candidates: Vec<_> = program
                        .symbols
                        .resolve_function_candidates(callee_name, file);
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
                            self.apply_return_model(dst_n, model, params.as_deref());
                        }
                    }
                }
                FlowConstraint::CallReturnIndirect { dst, callee_var } => {
                    // Record the return destination so the solver can expand
                    // return flows when it resolves indirect call targets.
                    // INVARIANT: each callee_var maps to exactly one dst —
                    // lowering must not reuse callee load vars across sites.
                    let dst_n = self.var_node_id(*dst);
                    self.indirect_return_dst.insert(*callee_var, dst_n);
                }
                FlowConstraint::NewHeap { dst } => {
                    let dst_n = self.var_node_id(*dst);
                    let var = program.symbols.variable(*dst);
                    let type_id = var.type_id;
                    let loc = self.alloc_heap_loc("new heap".to_string());
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
            .find(|t| matches!(t.desc, trace_ir::TypeDesc::Char))
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
                            let file = program.symbols.function(callee).file;
                            let inner_candidates = program
                                .symbols
                                .resolve_function_candidates(&callee_name, Some(file));
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
                                    self.apply_return_model(dst, model, params.as_deref());
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
            } else if program.symbols.resolve_function(&cs.callee_name).is_none() {
                let call_target = self.call_target_node(cs.id);
                if let Some(v) = lookup_var_in_fn(&fn_vars, program, &cs.callee_name, cs.caller) {
                    let var_node = self.var_node_id(v);
                    self.add_load(call_target, var_node);
                }
            }
        }
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
        let mut type_id = program.symbols.variable_by_id(var)?.type_id;
        for _ in 0..4 {
            match &program.types.get(type_id).desc {
                trace_ir::TypeDesc::Ptr(inner) => {
                    type_id = match inner.as_ref() {
                        trace_ir::TypeDesc::Struct { name, .. } => program
                            .types
                            .type_id_by_tag(name, trace_ir::TypeKind::Struct)
                            .unwrap_or_else(|| program.types.resolve_type_id(inner)),
                        trace_ir::TypeDesc::Union { name, .. } => program
                            .types
                            .type_id_by_tag(name, trace_ir::TypeKind::Union)
                            .unwrap_or_else(|| program.types.resolve_type_id(inner)),
                        _ => program.types.resolve_type_id(inner),
                    };
                }
                // Arrays of structs: resolve fields against the element type.
                trace_ir::TypeDesc::Array { elem, .. } => {
                    type_id = program.types.resolve_type_id(inner_or_elem(elem));
                }
                trace_ir::TypeDesc::Struct { .. } | trace_ir::TypeDesc::Union { .. } => {
                    return Some(type_id);
                }
                _ => return Some(type_id),
            }
        }
        return Some(type_id);
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
        match &program.types.get(type_id).desc {
            trace_ir::TypeDesc::Ptr(inner) => {
                type_id = program.types.resolve_type_id(inner);
            }
            _ => break,
        }
    }
    Some(type_id)
}

fn struct_type_from_type_id(
    program: &Program,
    mut type_id: trace_ir::TypeId,
) -> Option<trace_ir::TypeId> {
    for _ in 0..6 {
        match &program.types.get(type_id).desc {
            trace_ir::TypeDesc::Ptr(inner) => {
                type_id = match inner.as_ref() {
                    trace_ir::TypeDesc::Struct { name, .. } => program
                        .types
                        .type_id_by_tag(name, trace_ir::TypeKind::Struct)
                        .unwrap_or_else(|| program.types.resolve_type_id(inner)),
                    trace_ir::TypeDesc::Union { name, .. } => program
                        .types
                        .type_id_by_tag(name, trace_ir::TypeKind::Union)
                        .unwrap_or_else(|| program.types.resolve_type_id(inner)),
                    _ => program.types.resolve_type_id(inner),
                };
            }
            // Arrays of structs: resolve fields against the element type.
            trace_ir::TypeDesc::Array { elem, .. } => {
                type_id = program.types.resolve_type_id(inner_or_elem(elem));
            }
            trace_ir::TypeDesc::Struct { .. } | trace_ir::TypeDesc::Union { .. } => {
                return Some(type_id);
            }
            _ => return None,
        }
    }
    None
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
        .or_else(|| program.symbols.global_by_name.get(name).copied())
}
