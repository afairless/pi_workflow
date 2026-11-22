//! External per-project run-state root (`~/.pi-plan/<project-key>/`).
//!
//! Every entry point (`supervise` / `status` / `stop` / `mark`) resolves the
//! SAME project key from the working directory, so one run's state is found
//! by every consumer and the project directory stays clean: no
//! `supervisor-state.json`, no `.pi-plan/`, no `.pi-plan-stop` ever appear
//! in the repo. The base directory is `$PI_PLAN_STATE_DIR` when set
//! (test/portability override), else `$HOME/.pi-plan`; when neither exists
//! the resolver hard-errors — there is deliberately NO silent fallback into
//! the project directory.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Default base directory name under `$HOME`.
pub const DEFAULT_BASE_NAME: &str = ".pi-plan";

/// Environment override for the base directory (tests / portability).
pub const STATE_DIR_ENV: &str = "PI_PLAN_STATE_DIR";

/// Length of the sha256 prefix used in the project key.
const KEY_HASH_CHARS: usize = 8;

/// Namespace for the per-project run-state root resolver. All functions are
/// associated so the pure path math is unit-testable without touching the
/// filesystem beyond the canonicalize probe.
pub struct ProjectStorage {}

impl ProjectStorage {
    /// Resolve the per-project run-state root for `cwd`.
    ///
    /// Precedence: `$PI_PLAN_STATE_DIR` (passed in as `state_dir_override`)
    /// > `$HOME/.pi-plan` > hard error. `cwd` is canonicalized first
    /// > (`fs::canonicalize`, falling back to the absolute `current_dir()`
    /// > on failure) so symlinked views of one project share a key; the key
    /// > is `<sanitized-basename>-<sha256(canonical cwd)[..8]>`, with
    /// > `root-<hash>` when the basename is empty (filesystem root).
    pub fn resolve(
        home: Option<&str>,
        state_dir_override: Option<&str>,
        cwd: &Path,
    ) -> Result<PathBuf, String> {
        let base = base_dir(home, state_dir_override)?;
        let canonical = canonical_cwd(cwd)?;
        let key = project_key(&canonical)?;
        Ok(base.join(key.as_str()))
    }
}

/// The base directory the project keys live under: the `$PI_PLAN_STATE_DIR`
/// override wins when set (even over a valid HOME), else `$HOME/.pi-plan`.
/// `None` on both sides is a hard error — never a silent fallback.
fn base_dir(home: Option<&str>, state_dir_override: Option<&str>) -> Result<PathBuf, String> {
    if let Some(dir) = state_dir_override.filter(|d| !d.is_empty()) {
        return Ok(Path::new(dir).to_path_buf());
    }
    if let Some(h) = home.filter(|h| !h.is_empty()) {
        return Ok(Path::new(h).join(DEFAULT_BASE_NAME).to_path_buf());
    }
    Err(format!(
        "cannot resolve the run-state directory: HOME is not set (set HOME or {STATE_DIR_ENV})"
    ))
}

/// A canonical, absolute form of the working directory: `fs::canonicalize`
/// wins; on failure (an unlinkable/exotic cwd) the process's own absolute
/// `current_dir()` is the fallback, matching how `main` first obtains it.
fn canonical_cwd(cwd: &Path) -> Result<PathBuf, String> {
    match fs::canonicalize(cwd) {
        Ok(path) => Ok(path),
        Err(_) => match std::env::current_dir() {
            Ok(path) => Ok(path),
            Err(err) => Err(format!("cannot resolve the working directory: {err}")),
        },
    }
}

/// The project key: `<sanitized-basename(canonical)>-<sha256[..8]>`.
/// A basename of `""` (the filesystem root) falls back to `root-<hash>` so
/// the key is never a bare hash prefix.
pub fn project_key(canonical: &Path) -> Result<String, String> {
    let basename = basename_of(canonical);
    let hash = key_hash_of(canonical);
    if basename.is_empty() {
        Ok(format!("root-{hash}"))
    } else {
        Ok(format!("{}-{hash}", sanitize_name(basename.as_str())))
    }
}

/// The final path segment of `path` as a `String` (`""` for the filesystem
/// root).
fn basename_of(path: &Path) -> String {
    let owned = path.to_string_lossy().into_owned();
    match owned.as_str().split('/').next_back() {
        Some(segment) => segment.to_string(),
        None => String::new(),
    }
}

