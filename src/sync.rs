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
//! "helped". [`skills`] does not keep this promise either, for the same reason
//! [`config`] and [`pipelines`] do not — see the `config.toml` paragraph
//! below.
//!
//! Prompts, their assets, and task skeletons are not here at all, and that is
//! the design. All are prose a project owns outright, with nothing generated
//! inside them to keep current: what a lane is told about finishing comes from
//! the opening prompt, and a task's frontmatter is serialised from a struct in
//! the binary on every save. Syncing spoolway therefore cannot disturb a word
//! a project wrote in any of them, and `spoolway pipeline check` is what
//! catches a command name that has fallen behind the CLI.
//!
//! Everything below writes `repo.checkout`, never `repo.root`: the control
//! plane this command refreshes is tracked, so a lane running in a linked
//! worktree has its own branch's copies, and a sync taken there has to
//! land on that branch — where it is reviewed and merged — rather than on
//! whatever the main checkout happens to have out. `config set` and
//! `override promote` keep the opposite rule and refuse outright in a linked
//! worktree; this command does not.
//!
//! A prompt's assets are the document skeletons, and they were a page template
//! until they moved under the archivist. That move was the whole point: a
//! skeleton spoolway keeps current is one a project cannot restructure, and
//! restructuring documentation is exactly what a project should be able to do.
//!
//! `config.toml` is the file that works the other way round, and [`config`]
//! says why: there the values are the project's and everything around them —
//! which settings are written down, and every comment — is spoolway's, and is
//! rewritten outright. A pipeline file's key reference is the same bargain in
//! one fenced block of an otherwise untouched file; [`pipelines`] says why it
//! is documentation of the binary's contract rather than anything a project
//! meant. A skill file is the whole-file version of the same bargain: nothing
//! in one is a project's to have meant, so [`skills`] rewrites it outright
//! wherever it differs from the shipped copy, hand edit or not — in the
//! project's own skill folders and in the agent's user folder alike.
//!
//! This used to be `spoolway update`'s job, along with installing the binary
//! itself. The two were split apart so `update` can run from any directory,
//! including one with no project in it: installing a binary is a fact about
//! this machine's `PATH`, and file work is a fact about a checkout that has
//! to exist first. See [`crate::update`] for the half that stayed there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::SyncArgs;
use crate::repo::Repo;
use crate::task::write_atomic;

