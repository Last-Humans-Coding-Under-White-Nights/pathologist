//! Regression tests for cross-TU resolution bugs found on a real HDF-scale
//! codebase: pointer-returning prototypes shadowed by phantom variables,
//! direct calls whose definition lives in another TU, and file-`static`
//! definitions that must shadow same-name external functions.

mod common;

use common::*;
use trace_analysis::{analyze, ResolutionKind};
use trace_ir::Linkage;
use trace_parse::{build_program, build_program_with_jobs};

/// `struct Widget *WidgetGet(void);` is declared in a header and defined in
/// another TU. Lowering used to register a *variable* named `WidgetGet` for
/// the pointer-returning prototype, turning every call into an indirect call
/// through a variable that never receives function addresses (no edge).
#[test]
fn ptr_return_prototype_resolves_direct_edge() {
    let root = fixture("ptr_return_proto");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_edge(
            &program,
            &analysis,
            "CheckReady",
            "WidgetGet",
            ResolutionKind::Direct
        ),
        "cross-TU call to pointer-returning function must produce a direct edge"
    );

    // The prototype must not leak into the variable table.
    assert!(
        !program
            .symbols
            .variables
            .iter()
            .any(|v| v.name == "WidgetGet"),
        "prototype registered as phantom variable"
    );
}

/// A plain call to a function defined in another TU (no fn-ptr var) must
/// still yield a Direct edge after merge, even though lowering could not see
/// the callee in the calling TU.
#[test]
fn cross_tu_direct_call_recovers_edge() {
    let root = fixture("ptr_return_proto");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let edges = callees_of(&program, &analysis, "CheckReady");
    assert_eq!(edges.len(), 1, "exactly one edge from CheckReady");
    assert_eq!(edges[0].0, "WidgetGet");
    assert_eq!(edges[0].1, ResolutionKind::Direct);
}

/// Within a.c, the internal-linkage `helper` shadows b.c's external `helper`.
#[test]
fn static_definition_shadows_external_same_name() {
    let root = fixture("static_shadow");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let caller_a_id = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "caller_a")
        .expect("caller_a exists")
        .id;
    let edges_to_helper: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == caller_a_id)
        .map(|e| program.symbols.function(e.callee))
        .filter(|f| f.name == "helper")
        .collect();

    assert_eq!(edges_to_helper.len(), 1, "one helper edge from caller_a");
    assert_eq!(
        edges_to_helper[0].linkage,
        Linkage::Internal,
        "caller_a must bind to its own file-static helper"
    );

    // caller_b still binds to the external helper in b.c.
    let caller_b_id = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "caller_b")
        .expect("caller_b exists")
        .id;
    let b_edges: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == caller_b_id && fn_name(&program, e.callee) == "helper")
        .collect();
    assert_eq!(b_edges.len(), 1, "one helper edge from caller_b");
    assert_eq!(
        program.symbols.function(b_edges[0].callee).linkage,
        Linkage::External,
        "caller_b binds to the external helper"
    );
}

/// Arrays of structs with fn-ptr members, initialized with nested positional
/// initializer lists (`{ { FnA }, { FnB } }`), must feed ArrayFnMember facts
/// into the table var; an element field call resolves to every listed fn.
#[test]
fn nested_positional_init_table_resolves_members() {
    let root = fixture("fn_ptr_nested_table");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    for callee in ["FnA", "FnB"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                "CallTbl",
                callee,
                ResolutionKind::Indirect
            ),
            "tbl[i].fn call must resolve to {callee}"
        );
    }
}

/// Same table shape with designated initializers inside the nested lists
/// (`{ .name = "..", .init = Fn }`), invoked through a pointer to an element.
#[test]
fn nested_designated_init_table_resolves_members() {
    let root = fixture("fn_ptr_nested_table");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    for callee in ["InitNet", "InitFs"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                "CallMod",
                callee,
                ResolutionKind::Indirect
            ),
            "m->init call through &g_modules[i] must resolve to {callee}"
        );
    }
}