/// Map every char outside `[A-Za-z0-9._-]` to `_` — readable, collision-safe
/// directory names (no whitespace, no path separators, no shell metachar).
fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphabetic() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

/// First `KEY_HASH_CHARS` hex chars of sha256 over the canonical path's
/// string form — the collision guard inside the project key.
fn key_hash_of(canonical: &Path) -> String {
    let digest = Sha256::digest(canonical.to_string_lossy().into_owned().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex.as_str()[0..KEY_HASH_CHARS].to_string()
}

/// File name of the durable per-worker stats audit log under the run-state
/// root (`~/.pi-plan/<key>/worker-stats.jsonl`).
pub const WORKER_STATS_FILE_NAME: &str = "worker-stats.jsonl";

/// File name of the durable, append-only hole/verifier decision log under the
/// run-state root (`~/.pi-plan/<key>/decisions.jsonl`). It is the surviving
/// audit trail once `supervisor-state.json` is cleared at plan completion.
pub const DECISIONS_FILE_NAME: &str = "decisions.jsonl";

/// File name of the durable permission store under the run-state root
/// (`~/.pi-plan/<key>/permissions.json`). The resolver hook for the
/// supervisor's session-grant memory: the store path is
/// `root.join(PERMISSIONS_FILE_NAME)` on the SAME resolved root as
/// `supervisor-state.json` / `worker-stats.jsonl`, so it is per-project
/// and honors `$PI_PLAN_STATE_DIR` like every other run-state file (the
/// permissions leaf owns the load/save/dedupe semantics).
pub const PERMISSIONS_FILE_NAME: &str = "permissions.json";

/// One run attempt's statistics as persisted to `worker-stats.jsonl`.
/// `"v": 1` up front is an explicit schema marker so a later field
/// addition is detectable by version rather than by guesswork.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerStatsRecord {
    /// Schema marker (`1`).
    pub v: u64,
    /// Plan row number.
    pub row: u64,
    /// 1-based attempt within the row's budget.
    pub attempt: u32,
    pub agent_id: String,
    /// Outcome-kind label, matching the report's per-attempt label.
    pub outcome: String,
    pub cost: Option<f64>,
    pub tokens: Option<u64>,
    pub context_percent: Option<f64>,
    pub context_window: Option<u64>,
    pub turns: u32,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub transcript: Option<String>,
}

/// Best-effort, atomic append of one record to `<root>/worker-stats.jsonl`
/// — the same write-temp-then-rename discipline as `save_state_file`, so
/// an interrupted run cannot corrupt the audit log.
pub fn append_worker_stats(root: &Path, record: &WorkerStatsRecord) {
    append_jsonl(root, WORKER_STATS_FILE_NAME, record);
}

/// The pinned decision kinds of the hole/verifier audit log. Serialized in
/// kebab-case so the on-disk vocabulary is exactly
/// `hole-verified | hole-escalated | verifier-failed | operator-complete |
/// operator-work | operator-reword | reword-failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionKind {
    /// The verifier authoritatively skipped a hole row.
    HoleVerified,
    /// A hole was handed to the operator (triage or a non-`COMPLETE` verdict).
    HoleEscalated,
    /// The verifier could not be run or its verdict could not be parsed.
    VerifierFailed,
    /// The operator marked the row done (`adjudicated`).
    OperatorComplete,
    /// The operator asked for the row worker to run once.
    OperatorWork,
    /// The operator asked for the drifted commit subject to be reworded.
    OperatorReword,
    /// A sanctioned reword attempt failed and the repository was restored.
    RewordFailed,
}

impl DecisionKind {
    /// The on-disk label of this kind (the serde kebab-case form).
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionKind::HoleVerified => "hole-verified",
            DecisionKind::HoleEscalated => "hole-escalated",
            DecisionKind::VerifierFailed => "verifier-failed",
            DecisionKind::OperatorComplete => "operator-complete",
            DecisionKind::OperatorWork => "operator-work",
            DecisionKind::OperatorReword => "operator-reword",
            DecisionKind::RewordFailed => "reword-failed",
        }
    }
}

