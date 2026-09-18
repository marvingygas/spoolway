//! `spoolway template contract`: the one shape a project writes prose into
//! that is neither a pipeline nor a prompt — a task's own body.
//!
//! Not parsed the way a pipeline file is: prose a project owns outright,
//! read back whole or substituted by name, never validated against a
//! schema. This command exists so an agent asked to reshape it reaches for
//! what actually reads it — [`crate::task_template`] — rather than guessing
//! at a placeholder's spelling. The seven typed messages a lane's pane
//! receives used to be a second such shape, project-overridable through
//! `.spoolway/templates/lane-prompts.md`; they are spoolway's own now, fixed
//! wording with no template behind them, so this contract no longer names
//! them.

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
        "One shape, not parsed the way a pipeline file is: prose a project owns\noutright, \
         read back whole or substituted by name — never validated against a\nschema, never \
         rewritten by `spoolway sync` once it exists.\n\n",
    );

    out.push_str("TASK — .spoolway/templates/tasks/<pipeline>.md\n");
    out.push_str(&format!(
        "  Selected by pipeline name, `{}.md` when a pipeline has none of its own. \
         Nothing\n  in the body is ever parsed; the three headings a running task grows — Status \
         Log,\n  Handoff, Blocker — are appended by `spoolway report` if the skeleton does\n  not \
         already have them, so a skeleton may be a single `## Goal` and still \
         work.\n  `.spoolway/templates/tracking/epic.md` and `ticket.md` are the two ticket-\n  \
         body templates an issue-tracking hook's `open` event fills in, substituting every\n  \
         `${{SPOOLWAY_*}}` name the hook's own environment carries — see `spoolway hook \
         contract`.\n\n",
        crate::task_template::FALLBACK
    ));

    out.push_str("This project's own files:\n");
    out.push_str(&format!(
        "  {}\n",
        relative(&repo.checkout, &repo.task_templates_dir())
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mockup's own promise: the task shape and no format copied in —
    /// only the path this project's own template lives at. No `LANE-PROMPT —`
    /// section any more: the seven typed messages are spoolway's own, with no
    /// project template behind them. No `PR —` section either: nothing reads
    /// the pull request template any more, so it is not one of the shapes
    /// this contract names.
    #[test]
    fn template_contract_names_the_task_shape_and_no_longer_lane_prompt_or_pr() {
        let repo = crate::commands::testutil::fixture("template-contract");
        let text = render_template_contract(&repo);
        for fact in ["TASK —", "Status Log"] {
            assert!(text.contains(fact), "template contract drops `{fact}`");
        }
        for gone in ["LANE-PROMPT —", "PR —"] {
            assert!(
                !text.contains(gone),
                "template contract still names a shape nothing reads"
            );
        }
    }
}