/// `&outer.member` must yield the member subobject location (typed by the
/// member's own struct), not the flattened outer instance. A Dispatch load
/// through `dev.service` must not pick up functions stored only in other
/// fields of the outer struct (HDF RegulatorTest.TestEntry vs
/// IDeviceIoService.Dispatch shared positional index 2).
#[test]
fn member_address_of_preserves_field_identity() {
    let root = fixture("fn_ptr_nested_table");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_edge(
            &program,
            &analysis,
            "InvokeTest",
            "EntryFn",
            ResolutionKind::Indirect
        ),
        "inst.TestEntry call must resolve to EntryFn"
    );
    assert!(
        has_edge(
            &program,
            &analysis,
            "CoreRun",
            "RealDispatch",
            ResolutionKind::Indirect
        ),
        "dev.service Dispatch call must resolve to RealDispatch"
    );
    assert!(
        !has_edge(
            &program,
            &analysis,
            "CoreRun",
            "EntryFn",
            ResolutionKind::Indirect
        ),
        "Dispatch load must not see fns stored in sibling fields of the outer struct"
    );
}

/// A `static inline` defined in a header and called from several TUs must
/// appear once, attributed to the header file (not once per including TU),
/// and its internal call sites must be deduplicated with header-origin
/// spans. Direct edges into/out of the canonical copy must survive.
#[test]
fn header_inline_calls_deduplicate_to_header_attribution() {
    let root = fixture("header_dedup");
    let program = build_program(&root, &default_opts(&root)).expect("build");

    let file_path = |program: &trace_ir::Program, id: trace_ir::FileId| -> String {
        program
            .symbols
            .files
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.path.display().to_string())
            .unwrap_or_default()
    };

    let hdr_adds: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "hdr_add")
        .collect();
    assert_eq!(hdr_adds.len(), 1, "hdr_add must collapse to one row");
    let hdr_add = hdr_adds[0];
    assert!(
        file_path(&program, hdr_add.span.file).ends_with("shared.h"),
        "hdr_add span must attribute to shared.h, got {}",
        file_path(&program, hdr_add.span.file)
    );
    assert_eq!(hdr_add.file, hdr_add.span.file);

    let helpers: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "hdr_helper")
        .collect();
    assert_eq!(helpers.len(), 1, "hdr_helper must collapse to one row");
    let helper_id = helpers[0].id;

    // One deduplicated call site inside hdr_add, attributed to the header.
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.caller == hdr_add.id && cs.callee_name == "hdr_helper")
        .collect();
    assert_eq!(sites.len(), 1, "duplicate hdr_helper call sites must merge");
    assert!(
        file_path(&program, sites[0].span.file).ends_with("shared.h"),
        "call site span must attribute to shared.h"
    );

    let (_pag, analysis) = analyze(&program);
    for (caller, callee) in [
        ("use_a", "hdr_add"),
        ("use_b", "hdr_add"),
        ("hdr_add", "hdr_helper"),
    ] {
        assert!(
            has_edge(&program, &analysis, caller, callee, ResolutionKind::Direct),
            "{caller} -> {callee} direct edge must survive dedup"
        );
    }
    assert_eq!(helpers[0].id, helper_id);

    // TU-local functions stay distinct.
    assert_eq!(
        program
            .symbols
            .functions
            .iter()
            .filter(|f| f.name == "use_a")
            .count(),
        1
    );
}

