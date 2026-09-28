use indexmap::IndexMap;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet as HashSet};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// `#include` edge: dependent file → included project file (canonical paths).
#[derive(Debug, Clone, Default)]
pub struct IncludeGraph {
    pub root: PathBuf,
    pub test_partition: trace_ir::TestPartition,
    /// Canonical roots whose entities contribute declarations only.
    pub dep_roots: Vec<PathBuf>,
    /// All `.c` / `.h` files under the analyzed root (canonical).
    pub project_files: HashSet<PathBuf>,
    /// Direct local include dependencies (dependent → included).
    pub edges: IndexMap<PathBuf, Vec<PathBuf>>,
    /// Search paths for `"..."` / `<...>` includes (project-local dirs).
    pub include_dirs: Vec<PathBuf>,
    /// Project files that should run through the preprocessor (have or receive `#include`s).
    pub needs_preprocess: HashSet<PathBuf>,
    /// Raw source text loaded while building the include graph (canonical paths).
    pub source_cache: FxHashMap<PathBuf, std::sync::Arc<str>>,
    /// Basename → project files (for fast include resolution without tree walks).
    pub basename_index: FxHashMap<String, Vec<PathBuf>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncludeKind {
    Local,
    System,
}

#[derive(Debug, Clone)]
struct IncludeRef {
    kind: IncludeKind,
    path: String,
}

impl IncludeGraph {
    pub fn build(root: &Path, c_files: &[PathBuf], h_files: &[PathBuf]) -> Self {
        Self::build_with_deps(root, c_files, h_files, &[], &[])
    }

    /// Include graph over the analyzed tree plus dependency-root headers
    /// (`--dep`, #60). Dependency headers resolve like project headers; the
    /// roots themselves join the include path so `<subdir/foo.h>` resolves.
    pub fn build_with_deps(
        root: &Path,
        c_files: &[PathBuf],
        h_files: &[PathBuf],
        dep_roots: &[PathBuf],
        dep_headers: &[PathBuf],
    ) -> Self {
        Self::build_with_partition(
            root,
            c_files,
            h_files,
            dep_roots,
            dep_headers,
            &trace_ir::TestPartition::default(),
        )
    }

    pub fn build_with_partition(
        root: &Path,
        c_files: &[PathBuf],
        h_files: &[PathBuf],
        dep_roots: &[PathBuf],
        dep_headers: &[PathBuf],
        test_partition: &trace_ir::TestPartition,
    ) -> Self {
        let root = trace_ir::canonicalize(root);
        let mut project_files: HashSet<PathBuf> = HashSet::default();
        for p in c_files
            .iter()
            .chain(h_files.iter())
            .chain(dep_headers.iter())
        {
            project_files.insert(canonicalize(p));
        }

        let include_dirs = discover_include_dirs(&root, h_files, dep_roots, dep_headers);
        let basename_index = build_basename_index(&project_files);

        let mut edges: IndexMap<PathBuf, Vec<PathBuf>> = IndexMap::new();
        let mut source_cache: FxHashMap<PathBuf, std::sync::Arc<str>> = FxHashMap::default();
        let project_list: Vec<PathBuf> = project_files.iter().cloned().collect();
        let scanned: Vec<(PathBuf, String, Vec<PathBuf>)> = project_list
            .par_iter()
            .filter_map(|path| {
                let Ok(content) = std::fs::read_to_string(path) else {
                    return None;
                };
                let mut deps = Vec::new();
                let from_is_test = trace_ir::is_test_path(&root, path, test_partition);
                for inc in scan_includes(&content) {
                    if let Some(resolved) = resolve_include(
                        &root,
                        test_partition,
                        path,
                        from_is_test,
                        &inc,
                        &include_dirs,
                        &basename_index,
                    ) {
                        let canon = canonicalize(&resolved);
                        if project_files.contains(&canon) {
                            deps.push(canon);
                        }
                    }
                }
                deps.sort();
                deps.dedup();
                Some((path.clone(), content, deps))
            })
            .collect();
        // Discovery uses the global pool; indexing uses a separate pool.
        // Drop discovery memos before their idle workers retain them for the run.
        rayon::broadcast(|_| trace_ir::release_thread_path_caches());
        for (path, content, deps) in scanned {
            source_cache.insert(path.clone(), std::sync::Arc::<str>::from(content));
            if !deps.is_empty() {
                edges.insert(path, deps);
            }
        }

        let needs_preprocess = Self::compute_needs_preprocess(&edges, &project_files);

        Self {
            root,
            test_partition: test_partition.clone(),
            dep_roots: dep_roots.iter().map(|p| canonicalize(p)).collect(),
            project_files,
            edges,
            include_dirs,
            needs_preprocess,
            source_cache,
            basename_index,
        }
    }

