//! supervisor-state.json persistence (Contract 5) — a port of the TS
//! supervisor's `state.ts` (atomic write, best-effort, never throws).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// File name resolved against the run-state root (bare name is gitignored).
pub const STATE_FILE_NAME: &str = "supervisor-state.json";

/// Upper bound on `verifiedCache`: the newest verdicts win and older ones are
/// evicted so a long-lived plan cannot grow the state file without limit.
pub const VERIFIED_CACHE_LIMIT: usize = 64;

/// Serialises the read-modify-write in [`save_state_file`] across the whole
/// process, so a concurrent `pi-plan mark <n> done` (its own read + save)
/// cannot be lost between another save's read and its rename.
static STATE_SAVE_LOCK: Mutex<()> = Mutex::new(());

/// A row the verifier authoritatively skipped (Contract 5 provenance). Feeds
/// the done set exactly like `adjudicated`, so it survives restarts and a
/// changed plan hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedRow {
    pub row: u64,
    /// The `HEAD` the verifier saw.
    pub head: String,
    /// The verifier's verdict label (`complete`, `incomplete`, …).
    pub verdict: String,
    /// The commit the evidence rests on, when the bundle named one.
    pub commit: Option<String>,
    /// Epoch ms of the verification.
    pub at: u64,
    /// The supervisor-authored evidence lines (deliverable paths, commit ids).
    pub evidence: Vec<String>,
}

/// One cached verifier verdict, keyed by `(row, head)` so a restart with an
/// unchanged `HEAD` never re-spawns the verifier. Non-`COMPLETE` verdicts are
/// cached too: they are re-presented to the operator instead of silently
/// skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedVerdict {
    pub row: u64,
    pub head: String,
    pub verdict: String,
    /// Epoch ms of the verdict; the LRU eviction key.
    pub at: u64,
}

/// Value persisted in supervisor-state.json. `adjudicated` holds row numbers
/// the human explicitly marked done (`pi-plan mark <n> done`); `verified` holds
/// the rows the verifier skipped. Both are provenance and are merged additively
/// on save, so they survive a plan-hash change. `agent_id`/`started_at` are set
/// at spawn time (optional, backward compatible) and carried forward in
/// terminal-event saves for crash recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorState {
    pub plan_hash: String,
    pub current_row: u64,
    pub runs_used: u32,
    pub last_outcome: String,
    pub adjudicated: Vec<u64>,
    /// Rows the verifier skipped (authoritative provenance).
    #[serde(default)]
    pub verified: Vec<VerifiedRow>,
    /// Every verifier verdict by `(row, head)`, bounded to
    /// [`VERIFIED_CACHE_LIMIT`] entries.
    #[serde(default)]
    pub verified_cache: Vec<CachedVerdict>,
    /// Agent id of the last live worker (set at spawn time, carried through terminal saves).
    pub agent_id: Option<String>,
    /// Epoch ms when the last worker was spawned.
    pub started_at: Option<u64>,
}

impl SupervisorState {
    /// The cached verdict for `(row, head)`, when one exists.
    pub fn cached_verdict(&self, row: u64, head: &str) -> Option<&CachedVerdict> {
        self.verified_cache
            .iter()
            .find(|c| c.row == row && c.head == head)
    }

    /// Insert-or-refresh a cached verdict: keyed by `(row, head)`, the newest
    /// `at` wins, and the cache is re-bounded after every insert.
    pub fn insert_cached_verdict(&mut self, verdict: CachedVerdict) {
        match self
            .verified_cache
            .iter_mut()
            .find(|c| c.row == verdict.row && c.head == verdict.head)
        {
            Some(existing) if existing.at <= verdict.at => *existing = verdict,
            Some(_) => {}
            None => self.verified_cache.push(verdict),
        }
        bound_verified_cache(&mut self.verified_cache);
    }
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
    // Provenance is coerced elementwise, like `adjudicated`: absent or
    // wrong-shaped containers yield empty, and a malformed member is dropped
    // instead of rejecting the whole file (a corrupt verdict cache must never
    // cost an operator their adjudications).
    let verified = coerce_verified(object.get("verified"));
    let verified_cache = coerce_verified_cache(object.get("verifiedCache"));
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
        verified,
        verified_cache,
        agent_id,
        started_at,
    })
}

