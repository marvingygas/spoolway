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
//! is changed with one write, so a delete that stops partway leaves whole
//! files changed or not, and the notice names the ones already changed.

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
    /// The keys to remove, or every key the file holds when `whole`.
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

    /// Delete every file in turn, each with one write. A failure stops at
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
/// column, as the mockup lines them up. A longer name pushes only its own
/// value along, one space past its end.
const FILE_FIELD: usize = 31;
const KEY_FIELD: usize = 25;

/// The columns between a file and its first key in the confirm popup.
const TARGET_GAP: usize = 3;

/// The menu's items, inside a box as wide as the mockup draws it.
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
        (!files.is_empty()).then(|| Deletion {
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
                        lines.push(format!("{:indent$}{SET_AGAIN}", "", indent = head_len(&head)));
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

/// A row's options menu drawn as the mockup draws the active section's: a
/// blank row under the title, each item, and the answer keys, in a box 40
/// columns wide. A menu with no items says there is nothing to do, as every
/// tab's does.
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
    let mut patch = crate::overrides::read_pipeline_patch(&overrides, pipeline)?.unwrap_or_default();
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

/// Every file in the layer, config first, then pipelines, then prompts, as
/// the mockup lists them, and why no key is marked ignored when `override
/// list` cannot read the layer.
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
                keys.push(key_row(&reasons, &target, key, key, old, new, tracked.is_some()));
            }
        }
        for (step_id, fields) in &patch.steps {
            let step = tracked.and_then(|p| p.steps.iter().find(|s| s.id == *step_id));
            for (field, value) in fields {
                let Some(field) = field.as_str() else { continue };
                let key = format!("{step_id}.{field}");
                let old = step.map(|s| step_value(s, field)).unwrap_or_default();
                // The value as the merge would apply it, read back the way
                // the setup's is, so `5m` and `"5m"` compare equal. One the
                // step refuses is shown as the patch spells it.
                let applied = step.and_then(|step| {
                    let mut probe = step.clone();
                    let mut one = serde_norway::Mapping::new();
                    one.insert(serde_norway::Value::String(field.to_string()), value.clone());
                    crate::overrides::apply_step_patch(&mut probe, &one)
                        .ok()
                        .map(|()| step_value(&probe, field))
                });
                let new = applied.clone().unwrap_or_else(|| raw_yaml(value));
                keys.push(key_row(
                    &reasons,
                    &target,
                    &key,
                    &key,
                    old,
                    new,
                    applied.is_some(),
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
            // The value as the merge would apply it, read back the way the
            // setup's is, so `5` and `5.0` or a list's spacing compare equal.
            let applied = setup.as_ref().zip(leaf).and_then(|(setup, leaf)| {
                crate::confkv::set(setup, set_as, &leaf.input)
                    .and_then(|set| crate::confkv::get(&set, set_as))
                    .ok()
            });
            let new = applied
                .clone()
                .or_else(|| leaf.map(|leaf| leaf.input.clone()))
                .unwrap_or_default();
            key_row(
                reasons,
                &target,
                key,
                set_as,
                old,
                new,
                applied.is_some(),
            )
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
/// setup` when `comparable` and the two values are the same, and the setup's
/// value then the layer's otherwise. `comparable` is false when either
/// value could not be read as the merge reads it, where text that happens
/// to be equal does not mean the setup holds it.
fn key_row(
    reasons: &Reasons,
    target: &str,
    key: &str,
    set_as: &str,
    old: String,
    new: String,
    comparable: bool,
) -> KeyRow {
    let (state, in_use) = match reasons.key(target, key) {
        Some(reason) => (KeyState::Ignored(reason.clone()), old),
        None if comparable && old == new => (KeyState::Matches, new),
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
