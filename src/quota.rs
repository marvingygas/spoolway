//! Reading an agent kind's own cached usage percentage back off disk.
//!
//! Distinct from [`crate::usage`], which reads a *transcript* to price and
//! size one session: this reads a kind's own account-wide figure, written by
//! the agent itself against a clock spoolway has no say in. Two rows carry
//! one today — [`crate::agent::Adapter::quota`] on `claude` and `codex` —
//! and every rule here is established against a real file or rollout, not
//! guessed at a shape neither has been seen to carry.
//!
//! `claude` caches its figure in one file, `~/.claude.json`. `codex` writes
//! no such cache — it reports its account's rate limits per session, beside
//! every turn's token count, in the same rollout [`crate::usage`] already
//! reads for cost. So `read` resolves two different ways of finding the same
//! kind of number, branching on [`crate::agent::Accounting::format`] rather
//! than growing a second entry point: every caller here already asks for a
//! kind's reading without caring which shape answered it, and that has to
//! stay true of a second kind's reading too.
//!
//! Reading failures are explicit. An enabled dispatcher ceiling holds new
//! launches until a trustworthy reading exists; disabled ceilings read nothing.

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
/// `spoolway agent verify` names each case. An enabled dispatcher ceiling
/// holds new launches for all of them.
pub enum Miss {
    /// This kind carries no [`crate::agent::Adapter::quota`] row at all.
    NoProbe,
    /// The file could not be opened or read.
    Unreadable(String),
    /// The file was read but did not parse the way this reader expects.
    Unparseable(String),
    /// The file parsed, and understood cleanly, into a reading that carries
    /// nothing to act on — codex's own shape for "not signed in with
    /// ChatGPT": `rate_limits` present with `primary` and `secondary` both
    /// null, on a session settled against a local endpoint or a bare API
    /// key. `limit_id` stays populated even then (`"codex"` on the real
    /// reading this was checked against) — it is the two windows that go
    /// null, not the object as a whole. Distinct from [`Miss::Unparseable`]
    /// because nothing here is malformed; there is simply no percentage
    /// behind it yet.
    NoReading(String),
}

/// Read `kind`'s own quota probe, if it has one — see [`crate::agent::Adapter::quota`].
pub fn read(kind: &str) -> std::result::Result<Reading, Miss> {
    let adapter = crate::agent::adapter(kind).ok_or(Miss::NoProbe)?;
    let rel = adapter.quota.ok_or(Miss::NoProbe)?;
    match adapter.accounting.as_ref().map(|a| a.format) {
        Some(crate::usage::Format::Codex) => read_codex(kind, rel),
        _ => read_claude_cache(rel),
    }
}

/// A reading safe to use for admission. Expired windows require another
/// observation: the cached percentage says nothing about their new usage.
pub fn trusted(kind: &str) -> std::result::Result<Reading, String> {
    let reading = read(kind).map_err(|miss| match miss {
        Miss::NoProbe => "no quota probe".to_string(),
        Miss::Unreadable(why) | Miss::Unparseable(why) | Miss::NoReading(why) => why,
    })?;
    let now = Utc::now();
    if reading.stale(now) {
        return Err(format!(
            "quota reading is {} minutes old",
            (now - reading.fetched_at).num_minutes()
        ));
    }
    if [&reading.five_hour, &reading.seven_day]
        .iter()
        .any(|w| w.resets_at <= now)
    {
        return Err("quota window has reset; a fresh reading is required".into());
    }
    Ok(reading)
}

/// `claude`'s own shape: one file, relative to the home directory, holding
/// its whole reading — see [`crate::agent::Adapter::quota`]'s own doc.
fn read_claude_cache(rel: &str) -> std::result::Result<Reading, Miss> {
    let home = crate::platform::home_dir()
        .ok_or_else(|| Miss::Unreadable("no home directory to resolve it under".into()))?;
    let path = home.join(rel);
    let text = std::fs::read_to_string(&path)
        .map_err(|err| Miss::Unreadable(format!("{}: {err}", path.display())))?;
    parse(&text).map_err(|err| Miss::Unparseable(format!("{}: {err}", path.display())))
}

