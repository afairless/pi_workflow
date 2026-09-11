//! Git facade + Contract 2 commit matcher — a port of the TS supervisor's
//! `git.ts` (the facade mirrors `spawnSync`: it never throws, it reports
//! status instead).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Stable ratio thresholds; computed on normalized strings by callers.
pub const SIMILAR_RATIO: f64 = 0.9;
pub const CANDIDATE_RATIO: f64 = 0.8;

/// Contract 2 match tiers, from strongest to weakest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchTier {
    None,
    Candidate,
    Similar,
    Exact,
}

/// Result of matching one planned message against git subjects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    /// Strongest tier achieved across all subjects.
    pub tier: MatchTier,
    /// The subject that produced the strongest tier, when any did.
    pub subject: Option<String>,
}

/// Normalize a planned or actual commit message:
/// strip surrounding backticks, trim, collapse internal whitespace, strip
/// trailing punctuation, lowercase.
pub fn normalize_message(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if s.len() >= 2 && s.starts_with('`') && s.ends_with('`') {
        s = s[1..s.len() - 1].to_string();
    }
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    while s.ends_with(['.', '!', '?']) {
        s.pop();
    }
    s.to_lowercase()
}

/// Classic iterative edit distance over character counts.
pub fn levenshtein(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    let n = a.chars().count();
    let m = b.chars().count();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=m).collect();
    for (i, ac) in a.iter().enumerate() {
        let mut curr = vec![0usize; m + 1];
        curr[0] = i + 1;
        for (j, bc) in b.iter().enumerate() {
            let cost = usize::from(ac != bc);
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        prev = curr;
    }
    prev[m]
}

/// Similarity ratio `1 − lev/max(len)`, 1.0 for identical strings.
pub fn similarity_ratio(a: &str, b: &str) -> f64 {
    let max_len = a.chars().count().max(b.chars().count());
    if max_len == 0 {
        return 1.0;
    }
    1.0 - levenshtein(a, b) as f64 / max_len as f64
}

/// Tier for one pair of messages (Contract 2):
/// - `Exact`: normalized strings equal
/// - `Similar`: similarity ratio ≥ 0.9 (auto-accepted, reported as near-miss)
/// - `Candidate`: subject starts with planned, or ratio in [0.8, 0.9) — NOT
///   auto-accepted; the supervisor reports both strings for adjudication
/// - `None`: everything weaker
pub fn match_tier(planned: &str, subject: &str) -> MatchTier {
    let p = normalize_message(planned);
    let s = normalize_message(subject);
    if p == s {
        return MatchTier::Exact;
    }
    let ratio = similarity_ratio(&p, &s);
    if ratio >= SIMILAR_RATIO {
        return MatchTier::Similar;
    }
    if s.starts_with(&p) || ratio >= CANDIDATE_RATIO {
        return MatchTier::Candidate;
    }
    MatchTier::None
}

/// Strongest tier over all git subjects for one planned message.
///
/// A prefix match of a much longer actual subject still only reaches
/// `Candidate` — the worker that committed the planned prefix plus unrelated
/// work must be confirmed by the user.
pub fn match_planned(planned: &str, subjects: &[String]) -> MatchResult {
    let mut best = MatchResult {
        tier: MatchTier::None,
        subject: None,
    };
    for subject in subjects {
        let tier = match_tier(planned, subject);
        if tier > best.tier {
            best = MatchResult {
                tier,
                subject: Some(subject.clone()),
            };
        }
    }
    best
}

/// True when the planned message is done: exact or similar match exists.
pub fn is_row_done(planned: &str, subjects: &[String]) -> bool {
    let tier = match_planned(planned, subjects).tier;
    tier == MatchTier::Exact || tier == MatchTier::Similar
}

/// Outcome of one `git` invocation. `status` is `None` when the process could
/// not be spawned at all (mirrors Node's `spawnSync` `res.status === null`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Overridable command runner; production uses `Command`, tests inject fakes.
pub type GitRunner = Box<dyn Fn(&[String]) -> RunResult>;

