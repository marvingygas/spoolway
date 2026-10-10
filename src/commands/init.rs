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
    /// Whether the shipped pipelines, prompts, task templates and ticket
    /// templates are placed. Asked only on a fresh project; an established
    /// one reads it off its own pipelines unless a flag answers it — see
    /// [`Answers::examples`].
    examples: bool,
    /// The tracker `[issue_tracking]` names — `Tracker::None` when the
    /// config is staying as it is, because there is nothing to land it in.
    tracker: Tracker,
    /// The project the tracker's tickets open into. Blank whenever `tracker`
    /// is, and always blank on a config that is staying as it is.
    project_key: String,
    /// Whether `tracker`/`project_key` were actually resolved this run —
    /// true on a project with no config of its own (always asked or
    /// answered) and on an established one when `--tracker` was given a
    /// value, or given bare with somebody there to answer the picker. A
    /// `--force` run on an established project without `--tracker` is
    /// false too: it carries the project's own tracker into the config it
    /// rewrites rather than asking. False means
    /// `tracker`/`project_key` are the untouched placeholders above, and
    /// whoever decides whether the hook scripts are placed has to read the
    /// tracker already on disk instead — which
    /// also covers a bare `--tracker` on an established project with nobody
    /// to ask: there is no answer to apply, so nothing about
    /// `[issue_tracking]` is touched, the same as the flag being absent.
    tracker_touched: bool,
}

