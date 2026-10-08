//! #167: class templates inheriting from a computed `decltype(...)` base.
//! The parse input omits only the complete computed base entries (and the
//! separators they owned), keeping concrete siblings, the class body and every
//! line/column position; computed-base inheritance stays explicitly
//! unresolved (docs/ANALYSIS.md, "C++ parse-input normalization").

use std::fs;
use std::path::Path;
use trace_ir::Program;
use trace_parse::{build_program_with_jobs, has_parse_errors, parse_source_with_lang, SourceLang};
use trace_preproc::PreprocessOptions;

const ISSUE: &str = "template<class T> T make();\n\
\n\
template<class T>\n\
struct Derived : decltype(make<T>()) {\n\
    void member();\n\
};\n\
\n\
struct Base {};\n\
using Example = Derived<Base>;\n\
int after_decltype();\n\
int main() { return 0; }\n";

fn project(main_cpp: &str) -> (tempfile::TempDir, Program) {
    let scratch = tempfile::tempdir().unwrap();
    fs::write(scratch.path().join("main.cpp"), main_cpp).unwrap();
    let program = build_program_with_jobs(scratch.path(), &PreprocessOptions::new(), 1).unwrap();
    (scratch, program)
}

fn with_dependency(api_hpp: &str, main_cpp: &str) -> (tempfile::TempDir, Program) {
    let scratch = tempfile::tempdir().unwrap();
    let dep = scratch.path().join("dep");
    fs::create_dir(&dep).unwrap();
    fs::write(dep.join("api.hpp"), api_hpp).unwrap();
    fs::write(scratch.path().join("main.cpp"), main_cpp).unwrap();
    let program =
        build_program_with_jobs(scratch.path(), &PreprocessOptions::new().with_dep(&dep), 1)
            .unwrap();
    (scratch, program)
}

fn parse_diagnostics(program: &Program) -> Vec<String> {
    program
        .diagnostics
        .iter()
        .filter(|d| d.stage == "parse")
        .map(|d| d.message.clone())
        .collect()
}

fn functions_named<'a>(program: &'a Program, name: &str) -> Vec<&'a trace_ir::Function> {
    program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == name)
        .collect()
}

fn only_function<'a>(program: &'a Program, name: &str) -> &'a trace_ir::Function {
    let found = functions_named(program, name);
    assert_eq!(
        found.len(),
        1,
        "{name}: {:?}",
        program
            .symbols
            .functions
            .iter()
            .map(|f| &f.name)
            .collect::<Vec<_>>()
    );
    found[0]
}

fn normalized(source: &str) -> String {
    parse_source_with_lang(source, SourceLang::Cpp)
        .unwrap()
        .source
        .to_string()
}

fn parses_cleanly(source: &str) -> bool {
    let parsed = parse_source_with_lang(source, SourceLang::Cpp).unwrap();
    !has_parse_errors(&parsed.tree)
}

fn assert_parses_cleanly(source: &str) {
    let parsed = parse_source_with_lang(source, SourceLang::Cpp).unwrap();
    assert!(
        !has_parse_errors(&parsed.tree),
        "unrecovered parse error\n{}\n{}",
        parsed.source,
        parsed.tree.root_node().to_sexp()
    );
}

#[test]
fn decltype_base_preserves_declarations() {
    assert_parses_cleanly(ISSUE);
    let (_scratch, program) = project(ISSUE);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    let make = only_function(&program, "make");
    assert!(!make.is_defined);
    assert_eq!((make.span.line, make.span.col), (1, 21));
    let member = only_function(&program, "Derived::member");
    assert!(!member.is_defined);
    assert_eq!(member.span.line, 5);
    let after = only_function(&program, "after_decltype");
    assert!(!after.is_defined);
    assert_eq!((after.span.line, after.span.col), (10, 5));
    assert!(only_function(&program, "main").is_defined);
    assert!(
        program.symbols.call_sites.is_empty(),
        "the operand is not a call: {:?}",
        program.symbols.call_sites
    );
    assert_eq!(program.bases_of("Derived"), Vec::<String>::new());
    assert!(
        program
            .inheritance()
            .iter()
            .all(|(_, base)| !base.contains("decltype")),
        "{:?}",
        program.inheritance()
    );
}

