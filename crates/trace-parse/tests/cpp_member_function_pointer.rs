//! #170: parenthesized member-function-pointer declarators
//! (`R (C::*)(Args...) const`) in project code and declaration-only
//! dependencies. The parse input blanks the owner (and, for named typedefs,
//! the trailing function qualifiers the ordinary-pointer approximation cannot
//! carry) in place; original bytes outside those ranges and all line/column
//! positions are unchanged (docs/ANALYSIS.md, "C++ parse-input
//! normalization").

use std::fs;
use std::path::Path;
use trace_ir::Program;
use trace_ir::TypeDesc;
use trace_parse::{build_program_with_jobs, has_parse_errors, parse_source_with_lang, SourceLang};
use trace_preproc::PreprocessOptions;

const ISSUE_USING_CONST: &str = "template<class R, class C, class... Args>\n\
struct Holder {\n\
    using Member = R (C::*)(Args...) const;\n\
};\n\
\n\
int after_member_pointer();\n\
int main() { return 0; }\n";

const ISSUE_USING_PLAIN: &str = "template<class R, class C, class... Args>\n\
struct Holder {\n\
    using Member = R (C::*)(Args...);\n\
};\n\
\n\
int after_member_pointer();\n\
int main() { return 0; }\n";

const ISSUE_TYPEDEF_CONST: &str = "template<class R, class C, class... Args>\n\
struct Holder {\n\
    typedef R (C::*Member)(Args...) const;\n\
};\n\
\n\
int after_member_pointer();\n\
int main() { return 0; }\n";

const ISSUE_TYPEDEF_PLAIN: &str = "template<class R, class C, class... Args>\n\
struct Holder {\n\
    typedef R (C::*Member)(Args...);\n\
};\n\
\n\
int after_member_pointer();\n\
int main() { return 0; }\n";

/// Index `main_cpp` as the only translation unit of a scratch tree.
fn project(main_cpp: &str) -> (tempfile::TempDir, Program) {
    let scratch = tempfile::tempdir().unwrap();
    fs::write(scratch.path().join("main.cpp"), main_cpp).unwrap();
    let program = build_program_with_jobs(scratch.path(), &PreprocessOptions::new(), 1).unwrap();
    (scratch, program)
}

/// Index a project TU that includes `api_hpp` from a dependency root.
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

fn function<'a>(program: &'a Program, name: &str) -> &'a trace_ir::Function {
    program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| {
            panic!(
                "missing function {name}; have {:?}",
                program
                    .symbols
                    .functions
                    .iter()
                    .map(|f| &f.name)
                    .collect::<Vec<_>>()
            )
        })
}

fn assert_no_phantom_functions(program: &Program) {
    for phantom in ["R", "C", "Args", "Member", "Holder::Member"] {
        assert!(
            !program.symbols.functions.iter().any(|f| f.name == phantom),
            "phantom function {phantom}: {:?}",
            program
                .symbols
                .functions
                .iter()
                .map(|f| &f.name)
                .collect::<Vec<_>>()
        );
    }
}

