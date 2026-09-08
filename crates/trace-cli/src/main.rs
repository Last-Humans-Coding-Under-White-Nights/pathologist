use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Instant;
use trace_analysis::{analyze_with_options, AnalyzeOptions, ResolutionKind};
use trace_db::{basename, export_to_sqlite, open_db, ExportOptions};
use trace_parse::build_program_with_jobs;
use trace_preproc::PreprocessOptions;

mod build_info;

#[cfg(test)]
#[path = "../build_support.rs"]
mod build_support;

#[derive(Parser)]
#[command(
    name = "trace",
    version = build_info::TRACE_VERSION,
    about = "C call graph and pointer analysis tool"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Analyze a C project directory and write results to SQLite.
    Analyze {
        /// Target project directory containing .c files.
        target: PathBuf,
        /// Output SQLite database path.
        #[arg(short, long, default_value = "trace.db")]
        output: PathBuf,
        /// Add include search path (repeatable).
        #[arg(long = "include")]
        includes: Vec<PathBuf>,
        /// Define preprocessor macro NAME or NAME=VALUE (repeatable).
        #[arg(short = 'D')]
        defines: Vec<String>,
        /// Number of parallel jobs for indexing (parse/lower).
        #[arg(long)]
        jobs: Option<usize>,
        /// Abort the whole analyze process after N seconds (watchdog).
        #[arg(long)]
        timeout_secs: Option<u64>,
        /// Include points-to debug table in output (also retains points-to in memory during analysis).
        #[arg(long)]
        debug_points_to: bool,
        /// Disable IPC proxy/stub bridge edge detection (enabled by default).
        #[arg(long)]
        no_ipc: bool,
        /// Export full IR detail (types, all variables, PAG locations). Default: call graph + arg-flow only.
        #[arg(long)]
        full_export: bool,
        /// Function-model TOML file (repeatable; overrides built-ins by name).
        #[arg(long = "models")]
        models: Vec<PathBuf>,
    },
    /// Inspect an existing analysis database.
    Inspect {
        /// Path to SQLite database.
        db: PathBuf,
        #[command(subcommand)]
        command: InspectCommands,
    },
}

