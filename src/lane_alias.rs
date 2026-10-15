//! A durable record mapping the name a lane has in herdr back to its full
//! spoolway name — `<task> · <step>`.
//!
//! Herdr's sidebar shows one name per agent, at most 32 characters and unique
//! among every live agent. A lane is started as `<step>-<tag>`, where the tag
//! is the first characters of a stable hash of its task id, so a task's lanes
//! share it and the step is what tells them apart. A name like that cannot be
//! turned back into a lane by reading it, so [`crate::mux::Herdr::start_lane`]
//! asks [`reserve`] for every lane it starts, and [`reserve`] writes the
//! record down — atomically, and before the launch it names ever reaches
//! herdr — so a dispatcher that dies between the two, or right after them,
//! still finds its way back from the name to the lane on its next restart.
//!
//! Two tasks can share a tag. The name is then taken only if herdr lists it
//! as a live agent right now: the tag grows by one character, up to five, for
//! the second task. A record never makes a name taken, because the file is
//! not pruned until a task is archived and an old record would keep
//! `implement-5f2` unavailable long after its agent closed.
//!
//! A session started before this existed is read back as it always was: a
//! `<task>__<step>` name through [`crate::mux::from_agent_name`], and an
//! opaque `l…` name through the record it was given.
//!
//! One file per project, beside its other lane bookkeeping (`lanes.json`) —
//! see [`crate::repo::Repo::home`] — never per task, so a restart has exactly
//! one place to read every alias this project has ever handed out from.
//! [`remove_task`] drops a task's records when it is archived.
//!
//! Every lane depends on its record, so a file that does not parse is an
//! error every reader reports, never "no records": reading it as empty would
//! make each live agent look like an unknown name, and the next reservation
//! would save over the file and lose every other lane's record for good.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The file every alias this project has reserved is recorded in.
const FILE_NAME: &str = "lane_aliases.json";

/// The file the records' cross-process lock is taken on.
const LOCK_NAME: &str = "lane_aliases.lock";

/// Held across every load-modify-save of the file in this process, so the
/// dispatcher's lane threads queue here rather than polling the file lock
/// below for each other. It does not reach another process: `spoolway trial
/// discard` drops records from one of its own, which
/// [`crate::lock::LaneAliasLock`] covers.
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// The fewest tag characters a name carries.
const TAG_MIN: usize = 3;

/// The most tag characters a name carries. A step id is held to
/// [`crate::config::STEP_ID_MAX`], which is 32 less a hyphen and this.
const TAG_MAX: usize = 5;

/// One lane's alias, as recorded on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AliasRecord {
    /// The wire name handed to herdr — `agent start`'s own argument, and what
    /// `agent list` answers back for this session.
    pub alias: String,
    /// The full lane name — `<task> · <step>` — this alias stands for.
    pub lane: String,
    /// The pane the aliased session was launched in. Checked again on
    /// restart, against the pane a live `agent list` reports for this same
    /// alias, so a stale record — or one whose alias has since been handed to
    /// an unrelated session — is never mistaken for a live lane of this name.
    pub pane_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Records {
    #[serde(default)]
    entries: Vec<AliasRecord>,
}

/// Where a project's alias records live.
pub fn alias_store_path(project_home: &Path) -> PathBuf {
    project_home.join(FILE_NAME)
}

/// Read every record a project has on file. A file that does not exist yet
/// (no lane has been started) reads as no records. A file that cannot be read
/// or does not parse is an error: starting from nothing would hide every
/// live lane's record, and the next [`save`] would overwrite the file that
/// still holds them.
fn load(path: &Path) -> Result<Records> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Records::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&raw).with_context(|| {
        format!(
            "the lane records in {} do not parse, so no lane can be found by its herdr name — \
             repair the file, or move it aside to start with no records (running lanes are then \
             no longer matched to their names)",
            path.display()
        )
    })
}

