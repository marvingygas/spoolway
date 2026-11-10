//! The board's tips: one line of advice per entry in `assets/tips.txt`.
//!
//! The list is compiled into the binary, the way `assets/known-copies.txt` is,
//! so the board needs nothing on disk to draw one. Each line is a plain
//! sentence. After a bar, a line may name what it explains: a key on a tab's
//! key line, a config setting, a pipeline or step key, or a task key. The
//! board draws only the text before the bar, and the tests here read the part
//! after it to hold the list complete as spoolway grows. A [`Deck`] deals
//! the tips in shuffled order, one every [`SHOWN_FOR`].

use std::time::{Duration, Instant};

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
pub fn texts() -> Vec<&'static str> {
    parse(LIST).into_iter().map(|tip| tip.text).collect()
}

/// How long one tip stays on the board before the next one takes its row:
/// long enough to notice a tip and read it.
pub const SHOWN_FOR: Duration = Duration::from_secs(30);

/// The tips in the order the board shows them, and which one is up.
///
/// Dealt from a shuffled deck rather than picked at random each time, since
/// a fresh pick can show the same tip twice within minutes. Every tip shows
/// once before any shows again, and then the deck is shuffled again.
///
/// Held by the board for as long as it is open, so the clock and the place
/// in the deck carry from one frame to the next. A deck made on every frame
/// would either never move past its first tip or move on every redraw.
pub struct Deck {
    /// Every tip's text, in file order.
    texts: Vec<&'static str>,
    /// Indexes into `texts`, in the order they are shown.
    order: Vec<usize>,
    /// Where in `order` the tip on screen sits.
    at: usize,
    /// When the tip on screen first showed. `None` until the first frame
    /// asks for one, so a deck made long before its board draws still gives
    /// its first tip the full time.
    since: Option<Instant>,
    /// The state of the generator each shuffle draws from. `None` keeps the
    /// deck in file order for good, so a test always draws a known tip.
    random: Option<u64>,
}

impl Deck {
    /// The deck a board opens with: shuffled in a build people run, and in
    /// file order under `cargo test`, where whole frames are compared and a
    /// random tip in each would make no two runs agree.
    pub fn for_board() -> Deck {
        #[cfg(test)]
        return Deck::in_order(texts());
        #[cfg(not(test))]
        Deck::shuffled(texts(), seed())
    }

    /// `texts` in the order given, and kept in it every time round.
    #[cfg(test)]
    pub fn in_order(texts: Vec<&'static str>) -> Deck {
        Deck::dealt(texts, None)
    }

    /// `texts` shuffled from `seed`, and shuffled again from the same
    /// generator each time the last tip has shown.
    pub fn shuffled(texts: Vec<&'static str>, seed: u64) -> Deck {
        Deck::dealt(texts, Some(seed))
    }

    fn dealt(texts: Vec<&'static str>, random: Option<u64>) -> Deck {
        let mut deck = Deck {
            order: (0..texts.len()).collect(),
            texts,
            at: 0,
            since: None,
            random,
        };
        deck.shuffle();
        deck
    }

    /// The tip to draw at `now`: the one on screen until it has been up for
    /// [`SHOWN_FOR`], then the next one in the deck. `None` only for a deck
    /// with no tips at all.
    ///
    /// Moves at most one tip per call. A board that went undrawn for minutes,
    /// on another tab, shows the next tip when it comes back rather than
    /// skipping the ones it would have shown meanwhile.
    pub fn current(&mut self, now: Instant) -> Option<&'static str> {
        if self.order.is_empty() {
            return None;
        }
        match self.since {
            None => self.since = Some(now),
            Some(since) if now.saturating_duration_since(since) >= SHOWN_FOR => {
                self.next();
                self.since = Some(now);
            }
            Some(_) => {}
        }
        Some(self.texts[self.order[self.at]])
    }

    /// Move to the next tip, shuffling the deck again past its last one.
    fn next(&mut self) {
        self.at += 1;
        if self.at < self.order.len() {
            return;
        }
        let last = self.order[self.order.len() - 1];
        self.at = 0;
        self.shuffle();
        // A new shuffle may open on the tip that just closed the old one,
        // which would leave the same tip up for twice as long, as if the row
        // had stopped changing. It trades places with the deck's last tip.
        if self.order.len() > 1 && self.order[0] == last {
            let end = self.order.len() - 1;
            self.order.swap(0, end);
        }
    }

    /// Shuffle `order` in place, a Fisher-Yates shuffle over the deck's own
    /// generator. Does nothing to a deck kept in file order.
    fn shuffle(&mut self) {
        let Some(state) = self.random.as_mut() else {
            return;
        };
        for i in (1..self.order.len()).rev() {
            let j = (splitmix(state) % (i as u64 + 1)) as usize;
            self.order.swap(i, j);
        }
    }
}

