//! Conservative GN source and dependency ownership inference.
//! See docs/ANALYSIS.md, "Link targets and weak symbols".
use crate::link_commands::{LinkDatabase, LinkTargetSpec};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Clone, Debug)]
struct Token {
    text: String,
    quoted: bool,
}

fn lex(text: &str) -> Result<Vec<Token>, String> {
    let mut chars = text.chars().peekable();
    let mut out = Vec::new();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c == '#' {
            for ch in chars.by_ref() {
                if ch == '\n' {
                    break;
                }
            }
            continue;
        }
        let mut word = String::from(c);
        let quoted = c == '"';
        if quoted {
            word.clear();
            let mut closed = false;
            while let Some(ch) = chars.next() {
                if ch == '"' {
                    closed = true;
                    break;
                }
                if ch == '\\' {
                    let next = chars.next().ok_or("unfinished escape")?;
                    if next == '$' {
                        word.push('\0');
                    } else {
                        word.push(next);
                    }
                } else {
                    word.push(ch);
                }
            }
            if !closed {
                return Err("unterminated string".into());
            }
        } else if c.is_alphanumeric() || c == '_' {
            while chars
                .peek()
                .is_some_and(|c| c.is_alphanumeric() || *c == '_')
            {
                word.push(chars.next().unwrap());
            }
        } else if matches!(c, '+' | '-' | '=' | '!' | '<' | '>') && chars.peek() == Some(&'=') {
            word.push(chars.next().unwrap());
        }
        out.push(Token { text: word, quoted });
    }
    Ok(out)
}

#[derive(Clone, Debug, Default)]
struct Value {
    strings: Vec<String>,
    list: bool,
    known: bool,
}
impl Value {
    fn list() -> Self {
        Self {
            strings: Vec::new(),
            list: true,
            known: true,
        }
    }
    fn union(&mut self, other: Self) {
        self.known &= other.known;
        for item in other.strings {
            if !self.strings.contains(&item) {
                self.strings.push(item);
            }
        }
    }
}
/// A variable scope. A target body writes into its own layer over the
/// enclosing scope instead of copying it; the first write to an inherited
/// name copies that one value, so reads see exactly what a full copy would.
#[derive(Default)]
struct Env<'p> {
    vars: BTreeMap<String, Value>,
    parent: Option<&'p Env<'p>>,
}
impl Env<'_> {
    fn get(&self, name: &str) -> Option<&Value> {
        let mut env = self;
        loop {
            if let Some(value) = env.vars.get(name) {
                return Some(value);
            }
            env = env.parent?;
        }
    }
    fn entry(&mut self, name: &str, default: fn() -> Value) -> &mut Value {
        let parent = self.parent;
        self.vars.entry(name.to_string()).or_insert_with(|| {
            parent
                .and_then(|p| p.get(name))
                .cloned()
                .unwrap_or_else(default)
        })
    }
    fn insert(&mut self, name: &str, value: Value) {
        self.vars.insert(name.to_string(), value);
    }
}

#[cfg(test)]
thread_local! {
    static IMPORT_LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
fn load(path: &Path) -> Result<Vec<Token>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    lex(&text)
}