/// Functions referenced before their definition (no forward declaration) —
/// in a global designated initializer, in a function-body field store, and
/// through a fn-ptr variable initializer — must still resolve. Lowering
/// used to drop these silently when the definition had not been interned
/// yet (verified FN class on a real corpus).
#[test]
fn later_defined_fn_resolves_from_initializer_and_store() {
    let root = fixture("later_defined_init");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    // `g_tbl.init = LaterBody;` inside Bind(), defined after Bind.
    assert!(
        has_edge(
            &program,
            &analysis,
            "Caller",
            "LaterBody",
            ResolutionKind::Indirect
        ),
        "call through g_tbl.init must reach LaterBody despite definition order"
    );

    // `g_fp = LaterInit;` where LaterInit is defined below BindFp.
    assert!(
        has_edge(
            &program,
            &analysis,
            "UseFp",
            "LaterInit",
            ResolutionKind::Indirect
        ),
        "fn-ptr stored from a later-defined fn must flow"
    );
}

/// Plain-identifier calls to functions with no definition under the root
/// (declared-only or fully implicit) must be classified as External edges,
/// not left as unresolved indirect noise. Synthesized extern entries carry
/// `is_defined == false` and stay out of the variable table.
#[test]
fn unresolved_plain_ident_calls_become_external() {
    let root = fixture("extern_call");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    for callee in ["ext_helper", "undeclared_stub"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                "local_wrap",
                callee,
                ResolutionKind::External
            ),
            "call to {callee} must produce an external edge"
        );
    }

    // The synthesized entries exist as bodyless functions...
    for callee in ["ext_helper", "undeclared_stub"] {
        let f = program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == callee)
            .unwrap_or_else(|| panic!("{callee} must be interned"));
        assert!(!f.is_defined, "{callee} is not defined in the tree");
    }
    // ...and none of these sites leak indirect edges or phantom variables.
    for e in &analysis.call_edges {
        if fn_name(&program, e.caller) == "local_wrap" {
            assert_eq!(
                e.resolution,
                ResolutionKind::External,
                "external calls must not degrade into other resolutions"
            );
        }
    }
    assert!(!program
        .symbols
        .variables
        .iter()
        .any(|v| v.name == "ext_helper"));
    assert!(!program
        .symbols
        .variables
        .iter()
        .any(|v| v.name == "undeclared_stub"));
}

/// A call whose target is defined in another TU but has no prototype in the
/// caller must recover the REAL definition (Direct edge, arg-flow into its
/// body) — not be swallowed by external-callee synthesis.
#[test]
fn cross_tu_no_proto_call_recovers_definition() {
    let root = fixture("extern_call");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_edge(
            &program,
            &analysis,
            "caller",
            "helper",
            ResolutionKind::Direct
        ),
        "cross-TU call to a defined-but-undeclared function must produce a direct edge"
    );
    assert!(
        !has_edge(
            &program,
            &analysis,
            "caller",
            "helper",
            ResolutionKind::External
        ),
        "must not degrade into an external edge"
    );

    // The real definition keeps its identity: exactly one `helper`, defined.
    let helpers: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "helper")
        .collect();
    assert_eq!(helpers.len(), 1, "no phantom duplicate rows for helper");
    assert!(helpers[0].is_defined);
}

/// HDF-shaped: struct in a header, designated `.Init = fn` in one TU, load
/// `entry->Init()` in another. PCH-style header IR must still connect them
/// through the field summary (HdfDeviceLaunchNode / DeviceDriverBind).
#[test]
fn cross_tu_designated_init_resolves_indirect() {
    let root = fixture("cross_tu_designated");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_edge(
            &program,
            &analysis,
            "launch",
            "my_init",
            ResolutionKind::Indirect
        ),
        "launch -> g_entry.Init must reach my_init stored in the other TU"
    );
    assert!(
        has_edge(
            &program,
            &analysis,
            "launch",
            "my_bind",
            ResolutionKind::Indirect
        ),
        "launch -> g_entry.Bind must reach my_bind stored in the other TU"
    );
}

