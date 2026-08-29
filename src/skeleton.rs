//! The one region of a page skeleton that a machine reads — currently fed by
//! nothing, see `skeletons()` below.
//!
//! A skeleton exists to be restyled — that is the whole reason one is a file
//! rather than prose in a skill. So `update`'s usual rule (take the file back
//! only when it is byte-for-byte ours) is useless for it: every project that
//! used it as intended would be refused an update forever.
//!
//! But it is not decoration either. A skeleton can carry a block something
//! parses, and a restyled skeleton whose block predates a schema change would
//! write pages that are quietly short of a field.
//!
//! So this finds that block and nothing else. No markers are invented for it,
//! because the host format already delimits one: a `<script>` element has an end
//! tag. The question "is this still ours?"
//! is answered by comparing the block's text to what we ship and to what we
//! shipped before, which needs nothing recorded in the file at all.
//!
//! Document skeletons were here once and are not any more. They belong to the
//! archivist now — written once by `init`, never updated — so there is no
//! shipped version for a project's copy to have drifted from. The plan
//! skeleton was here too, and went out with the plan store: spoolway-plan
//! writes its own self-contained page from a skeleton under the skill's own
//! `assets/`, and nothing in the binary reads a byte of it.

/// What the fingerprint is taken over: line endings and trailing whitespace
/// removed, so a file that made a round trip through an editor on Windows is
/// not reported as edited by a person who never opened it.
fn normalized(text: &str) -> String {
    text.replace('\r', "")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Whether two pieces of generated text are the same one, ignoring the
/// whitespace an editor may have changed on the way past.
pub fn same(a: &str, b: &str) -> bool {
    normalized(a) == normalized(b)
}

/// Byte offset of the first line at or after `from` that *is* `marker`.
///
/// `split_inclusive` rather than `lines`, because the offsets have to survive a
/// file written on Windows: `lines` drops the `\r` it strips, and an offset a
/// byte short per line lands the replacement in the middle of a word.
fn line_at(text: &str, marker: &str, from: usize) -> Option<usize> {
    let mut at = from;
    for line in text[from..].split_inclusive('\n') {
        if line.trim() == marker {
            return Some(at);
        }
        at += line.len();
    }
    None
}

/// FNV-1a, folded to 32 bits and printed as eight hex digits.
///
/// Deliberately not a cryptographic hash and deliberately not a dependency:
/// this detects an edit, it does not resist one. Anyone who wants to defeat it
/// owns the file already.
pub fn fingerprint(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in normalized(text).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", ((hash >> 32) as u32) ^ (hash as u32))
}

/// How a block spoolway owns is delimited inside a file a project owns the rest
/// of, in that file's own format.
///
/// Two variants, one per host format. Nothing in [`skeletons`] uses `Script`
/// any more — the plan skeleton was its one caller, and it went out with the
/// plan store — but the variant stays: the next skeleton this machinery
/// serves is as likely to be another `<script>`-fenced page as another
/// comment-fenced one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// `<script type="application/json" id="...">` through `</script>`.
    #[allow(dead_code)]
    Script(&'static str),
    /// A run of comment lines, from the line that is the begin marker through
    /// the line that is the end marker.
    ///
    /// For a format with no element to close, which is every plain-text one: a
    /// pipeline file's key reference is comments in YAML, so the fence has to
    /// be written down rather than borrowed from the syntax. The pair is
    /// [`crate::assets::IGNORE_BEGIN`] / [`crate::assets::IGNORE_END`], the
    /// same one already fencing spoolway's rules in a project's `.gitignore`.
    Comment(&'static str, &'static str),
}

impl Region {
    /// Byte range of the block, markers included, or `None` where the file has
    /// no such block — which for a project's own file means it was restyled
    /// past recognition, or never opted in, and for ours would be a bug the
    /// tests below catch.
    pub fn find(&self, text: &str) -> Option<(usize, usize)> {
        match self {
            Region::Script(id) => {
                let needle = format!("id=\"{id}\"");
                let at = text.find(&needle)?;
                let open = text[..at].rfind("<script")?;
                let close = text[at..].find("</script>")? + at + "</script>".len();
                Some((open, close))
            }
            // Whole lines, matched trimmed, because a marker is a line and not
            // a substring: a comment *about* the fence must not be read as one
            // half of it.
            Region::Comment(begin, end) => {
                let start = line_at(text, begin, 0)?;
                let last = line_at(text, end, start)?;
                let line = text[last..].split_inclusive('\n').next()?;
                Some((start, last + line.trim_end().len()))
            }
        }
    }

    /// The block's text, as it stands in `text`.
    pub fn read<'a>(&self, text: &'a str) -> Option<&'a str> {
        self.find(text).map(|(start, end)| &text[start..end])
    }

    /// `text` with its block replaced by `block`, and every other byte kept.
    pub fn replace(&self, text: &str, block: &str) -> Option<String> {
        let (start, end) = self.find(text)?;
        Some(format!("{}{}{}", &text[..start], block, &text[end..]))
    }
}

