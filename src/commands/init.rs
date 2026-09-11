//! `spoolway init`: scaffolding a project, and the questions it asks first.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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

/// The file naming which checkout a project's home directory belongs to.
const PROJECT_FILE: &str = "project.toml";

/// What a project's own home directory (`~/.spoolway/<name>/`) is pointed
/// back at — see [`crate::mux::project_home`].
///
/// Written once, by [`claim`], and read only to tell a repeat `init` of the
/// same checkout from a second checkout asking for a name already spoken for.
#[derive(Debug, Serialize, Deserialize)]
struct ProjectPointer {
    root: PathBuf,
}

/// Claim this checkout's name under `~/.spoolway/`, or refuse it.
///
/// A path is unique and a basename is not, so two checkouts sharing one — a
/// work clone and a personal one of the same repo, most often — would
/// otherwise share one home directory and one queue without either of them
/// knowing it. The pointer file is the whole of the check: the same root,
/// every time, and `init` carries on; a different one, and it refuses. There
/// is no flag that talks it out of that.
///
/// `.dispatcher` is refused outright, with no pointer file to disagree with —
/// that name belongs to the shared dispatch workspace (see
/// [`crate::mux::dispatch_home`]), not to any project.
fn claim(root: &Path, take_over: bool) -> Result<Option<String>> {
    let name = crate::mux::project_label(root);
    if name == crate::mux::DISPATCH_HOME_NAME {
        bail!(
            "`{name}` is spoolway's own name for its shared dispatch workspace — rename this \
             checkout and run `spoolway init` again"
        );
    }

    let absolute = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    let home = crate::mux::project_home(root);
    let pointer_path = home.join(PROJECT_FILE);

    // A missing pointer is the only case this proceeds past: nobody has
    // claimed the name yet, so this checkout may. Anything else that stops
    // the file from being read as a pointer — a permissions problem, a
    // truncated write, a person's own edit that broke the TOML — is refused
    // rather than treated as "nobody's claimed it", or a claim that already
    // exists could be silently overwritten by the very check meant to catch
    // that collision.
    match std::fs::read_to_string(&pointer_path) {
        Ok(raw) => {
            let pointer: ProjectPointer = toml::from_str(&raw)
                .with_context(|| format!("parsing {}", pointer_path.display()))?;
            if pointer.root == absolute {
                return Ok(None);
            }
            // The checkout the name is registered to is simply gone —
            // deleted, moved, a scratch directory from a test run — and
            // `rename one of the two directories` is advice nobody can act
            // on when there is only one directory left to rename. Answered
            // by [`reclaim`] instead of the collision refusal below, which is
            // for the case where both checkouts are real.
            if !pointer.root.exists() {
                return reclaim(
                    &name,
                    &home,
                    &pointer_path,
                    &pointer.root,
                    &absolute,
                    take_over,
                );
            }
            bail!(
                "the name `{name}` is already taken\n  {} belongs to {}\n  this project is {}\n\
                 rename one of the two directories, then run this again",
                pointer_path.display(),
                pointer.root.display(),
                absolute.display(),
            );
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("reading {}", pointer_path.display())),
    }

    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
    write_pointer(&pointer_path, &absolute)?;
    Ok(None)
}

/// Write `project.toml`, claiming `root` for the home `pointer_path` sits in.
/// Shared by a fresh claim and [`reclaim`], which both end the same way once
/// they have decided the name is theirs to take.
fn write_pointer(pointer_path: &Path, root: &Path) -> Result<()> {
    let pointer = ProjectPointer {
        root: root.to_path_buf(),
    };
    let body = format!(
        "# Which checkout this directory holds the state of. Written once, by\n\
         # `spoolway init`, and read only to tell a repeat init of the same\n\
         # project from a second project that wants the same name.\n{}",
        toml::to_string_pretty(&pointer).context("serialising project.toml")?
    );
    write_atomic(pointer_path, body)
}

