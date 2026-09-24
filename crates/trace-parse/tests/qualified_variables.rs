//! Stage 1 (issue #133): canonical identity for namespace-scoped variables.
//!
//! Before this stage, `SymbolTable::add_variable` indexed every untargeted
//! global under its bare `name`, so `a::ptr`, `b::ptr` and a plain `ptr`
//! collided in `global_by_name` — the last one registered won the slot and
//! the others became unreachable by name. This regression first shows that
//! collision, then (once canonical identity is wired in) that each namespace
//! variable keeps its own distinct `VarId` while still displaying as `ptr`.

use std::fs;
use trace_ir::{FlowConstraint, ReturnFlow, StorageClass, TypeDesc, TypeKind};
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

/// The one variable in `program` carrying `qualified_name`, panicking with
/// the full variable list if there is none or more than one — every
/// assertion below expects a static member's declaration and definition to
/// have merged onto a single canonical entry.
fn the_variable_named<'p>(
    program: &'p trace_ir::Program,
    qualified_name: &str,
) -> &'p trace_ir::Variable {
    let mut matches = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.qualified_name.as_deref() == Some(qualified_name));
    let found = matches
        .next()
        .unwrap_or_else(|| panic!("no variable carries qualified_name {qualified_name:?}"));
    assert!(
        matches.next().is_none(),
        "more than one variable carries qualified_name {qualified_name:?}: {:?}",
        program
            .symbols
            .variables
            .iter()
            .filter(|v| v.qualified_name.as_deref() == Some(qualified_name))
            .collect::<Vec<_>>()
    );
    found
}

#[test]
fn namespace_identity() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("ns.cpp"),
        "int root;\n\
         int *ptr = &root;\n\
         namespace a { int ax; int *ptr = &ax; }\n\
         namespace b { int bx; int *ptr = &bx; }\n\
         namespace outer { namespace inner { int value; int *ptr = &value; } }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();

    // Every `ptr` declaration lowered to its own variable: four entries,
    // four distinct ids.
    let ptrs: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == "ptr")
        .collect();
    assert_eq!(
        ptrs.len(),
        4,
        "expected one variable per `ptr` declaration: {ptrs:?}"
    );
    let mut ids: Vec<_> = ptrs.iter().map(|v| v.id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 4, "ptr declarations must keep distinct ids");

    // Display name stays the bare token written at each declaration site.
    for v in &ptrs {
        assert_eq!(v.name, "ptr");
    }

    let bare_ptr = ptrs
        .iter()
        .find(|v| v.qualified_name.is_none())
        .expect("the file-scope `ptr` carries no namespace qualification");
    let a_ptr = ptrs
        .iter()
        .find(|v| v.qualified_name.as_deref() == Some("a::ptr"))
        .expect("namespace a's ptr must carry canonical name a::ptr");
    let b_ptr = ptrs
        .iter()
        .find(|v| v.qualified_name.as_deref() == Some("b::ptr"))
        .expect("namespace b's ptr must carry canonical name b::ptr");
    let outer_inner_ptr = ptrs
        .iter()
        .find(|v| v.qualified_name.as_deref() == Some("outer::inner::ptr"))
        .expect("nested namespace's ptr must carry canonical name outer::inner::ptr");

    // The bare index is the collision surface #133 reports: a namespaced
    // `ptr` must never claim the slot a lookup of the unqualified global
    // name resolves through.
    assert_eq!(
        program.symbols.global_by_name.get("ptr").copied(),
        Some(bare_ptr.id),
        "the unqualified global index must resolve to the file-scope ptr, \
         not a namespace-scoped one"
    );
    assert_eq!(
        program.symbols.global_by_name.get("a::ptr").copied(),
        Some(a_ptr.id)
    );
    assert_eq!(
        program.symbols.global_by_name.get("b::ptr").copied(),
        Some(b_ptr.id)
    );
    assert_eq!(
        program
            .symbols
            .global_by_name
            .get("outer::inner::ptr")
            .copied(),
        Some(outer_inner_ptr.id)
    );
}

