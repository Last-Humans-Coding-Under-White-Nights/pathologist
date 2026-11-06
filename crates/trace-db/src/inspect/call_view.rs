use super::*;
use trace_ir::FnId;

/// The complete analysis graph or its stable lambda-free SQL projection.
/// Projection precedes traversal, so hidden lambdas consume no depth.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CallGraphView {
    #[default]
    Complete,
    WithoutLambdas,
}

impl CallGraphView {
    pub(super) fn edges(self, conn: &Connection) -> Result<&'static str> {
        if self == Self::Complete {
            return Ok("call_edges");
        }
        for relation in [
            "functions_no_lambdas",
            "call_edges_no_lambdas",
            "unresolved_calls_no_lambdas",
        ] {
            if !table_exists(conn, relation)? {
                bail!("`{relation}` missing: re-run `trace analyze` (or `trace-merge`) with this binary to use --hide-lambdas");
            }
        }
        Ok("call_edges_no_lambdas")
    }

    pub(super) fn require_visible(self, conn: &Connection, id: i64) -> Result<()> {
        if self == Self::WithoutLambdas {
            self.edges(conn)?;
            if !Self::visible_without_lambdas(conn, id)? {
                bail!("function id {id} is hidden by --hide-lambdas; select a non-lambda function");
            }
        }
        Ok(())
    }

    fn visible_without_lambdas(conn: &Connection, id: i64) -> Result<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM functions_no_lambdas WHERE id = ?1)",
            [id],
            |r| r.get(0),
        )?)
    }

    /// Resolve a position in the selected view. A position inside a lambda
    /// selects its enclosing visible function when one exists.
    pub fn require_function_at(
        self,
        conn: &Connection,
        file: &str,
        line: i64,
    ) -> Result<FunctionRef> {
        if self == Self::Complete {
            return require_function_at(conn, file, line);
        }
        self.edges(conn)?;
        for candidate in find_functions_at(conn, file, line)? {
            if Self::visible_without_lambdas(conn, candidate.id)? {
                return Ok(candidate);
            }
        }
        bail!("no non-lambda function contains {file}:{line}");
    }

    /// Resolve a name or source position using this view's visible functions.
    pub fn resolve_function_target(
        self,
        conn: &Connection,
        name: Option<&str>,
        file: Option<&str>,
        line: Option<i64>,
    ) -> Result<FunctionRef> {
        resolve_function_target_impl(conn, name, file, line, self)
    }

    /// Bounded BFS in the selected SQL view, in either direction.
    pub fn call_graph(
        self,
        conn: &Connection,
        root: FnId,
        dir: Direction,
        max_depth: u32,
    ) -> Result<QueryGraph> {
        call_graph_impl(conn, i64::from(root.0), dir, max_depth, self)
    }

    /// Simple paths in the selected SQL view; limits count visible hops.
    #[allow(clippy::too_many_arguments)]
    pub fn call_chains(
        self,
        conn: &Connection,
        from: FnId,
        to: FnId,
        dir: Direction,
        max_depth: u32,
        limit: Option<usize>,
    ) -> Result<CallChainsResult> {
        call_chains_impl(
            conn,
            i64::from(from.0),
            i64::from(to.0),
            dir,
            max_depth,
            limit,
            self,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database(compact: bool) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(if compact {
            crate::SCHEMA_V8
        } else {
            crate::SCHEMA_V7
        })
        .unwrap();
        conn.execute("INSERT INTO files VALUES(1, '/src/main.cpp', '', 0)", [])
            .unwrap();
        for (id, name, start, end) in [
            (1, "ScheduleAbilityTransaction", 1, 20),
            (2, "ScheduleAbilityTransaction::$lambda3:1", 3, 15),
            (
                3,
                "ScheduleAbilityTransaction::$lambda3:1::$lambda5:1",
                5,
                10,
            ),
            (4, "HandleAbilityTransaction", 30, 31),
            (5, "OtherCaller", 40, 41),
            (6, "OtherCaller::$lambda50:1", 50, 60),
            (7, "Unreachable", 70, 71),
        ] {
            conn.execute(
                "INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined) \
                 VALUES(?1,?2,1,?3,?4,'external','void()',1)",
                rusqlite::params![id, name, start, end],
            ).unwrap();
        }
        let mut pool = compact.then(|| crate::TextPool::new(&conn).unwrap());
        for (id, caller, callee, site, resolution) in [
            (10, 1, 2, Some(100), "indirect"),
            (11, 2, 3, Some(101), "indirect"),
            (12, 3, 2, Some(102), "indirect"), // lambda cycle
            (13, 3, 4, Some(103), "direct"),
            (14, 2, 4, Some(104), "direct"),   // duplicate endpoint
            (15, 5, 2, Some(105), "indirect"), // shared lambda
            (16, 6, 7, Some(106), "direct"),   // never invoked
            (17, 1, 5, Some(107), "direct"),
            (18, 4, 1, None, "ipc"),
            (19, 3, 1, Some(108), "direct"), // visible recursion
        ] {
            if let Some(site) = site {
                insert_site(&conn, &mut pool, site, caller, true);
            }
            if let Some(pool) = pool.as_mut() {
                conn.execute(
                    "INSERT INTO call_edges_data VALUES(?1,?2,?3,?4,?5)",
                    rusqlite::params![id, site, caller, callee, pool.intern(resolution).unwrap()],
                )
                .unwrap();
            } else {
                conn.execute(
                    "INSERT INTO call_edges VALUES(?1,?2,?3,?4,?5)",
                    rusqlite::params![id, site, caller, callee, resolution],
                )
                .unwrap();
            }
        }
        insert_site(&conn, &mut pool, 109, 3, false); // unresolved inside lambda
        insert_site(&conn, &mut pool, 110, 3, true); // not an unresolved leaf
        drop(pool);
        conn
    }

    fn insert_site(
        conn: &Connection,
        pool: &mut Option<crate::TextPool<'_>>,
        id: i64,
        caller: i64,
        direct: bool,
    ) {
        if let Some(pool) = pool.as_mut() {
            conn.execute(
                "INSERT INTO call_sites_data(id,caller_fn_id,file_id,line,col,callee_text_id,is_direct) \
                 VALUES(?1,?2,1,7,2,?3,?4)",
                rusqlite::params![id, caller, pool.intern("callback").unwrap(), direct],
            ).unwrap();
        } else {
            conn.execute(
                "INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct) \
                 VALUES(?1,?2,1,7,2,'callback',?3)",
                rusqlite::params![id, caller, direct],
            )
            .unwrap();
        }
    }

    #[test]
    fn lambda_projection_contracts_nested_shared_and_cyclic_paths_in_both_layouts() {
        for compact in [false, true] {
            let conn = database(compact);
            let edges: Vec<(i64, Option<i64>, i64, i64, String)> = conn
                .prepare(
                    "SELECT entry_edge_id,call_site_id,caller_fn_id,callee_fn_id,resolution \
                 FROM call_edges_no_lambdas ORDER BY entry_edge_id,callee_fn_id",
                )
                .unwrap()
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(
                edges,
                [
                    (10, Some(100), 1, 1, "indirect".into()),
                    (10, Some(100), 1, 4, "indirect".into()),
                    (15, Some(105), 5, 1, "indirect".into()),
                    (15, Some(105), 5, 4, "indirect".into()),
                    (17, Some(107), 1, 5, "direct".into()),
                    (18, None, 4, 1, "ipc".into()),
                ]
            );
            let visible: Vec<i64> = conn
                .prepare("SELECT id FROM functions_no_lambdas ORDER BY id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(visible, [1, 4, 5, 7]);
            let raw_count: i64 = conn
                .query_row("SELECT count(*) FROM call_edges", [], |r| r.get(0))
                .unwrap();
            assert_eq!(raw_count, 10, "raw graph remains intact");
            assert!(call_chains(&conn, 1, 4, Direction::Down, 1, None)
                .unwrap()
                .chains
                .is_empty());
            let view = CallGraphView::WithoutLambdas;
            for (from, to, dir) in [(1, 4, Direction::Down), (4, 1, Direction::Up)] {
                let chains = view
                    .call_chains(&conn, FnId(from), FnId(to), dir, 1, Some(1))
                    .unwrap();
                assert_eq!(chains.chains.len(), 1);
                assert_eq!(chains.chains[0].nodes, [i64::from(from), i64::from(to)]);
                assert_eq!(chains.chains[0].edges[0].resolution, "indirect");
                assert!(!chains.truncated);
            }
            let graph = view.call_graph(&conn, FnId(1), Direction::Down, 1).unwrap();
            assert!(graph.nodes.contains_key(&4));
            assert!(!graph.nodes.values().any(|n| n.label.contains("$lambda")));
            assert!(graph
                .edges
                .iter()
                .any(|e| e.from == 1 && e.to == -110 && e.label == "unresolved"));
            assert_eq!(
                view.require_function_at(&conn, "main.cpp", 7).unwrap().id,
                1
            );
            assert_eq!(require_function_at(&conn, "main.cpp", 7).unwrap().id, 3);
            assert!(view.call_graph(&conn, FnId(2), Direction::Down, 1).is_err());
            assert!(view
                .resolve_function_target(&conn, Some("main.cpp:7"), None, None)
                .is_ok());
            let unresolved: Vec<(i64,i64)> = conn.prepare(
                "SELECT call_site_id,caller_fn_id FROM unresolved_calls_no_lambdas ORDER BY caller_fn_id",
            ).unwrap().query_map([], |r| Ok((r.get(0)?,r.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            assert_eq!(unresolved, [(109, 1), (109, 5)]);
        }
    }

    #[test]
    fn old_databases_keep_raw_queries_and_explain_missing_projection() {
        let conn = database(false);
        conn.execute_batch("DROP VIEW call_edges_no_lambdas;")
            .unwrap();
        call_graph(&conn, 1, Direction::Down, 1).unwrap();
        let err = CallGraphView::WithoutLambdas
            .call_graph(&conn, FnId(1), Direction::Down, 1)
            .unwrap_err();
        assert!(err.to_string().contains("re-run `trace analyze`"), "{err}");
    }
}
