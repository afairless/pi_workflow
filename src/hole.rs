//! Deterministic TODO-row hole detection (C) — the gate in front of every
//! LLM step in hole resolution.
//!
//! A **hole** is an unmatched row that has at least one *git-matched* row
//! with a higher number: the plan advanced past this row without a commit
//! that matches its planned message. Trailing unmatched rows are not holes —
//! the plan merely stopped — and evidence for the successor must come from
//! git, never from `adjudicated` or `verified` (a row without a commit is not
//! proof that the plan advanced).

use std::collections::{BTreeMap, BTreeSet};

use crate::git;
use crate::todo::{TodoRow, UNNUMBERED_ROW_NUMBER};

/// Subject-drift report (B): how the closest commit subject compares to the
/// row's planned commit message.
///
/// B is **evidence, not a tier**: it changes nothing in Contract 2 and its
/// `tier` is the plain [`git::match_tier`] verdict for the pair. Callers use
/// it for the verifier bundle and the operator label only.
#[derive(Debug, Clone, PartialEq)]
pub struct SubjectDiff {
    /// The row's planned commit message, verbatim.
    pub planned: String,
    /// The raw closest subject (empty only when the history has no commits).
    pub best_subject: String,
    /// Hash of the commit carrying `best_subject` (`None` with no history).
    pub best_commit: Option<String>,
    /// Contract 2 tier for the pair — `Exact`, `Similar`, `Candidate` or
    /// `None`.
    pub tier: git::MatchTier,
    /// Similarity ratio of the normalized strings, rounded to two decimals.
    pub ratio: f64,
    /// Words the actual subject has that the planned message does not
    /// (planned → actual insertions), in actual order.
    pub extra_tokens: Vec<String>,
    /// Words the planned message has that the actual subject does not, in
    /// planned order.
    pub missing_tokens: Vec<String>,
}

