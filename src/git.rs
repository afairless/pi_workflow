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

/// One commit as the hole detector sees it: identity, planned-message
/// subject, and author time (newest first in [`GitCommands::history`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRef {
    /// Full commit hash (`%H`).
    pub hash: String,
    /// Commit subject (`%s`).
    pub subject: String,
    /// Strict ISO-8601 author date (`%aI`).
    pub authored_at: String,
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

/// Split one `%H%x1f%aI%x1f%s` line into a [`CommitRef`].
///
/// A line without the two unit-separator fields, or with an empty hash, is
/// not a commit record and yields `None`.
fn parse_commit_ref(line: &str) -> Option<CommitRef> {
    let mut parts = line.splitn(3, '\u{1f}');
    let hash = parts.next()?.trim();
    let authored_at = parts.next()?.trim();
    let subject = parts.next()?.trim();
    if hash.is_empty() {
        return None;
    }
    Some(CommitRef {
        hash: hash.to_string(),
        subject: subject.to_string(),
        authored_at: authored_at.to_string(),
    })
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

    /// Commit subjects, newest first — derived from [`Self::history`].
    pub fn subjects(&self) -> Vec<String> {
        self.history().into_iter().map(|c| c.subject).collect()
    }

    /// Commit history, newest first, with hashes and author dates
    /// (`git log --format=%H%x1f%aI%x1f%s --no-decorate`).
    ///
    /// Best-effort: a git failure or an unparseable line is dropped instead of
    /// panicking, so callers treat an empty vector as "no evidence".
    pub fn history(&self) -> Vec<CommitRef> {
        let r = (self.run)(&[
            "log".into(),
            "--format=%H%x1f%aI%x1f%s".into(),
            "--no-decorate".into(),
        ]);
        if !ok(&r) {
            return Vec::new();
        }
        lines(&r.stdout)
            .iter()
            .filter_map(|line| parse_commit_ref(line))
            .collect()
    }

    /// `git show --stat --no-color --format=%H%n%an%n%aI%n%s <hash>` — one
    /// commit's identity plus its diffstat, as the evidence bundle renders
    /// it. Best-effort: `None` on a git failure, so a caller can tell "no
    /// stat" from "a commit with no changes".
    pub fn show_stat(&self, hash: &str) -> Option<String> {
        let r = (self.run)(&[
            "show".into(),
            "--stat".into(),
            "--no-color".into(),
            "--format=%H%n%an%n%aI%n%s".into(),
            hash.to_string(),
        ]);
        if !ok(&r) {
            return None;
        }
        Some(r.stdout)
    }

    /// `git show --patch --no-color --format= <hash>` — one commit's patch
    /// without a commit header, so the caller can prepend its own identity
    /// lines. Best-effort: `None` on a git failure.
    pub fn show_patch(&self, hash: &str) -> Option<String> {
        let r = (self.run)(&[
            "show".into(),
            "--patch".into(),
            "--no-color".into(),
            "--format=".into(),
            hash.to_string(),
        ]);
        if !ok(&r) {
            return None;
        }
        Some(r.stdout)
    }

    /// `git rev-parse <args…>` — the first output line, or `None` when git
    /// fails or prints nothing.
    ///
    /// Arguments are passed verbatim, so a caller can ask for
    /// `--abbrev-ref HEAD` (the branch name), `--abbrev-ref
    /// --symbolic-full-name @{u}` (the upstream ref), `<rev>^2` (the second
    /// parent, i.e. "is a merge") or `<rev>^{tree}`.
    pub fn rev_parse(&self, args: &[&str]) -> Option<String> {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push("rev-parse".to_string());
        argv.extend(args.iter().map(|arg| arg.to_string()));
        let r = (self.run)(&argv);
        if !ok(&r) {
            return None;
        }
        lines(&r.stdout).into_iter().next()
    }

    /// `git config --get <key>` — the trimmed value, or `None` when the key
    /// is unset (git exits 1) or empty.
    pub fn config_get(&self, key: &str) -> Option<String> {
        let r = (self.run)(&["config".into(), "--get".into(), key.to_string()]);
        if !ok(&r) {
            return None;
        }
        let value = r.stdout.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    }

    /// `git rev-list --merges <range>` — the merge commit hashes reachable in
    /// the range.
    ///
    /// `None` when git could not answer, so a caller can tell "no merges"
    /// from "unknown" and fail closed instead of reading a git failure as a
    /// clean range.
    pub fn rev_list_merges(&self, range: &str) -> Option<Vec<String>> {
        let r = (self.run)(&["rev-list".into(), "--merges".into(), range.to_string()]);
        if !ok(&r) {
            return None;
        }
        Some(lines(&r.stdout))
    }

    /// `git rev-parse <rev>^{tree}` — the tree hash of one commit, the
    /// content identity a reword must preserve.
    pub fn show_tree_of(&self, rev: &str) -> Option<String> {
        self.rev_parse(&[&format!("{rev}^{{tree}}")])
    }

    /// `git checkout <target>` — switch to a branch, or detach `HEAD` at a
    /// commit.
    pub fn checkout(&self, target: &str) -> RunResult {
        (self.run)(&["checkout".into(), target.to_string()])
    }

    /// `git checkout --detach <rev>` — detach `HEAD` at a commit without
    /// depending on the rev being a non-branch name.
    pub fn checkout_detach(&self, rev: &str) -> RunResult {
        (self.run)(&["checkout".into(), "--detach".into(), rev.to_string()])
    }

    /// `git commit --amend -q -m <message>` on the checked-out commit.
    pub fn commit_amend(&self, message: &str) -> RunResult {
        (self.run)(&[
            "commit".into(),
            "--amend".into(),
            "-q".into(),
            "-m".into(),
            message.to_string(),
        ])
    }

    /// `git rebase --abort` — abandon an in-progress rebase, restoring the
    /// branch and worktree to their pre-rebase state.
    pub fn rebase_abort(&self) -> RunResult {
        (self.run)(&["rebase".into(), "--abort".into()])
    }

    /// `git rebase --onto <new-base> <upstream> <branch>` — replays
    /// `<upstream>..<branch>` onto `<new-base>`.
    pub fn rebase_onto(&self, new_base: &str, upstream: &str, branch: &str) -> RunResult {
        (self.run)(&[
            "rebase".into(),
            "--onto".into(),
            new_base.to_string(),
            upstream.to_string(),
            branch.to_string(),
        ])
    }

    /// `git branch -f <branch> <rev>` — re-point a branch that is **not**
    /// checked out.
    pub fn branch_force(&self, branch: &str, rev: &str) -> RunResult {
        (self.run)(&[
            "branch".into(),
            "-f".into(),
            branch.to_string(),
            rev.to_string(),
        ])
    }

    /// `git reset --hard <rev>` — move the checked-out branch and discard
    /// tracked worktree changes.
    pub fn reset_hard(&self, rev: &str) -> RunResult {
        (self.run)(&["reset".into(), "--hard".into(), rev.to_string()])
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
    use std::cell::RefCell;
    use std::rc::Rc;

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
        assert!(git.history().is_empty());
        assert_eq!(git.show_stat("abc123"), None);
        assert_eq!(git.show_patch("abc123"), None);
        assert!(git.status_short().is_empty());
        assert!(!git.is_clean());
        assert_eq!(git.rev_parse(&["HEAD"]), None);
        assert_eq!(git.rev_parse(&["--abbrev-ref", "HEAD"]), None);
        assert_eq!(git.config_get("branch.main.remote"), None);
        assert_eq!(git.rev_list_merges("a..b"), None);
        assert_eq!(git.show_tree_of("abc123"), None);
        assert_eq!(git.checkout("abc123").status, None);
        assert_eq!(git.checkout_detach("abc123").status, None);
        assert_eq!(git.commit_amend("feat: x").status, None);
        assert_eq!(git.rebase_abort().status, None);
        assert_eq!(git.rebase_onto("a", "b", "c").status, None);
        assert_eq!(git.branch_force("b", "c").status, None);
        assert_eq!(git.reset_hard("abc123").status, None);
    }

    /// Runner stub that ignores its arguments and returns scripted output.
    fn scripted_runner(
        status: Option<i32>,
        stdout: &'static str,
        args: Rc<RefCell<Vec<Vec<String>>>>,
    ) -> GitRunner {
        Box::new(move |a: &[String]| {
            args.borrow_mut().push(a.to_vec());
            RunResult {
                status,
                stdout: stdout.to_string(),
                stderr: String::new(),
            }
        })
    }

    #[test]
    fn history_parses_hash_date_and_subject_in_order() {
        let stdout = "aaaa\u{1f}2026-11-20T14:43:00+05:00\u{1f}feat: row twelve\n\
                      bbbb\u{1f}2026-11-19T09:00:00+05:00\u{1f}test: row eight suite\n";
        let args: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(0), stdout, Rc::clone(&args)),
        );
        assert_eq!(
            git.history(),
            vec![
                CommitRef {
                    hash: "aaaa".to_string(),
                    subject: "feat: row twelve".to_string(),
                    authored_at: "2026-11-20T14:43:00+05:00".to_string(),
                },
                CommitRef {
                    hash: "bbbb".to_string(),
                    subject: "test: row eight suite".to_string(),
                    authored_at: "2026-11-19T09:00:00+05:00".to_string(),
                },
            ]
        );
        let calls = args.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            vec![
                "log".to_string(),
                "--format=%H%x1f%aI%x1f%s".to_string(),
                "--no-decorate".to_string(),
            ]
        );
    }

    #[test]
    fn history_skips_malformed_lines_and_failures() {
        // A record without both separators and one with an empty hash are
        // dropped; a commit with an empty subject is kept.
        let stdout = "no-separators-here\n\
                      \u{1f}2026-01-01T00:00:00Z\u{1f}orphan\n\
                      cccc\u{1f}2026-01-02T00:00:00Z\u{1f}\n";
        let args: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(0), stdout, Rc::clone(&args)),
        );
        let history = git.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].hash, "cccc");
        assert_eq!(history[0].subject, "");

        let failed: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(
                Some(128),
                "dddd\u{1f}2026-01-01T00:00:00Z\u{1f}feat: x\n",
                failed,
            ),
        );
        assert!(git.history().is_empty());
    }

    #[test]
    fn subjects_is_history_subjects_projected() {
        let stdout = "aaaa\u{1f}2026-11-20T14:43:00+05:00\u{1f}feat: one\n\
                      bbbb\u{1f}2026-11-19T09:00:00+05:00\u{1f}feat: two\n";
        let calls: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));
        let count = Rc::clone(&calls);
        let runner: GitRunner = Box::new(move |_a: &[String]| {
            *count.borrow_mut() += 1;
            RunResult {
                status: Some(0),
                stdout: stdout.to_string(),
                stderr: String::new(),
            }
        });
        let git = GitCommands::with_runner(Path::new("."), runner);
        assert_eq!(git.subjects(), vec!["feat: one", "feat: two"]);
        // `subjects()` goes through `history()` — one git call, not two.
        assert_eq!(*calls.borrow(), 1);
    }

    #[test]
    fn show_stat_and_show_patch_pin_their_argv_and_pass_stdout_through() {
        let args: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(0), "stat body\n", Rc::clone(&args)),
        );
        assert_eq!(git.show_stat("abc123").as_deref(), Some("stat body\n"));
        assert_eq!(
            args.borrow()[0],
            vec![
                "show".to_string(),
                "--stat".to_string(),
                "--no-color".to_string(),
                "--format=%H%n%an%n%aI%n%s".to_string(),
                "abc123".to_string(),
            ]
        );

        let args: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(0), "diff body\n", Rc::clone(&args)),
        );
        assert_eq!(git.show_patch("def456").as_deref(), Some("diff body\n"));
        assert_eq!(
            args.borrow()[0],
            vec![
                "show".to_string(),
                "--patch".to_string(),
                "--no-color".to_string(),
                "--format=".to_string(),
                "def456".to_string(),
            ]
        );
    }

    #[test]
    fn show_stat_and_show_patch_report_failure_as_none() {
        let args: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(128), "fatal: bad object\n", args),
        );
        assert_eq!(git.show_stat("missing"), None);
        assert_eq!(git.show_patch("missing"), None);
    }

    #[test]
    fn rev_parse_pins_its_argv_and_returns_the_first_line() {
        let args: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(0), "agent/reword\nHEAD\n", Rc::clone(&args)),
        );
        assert_eq!(
            git.rev_parse(&["--abbrev-ref", "HEAD"]).as_deref(),
            Some("agent/reword")
        );
        assert_eq!(
            args.borrow()[0],
            vec!["rev-parse", "--abbrev-ref", "HEAD"],
            "argv is `rev-parse` prepended to the caller's arguments"
        );
    }

    #[test]
    fn rev_parse_reports_failure_and_empty_output_as_none() {
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(128), "", Rc::new(RefCell::new(Vec::new()))),
        );
        assert_eq!(git.rev_parse(&["HEAD"]), None);
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(0), "  \n\n", Rc::new(RefCell::new(Vec::new()))),
        );
        assert_eq!(git.rev_parse(&["HEAD"]), None);
    }

    #[test]
    fn config_get_skips_unset_and_empty_values() {
        let args: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(0), "origin\n", Rc::clone(&args)),
        );
        assert_eq!(
            git.config_get("branch.agent/x.remote").as_deref(),
            Some("origin")
        );
        assert_eq!(
            args.borrow()[0],
            vec!["config", "--get", "branch.agent/x.remote"]
        );

        // git exits 1 when the key is unset.
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(1), "", Rc::new(RefCell::new(Vec::new()))),
        );
        assert_eq!(git.config_get("branch.agent/x.remote"), None);
        // A blank value is "unset" too, never `Some("")`.
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(0), " \n", Rc::new(RefCell::new(Vec::new()))),
        );
        assert_eq!(git.config_get("branch.agent/x.remote"), None);
    }

    #[test]
    fn rev_list_merges_returns_hashes_and_none_on_failure() {
        let args: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(0), "m1\nm2\n", Rc::clone(&args)),
        );
        assert_eq!(
            git.rev_list_merges("abc123..HEAD"),
            Some(vec!["m1".to_string(), "m2".to_string()])
        );
        assert_eq!(
            args.borrow()[0],
            vec!["rev-list", "--merges", "abc123..HEAD"]
        );

        // An empty range is `Some(vec![])`; a git failure is `None`.
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(0), "", Rc::new(RefCell::new(Vec::new()))),
        );
        assert_eq!(git.rev_list_merges("abc123..HEAD"), Some(Vec::new()));
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(
                Some(128),
                "fatal: bad revision\n",
                Rc::new(RefCell::new(Vec::new())),
            ),
        );
        assert_eq!(git.rev_list_merges("abc123..HEAD"), None);
    }

    #[test]
    fn show_tree_of_asks_for_the_tree_object() {
        let args: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(0), "tree1\n", Rc::clone(&args)),
        );
        assert_eq!(git.show_tree_of("abc123").as_deref(), Some("tree1"));
        assert_eq!(args.borrow()[0], vec!["rev-parse", "abc123^{tree}"]);
    }

    #[test]
    fn mutating_commands_pin_their_argv_and_pass_the_status_through() {
        let args: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let git = GitCommands::with_runner(
            Path::new("."),
            scripted_runner(Some(0), "", Rc::clone(&args)),
        );
        assert_eq!(git.checkout("abc123").status, Some(0));
        assert_eq!(git.checkout_detach("abc123").status, Some(0));
        assert_eq!(git.commit_amend("feat: reword it").status, Some(0));
        assert_eq!(git.rebase_abort().status, Some(0));
        assert_eq!(git.rebase_onto("new", "abc123", "agent/x").status, Some(0));
        assert_eq!(git.branch_force("agent/x", "new").status, Some(0));
        assert_eq!(git.reset_hard("head1").status, Some(0));
        assert_eq!(
            *args.borrow(),
            vec![
                vec!["checkout", "abc123"],
                vec!["checkout", "--detach", "abc123"],
                vec!["commit", "--amend", "-q", "-m", "feat: reword it"],
                vec!["rebase", "--abort"],
                vec!["rebase", "--onto", "new", "abc123", "agent/x"],
                vec!["branch", "-f", "agent/x", "new"],
                vec!["reset", "--hard", "head1"],
            ]
        );
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
