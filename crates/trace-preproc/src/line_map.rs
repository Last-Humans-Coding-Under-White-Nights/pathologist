use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};

/// Maps output byte offsets back to original source locations.
///
/// File paths are interned in [`LineMap::files`]; entries store the index so
/// per-token recording stays allocation-free and cache-friendly.
#[derive(Debug, Clone, Default)]
pub struct LineMap {
    /// Interned origin paths; entry `file` indexes into this vec.
    pub files: Vec<PathBuf>,
    /// Interned macro names; entry `expansion_macro` indexes into this vec.
    pub macros: Vec<String>,
    #[doc(hidden)]
    macro_indices: FxHashMap<String, u32>,
    pub entries: Vec<LineMapEntry>,
}

impl PartialEq for LineMap {
    fn eq(&self, other: &Self) -> bool {
        self.files == other.files && self.macros == other.macros && self.entries == other.entries
    }
}
impl Eq for LineMap {}

/// One mapping: byte offset in preprocessed output → original location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineMapEntry {
    pub output_offset: u32,
    pub file: u32,
    pub line: u32,
    pub col: u32,
    /// Invocation location for a token whose primary location is its spelling
    /// in a macro replacement list. `u32::MAX` means no expansion location.
    pub expansion_file: u32,
    pub expansion_line: u32,
    pub expansion_col: u32,
    /// Stable fingerprint of the full macro expansion/substitution chain.
    /// Zero denotes source text without macro provenance.
    pub expansion_id: u64,
    /// Interned macro name for the outermost macro invocation. `u32::MAX` means none.
    pub expansion_macro: u32,
}

