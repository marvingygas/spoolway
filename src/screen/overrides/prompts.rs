//! The overrides tab's prompts section: every tracked prompt, and `enter`
//! to copy one into the layer.
//!
//! A prompt override replaces the whole `PROMPT.md`, so a one-line field
//! cannot hold it, and the board starts no editor nobody asked for. `enter`
//! writes the same copy `spoolway prompt override` writes and says where it
//! is; `w` opens the folder it is in. A prompt the layer already holds is
//! never written over, since that copy may carry a person's edits.

use std::collections::BTreeSet;

use super::{SEARCH_KEYS, edit, one_line};
use crate::repo::Repo;
use crate::screen::{Key, Notice, keys, pad_to};

/// What has the section's keys.
enum View {
    List,
    /// `f` was pressed and the query is being typed.
    Search,
    /// A copy was written, until `enter` or `w` closes it. Holds the copy's
    /// path under the project home.
    Wrote(String),
    /// The layer already holds a copy, until `enter` or `w` closes it.
    Exists(String),
    /// A copy that failed to write, until `enter` closes it.
    Failed(Notice),
}

/// The prompts section while the tab is open. Built fresh on every visit.
pub(super) struct PromptsSection {
    /// Every prompt the checkout's `.spoolway/prompts/` holds, sorted.
    names: Vec<String>,
    /// The prompts the layer holds a copy of.
    overridden: BTreeSet<String>,
    /// Why the list is empty because the prompts did not read, drawn at the
    /// top of the box.
    pub(super) stale: Option<String>,
    view: View,
    /// Which prompt the cursor is on, among the ones the search leaves.
    cursor: usize,
    /// The search query, kept after `enter` leaves the search row.
    filter: String,
}

/// The column `overridden` starts at, counted from the prompt's name: the
/// column the mockup draws it at. A longer name widens it for every row, so
/// the marks still line up.
const PROMPT_NAME: usize = 32;

/// What the list says beside a prompt the layer holds.
const OVERRIDDEN: &str = "overridden";

/// The keys of the popups `enter` opens.
const POPUP_KEYS: &[(&str, &str)] = &[("enter", "close"), ("w", "open workspace")];

impl PromptsSection {
    pub(super) fn new() -> PromptsSection {
        PromptsSection {
            names: Vec::new(),
            overridden: BTreeSet::new(),
            stale: None,
            view: View::List,
            cursor: 0,
            filter: String::new(),
        }
    }

    /// Read the tracked prompts and the layer again, after a write or on a
    /// visit.
    pub(super) fn reload(&mut self, repo: &Repo) {
        match crate::prompt::tracked_names_in(&repo.checkout) {
            Ok(names) => {
                self.names = names.into_iter().collect();
                self.stale = None;
            }
            Err(err) => {
                self.names = Vec::new();
                self.stale = Some(format!(
                    "the prompts in .spoolway/prompts/ do not read: {}. fix the folder, then \
                     leave the overrides tab and open it again.",
                    one_line(&err)
                ));
            }
        }
        // A layer that does not read lists nothing as overridden, the way
        // the tab's `active` count reads it; `override list` reports why.
        self.overridden = crate::overrides::list_prompt_overrides(&repo.overrides_dir())
            .unwrap_or_default()
            .into_iter()
            .collect();
        self.clamp();
    }

    /// Whether the list has the keys, with nothing open over it: the only
    /// time `tab`, `q` and `←`/`→` reach past the section.
    pub(super) fn at_rest(&self) -> bool {
        matches!(self.view, View::List)
    }

    /// Whether `w` opens the overrides folder now: over the list, or in
    /// either popup `enter` opens, which it closes first. Not while a query
    /// is typed, where `w` is a letter of it, nor over a failed write's
    /// notice, which only `enter` closes.
    pub(super) fn take_w(&mut self) -> bool {
        match self.view {
            View::List => true,
            View::Wrote(_) | View::Exists(_) => {
                self.view = View::List;
                true
            }
            View::Search | View::Failed(_) => false,
        }
    }

    /// The prompts the search leaves, in list order. A prompt matches on its
    /// name, or on `overridden` when the layer holds it.
    fn matches(&self) -> Vec<&str> {
        let query = self.filter.to_lowercase();
        self.names
            .iter()
            .filter(|name| {
                query.is_empty()
                    || name.to_lowercase().contains(&query)
                    || (self.overridden.contains(*name) && OVERRIDDEN.contains(&query))
            })
            .map(String::as_str)
            .collect()
    }

    fn clamp(&mut self) {
        self.cursor = self.cursor.min(self.matches().len().saturating_sub(1));
    }

    fn move_cursor(&mut self, key: Key) {
        self.cursor = crate::screen::move_cursor(self.cursor, self.matches().len(), key);
    }