/// Stage 2 (issue #133): a `static` data member is one canonical variable,
/// not a per-instance field.
///
/// Before this stage, class scanning swept every non-function field
/// declaration — `static` ones included — into the class's instance-field
/// layout, so `Holder::member`'s storage was never registered as a
/// `Variable` at all and its out-of-class initializer (`&object`) was lost.
/// This fixture is the plan's required regression: an in-class-declared,
/// out-of-class-defined member (`Holder::member`), a namespace-nested one
/// declared and defined the same way (`nest::Box::member`), and an inline
/// one defined entirely in-class (`nest::Inline::member`) — beside an
/// ordinary instance field (`ordinary`) that must stay untouched.
#[test]
fn static_member_shares_one_variable_with_its_out_of_class_definition() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("statics.cpp"),
        "int object;\n\
         struct Holder { static int *member; int *ordinary; };\n\
         int *Holder::member = &object;\n\
         namespace nest {\n\
         struct Box { static int *member; };\n\
         int *Box::member = &object;\n\
         struct Inline { inline static int *member = &object; };\n\
         }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();

    let object = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "object")
        .expect("file-scope `object` must be registered");

    // Exactly one variable per canonical static member: three `member`
    // declarations, three distinct display-named entries, none an instance
    // field's positional slot.
    let members: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == "member")
        .collect();
    assert_eq!(
        members.len(),
        3,
        "expected one variable per static member declaration: {members:?}"
    );

    // Line numbers in the fixture written above (1-indexed):
    //   2: struct Holder { static int *member; ... };   <- in-class declaration
    //   3: int *Holder::member = &object;                <- out-of-class definition
    //   5: struct Box { static int *member; };            <- in-class declaration
    //   6: int *Box::member = &object;                    <- out-of-class definition
    //   7: struct Inline { inline static int *member = &object; };  <- declared AND defined here
    //
    // `(qualified, in_class_declaration_line, expected_span_line)`: for a
    // member with a separate out-of-class definition, the declaration and
    // definition lines differ and the reconciled variable's span must be
    // the DEFINITION's — reconciliation is definition precedence, the same
    // way a function definition's span supersedes its prototype's
    // (`SymbolTable::register_function`). `nest::Inline::member` has no
    // separate out-of-class definition, so both lines coincide.
    for (qualified, in_class_declaration_line, expected_span_line) in [
        ("Holder::member", 2, 3),
        ("nest::Box::member", 5, 6),
        ("nest::Inline::member", 7, 7),
    ] {
        let member = the_variable_named(&program, qualified);
        assert_eq!(member.name, "member", "{qualified} displays as `member`");
        assert_eq!(
            member.storage,
            StorageClass::Global,
            "{qualified} has external storage, not FileStatic — the `static` \
             keyword on a data member means shared instance, not file scope"
        );
        assert!(member.is_defined, "{qualified} must be marked defined");
        assert!(
            member.is_pointer,
            "{qualified} is declared as a pointer member"
        );
        assert_eq!(
            member.span.line, expected_span_line,
            "{qualified}'s span must be the DEFINITION's line ({expected_span_line}), \
             not the in-class declaration's — a reconciled static member takes \
             definition precedence for its span, the same way a function \
             definition's span supersedes its prototype's"
        );
        if expected_span_line != in_class_declaration_line {
            assert_ne!(
                member.span.line, in_class_declaration_line,
                "{qualified}'s span must have moved off the in-class \
                 declaration's line ({in_class_declaration_line}) once the \
                 out-of-class definition reconciled onto it"
            );
        }
        assert!(
            program.flow.contains(&FlowConstraint::AddrOfVar {
                dst: member.id,
                src: object.id,
            }),
            "{qualified}'s initializer `&object` must flow exactly once, \
             into the SAME variable the in-class declaration registered"
        );
    }

    // `ordinary` remains a plain instance field: one field on `Holder`'s
    // layout, and no `Variable` was allocated for it.
    let holder_ty = program
        .types
        .type_id_by_tag("Holder", TypeKind::Struct)
        .expect("Holder must register a struct layout");
    match program.types.get(holder_ty).desc.as_ref() {
        TypeDesc::Struct { fields, .. } => {
            assert_eq!(
                fields
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
                vec!["ordinary"],
                "Holder's instance-field layout must hold only `ordinary`, \
                 with `member` routed to canonical-variable registration \
                 instead"
            );
        }
        other => panic!("Holder did not register a struct layout: {other:?}"),
    }
    assert!(
        !program
            .symbols
            .variables
            .iter()
            .any(|v| v.name == "ordinary"),
        "an ordinary instance field must never become a Variable"
    );
}

/// An out-of-class definition without an initializer (`int *Holder::member;`)
/// is still the member's definition: `is_defined` must flip and the span
/// must move onto the definition's line, with no initializer flow to emit.
#[test]
fn static_member_uninitialized_definition_is_still_defined() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("uninit.cpp"),
        "struct Holder { static int *member; };\n\
         int *Holder::member;\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();

    let members: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.qualified_name.as_deref() == Some("Holder::member"))
        .collect();
    assert_eq!(
        members.len(),
        1,
        "declaration and definition share one VarId"
    );
    let member = members[0];
    assert!(
        member.is_defined,
        "an out-of-class definition, even without an \
             initializer, is a definition"
    );
    // Line 1 declares `Holder::member` in-class; line 2 is the out-of-class
    // definition. The reconciled span must be the definition's, not the
    // declaration's, even with no initializer to lower.
    assert_eq!(
        member.span.line, 2,
        "the span must move onto the out-of-class definition's line, not \
         stay on the in-class declaration's (line 1)"
    );
    assert!(
        !program
            .flow
            .iter()
            .any(|f| matches!(f, FlowConstraint::AddrOfVar { dst, .. } if *dst == member.id)),
        "no initializer means no initializer flow"
    );
}

