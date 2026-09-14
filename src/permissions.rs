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

use crate::rpc::{ExtensionUiRequest, UiMethod};
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

/// Whether the start-of-run keep/reset prompt must open (D12): the store
/// must be HEALTHY and hold ≥ 1 grant. A corrupt store skips the prompt
/// (its warning surfaces once in the final report); an empty store has
/// nothing to reset. `status`/`mark` never consult this — they never
/// prompt by construction.
pub fn wants_keep_reset_prompt(store: &PermissionStore) -> bool {
    store.health == StoreHealth::Healthy && !store.grants.is_empty()
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

// ---------------- label parsing (the extension's session vocabulary) --------
//
// The parser reads the permission system's own session-option strings —
// pi-plan never parses shell; the extension's tree-sitter pipeline already
// produced the suggested pattern. Unrecognized labels degrade to `None`
// (prompt), never to an auto-approval (label-skew risk, isolated here).

/// A parsed "… for this session" option label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantLabel {
    /// The direction verb (`reads` → `read` …); `None` marks a verb-less
    /// surface (D11).
    pub direction: Option<GrantDirection>,
    /// The quoted suggested pattern, verbatim.
    pub pattern: String,
    /// The surface phrase a verb-less label names (`bash`, `skill`,
    /// `access to external directory`, a tool name, …); `None` for
    /// directional labels (their surface family is path by construction).
    pub surface: Option<String>,
}

/// Parse one session-option string into a grant label; `None` for plain
/// `Yes`/`No`/`No, provide reason`, pattern-less labels (`Yes, for this
/// session`, `… to N paths for this session`), and anything else.
///
/// Accepted grammar (the extension's session-approval vocabulary,
/// `presentation/pattern-suggest.ts`):
///   `Yes, allow reads to "⟨p⟩" for this session`
///   `Yes, allow writes to "⟨p⟩" for this session`
///   `Yes, allow reads and writes to "⟨p⟩" for this session`
///   `Yes, allow [⟨surface phrase⟩] "⟨p⟩" for this session`  (verb-less)
/// plus the plan-documented phrase-less `Yes, allow "⟨p⟩" for this
/// session`. The surface phrase is at most four words (`access to external
/// directory` is the longest) — anything with more structure, multi-path
/// labels, or unquoted text is rejected (degrade to prompt, never
/// auto-approve on confusion).
pub fn parse_session_label(label: &str) -> Option<GrantLabel> {
    let trimmed = label.trim();
    let rest0 = trimmed.strip_prefix("Yes, allow ")?;
    let mut rest = rest0;
    let mut direction: Option<GrantDirection> = None;
    let mut had_direction = false;
    if let Some(after) = rest.strip_prefix("reads to ") {
        direction = Some(GrantDirection::Read);
        had_direction = true;
        rest = after;
    } else if let Some(after) = rest.strip_prefix("writes to ") {
        direction = Some(GrantDirection::Write);
        had_direction = true;
        rest = after;
    } else if let Some(after) = rest.strip_prefix("reads and writes to ") {
        direction = Some(GrantDirection::Both);
        had_direction = true;
        rest = after;
    }

    // Char-based scan (no str slicing): collect the pre-quote surface
    // phrase (≤ 3 spaces → at most four words), then the quoted pattern,
    // then require the closing quote to be followed exactly by
    // ` for this session`.
    let suffix = " for this session".chars().collect::<Vec<char>>();
    let mut pre = String::new();
    let mut pre_spaces: usize = 0;
    let mut pattern = String::new();
    let mut after_open = false;
    let mut found_close = false;
    let mut suffix_pos: usize = 0;
    let mut tail_ok = true;
    for c in rest.chars() {
        if !after_open {
            if c == '"' {
                after_open = true;
            } else {
                pre.push(c);
                if c == ' ' {
                    pre_spaces += 1;
                    if pre_spaces > 4 {
                        return None; // more than the four-word phrase + space
                    }
                }
            }
            continue;
        }
        if !found_close {
            if c == '"' {
                found_close = true;
            } else {
                pattern.push(c);
            }
            continue;
        }
        if suffix_pos < suffix.len() {
            if c != suffix[suffix_pos] {
                tail_ok = false;
                break;
            }
            suffix_pos += 1;
        } else if c != ' ' {
            tail_ok = false;
            break;
        }
    }
    if !after_open || !found_close || !tail_ok || suffix_pos != suffix.len() {
        return None;
    }
    let phrase = pre.trim();
    if had_direction && !phrase.is_empty() {
        // A directional label (`reads to "…"`) never names a surface
        // phrase — that combination is not the extension's vocabulary.
        return None;
    }
    let surface = if phrase.is_empty() {
        None
    } else {
        Some(phrase.to_string())
    };
    Some(GrantLabel {
        direction,
        pattern,
        surface,
    })
}

// ---------------- ask view (the dialog's own facts) --------

/// One session option of the ask that parsed as a grant label: the raw
/// option string (the reply payload) plus its parsed label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOption {
    /// The exact option string — what an auto-approval replies verbatim.
    pub raw: String,
    pub label: GrantLabel,
}

/// The supervisor's view of one relayed permission dialog, built from the
/// ask's OWN facts and options (never from shell parsing): the surface
/// family, the pending command (rule #3), the flagged paths, and every
/// parseable session option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskView {
    /// Canonical surface family, when derivable from the title facts.
    pub surface: Option<String>,
    /// The pending `command : …` fact (always-grant #3).
    pub command: Option<String>,
    /// Flagged paths: the `path : …` core facts, the `external path : …`
    /// evidence lines, and — only when no path facts exist — the quoted
    /// glob in the session options. A bash command-prefix pattern
    /// (`git status *`) is NOT a flagged path (D7 non-vacuous guard).
    pub flagged_paths: Vec<String>,
    /// The session options that parse as grant labels, in dialog order.
    pub session_options: Vec<SessionOption>,
}

/// Split one dialog title line into `(fact, value)` when it is a
/// `label : value` fact row; `None` for plain text rows.
fn split_fact(line: &str) -> Option<(String, String)> {
    let sep = line.find(':')?;
    let label = line[..sep].trim();
    if label.is_empty() {
        return None;
    }
    let value = line[sep + 1..].trim();
    if value.is_empty() {
        return None;
    }
    Some((label.to_string(), value.to_string()))
}

/// Canonical surface family for BOTH stored grants and asks: the
/// path-shaped directional surfaces (`path`, `path_read`, `path_write`,
/// `external_directory`, `external_directory_read`,
/// `external_directory_write`) all canonicalize to `path` — the dialog
/// facts cannot distinguish in-cwd from out-of-cwd, and the matching glob
/// covers the same access either way.
pub fn canonical_family(surface: &str) -> String {
    if surface == "path"
        || surface == "path_read"
        || surface == "path_write"
        || surface == "external_directory"
        || surface == "external_directory_read"
        || surface == "external_directory_write"
    {
        "path".to_string()
    } else {
        surface.to_string()
    }
}

/// A flagged path candidate: only patterns that start like filesystem
/// paths (`/` or `~/`) count as flagged paths — a bash command-prefix
/// suggestion (`git status *`) is not one.
fn looks_like_path(pattern: &str) -> bool {
    pattern.starts_with("/") || pattern.starts_with("~/")
}

