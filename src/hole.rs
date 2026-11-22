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
use std::path::Path;

use crate::git::{self, CommitRef, GitCommands, MatchTier};
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

// ---- Change 2: the deterministic evidence bundle (B lives here) ----

/// Cap for one candidate commit's `--stat` body (bytes).
pub const MAX_STAT_BYTES: usize = 2 * 1024;
/// Cap for the closest commit's patch (bytes).
pub const MAX_PATCH_BYTES: usize = 8 * 1024;
/// Cap for the small bundle sections (row, diff, successors, deliverables,
/// worktree), and for one row cell.
pub const MAX_SECTION_BYTES: usize = 2 * 1024;
/// Cap for the whole rendered bundle (bytes).
pub const MAX_BUNDLE_BYTES: usize = 24 * 1024;
/// How many candidate commits the bundle carries.
pub const MAX_CANDIDATE_COMMITS: usize = 3;

/// The bundle's sections in fixed order. The order is the renderer's order
/// *and* the order `truncated` reports, so truncation is reproducible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Row,
    Diff,
    Successors,
    CandidateCommits,
    BestPatch,
    Deliverables,
    Worktree,
}

impl Section {
    /// In the fixed section order used by the renderer and `truncated`.
    const ALL: [Section; 7] = [
        Section::Row,
        Section::Diff,
        Section::Successors,
        Section::CandidateCommits,
        Section::BestPatch,
        Section::Deliverables,
        Section::Worktree,
    ];

    fn name(self) -> &'static str {
        match self {
            Section::Row => "row",
            Section::Diff => "diff",
            Section::Successors => "successors",
            Section::CandidateCommits => "candidate_commits",
            Section::BestPatch => "best_patch",
            Section::Deliverables => "deliverables",
            Section::Worktree => "worktree",
        }
    }
}

/// One candidate commit as the bundle shows it: identity plus the capped
/// `git show --stat` body (empty when git could not render it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitStat {
    /// Full commit hash.
    pub hash: String,
    /// Commit subject.
    pub subject: String,
    /// `git show --stat` output for the commit, capped at [`MAX_STAT_BYTES`].
    pub stat: String,
}

/// One `Key deliverables` token, checked against the worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deliverable {
    /// A checkable path that exists under the bundle's cwd.
    Present(String),
    /// A checkable path that does not exist under the bundle's cwd.
    Missing(String),
    /// A token that cannot be interpreted as a single worktree path (a glob,
    /// an index expression, a bare identifier/id). Inconclusive is **not**
    /// missing: the bundle never claims such a deliverable is absent.
    Inconclusive(String),
}

impl Deliverable {
    /// The path or token as written in the plan.
    pub fn text(&self) -> &str {
        match self {
            Deliverable::Present(text)
            | Deliverable::Missing(text)
            | Deliverable::Inconclusive(text) => text,
        }
    }

    /// True only for a path the bundle actually found in the worktree.
    pub fn is_present(&self) -> bool {
        matches!(self, Deliverable::Present(_))
    }
}

/// Everything the verifier is allowed to see (B, the subject-drift report).
///
/// The bundle is **evidence, not a tier**: it changes nothing in Contract 2.
/// Its fields are capped at build time so the bytes handed to an LLM are
/// bounded and reproducible; `truncated` names the trimmed sections in
/// [`Section::ALL`] order.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceBundle {
    /// HEAD at build time.
    pub head: String,
    /// Branch name at build time (the bundle never resolves refs itself).
    pub branch: String,
    /// The hole row, cells verbatim (except per-cell caps).
    pub row: TodoRow,
    /// The subject-drift report for the row's planned message.
    pub diff: SubjectDiff,
    /// Higher-numbered git-matched rows: `(row, hash, subject)`.
    pub successors: Vec<(u64, String, String)>,
    /// Up to [`MAX_CANDIDATE_COMMITS`] `Candidate`/`Similar` commits, newest
    /// first.
    pub candidate_commits: Vec<CommitStat>,
    /// The closest commit's patch, capped at [`MAX_PATCH_BYTES`].
    pub best_patch: Option<String>,
    /// The row's checkable deliverables with their worktree presence.
    pub deliverables: Vec<Deliverable>,
    /// `git status --short` lines, capped.
    pub worktree: Vec<String>,
    /// Sections that were trimmed, in fixed section order.
    pub truncated: Vec<&'static str>,
}

/// The git queries the bundle builder needs. `GitCommands` implements it;
/// tests inject a scripted fake, and the supervise seam can extend `GitFacts`
/// with it without the builder depending on a concrete type.
pub trait BundleFacts {
    /// `git status --short` lines; empty means a clean worktree.
    fn status_short(&self) -> Vec<String>;
    /// `git show --stat` body for `hash`, or `None` when git cannot show it.
    fn show_stat(&self, hash: &str) -> Option<String>;
    /// `git show --patch` body for `hash`, or `None` when git cannot show it.
    fn show_patch(&self, hash: &str) -> Option<String>;
}

impl BundleFacts for GitCommands {
    fn status_short(&self) -> Vec<String> {
        GitCommands::status_short(self)
    }

    fn show_stat(&self, hash: &str) -> Option<String> {
        GitCommands::show_stat(self, hash)
    }

    fn show_patch(&self, hash: &str) -> Option<String> {
        GitCommands::show_patch(self, hash)
    }
}

/// Truncate `text` to at most `max_bytes` bytes **on a UTF-8 character
/// boundary**, appending `…` when anything was dropped. The ellipsis is
/// counted inside the cap, so the result is always ≤ `max_bytes` for the
/// caps used here (all ≥ 4 bytes); a multi-byte character is never split.
pub fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    const ELLIPSIS: char = '…';
    let budget = max_bytes.saturating_sub(ELLIPSIS.len_utf8());
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + ELLIPSIS.len_utf8());
    out.push_str(&text[..end]);
    out.push(ELLIPSIS);
    out
}

/// Expand one simple `dir/{a,b}` group. `None` when the token is malformed,
/// nested, or names a single/empty alternative — an inconclusive token, never
/// a guess. A token without braces is returned unchanged.
fn expand_braces(token: &str) -> Option<Vec<String>> {
    let Some(open) = token.find('{') else {
        return Some(vec![token.to_string()]);
    };
    let rest = &token[open + 1..];
    let close = open + 1 + rest.find('}')?;
    let inner = &token[open + 1..close];
    let suffix = &token[close + 1..];
    if inner.contains(['{', '}']) || suffix.contains(['{', '}']) {
        return None;
    }
    if inner.is_empty() || inner.contains('/') {
        return None;
    }
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() < 2 || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    let prefix = &token[..open];
    Some(
        parts
            .into_iter()
            .map(|part| format!("{prefix}{part}{suffix}"))
            .collect(),
    )
}