/// The next number from a SplitMix64 generator at `state`. The deck needs
/// only an order no person can predict, not one an attacker cannot, and this
/// is enough for that without taking on a dependency.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A seed that differs from one opening of the board to the next. The
/// standard library keys each new hasher at random, so hashing nothing still
/// gives a fresh number every run.
#[cfg(not(test))]
fn seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
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

    /// Every tip `deck` shows over `rounds` passes through it, read off the
    /// clock stepped [`SHOWN_FOR`] at a time.
    fn shown(deck: &mut Deck, rounds: usize) -> Vec<&'static str> {
        let start = Instant::now();
        (0..deck.order.len() * rounds)
            .map(|step| deck.current(start + SHOWN_FOR * step as u32).unwrap())
            .collect()
    }

    /// One tip stays up for 30 seconds by the clock it is given, and the
    /// next one takes its place at 30.
    #[test]
    fn a_tip_stays_up_for_30_seconds() {
        let mut deck = Deck::in_order(vec!["one", "two", "three"]);
        let start = Instant::now();
        assert_eq!(deck.current(start), Some("one"));
        assert_eq!(deck.current(start + Duration::from_secs(29)), Some("one"));
        assert_eq!(deck.current(start + SHOWN_FOR), Some("two"));
        assert_eq!(
            deck.current(start + SHOWN_FOR + Duration::from_secs(29)),
            Some("two")
        );
        assert_eq!(deck.current(start + SHOWN_FOR * 2), Some("three"));
    }

    /// A board left undrawn for minutes shows the next tip when it comes
    /// back, not one several places further on.
    #[test]
    fn a_long_gap_moves_one_tip() {
        let mut deck = Deck::in_order(vec!["one", "two", "three"]);
        let start = Instant::now();
        deck.current(start);
        assert_eq!(deck.current(start + SHOWN_FOR * 10), Some("two"));
    }

    /// The shipped list, shuffled: each round shows every tip once, in an
    /// order that is not the file's, and the next round is shuffled again
    /// rather than repeating the first.
    #[test]
    fn a_shuffled_deck_shows_every_tip_once_a_round() {
        let all = texts();
        let mut deck = Deck::shuffled(all.clone(), 7);
        let shown = shown(&mut deck, 2);
        let (first, second) = shown.split_at(all.len());
        for round in [first, second] {
            let mut sorted = round.to_vec();
            sorted.sort_unstable();
            let mut expected = all.clone();
            expected.sort_unstable();
            assert_eq!(sorted, expected, "a round missed a tip or showed one twice");
        }
        assert_ne!(first, all.as_slice(), "the first round is in file order");
        assert_ne!(first, second, "the second round was not shuffled again");
    }

    /// Two boards opened apart deal two different orders.
    #[test]
    fn two_seeds_deal_two_orders() {
        let first = shown(&mut Deck::shuffled(texts(), 1), 1);
        let second = shown(&mut Deck::shuffled(texts(), 2), 1);
        assert_ne!(first, second);
    }

    /// The tip that closes one round never opens the next, whatever the
    /// shuffle deals, so no tip stays up for a minute.
    #[test]
    fn a_new_round_never_opens_on_the_tip_that_closed_the_last() {
        for seed in 0..200 {
            let mut deck = Deck::shuffled(vec!["one", "two", "three"], seed);
            let shown = shown(&mut deck, 20);
            for pair in shown.windows(2) {
                assert_ne!(pair[0], pair[1], "seed {seed}: {shown:?}");
            }
        }
    }

    /// A deck with no tips, or with one, never panics and never moves off
    /// the one it has.
    #[test]
    fn a_deck_of_none_or_one_tip() {
        let start = Instant::now();
        let mut empty = Deck::shuffled(Vec::new(), 3);
        assert_eq!(empty.current(start), None);
        assert_eq!(empty.current(start + SHOWN_FOR), None);
        let mut one = Deck::shuffled(vec!["only"], 3);
        assert_eq!(one.current(start), Some("only"));
        assert_eq!(one.current(start + SHOWN_FOR), Some("only"));
        assert_eq!(one.current(start + SHOWN_FOR * 2), Some("only"));
    }
}
