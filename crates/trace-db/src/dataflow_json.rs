//! The source-level JSON contract is separate from internal graph metadata.
use super::*;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "lowercase")]
enum ScopeRef {
    Function(FnId),
    Global(VarId),
    Static(VarId),
    Value(PagNodeId),
}

#[derive(Debug, Serialize)]
struct FunctionDescription {
    id: FnId,
    name: String,
    signature: String,
    location: FlowSite,
}

#[derive(Debug, Serialize)]
struct VariableDescription {
    id: VarId,
    name: String,
    location: FlowSite,
    #[serde(skip)]
    storage: String,
}

#[derive(Debug, Serialize)]
struct ValueDescription<'a> {
    id: PagNodeId,
    name: &'a str,
    location: &'a FlowSite,
}

#[derive(Debug, Default)]
pub(super) struct Metadata {
    functions: BTreeMap<FnId, FunctionDescription>,
    variables: BTreeMap<VarId, VariableDescription>,
    callers: BTreeMap<CallSiteId, Option<FnId>>,
    operation_functions: BTreeMap<FlowSite, Option<FnId>>,
}

// Numeric IDs originate in typed graph metadata, never user-provided SQL.
fn id_list(ids: impl Iterator<Item = u32>) -> String {
    ids.map(|id| id.to_string()).collect::<Vec<_>>().join(",")
}

impl Metadata {
    pub(super) fn load(conn: &Connection, view: &DataflowView) -> Result<Self> {
        let mut metadata = Self::default();
        let variables: BTreeSet<_> = view.nodes.values().filter_map(|n| n.var_id).collect();
        if !variables.is_empty() {
            let ids = id_list(variables.iter().map(|id| id.0));
            let mut stmt = conn.prepare(&format!(
                "SELECT v.id,v.name,v.kind,p.path,v.line,v.col FROM variables v \
                 JOIN files p ON p.id=v.file_id WHERE v.id IN ({ids}) ORDER BY v.id"
            ))?;
            for row in stmt.query_map([], |r| {
                Ok(VariableDescription {
                    id: VarId(r.get(0)?),
                    name: r.get(1)?,
                    storage: r.get(2)?,
                    location: FlowSite {
                        path: r.get(3)?,
                        line: r.get(4)?,
                        col: r.get(5)?,
                    },
                })
            })? {
                let description = row?;
                metadata.variables.insert(description.id, description);
            }
        }

        let calls: BTreeSet<_> = view.edges.iter().filter_map(|e| e.call_site_id).collect();
        if !calls.is_empty() {
            let ids = id_list(calls.iter().map(|id| id.0));
            let mut stmt = conn.prepare(&format!(
                "SELECT id,caller_fn_id FROM call_sites WHERE id IN ({ids}) ORDER BY id"
            ))?;
            for row in stmt.query_map([], |r| {
                Ok((CallSiteId(r.get(0)?), r.get::<_, Option<u32>>(1)?.map(FnId)))
            })? {
                let (id, caller) = row?;
                metadata.callers.insert(id, caller);
            }
        }

        // Resolve source operations by exact file path and definition line range.
        // A recorded call's caller (including NULL) always takes precedence.
        let positions: BTreeSet<_> = view
            .edges
            .iter()
            .flat_map(|e| {
                std::iter::once(&e.location).chain(e.provenance.iter().map(|p| &p.location))
            })
            .filter(|p| !p.path.is_empty() && p.line > 0)
            .cloned()
            .collect();
        let mut owners = crate::inspect::DefinitionOwners::default();
        let mut file_ids = conn.prepare("SELECT id FROM files WHERE path = ?1")?;
        let mut file: Option<(String, Option<i64>)> = None;
        for site in positions {
            // Positions are sorted: each path is looked up once.
            if file.as_ref().is_none_or(|(path, _)| *path != site.path) {
                let id = file_ids.query_row([&site.path], |r| r.get(0)).optional()?;
                file = Some((site.path.clone(), id));
            }
            let owner = match file.as_ref().and_then(|(_, id)| *id) {
                Some(id) => owners.owner(conn, id, site.line)?,
                None => None,
            };
            let owner = owner.map(|id| FnId(id as u32));
            metadata.operation_functions.insert(site, owner);
        }

        let mut functions: BTreeSet<_> = view
            .nodes
            .values()
            .flat_map(|n| n.fn_id.into_iter().chain(n.callees.iter().copied()))
            .chain(view.edges.iter().filter_map(|e| e.callee_fn_id))
            .chain(
                view.edges
                    .iter()
                    .flat_map(|e| e.provenance.iter().filter_map(|p| p.callee_fn_id)),
            )
            .chain(metadata.callers.values().filter_map(|id| *id))
            .chain(metadata.operation_functions.values().filter_map(|id| *id))
            .collect();
        // Reuse already loaded descriptions; keep extra JSON-only headers out of
        // text/diagram scope discovery and ordering.
        for scope in view.scopes.values() {
            if let Some(id) = scope.fn_id.filter(|id| functions.remove(id)) {
                metadata.functions.insert(
                    id,
                    FunctionDescription {
                        id,
                        name: scope.name.clone(),
                        signature: scope.signature.clone(),
                        location: scope.location.clone(),
                    },
                );
            }
        }
        if !functions.is_empty() {
            let ids = id_list(functions.iter().map(|id| id.0));
            let mut stmt = conn.prepare(&format!(
                "SELECT f.id,f.name,f.signature,p.path,f.line_start FROM functions f \
                 JOIN files p ON p.id=f.file_id WHERE f.id IN ({ids}) ORDER BY f.id"
            ))?;
            for row in stmt.query_map([], |r| {
                Ok(FunctionDescription {
                    id: FnId(r.get(0)?),
                    name: r.get(1)?,
                    signature: r.get(2)?,
                    location: FlowSite {
                        path: r.get(3)?,
                        line: r.get(4)?,
                        col: 0,
                    },
                })
            })? {
                let description = row?;
                metadata.functions.insert(description.id, description);
            }
        }
        Ok(metadata)
    }

