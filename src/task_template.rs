//! The skeleton a task file is written from.
//!
//! A task file has two owners and they divide cleanly. The frontmatter is
//! spoolway's for the task's whole life — generated from [`crate::task::
//! Frontmatter`] and re-serialised on every save, so changing what a task
//! records is a struct edit and never a migration on disk. The body is written
//! once, at queue time, from a skeleton this project owns outright.
//!
//! Nothing in that body is ever parsed. No heading is required, none is
//! validated, and the three sections spoolway appends to a running task —
//! `## Status Log`, `## Handoff`, `## Blocker` — are created by
//! [`crate::task::Task::append_to_section`] if the skeleton does not have them.
//! A skeleton may be a single `## Goal` and everything still works.
//!
//! Which is why there is nothing of ours in the file and `spoolway update` never
//! touches one. `init` writes a skeleton where none exists; after that it is the
//! project's, exactly like a prompt. `spoolway update --replace <path>` is the
//! deliberate way to take a shipped one back.
//!
//! One skeleton per pipeline, selected by filename, the way a step's `prompt:`
//! selects a prompt: a task queued on pipeline `bugfix` is written from
//! `bugfix.md` if the project wrote one, and from `default.md` if it did not.

use crate::repo::Repo;

/// The pipeline name whose skeleton answers for any pipeline without one.
pub const FALLBACK: &str = "default";

/// The skeleton a task queued on `pipeline` is written from.
///
/// `<pipeline>.md`, then `default.md`, then the built-in — the same shape as
/// a step's `prompt:` fallback, and the same refusal to treat a missing file
/// as an error. A skeleton that is not there is a project that has not written
/// one, which is the normal state of most projects and no reason to stop a
/// queue.
pub fn resolve(repo: &Repo, pipeline: &str) -> String {
    let dir = repo.task_templates_dir();

    if let Some(contents) = read(&dir.join(format!("{pipeline}.md"))) {
        return contents;
    }
    if let Some(contents) = read(&dir.join(format!("{FALLBACK}.md"))) {
        return contents;
    }

    crate::assets::task_template(pipeline)
        .or_else(|| crate::assets::task_template(FALLBACK))
        .unwrap_or_default()
        .to_string()
}

/// A skeleton file's markdown, or nothing if it is absent or empty. An empty
/// file is a deleted one that somebody touched, and answering with it would
/// queue tasks with no body at all.
fn read(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .filter(|text| !text.trim().is_empty())
}

/// The `epic` or `ticket` template `queue add`'s open hook renders, `name`
/// being one of those two.
///
/// Unlike [`resolve`], this never falls back to a shipped default: `resolve`
/// exists because every task is queued on some pipeline, and a project that
/// never wrote that pipeline a skeleton still needs one — the built-in body
/// stands in. `assets/tracking/epic.md` and `ticket.md` are not that kind of
/// stand-in; they are the seed a project's own `.spoolway/templates/tracking/`
/// is meant to start from, written there by `spoolway init` — see
/// `tracking-scripts`, which wires that up — and a project that has not run
/// that yet, or has deleted the file, has written no ticket body of its own
/// at all. Substituting the shipped prose in that case would render a
/// project's tracker in spoolway's own words instead of saying nothing was
/// customised; a single line naming the task is the honest answer, plain
/// enough that nobody mistakes it for the project's own words.
pub fn resolve_tracking(repo: &Repo, name: &str) -> String {
    let dir = repo.tracking_templates_dir();
    if let Some(contents) = read(&dir.join(format!("{name}.md"))) {
        return contents;
    }
    "Opened automatically for task `${SPOOLWAY_TASK}`.\n".to_string()
}

