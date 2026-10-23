//! `spoolway sync` — take what a newer spoolway writes, keep what you wrote.
//!
//! `init` has only ever had two answers for a file that already exists: leave it
//! alone, or overwrite it. Neither is what bringing a project current means. A
//! project that keeps everything runs last year's contract against this year's
//! binary; a project that runs `init --force` gets the new one and loses every
//! prompt it has written since.
//!
//! So this replaces, and it never merges. For a task skeleton or a page template
//! that means one delimited region, generated from [`crate::block`], swapped for
//! the freshly rendered version — every byte around it is copied through without
//! being read. Anything a person has changed is reported and left exactly as it
//! is: this command's failure mode has to be "did nothing and said so", never
//! "helped". [`config`], [`pipelines`] and [`skills`] keep it too, in the part
//! of their file that is spoolway's: a skill file, a pipeline's key block and
//! `config.toml`'s reference table are refused when they have been edited. The
//! rest of `config.toml` is still spoolway's to rewrite, as the paragraph
//! below says.
//!
//! Prompts, their assets, and task skeletons are not here at all, and that is
//! the design. All are prose a project owns outright, with nothing generated
//! inside them to keep current: what a lane is told about finishing comes from
//! the opening prompt, and a task's frontmatter is serialised from a struct in
//! the binary on every save. Syncing spoolway therefore cannot disturb a word
//! a project wrote in any of them, and `spoolway pipeline check` is what
//! catches a command name that has fallen behind the CLI.
//!
//! Nothing is written while the files are being read. Each step below only
//! records what it would write or remove, and [`scan`] checks that every one
//! of them can be made before it makes the first. A file that cannot is
//! named and the sync writes nothing, because a failure halfway leaves some
//! files new and some old, and a `config.toml` this binary rewrote is one an
//! older teammate's spoolway cannot read. A dry run stops after the planning,
//! so it reports exactly what a real run would do.
//!
//! Everything below writes `repo.checkout`, never `repo.root`: the control
//! plane this command refreshes is tracked, so a lane running in a linked
//! worktree has its own branch's copies, and a sync taken there has to
//! land on that branch — where it is reviewed and merged — rather than on
//! whatever the main checkout happens to have out. `config set` and
//! `override promote` follow the same rule through
//! `Repo::tracked_write_root`, which also refuses a dispatcher lane's
//! worktree and one on a detached HEAD. This command refuses in a worktree whose branch has no
//! `.spoolway/` at all. Its setup is the main checkout's, so there is no
//! branch copy to refresh, and writing one would start a setup the branch
//! never had. That case is refused, naming the main checkout.
//!
//! The private override layer is the one file in the project home this command
//! writes, and the plan pages below are its other writes outside the checkout.
//! A lane leaves the layer alone: a retired key stays there and the
//! report says so, because the layer is the person's and sits in no git
//! branch to revert from.
//!
//! In a home-mode clone, sync also moves the clone's plan pages into the
//! workspace and relinks the tasks that name them. That is
//! [`crate::sync_plans`]'s work and it happens in [`run`] only: it writes
//! outside the checkout, under the workspace, and [`scan`] stays the read of
//! what spoolway owns that `init`, `install` and `doctor` also call.
//!
//! A prompt's assets are the document skeletons, and they were a page template
//! until they moved under the archivist. That move was the whole point: a
//! skeleton spoolway keeps current is one a project cannot restructure, and
//! restructuring documentation is exactly what a project should be able to do.
//!
//! `config.toml` is the file that works the other way round, and [`config`]
//! says why: there the values are the project's and everything around them —
//! which settings are written down, and every comment but the reference table
//! — is spoolway's, and is rewritten from the binary. Only the reference
//! table is checked against the copies spoolway has shipped. A pipeline file's
//! key reference is the same bargain in one fenced block of an otherwise
//! untouched file; [`pipelines`] says why it is documentation of the binary's
//! contract rather than anything a project meant. A skill file is the
//! whole-file version of the same bargain, and [`skills`] (and [`user_level`],
//! for the agent's user folder) keeps it.
//!
//! The bargain has one condition, decided per file by [`crate::known_copies`].
//! A skill file, key block or config reference table equal to this binary's
//! copy is kept. One equal to a copy an older spoolway shipped is stale and is
//! rewritten. One equal to nothing any spoolway shipped was written by a newer
//! spoolway or edited by hand, and it is refused, naming the file and `spoolway
//! sync --replace <path>`, which takes this binary's copy over it and saves
//! the file as a `.bak` first. `init` and `install` make the same call for the
//! skills they write.
//!
//! This used to be `spoolway update`'s job, along with installing the binary
//! itself. The two were split apart so `update` can run from any directory,
//! including one with no project in it: installing a binary is a fact about
//! this machine's `PATH`, and file work is a fact about a checkout that has
//! to exist first. See [`crate::update`] for the half that stayed there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::SyncArgs;
use crate::known_copies::{Known, Shipped, lookup, refusal};
use crate::repo::Repo;
use crate::task::write_atomic;

/// What happened to one file, in the order a person wants to read it.
///
/// The path and the reason are separate fields because they read in
/// different places: the report prints a written file's path alone and puts
/// the reason under it only for a [`Outcome::Migrated`], while a refusal
/// always prints with its reason, ahead of everything else — a refusal that
/// is never said is a project that quietly stays behind. `spoolway doctor`
/// reads every reason.
pub enum Outcome {
    Wrote {
        path: String,
        detail: String,
    },
    /// A note about a file this run changed, spelled out in the report: the
    /// settings it added, what dropping a retired setting means for the
    /// project — see [`config`]'s own `issue_tracking.on_fail` and
    /// `dispatch.worktree_root` notes — a value `sync` set that the file never
    /// named, unknown settings it kept, or a pipeline key block it replaced. Its own
    /// variant rather than another [`Outcome::Wrote`], because its detail is
    /// one of the few this
    /// report actually prints, after every file line, in both the
    /// long form (`report`) `run`'s own report and `--dry-run` use and the
    /// short one (`panel`) that fits [`run_asking`]'s bounded confirm line —
    /// an ordinary `Wrote`'s `detail` is never shown this way, and giving it
    /// two forms besides would only invite it to grow long enough to need
    /// them.
    Migrated {
        path: String,
        report: String,
        panel: String,
    },
    /// Already ours, or already current. Carries no path because nothing needs
    /// one: a file nothing was done to is a file nothing has to say about it,
    /// and naming it was how the old report came to be mostly `kept` lines.
    Kept,
    /// Ours, and changed by hand. The only outcome that is a refusal.
    Blocked {
        path: String,
        why: String,
    },
    /// A stale skill directory or template file this project no longer ships
    /// — [`crate::install::RETIRED_SKILLS`] or [`crate::install::
    /// RETIRED_TEMPLATES`], and nothing else — removed outright rather than
    /// rewritten.
    Removed {
        path: String,
        why: String,
    },
}

impl Outcome {
    fn wrote(path: impl Into<String>, detail: impl Into<String>) -> Outcome {
        Outcome::Wrote {
            path: path.into(),
            detail: detail.into(),
        }
    }
    fn blocked(path: impl Into<String>, why: impl Into<String>) -> Outcome {
        Outcome::Blocked {
            path: path.into(),
            why: why.into(),
        }
    }
    fn removed(path: impl Into<String>, why: impl Into<String>) -> Outcome {
        Outcome::Removed {
            path: path.into(),
            why: why.into(),
        }
    }
    fn migrated(
        path: impl Into<String>,
        report: impl Into<String>,
        panel: impl Into<String>,
    ) -> Outcome {
        Outcome::Migrated {
            path: path.into(),
            report: report.into(),
            panel: panel.into(),
        }
    }
}

/// The one line that says what the paths above it do not.
///
/// Without it the report is a list of files with no statement of what
/// happened to them, and the thing people actually worry about — "did it eat
/// my config?" — goes unanswered.
const KEPT: &str =
    "Files were overwritten; your config values, prompts and task skeletons were kept.";

/// The same line for a dry run, which has to say the opposite: the paths
/// above it are what a sync *would* take, and nothing was touched.
const DRY_RUN: &str = "Dry run: nothing was written. Run without --dry-run to take it.";

/// What a sync says when the scan wrote or removed nothing, dry run or not:
/// `KEPT` talks about files being overwritten and `DRY_RUN` about what a
/// write *would* do, both of which are false when there was nothing to do
/// at all and would otherwise read as the tool telling a project to re-run
/// a command that would do the exact same nothing.
const NOOP: &str = "Nothing updating.";

/// What a sync says when it refused a file and wrote nothing else. A refused
/// file is not necessarily behind: a newer spoolway may have written it, or a
/// person edited it, so the line says only what was done to it.
const REFUSED_ONLY: &str = "Nothing else updating; the refused files above were left as they are.";

/// What a run refused, as far as its closing line cares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Refused {
    Nothing,
    /// At least one file was refused.
    Files,
}

impl Refused {
    fn of(refused: &[(&str, &str)]) -> Refused {
        match refused.is_empty() {
            true => Refused::Nothing,
            false => Refused::Files,
        }
    }
}

/// Which of the closing lines above a run prints, kept as its own
/// pure function so a test can hold it to its text without capturing
/// stdout: `nothing_to_do` wins over dry run, since neither `DRY_RUN` nor
/// `KEPT` is true of a run that touched nothing, and a refusal beats
/// `NOOP`, since "Nothing updating." under a refusal would read as the
/// project being current.
fn closing_line(dry_run: bool, nothing_to_do: bool, refused: Refused) -> &'static str {
    match (dry_run, nothing_to_do, refused) {
        (_, true, Refused::Files) => REFUSED_ONLY,
        (_, true, Refused::Nothing) => NOOP,
        (true, false, _) => DRY_RUN,
        (false, false, _) => KEPT,
    }
}

/// The closing line of a run, which has to know whether it also seeded, moved
/// or refused plan pages. The plan lines say what they did, and `KEPT` and
/// `NOOP` speak of config files, so a run whose only changes were plan pages
/// closes with the dry run's line or none at all. A refused plan page picks
/// the refused-only line like any other refusal.
fn closing(
    dry_run: bool,
    nothing_else: bool,
    refused: Refused,
    plans_changed: bool,
) -> Option<&'static str> {
    if nothing_else && plans_changed && refused == Refused::Nothing {
        return dry_run.then_some(DRY_RUN);
    }
    Some(closing_line(dry_run, nothing_else, refused))
}

pub fn run(repo: &Repo, args: &SyncArgs) -> Result<()> {
    if repo.borrowed {
        bail!(
            "this linked worktree has no `.spoolway/` on its branch, so it reads the setup of {} \
             — `sync` would start a second setup in the worktree\n  run `spoolway sync` in {} \
             instead",
            repo.checkout.display(),
            repo.checkout.display(),
        );
    }

    // Held from the first read of the tracked files to the last write, and
    // taken here once: `scan` reads the config and `config set` rewrites it,
    // so a `set` that landed in between is rebuilt away by this run's own
    // write while its caller has already been told it succeeded. Nothing
    // below takes this lock again, and the confirm panel of
    // [`run_asking`] sits outside this function so a person reading it holds
    // nobody up.
    let _lock = crate::lock::EditLock::acquire(&repo.edit_lock_path())?;

    if !args.replace.is_empty() {
        return replace(repo, args);
    }

    // Printed here, not below: this project's own files are what the note is
    // about, and a lane running in a linked worktree needs to know which
    // checkout is about to be rewritten before it reads the report.
    if let Some(note) = repo.checkout_note()? {
        note.print(false)?;
        println!();
    }

    // Deduped, because one file can be written for several reasons at once —
    // a config gains a setting and drops a retired one in the same rewrite —
    // and a path printed twice reads as two files. Kept as two lists rather
    // than one: a directory removed outright (a stale skill renamed out from
    // under a project) has to read as removed, not as a file this pass
    // wrote, or the person reading it would think a deletion was a rewrite.
    let outcomes = scan(repo, args)?;
    let (wrote, removed) = dedup_paths(&outcomes);
    let notes = migration_notes(&outcomes);
    let refused = refusals(&outcomes);
    // Refusals print before everything else, so the line that matters is never
    // below a long list of ordinary writes. A dry run says them too: the file
    // is refused either way.
    // Columns as the report is drawn: the word padded so the path starts one
    // column past the longest of the set. A dry run's words are longer, so it
    // pads wider.
    let (refused_word, wrote_word, removed_word) = match args.dry_run {
        true => ("refused      ", "would write  ", "would remove "),
        false => ("refused ", "wrote   ", "removed "),
    };
    for (path, why) in &refused {
        println!("{refused_word}{path} — {why}");
    }
    // A dry run reports the same paths in the conditional: "wrote" over a
    // tree nothing touched reads as a lie the moment `git status` is run.
    for path in &wrote {
        println!("{wrote_word}{path}");
    }
    for (path, _) in &removed {
        println!("{removed_word}{path}");
    }
    // The parenthesised notes come after every file line, so the files read as
    // one list and the explanations as another.
    for path in &wrote {
        for (report, _) in notes.get(path).into_iter().flatten() {
            println!("({report})");
        }
    }
    // Read the way `doctor` reads it, so both name the same steps. Quietly,
    // because a stale override is `doctor`'s to report. A load that fails
    // drops only the note: the note writes no file, and must never change
    // sync's exit code.
    let pipelines = crate::pipeline::Pipelines::load_quietly(&repo.checkout, &repo.config);
    if let Some(sentences) = pipelines
        .ok()
        .as_ref()
        .and_then(crate::commands::stack_deprecation)
    {
        for line in stack_note_lines(&sentences) {
            println!("{line}");
        }
    }

    // After the file lines, and before the closing line, which has to know
    // whether this also seeded, moved or refused a plan page.
    let plans = crate::sync_plans::run(repo, args.dry_run)?;

    println!();
    let nothing_else = wrote.is_empty() && removed.is_empty();
    let refused = match plans.refused {
        true => Refused::Files,
        false => Refused::of(&refused),
    };
    if let Some(line) = closing(args.dry_run, nothing_else, refused, plans.changed) {
        println!("{line}");
    }

    if !args.dry_run {
        remove_skill_stamp(&repo.home);
    }
    Ok(())
}

/// The deprecation note as `sync` prints it: under its own `note` word, so it
/// never reads as a file line that wrote or refused something, and each
/// sentence on a line of its own, wrapped to [`MAX_LINE`] and indented under
/// the first.
fn stack_note_lines(sentences: &[String]) -> Vec<String> {
    let mut lines = Vec::new();
    for sentence in sentences {
        for line in crate::screen::wrap(sentence, MAX_LINE) {
            let word = if lines.is_empty() { "note" } else { "" };
            lines.push(format!("{word:<8}{line}"));
        }
    }
    lines
}

/// What `spoolway sync` says when the panel was answered with esc or ctrl-c.
const CANCELLED: &str = "Nothing was changed.";

/// Every body line, truncated to this many characters before
/// [`crate::screen::panel`] sizes the box around it — so the panel's total
/// width, its two border columns included, never exceeds 80 columns whatever
/// the paths in it are. The deprecation note's text wraps to this width too,
/// after its 8-column `note` gutter, so one of its lines can reach 82
/// columns: the width the note is drawn at.
const MAX_LINE: usize = 74;

/// The confirm panel's title.
pub(crate) const TITLE: &str = "new version installed, apply updates";
const KEPT_LINE: &str = "Your config values, prompts and task skeletons are kept.";

/// Truncate `line` to [`MAX_LINE`] characters, with a trailing mark where it
/// was cut — sized for a whole panel row, borders included.
fn fit(line: String) -> String {
    if line.chars().count() <= MAX_LINE {
        return line;
    }
    let head: String = line.chars().take(MAX_LINE - 1).collect();
    format!("{head}…")
}

/// The rows inside the confirm panel: a blank row under [`TITLE`], then every
/// refused file with its reason, then what `sync` would write — every write, with a retired-shape migration's own
/// short note under it where `notes` carries one, then every removal with
/// its reason on the line under it, then the one sentence that answers "did
/// it eat my config?" before anybody has pressed anything.
fn panel_body(
    refused: &[(&str, &str)],
    wrote: &[&str],
    notes: &BTreeMap<&str, Vec<(&str, &str)>>,
    removed: &[(&str, &str)],
) -> Vec<String> {
    let mut body = vec![String::new()];
    for (path, why) in refused {
        body.push(fit(format!("{:<7} {path}", "refused")));
        // Wrapped, not cut: the fix a refusal names is at the end of its
        // reason, and declining the panel is the only other place to read it.
        for line in crate::screen::wrap(&format!("({why})"), MAX_LINE - 8) {
            body.push(format!("        {line}"));
        }
    }
    for path in wrote {
        body.push(fit(format!("{:<6}  {path}", "write")));
        for (_, panel) in notes.get(path).into_iter().flatten() {
            body.push(fit(format!("        ({panel})")));
        }
    }
    for (path, why) in removed {
        body.push(fit(format!("{:<6}  {path}", "remove")));
        body.push(fit(format!("        ({why})")));
    }
    body.push(String::new());
    body.push(fit(KEPT_LINE.to_string()));
    body
}

