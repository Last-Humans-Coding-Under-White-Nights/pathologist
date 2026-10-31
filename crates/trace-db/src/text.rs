use anyhow::Result;
use rusqlite::{params, types::ToSqlOutput, Connection, Statement, ToSql};
use rustc_hash::FxHashMap;
use trace_ir::{BlockKind, EdgeKind, EventKind, GapKind};

/// `cfg_functions.coverage` by whether the graph has a gap.
pub(crate) const COVERAGE: [&str; 2] = ["complete", "partial"];

/// Identity of an interned SQLite string, independent of IR entity IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextId(i64);

impl ToSql for TextId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

/// The ids of the call-sequencing kind tags, so a graph's rows are written
/// without a string lookup each.
pub struct SequencingTexts {
    block: Vec<TextId>,
    edge: Vec<TextId>,
    event: Vec<TextId>,
    gap: Vec<TextId>,
    coverage: [TextId; 2],
}

impl SequencingTexts {
    pub fn block(&self, kind: BlockKind) -> TextId {
        self.block[kind as usize]
    }

    pub fn edge(&self, kind: EdgeKind) -> TextId {
        self.edge[kind as usize]
    }

    pub fn event(&self, kind: EventKind) -> TextId {
        self.event[kind as usize]
    }

    pub fn gap(&self, kind: GapKind) -> TextId {
        self.gap[kind as usize]
    }

    /// `cfg_functions.coverage` of a graph with or without a gap.
    pub fn coverage(&self, has_gap: bool) -> TextId {
        self.coverage[usize::from(has_gap)]
    }
}

/// Shared text storage for analysis and merge exports. IDs follow first
/// occurrence, never hash iteration. Only distinct strings are copied.
pub struct TextPool<'conn> {
    ids: FxHashMap<String, TextId>,
    insert: Statement<'conn>,
}

impl<'conn> TextPool<'conn> {
    pub fn new(conn: &'conn Connection) -> Result<Self> {
        let mut pool = Self {
            ids: FxHashMap::default(),
            insert: conn.prepare("INSERT INTO strings VALUES (?1, ?2)")?,
        };
        // Frequent graph tags get small, one-byte SQLite integers regardless
        // of how many source expressions or names precede graph export.
        // The empty string has ID 1, also the default for optional node text.
        for text in [
            "",
            "var",
            "loc",
            "call_target",
            "constant",
            "terminator",
            "local",
            "global",
            "field",
            "field_summary",
            "heap",
            "function",
            "string_lit",
            "copy",
            "addr_of",
            "load",
            "store",
            "gep",
            "dlsym",
            "unwrap",
            "points_to",
            "call_arg",
            "terminates",
            "direct",
            "indirect",
            "external",
            "ambiguous",
            "ipc",
            "write field",
            "return value",
            "file_static",
            "fn_static",
            "param",
        ] {
            pool.intern(text)?;
        }
        Ok(pool)
    }

    /// Give the call-sequencing kind tags small ids too. Only a sequenced
    /// export calls this, so other exports keep their string table.
    pub fn seed_sequencing(&mut self) -> Result<SequencingTexts> {
        // Indexed by discriminant: each `ALL` lists its kinds in declaration
        // order (`sequencing_kinds_are_listed_in_declaration_order`).
        let mut ids = |names: &[&str]| -> Result<Vec<TextId>> {
            names.iter().map(|name| self.intern(name)).collect()
        };
        Ok(SequencingTexts {
            block: ids(&BlockKind::ALL.map(BlockKind::as_str))?,
            edge: ids(&EdgeKind::ALL.map(EdgeKind::as_str))?,
            event: ids(&EventKind::ALL.map(EventKind::as_str))?,
            gap: ids(&GapKind::ALL.map(GapKind::as_str))?,
            coverage: [self.intern(COVERAGE[0])?, self.intern(COVERAGE[1])?],
        })
    }

    pub fn intern(&mut self, text: &str) -> Result<TextId> {
        if let Some(&id) = self.ids.get(text) {
            return Ok(id);
        }
        let id = TextId(self.ids.len() as i64 + 1);
        self.insert.execute(params![id, text])?;
        self.ids.insert(text.to_owned(), id);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequencing_kinds_are_listed_in_declaration_order() {
        assert!(BlockKind::ALL
            .iter()
            .enumerate()
            .all(|(i, &k)| k as usize == i));
        assert!(EdgeKind::ALL
            .iter()
            .enumerate()
            .all(|(i, &k)| k as usize == i));
        assert!(EventKind::ALL
            .iter()
            .enumerate()
            .all(|(i, &k)| k as usize == i));
        assert!(GapKind::ALL
            .iter()
            .enumerate()
            .all(|(i, &k)| k as usize == i));
    }
}
