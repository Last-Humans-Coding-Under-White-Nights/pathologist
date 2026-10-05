//! Parse preprocessed C source into trace IR.

mod compile_commands;
mod compiler_includes;
mod cpp_type_names;
mod deps;
mod discover;
mod expansion_discovery;
pub mod explore;
pub mod gn_defines;
mod gn_targets;
mod idl;
mod index_cache;
mod link_commands;
mod lower;
mod memory;
mod merge;
mod node_metadata;
mod parse;
mod template_bases;

pub use deps::*;
pub use explore::*;
pub use gn_defines::{Candidate, Confidence};
pub use index_cache::{remove_spill_dir, IndexSourceCache};

pub use discover::*;
pub use lower::*;
pub use parse::*;
