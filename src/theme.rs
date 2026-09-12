//! Pi theme loader (plan step 1) — resolves the operator's active Pi theme
//! at runtime so the TUI matches the live Pi terminal's colors.
//!
//! Pipeline, mirroring Pi's own precedence (`themes.md`):
//!
//! 1. **Selection** — `--theme <path>` wins over the settings file's
//!    `theme` name, which wins over the bundled gruvbox-dark palette.
//! 2. **Named resolution** — a theme name is looked up as `<name>.json`
//!    across injected roots in Pi's order: global (`~/.pi/agent/themes`)
//!    → built-ins (`<pi>/dist/modes/interactive/theme`) → project
//!    (`.pi/themes`) → installed packages. The roots are injected so the
//!    lookup order is unit-testable without touching the real filesystem.
//! 3. **Parse/resolve** — the theme JSON (`vars` alias map + `colors`
//!    token map, per theme-schema.json) is resolved recursively into a
//!    [`Palette`] of concrete colors. `""` means "terminal default";
//!    missing or corrupt themes fall back to [`default_palette`] — the
//!    loader never fails hard.
//!
//! Everything here except the two thin `read_*` wrappers is pure and
//! unit-testable without a terminal.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A concrete terminal color: an RGB triple, or the terminal's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// Terminal default (`""` in the theme JSON) — emit no SGR escape.
    Default,
    /// Truecolor RGB value.
    Rgb { r: u8, g: u8, b: u8 },
}

/// The resolved color tokens this UI uses, keyed on Pi's token names.
///
/// Values are already resolved (var aliases expanded, `""` → [`Color::Default`]),
/// so renderers can style against this struct directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub accent: Color,
    pub border: Color,
    pub border_accent: Color,
    pub border_muted: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    pub muted: Color,
    pub dim: Color,
    pub text: Color,
    pub thinking_text: Color,
    pub user_message_bg: Color,
    pub user_message_text: Color,
    pub md_heading: Color,
    pub tool_title: Color,
    pub tool_pending_bg: Color,
    pub tool_success_bg: Color,
    pub tool_error_bg: Color,
    pub tool_output: Color,
    pub bash_mode: Color,
    pub thinking_high: Color,
    pub md_code: Color,
    pub md_code_block_border: Color,
}

/// One parsed theme JSON: the `vars` alias map and the `colors` token map.
///
/// Values are raw strings as written in the file (`"accent"`, `"#fabd2f"`,
/// `""`) — resolution to [`Color`] happens in [`resolve_palette`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Var name → var value (usually a hex color, possibly another alias).
    pub vars: BTreeMap<String, String>,
    /// Token name → color value (`""` = terminal default).
    pub colors: BTreeMap<String, String>,
}

/// Where the active theme comes from (selection precedence, plan locked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeSource {
    /// An explicit theme JSON path from `--theme`.
    Path(PathBuf),
    /// A theme name from the settings file's `theme` key.
    Name(String),
    /// The bundled gruvbox-dark fallback palette.
    Bundled,
}

/// The directories searched for a named theme, in Pi's documented order:
/// global → built-ins → project → packages. Injected so tests can build a
/// fake filesystem and assert the precedence exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeRoots {
    /// `~/.pi/agent/themes` (global user themes).
    pub global_dir: PathBuf,
    /// `<pi-install>/dist/modes/interactive/theme` (bundled themes), in
    /// search order.
    pub builtin_dirs: Vec<PathBuf>,
    /// `.pi/themes` in the project root, when the project is trusted.
    pub project_dir: Option<PathBuf>,
    /// Installed packages' `themes` directories, in search order.
    pub package_dirs: Vec<PathBuf>,
}

const HEX_DIGITS: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb { r, g, b }
}

fn hex_digit(c: char) -> Option<u8> {
    const LOWER: &str = "0123456789abcdef";
    const UPPER: &str = "0123456789ABCDEF";
    for (i, d) in LOWER.chars().enumerate() {
        if d == c {
            return Some(i as u8);
        }
    }
    for (i, d) in UPPER.chars().enumerate() {
        if d == c {
            return Some(i as u8);
        }
    }
    None
}

