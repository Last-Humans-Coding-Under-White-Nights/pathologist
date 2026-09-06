use crate::schema::{SCHEMA_V3, SCHEMA_VERSION};
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
        conn.execute_batch(SCHEMA_V3)?;

        let options_json = serde_json::json!({
            "include_paths": program.include_paths,
            "defines": program.defines,
            "include_points_to": opts.include_points_to,
            "full_detail": opts.full_detail,
            "model_files": opts.model_files,
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
    let mut stmt =
        conn.prepare_cached("INSERT INTO files (id, path, sha256) VALUES (?1, ?2, ?3)")?;
    for file in &program.symbols.files {
        stmt.execute(params![file.id.0, file.path.display().to_string(), ""])?;
    }
    Ok(())
}

fn export_types(conn: &Connection, program: &Program) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO types (id, kind, name, size, layout_json) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for ty in program.types.all() {
        let kind = match &ty.desc {
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

fn export_variables(conn: &Connection, program: &Program) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO variables (id, name, kind, fn_id, type_id, file_id, line, col) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
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
    let mut stmt = conn.prepare_cached(
        "INSERT INTO variables (id, name, kind, fn_id, type_id, file_id, line, col) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
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
        var.span.col
    ])?;
    Ok(())
}

fn export_functions(conn: &Connection, program: &Program) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO functions (id, name, file_id, line_start, line_end, linkage, signature, is_defined) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
            func.is_defined as i32
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
