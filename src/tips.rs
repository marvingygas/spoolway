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
/// or `→`, each more than one byte but one column on screen.
#[cfg(test)]
const MAX_CHARACTERS: usize = 69;

/// One tip as the file holds it.
///
/// Only the tests that hold the list complete read `line` and `about`; the
/// board draws `text` alone.
#[allow(dead_code)]
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
    use crate::commands::contract_keys;

    /// The shipped list yields tips, and each is a bare sentence: no empty
    /// text, no comment line and nothing from the bar on.
    #[test]
    fn the_shipped_list_yields_bare_sentences() {
        let texts = texts();
        assert!(!texts.is_empty());
        for text in texts {
            assert!(!text.trim().is_empty(), "an empty tip");
            assert!(!text.starts_with('#'), "a comment as a tip: {text}");
            assert!(!text.contains('|'), "a tag left in a tip: {text}");
        }
    }

    /// The keys a key line draws, in the order it draws them: each pair is
    /// `[key] label`, three spaces apart.
    fn keys_of(line: &str) -> Vec<String> {
        line.split("   ")
            .filter_map(|pair| pair.trim().strip_prefix('['))
            .filter_map(|pair| pair.split_once(']'))
            .map(|(key, _)| key.to_string())
            .collect()
    }

    /// Every item that needs a tip, read from the code that defines it.
    fn wanted() -> Vec<String> {
        let mut items: Vec<String> = Vec::new();
        items.extend(
            crate::confkv::REFERENCE
                .iter()
                .map(|row| format!("config {}", row.key)),
        );
        items.extend(
            contract_keys::PIPELINE_KEYS
                .iter()
                .map(|key| format!("pipeline {key}")),
        );
        items.extend(
            contract_keys::STEP_KEYS
                .iter()
                .map(|key| format!("step {key}")),
        );
        items.extend(
            contract_keys::REQUIRED_KEYS
                .iter()
                .chain(contract_keys::OPTIONAL_KEYS)
                .map(|key| format!("task {key}")),
        );
        let (queue, routines) = crate::commands::queue_resting_key_lines();
        for (tab, lines) in [
            ("dispatch", crate::status::resting_key_lines()),
            ("queue", queue),
            ("routines", routines),
            ("overrides", crate::screen::overrides::resting_key_lines()),
            ("eval", crate::eval::resting_key_lines()),
        ] {
            for key in lines.iter().flat_map(|line| keys_of(line)) {
                let item = format!("key {tab} {key}");
                if !items.contains(&item) {
                    items.push(item);
                }
            }
        }
        items
    }

    /// The items come from the code, not from a list kept here: every
    /// setting the config contract lists, every pipeline and step key, every
    /// task key, and the keys of each tab's resting key line.
    #[test]
    fn the_wanted_items_are_read_from_the_code() {
        let wanted = wanted();
        let has = |item: String| assert!(wanted.contains(&item), "{item} is not wanted");
        for row in crate::confkv::REFERENCE {
            has(format!("config {}", row.key));
        }
        for key in contract_keys::PIPELINE_KEYS {
            has(format!("pipeline {key}"));
        }
        for key in contract_keys::STEP_KEYS {
            has(format!("step {key}"));
        }
        for key in contract_keys::REQUIRED_KEYS
            .iter()
            .chain(contract_keys::OPTIONAL_KEYS)
        {
            has(format!("task {key}"));
        }
        for item in [
            "key dispatch enter",
            "key dispatch r",
            "key dispatch u",
            "key dispatch q",
            "key queue space",
            "key queue h",
            "key routines enter",
            "key overrides w",
            "key overrides a",
            "key eval e",
            "key eval enter",
        ] {
            has(item.to_string());
        }
    }

    /// Every key, setting and contract key has a tip. Adding one without a
    /// tip fails here, naming it and the line to add.
    #[test]
    fn every_item_has_a_tip() {
        let report = gaps(&wanted(), &parse(LIST));
        assert!(report.is_empty(), "{report}");
    }

    /// The items of `wanted` that no tip in `tips` explains, as the message a
    /// failing test prints: each item, then the line to add for it. Empty
    /// when every item has a tip.
    fn gaps(wanted: &[String], tips: &[Tip]) -> String {
        let named: std::collections::HashSet<&str> =
            tips.iter().filter_map(|tip| tip.about).collect();
        let covered = |item: &str| {
            // A `key * q` tag explains `q` on every tab.
            let every_tab = item
                .strip_prefix("key ")
                .and_then(|rest| rest.split_once(' '))
                .map(|(_, key)| format!("key * {key}"));
            named.contains(item) || every_tab.is_some_and(|tag| named.contains(tag.as_str()))
        };
        let missing: Vec<&String> = wanted.iter().filter(|item| !covered(item)).collect();
        if missing.is_empty() {
            return String::new();
        }
        let split = |item: &str| {
            let (kind, rest) = item.split_once(' ').unwrap_or((item, ""));
            (kind.to_string(), rest.to_string())
        };
        let width = missing
            .iter()
            .map(|item| split(item).0.len())
            .max()
            .unwrap_or(0);
        let mut report = format!("assets/tips.txt has no tip for {} items:\n", missing.len());
        for item in &missing {
            let (kind, rest) = split(item);
            report += &format!("  {kind:width$}  {rest}\n");
        }
        report += "add one line for each: the tip, then a bar, then the item.\n";
        for item in &missing {
            report += &format!(
                "  <one sentence, lowercase, at most {MAX_CHARACTERS} characters> | {item}\n"
            );
        }
        report
    }

    /// An item no tip names is reported with the line that would cover it,
    /// and one a tip names is not. A `key * q` tip covers `q` on every tab.
    #[test]
    fn an_item_without_a_tip_is_named_with_the_line_to_add() {
        let list = "q quits. | key * q\nenter starts. | key dispatch enter\n";
        let wanted: Vec<String> = [
            "key dispatch q",
            "key queue q",
            "key dispatch enter",
            "key queue x",
            "config dispatch.example",
        ]
        .map(String::from)
        .to_vec();
        let report = gaps(&wanted, &parse(list));
        assert!(report.contains("has no tip for 2 items:"), "{report}");
        assert!(report.contains("  key     queue x\n"), "{report}");
        assert!(report.contains("  config  dispatch.example\n"), "{report}");
        assert!(
            report.contains("  <one sentence, lowercase, at most 69 characters> | key queue x\n"),
            "{report}"
        );
        assert!(!report.contains("queue q"), "{report}");
        assert!(!report.contains("dispatch enter"), "{report}");
        assert!(!report.contains("dispatch q"), "{report}");
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
            if let Some((_, word)) = crate::screen::capital_violation(tip.text) {
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
                     must keep its capital,\nadd it to `ALLOWED_CAPITALS` in src/screen.rs.\n\n",
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
        assert!(
            report.contains("assets/tips.txt:2 is 70 characters"),
            "{report}"
        );
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