    fn node_scope(&self, node: &FlowEntity) -> Option<ScopeRef> {
        if let Some(variable) = node.var_id.and_then(|id| self.variables.get(&id)) {
            match variable.storage.as_str() {
                "global" => return Some(ScopeRef::Global(variable.id)),
                "file_static" => return Some(ScopeRef::Static(variable.id)),
                _ => {}
            }
        }
        if let Some(id) = node.fn_id {
            Some(ScopeRef::Function(id))
        } else if node.var_id.is_none() {
            Some(ScopeRef::Value(node.id))
        } else {
            None
        }
    }

    fn edge_scope(&self, edge: &FlowStep) -> Option<ScopeRef> {
        let owner = match edge.call_site_id {
            Some(id) => self.callers.get(&id).copied().flatten(),
            None => self
                .operation_functions
                .get(&edge.location)
                .copied()
                .flatten(),
        };
        owner.map(ScopeRef::Function)
    }
}

#[derive(Serialize)]
struct Scopes<'a> {
    functions: Vec<&'a FunctionDescription>,
    globals: Vec<&'a VariableDescription>,
    statics: Vec<&'a VariableDescription>,
    values: Vec<ValueDescription<'a>>,
}

#[derive(Serialize)]
struct Node<'a> {
    id: PagNodeId,
    name: &'a str,
    kind: &'a str,
    scope: Option<ScopeRef>,
    location: &'a FlowSite,
    depth: u32,
    callees: &'a [FnId],
}

#[derive(Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct Operation<'a> {
    kind: &'a str,
    expression: &'a str,
    location: &'a FlowSite,
    #[serde(skip_serializing_if = "Option::is_none")]
    callee_id: Option<FnId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arg_index: Option<i64>,
}

#[derive(Serialize)]
struct Edge<'a> {
    from: PagNodeId,
    to: PagNodeId,
    scope: Option<ScopeRef>,
    expression: &'a str,
    location: &'a FlowSite,
    operations: Vec<Operation<'a>>,
}

fn operations(edge: &FlowStep) -> Vec<Operation<'_>> {
    static EMPTY_LOCATION: FlowSite = FlowSite {
        path: String::new(),
        line: 0,
        col: 0,
    };
    let mut operations = Vec::new();
    let mut seen = BTreeSet::new();
    for origin in &edge.provenance {
        let operation = Operation {
            kind: &origin.operation,
            expression: &origin.expression,
            location: &origin.location,
            callee_id: origin.callee_fn_id,
            arg_index: origin.arg_index,
        };
        if seen.insert((
            &origin.operation,
            &origin.expression,
            &origin.location,
            origin.arg_index,
            origin.callee_fn_id,
        )) {
            operations.push(operation);
        }
    }
    for kind in &edge.operations {
        if !operations.iter().any(|op| op.kind == kind) {
            operations.push(Operation {
                kind,
                expression: "",
                location: &EMPTY_LOCATION,
                callee_id: None,
                arg_index: None,
            });
        }
    }
    operations
}