/// codex's own shape: no cache file, so `rel` (`"sessions"`) is a directory
/// under a lane's own home rather than a file — the newest rollout in it
/// carries the freshest reading, exactly as `~/.claude.json`'s single file
/// does for claude, just spread across many files instead of one.
///
/// The home spoolway made for a lane, never `~/.codex` — the task this row
/// was established for says so directly, and the reason is trust as much as
/// correctness: a rollout under a lane's own `$CODEX_HOME` is one this
/// binary itself started and can vouch for, where an interactive session or
/// a run against a local endpoint is neither. Reading `own_home(kind)` too
/// would let whichever of the two happened to write more recently override
/// the other silently — a person testing a local model minutes before a
/// dispatcher pass would suppress the account's real reading with a
/// same-account run this binary never started.
fn read_codex(kind: &str, rel: &str) -> std::result::Result<Reading, Miss> {
    let path = newest_managed_codex_rollout(kind, rel).ok_or_else(|| {
        Miss::Unreadable(
            "no rollout found under any managed lane home for this kind — no codex lane has \
             written one here yet"
                .into(),
        )
    })?;
    let text = std::fs::read_to_string(&path)
        .map_err(|err| Miss::Unreadable(format!("{}: {err}", path.display())))?;
    match parse_codex(&text) {
        Ok(Some(reading)) => Ok(reading),
        Ok(None) => Err(Miss::NoReading(format!(
            "{}: rate_limits present with both windows null — not signed in with ChatGPT",
            path.display()
        ))),
        Err(err) => Err(Miss::Unparseable(format!("{}: {err}", path.display()))),
    }
}

/// The newest rollout under any per-lane home spoolway has made for `kind` —
/// see [`read_codex`]'s own doc for why `own_home` is deliberately not among
/// the places this looks.
///
/// `state_root().join(home.dir)` (`<state_root>/codex`) holds one directory
/// per session id, each a full `$CODEX_HOME` of its own — including its own
/// `.tmp`, where codex keeps a plugin fixture `.jsonl` that is not a rollout
/// (see [`crate::agent::Accounting::store`]'s own doc). So each session's
/// directory is walked at `rel` specifically, the same restriction a
/// [`crate::usage::FileShape::OwnHome`] lookup already applies for one known
/// session, rather than handed to [`crate::usage::newest_transcript`] whole
/// and risking exactly that fixture file winning.
fn newest_managed_codex_rollout(kind: &str, rel: &str) -> Option<std::path::PathBuf> {
    let sessions_root =
        crate::usage::state_root()?.join(crate::agent::adapter(kind)?.home.as_ref()?.dir);
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(&sessions_root).ok()?.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(path) = crate::usage::newest_transcript(&entry.path().join(rel)) else {
            continue;
        };
        let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(at, _)| modified > *at) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
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
        five_hour: window(
            cached.get("utilization").unwrap_or(cached),
            "five_hour",
            Window::FiveHour,
        )?,
        seven_day: window(
            cached.get("utilization").unwrap_or(cached),
            "seven_day",
            Window::SevenDay,
        )?,
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