/// What happened to one file, in the order a person wants to read it.
///
/// The path and the reason are separate fields because they have separate
/// audiences: the report prints paths and nothing else — which files moved is
/// the only question anybody runs this to answer — while `spoolway doctor`
/// reads the reasons, since a refusal that is never said is a project that
/// quietly stays behind.
pub enum Outcome {
    Wrote {
        path: String,
        detail: String,
    },
    /// A file rewritten to drop a retired setting, with what that drop
    /// means for the project spelled out — see [`config`]'s own
    /// `issue_tracking.on_fail` and `dispatch.worktree_root` notes. Its own
    /// variant rather than another [`Outcome::Wrote`], because its detail is
    /// one of the few this
    /// report actually prints under the file's own `wrote` line, in both the
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

/// What a non-dry sync says when the scan wrote or removed nothing: `KEPT`
/// talks about files being overwritten, which is false when there were none
/// to overwrite and reads as the tool lying about having touched the tree.
const NOOP: &str = "Nothing updating.";

pub fn run(repo: &Repo, args: &SyncArgs, json: bool) -> Result<()> {
    if !args.replace.is_empty() {
        return replace(repo, args);
    }

    // Printed here, not below: this project's own files are what the note is
    // about, and a lane running in a linked worktree needs to know which
    // checkout is about to be rewritten before it reads the report.
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
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
    // A dry run reports the same paths in the conditional: "wrote" over a
    // tree nothing touched reads as a lie the moment `git status` is run.
    let (wrote_word, removed_word) = match args.dry_run {
        true => ("would write ", "would remove"),
        false => ("wrote       ", "removed     "),
    };
    for path in &wrote {
        println!("  {wrote_word} {path}");
        for (report, _) in notes.get(path).into_iter().flatten() {
            println!("               ({report})");
        }
    }
    for (path, why) in &removed {
        println!("  {removed_word} {path}");
        println!("               ({why})");
    }

    println!();
    match (args.dry_run, wrote.is_empty() && removed.is_empty()) {
        (true, _) => println!("{DRY_RUN}"),
        (false, true) => println!("{NOOP}"),
        (false, false) => println!("{KEPT}"),
    }

    // Only once the write has actually happened: the stamp records what a
    // checkout was last brought to, and a dry run brings it to nothing.
    if !args.dry_run {
        write_stamp(&repo.home, &repo.checkout)?;
        remove_skill_stamp(&repo.home);
    }
    Ok(())
}

/// What `spoolway sync` says when the panel was answered with esc or ctrl-c.
const CANCELLED: &str = "Nothing was changed.";

/// Every body line, truncated to this many characters before
/// [`crate::screen::panel`] sizes the box around it — so the panel's total
/// width, its two border columns included, never exceeds 80 columns whatever
/// the paths in it are.
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

/// The rows inside the confirm panel: a blank row under [`TITLE`], then what
/// `sync` would write — every write, with a retired-shape migration's own
/// short note under it where `notes` carries one, then every removal with
/// its reason on the line under it, then the one sentence that answers "did
/// it eat my config?" before anybody has pressed anything.
fn panel_body(
    wrote: &[&str],
    notes: &BTreeMap<&str, Vec<(&str, &str)>>,
    removed: &[(&str, &str)],
) -> Vec<String> {
    let mut body = vec![String::new()];
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
/// a script, `--json` and a lane all reach `run` without a question.
pub(crate) fn run_asking(repo: &Repo, args: &SyncArgs, json: bool, in_lane: bool) -> Result<()> {
    run_asking_with(
        repo,
        args,
        json,
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
    json: bool,
    in_lane: bool,
    interactive: bool,
    input: &mut impl crate::screen::PollableRead,
    out: &mut impl std::io::Write,
    term: impl FnOnce() -> crate::platform::TermGuard,
    interrupt: impl FnOnce() -> SigintGuard,
) -> Result<()> {
    // Nobody to answer, or nothing to ask about: a script (the e2e suites run
    // `sync` with no terminal), `--json`, and a lane — whose pane can carry a
    // real terminal nobody is watching, so a panel parked there would hang
    // the step until its own timeout — write straight away. `--dry-run` writes nothing to ask
    // about, and `--replace` names its files on the command line already.
    if !interactive || json || in_lane || args.dry_run || !args.replace.is_empty() {
        return run(repo, args, json);
    }
    let dry = SyncArgs {
        dry_run: true,
        replace: Vec::new(),
    };
    let outcomes = scan(repo, &dry)?;
    let (wrote, removed) = dedup_paths(&outcomes);
    if wrote.is_empty() && removed.is_empty() {
        return run(repo, args, json);
    }
    let notes = migration_notes(&outcomes);
    let keys = crate::screen::keys(&[("enter", "apply"), ("esc", "cancel")]);
    let body = panel_body(&wrote, &notes, &removed);
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
        Answer::Apply => run(repo, args, json),
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

/// The `(migrated: …)` lines a scan's own [`Outcome::Migrated`] entries
/// carry, keyed by path and kept in the order they were recorded — the
/// extra explanation [`run`]'s own report and [`run_asking`]'s confirm panel
/// each draw under a pipeline file's `wrote` line, one tuple of `(report,
/// panel)` per change so each surface reads the length it can afford.
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

/// Every file spoolway owns here, and what would happen to it.
///
/// Split out from [`run`] so that `spoolway doctor` can ask the same question
/// without printing anything — which is where a [`Outcome::Blocked`] surfaces,
/// now that the report itself is only paths.
/// The `detail` a [`Outcome::Wrote`] carries for a file that was not there
/// at all — named so `doctor` can tell one apart from a file that was there
/// and out of date, which is a different thing to advise about.
pub const MISSING: &str = "was missing";

pub fn scan(repo: &Repo, args: &SyncArgs) -> Result<Vec<Outcome>> {
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
    // `ignores` and the project-level half of `skills` join `repo.checkout`
    // directly instead, with no such indirection, so they are the two steps
    // that need telling.
    let home_mode = crate::repo::workspace_clone(&repo.root).is_some();
    let mut outcomes = Vec::new();
    ignores(repo, args, home_mode, &mut outcomes)?;
    config(repo, args, &mut outcomes)?;
    templates(repo, args, &mut outcomes)?;
    skills(repo, args, home_mode, &mut outcomes)?;
    retired_skills(repo, args, home_mode, &mut outcomes)?;
    retired_templates(repo, args, &mut outcomes)?;
    pipelines(repo, args, &mut outcomes)?;
    Ok(outcomes)
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
    args: &SyncArgs,
    home_mode: bool,
    outcomes: &mut Vec<Outcome>,
) -> Result<()> {
    use crate::gitignore::Removed;

    if home_mode {
        return Ok(());
    }

    let shown = crate::platform::relative(&repo.checkout, &crate::gitignore::file(&repo.checkout));
    match crate::gitignore::remove(&repo.checkout, args.dry_run)? {
        Removed::Gone => outcomes.push(Outcome::wrote(&shown, "spoolway's old block removed")),
        Removed::Absent => {}
        Removed::Unterminated => outcomes.push(Outcome::blocked(
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

/// The config file: rewritten whole, except for what you set it to.
///
/// The one file spoolway owns outright and a person reads constantly, so the
/// two halves are split the other way round from every other file here: the
/// **values** are the project's and survive a sync untouched, and everything
/// else — which settings are written down, what order they come in, and every
/// comment — is spoolway's and is rewritten from the binary.
///
/// That is why the explanations in `config.toml` can be trusted: a note there
/// is what this binary says today, not a copy of what some older one said, and
/// not somebody's sentence sitting where a note belongs. Prose about a
/// project's own choices has somewhere better to live — a prompt, a pipeline,
/// or project documentation — where it is read by whoever needs it
/// rather than by nobody.
///
/// Until this existed nothing brought a config forward at all, and the only
/// thing that ever added a missing key was `config set` re-rendering the whole
/// file: the right rewrite, performed at the wrong moment, while somebody was
/// changing an interval. See [`crate::confdoc`].
fn config(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
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
            if !args.dry_run {
                crate::config::Config::default().save(&repo.checkout)?;
            }
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
    // `load_dropping_retired_keys` rather than `Config::load`: `dispatch.
    // interval` and `issue_tracking.on_fail` are each retired hard enough
    // that an ordinary load refuses a file still naming either, and this is
    // the one place that has to bring such a file forward instead of
    // rejecting it.
    // A config this binary cannot even parse, same as the read failure
    // above: this fails loudly rather than recording a quiet
    // `Outcome::Blocked` a plain `sync` or `sync --dry-run` never prints.
    // Named here rather than left to `load_dropping_retired_keys`'s own
    // error: the retired-key strip it runs first (`crate::confdoc::remove`)
    // parses the raw text itself, ahead of the `toml::from_str` that would
    // otherwise have named the file, so its failure says only "this file is
    // not valid TOML" with no path at all — and `spoolway config edit` is
    // named here too, since it reads this same file leniently for exactly
    // this reason (see `Command::Config(ConfigCommand::Edit)` in `main.rs`).
    let current = crate::config::Config::load_dropping_retired_keys(&repo.checkout)
        .with_context(|| format!("{} — fix it with `spoolway config edit`", path.display()))?;

    let rewritten = current.render()?;

    // Its own work, checked. A rewrite may not change a single value — every one
    // of them came out of this file a moment ago — so a rendering that no longer
    // means what the file meant is a bug, and is not written.
    if let Err(err) = current.agrees_with(&rewritten) {
        outcomes.push(Outcome::blocked(
            &shown,
            format!("rewriting it would have changed what it says — {err:#}"),
        ));
        return Ok(());
    }

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
    if !refresh.added.is_empty() {
        outcomes.push(Outcome::wrote(
            &shown,
            format!(
                "{} setting(s) this spoolway has and it did not: {}",
                refresh.added.len(),
                listed(&refresh.added)
            ),
        ));
    }
    if !refresh.dropped.is_empty() {
        outcomes.push(Outcome::wrote(
            &shown,
            format!(
                "{} retired setting(s) dropped: {}",
                refresh.dropped.len(),
                listed(&refresh.dropped)
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
                "migrated: issue_tracking.on_fail removed, every failing hook now pauses \
                 its task; [issue_tracking] moved directly under [watch]",
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
            .then(|| {
                toml::from_str::<toml::Value>(&text).ok().and_then(|doc| {
                    doc.get("dispatch")?
                        .get("worktree_root")?
                        .as_str()
                        .map(str::to_string)
                })
            })
            .flatten()
            .filter(|path| !path.trim().is_empty());
        if let Some(old) = old_worktree_root {
            outcomes.push(Outcome::migrated(
                &shown,
                format!(
                    "migrated: dispatch.worktree_root removed; its worktrees were cut at \
                     {old} — yours to remove",
                ),
                format!("migrated: worktree_root removed; worktrees were at {old}"),
            ));
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
    // Whitespace and key order, and nothing a person would recognise as a
    // setting. Recorded anyway, because a file that came back changed with
    // nothing said about it is the one thing worse than a noisy report — and
    // this is the detail `doctor` reads, not something the report prints.
    if refresh.is_empty() {
        outcomes.push(Outcome::wrote(&shown, "relaid out"));
    }

    if !args.dry_run {
        write_atomic(&path, &rewritten)?;
    }
    Ok(())
}

/// The page skeletons: their one machine-read block, and nothing else.
///
/// These are meant to be restyled, so taking the file back only when it is
/// byte-for-byte ours would block every project that used them as intended.
/// And they are not decoration either: each carries a block
/// something parses, and a restyled skeleton whose block predates a schema
/// change writes pages that are quietly short of a field. So the block is kept
/// current and the styling around it is never read. See [`crate::skeleton`].
fn templates(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
    for skeleton in crate::skeleton::skeletons() {
        let path = repo.checkout.join(skeleton.path);
        let shown = crate::platform::relative(&repo.checkout, &path);

        let on_disk = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if !args.dry_run {
                    write_atomic(&path, skeleton.shipped)?;
                }
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
                write_block(&path, &shown, &on_disk, &skeleton, args, outcomes, "block")?;
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
    args: &SyncArgs,
    outcomes: &mut Vec<Outcome>,
    note: &str,
) -> Result<()> {
    let Some(next) = skeleton.region.replace(on_disk, skeleton.block()) else {
        outcomes.push(Outcome::blocked(shown, "its block moved while we read it"));
        return Ok(());
    };
    if !args.dry_run {
        write_atomic(path, &next)?;
    }
    outcomes.push(Outcome::wrote(shown, note));
    Ok(())
}

/// `--replace`: the whole shipped file, over whatever is there.
///
/// The way back from a file so far from the shape we know that no region can be
/// found in it, or one whose machine-readable block was hand-edited and so is
/// left alone by the in-place rewrite. Everything about it is deliberately
/// blunt: named paths only, the old contents saved beside the new, and a count
/// of what is being discarded.
fn replace(repo: &Repo, args: &SyncArgs) -> Result<()> {
    let mut failed = 0;
    for name in &args.replace {
        let path = match std::path::Path::new(name).is_absolute() {
            true => PathBuf::from(name),
            false => repo.checkout.join(name),
        };
        let shown = crate::platform::relative(&repo.checkout, &path);

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
                    crate::platform::relative(&repo.checkout, &nested)
                );
                failed += 1;
                continue;
            }
        }

        let Some(shipped) = shipped_for(repo, &path) else {
            println!(
                "  ! {shown}: not a file spoolway ships, so there is nothing to replace it with"
            );
            failed += 1;
            continue;
        };

        let is_hook = path.starts_with(crate::tracking::hooks_dir_in(&repo.checkout));

        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
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
            println!("  would   {shown} (whole file, discarding {discarded} line(s))");
            continue;
        }

        if !on_disk.is_empty() {
            let backup = path.with_extension(format!(
                "{}.bak",
                path.extension().and_then(|e| e.to_str()).unwrap_or("")
            ));
            write_atomic(&backup, &on_disk)?;
            println!(
                "  ! your version is saved to {}",
                crate::platform::relative(&repo.checkout, &backup)
            );
        }
        write_atomic(&path, &shipped)?;
        if is_hook {
            // `write_atomic` has no opinion about permissions, so a hook
            // replaced here would land back at the writer's default mode —
            // not executable — and the next hook call would exit 126.
            // `init` ships hooks at `0755`; match that here too.
            repair_hook_mode(&path)?;
        }
        println!("  wrote   {shown} (whole file, discarding your changes)");
    }

    println!();
    if failed > 0 {
        anyhow::bail!(
            "{failed} of {} path(s) could not be replaced",
            args.replace.len()
        );
    }
    match args.dry_run {
        true => println!("{DRY_RUN}"),
        false => println!("`git diff` shows exactly what changed."),
    }
    Ok(())
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

    // The two ticket-body templates `init` seeds into
    // `.spoolway/templates/tracking/` — never touched by an ordinary sync,
    // exactly like a prompt or a task skeleton, but reachable by name here
    // the same way both of those already are.
    if path.starts_with(repo.tracking_templates_dir()) {
        return crate::assets::tracking_template(stem).map(str::to_string);
    }

    // The hook scripts `init` seeds into `.spoolway/hooks/` — same rule:
    // never touched by an ordinary sync, `--replace` only.
    if path.starts_with(crate::tracking::hooks_dir_in(&repo.checkout)) {
        return crate::assets::HOOK_SCRIPTS
            .iter()
            .find(|(known, _)| Path::new(known).file_stem().and_then(|s| s.to_str()) == Some(stem))
            .map(|(_, body)| body.to_string());
    }

    // The ignore rules are deliberately not here. They are a block in a file the
    // project owns the rest of, so there is no whole-file version of it to write:
    // `ignores` above refreshes the block, on every run, and that is the only way
    // back to ours.

    for skeleton in crate::skeleton::skeletons() {
        if *path == repo.checkout.join(skeleton.path) {
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
/// The one place this decision is made. [`text_fingerprint`] calls this too,
/// so the fingerprint is taken over exactly the skill files [`skills`] would
/// itself write for this checkout — not a looser stand-in that could disagree
/// with it about which providers even count as installed.
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
/// checks its own marker instead; see that function's doc.
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
/// Unlike [`installed_at`], a planned file on disk proves nothing here: no
/// release before #576 ever installed skills at user level, so a folder
/// there belongs to the person who made it unless
/// [`crate::install::USER_INSTALL_MARKER`] says `install_user` wrote it.
/// Checking the marker alone, rather than inferring installation from what
/// is on disk the way a project folder's own check does, is what keeps
/// [`skills`] from writing spoolway's set into a folder nobody asked for it
/// in. [`retired_skills`] does not call this at all, marker or not: both
/// retired names predate #576, so a retired-name directory at user level
/// can never be spoolway's own rename leftover — see that function's own
/// comment.
///
/// Read by [`skills`] and [`text_fingerprint`] alike, so the two can never
/// disagree about which user folders count.
fn user_skills() -> Vec<(PathBuf, Vec<crate::install::Planned>)> {
    let Some(home) = crate::install::user_home() else {
        return Vec::new();
    };
    <crate::cli::Provider as clap::ValueEnum>::value_variants()
        .iter()
        .map(|provider| (provider.user_skills_dir(&home), provider.plan_user(&home)))
        .filter(|(dir, _)| dir.join(crate::install::USER_INSTALL_MARKER).is_file())
        .collect()
}

/// Rewrite each of `planned` that differs from the shipped copy, or is
/// missing, naming it as `shown` gives it — the half of [`skills`] a
/// project folder and a user folder share.
fn refresh(
    planned: Vec<crate::install::Planned>,
    shown: impl Fn(&Path) -> String,
    args: &SyncArgs,
    outcomes: &mut Vec<Outcome>,
) -> Result<()> {
    for planned in planned {
        let detail = match std::fs::read_to_string(&planned.path) {
            Ok(on_disk) if on_disk == planned.contents => {
                outcomes.push(Outcome::Kept);
                continue;
            }
            Ok(_) => "rewritten",
            Err(_) => "added",
        };
        if !args.dry_run {
            write_atomic(&planned.path, planned.contents)?;
        }
        outcomes.push(Outcome::wrote(shown(&planned.path), detail));
    }
    Ok(())
}

/// Skills are rewritten to the shipped copy wherever a project installed
/// them: a skill file belongs to spoolway outright, the same bargain
/// `config.toml` keeps for everything around a project's own values, so
/// there is no hand edit to protect here — nothing but the shipped copy was
/// ever meant to be there. Writing them into a project that never ran
/// `install` would still be this command choosing an agent on someone's
/// behalf, so that guard stays.
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
/// user-level loop below is unaffected: it already writes outside the
/// checkout, in the agent's own folder, which is exactly where a home-mode
/// project keeps its skills.
fn skills(
    repo: &Repo,
    args: &SyncArgs,
    home_mode: bool,
    outcomes: &mut Vec<Outcome>,
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
            args,
            outcomes,
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
                if !args.dry_run {
                    std::fs::remove_dir_all(&stale)
                        .with_context(|| format!("removing {}", stale.display()))?;
                }
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
            if !args.dry_run {
                let _ = std::fs::remove_dir(&old_codex_root);
                let _ = std::fs::remove_dir(repo.checkout.join(".codex"));
            }
        }
    }
    // The user-level copies too, the same way: a home-mode project keeps its
    // skills only there, and a person who ran `spoolway install --user` from
    // a repo-mode one would otherwise keep whichever release they last
    // installed. Named with `~`, since they sit outside the checkout.
    for (_, planned) in user_skills() {
        refresh(planned, crate::repo::shorten_home, args, outcomes)?;
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
    args: &SyncArgs,
    home_mode: bool,
    outcomes: &mut Vec<Outcome>,
) -> Result<()> {
    if home_mode {
        return Ok(());
    }
    // Project folders only. Both retired names predate #576 — the release
    // that first wrote anything under a user folder at all — so a
    // retired-name directory there can never be a rename this binary left
    // behind; it is always a person's own, marker or not, and must never be
    // removed.
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
            if !args.dry_run {
                std::fs::remove_dir_all(&stale)
                    .with_context(|| format!("removing {}", stale.display()))?;
            }
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
fn retired_templates(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
    for (name, why) in crate::install::RETIRED_TEMPLATES {
        let path = crate::config::under_setup(&repo.setup_dir(), name);
        if !path.is_file() {
            continue;
        }
        let shown = crate::platform::relative(&repo.checkout, &path);
        if !args.dry_run {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        outcomes.push(Outcome::removed(&shown, *why));
    }
    Ok(())
}

/// The key reference in a pipeline file: the fenced block, and nothing else.
///
/// A pipeline is the project's flow, written in the project's words, so almost
/// none of the file is ours. The exception is the table of keys in its header:
/// that is documentation of *this binary's* contract, and a copy of it written
/// by an older release is a copy that is now wrong. So the fence is refreshed
/// wherever it is found and every line around it — the title, the steps, the
/// notes between them — is copied through unread.
///
/// Two departures from the module doc's promise, specific to a pipeline
/// file — [`skills`] departs from the same promise too, in its own way; see
/// the module doc's own paragraph on it. Both below are deliberate:
///
/// - An edit inside the markers is discarded, not refused. This is
///   `config.toml`'s bargain, not a skeleton's: there is nothing in here for a
///   project to have meant, because every sentence is a claim about what the
///   binary does. A hand-edited one is a claim that has stopped being true.
/// - A file with no markers is left completely alone and not reported. Writing
///   a block into a pipeline somebody wrote themselves would be this command
///   helping, which is the one thing it must never do. Pasting the two markers
///   in is how a pipeline opts in.
fn pipelines(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
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
        let on_disk = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                outcomes.push(Outcome::blocked(
                    &shown,
                    format!("could not be read ({err})"),
                ));
                continue;
            }
        };

        let region = crate::pipeline::KEY_BLOCK;
        if region.read(&on_disk).is_none() {
            // Half a fence is the one shape worth saying something about: the
            // lines under a start marker with no end could be anyone's, so
            // nothing is written and the missing marker is named. No markers
            // at all means the file never opted in, and the retired shapes
            // below are left alone right along with the key reference — see
            // this function's own doc on why an unfenced file is never ours
            // to touch.
            let opened = on_disk
                .lines()
                .any(|line| line.trim() == crate::assets::PIPELINE_KEYS_BEGIN);
            outcomes.push(match opened {
                true => Outcome::blocked(
                    &shown,
                    format!(
                        "spoolway's key reference starts with `{}` and never ends — restore the \
                         `{}` marker, or delete the block and run this again",
                        crate::assets::PIPELINE_KEYS_BEGIN,
                        crate::assets::PIPELINE_KEYS_END
                    ),
                ),
                false => Outcome::Kept,
            });
            continue;
        }

        let mut on_disk = on_disk;
        let mut changed = false;
        let found = region
            .read(&on_disk)
            .expect("the fence was just confirmed above");
        if crate::skeleton::same(found, crate::pipeline::key_block()) {
            outcomes.push(Outcome::Kept);
        } else {
            match region.replace(&on_disk, crate::pipeline::key_block()) {
                Some(next) => {
                    on_disk = next;
                    changed = true;
                    outcomes.push(Outcome::wrote(&shown, "key reference refreshed"));
                }
                None => {
                    outcomes.push(Outcome::blocked(&shown, "its block moved while we read it"));
                }
            }
        }

        if changed && !args.dry_run {
            write_atomic(&path, &on_disk)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The stamp: what a checkout was last brought to. Written here, by `sync` on
// success, and by `commands::init` once it has finished placing a project's
// files — a fresh project is, by definition, exactly what this binary would
// write, so `init` records the same fact `sync` would have recorded had it
// run instead of `init` doing the writing itself.
// ---------------------------------------------------------------------------

/// The stamp file's name, under [`Repo::home`] — never the checkout: a home is
/// shared by the main checkout and every linked worktree cut from it, and
/// each keeps its own line here rather than fighting over one shared value.
pub const STAMP_FILE: &str = "sync-stamp";

/// Where the stamp lives, given a project's home directory.
pub fn stamp_path(home: &Path) -> PathBuf {
    home.join(STAMP_FILE)
}

/// Record that `checkout` now stands at `version`/`fingerprint` — one line,
/// `<version> <fingerprint> <checkout path>`, replacing any earlier line for
/// the same checkout and leaving every other checkout's own line untouched.
fn write_stamp_line(home: &Path, checkout: &Path, version: &str, fingerprint: &str) -> Result<()> {
    let path = stamp_path(home);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let shown = checkout.display().to_string();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|line| line.splitn(3, ' ').nth(2) != Some(shown.as_str()))
        .map(str::to_string)
        .collect();
    lines.push(format!("{version} {fingerprint} {shown}"));
    lines.sort();
    let mut body = lines.join("\n");
    body.push('\n');
    write_atomic(&path, body)
}

/// What the stamp says for `checkout`, if anything — `(version, fingerprint)`.
///
/// [`stamp_behind`] is the one caller outside this module's own tests:
/// comparing what this reads back against what this binary would write now
/// is exactly what says a checkout is behind.
pub fn read_stamp(home: &Path, checkout: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(stamp_path(home)).ok()?;
    let shown = checkout.display().to_string();
    text.lines().find_map(|line| {
        let mut parts = line.splitn(3, ' ');
        let version = parts.next()?;
        let fingerprint = parts.next()?;
        let path = parts.next()?;
        (path == shown).then(|| (version.to_string(), fingerprint.to_string()))
    })
}

/// The text this binary would write at `checkout` right now, fingerprinted —
/// the rendered config (the project's own values, this binary's comments and
/// key list), the pipeline key reference every tracked pipeline carries, and
/// the current skill set for every provider actually installed here. This is
/// what [`write_stamp`] records: a project reading its own stamp back can
/// tell, in two string comparisons, whether this same binary would still
/// write the same thing — the version and this fingerprint both settled
/// exactly once, at the moment they were last brought current.
fn text_fingerprint(checkout: &Path) -> String {
    let mut material = String::new();
    // Best-effort, like `version::layer_fingerprint`'s own read of a layer: a
    // config that will not load or render at all is not a reason to fail the
    // whole stamp — unlike `sync`'s own `config()`, which now fails the
    // whole scan on exactly this (see `config`'s own doc) — so the
    // fingerprint it produces here is taken over the pipeline key block and
    // installed skills alone, which is still a real answer for whether
    // *those* have drifted.
    if let Ok(config) = crate::config::Config::load(checkout)
        && let Ok(rendered) = config.render()
    {
        material.push_str(&rendered);
    }
    material.push_str(crate::pipeline::key_block());
    for provider in <crate::cli::Provider as clap::ValueEnum>::value_variants() {
        let planned = provider.plan(checkout);
        if !provider_installed(*provider, checkout, &planned) {
            continue;
        }
        for planned in planned {
            material.push_str(planned.contents);
        }
    }
    // The user-level copies [`skills`] also rewrites, so a user folder
    // installed since the last sync reads as something to bring current.
    for (_, planned) in user_skills() {
        for planned in planned {
            material.push_str(planned.contents);
        }
    }
    crate::skeleton::fingerprint(&material)
}

/// Write the stamp for `checkout`, under `home` — [`run`]'s own call on
/// success, and `commands::init`'s once a fresh project's files are down.
pub fn write_stamp(home: &Path, checkout: &Path) -> Result<()> {
    let fingerprint = text_fingerprint(checkout);
    write_stamp_line(home, checkout, crate::release::current(), &fingerprint)
}

/// Whether `checkout`'s stamp — what `sync` or `init` last recorded there —
/// no longer matches what this binary would write now: a newer release, or
/// a config whose own values have changed since. `crate::gate`'s notice
/// reads this before paying for a full [`scan`], so an up-to-date project
/// pays nothing beyond one file read and a few hashes per command.
///
/// No stamp at all reads as "not behind": a project this stamp predates, or
/// a fixture that never ran `init` or `sync`, has nothing recorded to
/// compare against, and guessing behind would nag a project this stamp has
/// simply never reached yet.
pub fn stamp_behind(home: &Path, checkout: &Path) -> bool {
    match read_stamp(home, checkout) {
        Some((version, fingerprint)) => {
            version != crate::release::current() || fingerprint != text_fingerprint(checkout)
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// The skill stamp is gone: a skill file belongs to spoolway outright now, so
// there is no hand edit left to tell from a stale shipped copy, and nothing
// here reads or writes one any more. [`remove_skill_stamp`] is the one thing
// left to do with the name — clear out the file an upgraded project may
// still be carrying from before this change.
// ---------------------------------------------------------------------------

/// The skill stamp's old file name, under [`Repo::home`] — kept only so a
/// leftover one can be found and removed.
const SKILL_STAMP_FILE: &str = "skill-stamp";

/// Delete a project's leftover skill stamp, if one is still there from
/// before skill files became spoolway's outright. Best-effort, the same way
/// [`write_stamp`] tolerates a home it cannot resolve: a stamp nobody reads
/// any more is clutter, not a fact worth failing a sync over.
pub(crate) fn remove_skill_stamp(home: &Path) {
    let _ = std::fs::remove_file(home.join(SKILL_STAMP_FILE));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn fixture(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let root = crate::scratch::root(&format!("sync-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let config = Config::default();
        std::fs::create_dir_all(root.join(crate::config::TASK_TEMPLATES_DIR)).unwrap();
        std::fs::create_dir_all(root.join(".spoolway/templates")).unwrap();
        let home = root.join(".home");
        (
            Repo {
                checkout: root.to_path_buf(),
                root: root.to_path_buf(),
                config,
                home,
            },
            root,
        )
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
    fn ask(
        repo: &Repo,
        args: &SyncArgs,
        json: bool,
        in_lane: bool,
        tty: bool,
        keys: &str,
    ) -> String {
        let mut input = std::io::Cursor::new(keys.as_bytes().to_vec());
        let mut out = Vec::new();
        run_asking_with(
            repo,
            args,
            json,
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
        let (repo, _root_guard) = fixture("home-mode");
        let home = repo.root.join(".ws-home");
        let _ = std::fs::remove_dir_all(&home);
        let workspace = home.join(".spoolway").join("home-mode-ws");
        std::fs::create_dir_all(workspace.join("config")).unwrap();
        std::fs::write(
            workspace.join(crate::repo::BINDING_FILE),
            format!(
                "id = \"home-mode-ws\"\nclones = [{{ root = {:?}, dispatcher = \"api\" }}]\n",
                repo.root.display(),
            ),
        )
        .unwrap();

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
            run(&repo, &args(), false).unwrap();
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

    /// The confirm panel's width is bounded even when a path in it is not:
    /// every row `screen::boxed` draws — borders included — stays at or
    /// under 80 columns.
    #[test]
    fn the_confirm_panel_is_at_most_eighty_columns_wide() {
        let long = "a/very/long/path/".repeat(6) + "SKILL.md";
        let body = panel_body(&[long.as_str()], &BTreeMap::new(), &[]);
        for line in crate::screen::panel(TITLE, &body, "[enter] apply") {
            assert!(
                line.chars().count() <= 80,
                "{} columns: {line}",
                line.chars().count()
            );
        }
    }

    /// Enter at the panel writes the files, records the stamp and draws
    /// nothing more than the panel itself; a stray key before it is ignored.
    #[test]
    fn asking_sync_writes_only_once_enter_is_pressed() {
        let (repo, _root_guard) = fixture("ask-enter");
        let drawn = ask(&repo, &args(), false, false, true, "q\r");
        assert!(drawn.contains(TITLE), "{drawn}");
        assert!(drawn.contains("config.toml"), "{drawn}");
        assert!(drawn.contains("[enter] apply"), "{drawn}");
        assert!(drawn.contains("[esc] cancel"), "{drawn}");
        assert!(!drawn.contains(CANCELLED), "{drawn}");
        assert!(Config::path_in(&repo.checkout).is_file());
        assert!(stamp_path(&repo.home).is_file());
    }

    /// Esc, and ctrl-c — which reaches `read_key` as a read that failed,
    /// the same `None` as input running out — write nothing, leave the
    /// stamp alone and say so.
    #[test]
    fn asking_sync_writes_nothing_on_esc_or_ctrl_c() {
        for (name, keys) in [("ask-esc", "\x1b"), ("ask-ctrl-c", "")] {
            let (repo, _root_guard) = fixture(name);
            let drawn = ask(&repo, &args(), false, false, true, keys);
            assert!(drawn.contains(TITLE), "{name}: {drawn}");
            assert!(
                drawn.ends_with(&format!("{CANCELLED}\n")),
                "{name}: {drawn}"
            );
            assert!(!Config::path_in(&repo.checkout).exists(), "{name}");
            assert!(!stamp_path(&repo.home).exists(), "{name}");
        }
    }

    /// No terminal, `--json` or a lane: written straight away with no panel
    /// and no key read — `keys` is empty, so a read would cancel and leave
    /// the config unwritten rather than pass by accident.
    #[test]
    fn asking_sync_writes_straight_away_with_nobody_to_answer() {
        for (name, json, in_lane, tty) in [
            ("ask-no-tty", false, false, false),
            ("ask-json", true, false, true),
            ("ask-lane", false, true, true),
        ] {
            let (repo, _root_guard) = fixture(name);
            let drawn = ask(&repo, &args(), json, in_lane, tty, "");
            assert!(drawn.is_empty(), "{name}: {drawn}");
            assert!(Config::path_in(&repo.checkout).is_file(), "{name}");
            assert!(stamp_path(&repo.home).is_file(), "{name}");
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
        assert!(ask(&repo, &dry, false, false, true, "").is_empty());
        assert!(!Config::path_in(&repo.checkout).exists());

        run(&repo, &args(), false).unwrap();
        assert!(ask(&repo, &args(), false, false, true, "").is_empty());
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
        let mut written: Vec<&str> = Vec::new();
        for outcome in &outcomes {
            if let Outcome::Wrote { path, .. } = outcome
                && !written.contains(&path.as_str())
            {
                written.push(path);
            }
        }

        assert!(written.contains(&".spoolway/config.toml"), "{written:?}");
        // One rewrite can add a setting and drop a retired one at once. Two
        // records, one file, and the report says the file once.
        assert_eq!(
            written.len(),
            written
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            "a path printed twice reads as two files: {written:?}"
        );
        assert!(KEPT.contains("config values"), "{KEPT}");
        assert!(KEPT.contains("prompts"), "{KEPT}");
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
                .any(|line| line.starts_with("wrote") && line.contains("auto_commit")),
            "{:?}",
            outcome_lines(&outcomes)
        );
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

    /// `dispatch.interval` is retired hard enough that an ordinary load
    /// refuses a file still naming it — the same refusal `spoolway config
    /// get` or `dispatch` would hit on this file today. `sync` is the one
    /// path that has to bring it forward instead: it drops the key, rewrites
    /// the reference header around its removal, and leaves the rest of the
    /// file exactly as it read it.
    #[test]
    fn a_config_still_naming_dispatch_interval_is_refused_before_sync_and_accepted_after() {
        let (repo, _root_guard) = fixture("config-retired-interval");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(
            &path,
            "[dispatch]\nbackend = \"headless\"\ninterval = \"45s\"\n",
        )
        .unwrap();

        assert!(
            crate::config::Config::load(&repo.root).is_err(),
            "an ordinary load must still refuse the retired key"
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
                .any(|line| line.starts_with("wrote") && line.contains("dispatch.interval")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// `issue_tracking.on_fail` is the other key retired hard enough that an
    /// ordinary load refuses a file still naming it. Unlike `dispatch.
    /// interval` this retirement also moves the table it lived in — from
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
            crate::config::Config::load(&repo.root).is_err(),
            "an ordinary load must still refuse the retired key"
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
                .any(|line| line.starts_with("wrote") && line.contains("issue_tracking.on_fail")),
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
                .any(|line| line.starts_with("wrote") && line.contains("dispatch.worktree_root")),
            "{lines:?}"
        );

        let notes = migration_notes(&outcomes);
        let shown = notes.values().flatten().collect::<Vec<_>>();
        assert!(
            shown.iter().any(|(report, panel)| {
                report.contains("dispatch.worktree_root removed")
                    && report.contains("/old/worktrees")
                    && panel.contains("/old/worktrees")
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
                .any(|line| line.starts_with("wrote") && line.contains("dispatch.worktree_root")),
            "{lines:?}"
        );
        assert!(
            migration_notes(&outcomes).is_empty(),
            "a blank value must not earn the dedicated directory note"
        );
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
        std::fs::write(&first.path, "stale, from an older release\n").unwrap();

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

        run(&repo, &args(), false).unwrap();
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
                 - id: work\n    end: true\n"
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
        let stale = format!(
            "{}\n# Top level\n#   template    What this used to say.\n{}",
            crate::assets::PIPELINE_KEYS_BEGIN,
            crate::assets::PIPELINE_KEYS_END
        );
        let path = pipeline_file(&repo, "hotfix", &stale);

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(after.contains(crate::pipeline::key_block()), "{after}");
        assert!(!after.contains("What this used to say"));
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

    /// The departure from every other file here: an edit *inside* the fence is
    /// discarded rather than refused. There is nothing in there for a project
    /// to have meant — every line is a claim about what this binary does.
    #[test]
    fn an_edit_inside_the_markers_is_discarded_not_refused() {
        let (repo, _root_guard) = fixture("pipeline-edited");
        let edited =
            crate::pipeline::key_block().replace("Absent: 30m.", "Absent: however long you like.");
        let path = pipeline_file(&repo, "default", &edited);

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(!after.contains("however long you like"));
        assert!(after.contains("Absent: 30m."));
        assert!(
            !outcome_lines(&outcomes)
                .iter()
                .any(|line| line.starts_with("blocked")),
            "a differing key block is rewritten, not reported: {:?}",
            outcome_lines(&outcomes)
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
        let mine = "# mine, whole.\nsteps:\n  - id: work\n    end: true\n";
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

    /// A dry run reads and says, on this file as on every other.
    #[test]
    fn a_dry_run_leaves_a_stale_key_reference_where_it_is() {
        let (repo, _root_guard) = fixture("pipeline-dry");
        let stale = format!(
            "{}\n# Top level\n{}",
            crate::assets::PIPELINE_KEYS_BEGIN,
            crate::assets::PIPELINE_KEYS_END
        );
        let path = pipeline_file(&repo, "hotfix", &stale);
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
                repo.tracking_templates_dir().join("ticket.md"),
                "${SPOOLWAY_TASK}",
            ),
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
        // A real `install --user`, which is what actually leaves the marker
        // `user_skills` looks for — not a hand-written stand-in for it.
        crate::platform::test_home::with_home(&home, || {
            crate::install::install_user(crate::cli::Provider::Claude, false).unwrap();
        });
        std::fs::remove_file(&missing.path).unwrap();
        std::fs::write(&stale.path, "an older copy").unwrap();

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

    /// The marker alone must not be read as blanket permission to delete
    /// anything under a user folder: a real `install --user` ran here, so
    /// the folder is spoolway's to refresh, but the retired-name directory
    /// sitting beside the installed skills still predates #576 and is still
    /// a person's own. Before this fix, `retired_skills` chained every
    /// marked user folder into its removal loop and deleted it anyway.
    #[test]
    fn a_marked_user_folder_still_keeps_a_hand_made_retired_name_directory() {
        let (repo, _root_guard) = fixture("skills-user-level");
        let home = crate::scratch::root("sync-user-home-marked-and-owned");
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
             from a user folder, marker or not"
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
            std::fs::write(&first.path, "stale, from an older release\n").unwrap();
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

    /// A skill file belongs to spoolway outright now, unlike a config value
    /// or a skeleton's own styling — so a hand edit to one is not protected
    /// the way [`BlockState::HandEdited`] protects a project's own prose.
    /// Sync writes the shipped copy over it and says so, the same as any
    /// other stale file.
    #[test]
    fn a_hand_edited_skill_file_is_overwritten_and_named() {
        let (repo, _root_guard) = fixture("skills-hand-edited");
        let planned = crate::cli::Provider::Claude.plan(&repo.root);
        let first = planned.into_iter().next().unwrap();
        std::fs::create_dir_all(first.path.parent().unwrap()).unwrap();
        let edited = format!("{}\na line a person added by hand\n", first.contents);
        std::fs::write(&first.path, &edited).unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), false, &mut outcomes).unwrap();
        let lines = outcome_lines(&outcomes);

        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            first.contents,
            "a skill file is spoolway's outright, so a hand edit does not survive a sync: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("wrote") && l.contains("rewritten")),
            "{lines:?}"
        );
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
        std::fs::write(&first.path, "0.5.0's text, no fingerprint ever recorded\n").unwrap();

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

    /// All three dead templates go, each with its own reason, and a project's
    /// own file under `.spoolway/templates/` — named by neither
    /// [`crate::install::RETIRED_TEMPLATES`] nor a shape `init` still
    /// places — is left exactly where it was.
    #[test]
    fn sync_removes_every_retired_template_and_leaves_a_projects_own_file() {
        let (repo, _root_guard) = fixture("retired-templates");
        let dir = repo.checkout.join(".spoolway/templates");
        let task_log = dir.join("task-log.md");
        let pull_request = dir.join("pull-request.md");
        let lane_prompts = dir.join("lane-prompts.md");
        std::fs::write(&task_log, "stale\n").unwrap();
        std::fs::write(&pull_request, "stale\n").unwrap();
        std::fs::write(&lane_prompts, "stale\n").unwrap();
        let untouched = dir.join("a-projects-own-notes.md");
        std::fs::write(&untouched, "mine\n").unwrap();

        let mut outcomes = Vec::new();
        retired_templates(&repo, &args(), &mut outcomes).unwrap();

        assert!(!task_log.exists(), "task-log.md must be removed");
        assert!(!pull_request.exists(), "pull-request.md must be removed");
        assert!(!lane_prompts.exists(), "lane-prompts.md must be removed");
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

    /// The stamp: written on a real run, re-readable straight back, and
    /// keeping a sibling checkout's own line untouched — the "one line per
    /// checkout" the mockup shows for a project sharing its home between a
    /// main checkout and a linked worktree.
    #[test]
    fn sync_writes_a_stamp_that_reads_back_and_keeps_a_siblings_line() {
        let (repo, _root_guard) = fixture("stamp-roundtrip");
        let other = repo.root.join("other-checkout");

        write_stamp(&repo.home, &other).unwrap();
        run(&repo, &args(), false).unwrap();

        let (version, fingerprint) = read_stamp(&repo.home, &repo.checkout)
            .expect("sync on success writes a stamp for this checkout");
        assert_eq!(version, crate::release::current());
        assert!(!fingerprint.is_empty());

        // The sibling's own line is still there, untouched.
        assert!(read_stamp(&repo.home, &other).is_some());
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

        run(&repo, &args(), false).unwrap();

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
            false,
        )
        .unwrap();

        assert!(stamp.exists(), "a dry run must not remove the old stamp");
    }

    /// `text_fingerprint` has to agree with `skills()` about which providers
    /// count as installed — a `.claude/skills/` holding only a project's own
    /// skill, with none of spoolway's own planned files anywhere under it,
    /// is not an install, so the fingerprint must not change when one shows
    /// up there. `provider_installed` is what both now share; this pins the
    /// behaviour rather than the helper, so a future split of the two rules
    /// would be caught here.
    #[test]
    fn the_fingerprint_ignores_a_directory_that_holds_no_installed_skill() {
        let checkout = crate::scratch::root("sync-fingerprint-bystander-dir");
        let _ = std::fs::remove_dir_all(&checkout);
        std::fs::create_dir_all(&checkout).unwrap();
        let before = text_fingerprint(&checkout);

        let bystander = crate::cli::Provider::Claude
            .skills_dir(&checkout)
            .join("a-projects-own-skill");
        std::fs::create_dir_all(&bystander).unwrap();
        std::fs::write(bystander.join("SKILL.md"), "not spoolway's\n").unwrap();
        assert_eq!(
            text_fingerprint(&checkout),
            before,
            "a directory with no installed skill in it must not count as an install"
        );

        // A real install does change it.
        let planned = crate::cli::Provider::Claude
            .plan(&checkout)
            .into_iter()
            .next()
            .unwrap();
        std::fs::create_dir_all(planned.path.parent().unwrap()).unwrap();
        std::fs::write(&planned.path, planned.contents).unwrap();
        assert_ne!(
            text_fingerprint(&checkout),
            before,
            "an actually installed provider must change the fingerprint"
        );
    }

    /// A dry run brings nothing forward, so it must not claim a checkout was
    /// synced either.
    #[test]
    fn a_dry_run_never_writes_the_stamp() {
        let (repo, _root_guard) = fixture("stamp-dry-run");
        run(
            &repo,
            &SyncArgs {
                dry_run: true,
                ..args()
            },
            false,
        )
        .unwrap();
        assert!(read_stamp(&repo.home, &repo.checkout).is_none());
    }
}
