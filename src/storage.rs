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

use crate::supervise::{RunRecord, run_outcome_label};

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

/// Build a stats record from a finalized run record. `None` when the run
/// has no terminal snapshot (spawn-error / clean-pass / question-only
/// bookkeeping) — such runs write nothing, so the log has no all-null
/// rows.
pub fn worker_stats_from_run(record: &RunRecord) -> Option<WorkerStatsRecord> {
    let snap = record.snapshot.as_ref()?;
    Some(WorkerStatsRecord {
        v: 1,
        row: record.row.number,
        attempt: record.attempt,
        agent_id: record.agent_id.clone(),
        outcome: run_outcome_label(record.outcome),
        cost: snap.cost,
        tokens: snap.tokens.map(|t| t.total),
        context_percent: snap.context_percent,
        context_window: snap.context_window,
        turns: snap.turn_count,
        started_at: record.started_at,
        completed_at: record.completed_at,
        transcript: snap
            .transcript
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
    })
}

/// Best-effort, atomic append of one record to `<root>/worker-stats.jsonl`
/// — the same write-temp-then-rename discipline as `save_state_file`, so
/// an interrupted run cannot corrupt the audit log. Each record is a
/// FULL-FILE REWRITE (the temp holds the prior content plus the new line):
/// at one record per run attempt this is trivially cheap, but a reader
/// tailing the path across the rename misses the newest line — the file is
/// an audit log, not a live tail.
pub fn append_worker_stats(root: &Path, record: &WorkerStatsRecord) {
    let _ = fs::create_dir_all(root);
    let path = root.join(WORKER_STATS_FILE_NAME);
    let tmp = path.with_extension("jsonl.tmp");
    // Serialize first: a record that cannot serialize writes nothing
    // (never a blank row in the audit log).
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

    use crate::supervise::RunOutcomeKind;
    use crate::todo::TodoRow;
    use crate::worker::{Tokens, WorkerSnapshot};

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

    fn run_record(snapshot: Option<WorkerSnapshot>) -> RunRecord {
        RunRecord {
            attempt: 1,
            row: TodoRow {
                id: "3".to_string(),
                number: 3,
                commit_message: "feat: x".to_string(),
                logical_unit: "u".to_string(),
                deliverables: "d".to_string(),
                tests: "t".to_string(),
            },
            agent_id: "0".to_string(),
            outcome: RunOutcomeKind::Completed,
            question: None,
            tail: None,
            transcript_path: None,
            started_at: 1_000_000,
            completed_at: Some(1_090_000),
            snapshot,
        }
    }

    #[test]
    fn worker_stats_from_run_skips_snapshot_less_runs() {
        // Spawn-error / clean-pass bookkeeping records carry no snapshot
        // and write nothing — the log has no all-null rows.
        assert_eq!(worker_stats_from_run(&run_record(None)), None);
    }

    #[test]
    fn worker_stats_from_run_maps_the_terminal_snapshot_fields() {
        let snap = WorkerSnapshot {
            id: 0,
            text: "assembled".to_string(),
            tool_uses: 3,
            turn_count: 4,
            compaction_count: 0,
            context_percent: Some(61.5),
            transcript: Some(Path::new("/run/sessions/pi-0/session.jsonl").to_path_buf()),
            cost: Some(0.0451),
            tokens: Some(Tokens {
                input: 50_000,
                output: 9_300,
                cache_read: 40_000,
                cache_write: 5_000,
                total: 59_300,
            }),
            context_window: Some(200_000),
            started_at: 1_000_000,
            pending_tool: None,
            terminal: None,
        };
        let stats = worker_stats_from_run(&run_record(Some(snap))).expect("stats record");
        assert_eq!(stats.v, 1);
        assert_eq!(stats.row, 3);
        assert_eq!(stats.attempt, 1);
        assert_eq!(stats.agent_id, "0");
        assert_eq!(stats.outcome, "completed");
        assert_eq!(stats.cost, Some(0.0451));
        assert_eq!(stats.tokens, Some(59_300));
        assert_eq!(stats.context_percent, Some(61.5));
        assert_eq!(stats.context_window, Some(200_000));
        assert_eq!(stats.turns, 4);
        assert_eq!(stats.started_at, 1_000_000);
        assert_eq!(stats.completed_at, Some(1_090_000));
        assert_eq!(
            stats.transcript,
            Some("/run/sessions/pi-0/session.jsonl".to_string())
        );
    }
}
