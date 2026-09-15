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

        // Tracker and project key are only asked and applied when a config is
        // going to be written. An established project's issue integration is
        // not replaced merely because init was run to add another skill copy.
        if !fresh {
            if args.tracker.is_some() || args.project_key.is_some() {
                println!(
                    "  note  this project has a config already, so --tracker/--project-key \
                     were not applied — change them with `spoolway config set`, \
                     or re-run with --force to take the shipped config back"
                );
            }
            return Ok(Self {
                provider,
                tracker: Tracker::None,
                project_key: String::new(),
            });
        }

        let (tracker, project_key) = Self::tracker(args)?;

        Ok(Self {
            provider,
            tracker,
            project_key,
        })
    }

    /// The tracker `[issue_tracking]` names, and the project it files into —
    /// off the flags, or off a menu the provider question's own takes.
    ///
    /// The note beside each entry is whether its command-line tool is on
    /// `PATH`, so choosing Jira without `acli` installed says so at the
    /// moment it is chosen rather than at the first hook that fails. `none`
    /// is the menu's default: a script with nobody to ask gets the same "no
    /// issue tracking" behaviour a project had before this existed, not a
    /// `gh` hook nobody asked for.
    fn tracker(args: &InitArgs) -> Result<(Tracker, String)> {
        let tracker = match args.tracker {
            Some(tracker) => tracker,
            None => {
                if !crate::ask::interactive() {
                    return Ok((Tracker::None, String::new()));
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
            return Ok((tracker, String::new()));
        }

        let project_key = match &args.project_key {
            Some(key) => key.clone(),
            None => crate::ask::line(
                "Which project does it file into?",
                "owner/repo for github, project key for jira",
            )?
            .unwrap_or_default(),
        };
        Ok((tracker, project_key))
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

/// One `wrote` row for the mockup's report block, aligned to the same
/// column [`pad_to_value_column`] gives the `stamped` row below it.
///
/// `count_noun` is `None` for a single file's row, which ends right after
/// the path — there is nothing to count — and `Some((n, noun))` for a
/// directory's row, which reports how many of `noun` it holds and pluralizes
/// accordingly.
fn wrote_row(path: &str, count_noun: Option<(usize, &str)>) -> String {
    match count_noun {
        None => format!("  wrote    {path}"),
        Some((count, noun)) => format!(
            "  wrote    {}{count} {noun}{}",
            pad_to_value_column(path),
            if count == 1 { "" } else { "s" }
        ),
    }
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
    config.pipeline_gen.pipeline_agent = profile.to_string();
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

    // Whether `place` actually wrote `path`, so the `wrote` rows below
    // report only what a run actually did — a repeat `init` that adds
    // nothing new says nothing about `config.toml`, pipelines or prompts,
    // the same way `claim` and the stamped line already say nothing on a
    // repeat run.
    let place = |path: std::path::PathBuf, contents: &[u8], exec: bool| -> Result<bool> {
        if path.exists() && !args.force {
            return Ok(false);
        }
        write_atomic(&path, contents)?;
        if exec {
            // A hook is invoked as a bare command line — see
            // `crate::tracking::hook_path` — so it needs the execute bit
            // itself; nothing else `init` writes is ever run rather than
            // read. No-op on Windows, where a `.ps1` is handed to
            // `powershell -EncodedCommand` rather than executed directly.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&path)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&path, perms)?;
            }
        }
        Ok(true)
    };

    // The mockup's three `wrote` rows, collected as they happen rather than
    // guessed from whether a config already existed: `--force` writes every
    // one of these again on a project that was already initialized, and a
    // plain repeat run writes none of them, so what actually happened is
    // the only thing worth trusting.
    let mut wrote_rows: Vec<String> = Vec::new();

    if place(
        Config::path_in(root),
        config
            .render()
            .context("rendering default config")?
            .as_bytes(),
        false,
    )? {
        wrote_rows.push(wrote_row(".spoolway/config.toml", None));
    }
    // One file per pipeline, named for the pipeline it holds. A project adds
    // its own by writing another file here and nothing else.
    let mut pipelines_written = 0usize;
    for (name, body) in crate::pipeline::BUILTIN_PIPELINES {
        if place(
            Pipelines::file_in(root, name),
            answers.fill(body).as_bytes(),
            false,
        )? {
            pipelines_written += 1;
        }
    }
    if pipelines_written > 0 {
        wrote_rows.push(wrote_row(
            ".spoolway/pipelines/",
            Some((pipelines_written, "pipeline")),
        ));
    }
    // Written whole, and never looked at again. A prompt is the project's from
    // the moment `init` finishes: no update rewrites one, so nothing here has to
    // be a shape a later binary can still find its way around in.
    let mut prompts_written = 0usize;
    for prompt in assets::PROMPTS {
        let dir = state.join("prompts").join(prompt.name);
        // A directory counts as written the moment anything in it is — its
        // own `PROMPT.md` or, on a repeat run, only one of its assets that
        // had gone missing — so `|=` rather than overwriting: the first
        // `place` that actually writes must not be undone by a later one
        // that finds nothing to do.
        let mut wrote_this_prompt =
            place(dir.join(assets::PROMPT_FILE), prompt.body.as_bytes(), false)?;
        // A prompt's belongings follow its prose: written once, never updated,
        // and the project's to restyle from here on. No setting names them —
        // the prompt that fills them is the only thing that reads them.
        for (name, body) in prompt.assets {
            wrote_this_prompt |= place(
                dir.join(assets::PROMPT_ASSETS).join(name),
                body.as_bytes(),
                false,
            )?;
        }
        if wrote_this_prompt {
            prompts_written += 1;
        }
    }
    if prompts_written > 0 {
        wrote_rows.push(wrote_row(
            ".spoolway/prompts/",
            Some((prompts_written, "prompt")),
        ));
    }
    // One task skeleton per shipped pipeline, named for the pipeline that takes
    // it. A project adds a pipeline's shape by writing a file beside these, and
    // one that writes nothing takes `default`.
    for (name, skeleton) in assets::TASK_TEMPLATES {
        place(
            root.join(crate::config::TASK_TEMPLATES_DIR)
                .join(format!("{name}.md")),
            skeleton.as_bytes(),
            false,
        )?;
    }
    // The two ticket-body templates a tracker hook renders and hands to its
    // own `gh`/`acli` call — seeded once, like a task skeleton, and never
    // looked at again by `update`. A project with neither file written gets
    // a single line naming the task instead of this prose; see
    // `crate::task_template::resolve_tracking`.
    for (name, body) in assets::TRACKING_TEMPLATES {
        place(
            root.join(crate::config::TRACKING_TEMPLATES_DIR)
                .join(format!("{name}.md")),
            body.as_bytes(),
            false,
        )?;
    }
    // The hook scripts every project gets, whichever tracker it answered —
    // switching later is a `spoolway config set issue_tracking.hook` away,
    // not a second `init`. Only this platform's own pair: `.sh` wherever
    // `crate::platform::shell_command` reaches for `sh -c`, `.ps1` wherever
    // it reaches for PowerShell instead — the other pair could never run
    // here.
    let hook_ext = if cfg!(windows) { "ps1" } else { "sh" };
    for (name, body) in assets::HOOK_SCRIPTS
        .iter()
        .filter(|(name, _)| name.ends_with(hook_ext))
    {
        place(
            root.join(".spoolway/hooks").join(name),
            body.as_bytes(),
            true,
        )?;
    }
    // A pull request template, unread by anything in this binary — see
    // `crate::config::PULL_REQUEST_TEMPLATE` — but installed and managed
    // like any other file in this module.
    place(
        root.join(crate::config::PULL_REQUEST_TEMPLATE),
        assets::PULL_REQUEST_TEMPLATE.as_bytes(),
        false,
    )?;
    // The seven typed messages a lane's pane receives. One file, not one per
    // pipeline or per state — a project overrides as many `##` sections as
    // it wants and leaves the rest to fall back to spoolway's own words.
    place(
        root.join(crate::config::LANE_PROMPTS_TEMPLATE),
        assets::LANE_PROMPTS.as_bytes(),
        false,
    )?;
    // What belongs under each heading spoolway appends to a task file. One
    // file, not one per pipeline — the three headings are fixed and every
    // task shares them.
    place(
        root.join(crate::config::TASK_LOG_TEMPLATE),
        assets::TASK_LOG.as_bytes(),
        false,
    )?;
    // No plan skeleton here any more. spoolway-plan carries its own, under the
    // skill's own `assets/`, and writes a self-contained page with it — there
    // is nothing left for `init` to place in the project.

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
    // The skills, in the provider's own convention. Run from here rather than
    // suggested, because "and now run this other command" is the manual step
    // this exists to remove — and a project that skipped it had skills that
    // were shipped, documented, and never installed.
    let installed = crate::install::install(root, answers.provider, args.force)?;
    crate::install::report(installed);
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
        let root = scaffold("defaults", &InitArgs::default());

        let config = Config::load(&root).unwrap();
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents["claude"].kind, "claude");
        assert_eq!(config.pipeline_gen.pipeline_agent, "claude");
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

    /// `spoolway init` stamps a project's home off its own `.git`, and a
    /// directory with none is refused for that reason — not silently handed
    /// a basename-keyed home it could never move or rename without losing.
    #[test]
    fn init_refuses_a_directory_with_no_git_repository() {
        let root = crate::scratch::root("init-no-git");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let err = run_init(&root, &InitArgs::default()).expect_err("no .git here at all");
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
    #[cfg(unix)]
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
        let err = run_init(&root, &InitArgs::default());

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

        run_init(&root, &InitArgs::default()).expect("init");

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
                ..InitArgs::default()
            },
        );

        let config = Config::load(&root).unwrap();
        assert_eq!(config.agents.len(), 1);
        assert!(config.agents.contains_key("codex"));
        assert_eq!(config.pipeline_gen.pipeline_agent, "codex");
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
                ..InitArgs::default()
            },
        );
        let before = std::fs::read_to_string(Config::path_in(&root)).unwrap();

        run_init(
            &root,
            &InitArgs {
                provider: Some(PlanningAgent::Codex),
                ..InitArgs::default()
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

        run_init(&root, &InitArgs::default()).expect("init");
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
        run_init(&root, &InitArgs::default()).expect("first init");
        let before = std::fs::read_to_string(root.join(".git").join("spoolway-id")).unwrap();

        run_init(
            &root,
            &InitArgs {
                new_id: true,
                ..InitArgs::default()
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
        crate::platform::test_home::with_home(&home_root, || init(&original, &InitArgs::default()))
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
                    ..InitArgs::default()
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
                    ..InitArgs::default()
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
                    ..InitArgs::default()
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
                tracker: Some(Tracker::Github),
                project_key: Some("acme/app".into()),
                ..InitArgs::default()
            },
        );

        let config = std::fs::read_to_string(Config::path_in(&root)).unwrap();
        let github = Tracker::Github.hook_name();
        let jira = Tracker::Jira.hook_name();
        assert!(config.contains(&format!("hook = \"{github}\"")), "{config}");
        assert!(config.contains("project_key = \"acme/app\""), "{config}");

        let hook = root.join(".spoolway/hooks").join(&github);
        assert!(hook.is_file());
        #[cfg(unix)]
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
                tracker: Some(Tracker::None),
                project_key: Some("should-be-ignored".into()),
                ..InitArgs::default()
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
    /// hook script too: once `init` has written one, `spoolway update` must
    /// leave it exactly as it is, whatever a project has done to it since.
    #[test]
    fn an_update_leaves_a_written_hook_alone() {
        let root = scaffold(
            "hook-untouched",
            &InitArgs {
                tracker: Some(Tracker::Github),
                project_key: Some("acme/app".into()),
                ..InitArgs::default()
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
        // `scan`, not `run`: `run` checks for a newer release first, which
        // is `update`'s own concern and not this one — see `update.rs`'s
        // own tests, which call `scan` for the same reason.
        crate::update::scan(
            &repo,
            &crate::cli::UpdateArgs {
                dry_run: false,
                replace: Vec::new(),
            },
        )
        .expect("update scan");

        assert_eq!(
            std::fs::read_to_string(&hook).unwrap(),
            mine,
            "`spoolway update` must never touch a hook `init` has already written"
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
}
