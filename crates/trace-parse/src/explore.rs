//! Bounded configuration-variant exploration (#59).
//!
//! Preprocesses and lowers feasible variants separately, then unions their
//! facts. Feasible variants are preprocessing-consistent combinations of
//! candidate defines from in-tree build evidence (#58) that activate conditional
//! code regions excluded by the default configuration (#57).

use crate::gn_defines::{self, Candidate, Confidence};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use trace_preproc::{ArmDirective, ArmOutcome, ConditionalChain, PreprocessOptions};
use walkdir::WalkDir;

/// Identity of a conditional chain within a translation unit.
///
/// A translation unit spans many files, so the source line alone is not
/// unique: two unrelated chains routinely start on the same line of two
/// different headers.
type ChainId = (PathBuf, u32);

/// Memoized `#if` / `#elif` activation verdicts, keyed by
/// (condition expression, macro name, macro value).
type ActivationCache = HashMap<(String, String, String), bool>;

/// A configuration variant to explore for a translation unit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VariantConfig {
    /// Macro definitions for this variant (NAME, VALUE).
    pub defines: Vec<(String, String)>,
    /// The conditional arms targeted by this variant, as ((file, line), arm).
    pub target_chains: Vec<(ChainId, usize)>,
}

impl VariantConfig {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn add_goal(&mut self, goal: &ExplorationGoal, index: &VariantIndex) {
        // `index` already answers what these vectors would have to be scanned
        // for, which is what keeps placement from costing more the fuller a
        // variant gets.
        if !index.defines.contains_key(&goal.name) {
            self.defines.push((goal.name.clone(), goal.value.clone()));
        }
        if !index.arms.contains_key(&goal.chain) {
            self.target_chains.push((goal.chain.clone(), goal.arm_idx));
        }
    }
}

/// Search-side index over one [`VariantConfig`].
///
/// Kept beside the variant rather than inside it: the config describes a
/// configuration to preprocess, while this is the bookkeeping that decides
/// where a goal can go. Every question the placement loop asks — does this
/// variant already fix this macro, does it already target this chain, can this
/// define reach an arm it secured — is a hash lookup here, where it used to be
/// a scan over everything placed so far (#59 review).
#[derive(Default)]
struct VariantIndex {
    /// Macro name → the value this variant binds it to.
    defines: HashMap<String, String>,
    /// Chain → the single arm this variant targets in it.
    arms: HashMap<ChainId, usize>,
    /// Every name a secured arm could depend on.
    deps: HashSet<String>,
}

impl VariantIndex {
    /// Whether a candidate goal is consistent with this variant.
    fn accepts(&self, goal: &ExplorationGoal) -> bool {
        // Different arms of the SAME chain are mutually exclusive: one run
        // takes at most one of them. Two defines that open the *same* arm
        // (`#if defined(A) || defined(B)`) are not in conflict, and keeping
        // both in one variant also covers whatever each guards elsewhere.
        if self
            .arms
            .get(&goal.chain)
            .is_some_and(|arm| *arm != goal.arm_idx)
        {
            return false;
        }
        // Inconsistent values for the same macro cannot agree.
        !self
            .defines
            .get(&goal.name)
            .is_some_and(|value| value != &goal.value)
    }

    fn record(&mut self, goal: &ExplorationGoal) {
        self.defines
            .entry(goal.name.clone())
            .or_insert_with(|| goal.value.clone());
        self.arms.entry(goal.chain.clone()).or_insert(goal.arm_idx);
    }
}

#[derive(Debug, Clone)]
struct ExplorationGoal {
    name: String,
    value: String,
    confidence: Confidence,
    chain: ChainId,
    arm_idx: usize,
    body_lines: u32,
    /// The arm is inside a region an enclosing chain excluded, so this define
    /// alone cannot open it. Explored last, if budget remains.
    enclosed: bool,
}

/// An arm is reachable only when its condition holds and every earlier arm
/// in the same chain is false.
struct ArmCondition {
    expression: String,
}

