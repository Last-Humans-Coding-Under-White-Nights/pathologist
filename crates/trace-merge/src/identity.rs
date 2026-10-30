//! Persistent identities for unchanged merge inputs. See docs/ANALYSIS.md,
//! "Stable merged call-edge IDs" for the format and scope of the contract.

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

pub(super) const TABLES: &str = "
CREATE TABLE merge_target_identities (
    target_id INTEGER PRIMARY KEY REFERENCES link_targets(id),
    identity BLOB NOT NULL CHECK(length(identity) = 32)
);
CREATE TABLE merge_function_identities (
    function_id INTEGER PRIMARY KEY REFERENCES functions(id),
    identity BLOB NOT NULL CHECK(length(identity) = 32)
);
CREATE TABLE merge_call_site_identities (
    call_site_id INTEGER PRIMARY KEY REFERENCES call_sites_data(id),
    identity BLOB NOT NULL CHECK(length(identity) = 32)
);";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Identity([u8; 32]);

impl Identity {
    pub(super) fn hash(domain: &str, parts: &[&[u8]]) -> Self {
        let mut hash = Sha256::new();
        for part in std::iter::once(domain.as_bytes()).chain(parts.iter().copied()) {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part);
        }
        Self(hash.finalize().into())
    }

    pub(super) fn input(database: trace_db::DatabaseIdentity) -> Self {
        Self::hash("trace-merge-input-v1", &[database.as_bytes()])
    }

    pub(super) fn local(self, kind: &str, id: i64) -> Self {
        Self::hash(kind, &[self.bytes(), &id.to_be_bytes()])
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.0
    }

    pub(super) fn edge(caller: Self, callee: Self, site: Option<Self>) -> Self {
        Self::hash(
            "trace-merge-call-edge-v1",
            &[
                caller.bytes(),
                callee.bytes(),
                site.as_ref().map_or(&[], Self::bytes),
            ],
        )
    }

    pub(super) fn edge_id(self) -> i64 {
        i64::from_be_bytes(self.0[..8].try_into().unwrap()) & i64::MAX
    }
}

/// Both relations must be present for a new merged input. Older inputs
/// establish a namespace from their database content identity instead.
pub(super) fn has_metadata(conn: &Connection) -> Result<bool> {
    let options: String = conn.query_row(
        "SELECT options_json FROM analysis_run ORDER BY id LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    let options = serde_json::from_str::<serde_json::Value>(&options).ok();
    let version = options
        .as_ref()
        .and_then(|v| v.get("call_edge_identity_version"));
    if version.is_some_and(|v| v.as_u64() != Some(1)) {
        bail!(
            "unsupported call_edge_identity_version: {}",
            version.unwrap()
        );
    }
    let functions = trace_db::table_exists(conn, "merge_function_identities")?;
    let sites = trace_db::table_exists(conn, "merge_call_site_identities")?;
    let targets = trace_db::table_exists(conn, "merge_target_identities")?;
    if functions != sites || (version.is_some() && !functions) || (targets && !functions) {
        bail!("incomplete stable call-edge identity metadata: both merge identity tables are required");
    }
    Ok(functions)
}

pub(super) fn read(
    conn: &Connection,
    table: &str,
    column: &str,
) -> Result<FxHashMap<i64, Identity>> {
    let mut stmt = conn.prepare(&format!("SELECT {column}, identity FROM {table}"))?;
    let mut rows = stmt.query([])?;
    let mut identities = FxHashMap::default();
    while let Some(row) = rows.next()? {
        let id = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        let bytes = bytes.try_into().map_err(|bytes: Vec<u8>| {
            anyhow::anyhow!(
                "{table} row {id} has {} identity bytes; expected 32",
                bytes.len()
            )
        })?;
        if identities.insert(id, Identity(bytes)).is_some() {
            bail!("duplicate {table} identity for row {id}");
        }
    }
    Ok(identities)
}

pub(super) fn inherited(
    metadata: Option<&FxHashMap<i64, Identity>>,
    id: i64,
) -> Result<Option<Identity>> {
    metadata
        .map(|m| {
            m.get(&id)
                .copied()
                .with_context(|| format!("missing stable identity for row {id}"))
        })
        .transpose()
}

/// Never probe for another numeric ID on collision: the answer would depend
/// on the other inputs. An identical identity denotes the same retained edge.
pub(super) fn register(seen: &mut FxHashMap<i64, Identity>, identity: Identity) -> Result<bool> {
    let id = identity.edge_id();
    if let Some(previous) = seen.get(&id) {
        if previous != &identity {
            bail!("stable call-edge ID collision at {id}; output was not published");
        }
        return Ok(false);
    }
    seen.insert(id, identity);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_one_ids_match_the_published_encoding() {
        // Fixed vectors keep future refactors from silently renumbering IDs.
        let input = Identity::hash("trace-merge-input-v1", &[&[7; 32]]);
        let caller = input.local("trace-merge-local-function-v1", 1);
        let callee = input.local("trace-merge-local-function-v1", 2);
        let site = input.local("trace-merge-call-site-v1", 3);
        assert_eq!(
            Identity::edge(caller, callee, Some(site)).edge_id(),
            668_311_814_176_645_992
        );
        assert_eq!(
            Identity::edge(caller, callee, None).edge_id(),
            6_228_181_068_701_381_468
        );
    }

    #[test]
    fn collisions_fail_in_both_orders_and_duplicates_keep_the_id() {
        let a = Identity([0; 32]);
        let mut b = a;
        b.0[31] = 1;
        for (first, second) in [(a, b), (b, a)] {
            let mut seen = FxHashMap::default();
            assert!(register(&mut seen, first).unwrap());
            assert!(!register(&mut seen, first).unwrap());
            assert!(register(&mut seen, second)
                .unwrap_err()
                .to_string()
                .contains("collision"));
        }
    }
}