    fn compute_needs_preprocess(
        edges: &IndexMap<PathBuf, Vec<PathBuf>>,
        project_files: &HashSet<PathBuf>,
    ) -> HashSet<PathBuf> {
        let mut set = HashSet::default();
        for dep in edges.keys() {
            set.insert(dep.clone());
        }
        for targets in edges.values() {
            for t in targets {
                set.insert(t.clone());
            }
        }
        let _ = project_files;
        set
    }

    /// Files that should be preprocessed/expanded: any project file with `#include` or included by another.
    pub fn files_needing_includes(&self) -> &HashSet<PathBuf> {
        &self.needs_preprocess
    }

    /// Topological index order: dependencies before dependents.
    /// The input order does not affect the result: files are sorted up front
    /// and dependents are visited in sorted order, so two runs over the same
    /// tree always agree (std HashMap iteration would otherwise leak into
    /// downstream processing order and make output nondeterministic).
    pub fn index_order(&self, files: &[PathBuf]) -> Vec<PathBuf> {
        let mut ordered_files: Vec<PathBuf> = files.to_vec();
        ordered_files.sort();
        let file_set: HashSet<PathBuf> = ordered_files.iter().cloned().collect();

        let mut in_degree: IndexMap<PathBuf, usize> = IndexMap::new();
        for f in &ordered_files {
            in_degree.entry(f.clone()).or_insert(0);
        }
        for (dep, incs) in &self.edges {
            if !file_set.contains(dep) {
                continue;
            }
            for inc in incs {
                if file_set.contains(inc) {
                    *in_degree.entry(dep.clone()).or_insert(0) += 1;
                }
            }
        }

        let mut reverse: IndexMap<PathBuf, Vec<PathBuf>> = IndexMap::new();
        for (dep, incs) in &self.edges {
            if !file_set.contains(dep) {
                continue;
            }
            for inc in incs {
                if file_set.contains(inc) {
                    reverse.entry(inc.clone()).or_default().push(dep.clone());
                }
            }
        }

        let mut queue: VecDeque<PathBuf> = in_degree
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(f, _)| f.clone())
            .collect();
        queue.make_contiguous().sort();