    /// One key, read by whatever the section shows. Answers whether the
    /// layer was written, so the tab can count its keys again.
    pub(super) fn handle_key(&mut self, repo: &Repo, key: Key) -> bool {
        match std::mem::replace(&mut self.view, View::List) {
            View::List => match key {
                Key::Up | Key::Down => self.move_cursor(key),
                Key::Char('f') => self.view = View::Search,
                Key::Enter => {
                    if let Some(name) = self.matches().get(self.cursor).map(|n| n.to_string()) {
                        return self.override_prompt(repo, &name);
                    }
                }
                _ => {}
            },
            View::Search => {
                match key {
                    Key::Enter => return false,
                    Key::Esc => self.filter.clear(),
                    Key::Up | Key::Down => self.move_cursor(key),
                    other => edit(&mut self.filter, other),
                }
                self.clamp();
                if key != Key::Esc {
                    self.view = View::Search;
                }
            }
            View::Wrote(path) => {
                if key != Key::Enter {
                    self.view = View::Wrote(path);
                }
            }
            View::Exists(path) => {
                if key != Key::Enter {
                    self.view = View::Exists(path);
                }
            }
            View::Failed(notice) => {
                if key != Key::Enter {
                    self.view = View::Failed(notice);
                }
            }
        }
        false
    }

    /// Copy `name` into the layer, unless the layer already holds a copy,
    /// and open the popup saying which. Read off the disk rather than the
    /// list, which may be older than a copy another command just wrote.
    fn override_prompt(&mut self, repo: &Repo, name: &str) -> bool {
        let path = crate::overrides::prompt_patch_path(&repo.overrides_dir(), name);
        if path.is_file() {
            self.view = View::Exists(shown(repo, &path));
            return false;
        }
        match crate::commands::write_prompt_override(repo, name) {
            Ok(write) => {
                self.view = View::Wrote(shown(repo, &write.path));
                true
            }
            Err(err) => {
                self.view = View::Failed(Notice::new(
                    "not written",
                    format!(
                        "prompt {name} was not copied into the overrides layer: {err:#}. \
                         nothing was written; fix what stopped it and press enter on the \
                         prompt again."
                    ),
                ));
                false
            }
        }
    }

    /// The list box's lines, `width` columns each: the search row while a
    /// query is typed, a blank row, then every prompt the search leaves with
    /// `overridden` beside those the layer holds, cut to `rows` with the
    /// cursor's row kept in view.
    pub(super) fn lines(&self, rows: Option<usize>, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        if matches!(self.view, View::Search) {
            lines.push(format!("search: {}▏", self.filter));
        }
        lines.push(String::new());
        let matches = self.matches();
        if matches.is_empty() {
            lines.push(match self.filter.is_empty() {
                true => "  no prompts in .spoolway/prompts/".to_string(),
                false => "  no prompts match this search".to_string(),
            });
            return lines;
        }
        let name_field = matches
            .iter()
            .map(|name| name.chars().count() + 2)
            .max()
            .unwrap_or(0)
            .max(PROMPT_NAME);
        let mut starts = Vec::new();
        for (at, name) in matches.iter().enumerate() {
            let mark = if at == self.cursor { "> " } else { "  " };
            let overridden = match self.overridden.contains(*name) {
                true => OVERRIDDEN,
                false => "",
            };
            starts.push(lines.len());
            lines.push(
                format!("{mark}{name:name_field$}{overridden}")
                    .trim_end()
                    .to_string(),
            );
        }
        let focus = starts[self.cursor.min(starts.len() - 1)];
        crate::screen::pane::window(
            &lines,
            crate::screen::pane::Items {
                starts: &starts,
                noun: "prompt",
            },
            (focus, focus),
            rows,
            width,
        )
    }

    /// The popup over the list, if one is open.
    pub(super) fn popup(&self) -> Option<Vec<String>> {
        let lines = match &self.view {
            View::Wrote(path) => vec![format!("wrote {path}")],
            View::Exists(path) => vec![path.clone(), "already exists, nothing written".to_string()],
            View::Failed(notice) => return Some(notice.panel(crate::screen::NOTICE_WRAP)),
            View::List | View::Search => return None,
        };
        let mut body = vec![String::new()];
        for line in lines {
            body.extend(
                crate::screen::wrap(&line, crate::screen::NOTICE_WRAP)
                    .iter()
                    .map(|line| pad_to(line, super::POPUP_TEXT)),
            );
        }
        Some(crate::screen::panel(
            "prompt override",
            &body,
            &keys(POPUP_KEYS),
        ))
    }