/// A function-pointer-typed static member exercises the callback-shaped
/// declarator (`void (*Cls::cb)(int)`), which reaches the qualified-name
/// reconciliation through the fn-ptr-declarator path rather than the plain
/// one — the two declarator-dispatch gaps Stage 2 has to close together.
#[test]
fn static_member_callback_typed() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("callback.cpp"),
        "void handler(int x);\n\
         struct Holder { static void (*cb)(int); };\n\
         void (*Holder::cb)(int) = &handler;\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();

    let handler = program
        .symbols
        .resolve_function("handler")
        .expect("handler must be registered");
    let cb = the_variable_named(&program, "Holder::cb");
    assert!(cb.is_pointer, "a callback member is pointer-typed");
    // `(*cb)(int)` is a pointer declarator wrapping a function-pointer
    // shape, the same `Ptr(FnPtr)` an ordinary (unqualified) callback
    // variable of this spelling gets — see `walk_declarator_shape`.
    assert!(matches!(
        program.types.get(cb.type_id).desc.as_ref(),
        TypeDesc::Ptr(inner) if matches!(inner.as_ref(), TypeDesc::FnPtr { .. })
    ));
    // Line 2 declares `cb` in-class; line 3 is the out-of-class definition —
    // the fn-ptr-declarator reconciliation path must move the span too, the
    // same as the plain-declarator path does.
    assert_eq!(
        cb.span.line, 3,
        "a callback member's span must also move onto the out-of-class \
         definition's line, not stay on the in-class declaration's (line 2)"
    );
    assert!(
        program.flow.contains(&FlowConstraint::AddrOfFn {
            dst: cb.id,
            callee: handler,
        }),
        "the out-of-class definition's `&handler` must flow into the \
         SAME variable the in-class declaration registered"
    );
}

/// A genuinely nested class (as opposed to a namespace-nested one, already
/// covered by the required fixture) spells its static member's out-of-class
/// definition with the full nested chain (`Outer::Inner::member`).
#[test]
fn static_member_nested_class_spelling() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("nested.cpp"),
        "int object;\n\
         struct Outer { struct Inner { static int *member; }; };\n\
         int *Outer::Inner::member = &object;\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();

    let object = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "object")
        .expect("file-scope `object` must be registered");
    let member = the_variable_named(&program, "Outer::Inner::member");
    assert_eq!(member.name, "member");
    // A struct with a static data member cannot be a C struct, so `Inner`
    // is `Outer::Inner`, not a file-scope tag its in-class declaration
    // would register a second, `Inner::member`, variable under.
    let members: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == "member")
        .collect();
    assert_eq!(members.len(), 1, "{members:?}");
    // Line 2 declares the nested member in-class; line 3 is the out-of-class
    // definition, spelled with the full nested chain.
    assert_eq!(
        member.span.line, 3,
        "a nested class's static member span must also move onto the \
         out-of-class definition's line, not stay on the in-class \
         declaration's (line 2)"
    );
    assert!(program.flow.contains(&FlowConstraint::AddrOfVar {
        dst: member.id,
        src: object.id,
    }));
}

/// An inline member function that reads the static member textually BEFORE
/// its out-of-class definition must not disturb registration: the field-
/// layout pass registers every static member's `VarId` before any method
/// body — inline or not — is lowered, so the out-of-class definition still
/// finds and reuses the same entry regardless of body order.
#[test]
fn static_member_registered_before_inline_body_that_uses_it() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("use_before_def.cpp"),
        "int object;\n\
         struct Holder {\n\
         static int *member;\n\
         int *get() { return member; }\n\
         };\n\
         int *Holder::member = &object;\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();

    let object = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "object")
        .expect("file-scope `object` must be registered");
    let members: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.qualified_name.as_deref() == Some("Holder::member"))
        .collect();
    assert_eq!(
        members.len(),
        1,
        "an inline body sitting between declaration and definition must not \
         cause a duplicate registration: {members:?}"
    );
    let member = members[0];
    assert!(member.is_defined);
    // Line 3 declares `member` in-class; line 6 is the out-of-class
    // definition. An inline method sitting between them (line 4) must not
    // stop the span from moving onto the definition's line.
    assert_eq!(
        member.span.line, 6,
        "the span must move onto the out-of-class definition's line even \
         with an inline method between it and the in-class declaration \
         (line 3)"
    );
    assert!(program.flow.contains(&FlowConstraint::AddrOfVar {
        dst: member.id,
        src: object.id,
    }));
}

// --- Stage 4 (issue #133): qualified identity across translation units. ---

/// Every `VarId` whose address `object`'s is copied into (`dst = &object`).
fn addresses_of(program: &trace_ir::Program, object: trace_ir::VarId) -> Vec<trace_ir::VarId> {
    program
        .flow
        .iter()
        .filter_map(|f| match f {
            FlowConstraint::AddrOfVar { dst, src } if *src == object => Some(*dst),
            _ => None,
        })
        .collect()
}

/// What `reader`'s plain copies read (`reader = source`).
fn sources_of(program: &trace_ir::Program, reader: trace_ir::VarId) -> Vec<trace_ir::VarId> {
    program
        .flow
        .iter()
        .filter_map(|f| match f {
            FlowConstraint::Copy { dst, src } if *dst == reader => Some(*src),
            _ => None,
        })
        .collect()
}

fn the_global<'p>(program: &'p trace_ir::Program, name: &str) -> &'p trace_ir::Variable {
    let found: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == name && v.fn_id.is_none())
        .collect();
    assert_eq!(found.len(), 1, "expected one {name}: {found:?}");
    found[0]
}

