//! The seven typed messages a lane's pane receives across its life, and the
//! project's own say over them.
//!
//! A lane is sent two things: the composed system prompt — spoolway's
//! framing, the role's own prose, this pass's policy — and one message typed
//! into its pane. The system prompt stays spoolway's outright; this module is
//! the other half, and it is the project's to rewrite, one state at a time,
//! in `.spoolway/templates/lane-prompts.md`.
//!
//! Seven states, seven `##` sections: `opening`, `resume`, `resume-unattended`,
//! `carry`, `park`, `park-escalated`, `reminder` — see [`STATES`]. [`render`]
//! resolves one section at a time, the same fallback chain
//! [`crate::task_template::resolve`] uses for a task skeleton: the project's
//! own section wins when the file has it and it is not blank, spoolway's
//! built-in wording otherwise. A file missing entirely, a section the file
//! does not name, and a section left blank are the same answer — the
//! built-in — so a project can override one state and say nothing about the
//! other six.
//!
//! Four names are substituted for the seven states: `{task_file}`, `{step}`,
//! `{skills}`, `{report_contract}` — see [`PLACEHOLDERS`], the set
//! `spoolway template contract` prints alongside each state. A `{...}` naming
//! anything else is left exactly as written rather than rendered empty — a
//! project's own placeholder-shaped prose is not this module's to eat, and
//! blanking it silently is how a typo in a project's own template would go
//! unnoticed.
//!
//! [`render`] itself answers for more than the seven states: `## arrived-by-
//! fail`, an eighth section composed straight into the system prompt by
//! [`crate::compose::policy`] rather than typed into a pane, goes through it
//! too, substituting a fifth name — `{from}` — that names no state in
//! [`STATES`].
//!
//! `init` writes the whole shipped file; `spoolway update` never touches it
//! once it exists, the same rule a prompt already keeps;
//! `spoolway update --replace .spoolway/templates/lane-prompts.md` is the
//! deliberate way to take the shipped wording back.

use crate::repo::Repo;

/// Every state a lane's pane is prompted in, in the order a lane can reach
/// them. `reminder` is the odd one out — not a launch at all, but the nudge
/// sent to a lane that has gone quiet — kept in this same set because it is
/// the seventh typed message spoolway ever sends, and a project rewriting
/// the other six has just as much reason to rewrite this one.
pub const STATES: &[&str] = &[
    "opening",
    "resume",
    "resume-unattended",
    "carry",
    "park",
    "park-escalated",
    "reminder",
];

/// Every placeholder [`render`] substitutes — what `spoolway template
/// contract` prints against `LANE-PROMPT`, and the only reader left for this
/// set now that nothing checks a project's own section against it.
pub(crate) const PLACEHOLDERS: &[&str] = &["task_file", "step", "skills", "report_contract"];

/// Where a project overrides these seven messages, relative to its checkout.
pub fn path(repo: &Repo) -> std::path::PathBuf {
    repo.lane_prompts_path()
}

/// Resolve one state's template — the project's own `## <state>` section, or
/// `builtin` when the file is absent, the section is absent, or the section
/// is whitespace-only — and render it against `values`.
pub fn render(repo: &Repo, state: &str, builtin: &str, values: &[(&str, &str)]) -> String {
    let template = project_section(repo, state).unwrap_or_else(|| builtin.to_string());
    substitute(&template, values).trim().to_string()
}

/// The project's own `## <state>` section, if the file exists, names the
/// section, and the section is not blank.
fn project_section(repo: &Repo, state: &str) -> Option<String> {
    let text = std::fs::read_to_string(path(repo)).ok()?;
    section(&text, state).filter(|body| !body.trim().is_empty())
}