/// The normalized parse input the C++ grammar sees for `source`.
fn normalized(source: &str) -> String {
    parse_source_with_lang(source, SourceLang::Cpp)
        .unwrap()
        .source
        .to_string()
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

fn assert_issue_example(main_cpp: &str) {
    let (_scratch, program) = project(main_cpp);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert_parses_cleanly(main_cpp);
    assert!(
        program.types.resolve_alias("Holder::Member").is_some(),
        "aliases: {:?}",
        program.types.all_aliases().keys().collect::<Vec<_>>()
    );
    assert_no_phantom_functions(&program);
    let after = function(&program, "after_member_pointer");
    assert!(!after.is_defined);
    assert_eq!((after.span.line, after.span.col), (6, 5));
    let main = function(&program, "main");
    assert!(main.is_defined);
    assert_eq!(main.span.line, 7);
    assert!(
        program.symbols.call_sites.is_empty(),
        "{:?}",
        program.symbols.call_sites
    );
}

#[test]
fn member_function_pointer_const_parses() {
    assert_issue_example(ISSUE_USING_CONST);
    assert_issue_example(ISSUE_TYPEDEF_CONST);
}

#[test]
fn member_function_pointer_unqualified_parses() {
    assert_issue_example(ISSUE_USING_PLAIN);
    assert_issue_example(ISSUE_TYPEDEF_PLAIN);
}

/// The grammar accepts a trailing `const` on an abstract function-pointer
/// using-alias but not on a named function-pointer typedef, so owner blanking
/// alone is not enough for `typedef R (C::*Member)(Args...) const;`.
#[test]
fn owner_blanking_alone_leaves_named_typedef_const_unparsed() {
    let owner_blanked = "template<class R, class C, class... Args>\n\
struct Holder {\n\
    typedef R (   *Member)(Args...) const;\n\
};\n";
    let parsed = parse_source_with_lang(owner_blanked, SourceLang::Cpp).unwrap();
    assert_eq!(
        parsed.source.as_ref(),
        owner_blanked,
        "no rewrite applies to plain pointers"
    );
    assert!(
        has_parse_errors(&parsed.tree),
        "{}",
        parsed.tree.root_node().to_sexp()
    );
}

fn alias_desc(program: &Program, alias: &str) -> TypeDesc {
    program
        .types
        .resolve_alias(alias)
        .unwrap_or_else(|| {
            panic!(
                "missing alias {alias}; have {:?}",
                program.types.all_aliases().keys().collect::<Vec<_>>()
            )
        })
        .clone()
}

fn assert_coarse_function_pointer(desc: &TypeDesc, context: &str) {
    let inner = match desc {
        TypeDesc::Ptr(inner) => inner.as_ref(),
        TypeDesc::FnPtr { .. } => desc,
        other => panic!("{context}: not a pointer shape: {other:?}"),
    };
    match inner {
        TypeDesc::FnPtr { ret, .. } => {
            assert!(
                !matches!(ret.as_ref(), TypeDesc::FnPtr { .. }),
                "{context}: nested function type artifact {desc:?}"
            );
        }
        other => panic!("{context}: not a function pointer: {other:?}"),
    }
}

#[test]
fn member_function_pointer_shape_matches_approximation() {
    let (_scratch, program) = project(
        "struct C { int m(double) const; int n(double); };\n\
         using Plain = int (*)(double);\n\
         using ConstMember = int (C::*)(double) const;\n\
         using Member = int (C::*)(double);\n\
         typedef int (C::*TypedefConstMember)(double) const;\n\
         typedef int (C::*TypedefMember)(double);\n\
         int after_shapes();\n",
    );
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    let plain = alias_desc(&program, "Plain");
    assert_coarse_function_pointer(&plain, "Plain");
    for alias in [
        "ConstMember",
        "Member",
        "TypedefConstMember",
        "TypedefMember",
    ] {
        let desc = alias_desc(&program, alias);
        assert_coarse_function_pointer(&desc, alias);
        assert_eq!(desc, plain, "{alias}");
    }
    assert_eq!(function(&program, "after_shapes").span.line, 7);
}

/// The shape, not the declaration context, selects the rewrite.
#[test]
fn member_function_pointer_contexts() {
    let source = "struct C { int m(double) const; };\n\
template<class F> struct traits;\n\
template<class R, class C2, class... Args>\n\
struct traits<R (C2::*)(Args...) const> { using result = R; };\n\
template<class R, class C2, class... Args>\n\
struct traits<R (C2::*)(Args...)> { using result = R; };\n\
template<class R, class C2, class... Args>\n\
int mem_fn(R (C2::*pm)(Args...) const);\n\
struct Fields {\n\
    int (C::*field)(double) const;\n\
    int (C::*plain_field)(double);\n\
    void useful();\n\
};\n\
struct Control {\n\
    void control();\n\
};\n\
int (C::*variable)(double) const = &C::m;\n\
typedef void (*callback)(int (C::*inner)(double) const, int);\n\
int after_contexts();\n";
    assert_parses_cleanly(source);
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert!(
        program.types.resolve_alias("traits::result").is_some(),
        "{:?}",
        program.types.all_aliases().keys().collect::<Vec<_>>()
    );
    assert!(program.types.resolve_alias("callback").is_some());
    let useful = function(&program, "Fields::useful");
    let control = function(&program, "Control::control");
    assert_eq!((useful.span.line, useful.span.col), (12, control.span.col));
    assert_eq!(control.span.line, 15);
    assert!(!useful.is_defined);
    assert_eq!(function(&program, "mem_fn").span.line, 8);
    assert_eq!(function(&program, "after_contexts").span.line, 19);
    assert!(
        program
            .symbols
            .variables
            .iter()
            .any(|v| v.name == "variable"),
        "{:?}",
        program
            .symbols
            .variables
            .iter()
            .map(|v| &v.name)
            .collect::<Vec<_>>()
    );
    for phantom in ["R", "C2", "Args", "pm", "field", "inner"] {
        assert!(
            !program.symbols.functions.iter().any(|f| f.name == phantom),
            "{phantom}"
        );
    }
}

/// Only the owner (and, for named typedefs, the incompatible suffix) changes;
/// every other byte of the parse input is the original.
fn assert_only_ranges_blanked(original: &str, expected_blanked: &[&str]) -> String {
    assert_only_ranges_blanked_at(original, expected_blanked, |hay, needle| hay.find(needle))
}

/// `find` selects which occurrence of each needle is the blanked one.
fn assert_only_ranges_blanked_at(
    original: &str,
    expected_blanked: &[&str],
    find: fn(&str, &str) -> Option<usize>,
) -> String {
    let output = normalized(original);
    assert_eq!(output.len(), original.len());
    let mut expected = original.to_string();
    for needle in expected_blanked {
        let at =
            find(&expected, needle).unwrap_or_else(|| panic!("{needle:?} not in {original:?}"));
        let blank: String = needle
            .chars()
            .map(|c| if c == '\n' || c == '\r' { c } else { ' ' })
            .collect();
        expected.replace_range(at..at + needle.len(), &blank);
    }
    assert_eq!(output, expected);
    output
}

#[test]
fn using_alias_keeps_suffixes_and_blanks_owner_only() {
    for suffix in [
        "",
        " const",
        " volatile",
        " const volatile",
        " &",
        " &&",
        " noexcept",
        " noexcept(false)",
        " const & noexcept",
    ] {
        let source =
            format!("struct C {{}};\nusing M = int (C::*)(double){suffix};\nint after();\n");
        let output = assert_only_ranges_blanked(&source, &["C::"]);
        assert_parses_cleanly(&output);
        let qualified = format!("namespace ns {{ struct C {{}}; }}\nusing M = int (ns::C::*)(double){suffix};\nint after();\n");
        let output = assert_only_ranges_blanked(&qualified, &["ns::C::"]);
        assert_parses_cleanly(&output);
    }
}

#[test]
fn named_typedef_blanks_owner_and_incompatible_suffix() {
    for suffix in [
        " const",
        " volatile",
        " const volatile",
        " &",
        " &&",
        " noexcept",
        " noexcept(false)",
        " const & noexcept",
    ] {
        let source =
            format!("struct C {{}};\ntypedef int (C::*M)(double){suffix};\nint after();\n");
        let output = assert_only_ranges_blanked(&source, &["C::", suffix]);
        assert_parses_cleanly(&output);
    }
    let plain = "struct C {};\ntypedef int (C::*M)(double);\nint after();\n";
    assert_parses_cleanly(&assert_only_ranges_blanked(plain, &["C::"]));
}

#[test]
fn member_function_pointer_tolerates_comments_and_newlines() {
    let source = "struct C {};\n\
using M = int (/* owner */ C /* sep */ ::\n  * /* star */)(double) const;\n\
typedef int (C\r\n::*N)(double)\r\n  const /* trailing */;\n\
int after();\n";
    let output = normalized(source);
    assert_eq!(output.len(), source.len());
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let out_lines: Vec<&str> = output.split_inclusive('\n').collect();
    assert_eq!(lines.len(), out_lines.len());
    for (a, b) in lines.iter().zip(&out_lines) {
        assert_eq!(a.len(), b.len());
        assert_eq!(a.ends_with("\r\n"), b.ends_with("\r\n"));
    }
    assert!(!output.contains("C /* sep */ ::"));
    assert!(!out_lines[5].contains("const"), "{:?}", out_lines[5]);
    assert!(out_lines[5].contains("/* trailing */"));
    assert_parses_cleanly(&output);
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert!(program.types.resolve_alias("M").is_some());
    assert!(program.types.resolve_alias("N").is_some());
    assert_eq!(function(&program, "after").span.line, 7);
}

#[test]
fn unrelated_syntax_is_unchanged() {
    let sources = [
        // Ordinary function pointers and qualified names.
        "struct C { static int s(double); };\nusing F = int (*)(double);\nint (*fp)(double) = &C::s;\nint x = C::s(1.0);\n",
        // Project data-member pointers keep their owner (the project rule
        // only covers the parenthesized function-pointer shape).
        "struct C { int v; };\nint C::* dm = &C::v;\nusing DM = int C::*;\n",
        // Literals and comments.
        "const char *s = \"int (C::*)(double) const\";\nconst char *r = R\"x(int (C::*)(double) const)x\";\n// typedef int (C::*M)(double) const;\n/* using M = int (C::*)(double) const; */\n",
        // Template-qualified owners are outside the supported shape.
        "template<class T> struct W { struct C {}; };\nusing M = int (W<int>::C::*)(double) const;\n",
        // Malformed declarations stay malformed.
        "struct C {};\nusing M = int (C::*)(double const;\ntypedef int (C::*N)(double) noexcept(;\n",
    ];
    for source in sources {
        assert_eq!(normalized(source), source);
    }
    // Expression contexts spelling `.*` / `->*` are untouched; only the
    // parameter's owner is approximated.
    let call = "struct C { int m(double); };\nint call(C &c, int (C::*pm)(double)) { return (c.*pm)(1.0) + ((&c)->*pm)(2.0); }\n";
    let output = assert_only_ranges_blanked(call, &["C::"]);
    assert!(output.contains("(c.*pm)(1.0) + ((&c)->*pm)(2.0)"));
}

/// Parameter and pointer-level qualifiers are not function qualifiers.
#[test]
fn named_typedef_keeps_parameter_and_pointer_qualifiers() {
    let source = "struct C {};\ntypedef const int *(C::* const M)(const double &, char *const) const;\nint after();\n";
    let output =
        assert_only_ranges_blanked_at(source, &["C::", " const"], |hay, needle| hay.rfind(needle));
    assert!(output.contains("(const double &, char *const)"));
    assert!(output.contains("* const M"));
    assert_parses_cleanly(&output);
}

#[test]
fn malformed_suffix_is_not_degraded_to_owner_only_blanking() {
    // Owner-only blanking would hide the unterminated `noexcept(` behind a
    // different diagnostic; the whole declarator must stay as written.
    let source = "struct C {};\ntypedef int (C::*M)(double) noexcept(false;\nint after();\n";
    assert_eq!(normalized(source), source);
}

const DEP_API: &str = "template<class R, class C, class... Args>\n\
struct Holder {\n\
    using Member = R (C::*)(Args...) const;\n\
    typedef R (C::*Named)(Args...) const;\n\
    using Plain = R (C::*)(Args...);\n\
    typedef R (C::*NamedPlain)(Args...);\n\
    void useful() { helper(); }\n\
    void helper();\n\
};\n\
struct Control {\n\
    void control() { helper(); }\n\
    void helper();\n\
};\n\
template<class R, class C2, class... Args>\n\
struct traits<R (C2::*)(Args...) const> { using result = R; };\n\
int dep_declared();\n";

#[test]
fn dependency_member_function_pointers_keep_ownership_and_positions() {
    let main_cpp = "#include \"dep/api.hpp\"\n\
struct Mine { int m(double) const; };\n\
using MyMember = int (Mine::*)(double) const;\n\
int project_defined() { return 1; }\n\
int main() { return project_defined(); }\n";
    let (_scratch, program) = with_dependency(DEP_API, main_cpp);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    for alias in [
        "Holder::Member",
        "Holder::Named",
        "Holder::Plain",
        "Holder::NamedPlain",
        "MyMember",
    ] {
        assert!(
            program.types.resolve_alias(alias).is_some(),
            "{alias}: {:?}",
            program.types.all_aliases().keys().collect::<Vec<_>>()
        );
    }
    let useful = function(&program, "Holder::useful");
    assert!(program.symbols.file_is_dep(useful.span.file));
    assert!(
        !useful.is_defined,
        "dependency bodies merge as declarations"
    );
    assert_eq!(
        (useful.span.line, useful.span.col),
        (7, function(&program, "Control::control").span.col)
    );
    assert_eq!(function(&program, "dep_declared").span.line, 16);
    let defined = function(&program, "project_defined");
    assert!(defined.is_defined);
    assert!(!program.symbols.file_is_dep(defined.span.file));
    assert_eq!(defined.span.line, 4);
    assert!(
        program
            .symbols
            .call_sites
            .iter()
            .all(|cs| cs.caller == function(&program, "main").id),
        "only project bodies contribute calls: {:?}",
        program.symbols.call_sites
    );
    assert_eq!(program.symbols.call_sites.len(), 1);
    assert_no_phantom_functions(&program);
}

/// The dependency pass runs before the general pass; its legacy `C::*`
/// blanking must leave the parenthesized shape to the general pass so the
/// named-typedef suffix is still recognized.
#[test]
fn dependency_named_typedef_suffix_is_blanked() {
    let api = "struct C {};\ntypedef int (C::*M)(double) const;\nusing DM = int C::*;\nint dep_after();\n";
    let main_cpp = "#include \"dep/api.hpp\"\nint main() { return 0; }\n";
    let (_scratch, program) = with_dependency(api, main_cpp);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert!(program.types.resolve_alias("M").is_some());
    assert!(program.types.resolve_alias("DM").is_some());
    assert_eq!(function(&program, "dep_after").span.line, 4);
    assert!(program
        .symbols
        .file_is_dep(function(&program, "dep_after").span.file));
}

#[test]
fn fixture_tree_indexes_project_and_dependency() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/cpp_member_function_pointer");
    let dep = root.join("dep");
    let program =
        build_program_with_jobs(&root, &PreprocessOptions::new().with_dep(&dep), 1).unwrap();
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert!(program.types.resolve_alias("Holder::Member").is_some());
    assert!(function(&program, "main").is_defined);
    assert!(!program
        .symbols
        .file_is_dep(function(&program, "main").span.file));
    assert!(program
        .symbols
        .file_is_dep(function(&program, "after_member_pointer").span.file));
}