/// HDF `DeviceNodeExtDispatch`: nested `host->service.Dispatch = Fn` where
/// `IDeviceIoService` lives in a different header than `StreamHost`. PCH
/// isolation used to intern `service` as an empty tag, so the Dispatch
/// store was dropped.
#[test]
fn nested_header_struct_field_store_resolves() {
    let root = fixture("nested_host_dispatch");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_edge(
            &program,
            &analysis,
            "launch",
            "StreamDispatch",
            ResolutionKind::Indirect
        ),
        "launch -> s->Dispatch must see StreamDispatch stored via host->service.Dispatch"
    );
}

/// HDF `wifiService = { .object.objectId = 1, .Dispatch = DispatchToMessage }`:
/// nested `HdfObject` prefix plus a prototype that lives in a nested header
/// (`sidecar.h` via `wrapper.h`, not a direct include of the store TU).
#[test]
fn nested_designated_embedded_object_resolves() {
    let root = fixture("nested_designated_dispatch");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_edge(
            &program,
            &analysis,
            "launch",
            "DispatchToMessage",
            ResolutionKind::Indirect
        ),
        "launch -> g_svc.Dispatch must see DispatchToMessage from designated init"
    );
}

/// Raw `#include` scanner misses `#include OBJECT_HDR`. Parallel PCH must
/// still serialize `object.h` before `device.h` via preprocess edges, and
/// the nested designated store must resolve (jobs>1).
#[test]
fn macro_include_nested_object_resolves() {
    let root = fixture("macro_include_dispatch");
    let program = build_program_with_jobs(&root, &default_opts(&root), 2).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_edge(
            &program,
            &analysis,
            "launch",
            "DispatchToMessage",
            ResolutionKind::Indirect
        ),
        "launch -> g_svc.Dispatch must see DispatchToMessage via #include MACRO"
    );
}

/// `.hpp` reachable only from `.cpp` used to skip the warm/PCH header set
/// (filter was `.h` only). With `inline_include_bodies = false` the TU
/// remainder has no layout, so the field store was dropped.
#[test]
fn hpp_header_is_warmed_and_resolves() {
    let root = fixture("hpp_designated_dispatch");
    let program = build_program_with_jobs(&root, &default_opts(&root), 2).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_edge(
            &program,
            &analysis,
            "launch",
            "DispatchToMessage",
            ResolutionKind::Indirect
        ),
        "launch -> g_svc.Dispatch must see DispatchToMessage stored via layout.hpp"
    );
}

/// HDF `GpioOnDevEventReceive`: `GpioIrqFunc` typedef in one header, field
/// `func` on a struct in another. PCH isolation typed the field as `Int`
/// and dropped fn-ptr arg-flow into `set_irq`.
#[test]
fn typedef_fnptr_field_store_resolves() {
    let root = fixture("typedef_fnptr_field");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_edge(
            &program,
            &analysis,
            "fire",
            "Handler",
            ResolutionKind::Indirect
        ),
        "fire -> p->func must see Handler stored through GpioIrqFunc"
    );

    // C++-parsed header prototype must collapse into the C definition so
    // `register_it` (in register.cpp) actually reaches `set_irq`'s body.
    let set_irq: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "set_irq")
        .collect();
    assert!(
        set_irq.iter().any(|f| f.is_defined),
        "set_irq prototype must merge with the C definition"
    );
}

