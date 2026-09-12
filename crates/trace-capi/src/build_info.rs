/// Full build identity stamped into databases this library produces:
/// `0.1.0 (sha[-dirty] date)`. Set by `build.rs` (same logic as trace-cli).
pub const TRACE_VERSION: &str = env!("TRACE_BUILD_VERSION");
