use clap::Parser;
use std::path::PathBuf;
use trace_lsp::{locations::PathMapping, transport, Server};

#[derive(Parser)]
#[command(
    version,
    about = "Read-only LSP call hierarchy for a trace SQLite analysis snapshot"
)]
struct Args {
    /// Existing analysis database (minimal or full export, schema v7).
    #[arg(long)]
    db: PathBuf,
    /// Remap an absolute database source prefix to an absolute local prefix.
    /// Repeatable; the longest matching component prefix wins.
    #[arg(long, value_name = "DB_PREFIX=LOCAL_PREFIX")]
    path_map: Vec<PathMapping>,
}

fn main() {
    let args = Args::parse();
    let result = Server::open(&args.db, &args.path_map).and_then(|mut server| {
        transport::serve(
            &mut server,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
        )
    });
    match result {
        Ok(status) => std::process::exit(status),
        Err(error) => {
            eprintln!("trace-lsp: {error:#}");
            std::process::exit(1);
        }
    }
}
