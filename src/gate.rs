//! The notice in front of a command that needs a project, once `spoolway
//! update` has installed a newer binary than the one that last brought this
//! checkout's files current.
//!
//! It only informs. Nothing here writes a file, reads a key or stops a
//! command: `spoolway sync` is the one thing that applies an update, and it
//! shows what it will write and asks first (`crate::sync::run_asking`). A
//! notice that applied the update itself would be a second, less careful way
//! to do the same writes.
//!
//! [`crate::sync::stamp_behind`] is the cheap question — one file read and a
//! few hashes — asked before anything else here runs, so a project already
//! current pays nothing extra on every single command. Only once that says
//! yes does this pay for a real, dry [`crate::sync::scan`], and only once
//! *that* finds something to write, remove or refuse does anybody see the
//! notice: a stamp that has moved but a scan that finds nothing to do (every
//! file already hand-matches what the new release would write) has nothing
//! worth mentioning. A stamp that is missing or unreadable skips the scan and
//! shows the notice outright, since nothing says the project was ever brought
//! current.
//!
//! Every other command prints [`LINE`] on stderr and runs — see [`notify`].
//! Bare `spoolway` would wipe a printed line with its screen's first frame,
//! so [`sync_popup`] hands the same sentence to the screen to lay over the
//! tab it opens on — see `crate::screen::shell::OnOpen`.

use std::io::Write;

use anyhow::Result;

use crate::cli::SyncArgs;
use crate::repo::Repo;
use crate::screen;

/// The notice, printed and in the popup alike.
pub(crate) const LINE: &str = "Run spoolway sync to apply the last update.";

/// The popup's title.
const TITLE: &str = "update installed";

/// Whether this checkout is behind what this binary would write: the stamp
/// is missing or unreadable, or it has moved and a dry scan finds something
/// to write, remove or refuse — see the
/// module doc for why the two questions are asked in that order.
fn behind(repo: &Repo) -> Result<bool> {
    if !crate::sync::stamp_behind(&repo.home, &repo.checkout) {
        return Ok(false);
    }
    // No stamp to compare against is not "a version moved with nothing to
    // write": nothing says this project was ever brought current, and a scan
    // with nothing to do would hide that. The notice stays until a `sync`
    // has really run and written one.
    if crate::sync::read_stamp(&repo.home, &repo.checkout).is_none() {
        return Ok(true);
    }
    let dry = SyncArgs {
        dry_run: true,
        replace: Vec::new(),
    };
    // Not `?`: `crate::sync::config` now fails the whole scan outright on a
    // `config.toml` it cannot even parse, naming the file — the right thing
    // for `sync` itself to say, but the wrong thing for this module, whose
    // own doc promises that nothing here stops a command. An ordinary
    // command hitting that in passing, on a checkout a stamp already calls
    // behind, reads as "not behind" instead: `sync` is still the thing that
    // will say so, the moment somebody actually runs it.
    let Ok(outcomes) = crate::sync::scan(repo, &dry) else {
        return Ok(false);
    };
    let (wrote, removed) = crate::sync::dedup_paths(&outcomes);
    let refused = crate::sync::refusals(&outcomes);
    Ok(!wrote.is_empty() || !removed.is_empty() || !refused.is_empty())
}

/// Print [`LINE`] on stderr when this checkout is behind and a person is
/// there to read it. The real-stderr wrapper over [`notify_with`].
pub(crate) fn notify(repo: &Repo, in_lane: bool, json: bool) -> Result<()> {
    use std::io::IsTerminal;
    notify_with(
        repo,
        in_lane,
        json,
        std::io::stderr().is_terminal(),
        &mut std::io::stderr(),
    )
}

/// [`notify`]'s own logic, taking whether stderr is a terminal and where the
/// line goes, so a test can drive every branch.
///
/// Only to a person at a terminal, the same audience
/// `crate::release::Audience::wants_notice` picks for the update notice: not
/// with `--json`, where something is parsing the output; not in a lane,
/// where a line naming `spoolway sync` is a lane that runs it, mid-step, in
/// the worktree it is being reviewed on; and not with stderr going anywhere
/// but a terminal. Asked before [`behind`], so none of those pays for a scan.
fn notify_with(
    repo: &Repo,
    in_lane: bool,
    json: bool,
    tty: bool,
    err: &mut impl Write,
) -> Result<()> {
    if in_lane || json || !tty || !behind(repo)? {
        return Ok(());
    }
    writeln!(err, "{LINE}")?;
    Ok(())
}