/// Coerce the `verified` provenance array: absent/non-array → empty, malformed
/// members dropped, one entry per row (the later element wins `at` ties).
fn coerce_verified(value: Option<&serde_json::Value>) -> Vec<VerifiedRow> {
    let Some(serde_json::Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        if let Some(entry) = coerce_verified_row(item) {
            rows.push(entry);
        }
    }
    normalize_verified(&mut rows);
    rows
}

fn coerce_verified_row(value: &serde_json::Value) -> Option<VerifiedRow> {
    let object = value.as_object()?;
    let row = int_or_none(object.get("row")?)?;
    let head = object.get("head")?.as_str()?.to_string();
    let verdict = object.get("verdict")?.as_str()?.to_string();
    let commit = match object.get("commit") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    let at = int_or_none(object.get("at")?)?;
    let evidence = match object.get("evidence") {
        None => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(_) => return None,
    };
    Some(VerifiedRow {
        row,
        head,
        verdict,
        commit,
        at,
        evidence,
    })
}

/// Coerce the `verifiedCache` array (same elementwise discipline), then apply
/// the cache bound so an oversized legacy file is trimmed on read.
fn coerce_verified_cache(value: Option<&serde_json::Value>) -> Vec<CachedVerdict> {
    let Some(serde_json::Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut cache: Vec<CachedVerdict> = items.iter().filter_map(coerce_cached_verdict).collect();
    bound_verified_cache(&mut cache);
    cache
}

fn coerce_cached_verdict(value: &serde_json::Value) -> Option<CachedVerdict> {
    let object = value.as_object()?;
    Some(CachedVerdict {
        row: int_or_none(object.get("row")?)?,
        head: object.get("head")?.as_str()?.to_string(),
        verdict: object.get("verdict")?.as_str()?.to_string(),
        at: int_or_none(object.get("at")?)?,
    })
}

/// Collapse duplicate rows onto the newest `at` (the later element wins a tie)
/// and sort by row, so the serialized provenance is order-stable.
fn normalize_verified(rows: &mut Vec<VerifiedRow>) {
    let mut collapsed: Vec<VerifiedRow> = Vec::with_capacity(rows.len());
    for entry in rows.drain(..) {
        match collapsed.iter_mut().find(|r| r.row == entry.row) {
            Some(existing) if existing.at <= entry.at => *existing = entry,
            Some(_) => {}
            None => collapsed.push(entry),
        }
    }
    collapsed.sort_by_key(|r| r.row);
    *rows = collapsed;
}

/// Collapse duplicate `(row, head)` keys onto the newest `at` and keep only the
/// newest [`VERIFIED_CACHE_LIMIT`] entries: eviction order is LRU by `at`, then
/// row, then head, so it is deterministic and testable.
pub fn bound_verified_cache(cache: &mut Vec<CachedVerdict>) {
    cache.sort_by(|a, b| {
        a.at.cmp(&b.at)
            .then(a.row.cmp(&b.row))
            .then(a.head.cmp(&b.head))
    });
    let mut collapsed: Vec<CachedVerdict> = Vec::with_capacity(cache.len());
    for entry in cache.drain(..) {
        match collapsed
            .iter_mut()
            .find(|c| c.row == entry.row && c.head == entry.head)
        {
            Some(existing) => *existing = entry,
            None => collapsed.push(entry),
        }
    }
    if collapsed.len() > VERIFIED_CACHE_LIMIT {
        let excess = collapsed.len() - VERIFIED_CACHE_LIMIT;
        collapsed.drain(..excess);
    }
    *cache = collapsed;
}

/// Merge on-disk provenance into the state being saved (the defect-4 fix):
/// `adjudicated` is a union, `verified` is keyed by row and `verifiedCache` by
/// `(row, head)`, both keeping the newest `at`. The merge is additive and
/// ignores `planHash`, so a `TODO.md` edit can no longer erase a human
/// adjudication or a verifier skip; the in-memory value wins `at` ties.
pub fn merge_provenance(into: &mut SupervisorState, disk: &SupervisorState) {
    into.adjudicated.extend(disk.adjudicated.iter().copied());
    into.adjudicated.sort_unstable();
    into.adjudicated.dedup();
    for row in &disk.verified {
        match into.verified.iter_mut().find(|v| v.row == row.row) {
            Some(existing) if existing.at <= row.at => *existing = row.clone(),
            Some(_) => {}
            None => into.verified.push(row.clone()),
        }
    }
    normalize_verified(&mut into.verified);
    for cached in &disk.verified_cache {
        into.insert_cached_verdict(cached.clone());
    }
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

/// Best-effort, atomic, merging save.
///
/// Reads the file immediately before writing and merges its provenance
/// (`adjudicated`/`verified`/`verifiedCache`) into `state`, so a save from the
/// supervise loop or a concurrent `pi-plan mark` never drops a row the other
/// writer recorded. The read-modify-write runs under a process-wide lock; the
/// merge is additive and plan-hash independent.
pub fn save_state_file(root: &Path, state: &SupervisorState) {
    let guard = STATE_SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut merged = state.clone();
    if let Some(disk) = read_state_file(root) {
        merge_provenance(&mut merged, &disk);
    }
    write_state_file(root, &merged);
    drop(guard);
}

/// Best-effort, atomic write (temp file + rename): a kill mid-write cannot
/// corrupt the recovery input, and a failed write never crashes the loop.
/// The run-state root is created on demand so a first save (including
/// `mark <n> done` before any `supervise`) always has a home.
fn write_state_file(root: &Path, state: &SupervisorState) {
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
            verified: Vec::new(),
            verified_cache: Vec::new(),
            agent_id: None,
            started_at: None,
        }
    }

    fn verified_row(row: u64, head: &str, at: u64) -> VerifiedRow {
        VerifiedRow {
            row,
            head: head.to_string(),
            verdict: "complete".to_string(),
            commit: Some("fc5ea15".to_string()),
            at,
            evidence: vec![
                "tests/test_export.py".to_string(),
                "commit fc5ea15".to_string(),
            ],
        }
    }

    fn cached(row: u64, head: &str, verdict: &str, at: u64) -> CachedVerdict {
        CachedVerdict {
            row,
            head: head.to_string(),
            verdict: verdict.to_string(),
            at,
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

    // ---------------- verified provenance + verdict cache ----------------

    #[test]
    fn verified_and_verified_cache_round_trip() {
        let root = temp_cwd();
        let mut state = test_state();
        state.verified = vec![verified_row(8, "6bf2c8e", 1_795_360_341_333)];
        state.verified_cache = vec![
            cached(8, "6bf2c8e", "complete", 1_795_360_341_333),
            cached(9, "6bf2c8e", "incomplete", 1_795_360_341_334),
        ];
        save_state_file(&root, &state);

        let got = read_state_file(&root).expect("round-trip");
        assert_eq!(got.verified, state.verified);
        assert_eq!(got.verified_cache, state.verified_cache);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verified_serializes_with_camel_case_keys() {
        let root = temp_cwd();
        let mut state = test_state();
        state.verified_cache = vec![cached(9, "abc", "incomplete", 5)];
        save_state_file(&root, &state);
        let raw = fs::read_to_string(state_file_path(&root)).expect("read");
        assert!(raw.contains("\"verifiedCache\""), "camelCase key: {raw}");
        assert!(raw.contains("\"verified\":"), "empty provenance is written");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn coerce_tolerates_a_legacy_file_without_verified_fields() {
        let root = temp_cwd();
        fs::write(
            state_file_path(&root),
            r#"{"planHash": "x", "currentRow": 1, "runsUsed": 0, "lastOutcome": "", "adjudicated": [3]}"#,
        )
        .expect("write legacy-shaped state");
        let got = read_state_file(&root).expect("legacy state accepted");
        assert_eq!(got.adjudicated, [3]);
        assert!(got.verified.is_empty());
        assert!(got.verified_cache.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn coerce_drops_malformed_verified_members_but_keeps_the_rest() {
        let root = temp_cwd();
        fs::write(
            state_file_path(&root),
            concat!(
                r#"{"planHash": "x", "currentRow": 1, "runsUsed": 0, "lastOutcome": "", "adjudicated": [3],"#,
                r#" "verified": ["#,
                r#"  {"row": 8, "head": "abc", "verdict": "complete", "commit": null, "at": 7, "evidence": ["a"]},"#,
                r#"  {"row": "nine", "head": "abc", "verdict": "complete", "at": 7},"#,
                r#"  {"row": 9, "head": 5, "verdict": "complete", "at": 7},"#,
                r#"  {"row": 10, "head": "abc", "verdict": "complete", "commit": 42, "at": 7},"#,
                r#"  {"row": 11, "head": "abc", "verdict": "complete", "at": 7, "evidence": "not-an-array"},"#,
                r#"  "not-an-object""#,
                r#" ],"#,
                r#" "verifiedCache": ["#,
                r#"  {"row": 8, "head": "abc", "verdict": "complete", "at": 7},"#,
                r#"  {"row": 9, "head": "abc", "verdict": "incomplete"}"#,
                r#" ]}"#,
            ),
        )
        .expect("write");
        let got = read_state_file(&root).expect("the file is accepted");
        assert_eq!(got.adjudicated, [3], "unrelated provenance survives");
        assert_eq!(got.verified.len(), 1, "only the sound member survives");
        assert_eq!(got.verified[0].row, 8);
        assert_eq!(got.verified[0].commit, None);
        assert_eq!(
            got.verified_cache.len(),
            1,
            "malformed cache member dropped"
        );
        assert_eq!(got.verified_cache[0].row, 8);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn wrong_shaped_provenance_containers_coerce_to_empty() {
        let root = temp_cwd();
        fs::write(
            state_file_path(&root),
            r#"{"planHash": "x", "currentRow": 1, "runsUsed": 0, "lastOutcome": "", "adjudicated": [], "verified": {}, "verifiedCache": 7}"#,
        )
        .expect("write");
        let got = read_state_file(&root).expect("shape tolerated like adjudicated");
        assert!(got.verified.is_empty());
        assert!(got.verified_cache.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verified_keeps_the_newest_entry_per_row() {
        let root = temp_cwd();
        fs::write(
            state_file_path(&root),
            concat!(
                r#"{"planHash": "x", "currentRow": 1, "runsUsed": 0, "lastOutcome": "", "adjudicated": [],"#,
                r#" "verified": ["#,
                r#"  {"row": 8, "head": "old", "verdict": "complete", "at": 1},"#,
                r#"  {"row": 8, "head": "new", "verdict": "complete", "at": 2}"#,
                r#" ]}"#,
            ),
        )
        .expect("write");
        let got = read_state_file(&root).expect("accepted");
        assert_eq!(got.verified.len(), 1);
        assert_eq!(got.verified[0].head, "new");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cache_bound_keeps_the_newest_64_entries_by_at_row_head() {
        // 70 entries with ascending `at`: the nine oldest must be evicted.
        let mut cache: Vec<CachedVerdict> = (0..70u64)
            .map(|i| cached(i, "h", "complete", 1_000 + i))
            .collect();
        bound_verified_cache(&mut cache);
        assert_eq!(cache.len(), VERIFIED_CACHE_LIMIT);
        assert_eq!(cache.first().map(|c| c.row), Some(6), "oldest evicted");
        assert_eq!(cache.last().map(|c| c.row), Some(69));

        // Equal `at`: the eviction tiebreak is by row, then head.
        let mut tied: Vec<CachedVerdict> = (0..3u64)
            .map(|i| cached(10 - i, "h", "complete", 5))
            .collect();
        bound_verified_cache(&mut tied);
        let rows: Vec<u64> = tied.iter().map(|c| c.row).collect();
        assert_eq!(rows, [8, 9, 10], "deterministic ascending order");

        let mut tied_heads = vec![
            cached(1, "zeta", "complete", 5),
            cached(1, "alpha", "complete", 5),
        ];
        bound_verified_cache(&mut tied_heads);
        assert_eq!(tied_heads[0].head, "alpha");
        assert_eq!(tied_heads[1].head, "zeta");
    }

    #[test]
    fn cache_bound_evicts_the_oldest_when_over_the_limit() {
        let mut cache: Vec<CachedVerdict> = (0..64u64)
            .map(|i| cached(i, "h", "complete", 100 + i))
            .collect();
        cache.push(cached(64, "h", "complete", 1));
        bound_verified_cache(&mut cache);
        assert_eq!(cache.len(), VERIFIED_CACHE_LIMIT);
        assert!(
            !cache.iter().any(|c| c.row == 64),
            "the lowest `at` is evicted even when inserted last"
        );
    }

    #[test]
    fn insert_cached_verdict_keys_by_row_and_head_and_bounds_the_cache() {
        let mut state = test_state();
        state.insert_cached_verdict(cached(9, "abc", "incomplete", 5));
        state.insert_cached_verdict(cached(9, "abc", "incomplete", 6));
        assert_eq!(state.verified_cache.len(), 1, "(row, head) is the key");
        assert_eq!(state.verified_cache[0].at, 6, "newest `at` wins");
        state.insert_cached_verdict(cached(9, "def", "complete", 7));
        assert_eq!(state.verified_cache.len(), 2, "a new HEAD is a new entry");
        assert_eq!(
            state.cached_verdict(9, "def").map(|c| c.verdict.as_str()),
            Some("complete")
        );
        assert_eq!(state.cached_verdict(9, "abc").map(|c| c.at), Some(6));
        assert!(state.cached_verdict(10, "def").is_none());

        for i in 0..70u64 {
            state.insert_cached_verdict(cached(i, "h", "complete", 1_000 + i));
        }
        assert_eq!(state.verified_cache.len(), VERIFIED_CACHE_LIMIT);
    }

    #[test]
    fn save_merges_disk_adjudications_additively() {
        let root = temp_cwd();
        let mut disk_state = test_state();
        disk_state.adjudicated = vec![7];
        save_state_file(&root, &disk_state);

        let mut saving = test_state();
        saving.adjudicated = vec![3];
        save_state_file(&root, &saving);

        let got = read_state_file(&root).expect("merged");
        assert_eq!(got.adjudicated, [3, 7], "union, sorted");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_plan_hash_mismatch_keeps_provenance() {
        // The defect-4 regression: `mark 8 done`, then a TODO.md edit, then
        // the loop saves under the new plan hash. Row 8 must stay adjudicated.
        let root = temp_cwd();
        let mut marked = test_state();
        marked.adjudicated = vec![8];
        save_state_file(&root, &marked);

        let mut loop_state = test_state();
        loop_state.plan_hash = plan_hash_of(TODO_B);
        loop_state.adjudicated = Vec::new();
        save_state_file(&root, &loop_state);

        let got = read_state_file(&root).expect("saved");
        assert_eq!(got.plan_hash, plan_hash_of(TODO_B));
        assert_eq!(
            got.adjudicated,
            [8],
            "editing TODO.md must not erase a human adjudication"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn save_merges_verified_and_cache_by_key_keeping_the_newest_at() {
        let root = temp_cwd();
        let mut disk_state = test_state();
        disk_state.verified = vec![verified_row(8, "abc", 10), verified_row(9, "abc", 11)];
        disk_state.verified_cache = vec![cached(8, "abc", "complete", 10)];
        save_state_file(&root, &disk_state);

        let mut saving = test_state();
        saving.verified = vec![verified_row(8, "def", 9)]; // older `at` loses
        saving.verified_cache = vec![cached(8, "abc", "incomplete", 12)];
        save_state_file(&root, &saving);

        let got = read_state_file(&root).expect("merged");
        assert_eq!(got.verified.len(), 2);
        assert_eq!(got.verified[0].head, "abc", "disk entry is newer");
        assert_eq!(got.verified[1].row, 9, "disk-only provenance is kept");
        assert_eq!(got.verified_cache.len(), 1);
        assert_eq!(got.verified_cache[0].verdict, "incomplete");
        assert_eq!(got.verified_cache[0].at, 12);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_concurrent_mark_cannot_be_lost_between_read_and_write() {
        // Several threads each add a distinct row; the process-wide lock plus
        // the additive merge must leave every row in the final file.
        let root = temp_cwd();
        let mut seed = test_state();
        seed.adjudicated = Vec::new();
        save_state_file(&root, &seed);

        let mut handles = Vec::new();
        for row in 1..=6u64 {
            let root = root.clone();
            handles.push(std::thread::spawn(move || {
                let mut state = test_state();
                state.adjudicated = vec![row];
                save_state_file(&root, &state);
            }));
        }
        for handle in handles {
            handle.join().expect("writer thread");
        }

        let got = read_state_file(&root).expect("final state");
        assert_eq!(got.adjudicated, [1, 2, 3, 4, 5, 6]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn merge_provenance_is_additive_and_idempotent() {
        let mut into = test_state();
        into.adjudicated = vec![5, 5];
        into.verified = vec![verified_row(8, "abc", 10)];
        into.verified_cache = vec![cached(8, "abc", "complete", 10)];

        let mut disk = test_state();
        disk.adjudicated = vec![5, 6];
        disk.verified = vec![verified_row(8, "abc", 11), verified_row(9, "abc", 12)];
        disk.verified_cache = vec![cached(9, "abc", "incomplete", 13)];

        merge_provenance(&mut into, &disk);
        let once = into.clone();
        merge_provenance(&mut into, &disk);

        assert_eq!(into, once, "merging the same disk state twice is a no-op");
        assert_eq!(into.adjudicated, [5, 6]);
        assert_eq!(into.verified.len(), 2);
        assert_eq!(into.verified_cache.len(), 2);
    }
}
