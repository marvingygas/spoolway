//! The one place spoolway turns instants into text and back.
//!
//! Timestamps are written to disk — ledger rows, problem-log lines, hook
//! payloads — and read again by later releases, so their text form is a file
//! format. Releases before this one wrote them with `chrono`, whose
//! `to_rfc3339` prints a `+00:00` offset for UTC and trims the fraction to
//! none, three, six or nine digits. `jiff`'s own `Display` prints `Z` and
//! trims every trailing zero instead, so a call site that formatted with it
//! would quietly change what lands in a file. Every writer goes through the
//! functions here, and every reader accepts both spellings.

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

/// Format `at` as `to_rfc3339` of a UTC `chrono::DateTime` did: a `+00:00`
/// offset and a fraction of none, three, six or nine digits.
pub fn rfc3339(at: Timestamp) -> String {
    let mut out = at.strftime("%Y-%m-%dT%H:%M:%S").to_string();
    // The fraction is read off the civil time, which is never negative.
    // `Timestamp::subsec_nanosecond` is negative before 1970, while the
    // seconds printed above are floored, so using it would name the wrong
    // instant.
    push_fraction(&mut out, at.to_zoned(TimeZone::UTC).subsec_nanosecond());
    out.push_str("+00:00");
    out
}

/// Format `at` as `to_rfc3339` of a local `chrono::DateTime` did: the zone's
/// own offset after the same fraction [`rfc3339`] writes.
pub fn rfc3339_local(at: &Zoned) -> String {
    let mut out = at.strftime("%Y-%m-%dT%H:%M:%S").to_string();
    push_fraction(&mut out, at.subsec_nanosecond());
    out.push_str(&at.strftime("%:z").to_string());
    out
}

/// Read an RFC 3339 timestamp in any offset, or `None` when it is not one.
pub fn parse_rfc3339(raw: &str) -> Option<Timestamp> {
    raw.parse().ok()
}

/// Read `raw` as an RFC 3339 timestamp and draw it in the local zone with
/// the `strftime` pattern `fmt`, or `None` when it does not parse.
pub fn format_local(raw: &str, fmt: &str) -> Option<String> {
    let at = parse_rfc3339(raw)?;
    Some(at.to_zoned(TimeZone::system()).strftime(fmt).to_string())
}

/// Today's date in UTC, the calendar the price table's `generated` day is
/// written in.
pub fn utc_today() -> Date {
    Timestamp::now().to_zoned(TimeZone::UTC).date()
}

/// Append `.fff`, `.ffffff` or `.fffffffff` for the shortest of the three
/// that holds `nanos` exactly, and nothing for a whole second. `nanos` is
/// the civil time's, so never negative.
fn push_fraction(out: &mut String, nanos: i32) {
    let nanos = nanos as u32;
    if nanos == 0 {
        return;
    }
    if nanos.is_multiple_of(1_000_000) {
        out.push_str(&format!(".{:03}", nanos / 1_000_000));
    } else if nanos.is_multiple_of(1_000) {
        out.push_str(&format!(".{:06}", nanos / 1_000));
    } else {
        out.push_str(&format!(".{nanos:09}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fraction is cut at three, six or nine digits, never at the first
    /// trailing zero, which is where `jiff`'s own `Display` would cut it.
    #[test]
    fn rfc3339_keeps_the_fraction_widths_chrono_wrote() {
        let at = |nanos: i64| {
            Timestamp::from_nanosecond(1_795_120_499_000_000_000 + nanos as i128).unwrap()
        };
        assert_eq!(rfc3339(at(0)), "2026-11-19T20:34:59+00:00");
        assert_eq!(rfc3339(at(500_000_000)), "2026-11-19T20:34:59.500+00:00");
        assert_eq!(rfc3339(at(120_000)), "2026-11-19T20:34:59.000120+00:00");
        assert_eq!(rfc3339(at(7)), "2026-11-19T20:34:59.000000007+00:00");
    }

    /// Before 1970 a timestamp's own fraction is negative while its printed
    /// seconds are floored, so the text must take the fraction from the
    /// civil time: a quarter second before the epoch is `.750` past 23:59:59.
    #[test]
    fn rfc3339_writes_a_pre_epoch_fraction_as_the_civil_time_reads_it() {
        let at = Timestamp::from_nanosecond(-250_000_000).unwrap();
        assert_eq!(rfc3339(at), "1969-12-31T23:59:59.750+00:00");
        assert_eq!(parse_rfc3339(&rfc3339(at)), Some(at));
    }

    /// spoolway is built on `jiff` 1.0: the manifest asks for it with the
    /// `serde` feature, and the lock file resolves a 1.x release.
    #[test]
    fn jiff_is_required_at_1_0_and_locked_to_a_1_x_release() {
        let manifest = include_str!("../Cargo.toml");
        let line = manifest
            .lines()
            .find(|l| l.starts_with("jiff = "))
            .expect("Cargo.toml has a jiff dependency");
        assert_eq!(line, r#"jiff = { version = "1.0", features = ["serde"] }"#);

        let lock = include_str!("../Cargo.lock");
        let version = lock
            .split("[[package]]")
            .find(|p| p.lines().any(|l| l == r#"name = "jiff""#))
            .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = ")))
            .expect("Cargo.lock resolves jiff");
        assert!(
            version.starts_with("\"1."),
            "Cargo.lock resolves jiff {version}, want 1.x"
        );
    }

    /// A zone's offset is written with a colon, and the same instant reads
    /// back whichever way it was spelled.
    #[test]
    fn rfc3339_round_trips_in_both_offset_spellings() {
        let at: Timestamp = "2026-11-19T20:34:59.250Z".parse().unwrap();
        let zoned = at.to_zoned(TimeZone::posix("CET-1CEST,M3.5.0,M10.5.0/3").unwrap());
        assert_eq!(rfc3339_local(&zoned), "2026-11-19T21:34:59.250+01:00");
        assert_eq!(parse_rfc3339(&rfc3339_local(&zoned)), Some(at));
        assert_eq!(parse_rfc3339(&rfc3339(at)), Some(at));
        assert_eq!(parse_rfc3339("2026-11-19T20:34:59.250Z"), Some(at));
        assert_eq!(parse_rfc3339("not a time"), None);
    }
}
