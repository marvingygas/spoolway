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
//! "helped".
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
//! meant.
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
    /// A pipeline file rewritten to migrate a retired step shape — see
    /// `crate::pipeline::migrate_retired_shapes`. Its own variant rather than
    /// another [`Outcome::Wrote`], because its detail is one of the few this
    /// report actually prints under the file's own `wrote` line, in both the
    /// long form (`report`) `run`'s own report and `--dry-run` use and the
    /// short one (`panel`) that fits `crate::gate`'s bounded confirm line —
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
    }
    Ok(())
}

/// The paths worth naming out of a scan, deduplicated: one file can be
/// behind for several reasons at once — a config gains a setting and drops a
/// retired one in the same rewrite — and a path printed twice reads as two
/// files. Shared by [`run`]'s own report and `confirm-dialog`'s gate, which
/// draws the same two lists in a panel before either has run for real.
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
/// extra explanation [`run`]'s own report and `crate::gate`'s confirm panel
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
    let mut outcomes = Vec::new();
    ignores(repo, args, &mut outcomes)?;
    config(repo, args, &mut outcomes)?;
    templates(repo, args, &mut outcomes)?;
    skills(repo, args, &mut outcomes)?;
    retired_skills(repo, args, &mut outcomes)?;
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
fn ignores(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
    use crate::gitignore::Removed;

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
        Err(err) => {
            outcomes.push(Outcome::blocked(
                &shown,
                format!("could not be read ({err})"),
            ));
            return Ok(());
        }
    };

    // Read from the file rather than taken from `repo`, so that what is
    // written back is what this document says. Not the same config as
    // `repo.config`: that is `Config::load(&repo.root)`, and in a linked
    // worktree — the run this command now exists for — `root` and
    // `checkout` can hold two different files. `current` is always the
    // checkout's own, so a rewrite is sourced from the file it replaces.
    //
    // `load_dropping_interval` rather than `Config::load`: `dispatch.interval`
    // is retired hard enough that an ordinary load refuses a file still
    // naming it, and this is the one place that has to bring such a file
    // forward instead of rejecting it.
    let current = match crate::config::Config::load_dropping_interval(&repo.checkout) {
        Ok(config) => config,
        Err(err) => {
            outcomes.push(Outcome::blocked(&shown, format!("{err:#}")));
            return Ok(());
        }
    };

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

        let is_hook = path.starts_with(repo.checkout.join(".spoolway/hooks"));

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
    if path.starts_with(repo.checkout.join(".spoolway/hooks")) {
        return crate::assets::HOOK_SCRIPTS
            .iter()
            .find(|(known, _)| Path::new(known).file_stem().and_then(|s| s.to_str()) == Some(stem))
            .map(|(_, body)| body.to_string());
    }

    // The workflow `init` writes into `.github/workflows/` only for
    // `github` — the one shipped asset outside `.spoolway/` entirely, and
    // still never touched by an ordinary sync.
    if path == repo.checkout.join(".github/workflows/spoolway-issues.yml") {
        return Some(crate::assets::GITHUB_ISSUE_WORKFLOW.to_string());
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
        || planned.iter().any(|file| file.path.exists())
        || crate::install::RETIRED_SKILLS
            .iter()
            .any(|name| provider.skills_dir(checkout).join(name).is_dir())
}

