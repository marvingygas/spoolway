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
//! which answers every request it finds under `reload/answers/`, and
//! [`answer`] reads that answer back by id.
//!
//! A reload is refused, and changes nothing, when the new pipelines do not
//! load, or when a task in the queue sits on a step, or names a pipeline, the
//! new pipelines lack. No command moves a task off a step that is gone, so the
//! one edit that strands work is the one this refuses.

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
    /// is about one.
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
/// The file is created in place, not renamed into place, so the one event the
/// watch sees already finds the whole request there.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the board key and the terminal command write requests through this"
    )
)]
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the board key and the terminal command read answers through this"
    )
)]
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

/// The ids of the requests waiting, oldest first. A name starting with a dot
/// is a file still being written by something else, and is not a request.
fn pending(repo: &Repo) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(requests_dir(repo))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    ids.sort();
    ids
}

/// Answer every id in `ids` with `outcome`, then clear the requests. The
/// answers go first: a request that vanished before its answer existed would
/// leave the asker waiting for nothing.
fn settle(repo: &Repo, ids: &[String], outcome: &Outcome) {
    let answers = answers_dir(repo);
    prune_answers(&answers);
    for id in ids {
        let answer = Answer {
            request: id.clone(),
            outcome: outcome.clone(),
        };
        if let Ok(text) = serde_json::to_string(&answer) {
            let _ = crate::task::write_atomic(&answers.join(format!("{id}.json")), text);
        }
        let _ = std::fs::remove_file(requests_dir(repo).join(id));
    }
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

/// Answer, as loaded, every request a dead dispatcher left behind.
///
/// Called by a dispatcher right after it takes the lock. It has just loaded
/// every file, so there is nothing more for those requests to ask for, and
/// serving them as well would load twice.
pub(crate) fn settle_left_behind(repo: &Repo) {
    let ids = pending(repo);
    if !ids.is_empty() {
        settle(repo, &ids, &Outcome::Loaded);
    }
}

/// Serve the requests waiting, at the top of a pass.
///
/// Returns `None` when there were none. Otherwise it loads the pipelines from
/// `live`'s root with the config as it now stands, checks them, and on a good
/// load rewrites the snapshot and then replaces `pipelines` and `live.config`.
/// The snapshot is written before the swap and before anything else runs, so
/// no lane launches on a graph its own `report` would not route on. Every
/// request waiting gets the same answer.
///
/// All of it is built and checked before anything is touched. A refusal, or a
/// snapshot that cannot be written, leaves `pipelines`, `live.config` and the
/// snapshot as they were.
pub(crate) fn serve(live: &mut Repo, pipelines: &mut Pipelines) -> Option<Answer> {
    let ids = pending(live);
    let last = ids.last()?.clone();
    let outcome = match try_reload(live) {
        Ok((config, loaded)) => match crate::pipeline_snapshot::write(live, &loaded) {
            Ok(()) => {
                live.config = config;
                *pipelines = loaded;
                Outcome::Loaded
            }
            Err(err) => Outcome::Refused(vec![Reason {
                what: format!("the new pipelines could not be recorded: {err:#}"),
                fix: "Make the project home writable, then reload again.".to_string(),
            }]),
        },
        Err(reasons) => Outcome::Refused(reasons),
    };
    settle(live, &ids, &outcome);
    Some(Answer {
        request: last,
        outcome,
    })
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
    let loaded = Pipelines::load(&live.root, &config).map_err(|err| {
        vec![Reason {
            what: format!("the pipelines do not load: {err:#}"),
            fix: "Fix the file it names, then reload again.".to_string(),
        }]
    })?;
    let (tasks, _unreadable) = live.tasks_and_problems().map_err(|err| {
        vec![Reason {
            what: format!("the queue cannot be read: {err:#}"),
            fix: "Make the queue readable, then reload again.".to_string(),
        }]
    })?;
    let stranded = stranded(&loaded, &tasks);
    if stranded.is_empty() {
        Ok((config, loaded))
    } else {
        Err(stranded)
    }
}

/// Every task, running or waiting, that the new pipelines would leave without
/// the step it sits on or the pipeline it names.
///
/// A task on a reserved stage sits on no step. A task on a name two files
/// claim is held by the dispatcher in any case, and a task naming no pipeline
/// is refused at the start of a run, so neither says anything about this
/// reload.
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
        let stage = task.stage();
        if !crate::pipeline::RESERVED.contains(&stage) && pipeline.step(stage).is_none() {
            reasons.push(Reason {
                what: format!(
                    "{id}: sitting on `{stage}`, which pipeline `{name}` no longer defines"
                ),
                fix: format!(
                    "Put `{stage}` back in {name}.yml, or wait until {id} has left it, then \
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
}