/// True when a path token cannot be checked as a single path: empty, or it
/// carries whitespace or a glob/shell metacharacter. Such a token is marked
/// inconclusive rather than "missing".
fn is_uncheckable(token: &str) -> bool {
    token.is_empty()
        || token.chars().any(|c| {
            c.is_whitespace()
                || matches!(
                    c,
                    '*' | '?' | '[' | ']' | '{' | '}' | '\\' | '$' | '`' | '"' | '\''
                )
        })
}

/// Backticked tokens of a `Key deliverables` cell, in cell order.
fn backticked(cell: &str) -> Vec<&str> {
    cell.split('`').skip(1).step_by(2).collect()
}

/// Parse the row's `Key deliverables` cell and check every path against
/// `cwd` (the same tree the verifier reads).
///
/// Backticked tokens containing `/` are paths; simple `dir/{a,b}` groups are
/// expanded deterministically. A token that cannot be interpreted as a path
/// (a glob, an index expression like `STAGES['x']`, a bare id like `AD-48`)
/// is **inconclusive** — never "missing" — and a cell with no qualifying
/// token yields an empty list, which triage reads as "no present path".
/// Tokens are deduplicated in cell order; expanded brace groups each count.
pub fn deliverable_paths(cell: &str, cwd: &Path) -> Vec<Deliverable> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<Deliverable> = Vec::new();
    let mut push = |deliverable: Deliverable, seen: &mut Vec<String>| {
        let text = deliverable.text().to_string();
        if seen.contains(&text) {
            return;
        }
        seen.push(text);
        out.push(deliverable);
    };
    for token in backticked(cell) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if !token.contains('/') {
            push(Deliverable::Inconclusive(token.to_string()), &mut seen);
            continue;
        }
        let Some(paths) = expand_braces(token) else {
            push(Deliverable::Inconclusive(token.to_string()), &mut seen);
            continue;
        };
        if paths.iter().any(|path| is_uncheckable(path)) {
            push(Deliverable::Inconclusive(token.to_string()), &mut seen);
            continue;
        }
        for path in paths {
            let deliverable = if cwd.join(&path).exists() {
                Deliverable::Present(path)
            } else {
                Deliverable::Missing(path)
            };
            push(deliverable, &mut seen);
        }
    }
    out
}

/// Up to [`MAX_CANDIDATE_COMMITS`] commits whose subjects score
/// `Candidate`/`Similar` for the row, newest first. A commit whose stat git
/// cannot render is kept with an empty stat (its subject is still evidence).
fn candidate_commits<G: BundleFacts + ?Sized>(
    git: &G,
    row: &TodoRow,
    history: &[CommitRef],
) -> Vec<CommitStat> {
    let mut candidates = Vec::new();
    for commit in history {
        if candidates.len() >= MAX_CANDIDATE_COMMITS {
            break;
        }
        let tier = git::match_tier(&row.commit_message, &commit.subject);
        if !matches!(tier, MatchTier::Candidate | MatchTier::Similar) {
            continue;
        }
        candidates.push(CommitStat {
            hash: commit.hash.clone(),
            subject: commit.subject.clone(),
            stat: git.show_stat(&commit.hash).unwrap_or_default(),
        });
    }
    candidates
}

/// Build the deterministic evidence bundle for one hole row.
///
/// `head` and `branch` are supplied by the caller (the bundle never resolves
/// refs itself), `history` is the pass's single `subjects()`/`history()`
/// snapshot, and `successors` are the higher-numbered git-matched rows. Every
/// section is capped, then the whole bundle is capped by trimming later
/// sections first, so the output is reproducible and bounded.
pub fn build_bundle<G: BundleFacts + ?Sized>(
    git: &G,
    cwd: &Path,
    row: &TodoRow,
    history: &[CommitRef],
    head: &str,
    branch: &str,
    successors: &[(u64, String, String)],
) -> EvidenceBundle {
    let diff = diff_subject(&row.commit_message, history);
    let mut bundle = EvidenceBundle {
        head: head.to_string(),
        branch: branch.to_string(),
        row: row.clone(),
        candidate_commits: candidate_commits(git, row, history),
        best_patch: diff
            .best_commit
            .as_deref()
            .and_then(|hash| git.show_patch(hash)),
        deliverables: deliverable_paths(&row.deliverables, cwd),
        worktree: git.status_short(),
        successors: successors.to_vec(),
        diff,
        truncated: Vec::new(),
    };
    cap_sections(&mut bundle);
    cap_whole_bundle(&mut bundle);
    bundle
}

/// Record `section` as trimmed, keeping `truncated` duplicate-free.
fn mark_truncated(truncated: &mut Vec<&'static str>, section: Section) {
    if !truncated.contains(&section.name()) {
        truncated.push(section.name());
    }
}