impl ArmCondition {
    fn from_chain(chain: &ConditionalChain, arm_idx: usize) -> Self {
        let mut terms = Vec::new();
        for (idx, arm) in chain.arms[..=arm_idx].iter().enumerate() {
            let expr = match arm.directive {
                ArmDirective::Ifdef => format!("defined({})", arm.expression.trim()),
                ArmDirective::Ifndef => format!("!defined({})", arm.expression.trim()),
                ArmDirective::Else => "1".into(),
                ArmDirective::If | ArmDirective::Elif => arm.expression.clone(),
            };
            terms.push(if idx == arm_idx {
                format!("({expr})")
            } else {
                format!("!({expr})")
            });
        }
        Self {
            expression: terms.join(" && "),
        }
    }
}

/// Discover candidate defines from GN build files (`BUILD.gn`, `*.gni`, `*.gn`)
/// under the project root.
pub fn scan_project_gn_candidates(root: &Path) -> HashMap<String, Vec<Candidate>> {
    let mut candidates_by_name: HashMap<String, Vec<(PathBuf, Candidate)>> = HashMap::new();
    // Sorted: candidate ordering feeds variant selection, which must not
    // depend on filesystem `read_dir` order.
    let gn_files: Vec<PathBuf> = WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            (e.file_type().is_file() && e.file_name() == ".gn") || scan_entry_name(e.file_name())
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && is_gn_file(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect();

    for path in gn_files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for mut cand in gn_defines::scan(&text) {
            // Default empty or bare define to "1".
            if cand.value.as_deref().unwrap_or("").is_empty() {
                cand.value = Some("1".to_string());
            }
            candidates_by_name
                .entry(cand.name.clone())
                .or_default()
                .push((path.clone(), cand));
        }
    }

    for cands in candidates_by_name.values_mut() {
        // (confidence, line) alone ties for the same macro defined on the same
        // line of two different GN files; the path breaks the tie so the
        // winning value does not depend on the filesystem.
        cands.sort_by(|(a_path, a), (b_path, b)| {
            b.confidence
                .cmp(&a.confidence)
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a_path.cmp(b_path))
        });
    }

    candidates_by_name
        .into_iter()
        .map(|(name, cands)| (name, cands.into_iter().map(|(_, c)| c).collect()))
        .collect()
}

fn scan_entry_name(name: &std::ffi::OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    !bytes.starts_with(b".") && bytes != b"target"
}

fn is_gn_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "BUILD.gn" || n.ends_with(".gni") || n.ends_with(".gn"))
}