/// One shipped skeleton: where a project keeps it, what we ship, and how to
/// find the block in either.
pub struct Skeleton {
    /// Where the project keeps it, relative to the repo root.
    pub path: &'static str,
    pub shipped: &'static str,
    pub region: Region,
    /// Fingerprints of blocks earlier releases shipped in this file.
    ///
    /// Appended to at release time whenever the block changes, and never
    /// rewritten: the whole value is that a project running last year's
    /// skeleton is still recognised as one that nobody has edited. Empty means
    /// this block has only ever had the shape it has today.
    pub history: &'static [&'static str],
}

/// Empty now: the plan skeleton was the one entry here, and it went out with
/// the plan store — spoolway-plan writes its own self-contained page, from a
/// skeleton under the skill's own `assets/`, and nothing in the binary reads
/// a byte of it. Document skeletons were never here either; they are the
/// archivist's belongings, written once and never updated, so there is no
/// shipped version for a project's copy to have drifted from — see
/// `assets::Prompt::assets`.
///
/// Kept as a function, not deleted outright, so `update`'s machinery above
/// stays the general "keep a project's copy of a shipped block current"
/// mechanism it always was, ready for whatever next needs it — rather than
/// dead code deleted and then rewritten from scratch.
pub fn skeletons() -> Vec<Skeleton> {
    vec![]
}

impl Skeleton {
    /// The block as this binary ships it.
    pub fn block(&self) -> &'static str {
        self.region
            .read(self.shipped)
            .expect("a shipped skeleton carries its own block")
    }

    /// What `update` should do to the copy in `text`.
    pub fn state(&self, text: &str) -> BlockState {
        let Some(found) = self.region.read(text) else {
            return BlockState::Missing;
        };
        if same(found, self.block()) {
            return BlockState::Current;
        }
        let print = fingerprint(found);
        match self.history.contains(&print.as_str()) {
            true => BlockState::Stale,
            false => BlockState::HandEdited,
        }
    }
}