/// Different struct types with function pointers at the same positional index
/// (FieldId) but different field names must NOT leak across structs. Before
/// the field_name guard in the solver, GEP accesses into struct A would
/// pick up function pointers stored in struct B's same-index field, causing
/// massive false-positive indirect call edges (observed as 140 false
/// targets for HdfSbufReadBuffer in the real HDF corpus).
#[test]
fn cross_struct_field_id_no_pollution() {
    let root = fixture("fn_ptr_cross_struct");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    // CallWithOpsA loads "callback" — must resolve to CallbackImplA only.
    assert!(
        has_edge(
            &program,
            &analysis,
            "CallWithOpsA",
            "CallbackImplA",
            ResolutionKind::Indirect
        ),
        "CallWithOpsA must resolve to CallbackImplA"
    );
    assert!(
        !has_edge(
            &program,
            &analysis,
            "CallWithOpsA",
            "HandlerImplB",
            ResolutionKind::Indirect
        ),
        "CallWithOpsA must NOT see HandlerImplB from OpsB (cross-struct pollution)"
    );

    // CallWithOpsB loads "handler" — must resolve to HandlerImplB only.
    assert!(
        has_edge(
            &program,
            &analysis,
            "CallWithOpsB",
            "HandlerImplB",
            ResolutionKind::Indirect
        ),
        "CallWithOpsB must resolve to HandlerImplB"
    );
    assert!(
        !has_edge(
            &program,
            &analysis,
            "CallWithOpsB",
            "CallbackImplA",
            ResolutionKind::Indirect
        ),
        "CallWithOpsB must NOT see CallbackImplA from OpsA (cross-struct pollution)"
    );

    // CallBoth exercises both paths — verify it calls both dispatchers
    // directly, and that the indirect edges are inside them.
    for callee in ["CallWithOpsA", "CallWithOpsB"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                "CallBoth",
                callee,
                ResolutionKind::Direct
            ),
            "CallBoth must directly call {callee}"
        );
    }

    // Total indirect edges must be exactly 2 (one per field load)
    let all_indirect: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|e| e.resolution == ResolutionKind::Indirect)
        .map(|e| (fn_name(&program, e.caller), fn_name(&program, e.callee)))
        .collect();
    assert_eq!(
        all_indirect.len(),
        2,
        "total indirect edges must be exactly 2, got: {:?}",
        all_indirect
    );
}

#[test]
fn identical_header_statics_share_bodies_and_keep_visibility() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("util.h"), "static int helper(int x) { return x + 1; }\n[[maybe_unused]] static int helper2(int x) { return helper(x); }\n").unwrap();
    std::fs::write(
        root.join("other.h"),
        "static int helper(int x) { return x + 2; }\n",
    )
    .unwrap();
    for n in 1..=3 {
        std::fs::write(
            root.join(format!("t{n}.cpp")),
            format!("#include \"util.h\"\nint use{n}() {{ return helper2({n}); }}\n"),
        )
        .unwrap();
    }
    std::fs::write(
        root.join("other.cpp"),
        "#include \"other.h\"\nint other() { return helper(0); }\n",
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).unwrap();
    // Identical header bodies must be shared.
    let helper = only_function(&program, "helper2");
    assert_eq!(
        program
            .symbols
            .call_sites
            .iter()
            .filter(|s| s.caller == helper)
            .count(),
        1
    );
    let (_, analysis) = analyze(&program);
    for n in 1..=3 {
        assert!(has_edge(
            &program,
            &analysis,
            &format!("use{n}"),
            "helper2",
            ResolutionKind::Direct
        ));
        let tu = file_id(&program, root, &format!("t{n}.cpp"));
        assert!(program.symbols.function_visible_from(helper, tu));
    }
    let other = file_id(&program, root, "other.cpp");
    assert!(!program.symbols.function_visible_from(helper, other));
    let other_caller = program.symbols.resolve_function("other").unwrap();
    let other_edges: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == other_caller)
        .collect();
    assert_eq!(other_edges.len(), 1);
    assert_eq!(
        program.symbols.function(other_edges[0].callee).file,
        file_id(&program, root, "other.h")
    );
    let edges: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == helper)
        .collect();
    assert_eq!(edges.len(), 1);
    assert_eq!(
        program.symbols.function(edges[0].callee).file,
        file_id(&program, root, "util.h")
    );
}

