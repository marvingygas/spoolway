//! The overrides tab's active section: everything the layer holds, file by
//! file with its keys under it, and `o` to delete any of it.
//!
//! Each key shows the setup's value and the layer's, the reason when the
//! merge ignores it, or `matches the setup` when the two are the same. The
//! last is how a promoted entry shows that its branch has merged: a promote
//! keeps its entries in the layer, and once the setup holds the same value
//! the entry does nothing and can go. The setup is read from the main
//! checkout, the one the dispatcher reads, never from a worktree's copy.
//!
//! A delete names every file and key in one confirm popup first. Each file
//! is changed in one step, one write or its removal, so a delete that stops
//! partway leaves whole files changed or not, and the notice names the ones
//! already changed.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use super::pipeline::{column, fit_blocks, step_value, tick};
use super::{POPUP_TEXT, SET_KEYS, edit, field, one_line, shown};
use crate::config::Config;
use crate::pipeline::Pipelines;
use crate::repo::Repo;
use crate::screen::{Key, Notice, keys, pad_to, plural};

/// What a key in the layer does to the setup.
#[derive(Debug, Clone)]
enum KeyState {
    /// The merge applies it: the setup's value, then the layer's.
    Changed { old: String, new: String },
    /// The setup already holds the same value, so the key does nothing.
    Matches,
    /// The merge leaves it out, for the reason `override list` prints.
    Ignored(String),
}

/// One key of a file in the layer.
#[derive(Debug, Clone)]
struct KeyRow {
    /// The key as `override list` prints it: a dotted config key,
    /// `<step>.<field>`, `description` or `task_template`.
    key: String,
    /// The key `enter` sets: a config key as the merge applies it, which
    /// differs from `key` only for a retired spelling the merge still reads.
    set_as: String,
    /// The value in use now, which `enter`'s field opens on: the layer's
    /// when the merge applies it, the setup's when it does not.
    in_use: String,
    state: KeyState,
}

/// What kind of file a row is, and so how a key in it is set again.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Config,
    Pipeline(String),
    Prompt,
}

/// One file in the layer.
#[derive(Debug, Clone)]
struct FileRow {
    /// The file as `override list` prints it, such as `pipelines/impl.yml`.
    target: String,
    kind: Kind,
    /// What the file's own row says: `whole file` for a prompt, why the
    /// merge ignores the whole file, or why it does not read.
    note: Option<String>,
    keys: Vec<KeyRow>,
}

/// One row of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// A file, by its index.
    File(usize),
    /// A key, by its file's index and its own.
    Key(usize, usize),
}

/// One thing a person can select: a key of a file, or a file with no keys,
/// such as a prompt, as a whole.
type Item = (String, Option<String>);

/// What `enter` and `o` lead to that the tab itself has to act on.
pub(super) enum Outcome {
    Nothing,
    /// The layer changed, so the tab reads everything again.
    Changed,
    /// `enter` on a config key: the tab opens its own set popup on it.
    SetConfig(String),
}

/// One file a [`Deletion`] changes.
#[derive(Debug, Clone)]
struct Doomed {
    target: String,
    /// The keys to remove. When `whole`, the keys the confirm popup lists
    /// for a file that goes whatever it holds.
    keys: Vec<String>,
    /// Whether the file goes as a whole, whatever it holds.
    whole: bool,
}

/// A delete waiting on its confirm popup: every file and key it removes.
#[derive(Debug, Clone)]
pub(super) struct Deletion {
    title: &'static str,
    files: Vec<Doomed>,
}

impl Deletion {
    /// `delete override` on a config key: every leaf of the layer's
    /// `config.toml` that sets it, which is two when the file holds a
    /// retired spelling beside the current one. `None` when none does.
    pub(super) fn config_key(repo: &Repo, key: &str) -> Option<Deletion> {
        let leaves = crate::overrides::config_patch_leaves_for(&repo.overrides_dir(), key).ok()?;
        (!leaves.is_empty()).then(|| {
            Deletion::of(vec![Doomed {
                target: crate::config::CONFIG_FILE.to_string(),
                keys: leaves,
                whole: false,
            }])
        })
    }

