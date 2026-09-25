use trace_parse::parse_c_source;

#[test]
fn dump_net_device_impl_op_field_decl() {
    let src = r#"
struct NetDeviceImplOp {
    int32_t (*setIpAddr)(struct NetDeviceImpl *netDevice, const IpV4Addr *ipAddr);
    int32_t (*init)(struct NetDeviceImpl *netDevice);
};
"#;
    let parsed = parse_c_source(src).unwrap();
    fn walk(node: tree_sitter::Node, depth: usize) {
        if depth <= 8 {
            println!("{}{}", "  ".repeat(depth), node.kind());
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            walk(ch, depth + 1);
        }
    }
    walk(parsed.tree.root_node(), 0);
    let body = parsed
        .tree
        .root_node()
        .descendant_for_byte_range(0, parsed.source.len())
        .unwrap();
    fn find_field(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
        if node.kind() == "field_declaration" {
            return Some(node);
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            if let Some(n) = find_field(ch) {
                return Some(n);
            }
        }
        None
    }
    let fd = find_field(body).expect("field_declaration");
    let struct_node = parsed.tree.root_node().named_child(0).unwrap();
    println!(
        "body field: {:?}",
        struct_node.child_by_field_name("body").map(|n| n.kind())
    );
    println!(
        "field_declaration_list: {:?}",
        struct_node
            .child_by_field_name("field_declaration_list")
            .map(|n| n.kind())
    );
    println!(
        "declarator field: {:?}",
        fd.child_by_field_name("declarator").map(|n| n.kind())
    );
    println!(
        "type field: {:?}",
        fd.child_by_field_name("type").map(|n| n.kind())
    );
}

#[test]
fn unnamed_bitfields_do_not_produce_parse_errors() {
    // Standard C and C++ permit unnamed bitfields (e.g. for alignment or padding).
    // Upstream tree-sitter grammars insert a `(MISSING field_identifier)` node,
    // which must be recognized as benign and not flagged as a parse error.
    for (src, lang) in [
        ("struct S { unsigned : 0; };", trace_parse::SourceLang::C),
        (
            "struct S { const volatile signed : 0; };",
            trace_parse::SourceLang::C,
        ),
        (
            "struct S { int a : 1, : 0, b : 2; };",
            trace_parse::SourceLang::C,
        ),
        ("union U { unsigned : 0; };", trace_parse::SourceLang::C),
        ("struct S { unsigned : 0; };", trace_parse::SourceLang::Cpp),
        ("class C { unsigned : 0; };", trace_parse::SourceLang::Cpp),
        ("struct S { int : 4; };", trace_parse::SourceLang::C),
    ] {
        let parsed = trace_parse::parse_source_with_lang(src, lang).unwrap();
        assert!(
            !trace_parse::has_parse_errors(&parsed.tree),
            "unnamed bitfield should not be flagged as parse error in {src} ({lang:?})"
        );
    }
}

#[test]
fn genuine_parse_errors_are_still_detected() {
    for (src, lang) in [
        ("struct S { unsigned : ; };", trace_parse::SourceLang::C),
        ("struct S { unsigned x : ; };", trace_parse::SourceLang::C),
        ("struct S { int x }", trace_parse::SourceLang::C),
        ("int foo(;", trace_parse::SourceLang::C),
    ] {
        let parsed = trace_parse::parse_source_with_lang(src, lang).unwrap();
        assert!(
            trace_parse::has_parse_errors(&parsed.tree),
            "malformed syntax should be flagged as parse error in {src} ({lang:?})"
        );
    }
}

#[test]
fn unnamed_bitfield_struct_lowers_named_fields_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let src = r#"
struct S0 {
    volatile signed f0 : 7;
    const volatile signed : 0;
    volatile signed f1 : 2;
    signed f2 : 6;
    volatile signed f3 : 29;
    unsigned : 0;
};
void test_fn(struct S0 *s) {
    (void)s->f0;
}
"#;
    std::fs::write(dir.path().join("test.c"), src).unwrap();
    let program =
        trace_parse::build_program(dir.path(), &trace_preproc::PreprocessOptions::new()).unwrap();
    assert!(
        !program
            .diagnostics
            .iter()
            .any(|d| d.stage == "parse" && d.message.starts_with("parse errors in")),
        "program should have no parse error diagnostics: {:?}",
        program.diagnostics
    );
    let s0_type = program
        .types
        .all()
        .iter()
        .find(
            |t| matches!(t.desc.as_ref(), trace_ir::TypeDesc::Struct { name, .. } if name == "S0"),
        )
        .expect("S0 struct type");
    let field_names: Vec<&str> = s0_type
        .layout
        .fields
        .values()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(field_names, vec!["f0", "f1", "f2", "f3"]);
}
