//! Parse preprocessed C source into trace IR.

mod deps;
mod discover;
pub mod explore;
pub mod gn_defines;
mod index_cache;
mod lower;
mod merge;
mod parse;

pub use deps::*;
pub use explore::*;
pub use gn_defines::{Candidate, Confidence};
pub use index_cache::IndexSourceCache;

pub use discover::*;
pub use lower::*;
pub use parse::*;