    /// `delete override` on a pipeline field: the field on every step of
    /// every pipeline patch that sets it. A patch that does not read is
    /// left out; the pipeline section names it above its list. `None` when
    /// no patch sets the field.
    pub(super) fn pipeline_field(repo: &Repo, name: &str) -> Option<Deletion> {
        let overrides = repo.overrides_dir();
        let files: Vec<Doomed> = crate::overrides::list_pipeline_patches(&overrides)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|pipeline| {
                let patch = crate::overrides::read_pipeline_patch(&overrides, &pipeline)
                    .ok()
                    .flatten()?;
                let keys: Vec<String> = patch
                    .steps
                    .iter()
                    .filter(|(_, fields)| fields.contains_key(name))
                    .map(|(step, _)| format!("{step}.{name}"))
                    .collect();
                (!keys.is_empty()).then(|| Doomed {
                    target: format!("pipelines/{pipeline}.yml"),
                    keys,
                    whole: false,
                })
            })
            .collect();
        (!files.is_empty()).then(|| Deletion::of(files))
    }

    /// A delete of `files`, its title saying whether it is one key or more.
    fn of(files: Vec<Doomed>) -> Deletion {
        let title = match count_keys(&files) {
            1 => "delete this override",
            _ => "delete these overrides",
        };
        Deletion { title, files }
    }

    /// The confirm popup: how many keys in how many files, then each file
    /// with its keys under one another, cut to the `rows` the list box has.
    pub(super) fn popup(&self, rows: Option<usize>) -> Vec<String> {
        let label = self
            .files
            .iter()
            .map(|file| file.target.chars().count() + TARGET_GAP)
            .max()
            .unwrap_or(0);
        let mut body = vec![
            String::new(),
            format!(
                "{} in {}",
                plural(count_keys(&self.files), "key"),
                plural(self.files.len(), "file")
            ),
            String::new(),
        ];
        let blocks: Vec<Vec<String>> = self
            .files
            .iter()
            .map(|file| {
                let keys = match file.keys.is_empty() {
                    true => vec![WHOLE_FILE.to_string()],
                    false => file.keys.clone(),
                };
                keys.iter()
                    .enumerate()
                    .map(|(at, key)| {
                        let head = if at == 0 { file.target.as_str() } else { "" };
                        let line = format!("{head:label$}{key}");
                        pad_to(&line, POPUP_TEXT.max(line.chars().count()))
                    })
                    .collect()
            })
            .collect();
        // The rows the popup's body may take, as the pipeline section's
        // value popup counts them: the box's two borders, less the row
        // `overlay` keeps above a popup and the popup's own two borders,
        // blank row and key row.
        let room = rows.map(|rows| (rows + 2).saturating_sub(1 + 4));
        body.extend(fit_blocks(
            blocks,
            room.map(|room| room.saturating_sub(body.len())),
            label,
            "file",
        ));
        crate::screen::ask(self.title, &body, &["delete".to_string()], 0)
    }

    /// Delete every file in turn, each in one step. A failure stops at
    /// that file, and the notice names the files already changed and the
    /// ones not, so nobody reads a delete that stopped partway as undone.
    pub(super) fn run(&self, repo: &Repo) -> std::result::Result<(), Notice> {
        for (at, file) in self.files.iter().enumerate() {
            let result = match file.whole || file.keys.is_empty() {
                true => crate::commands::drop_override_target(repo, &file.target),
                false => crate::commands::drop_override_keys(repo, &file.target, &file.keys),
            };
            let Err(err) = result else { continue };
            let names = |files: &[Doomed]| {
                files
                    .iter()
                    .map(|file| file.target.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let (done, left) = self.files.split_at(at);
            return Err(match done.is_empty() {
                true => Notice::new(
                    "not deleted",
                    format!(
                        "nothing was deleted: {err:#}. fix what stopped it and delete the \
                         overrides again."
                    ),
                ),
                false => Notice::new(
                    "deleted in part",
                    format!(
                        "the overrides in {} were deleted, but not those in {}: {err:#}. fix \
                         what stopped it and delete what is left again.",
                        names(done),
                        names(left)
                    ),
                ),
            });
        }
        Ok(())
    }
}

/// How many keys `files` holds, a file with none named counting as one, as
/// the box title's `active` count counts a prompt.
fn count_keys(files: &[Doomed]) -> usize {
    files.iter().map(|file| file.keys.len().max(1)).sum()
}

/// What has the section's keys.
enum View {
    List,
    /// `o`'s menu, its cursor on the `n`th item.
    Menu(usize),
    /// A delete's confirm popup.
    Confirm(Deletion),
    /// The value field over a pipeline key, opened by `enter`.
    Set {
        pipeline: String,
        key: String,
        current: String,
        value: String,
    },
    /// A delete or a write that failed, until `enter` closes it.
    Failed(Notice),
}

/// The active section while the tab is open. Built fresh on every visit.
pub(super) struct ActiveSection {
    files: Vec<FileRow>,
    /// Why no key is marked ignored, when `override list`'s own read of the
    /// layer fails, drawn at the top of the box.
    pub(super) stale: Option<String>,
    view: View,
    /// Which of [`ActiveSection::rows`] the cursor is on.
    cursor: usize,
    selected: BTreeSet<Item>,
}

/// What a prompt's row says: a prompt override replaces the whole file.
const WHOLE_FILE: &str = "whole file";

/// What a key the setup already holds says.
const MATCHES: &str = "matches the setup — o to delete";

/// The row under an ignored key's reason.
const SET_AGAIN: &str = "[enter] to set it again, or o to delete";

/// The column a file's note starts at, counted from its name, and the one a
/// key's value starts at, counted from the key: both land on the same
/// column, so every value in the list lines up under the others. A longer
/// name pushes only its own value along, one space past its end.
const FILE_FIELD: usize = 31;
const KEY_FIELD: usize = 25;

/// The columns between a file and its first key in the confirm popup.
const TARGET_GAP: usize = 3;

/// The menu's items, padded so the menu is 40 columns wide whatever it
/// lists, the same width with or without a selection.
const MENU: [&str; 2] = ["delete selected", "delete all overrides"];
const MENU_TEXT: usize = 32;

/// The keys of the menu and the confirm popup, on the line under the frame.
const ANSWER_KEYS: &[(&str, &str)] = &[("enter", "confirm"), ("esc", "close")];

impl ActiveSection {
    pub(super) fn new() -> ActiveSection {
        ActiveSection {
            files: Vec::new(),
            stale: None,
            view: View::List,
            cursor: 0,
            selected: BTreeSet::new(),
        }
    }

    /// Read the layer and the setup again, after a write or on a visit,
    /// with `config` the config in use now. A selection whose key has gone
    /// is dropped.
    pub(super) fn reload(&mut self, repo: &Repo, config: &Config) {
        let (files, stale) = load(repo, config);
        self.files = files;
        self.stale = stale;
        let items: BTreeSet<Item> = self.every_item().into_iter().collect();
        self.selected.retain(|item| items.contains(item));
        self.cursor = self.cursor.min(self.rows().len().saturating_sub(1));
    }

    /// Whether the list has the keys, with nothing open over it: the only
    /// time `tab`, `q`, `w` and `←`/`→` reach past the section.
    pub(super) fn at_rest(&self) -> bool {
        matches!(self.view, View::List)
    }

    /// Every file and key, in list order.
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (f, file) in self.files.iter().enumerate() {
            rows.push(Row::File(f));
            rows.extend((0..file.keys.len()).map(|k| Row::Key(f, k)));
        }
        rows
    }

    /// What selecting `row` selects: every key of a file, a file with no
    /// keys as itself, or one key.
    fn items(&self, row: Row) -> Vec<Item> {
        match row {
            Row::File(f) => {
                let file = &self.files[f];
                match file.keys.is_empty() {
                    true => vec![(file.target.clone(), None)],
                    false => file
                        .keys
                        .iter()
                        .map(|key| (file.target.clone(), Some(key.key.clone())))
                        .collect(),
                }
            }
            Row::Key(f, k) => {
                let file = &self.files[f];
                vec![(file.target.clone(), Some(file.keys[k].key.clone()))]
            }
        }
    }

    fn every_item(&self) -> Vec<Item> {
        (0..self.files.len())
            .flat_map(|f| self.items(Row::File(f)))
            .collect()
    }

    fn is_selected(&self, row: Row) -> bool {
        self.items(row)
            .iter()
            .all(|item| self.selected.contains(item))
    }

    /// One key, read by whatever the section shows.
    pub(super) fn handle_key(&mut self, repo: &Repo, key: Key) -> Outcome {
        match std::mem::replace(&mut self.view, View::List) {
            View::List => return self.list_key(key),
            View::Menu(cursor) => match key {
                Key::Up | Key::Down => {
                    let len = self.menu_items().len();
                    self.view = View::Menu(crate::screen::move_cursor(cursor, len, key));
                }
                Key::Enter => {
                    let deletion = match cursor {
                        0 => self.selected_deletion(),
                        _ => self.whole_deletion(),
                    };
                    if let Some(deletion) = deletion {
                        self.view = View::Confirm(deletion);
                    }
                }
                Key::Esc => {}
                _ => self.view = View::Menu(cursor),
            },
            View::Confirm(deletion) => match key {
                Key::Enter => {
                    if let Err(notice) = deletion.run(repo) {
                        self.view = View::Failed(notice);
                    } else {
                        self.selected.clear();
                    }
                    return Outcome::Changed;
                }
                Key::Esc => {}
                _ => self.view = View::Confirm(deletion),
            },
            View::Set {
                pipeline,
                key: name,
                current,
                value,
            } => match key {
                Key::Esc => {}
                // `enter` on an empty field is how the popup is left without
                // a write, as on the config section.
                Key::Enter if value.trim().is_empty() => {}
                Key::Enter => {
                    if let Err(err) = write_pipeline_key(repo, &pipeline, &name, &value) {
                        self.view = View::Failed(Notice::new(
                            "not written",
                            format!(
                                "{name} on pipeline {pipeline} was not written to the overrides \
                                 layer: {err:#}. nothing changed; fix what stopped it and set \
                                 the key again."
                            ),
                        ));
                        return Outcome::Nothing;
                    }
                    return Outcome::Changed;
                }
                other => {
                    let mut value = value;
                    edit(&mut value, other);
                    self.view = View::Set {
                        pipeline,
                        key: name,
                        current,
                        value,
                    };
                }
            },
            View::Failed(notice) => {
                if key != Key::Enter {
                    self.view = View::Failed(notice);
                }
            }
        }
        Outcome::Nothing
    }

    /// One key over the list, with nothing open on it.
    fn list_key(&mut self, key: Key) -> Outcome {
        let rows = self.rows();
        let under = rows.get(self.cursor).copied();
        match key {
            Key::Up | Key::Down => {
                self.cursor = crate::screen::move_cursor(self.cursor, rows.len(), key);
            }
            Key::Char(' ') => {
                if let Some(row) = under {
                    let items = self.items(row);
                    if self.is_selected(row) {
                        for item in &items {
                            self.selected.remove(item);
                        }
                    } else {
                        self.selected.extend(items);
                    }
                }
            }
            Key::Char('a') => {
                // The list has no search, so every row is visible and `a`
                // covers them all.
                let all = self.every_item();
                if all.iter().all(|item| self.selected.contains(item)) {
                    self.selected.clear();
                } else {
                    self.selected.extend(all);
                }
            }
            Key::Char('o') => self.view = View::Menu(0),
            Key::Enter => {
                if let Some(Row::Key(f, k)) = under {
                    let (file, row) = (&self.files[f], &self.files[f].keys[k]);
                    match &file.kind {
                        Kind::Config => return Outcome::SetConfig(row.set_as.clone()),
                        Kind::Pipeline(pipeline) => {
                            self.view = View::Set {
                                pipeline: pipeline.clone(),
                                key: row.key.clone(),
                                current: row.in_use.clone(),
                                value: row.in_use.clone(),
                            };
                        }
                        Kind::Prompt => {}
                    }
                }
            }
            _ => {}
        }
        Outcome::Nothing
    }

    /// The menu's items: both deletes, or none when the layer is empty.
    fn menu_items(&self) -> &'static [&'static str] {
        match self.files.is_empty() {
            true => &[],
            false => &MENU,
        }
    }

    /// The menu's title: how many keys are selected, or the row under the
    /// cursor, which `delete selected` acts on when nothing is.
    fn menu_title(&self) -> String {
        if !self.selected.is_empty() {
            return format!("{} selected", self.selected.len());
        }
        match self.rows().get(self.cursor) {
            Some(Row::File(f)) => self.files[*f].target.clone(),
            Some(Row::Key(f, k)) => self.files[*f].keys[*k].key.clone(),
            None => "nothing is overridden".to_string(),
        }
    }

    /// `delete selected`: the selected keys, or the row under the cursor
    /// when nothing is selected, file by file in list order.
    fn selected_deletion(&self) -> Option<Deletion> {
        let chosen: BTreeSet<Item> = match self.selected.is_empty() {
            true => self
                .rows()
                .get(self.cursor)
                .map(|row| self.items(*row).into_iter().collect())?,
            false => self.selected.clone(),
        };
        let files: Vec<Doomed> = self
            .files
            .iter()
            .filter_map(|file| {
                if file.keys.is_empty() {
                    return chosen
                        .contains(&(file.target.clone(), None))
                        .then(|| Doomed {
                            target: file.target.clone(),
                            keys: Vec::new(),
                            whole: true,
                        });
                }
                let keys: Vec<String> = file
                    .keys
                    .iter()
                    .map(|key| key.key.clone())
                    .filter(|key| chosen.contains(&(file.target.clone(), Some(key.clone()))))
                    .collect();
                (!keys.is_empty()).then(|| Doomed {
                    target: file.target.clone(),
                    keys,
                    whole: false,
                })
            })
            .collect();
        (!files.is_empty()).then(|| Deletion::of(files))
    }

    /// `delete all overrides`: every file, as a whole.
    fn whole_deletion(&self) -> Option<Deletion> {
        let files: Vec<Doomed> = self
            .files
            .iter()
            .map(|file| Doomed {
                target: file.target.clone(),
                keys: file.keys.iter().map(|key| key.key.clone()).collect(),
                whole: true,
            })
            .collect();
        (!files.is_empty()).then_some(Deletion {
            title: "delete all overrides",
            files,
        })
    }

    /// The list box's lines, `width` columns each: a blank row, then every
    /// file with its keys under it, cut to `rows` with the cursor's row
    /// kept in view.
    pub(super) fn lines(&self, rows: Option<usize>, width: usize) -> Vec<String> {
        let mut lines = vec![String::new()];
        if self.files.is_empty() {
            lines.push("  nothing is overridden".to_string());
            return lines;
        }
        let mut starts = Vec::new();
        let mut focus = (0, 0);
        for (at, row) in self.rows().into_iter().enumerate() {
            let mark = if at == self.cursor { "> " } else { "  " };
            let start = lines.len();
            starts.push(start);
            let ticked = tick(self.is_selected(row));
            match row {
                Row::File(f) => {
                    let file = &self.files[f];
                    let head = format!("{mark}{ticked} {}", column(&file.target, FILE_FIELD));
                    lines.extend(beside(&head, file.note.as_deref().unwrap_or(""), width));
                }
                Row::Key(f, k) => {
                    let key = &self.files[f].keys[k];
                    let head = format!("{mark}      {ticked} {}", column(&key.key, KEY_FIELD));
                    let value = match &key.state {
                        KeyState::Changed { old, new } => {
                            format!("{} → {}", shown(old), shown(new))
                        }
                        KeyState::Matches => MATCHES.to_string(),
                        KeyState::Ignored(reason) => format!("ignored — {reason}"),
                    };
                    lines.extend(beside(&head, &value, width));
                    if matches!(key.state, KeyState::Ignored(_)) {
                        lines.push(format!(
                            "{:indent$}{SET_AGAIN}",
                            "",
                            indent = head_len(&head)
                        ));
                    }
                }
            }
            if at == self.cursor {
                focus = (start, lines.len() - 1);
            }
        }
        crate::screen::pane::window(
            &lines,
            crate::screen::pane::Items {
                starts: &starts,
                noun: "row",
            },
            focus,
            rows,
            width,
        )
    }

    /// The popup over the list, if one is open. `rows` is how many rows the
    /// list box has.
    pub(super) fn popup(&self, rows: Option<usize>) -> Option<Vec<String>> {
        match &self.view {
            View::List => None,
            View::Menu(cursor) => Some(menu(&self.menu_title(), self.menu_items(), *cursor)),
            View::Confirm(deletion) => Some(deletion.popup(rows)),
            View::Set {
                pipeline,
                key,
                current,
                value,
            } => {
                let body = vec![
                    String::new(),
                    field("value: ", value, true),
                    String::new(),
                    format!("current   {}", shown(current)),
                ];
                Some(crate::screen::panel(
                    &format!("set {key} in {pipeline}"),
                    &body,
                    &keys(SET_KEYS),
                ))
            }
            View::Failed(notice) => Some(notice.panel(crate::screen::NOTICE_WRAP)),
        }
    }

    /// The key line under the frame: whatever the section shows names its
    /// own keys.
    pub(super) fn footer(&self) -> Vec<(&'static str, &'static str)> {
        match &self.view {
            View::List => [
                [
                    ("space", "select"),
                    ("a", "all"),
                    super::OPTIONS,
                    super::OPEN_WORKSPACE,
                    ("tab", "section"),
                ]
                .as_slice(),
                crate::screen::shell::quit_hint(),
            ]
            .concat(),
            View::Menu(_) if self.menu_items().is_empty() => vec![("esc", "close")],
            View::Menu(_) | View::Confirm(_) => ANSWER_KEYS.to_vec(),
            View::Set { .. } => SET_KEYS.to_vec(),
            View::Failed(_) => vec![("enter", "confirm")],
        }
    }
}

