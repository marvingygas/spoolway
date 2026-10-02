//! The private layer: `local/pipelines/`, `local/prompts/` and
//! `local/templates/tasks/`, read beside the tracked `.spoolway/` — never in
//! place of it. See the plan's own `d-private-pipelines` decision.
//!
//! A private file is never merged onto a tracked one the way
//! [`crate::overrides`] merges a patch: it is a whole extra entry, and a
//! private pipeline or prompt whose name already belongs to a tracked one is
//! refused outright, at [`crate::pipeline::Pipelines::load_impl`] and
//! [`crate::prompt::path_for`]'s own callers — so nothing private can ever
//! silently stand in for something tracked. A tracked pipeline naming a
//! prompt that exists only here is refused the same way, for the opposite
//! reason: that pipeline would break the moment it ran on a machine with no
//! copy of this private prompt.
//!
//! Repo mode only. Home mode has no tracked control plane to sit beside in
//! the first place — the whole of a home-mode project's setup is already
//! private to the machine it runs on — so there is nothing for a second
//! private layer to add, and [`is_repo_mode`] is checked before any of the
//! three directories below is ever read.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// Directory under a project's home holding the private layer.
pub const LOCAL_DIR: &str = "local";

/// Whether `root` is a repo-mode checkout — the only mode the private layer
/// is read in. A tracked `.spoolway/` always wins, the same precedence
/// `crate::config::setup_dir_in` gives it; short of that, a checkout some
/// workspace's `project.toml` lists by path is home mode instead — see
/// `crate::repo::workspace_clone`, the same check `doctor`'s own mode line
/// reads.
pub(crate) fn is_repo_mode(root: &Path) -> bool {
    crate::config::tracked_setup_dir_in(root).is_dir()
        || crate::repo::workspace_clone(root).is_none()
}

/// Where the private layer lives for `root` — the same project-home
/// resolution [`crate::overrides::dir_for`] uses, so a lane running from its
/// own worktree lands on the identical directory a command run from the main
/// checkout does, with nothing copied in.
pub(crate) fn dir_for(root: &Path) -> Result<PathBuf> {
    let main = crate::repo::main_checkout(root).unwrap_or_else(|| root.to_path_buf());
    Ok(crate::mux::project_home(&main)?.join(LOCAL_DIR))
}

/// `local/pipelines/` — one file per private pipeline, the same shape as the
/// tracked `.spoolway/pipelines/`.
pub(crate) fn pipelines_dir(local: &Path) -> PathBuf {
    local.join("pipelines")
}

/// `local/prompts/` — one directory per private prompt, the same shape as
/// the tracked `.spoolway/prompts/`.
pub(crate) fn prompts_dir(local: &Path) -> PathBuf {
    local.join("prompts")
}

/// `local/templates/tasks/` — one skeleton per private pipeline, the same
/// shape as the tracked `.spoolway/templates/tasks/`.
pub(crate) fn task_templates_dir(local: &Path) -> PathBuf {
    local.join("templates").join("tasks")
}

/// Whether `name` is one plain path component — not empty, not absolute, and
/// carrying none of `/`, `\` or `..`. `pipeline copy` and `prompt copy` join
/// a `<from>`/`<to>` straight onto a private directory with nothing else
/// standing between the string and the filesystem, so this is the one shape
/// that join can ever be trusted with; anything else walks the write
/// outside the private folder — in repo mode as far as a tracked
/// `.spoolway/` the traversal happens to reach, and in home mode out of the
/// workspace's own `config/`, the private layer's stand-in there. `\` is
/// checked as a literal character rather than through
/// [`std::path::Component`], which treats it as ordinary on this platform
/// and would wave a Windows-style traversal straight through. Also the shape
/// [`prompt::local_names_in`](crate::prompt::local_names_in) enumerates —
/// direct children only — so a nested name like `nest/inner` is never
/// resolved privately either, closing the gap
/// [`prompt::path_for`](crate::prompt::path_for) otherwise has by joining it
/// straight on.
pub(crate) fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !Path::new(name).is_absolute()
}

/// Refuse `name` as a `<from>`/`<to>` unless [`is_plain_name`] — the one
/// check `pipeline copy` and `prompt copy` run on both arguments before
/// touching the filesystem at all, so a bad name is caught before anything
/// is read or written rather than after. `label` names what kind of thing
/// `name` is meant to be (`"pipeline"`, `"prompt"`), for the message.
pub(crate) fn refuse_unless_plain_name(label: &str, name: &str) -> Result<()> {
    if is_plain_name(name) {
        return Ok(());
    }
    bail!(
        "`{name}` is not a valid {label} name — one plain name, no `/`, `\\` or `..`, and not \
         empty or absolute"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain, ordinary name is the only shape `pipeline copy` and `prompt
    /// copy` may ever write — everything a name could do to escape the
    /// private directory it is joined onto, refused.
    #[test]
    fn a_name_that_is_empty_absolute_or_holds_a_separator_or_dotdot_is_not_plain() {
        for bad in [
            "",
            "/etc/passwd",
            "a/b",
            "..",
            "../../../../escaped",
            "a\\b",
            "\\\\x",
        ] {
            assert!(!is_plain_name(bad), "`{bad}` should not be plain");
            assert!(
                refuse_unless_plain_name("pipeline", bad).is_err(),
                "`{bad}` should have been refused"
            );
        }
    }

    #[test]
    fn an_ordinary_name_is_plain() {
        for good in ["default", "default-strict", "implementer_v2"] {
            assert!(is_plain_name(good), "`{good}` should be plain");
            assert!(refuse_unless_plain_name("pipeline", good).is_ok());
        }
    }
}
