//! TODO.md parser (Contract 1) — a port of the TS supervisor's `todo.ts`.
//!
//! Reads the row contract from a `## Steps` table:
//!
//! ```text
//! | # | Commit message | Logical unit | Key deliverables | Tests |
//! | --- | --- | --- | --- | --- |
//! | 1 | `feat: ...` | ... | ... | Unit |
//! ```
//!
//! Rows carry no progress annotation — the parser only extracts structure.
//! Also extracts the `Source:` line and the `## Prerequisites` block.

/// Sentinel for a row whose id carries no digits at all. Sorts last so the
/// supervisor never silently skips it; reports still show the raw id.
/// (Node's `Number.MAX_SAFE_INTEGER` is 2^53−1; a larger sentinel sorts last
/// all the same.)
pub const UNNUMBERED_ROW_NUMBER: u64 = u64::MAX;

/// One row of the `## Steps` table in TODO.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoRow {
    /// Row id cell as written (`1`, `step38`, ...). Kept raw so reports can
    /// echo the plan's own spelling.
    pub id: String,
    /// Leading digit run extracted from `id` per Contract 1.
    pub number: u64,
    /// The planned commit message (backticks stripped, raw otherwise).
    pub commit_message: String,
    /// Logical unit column.
    pub logical_unit: String,
    /// Key deliverables column.
    pub deliverables: String,
    /// Tests column.
    pub tests: String,
}

/// The parsed shape of a TODO.md plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoPlan {
    /// Value of the `Source:` line, if present.
    pub source: Option<String>,
    /// Lines of the `## Prerequisites` block, if present.
    pub prerequisites: Vec<String>,
    /// Rows in document order.
    pub rows: Vec<TodoRow>,
    /// True when the document carries an explicit `## Done` marker.
    pub done_marked: bool,
}

/// Row-completion predicate; callers close over git state.
pub type RowDone<'a> = dyn Fn(&TodoRow) -> bool + 'a;

/// Split a table row line into cells, honoring `\|` escapes.
pub fn split_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let mut cells: Vec<String> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = trimmed.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && chars.get(i + 1) == Some(&'|') {
            // Escaped pipe: literal character, not a column separator.
            current.push('|');
            i += 2;
            continue;
        }
        if ch == '|' {
            cells.push(current.trim().to_string());
            current.clear();
        } else {
            current.push(ch);
        }
        i += 1;
    }
    cells.push(current.trim().to_string());
    // Outer pipes produce empty first/last fragments — drop them.
    if cells.len() > 1 && cells[0].is_empty() {
        cells.remove(0);
    }
    if cells.len() > 1 && cells.last().is_some_and(|c| c.is_empty()) {
        cells.pop();
    }
    cells
}

/// True when a line is a table separator row (`| --- | --- | ... |`).
pub fn is_separator_row(line: &str) -> bool {
    let cells = split_row(line);
    !cells.is_empty()
        && cells
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-'))
}

/// True when a row looks like the canonical table header.
pub fn is_header_row(line: &str) -> bool {
    split_row(line)
        .iter()
        .any(|c| c.to_lowercase() == "commit message")
}

/// Strip one level of surrounding backticks from a cell.
fn strip_wrapping_backticks(cell: &str) -> String {
    let t = cell.trim();
    if t.len() >= 2 && t.starts_with('`') && t.ends_with('`') {
        return t[1..t.len() - 1].trim().to_string();
    }
    t.to_string()
}

/// Parse a single data row into a `TodoRow` (positional column mapping).
pub fn parse_row(line: &str) -> TodoRow {
    let cells = split_row(line);
    let get = |i: usize| cells.get(i).map(|s| s.as_str()).unwrap_or("");
    let id = get(0).trim().to_string();
    TodoRow {
        number: extract_row_number(&id),
        commit_message: strip_wrapping_backticks(get(1)),
        logical_unit: get(2).trim().to_string(),
        deliverables: get(3).trim().to_string(),
        tests: get(4).trim().to_string(),
        id,
    }
}

/// Extract the row number from its id cell (Contract 1).
///
/// Equivalent to the TS regex `(?:^|[^0-9])([0-9]+)` executed against the id:
/// the first digit run after a non-digit boundary (or at the string start)
/// is the row number. `1` → 1, `step38` → 38, `row-7` → 7, `wee` → sentinel.
pub fn extract_row_number(id: &str) -> u64 {
    let Some(first_digit) = id.find(|c: char| c.is_ascii_digit()) else {
        return UNNUMBERED_ROW_NUMBER;
    };
    let run_end = id[first_digit..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|len| first_digit + len)
        .unwrap_or(id.len());
    id[first_digit..run_end]
        .parse::<u64>()
        .unwrap_or(UNNUMBERED_ROW_NUMBER)
}

/// Split a document into lines, tolerating CRLF and trailing whitespace.
fn split_lines(content: &str) -> Vec<String> {
    content
        .replace("\r\n", "\n")
        .split('\n')
        .map(str::trim_end)
        .map(String::from)
        .collect()
}

