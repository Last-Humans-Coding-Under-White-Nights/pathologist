//! Identity of an immutable database snapshot, independent of its filename,
//! run timestamp, SQLite page layout and indexes. See docs/SQLITE_SCHEMA.md.

use anyhow::{bail, Context, Result};
use rusqlite::{types::ValueRef, Connection};
use sha2::{Digest, Sha256};

const ALGORITHM: &str = "sha256-logical-v1";

/// SHA-256 identity of an exported database's logical contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DatabaseIdentity([u8; 32]);

impl DatabaseIdentity {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Read a persisted identity, or compute one for a legacy database without
/// modifying it. Published snapshots must be immutable; use
/// `record_database_identity` after deliberately editing their contents.
pub fn database_identity(conn: &Connection) -> Result<DatabaseIdentity> {
    if crate::table_exists(conn, "database_identity")? {
        let mut stmt = conn.prepare("SELECT id, algorithm, digest FROM database_identity")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let algorithm: String = row.get(1)?;
            let digest: Vec<u8> = row.get(2)?;
            if id != 1 || rows.next()?.is_some() {
                bail!("database_identity must contain exactly one row with id=1");
            }
            if algorithm != ALGORITHM {
                bail!("unsupported database identity algorithm: {algorithm}");
            }
            return Ok(DatabaseIdentity(digest.try_into().map_err(
                |bytes: Vec<u8>| {
                    anyhow::anyhow!(
                        "database_identity has {} digest bytes; expected 32",
                        bytes.len()
                    )
                },
            )?));
        }
    }
    compute(conn)
}

/// Compute and store the identity after all analysis rows have been written,
/// inside the producer's export transaction and before publication.
pub fn record_database_identity(conn: &Connection) -> Result<DatabaseIdentity> {
    let identity = compute(conn)?;
    conn.execute_batch(crate::DATABASE_IDENTITY_TABLE)?;
    conn.execute(
        "INSERT INTO database_identity(id, algorithm, digest) VALUES(1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET algorithm=excluded.algorithm, digest=excluded.digest",
        rusqlite::params![ALGORITHM, identity.as_bytes().as_slice()],
    )?;
    Ok(identity)
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn compute(conn: &Connection) -> Result<DatabaseIdentity> {
    let tables: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_schema WHERE type='table'
             AND name NOT GLOB 'sqlite_*' AND name <> 'database_identity' ORDER BY name",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut hash = Sha256::new();
    field(&mut hash, ALGORITHM.as_bytes());
    for table in tables {
        let columns: Vec<(String, i64)> = conn
            .prepare("SELECT name, pk FROM pragma_table_info(?1) ORDER BY cid")?
            .query_map([&table], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(name, _)| table != "analysis_run" || name != "created_at")
            .collect();
        hash.update([0x10]);
        field(&mut hash, table.as_bytes());
        hash.update((columns.len() as u64).to_be_bytes());
        for (name, _) in &columns {
            field(&mut hash, name.as_bytes());
        }
        let select = columns
            .iter()
            .map(|(name, _)| quoted(name))
            .collect::<Vec<_>>()
            .join(",");
        // Use indexed primary-key order where available. Relations without a
        // primary key sort by all values, retaining duplicate rows. Rows stream
        // into the digest rather than accumulating a database-sized dump.
        let mut primary: Vec<_> = columns.iter().filter(|(_, pk)| *pk > 0).collect();
        primary.sort_by_key(|(_, pk)| *pk);
        let order = if primary.is_empty() {
            columns.iter().collect::<Vec<_>>()
        } else {
            primary
        }
        .into_iter()
        .map(|(name, _)| format!("{} COLLATE BINARY", quoted(name)))
        .collect::<Vec<_>>()
        .join(",");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {select} FROM {} ORDER BY {order}",
                quoted(&table)
            ))
            .with_context(|| format!("cannot hash database table {table}"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            hash.update([0x20]);
            for i in 0..columns.len() {
                match row.get_ref(i)? {
                    ValueRef::Null => hash.update([0]),
                    ValueRef::Integer(value) => {
                        hash.update([1]);
                        hash.update(value.to_be_bytes());
                    }
                    ValueRef::Real(value) => {
                        hash.update([2]);
                        hash.update(value.to_bits().to_be_bytes());
                    }
                    ValueRef::Text(value) => {
                        hash.update([3]);
                        field(&mut hash, value);
                    }
                    ValueRef::Blob(value) => {
                        hash.update([4]);
                        field(&mut hash, value);
                    }
                }
            }
        }
        hash.update([0x30]);
    }
    Ok(DatabaseIdentity(hash.finalize().into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE analysis_run(id INTEGER PRIMARY KEY, created_at TEXT, options_json TEXT);
             INSERT INTO analysis_run VALUES(1,'now','{\"defines\":[]}');
             CREATE TABLE facts(id INTEGER PRIMARY KEY, value TEXT);
             INSERT INTO facts VALUES(2,'b'),(1,'a');
             CREATE TABLE unordered(value TEXT);
             INSERT INTO unordered VALUES('b'),('a'),('a');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn identity_ignores_timestamps_layout_indexes_and_insertion_order() {
        let conn = snapshot();
        let original = database_identity(&conn).unwrap();
        conn.execute_batch(
            "UPDATE analysis_run SET created_at='later';
             DELETE FROM unordered; INSERT INTO unordered VALUES('a'),('a'),('b');
             CREATE INDEX facts_value ON facts(value); VACUUM;",
        )
        .unwrap();
        assert_eq!(original, database_identity(&conn).unwrap());
        assert_eq!(original, record_database_identity(&conn).unwrap());
        assert_eq!(original, database_identity(&conn).unwrap());
        assert_eq!(original, record_database_identity(&conn).unwrap());
    }

    #[test]
    fn contents_configuration_and_duplicate_facts_affect_identity() {
        for change in [
            "UPDATE facts SET value='changed' WHERE id=1",
            "UPDATE analysis_run SET options_json='{\"defines\":[\"FEATURE\"]}'",
            "DELETE FROM unordered WHERE rowid=2",
        ] {
            let conn = snapshot();
            let original = database_identity(&conn).unwrap();
            conn.execute_batch(change).unwrap();
            assert_ne!(original, database_identity(&conn).unwrap(), "{change}");
        }
    }

    #[test]
    fn unsupported_or_malformed_stored_identity_is_rejected() {
        let conn = snapshot();
        record_database_identity(&conn).unwrap();
        conn.execute("UPDATE database_identity SET algorithm='future'", [])
            .unwrap();
        assert!(database_identity(&conn).is_err());
        conn.execute_batch("PRAGMA ignore_check_constraints=ON; UPDATE database_identity SET algorithm='sha256-logical-v1',digest=x'00'").unwrap();
        assert!(database_identity(&conn).is_err());
    }
}