/// codex's own reading, out of one rollout's text — the last `token_count`
/// event's `rate_limits`.
///
/// `Ok(None)` for the shape settled against a local endpoint or a bare API
/// key: `rate_limits` present with both `primary` and `secondary` null (and,
/// on the real reading this was checked against, every optional field
/// beside them null too — `limit_id` alone stays populated, `"codex"` there
/// as on a real reading). That is not a file this reader fails to
/// understand — it understands it perfectly, and what it understands is
/// that there is nothing to report yet, which is exactly
/// [`Miss::NoReading`]'s case at the caller. A reading with only one of the
/// two null is a shape nobody has seen — `Err`, not `Ok(None)` — so a real
/// breach on the window that did parse can never be swallowed by a
/// malformed sibling.
fn parse_codex(text: &str) -> Result<Option<Reading>> {
    // Last rather than first: a rollout carries one `token_count` per turn,
    // and the account's rate limit as of the *latest* turn is the one worth
    // acting on — the same "newest wins" rule `newest_transcript` already
    // applies across files, continued within one.
    let record = text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .rfind(|value| {
            value.get("type").and_then(|t| t.as_str()) == Some("event_msg")
                && value
                    .get("payload")
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("token_count")
        })
        .ok_or_else(|| anyhow::anyhow!("no `token_count` event in this rollout"))?;

    let rate_limits = record
        .get("payload")
        .and_then(|p| p.get("rate_limits"))
        .ok_or_else(|| anyhow::anyhow!("no `rate_limits` on the last `token_count` event"))?;

    // The negative case, and only this exact shape of it: settled against a
    // local endpoint or an API key, *both* `primary` and `secondary` are
    // null — the shape actually seen on this machine (see the comment on
    // codex's own row in `agent.rs`). A window null on one side and not the
    // other, or `primary` missing outright rather than present-and-null, is
    // not that shape — it is a rollout this reader does not understand, so
    // it falls through to `codex_window` below and comes back `Unparseable`
    // rather than being read as though nothing were there. Silently treating
    // any half-null reading as "nothing to report" would let a real ceiling
    // breach on one window go unnoticed because the other happened to be
    // malformed.
    let primary_null = rate_limits.get("primary").is_some_and(|v| v.is_null());
    let secondary_null = rate_limits.get("secondary").is_some_and(|v| v.is_null());
    if primary_null && secondary_null {
        return Ok(None);
    }
    if primary_null != secondary_null {
        anyhow::bail!(
            "`rate_limits` has one window null and the other not — not the established \
             all-null or all-present shape"
        );
    }

    let fetched_at = record
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .ok_or_else(|| anyhow::anyhow!("no readable `timestamp` on the `token_count` event"))?;

    Ok(Some(Reading {
        fetched_at,
        five_hour: codex_window(rate_limits, "primary", Window::FiveHour)?,
        seven_day: codex_window(rate_limits, "secondary", Window::SevenDay)?,
    }))
}