impl AskView {
    /// Build the view from the raw dialog request. The surface family is
    /// derived from the facts: an explicit `surface :` fact wins, then
    /// `tool : bash` → `bash`, then path facts → `path`, then a `skill :`
    /// fact → `skill`, then the `tool :` family itself.
    pub fn from_request(req: &ExtensionUiRequest) -> Self {
        let title = req
            .title
            .as_deref()
            .unwrap_or("")
            .lines()
            .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
            .collect::<Vec<String>>();
        let mut path_facts: Vec<String> = Vec::new();
        let mut external_paths: Vec<String> = Vec::new();
        let mut command: Option<String> = None;
        let mut tool_fact: Option<String> = None;
        let mut surface_fact: Option<String> = None;
        let mut skill_fact: Option<String> = None;
        for line in title {
            let Some((fact, value)) = split_fact(line.as_str()) else {
                continue;
            };
            match fact.as_str() {
                "path" => path_facts.push(value),
                "external path" => external_paths.push(value),
                "command" => command = Some(value),
                "tool" => tool_fact = Some(value),
                "surface" => surface_fact = Some(value),
                "skill" => skill_fact = Some(value),
                _ => {}
            }
        }
        // The session options are the strongest surface signal: a
        // DIRECTIONED option (`reads/writes/… to "…"`) means the ask is
        // gated on a path surface (bash-external asks resolve their file
        // access to a path glob + verb); a verb-less option is the bash /
        // skill / tool surface's own command-shaped suggestion.
        let mut session_options: Vec<SessionOption> = Vec::new();
        for option in req.options.iter() {
            if let Some(label) = parse_session_label(option.as_str()) {
                session_options.push(SessionOption {
                    raw: option.clone(),
                    label,
                });
            }
        }
        let has_directioned_option = session_options.iter().any(|o| o.label.direction.is_some());
        let mut label_phrase: Option<String> = None;
        for opt in session_options.iter() {
            if let Some(surface) = opt.label.surface.as_ref() {
                label_phrase = Some(surface.clone());
                break;
            }
        }
        // Surface precedence: an explicit `surface :` fact wins; a
        // DIRECTIONED option means a path-surface ask; a verb-less label's
        // own surface phrase (`bash`/`skill`/`path`/…) names the family;
        // else path facts → `path`, then `tool : bash` → `bash`, then a
        // `skill :` fact → `skill`, then the `tool :` family itself.
        let mut surface: Option<String> = surface_fact.map(|s| canonical_family(s.as_str()));
        if surface.is_none() {
            if has_directioned_option {
                surface = Some("path".to_string());
            } else if let Some(phrase) = label_phrase.as_ref() {
                surface = Some(canonical_family(phrase.as_str()));
            } else if !path_facts.is_empty() || !external_paths.is_empty() {
                surface = Some("path".to_string());
            } else if tool_fact.as_deref() == Some("bash") {
                surface = Some("bash".to_string());
            } else if skill_fact.is_some() {
                surface = Some("skill".to_string());
            } else {
                surface = tool_fact.map(|s| canonical_family(s.as_str()));
            }
        }

        let mut flagged_paths: Vec<String> = path_facts;
        for p in external_paths {
            flagged_paths.push(p);
        }
        if flagged_paths.is_empty() {
            for opt in session_options.iter() {
                if looks_like_path(opt.label.pattern.as_str()) {
                    flagged_paths.push(opt.label.pattern.clone());
                }
            }
        }

        AskView {
            surface,
            command,
            flagged_paths,
            session_options,
        }
    }
}

// ---------------- per-surface containment --------
//
// Containment is per-surface grammar (D9/D11): path globs use glob
// containment (`*` crosses `/`, `?` one char), bash uses token-segment
// prefix containment (`git status *` covers `git status --short`, never
// `git push`), skill uses exact names.

/// Split a command (or suggestion) into whitespace-separated tokens.
/// Quotes stay inside their token; the extension's suggestions carry none.
fn word_tokens(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for c in s.chars() {
        if c.is_whitespace() {
            if !current.is_empty() {
                out.push(current);
                current = String::new();
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Bash token-segment prefix containment: the covering pattern's real
/// tokens (before its trailing `*`) must prefix the ask's tokens; when the
/// cover ends in `*`, it swallows the rest (including another `*`). A
/// cover without a trailing `*` is an exact-command grant and only covers
/// an identical, non-broader ask. A bare `*` (no concrete token) covers
/// nothing — D11 requires ≥ 1 concrete token.
pub fn bash_contains(cover: &str, ask: &str) -> bool {
    let cover_tokens = word_tokens(cover);
    let ask_tokens = word_tokens(ask);
    let cover_star = cover_tokens.last().cloned() == Some("*".to_string());
    let ask_star = ask_tokens.last().cloned() == Some("*".to_string());
    let cover_len = if cover_star {
        cover_tokens.len() - 1
    } else {
        cover_tokens.len()
    };
    let ask_len = if ask_star {
        ask_tokens.len() - 1
    } else {
        ask_tokens.len()
    };
    if cover_len == 0 {
        // D11: a bare `*` (no concrete token) covers nothing.
        return false;
    }
    if cover_len > ask_len {
        return false;
    }
    for i in 0..cover_len {
        if cover_tokens[i] != ask_tokens[i] {
            return false;
        }
    }
    if cover_star {
        true
    } else {
        !ask_star && cover_len == ask_len
    }
}

/// One NFA char transition for a glob position set: `*` loops in place,
/// `?` consumes any char, a literal consumes only itself.
fn glob_move(pattern: &[char], set: &[usize], c: char) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for i in set.iter() {
        if *i >= pattern.len() {
            continue;
        }
        let pc = pattern[*i];
        if pc == '*' {
            out.push(*i);
        } else if pc == '?' || pc == c {
            out.push(*i + 1);
        }
    }
    out
}

/// Epsilon closure of a glob position set (every `*` may match the empty
/// string and skip to the next position).
fn glob_eps_closure(pattern: &[char], set: &[usize]) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for i in set.iter() {
        stack.push(*i);
    }
    while let Some(i) = stack.pop() {
        if !out.contains(&i) {
            out.push(i);
            if i < pattern.len() && pattern[i] == '*' {
                stack.push(i + 1);
            }
        }
    }
    out.sort();
    out
}

/// Exact glob-language containment for the glob grammar `*` (any run,
/// crosses `/`) and `?` (one character): TRUE iff every string matched by
/// `ask` is also matched by `cover`.
///
/// Decision procedure: a bounded product search over `ask`'s NFA
/// positions × the reachable position-SET of `cover`'s NFA (the subset
/// construction merged with `ask`'s own states). Branching only on the
/// alphabet of literal characters present in either pattern (plus one
/// sentinel standing for every char outside the union), the search finds a
/// witness accepted by `ask` but rejected by `cover` iff `cover` does not
/// contain `ask`; the visited set terminates it. Exact, so reflexivity and
/// transitivity hold by construction (proptested).
pub fn glob_contains(cover: &str, ask: &str) -> bool {
    let cover_c = cover.chars().collect::<Vec<char>>();
    let ask_c = ask.chars().collect::<Vec<char>>();
    let n_cover = cover_c.len();
    let n_ask = ask_c.len();

    // The alphabet that matters: the literal chars of either pattern,
    // plus one sentinel for every char outside the union.
    let mut chars: Vec<char> = Vec::new();
    for c in cover_c.iter().chain(ask_c.iter()) {
        if *c != '*' && *c != '?' && !chars.contains(c) {
            chars.push(*c);
        }
    }
    chars.push('\u{0}');

    let start_ask: Vec<usize> = glob_eps_closure(&ask_c, &[0usize]);
    let start_cover: Vec<usize> = glob_eps_closure(&cover_c, &[0usize]);
    if start_ask.contains(&n_ask) && !start_cover.contains(&n_cover) {
        return false; // the empty string is a witness
    }
    let start = (start_ask, start_cover);
    let mut visited: Vec<(Vec<usize>, Vec<usize>)> = vec![start.clone()];
    let mut frontier: Vec<(Vec<usize>, Vec<usize>)> = vec![start];
    while let Some((ask_set, cover_set)) = frontier.pop() {
        for c in chars.iter() {
            let ask_next = glob_eps_closure(&ask_c, &glob_move(&ask_c, &ask_set, *c));
            let cover_next = glob_eps_closure(&cover_c, &glob_move(&cover_c, &cover_set, *c));
            if ask_next.contains(&n_ask) && !cover_next.contains(&n_cover) {
                return false; // witness accepted by ask, rejected by cover
            }
            let state = (ask_next, cover_next);
            if !visited.contains(&state) {
                visited.push(state.clone());
                frontier.push(state);
            }
        }
    }
    true
}

/// The containment grammar for a canonical surface family: path globs,
/// bash token-prefix, skill exact names; anything else never contains.
fn contains_for(family: &str, cover: &str, ask: &str) -> bool {
    match family {
        "path" => glob_contains(cover, ask),
        "bash" => bash_contains(cover, ask),
        "skill" => cover == ask,
        _ => false,
    }
}

/// D11 recordability guard, enforced defensively on the MATCHing side
/// too: a verb-less grant is only matchable when it could have been
/// recorded — bash with ≥ 1 concrete token before the trailing `*`;
/// skill exact names only. `mcp`, tool catch-alls, and bare `/`-star
/// patterns (which recording never writes) can never auto-approve
/// wholesale.
pub fn grant_is_matchable(grant: &Grant) -> bool {
    if grant.direction.is_some() {
        // A directioned grant is the operator's approved width on a path
        // glob — always matchable.
        return true;
    }
    let family = canonical_family(grant.surface.as_str());
    match family.as_str() {
        "bash" => word_tokens(grant.pattern.as_str()).iter().any(|t| t != "*"),
        "skill" => !grant.pattern.contains('*') && !grant.pattern.contains('?'),
        _ => false,
    }
}

/// Direction compatibility (D3/D11): a verb-less grant has no capability
/// axis and skips the check; a directioned grant covers an ask whose own
/// label direction equals it, or a `both`-width grant covers either.
fn direction_covers(grant: Option<GrantDirection>, ask: Option<GrantDirection>) -> bool {
    match (grant, ask) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(g), Some(a)) => g == a || g == GrantDirection::Both,
    }
}