/// Without link metadata each unit keeps its own copy of a header's globals
/// (the whole-program policy, `docs/ANALYSIS.md`, "Link targets and weak
/// symbols"); a qualified reference in either unit still binds a copy of the
/// qualified declaration, never the bare global of the same leaf name.
#[test]
fn shared_header_qualified_references_bind_the_qualified_declaration() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("h.h"),
        "struct X { int v; };\nnamespace ns { extern X *ptr; }\nextern X *ptr;\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("a.cpp"),
        "#include \"h.h\"\nX object;\nnamespace ns { X *ptr = &object; }\nX *ptr;\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("b.cpp"),
        "#include \"h.h\"\nX *seen, *seen_bare;\n\
         void read() { seen = ns::ptr; seen_bare = ptr; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let name_of = |id| {
        let v = program.symbols.variable(id);
        v.lookup_name().to_string()
    };
    let read = |reader| {
        sources_of(&program, the_global(&program, reader).id)
            .into_iter()
            .map(name_of)
            .collect::<Vec<_>>()
    };
    assert_eq!(read("seen"), ["ns::ptr"]);
    assert_eq!(read("seen_bare"), ["ptr"]);
    // The definition's initializer is lowered once, into the qualified one.
    let object = the_global(&program, "object").id;
    let stores: Vec<_> = addresses_of(&program, object)
        .into_iter()
        .map(name_of)
        .collect();
    assert_eq!(stores, ["ns::ptr"]);
}

/// A namespace reopened any number of times is one scope: its redeclarations
/// name the variable its definition initializes, the initializer is lowered
/// once, and an unqualified read inside a later block of it binds the same
/// variable.
#[test]
fn a_reopened_namespace_redeclares_one_variable_initialized_once() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("reopen.cpp"),
        "struct X { int v; };\n\
         X object;\n\
         namespace ns { extern X *ptr; }\n\
         namespace ns { X *ptr = &object; }\n\
         X *seen, *seen_inside;\n\
         namespace ns { void read_inside() { seen_inside = ptr; } }\n\
         void read() { seen = ns::ptr; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let object = the_global(&program, "object").id;
    let initialized = addresses_of(&program, object);
    assert_eq!(initialized.len(), 1, "one initializer: {initialized:?}");
    let ptr = program.symbols.variable(initialized[0]);
    assert_eq!(ptr.qualified_name.as_deref(), Some("ns::ptr"));
    assert!(ptr.is_defined);
    assert_eq!(
        sources_of(&program, the_global(&program, "seen").id),
        [ptr.id]
    );
    assert_eq!(
        sources_of(&program, the_global(&program, "seen_inside").id),
        [ptr.id]
    );
}

/// Internal-linkage variables in a namespace are the file's own storage:
/// neither an anonymous namespace's variable nor a namespace `static` is an
/// external symbol, and both still resolve through their scope.
#[test]
fn internal_namespace_variables_are_not_external_symbols() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("internal.cpp"),
        "struct X { int v; };\n\
         X object;\n\
         namespace { X *hidden = &object; }\n\
         namespace ns { static X *internal = &object; }\n\
         namespace ns { namespace { X *nested = &object; } }\n\
         X *seen_hidden, *seen_internal, *seen_nested;\n\
         void read() {\n\
             seen_hidden = hidden;\n\
             seen_internal = ns::internal;\n\
             seen_nested = ns::nested;\n\
         }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let object = the_global(&program, "object").id;
    for (name, qualified, reader) in [
        ("hidden", "hidden", "seen_hidden"),
        ("internal", "ns::internal", "seen_internal"),
        ("nested", "ns::nested", "seen_nested"),
    ] {
        let var = the_global(&program, name);
        assert_eq!(var.storage, StorageClass::FileStatic, "{var:?}");
        assert_eq!(var.lookup_name(), qualified);
        assert_eq!(var.external_symbol_name(), None, "{var:?}");
        assert!(addresses_of(&program, object).contains(&var.id), "{name}");
        assert_eq!(
            sources_of(&program, the_global(&program, reader).id),
            [var.id],
            "{reader}"
        );
    }
    // A top-level anonymous namespace names no scope: `hidden` is spelled
    // plainly, like a file `static`, with no qualified name to allocate.
    assert_eq!(the_global(&program, "hidden").qualified_name, None);
}

/// A dependency header's qualified variables are declarations the target
/// resolves against, and nothing more: no initializer or body of the
/// dependency contributes flow (`docs/ANALYSIS.md`, "Dependency roots").
#[test]
fn dependency_qualified_variables_declare_without_initializer_flow() {
    let dir = tempfile::tempdir().unwrap();
    let dep = dir.path().join("dep");
    fs::create_dir(&dep).unwrap();
    fs::write(
        dep.join("api.h"),
        "struct X { int v; };\n\
         extern X dep_object;\n\
         namespace dep { X *initialized = &dep_object; }\n\
         struct Holder {\n\
             static X *member;\n\
             inline static X *inline_member = &dep_object;\n\
         };\n\
         X *Holder::member = &dep_object;\n\
         inline X *get() { return Holder::inline_member; }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.cpp"),
        "#include <api.h>\n\
         X *seen_ns, *seen_member, *seen_inline;\n\
         void target() {\n\
             seen_ns = dep::initialized;\n\
             seen_member = Holder::member;\n\
             seen_inline = Holder::inline_member;\n\
         }\n",
    )
    .unwrap();
    for jobs in [1, 2] {
        let program = trace_parse::build_program_with_jobs(
            dir.path(),
            &PreprocessOptions::new().with_dep(&dep),
            jobs,
        )
        .unwrap();
        // Without link metadata each includer holds its own copy of a
        // header's globals; any of them is the declaration.
        let copies = |qualified: &str| -> Vec<_> {
            program
                .symbols
                .variables
                .iter()
                .filter(|v| v.lookup_name() == qualified)
                .collect()
        };
        for (qualified, reader) in [
            ("dep::initialized", "seen_ns"),
            ("Holder::member", "seen_member"),
            ("Holder::inline_member", "seen_inline"),
        ] {
            let declared = copies(qualified);
            assert!(!declared.is_empty(), "{qualified} is not declared");
            assert!(declared.iter().all(|v| program.is_dep_file(v.span.file)));
            let read = sources_of(&program, the_global(&program, reader).id);
            assert_eq!(read.len(), 1, "{reader} reads {read:?}");
            assert!(
                declared.iter().any(|v| v.id == read[0]),
                "{reader} reads the dependency's {qualified}"
            );
        }
        // Every fact left is the target's own read; nothing the dependency
        // initializes or returns survives.
        for object in copies("dep_object") {
            assert!(
                addresses_of(&program, object.id).is_empty(),
                "dependency initializer flow leaked: {:?}",
                program.flow
            );
        }
        assert!(program.fn_returns.is_empty(), "{:?}", program.fn_returns);
        assert_eq!(program.flow.len(), 3, "{:?}", program.flow);
    }
}

