//! `trace_index`: run the whole analyze pipeline against a project directory
//! and write the result to a SQLite database.

use crate::types::{TraceIndexOptions, TraceIndexResult, TraceStatus};
use crate::util::{guard, set_error, ApiError};
use std::ffi::{c_char, c_int};
use std::path::PathBuf;
use std::sync::Arc;
use trace_analysis::{analyze_with_options, AnalyzeOptions, FnModelSet};
use trace_db::{export_to_sqlite, ExportOptions};
use trace_parse::build_program_with_jobs;
use trace_preproc::PreprocessOptions;

/// Rust-owned copy of the C options, so no borrowed C pointer outlives the
/// call.
struct IndexConfig {
    root: PathBuf,
    output: PathBuf,
    includes: Vec<PathBuf>,
    defines: Vec<(String, String)>,
    jobs: usize,
    full_export: bool,
    debug_points_to: bool,
    models: Vec<PathBuf>,
}

unsafe fn read_config(opts: &TraceIndexOptions) -> Result<IndexConfig, ApiError> {
    let root = crate::util::cstr(opts.root)?.to_owned();
    let output = crate::util::cstr(opts.output_db)?.to_owned();
    let includes = crate::util::str_array(opts.includes, opts.n_includes)?
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let defines_raw = crate::util::str_array(opts.defines, opts.n_defines)?;
    let mut defines = Vec::with_capacity(defines_raw.len());
    for def in defines_raw {
        if let Some((name, value)) = def.split_once('=') {
            defines.push((name.to_owned(), value.to_owned()));
        } else {
            defines.push((def.clone(), "1".to_owned()));
        }
    }
    let models = crate::util::str_array(opts.models, opts.n_models)?
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let jobs = if opts.jobs <= 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1)
    } else {
        opts.jobs as usize
    };

    Ok(IndexConfig {
        root: PathBuf::from(root),
        output: PathBuf::from(output),
        includes,
        defines,
        jobs,
        full_export: opts.full_export != 0,
        debug_points_to: opts.debug_points_to != 0,
        models,
    })
}

fn arg_err(msg: &str, out_err: *mut *mut c_char) -> i32 {
    unsafe { set_error(out_err, msg) };
    TraceStatus::TraceErrInvalidArg as i32
}

/// Fail fast with a clear `TRACE_ERR_IO` when the output database path
/// cannot be used, instead of running the whole pipeline first.
///
/// The probe mirrors `export_to_sqlite`'s I/O surface exactly: it creates
/// the parent directory and touches the temporary file the exporter will
/// write (`<out>.db.tmp`), never the final path. That means a destination the
/// exporter would accept is accepted here too — including a missing parent
/// directory (created, like the CLI does), a read-only file the exporter
/// would unlink-and-replace, or a dangling symlink at `output` — and one it
/// would reject is rejected before any parsing happens.
///
/// The probe is side-effect-free: the parent directory it creates is left
/// (the exporter will need it anyway) and the temp file it probes is
/// unlinked again, so a failed run does not yield a stray 0-byte file.
fn preflight_output(path: &std::path::Path) -> Result<(), ApiError> {
    let temp = path.with_extension("db.tmp");
    if let Some(parent) = temp.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ApiError::Io(format!(
                "cannot create output directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    let created = !temp.exists();
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&temp)
        .map_err(|e| {
            ApiError::Io(format!(
                "cannot open output database {}: {e}",
                path.display()
            ))
        })?;
    if created {
        let _ = std::fs::remove_file(&temp);
    }
    Ok(())
}

fn run_index(cfg: &IndexConfig) -> Result<TraceIndexResult, ApiError> {
    preflight_output(&cfg.output)?;
    let mut models = FnModelSet::builtin();
    for path in &cfg.models {
        let src = std::fs::read_to_string(path).map_err(|e| {
            ApiError::Analysis(format!(
                "failed to read models file {}: {e}",
                path.display()
            ))
        })?;
        models
            .merge_toml_str(&src)
            .map_err(|e| ApiError::Analysis(format!("{}: {e}", path.display())))?;
    }
    let models = Arc::new(models);

    let mut popts = PreprocessOptions::new();
    for inc in &cfg.includes {
        popts.include_paths.push(inc.clone());
    }
    for (name, value) in &cfg.defines {
        popts = popts.with_define(name, value);
    }

    let program =
        build_program_with_jobs(&cfg.root, &popts, cfg.jobs).map_err(ApiError::Analysis)?;
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: cfg.debug_points_to,
            models,
            ..AnalyzeOptions::default()
        },
    );
    let model_files: Vec<String> = cfg.models.iter().map(|p| p.display().to_string()).collect();
    export_to_sqlite(
        &program,
        &pag,
        &analysis,
        &ExportOptions {
            output: cfg.output.clone(),
            trace_version: crate::build_info::TRACE_VERSION.to_owned(),
            include_points_to: cfg.debug_points_to,
            full_detail: cfg.full_export,
            model_files,
        },
    )
    .map_err(ApiError::from)?;

    Ok(TraceIndexResult {
        files: program.symbols.files.len() as u64,
        functions: program.symbols.functions.len() as u64,
        call_edges: analysis.call_edges.len() as u64,
        arg_flow_edges: analysis.arg_flow_edges.len() as u64,
    })
}

/// Index a project directory (`opts.root`) into a SQLite database
/// (`opts.output_db`) and fill `out` with summary counters.
///
/// Returns `TRACE_OK` (0) on success. On failure returns a non-zero status
/// and, when `out_err` is non-null, sets it to a message the caller frees
/// with `trace_string_free`. `opts` and every string it references are
/// borrowed for the duration of the call only.
///
/// # Safety
///
/// `opts` and `out` must be valid for the duration of the call, and `opts`
/// must point to a `trace_index_options` at least as large as `size`
/// reports.
#[no_mangle]
pub unsafe extern "C" fn trace_index(
    opts: *const TraceIndexOptions,
    out: *mut TraceIndexResult,
    out_err: *mut *mut c_char,
) -> c_int {
    crate::util::reset_err(out_err);
    if opts.is_null() || out.is_null() {
        return arg_err("opts and out must not be null", out_err);
    }
    // Read the leading `size` field through the raw pointer and reject
    // undersized nonzero values BEFORE constructing a `&TraceIndexOptions`.
    // `&*opts` is only valid when `size >= sizeof(TraceIndexOptions)`; a
    // genuinely smaller allocation (as a legacy consumer provides) would make
    // that reference invalid even though this function only reads `size`.
    let size = unsafe { (*opts).size };
    let expected = std::mem::size_of::<TraceIndexOptions>();
    if size != 0 && size < expected {
        let msg = format!(
            "trace_index_options.size too small ({size} < {expected}); \
             consumer and library were built against different ABI versions"
        );
        unsafe { set_error(out_err, &msg) };
        return TraceStatus::TraceErrInvalidArg as c_int;
    }
    let cfg = match unsafe { read_config(&*opts) } {
        Ok(c) => c,
        // The remaining read_config failures are all invalid arguments
        // (null/overshort pointers, malformed arrays).
        Err(e) => {
            unsafe { set_error(out_err, e.message()) };
            return e.status();
        }
    };

    match guard(|| run_index(&cfg)) {
        Ok(result) => {
            unsafe { *out = result };
            TraceStatus::TraceOk as c_int
        }
        Err(err) => {
            unsafe { set_error(out_err, err.message()) };
            err.status()
        }
    }
}