        let mut order = Vec::with_capacity(ordered_files.len());
        while let Some(node) = queue.pop_front() {
            order.push(node.clone());
            if let Some(dependents) = reverse.get(&node) {
                let mut next: Vec<&PathBuf> = dependents.iter().collect();
                next.sort();
                for dep in next {
                    if let Some(deg) = in_degree.get_mut(dep) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back((*dep).clone());
                        }
                    }
                }
            }
        }

        // Whatever a cycle left unvisited, appended in sorted order. Membership
        // comes from a set: `order.contains` re-scanned the whole prefix once
        // per file, so EVERY tree paid O(V^2) here whether or not it had a
        // cycle -- the scan runs over all V files, and cycles only decide how
        // many of them are left to append (1,483 files on hdf) (#83).
        // The set grows as files are appended, the way `order.contains` was
        // re-evaluated: `files` is deduped by every caller in-tree, but this is
        // `pub`, and a duplicate cyclic leftover must not index its unit twice.
        let mut placed: HashSet<PathBuf> = order.iter().cloned().collect();
        let missing: Vec<PathBuf> = ordered_files
            .iter()
            .filter(|f| placed.insert((*f).clone()))
            .cloned()
            .collect();
        order.extend(missing);
        order
    }

    /// Topological waves: each wave's includes (inside `files`) are in
    /// earlier waves, so the wave can be indexed in parallel.
    ///
    /// Cyclic leftovers are **not** a parallel wave: they still depend on
    /// each other, so they must be indexed in [`Self::index_order`] (the same
    /// append [`Self::index_order`] uses for cycles).
    pub fn index_waves(&self, files: &[PathBuf]) -> (Vec<Vec<PathBuf>>, Vec<PathBuf>) {
        let mut ordered_files: Vec<PathBuf> = files.to_vec();
        ordered_files.sort();
        let file_set: HashSet<PathBuf> = ordered_files.iter().cloned().collect();

        let mut in_degree: IndexMap<PathBuf, usize> = IndexMap::new();
        for f in &ordered_files {
            in_degree.entry(f.clone()).or_insert(0);
        }
        for (dep, incs) in &self.edges {
            if !file_set.contains(dep) {
                continue;
            }
            for inc in incs {
                if file_set.contains(inc) {
                    *in_degree.entry(dep.clone()).or_insert(0) += 1;
                }
            }
        }

        let mut reverse: IndexMap<PathBuf, Vec<PathBuf>> = IndexMap::new();
        for (dep, incs) in &self.edges {
            if !file_set.contains(dep) {
                continue;
            }
            for inc in incs {
                if file_set.contains(inc) {
                    reverse.entry(inc.clone()).or_default().push(dep.clone());
                }
            }
        }

        let mut remaining: HashSet<PathBuf> = file_set;
        let mut waves = Vec::new();
        while !remaining.is_empty() {
            let mut wave: Vec<PathBuf> = remaining
                .iter()
                .filter(|f| in_degree.get(*f).copied().unwrap_or(0) == 0)
                .cloned()
                .collect();
            if wave.is_empty() {
                let mut rest: Vec<PathBuf> = remaining.iter().cloned().collect();
                rest.sort();
                return (waves, rest);
            }
            wave.sort();
            for node in &wave {
                remaining.remove(node);
                if let Some(dependents) = reverse.get(node) {
                    for dep in dependents {
                        if let Some(deg) = in_degree.get_mut(dep) {
                            *deg = deg.saturating_sub(1);
                        }
                    }
                }
            }
            waves.push(wave);
        }
        (waves, Vec::new())
    }

    /// Record `#include` edges discovered only after preprocessing
    /// (macro-expanded includes the raw scanner misses). Used to order PCH
    /// so a header is not lowered in parallel with a nested type it needs.
    pub fn add_preprocess_includes(&mut self, from: &Path, headers: &[PathBuf]) {
        let from = self.intern_path(from);
        if !self.project_files.contains(&from) {
            return;
        }
        for h in headers {
            let to = self.intern_path(h);
            if to == from || !self.project_files.contains(&to) {
                continue;
            }
            let e = self.edges.entry(from.clone()).or_default();
            if !e.contains(&to) {
                e.push(to);
            }
        }
    }

    /// `path` as stored in [`Self::project_files`] when it is already a key,
    /// otherwise [`trace_ir::canonicalize`] (syscall).
    pub fn intern_path(&self, path: &Path) -> PathBuf {
        if self.project_files.contains(path) {
            return path.to_path_buf();
        }
        canonicalize(path)
    }

    /// Project files reachable from `start`, including `start`.
    pub fn reachable_paths<'a>(&'a self, start: &'a Path) -> HashSet<&'a Path> {
        let mut seen: HashSet<&Path> = HashSet::default();
        let mut queue: VecDeque<&Path> = VecDeque::new();
        if seen.insert(start) {
            queue.push_back(start);
        }
        while let Some(node) = queue.pop_front() {
            let Some(incs) = self.edges.get(node) else {
                continue;
            };
            for inc in incs {
                if self.project_files.contains(inc) && seen.insert(inc.as_path()) {
                    queue.push_back(inc.as_path());
                }
            }
        }
        seen
    }

    pub fn reachable_from<'a>(
        &self,
        sources: impl IntoIterator<Item = &'a PathBuf>,
    ) -> HashSet<PathBuf> {
        let mut seen = HashSet::default();
        let mut queue: VecDeque<PathBuf> = VecDeque::new();
        for s in sources {
            if seen.insert(s.clone()) {
                queue.push_back(s.clone());
            }
        }
        while let Some(node) = queue.pop_front() {
            if let Some(includes) = self.edges.get(&node) {
                for inc in includes {
                    if self.project_files.contains(inc) && seen.insert(inc.clone()) {
                        queue.push_back(inc.clone());
                    }
                }
            }
        }
        seen
    }

    pub fn edge_list(&self) -> Vec<(PathBuf, PathBuf)> {
        let mut out = Vec::new();
        for (from, tos) in &self.edges {
            for to in tos {
                out.push((from.clone(), to.clone()));
            }
        }
        out
    }
}

fn canonicalize(path: &Path) -> PathBuf {
    trace_ir::canonicalize(path)
}