#[test]
fn decltype_base_preserves_concrete_siblings() {
    let source = "template<class T> T make();\n\
template<class... Ts> int pack();\n\
struct A {}; struct B {}; struct C {};\n\
template<class T> struct First : decltype(make<T>()), A, B { void m(); };\n\
template<class T> struct Middle : A, public decltype(make<T>()), B { void m(); };\n\
template<class T> struct Last : A, B, virtual decltype(make<T>()) { void m(); };\n\
template<class T> struct Consecutive : A, decltype(make<T>()), protected decltype(make<T>()), C { void m(); };\n\
template<class T> struct All : decltype(make<T>()), private decltype(make<T>()) { void m(); };\n\
template<class... Ts> struct Pack : A, decltype(pack<Ts>())... { void m(); };\n\
template<class... Ts> struct OnlyPack : decltype(pack<Ts>())... { void m(); };\n\
template<class T> struct Multi\n\
    : A,\n\
      decltype(\n\
          make<T>()),\n\
      B\n\
{ void m(); };\n\
int after();\n";
    assert_parses_cleanly(source);
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    let bases = |cls: &str| program.bases_of(cls);
    assert_eq!(bases("First"), ["A", "B"]);
    assert_eq!(bases("Middle"), ["A", "B"]);
    assert_eq!(bases("Last"), ["A", "B"]);
    assert_eq!(bases("Consecutive"), ["A", "C"]);
    assert_eq!(bases("All"), Vec::<String>::new());
    assert_eq!(bases("Pack"), ["A"]);
    assert_eq!(bases("OnlyPack"), Vec::<String>::new());
    assert_eq!(bases("Multi"), ["A", "B"]);
    for (derived, base) in program.inheritance() {
        assert!(
            !base.contains("decltype") && !base.contains("..."),
            "{derived} : {base}"
        );
    }
    for cls in [
        "First",
        "Middle",
        "Last",
        "Consecutive",
        "All",
        "Pack",
        "OnlyPack",
        "Multi",
    ] {
        only_function(&program, &format!("{cls}::m"));
    }
    assert_eq!(only_function(&program, "after").span.line, 17);
    assert!(program.symbols.call_sites.is_empty());

    let output = normalized(source);
    assert_eq!(output.len(), source.len());
    // Exactly one comma separates retained entries; omitted entries and
    // their separators are spaces; a list with nothing left loses its colon.
    assert_gap(&output, "struct First :", "A, B {", 0);
    assert_gap(&output, "struct Middle : A", "B {", 1);
    assert_gap(&output, "struct Last : A, B", "{ void", 0);
    assert_gap(&output, "struct All", "{ void", 0);
    assert_gap(&output, "struct Pack : A", "{ void", 0);
    assert_gap(&output, "struct OnlyPack", "{ void", 0);
    assert!(!output.contains(": ..."));
}

/// The text between `before` and `after` in `output` is blank except for
/// exactly `commas` commas.
fn assert_gap(output: &str, before: &str, after: &str, commas: usize) {
    let start = output
        .find(before)
        .unwrap_or_else(|| panic!("{before:?} in {output}"))
        + before.len();
    let end = start
        + output[start..]
            .find(after)
            .unwrap_or_else(|| panic!("{after:?} in {output}"));
    let gap = &output[start..end];
    assert_eq!(
        gap.matches(',').count(),
        commas,
        "{before}…{after}: {gap:?}"
    );
    assert!(
        gap.chars().all(|c| c == ' ' || c == ','),
        "{before}…{after}: {gap:?}"
    );
}

#[test]
fn decltype_base_handles_crlf() {
    let source = "template<class T> T make();\r\nstruct A {};\r\ntemplate<class T>\r\nstruct D\r\n    : decltype(\r\n        make<T>()),\r\n      A\r\n{\r\n    void m();\r\n};\r\nint after();\r\n";
    let output = normalized(source);
    assert_eq!(output.len(), source.len());
    let crlf = |s: &str| s.match_indices("\r\n").map(|(i, _)| i).collect::<Vec<_>>();
    assert_eq!(crlf(&output), crlf(source));
    assert_parses_cleanly(source);
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert_eq!(program.bases_of("D"), ["A"]);
    assert_eq!(only_function(&program, "D::m").span.line, 9);
    assert_eq!(only_function(&program, "after").span.line, 11);
}

