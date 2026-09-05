//! Reading an agent kind's own cached usage percentage back off disk.
//!
//! Distinct from [`crate::usage`], which reads a *transcript* to price and
//! size one session: this reads a kind's own account-wide figure, written by
//! the agent itself against a clock spoolway has no say in. There is exactly
//! one row today — [`crate::agent::Adapter::quota`] on `claude`, pointing at
//! `~/.claude.json`'s `cachedUsageUtilization` — and every rule here is
//! established against that one file, not guessed at a shape a second kind
//! might one day share.
//!
//! **Fails open, always.** A reading spoolway cannot get, cannot parse, or
//! judges stale never blocks a launch — the pane-phrase hold in `dispatch.rs`
//! is still behind it. [`read`] hands the caller a [`Miss`] instead of an
//! error for exactly that reason: every caller here is a gate that degrades
//! to "off" rather than a command that has anything to refuse.

use anyhow::Result;
use chrono::{DateTime, Utc};

/// One of the two windows Claude Code tracks against its account-wide quota.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    FiveHour,
    SevenDay,
}

impl Window {
    /// The spelling `cachedUsageUtilization` itself uses — what `spoolway
    /// agent verify` prints, so a reader can go compare it against the real
    /// file.
    pub fn key(self) -> &'static str {
        match self {
            Window::FiveHour => "five_hour",
            Window::SevenDay => "seven_day",
        }
    }

    /// The same window, in the words a park message reads out loud.
    pub fn human(self) -> &'static str {
        match self {
            Window::FiveHour => "five-hour",
            Window::SevenDay => "seven-day",
        }
    }
}

/// One window's own reading: how much of it is spent, and when it resets.
#[derive(Clone, Debug)]
pub struct WindowReading {
    pub window: Window,
    pub utilization: u8,
    pub resets_at: DateTime<Utc>,
}

/// How old a reading may be before it is no longer trusted to gate anything.
///
/// Tied to the *faster* of Claude's own two windows rather than an arbitrary
/// figure of its own: a percentage cached longer ago than one five-hour
/// window could already be describing a window that has since reset, with
/// nothing here to say so. Past this the whole reading — both windows — is
/// reported stale rather than acted on; `spoolway agent verify` still prints
/// it, which is where a person finds out this happened.
const STALE_AFTER: chrono::Duration = chrono::Duration::hours(5);

/// A kind's own quota reading, both windows at once. `cachedUsageUtilization`
/// is specified to carry both keys always, so a cache missing either one is
/// not a legal partial reading — it is [`Miss::Unparseable`], the same as a
/// window whose fields do not parse.
pub struct Reading {
    pub fetched_at: DateTime<Utc>,
    pub five_hour: WindowReading,
    pub seven_day: WindowReading,
}

impl Reading {
    /// Whether this reading is too old to act on — see [`STALE_AFTER`].
    pub fn stale(&self, now: DateTime<Utc>) -> bool {
        now.signed_duration_since(self.fetched_at) >= STALE_AFTER
    }

    /// The first window at or above `ceiling`, five-hour checked ahead of
    /// seven-day: it is the one that resets soonest, so a task blocked on it
    /// alone is parked for the shorter of the two waits whenever both are
    /// over.
    ///
    /// A window whose own `resets_at` has already passed is skipped — never
    /// treated as "over" — however high its cached `utilization` reads.
    /// Nothing here rewrites the cache once a window rolls over, so a probe
    /// nobody has run since would otherwise gate every pass on a figure a
    /// fresh window has already invalidated: exactly the case
    /// [`Reading::stale`] cannot catch on its own, since `fetchedAtMs` can
    /// stay recent while one window's clock has already turned over.
    pub fn over_ceiling(&self, ceiling: u8) -> Option<&WindowReading> {
        let now = Utc::now();
        [&self.five_hour, &self.seven_day]
            .into_iter()
            .filter(|w| w.resets_at > now)
            .find(|w| w.utilization >= ceiling)
    }
}

/// Why a reading could not be produced, for a caller that wants to say so —
/// `spoolway agent verify` names all three; a gate in `dispatch.rs` just
/// treats every one of them as "nothing to act on".
pub enum Miss {
    /// This kind carries no [`crate::agent::Adapter::quota`] row at all.
    NoProbe,
    /// The file could not be opened or read.
    Unreadable(String),
    /// The file was read but did not parse the way this reader expects.
    Unparseable(String),
}