#[derive(Subcommand)]
enum InspectCommands {
    /// List call graph edges.
    Calls {
        /// Filter edges whose caller name equals FN or ends with `::FN`
        /// (C++ qualified methods: `--from OnEventProxy` matches
        /// `ns::Plugin::OnEventProxy`).
        #[arg(long)]
        from: Option<String>,
        /// Filter edges whose callee name equals FN or ends with `::FN`.
        #[arg(long)]
        to: Option<String>,
        /// Only edges whose call-site or callee file path contains this substring.
        /// For a synthetic edge with no call site, match its caller definition file.
        #[arg(long)]
        file: Option<String>,
        /// JSON config file listing regex patterns for function names to keep.
        /// An edge is shown when its caller or callee matches any pattern.
        #[arg(long = "callgraph-filter")]
        callgraph_filter: Option<PathBuf>,
    },
    /// Call graph around the function containing FILE:LINE.
    ///
    /// `--direction down` follows callees, `up` follows callers.
    Callgraph {
        /// File path substring (e.g. basename) locating the start function.
        #[arg(long)]
        file: String,
        /// Line inside the start function (start <= line <= end).
        #[arg(long)]
        line: i64,
        /// Traversal depth limit.
        #[arg(long, default_value_t = 3)]
        depth: u32,
        /// Traversal direction: `down` (callees) or `up` (callers).
        #[arg(long, default_value = "down")]
        direction: String,
        /// Graph output format: `text`, `json`, `graphviz`, or `mermaid`.
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
        /// JSON config file listing regex patterns for function names to keep.
        /// Only edges whose caller or callee matches are shown.
        #[arg(long = "callgraph-filter")]
        callgraph_filter: Option<PathBuf>,
    },
    /// Value-flow (dataflow) graph for the variable declared at FILE:LINE:COL.
    ///
    /// Lookup matches declarations; with several candidates on the line the
    /// one covering COL wins, else the nearest column is used.
    Dataflow {
        /// File path substring (e.g. basename) locating the symbol.
        #[arg(long)]
        file: String,
        /// Declaration line of the symbol.
        #[arg(long)]
        line: i64,
        /// Column of the symbol (1-based); disambiguates same-line symbols.
        #[arg(long)]
        col: i64,
        /// Traversal depth limit.
        #[arg(long, default_value_t = 3)]
        depth: u32,
        /// Traversal direction: `down` (where values flow) or `up`
        /// (where they come from).
        #[arg(long, default_value = "down")]
        direction: String,
        /// Graph output format: `text`, `json`, `graphviz`, or `mermaid`.
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
    },
    /// Call chains (paths) between two functions no longer than depth.
    #[command(alias = "chains")]
    Callchain {
        /// Start function name, C++ qualified suffix, or FILE:LINE (e.g. `main` or `main.c:10`).
        #[arg(long)]
        from: Option<String>,
        /// Target function name, C++ qualified suffix, or FILE:LINE (e.g. `target` or `main.c:20`).
        #[arg(long)]
        to: Option<String>,
        /// File path substring locating the start function.
        #[arg(long = "from-file")]
        from_file: Option<String>,
        /// Line inside the start function.
        #[arg(long = "from-line")]
        from_line: Option<i64>,
        /// File path substring locating the target function.
        #[arg(long = "to-file")]
        to_file: Option<String>,
        /// Line inside the target function.
        #[arg(long = "to-line")]
        to_line: Option<i64>,
        /// Maximum traversal depth (path length in call hops).
        #[arg(long, default_value_t = 5)]
        depth: u32,
        /// Traversal direction: `down` (callees) or `up` (callers).
        #[arg(long, default_value = "down")]
        direction: String,
        /// Maximum number of chains to return (0 for unlimited).
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Graph output format: `text`, `json`, `graphviz`, or `mermaid`.
        #[arg(long, value_enum, default_value = "text")]
        format: OutputFormat,
        /// JSON config file listing regex patterns for function names to keep.
        #[arg(long = "callgraph-filter")]
        callgraph_filter: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    Graphviz,
    Mermaid,
}

impl OutputFormat {
    fn to_render(self) -> trace_db::RenderFormat {
        match self {
            OutputFormat::Text => trace_db::RenderFormat::Text,
            OutputFormat::Json => trace_db::RenderFormat::Json,
            OutputFormat::Graphviz => trace_db::RenderFormat::Graphviz,
            OutputFormat::Mermaid => trace_db::RenderFormat::Mermaid,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Analyze {
            target,
            output,
            includes,
            defines,
            jobs,
            timeout_secs,
            debug_points_to,
            full_export,
            models,
            no_ipc,
        } => run_analyze(
            target,
            output,
            includes,
            defines,
            jobs,
            timeout_secs,
            debug_points_to,
            full_export,
            models,
            no_ipc,
        ),
        Commands::Inspect { db, command } => run_inspect(db, command),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_analyze(
    target: PathBuf,
    output: PathBuf,
    includes: Vec<PathBuf>,
    defines: Vec<String>,
    jobs: Option<usize>,
    timeout_secs: Option<u64>,
    debug_points_to: bool,
    full_export: bool,
    model_files: Vec<PathBuf>,
    no_ipc: bool,
) -> Result<()> {
    if let Some(secs) = timeout_secs {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            eprintln!("error: timed out after {secs}s");
            std::process::exit(124);
        });
    }
    let jobs = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1)
    });
    let mut models = trace_analysis::FnModelSet::builtin();
    for path in &model_files {
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read models file {}", path.display()))?;
        models
            .merge_toml_str(&src)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    }
    let models = std::sync::Arc::new(models);
    if model_files.is_empty() {
        eprintln!(
            "models: built-in ({} functions); add --models <file.toml> for project-specific summaries",
            models.len()
        );
    } else {
        eprintln!(
            "models: {} functions (built-ins + {} config file(s))",
            models.len(),
            model_files.len()
        );
    }
    let mut opts = PreprocessOptions::new();
    for inc in includes {
        opts.include_paths.push(inc);
    }
    for def in defines {
        if let Some((name, value)) = def.split_once('=') {
            opts = opts.with_define(name, value);
        } else {
            opts = opts.with_define(def, "1");
        }
    }

    // Include paths pointing outside the analyzed tree make twin headers
    // (same basename, different tree) resolve to the wrong copy, which
    // silently starves translation units. Warn loudly — this misconfiguration
    // previously produced silent false negatives.
    // The C API repeats this check (`outside_root_warning`, trace-capi);
    // keep the containment predicate in step with it.
    let root_canon = trace_ir::canonicalize(&target);
    let outside: Vec<PathBuf> = opts
        .include_paths
        .iter()
        .map(|p| trace_ir::canonicalize(p))
        .filter(|c| !(c.starts_with(&root_canon) || root_canon.starts_with(c)))
        .collect();
    if !outside.is_empty() {
        eprintln!(
            "warning: {} include path(s) lie outside the analysis tree {};",
            outside.len(),
            root_canon.display()
        );
        eprintln!("         headers may resolve to twins in another tree and lose definitions:");
        for p in outside.iter().take(5) {
            eprintln!("           {}", p.display());
        }
        if outside.len() > 5 {
            eprintln!("           ... and {} more", outside.len() - 5);
        }
    }

    let t0 = Instant::now();
    let program = build_program_with_jobs(&target, &opts, jobs).map_err(|e| anyhow::anyhow!(e))?;
    eprintln!(
        "index: {:.1}s ({} files, {} functions, {} flow)",
        t0.elapsed().as_secs_f64(),
        program.symbols.files.len(),
        program.symbols.functions.len(),
        program.flow.len(),
    );

    let t1 = Instant::now();
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: debug_points_to,
            models,
            solve_budget: Some(800_000),
            enable_ipc: !no_ipc,
        },
    );
    let indirect = analysis
        .call_edges
        .iter()
        .filter(|e| e.resolution == ResolutionKind::Indirect)
        .count();
    eprintln!(
        "analyze: {:.1}s ({} edges, {} indirect)",
        t1.elapsed().as_secs_f64(),
        analysis.call_edges.len(),
        indirect,
    );

    let t2 = Instant::now();
    export_to_sqlite(
        &program,
        &pag,
        &analysis,
        &ExportOptions {
            output: output.clone(),
            trace_version: build_info::TRACE_VERSION.to_owned(),
            include_points_to: debug_points_to,
            full_detail: full_export,
            model_files: model_files
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
        },
    )
    .with_context(|| format!("failed to export to {}", output.display()))?;
    eprintln!("export: {:.1}s", t2.elapsed().as_secs_f64());

    let mut direct_edges = 0usize;
    let mut indirect_edges = 0usize;
    let mut external_edges = 0usize;
    let mut ipc_edges = 0usize;
    for e in &analysis.call_edges {
        match e.resolution {
            // Ambiguous groups with direct in the summary: both are
            // statically-name-resolved; ambiguity only means several
            // same-name candidates, not pointer indirection.
            trace_analysis::ResolutionKind::Direct | trace_analysis::ResolutionKind::Ambiguous => {
                direct_edges += 1
            }
            trace_analysis::ResolutionKind::Indirect => indirect_edges += 1,
            trace_analysis::ResolutionKind::External => external_edges += 1,
            trace_analysis::ResolutionKind::IpcBridge => ipc_edges += 1,
        }
    }
    eprintln!(
        "analysis complete: {} functions ({} external), {} call edges ({} direct, {} indirect, {} external, {} ipc), {} arg-flow edges -> {}",
        program.symbols.functions.len(),
        program
            .symbols
            .functions
            .iter()
            .filter(|f| !f.is_defined)
            .count(),
        analysis.call_edges.len(),
        direct_edges,
        indirect_edges,
        external_edges,
        ipc_edges,
        analysis.arg_flow_edges.len(),
        output.display()
    );
    Ok(())
}

