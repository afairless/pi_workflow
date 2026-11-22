//! Real-git integration tests for the deterministic reword: one scratch
//! repository per test, seeded with a drifted target and later commits, then
//! driven through the production `GitCommands` implementation.
//!
//! The unit tests in `src/hole.rs` script a fake runner; these prove the same
//! sequence against git itself: the tree is unchanged, only the drifted
//! subject moves, the later subjects survive their replay, and every guard
//! refuses on a real repository.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use pi_plan::git::GitCommands;
use pi_plan::hole::{RewordOutcome, diff_subject, offer_reword, reword};
use pi_plan::todo::TodoRow;

static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

/// The planned message row 8 should have committed with.
const PLANNED: &str = "test: add the decision-time leakage suite";
/// What the target commit actually says (one inserted word).
const DRIFTED: &str = "test: add the decision-time leakage suite thoroughly";

/// A fresh scratch repository on `agent/reword`, removed on drop.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// `git init` + a first, non-row commit (so a repository exists even if a
    /// test never seeds rows).
    fn new(name: &str) -> Scratch {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "pi-plan-reword-git-{}-{name}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let scratch = Scratch { dir };
        scratch.git(&["init", "--quiet", "--initial-branch", "agent/reword"]);
        scratch.git(&["config", "user.name", "Reword Test"]);
        scratch.git(&["config", "user.email", "reword@example.invalid"]);
        scratch.git(&["config", "commit.gpgsign", "false"]);
        scratch.git(&["config", "tag.gpgsign", "false"]);
        scratch
    }

    fn git_commands(&self) -> GitCommands {
        GitCommands::new(self.dir.as_path())
    }

    /// Run git, panicking with its stderr on failure.
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.dir.as_path())
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Commit one file with a subject, returning the new commit hash.
    fn commit(&self, file: &str, content: &str, subject: &str) -> String {
        std::fs::write(self.dir.join(file), content).expect("write file");
        self.git(&["add", file]);
        self.git(&["commit", "--quiet", "-m", subject]);
        self.git(&["rev-parse", "HEAD"]).trim().to_string()
    }

    /// The incident history: three done rows, the drifted row 8, then two
    /// later rows. Returns `(target, later)` — the drifted commit's hash and
    /// the later commits' hashes, oldest first.
    fn seed_incident(&self) -> (String, Vec<String>) {
        self.commit("row1.txt", "one\n", "docs: write the first row");
        self.commit("row2.txt", "two\n", "docs: write the second row");
        self.commit("row3.txt", "three\n", "docs: write the third row");
        let target = self.commit("row8.txt", "eight\n", DRIFTED);
        let later = vec![
            self.commit("row9.txt", "nine\n", "docs: write the ninth row"),
            self.commit("row10.txt", "ten\n", "docs: write the tenth row"),
        ];
        (target, later)
    }

    /// The full subject list, newest first, at the checked-out branch tip.
    fn subjects(&self) -> Vec<String> {
        self.git(&["log", "--format=%s"])
            .lines()
            .map(String::from)
            .collect()
    }

    /// The tree hash at the branch tip.
    fn tip_tree(&self) -> String {
        self.git(&["rev-parse", "HEAD^{tree}"]).trim().to_string()
    }

    /// The row 8 the plan describes.
    fn row(&self) -> TodoRow {
        TodoRow {
            id: "8".to_string(),
            number: 8,
            commit_message: PLANNED.to_string(),
            logical_unit: "Reword sequence".to_string(),
            deliverables: "src/hole.rs".to_string(),
            tests: "unit, integration".to_string(),
        }
    }

    /// `offer_reword` over the repository's own history, the way the operator
    /// action calls it.
    fn offer(&self) -> Result<pi_plan::hole::RewordOffer, String> {
        let git = self.git_commands();
        let history = git.history();
        let row = self.row();
        let diff = diff_subject(&row.commit_message, &history);
        offer_reword(&git, &row, &diff, &history)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// True when any subject matches the planned message exactly.
fn has_subject(subjects: &[String], subject: &str) -> bool {
    subjects.iter().any(|s| s == subject)
}

#[test]
fn reword_rewrites_only_the_drifted_subject_and_preserves_every_tree() {
    let scratch = Scratch::new("success");
    let (target, later) = scratch.seed_incident();
    let before_tree = scratch.tip_tree();
    let before_subjects = scratch.subjects();

    let git = scratch.git_commands();
    let offer = scratch.offer().expect("the guards pass");
    assert_eq!(offer.target, target);
    assert_eq!(offer.planned, PLANNED);
    assert_eq!(offer.branch, "agent/reword");
    assert_eq!(offer.later_commits, later.len());

    let outcome = reword(&git, &offer).expect("reword");
    let RewordOutcome::Reworded {
        new_head,
        new_target,
    } = outcome
    else {
        panic!("expected Reworded, got {outcome:?}");
    };

    // A fresh hash for the amended commit, and a fresh branch tip for the
    // replay.
    assert_ne!(new_target, target, "the amend must make a fresh commit");
    let head = scratch.git(&["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(new_head, head);
    assert_eq!(
        scratch.git(&["rev-parse", "agent/reword"]).trim(),
        new_head,
        "the branch was restored to the replay's tip"
    );
    assert_ne!(
        new_head.as_str(),
        later.last().expect("later commit").as_str(),
        "the later commits have new hashes"
    );

    // Content is byte-identical: the reword changed a message, nothing else.
    assert_eq!(
        scratch
            .git(&["rev-parse", &format!("{new_target}^{{tree}}")])
            .trim(),
        scratch
            .git(&["rev-parse", &format!("{target}^{{tree}}")])
            .trim()
    );
    assert_eq!(
        scratch.tip_tree(),
        before_tree,
        "the branch tip tree is unchanged"
    );

    // Exactly the one subject moved; the later subjects survived unchanged
    // and in order, and the earlier rows are untouched.
    let after_subjects = scratch.subjects();
    assert_eq!(after_subjects.len(), before_subjects.len());
    assert!(has_subject(&after_subjects, PLANNED));
    assert!(!has_subject(&after_subjects, DRIFTED));
    for subject in &before_subjects {
        if subject == DRIFTED {
            continue;
        }
        assert!(
            after_subjects.iter().any(|s| s == subject),
            "subject `{subject}` disappeared: {after_subjects:?}"
        );
    }
    // Newest first: the two later rows, the planned row, then the done rows.
    assert_eq!(
        &after_subjects[0..3],
        &[
            "docs: write the tenth row".to_string(),
            "docs: write the ninth row".to_string(),
            PLANNED.to_string(),
        ]
    );

    // The rewritten commit is a plain commit (one parent), as before.
    let parents = scratch.git(&["rev-list", "--parents", "-n", "1", &new_target]);
    assert_eq!(parents.split_whitespace().count(), 2);
}

#[test]
fn main_is_a_protected_branch() {
    let scratch = Scratch::new("main");
    scratch.seed_incident();
    scratch.git(&["branch", "-m", "agent/reword", "main"]);
    let err = scratch.offer().expect_err("main is refused");
    assert_eq!(err, "`main` is a protected branch name");
}

#[test]
fn an_upstream_remote_refuses() {
    let scratch = Scratch::new("upstream");
    scratch.seed_incident();
    scratch.git(&["config", "branch.agent/reword.remote", "origin"]);
    let err = scratch.offer().expect_err("a published branch is refused");
    assert_eq!(err, "the branch has an upstream remote");
}

#[test]
fn a_dirty_worktree_refuses() {
    let scratch = Scratch::new("dirty");
    scratch.seed_incident();
    std::fs::write(scratch.dir.join("untracked.txt"), "notes\n").expect("write");
    let err = scratch.offer().expect_err("a dirty tree is refused");
    assert_eq!(err, "the worktree has uncommitted changes");
}

#[test]
fn a_merge_after_the_target_refuses() {
    let scratch = Scratch::new("merge-after");
    scratch.seed_incident();
    // Side branch off the target, advanced and merged back: the range
    // `target..HEAD` now contains a merge commit.
    let target = scratch.git(&["rev-parse", "HEAD~2"]).trim().to_string();
    scratch.git(&["checkout", "--quiet", "-b", "side", &target]);
    scratch.commit("side.txt", "side\n", "docs: side work");
    scratch.git(&["checkout", "--quiet", "agent/reword"]);
    scratch.git(&["merge", "--quiet", "--no-ff", "-m", "merge side", "side"]);
    let err = scratch.offer().expect_err("a merge in range is refused");
    assert_eq!(err, "the commits after the target include merge commits");
}

#[test]
fn a_merge_target_refuses() {
    let scratch = Scratch::new("merge-target");
    // The drifted commit is itself a merge: side branch first, then merge it
    // with the planned subject drifting.
    scratch.commit("row1.txt", "one\n", "docs: write the first row");
    scratch.git(&["checkout", "--quiet", "-b", "side"]);
    scratch.commit("side.txt", "side\n", "docs: side work");
    scratch.git(&["checkout", "--quiet", "agent/reword"]);
    scratch.commit("main.txt", "main\n", "docs: main work");
    scratch.git(&["merge", "--quiet", "--no-ff", "-m", DRIFTED, "side"]);
    // A later commit so the row is a hole, not a trailing row.
    scratch.commit("row10.txt", "ten\n", "docs: write the tenth row");

    let err = scratch.offer().expect_err("a merge target is refused");
    assert_eq!(err, "the target commit is a merge commit");
}

#[test]
fn a_detached_head_refuses() {
    let scratch = Scratch::new("detached");
    scratch.seed_incident();
    scratch.git(&["checkout", "--quiet", "--detach", "HEAD"]);
    let err = scratch.offer().expect_err("a detached head is refused");
    assert_eq!(err, "HEAD is detached (no branch to restore)");
}

#[test]
fn an_exact_match_leaves_nothing_to_reword() {
    let scratch = Scratch::new("exact");
    scratch.seed_incident();
    // A plan whose row is already exact: the nearest subject equals it.
    let git = scratch.git_commands();
    let history = git.history();
    let mut row = scratch.row();
    row.commit_message = DRIFTED.to_string();
    let diff = diff_subject(&row.commit_message, &history);
    let err = offer_reword(&git, &row, &diff, &history).expect_err("nothing to reword");
    assert_eq!(
        err,
        "the planned message already matches the closest commit"
    );
}

#[test]
fn scratch_directories_are_isolated() {
    let first = Scratch::new("isolation");
    let second = Scratch::new("isolation");
    assert_ne!(first.dir, second.dir);
    assert!(first.dir.is_dir());
    assert!(second.dir.is_dir());
}
