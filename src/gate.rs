//! The confirm dialog in front of a command that needs a project, once
//! `spoolway update` has installed a newer binary than the one that last
//! brought this checkout's files current.
//!
//! [`crate::sync::stamp_behind`] is the cheap question — one file read and a
//! few hashes — asked before anything else here runs, so a project already
//! current pays nothing extra on every single command. Only once that says
//! yes does this pay for a real [`crate::sync::scan`], and only once *that*
//! finds something does anybody see a panel: a stamp that has moved but a
//! scan that finds nothing to do (every file already hand-matches what the
//! new release would write) has nothing worth interrupting a command for.
//!
//! The split mirrors `commands::dispatch::overrides_gate` / `_with`: a thin
//! wrapper over the process's real stdio and terminal, and an injectable
//! core a test drives over a `Cursor` with `TermGuard::inert` — see
//! `src/commands/dispatch.rs:1614` for the same shape proven out first.
//!
//! Enter is the only key that does anything, and it does two things at
//! once: `spoolway sync` for real, un-dried, and then the command this
//! checkout was actually asked to run. Ctrl-c is the only other exit, and it
//! runs neither — the two are inseparable, because a caller that saw only
//! the files change without the command it asked for would have to notice
//! the confusion for itself.
//!
//! Ctrl-c has no [`crate::screen::Key`] variant of its own: under this
//! project's one raw mode (`crate::platform::TermGuard`, `ISIG` deliberately
//! kept, see its own doc), a real ctrl-c is intercepted by the terminal
//! driver and delivered as `SIGINT`, not as a byte a `read` call ever sees.
//! Left uncaught, the kernel's default disposition would kill this process
//! before `TermGuard`'s own `Drop` ever ran, leaving the terminal in raw
//! mode for whatever shell prompt landed next — so [`confirm_sync_gate`]
//! installs its own handler for the span of the one blocking read below.
//!
//! It is a `sigaction`, not [`crate::platform::stop::catch_interrupt`]'s
//! `signal`: glibc's `signal` installs with `SA_RESTART`, which — a real
//! regression found in review — restarts the blocked `read` underneath
//! `screen::read_key` instead of failing it with `EINTR`, so the interrupt
//! is caught, the flag is set, and the read simply keeps blocking as if
//! nothing happened. `SA_RESTART` off is what actually unblocks it. The
//! flag itself is still [`crate::platform::stop`]'s own shared one — a test
//! drives the same branch by injecting a closure, never by raising a signal
//! or touching that flag. Installed and restored to whatever was there
//! before around this one blocking read alone, so ctrl-c means exactly what
//! it always has in whatever this process runs next, gate or no gate.

use std::io::Write;

use anyhow::Result;

use crate::cli::SyncArgs;
use crate::repo::Repo;
use crate::screen::{self, Key, PollableRead};

/// Every body line, truncated to this many characters before
/// [`screen::panel`] sizes the box around it — so the panel's total width,
/// its two border columns included, never exceeds 80 columns whatever the
/// paths in it are.
const MAX_LINE: usize = 74;

const TITLE: &str = "new version installed, apply updates";
const KEPT_LINE: &str = "Your config values, prompts and task skeletons are kept.";
const KEYS: &str = "[enter] confirm";

/// How a blocking read at the panel ended.
enum Answer {
    /// Enter: write the files for real, then run the command.
    Confirmed,
    /// Ctrl-c: write nothing, run nothing.
    Aborted,
    /// The tty went away mid-question, or nobody was ever going to answer.
    /// Nothing here may hang waiting for an answer that cannot come, so this
    /// runs the command exactly as if the gate had never fired — the same
    /// default `overrides_gate_with` takes for the identical shape.
    JustRun,
}

