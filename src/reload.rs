//! Asking a running dispatcher to load the pipelines and config again, and
//! reading what it answered.
//!
//! The dispatcher loads its pipelines once, at start, and every lane's
//! `spoolway report` routes on the copy it wrote down — see
//! [`crate::pipeline_snapshot`]. An edit to a pipeline file therefore waits
//! for a restart, which stops the run. A reload is the same load done between
//! two passes, with no lane stopped.
//!
//! It happens only when a person asks. A pipeline edit spans several files,
//! and one that is half saved but still parses would go live in the middle of
//! a run if the dispatcher took files up on its own.
//!
//! The request and its answer are files in the project home, so a dispatcher
//! started in a terminal is reached the same way as one the screen started.
//! [`request`] writes one file under `reload/requests/`, named by the id it
//! returns. The directory is watched, which wakes the dispatcher's wait
//! between passes. At the top of its next pass the dispatcher calls [`serve`],
//! which answers every request it finds under `reload/requests/` by writing
//! one file under `reload/answers/`, and [`answer`] reads that answer back by
//! id.
//!
//! A reload also sets the run's mode to `unattended.enabled`, whatever flag the
//! run started with, because the mode is the one setting the lock records. See
//! [`crate::lock::Lock::switch_mode`].
//!
//! A reload is refused, and changes nothing, when the new pipelines do not
//! load, or when a task in the queue is recorded on a step, or names a
//! pipeline, the new pipelines lack. A running lane whose step is gone is no
//! longer recognised by the dispatcher, which then drops its spend record, and
//! a task held on a gone step can only be moved by hand with `spoolway resume
//! --stage`. So the one edit that strands work is the one this refuses.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::pipeline::Pipelines;
use crate::repo::Repo;

/// How long an answer stays in the project home. Whoever asked reads it within
/// a few passes, so an hour is far longer than needed, and short enough that
/// answers nobody collected do not pile up.
const ANSWER_KEPT: Duration = Duration::from_secs(60 * 60);

/// What the dispatcher did with a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// The id [`request`] returned for the request this answers. An asker
    /// reads only the answer to its own request, so a second person's reload
    /// is never shown as the outcome of the first one's.
    pub request: String,
    pub outcome: Outcome,
}

/// Whether the reload happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// The new pipelines and config are in use.
    Loaded,
    /// Nothing changed. One entry for each thing in the way.
    Refused(Vec<Reason>),
}

/// One thing in the way of a reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    /// The fact, as a sentence that starts with the task it is about when it
    /// is about one. May run over several lines when it quotes a parse error.
    pub what: String,
    /// What to do about it, ending in the reload to try again.
    pub fix: String,
}

impl Answer {
    /// The answer as a person reads it, in the words the refusal is drawn in.
    pub fn render(&self) -> String {
        match &self.outcome {
            Outcome::Loaded => "successfully loaded".to_string(),
            Outcome::Refused(reasons) => {
                let mut text = String::from(
                    "reload refused — nothing changed, the run is still on what it had:",
                );
                for reason in reasons {
                    text.push_str(&format!("\n  {}\n  {}", reason.what, reason.fix));
                }
                text
            }
        }
    }

    /// A refusal on one line, for the problem log, which keeps one line per
    /// problem. Every run of whitespace, a parse error's line breaks included,
    /// becomes one space: a continuation line the log cannot read a timestamp
    /// from is never trimmed. `None` for a good reload, which is no problem.
    pub fn problem_line(&self) -> Option<String> {
        let Outcome::Refused(reasons) = &self.outcome else {
            return None;
        };
        let reasons: Vec<String> = reasons
            .iter()
            .map(|reason| format!("{} {}", reason.what, reason.fix))
            .collect();
        let line = format!("reload refused, nothing changed: {}", reasons.join(" | "));
        Some(line.split_whitespace().collect::<Vec<_>>().join(" "))
    }
}

