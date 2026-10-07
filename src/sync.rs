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
    /// A note about a file this run changed, spelled out in the report: what
    /// dropping a retired setting means for the project — see [`config`]'s own
    /// `issue_tracking.on_fail` and `dispatch.worktree_root` notes — a value
    /// `sync` set that the file never named, or a pipeline key block it
    /// replaced. Its own
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
    /// Ours, and changed by hand. The only outcome that is a refusal, and the
    /// one that keeps a sync from stamping the project current.
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

/// What a sync says when it refused a file and wrote nothing else.
const REFUSED_ONLY: &str = "Nothing else updating; the refused files above are still behind.";

/// Which of the four closing lines above a run prints, kept as its own
/// pure function so a test can hold it to its text without capturing
/// stdout: `nothing_to_do` wins over dry run, since neither `DRY_RUN` nor
/// `KEPT` is true of a run that touched nothing, and a refusal beats
/// `NOOP`, since "Nothing updating." under a refusal would read as the
/// project being current.
fn closing_line(dry_run: bool, nothing_to_do: bool, refused: bool) -> &'static str {
    match (dry_run, nothing_to_do, refused) {
        (_, true, true) => REFUSED_ONLY,
        (_, true, false) => NOOP,
        (true, false, _) => DRY_RUN,
        (false, false, _) => KEPT,
    }
}

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

    println!();
    let nothing_else = wrote.is_empty() && removed.is_empty();
    println!(
        "{}",
        closing_line(args.dry_run, nothing_else, !refused.is_empty())
    );

    // Only once the write has actually happened: the stamp records what a
    // checkout was last brought to, and a dry run brings it to nothing.
    // A refused file is one this run did not bring forward, so the project is
    // not current: the checkout's stamp line is dropped, not just left
    // unwritten, or an earlier "current" stamp would keep the notice off.
    if !args.dry_run {
        match refused.is_empty() {
            true => write_stamp(&repo.home, &repo.checkout)?,
            false => forget_stamp(&repo.home, &repo.checkout)?,
        }
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

/// Every refused file in a scan, once each, with the reason: what [`run`]'s
/// report prints before anything else and what keeps the run from stamping
/// the project current. Kept apart from [`dedup_paths`], whose two lists are
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

/// Every file spoolway owns here, and what would happen to it.
///
/// Split out from [`run`] so that `spoolway doctor` and the "Run spoolway
/// sync" notice can ask the same question without printing anything.
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
    // `templates` does the same for a skeleton spelled under `.spoolway/`
    // (see `skeleton_path`) and skips one spelled anywhere else, which
    // would land in the checkout. `ignores` and the project-level half of
    // `skills` join `repo.checkout` directly instead, with no such
    // indirection, so they are the two steps that need telling.
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
    // `load_dropping_retired_keys` rather than `Config::load`: both strip
    // `dispatch.interval` and `issue_tracking.on_fail` the same way now, but
    // `Config::load` also merges the private override layer onto the
    // result, and that merged value must never be what gets rendered and
    // written back here — see this function's own doc.
    // A config this binary cannot even parse, same as the read failure
    // above: this fails loudly rather than recording a quiet
    // `Outcome::Blocked` a plain `sync` or `sync --dry-run` never prints.
    // `load_dropping_retired_keys`'s own error already names the path —
    // `strip_hard_retired_keys` only touches the raw text through
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
    // Computed here regardless of `args.dry_run` — a dry run has to say what
    // it would drop, the same as every other outcome this function reports —
    // but written to disk only when `!args.dry_run`, the same guard the
    // tracked file's own write gets below. Unconditional before this fix,
    // `spoolway sync --dry-run` rewrote the private layer and then printed
    // "Dry run: nothing was written" over it.
    if let Ok(overrides_dir) = crate::overrides::dir_for(&repo.checkout) {
        let dropped = crate::overrides::retired_config_patch_keys(&overrides_dir, &current)?;
        if !dropped.is_empty() {
            // Read before the write below removes it. A `worktree_root` set
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
            if !args.dry_run {
                crate::overrides::write_dropped_config_patch_keys(&overrides_dir, &dropped)?;
            }
            let shown_override = crate::platform::relative(
                &repo.checkout,
                &crate::overrides::config_patch_path(&overrides_dir),
            );
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
    templates_of(repo, &crate::skeleton::skeletons(), args, outcomes)
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
    args: &SyncArgs,
    outcomes: &mut Vec<Outcome>,
) -> Result<()> {
    for skeleton in skeletons {
        let Some(path) = skeleton_path(repo, skeleton.path) else {
            continue;
        };
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
                write_block(&path, &shown, &on_disk, skeleton, args, outcomes, "block")?;
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
    let home_mode = crate::repo::workspace_clone(&repo.root).is_some();
    for name in &args.replace {
        // A path spelled under `.spoolway/` names the project's setup folder,
        // which in home mode is the workspace's `config/` and not a folder
        // of the checkout. Anything else relative joins the checkout.
        let path = if Path::new(name).is_absolute() {
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
            true => println!("  wrote   {shown}\n          (whole file, discarding your changes)"),
            false => println!("  wrote   {shown} (whole file, discarding your changes)"),
        }
    }

    println!();
    if failed > 0 {
        anyhow::bail!(
            "{failed} of {} path(s) could not be replaced",
            args.replace.len()
        );
    }
    if let Some(line) = replace_footer(repo, args.dry_run) {
        println!("{line}");
    }
    Ok(())
}

/// How `--replace` names `path`: relative to the checkout, as ever, except
/// that in home mode a file in the workspace's `config/` is named by its `~`
/// form, since the long absolute path is not one the person typed.
fn shown_path(repo: &Repo, path: &Path) -> String {
    let in_workspace =
        crate::repo::workspace_clone(&repo.root).is_some() && path.starts_with(repo.setup_dir());
    match in_workspace {
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
        .filter(|(_, planned)| planned.iter().any(|file| file.path.exists()))
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
/// - A file with no markers is never written, and is reported only by the
///   check below. Writing a block into a pipeline somebody wrote themselves
///   would be this command helping, which is the one thing it must never do.
///   Pasting the two markers in is how a pipeline opts in.
///
/// One check reads every file, markers or not: a step shape this release
/// refuses to load — see [`crate::pipeline::Pipeline::retired_shape_problems`]
/// — refuses the file, naming the step and the edit. Nothing is written to it.
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

        // A shape this release stopped loading — `loop: 0` is the one an
        // earlier release still ran — is refused by name, fenced or not, and
        // the file is left as it is. `sync` never edits a step, but without
        // this it would report the upgrade done and stamp the project current
        // over a pipeline the next command refuses to load.
        if let Ok(pipeline) = serde_norway::from_str::<crate::pipeline::Pipeline>(&on_disk) {
            let problems = pipeline.retired_shape_problems();
            if !problems.is_empty() {
                outcomes.push(Outcome::blocked(
                    &shown,
                    format!("will not load: {}", problems.join("; ")),
                ));
                continue;
            }
        }

        let region = crate::pipeline::KEY_BLOCK;
        if region.read(&on_disk).is_none() {
            // Half a fence is the one shape worth saying something about: the
            // lines under a start marker with no end could be anyone's, so
            // nothing is written and the missing marker is named. No markers
            // at all means the file never opted in, so it is kept as it is —
            // a retired shape in it was already refused above, never rewritten
            // — see this function's own doc on why an unfenced file is never
            // ours to touch.
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
                    // The block belongs to spoolway, so an edit inside it is
                    // gone after this write. Said in the report, because the
                    // line a person added is otherwise lost without a word.
                    // Names the file: the notes print after every file line,
                    // and an upgrade can replace the block in several files.
                    outcomes.push(Outcome::migrated(
                        &shown,
                        format!(
                            "migrated: the block between spoolway's key markers in {shown} was \
                             replaced; a hand edit inside it is not kept"
                        ),
                        format!("migrated: key block in {shown} replaced; edits are not kept"),
                    ));
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
// a run that refused nothing, and by `commands::init` once it has finished
// placing a project's files and kept none of them — a project `init` wrote
// whole is, by definition, exactly what this binary would write, so `init`
// records the same fact `sync` would have recorded had it run instead. A
// file `init` kept is whatever an older version left, which is for `sync`.
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

/// Drop `checkout`'s line from the stamp, leaving every other checkout's own.
///
/// A sync that refused a file calls this rather than skipping its write: an
/// earlier current stamp would otherwise stay and say the project is fine.
fn forget_stamp(home: &Path, checkout: &Path) -> Result<()> {
    let path = stamp_path(home);
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let shown = checkout.display().to_string();
    let kept: Vec<&str> = existing
        .lines()
        .filter(|line| line.splitn(3, ' ').nth(2) != Some(shown.as_str()))
        .collect();
    let mut body = kept.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    write_atomic(&path, body)
}

/// What the stamp says for `checkout`, if anything — `(version, fingerprint)`.
///
/// [`stamp_behind`] compares what this reads back against what this binary
/// would write now, which is exactly what says a checkout is behind.
/// `crate::gate` also asks it whether a stamp exists at all, since a missing
/// one shows the notice without a scan.
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

/// Write the stamp for `checkout`, under `home` — [`run`]'s own call when it
/// refused nothing, and `commands::init`'s once every file it placed was one
/// it wrote itself.
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
/// No stamp, or one that cannot be read, reads as behind: nothing records
/// that this project was ever brought current, and `init` stamps only the
/// files it set up itself, so a project it claimed over an older setup has
/// no stamp until a `sync` has really run.
pub fn stamp_behind(home: &Path, checkout: &Path) -> bool {
    match read_stamp(home, checkout) {
        Some((version, fingerprint)) => {
            version != crate::release::current() || fingerprint != text_fingerprint(checkout)
        }
        None => true,
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
        };
        (repo, root, home)
    }

    /// The workspace's `config/` for a [`home_mode_fixture`] repo.
    fn workspace_config_dir(repo: &Repo, home: &Path) -> PathBuf {
        crate::platform::test_home::with_home(home, || repo.setup_dir())
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
            templates_of(&repo, &skeletons, &args(), &mut outcomes).unwrap();
            assert_eq!(outcomes.len(), 1, "the outside skeleton is skipped");

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
            run(&repo, &args(), false).unwrap();
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
                "# hotfix — mine.\n\n{}\n# Top level\n#   template    What this used to say.\n{}\n\n\
                 steps:\n  - id: work\n    end: true\n",
                crate::assets::PIPELINE_KEYS_BEGIN,
                crate::assets::PIPELINE_KEYS_END
            ),
        )
        .unwrap();

        crate::platform::test_home::with_home(&home, || {
            run(&repo, &args(), false).unwrap();
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
        assert!(!pipeline_after.contains("What this used to say"));
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

    /// A dry run with nothing to do used to say `DRY_RUN` anyway — "Run
    /// without --dry-run to take it" over an empty scan, which took nothing
    /// and had nothing to take. `nothing_to_do` must win over `dry_run`.
    #[test]
    fn a_dry_run_with_nothing_to_do_says_so_instead_of_offering_to_run_it() {
        assert_eq!(closing_line(true, true, false), NOOP);
        assert_eq!(closing_line(false, true, false), NOOP);
        assert_eq!(closing_line(true, false, false), DRY_RUN);
        assert_eq!(closing_line(false, false, false), KEPT);
        // A refusal with nothing else written is not "Nothing updating."
        assert_eq!(closing_line(true, true, true), REFUSED_ONLY);
        assert_eq!(closing_line(false, true, true), REFUSED_ONLY);
        assert_eq!(closing_line(false, false, true), KEPT);
        assert!(!NOOP.contains("--dry-run"), "{NOOP}");
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
                .any(|line| line.starts_with("wrote") && line.contains("dispatch.interval")),
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
                .any(|line| line.starts_with("wrote") && line.contains("dispatch.worktree_root")),
            "{lines:?}"
        );
        // The config never named `key_in_names` either, which earns its own
        // note; only a note about the directory is ruled out here.
        assert!(
            migration_notes(&outcomes)
                .values()
                .flatten()
                .all(|(report, _)| !report.contains("worktree_root")),
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

    /// A `worktree_root` set only in the private override layer is dropped
    /// too, and must be reported the way one dropped from `config.toml` is:
    /// the folder it named, and the queued tasks still under it.
    #[test]
    fn an_override_only_worktree_root_names_the_folder_and_its_queued_tasks() {
        let (repo, _root_guard) = fixture("config-override-worktree-root");
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

    /// A mistake in a private override — a typo, not a key this binary
    /// retired — must never be deleted on the project's behalf: it is left
    /// in the layer, printing "override ignored" until the person who wrote
    /// it fixes the spelling themselves.
    #[test]
    fn a_typo_in_the_override_is_left_in_the_layer_not_deleted() {
        let (repo, _root_guard) = fixture("config-override-typo");
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
        std::fs::write(&mine, "steps:\n  - id: work\n    loop: 0\n    end: true\n").unwrap();

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

    /// A project with no readable stamp has nothing recording that it was
    /// ever brought current, so the "Run spoolway sync" notice keeps showing.
    #[test]
    fn a_missing_or_unreadable_stamp_counts_as_behind() {
        let (repo, _root_guard) = fixture("stamp-missing");
        std::fs::create_dir_all(&repo.home).unwrap();

        assert!(stamp_behind(&repo.home, &repo.checkout), "no stamp at all");

        std::fs::write(stamp_path(&repo.home), "garbage\n").unwrap();
        assert!(stamp_behind(&repo.home, &repo.checkout), "unparsable stamp");

        write_stamp(&repo.home, &repo.checkout).unwrap();
        assert!(!stamp_behind(&repo.home, &repo.checkout), "a real stamp");
    }

    /// A sync that refused a file has not brought the project current, so the
    /// stamp it leaves must still read as behind and the notice keeps showing.
    #[test]
    fn a_sync_that_refused_a_file_leaves_the_project_behind() {
        let (repo, _root_guard) = fixture("stamp-refused");
        write_stamp(&repo.home, &repo.checkout).unwrap();
        assert!(!stamp_behind(&repo.home, &repo.checkout), "starts current");
        let half = format!("{}\n# Top level\n", crate::assets::PIPELINE_KEYS_BEGIN);
        pipeline_file(&repo, "half", &half);

        run(&repo, &args(), false).unwrap();

        assert!(
            stamp_behind(&repo.home, &repo.checkout),
            "a refused pipeline file must leave the project marked as behind"
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

    /// Dropping one checkout's stamp keeps a sibling's line.
    #[test]
    fn forgetting_a_stamp_leaves_a_siblings_line() {
        let (repo, _root_guard) = fixture("stamp-forget");
        let other = repo.checkout.join("other");
        write_stamp(&repo.home, &other).unwrap();
        write_stamp(&repo.home, &repo.checkout).unwrap();

        forget_stamp(&repo.home, &repo.checkout).unwrap();

        assert!(read_stamp(&repo.home, &repo.checkout).is_none());
        assert!(read_stamp(&repo.home, &other).is_some());
    }

    /// Replacing the key block says so in a note of its own, since the
    /// replaced lines are otherwise gone without a word.
    #[test]
    fn a_replaced_key_block_is_named_in_the_report() {
        let (repo, _root_guard) = fixture("pipeline-replaced-note");
        let stale = format!(
            "{}\n# edited by hand\n{}\nname: p\n",
            crate::assets::PIPELINE_KEYS_BEGIN,
            crate::assets::PIPELINE_KEYS_END
        );
        pipeline_file(&repo, "p", &stale);

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();

        let notes = migration_notes(&outcomes);
        let note = notes
            .get(".spoolway/pipelines/p.yml")
            .expect("a note under the replaced file");
        assert!(note[0].0.contains("replaced"), "{note:?}");
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
        std::fs::write(&stale, "stale, from before #593\n").unwrap();

        let mut outcomes = Vec::new();
        crate::platform::test_home::with_home(&home, || {
            skills(&repo, &args(), false, &mut outcomes).unwrap();
        });

        let on_disk = std::fs::read_to_string(&stale).unwrap();
        assert_ne!(
            on_disk, "stale, from before #593\n",
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