/// Supervisor-authored facts about one verifier spawn, embedded in a decision
/// record. Every field is optional: a spawn that never produced facts writes
/// nothing rather than zeros.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifierFacts {
    pub agent_id: String,
    pub transcript: Option<String>,
    pub cost: Option<f64>,
    pub tokens: Option<u64>,
    pub turns: u32,
}

/// One hole/verifier/operator decision as persisted to `decisions.jsonl`.
/// `"v": 1` leads the serialized object, like [`WorkerStatsRecord`], so a
/// later schema change is detectable by version. The evidence fields
/// (`successors`, `planned`, `bestSubject`, `tier`, `ratio`) are the
/// supervisor-authored facts the decision rested on; absent fields are
/// omitted rather than written as `null`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionRecord {
    /// Schema marker (`1`).
    pub v: u64,
    /// Epoch ms of the decision.
    pub at: u64,
    pub kind: DecisionKind,
    /// Plan row number the decision is about.
    pub row: u64,
    /// The `HEAD` the decision saw.
    pub head: String,
    /// Rows after the hole that were already matched (ascending).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub successors: Vec<u64>,
    /// The row's planned commit subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planned: Option<String>,
    /// The closest history subject, when the subject-drift report named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_subject: Option<String>,
    /// The Contract 2 match tier of `best_subject`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Token-similarity ratio of `best_subject` (2dp).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    /// The verifier's verbatim verdict label (`COMPLETE`, `INCOMPLETE`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// Facts about the verifier spawn, when one ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verifier: Option<VerifierFacts>,
    /// The chosen action (`skip`, `mark`, `work`, `reword`, `stop`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// The guard/path reason of a refusal or failure (reword, verifier).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl DecisionRecord {
    /// A bare record for `kind` with the schema marker set and every optional
    /// field absent — the caller fills the fields its decision actually has.
    pub fn new(kind: DecisionKind, row: u64, head: &str, at: u64) -> DecisionRecord {
        DecisionRecord {
            v: 1,
            at,
            kind,
            row,
            head: head.to_string(),
            successors: Vec::new(),
            planned: None,
            best_subject: None,
            tier: None,
            ratio: None,
            verdict: None,
            verifier: None,
            action: None,
            reason: None,
        }
    }
}

/// Best-effort, atomic append of one decision to `<root>/decisions.jsonl`.
/// Exactly the `append_worker_stats` discipline — write-temp-then-rename,
/// FULL-FILE REWRITE, a failed write never fails the run — so a crashed run
/// can neither corrupt the audit log nor be reported as failed because of it.
pub fn append_decision(root: &Path, record: &DecisionRecord) {
    append_jsonl(root, DECISIONS_FILE_NAME, record);
}

