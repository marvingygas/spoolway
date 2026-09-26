//! `spoolway init`: scaffolding a project, and the questions it asks first.

use super::*;

/// The answers `init` needs that are not spoolway's to choose.
///
/// Gathered before anything is written, so that a question is only asked when
/// its answer can still land somewhere: a project whose config already exists
/// is not asked what kind its lanes run, because `init` is not going to rewrite
/// that file and the answer would be taken and dropped.
struct Answers {
    /// The coding agent a person plans in and a fresh scaffold runs. Always
    /// settled — installing skills is worth doing for an established project.
    provider: Provider,
    /// The tracker `[issue_tracking]` names — `Tracker::None` when the
    /// config is staying as it is, because there is nothing to land it in.
    tracker: Tracker,
    /// The project the tracker's tickets open into. Blank whenever `tracker`
    /// is, and always blank on a config that is staying as it is.
    project_key: String,
    /// Whether `tracker`/`project_key` were actually resolved this run —
    /// true on a fresh project (always asked or answered) and on an
    /// established one when `--tracker` was given a value, or given bare
    /// with somebody there to answer the picker. False means
    /// `tracker`/`project_key` are the untouched placeholders above, and
    /// whoever decides what `.github/workflows/spoolway-issues.yml` gets
    /// written for has to read the tracker already on disk instead — which
    /// also covers a bare `--tracker` on an established project with nobody
    /// to ask: there is no answer to apply, so nothing about
    /// `[issue_tracking]` is touched, the same as the flag being absent.
    tracker_touched: bool,
}

impl Answers {
    /// Flags first, then the person, then the default — and the person is
    /// skipped whenever [`crate::ask::interactive`] says there is not one.
    fn gather(root: &Path, args: &InitArgs) -> Result<Self> {
        // Whether the config this run renders will actually be written. `place`
        // below decides the same thing for every file; this is the one case
        // where the decision has to be made early, because it is what makes two
        // of these questions worth asking.
        let fresh = args.force || !Config::path_in(root).exists();

        let provider = match args.provider {
            Some(provider) => provider,
            None => {
                // Straight off clap's own list, so the menu and `--provider`
                // cannot come to offer different things.
                let providers = <PlanningAgent as clap::ValueEnum>::value_variants();
                let notes: Vec<String> = providers
                    .iter()
                    .map(|provider| {
                        // Against the project's own root, so the note names the
                        // directory that will actually be written, separators
                        // and all.
                        let dir = provider.provider().skills_dir(root);
                        format!("skills go in {}", crate::platform::relative(root, &dir))
                    })
                    .collect();
                let menu: Vec<(&str, &str)> = providers
                    .iter()
                    .zip(&notes)
                    .map(|(provider, note)| (provider.name(), note.as_str()))
                    .collect();
                providers[crate::ask::choose("Which coding agent will you plan in?", &menu, 0)?]
            }
        }
        .provider();

        // Tracker and project key are asked, or answered outright, whenever a
        // config is going to be written (a fresh project) or `--tracker` was
        // given (whatever the project's age — see this task's own acceptance
        // criteria). `--project-key` alone, with no `--tracker` alongside it,
        // is dropped on an established project exactly as it always was: an
        // established project's issue integration is not replaced merely
        // because init was run to add another skill copy.
        if !fresh && args.tracker.is_none() {
            if args.project_key.is_some() {
                println!(
                    "  note  this project has a config already, so --project-key was not \
                     applied without --tracker — change it with `spoolway config set`, \
                     or re-run with --force to take the shipped config back"
                );
            }
            return Ok(Self {
                provider,
                tracker: Tracker::None,
                project_key: String::new(),
                tracker_touched: false,
            });
        }

        match Self::tracker(args, fresh)? {
            Some((tracker, project_key)) => Ok(Self {
                provider,
                tracker,
                project_key,
                tracker_touched: true,
            }),
            // A bare `--tracker` on an established project with nobody to
            // ask: review finding 4 — answering `none` here on the
            // person's behalf would silently clear a tracker the project
            // already had. There is no answer, so nothing is touched,
            // exactly as if the flag had been left out.
            None => {
                println!(
                    "  note  --tracker was given with no value and there is nobody to answer \
                     its picker, so [issue_tracking] was left exactly as it is — answer with \
                     `--tracker <value>` or run this at a terminal"
                );
                Ok(Self {
                    provider,
                    tracker: Tracker::None,
                    project_key: String::new(),
                    tracker_touched: false,
                })
            }
        }
    }

    /// The tracker `[issue_tracking]` names, and the project it files into —
    /// off the flags, or off a menu the provider question's own takes.
    /// `None` only for a bare `--tracker` on an established project
    /// (`fresh` false) with nobody to answer its picker — see
    /// [`Answers::tracker_touched`]'s own doc for why that case answers
    /// nothing rather than `none`.
    ///
    /// The note beside each entry is whether its command-line tool is on
    /// `PATH`, so choosing Jira without `acli` installed says so at the
    /// moment it is chosen rather than at the first hook that fails. `none`
    /// is the menu's default: a script with nobody to ask gets the same "no
    /// issue tracking" behaviour a project had before this existed, not a
    /// `gh` hook nobody asked for.
    fn tracker(args: &InitArgs, fresh: bool) -> Result<Option<(Tracker, String)>> {
        let tracker = match args.tracker.as_deref() {
            // A value answers outright — `--tracker github` — parsed by the
            // same case-insensitive rule clap's own `value_enum` uses, since
            // `InitArgs::tracker` is a plain string now (see its own doc).
            Some(raw) if !raw.is_empty() => <Tracker as clap::ValueEnum>::from_str(raw, true)
                .map_err(|message| {
                    anyhow::anyhow!("--tracker: {message} — pick `github`, `jira` or `none`")
                })?,
            // The flag absent entirely, or given bare (`--tracker` with no
            // value, filled in by `default_missing_value`): both fall
            // through to the picker below, whatever the project's age.
            _ => {
                if !crate::ask::interactive() {
                    // A fresh project has no existing answer to preserve, so
                    // nobody to ask still settles on `none` — the same
                    // scripted-default behaviour this always had. An
                    // established project does have one, and answering on
                    // its behalf is exactly the bug review finding 4 named.
                    return Ok(if fresh {
                        Some((Tracker::None, String::new()))
                    } else {
                        None
                    });
                }

                let trackers = <Tracker as clap::ValueEnum>::value_variants();
                let notes: Vec<String> = trackers
                    .iter()
                    .map(|tracker| match tracker.binary() {
                        Some(bin) => match which(bin) {
                            Some(path) => format!("{bin} on PATH, at {path}"),
                            None => format!("{bin} not on PATH — install it before dispatching"),
                        },
                        None => "hooks are still written; the table stays empty".to_string(),
                    })
                    .collect();
                let menu: Vec<(&str, &str)> = trackers
                    .iter()
                    .zip(&notes)
                    .map(|(tracker, note)| (tracker.name(), note.as_str()))
                    .collect();
                // `none` is the default here, unlike the coding-agent menu
                // above: a project a script scaffolds unattended should come
                // up with issue tracking off, not pointed at whichever
                // tracker happens to sit first on the menu.
                let default = trackers
                    .iter()
                    .position(|tracker| *tracker == Tracker::None)
                    .unwrap_or(0);
                trackers[crate::ask::choose("Which issue tracker do you use?", &menu, default)?]
            }
        };

        if tracker == Tracker::None {
            return Ok(Some((tracker, String::new())));
        }

        let project_key = match &args.project_key {
            Some(key) => key.clone(),
            None => crate::ask::line(
                "Which project does it file into?",
                "owner/repo for github, project key for jira",
            )?
            .unwrap_or_default(),
        };
        Ok(Some((tracker, project_key)))
    }

