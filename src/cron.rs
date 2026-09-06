//! A five-field cron expression, parsed here rather than pulled in as a
//! dependency — the crate list is short and this is the only place
//! five-field expressions are needed.
//!
//! The grammar is `minute hour day-of-month month day-of-week`. Each field is
//! a comma list of terms, and each term is one of `*`, `n`, `a-b`, `*/n` or
//! `a-b/n`. The month field also takes `jan`-`dec`, and the day-of-week field
//! `sun`-`sat` (`0` is Sunday). `@hourly`, `@daily`, `@weekly` and `@monthly`
//! are whole-expression aliases. A field out of range, or a sixth field, is
//! refused with the field named. Times are the dispatcher machine's own local
//! time.
//!
//! The one rule a hand-written parser usually gets wrong: when *both* day
//! fields are restricted, a match on *either* fires the job. So `0 3 13 * fri`
//! is the thirteenth or any Friday, not Friday the thirteenth. This follows
//! Vixie cron, where a field counts as restricted when it does not begin with
//! `*` — so `*/2` is not restricted, but `1-5` is.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};

/// A parsed cron expression. Each field is a bitmask with bit `v` set for
/// every value `v` the field matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64,
    /// Whether the day-of-month field is restricted (does not begin with
    /// `*`). Both this and `dow_restricted` decide whether the two day
    /// fields are combined with "or" rather than "and".
    dom_restricted: bool,
    dow_restricted: bool,
}

const MONTHS: &[(&str, u32)] = &[
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

const DAYS: &[(&str, u32)] = &[
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

/// How far ahead [`Cron::next_after`] scans before concluding that an
/// expression can never fire. The Gregorian calendar — dates and weekdays
/// both — repeats exactly every 400 years, which is 146097 days and a whole
/// number of weeks, so an expression that has not fired within one full
/// cycle never will. A shorter bound is a real bug: `0 0 29 2 *` skips the
/// non-leap century year 2100, so after 29 Feb 2096 its next firing is 29
/// Feb 2104 — an eight-year gap a four-year scan would misreport as "never".
/// The scan is day-by-day, and a day's 1440 minutes are examined only once
/// the day itself matches, so covering the whole cycle still costs well
/// under a millisecond for an ordinary expression.
///
/// [`crate::jobs`] reuses this as the one overall horizon for its
/// timezone-aware scan, so a resolver that keeps rejecting candidates still
/// terminates.
pub(crate) const SEARCH_LIMIT_DAYS: i64 = 146_097;

impl Cron {
    /// Parse a five-field expression or a `@` alias. The error names the
    /// field at fault and quotes the input, matching `config::parse_duration`.
    pub fn parse(expr: &str) -> Result<Cron, String> {
        let trimmed = expr.trim();
        if trimmed.is_empty() {
            return Err("empty cron expression".into());
        }

        if let Some(alias) = trimmed.strip_prefix('@') {
            let expanded = match alias {
                "hourly" => "0 * * * *",
                "daily" => "0 0 * * *",
                "weekly" => "0 0 * * 0",
                "monthly" => "0 0 1 * *",
                other => {
                    return Err(format!(
                        "`@{other}`: the only aliases are @hourly, @daily, @weekly and @monthly"
                    ));
                }
            };
            return Cron::parse(expanded);
        }

        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        if fields.len() > 5 {
            return Err(format!(
                "`{}`: a sixth field, `{}` — a cron expression has five \
                 (minute hour day-of-month month day-of-week)",
                trimmed, fields[5]
            ));
        }
        if fields.len() < 5 {
            return Err(format!(
                "`{}`: only {} field{} — a cron expression has five \
                 (minute hour day-of-month month day-of-week)",
                trimmed,
                fields.len(),
                if fields.len() == 1 { "" } else { "s" }
            ));
        }

        Ok(Cron {
            minute: parse_field(fields[0], 0, 59, "minute", &[])?,
            hour: parse_field(fields[1], 0, 23, "hour", &[])?,
            dom: parse_field(fields[2], 1, 31, "day-of-month", &[])?,
            month: parse_field(fields[3], 1, 12, "month", MONTHS)?,
            dow: parse_field(fields[4], 0, 6, "day-of-week", DAYS)?,
            dom_restricted: !fields[2].starts_with('*'),
            dow_restricted: !fields[4].starts_with('*'),
        })
    }

    /// Whether this expression fires at `when`, to the minute.
    pub fn matches(&self, when: &NaiveDateTime) -> bool {
        bit(self.minute, when.minute())
            && bit(self.hour, when.hour())
            && self.date_matches(when.date())
    }

    /// The month-and-day half of [`Cron::matches`] — split out so
    /// [`Cron::next_after`] can rule a day out without walking its 1440
    /// minutes.
    ///
    /// The both-day-fields-restricted rule lives here: when neither day field
    /// is `*`, a match on *either* counts (`0 3 13 * fri` is the thirteenth
    /// or any Friday); otherwise both must match, and the `*` one always
    /// does.
    fn date_matches(&self, date: NaiveDate) -> bool {
        if !bit(self.month, date.month()) {
            return false;
        }
        // chrono's `num_days_from_sunday` is 0 for Sunday, matching the cron
        // day-of-week numbering.
        let dom_ok = bit(self.dom, date.day());
        let dow_ok = bit(self.dow, date.weekday().num_days_from_sunday());
        if self.dom_restricted && self.dow_restricted {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        }
    }

    /// The first whole minute strictly after `after` that this expression
    /// fires. `None` only when nothing matches within one full Gregorian
    /// cycle ([`SEARCH_LIMIT_DAYS`]), which means the expression can never
    /// fire.
    ///
    /// Scans day by day: a day that [`Cron::date_matches`] rejects is
    /// skipped whole, and only a matching day has its minutes walked. So an
    /// impossible expression like `0 0 30 2 *` costs one cheap check per day
    /// across the cycle rather than one per minute.
    pub fn next_after(&self, after: NaiveDateTime) -> Option<NaiveDateTime> {
        let start = (after + Duration::minutes(1))
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))?;
        let last_day = start.date() + Duration::days(SEARCH_LIMIT_DAYS);

        let mut day = start.date();
        while day <= last_day {
            if self.date_matches(day) {
                // On the starting day, only minutes at or after `start`
                // count; every later day starts from midnight.
                let from = if day == start.date() {
                    start.hour() * 60 + start.minute()
                } else {
                    0
                };
                for minute in from..1440 {
                    let (hour, minute) = (minute / 60, minute % 60);
                    if bit(self.hour, hour) && bit(self.minute, minute) {
                        return day.and_hms_opt(hour, minute, 0);
                    }
                }
            }
            day = day.succ_opt()?;
        }
        None
    }
}