/// Two lowercase hex digits for a byte (used by property tests to prove
/// the resolution fixed point).
fn hex2(n: u8) -> String {
    let mut out = String::new();
    out.push(HEX_DIGITS[(n >> 4) as usize]);
    out.push(HEX_DIGITS[(n & 0x0f) as usize]);
    out
}

/// Serialize a color back to a `#rrggbb` string (`""` for [`Color::Default`]).
pub fn rgb_to_hex(color: &Color) -> String {
    match color {
        Color::Default => "".to_string(),
        Color::Rgb { r, g, b } => format!("#{}{}{}", hex2(*r), hex2(*g), hex2(*b)),
    }
}

/// Parse `#rrggbb` or `#rgb` (the `#` is optional) into a color.
/// Accepts upper- and lowercase hex; anything else yields `None`.
pub fn parse_hex_color(value: &str) -> Option<Color> {
    let trimmed = value.trim();
    let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if hex.len() != 6 && hex.len() != 3 {
        return None;
    }
    let mut parsed: Vec<u8> = Vec::new();
    for c in hex.chars() {
        match hex_digit(c) {
            Some(d) => parsed.push(d),
            None => return None,
        }
    }
    match hex.len() {
        6 => {
            let r = parsed[0] * 16 + parsed[1];
            let g = parsed[2] * 16 + parsed[3];
            let b = parsed[4] * 16 + parsed[5];
            Some(Color::Rgb { r, g, b })
        }
        3 => {
            let r = parsed[0] * 17;
            let g = parsed[1] * 17;
            let b = parsed[2] * 17;
            Some(Color::Rgb { r, g, b })
        }
        _ => None,
    }
}

/// Resolve one color value against a theme's `vars` map.
///
/// Values may be `""` (terminal default), `#rrggbb`, a bare var name
/// (`"gray"`), or a `$`-prefixed alias (`"$accent"`); var values may alias
/// other vars. Cycles and unknown references yield `None` (the caller falls
/// back) — resolution always terminates.
pub fn resolve_color(value: &str, vars: &BTreeMap<String, String>) -> Option<Color> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    resolve_color_at(value, vars, &mut seen)
}

/// Recursive alias walk with cycle detection via the visited set.
fn resolve_color_at(
    value: &str,
    vars: &BTreeMap<String, String>,
    seen: &mut BTreeSet<String>,
) -> Option<Color> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Some(Color::Default);
    }
    if trimmed.starts_with('#') {
        return parse_hex_color(trimmed);
    }
    let name = trimmed.strip_prefix('$').unwrap_or(trimmed);
    if !seen.insert(name.to_string()) {
        return None; // alias cycle
    }
    match vars.get(name) {
        Some(next) => resolve_color_at(next.as_str(), vars, seen),
        None => None, // unknown reference
    }
}

/// Select the theme source by precedence: an explicit `--theme` path wins,
/// then the settings file's theme name, then the bundled palette.
pub fn select_source(cli_theme: Option<&Path>, settings_theme: Option<&str>) -> ThemeSource {
    if let Some(path) = cli_theme {
        return ThemeSource::Path(path.to_path_buf());
    }
    match settings_theme {
        Some(name) if !name.trim().is_empty() => ThemeSource::Name(name.trim().to_string()),
        _ => ThemeSource::Bundled,
    }
}

/// The first `<name>.json` matching across the injected roots, in Pi's
/// documented order (global → built-ins → project → packages), or `None`.
pub fn find_theme_file(name: &str, roots: &ThemeRoots) -> Option<PathBuf> {
    let file_name = format!("{name}.json");
    let mut candidates: Vec<PathBuf> = vec![roots.global_dir.join(file_name.as_str())];
    for dir in &roots.builtin_dirs {
        candidates.push(dir.join(file_name.as_str()));
    }
    if let Some(project_dir) = &roots.project_dir {
        candidates.push(project_dir.join(file_name.as_str()));
    }
    for dir in &roots.package_dirs {
        candidates.push(dir.join(file_name.as_str()));
    }
    candidates.iter().find(|c| c.exists()).map(|path| path.to_path_buf())
}

