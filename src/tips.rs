//! The board's tips: one line of advice per entry in `assets/tips.txt`.
//!
//! The list is compiled into the binary, the way `assets/known-copies.txt` is,
//! so the board needs nothing on disk to draw one. Each line is a plain
//! sentence. A line in brackets, such as `[queue]`, starts a section, and
//! every tip below it belongs to that section. The rules for what earns a
//! tip are written at the top of the file, for the person editing it.

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
/// Only the tests read `line` and `section`; the board draws `text` alone.
#[allow(dead_code)]
struct Tip<'a> {
    /// The line it sits on, counted from 1, as an editor shows it.
    line: usize,
    /// The sentence the board draws.
    text: &'a str,
    /// The label of the section it sits under, such as `queue`. `None` for
    /// a tip above the file's first section line.
    section: Option<&'a str>,
}

/// Every tip in `list`, in file order. Comment lines, blank lines and
/// section lines are not tips; a section line sets the section of every tip
/// after it.
fn parse(list: &str) -> Vec<Tip<'_>> {
    let mut section = None;
    let mut tips = Vec::new();
    for (at, line) in list.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(label) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = Some(label.trim());
            continue;
        }
        tips.push(Tip {
            line: at + 1,
            text: line,
            section,
        });
    }
    tips
}

/// Every tip's text in file order, with comment lines, blank lines and
/// section lines left out.
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

    /// The shipped list yields tips, and each is a bare sentence: no empty
    /// text, no comment line, no section line and no bar.
    #[test]
    fn the_shipped_list_yields_bare_sentences() {
        let texts = texts();
        assert!(!texts.is_empty());
        for text in texts {
            assert!(!text.trim().is_empty(), "an empty tip");
            assert!(!text.starts_with('#'), "a comment as a tip: {text}");
            assert!(!text.starts_with('['), "a section line as a tip: {text}");
            assert!(!text.contains('|'), "a bar left in a tip: {text}");
        }
    }

    /// Every tip in the shipped list sits under a section, so the board
    /// always has a label to draw before it.
    #[test]
    fn every_shipped_tip_has_a_section() {
        for tip in parse(LIST) {
            assert!(tip.section.is_some(), "line {} has no section", tip.line);
        }
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
        let list = "the NEXT column and rows of CSV.\nPress r to resume.\n";
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

    /// A comment line, a blank line and a section line are not tips, each
    /// tip takes the section above it, and a text holding characters wider
    /// than a byte keeps every one of them.
    #[test]
    fn a_tip_is_a_line_under_its_section() {
        let list = "# a comment\n\nfirst.\n[queue]\nslots 1/∞ means no cap.\n   \n\
                    [eval ]\nr ↻ again\n";
        let tips = parse(list);
        let texts: Vec<_> = tips.iter().map(|tip| tip.text).collect();
        assert_eq!(texts, ["first.", "slots 1/∞ means no cap.", "r ↻ again"]);
        let sections: Vec<_> = tips.iter().map(|tip| tip.section).collect();
        assert_eq!(sections, [None, Some("queue"), Some("eval")]);
        let lines: Vec<_> = tips.iter().map(|tip| tip.line).collect();
        assert_eq!(lines, [3, 5, 8]);
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
