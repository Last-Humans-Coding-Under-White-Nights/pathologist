//! #169: GNU `__typeof__(...)` / `__typeof(...)` in type positions, and the
//! conservative descriptor for any `decltype` of an unresolved expression.
//! A simple type operand is unwrapped in place; an expression operand is
//! re-spelled as `decltype` with identical byte positions and lowers to
//! `TypeDesc::Unknown`, exactly like native `decltype` (docs/ANALYSIS.md,
//! "C++ parse-input normalization").

use std::fs;
use std::path::Path;
use trace_ir::{Program, TypeDesc};
use trace_parse::{build_program_with_jobs, has_parse_errors, parse_source_with_lang, SourceLang};
use trace_preproc::PreprocessOptions;

const ISSUE: &str = "template<class T, class U>\n\
struct sum_result {\n\
    typedef __typeof__(T() + U()) type;\n\
};\n\
\n\
int after_typeof();\n\
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

fn alias(program: &Program, name: &str) -> TypeDesc {
    program
        .types
        .resolve_alias(name)
        .unwrap_or_else(|| {
            panic!(
                "missing alias {name}; have {:?}",
                program.types.all_aliases().keys().collect::<Vec<_>>()
            )
        })
        .clone()
}

fn normalized(source: &str) -> String {
    parse_source_with_lang(source, SourceLang::Cpp)
        .unwrap()
        .source
        .to_string()
}

