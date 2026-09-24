//! A durable record mapping a lane's short Herdr wire alias back to its full
//! spoolway name — `<task> · <step>` — for a lane too long to spell out on
//! the wire the reversible way [`crate::mux::to_agent_name`] does.
//!
//! Herdr refuses an agent name over 32 characters outright, and a task id is
//! now free to make a lane's own name run well past that (see gh-359). When
//! it does, [`crate::mux::Herdr::start_lane`] mints a short opaque alias
//! instead of the readable wire spelling, and [`reserve`] is what writes it
//! down — atomically, and before the launch it names ever reaches herdr — so
//! a dispatcher that dies between the two, or right after them, still finds
//! its way back from the alias to the lane on its next restart. A lane whose
//! reversible spelling already fits gets no record at all: it keeps crossing
//! the wire the old way, and an already-live session started before this
//! existed is read back exactly as it always was.
//!
//! One file per project, beside its other lane bookkeeping (`lanes.json`) —
//! see [`crate::repo::Repo::home`] — never per task, so a restart has exactly
//! one place to read every alias this project has ever handed out from.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The file every alias this project has reserved is recorded in.
const FILE_NAME: &str = "lane_aliases.json";

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

/// Read every record a project has on file. Never fails: a file that does not
/// exist yet (no lane has ever needed an alias) or that does not parse (an
/// interrupted write from a version that wrote a different shape) both read
/// back as no records at all — the same "start from nothing" a corrupt
/// `lanes.json` already falls back to, and for the same reason: refusing to
/// read here would strand every restart behind a file this module itself can
/// always rebuild from the next reservation.
fn load(path: &Path) -> Records {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save(path: &Path, records: &Records) -> Result<()> {
    let json = serde_json::to_string_pretty(records).context("encoding lane alias records")?;
    // The same temp-file-then-rename `write_atomic` every other piece of
    // project state goes through — see `Repo::lanes_file`'s own writer — so a
    // crash mid-write leaves the previous, still-consistent file in place
    // rather than a truncated one for the very next restart to choke on.
    crate::task::write_atomic(path, json)
}

/// A candidate alias for `lane`, herdr-legal and deterministic in `lane` and
/// `attempt` alone — nothing here needs to survive a restart on its own; only
/// the record [`reserve`] then writes down does, so a collision just asks
/// again with the next `attempt`.
fn candidate_alias(lane: &str, attempt: u32) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    lane.hash(&mut hasher);
    attempt.hash(&mut hasher);
    // `l` for "lane": herdr requires a lowercase letter first, and a hash can
    // just as easily start with a digit.
    format!("l{:x}", hasher.finish())
}

/// Reserve a short wire alias for `lane`, about to be started in `pane_id`,
/// and write it down before the caller ever tells herdr to launch anything —
/// the ordering the restart check in [`lane_for`] depends on: a crash right
/// after this call still has a record naming the pane the agent is about to
/// occupy, and a crash before it leaves nothing for a later `agent list` to
/// misread as this lane at all.
///
/// `taken` is every wire name herdr already answers to, read fresh from a
/// live `agent list` immediately before this call — the one source that can
/// catch a collision with a session this project's own records know nothing
/// about, a human's own scratch session included.
///
/// Any record already on file for `lane` is replaced: a lane restarted on a
/// new pane, after its old one closed, asks for a fresh alias rather than
/// keeping one that named a pane it no longer occupies.
pub fn reserve(
    project_home: &Path,
    lane: &str,
    pane_id: &str,
    taken: &HashSet<String>,
) -> Result<String> {
    let path = alias_store_path(project_home);
    let mut records = load(&path);
    records.entries.retain(|r| r.lane != lane);

    let mut attempt = 0u32;
    let alias = loop {
        let candidate = candidate_alias(lane, attempt);
        let collides =
            taken.contains(&candidate) || records.entries.iter().any(|r| r.alias == candidate);
        if !collides {
            break candidate;
        }
        attempt += 1;
    };

    records.entries.push(AliasRecord {
        alias: alias.clone(),
        lane: lane.to_string(),
        pane_id: pane_id.to_string(),
    });
    save(&path, &records)?;
    Ok(alias)
}

/// The alias already on record for `lane`, if [`reserve`] was ever called for
/// it — what every wire call past the launch itself (`prompt`, `read`,
/// `interrupt_lane`, `focus_lane`) resolves a long lane's name through,
/// rather than trying to spell it out on the wire a second time.
pub fn alias_for(project_home: &Path, lane: &str) -> Option<String> {
    load(&alias_store_path(project_home))
        .entries
        .into_iter()
        .find(|r| r.lane == lane)
        .map(|r| r.alias)
}

/// The full lane name behind `alias`, if its record's pane still matches
/// `pane_id` — the restart check itself. An alias this project never
/// reserved, or one whose record now points at a different pane, answers
/// `None` rather than a guess: [`crate::mux::Herdr::list_lanes`] then leaves
/// the wire name to [`crate::mux::from_agent_name`]'s own reversible
/// decoding instead, which is exactly what a session herdr did not get this
/// alias from — someone else's scratch session, or a stale record's pane
/// reused by an unrelated launch — needs: never adopted as one of ours, and
/// never mistaken for a duplicate of the lane it used to name.
pub fn lane_for(project_home: &Path, alias: &str, pane_id: &str) -> Option<String> {
    load(&alias_store_path(project_home))
        .entries
        .into_iter()
        .find(|r| r.alias == alias && r.pane_id == pane_id)
        .map(|r| r.lane)
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
        assert_eq!(alias_for(dir.path(), lane), Some(alias.clone()));
        assert_eq!(
            lane_for(dir.path(), &alias, "w1:p1"),
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
        assert_eq!(lane_for(dir.path(), &alias, "w1:p9"), None);
    }

    /// An alias nothing ever reserved is unknown, not a guess.
    #[test]
    fn an_unrecorded_alias_resolves_to_nothing() {
        let dir = tempdir();
        assert_eq!(lane_for(dir.path(), "lnotreal", "w1:p1"), None);
    }

    #[test]
    fn reserving_again_for_the_same_lane_drops_its_old_record() {
        let dir = tempdir();
        let lane = "release-spoolway-2 · merge-released-repair";
        let first = reserve(dir.path(), lane, "w1:p1", &HashSet::new()).unwrap();
        let second = reserve(dir.path(), lane, "w1:p2", &HashSet::new()).unwrap();
        // The old pane no longer answers for this lane at all — not even to
        // refuse it as mismatched — because the record itself is gone.
        assert_eq!(lane_for(dir.path(), &first, "w1:p1"), None);
        assert_eq!(
            lane_for(dir.path(), &second, "w1:p2"),
            Some(lane.to_string())
        );
    }

    /// A candidate that collides with a name already live must never be
    /// handed out — that agent is somebody else's, or this project's own
    /// session under a different lane, and either way herdr would refuse a
    /// second `agent start` under the same name.
    #[test]
    fn a_collision_with_a_live_name_is_never_reserved() {
        let dir = tempdir();
        let lane = "release-spoolway-2 · merge-released-repair";
        let clash = candidate_alias(lane, 0);
        let taken: HashSet<String> = [clash.clone()].into_iter().collect();
        let alias = reserve(dir.path(), lane, "w1:p1", &taken).unwrap();
        assert_ne!(alias, clash);
    }
}
