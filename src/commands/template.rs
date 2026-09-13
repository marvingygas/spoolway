//! `spoolway template contract`: the shapes a project writes prose into that
//! are neither a pipeline nor a prompt — a task's own body, a lane's seven
//! typed messages, the three headings appended to a running task, and the
//! pull request body `spoolway stack` opens.
//!
//! None of these are parsed the way a pipeline file is: every one is prose a
//! project owns outright, read back whole or substituted by name, never
//! validated against a schema. This command exists so an agent asked to
//! reshape one reaches for what actually reads it — [`crate::task_template`],
//! [`crate::lane_prompts`] and [`crate::task_log`] — rather than guessing at
//! a placeholder's spelling.

use super::*;

/// `spoolway template contract`.
pub fn template_contract(repo: &Repo) -> Result<()> {
    print!("{}", render_template_contract(repo));
    Ok(())
}

/// [`template_contract`]'s body, built as a string so a test can assert on
/// it directly rather than capturing stdout.
fn render_template_contract(repo: &Repo) -> String {
    let mut out = String::new();
    out.push_str("THE TEMPLATE CONTRACT\n");
    out.push_str("=====================\n\n");
    out.push_str(
        "Four shapes, none of them parsed the way a pipeline file is: each is prose a \
         project\nowns outright, read back whole or substituted by name — never validated \
         against a\nschema, never rewritten by `spoolway update` once it exists.\n\n",
    );

    out.push_str("TASK — .spoolway/templates/tasks/<pipeline>.md\n");
    out.push_str(&format!(
        "  Selected by pipeline name, `{}.md` when a pipeline has none of its own. \
         Nothing\n  in the body is ever parsed; the three headings a running task grows — \
         Status\n  Log, Handoff, Blocker — are appended by `spoolway report` if the skeleton \
         does\n  not already have them, so a skeleton may be a single `## Goal` and still \
         work.\n  `.spoolway/templates/tracking/epic.md` and `ticket.md` are the two ticket-\n  \
         body templates an issue-tracking hook's `open` event fills in, substituting every\n  \
         `${{SPOOLWAY_*}}` name the hook's own environment carries — see `spoolway hook \
         contract`.\n\n",
        crate::task_template::FALLBACK
    ));

    out.push_str("LANE-PROMPT — .spoolway/templates/lane-prompts.md\n");
    out.push_str("  One `## <state>` section per typed message a lane's pane receives:\n");
    for state in crate::lane_prompts::STATES {
        out.push_str(&format!("    {state}\n"));
    }
    out.push_str(
        "  A project silent about one state — the file absent, the section absent, or the\n  \
         section blank — gets spoolway's own built-in wording for it. Placeholders \
         substituted:\n",
    );
    for placeholder in crate::lane_prompts::PLACEHOLDERS {
        out.push_str(&format!("    {{{placeholder}}}\n"));
    }
    out.push_str(
        "  A `{...}` naming anything else is left exactly as written. `## arrived-by-fail`, \
         an\n  eighth section composed into the system prompt rather than typed into a pane, \
         takes\n  one placeholder of its own: `{from}`.\n\n",
    );

    out.push_str("TASK-LOG — .spoolway/templates/task-log.md\n");
    out.push_str(
        "  Plain prose under three headings, describing what belongs there rather than\n  \
         substituting anything — this is documentation sent to a lane's system prompt as\n  \
         WHAT YOU WRITE DOWN, never rendered or read back by spoolway itself:\n",
    );
    for (heading, _) in crate::task_log::HEADINGS {
        out.push_str(&format!("    {heading}\n"));
    }
    out.push_str(
        "  A project that has written the file but left a heading out of it says nothing \
         about\n  that heading at all — unlike the lane-prompt fallback above, leaving a \
         heading out\n  is not the same as the file being absent.\n\n",
    );

    out.push_str("PR — .spoolway/templates/pull-request.md\n");
    out.push_str(
        "  What `spoolway stack`'s optional summary turn fills in: the first line becomes \
         the\n  pull request's title, everything below it the body. No `${...}` \
         placeholder —\n  a project writes headings and instructions for the model, the same \
         way a task\n  skeleton does.\n\n",
    );

    out.push_str("This project's own files:\n");
    out.push_str(&format!(
        "  {}\n",
        relative(&repo.checkout, &repo.task_templates_dir())
    ));
    out.push_str(&format!(
        "  {}\n",
        relative(&repo.checkout, &repo.lane_prompts_path())
    ));
    out.push_str(&format!(
        "  {}\n",
        relative(&repo.checkout, &repo.task_log_path())
    ));
    out.push_str(&format!(
        "  {}\n",
        relative(&repo.checkout, &repo.pull_request_template_path())
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mockup's own promise: the task, lane-prompt, task-log and PR
    /// shapes, and no format copied in — only the paths this project's own
    /// templates already live at, and pointers at the modules that read
    /// them.
    #[test]
    fn template_contract_names_all_four_shapes() {
        let repo = crate::commands::testutil::fixture("template-contract");
        let text = render_template_contract(&repo);
        for fact in [
            "TASK —",
            "LANE-PROMPT —",
            "TASK-LOG —",
            "PR —",
            "opening",
            "task_file",
            "Status Log",
            "pull-request.md",
        ] {
            assert!(text.contains(fact), "template contract drops `{fact}`");
        }
    }
}
