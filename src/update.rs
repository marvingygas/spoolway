//! `spoolway update` — take what a newer spoolway writes, keep what you wrote.
//!
//! `init` has only ever had two answers for a file that already exists: leave it
//! alone, or overwrite it. Neither is what upgrading means. A project that keeps
//! everything runs last year's contract against this year's binary; a project
//! that runs `init --force` gets the new one and loses every prompt it has
//! written since.
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
//! the binary on every save. Upgrading spoolway therefore cannot disturb a word
//! a project wrote in any of them, and `spoolway pipeline check` is what
//! catches a command name that has fallen behind the CLI.
//!
//! Everything below writes `repo.checkout`, never `repo.root`: the control
//! plane this command refreshes is tracked, so a lane running in a linked
//! worktree has its own branch's copies, and an update taken there has to
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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::UpdateArgs;
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
}

/// The one line that says what the paths above it do not.
///
/// Without it the report is a list of files with no statement of what
/// happened to them, and the thing people actually worry about — "did it eat
/// my config?" — goes unanswered.
const KEPT: &str =
    "Files were overwritten; your config values, prompts and task skeletons were kept.";

/// The same line for a dry run, which has to say the opposite: the paths
/// above it are what an update *would* take, and nothing was touched.
const DRY_RUN: &str = "Dry run: nothing was written. Run without --dry-run to take it.";

pub fn run(repo: &Repo, args: &UpdateArgs, json: bool) -> Result<()> {
    use std::io::IsTerminal;

    if !args.replace.is_empty() {
        return replace(repo, args);
    }

    // The binary first, and then its own files. A dry run installs nothing:
    // "what would change" is a question about this project, and answering it
    // by replacing the executable would be the least expected thing this
    // command could do.
    if !args.dry_run
        && let Some(status) = install(repo)?
    {
        // The new binary did the file work, and its exit code is the one
        // that means anything — this process has nothing left to say.
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
        return Ok(());
    }

    // Printed here, not above install(): this project's own files are what
    // the note is about, and it would be noise ahead of an npm check that has
    // nothing to do with which checkout gets written.
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
    let mut wrote: Vec<&str> = Vec::new();
    let mut removed: Vec<(&str, &str)> = Vec::new();
    for outcome in &outcomes {
        match outcome {
            Outcome::Wrote { path, .. } if !wrote.contains(&path.as_str()) => wrote.push(path),
            Outcome::Removed { path, why } if !removed.iter().any(|(p, _)| *p == path) => {
                removed.push((path, why))
            }
            Outcome::Wrote { .. }
            | Outcome::Removed { .. }
            | Outcome::Kept
            | Outcome::Blocked { .. } => {}
        }
    }
    // A dry run reports the same paths in the conditional: "wrote" over a
    // tree nothing touched reads as a lie the moment `git status` is run.
    let (wrote_word, removed_word) = match args.dry_run {
        true => ("would write ", "would remove"),
        false => ("wrote       ", "removed     "),
    };
    for path in &wrote {
        println!("  {wrote_word} {path}");
    }
    for (path, why) in &removed {
        println!("  {removed_word} {path}");
        println!("               ({why})");
    }

    println!();
    match args.dry_run {
        true => println!("{DRY_RUN}"),
        false => println!("{KEPT}"),
    }

    let upgraded = std::env::var(crate::release::ENV_UPGRADED).ok();
    if let Some(previous) =
        digest_previous(args, upgraded.as_deref(), std::io::stdout().is_terminal())
        && let Some(digest) = crate::release_notes::update_digest(previous, true)?
    {
        println!();
        print!("{digest}");
    }
    Ok(())
}

/// Whether this invocation is the successful far side of a person-facing npm
/// handover. The explicit arguments prevent a forged/stale environment value
/// from making dry runs or file replacement print an upgrade digest, while the
/// terminal gate keeps stdout stable for scripts and pipes.
fn digest_previous<'a>(
    args: &UpdateArgs,
    upgraded: Option<&'a str>,
    terminal: bool,
) -> Option<&'a str> {
    (!args.dry_run && args.replace.is_empty() && terminal)
        .then_some(upgraded)
        .flatten()
}