#[test]
fn header_static_macro_expansions_remain_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("util.h"),
        "static int helper() { return VALUE; }\n",
    )
    .unwrap();
    for (n, value) in [(1, 10), (2, 20), (3, 10)] {
        std::fs::write(
            root.join(format!("t{n}.cpp")),
            format!(
                "#define VALUE {value}\n#include \"util.h\"\nint use{n}() {{ return helper(); }}\n"
            ),
        )
        .unwrap();
    }
    for configured in [false, true] {
        if configured {
            write_compile_commands(root, &["t1.cpp", "t2.cpp", "t3.cpp"]);
        }
        let program = build_program(root, &default_opts(root)).unwrap();
        let helpers: Vec<_> = program
            .symbols
            .functions
            .iter()
            .filter(|f| f.name == "helper")
            .collect();
        assert_eq!(helpers.len(), 2, "different expansions must stay distinct");
        let targets: Vec<_> = (1..=3)
            .map(|n| {
                let caller = program
                    .symbols
                    .resolve_function(&format!("use{n}"))
                    .unwrap();
                let site = program
                    .symbols
                    .call_sites
                    .iter()
                    .find(|s| s.caller == caller)
                    .unwrap();
                program.callees_of(site)
            })
            .collect();
        assert_eq!(targets[0].len(), 1);
        assert_eq!(targets[0], targets[2]);
        assert_ne!(targets[0], targets[1]);
    }
}

#[test]
fn shared_header_return_flow_keeps_each_tus_definition() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("util.h"), "int *target();\nstatic int *helper() { int *p = target(); return p; }\nstatic int *forward() { return target(); }\n").unwrap();
    for n in 1..=2 {
        std::fs::write(root.join(format!("t{n}.cpp")), format!("#include \"util.h\"\nint value{n};\nint *target() {{ return &value{n}; }}\nint *use{n}() {{ return helper(); }}\nint *pass{n}() {{ int *q{n} = forward(); return q{n}; }}\n")).unwrap();
    }
    let program = build_program(root, &default_opts(root)).unwrap();
    let (pag, analysis) = trace_analysis::analyze_with_options(
        &program,
        trace_analysis::AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    for name in ["p", "q1", "q2"] {
        let pointer = program
            .symbols
            .variables
            .iter()
            .find(|v| v.name == name)
            .unwrap();
        for n in 1..=2 {
            let value = program
                .symbols
                .variables
                .iter()
                .find(|v| v.name == format!("value{n}"))
                .unwrap();
            assert!(
                analysis
                    .points_to
                    .get(&pag.var_node[&pointer.id])
                    .is_some_and(|pts| pts.contains(&pag.var_location[&value.id])),
                "{name} lost TU {n}'s return value"
            );
        }
    }
}

#[test]
fn shared_header_call_keeps_the_context_without_a_local_definition() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("util.h"),
        "void target(); static void helper() { target(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("a.cpp"),
        "#include \"util.h\"\nvoid target() {} void usea() { helper(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.cpp"),
        "#include \"util.h\"\nvoid useb() { helper(); }\n",
    )
    .unwrap();
    std::fs::write(root.join("c.cpp"), "void target() {}\n").unwrap();
    let program = build_program(root, &default_opts(root)).unwrap();
    let helper = only_function(&program, "helper");
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == helper)
        .collect();
    assert_eq!(sites.len(), 1);
    assert_eq!(
        program.callees_of(sites[0]).len(),
        2,
        "the includer without a definition must retain both candidates"
    );
}

#[test]
fn header_static_lambdas_share_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("util.h"), "static void helper(void (*value)()) { auto callback = [value]() { value(); }; callback(); }\n").unwrap();
    for n in 1..=3 {
        std::fs::write(
            root.join(format!("t{n}.cpp")),
            format!("#include \"util.h\"\nvoid target{n}() {{}}\nvoid use{n}() {{ helper(target{n}); }}\n"),
        )
        .unwrap();
    }
    let program = build_program(root, &default_opts(root)).unwrap();
    let lambdas: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name.contains("$lambda"))
        .collect();
    assert_eq!(lambdas.len(), 1);
    assert_eq!(
        program
            .symbols
            .call_sites
            .iter()
            .filter(|s| s.caller == lambdas[0].id)
            .count(),
        1
    );
    let (_, analysis) = analyze(&program);
    for n in 1..=3 {
        assert!(
            analysis
                .call_edges
                .iter()
                .any(|edge| edge.caller == lambdas[0].id
                    && fn_name(&program, edge.callee) == format!("target{n}")),
            "shared capture lost callback from TU {n}"
        );
    }
}

