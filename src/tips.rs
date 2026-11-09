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

/// One tip as the file holds it.
struct Tip {
    /// The line it sits on, counted from 1, as an editor shows it.
    line: usize,
    /// The sentence the board draws.
    text: &'static str,
    /// What it explains, written after the bar. `None` for a tip about
    /// nothing in particular.
    about: Option<&'static str>,
}

/// Every tip in `list`, in file order. Comment lines and blank lines are
/// not tips.
fn parse(list: &'static str) -> Vec<Tip> {
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
