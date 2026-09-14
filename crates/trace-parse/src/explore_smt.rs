//! MaxSMT Bounded Configuration Exploration (#Phase S1).
//!
//! Replaces greedy single-define exploration with MaxSMT optimization over Z3.
//! Formulates multi-variable condition satisfaction, chain mutual exclusion, and
//! line recovery maximization under budget constraint K.

use crate::explore::VariantConfig;
use crate::gn_defines::Candidate;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use trace_preproc::{ArmOutcome, ConditionalChain, MacroSmtEnv};
use z3::ast::{Ast, Bool, BV};
use z3::{Config, Context, Optimize, SatResult};

type ChainId = (PathBuf, u32);

struct SmtEnv<'a, 'ctx> {
    ctx: &'ctx Context,
    base: &'a HashMap<String, (Bool<'ctx>, BV<'ctx>)>,
    cands: &'a HashMap<String, (Bool<'ctx>, BV<'ctx>)>,
}

impl<'ctx> MacroSmtEnv<'ctx> for SmtEnv<'_, 'ctx> {
    fn is_defined(&self, name: &str) -> Bool<'ctx> {
        if let Some((d, _)) = self.base.get(name) {
            d.clone()
        } else if let Some((d, _)) = self.cands.get(name) {
            d.clone()
        } else {
            Bool::from_bool(self.ctx, false)
        }
    }

    fn value(&self, name: &str) -> BV<'ctx> {
        if let Some((_, v)) = self.base.get(name) {
            v.clone()
        } else if let Some((d, v)) = self.cands.get(name) {
            d.ite(v, &BV::from_i64(self.ctx, 0, 64))
        } else {
            BV::from_i64(self.ctx, 0, 64)
        }
    }
}

fn compute_arm_active<'ctx>(
    ctx: &'ctx Context,
    env: &SmtEnv<'_, 'ctx>,
    chain: &ConditionalChain,
    arm_idx: usize,
) -> Bool<'ctx> {
    let mut terms: Vec<Bool<'ctx>> = Vec::with_capacity(arm_idx + 1);
    for (idx, arm) in chain.arms[..=arm_idx].iter().enumerate() {
        let smt_expr = arm.to_smt_expr(ctx, env);
        if idx == arm_idx {
            terms.push(smt_expr);
        } else {
            terms.push(smt_expr.not());
        }
    }
    let term_refs: Vec<&Bool<'ctx>> = terms.iter().collect();
    Bool::and(ctx, &term_refs)
}

fn parse_candidate_int(val_str: &str) -> i64 {
    let trimmed = val_str.trim();
    if trimmed == "true" {
        1
    } else if trimmed == "false" {
        0
    } else {
        trace_preproc::parse_int_literal(trimmed).unwrap_or(1)
    }
}