#[test]
fn decltype_base_class_heads() {
    let source = "template<class T> T make();\n\
template<class T, class U> int check(int);\n\
template<class...> using void_t = void;\n\
struct A {};\n\
template<class T, class = void> struct has_foo : A {};\n\
template<class T> struct has_foo<T, void_t<decltype(check<T, int>(0))>> : decltype(check<T, int>(0)), A { void m(); };\n\
template<class T> struct Final final : decltype(make<T>()), A { void m(); };\n\
template<class T> struct alignas(8) Aligned : decltype(make<T>()), A { void m(); };\n\
template<class T> struct __attribute__((packed)) Packed : decltype(make<T>()), A { void m(); };\n\
template<class T> struct [[deprecated]] Attributed : A, decltype(make<T>()) { void m(); };\n\
struct Outer { struct Nested; template<class T> struct Tmpl; };\n\
struct Outer::Nested : decltype(make<int>()), A { void m(); };\n\
template<class T> struct Outer::Tmpl<T> : A, decltype(make<T>()) { void m(); };\n\
template<class T> class Klass : public decltype(make<T>()), private A { public: void m(); };\n\
int after();\n";
    assert_parses_cleanly(source);
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    for cls in [
        "has_foo",
        "Final",
        "Aligned",
        "Packed",
        "Attributed",
        "Outer::Nested",
        "Outer::Tmpl",
        "Klass",
    ] {
        assert_eq!(program.bases_of(cls), ["A"], "{cls}");
        only_function(&program, &format!("{cls}::m"));
    }
    let output = normalized(source);
    assert!(
        output.contains("has_foo<T, void_t<decltype(check<T, int>(0))>> :"),
        "template arguments intact: {output}"
    );
    assert!(output.contains("struct Final final :"));
    assert!(output.contains("struct alignas(8) Aligned :"));
    assert!(output.contains("struct __attribute__((packed)) Packed :"));
    assert_eq!(only_function(&program, "after").span.line, 15);
    assert!(program.symbols.call_sites.is_empty());
}

#[test]
fn decltype_base_dependency_ownership() {
    let api = "template<class T> T make();\n\
struct A { void a(); };\n\
template<class T>\n\
struct Derived : decltype(make<T>()), A {\n\
    void member() { helper(); }\n\
    void helper();\n\
};\n\
struct Control {\n\
    void control() { helper(); }\n\
    void helper();\n\
};\n\
int dep_after();\n";
    let main_cpp = "#include \"dep/api.hpp\"\n\
template<class T> struct Mine : decltype(make<T>()), A { void mine() { make<T>(); } };\n\
int project_defined() { return 0; }\n\
int main() { return project_defined(); }\n";
    let (_scratch, program) = with_dependency(api, main_cpp);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert_parses_cleanly(api);
    let member = only_function(&program, "Derived::member");
    assert!(program.symbols.file_is_dep(member.span.file));
    assert!(
        !member.is_defined,
        "dependency bodies merge as declarations"
    );
    assert!(member.locals.is_empty());
    assert_eq!(
        (member.span.line, member.span.col),
        (5, only_function(&program, "Control::control").span.col)
    );
    assert_eq!(only_function(&program, "dep_after").span.line, 12);
    assert_eq!(program.bases_of("Derived"), ["A"]);
    assert_eq!(program.bases_of("Mine"), ["A"]);
    let mine = only_function(&program, "Mine::mine");
    assert!(!program.symbols.file_is_dep(mine.span.file));
    assert!(mine.is_defined);
    assert_eq!(mine.span.line, 2);
    let defined = only_function(&program, "project_defined");
    assert!(defined.is_defined);
    assert_eq!(defined.span.line, 3);
    let project_callers: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .map(|cs| {
            &program
                .symbols
                .functions
                .iter()
                .find(|f| f.id == cs.caller)
                .unwrap()
                .name
        })
        .collect();
    assert!(
        project_callers
            .iter()
            .all(|n| *n == "main" || *n == "Mine::mine"),
        "dependency bodies contribute no calls: {project_callers:?}"
    );
    assert!(project_callers.iter().any(|n| *n == "main"));
}