/// The same notice as a popup, for bare `spoolway` to lay over the tab it
/// opens on rather than print before its screen is drawn — or `None` when
/// this checkout is not behind. `enter` dismisses it and does nothing else.
///
/// Its key reads `dismiss` where every other notice over a tab reads
/// `close`: this one names something left undone, and closing it does not
/// do it.
pub(crate) fn sync_popup(repo: &Repo) -> Result<Option<Vec<String>>> {
    if !behind(repo)? {
        return Ok(None);
    }
    Ok(Some(screen::notice(
        TITLE,
        LINE,
        &screen::keys(&[("enter", "dismiss")]),
        screen::NOTICE_WRAP,
    )))
}

/// Whether bare `spoolway` may show this notice as [`sync_popup`] over its
/// screen, rather than printed ahead of it the way every other command does.
///
/// Not when the project's pipelines do not load. The screen needs them to
/// open at all, and a pipeline file carrying a retired step shape is
/// refused by the load — so the popup could never be drawn, and bare
/// `spoolway` ends on the refusal. It prints [`LINE`] first instead, so the
/// refusal is not the only thing it says.
pub(crate) fn asks_as_popup(repo: &Repo) -> bool {
    crate::pipeline::Pipelines::load(&repo.root, &repo.config).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    // `checkout` and `root` the same path, as `sync`'s own fixture keeps
    // them: `Repo::checkout_note` only shells out to `git branch` once the
    // two differ, and a scratch directory here is no git repository at all.
    fn fixture(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let root = crate::scratch::root(&format!("gate-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(crate::config::TASK_TEMPLATES_DIR)).unwrap();
        std::fs::create_dir_all(root.join(".spoolway/templates")).unwrap();
        let home = root.join(".home");
        std::fs::create_dir_all(&home).unwrap();
        (
            Repo {
                checkout: root.to_path_buf(),
                root: root.to_path_buf(),
                config: Config::default(),
                home,
            },
            root,
        )
    }

    /// The stamp claims a release that never shipped, so [`crate::sync::
    /// stamp_behind`] reads true whatever is actually on disk.
    fn make_stale(repo: &Repo) {
        std::fs::write(
            crate::sync::stamp_path(&repo.home),
            format!("0.0.0-old deadbeef {}\n", repo.checkout.display()),
        )
        .unwrap();
    }

    fn stamp(repo: &Repo) -> String {
        std::fs::read_to_string(crate::sync::stamp_path(&repo.home)).unwrap()
    }

    /// What [`notify_with`] printed, as `(in_lane, json, tty)` say.
    fn notified(repo: &Repo, in_lane: bool, json: bool, tty: bool) -> String {
        let mut err = Vec::new();
        notify_with(repo, in_lane, json, tty, &mut err).unwrap();
        String::from_utf8(err).unwrap()
    }

    /// A behind checkout at a terminal gets exactly the one line — and the
    /// command's files are left as they were: no config written, the stamp
    /// untouched.
    #[test]
    fn a_behind_checkout_at_a_terminal_prints_the_line_and_writes_nothing() {
        let (repo, _root_guard) = fixture("notify");
        make_stale(&repo);
        let before = stamp(&repo);
        assert_eq!(
            notified(&repo, false, false, true),
            "Run spoolway sync to apply the last update.\n"
        );
        assert!(!Config::path_in(&repo.checkout).exists());
        assert_eq!(stamp(&repo), before);
    }

    /// `--json`, a lane and a stderr that is no terminal each print nothing,
    /// behind or not.
    #[test]
    fn json_a_lane_and_no_terminal_each_print_nothing() {
        let (repo, _root_guard) = fixture("notify-gated");
        make_stale(&repo);
        for (in_lane, json, tty) in [
            (false, true, true),
            (true, false, true),
            (false, false, false),
        ] {
            assert_eq!(
                notified(&repo, in_lane, json, tty),
                "",
                "in_lane {in_lane}, json {json}, tty {tty}"
            );
        }
    }

    /// A project whose stamp was never written, or cannot be read, is told to
    /// run `sync` even with nothing for a scan to do: nothing records that it
    /// was ever brought current, and one `sync` writes the stamp.
    #[test]
    fn no_stamp_at_all_shows_the_notice() {
        let (repo, _root_guard) = fixture("no-stamp");
        assert_eq!(notified(&repo, false, false, true), format!("{LINE}\n"));
        assert!(sync_popup(&repo).unwrap().is_some());

        std::fs::write(crate::sync::stamp_path(&repo.home), "garbage\n").unwrap();
        assert_eq!(notified(&repo, false, false, true), format!("{LINE}\n"));
    }

    /// A stale stamp with nothing for a scan to do — every tracked file
    /// already matches what this binary would write — says nothing either:
    /// the notice is for a stamp *and* a scan that both say so.
    #[test]
    fn a_stale_stamp_with_nothing_to_scan_prints_nothing() {
        let (repo, _root_guard) = fixture("stale-nothing-to-do");
        make_stale(&repo);
        // A rendered config already in the canonical shape `sync` would
        // write, so `scan`'s own `config()` reports `Kept` rather than a
        // rewrite — the one file this fixture has for it to look at.
        let rendered = Config::default().render().unwrap();
        std::fs::write(Config::path_in(&repo.checkout), rendered).unwrap();
        assert_eq!(notified(&repo, false, false, true), "");
        assert!(sync_popup(&repo).unwrap().is_none());
    }

    /// A moved stamp over a scan that finds only a refused file still shows
    /// the notice: a refusal is something `sync` has left to fix.
    #[test]
    fn a_stale_stamp_with_only_a_refusal_shows_the_notice() {
        let (repo, _root_guard) = fixture("stale-refusal-only");
        make_stale(&repo);
        let rendered = Config::default().render().unwrap();
        std::fs::write(Config::path_in(&repo.checkout), rendered).unwrap();
        let dir = repo.checkout.join(".spoolway/pipelines");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("half.yml"),
            format!("{}\n# Top level\n", crate::assets::PIPELINE_KEYS_BEGIN),
        )
        .unwrap();
        assert_eq!(notified(&repo, false, false, true), format!("{LINE}\n"));
    }

    /// A stale stamp over a `config.toml` this binary cannot even parse
    /// prints nothing and does not fail the command: `crate::sync::config`
    /// now fails the whole scan on a file like this, which is right for
    /// `sync` itself to say but wrong for this module's own promise that
    /// nothing here stops a command — `sync` is still the thing that will
    /// say so, the moment somebody actually runs it.
    #[test]
    fn a_stale_stamp_over_a_config_that_does_not_parse_prints_nothing() {
        let (repo, _root_guard) = fixture("stale-unparseable-config");
        make_stale(&repo);
        std::fs::write(Config::path_in(&repo.checkout), "garbage = [\n").unwrap();
        assert_eq!(notified(&repo, false, false, true), "");
        assert_eq!(sync_popup(&repo).unwrap(), None);
    }

    /// Bare `spoolway`'s popup: the one sentence, titled `update installed`,
    /// over `[enter] dismiss` — and building it writes nothing.
    #[test]
    fn the_popup_carries_the_line_over_enter_dismiss() {
        let (repo, _root_guard) = fixture("popup");
        make_stale(&repo);
        let before = stamp(&repo);
        let panel = sync_popup(&repo)
            .unwrap()
            .expect("a stale stamp with a config to write shows the popup");
        assert!(panel[0].starts_with("┌─ update installed "), "{panel:?}");
        let flat = panel.join("\n");
        assert!(flat.contains(LINE), "{flat}");
        assert!(flat.contains("[enter] dismiss"), "{flat}");
        assert!(!flat.contains("[enter] confirm"), "{flat}");
        assert!(!flat.contains("[esc]"), "{flat}");
        assert!(!Config::path_in(&repo.checkout).exists());
        assert_eq!(stamp(&repo), before);
    }

    /// A pipeline with a retired step shape — the load refuses it — sends
    /// bare `spoolway` to the printed line, since its screen could never
    /// open to show the popup; once the file is fixed by hand and loads,
    /// the popup shows.
    #[test]
    fn a_pipeline_with_a_retired_shape_is_told_printed_not_as_a_popup() {
        let (repo, _root_guard) = fixture("popup-retired-shape");
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let retired = "steps:\n  \
                       - id: a\n    agent: pi\n    on_pass: b\n  \
                       - id: b\n    agent: pi\n    loop:\n      a: 1\n    on_pass: z\n    \
                       on_fail: a\n  \
                       - id: z\n    end: true\n";
        std::fs::write(dir.join("default.yml"), retired).unwrap();
        assert!(!asks_as_popup(&repo));

        let fixed = "steps:\n  \
                     - id: a\n    agent: pi\n    on_pass: b\n  \
                     - id: b\n    agent: pi\n    loop: 2\n    on_pass: z\n    on_fail: a\n  \
                     - id: z\n    end: true\n";
        std::fs::write(dir.join("default.yml"), fixed).unwrap();
        assert!(asks_as_popup(&repo));
    }
}