/// Generate feasible, preprocessing-consistent variants that activate excluded
/// code arms in `conditionals`.
///
/// Returns the generated variants (capped at `budget`) and the count of
/// candidate activation goals that could not be placed within the budget.
pub fn generate_feasible_variants(
    conditionals: &[ConditionalChain],
    candidates: &HashMap<String, Vec<Candidate>>,
    base_defines: &BTreeMap<String, String>,
    budget: usize,
) -> (Vec<VariantConfig>, usize) {
    if budget == 0 || candidates.is_empty() {
        return (Vec::new(), 0);
    }

    let mut goals: Vec<ExplorationGoal> = Vec::new();
    let mut seen_goals: HashSet<(String, String, ChainId, usize)> = HashSet::new();
    let mut activation_cache = ActivationCache::new();
    let mut conditions = HashMap::new();

    for chain in conditionals {
        if chain.include_guard {
            continue;
        }
        let chain_id: ChainId = (chain.file.clone(), chain.line());
        for (arm_idx, arm) in chain.arms.iter().enumerate() {
            if arm.outcome == ArmOutcome::Taken {
                continue;
            }
            let condition = ArmCondition::from_chain(chain, arm_idx);
            // `Unevaluated` means the chain sits inside a region excluded by an
            // enclosing chain (C11 6.10.1p6). Defining this arm's macro cannot
            // open it on its own, and exploration does not couple the two
            // chains — so such a goal is demoted behind every arm the define
            // really does open, and spends only budget nothing else claimed.
            let enclosed = arm.outcome == ArmOutcome::Unevaluated;
            // An arm not taken in the base configuration: check which candidate
            // define(s) can activate it.
            //
            // The macros to try come from the whole chain prefix, not from this
            // arm alone. An `#else` arm reads nothing — it is opened by
            // *falsifying* the arms above it — so drawing candidates from
            // `arm.reads` would leave the `#ifndef FOO / #else` shape, one of
            // the most common ways an alternate implementation is spelled,
            // permanently unexplored. A macro pulled in from an earlier arm is
            // no less filtered for it: `condition.expression` negates every
            // preceding arm, so a candidate that would keep an earlier arm true
            // fails the activation check below.
            let mut seen_reads = HashSet::new();
            for read in chain.arms[..=arm_idx].iter().flat_map(|a| &a.reads) {
                if !seen_reads.insert(&read.name) {
                    continue;
                }
                if let Some(cands) = candidates.get(&read.name) {
                    for cand in cands {
                        let val = cand.value.as_deref().unwrap_or("1").to_string();
                        // The base configuration already decides this macro:
                        // a different value conflicts, and the same value is a
                        // no-op that would re-lower the base as a "variant".
                        if base_defines.contains_key(&cand.name) {
                            continue;
                        }
                        let key = (cand.name.clone(), val.clone(), chain_id.clone(), arm_idx);
                        if seen_goals.contains(&key) {
                            continue;
                        }
                        if is_activating_define(
                            &condition.expression,
                            &cand.name,
                            &val,
                            base_defines,
                            &mut activation_cache,
                        ) && seen_goals.insert(key)
                        {
                            goals.push(ExplorationGoal {
                                name: cand.name.clone(),
                                value: val,
                                confidence: cand.confidence,
                                chain: chain_id.clone(),
                                arm_idx,
                                body_lines: arm.body_lines(),
                                enclosed,
                            });
                        }
                    }
                }
            }
            conditions.insert((chain_id.clone(), arm_idx), condition);
        }
    }

    if goals.is_empty() {
        return (Vec::new(), 0);
    }

    // Arms the define actually opens first, then high confidence, then largest
    // body lines, then chain.
    goals.sort_by(|a, b| {
        a.enclosed
            .cmp(&b.enclosed)
            .then_with(|| b.confidence.cmp(&a.confidence))
            .then_with(|| b.body_lines.cmp(&a.body_lines))
            .then_with(|| a.chain.cmp(&b.chain))
            .then_with(|| a.arm_idx.cmp(&b.arm_idx))
    });

    let mut variants: Vec<VariantConfig> = Vec::new();
    let mut indexes: Vec<VariantIndex> = Vec::new();
    let mut truncated = 0usize;

    for goal in goals {
        let mut placed = false;
        for (var, index) in variants.iter_mut().zip(indexes.iter_mut()) {
            if index.accepts(&goal)
                && preserves_targets(var, index, &goal, &conditions, base_defines)
            {
                extend_dependencies(index, &goal, &conditions, base_defines);
                var.add_goal(&goal, index);
                index.record(&goal);
                placed = true;
                break;
            }
        }
        if !placed {
            if variants.len() < budget {
                let mut var = VariantConfig::new();
                let mut index = VariantIndex::default();
                extend_dependencies(&mut index, &goal, &conditions, base_defines);
                var.add_goal(&goal, &index);
                index.record(&goal);
                variants.push(var);
                indexes.push(index);
            } else {
                truncated += 1;
            }
        }
    }

    (variants, truncated)
}

/// Identifiers in a condition expression, in source order. Numeric literals are
/// not identifiers and no macro can be named after one.
fn identifiers(expression: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in expression.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out.retain(|name| !name.starts_with(|c: char| c.is_ascii_digit()));
    out
}

/// Every macro name whose definition could change what `expression` evaluates
/// to: the identifiers it reads, plus everything reachable through the
/// replacement lists of the macros already in force.
///
/// This is what makes an added define's effect decidable without re-running the
/// preprocessor: a name outside this set cannot reach the expression, through an
/// alias or otherwise. The set over-approximates — an identifier that only ever
/// appears inside a `defined()` whose verdict is already fixed still joins it —
/// so a miss is a proof and a hit only costs one extra evaluation.
fn condition_dependencies(
    expression: &str,
    base_defines: &BTreeMap<String, String>,
    variant_defines: &HashMap<String, String>,
    pending: Option<(&str, &str)>,
) -> HashSet<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = identifiers(expression);
    while let Some(name) = queue.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let body = pending
            .filter(|(pending_name, _)| *pending_name == name)
            .map(|(_, value)| value)
            .or_else(|| variant_defines.get(&name).map(String::as_str))
            .or_else(|| base_defines.get(&name).map(String::as_str));
        if let Some(body) = body {
            queue.extend(identifiers(body));
        }
    }
    seen
}

