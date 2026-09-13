//! `spoolway hook contract`: the environment an issue-tracking hook is
//! handed, event by event.
//!
//! A hook is one script, named by `issue_tracking.hook`, resolved inside
//! `.spoolway/hooks/` and run on six events — [`crate::tracking`] is the
//! whole of what calls it. This prints straight from the tables
//! [`crate::tracking::COMMON_EVENT_VARS`], [`crate::tracking::
//! OPEN_EVENT_VARS`], [`crate::tracking::DISPATCH_EVENT_VARS`] and
//! [`crate::tracking::FETCH_EVENT_VARS`] kept beside the functions that
//! build the real environment, rather than a second copy of the variable
//! list — `tracking::tests::open_dispatch_and_fetch_vars_match_a_real_environment`
//! is what checks those tables against a real call.

use super::*;

/// `spoolway hook contract`.
pub fn hook_contract() -> Result<()> {
    print!("{}", render_hook_contract());
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
fn render_hook_contract() -> String {
    use crate::tracking::{
        COMMON_EVENT_VARS, DISPATCH_EVENT_VARS, FETCH_EVENT_VARS, OPEN_EVENT_VARS,
    };

    let mut out = String::new();
    out.push_str("THE HOOK CONTRACT\n");
    out.push_str("=================\n\n");
    out.push_str(
        "One script, named by `issue_tracking.hook`, resolved inside .spoolway/hooks/ — a\n\
         bare filename only, never a path. Run on six events, `SPOOLWAY_EVENT` naming which.\n\n",
    );

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
         recorded either way; `issue_tracking.on_fail` decides whether it also pauses the \
         task.\n\n",
    );

    out.push_str("fetch       — synchronous, from `spoolway issue show <reference>`\n");
    render_vars(&mut out, FETCH_EVENT_VARS);
    out.push_str(
        "  Refused by name when the configured script's own text never mentions `fetch` \
         anywhere\n  — an unmodified script from before this event existed, rather than one \
         that ran and\n  found nothing.\n\n",
    );

    out.push_str("spoolway doctor  checks the script names `fetch` when the event is used, and\n");
    out.push_str(
        "                 writes a `slug=` line when `issue_tracking.key_in_names` is on.\n",
    );
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
        let text = render_hook_contract();
        for fact in [
            "SPOOLWAY_EVENT",
            "SPOOLWAY_PROJECT_KEY",
            "open        —",
            "SPOOLWAY_DEPENDS_TICKETS",
            "queued, blocked, paused, done",
            "SPOOLWAY_GROUP_LAST",
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
    }
}
