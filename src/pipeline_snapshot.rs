//! The pipelines the running dispatcher loaded, written down so every other
//! process in the project routes on the same graph it does.
//!
//! The dispatcher reads the pipeline files at start, and again when a person
//! asks for a reload — see [`crate::reload`] — and keeps that copy between.
//! Every other command is its own process and reads the files again. Before
//! this, an edit made while tasks ran split the two: a lane's `spoolway
//! report` routed its task onto a step the dispatcher's copy did not have,
//! and the task sat there until somebody restarted it.
//!
//! So the dispatcher that holds the project's lock writes what it loaded
//! here, right after taking the lock and again after each good reload, and
//! `report`, `resume`, `queue resume`, `queue route`, `queue list`, the
//! board's rows and its `r` and `queue add` route on this copy for as long as
//! that dispatcher lives. Each decodes only the pipelines its own tasks name,
//! so a pipeline file somebody broke mid-run no longer fails every lane's
//! report — the dispatcher validated this copy when it loaded it, and nothing
//! here reads the files again.
//!
//! The snapshot also keeps the text of every file the load read. That is
//! how the board's footer and `pipeline check` can name each file edited
//! since the run started: the edit is real on disk and waiting, and what
//! applies it is a restart or a reload.
//!
//! The config the dispatcher last loaded cleanly is kept beside it, under
//! the same lock, for a lane's `report` and a `spoolway stack` step to run
//! on when `config.toml` breaks or goes missing — see [`recorded_config`].
//!
//! Read only while the lock names a live dispatcher, and only when that lock
//! is the one this snapshot was written under — see [`LoadedPipelines::live`].
//! A snapshot a crashed dispatcher left behind is never current, and with
//! no dispatcher running every command reads the files exactly as it did
//! before this existed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::pipeline::{Pipeline, Pipelines};
use crate::repo::Repo;
use crate::task::Task;

/// Beside the dispatcher's own lock file, in the project home.
pub const SNAPSHOT_FILE: &str = "dispatch-pipelines.json";

/// Where this project's snapshot lives.
pub fn path(repo: &Repo) -> PathBuf {
    repo.home().join(SNAPSHOT_FILE)
}

/// The file on disk, as the dispatcher wrote it.
#[derive(Debug, Serialize, Deserialize)]
struct Written {
    /// The lock file's whole text as the writing dispatcher held it: its pid
    /// and that process's start time. Compared whole against the lock as it
    /// is now, rather than by pid alone, because a pid is reused — a new
    /// dispatcher can come up under a dead one's number before it has
    /// written its own snapshot over the old one.
    lock: String,
    /// Each pipeline as loaded — overrides merged, the `blocked` step filled
    /// in from config — kept as an undecoded value per name, so a reader
    /// decodes only the ones it routes on.
    pipelines: BTreeMap<String, serde_json::Value>,
    /// Every name a private and a tracked pipeline both claimed. A snapshot
    /// from before this existed has none, which reads as no clash.
    #[serde(default)]
    clashes: BTreeMap<String, crate::pipeline::Clash>,
    /// Every pipeline source file the load could have read, label to text.
    sources: BTreeMap<String, String>,
    /// The `unattended.blocked_*` values the `blocked` steps above were filled
    /// in from. A snapshot from before this existed has none, which names no
    /// key as changed.
    #[serde(default)]
    blocked: Option<BlockedKeys>,
}

/// The five `unattended.blocked_*` keys, as the dispatcher loaded them.
///
/// Recorded because nothing else keeps them: they are baked into each
/// pipeline's `blocked` step when the pipelines load, while
/// `dispatch-config.toml` follows `config.toml` on every pass. Without this
/// copy the board could not tell an edit to one of them, which waits for a
/// reload, from one the run already uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BlockedKeys {
    agent: String,
    model: String,
    effort: String,
    session: bool,
    prompt: String,
}

