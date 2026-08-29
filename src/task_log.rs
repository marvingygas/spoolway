//! What belongs under each of the three headings spoolway appends to a task
//! file — `## Status Log`, `## Handoff` and `## Blocker` — described in
//! plain prose a project can rewrite in `.spoolway/templates/task-log.md`.
//!
//! This is documentation, not a format string: the resolved prose is sent to
//! a lane as the `WHAT YOU WRITE DOWN` block of its system prompt — see
//! [`crate::compose`] — and spoolway never renders it and never reads it
//! back. What actually lands under a heading is whatever the lane's own
//! `spoolway report` writes, verbatim.
//!
//! The three headings are spoolway's own and are never read from this file —
//! see [`HEADINGS`] — only the prose under each is the project's.
//!
//! Resolution is deliberately not the per-section fallback
//! [`crate::lane_prompts::render`] gives the six typed messages: there, a
//! project silent about one state still gets spoolway's built-in wording for
//! it, because a lane launched with nothing to say would be a bug. Here, a
//! project that has written the file but left a heading out of it is telling
//! a lane nothing about that heading at all — see [`resolve`] — and the
//! built-in only answers when the file itself is absent, the same way a
//! project that has never customised anything sees spoolway's own words.
//!
//! `init` writes the whole shipped file; `spoolway update` never touches it
//! once it exists, the same rule [`crate::lane_prompts`] already keeps;
//! `spoolway update --replace .spoolway/templates/task-log.md` is the
//! deliberate way to take the shipped wording back.

use crate::repo::Repo;

/// The three headings spoolway appends to a task file, each paired with the
/// built-in prose [`resolve`] falls back to when the project has written no
/// `.spoolway/templates/task-log.md` at all. Order matters: this is also the
/// order `WHAT YOU WRITE DOWN` lists them in.
pub const HEADINGS: &[(&str, &str)] = &[
    ("Status Log", STATUS_LOG),
    ("Handoff", HANDOFF),
    ("Blocker", BLOCKER),
];

const STATUS_LOG: &str = "One line per transition, in behaviour rather than in file names. \
    Somebody scanning the board reads this and nothing else, so write it for them.";

const HANDOFF: &str = "Anything the next step needs and the diff does not show: what you \
    tried and abandoned, a call site you found, a constraint that made the obvious approach \
    wrong. One entry per thing.";

const BLOCKER: &str =
    "What is in the way, what you tried, and what somebody would have to do to get past it.";

/// Where a project overrides this prose, relative to its checkout.
pub fn path(repo: &Repo) -> std::path::PathBuf {
    repo.task_log_path()
}

/// The prose for one heading — the project's own `## <heading>` section when
/// `.spoolway/templates/task-log.md` exists and names it, `builtin` when the
/// file does not exist at all, and nothing when the file exists but leaves
/// this heading out or blank.
///
/// The last case is the one that differs from every other resolution chain
/// in this codebase: a project that has written the file has said what it
/// has to say, and a heading it left silent about gets no row in `WHAT YOU
/// WRITE DOWN` at all rather than spoolway's own words standing in
/// unannounced.
pub fn resolve(repo: &Repo, heading: &str, builtin: &str) -> Option<String> {
    match std::fs::read_to_string(path(repo)) {
        Ok(text) => {
            crate::lane_prompts::section(&text, heading).filter(|body| !body.trim().is_empty())
        }
        Err(_) => Some(builtin.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with(root: &std::path::Path, task_log: &str) -> Repo {
        let dir = root.join(".spoolway/templates");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("task-log.md"), task_log).unwrap();
        Repo {
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: crate::config::Config::default(),
            home: root.join(".home"),
        }
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        let root = crate::scratch::root(&format!("task-log-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A missing file falls back to the built-in wording for every heading.
    #[test]
    fn a_missing_file_falls_back_to_the_builtin_for_every_heading() {
        let root = fixture("missing-file");
        let repo = Repo {
            checkout: root.clone(),
            root: root.clone(),
            config: crate::config::Config::default(),
            home: root.join(".home"),
        };
        for (heading, builtin) in HEADINGS {
            assert_eq!(resolve(&repo, heading, builtin), Some(builtin.to_string()));
        }
    }

    /// A project's own section wins over the built-in when the file names
    /// the heading and it is not blank.
    #[test]
    fn a_projects_own_section_wins_over_the_builtin() {
        let root = fixture("override");
        let repo = repo_with(&root, "## Status Log\n\nOur own words.\n");
        assert_eq!(
            resolve(&repo, "Status Log", "builtin"),
            Some("Our own words.".to_string())
        );
    }

    /// A file that exists but leaves a heading out, or leaves it blank,
    /// answers with nothing at all — not the built-in. An absent file is a
    /// different answer from an absent section.
    #[test]
    fn an_existing_file_that_omits_or_blanks_a_heading_answers_nothing() {
        let root = fixture("omitted");
        let omitted = repo_with(&root, "## Handoff\n\nsomething else\n");
        assert_eq!(resolve(&omitted, "Status Log", "builtin"), None);

        let blank = repo_with(&root, "## Status Log\n\n   \n\n## Handoff\nx\n");
        assert_eq!(resolve(&blank, "Status Log", "builtin"), None);
    }
}