/// Take the newer release, if there is one, and hand over to it.
///
/// `Some(status)` means this process is done: the new binary was run in its
/// place and its exit status is the one that matters. `None` means carry on
/// here — nothing newer, nothing installable, or a dispatcher that must not
/// have its binary rewritten under it.
fn install(repo: &Repo) -> Result<Option<std::process::ExitStatus>> {
    let upgrade = crate::release::upgrade(&repo.lock_file());
    install_upgrade(repo, upgrade, crate::release::hand_over)
}

/// Turn the release layer's exhaustive outcome into update behaviour. Keeping
/// the handover call injectable makes the success edge testable without
/// replacing the running binary or invoking npm; the production caller passes
/// [`crate::release::hand_over`] unchanged.
fn install_upgrade<F>(
    repo: &Repo,
    upgrade: crate::release::Upgrade,
    hand_over: F,
) -> Result<Option<std::process::ExitStatus>>
where
    F: FnOnce(&[String], &str) -> Result<std::process::ExitStatus>,
{
    use crate::release::Upgrade;

    match upgrade {
        Upgrade::Current => Ok(None),

        Upgrade::Installed(version) => {
            println!("Installing spoolway {version}");
            println!(
                "  npm install -g {}@{version} --ignore-scripts",
                crate::release::PACKAGE
            );
            println!();
            Ok(Some(hand_over(&relaunch(repo), &version)?))
        }

        // The whole reason a channel is resolved before anything is said: an
        // install npm cannot reach is told how it is upgraded instead of being
        // handed a command that would refuse.
        Upgrade::Unmanaged(version) => {
            println!(
                "spoolway {version} is out. This binary was not installed by npm, so upgrade it"
            );
            println!("the way you installed it.");
            println!();
            Ok(None)
        }

        Upgrade::Dispatching(version, pid) => {
            println!("spoolway {version} is out, and a dispatcher is running (pid {pid}), so the");
            println!("binary was left alone — replacing it would kill the run mid-pass. The");
            println!("files below were still brought forward.");
            println!();
            Ok(None)
        }
    }
}

