//! SQLite export for trace analysis results.

/// Database row ids the read APIs name, each typed by its table so one
/// cannot stand for another; they serialize as the bare id.
macro_rules! row_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl rusqlite::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                self.0.to_sql()
            }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(
                value: rusqlite::types::ValueRef<'_>,
            ) -> rusqlite::types::FromSqlResult<Self> {
                i64::column_result(value).map(Self)
            }
        }
    };
}

mod export;
mod filter;
mod identity;
mod inspect;
mod linking;
mod render;
mod schema;
mod text;

pub use export::*;
pub use filter::filter_call_chains;
pub use filter::filter_query_graph;
pub use filter::CallGraphFilter;
pub use identity::{database_identity, record_database_identity, DatabaseIdentity};
pub use inspect::*;
pub use linking::{read_linking_facts, write_linking_facts};
pub use render::*;
pub use schema::*;
pub use text::{TextId, TextPool};

mod dataflow;
pub use dataflow::*;
/// The multi-instance evidence ranking `execution_contexts.multi_instance`
/// stores, for readers of an exported database.
pub use trace_analysis::MultiInstance;
