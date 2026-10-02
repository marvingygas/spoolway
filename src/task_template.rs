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
/// Reads through [`resolve_named`] as `pipeline.name` naming the pipeline
/// itself and [`crate::pipeline::Pipeline::task_template_name`] naming the
/// file — see there for the chain and why those two names are not always
/// the same one.
pub fn resolve_for(repo: &Repo, pipeline: &crate::pipeline::Pipeline) -> String {
    resolve_named(repo, &pipeline.name, pipeline.task_template_name())
}

/// [`resolve_for`] for a bare name with no `task_template:` of its own to
/// consult — `pipeline` names both the pipeline and the skeleton it reads,
/// which is exactly right for [`FALLBACK`] itself and for any caller with
/// only a name in hand and no loaded [`crate::pipeline::Pipeline`] to ask.
pub fn resolve(repo: &Repo, pipeline: &str) -> String {
    resolve_named(repo, pipeline, pipeline)
}

/// The skeleton a task queued on the pipeline named `pipeline_name` is
/// written from, where `skeleton_name` is the file to look for —
/// `pipeline_name` itself unless `task_template:` names something else.
///
/// `<skeleton_name>.md`, then `local/templates/tasks/<skeleton_name>.md`
/// (repo mode only, and only when *`pipeline_name`* is not itself a tracked
/// pipeline — see below), then `default.md` and its own private fallback,
/// then the built-in — the same shape as a step's `prompt:` fallback, and
/// the same refusal to treat a missing file as an error. A skeleton that is
/// not there is a project that has not written one, which is the normal
/// state of most projects and no reason to stop a queue.
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
///
/// The guard is decided from `pipeline_name`, never `skeleton_name` — a
/// tracked pipeline `impl` naming `task_template: foo` must still never
/// reach a private `foo.md`, even though `foo` itself is not a tracked
/// pipeline's own name. Deciding it from `skeleton_name` instead was the
/// bug: every caller already hands this `task_template_name()`, the
/// *skeleton* name, so a trackedness check made from that argument alone
/// was really asking "is `foo` a tracked pipeline", never the question that
/// actually matters, "is `impl` one" — which is exactly how a private
/// skeleton kept leaking into a tracked pipeline even after the fallback
/// lookup below was first guarded.
fn resolve_named(repo: &Repo, pipeline_name: &str, skeleton_name: &str) -> String {
    let dir = repo.task_templates_dir();
    let tracked = is_tracked_pipeline(repo, pipeline_name);

    if let Some(contents) = read(&dir.join(format!("{skeleton_name}.md"))) {
        return contents;
    }
    if !tracked && let Some(contents) = local(repo, skeleton_name) {
        return contents;
    }
    if let Some(contents) = read(&dir.join(format!("{FALLBACK}.md"))) {
        return contents;
    }
    if !tracked && let Some(contents) = local(repo, FALLBACK) {
        return contents;
    }

    crate::assets::task_template(skeleton_name)
        .or_else(|| crate::assets::task_template(FALLBACK))
        .unwrap_or_default()
        .to_string()
}

