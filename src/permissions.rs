//! Durable project permissions store (the supervisor's session-grant
//! memory — plan supervisor-permission-memory, D3/D5).
//!
//! `permissions.json` lives under the run-state root
//! (`~/.pi-plan/<key>/permissions.json`, `$PI_PLAN_STATE_DIR`-aware via
//! `storage::ProjectStorage::resolve`) and holds the "…for this session"
//! grants the operator approved on relayed permission dialogs — the
//! precedent store the supervisor's proxy auto-approves against. The
//! record is permission-shaped (D3): never a command string — `surface`
//! family, `direction` (`read`/`write`/`both`, null for verb-less
//! surfaces), the approved suggested `pattern` verbatim, `width`
//! (`proven`/`family`/null), plus audit fields (`worker` who earned it,
//! RFC3339 `createdAt`).
//!
//! Read discipline follows D5: a missing file is a healthy empty store
//! (normal first run); an unreadable / unparseable / wrong-shaped file
//! fails closed to an EMPTY store plus a stderr warning — nothing
//! auto-approves and the run never blocks. Writes are whole-file atomic
//! rewrites (temp file + rename), the same discipline as
//! `storage::append_worker_stats` and `state::save_state_file`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage::PERMISSIONS_FILE_NAME;

/// Schema marker of the persisted envelope (`{ "v": 1, "grants": [...] }`).
/// A later field addition is detectable by version rather than by guesswork.
pub const SCHEMA_VERSION: u64 = 1;

/// A direction-verb value from a path-surface label (`reads` → `read`,
/// `writes` → `write`, `reads and writes` → `both`). `None` on a grant
/// marks a verb-less surface (`bash` / `skill` / `mcp` / tool catch-all,
/// D11) that has no capability axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrantDirection {
    Read,
    Write,
    Both,
}

/// Pattern width, mirroring the permission system's `proven`/`family`
/// vocabulary: a proven-width grant covers exactly the paths the proof
/// produced; a family-width grant expands to both directions. `None`
/// again marks a verb-less surface (no axis to split).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrantWidth {
    Proven,
    Family,
}

/// One durable "…for this session" grant (the D3 record shape).
///
/// `direction`/`width` are `None` together on verb-less surfaces; the
/// store's dedupe key coalesces that `None` so re-grants are idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    /// Store-assigned unique id (hash of the dedupe key).
    pub id: String,
    /// Surface family name only — e.g. `external_directory`, `bash`,
    /// `skill` — never a surface+parameter composite.
    pub surface: String,
    /// The direction verb in the approved label; `None` for verb-less
    /// surfaces.
    pub direction: Option<GrantDirection>,
    /// The approved suggested pattern, verbatim (path glob / bash
    /// token-prefix / exact skill name).
    pub pattern: String,
    /// The approved pattern's width; `None` for verb-less surfaces.
    pub width: Option<GrantWidth>,
    /// Which worker earned the grant (audit).
    pub worker: String,
    /// When the operator approved it, RFC3339 UTC.
    pub created_at: String,
}

/// The persisted envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsFile {
    /// Schema marker (`1`).
    pub v: u64,
    pub grants: Vec<Grant>,
}

/// Health of a permission-store read: `Healthy` when the file parsed and
/// matched the v:1 schema; `Corrupt` when it was unreadable, unparseable,
/// or wrong-shaped. Both loads return a usable (possibly empty) store —
/// the health flag lets the keep/reset startup prompt avoid asking about
/// grants that could not be trusted (D12 corrupt-store skip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreHealth {
    Healthy,
    Corrupt,
}

/// The in-memory permission store: the grants plus how the persisted file
/// was read (D5 corrupt → empty-with-warning contract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionStore {
    pub grants: Vec<Grant>,
    pub health: StoreHealth,
}

/// A fresh, healthy, empty store (the normal first-run state).
pub fn empty_store() -> PermissionStore {
    PermissionStore {
        grants: Vec::new(),
        health: StoreHealth::Healthy,
    }
}