/// Whether a stored grant covers the ask: family equality (canonical),
/// direction compatibility, and per-surface pattern containment against
/// at least one of the ask's own suggested options.
pub fn grant_covers(grant: &Grant, view: &AskView) -> bool {
    if !grant_is_matchable(grant) {
        return false;
    }
    let family = canonical_family(grant.surface.as_str());
    if view.surface.clone().unwrap_or_default() != family {
        return false;
    }
    view.session_options.iter().any(|opt| {
        direction_covers(grant.direction, opt.label.direction)
            && contains_for(
                family.as_str(),
                grant.pattern.as_str(),
                opt.label.pattern.as_str(),
            )
    })
}

// ---------------- always-grants (D7) --------

/// The environment every always-grant decision needs: the project root,
/// HOME (for `~/…` expansion), and the derived skills roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionEnv {
    /// Project root (cwd) — rule #1.
    pub cwd: PathBuf,
    /// HOME for `~/…` expansion in flagged paths (best-effort).
    pub home: Option<String>,
    /// Derived skills roots — rules #2/#3 (`$PI_PLAN_SKILL`,
    /// `$PI_PLAN_CLEAN_SKILL`, then `$HOME/.pi/agent/skills`).
    pub skills_roots: Vec<PathBuf>,
}

impl PermissionEnv {
    /// Build the environment from the resolved cwd and env vars — the
    /// skills roots mirror the repo's own skill-flag precedence, so a
    /// relocated skill keeps its read auto-approval.
    pub fn from_env(
        cwd: &Path,
        home: Option<&str>,
        skill_env: Option<&str>,
        clean_skill_env: Option<&str>,
    ) -> Self {
        let mut skills_roots: Vec<PathBuf> = Vec::new();
        for p in [skill_env, clean_skill_env] {
            if let Some(dir) = p.filter(|v| !v.is_empty()) {
                skills_roots.push(Path::new(dir).to_path_buf());
            }
        }
        if let Some(h) = home.filter(|h| !h.is_empty()) {
            skills_roots.push(Path::new(h).join(".pi").join("agent").join("skills"));
        }
        PermissionEnv {
            cwd: cwd.to_path_buf(),
            home: home.map(String::from),
            skills_roots,
        }
    }
}

/// The auto-reply payload for a covered ask (D10: always machine
/// generated — the recorder must never persist it durably).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoReply {
    /// Plain one-time `Yes` — rule #3 (skill scripts), or a path-covered
    /// ask whose label carries no pattern.
    PlainYes,
    /// The session-option string the worker records in ITS OWN session
    /// rules (so it stops re-asking mid-run).
    SessionOption(String),
}

/// The supervisor's decision for one relayed permission dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogVerdict {
    /// Nothing covers the ask — render and prompt exactly as today.
    Prompt,
    /// Auto-approve without the operator (D10: generated, never recorded).
    AutoApprove(AutoReply),
}

/// True when `flagged` (a concrete path or a path glob) lies at or under
/// `root`. A `~/` prefix expands via `home` when known; a glob keeps only
/// its literal prefix (the suggested patterns are parent-dir globs like
/// `/home/tr/*`); a `*`-led prefix (empty or bare-root literal) is never
/// "within" — that keeps the non-vacuous guard honest.
fn path_within(flagged: &str, root: &str, home: Option<&str>) -> bool {
    let mut owned: String = flagged.to_string();
    if let Some(after) = flagged.strip_prefix("~/")
        && let Some(h) = home
    {
        owned = format!("{h}/{after}");
    }
    let mut literal = String::new();
    for c in owned.chars() {
        if c == '*' || c == '?' {
            break;
        }
        literal.push(c);
    }
    let dir = literal.strip_suffix('/').unwrap_or(&literal).to_string();
    if dir.is_empty() || dir == "/" {
        return false;
    }
    let root_slash = format!("{root}/");
    dir == root || dir.starts_with(root_slash.as_str()) || (root == "/" && dir.starts_with("/"))
}

/// Whether the ask is read-direction for rule #2: some session option's
/// label carries the `reads` verb. (The permission system offers a
/// read-verb option exactly on read asks; write asks offer only
/// `writes`/`both`.)
fn ask_is_read(view: &AskView) -> bool {
    view.session_options
        .iter()
        .any(|o| o.label.direction == Some(GrantDirection::Read))
}

/// Split an absolute path into its non-empty `/`-separated segments.
fn path_segments(p: &str) -> Vec<String> {
    p.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect::<Vec<String>>()
}

/// Rule #3: the command references a script under
/// `<root>/**/scripts/**` — any whitespace token that expands to a path
/// strictly deeper than the root whose segments past the root's own
/// contain a `scripts` segment. `~` expands via `home`; surrounding
/// quotes are stripped (no shell parsing — only token/path structure).
fn command_references_scripts(command: &str, root: &str, home: Option<&str>) -> bool {
    let root_segments = path_segments(root);
    if root_segments.is_empty() {
        return false;
    }
    for raw in word_tokens(command) {
        let mut token: String = raw;
        if let Some(after) = token.as_str().strip_prefix("~/")
            && let Some(h) = home
        {
            token = format!("{h}/{after}");
        }
        let unquoted = token.as_str().strip_prefix('\'').unwrap_or(token.as_str());
        let unquoted = unquoted.strip_suffix('\'').unwrap_or(unquoted);
        let unquoted = unquoted.strip_prefix('"').unwrap_or(unquoted);
        let unquoted = unquoted.strip_suffix('"').unwrap_or(unquoted);
        let segments = path_segments(unquoted);
        if segments.len() <= root_segments.len() {
            continue;
        }
        let mut under_root = true;
        for i in 0..root_segments.len() {
            if segments[i] != root_segments[i] {
                under_root = false;
                break;
            }
        }
        if !under_root {
            continue;
        }
        // Beyond the root's own segments, a `scripts` segment must appear
        // (the script is under `<root>/**/scripts/**`).
        let references_scripts = segments
            .iter()
            .skip(root_segments.len())
            .any(|seg| seg.as_str() == "scripts");
        if references_scripts {
            return true;
        }
    }
    false
}