    /// The key line under the frame: whatever the section shows names its
    /// own keys.
    pub(super) fn footer(&self) -> Vec<(&'static str, &'static str)> {
        match &self.view {
            View::List => [
                [
                    ("enter", "override"),
                    ("f", "search"),
                    super::OPEN_WORKSPACE,
                    ("tab", "section"),
                ]
                .as_slice(),
                crate::screen::shell::quit_hint(),
            ]
            .concat(),
            View::Search => SEARCH_KEYS.to_vec(),
            View::Wrote(_) | View::Exists(_) => POPUP_KEYS.to_vec(),
            View::Failed(_) => vec![("enter", "confirm")],
        }
    }
}

/// `path` as the popups show it: under the project home, where the
/// `overrides:` line of `spoolway config path` points, so it reads
/// `overrides/prompts/<name>/PROMPT.md` however long the home's own path is.
fn shown(repo: &Repo, path: &std::path::Path) -> String {
    path.strip_prefix(&repo.home)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{lines, project, type_keys};
    use super::super::{Mode, Section, State};
    use crate::screen::shell;

    /// A project whose `.spoolway/prompts/` holds `names`, each prompt's
    /// text its own name, with the tab open on the prompts section.
    fn prompts_project(
        name: &str,
        names: &[&str],
    ) -> (crate::repo::Repo, crate::scratch::ScratchRoot, State) {
        let (repo, guard) = project(name);
        for name in names {
            let path = crate::prompt::directory_form(&repo, name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("{name}\n")).unwrap();
        }
        let mut state = State::new(&repo);
        type_keys(&repo, &mut state, "\t\t");
        assert_eq!(state.section, Section::Prompts);
        (repo, guard, state)
    }

    /// The layer's copy of prompt `name`, as written.
    fn copy(repo: &crate::repo::Repo, name: &str) -> std::path::PathBuf {
        crate::overrides::prompt_patch_path(&repo.overrides_dir(), name)
    }

    /// The popup's rows with the colour of its key row taken out.
    fn popup(state: &State) -> Vec<String> {
        state
            .prompts
            .popup()
            .unwrap()
            .iter()
            .map(|l| shell::plain(l))
            .collect()
    }

    // The section lists every tracked prompt in name order, with
    // `overridden` at the mockup's column beside those the layer holds.
    #[test]
    fn the_section_lists_every_prompt_with_overridden_beside_the_layers() {
        let (repo, _guard, mut state) = prompts_project(
            "prompts-list",
            &["implementer", "archivist", "code-reviewer"],
        );
        let held = copy(&repo, "code-reviewer");
        std::fs::create_dir_all(held.parent().unwrap()).unwrap();
        std::fs::write(&held, "mine\n").unwrap();
        state.reload(&repo);
        type_keys(&repo, &mut state, "\x1b[B\x1b[B");

        let drawn = lines(&state);
        let row = |text: &str| format!("│ {text:<77}│");
        assert_eq!(
            drawn[0],
            format!(
                "╭─ config   pipeline   [prompts]   active (1) {}╮",
                "─".repeat(33)
            )
        );
        assert_eq!(drawn[1], row(""));
        assert_eq!(drawn[2], row("  archivist"));
        assert_eq!(
            drawn[3],
            row(&format!("  {:32}overridden", "code-reviewer"))
        );
        assert_eq!(drawn[4], row("> implementer"));
        assert_eq!(
            drawn.last().unwrap(),
            " [enter] override   [f] search   [w] open workspace   [tab] section"
        );
    }

    // `f` narrows the list on a prompt's name, and on `overridden` for the
    // prompts the layer holds; `esc` brings every prompt back.
    #[test]
    fn f_searches_names_and_overridden() {
        let (repo, _guard, mut state) = prompts_project(
            "prompts-search",
            &["implementer", "archivist", "code-reviewer"],
        );
        let held = copy(&repo, "archivist");
        std::fs::create_dir_all(held.parent().unwrap()).unwrap();
        std::fs::write(&held, "mine\n").unwrap();
        state.reload(&repo);

        type_keys(&repo, &mut state, "freview");
        assert_eq!(state.prompts.matches(), ["code-reviewer"]);
        assert!(lines(&state)[1].starts_with("│ search: review▏"));
        assert_eq!(
            lines(&state).last().unwrap(),
            " [enter] leave search   [esc] cancel"
        );
        type_keys(&repo, &mut state, "\x1b");
        type_keys(&repo, &mut state, "foverrid\r");
        assert_eq!(state.prompts.matches(), ["archivist"]);
        type_keys(&repo, &mut state, "fzzz");
        assert!(
            lines(&state)
                .iter()
                .any(|l| l.contains("no prompts match this search"))
        );
        type_keys(&repo, &mut state, "\x1b");
        assert_eq!(state.prompts.matches().len(), 3);
    }

    // `enter` on a prompt the layer does not hold writes the copy
    // `prompt override` writes, and the popup says where it is, as the
    // mockup draws it. `enter` closes it onto the list, which now reads
    // `overridden`.
    #[test]
    fn enter_writes_the_copy_and_says_where_it_is() {
        let (repo, _guard, mut state) = prompts_project("prompts-write", &["implementer"]);
        type_keys(&repo, &mut state, "\r");
        assert_eq!(
            std::fs::read_to_string(copy(&repo, "implementer")).unwrap(),
            "implementer\n"
        );
        assert_eq!(
            popup(&state),
            [
                "╭─ prompt override ────────────────────────────────╮",
                "│                                                  │",
                "│  wrote overrides/prompts/implementer/PROMPT.md   │",
                "│                                                  │",
                "│  [enter] close   [w] open workspace              │",
                "╰──────────────────────────────────────────────────╯",
            ]
        );
        assert_eq!(
            lines(&state).last().unwrap(),
            " [enter] close   [w] open workspace"
        );
        assert_eq!(state.active, 1, "the tab counts the new copy");

        type_keys(&repo, &mut state, "x\x1b");
        assert!(state.prompts.popup().is_some(), "only enter or w closes it");
        type_keys(&repo, &mut state, "\r");
        assert!(state.prompts.popup().is_none());
        assert!(
            lines(&state)[2].contains("overridden"),
            "{:?}",
            lines(&state)
        );
    }

    // `enter` on a prompt the layer already holds writes nothing, so a copy
    // a person has edited is kept, and the popup says so.
    #[test]
    fn enter_on_an_overridden_prompt_writes_nothing() {
        let (repo, _guard, mut state) = prompts_project("prompts-exists", &["code-reviewer"]);
        let held = copy(&repo, "code-reviewer");
        std::fs::create_dir_all(held.parent().unwrap()).unwrap();
        std::fs::write(&held, "my edits\n").unwrap();
        state.reload(&repo);

        type_keys(&repo, &mut state, "\r");
        assert_eq!(std::fs::read_to_string(&held).unwrap(), "my edits\n");
        assert_eq!(
            popup(&state),
            [
                "╭─ prompt override ────────────────────────────────╮",
                "│                                                  │",
                "│  overrides/prompts/code-reviewer/PROMPT.md       │",
                "│  already exists, nothing written                 │",
                "│                                                  │",
                "│  [enter] close   [w] open workspace              │",
                "╰──────────────────────────────────────────────────╯",
            ]
        );
    }

    // A prompts folder that does not read is named above the empty list,
    // and the note's advice works: once the folder is fixed, opening the
    // tab again lists the prompts.
    #[test]
    fn a_prompts_folder_that_does_not_read_is_said_above_the_list() {
        let (repo, _guard) = project("prompts-stale");
        let folder = repo.prompts_dir();
        std::fs::create_dir_all(folder.parent().unwrap()).unwrap();
        std::fs::write(&folder, "").unwrap();
        let mut state = State::new(&repo);
        type_keys(&repo, &mut state, "\t\t");
        let note = lines(&state)
            .iter()
            .map(|l| l.trim_matches('│').trim().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            note.contains("the prompts in .spoolway/prompts/ do not read"),
            "{note}"
        );
        assert!(
            note.contains("leave the overrides tab and open it again"),
            "{note}"
        );

        std::fs::remove_file(&folder).unwrap();
        let path = crate::prompt::directory_form(&repo, "implementer");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "implementer\n").unwrap();
        let reopened = State::new(&repo);
        assert_eq!(reopened.prompts.stale, None);
        assert_eq!(reopened.prompts.matches(), ["implementer"]);
    }

    // A copy that cannot be written is held as a notice naming the prompt,
    // with nothing left in the layer.
    #[test]
    fn a_copy_that_fails_says_so_until_enter() {
        let (repo, _guard, mut state) = prompts_project("prompts-fails", &["implementer"]);
        // A file where the layer's `prompts/` folder goes stops the write.
        let blocker = repo.overrides_dir().join("prompts");
        std::fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        std::fs::write(&blocker, "").unwrap();

        type_keys(&repo, &mut state, "\r");
        let drawn = popup(&state).join("\n");
        assert!(drawn.contains("╭─ not written ─"), "{drawn}");
        assert!(
            drawn.contains("prompt implementer was not copied into the overrides"),
            "{drawn}"
        );
        assert_eq!(lines(&state).last().unwrap(), " [enter] confirm");
        type_keys(&repo, &mut state, "w");
        assert!(state.prompts.popup().is_some(), "w does not close a notice");
        assert!(crate::browser::opened().is_empty());
        type_keys(&repo, &mut state, "\r");
        assert!(state.prompts.popup().is_none());
        assert!(matches!(state.mode, Mode::Browsing));
    }
}