fn run_inspect(db: PathBuf, command: InspectCommands) -> Result<()> {
    let conn = open_db(&db)?;
    match command {
        InspectCommands::Calls {
            from,
            to,
            file,
            callgraph_filter,
        } => {
            let edges = trace_db::call_edges(
                &conn,
                &trace_db::CallEdgeFilter {
                    from: from.as_deref(),
                    to: to.as_deref(),
                    file: file.as_deref(),
                },
            )?;
            let filter = match callgraph_filter {
                Some(p) => Some(trace_db::CallGraphFilter::from_file(&p)?),
                None => None,
            };
            for e in edges {
                if let Some(f) = &filter {
                    if !f.matches(&e.caller_name) && !f.matches(&e.callee_name) {
                        continue;
                    }
                }
                match (e.call_site_path, e.call_site_line) {
                    // Real call sites.
                    (Some(cf), Some(l)) => println!(
                        "{caller} ({basename_of_call_site}:{l}) -> {callee} [{basename_of_callee}] \
                         ({res})",
                        basename_of_call_site = basename(&cf),
                        callee = e.callee_name,
                        basename_of_callee = basename(&e.callee_path),
                        res = e.resolution,
                        caller = e.caller_name,
                    ),
                    // Synthetic IPC bridge edges have no source call site.
                    _ => println!(
                        "{caller} -> {callee} [{basename_of_callee}] ({res})",
                        callee = e.callee_name,
                        basename_of_callee = basename(&e.callee_path),
                        res = e.resolution,
                        caller = e.caller_name,
                    ),
                }
            }
        }
        InspectCommands::Callgraph {
            file,
            line,
            depth,
            direction,
            format,
            callgraph_filter,
        } => {
            let dir = trace_db::Direction::parse(&direction)?;
            if depth == 0 {
                anyhow::bail!("depth must be >= 1");
            }
            let start = trace_db::require_function_at(&conn, &file, line)?;
            let mut graph = trace_db::call_graph(&conn, start.id, dir, depth)?;
            if let Some(p) = callgraph_filter {
                let filter = trace_db::CallGraphFilter::from_file(&p)?;
                trace_db::filter_query_graph(&mut graph, &filter);
            }
            let dir_word = match dir {
                trace_db::Direction::Down => "callees",
                trace_db::Direction::Up => "callers",
            };
            let meta = trace_db::GraphMeta {
                title: &format!("callgraph from {start} ({dir_word}, depth {depth}):"),
                direction: dir_word,
                depth,
                summary: &format!(
                    "{} functions, {} edges",
                    graph.nodes.len(),
                    graph.edges.len()
                ),
            };
            let out =
                trace_db::render_graph(
                    &graph,
                    format.to_render(),
                    &meta,
                    &mut |id, out| match graph.nodes.get(&id) {
                        Some(n) => out.push_str(&format!(
                            "{} ({})",
                            n.label,
                            if n.detail.is_empty() { "?" } else { &n.detail }
                        )),
                        None => out.push_str(&format!("fn{id}")),
                    },
                );
            print!("{out}");
        }
        InspectCommands::Dataflow {
            file,
            line,
            col,
            depth,
            direction,
            format,
        } => {
            let dir = trace_db::Direction::parse(&direction)?;
            if depth == 0 {
                anyhow::bail!("depth must be >= 1");
            }
            let cands = trace_db::require_symbols_at(&conn, &file, line, col)?;
            let best = &cands[0];
            let exact =
                best.line == line && col >= best.col && col <= best.col + best.name.len() as i64;
            if !exact {
                eprintln!(
                    "note: no declaration exactly at {file}:{line}:{col}; using {}",
                    best
                );
            } else if cands.len() > 1 {
                let mut others: Vec<String> = cands[1..].iter().map(|s| s.name.clone()).collect();
                others.dedup();
                let shown = if others.len() > 5 {
                    format!("{} … (+{} more)", others[..5].join(", "), others.len() - 5)
                } else {
                    others.join(", ")
                };
                eprintln!(
                    "note: {} candidates on this line; using {} (others: {})",
                    cands.len(),
                    best.name,
                    shown
                );
            }
            let graph = trace_db::dataflow_graph(&conn, std::slice::from_ref(best), dir, depth)?;
            let dir_word = match dir {
                trace_db::Direction::Down => "flows-to",
                trace_db::Direction::Up => "flows-from",
            };
            let meta = trace_db::GraphMeta {
                title: &format!("dataflow for {best} ({dir_word}, depth {depth}):"),
                direction: dir_word,
                depth,
                summary: &format!(
                    "{} flow nodes, {} flow edges",
                    graph.nodes.len(),
                    graph.edges.len()
                ),
            };
            let out =
                trace_db::render_graph(
                    &graph,
                    format.to_render(),
                    &meta,
                    &mut |id, out| match graph.nodes.get(&id) {
                        Some(n) => {
                            out.push_str(&n.label);
                            if !n.detail.is_empty() {
                                out.push_str(&format!(" ({})", n.detail));
                            }
                        }
                        None => out.push_str(&format!("node{id}")),
                    },
                );
            print!("{out}");
        }
        InspectCommands::Callchain {
            from,
            to,
            from_file,
            from_line,
            to_file,
            to_line,
            depth,
            direction,
            limit,
            format,
            callgraph_filter,
        } => {
            let dir = trace_db::Direction::parse(&direction)?;
            let start = trace_db::resolve_function_target(
                &conn,
                from.as_deref(),
                from_file.as_deref(),
                from_line,
            )?;
            let target = trace_db::resolve_function_target(
                &conn,
                to.as_deref(),
                to_file.as_deref(),
                to_line,
            )?;
            let mut result = trace_db::call_chains(
                &conn,
                start.id,
                target.id,
                dir,
                depth,
                if limit == 0 { None } else { Some(limit) },
            )?;
            let labels = trace_db::load_function_labels(&conn)?;
            if let Some(p) = callgraph_filter {
                let filter = trace_db::CallGraphFilter::from_file(&p)?;
                trace_db::filter_call_chains(&mut result, &filter, &labels);
            }
            let out = trace_db::render_call_chains(
                &result,
                format.to_render(),
                &start,
                &target,
                dir,
                depth,
                &labels,
            );
            print!("{out}");
        }
    }
    Ok(())
}
