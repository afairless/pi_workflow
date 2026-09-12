//! supervisor-state.json persistence (Contract 5) — a port of the TS
//! supervisor's `state.ts` (atomic write, best-effort, never throws).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// File name resolved against the run-state root (bare name is gitignored).
pub const STATE_FILE_NAME: &str = "supervisor-state.json";

/// Value persisted in supervisor-state.json. `adjudicated` holds row numbers
/// the human explicitly marked done (`pi-plan mark <n> done`).
/// `agent_id`/`started_at` are set at spawn time (optional, backward
/// compatible) and carried forward in terminal-event saves for crash recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorState {
    pub plan_hash: String,
    pub current_row: u64,
    pub runs_used: u32,
    pub last_outcome: String,
    pub adjudicated: Vec<u64>,
    /// Agent id of the last live worker (set at spawn time, carried through terminal saves).
    pub agent_id: Option<String>,
    /// Epoch ms when the last worker was spawned.
    pub started_at: Option<u64>,
}

/// Sha256 hex of the TODO.md content — the plan identity for recovery.
pub fn plan_hash_of(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn state_file_path(root: &Path) -> PathBuf {
    root.join(STATE_FILE_NAME)
}

/// Read and validate the state file. Any parse/shape failure returns `None`
/// (never throws) so the caller recomputes from git.
pub fn read_state_file(root: &Path) -> Option<SupervisorState> {
    let raw = fs::read_to_string(state_file_path(root)).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    coerce_state(&parsed)
}

/// Shape-check an unknown JSON value into a `SupervisorState`.
///
/// Mirrors the TS validation: `planHash`/`currentRow`/`runsUsed` must have
/// the right types; optional `agentId`/`startedAt` must be absent-or-correct;
/// `adjudicated` is coerced to the numeric members (anything else → empty).
fn coerce_state(parsed: &serde_json::Value) -> Option<SupervisorState> {
    let object = parsed.as_object()?;
    let plan_hash = object.get("planHash")?.as_str()?.to_string();
    let current_row = int_or_none(object.get("currentRow")?)?;
    let runs_used = u32_or_none(object.get("runsUsed")?)?;
    let last_outcome = match object.get("lastOutcome") {
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let adjudicated = match object.get("adjudicated") {
        Some(serde_json::Value::Array(items)) => {
            items.iter().filter_map(int_or_none).collect::<Vec<u64>>()
        }
        _ => Vec::new(),
    };
    let agent_id = match object.get("agentId") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        _ => return None, // wrong type for agentId → whole file rejected
    };
    let started_at = match object.get("startedAt") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(int_or_none(v)?),
    };
    Some(SupervisorState {
        plan_hash,
        current_row,
        runs_used,
        last_outcome,
        adjudicated,
        agent_id,
        started_at,
    })
}

/// A JSON number that is a finite non-negative integer, as `u32` (clamped
/// so an oversized runsUsed still resumes rather than being rejected).
fn u32_or_none(value: &serde_json::Value) -> Option<u32> {
    int_or_none(value).map(|n| n.min(u32::MAX as u64) as u32)
}

/// A JSON number that is a finite non-negative integer, as `u64`.
fn int_or_none(value: &serde_json::Value) -> Option<u64> {
    let n = value.as_f64()?;
    if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= u64::MAX as f64 {
        Some(n as u64)
    } else {
        None
    }
}

