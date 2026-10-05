use crate::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Span {
    pub file: FileId,
    pub line: u32,
    pub col: u32,
}

impl Span {
    pub fn new(file: FileId, line: u32, col: u32) -> Self {
        Self { file, line, col }
    }
}
