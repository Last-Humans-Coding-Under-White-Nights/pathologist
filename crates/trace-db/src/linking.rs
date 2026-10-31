//! Optional graph-linking metadata. Version 2 shares text, type shapes and
//! dispatch queries instead of repeating them for every member call.
use crate::{TextId, TextPool};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use trace_ir::{CallSiteId, FnId, LinkCall, LinkFunction, LinkingFacts};

#[derive(Serialize, Deserialize)]
struct Header {
    complete: bool,
}

fn fn_id(id: i64) -> Result<FnId> {
    Ok(FnId(
        u32::try_from(id).context("invalid linking function ID")?,
    ))
}
fn site_id(id: i64) -> Result<CallSiteId> {
    Ok(CallSiteId(
        u32::try_from(id).context("invalid linking call-site ID")?,
    ))
}
fn checked_flags(flags: u8, mask: u8) -> Result<u8> {
    if flags & !mask != 0 {
        bail!("invalid linking fact flags {flags}");
    }
    Ok(flags)
}

pub fn write_linking_facts(
    conn: &Connection,
    facts: &LinkingFacts,
    strings: &mut TextPool<'_>,
) -> Result<()> {
    let mut stmt = conn.prepare_cached("INSERT INTO link_class_facts VALUES (?1,?2,?3)")?;
    for (i, (name, defined, final_class)) in facts.classes.iter().enumerate() {
        stmt.execute(params![
            i as i64,
            strings.intern(name)?,
            u8::from(*defined) | u8::from(*final_class) << 1
        ])?;
    }
    let mut stmt = conn.prepare_cached("INSERT INTO link_alias_facts VALUES (?1,?2,?3)")?;
    for (i, (name, shape)) in facts.aliases.iter().enumerate() {
        stmt.execute(params![
            i as i64,
            strings.intern(name)?,
            strings.intern(&serde_json::to_string(shape)?)?
        ])?;
    }
    let mut stmt = conn.prepare_cached("INSERT INTO link_base_facts VALUES (?1,?2,?3)")?;
    for (i, (derived, base)) in facts.bases.iter().enumerate() {
        stmt.execute(params![
            i as i64,
            strings.intern(derived)?,
            strings.intern(base)?
        ])?;
    }
    let mut stmt = conn.prepare_cached("INSERT INTO link_template_facts VALUES (?1,?2)")?;
    for (i, fact) in facts.template_bases.iter().enumerate() {
        stmt.execute(params![
            i as i64,
            strings.intern(&serde_json::to_string(fact)?)?
        ])?;
    }
    let mut stmt = conn.prepare_cached("INSERT INTO link_idl_facts VALUES (?1,?2)")?;
    for (i, fact) in facts.interfaces.iter().enumerate() {
        stmt.execute(params![
            i as i64,
            strings.intern(&serde_json::to_string(fact)?)?
        ])?;
    }
    let mut method = conn.prepare_cached("INSERT INTO link_method_facts VALUES (?1,?2,?3,?4)")?;
    let mut origin = conn.prepare_cached("INSERT INTO link_method_origins VALUES (?1,?2,?3)")?;
    for (i, f) in facts.functions.iter().enumerate() {
        let shape = f.params.as_ref().map(serde_json::to_string).transpose()?;
        let shape = shape.as_deref().map(|s| strings.intern(s)).transpose()?;
        let flags = u8::from(f.virtual_member)
            | u8::from(f.final_member) << 1
            | u8::from(f.static_member) << 2
            | u8::from(f.variadic) << 3;
        method.execute(params![f.id.0, i as i64, shape, flags])?;
        for (j, name) in f.definition_origins.iter().enumerate() {
            origin.execute(params![f.id.0, j as i64, strings.intern(name)?])?;
        }
    }
    // The public call-sites projection is shared by v7/v8. Facts for sites
    // omitted by export cannot contribute to linking, so do not persist them.
    let sites: FxHashSet<i64> = conn
        .prepare("SELECT id FROM call_sites")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut queries: FxHashMap<(TextId, TextId, Option<FnId>, u8, TextId), i64> =
        FxHashMap::default();
    let mut query =
        conn.prepare_cached("INSERT INTO link_dispatch_queries VALUES (?1,?2,?3,?4,?5,?6)")?;
    let mut call = conn.prepare_cached("INSERT INTO link_member_calls VALUES (?1,?2,?3,?4)")?;
    for (i, c) in facts.calls.iter().enumerate() {
        if !sites.contains(&i64::from(c.id.0)) {
            continue;
        }
        let flags =
            u8::from(c.receiver_declared) | u8::from(c.exact) << 1 | u8::from(c.implicit) << 2;
        let key = (
            strings.intern(&c.name)?,
            strings.intern(&c.receiver)?,
            c.callee,
            flags,
            strings.intern(&c.origin)?,
        );
        let next = queries.len() as i64 + 1;
        let id = *queries.entry(key).or_insert(next);
        if id == next {
            query.execute(params![
                id,
                key.0,
                key.1,
                c.callee.map(|f| f.0),
                flags,
                key.4
            ])?;
        }
        let source = (c.source_site != c.id && sites.contains(&i64::from(c.source_site.0)))
            .then_some(c.source_site.0);
        call.execute(params![c.id.0, i as i64, id, source])?;
    }
    let mut stmt = conn.prepare_cached("INSERT INTO link_ipc_senders VALUES (?1)")?;
    for f in &facts.senders {
        stmt.execute(params![f.0])?;
    }
    conn.execute(
        "INSERT INTO linking_facts(id,version,data) VALUES(1,2,?1)",
        params![serde_json::to_string(&Header {
            complete: facts.complete
        })?],
    )?;
    Ok(())
}