    /// Point a bundled pipeline at this project's one profile.
    ///
    /// The assets use Claude as their parseable default so the binary can use
    /// them as test fixtures before a project exists. Init owns the one textual
    /// substitution that turns that default into the selected identity.
    fn fill<'a>(&self, body: &'a str) -> std::borrow::Cow<'a, str> {
        if self.provider == Provider::Claude {
            body.into()
        } else {
            body.replace("agent: claude", &format!("agent: {}", self.provider.name()))
                .into()
        }
    }
}

/// How wide the path column is on the `stamped` line `init` prints, so that
/// the id lands in the same column as the value on every `wrote` row above
/// it rather than one space after a path of whatever length. Eleven
/// characters of `"  stamped  "` plus this is column 34, which is where the
/// task's own mockup puts it.
const STAMP_PATH_WIDTH: usize = 23;

/// Right-pads `path` to the column [`STAMP_PATH_WIDTH`] fixes — the column
/// every `wrote`/`stamped` row's value starts at — without ever letting a
/// long path swallow the separator outright.
///
/// `{:<width$}` alone pads only up to `width`; handed a path already that
/// wide or wider — the common git directory `init` stamps from inside a
/// linked worktree is an absolute path, easily past 23 characters — it adds
/// no padding at all, so the value that follows would run straight up
/// against the path with nothing between them. One explicit space is what a
/// path this long falls back to instead.
fn pad_to_value_column(path: &str) -> String {
    if path.chars().count() < STAMP_PATH_WIDTH {
        format!("{path:<width$}", width = STAMP_PATH_WIDTH)
    } else {
        format!("{path} ")
    }
}

/// One row of the mockup's report block: `verb` (`project`, `wrote`, `kept`
/// or `set`) against `what` — the resolved root for `project`, a path
/// relative to the project root for `wrote`/`kept`, or — for `set` — a
/// `key = value` pair. Every verb is left-padded to nine columns — `project`
/// plus two spaces, `wrote` plus four, `kept` plus five, `set` plus six — so
/// all four line up whichever one a row starts with.
fn report_row(verb: &str, what: &str) -> String {
    format!("  {verb:<9}{what}")
}

/// The mockup's second `bound` row: how much was already sitting under a
/// home a checkout was just pointed at by name — the queue and archive
/// task counts, whether a usage ledger exists, and how many worktrees are
/// cut. `--adopt` prints this because it is the one case that can bind a
/// checkout to a home carrying real state a person did not just watch
/// `init` create empty; `migrate-legacy-home`'s own move prints it for the
/// same reason, against the home it just moved.
///
/// Worktrees are counted at `root`'s own configured
/// `dispatch.worktree_root` when it has one, and at `home`'s own default
/// location otherwise — reading `root`'s tracked `config.toml` directly
/// rather than going through `Repo::discover`, which is not safe to call
/// mid-`init` (before the binding this call is itself establishing exists)
/// and not yet safe mid-migration either (`crate::repo::migrate_legacy_home`
/// calls this before the move it is reporting on has finished settling into
/// `Repo::discover`'s own accessors). A config that fails to load, or a
/// configured root `crate::mux::worktree_root` cannot resolve, falls back
/// to the default location rather than erroring out of an inventory line
/// that only ever reports, never fails a bind.
pub(crate) fn home_inventory_line(root: &Path, home: &Path) -> String {
    let count_docs = |dir: std::path::PathBuf| -> usize {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("md"))
            .count()
    };
    let queue = count_docs(home.join(crate::config::QUEUE_DIR));
    let archive = count_docs(home.join(crate::config::ARCHIVE_DIR));
    let ledger = std::fs::metadata(home.join(crate::usage::LEDGER_FILE))
        .map(|meta| meta.len() > 0)
        .unwrap_or(false);
    let worktree_dir = Config::load_tracked(root)
        .ok()
        .filter(|config| !config.dispatch.worktree_root.trim().is_empty())
        .and_then(|config| crate::mux::worktree_root(root, &config.dispatch).ok())
        .unwrap_or_else(|| home.join("worktrees"));
    let worktrees = std::fs::read_dir(worktree_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .count();
    format!(
        "queue {queue} . archive {archive} . ledger{} . worktrees {worktrees}",
        if ledger { "" } else { " (none)" }
    )
}

