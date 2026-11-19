//! The picker bare `spoolway` opens when it starts in a main checkout with
//! nothing to read: no `.spoolway/`, no workspace and no `source` line, while
//! another worktree of the repository holds a `.spoolway/`.
//!
//! It runs on its own, before any project is discovered, so nothing in it can
//! reach code that expects a [`crate::repo::Repo`]. `main` writes the line the
//! pick names and then discovers the project as any start does, so the screen
//! that opens next is the ordinary one, reading the picked folder.
//!
//! It is the same [`super::picker::Picker`] every popup list uses, drawn in a
//! frame of its own under one blank row, since there is no tab under it yet.

use std::io::Write;
use std::path::PathBuf;

use super::picker::{Entry, Look, Picked, Picker};
use super::{PollableRead, RawStdin};

/// The box's title, as the mockup words it.
const TITLE: &str = "read the setup from";

/// How the list is drawn: the width and keys of every other pick-one list,
/// so this popup reads as one of them.
const LOOK: Look = Look {
    width: 44,
    column: 30,
    keys: &[("enter", "confirm"), ("f", "search"), ("esc", "back")],
    noun: ("folder", "folders"),
    empty: "no folder to pick",
};

/// Open the picker over `folders` on the terminal, and answer the one
/// picked, or `None` for `esc`, `ctrl-c` or input that ran out.
///
/// Installs the `ctrl-c` handler before taking the terminal, for the reason
/// [`super::shell::run`] gives. The terminal is given back before this
/// returns, so the screen that opens next takes it afresh.
pub(crate) fn pick(folders: &[PathBuf]) -> Option<PathBuf> {
    crate::platform::stop::catch_interrupt();
    let _term = crate::platform::TermGuard::screen();
    choose(folders, &mut RawStdin, &mut std::io::stdout())
}

/// The picker's loop, apart from the terminal so a test can drive it over a
/// scripted input.
fn choose(
    folders: &[PathBuf],
    input: &mut impl PollableRead,
    out: &mut impl Write,
) -> Option<PathBuf> {
    let entries = folders
        .iter()
        .map(|folder| Entry {
            label: crate::repo::shorten_home(folder),
            column: None,
            value: folder.clone(),
        })
        .collect();
    let mut picker = Picker::new(TITLE, entries, LOOK);
    let mut writer = super::frame_writer::FrameWriter::new();
    loop {
        writer.write_frame(&frame(&picker), super::pane_size(), out);
        let key = super::shell::wait_key(input, || {})?;
        match picker.handle_key(key) {
            Picked::Open(open) => picker = *open,
            Picked::Closed => return None,
            Picked::Chosen(folder) => return Some(folder),
        }
    }
}

/// One blank row, then the popup. The list is cut to the terminal's height:
/// [`Picker::popup`] draws a box two rows taller than the rows it is given,
/// and the blank row above it and the row the cursor's last newline leaves
/// at the foot take one each, so it is given three fewer than the terminal
/// has. With no terminal to measure it draws its usual number of rows.
fn frame(picker: &Picker<PathBuf>) -> Vec<String> {
    let rows = super::drawing_area().map(|(_, height)| height.saturating_sub(3));
    let mut frame = vec![String::new()];
    frame.extend(picker.popup(rows));
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::shell::plain;

    /// The frames [`choose`] draws over `keys`, colour codes taken out, and
    /// what it answered.
    fn drive(folders: &[PathBuf], keys: &str) -> (Vec<String>, Option<PathBuf>) {
        let mut input = std::io::Cursor::new(keys.as_bytes().to_vec());
        let mut out = Vec::new();
        let picked = choose(folders, &mut input, &mut out);
        let drawn = String::from_utf8(out).unwrap();
        let frames = crate::screen::spool::testutil::tab_frames(&drawn)
            .iter()
            .map(|frame| plain(frame))
            .collect();
        (frames, picked)
    }

    fn folders() -> Vec<PathBuf> {
        let home = crate::platform::home_dir().unwrap();
        vec![home.join("dev/app-adopt"), home.join("dev/app-limits")]
    }

    /// Each row of a frame, the frame writer's indent and the popup's
    /// trailing padding left on, so the box reads as drawn.
    fn rows(frame: &str) -> Vec<String> {
        frame
            .lines()
            .map(|line| line.strip_prefix(' ').unwrap_or(line).to_string())
            .collect()
    }

    // The picker opens on the box the mockup draws: its title, the search
    // row, each folder under home in its `~` form with the cursor on the
    // first, and the pick-one keys.
    #[test]
    fn the_picker_draws_the_folders_in_the_mockups_box() {
        let (frames, _) = drive(&folders(), "");
        assert_eq!(
            rows(&frames[0])[..9],
            [
                "",
                "╭─ read the setup from ──────────────────────────╮",
                "│  search: ▏                                     │",
                "│                                                │",
                "│  > ~/dev/app-adopt                             │",
                "│    ~/dev/app-limits                            │",
                "│                                                │",
                "│  [enter] confirm   [f] search   [esc] back     │",
                "╰────────────────────────────────────────────────╯",
            ]
        );
    }

    // `enter` answers the folder under the cursor, `↓` moves it, and `esc`
    // answers none.
    #[test]
    fn enter_picks_the_folder_under_the_cursor_and_esc_none() {
        let folders = folders();
        assert_eq!(drive(&folders, "\r").1.as_ref(), Some(&folders[0]));
        assert_eq!(drive(&folders, "\x1b[B\r").1.as_ref(), Some(&folders[1]));
        assert_eq!(drive(&folders, "\x1b").1, None);
    }

    // `f` searches the list down, and `enter` then picks what the search
    // left.
    #[test]
    fn a_search_narrows_the_list_before_enter_picks() {
        let folders = folders();
        let (_, picked) = drive(&folders, "flimits\r\r");
        assert_eq!(picked.as_ref(), Some(&folders[1]));
    }
}