/// Run `change` over the records with the file locked against every other
/// thread and process that changes it, and save what it leaves. `change`
/// answers whether it altered anything, so a pass that finds nothing to do
/// leaves the file untouched. A file that does not parse stops here, before
/// `change` runs, so it is never overwritten.
fn modify(project_home: &Path, change: impl FnOnce(&mut Records) -> bool) -> Result<()> {
    let path = alias_store_path(project_home);
    let _threads = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _processes = crate::lock::LaneAliasLock::acquire(&lock_path(project_home))?;
    let mut records = load(&path)?;
    if change(&mut records) {
        save(&path, &records)?;
    }
    Ok(())
}

/// The file [`crate::lock::LaneAliasLock`] is taken on, beside the records.
fn lock_path(project_home: &Path) -> PathBuf {
    project_home.join(LOCK_NAME)
}

fn save(path: &Path, records: &Records) -> Result<()> {
    let json = serde_json::to_string_pretty(records).context("encoding lane alias records")?;
    // The same temp-file-then-rename `write_atomic` every other piece of
    // project state goes through — see `Repo::lanes_file`'s own writer — so a
    // crash mid-write leaves the previous, still-consistent file in place
    // rather than a truncated one for the very next restart to choke on.
    crate::task::write_atomic(path, json)
}