/// Sort `truncated` into the fixed section order.
fn order_truncated(truncated: &mut [&'static str]) {
    let rank = |name: &str| {
        Section::ALL
            .iter()
            .position(|section| section.name() == name)
            .unwrap_or(usize::MAX)
    };
    truncated.sort_by_key(|name| rank(name));
}

/// Apply the per-section caps: the row cells, the token lists, the successors,
/// the candidate stats (2 KiB each), the patch (8 KiB), the deliverables and
/// the worktree. Dropped entries and truncated strings are recorded in
/// `truncated`; nothing else about the bundle changes.
fn cap_sections(bundle: &mut EvidenceBundle) {
    let mut truncated = std::mem::take(&mut bundle.truncated);

    for cell in [
        &mut bundle.row.commit_message,
        &mut bundle.row.logical_unit,
        &mut bundle.row.deliverables,
        &mut bundle.row.tests,
    ] {
        if cell.len() > MAX_SECTION_BYTES {
            *cell = truncate_utf8(cell, MAX_SECTION_BYTES);
            mark_truncated(&mut truncated, Section::Row);
        }
    }
    for text in [&mut bundle.diff.planned, &mut bundle.diff.best_subject] {
        if text.len() > MAX_SECTION_BYTES {
            *text = truncate_utf8(text, MAX_SECTION_BYTES);
            mark_truncated(&mut truncated, Section::Diff);
        }
    }
    for tokens in [
        &mut bundle.diff.extra_tokens,
        &mut bundle.diff.missing_tokens,
    ] {
        let mut total: usize = tokens.iter().map(|token| token.len() + 1).sum();
        while total > MAX_SECTION_BYTES {
            let Some(token) = tokens.pop() else {
                break;
            };
            total -= token.len() + 1;
            mark_truncated(&mut truncated, Section::Diff);
        }
    }

    let mut success = 0usize;
    for (row, hash, subject) in &bundle.successors {
        success += format!("{row} {hash} {subject}\n").len();
    }
    while success > MAX_SECTION_BYTES {
        let Some((row, hash, subject)) = bundle.successors.pop() else {
            break;
        };
        success -= format!("{row} {hash} {subject}\n").len();
        mark_truncated(&mut truncated, Section::Successors);
    }

    for candidate in &mut bundle.candidate_commits {
        if candidate.stat.len() > MAX_STAT_BYTES {
            candidate.stat = truncate_utf8(&candidate.stat, MAX_STAT_BYTES);
            mark_truncated(&mut truncated, Section::CandidateCommits);
        }
    }

    if let Some(patch) = &mut bundle.best_patch
        && patch.len() > MAX_PATCH_BYTES
    {
        *patch = truncate_utf8(patch, MAX_PATCH_BYTES);
        mark_truncated(&mut truncated, Section::BestPatch);
    }

    let mut deliverables = 0usize;
    for deliverable in &bundle.deliverables {
        deliverables += render_deliverable(deliverable).len() + 1;
    }
    while deliverables > MAX_SECTION_BYTES {
        let Some(deliverable) = bundle.deliverables.pop() else {
            break;
        };
        deliverables -= render_deliverable(&deliverable).len() + 1;
        mark_truncated(&mut truncated, Section::Deliverables);
    }

    let mut worktree = 0usize;
    for line in &bundle.worktree {
        worktree += line.len() + 1;
    }
    while worktree > MAX_SECTION_BYTES {
        let Some(line) = bundle.worktree.pop() else {
            break;
        };
        worktree -= line.len() + 1;
        mark_truncated(&mut truncated, Section::Worktree);
    }

    bundle.truncated = truncated;
}

/// Shrink `section` by one deterministic step. Returns `false` when the
/// section has nothing left to give up.
fn trim_section(bundle: &mut EvidenceBundle, section: Section) -> bool {
    match section {
        Section::Row => {
            for cell in [
                &mut bundle.row.deliverables,
                &mut bundle.row.tests,
                &mut bundle.row.logical_unit,
            ] {
                if !cell.is_empty() {
                    cell.clear();
                    return true;
                }
            }
            false
        }
        Section::Diff => {
            if bundle.diff.missing_tokens.pop().is_some() {
                return true;
            }
            if bundle.diff.extra_tokens.pop().is_some() {
                return true;
            }
            for text in [&mut bundle.diff.best_subject, &mut bundle.diff.planned] {
                if !text.is_empty() {
                    text.clear();
                    return true;
                }
            }
            false
        }
        Section::Successors => bundle.successors.pop().is_some(),
        Section::CandidateCommits => bundle.candidate_commits.pop().is_some(),
        Section::BestPatch => match &mut bundle.best_patch {
            Some(patch) if !patch.is_empty() => {
                patch.clear();
                true
            }
            _ => false,
        },
        Section::Deliverables => bundle.deliverables.pop().is_some(),
        Section::Worktree => bundle.worktree.pop().is_some(),
    }
}

/// Enforce the 24 KiB whole-bundle cap by trimming sections from the last one
/// backwards (worktree → deliverables → patch → candidates → successors →
/// diff → row), so the most load-bearing evidence survives longest. Every
/// trimmed section is recorded; the loop is bounded and idempotent.
fn cap_whole_bundle(bundle: &mut EvidenceBundle) {
    let mut guard = 0usize;
    while render_bundle(bundle).len() > MAX_BUNDLE_BYTES && guard < 4096 {
        guard += 1;
        let mut trimmed = false;
        for section in Section::ALL.iter().rev() {
            if !trim_section(bundle, *section) {
                continue;
            }
            mark_truncated(&mut bundle.truncated, *section);
            trimmed = true;
            break;
        }
        if !trimmed {
            break;
        }
    }
    order_truncated(&mut bundle.truncated);
}

/// True when the pair is done at Contract 2's tiers (`Exact` or `Similar`).
fn tier_is_done(tier: MatchTier) -> bool {
    matches!(tier, MatchTier::Exact | MatchTier::Similar)
}

/// What the deterministic triage says to do with a hole row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageOutcome {
    /// The row is git-done, so it cannot be a hole (an internal error: the
    /// caller reports a failed verification, never a skip).
    Impossible,
    /// No candidate commit and no deliverable path present: the row was
    /// plainly never implemented. Escalate without spending a verifier.
    PlainlyUnimplemented,
    /// Evidence is sufficient to spend one contained verifier call.
    RunVerifier,
}

/// The deterministic triage ladder that runs before any verifier spawn.
///
/// 1. A row that is git-done cannot be a hole → [`TriageOutcome::Impossible`].
/// 2. No candidate commit **and** zero present deliverable paths →
///    [`TriageOutcome::PlainlyUnimplemented`]. An inconclusive token is not
///    a present path, so it never buys a verifier call on its own.
/// 3. Otherwise → [`TriageOutcome::RunVerifier`].
///
/// The check is derived from the bundle (never from git again), so triage and
/// the verifier see exactly the same evidence.
pub fn triage(bundle: &EvidenceBundle) -> TriageOutcome {
    if tier_is_done(bundle.diff.tier) {
        return TriageOutcome::Impossible;
    }
    let has_present_deliverable = bundle.deliverables.iter().any(Deliverable::is_present);
    if bundle.candidate_commits.is_empty() && !has_present_deliverable {
        return TriageOutcome::PlainlyUnimplemented;
    }
    TriageOutcome::RunVerifier
}

/// Render one checked deliverable as a bundle line.
fn render_deliverable(deliverable: &Deliverable) -> String {
    match deliverable {
        Deliverable::Present(path) => format!("- `{path}`: present"),
        Deliverable::Missing(path) => format!("- `{path}`: missing"),
        Deliverable::Inconclusive(token) => format!("- `{token}`: inconclusive"),
    }
}

fn tier_name(tier: MatchTier) -> &'static str {
    match tier {
        MatchTier::Exact => "exact",
        MatchTier::Similar => "similar",
        MatchTier::Candidate => "candidate",
        MatchTier::None => "none",
    }
}

