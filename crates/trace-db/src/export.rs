use crate::schema::{INDEXES_V5, SCHEMA_VERSION, TABLES_V5};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use rustc_hash::FxHashSet;
use std::fs;
use std::path::{Path, PathBuf};
use trace_analysis::{AnalysisResult, ConstraintKind, LocKind, Pag, PagNodeKind};
use trace_ir::{Linkage, Program, StorageClass, TypeDesc, VarId};

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
        conn.execute_batch(TABLES_V5)?;

        let options_json = serde_json::json!({
            "include_paths": program.include_paths,
            "defines": program.defines,
            "dep_roots": program.dep_roots(),
            "include_points_to": opts.include_points_to,
            "full_detail": opts.full_detail,
            "model_files": opts.model_files,
            "explore": program.explore,
            "explore_budget": program.explore_budget,
            "variants_merged": program.variants_merged,
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
        export_call_sites_filtered(&conn, program, analysis)?;
        export_call_edges(&conn, analysis)?;
        if opts.full_detail {
            export_types(&conn, program)?;
            export_variables(&conn, program)?;
            export_locations(&conn, pag)?;
        } else {
            export_flow_and_arg_flow_vars(&conn, program, pag, analysis)?;
        }
        export_arg_flow(&conn, analysis)?;
        export_flow_graph(&conn, program, pag, analysis)?;
        if opts.include_points_to {
            export_points_to(&conn, pag, analysis)?;
        }
        export_diagnostics(&conn, program)?;
        conn.execute_batch(INDEXES_V5)?;
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

const INSERT_VARIABLE: &str = "INSERT INTO variables (id, name, kind, fn_id, type_id, file_id, line, col, is_weak, target_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)";

fn export_variables(conn: &Connection, program: &Program) -> Result<()> {
    let mut stmt = conn.prepare_cached(INSERT_VARIABLE)?;
    for var in &program.symbols.variables {
        export_one_variable(&mut stmt, var)?;
    }
    Ok(())
}

fn export_flow_and_arg_flow_vars(
    conn: &Connection,
    program: &Program,
    pag: &Pag,
    analysis: &AnalysisResult,
) -> Result<()> {
    let mut needed: FxHashSet<VarId> = FxHashSet::default();
    for edge in &analysis.arg_flow_edges {
        if let Some(v) = edge.actual_var {
            needed.insert(v);
        }
        needed.insert(edge.formal);
    }
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
            export_one_variable(&mut stmt, var)?;
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
) -> Result<()> {
    let mut nodes = conn.prepare_cached(
        "INSERT INTO flow_nodes (id, kind, label, detail, var_id, fn_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut edges = conn.prepare_cached(
        "INSERT INTO flow_edges (id, src_node, dst_node, kind) VALUES (?1, ?2, ?3, ?4)",
    )?;

    for node in &pag.nodes {
        let (kind, label, detail, var_id, fn_id) = match node.kind {
            PagNodeKind::Var(v) => match program.symbols.variable_by_id(v) {
                Some(var) => {
                    let storage = match var.storage {
                        StorageClass::Global => "global",
                        StorageClass::FileStatic => "file_static",
                        StorageClass::FnStatic => "fn_static",
                        StorageClass::Param => "param",
                        StorageClass::Local => "local",
                    };
                    let mut detail = format!("{storage} @{}", var.span.line);
                    if let Some(f) = var.fn_id {
                        detail.push_str(&format!(" in {}", program.symbols.function(f).name));
                    }
                    ("var", var.name.clone(), detail, Some(v), var.fn_id)
                }
                None => ("var", format!("var{}", v.0), String::new(), Some(v), None),
            },
            PagNodeKind::Loc(loc_id) => {
                let loc = &pag.locations[loc_id.0 as usize];
                let kind_str = match loc.kind {
                    LocKind::Global => "global",
                    LocKind::FileStatic => "file_static",
                    LocKind::FnStatic => "fn_static",
                    LocKind::Local => "local",
                    LocKind::Heap => "heap",
                    LocKind::Field => "field",
                    LocKind::FieldSummary => "field_summary",
                    LocKind::ArraySummary => "array_summary",
                    LocKind::Function => "function",
                    LocKind::StringLit => "string_lit",
                };
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
                    site.callee_name.clone(),
                    format!("call @{}", site.span.line),
                    None,
                    None,
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
            kind,
            label,
            detail,
            var_id.map(|v| v.0),
            fn_id.map(|f| f.0)
        ])?;
    }

    let mut edge_rows: Vec<(u32, u32, &'static str)> = Vec::new();
    for c in &pag.constraints {
        let kind = match c.kind {
            ConstraintKind::Copy => "copy",
            ConstraintKind::AddrOf => "addr_of",
            ConstraintKind::Load => "load",
            ConstraintKind::Store => "store",
            ConstraintKind::Gep => "gep",
            ConstraintKind::Dlsym => "dlsym",
        };
        edge_rows.push((c.src.0, c.dst.0, kind));
    }
    // Implicit var → storage-location edges mirror the solver's direct
    // points-to seeding (no constraint exists for it in the PAG).
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
    for e in &analysis.arg_flow_edges {
        let Some(formal_node) = pag.var_node.get(&e.formal) else {
            continue;
        };
        match (e.actual_var, e.actual_fn) {
            (Some(actual), _) => {
                let Some(actual_node) = pag.var_node.get(&actual) else {
                    continue;
                };
                if wired_pairs.contains(&(actual_node.0, formal_node.0)) {
                    continue;
                }
                edge_rows.push((actual_node.0, formal_node.0, "call_arg"));
            }
            (None, Some(actual_fn)) => {
                if let Some(&fn_loc) = pag.fn_locations.get(&actual_fn) {
                    if let Some(&fn_node) = pag.loc_node.get(&fn_loc) {
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
        let Some(&actual) = site
            .var_args
            .iter()
            .find(|(j, _)| *j == param)
            .map(|(_, v)| v)
        else {
            continue;
        };
        let Some(&actual_node) = pag.var_node.get(&actual) else {
            continue;
        };
        let term_node = next_node_id;
        next_node_id += 1;
        nodes.execute(params![
            term_node,
            "terminator",
            format!("{} clears arg{param}", site.callee_name),
            format!(
                "call @{} in {}",
                site.span.line,
                program.symbols.function(site.caller).name
            ),
            Option::<i64>::None,
            Some(site.caller.0 as i64),
        ])?;
        edge_rows.push((actual_node.0, term_node as u32, "terminates"));
    }
    edge_rows.sort_unstable();
    edge_rows.dedup();
    for (i, (src, dst, kind)) in edge_rows.iter().enumerate() {
        edges.execute(params![i as i64 + 1, src, dst, kind])?;
    }
    Ok(())
}

fn export_one_variable(stmt: &mut rusqlite::Statement<'_>, var: &trace_ir::Variable) -> Result<()> {
    let kind = match var.storage {
        StorageClass::Global => "global",
        StorageClass::FileStatic => "file_static",
        StorageClass::FnStatic => "fn_static",
        StorageClass::Param => "param",
        StorageClass::Local => "local",
    };
    stmt.execute(params![
        var.id.0,
        var.name,
        kind,
        var.fn_id.map(|f| f.0),
        var.type_id.0,
        var.span.file.0,
        var.span.line,
        var.span.col,
        var.is_weak,
        var.target.map(|target| target.0),
    ])?;
    Ok(())
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
        stmt.execute(params![
            func.id.0,
            func.name,
            func.file.0,
            func.span.line,
            func.end_line.max(func.span.line),
            linkage,
            format!("fn_{}", func.name),
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
) -> Result<()> {
    let mut with_edge = FxHashSet::default();
    for edge in &analysis.call_edges {
        with_edge.insert(edge.call_site);
    }
    let mut with_arg_flow = FxHashSet::default();
    for edge in &analysis.arg_flow_edges {
        with_arg_flow.insert(edge.call_site);
    }

    let mut stmt = conn.prepare_cached(
        "INSERT INTO call_sites (id, caller_fn_id, file_id, line, col, callee_text, is_direct) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for cs in &program.symbols.call_sites {
        let export = with_edge.contains(&cs.id) || with_arg_flow.contains(&cs.id) || !cs.is_direct;
        if !export {
            continue;
        }
        stmt.execute(params![
            cs.id.0,
            cs.caller.0,
            cs.span.file.0,
            cs.span.line,
            cs.span.col,
            cs.callee_name,
            cs.is_direct as i32
        ])?;
    }
    Ok(())
}

fn export_call_edges(conn: &Connection, analysis: &AnalysisResult) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO call_edges (id, call_site_id, caller_fn_id, callee_fn_id, resolution) VALUES (?1, ?2, ?3, ?4, ?5)",
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
            resolution
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

fn export_diagnostics(conn: &Connection, program: &Program) -> Result<()> {
    if program.diagnostics.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare_cached(
        "INSERT INTO diagnostics (id, severity, file_id, line, message, stage) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (i, d) in program.diagnostics.iter().enumerate() {
        let severity = match d.severity {
            trace_ir::DiagnosticSeverity::Error => "error",
            trace_ir::DiagnosticSeverity::Warning => "warning",
            trace_ir::DiagnosticSeverity::Info => "info",
        };
        stmt.execute(params![
            i as i64 + 1,
            severity,
            d.file.map(|f| f.0),
            d.line,
            d.message,
            d.stage
        ])?;
    }
    Ok(())
}

pub fn open_db(path: &Path) -> Result<Connection> {
    Connection::open(path).with_context(|| format!("failed to open db at {}", path.display()))
}

#[cfg(test)]
mod tests {
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
        conn.execute_batch(TABLES_V5).unwrap();
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
                default_args: 0,
                is_virtual: false,
                is_final: false,
                is_cpp: false,
                tu: None,
            });
            let id = program.symbols.alloc_var_id();
            program.symbols.add_variable(Variable {
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
            });
        }
        let pag = Pag::build(&program);
        for full_detail in [false, true] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
            conn.execute_batch(TABLES_V5).unwrap();
            export_functions(&conn, &program).unwrap();
            if full_detail {
                export_variables(&conn, &program).unwrap();
            } else {
                export_flow_and_arg_flow_vars(&conn, &program, &pag, &AnalysisResult::default())
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
