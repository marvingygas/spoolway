//! The board's tips: one line of advice per entry in `assets/tips.txt`.
//!
//! The list is compiled into the binary, the way `assets/known-copies.txt` is,
//! so the board needs nothing on disk to draw one. Each line is a plain
//! sentence. After a bar, a line may name what it explains: a key on a tab's
//! key line, a config setting, a pipeline or step key, or a task key. The
//! board draws only the text before the bar, and the tests here read the part
//! after it to hold the list complete as spoolway grows.

/// The list, embedded so the binary needs nothing on disk to show a tip.
const LIST: &str = include_str!("../assets/tips.txt");

/// The most characters a tip holds, so it fits one row of an 80-column
/// terminal. Counted in characters, not bytes: a tip may hold `∞`, `↻`, `←`
/// or `→`, each wider than one byte and one column on screen.
#[cfg(test)]
const MAX_CHARACTERS: usize = 69;

/// One tip as the file holds it.
struct Tip<'a> {
    /// The line it sits on, counted from 1, as an editor shows it.
    line: usize,
    /// The sentence the board draws.
    text: &'a str,
    /// What it explains, written after the bar. `None` for a tip about
    /// nothing in particular.
    about: Option<&'a str>,
}

/// Every tip in `list`, in file order. Comment lines and blank lines are
/// not tips.
fn parse(list: &str) -> Vec<Tip<'_>> {
    list.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|(at, line)| {
            let (text, about) = match line.split_once('|') {
                Some((text, about)) => (text, Some(about.trim())),
                None => (line, None),
            };
            Tip {
                line: at + 1,
                text: text.trim(),
                about,
            }
        })
        .collect()
}

/// Every tip's text in file order, with comment lines, blank lines and
/// everything from the bar on removed.
#[allow(dead_code)]
pub fn texts() -> Vec<&'static str> {
    parse(LIST).into_iter().map(|tip| tip.text).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped list holds the 197 tips the plan approved.
    #[test]
    fn the_shipped_list_holds_every_approved_tip() {
        assert_eq!(texts().len(), 197);
    }

    /// What is wrong with each tip in `tips`, as the message a failing test
    /// prints: the line, what is wrong and the fix. Empty when none is.
    fn complaints(tips: &[Tip]) -> String {
        let mut report = String::new();
        for tip in tips {
            let length = tip.text.chars().count();
            if length > MAX_CHARACTERS {
                report += &format!(
                    "assets/tips.txt:{} is {length} characters. a tip holds at most \
                     {MAX_CHARACTERS},\nso it fits one row of an 80-column terminal. \
                     shorten it.\n\n",
                    tip.line
                );
            }
            if let Some((_, word)) = crate::screen::tests::capital_violation(tip.text) {
                let mut letters = word.chars();
                let lower: String = letters
                    .next()
                    .into_iter()
                    .flat_map(char::to_lowercase)
                    .chain(letters)
                    .collect();
                report += &format!(
                    "assets/tips.txt:{} starts a word with a capital: \"{word}\".\n\
                     tui text is lowercase: write \"{lower}\".\n\
                     all-caps words, dates and proper nouns are allowed. if this word \
                     must keep its capital,\nadd it to the allowed list beside \
                     `tui_text_is_lowercase` in src/screen.rs.\n\n",
                    tip.line
                );
            }
        }
        report
    }

    /// A tip longer than 69 characters is named by its line. It is counted in
    /// characters, so `∞` and `→` cost one each and a tip of exactly 69
    /// passes.
    #[test]
    fn a_tip_longer_than_69_characters_is_named() {
        let long = "a".repeat(70);
        let exact = format!("{}→", "a".repeat(68));
        let list = format!("{exact}\n{long}\n");
        let report = complaints(&parse(&list));
        assert!(report.contains("assets/tips.txt:2 is 70 characters"), "{report}");
        assert!(!report.contains("assets/tips.txt:1"), "{report}");
    }

    /// A tip that starts a word with a capital is named by its line, with
    /// the fix. The words the board's own rule lets through are not.
    #[test]
    fn a_tip_that_breaks_the_lowercase_rule_is_named() {
        let list = "the NEXT column and <N> rows of CSV.\nPress r to resume.\n";
        let report = complaints(&parse(list));
        assert!(report.contains("assets/tips.txt:2"), "{report}");
        assert!(report.contains("\"Press\""), "{report}");
        assert!(report.contains("write \"press\""), "{report}");
        assert!(!report.contains("assets/tips.txt:1"), "{report}");
    }

    /// Every tip in the shipped list fits one row and keeps the board's
    /// lowercase rule.
    #[test]
    fn every_tip_is_short_and_lowercase() {
        let report = complaints(&parse(LIST));
        assert!(report.is_empty(), "{report}");
    }

    /// A comment line, a blank line and the tag after a bar are not part of
    /// a tip's text, and a text holding characters wider than a byte keeps
    /// every one of them.
    #[test]
    fn a_tip_is_the_text_before_the_bar() {
        let list = "# a comment\n\nslots 1/∞ means no cap.\nq quits. | key * q\n   \nr ↻ again\n";
        let tips = parse(list);
        let texts: Vec<_> = tips.iter().map(|tip| tip.text).collect();
        assert_eq!(texts, ["slots 1/∞ means no cap.", "q quits.", "r ↻ again"]);
        let about: Vec<_> = tips.iter().map(|tip| tip.about).collect();
        assert_eq!(about, [None, Some("key * q"), None]);
        let lines: Vec<_> = tips.iter().map(|tip| tip.line).collect();
        assert_eq!(lines, [3, 4, 6]);
    }
}