fn discover_include_dirs(
    root: &Path,
    headers: &[PathBuf],
    dep_roots: &[PathBuf],
    dep_headers: &[PathBuf],
) -> Vec<PathBuf> {
    let mut dirs: HashSet<PathBuf> = HashSet::default();
    dirs.insert(canonicalize(root));
    for h in headers.iter().chain(dep_headers.iter()) {
        if let Some(parent) = h.parent() {
            dirs.insert(canonicalize(parent));
        }
    }
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
    {
        if entry.file_name() == "include" {
            dirs.insert(canonicalize(entry.path()));
        }
    }
    // Each dependency header's own directory is already in `dirs` above; the
    // root itself is what a rooted spelling (`<subdir/foo.h>`) resolves from.
    for dep in dep_roots {
        dirs.insert(canonicalize(dep));
    }
    let mut v: Vec<PathBuf> = dirs.into_iter().collect();
    v.sort();
    v
}

fn scan_includes(source: &str) -> Vec<IncludeRef> {
    let mut out = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("#include") {
            continue;
        }
        let rest = trimmed["#include".len()..].trim();
        if let Some(path) = rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            out.push(IncludeRef {
                kind: IncludeKind::Local,
                path: path.to_string(),
            });
        } else if let Some(path) = rest.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
            out.push(IncludeRef {
                kind: IncludeKind::System,
                path: path.to_string(),
            });
        }
    }
    out
}

fn build_basename_index(project_files: &HashSet<PathBuf>) -> FxHashMap<String, Vec<PathBuf>> {
    let mut index: FxHashMap<String, Vec<PathBuf>> = FxHashMap::default();
    for path in project_files {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            index
                .entry(name.to_string())
                .or_default()
                .push(path.clone());
        }
    }
    for paths in index.values_mut() {
        paths.sort();
    }
    index
}