impl Answers {
    /// Flags first, then the person, then the default — and the person is
    /// skipped whenever [`crate::ask::interactive`] says there is not one.
    ///
    /// `new_setup` says the setup is a new workspace's whatever `root`
    /// resolves to now: a listed checkout moving into a new workspace still
    /// reads its old workspace's `config.toml` until the move, and taking
    /// that for an established project would skip the very questions the
    /// new workspace's empty `config/` needs answered.
    fn gather(root: &Path, args: &InitArgs, new_setup: bool) -> Result<Self> {
        // Whether the config this run renders will actually be written.
        // `Placer::file` decides the same thing for every file later on; this
        // is the one case where the decision has to be made early, because it
        // is what makes these questions worth asking.
        //
        // `established` is the narrower fact: this project has a config of its
        // own whose choices are worth keeping. A `--force` run rewrites that
        // config, but the provider and tracker in it are still the project's
        // answers, so they are read back unless a flag replaces them.
        let established = !new_setup && Config::path_in(root).exists();
        let fresh = !established || args.force;

        let provider = Self::provider(root, args, established)?;
        let examples = Self::examples(root, args, fresh)?;

        // Tracker and project key are asked, or answered outright, for a
        // project with no config of its own, and for an established one
        // whenever `--tracker` was given. An established project that is
        // being `--force`d is not asked: the config is rewritten, but its
        // tracker is carried across by `init`. `--project-key` alone, with no `--tracker` alongside it,
        // is dropped on an established project exactly as it always was: an
        // established project's issue integration is not replaced merely
        // because init was run to add another skill copy.
        if established && args.tracker.is_none() {
            if args.project_key.is_some() {
                let advice = if args.force {
                    "pass --tracker with it, or change it with `spoolway config set`"
                } else {
                    "change it with `spoolway config set`, or re-run with --force to take \
                     the shipped config back"
                };
                println!(
                    "  note  this project has a config already, so --project-key was not \
                     applied without --tracker — {advice}"
                );
            }
            return Ok(Self {
                provider,
                examples,
                tracker: Tracker::None,
                project_key: String::new(),
                tracker_touched: false,
            });
        }

        match Self::tracker(root, args, established)? {
            Some((tracker, project_key)) => Ok(Self {
                provider,
                examples,
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
                    examples,
                    tracker: Tracker::None,
                    project_key: String::new(),
                    tracker_touched: false,
                })
            }
        }
    }

    /// The answers for a checkout joining a workspace, or moving into one
    /// that already has a setup: the agent question alone. The setup is the workspace's and
    /// already chosen — its examples, its tracker — so asking either again
    /// would take an answer this run has nowhere to put, and writing one into
    /// the shared `config/` would change every other clone's setup from here.
    ///
    /// Any of `--tracker`, `--project-key`, `--examples` or `--no-examples`
    /// given alongside a join used to be silently dropped on the floor —
    /// answered, from this function's point of view, exactly as if they had
    /// never been typed, with nothing printed to say so. Named here instead,
    /// so a person who typed one does not have to notice its absence from
    /// `config.toml` to learn it did nothing.
    fn joining(root: &Path, args: &InitArgs) -> Result<Self> {
        // Ignored, but still checked: a value no other run would accept is
        // a mistake worth stopping on before the join writes anything, not
        // one to wave through because this run happens not to use it.
        if let Some(raw) = args.tracker.as_deref().filter(|raw| !raw.is_empty()) {
            parse_tracker(raw)?;
        }
        let mut ignored = Vec::new();
        if args.tracker.is_some() {
            ignored.push("--tracker");
        }
        if args.project_key.is_some() {
            ignored.push("--project-key");
        }
        if args.examples {
            ignored.push("--examples");
        }
        if args.no_examples {
            ignored.push("--no-examples");
        }
        if !ignored.is_empty() {
            println!(
                "  note  joining a workspace uses its own shared setup, already chosen — \
                 ignored: {}",
                ignored.join(", ")
            );
        }
        Ok(Self {
            // A checkout joining a workspace has no own-checkout config to
            // read an existing provider off — the workspace's shared config
            // lives elsewhere — so this is `fresh` from `provider`'s point of
            // view: nobody to ask falls to the menu's default, as it always
            // did.
            provider: Self::provider(root, args, true)?,
            examples: false,
            tracker: Tracker::None,
            project_key: String::new(),
            tracker_touched: false,
        })
    }

    /// The coding agent: `--provider`, or the menu — except on an established
    /// project, which prefers the provider it already has over the menu's
    /// first entry.
    ///
    /// A repeat `init` with no `--provider` is how a project restores a
    /// missing `pipelines/` folder (see [`Answers::examples`]), and with
    /// nobody to answer the menu it used to fall to the menu's first entry,
    /// `claude`, restoring claude's pipelines — and `fill`'s skills install —
    /// onto a project set up for codex. `--provider` is checked first and
    /// always wins; failing that, with nobody to ask, the existing provider
    /// is taken outright, and with a person at the terminal it is only the
    /// menu's default, highlighted but not forced, so a person can still
    /// pick a different provider to add its skills.
    ///
    /// The existing provider is read off `config.unattended.blocked_agent`
    /// rather than `config.agents`' own keys, because that is the one field
    /// `init` writes to name the project's chosen profile (below, where
    /// `profile` is bound) — `config.agents` itself is free-form and a
    /// project may have added more profiles since. If the unblocker has
    /// since been pointed at a different or custom profile by hand, this
    /// reads that instead and falls back to the menu's default, `claude`,
    /// the same as a project with no provider configured at all.
    fn provider(root: &Path, args: &InitArgs, established: bool) -> Result<Provider> {
        if let Some(provider) = args.provider {
            return Ok(provider.provider());
        }
        let existing = if established {
            // The tracked file alone when `--force` will write the answer back
            // into it: the merged read includes a person's private override,
            // and `--force` would publish that into the shared config and
            // every pipeline.
            let loaded = if args.force {
                Config::load_tracked(root)
            } else {
                Config::load(root)
            };
            loaded.ok().and_then(|config| {
                <PlanningAgent as clap::ValueEnum>::from_str(&config.unattended.blocked_agent, true)
                    .ok()
            })
        } else {
            None
        };
        if !crate::ask::interactive() {
            return Ok(existing.unwrap_or(PlanningAgent::Claude).provider());
        }
        // Straight off clap's own list, so the menu and `--provider` cannot
        // come to offer different things. Names alone, with no note beside
        // them, as the mockup draws it.
        let providers = <PlanningAgent as clap::ValueEnum>::value_variants();
        let menu: Vec<(&str, &str)> = providers
            .iter()
            .map(|provider| (provider.name(), ""))
            .collect();
        let default = existing
            .and_then(|agent| providers.iter().position(|candidate| *candidate == agent))
            .unwrap_or(0);
        Ok(providers[crate::ask::choose("Select your agent", &menu, default)?].provider())
    }

    /// Whether to place the example setup: the flags first, then — on a fresh
    /// project only — the person, whose default is yes because that is what
    /// every `init` wrote before this question existed.
    ///
    /// An established project is not asked, for the same reason the tracker
    /// question is skipped there: its setup is already chosen, and it keeps
    /// the answer its own files give. One that still has any shipped
    /// pipeline or shipped prompt took the examples, so a repeat run restores
    /// whatever of them went missing — the documented way back from a
    /// deleted `pipelines/` folder, which leaves the prompts behind. One
    /// with neither declined them, and a repeat run, say to add another
    /// agent's skills, must not drop the examples in now.
    fn examples(root: &Path, args: &InitArgs, fresh: bool) -> Result<bool> {
        if args.examples {
            return Ok(true);
        }
        if args.no_examples {
            return Ok(false);
        }
        if !fresh {
            let prompts = crate::config::under_setup(
                &crate::config::setup_dir_in(root),
                crate::config::PROMPTS_DIR,
            );
            return Ok(crate::pipeline::BUILTIN_PIPELINES
                .iter()
                .any(|(name, _)| Pipelines::file_in(root, name).exists())
                || assets::PROMPTS
                    .iter()
                    .any(|prompt| prompts.join(prompt.name).exists()));
        }
        let menu = [("yes", ""), ("no", "")];
        Ok(crate::ask::choose("Install the example setup?", &menu, 0)? == 0)
    }

    /// The tracker `[issue_tracking]` names, and the project it files into —
    /// off the flags, or off a menu the provider question's own takes.
    /// `None` only for a bare `--tracker` on an established project
    /// (`established`) with nobody to answer its picker — see
    /// [`Answers::tracker_touched`]'s own doc for why that case answers
    /// nothing rather than `none`.
    ///
    /// The note beside each entry is whether its command-line tool is on
    /// `PATH`, so choosing Jira without `acli` installed says so at the
    /// moment it is chosen rather than at the first hook that fails. `none`
    /// is the menu's default: a script with nobody to ask gets the same "no
    /// issue tracking" behaviour a project had before this existed, not a
    /// `gh` hook nobody asked for.
    fn tracker(
        root: &Path,
        args: &InitArgs,
        established: bool,
    ) -> Result<Option<(Tracker, String)>> {
        let tracker = match args.tracker.as_deref() {
            // A value answers outright — `--tracker github`.
            Some(raw) if !raw.is_empty() => parse_tracker(raw)?,
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
                    return Ok(if established {
                        None
                    } else {
                        Some((Tracker::None, String::new()))
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
                        None => "no hooks are written; the table stays empty".to_string(),
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
            // No answer on the line — either there was nobody to ask, or a
            // person answered with nothing. Either way there is no new key,
            // so fall back to the one the project already has rather than
            // blanking it: that was the bug a repeat `--tracker` with no
            // `--project-key` had, losing a key like `acme/app` on every
            // established project that re-ran init to add a provider's
            // skills. Only an established project has an existing key worth
            // keeping; a fresh one has nothing to fall back on.
            None => {
                let answer = crate::ask::line(
                    "Which project does it file into?",
                    "owner/repo for github, project key for jira",
                )?;
                match answer {
                    Some(key) => key,
                    None => {
                        let existing = if established {
                            Config::load_tracked(root)
                                .map(|config| config.issue_tracking.project_key)
                                .unwrap_or_default()
                        } else {
                            String::new()
                        };
                        if existing.is_empty() {
                            println!(
                                "  note  {} was chosen with no project key, and there is none \
                                 to keep — issue_tracking.project_key was left empty; answer \
                                 with --project-key <key>",
                                tracker.name()
                            );
                        }
                        existing
                    }
                }
            }
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

/// A `--tracker` value, parsed by the same case-insensitive rule clap's own
/// `value_enum` uses, since `InitArgs::tracker` is a plain string (see its
/// own doc). The refusal lists the trackers off clap's own list, so a new
/// tracker is named here without anyone editing this message.
fn parse_tracker(raw: &str) -> Result<Tracker> {
    <Tracker as clap::ValueEnum>::from_str(raw, true).map_err(|message| {
        let names: Vec<String> = <Tracker as clap::ValueEnum>::value_variants()
            .iter()
            .map(|tracker| format!("`{}`", tracker.name()))
            .collect();
        anyhow::anyhow!("--tracker: {message} — pick one of {}", names.join(", "))
    })
}

/// Where this run puts the project's setup, settled before anything is
/// written — the answers to "Where should this project's setup live?" and,
/// in home mode, the workspace menu ([`WORKSPACE_QUESTION`]).
enum Placement {
    /// A tracked `.spoolway/` in the checkout, as `init` always did.
    Repo,
    /// A checkout some workspace already lists, staying there: a repeat
    /// run, set up in that workspace's `config/` exactly as a repeat
    /// repo-mode run is.
    Listed,
    /// Start a new workspace for this checkout.
    New,
    /// Add this checkout to the workspace with this folder name — or take
    /// over the queue of a gone checkout of the same repository there; see
    /// [`crate::repo::join_workspace`].
    Join(String),
    /// Move a checkout some workspace already lists into the workspace with
    /// this folder name, or into a new workspace for `None`; see
    /// [`crate::repo::move_checkout`].
    Move(Option<String>),
}

impl Placement {
    /// Flags first, then the person, then the default — the same order
    /// [`Answers::gather`] takes.
    ///
    /// A checkout whose setup already lives somewhere is not asked where
    /// its setup should live: one a workspace lists stays in home mode, and
    /// one with a tracked `.spoolway/` stays in repo mode. A flag asking to
    /// switch either between the two modes is refused rather than ignored,
    /// because that is not something `init` does, and a silent repeat run
    /// would read as if it had. A listed checkout may still move between
    /// workspaces: at a terminal it is shown the workspace menu, and
    /// `--workspace <other>` moves it without asking.
    fn choose(root: &Path, args: &InitArgs) -> Result<Self> {
        let placement = Self::choose_any(root, args)?;
        // Repo mode only: `root` being `$HOME` itself means its tracked
        // `.spoolway/` would be the very directory `crate::mux::state_root()`
        // names, spoolway's own state root. Home mode writes nothing into the
        // checkout, so a dotfiles repository in `$HOME` can still be a
        // home-mode clone. Refused here, before anything is written, because
        // going on used to fail deep inside `init` with a raw "file name
        // contained an unexpected NUL byte" — see
        // `crate::config::tracked_setup_dir_in`.
        if matches!(placement, Self::Repo) && crate::config::is_state_root_checkout(root) {
            bail!(
                "{} is your home directory, and `~/.spoolway` there is spoolway's own state \
                 directory — it can never also hold a project's tracked setup\n  run `spoolway \
                 init --setup home` to keep this checkout's setup under `~/.spoolway/` instead, \
                 or run `spoolway init` inside the actual project checkout",
                root.display()
            );
        }
        // Repo mode writes `.spoolway/` into the checkout, so anything already
        // there that is not a folder would only fail at the first write, after
        // `bind` had stamped `.git`. Named here instead, before either.
        if matches!(placement, Self::Repo) {
            let tracked = crate::config::tracked_setup_dir_in(root);
            if tracked.symlink_metadata().is_ok() && !tracked.is_dir() {
                bail!(
                    "{} is not a folder, so a repo-mode setup cannot create `.spoolway/` \
                     there\n  move or delete it and run `spoolway init` again, or run \
                     `spoolway init --setup home` to keep the setup under `~/.spoolway/` \
                     instead",
                    tracked.display()
                );
            }
        }
        Ok(placement)
    }

    /// [`Self::choose`] before the refusals that depend on the mode chosen:
    /// the home directory in repo mode, and a `.spoolway` that is not a
    /// folder.
    fn choose_any(root: &Path, args: &InitArgs) -> Result<Self> {
        let asked_home = args.setup == Some(Setup::Home) || args.workspace.is_some();
        if args.setup == Some(Setup::Repo) && args.workspace.is_some() {
            bail!("--workspace sets a project up in home mode, so it cannot go with --setup repo");
        }

        // Fallible, not the ordinary lenient `workspace_clone`: a broken
        // workspace file silently answering `None` here is exactly the
        // `workspace-scan-strict` bug — `init` falls through to `Placement::
        // New`/`Repo` below and converts the clone it could not read about
        // to repo mode instead of reporting what is actually wrong.
        if let Some(clone) = crate::repo::workspace_clone_checked(root)? {
            // Listed, but the workspace's own `config/` is missing — a
            // worse case than nothing being set up here at all, since the
            // sibling clones `init` would otherwise ask nothing about still
            // share it. Left to fall through, this reads the same as a
            // checkout nobody has configured and writes a fresh default
            // `config.toml` into a folder other clones expect to find their
            // own setup in — see the `broken-workspace-skipped` task.
            if !clone.config_dir().is_dir() {
                bail!(
                    "{} is listed as a clone of {}, but {} does not exist\n  restore it by \
                     hand, or remove this checkout's entry from {} by hand to leave the \
                     workspace",
                    root.display(),
                    clone.workspace.display(),
                    clone.config_dir().display(),
                    clone.workspace.join(crate::repo::BINDING_FILE).display(),
                );
            }
            let name = clone
                .workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if args.setup == Some(Setup::Repo) {
                bail!(
                    "{} is already set up in home mode, in workspace {name} — init does not \
                     move a project out of its workspace\n  run `spoolway init` without \
                     --setup to repeat the setup in workspace {name}",
                    root.display()
                );
            }
            // A listed checkout stays where it is unless it is asked to
            // move: by `--workspace`, or by a person picking another
            // workspace in the menu. With nobody to ask, a repeat run is a
            // repeat run, as it always was.
            return Ok(match args.workspace.as_deref() {
                Some(wanted) if wanted == name => Self::Listed,
                Some(NEW_WORKSPACE) => Self::Move(None),
                Some(wanted) => Self::Move(Some(wanted.to_string())),
                None if !crate::ask::interactive() => Self::Listed,
                None => match pick_workspace(root, Some(&name))? {
                    Some(picked) if picked == name => Self::Listed,
                    picked => Self::Move(picked),
                },
            });
        }
        if crate::config::tracked_setup_dir_in(root).is_dir() {
            if asked_home {
                bail!(
                    "{} already has a tracked `.spoolway/` — init does not move a repo-mode \
                     project into a home workspace\n  run `spoolway init` without --setup home \
                     or --workspace to repeat the repo-mode setup",
                    root.display()
                );
            }
            return Ok(Self::Repo);
        }

        let setup = match args.setup {
            Some(setup) => setup,
            None if asked_home => Setup::Home,
            None => {
                // `repo` first and the default: it is what `init` did before
                // this question existed, so a script with nobody to answer
                // gets the same tracked `.spoolway/` it always did.
                let setups = <Setup as clap::ValueEnum>::value_variants();
                let menu: Vec<(&str, &str)> = setups
                    .iter()
                    .map(|setup| match setup {
                        Setup::Repo => ("repo", ".spoolway/ in this checkout, tracked by git"),
                        Setup::Home => ("home", "~/.spoolway/"),
                    })
                    .collect();
                setups[crate::ask::choose("Where should this project's setup live?", &menu, 0)?]
            }
        };
        if setup == Setup::Repo {
            return Ok(Self::Repo);
        }

        // The current branch carries no tracked `.spoolway/` — checked
        // above, or this checkout would already be `Self::Repo` — but a
        // home-mode setup here still conflicts with one tracked on the
        // project's default branch: switching back finds `.spoolway/`
        // again, and every command run meanwhile wrote its state into
        // `~/.spoolway/` instead. See
        // `crate::repo::default_branch_tracking_spoolway`'s own doc.
        if let Some((branch, is_default)) = crate::repo::default_branch_tracking_spoolway(root) {
            if is_default {
                bail!(
                    "{} tracks `.spoolway/` on its default branch, `{branch}` — this checkout \
                     is not on it now, but a home-mode setup here would still conflict with it \
                     once `{branch}` is checked out again\n  check out `{branch}` and run \
                     `spoolway init` there instead",
                    root.display(),
                );
            }
            bail!(
                "{} has no default branch spoolway can name, and the local branch `{branch}` \
                 tracks `.spoolway/` — a home-mode setup here would conflict with it once \
                 `{branch}` is checked out\n  if `{branch}` is the project's trunk, check it \
                 out and run `spoolway init` there; if it is stale, remove `.spoolway/` from it \
                 or delete the branch, then run this again",
                root.display(),
            );
        }

        // A repo-mode project that deleted its `.spoolway/` still has a home
        // under `~/.spoolway/` keyed by the id in its `.git`, and its queue
        // stays there. A home-mode workspace starts an empty queue of its own,
        // so those tasks would sit unseen in the old home.
        let stranded = crate::repo::repo_mode_queued_tasks(root)?;
        if !stranded.is_empty() {
            bail!(
                "{} still has tasks queued from its repo-mode setup: {} — home mode would \
                 start an empty queue and leave them behind\n  queue commands do not work \
                 here while `.spoolway/` is missing, so run `spoolway init` to restore repo \
                 mode, carry each task back to pending with `spoolway queue unqueue <id>` (or \
                 let it finish), remove `.spoolway/` again, then run `spoolway init --setup \
                 home`; or stay in repo mode with `spoolway init`",
                root.display(),
                stranded.join(", "),
            );
        }

        let workspaces = crate::repo::workspaces();
        match args.workspace.as_deref() {
            Some(NEW_WORKSPACE) => Ok(Self::New),
            Some(name) => {
                if workspaces.iter().any(|workspace| workspace.name == name) {
                    Ok(Self::Join(name.to_string()))
                } else {
                    let known: Vec<&str> = workspaces.iter().map(|w| w.name.as_str()).collect();
                    bail!(
                        "--workspace {name}: no workspace by that name under {} — pick one of \
                         [{}], or `new`",
                        crate::repo::shorten_home(&crate::mux::state_root()),
                        known.join(", ")
                    )
                }
            }
            None if workspaces.is_empty() => Ok(Self::New),
            // Nobody to ask: a new workspace rather than the menu's default
            // — the same default the interactive menu takes when Enter is
            // pressed without reading it, so the two agree even when a
            // workspace already holds this very repository. That case is
            // said out loud: a script that used to get a new workspace here
            // still gets one, and is told which flag joins the other instead
            // on a later run, where a listed checkout given `--workspace`
            // moves there.
            None if !crate::ask::interactive() => {
                let mine = crate::repo::root_commit(root);
                if let Some(existing) = workspaces
                    .iter()
                    .find(|workspace| workspace.holds_repository_of(root, mine.as_deref()))
                {
                    println!(
                        "  note  workspace {name} already holds a clone of this repository.\n        \
                         Starting a new workspace, because nobody is here to pick one.\n        \
                         To use {name} instead, run `spoolway init --workspace {name}`.",
                        name = existing.name,
                    );
                }
                Ok(Self::New)
            }
            None => Ok(match pick_workspace(root, None)? {
                Some(name) => Self::Join(name),
                None => Self::New,
            }),
        }
    }
}

/// The workspace menu's question, the same for a checkout joining a
/// workspace and for one moving to another.
const WORKSPACE_QUESTION: &str = "Select the spoolway workspace for this checkout.\nPick an existing workspace or create a new one.";

/// The menu entry, last on the workspace menu, that starts a new workspace.
const NEW_WORKSPACE_ENTRY: &str = "Create a new workspace";

/// Ask which workspace this checkout uses, and answer the folder name picked,
/// or `None` for [`NEW_WORKSPACE_ENTRY`].
///
/// `current` is the workspace a listed checkout uses now. It comes first,
/// marked `current`, and is the default, so Enter changes nothing. Only
/// workspaces it may move into follow it — never one of another repository,
/// see [`crate::repo::WorkspaceSummary::may_move_into`]. An unlisted checkout
/// sees the workspaces it may join, those of its own repository first, never
/// one that is provably another's (see
/// [`crate::repo::WorkspaceSummary::may_join`]), and defaults to
/// a new workspace: joining shares one setup with every clone already in the
/// chosen workspace, so pressing Enter without reading must never land there.
///
/// The note beside a workspace is `same repository` when it holds a clone of
/// this one, and otherwise the repository it holds. The marker is the whole
/// note, so the line fits 80 columns however long that repository's origin
/// is; [`crate::ask::choose`] cuts a row at the terminal's width from the
/// right, and an origin after the marker used to be the part lost.
fn pick_workspace(root: &Path, current: Option<&str>) -> Result<Option<String>> {
    let mine = crate::repo::root_commit(root);
    let mut workspaces = crate::repo::workspaces();
    if let Some(current) = current {
        workspaces.retain(|workspace| {
            workspace.name == current || workspace.may_move_into(root, mine.as_deref())
        });
    } else {
        workspaces.retain(|workspace| workspace.may_join(root, mine.as_deref()));
    }
    // A stable sort: `workspaces()` is already sorted by name, and nothing
    // here should reorder two workspaces that agree on where they belong.
    workspaces.sort_by_key(|workspace| {
        (
            Some(workspace.name.as_str()) != current,
            !workspace.holds_repository_of(root, mine.as_deref()),
        )
    });
    let notes: Vec<String> = workspaces
        .iter()
        .map(|workspace| {
            if Some(workspace.name.as_str()) == current {
                "current".to_string()
            } else if workspace.holds_repository_of(root, mine.as_deref()) {
                "same repository".to_string()
            } else {
                workspace
                    .repo_display
                    .clone()
                    .unwrap_or_else(|| "no checkouts".to_string())
            }
        })
        .collect();
    let mut menu: Vec<(&str, &str)> = workspaces
        .iter()
        .zip(&notes)
        .map(|(workspace, note)| (workspace.name.as_str(), note.as_str()))
        .collect();
    menu.push((NEW_WORKSPACE_ENTRY, ""));
    let default = match current {
        Some(current) => workspaces
            .iter()
            .position(|workspace| workspace.name == current)
            .unwrap_or(workspaces.len()),
        None => workspaces.len(),
    };
    let picked = crate::ask::choose(WORKSPACE_QUESTION, &menu, default)?;
    Ok(workspaces
        .get(picked)
        .map(|workspace| workspace.name.clone()))
}

/// The `--workspace` value that starts a new workspace rather than naming
/// one — what [`NEW_WORKSPACE_ENTRY`] answers on the menu.
const NEW_WORKSPACE: &str = "new";

/// `path` as `init`'s report rows name it: relative to the checkout when it
/// sits inside it, as every repo-mode row always has, and with `~` for the
/// home directory otherwise — a home-mode workspace's files, and nothing
/// else `init` writes, sit outside the checkout.
fn shown(root: &Path, path: &Path) -> String {
    if path.starts_with(root) {
        relative(root, path)
    } else {
        crate::repo::shorten_home(path)
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

/// One row of the mockup's report block: `verb` (`project`, `wrote`, `kept`,
/// `made`, `set` or `skipped`) against `what` — the resolved root for
/// `project`, a path relative to the project root for
/// `wrote`/`kept`/`made`/`skipped`, or — for `set` — a `key = value` pair.
/// Every verb is left-padded to nine columns — `project` and `skipped` plus
/// two spaces, `wrote` plus four, `kept` and `made` plus five, `set` plus
/// six — so all six line up whichever one a row starts with.
fn report_row(verb: &str, what: &str) -> String {
    format!("  {verb:<9}{what}")
}

/// Writes what `init` places and keeps the report rows for it.
///
/// Every file or folder considered gets one row, in call order, rather than
/// an aggregate count: a repeat `init` that adds nothing new reports `kept`
/// for everything it considered, and a project adding its own pipeline or
/// prompt directory later means a fixed set of buckets could not name it.
/// A struct rather than a closure so the empty-folder rows and the `set`
/// rows can land in the same list between file writes.
struct Placer {
    /// `--force`: rewrite a file that already exists instead of keeping it.
    force: bool,
    /// One `wrote`/`kept`/`made`/`set` row per thing considered.
    rows: Vec<String>,
    /// Whether any file already there was left as it was. Such a file is
    /// whatever an older version wrote, so `init` cannot vouch that the
    /// project is current and leaves the update stamp to `sync`.
    kept_a_file: bool,
}

impl Placer {
    /// Write `contents` to `path` unless it exists and `force` is off.
    /// Whether it was actually written.
    fn file(
        &mut self,
        path: std::path::PathBuf,
        rel: &str,
        contents: &[u8],
        exec: bool,
    ) -> Result<bool> {
        if path.exists() && !self.force {
            self.rows.push(report_row("kept", rel));
            self.kept_a_file = true;
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
        self.rows.push(report_row("wrote", rel));
        Ok(true)
    }

    /// Create the empty folder `dir`: `made` when this run created it, `kept`
    /// when it was already there, the same pair a file gets. Whether it was
    /// actually created.
    fn dir(&mut self, dir: &Path, rel: &str) -> Result<bool> {
        if dir.is_dir() {
            self.rows.push(report_row("kept", rel));
            return Ok(false);
        }
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        self.rows.push(report_row("made", rel));
        Ok(true)
    }
}

/// The example setup: the shipped pipelines, prompts, task templates and
/// ticket templates, each handed to [`Placer::file`] so it reports `wrote` or
/// `kept` in call order — except a pipeline whose name a private pipeline
/// already claims, reported `skipped` directly onto [`Placer::rows`] instead,
/// in the same call order. Whether anything was actually written.
fn place_examples(
    root: &Path,
    state: &Path,
    answers: &Answers,
    placer: &mut Placer,
) -> Result<bool> {
    let mut wrote = false;
    // One file per pipeline, named for the pipeline it holds. A project adds
    // its own by writing another file here and nothing else.
    for (name, body) in crate::pipeline::BUILTIN_PIPELINES {
        // A private pipeline already named this before the tracked file
        // existed — declined on first `init`, then claimed by `pipeline
        // copy` or written by hand — would otherwise have the tracked
        // example land right on top of it: `merge_private` bails on that
        // clash the moment anything next loads the project's pipelines, and
        // this run would have caused it rather than caught it. Skip the
        // shipped example instead; a repeat `init` picks it up again once
        // the private pipeline is renamed — `pipeline promote` is no
        // alternative route to the same thing, since promoting *becomes*
        // the tracked file: a repeat `init` after that would just report it
        // `kept`, with the shipped example still never restored.
        if crate::local::is_repo_mode(root)
            && let Some(private) = pipeline_file_in(
                &crate::local::pipelines_dir(&crate::local::dir_for(root)?),
                name,
            )
        {
            let rel = shown(root, &Pipelines::file_in(root, name));
            placer.rows.push(report_row(
                "skipped",
                &format!(
                    "{rel} — {} already uses the name `{name}`; rename it to restore the \
                     shipped `{name}`",
                    shown(root, &private)
                ),
            ));
            continue;
        }
        let path = Pipelines::file_in(root, name);
        let rel = shown(root, &path);
        wrote |= placer.file(path, &rel, answers.fill(body).as_bytes(), false)?;
    }
    // Written whole, and never looked at again. A prompt is the project's from
    // the moment `init` finishes: no sync rewrites one, so nothing here has to
    // be a shape a later binary can still find its way around in.
    for prompt in assets::PROMPTS {
        let dir = state.join("prompts").join(prompt.name);
        let path = dir.join(assets::PROMPT_FILE);
        let rel = shown(root, &path);
        wrote |= placer.file(path, &rel, prompt.body.as_bytes(), false)?;
        // A prompt's belongings follow its prose: written once, never updated,
        // and the project's to restyle from here on. No setting names them —
        // the prompt that fills them is the only thing that reads them.
        for (name, body) in prompt.assets {
            let path = dir.join(assets::PROMPT_ASSETS).join(name);
            let rel = shown(root, &path);
            wrote |= placer.file(path, &rel, body.as_bytes(), false)?;
        }
    }
    // One task skeleton per shipped pipeline, named for the pipeline that takes
    // it. A project adds a pipeline's shape by writing a file beside these, and
    // one that writes nothing takes `default`.
    for (name, skeleton) in assets::TASK_TEMPLATES {
        let path = crate::config::under_setup(state, crate::config::TASK_TEMPLATES_DIR)
            .join(format!("{name}.md"));
        let rel = shown(root, &path);
        wrote |= placer.file(path, &rel, skeleton.as_bytes(), false)?;
    }
    Ok(wrote)
}

/// A fresh or repeat run's own setup: `config.toml`, the example setup or
/// the empty folders standing in for it, and the hook scripts for a project
/// with a tracker — each handed to [`Placer`] so it reports in call order.
/// Whether anything was actually written. Not called for a checkout joining
/// a workspace, whose setup is the workspace's and already there.
fn place_setup(
    root: &Path,
    state: &Path,
    config: &Config,
    answers: &Answers,
    placer: &mut Placer,
) -> Result<bool> {
    // The end of the link chain, so a `config.toml` linked into a shared folder
    // is rewritten there rather than replaced, and a dangling link counts as
    // absent only if its target is.
    let mut wrote_any = placer.file(
        crate::config::write_target(&Config::path_in(root)),
        &shown(root, &Config::path_in(root)),
        config
            .render()
            .context("rendering default config")?
            .as_bytes(),
        false,
    )?;
    if answers.examples {
        wrote_any |= place_examples(root, state, answers, placer)?;
    } else {
        // The folders the examples would have filled, empty, so the
        // spoolway-config skill the closing line names has somewhere to
        // write.
        for dir in [
            Pipelines::dir_in(root),
            crate::config::under_setup(state, crate::config::PROMPTS_DIR),
            state.join("templates"),
        ] {
            let rel = format!("{}/", shown(root, &dir));
            wrote_any |= placer.dir(&dir, &rel)?;
        }
    }
    // The hook scripts, every one of them, but only for a project with a
    // tracker: `hooks/` exists exactly when a tracker is chosen, and with
    // `none` there is no folder at all. Every script rather than only the
    // chosen one, so switching trackers later is a `spoolway config set
    // issue_tracking.hook` away, not a second `init`. Driven by the tracker
    // actually in force, not only one just answered: a bare repeat `init`
    // never touches `answers.tracker` (see `Answers::tracker_touched`) but
    // still reports `kept` for the scripts an earlier run wrote.
    let tracker_in_force = if answers.tracker_touched {
        answers.tracker != Tracker::None
    } else {
        Config::load_tracked(root)
            .map(|existing| !existing.issue_tracking.hook.trim().is_empty())
            .unwrap_or(false)
    };
    if tracker_in_force {
        for (name, body) in assets::HOOK_SCRIPTS {
            let path = crate::tracking::hooks_dir_in(root).join(name);
            let rel = shown(root, &path);
            wrote_any |= placer.file(path, &rel, body.as_bytes(), true)?;
        }
    }
    // No plan skeleton here any more. spoolway-plan carries its own, under the
    // skill's own `assets/`, and writes a self-contained page with it — there
    // is nothing left for `init` to place in the project.

    Ok(wrote_any)
}

pub fn init(root: &Path, args: &InitArgs) -> Result<()> {
    // Before anything is written or asked, because this is the one command a
    // person runs without knowing yet what they have got hold of.
    print!("{}", crate::status::banner("setting up a project"));

    // A key pressed in a herdr pane opens this popup in whatever directory
    // herdr handed it — usually the one you are looking at, but there is no
    // way to be certain of that from in here. So every caller gets told the
    // root `init_root` resolved before anything below writes to it: a path
    // you do not recognise is the whole of the check. At a terminal the run
    // goes straight on to its first menu, if any question is left to ask,
    // and a person who sees the wrong path stops it there with Ctrl-C —
    // nothing is written before every question is answered. A run whose
    // flags answer everything shows no menu and writes right after the path.
    //
    // With nobody at a terminal there is nobody to read that path, so a
    // silent `init` writes nothing unless it was told to: a script or CI
    // runner that means it passes `--yes`, the same shape
    // `spoolway herdr bind --yes` already uses. That is also the one shape
    // an agent actually hits, since it has no terminal either.
    println!();
    println!("{}", report_row("project", &root.display().to_string()));
    if !args.yes && !crate::ask::interactive() {
        // The row names the flag that lets a silent run write, the one the
        // `spoolway-config` skill's `init` route must pass for exactly this
        // reason.
        println!(
            "{}",
            report_row(
                "wrote",
                "nothing — pass --yes to confirm with nobody here to answer"
            )
        );
        return Ok(());
    }

    // Where the setup lives comes first, because every path below depends on
    // it — but `choose` only decides, it never writes, so working this out
    // does not yet commit the checkout to anything. `joined` is what the
    // rest of the run keys the joining case off: that workspace's setup is
    // shared and already chosen, so nothing below writes into `config/`.
    //
    // These two come before `bind` below, which stamps the checkout's `.git`:
    // a path or repository `init` can never set up must leave nothing behind
    // there.
    crate::repo::require_utf8_root(root)?;
    crate::repo::require_git_repository(root)?;
    let placement = Placement::choose(root, args)?;
    // A move is refused here, right after the menu and before any question
    // or write, so a refusal leaves everything as it was.
    if let Placement::Move(to) = &placement {
        crate::repo::check_move(root, to.as_deref())?;
    }
    let joined = matches!(placement, Placement::Join(_) | Placement::Move(Some(_)));

    // Every flag validated and every question answered before the first
    // write below — `Answers::gather`/`Answers::joining` only read and ask,
    // they never touch disk. A bad `--tracker` value, or a Ctrl-C at the
    // agent menu, used to be caught only after `create_workspace`/
    // `join_workspace` had already listed this checkout and `bind` had
    // already stamped its `.git`, leaving a project half set up that the
    // next `init` then refused. Settling every answer first means a run
    // that fails here has written nothing at all.
    let answers = if joined {
        Answers::joining(root, args)?
    } else {
        Answers::gather(root, args, matches!(placement, Placement::Move(None)))?
    };

    // What the closing lines say about where this checkout went. `bound` is
    // the row a new workspace prints; a join or a move says it in a sentence
    // instead, as the last thing the run prints.
    let mut bound = None;
    let mut placed_lines = Vec::new();
    match &placement {
        Placement::New => bound = Some(crate::repo::create_workspace(root)?),
        Placement::Join(name) => {
            let (_, took_over) = crate::repo::join_workspace(root, name)?;
            placed_lines.push(match took_over {
                Some(old) => format!(
                    "Joined workspace {name}, taking over the entry for {} — its queue, \
                     archive and worktrees carry on here.",
                    old.display()
                ),
                None => format!("Joined workspace {name}."),
            });
        }
        Placement::Move(to) => {
            let moved = crate::repo::move_checkout(root, to.as_deref())?;
            let name = moved
                .clone
                .workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            placed_lines.push(format!("Moved to workspace {name}."));
            if let Some(emptied) = moved.emptied {
                placed_lines.push(format!(
                    "Workspace {} lists no checkout now. Its folder is kept:\n  {}\n\
                     Remove it yourself once nothing in it is needed.",
                    emptied
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    crate::repo::shorten_home(&emptied),
                ));
            }
        }
        Placement::Repo | Placement::Listed => {}
    }

    // A project's home is keyed off an id stamped into its own common git
    // directory, checked against that home's own record of which checkout
    // it belongs to — `crate::repo::bind` and friends, the whole of the
    // `binding-record` task. Every other command reaches the same check
    // through `Repo::discover`; `init` places, moves and re-attaches a
    // checkout through its own menu, so it calls `bind` directly rather
    // than `Repo::discover` itself.
    //
    // `already_stamped` is read before `bind` runs, since that call may be
    // the very one that mints this checkout's id for the first time now —
    // criterion 7, "no stamp where nothing records it binds itself once",
    // no `spoolway init` required first any more. It is what lets the
    // mockup's own "stamped" line, printed further down at its own
    // position, tell a checkout that was freshly minted apart from a
    // repeat run that only read its id back.
    let stamp_path = crate::repo::id_file_path(root)?;
    let already_stamped = stamp_path.as_deref().is_some_and(|path| path.exists());
    // Nothing to say unless the binding itself had something to record — a
    // moved checkout prints its own one line from inside `bind` (acceptance
    // criterion 2); a fresh one, silent criterion 7, stays silent here too.
    crate::repo::bind(root)?;
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

    // Read after binding rather than off `placement`, so a checkout just
    // joined or taken over through the menu counts as home mode too, and
    // is kept out of its checkout exactly as a fresh one is.
    let home_mode = crate::repo::workspace_clone(root).is_some();

    // A repeat run is how a project adds another provider's skills. Keep that
    // successful outcome distinct from creating (or deliberately replacing)
    // the project's scaffold. Read before anything below writes `config.toml`,
    // and after binding, so a home-mode clone just joined or re-attached
    // reads its workspace's existing `config.toml` rather than the
    // checkout's none.
    let already_initialized = Config::path_in(root).exists() && !args.force;

    let state = crate::config::setup_dir_in(root);
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
    // A `--force` run rewrites the config but, with no tracker answer this
    // run, the project's own tracker is carried across rather than blanked.
    if !answers.tracker_touched
        && args.force
        && !joined
        && let Ok(existing) = Config::load_tracked(root)
    {
        config.issue_tracking.hook = existing.issue_tracking.hook;
        config.issue_tracking.project_key = existing.issue_tracking.project_key;
    }
    let config = config;

    // Only the tracked control plane. `queue/` and `archive/` moved out of the
    // checkout — see `crate::repo::Repo::home` — and are created silently by
    // the accessors that resolve them, the first time anything asks for one.
    // The setup folders themselves are made below, by whatever lands in them
    // or, without the examples, as the empty folders the mockup draws.

    // `placer.rows` collects one row per file or folder `init` considered,
    // in call order — see [`Placer`] — and `wrote_any` whether any of them
    // was actually written.
    let mut placer = Placer {
        force: args.force,
        rows: Vec::new(),
        kept_a_file: false,
    };
    let mut wrote_any = false;

    if joined {
        // The workspace's setup, shared with every clone already in it, is
        // left exactly as it is, and the closing line names the workspace.
    } else {
        // `--force` in a home-mode clone rewrites `state` — this workspace's
        // shared `config/`, read by every other clone's own `init`/`sync` —
        // not just this checkout's own setup. Naming the others first is the
        // one thing standing between a person meaning to reset their own
        // config and silently resetting a teammate's `lane_quiet` too.
        if args.force
            && let Some(clone) = crate::repo::workspace_clone(root)
        {
            let siblings = crate::repo::sibling_clones(&clone.workspace, root);
            if !siblings.is_empty() {
                println!(
                    "  note  --force rewrites {}, shared by every clone below — their setup \
                     resets too:\n{}",
                    shown(root, &state),
                    siblings
                        .iter()
                        .map(|clone_root| format!("           {}", clone_root.display()))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
        wrote_any |= place_setup(root, &state, &config, &answers, &mut placer)?;
    }

    // `--tracker`, given bare or with a value, answers `[issue_tracking]`
    // even on a project that already has a `config.toml` — `Placer::file` above
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
        placer.rows.push(report_row(
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
            placer.rows.push(report_row(
                "set",
                &format!(
                    "issue_tracking.project_key = {}",
                    crate::confkv::get(&updated, "issue_tracking.project_key")?
                ),
            ));
        }
    }

    // Not written through `Placer` either: this only ever removes, and never
    // creates a `.gitignore` a project did not already have. See
    // [`crate::gitignore`] — runtime state moved out of the checkout, so
    // there are no rules left to write, only an old block to take back out.
    // Not in home mode, which promises to write nothing into the checkout:
    // taking an old block out is still an edit to a tracked file there.
    let mut notes = Vec::new();
    let ignore_file = relative(root, &crate::gitignore::file(root));
    let removed = if home_mode {
        crate::gitignore::Removed::Absent
    } else {
        crate::gitignore::remove(root, false)?
    };
    match removed {
        crate::gitignore::Removed::Gone => notes.push(format!(
            "  removed {ignore_file} (spoolway's rules are gone)"
        )),
        crate::gitignore::Removed::Absent => {}
        crate::gitignore::Removed::Unterminated => notes.push(format!(
            "  !       {ignore_file}: `{}` with no `{}` — left alone, restore the marker \
             or delete the block",
            assets::IGNORE_BEGIN,
            assets::IGNORE_END
        )),
    }

    crate::usage::registry::register(root);

    for row in &placer.rows {
        println!("{row}");
    }
    for note in &notes {
        println!("{note}");
    }
    if let Some(line) = &stamped_line {
        println!("{line}");
    }
    // Home mode's counterpart to the `stamped` line: the workspace entry this
    // run added is the whole of the binding, so it is named instead.
    if let Some(clone) = &bound {
        println!(
            "{}",
            report_row(
                "bound",
                &format!(
                    "{}  ->  {}/",
                    root.display(),
                    crate::repo::shorten_home(&clone.home_dir())
                )
            )
        );
    }
    // The skills, in the provider's own convention. Run from here rather than
    // suggested, because "and now run this other command" is the manual step
    // this exists to remove — and a project that skipped it had skills that
    // were shipped, documented, and never installed. A home-mode project
    // takes them in the agent's user folder, since its project folder sits
    // inside the checkout.
    let installed = if home_mode {
        crate::install::install_user(answers.provider, args.force)?
    } else {
        crate::install::install(root, answers.provider, args.force)?
    };
    // Folded in before the report consumes `installed`: a repeat run whose
    // scaffold rows all came back `kept` but whose skills were installed for
    // the first time (a provider switch, say) has written something, and
    // must not also claim below that there was "nothing to install" right
    // next to "Skills installed successfully" saying otherwise.
    wrote_any |= installed.wrote;
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
    for line in &placed_lines {
        println!("{line}");
    }
    if joined {
        // A clone joining a workspace, or moving into one, uses a setup
        // whoever started that workspace settled, so the next-step lines
        // below are not this run's to repeat: the line above is the whole
        // of what it says.
    } else if !already_initialized {
        println!("Project initialized successfully.");
        if answers.examples {
            println!(
                "Set model and effort on every agent step in {}/*.yml before dispatching.",
                shown(root, &Pipelines::dir_in(root))
            );
        } else {
            // No pipeline exists yet, so there is no step to set anything on:
            // the one next step is writing a pipeline, and the skill does it.
            println!("Use the spoolway-config skill to create pipelines.");
        }
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

    /// `InitArgs::default()` with `--yes` given — what a script that means
    /// it passes.
    ///
    /// Every test below that expects a project on disk starts from this
    /// rather than from `InitArgs::default()`, because a default `InitArgs`
    /// with nobody at a terminal was not told it may write: it writes
    /// nothing, which is precisely what
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
    fn scaffold(name: &str, args: &InitArgs) -> crate::scratch::ScratchRoot {
        let root = crate::scratch::root(&format!("init-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        run_init(&root, args).expect("init");
        root
    }

    /// Acceptance criterion 5: `init` restoring an example pipeline whose
    /// name a private pipeline already uses warns and skips it, rather than
    /// writing the tracked example right on top of a name `merge_private`
    /// would then refuse as a clash the moment anything next loads the
    /// project's pipelines.
    #[test]
    fn init_skips_an_example_pipeline_a_private_one_already_names() {
        let root = scaffold(
            "examples-skip-private-clash",
            &InitArgs {
                no_examples: true,
                ..confirmed()
            },
        );
        crate::platform::test_home::with_home(&home_for(&root), || {
            let local = crate::local::dir_for(&root).unwrap();
            let dir = crate::local::pipelines_dir(&local);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("default.yml"),
                "steps:\n  - id: solo\n    agent: pi\n    model: m\n    on_pass: done\n",
            )
            .unwrap();
        });

        run_init(
            &root,
            &InitArgs {
                examples: true,
                ..confirmed()
            },
        )
        .expect("a repeat init with examples still succeeds even when one is skipped");

        assert!(
            !Pipelines::file_in(&root, "default").exists(),
            "the tracked `default` example must not be written over the private pipeline \
             already using that name"
        );
        assert!(
            Pipelines::file_in(&root, "bugfix").exists(),
            "an example whose name has no private clash is still written"
        );
    }

    /// Acceptance criterion 6: a repo-mode project `init` already set up —
    /// a home under `~/.spoolway/` holding a plain `id`/`root` binding with
    /// no `clones` key, and `.git/spoolway-id` stamped to match, exactly
    /// the shape spoolway 0.6.0 wrote and the only shape repo mode has ever
    /// written — still binds on a repeat run after the reorder that moved
    /// `Answers::gather` ahead of `bind`, and that reorder adds no new
    /// folder directly under `~/.spoolway/`: `Answers::gather`'s own
    /// questions never touch the home directory at all for a project in
    /// repo mode.
    #[test]
    fn a_repeat_init_on_a_0_6_0_shaped_repo_mode_home_still_binds_to_it() {
        let root = scaffold("repo-mode-0-6-0", &confirmed());
        let home = home_for(&root);
        let bound_before = crate::platform::test_home::with_home(&home, || {
            crate::mux::project_home(&root).unwrap()
        });
        let before: std::collections::BTreeSet<String> = std::fs::read_dir(home.join(".spoolway"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();

        run_init(&root, &confirmed()).expect("a repeat init still succeeds");

        let bound_after = crate::platform::test_home::with_home(&home, || {
            crate::mux::project_home(&root).unwrap()
        });
        assert_eq!(
            bound_before, bound_after,
            "the repeat run binds to the same home"
        );
        let after: std::collections::BTreeSet<String> = std::fs::read_dir(home.join(".spoolway"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            before, after,
            "the repeat run — and the workspace lock it never takes in repo mode — adds no \
             folder directly under ~/.spoolway/"
        );
    }

    /// Every pipeline file `init` wrote, concatenated. What a step names is
    /// the question every test below asks of them, and which file it is in is
    /// not.
    fn pipelines_on_disk(root: &Path) -> String {
        let dir = root.join(crate::config::STATE_DIR).join("pipelines");
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

    /// Home mode used to call `create_workspace`/`join_workspace` before any
    /// git check at all — a non-git folder was accepted into a fresh
    /// workspace, registered by a path that no later command could ever
    /// walk back up from with a bounded search. `--setup home` must refuse
    /// it exactly as the ordinary repo-mode case above does.
    #[test]
    fn init_setup_home_refuses_a_directory_with_no_git_repository() {
        let parent = crate::scratch::root("init-home-no-git");
        let home = parent.join("home");
        let root = parent.join("nogit");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let err =
            crate::platform::test_home::with_home(&home, || init(&root, &home_args(NEW_WORKSPACE)))
                .expect_err("no .git here at all");
        let said = format!("{err:#}");
        assert!(
            said.contains("git repository"),
            "the refusal names the actual reason: {said}"
        );
        assert!(
            crate::repo::workspace_clone(&root).is_none(),
            "a refused checkout must never end up listed in a workspace"
        );
    }

    /// Acceptance: `.spoolway/` tracked on the repository's default branch
    /// refuses `--setup home` even from a branch that carries no
    /// `.spoolway/` of its own — an orphan branch's empty working tree used
    /// to pass the ordinary "is there a `.spoolway/` here" check and accept
    /// a home-mode setup that broke the moment the default branch came
    /// back.
    #[test]
    fn home_mode_is_refused_when_the_default_branch_tracks_spoolway() {
        let root = crate::scratch::root("init-home-tracked-on-default");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        std::fs::create_dir_all(root.join(".spoolway")).unwrap();
        std::fs::write(root.join(".spoolway").join("config.toml"), "").unwrap();
        crate::repo::run(&root, "git", &["add", "-A"]).unwrap();
        crate::repo::run(&root, "git", &["commit", "-q", "-m", "init"]).unwrap();
        crate::repo::run(&root, "git", &["checkout", "-q", "--orphan", "scratch"]).unwrap();
        crate::repo::run(&root, "git", &["rm", "-rf", "-q", "."]).unwrap();
        assert!(
            !root.join(".spoolway").exists(),
            "the orphan branch has none"
        );

        let home = crate::scratch::root("init-home-tracked-on-default-home");
        let err =
            crate::platform::test_home::with_home(&home, || init(&root, &home_args(NEW_WORKSPACE)))
                .expect_err("a home-mode setup must not ignore .spoolway/ tracked on main");
        let said = format!("{err:#}");
        assert!(
            said.contains("default branch") && said.contains("main"),
            "the refusal names the branch: {said}"
        );
        assert!(
            crate::repo::workspace_clone(&root).is_none(),
            "nothing is joined when the refusal fires"
        );
    }

    /// A repository whose default branch `trunk` tracks `.spoolway/`, left on
    /// an orphan branch that carries none. `origin/HEAD` is not set, so only
    /// the later rules can find `trunk`.
    fn trunk_tracking_spoolway(name: &str) -> crate::scratch::ScratchRoot {
        let root = crate::scratch::root(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "trunk"]);
        std::fs::create_dir_all(root.join(".spoolway")).unwrap();
        std::fs::write(root.join(".spoolway").join("config.toml"), "").unwrap();
        crate::repo::run(&root, "git", &["add", "-A"]).unwrap();
        crate::repo::run(&root, "git", &["commit", "-q", "-m", "init"]).unwrap();
        crate::repo::run(&root, "git", &["checkout", "-q", "--orphan", "feature"]).unwrap();
        crate::repo::run(&root, "git", &["rm", "-rf", "-q", "."]).unwrap();
        root
    }

    /// With no `origin/HEAD`, `main` or `master`, a default branch named
    /// `trunk` is found through `init.defaultBranch` when it is set, and
    /// otherwise through the branch that tracks `.spoolway/` — either way
    /// home mode is refused from a branch that does not carry it.
    #[test]
    fn home_mode_is_refused_when_an_unconventional_default_branch_tracks_spoolway() {
        for configured in [true, false] {
            let root = trunk_tracking_spoolway("init-home-tracked-on-trunk");
            // Set locally either way: an unset one would fall through to a
            // global `init.defaultBranch`, which may name `trunk` and answer
            // the scan case as the default. A branch that does not exist
            // shadows it and still reaches the scan.
            let named = if configured {
                "trunk"
            } else {
                "no-such-branch"
            };
            crate::repo::run(&root, "git", &["config", "init.defaultBranch", named]).unwrap();
            let home = crate::scratch::root("init-home-tracked-on-trunk-home");
            let err = crate::platform::test_home::with_home(&home, || {
                init(&root, &home_args(NEW_WORKSPACE))
            })
            .expect_err("home mode must be refused while trunk tracks .spoolway/");
            let said = format!("{err:#}");
            assert!(
                said.contains("trunk"),
                "names the branch (configured={configured}): {said}"
            );
            assert_eq!(
                said.contains("no default branch spoolway can name"),
                !configured,
                "only a branch found by scanning is not called the default: {said}"
            );
            assert!(
                crate::repo::workspace_clone(&root).is_none(),
                "nothing is joined when the refusal fires"
            );
        }
    }

    /// A repo-mode project that deleted `.spoolway/` keeps its queue in the
    /// home its `.git` stamp names. Home mode would start an empty queue and
    /// leave those tasks behind, so it is refused, naming them.
    #[test]
    fn home_mode_is_refused_while_the_repo_mode_home_holds_tasks() {
        let root = scaffold("init-home-over-repo-queue", &confirmed());
        let home = home_for(&root);
        crate::platform::test_home::with_home(&home, || {
            let queue = crate::mux::project_home(&root)
                .unwrap()
                .join(crate::config::QUEUE_DIR);
            std::fs::create_dir_all(&queue).unwrap();
            std::fs::write(
                queue.join("sw1.md"),
                "---\nid: sw1\ntitle: sw1\nstage: queued\n---\n## Goal\n\nx\n",
            )
            .unwrap();
            std::fs::remove_dir_all(root.join(".spoolway")).unwrap();

            let err = init(&root, &home_args(NEW_WORKSPACE))
                .expect_err("the repo-mode queue must block a switch to home mode");
            let said = format!("{err:#}");
            assert!(said.contains("sw1"), "names the task: {said}");
            assert!(
                crate::repo::workspace_clone(&root).is_none(),
                "nothing is joined when the refusal fires"
            );
        });
    }

    /// A bare repository is a git repository, so telling the person to run
    /// `git init` there names the wrong cause.
    #[test]
    fn init_names_a_bare_repository_as_the_cause_not_a_missing_one() {
        let root = crate::scratch::root("init-bare-repo");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["--bare"]);
        let err = run_init(&root, &confirmed()).expect_err("a bare repository has no checkout");
        let said = format!("{err:#}");
        assert!(said.contains("bare"), "{said}");
        assert!(!said.contains("`git init` here"), "{said}");
    }

    /// A file where `.spoolway/` would go is named before anything is
    /// written, so `.git` is not left stamped by a run that then fails.
    #[test]
    fn a_file_named_spoolway_is_refused_before_the_git_directory_is_stamped() {
        let root = crate::scratch::root("init-spoolway-is-a-file");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        std::fs::write(root.join(".spoolway"), "not a folder").unwrap();

        let err = run_init(&root, &confirmed()).expect_err("`.spoolway` is a file");
        let said = format!("{err:#}");
        assert!(said.contains("not a folder"), "{said}");
        assert!(
            !root.join(".git").join("spoolway-id").exists(),
            "nothing is stamped into .git"
        );
    }

    /// A join that takes over a gone entry says so, by answering the old
    /// root, where an ordinary join answers none.
    #[test]
    fn a_takeover_join_reports_the_entry_it_took_over() {
        let parent = crate::scratch::root("init-home-takeover-reported");
        let home = parent.join("home");
        let first = committed_checkout(&parent, "api");
        let second = clone_of(&first, &parent.join("elsewhere"), "api2");

        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            let name = workspace_name_of(&first);
            std::fs::remove_dir_all(&first).unwrap();
            let (_, took_over) = crate::repo::join_workspace(&second, &name).unwrap();
            assert_eq!(took_over.as_deref(), Some(first.as_path()));
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// `$HOME` being a real git repository (a dotfiles checkout) used to
    /// reach `setup_dir_in`'s own fallback for "nothing tracked here yet",
    /// which joins `.spoolway` onto `$HOME` — exactly
    /// `crate::mux::state_root()` — and then tried to write project files
    /// through the NUL-poisoned path `tracked_setup_dir_in` hands back for
    /// that one identity collision, failing deep inside `init` with a raw
    /// "file name contained an unexpected NUL byte" rather than a real
    /// refusal. `Placement::choose` must catch this first, by name, before
    /// anything is written at all.
    #[test]
    fn repo_mode_init_refuses_outright_when_the_checkout_is_home_itself() {
        let home = crate::scratch::root("init-home-is-the-checkout");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        crate::scratch::git_init(&home, &["-b", "main"]);

        let err = crate::platform::test_home::with_home(&home, || init(&home, &confirmed()))
            .expect_err("$HOME itself can never be a project checkout");
        let said = format!("{err:#}");
        assert!(
            said.contains("your home directory") && said.contains("state directory"),
            "the refusal names the actual reason, with no raw NUL-byte error: {said}"
        );
        assert!(
            !home.join(".git").join("spoolway-id").exists(),
            "nothing must be stamped before the refusal"
        );
    }

    /// Home mode writes nothing into the checkout, so `~/.spoolway` being
    /// spoolway's own state directory is no conflict for it. A dotfiles
    /// repository in `$HOME` must still be set up as a home-mode clone, not
    /// caught by the refusal the repo-mode test above checks.
    #[test]
    fn home_mode_init_in_a_git_home_lists_home_as_a_clone() {
        let home = crate::scratch::root("init-home-mode-in-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        crate::scratch::git_init(&home, &["-b", "main"]);

        crate::platform::test_home::with_home(&home, || {
            init(&home, &home_args(NEW_WORKSPACE)).expect("home mode in $HOME");
            assert!(
                crate::repo::workspace_clone(&home).is_some(),
                "$HOME is listed as a clone of the new workspace"
            );
        });
    }

    /// Home mode stamps nothing into a clone, so a `--separate-git-dir`
    /// clone's linked worktrees have no recorded path to find the main
    /// checkout by, and the parent of the git directory is not it. They
    /// have to be matched to the workspace entry by common git directory,
    /// or every lane worktree cut from such a clone reads "no spoolway
    /// project found" — the task's own third repro.
    #[test]
    fn a_home_mode_separate_git_dir_clone_is_found_from_its_linked_worktree() {
        let parent = crate::scratch::root("init-home-separate-git-dir");
        let _ = std::fs::remove_dir_all(&parent);
        let home = parent.join("home");
        std::fs::create_dir_all(parent.join("repos")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let work = parent.join("work");
        let git_dir = parent.join("repos").join("foo.git");
        crate::repo::run(
            &parent,
            "git",
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--separate-git-dir={}", git_dir.display()),
                work.to_str().unwrap(),
            ],
        )
        .unwrap();
        for (key, value) in [("user.email", "t@example.com"), ("user.name", "t")] {
            crate::repo::run(&work, "git", &["config", key, value]).unwrap();
        }
        std::fs::write(work.join("f.txt"), "x").unwrap();
        crate::repo::run(&work, "git", &["add", "f.txt"]).unwrap();
        crate::repo::run(&work, "git", &["commit", "-q", "-m", "x"]).unwrap();
        let wt = parent.join("task-wt");
        crate::repo::run(
            &work,
            "git",
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/a",
                wt.to_str().unwrap(),
            ],
        )
        .unwrap();

        crate::platform::test_home::with_home(&home, || {
            init(&work, &home_args(NEW_WORKSPACE)).expect("home-mode init");
            let repo = crate::repo::Repo::discover(&wt).expect("the worktree finds its project");
            assert_eq!(
                crate::platform::PathExt::canonical(&repo.root).unwrap(),
                crate::platform::PathExt::canonical(&work).unwrap(),
                "the worktree resolves to the listed main checkout"
            );
        });
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
            !said.contains("is not a git repository") && !said.contains("bare git repository"),
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

    /// `--provider pi` is accepted at `init`, the same as `install` and the
    /// docs already list it — `PlanningAgent` used to offer only Claude and
    /// Codex, rejecting it outright.
    #[test]
    fn init_accepts_provider_pi_and_scaffolds_its_profile() {
        let root = scaffold(
            "provider-pi",
            &InitArgs {
                provider: Some(PlanningAgent::Pi),
                ..confirmed()
            },
        );

        let config = Config::load(&root).unwrap();
        assert_eq!(config.unattended.blocked_agent, "pi");

        let pipelines = pipelines_on_disk(&root);
        assert!(pipelines.contains("agent: pi"), "{pipelines}");
        assert!(!pipelines.contains("agent: claude"), "{pipelines}");

        assert!(root.join(".pi").join("skills").is_dir());
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
            borrowed: false,
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
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

    /// Answering `github` writes the hook name and the project key into
    /// `[issue_tracking]`, and the executable script it names actually lands
    /// on disk, chmod'd so `crate::tracking`'s hook runner can exec it
    /// directly rather than being handed a path with no execute bit. Every
    /// script is written, not only the chosen one's — switching between
    /// trackers later is a config edit, not a second `init`.
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
        // `IssueTrackingConfig::default` now turns this on, so a project
        // picking a tracker gets names that carry its key from the start.
        assert!(config.contains("key_in_names = true"), "{config}");
    }

    /// Answering `none` leaves the table empty, which is what turns issue
    /// tracking off in `crate::tracking`, and writes no `hooks/` folder at
    /// all. A `--project-key` given alongside `none` is dropped, not
    /// half-applied.
    #[test]
    fn answering_none_writes_no_hooks_and_leaves_the_table_empty() {
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
        assert!(!crate::tracking::hooks_dir_in(&root).exists());
    }

    /// A repeat `--tracker github` with no `--project-key` and nobody to
    /// answer the free-text question must keep the project's own key,
    /// guarding `Answers::tracker`'s fallback to the key already on disk
    /// rather than the blank `ask::line` itself returns with nobody there.
    #[test]
    fn a_repeat_tracker_answer_with_no_project_key_keeps_the_existing_one() {
        let root = scaffold(
            "tracker-repeat-key",
            &InitArgs {
                tracker: Some("github".into()),
                project_key: Some("acme/app".into()),
                ..confirmed()
            },
        );

        run_init(
            &root,
            &InitArgs {
                tracker: Some("github".into()),
                ..confirmed()
            },
        )
        .expect("repeat init");

        let config = std::fs::read_to_string(Config::path_in(&root)).unwrap();
        assert!(
            config.contains("project_key = \"acme/app\""),
            "a repeat --tracker with no --project-key and nobody to ask must keep the \
             project's own key instead of blanking it: {config}"
        );
    }

    /// A repeat `init` with no `--provider` and a missing `pipelines/`
    /// folder is documented as the way to restore it, and must restore it
    /// for the project's own configured agent — guarding
    /// `Answers::provider`'s fallback to `config.unattended.blocked_agent`
    /// with nobody to ask, rather than the menu's first entry, `claude`,
    /// which would restore claude pipelines (and install claude skills) onto
    /// a project set up for codex.
    #[test]
    fn a_repeat_init_restores_the_projects_own_provider_not_claude() {
        let root = scaffold(
            "repeat-provider",
            &InitArgs {
                provider: Some(PlanningAgent::Codex),
                ..confirmed()
            },
        );
        std::fs::remove_dir_all(Pipelines::dir_in(&root)).unwrap();

        run_init(&root, &confirmed()).expect("repeat init restores the missing pipelines");

        let pipelines = pipelines_on_disk(&root);
        assert!(
            pipelines.contains("agent: codex"),
            "a repeat init with no --provider must restore files for the project's own \
             configured provider, not fall back to claude: {pipelines}"
        );
        assert!(
            !root.join(".claude").join("skills").is_dir(),
            "and must install only that provider's skills: {}",
            root.display()
        );
    }

    /// `init --force` rewrites the scaffold's files but keeps the choices the
    /// project already made: its provider and its tracker stay unless
    /// `--provider` or `--tracker` is passed.
    #[test]
    fn a_forced_init_keeps_the_projects_provider_and_tracker() {
        let root = scaffold(
            "force-keeps-choices",
            &InitArgs {
                provider: Some(PlanningAgent::Codex),
                tracker: Some("github".into()),
                project_key: Some("o/r".into()),
                ..confirmed()
            },
        );

        run_init(
            &root,
            &InitArgs {
                force: true,
                ..confirmed()
            },
        )
        .expect("forced init");

        let pipelines = pipelines_on_disk(&root);
        assert!(
            pipelines.contains("agent: codex") && !pipelines.contains("agent: claude"),
            "a forced init with no --provider must keep the project's provider: {pipelines}"
        );
        let config = std::fs::read_to_string(Config::path_in(&root)).unwrap();
        assert!(
            config.contains("blocked_agent = \"codex\""),
            "the configured blocked agent must stay codex: {config}"
        );
        assert!(
            config.contains("project_key = \"o/r\""),
            "a forced init with no --tracker must keep the project's tracker: {config}"
        );
    }

    /// `init --force` rewrites a `config.toml` that is a symlink through the
    /// link: the shared file it points at is rewritten and the link stays.
    #[cfg(unix)]
    #[test]
    fn a_forced_init_writes_through_a_symlinked_config() {
        let root = scaffold("force-symlinked-config", &confirmed());
        let shared = root.join("shared-dotfiles");
        std::fs::create_dir_all(&shared).unwrap();
        let target = shared.join("config.toml");
        std::fs::write(&target, "# stale\n").unwrap();
        let link = Config::path_in(&root);
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        run_init(
            &root,
            &InitArgs {
                force: true,
                ..confirmed()
            },
        )
        .expect("forced init");

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "config.toml is no longer a symlink"
        );
        let written = std::fs::read_to_string(&target).unwrap();
        assert!(!written.contains("# stale"), "{written}");
    }

    /// A person's private `overrides/config.toml` is theirs alone: a forced
    /// init keeps the provider the tracked config names, not the one the
    /// override layer merges over it, so the override is never published into
    /// the shared config or the pipelines.
    #[test]
    fn a_forced_init_does_not_publish_a_private_provider_override() {
        let root = scaffold(
            "force-ignores-override",
            &InitArgs {
                provider: Some(PlanningAgent::Codex),
                ..confirmed()
            },
        );
        let home = home_for(&root);
        crate::platform::test_home::with_home(&home, || {
            let overrides = crate::mux::project_home(&root)
                .unwrap()
                .join(crate::config::OVERRIDES_DIR);
            std::fs::create_dir_all(&overrides).unwrap();
            std::fs::write(
                crate::overrides::config_patch_path(&overrides),
                "[unattended]\nblocked_agent = \"claude\"\n",
            )
            .unwrap();
        });

        run_init(
            &root,
            &InitArgs {
                force: true,
                ..confirmed()
            },
        )
        .expect("forced init");

        let config = std::fs::read_to_string(Config::path_in(&root)).unwrap();
        assert!(config.contains("blocked_agent = \"codex\""), "{config}");
        assert!(pipelines_on_disk(&root).contains("agent: codex"));
    }

    /// With nobody to ask, `Install the example setup?` answers yes — what
    /// every `init` wrote before the question existed — and `--no-examples`
    /// answers no without asking.
    #[test]
    fn the_example_setup_is_yes_with_nobody_to_ask_and_no_when_declined() {
        let with = scaffold("examples-default", &confirmed());
        assert!(Pipelines::file_in(&with, "default").is_file());

        let without = scaffold(
            "examples-declined",
            &InitArgs {
                no_examples: true,
                ..confirmed()
            },
        );
        assert!(Config::path_in(&without).is_file());
        let pipelines = Pipelines::dir_in(&without);
        assert!(pipelines.is_dir());
        assert_eq!(std::fs::read_dir(&pipelines).unwrap().count(), 0);
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
            borrowed: false,
            checkout: root.to_path_buf(),
            config: Config::load(&root).unwrap(),
            home: root.join(".home"),
            root: root.to_path_buf(),
        };
        // `scan`, not `run`: `run` also prints the report, which this test is
        // not about — see `sync.rs`'s
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

    /// With nobody at a terminal and no `--yes`, `init` writes nothing at
    /// all: no `.spoolway/`, no skills, and no home claimed under `$HOME`.
    /// It does not hang either, which is the other half of the contract
    /// `init_with_nobody_to_ask_takes_every_default` proves for the
    /// questions this check sits in front of: a suite that gave `init` no
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
        assert!(
            !root.join(crate::config::STATE_DIR).exists(),
            "no .spoolway/ at all"
        );
        assert!(!root.join(".claude").exists(), "no skills were installed");
        assert!(
            !home.join(".spoolway").exists(),
            "no home was claimed under ~/.spoolway/"
        );
    }

    /// The same run with `--yes`: a run with no terminal may write, and the
    /// scaffold lands exactly where it always has. The pair is what makes
    /// the flag the thing that decides whether a silent run writes.
    #[test]
    fn init_with_yes_writes_the_scaffold_without_asking() {
        let root = scaffold("confirm-yes", &confirmed());
        assert!(Config::path_in(&root).exists());
        assert!(root.join(".claude").join("skills").is_dir());
    }

    /// A fresh git checkout named `name` under its own scratch folder, for
    /// the home-mode tests below, which share one scratch `$HOME` between
    /// two clones rather than taking [`home_for`]'s one per root.
    fn home_mode_checkout(parent: &Path, name: &str) -> std::path::PathBuf {
        let root = parent.join(name);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        root
    }

    /// Every path under `dir`, relative to it and sorted — how the home-mode
    /// tests tell that `.git` gained nothing from `init`.
    fn listing(dir: &Path) -> Vec<String> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                out.push(relative(base, &path));
                if path.is_dir() {
                    walk(base, &path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    /// `InitArgs` for the task's own scripted home-mode line:
    /// `--setup home --workspace <workspace> --provider claude --examples
    /// --tracker none --yes`.
    fn home_args(workspace: &str) -> InitArgs {
        InitArgs {
            yes: true,
            setup: Some(Setup::Home),
            workspace: Some(workspace.to_string()),
            provider: Some(PlanningAgent::Claude),
            examples: true,
            tracker: Some("none".to_string()),
            ..InitArgs::default()
        }
    }

    /// Acceptance: a home-mode `init` with `new` creates
    /// `~/.spoolway/<label>-<id>/` with `config/`, `dispatchers/<name>/` and
    /// a `project.toml` listing the clone; the example setup lands in
    /// `config/`; skills land in the user folder; and the checkout and its
    /// `.git` are exactly as they were.
    #[test]
    fn a_home_mode_init_creates_a_workspace_and_leaves_the_checkout_untouched() {
        let parent = crate::scratch::root("init-home-new");
        let home = parent.join("home");
        let root = home_mode_checkout(&parent, "api");
        let git_before = listing(&root.join(".git"));

        crate::platform::test_home::with_home(&home, || {
            init(&root, &home_args(NEW_WORKSPACE)).expect("init");

            let clone = crate::repo::workspace_clone(&root).expect("the clone is listed");
            let name = clone.workspace.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("api-"), "workspace {name} is <label>-<id>");
            assert_eq!(
                clone.workspace.parent(),
                Some(home.join(".spoolway").as_path())
            );
            assert_eq!(clone.dispatcher, "api");
            assert!(clone.home_dir().is_dir(), "dispatchers/api/ is made");
            assert!(clone.config_dir().join("config.toml").is_file());
            assert!(
                clone
                    .config_dir()
                    .join("pipelines")
                    .join("default.yml")
                    .is_file(),
                "the examples land in the workspace's config/"
            );
            assert!(
                !clone.config_dir().join("hooks").exists(),
                "no tracker, no hooks/"
            );
            assert!(
                crate::cli::Provider::Claude
                    .user_skills_dir(&home)
                    .join("spoolway-plan")
                    .join("SKILL.md")
                    .is_file(),
                "skills go to the user folder"
            );
        });

        let porcelain = crate::repo::run(&root, "git", &["status", "--porcelain"]).unwrap();
        assert_eq!(porcelain.trim(), "", "the checkout gained nothing");
        assert_eq!(
            listing(&root.join(".git")),
            git_before,
            ".git gained nothing from spoolway"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Acceptance: a bad `--tracker` value is still refused after every
    /// answer is gathered before any write — a home-mode run that fails
    /// here must not have listed the checkout in any workspace, exactly
    /// the task's own observed bug (a clone left joined to a workspace
    /// with no `config/`, because the old order joined first and only
    /// found the bad value while asking the tracker question afterwards).
    #[test]
    fn a_bad_tracker_value_in_home_mode_joins_no_workspace() {
        let parent = crate::scratch::root("init-home-bad-tracker");
        let home = parent.join("home");
        let root = home_mode_checkout(&parent, "api");
        crate::platform::test_home::with_home(&home, || {
            let args = InitArgs {
                tracker: Some("gitlab".to_string()),
                ..home_args(NEW_WORKSPACE)
            };
            let err = init(&root, &args).unwrap_err();
            assert!(
                err.to_string().contains("--tracker"),
                "the refusal names the bad flag: {err}"
            );
            assert!(
                crate::repo::workspace_clone(&root).is_none(),
                "a run that fails validation must not have joined any workspace"
            );
            assert!(
                !home.join(".spoolway").is_dir(),
                "nothing under ~/.spoolway/ either, not even a workspace with no config/"
            );
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Acceptance: a second clone joins with `--workspace <name>`, is added
    /// to that `project.toml` with a dispatcher folder of its own, and the
    /// shared `config/` is kept exactly as it was — including against the
    /// example and tracker flags, which a joining run does not apply.
    #[test]
    fn a_second_clone_joins_the_workspace_and_keeps_its_config() {
        let parent = crate::scratch::root("init-home-join");
        let home = parent.join("home");
        let first = home_mode_checkout(&parent, "api");
        let second = home_mode_checkout(&parent.join("elsewhere"), "api");

        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            let workspace = crate::repo::workspace_clone(&first).unwrap().workspace;
            let name = workspace
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let config = workspace.join("config").join("config.toml");
            std::fs::write(&config, "# edited by a person\n").unwrap();

            let joining = InitArgs {
                tracker: Some("github".to_string()),
                project_key: Some("o/r".to_string()),
                ..home_args(&name)
            };
            init(&second, &joining).expect("second init");

            let clone = crate::repo::workspace_clone(&second).expect("the second clone is listed");
            assert_eq!(clone.workspace, workspace);
            assert_eq!(clone.dispatcher, "api-2", "a taken dispatcher name gets -2");
            assert!(clone.home_dir().is_dir());
            assert_eq!(
                crate::repo::workspace_clone(&first).unwrap().dispatcher,
                "api",
                "the first clone keeps its own entry"
            );
            assert_eq!(
                std::fs::read_to_string(&config).unwrap(),
                "# edited by a person\n",
                "joining leaves the shared config alone"
            );
            assert!(!workspace.join("config").join("hooks").exists());
        });
        let porcelain = crate::repo::run(&second, "git", &["status", "--porcelain"]).unwrap();
        assert_eq!(porcelain.trim(), "");
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// `workspace-scan-strict` acceptance: a broken workspace file is
    /// refused by `init`, not silently read as "this checkout belongs to no
    /// workspace" — which is exactly what used to send `Placement::choose`
    /// down to `Placement::Repo`/`New` and convert a home-mode clone to repo
    /// mode, stamping `.git` and writing a tracked `.spoolway/` into a
    /// checkout that already had a home, just one `init` could not read
    /// about.
    #[test]
    fn init_refuses_rather_than_converting_a_clone_whose_workspace_file_is_broken() {
        let parent = crate::scratch::root("init-home-broken");
        let home = parent.join("home");
        let first = home_mode_checkout(&parent, "api");

        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            let workspace = crate::repo::workspace_clone(&first).unwrap().workspace;
            let record = workspace.join("project.toml");
            let mut broken = std::fs::read_to_string(&record).unwrap();
            broken.push_str("garbage = [\n");
            std::fs::write(&record, broken).unwrap();

            let err = init(&first, &home_args(NEW_WORKSPACE))
                .expect_err("a workspace file that fails to parse must refuse, not fall back")
                .to_string();
            assert!(
                err.contains(&record.display().to_string()),
                "error must name the broken file {}, got: {err}",
                record.display(),
            );
            assert!(
                !crate::config::tracked_setup_dir_in(&first).is_dir(),
                "must not fall back to writing a tracked .spoolway/ into this clone"
            );
        });
        assert!(
            std::fs::read_to_string(first.join(".git").join("spoolway-id")).is_err(),
            "must not stamp .git either"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A clone a readable workspace still lists, but whose shared `config/`
    /// was lost, must refuse naming the missing folder — not fall through
    /// to treating the clone as unconfigured and writing a fresh default
    /// `config.toml` into the very folder every other clone of that
    /// workspace shares.
    #[test]
    fn init_refuses_a_listed_clone_whose_workspace_has_no_config_rather_than_writing_a_fresh_one() {
        let parent = crate::scratch::root("init-listed-no-config");
        let home = parent.join("home");
        let first = home_mode_checkout(&parent, "api");

        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            let workspace = crate::repo::workspace_clone(&first).unwrap().workspace;
            let config_dir = workspace.join("config");
            std::fs::remove_dir_all(&config_dir).unwrap();

            let err = init(&first, &home_args(NEW_WORKSPACE))
                .expect_err("a listed clone with no shared config/ must refuse")
                .to_string();
            assert!(
                err.contains(&config_dir.display().to_string()),
                "error must name the missing folder {}, got: {err}",
                config_dir.display(),
            );
            assert!(
                !config_dir.is_dir(),
                "must not write a fresh config/ back into the workspace"
            );
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// With nobody to ask and no flag, the setup question answers `repo`:
    /// what `init` always did, so a script is unchanged by this question.
    #[test]
    fn the_setup_question_defaults_to_repo_with_nobody_to_ask() {
        let root = scaffold("setup-default", &confirmed());
        assert!(
            crate::config::tracked_setup_dir_in(&root)
                .join("config.toml")
                .is_file()
        );
        crate::platform::test_home::with_home(&home_for(&root), || {
            assert!(crate::repo::workspace_clone(&root).is_none());
        });
    }

    /// With nobody to ask and no `--workspace`, a home-mode run starts a
    /// new workspace rather than joining the one already there unasked.
    #[test]
    fn with_nobody_to_ask_a_home_mode_run_starts_its_own_workspace() {
        let parent = crate::scratch::root("init-home-unasked");
        let home = parent.join("home");
        let first = home_mode_checkout(&parent, "api");
        let second = home_mode_checkout(&parent, "api-review");
        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).unwrap();
            let unasked = InitArgs {
                workspace: None,
                ..home_args(NEW_WORKSPACE)
            };
            init(&second, &unasked).unwrap();
            assert_ne!(
                crate::repo::workspace_clone(&first).unwrap().workspace,
                crate::repo::workspace_clone(&second).unwrap().workspace
            );
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// With nobody to ask and no `--workspace`, a second clone of a
    /// repository a workspace already holds still starts its own new
    /// workspace — the same default the interactive menu takes on Enter, so
    /// criterion 1's "the interactive and non-interactive defaults agree"
    /// holds even in this case. (`Placement::choose_any` also prints a note
    /// naming the existing workspace and how to join it instead, satisfying
    /// criterion 3's "says it exists and how to join it" — not asserted
    /// here, since nothing in this crate's unit tests captures `init`'s own
    /// stdout; `tests/init_output.rs` is where printed output is checked,
    /// through a real subprocess.)
    #[test]
    fn non_interactive_home_mode_still_starts_a_new_workspace_for_the_same_repository() {
        let parent = crate::scratch::root("init-home-same-repo");
        let home = parent.join("home");
        let first = home_mode_checkout(&parent, "api");
        let second = home_mode_checkout(&parent, "api-review");
        let origin = "https://example.com/api.git";
        crate::repo::run(&first, "git", &["remote", "add", "origin", origin]).unwrap();
        crate::repo::run(&second, "git", &["remote", "add", "origin", origin]).unwrap();
        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).unwrap();
            let unasked = InitArgs {
                workspace: None,
                ..home_args(NEW_WORKSPACE)
            };
            init(&second, &unasked).unwrap();
            assert_ne!(
                crate::repo::workspace_clone(&first).unwrap().workspace,
                crate::repo::workspace_clone(&second).unwrap().workspace,
                "told about the existing workspace or not, a script that named none of its own \
                 still gets a fresh one rather than being joined to it unasked"
            );
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Moving a project between the two modes is not something `init` does,
    /// so a flag asking for it is refused before anything is written, and a
    /// `--workspace` naming nothing is refused — for a fresh checkout naming
    /// what is there, and for a listed one leaving it where it was.
    #[test]
    fn a_setup_flag_that_would_move_a_project_is_refused() {
        let parent = crate::scratch::root("init-home-refused");
        let home = parent.join("home");
        let repo_mode = home_mode_checkout(&parent, "tracked");
        let home_mode = home_mode_checkout(&parent, "api");
        crate::platform::test_home::with_home(&home, || {
            init(&repo_mode, &confirmed()).unwrap();
            let err = init(&repo_mode, &home_args(NEW_WORKSPACE)).unwrap_err();
            assert!(err.to_string().contains("tracked `.spoolway/`"), "{err}");
            assert!(
                err.to_string()
                    .contains("run `spoolway init` without --setup home"),
                "the refusal names what to run instead: {err}"
            );

            init(&home_mode, &home_args(NEW_WORKSPACE)).unwrap();
            let back = InitArgs {
                setup: Some(Setup::Repo),
                workspace: None,
                ..home_args(NEW_WORKSPACE)
            };
            let err = init(&home_mode, &back).unwrap_err();
            assert!(
                err.to_string().contains("already set up in home mode"),
                "{err}"
            );
            assert!(
                err.to_string()
                    .contains("run `spoolway init` without --setup"),
                "the refusal names what to run instead: {err}"
            );
            assert!(!crate::config::tracked_setup_dir_in(&home_mode).exists());

            let name = crate::repo::workspace_clone(&home_mode)
                .unwrap()
                .workspace
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let err = init(&home_mode, &home_args("elsewhere")).unwrap_err();
            assert!(
                err.to_string().contains("no workspace named elsewhere"),
                "a move to a workspace that does not exist is refused: {err}"
            );
            assert_eq!(
                crate::repo::workspace_clone(&home_mode)
                    .unwrap()
                    .workspace
                    .file_name()
                    .unwrap()
                    .to_string_lossy(),
                name,
                "the refused move left the checkout where it was"
            );

            let other = home_mode_checkout(&parent, "other");
            let err = init(&other, &home_args("nope")).unwrap_err();
            assert!(
                err.to_string().contains("no workspace by that name"),
                "{err}"
            );
            assert!(crate::repo::workspace_clone(&other).is_none());
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// [`home_mode_checkout`] with one commit, so it has a root commit —
    /// what tells a takeover or a move that a workspace holds the same
    /// repository. The message is the name, so two of these never share a
    /// root commit by accident of being made in the same second.
    fn committed_checkout(parent: &Path, name: &str) -> std::path::PathBuf {
        let root = home_mode_checkout(parent, name);
        crate::repo::run(&root, "git", &["commit", "--allow-empty", "-q", "-m", name]).unwrap();
        root
    }

    /// A `git clone` of `source` at `parent/name`: the same repository,
    /// so the same root commit.
    fn clone_of(source: &Path, parent: &Path, name: &str) -> std::path::PathBuf {
        let root = parent.join(name);
        std::fs::create_dir_all(parent).unwrap();
        crate::repo::run(
            parent,
            "git",
            &[
                "clone",
                "-q",
                source.to_str().unwrap(),
                root.to_str().unwrap(),
            ],
        )
        .unwrap();
        root
    }

    /// Every file under `dir` with its bytes, for checking a workspace's
    /// `config/` byte for byte.
    fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
        listing(dir)
            .into_iter()
            .filter(|rel| dir.join(rel).is_file())
            .map(|rel| {
                let bytes = std::fs::read(dir.join(&rel)).unwrap();
                (rel, bytes)
            })
            .collect()
    }

    /// The folder name of the workspace that lists `root`.
    fn workspace_name_of(root: &Path) -> String {
        crate::repo::workspace_clone(root)
            .expect("the checkout is listed")
            .workspace
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    /// A queued task `id` in `clone`'s queue, holding a worktree when
    /// `worktree` names one.
    fn queue_task(root: &Path, id: &str, worktree: Option<&str>) {
        let home = crate::repo::workspace_clone(root).unwrap().home_dir();
        let queue = home.join(crate::config::QUEUE_DIR);
        std::fs::create_dir_all(&queue).unwrap();
        let held = worktree
            .map(|path| format!("worktree_path: {path}\n"))
            .unwrap_or_default();
        std::fs::write(
            queue.join(format!("{id}.md")),
            format!("---\nid: {id}\ntitle: {id}\nstage: queued\n{held}---\n## Goal\n\nx\n"),
        )
        .unwrap();
    }

    /// Joining a workspace in which exactly one entry has this repository's
    /// root commit and a folder that is gone takes over that entry's queue:
    /// no new dispatcher folder, the gone checkout's task reachable from
    /// here, and the shared `config/` byte for byte the same.
    #[test]
    fn joining_takes_over_the_one_gone_checkout_of_this_repository() {
        let parent = crate::scratch::root("init-home-takeover");
        let home = parent.join("home");
        let first = committed_checkout(&parent, "api");
        let second = clone_of(&first, &parent.join("elsewhere"), "api2");

        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            let name = workspace_name_of(&first);
            queue_task(&first, "d1", None);
            let config = crate::repo::workspace_clone(&first).unwrap().config_dir();
            let before = snapshot(&config);
            std::fs::remove_dir_all(&first).unwrap();

            init(&second, &home_args(&name)).expect("join");

            let clone = crate::repo::workspace_clone(&second).expect("the clone is listed");
            assert_eq!(
                clone.dispatcher, "api",
                "the gone checkout's entry is taken over"
            );
            assert!(
                clone.home_dir().join("queue").join("d1.md").is_file(),
                "its queue comes along"
            );
            assert_eq!(
                std::fs::read_dir(clone.workspace.join("dispatchers"))
                    .unwrap()
                    .count(),
                1,
                "no second dispatcher folder is made"
            );
            assert_eq!(
                snapshot(&config),
                before,
                "config/ is byte for byte the same"
            );
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// With two gone entries of this repository there is no telling which
    /// one this checkout replaces, and with none there is nothing to take
    /// over: both join as a new checkout, and leave `config/` as it was. A
    /// checkout with no root commit never takes over.
    #[test]
    fn joining_takes_over_nothing_with_two_gone_entries_or_no_root_commit() {
        let parent = crate::scratch::root("init-home-no-takeover");
        let home = parent.join("home");
        let first = committed_checkout(&parent, "api");
        let gone_a = clone_of(&first, &parent.join("a"), "api");
        let gone_b = clone_of(&first, &parent.join("b"), "api");
        let joiner = clone_of(&first, &parent.join("c"), "api");
        let empty = home_mode_checkout(&parent.join("d"), "api");

        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            let name = workspace_name_of(&first);
            init(&gone_a, &home_args(&name)).expect("join a");
            init(&gone_b, &home_args(&name)).expect("join b");
            let config = crate::repo::workspace_clone(&first).unwrap().config_dir();
            let before = snapshot(&config);
            std::fs::remove_dir_all(&gone_a).unwrap();
            std::fs::remove_dir_all(&gone_b).unwrap();

            init(&joiner, &home_args(&name)).expect("join c");
            assert_eq!(
                crate::repo::workspace_clone(&joiner).unwrap().dispatcher,
                "api-4",
                "two gone entries: a new checkout, not a guess"
            );
            assert_eq!(snapshot(&config), before);

            // The gone checkout's root commit is recorded, but this one has
            // none to match it with.
            std::fs::remove_dir_all(&joiner).unwrap();
            std::fs::remove_dir_all(&first).unwrap();
            init(&empty, &home_args(&name)).expect("join d");
            assert_eq!(
                crate::repo::workspace_clone(&empty).unwrap().dispatcher,
                "api-5",
                "no root commit: never a takeover"
            );
            assert_eq!(snapshot(&config), before);
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A join checks `--tracker` even though it does not apply it, before it
    /// writes anything.
    #[test]
    fn a_bad_tracker_value_refuses_a_join_before_it_lists_the_checkout() {
        let parent = crate::scratch::root("init-home-join-bad-tracker");
        let home = parent.join("home");
        let first = home_mode_checkout(&parent, "api");
        let second = home_mode_checkout(&parent.join("b"), "api");
        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            let name = workspace_name_of(&first);
            let args = InitArgs {
                tracker: Some("gitlab".to_string()),
                ..home_args(&name)
            };
            let err = init(&second, &args).unwrap_err();
            assert!(err.to_string().contains("--tracker"), "{err}");
            assert!(crate::repo::workspace_clone(&second).is_none());
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Picking another workspace moves the checkout — but not while any of
    /// its tasks holds a worktree, and then with nothing written and every
    /// such task named.
    #[test]
    fn a_move_is_refused_while_tasks_hold_worktrees_and_writes_nothing() {
        let parent = crate::scratch::root("init-home-move-refused");
        let home = parent.join("home");
        let first = committed_checkout(&parent, "api");
        let second = clone_of(&first, &parent.join("b"), "api");
        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            init(&second, &home_args(NEW_WORKSPACE)).expect("second init");
            let target = workspace_name_of(&second);
            queue_task(&first, "c1", Some("/somewhere/task-c1"));
            queue_task(&first, "c2", Some("/somewhere/task-c2"));
            queue_task(&first, "c3", None);
            let state = home.join(".spoolway");
            let before = snapshot(&state);

            let err = init(&first, &home_args(&target)).unwrap_err().to_string();
            assert!(
                err.contains("c1 and c2 hold worktrees in this checkout"),
                "every holding task is named, and only those: {err}"
            );
            assert!(err.contains("Finish or unqueue them"), "{err}");
            assert_eq!(snapshot(&state), before, "nothing is written");
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Joining is held to the same rule as moving: an unlisted checkout may not
    /// join a workspace of another repository.
    #[test]
    fn joining_a_workspace_of_another_repository_is_refused() {
        let parent = crate::scratch::root("init-home-join-other-repo");
        let home = parent.join("home");
        let other = committed_checkout(&parent, "web");
        let stranger = committed_checkout(&parent, "api");
        crate::platform::test_home::with_home(&home, || {
            init(&other, &home_args(NEW_WORKSPACE)).expect("other init");
            let target = workspace_name_of(&other);
            let state = home.join(".spoolway");
            let before = snapshot(&state);

            let err = init(&stranger, &home_args(&target))
                .unwrap_err()
                .to_string();
            assert!(err.contains("holds another repository"), "{err}");
            assert_eq!(snapshot(&state), before, "nothing is written");
            assert!(crate::repo::workspace_clone(&stranger).is_none());
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A workspace whose last checkout left is kept with its setup, and the
    /// next checkout of the same repository may join it again.
    #[test]
    fn a_kept_empty_workspace_takes_a_checkout_of_its_repository_back() {
        let parent = crate::scratch::root("init-home-rejoin-kept");
        let home = parent.join("home");
        let first = committed_checkout(&parent, "api");
        let second = clone_of(&first, &parent.join("b"), "api");
        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            let kept = workspace_name_of(&first);
            init(&first, &home_args(NEW_WORKSPACE)).expect("move out, emptying the first");
            assert_ne!(workspace_name_of(&first), kept);

            init(&second, &home_args(&kept)).expect("a checkout of the same repository joins");
            assert_eq!(workspace_name_of(&second), kept);
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A checkout with no commit has no root commit to compare, so it is not
    /// guessed to belong to another repository.
    #[test]
    fn a_checkout_with_no_commit_may_join_a_workspace_of_a_repository() {
        let parent = crate::scratch::root("init-home-join-no-commit");
        let home = parent.join("home");
        let first = committed_checkout(&parent, "api");
        let fresh = home_mode_checkout(&parent.join("d"), "api");
        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            let target = workspace_name_of(&first);
            init(&fresh, &home_args(&target)).expect("a checkout with no commit joins");
            assert_eq!(workspace_name_of(&fresh), target);
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A workspace file with a bad `dispatcher` would refuse every command in
    /// a checkout listed there, so neither a join nor a move writes into it.
    #[test]
    fn nothing_joins_or_moves_into_a_workspace_with_a_bad_dispatcher() {
        let parent = crate::scratch::root("init-home-bad-dispatcher-target");
        let home = parent.join("home");
        let first = committed_checkout(&parent, "api");
        let joiner = clone_of(&first, &parent.join("b"), "api");
        let mover = clone_of(&first, &parent.join("c"), "api");
        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            init(&mover, &home_args(NEW_WORKSPACE)).expect("mover init");
            let target = workspace_name_of(&first);
            let record = home.join(".spoolway").join(&target).join("project.toml");
            let mut raw = std::fs::read_to_string(&record).unwrap();
            raw.push_str(
                "\n[[clones]]\nroot = \"/nonexistent/gone\"\ndispatcher = \"../../evil\"\n",
            );
            std::fs::write(&record, raw).unwrap();
            let state = home.join(".spoolway");
            let before = snapshot(&state);

            for checkout in [&joiner, &mover] {
                let err = init(checkout, &home_args(&target)).unwrap_err().to_string();
                assert!(
                    err.contains("../../evil") && err.contains("fix it by hand"),
                    "{err}"
                );
                assert_eq!(snapshot(&state), before, "nothing is written");
            }
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A move renames the checkout's dispatcher folder, so the usage
    /// registry must not keep the old home beside the new one.
    #[test]
    fn a_move_leaves_one_registry_entry_for_the_checkout() {
        let parent = crate::scratch::root("init-home-move-registry");
        let home = parent.join("home");
        let first = committed_checkout(&parent, "api");
        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            crate::usage::registry::register(&first);
            init(&first, &home_args(NEW_WORKSPACE)).expect("move");
            crate::usage::registry::register(&first);
            let raw = std::fs::read_to_string(crate::usage::registry::path().unwrap())
                .unwrap_or_default();
            let root = first.display().to_string();
            assert_eq!(raw.matches(&root).count(), 1, "one entry per root: {raw}");
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A move never crosses repositories, and there is no flag to force it.
    #[test]
    fn a_move_into_a_workspace_of_another_repository_is_refused() {
        let parent = crate::scratch::root("init-home-move-other-repo");
        let home = parent.join("home");
        let first = committed_checkout(&parent, "api");
        let other = committed_checkout(&parent, "web");
        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            init(&other, &home_args(NEW_WORKSPACE)).expect("other init");
            let from = workspace_name_of(&first);
            let target = workspace_name_of(&other);
            let state = home.join(".spoolway");
            let before = snapshot(&state);

            let err = init(&first, &home_args(&target)).unwrap_err().to_string();
            assert!(err.contains("holds another repository"), "{err}");
            assert_eq!(snapshot(&state), before, "nothing is written");
            assert_eq!(workspace_name_of(&first), from);
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A move carries the checkout's queue into the workspace it picked,
    /// leaves that workspace's `config/` byte for byte the same, and keeps the
    /// workspace it emptied.
    #[test]
    fn a_move_carries_the_queue_and_keeps_the_workspace_it_emptied() {
        let parent = crate::scratch::root("init-home-move-done");
        let home = parent.join("home");
        let first = committed_checkout(&parent, "api");
        let second = clone_of(&first, &parent.join("b"), "api");
        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            init(&second, &home_args(NEW_WORKSPACE)).expect("second init");
            let from = crate::repo::workspace_clone(&first).unwrap().workspace;
            let target = workspace_name_of(&second);
            let config = crate::repo::workspace_clone(&second).unwrap().config_dir();
            let before = snapshot(&config);
            queue_task(&first, "c3", None);

            init(&first, &home_args(&target)).expect("move");

            let clone = crate::repo::workspace_clone(&first).unwrap();
            assert_eq!(workspace_name_of(&first), target);
            assert_eq!(
                clone.dispatcher, "api-2",
                "a taken name draws the next free one"
            );
            assert!(clone.home_dir().join("queue").join("c3.md").is_file());
            assert_eq!(
                snapshot(&config),
                before,
                "config/ is byte for byte the same"
            );
            assert!(from.is_dir(), "the emptied workspace is kept");
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// `--workspace new` from a listed checkout moves it into a new workspace
    /// set up from scratch, and the one it left keeps every other checkout.
    #[test]
    fn a_listed_checkout_can_move_into_a_new_workspace() {
        let parent = crate::scratch::root("init-home-move-new");
        let home = parent.join("home");
        let first = committed_checkout(&parent, "api");
        let second = clone_of(&first, &parent.join("b"), "api");
        crate::platform::test_home::with_home(&home, || {
            init(&first, &home_args(NEW_WORKSPACE)).expect("first init");
            let from = workspace_name_of(&first);
            init(&second, &home_args(&from)).expect("join");

            init(&second, &home_args(NEW_WORKSPACE)).expect("move to a new workspace");

            let clone = crate::repo::workspace_clone(&second).unwrap();
            assert_ne!(workspace_name_of(&second), from);
            assert!(clone.config_dir().join("config.toml").is_file());
            assert_eq!(workspace_name_of(&first), from, "the first checkout stays");
            assert!(
                home.join(".spoolway").join(&from).is_dir(),
                "a workspace still holding a checkout is kept"
            );
        });
        let _ = std::fs::remove_dir_all(&parent);
    }
}