/// A row's options menu on the overrides tab: a blank row under the title,
/// each item, and the answer keys, in a box 40 columns wide. A menu with no
/// items says there is nothing to do, as every tab's does.
pub(super) fn menu(title: &str, items: &[&str], cursor: usize) -> Vec<String> {
    if items.is_empty() {
        return crate::screen::options(title, items, cursor);
    }
    let answers: Vec<String> = items.iter().map(|item| pad_to(item, MENU_TEXT)).collect();
    // `pick`, not `ask`: a menu of one item still names it, where `ask`
    // would draw a bare confirmation.
    crate::screen::pick(title, &[], &answers, cursor)
}

/// How many columns `head` takes on screen.
fn head_len(head: &str) -> usize {
    head.chars().count()
}

/// `head`, then `value` beside it wrapped to the room left in `width`, each
/// later line starting under the first. No value leaves `head` alone, with
/// the padding its column added taken off.
fn beside(head: &str, value: &str, width: usize) -> Vec<String> {
    if value.is_empty() {
        return vec![head.trim_end().to_string()];
    }
    let indent = head_len(head);
    crate::screen::wrap(value, width.saturating_sub(indent).max(1))
        .into_iter()
        .enumerate()
        .map(|(at, line)| match at {
            0 => format!("{head}{line}"),
            _ => format!("{:indent$}{line}", ""),
        })
        .collect()
}

