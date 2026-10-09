//! `spoolway hook contract`: the environment an issue-tracking hook is
//! handed, event by event.
//!
//! A hook is one script, named by `issue_tracking.hook`, resolved inside
//! `.spoolway/hooks/` (a home-mode workspace's own `config/hooks/`) and run
//! on seven events — [`crate::tracking`] is the whole of what calls it.
//! This prints straight from the tables
//! [`crate::tracking::COMMON_EVENT_VARS`], [`crate::tracking::
//! OPEN_EVENT_VARS`], [`crate::tracking::DISPATCH_EVENT_VARS`] and
//! [`crate::tracking::FETCH_EVENT_VARS`] kept beside the functions that
//! build the real environment, rather than a second copy of the variable
//! list — `tracking::tests::open_dispatch_and_fetch_vars_match_a_real_environment`
//! is what checks those tables against a real call. `started` reuses
//! [`crate::tracking::DISPATCH_EVENT_VARS`], the same table `queued`,
//! `blocked`, `paused` and `done` do. A hook proves itself only on these
//! real events — there is no `check` event asking it to prove itself ahead
//! of time.

use super::*;

/// `spoolway hook contract`.
pub fn hook_contract(repo: &Repo) -> Result<()> {
    print!("{}", render_hook_contract(repo));
    Ok(())
}

/// One `(name, gloss)` table, indented and joined into `out`.
fn render_vars(out: &mut String, vars: &[(&str, &str)]) {
    let width = vars.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    for (name, gloss) in vars {
        out.push_str(&format!("  {name:width$}  {gloss}\n"));
    }
}