/// Read `kind`'s own quota probe, if it has one — see [`crate::agent::Adapter::quota`].
pub fn read(kind: &str) -> std::result::Result<Reading, Miss> {
    let adapter = crate::agent::adapter(kind).ok_or(Miss::NoProbe)?;
    let rel = adapter.quota.ok_or(Miss::NoProbe)?;
    let home = crate::platform::home_dir()
        .ok_or_else(|| Miss::Unreadable("no home directory to resolve it under".into()))?;
    let path = home.join(rel);
    let text = std::fs::read_to_string(&path)
        .map_err(|err| Miss::Unreadable(format!("{}: {err}", path.display())))?;
    parse(&text).map_err(|err| Miss::Unparseable(format!("{}: {err}", path.display())))
}

fn parse(text: &str) -> Result<Reading> {
    let root: serde_json::Value = serde_json::from_str(text)?;
    let cached = root
        .get("cachedUsageUtilization")
        .ok_or_else(|| anyhow::anyhow!("no `cachedUsageUtilization` key"))?;
    let fetched_at = cached
        .get("fetchedAtMs")
        .and_then(|v| v.as_i64())
        .and_then(DateTime::from_timestamp_millis)
        .ok_or_else(|| anyhow::anyhow!("no readable `fetchedAtMs`"))?;
    Ok(Reading {
        fetched_at,
        five_hour: window(cached, "five_hour", Window::FiveHour)?,
        seven_day: window(cached, "seven_day", Window::SevenDay)?,
    })
}