/// The confirm panel's own `SIGINT` handler for the span of its one blocking
/// read.
///
/// Ctrl-c has no [`crate::screen::Key`] variant of its own: under this
/// project's one raw mode (`crate::platform::TermGuard`, `ISIG` deliberately
/// kept, see its own doc), a real ctrl-c is intercepted by the terminal
/// driver and delivered as `SIGINT`, not as a byte a `read` call ever sees.
/// Left uncaught, the kernel's default disposition would kill this process
/// before `TermGuard`'s own `Drop` ever ran, leaving the terminal in raw
/// mode for whatever shell prompt landed next.
///
/// It is a `sigaction`, not [`crate::platform::stop::catch_interrupt`]'s
/// `signal`: glibc's `signal` installs with `SA_RESTART`, which — a real
/// regression found in review — restarts the blocked `read` underneath
/// `screen::read_key` instead of failing it with `EINTR`, so the interrupt
/// is caught, the flag is set, and the read simply keeps blocking as if
/// nothing happened. `SA_RESTART` off is what actually unblocks it. The
/// flag itself is [`crate::platform::stop`]'s own shared one. Installed and
/// restored to whatever was there before around the one blocking read alone,
/// so ctrl-c means exactly what it always has in whatever this process runs
/// next.
pub(crate) struct SigintGuard {
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
    pub(crate) fn new() -> SigintGuard {
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
    pub(crate) fn inert() -> SigintGuard {
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

/// How the confirm panel in front of a real sync ended.
enum Answer {
    /// Enter: write the files and print [`run`]'s own report.
    Apply,
    /// Esc, ctrl-c, or the terminal going away mid-question: write nothing.
    /// A read that ran out of input is a cancel rather than a yes: the only
    /// thing enter would do is write, and this module's failure mode is "did
    /// nothing and said so".
    Cancel,
}

/// `spoolway sync` as a person runs it: the files it is about to write or
/// remove, drawn in a panel, and [`run`] only once enter says so. `run`
/// itself never asks: this is the one place a person answers, and `--replace`,
/// a script and a lane all reach `run` without a question.
pub(crate) fn run_asking(repo: &Repo, args: &SyncArgs, in_lane: bool) -> Result<()> {
    run_asking_with(
        repo,
        args,
        in_lane,
        crate::ask::interactive(),
        &mut crate::screen::RawStdin,
        &mut std::io::stdout(),
        crate::platform::TermGuard::new,
        SigintGuard::new,
    )
}

/// [`run_asking`]'s own logic over injected input, output, terminal and
/// `SIGINT` handling, so a test can drive every branch without a real
/// terminal and without ever raising a real signal.
#[allow(clippy::too_many_arguments)]
fn run_asking_with(
    repo: &Repo,
    args: &SyncArgs,
    in_lane: bool,
    interactive: bool,
    input: &mut impl crate::screen::PollableRead,
    out: &mut impl std::io::Write,
    term: impl FnOnce() -> crate::platform::TermGuard,
    interrupt: impl FnOnce() -> SigintGuard,
) -> Result<()> {
    // Nobody to answer, or nothing to ask about: a script (the e2e suites run
    // `sync` with no terminal) and a lane — whose pane can carry a
    // real terminal nobody is watching, so a panel parked there would hang
    // the step until its own timeout — write straight away. `--dry-run` writes nothing to ask
    // about, and `--replace` names its files on the command line already.
    if !interactive || in_lane || args.dry_run || !args.replace.is_empty() {
        return run(repo, args);
    }
    let dry = SyncArgs {
        dry_run: true,
        replace: Vec::new(),
    };
    let outcomes = scan(repo, &dry)?;
    let (wrote, removed) = dedup_paths(&outcomes);
    if wrote.is_empty() && removed.is_empty() {
        return run(repo, args);
    }
    let notes = migration_notes(&outcomes);
    let keys = crate::screen::keys(&[("enter", "apply"), ("esc", "cancel")]);
    let body = panel_body(&refusals(&outcomes), &wrote, &notes, &removed);
    for line in crate::screen::panel(TITLE, &body, &keys) {
        writeln!(out, "{line}")?;
    }

    // Taken only around the one read that blocks, and given back before
    // anything else prints — `run`'s report is ordinary output and must land
    // on a terminal with its cursor and echo restored.
    let answer = {
        let _term = term();
        let _sigint = interrupt();
        loop {
            match crate::screen::read_key(input) {
                Some(crate::screen::Key::Enter) => break Answer::Apply,
                // Ctrl-c arrives as `SIGINT`, never as a byte — see
                // [`SigintGuard`] — so it lands here as the read failing
                // (`None`), the same as input running out. Both mean write
                // nothing.
                Some(crate::screen::Key::Esc) | None => break Answer::Cancel,
                _ => {}
            }
        }
    };
    match answer {
        Answer::Apply => run(repo, args),
        Answer::Cancel => {
            writeln!(out, "{CANCELLED}")?;
            Ok(())
        }
    }
}

/// The paths worth naming out of a scan, deduplicated: one file can be
/// behind for several reasons at once — a config gains a setting and drops a
/// retired one in the same rewrite — and a path printed twice reads as two
/// files. Shared by [`run`]'s own report, [`run_asking`]'s panel, which
/// draws the same two lists before either has run for real, and
/// `crate::gate`, which only asks whether either list has anything in it.
pub(crate) fn dedup_paths(outcomes: &[Outcome]) -> (Vec<&str>, Vec<(&str, &str)>) {
    let mut wrote: Vec<&str> = Vec::new();
    let mut removed: Vec<(&str, &str)> = Vec::new();
    for outcome in outcomes {
        match outcome {
            Outcome::Wrote { path, .. } | Outcome::Migrated { path, .. }
                if !wrote.contains(&path.as_str()) =>
            {
                wrote.push(path)
            }
            Outcome::Removed { path, why } if !removed.iter().any(|(p, _)| *p == path) => {
                removed.push((path, why))
            }
            Outcome::Wrote { .. }
            | Outcome::Migrated { .. }
            | Outcome::Removed { .. }
            | Outcome::Kept
            | Outcome::Blocked { .. } => {}
        }
    }
    (wrote, removed)
}

/// Every refused file in a scan, once each, with the reason: what [`run`]'s
/// report prints before anything else. Kept apart from [`dedup_paths`], whose two lists are
/// the files a sync changes — a refused file is one it did not.
pub(crate) fn refusals(outcomes: &[Outcome]) -> Vec<(&str, &str)> {
    let mut refused: Vec<(&str, &str)> = Vec::new();
    for outcome in outcomes {
        if let Outcome::Blocked { path, why } = outcome
            && !refused.iter().any(|(p, _)| *p == path)
        {
            refused.push((path, why));
        }
    }
    refused
}

/// The parenthesised notes a scan's own [`Outcome::Migrated`] entries
/// carry, keyed by path and kept in the order they were recorded — the
/// extra explanation [`run`]'s own report draws below the file lines and
/// [`run_asking`]'s confirm panel under the file's own row, one tuple of
/// `(report, panel)` per change so each surface reads the length it can
/// afford.
pub(crate) fn migration_notes(outcomes: &[Outcome]) -> BTreeMap<&str, Vec<(&str, &str)>> {
    let mut notes: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for outcome in outcomes {
        if let Outcome::Migrated {
            path,
            report,
            panel,
        } = outcome
        {
            notes
                .entry(path.as_str())
                .or_default()
                .push((report.as_str(), panel.as_str()));
        }
    }
    notes
}

/// The `detail` a [`Outcome::Wrote`] carries for a file that was not there
/// at all — named so `doctor` can tell one apart from a file that was there
/// and out of date, which is a different thing to advise about.
pub const MISSING: &str = "was missing";

/// One write or removal a sync has worked out, not yet made.
///
/// Every file step above only reads the project and records one of these, so
/// the whole set is known before any of it happens. That is what lets [`check`]
/// refuse a sync that could not finish, and lets a dry run and a real run
/// print the same list: they are the same plan, and only the real run goes on
/// to [`apply`] it.
enum Act {
    /// The whole new contents of a file, through [`write_atomic`].
    Write {
        path: PathBuf,
        shown: String,
        contents: String,
    },
    /// A retired template file.
    RemoveFile { path: PathBuf, shown: String },
    /// A retired skill directory, and everything in it.
    RemoveDir { path: PathBuf, shown: String },
    /// A directory a move may have emptied, removed only if it is empty.
    /// Never checked and never listed: `remove_dir` refusing it means a
    /// project's own files are in it, which is the outcome wanted.
    Tidy(PathBuf),
}

impl Act {
    fn write(path: PathBuf, shown: &str, contents: impl Into<String>) -> Act {
        Act::Write {
            path,
            shown: shown.to_string(),
            contents: contents.into(),
        }
    }
    fn remove_file(path: PathBuf, shown: &str) -> Act {
        Act::RemoveFile {
            path,
            shown: shown.to_string(),
        }
    }
    fn remove_dir(path: PathBuf, shown: &str) -> Act {
        Act::RemoveDir {
            path,
            shown: shown.to_string(),
        }
    }

    /// The path as the report names it; `None` for a [`Act::Tidy`], which the
    /// report never names.
    fn shown(&self) -> Option<&str> {
        match self {
            Act::Write { shown, .. }
            | Act::RemoveFile { shown, .. }
            | Act::RemoveDir { shown, .. } => Some(shown),
            Act::Tidy(_) => None,
        }
    }
}

/// An `io::Error` as a person reads it: without the `(os error 13)` tail the
/// platform adds to its own wording.
fn plain(err: &std::io::Error) -> String {
    let text = err.to_string();
    match text.split_once(" (os error") {
        Some((head, _)) => head.to_string(),
        None => text,
    }
}

/// Whether a new file can be created in `dir`, found by creating one and
/// taking it away again. Permission bits cannot answer this: root ignores
/// them, and an access list or a read-only mount says no with them set.
///
/// A probe left behind by a sync killed mid-check must not make the next one
/// refuse, and a process id is reused, so a name already taken is skipped for
/// the next one rather than reported as a failure.
fn can_create_in(dir: &Path) -> std::io::Result<()> {
    let pid = std::process::id();
    for attempt in 0.. {
        let probe = dir.join(format!(".spoolway-probe-{pid}-{attempt}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                return Ok(());
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && attempt < 100 => {}
            Err(err) => return Err(err),
        }
    }
    unreachable!("the loop returns on every path")
}

/// Whether `path` can be created, or replaced, without making anything that
/// stays: the nearest directory that exists must take a new file. The write
/// goes through a temporary file beside its target and a rename, so that is
/// the permission it needs, whatever the target's own mode is.
fn can_write(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        return Err(std::io::Error::other("is a directory"));
    }
    let mut dir = path.parent().unwrap_or_else(|| Path::new("."));
    while !dir.exists() {
        match dir.parent() {
            Some(up) => dir = up,
            None => break,
        }
    }
    if !dir.is_dir() {
        return Err(std::io::Error::other("a parent of it is not a directory"));
    }
    can_create_in(dir)
}

/// Whether `path` can be removed: its parent must take a change, and for a
/// directory so must every directory under it, since each loses entries.
fn can_remove(path: &Path, tree: bool) -> std::io::Result<()> {
    can_create_in(path.parent().unwrap_or_else(|| Path::new(".")))?;
    if !tree {
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            can_remove(&entry.path(), true)?;
        }
    }
    can_create_in(path)
}

/// Every action in `acts` that cannot be made, as one line each.
///
/// Fails the whole sync, before the first write, naming each file: a failure
/// halfway leaves a project with some files new and some old, and a
/// `config.toml` rewritten by this binary is one an older teammate's cannot
/// read back.
fn check(acts: &[Act]) -> Result<()> {
    let mut lines = Vec::new();
    for act in acts {
        let (verb, shown, result) = match act {
            Act::Write { path, shown, .. } => ("write", shown, can_write(path)),
            Act::RemoveFile { path, shown } => ("remove", shown, can_remove(path, false)),
            Act::RemoveDir { path, shown } => ("remove", shown, can_remove(path, true)),
            Act::Tidy(_) => continue,
        };
        if let Err(err) = result {
            lines.push(format!("cannot {verb} {shown}: {}", plain(&err)));
        }
    }
    if lines.is_empty() {
        return Ok(());
    }
    bail!(
        "{}\nnothing was written — fix what each line names and run sync again",
        lines.join("\n")
    )
}

/// Make every action in `acts`, in order.
///
/// [`check`] has already said each can be made, but a file can still change
/// between that and now. A failure here stops the sync and lists every file
/// already changed, since nothing is rolled back and the project is part-way.
fn apply(acts: Vec<Act>) -> Result<()> {
    let mut changed: Vec<String> = Vec::new();
    for act in acts {
        let result = match &act {
            Act::Write { path, contents, .. } => write_atomic(path, contents),
            Act::RemoveFile { path, .. } => {
                std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
            }
            Act::RemoveDir { path, .. } => std::fs::remove_dir_all(path)
                .with_context(|| format!("removing {}", path.display())),
            Act::Tidy(path) => {
                let _ = std::fs::remove_dir(path);
                Ok(())
            }
        };
        if let Err(err) = result {
            let listed = match changed.is_empty() {
                true => "nothing else was changed".to_string(),
                false => format!(
                    "already changed, and not undone:\n  {}",
                    changed.join("\n  ")
                ),
            };
            let failed = match &act {
                Act::Write { shown, .. } => format!("could not write {shown}"),
                Act::RemoveFile { shown, .. } | Act::RemoveDir { shown, .. } => {
                    format!("could not remove {shown}")
                }
                Act::Tidy(_) => unreachable!("tidying a directory never fails"),
            };
            bail!("{failed}: {err:#}\n{listed}");
        }
        if let Some(shown) = act.shown() {
            changed.push(shown.to_string());
        }
    }
    Ok(())
}

/// Every file spoolway owns here, and what would happen to it — and, unless
/// `args.dry_run`, doing it.
///
/// Split out from [`run`] so that `spoolway doctor` and the "Run spoolway
/// sync" notice can ask the same question without printing anything.
///
/// Every write and removal is worked out first, by files that only read; the
/// real run then checks all of them can be made, and only then makes any. A
/// dry run stops after working them out, so it reports the very list a real
/// run would act on.
pub fn scan(repo: &Repo, args: &SyncArgs) -> Result<Vec<Outcome>> {
    let (outcomes, acts) = plan(repo)?;
    if !args.dry_run {
        check(&acts)?;
        apply(acts)?;
    }
    Ok(outcomes)
}

/// What a sync would report and what it would write, with nothing written.
fn plan(repo: &Repo) -> Result<(Vec<Outcome>, Vec<Act>)> {
    // Mirrors `commands::init`'s own `home_mode` (see `src/commands/init.rs`,
    // and `Command::Install` in `src/main.rs`, which keys the same question
    // off `repo.root` for the same reason): home mode promises nothing is
    // written into the checkout, and `config`, `templates`,
    // `retired_templates` and `pipelines` already keep that promise on
    // their own, since every one of them resolves its path through
    // `crate::config::setup_dir_in`, which a home-mode `Repo` always answers
    // with the workspace's own `config/` — `Repo::discover` refuses a
    // checkout that carries both a tracked `.spoolway/` and a workspace
    // entry, so that indirection can never land back on the checkout here.
    // `templates` does the same for a skeleton spelled under `.spoolway/`
    // (see `skeleton_path`) and skips one spelled anywhere else, which
    // would land in the checkout. `ignores` and the project-level half of
    // `skills` join `repo.checkout` directly instead, with no such
    // indirection, so they are the two steps that need telling.
    let home_mode = crate::repo::workspace_clone(&repo.root).is_some();
    let mut outcomes = Vec::new();
    let mut acts = Vec::new();
    ignores(repo, home_mode, &mut outcomes, &mut acts)?;
    config(repo, &mut outcomes, &mut acts)?;
    templates(repo, &mut outcomes, &mut acts)?;
    skills(repo, home_mode, &mut outcomes, &mut acts)?;
    retired_skills(repo, home_mode, &mut outcomes, &mut acts)?;
    retired_templates(repo, &mut outcomes, &mut acts)?;
    retired_markers(repo, &mut outcomes, &mut acts);
    pipelines(repo, &mut outcomes, &mut acts)?;
    override_pipelines(repo, &mut outcomes, &mut acts);
    private_pipelines(repo, &mut outcomes, &mut acts);
    user_level(&mut outcomes, &mut acts)?;
    Ok((outcomes, acts))
}

/// The file builds of 0.10.0 wrote to record which spoolway last synced a
/// scope: one `X.Y.Z` line. The project's copy sat in the setup folder, and
/// each user-level skills folder held a hidden one, since every project on the
/// machine shares that folder.
///
/// Nothing reads it any more: whether a file is stale, or newer than this
/// binary, is decided per file by [`crate::known_copies`]. It stays named here
/// only so `sync`, `init` and `install` can delete the copies still on disk.
const RETIRED_MARKER: &str = "spoolway-version";

/// The hidden spelling of [`RETIRED_MARKER`] inside a user-level skills
/// folder, where a visible file would have sat among the skill directories.
const RETIRED_USER_MARKER: &str = ".spoolway-version";

/// The retired marker in a project's setup folder, when there is one, and the
/// way a report spells it: relative to the checkout, or with `~` outside it.
/// A home-mode project's setup folder is the workspace's, which is outside the
/// checkout, and `relative` would answer its full absolute path there where
/// every other path `init` and `install` print spells a file under the home
/// directory with `~`.
pub(crate) fn project_marker(checkout: &Path) -> Option<(PathBuf, String)> {
    let marker = crate::config::setup_dir_in(checkout).join(RETIRED_MARKER);
    let shown = match marker.starts_with(checkout) {
        true => crate::platform::relative(checkout, &marker),
        false => crate::repo::shorten_home(&marker),
    };
    marker.is_file().then_some((marker, shown))
}

/// The retired marker in a user-level skills folder, when there is one, with
/// the `~` spelling a report gives it.
pub(crate) fn user_marker(skills_dir: &Path) -> Option<(PathBuf, String)> {
    let marker = skills_dir.join(RETIRED_USER_MARKER);
    let shown = crate::repo::shorten_home(&marker);
    marker.is_file().then_some((marker, shown))
}

/// The reason `sync` gives beside a marker it removes. `init` and `install`
/// print a bare `removed` row instead, as they do for every file they take away.
const MARKER_GONE: &str = "no longer written";

/// Remove the retired marker at `found`, for the commands that do not plan
/// their writes the way `sync` does, and answer the path to report.
///
/// A marker that is already gone is not an error and is not reported: a
/// home-mode workspace's `config/` is shared by every clone of it, so another
/// clone's `sync` can delete the file between the check that found it and
/// this removal.
pub(crate) fn remove_marker(found: Option<(PathBuf, String)>) -> Result<Option<String>> {
    let Some((marker, shown)) = found else {
        return Ok(None);
    };
    match std::fs::remove_file(&marker) {
        Ok(()) => Ok(Some(shown)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => bail!(
            "could not remove {shown}, a version marker this spoolway no longer reads: {}\n  \
             delete it by hand and run this again",
            plain(&err)
        ),
    }
}

/// Delete the version marker a 0.10.0 `sync`, `init` or `install` left in the
/// project's setup folder, which is the workspace's `config/` in home mode.
///
/// Reported as a retired file removed, like a retired template, so a dry run
/// lists it too. Nothing else in the setup folder is touched.
fn retired_markers(repo: &Repo, outcomes: &mut Vec<Outcome>, acts: &mut Vec<Act>) {
    if let Some((marker, shown)) = project_marker(&repo.checkout) {
        acts.push(Act::remove_file(marker, &shown));
        outcomes.push(Outcome::removed(shown, MARKER_GONE));
    }
}

/// The user-level skill folders, kept current the same way a project folder is
/// — a home-mode project keeps its skills only there, and a person who ran
/// `spoolway install --user` from a repo-mode one would otherwise keep
/// whichever release they last installed. Named with `~`, since they sit
/// outside the checkout.
///
/// A folder's retired marker goes whether or not the folder still holds a
/// skill file, because the marker was written beside the skills and outlives
/// them when a person deletes those by hand.
fn user_level(outcomes: &mut Vec<Outcome>, acts: &mut Vec<Act>) -> Result<()> {
    if let Some(home) = crate::install::user_home() {
        for provider in <crate::cli::Provider as clap::ValueEnum>::value_variants() {
            if let Some((marker, shown)) = user_marker(&provider.user_skills_dir(&home)) {
                acts.push(Act::remove_file(marker, &shown));
                outcomes.push(Outcome::removed(shown, MARKER_GONE));
            }
        }
    }
    for (_, planned) in user_skills() {
        refresh(planned, crate::repo::shorten_home, outcomes, acts)?;
    }
    Ok(())
}

/// Spoolway's own block, still standing in a project set up before runtime
/// state moved out of the checkout.
///
/// `init` never writes one any more, so this only ever has something to do
/// once, for a project syncing in place; from then on it finds nothing and
/// reports nothing. See [`crate::gitignore`].
///
/// Skipped outright in home mode: `.gitignore` is a tracked file in the
/// checkout, and home mode promises to write nothing there — the same
/// promise `commands::init`'s own `home_mode` skip keeps for this exact
/// file (`src/commands/init.rs`).
fn ignores(
    repo: &Repo,
    home_mode: bool,
    outcomes: &mut Vec<Outcome>,
    acts: &mut Vec<Act>,
) -> Result<()> {
    use crate::gitignore::Removed;

    if home_mode {
        return Ok(());
    }

    let shown = crate::platform::relative(&repo.checkout, &crate::gitignore::file(&repo.checkout));
    match crate::gitignore::without_block(&repo.checkout)? {
        (Removed::Gone, text) => {
            if let Some(text) = text {
                acts.push(Act::write(
                    crate::gitignore::file(&repo.checkout),
                    &shown,
                    text,
                ));
            }
            outcomes.push(Outcome::wrote(&shown, "spoolway's old block removed"))
        }
        (Removed::Absent, _) => {}
        (Removed::Unterminated, _) => outcomes.push(Outcome::blocked(
            &shown,
            format!(
                "spoolway's block starts with `{}` and never ends — restore the `{}` marker, \
                 or delete the block and run this again",
                crate::assets::IGNORE_BEGIN,
                crate::assets::IGNORE_END
            ),
        )),
    }
    Ok(())
}

/// The non-blank `dispatch.worktree_root` a config document names, read
/// untyped because the loaded `Config` no longer keeps the retired field.
fn worktree_root_in(text: &str) -> Option<String> {
    toml::from_str::<toml::Value>(text)
        .ok()
        .and_then(|doc| {
            doc.get("dispatch")?
                .get("worktree_root")?
                .as_str()
                .map(str::to_string)
        })
        .filter(|path| !path.trim().is_empty())
}

/// The migration note for a dropped `dispatch.worktree_root`: where its
/// worktrees were cut, and which queued tasks still have one there.
///
/// A queued task can still have its own worktree sitting under the old
/// path — this never moves one already cut, only new ones land under the
/// project home — so the old directory is never called safe to remove
/// outright: a task still using it is named instead, the same way `doctor`'s
/// own `worktree_root_note` does (see `commands::doctor`). Shared by the
/// tracked file and the private override layer, so a key dropped from
/// either is reported the same way.
fn worktree_root_outcome(repo: &Repo, shown: &str, old: &str) -> Outcome {
    let tasks = repo.tasks().unwrap_or_default();
    let still_there = crate::config::tasks_under_worktree_root(old, &tasks);
    let (report_detail, panel_detail) = if still_there.is_empty() {
        (
            format!(
                "migrated: dispatch.worktree_root removed; its worktrees were cut at \
                 {old} — no queued task has a worktree there any more",
            ),
            format!("migrated: worktree_root removed; worktrees were at {old}"),
        )
    } else {
        (
            format!(
                "migrated: dispatch.worktree_root removed; its worktrees were cut at \
                 {old} — still in use by: {}",
                still_there.join(", "),
            ),
            format!(
                "migrated: worktree_root removed; {old} still used by {}",
                still_there.join(", "),
            ),
        )
    };
    Outcome::migrated(shown, report_detail, panel_detail)
}

/// A rewritten `config.toml` and the settings it carried over unknown.
type ConfigRewrite = (String, Vec<Vec<String>>);

/// What `config.toml` becomes under a sync: `text` rewritten from `current`,
/// and the settings it carried over that this binary does not know. The inner
/// `Err` is the reason there is nothing safe to write.
///
/// One place for the rewrite, because `sync --replace config.toml` promises to
/// rewrite the file "as sync does": two copies of these steps would drift the
/// next time one of them changed, and nothing would notice.
fn rewritten_config(
    current: &crate::config::Config,
    text: &str,
) -> Result<std::result::Result<ConfigRewrite, String>> {
    // A key this binary does not know is not in what `current` renders, so it
    // is carried over from the file with its comment instead — a typo is the
    // person's to fix and a newer binary's key is that binary's to read, and
    // neither is `sync`'s to delete. `doctor` and every load name them.
    let unknown = crate::config::unknown_keys(text);

    let (rewritten, kept) = crate::confdoc::keep(text, &current.render()?, &unknown)?;

    // Its own work, checked. A rewrite may not change a single value — every one
    // of them came out of this file a moment ago — so a rendering that no longer
    // means what the file meant is a bug, and is not written.
    if let Err(err) = current.agrees_with(&rewritten) {
        return Ok(Err(format!(
            "rewriting it would have changed what it says — {err:#}"
        )));
    }
    Ok(Ok((rewritten, kept)))
}

/// The rows of `text` that hold nothing but an empty `prompt_cache_ttl`, as the
/// dotted key a rewrite drops and the glob it sat under.
///
/// Read from the text, not the loaded config, because the load already
/// dropped them (see `Config::migrate`); `compare` then lists their key as
/// gone, and this is how the report tells that apart from a retired setting.
fn blank_lifetime_only_rows(text: &str) -> Vec<(String, String)> {
    let Ok(table) = toml::from_str::<toml::Table>(text) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for models in ["models", "pricing"] {
        let Some(globs) = table.get(models).and_then(toml::Value::as_table) else {
            continue;
        };
        for (glob, row) in globs {
            let Some(row) = row.as_table().filter(|row| row.len() == 1) else {
                continue;
            };
            for key in ["prompt_cache_ttl", "session_reuse_idle", "cache_ttl"] {
                if row
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .is_some_and(|raw| raw.trim().is_empty())
                {
                    rows.push((format!("{models}.{glob}.{key}"), glob.clone()));
                }
            }
        }
    }
    rows
}

/// The config file: rewritten whole, except for what you set it to.
///
/// The one file spoolway owns outright and a person reads constantly, so the
/// two halves are split the other way round from every other file here: the
/// **values** are the project's and survive a sync untouched, and everything
/// else — which settings are written down, what order they come in, and every
/// comment but the reference table's — is spoolway's and is rewritten from the
/// binary.
///
/// That is why the explanations in `config.toml` can be trusted: a note there
/// is what this binary says today, not a copy of what some older one said, and
/// not somebody's sentence sitting where a note belongs. The reference table on
/// top is rewritten only when it is a table some spoolway shipped. A table
/// equal to none of them was written by a newer spoolway or edited by hand, so
/// the file is refused, naming `spoolway sync --replace <path>`, and nothing in
/// it is rewritten. Prose about a project's own choices has somewhere better
/// to live — a prompt, a pipeline, or project documentation — where it is read
/// by whoever needs it rather than by nobody.
///
/// Until this existed nothing brought a config forward at all, and the only
/// thing that ever added a missing key was `config set` re-rendering the whole
/// file: the right rewrite, performed at the wrong moment, while somebody was
/// changing an interval. See [`crate::confdoc`].
///
/// The private `overrides/config.toml` layer is never part of what gets
/// rewritten here: `current` below is loaded with
/// [`crate::config::Config::load_dropping_retired_keys`], which merges no
/// patch onto it, precisely so a value set through `spoolway config
/// override` can never leak into the tracked file through a sync — a
/// project's private layer is not this project's to commit. What this *does*
/// touch is the layer itself: a key the override still names that the
/// tracked config has since stopped recognising is dropped straight out of
/// `overrides/config.toml` too, the same way a retired key vanishes from the
/// tracked file, so it stops reprinting "override ignored" forever with no
/// way to clear on its own.
fn config(repo: &Repo, outcomes: &mut Vec<Outcome>, acts: &mut Vec<Act>) -> Result<()> {
    let path = crate::config::Config::path_in(&repo.checkout);
    let shown = crate::platform::relative(&repo.checkout, &path);

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // The shipped default, not `repo.config`: `repo.config` is
            // `Config::load(&repo.root)` (see `Repo::discover`), and once
            // `checkout` and `root` can diverge that is a different branch's
            // values, not this one's — the same reason `templates` below
            // seeds a missing skeleton from `skeleton.shipped` rather than
            // from anything read out of the main checkout.
            let shipped = crate::config::Config::default();
            acts.push(Act::write(
                crate::config::write_target(&path),
                &shown,
                shipped.render()?,
            ));
            outcomes.push(Outcome::wrote(&shown, MISSING));
            return Ok(());
        }
        // Not an `Outcome::Blocked` like the two checks below: those are a
        // file this command understood and declined to touch, which is
        // `doctor`'s business to raise, not an ordinary sync's. A config it
        // cannot even read is a different thing — proceeding past it is how
        // a dry run came to print "nothing was written" over a project
        // whose config it never looked at, so this fails the whole command
        // instead, naming the file and what to do about it.
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "reading {} — fix the permissions, or see `spoolway doctor`",
                    path.display()
                )
            });
        }
    };

    // Read from the file rather than taken from `repo`, so that what is
    // written back is what this document says. Not the same config as
    // `repo.config`: that is `Config::load(&repo.root)`, and in a linked
    // worktree — the run this command now exists for — `root` and
    // `checkout` can hold two different files. `current` is always the
    // checkout's own, so a rewrite is sourced from the file it replaces.
    //
    // `load_dropping_retired_keys` rather than `Config::load`: both strip
    // `dispatch.interval` and `issue_tracking.on_fail` the same way now, but
    // `Config::load` also merges the private override layer onto the
    // result, and that merged value must never be what gets rendered and
    // written back here — see this function's own doc.
    // A config this binary cannot even parse, same as the read failure
    // above: this fails loudly rather than recording a quiet
    // `Outcome::Blocked` a plain `sync` or `sync --dry-run` never prints.
    // `load_dropping_retired_keys`'s own error already names the path —
    // `strip_refused_keys` only touches the raw text through
    // `crate::confdoc::remove` when a lenient, untyped parse finds the key
    // actually there, so invalid TOML reaches the named `toml::from_str`
    // unchanged and its error names the file same as any other parse
    // failure here. `spoolway config edit` is still worth naming in the
    // context below, since it reads this same file leniently for exactly
    // this reason (see `Command::Config(ConfigCommand::Edit)` in `main.rs`).
    let current = crate::config::Config::load_dropping_retired_keys(&repo.checkout)
        .with_context(|| format!("{} — fix it with `spoolway config edit`", path.display()))?;

    // Dropped against the layer's own directory, not the loaded `repo.home`:
    // `dir_for` is what `Config::load` itself resolves the layer through,
    // and a linked worktree's `repo.home` may not even point at the same
    // project's state — see `crate::overrides::dir_for`'s own doc.
    //
    // Only planned here, like the tracked file's own write below: a dry run
    // has to say what it would drop, and a layer rewritten before the plan
    // was checked is a file changed that a failed check never mentions.
    if let Ok(overrides_dir) = crate::overrides::dir_for(&repo.checkout) {
        let dropped = crate::overrides::retired_config_patch_keys(&overrides_dir, &current)?;
        if !dropped.is_empty() && repo.in_lane_worktree() {
            // The layer is the person's private file in the project home,
            // outside git, and a lane's rewrite of it cannot be reviewed or
            // undone. Sync still refreshes the lane's tracked files, so
            // this only leaves the layer alone and says so.
            let override_path = crate::overrides::config_patch_path(&overrides_dir);
            outcomes.push(Outcome::blocked(
                crate::platform::relative(&repo.checkout, &override_path),
                format!(
                    "a lane does not change the project's setup, so retired override key(s) \
                     stay in the private layer: {} — a person runs `spoolway sync` in the \
                     main checkout to drop them",
                    dropped.join(", ")
                ),
            ));
        } else if !dropped.is_empty() {
            // Read before the write removes it. A `worktree_root` set
            // only here is dropped just the same, and the folder it named
            // and the tasks still under it are said the same way.
            let old_worktree_root = dropped
                .iter()
                .any(|key| key == "dispatch.worktree_root")
                .then(|| {
                    std::fs::read_to_string(crate::overrides::config_patch_path(&overrides_dir))
                        .ok()
                        .and_then(|raw| worktree_root_in(&raw))
                })
                .flatten();
            let override_path = crate::overrides::config_patch_path(&overrides_dir);
            let shown_override = crate::platform::relative(&repo.checkout, &override_path);
            acts.push(Act::write(
                override_path,
                &shown_override,
                crate::overrides::config_patch_without(&overrides_dir, &dropped)?,
            ));
            outcomes.push(Outcome::migrated(
                &shown_override,
                format!(
                    "migrated: retired override key(s) dropped from the private layer: {}",
                    dropped.join(", ")
                ),
                format!(
                    "migrated: retired override key(s) dropped: {}",
                    dropped.join(", ")
                ),
            ));
            if let Some(old) = old_worktree_root {
                outcomes.push(worktree_root_outcome(repo, &shown_override, &old));
            }
        }
    }

    // The reference table on top is spoolway's to rewrite, but only when it is
    // a table some spoolway wrote. One that is none of them was written by a
    // newer spoolway or edited by hand, and rewriting the file would discard
    // that. Nothing else in the file is touched either, since a rewrite is
    // the whole file: `--replace` is the way to take this binary's.
    //
    // A file with no table at all is not that: nothing was edited, there is
    // simply no table yet, so the rewrite adds it as it adds any setting the
    // file never named. A hand-written `[dispatch]` with a couple of values is
    // the usual case.
    let table = reference_in(&text);
    if !table.trim().is_empty() && lookup(&Shipped::ConfigReference, table) == Known::Unknown {
        outcomes.push(Outcome::blocked(&shown, refusal(&shown)));
        return Ok(());
    }

    let (rewritten, kept) = match rewritten_config(&current, &text)? {
        Ok(rewrite) => rewrite,
        Err(why) => {
            outcomes.push(Outcome::blocked(&shown, why));
            return Ok(());
        }
    };

    if rewritten == text {
        outcomes.push(Outcome::Kept);
        return Ok(());
    }

    let refresh = match crate::confdoc::compare(&text, &rewritten) {
        Ok(refresh) => refresh,
        Err(err) => {
            outcomes.push(Outcome::blocked(&shown, format!("{err:#}")));
            return Ok(());
        }
    };

    let listed = |keys: &[String]| keys.join(", ");
    // What `keep` carried, not what it was asked to: a key whose table the
    // rewrite no longer has is gone, and `dropped` below says so.
    if !kept.is_empty() {
        let names: Vec<String> = kept.iter().map(|path| path.join(".")).collect();
        outcomes.push(Outcome::migrated(
            &shown,
            format!(
                "kept {} setting(s) this spoolway does not know, with their comments: {} — \
                 a newer spoolway may read them, otherwise they are typos",
                names.len(),
                listed(&names)
            ),
            format!(
                "kept {} unknown setting(s): {}",
                names.len(),
                listed(&names)
            ),
        ));
    }
    if !refresh.added.is_empty() {
        // A `Migrated`, not a `Wrote`: a `Wrote`'s detail is never printed,
        // and a sync that adds settings while naming none leaves a person
        // diffing the file to learn what arrived.
        outcomes.push(Outcome::migrated(
            &shown,
            format!(
                "{} setting(s) this spoolway has and the file did not: {}",
                refresh.added.len(),
                listed(&refresh.added)
            ),
            format!(
                "added {} setting(s): {}",
                refresh.added.len(),
                listed(&refresh.added)
            ),
        ));
    }
    // `compare` lists every path that is no longer there, and a path that
    // moved is not one that was dropped: `cache_write` is written back as
    // `cache_write_5m`, and a `[pricing]` row as a `[models]` row. Such a key
    // keeps its table, so a retired key or one whose table went with it are
    // the only ones that were removed — reporting a renamed price as deleted
    // would tell a person their prices are gone.
    let now = crate::confdoc::leaf_paths(&rewritten)?;
    // An earlier release's row holding nothing but a blank `prompt_cache_ttl`
    // is dropped on load, and the key is no retired one: it is reported on its
    // own, below, in words that do not tell a person the setting is gone.
    let blank_rows = blank_lifetime_only_rows(&text);
    let removed: Vec<String> = refresh
        .dropped
        .iter()
        .filter(|key| !blank_rows.iter().any(|(dropped, _)| dropped == *key))
        .filter(|key| {
            let key = key
                .strip_prefix("pricing.")
                .map_or_else(|| key.to_string(), |rest| format!("models.{rest}"));
            let top = key.split('.').next().unwrap_or_default();
            let table = key.rsplit_once('.').map_or("", |(table, _)| table);
            crate::config::is_retired_table(top)
                || crate::config::is_retired_key(&key)
                || !now
                    .iter()
                    .any(|path| path.starts_with(&format!("{table}.")))
        })
        .cloned()
        .collect();
    let (commented, bare): (Vec<_>, Vec<_>) = blank_rows
        .iter()
        .filter(|(dropped, _)| refresh.dropped.contains(dropped))
        .partition(|(_, glob)| rewritten.contains(&format!("# [models.\"{glob}\"]\n")));
    if !commented.is_empty() {
        let names: Vec<&str> = commented.iter().map(|(_, glob)| glob.as_str()).collect();
        outcomes.push(Outcome::migrated(
            &shown,
            format!(
                "the empty `prompt_cache_ttl` row for {} is now the commented cache-lifetime \
                 example: an older spoolway refuses an empty lifetime, and the row outranked \
                 your own model rows",
                names.join(", ")
            ),
            format!(
                "empty prompt_cache_ttl row now a comment: {}",
                names.join(", ")
            ),
        ));
    }
    if !bare.is_empty() {
        let names: Vec<&str> = bare.iter().map(|(_, glob)| glob.as_str()).collect();
        outcomes.push(Outcome::migrated(
            &shown,
            format!(
                "the `[models]` row holding only an empty `prompt_cache_ttl` was dropped for {}: \
                 it set nothing, and an older spoolway refuses an empty lifetime. Write \
                 `prompt_cache_ttl = \"1h\"` under the model to choose one",
                names.join(", ")
            ),
            format!("dropped empty prompt_cache_ttl row: {}", names.join(", ")),
        ));
    }
    if !removed.is_empty() {
        // A `Migrated`, not a `Wrote`: a `Wrote`'s detail is never printed, and
        // a rewrite that removes a setting without saying which one is the
        // "did it eat my config?" the report exists to answer.
        outcomes.push(Outcome::migrated(
            &shown,
            format!(
                "{} retired setting(s) dropped: {}",
                removed.len(),
                listed(&removed)
            ),
            format!(
                "dropped {} retired setting(s): {}",
                removed.len(),
                listed(&removed)
            ),
        ));
        // `on_fail` never drops on its own — it is the one field
        // `IssueTrackingConfig` retired, and the section it lived in moved
        // in the same pass: `Config`'s own field order put `[issue_tracking]`
        // last, below every `[agents.*]` and `[models.*]` table, and now
        // puts it right after `[watch]` instead (see `Config::render`). A
        // person reading only the dropped-key line would miss that the
        // section itself is now somewhere else in the file.
        //
        // A `Migrated` rather than a second `Wrote`: an ordinary `Wrote`'s
        // detail is never printed, and a project that relied on `on_fail =
        // "ignore"` has to read in the report itself that it is gone.
        if refresh
            .dropped
            .iter()
            .any(|key| key == "issue_tracking.on_fail")
        {
            outcomes.push(Outcome::migrated(
                &shown,
                "migrated: issue_tracking.on_fail removed, every failing hook now stops \
                 its task on hook_failed; [issue_tracking] moved directly under [watch]",
                "migrated: on_fail removed; [issue_tracking] moved under [watch]",
            ));
        }
        // A blank `worktree_root` was never anybody's decision — the ordinary
        // "retired setting(s) dropped" line above already covers it, and
        // there is nothing further to say. A real path is different: it is
        // where this project's worktrees actually were, and nothing else
        // tells a person they are still sitting there once the key that
        // named them is gone.
        let old_worktree_root = refresh
            .dropped
            .iter()
            .any(|key| key == "dispatch.worktree_root")
            .then(|| worktree_root_in(&text))
            .flatten();
        if let Some(old) = old_worktree_root {
            outcomes.push(worktree_root_outcome(repo, &shown, &old));
        }
    }
    if !refresh.renoted.is_empty() {
        outcomes.push(Outcome::wrote(
            &shown,
            format!(
                "the explanation above {} rewritten",
                listed(&refresh.renoted)
            ),
        ));
    }
    // A value the file never named is written at its default. Said on a line
    // of its own because `key_in_names` changes the keys `spoolway issues`
    // creates, and the report closing on "config values were kept" would
    // otherwise be read as meaning nothing was set. Pushed after the migration
    // notes, which the report prints first.
    if refresh
        .added
        .iter()
        .any(|key| key == "issue_tracking.key_in_names")
    {
        let value = current.issue_tracking.key_in_names;
        outcomes.push(Outcome::migrated(
            &shown,
            format!(
                "set issue_tracking.key_in_names = {value} — the 0.7 default; the file did \
                 not name it. `spoolway config set issue_tracking.key_in_names false` turns \
                 it off"
            ),
            format!("set issue_tracking.key_in_names = {value}, the default"),
        ));
    }
    // Whitespace and key order, and nothing a person would recognise as a
    // setting. Recorded anyway, because a file that came back changed with
    // nothing said about it is the one thing worse than a noisy report — and
    // this is the detail `doctor` reads, not something the report prints.
    if refresh.is_empty() {
        outcomes.push(Outcome::wrote(&shown, "relaid out"));
    }

    // Through the link, if `config.toml` is one: `sync` runs after every
    // upgrade, and a rename over a linked shared file would replace the link.
    acts.push(Act::write(
        crate::config::write_target(&path),
        &shown,
        rewritten,
    ));
    Ok(())
}

/// The reference table at the top of a `config.toml`: its leading run of
/// comment lines, the span [`crate::confkv::reference_table`] writes above
/// the blank line that separates it from the settings. Empty when the file
/// starts with no comment, or with one that sits straight on a setting.
///
/// A file whose first line is not a comment has no table, and this answers the
/// empty text.
fn reference_in(text: &str) -> &str {
    let mut end = 0;
    for line in text.split_inclusive('\n') {
        if !line.starts_with('#') {
            break;
        }
        end += line.len();
    }
    // A comment block straight on top of a key or a table is that setting's
    // own comment, not a table: the one `reference_table` writes is always
    // followed by a blank line.
    match text[end..].lines().next() {
        Some(next) if !next.trim().is_empty() => "",
        _ => &text[..end],
    }
}

/// The page skeletons: their one machine-read block, and nothing else.
///
/// These are meant to be restyled, so taking the file back only when it is
/// byte-for-byte ours would block every project that used them as intended.
/// And they are not decoration either: each carries a block
/// something parses, and a restyled skeleton whose block predates a schema
/// change writes pages that are quietly short of a field. So the block is kept
/// current and the styling around it is never read. See [`crate::skeleton`].
fn templates(repo: &Repo, outcomes: &mut Vec<Outcome>, acts: &mut Vec<Act>) -> Result<()> {
    templates_of(repo, &crate::skeleton::skeletons(), outcomes, acts)
}

/// Where `skeleton_path` — spelled relative to the repo root, as
/// [`crate::skeleton::Skeleton::path`] is — lives for this project.
///
/// A path under `.spoolway/` is the project's setup folder, which is the
/// workspace's `config/` in home mode, so it goes through
/// [`Repo::setup_dir`]. Any other path is a file in the checkout, and home
/// mode promises nothing is written there, so it has no place and is `None`.
/// In repo mode both spellings land in the checkout, as they always did.
fn skeleton_path(repo: &Repo, skeleton_path: &str) -> Option<PathBuf> {
    if Path::new(skeleton_path).starts_with(crate::config::STATE_DIR) {
        return Some(repo.under_setup(skeleton_path));
    }
    match crate::repo::workspace_clone(&repo.root) {
        Some(_) => None,
        None => Some(repo.checkout.join(skeleton_path)),
    }
}

/// [`templates`] over an explicit list, so a test can stand a skeleton in for
/// the shipped list, which is empty today.
fn templates_of(
    repo: &Repo,
    skeletons: &[crate::skeleton::Skeleton],
    outcomes: &mut Vec<Outcome>,
    acts: &mut Vec<Act>,
) -> Result<()> {
    for skeleton in skeletons {
        let Some(path) = skeleton_path(repo, skeleton.path) else {
            continue;
        };
        let shown = crate::platform::relative(&repo.checkout, &path);

        let on_disk = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                acts.push(Act::write(path.clone(), &shown, skeleton.shipped));
                outcomes.push(Outcome::wrote(&shown, MISSING));
                continue;
            }
            Err(err) => {
                outcomes.push(Outcome::blocked(
                    &shown,
                    format!("could not be read ({err})"),
                ));
                continue;
            }
        };

        use crate::skeleton::BlockState;
        match skeleton.state(&on_disk) {
            BlockState::Current => outcomes.push(Outcome::Kept),

            BlockState::Stale => {
                write_block(&path, &shown, &on_disk, skeleton, outcomes, acts, "block")?;
            }

            // The project changed the part a machine reads. Their styling is
            // theirs and always was; this is the half that has to agree with
            // the binary, so say so rather than choosing for them. `--replace`
            // is the deliberate way to take the whole shipped file back.
            BlockState::HandEdited => outcomes.push(Outcome::blocked(
                &shown,
                // Named as "the file" rather than by path: `shown` has
                // already printed it.
                "its machine-readable block was edited by hand, so it was left alone — but that \
                 block is what the file is read for, and the styling around it was never ours to \
                 keep current. `spoolway sync --replace <path>` takes the shipped file back",
            )),

            // Restyled past recognition. Writing a block into a file we cannot
            // place it in would put it somewhere arbitrary.
            BlockState::Missing => outcomes.push(Outcome::blocked(
                &shown,
                "no machine-readable block found in it, so it was left alone",
            )),
        }
    }
    Ok(())
}

/// Swap a skeleton's block, keeping every other byte of the project's file.
fn write_block(
    path: &Path,
    shown: &str,
    on_disk: &str,
    skeleton: &crate::skeleton::Skeleton,
    outcomes: &mut Vec<Outcome>,
    acts: &mut Vec<Act>,
    note: &str,
) -> Result<()> {
    let Some(next) = skeleton.region.replace(on_disk, skeleton.block()) else {
        outcomes.push(Outcome::blocked(shown, "its block moved while we read it"));
        return Ok(());
    };
    acts.push(Act::write(path.to_path_buf(), shown, next));
    outcomes.push(Outcome::wrote(shown, note));
    Ok(())
}

