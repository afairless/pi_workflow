//! Contract 3 worker prompt builder — a port of the TS supervisor's
//! `prompt.ts`, adapted for the RPC worker: the `ask_parent` instruction is
//! replaced by the `PI_WORKER_STATUS: ASK` + `QUESTION:` marker contract.

use crate::todo::TodoRow;

/// Escape text so row content cannot leak markdown structure into the prompt.
/// Order matters: backslashes first, then backticks, then pipes.
pub fn escape_row_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '`' => out.push_str("\\`"),
            '|' => out.push_str("\\|"),
            other => out.push(other),
        }
    }
    out
}

/// One line of the row block shown to the worker. Empty deliverables/tests
/// lines are omitted.
pub fn format_row_block(row: &TodoRow) -> Vec<String> {
    let escaped = RowCellEscaped {
        commit_message: escape_row_text(&row.commit_message),
        logical_unit: escape_row_text(&row.logical_unit),
        deliverables: escape_row_text(&row.deliverables),
        tests: escape_row_text(&row.tests),
    };
    let mut lines = vec![
        format!("Row {} (step {}):", row.id, row.number),
        format!("  commit: {}", escaped.commit_message),
        format!("  unit:   {}", escaped.logical_unit),
    ];
    if !escaped.deliverables.is_empty() {
        lines.push(format!("  deliv:  {}", escaped.deliverables));
    }
    if !escaped.tests.is_empty() {
        lines.push(format!("  tests:  {}", escaped.tests));
    }
    lines
}

struct RowCellEscaped {
    commit_message: String,
    logical_unit: String,
    deliverables: String,
    tests: String,
}

/// Inputs to the worker prompt renderer.
pub struct PromptInputs<'a> {
    /// cwd where the worker will operate (= project root).
    pub cwd: &'a str,
    /// Value of the TODO.md `Source:` line, when present.
    pub plan_source: Option<&'a str>,
    /// The row the worker must implement.
    pub row: &'a TodoRow,
    /// The human's answer to a previous row question, when continuing one.
    pub answer: Option<&'a str>,
    /// Set when this spawn resumes an in-progress row on its dirty WIP.
    pub resume_dirty_wip: Option<ResumeDirtyWip<'a>>,
    /// The implement-from-plan skill body (frontmatter stripped), when the
    /// caller loaded it; absent keeps the terse fallback line.
    pub skill_body: Option<&'a str>,
}

/// Reference to the prior agent when resuming a dirty in-progress row.
pub struct ResumeDirtyWip<'a> {
    pub agent_id: Option<&'a str>,
}

/// A human answer carried across exactly one re-generated clean-worktree
/// agent: the question the paused agent asked (it rides along so an
/// unconsumable continuation can still be reported) and the answer itself,
/// which the next clean prompt folds in (Contract 4 answer parity).
pub struct CleanContinuation {
    pub question: String,
    pub answer: String,
}

/// Inputs to the clean-worktree agent prompt renderer.
pub struct CleanPromptInputs<'a> {
    /// cwd where the agent will operate (= project root).
    pub cwd: &'a str,
    /// Value of the TODO.md `Source:` line, when present.
    pub plan_source: Option<&'a str>,
    /// The row the supervisor will prepare next — context only: the clean
    /// agent must NOT implement it.
    pub row: &'a TodoRow,
    /// The clean-worktree skill body (frontmatter stripped): the operative
    /// instructions for this run. Absent keeps a terse fallback line.
    pub clean_body: Option<&'a str>,
    /// The implement-from-plan skill body (frontmatter stripped):
    /// reference context for reading the repository and TODO.md only.
    pub impl_body: Option<&'a str>,
    /// A carried answer to a previous clean question, folded in exactly
    /// once (one-continuation lifetime).
    pub continuation: Option<CleanContinuation>,
}