/// Fold the arm a goal secures — and the value it binds, which a later define
/// can alias into — into the variant's dependency set.
fn extend_dependencies(
    index: &mut VariantIndex,
    goal: &ExplorationGoal,
    conditions: &HashMap<(ChainId, usize), ArmCondition>,
    base_defines: &BTreeMap<String, String>,
) {
    let target = (goal.chain.clone(), goal.arm_idx);
    let pending = (goal.name.as_str(), goal.value.as_str());
    let mut names = condition_dependencies(
        &conditions[&target].expression,
        base_defines,
        &index.defines,
        Some(pending),
    );
    names.extend(condition_dependencies(
        &goal.value,
        base_defines,
        &index.defines,
        Some(pending),
    ));
    index.deps.extend(names);
}

/// Evaluate one arm's condition with `goal` bound, in the macro environment
/// `index` describes.
///
/// Only the names the expression can actually reach are bound. A variant can
/// carry thousands of defines, and materializing all of them for every
/// placement is quadratic on its own — the closure is exactly the set that can
/// change the verdict, so the shorter environment decides the same way.
fn arm_holds(
    expression: &str,
    goal: &ExplorationGoal,
    index: &VariantIndex,
    base_defines: &BTreeMap<String, String>,
) -> bool {
    let mut opts = PreprocessOptions::new();
    opts.track_line_map = false;
    // The goal's own binding takes part in the walk: a GN entry can bind a
    // candidate to another macro's name (`X=FOO`), and the expression then
    // expands through whatever defines `FOO`. Leaving it out of the closure
    // left that macro unbound and judged the arm closed (#59 cloud review).
    let pending = (goal.name.as_str(), goal.value.as_str());
    for name in condition_dependencies(expression, base_defines, &index.defines, Some(pending)) {
        if name == goal.name {
            continue;
        }
        if let Some(value) = index.defines.get(&name).or_else(|| base_defines.get(&name)) {
            opts.defines.insert(name, value.clone());
        }
    }
    opts = opts.with_define(&goal.name, &goal.value);
    evaluate_condition(expression, &opts)
}

/// Whether adding a goal to a variant opens its arm without closing one the
/// variant already secured.
///
/// Each arm is evaluated in the full macro environment, so an alias — a base
/// define that expands to a newly added candidate — is decided correctly. Which
/// arms need re-evaluating is decided from `deps`: a define the secured arms
/// cannot reach leaves every one of them at the value it already had. Checking
/// them anyway made a variant's cost grow with the arms already in it, so a
/// translation unit whose configuration header offers thousands of candidate
/// arms spent quadratic time in the search alone (#59 review).
fn preserves_targets(
    variant: &VariantConfig,
    index: &VariantIndex,
    goal: &ExplorationGoal,
    conditions: &HashMap<(ChainId, usize), ArmCondition>,
    base_defines: &BTreeMap<String, String>,
) -> bool {
    let target = (goal.chain.clone(), goal.arm_idx);
    let already_defined = index.defines.contains_key(&goal.name);
    if already_defined && index.arms.get(&goal.chain) == Some(&goal.arm_idx) {
        return true;
    }
    // The goal's own arm has to open under the combined definitions.
    if !arm_holds(&conditions[&target].expression, goal, index, base_defines) {
        return false;
    }
    // Nothing new is bound, so no secured arm can have moved.
    if already_defined {
        return true;
    }
    if !index.deps.contains(&goal.name) {
        return true;
    }
    variant.target_chains.iter().all(|existing| {
        existing == &target
            || arm_holds(&conditions[existing].expression, goal, index, base_defines)
    })
}