/// This command, as the new binary should run it.
///
/// `-C` is passed whether or not it was typed, and that is the point: the
/// child inherits a working directory, not a discovery. A `spoolway -C /elsewhere
/// update` that handed over without it would upgrade the binary and then update
/// whichever project the terminal happened to be sitting in.
///
/// `repo.checkout`, not `repo.root`: this command writes the checkout it was
/// run in (see the module doc), and a `-C <worktree> update` that relaunched
/// against `root` would hand the newly installed binary back to the main
/// checkout, undoing the redirect the flag asked for.
fn relaunch(repo: &Repo) -> Vec<String> {
    vec![
        "-C".to_string(),
        repo.checkout.display().to_string(),
        "update".to_string(),
    ]
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

pub fn scan(repo: &Repo, args: &UpdateArgs) -> Result<Vec<Outcome>> {
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
/// once, for a project upgrading in place; from then on it finds nothing and
/// reports nothing. See [`crate::gitignore`].
fn ignores(repo: &Repo, args: &UpdateArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
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
/// **values** are the project's and survive an update untouched, and everything
/// else — which settings are written down, what order they come in, and every
/// comment above them — is spoolway's and is rewritten from the binary.
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
fn config(repo: &Repo, args: &UpdateArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
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
    let current = match crate::config::Config::load(&repo.checkout) {
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
fn templates(repo: &Repo, args: &UpdateArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
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
                 keep current. `spoolway update --replace <path>` takes the shipped file back",
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
    args: &UpdateArgs,
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
fn replace(repo: &Repo, args: &UpdateArgs) -> Result<()> {
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

        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        if on_disk == shipped {
            println!("  kept    {shown} (already ours)");
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

/// The text spoolway would write at `path`, if it writes anything there at all.
fn shipped_for(repo: &Repo, path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;

    // Prompts are outside the update cycle, but not outside `--replace`. This
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

    if *path == repo.lane_prompts_path() {
        return Some(crate::assets::LANE_PROMPTS.to_string());
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

/// Skills carry no local edits by design, so they are rewritten — but only where
/// a project installed them. Writing them into a project that never ran
/// `install` would be this command choosing an agent on someone's behalf.
///
/// Every provider, not just `claude`: `init` and `install` write the same
/// skills under `.agents/skills/` and `.pi/skills/` too, and a codex or pi
/// project that never sees them refreshed keeps stale skills after every
/// update. Whether a provider is installed is decided once, for the whole
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
fn skills(repo: &Repo, args: &UpdateArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
    for provider in <crate::cli::Provider as clap::ValueEnum>::value_variants() {
        let planned = provider.plan(&repo.checkout);
        let dir = provider.skills_dir(&repo.checkout);
        let old_codex_root = repo.checkout.join(".codex").join("skills");
        let migrate_codex = *provider == crate::cli::Provider::Codex
            && planned.iter().any(|file| {
                let relative = file
                    .path
                    .strip_prefix(&dir)
                    .expect("a provider's plan is below its skills directory");
                old_codex_root.join(relative).exists()
            });
        let installed = migrate_codex
            || planned.iter().any(|file| file.path.exists())
            || crate::install::RETIRED_SKILLS
                .iter()
                .any(|name| dir.join(name).is_dir());
        if !installed {
            continue;
        }

        for planned in planned {
            let shown = crate::platform::relative(&repo.checkout, &planned.path);
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
            outcomes.push(Outcome::wrote(&shown, detail));
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
fn retired_skills(repo: &Repo, args: &UpdateArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
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
fn retired_templates(repo: &Repo, args: &UpdateArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
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
/// Two departures from the rule the module doc states, both deliberate:
///
/// - An edit inside the markers is discarded, not refused. This is
///   `config.toml`'s bargain, not a skeleton's: there is nothing in here for a
///   project to have meant, because every sentence is a claim about what the
///   binary does. A hand-edited one is a claim that has stopped being true.
/// - A file with no markers is left completely alone and not reported. Writing
///   a block into a pipeline somebody wrote themselves would be this command
///   helping, which is the one thing it must never do. Pasting the two markers
///   in is how a pipeline opts in.
fn pipelines(repo: &Repo, args: &UpdateArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
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
        let Some(found) = region.read(&on_disk) else {
            // Half a fence is the one shape worth saying something about: the
            // lines under a start marker with no end could be anyone's, so
            // nothing is written and the missing marker is named.
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
        };

        if crate::skeleton::same(found, crate::pipeline::key_block()) {
            outcomes.push(Outcome::Kept);
            continue;
        }

        let Some(next) = region.replace(&on_disk, crate::pipeline::key_block()) else {
            outcomes.push(Outcome::blocked(&shown, "its block moved while we read it"));
            continue;
        };
        if !args.dry_run {
            write_atomic(&path, &next)?;
        }
        outcomes.push(Outcome::wrote(&shown, "key reference refreshed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn fixture(name: &str) -> Repo {
        let root = crate::scratch::root(&format!("update-{name}"));
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

    fn args() -> UpdateArgs {
        UpdateArgs {
            dry_run: false,
            replace: Vec::new(),
        }
    }

    #[test]
    fn only_a_successful_interactive_handover_gets_a_digest() {
        let normal = args();
        assert_eq!(digest_previous(&normal, Some("0.1.0"), true), Some("0.1.0"));
        assert_eq!(
            digest_previous(&normal, None, true),
            None,
            "ordinary file update"
        );
        assert_eq!(
            digest_previous(&normal, Some("0.1.0"), false),
            None,
            "piped output"
        );

        let dry = UpdateArgs {
            dry_run: true,
            ..args()
        };
        assert_eq!(digest_previous(&dry, Some("0.1.0"), true), None);
        let replace = UpdateArgs {
            replace: vec!["file".into()],
            ..args()
        };
        assert_eq!(digest_previous(&replace, Some("0.1.0"), true), None);
    }

    fn successful_status() -> std::process::ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(0)
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(0)
        }
    }

    #[test]
    fn every_upgrade_outcome_has_one_explicit_update_path() {
        use crate::release::Upgrade;

        let repo = fixture("upgrade-outcomes");
        assert!(
            install_upgrade(&repo, Upgrade::Current, |_, _| panic!("no handover"))
                .unwrap()
                .is_none()
        );
        assert!(
            install_upgrade(&repo, Upgrade::Unmanaged("0.2.0".into()), |_, _| panic!(
                "no handover"
            ))
            .unwrap()
            .is_none()
        );
        assert!(
            install_upgrade(
                &repo,
                Upgrade::Dispatching("0.2.0".into(), 42),
                |_, _| panic!("no handover")
            )
            .unwrap()
            .is_none()
        );

        let status = install_upgrade(
            &repo,
            Upgrade::Installed("0.2.0".into()),
            |args, version| {
                assert_eq!(args, relaunch(&repo));
                assert_eq!(version, "0.2.0");
                Ok(successful_status())
            },
        )
        .unwrap()
        .expect("an installed release hands over");
        assert!(status.success());
    }

    /// `-C <worktree> update` has to still target that worktree after the
    /// handover — the bug's own second paragraph. `every_upgrade_outcome_has_
    /// one_explicit_update_path` above only compares `relaunch(&repo)` against
    /// itself, which cannot catch a wrong path since `fixture()` also sets
    /// `checkout == root`; this asserts the literal argument list against a
    /// fixture where the two differ.
    #[test]
    fn relaunch_targets_the_checkout_not_the_root() {
        let root = crate::scratch::root("update-relaunch-checkout-vs-root");
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

        assert_eq!(
            relaunch(&repo),
            vec![
                "-C".to_string(),
                checkout.display().to_string(),
                "update".to_string(),
            ]
        );
    }

    fn outcome_lines(outcomes: &[Outcome]) -> Vec<String> {
        outcomes
            .iter()
            .map(|outcome| match outcome {
                Outcome::Wrote { path, detail } => format!("wrote {path} ({detail})"),
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
            "[dispatch]\ninterval = \"45s\"\n",
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

    /// The upgrade nothing used to perform: a config written before a setting
    /// existed gains it, with its note, and keeps every value and comment it
    /// already had.
    #[test]
    fn an_update_brings_a_config_forward_without_touching_its_values() {
        let repo = fixture("config-forward");
        let path = crate::config::Config::path_in(&repo.root);
        std::fs::write(
            &path,
            "[dispatch]\nbackend = \"headless\"\ninterval = \"45s\"\n",
        )
        .unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(after.contains("backend = \"headless\""));
        assert!(after.contains("interval = \"45s\""));
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
        let mine = "# Ten seconds: our lanes are quick and we watch the board.";
        std::fs::write(&path, format!("[dispatch]\n{mine}\ninterval = \"10s\"\n")).unwrap();

        let mut outcomes = Vec::new();
        config(&repo, &args(), &mut outcomes).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(!after.contains(mine));
        // Still explained — just in the table, not standing above the key.
        assert!(after.contains("How long the dispatcher waits"));
        assert!(
            after.contains("interval = \"10s\""),
            "the value is the one thing kept"
        );
        assert!(
            outcome_lines(&outcomes)
                .iter()
                .any(|line| line.starts_with("wrote") && line.contains("dispatch.interval")),
            "{:?}",
            outcome_lines(&outcomes)
        );
    }

    /// A dry run is a reading, on this file as on every other.
    #[test]
    fn a_dry_run_says_what_the_config_would_gain_and_writes_nothing() {
        let repo = fixture("config-dry");
        let path = crate::config::Config::path_in(&repo.root);
        let before = "[dispatch]\ninterval = \"45s\"\n";
        std::fs::write(&path, before).unwrap();

        let mut outcomes = Vec::new();
        config(
            &repo,
            &UpdateArgs {
                dry_run: true,
                ..args()
            },
            &mut outcomes,
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(!outcomes.is_empty());
    }

    /// Run from a linked worktree, `update` writes the checkout it was run
    /// in, not the main checkout's root — the whole point of the fix this
    /// bug describes. `config` is the sharpest case: it joins `repo.root`
    /// directly (src/update.rs:337) instead of going through
    /// `Config::path_in(&repo.checkout)` the way every other tracked-control-
    /// plane accessor already does.
    #[test]
    fn config_is_written_to_the_checkout_not_the_root_when_they_differ() {
        let root = crate::scratch::root("update-config-checkout-vs-root");
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
    /// the bug (src/update.rs:531-556 in the task's own account) since it
    /// matched a `repo.root`-built path against a `repo.checkout`-based
    /// accessor. Every assertion is positive — the checkout's own file
    /// actually changed — not just "nothing landed under root": a function
    /// that silently does nothing under a missing/uninstalled root would
    /// pass a root-only check without ever having read `checkout` at all.
    #[test]
    fn scan_and_replace_write_the_checkout_not_the_root_when_they_differ() {
        let root = crate::scratch::root("update-scan-checkout-vs-root");
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

        // `retired_skills`: a directory this binary no longer ships.
        let retired = claude_dir.join(crate::install::RETIRED_SKILLS[0]);
        std::fs::create_dir_all(&retired).unwrap();

        // `replace`: a task template this project no longer keeps, named
        // relative to the checkout — the join at src/update.rs:557 this test
        // exists to cover is never reached from an absolute path.
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
            &UpdateArgs {
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

    /// A task skeleton is the project's outright, so an update must not read it,
    /// rewrite it, or have an opinion about it — whatever is in it, and whether
    /// or not it looks anything like the one we ship.
    #[test]
    fn a_task_skeleton_is_never_touched_by_an_update() {
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

    /// `lane-prompts.md` is prose a project owns outright, the same rule a
    /// prompt or a task skeleton already keeps — so an ordinary update must
    /// not read it, rewrite it, or say a word about it.
    #[test]
    fn lane_prompts_is_never_touched_by_an_update() {
        let repo = fixture("lane-prompts-untouched");
        let path = repo.lane_prompts_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "## opening\n\nkeep me\n").unwrap();

        run(&repo, &args(), false).unwrap();
        let outcomes = scan(&repo, &args()).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "## opening\n\nkeep me\n"
        );
        assert!(
            !outcome_lines(&outcomes)
                .iter()
                .any(|line| line.contains("lane-prompts")),
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

    /// The upgrade a pipeline file could not otherwise get: a key reference an
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
            &UpdateArgs {
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
            (repo.task_templates_dir().join("default.md"), "## Goal"),
            (repo.lane_prompts_path(), "## opening"),
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

    /// `update::skills` refreshes every installed provider, not `claude` alone —
    /// a codex or pi project kept stale skills after every update otherwise
    /// (finding 27). The `path.exists()` guard still limits it to providers the
    /// project actually installed.
    #[test]
    fn skills_refreshes_every_installed_provider() {
        let repo = fixture("skills-providers");

        for provider in [crate::cli::Provider::Codex, crate::cli::Provider::Pi] {
            let first = provider.plan(&repo.root).into_iter().next().unwrap();
            std::fs::create_dir_all(first.path.parent().unwrap()).unwrap();
            std::fs::write(&first.path, "stale, from an older release\n").unwrap();
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
    fn update_adds_a_newly_shipped_skill_to_an_installed_provider() {
        let repo = fixture("skills-new-skill");
        let claude_dir = crate::cli::Provider::Claude.skills_dir(&repo.root);
        let plan = claude_dir.join("spoolway-plan").join("SKILL.md");
        std::fs::create_dir_all(plan.parent().unwrap()).unwrap();
        std::fs::write(&plan, "stale, from an older release\n").unwrap();

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

    /// The rename case in full: a project whose only trace of an install is
    /// the retired directory still counts as installed, so the skill that
    /// replaced it is written in the same pass that removes the old one.
    #[test]
    fn update_writes_the_renamed_skill_where_only_the_retired_one_stood() {
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
            &UpdateArgs {
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
    /// `.agents/skills`, and a 0.1.0 install lives under the old one. An
    /// update is the one chance to carry it forward without asking every
    /// project to reinstall by hand: the current set is written where Codex
    /// reads it now, and the copies spoolway itself put under the old root —
    /// current names and retired ones alike — go with the move. Skills carry
    /// no local edits by design, which is what makes that safe; a directory
    /// the project made there is not spoolway's and stays.
    #[test]
    fn update_moves_codex_skills_out_of_the_old_root() {
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
            &UpdateArgs {
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
    /// `spoolway-pipeline/` directory sitting beside its skills — `update`
    /// removes it, and a directory update has no reason to touch (a
    /// project's own skill, sharing no name with anything on the retired
    /// list) is left exactly as it was.
    #[test]
    fn update_removes_a_stale_renamed_skill_and_leaves_everything_else() {
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
    /// the same promise every other `update` scan already keeps.
    #[test]
    fn update_dry_run_reports_a_stale_skill_without_removing_it() {
        let repo = fixture("retired-skill-dry-run");
        let stale = crate::cli::Provider::Claude
            .skills_dir(&repo.root)
            .join("spoolway-pipeline");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("SKILL.md"), "the old skill\n").unwrap();

        let mut outcomes = Vec::new();
        let dry = UpdateArgs {
            dry_run: true,
            replace: Vec::new(),
        };
        retired_skills(&repo, &dry, &mut outcomes).unwrap();

        assert!(stale.is_dir(), "a dry run must not delete anything");
        assert_eq!(outcome_lines(&outcomes).len(), 1);
    }

    /// Both dead templates go, each with its own reason, and a project's own
    /// file under `.spoolway/templates/` — named by neither
    /// [`crate::install::RETIRED_TEMPLATES`] nor a shape `init` still
    /// places — is left exactly where it was.
    #[test]
    fn update_removes_both_retired_templates_and_leaves_a_projects_own_file() {
        let repo = fixture("retired-templates");
        let dir = repo.checkout.join(".spoolway/templates");
        let task_log = dir.join("task-log.md");
        let pull_request = dir.join("pull-request.md");
        std::fs::write(&task_log, "stale\n").unwrap();
        std::fs::write(&pull_request, "stale\n").unwrap();
        let untouched = dir.join("a-projects-own-notes.md");
        std::fs::write(&untouched, "mine\n").unwrap();

        let mut outcomes = Vec::new();
        retired_templates(&repo, &args(), &mut outcomes).unwrap();

        assert!(!task_log.exists(), "task-log.md must be removed");
        assert!(!pull_request.exists(), "pull-request.md must be removed");
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
    }

    /// A dry run reports both removals without deleting anything — the same
    /// promise [`update_dry_run_reports_a_stale_skill_without_removing_it`]
    /// keeps for a retired skill.
    #[test]
    fn update_dry_run_reports_retired_templates_without_removing_them() {
        let repo = fixture("retired-templates-dry-run");
        let dir = repo.checkout.join(".spoolway/templates");
        let task_log = dir.join("task-log.md");
        let pull_request = dir.join("pull-request.md");
        std::fs::write(&task_log, "stale\n").unwrap();
        std::fs::write(&pull_request, "stale\n").unwrap();

        let mut outcomes = Vec::new();
        let dry = UpdateArgs {
            dry_run: true,
            replace: Vec::new(),
        };
        retired_templates(&repo, &dry, &mut outcomes).unwrap();

        assert!(task_log.is_file(), "a dry run must not delete anything");
        assert!(pull_request.is_file(), "a dry run must not delete anything");
        assert_eq!(outcome_lines(&outcomes).len(), 2);
    }
}