#[derive(Serialize)]
struct Document<'a> {
    schema: &'static str,
    title: &'a str,
    direction: &'a str,
    depth: u32,
    truncated: bool,
    scopes: Scopes<'a>,
    nodes: Vec<Node<'a>>,
    edges: Vec<Edge<'a>>,
}

pub(super) fn render<'a>(
    view: &'a DataflowView,
    meta: &GraphMeta,
    edges: impl Iterator<Item = &'a FlowStep>,
) -> String {
    let metadata = &view.json_metadata;
    let nodes: Vec<_> = view
        .nodes
        .values()
        .map(|node| Node {
            id: node.id,
            name: &node.name,
            kind: &node.kind,
            scope: metadata.node_scope(node),
            location: &node.location,
            depth: node.depth,
            callees: &node.callees,
        })
        .collect();
    let globals: BTreeSet<_> = nodes
        .iter()
        .filter_map(|n| match n.scope {
            Some(ScopeRef::Global(id)) => Some(id),
            _ => None,
        })
        .collect();
    let statics: BTreeSet<_> = nodes
        .iter()
        .filter_map(|n| match n.scope {
            Some(ScopeRef::Static(id)) => Some(id),
            _ => None,
        })
        .collect();
    let scopes = Scopes {
        functions: metadata.functions.values().collect(),
        globals: globals
            .iter()
            .filter_map(|id| metadata.variables.get(id))
            .collect(),
        statics: statics
            .iter()
            .filter_map(|id| metadata.variables.get(id))
            .collect(),
        values: nodes
            .iter()
            .filter_map(|n| match n.scope {
                Some(ScopeRef::Value(id)) => Some(ValueDescription {
                    id,
                    name: n.name,
                    location: n.location,
                }),
                _ => None,
            })
            .collect(),
    };
    let edges = edges
        .map(|edge| Edge {
            from: edge.from,
            to: edge.to,
            scope: metadata.edge_scope(edge),
            expression: &edge.expression,
            location: &edge.location,
            operations: operations(edge),
        })
        .collect();
    serde_json::to_string_pretty(&Document {
        schema: "dataflow-source-v1",
        title: meta.title,
        direction: meta.direction,
        depth: meta.depth,
        truncated: view.truncated,
        scopes,
        nodes,
        edges,
    })
    .unwrap()
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_owners_read_each_file_through_the_function_range_index() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::SCHEMA_V7).unwrap();
        conn.execute_batch("PRAGMA automatic_index=OFF;
            INSERT INTO files VALUES(0,'/src/main.c','',0),(1,'/other/main.c','',0);
            INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined) VALUES
            (1,'owner',0,10,20,'external','owner()',1),
            (2,'declaration',0,10,20,'external','declaration()',0),
            (3,'unrelated',1,10,20,'external','unrelated()',1),
            (4,'lambda',0,12,14,'external','lambda()',1),
            (5,'twin',0,12,14,'external','twin()',1);").unwrap();
        let plan: Vec<String> = conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id, line_start, line_end FROM functions \
                 WHERE file_id = ?1 AND is_defined = 1 ORDER BY id",
            )
            .unwrap()
            .query_map([0], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter().any(|detail| detail
                .contains("USING COVERING INDEX idx_functions_file_range")
                && detail.contains("file_id=?")
                && detail.contains("is_defined=?")),
            "{plan:?}"
        );
        let mut owners = crate::inspect::DefinitionOwners::default();
        // The declaration and the other file's definition own nothing here;
        // the two definitions inside `owner` share a range, so neither is
        // innermost, and on its first line `owner` is not enclosing either.
        let at = |owners: &mut crate::inspect::DefinitionOwners, line| {
            owners.owner(&conn, 0, line).unwrap()
        };
        assert_eq!(at(&mut owners, 10), Some(1));
        assert_eq!(at(&mut owners, 11), Some(1));
        assert_eq!(at(&mut owners, 12), None);
        assert_eq!(at(&mut owners, 13), None);
        assert_eq!(at(&mut owners, 20), Some(1));
        assert_eq!(at(&mut owners, 21), None);
        conn.execute_batch("DELETE FROM functions WHERE id = 5;")
            .unwrap();
        let mut owners = crate::inspect::DefinitionOwners::default();
        assert_eq!(at(&mut owners, 12), None, "on the lambda's first line");
        assert_eq!(at(&mut owners, 13), Some(4), "inside the lambda");
        conn.execute_batch("DROP INDEX idx_functions_file_range;")
            .unwrap();
        let mut legacy = crate::inspect::DefinitionOwners::default();
        assert_eq!(
            at(&mut legacy, 13),
            Some(4),
            "older v7 databases remain readable"
        );
    }
}