pub fn init(root: &Path, args: &InitArgs) -> Result<()> {
    // Before anything is written or asked, because this is the one command a
    // person runs without knowing yet what they have got hold of.
    print!("{}", crate::status::banner("setting up a project"));

    // A key pressed in a herdr pane opens this popup in whatever directory
    // herdr handed it — usually the one you are looking at, but there is no
    // way to be certain of that from in here. So every caller gets told the
    // root `init_root` resolved before anything below writes to it, and a
    // real terminal on the other end waits for a yes before going on: a
    // path you do not recognise is the whole of the check.
    //
    // The question goes through `crate::ask::confirm` like every other one
    // in this file, so a run with nobody to answer takes its declared
    // default rather than hanging — and that default is `false`, decline.
    // A silent `init` therefore writes nothing unless it was told to: a
    // script or CI runner that means it passes `--yes`, the same shape
    // `spoolway herdr bind --yes` already uses. An `init` reached from a
    // herdr keybinding never passes it, because the whole point of the
    // popup is that nobody has confirmed which project it opened in.
    println!();
    println!("{}", report_row("project", &root.display().to_string()));
    if !args.yes && !crate::ask::confirm("Set up this project?", false)? {
        return Ok(());
    }

    // A repeat run is how a project adds another provider's skills. Keep that
    // successful outcome distinct from creating (or deliberately replacing)
    // the project's scaffold. Read before anything below writes `config.toml`.
    let already_initialized = Config::path_in(root).exists() && !args.force;

    // A project's home is keyed off an id stamped into its own common git
    // directory, checked against that home's own record of which checkout
    // it belongs to — `crate::repo::bind` and friends, the whole of the
    // `binding-record` task. Every other command reaches the same check
    // through `Repo::discover`; `init` is one of only two things allowed to
    // write a binding *over* one that already disagrees, so it calls
    // straight into the flags that do that rather than `Repo::discover`
    // itself.
    //
    // `--take-over` is accepted and otherwise does nothing: the collision it
    // used to resolve (two checkouts sharing one *basename*) cannot happen
    // once a home is keyed by id instead, and the one case that looks like
    // it now — a home whose recorded checkout is simply gone — settles
    // itself without asking, per acceptance criterion 2 of that task.
    //
    // `already_stamped` is read before any of the three calls below run,
    // since the ordinary one may be the very call that mints this
    // checkout's id for the first time now — criterion 7, "no stamp where
    // nothing records it binds itself once", no `spoolway init` required
    // first any more. It is what lets the mockup's own "stamped" line,
    // printed further down at its own position, tell a checkout that was
    // freshly minted apart from a repeat run that only read its id back.
    let stamp_path = crate::repo::id_file_path(root)?;
    let already_stamped = stamp_path.as_deref().is_some_and(|path| path.exists());
    if let Some(name) = &args.adopt {
        let home = crate::repo::adopt(root, name)?;
        println!("  bound  {}  ->  {}/", root.display(), home.display());
        println!("         {}", home_inventory_line(root, &home));
    } else if args.new_id {
        let home = crate::repo::restamp(root)?;
        println!(
            "  bound    {}  ->  {}/ (new id)",
            root.display(),
            home.display()
        );
    } else {
        // The ordinary case: nothing to say unless the binding itself had
        // something to record — a moved checkout prints its own one line
        // from inside `bind` (acceptance criterion 2); a fresh one, silent
        // criterion 7, stays silent here too.
        crate::repo::bind(root)?;
    }
    let stamped_line = (!already_stamped)
        .then_some(stamp_path)
        .flatten()
        .filter(|path| path.exists())
        .and_then(|path| std::fs::read_to_string(&path).ok().map(|id| (path, id)))
        .map(|(path, id)| {
            format!(
                "  stamped  {}{}",
                pad_to_value_column(&relative(root, &path)),
                id.trim()
            )
        });

    let answers = Answers::gather(root, args)?;

    let state = root.join(STATE_DIR);
    let mut config = Config::default();
    let profile = answers.provider.name();
    // A fresh scaffold has one identity, not a menu of hypothetical profiles:
    // that makes the first answer sufficient to understand every agent name
    // written below. The defaults contain this profile by construction.
    config.agents.retain(|name, _| name == profile);
    config.unattended.blocked_agent = profile.to_string();
    // An unblocker is an agent step too. Leaving both knobs blank avoids
    // silently choosing more for it than init chooses for declared steps.
    config.unattended.blocked_model.clear();
    config.unattended.blocked_effort.clear();
    // Blank on a config that is staying as it is, which matches the
    // rendered default already sitting in `config` unmodified — writing it
    // through here rather than skipping it is one fewer branch to keep in
    // step with `fresh`.
    config.issue_tracking.hook = answers.tracker.hook_name();
    config.issue_tracking.project_key = answers.project_key.clone();
    let config = config;

    // Only the tracked control plane. `queue/` and `archive/` moved out of the
    // checkout — see `crate::repo::Repo::home` — and are created silently by
    // the accessors that resolve them, the first time anything asks for one.
    std::fs::create_dir_all(state.join("prompts"))
        .with_context(|| format!("creating {}", state.join("prompts").display()))?;

    // Whether `place` actually wrote `path`, so `wrote_rows` and `wrote_any`
    // below report only what a run actually did — a repeat `init` that adds
    // nothing new reports `kept` for everything it considered, the same way
    // `claim` and the stamped line already say nothing on a repeat run.
    // `wrote_rows` collects one `kept`/`wrote` row per file `place` is asked
    // about, in call order, rather than an aggregate count: acceptance
    // criterion 3 asks for every file it considered, and a project adding
    // its own pipeline or prompt directory later means a fixed set of
    // aggregate buckets could not have named it anyway.
    let mut wrote_rows: Vec<String> = Vec::new();
    let mut wrote_any = false;
    let mut place =
        |path: std::path::PathBuf, rel: &str, contents: &[u8], exec: bool| -> Result<bool> {
            if path.exists() && !args.force {
                wrote_rows.push(report_row("kept", rel));
                return Ok(false);
            }
            write_atomic(&path, contents)?;
            if exec {
                // A hook is invoked as a bare command line — see
                // `crate::tracking::hook_path` — so it needs the execute bit
                // itself; nothing else `init` writes is ever run rather than
                // read.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = std::fs::metadata(&path)?.permissions();
                    perms.set_mode(0o755);
                    std::fs::set_permissions(&path, perms)?;
                }
            }
            wrote_rows.push(report_row("wrote", rel));
            Ok(true)
        };

    wrote_any |= place(
        Config::path_in(root),
        ".spoolway/config.toml",
        config
            .render()
            .context("rendering default config")?
            .as_bytes(),
        false,
    )?;
    // One file per pipeline, named for the pipeline it holds. A project adds
    // its own by writing another file here and nothing else.
    for (name, body) in crate::pipeline::BUILTIN_PIPELINES {
        let path = Pipelines::file_in(root, name);
        let rel = relative(root, &path);
        wrote_any |= place(path, &rel, answers.fill(body).as_bytes(), false)?;
    }
    // Written whole, and never looked at again. A prompt is the project's from
    // the moment `init` finishes: no sync rewrites one, so nothing here has to
    // be a shape a later binary can still find its way around in.
    for prompt in assets::PROMPTS {
        let dir = state.join("prompts").join(prompt.name);
        let path = dir.join(assets::PROMPT_FILE);
        let rel = relative(root, &path);
        wrote_any |= place(path, &rel, prompt.body.as_bytes(), false)?;
        // A prompt's belongings follow its prose: written once, never updated,
        // and the project's to restyle from here on. No setting names them —
        // the prompt that fills them is the only thing that reads them.
        for (name, body) in prompt.assets {
            let path = dir.join(assets::PROMPT_ASSETS).join(name);
            let rel = relative(root, &path);
            wrote_any |= place(path, &rel, body.as_bytes(), false)?;
        }
    }
    // One task skeleton per shipped pipeline, named for the pipeline that takes
    // it. A project adds a pipeline's shape by writing a file beside these, and
    // one that writes nothing takes `default`.
    for (name, skeleton) in assets::TASK_TEMPLATES {
        let path = root
            .join(crate::config::TASK_TEMPLATES_DIR)
            .join(format!("{name}.md"));
        let rel = relative(root, &path);
        wrote_any |= place(path, &rel, skeleton.as_bytes(), false)?;
    }
    // The two ticket-body templates a tracker hook renders and hands to its
    // own `gh`/`acli` call — seeded once, like a task skeleton, and never
    // looked at again by `sync`. A project with neither file written gets
    // a single line naming the task instead of this prose; see
    // `crate::task_template::resolve_tracking`.
    for (name, body) in assets::TRACKING_TEMPLATES {
        let path = root
            .join(crate::config::TRACKING_TEMPLATES_DIR)
            .join(format!("{name}.md"));
        let rel = relative(root, &path);
        wrote_any |= place(path, &rel, body.as_bytes(), false)?;
    }
    // The hook scripts every project gets, whichever tracker it answered —
    // switching later is a `spoolway config set issue_tracking.hook` away,
    // not a second `init`.
    for (name, body) in assets::HOOK_SCRIPTS {
        let path = root.join(".spoolway/hooks").join(name);
        let rel = relative(root, &path);
        wrote_any |= place(path, &rel, body.as_bytes(), true)?;
    }
    // The workflow that closes a mirrored issue once its pull request
    // merges, written only for github — see `.github/workflows/
    // spoolway-issues.yml` in this repository, unchanged. Driven by the
    // tracker actually in force, not only one just answered: a bare repeat
    // `init` never touches `answers.tracker` (see `Answers::tracker_touched`)
    // but still has to report `kept` for a workflow an earlier run already
    // wrote, per the mockup's own bare-rerun transcript.
    let github_in_force = if answers.tracker_touched {
        answers.tracker == Tracker::Github
    } else {
        Config::load_tracked(root)
            .map(|existing| existing.issue_tracking.hook == Tracker::Github.hook_name())
            .unwrap_or(false)
    };
    if github_in_force {
        let path = root.join(".github/workflows/spoolway-issues.yml");
        let rel = relative(root, &path);
        wrote_any |= place(path, &rel, assets::GITHUB_ISSUE_WORKFLOW.as_bytes(), false)?;
    }
    // No plan skeleton here any more. spoolway-plan carries its own, under the
    // skill's own `assets/`, and writes a self-contained page with it — there
    // is nothing left for `init` to place in the project.

    // `--tracker`, given bare or with a value, answers `[issue_tracking]`
    // even on a project that already has a `config.toml` — `place` above
    // leaves that file `kept` rather than rewriting it wholesale, so the
    // two keys the tracker question settles are edited into it directly,
    // the same narrow edit `spoolway config set` itself makes.
    if already_initialized && answers.tracker_touched {
        let existing = Config::load_tracked(root)?;
        let updated = crate::confkv::set(
            &existing,
            "issue_tracking.hook",
            &config.issue_tracking.hook,
        )?;
        updated.save_key(root, "issue_tracking.hook")?;
        wrote_rows.push(report_row(
            "set",
            &format!(
                "issue_tracking.hook = {}",
                crate::confkv::get(&updated, "issue_tracking.hook")?
            ),
        ));
        wrote_any = true;
        if answers.tracker != Tracker::None {
            let updated =
                crate::confkv::set(&updated, "issue_tracking.project_key", &answers.project_key)?;
            updated.save_key(root, "issue_tracking.project_key")?;
            wrote_rows.push(report_row(
                "set",
                &format!(
                    "issue_tracking.project_key = {}",
                    crate::confkv::get(&updated, "issue_tracking.project_key")?
                ),
            ));
        }
    }

    // Not written through `place` either: this only ever removes, and never
    // creates a `.gitignore` a project did not already have. See
    // [`crate::gitignore`] — runtime state moved out of the checkout, so
    // there are no rules left to write, only an old block to take back out.
    let mut notes = Vec::new();
    let shown = relative(root, &crate::gitignore::file(root));
    match crate::gitignore::remove(root, false)? {
        crate::gitignore::Removed::Gone => {
            notes.push(format!("  removed {shown} (spoolway's rules are gone)"))
        }
        crate::gitignore::Removed::Absent => {}
        crate::gitignore::Removed::Unterminated => notes.push(format!(
            "  !       {shown}: `{}` with no `{}` — left alone, restore the marker \
             or delete the block",
            assets::IGNORE_BEGIN,
            assets::IGNORE_END
        )),
    }

    crate::usage::registry::register(root);

    for row in &wrote_rows {
        println!("{row}");
    }
    for note in &notes {
        println!("{note}");
    }
    if let Some(line) = &stamped_line {
        println!("{line}");
    }
    // Resolved once and shared: `write_stamp` below records this checkout's
    // fact against it. Best-effort, and never printed: a project's home
    // resolving is not this command's own concern to fail over, and `bind`
    // above has already settled it for every ordinary case.
    let home = crate::mux::project_home(root).ok();

    // The skills, in the provider's own convention. Run from here rather than
    // suggested, because "and now run this other command" is the manual step
    // this exists to remove — and a project that skipped it had skills that
    // were shipped, documented, and never installed.
    let installed = crate::install::install(root, answers.provider, args.force)?;
    crate::install::report(installed);
    // A fresh (or freshly `--force`d) project is, by construction, exactly
    // what this binary would write — so it is stamped the same fact
    // `spoolway sync` would have recorded had it run here instead: this
    // checkout, at this binary's version, matching what it would still
    // write today.
    if let Some(home) = &home {
        let _ = crate::sync::write_stamp(home, root);
    }
    // The mockup above ends its transcript at `crate::install::report`'s
    // line, but it is an excerpt of the run this task changes, not a
    // contract for every line `init` has ever printed: it also elides the
    // tracker question and the claim note. These two closing lines are
    // documented behaviour — `docs/cli-reference.md` and
    // `docs/installation.md` both promise a person exactly them — and the
    // one place `init` says what to do next, so they stay on stdout where
    // they were. Nothing in this task's acceptance criteria asks about
    // them.
    if !already_initialized {
        println!("Project initialized successfully.");
        println!(
            "Set model and effort on every agent step in .spoolway/pipelines/*.yml before \
             dispatching."
        );
    } else if answers.tracker_touched && answers.tracker != Tracker::None {
        // The one closing line an established project's tracker question
        // gets: it never sees "Project initialized successfully." above,
        // and `--tracker` just changed something real about it.
        println!();
        println!("issue tracking is on.");
    } else if !wrote_any {
        // A bare repeat run that found nothing missing — every row above
        // read `kept` — gets a closing line of its own too, rather than
        // trailing off silently into the install report's own output.
        println!();
        println!("nothing to install.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scratch `$HOME` every test below runs `init` under.
    ///
    /// `init` binds this checkout under the real `~/.spoolway/`, so running
    /// it unguarded in a test would write into whoever is running the
    /// suite's actual home directory. Binding is keyed by the id stamped
    /// into `root`'s own `.git`, not by basename, so two scratch roots
    /// sharing a real machine's `~/.spoolway/` would not actually collide
    /// any more — but isolating each test's home here still keeps its
    /// writes out of a person's real state, which matters regardless. One
    /// scratch home per root keeps every call in one test, including a
    /// deliberate second `init`, agreeing about where `root` is bound.
    fn home_for(root: &Path) -> std::path::PathBuf {
        root.parent().unwrap().join(format!(
            "{}-home",
            root.file_name().unwrap().to_string_lossy()
        ))
    }

    /// `InitArgs::default()` with the opening confirmation already
    /// answered — what a script that means it passes as `--yes`.
    ///
    /// Every test below that expects a project on disk starts from this
    /// rather than from `InitArgs::default()`, because a default `InitArgs`
    /// with nobody to answer takes `Set up this project?`'s own default,
    /// which is no: it writes nothing, which is precisely what
    /// [`init_with_nobody_to_ask_and_no_yes_writes_nothing`] asserts.
    fn confirmed() -> InitArgs {
        InitArgs {
            yes: true,
            ..InitArgs::default()
        }
    }

    /// `init`, with `$HOME` pointed at `root`'s own scratch home. See
    /// [`home_for`].
    fn run_init(root: &Path, args: &InitArgs) -> Result<()> {
        crate::platform::test_home::with_home(&home_for(root), || init(root, args))
    }

    /// A project on disk after `init`, in a directory of its own.
    fn scaffold(name: &str, args: &InitArgs) -> std::path::PathBuf {
        let root = crate::scratch::root(&format!("init-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        run_init(&root, args).expect("init");
        root
    }

    /// Every pipeline file `init` wrote, concatenated. What a step names is
    /// the question every test below asks of them, and which file it is in is
    /// not.
    fn pipelines_on_disk(root: &Path) -> String {
        let dir = root.join(STATE_DIR).join("pipelines");
        let mut all = String::new();
        for entry in std::fs::read_dir(&dir).expect("pipelines dir").flatten() {
            all.push_str(&std::fs::read_to_string(entry.path()).unwrap());
        }
        assert!(!all.is_empty(), "init wrote no pipelines");
        all
    }

    /// With no terminal — every script and CI run — init asks nothing and
    /// chooses Claude, while still leaving the per-step choices visible.
    ///
    /// Worth a test of its own because the failure is not a wrong file, it is a
    /// hang: an `init` that reads stdin in a suite that gives it none waits
    /// forever, and the suite reports a timeout somewhere unrelated.
    #[test]
    fn init_with_nobody_to_ask_takes_every_default() {
        let root = scaffold("defaults", &confirmed());

        let config = Config::load(&root).unwrap();
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents["claude"].kind, "claude");
        assert_eq!(config.unattended.blocked_agent, "claude");
        assert!(config.unattended.blocked_model.is_empty());

        let pipelines = pipelines_on_disk(&root);
        assert!(!pipelines.contains("agent: pi"), "{pipelines}");
        assert!(pipelines.contains("agent: claude"), "{pipelines}");
        assert!(pipelines.contains("model: \"\""), "{pipelines}");
        assert!(pipelines.contains("effort: \"\""), "{pipelines}");

        // And the default provider's skills, installed rather than suggested.
        assert!(root.join(".claude").join("skills").is_dir());
    }

    /// `init` writes the same sync stamp `spoolway sync` would on success —
    /// a fresh project is, by construction, exactly what this binary would
    /// write, so the stamp says so without a sync ever having run.
    #[test]
    fn init_writes_a_sync_stamp() {
        let root = scaffold("sync-stamp", &confirmed());
        let stamp = crate::platform::test_home::with_home(&home_for(&root), || {
            let home = crate::mux::project_home(&root).unwrap();
            crate::sync::read_stamp(&home, &root)
        });
        let (version, fingerprint) = stamp.expect("init records a sync stamp");
        assert_eq!(version, crate::release::current());
        assert!(!fingerprint.is_empty());
    }

    /// `spoolway init` stamps a project's home off its own `.git`, and a
    /// directory with none is refused for that reason — not silently handed
    /// a basename-keyed home it could never move or rename without losing.
    #[test]
    fn init_refuses_a_directory_with_no_git_repository() {
        let root = crate::scratch::root("init-no-git");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let err = run_init(&root, &confirmed()).expect_err("no .git here at all");
        let said = format!("{err:#}");
        assert!(
            said.contains("git repository"),
            "the refusal names the actual reason: {said}"
        );
    }

    /// A real git repository whose stamp cannot be written — the common git
    /// directory itself is read-only — must be refused for that reason, not
    /// reported as "no git repository behind it", which would be a lie about
    /// a project that is a real, ordinary git repository.
    #[test]
    fn init_names_a_real_write_failure_rather_than_claiming_no_git_repository() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::scratch::root("init-write-failure");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        let git_dir = root.join(".git");
        let mut perms = std::fs::metadata(&git_dir).unwrap().permissions();
        perms.set_mode(0o500); // read + execute, no write

        std::fs::set_permissions(&git_dir, perms.clone()).unwrap();
        let err = run_init(&root, &confirmed());

        // Restore before asserting, so a failed assertion still leaves this
        // test's own directory cleanup able to remove it.
        perms.set_mode(0o700);
        std::fs::set_permissions(&git_dir, perms).unwrap();

        let said = format!("{:#}", err.expect_err("a read-only .git cannot be stamped"));
        assert!(
            !said.contains("has no git repository behind it"),
            "a real write failure must not be reported as no repository: {said}"
        );
    }

    /// The id the mockup shows: six characters, stamped into `.git/`.
    #[test]
    fn init_stamps_an_id_into_the_common_git_directory() {
        let root = crate::scratch::root("init-stamp");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);

        run_init(&root, &confirmed()).expect("init");

        let on_disk = std::fs::read_to_string(root.join(".git").join("spoolway-id")).unwrap();
        let id = on_disk.trim();
        assert_eq!(id.len(), 6, "{id}");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "{id}"
        );
    }

    /// The planning-agent answer supplies the profile, every step reference,
    /// the unattended unblocker, and the skills convention as one decision.
    #[test]
    fn the_provider_answer_reaches_every_file_it_controls() {
        let root = scaffold(
            "answered",
            &InitArgs {
                provider: Some(PlanningAgent::Codex),
                ..confirmed()
            },
        );

        let config = Config::load(&root).unwrap();
        assert_eq!(config.agents.len(), 1);
        assert!(config.agents.contains_key("codex"));
        assert_eq!(config.unattended.blocked_agent, "codex");
        assert!(config.unattended.blocked_model.is_empty());
        assert!(config.unattended.blocked_effort.is_empty());

        let pipelines = pipelines_on_disk(&root);
        assert!(pipelines.contains("agent: codex"), "{pipelines}");
        assert!(!pipelines.contains("agent: claude"), "{pipelines}");
        assert!(!pipelines.contains("agent: pi"), "{pipelines}");

        assert!(root.join(".agents").join("skills").is_dir());
        assert!(
            !root.join(".claude").exists(),
            "only one provider was asked for"
        );
    }

    /// `init` run again is a project asking for skills, not for its settings
    /// back. The config it has been running on for a month is not rewritten,
    /// and — the point of running it again — the new provider's skills land
    /// anyway.
    #[test]
    fn a_second_init_installs_skills_without_touching_the_config() {
        let root = scaffold(
            "twice",
            &InitArgs {
                provider: Some(PlanningAgent::Claude),
                ..confirmed()
            },
        );
        let before = std::fs::read_to_string(Config::path_in(&root)).unwrap();

        run_init(
            &root,
            &InitArgs {
                provider: Some(PlanningAgent::Codex),
                ..confirmed()
            },
        )
        .expect("second init");

        assert_eq!(
            before,
            std::fs::read_to_string(Config::path_in(&root)).unwrap(),
            "the second run must not rewrite a config the project has been using"
        );
        assert!(root.join(".agents").join("skills").is_dir());
        assert!(root.join(".claude").join("skills").is_dir());
    }

    /// Everything that resolves a prompt path has to agree with what `init`
    /// wrote. Each caller used to build the path itself, so moving prompts into
    /// directories left them looking for a file init no longer writes.
    ///
    /// So this asserts the two halves against each other rather than against a
    /// literal: init the real thing, then resolve every prompt the builtin
    /// pipelines name, through the same lookup the dispatcher uses.
    #[test]
    fn every_prompt_init_writes_is_one_the_rest_of_the_binary_can_find() {
        let root = crate::scratch::root("commands-prompt-paths");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);

        run_init(&root, &confirmed()).expect("init");
        let home = root.join(".home");
        let repo = Repo {
            checkout: root.clone(),
            root,
            config: Config::default(),
            home,
        };

        let pipelines = Pipelines::builtin();

        // Every step a shipped pipeline runs has prose on disk after `init`.
        for pipeline in pipelines.pipelines.values() {
            for step in &pipeline.steps {
                if step.kind() != StepKind::Agent {
                    continue;
                }
                let name = step.prompt_name();
                let path = crate::prompt::path_for(&repo, name);
                assert!(
                    path.is_file(),
                    "step `{}` resolves prompt `{name}` to {}, which init did not write",
                    step.id,
                    path.display()
                );
            }
        }

        // `entries` is the listing half — it must see the same set, or
        // `prompt list` disagrees with what actually runs.
        let listed = crate::prompt::entries(&repo).expect("entries");
        assert_eq!(
            listed.len(),
            crate::assets::PROMPTS.len(),
            "listed {:?}",
            listed.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }

    /// `--new-id` mints a checkout a fresh id even though it already carries
    /// one, and moves it into the fresh home that id keys — the escape
    /// hatch for two checkouts caught sharing one id (acceptance criterion 3
    /// of `binding-record`).
    #[test]
    fn new_id_mints_a_fresh_id_and_a_fresh_home() {
        let root = crate::scratch::root("init-new-id");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        run_init(&root, &confirmed()).expect("first init");
        let before = std::fs::read_to_string(root.join(".git").join("spoolway-id")).unwrap();

        run_init(
            &root,
            &InitArgs {
                new_id: true,
                ..confirmed()
            },
        )
        .expect("--new-id");
        let after = std::fs::read_to_string(root.join(".git").join("spoolway-id")).unwrap();

        assert_ne!(before.trim(), after.trim(), "a fresh id was not minted");
        let home = crate::platform::test_home::with_home(&home_for(&root), || {
            crate::mux::project_home(&root)
        })
        .unwrap();
        assert!(
            home.join("project.toml").is_file(),
            "the fresh home is bound to the checkout"
        );
    }

    /// `--adopt <name>` binds a checkout to the home already sitting under
    /// that name — even one that already recorded a different checkout —
    /// the other escape hatch, for a home whose checkout is gone but which
    /// nothing has restamped a new one to point at yet (criterion 4). The
    /// name given is the mockup's own shape, `<label>-<id>`, not the bare
    /// id alone — `spoolway init --adopt api-8w4r2c`.
    #[test]
    fn adopt_binds_to_the_home_already_sitting_under_that_name() {
        let base = crate::scratch::root("init-adopt");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let home_root = base.join("home");

        let original = base.join("original");
        std::fs::create_dir_all(&original).unwrap();
        crate::scratch::git_init(&original, &["-b", "plan/demo"]);
        crate::platform::test_home::with_home(&home_root, || init(&original, &confirmed()))
            .expect("stamp and bind the original checkout");
        let id = std::fs::read_to_string(original.join(".git").join("spoolway-id"))
            .unwrap()
            .trim()
            .to_string();
        let home = crate::platform::test_home::with_home(&home_root, || {
            crate::mux::project_home(&original)
        })
        .unwrap();
        let name = home.file_name().unwrap().to_str().unwrap().to_string();
        assert!(name.ends_with(&id), "{name}");

        // The original checkout is gone; a fresh one adopts its home by name.
        std::fs::remove_dir_all(&original).unwrap();
        let fresh = base.join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        crate::scratch::git_init(&fresh, &["-b", "plan/demo"]);

        crate::platform::test_home::with_home(&home_root, || {
            init(
                &fresh,
                &InitArgs {
                    adopt: Some(name.clone()),
                    ..confirmed()
                },
            )
        })
        .expect("--adopt");

        let stamped = std::fs::read_to_string(fresh.join(".git").join("spoolway-id"))
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(
            stamped, id,
            "the adopting checkout carries the id the named home is keyed on"
        );

        // The real regression: `fresh`'s own basename is not `original`'s,
        // so if `adopt` left the checkout's label alone, the very next
        // resolution would key off `fresh-<id>` — a home nothing ever
        // wrote — rather than the one just adopted. Only a `Repo::discover`
        // that lands back on the adopted home proves the label was
        // actually overwritten to match it.
        let repo = crate::platform::test_home::with_home(&home_root, || {
            crate::repo::Repo::discover(&fresh)
        })
        .expect("the adopted home resolves on the very next command");
        assert_eq!(
            repo.home, home,
            "discovery after --adopt must land back on the home just adopted, not a home \
             keyed off this checkout's own current basename"
        );
    }

    /// The mockup's own second `bound` line, with real state under the
    /// home to count — an empty home (the common case, an ordinary `init`)
    /// is not enough on its own to prove the counters, only that they
    /// don't crash on nothing.
    #[test]
    fn home_inventory_line_counts_what_is_actually_there() {
        let home = crate::scratch::root("home-inventory");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("queue")).unwrap();
        std::fs::create_dir_all(home.join("archive")).unwrap();
        std::fs::create_dir_all(home.join("worktrees").join("task-a")).unwrap();
        std::fs::create_dir_all(home.join("worktrees").join("task-b")).unwrap();
        for name in ["one.md", "two.md"] {
            std::fs::write(home.join("queue").join(name), "---\n---\n").unwrap();
        }
        std::fs::write(home.join("archive").join("done.md"), "---\n---\n").unwrap();
        // A stray non-task file must not be counted as a queued document.
        std::fs::write(home.join("queue").join("notes.txt"), "not a task").unwrap();
        std::fs::write(home.join("usage.jsonl"), "{}\n").unwrap();

        // No `.spoolway/config.toml` under this root at all, exactly like a
        // bare `root` at `--adopt` time before `init` has written one —
        // `dispatch.worktree_root` reads as unconfigured, so the count
        // falls back to `home`'s own `worktrees/`.
        let root = crate::scratch::root("home-inventory-root");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        assert_eq!(
            home_inventory_line(&root, &home),
            "queue 2 . archive 1 . ledger . worktrees 2"
        );

        // Empty, as an ordinary fresh `init` leaves it: every counter reads
        // zero, and the ledger is reported absent rather than crashing on
        // directories that do not exist yet.
        let fresh = crate::scratch::root("home-inventory-empty");
        let _ = std::fs::remove_dir_all(&fresh);
        std::fs::create_dir_all(&fresh).unwrap();
        assert_eq!(
            home_inventory_line(&root, &fresh),
            "queue 0 . archive 0 . ledger (none) . worktrees 0"
        );
    }

    /// A project that points `dispatch.worktree_root` somewhere other than
    /// its home's own default location must be counted there, not against
    /// `home`'s own (empty) `worktrees/` — the bug the second review
    /// caught: the mockup's inventory line could read `worktrees 0` while
    /// worktrees plainly existed, because the count never looked anywhere
    /// but the default.
    #[test]
    fn home_inventory_line_counts_a_configured_worktree_root() {
        let root = crate::scratch::root("home-inventory-configured-root");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(crate::config::STATE_DIR)).unwrap();

        let elsewhere = crate::scratch::root("home-inventory-configured-worktrees");
        let _ = std::fs::remove_dir_all(&elsewhere);
        std::fs::create_dir_all(elsewhere.join("task-a")).unwrap();
        std::fs::write(
            Config::path_in(&root),
            format!(
                "[dispatch]\nworktree_root = {:?}\n",
                elsewhere.display().to_string()
            ),
        )
        .unwrap();

        let home = crate::scratch::root("home-inventory-configured-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // The home's own default location is left empty, on purpose: a
        // count that fell back to it despite the config above would still
        // read zero.
        std::fs::create_dir_all(home.join("worktrees")).unwrap();

        assert_eq!(
            home_inventory_line(&root, &home),
            "queue 0 . archive 0 . ledger (none) . worktrees 1"
        );
    }

    /// A name that is not a plain directory component must be refused
    /// before any path is built from it — the acceptance criterion that
    /// something able to escape `~/.spoolway/` (a separator, a `..`) never
    /// reaches `state_root().join(...)`.
    #[test]
    fn adopt_refuses_a_name_that_would_escape_the_state_root() {
        let root = crate::scratch::root("init-adopt-bad-name");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);

        let err = crate::platform::test_home::with_home(&home_for(&root), || {
            init(
                &root,
                &InitArgs {
                    adopt: Some("../../evil".to_string()),
                    ..confirmed()
                },
            )
        })
        .expect_err("a name that could escape ~/.spoolway/ must be refused");
        assert!(
            format!("{err:#}").contains("not a plain directory name"),
            "{err:#}"
        );
        // And nothing was built from it: no directory escaping the scratch
        // home's own `.spoolway/` exists.
        assert!(!home_for(&root).join("..").join("evil").exists());
    }

    /// `name` itself passes [`crate::tracking::is_bare_filename`] — it is
    /// one plain path component — but splitting it on its last `-` can
    /// still leave a label half that is not: `-abc123` splits into an
    /// empty label and the id `abc123`, and an empty label written to the
    /// checkout's `spoolway-label` file is exactly the kind of value
    /// [`crate::mux::project_home`] cannot key a resolvable path off. That
    /// must be refused before `stamp_over` ever writes it, not discovered
    /// the next time the checkout is used.
    #[test]
    fn adopt_refuses_a_name_whose_label_half_is_unusable() {
        let root = crate::scratch::root("init-adopt-bad-label");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);

        let home_root = home_for(&root);
        let bad_home = home_root.join(".spoolway").join("-abc123");
        std::fs::create_dir_all(&bad_home).unwrap();

        let err = crate::platform::test_home::with_home(&home_root, || {
            init(
                &root,
                &InitArgs {
                    adopt: Some("-abc123".to_string()),
                    ..confirmed()
                },
            )
        })
        .expect_err("a name whose label half is empty must be refused");
        let message = format!("{err:#}");
        assert!(message.contains("-abc123"), "{message}");
        // The rejected name is exactly what was just handed to `--adopt`,
        // so telling the person to run the same command with the same name
        // again cannot resolve anything — the guidance has to point at
        // renaming the home, or at the other escape hatch, `--new-id`.
        assert!(
            !message.contains("Run `spoolway init --adopt <name>` again"),
            "{message}"
        );
        assert!(message.to_lowercase().contains("rename"), "{message}");
        assert!(message.contains("--new-id"), "{message}");

        // Nothing was written: the checkout was left unstamped.
        assert!(!root.join(".git").join("spoolway-id").exists());
    }

    /// Answering `github` writes the hook name and the project key into
    /// `[issue_tracking]`, and the executable script it names actually lands
    /// on disk, chmod'd so `crate::tracking`'s hook runner can exec it
    /// directly rather than being handed a path with no execute bit. Every
    /// script is written whichever tracker was answered, not only the
    /// chosen one — switching later is a config edit, not a second `init`.
    #[test]
    fn answering_github_writes_the_hook_and_project_key() {
        let root = scaffold(
            "tracker-github",
            &InitArgs {
                tracker: Some("github".into()),
                project_key: Some("acme/app".into()),
                ..confirmed()
            },
        );

        let config = std::fs::read_to_string(Config::path_in(&root)).unwrap();
        let github = Tracker::Github.hook_name();
        let jira = Tracker::Jira.hook_name();
        assert!(config.contains(&format!("hook = \"{github}\"")), "{config}");
        assert!(config.contains("project_key = \"acme/app\""), "{config}");

        let hook = root.join(".spoolway/hooks").join(&github);
        assert!(hook.is_file());
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&hook).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "the hook must be executable");
        }
        assert!(root.join(".spoolway/hooks").join(&jira).is_file());
    }

    /// Answering `none` still writes every hook script — the whole point of
    /// writing all of them regardless of the answer — but the table stays
    /// empty, which is what turns issue tracking off in `crate::tracking`. A
    /// `--project-key` given alongside `none` is dropped, not half-applied.
    #[test]
    fn answering_none_writes_scripts_but_leaves_the_table_empty() {
        let root = scaffold(
            "tracker-none",
            &InitArgs {
                tracker: Some("none".into()),
                project_key: Some("should-be-ignored".into()),
                ..confirmed()
            },
        );

        let config = std::fs::read_to_string(Config::path_in(&root)).unwrap();
        assert!(config.contains("hook = \"\""), "{config}");
        assert!(config.contains("project_key = \"\""), "{config}");
        let hooks = root.join(".spoolway/hooks");
        assert!(hooks.join(Tracker::Github.hook_name()).is_file());
        assert!(hooks.join(Tracker::Jira.hook_name()).is_file());
    }

    /// The rule a prompt or a task skeleton already follows, proven for a
    /// hook script too: once `init` has written one, `spoolway sync` must
    /// leave it exactly as it is, whatever a project has done to it since.
    #[test]
    fn a_sync_leaves_a_written_hook_alone() {
        let root = scaffold(
            "hook-untouched",
            &InitArgs {
                tracker: Some("github".into()),
                project_key: Some("acme/app".into()),
                ..confirmed()
            },
        );
        let hook = root
            .join(".spoolway/hooks")
            .join(Tracker::Github.hook_name());
        let mine = "#!/bin/sh\necho mine\n";
        std::fs::write(&hook, mine).unwrap();

        let repo = Repo {
            checkout: root.clone(),
            config: Config::load(&root).unwrap(),
            home: root.join(".home"),
            root,
        };
        // `scan`, not `run`: `run` also prints the report and writes the
        // sync-stamp, neither of which this test is about — see `sync.rs`'s
        // own tests, which call `scan` for the same reason.
        crate::sync::scan(
            &repo,
            &crate::cli::SyncArgs {
                dry_run: false,
                replace: Vec::new(),
            },
        )
        .expect("sync scan");

        assert_eq!(
            std::fs::read_to_string(&hook).unwrap(),
            mine,
            "`spoolway sync` must never touch a hook `init` has already written"
        );
    }

    /// A path at least [`STAMP_PATH_WIDTH`] wide must still be followed by a
    /// separating space before whatever value comes next — `{:<width$}`
    /// alone pads only up to `width`, so a path already that long or longer
    /// gets none, and the value would land concatenated straight onto the
    /// path with nothing between them. `init` run from inside a linked
    /// worktree stamps at the main checkout's common git directory, which is
    /// an absolute path easily past the mockup's short `.git/spoolway-id`.
    #[test]
    fn a_long_path_still_gets_a_separating_space_before_its_value() {
        let short = ".git/spoolway-id";
        assert!(short.len() < STAMP_PATH_WIDTH);
        assert_eq!(
            pad_to_value_column(short),
            format!("{short:<width$}", width = STAMP_PATH_WIDTH)
        );

        let long = "/home/marvin/projects/some-very-long-checkout-name/.git/spoolway-id";
        assert!(long.len() >= STAMP_PATH_WIDTH);
        let padded = pad_to_value_column(long);
        assert_eq!(padded, format!("{long} "));
        assert!(
            padded.ends_with(' '),
            "a path this long must still be followed by a separator: {padded:?}"
        );
    }

    /// The mockup's `project  ~/code/billing-svc` row is `report_row`'s own
    /// column, not a one-off — `"project"` is seven characters, same as
    /// `"wrote"` plus its four spaces of padding, so it lines up with every
    /// other row this same helper produces.
    #[test]
    fn the_project_row_matches_the_mockups_column() {
        assert_eq!(
            report_row("project", "~/code/billing-svc"),
            "  project  ~/code/billing-svc"
        );
    }

    /// With nobody to answer and no `--yes`, `Set up this project?` takes
    /// its own declared default — no — and `init` writes nothing at all:
    /// no `.spoolway/`, no skills, and no home claimed under `$HOME`. It
    /// does not hang either, which is the other half of the contract
    /// `init_with_nobody_to_ask_takes_every_default` proves for the two
    /// questions this one sits in front of: a suite that gave `init` no
    /// stdin and got a hang would time out somewhere unrelated.
    #[test]
    fn init_with_nobody_to_ask_and_no_yes_writes_nothing() {
        let root = crate::scratch::root("init-confirm-declined");
        let _ = std::fs::remove_dir_all(&root);
        let home = home_for(&root);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);

        run_init(&root, &InitArgs::default()).expect("a declined init is not an error");

        assert!(!Config::path_in(&root).exists(), "no config was written");
        assert!(!root.join(STATE_DIR).exists(), "no .spoolway/ at all");
        assert!(!root.join(".claude").exists(), "no skills were installed");
        assert!(
            !home.join(".spoolway").exists(),
            "no home was claimed under ~/.spoolway/"
        );
    }

    /// The same run with `--yes`: the confirmation is answered without
    /// asking, and the scaffold lands exactly where it always has. The pair
    /// is what makes the flag the thing that decides, rather than the
    /// presence of a terminal.
    #[test]
    fn init_with_yes_writes_the_scaffold_without_asking() {
        let root = scaffold("confirm-yes", &confirmed());
        assert!(Config::path_in(&root).exists());
        assert!(root.join(".claude").join("skills").is_dir());
    }
}