/// One window out of `cachedUsageUtilization` — `five_hour` or `seven_day`,
/// both of which the cache is specified to always carry. A key that is
/// missing outright is not a legal partial reading any more than one that
/// is present but does not parse — `utilization` missing, not a number, or
/// over 100; `resets_at` missing or not an RFC3339 timestamp — both are
/// [`Miss::Unparseable`]'s case, the cache being one this reader does not
/// understand. Collapsing a missing key to `None` used to mean a cache
/// missing `five_hour` outright still returned `Ok` with that window simply
/// unavailable, so `spoolway agent verify` printed "unavailable" for a
/// window the spec says is always there, and a gate reading it saw an
/// absent window rather than a reason to distrust the whole file.
fn window(cached: &serde_json::Value, key: &str, window: Window) -> Result<WindowReading> {
    let raw = cached
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("`{key}` is missing"))?;
    let utilization = raw
        .get("utilization")
        .and_then(|v| v.as_u64())
        .filter(|u| *u <= 100)
        .ok_or_else(|| anyhow::anyhow!("`{key}.utilization` is missing or not 0..=100"))?
        as u8;
    let resets_at = raw
        .get("resets_at")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("`{key}.resets_at` is missing"))?;
    let resets_at = DateTime::parse_from_rfc3339(resets_at)
        .map_err(|err| anyhow::anyhow!("`{key}.resets_at` does not parse: {err}"))?
        .with_timezone(&Utc);
    Ok(WindowReading {
        window,
        utilization,
        resets_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_home::with_home;

    fn write_claude_json(home: &std::path::Path, body: &str) {
        std::fs::write(home.join(".claude.json"), body).unwrap();
    }

    #[test]
    fn a_kind_with_no_probe_row_is_no_probe() {
        assert!(matches!(read("pi"), Err(Miss::NoProbe)));
    }

    #[test]
    fn an_unknown_kind_is_no_probe() {
        assert!(matches!(read("nope"), Err(Miss::NoProbe)));
    }

    #[test]
    fn a_missing_file_is_unreadable() {
        let home = crate::scratch::root("quota-missing-file");
        std::fs::create_dir_all(&home).unwrap();
        with_home(&home, || {
            assert!(matches!(read("claude"), Err(Miss::Unreadable(_))));
        });
    }

    #[test]
    fn a_malformed_file_is_unparseable() {
        let home = crate::scratch::root("quota-malformed-file");
        std::fs::create_dir_all(&home).unwrap();
        write_claude_json(&home, "not json at all");
        with_home(&home, || {
            assert!(matches!(read("claude"), Err(Miss::Unparseable(_))));
        });
    }

    #[test]
    fn a_file_with_no_cached_usage_key_is_unparseable() {
        let home = crate::scratch::root("quota-no-key");
        std::fs::create_dir_all(&home).unwrap();
        write_claude_json(&home, "{}");
        with_home(&home, || {
            assert!(matches!(read("claude"), Err(Miss::Unparseable(_))));
        });
    }

    /// `cachedUsageUtilization` is specified to always carry both windows —
    /// a cache missing one outright is not a legal partial reading, the
    /// same as one whose fields do not parse, which the next few tests pin.
    #[test]
    fn a_window_missing_outright_is_unparseable_not_absent() {
        let home = crate::scratch::root("quota-window-absent");
        std::fs::create_dir_all(&home).unwrap();
        let fetched_at = Utc::now().timestamp_millis();
        write_claude_json(
            &home,
            &format!(
                r#"{{"cachedUsageUtilization": {{
                    "fetchedAtMs": {fetched_at},
                    "five_hour": {{"utilization": 40, "resets_at": "2099-01-01T00:00:00Z"}}
                }}}}"#
            ),
        );
        with_home(&home, || {
            assert!(matches!(read("claude"), Err(Miss::Unparseable(_))));
        });
    }

    /// A window present but missing its own `utilization` is a parse
    /// failure of the whole file, not a window silently read as absent —
    /// the cache exists and this reader does not understand it.
    #[test]
    fn a_window_present_with_no_utilization_is_unparseable() {
        let home = crate::scratch::root("quota-window-no-utilization");
        std::fs::create_dir_all(&home).unwrap();
        let fetched_at = Utc::now().timestamp_millis();
        write_claude_json(
            &home,
            &format!(
                r#"{{"cachedUsageUtilization": {{
                    "fetchedAtMs": {fetched_at},
                    "five_hour": {{"resets_at": "2099-01-01T00:00:00Z"}},
                    "seven_day": {{"utilization": 10, "resets_at": "2099-01-01T00:00:00Z"}}
                }}}}"#
            ),
        );
        with_home(&home, || {
            assert!(matches!(read("claude"), Err(Miss::Unparseable(_))));
        });
    }

    /// A `utilization` over 100 used to be silently clamped to 100 rather
    /// than treated as the malformed cache it is.
    #[test]
    fn a_utilization_over_100_is_unparseable_not_clamped() {
        let home = crate::scratch::root("quota-utilization-over-100");
        std::fs::create_dir_all(&home).unwrap();
        let fetched_at = Utc::now().timestamp_millis();
        write_claude_json(
            &home,
            &format!(
                r#"{{"cachedUsageUtilization": {{
                    "fetchedAtMs": {fetched_at},
                    "five_hour": {{"utilization": 250, "resets_at": "2099-01-01T00:00:00Z"}},
                    "seven_day": {{"utilization": 10, "resets_at": "2099-01-01T00:00:00Z"}}
                }}}}"#
            ),
        );
        with_home(&home, || {
            assert!(matches!(read("claude"), Err(Miss::Unparseable(_))));
        });
    }

    /// A `resets_at` that is not a real timestamp is the same kind of
    /// failure as a missing `utilization` — the whole file is unparseable.
    #[test]
    fn a_window_present_with_a_bad_resets_at_is_unparseable() {
        let home = crate::scratch::root("quota-window-bad-resets-at");
        std::fs::create_dir_all(&home).unwrap();
        let fetched_at = Utc::now().timestamp_millis();
        write_claude_json(
            &home,
            &format!(
                r#"{{"cachedUsageUtilization": {{
                    "fetchedAtMs": {fetched_at},
                    "five_hour": {{"utilization": 40, "resets_at": "not a timestamp"}},
                    "seven_day": {{"utilization": 10, "resets_at": "2099-01-01T00:00:00Z"}}
                }}}}"#
            ),
        );
        with_home(&home, || {
            assert!(matches!(read("claude"), Err(Miss::Unparseable(_))));
        });
    }

    #[test]
    fn a_real_shaped_reading_parses_both_windows() {
        let home = crate::scratch::root("quota-real-shape");
        std::fs::create_dir_all(&home).unwrap();
        write_claude_json(
            &home,
            r#"{"cachedUsageUtilization": {
                "fetchedAtMs": 1700000000000,
                "five_hour": {"utilization": 61, "resets_at": "2023-11-15T00:00:00Z"},
                "seven_day": {"utilization": 16, "resets_at": "2023-11-20T00:00:00Z"}
            }}"#,
        );
        with_home(&home, || {
            let reading = read("claude").unwrap_or_else(|_| panic!("expected a reading"));
            assert_eq!(reading.five_hour.utilization, 61);
            assert_eq!(reading.seven_day.utilization, 16);
        });
    }

    #[test]
    fn a_reading_fetched_long_ago_is_stale() {
        let home = crate::scratch::root("quota-stale");
        std::fs::create_dir_all(&home).unwrap();
        // Six hours ago — past the five-hour window this is judged against.
        let fetched_at = (Utc::now() - chrono::Duration::hours(6)).timestamp_millis();
        write_claude_json(
            &home,
            &format!(
                r#"{{"cachedUsageUtilization": {{
                    "fetchedAtMs": {fetched_at},
                    "five_hour": {{"utilization": 90, "resets_at": "2099-01-01T00:00:00Z"}},
                    "seven_day": {{"utilization": 90, "resets_at": "2099-01-01T00:00:00Z"}}
                }}}}"#
            ),
        );
        with_home(&home, || {
            let reading = read("claude").unwrap_or_else(|_| panic!("expected a reading"));
            assert!(reading.stale(Utc::now()));
        });
    }

    #[test]
    fn a_fresh_reading_under_ceiling_never_fires() {
        let home = crate::scratch::root("quota-under-ceiling");
        std::fs::create_dir_all(&home).unwrap();
        let fetched_at = Utc::now().timestamp_millis();
        write_claude_json(
            &home,
            &format!(
                r#"{{"cachedUsageUtilization": {{
                    "fetchedAtMs": {fetched_at},
                    "five_hour": {{"utilization": 40, "resets_at": "2099-01-01T00:00:00Z"}},
                    "seven_day": {{"utilization": 10, "resets_at": "2099-01-01T00:00:00Z"}}
                }}}}"#
            ),
        );
        with_home(&home, || {
            let reading = read("claude").unwrap_or_else(|_| panic!("expected a reading"));
            assert!(reading.over_ceiling(85).is_none());
        });
    }

    #[test]
    fn over_ceiling_prefers_the_five_hour_window() {
        let home = crate::scratch::root("quota-five-hour-first");
        std::fs::create_dir_all(&home).unwrap();
        let fetched_at = Utc::now().timestamp_millis();
        write_claude_json(
            &home,
            &format!(
                r#"{{"cachedUsageUtilization": {{
                    "fetchedAtMs": {fetched_at},
                    "five_hour": {{"utilization": 88, "resets_at": "2099-01-01T00:00:00Z"}},
                    "seven_day": {{"utilization": 97, "resets_at": "2099-02-01T00:00:00Z"}}
                }}}}"#
            ),
        );
        with_home(&home, || {
            let reading = read("claude").unwrap_or_else(|_| panic!("expected a reading"));
            let hit = reading.over_ceiling(85).expect("over ceiling");
            assert_eq!(hit.window, Window::FiveHour);
        });
    }

    /// The bug this test pins: a window whose own clock has already turned
    /// over must never gate a launch on the figure cached before it did,
    /// however fresh `fetchedAtMs` still reads — or a probe nobody has run
    /// since the reset would park every pass forever on a number a fresh
    /// window has already invalidated.
    #[test]
    fn a_window_whose_own_reset_has_passed_never_reads_as_over_ceiling() {
        let home = crate::scratch::root("quota-window-already-reset");
        std::fs::create_dir_all(&home).unwrap();
        let fetched_at = Utc::now().timestamp_millis();
        let already_past = (Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
        write_claude_json(
            &home,
            &format!(
                r#"{{"cachedUsageUtilization": {{
                    "fetchedAtMs": {fetched_at},
                    "five_hour": {{"utilization": 97, "resets_at": "{already_past}"}},
                    "seven_day": {{"utilization": 10, "resets_at": "2099-01-01T00:00:00Z"}}
                }}}}"#
            ),
        );
        with_home(&home, || {
            let reading = read("claude").unwrap_or_else(|_| panic!("expected a reading"));
            assert!(
                !reading.stale(Utc::now()),
                "fetchedAtMs is fresh — this is not the whole-reading staleness case"
            );
            assert!(
                reading.over_ceiling(85).is_none(),
                "the five-hour window's own reset has passed, so its cached 97% must not gate"
            );
        });
    }
}
