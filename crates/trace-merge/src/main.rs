use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use trace_merge::{merge_databases, MergeOptions, WarningKind};

#[derive(Parser, Debug)]
#[command(
    name = "trace-merge",
    version = env!("CARGO_PKG_VERSION"),
    about = "Merge multiple trace analysis databases and reconstruct cross-repository callgraph"
)]
struct Cli {
    /// Input SQLite databases from `trace analyze`.
    #[arg(required = true)]
    inputs: Vec<PathBuf>,

    /// Output merged SQLite database path [default: unified.db].
    #[arg(short, long, default_value = "unified.db")]
    output: PathBuf,

    /// Print detailed information about all cross-repo diagnostics.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    eprintln!(
        "trace-merge: merging {} database(s) into {}...",
        cli.inputs.len(),
        cli.output.display()
    );

    let report = merge_databases(
        &cli.inputs,
        &MergeOptions {
            output: cli.output.clone(),
            verbose: cli.verbose,
        },
    )?;

    // Report problems and warnings to the user
    if !report.warnings.is_empty() {
        let mut collisions = Vec::new();
        let mut unresolved = Vec::new();
        let mut weak_overrides = Vec::new();
        let mut duplicates = Vec::new();
        let mut without_contexts = Vec::new();

        for w in &report.warnings {
            match w.kind {
                WarningKind::MultipleDefinitions => collisions.push(w),
                WarningKind::UnresolvedExternal => unresolved.push(w),
                WarningKind::WeakOverride => weak_overrides.push(w),
                WarningKind::DuplicateInput => duplicates.push(w),
                WarningKind::MissingExecutionContexts => without_contexts.push(w),
            }
        }

        eprintln!(
            "\nwarning: merge diagnostics: {} collision(s), {} unresolved external symbol(s), {} weak override(s)",
            collisions.len(),
            unresolved.len(),
            weak_overrides.len()
        );

        // Always display multiple definition collisions in full: they are critical ODR / linking conflicts.
        if !collisions.is_empty() {
            eprintln!("  Collisions (multiple strong definitions):");
            for (i, w) in collisions.iter().enumerate() {
                eprintln!("    {}. [collision] {}", i + 1, w.message);
            }
        }

        if !duplicates.is_empty() {
            for w in &duplicates {
                eprintln!("    [duplicate-input] {}", w.message);
            }
        }

        for w in &without_contexts {
            eprintln!("    [no-execution-contexts] {}", w.message);
        }

        if cli.verbose {
            if !unresolved.is_empty() {
                eprintln!("  Unresolved external functions:");
                for (i, w) in unresolved.iter().enumerate() {
                    eprintln!("    {}. [unresolved] {}", i + 1, w.message);
                }
            }
            if !weak_overrides.is_empty() {
                eprintln!("  Weak symbol overrides:");
                for (i, w) in weak_overrides.iter().enumerate() {
                    eprintln!("    {}. [weak-override] {}", i + 1, w.message);
                }
            }
        } else if !unresolved.is_empty() || !weak_overrides.is_empty() {
            eprintln!(
                "  (pass --verbose to see detailed locations for all {} diagnostics)",
                report.warnings.len()
            );
        }
    }

    eprintln!(
        "\nmerge complete: \
         {} databases -> {} \
         ({} files, {} functions [{} defined, {} decls], {} call edges \
         [{} cross-repo resolved, {} unresolved external, {} ambiguous])",
        report.input_dbs.len(),
        cli.output.display(),
        report.files_total,
        report.functions_total,
        report.functions_defined,
        report.functions_declarations,
        report.call_edges_total,
        report.cross_repo_calls_resolved,
        report.external_calls_unresolved,
        report.ambiguous_calls,
    );

    Ok(())
}