#[test]
fn decltype_base_does_not_hide_invalid_syntax() {
    let invalid = [
        // Unclosed and balanced-but-invalid operands.
        "template<class T> T make();\ntemplate<class T> struct D : decltype(make<T>() { void m(); };\n",
        "template<class T> T make();\ntemplate<class T> struct D : decltype(make<T>() +) { void m(); };\n",
        "template<class T> struct D : decltype() { void m(); };\n",
        // Empty entries, double commas, trailing separators.
        "struct A {};\ntemplate<class T> struct D : , A { void m(); };\n",
        "struct A {};\ntemplate<class T> struct D : A,, decltype(T()) { void m(); };\n",
        "struct A {};\ntemplate<class T> struct D : decltype(T()), { void m(); };\n",
        // Unknown modifier before the computed base: not a recognized entry.
        "template<class T> struct D : publik decltype(T()) { void m(); };\n",
        // Uncertain class head: no name.
        "template<class T> struct : decltype(T()) { void m(); } x;\n",
    ];
    for source in invalid {
        assert_eq!(normalized(source), source, "must stay unchanged");
        assert!(!parses_cleanly(source), "must stay diagnosed: {source}");
    }
}

#[test]
fn decltype_outside_base_lists_is_unchanged() {
    let unchanged = [
        // Template parameter `class T`, defaults, ternaries, initializer lists.
        "template<class T = decltype(0)> struct D { T t; };\n",
        // Non-base decltype in members and aliases.
        "template<class T> struct D { decltype(T()) value; using type = decltype(value); };\n",
        // Concrete base whose template arguments hold a decltype.
        "template<class T> T make();\ntemplate<class, class> struct Base {};\ntemplate<class T> struct D : Base<int, decltype(make<T>())> { void m(); };\n",
        // Nested `>>` and multi-argument operands inside template arguments.
        "template<class T, class U> int make();\ntemplate<class> struct W {};\ntemplate<class T> struct D : W<W<decltype(make<T, int>())>> { void m(); };\n",
        // Qualified computed base spelling parses already; not a complete entry.
        "template<class T> struct H { using base = T; };\ntemplate<class T> H<T> f();\nstruct A {};\ntemplate<class T> struct D : decltype(f<T>())::base, A { void m(); };\n",
        // Comparison/ternary operands inside a concrete template argument.
        "template<bool> struct Cond {};\ntemplate<int N> struct D : Cond<(N > 1 ? true : false)> { void m(); };\n",
        // Literals and comments.
        "const char *s = \"struct D : decltype(x) {\";\nconst char *r = R\"x(struct D : decltype(x) {)x\";\n// struct D : decltype(x) {\n/* struct D : decltype(x) { */\n",
    ];
    for source in unchanged {
        assert_eq!(normalized(source), source);
        assert_parses_cleanly(source);
    }
    // A ternary's `: decltype(a){}` and a constructor initializer's
    // `: B(decltype(0){})` are no base lists; the grammar rejects the braced
    // temporaries regardless, so only the bytes are checked.
    for source in [
        "int f(int a, int b) { return a ? b : decltype(a){}; }\n",
        "struct B { B(int); };\nstruct D : B { D() : B(decltype(0){}) {} };\n",
    ] {
        assert_eq!(normalized(source), source);
    }
    // Scoped enums keep a `decltype` underlying type byte-identical, with the
    // pre-existing grammar diagnostic; they must not become base lists.
    let enums = [
        "int x;\nenum class E : decltype(x) { a };\n",
        "int x;\nenum struct E : decltype(x) { a };\n",
        "int x;\nenum /* scoped */ class E : decltype(x) { a };\n",
        "enum class F : int { a };\nenum struct G : unsigned { b };\n",
    ];
    for source in enums {
        assert_eq!(normalized(source), source, "{source}");
    }
    assert_parses_cleanly(enums[3]);
}

