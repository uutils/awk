use bumpalo::Bump;
use parser::{FileCache, Parser};

use super::{lower::CodeGen, *};

fn with_lower(source: &str, f: impl FnOnce(&CodeGen<'_>)) {
    let arena = Bump::new();
    let mut parser = Parser::new(&arena, false);
    let ast = parser
        .parse(FileCache(None), source.as_bytes())
        .expect("parse");
    let mut cg = CodeGen::new(&arena);
    cg.lower_ast(ast);
    f(&cg);
}

#[test]
fn switch_lowers_case_comparisons() {
    with_lower(
        "BEGIN { switch (x) { case 1: print; case \"a\": print 2; default: print 3 } }",
        |cg| {
            let bc = format!("{}", cg.bc);
            assert!(bc.contains("eq"), "expected Eq for literal cases:\n{bc}");
            assert!(bc.contains("brif"), "expected case branches:\n{bc}");
            assert!(bc.contains("jmp"), "expected jumps to end of switch:\n{bc}");
        },
    );
}

#[test]
fn switch_lowers_regex_case_with_matches() {
    with_lower("BEGIN { switch (x) { case /pat/: print } }", |cg| {
        let bc = format!("{}", cg.bc);
        assert!(
            bc.contains("mtch"),
            "expected Matches for regex case:\n{bc}"
        );
        assert!(!bc.contains("eq"), "regex case should not use Eq:\n{bc}");
    });
}

fn brif_count(cg: &CodeGen<'_>) -> usize {
    cg.bc
        .code
        .iter()
        .filter(|i| matches!(i, Instruction::Branch { .. }))
        .count()
}

#[test]
fn and_or_use_branches_not_dedicated_ops() {
    with_lower("BEGIN { print (0 && 1); print (1 || 0) }", |cg| {
        let bc = format!("{}", cg.bc);
        assert!(!bc.contains("and"), "unexpected And instruction:\n{bc}");
        assert!(!bc.contains("or"), "unexpected Or instruction:\n{bc}");
        assert!(
            bc.contains("brif"),
            "expected short-circuit branches:\n{bc}"
        );
    });
}

#[test]
fn switch_default_in_middle_uses_no_match_jump_only() {
    with_lower(
        "BEGIN { switch (x) { case 1: print 1; default: print 2; case 3: print 3 } }",
        |cg| {
            let jmp_count = cg
                .bc
                .code
                .iter()
                .filter(|i| matches!(i, Instruction::Jump { .. }))
                .count();
            assert_eq!(
                jmp_count, 1,
                "expected a single no-match jump, not per-case exits:\n{}",
                cg.bc
            );
        },
    );
}

#[test]
fn switch_typed_regex_case_uses_matches() {
    with_lower("BEGIN { switch (x) { case @/pat/: print } }", |cg| {
        let bc = format!("{}", cg.bc);
        assert!(
            bc.contains("mtch"),
            "expected Matches for typed regex case:\n{bc}"
        );
    });
}

#[test]
fn chained_and_lowers_one_branch_per_operator() {
    let mut single = 0;
    with_lower("BEGIN { print (0 && 1) }", |cg| single = brif_count(cg));
    with_lower("BEGIN { print (0 && 1 && 2) }", |cg| {
        assert_eq!(brif_count(cg), single + 1, "each && should add one branch");
    });
}

#[test]
fn switch_no_exit_jumps_between_case_bodies() {
    with_lower(
        "BEGIN { switch (1) { case 1: print; default: print 2 } }",
        |cg| {
            let jmp_count = cg
                .bc
                .code
                .iter()
                .filter(|i| matches!(i, Instruction::Jump { .. }))
                .count();
            assert_eq!(
                jmp_count, 1,
                "gawk switch fallthrough should not emit per-case exit jumps:\n{}",
                cg.bc
            );
        },
    );
}

#[test]
fn switch_scrutinee_expression_is_evaluated_once() {
    with_lower("BEGIN { switch (1 + 1) { case 2: print } }", |cg| {
        let add_count = cg
            .bc
            .code
            .iter()
            .filter(|i| matches!(i, Instruction::Add { .. }))
            .count();
        assert_eq!(
            add_count, 1,
            "scrutinee should be evaluated once:\n{}",
            cg.bc
        );
    });
}

#[test]
fn chained_or_lowers_one_branch_per_operator() {
    let mut single = 0;
    with_lower("BEGIN { print (1 || 0) }", |cg| single = brif_count(cg));
    with_lower("BEGIN { print (1 || 0 || 2) }", |cg| {
        assert_eq!(brif_count(cg), single + 1, "each || should add one branch");
    });
}

fn jump_targets(cg: &CodeGen<'_>) -> Vec<IxWidth> {
    cg.bc
        .code
        .iter()
        .filter_map(|i| match i {
            Instruction::Jump { to } => Some(to.0),
            _ => None,
        })
        .collect()
}

#[test]
fn break_in_while_jumps_past_loop() {
    with_lower("BEGIN { while (1) { break } }", |cg| {
        let targets = jump_targets(cg);
        assert!(!targets.is_empty(), "expected break jump:\n{}", cg.bc);
        let max = *targets.iter().max().expect("jumps");
        // Break must land at/after the last emitted instruction index (past loop-back).
        assert!(
            targets.contains(&max),
            "break should jump to the loop exit:\n{}",
            cg.bc
        );
        assert!(
            max >= cg.bc.len().saturating_sub(1),
            "break target should be at the end of the loop:\n{}",
            cg.bc
        );
    });
}

#[test]
fn break_in_switch_jumps_to_end() {
    with_lower(
        "BEGIN { switch (1) { case 1: break; case 2: print 2 } }",
        |cg| {
            let targets = jump_targets(cg);
            assert!(
                targets.iter().any(|&t| t == cg.bc.len()),
                "break should jump to end of switch:\n{}",
                cg.bc
            );
        },
    );
}

#[test]
fn nested_break_targets_innermost() {
    with_lower("BEGIN { while (1) { for (;;) { break } } }", |cg| {
        let targets = jump_targets(cg);
        // Inner for break + outer while loop-back (and possibly more) produce jumps;
        // the earliest jump target should be the inner for exit (before outer loop-back).
        assert!(
            targets.len() >= 2,
            "expected inner break and outer loop jumps:\n{}",
            cg.bc
        );
        let min = *targets.iter().min().expect("jumps");
        let max = *targets.iter().max().expect("jumps");
        assert!(
            min < max,
            "innermost break exit should precede outer loop targets:\n{}",
            cg.bc
        );
    });
}

#[test]
fn continue_in_while_jumps_to_condition() {
    with_lower("BEGIN { while (1) { continue } }", |cg| {
        let targets = jump_targets(cg);
        assert!(
            targets.len() >= 2,
            "expected continue + loop-back jumps:\n{}",
            cg.bc
        );
        // Both continue and the end-of-body jump re-enter at the condition.
        assert!(
            targets.windows(2).any(|w| w[0] == w[1]),
            "continue should share the while condition label with the loop-back:\n{}",
            cg.bc
        );
    });
}

#[test]
fn continue_in_for_jumps_to_update() {
    with_lower("BEGIN { for (i = 0; i < 3; i++) { continue } }", |cg| {
        let targets = jump_targets(cg);
        let min = *targets.iter().min().expect("jumps");
        let max = *targets.iter().max().expect("jumps");
        assert!(
            min < max,
            "update (continue) label should precede condition:\n{}",
            cg.bc
        );
        // Initial jump skips update → condition; continue + loop-back → update.
        assert_eq!(
            targets.iter().filter(|&&t| t == min).count(),
            2,
            "continue and loop-back should jump to update:\n{}",
            cg.bc
        );
        assert_eq!(
            targets.iter().filter(|&&t| t == max).count(),
            1,
            "one jump should skip update into the condition:\n{}",
            cg.bc
        );
    });
}

#[test]
fn continue_in_do_while_jumps_to_condition() {
    with_lower("BEGIN { do { continue } while (0) }", |cg| {
        let targets = jump_targets(cg);
        let min = *targets.iter().min().expect("jumps");
        let max = *targets.iter().max().expect("jumps");
        assert!(
            min < max,
            "condition (continue) label should precede body:\n{}",
            cg.bc
        );
        assert_eq!(
            targets.iter().filter(|&&t| t == min).count(),
            2,
            "continue and loop-back should jump to the condition:\n{}",
            cg.bc
        );
        assert_eq!(
            targets.iter().filter(|&&t| t == max).count(),
            1,
            "one jump should enter the body on first iteration:\n{}",
            cg.bc
        );
    });
}

#[test]
fn nested_continue_targets_innermost_loop() {
    // Outer while + inner for: continue must use the for update label, not the while condition.
    with_lower(
        "BEGIN { while (1) { for (i = 0; i < 3; i++) { continue } } }",
        |cg| {
            let targets = jump_targets(cg);
            // Among jumps, the innermost continue/loop-back share the smallest label that
            // is hit twice (the for update). The while loop-back is a distinct third target.
            let mut counts = std::collections::BTreeMap::<IxWidth, usize>::new();
            for t in &targets {
                *counts.entry(*t).or_default() += 1;
            }
            let twice = counts.iter().filter(|(_, c)| **c == 2).count();
            assert!(
                twice >= 1,
                "inner for continue+loop-back should share a label:\n{}",
                cg.bc
            );
            assert!(
                counts.len() >= 3,
                "nested loops should produce distinct continue/loop targets:\n{}",
                cg.bc
            );
        },
    );
}

#[test]
fn array_index_assignment_lowers_insert() {
    with_lower("BEGIN { a[1] = 2 }", |cg| {
        let bc = format!("{}", cg.bc);
        assert!(bc.contains("insert"), "expected Insert:\n{bc}");
        assert!(bc.contains("user("), "expected user-array place:\n{bc}");
    });
}

#[test]
fn array_index_read_lowers_index() {
    with_lower("BEGIN { print a[1] }", |cg| {
        let bc = format!("{}", cg.bc);
        assert!(bc.contains("sindex"), "expected IndexS:\n{bc}");
        assert!(bc.contains("user("), "expected user-array place:\n{bc}");
    });
}

#[test]
fn array_index_increment_lowers_index_and_insert() {
    with_lower("BEGIN { ++a[1] }", |cg| {
        let bc = format!("{}", cg.bc);
        assert!(bc.contains("sindex"), "expected IndexS:\n{bc}");
        assert!(bc.contains("insert"), "expected Insert:\n{bc}");
    });
}

#[test]
fn array_multi_index_assignment_lowers_insert() {
    with_lower("BEGIN { a[1, 2] = 3 }", |cg| {
        let bc = format!("{}", cg.bc);
        assert!(bc.contains("insert"), "expected Insert:\n{bc}");
    });
}

#[test]
fn builtin_call_lowers_to_icall() {
    with_lower("BEGIN { print int(3.7) }", |cg| {
        let bc = format!("{}", cg.bc);
        assert!(bc.contains("int,"), "expected bcall for int():\n{bc}");
    });
}

#[test]
fn builtin_call_nested_in_expression_lowers_icall() {
    with_lower("BEGIN { x = length(\"abc\") + sqrt(4) }", |cg| {
        let bc = format!("{}", cg.bc);
        assert!(bc.contains("length,"), "expected length bcall:\n{bc}");
        assert!(bc.contains("sqrt,"), "expected sqrt bcall:\n{bc}");
    });
}