/// Best-effort, atomic append of one JSON line to `<root>/<file_name>` — the
/// same write-temp-then-rename discipline as `save_state_file`, so an
/// interrupted run cannot corrupt the audit log. Each record is a FULL-FILE
/// REWRITE (the temp holds the prior content plus the new line): at one record
/// per decision/attempt this is trivially cheap, but a reader tailing the path
/// across the rename misses the newest line — these files are audit logs, not
/// live tails. The record is serialized FIRST, so a record that cannot
/// serialize writes nothing (never a blank row in the audit log).
fn append_jsonl<T: Serialize>(root: &Path, file_name: &str, record: &T) {
    let _ = fs::create_dir_all(root);
    let path = root.join(file_name);
    let tmp = path.with_extension("jsonl.tmp");
    let Some(line) = serde_json::to_string(record).ok() else {
        return;
    };
    let owned_line = format!("{line}\n");
    let bytes = owned_line.as_bytes();
    let result = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        let prior = fs::read_to_string(path.as_path()).ok().unwrap_or_default();
        f.write_all(prior.as_bytes())?;
        f.write_all(bytes)?;
        f.sync_all().ok();
        fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_cwd() -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("pi-plan-storage-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    // ---- key derivation ----

    #[test]
    fn sanitize_name_keeps_alnum_dot_dash_and_underscore() {
        assert_eq!(sanitize_name("my-repo_1.2"), "my-repo_1.2");
        assert_eq!(sanitize_name("my repo"), "my_repo");
        assert_eq!(sanitize_name("a/b/c"), "a_b_c");
        assert_eq!(sanitize_name("  ..__--  "), "__..__--__");
        assert_eq!(sanitize_name(""), "");
    }

    #[test]
    fn key_hash_is_the_first_8_hex_chars_of_sha256() {
        let path = Path::new("/tmp/pi-plan-storage");
        let hex: String = Sha256::digest("/tmp/pi-plan-storage".as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            key_hash_of(path),
            hex.as_str()[0..KEY_HASH_CHARS].to_string()
        );
    }

    #[test]
    fn project_key_combines_sanitized_basename_and_hash() {
        let canonical = Path::new("/home/u/My Repo");
        let key = project_key(canonical).expect("key");
        assert_eq!(
            key,
            format!("My_Repo-{}", key_hash_of(canonical)),
            "spaces sanitized, hash appended with a dash"
        );
    }

    #[test]
    fn project_key_falls_back_to_root_label_for_filesystem_root() {
        let root_path = Path::new("/");
        let key = project_key(root_path).expect("key");
        assert!(key.starts_with("root-"));
        assert_eq!(
            key.chars().count(),
            "root-".chars().count() + KEY_HASH_CHARS,
            "root fallback is root-<hash>, never an empty basename"
        );
    }

    // ---- override / HOME precedence ----

    #[test]
    fn resolve_uses_the_state_dir_override_over_home() {
        let cwd = temp_cwd();
        let root = ProjectStorage::resolve(Some("/home/u"), Some("/s/override"), &cwd)
            .expect("resolve with override");
        let rendered = root.to_string_lossy().into_owned();
        assert!(
            rendered.as_str().starts_with("/s/override/"),
            "override wins even with HOME set"
        );
        assert!(
            !rendered.as_str().starts_with("/home/u/.pi-plan/"),
            "HOME must not leak into the override path"
        );
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn resolve_falls_back_to_home_pi_plan_without_an_override() {
        let cwd = temp_cwd();
        let root = ProjectStorage::resolve(Some("/home/u"), None, &cwd).expect("resolve");
        let rendered = root.to_string_lossy().into_owned();
        assert!(rendered.as_str().starts_with("/home/u/.pi-plan/"));
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn resolve_errors_when_home_and_override_are_both_absent() {
        let cwd = temp_cwd();
        match ProjectStorage::resolve(None, None, &cwd) {
            Err(err) => {
                assert!(
                    err.contains("HOME"),
                    "the hard error names the missing HOME: {err}"
                );
            }
            Ok(_) => panic!("HOME-less resolve must not fall back anywhere"),
        }
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn resolve_keys_by_the_canonical_cwd() {
        // A canonical path (the temp dir through a symlink-free ancestor)
        // yields a stable, basename-keyed root for every entry point.
        let cwd = temp_cwd();
        let first = ProjectStorage::resolve(Some("/home/u"), None, &cwd).expect("resolve");
        let second = ProjectStorage::resolve(Some("/home/u"), None, &cwd).expect("resolve");
        assert_eq!(first, second, "same cwd → same key");
        let _ = fs::remove_dir_all(&cwd);
    }

    // ---- worker-stats audit log ----

    fn stats_record() -> WorkerStatsRecord {
        WorkerStatsRecord {
            v: 1,
            row: 3,
            attempt: 1,
            agent_id: "0".to_string(),
            outcome: "completed".to_string(),
            cost: Some(0.0451),
            tokens: Some(59_300),
            context_percent: Some(61.5),
            context_window: Some(200_000),
            turns: 4,
            started_at: 1_000_000,
            completed_at: Some(1_090_000),
            transcript: Some("/run/sessions/pi-0/session.jsonl".to_string()),
        }
    }

    #[test]
    fn append_worker_stats_writes_one_json_line_per_record_and_keeps_prior_rows() {
        let root = temp_cwd();
        append_worker_stats(root.as_path(), &stats_record());
        append_worker_stats(root.as_path(), &stats_record());
        let file_path = root.join(WORKER_STATS_FILE_NAME);
        let raw = fs::read_to_string(&file_path).expect("read log");
        let lines = raw.lines().collect::<Vec<_>>();
        assert_eq!(
            lines.len(),
            2,
            "the full-file rewrite preserves the prior record (no gaps)"
        );
        for line in lines {
            let parsed: serde_json::Value = serde_json::from_str(line).expect("parse line");
            let obj = parsed.as_object().expect("record is an object");
            assert_eq!(
                obj.get("v").and_then(serde_json::Value::as_f64),
                Some(1.0),
                "schema marker up front"
            );
            assert_eq!(
                obj.get("row").and_then(serde_json::Value::as_f64),
                Some(3.0)
            );
            assert_eq!(
                obj.get("agentId").and_then(serde_json::Value::as_str),
                Some("0")
            );
            assert_eq!(
                obj.get("outcome").and_then(serde_json::Value::as_str),
                Some("completed")
            );
            assert_eq!(
                obj.get("cost").and_then(serde_json::Value::as_f64),
                Some(0.0451)
            );
            assert_eq!(
                obj.get("tokens").and_then(serde_json::Value::as_f64),
                Some(59_300.0)
            );
            assert_eq!(
                obj.get("contextPercent")
                    .and_then(serde_json::Value::as_f64),
                Some(61.5)
            );
            assert_eq!(
                obj.get("contextWindow").and_then(serde_json::Value::as_f64),
                Some(200_000.0)
            );
            assert_eq!(
                obj.get("turns").and_then(serde_json::Value::as_f64),
                Some(4.0)
            );
            assert_eq!(
                obj.get("completedAt").and_then(serde_json::Value::as_f64),
                Some(1_090_000.0)
            );
            assert_eq!(
                obj.get("transcript").and_then(serde_json::Value::as_str),
                Some("/run/sessions/pi-0/session.jsonl")
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn append_worker_stats_creates_the_root_directory_on_demand() {
        let base = temp_cwd();
        let fresh = base.join("nested").join("root");
        append_worker_stats(fresh.as_path(), &stats_record());
        assert!(
            fs::exists(fresh.join(WORKER_STATS_FILE_NAME).as_path())
                .ok()
                .unwrap_or(false),
            "a fresh run-state root is created before the first write"
        );
        let _ = fs::remove_dir_all(&base);
    }

    // ---- decisions audit log ----

    fn decision_record(kind: DecisionKind) -> DecisionRecord {
        let mut record = DecisionRecord::new(kind, 8, "6bf2c8e", 1_795_360_341_333);
        record.successors = vec![9, 10, 11, 12];
        record.planned = Some("test: add the decision-time leakage suite".to_string());
        record.best_subject =
            Some("test: add the decision-time training leakage suite".to_string());
        record.tier = Some("candidate".to_string());
        record.ratio = Some(0.82);
        record.verdict = Some("COMPLETE".to_string());
        record.verifier = Some(VerifierFacts {
            agent_id: "0".to_string(),
            transcript: Some("/run/sessions/pi-0/session.jsonl".to_string()),
            cost: Some(0.002),
            tokens: Some(94_189),
            turns: 8,
        });
        record.action = Some("skip".to_string());
        record
    }

    #[test]
    fn decision_kind_labels_are_pinned() {
        let labels = [
            DecisionKind::HoleVerified,
            DecisionKind::HoleEscalated,
            DecisionKind::VerifierFailed,
            DecisionKind::OperatorComplete,
            DecisionKind::OperatorWork,
            DecisionKind::OperatorReword,
            DecisionKind::RewordFailed,
        ]
        .iter()
        .map(|kind| kind.as_str())
        .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                "hole-verified",
                "hole-escalated",
                "verifier-failed",
                "operator-complete",
                "operator-work",
                "operator-reword",
                "reword-failed",
            ],
            "the on-disk vocabulary matches the plan"
        );
        for kind in [
            DecisionKind::HoleVerified,
            DecisionKind::HoleEscalated,
            DecisionKind::VerifierFailed,
            DecisionKind::OperatorComplete,
            DecisionKind::OperatorWork,
            DecisionKind::OperatorReword,
            DecisionKind::RewordFailed,
        ] {
            let rendered = serde_json::to_string(&kind).expect("serialize kind");
            assert_eq!(
                rendered,
                format!("\"{}\"", kind.as_str()),
                "serde and as_str agree"
            );
        }
    }

    #[test]
    fn append_decision_writes_one_camel_case_json_line_per_record_and_keeps_prior_rows() {
        let root = temp_cwd();
        append_decision(root.as_path(), &decision_record(DecisionKind::HoleVerified));
        let mut escalated =
            DecisionRecord::new(DecisionKind::HoleEscalated, 9, "6bf2c8e", 1_795_360_341_400);
        escalated.reason = Some("verifier reported INCOMPLETE".to_string());
        append_decision(root.as_path(), &escalated);

        let file_path = root.join(DECISIONS_FILE_NAME);
        let raw = fs::read_to_string(&file_path).expect("read decisions log");
        let lines = raw.lines().collect::<Vec<_>>();
        assert_eq!(
            lines.len(),
            2,
            "the full-file rewrite preserves the prior record (no gaps)"
        );
        assert!(
            lines[0].starts_with(r#"{"v":1,"at":"#),
            "the schema marker leads the serialized record: {}",
            lines[0]
        );

        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("parse first");
        let obj = first.as_object().expect("record is an object");
        assert_eq!(
            obj.get("kind").and_then(serde_json::Value::as_str),
            Some("hole-verified")
        );
        assert_eq!(obj.get("row").and_then(serde_json::Value::as_u64), Some(8));
        assert_eq!(
            obj.get("head").and_then(serde_json::Value::as_str),
            Some("6bf2c8e")
        );
        assert_eq!(
            obj.get("successors").and_then(serde_json::Value::as_array),
            Some(&vec![
                serde_json::json!(9),
                serde_json::json!(10),
                serde_json::json!(11),
                serde_json::json!(12),
            ])
        );
        assert_eq!(
            obj.get("bestSubject").and_then(serde_json::Value::as_str),
            Some("test: add the decision-time training leakage suite")
        );
        assert_eq!(
            obj.get("tier").and_then(serde_json::Value::as_str),
            Some("candidate")
        );
        assert_eq!(
            obj.get("ratio").and_then(serde_json::Value::as_f64),
            Some(0.82)
        );
        assert_eq!(
            obj.get("verdict").and_then(serde_json::Value::as_str),
            Some("COMPLETE")
        );
        assert_eq!(
            obj.get("action").and_then(serde_json::Value::as_str),
            Some("skip")
        );
        let verifier = obj
            .get("verifier")
            .and_then(serde_json::Value::as_object)
            .expect("verifier facts");
        assert_eq!(
            verifier.get("agentId").and_then(serde_json::Value::as_str),
            Some("0")
        );
        assert_eq!(
            verifier.get("tokens").and_then(serde_json::Value::as_u64),
            Some(94_189)
        );
        assert_eq!(
            verifier.get("turns").and_then(serde_json::Value::as_u64),
            Some(8)
        );

        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("parse second");
        let obj = second.as_object().expect("record is an object");
        assert_eq!(
            obj.get("kind").and_then(serde_json::Value::as_str),
            Some("hole-escalated")
        );
        assert_eq!(
            obj.get("reason").and_then(serde_json::Value::as_str),
            Some("verifier reported INCOMPLETE")
        );
        assert!(
            !obj.contains_key("verifier"),
            "absent facts are omitted, never written as null"
        );
        assert!(!obj.contains_key("successors"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn append_decision_creates_the_root_directory_on_demand() {
        let base = temp_cwd();
        let fresh = base.join("nested").join("root");
        append_decision(
            fresh.as_path(),
            &decision_record(DecisionKind::OperatorWork),
        );
        assert!(
            fs::exists(fresh.join(DECISIONS_FILE_NAME).as_path())
                .ok()
                .unwrap_or(false),
            "a fresh run-state root is created before the first write"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn append_decision_keeps_the_prior_log_when_a_record_cannot_serialize() {
        let root = temp_cwd();
        append_decision(root.as_path(), &decision_record(DecisionKind::HoleVerified));
        let before = fs::read_to_string(root.join(DECISIONS_FILE_NAME)).expect("read log");

        // A record whose Serialize fails writes nothing at all: no blank line,
        // no truncated file, no leftover temp file next to the audit log.
        append_jsonl(root.as_path(), DECISIONS_FILE_NAME, &Unserializable);

        let after = fs::read_to_string(root.join(DECISIONS_FILE_NAME)).expect("read log");
        assert_eq!(before, after, "a failed serialize leaves the log untouched");
        assert_eq!(after.lines().count(), 1);
        assert!(
            !fs::exists(root.join("decisions.jsonl.tmp"))
                .ok()
                .unwrap_or(false),
            "a failed serialize leaves no temp file behind"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A record type whose serialization always fails — the only way to
    /// exercise the "serialize first" guard with the real serde_json writer.
    struct Unserializable;

    impl Serialize for Unserializable {
        fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("nope"))
        }
    }
}