/// Write `value` as `key` into pipeline `pipeline`'s patch, keeping every
/// other key in it: read as YAML for a step key, as `pipeline override
/// --set` reads it, and as text for `description` and `task_template`.
/// Checked no more than the pipeline section checks a value: the merge
/// decides, and the row says so.
fn write_pipeline_key(repo: &Repo, pipeline: &str, key: &str, value: &str) -> Result<()> {
    repo.refuse_in_lane("the overrides layer")?;
    let overrides = repo.overrides_dir();
    // Held from the read of the patch to its write, so a parallel
    // `pipeline override --set` starts from what this saved.
    let _lock = crate::lock::EditLock::acquire(&repo.edit_lock_path())?;
    let mut patch =
        crate::overrides::read_pipeline_patch(&overrides, pipeline)?.unwrap_or_default();
    match key.split_once('.') {
        Some((step, field)) => {
            let parsed: serde_norway::Value = serde_norway::from_str(value)
                .with_context(|| format!("`{value}` does not read as YAML"))?;
            patch
                .steps
                .entry(step.to_string())
                .or_default()
                .insert(serde_norway::Value::String(field.to_string()), parsed);
        }
        None if key == "description" => patch.description = Some(value.trim().to_string()),
        None if key == "task_template" => patch.task_template = Some(value.trim().to_string()),
        None => bail!("`{key}` is not a key a pipeline patch holds"),
    }
    crate::overrides::write_pipeline_patch(&overrides, pipeline, &patch)
}

/// Why the merge leaves out each key, and each whole file, as `override
/// list` prints it. Keyed by (file, key) for a key and by file for a file.
#[derive(Default)]
struct Reasons {
    keys: BTreeMap<(String, String), String>,
    files: BTreeMap<String, String>,
}

impl Reasons {
    /// Read off [`crate::commands::collect_override_rows`], the rows
    /// `override list` prints, so the two never give different reasons.
    fn read(repo: &Repo) -> Result<Reasons> {
        let mut reasons = Reasons::default();
        for row in crate::commands::collect_override_rows(repo)? {
            for item in &row.ignored {
                let reason = item.short_reason().to_string();
                if item.fields.is_empty() {
                    reasons.files.insert(row.target.clone(), reason);
                    continue;
                }
                for field in item.fields.split(", ") {
                    reasons
                        .keys
                        .insert((row.target.clone(), field.to_string()), reason.clone());
                }
            }
        }
        Ok(reasons)
    }

    fn key(&self, target: &str, key: &str) -> Option<&String> {
        self.keys.get(&(target.to_string(), key.to_string()))
    }
}