/// The first parseable session option as the auto-reply payload, or plain
/// `Yes` when the ask is path-covered but its label carries no pattern.
fn first_session_option(view: &AskView) -> AutoReply {
    match view.session_options.first() {
        Some(opt) => AutoReply::SessionOption(opt.raw.clone()),
        None => AutoReply::PlainYes,
    }
}

/// Supervisor-side always-grants (D7), evaluated before stored grants in
/// rule order: #1 every flagged path within the project root (any
/// direction); #2 every flagged path within a skills root AND the ask is
/// read-direction; #3 a bash command referencing `<root>/**/scripts/**`.
/// Each needs ≥ 1 flagged path — a path-less bash ask (`git push`,
/// `curl …`) never satisfies #1/#2 vacuously.
pub fn always_grant_verdict(view: &AskView, env: &PermissionEnv) -> Option<AutoReply> {
    let cwd_label = env.cwd.to_string_lossy().into_owned();
    let in_cwd = !view.flagged_paths.is_empty()
        && view
            .flagged_paths
            .iter()
            .all(|p| path_within(p.as_str(), cwd_label.as_str(), env.home.as_deref()));
    if in_cwd {
        return Some(first_session_option(view));
    }
    if !view.flagged_paths.is_empty() && ask_is_read(view) {
        let in_skills = env.skills_roots.iter().any(|root| {
            let root_label = root.to_string_lossy().into_owned();
            view.flagged_paths
                .iter()
                .all(|p| path_within(p.as_str(), root_label.as_str(), env.home.as_deref()))
        });
        if in_skills {
            return Some(first_session_option(view));
        }
    }
    if let Some(cmd) = view.command.as_deref() {
        for root in env.skills_roots.iter() {
            let root_label = root.to_string_lossy().into_owned();
            if command_references_scripts(cmd, root_label.as_str(), env.home.as_deref()) {
                return Some(AutoReply::PlainYes);
            }
        }
    }
    None
}

/// The full decision for one relayed permission dialog (step 4's pre-arm):
/// always-grants first (D7), then the stored grants; anything uncovered
/// prompts exactly as today. `AutoApprove` replies are machine-generated
/// (D10) — the caller must never record them durably.
pub fn decide(
    store: &PermissionStore,
    env: &PermissionEnv,
    req: &ExtensionUiRequest,
) -> DialogVerdict {
    let view = AskView::from_request(req);
    if let Some(reply) = always_grant_verdict(&view, env) {
        return DialogVerdict::AutoApprove(reply);
    }
    for grant in store.grants.iter() {
        if grant_covers(grant, &view) {
            return DialogVerdict::AutoApprove(first_session_option(&view));
        }
    }
    DialogVerdict::Prompt
}

// ---------------- step 4: the dialog proxy ---------------

/// The pre-arm verdict (step 4): `Some(reply)` — machine-generated
/// (D10), never recorded — when an always-grant or a stored grant covers
/// the ask; `None` → the operator must see the dialog exactly as today.
pub fn auto_approval(
    store: &PermissionStore,
    env: &PermissionEnv,
    req: &ExtensionUiRequest,
) -> Option<AutoReply> {
    match decide(store, env, req) {
        DialogVerdict::AutoApprove(reply) => Some(reply),
        DialogVerdict::Prompt => None,
    }
}

/// The grant an operator-chosen reply would record (D10/D11), or `None`
/// when nothing may be recorded: the ask must be a SELECT dialog whose
/// option set byte-contains the replied label, the label must parse as a
/// session grant (never plain `Yes`, never pattern-less), and the grant
/// must be matchable (D11: bash needs ≥ 1 concrete token, skill exact
/// names, `mcp`/tool catch-alls and any bare-`*` pattern never record).
fn recordable_grant(
    req: &ExtensionUiRequest,
    reply: &str,
    worker: &str,
    created_at_ms: u64,
) -> Option<Grant> {
    if req.method != UiMethod::Select {
        return None;
    }
    if !req.options.iter().any(|o| o.as_str() == reply) {
        return None;
    }
    let view = AskView::from_request(req);
    let surface: &String = view.surface.as_ref()?;
    let label = parse_session_label(reply)?;
    if label.pattern == "*" {
        // D11: the tool catch-all `*` and any other bare-`*` pattern are
        // never recorded — recording would auto-approve a whole surface.
        return None;
    }
    let grant = Grant {
        id: String::new(),
        surface: surface.clone(),
        direction: label.direction,
        pattern: label.pattern,
        width: if label.direction.is_some() {
            Some(GrantWidth::Proven)
        } else {
            None
        },
        worker: worker.to_string(),
        created_at: format_rfc3339_utc(created_at_ms),
    };
    if !grant_is_matchable(&grant) {
        // D11 enforced defensively on the WRITE side too: an un-matchable
        // grant must never enter the store (it would skew the status count
        // and the keep/reset prompt for nothing).
        return None;
    }
    Some(grant)
}

