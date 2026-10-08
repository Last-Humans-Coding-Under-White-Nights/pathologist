use trace_parse::parse_c_source;

#[test]
fn dump_init_body_kinds() {
    let src = r#"
int global_x;
void init(int **pp) { *pp = &global_x; }
void caller(void) { int *p; init(&p); }
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
}
