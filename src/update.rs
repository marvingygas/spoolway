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
//! a project wrote in any of them, and `spoolway prompt check` is what catches
//! prose that has fallen behind the CLI.
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

use anyhow::Result;

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
}

/// The one line that says what the paths above it do not.
///
/// Without it the report is a list of files with no statement of what
/// happened to them, and the thing people actually worry about — "did it eat
/// my config?" — goes unanswered.
const KEPT: &str =
    "Files were overwritten; your config values, prompts and task skeletons were kept.";

pub fn run(repo: &Repo, args: &UpdateArgs) -> Result<()> {
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

    // Deduped, because one file can be written for several reasons at once —
    // a config gains a setting and drops a retired one in the same rewrite —
    // and a path printed twice reads as two files.
    let outcomes = scan(repo, args)?;
    let mut written: Vec<&str> = Vec::new();
    for outcome in &outcomes {
        if let Outcome::Wrote { path, .. } = outcome
            && !written.contains(&path.as_str())
        {
            written.push(path);
        }
    }
    for path in &written {
        println!("{path}");
    }

    println!();
    println!("{KEPT}");
    Ok(())
}

/// Take the newer release, if there is one, and hand over to it.
///
/// `Some(status)` means this process is done: the new binary was run in its
/// place and its exit status is the one that matters. `None` means carry on
/// here — nothing newer, nothing installable, or a dispatcher that must not
/// have its binary rewritten under it.
fn install(repo: &Repo) -> Result<Option<std::process::ExitStatus>> {
    use crate::release::Upgrade;

    match crate::release::upgrade(&repo.lock_file()) {
        Upgrade::Current => Ok(None),

        Upgrade::Installed(version) => {
            println!("Installing spoolway {version}");
            println!(
                "  npm install -g {}@{version} --ignore-scripts",
                crate::release::PACKAGE
            );
            println!();
            Ok(Some(crate::release::hand_over(&relaunch(repo), &version)?))
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
fn relaunch(repo: &Repo) -> Vec<String> {
    vec![
        "-C".to_string(),
        repo.root.display().to_string(),
        "update".to_string(),
    ]
}

/// Every file spoolway owns here, and what would happen to it.
///
/// Split out from [`run`] so that `spoolway doctor` can ask the same question
/// without printing anything — which is where a [`Outcome::Blocked`] surfaces,
/// now that the report itself is only paths.
pub fn scan(repo: &Repo, args: &UpdateArgs) -> Result<Vec<Outcome>> {
    let mut outcomes = Vec::new();
    ignores(repo, args, &mut outcomes)?;
    config(repo, args, &mut outcomes)?;
    templates(repo, args, &mut outcomes)?;
    skills(repo, args, &mut outcomes)?;
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

    let shown = crate::platform::relative(&repo.root, &crate::gitignore::file(&repo.root));
    match crate::gitignore::remove(&repo.root, args.dry_run)? {
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
    let path = crate::config::Config::path_in(&repo.root);
    let shown = crate::platform::relative(&repo.root, &path);

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if !args.dry_run {
                repo.config.save(&repo.root)?;
            }
            outcomes.push(Outcome::wrote(&shown, "was missing"));
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

    // Read from the file rather than taken from `repo`, so that what is written
    // back is what this document says. The two are the same config in every
    // real run; making that an assumption is how a rewrite ends up sourced from
    // something other than the file it replaces.
    let current = match crate::config::Config::load(&repo.root) {
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
        let path = repo.root.join(skeleton.path);
        let shown = crate::platform::relative(&repo.root, &path);

        let on_disk = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if !args.dry_run {
                    write_atomic(&path, skeleton.shipped)?;
                }
                outcomes.push(Outcome::wrote(&shown, "was missing"));
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
            false => repo.root.join(name),
        };
        let shown = crate::platform::relative(&repo.root, &path);

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
                    crate::platform::relative(&repo.root, &nested)
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
                crate::platform::relative(&repo.root, &backup)
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
        true => println!("Dry run: nothing was written. Run without --dry-run to take it."),
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

    if *path == repo.pull_request_template_path() {
        return Some(crate::assets::PULL_REQUEST_TEMPLATE.to_string());
    }

    if *path == repo.lane_prompts_path() {
        return Some(crate::assets::LANE_PROMPTS.to_string());
    }

    if *path == repo.task_log_path() {
        return Some(crate::assets::TASK_LOG.to_string());
    }

    // The ignore rules are deliberately not here. They are a block in a file the
    // project owns the rest of, so there is no whole-file version of it to write:
    // `ignores` above refreshes the block, on every run, and that is the only way
    // back to ours.

    for skeleton in crate::skeleton::skeletons() {
        if *path == repo.root.join(skeleton.path) {
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
/// update. The `planned.path.exists()` check below still limits the writes to
/// providers a project actually installed. Codex's former `.codex/skills/`
/// root counts as an installation too: writing the current set under
/// `.agents/skills/` migrates it without deleting anything from the old root.
fn skills(repo: &Repo, args: &UpdateArgs, outcomes: &mut Vec<Outcome>) -> Result<()> {
    for provider in <crate::cli::Provider as clap::ValueEnum>::value_variants() {
        let planned = provider.plan(&repo.root);
        let migrate_codex = *provider == crate::cli::Provider::Codex
            && planned.iter().any(|file| {
                let relative = file
                    .path
                    .strip_prefix(provider.skills_dir(&repo.root))
                    .expect("a provider's plan is below its skills directory");
                repo.root
                    .join(".codex")
                    .join("skills")
                    .join(relative)
                    .exists()
            });

        for planned in planned {
            if !planned.path.exists() && !migrate_codex {
                continue;
            }
            let shown = crate::platform::relative(&repo.root, &planned.path);
            let on_disk = std::fs::read_to_string(&planned.path).unwrap_or_default();
            if on_disk == planned.contents {
                outcomes.push(Outcome::Kept);
                continue;
            }
            if !args.dry_run {
                write_atomic(&planned.path, planned.contents)?;
            }
            outcomes.push(Outcome::wrote(&shown, "rewritten"));
        }
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

    fn outcome_lines(outcomes: &[Outcome]) -> Vec<String> {
        outcomes
            .iter()
            .map(|outcome| match outcome {
                Outcome::Wrote { path, detail } => format!("wrote {path} ({detail})"),
                Outcome::Kept => "kept".to_string(),
                Outcome::Blocked { path, why } => format!("blocked {path}: {why}"),
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

    /// `lane-prompts.md` is prose a project owns outright, the same rule a
    /// prompt or a task skeleton already keeps — so an ordinary update must
    /// not read it, rewrite it, or say a word about it.
    #[test]
    fn lane_prompts_is_never_touched_by_an_update() {
        let repo = fixture("lane-prompts-untouched");
        let path = repo.lane_prompts_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "## opening\n\nkeep me\n").unwrap();

        run(&repo, &args()).unwrap();
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

    /// `task-log.md` is prose a project owns outright, the same rule
    /// `lane-prompts.md` above already keeps — so an ordinary update must
    /// not read it, rewrite it, or say a word about it.
    #[test]
    fn task_log_is_never_touched_by_an_update() {
        let repo = fixture("task-log-untouched");
        let path = repo.task_log_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "## Status Log\n\nkeep me\n").unwrap();

        run(&repo, &args()).unwrap();
        let outcomes = scan(&repo, &args()).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "## Status Log\n\nkeep me\n"
        );
        assert!(
            !outcome_lines(&outcomes)
                .iter()
                .any(|line| line.contains("task-log")),
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
            (repo.task_log_path(), "## Status Log"),
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

    /// Codex moved its repository skill root from `.codex/skills` to
    /// `.agents/skills`. An update is the one chance to carry existing
    /// installations forward without asking every project to reinstall by
    /// hand. The old files are deliberately left in place: they may include
    /// work that is not spoolway's to remove.
    #[test]
    fn skills_migrates_a_legacy_codex_install_without_deleting_it() {
        let repo = fixture("skills-codex-legacy");
        let legacy = repo.root.join(".codex/skills/spoolway-plan/SKILL.md");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "stale, from an older release\n").unwrap();

        let mut outcomes = Vec::new();
        skills(&repo, &args(), &mut outcomes).unwrap();

        for planned in crate::cli::Provider::Codex.plan(&repo.root) {
            assert_eq!(
                std::fs::read_to_string(&planned.path).unwrap(),
                planned.contents,
                "{} was not migrated",
                planned.path.display()
            );
        }
        assert!(legacy.exists(), "the legacy install must not be deleted");
    }
}
