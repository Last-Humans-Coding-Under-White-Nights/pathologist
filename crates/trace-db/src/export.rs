use crate::schema::{INDEXES_V8, PROVENANCE_INDEXES_V8, SCHEMA_VERSION, TABLES_V8};
use crate::TextPool;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use rustc_hash::{FxHashMap, FxHashSet};
use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use trace_analysis::{AnalysisResult, ConstraintKind, LocKind, Pag, PagNodeKind};
use trace_ir::PagNodeId;
use trace_ir::{FnId, Linkage, Program, StorageClass, TypeDesc, VarId};

const INDIRECT_RETURN_CALLEES_SQL: &str = "SELECT DISTINCT e.callee_fn_id
    FROM call_sites cs JOIN call_edges e ON e.call_site_id=cs.id
    WHERE cs.callee_var=?1 AND cs.return_dst=?2 ORDER BY e.callee_fn_id";

pub struct ExportOptions {
    pub output: PathBuf,
    /// Identity of the binary producing the database. `trace` passes its
    /// full build identity (version, revision, dirty state, build date);
    /// `minimal` falls back to this crate's package version for library
    /// callers that have no build identity of their own.
    pub trace_version: String,
    /// Export points-to debug table (requires analysis with `retain_points_to`).
    pub include_points_to: bool,
    /// Export types, all variables, and PAG locations (slower, larger DB).
    pub full_detail: bool,
    /// Function-model config files used by the analysis run (metadata only).
    pub model_files: Vec<String>,
}

impl ExportOptions {
    pub fn minimal(output: PathBuf) -> Self {
        Self {
            output,
            trace_version: env!("CARGO_PKG_VERSION").to_owned(),
            include_points_to: false,
            full_detail: false,
            model_files: Vec::new(),
        }
    }
}

pub fn export_to_sqlite(
    program: &Program,
    pag: &Pag,
    analysis: &AnalysisResult,
    opts: &ExportOptions,
) -> Result<()> {
    let temp = opts.output.with_extension("db.tmp");
    if let Some(parent) = temp.parent() {
        fs::create_dir_all(parent)?;
    }
    if temp.exists() {
        fs::remove_file(&temp)?;
    }
    {
        let conn = Connection::open(&temp)?;
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF; PRAGMA synchronous = OFF; PRAGMA journal_mode = MEMORY;",
        )?;
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        conn.execute_batch(TABLES_V8)?;
        let mut strings = TextPool::new(&conn)?;
        if program.call_sequencing {
            strings.seed_sequencing()?;
        }

        let options_json = serde_json::json!({
            "test_partition": {
                "enabled": program.symbols.test_partition().enabled(),
                "directories": program.symbols.test_partition().directories(),
            },
            "include_paths": program.include_paths,
            "defines": program.defines,
            "dep_roots": program.dep_roots(),
            "ignored_macros": program.ignored_macros,
            "include_points_to": opts.include_points_to,
            "full_detail": opts.full_detail,
            "model_files": opts.model_files,
            "explore": program.explore,
            "call_sequencing": crate::SequencingMarker::analysed(program.call_sequencing).to_json(),
            "explore_budget": program.explore_budget,
            "variants_merged": program.variants_merged,
            "solver_partial": !analysis.solve.converged,
            "solver_pops": analysis.solve.pops,
            "solve_budget_pops": analysis.solve.budget_pops,
            "solve_budget_secs": analysis.solve.budget_secs,
        })
        .to_string();

        conn.execute(
            "INSERT INTO analysis_run (trace_version, schema_version, target_root, created_at, options_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                opts.trace_version,
                SCHEMA_VERSION,
                program.root.display().to_string(),
                chrono_lite_now(),
                options_json
            ],
        )?;

        export_files(&conn, program)?;
        export_link_targets(&conn, program)?;
        export_functions(&conn, program)?;
        export_flow_parameters(&conn, program, &mut strings)?;
        export_call_sites_filtered(&conn, program, analysis, &mut strings)?;
        crate::write_linking_facts(
            &conn,
            &trace_ir::LinkingFacts::collect(program, pag.ipc_enabled),
            &mut strings,
        )?;
        export_cfg(&conn, program.cfgs.values().flatten(), &mut strings)?;
        export_call_edges(&conn, analysis, &mut strings)?;
        conn.execute_batch(PROVENANCE_INDEXES_V8)?;
        if opts.full_detail {
            export_types(&conn, program)?;
            export_variables(&conn, program, &mut strings)?;
            export_locations(&conn, pag)?;
        } else {
            export_flow_and_arg_flow_vars(&conn, program, pag, analysis, &mut strings)?;
        }
        export_arg_flow(&conn, analysis)?;
        export_execution_contexts(&conn, analysis)?;
        export_flow_graph(&conn, program, pag, analysis, &mut strings)?;
        if opts.include_points_to {
            export_points_to(&conn, pag, analysis)?;
        }
        export_diagnostics(&conn, program, analysis)?;
        crate::record_database_identity(&conn)?;
        conn.execute_batch(INDEXES_V8)?;
        conn.execute_batch("COMMIT;")?;
    }

    if opts.output.exists() {
        fs::remove_file(&opts.output)?;
    }
    fs::rename(&temp, &opts.output)?;
    Ok(())
}

fn chrono_lite_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", dur.as_secs())
}

fn export_files(conn: &Connection, program: &Program) -> Result<()> {
    let mut stmt = conn
        .prepare_cached("INSERT INTO files (id, path, sha256, is_dep) VALUES (?1, ?2, ?3, ?4)")?;
    for file in &program.symbols.files {
        stmt.execute(params![
            file.id.0,
            file.path.display().to_string(),
            "",
            file.is_dep as i32
        ])?;
    }
    Ok(())
}

fn export_link_targets(conn: &Connection, program: &Program) -> Result<()> {
    let mut targets =
        conn.prepare_cached("INSERT INTO link_targets (id, name, output) VALUES (?1, ?2, ?3)")?;
    // Insert the catalog first so forward dependency references are valid
    // even when this helper is used with foreign-key enforcement enabled.
    for target in &program.link_targets {
        targets.execute(params![
            target.id.0,
            target.name,
            target.output.display().to_string()
        ])?;
    }
    let mut sources =
        conn.prepare_cached("INSERT INTO target_sources (target_id, file_id) VALUES (?1, ?2)")?;
    let mut dependencies = conn.prepare_cached(
        "INSERT INTO target_dependencies (target_id, dependency_id) VALUES (?1, ?2)",
    )?;
    for target in &program.link_targets {
        for source in &target.sources {
            sources.execute(params![target.id.0, source.0])?;
        }
        for dependency in &target.dependencies {
            dependencies.execute(params![target.id.0, dependency.0])?;
        }
    }
    Ok(())
}

fn export_types(conn: &Connection, program: &Program) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO types (id, kind, name, size, layout_json) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for ty in program.types.all() {
        let kind = match ty.desc.as_ref() {
            TypeDesc::Void => "void",
            TypeDesc::Char => "char",
            TypeDesc::Bool => "bool",
            TypeDesc::Short => "short",
            TypeDesc::Int => "int",
            TypeDesc::Long => "long",
            TypeDesc::LongLong => "long long",
            TypeDesc::Float => "float",
            TypeDesc::Double => "double",
            TypeDesc::SizeT => "size_t",
            TypeDesc::Unknown => "unknown",
            TypeDesc::Ptr(_) => "ptr",
            TypeDesc::Array { .. } => "array",
            TypeDesc::Struct { .. } => "struct",
            TypeDesc::Union { .. } => "union",
            TypeDesc::FnPtr { .. } => "fn_ptr",
        };
        let name = type_name(&ty.desc);
        let layout_json = serde_json::to_string(&ty.layout)?;
        stmt.execute(params![ty.id.0, kind, name, ty.size, layout_json])?;
    }
    Ok(())
}

fn type_name(desc: &TypeDesc) -> String {
    match desc {
        TypeDesc::Struct { name, .. } | TypeDesc::Union { name, .. } => name.clone(),
        TypeDesc::Ptr(inner) => format!("{}*", type_name(inner)),
        TypeDesc::Array { elem, size } => {
            format!(
                "{}[{}]",
                type_name(elem),
                size.map(|s| s.to_string()).unwrap_or_default()
            )
        }
        TypeDesc::FnPtr { .. } => "fn_ptr".into(),
        TypeDesc::Void => "void".into(),
        TypeDesc::Char => "char".into(),
        TypeDesc::Bool => "bool".into(),
        TypeDesc::Short => "short".into(),
        TypeDesc::Int => "int".into(),
        TypeDesc::Long => "long".into(),
        TypeDesc::LongLong => "long long".into(),
        TypeDesc::Float => "float".into(),
        TypeDesc::Double => "double".into(),
        TypeDesc::SizeT => "size_t".into(),
        TypeDesc::Unknown => "unknown".into(),
    }
}

const INSERT_VARIABLE: &str = "INSERT INTO variables_data (id, name_id, kind, fn_id, type_id, file_id, line, col, is_weak, target_id, is_synthetic) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";