impl BlockedKeys {
    fn of(unattended: &crate::config::UnattendedConfig) -> BlockedKeys {
        BlockedKeys {
            agent: unattended.blocked_agent.clone(),
            model: unattended.blocked_model.clone(),
            effort: unattended.blocked_effort.clone(),
            session: unattended.blocked_session,
            prompt: unattended.blocked_prompt.clone(),
        }
    }

    /// The full name of every key whose value differs between `self` and
    /// `now`, in the order `config.toml` lists them.
    fn changed(&self, now: &BlockedKeys) -> Vec<String> {
        [
            ("unattended.blocked_agent", self.agent != now.agent),
            ("unattended.blocked_model", self.model != now.model),
            ("unattended.blocked_effort", self.effort != now.effort),
            ("unattended.blocked_session", self.session != now.session),
            ("unattended.blocked_prompt", self.prompt != now.prompt),
        ]
        .into_iter()
        .filter(|(_, differs)| *differs)
        .map(|(key, _)| key.to_string())
        .collect()
    }
}

/// Write the snapshot for the dispatcher that holds `repo`'s lock: just after
/// it took the lock, and again each time a reload swaps its pipelines.
///
/// `pipelines` is what that dispatcher loaded, and `unattended` the section
/// of the config their `blocked` steps were filled in from. The sources are
/// read here, a moment after the load, so an edit landing in between is
/// recorded as already applied and is never named as waiting on a restart or
/// a reload. That window is the dispatcher's own start-up checks, or on a
/// reload the queue read and the check of it, and anything wider would mean
/// threading the file text through [`Pipelines::load`] for every caller.
///
/// Replaces whatever is there, whoever wrote it: only one dispatcher holds
/// the lock, so an older snapshot belongs to a run that has ended.
pub fn write(
    repo: &Repo,
    pipelines: &Pipelines,
    unattended: &crate::config::UnattendedConfig,
) -> Result<()> {
    let lock_file = repo.lock_file();
    let lock = std::fs::read_to_string(&lock_file)
        .with_context(|| format!("reading {}", lock_file.display()))?;
    let mut encoded = BTreeMap::new();
    for (name, pipeline) in &pipelines.pipelines {
        let value = serde_json::to_value(pipeline)
            .with_context(|| format!("recording pipeline `{name}`"))?;
        encoded.insert(name.clone(), value);
    }
    let written = Written {
        lock,
        pipelines: encoded,
        clashes: pipelines.clashes.clone(),
        sources: sources(&repo.root),
        blocked: Some(BlockedKeys::of(unattended)),
    };
    let path = path(repo);
    crate::task::write_atomic(&path, serde_json::to_string(&written)?)
        .with_context(|| format!("writing {}", path.display()))
}

/// Beside the snapshot, the config the dispatcher last loaded cleanly.
pub const CONFIG_RECORD_FILE: &str = "dispatch-config.toml";

/// Where this project's record of the last good config lives.
pub fn config_record_path(repo: &Repo) -> PathBuf {
    repo.home().join(CONFIG_RECORD_FILE)
}

/// The record as written: the config, headed by the lock text of the
/// dispatcher that loaded it. `lock` comes first because TOML wants plain
/// values ahead of tables.
#[derive(Serialize)]
struct RecordedConfig<'a> {
    lock: String,
    config: &'a crate::config::Config,
}

/// Record `config` as the last one that loaded cleanly under the dispatcher
/// holding `repo`'s lock.
///
/// The merged result is written, override layer included, so a reader needs
/// no second file. It is skipped when the file already holds the same text,
/// so an idle dispatcher does not rewrite it every pass. A command that runs
/// while `config.toml` is broken or gone reads it back through
/// [`recorded_config`].
pub fn record_config(repo: &Repo, config: &crate::config::Config) -> Result<()> {
    let lock_file = repo.lock_file();
    let lock = std::fs::read_to_string(&lock_file)
        .with_context(|| format!("reading {}", lock_file.display()))?;
    let path = config_record_path(repo);
    let rendered = toml::to_string_pretty(&RecordedConfig { lock, config })
        .context("serialising the last good config")?;
    if std::fs::read_to_string(&path).is_ok_and(|held| held == rendered) {
        return Ok(());
    }
    crate::task::write_atomic(&path, rendered)
        .with_context(|| format!("writing {}", path.display()))
}

