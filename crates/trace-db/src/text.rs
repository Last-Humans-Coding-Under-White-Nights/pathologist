use anyhow::Result;
use rusqlite::{params, types::ToSqlOutput, Connection, Statement, ToSql};
use rustc_hash::FxHashMap;

/// Identity of an interned SQLite string, independent of IR entity IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextId(i64);

impl ToSql for TextId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
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
