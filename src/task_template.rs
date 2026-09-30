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
//! Which is why there is nothing of ours in the file and `spoolway sync` never
//! touches one. `init` writes a skeleton where none exists; after that it is the
//! project's, exactly like a prompt. `spoolway sync --replace <path>` is the
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
/// `<pipeline>.md`, then `local/templates/tasks/<pipeline>.md` (repo mode
/// only, and only when `pipeline` is not itself a tracked one — see below),
/// then `default.md` and its own private fallback, then the built-in — the
/// same shape as a step's `prompt:` fallback, and the same refusal to treat
/// a missing file as an error. A skeleton that is not there is a project
/// that has not written one, which is the normal state of most projects and
/// no reason to stop a queue.
///
/// `local/templates/tasks/<name>.md` belongs to the private pipeline named
/// `<name>` — the skeleton `pipeline copy` writes beside a private pipeline
/// it names the same. A *tracked* pipeline with no skeleton of its own is
/// never one of those: it falls through to `default.md`, the tracked
/// project's own answer, never to a private file that merely happens to
/// share its name — nothing private ever replaces a tracked file, and
/// stepping in front of `default.md` here would be exactly that, however
/// different the two filenames are. Every private fallback below is tried
/// only once its tracked counterpart is confirmed absent, for the same
/// reason.
pub fn resolve(repo: &Repo, pipeline: &str) -> String {
    let dir = repo.task_templates_dir();

    if let Some(contents) = read(&dir.join(format!("{pipeline}.md"))) {
        return contents;
    }
    if !is_tracked_pipeline(repo, pipeline)
        && let Some(contents) = local(repo, pipeline)
    {
        return contents;
    }
    if let Some(contents) = read(&dir.join(format!("{FALLBACK}.md"))) {
        return contents;
    }
    if let Some(contents) = local(repo, FALLBACK) {
        return contents;
    }

    crate::assets::task_template(pipeline)
        .or_else(|| crate::assets::task_template(FALLBACK))
        .unwrap_or_default()
        .to_string()
}

/// Whether `pipeline` names a pipeline the tracked `.spoolway/pipelines/`
/// (or a home-mode workspace's own `config/pipelines/`) actually has a file
/// for — checked directly on disk rather than through a loaded
/// [`crate::pipeline::Pipelines`], since `resolve` is reached from places
/// (queuing a task, `pipeline contract`'s own sample) that have a pipeline
/// *name* in hand, not always a parsed set to ask. Both extensions
/// `crate::pipeline::read_pipeline_dir` accepts are checked, so a project
/// that wrote `.yaml` is not mistaken for one with no file at all.
fn is_tracked_pipeline(repo: &Repo, pipeline: &str) -> bool {
    let dir = crate::pipeline::Pipelines::dir_in(&repo.checkout);
    dir.join(format!("{pipeline}.yml")).is_file() || dir.join(format!("{pipeline}.yaml")).is_file()
}

/// `local/templates/tasks/<name>.md`, repo mode only — `None` in home mode,
/// where the whole setup is already private and there is no `local/` beside
/// it to read.
fn local(repo: &Repo, name: &str) -> Option<String> {
    if !crate::local::is_repo_mode(&repo.checkout) {
        return None;
    }
    read(&crate::local::task_templates_dir(&repo.local_dir()).join(format!("{name}.md")))
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
///
/// A whole line is dropped, its own newline with it, when it holds at least
/// one placeholder and every placeholder on it resolves empty — `- Blocked
/// by: ${SPOOLWAY_DEPENDS_TICKETS}` disappears entirely for a task with no
/// dependency, rather than rendering as an empty bullet nobody meant to
/// leave behind. A line with no placeholder at all, or one where at least
/// one placeholder resolves to something, is rendered exactly as before —
/// this only ever removes a line that would otherwise say nothing.
pub fn render_tracking(text: &str, env: &std::collections::BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let (line, newline) = match rest.find('\n') {
            Some(idx) => (&rest[..idx], true),
            None => (rest, false),
        };
        let (rendered, has_placeholder, all_empty) = render_line(line, env);
        if !(has_placeholder && all_empty) {
            out.push_str(&rendered);
            if newline {
                out.push('\n');
            }
        }
        if !newline {
            break;
        }
        rest = &rest[line.len() + 1..];
    }
    out
}

