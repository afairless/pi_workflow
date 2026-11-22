//! Deterministic TODO-row hole detection (C) — the gate in front of every
//! LLM step in hole resolution.
//!
//! A **hole** is an unmatched row that has at least one *git-matched* row
//! with a higher number: the plan advanced past this row without a commit
//! that matches its planned message. Trailing unmatched rows are not holes —
//! the plan merely stopped — and evidence for the successor must come from
//! git, never from `adjudicated` or `verified` (a row without a commit is not
//! proof that the plan advanced).

use std::collections::BTreeSet;

use crate::git;
use crate::todo::{TodoRow, UNNUMBERED_ROW_NUMBER};

/// The repo evidence: the row's planned commit message matches a commit
/// subject at Contract 2's `Exact` or `Similar` tier.
///
/// Contract 2 itself is untouched: `Candidate` matches are *not* done.
pub fn git_done(row: &TodoRow, subjects: &[String]) -> bool {
    git::is_row_done(&row.commit_message, subjects)
}

/// Every way a row counts as done:
/// - `git_done` — the repo evidence,
/// - `adjudicated` — the human's `pi-plan mark <n> done`,
/// - `verified` — the verifier's authoritative skip.
pub fn is_done(row: &TodoRow, subjects: &[String], adjudicated: &[u64], verified: &[u64]) -> bool {
    git_done(row, subjects) || adjudicated.contains(&row.number) || verified.contains(&row.number)
}