/// Bit `value` of `mask`, false for any value past bit 63.
fn bit(mask: u64, value: u32) -> bool {
    value < 64 && mask & (1u64 << value) != 0
}

/// One field: a comma list of terms, folded into a bitmask. `names` maps
/// lowercased aliases onto numbers for the fields that take them.
fn parse_field(
    field: &str,
    lo: u32,
    hi: u32,
    name: &str,
    names: &[(&str, u32)],
) -> Result<u64, String> {
    let mut mask = 0u64;
    for term in field.split(',') {
        let term = term.trim();
        if term.is_empty() {
            return Err(format!("{name} field: an empty term in `{field}`"));
        }

        let (range_part, step) = match term.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .map_err(|_| format!("{name} field: `{step}` in `{term}` is not a step"))?;
                if step == 0 {
                    return Err(format!("{name} field: a step of zero in `{term}`"));
                }
                (range, step)
            }
            None => (term, 1),
        };

        let (start, end) = if range_part == "*" {
            (lo, hi)
        } else if let Some((from, to)) = range_part.split_once('-') {
            (
                resolve(from, name, names, lo, hi)?,
                resolve(to, name, names, lo, hi)?,
            )
        } else if term.contains('/') {
            // A step applies only to `*` or a range — `*/n` and `a-b/n`.
            // `n/step` is not grammar this accepts.
            return Err(format!(
                "{name} field: `{term}` — a step goes on `*` or a range `a-b`, not a bare value"
            ));
        } else {
            let value = resolve(range_part, name, names, lo, hi)?;
            (value, value)
        };

        if start > end {
            return Err(format!(
                "{name} field: `{term}` runs backwards ({start} to {end})"
            ));
        }

        // `checked_add` rather than `+`: a step near `u32::MAX` — a
        // syntactically valid `*/4000000000` — would otherwise overflow and
        // panic instead of just landing on `start` alone.
        let mut value = start;
        while value <= end {
            mask |= 1u64 << value;
            match value.checked_add(step) {
                Some(next) => value = next,
                None => break,
            }
        }
    }
    Ok(mask)
}