/// Where requests wait for the dispatcher. Watched, so a new file wakes the
/// dispatcher's wait between passes.
pub(crate) fn requests_dir(repo: &Repo) -> PathBuf {
    let dir = repo.home().join("reload").join("requests");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Where the dispatcher leaves its answers. Not the watched directory, so
/// writing an answer does not wake the dispatcher that wrote it.
fn answers_dir(repo: &Repo) -> PathBuf {
    let dir = repo.home().join("reload").join("answers");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Ask the dispatcher running in `repo`'s project to reload, and return the id
/// its answer will name.
///
/// Writes the request whether or not a dispatcher is running; a caller that
/// must tell the two apart looks at the lock first. A request left with nobody
/// to read it is answered as loaded by the next dispatcher at its start, since
/// a start loads every file anyway.
///
/// The request is its file's name and nothing in the file is read, so the
/// first event the watch sees, which comes while the file is still empty,
/// already finds the whole request.
pub fn request(repo: &Repo) -> Result<String> {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let id = format!("{}-{nanos}", std::process::id());
    let path = requests_dir(repo).join(&id);
    std::fs::write(&path, &id).with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

/// The dispatcher's answer to request `id`, or `None` while it has not
/// answered yet.
pub fn answer(repo: &Repo, id: &str) -> Result<Option<Answer>> {
    // The id becomes part of a path below, so only what `request` makes is
    // looked up.
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        return Ok(None);
    }
    let path = answers_dir(repo).join(format!("{id}.json"));
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&raw)
        .map(Some)
        .with_context(|| format!("reading {}", path.display()))
}

/// The ids of the requests waiting, sorted by name. The name starts with the
/// asker's pid, so the order says nothing about which request came first.
fn pending(repo: &Repo) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(requests_dir(repo))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    ids.sort();
    ids
}

/// Answer every id in `ids` with `outcome`, then clear the requests that were
/// answered, and return a sentence for each answer that could not be written.
///
/// A request is cleared only once its answer exists. One cleared without it
/// would leave the asker waiting for nothing, so a request whose answer failed
/// stays, and the next pass tries it again.
fn settle(repo: &Repo, ids: &[String], outcome: &Outcome) -> Vec<String> {
    let answers = answers_dir(repo);
    prune_answers(&answers);
    let mut problems = Vec::new();
    for id in ids {
        let answer = Answer {
            request: id.clone(),
            outcome: outcome.clone(),
        };
        let path = answers.join(format!("{id}.json"));
        let written = serde_json::to_string(&answer)
            .map_err(anyhow::Error::from)
            .and_then(|text| crate::task::write_atomic(&path, text));
        match written {
            Ok(()) => {
                let _ = std::fs::remove_file(requests_dir(repo).join(id));
            }
            Err(err) => problems.push(format!(
                "the answer to reload request {id} could not be written: {err:#}. The request \
                 stays, and the next pass answers it again"
            )),
        }
    }
    problems
}

fn prune_answers(dir: &std::path::Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age > ANSWER_KEPT);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Answer, as loaded, every request a dead dispatcher left behind, and return
/// a sentence for each answer that could not be written.
///
/// Called by a dispatcher right after it takes the lock. It has just loaded
/// every file, so there is nothing more for those requests to ask for, and
/// serving them as well would load twice.
pub(crate) fn settle_left_behind(repo: &Repo) -> Vec<String> {
    let ids = pending(repo);
    if ids.is_empty() {
        return Vec::new();
    }
    settle(repo, &ids, &Outcome::Loaded)
}

/// What [`serve`] did.
pub(crate) struct Served {
    /// The answer, as the first request waiting gets it. Every request waiting
    /// gets the same one, differing only in the request it names.
    pub answer: Answer,
    /// Each answer that could not be written, as a sentence.
    pub problems: Vec<String>,
}

/// Serve the requests waiting, at the top of a pass.
///
/// Returns `None` when there were none. Otherwise it loads the pipelines from
/// `live`'s root with the config as it now stands, checks them, and on a good
/// load switches the run's mode to `unattended.enabled`, rewrites the snapshot
/// and then replaces `pipelines` and `live.config`. The snapshot is written
/// before the swap and before anything else runs, so no lane launches on a
/// graph its own `report` would not route on.
///
/// All of it is built and checked before anything is touched. A refusal, or a
/// snapshot that cannot be written, leaves `pipelines`, `live.config`, the lock
/// and the snapshot as they were. See [`apply`] for the one way it cannot.
pub(crate) fn serve(live: &mut Repo, pipelines: &mut Pipelines) -> Option<Served> {
    let ids = pending(live);
    let first = ids.first()?.clone();
    let mut problems = Vec::new();
    let outcome = match try_reload(live) {
        Ok((config, loaded)) => apply(live, pipelines, config, loaded, &mut problems),
        Err(reasons) => Outcome::Refused(reasons),
    };
    problems.extend(settle(live, &ids, &outcome));
    Some(Served {
        answer: Answer {
            request: first,
            outcome,
        },
        problems,
    })
}