/// #133 review: an out-of-class static member definition with no in-class
/// entry to reconcile with (its class's header was not found) keeps its
/// anonymous namespace's internal linkage.
#[test]
fn unreconciled_static_member_definition_in_an_anonymous_namespace_is_internal() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("unreconciled.cpp"),
        "#include \"holder_not_in_tree.h\"\n\
         int object;\n\
         namespace { int *Holder::m = &object; }\n\
         int *Visible::m = &object;\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let hidden = the_variable_named(&program, "Holder::m");
    assert_eq!(hidden.storage, StorageClass::FileStatic, "{hidden:?}");
    assert_eq!(hidden.external_symbol_name(), None);
    let visible = the_variable_named(&program, "Visible::m");
    assert_eq!(visible.storage, StorageClass::Global, "{visible:?}");
}

/// #133 review: a pointer-to-member declarator (`int *Foo::*pm`) names no
/// static member, local or global: it registers as an ordinary variable.
#[test]
fn a_pointer_to_member_declarator_is_no_static_member_definition() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("ptm.cpp"),
        "struct Foo { int *n; };\n\
         int *Foo::*global_pm = &Foo::n;\n\
         void f() { int *Foo::*pm = &Foo::n; (void)pm; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    for var in &program.symbols.variables {
        assert!(!var.name.starts_with('*'), "{var:?}");
        assert!(
            var.qualified_name.is_none(),
            "a pointer to member is no static member: {var:?}"
        );
    }
    let local = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name.ends_with("pm") && !v.name.contains("global"))
        .expect("the local pointer to member");
    assert_eq!(local.storage, StorageClass::Local, "{local:?}");
}

/// #133 review: an uninitialized out-of-class definition with a plain
/// declarator (`cb_t H::cb;`, `cb_t ns::ncb;`) defines the declared
/// variable, as a pointer declarator's (`int *H::p;`) does.
#[test]
fn an_uninitialized_plain_declarator_definition_is_reconciled() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("uninit.cpp"),
        "typedef void (*cb_t)();\n\
         struct H { static cb_t cb; };\n\
         namespace ns { extern cb_t ncb; }\n\
         cb_t H::cb;\n\
         cb_t ns::ncb;\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    for (qualified, line) in [("H::cb", 4), ("ns::ncb", 5)] {
        let var = the_variable_named(&program, qualified);
        assert!(var.is_defined, "{var:?}");
        assert_eq!(var.span.line, line, "{var:?}");
    }
}

/// #133 review: a static data member never has C language linkage, so an
/// out-of-class definition inside `extern "C"` whose class this unit did not
/// see still links as `H::cb`, not as a bare `cb`.
#[test]
fn a_static_member_defined_inside_extern_c_keeps_cpp_linkage() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("def.cpp"),
        "typedef void (*CB)();\nvoid f() {}\nextern \"C\" { CB H::cb = f; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let cb = the_variable_named(&program, "H::cb");
    assert!(!cb.c_linkage, "{cb:?}");
    assert_eq!(cb.external_symbol_name(), Some("H::cb"));
}

/// #142 review: a namespace variable whose first declaration is qualified
/// (`extern "C" CB ns::cb;`, the namespace's own declaration of it not seen)
/// takes the `extern "C"` around it and links by its bare name, as an
/// unqualified declaration inside the namespace does.
#[test]
fn a_qualified_first_declaration_inside_extern_c_links_by_bare_name() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("decl.cpp"),
        "typedef void (*CB)();\n\
         namespace ns { void f(); }\n\
         extern \"C\" CB ns::cb;\n\
         extern \"C\" { CB ns::cb2; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    for (qualified, bare) in [("ns::cb", "cb"), ("ns::cb2", "cb2")] {
        let var = the_variable_named(&program, qualified);
        assert!(var.c_linkage, "{var:?}");
        assert_eq!(var.external_symbol_name(), Some(bare));
    }
}