fn tokens_or_none(tokens: &[String]) -> String {
    if tokens.is_empty() {
        "(none)".to_string()
    } else {
        tokens.join(" ")
    }
}

/// Render the bundle to the exact bytes the verifier prompt embeds. Pure and
/// deterministic: same bundle, same string. Sections appear in the fixed order
/// row → diff → successors → candidates → patch → deliverables → worktree,
/// and `truncated` (when non-empty) closes the report.
pub fn render_bundle(bundle: &EvidenceBundle) -> String {
    let mut out = String::new();
    out.push_str("# hole evidence bundle\n");
    out.push_str(&format!("row: {}\n", bundle.row.id));
    out.push_str(&format!("head: {}\n", bundle.head));
    out.push_str(&format!("branch: {}\n", bundle.branch));
    out.push_str(&format!("planned: {}\n", bundle.row.commit_message));
    out.push_str(&format!("logical unit: {}\n", bundle.row.logical_unit));
    out.push_str(&format!("deliverables cell: {}\n", bundle.row.deliverables));
    out.push_str(&format!("tests: {}\n", bundle.row.tests));

    out.push_str("\n## subject diff\n");
    out.push_str(&format!("closest: {}\n", bundle.diff.best_subject));
    out.push_str(&format!(
        "commit: {}\n",
        bundle.diff.best_commit.as_deref().unwrap_or("(none)")
    ));
    out.push_str(&format!("tier: {}\n", tier_name(bundle.diff.tier)));
    out.push_str(&format!("ratio: {:.2}\n", bundle.diff.ratio));
    out.push_str(&format!(
        "extra tokens: {}\n",
        tokens_or_none(&bundle.diff.extra_tokens)
    ));
    out.push_str(&format!(
        "missing tokens: {}\n",
        tokens_or_none(&bundle.diff.missing_tokens)
    ));

    out.push_str("\n## successors\n");
    if bundle.successors.is_empty() {
        out.push_str("(none)\n");
    } else {
        for (row, hash, subject) in &bundle.successors {
            out.push_str(&format!("{row} {hash} {subject}\n"));
        }
    }

    out.push_str("\n## candidate commits\n");
    if bundle.candidate_commits.is_empty() {
        out.push_str("(none)\n");
    } else {
        for (index, commit) in bundle.candidate_commits.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str(&format!("### {} {}\n", commit.hash, commit.subject));
            let stat = commit.stat.trim_end();
            if !stat.is_empty() {
                out.push_str(stat);
                out.push('\n');
            }
        }
    }

    out.push_str("\n## best patch\n");
    match &bundle.best_patch {
        Some(patch) if !patch.trim().is_empty() => {
            out.push_str(patch.trim_end());
            out.push('\n');
        }
        _ => out.push_str("(none)\n"),
    }

    out.push_str("\n## deliverables\n");
    if bundle.deliverables.is_empty() {
        out.push_str("(inconclusive; the cell names no backticked path)\n");
    } else {
        for deliverable in &bundle.deliverables {
            out.push_str(&render_deliverable(deliverable));
            out.push('\n');
        }
    }

    out.push_str("\n## worktree\n");
    if bundle.worktree.is_empty() {
        out.push_str("(clean)\n");
    } else {
        for line in &bundle.worktree {
            out.push_str(line);
            out.push('\n');
        }
    }

    if !bundle.truncated.is_empty() {
        out.push_str("\n## truncated\n");
        for name in &bundle.truncated {
            out.push_str(name);
            out.push('\n');
        }
    }

    out
}

/// The verifier's verdict (Contract 3, verifier section). `Complete` is the
/// only skip; every other variant escalates to the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The row's deliverables demonstrably exist as committed work.
    Complete,
    /// The evidence is sufficient to say the work is incomplete.
    Incomplete,
    /// The evidence is insufficient either way.
    Uncertain,
    /// Missing, duplicated, or unparseable marker, or a row mismatch.
    Unparseable,
    /// The verifier could not run at all (spawn error, wall timeout, turn
    /// ceiling, ASK, port error); the string is the reason.
    NotRun(String),
}

/// The five terminal markers of the verifier's output block. They are the
/// only thing `parse_verdict` reads; marker names are case-sensitive so a
/// verifier can never be confused with a row worker's status contract.
pub const VERDICT_MARKER: &str = "PI_HOLE_VERDICT:";
pub const VERDICT_ROW_MARKER: &str = "PI_HOLE_ROW:";
pub const VERDICT_EVIDENCE_MARKER: &str = "PI_HOLE_EVIDENCE:";
pub const VERDICT_MISSING_MARKER: &str = "PI_HOLE_MISSING:";
pub const VERDICT_EXPLANATION_MARKER: &str = "PI_HOLE_EXPLANATION:";

const VERDICT_MARKERS: [&str; 5] = [
    VERDICT_MARKER,
    VERDICT_ROW_MARKER,
    VERDICT_EVIDENCE_MARKER,
    VERDICT_MISSING_MARKER,
    VERDICT_EXPLANATION_MARKER,
];

/// The lines of `text` that are not inside a fenced code block.
fn unfenced_lines(text: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut fence: Option<char> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        match fence {
            Some(ch) => {
                if trimmed.starts_with(&ch.to_string().repeat(3)) {
                    fence = None;
                }
            }
            None => {
                if trimmed.starts_with("```") {
                    fence = Some('`');
                } else if trimmed.starts_with("~~~") {
                    fence = Some('~');
                } else {
                    out.push(line);
                }
            }
        }
    }
    out
}

/// The prose fields of the verifier's terminal block: everything the operator
/// is shown besides the verdict itself. Empty when the block was not parseable
/// (a partially trusted block is never partially displayed).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerdictFields {
    /// `PI_HOLE_EVIDENCE: a; b` split on `;`, trimmed, empties dropped.
    pub evidence: Vec<String>,
    /// `PI_HOLE_MISSING` verbatim (`none` is preserved, not normalized).
    pub missing: Option<String>,
    /// `PI_HOLE_EXPLANATION` verbatim (the operator-facing prose).
    pub explanation: Option<String>,
}

