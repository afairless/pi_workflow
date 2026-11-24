//! CLI surface (step 8): clap subcommand parsing, the one-shot `stop`
//! control-file IPC, `mark <n> done` adjudication, the
//! `reset-permissions` store clear, config/persona/skill resolution, and
//! the read-only `status` report builder.
//!
//! Everything interactive (stdin, the supervise UI loop, command dispatch)
//! lives in the binary; this module is parsing + pure helpers so the CLI
//! contract is unit-testable without spawning processes.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};

use crate::config::{
    DEFAULT_MODEL, SupervisorConfig, coerce_config, default_global_config_json, read_config_file,
};
use crate::git::{MatchTier, match_planned};
use crate::hole::{detect_holes, is_done};
use crate::prompt::strip_skill_frontmatter;
use crate::state::{
    STATE_FILE_NAME, SupervisorState, plan_hash_of, read_state_file, save_state_file,
};
use crate::supervise::{
    RowOutcome, RunRecord, VerifierReport, bundle_evidence, read_todo_file, run_outcome_label,
};
use crate::todo::{TodoPlan, TodoRow, parse_plan};
use crate::ui::{
    action_hint_label, format_cost, format_duration, format_tokens, short_hash, verdict_upper,
};
use crate::worker::{WorkerSnapshot, now_epoch_ms};

// ---------------- clap surface ----------------

/// pi-plan — deterministic orchestrator for the TODO.md workflow.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The literal command word for `mark <n> done`, as a clap `ValueEnum`:
/// the only accepted value kebab-cases to `done`, so any other word fails
/// at parse time with clap's usage error (exit 2) instead of the runtime
/// `Err` path (exit 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MarkWord {
    /// Mark the row done (adjudicated).
    Done,
}

/// Subcommands of pi-plan.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Supervise the TODO.md plan (or one row) with a fresh pi worker per row.
    Supervise {
        /// Only supervise this row number, then stop (default: next unmatched row).
        #[arg(long)]
        row: Option<u64>,
        /// Pre-answered reply for the row's ASK question (folds into the
        /// first worker prompt).
        #[arg(long)]
        answer: Option<String>,
        /// Path to a supervisor.config.json.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Path to a theme JSON (overrides the active pi theme from settings).
        #[arg(long)]
        theme: Option<PathBuf>,
    },
    /// Print a status summary (plan, git matches, persisted state).
    Status,
    /// Ask a running `supervise` to stop at the next boundary.
    Stop,
    /// Mark a row done even without a matching commit: `mark <n> done`.
    Mark {
        /// Row number to mark done.
        row: u64,
        /// The literal command word "done".
        #[arg(value_enum)]
        done: MarkWord,
    },
    /// Clear every stored project permission: `reset-permissions [--yes]`.
    ResetPermissions {
        /// Skip the confirmation prompt (default: ask first).
        #[arg(long)]
        yes: bool,
    },
    /// Supervise exactly one row, then exit.
    Step {
        /// Row number to supervise.
        row: u64,
        /// Pre-answered reply for the row's ASK question.
        #[arg(long)]
        answer: Option<String>,
        /// Path to a supervisor.config.json.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Path to a theme JSON (overrides the active pi theme from settings).
        #[arg(long)]
        theme: Option<PathBuf>,
    },
}

// ---------------- one-shot stop control file ----------------

/// One-shot stop-request file name (bare, resolved against the run-state
/// root). `supervise` discards a stale file at startup so a killed run's
/// request is never inherited, then watches for it while running and
/// consumes it at the next boundary (plan step 8).
pub const STOP_FILE_NAME: &str = ".pi-plan-stop";

pub fn stop_file_path(root: &Path) -> PathBuf {
    root.join(STOP_FILE_NAME)
}

/// Best-effort write of a stop request; a failed write must not fail `stop`.
pub fn write_stop_request(root: &Path) {
    let _ = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(stop_file_path(root))?;
        f.write_all(b"requested\n")?;
        Ok(())
    })();
}

/// Remove a stop-request file (startup discard / consume-on-read).
pub fn clear_stop_request(root: &Path) {
    let _ = fs::remove_file(stop_file_path(root));
}

/// True when a stop request is pending under `root`.
pub fn stop_request_present(root: &Path) -> bool {
    fs::read_to_string(stop_file_path(root)).ok().is_some()
}

// ---------------- mark <n> done ----------------

/// Merge row `n` into `state.adjudicated` and persist under `root`. Creates
/// a state file when none exists (tied to the current plan hash) so a `mark`
/// before the first `supervise` still takes effect; the run-state root is
/// created on demand by the save. Fails when the plan has no such
/// row. A repeated mark is idempotent.
pub fn mark_done(cwd: &Path, root: &Path, todo: &TodoPlan, row: u64) -> Result<(), String> {
    if !todo.rows.iter().any(|r| r.number == row) {
        return Err(format!("no such row {row} in TODO.md"));
    }
    let mut state = read_state_file(root).unwrap_or_else(|| {
        let content = read_todo_file(cwd);
        SupervisorState {
            plan_hash: plan_hash_of(content.as_str()),
            current_row: row,
            runs_used: 0,
            last_outcome: "adjudicated".to_string(),
            adjudicated: Vec::new(),
            verified: Vec::new(),
            verified_cache: Vec::new(),
            agent_id: None,
            started_at: None,
        }
    });
    if !state.adjudicated.contains(&row) {
        state.adjudicated.push(row);
        state.adjudicated.sort_by_key(|&n| n);
    }
    save_state_file(root, &state);
    Ok(())
}

// ---------------- config / persona / skill ----------------

/// Where the winning supervisor config came from — one of the four
/// layering tiers (surfaced at supervise startup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// `--config PATH` was passed (absolute precedence).
    Explicit(PathBuf),
    /// `<cwd>/supervisor.config.json` was present.
    Project(PathBuf),
    /// The XDG global file was used.
    Global(PathBuf),
    /// Nothing resolved — built-in defaults.
    Builtin,
}

/// A resolved config plus the tier it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedConfig {
    pub config: SupervisorConfig,
    pub source: ConfigSource,
}

/// Resolve the global config path: `$XDG_CONFIG_HOME` (non-empty) wins,
/// else `$HOME/.config`, joined with `pi-plan/supervisor.config.json`;
/// `None` when neither is known (no global config possible).
pub fn global_config_path(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home
        && !xdg.is_empty()
    {
        return Some(
            Path::new(xdg)
                .join("pi-plan")
                .join("supervisor.config.json"),
        );
    }
    home.map(|h| {
        Path::new(h)
            .join(".config")
            .join("pi-plan")
            .join("supervisor.config.json")
    })
}

/// Best-effort scaffold: `mkdir -p` the parent, then write the default
/// global config **only when the file does not already exist** (never
/// overwrites a user edit). Non-atomic exists-then-write is benign under
/// concurrency (identical deterministic bytes; a reader catching a partial
/// document falls through to defaults). Any I/O failure returns `Err` but
/// MUST NOT fail the run — the caller falls back to built-in defaults.
pub fn ensure_global_config(path: &Path) -> std::io::Result<()> {
    (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            fs::write(path, default_global_config_json().as_str())?;
        }
        Ok(())
    })()
}