/// A class head with a base list is still scanned for member-pointer shapes,
/// whatever else the unit contains (`decltype` elsewhere turns the
/// computed-base recognizer on; it must not swallow the head).
#[test]
fn specialization_with_base_clause_is_rewritten_despite_unrelated_decltype() {
    let source = "template<class R, class C, class... A> struct traits_base {};\n\
template<class F> struct traits;\n\
template<class R, class C, class... A>\n\
struct traits<R (C::*)(A...) const &> : traits_base<R, const C, A...> { using result = R; };\n\
template<class T> auto g(T t) -> decltype(t);\n\
int after();\n";
    let output = assert_only_ranges_blanked(source, &["C::"]);
    assert!(
        output.contains("struct traits<R (   *)(A...) const &> : traits_base<R, const C, A...>"),
        "{output}"
    );
    assert_parses_cleanly(source);
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert!(program.types.resolve_alias("traits::result").is_some());
    assert_eq!(function(&program, "after").span.line, 6);
}

/// The typedef rule keys on the statement's first token; access labels,
/// plain labels, `__extension__` and attributes precede it in real classes.
#[test]
fn named_typedef_after_access_label_blanks_suffix() {
    // Every form is recognized and blanked identically.
    let all_forms = "class X {\n\
public:\n\
    typedef void (X::*Handler)() const;\n\
protected:\n\
    __extension__ typedef int (X::*Ext)(int) volatile;\n\
private:\n\
    [[maybe_unused]] typedef int (X::*Attr)() &;\n\
    void method() { lbl: typedef int (X::*Local)() const; }\n\
};\n\
int after();\n";
    assert_only_ranges_blanked(
        all_forms,
        &[
            "X::",
            " const",
            "X::",
            " volatile",
            "X::",
            " &",
            "X::",
            " const",
        ],
    );
    // The grammar rejects an attributed typedef member and a labeled typedef
    // statement on its own, so the end-to-end check uses the accepted forms.
    let source = "class X {\n\
public:\n\
    typedef void (X::*Handler)() const;\n\
protected:\n\
    __extension__ typedef int (X::*Ext)(int) volatile;\n\
    void method();\n\
};\n\
int after();\n";
    assert_parses_cleanly(&assert_only_ranges_blanked(
        source,
        &["X::", " const", "X::", " volatile"],
    ));
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    for alias in ["X::Handler", "X::Ext"] {
        assert!(
            program.types.resolve_alias(alias).is_some(),
            "{alias}: {:?}",
            program.types.all_aliases().keys().collect::<Vec<_>>()
        );
    }
    assert_eq!(function(&program, "after").span.line, 8);
}