/// #142 review: a C++ union is a class, so its `static` data member is one
/// storage shared with its out-of-class definition, not an instance field.
#[test]
fn a_union_static_member_is_one_storage() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("union.cpp"),
        "int object;\n\
         namespace ns {\n\
         union U { int *slot; static int *m; };\n\
         int *U::m = &object;\n\
         }\n\
         int *read() { return ns::U::m; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let m = the_variable_named(&program, "ns::U::m");
    assert!(m.is_defined, "{m:?}");
    assert_eq!(m.span.line, 4, "{m:?}");
    let object = the_global(&program, "object");
    assert_eq!(addresses_of(&program, object.id), vec![m.id]);
    let union_ty = program
        .types
        .type_id_by_tag("ns::U", TypeKind::Union)
        .expect("ns::U must register a union layout");
    match program.types.get(union_ty).desc.as_ref() {
        TypeDesc::Union { fields, .. } => assert_eq!(
            fields
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["slot"],
        ),
        other => panic!("ns::U did not register a union layout: {other:?}"),
    }
}

/// #142 review: a union's in-class static member initializer is lowered with
/// the rest of its members, at top level and nested in a class alike.
#[test]
fn a_union_in_class_static_initializer_flows() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("union_inline.cpp"),
        "int object;\n\
         int other;\n\
         union U { int *slot; inline static int *m = &object; };\n\
         struct Outer { union Inner { int *slot; inline static int *n = &other; }; };\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    for (object, member) in [("object", "U::m"), ("other", "Outer::Inner::n")] {
        let m = the_variable_named(&program, member);
        assert!(m.is_defined, "{m:?}");
        let object = the_global(&program, object);
        assert_eq!(addresses_of(&program, object.id), vec![m.id], "{member}");
    }
}

/// #142 review: `T H::m();` declares a function, as `T w();` does; it never
/// defines the static member `H::m` by an empty direct initializer.
#[test]
fn an_empty_qualified_direct_initializer_defines_nothing() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("empty.cpp"),
        "struct H { static int *m; };\n\
         int *H::m();\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let m = the_variable_named(&program, "H::m");
    assert!(!m.is_defined, "{m:?}");
    assert_eq!(m.span.line, 1, "{m:?}");
}

/// Build `source` as one C++ unit.
fn build_cpp(source: &str) -> trace_ir::Program {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("unit.cpp"), source).unwrap();
    build_program(dir.path(), &PreprocessOptions::new()).unwrap()
}

/// What the function named `name` returns.
fn returns_of<'p>(program: &'p trace_ir::Program, name: &str) -> &'p [ReturnFlow] {
    let function = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no function {name}"));
    program
        .fn_returns
        .get(&function.id)
        .map_or(&[], Vec::as_slice)
}

/// #142 review: `return field;` in a member body names the instance field,
/// which hides a global of the name now and in the end-of-unit pass.
#[test]
fn a_returned_name_a_field_hides_reads_no_global() {
    let program = build_cpp(
        "int *field;\n\
         struct S { int *field; int *get() { return field; } };\n",
    );
    let global = the_global(&program, "field");
    let returns = returns_of(&program, "S::get");
    assert!(
        !returns
            .iter()
            .any(|r| matches!(r, ReturnFlow::Copy { src } if *src == global.id)),
        "{returns:?}"
    );
}

/// #142 review: under `&`, a declared name shadows a function of the same
/// name, as it does in a value.
#[test]
fn an_address_of_a_variable_is_not_taken_of_a_function_it_shadows() {
    let program = build_cpp(
        "void x();\n\
         int **out;\n\
         int *outer;\n\
         void use() { int *x = outer; out = &x; }\n\
         int **ret() { static int *x; return &x; }\n",
    );
    assert!(
        !program
            .flow
            .iter()
            .any(|f| matches!(f, FlowConstraint::AddrOfFn { .. })),
        "{:?}",
        program.flow
    );
    assert!(
        program.flow.iter().any(|f| matches!(
            f,
            FlowConstraint::AddrOfVar { src, .. } if program.symbols.variable(*src).name == "x"
        )),
        "{:?}",
        program.flow
    );
    assert!(
        matches!(returns_of(&program, "ret"), [ReturnFlow::AddrOfVar { .. }]),
        "{:?}",
        returns_of(&program, "ret")
    );
}

/// #142 review: `using ::ns::var;` imports `ns::var`; a globally qualified
/// target is spelled canonically, without its leading `::`.
#[test]
fn a_globally_qualified_using_declaration_imports_the_variable() {
    let program = build_cpp(
        "namespace ns { int *var; }\n\
         using ::ns::var;\n\
         int *read() { return var; }\n",
    );
    let var = the_variable_named(&program, "ns::var");
    assert!(
        matches!(returns_of(&program, "read"), [ReturnFlow::Copy { src }] if *src == var.id),
        "{:?}",
        returns_of(&program, "read")
    );
}

/// #142 review: the innermost `using namespace` directive is asked first:
/// `Outer::B::v` over the file-scope directive's `A::v`.
#[test]
fn the_innermost_using_directive_is_asked_first() {
    let program = build_cpp(
        "namespace A { int *v; }\n\
         using namespace A;\n\
         namespace Outer {\n\
         namespace B { int *v; }\n\
         using namespace B;\n\
         int *read() { return v; }\n\
         }\n",
    );
    let inner = the_variable_named(&program, "Outer::B::v");
    assert!(
        matches!(returns_of(&program, "Outer::read"), [ReturnFlow::Copy { src }] if *src == inner.id),
        "{:?}",
        returns_of(&program, "Outer::read")
    );
}