/// Strip the YAML frontmatter (the leading `---` block) from a skill
/// `SKILL.md`, returning the body after the closing delimiter verbatim.
/// The block must start at the very first byte — no BOM, no leading blank
/// lines — and must terminate; anything else returns `Err` so the caller
/// can fail fast instead of guessing where the instructions begin.
pub fn strip_skill_frontmatter(raw: &str) -> Result<String, String> {
    if raw.starts_with('\u{feff}') {
        return Err("skill file starts with a BOM; frontmatter unresolvable".to_string());
    }
    let lines: Vec<&str> = raw.lines().collect();
    if lines.is_empty() || lines[0].trim() != "---" {
        return Err("skill file does not begin with a --- frontmatter block".to_string());
    }
    let mut close_at: Option<usize> = None;
    let mut i = 1;
    while i < lines.len() {
        if lines[i].trim() == "---" {
            close_at = Some(i);
            break;
        }
        i += 1;
    }
    let Some(close_at) = close_at else {
        return Err("skill frontmatter block is unterminated (no closing ---)".to_string());
    };
    // The closing `---` line's newline leaves one leading blank line in the
    // body of the common skill layout (frontmatter, blank line, headings);
    // drop leading blank lines so the framed section renders cleanly.
    let mut body: String = lines[close_at + 1..].join("\n");
    while body.starts_with('\n') {
        body = body[1..].to_string();
    }
    if !body.is_empty() && raw.ends_with('\n') {
        body.push('\n');
    }
    Ok(body)
}

/// The frame announcing an injected skill body (commit 3/4 transition: the
/// section is emitted only when a body is present; `None` keeps the terse
/// fallback line byte-identical).
pub const SKILL_BODY_HEADER: &str =
    "The implement-from-plan skill has been loaded for you automatically; its";

/// Render the worker prompt for one row (Contract 3, RPC-adapted).
pub fn render_worker_prompt(inputs: PromptInputs<'_>) -> String {
    let mut lines: Vec<String> = vec![
        "You are a worker operating under a supervisor.".to_string(),
        String::new(),
        format!("Project: {}", inputs.cwd),
        format!("Plan source: {}", inputs.plan_source.unwrap_or("(none)")),
        format!("TODO.md: {}/TODO.md", inputs.cwd),
        String::new(),
        format!("Implement ONLY row {} of TODO.md:", inputs.row.id),
    ];
    lines.extend(format_row_block(inputs.row));
    lines.push(format!(
        "Do not start row {} if it exists. A supervisor coordinates the rest; stop when row {} is done.",
        inputs.row.number + 1,
        inputs.row.id
    ));
    lines.push(String::new());
    if let Some(body) = inputs.skill_body.filter(|b| !b.is_empty()) {
        // Framed section: the body is embedded verbatim (no escaping — it
        // is already plain markdown instructions), and the closing marker
        // re-arms the prompt boundary so the model cannot confuse the
        // injected skill with the orchestrator's own instructions below.
        let mut section = String::with_capacity(body.len() + 256);
        section.push_str(SKILL_BODY_HEADER);
        section.push_str("\ninstructions are included below in full. Follow them for this step\n");
        section.push_str("(incremental loop). Do not read the skill file again.\n\n");
        section.push_str(body);
        if !body.ends_with('\n') {
            section.push('\n');
        }
        section.push('\n');
        section.push_str("--- (end of the automatically loaded implement-from-plan skill)");
        lines.push(section);
    } else {
        lines.push(
            "Follow the implement-from-plan skill for this step (incremental loop).".to_string(),
        );
    }
    lines.push(format!(
        "Commit with exactly the plan's commit message for this row: {}",
        escape_row_text(&inputs.row.commit_message)
    ));
    lines.push(String::new());
    lines
        .push("If you are blocked and need a decision or information you do not have,".to_string());
    lines.push("end your final message with:".to_string());
    lines.push("  PI_WORKER_STATUS: ASK".to_string());
    lines.push("  QUESTION: <crisp question>".to_string());
    lines.push(
        "You will not be resumed after a question — the human's answer is folded".to_string(),
    );
    lines.push("into a fresh worker's prompt, and that worker continues the row.".to_string());

    if let Some(answer) = inputs.answer.filter(|a| !a.trim().is_empty()) {
        lines.push(String::new());
        lines.push("The human answered a previous worker's question for this row:".to_string());
        lines.push(String::new());
        lines.extend(format_answer_block(answer));
    }

    if let Some(flag) = inputs.resume_dirty_wip {
        let agent_note = match flag.agent_id {
            Some(id) => format!(" (agent {})", escape_row_text(id)),
            None => String::new(),
        };
        lines.push(String::new());
        lines.push(
            "The working tree already contains uncommitted changes from a previous".to_string(),
        );
        lines.push(format!(
            "worker for this row{agent_note}. Run `git status` before doing anything:"
        ));
        lines.push(
            "- fold the previous attempt's work that belongs to this row into your commit;"
                .to_string(),
        );
        lines.push("- revert or delete anything that does not belong to this row;".to_string());
        lines.push("- never commit unrelated strays into this row's commit.".to_string());
    }

    lines.push(String::new());
    lines.push(
        "End your final message with the line: PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>".to_string(),
    );

    lines.join("\n") + "\n"
}