/// [`hook_contract`]'s body, built as a string so a test can assert on it
/// directly rather than capturing stdout.
fn render_hook_contract(repo: &Repo) -> String {
    use crate::tracking::{
        COMMON_EVENT_VARS, DISPATCH_EVENT_VARS, FETCH_EVENT_VARS, OPEN_EVENT_VARS,
    };

    let hooks_dir = relative(
        &repo.checkout,
        &crate::tracking::hooks_dir_in(&repo.checkout),
    );
    let mut out = String::new();
    out.push_str("THE HOOK CONTRACT\n");
    out.push_str("=================\n\n");
    out.push_str(&format!(
        "One script, named by `issue_tracking.hook`, resolved inside {hooks_dir}/ — a\n\
         bare filename only, never a path. Run on seven events, `SPOOLWAY_EVENT` naming which.\n\n",
    ));

    out.push_str("EVERY EVENT CARRIES\n");
    render_vars(&mut out, COMMON_EVENT_VARS);
    out.push('\n');

    out.push_str(
        "open        — synchronous, from `spoolway queue add`, before the task is queued\n",
    );
    render_vars(&mut out, OPEN_EVENT_VARS);
    out.push_str(
        "  A hook writing none of the SPOOLWAY_OUT lines reads back as four blank strings,\n  \
         never an error.\n\n",
    );

    out.push_str("queued, blocked, paused, done  — fire-and-forget, once a task settles there\n");
    render_vars(&mut out, DISPATCH_EVENT_VARS);
    out.push_str(
        "  No SPOOLWAY_OUT: nothing reads an answer back from these four. A non-zero exit is\n  \
         always recorded; on `queued` or `done` it also pauses the task, with a reason naming \
         the hook's own log under tracking/ — `spoolway resume` forgets that run, so the hook \
         runs again. On `blocked` or `paused`, already stopped for a person, it is only ever \
         recorded.\n  A run killed without an exit code (a reboot, `wsl --shutdown`, an OOM kill) is \
         fired again by the\n  dispatcher, up to 3 kills in a row, then the task pauses as for a \
         non-zero exit. So a hook\n  must be safe to run twice: the killed run may already have \
         done part of its work.\n\n",
    );

    out.push_str(
        "started     — fire-and-forget, once a task actually leaves `queued` for its entry \
         step\n",
    );
    render_vars(&mut out, DISPATCH_EVENT_VARS);
    out.push_str(
        "  The same variables `queued`/`blocked`/`paused`/`done` carry — see above. Not a \
         fifth\n  stage: `queued` still fires on the dispatcher's very first pass over a task, \
         even while\n  it waits on a dependency; `started` fires once, only when the task is \
         actually about to\n  launch. The task launches only once this hook exits clean; a \
         non-zero exit pauses it\n  exactly as a failing `queued` hook does, and `spoolway \
         resume` runs it again. A run killed without an exit code is fired again, as above.\n\n",
    );

    out.push_str("fetch       — synchronous, from `spoolway issue show <reference>`\n");
    render_vars(&mut out, FETCH_EVENT_VARS);
    out.push_str(
        "  Refused by name when the configured script's own text never mentions `fetch` \
         anywhere\n  — an unmodified script from before this event existed, rather than one \
         that ran and\n  found nothing.\n\n",
    );

    out.push_str(
        "spoolway doctor  checks the script names `fetch` when it is used, and writes a \
         `slug=`\n                 line when `issue_tracking.key_in_names` is on. A hook \
         proves itself only on\n                 a real event: `queued`, `started` or `done` \
         failing pauses the task, with the\n                 hook's own last output under \
         `## Hook error` on the task file.\n\n",
    );

    out.push_str(
        "Every shipped hook's `done` is a pull request handed off, not a merge — `spoolway \
         stack` has just opened it, and nobody has reviewed anything yet.\n",
    );
    out.push_str(
        "Neither `github.sh` nor `jira.sh` closes anything on `done`: the GitHub issue moves \
         to `spoolway:review`, the Jira Sub-task and its Story move to Review, and that is \
         where spoolway's own part ends.\n",
    );
    out.push_str(
        "Resolved, Done, Closed — whatever a tracker calls it — is the user's own merge \
         automation to wire, not a shipped hook's. Three ordinary ways to wire it: the \
         tracker's own GitHub app with an automation rule keyed on the pull request title, a \
         pull request workflow the project already runs, or a `spoolway jobs` routine polled \
         on a schedule. A custom hook should keep the same split.\n\n",
    );

    out.push_str("This project's own files:\n");
    out.push_str(&format!(
        "  {}\n",
        relative(
            &repo.checkout,
            &crate::tracking::hooks_dir_in(&repo.checkout)
        )
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The acceptance shape: every event, and the variables each one — and
    /// only that one — actually carries, so a hook author is not left
    /// guessing which lines a script may lean on.
    #[test]
    fn hook_contract_names_every_event_and_its_own_variables() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("hook-contract-events");
        let text = render_hook_contract(&repo);
        for fact in [
            "SPOOLWAY_EVENT",
            "SPOOLWAY_PROJECT_KEY",
            "open        —",
            "SPOOLWAY_DEPENDS_TICKETS",
            "queued, blocked, paused, done",
            "SPOOLWAY_GROUP_LAST",
            "started     —",
            "fetch       —",
            "SPOOLWAY_REF",
        ] {
            assert!(text.contains(fact), "hook contract drops `{fact}`");
        }
        // `open` never sees `SPOOLWAY_FROM` — nothing has happened to a task
        // before it is queued — and the four fire-and-forget events never
        // see `SPOOLWAY_OUT`, since nothing reads an answer back from them.
        let open_section = text.split("queued, blocked").next().unwrap();
        assert!(!open_section.contains("SPOOLWAY_FROM"));
        assert!(!text.contains("check       —"), "no `check` event remains");
        assert!(
            !text.contains("SPOOLWAY_EVENT=check"),
            "no `check` event remains"
        );
        // The count in the gloss line has to move with the count in the
        // opening paragraph — review finding: `COMMON_EVENT_VARS`' own
        // `SPOOLWAY_EVENT` gloss still said "eight events" after `check`
        // was dropped everywhere else, so `hook contract` disagreed with
        // itself between its own two counts.
        assert!(
            !text.contains("eight"),
            "the contract still counts eight events somewhere: {text}"
        );
        assert!(text.contains("seven events"), "the contract's own count");
    }

    /// The acceptance criterion behind the whole task this contract text was
    /// last edited for: `done` is a handoff, never a close, for either
    /// shipped hook — and closing a ticket is named as the user's own to
    /// wire, with three ordinary ways to do it.
    #[test]
    fn hook_contract_says_done_is_a_handoff_and_names_three_ways_to_close() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("hook-contract-done");
        let text = render_hook_contract(&repo);
        // Each assertion below is a distinct claim the acceptance criteria
        // make: a looser one (just "GitHub" and "Jira" appearing anywhere)
        // would still pass if the load-bearing sentences it is drawn from
        // were edited away, which is exactly what a prior version of this
        // test let happen.
        for fact in [
            "Every shipped hook's `done` is a pull request handed off, not a merge",
            "Neither `github.sh` nor `jira.sh` closes anything on `done`",
            "the user's own merge automation to wire",
            "the tracker's own GitHub app with an automation rule keyed",
            "a pull request workflow the project already runs",
            "a `spoolway jobs` routine polled on a schedule",
        ] {
            assert!(text.contains(fact), "hook contract drops `{fact}`");
        }
    }

    /// A home-mode checkout has no `.spoolway/hooks/` of its own — the
    /// contract's closing "this project's own files" line has to name the
    /// workspace's `config/hooks/` instead, the folder a hook actually
    /// resolves against there.
    #[test]
    fn hook_contract_names_the_workspace_hooks_folder_in_home_mode() {
        let root = crate::scratch::root("hook-contract-home-mode");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let home = crate::scratch::root("hook-contract-home-mode-home");
        let _ = std::fs::remove_dir_all(&home);
        let workspace = home.join(".spoolway").join("ws");
        std::fs::create_dir_all(workspace.join("config")).unwrap();
        std::fs::write(
            workspace.join(crate::repo::BINDING_FILE),
            format!(
                "id = \"ws\"\nclones = [{{ root = \"{}\", dispatcher = \"api\" }}]\n",
                root.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        let repo = Repo {
            borrowed: false,
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: Config::default(),
            home: workspace.join("dispatchers").join("api"),
        };

        let text = crate::platform::test_home::with_home(&home, || render_hook_contract(&repo));
        assert!(
            text.contains(&workspace.join("config").join("hooks").display().to_string()),
            "{text}"
        );
        // The opening sentence, not only the closing "this project's own
        // files" line, has to name the workspace's folder — review finding
        // 2: it used to say `.spoolway/hooks/` unconditionally, which
        // contradicted the closing line right below it.
        assert!(!text.contains(".spoolway/hooks/"), "{text}");
    }
}