/// Cache singleton feasibility within a TU; base defines are fixed for this
/// search. Header-provided macro bindings are checked by the real variant run.
fn is_activating_define(
    expression: &str,
    name: &str,
    value: &str,
    base_defines: &BTreeMap<String, String>,
    cache: &mut ActivationCache,
) -> bool {
    let key = (expression.to_string(), name.to_string(), value.to_string());
    *cache.entry(key).or_insert_with(|| {
        let mut opts = PreprocessOptions::new();
        opts.track_line_map = false;
        opts.defines.extend(
            base_defines
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
        opts = opts.with_define(name, value);
        evaluate_condition(expression, &opts)
    })
}

fn evaluate_condition(expression: &str, opts: &PreprocessOptions) -> bool {
    let code = format!("#if {expression}\n1\n#endif\n");
    let result = trace_preproc::preprocess_string(&code, Path::new("<eval>.c"), opts);
    result.output.lines().any(|line| line.trim() == "1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use trace_preproc::{ArmDirective, ArmOutcome, ConditionRead, ConditionalArm};

    #[cfg(unix)]
    #[test]
    fn candidate_filter_preserves_non_utf8_directory_names() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        assert!(scan_entry_name(OsStr::from_bytes(b"platform_\xff")));
        assert!(!scan_entry_name(OsStr::from_bytes(b".hidden_\xff")));
        assert!(!scan_entry_name(OsStr::new("target")));
    }

    #[test]
    fn candidate_scan_normalizes_bare_and_empty_defines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("BUILD.gn"),
            r#"defines = [ "BARE", "EMPTY=", "VALUE=2" ]"#,
        )
        .unwrap();
        let candidates = scan_project_gn_candidates(dir.path());
        for (name, value) in [("BARE", "1"), ("EMPTY", "1"), ("VALUE", "2")] {
            assert_eq!(candidates[name][0].value.as_deref(), Some(value), "{name}");
        }
    }

    #[test]
    fn candidate_scan_includes_dot_gn_but_skips_hidden_and_target_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gn"), r#"defines = [ "ROOT" ]"#).unwrap();
        for directory in [".hidden", "target", "nested/.gn"] {
            std::fs::create_dir_all(dir.path().join(directory)).unwrap();
            std::fs::write(
                dir.path().join(directory).join("BUILD.gn"),
                r#"defines = [ "IGNORED" ]"#,
            )
            .unwrap();
        }
        let candidates = scan_project_gn_candidates(dir.path());
        assert!(candidates.contains_key("ROOT"));
        assert!(!candidates.contains_key("IGNORED"));
    }

    fn ifdef_chain(file: &str, line: u32, macro_name: &str) -> ConditionalChain {
        ConditionalChain {
            file: PathBuf::from(file),
            arms: vec![ConditionalArm {
                directive: ArmDirective::Ifdef,
                expression: macro_name.into(),
                line,
                end_line: line + 5,
                outcome: ArmOutcome::Skipped,
                evaluated: true,
                reads: vec![ConditionRead {
                    name: macro_name.into(),
                    bound: Some(false),
                }],
            }],
            depth: 0,
            terminated: true,
            include_guard: false,
        }
    }

    #[test]
    fn condition_dependencies_follow_macro_aliases() {
        let mut base = BTreeMap::new();
        // `MODE` is bound to another name, so defining *that* name decides the
        // condition even though it never appears in the expression.
        base.insert("MODE".to_string(), "FEATURE_LEVEL".to_string());
        base.insert("FEATURE_LEVEL".to_string(), "TIER".to_string());

        let deps = condition_dependencies("MODE == 1", &base, &HashMap::new(), None);
        assert!(deps.contains("MODE"));
        assert!(
            deps.contains("FEATURE_LEVEL"),
            "an alias one hop away must be reachable: {deps:?}"
        );
        assert!(
            deps.contains("TIER"),
            "alias chains are followed transitively: {deps:?}"
        );
        assert!(
            !deps.contains("UNRELATED"),
            "a name the expression cannot reach stays out, which is what lets \
             the search skip re-evaluating this arm: {deps:?}"
        );
        assert!(
            !deps.iter().any(|d| d == "1"),
            "numeric literals are not names"
        );
    }

    #[test]
    fn condition_dependencies_see_variant_bindings() {
        let base = BTreeMap::new();
        let variant: HashMap<String, String> = [("MODE".to_string(), "LATE_ALIAS".to_string())]
            .into_iter()
            .collect();
        let deps = condition_dependencies("defined(MODE)", &base, &variant, None);
        assert!(
            deps.contains("LATE_ALIAS"),
            "a binding the variant added is in force too: {deps:?}"
        );
    }

    fn high(name: &str) -> Vec<Candidate> {
        vec![Candidate {
            name: name.into(),
            value: None,
            line: 1,
            conditions: vec![],
            confidence: Confidence::High,
        }]
    }

    #[test]
    fn alternative_defines_for_one_arm_do_not_split_variants() {
        // `#if defined(A) || defined(B)` with both A and B as candidates: one
        // arm, so one activation — not two variants lowering the same region.
        let chain = ConditionalChain {
            file: PathBuf::from("a.c"),
            arms: vec![ConditionalArm {
                directive: ArmDirective::If,
                expression: "defined(FEATURE_A) || defined(FEATURE_B)".into(),
                line: 10,
                end_line: 20,
                outcome: ArmOutcome::Skipped,
                evaluated: true,
                reads: vec![
                    ConditionRead {
                        name: "FEATURE_A".into(),
                        bound: Some(false),
                    },
                    ConditionRead {
                        name: "FEATURE_B".into(),
                        bound: Some(false),
                    },
                ],
            }],
            depth: 0,
            terminated: true,
            include_guard: false,
        };
        let mut candidates = HashMap::new();
        candidates.insert("FEATURE_A".to_string(), high("FEATURE_A"));
        candidates.insert("FEATURE_B".to_string(), high("FEATURE_B"));

        let (variants, truncated) =
            generate_feasible_variants(&[chain], &candidates, &BTreeMap::new(), 4);
        assert_eq!(truncated, 0);
        // One variant, carrying both defines: neither is dropped, so code
        // either of them guards elsewhere in the unit is still reached.
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].defines.len(), 2);
    }

    fn if_chain(file: &str, line: u32, expr: &str, macro_name: &str) -> ConditionalChain {
        ConditionalChain {
            file: PathBuf::from(file),
            arms: vec![ConditionalArm {
                directive: ArmDirective::If,
                expression: expr.into(),
                line,
                end_line: line + 5,
                outcome: ArmOutcome::Skipped,
                evaluated: true,
                reads: vec![ConditionRead {
                    name: macro_name.into(),
                    bound: Some(false),
                }],
            }],
            depth: 0,
            terminated: true,
            include_guard: false,
        }
    }

    #[test]
    fn a_goal_whose_value_aliases_a_base_macro_packs_into_a_variant() {
        // A GN entry can bind a candidate to another macro's *name*
        // (`X=FOO`), which a base header then defines. Deciding whether the
        // arm opens has to expand through that binding: judging it closed
        // burns a budget slot on a second variant, or drops the arm outright
        // once the budget is gone (#59 cloud review).
        // The aliased arm sorts *second*, so it has to pack into the variant
        // the first goal opened — which is the path that expands the binding.
        let plain = if_chain("a.c", 10, "defined(Y)", "Y");
        let aliased = if_chain("a.c", 40, "X == 5", "X");

        let mut candidates = HashMap::new();
        candidates.insert(
            "X".to_string(),
            vec![Candidate {
                name: "X".into(),
                value: Some("FOO".into()),
                line: 1,
                conditions: vec![],
                confidence: Confidence::High,
            }],
        );
        candidates.insert("Y".to_string(), high("Y"));

        let mut base = BTreeMap::new();
        base.insert("FOO".to_string(), "5".to_string());

        // One slot: the two goals do not conflict, so both belong in it.
        let (variants, truncated) =
            generate_feasible_variants(&[plain, aliased], &candidates, &base, 1);
        assert_eq!(
            truncated, 0,
            "neither goal conflicts with the other, so nothing should be truncated"
        );
        assert_eq!(variants.len(), 1);
        let names: Vec<&str> = variants[0]
            .defines
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert!(
            names.contains(&"X") && names.contains(&"Y"),
            "both arms pack into the one variant, got {names:?}"
        );
    }

    #[test]
    fn arms_inside_an_excluded_region_are_explored_last() {
        // Two chains reading the same macro at different values, so their
        // goals cannot share a variant. One sits inside a region an enclosing
        // chain excluded (`Unevaluated`), where the define cannot open it; the
        // single budget slot must go to the arm the define really does open.
        let mut enclosed = if_chain("a.c", 10, "MODE == 1", "MODE");
        enclosed.arms[0].outcome = ArmOutcome::Unevaluated;
        enclosed.arms[0].evaluated = false;
        let open = if_chain("a.c", 40, "MODE == 2", "MODE");

        let mut candidates = HashMap::new();
        candidates.insert(
            "MODE".to_string(),
            vec![
                Candidate {
                    name: "MODE".into(),
                    value: Some("1".into()),
                    line: 1,
                    conditions: vec![],
                    confidence: Confidence::High,
                },
                Candidate {
                    name: "MODE".into(),
                    value: Some("2".into()),
                    line: 2,
                    conditions: vec![],
                    confidence: Confidence::High,
                },
            ],
        );

        let (variants, truncated) = generate_feasible_variants(
            &[enclosed.clone(), open.clone()],
            &candidates,
            &BTreeMap::new(),
            1,
        );
        assert_eq!(variants.len(), 1);
        assert_eq!(truncated, 1);
        assert_eq!(
            variants[0].defines,
            vec![("MODE".to_string(), "2".to_string())]
        );

        // With budget to spare it is still explored, rather than discarded.
        let (variants, truncated) =
            generate_feasible_variants(&[enclosed, open], &candidates, &BTreeMap::new(), 4);
        assert_eq!(truncated, 0);
        assert_eq!(variants.len(), 2);
    }

    #[test]
    fn an_else_arm_is_explored_through_the_arm_it_negates() {
        // `#ifndef FOO / #else` is one of the most common ways an alternate
        // implementation is spelled. An `#else` arm reads no macro of its own —
        // it opens when the arm above it goes false — so a search that looks
        // only at the target arm's reads never tries FOO at all.
        let mut chain = ifdef_chain("a.c", 10, "USE_HW_ACCEL");
        chain.arms[0].directive = ArmDirective::Ifndef;
        chain.arms[0].outcome = ArmOutcome::Taken;
        chain.arms.push(ConditionalArm {
            directive: ArmDirective::Else,
            expression: String::new(),
            line: 15,
            end_line: 20,
            outcome: ArmOutcome::Skipped,
            evaluated: false,
            reads: Vec::new(),
        });
        let candidates = [("USE_HW_ACCEL".to_string(), high("USE_HW_ACCEL"))]
            .into_iter()
            .collect();

        let (variants, truncated) =
            generate_feasible_variants(&[chain], &candidates, &BTreeMap::new(), 4);
        assert_eq!(truncated, 0);
        assert_eq!(variants.len(), 1);
        assert_eq!(
            variants[0].defines,
            vec![("USE_HW_ACCEL".to_string(), "1".to_string())]
        );
        assert_eq!(
            variants[0].target_chains,
            vec![((PathBuf::from("a.c"), 10), 1)],
            "the #else arm, not the #ifndef arm the base already took"
        );
    }

    #[test]
    fn a_define_the_base_already_carries_is_not_a_goal() {
        let chain = ifdef_chain("a.c", 10, "FEATURE_A");
        let mut candidates = HashMap::new();
        candidates.insert("FEATURE_A".to_string(), high("FEATURE_A"));
        let base: BTreeMap<String, String> = [("FEATURE_A".to_string(), "1".to_string())]
            .into_iter()
            .collect();

        let (variants, truncated) = generate_feasible_variants(&[chain], &candidates, &base, 4);
        assert_eq!(truncated, 0);
        assert!(variants.is_empty());
    }

    #[test]
    fn same_line_in_different_files_does_not_conflict() {
        // Two independent chains that happen to start on the same source line
        // of two different files. Their defines are compatible, so both goals
        // belong in ONE variant.
        let chains = vec![
            ifdef_chain("a.c", 10, "FEATURE_A"),
            ifdef_chain("b.h", 10, "FEATURE_B"),
        ];
        let mut candidates = HashMap::new();
        candidates.insert("FEATURE_A".to_string(), high("FEATURE_A"));
        candidates.insert("FEATURE_B".to_string(), high("FEATURE_B"));

        let (variants, truncated) =
            generate_feasible_variants(&chains, &candidates, &BTreeMap::new(), 4);
        assert_eq!(truncated, 0);
        assert_eq!(
            variants.len(),
            1,
            "chains in different files must not collide on line number"
        );
    }

    #[test]
    fn earlier_arm_must_be_false_to_activate_elif() {
        let mut chain = if_chain("a.c", 10, "1", "A");
        let mut later = if_chain("a.c", 20, "defined(A)", "A").arms.remove(0);
        later.directive = ArmDirective::Elif;
        chain.arms[0].outcome = ArmOutcome::Taken;
        chain.arms.push(later);
        let candidates = [("A".into(), high("A"))].into_iter().collect();
        let (variants, omitted) =
            generate_feasible_variants(&[chain], &candidates, &BTreeMap::new(), 1);
        assert!(variants.is_empty());
        assert_eq!(omitted, 0);
    }

    #[test]
    fn compatible_goals_merge_into_single_variant() {
        let chains = vec![
            ifdef_chain("a.c", 10, "FEATURE_A"),
            ifdef_chain("a.c", 30, "FEATURE_B"),
        ];
        let mut candidates = HashMap::new();
        candidates.insert("FEATURE_A".to_string(), high("FEATURE_A"));
        candidates.insert("FEATURE_B".to_string(), high("FEATURE_B"));

        let (variants, truncated) =
            generate_feasible_variants(&chains, &candidates, &BTreeMap::new(), 4);
        assert_eq!(truncated, 0);
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].defines.len(), 2);
    }

    #[test]
    fn conflicting_arms_split_into_separate_variants() {
        let chain = ConditionalChain {
            file: PathBuf::from("a.c"),
            arms: vec![
                ConditionalArm {
                    directive: ArmDirective::If,
                    expression: "MODE == 1".into(),
                    line: 10,
                    end_line: 20,
                    outcome: ArmOutcome::Skipped,
                    evaluated: true,
                    reads: vec![ConditionRead {
                        name: "MODE".into(),
                        bound: Some(false),
                    }],
                },
                ConditionalArm {
                    directive: ArmDirective::Elif,
                    expression: "MODE == 2".into(),
                    line: 20,
                    end_line: 30,
                    outcome: ArmOutcome::Skipped,
                    evaluated: false,
                    reads: vec![ConditionRead {
                        name: "MODE".into(),
                        bound: None,
                    }],
                },
            ],
            depth: 0,
            terminated: true,
            include_guard: false,
        };

        let mut candidates = HashMap::new();
        candidates.insert(
            "MODE".into(),
            vec![
                Candidate {
                    name: "MODE".into(),
                    value: Some("1".into()),
                    line: 1,
                    conditions: vec![],
                    confidence: Confidence::High,
                },
                Candidate {
                    name: "MODE".into(),
                    value: Some("2".into()),
                    line: 2,
                    conditions: vec![],
                    confidence: Confidence::High,
                },
            ],
        );

        let (variants, truncated) =
            generate_feasible_variants(&[chain], &candidates, &BTreeMap::new(), 4);
        assert_eq!(truncated, 0);
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].defines, vec![("MODE".into(), "1".into())]);
        assert_eq!(variants[1].defines, vec![("MODE".into(), "2".into())]);
    }

    #[test]
    fn budget_truncation_is_reported() {
        let make_chain = |line: u32, name: &str| ConditionalChain {
            file: PathBuf::from("a.c"),
            arms: vec![
                ConditionalArm {
                    directive: ArmDirective::If,
                    expression: format!("{name} == 1"),
                    line,
                    end_line: line + 5,
                    outcome: ArmOutcome::Skipped,
                    evaluated: true,
                    reads: vec![ConditionRead {
                        name: name.into(),
                        bound: Some(false),
                    }],
                },
                ConditionalArm {
                    directive: ArmDirective::Elif,
                    expression: format!("{name} == 2"),
                    line: line + 5,
                    end_line: line + 10,
                    outcome: ArmOutcome::Skipped,
                    evaluated: false,
                    reads: vec![ConditionRead {
                        name: name.into(),
                        bound: None,
                    }],
                },
            ],
            depth: 0,
            terminated: true,
            include_guard: false,
        };

        let mut candidates = HashMap::new();
        candidates.insert(
            "M1".into(),
            vec![
                Candidate {
                    name: "M1".into(),
                    value: Some("1".into()),
                    line: 1,
                    conditions: vec![],
                    confidence: Confidence::High,
                },
                Candidate {
                    name: "M1".into(),
                    value: Some("2".into()),
                    line: 2,
                    conditions: vec![],
                    confidence: Confidence::High,
                },
            ],
        );

        // With budget 1, one variant is created and the second conflicting arm is truncated.
        let (variants, truncated) =
            generate_feasible_variants(&[make_chain(10, "M1")], &candidates, &BTreeMap::new(), 1);
        assert_eq!(variants.len(), 1);
        assert_eq!(truncated, 1);
    }
}