/// The config the running dispatcher recorded, or `None` when no dispatcher
/// is running, or the record was written under another lock, or none was
/// written yet. A record from a run that has ended is never current, for the
/// same reason a snapshot is not: its settings belong to a run that is gone.
///
/// A record that is there under the live lock but cannot be decoded is an
/// error naming the file, not `None`: a record written by a newer
/// dispatcher with a value this build does not know would otherwise read
/// as "nothing recorded", and the caller would fall back to defaults and
/// drop every cap.
pub fn recorded_config(repo: &Repo) -> Result<Option<crate::config::Config>> {
    let lock_file = repo.lock_file();
    if crate::lock::Lock::holder(&lock_file)?.is_none() {
        return Ok(None);
    }
    let lock = std::fs::read_to_string(&lock_file)
        .with_context(|| format!("reading {}", lock_file.display()))?;
    let path = config_record_path(repo);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let unreadable = || {
        format!(
            "the running dispatcher's record of the last good config, {}, cannot be read \
             (delete it; the dispatcher writes it again on its next pass)",
            path.display()
        )
    };
    let mut table: toml::Table = toml::from_str(&raw).with_context(unreadable)?;
    if table.get("lock").and_then(toml::Value::as_str) != Some(lock.as_str()) {
        return Ok(None);
    }
    let value = table.remove("config").with_context(unreadable)?;
    let mut config: crate::config::Config = value.try_into().with_context(unreadable)?;
    config.migrate();
    Ok(Some(config))
}

/// A snapshot whose writer is the dispatcher running now.
#[derive(Debug)]
pub struct LoadedPipelines {
    written: Written,
    path: PathBuf,
}

impl LoadedPipelines {
    /// The running dispatcher's snapshot, or `None` when no dispatcher is
    /// running — and then the caller reads the files, as it always did.
    ///
    /// Also `None` while a live lock does not match the snapshot's: a
    /// dispatcher in the moment between taking the lock and writing its own
    /// snapshot, or a snapshot that will not parse. Both fall back to the
    /// files rather than refuse, since the files are what every command
    /// routed on before the snapshot existed.
    pub fn live(repo: &Repo) -> Option<LoadedPipelines> {
        let lock_file = repo.lock_file();
        crate::lock::Lock::holder(&lock_file).ok().flatten()?;
        let lock = std::fs::read_to_string(&lock_file).ok()?;
        let path = path(repo);
        let raw = std::fs::read_to_string(&path).ok()?;
        let written: Written = serde_json::from_str(&raw).ok()?;
        (written.lock == lock).then_some(LoadedPipelines { written, path })
    }

    /// The dispatcher's pid, off the lock text's first line.
    fn pid(&self) -> &str {
        self.written.lock.lines().next().unwrap_or_default().trim()
    }