/// Skills are rewritten to the shipped copy where a project installed them —
/// unless the skill stamp says a person changed one by hand, which is
/// reported and left alone the same way a hand-edited skeleton block is; see
/// [`read_skill_fingerprint`]. Writing them into a project that never ran
/// `install` would be this command choosing an agent on someone's behalf.
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
fn skills(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
    for provider in <crate::cli::Provider as clap::ValueEnum>::value_variants() {
        let planned = provider.plan(&repo.checkout);
        let dir = provider.skills_dir(&repo.checkout);
        let old_codex_root = repo.checkout.join(".codex").join("skills");
        let migrate_codex = provider_needs_codex_migration(*provider, &repo.checkout, &planned);
        if !provider_installed(*provider, &repo.checkout, &planned) {
            continue;
        }

        for planned in planned {
            let shown = crate::platform::relative(&repo.checkout, &planned.path);
            match std::fs::read_to_string(&planned.path) {
                Ok(on_disk) if on_disk == planned.contents => {
                    outcomes.push(Outcome::Kept);
                    if !args.dry_run {
                        record_skill_fingerprint(
                            &repo.home,
                            &planned.path,
                            &crate::skeleton::fingerprint(planned.contents),
                        )?;
                    }
                    continue;
                }
                Ok(on_disk) => {
                    // Ours only if the fingerprint spoolway itself last
                    // recorded here still matches what is on disk now — the
                    // same distinction `BlockState` draws for a skeleton's
                    // block, kept the other way round because a skill file
                    // has no hand-curated `history` of every shape it has
                    // ever shipped. No record at all is not proof either
                    // way, so it reads as a hand edit too — see
                    // [`read_skill_fingerprint`].
                    let last_shipped = read_skill_fingerprint(&repo.home, &planned.path);
                    if last_shipped.as_deref()
                        != Some(crate::skeleton::fingerprint(&on_disk).as_str())
                    {
                        outcomes.push(Outcome::blocked(
                            &shown,
                            format!(
                                "was changed by hand, so it was left alone — `spoolway install \
                                 {} --force` takes the shipped skills back",
                                provider.name()
                            ),
                        ));
                        continue;
                    }
                    if !args.dry_run {
                        write_atomic(&planned.path, planned.contents)?;
                        record_skill_fingerprint(
                            &repo.home,
                            &planned.path,
                            &crate::skeleton::fingerprint(planned.contents),
                        )?;
                    }
                    outcomes.push(Outcome::wrote(&shown, "rewritten"));
                }
                Err(_) => {
                    if !args.dry_run {
                        write_atomic(&planned.path, planned.contents)?;
                        record_skill_fingerprint(
                            &repo.home,
                            &planned.path,
                            &crate::skeleton::fingerprint(planned.contents),
                        )?;
                    }
                    outcomes.push(Outcome::wrote(&shown, "added"));
                }
            }
        }

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
            names.extend(crate::install::RETIRED_SKILLS.iter().map(|s| s.to_string()));
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
fn retired_skills(repo: &Repo, args: &SyncArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
    for provider in <crate::cli::Provider as clap::ValueEnum>::value_variants() {
        let dir = provider.skills_dir(&repo.checkout);
        for name in crate::install::RETIRED_SKILLS {
            let stale = dir.join(name);
            if !stale.is_dir() {
                continue;
            }
            let shown = crate::platform::relative(&repo.checkout, &stale);
            if !args.dry_run {
                std::fs::remove_dir_all(&stale)
                    .with_context(|| format!("removing {}", stale.display()))?;
            }
            outcomes.push(Outcome::removed(&shown, "renamed to spoolway-config"));
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
        let path = repo.checkout.join(name);
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
/// Three departures from the rule the module doc states, all deliberate:
///
/// - An edit inside the markers is discarded, not refused. This is
///   `config.toml`'s bargain, not a skeleton's: there is nothing in here for a
///   project to have meant, because every sentence is a claim about what the
///   binary does. A hand-edited one is a claim that has stopped being true.
/// - A file with no markers is left completely alone and not reported. Writing
///   a block into a pipeline somebody wrote themselves would be this command
///   helping, which is the one thing it must never do. Pasting the two markers
///   in is how a pipeline opts in.
/// - The three retired step shapes — an `on_fail:` naming its own step,
///   `loop:` as the old per-route map, and `on_loop_max:` — are migrated
///   ahead of the fence, on any file that has opted in. Unlike the key
///   reference this does read into a step, but it still never re-serialises
///   one: [`crate::pipeline::migrate_retired_shapes`] edits the file's own
///   text, so everything else about a step — its prose, its key order, the
///   blank lines around it — is copied through unread the same as ever.
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

        // The three retired step shapes migrated ahead of the key
        // reference: each rewrites this file's own text, never re-parsing
        // it back out to serde, so re-reading the fence just below still
        // finds it exactly where it was — see
        // `crate::pipeline::migrate_retired_shapes`.
        let mut on_disk = on_disk;
        let mut changed = false;
        if let Some((migrated_text, changes)) = crate::pipeline::migrate_retired_shapes(&on_disk) {
            on_disk = migrated_text;
            changed = true;
            for change in changes {
                outcomes.push(Outcome::migrated(
                    &shown,
                    format!("migrated: {}", change.report),
                    format!("migrated: {}", change.panel),
                ));
            }
        }

        let found = region
            .read(&on_disk)
            .expect("migrate_retired_shapes never touches the fenced key reference");
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
    // config that will not load or render at all is a fact `sync`'s own
    // `config()` reports as `Outcome::Blocked` and leaves untouched, not a
    // reason to fail the whole stamp — the fingerprint it produces here is
    // then taken over the pipeline key block and installed skills alone,
    // which is still a real answer for whether *those* have drifted.
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
/// a config whose own values have changed since. `confirm-dialog`'s gate
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
// The skill stamp: what spoolway itself last wrote at each installed skill
// file. [`skeleton::BlockState`] tells a block an earlier release shipped
// from one a person edited by keeping a hand-curated `history` of every
// fingerprint that block has ever had. A skill file has no such curated list
// — nobody appends to one every time a `SKILL.md` changes — so this keeps the
// same fact the other way round: not every fingerprint a file has ever had,
// but the one fingerprint spoolway itself put there last. A mismatch against
// that recorded value, not against today's shipped copy, is what a hand edit
// looks like; a mismatch that agrees with it is exactly a shipped copy this
// project has not been brought current yet.
// ---------------------------------------------------------------------------

/// The skill stamp's file name, under [`Repo::home`] — beside [`STAMP_FILE`]
/// for the same reason: a home is shared by every worktree cut from it, and
/// each keeps its own lines rather than fighting over shared ones.
pub const SKILL_STAMP_FILE: &str = "skill-stamp";

/// Where the skill stamp lives, given a project's home directory.
pub fn skill_stamp_path(home: &Path) -> PathBuf {
    home.join(SKILL_STAMP_FILE)
}

/// Record that spoolway itself last wrote `fingerprint` at `path` — one line,
/// `<fingerprint> <path>`, replacing any earlier line for the same path.
/// `path` is the absolute path a [`crate::install::Planned`] entry carries,
/// which already encodes the checkout: no separate key is needed the way
/// [`write_stamp_line`] needs one for a checkout with several tracked files.
pub(crate) fn record_skill_fingerprint(home: &Path, path: &Path, fingerprint: &str) -> Result<()> {
    let stamp = skill_stamp_path(home);
    let existing = std::fs::read_to_string(&stamp).unwrap_or_default();
    let shown = path.display().to_string();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|line| line.split_once(' ').map(|(_, path)| path) != Some(shown.as_str()))
        .map(str::to_string)
        .collect();
    lines.push(format!("{fingerprint} {shown}"));
    lines.sort();
    let mut body = lines.join("\n");
    body.push('\n');
    write_atomic(&stamp, body)
}

/// What the skill stamp says spoolway last wrote at `path`, if anything.
/// [`crate::install::install`] records one here for every file it actually
/// writes, and so does [`skills`] itself, so an ordinary install followed by
/// an ordinary sync always has one. No record at all — a project installed
/// by a spoolway old enough not to keep this stamp — is not proof the file
/// on disk is ours, so [`skills`] treats it the same as a fingerprint that
/// disagrees: blocked, not silently rewritten. The one place this bites is
/// the first sync after upgrading such a project to a spoolway that keeps
/// this stamp: every one of its skill files reads as blocked once,
/// `spoolway install <provider> --force` takes them back, and every sync
/// after that — like every sync on a project that installed fresh — tells a
/// real hand edit apart from a stale shipped copy correctly.
fn read_skill_fingerprint(home: &Path, path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(skill_stamp_path(home)).ok()?;
    let shown = path.display().to_string();
    text.lines().find_map(|line| {
        let (fingerprint, recorded_path) = line.split_once(' ')?;
        (recorded_path == shown).then(|| fingerprint.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn fixture(name: &str) -> Repo {
        let root = crate::scratch::root(&format!("sync-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let config = Config::default();
        std::fs::create_dir_all(root.join(crate::config::TASK_TEMPLATES_DIR)).unwrap();
        std::fs::create_dir_all(root.join(".spoolway/templates")).unwrap();
        let home = root.join(".home");
        Repo {
            checkout: root.clone(),
            root,
            config,
            home,
        }
    }

    fn args() -> SyncArgs {
        SyncArgs {
            dry_run: false,
            replace: Vec::new(),
        }
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
        let repo = fixture("report-shape");
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
        let repo = fixture("config-forward");
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
        let repo = fixture("config-comment");
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
        let repo = fixture("config-dry");
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

    /// `dispatch.interval` is retired hard enough that an ordinary load
    /// refuses a file still naming it — the same refusal `spoolway config
    /// get` or `dispatch` would hit on this file today. `sync` is the one
    /// path that has to bring it forward instead: it drops the key, rewrites
    /// the reference header around its removal, and leaves the rest of the
    /// file exactly as it read it.
    #[test]
    fn a_config_still_naming_dispatch_interval_is_refused_before_sync_and_accepted_after() {
        let repo = fixture("config-retired-interval");
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
            root: root.clone(),
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
            root: root.clone(),
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
        // The skill stamp is what tells this apart from a hand edit: record
        // it as spoolway's own last write, the way an earlier sync actually
        // would have.
        record_skill_fingerprint(
            &repo.home,
            &first.path,
            &crate::skeleton::fingerprint("stale, from an older release\n"),
        )
        .unwrap();

        // `retired_skills`: a directory this binary no longer ships.
        let retired = claude_dir.join(crate::install::RETIRED_SKILLS[0]);
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

        let repo = fixture("replace-hook-executable");
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

        let repo = fixture("replace-hook-dry-run");
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
        let repo = fixture("task-skeleton-untouched");
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
        let repo = fixture("no-skeletons-left");
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
        let repo = fixture("pipeline-stale");
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
        let repo = fixture("pipeline-edited");
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
        let repo = fixture("pipeline-unmarked");
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

    /// `spoolway sync` migrates a pipeline file's three retired step shapes
    /// in the same pass it refreshes the key reference: the result loads,
    /// the migration is named as an `Outcome::Migrated` under the file's own
    /// path, and the key reference is current too.
    #[test]
    fn sync_migrates_a_pipelines_retired_shapes_and_refreshes_its_key_reference() {
        let repo = fixture("pipeline-retired-shapes");
        let dir = crate::pipeline::Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bugfix.yml");
        std::fs::write(
            &path,
            format!(
                "{}\n# a stale key reference this sync also brings forward\n{}\n\nsteps:\n  \
                 - id: fix\n    agent: pi\n    on_pass: review\n  \
                 - id: review\n    agent: pi\n    loop:\n      fix: 2\n    on_pass: checks\n    \
                 on_fail: fix\n  \
                 - id: checks\n    run: gh pr checks\n    loop:\n      checks: 3\n    \
                 on_pass: done\n    on_fail: checks\n",
                crate::assets::PIPELINE_KEYS_BEGIN,
                crate::assets::PIPELINE_KEYS_END
            ),
        )
        .unwrap();

        let mut outcomes = Vec::new();
        pipelines(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(after.contains(crate::pipeline::key_block()), "{after}");
        assert!(
            after.contains("    loop: 3\n    on_pass: review"),
            "{after}"
        );
        assert!(!after.contains("on_fail: checks"), "{after}");
        assert!(!after.contains("checks: 3"), "{after}");
        crate::pipeline::Pipeline::parse("bugfix", &after).expect("migrated file must load");

        let migrated: Vec<(&str, &str)> = outcomes
            .iter()
            .filter_map(|o| match o {
                Outcome::Migrated { path, report, .. } => Some((path.as_str(), report.as_str())),
                _ => None,
            })
            .collect();
        assert!(
            migrated
                .iter()
                .any(|(p, r)| p.contains("bugfix.yml") && r.contains("no longer routes a failure")),
            "{migrated:?}"
        );
        assert!(
            migrated
                .iter()
                .any(|(p, r)| p.contains("bugfix.yml") && r.contains("became `loop: 3` on `fix`")),
            "{migrated:?}"
        );
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .any(|line| line.starts_with("wrote") && line.contains("key reference refreshed")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// Half a fence is the one shape worth a refusal: the lines under a start
    /// marker with no end could be anyone's.
    #[test]
    fn a_start_marker_with_no_end_is_reported_rather_than_guessed_at() {
        let repo = fixture("pipeline-unterminated");
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
        let repo = fixture("pipeline-dry");
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
        let repo = fixture("replace");
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
            (
                repo.checkout.join(".github/workflows/spoolway-issues.yml"),
                "close-issue",
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
        let repo = fixture("prompt-shadowed");
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

    /// `sync::skills` refreshes every installed provider, not `claude` alone —
    /// a codex or pi project kept stale skills after every sync otherwise
    /// (finding 27). The `path.exists()` guard still limits it to providers the
    /// project actually installed.
    #[test]
    fn skills_refreshes_every_installed_provider() {
        let repo = fixture("skills-providers");

        for provider in [crate::cli::Provider::Codex, crate::cli::Provider::Pi] {
            let first = provider.plan(&repo.root).into_iter().next().unwrap();
            std::fs::create_dir_all(first.path.parent().unwrap()).unwrap();
            std::fs::write(&first.path, "stale, from an older release\n").unwrap();
            // Recorded as spoolway's own last write, standing in for the
            // earlier sync that would really have put it there — without
            // this, an unrecognised fingerprint reads as a hand edit.
            record_skill_fingerprint(
                &repo.home,
                &first.path,
                &crate::skeleton::fingerprint("stale, from an older release\n"),
            )
            .unwrap();
        }

        let mut outcomes = Vec::new();
        skills(&repo, &args(), &mut outcomes).unwrap();
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
        let repo = fixture("skills-new-skill");
        let claude_dir = crate::cli::Provider::Claude.skills_dir(&repo.root);
        let plan = claude_dir.join("spoolway-plan").join("SKILL.md");
        std::fs::create_dir_all(plan.parent().unwrap()).unwrap();
        std::fs::write(&plan, "stale, from an older release\n").unwrap();
        // Recorded as spoolway's own last write, standing in for the earlier
        // sync that would really have put it there.
        record_skill_fingerprint(
            &repo.home,
            &plan,
            &crate::skeleton::fingerprint("stale, from an older release\n"),
        )
        .unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), &mut outcomes).unwrap();
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

    /// The module doc promises "Anything a person has changed is reported
    /// and left exactly as it is" — `templates()` keeps that promise via
    /// `BlockState::HandEdited`; `skills()` now does too, via the skill
    /// stamp. This fixture stands for the legacy case: a project whose
    /// skills predate the stamp, so nothing was ever recorded for this
    /// path — `install` now records one for every file it writes, so an
    /// ordinary install-then-sync never lands here. No fingerprint recorded
    /// is not proof the file on disk is ours, so it is reported as blocked
    /// and left alone rather than rewritten like a stale one. See the next
    /// test for the sharper case, where a recorded fingerprint disagrees
    /// with what is on disk.
    #[test]
    fn a_hand_edited_skill_file_is_reported_as_blocked_and_left_alone() {
        let repo = fixture("skills-hand-edited");
        let planned = crate::cli::Provider::Claude.plan(&repo.root);
        let first = planned.into_iter().next().unwrap();
        std::fs::create_dir_all(first.path.parent().unwrap()).unwrap();
        let edited = format!("{}\na line a person added by hand\n", first.contents);
        std::fs::write(&first.path, &edited).unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), &mut outcomes).unwrap();
        let lines = outcome_lines(&outcomes);

        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            edited,
            "a hand-edited skill file must be left exactly as it is: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("blocked")
                && l.contains("spoolway install")
                && l.contains("--force")),
            "{lines:?}"
        );
    }

    /// The discrimination the fix is actually built on: a file whose
    /// recorded fingerprint agrees with what is on disk, exactly as
    /// `install` or an earlier `sync` would have left it, is stale and
    /// gets rewritten — the same file, edited by hand afterwards, is
    /// blocked instead, even though both start from a real recorded
    /// fingerprint rather than no record at all.
    #[test]
    fn a_recorded_fingerprint_tells_a_stale_copy_from_a_later_hand_edit() {
        let repo = fixture("skills-recorded-then-edited");
        let planned = crate::cli::Provider::Claude.plan(&repo.root);
        let first = planned.into_iter().next().unwrap();
        std::fs::create_dir_all(first.path.parent().unwrap()).unwrap();

        // An older release's text, recorded as spoolway's own — the shape
        // `install` leaves a file in when it writes it.
        let old_release = "an older release's text\n";
        std::fs::write(&first.path, old_release).unwrap();
        record_skill_fingerprint(
            &repo.home,
            &first.path,
            &crate::skeleton::fingerprint(old_release),
        )
        .unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), &mut outcomes).unwrap();
        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            first.contents,
            "a recorded fingerprint that matches what is on disk is a stale copy, not a hand edit: {:?}",
            outcome_lines(&outcomes)
        );

        // Now a person edits the file this sync just brought current.
        // Its fingerprint still matches, but the text it matches is no
        // longer what is on disk.
        let edited = format!("{}\na line a person added by hand\n", first.contents);
        std::fs::write(&first.path, &edited).unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), &mut outcomes).unwrap();
        let lines = outcome_lines(&outcomes);
        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            edited,
            "a recorded fingerprint that no longer matches what is on disk is a hand edit: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("blocked") && l.contains("--force")),
            "{lines:?}"
        );
    }

    /// The rename case in full: a project whose only trace of an install is
    /// the retired directory still counts as installed, so the skill that
    /// replaced it is written in the same pass that removes the old one.
    #[test]
    fn sync_writes_the_renamed_skill_where_only_the_retired_one_stood() {
        let repo = fixture("skills-only-retired");
        let claude_dir = crate::cli::Provider::Claude.skills_dir(&repo.root);
        let stale = claude_dir.join("spoolway-pipeline");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("SKILL.md"), "the old skill\n").unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), &mut outcomes).unwrap();
        retired_skills(&repo, &args(), &mut outcomes).unwrap();

        assert!(claude_dir.join("spoolway-config").join("SKILL.md").exists());
        assert!(!stale.exists());
        for planned in crate::cli::Provider::Claude.plan(&repo.root) {
            assert!(planned.path.exists(), "{}", planned.path.display());
        }
    }

    /// A dry run over the same tree reports every addition and writes none.
    #[test]
    fn a_dry_run_reports_a_missing_skill_without_writing_it() {
        let repo = fixture("skills-new-skill-dry");
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
        let repo = fixture("skills-codex-old-root");
        let old_root = repo.root.join(".codex").join("skills");
        for name in ["spoolway-plan", "spoolway-pipeline", "a-projects-own-skill"] {
            let dir = old_root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), "from 0.1.0\n").unwrap();
        }

        let mut outcomes = Vec::new();
        skills(&repo, &args(), &mut outcomes).unwrap();
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
        let repo = fixture("skills-codex-emptied");
        let dir = repo
            .root
            .join(".codex")
            .join("skills")
            .join("spoolway-plan");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "from 0.1.0\n").unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), &mut outcomes).unwrap();

        assert!(!repo.root.join(".codex").exists());
    }

    /// The same move in a dry run is reported and not made.
    #[test]
    fn a_dry_run_reports_the_codex_move_without_making_it() {
        let repo = fixture("skills-codex-old-root-dry");
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
        let repo = fixture("retired-skill");
        let claude_dir = crate::cli::Provider::Claude.skills_dir(&repo.root);
        let stale = claude_dir.join("spoolway-pipeline");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("SKILL.md"), "the old skill\n").unwrap();

        let untouched = claude_dir.join("a-projects-own-skill");
        std::fs::create_dir_all(&untouched).unwrap();
        std::fs::write(untouched.join("SKILL.md"), "not spoolway's\n").unwrap();

        let mut outcomes = Vec::new();
        retired_skills(&repo, &args(), &mut outcomes).unwrap();

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

    /// A dry run reports the removal without actually deleting anything —
    /// the same promise every other `sync` scan already keeps.
    #[test]
    fn sync_dry_run_reports_a_stale_skill_without_removing_it() {
        let repo = fixture("retired-skill-dry-run");
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
        retired_skills(&repo, &dry, &mut outcomes).unwrap();

        assert!(stale.is_dir(), "a dry run must not delete anything");
        assert_eq!(outcome_lines(&outcomes).len(), 1);
    }

    /// All three dead templates go, each with its own reason, and a project's
    /// own file under `.spoolway/templates/` — named by neither
    /// [`crate::install::RETIRED_TEMPLATES`] nor a shape `init` still
    /// places — is left exactly where it was.
    #[test]
    fn sync_removes_every_retired_template_and_leaves_a_projects_own_file() {
        let repo = fixture("retired-templates");
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
        let repo = fixture("retired-templates-dry-run");
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
        let repo = fixture("stamp-roundtrip");
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

    /// The skill stamp's own version of the same guarantee: written for one
    /// path, re-readable straight back, a later write for that same path
    /// replacing rather than duplicating its line, and a sibling path's own
    /// line kept untouched.
    #[test]
    fn a_skill_fingerprint_reads_back_and_keeps_a_siblings_line() {
        let repo = fixture("skill-stamp-roundtrip");
        let path = repo.checkout.join(".claude/skills/spoolway-plan/SKILL.md");
        let sibling = repo.checkout.join(".claude/skills/spoolway-tasks/SKILL.md");

        record_skill_fingerprint(&repo.home, &sibling, "sibling-fingerprint").unwrap();
        record_skill_fingerprint(&repo.home, &path, "first-fingerprint").unwrap();
        assert_eq!(
            read_skill_fingerprint(&repo.home, &path).as_deref(),
            Some("first-fingerprint")
        );

        // A later write for the same path replaces its line rather than
        // adding a second one — a stamp with two lines for one path would
        // leave `find_map` picking whichever happened to come first.
        record_skill_fingerprint(&repo.home, &path, "second-fingerprint").unwrap();
        assert_eq!(
            read_skill_fingerprint(&repo.home, &path).as_deref(),
            Some("second-fingerprint")
        );
        let stamp = std::fs::read_to_string(skill_stamp_path(&repo.home)).unwrap();
        assert_eq!(
            stamp
                .lines()
                .filter(|l| l.ends_with(&path.display().to_string()))
                .count(),
            1,
            "{stamp}"
        );

        // The sibling's own line is still there, untouched.
        assert_eq!(
            read_skill_fingerprint(&repo.home, &sibling).as_deref(),
            Some("sibling-fingerprint")
        );

        // A path with nothing ever recorded for it reads as no evidence
        // either way, not as an empty string.
        let never_written = repo
            .checkout
            .join(".claude/skills/spoolway-doctor/SKILL.md");
        assert_eq!(read_skill_fingerprint(&repo.home, &never_written), None);
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
        let repo = fixture("stamp-dry-run");
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