/// Parse the prose fields of the verifier's terminal block.
///
/// The block is only trusted when [`parse_verdict`] accepted it, so callers
/// pass the same `text`; markers inside a fenced code block are ignored by the
/// same rule, and a duplicated marker drops the field rather than guessing.
pub fn parse_verdict_fields(text: &str) -> VerdictFields {
    let mut seen: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for line in unfenced_lines(text) {
        let trimmed = line.trim_start();
        for marker in VERDICT_MARKERS {
            if let Some(value) = trimmed.strip_prefix(marker) {
                seen.entry(marker)
                    .or_default()
                    .push(value.trim().to_string());
            }
        }
    }
    let exactly_one = |marker: &'static str| -> Option<String> {
        match seen.get(marker).map(Vec::as_slice) {
            Some([only]) if !only.is_empty() => Some(only.clone()),
            _ => None,
        }
    };
    let evidence = exactly_one(VERDICT_EVIDENCE_MARKER)
        .map(|value| {
            value
                .split(';')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    VerdictFields {
        evidence,
        missing: exactly_one(VERDICT_MISSING_MARKER),
        explanation: exactly_one(VERDICT_EXPLANATION_MARKER),
    }
}

/// Parse the verifier's terminal block into a [`Verdict`] for `row`.
///
/// Exactly one occurrence of each of the five markers is required, the verdict
/// value must be one of `COMPLETE`/`INCOMPLETE`/`UNCERTAIN`, its row must equal
/// `row`, and every field must carry a non-empty value. Anything else — a
/// missing, duplicated, or unparseable marker, a row mismatch — is
/// [`Verdict::Unparseable`]: only `Complete` ever skips a row, so every
/// ambiguity falls through to the operator. Markers quoted inside a fenced
/// code block are ignored (prose or a pasted contract must not decide a row),
/// and markers mid-line in ordinary prose do not count.
pub fn parse_verdict(text: &str, row: u64) -> Verdict {
    let mut seen: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for line in unfenced_lines(text) {
        let trimmed = line.trim_start();
        for marker in VERDICT_MARKERS {
            if let Some(value) = trimmed.strip_prefix(marker) {
                seen.entry(marker)
                    .or_default()
                    .push(value.trim().to_string());
            }
        }
    }
    let exactly_one = |marker: &'static str| -> Option<&String> {
        match seen.get(marker).map(Vec::as_slice) {
            Some([only]) if !only.is_empty() => Some(only),
            _ => None,
        }
    };
    let Some(verdict) = exactly_one(VERDICT_MARKER) else {
        return Verdict::Unparseable;
    };
    let Some(row_text) = exactly_one(VERDICT_ROW_MARKER) else {
        return Verdict::Unparseable;
    };
    // The remaining three lines are mandatory too: an incomplete block is an
    // unparseable one, never a partially trusted verdict.
    if exactly_one(VERDICT_EVIDENCE_MARKER).is_none()
        || exactly_one(VERDICT_MISSING_MARKER).is_none()
        || exactly_one(VERDICT_EXPLANATION_MARKER).is_none()
    {
        return Verdict::Unparseable;
    }
    let Ok(parsed_row) = row_text.parse::<u64>() else {
        return Verdict::Unparseable;
    };
    if parsed_row != row {
        return Verdict::Unparseable;
    }
    match verdict.to_ascii_uppercase().as_str() {
        "COMPLETE" => Verdict::Complete,
        "INCOMPLETE" => Verdict::Incomplete,
        "UNCERTAIN" => Verdict::Uncertain,
        _ => Verdict::Unparseable,
    }
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

    // ---- deliverable paths ----

    fn temp_cwd() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("pi-plan-hole-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_file(dir: &Path, path: &str) {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(full, "x").expect("write file");
    }

    #[test]
    fn deliverable_paths_check_presence_and_mark_non_paths_inconclusive() {
        let cwd = temp_cwd();
        write_file(&cwd, "src/hole.rs");
        let cell = "`src/hole.rs`, `src/gone.rs`, `EvidenceBundle`, `AD-48`, \
                    `STAGES['x']`, `docs/guide`";
        assert_eq!(
            deliverable_paths(cell, &cwd),
            vec![
                Deliverable::Present("src/hole.rs".to_string()),
                Deliverable::Missing("src/gone.rs".to_string()),
                Deliverable::Inconclusive("EvidenceBundle".to_string()),
                Deliverable::Inconclusive("AD-48".to_string()),
                Deliverable::Inconclusive("STAGES['x']".to_string()),
                Deliverable::Missing("docs/guide".to_string()),
            ]
        );
    }

    #[test]
    fn deliverable_paths_expand_simple_brace_groups() {
        let cwd = temp_cwd();
        write_file(&cwd, "src/a.rs");
        assert_eq!(
            deliverable_paths("`src/{a,b}.rs`", &cwd),
            vec![
                Deliverable::Present("src/a.rs".to_string()),
                Deliverable::Missing("src/b.rs".to_string()),
            ]
        );
    }

    #[test]
    fn unsupported_braces_and_globs_are_inconclusive_not_missing() {
        let cwd = temp_cwd();
        for token in [
            "`src/{a}.rs`",
            "`src/{a,{b,c}}.rs`",
            "`src/{a,}.rs`",
            "`src/*.rs`",
            "`src/?.rs`",
            "`src/[ab].rs`",
        ] {
            let paths = deliverable_paths(token, &cwd);
            assert_eq!(paths.len(), 1, "token {token}");
            assert!(
                matches!(paths[0], Deliverable::Inconclusive(_)),
                "token {token}: {paths:?}"
            );
        }
    }

    #[test]
    fn deliverable_paths_are_deduplicated_in_cell_order() {
        let cwd = temp_cwd();
        write_file(&cwd, "src/a.rs");
        assert_eq!(
            deliverable_paths("`src/a.rs`, `src/a.rs`", &cwd),
            vec![Deliverable::Present("src/a.rs".to_string())]
        );
    }

    #[test]
    fn a_cell_without_backticked_paths_yields_no_deliverables() {
        let cwd = temp_cwd();
        assert!(deliverable_paths("", &cwd).is_empty());
        assert!(deliverable_paths("no tokens here", &cwd).is_empty());
        assert!(deliverable_paths("``", &cwd).is_empty());
    }

    // ---- evidence bundle ----

    #[derive(Default)]
    struct FakeBundle {
        status: Vec<String>,
        stats: BTreeMap<String, String>,
        patches: BTreeMap<String, String>,
    }

    impl BundleFacts for FakeBundle {
        fn status_short(&self) -> Vec<String> {
            self.status.clone()
        }

        fn show_stat(&self, hash: &str) -> Option<String> {
            self.stats.get(hash).cloned()
        }

        fn show_patch(&self, hash: &str) -> Option<String> {
            self.patches.get(hash).cloned()
        }
    }

    fn bundle_for(git: &FakeBundle, row: &TodoRow, history: &[git::CommitRef]) -> EvidenceBundle {
        let cwd = temp_cwd();
        build_bundle(git, &cwd, row, history, "head1", "agent/test", &[])
    }

    fn candidate_row() -> TodoRow {
        row(8, "test: add the decision-time leakage suite")
    }

    fn candidate_history() -> Vec<git::CommitRef> {
        // Prefix matches, so every one is a Contract 2 `Candidate` (a
        // trailing-word insertion can never pull a pair into `Similar`).
        vec![
            commit("c1", "test: add the decision-time leakage suite alpha"),
            commit("c2", "test: add the decision-time leakage suite beta"),
            commit("c3", "test: add the decision-time leakage suite gamma"),
            commit("c4", "test: add the decision-time leakage suite delta"),
            commit("c5", "test: add the decision-time leakage suite epsilon"),
            commit("other", "docs: rewrite the readme"),
        ]
    }

    #[test]
    fn candidate_commits_are_capped_at_three_newest_first() {
        let git = FakeBundle::default();
        let bundle = bundle_for(&git, &candidate_row(), &candidate_history());
        let hashes: Vec<&str> = bundle
            .candidate_commits
            .iter()
            .map(|commit| commit.hash.as_str())
            .collect();
        assert_eq!(hashes, vec!["c1", "c2", "c3"]);
        assert_eq!(
            bundle.candidate_commits[0].subject,
            candidate_history()[0].subject
        );
    }

    #[test]
    fn candidate_stats_are_capped_at_two_kib_and_marked_truncated() {
        let mut git = FakeBundle::default();
        git.stats.insert("c1".to_string(), "é".repeat(2000));
        let bundle = bundle_for(&git, &candidate_row(), &candidate_history());
        let stat = &bundle.candidate_commits[0].stat;
        assert!(stat.len() <= MAX_STAT_BYTES, "stat is {} bytes", stat.len());
        assert!(stat.ends_with('…'));
        assert!(bundle.truncated.contains(&"candidate_commits"));
    }

    #[test]
    fn best_patch_comes_from_the_closest_commit_and_is_capped_at_eight_kib() {
        let mut git = FakeBundle::default();
        git.patches
            .insert("best".to_string(), "diff --git a/x b/x\n".repeat(1000));
        let history = vec![commit("best", "feat: add parser and more")];
        let bundle = bundle_for(&git, &row(1, "feat: add parser"), &history);
        assert_eq!(bundle.diff.best_commit.as_deref(), Some("best"));
        let patch = bundle.best_patch.as_deref().expect("best patch");
        assert!(patch.len() <= MAX_PATCH_BYTES);
        assert!(patch.ends_with('…'));
        assert!(bundle.truncated.contains(&"best_patch"));
    }

    #[test]
    fn a_failed_show_keeps_the_candidate_with_an_empty_stat_and_no_patch() {
        let git = FakeBundle::default();
        let history = vec![commit("c1", "feat: add parser and more")];
        let bundle = bundle_for(&git, &row(1, "feat: add parser"), &history);
        assert_eq!(bundle.candidate_commits.len(), 1);
        assert!(bundle.candidate_commits[0].stat.is_empty());
        assert!(bundle.best_patch.is_none());
    }

    #[test]
    fn pathological_inputs_stay_within_the_caps_and_truncation_is_ordered() {
        let mut git = FakeBundle {
            status: (0..400).map(|i| format!(" M file{i:04}.rs")).collect(),
            ..FakeBundle::default()
        };
        for hash in ["best", "c2", "c3"] {
            git.stats.insert(hash.to_string(), "stat\n".repeat(4000));
            git.patches
                .insert(hash.to_string(), "patch line\n".repeat(2000));
        }
        let mut hole_row = row(1, "feat: add parser");
        hole_row.logical_unit = "unit".repeat(1000);
        hole_row.deliverables = "deliverable cell ".repeat(500);
        hole_row.tests = "test cell ".repeat(500);
        let history = vec![
            commit("best", "feat: add parser and more"),
            commit("c2", "feat: add parser and tests"),
            commit("c3", "feat: add parser and docs"),
        ];
        let bundle = bundle_for(&git, &hole_row, &history);
        assert!(
            render_bundle(&bundle).len() <= MAX_BUNDLE_BYTES,
            "bundle is {} bytes",
            render_bundle(&bundle).len()
        );
        assert!(bundle.truncated.contains(&"worktree"));
        // Fixed section order, whatever order the caps fired in.
        let rank = |name: &str| {
            Section::ALL
                .iter()
                .position(|section| section.name() == name)
                .unwrap()
        };
        let mut previous = 0usize;
        for name in &bundle.truncated {
            let current = rank(name);
            assert!(
                current >= previous,
                "truncated out of order: {:?}",
                bundle.truncated
            );
            previous = current;
        }
    }

    #[test]
    fn render_bundle_is_byte_stable_and_structured() {
        let cwd = temp_cwd();
        write_file(&cwd, "src/hole.rs");
        let mut git = FakeBundle::default();
        git.stats.insert(
            "fc5ea15".to_string(),
            "fc5ea15\nAda\n2026-11-22T10:00:00+00:00\n\
             test: add the decision-time training leakage suite\n\
             \u{20}tests/x.py | 2 +\n"
                .to_string(),
        );
        git.patches.insert(
            "fc5ea15".to_string(),
            "diff --git a/tests/x.py b/tests/x.py\n+test\n".to_string(),
        );
        let mut hole_row = candidate_row();
        hole_row.deliverables = "`src/hole.rs`".to_string();
        let bundle = build_bundle(
            &git,
            &cwd,
            &hole_row,
            &[commit(
                "fc5ea15",
                "test: add the decision-time training leakage suite",
            )],
            "6bf2c8e",
            "agent/todo-hole-detection",
            &[(9, "aaaa".to_string(), "feat: next".to_string())],
        );
        let expected = "# hole evidence bundle\n\
                        row: 8\n\
                        head: 6bf2c8e\n\
                        branch: agent/todo-hole-detection\n\
                        planned: test: add the decision-time leakage suite\n\
                        logical unit: unit\n\
                        deliverables cell: `src/hole.rs`\n\
                        tests: unit\n\
                        \n\
                        ## subject diff\n\
                        closest: test: add the decision-time training leakage suite\n\
                        commit: fc5ea15\n\
                        tier: candidate\n\
                        ratio: 0.82\n\
                        extra tokens: training\n\
                        missing tokens: (none)\n\
                        \n\
                        ## successors\n\
                        9 aaaa feat: next\n\
                        \n\
                        ## candidate commits\n\
                        ### fc5ea15 test: add the decision-time training leakage suite\n\
                        fc5ea15\n\
                        Ada\n\
                        2026-11-22T10:00:00+00:00\n\
                        test: add the decision-time training leakage suite\n\
                        \u{20}tests/x.py | 2 +\n\
                        \n\
                        ## best patch\n\
                        diff --git a/tests/x.py b/tests/x.py\n\
                        +test\n\
                        \n\
                        ## deliverables\n\
                        - `src/hole.rs`: present\n\
                        \n\
                        ## worktree\n\
                        (clean)\n";
        assert_eq!(render_bundle(&bundle), expected);
        // Pure: rendering twice yields the same bytes.
        assert_eq!(render_bundle(&bundle), expected);
    }

    fn manual_bundle() -> EvidenceBundle {
        EvidenceBundle {
            head: "head1".to_string(),
            branch: "agent/test".to_string(),
            row: row(1, "feat: add parser"),
            diff: diff_subject("feat: add parser", &[]),
            successors: Vec::new(),
            candidate_commits: vec![CommitStat {
                hash: "c1".to_string(),
                subject: "feat: add parser and more".to_string(),
                stat: "stat\n".to_string(),
            }],
            best_patch: Some("patch line\n".to_string()),
            deliverables: vec![Deliverable::Present("src/lib.rs".to_string())],
            worktree: (0..2000).map(|i| format!(" M file{i:04}.rs")).collect(),
            truncated: Vec::new(),
        }
    }

    #[test]
    fn the_whole_bundle_cap_trims_later_sections_first() {
        let mut bundle = manual_bundle();
        assert!(render_bundle(&bundle).len() > MAX_BUNDLE_BYTES);
        cap_whole_bundle(&mut bundle);
        assert!(render_bundle(&bundle).len() <= MAX_BUNDLE_BYTES);
        assert_eq!(bundle.truncated, vec!["worktree"]);
        // Later sections give way; earlier evidence is untouched.
        assert!(bundle.worktree.len() < 2000);
        assert_eq!(bundle.candidate_commits.len(), 1);
        assert_eq!(bundle.best_patch.as_deref(), Some("patch line\n"));
    }

    #[test]
    fn truncate_utf8_stays_on_character_boundaries_and_respects_the_cap() {
        let text = "αβγδεζηθικ";
        for cap in [4usize, 7, 10, 20, 100] {
            let cut = truncate_utf8(text, cap);
            assert!(cut.len() <= cap, "cap {cap} produced {} bytes", cut.len());
            // The retained prefix is a real prefix of the input, never a
            // split character.
            let prefix = cut.trim_end_matches('…');
            assert!(text.starts_with(prefix));
        }
        assert_eq!(truncate_utf8("short", 10), "short");
    }

    // ---- triage ----

    fn triage_bundle(
        git: &FakeBundle,
        row: &TodoRow,
        history: &[git::CommitRef],
    ) -> EvidenceBundle {
        bundle_for(git, row, history)
    }

    #[test]
    fn triage_refuses_a_git_done_row_as_impossible() {
        let git = FakeBundle::default();
        let history = vec![commit("exact", "feat: add parser")];
        let bundle = triage_bundle(&git, &row(1, "feat: add parser"), &history);
        assert_eq!(bundle.diff.tier, MatchTier::Exact);
        assert_eq!(triage(&bundle), TriageOutcome::Impossible);

        let history = vec![commit("similar", "feat: add parsr")];
        let bundle = triage_bundle(&git, &row(1, "feat: add parser"), &history);
        assert_eq!(bundle.diff.tier, MatchTier::Similar);
        assert_eq!(triage(&bundle), TriageOutcome::Impossible);
    }

    #[test]
    fn triage_calls_an_empty_evidence_hole_plainly_unimplemented() {
        let cwd = temp_cwd();
        let git = FakeBundle::default();
        let mut hole_row = row(8, "feat: add parser");
        hole_row.deliverables = "`src/nowhere.rs`, `AD-48`".to_string();
        let bundle = build_bundle(&git, &cwd, &hole_row, &[], "head1", "agent/test", &[]);
        assert_eq!(triage(&bundle), TriageOutcome::PlainlyUnimplemented);

        // A cell with no checkable path at all is also plainly unimplemented.
        let mut hole_row = row(8, "feat: add parser");
        hole_row.deliverables = "no paths".to_string();
        let bundle = build_bundle(&git, &cwd, &hole_row, &[], "head1", "agent/test", &[]);
        assert_eq!(triage(&bundle), TriageOutcome::PlainlyUnimplemented);
    }

    #[test]
    fn triage_runs_the_verifier_on_a_candidate_commit_or_a_present_deliverable() {
        let git = FakeBundle::default();
        let history = vec![commit(
            "c1",
            "test: add the decision-time training leakage suite",
        )];
        let bundle = triage_bundle(&git, &candidate_row(), &history);
        assert!(!bundle.candidate_commits.is_empty());
        assert_eq!(triage(&bundle), TriageOutcome::RunVerifier);

        // No candidate commit, but the deliverable exists in the tree.
        let cwd = temp_cwd();
        write_file(&cwd, "tests/test_decision_training_leakage.py");
        let mut hole_row = row(8, "feat: add parser");
        hole_row.deliverables = "`tests/test_decision_training_leakage.py`".to_string();
        let bundle = build_bundle(&git, &cwd, &hole_row, &[], "head1", "agent/test", &[]);
        assert!(bundle.candidate_commits.is_empty());
        assert_eq!(triage(&bundle), TriageOutcome::RunVerifier);
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

    // ---- verdict parsing ----

    /// A complete terminal block with `verdict` for `row`, optionally
    /// preceded by arbitrary prose.
    fn block(verdict: &str, row: u64) -> String {
        format!(
            "Some prose about the row.\n\n\
             PI_HOLE_VERDICT: {verdict}\n\
             PI_HOLE_ROW: {row}\n\
             PI_HOLE_EVIDENCE: src/hole.rs:1; commit abc123\n\
             PI_HOLE_MISSING: none\n\
             PI_HOLE_EXPLANATION: The deliverable exists.\n"
        )
    }

    #[test]
    fn parse_verdict_maps_the_three_answers_for_a_matching_row() {
        assert_eq!(parse_verdict(&block("COMPLETE", 8), 8), Verdict::Complete);
        assert_eq!(
            parse_verdict(&block("INCOMPLETE", 8), 8),
            Verdict::Incomplete
        );
        assert_eq!(parse_verdict(&block("UNCERTAIN", 8), 8), Verdict::Uncertain);
        // Lowercase answers are the same answer.
        assert_eq!(parse_verdict(&block("complete", 8), 8), Verdict::Complete);
    }

    #[test]
    fn parse_verdict_rejects_a_row_mismatch() {
        assert_eq!(
            parse_verdict(&block("COMPLETE", 9), 8),
            Verdict::Unparseable
        );
    }

    #[test]
    fn parse_verdict_rejects_missing_markers() {
        let full = block("COMPLETE", 8);
        for marker in VERDICT_MARKERS {
            let without: String = full
                .lines()
                .filter(|line| !line.trim_start().starts_with(marker))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                parse_verdict(&without, 8),
                Verdict::Unparseable,
                "marker {marker} must be mandatory"
            );
        }
    }

    #[test]
    fn parse_verdict_rejects_duplicate_markers_even_when_they_agree() {
        let duplicated = format!("{}{}", block("COMPLETE", 8), block("COMPLETE", 8));
        assert_eq!(parse_verdict(&duplicated, 8), Verdict::Unparseable);
        let duplicated_row = format!("{}PI_HOLE_ROW: 8\n", block("COMPLETE", 8));
        assert_eq!(parse_verdict(&duplicated_row, 8), Verdict::Unparseable);
    }

    #[test]
    fn parse_verdict_rejects_unparseable_values() {
        // Unknown answer word.
        assert_eq!(parse_verdict(&block("MAYBE", 8), 8), Verdict::Unparseable);
        // The contract's enumeration copied verbatim is not an answer.
        assert_eq!(
            parse_verdict(&block("COMPLETE | INCOMPLETE | UNCERTAIN", 8), 8),
            Verdict::Unparseable
        );
        // A row that is not a bare number.
        assert_eq!(
            parse_verdict(
                &block("COMPLETE", 8).replace("PI_HOLE_ROW: 8", "PI_HOLE_ROW: row 8"),
                8
            ),
            Verdict::Unparseable
        );
        // An empty field value.
        assert_eq!(
            parse_verdict(
                &block("COMPLETE", 8).replace("PI_HOLE_MISSING: none", "PI_HOLE_MISSING:"),
                8
            ),
            Verdict::Unparseable
        );
    }

    #[test]
    fn parse_verdict_ignores_fake_markers_inside_a_code_fence() {
        // The fenced pasted contract says COMPLETE; the real answer is
        // INCOMPLETE and must win.
        let text = format!(
            "Here is the block I was asked to produce:\n\n\
             ```text\n\
             PI_HOLE_VERDICT: COMPLETE\n\
             PI_HOLE_ROW: 8\n\
             PI_HOLE_EVIDENCE: fake\n\
             PI_HOLE_MISSING: none\n\
             PI_HOLE_EXPLANATION: fake\n\
             ```\n\n\
             {}",
            block("INCOMPLETE", 8)
        );
        assert_eq!(parse_verdict(&text, 8), Verdict::Incomplete);
        // A file-quoted marker in prose (mid-line) never counts either.
        let mid_line = format!(
            "The string PI_HOLE_VERDICT: COMPLETE appeared in a README.\n\n{}",
            block("UNCERTAIN", 8)
        );
        assert_eq!(parse_verdict(&mid_line, 8), Verdict::Uncertain);
        // Fenced only → nothing to parse.
        let fenced_only = "```\nPI_HOLE_VERDICT: COMPLETE\nPI_HOLE_ROW: 8\n```\n";
        assert_eq!(parse_verdict(fenced_only, 8), Verdict::Unparseable);
        // Tilde fences are fences too.
        let tilde = format!(
            "~~~\nPI_HOLE_VERDICT: COMPLETE\n~~~\n\n{}",
            block("UNCERTAIN", 8)
        );
        assert_eq!(parse_verdict(&tilde, 8), Verdict::Uncertain);
    }

    #[test]
    fn parse_verdict_tolerates_leading_whitespace_and_crlf() {
        let indented = format!("  {}", block("COMPLETE", 8));
        assert_eq!(parse_verdict(&indented, 8), Verdict::Complete);
        let crlf = block("COMPLETE", 8).replace('\n', "\r\n");
        assert_eq!(parse_verdict(&crlf, 8), Verdict::Complete);
    }

    #[test]
    fn parse_verdict_of_empty_text_is_unparseable() {
        assert_eq!(parse_verdict("", 8), Verdict::Unparseable);
        assert_eq!(parse_verdict("no markers here", 8), Verdict::Unparseable);
    }

    #[test]
    fn parse_verdict_fields_reads_the_prose_block() {
        let fields = parse_verdict_fields(&block("INCOMPLETE", 8));
        assert_eq!(
            fields.evidence,
            vec!["src/hole.rs:1".to_string(), "commit abc123".to_string()],
            "evidence splits on semicolons and trims"
        );
        assert_eq!(fields.missing.as_deref(), Some("none"));
        assert_eq!(
            fields.explanation.as_deref(),
            Some("The deliverable exists.")
        );
    }

    #[test]
    fn parse_verdict_fields_ignores_fenced_or_duplicated_markers() {
        // A fenced pasted block never supplies fields; the unfenced block does.
        let text = format!(
            "```\nPI_HOLE_EXPLANATION: fake\n```\n\n{}",
            block("COMPLETE", 8)
        );
        let fields = parse_verdict_fields(&text);
        assert_eq!(
            fields.explanation.as_deref(),
            Some("The deliverable exists.")
        );

        // A duplicated explanation is ambiguous → dropped, never guessed.
        let duplicated = format!(
            "{}PI_HOLE_EXPLANATION: second opinion\n",
            block("COMPLETE", 8)
        );
        assert_eq!(parse_verdict_fields(&duplicated).explanation, None);
        // An empty evidence field yields no evidence entries.
        let empty = block("COMPLETE", 8).replace(
            "PI_HOLE_EVIDENCE: src/hole.rs:1; commit abc123",
            "PI_HOLE_EVIDENCE:",
        );
        assert!(parse_verdict_fields(&empty).evidence.is_empty());
        assert!(parse_verdict_fields("").evidence.is_empty());
    }

    #[test]
    fn only_complete_ever_skips() {
        // Pin the escalation invariant: exactly one variant can skip a row.
        for verdict in [
            Verdict::Incomplete,
            Verdict::Uncertain,
            Verdict::Unparseable,
            Verdict::NotRun("spawn failed".to_string()),
        ] {
            assert_ne!(verdict, Verdict::Complete);
        }
    }
}