/// Render a tracking template by substituting every `${SPOOLWAY_*}`
/// placeholder it names with the value `env` carries for it — the same map
/// the hook's own environment is built from, so a template can never ask for
/// something the hook's environment does not also answer. A name `env` does
/// not carry — `SPOOLWAY_EPIC` for a group of one, say — renders empty
/// rather than failing: a blank body is a project's problem to notice, not
/// spoolway's to refuse over.
pub fn render_tracking(text: &str, env: &std::collections::BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        match rest.find('}') {
            Some(end) => {
                out.push_str(env.get(&rest[..end]).map(String::as_str).unwrap_or(""));
                rest = &rest[end + 1..];
            }
            // An unclosed `${` at the end of the template is not a
            // placeholder to resolve — left verbatim rather than swallowed.
            None => {
                out.push_str("${");
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A skeleton is markdown and nothing else. A marker here would be copied
    /// into every task file queued from it, where a lane would read it as part
    /// of its own specification.
    #[test]
    fn no_shipped_skeleton_carries_a_marker() {
        for (name, skeleton) in crate::assets::TASK_TEMPLATES {
            assert!(
                !skeleton.contains("<!-- spoolway:"),
                "{name} carries a spoolway marker"
            );
        }
    }

    /// The built-in answers for a pipeline the project has written no file for,
    /// which is what makes adding a pipeline cost no configuration. And it
    /// answers with the markdown itself now, not a rendering of it — there is
    /// nothing left to render.
    // covers: pipeline.task_template — the skeleton a pipeline's tasks are written from, and the fallback when it names none
    #[test]
    fn a_pipeline_without_a_file_of_its_own_falls_back_to_the_builtin() {
        let root = crate::scratch::root("task-template-fallback");
        let _ = std::fs::remove_dir_all(&root);
        let config = crate::config::Config::default();
        std::fs::create_dir_all(root.join(crate::config::TASK_TEMPLATES_DIR)).unwrap();
        let home = root.join(".home");
        let repo = Repo {
            checkout: root.clone(),
            root,
            config,
            home,
        };

        let resolved = resolve(&repo, "hotfix");
        assert_eq!(resolved, crate::assets::task_template(FALLBACK).unwrap());
    }

    /// A project that has written its own `epic.md` or `ticket.md` wins —
    /// the same override `resolve` gives a task skeleton — and one it has
    /// not written renders a single line naming the task rather than the
    /// shipped `assets/tracking/<name>.md`: that file is the seed `spoolway
    /// init` writes into the project, never a runtime default this
    /// substitutes on its own.
    #[test]
    fn a_projects_own_tracking_template_wins_and_an_unwritten_one_is_a_single_line() {
        let root = crate::scratch::root("task-template-tracking-override");
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join(crate::config::TRACKING_TEMPLATES_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ticket.md"), "our own ticket body\n").unwrap();
        let repo = Repo {
            checkout: root.clone(),
            root,
            config: crate::config::Config::default(),
            home: dir.join(".home"),
        };

        assert_eq!(resolve_tracking(&repo, "ticket"), "our own ticket body\n");
        // `epic.md` was never written, so this is the single-line fallback —
        // never the shipped `assets/tracking/epic.md`, and never blank.
        let epic = resolve_tracking(&repo, "epic");
        assert_eq!(epic, "Opened automatically for task `${SPOOLWAY_TASK}`.\n");
        assert_ne!(epic, crate::assets::tracking_template("epic").unwrap());
    }

    /// The substitution `render_tracking` does: every `${SPOOLWAY_*}` the
    /// template names is replaced with `env`'s value for it, a name `env`
    /// does not carry renders empty, and plain text around the placeholders
    /// is left untouched.
    #[test]
    fn render_tracking_substitutes_known_names_and_blanks_unknown_ones() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("SPOOLWAY_TASK".to_string(), "scan-pending".to_string());

        let rendered = render_tracking(
            "task `${SPOOLWAY_TASK}`, blocked by: ${SPOOLWAY_DEPENDS_TICKETS}.",
            &env,
        );
        assert_eq!(rendered, "task `scan-pending`, blocked by: .");
    }
}