/// The bundled fallback palette: the gruvbox-dark tokens, resolved (the
/// values this machine's active theme resolves to, verified 2026-09-12).
/// `text`/`userMessageText` stay terminal-default, exactly like the theme.
pub fn default_palette() -> Palette {
    Palette {
        accent: rgb(250, 189, 47),
        border: rgb(146, 131, 116),
        border_accent: rgb(250, 189, 47),
        border_muted: rgb(55, 55, 55),
        success: rgb(142, 192, 124),
        error: rgb(251, 73, 52),
        warning: rgb(250, 189, 47),
        muted: rgb(146, 131, 116),
        dim: rgb(146, 131, 116),
        text: Color::Default,
        thinking_text: rgb(146, 131, 116),
        user_message_bg: rgb(45, 45, 45),
        user_message_text: Color::Default,
        md_heading: rgb(235, 219, 178),
        tool_title: rgb(235, 219, 178),
        tool_pending_bg: rgb(48, 48, 48),
        tool_success_bg: rgb(47, 48, 47),
        tool_error_bg: rgb(56, 47, 46),
        tool_output: rgb(235, 219, 178),
        bash_mode: rgb(250, 189, 47),
        thinking_high: rgb(250, 189, 47),
        md_code: rgb(250, 189, 47),
        md_code_block_border: rgb(200, 139, 0),
    }
}

fn pick_color(map: &BTreeMap<String, Color>, token: &str, fallback: Color) -> Color {
    match map.get(token) {
        Some(color) => *color,
        None => fallback,
    }
}

/// Resolve a parsed theme into a concrete [`Palette`].
///
/// Tokens that are missing, malformed, or alias an unknown var fall back to
/// the bundled gruvbox-dark value for that token — a single broken token
/// never cancels the whole theme.
pub fn resolve_palette(theme: &Theme) -> Palette {
    let mut resolved: BTreeMap<String, Color> = BTreeMap::new();
    for (token, value) in &theme.colors {
        if let Some(color) = resolve_color(value.as_str(), &theme.vars) {
            resolved.insert(token.clone(), color);
        }
    }
    let fallback = default_palette();
    Palette {
        accent: pick_color(&resolved, "accent", fallback.accent),
        border: pick_color(&resolved, "border", fallback.border),
        border_accent: pick_color(&resolved, "borderAccent", fallback.border_accent),
        border_muted: pick_color(&resolved, "borderMuted", fallback.border_muted),
        success: pick_color(&resolved, "success", fallback.success),
        error: pick_color(&resolved, "error", fallback.error),
        warning: pick_color(&resolved, "warning", fallback.warning),
        muted: pick_color(&resolved, "muted", fallback.muted),
        dim: pick_color(&resolved, "dim", fallback.dim),
        text: pick_color(&resolved, "text", fallback.text),
        thinking_text: pick_color(&resolved, "thinkingText", fallback.thinking_text),
        user_message_bg: pick_color(&resolved, "userMessageBg", fallback.user_message_bg),
        user_message_text: pick_color(&resolved, "userMessageText", fallback.user_message_text),
        md_heading: pick_color(&resolved, "mdHeading", fallback.md_heading),
        tool_title: pick_color(&resolved, "toolTitle", fallback.tool_title),
        tool_pending_bg: pick_color(&resolved, "toolPendingBg", fallback.tool_pending_bg),
        tool_success_bg: pick_color(&resolved, "toolSuccessBg", fallback.tool_success_bg),
        tool_error_bg: pick_color(&resolved, "toolErrorBg", fallback.tool_error_bg),
        tool_output: pick_color(&resolved, "toolOutput", fallback.tool_output),
        bash_mode: pick_color(&resolved, "bashMode", fallback.bash_mode),
        thinking_high: pick_color(&resolved, "thinkingHigh", fallback.thinking_high),
        md_code: pick_color(&resolved, "mdCode", fallback.md_code),
        md_code_block_border: pick_color(
            &resolved,
            "mdCodeBlockBorder",
            fallback.md_code_block_border,
        ),
    }
}

// ---------------- thin I/O wrappers ----------------