/// The store path for a resolved run-state root.
pub fn permissions_path(root: &Path) -> PathBuf {
    root.join(PERMISSIONS_FILE_NAME)
}

/// Format an epoch-millis instant as an RFC3339 UTC timestamp
/// (`2026-09-13T12:00:00Z`) — the `createdAt` serialization. Pure civil
/// math (days-from-civil), no local-timezone dependency, so the value is
/// deterministic and unit-testable.
pub fn format_rfc3339_utc(epoch_ms: u64) -> String {
    let days = (epoch_ms / 86_400_000) as i64;
    let ms_of_day = epoch_ms % 86_400_000;
    let (year, month, day) = civil_from_days(days);
    let hour = ms_of_day / 3_600_000;
    let minute = (ms_of_day / 60_000) % 60;
    let second = (ms_of_day / 1000) % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Days-since-epoch → proleptic Gregorian (year, month 1-12, day 1-31)
/// via the standard days-from-civil decomposition; negative-safe.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year, month as u64, day as u64)
}

/// Read the store. Never `Err`: a missing file is a healthy empty store
/// (nothing persisted yet); an unreadable / unparseable / wrong-shaped
/// file fails closed to an EMPTY store with a stderr warning (D5). The
/// caller distinguishes the two states via [`PermissionStore::health`].
pub fn load_permissions(root: &Path) -> PermissionStore {
    let Some(raw) = fs::read_to_string(permissions_path(root)).ok() else {
        return empty_store();
    };
    let Some(parsed) = serde_json::from_str(raw.as_str()).ok() else {
        return corrupt_empty();
    };
    let Some(grants) = coerce_file(&parsed) else {
        return corrupt_empty();
    };
    PermissionStore {
        grants,
        health: StoreHealth::Healthy,
    }
}

/// Persist the store as a whole-file atomic rewrite (temp file + rename)
/// under the run-state root — a kill mid-write cannot corrupt the store.
/// Best-effort: a failed write never crashes the run; the in-memory store
/// stays the caller's working copy. Saving after a corrupt read overwrites
/// the corrupt file with clean, valid JSON (a repair).
pub fn save_permissions(root: &Path, store: &PermissionStore) {
    let _ = fs::create_dir_all(root);
    let path = permissions_path(root);
    let tmp = path.with_extension("json.tmp");
    let file = PermissionsFile {
        v: SCHEMA_VERSION,
        grants: store.grants.clone(),
    };
    let result = (|| -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&file).unwrap_or_default();
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

/// Remove the persisted store entirely (the reset path — D6). Best-effort
/// like the state-file clear; an already-missing file clears to nothing.
pub fn clear_permissions(root: &Path) {
    let _ = fs::remove_file(permissions_path(root));
}

/// Add one grant, deduped on `(family, direction, pattern)` — the `None`
/// direction coalesces, so all verb-less entries of one family+pattern
/// share a key and re-granting an identical permission changes nothing
/// (idempotent). The store assigns the grant's `id`. Returns whether the
/// grant was newly added.
pub fn add_grant(store: &mut PermissionStore, grant: &Grant) -> bool {
    let key = dedupe_key(grant);
    if store.grants.iter().any(|g| dedupe_key(g) == key) {
        return false;
    }
    let mut copy = grant.clone();
    copy.id = grant_id(&key);
    store.grants.push(copy);
    true
}

/// The dedupe key: `(surface, direction, pattern)`. The `None` direction
/// coalesces to `<none>` so verb-less surfaces dedupe on
/// (family, pattern) alone.
fn dedupe_key(grant: &Grant) -> String {
    let direction = grant.direction.map(direction_label).unwrap_or("none");
    format!(
        "{}\u{1f}{}\u{1f}{}",
        grant.surface, direction, grant.pattern
    )
}

/// Store-assigned grant id: the first 12 hex chars of sha256 over the
/// dedupe key — stable across re-grants of the same permission and unique
/// within the store by construction (the key IS the store's uniqueness
/// predicate).
fn grant_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// The stable wire label of a direction (also the `dedupe_key` projection).
fn direction_label(d: GrantDirection) -> &'static str {
    match d {
        GrantDirection::Read => "read",
        GrantDirection::Write => "write",
        GrantDirection::Both => "both",
    }
}

