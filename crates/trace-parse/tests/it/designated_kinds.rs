use trace_parse::parse_c_source;

#[test]
fn dump_designated_init_kinds() {
    let src = r#"
static struct Ops { void (*handler)(int *); } g_ops = { .handler = target };
static void target(int *p) { (void)p; }
"#;
    let parsed = parse_c_source(src).unwrap();
    let root = parsed.tree.root_node();
    fn find_init(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
        if node.kind() == "init_declarator" {
            return Some(node);
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            if let Some(n) = find_init(ch) {
                return Some(n);
            }
        }
        None
    }
    let init = find_init(root).expect("init_declarator");
    assert!(init.child_by_field_name("value").is_some(), "value field");
    assert!(
        init.child_by_field_name("declarator").is_some(),
        "declarator field"
    );
    fn walk(node: tree_sitter::Node, depth: usize) {
        if depth <= 15 {
            println!("{}{}", "  ".repeat(depth), node.kind());
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            walk(ch, depth + 1);
        }
    }
    walk(parsed.tree.root_node(), 0);
}