/// Every file in the layer, config first, then pipelines, then prompts, and
/// why no key is marked ignored when `override list` cannot read the layer.
/// `override list` prints config last; the section lists it first.
///
/// A file that does not read is listed with why, and no keys, so it can
/// still be deleted as a whole.
fn load(repo: &Repo, config: &Config) -> (Vec<FileRow>, Option<String>) {
    let overrides = repo.overrides_dir();
    let (reasons, stale) = match Reasons::read(repo) {
        Ok(reasons) => (reasons, None),
        Err(err) => (
            Reasons::default(),
            Some(format!(
                "the overrides layer does not read: {}. no key here is marked ignored until it \
                 does; fix the file it names, or delete it here.",
                one_line(&err)
            )),
        ),
    };
    let mut files = Vec::new();
    if let Some(file) = config_file(repo, &overrides, &reasons) {
        files.push(file);
    }
    // The main checkout's pipelines, as the dispatcher reads them: a
    // worktree's copy may differ, and would say a key matches a setup the
    // dispatcher does not run.
    let setup = Pipelines::load_tracked(&repo.root, config).ok();
    for name in crate::overrides::list_pipeline_patches(&overrides).unwrap_or_default() {
        let target = format!("pipelines/{name}.yml");
        let patch = match crate::overrides::read_pipeline_patch(&overrides, &name) {
            Ok(Some(patch)) => patch,
            Ok(None) => continue,
            Err(err) => {
                files.push(unreadable(target, Kind::Pipeline(name), &err));
                continue;
            }
        };
        let tracked = setup.as_ref().and_then(|s| s.pipelines.get(&name));
        let mut keys = Vec::new();
        for (key, theirs, ours) in [
            (
                "description",
                tracked.and_then(|p| p.description.clone()),
                patch.description.clone(),
            ),
            (
                "task_template",
                tracked.and_then(|p| p.task_template.clone()),
                patch.task_template.clone(),
            ),
        ] {
            if let Some(ours) = ours {
                let old = one_line_text(&theirs.unwrap_or_default());
                let new = one_line_text(&ours);
                let same = tracked.is_some() && old == new;
                keys.push(key_row(&reasons, &target, key, key, old, new, same));
            }
        }
        for (step_id, fields) in &patch.steps {
            let step = tracked.and_then(|p| p.steps.iter().find(|s| s.id == *step_id));
            for (field, value) in fields {
                let Some(field) = field.as_str() else {
                    continue;
                };
                let key = format!("{step_id}.{field}");
                let old = step.map(|s| step_value(s, field)).unwrap_or_default();
                // Compared as the merge would apply it, read back the way the
                // setup's value is, so `90m` and `1h30m` are the same value.
                // Shown as the patch spells it.
                let applied = step.and_then(|step| {
                    let mut probe = step.clone();
                    let mut one = serde_norway::Mapping::new();
                    one.insert(
                        serde_norway::Value::String(field.to_string()),
                        value.clone(),
                    );
                    crate::overrides::apply_step_patch(&mut probe, &one)
                        .ok()
                        .map(|()| step_value(&probe, field))
                });
                let same = applied.is_some_and(|applied| applied == old);
                keys.push(key_row(
                    &reasons,
                    &target,
                    &key,
                    &key,
                    old,
                    raw_yaml(value),
                    same,
                ));
            }
        }
        let note = reasons.files.get(&target).map(|r| format!("ignored — {r}"));
        files.push(FileRow {
            target,
            kind: Kind::Pipeline(name),
            note,
            keys,
        });
    }
    for name in crate::overrides::list_prompt_overrides(&overrides).unwrap_or_default() {
        let target = format!("prompts/{name}");
        let note = match reasons.files.get(&target) {
            Some(reason) => format!("ignored — {reason}"),
            None => WHOLE_FILE.to_string(),
        };
        files.push(FileRow {
            target,
            kind: Kind::Prompt,
            note: Some(note),
            keys: Vec::new(),
        });
    }
    (files, stale)
}

/// The layer's `config.toml` as a file row, `None` when there is none. Each
/// key's setup value is read from the main checkout's tracked config, the
/// one the dispatcher reads.
fn config_file(repo: &Repo, overrides: &std::path::Path, reasons: &Reasons) -> Option<FileRow> {
    let target = crate::config::CONFIG_FILE.to_string();
    let listed = match crate::overrides::config_patch_keys(overrides) {
        Ok(listed) => listed?,
        Err(err) => return Some(unreadable(target, Kind::Config, &err)),
    };
    let leaves = crate::overrides::config_patch_inputs(overrides)
        .ok()
        .flatten()
        .unwrap_or_default();
    let setup = Config::load_tracked(&repo.root).ok();
    let keys = listed
        .iter()
        .map(|key| {
            let leaf = leaves.iter().find(|leaf| leaf.key == *key);
            let set_as = leaf.map_or(key.as_str(), |leaf| leaf.applies_as.as_str());
            let old = setup
                .as_ref()
                .and_then(|setup| crate::confkv::get(setup, set_as).ok())
                .unwrap_or_default();
            // Compared as the merge would apply it, read back the way the
            // setup's value is, so `5` and `5.0` or a list's spacing are the
            // same value. Shown as the layer spells it.
            let applied = setup.as_ref().zip(leaf).and_then(|(setup, leaf)| {
                crate::confkv::set(setup, set_as, &leaf.input)
                    .and_then(|set| crate::confkv::get(&set, set_as))
                    .ok()
            });
            let same = applied.is_some_and(|applied| applied == old);
            let new = leaf.map(|leaf| leaf.input.clone()).unwrap_or_default();
            key_row(reasons, &target, key, set_as, old, new, same)
        })
        .collect();
    Some(FileRow {
        target,
        kind: Kind::Config,
        note: None,
        keys,
    })
}

/// One key's row: ignored when `override list` says so, `matches the
/// setup` when `same` says the setup already holds the layer's value, and
/// the setup's value then the layer's otherwise.
fn key_row(
    reasons: &Reasons,
    target: &str,
    key: &str,
    set_as: &str,
    old: String,
    new: String,
    same: bool,
) -> KeyRow {
    let (state, in_use) = match reasons.key(target, key) {
        Some(reason) => (KeyState::Ignored(reason.clone()), old),
        None if same => (KeyState::Matches, new),
        None => (
            KeyState::Changed {
                old,
                new: new.clone(),
            },
            new,
        ),
    };
    KeyRow {
        key: key.to_string(),
        set_as: set_as.to_string(),
        in_use,
        state,
    }
}

/// A file that does not read, as a row of its own with no keys.
fn unreadable(target: String, kind: Kind, err: &anyhow::Error) -> FileRow {
    FileRow {
        target,
        kind,
        note: Some(format!("does not read — {}", one_line(err))),
        keys: Vec::new(),
    }
}