/// One window out of codex's `rate_limits` — `primary` or `secondary`. Same
/// no-partial-reading rule as [`window`]: once `primary` is known non-null
/// (checked by the caller before either window is read), a window missing or
/// malformed here is the rollout carrying a shape this reader does not
/// understand, not an absent window.
///
/// `used_percent` is a float on the wire and `resets_at` an epoch-seconds
/// integer, not the RFC3339 string claude's cache carries — codex's own
/// shapes, read verbatim off the real reading quoted on codex's row in
/// `agent.rs`.
fn codex_window(
    rate_limits: &serde_json::Value,
    key: &str,
    window: Window,
) -> Result<WindowReading> {
    let raw = rate_limits
        .get(key)
        .filter(|v| !v.is_null())
        .ok_or_else(|| anyhow::anyhow!("`{key}` is missing"))?;
    let used_percent = raw
        .get("used_percent")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow::anyhow!("`{key}.used_percent` is missing or not a number"))?;
    if !(0.0..=100.0).contains(&used_percent) {
        anyhow::bail!("`{key}.used_percent` out of range: {used_percent}");
    }
    let utilization = used_percent.round() as u8;
    let resets_at = raw
        .get("resets_at")
        .and_then(|v| v.as_i64())
        .and_then(|secs| DateTime::from_timestamp(secs, 0))
        .ok_or_else(|| anyhow::anyhow!("`{key}.resets_at` is missing or not a Unix timestamp"))?;
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
    fn trusted_quota_rejects_stale_expired_and_malformed_readings() {
        let home = crate::scratch::root("quota-trust");
        std::fs::create_dir_all(&home).unwrap();
        with_home(&home, || {
            for (fetched, reset, reason) in [
                (
                    (Utc::now() - chrono::Duration::hours(6)).timestamp_millis(),
                    "2099-01-01T00:00:00Z",
                    "minutes old",
                ),
                (
                    Utc::now().timestamp_millis(),
                    "2020-01-01T00:00:00Z",
                    "has reset",
                ),
            ] {
                write_claude_json(
                    &home,
                    &format!(
                        r#"{{"cachedUsageUtilization":{{"fetchedAtMs":{fetched},
                    "five_hour":{{"utilization":10,"resets_at":"{reset}"}},
                    "seven_day":{{"utilization":10,"resets_at":"2099-01-08T00:00:00Z"}}
                }}}}"#
                    ),
                );
                assert!(trusted("claude").err().unwrap().contains(reason));
            }
            write_claude_json(&home, "{}");
            assert!(trusted("claude").is_err());
        });
    }

    #[test]
    fn nested_claude_cache_matches_the_observed_shape() {
        let text = r#"{"cachedUsageUtilization":{"fetchedAtMs":1788636884908,
            "utilization":{"five_hour":{"utilization":78,"resets_at":"2026-09-05T22:39:59Z"},
            "seven_day":{"utilization":47,"resets_at":"2026-09-11T03:59:59Z"},
            "seven_day_opus":null}}}"#;
        let reading = parse(text).unwrap();
        assert_eq!(reading.five_hour.utilization, 78);
        assert_eq!(reading.seven_day.utilization, 47);
        assert_eq!(reading.fetched_at.timestamp_millis(), 1788636884908);
        let malformed = text.replace("\"utilization\":78", "\"utilization\":null");
        assert!(parse(&malformed).is_err());
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

    // ------------------------------------------------------------- codex

    /// A `token_count` event's line, shaped like the real reading quoted on
    /// codex's own row in `agent.rs` — `used_percent` as a float, `resets_at`
    /// as epoch seconds. A rollout is one record per physical line —
    /// `parse_codex` reads it that way — so the multi-line `rate_limits`
    /// constants below (kept readable, not written that way for real) have
    /// their embedded newlines folded out before this returns.
    fn codex_token_count_line(timestamp: &str, rate_limits: &str) -> String {
        format!(
            r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":1,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":2}}}},"rate_limits":{rate_limits}}}}}"#
        )
        .replace('\n', "")
    }

    /// A rollout under the managed lane home `newest_managed_codex_rollout`
    /// actually reads — `<home>/.local/state/spoolway/codex/<session>/sessions/**`
    /// — the only place a real codex lane writes one. `session` need not be a
    /// real id; it only has to be some directory under `codex`, the same as
    /// a real per-lane home would be.
    fn write_managed_codex_rollout(
        home: &std::path::Path,
        session: &str,
        timestamp: &str,
        rate_limits: &str,
    ) {
        let dir = home
            .join(".local/state/spoolway/codex")
            .join(session)
            .join("sessions/2026/09/05");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rollout-fixture.jsonl"),
            codex_token_count_line(timestamp, rate_limits),
        )
        .unwrap();
    }

    /// A rollout under this kind's own home — `~/.codex` under a fixture's
    /// `$HOME` — the shape an interactive session or a run against a local
    /// endpoint writes, and never a place [`read_codex`] looks.
    fn write_interactive_codex_rollout(home: &std::path::Path, timestamp: &str, rate_limits: &str) {
        let dir = home.join(".codex/sessions/2026/09/05");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rollout-fixture.jsonl"),
            codex_token_count_line(timestamp, rate_limits),
        )
        .unwrap();
    }

    const REAL_SHAPED_RATE_LIMITS: &str = r#"{"limit_id":"codex","limit_name":null,
        "primary":{"used_percent":5.0,"window_minutes":300,"resets_at":1788611977},
        "secondary":{"used_percent":2.0,"window_minutes":10080,"resets_at":1789151593},
        "credits":{"has_credits":false,"unlimited":false,"balance":"0"},
        "individual_limit":null,"spend_control_reached":null,"plan_type":"plus",
        "rate_limit_reached_type":null}"#;

    const NULL_RATE_LIMITS: &str = r#"{"limit_id":"codex","limit_name":null,"primary":null,
        "secondary":null,"credits":null,"individual_limit":null,
        "spend_control_reached":null,"plan_type":null,"rate_limit_reached_type":null}"#;

    /// The real-shaped positive case — see the comment on codex's row in
    /// `agent.rs`, which this reading is copied from verbatim. The
    /// `timestamp` itself is `Utc::now()` rather than the real moment it was
    /// captured at, so this stays fresh (not [`Reading::stale`]) however long
    /// after that capture the suite happens to run.
    #[test]
    fn a_real_shaped_codex_rollout_parses_both_windows() {
        let home = crate::scratch::root("quota-codex-real-shape");
        std::fs::create_dir_all(&home).unwrap();
        write_managed_codex_rollout(
            &home,
            "fixture-session",
            &Utc::now().to_rfc3339(),
            REAL_SHAPED_RATE_LIMITS,
        );
        with_home(&home, || {
            let reading = read("codex").unwrap_or_else(|_| panic!("expected a reading"));
            assert_eq!(reading.five_hour.utilization, 5);
            assert_eq!(reading.seven_day.utilization, 2);
            assert_eq!(reading.five_hour.resets_at.timestamp(), 1788611977);
            assert_eq!(reading.seven_day.resets_at.timestamp(), 1789151593);
        });
    }

    /// The negative case this reader exists to refuse rather than misreport:
    /// a session settled against a local endpoint or a bare API key writes
    /// `rate_limits` with `primary` and `secondary` both null (`limit_id`
    /// stays `"codex"`, same as a real reading). That is a reading spoolway
    /// understands, not one it fails to parse — `Miss::NoReading`, and per
    /// the module's own fail-open rule, never a block.
    #[test]
    fn a_codex_session_with_null_rate_limits_is_no_reading_not_unparseable() {
        let home = crate::scratch::root("quota-codex-null");
        std::fs::create_dir_all(&home).unwrap();
        write_managed_codex_rollout(
            &home,
            "fixture-session",
            "2026-09-05T07:51:22.019Z",
            NULL_RATE_LIMITS,
        );
        with_home(&home, || {
            assert!(matches!(read("codex"), Err(Miss::NoReading(_))));
        });
    }

    /// The newest rollout across every managed lane home wins, the same
    /// "newest wins" rule `crate::usage::newest_transcript` already applies
    /// for a session's own transcript — codex writes one file per session,
    /// not one cache, and each session gets its own directory under
    /// `codex`, so this is what stands in for `fetchedAtMs` deciding between
    /// two files.
    #[test]
    fn the_newest_managed_rollout_is_the_one_read() {
        let home = crate::scratch::root("quota-codex-newest");
        std::fs::create_dir_all(&home).unwrap();
        write_managed_codex_rollout(
            &home,
            "older-session",
            "2026-09-05T06:00:00Z",
            REAL_SHAPED_RATE_LIMITS,
        );
        // A gap wide enough to survive a filesystem's own mtime resolution
        // under load — otherwise two writes in quick succession can land on
        // the same tick and leave "newest" undecided. Flaked without this
        // under a loaded test run.
        std::thread::sleep(std::time::Duration::from_millis(5));
        // A second, later-written session — its rollout is the newest file
        // on disk, and it carries the null shape, which is what makes the
        // two distinguishable in the assertion below.
        write_managed_codex_rollout(
            &home,
            "newer-session",
            "2026-09-05T07:51:22Z",
            NULL_RATE_LIMITS,
        );
        with_home(&home, || {
            assert!(
                matches!(read("codex"), Err(Miss::NoReading(_))),
                "the newer session's null-shaped rollout must be the one read"
            );
        });
    }

    /// A rollout with no `token_count` event at all — a session that has not
    /// completed a turn yet — is unparseable rather than silently absent, the
    /// same "no legal partial reading" rule claude's own `window` carries.
    #[test]
    fn a_codex_rollout_with_no_token_count_event_is_unparseable() {
        let home = crate::scratch::root("quota-codex-no-token-count");
        std::fs::create_dir_all(&home).unwrap();
        let dir = home.join(".local/state/spoolway/codex/fixture-session/sessions/2026/09/05");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rollout-fixture.jsonl"),
            r#"{"timestamp":"2026-09-05T07:51:22Z","type":"session_meta","payload":{}}"#,
        )
        .unwrap();
        with_home(&home, || {
            assert!(matches!(read("codex"), Err(Miss::Unparseable(_))));
        });
    }

    /// No rollout under any managed lane home at all — no codex lane has run
    /// here yet — reads as unreadable, the same as claude's own
    /// missing-file case.
    #[test]
    fn a_codex_home_with_no_rollout_is_unreadable() {
        let home = crate::scratch::root("quota-codex-missing");
        std::fs::create_dir_all(&home).unwrap();
        with_home(&home, || {
            assert!(matches!(read("codex"), Err(Miss::Unreadable(_))));
        });
    }

    /// The bug this test pins: a real codex lane never writes under
    /// `own_home` at all — it writes under the per-session home spoolway
    /// made for it, `<state_root>/codex/<session>/sessions/**`. A probe that
    /// only checked `own_home` would never see a real lane's own usage, so
    /// the ceiling it is meant to gate would never fire against it. Nothing
    /// under `own_home` here — only the managed lane home — and the reading
    /// still has to come back.
    #[test]
    fn a_reading_under_a_managed_lane_home_is_found_with_no_own_home_rollout() {
        let home = crate::scratch::root("quota-codex-managed-home-only");
        write_managed_codex_rollout(
            &home,
            "019ffa86-b076-78a0-9ee3-cda940041941",
            &Utc::now().to_rfc3339(),
            REAL_SHAPED_RATE_LIMITS,
        );
        with_home(&home, || {
            let reading = read("codex").unwrap_or_else(|_| panic!("expected a reading"));
            assert_eq!(reading.five_hour.utilization, 5);
            assert_eq!(reading.seven_day.utilization, 2);
        });
    }

    /// The other half of the same guarantee, and the one review's own repro
    /// pinned: a rollout under `own_home` — an interactive session, or a run
    /// against a local endpoint — must never win, however much newer it is
    /// than the managed lane's own reading. Written *after* the managed one,
    /// so its mtime really is the latest file on disk, and carrying a wildly
    /// different reading (99%/99%) so a wrong pick is unmistakable.
    #[test]
    fn an_interactive_rollout_never_overrides_a_managed_lane_reading() {
        let home = crate::scratch::root("quota-codex-interactive-never-wins");
        write_managed_codex_rollout(
            &home,
            "fixture-session",
            &Utc::now().to_rfc3339(),
            REAL_SHAPED_RATE_LIMITS,
        );
        let alarming_rate_limits = r#"{"limit_id":"codex","limit_name":null,
            "primary":{"used_percent":99.0,"window_minutes":300,"resets_at":9999999999},
            "secondary":{"used_percent":99.0,"window_minutes":10080,"resets_at":9999999999},
            "credits":null,"individual_limit":null,"spend_control_reached":null,
            "plan_type":"plus","rate_limit_reached_type":null}"#;
        write_interactive_codex_rollout(&home, &Utc::now().to_rfc3339(), alarming_rate_limits);
        with_home(&home, || {
            let reading = read("codex").unwrap_or_else(|_| panic!("expected a reading"));
            assert_eq!(
                reading.five_hour.utilization, 5,
                "the newer interactive rollout must never be read"
            );
            assert_eq!(reading.seven_day.utilization, 2);
        });
    }
}
