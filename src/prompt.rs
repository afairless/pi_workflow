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
}

/// Reference to the prior agent when resuming a dirty in-progress row.
pub struct ResumeDirtyWip<'a> {
    pub agent_id: Option<&'a str>,
}

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
    lines
        .push("Follow the implement-from-plan skill for this step (incremental loop).".to_string());
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
        });
        let blank = render_worker_prompt(PromptInputs {
            cwd: "/repo",
            plan_source: None,
            row: &row(None),
            answer: Some("   "),
            resume_dirty_wip: None,
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
}