pub fn read_linking_facts(conn: &Connection) -> Result<Option<LinkingFacts>> {
    if !crate::table_exists(conn, "linking_facts")? {
        return Ok(None);
    }
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT version,data FROM linking_facts WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((version, data)) = row else {
        return Ok(None);
    };
    match version {
        1 => Ok(Some(
            serde_json::from_str(&data).context("invalid linking_facts payload")?,
        )),
        2 => {
            let header: Header =
                serde_json::from_str(&data).context("invalid linking_facts header")?;
            Ok(Some(
                read_normalized(conn, header).context("invalid normalized linking facts")?,
            ))
        }
        _ => bail!("unsupported linking_facts version {version}; re-analyze the input"),
    }
}

fn read_json_rows<T: DeserializeOwned>(conn: &Connection, sql: &str) -> Result<Vec<T>> {
    conn.prepare(sql)?
        .query_map([], |r| r.get::<_, String>(0))?
        .map(|s| Ok(serde_json::from_str(&s?)?))
        .collect()
}

fn read_normalized(conn: &Connection, header: Header) -> Result<LinkingFacts> {
    let mut out = LinkingFacts {
        complete: header.complete,
        ..Default::default()
    };
    for row in conn.prepare("SELECT s.text,c.flags FROM link_class_facts c LEFT JOIN strings s ON s.id=c.name_id ORDER BY c.ordinal")?
        .query_map([],|r| Ok((r.get::<_,String>(0)?,r.get::<_,u8>(1)?)))? {
        let (name,flags) = row?;let flags=checked_flags(flags,3)?;
        out.classes.push((name,flags&1!=0,flags&2!=0));
    }
    for row in conn.prepare("SELECT n.text,t.text FROM link_alias_facts a LEFT JOIN strings n ON n.id=a.name_id LEFT JOIN strings t ON t.id=a.type_id ORDER BY a.ordinal")?
        .query_map([],|r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))? {
        let (name,shape)=row?;out.aliases.push((name,serde_json::from_str(&shape)?));
    }
    out.bases=conn.prepare("SELECT d.text,b.text FROM link_base_facts f LEFT JOIN strings d ON d.id=f.derived_id LEFT JOIN strings b ON b.id=f.base_id ORDER BY f.ordinal")?
        .query_map([],|r| Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    out.template_bases=read_json_rows(conn,"SELECT s.text FROM link_template_facts t LEFT JOIN strings s ON s.id=t.fact_id ORDER BY t.ordinal")?;
    out.interfaces=read_json_rows(conn,"SELECT s.text FROM link_idl_facts i LEFT JOIN strings s ON s.id=i.fact_id ORDER BY i.ordinal")?;
    let mut origins: FxHashMap<FnId, Vec<String>> = FxHashMap::default();
    for row in conn.prepare("SELECT o.fn_id,s.text FROM link_method_origins o LEFT JOIN strings s ON s.id=o.origin_id ORDER BY o.fn_id,o.ordinal")?
        .query_map([],|r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))? {
        let (id,origin)=row?;origins.entry(fn_id(id)?).or_default().push(origin);
    }
    for row in conn.prepare("SELECT m.fn_id,f.name,f.is_defined,m.params_id,s.text,m.flags FROM link_method_facts m LEFT JOIN functions f ON f.id=m.fn_id LEFT JOIN strings s ON s.id=m.params_id ORDER BY m.ordinal")?
        .query_map([],|r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,bool>(2)?,r.get::<_,Option<i64>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,u8>(5)?)))? {
        let (id,name,defined,shape_id,shape,flags)=row?;let flags=checked_flags(flags,15)?;let id=fn_id(id)?;
        let params=match shape_id { Some(_) => Some(serde_json::from_str(&shape.context("missing method parameter shape")?)?),None=>None };
        out.functions.push(LinkFunction { id,name,params,defined,virtual_member:flags&1!=0,final_member:flags&2!=0,static_member:flags&4!=0,variadic:flags&8!=0,definition_origins:origins.remove(&id).unwrap_or_default() });
    }
    if !origins.is_empty() {
        bail!("definition origins reference a missing method");
    }
    struct Query {
        name: String,
        receiver: String,
        callee: Option<FnId>,
        flags: u8,
        origin: String,
    }
    let mut queries = FxHashMap::default();
    for row in conn.prepare("SELECT q.id,n.text,r.text,q.callee_fn_id,q.flags,o.text FROM link_dispatch_queries q LEFT JOIN strings n ON n.id=q.name_id LEFT JOIN strings r ON r.id=q.receiver_id LEFT JOIN strings o ON o.id=q.origin_id ORDER BY q.id")?
        .query_map([],|r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<i64>>(3)?,r.get::<_,u8>(4)?,r.get::<_,String>(5)?)))? {
        let (id,name,receiver,callee,flags,origin)=row?;
        queries.insert(id,Query {name,receiver,callee:callee.map(fn_id).transpose()?,flags:checked_flags(flags,7)?,origin});
    }
    for row in conn.prepare("SELECT m.call_site_id,c.caller_fn_id,m.query_id,COALESCE(m.source_site_id,m.call_site_id),s.id FROM link_member_calls m LEFT JOIN call_sites c ON c.id=m.call_site_id LEFT JOIN call_sites s ON s.id=COALESCE(m.source_site_id,m.call_site_id) ORDER BY m.ordinal")?
        .query_map([],|r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,r.get::<_,Option<i64>>(4)?)))? {
        let (id,caller,query,source,source_exists)=row?;
        if source_exists.is_none() { bail!("member call references a missing source invocation"); }
        let q=queries.get(&query).context("member call references a missing dispatch query")?;
        out.calls.push(LinkCall {id:site_id(id)?,source_site:site_id(source)?,caller:fn_id(caller)?,callee:q.callee,name:q.name.clone(),receiver:q.receiver.clone(),receiver_declared:q.flags&1!=0,exact:q.flags&2!=0,implicit:q.flags&4!=0,origin:q.origin.clone()});
    }
    out.senders = conn
        .prepare("SELECT fn_id FROM link_ipc_senders ORDER BY fn_id")?
        .query_map([], |r| r.get::<_, i64>(0))?
        .map(|id| fn_id(id?))
        .collect::<Result<_>>()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TABLES_V8;
    use trace_ir::TypeDesc;

    fn sample() -> (Connection, LinkingFacts) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TABLES_V8).unwrap();
        conn.execute_batch(
            "INSERT INTO files(id,path,sha256) VALUES(0,'/fixture/code.cpp','');
            INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined)
            VALUES(1,'svc::Client::Run',0,1,2,'external','svc::Client::Run(int)',1),
                  (2,'svc::Iface::Open',0,3,3,'external','svc::Iface::Open(int)',0);",
        )
        .unwrap();
        let calls = (10..1010)
            .map(|i| LinkCall {
                id: CallSiteId(i),
                source_site: CallSiteId(10 + (i - 10) / 10 * 10),
                caller: FnId(1),
                callee: Some(FnId(2)),
                name: "svc::Iface::Open".into(),
                receiver: "svc::Iface".into(),
                receiver_declared: true,
                exact: false,
                origin: "/fixture".into(),
                implicit: false,
            })
            .collect();
        let method = |id, name: &str, defined| LinkFunction {
            id: FnId(id),
            name: name.into(),
            params: Some(vec![TypeDesc::Int]),
            defined,
            virtual_member: true,
            final_member: false,
            static_member: false,
            variadic: false,
            definition_origins: if defined {
                vec!["/fixture".into()]
            } else {
                vec![]
            },
        };
        let facts = LinkingFacts {
            complete: true,
            classes: vec![("svc::Iface".into(), true, false)],
            aliases: vec![(
                "svc::Alias".into(),
                TypeDesc::Struct {
                    name: "svc::Iface".into(),
                    fields: vec![],
                },
            )],
            bases: vec![("svc::Client".into(), "svc::Iface".into())],
            functions: vec![
                method(1, "svc::Client::Run", true),
                method(2, "svc::Iface::Open", false),
            ],
            calls,
            senders: vec![FnId(1)],
            ..Default::default()
        };
        (conn, facts)
    }

    fn write(conn: &Connection, facts: &LinkingFacts) {
        let mut strings = TextPool::new(conn).unwrap();
        let mut insert=conn.prepare("INSERT INTO call_sites_data(id,caller_fn_id,file_id,line,col,callee_text_id,is_direct) VALUES(?1,?2,0,4,?1,?3,1)").unwrap();
        for c in &facts.calls {
            insert
                .execute(params![
                    c.id.0,
                    c.caller.0,
                    strings.intern(&c.name).unwrap()
                ])
                .unwrap();
        }
        write_linking_facts(conn, facts, &mut strings).unwrap();
    }

    #[test]
    fn repeated_calls_share_queries_origins_and_parameter_shapes() {
        let (conn, facts) = sample();
        write(&conn, &facts);
        let roundtrip = read_linking_facts(&conn).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&facts).unwrap(),
            serde_json::to_value(roundtrip).unwrap()
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM link_dispatch_queries", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM link_member_calls", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1000
        );
        for value in ["/fixture", "[\"Int\"]"] {
            assert_eq!(
                conn.query_row("SELECT count(*) FROM strings WHERE text=?1", [value], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
        assert!(
            conn.query_row("SELECT length(data) FROM linking_facts", [], |r| r
                .get::<_, i64>(0))
                .unwrap()
                < 100
        );
    }

    #[test]
    fn legacy_json_and_normalized_tables_read_identical_facts() {
        let (conn, facts) = sample();
        conn.execute(
            "INSERT INTO linking_facts VALUES(1,1,?1)",
            params![serde_json::to_string(&facts).unwrap()],
        )
        .unwrap();
        let legacy = read_linking_facts(&conn).unwrap().unwrap();
        conn.execute("DELETE FROM linking_facts", []).unwrap();
        write(&conn, &legacy);
        assert_eq!(
            serde_json::to_value(legacy).unwrap(),
            serde_json::to_value(read_linking_facts(&conn).unwrap()).unwrap()
        );
    }

    #[test]
    fn missing_query_is_rejected() {
        let (conn, facts) = sample();
        write(&conn, &facts);
        conn.execute_batch("PRAGMA foreign_keys=OFF; DELETE FROM link_dispatch_queries;")
            .unwrap();
        assert!(format!("{:#}", read_linking_facts(&conn).unwrap_err())
            .contains("missing dispatch query"));
    }
}