/// One line of [`render_tracking`]'s own substitution — split out so the
/// caller can decide whether to keep the line at all, not only what to
/// render it as. Returns the rendered line, whether it held any
/// `${SPOOLWAY_*}` placeholder, and whether every placeholder it held
/// resolved to the empty string.
fn render_line(
    line: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> (String, bool, bool) {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    let mut has_placeholder = false;
    let mut all_empty = true;
    loop {
        let Some(start) = rest.find("${") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        match rest.find('}') {
            Some(end) => {
                has_placeholder = true;
                let value = env.get(&rest[..end]).map(String::as_str).unwrap_or("");
                if !value.is_empty() {
                    all_empty = false;
                }
                out.push_str(value);
                rest = &rest[end + 1..];
            }
            // An unclosed `${` at the end of the line is not a placeholder
            // to resolve — left verbatim rather than swallowed.
            None => {
                out.push_str("${");
                out.push_str(rest);
                break;
            }
        }
    }
    (out, has_placeholder, all_empty)
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

    /// `local/templates/tasks/<pipeline>.md` — the private layer, see
    /// `crate::local` — answers when the tracked skeleton is absent, in repo
    /// mode, before falling all the way back to the built-in.
    #[test]
    fn a_private_skeleton_is_used_when_the_tracked_file_is_absent() {
        let root = crate::scratch::root("task-template-private");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(crate::config::TASK_TEMPLATES_DIR)).unwrap();
        let repo = Repo {
            checkout: root.clone(),
            home: root.join(".home"),
            root,
            config: crate::config::Config::default(),
        };

        let private = crate::local::task_templates_dir(&repo.local_dir()).join("impl-strict.md");
        std::fs::create_dir_all(private.parent().unwrap()).unwrap();
        std::fs::write(&private, "our own private skeleton\n").unwrap();

        assert_eq!(resolve(&repo, "impl-strict"), "our own private skeleton\n");

        std::fs::remove_dir_all(&repo.checkout).ok();
    }

    /// A tracked skeleton is never shadowed by a private one of the same
    /// name — nothing private ever replaces a tracked file.
    #[test]
    fn a_tracked_skeleton_wins_over_a_private_one_of_the_same_name() {
        let root = crate::scratch::root("task-template-private-shadowed");
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join(crate::config::TASK_TEMPLATES_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("impl-strict.md"), "the tracked skeleton\n").unwrap();
        let repo = Repo {
            checkout: root.clone(),
            home: root.join(".home"),
            root,
            config: crate::config::Config::default(),
        };

        let private = crate::local::task_templates_dir(&repo.local_dir()).join("impl-strict.md");
        std::fs::create_dir_all(private.parent().unwrap()).unwrap();
        std::fs::write(&private, "the private skeleton\n").unwrap();

        assert_eq!(resolve(&repo, "impl-strict"), "the tracked skeleton\n");

        std::fs::remove_dir_all(&repo.checkout).ok();
    }

    /// A tracked pipeline with no skeleton of its own falls through to the
    /// tracked `default.md`, never to a private `<pipeline>.md` that merely
    /// shares its name — `local/templates/tasks/` belongs to a private
    /// pipeline of that name, and `impl` here is a tracked one.
    #[test]
    fn a_tracked_pipeline_with_no_skeleton_of_its_own_falls_back_to_tracked_default_not_a_private_file()
     {
        let root = crate::scratch::root("task-template-tracked-pipeline-over-private-fallback");
        let _ = std::fs::remove_dir_all(&root);
        let templates_dir = root.join(crate::config::TASK_TEMPLATES_DIR);
        std::fs::create_dir_all(&templates_dir).unwrap();
        std::fs::write(templates_dir.join("default.md"), "the tracked default\n").unwrap();

        let pipelines_dir = crate::pipeline::Pipelines::dir_in(&root);
        std::fs::create_dir_all(&pipelines_dir).unwrap();
        std::fs::write(
            pipelines_dir.join("impl.yml"),
            "steps:\n  - id: a\n    agent: pi\n    model: base-model\n    on_pass: done\n",
        )
        .unwrap();

        let repo = Repo {
            checkout: root.clone(),
            home: root.join(".home"),
            root,
            config: crate::config::Config::default(),
        };

        let private = crate::local::task_templates_dir(&repo.local_dir()).join("impl.md");
        std::fs::create_dir_all(private.parent().unwrap()).unwrap();
        std::fs::write(&private, "a private file that only shares impl's name\n").unwrap();

        assert_eq!(resolve(&repo, "impl"), "the tracked default\n");

        std::fs::remove_dir_all(&repo.checkout).ok();
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

    /// A line holding at least one placeholder whose placeholders all
    /// resolve empty is dropped entirely — no empty bullet, no gap where it
    /// stood — while a line with no placeholder, and a line where one
    /// placeholder of several resolves non-empty, are both rendered exactly
    /// as before. The mockup this covers: `- Blocked by: ` on a dependency-
    /// free task.
    #[test]
    fn render_tracking_drops_a_line_whose_placeholders_all_resolve_empty() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("SPOOLWAY_TASK".to_string(), "demo".to_string());
        env.insert("SPOOLWAY_BRANCH".to_string(), "task/demo".to_string());

        let rendered = render_tracking(
            "Mirrors task `${SPOOLWAY_TASK}`.\n\
             \n\
             - Source: `${SPOOLWAY_SOURCE}`\n\
             - Blocked by: ${SPOOLWAY_DEPENDS_TICKETS}\n\
             - Branch: `${SPOOLWAY_BRANCH}`\n",
            &env,
        );

        assert_eq!(
            rendered,
            "Mirrors task `demo`.\n\
             \n\
             - Branch: `task/demo`\n"
        );
    }

    /// A line with two placeholders, one empty and one not, keeps its
    /// substitution and is not dropped — dropping only ever happens when
    /// *every* placeholder on the line resolves empty.
    #[test]
    fn render_tracking_keeps_a_line_with_one_set_and_one_empty_placeholder() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("SPOOLWAY_TASK".to_string(), "demo".to_string());

        let rendered = render_tracking("task `${SPOOLWAY_TASK}`, epic `${SPOOLWAY_EPIC}`\n", &env);
        assert_eq!(rendered, "task `demo`, epic ``\n");
    }
}