/// Render the answered-question block indented so it reads as quotation.
fn format_answer_block(answer: &str) -> Vec<String> {
    answer
        .trim()
        .lines()
        .map(|l| format!("> {}", escape_row_text(l)))
        .collect()
}

/// Render the clean-worktree agent prompt (Change 4, pure): opens with the
/// authoritative **operative-line pin** — the clean-worktree skill body is
/// the agent's ONLY operating instruction for the run, and the
/// implement-from-plan body is reference context only ("do not follow it") —
/// then frames both bodies under distinct headers, carries the
/// `Project:`/`TODO.md:`/`Plan source:` orientation lines and the row-being-
/// prepared note with the explicit no-implementation instruction, states the
/// git-keyed success criterion (`git status --short` empty), and repeats the
/// `PI_WORKER_STATUS` marker contract. An optional carried continuation
/// folds the human's answered-question block in exactly like the row prompt
/// does. The gate decides whether a clean agent spawns at all.
pub fn render_clean_prompt(inputs: CleanPromptInputs<'_>) -> String {
    let mut lines: Vec<String> = vec![
        "You are a clean-worktree agent operating under a supervisor.".to_string(),
        String::new(),
        format!("Project: {}", inputs.cwd),
        format!("Plan source: {}", inputs.plan_source.unwrap_or("(none)")),
        format!("TODO.md: {}/TODO.md", inputs.cwd),
        String::new(),
        format!(
            "The supervisor is preparing row {} next (see TODO.md).",
            inputs.row.id
        ),
        "Do not implement the row — the next worker will do that. Only restore".to_string(),
        "a clean worktree.".to_string(),
        String::new(),
        // Operative-line pin: the shared row persona tells workers to step
        // through the plan incrementally; the pin is what keeps that persona
        // from pushing the clean agent into implementing the row. The
        // clean-worktree body is the run's only operating authority, the
        // implement-from-plan body is demoted to reference context.
        "Operative-line pin: the clean-worktree skill body below is your".to_string(),
        "ONLY operating instruction for this run. The implement-from-plan".to_string(),
        "body further below is reference context for reading the repository".to_string(),
        "and TODO.md — do not follow it.".to_string(),
        String::new(),
    ];
    lines.push("--- clean-worktree skill (operative instructions) ---".to_string());
    lines.push(String::new());
    let operative: &str = match inputs.clean_body.filter(|b| !b.is_empty()) {
        Some(body) => body,
        None => {
            "Follow the clean-worktree skill for this run: make `git status --short`\n\
print nothing without destroying meaningful work."
        }
    };
    lines.push(operative.to_string());
    if !operative.ends_with('\n') {
        lines.push(String::new());
    }
    lines.push("--- (end of clean-worktree skill) ---".to_string());
    if let Some(body) = inputs.impl_body.filter(|b| !b.is_empty()) {
        lines.push(String::new());
        lines.push("--- implement-from-plan skill (reference context only) ---".to_string());
        lines.push(String::new());
        lines.push(body.to_string());
        if !body.ends_with('\n') {
            lines.push(String::new());
        }
        lines.push("--- (end of implement-from-plan skill) ---".to_string());
    }
    lines.push(String::new());
    lines.push("Success criterion: before reporting COMPLETE, `git status --short`".to_string());
    lines.push("must print nothing, and `git diff` / `git diff --cached` must be".to_string());
    lines.push("clean — verify it yourself first.".to_string());
    lines.push(String::new());
    lines
        .push("If you are blocked and need a decision or information you do not have,".to_string());
    lines.push("end your final message with:".to_string());
    lines.push("  PI_WORKER_STATUS: ASK".to_string());
    lines.push("  QUESTION: <crisp question>".to_string());
    lines.push("You will not be resumed after a question — the supervisor pauses".to_string());
    lines.push("for the human's answer and re-runs you with it folded in.".to_string());

    if let Some(c) = inputs.continuation {
        lines.push(String::new());
        lines.push("The human answered a previous clean-worktree agent's question:".to_string());
        lines.push(String::new());
        lines.extend(format_answer_block(c.answer.as_str()));
    }

    lines.push(String::new());
    lines.push(
        "End your final message with the line: PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>".to_string(),
    );

    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(overrides: Option<&dyn Fn(&mut TodoRow)>) -> TodoRow {
        let mut r = TodoRow {
            id: "3".to_string(),
            number: 3,
            commit_message: "feat: add parser".to_string(),
            logical_unit: "Parser".to_string(),
            deliverables: "`src/todo.rs`".to_string(),
            tests: "Unit + property".to_string(),
        };
        if let Some(f) = overrides {
            f(&mut r);
        }
        r
    }

    #[test]
    fn template_includes_project_plan_source_todo_path_and_row_block() {
        let prompt = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: Some("docs/research/plan.md"),
            row: &row(None),
            answer: None,
            resume_dirty_wip: None,
            skill_body: None,
        });
        assert!(prompt.contains("Project: /repo"));
        assert!(prompt.contains("Plan source: docs/research/plan.md"));
        assert!(prompt.contains("TODO.md: /repo/TODO.md"));
        assert!(prompt.contains("Implement ONLY row 3 of TODO.md:"));
        assert!(prompt.contains("commit: feat: add parser"));
        assert!(prompt.contains("Do not start row 4 if it exists."));
        assert!(prompt.contains("PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>"));
    }

    #[test]
    fn row_with_a_stepnn_id_uses_the_raw_id_and_step_number() {
        let prompt = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: None,
            row: &row(Some(&|r| {
                r.id = "step38".to_string();
                r.number = 38;
                r.commit_message = "feat: zap".to_string();
            })),
            answer: None,
            resume_dirty_wip: None,
            skill_body: None,
        });
        assert!(prompt.contains("Implement ONLY row step38 of TODO.md:"));
        assert!(prompt.contains("Row step38 (step 38):"));
        assert!(prompt.contains("Do not start row 39 if it exists."));
        assert!(prompt.contains("Plan source: (none)"));
    }

    #[test]
    fn ask_contract_instruction_is_present() {
        let prompt = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: None,
            row: &row(None),
            answer: None,
            resume_dirty_wip: None,
            skill_body: None,
        });
        assert!(prompt.contains("PI_WORKER_STATUS: ASK"));
        assert!(prompt.contains("QUESTION: <crisp question>"));
        assert!(prompt.contains("the human's answer is folded"));
    }

    #[test]
    fn answered_question_block_is_folded_in_when_present() {
        let prompt = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: None,
            row: &row(None),
            answer: Some("Use the polars API, not pandas."),
            resume_dirty_wip: None,
            skill_body: None,
        });
        assert!(prompt.contains("The human answered a previous worker's question for this row:"));
        assert!(prompt.contains("> Use the polars API, not pandas."));
    }

    #[test]
    fn no_answer_block_when_answer_is_absent_or_blank() {
        let none = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: None,
            row: &row(None),
            answer: None,
            resume_dirty_wip: None,
            skill_body: None,
        });
        let blank = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: None,
            row: &row(None),
            answer: Some("   "),
            resume_dirty_wip: None,
            skill_body: None,
        });
        assert!(!none.contains("answered a previous worker's question"));
        assert!(!blank.contains("answered a previous worker's question"));
    }

    #[test]
    fn row_text_is_escaped_against_backticks_and_pipes() {
        assert_eq!(escape_row_text("`a|b`"), "\\`a\\|b\\`");
        assert_eq!(escape_row_text("no-op"), "no-op");
        assert_eq!(escape_row_text("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn shadowed_row_content_cannot_break_the_structure() {
        let shadowed = row(Some(&|r| {
            r.id = "1".to_string();
            r.commit_message = "feat: close | code fence `".to_string();
            r.deliverables = "`README.md` | `x`".to_string();
        }));
        let prompt = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: Some("p.md"),
            row: &shadowed,
            answer: Some("a|b"),
            resume_dirty_wip: None,
            skill_body: None,
        });
        // The row's pipe cannot terminate the commit line early.
        assert!(prompt.contains("commit: feat: close \\| code fence \\`"));
        assert!(prompt.contains("deliv:  \\`README.md\\` \\| \\`x\\`"));
        // The answer's pipe is quoted, not structural.
        assert!(prompt.contains("> a\\|b"));
    }

    #[test]
    fn format_row_block_omits_empty_deliverables_tests_lines() {
        let lines = format_row_block(&row(Some(&|r| {
            r.deliverables = String::new();
            r.tests = String::new();
        })));
        assert!(lines.iter().any(|l| l.contains("commit: feat: add parser")));
        assert!(!lines.iter().any(|l| l.contains("deliv:")));
        assert!(!lines.iter().any(|l| l.contains("tests:")));
    }

    #[test]
    fn resume_dirty_wip_block_is_absent_when_not_flagged() {
        let base = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: Some("p.md"),
            row: &row(None),
            answer: None,
            resume_dirty_wip: None,
            skill_body: None,
        });
        assert!(!base.contains("uncommitted changes from a previous worker"));
    }

    #[test]
    fn resume_dirty_wip_block_names_the_prior_agent_when_flagged() {
        let prompt = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: None,
            row: &row(None),
            answer: None,
            resume_dirty_wip: Some(ResumeDirtyWip {
                agent_id: Some("fake-7"),
            }),
            skill_body: None,
        });
        assert!(
            prompt
                .contains("The working tree already contains uncommitted changes from a previous")
        );
        assert!(prompt.contains(
            "worker for this row (agent fake-7). Run `git status` before doing anything"
        ));
        assert!(prompt.contains("never commit unrelated strays into this row's commit."));
    }

    #[test]
    fn resume_dirty_wip_agent_id_is_escaped_like_other_dynamic_text() {
        let prompt = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: None,
            row: &row(None),
            answer: None,
            resume_dirty_wip: Some(ResumeDirtyWip {
                agent_id: Some("a`|b"),
            }),
            skill_body: None,
        });
        assert!(
            prompt.contains("agent a\\`\\|b"),
            "agent id is escaped like other dynamic text"
        );
        assert!(
            !prompt.contains("(agent a`|b)"),
            "no unescaped agent id leaks"
        );
    }

    /// A realistic body after frontmatter stripping, ending with a newline.
    fn sample_skill_body() -> String {
        "## Purpose\n\nFollow the implement-from-plan skill for this step\n".to_string()
    }

    #[test]
    fn strip_skill_frontmatter_removes_the_leading_dash_block() {
        let raw = "---\nname: implement-from-plan\ndescription: x\nallowed-tools: []\n---\n\n## Purpose\n\nDo the thing.\n";
        let body = strip_skill_frontmatter(raw).expect("frontmatter resolves");
        assert_eq!(body, "## Purpose\n\nDo the thing.\n");
        assert!(!body.contains("name:"));
        assert!(!body.contains("allowed-tools"));
    }

    #[test]
    fn strip_skill_frontmatter_keeps_a_body_without_a_trailing_newline() {
        let raw = "---\nname: implement-from-plan\n---\n## Purpose";
        let body = strip_skill_frontmatter(raw).expect("frontmatter resolves");
        assert_eq!(body, "## Purpose");
    }

    #[test]
    fn strip_skill_frontmatter_fails_fast_on_malformed_blocks() {
        // No leading --- block: the raw text is instructions straight away.
        let no_block = strip_skill_frontmatter("## Purpose\n\nno frontmatter here\n");
        assert!(no_block.is_err());
        // Uninterminated block.
        let unterminated = strip_skill_frontmatter("---\nname: implement-from-plan\n\n## Purpose");
        assert!(unterminated.is_err());
        // BOM before the opening delimiter.
        let bom =
            strip_skill_frontmatter("\u{feff}---\nname: implement-from-plan\n---\n## Purpose");
        assert!(bom.is_err());
    }

    #[test]
    fn missing_skill_body_keeps_the_terse_line_byte_identical() {
        let prompt = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: None,
            row: &row(None),
            answer: None,
            resume_dirty_wip: None,
            skill_body: None,
        });
        assert!(
            prompt
                .contains("Follow the implement-from-plan skill for this step (incremental loop).")
        );
        assert!(!prompt.contains("has been loaded for you automatically"));
        assert!(!prompt.contains("--- (end of the automatically loaded"));
    }

    #[test]
    fn skill_body_renders_the_framed_section_instead_of_the_terse_line() {
        let body = sample_skill_body();
        let prompt = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: None,
            row: &row(None),
            answer: None,
            resume_dirty_wip: None,
            skill_body: Some(body.as_str()),
        });
        assert!(
            prompt.contains(
                "The implement-from-plan skill has been loaded for you automatically; its"
            )
        );
        assert!(
            prompt.contains("instructions are included below in full. Follow them for this step")
        );
        assert!(prompt.contains("(incremental loop). Do not read the skill file again."));
        assert!(prompt.contains("--- (end of the automatically loaded implement-from-plan skill)"));
        assert!(
            !prompt
                .contains("Follow the implement-from-plan skill for this step (incremental loop).")
        );
    }

    #[test]
    fn skill_body_is_embedded_verbatim_without_fence_or_escape_corruption() {
        // The body may legally contain backticks, pipes, and dash rows;
        // injection must not escape or re-frame them.
        let body = "## Procedure\n\nRun ```bash cargo test```; pass `a|b` through.\n\n---\n\nKeep going.\n";
        let prompt = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: None,
            row: &row(None),
            answer: None,
            resume_dirty_wip: None,
            skill_body: Some(body),
        });
        assert!(prompt.contains(body));
        assert!(prompt.contains("Run ```bash cargo test```; pass `a|b` through."));
    }

    #[test]
    fn skill_body_framing_leaves_row_ask_and_dirty_wip_blocks_unchanged() {
        let prompt = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: Some("docs/research/plan.md"),
            row: &row(Some(&|r| {
                r.commit_message = "feat: zap".to_string();
            })),
            answer: Some("Use the polars API, not pandas."),
            resume_dirty_wip: Some(ResumeDirtyWip {
                agent_id: Some("fake-7"),
            }),
            skill_body: Some(sample_skill_body().as_str()),
        });
        assert!(
            prompt
                .contains("Commit with exactly the plan's commit message for this row: feat: zap")
        );
        assert!(prompt.contains("PI_WORKER_STATUS: ASK"));
        assert!(prompt.contains("The human answered a previous worker's question for this row:"));
        assert!(
            prompt
                .contains("The working tree already contains uncommitted changes from a previous")
        );
        assert!(prompt.contains("PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>"));
    }

    #[test]
    fn empty_skill_body_is_treated_as_absent() {
        let prompt = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: None,
            row: &row(None),
            answer: None,
            resume_dirty_wip: None,
            skill_body: Some(""),
        });
        assert!(
            prompt
                .contains("Follow the implement-from-plan skill for this step (incremental loop).")
        );
        assert!(!prompt.contains("has been loaded for you automatically"));
    }

    // ---- clean-worktree agent prompt (Change 4) ----

    fn clean_inputs<'a>(
        row: &'a TodoRow,
        overrides: Option<&dyn Fn(&mut CleanPromptInputs<'_>)>,
    ) -> CleanPromptInputs<'a> {
        let mut i = CleanPromptInputs {
            cwd: "/repo",
            plan_source: None,
            row,
            clean_body: Some("## Goal\nMake `git status --short` empty again.\n"),
            impl_body: Some("# Implement from Plan\n## Purpose\nReference body.\n"),
            continuation: None,
        };
        if let Some(f) = overrides {
            f(&mut i);
        }
        i
    }

    #[test]
    fn clean_prompt_frames_both_bodies_with_distinct_headers() {
        let prompt = render_clean_prompt(clean_inputs(&row(None), None));
        assert!(prompt.contains("--- clean-worktree skill (operative instructions) ---"));
        assert!(prompt.contains("--- (end of clean-worktree skill) ---"));
        assert!(prompt.contains("-- (end of implement-from-plan skill) ---"));
        assert!(prompt.contains("--- implement-from-plan skill (reference context only) ---"));
        assert!(prompt.contains("## Goal\nMake `git status --short` empty again."));
        assert!(prompt.contains("# Implement from Plan"));
    }

    #[test]
    fn clean_prompt_carries_the_operative_line_pin() {
        let prompt = render_clean_prompt(clean_inputs(&row(None), None));
        assert!(prompt.contains("Operative-line pin"));
        assert!(prompt.contains("ONLY operating instruction for this run"));
        assert!(prompt.contains("do not follow it"));
    }

    #[test]
    fn clean_prompt_names_the_row_without_implementing_it() {
        let prompt = render_clean_prompt(clean_inputs(&row(None), None));
        assert!(prompt.contains("The supervisor is preparing row 3 next (see TODO.md)."));
        assert!(
            prompt
                .contains("Do not implement the row — the next worker will do that. Only restore")
        );
    }

    #[test]
    fn clean_prompt_states_the_git_keyed_success_criterion_and_marker_contract() {
        let prompt = render_clean_prompt(clean_inputs(&row(None), None));
        assert!(prompt.contains("`git status --short`"));
        assert!(prompt.contains("must print nothing"));
        assert!(prompt.contains("`git diff` / `git diff --cached` must be"));
        assert!(prompt.contains("PI_WORKER_STATUS: <COMPLETE|STUCK|ASK>"));
        assert!(prompt.contains("QUESTION: <crisp question>"));
    }

    #[test]
    fn clean_prompt_folds_the_answered_question_block_only_when_carried() {
        let without = render_clean_prompt(clean_inputs(&row(None), None));
        assert!(!without.contains("answered a previous clean-worktree agent's question"));
        let with_answer = render_clean_prompt(clean_inputs(
            &row(None),
            Some(&|i| {
                i.continuation = Some(CleanContinuation {
                    question: "may I discard target/?".to_string(),
                    answer: "yes — add target/ to .gitignore".to_string(),
                });
            }),
        ));
        assert!(
            with_answer.contains("The human answered a previous clean-worktree agent's question:")
        );
        assert!(with_answer.contains("> yes — add target/ to .gitignore"));
    }

    #[test]
    fn clean_prompt_falls_back_to_a_terse_line_when_the_clean_body_is_missing() {
        let prompt = render_clean_prompt(clean_inputs(
            &row(None),
            Some(&|i| {
                i.clean_body = None;
            }),
        ));
        assert!(prompt.contains("Follow the clean-worktree skill for this run"));
        assert!(prompt.contains("print nothing without destroying meaningful work"));
        assert!(!prompt.contains("## Goal"));
    }

    #[test]
    fn clean_prompt_embeds_bodies_verbatim_without_escape_corruption() {
        let prompt = render_clean_prompt(clean_inputs(
            &row(None),
            Some(&|i| {
                i.clean_body = Some("## Goal\n| pipe | \\ backtick `tick`\nline two\n");
            }),
        ));
        assert!(prompt.contains("| pipe | \\ backtick `tick`"));
        assert!(prompt.contains("## Goal"));
        assert!(prompt.contains("line two"));
    }
}