fn export_flow_parameters(
    conn: &Connection,
    program: &Program,
    strings: &mut TextPool<'_>,
) -> Result<()> {
    let mut stmt =
        conn.prepare_cached("INSERT INTO flow_parameters_data VALUES (?1, ?2, ?3, ?4, ?5)")?;
    for func in &program.symbols.functions {
        // Match the solver's argument numbering, including an implicit receiver.
        for (index, &id) in func.params.iter().enumerate() {
            let var = program.symbols.variable(id);
            let ty = program.types.get(var.type_id);
            stmt.execute(params![
                func.id.0,
                index,
                id.0,
                strings.intern(&var.name)?,
                strings.intern(&param_type_name(&ty.desc))?
            ])?;
        }
    }
    Ok(())
}

fn export_variables(
    conn: &Connection,
    program: &Program,
    strings: &mut TextPool<'_>,
) -> Result<()> {
    let mut stmt = conn.prepare_cached(INSERT_VARIABLE)?;
    for var in &program.symbols.variables {
        export_one_variable(&mut stmt, var, strings)?;
    }
    Ok(())
}

fn export_flow_and_arg_flow_vars(
    conn: &Connection,
    program: &Program,
    pag: &Pag,
    analysis: &AnalysisResult,
    strings: &mut TextPool<'_>,
) -> Result<()> {
    let mut needed: FxHashSet<VarId> = FxHashSet::default();
    // Every global and static, with or without a PAG node: inspect finds a
    // symbol by its declaration, and one no fact names must be found as
    // itself (and reported flowless), never as a neighbour.
    needed.extend(
        program
            .symbols
            .variables
            .iter()
            .filter(|v| {
                matches!(
                    v.storage,
                    StorageClass::Global | StorageClass::FileStatic | StorageClass::FnStatic
                )
            })
            .map(|v| v.id),
    );
    for edge in &analysis.arg_flow_edges {
        if let Some(v) = edge.actual_var {
            needed.insert(v);
        }
        needed.insert(edge.formal);
    }
    // The queue, handler or pool a context was submitted on.
    needed.extend(
        analysis
            .execution_contexts
            .iter()
            .filter_map(|c| c.start.as_ref()?.receiver),
    );
    // The flow graph must be self-contained for inspect queries: every
    // variable with a PAG node is exported, not just arg-flow participants.
    for node in &pag.nodes {
        if let PagNodeKind::Var(v) = node.kind {
            needed.insert(v);
        }
    }
    for loc in &pag.locations {
        if let Some(v) = loc.var {
            needed.insert(v);
        }
    }
    if needed.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare_cached(INSERT_VARIABLE)?;
    for var in &program.symbols.variables {
        if needed.contains(&var.id) {
            export_one_variable(&mut stmt, var, strings)?;
        }
    }
    Ok(())
}