struct Target {
    label: String,
    sources: Vec<PathBuf>,
    deps: Vec<String>,
    complete: bool,
}
struct Reader {
    root: PathBuf,
    prefix: Option<String>,
    workspace: bool,
    targets: Vec<Target>,
    warnings: BTreeSet<String>,
    imports: BTreeSet<PathBuf>,
    /// Imported files are read and lexed once per run, failures included.
    /// Evaluation still happens per import, in the importing scope.
    imported: HashMap<PathBuf, Result<Rc<[Token]>, String>>,
}
impl Reader {
    fn path(&self, directory: &Path, text: &str) -> Option<PathBuf> {
        let path = if let Some(rest) = text.strip_prefix("//") {
            if self.workspace {
                self.root.join(rest)
            } else {
                let prefix = self.prefix.as_deref()?;
                let rest = rest.strip_prefix(prefix)?;
                if !rest.is_empty() && !rest.starts_with('/') {
                    return None;
                }
                self.root.join(rest.trim_start_matches('/'))
            }
        } else {
            directory.join(text)
        };
        let path = trace_ir::resolve_against(directory, &path);
        path.starts_with(&self.root).then_some(path)
    }
    fn label(&self, directory: &Path, text: &str) -> Option<String> {
        if text.contains('(') {
            return None;
        }
        let (dir, name) = match text.rsplit_once(':') {
            Some((dir, name)) => (self.path(directory, dir)?, name.to_string()),
            None => {
                let dir = self.path(directory, text)?;
                let name = dir.file_name()?.to_str()?.to_string();
                (dir, name)
            }
        };
        Some(format!(
            "//{}:{name}",
            dir.strip_prefix(&self.root).ok()?.to_string_lossy()
        ))
    }
    fn import(&mut self, path: &Path) -> Result<Rc<[Token]>, String> {
        self.imported
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                #[cfg(test)]
                IMPORT_LOADS.with(|n| n.set(n.get() + 1));
                load(path).map(Rc::from)
            })
            .clone()
    }
    /// Evaluates the loaded `tokens` of `path` in `env`. Read and lex
    /// failures surface only after the cycle check, as they always have.
    fn file(
        &mut self,
        path: &Path,
        tokens: Result<Rc<[Token]>, String>,
        env: &mut Env<'_>,
        depth: usize,
    ) -> Result<(), String> {
        if depth > 32 || !self.imports.insert(path.to_path_buf()) {
            return Err("cyclic or excessive imports".into());
        }
        let result = tokens.and_then(|tokens| {
            let mut parser = Parser {
                tokens: &tokens,
                pos: 0,
                reader: self,
                directory: path.parent().unwrap(),
                depth,
            };
            parser.block(env, false, false)
        });
        self.imports.remove(path);
        result
    }
}
struct Parser<'a, 'b> {
    tokens: &'a [Token],
    pos: usize,
    reader: &'b mut Reader,
    directory: &'a Path,
    depth: usize,
}
impl<'a> Parser<'a, '_> {
    fn at(&self, text: &str) -> bool {
        self.tokens
            .get(self.pos)
            .is_some_and(|t| !t.quoted && t.text == text)
    }
    fn eat(&mut self, text: &str) -> bool {
        if self.at(text) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn group(&mut self, open: &str, close: &str) -> Result<&'a [Token], String> {
        let tokens = self.tokens;
        if !self.eat(open) {
            return Err(format!("expected {open}"));
        }
        let start = self.pos;
        let mut depth = 1;
        while self.pos < self.tokens.len() {
            if self.at(open) {
                depth += 1;
            }
            if self.at(close) {
                depth -= 1;
                if depth == 0 {
                    let end = self.pos;
                    self.pos += 1;
                    return Ok(&tokens[start..end]);
                }
            }
            self.pos += 1;
        }
        Err(format!("unclosed {open}"))
    }
    fn string(text: &str, env: &Env) -> Value {
        let mut result = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\0' {
                result.push('$');
                continue;
            }
            if c != '$' {
                result.push(c);
                continue;
            }
            let braced = chars.peek() == Some(&'{');
            if braced {
                chars.next();
            }
            let mut name = String::new();
            while chars
                .peek()
                .is_some_and(|c| c.is_alphanumeric() || *c == '_')
            {
                name.push(chars.next().unwrap());
            }
            if braced && chars.next() != Some('}') {
                return Value::default();
            }
            let Some(value) = env
                .get(&name)
                .filter(|v| v.known && !v.list && v.strings.len() == 1)
            else {
                return Value::default();
            };
            result.push_str(&value.strings[0]);
        }
        Value {
            strings: vec![result],
            list: false,
            known: true,
        }
    }
    fn atom(&mut self, env: &Env) -> Value {
        if self.eat("[") {
            let mut result = Value::list();
            while self.pos < self.tokens.len() && !self.at("]") {
                let start = self.pos;
                let value = self.expr(env);
                result.union(value);
                self.eat(",");
                if start == self.pos {
                    self.pos += 1;
                    result.known = false;
                }
            }
            if !self.eat("]") {
                result.known = false;
            }
            return result;
        }
        if self.at("{") {
            // A scope literal (`sanitize = { cfi = true }`) is its own scope:
            // skip it whole so its closing brace cannot end the enclosing
            // target. Its value never names sources, so it stays unknown.
            let _ = self.group("{", "}");
            return Value::default();
        }
        let Some(token) = self.tokens.get(self.pos) else {
            return Value::default();
        };
        if matches!(token.text.as_str(), "}" | "]") && !token.quoted {
            return Value::default();
        }
        self.pos += 1;
        if token.quoted {
            return Self::string(&token.text, env);
        }
        let value = env.get(&token.text).cloned().unwrap_or_default();
        if self.at("(") {
            let _ = self.group("(", ")");
            return Value::default();
        }
        value
    }
    fn expr(&mut self, env: &Env) -> Value {
        let mut value = self.atom(env);
        while self.eat("+") {
            let other = self.atom(env);
            if value.list && other.list {
                value.union(other);
            } else if !value.list
                && !other.list
                && value.strings.len() == 1
                && other.strings.len() == 1
            {
                value.strings[0].push_str(&other.strings[0]);
                value.known &= other.known;
            } else {
                value.known = false;
            }
        }
        value
    }
    fn block(&mut self, env: &mut Env<'_>, nested: bool, conditional: bool) -> Result<(), String> {
        let tokens = self.tokens;
        while self.pos < tokens.len() && !self.at("}") {
            let name = tokens[self.pos].text.as_str();
            self.pos += 1;
            if self.at("=") || self.at("+=") || self.at("-=") {
                let op = tokens[self.pos].text.as_str();
                self.pos += 1;
                let value = self.expr(env);
                if op == "-=" {
                    // Keeping removed members is a safe union only when all
                    // operands are known. Unknown subtraction cannot assert ownership.
                    if !value.known {
                        env.entry(name, Value::default).known = false;
                    } else if !conditional {
                        let old = env.entry(name, Value::default);
                        old.strings.retain(|v| !value.strings.contains(v));
                    }
                } else if op == "+=" || conditional {
                    env.entry(name, Value::list).union(value);
                } else {
                    env.insert(name, value);
                }
                continue;
            }
            if !self.at("(") {
                continue;
            }
            let args = self.group("(", ")")?;
            if name == "import" {
                let value = args
                    .first()
                    .filter(|t| t.quoted)
                    .map(|t| Self::string(&t.text, env))
                    .unwrap_or_default();
                if value.known && value.strings.len() == 1 {
                    // An in-tree import that is not on disk (generated during
                    // the build, or optional) is unresolved like an
                    // out-of-tree one: it must not discard the whole file.
                    if let Some(path) = self
                        .reader
                        .path(self.directory, &value.strings[0])
                        .filter(|path| path.is_file())
                    {
                        let tokens = self.reader.import(&path);
                        self.reader.file(&path, tokens, env, self.depth + 1)?;
                        continue;
                    }
                }
                self.reader.warnings.insert(format!(
                    "GN {}: unresolved import; ownership remains unscoped",
                    self.directory.display()
                ));
                continue;
            }
            if !self.eat("{") {
                continue;
            }
            if name == "if" {
                self.block(env, true, true)?;
                if self.eat("else") && self.eat("{") {
                    self.block(env, true, true)?;
                }
                // An else-if is consumed by the next iteration as another
                // conditional alternative, retaining the accumulated union.
            } else if name == "declare_args" {
                self.block(env, true, conditional)?;
            } else if is_target(name) {
                let mut local = Env {
                    vars: BTreeMap::new(),
                    parent: Some(env),
                };
                for key in ["sources", "deps", "public_deps"] {
                    local.insert(key, Value::list());
                }
                self.block(&mut local, true, false)?;
                // The name may be a literal or a variable naming one
                // (`shared_library(name) { ... }`), evaluated as any value is.
                let target_name = Parser {
                    tokens: args,
                    pos: 0,
                    reader: &mut *self.reader,
                    directory: self.directory,
                    depth: self.depth,
                }
                .expr(env);
                let label =
                    (target_name.known && !target_name.list && target_name.strings.len() == 1)
                        .then(|| {
                            self.reader
                                .label(self.directory, &format!(":{}", target_name.strings[0]))
                        })
                        .flatten();
                // One unnamed target leaves the rest of the file readable;
                // the warning keeps resolution unscoped.
                let Some(label) = label else {
                    self.reader.warnings.insert(format!(
                        "GN {}: unresolved {name} target name; ownership remains unscoped",
                        self.directory.display()
                    ));
                    continue;
                };
                let sources = &local.vars["sources"];
                let mut target = Target {
                    label,
                    sources: Vec::new(),
                    deps: Vec::new(),
                    complete: sources.known,
                };
                for source in &sources.strings {
                    match self.reader.path(self.directory, source) {
                        Some(path) if path.is_file() => target.sources.push(path),
                        _ => target.complete = false,
                    }
                }
                for key in ["deps", "public_deps"] {
                    target.complete &= local.vars[key].known;
                    for dep in &local.vars[key].strings {
                        if let Some(label) = self.reader.label(self.directory, dep) {
                            target.deps.push(label);
                        } else {
                            target.complete = false;
                        }
                    }
                }
                target.sources.sort();
                target.sources.dedup();
                target.deps.sort();
                target.deps.dedup();
                self.reader.targets.push(target);
            } else {
                // Unsupported scopes may change lists in the enclosing target.
                let start = self.pos;
                let mut depth = 1;
                while self.pos < self.tokens.len() && depth > 0 {
                    if self.at("{") {
                        depth += 1;
                    }
                    if self.at("}") {
                        depth -= 1;
                    }
                    self.pos += 1;
                }
                if depth != 0 {
                    return Err("unclosed block".into());
                }
                let body = &self.tokens[start..self.pos];
                let names_ownership = || {
                    body.iter().any(|t| {
                        !t.quoted && matches!(t.text.as_str(), "sources" | "deps" | "public_deps")
                    })
                };
                // `set_defaults` that only sets configs or visibility cannot
                // change ownership; one that seeds sources or deps can.
                let benign = matches!(
                    name,
                    "config" | "action" | "action_foreach" | "copy" | "template" | "foreach"
                ) || (name == "set_defaults" && !names_ownership());
                if !benign {
                    self.reader.warnings.insert(format!(
                        "GN {}: unsupported block {name}; ownership remains unscoped",
                        self.directory.display()
                    ));
                }
                if name == "foreach" {
                    for key in ["sources", "deps", "public_deps"] {
                        if self.tokens[start..self.pos]
                            .iter()
                            .any(|t| !t.quoted && t.text == key)
                        {
                            env.entry(key, Value::default).known = false;
                        }
                    }
                }
            }
        }
        if nested && !self.eat("}") {
            return Err("unclosed block".into());
        }
        if !nested && self.at("}") {
            return Err("unexpected closing brace".into());
        }
        Ok(())
    }
}
fn is_target(name: &str) -> bool {
    matches!(
        name,
        "executable"
            | "shared_library"
            | "static_library"
            | "loadable_module"
            | "source_set"
            | "group"
            | "ohos_shared_library"
            | "ohos_static_library"
            | "ohos_executable"
            | "ohos_source_set"
            | "ohos_unittest"
            | "ohos_fuzztest"
            | "ohos_moduletest"
    )
}