/// A stable hash of a task id, as 16 hex characters.
///
/// FNV-1a rather than `std`'s `DefaultHasher`: that one's algorithm may
/// change between Rust releases, and the tag it gave would then differ
/// between a lane started before an upgrade and one started after, for the
/// same task.
fn task_hash(task: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in task.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Reserve the herdr name for `lane`, about to be started in `pane_id`, and
/// write it down before the caller ever tells herdr to launch anything — the
/// ordering the restart check in [`lane_for`] depends on: a crash right after
/// this call still has a record naming the pane the agent is about to occupy,
/// and a crash before it leaves nothing for a later `agent list` to misread
/// as this lane at all.
///
/// The name is `<step>-<tag>`: the tag is the first three hex characters of
/// [`task_hash`], and grows to four, then five, only for each name `taken`
/// already holds. `taken` is every name herdr answers to right now, read
/// fresh from a live `agent list` immediately before this call — the one
/// source that can catch a clash with a session this project's own records
/// know nothing about, a human's own scratch session included.
///
/// Any record already on file for `lane` is replaced: a lane restarted on a
/// new pane, after its old one closed, asks for a fresh name rather than
/// keeping one that named a pane it no longer occupies.
pub fn reserve(
    project_home: &Path,
    lane: &str,
    pane_id: &str,
    taken: &HashSet<String>,
) -> Result<String> {
    let Some((task, step)) = lane.split_once(crate::mux::LANE_NAME_SEPARATOR) else {
        bail!(
            "lane `{lane}` is not named `<task> · <step>`, so there is no step to name its herdr \
             agent after — start lanes through a pipeline step"
        );
    };
    let hash = task_hash(task);
    let longest = format!("{step}-{}", &hash[..TAG_MAX]);
    if longest.chars().count() > crate::mux::AGENT_NAME_MAX {
        bail!(
            "step id `{step}` is too long to name a herdr agent: with a hyphen and a tag of up to \
             {TAG_MAX} characters it passes herdr's {} characters — shorten the step id to {} \
             characters or fewer",
            crate::mux::AGENT_NAME_MAX,
            crate::config::STEP_ID_MAX
        );
    }
    let alias = (TAG_MIN..=TAG_MAX)
        .map(|len| format!("{step}-{}", &hash[..len]))
        .find(|name| !taken.contains(name));
    let Some(alias) = alias else {
        bail!(
            "every herdr name `{step}-<tag>` that lane `{lane}` could take is already held by a \
             live agent — close the agent that holds `{step}-{}` in herdr, or rename the task",
            &hash[..TAG_MIN]
        );
    };

    modify(project_home, |records| {
        records.entries.retain(|r| r.lane != lane);
        records.entries.push(AliasRecord {
            alias: alias.clone(),
            lane: lane.to_string(),
            pane_id: pane_id.to_string(),
        });
        true
    })?;
    Ok(alias)
}

/// Drop every record of the tasks in `tasks`, once they are archived: the
/// lanes they name are gone, and the file is never pruned any other way. A
/// routine copy's lanes are named for its routine, so the caller passes both.
pub fn remove_task(project_home: &Path, tasks: &[&str]) -> Result<()> {
    modify(project_home, |records| {
        let before = records.entries.len();
        records
            .entries
            .retain(|r| !tasks.contains(&crate::mux::lane_task(&r.lane)));
        records.entries.len() != before
    })
}

/// The alias already on record for `lane`, if [`reserve`] was ever called for
/// it — what every wire call past the launch itself (`prompt`, `read`,
/// `interrupt_lane`, `focus_lane`) resolves a lane's name through, rather
/// than trying to spell it out on the wire a second time. `Err` when the
/// file on disk does not parse.
pub fn alias_for(project_home: &Path, lane: &str) -> Result<Option<String>> {
    Ok(load(&alias_store_path(project_home))?
        .entries
        .into_iter()
        .find(|r| r.lane == lane)
        .map(|r| r.alias))
}

/// The full lane name behind `alias`, if its record's pane still matches
/// `pane_id` — the restart check itself. An alias this project never
/// reserved, or one whose record now points at a different pane, answers
/// `None` rather than a guess: [`crate::mux::Herdr::list_lanes`] then leaves
/// the wire name to [`crate::mux::from_agent_name`]'s own reversible
/// decoding instead, which is exactly what a session herdr did not get this
/// alias from — someone else's scratch session, or a stale record's pane
/// reused by an unrelated launch — needs: never adopted as one of ours, and
/// never mistaken for a duplicate of the lane it used to name. `Err` when the
/// file on disk does not parse, which is not the same as a name with no
/// record.
pub fn lane_for(project_home: &Path, alias: &str, pane_id: &str) -> Result<Option<String>> {
    Ok(load(&alias_store_path(project_home))?
        .entries
        .into_iter()
        .find(|r| r.alias == alias && r.pane_id == pane_id)
        .map(|r| r.lane))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that removes itself, without a dev-dependency —
    /// the same pattern `version.rs` uses for the same reason.
    struct TempDir(PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> TempDir {
        let base = std::env::temp_dir().join(format!(
            "spoolway-lane-alias-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        TempDir(base)
    }

    #[test]
    fn a_reserved_alias_resolves_back_through_its_pane() {
        let dir = tempdir();
        let lane = "release-spoolway-2 · merge-released-repair";
        let alias = reserve(dir.path(), lane, "w1:p1", &HashSet::new()).unwrap();
        assert!(alias.chars().count() <= 32);
        assert_eq!(alias_for(dir.path(), lane).unwrap(), Some(alias.clone()));
        assert_eq!(
            lane_for(dir.path(), &alias, "w1:p1").unwrap(),
            Some(lane.to_string())
        );
    }

    /// The restart check: an alias whose record now names a different pane —
    /// a stale record, or one a later launch reused for something else — must
    /// never hand back the lane it used to belong to.
    #[test]
    fn a_record_whose_pane_no_longer_matches_is_never_adopted() {
        let dir = tempdir();
        let lane = "release-spoolway-2 · merge-released-repair";
        let alias = reserve(dir.path(), lane, "w1:p1", &HashSet::new()).unwrap();
        assert_eq!(lane_for(dir.path(), &alias, "w1:p9").unwrap(), None);
    }

    /// An alias nothing ever reserved is unknown, not a guess.
    #[test]
    fn an_unrecorded_alias_resolves_to_nothing() {
        let dir = tempdir();
        assert_eq!(lane_for(dir.path(), "lnotreal", "w1:p1").unwrap(), None);
    }

    #[test]
    fn reserving_again_for_the_same_lane_drops_its_old_record() {
        let dir = tempdir();
        let lane = "release-spoolway-2 · merge-released-repair";
        let first = reserve(dir.path(), lane, "w1:p1", &HashSet::new()).unwrap();
        let second = reserve(dir.path(), lane, "w1:p2", &HashSet::new()).unwrap();
        // The old pane no longer answers for this lane at all — not even to
        // refuse it as mismatched — because the record itself is gone.
        assert_eq!(lane_for(dir.path(), &first, "w1:p1").unwrap(), None);
        assert_eq!(
            lane_for(dir.path(), &second, "w1:p2").unwrap(),
            Some(lane.to_string())
        );
    }

    /// Shows why [`FILE_LOCK`] and [`crate::lock::LaneAliasLock`] exist: a
    /// load-modify-save with nothing around it loses a record when two calls
    /// interleave. Orchestrated by hand rather than by racing real threads,
    /// which would make this test's result depend on how the scheduler lands
    /// rather than on the bug. Both callers load before either saves, and the
    /// second save overwrites the first's record outright rather than
    /// merging with it. [`reserve`] and [`remove_task`] go through [`modify`],
    /// which holds both locks from the load to the save.
    #[test]
    fn two_unsynchronised_reserves_lose_the_first_ones_record() {
        let dir = tempdir();
        let path = alias_store_path(dir.path());

        let mut first = load(&path).unwrap();
        first.entries.retain(|r| r.lane != "a");
        first.entries.push(AliasRecord {
            alias: "step-aaa".to_string(),
            lane: "a".to_string(),
            pane_id: "w1:p1".to_string(),
        });

        // `second`'s own load happens before `first`'s save below — the read
        // half of the race: it sees the same empty file `first` did.
        let mut second = load(&path).unwrap();
        second.entries.retain(|r| r.lane != "b");
        second.entries.push(AliasRecord {
            alias: "step-bbb".to_string(),
            lane: "b".to_string(),
            pane_id: "w1:p2".to_string(),
        });

        save(&path, &first).unwrap();
        save(&path, &second).unwrap();

        // `a`'s record, written first, is gone — `second`'s save carried no
        // knowledge of it and overwrote the whole file.
        assert_eq!(
            alias_for(dir.path(), "a").unwrap(),
            None,
            "a's record was lost"
        );
        assert!(alias_for(dir.path(), "b").unwrap().is_some());
    }

    /// A lane starts as its step and its task's tag, so the step is what tells
    /// a task's lanes apart in herdr's sidebar and the tag is shared by all
    /// of them.
    #[test]
    fn a_lane_is_named_for_its_step_and_its_tasks_three_character_tag() {
        let dir = tempdir();
        let tag = &task_hash("queue-gate-login")[..3];
        let first = reserve(
            dir.path(),
            "queue-gate-login · implement",
            "w1:p1",
            &HashSet::new(),
        )
        .unwrap();
        let second = reserve(
            dir.path(),
            "queue-gate-login · review-spec",
            "w1:p2",
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(first, format!("implement-{tag}"));
        assert_eq!(second, format!("review-spec-{tag}"));
    }

    /// The tag is in names herdr is still showing after an upgrade, so the
    /// hash behind it must not move. These are FNV-1a values worked out
    /// independently of [`task_hash`].
    #[test]
    fn the_tag_hash_is_a_fixed_function_of_the_task_id() {
        assert_eq!(task_hash("a"), "af63dc4c8601ec8c");
        assert_eq!(task_hash(""), "cbf29ce484222325");
    }

    /// A name a live agent holds is skipped by growing the tag, one character
    /// at a time, up to five; past that there is nothing left to try.
    #[test]
    fn a_live_name_grows_the_tag_to_four_then_five_characters() {
        let dir = tempdir();
        let lane = "queue-gate-login · implement";
        let hash = task_hash("queue-gate-login");
        let mut taken: HashSet<String> = [format!("implement-{}", &hash[..3])].into();
        let four = reserve(dir.path(), lane, "w1:p1", &taken).unwrap();
        assert_eq!(four, format!("implement-{}", &hash[..4]));
        taken.insert(four);
        let five = reserve(dir.path(), lane, "w1:p1", &taken).unwrap();
        assert_eq!(five, format!("implement-{}", &hash[..5]));
        taken.insert(five);
        let err = reserve(dir.path(), lane, "w1:p1", &taken).unwrap_err();
        assert!(format!("{err:#}").contains("close the agent"), "{err:#}");
    }

    /// A step id past the limit cannot be named at all, and the refusal says
    /// the step is the problem, not a live agent.
    #[test]
    fn a_step_too_long_for_herdr_is_refused_by_name() {
        let dir = tempdir();
        let lane = "task-a · a-step-name-longer-than-herdrs-own-rule";
        let err = reserve(dir.path(), lane, "w1:p1", &HashSet::new()).unwrap_err();
        assert!(
            format!("{err:#}").contains("shorten the step id"),
            "{err:#}"
        );
    }

    /// An old record does not make its name taken: only a live agent does.
    /// Otherwise a closed lane's `implement-5f2` would be lost for good.
    #[test]
    fn a_record_on_file_does_not_make_its_name_taken() {
        let dir = tempdir();
        let name = format!("implement-{}", &task_hash("task-a")[..3]);
        save(
            &alias_store_path(dir.path()),
            &Records {
                entries: vec![AliasRecord {
                    alias: name.clone(),
                    lane: "closed-task · implement".to_string(),
                    pane_id: "w1:p1".to_string(),
                }],
            },
        )
        .unwrap();
        let got = reserve(dir.path(), "task-a · implement", "w1:p2", &HashSet::new()).unwrap();
        assert_eq!(got, name);
    }

    /// Archiving a task removes its records, and only its own.
    #[test]
    fn removing_a_task_drops_only_its_records() {
        let dir = tempdir();
        reserve(dir.path(), "task-a · implement", "w1:p1", &HashSet::new()).unwrap();
        reserve(dir.path(), "task-a · review", "w1:p2", &HashSet::new()).unwrap();
        reserve(dir.path(), "task-b · implement", "w1:p3", &HashSet::new()).unwrap();
        remove_task(dir.path(), &["task-a"]).unwrap();
        assert_eq!(alias_for(dir.path(), "task-a · implement").unwrap(), None);
        assert_eq!(alias_for(dir.path(), "task-a · review").unwrap(), None);
        assert!(
            alias_for(dir.path(), "task-b · implement")
                .unwrap()
                .is_some()
        );
    }

    /// A name with no separator has no step to name an agent after, so it is
    /// refused rather than given a made-up one.
    #[test]
    fn a_name_that_is_not_a_lane_is_refused() {
        let dir = tempdir();
        assert!(reserve(dir.path(), "not-a-lane", "w1:p1", &HashSet::new()).is_err());
    }

    /// A file that does not parse is reported by every reader and left as it
    /// is. Read as empty, it would make every live agent look unknown, and
    /// the next reservation would save over it and lose every other lane's
    /// record.
    #[test]
    fn a_file_that_does_not_parse_is_reported_and_never_overwritten() {
        let dir = tempdir();
        let path = alias_store_path(dir.path());
        std::fs::write(&path, "{ not json").unwrap();

        let err = alias_for(dir.path(), "task-a · implement").unwrap_err();
        assert!(format!("{err:#}").contains("do not parse"), "{err:#}");
        assert!(lane_for(dir.path(), "implement-5f2", "w1:p1").is_err());
        assert!(reserve(dir.path(), "task-a · implement", "w1:p1", &HashSet::new()).is_err());
        assert!(remove_task(dir.path(), &["task-a"]).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    /// A project with no records file yet is a project with no records, not a
    /// broken one.
    #[test]
    fn a_missing_file_reads_as_no_records() {
        let dir = tempdir();
        assert_eq!(alias_for(dir.path(), "task-a · implement").unwrap(), None);
    }

    /// The cross-process lock is what keeps a record safe from another
    /// process, so a reservation must not run while one holds it. The lock
    /// is taken here by this process, and `reserve` waits for it to be
    /// released on another thread before it saves.
    #[test]
    fn a_reservation_waits_for_the_cross_process_lock() {
        let dir = tempdir();
        let held = crate::lock::LaneAliasLock::acquire(&lock_path(dir.path())).unwrap();
        let home = dir.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            reserve(&home, "task-a · implement", "w1:p1", &HashSet::new()).unwrap()
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !alias_store_path(dir.path()).exists(),
            "reserve wrote while the lock was held"
        );
        drop(held);
        worker.join().unwrap();
        assert!(
            alias_for(dir.path(), "task-a · implement")
                .unwrap()
                .is_some()
        );
    }
}