/// `--replace`: this binary's copy, over whatever is there.
///
/// The way back from a file so far from the shape we know that no region can be
/// found in it, one whose machine-readable block was hand-edited and so is
/// left alone by the in-place rewrite, or one `sync` refused because no
/// spoolway ever shipped it. For most files the copy is the whole file; a
/// pipeline file swaps only its key block, and `config.toml` is rewritten as
/// `sync` does, keeping its values (see [`Scope`]). Everything about it is
/// deliberately blunt: named paths only, the old contents saved beside the
/// new, and a count of what is being discarded.
fn replace(repo: &Repo, args: &SyncArgs) -> Result<()> {
    let mut failed = 0;
    // Whether a file of the checkout was written, which is what `git diff`
    // can show; a user-level skill sits outside it.
    let mut in_checkout = false;
    let home_mode = crate::repo::workspace_clone(&repo.root).is_some();
    for name in &args.replace {
        // A path spelled under `.spoolway/` names the project's setup folder,
        // which in home mode is the workspace's `config/` and not a folder
        // of the checkout. Anything else relative joins the checkout.
        let path = if let Some(under_home) = home_relative(name) {
            under_home
        } else if Path::new(name).is_absolute() {
            PathBuf::from(name)
        } else if Path::new(name).starts_with(crate::config::STATE_DIR) {
            repo.under_setup(name)
        } else {
            repo.checkout.join(name)
        };
        let shown = shown_path(repo, &path);

        // A flat `<name>.md` whose directory-shaped `<name>/PROMPT.md` exists is
        // a file this project no longer reads — `prompt::path_for` prefers the
        // directory. Name that path instead of writing a dead file (finding 67).
        if let Ok(rest) = path.strip_prefix(repo.prompts_dir())
            && rest.components().count() == 1
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            let nested = crate::prompt::directory_form(repo, stem);
            if nested.is_file() {
                println!(
                    "  ! {shown}: this project keeps its prompts as `<name>/PROMPT.md` — replace {} instead",
                    shown_path(repo, &nested)
                );
                failed += 1;
                continue;
            }
        }

        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        let (shipped, scope) = match replacement_for(repo, &path, &on_disk) {
            Ok(found) => found,
            Err(why) => {
                println!("  ! {shown}: {why}");
                failed += 1;
                continue;
            }
        };

        let is_hook = path.starts_with(crate::tracking::hooks_dir_in(&repo.checkout));

        if on_disk == shipped {
            // The text is already ours, but a hook's execute bit is not part
            // of that comparison — a stale checkout or an editor save can
            // still have stripped it. Repair the mode here too, or the early
            // return below would report the file "already ours" while it
            // stays non-executable (issue #331).
            if is_hook && !args.dry_run && repair_hook_mode(&path)? {
                println!("  wrote   {shown} (execute bit repaired)");
            } else {
                println!("  kept    {shown} (already ours)");
            }
            continue;
        }
        let discarded = on_disk.lines().count();

        if args.dry_run {
            println!("  would   {shown} ({})", scope.dry_run(discarded));
            continue;
        }

        if !on_disk.is_empty() {
            // Never over an existing backup: a second `--replace` of the same
            // file would otherwise destroy the person's own edits that the
            // first one saved. The first free name is taken instead.
            let first = path.with_extension(format!(
                "{}.bak",
                path.extension().and_then(|e| e.to_str()).unwrap_or("")
            ));
            let mut backup = first.clone();
            let mut n = 1;
            while backup.exists() {
                let mut name = first.as_os_str().to_owned();
                name.push(format!(".{n}"));
                backup = PathBuf::from(name);
                n += 1;
            }
            write_atomic(&backup, &on_disk)?;
            // `write_atomic` creates files at the writer's default mode, so a
            // hook's backup would lose its execute bit. The backup keeps the
            // mode of the file it is a copy of.
            let mode = std::fs::metadata(&path)
                .with_context(|| format!("reading {}", path.display()))?
                .permissions();
            std::fs::set_permissions(&backup, mode)
                .with_context(|| format!("writing {}", backup.display()))?;
            println!("  ! your version is saved to {}", shown_path(repo, &backup));
        }
        write_atomic(&path, &shipped)?;
        if is_hook {
            // `write_atomic` has no opinion about permissions, so a hook
            // replaced here would land back at the writer's default mode —
            // not executable — and the next hook call would exit 126.
            // `init` ships hooks at `0755`; match that here too.
            repair_hook_mode(&path)?;
        }
        match home_mode {
            // A workspace path is long, so the note goes on its own line,
            // under the path.
            true => println!("  wrote   {shown}\n          ({})", scope.wrote()),
            false => println!("  wrote   {shown} ({})", scope.wrote()),
        }
        in_checkout |= path.starts_with(&repo.checkout);
    }

    println!();
    if failed > 0 {
        anyhow::bail!(
            "{failed} of {} path(s) could not be replaced",
            args.replace.len()
        );
    }
    // Nothing to diff when only user-level skills were written: they sit
    // outside the checkout's git repository.
    if (args.dry_run || in_checkout)
        && let Some(line) = replace_footer(repo, args.dry_run)
    {
        println!("{line}");
    }
    Ok(())
}

/// How much of a file `--replace` rewrites, and so what it tells the person.
#[derive(Clone, Copy)]
enum Scope {
    /// Every byte: a prompt, a template, a hook, a skill, a skeleton.
    Whole,
    /// A pipeline file's fenced key block. Every other line is kept.
    KeyBlock,
    /// The key block, and `headless:` renamed to `hide:` on the steps that
    /// had it. Every other line is kept.
    KeyBlockRenamed,
    /// As [`Scope::KeyBlockRenamed`], and a `headless: false` line deleted on
    /// at least one step. Every other line is kept.
    KeyBlockRenamedDropped,
    /// `config.toml`, rewritten as `sync` does: values kept, the table and
    /// comments spoolway's.
    Config,
}

impl Scope {
    /// What a dry run says it would do to the file.
    fn dry_run(self, discarded: usize) -> String {
        match self {
            Scope::Whole => format!("whole file, discarding {discarded} line(s)"),
            Scope::KeyBlock
            | Scope::KeyBlockRenamed
            | Scope::KeyBlockRenamedDropped
            | Scope::Config => self.wrote().to_string(),
        }
    }

    /// What the report says a write did.
    fn wrote(self) -> &'static str {
        match self {
            Scope::Whole => "whole file, discarding your changes",
            Scope::KeyBlock => "key block only, every other line kept",
            Scope::KeyBlockRenamed => {
                "key block, and `headless:` renamed to `hide:`; every other line kept"
            }
            Scope::KeyBlockRenamedDropped => {
                "key block, and `headless:` renamed to `hide:`, or removed where it was `false`; \
                 every other line kept"
            }
            Scope::Config => "rewritten as sync does, your values kept",
        }
    }
}

/// `name` as a path under the home directory when it is spelled with a
/// leading `~`, the way `sync` names a user-level skill. `None` for any other
/// spelling, and for a `~` with no home directory to resolve it against.
fn home_relative(name: &str) -> Option<PathBuf> {
    let rest = name.strip_prefix('~')?;
    let home = crate::platform::home_dir()?;
    match rest.strip_prefix(['/', '\\']) {
        Some(rest) => Some(home.join(rest)),
        None if rest.is_empty() => Some(home),
        None => None,
    }
}

/// The text `--replace` writes at `path`, given what is there now, and how much
/// of the file that is. The `Err` is the reason there is nothing to write.
///
/// A file with one fixed shipped copy answers from [`shipped_for`]. A pipeline
/// file and `config.toml` have none, since each holds values or steps that are
/// the project's own, so their text is worked out from `on_disk`.
fn replacement_for(
    repo: &Repo,
    path: &Path,
    on_disk: &str,
) -> std::result::Result<(String, Scope), String> {
    if let Some(shipped) = shipped_for(repo, path) {
        return Ok((shipped, Scope::Whole));
    }

    if path == crate::config::Config::path_in(&repo.checkout) {
        // The same rewrite `config` plans, minus its report: the table and the
        // settings spoolway writes come from the binary, and every value is
        // read back out of this file.
        let current = crate::config::Config::load_dropping_retired_keys(&repo.checkout)
            .map_err(|err| format!("{err:#} — fix it with `spoolway config edit`"))?;
        let (rewritten, _) =
            rewritten_config(&current, on_disk).map_err(|err| format!("{err:#}"))??;
        return Ok((rewritten, Scope::Config));
    }

    let in_pipelines = path.parent() == Some(&crate::pipeline::Pipelines::dir_in(&repo.checkout))
        && path
            .extension()
            .is_some_and(|ext| ext == "yml" || ext == "yaml");
    if in_pipelines {
        let swapped = crate::pipeline::KEY_BLOCK
            .replace(on_disk, crate::pipeline::key_block())
            .ok_or_else(|| {
                format!(
                    "it has no key block to swap, so spoolway has no place in it to write; paste \
                     the `{}` and `{}` marker lines in where the block belongs, then run this again",
                    crate::assets::PIPELINE_KEYS_BEGIN,
                    crate::assets::PIPELINE_KEYS_END
                )
            })?;
        // A plain `sync` refuses a hand-edited key block before it renames
        // anything, and names this command as the way on. So this command
        // makes the rename too, or the file would still not load after it.
        return match rename_headless(&swapped, Layout::Steps) {
            Renamed::Nothing => Ok((swapped, Scope::KeyBlock)),
            Renamed::Done { text, dropped, .. } => {
                let scope = match dropped.is_empty() {
                    true => Scope::KeyBlockRenamed,
                    false => Scope::KeyBlockRenamedDropped,
                };
                Ok((text, scope))
            }
            Renamed::Refused(ids) => Err(both_keys_refusal(&ids)),
        };
    }

    Err("not a file spoolway ships, so there is nothing to replace it with".to_string())
}

/// How `--replace` names `path`: relative to the checkout, as ever, except
/// that a file outside it — a user-level skill, or in home mode a file in the
/// workspace's `config/` — is named by its `~` form, since the long absolute
/// path is not one the person typed.
fn shown_path(repo: &Repo, path: &Path) -> String {
    let in_workspace =
        crate::repo::workspace_clone(&repo.root).is_some() && path.starts_with(repo.setup_dir());
    match in_workspace || !path.starts_with(&repo.checkout) {
        true => crate::repo::shorten_home(path),
        false => crate::platform::relative(&repo.checkout, path),
    }
}

/// The closing line of a successful `--replace`, if it has one.
///
/// Workspace files are not in the project's git repository, so in home mode
/// there is no `git diff` to point at and the line is left out.
fn replace_footer(repo: &Repo, dry_run: bool) -> Option<&'static str> {
    match (dry_run, crate::repo::workspace_clone(&repo.root)) {
        (true, _) => Some(DRY_RUN),
        (false, Some(_)) => None,
        (false, None) => Some("`git diff` shows exactly what changed."),
    }
}

/// Set a replaced hook back to the executable mode `init` ships it with.
/// Returns whether the mode actually changed, so callers can tell a real
/// repair from a no-op. A no-op everywhere but Unix: there is no execute bit
/// to lose elsewhere.
#[cfg(unix)]
fn repair_hook_mode(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::metadata(path)?.permissions();
    let changed = perms.mode() & 0o777 != 0o755;
    if changed {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(changed)
}

#[cfg(not(unix))]
fn repair_hook_mode(_path: &Path) -> Result<bool> {
    Ok(false)
}

/// The text spoolway would write at `path`, if it writes anything there at all.
fn shipped_for(repo: &Repo, path: &Path) -> Option<String> {
    shipped_for_among(repo, path, &crate::skeleton::skeletons())
}

/// [`shipped_for`] against an explicit skeleton list, for the same reason as
/// [`templates_of`].
fn shipped_for_among(
    repo: &Repo,
    path: &Path,
    skeletons: &[crate::skeleton::Skeleton],
) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;

    // Prompts are outside the sync cycle, but not outside `--replace`. This
    // is the explicit, one-file-at-a-time way to take today's default back —
    // the same thing as running `init` in a scratch directory and copying, done
    // in place and named, rather than by a command that walks the whole set.
    //
    // A prompt is a directory now, so the name is the directory's and not the
    // file's — every `PROMPT.md` has the same stem. Its `assets/` are matched
    // on their own filename, one level further down: those are as replaceable
    // as the prose beside them, and reaching only half of a prompt's files
    // would be the surprising answer.
    if let Ok(rest) = path.strip_prefix(repo.prompts_dir()) {
        let mut parts = rest.components();
        let head = parts.next()?.as_os_str().to_str()?;
        let rest = parts.as_path();

        // The flat `<name>.md` a project had before prompts gained a
        // directory. Still replaceable — but only while it is still the file
        // this project reads. Once `<name>/PROMPT.md` exists, `prompt::path_for`
        // prefers it and the flat path is dead: writing it would leave a new
        // dead file beside the one that runs (finding 67).
        if rest.as_os_str().is_empty() {
            if crate::prompt::directory_form(repo, stem).is_file() {
                return None;
            }
            return crate::assets::prompt(stem).map(|prompt| prompt.body.to_string());
        }

        let prompt = crate::assets::prompt(head)?;
        if rest == Path::new(crate::assets::PROMPT_FILE) {
            return Some(prompt.body.to_string());
        }
        let asset = rest.strip_prefix(crate::assets::PROMPT_ASSETS).ok()?;
        return prompt
            .assets
            .iter()
            .find(|(known, _)| Path::new(known) == asset)
            .map(|(_, body)| body.to_string());
    }

    if path.starts_with(repo.task_templates_dir()) {
        return crate::assets::task_template(stem).map(str::to_string);
    }

    // The hook scripts `init` seeds into `.spoolway/hooks/` — same rule:
    // never touched by an ordinary sync, `--replace` only.
    if path.starts_with(crate::tracking::hooks_dir_in(&repo.checkout)) {
        return crate::assets::HOOK_SCRIPTS
            .iter()
            .find(|(known, _)| Path::new(known).file_stem().and_then(|s| s.to_str()) == Some(stem))
            .map(|(_, body)| body.to_string());
    }

    // A skill file, project or user level: whole-file, like the rest here, and
    // the only way back to ours once `sync` has refused one it does not know.
    // A project's skill folders are the checkout's, so in home mode, which
    // writes nothing there, only the user-level ones answer.
    let home_mode = crate::repo::workspace_clone(&repo.root).is_some();
    let providers = <crate::cli::Provider as clap::ValueEnum>::value_variants();
    let project = providers
        .iter()
        .filter(|_| !home_mode)
        .flat_map(|provider| provider.plan(&repo.checkout));
    let user = crate::install::user_home().into_iter().flat_map(|home| {
        providers
            .iter()
            .flat_map(move |provider| provider.plan_user(&home))
    });
    if let Some(planned) = project.chain(user).find(|planned| planned.path == path) {
        return Some(planned.contents.to_string());
    }

    // The ignore rules are deliberately not here. They are a block in a file the
    // project owns the rest of, so there is no whole-file version of it to write:
    // `ignores` above refreshes the block, on every run, and that is the only way
    // back to ours.

    for skeleton in skeletons {
        if skeleton_path(repo, skeleton.path).is_some_and(|known| known == path) {
            return Some(skeleton.shipped.to_string());
        }
    }

    None
}

/// Whether `provider`'s skills need to migrate off Codex's old
/// `.codex/skills/` root — true only for Codex, and only when a file this
/// binary would plan there today already sits under the old root. Split out
/// of [`skills`] so [`provider_installed`] can ask the identical question
/// rather than a second copy of it.
fn provider_needs_codex_migration(
    provider: crate::cli::Provider,
    checkout: &Path,
    planned: &[crate::install::Planned],
) -> bool {
    if provider != crate::cli::Provider::Codex {
        return false;
    }
    let dir = provider.skills_dir(checkout);
    let old_codex_root = checkout.join(".codex").join("skills");
    planned.iter().any(|file| {
        let relative = file
            .path
            .strip_prefix(&dir)
            .expect("a provider's plan is below its skills directory");
        old_codex_root.join(relative).exists()
    })
}

/// Whether a project has installed `provider`'s skills at all: a planned
/// file already on disk, a retired skill directory a rename left behind, or
/// — Codex only — an old-root install waiting on [`provider_needs_codex_migration`].
///
/// The one place this decision is made.
fn provider_installed(
    provider: crate::cli::Provider,
    checkout: &Path,
    planned: &[crate::install::Planned],
) -> bool {
    provider_needs_codex_migration(provider, checkout, planned)
        || installed_at(&provider.skills_dir(checkout), planned)
}

/// Whether a provider's skills are installed in the one folder `dir`, whose
/// files `planned` lists: any file this binary ships already there, or a
/// skill directory it has since retired. [`provider_installed`] asks it of a
/// project's folder only — a user-level one is never something a release
/// before #576 could have left a retired-name trace in, so [`user_skills`]
/// checks only the four shipped names instead; see that function's doc.
fn installed_at(dir: &Path, planned: &[crate::install::Planned]) -> bool {
    planned.iter().any(|file| file.path.exists())
        || crate::install::RETIRED_SKILLS
            .iter()
            .any(|(name, _)| dir.join(name).is_dir())
}

/// Each provider's user-level skill folder that holds spoolway's skills, with
/// the files this binary would write there — the copies a home-mode `init`
/// or `spoolway install --user` placed. Empty when there is no home
/// directory, and — inside the test binary — unless a test set a scratch
/// one; see [`crate::install::user_home`].
///
/// Unlike a project folder's own [`installed_at`] check, this never counts a
/// retired name: both retired names predate #576, so a retired-name
/// directory at user level can never be spoolway's own rename leftover, only
/// a person's own folder that happens to share the name — see
/// [`retired_skills`]'s own comment, which for that reason skips user
/// folders entirely. What is checked instead is whether any file one of the
/// four shipped skills would plant is already there: a planned `SKILL.md`
/// under a directory named `spoolway-plan`, `spoolway-tasks`,
/// `spoolway-config` or `spoolway-calibrate`, the one set
/// [`crate::install::SKILLS`] ships, reached through [`Provider::plan_user`]
/// rather than spelled out again here. A folder with none of those is a
/// person's own and is left alone; one holding even a single stale copy —
/// installed by any release, with or without the marker releases from #593
/// to this fix wrote — is spoolway's to refresh.
fn user_skills() -> Vec<(PathBuf, Vec<crate::install::Planned>)> {
    let Some(home) = crate::install::user_home() else {
        return Vec::new();
    };
    <crate::cli::Provider as clap::ValueEnum>::value_variants()
        .iter()
        .map(|provider| (provider.user_skills_dir(&home), provider.plan_user(&home)))
        .filter(|(_, planned)| planned.iter().any(|file| file.path.exists()))
        .collect()
}

/// Bring each of `planned` forward, naming it as `shown` gives it — the half
/// of [`skills`] a project folder and a user folder share.
///
/// A missing file is added, and one equal to the shipped copy is kept. One
/// that matches an older copy a build shipped is stale and rewritten. One that
/// matches no copy ever shipped was written by a newer spoolway or edited by
/// hand, so it is refused and left as it is: rolling it back would be this
/// command helping, the one thing it must not do. See [`crate::known_copies`].
fn refresh(
    planned: Vec<crate::install::Planned>,
    shown: impl Fn(&Path) -> String,
    outcomes: &mut Vec<Outcome>,
    acts: &mut Vec<Act>,
) -> Result<()> {
    for planned in planned {
        let detail = match planned.on_disk() {
            Some(Known::Current) => {
                outcomes.push(Outcome::Kept);
                continue;
            }
            Some(Known::Older) => "rewritten",
            Some(Known::Unknown) => {
                let shown = shown(&planned.path);
                outcomes.push(Outcome::blocked(&shown, refusal(&shown)));
                continue;
            }
            None => "added",
        };
        let shown = shown(&planned.path);
        acts.push(Act::write(planned.path, &shown, planned.contents));
        outcomes.push(Outcome::wrote(shown, detail));
    }
    Ok(())
}

/// Skills are brought to the shipped copy wherever a project installed
/// them: a skill file belongs to spoolway, the same bargain `config.toml`
/// keeps for everything around a project's own values, but only while it is a
/// copy some spoolway shipped. One equal to an older shipped copy is stale and
/// rewritten; one equal to no shipped copy was written by a newer spoolway or
/// edited by hand, and is refused per file (see [`refresh`]). Writing them
/// into a project that never ran `install` would still be this command
/// choosing an agent on someone's behalf, so that guard stays.
///
/// Every provider, not just `claude`: `init` and `install` write the same
/// skills under `.agents/skills/` and `.pi/skills/` too, and a codex or pi
/// project that never sees them refreshed keeps stale skills after every
/// sync. Whether a provider is installed is decided once, for the whole
/// set: any skill this binary ships already on disk there, or one it has
/// since retired — a project installed before a rename has only the old
/// name to show for it. Decided per file instead, a skill this release
/// newly ships would never reach a project that installed the last one,
/// and a rename would remove the old directory without ever writing the
/// new — which is exactly what happened to `spoolway-config`. Codex's
/// former `.codex/skills/` root counts as an installation too: writing the
/// current set under `.agents/skills/` migrates it, and then the copies
/// spoolway itself put under the old root — its own skills and any it has
/// since retired, never a directory the project made — are removed, since
/// Codex no longer reads that root and a stale set nothing loads is what a
/// person finds and edits by mistake. The old root itself, and `.codex/`
/// above it, are left in place unless the move emptied them.
///
/// The provider loop above is project-level — every path it writes sits
/// under `repo.checkout` — so it is skipped entirely in home mode, the same
/// promise `commands::init`'s own `home_mode` branch keeps by calling
/// `install_user` instead of `install` (`src/commands/init.rs`). The
/// user-level copies are not this function's: [`user_level`] refreshes them,
/// outside the checkout, in the agent's own folder, which is exactly where a
/// home-mode project keeps its skills.
fn skills(
    repo: &Repo,
    home_mode: bool,
    outcomes: &mut Vec<Outcome>,
    acts: &mut Vec<Act>,
) -> Result<()> {
    for provider in <crate::cli::Provider as clap::ValueEnum>::value_variants() {
        if home_mode {
            continue;
        }
        let planned = provider.plan(&repo.checkout);
        let dir = provider.skills_dir(&repo.checkout);
        let old_codex_root = repo.checkout.join(".codex").join("skills");
        let migrate_codex = provider_needs_codex_migration(*provider, &repo.checkout, &planned);
        if !provider_installed(*provider, &repo.checkout, &planned) {
            continue;
        }

        refresh(
            planned,
            |path| crate::platform::relative(&repo.checkout, path),
            outcomes,
            acts,
        )?;

        if migrate_codex {
            let mut names: Vec<String> = provider
                .plan(&repo.checkout)
                .iter()
                .filter_map(|file| {
                    file.path
                        .strip_prefix(&dir)
                        .ok()?
                        .components()
                        .next()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                })
                .collect();
            names.extend(
                crate::install::RETIRED_SKILLS
                    .iter()
                    .map(|(name, _)| name.to_string()),
            );
            names.sort();
            names.dedup();
            for name in names {
                let stale = old_codex_root.join(&name);
                if !stale.is_dir() {
                    continue;
                }
                let shown = crate::platform::relative(&repo.checkout, &stale);
                acts.push(Act::remove_dir(stale, &shown));
                outcomes.push(Outcome::removed(
                    &shown,
                    format!(
                        "moved to {}, which is where Codex reads skills from now",
                        crate::platform::relative(&repo.checkout, &dir)
                    ),
                ));
            }
            // The old root, and `.codex/` above it, go too once the move
            // has emptied them — `remove_dir` refuses a directory holding
            // anything, so a project's own files there are never at risk.
            acts.push(Act::Tidy(old_codex_root));
            acts.push(Act::Tidy(repo.checkout.join(".codex")));
        }
    }
    Ok(())
}

/// Remove a stale skill directory left behind by a rename — from every
/// provider a project has installed, the same way [`skills`] rewrites every
/// provider's own current set.
///
/// [`crate::install::RETIRED_SKILLS`] is the whole of what may be removed
/// here: a short, hand-written literal rather than anything derived from what
/// [`crate::install::SKILLS`] ships today, so a skill this binary still ships
/// can never end up on this list by construction — see that constant's own
/// doc. A project's own skill directory, named by neither list, is never
/// touched: this only ever joins a provider's skills directory onto a name
/// this project once shipped and no longer does.
///
/// Project folders only — see below — so skipped entirely in home mode,
/// the same as the project-level half of [`skills`].
fn retired_skills(
    repo: &Repo,
    home_mode: bool,
    outcomes: &mut Vec<Outcome>,
    acts: &mut Vec<Act>,
) -> Result<()> {
    if home_mode {
        return Ok(());
    }
    // Project folders only. Both retired names predate #576 — the release
    // that first wrote anything under a user folder at all — so a
    // retired-name directory there can never be a rename this binary left
    // behind; it is always a person's own, with or without the marker
    // releases from #593 to this fix wrote, and must never be removed.
    let dirs = <crate::cli::Provider as clap::ValueEnum>::value_variants()
        .iter()
        .map(|provider| provider.skills_dir(&repo.checkout));
    for dir in dirs {
        for (name, why) in crate::install::RETIRED_SKILLS {
            let stale = dir.join(name);
            if !stale.is_dir() {
                continue;
            }
            let shown = if stale.starts_with(&repo.checkout) {
                crate::platform::relative(&repo.checkout, &stale)
            } else {
                crate::repo::shorten_home(&stale)
            };
            acts.push(Act::remove_dir(stale, &shown));
            outcomes.push(Outcome::removed(&shown, *why));
        }
    }
    Ok(())
}

/// Remove a template this project no longer ships under
/// `.spoolway/templates/` — [`crate::install::RETIRED_TEMPLATES`], and
/// nothing else, the same bargain [`retired_skills`] keeps for a stale skill
/// directory. A file under that directory the project wrote itself is named
/// by neither list nor by anything `init` still places, so it is never
/// touched.
fn retired_templates(repo: &Repo, outcomes: &mut Vec<Outcome>, acts: &mut Vec<Act>) -> Result<()> {
    for (name, why) in crate::install::RETIRED_TEMPLATES {
        let path = crate::config::under_setup(&repo.setup_dir(), name);
        if !path.is_file() {
            continue;
        }
        let shown = crate::platform::relative(&repo.checkout, &path);
        acts.push(Act::remove_file(path, &shown));
        outcomes.push(Outcome::removed(&shown, *why));
    }
    Ok(())
}

/// The key reference in a pipeline file: the fenced block, and nothing else.
///
/// A pipeline is the project's flow, written in the project's words, so almost
/// none of the file is ours. The exception is the table of keys in its header:
/// that is documentation of *this binary's* contract, and a copy of it written
/// by an older release is a copy that is now wrong. So an older copy of the
/// fence is refreshed wherever it is found and every line around it — the
/// title, the steps, the notes between them — is copied through unread.
///
/// Two rules specific to a pipeline file, both deliberate:
///
/// - The block is replaced only when it is a copy some spoolway shipped. An
///   older one is stale, and every sentence in it is a claim about what the
///   binary does, so nothing in it is a project's to have meant. One that
///   matches no shipped copy was written by a newer spoolway or edited by hand,
///   so it is refused, naming the file and `spoolway sync --replace <path>`,
///   which swaps the block, renames or removes `headless:` as below, and keeps
///   every other line.
/// - A file with no markers is never written, and is reported only by the
///   check below. Writing a block into a pipeline somebody wrote themselves
///   would be this command helping, which is the one thing it must never do.
///   Pasting the two markers in is how a pipeline opts in.
///
/// Two checks read every file, markers or not. The first is the one edit this
/// command makes to a step: `headless: true` is renamed to `hide: true`, the
/// same setting under its new name, and `headless: false` is deleted, since
/// `hide: false` would force a pane — see [`rename_headless`]. The second
/// is a step shape this release refuses to load — see
/// [`crate::pipeline::Pipeline::retired_shape_problems`] — which refuses the
/// file, naming the step and the edit. Nothing is written to it.
fn pipelines(repo: &Repo, outcomes: &mut Vec<Outcome>, acts: &mut Vec<Act>) -> Result<()> {
    // `checkout`, not `root`: the pipelines are as tracked as the prompts
    // and the task skeletons `shipped_for` above already reads from there,
    // and a lane running in a linked worktree brings its own branch's copies
    // forward, not the main checkout's.
    let dir = crate::pipeline::Pipelines::dir_in(&repo.checkout);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // No pipeline directory is not a fault here. `doctor` is what says a
        // project has no pipelines; this command only brings files forward.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            outcomes.push(Outcome::blocked(
                crate::platform::relative(&repo.checkout, &dir),
                format!("could not be read ({err})"),
            ));
            return Ok(());
        }
    };

    // Sorted, so the report reads the same twice running: directory order is
    // the filesystem's business and differs between machines.
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(err) => {
                outcomes.push(Outcome::blocked(
                    crate::platform::relative(&repo.checkout, &dir),
                    format!("could not be read ({err})"),
                ));
                return Ok(());
            }
        };
        // The same two extensions the loader reads, or this would keep a file
        // current that nothing runs and leave a running one behind.
        if path
            .extension()
            .is_some_and(|ext| ext == "yml" || ext == "yaml")
        {
            paths.push(path);
        }
    }
    paths.sort();

    for path in paths {
        let shown = crate::platform::relative(&repo.checkout, &path);
        let mut on_disk = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                outcomes.push(Outcome::blocked(
                    &shown,
                    format!("could not be read ({err})"),
                ));
                continue;
            }
        };

        // Before the shape check below, which would otherwise refuse a file
        // for the one key this renames. Written only with the rest of the
        // file's plan: a file refused further down is left exactly as it was.
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut renamed = None;
        match rename_headless(&on_disk, Layout::Steps) {
            Renamed::Nothing => {}
            Renamed::Refused(ids) => {
                outcomes.push(Outcome::blocked(&shown, both_keys_refusal(&ids)));
                continue;
            }
            Renamed::Done { text, ids, dropped } => {
                on_disk = text;
                renamed = Some((ids, dropped));
            }
        }

        // A shape this release stopped loading — `loop: 0` is the one an
        // earlier release still ran — is refused by name, fenced or not, and
        // the file is left as it is. Apart from the `headless:` edit above,
        // `sync` never edits a step, and without this it would report the
        // upgrade done over a pipeline the next command refuses to load.
        if let Ok(pipeline) =
            serde_norway::from_str::<crate::pipeline::Pipeline>(crate::task::without_bom(&on_disk))
        {
            // A `headless:` still here after the rename sits on a line it
            // cannot read, such as a flow-style step: telling the person to
            // run `sync` again would send them round in a circle.
            let problems: Vec<String> = pipeline
                .retired_shape_problems()
                .into_iter()
                .map(|problem| {
                    pipeline
                        .steps
                        .iter()
                        .find(|step| {
                            step.headless.is_some()
                                && problem == crate::pipeline::headless_message(&step.id)
                        })
                        .map_or(problem.clone(), |step| unrenamed_message(&step.id))
                })
                .collect();
            if !problems.is_empty() {
                outcomes.push(Outcome::blocked(
                    &shown,
                    format!("will not load: {}", problems.join("; ")),
                ));
                continue;
            }
        }

        let region = crate::pipeline::KEY_BLOCK;
        let mut changed = renamed.is_some();
        match region.read(&on_disk) {
            // Half a fence is the one shape worth saying something about: the
            // lines under a start marker with no end could be anyone's, so
            // nothing is written and the missing marker is named. No markers
            // at all means the file never opted in, so its key reference is
            // kept as it is — a retired shape in it was already refused
            // above — see this function's own doc on why an unfenced file is
            // never ours to touch.
            None => {
                let opened = on_disk
                    .lines()
                    .any(|line| line.trim() == crate::assets::PIPELINE_KEYS_BEGIN);
                if opened {
                    outcomes.push(Outcome::blocked(
                        &shown,
                        format!(
                            "spoolway's key reference starts with `{}` and never ends — restore \
                             the `{}` marker, or delete the block and run this again",
                            crate::assets::PIPELINE_KEYS_BEGIN,
                            crate::assets::PIPELINE_KEYS_END
                        ),
                    ));
                    continue;
                }
                if renamed.is_none() {
                    outcomes.push(Outcome::Kept);
                }
            }
            Some(found) => match lookup(&Shipped::PipelineKeys, found) {
                Known::Current => outcomes.push(Outcome::Kept),
                // An older copy a release shipped: nothing in it is anyone's
                // to have meant, so the whole block is swapped and no line of
                // a person's is lost.
                Known::Older => match region.replace(&on_disk, crate::pipeline::key_block()) {
                    Some(next) => {
                        on_disk = next;
                        changed = true;
                        outcomes.push(Outcome::wrote(&shown, "key reference refreshed"));
                    }
                    None => {
                        outcomes.push(Outcome::blocked(&shown, "its block moved while we read it"));
                        continue;
                    }
                },
                // No release shipped it: a newer spoolway wrote it, or
                // somebody edited it. Rewriting would discard that, so the
                // file is left exactly as it is, the rename above included.
                Known::Unknown => {
                    outcomes.push(Outcome::blocked(&shown, refusal(&shown)));
                    continue;
                }
            },
        }

        if let Some((ids, dropped)) = renamed {
            outcomes.push(Outcome::wrote(&shown, rename_wrote(&ids, &dropped)));
            outcomes.push(rename_note(&shown, &name, &ids, &dropped, Layout::Steps));
        }
        if changed {
            acts.push(Act::write(path, &shown, on_disk));
        }
    }
    Ok(())
}

/// The private layer's pipeline patches, renamed the same way [`pipelines`]
/// renames the checkout's files.
///
/// A patch is `spoolway`'s own file and outside the checkout, but a
/// `headless:` in it is refused just as the tracked one is: the override is
/// skipped, with a notice, and the step runs as the tracked file says. So the
/// rename is made here, or the person's private `hide` would stay switched
/// off after the tracked files were fixed. A layer that cannot be found or
/// read is skipped quietly, as [`config`] skips it, since `doctor` is what
/// says an override is broken.
fn override_pipelines(repo: &Repo, outcomes: &mut Vec<Outcome>, acts: &mut Vec<Act>) {
    let Ok(overrides) = crate::overrides::dir_for(&repo.checkout) else {
        return;
    };
    let Ok(names) = crate::overrides::list_pipeline_patches(&overrides) else {
        return;
    };
    for name in names {
        let path = crate::overrides::pipeline_patch_path(&overrides, &name);
        rename_outside_checkout(
            repo,
            path,
            &name,
            Layout::Patch,
            "its override is skipped",
            outcomes,
            acts,
        );
    }
}