/// This gate's own `SIGINT` handler for the span of one blocking read —
/// see the module doc for why it cannot be [`crate::platform::stop::
/// catch_interrupt`]'s `signal`. Stores nothing but a stack-only flag; the
/// fact of the interrupt itself is [`crate::platform::stop`]'s own shared
/// one, read back through the `interrupted` closure so a test never has to
/// touch it.
struct SigintGuard {
    previous: libc::sigaction,
    /// An inert guard installs and restores nothing — test-only, the same
    /// reason `TermGuard::inert` exists: a test must never touch the real
    /// process's signal disposition, parallel tests included.
    inert: bool,
}

extern "C" fn record_interrupt(_: libc::c_int) {
    crate::platform::stop::asked_for();
}

impl SigintGuard {
    fn new() -> SigintGuard {
        // SAFETY: `action` and `previous` are plain-old-data structs;
        // `sigemptyset` and `sigaction` are ordinary syscalls against a
        // buffer this function owns for the call's duration.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record_interrupt as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            // No `SA_RESTART`: this handler exists so the blocking `read`
            // underneath `screen::read_key` fails with `EINTR` and returns,
            // not so it silently resumes as if ctrl-c had never happened.
            action.sa_flags = 0;
            let mut previous: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGINT, &action, &mut previous);
            SigintGuard {
                previous,
                inert: false,
            }
        }
    }

    #[cfg(test)]
    fn inert() -> SigintGuard {
        SigintGuard {
            // SAFETY: never installed and never restored — see `inert`.
            previous: unsafe { std::mem::zeroed() },
            inert: true,
        }
    }
}

impl Drop for SigintGuard {
    fn drop(&mut self) {
        if self.inert {
            return;
        }
        // SAFETY: restoring exactly what `new` read off `sigaction` a
        // moment ago, on the same signal, unmodified.
        unsafe {
            libc::sigaction(libc::SIGINT, &self.previous, std::ptr::null_mut());
        }
    }
}

/// The real-stdio wrapper — see the module doc for the split.
pub(crate) fn confirm_sync_gate(repo: &Repo, in_lane: bool, json: bool) -> Result<bool> {
    confirm_sync_gate_with(
        repo,
        in_lane,
        json,
        crate::ask::interactive(),
        &mut screen::RawStdin,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        crate::platform::TermGuard::new,
        SigintGuard::new,
        crate::platform::stop::asked,
    )
}

/// [`confirm_sync_gate`]'s own logic, taking whether anyone is there to
/// answer, where the notice and the panel go, how to take the terminal and
/// the `SIGINT` disposition for the one branch that reads a key, and how to
/// tell a real ctrl-c apart from a `read` that simply ran out of input — so
/// a test can drive every branch without a real terminal, and without ever
/// raising a real signal.
#[allow(clippy::too_many_arguments)]
fn confirm_sync_gate_with(
    repo: &Repo,
    in_lane: bool,
    json: bool,
    interactive: bool,
    input: &mut impl PollableRead,
    out: &mut impl Write,
    err: &mut impl Write,
    term: impl FnOnce() -> crate::platform::TermGuard,
    interrupt: impl FnOnce() -> SigintGuard,
    interrupted: impl Fn() -> bool,
) -> Result<bool> {
    if !crate::sync::stamp_behind(&repo.home, &repo.checkout) {
        return Ok(true);
    }

    let dry = SyncArgs {
        dry_run: true,
        replace: Vec::new(),
    };
    let outcomes = crate::sync::scan(repo, &dry)?;
    let (wrote, removed) = crate::sync::dedup_paths(&outcomes);
    let count = wrote.len() + removed.len();
    if count == 0 {
        return Ok(true);
    }

    // A lane is one of the places the Goal names outright — "a pipe, CI,
    // `--json`, a lane" — where no dialog can be drawn, independently of
    // whether a tty happens to be attached: a command step runs in a real
    // pane, which can carry a real controlling terminal, and a dialog
    // parked there waiting on a key nobody is watching for would hang the
    // step until its own timeout. `release::Audience::wants_notice` treats
    // `in_lane` the same way, as a hard no on its own.
    if !interactive || json || in_lane {
        let mut line = format!("spoolway wants to update: {count} file(s) in this checkout.");
        if !in_lane {
            line.push_str(" Run `spoolway sync`.");
        }
        writeln!(err, "{line}")?;
        return Ok(true);
    }

    print_panel(out, &wrote, &removed)?;

    // Taken only now, right before the first read that can actually block —
    // the same reason `overrides_gate_with` waits this long: every branch
    // above returns without ever touching the cursor or the real `SIGINT`
    // disposition.
    let _term = term();
    let _sigint = interrupt();
    let answer = loop {
        match screen::read_key(input) {
            Some(Key::Enter) => break Answer::Confirmed,
            None if interrupted() => break Answer::Aborted,
            None => break Answer::JustRun,
            _ => {}
        }
    };
    // Before anything more is printed: `sync::run`'s own report is ordinary,
    // non-raw output, and it must land on a terminal already given back —
    // cursor shown, echo restored — not the one this dialog borrowed. The
    // real `SIGINT` disposition goes back the same moment, so whatever runs
    // next sees ctrl-c behave exactly as it always has.
    drop(_sigint);
    drop(_term);

    match answer {
        Answer::Aborted => Ok(false),
        Answer::JustRun => Ok(true),
        Answer::Confirmed => {
            let real = SyncArgs {
                dry_run: false,
                replace: Vec::new(),
            };
            crate::sync::run(repo, &real, json)?;
            Ok(true)
        }
    }
}

