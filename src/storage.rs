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
use std::path::{Path, PathBuf};

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
}
