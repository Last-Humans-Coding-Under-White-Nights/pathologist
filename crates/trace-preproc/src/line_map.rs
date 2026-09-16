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
        for e in &other.entries {
            self.entries.push(LineMapEntry {
                output_offset: e.output_offset + offset as u32,
                file: remap[e.file as usize],
                line: e.line,
                col: e.col,
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
                self.entries.push(LineMapEntry {
                    output_offset: (e.output_offset as usize - range.start + offset) as u32,
                    file,
                    line: e.line,
                    col: e.col,
                });
            }
            return;
        }
        let mut remap = vec![u32::MAX; other.files.len()];
        for e in &other.entries[start..end] {
            let file = &mut remap[e.file as usize];
            if *file == u32::MAX {
                *file = self.intern_file(&other.files[e.file as usize]);
            }
            self.entries.push(LineMapEntry {
                output_offset: (e.output_offset as usize - range.start + offset) as u32,
                file: *file,
                line: e.line,
                col: e.col,
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