/// Whether `pipeline` has a skeleton of its own — the first half of
/// [`resolve_named`]'s own chain, pulled out so `pipeline check`'s
/// `step_problems` can ask the identical question
/// [`resolve_for`] is actually answered with, rather than looking in the
/// tracked directory alone (reporting a private skeleton missing) or
/// guarding trackedness from the skeleton name rather than the pipeline's
/// own (letting a tracked pipeline reach a private skeleton that merely
/// shares its `task_template:` value with no tracked pipeline of its own).
pub fn exists_for(repo: &Repo, pipeline: &crate::pipeline::Pipeline) -> bool {
    let skeleton_name = pipeline.task_template_name();
    let dir = repo.task_templates_dir();
    if dir.join(format!("{skeleton_name}.md")).is_file() {
        return true;
    }
    !is_tracked_pipeline(repo, &pipeline.name) && local(repo, skeleton_name).is_some()
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
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
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
            checkout: root.to_path_buf(),
            home: root.join(".home"),
            root: root.to_path_buf(),
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
            checkout: root.to_path_buf(),
            home: root.join(".home"),
            root: root.to_path_buf(),
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
            checkout: root.to_path_buf(),
            home: root.join(".home"),
            root: root.to_path_buf(),
            config: crate::config::Config::default(),
        };

        let private = crate::local::task_templates_dir(&repo.local_dir()).join("impl.md");
        std::fs::create_dir_all(private.parent().unwrap()).unwrap();
        std::fs::write(&private, "a private file that only shares impl's name\n").unwrap();

        assert_eq!(resolve(&repo, "impl"), "the tracked default\n");

        std::fs::remove_dir_all(&repo.checkout).ok();
    }

    /// A tracked pipeline naming `task_template: foo`, where `foo` is not
    /// itself a tracked pipeline's own name, used to leak the private
    /// `foo.md`: the old guard decided trackedness from the *skeleton*
    /// name (`foo`) rather than the *pipeline's* (`impl`), so
    /// `is_tracked_pipeline(repo, "foo")` came back false and let the
    /// private lookup through. [`resolve_for`] must decide the guard from
    /// `pipeline.name`, never from `task_template_name()`.
    #[test]
    fn a_tracked_pipeline_naming_a_private_skeleton_never_reaches_it() {
        let root = crate::scratch::root("task-template-tracked-pipeline-names-private-skeleton");
        let _ = std::fs::remove_dir_all(&root);

        let pipelines_dir = crate::pipeline::Pipelines::dir_in(&root);
        std::fs::create_dir_all(&pipelines_dir).unwrap();
        std::fs::write(
            pipelines_dir.join("impl.yml"),
            "task_template: foo\n\
             steps:\n  - id: a\n    agent: pi\n    model: base-model\n    on_pass: done\n",
        )
        .unwrap();

        let repo = Repo {
            checkout: root.to_path_buf(),
            home: root.join(".home"),
            root: root.to_path_buf(),
            config: crate::config::Config::default(),
        };

        // No tracked `foo.md` and no tracked `default.md` either — only a
        // private `foo.md`, written for some unrelated private pipeline
        // that happens to share the name `impl` asks for.
        let private = crate::local::task_templates_dir(&repo.local_dir()).join("foo.md");
        std::fs::create_dir_all(private.parent().unwrap()).unwrap();
        std::fs::write(&private, "an unrelated private foo\n").unwrap();

        let pipeline = crate::pipeline::Pipeline::parse(
            "impl",
            "task_template: foo\n\
             steps:\n  - id: a\n    agent: pi\n    model: base-model\n    on_pass: done\n",
        )
        .unwrap();

        assert_eq!(
            resolve_for(&repo, &pipeline),
            crate::assets::task_template(FALLBACK).unwrap()
        );

        std::fs::remove_dir_all(&repo.checkout).ok();
    }

    /// The fallback's own private lookup — `local/templates/tasks/default.md`
    /// — used to run unguarded: a tracked pipeline with no skeleton of its
    /// own, and no tracked `default.md` either, fell through to whatever
    /// private `default.md` happened to exist for some unrelated private
    /// pipeline. Nothing private may ever answer for a tracked pipeline,
    /// the fallback included, so this must reach the built-in instead.
    #[test]
    fn a_tracked_pipeline_never_falls_through_to_a_private_default_either() {
        let root = crate::scratch::root("task-template-tracked-pipeline-skips-private-fallback");
        let _ = std::fs::remove_dir_all(&root);

        let pipelines_dir = crate::pipeline::Pipelines::dir_in(&root);
        std::fs::create_dir_all(&pipelines_dir).unwrap();
        std::fs::write(
            pipelines_dir.join("impl.yml"),
            "steps:\n  - id: a\n    agent: pi\n    model: base-model\n    on_pass: done\n",
        )
        .unwrap();

        let repo = Repo {
            checkout: root.to_path_buf(),
            home: root.join(".home"),
            root: root.to_path_buf(),
            config: crate::config::Config::default(),
        };

        // No tracked `default.md` at all — only a private one, written for
        // an unrelated private pipeline that merely happens to be named
        // `default`.
        let private = crate::local::task_templates_dir(&repo.local_dir()).join("default.md");
        std::fs::create_dir_all(private.parent().unwrap()).unwrap();
        std::fs::write(&private, "an unrelated private default\n").unwrap();

        assert_eq!(
            resolve(&repo, "impl"),
            crate::assets::task_template(FALLBACK).unwrap()
        );

        std::fs::remove_dir_all(&repo.checkout).ok();
    }
}