/// A globally qualified owner (`::C::*`) is an owner like any other.
#[test]
fn globally_qualified_owner_is_blanked() {
    let source = "namespace ns { struct C { void m(int) const; }; }\n\
void (::ns::C::*cb)(int) const = &ns::C::m;\n\
typedef void (::ns::C::*F)(int) const;\n\
using U = void (::ns::C::*)(int) const;\n\
int after();\n";
    let output = normalized(source);
    assert_eq!(
        output,
        "namespace ns { struct C { void m(int) const; }; }\n\
void (         *cb)(int) const = &ns::C::m;\n\
typedef void (         *F)(int)      ;\n\
using U = void (         *)(int) const;\n\
int after();\n"
    );
    assert_parses_cleanly(&output);
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert!(
        program.symbols.variables.iter().any(|v| v.name == "cb"),
        "{:?}",
        program
            .symbols
            .variables
            .iter()
            .map(|v| &v.name)
            .collect::<Vec<_>>()
    );
    assert!(!program
        .symbols
        .variables
        .iter()
        .any(|v| v.name.contains("::*")));
    assert!(program.types.resolve_alias("F").is_some());
    assert!(program.types.resolve_alias("U").is_some());
    assert_eq!(function(&program, "after").span.line, 5);
}

/// The typedef statement is found past a ternary inside template arguments
/// (its `:` is not a label) and past a GNU attribute.
#[test]
fn named_typedef_after_template_ternary_and_gnu_attribute() {
    let source = "template<bool, class T, class F> struct cond { using type = T; };\n\
struct C {};\n\
template<bool B> struct S {\n\
    typedef typename cond<B ? true : false, int, long>::type (C::*F)() const;\n\
    void method();\n\
};\n\
int after();\n";
    let output = assert_only_ranges_blanked(source, &["C::", " const"]);
    assert_parses_cleanly(&output);
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert!(
        program.types.resolve_alias("S::F").is_some(),
        "{:?}",
        program.types.all_aliases().keys().collect::<Vec<_>>()
    );
    assert_eq!(function(&program, "after").span.line, 7);
    // The grammar rejects a GNU-attributed typedef on its own (even for a
    // plain pointer), so only the bytes are checked here.
    let attributed = "struct C {};\n__attribute__((deprecated)) typedef void (C::*G)() const;\n";
    assert_only_ranges_blanked(attributed, &["C::", " const"]);
}

#[test]
fn named_typedef_after_case_label_blanks_suffix() {
    for label in [
        "case 1:",
        "case 'a':",
        "case (1 + 2):",
        "case int{1}:",
        "case true ? 1 : 2:",
        "default:",
    ] {
        let source = format!(
            "struct C {{}};\nvoid f(int x) {{ switch (x) {{ {label}\n    typedef void (C::*F)() const;\n    break;\n}} }}\nint after();\n"
        );
        assert_only_ranges_blanked(&source, &["C::", " const"]);
        assert_parses_cleanly(&source);
        let (_scratch, program) = project(&source);
        assert_eq!(parse_diagnostics(&program), Vec::<String>::new(), "{label}");
        assert_eq!(function(&program, "after").span.line, 6);
    }
}

#[test]
fn literal_suffix_ending_in_r_does_not_hide_following_typedef() {
    let source = "constexpr int operator\"\"_R(const char*, unsigned long) { return 1; }\nint a = \"a\"_R\"b\";\nstruct C {}; typedef void (C::*F)() const;\n";
    // The pinned grammar rejects concatenation after a user-defined suffix;
    // normalization must still preserve it and handle the following typedef.
    assert_only_ranges_blanked(source, &["C::", " const"]);
}