/// Export the value-flow graph derived from the post-solve PAG. Edges are
/// the constraint set including parameter copies wired dynamically during
/// solving; implicit `points_to` edges connect each global/static var node
/// to its storage location so traversal crosses memory cells. Interprocedural
/// argument flow is added as `call_arg` edges (covering parameters the solver
/// did not wire as persistent copies, e.g. scalar buffer pointers).
fn export_flow_graph(
    conn: &Connection,
    program: &Program,
    pag: &Pag,
    analysis: &AnalysisResult,
    strings: &mut TextPool<'_>,
) -> Result<()> {
    let mut nodes = conn.prepare_cached(
        "INSERT INTO flow_nodes_data (id, kind_id, label_id, detail_id, var_id, fn_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut edges = conn.prepare_cached(
        "INSERT INTO flow_edges_data (id, src_node, dst_node, kind_id) VALUES (?1, ?2, ?3, ?4)",
    )?;

    for node in &pag.nodes {
        let (kind, label, detail, var_id, fn_id) = match node.kind {
            PagNodeKind::Var(v) => {
                let fn_id = program.symbols.variable_by_id(v).and_then(|var| var.fn_id);
                ("var", String::new(), String::new(), Some(v), fn_id)
            }
            PagNodeKind::NullPointer => (
                "constant",
                "nullptr".to_owned(),
                "null_pointer".to_owned(),
                None,
                None,
            ),
            PagNodeKind::Loc(loc_id) => {
                let loc = &pag.locations[loc_id.0 as usize];
                let kind_str = crate::inspect::loc_kind_schema_str(loc.kind);
                let label = match (loc.kind, loc.var) {
                    (LocKind::Function, _) => format!("fn:{}", loc.desc),
                    (LocKind::StringLit, _) => format!("string:{}", loc.desc),
                    (_, Some(v)) => match program.symbols.variable_by_id(v) {
                        Some(var) => format!("{} of {}", loc.desc, var.name),
                        None => loc.desc.clone(),
                    },
                    _ => loc.desc.clone(),
                };
                ("loc", label, kind_str.to_string(), loc.var, loc.fn_id)
            }
            PagNodeKind::CallTarget(cs) => match program.symbols.call_site_by_id(cs) {
                Some(site) => (
                    "call_target",
                    site.callee_name.to_string(),
                    format!("call @{}", site.span.line),
                    None,
                    Some(site.caller),
                ),
                None => (
                    "call_target",
                    format!("cs{}", cs.0),
                    String::new(),
                    None,
                    None,
                ),
            },
        };
        nodes.execute(params![
            node.id.0,
            strings.intern(kind)?,
            strings.intern(&label)?,
            strings.intern(&detail)?,
            var_id.map(|v| v.0),
            fn_id.map(|f| f.0)
        ])?;
    }

    let mut target_sites =
        conn.prepare_cached("UPDATE flow_nodes_data SET call_site_id=?2 WHERE id=?1")?;
    for node in &pag.nodes {
        if let PagNodeKind::CallTarget(cs) = node.kind {
            target_sites.execute(params![node.id.0, cs.0])?;
        }
    }
    let mut field_locations =
        conn.prepare_cached("INSERT INTO flow_field_locations VALUES (?1,?2,?3)")?;
    for ((parent, _), child) in &pag.field_loc {
        if let (Some(parent_node), Some(child_node)) =
            (pag.loc_node.get(parent), pag.loc_node.get(child))
        {
            field_locations.execute(params![
                child_node.0,
                parent_node.0,
                pag.locations[child.0 as usize].desc
            ])?;
        }
    }
    export_flow_provenance(conn, program, pag, analysis, strings)?;
    let mut edge_rows: Vec<(u32, u32, &'static str)> = Vec::new();
    for c in &pag.constraints {
        let kind = match c.kind {
            ConstraintKind::Copy => "copy",
            ConstraintKind::AddrOf => "addr_of",
            ConstraintKind::Load => "load",
            ConstraintKind::Store => "store",
            ConstraintKind::Gep => "gep",
            ConstraintKind::Dlsym => "dlsym",
            ConstraintKind::UnwrapPointer => "unwrap",
        };
        edge_rows.push((c.src.0, c.dst.0, kind));
    }
    // Implicit var → storage-location edges: storage connectivity, so a
    // dataflow walk goes from a value into its location and on through `&x`.
    // Drawn for every variable with a location; which of them the solver
    // seeds with their own location is a separate question (`seeds_own_location`).
    for (&var, &loc) in &pag.var_location {
        if let Some(&node) = pag.loc_node.get(&loc) {
            if let Some(&var_node) = pag.var_node.get(&var) {
                edge_rows.push((var_node.0, node.0, "points_to"));
            }
        }
    }
    // Interprocedural argument flow: actual var/function node → formal var
    // node. Only added where the constraint graph does not already connect
    // the pair (the solver wires persistent copies for pointee-carrying
    // params), so traversal sees each hop once.
    let mut wired_pairs: FxHashSet<(u32, u32)> = FxHashSet::default();
    for c in &pag.constraints {
        if matches!(
            c.kind,
            ConstraintKind::Copy | ConstraintKind::Load | ConstraintKind::Gep
        ) {
            wired_pairs.insert((c.src.0, c.dst.0));
        }
    }
    let mut calls = conn.prepare_cached("INSERT INTO flow_calls VALUES (?1, ?2, ?3, ?4)")?;
    for e in &analysis.arg_flow_edges {
        let Some(formal_node) = pag.var_node.get(&e.formal) else {
            continue;
        };
        match (e.actual_var, e.actual_fn) {
            (Some(actual), _) => {
                // The row names the argument's object (`x` for `f(&x)`); the
                // value the formal receives is the call site's actual, the
                // temporary holding `&x`, which the graph reaches from `x`'s
                // location through `addr_of`.
                let passed = program
                    .symbols
                    .call_site_by_id(e.call_site)
                    .filter(|cs| cs.addr_of_args().contains(&e.arg_index))
                    .and_then(|cs| cs.var_args.iter().find(|(i, _)| *i == e.arg_index))
                    .map_or(actual, |(_, temp)| *temp);
                let Some(actual_node) = pag.var_node.get(&passed) else {
                    continue;
                };
                calls.execute(params![
                    actual_node.0,
                    formal_node.0,
                    e.call_site.0,
                    e.arg_index
                ])?;
                if wired_pairs.contains(&(actual_node.0, formal_node.0)) {
                    continue;
                }
                edge_rows.push((actual_node.0, formal_node.0, "call_arg"));
            }
            (None, Some(actual_fn)) => {
                if let Some(&fn_loc) = pag.fn_locations.get(&actual_fn) {
                    if let Some(&fn_node) = pag.loc_node.get(&fn_loc) {
                        calls.execute(params![
                            fn_node.0,
                            formal_node.0,
                            e.call_site.0,
                            e.arg_index
                        ])?;
                        edge_rows.push((fn_node.0, formal_node.0, "call_arg"));
                    }
                }
            }
            (None, None) => {}
        }
    }
    // Terminator events (`clears` model effects): a synthetic terminal node
    // per (call site, parameter) with an edge from the cleared actual, so
    // dataflow walks show where value chains are zeroed.
    let mut next_node_id = pag.nodes.len() as u64;
    let mut seen_terms: FxHashSet<(trace_ir::CallSiteId, u32)> = FxHashSet::default();
    for &(cs_id, param) in &analysis.terminator_events {
        if !seen_terms.insert((cs_id, param)) {
            continue;
        }
        let Some(site) = program.symbols.call_site_by_id(cs_id) else {
            continue;
        };
        let Some(actual) = pag.argument_var(site, param) else {
            continue;
        };
        let Some(&actual_node) = pag.var_node.get(&actual) else {
            continue;
        };
        let term_node = next_node_id;
        next_node_id += 1;
        nodes.execute(params![
            term_node,
            strings.intern("terminator")?,
            strings.intern(&format!("{} clears arg{param}", site.callee_name))?,
            strings.intern(&format!(
                "call @{} in {}",
                site.span.line,
                program.symbols.function(site.caller).name
            ))?,
            Option::<i64>::None,
            Some(site.caller.0 as i64),
        ])?;
        target_sites.execute(params![term_node, cs_id.0])?;
        edge_rows.push((actual_node.0, term_node as u32, "terminates"));
    }
    edge_rows.sort_unstable();
    edge_rows.dedup();
    for (i, (src, dst, kind)) in edge_rows.iter().enumerate() {
        edges.execute(params![i as i64 + 1, src, dst, strings.intern(kind)?])?;
    }
    export_memory_accesses(conn, pag, analysis, &edge_rows)
}

/// The `flow_memory_access` rows: per load or store, by its `flow_edges` id
/// (its rank in `edge_rows`, which is sorted), each cell it reads or writes,
/// or one cell-less row when none is recorded (docs/ANALYSIS.md, "Memory
/// access edges").
fn export_memory_accesses(
    conn: &Connection,
    pag: &Pag,
    analysis: &AnalysisResult,
    edge_rows: &[(u32, u32, &'static str)],
) -> Result<()> {
    let mut rows: Vec<(i64, Option<u32>)> = Vec::with_capacity(analysis.memory_accesses.len());
    for access in &analysis.memory_accesses {
        let c = &pag.constraints[access.constraint.index()];
        let kind = match c.kind {
            ConstraintKind::Load => "load",
            ConstraintKind::Store => "store",
            _ => continue,
        };
        let Ok(rank) = edge_rows.binary_search(&(c.src.0, c.dst.0, kind)) else {
            continue;
        };
        let cell = access
            .loc
            .and_then(|loc| pag.loc_node.get(&loc))
            .map(|n| n.0);
        rows.push((rank as i64 + 1, cell));
    }
    rows.sort_unstable();
    rows.dedup();
    let mut insert = conn.prepare_cached("INSERT INTO flow_memory_access VALUES (?1, ?2)")?;
    for (i, &(edge, cell)) in rows.iter().enumerate() {
        // A cell-less row sorts first; an access with a recorded cell keeps
        // only its cells.
        if cell.is_none() && rows.get(i + 1).is_some_and(|&(next, _)| next == edge) {
            continue;
        }
        insert.execute(params![edge, cell])?;
    }
    Ok(())
}

// Keep provenance indexes local to their final consumer: edge export does not
// need them and builds its own row and argument-wiring buffers.
fn export_flow_provenance(
    conn: &Connection,
    program: &Program,
    pag: &Pag,
    analysis: &AnalysisResult,
    strings: &mut TextPool<'_>,
) -> Result<()> {
    let mut origins = conn
        .prepare_cached("INSERT INTO flow_origins_data VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?;
    let field_destinations: FxHashSet<_> = program
        .flow
        .iter()
        .filter_map(|flow| match flow {
            trace_ir::FlowConstraint::GepField { dst, .. } => Some(*dst),
            _ => None,
        })
        .collect();
    // Origins are already unioned under constraint identity. Visit each
    // constraint once in first-occurrence order, rather than replaying its
    // complete site list for every repeated lowering fact.
    let unique_flows: Vec<_> = {
        let mut seen_flows = FxHashSet::default();
        program
            .flow
            .iter()
            .filter(|flow| seen_flows.insert(*flow))
            .collect()
    };
    let mut field_access =
        conn.prepare_cached("INSERT OR IGNORE INTO flow_field_access VALUES (?1, ?2, ?3)")?;
    for constraint in &pag.constraints {
        if constraint.kind == ConstraintKind::Gep {
            if let Some(name) = constraint.field_name() {
                field_access.execute(params![constraint.src.0, constraint.dst.0, name])?;
            }
        }
    }
    // The heap object a `new` allocates into `dst`: the source of the
    // `addr_of` the PAG built for its `NewHeap` fact, which the fact itself
    // does not name.
    let new_heap = |dst: &PagNodeId| -> Option<&PagNodeId> {
        pag.indices.addr_of_dst.get(dst)?.iter().find_map(|&i| {
            let src = &pag.constraints[i].src;
            match pag.nodes[src.0 as usize].kind {
                PagNodeKind::Loc(loc) if pag.locations[loc.0 as usize].kind == LocKind::Heap => {
                    Some(src)
                }
                _ => None,
            }
        })
    };
    for flow in &unique_flows {
        use trace_ir::FlowConstraint as F;
        let Some(sites) = program.flow_origins.get(*flow) else {
            continue;
        };
        let dst = flow.vars().next().expect("constraint destination");
        let Some(d) = pag.var_node.get(&dst) else {
            continue;
        };
        let (src, kind) = match flow {
            F::Copy { src, .. } => (pag.var_node.get(src), "copy"),
            F::Store { src, .. } => (pag.var_node.get(src), "store"),
            F::Load { src, .. } => (pag.var_node.get(src), "load"),
            F::GepField { base, .. } => (pag.var_node.get(base), "gep"),
            F::UnwrapPointer { src, .. } => (pag.var_node.get(src), "unwrap"),
            F::AddrOfVar { src, .. } => (
                pag.var_location.get(src).and_then(|l| pag.loc_node.get(l)),
                "addr_of",
            ),
            F::AddrOfFn { callee, .. } | F::ArrayFnMember { callee, .. } => (
                pag.fn_locations
                    .get(callee)
                    .and_then(|l| pag.loc_node.get(l)),
                "addr_of",
            ),
            F::NullPointer { .. } => (pag.null_node.as_ref(), "copy"),
            F::NewHeap { .. } => (new_heap(d), "addr_of"),
            F::StringConst { value, .. } => (
                pag.string_locs.get(value).and_then(|l| pag.loc_node.get(l)),
                "addr_of",
            ),
            _ => continue,
        };
        let Some(s) = src else {
            continue;
        };
        let operation = if kind == "store" && field_destinations.contains(&dst) {
            "write field"
        } else {
            kind
        };
        for (span, expression) in sites {
            origins.execute(params![
                s.0,
                d.0,
                strings.intern(kind)?,
                span.file.0,
                span.line,
                span.col,
                strings.intern(expression.as_ref())?,
                strings.intern(operation)?
            ])?;
        }
    }
    let return_destinations: FxHashSet<_> = unique_flows
        .iter()
        .filter_map(|flow| match flow {
            trace_ir::FlowConstraint::CallReturn { dst, .. }
            | trace_ir::FlowConstraint::CallReturnIndirect { dst, .. } => {
                pag.var_node.get(dst).copied()
            }
            _ => None,
        })
        .collect();
    let mut incoming = std::collections::BTreeMap::<_, Vec<_>>::new();
    let mut seen_return_wiring = FxHashSet::default();
    for constraint in &pag.constraints {
        if return_destinations.contains(&constraint.dst)
            && matches!(
                constraint.kind,
                ConstraintKind::Copy | ConstraintKind::AddrOf
            )
            && seen_return_wiring.insert((constraint.kind, constraint.src, constraint.dst))
        {
            incoming.entry(constraint.dst).or_default().push(constraint);
        }
    }
    drop(seen_return_wiring);
    drop(return_destinations);
    let mut indirect_return_callees = conn.prepare_cached(INDIRECT_RETURN_CALLEES_SQL)?;
    let mut return_calls =
        conn.prepare_cached("INSERT INTO flow_return_calls VALUES (?1,?2,?3,?4)")?;
    let mut return_sites = std::collections::BTreeMap::<_, Vec<_>>::new();
    let mut seen_return_sites = FxHashSet::default();
    for edge in &analysis.call_edges {
        if let Some(site) = program.symbols.call_site_by_id(edge.call_site) {
            if let Some(dst) = site.return_dst {
                if seen_return_sites.insert((dst, edge.callee, edge.call_site)) {
                    return_sites
                        .entry((dst, edge.callee))
                        .or_default()
                        .push(edge.call_site);
                }
            }
        }
    }
    drop(seen_return_sites);
    let mut annotated_return_wiring = FxHashSet::default();
    // Return wiring is expanded by the PAG after merge. Attribute only
    // existing constraints whose source is a recorded return fact, resolving
    // through the same image-aware policy as PAG construction.
    for flow in &unique_flows {
        let (dst, root_callees) = match *flow {
            trace_ir::FlowConstraint::CallReturn {
                dst,
                callee_name,
                caller,
            } => (
                *dst,
                program
                    .symbols
                    .call_return_candidates(*caller, *dst, callee_name)
                    .to_vec(),
            ),
            trace_ir::FlowConstraint::CallReturnIndirect { dst, callee_var } => {
                let candidates = indirect_return_callees
                    .query_map(params![callee_var.0, dst.0], |r| Ok(FnId(r.get(0)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                (*dst, candidates)
            }
            _ => continue,
        };
        let Some(dst_node) = pag.var_node.get(&dst) else {
            continue;
        };
        let Some(sites) = program.flow_origins.get(*flow) else {
            continue;
        };
        let mut returned_by_callee = std::collections::BTreeMap::new();
        for root in root_callees {
            if returned_by_callee.contains_key(&root) {
                continue;
            }
            let mut pending = vec![root];
            let mut seen = FxHashSet::default();
            let mut returned = FxHashSet::default();
            while let Some(callee) = pending.pop() {
                if !seen.insert(callee) {
                    continue;
                }
                for ret in program.fn_returns.get(&callee).into_iter().flatten() {
                    match ret {
                        trace_ir::ReturnFlow::Copy { src } => {
                            if let Some(n) = pag.var_node.get(src) {
                                returned.insert(*n);
                            }
                        }
                        trace_ir::ReturnFlow::AddrOfVar { src } => {
                            if let Some(loc) =
                                pag.var_location.get(src).and_then(|l| pag.loc_node.get(l))
                            {
                                returned.insert(*loc);
                            }
                        }
                        trace_ir::ReturnFlow::AddrOfFn { callee } => {
                            if let Some(loc) = pag
                                .fn_locations
                                .get(callee)
                                .and_then(|l| pag.loc_node.get(l))
                            {
                                returned.insert(*loc);
                            }
                        }
                        trace_ir::ReturnFlow::Call { callee_name } => pending
                            .extend(program.symbols.return_flow_candidates(callee, callee_name)),
                    }
                }
            }
            returned_by_callee.insert(root, returned);
        }
        for constraint in incoming.get(dst_node).into_iter().flatten() {
            if !returned_by_callee
                .values()
                .any(|sources| sources.contains(&constraint.src))
            {
                continue;
            }
            let kind = match constraint.kind {
                ConstraintKind::Copy => "copy",
                ConstraintKind::AddrOf => "addr_of",
                _ => continue,
            };
            // Different caller constraints can share this wiring. Visit its
            // occurrence bucket once, rather than rescanning every call for
            // each caller. Origins below still belong to each distinct flow.
            for (&callee, sources) in &returned_by_callee {
                if sources.contains(&constraint.src)
                    && annotated_return_wiring.insert((constraint.src, constraint.dst, callee))
                {
                    for site in return_sites.get(&(dst, callee)).into_iter().flatten() {
                        return_calls.execute(params![
                            constraint.src.0,
                            constraint.dst.0,
                            site.0,
                            callee.0
                        ])?;
                    }
                }
            }
            for (span, expression) in sites {
                origins.execute(params![
                    constraint.src.0,
                    constraint.dst.0,
                    strings.intern(kind)?,
                    span.file.0,
                    span.line,
                    span.col,
                    strings.intern(expression.as_ref())?,
                    strings.intern("return value")?
                ])?;
            }
        }
    }
    Ok(())
}

fn export_one_variable(
    stmt: &mut rusqlite::Statement<'_>,
    var: &trace_ir::Variable,
    strings: &mut TextPool<'_>,
) -> Result<()> {
    let kind = match var.storage {
        StorageClass::Global => "global",
        StorageClass::FileStatic => "file_static",
        StorageClass::FnStatic => "fn_static",
        StorageClass::Param => "param",
        StorageClass::Local => "local",
    };
    stmt.execute(params![
        var.id.0,
        strings.intern(&var.name)?,
        kind,
        var.fn_id.map(|f| f.0),
        var.type_id.0,
        var.span.file.0,
        var.span.line,
        var.span.col,
        var.is_weak,
        var.target.map(|target| target.0),
        var.is_synthetic,
    ])?;
    Ok(())
}

fn param_type_name(desc: &TypeDesc) -> String {
    match desc {
        TypeDesc::Array { elem, .. } => format!("{}*", type_name(elem)),
        other => type_name(other),
    }
}

fn function_signature(func: &trace_ir::Function, program: &Program) -> String {
    if let Some(params) = program.symbols.explicit_params(func) {
        let mut parts = Vec::new();
        for i in 0..params.len() {
            if let Some(ty_id) = params.get(i) {
                if (ty_id.0 as usize) < program.types.all().len() {
                    let ty = program.types.get(ty_id);
                    parts.push(param_type_name(&ty.desc));
                } else {
                    parts.push("unknown".to_string());
                }
            } else {
                parts.push("unknown".to_string());
            }
        }
        if func.variadic {
            parts.push("...".to_string());
        }
        format!("{}({})", func.name, parts.join(", "))
    } else {
        format!("{}(...)", func.name)
    }
}

fn export_functions(conn: &Connection, program: &Program) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO functions (id, name, file_id, line_start, line_end, linkage, signature, is_defined, is_dep, is_weak, target_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for func in &program.symbols.functions {
        let linkage = match func.linkage {
            Linkage::External => "external",
            Linkage::Internal => "internal",
            Linkage::None => "none",
        };
        let signature = function_signature(func, program);
        stmt.execute(params![
            func.id.0,
            func.name,
            func.file.0,
            func.span.line,
            func.end_line.max(func.span.line),
            linkage,
            signature,
            func.is_defined as i32,
            program.is_dep_file(func.file) as i32,
            func.is_weak,
            func.target.map(|target| target.0),
        ])?;
    }
    Ok(())
}

fn export_call_sites_filtered(
    conn: &Connection,
    program: &Program,
    analysis: &AnalysisResult,
    strings: &mut TextPool<'_>,
) -> Result<()> {
    let mut with_edge = FxHashSet::default();
    for edge in &analysis.call_edges {
        with_edge.insert(edge.call_site);
    }
    let mut with_arg_flow = FxHashSet::default();
    for edge in &analysis.arg_flow_edges {
        with_arg_flow.insert(edge.call_site);
    }
    // Every site a call-order event names, so the graph never points past
    // the table (empty without `--sequencing`).
    let mut sequenced = FxHashSet::default();
    for cfg in program.cfgs.values().flatten() {
        for event in &cfg.events {
            sequenced.extend(event.sites.iter().copied());
        }
    }

    let mut stmt = conn.prepare_cached(
        "INSERT INTO call_sites_data (id, caller_fn_id, file_id, line, col, expansion_file_id, expansion_line, expansion_col, callee_text_id, is_direct, callee_var, return_dst) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    let mut operation_stmt =
        conn.prepare_cached("INSERT INTO flow_call_origins_data VALUES (?1, ?2, ?3, ?4, ?5)")?;
    let mut expression_stmt =
        conn.prepare_cached("INSERT INTO flow_call_expressions_data VALUES (?1, ?2)")?;
    for cs in &program.symbols.call_sites {
        let export = with_edge.contains(&cs.id)
            || with_arg_flow.contains(&cs.id)
            || !cs.is_direct
            || sequenced.contains(&cs.id);
        if !export {
            continue;
        }
        stmt.execute(params![
            cs.id.0,
            cs.caller.0,
            cs.span.file.0,
            cs.span.line,
            cs.span.col,
            cs.expansion_span.map(|span| span.file.0),
            cs.expansion_span.map(|span| span.line),
            cs.expansion_span.map(|span| span.col),
            strings.intern(cs.callee_name.as_str())?,
            cs.is_direct as i32,
            cs.callee_var.map(|id| id.0),
            cs.return_dst.map(|id| id.0),
        ])?;
        if let Some(expression) = cs.details.as_ref().and_then(|d| d.call_expression.as_ref()) {
            expression_stmt.execute(params![cs.id.0, strings.intern(expression.as_ref())?])?;
        }
        if let Some((span, expression)) = cs
            .details
            .as_ref()
            .and_then(|d| d.return_operation.as_deref())
        {
            operation_stmt.execute(params![
                cs.id.0,
                span.file.0,
                span.line,
                span.col,
                strings.intern(expression.as_ref())?
            ])?;
        }
    }
    Ok(())
}

/// The call-order graphs, in function and variant order; row ids count up
/// from 1 across graphs (see `docs/SQLITE_SCHEMA.md`, "Call sequencing
/// tables"). Nothing to write without `--sequencing`. trace-merge writes
/// its carried graphs through this too; [`read_cfgs`] reads them back.
pub fn export_cfg<'a>(
    conn: &Connection,
    cfgs: impl IntoIterator<Item = &'a trace_ir::FunctionCfg>,
    strings: &mut TextPool<'_>,
) -> Result<()> {
    let mut cfgs = cfgs.into_iter().peekable();
    if cfgs.peek().is_none() {
        return Ok(());
    }
    let kinds = strings.seed_sequencing()?;
    let mut function_stmt =
        conn.prepare_cached("INSERT INTO cfg_functions VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?;
    let mut block_stmt =
        conn.prepare_cached("INSERT INTO cfg_blocks VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
    let mut edge_stmt =
        conn.prepare_cached("INSERT OR IGNORE INTO cfg_edges VALUES (?1, ?2, ?3)")?;
    let mut event_stmt = conn.prepare_cached(
        "INSERT INTO cfg_events VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    let mut site_stmt =
        conn.prepare_cached("INSERT OR IGNORE INTO cfg_event_sites VALUES (?1, ?2, ?3)")?;
    let mut order_stmt =
        conn.prepare_cached("INSERT OR IGNORE INTO cfg_event_order VALUES (?1, ?2)")?;
    let mut gap_stmt =
        conn.prepare_cached("INSERT INTO cfg_gaps VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?;
    // Next row id of each table; a graph's blocks and events are offsets
    // from the first id it got.
    let (mut cfg_id, mut block_base, mut event_base, mut gap_id) = (1i64, 1i64, 1i64, 1i64);
    // An event's ordinal is its position in its block's evaluation order.
    let mut ordinals: Vec<i64> = Vec::new();
    for cfg in cfgs {
        let block = |b: trace_ir::BlockId| block_base + i64::from(b.0);
        let event = |e: trace_ir::EventId| event_base + i64::from(e.0);
        function_stmt.execute(params![
            cfg_id,
            cfg.function.0,
            cfg.tu.map(|f| f.0),
            cfg.variant,
            block(cfg.entry),
            block(cfg.exit),
            cfg.exceptional_exit.map(block),
            kinds.coverage(!cfg.gaps.is_empty()),
        ])?;
        for (ordinal, b) in cfg.blocks.iter().enumerate() {
            block_stmt.execute(params![
                block_base + ordinal as i64,
                cfg_id,
                ordinal as i64,
                kinds.block(b.kind),
                b.span.map(|s| s.file.0),
                b.span.map(|s| s.line),
                b.span.map(|s| s.col),
            ])?;
        }
        for e in &cfg.edges {
            edge_stmt.execute(params![block(e.from), block(e.to), kinds.edge(e.kind)])?;
        }
        ordinals.clear();
        ordinals.resize(cfg.events.len(), 0);
        for b in &cfg.blocks {
            for (ordinal, &e) in b.events.iter().enumerate() {
                ordinals[e.0 as usize] = ordinal as i64;
            }
        }
        for (i, e) in cfg.events.iter().enumerate() {
            let id = event_base + i as i64;
            let occurrence = &e.occurrence;
            event_stmt.execute(params![
                id,
                cfg_id,
                block(e.block),
                ordinals[i],
                kinds.event(e.kind),
                occurrence.span.file.0,
                occurrence.span.line,
                occurrence.span.col,
                occurrence.expansion_span.map(|s| s.file.0),
                occurrence.expansion_span.map(|s| s.line),
                occurrence.expansion_span.map(|s| s.col),
            ])?;
            // A repeated site keeps its first position.
            for (ordinal, site) in e.sites.iter().enumerate() {
                site_stmt.execute(params![id, site.0, ordinal as i64])?;
            }
        }
        for &(a, b) in &cfg.order {
            order_stmt.execute(params![event(a), event(b)])?;
        }
        for g in &cfg.gaps {
            gap_stmt.execute(params![
                gap_id,
                cfg_id,
                kinds.gap(g.kind),
                g.span.map(|s| s.file.0),
                g.span.map(|s| s.line),
                g.span.map(|s| s.col),
                g.events.map(|(first, _)| event(first)),
                g.events.map(|(_, last)| event(last)),
            ])?;
            gap_id += 1;
        }
        cfg_id += 1;
        block_base += cfg.blocks.len() as i64;
        event_base += cfg.events.len() as i64;
    }
    Ok(())
}

// The statements reading the call-order graphs whose `cfg_functions.id` is
// in `?1..=?2`, each by its graph id; events are reached through their
// blocks, as `cfg_events` has no index on `cfg_id`. `CROSS JOIN` keeps the
// blocks the outer loop: over a range, SQLite would otherwise scan a whole
// table in its `ORDER BY` order. Kinds are read as their names, and rows
// in the order they were written (edges, whose order is not stored, by
// kind name), so a graph reads back the same whatever ids its database
// gave its sites and kinds.
const READ_CFG_FUNCTIONS: &str =
    "SELECT id, fn_id, tu_file_id, variant, entry_block, exit_block, exceptional_exit_block \
     FROM cfg_functions WHERE id BETWEEN ?1 AND ?2 ORDER BY id";
const READ_CFG_BLOCKS: &str = "SELECT b.id, b.cfg_id, b.ordinal, k.text, b.file_id, b.line, b.col \
     FROM cfg_blocks b JOIN strings k ON k.id = b.kind_id \
     WHERE b.cfg_id BETWEEN ?1 AND ?2 ORDER BY b.cfg_id, b.ordinal";
const READ_CFG_EDGES: &str = "SELECT e.src_block, e.dst_block, k.text \
     FROM cfg_blocks b CROSS JOIN cfg_edges e ON e.src_block = b.id JOIN strings k ON k.id = e.kind_id \
     WHERE b.cfg_id BETWEEN ?1 AND ?2 ORDER BY e.src_block, e.dst_block, k.text";
const READ_CFG_EVENTS: &str = "SELECT e.id, b.cfg_id, e.block_id, e.ordinal, k.text, \
            e.file_id, e.line, e.col, e.expansion_file_id, e.expansion_line, e.expansion_col \
     FROM cfg_blocks b CROSS JOIN cfg_events e ON e.block_id = b.id JOIN strings k ON k.id = e.kind_id \
     WHERE b.cfg_id BETWEEN ?1 AND ?2 ORDER BY e.id";
const READ_CFG_EVENT_SITES: &str = "SELECT es.event_id, es.call_site_id \
     FROM cfg_blocks b CROSS JOIN cfg_events e ON e.block_id = b.id \
     CROSS JOIN cfg_event_sites es ON es.event_id = e.id \
     WHERE b.cfg_id BETWEEN ?1 AND ?2 ORDER BY es.event_id, es.ordinal";
const READ_CFG_EVENT_ORDER: &str = "SELECT o.event_id, o.next_event_id \
     FROM cfg_blocks b CROSS JOIN cfg_events e ON e.block_id = b.id \
     CROSS JOIN cfg_event_order o ON o.event_id = e.id \
     WHERE b.cfg_id BETWEEN ?1 AND ?2 ORDER BY o.event_id, o.next_event_id";
const READ_CFG_GAPS: &str = "SELECT g.cfg_id, k.text, g.file_id, g.line, g.col, \
            g.first_event, g.last_event \
     FROM cfg_gaps g JOIN strings k ON k.id = g.kind_id \
     WHERE g.cfg_id BETWEEN ?1 AND ?2 ORDER BY g.id";

/// The statements [`read_cfgs`] runs, by the table each reads first: a
/// reader preparing them all checks a database has every column it needs.
pub const CFG_READS: [(&str, &str); 7] = [
    ("cfg_functions", READ_CFG_FUNCTIONS),
    ("cfg_blocks", READ_CFG_BLOCKS),
    ("cfg_edges", READ_CFG_EDGES),
    ("cfg_events", READ_CFG_EVENTS),
    ("cfg_event_sites", READ_CFG_EVENT_SITES),
    ("cfg_event_order", READ_CFG_EVENT_ORDER),
    ("cfg_gaps", READ_CFG_GAPS),
];

/// One `cfg_functions` row read back by [`read_cfgs`], in the database's
/// own ids: `function`, sites and files are its rows.
#[derive(Debug, Clone)]
pub struct StoredCfg {
    pub id: crate::CfgId,
    pub cfg: trace_ir::FunctionCfg,
    /// The row of each event, by [`trace_ir::EventId`].
    pub event_ids: Vec<crate::CfgEventId>,
}

/// The kind `parse` reads from `text`, as [`export_cfg`] wrote it.
fn cfg_kind<K>(parse: fn(&str) -> Option<K>, text: &str) -> rusqlite::Result<K> {
    parse(text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown call-order kind '{text}'").into(),
        )
    })
}

/// The inverse of [`export_cfg`]: the graphs whose `cfg_functions.id` is in
/// `ids`, in id order. The export keeps no expansion fingerprint, so every
/// occurrence reads back with `expansion_id` 0.
pub fn read_cfgs(conn: &Connection, ids: RangeInclusive<crate::CfgId>) -> Result<Vec<StoredCfg>> {
    use trace_ir::{
        Block, BlockId, BlockKind, CallEvent, CallOccurrence, CallSiteId, CfgEdge, EdgeKind,
        EventId, EventKind, FileId, FnId, FunctionCfg, Gap, GapKind, Span,
    };
    let span = |file: Option<i64>, line: Option<i64>, col: Option<i64>| {
        Some(Span::new(FileId(file? as u32), line? as u32, col? as u32))
    };
    let range = params![ids.start(), ids.end()];
    let mut cfgs: Vec<StoredCfg> = Vec::new();
    // Graph ids -> their index in `cfgs`; block and event ids -> their
    // graph's index and local id.
    let mut graphs: FxHashMap<crate::CfgId, usize> = FxHashMap::default();
    let mut blocks: FxHashMap<i64, (usize, BlockId)> = FxHashMap::default();
    let mut events: FxHashMap<crate::CfgEventId, (usize, EventId)> = FxHashMap::default();
    let mut ends: Vec<(i64, i64, Option<i64>)> = Vec::new();
    let mut stmt = conn.prepare_cached(READ_CFG_FUNCTIONS)?;
    let mut rows = stmt.query(range)?;
    while let Some(r) = rows.next()? {
        let id: crate::CfgId = r.get(0)?;
        graphs.insert(id, cfgs.len());
        ends.push((r.get(4)?, r.get(5)?, r.get(6)?));
        cfgs.push(StoredCfg {
            id,
            cfg: FunctionCfg {
                function: FnId(r.get::<_, i64>(1)? as u32),
                tu: r.get::<_, Option<i64>>(2)?.map(|f| FileId(f as u32)),
                variant: r.get::<_, i64>(3)? as u32,
                entry: BlockId(0),
                exit: BlockId(0),
                exceptional_exit: None,
                blocks: Vec::new(),
                edges: Vec::new(),
                events: Vec::new(),
                order: Vec::new(),
                gaps: Vec::new(),
            },
            event_ids: Vec::new(),
        });
    }
    let mut stmt = conn.prepare_cached(READ_CFG_BLOCKS)?;
    let mut rows = stmt.query(range)?;
    while let Some(r) = rows.next()? {
        let Some(&g) = graphs.get(&r.get(1)?) else {
            continue;
        };
        let cfg = &mut cfgs[g].cfg;
        let ordinal = r.get::<_, i64>(2)? as usize;
        if cfg.blocks.len() <= ordinal {
            cfg.blocks.resize_with(ordinal + 1, || Block {
                kind: BlockKind::Ordinary,
                span: None,
                events: Vec::new(),
            });
        }
        cfg.blocks[ordinal] = Block {
            kind: cfg_kind(BlockKind::parse, &r.get::<_, String>(3)?)?,
            span: span(r.get(4)?, r.get(5)?, r.get(6)?),
            events: Vec::new(),
        };
        blocks.insert(r.get(0)?, (g, BlockId(ordinal as u32)));
    }
    let block = |id: i64| blocks.get(&id).map(|&(_, b)| b);
    for (stored, &(entry, exit, exceptional)) in cfgs.iter_mut().zip(&ends) {
        let cfg = &mut stored.cfg;
        cfg.entry = block(entry).context("cfg_functions.entry_block names no block")?;
        cfg.exit = block(exit).context("cfg_functions.exit_block names no block")?;
        cfg.exceptional_exit = exceptional.and_then(block);
    }
    let mut stmt = conn.prepare_cached(READ_CFG_EDGES)?;
    let mut rows = stmt.query(range)?;
    while let Some(r) = rows.next()? {
        let (Some(&(g, from)), Some(to)) = (blocks.get(&r.get(0)?), block(r.get(1)?)) else {
            continue;
        };
        let kind = cfg_kind(EdgeKind::parse, &r.get::<_, String>(2)?)?;
        cfgs[g].cfg.edges.push(CfgEdge { from, to, kind });
    }
    // Each block's events by their ordinal in it.
    let mut placed: Vec<(usize, BlockId, i64, EventId)> = Vec::new();
    let mut stmt = conn.prepare_cached(READ_CFG_EVENTS)?;
    let mut rows = stmt.query(range)?;
    while let Some(r) = rows.next()? {
        let (Some(&g), Some(at)) = (graphs.get(&r.get(1)?), block(r.get(2)?)) else {
            continue;
        };
        let id: crate::CfgEventId = r.get(0)?;
        let stored = &mut cfgs[g];
        let event = EventId(stored.cfg.events.len() as u32);
        stored.cfg.events.push(CallEvent {
            block: at,
            kind: cfg_kind(EventKind::parse, &r.get::<_, String>(4)?)?,
            occurrence: CallOccurrence {
                span: span(r.get(5)?, r.get(6)?, r.get(7)?).expect("NOT NULL columns"),
                expansion_span: span(r.get(8)?, r.get(9)?, r.get(10)?),
                expansion_id: 0,
            },
            sites: Vec::new(),
        });
        stored.event_ids.push(id);
        events.insert(id, (g, event));
        placed.push((g, at, r.get(3)?, event));
    }
    placed.sort_unstable();
    for (g, at, _, event) in placed {
        cfgs[g].cfg.blocks[at.0 as usize].events.push(event);
    }
    let mut stmt = conn.prepare_cached(READ_CFG_EVENT_SITES)?;
    let mut rows = stmt.query(range)?;
    while let Some(r) = rows.next()? {
        if let Some(&(g, event)) = events.get(&r.get(0)?) {
            let site = CallSiteId(r.get::<_, i64>(1)? as u32);
            cfgs[g].cfg.events[event.0 as usize].sites.push(site);
        }
    }
    let mut stmt = conn.prepare_cached(READ_CFG_EVENT_ORDER)?;
    let mut rows = stmt.query(range)?;
    while let Some(r) = rows.next()? {
        if let (Some(&(g, a)), Some(&(_, b))) = (events.get(&r.get(0)?), events.get(&r.get(1)?)) {
            cfgs[g].cfg.order.push((a, b));
        }
    }
    let event = |id: Option<crate::CfgEventId>| id.and_then(|id| events.get(&id)).map(|&(_, e)| e);
    let mut stmt = conn.prepare_cached(READ_CFG_GAPS)?;
    let mut rows = stmt.query(range)?;
    while let Some(r) = rows.next()? {
        let Some(&g) = graphs.get(&r.get(0)?) else {
            continue;
        };
        cfgs[g].cfg.gaps.push(Gap {
            kind: cfg_kind(GapKind::parse, &r.get::<_, String>(1)?)?,
            span: span(r.get(2)?, r.get(3)?, r.get(4)?),
            events: event(r.get(5)?).zip(event(r.get(6)?)),
        });
    }
    Ok(cfgs)
}

fn export_call_edges(
    conn: &Connection,
    analysis: &AnalysisResult,
    strings: &mut TextPool<'_>,
) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO call_edges_data (id, call_site_id, caller_fn_id, callee_fn_id, resolution_id) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (i, edge) in analysis.call_edges.iter().enumerate() {
        let resolution = match edge.resolution {
            trace_analysis::ResolutionKind::Direct => "direct",
            trace_analysis::ResolutionKind::Indirect => "indirect",
            trace_analysis::ResolutionKind::Ambiguous => "ambiguous",
            trace_analysis::ResolutionKind::External => "external",
            trace_analysis::ResolutionKind::IpcBridge => "ipc",
        };
        // Synthetic edges (e.g. IPC bridge injection) have no corresponding
        // source-level call site; store NULL so consumers can distinguish them
        // rather than mis-joining to a real call site.
        let call_site_id: Option<i64> = (edge.call_site != trace_analysis::SYNTHETIC_CALL_SITE)
            .then_some(edge.call_site.0 as i64);
        stmt.execute(params![
            i as i64 + 1,
            call_site_id,
            edge.caller.0,
            edge.callee.0,
            strings.intern(resolution)?
        ])?;
    }
    Ok(())
}

fn export_arg_flow(conn: &Connection, analysis: &AnalysisResult) -> Result<()> {
    if analysis.arg_flow_edges.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare_cached(
        "INSERT INTO arg_flow_edges (id, call_site_id, arg_index, actual_var_id, actual_fn_id, formal_var_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (i, edge) in analysis.arg_flow_edges.iter().enumerate() {
        stmt.execute(params![
            i as i64 + 1,
            edge.call_site.0,
            edge.arg_index,
            edge.actual_var.map(|v| v.0),
            edge.actual_fn.map(|f| f.0),
            edge.formal.0
        ])?;
    }
    Ok(())
}

/// One row per execution context, in the analysis' order (see
/// `docs/SQLITE_SCHEMA.md`, `execution_contexts`).
fn export_execution_contexts(conn: &Connection, analysis: &AnalysisResult) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO execution_contexts (id, kind, entry_fn_id, call_site_id, api_fn_id, \
         param_index, receiver_var_id, model, multi_instance, self_concurrent) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    for (i, context) in analysis.execution_contexts.iter().enumerate() {
        let start = context.start.as_ref();
        stmt.execute(params![
            i as i64 + 1,
            context.kind.as_str(),
            context.entry.0,
            start.map(|s| s.call_site.0),
            start.map(|s| s.api.0),
            start.map(|s| s.param),
            start.and_then(|s| s.receiver).map(|v| v.0),
            context.model.as_deref(),
            context.multi_instance.as_str(),
            context.self_concurrent as i32,
        ])?;
    }
    Ok(())
}

fn export_locations(conn: &Connection, pag: &Pag) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO locations (id, kind, desc, type_id) VALUES (?1, ?2, ?3, NULL)",
    )?;
    for loc in &pag.locations {
        let kind = format!("{:?}", loc.kind);
        stmt.execute(params![loc.id.0, kind, loc.desc])?;
    }
    Ok(())
}

fn export_points_to(conn: &Connection, _pag: &Pag, analysis: &AnalysisResult) -> Result<()> {
    let mut stmt = conn
        .prepare_cached("INSERT OR IGNORE INTO points_to (var_node_id, loc_id) VALUES (?1, ?2)")?;
    for (node, locs) in &analysis.points_to {
        for loc in locs {
            stmt.execute(params![node.0, loc.0])?;
        }
    }
    Ok(())
}

fn export_diagnostics(
    conn: &Connection,
    program: &Program,
    analysis: &AnalysisResult,
) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO diagnostics (id, severity, file_id, line, message, stage) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut next_id: i64 = 1;
    if !program.diagnostics.is_empty() {
        for d in program.diagnostics.iter() {
            let severity = match d.severity {
                trace_ir::DiagnosticSeverity::Error => "error",
                trace_ir::DiagnosticSeverity::Warning => "warning",
                trace_ir::DiagnosticSeverity::Info => "info",
            };
            stmt.execute(params![
                next_id,
                severity,
                d.file.map(|f| f.0),
                d.line,
                d.message,
                d.stage
            ])?;
            next_id += 1;
        }
    }
    // A budget-truncated solve is a property of the whole analysis run, not
    // of any source file: it goes in with a NULL file and stage "analyze",
    // and consumers can also read `analysis_run.options_json.solver_partial`.
    if !analysis.solve.converged {
        stmt.execute(params![
            next_id,
            "warning",
            None::<i64>,
            0i64,
            format!(
                "solver stopped early: {} of {} worklist pops processed; the result is a monotone partial \
                 fixpoint (converged=false; see analysis_run.options_json.solve_budget_pops / \
                 solve_budget_secs)",
                analysis.solve.pops,
                analysis.solve.budget_pops
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "unlimited".into())
            ),
            "analyze"
        ])?;
    }
    Ok(())
}

pub fn open_db(path: &Path) -> Result<Connection> {
    Connection::open(path).with_context(|| format!("failed to open db at {}", path.display()))
}

#[cfg(test)]
mod tests {
    /// A one-block graph whose call has its sites out of id order and whose
    /// two blocks are joined by two kinds of edge.
    fn cfg_with_alternatives() -> trace_ir::FunctionCfg {
        use trace_ir::*;
        FunctionCfg {
            function: FnId(0),
            tu: None,
            variant: 0,
            entry: BlockId(0),
            exit: BlockId(1),
            exceptional_exit: None,
            blocks: vec![
                Block {
                    kind: BlockKind::Entry,
                    span: None,
                    events: vec![EventId(0)],
                },
                Block {
                    kind: BlockKind::Exit,
                    span: None,
                    events: Vec::new(),
                },
            ],
            edges: vec![
                CfgEdge {
                    from: BlockId(0),
                    to: BlockId(1),
                    kind: EdgeKind::Return,
                },
                CfgEdge {
                    from: BlockId(0),
                    to: BlockId(1),
                    kind: EdgeKind::Fallthrough,
                },
            ],
            events: vec![CallEvent {
                block: BlockId(0),
                kind: EventKind::Call,
                occurrence: CallOccurrence {
                    span: Span::new(FileId(0), 2, 3),
                    expansion_span: None,
                    expansion_id: 0,
                },
                // The primary record, then an alternative with a lower id.
                sites: vec![CallSiteId(5), CallSiteId(3), CallSiteId(4)],
            }],
            order: Vec::new(),
            gaps: Vec::new(),
        }
    }

    /// The graph written to a fresh database after `before` were interned,
    /// read back.
    fn cfg_round_trip(before: &[&str]) -> trace_ir::FunctionCfg {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::SCHEMA_V8).unwrap();
        // The graph's function, file and sites have no rows here.
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        let mut strings = TextPool::new(&conn).unwrap();
        for text in before {
            strings.intern(text).unwrap();
        }
        super::export_cfg(&conn, [&cfg_with_alternatives()], &mut strings).unwrap();
        let mut read = super::read_cfgs(&conn, crate::CfgId(1)..=crate::CfgId(1)).unwrap();
        assert_eq!(read.len(), 1);
        read.pop().unwrap().cfg
    }

    #[test]
    fn cfgs_read_back_as_written_whatever_the_ids() {
        let fresh = cfg_round_trip(&[]);
        let written = cfg_with_alternatives();
        assert_eq!(
            fresh.events[0].sites, written.events[0].sites,
            "an event's sites keep their written order, primary first"
        );
        // Kind ids depend on what was interned first; the edges read back
        // do not.
        let reinterned = cfg_round_trip(&["return", "ordinary", "exit"]);
        assert!(fresh.structurally_equal(&reinterned));
    }

    #[test]
    fn interned_text_preserves_rows_and_uses_selective_indexes() {
        let legacy = Connection::open_in_memory().unwrap();
        legacy.execute_batch(crate::SCHEMA_V7).unwrap();
        let compact = Connection::open_in_memory().unwrap();
        compact.execute_batch(crate::SCHEMA_V8).unwrap();
        for conn in [&legacy, &compact] {
            conn.execute_batch(
                "INSERT INTO files(id,path,sha256) VALUES(0,'main.cpp','');
                 INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined)
                 VALUES(0,'send',0,1,1000,'external','send()',1);
                 INSERT INTO types VALUES(0,'ptr','char *',8,'{}');",
            ).unwrap();
        }
        let mut strings = TextPool::new(&compact).unwrap();
        let text = "receiver->send(\"quoted ' text\", payload /* Привет */)".repeat(20);
        let send = strings.intern("send").unwrap();
        let copy = strings.intern("copy").unwrap();
        let ret = strings.intern("return value").unwrap();
        let var = strings.intern("var").unwrap();
        let direct = strings.intern("direct").unwrap();
        let pointer = strings.intern("char *").unwrap();
        let empty = strings.intern("").unwrap();
        for site in 0..1000 {
            legacy.execute("INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct) VALUES(?1,0,0,?1,1,'send',1)", [site]).unwrap();
            compact.execute("INSERT INTO call_sites_data(id,caller_fn_id,file_id,line,col,callee_text_id,is_direct) VALUES(?1,0,0,?1,1,?2,1)", params![site, send]).unwrap();
            let expression = if site % 2 == 0 { text.as_str() } else { "" };
            let id = strings.intern(expression).unwrap();
            legacy.execute("INSERT INTO variables(id,name,kind,fn_id,type_id,file_id,line) VALUES(?1,?2,'param',0,0,0,?1)", params![site, expression]).unwrap();
            compact.execute("INSERT INTO variables_data(id,name_id,kind,fn_id,type_id,file_id,line) VALUES(?1,?2,'param',0,0,0,?1)", params![site, id]).unwrap();
            legacy.execute("INSERT INTO flow_nodes(id,kind,label,detail,var_id,fn_id,call_site_id) VALUES(?1,'var',?2,'',?1,0,?1)", params![site, expression]).unwrap();
            compact.execute("INSERT INTO flow_nodes_data(id,kind_id,label_id,var_id,fn_id,call_site_id) VALUES(?1,?2,?3,?1,0,?1)", params![site, var, id]).unwrap();
            legacy
                .execute(
                    "INSERT INTO flow_parameters VALUES(0,?1,?1,?2,'char *')",
                    params![site, expression],
                )
                .unwrap();
            compact
                .execute(
                    "INSERT INTO flow_parameters_data VALUES(0,?1,?1,?2,?3)",
                    params![site, id, pointer],
                )
                .unwrap();
            legacy
                .execute("INSERT INTO flow_edges VALUES(?1,?1,?1,'copy')", [site])
                .unwrap();
            compact
                .execute(
                    "INSERT INTO flow_edges_data VALUES(?1,?1,?1,?2)",
                    params![site, copy],
                )
                .unwrap();
            legacy
                .execute("INSERT INTO call_edges VALUES(?1,?1,0,0,'direct')", [site])
                .unwrap();
            compact
                .execute(
                    "INSERT INTO call_edges_data VALUES(?1,?1,0,0,?2)",
                    params![site, direct],
                )
                .unwrap();
            legacy
                .execute(
                    "INSERT INTO flow_call_expressions VALUES (?1,?2)",
                    params![site, expression],
                )
                .unwrap();
            compact
                .execute(
                    "INSERT INTO flow_call_expressions_data VALUES (?1,?2)",
                    params![site, id],
                )
                .unwrap();
            legacy
                .execute(
                    "INSERT INTO flow_call_origins VALUES (?1,0,?1,1,?2)",
                    params![site, expression],
                )
                .unwrap();
            compact
                .execute(
                    "INSERT INTO flow_call_origins_data VALUES (?1,0,?1,1,?2)",
                    params![site, id],
                )
                .unwrap();
            // Preserve duplicate operation rows as well as repeated text.
            for _ in 0..2 {
                legacy
                    .execute(
                        "INSERT INTO flow_origins VALUES (?1,?1,'copy',0,?1,1,?2,'return value')",
                        params![site, expression],
                    )
                    .unwrap();
                compact
                    .execute(
                        "INSERT INTO flow_origins_data VALUES (?1,?1,?2,0,?1,1,?3,?4)",
                        params![site, copy, id, ret],
                    )
                    .unwrap();
            }
        }
        assert_eq!(strings.intern("").unwrap(), empty);
        assert_eq!(
            compact
                .query_row("SELECT count(*) FROM strings WHERE text=?1", [&text], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        for (relation, order) in [
            ("flow_call_expressions", "call_site_id"),
            ("flow_call_origins", "call_site_id"),
            ("flow_origins", "src_node,dst_node,line"),
            ("variables", "id"),
            ("call_sites", "id"),
            ("call_edges", "id"),
            ("flow_nodes", "id"),
            ("flow_edges", "id"),
            ("flow_parameters", "fn_id,arg_index"),
        ] {
            let rows = |conn: &Connection| {
                let mut stmt = conn
                    .prepare(&format!("SELECT * FROM {relation} ORDER BY {order}"))
                    .unwrap();
                let columns = stmt.column_count();
                stmt.query_map([], |r| {
                    (0..columns)
                        .map(|i| r.get::<_, rusqlite::types::Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
            };
            assert_eq!(rows(&legacy), rows(&compact), "{relation}");
            assert!(crate::table_exists(&compact, relation).unwrap());
            let text_column = match relation {
                "variables" | "flow_parameters" => "name",
                "call_sites" => "callee_text",
                "call_edges" => "resolution",
                "flow_nodes" => "label",
                "flow_edges" => "kind",
                _ => "expression",
            };
            for column in [text_column, order.split(',').next().unwrap()] {
                assert!(crate::column_exists(&compact, relation, column).unwrap());
                let plan: Vec<String> = compact
                    .prepare(&format!(
                        "EXPLAIN QUERY PLAN SELECT {text_column} FROM {relation} WHERE {column}=?1"
                    ))
                    .unwrap()
                    .query_map([42], |r| r.get(3))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                if column != text_column {
                    assert!(
                        plan.iter()
                            .all(|step| !step.contains("SCAN") && !step.contains("MATERIALIZE")),
                        "{plan:?}"
                    );
                }
            }
        }
        let plan: Vec<String> = compact
            .prepare("EXPLAIN QUERY PLAN SELECT expression FROM flow_origins WHERE dst_node=?1")
            .unwrap()
            .query_map([42], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|step| step.contains("idx_flow_origins_dst_node")),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .all(|step| !step.contains("SCAN") && !step.contains("MATERIALIZE")),
            "{plan:?}"
        );
        // These are the left joins used by inspection. A view containing a
        // join can pass direct-lookup checks yet materialize every text row here.
        for query in [
            "SELECT e.src_node,o.expression FROM flow_edges e LEFT JOIN flow_origins o
             ON o.src_node=e.src_node AND o.dst_node=e.dst_node AND o.kind=e.kind WHERE e.src_node=?1",
            "SELECT a.src_node,x.expression FROM flow_calls a LEFT JOIN flow_call_expressions x
             ON x.call_site_id=a.call_site_id WHERE a.src_node=?1",
        ] {
            let plan: Vec<String> = compact.prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                .unwrap().query_map([42], |r| r.get(3)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            assert!(plan.iter().all(|step| !step.contains("SCAN") && !step.contains("MATERIALIZE")), "{plan:?}");
            assert!(plan.iter().any(|step| step.contains("SEARCH strings USING INTEGER PRIMARY KEY")), "{plan:?}");
        }
        let pages = |conn: &Connection| {
            conn.query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))
                .unwrap()
        };
        assert!(
            pages(&compact) * 4 < pages(&legacy),
            "repeated text must occupy substantially fewer pages"
        );
    }

    #[test]
    fn indirect_return_join_uses_export_indexes_and_excludes_unrelated_sites() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::TABLES_V7).unwrap();
        conn.execute(
            "INSERT INTO files(id,path,sha256) VALUES(0,'main.c','')",
            [],
        )
        .unwrap();
        for id in [0, 2, 9, 30, 40, 50, 60, 99] {
            conn.execute("INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined)
                VALUES(?1,?2,0,1,1,'external','f()',1)", params![id,format!("f{id}")]).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO call_sites
            (id,caller_fn_id,file_id,line,col,callee_text,is_direct,callee_var,return_dst) VALUES
            (1,0,0,1,1,'fp',0,10,20),(2,0,0,2,1,'fp',0,10,20),
            (3,0,0,3,1,'gp',0,11,20),(4,0,0,4,1,'fp',0,10,21),
            (5,0,0,5,1,'direct',1,NULL,20),(6,0,0,6,1,'fp',0,10,NULL);
            INSERT INTO call_edges VALUES
            (1,1,0,9,'indirect'),(2,2,0,2,'indirect'),(3,2,0,9,'indirect'),
            (4,3,0,30,'indirect'),(5,4,0,40,'indirect'),(6,5,0,50,'direct'),
            (7,6,0,60,'indirect'),(8,NULL,0,99,'ipc');",
        )
        .unwrap();
        // The lookup must be indexed during export, before deferred indexes.
        conn.execute_batch(crate::PROVENANCE_INDEXES_V7).unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                super::INDIRECT_RETURN_CALLEES_SQL
            ))
            .unwrap()
            .query_map([10, 20], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|p| p.contains("SEARCH") && p.contains("idx_call_sites_indirect_return")),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .any(|p| p.contains("SEARCH") && p.contains("idx_call_edges_callsite")),
            "{plan:?}"
        );
        assert!(!plan.iter().any(|p| p.contains("SCAN ")), "{plan:?}");
        let candidates: Vec<u32> = conn
            .prepare(super::INDIRECT_RETURN_CALLEES_SQL)
            .unwrap()
            .query_map([10, 20], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(candidates, [2, 9]);
    }

    use super::*;
    use trace_ir::{Function, Span, TargetId, TypeId, Variable};

    #[test]
    fn exports_target_catalog_with_source_and_dependency_relations() {
        let mut program = Program::new(PathBuf::from("/fixture"));
        let file = program.symbols.add_file(PathBuf::from("/fixture/main.c"));
        program.link_targets = vec![
            trace_ir::LinkTarget {
                id: TargetId(0),
                name: "app".into(),
                output: PathBuf::from("/fixture/app"),
                sources: vec![file],
                dependencies: vec![TargetId(1)],
            },
            trace_ir::LinkTarget {
                id: TargetId(1),
                name: "lib".into(),
                output: PathBuf::from("/fixture/lib.so"),
                sources: vec![],
                dependencies: vec![],
            },
        ];
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TABLES_V8).unwrap();
        export_files(&conn, &program).unwrap();
        export_link_targets(&conn, &program).unwrap();
        let mut stmt = conn.prepare("SELECT t.name, t.output, f.path, d.name FROM link_targets t JOIN target_sources s ON s.target_id = t.id JOIN files f ON f.id = s.file_id JOIN target_dependencies e ON e.target_id = t.id JOIN link_targets d ON d.id = e.dependency_id").unwrap();
        let rows: Vec<(String, String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![(
                "app".into(),
                "/fixture/app".into(),
                "/fixture/main.c".into(),
                "lib".into()
            )]
        );
    }

    #[test]
    fn exports_weak_and_target_metadata_in_both_detail_modes() {
        let mut program = Program::new(PathBuf::from("/fixture"));
        let file = program.symbols.add_file(PathBuf::from("/fixture/hooks.c"));
        for (name, weak, target) in [
            ("fallback", true, Some(TargetId(7))),
            ("normal", false, None),
        ] {
            let id = program.symbols.alloc_fn_id();
            program.symbols.add_function(Function {
                id,
                name: name.into(),
                linkage: Linkage::External,
                return_type: TypeId(0),
                params: vec![],
                locals: vec![],
                span: Span::new(file, 1, 1),
                end_line: 1,
                file,
                is_defined: true,
                is_weak: weak,
                target,
                param_type_ids: vec![],
                explicit_arity: Some(0),
                owner_unresolved: false,
                variadic: false,
                defaulted_in_class: false,
                declared_in_class: false,
                is_static_member: false,
                default_args: 0,
                reference_params: Vec::new(),
                is_virtual: false,
                is_final: false,
                is_cpp: false,
                c_linkage: false,
                tu: None,
            });
            let id = program.symbols.alloc_var_id();
            program.symbols.add_variable(Variable {
                temp: None,
                is_defined: true,
                id,
                name: name.into(),
                type_id: TypeId(0),
                storage: StorageClass::Global,
                fn_id: None,
                param_index: None,
                span: Span::new(file, 1, 1),
                is_pointer: true,
                is_weak: weak,
                target,
                is_namespaced: false,
                qualified_name: None,
                c_linkage: false,
                is_synthetic: false,
                is_static_member: false,
            });
            // A flow fact puts the variable in the graph minimal export
            // writes variables from.
            program
                .flow
                .push(trace_ir::FlowConstraint::NewHeap { dst: id });
        }
        let pag = Pag::build(&program);
        for full_detail in [false, true] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
            conn.execute_batch(TABLES_V8).unwrap();
            let mut strings = TextPool::new(&conn).unwrap();
            export_functions(&conn, &program).unwrap();
            if full_detail {
                export_variables(&conn, &program, &mut strings).unwrap();
            } else {
                export_flow_and_arg_flow_vars(
                    &conn,
                    &program,
                    &pag,
                    &AnalysisResult::default(),
                    &mut strings,
                )
                .unwrap();
            }
            for table in ["functions", "variables"] {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT name, is_weak, target_id FROM {table} ORDER BY name"
                    ))
                    .unwrap();
                let rows: Vec<(String, bool, Option<u32>)> = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                assert_eq!(
                    rows,
                    vec![
                        ("fallback".into(), true, Some(7)),
                        ("normal".into(), false, None)
                    ],
                    "{table}, full_detail={full_detail}"
                );
            }
        }
    }
}
