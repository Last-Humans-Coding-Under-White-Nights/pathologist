use std::path::{Path, PathBuf};

/// Maps output byte offsets back to original source locations.
///
/// File paths are interned in [`LineMap::files`]; entries store the index so
/// per-token recording stays allocation-free and cache-friendly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineMap {
    /// Interned origin paths; entry `file` indexes into this vec.
    pub files: Vec<PathBuf>,
    pub entries: Vec<LineMapEntry>,
}

/// One mapping: byte offset in preprocessed output → original location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineMapEntry {
    pub output_offset: u32,
    pub file: u32,
    pub line: u32,
    pub col: u32,
}

impl LineMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern a path, returning its entry index.
    pub fn intern_file(&mut self, path: &Path) -> u32 {
        if let Some(pos) = self.files.iter().position(|p| p == path) {
            return pos as u32;
        }
        self.files.push(path.to_path_buf());
        (self.files.len() - 1) as u32
    }

    pub fn push(&mut self, output_offset: usize, file: u32, line: u32, col: u32) {
        self.entries.push(LineMapEntry {
            output_offset: output_offset as u32,
            file,
            line,
            col,
        });
    }

    #[must_use]
    pub fn lookup(&self, output_offset: usize) -> Option<&LineMapEntry> {
        // Entries are pushed with non-decreasing output offsets.
        let idx = self
            .entries
            .partition_point(|e| e.output_offset as usize <= output_offset);
        if idx == 0 {
            return None;
        }
        self.entries.get(idx - 1)
    }

    /// Original path for an entry.
    #[must_use]
    pub fn path_of(&self, entry: &LineMapEntry) -> &Path {
        &self.files[entry.file as usize]
    }

    #[must_use]
    pub fn lookup_line(&self, _output_line: u32) -> Option<&LineMapEntry> {
        // Approximate: find last entry before this line
        self.entries.last()
    }

    /// Entries at or after `start`, re-based so `start` becomes offset 0 and
    /// with the file table reduced to the files actually referenced.
    #[must_use]
    pub fn slice_from(&self, start: usize) -> LineMap {
        let idx = self
            .entries
            .partition_point(|e| (e.output_offset as usize) < start);
        let mut out = LineMap::new();
        let mut remap = vec![u32::MAX; self.files.len()];
        for e in &self.entries[idx..] {
            if remap[e.file as usize] == u32::MAX {
                remap[e.file as usize] = out.intern_file(&self.files[e.file as usize]);
            }
        }
        let entries = self.entries[idx..]
            .iter()
            .map(|e| LineMapEntry {
                output_offset: e.output_offset - start as u32,
                file: remap[e.file as usize],
                line: e.line,
                col: e.col,
            })
            .collect();
        out.entries = entries;
        out
    }

    /// Drop mappings whose output offset is at or after `at`.
    pub fn truncate_at(&mut self, at: usize) {
        let at = at as u32;
        self.entries.retain(|e| e.output_offset < at);
    }

    /// Append `other`'s entries shifted by `offset`, renumbering its file
    /// indices through `remap` (indexed by `other`'s file table).
    pub fn splice(&mut self, other: &LineMap, offset: usize, remap: &[u32]) {
        for e in &other.entries {
            self.entries.push(LineMapEntry {
                output_offset: e.output_offset + offset as u32,
                file: remap[e.file as usize],
                line: e.line,
                col: e.col,
            });
        }
    }
}