#[test]
fn shared_header_fn_static_callback_storage_unions_tu_values() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("util.h"),
        r#"
typedef void (*Callback)();
static Callback file_saved;
static void helper(Callback cb) {
    static Callback saved;
    saved = cb;
    saved();
}
"#,
    )
    .unwrap();
    for n in 1..=2 {
        std::fs::write(root.join(format!("t{n}.cpp")), format!("#include \"util.h\"\nvoid target{n}() {{}}\nvoid use{n}() {{ file_saved = target{n}; file_saved(); helper(target{n}); }}\n")).unwrap();
    }
    for configured in [false, true] {
        if configured {
            write_compile_commands(root, &["t1.cpp", "t2.cpp"]);
        }
        let program = build_program(root, &default_opts(root)).unwrap();
        let saved: Vec<_> = program
            .symbols
            .variables
            .iter()
            .filter(|v| v.name == "saved")
            .collect();
        assert_eq!(
            saved.len(),
            1,
            "function-local static storage shares the body identity"
        );
        assert_eq!(saved[0].storage, trace_ir::StorageClass::FnStatic);
        let file_vars: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| s.callee_name == "file_saved")
            .map(|s| s.callee_var.unwrap())
            .collect();
        assert_eq!(file_vars.len(), 2);
        assert_ne!(file_vars[0], file_vars[1]);
        for id in file_vars {
            assert_eq!(
                program.symbols.variable(id).storage,
                trace_ir::StorageClass::FileStatic
            );
        }
        let helper = saved[0].fn_id.unwrap();
        let (_, analysis) = analyze(&program);
        let mut callbacks: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == helper)
            .map(|e| fn_name(&program, e.callee))
            .collect();
        callbacks.sort();
        assert_eq!(callbacks, ["target1", "target2"]);
    }
}

#[test]
fn shared_header_flows_are_not_repeated_by_later_tu_variants() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("util.h"), "static int *helper(int *p) { int *q = p; return q; }\n#ifdef EXTRA\nstatic int *extra_helper(int *p) { int *q = p; return q; }\n#endif\n").unwrap();
    std::fs::write(
        root.join("BUILD.gn"),
        "config(\"features\") { defines = [\"EXTRA\"] }\n",
    )
    .unwrap();
    for n in 1..=2 {
        std::fs::write(root.join(format!("t{n}.cpp")), format!("#include \"util.h\"\nint *use{n}(int *p) {{ return helper(p); }}\n#ifdef EXTRA\nvoid extra{n}() {{}}\n#endif\n")).unwrap();
    }
    for jobs in [1, 8] {
        let program = build_program_with_jobs(
            root,
            &default_opts(root).with_explore(true).with_explore_budget(1),
            jobs,
        )
        .unwrap();
        assert_eq!(
            program.variants_merged, 2,
            "each TU must contribute a variant"
        );
        for name in ["helper", "extra_helper"] {
            let helper = program
                .symbols
                .functions
                .iter()
                .find(|f| f.name == name)
                .unwrap()
                .id;
            let local = program
                .symbols
                .variables
                .iter()
                .find(|v| v.fn_id == Some(helper) && v.name == "q")
                .unwrap()
                .id;
            let copies = program.flow.iter().filter(|flow| matches!(flow, trace_ir::FlowConstraint::Copy { dst, .. } if *dst == local)).count();
            assert_eq!(
                copies, 1,
                "later TU variants must not replay a previously shared {name} flow"
            );
        }
    }
}