/// A registration whose checkout no longer exists: reclaim the name outright
/// when nothing of its state would be walked into by accident, or refuse and
/// say what `--take-over` is for.
///
/// `scripts/e2e/suites/warmth.sh` used to clear a dead registration by hand
/// before every run to get past exactly this — the refusal at [`claim`]
/// named "rename one of the two directories" as the only way out, which
/// cannot be followed once one of the two no longer exists to rename.
///
/// Never deletes `home` either way: a reclaim only rewrites the pointer, and
/// `--take-over` keeps whatever archive or queue was already there for the
/// checkout that takes the name over to read.
fn reclaim(
    name: &str,
    home: &Path,
    pointer_path: &Path,
    old_root: &Path,
    new_root: &Path,
    take_over: bool,
) -> Result<Option<String>> {
    let archived = count_task_documents(&home.join(crate::config::ARCHIVE_DIR));
    let queued = count_task_documents(&home.join(crate::config::QUEUE_DIR));
    let empty = archived == 0 && queued == 0;

    if !empty && !take_over {
        bail!(
            "the name `{name}` is registered to {}, which no longer exists, but its state \
             still holds {}\n  `spoolway init --take-over` claims the name and keeps it.",
            old_root.display(),
            describe_state(archived, queued),
        );
    }

    write_pointer(pointer_path, new_root)?;

    Ok(Some(if empty {
        format!(
            "reclaimed the name `{name}` — it was registered to {}, which no longer exists, \
             and its state held no archive and no queued tasks.",
            old_root.display()
        )
    } else {
        format!(
            "took over the name `{name}` — it was registered to {}, which no longer exists; \
             its state ({}) stays.",
            old_root.display(),
            describe_state(archived, queued)
        )
    }))
}

/// "an archive (12 tasks)", "3 queued tasks", or both — whichever of the two
/// [`reclaim`] found something in.
fn describe_state(archived: usize, queued: usize) -> String {
    let mut parts = Vec::new();
    if archived > 0 {
        parts.push(format!(
            "an archive ({archived} task{})",
            if archived == 1 { "" } else { "s" }
        ));
    }
    if queued > 0 {
        parts.push(format!(
            "{queued} queued task{}",
            if queued == 1 { "" } else { "s" }
        ));
    }
    parts.join(" and ")
}

/// How many task documents a queue or archive directory holds — the same
/// `.md` shape either one keeps, counted rather than parsed: a reclaim only
/// needs to know whether there is anything there, not what it says.
fn count_task_documents(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("md"))
        .count()
}