    /// Every pipeline name the dispatcher loaded, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.written.pipelines.keys().map(String::as_str).collect()
    }

    /// Every pipeline name the dispatcher held out of its set because a
    /// private file clashed with a tracked one, sorted. [`Self::names`] does
    /// not list them: the dispatcher routes nothing on them, and the "it has"
    /// list in [`Self::pipelines`]'s refusal would offer a name that cannot
    /// run.
    pub fn clash_names(&self) -> Vec<&str> {
        self.written.clashes.keys().map(String::as_str).collect()
    }

    /// A routing graph holding only the pipelines `names` asks for.
    ///
    /// Refuses a name the dispatcher never loaded, with the restart that
    /// would load it. That is a pipeline added or renamed since the run
    /// started — or one that does not exist at all, which the same line
    /// covers by listing what the dispatcher has.
    ///
    /// What comes back carries no [`Pipeline::blocked_declared`] or
    /// [`Pipeline::private_file`]: both are facts about where a pipeline
    /// was read from, kept only for `pipeline show` and `pipeline list`,
    /// and neither of those reads this.
    pub fn pipelines(&self, names: &[&str]) -> Result<Pipelines> {
        let mut pipelines = BTreeMap::new();
        let mut clashes = BTreeMap::new();
        for &name in names {
            if pipelines.contains_key(name) {
                continue;
            }
            // Carried over so the reader holds the task with the dispatcher's
            // own line, rather than reporting a pipeline it never heard of.
            if let Some(clash) = self.written.clashes.get(name) {
                clashes.insert(name.to_string(), clash.clone());
                continue;
            }
            let Some(value) = self.written.pipelines.get(name) else {
                bail!(
                    "pipeline `{name}` is not one the running dispatcher (pid {}) loaded — it \
                     has {}. A pipeline added since it started is used only once it restarts: \
                     restart the dispatcher, then try again.",
                    self.pid(),
                    self.names().join(", ")
                );
            };
            let mut pipeline: Pipeline =
                serde_json::from_value(value.clone()).with_context(|| {
                    format!("reading pipeline `{name}` from {}", self.path.display())
                })?;
            pipeline.name = name.to_string();
            pipelines.insert(name.to_string(), pipeline);
        }
        Ok(Pipelines {
            pipelines,
            ignored_overrides: Vec::new(),
            clashes,
        })
    }

    /// Every pipeline source file whose text differs from what the
    /// dispatcher loaded — edited, added or removed since — by label,
    /// sorted.
    pub fn changed(&self, root: &Path) -> Vec<String> {
        changed_between(&self.written.sources, &sources(root))
    }
}

/// The routing graph for a command that moves one task while a dispatcher
/// runs: that dispatcher's copy of `task`'s own pipeline. `None` when no
/// dispatcher is running, and then the caller routes on the files it loaded,
/// as it always did.
///
/// Every entry that moves a task one step, or names where a resume would
/// move it, goes through this — `spoolway report`, `spoolway resume`,
/// `spoolway queue resume`, `spoolway queue route` and the board's `r` —
/// because they all end in the same routing, and one left on the files is a
/// road onto a step the dispatcher never loaded.
///
/// Only `task`'s own pipeline is decoded, so a pipeline file broken mid-run
/// fails nothing but a task on that pipeline. A task that cannot be read, or
/// names no pipeline, gets an empty graph: the command's own read of it then
/// fails with its own message, which says more than one from here would.
pub fn for_task(repo: &Repo, task: Option<&str>) -> Option<Result<Pipelines>> {
    let loaded = LoadedPipelines::live(repo)?;
    // Checked before it becomes a path: the command checks it again, and
    // refuses it in its own words.
    let name = task
        .filter(|id| crate::config::check_task_id(id).is_ok())
        .and_then(|id| repo.task(id).ok())
        .and_then(|task| task.front.pipeline);
    let names: Vec<&str> = name.as_deref().into_iter().collect();
    Some(loaded.pipelines(&names))
}

/// What a reader that shows many tasks at once routes on while a dispatcher
/// runs — see [`for_tasks`].
pub struct Routed {
    /// The dispatcher's copy of every pipeline `tasks` name that it can
    /// route, and nothing else.
    pub pipelines: Pipelines,
    /// Each task the dispatcher's copy cannot route, by id, with the reason
    /// in a sentence a row can print.
    pub refused: BTreeMap<String, String>,
}