/// Truncate `line` to [`MAX_LINE`] characters, with a trailing mark where it
/// was cut — sized for a whole panel row, borders included.
fn fit(line: String) -> String {
    if line.chars().count() <= MAX_LINE {
        return line;
    }
    let head: String = line.chars().take(MAX_LINE - 1).collect();
    format!("{head}…")
}

/// Draw the panel itself: [`TITLE`] reads as a sentence in its own right
/// now, so a blank row separates it from the list rather than running the
/// two together — then what `sync` would write, in the order the mockup
/// draws it: every write, then every removal with its reason on the line
/// under it, then the one sentence that answers "did it eat my config?"
/// before anybody has pressed anything.
fn print_panel(out: &mut impl Write, wrote: &[&str], removed: &[(&str, &str)]) -> Result<()> {
    let mut body = vec![String::new()];
    for path in wrote {
        body.push(fit(format!("{:<6}  {path}", "write")));
    }
    for (path, why) in removed {
        body.push(fit(format!("{:<6}  {path}", "remove")));
        body.push(fit(format!("        ({why})")));
    }
    body.push(String::new());
    body.push(fit(KEPT_LINE.to_string()));

    for line in screen::panel(TITLE, &body, KEYS) {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    // `checkout` and `root` the same path, as `sync`'s own fixture keeps
    // them: `Repo::checkout_note` only shells out to `git branch` once the
    // two differ, and a scratch directory here is no git repository at all.
    fn fixture(name: &str) -> Repo {
        let root = crate::scratch::root(&format!("gate-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(crate::config::TASK_TEMPLATES_DIR)).unwrap();
        std::fs::create_dir_all(root.join(".spoolway/templates")).unwrap();
        let home = root.join(".home");
        std::fs::create_dir_all(&home).unwrap();
        Repo {
            checkout: root.clone(),
            root,
            config: Config::default(),
            home,
        }
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

    fn keys(s: &str) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(s.as_bytes().to_vec())
    }

    /// A project whose stamp was never written at all — a fixture that never
    /// ran `init` or `sync` — must not be nagged: [`crate::sync::
    /// stamp_behind`] has nothing to compare against, so this proceeds
    /// without ever touching `input`, which is left empty on purpose: a
    /// `read_key` call here would hang the test rather than fail it.
    #[test]
    fn no_stamp_at_all_proceeds_without_reading_a_key() {
        let repo = fixture("no-stamp");
        let mut input = keys("");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let proceed = confirm_sync_gate_with(
            &repo,
            false,
            false,
            true,
            &mut input,
            &mut out,
            &mut err,
            crate::platform::TermGuard::inert,
            SigintGuard::inert,
            || false,
        )
        .unwrap();
        assert!(proceed);
        assert!(out.is_empty());
        assert!(err.is_empty());
    }

    /// A stale stamp with nothing for a scan to do — every tracked file
    /// already matches what this binary would write — draws no panel and
    /// proceeds silently. Acceptance criterion: the dialog is for a stamp
    /// *and* a scan that both say so, not either alone.
    #[test]
    fn a_stale_stamp_with_nothing_to_scan_proceeds_silently() {
        let repo = fixture("stale-nothing-to-do");
        make_stale(&repo);
        // A rendered config already in the canonical shape `sync` would
        // write, so `scan`'s own `config()` reports `Kept` rather than a
        // rewrite — the one file this fixture has for it to look at.
        let rendered = Config::default().render().unwrap();
        std::fs::write(Config::path_in(&repo.checkout), rendered).unwrap();

        let mut input = keys("");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let proceed = confirm_sync_gate_with(
            &repo,
            false,
            false,
            true,
            &mut input,
            &mut out,
            &mut err,
            crate::platform::TermGuard::inert,
            SigintGuard::inert,
            || false,
        )
        .unwrap();
        assert!(proceed);
        assert!(out.is_empty(), "{out:?}");
    }

    /// No tty on either end: the one-line notice goes to stderr, names the
    /// count, and the command proceeds without a key ever being read.
    #[test]
    fn no_tty_prints_one_stderr_line_and_proceeds() {
        let repo = fixture("no-tty");
        make_stale(&repo);
        let mut input = keys("");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let proceed = confirm_sync_gate_with(
            &repo,
            false,
            false,
            false,
            &mut input,
            &mut out,
            &mut err,
            crate::platform::TermGuard::inert,
            SigintGuard::inert,
            || false,
        )
        .unwrap();
        assert!(proceed);
        assert!(out.is_empty(), "no cursor drawing on this path: {out:?}");
        let printed = String::from_utf8(err).unwrap();
        assert!(printed.contains("spoolway wants to update:"), "{printed}");
        assert!(printed.contains("Run `spoolway sync`."), "{printed}");
    }

    /// `--json` takes the same stderr path even with both ends a terminal —
    /// something is parsing the real output, and a panel drawn into it would
    /// be exactly the noise `--json` promises never to add.
    #[test]
    fn json_takes_the_stderr_path_even_at_a_tty() {
        let repo = fixture("json-at-tty");
        make_stale(&repo);
        let mut input = keys("");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let proceed = confirm_sync_gate_with(
            &repo,
            false,
            true,
            true,
            &mut input,
            &mut out,
            &mut err,
            crate::platform::TermGuard::inert,
            SigintGuard::inert,
            || false,
        )
        .unwrap();
        assert!(proceed);
        assert!(out.is_empty());
        assert!(!err.is_empty());
    }

    /// Inside a lane, the same notice drops the sentence that names a
    /// command nobody in a lane's worktree is meant to run by hand.
    #[test]
    fn a_lane_omits_the_sync_sentence() {
        let repo = fixture("lane");
        make_stale(&repo);
        let mut input = keys("");
        let mut out = Vec::new();
        let mut err = Vec::new();
        confirm_sync_gate_with(
            &repo,
            true,
            false,
            false,
            &mut input,
            &mut out,
            &mut err,
            crate::platform::TermGuard::inert,
            SigintGuard::inert,
            || false,
        )
        .unwrap();
        let printed = String::from_utf8(err).unwrap();
        assert!(printed.contains("spoolway wants to update:"), "{printed}");
        assert!(!printed.contains("Run `spoolway sync`."), "{printed}");
    }

    /// A command step runs in a real pane, which can carry a real
    /// controlling terminal — `interactive` true — and a lane must still
    /// take the stderr path rather than draw a panel nobody is watching
    /// for: review found this hangs a step until its own timeout otherwise.
    /// `input` is left empty on purpose, so a `read_key` call here would
    /// hang the test rather than fail it.
    #[test]
    fn a_lane_never_draws_the_panel_even_at_a_real_tty() {
        let repo = fixture("lane-at-tty");
        make_stale(&repo);
        let mut input = keys("");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let proceed = confirm_sync_gate_with(
            &repo,
            true,
            false,
            true,
            &mut input,
            &mut out,
            &mut err,
            crate::platform::TermGuard::inert,
            SigintGuard::inert,
            || false,
        )
        .unwrap();
        assert!(proceed);
        assert!(out.is_empty(), "no panel on this path: {out:?}");
        let printed = String::from_utf8(err).unwrap();
        assert!(printed.contains("spoolway wants to update:"), "{printed}");
    }

    /// Enter at the panel writes the files for real, rewrites the stamp, and
    /// says to proceed — the acceptance criterion in full.
    #[test]
    fn enter_confirms_writes_the_files_and_rewrites_the_stamp() {
        let repo = fixture("enter");
        make_stale(&repo);
        let mut input = keys("\r");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let proceed = confirm_sync_gate_with(
            &repo,
            false,
            false,
            true,
            &mut input,
            &mut out,
            &mut err,
            crate::platform::TermGuard::inert,
            SigintGuard::inert,
            || false,
        )
        .unwrap();
        assert!(proceed);
        assert!(
            Config::path_in(&repo.checkout).is_file(),
            "the missing config.toml should have been written for real"
        );
        assert!(
            !crate::sync::stamp_behind(&repo.home, &repo.checkout),
            "the stamp should now match what this binary just wrote"
        );
        // The panel, not `sync::run`'s own report — that one prints straight
        // to the real stdout in production, not through `out`.
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains(TITLE), "{printed}");
        assert!(printed.contains(KEYS), "{printed}");
    }

    /// Ctrl-c writes nothing and says not to proceed — the only other exit,
    /// driven here by a closure standing in for the real interrupt, not by
    /// raising one: `read_key` genuinely runs out of input either way, and
    /// only `interrupted` tells the two apart.
    #[test]
    fn ctrl_c_aborts_and_writes_nothing() {
        let repo = fixture("ctrl-c");
        make_stale(&repo);
        let mut input = keys("");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let proceed = confirm_sync_gate_with(
            &repo,
            false,
            false,
            true,
            &mut input,
            &mut out,
            &mut err,
            crate::platform::TermGuard::inert,
            SigintGuard::inert,
            || true,
        )
        .unwrap();
        assert!(!proceed);
        assert!(
            !Config::path_in(&repo.checkout).is_file(),
            "ctrl-c must write nothing"
        );
    }

    /// No other key does anything: a stray character is read and dropped,
    /// and the loop keeps waiting for Enter or the interrupt.
    #[test]
    fn a_stray_key_does_nothing_and_enter_still_confirms() {
        let repo = fixture("stray-key");
        make_stale(&repo);
        let mut input = keys("q\r");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let proceed = confirm_sync_gate_with(
            &repo,
            false,
            false,
            true,
            &mut input,
            &mut out,
            &mut err,
            crate::platform::TermGuard::inert,
            SigintGuard::inert,
            || false,
        )
        .unwrap();
        assert!(proceed);
        assert!(Config::path_in(&repo.checkout).is_file());
    }

    /// The panel's width is bounded even when a path in it is not: every
    /// row `screen::boxed` draws — borders included — stays at or under 80
    /// columns.
    #[test]
    fn the_panel_is_at_most_eighty_columns_wide() {
        let long = "a/very/long/path/".repeat(6) + "SKILL.md";
        let wrote = [long.as_str()];
        let mut out = Vec::new();
        print_panel(&mut out, &wrote, &[]).unwrap();
        let printed = String::from_utf8(out).unwrap();
        for line in printed.lines() {
            assert!(
                line.chars().count() <= 80,
                "{} columns: {line}",
                line.chars().count()
            );
        }
    }
}