/// The private layer's own pipelines under `local/pipelines/`, renamed the
/// same way [`pipelines`] renames the tracked ones.
///
/// A private pipeline is read by the same loader as a tracked one, so a
/// `headless:` in it fails the whole load with the advice to run `sync`.
/// Without this pass `sync` would answer "Nothing updating." and every
/// command would go on failing. Only repo mode has a private layer; a layer
/// that cannot be found or read is skipped quietly, as [`override_pipelines`]
/// skips its own.
fn private_pipelines(repo: &Repo, outcomes: &mut Vec<Outcome>, acts: &mut Vec<Act>) {
    if !crate::local::is_repo_mode(&repo.checkout) {
        return;
    }
    let Ok(local) = crate::local::dir_for(&repo.checkout) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(crate::local::pipelines_dir(&local)) else {
        return;
    };
    // Sorted for the same reason [`pipelines`] sorts: the report reads the
    // same twice running. The two extensions are the ones the loader reads.
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
        })
        .collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        rename_outside_checkout(
            repo,
            path,
            &name,
            Layout::Steps,
            "will not load",
            outcomes,
            acts,
        );
    }
}

/// Rename `headless:` in one file the checkout does not track — a patch or a
/// private pipeline — and plan its write.
///
/// `consequence` is what the file does while it still names `headless:`, for
/// the refusal. A file whose rename leaves a `headless:` behind on a line
/// [`rename_headless`] cannot read is refused whole and written not at all,
/// as [`pipelines`] refuses a tracked one, so the person is never told to run
/// `sync` again over a key it will never rename.
fn rename_outside_checkout(
    repo: &Repo,
    path: PathBuf,
    name: &str,
    layout: Layout,
    consequence: &str,
    outcomes: &mut Vec<Outcome>,
    acts: &mut Vec<Act>,
) {
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let shown = crate::platform::relative(&repo.checkout, &path);
    let (text, renamed) = match rename_headless(&text, layout) {
        Renamed::Nothing => (text, None),
        Renamed::Refused(ids) => {
            outcomes.push(Outcome::blocked(&shown, both_keys_refusal(&ids)));
            return;
        }
        Renamed::Done { text, ids, dropped } => (text, Some((ids, dropped))),
    };
    let stuck = headless_left(&text, layout);
    if !stuck.is_empty() {
        let why = stuck
            .iter()
            .map(|id| unrenamed_message(id))
            .collect::<Vec<_>>()
            .join("; ");
        outcomes.push(Outcome::blocked(&shown, format!("{consequence}: {why}")));
        return;
    }
    if let Some((ids, dropped)) = renamed {
        outcomes.push(Outcome::wrote(&shown, rename_wrote(&ids, &dropped)));
        outcomes.push(rename_note(&shown, name, &ids, &dropped, layout));
        acts.push(Act::write(path, &shown, text));
    }
}