/// The routing graph for a reader that shows many tasks at once, such as
/// `queue list` and the board's rows: that dispatcher's copy of the
/// pipelines `tasks` name. `None` when no dispatcher is running.
///
/// Such a reader cannot name one pipeline up front the way [`for_task`]
/// does, and each row it draws names the step a resume takes — which is the
/// step in the dispatcher's copy.
///
/// A task whose pipeline the dispatcher never loaded, or whose entry will
/// not decode, comes back in [`Routed::refused`] instead of failing the
/// reading. One such task is a pipeline added since the run started, which
/// the board's own queue tab can still queue, and a reading that failed on
/// it would stop the board redrawing for every other task. A task naming no
/// pipeline adds nothing here, and the reader's own read of that task fails
/// with its own message.
pub fn for_tasks(repo: &Repo, tasks: &[Task]) -> Option<Routed> {
    let loaded = LoadedPipelines::live(repo)?;
    let mut routed = Pipelines {
        pipelines: BTreeMap::new(),
        ignored_overrides: Vec::new(),
        clashes: Default::default(),
    };
    let mut refusals: BTreeMap<&str, String> = BTreeMap::new();
    let mut refused = BTreeMap::new();
    for task in tasks {
        let Some(name) = task.front.pipeline.as_deref() else {
            continue;
        };
        if !refusals.contains_key(name) && !routed.pipelines.contains_key(name) {
            match loaded.pipelines(&[name]) {
                Ok(one) => {
                    routed.pipelines.extend(one.pipelines);
                    routed.clashes.extend(one.clashes);
                }
                // A name the dispatcher never loaded gets a sentence short
                // enough for a table cell; anything else is an entry that
                // will not decode, and says so in the error's own words.
                Err(err) => {
                    let reason = match loaded.written.pipelines.contains_key(name) {
                        true => format!("{err:#}"),
                        false => format!(
                            "pipeline `{name}` is not one the running dispatcher loaded — \
                             restart the dispatcher to use it"
                        ),
                    };
                    refusals.insert(name, reason);
                }
            }
        }
        if let Some(refusal) = refusals.get(name) {
            refused.insert(task.id().to_string(), refusal.clone());
        }
    }
    Some(Routed {
        pipelines: routed,
        refused,
    })
}

/// The files edited since the running dispatcher started, or nothing when no
/// dispatcher is running — with none running, the next one loads them as
/// they are, and there is no restart to wait on.
pub fn changed_since_start(repo: &Repo) -> Vec<String> {
    LoadedPipelines::live(repo)
        .map(|loaded| loaded.changed(&repo.root))
        .unwrap_or_default()
}

/// The config keys whose value in `config.toml` differs from the one the
/// running dispatcher is using, and which wait for a reload or a restart:
/// `unattended.enabled` against the mode in the lock, and each
/// `unattended.blocked_*` key against what the snapshot recorded. Every other
/// key reaches the run on its next pass, so it never waits to be named.
///
/// Nothing when no dispatcher is running, as for [`changed_since_start`].
/// Nothing either while `config.toml` does not load: there is no value to
/// compare, and a reload would be refused until the file is fixed.
pub fn config_changed_since_start(repo: &Repo) -> Vec<String> {
    let Some(loaded) = LoadedPipelines::live(repo) else {
        return Vec::new();
    };
    let Ok(config) = crate::config::Config::load_present(&repo.root) else {
        return Vec::new();
    };
    let mut changed = Vec::new();
    if crate::lock::Lock::unattended(&repo.lock_file())
        .is_some_and(|run| run != config.unattended.enabled)
    {
        changed.push("unattended.enabled".to_string());
    }
    if let Some(run) = &loaded.written.blocked {
        changed.extend(run.changed(&BlockedKeys::of(&config.unattended)));
    }
    changed
}

/// The line `pipeline check` prints about `changed`, or `None` when nothing
/// changed. `since` is what the reader calls the run.
pub fn restart_notice(changed: &[String], since: &str) -> Option<String> {
    let pronoun = match changed.len() {
        0 => return None,
        1 => "it",
        _ => "them",
    };
    Some(format!(
        "{} changed since {since} started — restart the dispatcher to use {pronoun}",
        changed.join(", ")
    ))
}