/// Resolve the supervisor config, layered highest-first: `--config PATH`
/// (absolute), else `<cwd>/supervisor.config.json` when present, else the
/// XDG global file when it exists, else built-in defaults. Never throws
/// (missing/corrupt files fall back to defaults), and the winning tier
/// rides along so startup can show where the config came from.
pub fn resolve_config(
    cwd: &Path,
    config_path: Option<&Path>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> ResolvedConfig {
    let Some(path) = config_path else {
        let project_path = cwd.join("supervisor.config.json");
        if project_path.exists() {
            return ResolvedConfig {
                config: read_config_file(project_path.as_path()),
                source: ConfigSource::Project(project_path),
            };
        }
        if let Some(global) = global_config_path(xdg_config_home, home) {
            // Auto-create the scaffold when absent (best-effort: an I/O
            // failure falls back to built-in defaults, never fails a run).
            if !global.exists() {
                let _ = ensure_global_config(global.as_path());
            }
            // A present global file is read fresh so a corrupt (unparseable)
            // one collapses to built-in defaults — the file contributes
            // nothing, so the resolution is `Builtin`, not `Global`. The
            // project tier above is presence-based (a corrupt project file
            // still shadows); the global tier must parse to count.
            let Some(raw) = fs::read_to_string(global.as_path()).ok() else {
                return ResolvedConfig {
                    config: SupervisorConfig::default(),
                    source: ConfigSource::Builtin,
                };
            };
            let Some(parsed) = serde_json::from_str(raw.as_str()).ok() else {
                return ResolvedConfig {
                    config: SupervisorConfig::default(),
                    source: ConfigSource::Builtin,
                };
            };
            return ResolvedConfig {
                config: coerce_config(&parsed),
                source: ConfigSource::Global(global),
            };
        }
        return ResolvedConfig {
            config: SupervisorConfig::default(),
            source: ConfigSource::Builtin,
        };
    };
    ResolvedConfig {
        config: read_config_file(path),
        source: ConfigSource::Explicit(path.to_path_buf()),
    }
}

/// One-line label for the resolved model + config source, printed at
/// supervise startup — e.g.
/// `openrouter/deepseek/deepseek-v4-flash-0731 (global: /home/u/.config/pi-plan/supervisor.config.json)`,
/// `… (project: /repo/supervisor.config.json)`, `… (--config /path)`, or
/// `openrouter/deepseek/deepseek-v4-flash (built-in default)`. Renders
/// full resolved paths, and the model shown is the config-level model (or
/// `DEFAULT_MODEL` when unset) — the exact nibble that would have
/// diagnosed the rolling-alias drift.
pub fn config_source_label(resolved: &ResolvedConfig) -> String {
    let model = resolved
        .config
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    match &resolved.source {
        ConfigSource::Explicit(path) => {
            format!("{model} (--config {})", path.to_string_lossy().into_owned(),)
        }
        ConfigSource::Project(path) => {
            format!("{model} (project: {})", path.to_string_lossy().into_owned(),)
        }
        ConfigSource::Global(path) => {
            format!("{model} (global: {})", path.to_string_lossy().into_owned(),)
        }
        ConfigSource::Builtin => format!("{model} (built-in default)"),
    }
}

/// Resolve the worker persona file: `PI_PLAN_PERSONA` wins, else
/// `<cwd>/prompts/worker-persona.md` when it exists, else `None` (the
/// worker runs without a persona preamble).
pub fn resolve_persona_path(cwd: &Path, env_pi_plan_persona: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = env_pi_plan_persona
        && !p.is_empty()
    {
        return Some(Path::new(p).to_path_buf());
    }
    let candidate = cwd.join("prompts").join("worker-persona.md");
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// Environment override for the verifier persona file.
pub const VERIFIER_PERSONA_ENV: &str = "PI_PLAN_VERIFIER_PERSONA";

/// Resolve the verifier persona file: `PI_PLAN_VERIFIER_PERSONA` wins, else
/// `<cwd>/prompts/verifier-persona.md` when it exists, else `None` (the caller
/// uses the inline adversarial fallback). Same optional-resolution semantics as
/// [`resolve_persona_path`], but for the verifier's separate persona.
pub fn resolve_verifier_persona_path(
    cwd: &Path,
    env_pi_plan_verifier_persona: Option<&str>,
) -> Option<PathBuf> {
    if let Some(p) = env_pi_plan_verifier_persona
        && !p.is_empty()
    {
        return Some(Path::new(p).to_path_buf());
    }
    let candidate = cwd.join("prompts").join("verifier-persona.md");
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// Load the verifier persona, never failing: env override, else the repo
/// file, else — and on any read error — the inline adversarial fallback. The
/// rubric must exist even when the file was deleted or is unreadable, because
/// it is the only thing keeping the verifier adversarial.
pub fn load_verifier_persona(cwd: &Path, env_pi_plan_verifier_persona: Option<&str>) -> String {
    let fallback = || crate::prompt::VERIFIER_PERSONA_FALLBACK.to_string();
    match resolve_verifier_persona_path(cwd, env_pi_plan_verifier_persona) {
        Some(path) => fs::read_to_string(path).unwrap_or_else(|_| fallback()),
        None => fallback(),
    }
}

/// Resolve the `--skill` path for worker argv (Contract 3b pins the
/// implement-from-plan skill): `PI_PLAN_SKILL` wins, else
/// `<home>/.pi/agent/skills/implement-from-plan`, else `None` (omit the
/// flag).
pub fn resolve_skill_path(env_pi_plan_skill: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = env_pi_plan_skill
        && !p.is_empty()
    {
        return Some(Path::new(p).to_path_buf());
    }
    home.map(|h| {
        Path::new(h)
            .join(".pi")
            .join("agent")
            .join("skills")
            .join("implement-from-plan")
    })
    .filter(|p| p.exists())
}

/// Read and strip the implement-from-plan skill once at supervise startup.
/// Any failure — no skill configured, an unreadable file, or unresolvable
/// frontmatter — is a hard error so supervised runs fail fast before any
/// worker spawns. `status`/`stop`/`mark` never load the skill.
pub fn load_skill_body(skill_dir: Option<&Path>) -> Result<String, String> {
    let installation_hint =
        "set PI_PLAN_SKILL or install it to ~/.pi/agent/skills/implement-from-plan";
    let Some(dir) = skill_dir else {
        return Err(format!(
            "cannot read the implement-from-plan skill: not installed — {installation_hint}"
        ));
    };
    let skill_file = dir.join("SKILL.md");
    let skill_label = skill_file.to_string_lossy().into_owned();
    let Some(raw) = fs::read_to_string(skill_file).ok() else {
        return Err(format!(
            "cannot read the implement-from-plan skill: {} — {installation_hint}",
            skill_label,
        ));
    };
    match strip_skill_frontmatter(raw.as_str()) {
        Ok(body) => Ok(body),
        Err(_) => Err(format!(
            "cannot read the implement-from-plan skill: {} — {installation_hint}",
            skill_label,
        )),
    }
}

/// Resolve the clean-worktree skill path for the dirty-gate clean agent —
/// the same precedence shape as `resolve_skill_path`, but for the
/// clean-worktree skill, resolved **lazily** (only when the gate would
/// abort): `PI_PLAN_CLEAN_SKILL` wins, else
/// `<home>/.pi/agent/skills/clean-worktree` when it exists, else `None`
/// (the gate falls back to today's abort).
pub fn resolve_clean_skill_path(
    env_pi_plan_clean_skill: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(p) = env_pi_plan_clean_skill
        && !p.is_empty()
    {
        return Some(Path::new(p).to_path_buf());
    }
    home.map(|h| {
        Path::new(h)
            .join(".pi")
            .join("agent")
            .join("skills")
            .join("clean-worktree")
    })
    .filter(|p| p.exists())
}

/// Load the clean-worktree skill directory into the `(path, body)` pair the
/// clean spawn needs — the path feeds the agent's `--skill` argv entry, the
/// frontmatter-stripped body feeds its prompt. **Tolerant by design**: any
/// failure — an unreadable `SKILL.md` or unresolvable frontmatter — returns
/// `None`, so a missing or malformed clean-worktree skill can never fail a
/// run, unlike `load_skill_body` which is fail-fast for the hard-required
/// implement-from-plan skill.
pub fn load_clean_skill_body(skill_dir: &Path) -> Option<(PathBuf, String)> {
    let skill_file = skill_dir.join("SKILL.md");
    let raw = fs::read_to_string(skill_file).ok()?;
    let body = strip_skill_frontmatter(raw.as_str()).ok()?;
    Some((skill_dir.to_path_buf(), body))
}

/// The install layouts searched, in order, when the env override does not
/// resolve (D8). A false "missing" is as bad as a false "gated", and install
/// variants differ (scoped npm dir, unscoped npm dir, a subdirectory of pi's
/// global extensions directory), so every known layout is tried.
fn permission_extension_candidates(home: &Path) -> Vec<PathBuf> {
    let agent = home.join(".pi").join("agent");
    let npm_modules = agent.join("npm").join("node_modules");
    vec![
        npm_modules.join("@gotgenes").join("pi-permission-system"),
        npm_modules.join("pi-permission-system"),
        agent.join("extensions").join("pi-permission-system"),
    ]
}

/// Resolve the permission-system package directory for the worker's `-e`
/// flag (Contract 3b `--no-extensions` + `-e <dir>`). The candidates are
/// tried in order and the first one that **exists** wins (D6, D7, D8):
///
/// 1. `$PI_PLAN_PERMISSION_EXTENSION` (trimmed; not required to be under
///    `$HOME`) — existence-checked like every other candidate, so a typo is
///    reported instead of silently disabling gating,
/// 2. `<home>/.pi/agent/npm/node_modules/@gotgenes/pi-permission-system`,
/// 3. `<home>/.pi/agent/npm/node_modules/pi-permission-system`,
/// 4. `<home>/.pi/agent/extensions/pi-permission-system` (loadable through
///    `-e <dir>` only in a layout pi accepts — `index.ts`/`index.js`, or a
///    `package.json` with a `pi.extensions` field).
///
/// Paths are used **verbatim** — never absolutized, never canonicalized — so
/// a relative env value resolves against the supervisor's cwd, which is also
/// the worker's spawn cwd. A literal `builtin:<name>` is deliberately not
/// accepted: `exists()` rejects it, and workers never load a builtin.
///
/// Existence is the only test: a directory that exists but is not a
/// permission system still wins (pi then reports the load failure on the
/// worker's stderr while the worker runs ungated, and the resolved path is
/// printed, so the misconfiguration stays diagnosable).
///
/// Unlike the skills — optional, omitted from argv when absent — the
/// permission system is currently HARD-required: workers spawn bare
/// (`--no-extensions`) with it as the only loaded extension, so an
/// unresolvable extension is a hard error that fails the run before any
/// worker spawns (the same fail-fast shape as the skill prerequisite).
pub fn resolve_permission_extension(
    env_pi_plan_permission_extension: Option<&str>,
    home: Option<&str>,
) -> Result<PathBuf, String> {
    let env_override = env_pi_plan_permission_extension
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(value) = env_override {
        let candidate = Path::new(value);
        if candidate.exists() {
            return Ok(candidate.to_path_buf());
        }
    }
    let home = home.map(str::trim).filter(|h| !h.is_empty());
    let Some(h) = home else {
        let why = match env_override {
            Some(value) => format!(
                "PI_PLAN_PERMISSION_EXTENSION points at {value}, which does not \
exist, and HOME is unknown"
            ),
            None => "PI_PLAN_PERMISSION_EXTENSION is unset and HOME is unknown".to_string(),
        };
        return Err(format!(
            "cannot resolve the permission-system extension: {why} — set \
PI_PLAN_PERMISSION_EXTENSION to the pi-permission-system package directory"
        ));
    };
    let candidates = permission_extension_candidates(Path::new(h));
    if let Some(found) = candidates.iter().find(|candidate| candidate.exists()) {
        return Ok(found.clone());
    }
    // Every searched path is named: the operator has to be able to see where
    // the tool looked, not just that it gave up.
    let searched = candidates
        .iter()
        .map(|candidate| candidate.to_string_lossy().into_owned())
        .collect::<Vec<String>>()
        .join(", ");
    let why = match env_override {
        Some(value) => format!(
            "no package at {searched}; $PI_PLAN_PERMISSION_EXTENSION points at \
{value}, which does not exist"
        ),
        None => format!("no package at {searched}"),
    };
    Err(format!(
        "cannot resolve the permission-system extension: {why} — set \
PI_PLAN_PERMISSION_EXTENSION or install pi-permission-system under \
~/.pi/agent/npm/node_modules/@gotgenes"
    ))
}

// ---------------- keep/reset permissions prompt ----------------

/// One answer at the start-of-run keep/reset prompt (D6/D12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepResetAnswer {
    /// Keep the grants on file (the default).
    Keep,
    /// Clear every project grant.
    Reset,
}

/// The start-of-run keep/reset verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepResetVerdict {
    /// Proceed with the grants on file (keep / restart / EOF — D12).
    Keep,
    /// The operator typed `reset` — clear the store and proceed.
    Reset,
    /// The operator stopped at the prompt (`stop`, Ctrl-C, or ^D) —
    /// cancel the run start and exit 2.
    Stop,
}

/// One line-mode answer at the keep/reset prompt: exactly `reset`
/// (trimmed, case-insensitive) resets; everything else — `keep`,
/// `restart` (keep-and-proceed, D12), a blank line, or garbage — keeps.
pub fn parse_keep_reset_answer(input: &str) -> KeepResetAnswer {
    if input.trim().to_lowercase() == "reset" {
        KeepResetAnswer::Reset
    } else {
        KeepResetAnswer::Keep
    }
}

/// The keep/reset verdict decided purely from the pause outcome's
/// observable pieces (D12): `stopped` (`PauseOutcome::Stopped`, or a
/// line-mode `stop` — which surfaces as `NoAnswer` plus the stop flag)
/// cancels the run; `restarted` (a TUI `Restart` with no worker to
/// restart) keeps; a parsed `reset` answer resets; everything else —
/// `keep`, line-mode `restart`, EOF — keeps. The caller reads the flags
/// from the shared run control right after the pause returns.
pub fn keep_reset_verdict(
    answered: Option<&str>,
    stopped: bool,
    restarted: bool,
) -> KeepResetVerdict {
    if stopped {
        KeepResetVerdict::Stop
    } else if restarted {
        KeepResetVerdict::Keep
    } else if let Some(answer) = answered {
        if parse_keep_reset_answer(answer) == KeepResetAnswer::Reset {
            KeepResetVerdict::Reset
        } else {
            KeepResetVerdict::Keep
        }
    } else {
        KeepResetVerdict::Keep
    }
}

/// The question text the start-of-run keep/reset prompt renders (line
/// mode and the TUI modal both go through the `QuestionPause` seam,
/// D6/D12). The default is keep; `reset` clears the project grants;
/// `stop` cancels the run start (exit 2); `restart` is keep-and-proceed.
pub fn render_keep_reset_prompt(grant_count: usize) -> String {
    format!(
        "keep or reset the stored project permissions? ({grant_count} grant(s) on file)\n\
   keep — continue with the grants on file (default)\n\
   reset — clear every project grant\n\
   stop — cancel this run (exit 2)"
    )
}

// ---------------- status report ----------------

/// Row status label for the `status` report: human adjudication first (a
/// marked row is done even without a commit), then the verifier's provenance
/// (a verified row is done with no matching commit), then any cached verdict
/// for the row (re-presented, never re-spawned), and finally Contract 2's git
/// match tier.
fn row_status(row: &TodoRow, subjects: &[String], state: Option<&SupervisorState>) -> String {
    if let Some(state) = state {
        if state.adjudicated.contains(&row.number) {
            return "adjudicated done".to_string();
        }
        if state
            .verified
            .iter()
            .any(|verified| verified.row == row.number)
        {
            return "verified (no matching commit)".to_string();
        }
        if let Some(cached) = latest_cached_verdict(state, row.number) {
            return format!(
                "verified (cached: {}, HEAD {})",
                cached.verdict,
                short_hash(&cached.head)
            );
        }
    }
    match match_planned(&row.commit_message, subjects).tier {
        MatchTier::Exact | MatchTier::Similar => "done".to_string(),
        MatchTier::Candidate => "candidate (needs review)".to_string(),
        MatchTier::None => "pending".to_string(),
    }
}

/// The newest cached verdict for `row` (the cache is keyed by `(row, HEAD)`).
fn latest_cached_verdict(
    state: &SupervisorState,
    row: u64,
) -> Option<&crate::state::CachedVerdict> {
    state
        .verified_cache
        .iter()
        .filter(|cached| cached.row == row)
        .max_by_key(|cached| cached.at)
}

/// The row numbers the verifier skipped (provenance).
fn verified_numbers(state: Option<&SupervisorState>) -> Vec<u64> {
    state
        .map(|state| state.verified.iter().map(|verified| verified.row).collect())
        .unwrap_or_default()
}

/// A row is done when its commit matches, the human marked it, or the
/// verifier skipped it (the same predicate the supervisor loop runs).
fn row_is_done(row: &TodoRow, subjects: &[String], state: Option<&SupervisorState>) -> bool {
    let adjudicated: &[u64] = state
        .map(|state| state.adjudicated.as_slice())
        .unwrap_or(&[]);
    is_done(row, subjects, adjudicated, &verified_numbers(state))
}

/// The next action the operator can take, phrased with the same pure helpers
/// the unrecognised-row prompt uses (`[m]ark row N complete`, `[w]ork row N`,
/// `[r]eword …`, `[s]top`). There is no `pi-plan reword N` command in v1, so
/// a reword hint is the prompt action, never a command.
fn status_next_action(
    rows: &[TodoRow],
    subjects: &[String],
    state: Option<&SupervisorState>,
    holes: &[u64],
) -> Option<String> {
    if let Some(&row) = holes.first() {
        let actions = [
            crate::supervise::UnrecognisedDecision::MarkComplete,
            crate::supervise::UnrecognisedDecision::WorkRow,
            crate::supervise::UnrecognisedDecision::Reword,
            crate::supervise::UnrecognisedDecision::Stop,
        ]
        .iter()
        .map(|decision| action_hint_label(*decision, row, None))
        .collect::<Vec<_>>()
        .join(" · ");
        return Some(format!(
            "next: run pi-plan supervise — row {row}: {actions}"
        ));
    }
    if let Some(row) = rows
        .iter()
        .find(|row| match_planned(&row.commit_message, subjects).tier == MatchTier::Candidate)
    {
        return Some(format!(
            "next: pi-plan mark {} done (or run pi-plan supervise)",
            row.number
        ));
    }
    if rows.iter().any(|row| !row_is_done(row, subjects, state)) {
        return Some("next: run pi-plan supervise".to_string());
    }
    None
}

/// The numeric footer facts for the status report: worktree dirt and the
/// stored permission-grant count (D8). Bundled so `format_status_report`
/// stays within clippy's argument budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusFoot {
    pub dirty_lines: usize,
    pub grant_count: usize,
}

