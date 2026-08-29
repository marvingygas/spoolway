//! The handful of formatters every surface shares, so a token count or a
//! dollar figure reads the same on the board, in the cost table, and in
//! `spoolway models`.

use std::path::Path;

/// Token counts run to millions, and a column of raw digits is unreadable.
///
/// Crate-visible so `spoolway models` can format a window the same way `spoolway
/// cost` formats a token count — both are the same kind of number.
pub(crate) fn tokens_human(n: u64) -> String {
    match n {
        0 => "0".to_string(),
        n if n < 1_000 => n.to_string(),
        n if n < 1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        n => format!("{:.2}M", n as f64 / 1_000_000.0),
    }
}

/// Sub-cent amounts matter when the whole point is that most lanes are free, so
/// small figures keep more places rather than rounding to `$0.00`.
pub(crate) fn money(cost: Option<f64>) -> String {
    match cost {
        None => "—".to_string(),
        Some(0.0) => "$0".to_string(),
        Some(c) if c < 0.01 => format!("${c:.4}"),
        Some(c) => format!("${c:.2}"),
    }
}

/// The same rounding rule as [`money`], with no `$` — for `spoolway eval` and
/// `spoolway spend`, which name the currency once in a `USD`/`COST USD`
/// column header rather than repeating it in every cell.
pub(crate) fn money_plain(cost: Option<f64>) -> String {
    match cost {
        None => "—".to_string(),
        Some(0.0) => "0".to_string(),
        Some(c) if c < 0.01 => format!("{c:.4}"),
        Some(c) => format!("{c:.2}"),
    }
}

/// First non-empty line, trimmed of list markers.
pub(crate) fn first_line(text: &str) -> &str {
    text.lines()
        .map(|line| line.trim().trim_start_matches("- "))
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

/// A repo-relative path, always written with forward slashes.
///
/// See [`crate::platform::relative`] for why the separator is normalised, and
/// for the test that holds the Windows answer to account from either platform.
pub(crate) fn relative(root: &Path, path: &Path) -> String {
    crate::platform::relative(root, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_costs_keep_the_places_that_make_them_readable() {
        assert_eq!(money(Some(0.0004)), "$0.0004");
        assert_eq!(money(Some(45.409)), "$45.41");
        assert_eq!(money(None), "—");
    }

    #[test]
    fn money_plain_rounds_the_same_way_money_does_with_no_sigil() {
        assert_eq!(money_plain(Some(0.0004)), "0.0004");
        assert_eq!(money_plain(Some(45.409)), "45.41");
        assert_eq!(money_plain(Some(0.0)), "0");
        assert_eq!(money_plain(None), "—");
    }

    #[test]
    fn token_counts_stay_readable_at_every_scale() {
        assert_eq!(tokens_human(0), "0");
        assert_eq!(tokens_human(999), "999");
        assert_eq!(tokens_human(17_280_000), "17.28M");
    }
}