/// Shape-check a parsed store into the v:1 grant list. `None` when the
/// value is not the expected envelope — a missing/non-numeric/unknown
/// `v`, a non-array `grants`, or any malformed grant member corrupts the
/// WHOLE file (`None`), so a store of mixed trust never half-approves.
fn coerce_file(parsed: &serde_json::Value) -> Option<Vec<Grant>> {
    let object = parsed.as_object()?;
    if object
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default()
        != SCHEMA_VERSION
    {
        return None;
    }
    let items = object.get("grants")?.as_array()?;
    let mut grants: Vec<Grant> = Vec::new();
    for item in items {
        let grant = coerce_grant(item)?;
        grants.push(grant);
    }
    Some(grants)
}

/// Shape-check one grant member; `None` on any missing or wrong-typed
/// field (nullable `direction`/`width` accept absent or explicit null).
fn coerce_grant(value: &serde_json::Value) -> Option<Grant> {
    let object = value.as_object()?;
    let id = object.get("id")?.as_str()?.to_string();
    let surface = object.get("surface")?.as_str()?.to_string();
    let pattern = object.get("pattern")?.as_str()?.to_string();
    let worker = object.get("worker")?.as_str()?.to_string();
    let created_at = object.get("createdAt")?.as_str()?.to_string();
    let direction = match object.get("direction") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(coerce_direction(v)?),
    };
    let width = match object.get("width") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(coerce_width(v)?),
    };
    Some(Grant {
        id,
        surface,
        direction,
        pattern,
        width,
        worker,
        created_at,
    })
}

fn coerce_direction(value: &serde_json::Value) -> Option<GrantDirection> {
    match value.as_str()? {
        "read" => Some(GrantDirection::Read),
        "write" => Some(GrantDirection::Write),
        "both" => Some(GrantDirection::Both),
        _ => None,
    }
}

fn coerce_width(value: &serde_json::Value) -> Option<GrantWidth> {
    match value.as_str()? {
        "proven" => Some(GrantWidth::Proven),
        "family" => Some(GrantWidth::Family),
        _ => None,
    }
}