/// Step 4 recorder (D10): attempt to durably add the grant an OPERATOR
/// chose on a human dialog. Only the human path calls this — the
/// auto-approval pre-arm replies are structurally excluded, so
/// machine-generated approvals can never accrue precedents. Returns
/// whether a NEW grant was added (re-grant dedupes, idempotent); the
/// caller persists to disk when `true`.
pub fn record_human_reply(
    store: &mut PermissionStore,
    req: &ExtensionUiRequest,
    reply: &str,
    worker: &str,
    created_at_ms: u64,
) -> bool {
    let Some(grant) = recordable_grant(req, reply, worker, created_at_ms) else {
        return false;
    };
    add_grant(store, &grant)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::rpc::{ExtensionUiRequest, UiMethod};
    use crate::storage::ProjectStorage;
    use proptest::prelude::*;

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

    // ---------------- step 2: matching core ----------------

    fn req(title: &str, options: Vec<String>) -> ExtensionUiRequest {
        ExtensionUiRequest {
            id: "ui-1".to_string(),
            method: UiMethod::Select,
            title: Some(title.to_string()),
            message: None,
            options,
            placeholder: None,
            prefill: None,
            timeout_ms: None,
        }
    }

    fn env(cwd: &str) -> PermissionEnv {
        PermissionEnv {
            cwd: Path::new(cwd).to_path_buf(),
            home: Some("/home/tr".to_string()),
            skills_roots: vec![Path::new("/home/tr/.pi/agent/skills").to_path_buf()],
        }
    }

    fn grant_for(surface: &str, direction: Option<GrantDirection>, pattern: &str) -> Grant {
        Grant {
            id: String::new(),
            surface: surface.to_string(),
            direction,
            pattern: pattern.to_string(),
            width: if direction.is_some() {
                Some(GrantWidth::Proven)
            } else {
                None
            },
            worker: "pi-plan-row-3".to_string(),
            created_at: "2026-09-13T12:00:00Z".to_string(),
        }
    }

    fn ask_view(surface: &str, choices: &[(Option<GrantDirection>, String)]) -> AskView {
        let mut session_options: Vec<SessionOption> = Vec::new();
        for (d, p) in choices.iter() {
            session_options.push(SessionOption {
                raw: format!("option: {p}"),
                label: GrantLabel {
                    direction: *d,
                    pattern: p.clone(),
                    surface: None,
                },
            });
        }
        AskView {
            surface: Some(surface.to_string()),
            command: None,
            flagged_paths: Vec::new(),
            session_options,
        }
    }

    #[test]
    fn parses_the_session_option_vocabulary() {
        let Some(l) = parse_session_label(r##"Yes, allow reads to "/home/tr/*" for this session"##)
        else {
            panic!("read label must parse");
        };
        assert_eq!(l.direction, Some(GrantDirection::Read));
        assert_eq!(l.pattern, "/home/tr/*");

        let Some(l) =
            parse_session_label(r##"Yes, allow writes to "/home/tr/*" for this session"##)
        else {
            panic!("write label must parse");
        };
        assert_eq!(l.direction, Some(GrantDirection::Write));
        assert_eq!(l.pattern, "/home/tr/*");

        let Some(l) = parse_session_label(
            r##"Yes, allow reads and writes to "/home/tr/*" for this session"##,
        ) else {
            panic!("both label must parse");
        };
        assert_eq!(l.direction, Some(GrantDirection::Both));

        // Verb-less shapes (bash token-prefix, skill exact name), with the
        // label's own surface phrase named.
        let Some(l) = parse_session_label(r##"Yes, allow bash "git status *" for this session"##)
        else {
            panic!("verb-less label must parse");
        };
        assert_eq!(l.direction, None);
        assert_eq!(l.pattern, "git status *");
        assert_eq!(l.surface.as_deref(), Some("bash"));
        let Some(l) = parse_session_label(r##"Yes, allow skill "librarian" for this session"##)
        else {
            panic!("exact name must parse");
        };
        assert_eq!(l.pattern, "librarian");
        assert_eq!(l.surface.as_deref(), Some("skill"));
        // The longest real surface phrase (external directory): four words.
        let Some(l) = parse_session_label(
            r##"Yes, allow access to external directory "/home/tr/*" for this session"##,
        ) else {
            panic!("four-word phrase must parse");
        };
        assert_eq!(l.pattern, "/home/tr/*");
        assert_eq!(l.surface.as_deref(), Some("access to external directory"));
        // A directional label names no surface (path by construction).
        let Some(l) = parse_session_label(r##"Yes, allow reads to "/home/tr/*" for this session"##)
        else {
            panic!("directional label must parse");
        };
        assert_eq!(l.surface, None);
    }

    #[test]
    fn rejects_non_grant_and_pattern_less_labels() {
        assert_eq!(parse_session_label("Yes"), None, "one-time Yes");
        assert_eq!(parse_session_label("No"), None);
        assert_eq!(parse_session_label("No, provide reason"), None);
        assert_eq!(
            parse_session_label("Yes, for this session"),
            None,
            "pattern-less session offer"
        );
        assert_eq!(
            parse_session_label(r##"Yes, allow reads to 2 paths for this session"##),
            None,
            "multi-path label carries no single pattern"
        );
        assert_eq!(
            parse_session_label(r##"Yes, allow reads to "/a" and "/b" for this session"##),
            None,
            "two quoted patterns are not one"
        );
        assert_eq!(
            parse_session_label("Yes, allow reads to ssh host \"x\" for this session"),
            None,
            "a directional label never carries a surface phrase"
        );
        assert_eq!(parse_session_label("garbage"), None);
        assert_eq!(parse_session_label(""), None);
    }

    #[test]
    fn ask_view_derives_surface_facts_and_flagged_paths() {
        // bash ask with external-path evidence; the verb-less label names
        // its own surface phrase (`bash`).
        let v = AskView::from_request(&req(
            "Permission Required\ntool : bash\ncommand : cat /home/tr/t.txt\nexternal path : /home/tr/t.txt",
            vec![
                r##"Yes, allow bash "cat *" for this session"##.to_string(),
                "No".to_string(),
            ],
        ));
        assert_eq!(v.surface.as_deref(), Some("bash"));
        assert_eq!(v.command.as_deref(), Some("cat /home/tr/t.txt"));
        assert!(v.flagged_paths.contains(&"/home/tr/t.txt".to_string()));
        assert_eq!(v.session_options.len(), 1);

        // path-surface ask from an explicit `path :` fact.
        let v = AskView::from_request(&req(
            "Permission Required\npath : /home/tr/repo/out/x\ntool : write",
            vec![r##"Yes, allow writes to "/home/tr/repo/out/*" for this session"##.to_string()],
        ));
        assert_eq!(v.surface.as_deref(), Some("path"));
        assert!(v.flagged_paths.contains(&"/home/tr/repo/out/x".to_string()));

        // A bash command-prefix pattern is NOT a flagged path (D7).
        let v = AskView::from_request(&req(
            "Permission Required\ntool : bash\ncommand : git status",
            vec![r##"Yes, allow "git status *" for this session"##.to_string()],
        ));
        assert!(
            v.flagged_paths.is_empty(),
            "command-prefix patterns are not paths"
        );

        // Unknown surface degrades to None (no facts at all).
        let v = AskView::from_request(&req(
            "Permission Required",
            vec![r##"Yes, allow "git status *" for this session"##.to_string()],
        ));
        assert_eq!(v.surface, None);
    }

    #[test]
    fn canonical_family_unifies_the_path_shaped_surfaces() {
        assert_eq!(canonical_family("path"), "path");
        assert_eq!(canonical_family("path_read"), "path");
        assert_eq!(canonical_family("path_write"), "path");
        assert_eq!(canonical_family("external_directory"), "path");
        assert_eq!(canonical_family("external_directory_read"), "path");
        assert_eq!(canonical_family("external_directory_write"), "path");
        assert_eq!(canonical_family("bash"), "bash");
        assert_eq!(canonical_family("skill"), "skill");
    }

    #[test]
    fn path_glob_containment_covers_subtrees_and_not_siblings() {
        // `*` crosses `/`: /home/tr/* covers /home/tr/x/* and everything
        // beneath it, and a bare `*` covers everything.
        assert!(glob_contains("/home/tr/*", "/home/tr/*"));
        assert!(glob_contains("/home/tr/*", "/home/tr/x/*"));
        assert!(glob_contains("/home/tr/*", "/home/tr/x/y/z/*"));
        assert!(glob_contains("*", "/home/tr/x/*"));
        assert!(glob_contains("/home/tr/x/*", "/home/tr/x/y/*"));
        // Siblings and parents are NOT covered.
        assert!(!glob_contains("/home/tr/*", "/home/trx/*"));
        assert!(
            !glob_contains("/home/tr/x/*", "/home/tr/*"),
            "narrower never covers wider"
        );
        assert!(
            !glob_contains("/home/tr/*", "/home/tr"),
            "a literal file needs the trailing slash+star boundary"
        );
        // `?` is one character.
        assert!(glob_contains("/home/tr/?bc/*", "/home/tr/abc/*"));
        assert!(!glob_contains("/home/tr/a?c/*", "/home/tr/axxx/*"));
    }

    #[test]
    fn bash_token_prefix_containment_covers_command_families() {
        assert!(bash_contains("git *", "git status *"));
        assert!(bash_contains("git status *", "git status --short"));
        assert!(bash_contains("git status *", "git status -s"));
        assert!(bash_contains("git status *", "git status *"));
        assert!(!bash_contains("git status *", "git push"));
        assert!(!bash_contains("git *", "curl *"));
        assert!(!bash_contains("git status *", "git"));
        assert!(
            !bash_contains("*", "git status *"),
            "a bare `*` has no concrete token (D11)"
        );
        // An exact-command grant (no trailing star) only covers an
        // identical, no-broader ask.
        assert!(bash_contains("git status", "git status"));
        assert!(!bash_contains("git status", "git status --short"));
    }

    #[test]
    fn grant_covers_matches_family_direction_and_pattern_grammar() {
        // Path family: read grant covers read ask, `both` covers everything.
        let read_grant = grant_for(
            "external_directory",
            Some(GrantDirection::Read),
            "/home/tr/*",
        );
        assert!(grant_covers(
            &read_grant,
            &ask_view(
                "path",
                &[(Some(GrantDirection::Read), "/home/tr/*".to_string())]
            ),
        ));
        assert!(
            !grant_covers(
                &read_grant,
                &ask_view(
                    "path",
                    &[(Some(GrantDirection::Write), "/home/tr/*".to_string())]
                ),
            ),
            "a read grant never covers a write ask"
        );
        let both = grant_for("path", Some(GrantDirection::Both), "/home/tr/*");
        assert!(grant_covers(
            &both,
            &ask_view(
                "path",
                &[(Some(GrantDirection::Read), "/home/tr/*".to_string())]
            ),
        ));
        assert!(
            grant_covers(
                &both,
                &ask_view(
                    "path",
                    &[(Some(GrantDirection::Write), "/home/tr/*".to_string())]
                ),
            ),
            "family width covers both directions"
        );

        // Family mismatch: a bash grant never covers a path ask.
        let bash_grant = grant_for("bash", None, "git status *");
        assert!(!grant_covers(
            &bash_grant,
            &ask_view(
                "path",
                &[(Some(GrantDirection::Read), "/home/tr/*".to_string())]
            ),
        ));
        // …and a verb-less bash grant covers narrower bash asks.
        assert!(
            grant_covers(
                &bash_grant,
                &ask_view("bash", &[(None, "git status --short".to_string())]),
            ),
            "token-prefix covers the narrower command"
        );
        assert!(!grant_covers(
            &bash_grant,
            &ask_view("bash", &[(None, "git push".to_string())]),
        ));

        // Skill: exact names only.
        let skill_grant = grant_for("skill", None, "librarian");
        assert!(grant_covers(
            &skill_grant,
            &ask_view("skill", &[(None, "librarian".to_string())]),
        ));
        assert!(
            !grant_covers(
                &skill_grant,
                &ask_view("skill", &[(None, "librar".to_string())]),
            ),
            "exact-only: a prefix is not a match"
        );
        assert!(
            !grant_covers(
                &grant_for("skill", None, "librar*"),
                &ask_view("skill", &[(None, "librarian".to_string())]),
            ),
            "a wildcard skill grant is never matchable (D11)"
        );
    }

    #[test]
    fn stored_catch_alls_never_match() {
        // D11: verb-less catch-alls are never recorded and never match.
        for g in [
            grant_for("bash", None, "*"),
            grant_for("skill", None, "*"),
            grant_for("mcp", None, "*"),
            grant_for("mcp", None, "!*"),
        ] {
            assert!(
                !grant_covers(&g, &ask_view("bash", &[(None, "git *".to_string())]))
                    && !grant_covers(&g, &ask_view("skill", &[(None, "librarian".to_string())])),
                "catch-all {g:?} must not approve wholesale"
            );
        }
    }

    #[test]
    fn always_grant_one_covers_in_cwd_writes_but_not_pathless_bash() {
        let store = empty_store();
        let project = env("/home/tr/repo");
        // In-cwd write: flagged path inside cwd → auto-approved (any
        // direction) with the session option as the reply.
        let req_write = req(
            "Permission Required\npath : /home/tr/repo/out/hello.txt\ntool : write",
            vec![
                "Yes".to_string(),
                r##"Yes, allow writes to "/home/tr/repo/out/*" for this session"##.to_string(),
                "No".to_string(),
            ],
        );
        match decide(&store, &project, &req_write) {
            DialogVerdict::AutoApprove(AutoReply::SessionOption(opt)) => assert_eq!(
                opt,
                r##"Yes, allow writes to "/home/tr/repo/out/*" for this session"##
            ),
            other => panic!("in-cwd write must auto-approve: {other:?}"),
        }
        // …and the same for an in-cwd read.
        let req_read = req(
            "Permission Required\npath : /home/tr/repo/src/main.rs\ntool : read",
            vec![
                "Yes".to_string(),
                r##"Yes, allow reads to "/home/tr/repo/src/*" for this session"##.to_string(),
            ],
        );
        assert!(matches!(
            decide(&store, &project, &req_read),
            DialogVerdict::AutoApprove(_)
        ));
        // Non-vacuous guard: a path-less bash ask (`git status`) still
        // prompts even with an empty store.
        let req_bash = req(
            "Permission Required\ntool : bash\ncommand : git status",
            vec![
                "Yes".to_string(),
                r##"Yes, allow "git status *" for this session"##.to_string(),
                "No".to_string(),
            ],
        );
        assert_eq!(
            decide(&store, &project, &req_bash),
            DialogVerdict::Prompt,
            "no file access → never a vacuous auto-approval"
        );
    }

    #[test]
    fn always_grant_two_covers_skill_reads_but_not_writes() {
        let store = empty_store();
        let project = env("/home/tr/repo");
        // #2: read-direction ask under the skills root → auto-approve.
        let req_read = req(
            "Permission Required\npath : /home/tr/.pi/agent/skills/foo/lib.md\ntool : read",
            vec![
                "Yes".to_string(),
                r##"Yes, allow reads to "/home/tr/.pi/agent/skills/foo/*" for this session"##
                    .to_string(),
            ],
        );
        assert!(
            matches!(
                decide(&store, &project, &req_read),
                DialogVerdict::AutoApprove(_)
            ),
            "skills-root read auto-approves"
        );
        // Write into the skills tree still prompts (the boundary).
        let req_write = req(
            "Permission Required\npath : /home/tr/.pi/agent/skills/foo/custom.md\ntool : write",
            vec![
                "Yes".to_string(),
                r##"Yes, allow writes to "/home/tr/.pi/agent/skills/foo/*" for this session"##
                    .to_string(),
            ],
        );
        assert_eq!(
            decide(&store, &project, &req_write),
            DialogVerdict::Prompt,
            "write into the skills tree is never auto-approved"
        );
    }

    #[test]
    fn always_grant_three_approves_skill_scripts_once() {
        let store = empty_store();
        let project = env("/home/tr/repo");
        // #3: a bash command referencing <skill>/scripts/… → plain Yes.
        let req_script = req(
            "Permission Required\ntool : bash\ncommand : bash /home/tr/.pi/agent/skills/foo/scripts/run.sh",
            vec!["Yes".to_string(), "No".to_string()],
        );
        assert_eq!(
            decide(&store, &project, &req_script),
            DialogVerdict::AutoApprove(AutoReply::PlainYes)
        );
        // A stray script outside the skills tree still prompts.
        let req_stray = req(
            "Permission Required\ntool : bash\ncommand : bash /tmp/stray.sh",
            vec!["Yes".to_string(), "No".to_string()],
        );
        assert_eq!(decide(&store, &project, &req_stray), DialogVerdict::Prompt);
        // A sibling scripts dir of a root that merely CONTAINS the token
        // path is fine; the root prefix must match segment-wise.
        let req_different = req(
            "Permission Required\ntool : bash\ncommand : bash /home/tr/.pi/agent/other/scripts/run.sh",
            vec!["Yes".to_string(), "No".to_string()],
        );
        assert_eq!(
            decide(&store, &project, &req_different),
            DialogVerdict::Prompt
        );
    }

    #[test]
    fn stored_grants_cover_later_asks_with_the_same_suggested_pattern() {
        // ask 1 (`cat ~/text_file.txt`): out-of-cwd read → prompts.
        let project = env("/home/tr/repo");
        let req1 = req(
            "Permission Required\nexternal path : /home/tr/text_file.txt\ntool : bash\ncommand : cat /home/tr/text_file.txt",
            vec![
                "Yes".to_string(),
                r##"Yes, allow reads to "/home/tr/*" for this session"##.to_string(),
                "No".to_string(),
            ],
        );
        let fresh = empty_store();
        assert_eq!(
            decide(&fresh, &project, &req1),
            DialogVerdict::Prompt,
            "first ask prompts"
        );

        // The operator approves the session option; the store records it
        // with the SAME suggested pattern the extension proved.
        let mut store = empty_store();
        let Some(label) =
            parse_session_label(r##"Yes, allow reads to "/home/tr/*" for this session"##)
        else {
            panic!("label parses");
        };
        assert!(add_grant(
            &mut store,
            &Grant {
                id: String::new(),
                surface: "external_directory".to_string(),
                direction: label.direction,
                pattern: label.pattern.clone(),
                width: Some(GrantWidth::Proven),
                worker: "pi-plan-row-1".to_string(),
                created_at: "2026-09-13T12:00:00Z".to_string(),
            }
        ));

        // ask 2 — a differently-spelled command over the same files
        // (`cd ~; cat text_file.txt`): the suggested glob is identical, so
        // the precedent auto-approves (no prompt).
        let req2 = req(
            "Permission Required\nexternal path : /home/tr/text_file.txt\ntool : bash\ncommand : cd /home/tr; cat text_file.txt",
            vec![
                "Yes".to_string(),
                r##"Yes, allow reads to "/home/tr/*" for this session"##.to_string(),
                "No".to_string(),
            ],
        );
        match decide(&store, &project, &req2) {
            DialogVerdict::AutoApprove(AutoReply::SessionOption(opt)) => assert_eq!(
                opt,
                r##"Yes, allow reads to "/home/tr/*" for this session"##
            ),
            other => panic!("the stored precedent covers the second spelling: {other:?}"),
        }
    }

    #[test]
    fn verb_less_bash_round_trip_auto_approves_git_status_but_never_git_push() {
        let project = env("/home/tr/repo");
        let mut store = empty_store();
        // The earlier `git status` grant is stored verb-less, pattern
        // `git status *` (D11).
        assert!(add_grant(
            &mut store,
            &grant_for("bash", None, "git status *")
        ));
        // A later, differently-spelled `git status` ask auto-approves…
        let req_status = req(
            "Permission Required\ntool : bash\ncommand : git status --short",
            vec![
                "Yes".to_string(),
                r##"Yes, allow "git status *" for this session"##.to_string(),
            ],
        );
        assert!(matches!(
            decide(&store, &project, &req_status),
            DialogVerdict::AutoApprove(_)
        ));
        // …but a `git push` under the same store still does NOT.
        let req_push = req(
            "Permission Required\ntool : bash\ncommand : git push origin main",
            vec![
                "Yes".to_string(),
                r##"Yes, allow "git push *" for this session"##.to_string(),
            ],
        );
        assert_eq!(
            decide(&store, &project, &req_push),
            DialogVerdict::Prompt,
            "`git status *` never covers `git push`"
        );
    }

    #[test]
    fn unknown_or_unparseable_asks_prompt_exactly_as_today() {
        let store = empty_store();
        let project = env("/home/tr/repo");
        // An out-of-cwd ask with PLAIN options only (no session grant
        // offered, nothing parseable, not covered by any always-grant)
        // prompts exactly as today.
        let req_plain = req(
            "Permission Required\ntool : bash\ncommand : cat /home/tr/t.txt\nexternal path : /home/tr/t.txt",
            vec![
                "Yes".to_string(),
                "No".to_string(),
                "No, provide reason".to_string(),
            ],
        );
        assert_eq!(decide(&store, &project, &req_plain), DialogVerdict::Prompt);
        // A multi-path ask is path-covered (rule #1) but pattern-less:
        // it still auto-approves with plain `Yes` (D9), never records.
        let req_multi = req(
            "Permission Required\npath : /home/tr/repo/a\npath : /home/tr/repo/b\ntool : write",
            vec!["Yes, for this session".to_string(), "No".to_string()],
        );
        assert_eq!(
            decide(&store, &project, &req_multi),
            DialogVerdict::AutoApprove(AutoReply::PlainYes)
        );
    }

    #[test]
    fn corrupt_store_fails_closed_to_prompts() {
        // A corrupt store loads empty (step 1), so `decide` can only
        // consult always-grants; nothing stored ever half-approves.
        let mut store = empty_store();
        store.health = StoreHealth::Corrupt;
        let project = env("/home/tr/repo");
        let req_out = req(
            "Permission Required\nexternal path : /home/tr/text_file.txt\ntool : bash",
            vec![
                "Yes".to_string(),
                r##"Yes, allow reads to "/home/tr/*" for this session"##.to_string(),
            ],
        );
        assert_eq!(decide(&store, &project, &req_out), DialogVerdict::Prompt);
    }

    #[test]
    fn permission_env_derives_skills_roots_with_env_precedence() {
        let e = PermissionEnv::from_env(
            Path::new("/repo"),
            Some("/home/u"),
            Some("/skills/mine"),
            None,
        );
        let rendered = e
            .skills_roots
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<String>>();
        assert_eq!(
            rendered,
            vec![
                "/skills/mine".to_string(),
                "/home/u/.pi/agent/skills".to_string(),
            ]
        );
    }

    // ---------------- step 4: the dialog proxy ----------------

    #[test]
    fn record_human_reply_persists_only_operator_chosen_session_grants() {
        let mut store = empty_store();
        let ask = req(
            "Permission Required\nexternal path : /home/tr/text_file.txt\ntool : bash",
            vec![
                "Yes".to_string(),
                r##"Yes, allow reads to "/home/tr/*" for this session"##.to_string(),
                "No".to_string(),
            ],
        );
        // The operator picks the session option → one grant records, with
        // the ask's derived (canonical) surface family.
        assert!(record_human_reply(
            &mut store,
            &ask,
            r##"Yes, allow reads to "/home/tr/*" for this session"##,
            "pi-plan-worker-1".to_string().as_str(),
            1_786_000_000_000,
        ));
        assert_eq!(store.grants.len(), 1);
        assert_eq!(store.grants[0].surface, "path");
        assert_eq!(store.grants[0].direction, Some(GrantDirection::Read));
        assert_eq!(store.grants[0].pattern, "/home/tr/*");
        assert_eq!(store.grants[0].worker, "pi-plan-worker-1");
        assert_eq!(store.grants[0].created_at, "2026-08-06T07:06:40Z");
        // Re-recording the same permission is a dedupe no-op (idempotent).
        assert!(!record_human_reply(
            &mut store,
            &ask,
            r##"Yes, allow reads to "/home/tr/*" for this session"##,
            "pi-plan-worker-2".to_string().as_str(),
            1_786_000_000_000,
        ));
        assert_eq!(store.grants.len(), 1);
        // A DIFFERENT permission on the same dialog is a new grant.
        let req_write = req(
            "Permission Required\nexternal path : /home/tr/text_file.txt\ntool : bash",
            vec![
                r##"Yes, allow reads to "/home/tr/*" for this session"##.to_string(),
                r##"Yes, allow reads and writes to "/home/tr/*" for this session"##.to_string(),
            ],
        );
        assert!(record_human_reply(
            &mut store,
            &req_write,
            r##"Yes, allow reads and writes to "/home/tr/*" for this session"##,
            "pi-plan-worker-1".to_string().as_str(),
            1_786_000_000_000,
        ));
        assert_eq!(store.grants.len(), 2);
        assert_eq!(store.grants[1].direction, Some(GrantDirection::Both));
    }

    #[test]
    fn record_human_reply_never_records_plain_yes_pattern_less_or_non_select() {
        let mut store = empty_store();
        let req_multi = req(
            "Permission Required\npath : /home/tr/repo/a\npath : /home/tr/repo/b\ntool : write",
            vec![
                "Yes".to_string(),
                "Yes, for this session".to_string(),
                "No".to_string(),
            ],
        );
        // Plain one-time `Yes` (D4) is never a precedent.
        assert!(!record_human_reply(&mut store, &req_multi, "Yes", "w", 0));
        // A pattern-less session label (multi-path / direction-less ask,
        // D9) is never recordable.
        assert!(!record_human_reply(
            &mut store,
            &req_multi,
            "Yes, for this session",
            "w",
            0,
        ));
        // A reply string NOT among the ask's options is never trusted
        // (D10: the option set must byte-contain the replied label).
        assert!(!record_human_reply(
            &mut store,
            &req_multi,
            r##"Yes, allow writes to "/home/tr/repo/*" for this session"##,
            "w",
            0,
        ));
        assert_eq!(store.grants.len(), 0);

        // A non-select dialog never records, even with a grant-looking
        // label (only selects offer the extension's option set).
        let confirm = ExtensionUiRequest {
            id: "ui-2".to_string(),
            method: UiMethod::Confirm,
            title: Some("Permission Required".to_string()),
            message: None,
            options: vec![r##"Yes, allow reads to "/home/tr/*" for this session"##.to_string()],
            placeholder: None,
            prefill: None,
            timeout_ms: None,
        };
        assert!(!record_human_reply(
            &mut store,
            &confirm,
            r##"Yes, allow reads to "/home/tr/*" for this session"##,
            "w",
            0,
        ));
        assert_eq!(store.grants.len(), 0);
    }

    #[test]
    fn record_human_reply_accepts_verb_less_bash_and_exact_skill_labels() {
        let mut store = empty_store();
        // Verb-less bash label: D11 records it with direction/width null
        // and the bash family, so matching skips the axis check.
        let bash = req(
            "Permission Required\ntool : bash\ncommand : git status",
            vec![r##"Yes, allow "git status *" for this session"##.to_string()],
        );
        assert!(record_human_reply(
            &mut store,
            &bash,
            r##"Yes, allow "git status *" for this session"##,
            "w",
            0,
        ));
        assert_eq!(store.grants.len(), 1);
        assert_eq!(store.grants[0].surface, "bash");
        assert_eq!(store.grants[0].direction, None);
        assert_eq!(store.grants[0].width, None);
        assert_eq!(store.grants[0].pattern, "git status *");
        assert_eq!(store.grants[0].created_at, "1970-01-01T00:00:00Z");

        // An exact skill name records under the skill family.
        let skill = req(
            "Permission Required\nskill : librarian",
            vec![r##"Yes, allow skill "librarian" for this session"##.to_string()],
        );
        assert!(record_human_reply(
            &mut store,
            &skill,
            r##"Yes, allow skill "librarian" for this session"##,
            "w",
            0,
        ));
        assert_eq!(store.grants.len(), 2);
        assert_eq!(store.grants[1].surface, "skill");
        assert_eq!(store.grants[1].pattern, "librarian");
        assert_eq!(store.grants[1].direction, None);
    }

    #[test]
    fn record_human_reply_never_writes_bare_star_catch_alls() {
        let mut store = empty_store();
        // The tool catch-all `*` (D11) is never recorded.
        let tool = req(
            "Permission Required\ntool : bash\ncommand : git status",
            vec![r##"Yes, allow "*" for this session"##.to_string()],
        );
        assert!(!record_human_reply(
            &mut store,
            &tool,
            r##"Yes, allow "*" for this session"##,
            "w",
            0,
        ));
        // …and neither is ANY other bare-`*` pattern, even on a path ask.
        let path = req(
            "Permission Required\npath : /home/tr/repo/a\ntool : write",
            vec![r##"Yes, allow reads to "*" for this session"##.to_string()],
        );
        assert!(!record_human_reply(
            &mut store,
            &path,
            r##"Yes, allow reads to "*" for this session"##,
            "w",
            0,
        ));
        // An mcp surface label never records (no MCP tools in workers).
        let mcp = req(
            "Permission Required\ntool : mcp\ncommand : ls",
            vec![r##"Yes, allow mcp "ls" for this session"##.to_string()],
        );
        assert!(!record_human_reply(
            &mut store,
            &mcp,
            r##"Yes, allow mcp "ls" for this session"##,
            "w",
            0,
        ));
        assert_eq!(store.grants.len(), 0);
    }

    #[test]
    fn auto_approval_helper_returns_generated_replies_and_no_for_uncovered() {
        let store = empty_store();
        let project = env("/home/tr/repo");
        // Always-grant #1 (in-cwd write): the machine replies with the
        // session option so the worker stops re-asking mid-run (D10).
        let in_cwd = req(
            "Permission Required\npath : /home/tr/repo/out/hello.txt\ntool : write",
            vec![r##"Yes, allow writes to "/home/tr/repo/out/*" for this session"##.to_string()],
        );
        match auto_approval(&store, &project, &in_cwd) {
            Some(AutoReply::SessionOption(opt)) => assert_eq!(
                opt,
                r##"Yes, allow writes to "/home/tr/repo/out/*" for this session"##
            ),
            other => panic!("in-cwd ask auto-approves: {other:?}"),
        }
        // An uncovered out-of-cwd ask → None (the operator sees it).
        let out_cwd = req(
            "Permission Required\nexternal path : /home/tr/text_file.txt\ntool : bash",
            vec![r##"Yes, allow reads to "/home/tr/*" for this session"##.to_string()],
        );
        assert_eq!(auto_approval(&store, &project, &out_cwd), None);
        // A path-covered but pattern-less ask (D9) is approved with plain
        // `Yes`, still machine-generated.
        let multi = req(
            "Permission Required\npath : /home/tr/repo/a\npath : /home/tr/repo/b\ntool : write",
            vec!["Yes, for this session".to_string(), "No".to_string()],
        );
        assert_eq!(
            auto_approval(&store, &project, &multi),
            Some(AutoReply::PlainYes)
        );
    }

    // ---------------- step 5: keep/reset prompt ----------------

    #[test]
    fn keep_reset_prompt_fires_only_for_a_healthy_non_empty_store() {
        // Absent/empty store → no prompt.
        assert!(!wants_keep_reset_prompt(&empty_store()));
        // A grant on file → prompt.
        let mut store = empty_store();
        assert!(add_grant(
            &mut store,
            &grant_for(
                "external_directory",
                Some(GrantDirection::Read),
                "/home/tr/*"
            )
        ));
        assert!(wants_keep_reset_prompt(&store));
        // After a reset the store is empty again → no prompt.
        store.grants = Vec::new();
        assert!(!wants_keep_reset_prompt(&store));
        // A corrupt store never prompts, whatever it holds (D12: the
        // warning surfaces in the final report instead).
        let mut corrupt = empty_store();
        assert!(add_grant(
            &mut corrupt,
            &grant_for(
                "external_directory",
                Some(GrantDirection::Read),
                "/home/tr/*"
            )
        ));
        corrupt.health = StoreHealth::Corrupt;
        assert!(!wants_keep_reset_prompt(&corrupt));
    }

    fn glob_strategy() -> impl Strategy<Value = String> {
        "/[ab/*?]{0,12}"
    }

    proptest! {
        /// Containment is reflexive: every pattern covers itself, handles
        /// star/star and literal interleavings intact.
        #[test]
        fn glob_containment_is_reflexive(g in glob_strategy()) {
            prop_assert!(glob_contains(&g, &g), "reflexive: {g}");
        }

        /// Containment is transitive — the exact decision procedure makes
        /// `a ⊇ b ⊇ c ⟹ a ⊇ c` hold for every generated triple.
        #[test]
        fn glob_containment_is_transitive(
            a in glob_strategy(),
            b in glob_strategy(),
            c in glob_strategy(),
        ) {
            if glob_contains(&a, &b) && glob_contains(&b, &c) {
                prop_assert!(
                    glob_contains(&a, &c),
                    "{a} ⊇ {b} ⊇ {c} violates transitivity"
                );
            }
        }
    }
}