pub fn init(root: &Path, args: &InitArgs) -> Result<()> {
    // Before anything is written or asked, because this is the one command a
    // person runs without knowing yet what they have got hold of.
    print!("{}", crate::status::banner("setting up a project"));

    // A repeat run is how a project adds another provider's skills. Keep that
    // successful outcome distinct from creating (or deliberately replacing)
    // the project's scaffold.
    let already_initialized = Config::path_in(root).exists() && !args.force;

    // Before anything is written: a name clash is a refusal, not a partial
    // scaffold left for the next run to trip over. `claim` says what it did
    // only when there was something to say — a fresh or repeat claim is
    // silent, and a reclaim or take-over names the dead registration it
    // found.
    if let Some(note) = claim(root, args.take_over)? {
        println!("{note}");
    }

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

    let place = |path: std::path::PathBuf, contents: &[u8], exec: bool| -> Result<()> {
        if path.exists() && !args.force {
            return Ok(());
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
        Ok(())
    };

    place(
        Config::path_in(root),
        config
            .render()
            .context("rendering default config")?
            .as_bytes(),
        false,
    )?;
    // One file per pipeline, named for the pipeline it holds. A project adds
    // its own by writing another file here and nothing else.
    for (name, body) in crate::pipeline::BUILTIN_PIPELINES {
        place(
            Pipelines::file_in(root, name),
            answers.fill(body).as_bytes(),
            false,
        )?;
    }
    // Written whole, and never looked at again. A prompt is the project's from
    // the moment `init` finishes: no update rewrites one, so nothing here has to
    // be a shape a later binary can still find its way around in.
    for prompt in assets::PROMPTS {
        let dir = state.join("prompts").join(prompt.name);
        place(dir.join(assets::PROMPT_FILE), prompt.body.as_bytes(), false)?;
        // A prompt's belongings follow its prose: written once, never updated,
        // and the project's to restyle from here on. No setting names them —
        // the prompt that fills them is the only thing that reads them.
        for (name, body) in prompt.assets {
            place(
                dir.join(assets::PROMPT_ASSETS).join(name),
                body.as_bytes(),
                false,
            )?;
        }
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
    // The shape `spoolway stack`'s summary prompt fills in. One file, not one
    // per pipeline — there is one shape of pull request whichever pipeline
    // opened it.
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

    for note in &notes {
        println!("{note}");
    }
    // The skills, in the provider's own convention. Run from here rather than
    // suggested, because "and now run this other command" is the manual step
    // this exists to remove — and a project that skipped it had skills that
    // were shipped, documented, and never installed.
    let installed = crate::install::install(root, answers.provider, args.force)?;
    crate::install::report(installed);
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
    /// `init` now claims a name under the real `~/.spoolway/`, so running it
    /// unguarded in a test would write into whoever is running the suite's
    /// actual home directory — and could fail outright if that machine
    /// already has an unrelated project by this scratch root's basename. One
    /// scratch home per root keeps every call in one test, including a
    /// deliberate second `init`, agreeing about where `root` claimed its
    /// name.
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

    /// `.dispatcher` is spoolway's own name for the shared dispatch
    /// workspace, and a checkout may not claim it — there is no pointer file
    /// for this one to disagree with, so it has to be refused outright before
    /// anything is read or written.
    #[test]
    fn a_checkout_named_dispatcher_is_refused() {
        let root = crate::scratch::root("init-dispatcher-name").join(".dispatcher");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);

        let err = crate::platform::test_home::with_home(&home_for(&root), || claim(&root, false))
            .expect_err("a checkout named `.dispatcher` must be refused");
        assert!(err.to_string().contains("dispatch workspace"), "{err:#}");
    }

    /// A registered root that no longer exists, and no archive or queued
    /// tasks behind it, is reclaimed outright — the advice the collision
    /// refusal gives ("rename one of the two directories") cannot be
    /// followed once there is only one directory left. And it says what it
    /// did: `claim` hands the note back rather than printing it itself,
    /// which is what makes it a return value this test can check rather
    /// than something only a person watching the terminal would ever see.
    #[test]
    fn a_dead_registration_with_empty_state_is_reclaimed_and_says_what_it_did() {
        let base = crate::scratch::root("init-reclaim-empty");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let home = base.join("home");

        let old_root = base.join("old").join("proj");
        std::fs::create_dir_all(&old_root).unwrap();
        crate::scratch::git_init(&old_root, &["-b", "plan/demo"]);
        crate::platform::test_home::with_home(&home, || claim(&old_root, false)).unwrap();
        let old_root_canonical = old_root.canonicalize().unwrap();

        // The checkout the name was registered to is gone.
        std::fs::remove_dir_all(&old_root).unwrap();

        let new_root = base.join("new").join("proj");
        std::fs::create_dir_all(&new_root).unwrap();
        crate::scratch::git_init(&new_root, &["-b", "plan/demo"]);

        let note = crate::platform::test_home::with_home(&home, || claim(&new_root, false))
            .expect("a dead registration with nothing behind it should be reclaimed")
            .expect("a reclaim has something to say, unlike an ordinary claim");
        assert!(note.contains("reclaimed the name `proj`"), "{note}");
        assert!(
            note.contains(&old_root_canonical.display().to_string()),
            "{note}"
        );
        assert!(note.contains("no archive and no queued tasks"), "{note}");

        let pointer_path = home.join(".spoolway").join("proj").join(PROJECT_FILE);
        let pointer: ProjectPointer =
            toml::from_str(&std::fs::read_to_string(&pointer_path).unwrap()).unwrap();
        assert_eq!(pointer.root, new_root.canonicalize().unwrap());
    }

    /// A registered root that no longer exists, but whose home still holds an
    /// archive, is refused unless `--take-over` says to keep it anyway —
    /// nothing here is deleted either way.
    #[test]
    fn a_dead_registration_with_an_archive_is_refused_without_take_over() {
        let base = crate::scratch::root("init-reclaim-archive");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let home = base.join("home");

        let old_root = base.join("old").join("proj");
        std::fs::create_dir_all(&old_root).unwrap();
        crate::scratch::git_init(&old_root, &["-b", "plan/demo"]);
        crate::platform::test_home::with_home(&home, || claim(&old_root, false)).unwrap();
        let old_root_canonical = old_root.canonicalize().unwrap();

        let archive_dir = home.join(".spoolway").join("proj").join("archive");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(
            archive_dir.join("done-task.md"),
            "---\nid: done-task\n---\n",
        )
        .unwrap();

        std::fs::remove_dir_all(&old_root).unwrap();

        let new_root = base.join("new").join("proj");
        std::fs::create_dir_all(&new_root).unwrap();
        crate::scratch::git_init(&new_root, &["-b", "plan/demo"]);

        let err = crate::platform::test_home::with_home(&home, || claim(&new_root, false))
            .expect_err("a dead registration with an archive must not be taken silently");
        assert!(err.to_string().contains("--take-over"), "{err:#}");
        assert!(err.to_string().contains("archive"), "{err:#}");

        // Untouched: the refusal must not have moved the pointer.
        let pointer_path = home.join(".spoolway").join("proj").join(PROJECT_FILE);
        let pointer: ProjectPointer =
            toml::from_str(&std::fs::read_to_string(&pointer_path).unwrap()).unwrap();
        assert_eq!(pointer.root, old_root_canonical);

        // `--take-over` claims it, and keeps the archive rather than deleting it.
        let note = crate::platform::test_home::with_home(&home, || claim(&new_root, true))
            .expect("--take-over should claim a dead registration even with an archive")
            .expect("a take-over has something to say too");
        assert!(note.contains("took over the name `proj`"), "{note}");
        assert!(note.contains("archive"), "{note}");
        let pointer: ProjectPointer =
            toml::from_str(&std::fs::read_to_string(&pointer_path).unwrap()).unwrap();
        assert_eq!(pointer.root, new_root.canonicalize().unwrap());
        assert!(
            archive_dir.join("done-task.md").exists(),
            "take-over keeps the existing state — nothing here deletes it"
        );
    }

    /// A pointer file that exists but cannot be read as one — a permissions
    /// problem, or a person's own edit that broke the TOML — must refuse
    /// rather than fall through to the write at the end of `claim`, which
    /// would silently take over whatever claim was already there.
    #[test]
    fn a_pointer_file_that_will_not_parse_is_refused_rather_than_overwritten() {
        let root = crate::scratch::root("init-bad-pointer");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);

        let home = home_for(&root);
        let pointer_dir = home.join(".spoolway").join(root.file_name().unwrap());
        std::fs::create_dir_all(&pointer_dir).unwrap();
        let pointer_path = pointer_dir.join(PROJECT_FILE);
        std::fs::write(&pointer_path, "this is not = = toml\n").unwrap();

        let err = crate::platform::test_home::with_home(&home, || claim(&root, false))
            .expect_err("a pointer file that will not parse must be refused");
        assert!(
            err.to_string().contains("project.toml"),
            "the error should name the file that would not parse: {err:#}"
        );
        // Untouched: the whole point is that this is refused rather than
        // silently taken over by a fresh claim.
        assert_eq!(
            std::fs::read_to_string(&pointer_path).unwrap(),
            "this is not = = toml\n"
        );
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
}