fn resolve_include(
    root: &Path,
    test_partition: &trace_ir::TestPartition,
    from: &Path,
    from_is_test: bool,
    inc: &IncludeRef,
    include_dirs: &[PathBuf],
    basename_index: &FxHashMap<String, Vec<PathBuf>>,
) -> Option<PathBuf> {
    // Probed lazily, in the same order as before: the including directory
    // first for a `"..."` include, then the search list. The candidates used
    // to be collected into a `Vec` up front, so every include allocated one
    // `PathBuf` per search directory -- 205 to 291 of them on the corpora --
    // while the probe below almost always stops at the first or second (#83).
    // The probe itself goes through the run's memo: an include naming no
    // project file (`<string>`, `<vector>`) walks the whole list, and every
    // file that spells it walked it again with a `stat` per candidate --
    // ten seconds of kernel time on camera, at any job count.
    // The test partition is checked only on a candidate that exists: a path
    // walk per search directory cost more than the memoized probe itself.
    let local_first = match inc.kind {
        IncludeKind::Local => from.parent(),
        IncludeKind::System => None,
    };
    if let Some(cand) = local_first.map(|parent| parent.join(&inc.path)) {
        if trace_ir::is_file_cached(&cand) {
            return Some(cand);
        }
    }
    // A production includer never takes a test/mock candidate from the
    // inferred search (docs/ANALYSIS.md, "Declaring-header eligibility").
    let admits = |p: &Path| from_is_test || !trace_ir::is_test_path(root, p, test_partition);
    for dir in include_dirs {
        let cand = dir.join(&inc.path);
        if trace_ir::is_file_cached(&cand) && admits(&cand) {
            return Some(cand);
        }
    }

    // Last resort: the unique admitted match under project by filename.
    let name = Path::new(&inc.path).file_name().and_then(|n| n.to_str())?;
    let mut admitted = basename_index.get(name)?.iter().filter(|p| admits(p));
    match (admitted.next(), admitted.next()) {
        (Some(only), None) => Some(only.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[allow(clippy::cloned_ref_to_slice_refs)]
    #[test]
    fn include_graph_resolves_local_and_orders() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();
        fs::write(tmp.join("api.h"), "struct S { int x; };\n").unwrap();
        fs::write(
            tmp.join("main.c"),
            "#include \"api.h\"\nint f() { return 0; }\n",
        )
        .unwrap();

        let c = vec![tmp.join("main.c")];
        let h = vec![tmp.join("api.h")];
        let mut g = IncludeGraph::build(tmp, &c, &h);
        assert_eq!(g.edges.len(), 1);
        let deps = g.edges.get(&canonicalize(&tmp.join("main.c"))).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], canonicalize(&tmp.join("api.h")));

        let main = canonicalize(&tmp.join("main.c"));
        let api = canonicalize(&tmp.join("api.h"));
        assert_eq!(g.intern_path(&main), main);
        let reach = g.reachable_paths(&main);
        assert!(reach.contains(main.as_path()));
        assert!(reach.contains(api.as_path()));

        let all = [
            canonicalize(&tmp.join("api.h")),
            canonicalize(&tmp.join("main.c")),
        ];
        let order = g.index_order(&all);
        assert_eq!(order[0], canonicalize(&tmp.join("api.h")));
        assert_eq!(order[1], canonicalize(&tmp.join("main.c")));

        let (waves, leftover) = g.index_waves(&all);
        assert!(leftover.is_empty());
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[0], vec![canonicalize(&tmp.join("api.h"))]);
        assert_eq!(waves[1], vec![canonicalize(&tmp.join("main.c"))]);

        // A preprocess-only include (raw scanner missed it) must still
        // serialize the includer after the nested header.
        let late = canonicalize(&tmp.join("late.h"));
        fs::write(tmp.join("late.h"), "struct T { int y; };\n").unwrap();
        g.project_files.insert(late.clone());
        g.add_preprocess_includes(&tmp.join("main.c"), &[late.clone()]);
        let all2 = [
            canonicalize(&tmp.join("api.h")),
            late.clone(),
            canonicalize(&tmp.join("main.c")),
        ];
        let (waves2, leftover2) = g.index_waves(&all2);
        assert!(leftover2.is_empty());
        assert!(
            waves2.iter().position(|w| w.contains(&late)).unwrap()
                < waves2
                    .iter()
                    .position(|w| w.contains(&canonicalize(&tmp.join("main.c"))))
                    .unwrap(),
            "preprocess edge must put late.h before main.c"
        );
    }

    #[test]
    fn index_waves_keeps_include_cycles_sequential() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();
        fs::write(tmp.join("a.h"), "#include \"b.h\"\nstruct A { int x; };\n").unwrap();
        fs::write(tmp.join("b.h"), "#include \"a.h\"\nstruct B { int y; };\n").unwrap();
        let h = vec![tmp.join("a.h"), tmp.join("b.h")];
        let g = IncludeGraph::build(tmp, &[], &h);
        let all = [
            canonicalize(&tmp.join("a.h")),
            canonicalize(&tmp.join("b.h")),
        ];
        let (waves, leftover) = g.index_waves(&all);
        assert!(
            leftover.contains(&canonicalize(&tmp.join("a.h")))
                && leftover.contains(&canonicalize(&tmp.join("b.h"))),
            "cyclic pair must be leftover, not a parallel wave: waves={waves:?} leftover={leftover:?}"
        );
        assert!(
            waves
                .iter()
                .all(|w| w.iter().all(|f| !leftover.contains(f))),
            "leftover files must not also appear in a parallel wave"
        );
    }

    #[test]
    fn production_includers_never_take_test_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let root_dir = dir.path();
        for file in [
            "mock/a.h",
            "zsrc/a.h",
            "test/only.h",
            "src/b.h",
            "test/mock/b.h",
        ] {
            let path = root_dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        let dirs: Vec<PathBuf> = ["mock", "test", "zsrc"]
            .iter()
            .map(|d| root_dir.join(d))
            .collect();
        let index = build_basename_index(
            &[
                "src/b.h",
                "test/mock/b.h",
                "test/only.h",
                "mock/a.h",
                "zsrc/a.h",
            ]
            .iter()
            .map(|f| root_dir.join(f))
            .collect(),
        );
        let resolve = |from: &str, spelled: &str| {
            let from = root_dir.join(from);
            let from_is_test =
                trace_ir::is_test_path(root_dir, &from, &trace_ir::TestPartition::default());
            let inc = IncludeRef {
                kind: IncludeKind::System,
                path: spelled.to_string(),
            };
            resolve_include(
                root_dir,
                &trace_ir::TestPartition::default(),
                &from,
                from_is_test,
                &inc,
                &dirs,
                &index,
            )
            .map(|p| p.strip_prefix(root_dir).unwrap().to_path_buf())
        };
        // A later production directory wins over an earlier mock one...
        assert_eq!(resolve("app/u.c", "a.h"), Some("zsrc/a.h".into()));
        // ...but test code keeps the search order.
        assert_eq!(resolve("test/u.c", "a.h"), Some("mock/a.h".into()));
        // A header only under a test directory stays unresolved for
        // production code, by search or by basename.
        assert_eq!(resolve("app/u.c", "only.h"), None);
        assert_eq!(resolve("test/u.c", "only.h"), Some("test/only.h".into()));
        // Basename fallback: the one production match is taken; test code
        // sees two matches and takes neither.
        assert_eq!(resolve("app/u.c", "gone/b.h"), Some("src/b.h".into()));
        assert_eq!(resolve("test/u.c", "gone/b.h"), None);
    }
}