/// True for `#`, `##`, `###` headings (with a space after the markers).
fn is_heading(line: &str) -> bool {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    (1..=3).contains(&hashes) && trimmed[hashes..].starts_with(' ')
}

/// Collect the plain-text block under a `## heading` (until the next heading).
fn section_lines(lines: &[String], heading: &str) -> Vec<String> {
    let Some(start) = lines.iter().position(|l| l == heading) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for line in &lines[start + 1..] {
        if is_heading(line) {
            break;
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// Extract the `## Steps` table rows. The table header and separator are
/// skipped; rows map positionally. A missing header row is tolerated — the
/// first `|` line is then treated as a data row.
pub fn extract_rows(content: &str) -> Vec<TodoRow> {
    let lines = split_lines(content);
    let mut rows: Vec<TodoRow> = Vec::new();
    let mut in_steps = false;
    let mut seen_header = false;

    for line in &lines {
        if line == "## Steps" || line == "## Steps " {
            in_steps = true;
            continue;
        }
        if in_steps && is_heading(line) {
            break; // next heading ends the Steps section
        }
        if in_steps && line.starts_with('|') {
            if is_separator_row(line) {
                continue;
            }
            if !seen_header && is_header_row(line) {
                seen_header = true;
                continue;
            }
            rows.push(parse_row(line));
        }
    }
    rows
}

/// Parse a TODO.md document into a `TodoPlan` (Contract 1).
pub fn parse_plan(content: &str) -> TodoPlan {
    let lines = split_lines(content);

    let source = lines
        .iter()
        .find(|l| l.trim_start().starts_with("Source:"))
        .map(|l| {
            let rest = l.trim_start().strip_prefix("Source:").unwrap_or("").trim();
            strip_wrapping_backticks(rest)
        })
        .filter(|s| !s.is_empty());

    let prerequisites = section_lines(&lines, "## Prerequisites")
        .into_iter()
        .map(|l| l.trim_start_matches(['-', '*']).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let done_marked = lines.iter().any(|l| l == "## Done");

    TodoPlan {
        source,
        prerequisites,
        rows: extract_rows(content),
        done_marked,
    }
}

/// Next incomplete row: the lowest-numbered row that is not done, honoring
/// document order for ties. Returns its index into `rows`, or `None` when
/// every row is done.
pub fn next_row(rows: &[TodoRow], is_done: &RowDone) -> Option<usize> {
    let mut pending: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| !is_done(r))
        .map(|(i, _)| i)
        .collect();
    pending.sort_by_key(|&i| rows[i].number);
    pending.first().copied()
}

/// True when every row is done (or an explicit `## Done` marker exists).
pub fn all_done(rows: &[TodoRow], is_done: &RowDone, done_marked: bool) -> bool {
    done_marked || rows.iter().all(is_done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const FIXTURE: &str = include_str!("../test-fixtures/sample-todo.md");

    // ---- splitRow / helpers ----

    #[test]
    fn split_row_splits_canonical_rows_and_honors_escaped_pipes() {
        assert_eq!(split_row("| a | b | c |")[1], "b");
        assert_eq!(
            split_row("| 1 | `feat: a \\| b` | unit | d | t |"),
            ["1", "`feat: a | b`", "unit", "d", "t"]
        );
    }

    #[test]
    fn is_separator_row_and_is_header_row_classify_table_rows() {
        assert!(is_separator_row("| --- | --- | --- | --- | --- |"));
        assert!(!is_separator_row("| 1 | `x` | y | z | w |"));
        assert!(is_header_row(
            "| # | Commit message | Logical unit | Key deliverables | Tests |"
        ));
        assert!(!is_header_row("| 1 | `x` | y | z | w |"));
    }

    #[test]
    fn extract_row_number_handles_plain_and_prefixed_ids() {
        assert_eq!(extract_row_number("1"), 1);
        assert_eq!(extract_row_number("step38"), 38);
        assert_eq!(extract_row_number("38"), 38);
        assert_eq!(extract_row_number("row-7"), 7);
        assert_eq!(extract_row_number("wee"), UNNUMBERED_ROW_NUMBER);
    }

    #[test]
    fn parse_row_maps_columns_positionally_and_strips_wrapping_backticks() {
        let row =
            parse_row("| 3 | `feat: parse the table` | Parser | `src/todo.rs` | Unit + property |");
        assert_eq!(row.id, "3");
        assert_eq!(row.number, 3);
        assert_eq!(row.commit_message, "feat: parse the table");
        assert_eq!(row.logical_unit, "Parser");
        assert_eq!(row.deliverables, "`src/todo.rs`");
        assert_eq!(row.tests, "Unit + property");
    }

    // ---- extractRows / parsePlan against the committed fixture ----

    #[test]
    fn parses_the_canonical_sample_fixture_fully() {
        let plan = parse_plan(FIXTURE);

        assert_eq!(
            plan.source.as_deref(),
            Some("docs/research/plan-supervisor-extension.md")
        );
        assert!(plan.done_marked);

        assert!(plan.prerequisites.len() >= 2);
        assert!(
            plan.prerequisites
                .iter()
                .any(|l| l.contains("@gotgenes/pi-subagents"))
        );
        assert!(plan.prerequisites.iter().any(|l| l.contains("S0.5")));

        let ids: Vec<&str> = plan.rows.iter().map(|r| r.id.as_str()).collect();
        let numbers: Vec<u64> = plan.rows.iter().map(|r| r.number).collect();
        let msgs: Vec<&str> = plan
            .rows
            .iter()
            .map(|r| r.commit_message.as_str())
            .collect();
        assert_eq!(ids, ["1", "2", "step38", "99"]);
        assert_eq!(msgs[0], "chore: initialize pi_workflow repository");
        assert_eq!(
            msgs[1],
            "chore: scaffold supervisor extension and symlink it into pi"
        );
        assert_eq!(msgs[3], "docs: finish the thing");
        assert_eq!(numbers, [1, 2, 38, 99]);
    }

    #[test]
    fn escaped_pipe_in_a_commit_message_is_preserved() {
        let plan = parse_plan(FIXTURE);
        let step38 = plan
            .rows
            .iter()
            .find(|r| r.id == "step38")
            .expect("fixture contains step38");
        assert_eq!(
            step38.commit_message,
            "feat(api): parse rows with escaped | pipes"
        );
        assert_eq!(step38.number, 38);
    }

    #[test]
    fn extract_rows_handles_a_missing_table_header_row() {
        let content = [
            "## Steps",
            "| --- | --- | --- | --- | --- |",
            "| 1 | `chore: a` | unit | d | t |",
            "| 2 | `feat: b` | unit | d | t |",
        ]
        .join("\n");
        let rows = extract_rows(&content);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].commit_message, "chore: a");
    }

    #[test]
    fn extract_rows_is_empty_without_a_steps_section() {
        assert_eq!(extract_rows("# no steps here\n\nnothing\n").len(), 0);
        assert_eq!(extract_rows("").len(), 0);
    }

    #[test]
    fn next_row_honors_number_order_and_respects_done_rows() {
        let rows = extract_rows(
            &[
                "## Steps",
                "| # | Commit message | Logical unit | Key deliverables | Tests |",
                "| --- | --- | --- | --- | --- |",
                "| step3 | `c` | u | d | t |",
                "| 1 | `a` | u | d | t |",
                "| 2 | `b` | u | d | t |",
            ]
            .join("\n"),
        );

        let next = next_row(&rows, &|row: &TodoRow| row.number == 1).expect("a pending row");
        assert_eq!(rows[next].number, 2);
        assert_eq!(rows[next].id, "2");

        assert!(next_row(&rows, &|_| true).is_none());
    }

    #[test]
    fn all_done_is_false_until_every_row_is_done_and_honors_the_done_marker() {
        let rows = extract_rows(
            &[
                "## Steps",
                "| # | Commit message | Logical unit | Key deliverables | Tests |",
                "| --- | --- | --- | --- | --- |",
                "| 1 | `a` | u | d | t |",
                "| 2 | `b` | u | d | t |",
            ]
            .join("\n"),
        );

        assert!(!all_done(&rows, &|_| false, false));
        assert!(all_done(&rows, &|_| true, false));
        assert!(all_done(&rows, &|_| false, true)); // explicit ## Done marker wins
    }

    #[test]
    fn parser_tolerates_trailing_whitespace_and_crlf() {
        let lf = parse_plan(FIXTURE);
        let crlf = parse_plan(&FIXTURE.replace('\n', "\r\n"));
        let padded = parse_plan(
            &FIXTURE
                .lines()
                .map(|l| format!("{l}   "))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        assert_eq!(lf.rows.len(), crlf.rows.len());
        assert_eq!(lf.rows.len(), padded.rows.len());
        assert_eq!(crlf.rows[0].commit_message, lf.rows[0].commit_message);
        assert_eq!(padded.rows[0].commit_message, lf.rows[0].commit_message);
        assert_eq!(crlf.source, lf.source);
    }

    #[test]
    fn source_and_prerequisites_are_optional() {
        let plan = parse_plan(
            "## Steps\n| # | Commit message | Logical unit | Key deliverables | Tests |\n| --- | --- | --- | --- | --- |\n| 1 | `a` | u | d | t |\n",
        );
        assert_eq!(plan.source, None);
        assert!(plan.prerequisites.is_empty());
        assert_eq!(plan.rows.len(), 1);
    }

    // ---- property-based ----

    proptest! {
        /// Digit runs of any length yield either the parsed value or the
        /// unnumbered sentinel — never a panic or a wrong number.
        #[test]
        fn row_number_is_never_out_of_range(id in "[0-9]{0,40}") {
            let n = extract_row_number(&id);
            if n != UNNUMBERED_ROW_NUMBER {
                let expected: u64 = id.trim_start_matches('0').parse().unwrap_or(0);
                prop_assert_eq!(n, expected);
            }
        }
    }
}