/// #142 review: a file-scope definition keeps the C linkage its `extern "C"`
/// declaration gave it, as a namespace variable's does.
#[test]
fn a_file_scope_definition_keeps_its_declarations_c_linkage() {
    let program = build_cpp(
        "typedef void (*CB)();\n\
         void good() {}\n\
         extern \"C\" CB cb;\n\
         CB cb = good;\n",
    );
    let cbs: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == "cb")
        .collect();
    assert!(!cbs.is_empty());
    assert!(cbs.iter().all(|v| v.c_linkage), "{cbs:?}");
}

/// `/code-review`: a `using ns2::x;` inside a namespace declares `x` there,
/// so it hides a global `x`, for a variable and a function alike.
#[test]
fn a_namespace_using_declaration_hides_the_global_name() {
    let program = build_cpp(
        "int *x;\n\
         void h();\n\
         namespace ns2 { int *x; void h(); }\n\
         namespace A {\n\
         using ns2::x;\n\
         using ns2::h;\n\
         int *read() { return x; }\n\
         void (*get())() { return h; }\n\
         }\n",
    );
    let imported = the_variable_named(&program, "ns2::x");
    assert!(
        matches!(returns_of(&program, "A::read"), [ReturnFlow::Copy { src }] if *src == imported.id),
        "{:?}",
        returns_of(&program, "A::read")
    );
    match returns_of(&program, "A::get") {
        [ReturnFlow::AddrOfFn { callee }] => {
            assert_eq!(program.symbols.function(*callee).name, "ns2::h");
        }
        other => panic!("{other:?}"),
    }
}

