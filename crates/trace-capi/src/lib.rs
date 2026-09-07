//! C FFI surface for trace.
//!
//! Exposes the whole-program index (analyze) pipeline plus the inspect
//! queries (functions/symbols/call graph/dataflow) to C consumers behind a
//! small, ownership-disciplined ABI. See `include/trace.h` for the header
//! contract and `docs/CAPI.md` for the design.
//!
//! # Memory-safety rules (for consumers)
//!
//! - Handles (`trace_db`, result `_impl` fields) are opaque; only the
//!   matching `trace_*_free`/`trace_*_close` function accepts them.
//! - Every string a query returns is owned by the result object it lives in
//!   (an append-only arena). It is valid until the result is freed, then
//!   gone with it — there is no query-to-query invalidation.
//! - All inputs (`const char*`, option structs, symbol arrays) are borrowed;
//!   they are copied into Rust-owned memory during the call.
//! - No panic ever crosses the boundary; panics are caught and surfaced as
//!   errors.
//! - Each handle is single-threaded.

mod build_info;
mod index;
mod inspect;
mod types;
mod util;

pub use types::*;

pub use index::{trace_index, trace_index_ext};
pub use inspect::*;
pub use util::*;