pub(crate) fn infer_link_targets(root: &Path) -> LinkDatabase {
    let root = trace_ir::canonicalize(root);
    let prefix = std::fs::read(root.join("bundle.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|v| {
            v.pointer("/segment/destPath")
                .and_then(|v| v.as_str())
                .map(|s| s.trim_matches('/').to_string())
        });
    let mut reader = Reader {
        workspace: root.join(".gn").is_file(),
        root: root.clone(),
        prefix,
        targets: Vec::new(),
        warnings: BTreeSet::new(),
        imports: BTreeSet::new(),
        imported: HashMap::new(),
    };
    let builds: Vec<PathBuf> = walkdir::WalkDir::new(&root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !matches!(
                    e.file_name().to_str(),
                    Some(".git" | "target" | "node_modules")
                )
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.file_name() == "BUILD.gn")
        .map(walkdir::DirEntry::into_path)
        .collect();
    // Build files are read and lexed in parallel, a bounded chunk at a time,
    // and evaluated sequentially in discovery order.
    for chunk in builds.chunks(256) {
        let loaded: Vec<_> = chunk.par_iter().map(|path| load(path)).collect();
        for (path, tokens) in chunk.iter().zip(loaded) {
            let start = reader.targets.len();
            let tokens = tokens.map(Rc::from);
            if let Err(error) = reader.file(path, tokens, &mut Env::default(), 0) {
                reader.targets.truncate(start);
                reader.warnings.insert(format!(
                    "GN {}: {error}; ownership remains unscoped",
                    path.display()
                ));
            }
        }
    }
    reader.targets.sort_by(|a, b| a.label.cmp(&b.label));
    let mut by_label = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    for (i, target) in reader.targets.iter().enumerate() {
        if by_label.insert(target.label.clone(), i).is_some() {
            duplicates.insert(target.label.clone());
        }
    }
    for target in &mut reader.targets {
        if duplicates.contains(&target.label) {
            target.complete = false;
        }
    }
    // Incompleteness reaches every target that transitively depends on an
    // incomplete or missing one; walk reverse dependencies once.
    let mut dependents = vec![Vec::new(); reader.targets.len()];
    let mut pending = Vec::new();
    for (i, target) in reader.targets.iter_mut().enumerate() {
        for dep in &target.deps {
            match by_label.get(dep) {
                Some(&d) => dependents[d].push(i),
                None => target.complete = false,
            }
        }
        if !target.complete {
            pending.push(i);
        }
    }
    while let Some(i) = pending.pop() {
        for &dependent in &dependents[i] {
            if std::mem::replace(&mut reader.targets[dependent].complete, false) {
                pending.push(dependent);
            }
        }
    }
    // Incomplete targets are exported too, as observations: any of them
    // leaves a warning, and a warning keeps resolution unscoped, so they never
    // form an image. Only dependencies on known targets are recorded.
    let ids: BTreeMap<_, _> = reader
        .targets
        .iter()
        .enumerate()
        .map(|(i, t)| (t.label.clone(), i))
        .collect();
    let mut db = LinkDatabase::default();
    for target in reader.targets {
        if !target.complete {
            reader.warnings.insert(format!(
                "GN {}: incomplete source/dependency evidence; ownership remains unscoped",
                target.label
            ));
        }
        db.targets.push(LinkTargetSpec {
            output: root.join(".trace-gn").join(
                target
                    .label
                    .trim_start_matches("//")
                    .replace(':', "/")
                    .trim_start_matches('/'),
            ),
            name: target.label,
            sources: target.sources,
            dependencies: target
                .deps
                .iter()
                .filter_map(|d| ids.get(d).copied())
                .collect(),
            configurations: BTreeMap::new(),
        });
    }
    db.inferred = true;
    db.unscoped_inference = !reader.warnings.is_empty();
    db.warnings = reader.warnings.into_iter().collect();
    db
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in files {
            let file = dir.path().join(name);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, contents).unwrap();
        }
        dir
    }
    #[test]
    fn target_kinds_and_literal_dependencies() {
        for kind in [
            "ohos_shared_library",
            "ohos_static_library",
            "ohos_executable",
            "ohos_unittest",
            "ohos_fuzztest",
            "ohos_moduletest",
            "executable",
            "shared_library",
            "static_library",
            "source_set",
            "group",
        ] {
            let build = format!("{kind}(\"app\") {{ sources = [\"a.cpp\"] deps = [\":lib\"] data_deps = [\":data\"] }} source_set(\"lib\") {{ sources = [\"b.cpp\"] }} action(\"data\") {{ sources = [\"c.cpp\"] }}");
            let dir = tree(&[
                ("BUILD.gn", &build),
                ("a.cpp", ""),
                ("b.cpp", ""),
                ("c.cpp", ""),
            ]);
            let db = infer_link_targets(dir.path());
            assert_eq!(db.targets.len(), 2, "{kind}: {:?}", db.warnings);
            let app = db.targets.iter().find(|t| t.name == "//:app").unwrap();
            assert!(app
                .output
                .starts_with(dir.path().canonicalize().unwrap().join(".trace-gn")));
            assert_eq!(app.sources.len(), 1);
            assert_eq!(app.dependencies.len(), 1);
            assert_eq!(db.targets[app.dependencies[0]].name, "//:lib");
        }
    }
    #[test]
    fn imports_variables_conditionals_and_repository_root() {
        let dir = tree(&[
            (
                "bundle.json",
                r#"{"segment":{"destPath":"foundation/test/demo"}}"#,
            ),
            (
                "paths.gni",
                r#"base = "//foundation/test/demo" common = [ "a.cpp" ]"#,
            ),
            (
                "BUILD.gn",
                r#"import("//foundation/test/demo/paths.gni")
                ohos_shared_library("app") {
                    sources = common
                    if (flag) { sources += [ "b.cpp" ] } else { sources += [ "c.cpp" ] }
                    deps = [ "${base}/lib:library" ]
                }"#,
            ),
            (
                "lib/BUILD.gn",
                r#"static_library("library") { sources = [ "lib.cpp" ] }"#,
            ),
            ("a.cpp", ""),
            ("b.cpp", ""),
            ("c.cpp", ""),
            ("lib/lib.cpp", ""),
        ]);
        let db = infer_link_targets(dir.path());
        assert_eq!(db.targets.len(), 2, "{:?}", db.warnings);
        assert_eq!(db.targets[0].sources.len(), 3);
        assert_eq!(db.targets[0].dependencies, vec![1]);
        let again = infer_link_targets(dir.path());
        assert_eq!(format!("{db:?}"), format!("{again:?}"));
    }
    #[test]
    fn uncertainty_never_publishes_partial_images() {
        for body in [
            r#"sources = generated_sources"#,
            r#"sources = [ "a.cpp" ] deps = [ ":missing" ]"#,
            r#"sources = [ "a.cpp" ] deps = [ ":lib(//toolchain:x)" ]"#,
            r#"sources = [ "a.cpp" ] sources -= unknown"#,
            r#"sources = [ "a.cpp" ] foreach(x, things) { sources += x }"#,
        ] {
            let build = format!("shared_library(\"app\") {{ {body} }}");
            let dir = tree(&[("BUILD.gn", &build), ("a.cpp", "")]);
            let db = infer_link_targets(dir.path());
            // Observed, never isolated: the target is exported but keeps
            // whole-tree resolution.
            assert!(db.unscoped_inference, "{body}: {db:?}");
            assert_eq!(db.targets.len(), 1, "{body}: {db:?}");
            assert!(!db.warnings.is_empty());
        }
    }
    #[test]
    fn scope_literals_and_config_only_defaults_keep_ownership_complete() {
        for build in [
            r#"shared_library("app") { sanitize = { cfi = true debug = false } sources = [ "a.cpp" ] }"#,
            r#"set_defaults("shared_library") { configs = [ ":c" ] visibility = [ "*" ] }
            shared_library("app") { sources = [ "a.cpp" ] }"#,
        ] {
            let dir = tree(&[("BUILD.gn", build), ("a.cpp", "")]);
            let db = infer_link_targets(dir.path());
            assert!(!db.unscoped_inference, "{build}: {db:?}");
            assert_eq!(db.targets.len(), 1, "{build}: {db:?}");
            assert_eq!(db.targets[0].sources.len(), 1, "{build}: {db:?}");
        }
    }

    #[test]
    fn variable_target_names_resolve_and_unresolved_ones_keep_the_file() {
        let dir = tree(&[
            (
                "BUILD.gn",
                r#"name = "app"
                shared_library(name) { sources = [ "a.cpp" ] }
                static_library(target_name) { sources = [ "b.cpp" ] }
                static_library("lib") { sources = [ "b.cpp" ] }"#,
            ),
            ("a.cpp", ""),
            ("b.cpp", ""),
        ]);
        let db = infer_link_targets(dir.path());
        let names: Vec<_> = db.targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["//:app", "//:lib"], "{:?}", db.warnings);
        assert!(db.unscoped_inference);
        assert!(
            db.warnings
                .iter()
                .any(|w| w.contains("unresolved static_library target name")),
            "{:?}",
            db.warnings
        );
    }

    #[test]
    fn unknown_imports_labels_and_defaults_cannot_assert_complete_ownership() {
        for prefix in [
            r#"import("//build/defaults.gni")"#,
            r#"import(import_path)"#,
            r#"set_defaults("executable") { deps = [ ":lib" ] }"#,
        ] {
            let build = format!(r#"{prefix} executable("app") {{ sources = ["a.cpp"] }}"#);
            let dir = tree(&[("BUILD.gn", &build), ("a.cpp", "")]);
            let db = infer_link_targets(dir.path());
            assert!(db.unscoped_inference, "{prefix}: {db:?}");
        }
        let dir = tree(&[
            (
                "BUILD.gn",
                r#"executable("app") { sources = ["a.cpp"] deps = ["//project/lib:lib"] }"#,
            ),
            ("a.cpp", ""),
        ]);
        assert!(infer_link_targets(dir.path()).unscoped_inference);
    }

    #[test]
    fn malformed_target_label_is_diagnosed_without_panicking() {
        let dir = tree(&[
            (
                "BUILD.gn",
                r#"shared_library("bad(name") { sources = ["a.cpp"] }"#,
            ),
            ("a.cpp", ""),
        ]);
        let db = infer_link_targets(dir.path());
        assert!(db.unscoped_inference);
        assert!(db.targets.is_empty());
    }
    #[test]
    fn malformed_import_cycles_and_unknown_templates_are_diagnosed() {
        for build in [
            r#"shared_library("bad") { sources = [ "a.cpp" ]"#,
            r#"import("loop.gni") shared_library("bad") { sources = [ "a.cpp" ] }"#,
            r#"custom_library("bad") { sources = [ "a.cpp" ] }"#,
            r#"shared_library("bad") { sources = [ "$unknown/a.cpp" ] }"#,
        ] {
            let dir = tree(&[
                ("BUILD.gn", build),
                ("a.cpp", ""),
                ("loop.gni", r#"import("loop.gni")"#),
            ]);
            let db = infer_link_targets(dir.path());
            assert!(db.unscoped_inference, "{build}: {db:?}");
            assert!(!db.warnings.is_empty(), "{build}");
        }
    }
    #[test]
    fn incomplete_dependencies_propagate_and_names_are_directory_scoped() {
        let dir = tree(&[
            (
                "BUILD.gn",
                r#"shared_library("app") { deps = [ "lib:one" ] }"#,
            ),
            (
                "lib/BUILD.gn",
                r#"static_library("one") { sources = unknown }"#,
            ),
            (
                "a/BUILD.gn",
                r#"static_library("same") { sources = [ "a.cpp" ] }"#,
            ),
            (
                "b/BUILD.gn",
                r#"static_library("same") { sources = [ "b.cpp" ] }"#,
            ),
            ("a/a.cpp", ""),
            ("b/b.cpp", ""),
        ]);
        let db = infer_link_targets(dir.path());
        assert_eq!(
            db.targets
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["//:app", "//a:same", "//b:same", "//lib:one"]
        );
        assert!(db.unscoped_inference);
        assert_eq!(db.warnings.len(), 2);
    }

    #[test]
    fn shared_imports_are_read_once_and_evaluated_per_importer() {
        let dir = tree(&[
            ("common.gni", r#"extra += [ "shared.cpp" ]"#),
            (
                "a/BUILD.gn",
                r#"extra = [ "a.cpp" ] import("//common.gni") import("//common.gni")
                static_library("a") { sources = extra import("//missing.gni") }"#,
            ),
            (
                "b/BUILD.gn",
                r#"static_library("b") { extra = [ "b.cpp" ] import("//common.gni") sources = extra }"#,
            ),
            (
                "c/BUILD.gn",
                r#"import("//missing.gni") static_library("c") { sources = [ "c.cpp" ] }"#,
            ),
            (".gn", ""),
            ("a/a.cpp", ""),
            ("a/shared.cpp", ""),
            ("b/b.cpp", ""),
            ("b/shared.cpp", ""),
            ("c/c.cpp", ""),
        ]);
        IMPORT_LOADS.with(|n| n.set(0));
        let db = infer_link_targets(dir.path());
        // common.gni is read once for three imports; the missing file is
        // never read.
        assert_eq!(IMPORT_LOADS.with(|n| n.get()), 1);
        let root = dir.path().canonicalize().unwrap();
        let names: Vec<_> = db.targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["//a:a", "//b:b", "//c:c"], "{:?}", db.warnings);
        assert_eq!(
            db.targets[1].sources,
            vec![root.join("b/b.cpp"), root.join("b/shared.cpp")]
        );
        assert!(db.unscoped_inference);
        // The missing import is diagnosed at every importer, without
        // discarding the importing file's targets.
        for dir in ["a", "c"] {
            let prefix = format!("GN {}: unresolved import", root.join(dir).display());
            assert!(
                db.warnings.iter().any(|w| w.starts_with(&prefix)),
                "{dir}: {:?}",
                db.warnings
            );
        }
    }
    #[test]
    fn incompleteness_propagates_through_dependency_chains() {
        let dir = tree(&[
            (
                "BUILD.gn",
                r#"shared_library("app") { sources = [ "a.cpp" ] deps = [ ":mid" ] }
                static_library("mid") { sources = [ "a.cpp" ] deps = [ ":leaf", ":ok" ] }
                static_library("leaf") { sources = [ "a.cpp" ] deps = [ ":gone" ] }
                static_library("ok") { sources = [ "a.cpp" ] }"#,
            ),
            ("a.cpp", ""),
        ]);
        let db = infer_link_targets(dir.path());
        let names: Vec<_> = db.targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["//:app", "//:leaf", "//:mid", "//:ok"]);
        assert!(db.unscoped_inference);
        // The missing `:gone` dependency is not recorded; `:mid` keeps both
        // known ones.
        assert!(db.targets[1].dependencies.is_empty());
        assert_eq!(db.targets[2].dependencies, vec![1, 3]);
        assert_eq!(db.warnings.len(), 3, "{:?}", db.warnings);
    }
    #[test]
    fn cycles_terminate_and_quoted_braces_are_not_syntax() {
        let dir = tree(&[
            (
                "BUILD.gn",
                r#"
          # shared_library("ignored") { sources = [ "wrong.cpp" ] }
          shared_library("a}") { sources = [ "a.cpp" ] deps = [ ":b" ] }
          shared_library("b") { sources = [ "b.cpp" ] deps = [ ":a}" ] }
        "#,
            ),
            ("a.cpp", ""),
            ("b.cpp", ""),
        ]);
        let db = infer_link_targets(dir.path());
        assert_eq!(db.targets.len(), 2);
        assert_eq!(db.targets[0].dependencies, vec![1]);
        assert_eq!(db.targets[1].dependencies, vec![0]);
    }
}