/// Assemble the `pi-plan status` report lines (pure — the caller does the
/// I/O, this only formats facts). `root_label` is the resolved external
/// run-state root (`~/.pi-plan/<key>`), printed in the `state:` lines so a
/// user grepping the project root is never misled by a cwd-relative name.
pub fn format_status_report(
    cwd_label: &str,
    root_label: &str,
    source: Option<&str>,
    rows: &[TodoRow],
    subjects: &[String],
    state: Option<&SupervisorState>,
    foot: StatusFoot,
) -> Vec<String> {
    let mut out: Vec<String> = vec![format!("pi-plan status — {cwd_label}")];
    if let Some(source) = source {
        out.push(format!("plan source: {source}"));
    }
    let done = rows
        .iter()
        .filter(|r| row_is_done(r, subjects, state))
        .count();

    let done_str = done.to_string();
    out.push(format!("rows: {done_str}/{} done", rows.len()));
    for row in rows.iter() {
        out.push(format!(
            "  row {} ({}): {}",
            row.id,
            row.commit_message,
            row_status(row, subjects, state)
        ));
    }
    let adjudicated: Vec<u64> = state
        .map(|state| state.adjudicated.clone())
        .unwrap_or_default();
    let verified = verified_numbers(state);
    let holes = detect_holes(rows, subjects, &adjudicated, &verified);
    if !holes.is_empty() {
        let listed = holes
            .iter()
            .map(|number| number.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        out.push(format!("holes: [{listed}]"));
    }
    if let Some(hint) = status_next_action(rows, subjects, state, &holes) {
        out.push(hint);
    }
    if let Some(state) = state {
        out.push(format!("state: {root_label}/{}", STATE_FILE_NAME));
        out.push(format!(
            "  current row {} · runs used {} · last outcome {}",
            state.current_row, state.runs_used, state.last_outcome
        ));
        out.push(format!(
            "  adjudicated: [{}]",
            join_numbers(&state.adjudicated)
        ));
        out.push(format!("  verified: [{}]", join_numbers(&verified)));
        let agent = state.agent_id.clone().unwrap_or_else(|| "—".to_string());
        let started = state
            .started_at
            .map(|t| format!(" · started {}", crate::ui::format_duration(t)))
            .unwrap_or_default();
        out.push(format!("  agent {agent}{started}"));
    } else {
        out.push("state: none (no state file (nothing running))".to_string());
    }
    if foot.grant_count > 0 {
        out.push(format!("permissions: {} grant(s) stored", foot.grant_count));
    } else {
        out.push("permissions: none stored".to_string());
    }
    if foot.dirty_lines > 0 {
        out.push(format!("worktree: DIRTY — {} change(s)", foot.dirty_lines));
    } else {
        out.push("worktree: clean".to_string());
    }
    out
}

/// Render row numbers as the `[1, 2]` list the status block prints.
fn join_numbers(numbers: &[u64]) -> String {
    numbers
        .iter()
        .map(|number| number.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse + readless helper binding `parse_plan` for callers that already
/// have the TODO text (kept here so binary wiring stays thin).
pub fn parse_todo(content: &str) -> TodoPlan {
    parse_plan(content)
}

// ---------------- final report ----------------

/// One-line label for a row outcome in the final report.
pub fn outcome_label(outcome: &RowOutcome) -> String {
    match outcome {
        RowOutcome::Done { .. } => "done — commit matched".to_string(),
        RowOutcome::QuestionPause { .. } => "paused with a question for you".to_string(),
        RowOutcome::NearMiss { .. } => banner_label(outcome),
        RowOutcome::BudgetChoice { .. } => "stopped — budget exhausted".to_string(),
        RowOutcome::SpawnError { .. } => "stopped — worker spawn failed".to_string(),
        RowOutcome::DirtyWorktree { .. } => "paused — working tree not clean".to_string(),
        RowOutcome::CleanQuestionPause { .. } => {
            "paused — clean-worktree agent asks a question".to_string()
        }
        RowOutcome::CleanAnswerOrphaned { .. } => {
            "paused — clean answer cannot be consumed".to_string()
        }
        RowOutcome::Stopped { .. } => "stopped by user".to_string(),
        RowOutcome::UnrecognisedRow { action, .. } => match action {
            Some(action) => format!("{} — operator chose {action}", banner_label(outcome)),
            None => banner_label(outcome),
        },
        RowOutcome::Verified { .. } => banner_label(outcome),
    }
}

/// The supervisor-authored detail lines the final report prints beneath an
/// outcome: the evidence and verifier facts behind a `Verified` skip, and the
/// fenced verifier explanation behind an escalation.
fn outcome_detail_lines(outcome: &RowOutcome) -> Vec<String> {
    match outcome {
        RowOutcome::Verified { bundle, report, .. } => {
            let mut out = Vec::new();
            let evidence = bundle_evidence(bundle);
            if !evidence.is_empty() {
                out.push(format!("    evidence: {}", evidence.join("; ")));
            }
            out.push(format!("    verifier: {}", verifier_facts(report)));
            out
        }
        RowOutcome::UnrecognisedRow {
            evidence, verifier, ..
        } => {
            let mut out = Vec::new();
            if let Some(bundle) = evidence {
                let present = bundle_evidence(bundle);
                if !present.is_empty() {
                    out.push(format!("    evidence: {}", present.join("; ")));
                }
            }
            if let Some(report) = verifier {
                out.push(format!("    verifier: {}", verifier_facts(report)));
                if let Some(explanation) = report.fields.explanation.as_ref()
                    && !explanation.trim().is_empty()
                {
                    out.push("    verifier explanation (unverified):".to_string());
                    for line in explanation.trim().lines() {
                        out.push(format!("      {line}"));
                    }
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// The verifier's run facts as the final report prints them. Only facts the
/// report actually carries appear: a verifier that could not run shows its
/// `NotRun` verdict and nothing invented.
fn verifier_facts(report: &VerifierReport) -> String {
    let mut parts = vec![verdict_upper(&report.verdict).to_string()];
    if !report.agent_id.is_empty() {
        parts.push(report.agent_id.clone());
    }
    parts.push(format!("HEAD {}", short_hash(&report.head)));
    parts.push(format!("{} turn(s)", report.turns));
    if let Some(cost) = report.cost {
        parts.push(format_cost(Some(cost)));
    }
    if let Some(tokens) = report.tokens {
        parts.push(format!("{} tokens", format_tokens(tokens)));
    }
    if let Some(path) = &report.transcript_path {
        parts.push(format!("transcript {}", path.display()));
    }
    parts.join(" · ")
}

/// The existing `describe_outcome` label is the loop's banner one-liner; the
/// final report reuses it so both surfaces agree.
pub fn banner_label(outcome: &RowOutcome) -> String {
    crate::supervise::describe_outcome(outcome)
}

/// Row number of an outcome (all variants carry their row).
fn outcome_row_number(outcome: &RowOutcome) -> u64 {
    match outcome {
        RowOutcome::Done { row, .. }
        | RowOutcome::QuestionPause { row, .. }
        | RowOutcome::NearMiss { row, .. }
        | RowOutcome::BudgetChoice { row, .. }
        | RowOutcome::SpawnError { row, .. }
        | RowOutcome::DirtyWorktree { row, .. }
        | RowOutcome::CleanQuestionPause { row, .. }
        | RowOutcome::CleanAnswerOrphaned { row, .. }
        | RowOutcome::UnrecognisedRow { row, .. }
        | RowOutcome::Verified { row, .. }
        | RowOutcome::Stopped { row, .. } => row.number,
    }
}

/// The per-attempt records of an outcome (every variant carries `records`).
fn outcome_records(outcome: &RowOutcome) -> Vec<&RunRecord> {
    match outcome {
        RowOutcome::Done { records, .. }
        | RowOutcome::QuestionPause { records, .. }
        | RowOutcome::NearMiss { records, .. }
        | RowOutcome::BudgetChoice { records, .. }
        | RowOutcome::SpawnError { records, .. }
        | RowOutcome::DirtyWorktree { records, .. }
        | RowOutcome::CleanQuestionPause { records, .. }
        | RowOutcome::CleanAnswerOrphaned { records, .. }
        | RowOutcome::UnrecognisedRow { records, .. }
        | RowOutcome::Stopped { records, .. } => records.iter().collect::<Vec<&RunRecord>>(),
        RowOutcome::Verified { .. } => Vec::new(),
    }
}

/// Assemble the `--pi-plan report` block (stderr): one line per row outcome,
/// per-attempt details (outcome kind, question, result tail, transcript
/// path), and a done-count summary.
/// One attempt's stats line for the final report (step 5): worker id,
/// cost, tokens, context %, turns, and wall-clock run time — every value
/// already lives on the terminal snapshot (its `started_at` is kept fresh
/// by the stats poll cadence).
fn format_run_stats_line(record: &RunRecord, snap: &WorkerSnapshot) -> String {
    let now = now_epoch_ms().unwrap_or(snap.started_at);
    let ctx = match snap.context_percent {
        Some(p) => {
            let pct = p as u64;
            format!("{pct}%")
        }
        None => "?".to_string(),
    };
    format!(
        "worker: {} · cost {} · {} tokens · ctx {} · {} turns · {}",
        record.agent_id,
        format_cost(snap.cost),
        format_tokens(snap.tokens.map(|t| t.total).unwrap_or(0)),
        ctx,
        snap.turn_count,
        format_duration(now.saturating_sub(snap.started_at))
    )
}

pub fn format_final_report(outcomes: &[RowOutcome], plan_rows: usize) -> Vec<String> {
    let mut out: Vec<String> = vec!["── pi-plan report ──".to_string()];
    let mut done: usize = 0;
    for outcome in outcomes.iter() {
        let number = outcome_row_number(outcome);
        if matches!(*outcome, RowOutcome::Done { .. }) {
            done += 1;
        }
        out.push(format!("  row {number}: {}", outcome_label(outcome)));
        out.extend(outcome_detail_lines(outcome));
        for record in outcome_records(outcome) {
            out.push(format!(
                "    run {}: {}",
                record.attempt,
                run_outcome_label(record.outcome)
            ));
            if let Some(snap) = record.snapshot.as_ref() {
                out.push(format!("      {}", format_run_stats_line(record, snap)));
            }
            if let Some(question) = &record.question
                && !question.is_empty()
            {
                out.push(format!("      question: {question}"));
            }
            if let Some(tail) = &record.tail {
                for line in tail.lines() {
                    out.push(format!("      {line}"));
                }
            }
            if let Some(path) = record.transcript_path.as_ref() {
                out.push(format!(
                    "      transcript: {}",
                    path.to_string_lossy().into_owned()
                ));
            }
        }
    }
    let done_str = done.to_string();
    out.push(format!("done: {done_str}/{} rows", plan_rows));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::git::MatchResult;
    use crate::hole::{Deliverable, EvidenceBundle, SubjectDiff, Verdict, VerdictFields};
    use crate::state::{CachedVerdict, VerifiedRow};
    use crate::supervise::RunOutcomeKind;
    use crate::ui::ask_lines;
    use crate::worker::Tokens;

    static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_cwd() -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("pi-plan-cli-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn cfg(v: serde_json::Value) -> SupervisorConfig {
        coerce_config(&v)
    }

    const TWO_ROWS: &str = "## Steps\n\
| # | Commit message | Logical unit | Key deliverables | Tests |\n\
| --- | --- | --- | --- | --- |\n\
| 1 | `feat: a` | u | d | t |\n\
| 2 | `feat: implement reader` | u | d | t |\n";

    // ---- clap surface ----

    #[test]
    fn parse_supervise_flags_and_config() {
        let cli = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "supervise".to_string(),
            "--row".to_string(),
            "3".to_string(),
            "--answer".to_string(),
            "use tag v2".to_string(),
            "--config".to_string(),
            "/tmp/supervisor.config.json".to_string(),
            "--theme".to_string(),
            "/tmp/my-theme.json".to_string(),
        ])
        .expect("parse supervise");
        match cli.command {
            Command::Supervise {
                row,
                answer,
                config,
                theme,
            } => {
                assert_eq!(row, Some(3));
                assert_eq!(answer, Some("use tag v2".to_string()));
                assert_eq!(
                    config.map(|p| p.to_string_lossy().into_owned()),
                    Some("/tmp/supervisor.config.json".to_string())
                );
                assert_eq!(
                    theme.map(|p| p.to_string_lossy().into_owned()),
                    Some("/tmp/my-theme.json".to_string())
                );
            }
            other => panic!("expected Supervise, got {other:?}"),
        }
    }

    #[test]
    fn supervise_parses_without_a_theme_flag() {
        let cli = Cli::try_parse_from(vec!["pi-plan".to_string(), "supervise".to_string()])
            .expect("parse supervise");
        match cli.command {
            Command::Supervise { theme, .. } => assert_eq!(theme, None),
            other => panic!("expected Supervise, got {other:?}"),
        }
    }

    #[test]
    fn parse_step_requires_a_row_number_and_accepts_theme() {
        let cli = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "step".to_string(),
            "2".to_string(),
            "--theme".to_string(),
            "light.json".to_string(),
        ])
        .expect("parse step");
        match cli.command {
            Command::Step { row, theme, .. } => {
                assert_eq!(row, 2);
                assert_eq!(
                    theme.map(|p| p.to_string_lossy().into_owned()),
                    Some("light.json".to_string())
                );
            }
            other => panic!("expected Step, got {other:?}"),
        }
    }

    #[test]
    fn parse_mark_requires_the_done_word() {
        let cli = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "mark".to_string(),
            "4".to_string(),
            "done".to_string(),
        ])
        .expect("parse mark");
        match cli.command {
            Command::Mark { row, done } => {
                assert_eq!(row, 4);
                assert_eq!(done, MarkWord::Done);
            }
            other => panic!("expected Mark, got {other:?}"),
        }
    }

    #[test]
    fn mark_rejects_any_other_written_argument() {
        let result = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "mark".to_string(),
            "4".to_string(),
            "bogus".to_string(),
        ]);
        assert!(
            result.is_err(),
            "an invalid mark word must fail at clap parse time (exit 2)"
        );
    }

    #[test]
    fn bare_status_and_stop_parse_without_arguments() {
        let cli = Cli::try_parse_from(vec!["pi-plan".to_string(), "status".to_string()])
            .expect("parse status");
        assert!(matches!(cli.command, Command::Status));
        let cli = Cli::try_parse_from(vec!["pi-plan".to_string(), "stop".to_string()])
            .expect("parse stop");
        assert!(matches!(cli.command, Command::Stop));
    }

    // ---- stop control file ----

    #[test]
    fn stop_request_write_present_and_clear() {
        let cwd = temp_cwd();
        assert!(!stop_request_present(&cwd));
        write_stop_request(&cwd);
        assert!(stop_request_present(&cwd));
        clear_stop_request(&cwd);
        assert!(!stop_request_present(&cwd));
    }

    // ---- mark done ----

    #[test]
    fn mark_done_creates_state_and_merges_idempotently() {
        let root = temp_cwd();
        fs::write(root.join("TODO.md"), TWO_ROWS).expect("write todo");
        let todo = parse_plan(TWO_ROWS);

        // First mark creates a state file tied to the current plan hash.
        mark_done(&root, &root, &todo, 2).expect("mark row 2");
        let state = read_state_file(&root).expect("state file exists");
        assert_eq!(state.adjudicated, vec![2]);
        assert_eq!(state.plan_hash, plan_hash_of(TWO_ROWS));

        // A second, earlier row sorts in; repeating row 2 is idempotent.
        mark_done(&root, &root, &todo, 1).expect("mark row 1");
        mark_done(&root, &root, &todo, 2).expect("mark row 2 again");
        let state = read_state_file(&root).expect("state file exists");
        assert_eq!(state.adjudicated, vec![1, 2]);
    }

    #[test]
    fn mark_done_creates_the_storage_root_directory() {
        let cwd = temp_cwd();
        fs::write(cwd.join("TODO.md"), TWO_ROWS).expect("write todo");
        let todo = parse_plan(TWO_ROWS);
        let root = cwd.join("fresh-root");
        assert!(!root.exists(), "root starts missing");
        mark_done(&cwd, &root, &todo, 1).expect("mark row 1");
        assert!(root.exists(), "mark creates the storage root");
        assert!(
            root.join("supervisor-state.json").exists(),
            "state file lives under the created root"
        );
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn mark_done_rejects_unknown_rows() {
        let root = temp_cwd();
        let todo = parse_plan(TWO_ROWS);
        assert!(mark_done(&root, &root, &todo, 9).is_err());
    }

    // ---- status report ----

    fn two_row_plan() -> TodoPlan {
        parse_plan(TWO_ROWS)
    }

    #[test]
    fn status_report_shows_git_tiers_and_state() {
        let todo = two_row_plan();
        let state = Some(SupervisorState {
            plan_hash: "abc".to_string(),
            current_row: 2,
            runs_used: 1,
            last_outcome: "running".to_string(),
            adjudicated: vec![],
            verified: Vec::new(),
            verified_cache: Vec::new(),
            agent_id: Some("7".to_string()),
            started_at: Some(1_000_000),
        });
        // Row 1's commit exists exactly in git; row 2 has no match yet.
        let subjects = vec!["feat: a".to_string()];
        let lines = format_status_report(
            "/repo",
            "/home/u/.pi-plan/my-repo-a1b2c3d4",
            Some("docs/research/plan.md".to_string().as_str()),
            &todo.rows[..],
            &subjects,
            state.as_ref(),
            StatusFoot {
                dirty_lines: 0,
                grant_count: 3,
            },
        );
        assert!(lines.contains(&"rows: 1/2 done".to_string()));
        assert!(lines.contains(&"  row 1 (feat: a): done".to_string()));
        assert!(lines.contains(&"  row 2 (feat: implement reader): pending".to_string()));
        assert!(
            lines.contains(&"  current row 2 · runs used 1 · last outcome running".to_string())
        );
        assert!(lines.contains(
            &"state: /home/u/.pi-plan/my-repo-a1b2c3d4/supervisor-state.json".to_string()
        ));
        assert!(lines.contains(&"worktree: clean".to_string()));
        assert!(lines.contains(&"permissions: 3 grant(s) stored".to_string()));
    }

    #[test]
    fn status_report_lists_worktree_dirt_and_marked_rows() {
        let todo = two_row_plan();
        let state = Some(SupervisorState {
            plan_hash: "abc".to_string(),
            current_row: 1,
            runs_used: 0,
            last_outcome: "adjudicated".to_string(),
            adjudicated: vec![1],
            verified: Vec::new(),
            verified_cache: Vec::new(),
            agent_id: None,
            started_at: None,
        });
        let lines = format_status_report(
            "/repo",
            "/home/u/.pi-plan/my-repo-a1b2c3d4",
            None,
            &todo.rows[..],
            &[],
            state.as_ref(),
            StatusFoot {
                dirty_lines: 2,
                grant_count: 0,
            },
        );
        assert!(lines.contains(&"  row 1 (feat: a): adjudicated done".to_string()));
        assert!(lines.contains(&"worktree: DIRTY — 2 change(s)".to_string()));
        assert!(lines.contains(
            &"state: /home/u/.pi-plan/my-repo-a1b2c3d4/supervisor-state.json".to_string()
        ));
        assert!(lines.contains(&"permissions: none stored".to_string()));
    }

    #[test]
    fn status_report_without_state_file_says_none() {
        let todo = two_row_plan();
        let lines = format_status_report(
            "/repo",
            "/home/u/.pi-plan/my-repo-a1b2c3d4",
            None,
            &todo.rows[..],
            &[],
            None,
            StatusFoot {
                dirty_lines: 0,
                grant_count: 0,
            },
        );
        assert!(lines.contains(&"state: none (no state file (nothing running))".to_string()));
    }

    // ---- reset-permissions ----

    #[test]
    fn reset_permissions_parses_without_yes_and_with_flag() {
        let cli = Cli::try_parse_from(vec!["pi-plan".to_string(), "reset-permissions".to_string()])
            .expect("parse reset-permissions");
        match cli.command {
            Command::ResetPermissions { yes } => assert!(!yes, "no --yes → ask for confirmation"),
            other => panic!("expected ResetPermissions, got {other:?}"),
        }
        let cli = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "reset-permissions".to_string(),
            "--yes".to_string(),
        ])
        .expect("parse reset-permissions --yes");
        match cli.command {
            Command::ResetPermissions { yes } => assert!(yes, "--yes skips the confirmation"),
            other => panic!("expected ResetPermissions, got {other:?}"),
        }
    }

    #[test]
    fn reset_permissions_rejects_extra_arguments() {
        let result = Cli::try_parse_from(vec![
            "pi-plan".to_string(),
            "reset-permissions".to_string(),
            "bogus".to_string(),
        ]);
        assert!(
            result.is_err(),
            "a stray positional must fail at clap parse time (exit 2), not at runtime"
        );
    }

    #[test]
    fn status_report_counts_stored_grants() {
        let todo = two_row_plan();
        let lines = format_status_report(
            "/repo",
            "/home/u/.pi-plan/my-repo-a1b2c3d4",
            None,
            &todo.rows[..],
            &[],
            None,
            StatusFoot {
                dirty_lines: 0,
                grant_count: 2,
            },
        );
        assert!(lines.contains(&"permissions: 2 grant(s) stored".to_string()));
        let lines = format_status_report(
            "/repo",
            "/home/u/.pi-plan/my-repo-a1b2c3d4",
            None,
            &todo.rows[..],
            &[],
            None,
            StatusFoot {
                dirty_lines: 0,
                grant_count: 0,
            },
        );
        assert!(lines.contains(&"permissions: none stored".to_string()));
    }

    const THREE_ROWS: &str = "## Steps\n\
| # | Commit message | Logical unit | Key deliverables | Tests |\n\
| --- | --- | --- | --- | --- |\n\
| 1 | `feat: a` | u | d | t |\n\
| 2 | `feat: b` | u | d | t |\n\
| 3 | `feat: c` | u | d | t |\n";

    fn three_row_subjects() -> Vec<String> {
        vec!["feat: a".to_string(), "feat: c".to_string()]
    }

    fn hole_state() -> SupervisorState {
        SupervisorState {
            plan_hash: "abc".to_string(),
            current_row: 2,
            runs_used: 0,
            last_outcome: "hole-escalated".to_string(),
            adjudicated: Vec::new(),
            verified: Vec::new(),
            verified_cache: Vec::new(),
            agent_id: None,
            started_at: None,
        }
    }

    #[test]
    fn status_report_marks_a_hole_its_cached_verdict_and_the_next_action() {
        let todo = parse_plan(THREE_ROWS);
        let mut state = hole_state();
        state.verified_cache.push(CachedVerdict {
            row: 2,
            head: "6bf2c8e1234".to_string(),
            verdict: "incomplete".to_string(),
            at: 1,
        });
        let lines = format_status_report(
            "/repo",
            "/state",
            None,
            &todo.rows[..],
            &three_row_subjects(),
            Some(&state),
            StatusFoot {
                dirty_lines: 0,
                grant_count: 0,
            },
        );
        assert!(lines.contains(
            &"  row 2 (feat: b): verified (cached: incomplete, HEAD 6bf2c8e)".to_string()
        ));
        assert!(lines.contains(&"holes: [2]".to_string()));
        assert!(lines.contains(&"  verified: []".to_string()));
        assert!(
            lines.contains(&format!(
                "next: run pi-plan supervise — row 2: {}",
                "[m]ark row 2 complete · [w]ork row 2 · \
                 [r]eword the closest commit to the planned subject and continue · [s]top"
            )),
            "the hint is derived from the prompt helpers: {lines:?}"
        );
    }

    #[test]
    fn status_report_marks_a_verified_row_as_done_without_a_commit() {
        let todo = parse_plan(THREE_ROWS);
        let mut state = hole_state();
        state.verified.push(VerifiedRow {
            row: 2,
            head: "6bf2c8e1234".to_string(),
            verdict: "complete".to_string(),
            commit: None,
            at: 1,
            evidence: vec!["tests/leakage.rs".to_string()],
        });
        let lines = format_status_report(
            "/repo",
            "/state",
            None,
            &todo.rows[..],
            &three_row_subjects(),
            Some(&state),
            StatusFoot {
                dirty_lines: 0,
                grant_count: 0,
            },
        );
        assert!(lines.contains(&"rows: 3/3 done".to_string()));
        assert!(lines.contains(&"  row 2 (feat: b): verified (no matching commit)".to_string()));
        assert!(lines.contains(&"  verified: [2]".to_string()));
        assert!(
            !lines.iter().any(|line| line.starts_with("holes:")),
            "a verified row is not a hole: {lines:?}"
        );
    }

    // ---- config layering: global (XDG) ----

    #[test]
    fn global_config_path_prefers_xdg_over_home_config_over_none() {
        // XDG wins when set and non-empty.
        assert_eq!(
            global_config_path(
                Some("/xdg".to_string().as_str()),
                Some("/home/u".to_string().as_str())
            ),
            Some(
                Path::new("/xdg")
                    .join("pi-plan")
                    .join("supervisor.config.json")
            )
        );
        // An empty-string XDG falls back to ~/.config.
        assert_eq!(
            global_config_path(
                Some("".to_string().as_str()),
                Some("/home/u".to_string().as_str())
            ),
            Some(
                Path::new("/home/u")
                    .join(".config")
                    .join("pi-plan")
                    .join("supervisor.config.json")
            )
        );
        // No XDG → ~/.config.
        assert_eq!(
            global_config_path(None, Some("/home/u".to_string().as_str())),
            Some(
                Path::new("/home/u")
                    .join(".config")
                    .join("pi-plan")
                    .join("supervisor.config.json")
            )
        );
        // Neither XDG nor HOME → None (no global config possible).
        assert_eq!(global_config_path(None, None), None);
    }

    #[test]
    fn resolve_config_explicit_beats_project_beats_global_beats_builtin() {
        let cwd = temp_cwd();
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        // No files anywhere → built-in.
        let r = resolve_config(&cwd, None, None, None);
        assert!(matches!(r.source, ConfigSource::Builtin));
        assert_eq!(r.config, SupervisorConfig::default());

        // A global file exists under the fake home → Global wins.
        let global_dir = home.join(".config").join("pi-plan");
        fs::create_dir_all(&global_dir).expect("global config dir");
        fs::write(
            global_dir.join("supervisor.config.json"),
            r#"{"model": "global-model"}"#,
        )
        .expect("write global config");
        let r = resolve_config(&cwd, None, None, Some(home_str.as_str()));
        assert!(
            matches!(r.source, ConfigSource::Global { .. }),
            "the XDG global file wins when no project file exists"
        );
        assert_eq!(r.config.model.as_deref(), Some("global-model"));

        // A project file shadows the global file (XDG set to empty string).
        fs::write(
            cwd.join("supervisor.config.json"),
            r#"{"model": "project-model"}"#,
        )
        .expect("write project config");
        let r = resolve_config(
            &cwd,
            None,
            Some("".to_string().as_str()),
            Some(home_str.as_str()),
        );
        assert!(matches!(r.source, ConfigSource::Project { .. }));
        assert_eq!(r.config.model.as_deref(), Some("project-model"));

        // `--config` beats both files.
        let explicit = temp_cwd().join("explicit.json");
        fs::write(&explicit, r#"{"model": "explicit-model"}"#).expect("write explicit config");
        let r = resolve_config(
            &cwd,
            Some(explicit.as_path()),
            None,
            Some(home_str.as_str()),
        );
        assert!(matches!(r.source, ConfigSource::Explicit { .. }));
        assert_eq!(r.config.model.as_deref(), Some("explicit-model"));
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_config_project_presence_shadows_global_even_when_corrupt() {
        let cwd = temp_cwd();
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        fs::create_dir_all(home.join(".config").join("pi-plan")).expect("global config dir");
        fs::write(
            home.join(".config")
                .join("pi-plan")
                .join("supervisor.config.json"),
            r#"{"model": "global-model"}"#,
        )
        .expect("write global config");
        // A present-but-corrupt project file still shadows the global tier.
        fs::write(cwd.join("supervisor.config.json"), "{ nope").expect("write corrupt project");
        let r = resolve_config(&cwd, None, None, Some(home_str.as_str()));
        assert!(matches!(r.source, ConfigSource::Project { .. }));
        assert_eq!(
            r.config,
            SupervisorConfig::default(),
            "corrupt file → defaults"
        );
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_config_corrupt_global_falls_back_to_builtin() {
        let cwd = temp_cwd();
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        fs::create_dir_all(home.join(".config").join("pi-plan")).expect("global config dir");
        fs::write(
            home.join(".config")
                .join("pi-plan")
                .join("supervisor.config.json"),
            "{ nope",
        )
        .expect("write corrupt global");
        let r = resolve_config(&cwd, None, None, Some(home_str.as_str()));
        assert!(matches!(r.source, ConfigSource::Builtin));
        assert_eq!(r.config, SupervisorConfig::default());
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_config_missing_home_and_xdg_falls_back_to_builtin() {
        let cwd = temp_cwd();
        let r = resolve_config(&cwd, None, None, None);
        assert!(matches!(r.source, ConfigSource::Builtin));
        assert_eq!(r.config, SupervisorConfig::default());
        let _ = fs::remove_dir_all(&cwd);
    }

    // ---- global config auto-create ----

    #[test]
    fn ensure_global_config_creates_the_file_and_parent_when_absent() {
        let home = temp_cwd();
        let global = home
            .join(".config")
            .join("pi-plan")
            .join("supervisor.config.json");
        assert!(!global.exists());
        ensure_global_config(global.as_path()).expect("scaffold created");
        assert!(global.exists(), "the global config file was created");
        let content = fs::read_to_string(&global).expect("read scaffold");
        assert_eq!(content, default_global_config_json());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn ensure_global_config_leaves_an_existing_file_untouched() {
        let home = temp_cwd();
        let global = home
            .join(".config")
            .join("pi-plan")
            .join("supervisor.config.json");
        fs::create_dir_all(global.parent().expect("parent")).expect("create parent");
        fs::write(&global, "{\"maxTurns\": 12}").expect("write user edit");
        ensure_global_config(global.as_path()).expect("existing file kept");
        let content = fs::read_to_string(&global).expect("read back");
        assert_eq!(
            content, "{\"maxTurns\": 12}",
            "user edit is not overwritten"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn ensure_global_config_tolerates_a_write_failure() {
        // A FILE where the parent directory belongs makes mkdir -p fail;
        // the error propagates as Err but the run never fails.
        let home = temp_cwd();
        fs::write(home.join(".config"), "not a dir").expect("write blocker file");
        let global = home
            .join(".config")
            .join("pi-plan")
            .join("supervisor.config.json");
        assert!(ensure_global_config(global.as_path()).is_err());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_config_auto_creates_the_scaffold_in_the_global_branch() {
        let cwd = temp_cwd();
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        let r = resolve_config(&cwd, None, None, Some(home_str.as_str()));
        assert!(matches!(r.source, ConfigSource::Global { .. }));
        assert_eq!(
            r.config.model.as_deref(),
            Some(crate::config::DEFAULT_GLOBAL_MODEL)
        );
        let global = home
            .join(".config")
            .join("pi-plan")
            .join("supervisor.config.json");
        assert!(global.exists(), "the scaffold was created on first resolve");
        let content = fs::read_to_string(&global).expect("read scaffold");
        assert_eq!(content, default_global_config_json());
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_config_write_failure_falls_back_to_builtin() {
        let cwd = temp_cwd();
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        // A FILE where the parent directory belongs makes the auto-create
        // fail; the run still resolves to built-in defaults.
        fs::write(home.join(".config"), "not a dir").expect("write blocker file");
        let r = resolve_config(&cwd, None, None, Some(home_str.as_str()));
        assert!(matches!(r.source, ConfigSource::Builtin));
        assert_eq!(r.config, SupervisorConfig::default());
        let _ = fs::remove_dir_all(&cwd);
        let _ = fs::remove_dir_all(&home);
    }

    // ---- source label ----

    #[test]
    fn config_source_label_renders_all_four_sources() {
        let project = Path::new("/repo").join("supervisor.config.json");
        let global = Path::new("/home/u")
            .join(".config")
            .join("pi-plan")
            .join("supervisor.config.json");
        let explicit = Path::new("/etc/pi-plan.json").to_path_buf();
        let c = cfg(serde_json::json!({ "model": "m" }));
        assert_eq!(
            config_source_label(&ResolvedConfig {
                config: c.clone(),
                source: ConfigSource::Explicit(explicit),
            }),
            "m (--config /etc/pi-plan.json)"
        );
        assert_eq!(
            config_source_label(&ResolvedConfig {
                config: c.clone(),
                source: ConfigSource::Project(project),
            }),
            "m (project: /repo/supervisor.config.json)"
        );
        assert_eq!(
            config_source_label(&ResolvedConfig {
                config: c.clone(),
                source: ConfigSource::Global(global),
            }),
            "m (global: /home/u/.config/pi-plan/supervisor.config.json)"
        );
        assert_eq!(
            config_source_label(&ResolvedConfig {
                config: SupervisorConfig::default(),
                source: ConfigSource::Builtin,
            }),
            format!("{} (built-in default)", crate::config::DEFAULT_MODEL)
        );
    }

    // ---- persona / skill resolution ----

    #[test]
    fn persona_resolution_prefers_env_over_cwd_file() {
        let cwd = temp_cwd();
        fs::create_dir_all(cwd.join("prompts")).expect("create prompts dir");
        fs::write(
            cwd.join("prompts").join("worker-persona.md"),
            "you are a worker",
        )
        .expect("write persona");
        assert_eq!(
            resolve_persona_path(&cwd, Some("/env/persona.md".to_string().as_str())),
            Some(Path::new("/env/persona.md").to_path_buf())
        );
        let from_cwd = resolve_persona_path(&cwd, None);
        assert_eq!(
            from_cwd.map(|p| p.to_string_lossy().into_owned()),
            Some(
                cwd.join("prompts")
                    .join("worker-persona.md")
                    .to_string_lossy()
                    .into_owned()
            )
        );
    }

    #[test]
    fn persona_resolution_falls_back_to_none() {
        let cwd = temp_cwd();
        assert_eq!(resolve_persona_path(&cwd, None), None);
    }

    #[test]
    fn verifier_persona_prefers_env_then_repo_file_then_inline_fallback() {
        let cwd = temp_cwd();
        // No file and no env → the inline adversarial fallback, never empty.
        let fallback = load_verifier_persona(&cwd, None);
        assert_eq!(fallback, crate::prompt::VERIFIER_PERSONA_FALLBACK);
        assert!(fallback.contains("adversarial verifier"));

        // The repo file wins over the fallback.
        fs::create_dir_all(cwd.join("prompts")).expect("create prompts dir");
        fs::write(
            cwd.join("prompts").join("verifier-persona.md"),
            "you are the repo verifier",
        )
        .expect("write verifier persona");
        assert_eq!(
            load_verifier_persona(&cwd, None),
            "you are the repo verifier"
        );
        assert_eq!(
            resolve_verifier_persona_path(&cwd, None),
            Some(cwd.join("prompts").join("verifier-persona.md"))
        );

        // The env override wins over the repo file — even when it is empty
        // or unreadable, which still yields the fallback rubric.
        assert_eq!(
            resolve_verifier_persona_path(&cwd, Some("/env/verifier.md")),
            Some(Path::new("/env/verifier.md").to_path_buf())
        );
        assert_eq!(
            load_verifier_persona(&cwd, Some("/env/missing-verifier.md")),
            crate::prompt::VERIFIER_PERSONA_FALLBACK
        );
        // A directory path is unreadable → fallback, never a panic.
        assert_eq!(
            load_verifier_persona(&cwd, Some(cwd.to_string_lossy().as_ref())),
            crate::prompt::VERIFIER_PERSONA_FALLBACK
        );
    }

    #[test]
    fn verifier_persona_env_name_is_stable() {
        assert_eq!(VERIFIER_PERSONA_ENV, "PI_PLAN_VERIFIER_PERSONA");
    }

    #[test]
    fn skill_resolution_defaults_from_home_and_env_wins() {
        // No skill installed in the fake home → None.
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        assert_eq!(resolve_skill_path(None, Some(home_str.as_str())), None);
        // Env override always wins, even over a real home install.
        assert_eq!(
            resolve_skill_path(
                Some("/skills/mine".to_string().as_str()),
                Some(home_str.as_str())
            ),
            Some(Path::new("/skills/mine").to_path_buf())
        );
        // An installed skill under the fake home resolves.
        let installed = home
            .join(".pi")
            .join("agent")
            .join("skills")
            .join("implement-from-plan");
        fs::create_dir_all(&installed).expect("create skill dir");
        fs::write(installed.join("SKILL.md"), "# skill").expect("write skill");
        assert_eq!(
            resolve_skill_path(None, Some(home_str.as_str())),
            Some(installed)
        );
    }

    #[test]
    fn load_skill_body_fails_fast_when_no_skill_is_configured() {
        match load_skill_body(None) {
            Ok(_) => panic!("expected a hard error for a missing skill"),
            Err(msg) => assert!(msg.contains("cannot read the implement-from-plan skill")),
        }
    }

    #[test]
    fn load_skill_body_fails_fast_on_unreadable_skill_paths() {
        // A directory that does not exist.
        let missing = temp_cwd().join("nope");
        match load_skill_body(Some(missing.as_path())) {
            Ok(_) => panic!("expected a hard error for a missing dir"),
            Err(msg) => assert!(msg.contains("cannot read the implement-from-plan skill")),
        }
        // A directory without a SKILL.md.
        let empty = temp_cwd();
        match load_skill_body(Some(empty.as_path())) {
            Ok(_) => panic!("expected a hard error for a missing SKILL.md"),
            Err(msg) => assert!(msg.contains("cannot read the implement-from-plan skill")),
        }
    }

    #[test]
    fn load_skill_body_strips_frontmatter_from_a_valid_skill_file() {
        let dir = temp_cwd();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: implement-from-plan\ndescription: x\n---\n\n# Implement from Plan\n\n## Purpose\n",
        )
        .expect("write skill");
        let body = load_skill_body(Some(dir.as_path())).expect("skill body loads");
        assert_eq!(body, "# Implement from Plan\n\n## Purpose\n");
        assert!(!body.contains("name: implement-from-plan"));
    }

    #[test]
    fn load_skill_body_fails_fast_when_frontmatter_cannot_be_resolved() {
        // A BOM before the opening delimiter makes the frontmatter
        // unresolvable; the loader must not guess where instructions start.
        let dir = temp_cwd();
        fs::write(dir.join("SKILL.md"), "\u{feff}---\nname: x\n---\n# Body\n")
            .expect("write skill");
        match load_skill_body(Some(dir.as_path())) {
            Ok(_) => panic!("expected a hard error for BOM-leading frontmatter"),
            Err(msg) => assert!(msg.contains("cannot read the implement-from-plan skill")),
        }
    }

    #[test]
    fn keep_reset_verdict_maps_all_pause_outcomes() {
        // EOF (no answer, no flags) → keep.
        assert_eq!(
            keep_reset_verdict(None, false, false),
            KeepResetVerdict::Keep
        );
        // `keep` / line-mode `restart` / garbage → keep.
        assert_eq!(
            keep_reset_verdict(Some("keep".to_string().as_str()), false, false),
            KeepResetVerdict::Keep
        );
        assert_eq!(
            keep_reset_verdict(Some("restart".to_string().as_str()), false, false),
            KeepResetVerdict::Keep
        );
        assert_eq!(
            keep_reset_verdict(Some("mumble".to_string().as_str()), false, false),
            KeepResetVerdict::Keep
        );
        // Exactly `reset` (case/whitespace-insensitive) resets.
        assert_eq!(
            keep_reset_verdict(Some("reset".to_string().as_str()), false, false),
            KeepResetVerdict::Reset
        );
        assert_eq!(
            keep_reset_verdict(Some(" RESET ".to_string().as_str()), false, false),
            KeepResetVerdict::Reset
        );
        // Stop wins over any answer text (stop → exit 2, D12).
        assert_eq!(
            keep_reset_verdict(Some("reset".to_string().as_str()), true, false),
            KeepResetVerdict::Stop
        );
        // TUI restart (no answer, restart flag) → keep-and-proceed.
        assert_eq!(
            keep_reset_verdict(None, false, true),
            KeepResetVerdict::Keep
        );
    }

    #[test]
    fn keep_reset_prompt_renders_choices_and_round_trips_in_line_mode() {
        let question = render_keep_reset_prompt(3);
        let lines = ask_lines(question.as_str());
        // The rendered prompt names every choice, and each answer parses
        // back to its verdict — the line-mode round trip (the TUI modal
        // offers the same options through the same seam).
        assert!(lines.iter().any(|l| l.contains("keep")));
        assert!(lines.iter().any(|l| l.contains("reset")));
        assert!(lines.iter().any(|l| l.contains("stop")));
        assert!(lines.iter().any(|l| l.contains("3 grant(s) on file")));
        assert_eq!(
            parse_keep_reset_answer("reset".to_string().as_str()),
            KeepResetAnswer::Reset
        );
        assert_eq!(
            parse_keep_reset_answer("keep".to_string().as_str()),
            KeepResetAnswer::Keep
        );
        assert_eq!(
            parse_keep_reset_answer("RESTART".to_string().as_str()),
            KeepResetAnswer::Keep
        );
    }

    /// The three discovery candidates, in order, under a fixture home.
    fn permission_candidates(home: &Path) -> [PathBuf; 3] {
        let agent = home.join(".pi").join("agent");
        let npm_modules = agent.join("npm").join("node_modules");
        [
            npm_modules.join("@gotgenes").join("pi-permission-system"),
            npm_modules.join("pi-permission-system"),
            agent.join("extensions").join("pi-permission-system"),
        ]
    }

    #[test]
    fn permission_extension_resolution_env_wins_over_an_existing_install() {
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        let candidate_dirs = permission_candidates(&home);
        // Every discovery candidate is a fixture dir, so the env override is
        // proven to win against real installs rather than against absence.
        for installed in &candidate_dirs {
            fs::create_dir_all(installed).expect("create package dir");
        }
        let override_dir = home.join("ext").join("mine");
        fs::create_dir_all(&override_dir).expect("create override dir");
        let override_str = override_dir.to_string_lossy().into_owned();
        assert_eq!(
            resolve_permission_extension(Some(override_str.as_str()), Some(home_str.as_str())),
            Ok(override_dir.clone())
        );
    }

    #[test]
    fn permission_extension_resolution_env_set_but_absent_is_an_error() {
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        let absent = home.join("ext").join("missing");
        let absent_str = absent.to_string_lossy().into_owned();
        // No install under the fixture home → the error names both the env
        // value and every searched discovery path.
        let err = resolve_permission_extension(Some(absent_str.as_str()), Some(home_str.as_str()))
            .expect_err("a nonexistent env override must not resolve");
        assert!(err.contains("cannot resolve the permission-system extension"));
        assert!(err.contains(absent_str.as_str()));
        for candidate in permission_candidates(&home) {
            assert!(
                err.contains(candidate.to_string_lossy().as_ref()),
                "error lists every searched path: {err}"
            );
        }
    }

    #[test]
    fn permission_extension_resolution_env_absent_falls_through_to_discovery() {
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        let absent = home.join("ext").join("missing");
        let absent_str = absent.to_string_lossy().into_owned();
        // The installed candidate still resolves when the env override is a
        // typo — the bad override is named on the strict-mode/gated detail in
        // a later step, but it never blocks a real install.
        let installed = permission_candidates(&home)[1].clone();
        fs::create_dir_all(&installed).expect("create package dir");
        assert_eq!(
            resolve_permission_extension(Some(absent_str.as_str()), Some(home_str.as_str())),
            Ok(installed)
        );
    }

    #[test]
    fn permission_extension_resolution_searches_every_candidate_in_order() {
        // Each discovery layout resolves on its own…
        for index in 0..3 {
            let home = temp_cwd();
            let home_str = home.to_string_lossy().into_owned();
            let candidates = permission_candidates(&home);
            fs::create_dir_all(&candidates[index]).expect("create package dir");
            assert_eq!(
                resolve_permission_extension(None, Some(home_str.as_str())),
                Ok(candidates[index].clone()),
                "candidate {index} resolves"
            );
        }
        // …and the earliest existing candidate wins when several are
        // installed.
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        let candidates = permission_candidates(&home);
        for installed in &candidates {
            fs::create_dir_all(installed).expect("create package dir");
        }
        assert_eq!(
            resolve_permission_extension(None, Some(home_str.as_str())),
            Ok(candidates[0].clone())
        );
    }

    #[test]
    fn permission_extension_resolution_nothing_found_lists_searched_paths() {
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        let err = resolve_permission_extension(None, Some(home_str.as_str()))
            .expect_err("an unresolvable extension must fail fast");
        assert!(err.contains("cannot resolve the permission-system extension"));
        for candidate in permission_candidates(&home) {
            assert!(
                err.contains(candidate.to_string_lossy().as_ref()),
                "error lists every searched path: {err}"
            );
        }
    }

    #[test]
    fn permission_extension_resolution_blank_env_and_home_fall_through() {
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        let installed = permission_candidates(&home)[0].clone();
        fs::create_dir_all(&installed).expect("create package dir");
        // Set-but-blank (or whitespace-only) env values are not a resolution.
        for blank in ["", "   ", "\t\n"] {
            assert_eq!(
                resolve_permission_extension(Some(blank), Some(home_str.as_str())),
                Ok(installed.clone()),
                "blank env value {blank:?} falls through"
            );
        }
    }

    #[test]
    fn permission_extension_resolution_without_home_names_the_cause() {
        // No HOME at all → hard error that names the unset env var.
        let err = resolve_permission_extension(None, None)
            .expect_err("no HOME and no env var must fail fast");
        assert!(err.contains("cannot resolve the permission-system extension"));
        assert!(err.contains("HOME is unknown"));
        // A blank HOME is unknown too, not the cwd-relative root.
        let err = resolve_permission_extension(None, Some("   "))
            .expect_err("a blank HOME must fail fast");
        assert!(err.contains("HOME is unknown"));
        // A nonexistent env override plus no HOME names the env value.
        let err = resolve_permission_extension(Some("/ext/missing"), None)
            .expect_err("no HOME and a missing override must fail fast");
        assert!(err.contains("/ext/missing"));
        assert!(err.contains("HOME is unknown"));
    }

    #[test]
    fn clean_skill_resolution_defaults_from_home_and_env_wins() {
        // No clean-worktree skill installed in the fake home → None.
        let home = temp_cwd();
        let home_str = home.to_string_lossy().into_owned();
        assert_eq!(
            resolve_clean_skill_path(None, Some(home_str.as_str())),
            None
        );
        // Env override always wins, even over a real home install.
        assert_eq!(
            resolve_clean_skill_path(
                Some("/clean/skills/mine".to_string().as_str()),
                Some(home_str.as_str())
            ),
            Some(Path::new("/clean/skills/mine").to_path_buf())
        );
        // An installed skill under the fake home resolves.
        let installed = home
            .join(".pi")
            .join("agent")
            .join("skills")
            .join("clean-worktree");
        fs::create_dir_all(&installed).expect("create skill dir");
        fs::write(installed.join("SKILL.md"), "# skill").expect("write skill");
        assert_eq!(
            resolve_clean_skill_path(None, Some(home_str.as_str())),
            Some(installed)
        );
    }

    #[test]
    fn clean_skill_body_loading_is_tolerant_and_returns_the_path_body_pair() {
        // A missing directory is never a startup error → None.
        assert_eq!(
            load_clean_skill_body(temp_cwd().join("nope").as_path()),
            None
        );
        // A directory without a SKILL.md → None.
        assert_eq!(load_clean_skill_body(temp_cwd().as_path()), None);
        // Malformed frontmatter (BOM before the delimiter) → None, never a
        // hard error — the opposite posture of the fail-fast row skill.
        let bad = temp_cwd();
        fs::write(bad.join("SKILL.md"), "\u{feff}---\nname: x\n---\n# Body\n")
            .expect("write skill");
        assert_eq!(load_clean_skill_body(bad.as_path()), None);

        // A valid skill resolves to the (path, body) pair — the path feeds
        // the clean spawn's `--skill` argv, the stripped body its prompt.
        let dir = temp_cwd();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: clean-worktree\ndescription: x\n---\n\n# Clean Worktree\n\n## Goal\n",
        )
        .expect("write skill");
        let Some((path, body)) = load_clean_skill_body(dir.as_path()) else {
            panic!("expected the clean skill to load");
        };
        assert_eq!(path, dir);
        assert_eq!(body, "# Clean Worktree\n\n## Goal\n");
        assert!(!body.contains("name: clean-worktree"));
        // The loader is a pure function: every call re-resolves and returns
        // the same pair, so the gate can call the seam once per fire.
        let Some((path2, body2)) = load_clean_skill_body(dir.as_path()) else {
            panic!("expected the clean skill to load on a second call");
        };
        assert_eq!(path2, dir);
        assert_eq!(body2, body);
    }

    #[test]
    fn final_report_carries_clean_questions_and_the_orphan_tail() {
        // Step 6 report surface: a clean question pause's record and the
        // orphan's never-consumable tail must both show up in the final
        // report — the exit-2 path prints them verbatim.
        let row = TodoRow {
            id: "1".to_string(),
            number: 1,
            commit_message: "feat: row one".to_string(),
            logical_unit: "u".to_string(),
            deliverables: "d".to_string(),
            tests: "t".to_string(),
        };
        let outcomes: Vec<RowOutcome> = vec![
            RowOutcome::CleanQuestionPause {
                row: row.clone(),
                question: "may I discard target/?".to_string(),
                agent_id: "0".to_string(),
                records: vec![RunRecord {
                    attempt: 0,
                    row: row.clone(),
                    agent_id: "0".to_string(),
                    outcome: RunOutcomeKind::Completed,
                    question: Some("may I discard target/?".to_string()),
                    tail: Some("asks before destroying".to_string()),
                    transcript_path: None,
                    started_at: 1,
                    completed_at: None,
                    snapshot: None,
                }],
            },
            RowOutcome::CleanAnswerOrphaned {
                row: row.clone(),
                question: "may I discard target/?".to_string(),
                records: vec![RunRecord {
                    attempt: 0,
                    row: row.clone(),
                    agent_id: String::new(),
                    outcome: RunOutcomeKind::Failed,
                    question: Some("may I discard target/?".to_string()),
                    tail: Some(
                        "clean answer never consumable — the worktree is already clean".to_string(),
                    ),
                    transcript_path: None,
                    started_at: 1,
                    completed_at: None,
                    snapshot: None,
                }],
            },
        ];
        let report = format_final_report(&outcomes[..], 1).join("\n");
        assert!(report.contains("paused — clean-worktree agent asks a question"));
        assert!(report.contains("paused — clean answer cannot be consumed"));
        assert!(
            report.contains("question: may I discard target/?"),
            "the clean question reaches the report text"
        );
        assert!(
            report.contains("never consumable"),
            "the anomaly's never-consumable tail reaches the report text"
        );
    }

    #[test]
    fn final_report_lists_each_attempts_stats_from_the_terminal_snapshot() {
        let row = TodoRow {
            id: "1".to_string(),
            number: 1,
            commit_message: "feat: row one".to_string(),
            logical_unit: "u".to_string(),
            deliverables: "d".to_string(),
            tests: "t".to_string(),
        };
        let snap = WorkerSnapshot {
            id: 0,
            text: "assembled".to_string(),
            tool_uses: 38,
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
        let outcomes: Vec<RowOutcome> = vec![
            RowOutcome::Done {
                row: row.clone(),
                matched: MatchResult {
                    tier: MatchTier::Exact,
                    subject: Some("feat: row one".to_string()),
                },
                records: vec![RunRecord {
                    attempt: 1,
                    row: row.clone(),
                    agent_id: "0".to_string(),
                    outcome: RunOutcomeKind::Completed,
                    question: None,
                    tail: Some("one done".to_string()),
                    transcript_path: None,
                    started_at: 1_000_000,
                    completed_at: None,
                    snapshot: Some(snap),
                }],
            },
            RowOutcome::BudgetChoice {
                row: row.clone(),
                runs_used: 1,
                last_outcome: "failed".to_string(),
                records: vec![RunRecord {
                    attempt: 1,
                    row: row.clone(),
                    agent_id: "0".to_string(),
                    outcome: RunOutcomeKind::Failed,
                    question: None,
                    tail: Some("nope".to_string()),
                    transcript_path: None,
                    started_at: 1_000_000,
                    completed_at: None,
                    snapshot: None,
                }],
            },
        ];
        let report = format_final_report(&outcomes[..], 2);
        let text = report.join("\n");
        // The snapshot-backed attempt lists its stats; the duration suffix
        // is wall-clock, so the assertion pins the deterministic prefix.
        assert!(
            text.contains("      worker: 0 · cost $0.0451 · 59.3k tokens · ctx 61% · 4 turns · ")
        );
        // The snapshot-less record shows a run/outcome/tail but NO stats
        // line — exactly one stats line across the whole report.
        let stats_lines = report
            .iter()
            .filter(|l| l.starts_with("      worker: "))
            .collect::<Vec<_>>();
        assert_eq!(stats_lines.len(), 1);
        // The budget-exhausted attempt's run line is present without stats.
        assert!(text.contains("    run 1: failed"));
    }

    #[test]
    fn final_report_distinguishes_spawn_error_stops_with_repeated_records() {
        let row = TodoRow {
            id: "1".to_string(),
            number: 1,
            commit_message: "feat: row one".to_string(),
            logical_unit: "u".to_string(),
            deliverables: "d".to_string(),
            tests: "t".to_string(),
        };
        // Up to SPAWN_RETRY_LIMIT records may share the same attempt
        // number: each is one actual spawn attempt, never a spent run.
        let spawn_fail = |tail: &str| RunRecord {
            attempt: 1,
            row: row.clone(),
            agent_id: String::new(),
            outcome: RunOutcomeKind::SpawnError,
            question: None,
            tail: Some(tail.to_string()),
            transcript_path: None,
            started_at: 1_000_000,
            completed_at: None,
            snapshot: None,
        };
        let outcomes: Vec<RowOutcome> = vec![RowOutcome::SpawnError {
            row: row.clone(),
            records: vec![
                spawn_fail("the prompt command was rejected: peer closed the RPC stream"),
                spawn_fail("the prompt command was rejected: peer closed the RPC stream"),
                spawn_fail("the prompt command was rejected: peer closed the RPC stream"),
            ],
        }];
        let report = format_final_report(&outcomes[..], 1);
        let text = report.join("\n");
        assert!(
            text.contains("  row 1: stopped — worker spawn failed"),
            "distinct outcome label, not budget exhausted"
        );
        let spawn_lines = report
            .iter()
            .filter(|l| l.starts_with("    run 1: spawn-error"))
            .collect::<Vec<_>>();
        assert_eq!(
            spawn_lines.len(),
            3,
            "one report line per actual spawn attempt"
        );
        assert!(
            text.contains("peer closed the RPC stream"),
            "each respawn tail reaches the report"
        );
        assert!(
            !text.contains("budget exhausted"),
            "spawn-error must never read as budget exhaustion"
        );
    }

    fn report_bundle(row: TodoRow) -> EvidenceBundle {
        EvidenceBundle {
            head: "6bf2c8e1234".to_string(),
            branch: "agent/hole".to_string(),
            row,
            diff: SubjectDiff {
                planned: "feat: b".to_string(),
                best_subject: "feat: b plus extra".to_string(),
                best_commit: Some("abcdef1234567890".to_string()),
                tier: MatchTier::Candidate,
                ratio: 0.75,
                extra_tokens: vec!["plus".to_string(), "extra".to_string()],
                missing_tokens: Vec::new(),
            },
            successors: vec![(3, "fedcba".to_string(), "feat: c".to_string())],
            candidate_commits: Vec::new(),
            best_patch: None,
            deliverables: vec![Deliverable::Present("tests/leakage.rs".to_string())],
            worktree: Vec::new(),
            truncated: Vec::new(),
        }
    }

    fn report_verifier() -> VerifierReport {
        VerifierReport {
            row: 2,
            head: "6bf2c8e1234".to_string(),
            verdict: Verdict::Complete,
            fields: VerdictFields {
                evidence: vec!["tests/leakage.rs".to_string()],
                missing: None,
                explanation: Some("the suite exists and covers the case".to_string()),
            },
            agent_id: "pi-plan-verify-2".to_string(),
            transcript_path: Some(PathBuf::from("/run/sessions/pi-verify/session.jsonl")),
            cost: Some(0.0123),
            tokens: Some(12_345),
            turns: 3,
            started_at: 1,
            completed_at: Some(2),
            snapshot: None,
        }
    }

    #[test]
    fn final_report_names_the_evidence_and_verifier_facts_of_a_verified_row() {
        let todo = parse_plan(THREE_ROWS);
        let outcome = RowOutcome::Verified {
            row: todo.rows[1].clone(),
            bundle: report_bundle(todo.rows[1].clone()),
            report: report_verifier(),
        };
        let report = format_final_report(&[outcome], 3).join("\n");
        assert!(
            report.contains("  row 2: verified without a commit (verifier COMPLETE)"),
            "{report}"
        );
        assert!(
            report.contains("    evidence: tests/leakage.rs; commit abcdef1234567890"),
            "the supervisor-authored evidence is named: {report}"
        );
        assert!(
            report.contains(
                "    verifier: COMPLETE · pi-plan-verify-2 · HEAD 6bf2c8e · 3 turn(s) · \
                 $0.0123 · 12.3k tokens · transcript /run/sessions/pi-verify/session.jsonl"
            ),
            "the verifier facts are named: {report}"
        );
    }

    #[test]
    fn final_report_prints_near_miss_facts_and_the_chosen_unrecognised_action() {
        let todo = parse_plan(THREE_ROWS);
        let near_miss = RowOutcome::NearMiss {
            row: todo.rows[1].clone(),
            subject: "feat: b plus extra".to_string(),
            commit: Some("abcdef1234567890".to_string()),
            tier: MatchTier::Candidate,
            ratio: 0.75,
            records: Vec::new(),
        };
        let unrecognised = RowOutcome::UnrecognisedRow {
            row: todo.rows[1].clone(),
            reasons: vec![crate::supervise::UnrecognisedReason::Hole {
                successors: vec![(3, "fedcba".to_string(), "feat: c".to_string())],
            }],
            evidence: Some(report_bundle(todo.rows[1].clone())),
            verifier: Some(VerifierReport {
                verdict: Verdict::Incomplete,
                ..report_verifier()
            }),
            reword: None,
            action: Some("stop".to_string()),
            records: Vec::new(),
        };
        let report = format_final_report(&[near_miss, unrecognised], 3).join("\n");
        assert!(
            report.contains(
                "  row 2: near-miss — needs adjudication: \"feat: b plus extra\" \
                 (abcdef1, candidate, 75% match)"
            ),
            "{report}"
        );
        assert!(
            report
                .contains("  row 2: unrecognised — needs a decision (hole) — operator chose stop"),
            "the deterministic label and the chosen action agree: {report}"
        );
        assert!(
            report.contains("    verifier explanation (unverified):\n      the suite exists"),
            "model prose is labelled unverified: {report}"
        );
    }
}