/// Put a checked reload in force: the run's mode, then the snapshot, then the
/// swap, then the record of the last good config.
///
/// The mode comes first because the snapshot and the record both hold the
/// lock's text, and either written under the old mode line would no longer
/// match the lock, so every reader would fall back to the files. The snapshot
/// is done before the caller builds the pass, so no lane launches on a lock and
/// snapshot that disagree. A snapshot that cannot be written puts the lock
/// back, so a refusal still changes nothing; if the lock cannot be put back
/// either, the refusal says which mode the run is now in.
///
/// The record comes last and cannot refuse. It only serves a command that runs
/// while `config.toml` is broken, and the reload is already in force by then,
/// so a failure to write it is a problem for the log instead.
fn apply(
    live: &mut Repo,
    pipelines: &mut Pipelines,
    config: crate::config::Config,
    loaded: Pipelines,
    problems: &mut Vec<String>,
) -> Outcome {
    let enabled = config.unattended.enabled;
    let switch = match crate::lock::Lock::switch_mode(&live.lock_file(), enabled) {
        Ok(switch) => switch,
        Err(err) => {
            return Outcome::Refused(vec![Reason {
                what: format!("the run's mode could not be changed: {err:#}"),
                fix: "Make the project home writable, then reload again.".to_string(),
            }]);
        }
    };
    if let Err(err) = crate::pipeline_snapshot::write(live, &loaded) {
        let mut reasons = vec![Reason {
            what: format!("the new pipelines could not be recorded: {err:#}"),
            fix: "Make the project home writable, then reload again.".to_string(),
        }];
        if let Some(Err(undo)) = switch.map(crate::lock::ModeSwitch::undo) {
            let mode = if enabled { "unattended" } else { "attended" };
            reasons.push(Reason {
                what: format!(
                    "the lock could not be put back: {undo:#}. The run is now {mode}, and its \
                     snapshot no longer matches the lock"
                ),
                fix: "Make the project home writable, then reload again.".to_string(),
            });
        }
        return Outcome::Refused(reasons);
    }
    live.config = config;
    *pipelines = loaded;
    if let Err(err) = crate::pipeline_snapshot::record_config(live, &live.config) {
        problems.push(format!(
            "the last good config could not be recorded under the run's new mode: {err:#}. A \
             lane that reports while config.toml is broken fails until it is; reload again once \
             the project home is writable"
        ));
    }
    Outcome::Loaded
}

/// The config and pipelines as the files now stand, when nothing in the queue
/// would be stranded by them.
fn try_reload(live: &Repo) -> std::result::Result<(crate::config::Config, Pipelines), Vec<Reason>> {
    let config = crate::config::Config::load_present(&live.root).map_err(|err| {
        vec![Reason {
            what: format!("config.toml does not load: {err:#}"),
            fix: "Fix config.toml, then reload again.".to_string(),
        }]
    })?;
    // The state a dispatch start refuses, refused here too: a reload that
    // turned unattended on over a blank model would leave the next block on a
    // staffed `blocked` step that no lane can launch on, with nobody there.
    if config.unattended.enabled && config.unattended.blocked_model.trim().is_empty() {
        return Err(vec![Reason {
            what: "unattended.enabled is on and unattended.blocked_model is blank — an \
                   unattended run has nobody to clear a block, so `blocked` has to be staffed"
                .to_string(),
            fix: "Set unattended.blocked_model, or turn unattended.enabled off, then reload \
                  again."
                .to_string(),
        }]);
    }
    let loaded = Pipelines::load(&live.root, &config).map_err(|err| {
        vec![Reason {
            what: format!("the pipelines do not load: {err:#}"),
            fix: "Fix the file it names, then reload again.".to_string(),
        }]
    })?;
    let (tasks, unreadable) = live.tasks_and_problems().map_err(|err| {
        vec![Reason {
            what: format!("the queue cannot be read: {err:#}"),
            fix: "Make the queue readable, then reload again.".to_string(),
        }]
    })?;
    // A task file that does not parse has no step to read, so the check below
    // cannot say whether the edit strands it. Refused rather than guessed at.
    let mut refusals: Vec<Reason> = unreadable
        .iter()
        .map(|file| Reason {
            what: format!(
                "{}, so a reload cannot tell whether the edit strands it",
                file.skipped_line()
            ),
            fix: "Fix or remove that queue file, then reload again.".to_string(),
        })
        .collect();
    refusals.extend(stranded(&loaded, &tasks));
    if refusals.is_empty() {
        Ok((config, loaded))
    } else {
        Err(refusals)
    }
}