impl LineMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_raw_parts(
        files: Vec<PathBuf>,
        macros: Vec<String>,
        entries: Vec<LineMapEntry>,
    ) -> Self {
        let mut macro_indices = FxHashMap::default();
        for (idx, m) in macros.iter().enumerate() {
            macro_indices.insert(m.clone(), idx as u32);
        }
        Self {
            files,
            macros,
            macro_indices,
            entries,
        }
    }

    /// Intern a path, returning its entry index.
    pub fn intern_file(&mut self, path: &Path) -> u32 {
        if let Some(pos) = self.files.iter().position(|p| p == path) {
            return pos as u32;
        }
        self.files.push(path.to_path_buf());
        (self.files.len() - 1) as u32
    }

    /// Intern a macro name, returning its entry index.
    pub fn intern_macro(&mut self, name: &str) -> u32 {
        if let Some(&pos) = self.macro_indices.get(name) {
            return pos;
        }
        if self.macro_indices.is_empty() && !self.macros.is_empty() {
            for (idx, m) in self.macros.iter().enumerate() {
                self.macro_indices.insert(m.clone(), idx as u32);
            }
            if let Some(&pos) = self.macro_indices.get(name) {
                return pos;
            }
        }
        let pos = self.macros.len() as u32;
        self.macros.push(name.to_string());
        self.macro_indices.insert(name.to_string(), pos);
        pos
    }

    pub fn push(&mut self, output_offset: usize, file: u32, line: u32, col: u32) {
        self.entries.push(LineMapEntry {
            output_offset: output_offset as u32,
            file,
            line,
            col,
            expansion_file: u32::MAX,
            expansion_line: 0,
            expansion_col: 0,
            expansion_id: 0,
            expansion_macro: u32::MAX,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn push_expansion(
        &mut self,
        output_offset: usize,
        file: u32,
        line: u32,
        col: u32,
        expansion_file: u32,
        expansion_line: u32,
        expansion_col: u32,
        expansion_id: u64,
        expansion_macro: u32,
    ) {
        self.entries.push(LineMapEntry {
            output_offset: output_offset as u32,
            file,
            line,
            col,
            expansion_file,
            expansion_line,
            expansion_col,
            expansion_id,
            expansion_macro,
        });
    }

    #[must_use]
    pub fn expansion_path_of(&self, entry: &LineMapEntry) -> Option<&Path> {
        (entry.expansion_file != u32::MAX)
            .then(|| self.files[entry.expansion_file as usize].as_path())
    }

    #[must_use]
    pub fn expansion_macro_of(&self, entry: &LineMapEntry) -> Option<&str> {
        (entry.expansion_macro != u32::MAX)
            .then(|| self.macros[entry.expansion_macro as usize].as_str())
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
        let mut macro_remap = vec![u32::MAX; self.macros.len()];
        for e in &self.entries[idx..] {
            if remap[e.file as usize] == u32::MAX {
                remap[e.file as usize] = out.intern_file(&self.files[e.file as usize]);
            }
            if e.expansion_macro != u32::MAX && macro_remap[e.expansion_macro as usize] == u32::MAX
            {
                macro_remap[e.expansion_macro as usize] =
                    out.intern_macro(&self.macros[e.expansion_macro as usize]);
            }
        }
        let entries = self.entries[idx..]
            .iter()
            .map(|e| LineMapEntry {
                output_offset: e.output_offset - start as u32,
                file: remap[e.file as usize],
                line: e.line,
                col: e.col,
                expansion_file: if e.expansion_file == u32::MAX {
                    u32::MAX
                } else {
                    if remap[e.expansion_file as usize] == u32::MAX {
                        remap[e.expansion_file as usize] =
                            out.intern_file(&self.files[e.expansion_file as usize]);
                    }
                    remap[e.expansion_file as usize]
                },
                expansion_line: e.expansion_line,
                expansion_col: e.expansion_col,
                expansion_id: e.expansion_id,
                expansion_macro: if e.expansion_macro == u32::MAX {
                    u32::MAX
                } else {
                    macro_remap[e.expansion_macro as usize]
                },
            })
            .collect();
        out.entries = entries;
        out
    }

    /// Drop mappings whose output offset is at or after `at`.
    ///
    /// Entries are pushed with non-decreasing output offsets (the invariant
    /// [`LineMap::lookup`] binary-searches on), so the survivors are a prefix
    /// and the cut point is one binary search. `retain` had to walk all of
    /// them instead, and the preprocessor cuts here after every cacheable
    /// nested include, which made dropping a short suffix cost a full pass
    /// over a map that holds one entry per emitted token (#83).
    pub fn truncate_at(&mut self, at: usize) {
        let keep = self
            .entries
            .partition_point(|e| (e.output_offset as usize) < at);
        self.entries.truncate(keep);
    }

    /// Append `other`'s entries shifted by `offset`, renumbering its file
    /// indices through `remap` (indexed by `other`'s file table).
    pub fn splice(&mut self, other: &LineMap, offset: usize, remap: &[u32]) {
        let macro_remap: Vec<u32> = other.macros.iter().map(|m| self.intern_macro(m)).collect();
        for e in &other.entries {
            self.entries.push(LineMapEntry {
                output_offset: e.output_offset + offset as u32,
                file: remap[e.file as usize],
                line: e.line,
                col: e.col,
                expansion_file: if e.expansion_file == u32::MAX {
                    u32::MAX
                } else {
                    remap[e.expansion_file as usize]
                },
                expansion_line: e.expansion_line,
                expansion_col: e.expansion_col,
                expansion_id: e.expansion_id,
                expansion_macro: if e.expansion_macro == u32::MAX {
                    u32::MAX
                } else {
                    macro_remap[e.expansion_macro as usize]
                },
            });
        }
    }

    /// Append mappings in `range`, rebased from its start to `offset`.
    /// Only files referenced by the half-open range are interned. Equal-offset
    /// entries at the start are retained; no preceding entry is synthesized.
    pub fn splice_range(&mut self, other: &LineMap, range: std::ops::Range<usize>, offset: usize) {
        if range.start >= range.end {
            return;
        }
        let start = other
            .entries
            .partition_point(|e| (e.output_offset as usize) < range.start);
        let end = start
            + other.entries[start..].partition_point(|e| (e.output_offset as usize) < range.end);
        if start == end {
            return;
        }
        self.entries.reserve(end - start);
        // A chunk of a couple of entries is the common case in cache
        // composition; interning those directly costs less than a remap
        // table over every file of `other`.
        if end - start <= 2 {
            for e in &other.entries[start..end] {
                let file = self.intern_file(&other.files[e.file as usize]);
                let expansion_file = other
                    .expansion_path_of(e)
                    .map_or(u32::MAX, |path| self.intern_file(path));
                let expansion_macro = other
                    .expansion_macro_of(e)
                    .map_or(u32::MAX, |m| self.intern_macro(m));
                self.entries.push(LineMapEntry {
                    output_offset: (e.output_offset as usize - range.start + offset) as u32,
                    file,
                    line: e.line,
                    col: e.col,
                    expansion_file,
                    expansion_line: e.expansion_line,
                    expansion_col: e.expansion_col,
                    expansion_id: e.expansion_id,
                    expansion_macro,
                });
            }
            return;
        }
        let mut remap = vec![u32::MAX; other.files.len()];
        let mut macro_remap = vec![u32::MAX; other.macros.len()];
        for e in &other.entries[start..end] {
            if remap[e.file as usize] == u32::MAX {
                remap[e.file as usize] = self.intern_file(&other.files[e.file as usize]);
            }
            if e.expansion_file != u32::MAX && remap[e.expansion_file as usize] == u32::MAX {
                remap[e.expansion_file as usize] =
                    self.intern_file(&other.files[e.expansion_file as usize]);
            }
            if e.expansion_macro != u32::MAX && macro_remap[e.expansion_macro as usize] == u32::MAX
            {
                macro_remap[e.expansion_macro as usize] =
                    self.intern_macro(&other.macros[e.expansion_macro as usize]);
            }
            self.entries.push(LineMapEntry {
                output_offset: (e.output_offset as usize - range.start + offset) as u32,
                file: remap[e.file as usize],
                line: e.line,
                col: e.col,
                expansion_file: if e.expansion_file == u32::MAX {
                    u32::MAX
                } else {
                    remap[e.expansion_file as usize]
                },
                expansion_line: e.expansion_line,
                expansion_col: e.expansion_col,
                expansion_id: e.expansion_id,
                expansion_macro: if e.expansion_macro == u32::MAX {
                    u32::MAX
                } else {
                    macro_remap[e.expansion_macro as usize]
                },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_range_preserves_boundaries_and_origins() {
        let mut src = LineMap::new();
        let a = src.intern_file(Path::new("/t/a.h"));
        let b = src.intern_file(Path::new("/t/b.h"));
        let excluded = src.intern_file(Path::new("/t/excluded.h"));
        src.push(0, excluded, 1, 1);
        src.push(4, a, 20, 3);
        src.push(4, b, 30, 7);
        src.push(8, a, 21, 5);
        src.push(9, excluded, 2, 1);
        src.push(100, excluded, 3, 1);

        let mut dst = LineMap::new();
        let existing = dst.intern_file(Path::new("/t/b.h"));
        dst.push(0, existing, 10, 1);
        dst.splice_range(&src, 4..9, 12);
        assert_eq!(
            dst.files,
            vec![PathBuf::from("/t/b.h"), PathBuf::from("/t/a.h")]
        );
        assert_eq!(
            dst.entries
                .iter()
                .map(|e| (e.output_offset, dst.path_of(e), e.line, e.col))
                .collect::<Vec<_>>(),
            vec![
                (0, Path::new("/t/b.h"), 10, 1),
                (12, Path::new("/t/a.h"), 20, 3),
                (12, Path::new("/t/b.h"), 30, 7),
                (16, Path::new("/t/a.h"), 21, 5),
            ]
        );
        let unchanged = dst.clone();
        dst.splice_range(&src, 5..8, 20);
        dst.splice_range(&src, 9..9, 20);
        dst.splice_range(&src, 101..200, 20);
        assert_eq!(
            dst, unchanged,
            "empty ranges must not intern files or add preceding mappings"
        );
        dst.splice_range(&src, 100..usize::MAX, 20);
        let last = dst.entries.last().unwrap();
        assert_eq!(
            (last.output_offset, dst.path_of(last), last.line),
            (20, Path::new("/t/excluded.h"), 3)
        );
    }

    #[test]
    fn truncate_at_keeps_the_offsets_below_the_cut() {
        let mut map = LineMap::new();
        let f = map.intern_file(Path::new("/t/a.c"));
        for (offset, line) in [(0, 1), (4, 1), (9, 2), (9, 3), (20, 4)] {
            map.push(offset, f, line, 1);
        }
        map.truncate_at(9);
        assert_eq!(
            map.entries
                .iter()
                .map(|e| e.output_offset)
                .collect::<Vec<_>>(),
            vec![0, 4],
            "an entry exactly at the cut goes, and so does everything after it"
        );

        // A cut past the end keeps everything; a cut at 0 keeps nothing.
        let mut all = LineMap::new();
        let f = all.intern_file(Path::new("/t/a.c"));
        all.push(0, f, 1, 1);
        all.push(7, f, 2, 1);
        all.truncate_at(100);
        assert_eq!(all.entries.len(), 2);
        all.truncate_at(0);
        assert!(all.entries.is_empty());
    }
}
