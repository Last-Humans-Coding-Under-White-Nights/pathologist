//! Inferred bare-tree include directories are the last search class: an
//! explicit `-I` or `-isystem` directory wins over a same-named tree header.
//! See docs/ANALYSIS.md, "Compilation databases (#62)".

use trace_preproc::{preprocess_string, PreprocessOptions};

fn resolve(explicit: impl FnOnce(&mut PreprocessOptions, std::path::PathBuf)) -> String {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (sub, marker) in [("explicit", "from_explicit"), ("tree", "from_tree")] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
        std::fs::write(root.join(sub).join("shared.h"), format!("int {marker};\n")).unwrap();
    }
    let mut opts = PreprocessOptions::new();
    opts.inferred_include_paths.push(root.join("tree"));
    explicit(&mut opts, root.join("explicit"));
    preprocess_string("#include <shared.h>\n", &root.join("main.c"), &opts).output
}

#[test]
fn system_directory_precedes_inferred_directory() {
    let output = resolve(|opts, dir| opts.system_include_paths.push(dir));
    assert!(output.contains("from_explicit"), "{output}");
    assert!(!output.contains("from_tree"), "{output}");
}

#[test]
fn include_directory_precedes_inferred_directory() {
    let output = resolve(|opts, dir| opts.include_paths.push(dir));
    assert!(output.contains("from_explicit"), "{output}");
    assert!(!output.contains("from_tree"), "{output}");
}

#[test]
fn inferred_directory_resolves_when_nothing_else_does() {
    let output = resolve(|_, _| {});
    assert!(output.contains("from_tree"), "{output}");
}