/// Every task, running or waiting, that the new pipelines would leave without
/// a step it is recorded on or the pipeline it names.
///
/// A task is recorded on its `stage:` and, while held, on the step it was held
/// at — see [`crate::commands::missing_recorded_steps`], which `pipeline check`
/// reads as well. A task on a name two files claim is held by the dispatcher in
/// any case, and a task naming no pipeline is refused at the start of a run, so
/// neither says anything about this reload.
fn stranded(loaded: &Pipelines, tasks: &[crate::task::Task]) -> Vec<Reason> {
    let mut reasons = Vec::new();
    for task in tasks {
        let Some(name) = task.front.pipeline.as_deref().map(str::trim) else {
            continue;
        };
        if name.is_empty() || loaded.clashes.contains_key(name) {
            continue;
        }
        let id = task.id();
        let Some(pipeline) = loaded.pipelines.get(name) else {
            reasons.push(Reason {
                what: format!("{id}: names pipeline `{name}`, which is no longer defined"),
                fix: format!(
                    "Put `{name}.yml` back, or wait until {id} is done, then reload again."
                ),
            });
            continue;
        };
        for (field, step) in crate::commands::missing_recorded_steps(task, pipeline) {
            let held = match field {
                "stage" => format!("sitting on `{step}`"),
                _ => format!("held at `{step}` (`{field}`)"),
            };
            reasons.push(Reason {
                what: format!("{id}: {held}, which pipeline `{name}` no longer defines"),
                fix: format!(
                    "Put `{step}` back in {name}.yml, or wait until {id} has left it, then \
                     reload again."
                ),
            });
        }
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::fixture;
    use crate::task::Task;

    fn pipelines(yaml: &str) -> Pipelines {
        let pipeline: crate::pipeline::Pipeline = serde_norway::from_str(yaml).unwrap();
        let mut named = std::collections::BTreeMap::new();
        named.insert("feature".to_string(), pipeline);
        Pipelines {
            pipelines: named,
            ignored_overrides: Vec::new(),
            clashes: Default::default(),
        }
    }

    fn task(id: &str, pipeline: &str, stage: &str) -> Task {
        Task::parse(
            PathBuf::from(format!("{id}.md")),
            &format!("---\nid: {id}\npipeline: {pipeline}\nstage: {stage}\n---\nbody\n"),
        )
        .unwrap()
    }

    /// A step the new graph lacks strands the task sitting on it, and the
    /// refusal reads as the plan draws it. Tasks on a step that stays, on a
    /// reserved stage, or on no pipeline at all say nothing.
    #[test]
    fn a_deleted_step_strands_only_the_task_sitting_on_it() {
        let loaded = pipelines("steps:\n  - id: build\n    run: x\n    on_pass: done\n");
        let tasks = [
            task("fix-login", "feature", "review"),
            task("kept", "feature", "build"),
            task("fresh", "feature", "queued"),
            task("parked", "feature", "paused"),
        ];
        let reasons = stranded(&loaded, &tasks);
        assert_eq!(
            reasons,
            [Reason {
                what: "fix-login: sitting on `review`, which pipeline `feature` no longer defines"
                    .to_string(),
                fix: "Put `review` back in feature.yml, or wait until fix-login has left it, \
                      then reload again."
                    .to_string(),
            }]
        );
    }

    /// A task naming a pipeline the new graph lacks is stranded whatever its
    /// stage, including a reserved one: it cannot be routed anywhere.
    #[test]
    fn a_deleted_pipeline_strands_every_task_naming_it() {
        let loaded = pipelines("steps:\n  - id: build\n    run: x\n    on_pass: done\n");
        let tasks = [
            task("old", "retired", "queued"),
            task("on", "feature", "build"),
        ];
        let reasons = stranded(&loaded, &tasks);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert_eq!(
            reasons[0].what,
            "old: names pipeline `retired`, which is no longer defined"
        );
        assert_eq!(
            reasons[0].fix,
            "Put `retired.yml` back, or wait until old is done, then reload again."
        );
    }

    /// An answer names its request, and reads as nothing until one exists.
    #[test]
    fn an_answer_names_the_request_it_answers() {
        let (repo, _root) = fixture("reload-answer");
        assert_eq!(answer(&repo, "1-1").unwrap(), None);
        let first = request(&repo).unwrap();
        let second = request(&repo).unwrap();
        assert_ne!(first, second);
        assert_eq!(pending(&repo), [first.clone(), second.clone()]);
        assert_eq!(answer(&repo, &first).unwrap(), None);

        settle(&repo, std::slice::from_ref(&first), &Outcome::Loaded);
        let got = answer(&repo, &first).unwrap().unwrap();
        assert_eq!(got.request, first);
        assert_eq!(got.outcome, Outcome::Loaded);
        assert_eq!(answer(&repo, &second).unwrap(), None);
        assert_eq!(pending(&repo), [second]);
    }

    /// A request found at a start is answered as loaded and cleared, so one
    /// left by a crashed dispatcher cannot load again on the next pass.
    #[test]
    fn a_request_left_behind_is_answered_as_loaded_at_start() {
        let (repo, _root) = fixture("reload-left-behind");
        let id = request(&repo).unwrap();
        settle_left_behind(&repo);
        assert!(pending(&repo).is_empty());
        assert_eq!(
            answer(&repo, &id).unwrap().unwrap().outcome,
            Outcome::Loaded
        );
    }

    /// The id becomes part of a path, so one that `request` could not have
    /// made is never looked up.
    #[test]
    fn an_answer_is_not_looked_up_by_a_path() {
        let (repo, _root) = fixture("reload-answer-path");
        assert_eq!(answer(&repo, "../config").unwrap(), None);
    }

    /// A project whose config and `feature` pipeline load, with a lock held
    /// so a snapshot can be written, and one task on `stage`.
    fn served_project(
        name: &str,
        stage: &str,
    ) -> (
        Repo,
        Pipelines,
        crate::scratch::ScratchRoot,
        crate::lock::Lock,
    ) {
        let (repo, root) = fixture(name);
        crate::config::Config::default().save(&repo.root).unwrap();
        let file = Pipelines::file_in(&repo.root, "feature");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let yaml = "steps:\n  - id: build\n    run: x\n    on_pass: review\n  - id: review\n    \
                    run: y\n    on_pass: done\n";
        std::fs::write(&file, yaml).unwrap();
        let held = Pipelines::load(&repo.root, &repo.config).unwrap();
        let text = format!("---\nid: fix-login\npipeline: feature\nstage: {stage}\n---\nbody\n");
        std::fs::write(repo.queue_dir().join("fix-login.md"), text).unwrap();
        let lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        (repo, held, root, lock)
    }

    /// A refusal touches nothing: not the pipelines the run holds, not the
    /// config, not the snapshot, even when the config on disk has moved on.
    #[test]
    fn a_refusal_leaves_the_pipelines_the_config_and_the_snapshot_as_they_were() {
        let (mut repo, mut held, _root, _lock) = served_project("reload-refusal", "review");
        let mut edited = repo.config.clone();
        edited.agents.get_mut("claude").unwrap().concurrency += 1;
        edited.save(&repo.root).unwrap();
        std::fs::write(
            Pipelines::file_in(&repo.root, "feature"),
            "steps:\n  - id: build\n    run: x\n    on_pass: done\n",
        )
        .unwrap();
        let before = held.names().join(",");
        let cap_before = repo.config.agents["claude"].concurrency;
        let id = request(&repo).unwrap();

        let served = serve(&mut repo, &mut held).unwrap();

        assert!(matches!(served.answer.outcome, Outcome::Refused(_)));
        assert!(served.problems.is_empty(), "{:?}", served.problems);
        assert!(held.pipelines["feature"].step("review").is_some());
        assert_eq!(held.names().join(","), before);
        assert_eq!(repo.config.agents["claude"].concurrency, cap_before);
        assert!(!crate::pipeline_snapshot::path(&repo).exists());
        assert_eq!(
            answer(&repo, &id).unwrap().unwrap().outcome,
            served.answer.outcome
        );
    }

    /// A config.toml that no longer loads refuses the reload and says so.
    #[test]
    fn a_config_that_does_not_load_refuses_the_reload() {
        let (mut repo, mut held, _root, _lock) = served_project("reload-bad-config", "build");
        std::fs::write(
            crate::config::Config::path_in(&repo.root),
            "this is = = not toml\n",
        )
        .unwrap();
        request(&repo).unwrap();

        let served = serve(&mut repo, &mut held).unwrap();

        let Outcome::Refused(reasons) = served.answer.outcome else {
            panic!("reload was not refused");
        };
        let line = Answer {
            request: String::new(),
            outcome: Outcome::Refused(reasons.clone()),
        }
        .problem_line()
        .unwrap();
        assert!(
            reasons[0].what.contains('\n') && !line.contains('\n'),
            "{line}"
        );
        assert!(
            reasons[0].what.starts_with("config.toml does not load"),
            "{reasons:?}"
        );
        assert!(!crate::pipeline_snapshot::path(&repo).exists());
    }

    /// A good reload swaps the pipelines in and writes the snapshot, and the
    /// snapshot holds what was swapped in.
    #[test]
    fn a_good_reload_swaps_the_pipelines_and_writes_the_snapshot() {
        let (mut repo, mut held, _root, _lock) = served_project("reload-good", "build");
        std::fs::write(
            Pipelines::file_in(&repo.root, "feature"),
            "steps:\n  - id: build\n    run: x\n    on_pass: ship\n  - id: ship\n    run: y\n    \
             on_pass: done\n",
        )
        .unwrap();
        request(&repo).unwrap();

        let served = serve(&mut repo, &mut held).unwrap();

        assert_eq!(served.answer.outcome, Outcome::Loaded);
        assert!(held.pipelines["feature"].step("ship").is_some());
        let snapshot = crate::pipeline_snapshot::LoadedPipelines::live(&repo).unwrap();
        assert!(
            snapshot.pipelines(&["feature"]).unwrap().pipelines["feature"]
                .step("ship")
                .is_some()
        );
        assert!(pending(&repo).is_empty());
    }

    /// A reload that changes `unattended.enabled` rewrites the lock's mode line
    /// and the snapshot together, so the snapshot still matches the lock. One
    /// that leaves the mode alone does not touch the lock, whose modified time
    /// starts the spend window.
    #[test]
    fn a_reload_follows_the_configs_mode_and_leaves_the_lock_alone_otherwise() {
        let (mut repo, mut held, _root, _lock) = served_project("reload-mode", "build");
        let lock_file = repo.lock_file();
        let stamp = std::fs::metadata(&lock_file).unwrap().modified().unwrap();
        request(&repo).unwrap();
        serve(&mut repo, &mut held).unwrap();
        assert_eq!(
            std::fs::metadata(&lock_file).unwrap().modified().unwrap(),
            stamp
        );
        assert_eq!(crate::lock::Lock::unattended(&lock_file), Some(false));

        let mut edited = repo.config.clone();
        edited.unattended.enabled = true;
        edited.save(&repo.root).unwrap();
        request(&repo).unwrap();
        let served = serve(&mut repo, &mut held).unwrap();

        assert_eq!(served.answer.outcome, Outcome::Loaded);
        assert_eq!(crate::lock::Lock::unattended(&lock_file), Some(true));
        assert!(crate::pipeline_snapshot::LoadedPipelines::live(&repo).is_some());
        assert!(
            crate::pipeline_snapshot::recorded_config(&repo)
                .unwrap()
                .is_some()
        );
        assert!(repo.unattended());
    }

    /// A snapshot that cannot be written refuses the reload and puts the lock
    /// back: its text, its modified time and the mode it answers.
    #[test]
    fn a_snapshot_that_cannot_be_written_puts_the_lock_back() {
        let (mut repo, mut held, _root, _lock) = served_project("reload-mode-rollback", "build");
        let lock_file = repo.lock_file();
        let text = std::fs::read_to_string(&lock_file).unwrap();
        let stamp = std::fs::metadata(&lock_file).unwrap().modified().unwrap();
        // A directory where the snapshot belongs, so the rename onto it fails.
        std::fs::create_dir_all(crate::pipeline_snapshot::path(&repo)).unwrap();
        let mut edited = repo.config.clone();
        edited.unattended.enabled = true;
        edited.unattended.blocked_model = "claude-sonnet-5-5".to_string();
        edited.save(&repo.root).unwrap();
        request(&repo).unwrap();

        let served = serve(&mut repo, &mut held).unwrap();

        let Outcome::Refused(reasons) = served.answer.outcome else {
            panic!("reload was not refused");
        };
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert_eq!(std::fs::read_to_string(&lock_file).unwrap(), text);
        assert_eq!(
            std::fs::metadata(&lock_file).unwrap().modified().unwrap(),
            stamp
        );
        assert_eq!(crate::lock::Lock::unattended(&lock_file), Some(false));
        assert!(!repo.config.unattended.enabled);
    }

    /// Unattended on over a blank `unattended.blocked_model` is the state a
    /// start refuses, so a reload refuses it too and leaves the mode alone.
    #[test]
    fn a_reload_to_unattended_with_no_blocked_model_is_refused() {
        let (mut repo, mut held, _root, _lock) = served_project("reload-blank-model", "build");
        let mut edited = repo.config.clone();
        edited.unattended.enabled = true;
        edited.unattended.blocked_model = " ".to_string();
        edited.save(&repo.root).unwrap();
        request(&repo).unwrap();

        let served = serve(&mut repo, &mut held).unwrap();

        let Outcome::Refused(reasons) = served.answer.outcome else {
            panic!("reload was not refused");
        };
        assert!(
            reasons[0]
                .what
                .contains("unattended.blocked_model is blank")
                && reasons[0].fix.contains("turn unattended.enabled off"),
            "{reasons:?}"
        );
        assert_eq!(
            crate::lock::Lock::unattended(&repo.lock_file()),
            Some(false)
        );
    }

    /// A queue file that does not parse cannot be checked, so it refuses the
    /// reload by name instead of being left out of the check.
    #[test]
    fn an_unreadable_task_file_refuses_the_reload() {
        let (mut repo, mut held, _root, _lock) = served_project("reload-unreadable", "build");
        std::fs::write(repo.queue_dir().join("broken.md"), "no frontmatter at all").unwrap();
        request(&repo).unwrap();

        let served = serve(&mut repo, &mut held).unwrap();

        let Outcome::Refused(reasons) = served.answer.outcome else {
            panic!("reload was not refused");
        };
        assert!(
            reasons[0]
                .what
                .contains("broken.md did not parse and was skipped:")
                && reasons[0].what.contains("a reload cannot tell whether"),
            "{reasons:?}"
        );
    }

    /// A task held at a step records it in `paused_at`, `blocked_from` or
    /// `parked_from`, and its lane is still open there, so the step counts
    /// even though `stage:` is a reserved word.
    #[test]
    fn a_held_task_is_stranded_by_the_step_it_was_held_at() {
        let loaded = pipelines("steps:\n  - id: build\n    run: x\n    on_pass: done\n");
        let held = Task::parse(
            PathBuf::from("held.md"),
            "---\nid: held\npipeline: feature\nstage: paused\nparked_from: review\n---\nbody\n",
        )
        .unwrap();
        let reasons = stranded(&loaded, &[held]);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert_eq!(
            reasons[0].what,
            "held: held at `review` (`parked_from`), which pipeline `feature` no longer defines"
        );
    }

    /// An answer that cannot be written leaves its request in place, so the
    /// asker is answered on a later pass instead of never.
    #[test]
    fn a_request_whose_answer_cannot_be_written_stays() {
        let (repo, _root) = fixture("reload-answer-unwritable");
        let id = request(&repo).unwrap();
        // A file where the answers directory belongs.
        std::fs::write(repo.home().join("reload").join("answers"), "").unwrap();

        let problems = settle(&repo, std::slice::from_ref(&id), &Outcome::Loaded);

        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains(&id), "{problems:?}");
        assert_eq!(pending(&repo), [id]);
    }

    /// A refusal fits the problem log's one line per problem.
    #[test]
    fn a_refusal_is_one_line_in_the_problem_log_and_a_good_reload_is_none() {
        let refused = Answer {
            request: "1-1".to_string(),
            outcome: Outcome::Refused(vec![
                Reason {
                    what: "a: gone".into(),
                    fix: "Put it back.".into(),
                },
                Reason {
                    what: "b: gone".into(),
                    fix: "Put it back.".into(),
                },
            ]),
        };
        let line = refused.problem_line().unwrap();
        assert!(!line.contains('\n'), "{line}");
        assert!(
            line.contains("a: gone") && line.contains("b: gone"),
            "{line}"
        );
        let loaded = Answer {
            request: "1-1".to_string(),
            outcome: Outcome::Loaded,
        };
        assert_eq!(loaded.problem_line(), None);
    }
}
