"""Logical analysis relations shared by v7/v8 measurement and comparison tools."""

RUN_METADATA = frozenset({"created_at", "trace_version", "schema_version"})


def quote_identifier(name):
    return '"' + name.replace('"', '""') + '"'


def relation_types(conn):
    return dict(conn.execute(
        "SELECT name,type FROM sqlite_master WHERE type IN ('table','view') "
        "AND name NOT LIKE 'sqlite_%' ORDER BY name"))


def storage_relation(conn, relation):
    """Return the writable backing table of a compact view, or the v7 table."""
    types = relation_types(conn)
    backing = relation + "_data"
    if types.get(relation) == "view" and types.get(backing) == "table":
        return backing
    return relation


def logical_relations(conn):
    """Ignore compact storage and the derived flow_nodes_text presentation view."""
    types = relation_types(conn)
    backing = {name + "_data" for name, kind in types.items()
               if kind == "view" and types.get(name + "_data") == "table"}
    internal = backing | {"flow_nodes_text"}
    if backing:
        internal.add("strings")
    return [name for name in types if name not in internal]


def relation_query(conn, relation):
    """Canonical multiset order works for views and WITHOUT ROWID tables too.

    Keep entity IDs and duplicates; exclude only producer/run/version metadata.
    No implicit rowid or dictionary ID participates in logical comparisons.
    """
    name = quote_identifier(relation)
    columns = [r[1] for r in conn.execute(f"PRAGMA table_info({name})")
               if not (relation == "analysis_run" and r[1] in RUN_METADATA)]
    fields = ",".join(quote_identifier(c) for c in columns)
    return columns, f"SELECT {fields} FROM {name} ORDER BY {fields}"