fn parses_cleanly(source: &str) -> bool {
    !has_parse_errors(
        &parse_source_with_lang(source, SourceLang::Cpp)
            .unwrap()
            .tree,
    )
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
fn gnu_typeof_expression_retains_alias() {
    assert_parses_cleanly(ISSUE);
    let (_scratch, program) = project(ISSUE);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert_eq!(alias(&program, "sum_result::type"), TypeDesc::Unknown);
    for wrong in ["sum_result::T()+", "sum_result::int", "T()+", "type"] {
        assert!(program.types.resolve_alias(wrong).is_none(), "{wrong}");
    }
    let names: Vec<&str> = program
        .symbols
        .functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert!(
        names.iter().all(|n| *n == "after_typeof" || *n == "main"),
        "{names:?}"
    );
    assert!(program.symbols.call_sites.is_empty());
    let after = function(&program, "after_typeof");
    assert!(!after.is_defined);
    assert_eq!((after.span.line, after.span.col), (6, 5));
    assert_eq!(function(&program, "main").span.line, 7);
    assert_eq!(
        normalized(ISSUE),
        ISSUE.replace("__typeof__(T() + U())", "decltype  (T() + U())")
    );
}

/// Each operand through `__typeof__`/`__typeof` and typedef/using must equal
/// the descriptor of a native alias spelled with the same operand.
#[test]
fn gnu_typeof_simple_type_operand_retains_shape() {
    let operands = [
        "int",
        "void",
        "void *",
        "char",
        "char *",
        "float",
        "double",
        "bool",
        "unsigned long long",
        "unsigned",
        "uint32_t",
        "size_t",
        "const int",
        "int const *",
        "const char * const *",
        "int **",
        "volatile int * volatile",
        "T *",
        "T &",
        "T &&",
        "const T * const",
        "const ns::Type *",
        "ns::Type &",
        "int /* c */ *",
        "  int\n  *  ",
    ];
    let mut source = String::from(
        "typedef unsigned int uint32_t;\ntypedef unsigned long size_t;\nnamespace ns { struct Type { int f; }; }\ntemplate<class T> struct S {\n",
    );
    for (i, operand) in operands.iter().enumerate() {
        source.push_str(&format!("    typedef {operand} ctl{i};\n"));
        source.push_str(&format!("    using uctl{i} = {operand};\n"));
        source.push_str(&format!("    typedef __typeof__({operand}) td{i};\n"));
        source.push_str(&format!("    using us{i} = __typeof__({operand});\n"));
        source.push_str(&format!("    typedef __typeof({operand}) sd{i};\n"));
    }
    source.push_str("    void member();\n};\nint after();\n");
    assert_parses_cleanly(&source);
    let (_scratch, program) = project(&source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    for (i, operand) in operands.iter().enumerate() {
        // typedef and using controls are compared with their own form: the
        // two alias paths already differ on references (`T &`) at baseline.
        let control = alias(&program, &format!("S::ctl{i}"));
        assert_ne!(
            control,
            TypeDesc::Unknown,
            "{operand}: control must resolve"
        );
        for prefix in ["td", "sd"] {
            assert_eq!(
                alias(&program, &format!("S::{prefix}{i}")),
                control,
                "{prefix}: {operand}"
            );
        }
        let using_control = alias(&program, &format!("S::uctl{i}"));
        assert_ne!(
            using_control,
            TypeDesc::Unknown,
            "{operand}: using control must resolve"
        );
        assert_eq!(
            alias(&program, &format!("S::us{i}")),
            using_control,
            "us: {operand}"
        );
    }
    assert_eq!(alias(&program, "S::td0"), TypeDesc::Int);
    assert_eq!(
        alias(&program, "S::td2"),
        TypeDesc::Ptr(Box::new(TypeDesc::Void))
    );
    assert_eq!(
        alias(&program, "S::td4"),
        TypeDesc::Ptr(Box::new(TypeDesc::Char))
    );
    assert!(
        matches!(alias(&program, "S::td21"), TypeDesc::Ptr(inner) if matches!(*inner, TypeDesc::Struct { ref name, .. } if name.ends_with("Type"))),
        "{:?}",
        alias(&program, "S::td21")
    );
    let member = function(&program, "S::member");
    let line_of =
        |needle: &str| source.lines().position(|l| l.contains(needle)).unwrap() as u32 + 1;
    assert_eq!(member.span.line, line_of("void member()"));
    assert_eq!(
        function(&program, "after").span.line,
        line_of("int after()")
    );
    let output = normalized(&source);
    assert_eq!(output.len(), source.len());
    assert!(!output.contains("__typeof"));
    assert!(
        output.contains("typedef            void *  td2;"),
        "{output}"
    );
    assert!(output.contains("typedef          void *  sd2;"), "{output}");
}

/// Unresolved expression operands lower to `Unknown` for native `decltype` and
/// both GNU spellings alike, so neither the substring fallback (`avoid_copy`
/// → Void, `charge` → Char) nor peeling into a qualified operand
/// (`Outer::Inner` → the alias it names) fabricates a type.
#[test]
fn decltype_expression_is_unresolved() {
    let operands = [
        "T() + U()",
        "(T() + U())",
        "T",
        "MyType",
        "avoid_copy()",
        "charge()",
        "ns::value",
        "Outer::Inner",
        "x * y",
        "x & y",
        "x == y ? x : y",
    ];
    let mut source = String::from(
        "struct MyType {};\nint avoid_copy();\nchar charge();\nnamespace ns { int value; }\nstruct Outer { typedef double Inner; };\nint x; int y;\n\
using Ordinary = Outer::Inner;\n\
template<class T, class U> struct S {\n",
    );
    for (i, operand) in operands.iter().enumerate() {
        source.push_str(&format!("    typedef decltype({operand}) nd{i};\n"));
        source.push_str(&format!("    using nu{i} = decltype({operand});\n"));
        source.push_str(&format!("    typedef __typeof__({operand}) gd{i};\n"));
        source.push_str(&format!("    using gu{i} = __typeof__({operand});\n"));
        source.push_str(&format!("    typedef __typeof({operand}) sd{i};\n"));
    }
    source.push_str("    void member();\n};\nint after();\n");
    assert_parses_cleanly(&source);
    let (_scratch, program) = project(&source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert_eq!(
        alias(&program, "Ordinary"),
        TypeDesc::Double,
        "ordinary named-type resolution"
    );
    for (i, operand) in operands.iter().enumerate() {
        for prefix in ["nd", "nu", "gd", "gu", "sd"] {
            assert_eq!(
                alias(&program, &format!("S::{prefix}{i}")),
                TypeDesc::Unknown,
                "{prefix}: {operand}"
            );
        }
    }
    assert!(
        program.symbols.call_sites.is_empty(),
        "operands are not evaluated: {:?}",
        program.symbols.call_sites
    );
    let output = normalized(&source);
    assert_eq!(output.len(), source.len());
    for operand in operands {
        assert!(
            output.contains(&format!("decltype  ({operand})")),
            "{operand}"
        );
        assert!(
            output.contains(&format!("typedef decltype({operand})")),
            "{operand}"
        );
    }
}

#[test]
fn gnu_typeof_keyword_padding_and_lines() {
    let source = "int x;\r\ntypedef __typeof__(x\r\n  + 1) a;\r\ntypedef __typeof(x) b;\r\ntypedef __typeof__(\r\n  int *\r\n) c;\r\nint after();\r\n";
    let output = normalized(source);
    assert_eq!(output.len(), source.len());
    let crlf = |s: &str| s.match_indices("\r\n").map(|(i, _)| i).collect::<Vec<_>>();
    assert_eq!(crlf(&output), crlf(source));
    assert!(
        output.contains("typedef decltype  (x\r\n  + 1) a;"),
        "{output:?}"
    );
    assert!(output.contains("typedef decltype(x) b;"), "{output:?}");
    assert!(
        output.contains("typedef            \r\n  int *\r\n  c;"),
        "{output:?}"
    );
    assert_parses_cleanly(source);
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert_eq!(alias(&program, "a"), TypeDesc::Unknown);
    assert_eq!(alias(&program, "b"), TypeDesc::Unknown);
    assert_eq!(alias(&program, "c"), TypeDesc::Ptr(Box::new(TypeDesc::Int)));
    assert_eq!(function(&program, "after").span.line, 8);
}

/// Bare `typeof` (6 bytes) cannot be re-spelled as `decltype` in place and is
/// left alone; it keeps whatever the grammar makes of it.
#[test]
fn bare_typeof_is_unchanged() {
    let source = "int x;\ntypedef typeof(x) a;\ntypedef typeof(int) b;\n";
    assert_eq!(normalized(source), source);
}

const DEP_API: &str = "#define DEP_TYPEOF(e) __typeof__(e)\n\
template<class T, class U>\n\
struct sum_result {\n\
    typedef __typeof__(T() + U()) type;\n\
    typedef __typeof__(int) itype;\n\
    typedef __typeof(T *) ptype;\n\
    typedef DEP_TYPEOF(T() * U()) mtype;\n\
    void useful() { helper(); }\n\
    void helper();\n\
};\n\
struct Control {\n\
    void control() { helper(); }\n\
    void helper();\n\
};\n\
int dep_after();\n";

#[test]
fn gnu_typeof_dependency_ownership_and_positions() {
    let main_cpp = "#include \"dep/api.hpp\"\n\
#define MY_TYPEOF(e) __typeof__(e)\n\
template<class T> struct Mine {\n\
    typedef MY_TYPEOF(T() + 1) type;\n\
    typedef DEP_TYPEOF(int *) ptype;\n\
    void mine() { }\n\
};\n\
int project_defined() { return 0; }\n\
int main() { return project_defined(); }\n";
    let (_scratch, program) = with_dependency(DEP_API, main_cpp);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert_eq!(alias(&program, "sum_result::type"), TypeDesc::Unknown);
    assert_eq!(alias(&program, "sum_result::itype"), TypeDesc::Int);
    assert_ne!(alias(&program, "sum_result::ptype"), TypeDesc::Unknown);
    assert!(matches!(
        alias(&program, "sum_result::ptype"),
        TypeDesc::Ptr(_)
    ));
    assert_eq!(alias(&program, "sum_result::mtype"), TypeDesc::Unknown);
    assert_eq!(alias(&program, "Mine::type"), TypeDesc::Unknown);
    assert_eq!(
        alias(&program, "Mine::ptype"),
        TypeDesc::Ptr(Box::new(TypeDesc::Int))
    );
    for wrong in ["sum_result::T()+", "sum_result::int", "Mine::T()+"] {
        assert!(program.types.resolve_alias(wrong).is_none(), "{wrong}");
    }
    let useful = function(&program, "sum_result::useful");
    assert!(program.symbols.file_is_dep(useful.span.file));
    assert!(!useful.is_defined);
    assert!(useful.locals.is_empty());
    assert_eq!(
        (useful.span.line, useful.span.col),
        (8, function(&program, "Control::control").span.col)
    );
    assert_eq!(function(&program, "dep_after").span.line, 15);
    let mine = function(&program, "Mine::mine");
    assert!(!program.symbols.file_is_dep(mine.span.file));
    assert!(mine.is_defined);
    assert_eq!(mine.span.line, 6);
    assert_eq!(function(&program, "project_defined").span.line, 8);
    let callers: Vec<&str> = program
        .symbols
        .call_sites
        .iter()
        .map(|cs| {
            program
                .symbols
                .functions
                .iter()
                .find(|f| f.id == cs.caller)
                .unwrap()
                .name
                .as_str()
        })
        .collect();
    assert_eq!(
        callers,
        ["main"],
        "dependency bodies and typeof operands contribute no calls"
    );
}

#[test]
fn typeof_controls_stay_unchanged() {
    let unchanged = [
        // Keyword substrings in identifiers, strings, raw strings, comments.
        "int __typeof__x = 1;\nint my__typeof__(int);\nint __typeofx(int);\n",
        "const char *s = \"__typeof__(int)\";\nconst char *r = R\"x(__typeof(int))x\";\n// __typeof__(int)\n/* __typeof(int) */\n",
        // Unclosed groups.
        "typedef __typeof__(int a;\n",
        "typedef __typeof(x b;\n",
        // Type-looking operands the simple shape does not cover stay as
        // written, and diagnosed: abstract array/function declarators,
        // template-ids, malformed modifier sequences.
        "typedef __typeof__(int[2]) a;\n",
        "typedef __typeof__(void (*)(int)) b;\n",
        "typedef __typeof__(int const int) d;\n",
        "typedef __typeof__(int * *&& const) e;\n",
    ];
    for source in unchanged {
        assert_eq!(normalized(source), source, "{source}");
    }
    for diagnosed in &unchanged[2..] {
        assert!(!parses_cleanly(diagnosed), "{diagnosed}");
    }
    // A template-id core is outside the simple shape; it takes the
    // expression path, keeps its operand bytes, and stays diagnosed.
    let template_id = "template<class T> struct W {};\ntypedef __typeof__(W<int> *) c;\n";
    assert_eq!(
        normalized(template_id),
        template_id.replace("__typeof__(W<int> *)", "decltype  (W<int> *)")
    );
    assert!(!parses_cleanly(template_id));
    // Operators inside an expression are not declarator layers.
    let expressions = "int x; int y;\ntypedef __typeof__(x * y) m;\ntypedef __typeof__(x & y) n;\ntypedef __typeof__(x && y) o;\ntypedef __typeof__(*x) p;\n";
    let output = normalized(expressions);
    assert_eq!(output.matches("decltype  (").count(), 4, "{output}");
    assert!(
        output.contains("(x * y) m")
            && output.contains("(x & y) n")
            && output.contains("(x && y) o")
            && output.contains("(*x) p")
    );
    // A bare fixed-width spelling the grammar lists as primitive takes the
    // type path (`decltype(uint32_t)` would not parse).
    let fixed = "typedef unsigned uint32_t;\ntypedef __typeof__(uint32_t) f;\n";
    assert_eq!(
        normalized(fixed),
        "typedef unsigned uint32_t;\ntypedef            uint32_t  f;\n"
    );
    assert_parses_cleanly(fixed);
}

#[test]
fn gnu_typeof_cv_qualified_names_are_types() {
    for operand in [
        "const C",
        "volatile C",
        "C const",
        "C volatile",
        "const volatile ::ns::C",
    ] {
        let source = format!(
            "struct C {{}}; namespace ns {{ struct C {{}}; }}\nusing Control = {operand};\nusing U = __typeof__({operand});\ntypedef __typeof({operand}) T;\nint after();\n"
        );
        assert_parses_cleanly(&source);
        let (_scratch, program) = project(&source);
        assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
        assert_eq!(alias(&program, "U"), alias(&program, "Control"));
        assert_eq!(alias(&program, "T"), alias(&program, "Control"));
        assert_ne!(alias(&program, "U"), TypeDesc::Unknown);
        assert_eq!(function(&program, "after").span.line, 5);
    }
}

#[test]
fn gnu_typeof_qualified_type_ignores_angle_brackets_in_comments() {
    let source = "namespace ns { struct C {}; }\ntypedef __typeof__(ns /* < */ ::C *) P;\n";
    assert_eq!(
        normalized(source),
        "namespace ns { struct C {}; }\ntypedef            ns /* < */ ::C *  P;\n"
    );
    assert_parses_cleanly(source);
}

#[test]
fn fixture_tree_indexes_project_and_dependency() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/cpp_gnu_typeof");
    let dep = root.join("dep");
    let program =
        build_program_with_jobs(&root, &PreprocessOptions::new().with_dep(&dep), 1).unwrap();
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert!(function(&program, "main").is_defined);
    assert!(!program
        .symbols
        .file_is_dep(function(&program, "main").span.file));
    assert!(program
        .symbols
        .file_is_dep(function(&program, "after_typeof").span.file));
    assert_eq!(alias(&program, "sum_result::type"), TypeDesc::Unknown);
    assert_eq!(alias(&program, "sum_result::itype"), TypeDesc::Int);
}

/// A `__typeof__` inside a class head with a base list is still rewritten.
#[test]
fn typeof_in_specialization_head_with_base_is_rewritten() {
    let source = "template<class> struct W;\nstruct B {};\ntemplate<> struct W<__typeof__(int)> : B { using t = __typeof__(int *); };\ntemplate<class T> auto g(T t) -> decltype(t);\nint after();\n";
    let output = normalized(source);
    assert_eq!(output.len(), source.len());
    assert!(
        output.contains("struct W<           int > : B { using t =            int * ; };"),
        "{output}"
    );
    assert_parses_cleanly(source);
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    assert_eq!(function(&program, "after").span.line, 5);
}

/// A globally qualified type core is a simple type like a qualified one.
#[test]
fn gnu_typeof_globally_qualified_type_operand_is_unwrapped() {
    let source = "namespace ns { struct C { int f; }; }\n\
using Ctl = ::ns::C *;\n\
using P = __typeof__(::ns::C *);\n\
using R = __typeof(const ::ns::C &);\n\
typedef ::ns::C *tctl;\n\
typedef __typeof__(::ns::C *) tp;\n\
int after();\n";
    let output = normalized(source);
    assert!(
        output.contains("using P =            ::ns::C * ;"),
        "{output}"
    );
    assert!(!output.contains("decltype"), "{output}");
    assert_parses_cleanly(source);
    let (_scratch, program) = project(source);
    assert_eq!(parse_diagnostics(&program), Vec::<String>::new());
    let control = alias(&program, "Ctl");
    assert!(matches!(control, TypeDesc::Ptr(_)), "{control:?}");
    assert_eq!(alias(&program, "P"), control);
    assert_eq!(alias(&program, "tp"), alias(&program, "tctl"));
    assert_ne!(alias(&program, "R"), TypeDesc::Unknown);
    assert_eq!(function(&program, "after").span.line, 7);
}