/// Row numbers of every hole, ascending.
///
/// The return value is **row numbers**, not indices into `rows`; document
/// order is the tiebreak for duplicate row numbers (and duplicate numbers
/// collapse to one entry, because callers map the number back to the first
/// row carrying it).
///
/// `UNNUMBERED_ROW_NUMBER` sorts last, so an unnumbered row can never be a
/// hole and never serves as a successor's evidence.
pub fn detect_holes(
    rows: &[TodoRow],
    subjects: &[String],
    adjudicated: &[u64],
    verified: &[u64],
) -> Vec<u64> {
    // Unique numbered rows in document order, plus number-level done sets:
    // one matched (or adjudicated/verified) duplicate marks the whole number
    // done, so a plan with repeated ids cannot report a phantom hole.
    let mut order: Vec<u64> = Vec::new();
    let mut done_numbers: BTreeSet<u64> = BTreeSet::new();
    let mut git_done_numbers: BTreeSet<u64> = BTreeSet::new();
    for row in rows {
        if row.number == UNNUMBERED_ROW_NUMBER {
            continue;
        }
        if !order.contains(&row.number) {
            order.push(row.number);
        }
        if git_done(row, subjects) {
            git_done_numbers.insert(row.number);
        }
        if is_done(row, subjects, adjudicated, verified) {
            done_numbers.insert(row.number);
        }
    }

    // Successor evidence: the highest numbered git-matched row. A row can
    // only be a hole below it.
    let Some(highest_git_done) = git_done_numbers.iter().next_back().copied() else {
        return Vec::new();
    };

    let mut holes: Vec<(u64, usize)> = order
        .iter()
        .enumerate()
        .filter(|(_, number)| **number < highest_git_done && !done_numbers.contains(number))
        .map(|(index, number)| (*number, index))
        .collect();
    // Ascending by row number; document order breaks ties (which only exist
    // for repeated numbers, and those have already collapsed).
    holes.sort_unstable();
    holes.into_iter().map(|(number, _)| number).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn row(number: u64, commit: &str) -> TodoRow {
        TodoRow {
            id: format!("{number}"),
            number,
            commit_message: commit.to_string(),
            logical_unit: "unit".to_string(),
            deliverables: "deliverables".to_string(),
            tests: "unit".to_string(),
        }
    }

    /// One distinct word per row number, so an unmatched row never lands in
    /// Contract 2's `Similar` tier by accident (adjacent numbers must not
    /// near-match — that is the matcher's business, not the hole test's).
    const WORDS: [&str; 12] = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
        "juliet", "kilo", "lima",
    ];

    /// The planned message for row `number` (1-based word index).
    fn message(number: u64) -> String {
        format!("feat: {}", WORDS[(number as usize - 1) % WORDS.len()])
    }

    /// A `## Steps`-style plan of `1..=last` rows where `matched` are
    /// committed exactly and the rest have no commit at all.
    fn plan_1_to(last: u64, matched: &[u64]) -> (Vec<TodoRow>, Vec<String>) {
        let rows: Vec<TodoRow> = (1..=last).map(|n| row(n, &message(n))).collect();
        let subjects: Vec<String> = matched.iter().map(|n| message(*n)).collect();
        (rows, subjects)
    }

    // ---- git_done ----

    #[test]
    fn git_done_requires_exact_or_similar_not_candidate() {
        let r = row(8, "test: add the decision-time leakage suite");
        assert!(git_done(
            &r,
            &["test: add the decision-time leakage suite".into()]
        ));
        // One-character drift is similar → done.
        assert!(git_done(
            &r,
            &["test: add the decision-time leakage suie".into()]
        ));
        // The incident's 0.82 ratio is a candidate → not done.
        assert!(!git_done(
            &r,
            &["test: add the decision-time training leakage suite".into()]
        ));
        assert!(!git_done(&r, &[]));
    }

    // ---- is_done truth table ----

    #[test]
    fn is_done_covers_each_of_the_three_evidences() {
        let r = row(8, &message(8));
        let matched = vec![message(8)];
        let unmatched = vec![message(9)];

        // git evidence alone.
        assert!(is_done(&r, &matched, &[], &[]));
        // human adjudication alone.
        assert!(is_done(&r, &unmatched, &[8], &[]));
        // verifier skip alone.
        assert!(is_done(&r, &unmatched, &[], &[8]));
        // none of them.
        assert!(!is_done(&r, &unmatched, &[], &[]));
        // other rows' provenance does not leak.
        assert!(!is_done(&r, &unmatched, &[9], &[9]));
        // a candidate match is not done.
        assert!(!is_done(
            &row(8, &message(8)),
            &[format!("{} and then some", message(8))],
            &[],
            &[]
        ));
    }

    // ---- hole predicate table ----

    #[test]
    fn a_single_missing_row_below_matched_rows_is_a_hole() {
        let matched: Vec<u64> = (1..=12).filter(|n| *n != 8).collect();
        let (rows, subjects) = plan_1_to(12, &matched);
        assert_eq!(detect_holes(&rows, &subjects, &[], &[]), vec![8]);
    }

    #[test]
    fn trailing_unmatched_rows_are_not_holes() {
        let (rows, subjects) = plan_1_to(12, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(detect_holes(&rows, &subjects, &[], &[]).is_empty());
    }

    #[test]
    fn multiple_holes_are_reported_ascending() {
        let matched = [1, 2, 4, 5, 6, 8, 9];
        let (rows, subjects) = plan_1_to(9, &matched);
        assert_eq!(detect_holes(&rows, &subjects, &[], &[]), vec![3, 7]);
    }

    #[test]
    fn document_order_does_not_disturb_row_number_order() {
        // Rows written 7, 3, 9, 10 with 3 and 7 unmatched.
        let rows = vec![
            row(7, &message(7)),
            row(3, &message(3)),
            row(9, &message(9)),
            row(10, &message(10)),
        ];
        let subjects = vec![message(9), message(10)];
        assert_eq!(detect_holes(&rows, &subjects, &[], &[]), vec![3, 7]);
    }

    #[test]
    fn adjudicated_or_verified_rows_are_not_holes() {
        let matched = [1, 2, 3, 4, 5, 7, 8, 9];
        let (rows, subjects) = plan_1_to(9, &matched);
        assert_eq!(detect_holes(&rows, &subjects, &[6], &[]), Vec::<u64>::new());
        assert_eq!(detect_holes(&rows, &subjects, &[], &[6]), Vec::<u64>::new());
        // Provenance for another row does not close the hole.
        assert_eq!(detect_holes(&rows, &subjects, &[5], &[]), vec![6]);
    }

    #[test]
    fn successor_evidence_must_be_git_done_never_provenance() {
        // Row 3 is unmatched; rows 5 and 6 are only adjudicated/verified, so
        // the plan has no git evidence of having advanced.
        let rows = vec![
            row(1, &message(1)),
            row(3, &message(3)),
            row(5, &message(5)),
            row(6, &message(6)),
        ];
        let subjects = vec![message(1)];
        assert!(detect_holes(&rows, &subjects, &[5], &[6]).is_empty());
        // Once row 5 is committed, row 3 becomes a hole.
        let subjects = vec![message(1), message(5)];
        assert_eq!(detect_holes(&rows, &subjects, &[], &[6]), vec![3]);
    }

    #[test]
    fn duplicate_row_numbers_collapse_and_any_duplicate_done_closes_the_number() {
        // Two rows both numbered 8: one has the commit, the other does not.
        let rows = vec![
            row(1, &message(1)),
            row(8, &message(8)),
            row(8, &message(8)),
            row(9, &message(9)),
        ];
        let subjects = vec![message(1), message(9)];
        // Neither duplicate is done: one hole, reported once.
        assert_eq!(detect_holes(&rows, &subjects, &[], &[]), vec![8]);
        // The first duplicate's commit closes the number for both.
        let subjects = vec![message(1), message(8), message(9)];
        assert!(detect_holes(&rows, &subjects, &[], &[]).is_empty());
    }

    #[test]
    fn unnumbered_rows_can_be_neither_holes_nor_successors() {
        // Unnumbered row is unmatched and sits in the middle: never a hole.
        let rows = vec![
            row(1, &message(1)),
            row(UNNUMBERED_ROW_NUMBER, "feat: zulu unknown"),
            row(3, &message(3)),
        ];
        let subjects = vec![message(1), message(3)];
        assert!(detect_holes(&rows, &subjects, &[], &[]).is_empty());

        // Unnumbered row is git-matched and sorts last: it must not be the
        // successor evidence that turns row 5 into a hole.
        let rows = vec![
            row(5, &message(5)),
            row(UNNUMBERED_ROW_NUMBER, "feat: zulu extra"),
        ];
        let subjects = vec!["feat: zulu extra".to_string()];
        assert!(detect_holes(&rows, &subjects, &[], &[]).is_empty());
    }

    #[test]
    fn fully_matched_or_empty_plans_have_no_holes() {
        let (rows, subjects) = plan_1_to(5, &[1, 2, 3, 4, 5]);
        assert!(detect_holes(&rows, &subjects, &[], &[]).is_empty());
        assert!(detect_holes(&[], &[], &[], &[]).is_empty());
        let (rows, _) = plan_1_to(3, &[]);
        assert!(detect_holes(&rows, &[], &[], &[]).is_empty());
    }

    // ---- property-based ----

    /// Independent reference: one row at a time, with the number-level done
    /// and successor rules spelled out longhand.
    fn reference_holes(
        rows: &[TodoRow],
        subjects: &[String],
        adjudicated: &[u64],
        verified: &[u64],
    ) -> Vec<u64> {
        let numbered: Vec<&TodoRow> = rows
            .iter()
            .filter(|r| r.number != UNNUMBERED_ROW_NUMBER)
            .collect();
        let number_done = |number: u64| {
            numbered
                .iter()
                .any(|r| r.number == number && is_done(r, subjects, adjudicated, verified))
        };
        let number_git_done = |number: u64| {
            numbered
                .iter()
                .any(|r| r.number == number && git_done(r, subjects))
        };
        let mut holes: Vec<u64> = Vec::new();
        for r in &numbered {
            if number_done(r.number) {
                continue;
            }
            let advanced = numbered
                .iter()
                .any(|s| s.number > r.number && number_git_done(s.number));
            if advanced && !holes.contains(&r.number) {
                holes.push(r.number);
            }
        }
        holes.sort_unstable();
        holes
    }

    fn spec_strategy() -> impl Strategy<Value = Vec<(Option<u64>, bool)>> {
        // `None` number → an unnumbered row; `true` → committed exactly.
        proptest::collection::vec((proptest::option::of(1u64..=12), proptest::bool::ANY), 0..8)
    }

    fn build(specs: &[(Option<u64>, bool)]) -> (Vec<TodoRow>, Vec<String>) {
        let mut rows = Vec::new();
        let mut subjects = Vec::new();
        for (index, (number, matched)) in specs.iter().enumerate() {
            let number = number.unwrap_or(UNNUMBERED_ROW_NUMBER);
            // Disjoint messages per row (words, not digits) so only the
            // scripted subject lists can produce a match.
            let text = format!("feat: {}", WORDS[index % WORDS.len()]);
            rows.push(row(number, &text));
            if *matched {
                subjects.push(text);
            }
        }
        (rows, subjects)
    }

    proptest! {
        /// The detector agrees with the longhand reference on every shape.
        #[test]
        fn detect_holes_matches_the_reference(specs in spec_strategy()) {
            let (rows, subjects) = build(&specs);
            prop_assert_eq!(
                detect_holes(&rows, &subjects, &[], &[]),
                reference_holes(&rows, &subjects, &[], &[]),
                "rows {:?} subjects {:?}",
                rows.iter().map(|r| r.number).collect::<Vec<_>>(),
                subjects,
            );
        }

        /// Output is strictly ascending, so callers can binary-search it.
        #[test]
        fn holes_are_strictly_ascending_and_unique(
            specs in spec_strategy(),
            adjudicated in proptest::collection::vec(1u64..=12, 0..3),
            verified in proptest::collection::vec(1u64..=12, 0..3),
        ) {
            let (rows, subjects) = build(&specs);
            let holes = detect_holes(&rows, &subjects, &adjudicated, &verified);
            for pair in holes.windows(2) {
                prop_assert!(pair[0] < pair[1], "holes {:?} not ascending", holes);
            }
        }

        /// Every reported hole is genuinely unmatched and has a higher-numbered
        /// git-matched row; and no reported hole is done by any evidence.
        #[test]
        fn every_hole_has_a_git_done_successor(specs in spec_strategy()) {
            let (rows, subjects) = build(&specs);
            for number in detect_holes(&rows, &subjects, &[], &[]) {
                prop_assert!(number != UNNUMBERED_ROW_NUMBER);
                prop_assert!(rows.iter().any(|r| r.number == number));
                prop_assert!(!rows.iter().any(|r| r.number == number && git_done(r, &subjects)));
                prop_assert!(rows.iter().any(|s| s.number != UNNUMBERED_ROW_NUMBER
                    && s.number > number
                    && git_done(s, &subjects)));
            }
        }

        /// Trailing unmatched rows never open a hole: with no matched rows
        /// above them, the plan merely stopped.
        #[test]
        fn unmatched_tail_is_never_a_hole(specs in spec_strategy()) {
            let (rows, subjects) = build(&specs);
            let highest_matched = rows
                .iter()
                .filter(|r| r.number != UNNUMBERED_ROW_NUMBER && git_done(r, &subjects))
                .map(|r| r.number)
                .max();
            for number in detect_holes(&rows, &subjects, &[], &[]) {
                prop_assert!(highest_matched.is_some_and(|highest| number < highest));
            }
        }

        /// Adjudicating every hole closes all of them (idempotent progress).
        #[test]
        fn adjudicating_every_hole_empties_the_set(specs in spec_strategy()) {
            let (rows, subjects) = build(&specs);
            let holes = detect_holes(&rows, &subjects, &[], &[]);
            prop_assert!(detect_holes(&rows, &subjects, &holes, &[]).is_empty());
            prop_assert!(detect_holes(&rows, &subjects, &[], &holes).is_empty());
        }
    }
}