/// `text` on one row: a description may run over several lines, and the
/// list gives each key one value.
fn one_line_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `value` as the patch spells it after `key: `.
fn raw_yaml(value: &serde_norway::Value) -> String {
    serde_norway::to_string(value)
        .map(|text| text.trim_end().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{key_line, lines, project, type_keys};
    use super::super::{Mode, Section, State};
    use crate::repo::Repo;
    use crate::screen::shell;

    /// Two pipelines with agent steps on sonnet, and a command step with a
    /// timeout.
    const IMPL: &str = "steps:
  - id: implement
    agent: claude
    model: claude-sonnet-5-5
    on_pass: fix-spec-review
  - id: fix-spec-review
    agent: claude
    model: claude-sonnet-5-5
    on_pass: done
";
    const IMPL_FAST: &str = "steps:
  - id: implement
    agent: claude
    model: claude-sonnet-5-5
    on_pass: test
  - id: test
    run: make test
    timeout: 45m
    on_pass: done
";

    /// A project tracking `impl`, `impl_fast` and prompt `code-reviewer`,
    /// with a layer setting `dispatch.lane_quiet`, two keys of `impl`, one
    /// of `impl_fast` and a copy of the prompt.
    fn layered(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let (repo, guard) = project(&format!("active-{name}"));
        let dir = crate::pipeline::Pipelines::dir_in(&repo.checkout);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("impl.yml"), IMPL).unwrap();
        std::fs::write(dir.join("impl_fast.yml"), IMPL_FAST).unwrap();
        let prompt = crate::prompt::directory_form(&repo, "code-reviewer");
        std::fs::create_dir_all(prompt.parent().unwrap()).unwrap();
        std::fs::write(&prompt, "review\n").unwrap();

        crate::commands::write_config_override(&repo, "dispatch.lane_quiet", "5m").unwrap();
        patch(
            &repo,
            "impl",
            "steps:\n  implement:\n    model: claude-opus-5-5\n  fix-spec-review:\n    model: \
             claude-opus-5-5\n",
        );
        patch(&repo, "impl_fast", "steps:\n  test:\n    timeout: 90m\n");
        crate::commands::write_prompt_override(&repo, "code-reviewer").unwrap();
        (repo, guard)
    }

    /// Write `body` as pipeline `name`'s patch in the layer.
    fn patch(repo: &Repo, name: &str, body: &str) {
        let path = crate::overrides::pipeline_patch_path(&repo.overrides_dir(), name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// The tab, opened on its active section.
    fn on_active(repo: &Repo) -> State {
        let mut state = State::new(repo);
        type_keys(repo, &mut state, "\t\t\t");
        assert_eq!(state.section, Section::Active);
        state
    }

    /// The list box's rows, between its borders, with the padding taken off.
    fn rows_of(state: &State) -> Vec<String> {
        lines(state)
            .iter()
            .skip(1)
            .take_while(|l| !l.starts_with('╰'))
            .map(|l| {
                l.trim_start_matches('│')
                    .trim_end_matches('│')
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The popup over the list, with the colour of its key row taken out.
    fn popup(state: &State) -> Vec<String> {
        super::super::popup(state, None)
            .unwrap()
            .iter()
            .map(|l| shell::plain(l))
            .collect()
    }

    /// Put the cursor on the file or key named `text`.
    fn cursor_on(state: &mut State, text: &str) {
        let rows = state.layer.rows();
        let at = rows
            .iter()
            .position(|row| match *row {
                super::Row::File(f) => state.layer.files[f].target == text,
                super::Row::Key(f, k) => state.layer.files[f].keys[k].key == text,
            })
            .unwrap();
        state.layer.cursor = at;
    }

    // The section lists each file with its keys under it: config first, the
    // setup's value then the layer's as the layer spells it, and a prompt
    // as `whole file`, with `[w] open workspace` on the key line.
    #[test]
    fn the_section_lists_each_file_with_its_keys_under_it() {
        let (repo, _guard) = layered("list");
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = on_active(&repo);
        cursor_on(&mut state, "pipelines/impl_fast.yml");
        assert!(lines(&state)[0].starts_with("╭─ config   pipeline   prompts   [active (5)] ─"));
        assert_eq!(
            &rows_of(&state)[..9],
            [
                "",
                "   [ ] config.toml",
                "         [ ] dispatch.lane_quiet      15m → 5m",
                "   [ ] pipelines/impl.yml",
                "         [ ] fix-spec-review.model    claude-sonnet-5-5 → claude-opus-5-5",
                "         [ ] implement.model          claude-sonnet-5-5 → claude-opus-5-5",
                " > [ ] pipelines/impl_fast.yml",
                "         [ ] test.timeout             45m → 90m",
                "   [ ] prompts/code-reviewer          whole file",
            ]
        );
        assert_eq!(
            key_line(&state),
            " [space] select   [a] all   [o] options   [w] open workspace   [tab] section"
        );
    }

    // A key the merge ignores reads `ignored —` with the reason `override
    // list` prints, wrapped in its column, and the row under it says what
    // to do.
    #[test]
    fn an_ignored_key_reads_the_reason_override_list_prints() {
        let (repo, _guard) = project("active-ignored");
        let _terminal = crate::screen::test_terminal((80, 40));
        crate::commands::write_config_override(&repo, "dispatch.visible_lanes", "three").unwrap();
        let rows = crate::commands::collect_override_rows(&repo).unwrap();
        let reason = rows[0].ignored[0].short_reason().to_string();
        let state = on_active(&repo);
        let drawn = rows_of(&state);
        let indent = " ".repeat(38);
        let mut reading = drawn[2].split_at(38).1.to_string();
        let mut at = 3;
        while drawn[at].starts_with(&indent) && !drawn[at].contains("[enter]") {
            reading = format!("{reading} {}", drawn[at].trim());
            at += 1;
        }
        assert!(
            drawn[2].starts_with(" >       [ ] dispatch.visible_lanes   ignored — ")
                || drawn[2].starts_with("         [ ] dispatch.visible_lanes   ignored — "),
            "{drawn:#?}"
        );
        // Wrapped into its column, so compared word for word.
        let words = reason.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(reading, format!("ignored — {words}"));
        assert_eq!(
            drawn[at],
            format!("{indent}[enter] to set it again, or o to delete")
        );
        assert!(drawn.iter().all(|l| l.chars().count() <= 78), "{drawn:#?}");
    }

    // A key whose value the setup already holds reads `matches the setup`,
    // for a config key and a pipeline key alike.
    #[test]
    fn a_key_the_setup_holds_reads_matches_the_setup() {
        let (repo, _guard) = layered("matches");
        crate::commands::write_config_override(&repo, "dispatch.lane_quiet", "15m").unwrap();
        patch(
            &repo,
            "impl",
            "steps:\n  implement:\n    model: claude-sonnet-5-5\n",
        );
        let state = on_active(&repo);
        let drawn = rows_of(&state);
        for row in [
            "         [ ] dispatch.lane_quiet      matches the setup — o to delete",
            "         [ ] implement.model          matches the setup — o to delete",
        ] {
            assert!(drawn.iter().any(|l| l == row), "{row}: {drawn:#?}");
        }
    }

    // `matches the setup` reads the main checkout's setup, the one the
    // dispatcher reads: a worktree's own copy holding the layer's value
    // leaves the row on the main checkout's.
    #[test]
    fn matches_the_setup_reads_the_main_checkout_not_a_worktree() {
        let (repo, _guard) = layered("worktree");
        let worktree = repo.home.join("worktree-copy");
        let config = crate::config::Config::path_in(&worktree);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "[dispatch]\nlane_quiet = \"5m\"\n").unwrap();
        let pipelines = crate::pipeline::Pipelines::dir_in(&worktree);
        std::fs::create_dir_all(&pipelines).unwrap();
        std::fs::write(
            pipelines.join("impl.yml"),
            IMPL.replace("claude-sonnet-5-5", "claude-opus-5-5"),
        )
        .unwrap();
        std::fs::write(pipelines.join("impl_fast.yml"), IMPL_FAST).unwrap();
        let mut in_worktree = repo.clone();
        in_worktree.checkout = worktree;

        let drawn = rows_of(&on_active(&in_worktree));
        assert!(
            drawn
                .iter()
                .any(|l| l == "         [ ] dispatch.lane_quiet      15m → 5m"),
            "{drawn:#?}"
        );
        assert!(
            drawn.iter().any(|l| l
                == "         [ ] implement.model          claude-sonnet-5-5 → claude-opus-5-5"),
            "{drawn:#?}"
        );
        assert!(!drawn.iter().any(|l| l.contains("matches the setup")));
    }

    // `space` on a file selects its keys, and again clears them; `a`
    // selects every file and key, and a second `a` clears the selection.
    #[test]
    fn space_selects_a_files_keys_and_a_selects_everything() {
        let (repo, _guard) = layered("select");
        let mut state = on_active(&repo);
        cursor_on(&mut state, "pipelines/impl.yml");
        type_keys(&repo, &mut state, " ");
        let drawn = rows_of(&state);
        assert!(drawn.contains(&" > [x] pipelines/impl.yml".to_string()));
        assert_eq!(
            drawn
                .iter()
                .filter(|l| l.starts_with("         [x] "))
                .count(),
            2
        );
        assert_eq!(state.layer.selected.len(), 2);
        type_keys(&repo, &mut state, " ");
        assert!(state.layer.selected.is_empty());

        type_keys(&repo, &mut state, "a");
        assert_eq!(state.layer.selected.len(), 5, "four keys and the prompt");
        assert!(rows_of(&state).iter().all(|l| !l.contains("[ ]")));
        type_keys(&repo, &mut state, "a");
        assert!(state.layer.selected.is_empty());
    }

    // `o` with keys selected opens the menu titled with how many, and
    // `delete selected` names every file and key in its confirm popup, each
    // file's keys under one another. `enter` deletes them, and a file left
    // with no keys leaves the layer.
    #[test]
    fn delete_selected_names_every_file_and_key_then_deletes_them() {
        let (repo, _guard) = layered("delete-selected");
        let mut state = on_active(&repo);
        cursor_on(&mut state, "pipelines/impl.yml");
        type_keys(&repo, &mut state, " ");
        cursor_on(&mut state, "pipelines/impl_fast.yml");
        type_keys(&repo, &mut state, " o");
        assert_eq!(
            popup(&state),
            [
                "╭─ 3 selected ─────────────────────────╮",
                "│                                      │",
                "│  > delete selected                   │",
                "│    delete all overrides              │",
                "│                                      │",
                "│  [enter] confirm   [esc] close       │",
                "╰──────────────────────────────────────╯",
            ]
        );
        assert_eq!(key_line(&state), " [enter] confirm   [esc] close");

        type_keys(&repo, &mut state, "\r");
        let drawn = popup(&state);
        assert!(
            drawn[0].starts_with("╭─ delete these overrides ─"),
            "{drawn:#?}"
        );
        // Every row's text. The longest row is one column wider than the
        // other popups' text, and the box widens for it rather than drop the
        // two columns every popup keeps inside its right border.
        let inside: Vec<&str> = drawn[1..drawn.len() - 1]
            .iter()
            .map(|l| l.trim_start_matches('│').trim_end_matches('│').trim_end())
            .collect();
        assert_eq!(
            inside,
            [
                "",
                "  3 keys in 2 files",
                "",
                "  pipelines/impl.yml        fix-spec-review.model",
                "                            implement.model",
                "  pipelines/impl_fast.yml   test.timeout",
                "",
                "  [enter] confirm   [esc] close",
            ]
        );
        type_keys(&repo, &mut state, "\r");
        let dir = repo.overrides_dir();
        assert!(!crate::overrides::pipeline_patch_path(&dir, "impl").exists());
        assert!(!crate::overrides::pipeline_patch_path(&dir, "impl_fast").exists());
        assert!(state.layer.popup(None).is_none());
        assert_eq!(state.active, 2, "the config key and the prompt are left");
        assert!(state.layer.selected.is_empty());
    }

    // With nothing selected, `o` acts on the row under the cursor: one key
    // goes, and the rest of its file stays.
    #[test]
    fn with_nothing_selected_o_deletes_the_row_under_the_cursor() {
        let (repo, _guard) = layered("delete-cursor");
        let mut state = on_active(&repo);
        cursor_on(&mut state, "implement.model");
        type_keys(&repo, &mut state, "o");
        assert!(popup(&state)[0].starts_with("╭─ implement.model ─"));
        type_keys(&repo, &mut state, "\r");
        assert!(popup(&state)[0].starts_with("╭─ delete this override ─"));
        assert!(popup(&state).contains(&format!(
            "│  {:48}│",
            "pipelines/impl.yml   implement.model"
        )));
        type_keys(&repo, &mut state, "\r");
        let left = crate::overrides::read_pipeline_patch(&repo.overrides_dir(), "impl")
            .unwrap()
            .unwrap();
        assert_eq!(left.steps.keys().collect::<Vec<_>>(), ["fix-spec-review"]);
        assert_eq!(state.active, 4);

        // A prompt has no keys, so its row deletes the whole copy.
        cursor_on(&mut state, "prompts/code-reviewer");
        type_keys(&repo, &mut state, "o\r");
        assert!(popup(&state).iter().any(|l| l.contains("whole file")));
        type_keys(&repo, &mut state, "\r");
        assert!(
            !crate::overrides::prompt_patch_path(&repo.overrides_dir(), "code-reviewer").exists()
        );
    }

    // `esc` on the menu and on the confirm popup closes it with nothing
    // deleted.
    #[test]
    fn esc_closes_the_menu_and_the_confirm_popup() {
        let (repo, _guard) = layered("esc");
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "o\x1b");
        assert!(state.layer.popup(None).is_none());
        type_keys(&repo, &mut state, "o\r\x1b");
        assert!(state.layer.popup(None).is_none());
        assert_eq!(state.active, 5);
    }

    // `delete all overrides` names every file and key, and empties the
    // layer.
    #[test]
    fn delete_all_overrides_empties_the_layer() {
        let (repo, _guard) = layered("delete-all");
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "o\x1b[B\r");
        let drawn = popup(&state);
        assert!(
            drawn[0].starts_with("╭─ delete all overrides ─"),
            "{drawn:#?}"
        );
        assert!(drawn.iter().any(|l| l.contains("5 keys in 4 files")));
        for text in [
            "config.toml",
            "dispatch.lane_quiet",
            "pipelines/impl.yml",
            "fix-spec-review.model",
            "implement.model",
            "pipelines/impl_fast.yml",
            "test.timeout",
            "prompts/code-reviewer",
            "whole file",
        ] {
            assert!(drawn.iter().any(|l| l.contains(text)), "{text}: {drawn:#?}");
        }
        type_keys(&repo, &mut state, "\r");
        assert!(
            crate::commands::collect_override_rows(&repo)
                .unwrap()
                .is_empty()
        );
        assert_eq!(state.active, 0);
        assert!(
            rows_of(&state)
                .iter()
                .any(|l| l == "   nothing is overridden")
        );
    }

    // A delete across two files whose second file fails leaves the first
    // changed, and the notice names the file already changed and the one
    // that is not.
    #[test]
    fn a_delete_that_fails_on_the_second_file_names_the_first() {
        let (repo, _guard) = layered("delete-partial");
        let mut state = on_active(&repo);
        cursor_on(&mut state, "pipelines/impl.yml");
        type_keys(&repo, &mut state, " ");
        cursor_on(&mut state, "pipelines/impl_fast.yml");
        type_keys(&repo, &mut state, " o\r");
        // The second file stops reading after the popup listed it.
        let second = crate::overrides::pipeline_patch_path(&repo.overrides_dir(), "impl_fast");
        std::fs::write(&second, "steps: [\n").unwrap();
        type_keys(&repo, &mut state, "\r");

        let dir = repo.overrides_dir();
        assert!(!crate::overrides::pipeline_patch_path(&dir, "impl").exists());
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "steps: [\n");
        let drawn = popup(&state).join("\n");
        assert!(drawn.contains("╭─ deleted in part ─"), "{drawn}");
        let text: String = popup(&state)
            .iter()
            .map(|l| l.trim_matches('│').trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            text.contains(
                "the overrides in pipelines/impl.yml were deleted, but not those in \
                 pipelines/impl_fast.yml"
            ),
            "{text}"
        );
        type_keys(&repo, &mut state, "x\x1b");
        assert!(state.layer.popup(None).is_some(), "only enter closes it");
        type_keys(&repo, &mut state, "\r");
        assert!(state.layer.popup(None).is_none());
    }

    // `o` on a config key the layer sets offers `delete override`, which
    // removes the key after one confirm popup; on a key it does not set,
    // the menu has nothing to do.
    #[test]
    fn o_on_a_config_key_deletes_its_override() {
        let (repo, _guard) = layered("config-o");
        let mut state = State::new(&repo);
        let at = state
            .matches()
            .iter()
            .position(|s| s.key() == "dispatch.visible_lanes")
            .unwrap();
        state.cursor = at + 1;
        type_keys(&repo, &mut state, "o");
        assert!(
            popup(&state)
                .iter()
                .any(|l| l.contains("nothing to do here"))
        );
        type_keys(&repo, &mut state, "\x1b");
        assert!(matches!(state.mode, Mode::Browsing));

        let at = state
            .matches()
            .iter()
            .position(|s| s.key() == "dispatch.lane_quiet")
            .unwrap();
        state.cursor = at + 1;
        type_keys(&repo, &mut state, "o");
        assert_eq!(
            popup(&state)[..3],
            [
                format!("╭─ dispatch.lane_quiet {}╮", "─".repeat(16)),
                format!("│{:38}│", ""),
                format!("│{:38}│", "  > delete override"),
            ]
        );
        type_keys(&repo, &mut state, "\r");
        let drawn = popup(&state);
        assert!(
            drawn[0].starts_with("╭─ delete this override ─"),
            "{drawn:#?}"
        );
        assert!(drawn.contains(&format!("│  {:48}│", "config.toml   dispatch.lane_quiet")));
        type_keys(&repo, &mut state, "\r");
        assert!(matches!(state.mode, Mode::Browsing));
        assert!(!crate::overrides::config_patch_path(&repo.overrides_dir()).exists());
        assert_eq!(state.active, 4);
    }

    // `o` on a pipeline field offers `delete override`, which removes the
    // field from every pipeline patch that sets it, on every step.
    #[test]
    fn o_on_a_pipeline_field_deletes_it_from_every_patch() {
        let (repo, _guard) = layered("pipeline-o");
        patch(
            &repo,
            "impl_fast",
            "steps:\n  implement:\n    model: claude-opus-5-5\n  test:\n    timeout: 90m\n",
        );
        let mut state = State::new(&repo);
        type_keys(&repo, &mut state, "\to\r");
        let drawn = popup(&state);
        assert!(
            drawn.iter().any(|l| l.contains("3 keys in 2 files")),
            "{drawn:#?}"
        );
        type_keys(&repo, &mut state, "\r");
        let dir = repo.overrides_dir();
        assert!(!crate::overrides::pipeline_patch_path(&dir, "impl").exists());
        let left = crate::overrides::read_pipeline_patch(&dir, "impl_fast")
            .unwrap()
            .unwrap();
        assert_eq!(left.steps.keys().collect::<Vec<_>>(), ["test"]);
    }

    // A file that does not read says so on its row with no keys, and the
    // note above the list says why no key is marked ignored. Its advice
    // works: deleting the file here brings the reasons back.
    #[test]
    fn a_file_that_does_not_read_can_be_deleted_here() {
        let (repo, _guard) = layered("unreadable");
        let _terminal = crate::screen::test_terminal((80, 40));
        patch(&repo, "impl_fast", "steps: [\n");
        let mut state = on_active(&repo);
        let note = rows_of(&state).join(" ");
        assert!(note.contains("the overrides layer does not read"), "{note}");
        assert!(note.contains("delete it here"), "{note}");
        assert!(
            rows_of(&state)
                .iter()
                .any(|l| l.starts_with("   [ ] pipelines/impl_fast.yml        does not read — ")),
            "{:#?}",
            rows_of(&state)
        );

        cursor_on(&mut state, "pipelines/impl_fast.yml");
        type_keys(&repo, &mut state, "o\r");
        assert!(popup(&state).iter().any(|l| l.contains("whole file")));
        type_keys(&repo, &mut state, "\r");
        assert!(
            !crate::overrides::pipeline_patch_path(&repo.overrides_dir(), "impl_fast").exists()
        );
        assert_eq!(state.layer.stale, None);
    }

    // `enter` on an ignored config key opens the config section's set
    // popup, so it can be set again, and the write lands in the layer.
    #[test]
    fn enter_on_a_config_key_sets_it_again() {
        let (repo, _guard) = project("active-set-config");
        crate::commands::write_config_override(&repo, "dispatch.visible_lanes", "three").unwrap();
        let mut state = on_active(&repo);
        cursor_on(&mut state, "dispatch.visible_lanes");
        type_keys(&repo, &mut state, "\r");
        assert!(popup(&state)[0].starts_with("╭─ set dispatch.visible_lanes ─"));
        type_keys(&repo, &mut state, "\x7f\x7f\x7f\x7f\x7f4\r");
        let text =
            std::fs::read_to_string(crate::overrides::config_patch_path(&repo.overrides_dir()))
                .unwrap();
        assert!(text.contains("visible_lanes = 4"), "{text}");
        assert_eq!(state.section, Section::Active);
        assert!(!rows_of(&state).iter().any(|l| l.contains("ignored")));
    }

    // `enter` on a pipeline key opens a field on the value in use, and
    // writes what is typed into that one key.
    #[test]
    fn enter_on_a_pipeline_key_sets_it_again() {
        let (repo, _guard) = layered("set-pipeline");
        let mut state = on_active(&repo);
        cursor_on(&mut state, "test.timeout");
        type_keys(&repo, &mut state, "\r");
        let drawn = popup(&state);
        assert!(
            drawn[0].starts_with("╭─ set test.timeout in impl_fast ─"),
            "{drawn:#?}"
        );
        assert!(drawn.iter().any(|l| l.contains("value: 90m▏")));
        assert!(drawn.iter().any(|l| l.contains("current   90m")));
        type_keys(&repo, &mut state, "\x7f\x7f\x7f2h\r");
        let left = crate::overrides::read_pipeline_patch(&repo.overrides_dir(), "impl_fast")
            .unwrap()
            .unwrap();
        assert_eq!(
            left.steps["test"]["timeout"],
            serde_norway::Value::String("2h".to_string())
        );
        assert!(state.layer.popup(None).is_none());
        assert!(
            rows_of(&state)
                .iter()
                .any(|l| l == " >       [ ] test.timeout             45m → 2h")
        );
    }
}