/// Parse a theme JSON document (`vars` + `colors` maps) into a [`Theme`].
/// `None` when the document is not valid JSON, has no `colors` object, or
/// the colors object is empty.
pub fn parse_theme(json: &str) -> Option<Theme> {
    let parsed: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = parsed.as_object()?;
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    if let Some(serde_json::Value::Object(var_map)) = obj.get("vars") {
        for (key, value) in var_map {
            if let Some(s) = value.as_str() {
                vars.insert(key.clone(), s.to_string());
            }
        }
    }
    let Some(serde_json::Value::Object(color_map)) = obj.get("colors") else {
        return None;
    };
    if color_map.is_empty() {
        return None;
    }
    let mut colors: BTreeMap<String, String> = BTreeMap::new();
    for (key, value) in color_map {
        if let Some(s) = value.as_str() {
            colors.insert(key.clone(), s.to_string());
        }
    }
    Some(Theme { vars, colors })
}

/// Read a theme JSON file (`None` on any read/parse failure).
pub fn read_theme_file(path: &Path) -> Option<Theme> {
    let raw = std::fs::read_to_string(path).ok()?;
    parse_theme(&raw)
}

/// The settings file's `theme` value (`~/.pi/agent/settings.json`), `None`
/// when unreadable, unparseable, or unset.
pub fn read_settings_theme(settings_path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(settings_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    parsed
        .as_object()
        .and_then(|d| d.get("theme"))
        .and_then(serde_json::Value::as_str)
        .map(|s| s.to_string())
}

/// Resolve the active palette from a theme source. A missing or corrupt
/// explicit path/name falls back to the bundled gruvbox-dark palette —
/// this function never fails hard.
pub fn resolve_active_palette(source: &ThemeSource, roots: &ThemeRoots) -> Palette {
    match source {
        ThemeSource::Path(path) => match read_theme_file(path) {
            Some(theme) => resolve_palette(&theme),
            None => default_palette(),
        },
        ThemeSource::Name(name) => match find_theme_file(name.as_str(), roots) {
            Some(path) => match read_theme_file(&path) {
                Some(theme) => resolve_palette(&theme),
                None => default_palette(),
            },
            None => default_palette(),
        },
        ThemeSource::Bundled => default_palette(),
    }
}

/// ANSI SGR builders for truecolor styling (plan's `Stylize::fg`/`bg`).
///
/// [`Color::Default`] emits no escape at all, so unstyled text needs no
/// reset handling at the call site.
pub struct Stylize {}

impl Stylize {
    /// Foreground color escape (`\e[38;2;r;g;bm`), or `""` for default.
    pub fn fg(color: &Color) -> String {
        match color {
            Color::Default => "".to_string(),
            Color::Rgb { r, g, b } => format!("\u{1b}[38;2;{r};{g};{b}m"),
        }
    }

    /// Background color escape (`\e[48;2;r;g;bm`), or `""` for default.
    pub fn bg(color: &Color) -> String {
        match color {
            Color::Default => "".to_string(),
            Color::Rgb { r, g, b } => format!("\u{1b}[48;2;{r};{g};{b}m"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A throwaway directory for one test (mirrors the cli.rs convention).
    fn temp_dir() -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("pi-plan-theme-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(path, content).expect("write file");
    }

    /// A representative gruvbox-dark-style theme (subset of the real file):
    /// var aliases, `""` defaults, and tokens referencing vars by bare name.
    const SAMPLE: &str = "{\n\
  \"vars\": {\n\
    \"bg\": \"#282828\", \"fg\": \"#ebdbb2\", \"gray\": \"#928374\",\n\
    \"darkGray\": \"#373737\", \"accent\": \"#fabd2f\", \"accentDark\": \"#c88b00\",\n\
    \"white\": \"#ebdbb2\", \"panel\": \"#2d2d2d\", \"panelAlt\": \"#303030\",\n\
    \"panelSuccess\": \"#2f302f\", \"panelError\": \"#382f2e\",\n\
    \"success\": \"#8ec07c\", \"error\": \"#fb4934\", \"warning\": \"#fabd2f\"\n\
  },\n\
  \"colors\": {\n\
    \"accent\": \"accent\", \"border\": \"gray\", \"borderAccent\": \"accent\",\n\
    \"borderMuted\": \"darkGray\", \"success\": \"success\", \"error\": \"error\",\n\
    \"warning\": \"warning\", \"muted\": \"gray\", \"dim\": \"gray\", \"text\": \"\",\n\
    \"thinkingText\": \"gray\", \"userMessageBg\": \"panel\", \"userMessageText\": \"\",\n\
    \"mdHeading\": \"white\", \"toolTitle\": \"white\", \"toolPendingBg\": \"panelAlt\",\n\
    \"toolSuccessBg\": \"panelSuccess\", \"toolErrorBg\": \"panelError\",\n\
    \"toolOutput\": \"fg\", \"bashMode\": \"accent\", \"thinkingHigh\": \"accent\",\n\
    \"mdCode\": \"accent\", \"mdCodeBlockBorder\": \"accentDark\"\n\
  }\n\
}";

    fn sample_theme() -> Theme {
        parse_theme(SAMPLE).expect("sample is a valid theme")
    }

    // ---- hex parsing ----

    #[test]
    fn parse_hex_color_handles_6_digit_3_digit_and_hash_forms() {
        assert_eq!(
            parse_hex_color("#fabd2f"),
            Some(Color::Rgb {
                r: 250,
                g: 189,
                b: 47
            }) as Option<Color>
        );
        assert_eq!(
            parse_hex_color("fabd2f"),
            Some(Color::Rgb {
                r: 250,
                g: 189,
                b: 47
            })
        );
        assert_eq!(
            parse_hex_color("#FABD2F"),
            Some(Color::Rgb {
                r: 250,
                g: 189,
                b: 47
            }),
            "hex digits are case-insensitive"
        );
        assert_eq!(
            parse_hex_color("#faf"),
            Some(Color::Rgb {
                r: 255,
                g: 170,
                b: 255
            })
        );
    }

    #[test]
    fn parse_hex_color_rejects_bad_lengths_and_illegal_characters() {
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("#fabd2"), None);
        assert_eq!(parse_hex_color("#fabd2f0"), None);
        assert_eq!(parse_hex_color("#ggbd2f"), None);
        assert_eq!(parse_hex_color("gray"), None);
    }

    // ---- var-alias resolution ----

    fn vars_of(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in entries {
            out.insert(k.to_string(), v.to_string());
        }
        out
    }

    #[test]
    fn resolve_color_maps_empty_hash_bare_and_dollar_aliases() {
        let vars = vars_of(&[
            ("gray", "#928374"),
            ("accent", "#fabd2f"),
            ("fg", "#ebdbb2"),
        ]);
        // "" → terminal default.
        assert_eq!(resolve_color("", &vars), Some(Color::Default));
        // Bare var name.
        assert_eq!(
            resolve_color("gray", &vars),
            Some(Color::Rgb {
                r: 146,
                g: 131,
                b: 116
            })
        );
        // $-prefixed var name.
        assert_eq!(
            resolve_color("$accent", &vars),
            Some(Color::Rgb {
                r: 250,
                g: 189,
                b: 47
            })
        );
        // Literal hex (not a var reference).
        assert_eq!(
            resolve_color("#ebdbb2", &vars),
            Some(Color::Rgb {
                r: 235,
                g: 219,
                b: 178
            })
        );
    }

    #[test]
    fn resolve_color_follows_nested_aliases() {
        let vars = vars_of(&[("a", "$b"), ("b", "c"), ("c", "#123456")]);
        assert_eq!(
            resolve_color("$a", &vars),
            Some(Color::Rgb {
                r: 0x12,
                g: 0x34,
                b: 0x56
            })
        );
    }

    #[test]
    fn resolve_color_returns_none_for_unknown_references() {
        let vars = vars_of(&[("gray", "#928374")]);
        assert_eq!(resolve_color("nope", &vars), None);
        assert_eq!(resolve_color("$nope", &vars), None);
    }

    #[test]
    fn resolve_color_detects_self_and_mutual_cycles() {
        let vars = vars_of(&[
            ("a", "$a"),
            ("b", "$c"),
            ("c", "$b"),
            ("d", "$e"),
            ("e", "#123456"),
        ]);
        assert_eq!(resolve_color("$a", &vars), None, "self-loop");
        assert_eq!(resolve_color("$b", &vars), None, "mutual cycle");
        assert!(resolve_color("$d", &vars).is_some(), "acyclic tail resolves");
    }

    // ---- selection ----

    #[test]
    fn select_source_prefers_cli_path_over_settings_name_over_bundled() {
        let cli = Some(Path::new("/tmp/my-theme.json").to_path_buf());
        match select_source(cli.as_deref(), Some("gruvbox-dark".to_string().as_str())) {
            ThemeSource::Path(p) => assert_eq!(
                p.to_string_lossy().into_owned(),
                "/tmp/my-theme.json".to_string()
            ),
            other => panic!("expected Path, got {other:?}"),
        }
        match select_source(None, Some("gruvbox-dark".to_string().as_str())) {
            ThemeSource::Name(name) => assert_eq!(name, "gruvbox-dark".to_string()),
            other => panic!("expected Name, got {other:?}"),
        }
        // Empty or absent settings theme name → bundled.
        assert!(matches!(select_source(None, None), ThemeSource::Bundled));
        assert!(matches!(
            select_source(None, Some("   ".to_string().as_str())),
            ThemeSource::Bundled
        ));
    }

    // ---- named resolution across injected roots ----

    #[test]
    fn find_theme_file_searches_pi_order_global_builtin_project_packages() {
        let root = temp_dir();
        write(
            &root,
            "global/themes/dark.json",
            "{\"colors\":{\"accent\":\"#111111\"}}",
        );
        write(&root, "builtin1/theme/dark.json", "b1");
        write(&root, "builtin2/theme/dark.json", "b2");
        write(&root, "project/.pi/themes/dark.json", "p");
        write(&root, "packages/pkg1/themes/dark.json", "pkg1");
        write(&root, "packages/pkg2/themes/dark.json", "pkg2");

        let roots = ThemeRoots {
            global_dir: root.join("global/themes"),
            builtin_dirs: vec![root.join("builtin1/theme"), root.join("builtin2/theme")],
            project_dir: Some(root.join("project/.pi/themes")),
            package_dirs: vec![
                root.join("packages/pkg1/themes"),
                root.join("packages/pkg2/themes"),
            ],
        };

        // Global wins over everything.
        assert_eq!(
            find_theme_file("dark", &roots).map(|p| p.to_string_lossy().into_owned()),
            Some(
                root.join("global/themes/dark.json")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        // Removing the global file exposes the first built-in.
        let _ = std::fs::remove_file(root.join("global/themes/dark.json"));
        assert_eq!(
            find_theme_file("dark", &roots).map(|p| p.to_string_lossy().into_owned()),
            Some(
                root.join("builtin1/theme/dark.json")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        // Built-ins are searched in order, all before the project …
        let _ = std::fs::remove_file(root.join("builtin1/theme/dark.json"));
        assert_eq!(
            find_theme_file("dark", &roots).map(|p| p.to_string_lossy().into_owned()),
            Some(
                root.join("builtin2/theme/dark.json")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        // … then the project, then the packages in order.
        let _ = std::fs::remove_file(root.join("builtin2/theme/dark.json"));
        assert_eq!(
            find_theme_file("dark", &roots).map(|p| p.to_string_lossy().into_owned()),
            Some(
                root.join("project/.pi/themes/dark.json")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        let _ = std::fs::remove_file(root.join("project/.pi/themes/dark.json"));
        assert_eq!(
            find_theme_file("dark", &roots).map(|p| p.to_string_lossy().into_owned()),
            Some(
                root.join("packages/pkg1/themes/dark.json")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        let _ = std::fs::remove_file(root.join("packages/pkg1/themes/dark.json"));
        assert_eq!(
            find_theme_file("dark", &roots).map(|p| p.to_string_lossy().into_owned()),
            Some(
                root.join("packages/pkg2/themes/dark.json")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        // A name nobody ships is None.
        assert_eq!(find_theme_file("light", &roots), None);
    }

    #[test]
    fn find_theme_file_works_with_no_project_or_packages() {
        let root = temp_dir();
        write(&root, "t/dark.json", "x");
        let roots = ThemeRoots {
            global_dir: root.join("t"),
            builtin_dirs: vec![],
            project_dir: None,
            package_dirs: vec![],
        };
        assert_eq!(
            find_theme_file("dark", &roots).map(|p| p.to_string_lossy().into_owned()),
            Some(root.join("t/dark.json").to_string_lossy().into_owned())
        );
        assert_eq!(find_theme_file("nope", &roots), None);
    }

    // ---- palette resolution ----

    #[test]
    fn resolve_palette_maps_aliases_defaults_and_fallback() {
        let palette = resolve_palette(&sample_theme());
        // Aliased vars resolve per the sample's vars map.
        assert_eq!(
            palette.accent,
            Color::Rgb {
                r: 250,
                g: 189,
                b: 47
            }
        );
        assert_eq!(
            palette.border,
            Color::Rgb {
                r: 146,
                g: 131,
                b: 116
            }
        );
        assert_eq!(
            palette.border_muted,
            Color::Rgb {
                r: 55,
                g: 55,
                b: 55
            }
        );
        assert_eq!(
            palette.tool_output,
            Color::Rgb {
                r: 235,
                g: 219,
                b: 178
            }
        );
        assert_eq!(
            palette.md_code_block_border,
            Color::Rgb {
                r: 200,
                g: 139,
                b: 0
            }
        );
        // "" maps to terminal default.
        assert_eq!(palette.text, Color::Default);
        assert_eq!(palette.user_message_text, Color::Default);
        // Tokens absent from the sample fall back to the bundled palette.
        assert_eq!(palette.thinking_high, default_palette().thinking_high);
    }

    #[test]
    fn resolve_palette_falls_back_per_token_on_malformed_values() {
        let mut theme = sample_theme();
        theme
            .colors
            .insert("accent".to_string(), "#zzz123".to_string());
        theme
            .colors
            .insert("border".to_string(), "unknownvar".to_string());
        let palette = resolve_palette(&theme);
        assert_eq!(
            palette.accent,
            default_palette().accent,
            "bad hex falls back"
        );
        assert_eq!(
            palette.border,
            default_palette().border,
            "unknown var falls back"
        );
        // Unrelated tokens keep their themed values.
        assert_eq!(
            palette.success,
            Color::Rgb {
                r: 142,
                g: 192,
                b: 124
            }
        );
    }

    #[test]
    fn default_palette_matches_the_locked_gruvbox_dark_values() {
        let p = default_palette();
        assert_eq!(p.accent, rgb(250, 189, 47));
        assert_eq!(p.text, Color::Default);
        assert_eq!(p.user_message_bg, rgb(45, 45, 45));
        assert_eq!(p.tool_pending_bg, rgb(48, 48, 48));
        assert_eq!(p.tool_success_bg, rgb(47, 48, 47));
        assert_eq!(p.tool_error_bg, rgb(56, 47, 46));
    }

    // ---- parsing and files ----

    #[test]
    fn parse_theme_accepts_a_full_theme_document() {
        let theme = sample_theme();
        assert_eq!(
            theme.vars.get("accent").map(|v| v.as_str()),
            Some("#fabd2f")
        );
        assert_eq!(theme.colors.get("border").map(|v| v.as_str()), Some("gray"));
        assert_eq!(theme.colors.get("text").map(|v| v.as_str()), Some(""));
    }

    #[test]
    fn parse_theme_rejects_non_documents_and_empty_colors() {
        assert_eq!(parse_theme("not json"), None);
        assert_eq!(parse_theme("[]"), None);
        assert_eq!(parse_theme("{\"vars\":{}}"), None, "missing colors");
        assert_eq!(parse_theme("{\"colors\":{}}"), None, "empty colors");
    }

    #[test]
    fn resolve_active_palette_uses_a_path_when_given() {
        let root = temp_dir();
        write(&root, "my.json", SAMPLE);
        write(&root, "other.json", "not a theme");
        let roots = ThemeRoots {
            global_dir: root.join("global"),
            builtin_dirs: vec![],
            project_dir: None,
            package_dirs: vec![],
        };
        let themed = resolve_active_palette(&ThemeSource::Path(root.join("my.json")), &roots);
        assert_eq!(
            themed.accent,
            Color::Rgb {
                r: 250,
                g: 189,
                b: 47
            }
        );
        // A corrupt/missing path falls back — never fails hard.
        assert_eq!(
            resolve_active_palette(&ThemeSource::Path(root.join("other.json")), &roots),
            default_palette()
        );
        assert_eq!(
            resolve_active_palette(&ThemeSource::Path(root.join("gone.json")), &roots),
            default_palette()
        );
    }

    #[test]
    fn resolve_active_palette_resolves_names_across_roots_and_falls_back() {
        let root = temp_dir();
        write(&root, "g/themes/mine.json", SAMPLE);
        let roots = ThemeRoots {
            global_dir: root.join("g/themes"),
            builtin_dirs: vec![],
            project_dir: None,
            package_dirs: vec![],
        };
        let themed = resolve_active_palette(&ThemeSource::Name("mine".to_string()), &roots);
        assert_eq!(
            themed.dim,
            Color::Rgb {
                r: 146,
                g: 131,
                b: 116
            }
        );
        // An unknown name falls back to the bundled palette.
        assert_eq!(
            resolve_active_palette(&ThemeSource::Name("nope".to_string()), &roots),
            default_palette()
        );
        assert_eq!(
            resolve_active_palette(&ThemeSource::Bundled, &roots),
            default_palette()
        );
    }

    #[test]
    fn read_settings_theme_reads_the_theme_key() {
        let root = temp_dir();
        let settings = root.join("settings.json");
        std::fs::write(&settings, "{\"theme\": \"gruvbox-dark\"}").expect("write settings");
        assert_eq!(
            read_settings_theme(&settings),
            Some("gruvbox-dark".to_string())
        );
        // Missing file, corrupt JSON, and missing key are all None.
        assert_eq!(read_settings_theme(&root.join("gone.json")), None);
        std::fs::write(&settings, "not json").expect("write corrupt");
        assert_eq!(read_settings_theme(&settings), None);
        std::fs::write(&settings, "{\"theme\": 3}").expect("write wrong type");
        assert_eq!(read_settings_theme(&settings), None);
    }

    // ---- ANSI mapping ----

    #[test]
    fn stylize_emits_truecolor_escapes_and_nothing_for_default() {
        assert_eq!(
            Stylize::fg(&Color::Rgb {
                r: 250,
                g: 189,
                b: 47
            }),
            "\u{1b}[38;2;250;189;47m"
        );
        assert_eq!(
            Stylize::bg(&Color::Rgb {
                r: 45,
                g: 45,
                b: 45
            }),
            "\u{1b}[48;2;45;45;45m"
        );
        assert_eq!(Stylize::fg(&Color::Default), "");
        assert_eq!(Stylize::bg(&Color::Default), "");
    }

    #[test]
    fn rgb_to_hex_round_trips_rgb_colors() {
        assert_eq!(
            rgb_to_hex(&Color::Rgb {
                r: 250,
                g: 189,
                b: 47
            }),
            "#fabd2f".to_string()
        );
        assert_eq!(rgb_to_hex(&Color::Default), "".to_string());
    }

    // ---- property-based ----

    proptest! {
        /// Any valid 6-digit hex resolves to exactly one color, and resolving
        /// that color's own hex again is a fixed point (idempotent).
        #[test]
        fn hex_resolution_is_a_fixed_point(hex in "[0-9a-fA-F]{6}") {
            let vars: BTreeMap<String, String> = BTreeMap::new();
            let value = format!("#{hex}");
            let first = resolve_color(value.as_str(), &vars).expect("valid hex resolves");
            let again = resolve_color(rgb_to_hex(&first).as_str(), &vars).expect("resolves");
            prop_assert_eq!(first, again);
        }

        /// Alias maps are adversarial (self-loops, forward cycles, and
        /// acyclic hex bases) — resolution always terminates and, when it
        /// lands on a color, that color is a resolution fixed point.
        #[test]
        fn var_alias_resolution_never_loops(chain in "[0-2]{0,8}") {
            let mut vars: BTreeMap<String, String> = BTreeMap::new();
            let n = chain.len();
            for (i, c) in chain.chars().enumerate() {
                let target = match c {
                    '0' => format!("$v{i}"), // self-loop
                    '1' => {
                        let target_index = (i + 1) % n;
                        format!("$v{target_index}") // forward cycle
                    },
                    _ => "#12ab34".to_string(), // acyclic hex base
                };
                vars.insert(format!("v{i}"), target);
            }
            if let Some(color) = resolve_color("$v0", &vars) {
                // Resolution landed on a base color — re-resolving its own
                // hex is a fixed point. (A cycle yields None and proves
                // termination without a color.)
                let hex = rgb_to_hex(&color);
                prop_assert_eq!(resolve_color(hex.as_str(), &vars), Some(color));
            }
        }
    }
}