/// Best-effort, atomic write (temp file + rename): a kill mid-write cannot
/// corrupt the recovery input, and a failed write never crashes the loop.
/// The run-state root is created on demand so a first save (including
/// `mark <n> done` before any `supervise`) always has a home.
pub fn save_state_file(root: &Path, state: &SupervisorState) {
    let _ = fs::create_dir_all(root);
    let path = state_file_path(root);
    let tmp = path.with_extension("json.tmp");
    let result = (|| -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(state).unwrap_or_default();
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.write_all(b"\n")?;
        f.sync_all().ok();
        fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

/// Best-effort removal (used when the plan completes or is invalidated).
pub fn clear_state_file(root: &Path) {
    let _ = fs::remove_file(state_file_path(root));
}

/// Recovery decision rule (Contract 5):
/// - no / corrupt state file → `None` (recompute from git)
/// - plan hash mismatch → `None` (the plan changed)
/// - the state's currentRow is already matched in git → `None` (the row
///   completed; stale state must not contradict the repo)
/// - otherwise resume with `runs_used` intact
pub fn recover_state(
    root: &Path,
    todo_content: &str,
    is_row_done_at: impl Fn(u64) -> bool,
) -> Option<SupervisorState> {
    let state = read_state_file(root)?;
    if state.plan_hash != plan_hash_of(todo_content) {
        return None;
    }
    if is_row_done_at(state.current_row) {
        return None;
    }
    Some(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_cwd() -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("pi-plan-state-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    const TODO_A: &str = "## Steps\n| 1 | `feat: a` | u | d | t |\n";
    const TODO_B: &str = "## Steps\n| 1 | `feat: b` | u | d | t |\n";

    fn test_state() -> SupervisorState {
        SupervisorState {
            plan_hash: plan_hash_of(TODO_A),
            current_row: 3,
            runs_used: 1,
            last_outcome: "failed".to_string(),
            adjudicated: vec![7],
            agent_id: None,
            started_at: None,
        }
    }

    #[test]
    fn read_state_file_returns_none_when_the_file_does_not_exist() {
        let root = temp_cwd();
        assert_eq!(read_state_file(&root), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn save_then_read_round_trips() {
        let root = temp_cwd();
        let state = test_state();
        save_state_file(&root, &state);
        let got = read_state_file(&root).expect("round-trip");
        assert_eq!(got.plan_hash, state.plan_hash);
        assert_eq!(got.current_row, 3);
        assert_eq!(got.runs_used, 1);
        assert_eq!(got.last_outcome, "failed");
        assert_eq!(got.adjudicated, [7]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn save_state_file_creates_the_root_directory_on_demand() {
        let parent = temp_cwd();
        let root = parent.join("fresh-root");
        assert!(!root.exists(), "root starts missing");
        save_state_file(&root, &test_state());
        assert!(root.exists(), "save creates the root");
        assert_eq!(
            read_state_file(&root).as_ref().map(|s| s.runs_used),
            Some(1)
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn corrupt_or_wrong_shape_state_recomputes_without_throwing() {
        let root = temp_cwd();
        fs::write(state_file_path(&root), "{ not json !!").expect("write");
        assert_eq!(read_state_file(&root), None, "unparseable JSON rejected");

        fs::write(state_file_path(&root), r#"{"foo": 1}"#).expect("write");
        assert_eq!(read_state_file(&root), None, "wrong shape rejected");

        fs::write(
            state_file_path(&root),
            r#"{"planHash": "x", "currentRow": "not-a-number", "runsUsed": 1}"#,
        )
        .expect("write");
        assert_eq!(
            read_state_file(&root),
            None,
            "wrong currentRow type rejected"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_state_returns_none_when_the_plan_hash_changed() {
        let root = temp_cwd();
        let state = test_state();
        save_state_file(&root, &state);

        let res = recover_state(&root, TODO_B, |_| false);
        assert_eq!(
            res, None,
            "different TODO.md content must invalidate the state"
        );
        let res2 = recover_state(&root, TODO_A, |_| false);
        assert_eq!(res2.as_ref().map(|s| s.runs_used), Some(1));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_state_returns_none_when_the_row_is_already_done_in_git() {
        let root = temp_cwd();
        let mut state = test_state();
        state.current_row = 1;
        save_state_file(&root, &state);

        let res = recover_state(&root, TODO_A, |n| n == 1);
        assert_eq!(
            res, None,
            "git says the row is done; a stale state file must not contradict it"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_state_resumes_runs_used_when_the_row_is_still_unmatched() {
        let root = temp_cwd();
        let mut state = test_state();
        state.current_row = 1;
        state.runs_used = 2;
        save_state_file(&root, &state);

        let res = recover_state(&root, TODO_A, |_| false).expect("resume");
        assert_eq!(res.runs_used, 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_state_file_removes_the_file() {
        let root = temp_cwd();
        save_state_file(&root, &test_state());
        assert!(state_file_path(&root).exists());
        clear_state_file(&root);
        assert!(!state_file_path(&root).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn coerce_round_trips_agent_id_and_started_at() {
        let root = temp_cwd();
        let mut state = test_state();
        state.last_outcome = "running".to_string();
        state.agent_id = Some("fake-1".to_string());
        state.started_at = Some(1_000_000);
        save_state_file(&root, &state);
        let got = read_state_file(&root).expect("round-trip with optional fields");
        assert_eq!(got.agent_id.as_deref(), Some("fake-1"));
        assert_eq!(got.started_at, Some(1_000_000));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn coerce_tolerates_legacy_state_files_without_agent_id_or_started_at() {
        let root = temp_cwd();
        fs::write(
            state_file_path(&root),
            r#"{"planHash": "x", "currentRow": 1, "runsUsed": 0, "lastOutcome": "", "adjudicated": []}"#,
        )
        .expect("write legacy-shaped state");
        let got = read_state_file(&root).expect("legacy state accepted");
        assert_eq!(got.agent_id, None);
        assert_eq!(got.started_at, None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn coerce_rejects_wrong_types_for_agent_id_and_started_at() {
        let root = temp_cwd();
        fs::write(
            state_file_path(&root),
            r#"{"planHash": "x", "currentRow": 1, "runsUsed": 0, "lastOutcome": "", "adjudicated": [], "agentId": 42}"#,
        )
        .expect("write");
        assert_eq!(
            read_state_file(&root),
            None,
            "numeric agentId must be rejected"
        );

        fs::write(
            state_file_path(&root),
            r#"{"planHash": "x", "currentRow": 1, "runsUsed": 0, "lastOutcome": "", "adjudicated": [], "startedAt": "now"}"#,
        )
        .expect("write");
        assert_eq!(
            read_state_file(&root),
            None,
            "string startedAt must be rejected"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn atomic_write_leaves_no_temp_file_behind() {
        let root = temp_cwd();
        save_state_file(&root, &test_state());
        let tmp = root.join("supervisor-state.json.tmp");
        assert!(!tmp.exists(), "temp file must be renamed away");
        let _ = fs::remove_dir_all(&root);
    }
}