/// The ids of the steps in `text` that still carry `headless:`, read the way
/// the loader reads the file. Called on what [`rename_headless`] returned, so
/// anything it finds sits where the renamer could not reach it. Empty when
/// the text does not parse, since a file that is broken for another reason
/// is that reason's to report.
fn headless_left(text: &str, layout: Layout) -> Vec<String> {
    let text = crate::task::without_bom(text);
    match layout {
        Layout::Steps => serde_norway::from_str::<crate::pipeline::Pipeline>(text)
            .map(|pipeline| {
                pipeline
                    .steps
                    .iter()
                    .filter(|step| step.headless.is_some())
                    .map(|step| step.id.clone())
                    .collect()
            })
            .unwrap_or_default(),
        Layout::Patch => serde_norway::from_str::<crate::overrides::PipelinePatch>(text)
            .map(|patch| {
                patch
                    .steps
                    .iter()
                    .filter(|(_, fields)| fields.contains_key("headless"))
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The `wrote` line under a file [`rename_headless`] changed. A file whose
/// every `headless:` was `false` only lost keys, so it names no rename.
fn rename_wrote(ids: &[String], dropped: &[String]) -> &'static str {
    match (dropped.is_empty(), dropped.len() == ids.len()) {
        (true, _) => "`headless:` renamed to `hide:`",
        (false, true) => "`headless: false` removed",
        (false, false) => "`headless:` renamed to `hide:`, and `headless: false` removed",
    }
}

/// The note under a file whose steps [`rename_headless`] changed.
///
/// `ids` is every step it changed and `dropped` the ones among them that lost
/// a `headless: false` instead of gaining a `hide:`. In a pipeline file that
/// line only spelled out the default. In a patch it could have overridden the
/// tracked step's `headless: true`, whose `hide: true` now wins, so the note
/// says how to keep the pane instead.
fn rename_note(
    shown: &str,
    pipeline: &str,
    ids: &[String],
    dropped: &[String],
    layout: Layout,
) -> Outcome {
    let word = |ids: &[String]| if ids.len() == 1 { "step" } else { "steps" };
    let renamed: Vec<String> = ids
        .iter()
        .filter(|id| !dropped.contains(id))
        .cloned()
        .collect();
    let mut long = format!("migrated: {pipeline}:");
    let mut short = String::from("migrated:");
    if !renamed.is_empty() {
        let (word, listed) = (word(&renamed), renamed.join(", "));
        long.push_str(&format!(
            " `headless:` renamed to `hide:` on {word} {listed}"
        ));
        short.push_str(&format!(" headless renamed to hide on {word} {listed}"));
    }
    if !dropped.is_empty() {
        let (word, listed) = (word(dropped), dropped.join(", "));
        let join = if renamed.is_empty() { "" } else { ";" };
        long.push_str(&match layout {
            Layout::Steps => format!(
                "{join} `headless: false` removed from {word} {listed}, where it only spelled \
                 out the default"
            ),
            Layout::Patch => format!(
                "{join} `headless: false` removed from {word} {listed}; where the tracked step \
                 sets `hide: true`, that now wins, so set `hide: false` here to keep a pane"
            ),
        });
        short.push_str(&format!(
            "{join} headless false removed from {word} {listed}"
        ));
    }
    Outcome::migrated(shown, long, short)
}

/// Why `sync` still refuses a file whose `headless:` it could not rename:
/// the key sits on a line [`rename_headless`] does not read.
fn unrenamed_message(step_id: &str) -> String {
    format!(
        "step `{step_id}` declares `headless:` in a form `sync` cannot rename (flow style, a \
         quoted key, a `-` on a line of its own, or the step's only key); write `hide:` in \
         its place, or delete it if it is `false`."
    )
}

/// Why a file with `headless:` and `hide:` on one step is left unwritten.
/// Renaming one would leave two `hide:` lines, and dropping either would
/// choose a value the person never chose between.
fn both_keys_refusal(ids: &[String]) -> String {
    let (word, listed) = (
        if ids.len() == 1 { "step" } else { "steps" },
        ids.iter()
            .map(|id| format!("`{id}`"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    format!(
        "{word} {listed} declare{} both `headless:` and `hide:` — they set the same thing and \
         could disagree, so nothing was renamed; keep only `hide:`, then run this again",
        if ids.len() == 1 { "s" } else { "" }
    )
}

/// Which shape of file [`rename_headless`] reads.
#[derive(Clone, Copy)]
enum Layout {
    /// A pipeline file: `steps:` is a list, each item carrying its own `id:`.
    Steps,
    /// A patch under `overrides/pipelines/`: `steps:` is a map keyed by id.
    Patch,
}

/// What [`rename_headless`] does to the line of one `headless:` key.
enum Edit {
    /// Rename the key to `hide:`.
    Rename,
    /// Delete a `headless: false` line.
    Drop,
    /// Delete a `- headless: false` that opens a list item, and move the dash
    /// onto line `next`, the item's next key.
    DropOpening { next: usize },
}

/// What [`rename_headless`] found.
enum Renamed {
    /// No step carries `headless:`.
    Nothing,
    /// These steps carry both `headless:` and `hide:`.
    Refused(Vec<String>),
    /// The text with the key renamed or dropped, the ids of the steps it was
    /// on, and which of those had `headless: false` and lost the key instead.
    Done {
        text: String,
        ids: Vec<String>,
        dropped: Vec<String>,
    },
}

/// Rename the `headless:` key of every step in a pipeline file or patch to
/// `hide:`, and change nothing else, not one other byte.
///
/// The exception is `headless: false`, whose line is deleted. In 0.13.0 it
/// only spelled out the default and still followed `dispatch.hide_commands`,
/// while `hide: false` forces a pane. Renaming it would turn a step that
/// followed the project setting into one that ignores it. A `headless: false`
/// on the line that opens a list item (`- headless: false`) is deleted too, and
/// the dash moves onto the item's next key. Only a step with no other key is
/// left for [`unrenamed_message`]. Any spelling the loader reads as false
/// counts, so `False` is dropped like `false`.
///
/// A line edit, not a re-serialisation, so comments, ordering, quoting and
/// line endings survive, and it touches a line only when its key is exactly
/// `headless:` at the indentation of the step's own keys. That excludes a
/// `headless` inside a comment, a `description:` or a `run:` value, and the
/// lines of a block scalar, which sit deeper than the key that opens them.
/// A step written in flow style (`- {id: a, headless: true}`), a quoted key
/// (`"headless": true`) and a list item opened by a `-` on a line of its own
/// have no such line and are left as they are; the file then still names
/// `headless:`, and the caller refuses it with [`unrenamed_message`].
fn rename_headless(text: &str, layout: Layout) -> Renamed {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let indent = |line: &str| line.len() - line.trim_start_matches(' ').len();
    let content = |line: &str| {
        let trimmed = line.trim();
        !trimmed.is_empty() && !trimmed.starts_with('#')
    };
    let is_key = |rest: &str, key: &str| {
        rest.strip_prefix(key)
            .and_then(|after| after.strip_prefix(':'))
            .is_some_and(|after| after.is_empty() || after.starts_with([' ', '\t', '\r', '\n']))
    };

    // The span of `steps:`: from the line after it to the next top-level key.
    let Some(start) = lines
        .iter()
        .position(|line| indent(line) == 0 && is_key(line.trim_start_matches('\u{feff}'), "steps"))
    else {
        return Renamed::Nothing;
    };
    let end = lines[start + 1..]
        .iter()
        .position(|line| content(line) && indent(line) == 0 && !line.starts_with('-'))
        .map_or(lines.len(), |at| start + 1 + at);

    // Each step is the lines from its opening line up to the next one. Its keys
    // sit at `key_indent`: in a list, where the text after the dash starts;
    // in a patch, where the line under the id starts.
    struct Step {
        id: String,
        key_indent: usize,
        from: usize,
        to: usize,
    }
    let mut steps: Vec<Step> = Vec::new();
    let body = start + 1..end;
    let Some(first) = body.clone().find(|&at| content(lines[at])) else {
        return Renamed::Nothing;
    };
    let top = indent(lines[first]);
    for at in body.clone().filter(|&at| content(lines[at])) {
        let line = lines[at];
        if indent(line) != top {
            continue;
        }
        let key_indent = match layout {
            Layout::Steps => {
                let Some(rest) = line[top..].strip_prefix('-') else {
                    continue;
                };
                top + 1 + (rest.len() - rest.trim_start_matches(' ').len())
            }
            Layout::Patch => {
                match body
                    .clone()
                    .filter(|&next| next > at)
                    .find(|&n| content(lines[n]))
                {
                    Some(next) if indent(lines[next]) > top => indent(lines[next]),
                    _ => continue,
                }
            }
        };
        let id = match layout {
            Layout::Steps => String::new(),
            Layout::Patch => line[top..]
                .split(':')
                .next()
                .unwrap_or_default()
                .trim_matches(['"', '\''])
                .to_string(),
        };
        steps.push(Step {
            id,
            key_indent,
            from: at,
            to: end,
        });
    }
    for pair in 0..steps.len().saturating_sub(1) {
        steps[pair].to = steps[pair + 1].from;
    }

    let mut edits: Vec<(usize, Edit)> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    let mut both: Vec<String> = Vec::new();
    for step in &steps {
        let (mut headless, mut hide, mut id) = (None, false, step.id.clone());
        for (at, line) in lines.iter().enumerate().take(step.to).skip(step.from) {
            if !content(line) {
                continue;
            }
            // The first line of a list item carries the dash before its key.
            let column = step.key_indent;
            let on_key = match at == step.from && matches!(layout, Layout::Steps) {
                true => line.len() >= column && line.is_char_boundary(column),
                false => indent(line) == column,
            };
            if !on_key {
                continue;
            }
            let rest = &line[column..];
            if is_key(rest, "headless") {
                // What the key is set to, less a trailing comment and the
                // line ending. Whatever the loader reads as the boolean
                // false is dropped, so `False` and `FALSE` go with `false`.
                let value = rest["headless:".len()..]
                    .split(" #")
                    .next()
                    .unwrap_or_default()
                    .trim();
                let drop = matches!(serde_norway::from_str::<bool>(value), Ok(false));
                let opens_item = at == step.from && matches!(layout, Layout::Steps);
                let edit = match (drop, opens_item) {
                    (false, _) => Edit::Rename,
                    (true, false) => Edit::Drop,
                    // The next key takes the dash. A step with no key after
                    // this one has nothing to take it, so it is left for
                    // [`unrenamed_message`].
                    (true, true) => {
                        let next = (at + 1..step.to).find(|&n| content(lines[n]));
                        match next.filter(|&n| indent(lines[n]) == step.key_indent) {
                            Some(next) => Edit::DropOpening { next },
                            None => continue,
                        }
                    }
                };
                headless = Some((at, edit));
            } else if is_key(rest, "hide") {
                hide = true;
            } else if is_key(rest, "id") && matches!(layout, Layout::Steps) {
                id = rest["id:".len()..]
                    .split(" #")
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .trim_matches(['"', '\''])
                    .to_string();
            }
        }
        match (headless, hide) {
            (Some(_), true) => both.push(id),
            (Some((at, edit)), false) => {
                if !matches!(edit, Edit::Rename) {
                    dropped.push(id.clone());
                }
                edits.push((at, edit));
                ids.push(id);
            }
            (None, _) => {}
        }
    }
    if !both.is_empty() {
        return Renamed::Refused(both);
    }
    if edits.is_empty() {
        return Renamed::Nothing;
    }
    let mut out = String::with_capacity(text.len());
    for (at, line) in lines.iter().enumerate() {
        // The line after a dropped `- headless: false` is the item's new
        // first line, so the dash moves onto it, in the column it had.
        let takes_dash = edits
            .iter()
            .any(|(_, edit)| matches!(edit, Edit::DropOpening { next } if *next == at));
        if takes_dash {
            out.push_str(&line[..top]);
            out.push('-');
            out.push_str(&line[top + 1..]);
            continue;
        }
        match edits.iter().find(|(line, _)| *line == at) {
            Some((_, Edit::Drop | Edit::DropOpening { .. })) => {}
            Some((_, Edit::Rename)) => {
                let key = line.find("headless").expect("an edited line names the key");
                out.push_str(&line[..key]);
                out.push_str("hide");
                out.push_str(&line[key + "headless".len()..]);
            }
            None => out.push_str(line),
        }
    }
    Renamed::Done {
        text: out,
        ids,
        dropped,
    }
}

// ---------------------------------------------------------------------------
// The skill stamp is gone: [`crate::known_copies`] tells a stale shipped copy
// from a hand edit now, so nothing here reads or writes a per-project record
// of one any more. [`remove_skill_stamp`] is the one thing left to do with
// the name — clear out the file an upgraded project may still be carrying
// from before this change.
// ---------------------------------------------------------------------------

/// The skill stamp's old file name, under [`Repo::home`] — kept only so a
/// leftover one can be found and removed.
const SKILL_STAMP_FILE: &str = "skill-stamp";

/// Delete a project's leftover skill stamp, if one is still there from
/// before skill files became spoolway's outright. Best-effort: a stamp
/// nobody reads any more is clutter, not a fact worth failing a sync over.
pub(crate) fn remove_skill_stamp(home: &Path) {
    let _ = std::fs::remove_file(home.join(SKILL_STAMP_FILE));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    // The file steps only plan their writes now. Each shim below runs one step
    // the way `scan` does — plan it, then make the plan unless `args` is a dry
    // run — so a test can still ask what that one step leaves on disk.
    fn finish(args: &SyncArgs, acts: Vec<Act>) -> Result<()> {
        match args.dry_run {
            true => Ok(()),
            false => {
                check(&acts)?;
                apply(acts)
            }
        }
    }
    fn config(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
        let mut acts = Vec::new();
        super::config(repo, outcomes, &mut acts)?;
        finish(args, acts)
    }
    fn templates(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
        let mut acts = Vec::new();
        super::templates(repo, outcomes, &mut acts)?;
        finish(args, acts)
    }
    fn pipelines(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
        let mut acts = Vec::new();
        super::pipelines(repo, outcomes, &mut acts)?;
        finish(args, acts)
    }
    fn override_pipelines(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
        let mut acts = Vec::new();
        super::override_pipelines(repo, outcomes, &mut acts);
        finish(args, acts)
    }
    fn private_pipelines(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
        let mut acts = Vec::new();
        super::private_pipelines(repo, outcomes, &mut acts);
        finish(args, acts)
    }
    fn retired_templates(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
        let mut acts = Vec::new();
        super::retired_templates(repo, outcomes, &mut acts)?;
        finish(args, acts)
    }
    fn skills(
        repo: &Repo,
        args: &SyncArgs,
        home_mode: bool,
        outcomes: &mut Vec<Outcome>,
    ) -> Result<()> {
        let mut acts = Vec::new();
        super::skills(repo, home_mode, outcomes, &mut acts)?;
        super::user_level(outcomes, &mut acts)?;
        finish(args, acts)
    }
    fn retired_skills(
        repo: &Repo,
        args: &SyncArgs,
        home_mode: bool,
        outcomes: &mut Vec<Outcome>,
    ) -> Result<()> {
        let mut acts = Vec::new();
        super::retired_skills(repo, home_mode, outcomes, &mut acts)?;
        finish(args, acts)
    }

    fn fixture(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let root = crate::scratch::root(&format!("sync-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let config = Config::default();
        std::fs::create_dir_all(root.join(crate::config::TASK_TEMPLATES_DIR)).unwrap();
        std::fs::create_dir_all(root.join(".spoolway/templates")).unwrap();
        let home = root.join(".home");
        (
            Repo {
                borrowed: false,
                checkout: root.to_path_buf(),
                root: root.to_path_buf(),
                config,
                home,
            },
            root,
        )
    }

    /// `sync` waits for the edit lock a `config set` holds, then writes: the
    /// config it rebuilds is read after that `set` has saved, not before.
    #[test]
    fn sync_waits_for_the_edit_lock() {
        let (repo, _root) = fixture("edit-lock");
        let held = crate::lock::EditLock::acquire(&repo.edit_lock_path()).unwrap();
        let args = SyncArgs {
            dry_run: true,
            replace: Vec::new(),
        };
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(300));
                drop(held);
            });
            run(&repo, &args).unwrap();
        });
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(250),
            "sync ran while the edit lock was held"
        );
    }

    /// A checkout in home mode: it has no `.spoolway/` folder, and a
    /// workspace's `project.toml` under a scratch `$HOME` lists it, so
    /// `repo.setup_dir()` is the workspace's `config/` — which starts out
    /// empty. Returns the scratch `$HOME` to run under
    /// [`crate::platform::test_home::with_home`].
    fn home_mode_fixture(name: &str) -> (Repo, crate::scratch::ScratchRoot, PathBuf) {
        let root = crate::scratch::root(&format!("sync-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join(".ws-home");
        let workspace = home.join(".spoolway").join("home-mode-ws");
        std::fs::create_dir_all(workspace.join("config")).unwrap();
        std::fs::write(
            workspace.join(crate::repo::BINDING_FILE),
            format!(
                "id = \"home-mode-ws\"\nclones = [{{ root = {:?}, dispatcher = \"api\" }}]\n",
                root.display(),
            ),
        )
        .unwrap();
        let repo = Repo {
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: Config::default(),
            home: root.join(".home"),
            borrowed: false,
        };
        (repo, root, home)
    }

    /// The workspace's `config/` for a [`home_mode_fixture`] repo.
    fn workspace_config_dir(repo: &Repo, home: &Path) -> PathBuf {
        crate::platform::test_home::with_home(home, || repo.setup_dir())
    }

    /// A linked worktree whose branch has no `.spoolway/` reads the main
    /// checkout's setup. `sync` there would write a setup the branch never
    /// had, so it refuses, names the main checkout, and writes nothing.
    #[test]
    fn sync_refuses_in_a_worktree_that_borrows_the_main_checkouts_setup() {
        let (mut repo, root) = fixture("borrowed");
        repo.borrowed = true;

        let err = run(&repo, &args()).expect_err("a borrowed setup is not this worktree's");
        let said = format!("{err:#}");
        assert!(
            said.contains("no `.spoolway/` on its branch")
                && said.contains(&root.display().to_string()),
            "{said}"
        );
        assert!(
            !root.join(".spoolway/config.toml").exists(),
            "nothing may be written before the refusal"
        );
    }

    fn args() -> SyncArgs {
        SyncArgs {
            dry_run: false,
            replace: Vec::new(),
        }
    }

    /// [`run_asking_with`] over `keys` as typed input, with an inert
    /// terminal and `SIGINT` guard, returning what it drew — the panel and
    /// the cancel line, never `run`'s report, which prints to the real
    /// stdout.
    fn ask(repo: &Repo, args: &SyncArgs, in_lane: bool, tty: bool, keys: &str) -> String {
        let mut input = std::io::Cursor::new(keys.as_bytes().to_vec());
        let mut out = Vec::new();
        run_asking_with(
            repo,
            args,
            in_lane,
            tty,
            &mut input,
            &mut out,
            crate::platform::TermGuard::inert,
            SigintGuard::inert,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Home mode's promise — nothing written into the checkout or its
    /// `.git` — is kept by `init` (`src/commands/init.rs`'s own `home_mode`
    /// skip), but `sync` has no such guard at all: the `.gitignore` step
    /// and the project skill refresh both write straight into
    /// `repo.checkout` whether or not a workspace claims it.
    #[test]
    fn sync_writes_nothing_into_a_home_mode_checkout() {
        let (repo, _root_guard, home) = home_mode_fixture("home-mode");

        // A tracked `.gitignore` carrying spoolway's own marked block —
        // `sync`'s `ignores` step removes this in repo mode, but must leave
        // it alone in a home-mode checkout.
        let gitignore = crate::gitignore::file(&repo.checkout);
        std::fs::write(
            &gitignore,
            format!(
                "{}\n/.spoolway/\n{}\n",
                crate::assets::IGNORE_BEGIN,
                crate::assets::IGNORE_END
            ),
        )
        .unwrap();

        // An already-installed, stale skill folder — `sync`'s `skills` step
        // refreshes this in repo mode, but must leave it alone here too.
        let first = crate::cli::Provider::Claude
            .plan(&repo.checkout)
            .into_iter()
            .next()
            .unwrap();
        std::fs::create_dir_all(first.path.parent().unwrap()).unwrap();
        std::fs::write(&first.path, "stale, from an older release\n").unwrap();

        crate::platform::test_home::with_home(&home, || {
            run(&repo, &args()).unwrap();
        });

        assert!(
            std::fs::read_to_string(&gitignore)
                .unwrap()
                .contains(crate::assets::IGNORE_BEGIN),
            "home mode must leave the checkout's own .gitignore untouched"
        );
        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            "stale, from an older release\n",
            "home mode must leave an installed skill in the checkout untouched"
        );
    }

    /// The workspace's `config/` is where the fixture says it is, and the
    /// checkout carries no `.spoolway/` — so every test below that finds
    /// something under `config/` is proving the workspace was written, not
    /// the checkout.
    #[test]
    fn the_home_mode_fixture_resolves_to_the_workspace_config() {
        let (repo, _root_guard, home) = home_mode_fixture("home-fixture");
        let config_dir = workspace_config_dir(&repo, &home);
        assert_eq!(
            config_dir,
            home.join(".spoolway").join("home-mode-ws").join("config")
        );
        assert!(!repo.checkout.join(".spoolway").exists());
    }

    /// A `Skeleton` for the home-mode tests below; `skeletons()` ships none.
    fn test_skeleton(path: &'static str) -> crate::skeleton::Skeleton {
        crate::skeleton::Skeleton {
            path,
            shipped: "<h1>hi</h1>\n<script type=\"application/json\" id=\"fixture\">\n{ \"a\": 1 }\n</script>\n",
            region: crate::skeleton::Region::Script("fixture"),
            history: &[],
        }
    }

    /// In home mode `--replace .spoolway/...` lands in the workspace's
    /// `config/`, with its backup beside it, and the checkout gains nothing.
    #[test]
    fn a_home_mode_replace_resolves_a_setup_path_into_the_workspace() {
        let (repo, _root_guard, home) = home_mode_fixture("home-replace");
        let config_dir = workspace_config_dir(&repo, &home);
        let prompt = config_dir.join("prompts/implementer/PROMPT.md");
        std::fs::create_dir_all(prompt.parent().unwrap()).unwrap();
        std::fs::write(&prompt, "mine\n").unwrap();

        crate::platform::test_home::with_home(&home, || {
            replace(
                &repo,
                &SyncArgs {
                    replace: vec![".spoolway/prompts/implementer/PROMPT.md".to_string()],
                    dry_run: false,
                },
            )
            .unwrap();
            assert_eq!(
                shown_path(&repo, &prompt),
                "~/.spoolway/home-mode-ws/config/prompts/implementer/PROMPT.md"
            );
            assert_eq!(
                shown_path(&repo, &repo.checkout.join("src/main.rs")),
                "src/main.rs"
            );
            assert_eq!(replace_footer(&repo, false), None);
            assert_eq!(replace_footer(&repo, true), Some(DRY_RUN));
        });

        assert_eq!(
            std::fs::read_to_string(&prompt).unwrap(),
            crate::assets::prompt("implementer").unwrap().body
        );
        assert_eq!(
            std::fs::read_to_string(prompt.with_extension("md.bak")).unwrap(),
            "mine\n"
        );
        assert!(!repo.checkout.join(".spoolway").exists());
    }

    /// Repo mode still joins onto the checkout and still points at `git diff`.
    #[test]
    fn a_repo_mode_replace_keeps_the_git_diff_line() {
        let (repo, _root_guard) = fixture("repo-replace-footer");
        assert_eq!(
            replace_footer(&repo, false),
            Some("`git diff` shows exactly what changed.")
        );
        assert_eq!(
            skeleton_path(&repo, "docs/page.html"),
            Some(repo.checkout.join("docs/page.html"))
        );
        assert_eq!(
            skeleton_path(&repo, ".spoolway/page.html"),
            Some(repo.checkout.join(".spoolway/page.html"))
        );
    }

    /// In home mode a skeleton under `.spoolway/` is written, and matched by
    /// `shipped_for`, in the workspace; one anywhere else is skipped and
    /// nothing is written into the checkout.
    #[test]
    fn a_home_mode_skeleton_resolves_through_the_setup_folder() {
        let (repo, _root_guard, home) = home_mode_fixture("home-skeleton");
        let config_dir = workspace_config_dir(&repo, &home);
        let skeletons = [
            test_skeleton(".spoolway/page.html"),
            test_skeleton("outside.html"),
        ];

        crate::platform::test_home::with_home(&home, || {
            let mut outcomes = Vec::new();
            let mut acts = Vec::new();
            templates_of(&repo, &skeletons, &mut outcomes, &mut acts).unwrap();
            assert_eq!(outcomes.len(), 1, "the outside skeleton is skipped");
            check(&acts).unwrap();
            apply(acts).unwrap();

            assert!(shipped_for_among(&repo, &config_dir.join("page.html"), &skeletons).is_some());
            assert!(
                shipped_for_among(&repo, &repo.checkout.join("outside.html"), &skeletons).is_none()
            );
        });

        assert!(config_dir.join("page.html").is_file());
        assert!(!repo.checkout.join("outside.html").exists());
        assert!(!repo.checkout.join(".spoolway").exists());
    }

    /// A missing `config.toml` is written under the workspace's `config/`,
    /// and no `.spoolway/` appears in the checkout.
    #[test]
    fn a_home_mode_sync_writes_a_missing_config_into_the_workspace() {
        let (repo, _root_guard, home) = home_mode_fixture("home-missing-config");
        let config_dir = workspace_config_dir(&repo, &home);
        assert!(!config_dir.join(crate::config::CONFIG_FILE).exists());

        crate::platform::test_home::with_home(&home, || {
            run(&repo, &args()).unwrap();
        });

        assert!(
            config_dir.join(crate::config::CONFIG_FILE).is_file(),
            "config.toml should be written under the workspace's config/"
        );
        assert!(
            !repo.checkout.join(".spoolway").exists(),
            "home mode must not create .spoolway/ in the checkout"
        );
    }

    /// A stale `config.toml` and a pipeline file with a stale key reference,
    /// both in the workspace's `config/`, are rewritten in place. The
    /// checkout gains nothing.
    #[test]
    fn a_home_mode_sync_rewrites_stale_workspace_files_in_place() {
        let (repo, _root_guard, home) = home_mode_fixture("home-stale");
        let config_dir = workspace_config_dir(&repo, &home);

        let config_path = config_dir.join(crate::config::CONFIG_FILE);
        std::fs::write(
            &config_path,
            "[dispatch]\nbackend = \"headless\"\nlane_quiet = \"45m\"\n",
        )
        .unwrap();
        let pipelines_dir = config_dir.join(crate::pipeline::PIPELINE_DIR);
        std::fs::create_dir_all(&pipelines_dir).unwrap();
        let pipeline_path = pipelines_dir.join("hotfix.yml");
        std::fs::write(
            &pipeline_path,
            format!(
                "# hotfix — mine.\n\n{}\n\n\
                 steps:\n  - id: work\n    run: x\n    on_pass: done\n",
                older_fence()
            ),
        )
        .unwrap();

        crate::platform::test_home::with_home(&home, || {
            run(&repo, &args()).unwrap();
        });

        let config_after = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            config_after.contains("lane_quiet = \"45m\""),
            "{config_after}"
        );
        assert!(
            config_after.contains("auto_commit"),
            "a setting the file lacked should be added: {config_after}"
        );
        let pipeline_after = std::fs::read_to_string(&pipeline_path).unwrap();
        assert!(
            pipeline_after.contains(crate::pipeline::key_block()),
            "{pipeline_after}"
        );
        assert!(!pipeline_after.contains(crate::known_copies::older::key_block()));
        assert!(pipeline_after.contains("# hotfix — mine."));
        assert!(
            !repo.checkout.join(".spoolway").exists(),
            "home mode must not create .spoolway/ in the checkout"
        );
    }

    /// The confirm panel's width is bounded even when a path in it is not:
    /// every row `screen::boxed` draws — borders included — stays at or
    /// under 80 columns.
    #[test]
    fn the_confirm_panel_is_at_most_eighty_columns_wide() {
        let long = "a/very/long/path/".repeat(6) + "SKILL.md";
        let body = panel_body(&[], &[long.as_str()], &BTreeMap::new(), &[]);
        for line in crate::screen::panel(TITLE, &body, "[enter] apply") {
            assert!(
                line.chars().count() <= 80,
                "{} columns: {line}",
                line.chars().count()
            );
        }
    }

    /// Enter at the panel writes the files and draws
    /// nothing more than the panel itself; a stray key before it is ignored.
    #[test]
    fn asking_sync_writes_only_once_enter_is_pressed() {
        let (repo, _root_guard) = fixture("ask-enter");
        let drawn = ask(&repo, &args(), false, true, "q\r");
        assert!(drawn.contains(TITLE), "{drawn}");
        assert!(drawn.contains("config.toml"), "{drawn}");
        assert!(drawn.contains("[enter] apply"), "{drawn}");
        assert!(drawn.contains("[esc] cancel"), "{drawn}");
        assert!(!drawn.contains(CANCELLED), "{drawn}");
        assert!(Config::path_in(&repo.checkout).is_file());
    }

    /// Esc, and ctrl-c — which reaches `read_key` as a read that failed,
    /// the same `None` as input running out — write nothing and say so.
    #[test]
    fn asking_sync_writes_nothing_on_esc_or_ctrl_c() {
        for (name, keys) in [("ask-esc", "\x1b"), ("ask-ctrl-c", "")] {
            let (repo, _root_guard) = fixture(name);
            let drawn = ask(&repo, &args(), false, true, keys);
            assert!(drawn.contains(TITLE), "{name}: {drawn}");
            assert!(
                drawn.ends_with(&format!("{CANCELLED}\n")),
                "{name}: {drawn}"
            );
            assert!(!Config::path_in(&repo.checkout).exists(), "{name}");
        }
    }

    /// No terminal or a lane: written straight away with no panel
    /// and no key read — `keys` is empty, so a read would cancel and leave
    /// the config unwritten rather than pass by accident.
    #[test]
    fn asking_sync_writes_straight_away_with_nobody_to_answer() {
        for (name, in_lane, tty) in [("ask-no-tty", false, false), ("ask-lane", true, true)] {
            let (repo, _root_guard) = fixture(name);
            let drawn = ask(&repo, &args(), in_lane, tty, "");
            assert!(drawn.is_empty(), "{name}: {drawn}");
            assert!(Config::path_in(&repo.checkout).is_file(), "{name}");
        }
    }

    /// `--dry-run` never draws the panel, and neither does a sync with
    /// nothing left to write.
    #[test]
    fn asking_sync_draws_no_panel_for_a_dry_run_or_nothing_to_do() {
        let (repo, _root_guard) = fixture("ask-dry-run");
        let dry = SyncArgs {
            dry_run: true,
            replace: Vec::new(),
        };
        assert!(ask(&repo, &dry, false, true, "").is_empty());
        assert!(!Config::path_in(&repo.checkout).exists());

        run(&repo, &args()).unwrap();
        assert!(ask(&repo, &args(), false, true, "").is_empty());
    }

    fn outcome_lines(outcomes: &[Outcome]) -> Vec<String> {
        outcomes
            .iter()
            .map(|outcome| match outcome {
                Outcome::Wrote { path, detail } => format!("wrote {path} ({detail})"),
                Outcome::Migrated { path, report, .. } => format!("migrated {path} ({report})"),
                Outcome::Kept => "kept".to_string(),
                Outcome::Blocked { path, why } => format!("blocked {path}: {why}"),
                Outcome::Removed { path, why } => format!("removed {path}: {why}"),
            })
            .collect()
    }

    /// The report is paths, and one line saying what the paths do not.
    ///
    /// Both halves matter: a list with no sentence leaves "did it eat my
    /// config?" unanswered, and a sentence with a paragraph under it is what
    /// this replaced.
    #[test]
    fn the_report_is_the_paths_and_one_line() {
        let (repo, _root_guard) = fixture("report-shape");
        std::fs::write(
            crate::config::Config::path_in(&repo.root),
            "[dispatch]\nlane_quiet = \"45m\"\n",
        )
        .unwrap();

        let outcomes = scan(&repo, &args()).unwrap();
        // Every record, repeats kept: this rewrite leaves two `Migrated`
        // notes for the same file, the settings it adds and the
        // `key_in_names` value it sets.
        let written: Vec<&str> = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                Outcome::Wrote { path, .. } | Outcome::Migrated { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        let (deduped, _) = dedup_paths(&outcomes);

        assert!(written.contains(&".spoolway/config.toml"), "{written:?}");
        // One rewrite can add a setting and drop a retired one at once. Two
        // records, one file, and the report says the file once.
        assert_eq!(
            deduped.len(),
            written
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            "a path printed twice reads as two files: {deduped:?}"
        );
        assert!(KEPT.contains("config values"), "{KEPT}");
        assert!(KEPT.contains("prompts"), "{KEPT}");
    }

    /// A rewrite that adds settings names them, once under the one config row,
    /// in the report and in the confirm panel, and a rewrite that adds none
    /// says nothing of the kind. Before the note was a `Migrated`, the added
    /// keys were a `Wrote` detail that no surface printed.
    #[test]
    fn a_rewrite_names_the_settings_it_adds_and_only_then() {
        let (repo, _root_guard) = fixture("names-added");
        std::fs::write(
            crate::config::Config::path_in(&repo.root),
            "[dispatch]\nlane_quiet = \"45m\"\n",
        )
        .unwrap();

        let outcomes = scan(&repo, &args()).unwrap();
        let (wrote, removed) = dedup_paths(&outcomes);
        let notes = migration_notes(&outcomes);
        let config = ".spoolway/config.toml";
        assert_eq!(wrote.iter().filter(|p| **p == config).count(), 1);

        let (report, panel) = notes
            .get(config)
            .into_iter()
            .flatten()
            .find(|(report, _)| report.contains("this spoolway has and the file did not"))
            .unwrap_or_else(|| panic!("no note naming the added settings: {notes:?}"));
        assert!(report.contains("dispatch.lane_layout"), "{report}");
        assert!(panel.starts_with("added "), "{panel}");

        let body = panel_body(&[], &wrote, &notes, &removed);
        let row = body.iter().position(|l| l.contains(config)).unwrap();
        assert!(body[row + 1].contains("(added "), "{body:?}");
        assert!(
            body.iter().all(|l| l.chars().count() <= MAX_LINE),
            "{body:?}"
        );

        // A file that is current but for one retired key is rewritten, adds
        // nothing, and so says nothing of added settings.
        let current = crate::config::Config::default().render().unwrap();
        let stale = current.replacen("[dispatch]\n", "[dispatch]\nherdr_mode = true\n", 1);
        assert_ne!(stale, current);
        std::fs::write(crate::config::Config::path_in(&repo.root), stale).unwrap();
        let outcomes = scan(&repo, &args()).unwrap();
        let notes = migration_notes(&outcomes);
        let reports: Vec<&str> = notes.values().flatten().map(|(r, _)| *r).collect();
        assert!(
            reports
                .iter()
                .any(|r| r.contains("retired setting(s) dropped")),
            "the fixture must be a rewrite: {:?}",
            outcome_lines(&outcomes)
        );
        assert!(
            reports
                .iter()
                .all(|r| !r.contains("this spoolway has and the file did not")),
            "{reports:?}"
        );
    }

    /// A dry run with nothing to do used to say `DRY_RUN` anyway — "Run
    /// without --dry-run to take it" over an empty scan, which took nothing
    /// and had nothing to take. `nothing_to_do` must win over `dry_run`.
    #[test]
    fn a_dry_run_with_nothing_to_do_says_so_instead_of_offering_to_run_it() {
        assert_eq!(closing_line(true, true, Refused::Nothing), NOOP);
        assert_eq!(closing_line(false, true, Refused::Nothing), NOOP);
        assert_eq!(closing_line(true, false, Refused::Nothing), DRY_RUN);
        assert_eq!(closing_line(false, false, Refused::Nothing), KEPT);
        // A refusal with nothing else written is not "Nothing updating."
        assert_eq!(closing_line(true, true, Refused::Files), REFUSED_ONLY);
        assert_eq!(closing_line(false, true, Refused::Files), REFUSED_ONLY);
        assert_eq!(closing_line(false, false, Refused::Files), KEPT);
        assert!(!NOOP.contains("--dry-run"), "{NOOP}");
    }

    /// A run whose only changes are plan pages must not close with `KEPT` or
    /// `NOOP`, which talk about config files: a dry run says `DRY_RUN`, a real
    /// run says nothing, and a refused plan page still picks `REFUSED_ONLY`.
    #[test]
    fn a_run_that_only_moved_plan_pages_closes_without_a_config_line() {
        assert_eq!(closing(true, true, Refused::Nothing, true), Some(DRY_RUN));
        assert_eq!(closing(false, true, Refused::Nothing, true), None);
        assert_eq!(
            closing(false, true, Refused::Files, true),
            Some(REFUSED_ONLY)
        );
        // Config files were written too, or nothing at all happened: the
        // ordinary lines stand.
        assert_eq!(closing(false, false, Refused::Nothing, true), Some(KEPT));
        assert_eq!(closing(false, true, Refused::Nothing, false), Some(NOOP));
    }

    /// The deprecation note prints under its own `note` word, each sentence
    /// starting a line and the fix wrapped under the first, so it can never
    /// be read as a file sync wrote or refused.
    #[test]
    fn the_stack_note_prints_under_its_own_word_one_sentence_a_line() {
        let raw = "steps:\n  - id: handover\n    run: spoolway stack\n    on_pass: done\n";
        let mut pipelines = std::collections::BTreeMap::new();
        for name in ["default", "bugfix"] {
            pipelines.insert(name.to_string(), serde_norway::from_str(raw).unwrap());
        }
        let pipelines = crate::pipeline::Pipelines {
            pipelines,
            ignored_overrides: Vec::new(),
            clashes: Default::default(),
        };
        let sentences = crate::commands::stack_deprecation(&pipelines).unwrap();
        assert_eq!(
            stack_note_lines(&sentences),
            [
                "note    `spoolway stack` is deprecated and may be removed in a future release.",
                "        Still run by: bugfix · handover, default · handover.",
                "        Run `spoolway init` to add the `create-pr` prompt, then replace each step",
                "        with an agent step on it — see docs/migrations.md",
            ]
        );
    }

    /// The sync nothing used to perform: a config written before a setting
    /// existed gains it, with its note, and keeps every value and comment it
    /// already had.
    #[test]
    fn a_sync_brings_a_config_forward_without_touching_its_values() {
        let (repo, _root_guard) = fixture("config-forward");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(
            &path,
            "[dispatch]\nbackend = \"headless\"\nlane_quiet = \"45m\"\n",
        )
        .unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(after.contains("backend = \"headless\""));
        assert!(after.contains("lane_quiet = \"45m\""));
        assert!(after.contains("auto_commit"));
        // The explanation is not a comment standing above the key any more —
        // it is one row of the reference table on top of the whole file.
        assert!(after.contains("Whether spoolway commits"));
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .any(|line| line.starts_with("migrated") && line.contains("auto_commit")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// A config whose reference table is one an older release wrote is stale:
    /// the file is rewritten with this binary's table and keeps its values.
    #[test]
    fn an_older_reference_table_is_replaced_and_the_values_kept() {
        let (repo, _root_guard) = fixture("config-older-table");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(
            &path,
            format!(
                "{}\n[dispatch]\nlane_quiet = \"45m\"\n",
                crate::known_copies::older::config_reference()
            ),
        )
        .unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(
            after.starts_with(&crate::confkv::reference_table()),
            "{after}"
        );
        assert!(after.contains("lane_quiet = \"45m\""), "{after}");
        assert!(refusals(&outcomes).is_empty());
    }

    /// A table that is none of the shipped ones was written by a newer
    /// spoolway or edited by hand. The file is left byte for byte as it is,
    /// values and unknown keys included, and refused with the remedy.
    #[test]
    fn a_reference_table_no_release_shipped_refuses_the_config() {
        let (repo, _root_guard) = fixture("config-unknown-table");
        let path = crate::config::Config::path_in(&repo.root);
        let before = format!(
            "{}# a row a newer spoolway added\n\n[dispatch]\nlane_quiet = \"45m\"\n",
            crate::confkv::reference_table()
                .trim_end_matches('\n')
                .to_string()
                + "\n"
        );
        std::fs::write(&path, &before).unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let shown = ".spoolway/config.toml";
        assert_eq!(
            refusals(&outcomes),
            [(shown, crate::known_copies::refusal(shown).as_str())]
        );
        assert!(dedup_paths(&outcomes).0.is_empty());
    }

    /// A config with no table at all has nothing that was edited: it gains
    /// this binary's table like any setting the file never named. A comment
    /// standing straight on a setting is that setting's, not a table.
    #[test]
    fn a_config_with_no_reference_table_gains_one() {
        let (repo, _root_guard) = fixture("config-no-table");
        let path = crate::config::Config::path_in(&repo.root);
        for before in [
            "[dispatch]\nlane_quiet = \"45m\"\n",
            "# my note on the section\n[dispatch]\nlane_quiet = \"45m\"\n",
        ] {
            std::fs::write(&path, before).unwrap();
            let mut outcomes = Vec::new();
            config(&repo, &args(), &mut outcomes).unwrap();
            let after = std::fs::read_to_string(&path).unwrap();
            assert!(
                after.starts_with(&crate::confkv::reference_table()),
                "{after}"
            );
            assert!(refusals(&outcomes).is_empty(), "{before:?}");
        }
    }

    /// A key `config set` creates is already where `sync` would put it: the
    /// file a fresh `init` wrote and then `config set` added a profile's
    /// `concurrency` to is not behind, so `sync` leaves it byte for byte as it
    /// is and `doctor` has nothing to ask for.
    #[test]
    fn a_sync_leaves_a_config_that_config_set_gave_a_concurrency_as_it_is() {
        let (repo, _root_guard) = fixture("config-set-concurrency");
        let written = Config::default();
        written.save(&repo.root).unwrap();
        let loaded = Config::load(&repo.root).unwrap();
        let updated = crate::confkv::set(&loaded, "agents.claude.concurrency", "2").unwrap();
        updated
            .save_key(&repo.root, "agents.claude.concurrency")
            .unwrap();
        let path = Config::path_in(&repo.root);
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(before.contains("concurrency = 2"), "{before}");

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .all(|line| !line.starts_with("wrote") && !line.starts_with("migrated")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// A file an earlier release wrote, with its blank `prompt_cache_ttl`
    /// rows, has them turned into the commented example, and a second sync
    /// leaves that file as it is: the comment is there exactly once.
    #[test]
    fn a_sync_turns_the_blank_cache_lifetime_rows_into_one_comment() {
        let (repo, _root_guard) = fixture("blank-cache-ttl-rows");
        let mut written = Config::default();
        written.agents.retain(|_, profile| profile.kind == "claude");
        let old = format!(
            "{}\n[models.\"claude-*\"]\nprompt_cache_ttl = \"\"\n\n\
             [models.\"hand-*\"]\nprompt_cache_ttl = \"\"\n\n\
             [models.\"*haiku*\"]\nslots = 2\n",
            written.render().unwrap().replace("\n[models]\n", "\n")
        );
        let path = Config::path_in(&repo.root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &old).unwrap();

        let mut first = Vec::new();
        config(&repo, &args(), &mut first).unwrap();
        let said = outcome_lines(&first).join("\n");
        assert!(!said.contains("retired"), "{said}");
        assert!(
            said.contains("empty `prompt_cache_ttl` row for claude-* is now the commented"),
            "{said}"
        );
        assert!(
            said.contains("holding only an empty `prompt_cache_ttl` was dropped for hand-*"),
            "{said}"
        );
        let once = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            once.matches("# [models.\"claude-*\"]\n").count(),
            1,
            "{once}"
        );
        assert!(
            once.contains("# prompt_cache_ttl = \"1h\"\n\n[models.\"*haiku*\"]"),
            "{once}"
        );
        assert!(!once.contains("\nprompt_cache_ttl = \"\""), "{once}");
        assert!(!once.lines().any(|line| line == "[models.\"claude-*\"]"));

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), once);
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .all(|line| !line.starts_with("wrote") && !line.starts_with("migrated")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// The same holds for a `[models."<glob>"]` field `config set` creates:
    /// `sync` leaves the file as it is.
    #[test]
    fn a_sync_leaves_a_config_that_config_set_gave_a_models_row_as_it_is() {
        let (repo, _root_guard) = fixture("config-set-models");
        let written = Config::default();
        written.save(&repo.root).unwrap();
        let loaded = Config::load(&repo.root).unwrap();
        let updated = crate::confkv::set(&loaded, "models.foo-model.input", "5.0").unwrap();
        updated
            .save_key(&repo.root, "models.foo-model.input")
            .unwrap();
        let path = Config::path_in(&repo.root);
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(before.contains("foo-model"), "{before}");

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .all(|line| !line.starts_with("wrote") && !line.starts_with("migrated")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// A `config.toml` that is a symlink is brought forward in the file it
    /// points at, and stays a link: `sync` runs after every upgrade, and a
    /// rename over the link would leave the shared copy behind.
    #[cfg(unix)]
    #[test]
    fn a_sync_brings_a_symlinked_config_forward_in_its_target() {
        let (repo, _root_guard) = fixture("config-symlinked");
        let path = crate::config::Config::path_in(&repo.root);
        let target = repo.root.join("shared-config.toml");
        std::fs::write(&target, "[dispatch]\nlane_quiet = \"45m\"\n").unwrap();
        std::fs::remove_file(&path).ok();
        std::os::unix::fs::symlink(&target, &path).unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();

        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "config.toml is no longer a symlink"
        );
        let after = std::fs::read_to_string(&target).unwrap();
        assert!(after.contains("lane_quiet = \"45m\""), "{after}");
        assert!(after.contains("auto_commit"), "{after}");
    }

    /// The other half of the contract: a comment is spoolway's, so one
    /// somebody wrote where a note used to stand is dropped — the
    /// explanation for that key lives in the reference table on top now —
    /// and the report says which key stood under it.
    #[test]
    fn a_comment_that_is_not_the_binarys_is_rewritten_and_named() {
        let (repo, _root_guard) = fixture("config-comment");
        let path = crate::config::Config::path_in(&repo.root);
        let mine = "# Ten minutes: our lanes are quick and we watch the board.";
        std::fs::write(&path, format!("[dispatch]\n{mine}\nlane_quiet = \"10m\"\n")).unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(!after.contains(mine));
        // Still explained — just in the table, not standing above the key.
        assert!(after.contains("How long a lane may say nothing"));
        assert!(
            after.contains("lane_quiet = \"10m\""),
            "the value is the one thing kept"
        );
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .any(|line| line.starts_with("wrote") && line.contains("dispatch.lane_quiet")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// A dry run is a reading, on this file as on every other.
    #[test]
    fn a_dry_run_says_what_the_config_would_gain_and_writes_nothing() {
        let (repo, _root_guard) = fixture("config-dry");
        let path = crate::config::Config::path_in(&repo.root);
        let before = "[dispatch]\nlane_quiet = \"45m\"\n";
        std::fs::write(&path, before).unwrap();

        let mut outcomes = Vec::new();
        config(
            &repo,
            &SyncArgs {
                dry_run: true,
                ..args()
            },
            &mut outcomes,
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(!outcomes.is_empty());
    }

    /// A config this binary cannot even parse used to be swallowed as a
    /// quiet `Outcome::Blocked` — reported only to `doctor`, never printed
    /// by `sync` itself — so a dry run over a config like this said "Dry
    /// run: nothing was written" and exited 0, as if the file had been read
    /// and found current. `run`'s `?` on `scan` means the whole command now
    /// fails instead, naming the file, which is what `main`'s own `Err`
    /// handling turns into a non-zero exit.
    #[test]
    fn a_dry_run_fails_loudly_on_a_config_that_does_not_parse() {
        let (repo, _root_guard) = fixture("config-garbage");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(&path, "garbage = [\n").unwrap();

        let result = scan(
            &repo,
            &SyncArgs {
                dry_run: true,
                ..args()
            },
        );
        let Err(err) = result else {
            panic!("a config sync cannot parse must fail the scan, not get swallowed");
        };
        let said = format!("{err:#}");
        assert!(said.contains("config.toml"), "{said}");
        assert!(said.contains("spoolway config edit"), "{said}");
    }

    /// `dispatch.interval` is retired hard enough that `DispatchConfig`'s own
    /// `deny_unknown_fields` would refuse it outright if nothing stripped it
    /// first — but `Config::load` now does exactly that, the same strip
    /// `sync` has always used, with a note naming `spoolway sync`. `sync` is
    /// still the one path that writes the key away for good: it drops it,
    /// rewrites the reference header around its removal, and leaves the rest
    /// of the file exactly as it read it.
    #[test]
    fn a_config_still_naming_dispatch_interval_loads_past_it_and_sync_drops_it_for_good() {
        let (repo, _root_guard) = fixture("config-retired-interval");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(
            &path,
            "[dispatch]\nbackend = \"headless\"\ninterval = \"45s\"\n",
        )
        .unwrap();

        assert!(
            crate::config::Config::load(&repo.root).is_ok(),
            "an ordinary load must still load past the retired key"
        );

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(!after.contains("interval"), "{after}");
        assert!(after.contains("backend = \"headless\""), "{after}");
        assert!(
            crate::config::Config::load(&repo.root).is_ok(),
            "the rewritten file must load cleanly now that the key is gone"
        );
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .any(|line| line.starts_with("migrated") && line.contains("dispatch.interval")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// `visible_lanes = 3` is written whatever the file held, so a retired
    /// `keep_finished_lanes` has to be converted and dropped in the same
    /// rewrite: left beside a default-valued `visible_lanes` it would be read
    /// as the old choice again, or survive as an unknown key.
    #[test]
    fn a_sync_turns_keep_finished_lanes_into_visible_lanes_and_drops_the_old_key() {
        for (old, written) in [
            ("true", "visible_lanes = \"all\""),
            ("false", "visible_lanes = 1"),
        ] {
            let (repo, _root_guard) = fixture(&format!("config-keep-finished-{old}"));
            let path = crate::config::Config::path_in(&repo.root);
            std::fs::write(
                &path,
                format!("[dispatch]\nbackend = \"headless\"\nkeep_finished_lanes = {old}\n"),
            )
            .unwrap();

            let mut outcomes = Vec::new();
            config(&repo, &args(), &mut outcomes).unwrap();
            let after = std::fs::read_to_string(&path).unwrap();

            assert!(after.lines().any(|line| line == written), "{old}: {after}");
            assert!(
                !after
                    .lines()
                    .any(|line| !line.starts_with('#') && line.contains("keep_finished_lanes")),
                "{old}: {after}"
            );
            assert_eq!(after.matches("\nvisible_lanes = ").count(), 1, "{after}");
        }
    }

    /// A key this binary does not know — one typo'd inside a table, a whole
    /// misspelt table, and a model row's own — survives a full re-render, each
    /// with the comment written above it, while a retired top-level table is
    /// still dropped. The report names what it kept.
    #[test]
    fn an_unknown_key_survives_a_sync() {
        let (repo, _root_guard) = fixture("config-unknown-kept");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(
            &path,
            "# my own explanation\nfuture_key = 1\n\n\
             [dispatch]\nbackend = \"headless\"\n# quiet, spelt wrong\nlane_quite = \"5m\"\n\n\
             # a table I misspelt\n[unatended]\nmax_cost = 3\n\n\
             [models.\"qwen3.5\"]\nslots = 2\nflavour = \"x\"\n\n\
             [sandbox]\nenabled = true\n",
        )
        .unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(after.contains("future_key = 1"), "{after}");
        assert!(
            after.contains("# my own explanation\nfuture_key"),
            "{after}"
        );
        assert!(
            after.contains("# quiet, spelt wrong\nlane_quite = \"5m\""),
            "{after}"
        );
        assert!(
            after.contains("# a table I misspelt\n[unatended]"),
            "{after}"
        );
        assert!(after.contains("flavour = \"x\""), "{after}");
        assert!(!after.contains("[sandbox"), "{after}");
        assert!(
            crate::config::Config::load(&repo.root).is_ok(),
            "the rewritten file must still load"
        );
        let lines = outcome_lines(&outcomes);
        assert!(
            lines.iter().any(|line| line.contains("kept 4")
                && line.contains("dispatch.lane_quite")
                && line.contains("unatended")
                && line.contains("models.qwen3.5.flavour")),
            "{lines:?}"
        );
    }

    /// An unknown key in an old-spelling `[pricing."<glob>"]` row follows its
    /// row to `[models]` and is reported as kept; the row's known keys, which
    /// moved, and an aliased `cache_write` are not reported as dropped.
    #[test]
    fn a_pricing_row_keeps_its_unknown_key_and_renamed_keys_are_not_called_dropped() {
        let (repo, _root_guard) = fixture("config-pricing-unknown");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(
            &path,
            "[pricing.\"qwen3\"]\nslots = 2\n# explain\nflavour = 1\n\n\
             [pricing.\"m\"]\ncache_write = 2.0\n",
        )
        .unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(after.contains("# explain\nflavour = 1"), "{after}");
        assert!(after.contains("cache_write_5m"), "{after}");
        let lines = outcome_lines(&outcomes);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("kept 1") && line.contains("models.qwen3.flavour")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("dropped")),
            "a moved or renamed key is not a dropped one: {lines:?}"
        );
    }

    /// A second sync over a file holding unknown tables and keys changes
    /// nothing: a carried table's place must not depend on the file it came
    /// from, or every sync would move it again.
    #[test]
    fn a_second_sync_over_unknown_tables_changes_nothing() {
        let (repo, _root_guard) = fixture("config-unknown-stable");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(
            &path,
            "[dispatch]\nbackend = \"headless\"\n\n\
             [pricing.\"qwen3\"]\nslots = 2\nflavour = 1\n\n\
             [pricing.\"m\"]\ncache_write = 2.0\n\n\
             [sandbox]\nenabled = true\n\n[unatended]\nx = 1\n",
        )
        .unwrap();

        config(&repo, &args(), &mut Vec::new()).unwrap();
        let once = std::fs::read_to_string(&path).unwrap();
        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        let twice = std::fs::read_to_string(&path).unwrap();

        assert_eq!(once, twice, "a second sync moved something");
        assert!(
            !outcome_lines(&outcomes)
                .iter()
                .any(|line| line.starts_with("wrote")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// `issue_tracking.on_fail` is the other key retired hard enough that it
    /// would otherwise fail `deny_unknown_fields` outright — `Config::load`
    /// strips it first now, the same as `dispatch.interval` above. Unlike
    /// that one this retirement also moves the table it lived in — from
    /// after every `[agents.*]`/`[models.*]` table to directly under
    /// `[watch]`, following `Config`'s own field order (see
    /// `crate::config::Config::render`) — and `sync`'s summary names both
    /// changes, not only the dropped key.
    #[test]
    fn a_config_still_naming_issue_tracking_on_fail_loses_it_and_the_section_moves() {
        let (repo, _root_guard) = fixture("config-retired-on-fail");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(
            &path,
            "[watch]\ndirs = []\n\
             [agents.claude]\nkind = \"claude\"\n\
             [issue_tracking]\nhook = \"github.sh\"\nproject_key = \"o/r\"\non_fail = \"pause\"\n\
             key_in_names = true\n",
        )
        .unwrap();

        assert!(
            crate::config::Config::load(&repo.root).is_ok(),
            "an ordinary load must still load past the retired key"
        );

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(!after.contains("on_fail"), "{after}");
        assert!(after.contains("hook = \"github.sh\""), "{after}");
        assert!(after.contains("project_key = \"o/r\""), "{after}");
        assert!(after.contains("key_in_names = true"), "{after}");
        assert!(
            crate::config::Config::load(&repo.root).is_ok(),
            "the rewritten file must load cleanly now that the key is gone"
        );

        // The section moved: `[issue_tracking]` now stands ahead of
        // `[agents.claude]`, not behind it.
        let issue_tracking_at = after.find("[issue_tracking]").unwrap();
        let agents_at = after.find("[agents.claude]").unwrap();
        assert!(
            issue_tracking_at < agents_at,
            "[issue_tracking] must move ahead of [agents.*]:\n{after}"
        );

        let lines = outcome_lines(&outcomes);
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("migrated") && line.contains("issue_tracking.on_fail")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("[issue_tracking] moved directly under [watch]")),
            "{lines:?}"
        );

        // Named where a person reads it: an ordinary `Wrote`'s detail is
        // never printed, so both changes have to reach the notes `run`'s
        // report and the confirm panel draw under the config's own line.
        let notes = migration_notes(&outcomes);
        let shown = notes.values().flatten().collect::<Vec<_>>();
        assert!(
            shown.iter().any(|(report, panel)| {
                report.contains("issue_tracking.on_fail removed")
                    && report.contains("[issue_tracking] moved directly under [watch]")
                    && panel.contains("on_fail removed")
                    && panel.contains("[issue_tracking] moved under [watch]")
            }),
            "{shown:?}"
        );
    }

    /// `dispatch.worktree_root` held a real path: `sync` drops the key, the
    /// same as any other retired setting, but also names the old directory —
    /// nothing else tells a project its worktrees are still sitting there
    /// once the key that named them is gone.
    #[test]
    fn a_config_naming_a_real_worktree_root_names_the_old_directory() {
        let (repo, _root_guard) = fixture("config-retired-worktree-root-real");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(&path, "[dispatch]\nworktree_root = \"/old/worktrees\"\n").unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("worktree_root"), "{after}");

        let lines = outcome_lines(&outcomes);
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("migrated") && line.contains("dispatch.worktree_root")),
            "{lines:?}"
        );

        let notes = migration_notes(&outcomes);
        let shown = notes.values().flatten().collect::<Vec<_>>();
        assert!(
            shown.iter().any(|(report, panel)| {
                report.contains("dispatch.worktree_root removed")
                    && report.contains("/old/worktrees")
                    && panel.contains("/old/worktrees")
                    && !report.contains("yours to remove")
                    && !report.contains("safe to remove")
            }),
            "a folder with no queued task left in it must never be called removable \
             unconditionally — it must still never claim either: {shown:?}"
        );
    }

    /// The same two retired keys written inside inline tables: `sync` drops
    /// both and prints the same migrated notes a plain table earns. `sync` rewrites the whole file from the struct, so the
    /// tables come back in the shape the binary writes; the inline-preserving
    /// removal is what the load path uses, tested in `confdoc`.
    #[test]
    fn retired_keys_inside_inline_tables_are_dropped_with_the_same_notes() {
        let (repo, _root_guard) = fixture("config-retired-inline-tables");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(
            &path,
            "dispatch = { lane_quiet = \"25m\", worktree_root = \"/old/worktrees\" }\n\
             issue_tracking = { hook = \"\", project_key = \"\", on_fail = \"pause\", \
             key_in_names = false }\n",
        )
        .unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("worktree_root"), "{after}");
        assert!(!after.contains("on_fail"), "{after}");
        assert!(after.contains("lane_quiet"), "{after}");

        let notes = migration_notes(&outcomes);
        let shown = notes.values().flatten().collect::<Vec<_>>();
        assert!(
            shown
                .iter()
                .any(|(report, _)| report.contains("issue_tracking.on_fail removed")),
            "{shown:?}"
        );
        assert!(
            shown.iter().any(|(report, _)| {
                report.contains("dispatch.worktree_root removed")
                    && report.contains("/old/worktrees")
            }),
            "{shown:?}"
        );
    }

    /// A queued task still has its own worktree cut under the old path —
    /// `sync` must name that task rather than call the old directory
    /// anybody's to remove, the same promise `doctor`'s own
    /// `worktree_root_note` keeps.
    #[test]
    fn a_config_naming_a_real_worktree_root_names_a_queued_task_still_using_it() {
        let (repo, _root_guard) = fixture("config-retired-worktree-root-queued");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(&path, "[dispatch]\nworktree_root = \"/old/worktrees\"\n").unwrap();

        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        std::fs::write(
            repo.queue_dir().join("still-queued.md"),
            "---\nid: still-queued\nstage: queued\nworktree_path: /old/worktrees/still-queued\n\
             ---\n",
        )
        .unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();

        let notes = migration_notes(&outcomes);
        let shown = notes.values().flatten().collect::<Vec<_>>();
        assert!(
            shown.iter().any(|(report, panel)| {
                report.contains("still-queued")
                    && panel.contains("still-queued")
                    && !report.contains("yours to remove")
                    && !report.contains("safe to remove")
            }),
            "{shown:?}"
        );
    }

    /// A `worktree_root` written with a leading `~/` names the same folder as
    /// the absolute path a task's `worktree_path` records, so `sync` must
    /// name a queued task whose worktree sits under it.
    #[test]
    fn a_worktree_root_written_with_a_tilde_names_a_queued_task_under_it() {
        let Some(home) = crate::platform::home_dir() else {
            return;
        };
        let (repo, _root_guard) = fixture("config-retired-worktree-root-tilde");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(&path, "[dispatch]\nworktree_root = \"~/p25-wt\"\n").unwrap();

        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        std::fs::write(
            repo.queue_dir().join("z1.md"),
            format!(
                "---\nid: z1\nstage: paused\nworktree_path: {}\n---\n",
                home.join("p25-wt").join("task-z1").display()
            ),
        )
        .unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();

        let notes = migration_notes(&outcomes);
        let shown = notes.values().flatten().collect::<Vec<_>>();
        assert!(
            shown.iter().any(|(report, panel)| {
                report.contains("z1")
                    && panel.contains("z1")
                    && !report.contains("no queued task has a worktree there")
            }),
            "{shown:?}"
        );
    }

    /// A blank `dispatch.worktree_root` was never anybody's decision — the
    /// ordinary dropped-key line covers it, and there is nothing further
    /// worth a dedicated note.
    #[test]
    fn a_config_naming_a_blank_worktree_root_gets_no_extra_note() {
        let (repo, _root_guard) = fixture("config-retired-worktree-root-blank");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(&path, "[dispatch]\nworktree_root = \"\"\n").unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();

        let lines = outcome_lines(&outcomes);
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("migrated") && line.contains("dispatch.worktree_root")),
            "{lines:?}"
        );
        // The config never named `key_in_names` either, which earns its own
        // note, and the dropped-key line names `worktree_root`; only the note
        // about the directory is ruled out here.
        assert!(
            migration_notes(&outcomes)
                .values()
                .flatten()
                .all(|(report, _)| !report.contains("its worktrees were cut")),
            "a blank value must not earn the dedicated directory note"
        );
    }

    /// `spoolway config override` setting a live key must never reach the
    /// tracked file through a sync, and a retired key sitting in the same
    /// override must be dropped from the layer itself rather than left to
    /// print "override ignored" forever.
    #[test]
    fn a_config_override_never_leaks_into_the_tracked_file_and_a_retired_key_drops_from_it() {
        let (repo, _root_guard) = fixture("config-override-no-leak");
        crate::scratch::stamped(&repo.root);
        let home = crate::scratch::root("sync-config-override-no-leak-home");
        let _ = std::fs::remove_dir_all(&home);

        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(&path, "[dispatch]\nlane_quiet = \"15m\"\n").unwrap();

        crate::platform::test_home::with_home(&home, || {
            let overrides = crate::overrides::dir_for(&repo.root).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            std::fs::write(
                crate::overrides::config_patch_path(&overrides),
                "[dispatch]\nlane_quiet = \"20m\"\nworktree_root = \"/old/worktrees\"\n",
            )
            .unwrap();

            let mut outcomes = Vec::new();
            config(&repo, &args(), &mut outcomes).unwrap();

            let after = std::fs::read_to_string(&path).unwrap();
            assert!(
                after.contains("lane_quiet = \"15m\""),
                "the tracked value must survive a sync untouched: {after}"
            );
            assert!(
                !after.contains("20m"),
                "the private override's value must never reach the tracked file: {after}"
            );

            let override_after =
                std::fs::read_to_string(crate::overrides::config_patch_path(&overrides)).unwrap();
            assert!(
                override_after.contains("lane_quiet = \"20m\""),
                "a live override key must stay in the layer: {override_after}"
            );
            assert!(
                !override_after.contains("worktree_root"),
                "a retired key must be dropped from the layer too: {override_after}"
            );

            let lines = outcome_lines(&outcomes);
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("retired override key")
                        && line.contains("dispatch.worktree_root")),
                "{lines:?}"
            );
        });

        std::fs::remove_dir_all(&home).ok();
    }

    /// `models.<glob>.exclusive` is retired: `spoolway sync` drops it from the
    /// tracked file, and from the private layer too, even under a glob that
    /// holds a dot of its own.
    #[test]
    fn a_retired_models_exclusive_key_is_dropped_from_the_file_and_the_layer() {
        let (repo, _root_guard) = fixture("config-retired-model-exclusive");
        crate::scratch::stamped(&repo.root);
        let home = crate::scratch::root("sync-retired-model-exclusive-home");
        let _ = std::fs::remove_dir_all(&home);

        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(
            &path,
            "[models.\"*Ornith-1.5-35B-A3B\"]\nslots = 3\nexclusive = true\nlocal = true\n",
        )
        .unwrap();

        crate::platform::test_home::with_home(&home, || {
            let overrides = crate::overrides::dir_for(&repo.root).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            std::fs::write(
                crate::overrides::config_patch_path(&overrides),
                "[models.\"*Ornith-1.5-35B-A3B\"]\nexclusive = true\nslots = 2\n",
            )
            .unwrap();

            let mut outcomes = Vec::new();
            config(&repo, &args(), &mut outcomes).unwrap();

            let after = std::fs::read_to_string(&path).unwrap();
            assert!(!after.contains("exclusive"), "{after}");
            assert!(after.contains("slots = 3"), "{after}");

            let layer =
                std::fs::read_to_string(crate::overrides::config_patch_path(&overrides)).unwrap();
            assert!(!layer.contains("exclusive"), "{layer}");
            assert!(layer.contains("slots = 2"), "{layer}");
        });

        std::fs::remove_dir_all(&home).ok();
    }

    /// A `worktree_root` set only in the private override layer is dropped
    /// too, and must be reported the way one dropped from `config.toml` is:
    /// the folder it named, and the queued tasks still under it.
    #[test]
    fn an_override_only_worktree_root_names_the_folder_and_its_queued_tasks() {
        let (repo, _root_guard) = fixture("config-override-worktree-root");
        crate::scratch::stamped(&repo.root);
        let home = crate::scratch::root("sync-config-override-worktree-root-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::write(
            crate::config::Config::path_in(&repo.root),
            "[dispatch]\nlane_quiet = \"15m\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        std::fs::write(
            repo.queue_dir().join("z1.md"),
            "---\nid: z1\nstage: paused\nworktree_path: /old/worktrees/task-z1\n---\n",
        )
        .unwrap();

        crate::platform::test_home::with_home(&home, || {
            let overrides = crate::overrides::dir_for(&repo.root).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            std::fs::write(
                crate::overrides::config_patch_path(&overrides),
                "[dispatch]\nworktree_root = \"/old/worktrees\"\n",
            )
            .unwrap();

            let mut outcomes = Vec::new();
            config(&repo, &args(), &mut outcomes).unwrap();

            let notes = migration_notes(&outcomes);
            let shown = notes.values().flatten().collect::<Vec<_>>();
            assert!(
                shown.iter().any(|(report, panel)| {
                    report.contains("/old/worktrees")
                        && report.contains("z1")
                        && panel.contains("z1")
                }),
                "{shown:?}"
            );
        });

        std::fs::remove_dir_all(&home).ok();
    }

    /// `sync --dry-run` must report a retired override key as dropped
    /// without actually writing the layer — the same promise it keeps for
    /// the tracked file. Before this fix the override write ran
    /// unconditionally, so a dry run rewrote `overrides/config.toml` and
    /// then printed "Dry run: nothing was written" over it.
    #[test]
    fn a_dry_run_reports_a_retired_override_key_without_dropping_it() {
        let (repo, _root_guard) = fixture("config-override-dry-run");
        crate::scratch::stamped(&repo.root);
        let home = crate::scratch::root("sync-config-override-dry-run-home");
        let _ = std::fs::remove_dir_all(&home);

        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(&path, "[dispatch]\nlane_quiet = \"15m\"\n").unwrap();

        crate::platform::test_home::with_home(&home, || {
            let overrides = crate::overrides::dir_for(&repo.root).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            let override_path = crate::overrides::config_patch_path(&overrides);
            let before = "[dispatch]\nworktree_root = \"/old/worktrees\"\n";
            std::fs::write(&override_path, before).unwrap();

            let mut outcomes = Vec::new();
            config(
                &repo,
                &SyncArgs {
                    dry_run: true,
                    ..args()
                },
                &mut outcomes,
            )
            .unwrap();

            assert_eq!(
                std::fs::read_to_string(&override_path).unwrap(),
                before,
                "a dry run must never write the private layer"
            );

            let lines = outcome_lines(&outcomes);
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("retired override key")
                        && line.contains("dispatch.worktree_root")),
                "a dry run must still say what it would drop: {lines:?}"
            );
        });

        std::fs::remove_dir_all(&home).ok();
    }

    /// Run from a dispatcher lane's worktree, `sync` leaves the private
    /// override layer alone, even when it holds a retired key, and says why.
    /// The layer lives in the project home, so a lane that rewrote it would
    /// change the person's own setup with nothing to review or revert.
    #[test]
    fn a_lane_leaves_a_retired_override_key_in_the_layer() {
        let (mut repo, _root_guard) = fixture("config-override-lane");
        crate::scratch::stamped(&repo.root);
        let home = crate::scratch::root("sync-config-override-lane-home");
        let _ = std::fs::remove_dir_all(&home);
        let lane = repo.worktree_root().join("task-lane");
        std::fs::create_dir_all(&lane).unwrap();
        repo.checkout = lane;
        std::fs::create_dir_all(
            crate::config::Config::path_in(&repo.checkout)
                .parent()
                .unwrap(),
        )
        .unwrap();
        std::fs::write(
            crate::config::Config::path_in(&repo.checkout),
            "[dispatch]\nlane_quiet = \"15m\"\n",
        )
        .unwrap();

        crate::platform::test_home::with_home(&home, || {
            let overrides = crate::overrides::dir_for(&repo.checkout).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            let override_path = crate::overrides::config_patch_path(&overrides);
            let before = "[dispatch]\nworktree_root = \"/old/worktrees\"\n";
            std::fs::write(&override_path, before).unwrap();

            let mut outcomes = Vec::new();
            config(&repo, &args(), &mut outcomes).unwrap();
            assert_eq!(std::fs::read_to_string(&override_path).unwrap(), before);
            let lines = outcome_lines(&outcomes);
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("a lane does not change the project's setup")),
                "{lines:?}"
            );
        });

        std::fs::remove_dir_all(&home).ok();
    }

    /// A mistake in a private override — a typo, not a key this binary
    /// retired — must never be deleted on the project's behalf: it is left
    /// in the layer, printing "override ignored" until the person who wrote
    /// it fixes the spelling themselves.
    #[test]
    fn a_typo_in_the_override_is_left_in_the_layer_not_deleted() {
        let (repo, _root_guard) = fixture("config-override-typo");
        crate::scratch::stamped(&repo.root);
        let home = crate::scratch::root("sync-config-override-typo-home");
        let _ = std::fs::remove_dir_all(&home);

        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(&path, "[dispatch]\nlane_quiet = \"15m\"\n").unwrap();

        crate::platform::test_home::with_home(&home, || {
            let overrides = crate::overrides::dir_for(&repo.root).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            let override_path = crate::overrides::config_patch_path(&overrides);
            // `lane_quiett` earns a "did you mean"; `lane_quite` earns none,
            // and was the one sync deleted before the drop took only keys on
            // `config::RETIRED_KEYS`.
            std::fs::write(
                &override_path,
                "[dispatch]\nlane_quiett = \"20m\"\nlane_quite = \"1m\"\n",
            )
            .unwrap();

            let mut outcomes = Vec::new();
            config(&repo, &args(), &mut outcomes).unwrap();

            let override_after = std::fs::read_to_string(&override_path).unwrap();
            assert!(
                override_after.contains("lane_quiett") && override_after.contains("lane_quite "),
                "a typo must survive a sync untouched, not be deleted as though retired: \
                 {override_after}"
            );

            let lines = outcome_lines(&outcomes);
            assert!(
                !lines
                    .iter()
                    .any(|line| line.contains("retired override key")),
                "a typo must never be reported as a dropped retired key: {lines:?}"
            );
        });

        std::fs::remove_dir_all(&home).ok();
    }

    /// Run from a linked worktree, `sync` writes the checkout it was run
    /// in, not the main checkout's root — the whole point of the fix this
    /// bug describes. `config` is the sharpest case: it joins `repo.root`
    /// directly instead of going through `Config::path_in(&repo.checkout)`
    /// the way every other tracked-control-plane accessor already does.
    #[test]
    fn config_is_written_to_the_checkout_not_the_root_when_they_differ() {
        let root = crate::scratch::root("sync-config-checkout-vs-root");
        let _ = std::fs::remove_dir_all(&root);
        let checkout = root.join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        let home = root.join(".home");
        let repo = Repo {
            borrowed: false,
            checkout: checkout.clone(),
            root: root.to_path_buf(),
            config: Config::default(),
            home,
        };

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();

        assert!(
            crate::config::Config::path_in(&checkout).exists(),
            "config.toml should land under the checkout the command was run in"
        );
        assert!(
            !crate::config::Config::path_in(&root).exists(),
            "config.toml must not be written under repo.root when it differs from checkout"
        );
    }

    /// The rest of `scan()` — `ignores`, `skills` and `retired_skills` — has the
    /// same bug `config` does: each joined `repo.root` directly instead of
    /// `repo.checkout`. One fixture exercises all four at once, plus
    /// `replace`, which is not part of `scan()` but has the sharpest form of
    /// the bug since it matched a `repo.root`-built path against a
    /// `repo.checkout`-based accessor. Every assertion is positive — the
    /// checkout's own file actually changed — not just "nothing landed under
    /// root": a function that silently does nothing under a missing/
    /// uninstalled root would pass a root-only check without ever having
    /// read `checkout` at all.
    #[test]
    fn scan_and_replace_write_the_checkout_not_the_root_when_they_differ() {
        let root = crate::scratch::root("sync-scan-checkout-vs-root");
        let _ = std::fs::remove_dir_all(&root);
        let checkout = root.join("checkout");
        std::fs::create_dir_all(checkout.join(crate::config::TASK_TEMPLATES_DIR)).unwrap();
        let home = root.join(".home");
        let repo = Repo {
            borrowed: false,
            checkout: checkout.clone(),
            root: root.to_path_buf(),
            config: Config::default(),
            home,
        };

        // `ignores`: spoolway's marked block, still in the checkout's
        // `.gitignore`.
        let gitignore = crate::gitignore::file(&checkout);
        std::fs::write(
            &gitignore,
            format!(
                "{}\n/.spoolway/\n{}\n",
                crate::assets::IGNORE_BEGIN,
                crate::assets::IGNORE_END
            ),
        )
        .unwrap();

        // `skills`: an already-installed, stale claude skill to refresh.
        let claude_dir = crate::cli::Provider::Claude.skills_dir(&checkout);
        let first = crate::cli::Provider::Claude
            .plan(&checkout)
            .into_iter()
            .next()
            .unwrap();
        std::fs::create_dir_all(first.path.parent().unwrap()).unwrap();
        std::fs::write(
            &first.path,
            crate::known_copies::older::skill(crate::cli::Provider::Claude, "spoolway-plan"),
        )
        .unwrap();

        // `retired_skills`: a directory this binary no longer ships.
        let retired = claude_dir.join(crate::install::RETIRED_SKILLS[0].0);
        std::fs::create_dir_all(&retired).unwrap();

        // `replace`: a task template this project no longer keeps, named
        // relative to the checkout — the join this test exists to cover is
        // never reached from an absolute path.
        let template = repo.task_templates_dir().join("bugfix.md");
        std::fs::write(&template, "not what we ship\n").unwrap();
        let template_relative = template
            .strip_prefix(&checkout)
            .unwrap()
            .display()
            .to_string();

        scan(&repo, &args()).unwrap();
        replace(
            &repo,
            &SyncArgs {
                replace: vec![template_relative],
                ..args()
            },
        )
        .unwrap();

        assert!(
            !std::fs::read_to_string(crate::gitignore::file(&checkout))
                .unwrap()
                .contains(crate::assets::IGNORE_BEGIN),
            "ignores must remove spoolway's block from the checkout's own .gitignore"
        );
        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            first.contents,
            "skills must refresh the checkout's own stale skill file"
        );
        assert!(
            !retired.is_dir(),
            "retired_skills must remove the checkout's own copy of a retired skill"
        );
        assert_eq!(
            std::fs::read_to_string(&template).unwrap(),
            crate::assets::task_template("bugfix").unwrap(),
            "replace must rewrite the checkout's own copy"
        );

        assert!(
            !crate::gitignore::file(&root).exists(),
            "the .gitignore rewrite must not touch repo.root"
        );
        for provider in <crate::cli::Provider as clap::ValueEnum>::value_variants() {
            assert!(
                !provider.skills_dir(&root).exists(),
                "no provider's skills directory should exist under repo.root"
            );
        }
    }

    /// A hook script is a command, not prose — `spoolway init` writes it
    /// `0755` so it can run at all. `--replace` writes its bytes through the
    /// same atomic writer everything else in this module uses, but that
    /// writer has no opinion about permissions: a hook replaced from a stale
    /// copy lands back at the writer's default mode, not executable, and the
    /// next hook call exits 126. Repairing a hook whose text already matches
    /// what we ship has to work too, since `on_disk == shipped` returns
    /// early today and never looks at the mode bit at all.
    #[cfg(unix)]
    #[test]
    fn replacing_a_shipped_hook_leaves_it_executable() {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _root_guard) = fixture("replace-hook-executable");
        let hook = repo.checkout.join(".spoolway/hooks/github.sh");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();

        let shipped = crate::assets::HOOK_SCRIPTS
            .iter()
            .find(|(name, _)| *name == "github.sh")
            .unwrap()
            .1;

        // A stale hook, saved with no execute bit at all — the shape a
        // checkout picks up from a plain `git clone` or a non-executable
        // editor save.
        std::fs::write(&hook, "#!/bin/sh\necho stale\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o644)).unwrap();

        replace(
            &repo,
            &SyncArgs {
                replace: vec![".spoolway/hooks/github.sh".to_string()],
                ..args()
            },
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&hook).unwrap(),
            shipped,
            "replace must write the shipped hook's text"
        );
        let mode = std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "a replaced hook must be executable, not left at the writer's default mode"
        );

        // Now the text already matches what we ship, but the execute bit is
        // missing again — the `on_disk == shipped` early return must still
        // repair the mode rather than reporting the file as already current
        // and leaving it non-executable.
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o644)).unwrap();

        replace(
            &repo,
            &SyncArgs {
                replace: vec![".spoolway/hooks/github.sh".to_string()],
                ..args()
            },
        )
        .unwrap();

        let mode = std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "a hook already at the shipped text must still have its execute bit repaired"
        );
    }

    /// A second `--replace` of the same file never overwrites the backup the
    /// first one made, and every backup keeps the hook's execute bit.
    #[cfg(unix)]
    #[test]
    fn a_second_replace_keeps_the_first_backup_and_every_backup_keeps_the_hooks_mode() {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _root_guard) = fixture("replace-keeps-every-bak");
        let hook = repo.checkout.join(".spoolway/hooks/github.sh");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        let replace_args = SyncArgs {
            replace: vec![".spoolway/hooks/github.sh".to_string()],
            ..args()
        };

        std::fs::write(&hook, "#!/bin/sh\necho first edit\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        replace(&repo, &replace_args).unwrap();

        std::fs::write(&hook, "#!/bin/sh\necho second edit\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        replace(&repo, &replace_args).unwrap();

        let mut backups = std::fs::read_dir(hook.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("github.sh.bak"))
            })
            .map(|p| {
                let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                (std::fs::read_to_string(&p).unwrap(), mode)
            })
            .collect::<Vec<_>>();
        backups.sort();
        assert_eq!(
            backups,
            vec![
                ("#!/bin/sh\necho first edit\n".to_string(), 0o755),
                ("#!/bin/sh\necho second edit\n".to_string(), 0o755),
            ]
        );
    }

    /// A dry run only ever says what it would do — the hook mode repair added
    /// alongside the content write must stay behind that same gate, or
    /// `--replace --dry-run` would quietly fix a hook's permissions while
    /// claiming to have changed nothing.
    #[cfg(unix)]
    #[test]
    fn a_dry_run_replace_repairs_neither_a_hooks_text_nor_its_mode() {
        use std::os::unix::fs::PermissionsExt;

        let (repo, _root_guard) = fixture("replace-hook-dry-run");
        let hook = repo.checkout.join(".spoolway/hooks/github.sh");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();

        let stale = "#!/bin/sh\necho stale\n";
        std::fs::write(&hook, stale).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o644)).unwrap();

        replace(
            &repo,
            &SyncArgs {
                replace: vec![".spoolway/hooks/github.sh".to_string()],
                dry_run: true,
            },
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&hook).unwrap(),
            stale,
            "a dry run must not write the shipped hook's text"
        );
        let mode = std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o644,
            "a dry run must not repair the execute bit either"
        );
    }

    /// A task skeleton is the project's outright, so a sync must not read it,
    /// rewrite it, or have an opinion about it — whatever is in it, and whether
    /// or not it looks anything like the one we ship.
    #[test]
    fn a_task_skeleton_is_never_touched_by_a_sync() {
        let (repo, _root_guard) = fixture("task-skeleton-untouched");
        let mine = repo.task_templates_dir().join("default.md");
        let theirs = repo.task_templates_dir().join("bugfix.md");
        std::fs::write(&mine, "## Mine\n\nkeep me\n").unwrap();
        std::fs::write(&theirs, "## What to build\n\nAnything.\n").unwrap();

        run(&repo, &args()).unwrap();
        let outcomes = scan(&repo, &args()).unwrap();

        assert_eq!(
            std::fs::read_to_string(&mine).unwrap(),
            "## Mine\n\nkeep me\n"
        );
        assert_eq!(
            std::fs::read_to_string(&theirs).unwrap(),
            "## What to build\n\nAnything.\n"
        );
        // And not even mentioned: a line about a file nothing was done to is a
        // line that reads as if something might be.
        assert!(
            !outcome_lines(&outcomes)
                .iter()
                .any(|line| line.contains("templates/tasks")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// `templates` has nothing to iterate now that the plan skeleton is
    /// gone — `crate::skeleton::skeletons()` ships empty — so a run over it
    /// reports nothing and fails nothing, whatever is on disk.
    #[test]
    fn templates_has_nothing_left_to_check_and_reports_nothing() {
        let (repo, _root_guard) = fixture("no-skeletons-left");
        let mut outcomes = Vec::new();
        templates(&repo, &args(), &mut outcomes).unwrap();
        assert!(outcomes.is_empty(), "{:?}", outcome_lines(&outcomes));
    }

    /// The fenced key reference as an older release wrote it, markers included:
    /// a real earlier copy, so sync recognises it as stale rather than as an
    /// unknown edit.
    fn older_fence() -> String {
        let file = crate::known_copies::older::pipeline_file();
        let start = file.find(crate::assets::PIPELINE_KEYS_BEGIN).unwrap();
        let end = file.find(crate::assets::PIPELINE_KEYS_END).unwrap();
        file[start..end + crate::assets::PIPELINE_KEYS_END.len()].to_string()
    }

    /// A pipeline file, written the way a project's is: a title of its own, the
    /// fenced key reference, and the flow underneath.
    fn pipeline_file(repo: &Repo, name: &str, block: &str) -> PathBuf {
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.yml"));
        std::fs::write(
            &path,
            format!(
                "# {name} — mine, and nothing here is spoolway's.\n\n{block}\n\nsteps:\n  \
                 - id: work\n    run: x\n    on_pass: done\n"
            ),
        )
        .unwrap();
        path
    }

    /// The sync a pipeline file could not otherwise get: a key reference an
    /// older release wrote is brought forward, and every line the project wrote
    /// around it survives.
    #[test]
    fn a_stale_key_reference_is_refreshed_and_the_rest_of_the_file_kept() {
        let (repo, _root_guard) = fixture("pipeline-stale");
        let path = pipeline_file(&repo, "hotfix", &older_fence());

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(after.contains(crate::pipeline::key_block()), "{after}");
        assert!(!after.contains(crate::known_copies::older::key_block()));
        assert!(
            after.contains("# hotfix — mine"),
            "the title is the project's"
        );
        assert!(after.contains("  - id: work"), "the flow is untouched");
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .any(|line| line.starts_with("wrote") && line.contains("hotfix.yml")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// The other half of the rule: a block that is no copy any release shipped
    /// is refused and left as it is, whether a newer spoolway wrote it or a
    /// person edited it, and the refusal names the way to take this binary's.
    #[test]
    fn an_edit_inside_the_markers_is_refused_and_left_alone() {
        let (repo, _root_guard) = fixture("pipeline-edited");
        let edited =
            crate::pipeline::key_block().replace("Absent: 30m.", "Absent: however long you like.");
        let path = pipeline_file(&repo, "default", &edited);
        let before = std::fs::read_to_string(&path).unwrap();

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let refused = refusals(&outcomes);
        assert_eq!(refused.len(), 1, "{:?}", outcome_lines(&outcomes));
        assert_eq!(refused[0].0, ".spoolway/pipelines/default.yml");
        assert!(
            refused[0]
                .1
                .contains("`spoolway sync --replace .spoolway/pipelines/default.yml`"),
            "{refused:?}"
        );
    }

    /// And the limit of all of it: a pipeline that never carried the markers is
    /// somebody's own file. Nothing is written into it, and nothing is said.
    #[test]
    fn a_pipeline_with_no_markers_is_left_alone_and_not_mentioned() {
        let (repo, _root_guard) = fixture("pipeline-unmarked");
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mine.yml");
        let mine = "# mine, whole.\nsteps:\n  - id: work\n    run: x\n    on_pass: done\n";
        std::fs::write(&path, mine).unwrap();

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), mine);
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .all(|line| !line.contains("mine.yml")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// `loop: 0`, which 0.7 ran as no limit, no longer loads. A sync refuses
    /// the file by name and step, says the edit that keeps its meaning, and
    /// writes nothing to it — not even a stale key reference — rather than
    /// report an upgrade done over a pipeline the next command refuses.
    #[test]
    fn a_pipeline_with_loop_zero_is_refused_naming_the_edit_and_left_as_it_is() {
        let (repo, _root_guard) = fixture("pipeline-loop-zero");
        let stale = format!(
            "{}\n# Top level\n#   template    What this used to say.\n{}",
            crate::assets::PIPELINE_KEYS_BEGIN,
            crate::assets::PIPELINE_KEYS_END
        );
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("default.yml");
        let before = format!(
            "{stale}\n\nsteps:\n  \
             - id: implement\n    agent: pi\n    on_pass: document\n  \
             - id: document\n    agent: pi\n    loop: 0\n    on_pass: done\n"
        );
        std::fs::write(&path, &before).unwrap();
        // An unfenced file of the project's own is read for it too.
        let mine = dir.join("mine.yml");
        std::fs::write(
            &mine,
            "steps:\n  - id: work\n    loop: 0\n    run: x\n    on_pass: done\n",
        )
        .unwrap();

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let refused = refusals(&outcomes);
        assert_eq!(refused.len(), 2, "{:?}", outcome_lines(&outcomes));
        let (shown, why) = refused[0];
        assert!(shown.ends_with("default.yml"), "{shown}");
        assert_eq!(
            why,
            "will not load: step `document` has loop: 0 — a loop is 1 or more; delete `loop:` \
             for no limit"
        );
        assert!(refused[1].0.ends_with("mine.yml"), "{refused:?}");
        let (wrote, _) = dedup_paths(&outcomes);
        assert!(wrote.is_empty(), "{wrote:?}");
    }

    /// A pipeline saved with a byte order mark is read for its retired
    /// shapes like one without; otherwise sync skips the check and reports
    /// the upgrade done over a file the loader then refuses.
    #[test]
    fn a_pipeline_with_a_byte_order_mark_and_loop_zero_is_still_refused() {
        let (repo, _root_guard) = fixture("pipeline-loop-zero-bom");
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mine.yml"),
            "\u{feff}steps:\n  - id: work\n    loop: 0\n    run: x\n    on_pass: done\n",
        )
        .unwrap();

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        let refused = refusals(&outcomes);
        assert_eq!(refused.len(), 1, "{:?}", outcome_lines(&outcomes));
        assert!(refused[0].1.starts_with("will not load"), "{refused:?}");
    }

    /// A pipeline file's `headless:` steps are renamed to `hide:`, fenced or
    /// not, and every other byte stays: a `headless` in a comment, a
    /// `description:` and a `run:` value, a step already on `hide:`, and the
    /// file's stale key reference being refreshed in the same write. Each
    /// file is named, and the note lists the steps. A file with nothing to
    /// rename is not written, and a second run reports nothing.
    #[test]
    fn headless_steps_are_renamed_to_hide_and_nothing_else_is_touched() {
        let (repo, _root_guard) = fixture("pipeline-headless-rename");
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let steps = |key: &str, keep: &str| {
            format!(
                "# headless: true was the old spelling.\nsteps:\n  \
                 - id: check\n    description: Runs headless: true, they said.\n    \
                 run: echo headless: true\n    {key}: true\n    on_pass: merge\n  \
                 - id: merge\n    run: |\n      echo run\n      headless: true\n    \
                 {key}: false # keep\n    on_pass: done\n  \
                 - id: other\n    run: x\n    {keep}: true\n    on_pass: done\n"
            )
        };
        let stale = format!("{}\n", older_fence());
        let fenced = dir.join("fenced.yml");
        std::fs::write(&fenced, format!("{stale}{}", steps("headless", "hide"))).unwrap();
        let unfenced = dir.join("unfenced.yml");
        std::fs::write(&unfenced, steps("headless", "hide")).unwrap();
        let clean = dir.join("clean.yml");
        std::fs::write(&clean, steps("hide", "hide")).unwrap();
        let clean_before = std::fs::read(&clean).unwrap();

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        let fresh = crate::pipeline::key_block();
        let fenced_now = std::fs::read_to_string(&fenced).unwrap();
        assert!(fenced_now.contains(fresh), "{fenced_now}");
        // `headless: false` loses its line, since `hide: false` would force a
        // pane where it only spelled out the default.
        let renamed = steps("hide", "hide").replace("    hide: false # keep\n", "");
        assert!(
            fenced_now.ends_with(&renamed),
            "only the key names change: {fenced_now}"
        );
        assert_eq!(std::fs::read_to_string(&unfenced).unwrap(), renamed);
        assert_eq!(std::fs::read(&clean).unwrap(), clean_before);

        let (wrote, _) = dedup_paths(&outcomes);
        assert_eq!(wrote.len(), 2, "{wrote:?}");
        assert!(wrote.iter().all(|path| !path.ends_with("clean.yml")));
        let lines = outcome_lines(&outcomes);
        assert!(
            lines.contains(
                &"migrated .spoolway/pipelines/unfenced.yml (migrated: unfenced: `headless:` \
                  renamed to `hide:` on step check; `headless: false` removed from step merge, \
                  where it only spelled out the default)"
                    .to_string()
            ),
            "{lines:?}"
        );

        // Nothing is left to rename the second time.
        let mut again = Vec::new();
        pipelines(&repo, &args(), &mut again).unwrap();
        let (wrote, _) = dedup_paths(&again);
        assert!(wrote.is_empty(), "{:?}", outcome_lines(&again));
        assert!(migration_notes(&again).is_empty());
    }

    /// A dry run reports the rename, as it reports every write, and writes
    /// nothing.
    #[test]
    fn a_dry_run_reports_the_headless_rename_and_writes_nothing() {
        let (repo, _root_guard) = fixture("pipeline-headless-dry-run");
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mine.yml");
        let before = "steps:\n  - id: a\n    run: x\n    headless: true\n    on_pass: done\n";
        std::fs::write(&path, before).unwrap();

        let mut outcomes = Vec::new();
        let dry = SyncArgs {
            dry_run: true,
            replace: Vec::new(),
        };
        pipelines(&repo, &dry, &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let (wrote, _) = dedup_paths(&outcomes);
        assert_eq!(wrote.len(), 1, "{:?}", outcome_lines(&outcomes));
        assert_eq!(migration_notes(&outcomes).len(), 1);
    }

    /// `headless:` and `hide:` on one step cannot be renamed without losing a
    /// value. The file is refused, naming the step, with every other step in
    /// it left as it was, and nothing is written.
    #[test]
    fn a_step_with_both_headless_and_hide_is_refused_and_the_file_left_alone() {
        let (repo, _root_guard) = fixture("pipeline-headless-and-hide");
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mine.yml");
        let before = "steps:\n  - id: a\n    run: x\n    headless: true\n    on_pass: b\n  \
                      - id: b\n    run: x\n    headless: false\n    hide: true\n    \
                      on_pass: done\n";
        std::fs::write(&path, before).unwrap();

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let refused = refusals(&outcomes);
        assert_eq!(refused.len(), 1, "{:?}", outcome_lines(&outcomes));
        assert!(
            refused[0].1.contains("step `b` declares both"),
            "{refused:?}"
        );
        assert!(refused[0].1.contains("keep only `hide:`"), "{refused:?}");
        let (wrote, _) = dedup_paths(&outcomes);
        assert!(wrote.is_empty(), "{wrote:?}");
    }

    /// The renamer reads a patch as a map of ids, a list item whose `id:` is
    /// not its first key, a Windows line ending and a byte order mark, and in
    /// each renames only the step key.
    #[test]
    fn rename_headless_reads_patches_late_ids_and_line_endings() {
        let patch = "# headless: true\nsteps:\n  check:\n    headless: true\n    \
                     timeout: 5m\n  merge:\n    run: echo headless: true\n    hide: false\n";
        let Renamed::Done { text, ids, .. } = rename_headless(patch, Layout::Patch) else {
            panic!("the patch names `headless:` on `check`");
        };
        assert_eq!(ids, ["check"]);
        assert_eq!(
            text,
            patch.replacen("    headless: true\n", "    hide: true\n", 1)
        );

        let late = "\u{feff}steps:\r\n- run: x\r\n  headless: true\r\n  id: z\r\n  \
                    on_pass: done\r\n";
        let Renamed::Done { text, ids, .. } = rename_headless(late, Layout::Steps) else {
            panic!("the list item names `headless:`");
        };
        assert_eq!(ids, ["z"]);
        assert_eq!(text, late.replace("headless", "hide"));

        let none = "steps:\n  - id: a\n    run: x\n    on_pass: done\n";
        assert!(matches!(
            rename_headless(none, Layout::Steps),
            Renamed::Nothing
        ));
    }

    /// `headless: false` only spelled out the default in 0.13.0, and
    /// `hide: false` forces a pane, so the key is removed and never renamed.
    /// `headless: true` is renamed as before, and the other lines stay.
    #[test]
    fn headless_false_is_removed_and_headless_true_is_renamed() {
        let before = "steps:\n  - id: a\n    run: x\n    headless: false # default\n    \
                      on_pass: b\n  - id: b\n    run: x\n    headless: true\n    \
                      on_pass: done\n";
        let Renamed::Done { text, ids, dropped } = rename_headless(before, Layout::Steps) else {
            panic!("both steps name `headless:`");
        };
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(dropped, ["a"]);
        assert_eq!(
            text,
            "steps:\n  - id: a\n    run: x\n    on_pass: b\n  - id: b\n    run: x\n    \
             hide: true\n    on_pass: done\n"
        );

        let patch = "steps:\n  check:\n    headless: false\n    timeout: 5m\n";
        let Renamed::Done { text, dropped, .. } = rename_headless(patch, Layout::Patch) else {
            panic!("the patch names `headless:` on `check`");
        };
        assert_eq!(dropped, ["check"]);
        assert_eq!(text, "steps:\n  check:\n    timeout: 5m\n");

        // Every spelling the loader reads as false is dropped, so none of
        // them is renamed into a `hide: False` that forces a pane.
        let spelt = "steps:\n  - id: a\n    headless: False\n  - id: b\n    headless: FALSE\n";
        let Renamed::Done { text, dropped, .. } = rename_headless(spelt, Layout::Steps) else {
            panic!("both steps name `headless:`");
        };
        assert_eq!(dropped, ["a", "b"]);
        assert_eq!(text, "steps:\n  - id: a\n  - id: b\n");

        // On the line that opens a list item the dash moves onto the next key.
        let opens = "steps:\n  - headless: false\n    id: a\n    run: x\n";
        let Renamed::Done { text, dropped, .. } = rename_headless(opens, Layout::Steps) else {
            panic!("the item opens with `headless:`");
        };
        assert_eq!(dropped, ["a"]);
        assert_eq!(text, "steps:\n  - id: a\n    run: x\n");

        // A step with no other key has nothing to take the dash, so the key
        // is left for `sync` to refuse by name.
        let alone = "steps:\n  - headless: false\n  - id: b\n";
        assert!(matches!(
            rename_headless(alone, Layout::Steps),
            Renamed::Nothing
        ));
    }

    /// A file whose only `headless:` is `false` is written without the key,
    /// and the note says the key was removed, not renamed.
    #[test]
    fn sync_drops_headless_false_from_a_pipeline_file() {
        let (repo, _root_guard) = fixture("pipeline-headless-false");
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mine.yml");
        std::fs::write(
            &path,
            "steps:\n  - id: a\n    run: x\n    headless: false\n    on_pass: done\n",
        )
        .unwrap();

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "steps:\n  - id: a\n    run: x\n    on_pass: done\n");
        let notes = migration_notes(&outcomes);
        assert_eq!(notes.len(), 1, "{:?}", outcome_lines(&outcomes));
        assert!(
            format!("{notes:?}").contains("removed from step a"),
            "{notes:?}"
        );
        // Nothing was renamed, so the `wrote` line must not say so.
        let lines = outcome_lines(&outcomes);
        assert!(
            lines
                .iter()
                .any(|line| line.ends_with("(`headless: false` removed)")),
            "{lines:?}"
        );
    }

    /// A patch's `headless: false` could have overridden the tracked step's
    /// `headless: true`, so its note says how to keep the pane, never that the
    /// line only spelled out the default.
    #[test]
    fn a_patch_note_says_how_to_keep_the_pane() {
        let ids = ["lint".to_string()];
        let note = |layout| {
            let outcomes = [rename_note("p.yml", "default", &ids, &ids, layout)];
            format!("{:?}", migration_notes(&outcomes))
        };
        let tracked = note(Layout::Steps);
        assert!(
            tracked.contains("only spelled out the default"),
            "{tracked}"
        );
        let patch = note(Layout::Patch);
        assert!(!patch.contains("only spelled out the default"), "{patch}");
        assert!(
            patch.contains("set `hide: false` here to keep a pane"),
            "{patch}"
        );
    }

    /// `end: true` and `on_pass: blocked` no longer load. A sync names each
    /// file and step that still uses one, with the line that replaces it, and
    /// writes nothing to either file.
    #[test]
    fn a_pipeline_with_end_or_a_pass_to_blocked_is_refused_naming_each_step() {
        let (repo, _root_guard) = fixture("pipeline-end-and-blocked");
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let ends = dir.join("ends.yml");
        std::fs::write(
            &ends,
            "steps:\n  - id: a\n    run: x\n    on_pass: halt\n  - id: halt\n    end: true\n",
        )
        .unwrap();
        let parks = dir.join("parks.yml");
        std::fs::write(
            &parks,
            "steps:\n  - id: a\n    agent: pi\n    model: m\n    on_pass: blocked\n",
        )
        .unwrap();
        let before = [
            std::fs::read_to_string(&ends).unwrap(),
            std::fs::read_to_string(&parks).unwrap(),
        ];

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&ends).unwrap(), before[0]);
        assert_eq!(std::fs::read_to_string(&parks).unwrap(), before[1]);
        let refused = refusals(&outcomes);
        assert_eq!(refused.len(), 2, "{:?}", outcome_lines(&outcomes));
        let said = |file: &str| {
            refused
                .iter()
                .find(|(shown, _)| shown.ends_with(file))
                .map(|(_, why)| *why)
                .unwrap_or_default()
        };
        assert_eq!(
            said("ends.yml"),
            "will not load: step `halt` sets end: true, which is gone — use on_pass: done"
        );
        assert_eq!(
            said("parks.yml"),
            "will not load: step `a` has on_pass: blocked — use gate: true to stop for a person"
        );
    }

    /// A background command step with `on_fail`, which 0.10.0 routed on a
    /// late failure, no longer loads. A sync refuses the file with both ways
    /// out and writes nothing to it, rather than report an upgrade done over
    /// a pipeline the next command refuses.
    #[test]
    fn a_background_step_with_on_fail_is_refused_naming_both_fixes() {
        let (repo, _root_guard) = fixture("pipeline-background-on-fail");
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("default.yml");
        let before = "steps:\n  \
                      - id: implement\n    agent: pi\n    model: m\n    on_pass: lint\n  \
                      - id: lint\n    run: x\n    background: true\n    on_pass: done\n    \
                      on_fail: implement\n";
        std::fs::write(&path, before).unwrap();

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let refused = refusals(&outcomes);
        assert_eq!(refused.len(), 1, "{:?}", outcome_lines(&outcomes));
        let (shown, why) = refused[0];
        assert!(shown.ends_with("default.yml"), "{shown}");
        assert!(
            why.starts_with(
                "will not load: step `lint` runs in the background but declares `on_fail`"
            ),
            "{why}"
        );
        assert!(
            why.contains("keep it a sidecar:  delete `on_fail:`"),
            "{why}"
        );
        assert!(
            why.contains("make it a check:    delete `background:`"),
            "{why}"
        );
        let (wrote, _) = dedup_paths(&outcomes);
        assert!(wrote.is_empty(), "{wrote:?}");
    }

    /// Half a fence is the one shape worth a refusal: the lines under a start
    /// marker with no end could be anyone's.
    #[test]
    fn a_start_marker_with_no_end_is_reported_rather_than_guessed_at() {
        let (repo, _root_guard) = fixture("pipeline-unterminated");
        let half = format!("{}\n# Top level\n", crate::assets::PIPELINE_KEYS_BEGIN);
        let path = pipeline_file(&repo, "half", &half);
        let before = std::fs::read_to_string(&path).unwrap();

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .any(|line| line.starts_with("blocked") && line.contains("half.yml")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// A refused file is listed with its reason once, however many outcomes
    /// name it, and a file that only changed is not a refusal.
    #[test]
    fn refusals_lists_each_refused_file_once_with_its_reason() {
        let outcomes = vec![
            Outcome::wrote("a.yml", "key reference refreshed"),
            Outcome::blocked("b.yml", "never ends"),
            Outcome::blocked("b.yml", "never ends"),
            Outcome::Kept,
        ];
        assert_eq!(refusals(&outcomes), vec![("b.yml", "never ends")]);
    }

    /// The confirm panel names a refused file and its reason before any write,
    /// so declining it still shows what was refused.
    #[test]
    fn the_confirm_panel_lists_a_refused_file_first() {
        let body = panel_body(
            &[("p.yml", "never ends")],
            &["config.toml"],
            &BTreeMap::new(),
            &[],
        );
        assert!(body[1].starts_with("refused"), "{body:?}");
        assert!(body[2].contains("never ends"), "{body:?}");
        assert!(body[3].starts_with("write"), "{body:?}");
    }

    /// A long refusal reason keeps its ending in the panel, where the fix it
    /// names is.
    #[test]
    fn a_long_refusal_reason_wraps_in_the_panel_instead_of_being_cut() {
        let why = format!(
            "{}restore the marker, or delete the block",
            "word ".repeat(20)
        );
        let body = panel_body(&[("p.yml", why.as_str())], &[], &BTreeMap::new(), &[]);
        assert!(
            body.iter().any(|line| line.ends_with("the block)")),
            "{body:?}"
        );
        assert!(body.iter().all(|line| line.chars().count() <= MAX_LINE));
    }

    /// One token longer than the panel is broken across rows, so a path in a
    /// refusal reason cannot widen the panel past [`MAX_LINE`].
    #[test]
    fn a_refusal_reason_with_one_very_long_word_stays_inside_the_panel() {
        let why = format!("could not be read ({})", "x".repeat(150));
        let body = panel_body(&[("p.yml", why.as_str())], &[], &BTreeMap::new(), &[]);
        assert!(
            body.iter().all(|line| line.chars().count() <= MAX_LINE),
            "{body:?}"
        );
    }

    /// Refreshing an older key block takes nothing a person wrote, so it says
    /// nothing beyond the file line: no note claims an edit was lost.
    #[test]
    fn a_refreshed_key_block_carries_no_note_about_lost_edits() {
        let (repo, _root_guard) = fixture("pipeline-replaced-note");
        pipeline_file(&repo, "p", &older_fence());

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        assert!(migration_notes(&outcomes).is_empty());
        assert_eq!(dedup_paths(&outcomes).0, [".spoolway/pipelines/p.yml"]);
    }

    /// A config that never named `key_in_names` gains it at the default, and
    /// the report says so on its own line.
    #[test]
    fn a_default_value_sync_sets_is_named_in_the_report() {
        let (repo, _root_guard) = fixture("set-default-note");
        std::fs::write(
            crate::config::Config::path_in(&repo.checkout),
            "[issue_tracking]\nhook = \"\"\n",
        )
        .unwrap();

        let outcomes = scan(&repo, &args()).unwrap();

        let notes = migration_notes(&outcomes);
        let all: Vec<&str> = notes.values().flatten().map(|(r, _)| *r).collect();
        assert!(
            all.iter()
                .any(|r| r.contains("set issue_tracking.key_in_names = true")),
            "{all:?}"
        );
    }

    /// A dry run reads and says, on this file as on every other.
    #[test]
    fn a_dry_run_leaves_a_stale_key_reference_where_it_is() {
        let (repo, _root_guard) = fixture("pipeline-dry");
        let path = pipeline_file(&repo, "hotfix", &older_fence());
        let before = std::fs::read_to_string(&path).unwrap();

        let mut outcomes = Vec::new();
        pipelines(
            &repo,
            &SyncArgs {
                dry_run: true,
                ..args()
            },
            &mut outcomes,
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .any(|line| line.starts_with("wrote")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// A user-level skill no release shipped is refused per folder file and
    /// left alone, spelled with `~` so the remedy can be typed back.
    #[test]
    fn a_user_level_skill_no_release_shipped_is_refused_with_a_typed_back_path() {
        let (repo, _root_guard) = fixture("skills-user-unknown");
        let home = crate::scratch::root("sync-user-unknown-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        crate::platform::test_home::with_home(&home, || {
            crate::install::install_user(crate::cli::Provider::Claude, false).unwrap();
        });
        let edited = crate::cli::Provider::Claude
            .user_skills_dir(&home)
            .join("spoolway-plan")
            .join("SKILL.md");
        std::fs::write(&edited, "edited by hand\n").unwrap();

        let mut outcomes = Vec::new();
        crate::platform::test_home::with_home(&home, || {
            user_level(&mut outcomes, &mut Vec::new()).unwrap();
        });

        assert_eq!(
            std::fs::read_to_string(&edited).unwrap(),
            "edited by hand\n"
        );
        let refused = refusals(&outcomes);
        assert_eq!(refused.len(), 1, "{:?}", outcome_lines(&outcomes));
        assert_eq!(refused[0].0, "~/.claude/skills/spoolway-plan/SKILL.md");
        assert!(
            refused[0]
                .1
                .contains("`spoolway sync --replace ~/.claude/skills/spoolway-plan/SKILL.md`"),
            "{refused:?}"
        );
        let _ = (repo, std::fs::remove_dir_all(&home));
    }

    /// `--replace` takes this binary's copy over a skill `sync` refused: a
    /// project's by its relative path, a user-level one by `~` or absolute,
    /// each with the old file saved as a `.bak`.
    #[test]
    fn replace_takes_this_binarys_skill_over_a_refused_one() {
        let (repo, _root_guard) = fixture("replace-skills");
        let home = crate::scratch::root("sync-replace-skills-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let project = crate::cli::Provider::Claude.plan(&repo.checkout).remove(0);
        let user = crate::cli::Provider::Claude.plan_user(&home).remove(0);
        for file in [&project, &user] {
            std::fs::create_dir_all(file.path.parent().unwrap()).unwrap();
            std::fs::write(&file.path, "edited by hand\n").unwrap();
        }

        crate::platform::test_home::with_home(&home, || {
            let named = crate::platform::home_dir().unwrap();
            let tilde = format!("~/{}", user.path.strip_prefix(&named).unwrap().display());
            replace(
                &repo,
                &SyncArgs {
                    replace: vec![".claude/skills/spoolway-plan/SKILL.md".to_string(), tilde],
                    ..args()
                },
            )
            .unwrap();
        });

        for file in [&project, &user] {
            assert_eq!(std::fs::read_to_string(&file.path).unwrap(), file.contents);
            let bak = file.path.with_extension("md.bak");
            assert_eq!(std::fs::read_to_string(bak).unwrap(), "edited by hand\n");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// For a pipeline file `--replace` swaps the key block and nothing else,
    /// whatever the block held, and keeps the old file as a `.bak`.
    #[test]
    fn replace_swaps_only_the_key_block_of_a_pipeline_file() {
        let (repo, _root_guard) = fixture("replace-pipeline");
        let edited = crate::pipeline::key_block().replace("Absent: 30m.", "Absent: forever.");
        let path = pipeline_file(&repo, "default", &edited);
        let before = std::fs::read_to_string(&path).unwrap();

        replace(
            &repo,
            &SyncArgs {
                replace: vec![".spoolway/pipelines/default.yml".to_string()],
                ..args()
            },
        )
        .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, before.replace("Absent: forever.", "Absent: 30m."));
        assert_eq!(
            std::fs::read_to_string(path.with_extension("yml.bak")).unwrap(),
            before
        );
    }

    /// A pipeline file with no key block has no place for one to go. The
    /// reason says how to opt in, and the file is not touched.
    #[test]
    fn replace_names_the_markers_a_pipeline_without_a_key_block_needs() {
        let (repo, _root_guard) = fixture("replace-pipeline-bare");
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mine.yml");
        std::fs::write(&path, "steps:\n  - id: a\n    run: x\n    on_pass: done\n").unwrap();

        let why = replacement_for(&repo, &path, &std::fs::read_to_string(&path).unwrap())
            .err()
            .expect("nothing to swap");

        assert!(why.contains(crate::assets::PIPELINE_KEYS_BEGIN), "{why}");
        assert!(why.contains(crate::assets::PIPELINE_KEYS_END), "{why}");
    }

    /// A plain `sync` refuses a hand-edited key block before it renames
    /// anything, and names `--replace` as the way on. That command makes the
    /// rename too, so the file loads after it, and a plain `sync` then has
    /// nothing left to do.
    #[test]
    fn replace_renames_headless_in_a_pipeline_whose_key_block_was_edited() {
        let (repo, _root_guard) = fixture("replace-pipeline-headless");
        let edited = crate::pipeline::key_block().replace("Absent: 30m.", "Absent: forever.");
        let path = pipeline_file(&repo, "default", &edited);
        let before = std::fs::read_to_string(&path)
            .unwrap()
            .replace("    run: x\n", "    run: x\n    headless: true\n");
        std::fs::write(&path, &before).unwrap();

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let refused = refusals(&outcomes);
        assert_eq!(refused.len(), 1, "{:?}", outcome_lines(&outcomes));
        assert!(refused[0].1.contains("--replace"), "{refused:?}");

        let (shipped, scope) = replacement_for(&repo, &path, &before).unwrap();
        assert!(matches!(scope, Scope::KeyBlockRenamed));
        assert_eq!(
            shipped,
            before
                .replace("Absent: forever.", "Absent: 30m.")
                .replace("    headless: true\n", "    hide: true\n")
        );
        replace(
            &repo,
            &SyncArgs {
                replace: vec![".spoolway/pipelines/default.yml".to_string()],
                ..args()
            },
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), shipped);

        let mut again = Vec::new();
        pipelines(&repo, &args(), &mut again).unwrap();
        assert!(refusals(&again).is_empty(), "{:?}", outcome_lines(&again));
        assert!(
            dedup_paths(&again).0.is_empty(),
            "{:?}",
            outcome_lines(&again)
        );
    }

    /// `--replace` deletes a `headless: false` and says so, rather than
    /// reporting a rename that did not happen.
    #[test]
    fn replace_removes_headless_false_and_says_it_removed_it() {
        let (repo, _root_guard) = fixture("replace-pipeline-headless-false");
        let edited = crate::pipeline::key_block().replace("Absent: 30m.", "Absent: forever.");
        let path = pipeline_file(&repo, "default", &edited);
        let before = std::fs::read_to_string(&path)
            .unwrap()
            .replace("    run: x\n", "    run: x\n    headless: false\n");
        std::fs::write(&path, &before).unwrap();

        let (shipped, scope) = replacement_for(&repo, &path, &before).unwrap();
        assert!(matches!(scope, Scope::KeyBlockRenamedDropped));
        assert!(!shipped.contains("headless: false"));
        assert!(!shipped.contains("hide: false"));
        assert!(scope.wrote().contains("removed"), "{}", scope.wrote());
    }

    /// `--replace` on a pipeline with `headless:` and `hide:` on one step
    /// refuses as a plain `sync` does, naming the step, and writes nothing.
    #[test]
    fn replace_refuses_a_step_with_both_headless_and_hide() {
        let (repo, _root_guard) = fixture("replace-pipeline-both-keys");
        let path = pipeline_file(&repo, "default", crate::pipeline::key_block());
        let before = std::fs::read_to_string(&path).unwrap().replace(
            "    run: x\n",
            "    run: x\n    headless: true\n    hide: true\n",
        );

        let why = replacement_for(&repo, &path, &before)
            .err()
            .expect("refused");
        assert!(why.contains("step `work` declares both"), "{why}");
    }

    /// A `headless:` on a flow-style step is one the renamer cannot reach.
    /// The file is still refused, and the reason says to write `hide:`, not
    /// to run the `sync` that just failed to rename it.
    #[test]
    fn a_flow_style_headless_step_is_refused_without_naming_sync() {
        let (repo, _root_guard) = fixture("pipeline-headless-flow");
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mine.yml");
        let before = "steps:\n  - {id: a, run: x, headless: true, on_pass: done}\n";
        std::fs::write(&path, before).unwrap();

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let refused = refusals(&outcomes);
        assert_eq!(refused.len(), 1, "{:?}", outcome_lines(&outcomes));
        assert!(
            refused[0].1.contains("step `a` declares `headless:`"),
            "{refused:?}"
        );
        assert!(refused[0].1.contains("write `hide:`"), "{refused:?}");
        assert!(!refused[0].1.contains("spoolway sync"), "{refused:?}");
    }

    /// A patch with `headless:` and `hide:` on one step is refused, naming
    /// the step, and is left byte-identical, while a patch beside it that
    /// names only `headless:` is renamed.
    #[test]
    fn a_patch_with_both_headless_and_hide_is_refused_and_left_alone() {
        let (repo, _root_guard) = fixture("patch-headless-and-hide");
        crate::scratch::stamped(&repo.root);
        let home = crate::scratch::root("sync-patch-headless-and-hide-home");
        let _ = std::fs::remove_dir_all(&home);

        crate::platform::test_home::with_home(&home, || {
            let overrides = crate::overrides::dir_for(&repo.root).unwrap();
            let both = "steps:\n  check:\n    headless: true\n    hide: false\n";
            let only = "steps:\n  merge:\n    headless: true\n";
            let both_path = crate::overrides::pipeline_patch_path(&overrides, "both");
            let only_path = crate::overrides::pipeline_patch_path(&overrides, "only");
            std::fs::create_dir_all(both_path.parent().unwrap()).unwrap();
            std::fs::write(&both_path, both).unwrap();
            std::fs::write(&only_path, only).unwrap();

            let mut outcomes = Vec::new();
            override_pipelines(&repo, &args(), &mut outcomes).unwrap();

            assert_eq!(std::fs::read_to_string(&both_path).unwrap(), both);
            assert_eq!(
                std::fs::read_to_string(&only_path).unwrap(),
                only.replace("headless", "hide")
            );
            let refused = refusals(&outcomes);
            assert_eq!(refused.len(), 1, "{:?}", outcome_lines(&outcomes));
            assert!(refused[0].0.ends_with("both.yml"), "{refused:?}");
            assert!(
                refused[0].1.contains("step `check` declares both"),
                "{refused:?}"
            );
        });

        std::fs::remove_dir_all(&home).ok();
    }

    /// A patch step written in flow style cannot be renamed. The patch is
    /// still skipped at load, so `sync` refuses it and says to write `hide:`
    /// by hand, not to run the `sync` that just failed to rename it.
    #[test]
    fn a_flow_style_headless_patch_is_refused_without_naming_sync() {
        let (repo, _root_guard) = fixture("patch-headless-flow");
        crate::scratch::stamped(&repo.root);
        let home = crate::scratch::root("sync-patch-headless-flow-home");
        let _ = std::fs::remove_dir_all(&home);

        crate::platform::test_home::with_home(&home, || {
            let overrides = crate::overrides::dir_for(&repo.root).unwrap();
            let path = crate::overrides::pipeline_patch_path(&overrides, "mine");
            let before = "steps:\n  review: {headless: true}\n";
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, before).unwrap();

            let mut outcomes = Vec::new();
            override_pipelines(&repo, &args(), &mut outcomes).unwrap();

            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
            let refused = refusals(&outcomes);
            assert_eq!(refused.len(), 1, "{:?}", outcome_lines(&outcomes));
            assert!(
                refused[0].1.contains("step `review` declares `headless:`"),
                "{refused:?}"
            );
            assert!(refused[0].1.contains("write `hide:`"), "{refused:?}");
            assert!(!refused[0].1.contains("spoolway sync"), "{refused:?}");
        });

        std::fs::remove_dir_all(&home).ok();
    }

    /// A private pipeline under `local/pipelines/` fails the whole load over
    /// `headless:`, so `sync` renames it there as it does in the checkout. A
    /// private pipeline whose key it cannot reach, here a quoted one, is
    /// refused and left alone.
    #[test]
    fn a_private_pipeline_has_headless_renamed_and_an_unreadable_one_refused() {
        let (repo, _root_guard) = fixture("private-headless");
        crate::scratch::stamped(&repo.root);
        let home = crate::scratch::root("sync-private-headless-home");
        let _ = std::fs::remove_dir_all(&home);

        crate::platform::test_home::with_home(&home, || {
            let dir = crate::local::pipelines_dir(&crate::local::dir_for(&repo.root).unwrap());
            std::fs::create_dir_all(&dir).unwrap();
            let plain = dir.join("plain.yml");
            let quoted = dir.join("quoted.yml");
            let before = "steps:\n  - id: a\n    run: x\n    headless: true\n    on_pass: done\n";
            let stuck =
                "steps:\n  - id: a\n    run: x\n    \"headless\": true\n    on_pass: done\n";
            std::fs::write(&plain, before).unwrap();
            std::fs::write(&quoted, stuck).unwrap();

            let mut outcomes = Vec::new();
            private_pipelines(&repo, &args(), &mut outcomes).unwrap();

            assert_eq!(
                std::fs::read_to_string(&plain).unwrap(),
                before.replace("headless", "hide")
            );
            assert_eq!(std::fs::read_to_string(&quoted).unwrap(), stuck);
            let refused = refusals(&outcomes);
            assert_eq!(refused.len(), 1, "{:?}", outcome_lines(&outcomes));
            assert!(refused[0].0.ends_with("quoted.yml"), "{refused:?}");
            assert!(refused[0].1.contains("step `a` declares"), "{refused:?}");
            assert!(refused[0].1.contains("quoted key"), "{refused:?}");
            let notes = migration_notes(&outcomes);
            assert_eq!(notes.len(), 1, "{notes:?}");

            // The second run finds nothing to rename.
            let mut again = Vec::new();
            private_pipelines(&repo, &args(), &mut again).unwrap();
            let (wrote, _) = dedup_paths(&again);
            assert!(wrote.is_empty(), "{:?}", outcome_lines(&again));
        });

        std::fs::remove_dir_all(&home).ok();
    }

    /// A blank `prompt_cache_ttl` on a row that holds another setting is kept
    /// by the rewrite `sync` does, rather than dropped with the other unset
    /// keys, because only a row holding nothing else is the old reminder.
    #[test]
    fn a_rewritten_config_keeps_a_blank_prompt_cache_ttl_on_a_row_with_a_price() {
        let text = "[models.\"keep\"]\ninput = 1.0\nprompt_cache_ttl = \"\"\n";
        let current: crate::config::Config = toml::from_str(text).unwrap();
        let (rewritten, _) = rewritten_config(&current, text).unwrap().unwrap();
        assert!(
            rewritten.contains("input = 1.0\nprompt_cache_ttl = \"\"\n"),
            "{rewritten}"
        );
    }

    /// A `claude` or `codex` profile with no row for its glob gets the
    /// commented example, a row the file already has is left with its value
    /// and brings no example, and rewriting the result again changes nothing.
    #[test]
    fn a_rewritten_config_adds_the_cache_lifetime_comment_and_keeps_an_existing_row() {
        let mut current = crate::config::Config::default();
        current.agents.retain(|_, profile| profile.kind == "claude");
        let text = "[dispatch]\nlane_quiet = \"15m\"\n";

        let (added, _) = rewritten_config(&current, text).unwrap().unwrap();
        assert_eq!(
            added.matches("# [models.\"claude-*\"]\n").count(),
            1,
            "{added}"
        );
        assert!(!added.contains("gpt-*"), "{added}");
        assert!(!added.contains("\nprompt_cache_ttl"), "{added}");

        let reloaded: crate::config::Config = toml::from_str(&added).unwrap();
        let (again, _) = rewritten_config(&reloaded, &added).unwrap().unwrap();
        assert_eq!(again, added);

        let chosen = format!("{text}\n[models.\"claude-*\"]\nprompt_cache_ttl = \"1h\"\n");
        let reloaded: crate::config::Config = toml::from_str(&chosen).unwrap();
        let (kept, _) = rewritten_config(&reloaded, &chosen).unwrap().unwrap();
        assert!(!kept.contains("# [models.\"claude-*\"]"), "{kept}");
        assert!(kept.contains("[models.\"claude-*\"]\nprompt_cache_ttl = \"1h\"\n"));
    }

    /// For `config.toml` `--replace` rewrites the file as `sync` does, so a
    /// table `sync` refused is replaced, every value is kept, and the old
    /// file is saved as a `.bak`.
    #[test]
    fn replace_rewrites_a_refused_config_keeping_its_values() {
        let (repo, _root_guard) = fixture("replace-config");
        let path = crate::config::Config::path_in(&repo.root);
        let before = "# a table only a newer spoolway writes\n\n[dispatch]\nlane_quiet = \"45m\"\n";
        std::fs::write(&path, before).unwrap();

        replace(
            &repo,
            &SyncArgs {
                replace: vec![".spoolway/config.toml".to_string()],
                ..args()
            },
        )
        .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.starts_with(&crate::confkv::reference_table()),
            "{after}"
        );
        assert!(after.contains("lane_quiet = \"45m\""), "{after}");
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.bak")).unwrap(),
            before
        );
        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        assert!(
            refusals(&outcomes).is_empty(),
            "a sync accepts what --replace wrote"
        );
    }

    /// `reference_in` reads the table `reference_table` writes and nothing
    /// that merely starts with a comment.
    #[test]
    fn the_reference_table_is_the_comment_block_above_the_first_blank_line() {
        let table = crate::confkv::reference_table();
        let file = format!("{table}[dispatch]\nbackend = \"herdr\"\n");
        assert_eq!(reference_in(&file).trim_end(), table.trim_end());
        assert_eq!(reference_in("[dispatch]\n"), "");
        assert_eq!(reference_in("# on a key\nfoo = 1\n"), "");
        assert_eq!(reference_in(""), "");
    }

    /// `--replace` is the way back from a file with no region left to find. It
    /// has to reach every kind of file spoolway writes, or it is not that.
    #[test]
    fn replace_knows_every_kind_of_file_spoolway_writes() {
        let (repo, _root_guard) = fixture("replace");
        let prompts = repo.prompts_dir();
        for (path, expected) in [
            (prompts.join("reviewer").join("PROMPT.md"), "## "),
            // A prompt's belongings are as replaceable as its prose. Reaching
            // one and not the other would be the surprising half-answer.
            (
                prompts.join("archivist").join("assets").join("document.md"),
                "covers:",
            ),
            // The flat file a project had before prompts gained a directory.
            // `--replace` still reaches it, because reading still finds it.
            (prompts.join("reviewer.md"), "## "),
            (repo.task_templates_dir().join("default.md"), "## Intend"),
            (
                repo.checkout.join(".spoolway/hooks/github.sh"),
                "hand_off_for_review",
            ),
        ] {
            let shipped = shipped_for(&repo, &path)
                .unwrap_or_else(|| panic!("nothing shipped for {}", path.display()));
            assert!(shipped.contains(expected), "{}", path.display());
        }

        // And nothing else: replacing a file spoolway does not write would be
        // this command inventing content for somebody's own work. The project's
        // `.gitignore` is exactly that file — spoolway only ever removes its
        // own old block from it, and `--replace` writes whole files.
        assert!(shipped_for(&repo, &repo.root.join("src/main.rs")).is_none());
        assert!(shipped_for(&repo, &crate::gitignore::file(&repo.root)).is_none());
    }

    /// The flat `.spoolway/prompts/<name>.md` is not replaceable once the
    /// directory-shaped `<name>/PROMPT.md` exists — `prompt::path_for` reads the
    /// directory, so a flat write would land in a file nothing runs (finding
    /// 67).
    #[test]
    fn a_flat_prompt_shadowed_by_its_directory_is_not_shipped_for() {
        let (repo, _root_guard) = fixture("prompt-shadowed");
        let prompts = repo.prompts_dir();
        let flat = prompts.join("reviewer.md");

        // While only the flat file could exist, it is replaceable.
        assert!(shipped_for(&repo, &flat).is_some());

        // Once the directory shape is on disk, the flat path is dead.
        let nested = crate::prompt::directory_form(&repo, "reviewer");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "# reviewer\n").unwrap();
        assert!(shipped_for(&repo, &flat).is_none());
        assert!(shipped_for(&repo, &nested).is_some());
    }

    /// `sync` keeps a user-level install current too: a stale file there is
    /// rewritten and a missing one added, while a provider nobody installed
    /// at user level is left without a folder.
    #[test]
    fn skills_refreshes_a_user_level_install() {
        let (repo, _root_guard) = fixture("skills-user-level");
        let home = crate::scratch::root("sync-user-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let planned = crate::cli::Provider::Claude.plan_user(&home);
        let (stale, missing) = (&planned[0], &planned[planned.len() - 1]);
        // A real `install --user`, which is what actually leaves the
        // shipped files `user_skills` looks for — not a hand-written
        // stand-in for them.
        crate::platform::test_home::with_home(&home, || {
            crate::install::install_user(crate::cli::Provider::Claude, false).unwrap();
        });
        std::fs::remove_file(&missing.path).unwrap();
        std::fs::write(
            &stale.path,
            crate::known_copies::older::skill(crate::cli::Provider::Claude, "spoolway-plan"),
        )
        .unwrap();

        let mut outcomes = Vec::new();
        crate::platform::test_home::with_home(&home, || {
            skills(&repo, &args(), false, &mut outcomes).unwrap();
        });

        assert_eq!(
            std::fs::read_to_string(&stale.path).unwrap(),
            stale.contents
        );
        assert_eq!(
            std::fs::read_to_string(&missing.path).unwrap(),
            missing.contents
        );
        let lines = outcome_lines(&outcomes);
        assert!(
            lines.iter().any(|line| line.contains("~/.claude/skills/")),
            "user-level paths are named with ~: {lines:?}"
        );
        assert!(!crate::cli::Provider::Pi.user_skills_dir(&home).exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A person's own folder at user level, never written by `spoolway
    /// install --user`, happens to share a name with a skill spoolway has
    /// since retired. `sync` must leave it alone and must not install
    /// spoolway's skills there either — nobody asked for that. Before the
    /// fix, `installed_at` counts the retired-named folder as proof the
    /// provider is installed at user level, so `retired_skills` deletes the
    /// folder (and everything a person put inside it) and `skills` then
    /// writes spoolway's whole current set into that same directory.
    #[test]
    fn sync_leaves_a_hand_made_user_level_folder_with_a_retired_name_alone() {
        let (repo, _root_guard) = fixture("skills-user-level");
        let home = crate::scratch::root("sync-user-home-owned-only");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        // Nobody ever ran `spoolway install --user` here: there is no
        // planned file on disk at all. The only thing in this user-level
        // folder is a directory a person wrote by hand that happens to
        // share a name with a skill spoolway has since retired.
        let claude_dir = crate::cli::Provider::Claude.user_skills_dir(&home);
        let owned = claude_dir.join(crate::install::RETIRED_SKILLS[0].0);
        std::fs::create_dir_all(&owned).unwrap();
        std::fs::write(owned.join("notes.md"), "a person's own notes\n").unwrap();

        let mut outcomes = Vec::new();
        crate::platform::test_home::with_home(&home, || {
            skills(&repo, &args(), false, &mut outcomes).unwrap();
            retired_skills(&repo, &args(), false, &mut outcomes).unwrap();
        });

        assert!(
            owned.join("notes.md").is_file(),
            "a folder spoolway never installed must never be removed, \
             retired name or not"
        );
        assert!(
            !claude_dir.join("spoolway-plan").exists(),
            "sync must not install skills at user level when nobody ran \
             `install --user` there"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Holding the shipped skills alone must not be read as blanket
    /// permission to delete anything under a user folder: a real `install
    /// --user` ran here, so the folder is spoolway's to refresh, but the
    /// retired-name directory sitting beside the installed skills still
    /// predates #576 and is still a person's own. Before this fix,
    /// `retired_skills` chained every such user folder into its removal
    /// loop and deleted it anyway.
    #[test]
    fn a_user_folder_with_shipped_skills_still_keeps_a_hand_made_retired_name_directory() {
        let (repo, _root_guard) = fixture("skills-user-level");
        let home = crate::scratch::root("sync-user-home-shipped-and-owned");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        crate::platform::test_home::with_home(&home, || {
            crate::install::install_user(crate::cli::Provider::Claude, false).unwrap();
        });
        let claude_dir = crate::cli::Provider::Claude.user_skills_dir(&home);
        let owned = claude_dir.join(crate::install::RETIRED_SKILLS[0].0);
        std::fs::create_dir_all(&owned).unwrap();
        std::fs::write(owned.join("notes.md"), "a person's own notes\n").unwrap();

        let mut outcomes = Vec::new();
        crate::platform::test_home::with_home(&home, || {
            retired_skills(&repo, &args(), false, &mut outcomes).unwrap();
        });

        assert!(
            owned.join("notes.md").is_file(),
            "a retired-name directory predating #576 must never be removed \
             from a user folder, shipped skills beside it or not"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A folder installed by a binary from before #593 has no
    /// `.installed-by-spoolway` marker — a file spoolway no longer writes or
    /// reads at all. `user_skills` recognizes a user-level folder by the
    /// four shipped skill names instead, so a stale `spoolway-config/SKILL.md`
    /// installed by an old binary, marker or not, is still refreshed.
    #[test]
    fn a_user_level_skill_installed_before_593_is_still_refreshed_by_sync() {
        let (repo, _root_guard) = fixture("skills-user-level");
        let home = crate::scratch::root("sync-user-home-no-marker");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        crate::platform::test_home::with_home(&home, || {
            crate::install::install_user(crate::cli::Provider::Claude, false).unwrap();
        });
        let claude_dir = crate::cli::Provider::Claude.user_skills_dir(&home);
        let stale = claude_dir.join("spoolway-config").join("SKILL.md");
        let older =
            crate::known_copies::older::skill(crate::cli::Provider::Claude, "spoolway-config");
        std::fs::write(&stale, older).unwrap();

        let mut outcomes = Vec::new();
        crate::platform::test_home::with_home(&home, || {
            skills(&repo, &args(), false, &mut outcomes).unwrap();
        });

        let on_disk = std::fs::read_to_string(&stale).unwrap();
        assert_ne!(
            on_disk, older,
            "a user-level skill folder with no marker must still be \
             refreshed by sync"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `sync::skills` refreshes every installed provider, not `claude` alone —
    /// a codex or pi project kept stale skills after every sync otherwise
    /// (finding 27). The `path.exists()` guard still limits it to providers the
    /// project actually installed.
    #[test]
    fn skills_refreshes_every_installed_provider() {
        let (repo, _root_guard) = fixture("skills-providers");

        for provider in [crate::cli::Provider::Codex, crate::cli::Provider::Pi] {
            let first = provider.plan(&repo.root).into_iter().next().unwrap();
            std::fs::create_dir_all(first.path.parent().unwrap()).unwrap();
            std::fs::write(
                &first.path,
                crate::known_copies::older::skill(provider, "spoolway-plan"),
            )
            .unwrap();
        }

        let mut outcomes = Vec::new();
        skills(&repo, &args(), false, &mut outcomes).unwrap();
        let lines = outcome_lines(&outcomes);

        for provider_dir in [".agents", ".pi"] {
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with("wrote") && l.contains(provider_dir)),
                "{provider_dir} skills not refreshed: {lines:?}"
            );
        }
        for provider in [crate::cli::Provider::Codex, crate::cli::Provider::Pi] {
            let first = provider.plan(&repo.root).into_iter().next().unwrap();
            assert_eq!(
                std::fs::read_to_string(&first.path).unwrap(),
                first.contents,
                "{provider:?}'s stale skill was not rewritten"
            );
        }
    }

    /// A skill this release newly ships lands in every provider a project
    /// already installed — the guard is per provider, not per file. Decided
    /// per file, `spoolway-config` never reached a project that installed
    /// before the rename, while `retired_skills` removed `spoolway-pipeline`
    /// from under it: the project ended one skill short and doctor said
    /// everything checked out.
    #[test]
    fn sync_adds_a_newly_shipped_skill_to_an_installed_provider() {
        let (repo, _root_guard) = fixture("skills-new-skill");
        let claude_dir = crate::cli::Provider::Claude.skills_dir(&repo.root);
        let plan = claude_dir.join("spoolway-plan").join("SKILL.md");
        std::fs::create_dir_all(plan.parent().unwrap()).unwrap();
        std::fs::write(&plan, "stale, from an older release\n").unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), false, &mut outcomes).unwrap();
        let lines = outcome_lines(&outcomes);

        let config = claude_dir.join("spoolway-config").join("SKILL.md");
        assert!(config.exists(), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("spoolway-config/SKILL.md") && l.contains("added")),
            "a file that was not there is added, not rewritten: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("spoolway-plan/SKILL.md") && l.contains("rewritten")),
            "{lines:?}"
        );
        assert!(
            !crate::cli::Provider::Pi.skills_dir(&repo.root).exists(),
            "a provider the project never installed gets nothing"
        );
    }

    /// A skill file that is no copy any release shipped was written by a newer
    /// spoolway or edited by hand. Sync leaves it exactly as it is and refuses
    /// it, naming the file and the `--replace` that takes this binary's copy.
    #[test]
    fn a_hand_edited_skill_file_is_refused_and_left_alone() {
        let (repo, _root_guard) = fixture("skills-hand-edited");
        let planned = crate::cli::Provider::Claude.plan(&repo.root);
        let first = planned.into_iter().next().unwrap();
        std::fs::create_dir_all(first.path.parent().unwrap()).unwrap();
        let edited = format!("{}\na line a person added by hand\n", first.contents);
        std::fs::write(&first.path, &edited).unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), false, &mut outcomes).unwrap();

        assert_eq!(std::fs::read_to_string(&first.path).unwrap(), edited);
        let shown = ".claude/skills/spoolway-plan/SKILL.md";
        assert_eq!(
            refusals(&outcomes),
            [(shown, crate::known_copies::refusal(shown).as_str())]
        );
        assert!(
            !dedup_paths(&outcomes).0.contains(&shown),
            "a refused file is not a written one"
        );
    }

    /// The same file, as an older release wrote it, is stale and rewritten.
    #[test]
    fn an_older_shipped_skill_file_is_rewritten_and_named() {
        let (repo, _root_guard) = fixture("skills-older");
        let first = crate::cli::Provider::Claude
            .plan(&repo.root)
            .into_iter()
            .next()
            .unwrap();
        std::fs::create_dir_all(first.path.parent().unwrap()).unwrap();
        std::fs::write(
            &first.path,
            crate::known_copies::older::skill(crate::cli::Provider::Claude, "spoolway-plan"),
        )
        .unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), false, &mut outcomes).unwrap();

        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            first.contents
        );
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .any(|l| l.starts_with("wrote") && l.contains("rewritten")),
            "{:?}",
            outcome_lines(&outcomes)
        );
        assert!(refusals(&outcomes).is_empty());
    }

    /// The 0.5.0 case the whole change is for: a project with no fingerprint
    /// of any kind for its skill files — this binary never wrote one for
    /// them — still gets every stale file rewritten on the first sync, and
    /// the second sync then finds nothing left to do.
    #[test]
    fn a_project_with_no_fingerprint_at_all_is_brought_current_in_one_sync() {
        let (repo, _root_guard) = fixture("skills-0-5-0-project");
        let planned = crate::cli::Provider::Claude.plan(&repo.root);
        let first = planned.into_iter().next().unwrap();
        std::fs::create_dir_all(first.path.parent().unwrap()).unwrap();
        std::fs::write(
            &first.path,
            crate::known_copies::older::skill(crate::cli::Provider::Claude, "spoolway-plan"),
        )
        .unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), false, &mut outcomes).unwrap();
        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            first.contents,
            "{:?}",
            outcome_lines(&outcomes)
        );

        // Nothing left to do the second time around.
        let mut outcomes = Vec::new();
        skills(&repo, &args(), false, &mut outcomes).unwrap();
        assert!(
            outcomes.iter().all(|o| matches!(o, Outcome::Kept)),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// The rename case in full: a project whose only trace of an install is
    /// the retired directory still counts as installed, so the skill that
    /// replaced it is written in the same pass that removes the old one.
    #[test]
    fn sync_writes_the_renamed_skill_where_only_the_retired_one_stood() {
        let (repo, _root_guard) = fixture("skills-only-retired");
        let claude_dir = crate::cli::Provider::Claude.skills_dir(&repo.root);
        let stale = claude_dir.join("spoolway-pipeline");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("SKILL.md"), "the old skill\n").unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), false, &mut outcomes).unwrap();
        retired_skills(&repo, &args(), false, &mut outcomes).unwrap();

        assert!(claude_dir.join("spoolway-config").join("SKILL.md").exists());
        assert!(!stale.exists());
        for planned in crate::cli::Provider::Claude.plan(&repo.root) {
            assert!(planned.path.exists(), "{}", planned.path.display());
        }
    }

    /// A dry run over the same tree reports every addition and writes none.
    #[test]
    fn a_dry_run_reports_a_missing_skill_without_writing_it() {
        let (repo, _root_guard) = fixture("skills-new-skill-dry");
        let claude_dir = crate::cli::Provider::Claude.skills_dir(&repo.root);
        let plan = claude_dir.join("spoolway-plan").join("SKILL.md");
        std::fs::create_dir_all(plan.parent().unwrap()).unwrap();
        std::fs::write(&plan, "stale\n").unwrap();

        let mut outcomes = Vec::new();
        skills(
            &repo,
            &SyncArgs {
                dry_run: true,
                ..args()
            },
            false,
            &mut outcomes,
        )
        .unwrap();

        assert!(!claude_dir.join("spoolway-config").exists());
        assert_eq!(std::fs::read_to_string(&plan).unwrap(), "stale\n");
        let lines = outcome_lines(&outcomes);
        assert!(
            lines.iter().any(|l| l.contains("spoolway-config/SKILL.md")),
            "{lines:?}"
        );
    }

    /// Codex moved its repository skill root from `.codex/skills` to
    /// `.agents/skills`, and a 0.1.0 install lives under the old one. A sync
    /// is the one chance to carry it forward without asking every project to
    /// reinstall by hand: the current set is written where Codex reads it
    /// now, and the copies spoolway itself put under the old root — current
    /// names and retired ones alike — go with the move. Skills carry no
    /// local edits by design, which is what makes that safe; a directory the
    /// project made there is not spoolway's and stays.
    #[test]
    fn sync_moves_codex_skills_out_of_the_old_root() {
        let (repo, _root_guard) = fixture("skills-codex-old-root");
        let old_root = repo.root.join(".codex").join("skills");
        for name in ["spoolway-plan", "spoolway-pipeline", "a-projects-own-skill"] {
            let dir = old_root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), "from 0.1.0\n").unwrap();
        }

        let mut outcomes = Vec::new();
        skills(&repo, &args(), false, &mut outcomes).unwrap();
        let lines = outcome_lines(&outcomes);

        let new_root = crate::cli::Provider::Codex.skills_dir(&repo.root);
        for planned in crate::cli::Provider::Codex.plan(&repo.root) {
            assert_eq!(
                std::fs::read_to_string(&planned.path).unwrap(),
                planned.contents,
                "{} was not migrated",
                planned.path.display()
            );
        }
        assert!(new_root.join("spoolway-config").is_dir());
        assert!(!old_root.join("spoolway-plan").exists(), "{lines:?}");
        assert!(!old_root.join("spoolway-pipeline").exists(), "{lines:?}");
        assert!(
            old_root
                .join("a-projects-own-skill")
                .join("SKILL.md")
                .exists(),
            "a directory spoolway never shipped is not spoolway's to remove"
        );
        assert!(old_root.is_dir(), "and so the root holding it stays");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("removed .codex/skills/spoolway-plan")),
            "{lines:?}"
        );
    }

    /// A 0.1.0 codex root holding only spoolway's own skills is empty after
    /// the move, and an empty `.codex/` is nothing but a question — so both
    /// go with it.
    #[test]
    fn an_emptied_codex_root_goes_with_the_move() {
        let (repo, _root_guard) = fixture("skills-codex-emptied");
        let dir = repo
            .root
            .join(".codex")
            .join("skills")
            .join("spoolway-plan");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "from 0.1.0\n").unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), false, &mut outcomes).unwrap();

        assert!(!repo.root.join(".codex").exists());
    }

    /// The same move in a dry run is reported and not made.
    #[test]
    fn a_dry_run_reports_the_codex_move_without_making_it() {
        let (repo, _root_guard) = fixture("skills-codex-old-root-dry");
        let old_root = repo.root.join(".codex").join("skills");
        let dir = old_root.join("spoolway-plan");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "from 0.1.0\n").unwrap();

        let mut outcomes = Vec::new();
        skills(
            &repo,
            &SyncArgs {
                dry_run: true,
                ..args()
            },
            false,
            &mut outcomes,
        )
        .unwrap();

        assert!(dir.join("SKILL.md").exists());
        assert!(!crate::cli::Provider::Codex.skills_dir(&repo.root).exists());
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .any(|l| l.starts_with("removed .codex/skills/spoolway-plan")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// A project that ran `install` before the rename has a stale
    /// `spoolway-pipeline/` directory sitting beside its skills — `sync`
    /// removes it, and a directory a sync has no reason to touch (a
    /// project's own skill, sharing no name with anything on the retired
    /// list) is left exactly as it was.
    #[test]
    fn sync_removes_a_stale_renamed_skill_and_leaves_everything_else() {
        let (repo, _root_guard) = fixture("retired-skill");
        let claude_dir = crate::cli::Provider::Claude.skills_dir(&repo.root);
        let stale = claude_dir.join("spoolway-pipeline");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("SKILL.md"), "the old skill\n").unwrap();

        let untouched = claude_dir.join("a-projects-own-skill");
        std::fs::create_dir_all(&untouched).unwrap();
        std::fs::write(untouched.join("SKILL.md"), "not spoolway's\n").unwrap();

        let mut outcomes = Vec::new();
        retired_skills(&repo, &args(), false, &mut outcomes).unwrap();

        assert!(!stale.exists(), "the retired directory must be removed");
        assert!(
            untouched.is_dir() && untouched.join("SKILL.md").is_file(),
            "a directory not on the retired list must never be touched"
        );
        let lines = outcome_lines(&outcomes);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("removed") && l.contains("spoolway-pipeline")),
            "{lines:?}"
        );
    }

    /// `spoolway-doctor` is retired outright, not renamed, so its own entry
    /// on [`crate::install::RETIRED_SKILLS`] carries a different reason than
    /// `spoolway-pipeline`'s — repair moved into `spoolway-config` rather
    /// than a file simply changing name.
    #[test]
    fn sync_removes_an_installed_doctor_skill_with_its_own_reason() {
        let (repo, _root_guard) = fixture("retired-doctor-skill");
        let claude_dir = crate::cli::Provider::Claude.skills_dir(&repo.root);
        let stale = claude_dir.join("spoolway-doctor");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("SKILL.md"), "the old skill\n").unwrap();

        let mut outcomes = Vec::new();
        retired_skills(&repo, &args(), false, &mut outcomes).unwrap();

        assert!(!stale.exists(), "the retired directory must be removed");
        let lines = outcome_lines(&outcomes);
        assert!(
            lines.iter().any(|l| l.starts_with("removed")
                && l.contains("spoolway-doctor")
                && l.contains("retired: /spoolway-config repairs a project now")),
            "{lines:?}"
        );
    }

    /// A dry run reports the removal without actually deleting anything —
    /// the same promise every other `sync` scan already keeps.
    #[test]
    fn sync_dry_run_reports_a_stale_skill_without_removing_it() {
        let (repo, _root_guard) = fixture("retired-skill-dry-run");
        let stale = crate::cli::Provider::Claude
            .skills_dir(&repo.root)
            .join("spoolway-pipeline");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("SKILL.md"), "the old skill\n").unwrap();

        let mut outcomes = Vec::new();
        let dry = SyncArgs {
            dry_run: true,
            replace: Vec::new(),
        };
        retired_skills(&repo, &dry, false, &mut outcomes).unwrap();

        assert!(stale.is_dir(), "a dry run must not delete anything");
        assert_eq!(outcome_lines(&outcomes).len(), 1);
    }

    /// All five dead templates go, each with its own reason, and a project's
    /// own file under `.spoolway/templates/` — named by neither
    /// [`crate::install::RETIRED_TEMPLATES`] nor a shape `init` still
    /// places — is left exactly where it was. `epic.md` and `ticket.md` are
    /// removed whatever they hold — one left empty, the other still carrying
    /// a project's own words — since sync never inspects a retired
    /// template's contents before deleting it.
    #[test]
    fn sync_removes_every_retired_template_and_leaves_a_projects_own_file() {
        let (repo, _root_guard) = fixture("retired-templates");
        let dir = repo.checkout.join(".spoolway/templates");
        let task_log = dir.join("task-log.md");
        let pull_request = dir.join("pull-request.md");
        let lane_prompts = dir.join("lane-prompts.md");
        let tracking = dir.join("tracking");
        std::fs::create_dir_all(&tracking).unwrap();
        let epic = tracking.join("epic.md");
        let ticket = tracking.join("ticket.md");
        std::fs::write(&task_log, "stale\n").unwrap();
        std::fs::write(&pull_request, "stale\n").unwrap();
        std::fs::write(&lane_prompts, "stale\n").unwrap();
        std::fs::write(&epic, "").unwrap();
        std::fs::write(&ticket, "a project's own words\n").unwrap();
        let untouched = dir.join("a-projects-own-notes.md");
        std::fs::write(&untouched, "mine\n").unwrap();

        let mut outcomes = Vec::new();
        retired_templates(&repo, &args(), &mut outcomes).unwrap();

        assert!(!task_log.exists(), "task-log.md must be removed");
        assert!(!pull_request.exists(), "pull-request.md must be removed");
        assert!(!lane_prompts.exists(), "lane-prompts.md must be removed");
        assert!(!epic.exists(), "epic.md must be removed even when empty");
        assert!(!ticket.exists(), "ticket.md must be removed even with text");
        assert!(
            untouched.is_file(),
            "a file not on the retired list must never be touched"
        );

        let lines = outcome_lines(&outcomes);
        assert!(
            lines.iter().any(|l| {
                l.starts_with("removed")
                    && l.contains("task-log.md")
                    && l.contains("no longer written to a task file")
            }),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| {
                l.starts_with("removed")
                    && l.contains("pull-request.md")
                    && l.contains("the pull request body is the task file itself")
            }),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| {
                l.starts_with("removed")
                    && l.contains("lane-prompts.md")
                    && l.contains("the lane messages are spoolway's own")
            }),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| {
                l.starts_with("removed")
                    && l.contains("tracking/epic.md")
                    && l.contains("the issue body is the hook's own")
            }),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| {
                l.starts_with("removed")
                    && l.contains("tracking/ticket.md")
                    && l.contains("the issue body is the hook's own")
            }),
            "{lines:?}"
        );
    }

    /// A project with a missing `config.toml` and a retired template in a
    /// directory nothing can be removed from. Returns the directory, locked.
    #[cfg(unix)]
    fn project_with_a_template_that_cannot_be_removed(
        name: &str,
    ) -> (Repo, PathBuf, PathBuf, crate::scratch::ScratchRoot) {
        use std::os::unix::fs::PermissionsExt;
        let (repo, guard) = fixture(name);
        let dir = repo.checkout.join(".spoolway/templates/tracking");
        std::fs::create_dir_all(&dir).unwrap();
        let epic = dir.join("epic.md");
        std::fs::write(&epic, "").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        (repo, dir, epic, guard)
    }

    /// Root, or a mount that ignores the mode, can remove from a directory
    /// marked read-only, and then there is no failure to provoke.
    #[cfg(unix)]
    fn locked_dir_still_takes_files(dir: &Path) -> bool {
        can_create_in(dir).is_ok()
    }

    /// One file that cannot be removed stops the whole sync before it writes
    /// anything, and the error names that file and says nothing was written.
    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_removed_stops_the_sync_before_any_write() {
        use std::os::unix::fs::PermissionsExt;
        let (repo, dir, epic, _guard) =
            project_with_a_template_that_cannot_be_removed("check-before-write");
        if locked_dir_still_takes_files(&dir) {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let result = scan(&repo, &args());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let said = format!(
            "{:#}",
            result.err().expect("an unremovable file fails the sync")
        );
        assert!(
            said.contains("cannot remove .spoolway/templates/tracking/epic.md: Permission denied"),
            "{said}"
        );
        assert!(said.contains("nothing was written"), "{said}");
        assert!(epic.exists(), "the file it could not remove is still there");
        assert!(
            !Config::path_in(&repo.checkout).exists(),
            "config.toml would have been written first; the check has to come before it"
        );
    }

    /// A probe a killed sync left behind, under the name the next process id
    /// would pick, is stepped over rather than reported as a failure.
    #[test]
    fn a_stale_probe_file_does_not_stop_the_check() {
        let (repo, _guard) = fixture("stale-probe");
        let stale = repo
            .checkout
            .join(format!(".spoolway-probe-{}-0", std::process::id()));
        std::fs::write(&stale, "").unwrap();

        can_create_in(&repo.checkout).expect("a taken probe name is skipped");

        assert!(stale.exists(), "the stray file is left for its owner");
    }

    /// A write that fails after others succeeded lists every file already
    /// changed, in the order they were changed, and says they are not undone.
    #[test]
    fn a_write_that_fails_part_way_lists_every_file_already_changed() {
        let (repo, _guard) = fixture("apply-part-way");
        let first = repo.checkout.join("first.txt");
        let second = repo.checkout.join("second.txt");
        let blocker = repo.checkout.join("blocker");
        std::fs::write(&blocker, "a file where a directory is needed\n").unwrap();
        let acts = vec![
            Act::write(first.clone(), "first.txt", "one\n"),
            Act::write(second.clone(), "second.txt", "two\n"),
            Act::write(blocker.join("third.txt"), "blocker/third.txt", "three\n"),
            Act::write(repo.checkout.join("fourth.txt"), "fourth.txt", "four\n"),
        ];

        let said = format!(
            "{:#}",
            apply(acts).expect_err("the third write cannot be made")
        );

        assert!(said.contains("blocker/third.txt"), "{said}");
        assert!(
            said.contains("already changed, and not undone:\n  first.txt\n  second.txt"),
            "{said}"
        );
        assert!(first.exists() && second.exists());
        assert!(
            !repo.checkout.join("fourth.txt").exists(),
            "a failure stops the run; it does not go on to the next file"
        );
    }

    /// A failure on the very first change says so, rather than listing none.
    #[test]
    fn a_first_write_that_fails_says_nothing_else_was_changed() {
        let (repo, _guard) = fixture("apply-first");
        let blocker = repo.checkout.join("blocker");
        std::fs::write(&blocker, "x\n").unwrap();

        let said = format!(
            "{:#}",
            apply(vec![Act::write(
                blocker.join("a.txt"),
                "blocker/a.txt",
                "a\n"
            )])
            .expect_err("the parent is a file")
        );

        assert!(said.contains("nothing else was changed"), "{said}");
    }

    /// The dry run and the real run read one plan: the same files come out of
    /// both, and they are exactly the files the plan acts on.
    #[test]
    fn the_dry_run_lists_the_files_the_real_run_changes() {
        let (repo, _guard) = fixture("dry-matches-real");
        let dir = repo.checkout.join(".spoolway/templates");
        std::fs::write(dir.join("task-log.md"), "stale\n").unwrap();
        std::fs::write(dir.join("pull-request.md"), "stale\n").unwrap();

        let dry = SyncArgs {
            dry_run: true,
            replace: Vec::new(),
        };
        let listed = |outcomes: &[Outcome]| {
            let (wrote, removed) = dedup_paths(outcomes);
            let mut all: Vec<String> = wrote.iter().map(|p| format!("write {p}")).collect();
            all.extend(removed.iter().map(|(p, _)| format!("remove {p}")));
            all
        };
        let (_, acts) = plan(&repo).unwrap();
        let mut acted: Vec<String> = acts
            .iter()
            .filter_map(|act| act.shown().map(str::to_string))
            .collect();

        let before = listed(&scan(&repo, &dry).unwrap());
        assert!(
            dir.join("task-log.md").exists(),
            "a dry run removes nothing"
        );
        let after = listed(&scan(&repo, &args()).unwrap());

        assert!(!before.is_empty(), "the project is behind on purpose");
        assert_eq!(before, after);
        let mut named: Vec<String> = before
            .iter()
            .map(|line| line.split_once(' ').unwrap().1.to_string())
            .collect();
        named.sort();
        acted.sort();
        assert_eq!(named, acted, "the report names exactly the plan's files");
        assert!(!dir.join("task-log.md").exists());
    }

    /// A dry run reports every removal without deleting anything — the same
    /// promise [`sync_dry_run_reports_a_stale_skill_without_removing_it`]
    /// keeps for a retired skill.
    #[test]
    fn sync_dry_run_reports_retired_templates_without_removing_them() {
        let (repo, _root_guard) = fixture("retired-templates-dry-run");
        let dir = repo.checkout.join(".spoolway/templates");
        let task_log = dir.join("task-log.md");
        let pull_request = dir.join("pull-request.md");
        let lane_prompts = dir.join("lane-prompts.md");
        std::fs::write(&task_log, "stale\n").unwrap();
        std::fs::write(&pull_request, "stale\n").unwrap();
        std::fs::write(&lane_prompts, "stale\n").unwrap();

        let mut outcomes = Vec::new();
        let dry = SyncArgs {
            dry_run: true,
            replace: Vec::new(),
        };
        retired_templates(&repo, &dry, &mut outcomes).unwrap();

        assert!(task_log.is_file(), "a dry run must not delete anything");
        assert!(pull_request.is_file(), "a dry run must not delete anything");
        assert!(lane_prompts.is_file(), "a dry run must not delete anything");
        assert_eq!(outcome_lines(&outcomes).len(), 3);
    }

    /// A project brought forward from before this change may still carry the
    /// old per-skill stamp — a real sync clears it out, since nothing reads
    /// it any more.
    #[test]
    fn sync_removes_a_leftover_skill_stamp() {
        let (repo, _root_guard) = fixture("skill-stamp-leftover");
        let stamp = repo.home.join(SKILL_STAMP_FILE);
        std::fs::create_dir_all(&repo.home).unwrap();
        std::fs::write(&stamp, "some-fingerprint /a/skill/SKILL.md\n").unwrap();

        run(&repo, &args()).unwrap();

        assert!(!stamp.exists(), "a real sync must remove the old stamp");
    }

    /// A dry run reads and says, never writes — clearing the old stamp is no
    /// exception.
    #[test]
    fn a_dry_run_leaves_a_leftover_skill_stamp_alone() {
        let (repo, _root_guard) = fixture("skill-stamp-leftover-dry-run");
        let stamp = repo.home.join(SKILL_STAMP_FILE);
        std::fs::create_dir_all(&repo.home).unwrap();
        std::fs::write(&stamp, "some-fingerprint /a/skill/SKILL.md\n").unwrap();

        run(
            &repo,
            &SyncArgs {
                dry_run: true,
                ..args()
            },
        )
        .unwrap();

        assert!(stamp.exists(), "a dry run must not remove the old stamp");
    }

    /// `sync` leaves no `sync-stamp` file in the project's home: nothing
    /// reads one any more, so writing it would only be clutter.
    #[test]
    fn sync_writes_no_sync_stamp() {
        let (repo, _root_guard) = fixture("no-sync-stamp");
        run(&repo, &args()).unwrap();
        assert!(!repo.home.join("sync-stamp").exists());
    }

    /// Builds of 0.10.0 wrote a version marker in the setup folder and in each
    /// user-level skills folder. Nothing reads them now, so a sync deletes the
    /// ones it finds and says so as it says any removal, and a second sync has
    /// nothing left to report.
    #[test]
    fn sync_deletes_the_retired_version_markers() {
        let (repo, _root_guard) = fixture("retired-markers");
        let home = crate::scratch::root("sync-retired-markers-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let project = repo.setup_dir().join("spoolway-version");
        let user = crate::cli::Provider::Claude
            .user_skills_dir(&home)
            .join(".spoolway-version");
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(&project, "0.10.0\n").unwrap();
        std::fs::write(&user, "0.10.0\n").unwrap();

        let dry = SyncArgs {
            dry_run: true,
            replace: Vec::new(),
        };
        let planned = crate::platform::test_home::with_home(&home, || scan(&repo, &dry).unwrap());
        let removed = |outcomes: &[Outcome]| -> Vec<String> {
            let mut paths: Vec<String> = outcomes
                .iter()
                .filter_map(|outcome| match outcome {
                    Outcome::Removed { path, .. } => Some(path.clone()),
                    _ => None,
                })
                .collect();
            paths.sort();
            paths
        };
        let want = [
            ".spoolway/spoolway-version".to_string(),
            "~/.claude/skills/.spoolway-version".to_string(),
        ];
        assert_eq!(removed(&planned), want, "{:?}", outcome_lines(&planned));
        assert!(project.exists() && user.exists(), "a dry run deleted one");

        let done = crate::platform::test_home::with_home(&home, || scan(&repo, &args()).unwrap());
        assert_eq!(removed(&done), want, "{:?}", outcome_lines(&done));
        assert!(!project.exists(), "the project marker is still there");
        assert!(!user.exists(), "the user marker is still there");

        let again = crate::platform::test_home::with_home(&home, || scan(&repo, &args()).unwrap());
        assert!(removed(&again).is_empty(), "{:?}", outcome_lines(&again));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A marker another clone's `sync` deleted after it was found is already
    /// gone, so removing it answers nothing to report instead of an error.
    #[test]
    fn removing_a_marker_that_is_already_gone_is_not_an_error() {
        let root = crate::scratch::root("marker-already-gone");
        let gone = root.join("spoolway-version");
        let found = Some((gone, ".spoolway/spoolway-version".to_string()));
        assert_eq!(remove_marker(found).unwrap(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A refusal is about files that may be ahead of this binary or hand
    /// edited, so its closing line must not call them behind.
    #[test]
    fn no_closing_line_calls_a_refused_file_behind() {
        let other = [(".spoolway/pipelines/default.yml", "never ends")];
        assert_eq!(Refused::of(&other), Refused::Files);
        assert_eq!(Refused::of(&[]), Refused::Nothing);
        assert!(!REFUSED_ONLY.contains("behind"), "{REFUSED_ONLY}");
        assert!(REFUSED_ONLY.contains("left as they are"), "{REFUSED_ONLY}");
        assert_eq!(closing_line(false, true, Refused::Files), REFUSED_ONLY);
    }
}