/// What the project's copy of a block turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum BlockState {
    /// The block we would write, whatever the styling around it looks like.
    Current,
    /// A block an earlier release shipped, unedited. Replaceable.
    Stale,
    /// Neither — somebody changed the part that is read by a machine.
    HandEdited,
    /// No block of this kind in the file at all.
    Missing,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whitespace an editor changed on the way past is not an edit — a file that
    /// made a round trip through an editor on Windows must not be reported as
    /// rewritten by somebody who never opened it.
    #[test]
    fn line_endings_and_trailing_space_are_not_an_edit() {
        assert!(same("a\nb\n", "a\r\nb  \r\n"));
        assert_eq!(fingerprint("a\nb"), fingerprint("a\r\nb  "));
    }

    #[test]
    fn different_text_is_a_different_fingerprint() {
        assert_ne!(fingerprint("a"), fingerprint("b"));
        assert!(!same("a", "b"));
    }

    /// `skeletons()` ships empty now, with nothing plan-specific left to
    /// keep current — see its own doc comment.
    #[test]
    fn skeletons_ships_empty() {
        assert!(skeletons().is_empty());
    }

    /// A `Skeleton` for the tests below, standing in for whatever the next
    /// one this machinery serves turns out to be — nothing here is plan
    /// content any more.
    fn fixture_skeleton() -> Skeleton {
        Skeleton {
            path: "fixture.html",
            shipped: "<h1>hi</h1>\n<script type=\"application/json\" id=\"fixture\">\n{ \"a\": 1 }\n</script>\n<p>bye</p>\n",
            region: Region::Script("fixture"),
            // The fingerprint of the block below, before `"a"` became `1` —
            // pinned the same way a real skeleton's `history` pins the
            // fingerprint of a block an earlier release shipped.
            history: &["3feb9975"],
        }
    }

    /// The block a shipped skeleton carries has to be findable by its own
    /// region locator, or `update` would find nothing to keep current in a
    /// file it wrote itself.
    #[test]
    fn a_shipped_skeleton_carries_a_block_we_can_find() {
        let skeleton = fixture_skeleton();
        let block = skeleton
            .region
            .read(skeleton.shipped)
            .unwrap_or_else(|| panic!("{} has no block", skeleton.path));
        assert!(!block.trim().is_empty());
        assert_eq!(skeleton.state(skeleton.shipped), BlockState::Current);
    }

    /// The point of the whole module: restyling is not an edit to the block, so
    /// a project that rewrote every line of CSS is still current.
    #[test]
    fn restyling_everything_around_the_block_leaves_it_current() {
        let skeleton = fixture_skeleton();
        let block = skeleton.block();
        let restyled = format!("<!-- mine -->\n{block}\n<style>body{{color:red}}</style>\n");
        assert_eq!(skeleton.state(&restyled), BlockState::Current);
    }

    /// And the converse: touching the block itself is the one thing that has to
    /// be noticed rather than overwritten.
    #[test]
    fn editing_the_block_is_noticed() {
        let skeleton = fixture_skeleton();
        let edited = skeleton.shipped.replace("\"a\": 1", "\"a\": 2");
        assert_eq!(skeleton.state(&edited), BlockState::HandEdited);
    }

    /// A block whose fingerprint is in `history` is stale, not hand-edited —
    /// the difference being that `update` replaces one and reports the
    /// other. A fingerprint nobody appended at the time is what turns every
    /// unedited project's skeleton into a file spoolway refuses to touch.
    #[test]
    fn a_block_in_history_reads_as_stale_rather_than_hand_edited() {
        let skeleton = fixture_skeleton();
        let old = skeleton.shipped.replace("\"a\": 1", "\"a\": 0");
        // The fixture's `history` pins the fingerprint of the block alone —
        // what `state` actually hashes — standing in for a release that
        // shipped `"a": 0` before today's `"a": 1`.
        let old_block = skeleton.region.read(&old).unwrap();
        assert_eq!(fingerprint(old_block), skeleton.history[0]);
        assert_eq!(skeleton.state(&old), BlockState::Stale);
    }

    /// A replace puts our block in and leaves the project's styling alone.
    #[test]
    fn a_replace_swaps_the_block_and_nothing_else() {
        let skeleton = fixture_skeleton();
        let theirs = skeleton.shipped.replace("\"a\": 1", "\"a\": 2") + "<!-- my footer -->\n";
        let next = skeleton
            .region
            .replace(&theirs, skeleton.block())
            .expect("a block to replace");

        assert_eq!(skeleton.state(&next), BlockState::Current);
        assert!(next.ends_with("<!-- my footer -->\n"), "{next}");
        assert!(!next.contains("\"a\": 2"));
    }

    /// The comment fence, on the file shape it exists for: a marked run of
    /// comments in the middle of a file whose every other line is the
    /// project's, found by whole lines rather than by substring.
    #[test]
    fn a_comment_region_takes_the_marked_lines_and_no_others() {
        let region = Region::Comment("# >>> ours >>>", "# <<< ours <<<");
        let text = "# theirs\n\n# >>> ours >>>\n# a key\n# <<< ours <<<\n\nsteps:\n";

        assert_eq!(
            region.read(text),
            Some("# >>> ours >>>\n# a key\n# <<< ours <<<")
        );
        assert_eq!(
            region.replace(text, "# >>> ours >>>\n# two keys\n# <<< ours <<<"),
            Some("# theirs\n\n# >>> ours >>>\n# two keys\n# <<< ours <<<\n\nsteps:\n".to_string())
        );
    }

    /// The offsets have to survive a file that made a round trip through an
    /// editor on Windows, or a replacement lands mid-word.
    #[test]
    fn a_comment_region_is_found_the_same_with_crlf_line_endings() {
        let region = Region::Comment("# >>> ours >>>", "# <<< ours <<<");
        let text = "# theirs\r\n# >>> ours >>>\r\n# a key\r\n# <<< ours <<<\r\nsteps:\r\n";

        let next = region
            .replace(text, "# >>> ours >>>\n# two keys\n# <<< ours <<<")
            .expect("a block to replace");
        assert!(next.starts_with("# theirs\r\n"), "{next:?}");
        assert!(next.ends_with("# <<< ours <<<\r\nsteps:\r\n"), "{next:?}");
        assert!(!next.contains("a key"), "{next:?}");
    }

    /// A start marker with no end is half a fence, and the lines under it could
    /// be anyone's — so nothing is found, and the caller says so rather than
    /// writing to the end of the file.
    #[test]
    fn a_comment_region_with_no_end_marker_is_not_found() {
        let region = Region::Comment("# >>> ours >>>", "# <<< ours <<<");
        assert_eq!(region.find("# >>> ours >>>\n# a key\nsteps:\n"), None);
        assert_eq!(region.find("steps:\n"), None);
    }

    /// A skeleton restyled so far that its block is gone is not one to guess
    /// about: nothing is written, and `--replace` is the only way back.
    #[test]
    fn a_file_with_no_block_reports_it_rather_than_writing_one() {
        assert_eq!(
            fixture_skeleton().state("<h1>restyled past recognition</h1>"),
            BlockState::Missing
        );
    }
}