#[test]
fn decltype_base_macro_and_comment_boundaries() {
    let source = "template<class T> T make();\n\
struct A {};\n\
#define COMPUTED(T) decltype(make<T>())\n\
struct B {};\n\
template<class T> struct M : A, COMPUTED(T) /* computed */, B { void m(); };\n\
template<class T> struct N : /* lead */ decltype(/* in */ make<T>() /* out */) /* trail */ { void m(); };\n\
int after();\n";
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert_eq!(program.bases_of("M"), ["A", "B"]);
    assert_eq!(program.bases_of("N"), Vec::<String>::new());
    assert_eq!(only_function(&program, "after").span.line, 7);
    only_function(&program, "M::m");
    only_function(&program, "N::m");
}

#[test]
fn repeated_computed_bases_parse_on_one_thread() {
    // Each candidate validates on the shared thread-local parser; the borrow
    // must be released before the unit itself is parsed, every time.
    let mut source = String::from("template<class T> T make();\nstruct A {};\n");
    for i in 0..20 {
        source.push_str(&format!(
            "template<class T> struct D{i} : decltype(make<T>()), A, decltype(make<T>()) {{ void m(); }};\n"
        ));
    }
    for _ in 0..3 {
        assert_parses_cleanly(&source);
    }
    let (_scratch, program) = project(&source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    for i in 0..20 {
        assert_eq!(program.bases_of(&format!("D{i}")), ["A"]);
    }
}

#[test]
fn fixture_tree_indexes_project_and_dependency() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/cpp_decltype_base");
    let dep = root.join("dep");
    let program =
        build_program_with_jobs(&root, &PreprocessOptions::new().with_dep(&dep), 1).unwrap();
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert!(only_function(&program, "main").is_defined);
    assert!(!program
        .symbols
        .file_is_dep(only_function(&program, "main").span.file));
    assert!(program
        .symbols
        .file_is_dep(only_function(&program, "after_decltype").span.file));
    assert_eq!(program.bases_of("Derived"), ["Base"]);
}

/// Globally qualified concrete siblings and template-id heads holding a
/// character literal do not stop the computed entry from being recognized.
#[test]
fn decltype_base_with_global_qualification_and_literal_arguments() {
    let source = "template<class T> T make();\n\
struct Base {};\n\
template<char> struct S;\n\
template<class T> struct D1 : decltype(make<T>()), ::Base { void m(); };\n\
template<class T> struct D2 : ::Base, decltype(make<T>()) { void m(); };\n\
template<> struct S<'>'> : decltype(make<int>()), Base { void m(); };\n\
template<> struct S<'\\''> : Base, decltype(make<int>()) { void m(); };\n\
int after();\n";
    assert_parses_cleanly(source);
    let output = normalized(source);
    assert_gap(&output, "struct D1 :", "::Base {", 0);
    assert_gap(&output, "struct D2 : ::Base", "{ void", 0);
    assert_gap(&output, "struct S<'>'> :", "Base {", 0);
    assert_gap(&output, "struct S<'\\''> : Base", "{ void", 0);
    assert!(!output.contains("decltype(make<int>())"), "{output}");

    // Through the pipeline, minus `D2`: the preprocessor re-spaces a base
    // list that opens with a global qualification (`: ::Base`) as `:::`,
    // which no grammar reads; that gap predates this change and is not a
    // base-list concern.
    let source = source.replace(
        "template<class T> struct D2 : ::Base, decltype(make<T>()) { void m(); };\n",
        "",
    );
    let (_scratch, program) = project(&source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert_eq!(
        program.bases_of("D1").len(),
        1,
        "{:?}",
        program.bases_of("D1")
    );
    assert!(program.bases_of("D1")[0].ends_with("Base"));
    assert_eq!(program.bases_of("S"), ["Base"]);
    for cls in ["D1", "S"] {
        only_function(&program, &format!("{cls}::m"));
    }
    assert_eq!(only_function(&program, "after").span.line, 7);
}

/// Attributes between `enum` and `class`/`struct` still make it a scoped
/// enum, whose `decltype` underlying type is never a base list.
#[test]
fn attributed_scoped_enum_is_unchanged() {
    for source in [
        "int x;\nenum [[nodiscard]] class E : decltype(x) { a };\n",
        "int x;\nenum [[deprecated(\"old\")]] struct E : decltype(x) { a };\n",
        "int x;\nenum /* c */ [[nodiscard]] /* d */ class E : decltype(x) { a };\n",
    ] {
        assert_eq!(normalized(source), source, "{source}");
    }
}