/// The corrupt-path store: empty grants plus the D5 stderr warning (the
/// run continues, nothing auto-approves).
fn corrupt_empty() -> PermissionStore {
    eprintln!(
        "pi-plan: cannot read permissions.json (missing, unparseable, or wrong shape); \
treating it as empty — no stored grant will auto-approve until re-granted"
    );
    PermissionStore {
        grants: Vec::new(),
        health: StoreHealth::Corrupt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::storage::ProjectStorage;

    static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_cwd() -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "pi-plan-permissions-test-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn grant(
        surface: &str,
        direction: Option<GrantDirection>,
        pattern: &str,
        worker: &str,
    ) -> Grant {
        Grant {
            id: String::new(), // the store assigns ids on add
            surface: surface.to_string(),
            direction,
            pattern: pattern.to_string(),
            width: if direction.is_some() {
                Some(GrantWidth::Proven)
            } else {
                None
            },
            worker: worker.to_string(),
            created_at: "2026-09-13T12:00:00Z".to_string(),
        }
    }

    #[test]
    fn save_then_load_round_trips_the_v1_envelope() {
        let root = temp_cwd();
        let mut store = empty_store();
        assert!(
            add_grant(
                &mut store,
                &grant(
                    "external_directory",
                    Some(GrantDirection::Read),
                    "/home/tr/*",
                    "pi-plan-row-3"
                ),
            ),
            "first grant is new"
        );
        assert!(
            add_grant(
                &mut store,
                &grant("bash", None, "git status *", "pi-plan-row-9"),
            ),
            "verb-less grant is new"
        );
        save_permissions(&root, &store);

        let got = load_permissions(&root);
        assert_eq!(got.health, StoreHealth::Healthy);
        assert_eq!(got.grants.len(), 2);
        let read = &got.grants[0];
        assert_eq!(read.surface, "external_directory");
        assert_eq!(read.direction, Some(GrantDirection::Read));
        assert_eq!(read.pattern, "/home/tr/*");
        assert_eq!(read.width, Some(GrantWidth::Proven));
        assert_eq!(read.worker, "pi-plan-row-3");
        assert_eq!(read.created_at, "2026-09-13T12:00:00Z");
        assert!(!read.id.is_empty(), "the store assigned an id");
        let verbless = &got.grants[1];
        assert_eq!(verbless.surface, "bash");
        assert_eq!(
            verbless.direction, None,
            "verb-less direction round-trips null"
        );
        assert_eq!(verbless.width, None, "verb-less width round-trips null");
        assert_eq!(verbless.pattern, "git status *");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_persisted_file_uses_the_v1_schema_and_camel_case_fields() {
        let root = temp_cwd();
        let mut store = empty_store();
        add_grant(
            &mut store,
            &grant(
                "external_directory",
                Some(GrantDirection::Both),
                "/home/tr/*",
                "pi-plan-row-3",
            ),
        );
        save_permissions(&root, &store);

        let raw = fs::read_to_string(permissions_path(&root)).expect("read file");
        let parsed: serde_json::Value = serde_json::from_str(raw.as_str()).expect("parse json");
        let obj = parsed.as_object().expect("envelope is an object");
        assert_eq!(
            obj.get("v").and_then(serde_json::Value::as_f64),
            Some(1.0),
            "schema marker up front"
        );
        let grants = obj
            .get("grants")
            .and_then(serde_json::Value::as_array)
            .expect("grants");
        assert_eq!(grants.len(), 1);
        let g = grants[0].as_object().expect("grant object");
        assert_eq!(
            g.get("direction").and_then(serde_json::Value::as_str),
            Some("both")
        );
        assert_eq!(
            g.get("width").and_then(serde_json::Value::as_str),
            Some("proven")
        );
        assert_eq!(
            g.get("surface").and_then(serde_json::Value::as_str),
            Some("external_directory")
        );
        assert_eq!(
            g.get("worker").and_then(serde_json::Value::as_str),
            Some("pi-plan-row-3")
        );
        assert_eq!(
            g.get("createdAt").and_then(serde_json::Value::as_str),
            Some("2026-09-13T12:00:00Z"),
            "camelCase createdAt from snake_case field"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dedupe_makes_re_grants_idempotent_and_keeps_distinct_keys() {
        let mut store = empty_store();
        assert!(add_grant(
            &mut store,
            &grant(
                "external_directory",
                Some(GrantDirection::Read),
                "/home/tr/*",
                "pi-plan-row-3"
            ),
        ));
        assert!(
            !add_grant(
                &mut store,
                &grant(
                    "external_directory",
                    Some(GrantDirection::Read),
                    "/home/tr/*",
                    "pi-plan-row-5"
                ),
            ),
            "same (family, direction, pattern) is a no-op even from another worker"
        );
        assert_eq!(store.grants.len(), 1);

        // Verb-less surfaces coalesce the null direction: two bash grants
        // with the same pattern share one key.
        assert!(add_grant(
            &mut store,
            &grant("bash", None, "git status *", "pi-plan-row-9")
        ));
        assert!(
            !add_grant(
                &mut store,
                &grant("bash", None, "git status *", "pi-plan-row-12")
            ),
            "null direction coalesces for verb-less surfaces"
        );
        assert_eq!(store.grants.len(), 2);

        // A different pattern, a different direction, and a different
        // family are all distinct keys.
        assert!(add_grant(
            &mut store,
            &grant("bash", None, "git push *", "pi-plan-row-9")
        ));
        assert!(
            add_grant(
                &mut store,
                &grant(
                    "external_directory",
                    Some(GrantDirection::Write),
                    "/home/tr/*",
                    "pi-plan-row-5"
                ),
            ),
            "write is a distinct direction from read"
        );
        assert!(add_grant(
            &mut store,
            &grant("skill", None, "librarian", "pi-plan-row-9")
        ));
        assert_eq!(store.grants.len(), 5);
    }

    #[test]
    fn clear_removes_the_file() {
        let root = temp_cwd();
        let mut store = empty_store();
        add_grant(
            &mut store,
            &grant("bash", None, "git status *", "pi-plan-row-9"),
        );
        save_permissions(&root, &store);
        assert!(permissions_path(&root).exists());
        clear_permissions(&root);
        assert!(!permissions_path(&root).exists());
        // Loading after a clear is the normal first-run empty state.
        assert_eq!(load_permissions(&root).health, StoreHealth::Healthy);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_file_is_a_healthy_empty_store() {
        let root = temp_cwd();
        let store = load_permissions(&root);
        assert_eq!(store.health, StoreHealth::Healthy);
        assert!(store.grants.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_files_load_empty_with_corrupt_health() {
        for (name, content) in [
            ("unparseable", "{ nope !!"),
            ("not-an-object", r#"[1, 2, 3]"#),
            ("missing-v", r#"{"grants": []}"#),
            ("bad-v", r#"{"v": 2, "grants": []}"#),
            ("wrong-grants-type", r#"{"v": 1, "grants": "nope"}"#),
            (
                "malformed-grant-member",
                r#"{"v": 1, "grants": [{"surface": "bash"}]}"#,
            ),
            (
                "wrong-direction-type",
                r#"{"v": 1, "grants": [{"id": "x", "surface": "bash", "direction": 7, "pattern": "git status *", "worker": "w", "createdAt": "t"}]}"#,
            ),
        ] {
            let root = temp_cwd();
            let path = permissions_path(&root);
            fs::create_dir_all(&root).expect("root dir");
            fs::write(path, content).expect("write corrupt file");
            let store = load_permissions(&root);
            assert_eq!(
                store.health,
                StoreHealth::Corrupt,
                "{name} must read as corrupt"
            );
            assert!(
                store.grants.is_empty(),
                "{name} must fail closed to an empty store"
            );
            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn atomic_write_leaves_no_temp_file_behind() {
        let root = temp_cwd();
        let mut store = empty_store();
        add_grant(
            &mut store,
            &grant("bash", None, "git status *", "pi-plan-row-9"),
        );
        save_permissions(&root, &store);
        assert!(
            !root.join("permissions.json.tmp").exists(),
            "temp file must be renamed away"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn permissions_path_resolves_under_the_state_dir_override() {
        // The store joins the run-state family: resolving the root through
        // `ProjectStorage::resolve` (which honors $PI_PLAN_STATE_DIR) puts
        // permissions.json inside the override, never anywhere else.
        let cwd = temp_cwd();
        let override_dir = temp_cwd();
        let override_label = override_dir.to_string_lossy().into_owned();
        let root = ProjectStorage::resolve(
            Some("/home/u"),
            Some(override_label.as_str()),
            cwd.as_path(),
        )
        .expect("resolve with override");
        assert!(
            root.to_string_lossy()
                .into_owned()
                .starts_with(override_dir.to_string_lossy().into_owned().as_str())
        );
        let mut store = empty_store();
        add_grant(
            &mut store,
            &grant("skill", None, "librarian", "pi-plan-row-9"),
        );
        save_permissions(root.as_path(), &store);
        assert!(
            permissions_path(root.as_path()).exists(),
            "the store lands under the $PI_PLAN_STATE_DIR override"
        );
        assert_eq!(load_permissions(root.as_path()).grants.len(), 1);
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&override_dir);
    }

    #[test]
    fn format_rfc3339_utc_renders_known_instants() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_rfc3339_utc(1_700_000_000_000),
            "2023-11-14T22:13:20Z"
        );
        // A leap day stays sane.
        assert_eq!(
            format_rfc3339_utc(1_582_934_400_000),
            "2020-02-29T00:00:00Z"
        );
    }
}