/// One endpoint of a term: a plain number, or a name where the field takes
/// one. Out of range is refused with the field named.
fn resolve(
    token: &str,
    name: &str,
    names: &[(&str, u32)],
    lo: u32,
    hi: u32,
) -> Result<u32, String> {
    let token = token.trim();
    let value = match token.parse::<u32>() {
        Ok(number) => number,
        Err(_) => {
            let lower = token.to_ascii_lowercase();
            *names
                .iter()
                .find(|(alias, _)| *alias == lower)
                .map(|(_, number)| number)
                .ok_or_else(|| {
                    format!("{name} field: `{token}` is not a number or a name this field accepts")
                })?
        }
    };
    if value < lo || value > hi {
        return Err(format!("{name} field: {value} is out of range {lo}-{hi}"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn a_star_field_matches_every_value() {
        let cron = Cron::parse("* * * * *").unwrap();
        assert!(cron.matches(&at("2026-09-06 07:49")));
        assert!(cron.matches(&at("2026-12-31 23:59")));
    }

    #[test]
    fn a_fixed_minute_and_hour_match_only_that_one_time() {
        let cron = Cron::parse("0 3 * * *").unwrap();
        assert!(cron.matches(&at("2026-09-07 03:00")));
        assert!(!cron.matches(&at("2026-09-07 03:01")));
        assert!(!cron.matches(&at("2026-09-07 04:00")));
    }

    #[test]
    fn a_weekday_range_matches_monday_through_friday() {
        // 03:00, Monday to Friday.
        let cron = Cron::parse("0 3 * * 1-5").unwrap();
        assert!(cron.matches(&at("2026-09-07 03:00")), "Monday");
        assert!(cron.matches(&at("2026-09-11 03:00")), "Friday");
        assert!(!cron.matches(&at("2026-09-12 03:00")), "Saturday");
        assert!(!cron.matches(&at("2026-09-13 03:00")), "Sunday");
    }

    #[test]
    fn a_slash_step_on_star_matches_every_nth_minute() {
        let cron = Cron::parse("*/15 * * * *").unwrap();
        for minute in [0, 15, 30, 45] {
            assert!(cron.matches(&at(&format!("2026-09-06 07:{minute:02}"))));
        }
        assert!(!cron.matches(&at("2026-09-06 07:10")));
    }

    #[test]
    fn a_slash_step_on_a_range_matches_every_nth_value_within_it() {
        // Every second value from 10 to 20 in the minute field.
        let cron = Cron::parse("10-20/2 * * * *").unwrap();
        for minute in [10, 12, 14, 16, 18, 20] {
            assert!(
                cron.matches(&at(&format!("2026-09-06 07:{minute:02}"))),
                "{minute}"
            );
        }
        assert!(!cron.matches(&at("2026-09-06 07:11")));
        assert!(!cron.matches(&at("2026-09-06 07:22")));
    }

    #[test]
    fn a_comma_list_matches_any_of_its_terms() {
        let cron = Cron::parse("0,30 * * * *").unwrap();
        assert!(cron.matches(&at("2026-09-06 07:00")));
        assert!(cron.matches(&at("2026-09-06 07:30")));
        assert!(!cron.matches(&at("2026-09-06 07:15")));
    }

    #[test]
    fn month_names_are_accepted() {
        let cron = Cron::parse("0 0 1 jan-mar *").unwrap();
        assert!(cron.matches(&at("2026-02-01 00:00")));
        assert!(!cron.matches(&at("2026-04-01 00:00")));
    }

    #[test]
    fn weekday_names_are_accepted() {
        let cron = Cron::parse("0 3 * * sun").unwrap();
        assert!(cron.matches(&at("2026-09-13 03:00")), "a Sunday");
        assert!(!cron.matches(&at("2026-09-14 03:00")), "a Monday");
    }

    #[test]
    fn a_day_of_month_matches_only_that_date() {
        let cron = Cron::parse("30 2 1 * *").unwrap();
        assert!(cron.matches(&at("2026-09-01 02:30")));
        assert!(!cron.matches(&at("2026-09-02 02:30")));
    }

    #[test]
    fn both_day_fields_restricted_matches_either() {
        // The thirteenth, or any Friday.
        let cron = Cron::parse("0 3 13 * fri").unwrap();
        assert!(
            cron.matches(&at("2026-05-13 03:00")),
            "the 13th, a Wednesday"
        );
        assert!(
            cron.matches(&at("2026-09-04 03:00")),
            "a Friday that is not the 13th"
        );
        assert!(cron.matches(&at("2026-11-13 03:00")), "Friday the 13th");
        assert!(!cron.matches(&at("2026-09-05 03:00")), "neither");
    }

    #[test]
    fn one_day_field_restricted_matches_only_that_one() {
        // Only the day-of-month is restricted, so the `*` weekday does not
        // widen it: the 1st, whatever weekday it falls on, and nothing else.
        let cron = Cron::parse("0 0 1 * *").unwrap();
        assert!(cron.matches(&at("2026-09-01 00:00")));
        assert!(!cron.matches(&at("2026-09-08 00:00")));
    }

    #[test]
    fn the_at_aliases_expand_to_their_five_field_form() {
        assert_eq!(
            Cron::parse("@hourly").unwrap(),
            Cron::parse("0 * * * *").unwrap()
        );
        assert_eq!(
            Cron::parse("@daily").unwrap(),
            Cron::parse("0 0 * * *").unwrap()
        );
        assert_eq!(
            Cron::parse("@weekly").unwrap(),
            Cron::parse("0 0 * * 0").unwrap()
        );
        assert_eq!(
            Cron::parse("@monthly").unwrap(),
            Cron::parse("0 0 1 * *").unwrap()
        );
    }

    #[test]
    fn an_unknown_alias_is_refused() {
        let err = Cron::parse("@yearly").unwrap_err();
        assert!(err.contains("@yearly"), "{err}");
        assert!(err.contains("@daily"), "{err}");
    }

    #[test]
    fn a_field_out_of_range_names_the_field() {
        let err = Cron::parse("60 * * * *").unwrap_err();
        assert!(err.contains("minute field"), "{err}");
        assert!(err.contains("0-59"), "{err}");

        let err = Cron::parse("0 24 * * *").unwrap_err();
        assert!(err.contains("hour field"), "{err}");

        let err = Cron::parse("0 0 * 13 *").unwrap_err();
        assert!(err.contains("month field"), "{err}");

        let err = Cron::parse("0 0 * * 7").unwrap_err();
        assert!(err.contains("day-of-week field"), "{err}");
    }

    #[test]
    fn a_sixth_field_is_refused() {
        let err = Cron::parse("0 3 * * 1-5 extra").unwrap_err();
        assert!(err.contains("sixth field"), "{err}");
        assert!(err.contains("extra"), "{err}");
    }

    #[test]
    fn too_few_fields_is_refused() {
        let err = Cron::parse("0 3 *").unwrap_err();
        assert!(err.contains("only 3 fields"), "{err}");
    }

    #[test]
    fn a_step_on_a_bare_value_is_refused() {
        // `5/15` is not `*/15` or `10-20/15` — the grammar takes a step only
        // on a star or a range.
        let err = Cron::parse("5/15 * * * *").unwrap_err();
        assert!(err.contains("minute field"), "{err}");
        assert!(err.contains("bare value"), "{err}");
    }

    #[test]
    fn a_step_near_the_integer_ceiling_parses_without_overflowing() {
        // Once a syntactically valid step and a `*` are folded together, the
        // progression must not run off the end of `u32`.
        let cron = Cron::parse("0 0 * */4294967295 *").unwrap();
        assert!(
            cron.matches(&at("2026-01-15 00:00")),
            "January, the low bound"
        );
        assert!(
            !cron.matches(&at("2026-06-15 00:00")),
            "and nothing past it"
        );
    }

    #[test]
    fn next_after_finds_the_following_firing() {
        let cron = Cron::parse("0 3 * * 1-5").unwrap();
        // A Saturday: the next weekday-3am is the coming Monday.
        let next = cron.next_after(at("2026-09-12 10:00")).unwrap();
        assert_eq!(next, at("2026-09-14 03:00"));
    }

    #[test]
    fn next_after_skips_the_current_minute() {
        let cron = Cron::parse("*/15 * * * *").unwrap();
        let next = cron.next_after(at("2026-09-06 07:15")).unwrap();
        assert_eq!(next, at("2026-09-06 07:30"));
    }

    #[test]
    fn an_expression_that_never_fires_has_no_next() {
        // The 30th of February.
        let cron = Cron::parse("0 0 30 2 *").unwrap();
        assert_eq!(cron.next_after(at("2026-01-01 00:00")), None);
    }

    #[test]
    fn a_leap_day_expression_still_resolves_within_the_window() {
        let cron = Cron::parse("0 0 29 2 *").unwrap();
        let next = cron
            .next_after(
                NaiveDate::from_ymd_opt(2026, 3, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(next, at("2028-02-29 00:00"));
    }

    #[test]
    fn a_leap_day_expression_bridges_the_eight_year_gap_at_a_non_leap_century() {
        // 2100 is not a leap year, so after 29 Feb 2096 the next 29 Feb is
        // 2104 — eight years on. A scan that gave up before then would report
        // this real, recurring job as one that never fires.
        let cron = Cron::parse("0 0 29 2 *").unwrap();
        let next = cron.next_after(at("2096-02-29 12:00")).unwrap();
        assert_eq!(next, at("2104-02-29 00:00"));
    }

    #[test]
    fn an_impossible_expression_scans_the_whole_cycle_without_hanging() {
        // No 30th of February anywhere in 400 years — and the day-level scan
        // makes deciding that cheap.
        let cron = Cron::parse("0 0 30 2 *").unwrap();
        assert_eq!(cron.next_after(at("2026-06-15 09:00")), None);
    }
}