/// `Command`-based runner for git; never throws — reports status instead.
pub fn make_git_runner(cwd: &Path, git_binary: &str) -> GitRunner {
    let cwd: PathBuf = cwd.to_path_buf();
    let git = git_binary.to_string();
    Box::new(move |args: &[String]| {
        match Command::new(&git).args(args).current_dir(&cwd).output() {
            Ok(out) => RunResult {
                status: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(err) => RunResult {
                status: None,
                stdout: String::new(),
                stderr: err.to_string(),
            },
        }
    })
}

/// Split command output into non-empty, trimmed lines.
fn lines(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

fn ok(r: &RunResult) -> bool {
    r.status == Some(0)
}

/// The git command set bound to one working directory. The supervise loop
/// never shells out otherwise.
pub struct GitCommands {
    run: GitRunner,
}

impl GitCommands {
    /// Bind to `cwd` with the production `git` runner.
    pub fn new(cwd: &Path) -> Self {
        Self::with_runner(cwd, make_git_runner(cwd, "git"))
    }

    /// Bind to `cwd` with an injected runner (tests, custom git binary).
    pub fn with_runner(cwd: &Path, run: GitRunner) -> Self {
        let _ = cwd; // kept for API parity; the runner captures cwd itself
        Self { run }
    }

    /// Full hash of HEAD (`git rev-parse HEAD`), or `None` at unborn HEAD.
    pub fn head(&self) -> Option<String> {
        let r = (self.run)(&["rev-parse".into(), "HEAD".into()]);
        if !ok(&r) {
            return None;
        }
        lines(&r.stdout).into_iter().next()
    }

    /// Commit subjects, newest first (`git log --format=%s --no-decorate`).
    pub fn subjects(&self) -> Vec<String> {
        let r = (self.run)(&["log".into(), "--format=%s".into(), "--no-decorate".into()]);
        if !ok(&r) {
            return Vec::new();
        }
        lines(&r.stdout)
    }

    /// `git status --short` output lines.
    pub fn status_short(&self) -> Vec<String> {
        let r = (self.run)(&["status".into(), "--short".into()]);
        if !ok(&r) {
            return Vec::new();
        }
        lines(&r.stdout)
    }

    /// True when `git status --porcelain` is empty.
    pub fn is_clean(&self) -> bool {
        let r = (self.run)(&["status".into(), "--porcelain".into()]);
        ok(&r) && r.stdout.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ---- normalization ----

    #[test]
    fn normalize_message_strips_backticks_collapses_whitespace_trims_punctuation() {
        assert_eq!(normalize_message("`feat: add parser`"), "feat: add parser");
        assert_eq!(
            normalize_message("  feat:   add  \t parser  "),
            "feat: add parser"
        );
        assert_eq!(normalize_message("feat: add parser."), "feat: add parser");
        assert_eq!(normalize_message("Feat: Add Parser"), "feat: add parser");
        assert_eq!(
            normalize_message("fix: no trailing punct"),
            "fix: no trailing punct"
        );
    }

    // ---- edit distance / ratio ----

    #[test]
    fn levenshtein_corner_cases() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("", "xyz"), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn similarity_ratio_is_1_for_identical_and_low_for_disjoint() {
        assert_eq!(similarity_ratio("abc", "abc"), 1.0);
        assert_eq!(similarity_ratio("", ""), 1.0);
        assert!(similarity_ratio("abc", "xyz") < 0.5);
    }

    // ---- match tiers ----

    #[test]
    fn match_tier_classifies_exact_similar_candidate_and_none() {
        assert_eq!(
            match_tier("`feat: add parser`", "feat: add parser"),
            MatchTier::Exact
        );
        assert_eq!(
            match_tier("feat: add parser", "Feat:  Add  Parser."),
            MatchTier::Exact
        );

        // One-char drift on a long subject lands in similar (ratio ≥ 0.9).
        assert_eq!(
            match_tier("feat: add parser", "feat: add parsr"),
            MatchTier::Similar
        );

        // Prefix match of much longer subject → candidate, never auto-accept.
        assert_eq!(
            match_tier("feat: add parser", "feat: add parser and tests and more"),
            MatchTier::Candidate
        );

        // Disjoint.
        assert_eq!(
            match_tier("feat: add parser", "fix: typo in readme"),
            MatchTier::None
        );
    }

    #[test]
    fn similarity_tier_boundary_is_at_0_9() {
        // identical ratio (1.0) is exact by equality; drift keeps ratio ≥ 0.9 in similar.
        assert_eq!(levenshtein("feat: add parser", "feat: add parsr"), 1);
        assert!(similarity_ratio("feat: add parser", "feat: add parsr") >= 0.9);
        assert_eq!(
            match_tier("feat: add parser", "feat: add parsr"),
            MatchTier::Similar
        );
    }

    #[test]
    fn candidate_ratio_band_is_0_8_inclusive_to_0_9_exclusive() {
        // A one-character substitution on a 9-char string: ratio 0.8889, no prefix.
        let a = "abcdefghi";
        let b = "abcdXfghi";
        let r = similarity_ratio(a, b);
        assert!(
            (CANDIDATE_RATIO..0.9).contains(&r),
            "ratio {r} in [0.8, 0.9)"
        );
        assert_eq!(match_tier(a, b), MatchTier::Candidate);
    }

    #[test]
    fn similar_tier_is_not_absorbed_into_candidate() {
        // A similar pair whose subject does NOT start with planned and whose
        // ratio is ≥ 0.9 fails the candidate rule (startsWith || [0.8, 0.9)).
        let planned = "feat: add parser";
        let subject = "feat: add parsr";
        assert_eq!(match_tier(planned, subject), MatchTier::Similar);
        let ns = normalize_message(subject);
        assert!(!ns.starts_with(&normalize_message(planned)));
        assert!(similarity_ratio(&normalize_message(planned), &ns) >= 0.9);
    }

    #[test]
    fn match_planned_picks_the_strongest_tier_across_subjects() {
        let subjects = [
            "fix: old commit".to_string(),
            "feat: add parser".to_string(),
        ];
        let res = match_planned("feat: add parser", &subjects);
        assert_eq!(res.tier, MatchTier::Exact);
        assert_eq!(res.subject.as_deref(), Some("feat: add parser"));

        let none = match_planned("feat: unrelated", &["fix: old commit".to_string()]);
        assert_eq!(none.tier, MatchTier::None);
        assert_eq!(none.subject, None);

        let empty = match_planned("feat: anything", &[]);
        assert_eq!(empty.tier, MatchTier::None);
    }

    #[test]
    fn is_row_done_requires_exact_or_similar_candidate_is_not_done() {
        assert!(is_row_done(
            "feat: add parser",
            &["feat: add parser".into()]
        ));
        assert!(is_row_done("feat: add parser", &["feat: add parsr".into()]));
        assert!(!is_row_done(
            "feat: add parser",
            &["feat: add parser and more".into()]
        ));
        assert!(!is_row_done("feat: add parser", &["unrelated".into()]));
    }

    // ---- git facade ----

    #[test]
    fn runner_reports_spawn_failure_without_panicking() {
        let runner = make_git_runner(
            Path::new("/nonexistent-cwd-xyz"),
            "definitely-not-a-git-binary",
        );
        let r = runner(&["rev-parse".into(), "HEAD".into()]);
        assert_eq!(r.status, None);
        assert!(r.stdout.is_empty());
    }

    #[test]
    fn git_commands_degrades_gracefully_on_missing_repo() {
        let git = GitCommands::new(Path::new("/nonexistent-cwd-xyz"));
        assert_eq!(git.head(), None);
        assert!(git.subjects().is_empty());
        assert!(git.status_short().is_empty());
        assert!(!git.is_clean());
    }

    // ---- property-based ----

    fn msg_strategy() -> impl Strategy<Value = String> {
        "[\u{20}-\u{7e}]{0,40}"
    }

    proptest! {
        /// similarity_ratio ∈ [0, 1], and 1.0 exactly on identical strings.
        #[test]
        fn ratio_bounds_and_identity(a in msg_strategy(), b in msg_strategy()) {
            let r = similarity_ratio(&a, &b);
            prop_assert!((0.0..=1.0).contains(&r), "ratio {} out of [0,1]", r);
            // 1.0 iff the two inputs are literally identical (lev 0, incl. empty).
            prop_assert_eq!(r == 1.0, a == b, "ratio({:?},{:?}) == {}", a, b, r);
        }

        /// Tiers form an exclusive partition: the declared tier matches the
        /// specification predicates exactly, and no pair falls in two.
        #[test]
        fn tiers_are_an_exclusive_partition(planned in msg_strategy(), subject in msg_strategy()) {
            let tier = match_tier(&planned, &subject);
            let np = normalize_message(&planned);
            let ns = normalize_message(&subject);
            let ratio = similarity_ratio(&np, &ns);

            let is_exact = np == ns;
            let is_similar = !is_exact && ratio >= SIMILAR_RATIO;
            let is_candidate = !is_exact && !is_similar
                && (ns.starts_with(&np) || ratio >= CANDIDATE_RATIO);
            let is_none = !is_exact && !is_similar && !is_candidate;

            // exactly one predicate fires
            let fires = usize::from(is_exact)
                + usize::from(is_similar)
                + usize::from(is_candidate)
                + usize::from(is_none);
            prop_assert_eq!(fires, 1, "predicates: exact={} similar={} candidate={} none={}", is_exact, is_similar, is_candidate, is_none);

            let expected = if is_exact {
                MatchTier::Exact
            } else if is_similar {
                MatchTier::Similar
            } else if is_candidate {
                MatchTier::Candidate
            } else {
                MatchTier::None
            };
            prop_assert_eq!(tier, expected, "pair ({:?}, {:?}) ratio {}", np, ns, ratio);
        }

        /// is_row_done ⇔ the strongest tier over subjects is exact or similar.
        #[test]
        fn is_row_done_matches_best_tier(
            planned in msg_strategy(),
            subjects in proptest::collection::vec(msg_strategy(), 0..6),
        ) {
            let best = match_planned(&planned, &subjects).tier;
            let done = is_row_done(&planned, &subjects);
            let expect_done = best == MatchTier::Exact || best == MatchTier::Similar;
            prop_assert_eq!(done, expect_done);
            prop_assert_eq!(
                best,
                subjects
                    .iter()
                    .map(|s| (match_tier(&planned, s), s))
                    .max_by_key(|(t, _)| *t)
                    .map(|(t, _)| t)
                    .unwrap_or(MatchTier::None),
                "match_planned must equal the max over subjects (planned {:?})",
                planned,
            );
        }
    }
}