/// The body of one `## <heading>` section: every line from just after the
/// heading up to the next `## ` heading or the end of the file, trimmed.
/// `None` when the heading itself is not there.
///
/// Line-scoped rather than a markdown parser, the same trade
/// [`crate::task::Task::section`] already makes for a task file's own
/// headings — this file is never nested and never carries a heading spoolway
/// needs to tell apart from a state's own prose.
///
/// `pub(crate)` rather than private: [`crate::task_log`] parses the same
/// shape of file — one `##` section per heading — and reads this rather than
/// growing its own copy of a scan this module already got right.
pub(crate) fn section(text: &str, heading: &str) -> Option<String> {
    let wanted = format!("## {heading}");
    let mut lines = text.lines();
    lines.by_ref().find(|line| line.trim() == wanted)?;

    let mut body = String::new();
    for line in lines {
        if line.starts_with("## ") {
            break;
        }
        body.push_str(line);
        body.push('\n');
    }
    Some(body.trim().to_string())
}

/// Replace every `{name}` in `template` that `values` names. A `{...}`
/// naming anything else, or an unclosed `{`, is copied through unchanged —
/// this never renders a placeholder empty, only ever a known one filled in.
fn substitute(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        match tail.find('}') {
            Some(end) => {
                let name = &tail[1..end];
                match values.iter().find(|(known, _)| *known == name) {
                    Some((_, value)) => out.push_str(value),
                    None => out.push_str(&tail[..=end]),
                }
                rest = &tail[end + 1..];
            }
            // An unclosed `{` at the end of the template is not a
            // placeholder to resolve — left verbatim rather than swallowed,
            // the same rule `task_template::render_tracking` follows for
            // `${`.
            None => {
                out.push_str(tail);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with(root: &std::path::Path, lane_prompts: &str) -> Repo {
        let dir = root.join(".spoolway/templates");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lane-prompts.md"), lane_prompts).unwrap();
        Repo {
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: crate::config::Config::default(),
            home: root.join(".home"),
        }
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        let root = crate::scratch::root(&format!("lane-prompts-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A project's own section wins over the built-in when it names the
    /// state and is not blank.
    #[test]
    fn a_projects_own_section_wins_over_the_builtin() {
        let root = fixture("override");
        let repo = repo_with(&root, "## opening\n\nGo read {task_file} first.\n");
        let out = render(&repo, "opening", "the built-in", &[("task_file", "t.md")]);
        assert_eq!(out, "Go read t.md first.");
    }

    /// A file with no `.spoolway/templates/lane-prompts.md` at all, a file
    /// that names no section for this state, and a section left blank all
    /// fall back to the built-in — the same answer for all three.
    #[test]
    fn a_missing_file_an_absent_section_and_a_blank_section_all_fall_back() {
        let root = fixture("fallback");

        let no_file = Repo {
            checkout: root.clone(),
            root: root.clone(),
            config: crate::config::Config::default(),
            home: root.join(".home"),
        };
        assert_eq!(render(&no_file, "opening", "builtin", &[]), "builtin");

        let no_section = repo_with(&root, "## resume\n\nsomething else\n");
        assert_eq!(render(&no_section, "opening", "builtin", &[]), "builtin");

        let blank_section = repo_with(&root, "## opening\n\n   \n\n## resume\nx\n");
        assert_eq!(render(&blank_section, "opening", "builtin", &[]), "builtin");
    }

    /// The four real names are substituted; anything else shaped like `{...}`
    /// is left exactly as written, never rendered empty.
    #[test]
    fn known_names_are_substituted_and_an_unknown_one_is_left_verbatim() {
        let out = substitute(
            "`{step}` in {task_file}, but not {task_id}.",
            &[("step", "review"), ("task_file", "queue/demo.md")],
        );
        assert_eq!(out, "`review` in queue/demo.md, but not {task_id}.");
    }

    /// An unclosed `{` at the end of a template is left verbatim rather than
    /// swallowed.
    #[test]
    fn an_unclosed_brace_is_left_verbatim() {
        assert_eq!(substitute("read this {", &[]), "read this {");
    }
}