/// Round to two decimal places, the precision the report displays.
fn round_2dp(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Words of a message: [`git::normalize_message`] rules, then split on
/// whitespace. One tokenizer for the plan and the repo, so "add  parser" and
/// "`feat: add parser`" compare equal.
fn tokens(message: &str) -> Vec<String> {
    git::normalize_message(message)
        .split_whitespace()
        .map(String::from)
        .collect()
}

/// The words of `actual` that `planned` does not account for, in actual
/// order, honouring multiplicity (`add add` vs `add` yields one extra).
fn extra_tokens(planned: &[String], actual: &[String]) -> Vec<String> {
    let mut budget: BTreeMap<&str, usize> = BTreeMap::new();
    for token in planned {
        *budget.entry(token.as_str()).or_insert(0) += 1;
    }
    let mut extra = Vec::new();
    for token in actual {
        match budget.get_mut(token.as_str()) {
            Some(count) if *count > 0 => *count -= 1,
            _ => extra.push(token.clone()),
        }
    }
    extra
}

/// The words of `planned` that `actual` does not have, in planned order,
/// honouring multiplicity.
fn missing_tokens(planned: &[String], actual: &[String]) -> Vec<String> {
    let mut budget: BTreeMap<&str, usize> = BTreeMap::new();
    for token in actual {
        *budget.entry(token.as_str()).or_insert(0) += 1;
    }
    let mut missing = Vec::new();
    for token in planned {
        match budget.get_mut(token.as_str()) {
            Some(count) if *count > 0 => *count -= 1,
            _ => missing.push(token.clone()),
        }
    }
    missing
}

/// Diff the row's planned message against git history (newest first).
///
/// The best pair is the one with the strongest Contract 2 tier, then the
/// highest similarity ratio, then the newest commit (the first in `history`).
/// The chosen subject is always reported, even when its tier is `None`, so the
/// verifier and the operator see the closest thing git has. An empty history
/// yields no subject, no commit, `MatchTier::None` and a `0.0` ratio. Like
/// [`git::history`], this is best-effort and total — it never panics.
pub fn diff_subject(planned: &str, history: &[git::CommitRef]) -> SubjectDiff {
    let normalized = git::normalize_message(planned);
    let mut best: Option<(git::MatchTier, f64, &git::CommitRef)> = None;
    for commit in history {
        let tier = git::match_tier(planned, &commit.subject);
        let ratio = git::similarity_ratio(&normalized, &git::normalize_message(&commit.subject));
        let better = match best {
            None => true,
            Some((best_tier, best_ratio, _)) => {
                // Strict comparisons keep the newest of two equal pairs.
                tier > best_tier || (tier == best_tier && ratio > best_ratio)
            }
        };
        if better {
            best = Some((tier, ratio, commit));
        }
    }

    let planned_tokens = tokens(planned);
    let best_subject = best.map(|(_, _, c)| c.subject.clone()).unwrap_or_default();
    let actual_tokens = tokens(&best_subject);
    let (tier, ratio) = match best {
        Some((tier, ratio, _)) => (tier, round_2dp(ratio)),
        // With no commit there is nothing to be similar to; keep the ratio at
        // zero rather than letting two empty token lists report 1.0.
        None => (git::MatchTier::None, 0.0),
    };

    SubjectDiff {
        planned: planned.to_string(),
        best_subject,
        best_commit: best.map(|(_, _, c)| c.hash.clone()),
        tier,
        ratio,
        extra_tokens: extra_tokens(&planned_tokens, &actual_tokens),
        missing_tokens: missing_tokens(&planned_tokens, &actual_tokens),
    }
}

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

    // ---- subject diff (B) ----

    fn commit(hash: &str, subject: &str) -> git::CommitRef {
        git::CommitRef {
            hash: hash.to_string(),
            subject: subject.to_string(),
            authored_at: "2026-11-22T10:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn identical_messages_diff_to_exact_with_no_token_drift() {
        let diff = diff_subject(
            "feat: add deterministic TODO-hole detection",
            &[commit(
                "abc123",
                "feat: add deterministic TODO-hole detection",
            )],
        );
        assert_eq!(diff.tier, git::MatchTier::Exact);
        assert_eq!(diff.ratio, 1.0);
        assert_eq!(
            diff.best_subject,
            "feat: add deterministic TODO-hole detection"
        );
        assert_eq!(diff.best_commit.as_deref(), Some("abc123"));
        assert!(diff.extra_tokens.is_empty());
        assert!(diff.missing_tokens.is_empty());
    }

    #[test]
    fn an_inserted_word_is_an_extra_token_not_a_missing_one() {
        // The incident: the actual subject inserted "training".
        let planned = "test: add the decision-time leakage suite";
        let actual = "test: add the decision-time training leakage suite";
        let diff = diff_subject(planned, &[commit("fc5ea15", actual)]);
        assert_eq!(diff.extra_tokens, vec!["training"]);
        assert!(diff.missing_tokens.is_empty());
        // 1 − 9/50 = 0.82, pinned to two decimals and a candidate tier.
        assert_eq!(diff.ratio, 0.82);
        assert_eq!(diff.tier, git::MatchTier::Candidate);
    }

    #[test]
    fn a_deleted_word_is_a_missing_token_not_an_extra_one() {
        let diff = diff_subject(
            "feat: add deterministic TODO-hole detection",
            &[commit("def456", "feat: add deterministic detection")],
        );
        assert_eq!(diff.missing_tokens, vec!["todo-hole"]);
        assert!(diff.extra_tokens.is_empty());
    }

    #[test]
    fn a_substitution_reports_both_directions() {
        let diff = diff_subject("feat: add parser", &[commit("aaa111", "feat: add lexer")]);
        assert_eq!(diff.extra_tokens, vec!["lexer"]);
        assert_eq!(diff.missing_tokens, vec!["parser"]);
    }

    #[test]
    fn matching_ignores_case_whitespace_backticks_and_trailing_punctuation() {
        let diff = diff_subject(
            "`Feat:   Add  Parser.`",
            &[commit("bbb222", "feat: add parser")],
        );
        assert_eq!(diff.tier, git::MatchTier::Exact);
        assert_eq!(diff.ratio, 1.0);
        assert!(diff.extra_tokens.is_empty());
        assert!(diff.missing_tokens.is_empty());
    }

    #[test]
    fn duplicate_words_are_counted_not_collapsed() {
        let diff = diff_subject(
            "fix: add add parser",
            &[commit("ccc333", "fix: add parser")],
        );
        assert_eq!(diff.missing_tokens, vec!["add"]);
        assert!(diff.extra_tokens.is_empty());
        let diff = diff_subject(
            "fix: add parser",
            &[commit("ccc333", "fix: add add parser")],
        );
        assert_eq!(diff.extra_tokens, vec!["add"]);
        assert!(diff.missing_tokens.is_empty());
    }

    #[test]
    fn strongest_tier_wins_and_ties_go_to_the_newest_commit() {
        let history = vec![
            commit("newest", "feat: add parser"),
            commit("candidate", "feat: add parsers everywhere"),
            commit("older", "feat: add parser"),
        ];
        let diff = diff_subject("feat: add parser", &history);
        assert_eq!(diff.tier, git::MatchTier::Exact);
        assert_eq!(diff.best_commit.as_deref(), Some("newest"));
        assert_eq!(diff.best_subject, "feat: add parser");
    }

    #[test]
    fn a_weaker_subject_never_displaces_a_stronger_one() {
        let history = vec![
            commit("unrelated", "docs: rewrite the readme"),
            commit("near", "feat: add parsers"),
        ];
        let diff = diff_subject("feat: add parser", &history);
        assert_eq!(diff.tier, git::MatchTier::Similar);
        assert_eq!(diff.best_commit.as_deref(), Some("near"));
    }

    #[test]
    fn equal_tiers_are_broken_by_the_higher_ratio() {
        let history = vec![
            commit("prefix", "feat: add parser and more"),
            commit("drift", "feat: add parxxx"),
        ];
        // Both are candidates; the higher ratio wins over recency.
        let diff = diff_subject("feat: add parser", &history);
        assert_eq!(diff.tier, git::MatchTier::Candidate);
        assert_eq!(diff.best_commit.as_deref(), Some("drift"));
    }

    #[test]
    fn empty_history_yields_no_subject_no_commit_and_no_ratio() {
        let diff = diff_subject("feat: add parser", &[]);
        assert_eq!(diff.tier, git::MatchTier::None);
        assert_eq!(diff.ratio, 0.0);
        assert_eq!(diff.best_subject, "");
        assert!(diff.best_commit.is_none());
        assert_eq!(diff.missing_tokens, vec!["feat:", "add", "parser"]);
        assert!(diff.extra_tokens.is_empty());
    }

    #[test]
    fn an_unrelated_history_stays_below_candidate() {
        let diff = diff_subject(
            "feat: add parser",
            &[commit("zzz999", "docs: rewrite the readme")],
        );
        assert_eq!(diff.tier, git::MatchTier::None);
        // The closest subject is still reported so the report can name it.
        assert_eq!(diff.best_commit.as_deref(), Some("zzz999"));
        assert_eq!(diff.best_subject, "docs: rewrite the readme");
    }

    #[test]
    fn ratio_is_a_two_decimal_f64() {
        // 1 − 1/10 = 0.9 exactly → similar, still just two decimals.
        let diff = diff_subject("abcdefghij", &[commit("ddd444", "abcdefghiX")]);
        assert_eq!(diff.tier, git::MatchTier::Similar);
        assert_eq!(diff.ratio, 0.9);
        // Thirds cannot be represented exactly; the report still pins 2dp.
        let diff = diff_subject("abc", &[commit("eee555", "abd")]);
        assert_eq!(diff.ratio, 0.67);
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
        /// An identical subject always diffs to `Exact` at ratio 1.0 with no
        /// token drift, whatever the message.
        #[test]
        fn identical_subjects_always_diff_exact(message in "[a-z][a-z: -]{0,40}") {
            let diff = diff_subject(&message, &[commit("h", &message)]);
            prop_assert_eq!(diff.tier, git::MatchTier::Exact);
            prop_assert_eq!(diff.ratio, 1.0);
            prop_assert!(diff.extra_tokens.is_empty());
            prop_assert!(diff.missing_tokens.is_empty());
        }

        /// The reported tier is exactly Contract 2's verdict for the reported
        /// pair — B never invents a tier of its own.
        #[test]
        fn reported_tier_is_match_tier_of_the_reported_pair(
            planned in "[a-z][a-z: -]{0,30}",
            subject in "[a-z][a-z: -]{0,30}",
        ) {
            let diff = diff_subject(&planned, &[commit("h", &subject)]);
            prop_assert_eq!(diff.tier, git::match_tier(&planned, &subject));
            prop_assert_eq!(diff.best_subject, subject.clone());
            prop_assert_eq!(diff.best_commit.as_deref(), Some("h"));
        }

        /// Token drift partitions the actual words: nothing is invented and
        /// no token appears on both sides.
        #[test]
        fn extra_and_missing_tokens_are_disjoint(planned in "[a-z ]{0,30}", actual in "[a-z ]{0,30}") {
            let diff = diff_subject(&planned, &[commit("h", &actual)]);
            for token in &diff.extra_tokens {
                prop_assert!(!diff.missing_tokens.contains(token));
            }
        }

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