/// Labels whose text differs between `then` and `now`, including a file
/// present in only one of them.
fn changed_between(then: &BTreeMap<String, String>, now: &BTreeMap<String, String>) -> Vec<String> {
    let mut changed: Vec<String> = then
        .keys()
        .chain(now.keys())
        .filter(|label| then.get(*label) != now.get(*label))
        .cloned()
        .collect();
    changed.sort();
    changed.dedup();
    changed
}

/// Every file [`Pipelines::load`] reads for `root`, label to text: the
/// tracked pipelines, the private layer's and the override patches.
///
/// The tracked files are labelled by bare file name, since that is the name
/// a person edited them under. The other two carry their directory under the
/// project home, so a private `t.yml` is never mistaken for the tracked one.
///
/// Best effort throughout: a directory or file that cannot be read is left
/// out rather than failing, because the one thing this feeds is a notice.
/// The worst a gap costs is a restart nobody was told about, which is what
/// every run cost before.
fn sources(root: &Path) -> BTreeMap<String, String> {
    let mut sources = BTreeMap::new();
    read_yaml_into(&Pipelines::dir_in(root), "", &mut sources);
    if crate::local::is_repo_mode(root)
        && let Ok(local) = crate::local::dir_for(root)
    {
        read_yaml_into(
            &crate::local::pipelines_dir(&local),
            "local/pipelines/",
            &mut sources,
        );
    }
    if let Ok(overrides) = crate::overrides::dir_for(root) {
        read_yaml_into(
            &crate::overrides::pipeline_patches_dir(&overrides),
            "overrides/pipelines/",
            &mut sources,
        );
    }
    sources
}