/// Generate feasible, preprocessing-consistent variants that maximize excluded
/// code line recovery using Z3 MaxSMT (#Phase S1).
///
/// If SMT solving fails or produces no variants, falls back gracefully to the
/// greedy heuristic in [`crate::explore::generate_feasible_variants`].
pub fn generate_feasible_variants_smt(
    conditionals: &[ConditionalChain],
    candidates: &HashMap<String, Vec<Candidate>>,
    base_defines: &BTreeMap<String, String>,
    budget: usize,
) -> (Vec<VariantConfig>, usize) {
    if budget == 0 || candidates.is_empty() || conditionals.is_empty() {
        return (Vec::new(), 0);
    }

    // Collect all macro names referenced in the conditional chains of this TU.
    let mut referenced_macros = HashSet::new();
    for chain in conditionals {
        if chain.include_guard {
            continue;
        }
        for arm in &chain.arms {
            arm.condition_expr()
                .collect_macro_names(&mut referenced_macros);
            for read in &arm.reads {
                referenced_macros.insert(read.name.clone());
            }
        }
    }

    // Filter candidates to those relevant to this TU (sorted by name for bit-reproducible determinism).
    let mut relevant_candidates: BTreeMap<String, Candidate> = BTreeMap::new();
    for name in &referenced_macros {
        if base_defines.contains_key(name) {
            continue;
        }
        if let Some(cands) = candidates.get(name) {
            if let Some(best) = cands.first() {
                relevant_candidates.insert(name.clone(), best.clone());
            }
        }
    }

    if relevant_candidates.is_empty() {
        return (Vec::new(), 0);
    }

    // Initialize Z3 with deterministic settings and strict timeout.
    let mut cfg = Config::new();
    cfg.set_param_value("timeout", "500"); // 500ms timeout per TU
    cfg.set_param_value("model", "true");
    let ctx = Context::new(&cfg);

    // Build base define ASTs.
    let mut base_map = HashMap::new();
    for (name, val_str) in base_defines {
        let d = Bool::from_bool(&ctx, true);
        let val_int = parse_candidate_int(val_str);
        let v = BV::from_i64(&ctx, val_int, 64);
        base_map.insert(name.clone(), (d, v));
    }

    // Build candidate define ASTs.
    let mut cand_map = HashMap::new();
    for name in relevant_candidates.keys() {
        let d = Bool::new_const(&ctx, format!("def_{name}"));
        let v = BV::new_const(&ctx, format!("val_{name}"), 64);
        cand_map.insert(name.clone(), (d, v));
    }

    let env = SmtEnv {
        ctx: &ctx,
        base: &base_map,
        cands: &cand_map,
    };

    struct ExcludedArm<'ctx> {
        chain_id: ChainId,
        arm_idx: usize,
        active: Bool<'ctx>,
        body_lines: u32,
        enclosed: bool,
    }

    let mut covered_arms: HashSet<(ChainId, usize)> = HashSet::new();
    let mut excluded_arms: Vec<ExcludedArm<'_>> = Vec::new();

    for chain in conditionals {
        if chain.include_guard {
            continue;
        }
        let chain_id = (chain.file.clone(), chain.line());
        for (idx, arm) in chain.arms.iter().enumerate() {
            if arm.outcome == ArmOutcome::Taken {
                covered_arms.insert((chain_id.clone(), idx));
            } else {
                let active = compute_arm_active(&ctx, &env, chain, idx);
                let enclosed = arm.outcome == ArmOutcome::Unevaluated;
                excluded_arms.push(ExcludedArm {
                    chain_id: chain_id.clone(),
                    arm_idx: idx,
                    active,
                    body_lines: arm.body_lines(),
                    enclosed,
                });
            }
        }
    }

    let mut variants: Vec<VariantConfig> = Vec::new();

    while variants.len() < budget {
        let opt = Optimize::new(&ctx);

        // Hard constraints: candidate value consistency
        for (name, cand) in &relevant_candidates {
            if let Some((d_var, v_var)) = cand_map.get(name) {
                let cand_val = cand.value.as_deref().unwrap_or("1");
                let expected_int = parse_candidate_int(cand_val);
                let val_ast = BV::from_i64(&ctx, expected_int, 64);
                let zero_ast = BV::from_i64(&ctx, 0, 64);

                // D_m => V_m == expected_int
                opt.assert(&Bool::or(&ctx, &[&d_var.not(), &v_var._eq(&val_ast)]));
                // !D_m => V_m == 0
                opt.assert(&Bool::or(&ctx, &[&d_var.clone(), &v_var._eq(&zero_ast)]));
            }
        }

        // Soft constraints: maximize newly activated lines
        let mut added_any_soft = false;
        for arm in &excluded_arms {
            let key = (arm.chain_id.clone(), arm.arm_idx);
            if covered_arms.contains(&key) {
                continue;
            }
            let base_weight = if arm.enclosed {
                (arm.body_lines as u64).max(1)
            } else {
                (arm.body_lines as u64).saturating_mul(100).max(10)
            };
            opt.assert_soft(&arm.active, base_weight, None);
            added_any_soft = true;
        }

        if !added_any_soft {
            break;
        }

        // Parsimony soft constraints: prefer leaving candidates undefined (alphabetical order for bit-reproducible tie-breaking)
        for name in relevant_candidates.keys() {
            if let Some((d_var, _)) = cand_map.get(name) {
                opt.assert_soft(&d_var.not(), 1, None);
            }
        }

        match opt.check(&[]) {
            SatResult::Sat => {
                let Some(model) = opt.get_model() else {
                    break;
                };

                let mut var_defines = Vec::new();
                for (name, cand) in &relevant_candidates {
                    if let Some((d_var, _)) = cand_map.get(name) {
                        if model
                            .eval(d_var, true)
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false)
                        {
                            let val = cand.value.clone().unwrap_or_else(|| "1".to_string());
                            var_defines.push((name.clone(), val));
                        }
                    }
                }

                let mut newly_activated = Vec::new();
                for arm in &excluded_arms {
                    let key = (arm.chain_id.clone(), arm.arm_idx);
                    if covered_arms.contains(&key) {
                        continue;
                    }
                    if model
                        .eval(&arm.active, true)
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false)
                    {
                        newly_activated.push(key);
                    }
                }

                if var_defines.is_empty()
                    || newly_activated.is_empty()
                    || variants.iter().any(|v| v.defines == var_defines)
                {
                    break;
                }

                for arm in &newly_activated {
                    covered_arms.insert(arm.clone());
                }

                variants.push(VariantConfig {
                    defines: var_defines,
                    target_chains: newly_activated,
                });
            }
            _ => break,
        }
    }

    // Graceful fallback if SMT found no variants
    if variants.is_empty() {
        return crate::explore::generate_feasible_variants(
            conditionals,
            candidates,
            base_defines,
            budget,
        );
    }

    let unplaced_count = excluded_arms
        .iter()
        .filter(|arm| !covered_arms.contains(&(arm.chain_id.clone(), arm.arm_idx)))
        .count();
    let truncated = if variants.len() >= budget {
        unplaced_count
    } else {
        0
    };

    (variants, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gn_defines::Confidence;
    use trace_preproc::{ArmDirective, ConditionRead, ConditionalArm};

    fn make_arm(
        directive: ArmDirective,
        expression: &str,
        line: u32,
        end_line: u32,
        outcome: ArmOutcome,
        reads: &[&str],
    ) -> ConditionalArm {
        ConditionalArm {
            directive,
            expression: expression.into(),
            line,
            end_line,
            outcome,
            evaluated: outcome != ArmOutcome::Unevaluated,
            reads: reads
                .iter()
                .map(|&r| ConditionRead {
                    name: r.into(),
                    bound: None,
                })
                .collect(),
        }
    }

    fn make_chain(file: &str, arms: Vec<ConditionalArm>) -> ConditionalChain {
        ConditionalChain {
            file: PathBuf::from(file),
            arms,
            depth: 0,
            terminated: true,
            include_guard: false,
        }
    }

    #[test]
    fn smt_activates_multi_variable_condition() {
        // #if defined(CONFIG_A) && (LEVEL >= 2 || !defined(MINIMAL))
        // Arms: 0 is excluded (Skipped), lines = 50
        let chain = make_chain(
            "driver.c",
            vec![make_arm(
                ArmDirective::If,
                "defined(CONFIG_A) && (LEVEL >= 2 || !defined(MINIMAL))",
                10,
                61,
                ArmOutcome::Skipped,
                &["CONFIG_A", "LEVEL", "MINIMAL"],
            )],
        );

        let mut candidates = HashMap::new();
        candidates.insert(
            "CONFIG_A".to_string(),
            vec![Candidate {
                name: "CONFIG_A".into(),
                value: Some("1".into()),
                line: 1,
                conditions: Vec::new(),
                confidence: Confidence::High,
            }],
        );
        candidates.insert(
            "LEVEL".to_string(),
            vec![Candidate {
                name: "LEVEL".into(),
                value: Some("2".into()),
                line: 2,
                conditions: Vec::new(),
                confidence: Confidence::High,
            }],
        );

        let mut base_defines = BTreeMap::new();
        base_defines.insert("MINIMAL".to_string(), "1".to_string());

        let (variants, _) = generate_feasible_variants_smt(&[chain], &candidates, &base_defines, 4);
        assert_eq!(variants.len(), 1);
        let defines: HashMap<_, _> = variants[0].defines.iter().cloned().collect();
        assert_eq!(defines.get("CONFIG_A"), Some(&"1".to_string()));
        assert_eq!(defines.get("LEVEL"), Some(&"2".to_string()));
    }

    #[test]
    fn smt_separates_mutually_exclusive_arms_across_variants() {
        // #ifdef ARCH_A
        //   ... 10 lines
        // #elif defined(ARCH_B)
        //   ... 10 lines
        // #endif
        let chain = make_chain(
            "arch.h",
            vec![
                make_arm(
                    ArmDirective::Ifdef,
                    "ARCH_A",
                    1,
                    12,
                    ArmOutcome::Skipped,
                    &["ARCH_A"],
                ),
                make_arm(
                    ArmDirective::Elif,
                    "defined(ARCH_B)",
                    12,
                    23,
                    ArmOutcome::Skipped,
                    &["ARCH_B"],
                ),
            ],
        );

        let mut candidates = HashMap::new();
        candidates.insert(
            "ARCH_A".to_string(),
            vec![Candidate {
                name: "ARCH_A".into(),
                value: Some("1".into()),
                line: 1,
                conditions: Vec::new(),
                confidence: Confidence::High,
            }],
        );
        candidates.insert(
            "ARCH_B".to_string(),
            vec![Candidate {
                name: "ARCH_B".into(),
                value: Some("1".into()),
                line: 2,
                conditions: Vec::new(),
                confidence: Confidence::High,
            }],
        );

        let (variants, _) =
            generate_feasible_variants_smt(&[chain], &candidates, &BTreeMap::new(), 4);
        assert_eq!(variants.len(), 2);
        // First variant has one define, second variant has the other
        let v1_defs: HashSet<_> = variants[0]
            .defines
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        let v2_defs: HashSet<_> = variants[1]
            .defines
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert!(
            (v1_defs.contains("ARCH_A") && v2_defs.contains("ARCH_B"))
                || (v1_defs.contains("ARCH_B") && v2_defs.contains("ARCH_A"))
        );
    }

    #[test]
    fn smt_activates_kernel_version_comparison() {
        // #if LINUX_VERSION_CODE < KERNEL_VERSION(6, 6, 0)
        let chain = make_chain(
            "compat.c",
            vec![make_arm(
                ArmDirective::If,
                "defined(LINUX_VERSION_CODE) && LINUX_VERSION_CODE < KERNEL_VERSION(6, 6, 0)",
                1,
                45,
                ArmOutcome::Skipped,
                &["LINUX_VERSION_CODE"],
            )],
        );

        let mut candidates = HashMap::new();
        // 6.1.0 = (6 << 16) + (1 << 8) = 393472 < 394752 (6.6.0)
        candidates.insert(
            "LINUX_VERSION_CODE".to_string(),
            vec![Candidate {
                name: "LINUX_VERSION_CODE".into(),
                value: Some("393472".into()),
                line: 1,
                conditions: Vec::new(),
                confidence: Confidence::High,
            }],
        );

        let (variants, _) =
            generate_feasible_variants_smt(&[chain], &candidates, &BTreeMap::new(), 4);
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].defines[0].0, "LINUX_VERSION_CODE");
        assert_eq!(variants[0].defines[0].1, "393472");
    }

    #[test]
    fn smt_determinism_across_runs() {
        let chain = make_chain(
            "test.c",
            vec![make_arm(
                ArmDirective::Ifdef,
                "FOO",
                1,
                20,
                ArmOutcome::Skipped,
                &["FOO"],
            )],
        );
        let mut candidates = HashMap::new();
        candidates.insert(
            "FOO".to_string(),
            vec![Candidate {
                name: "FOO".into(),
                value: Some("1".into()),
                line: 1,
                conditions: Vec::new(),
                confidence: Confidence::High,
            }],
        );

        let res1 =
            generate_feasible_variants_smt(std::slice::from_ref(&chain), &candidates, &BTreeMap::new(), 4);
        let res2 =
            generate_feasible_variants_smt(std::slice::from_ref(&chain), &candidates, &BTreeMap::new(), 4);
        let res3 = generate_feasible_variants_smt(&[chain], &candidates, &BTreeMap::new(), 4);

        assert_eq!(res1, res2);
        assert_eq!(res2, res3);
    }
}