/// `/code-review`: a bare member name in a lambda reads through the `this`
/// the lambda captured, never through the lambda's first parameter; a lambda
/// that captures no `this` reads no member.
#[test]
fn a_lambda_reads_members_through_its_captured_this() {
    let program = build_cpp(
        "typedef void (*cb_t)();\n\
         struct Other { cb_t first; };\n\
         struct H {\n\
         cb_t cb;\n\
         void v(Other *o) {\n\
         auto l = [this](Other *p) { cb(); };\n\
         auto m = [](Other *q) { cb(); };\n\
         l(o);\n\
         m(o);\n\
         }\n\
         };\n",
    );
    let bases: Vec<_> = program
        .flow
        .iter()
        .filter_map(|f| match f {
            FlowConstraint::GepField {
                base, field_name, ..
            } if field_name == "cb" => Some(program.symbols.variable(*base).name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(bases, vec!["this"], "{:?}", program.flow);
}

/// `/code-review`: a union is a class whose data members are data: a call
/// through a union's function-pointer member invents no member function,
/// and a union's instance member hides an outer class's static member.
#[test]
fn a_union_data_member_is_no_member_function() {
    let program = build_cpp(
        "typedef void (*cb_t)();\n\
         void b() {}\n\
         union V { cb_t f; void *p; };\n\
         V gv;\n\
         void run() { gv.f = b; gv.f(); }\n\
         namespace n { union U { cb_t g; }; U u; void r() { u.g(); } }\n\
         struct Outer { static cb_t m; union In { cb_t m; cb_t get() { return m; } }; };\n",
    );
    for invented in ["V::f", "n::U::g"] {
        assert!(
            !program.symbols.functions.iter().any(|f| f.name == invented),
            "{invented} is a data member"
        );
    }
    let outer = the_variable_named(&program, "Outer::m");
    assert!(
        !returns_of(&program, "Outer::In::get")
            .iter()
            .any(|r| matches!(r, ReturnFlow::Copy { src } if *src == outer.id)),
        "{:?}",
        returns_of(&program, "Outer::In::get")
    );
}

/// `/code-review`: a class defined in a declaration or a typedef lowers its
/// members as one standing alone does: the in-class initializer flows, and
/// the member stays one variable.
#[test]
fn a_class_defined_in_a_declaration_lowers_its_members() {
    let program = build_cpp(
        "int a;\n\
         int b;\n\
         struct S4 { inline static int *r = &a; } s4;\n\
         typedef struct T3 { inline static int *q = &b; } T3A;\n",
    );
    for (object, member) in [("a", "S4::r"), ("b", "T3::q")] {
        let m = the_variable_named(&program, member);
        let object = the_global(&program, object);
        assert_eq!(addresses_of(&program, object.id), vec![m.id], "{member}");
    }
}

/// `/code-review`: in a body, `Widget make(Config);` naming a function in
/// scope redeclares it even when `Config` is unknown, so a later `make(...)`
/// calls the function rather than an object.
#[test]
fn a_block_scope_redeclaration_of_a_visible_function_declares_it() {
    let program = build_cpp(
        "struct Widget {};\n\
         Widget make(int);\n\
         void use() { Widget make(Config); make(0); }\n",
    );
    assert!(
        !program
            .symbols
            .variables
            .iter()
            .any(|v| v.name == "make" && v.fn_id.is_some()),
        "`make` is no local object"
    );
}

/// Assignment to an implicit `this` member variable emits a GEP temp and a Store.
#[test]
fn assignment_to_implicit_and_explicit_this_member_variable() {
    let program = build_cpp(
        "struct Target { int val; };\n\
         class Observer {\n\
             Target *target_ = nullptr;\n\
             Target *other_ = nullptr;\n\
         public:\n\
             void SetTarget(Target *t) {\n\
                 target_ = t;\n\
                 this->other_ = t;\n\
             }\n\
         };\n",
    );
    let gep_fields: Vec<_> = program
        .flow
        .iter()
        .filter_map(|f| match f {
            FlowConstraint::GepField {
                base,
                field_name,
                dst,
                ..
            } => {
                let base_name = program.symbols.variable(*base).name.as_str();
                Some((*dst, base_name, field_name.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        gep_fields
            .iter()
            .map(|(_, b, f)| (*b, *f))
            .collect::<Vec<_>>(),
        vec![("this", "target_"), ("this", "other_")]
    );

    let stores: Vec<_> = program
        .flow
        .iter()
        .filter_map(|f| match f {
            FlowConstraint::Store { dst, src } => {
                let src_name = program.symbols.variable(*src).name.as_str();
                Some((*dst, src_name))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        stores,
        vec![(gep_fields[0].0, "t"), (gep_fields[1].0, "t")],
        "flow constraints: {:?}",
        program.flow
    );
}

#[test]
fn nested_implicit_this_member_assignment_and_load() {
    let program = build_cpp(
        "struct Inner { int *val; };\n\
         class Container {\n\
             Inner inner_;\n\
         public:\n\
             void Set(int *p) {\n\
                 inner_.val = p;\n\
             }\n\
             int* Get() {\n\
                 return inner_.val;\n\
             }\n\
         };\n",
    );
    let gep_names: Vec<_> = program
        .flow
        .iter()
        .filter_map(|f| match f {
            FlowConstraint::GepField {
                base,
                field_name,
                dst,
                ..
            } => {
                let base_name = program.symbols.variable(*base).name.as_str();
                Some((*dst, base_name, field_name.as_str()))
            }
            _ => None,
        })
        .collect();
    assert!(
        gep_names
            .iter()
            .any(|(_, b, f)| *b == "this" && *f == "inner_"),
        "must emit GEP for this->inner_"
    );
    assert!(
        gep_names.iter().any(|(_, _, f)| *f == "val"),
        "must emit GEP for inner_.val"
    );

    let has_store = program.flow.iter().any(|f| match f {
        FlowConstraint::Store { src, .. } => program.symbols.variable(*src).name == "p",
        _ => false,
    });
    assert!(has_store, "must emit Store of p into inner_.val");

    let get_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name.ends_with("Get"))
        .expect("Get fn");
    let returns = program.fn_returns.get(&get_fn.id).expect("Get returns");
    assert!(
        !returns.is_empty(),
        "Get() returning inner_.val must emit return flow"
    );
}

#[test]
fn inherited_implicit_this_member_assignment() {
    let program = build_cpp(
        "struct Base {\n\
             int *base_val_;\n\
         };\n\
         class Derived : public Base {\n\
         public:\n\
             void SetBase(int *p) {\n\
                 base_val_ = p;\n\
             }\n\
             int* GetBase() {\n\
                 return base_val_;\n\
             }\n\
         };\n",
    );
    let gep_base = program
        .flow
        .iter()
        .find_map(|f| match f {
            FlowConstraint::GepField {
                base,
                field_name,
                dst,
                ..
            } if field_name == "base_val_" && program.symbols.variable(*base).name == "this" => {
                Some(*dst)
            }
            _ => None,
        })
        .expect("must emit GEP for this->base_val_ in Derived");

    let has_store = program.flow.iter().any(|f| match f {
        FlowConstraint::Store { dst, src } => {
            *dst == gep_base && program.symbols.variable(*src).name == "p"
        }
        _ => false,
    });
    assert!(has_store, "must emit Store of p into this->base_val_");

    let get_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name.ends_with("GetBase"))
        .expect("GetBase fn");
    let returns = program.fn_returns.get(&get_fn.id).expect("GetBase returns");
    assert!(
        !returns.is_empty(),
        "GetBase() returning base_val_ must emit return flow"
    );
}

#[test]
fn implicit_this_member_array_subscript_and_fn_ptr() {
    let program = build_cpp(
        "typedef void (*Callback)(int);\n\
         void handler(int x);\n\
         class Dispatcher {\n\
             Callback handlers_[4];\n\
             Callback single_cb_;\n\
         public:\n\
             void Init() {\n\
                 handlers_[0] = handler;\n\
                 single_cb_ = handler;\n\
             }\n\
             void Run() {\n\
                 handlers_[0](42);\n\
                 single_cb_(42);\n\
             }\n\
         };\n",
    );
    let gep_handlers = program
        .flow
        .iter()
        .find_map(|f| match f {
            FlowConstraint::GepField {
                base,
                field_name,
                dst,
                ..
            } if field_name == "handlers_" && program.symbols.variable(*base).name == "this" => {
                Some(*dst)
            }
            _ => None,
        })
        .expect("must emit GEP for this->handlers_");

    let gep_single = program
        .flow
        .iter()
        .find_map(|f| match f {
            FlowConstraint::GepField {
                base,
                field_name,
                dst,
                ..
            } if field_name == "single_cb_" && program.symbols.variable(*base).name == "this" => {
                Some(*dst)
            }
            _ => None,
        })
        .expect("must emit GEP for this->single_cb_");

    let has_handlers_store = program.flow.iter().any(|f| match f {
        FlowConstraint::Store { dst, .. } => *dst == gep_handlers,
        _ => false,
    });
    assert!(has_handlers_store, "must emit Store to handlers_ array");

    let has_single_store = program.flow.iter().any(|f| match f {
        FlowConstraint::Store { dst, .. } => *dst == gep_single,
        _ => false,
    });
    assert!(has_single_store, "must emit Store to single_cb_");
}