/// Every `.yml` and `.yaml` file directly under `dir`, labelled `prefix`
/// then its file name — the same two extensions [`Pipelines::load`] reads.
fn read_yaml_into(dir: &Path, prefix: &str, into: &mut BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_yaml = path
            .extension()
            .is_some_and(|ext| ext == "yml" || ext == "yaml");
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_yaml {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            into.insert(format!("{prefix}{file_name}"), text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::fixture;

    /// A project whose one pipeline file is the shipped `default`, loaded
    /// the way the dispatcher loads it.
    fn project(name: &str) -> (Repo, crate::scratch::ScratchRoot, Pipelines) {
        let (repo, root) = fixture(name);
        let dir = Pipelines::dir_in(&repo.root);
        std::fs::create_dir_all(&dir).unwrap();
        let shipped = Pipelines::builtin();
        let default = shipped.get("default").unwrap();
        let mut file = default.clone();
        // The file as a person would write it: no materialised `blocked`.
        file.steps.retain(|s| s.id != crate::pipeline::BLOCKED);
        std::fs::write(dir.join("t.yml"), serde_norway::to_string(&file).unwrap()).unwrap();
        let pipelines = Pipelines::load(&repo.root, &repo.config).unwrap();
        (repo, root, pipelines)
    }

    #[test]
    fn a_snapshot_is_read_only_under_the_lock_it_was_written_under() {
        let (repo, _root, pipelines) = project("snapshot-lock");

        // No dispatcher: no snapshot is current, whatever is on disk.
        assert!(LoadedPipelines::live(&repo).is_none());

        {
            let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
            // Held, but nothing written under it yet: the files still rule.
            assert!(LoadedPipelines::live(&repo).is_none());
            write(&repo, &pipelines, &repo.config.unattended).unwrap();
            let loaded = LoadedPipelines::live(&repo).expect("written under the live lock");
            assert_eq!(loaded.names(), vec!["t"]);
        }

        // The run ended and left its snapshot behind: never current again.
        assert!(path(&repo).exists());
        assert!(LoadedPipelines::live(&repo).is_none());

        // A new run under a new lock does not read the old run's snapshot
        // until it writes its own over it. This test is one process, so the
        // pid and start time match the old lock's and only the mode line
        // tells the two apart. A real new dispatcher differs by start time
        // even when it is handed the dead one's pid.
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), true, None).unwrap();
        assert!(LoadedPipelines::live(&repo).is_none());
    }

    /// The recorded config is current only under the lock it was written
    /// under, and a record that cannot be decoded under that lock is an
    /// error, not an absent record.
    #[test]
    fn a_recorded_config_is_current_only_under_its_own_lock() {
        let (repo, _root, _pipelines) = project("snapshot-config");
        let mut config = crate::config::Config::default();
        config.agents.get_mut("claude").unwrap().concurrency = 7;

        // No dispatcher, so nothing to record under or to read back.
        assert!(record_config(&repo, &config).is_err());
        assert!(recorded_config(&repo).unwrap().is_none());

        {
            let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
            assert!(recorded_config(&repo).unwrap().is_none(), "nothing yet");
            record_config(&repo, &config).unwrap();
            let back = recorded_config(&repo).unwrap().expect("recorded");
            assert_eq!(back.agents["claude"].concurrency, 7);
        }

        // The run ended and left its record: never current again, and not
        // current for a new run under another lock either.
        assert!(config_record_path(&repo).exists());
        assert!(recorded_config(&repo).unwrap().is_none());
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), true, None).unwrap();
        assert!(recorded_config(&repo).unwrap().is_none());

        // Under the live lock, a record this build cannot decode names itself.
        let lock = std::fs::read_to_string(repo.lock_file()).unwrap();
        let broken = toml::to_string(&toml::toml! {
            lock = lock
            [config.agents.claude]
            concurrency = "many"
        })
        .unwrap();
        std::fs::write(config_record_path(&repo), broken).unwrap();
        let said = format!("{:#}", recorded_config(&repo).unwrap_err());
        assert!(said.contains("dispatch-config.toml"), "{said}");
    }

    /// A clash survives the dispatcher's snapshot: a reader routing on it
    /// holds the task with the dispatcher's own line, rather than calling
    /// the pipeline one the dispatcher never loaded.
    #[test]
    fn a_snapshot_carries_a_name_clash_to_the_readers() {
        let (repo, _root, mut pipelines) = project("snapshot-clash");
        pipelines.pipelines.remove("t").unwrap();
        pipelines.clashes.insert(
            "t".into(),
            crate::pipeline::Clash {
                tracked: "tracked/t.yml".into(),
                private: "local/t.yml".into(),
            },
        );
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        write(&repo, &pipelines, &repo.config.unattended).unwrap();

        let task = crate::task::Task::parse(
            PathBuf::from("q.md"),
            "---\nid: q\nstage: queued\npipeline: t\n---\n",
        )
        .unwrap();
        let routed = for_tasks(&repo, std::slice::from_ref(&task)).unwrap();
        assert!(routed.refused.is_empty(), "{:?}", routed.refused);
        let held = routed.pipelines.for_task(&task).unwrap_err();
        assert!(
            format!("{held:#}").contains("spoolway pipeline rename t <new-name>"),
            "{held:#}"
        );
    }

    /// `unattended.enabled` is named once `config.toml` disagrees with the
    /// mode in the lock, and an `unattended.blocked_*` key once it disagrees
    /// with what the snapshot recorded; a key the run takes up on its own
    /// next pass is never named, and nothing is with no dispatcher running.
    /// Fails before the change: nothing compared the config with the run.
    #[test]
    fn config_keys_that_wait_for_a_reload_are_named_once_they_differ() {
        let (repo, _root, pipelines) = project("snapshot-config-keys");
        let lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        let mut config = repo.config.clone();
        config.unattended.enabled = false;
        config.save(&repo.root).unwrap();
        write(&repo, &pipelines, &config.unattended).unwrap();
        assert!(config_changed_since_start(&repo).is_empty());

        config.dispatch.lane_quiet = std::time::Duration::from_secs(45 * 60);
        config.save(&repo.root).unwrap();
        assert!(config_changed_since_start(&repo).is_empty());

        config.unattended.enabled = true;
        config.unattended.blocked_model = "another-model".into();
        config.unattended.blocked_session = !config.unattended.blocked_session;
        config.save(&repo.root).unwrap();
        assert_eq!(
            config_changed_since_start(&repo),
            [
                "unattended.enabled",
                "unattended.blocked_model",
                "unattended.blocked_session"
            ]
        );

        drop(lock);
        assert!(config_changed_since_start(&repo).is_empty());
    }

    #[test]
    fn a_snapshot_routes_on_what_was_loaded_not_what_is_on_disk() {
        let (repo, _root, pipelines) = project("snapshot-routes");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        write(&repo, &pipelines, &repo.config.unattended).unwrap();

        // The file is broken mid-run. The snapshot does not care.
        std::fs::write(Pipelines::file_in(&repo.root, "t"), "steps: [").unwrap();
        let loaded = LoadedPipelines::live(&repo).unwrap();
        let routed = loaded.pipelines(&["t", "t"]).unwrap();
        let t = routed.get("t").unwrap();
        assert_eq!(t.name, "t");
        let original = pipelines.get("t").unwrap();
        assert_eq!(t.step_ids(), original.step_ids());
        // Every field survives the trip, not only the step ids.
        assert_eq!(
            serde_json::to_value(t).unwrap(),
            serde_json::to_value(original).unwrap()
        );
        t.validate().unwrap();
    }

    #[test]
    fn a_pipeline_the_dispatcher_never_loaded_is_refused_naming_the_restart() {
        let (repo, _root, pipelines) = project("snapshot-unknown");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        write(&repo, &pipelines, &repo.config.unattended).unwrap();

        let err = LoadedPipelines::live(&repo)
            .unwrap()
            .pipelines(&["added"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("pipeline `added`"), "{err}");
        assert!(err.contains("it has t"), "{err}");
        assert!(err.contains("restart the dispatcher"), "{err}");
    }

    #[test]
    fn each_file_edited_added_or_removed_since_the_start_is_named() {
        let (repo, _root, pipelines) = project("snapshot-changed");
        let dir = Pipelines::dir_in(&repo.root);
        std::fs::write(dir.join("gone.yml"), "steps: []\n").unwrap();
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        write(&repo, &pipelines, &repo.config.unattended).unwrap();
        assert!(changed_since_start(&repo).is_empty());

        std::fs::write(dir.join("t.yml"), "# edited\n").unwrap();
        std::fs::write(dir.join("new.yaml"), "steps: []\n").unwrap();
        std::fs::remove_file(dir.join("gone.yml")).unwrap();
        // Not a pipeline file: never named.
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        assert_eq!(
            changed_since_start(&repo),
            vec!["gone.yml", "new.yaml", "t.yml"]
        );
    }

    #[test]
    fn nothing_is_waiting_on_a_restart_with_no_dispatcher_running() {
        let (repo, _root, pipelines) = project("snapshot-no-run");
        {
            let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
            write(&repo, &pipelines, &repo.config.unattended).unwrap();
        }
        std::fs::write(Pipelines::file_in(&repo.root, "t"), "# edited\n").unwrap();
        assert!(changed_since_start(&repo).is_empty());
    }

    #[test]
    fn the_restart_notice_names_every_file_and_agrees_in_number() {
        assert_eq!(restart_notice(&[], "this run"), None);
        assert_eq!(
            restart_notice(&["t.yml".into()], "this run").unwrap(),
            "t.yml changed since this run started — restart the dispatcher to use it"
        );
        assert_eq!(
            restart_notice(&["a.yml".into(), "b.yml".into()], "the running dispatcher").unwrap(),
            "a.yml, b.yml changed since the running dispatcher started — restart the \
             dispatcher to use them"
        );
    }
}
