//! Bare `spoolway`'s overrides tab: the private overrides layer, set from the
//! screen instead of one `override` command per key.
//!
//! The tab has four sections, named in its list box's own title — config,
//! pipeline, prompts and active — and `tab` moves between them. A second bar
//! under the strip would read as a second strip, so the box title carries
//! them, bracketed the way the strip brackets the open tab.
//!
//! The config section shows every key `spoolway config list` prints,
//! grouped under its table, with the value in use now: the tracked config
//! with the layer merged onto it. A key with no value reads `not set`, and
//! `+ new key` takes any dotted key the list does not show, such as a
//! `[models]` glob the config has no entry for yet. The pipeline section
//! sets one step field on many pipeline steps at once — see [`pipeline`].
//! The prompts and active sections have no list yet.
//!
//! A value is written exactly as typed, through the same unchecked write
//! `config override --set` makes after its own checks — see
//! [`crate::commands::write_config_override`]. A value or key the merge
//! cannot use is left in the layer, where `override list` reports it as
//! ignored. Checking it here would add rules a person has to learn.

mod pipeline;

use std::io::Write;

use anyhow::Result;

use super::shell::{self, Leave};
use super::{Key, Notice, PollableRead, keys, pad_to};
use crate::config::Config;
use crate::repo::Repo;

/// The sections, in the order the box title draws them and `tab` walks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Config,
    Pipeline,
    Prompts,
    Active,
}

const SECTIONS: [Section; 4] = [
    Section::Config,
    Section::Pipeline,
    Section::Prompts,
    Section::Active,
];

impl Section {
    /// The section after this one, wrapping from active back to config:
    /// `tab` is the only key that moves between them, so the far end has to
    /// lead somewhere.
    fn next(self) -> Section {
        let at = SECTIONS.iter().position(|s| *s == self).unwrap_or(0);
        SECTIONS[(at + 1) % SECTIONS.len()]
    }
}

/// One config key as the list shows it.
#[derive(Debug, Clone)]
struct Setting {
    /// The table the key sits under, such as `agents.claude`.
    table: String,
    /// The key's last part, such as `concurrency`.
    name: String,
    /// The value in use now, empty for a key with none.
    value: String,
}

impl Setting {
    fn key(&self) -> String {
        format!("{}.{}", self.table, self.name)
    }
}

/// What a value with nothing in it reads as, in the list and in a popup's
/// `current` row.
const NOT_SET: &str = "not set";

/// `value`, or [`NOT_SET`] when it is empty.
fn shown(value: &str) -> &str {
    match value.is_empty() {
        true => NOT_SET,
        false => value,
    }
}

/// What has the tab's keys.
#[derive(Debug, Clone)]
enum Mode {
    Browsing,
    /// `f` was pressed and the query is being typed.
    Search,
    /// The one-line field over a listed key.
    Set {
        key: String,
        value: String,
    },
    /// `+ new key`'s popup: a key field and a value field, `tab` between.
    NewKey {
        key: String,
        value: String,
        /// Whether the value field has the keys, rather than the key field.
        on_value: bool,
    },
    /// A write that failed, until `enter` closes it.
    Notice(Notice),
}

/// Everything the tab holds while it is open. Built fresh on every visit.
struct State {
    section: Section,
    mode: Mode,
    /// The search query, kept after `enter` leaves the search row.
    filter: String,
    /// Which selectable row the cursor is on: `0` is `+ new key`, and `n` is
    /// the `n`th key [`State::matches`] answers.
    cursor: usize,
    /// Every key, with its value in the tracked config with the layer
    /// merged onto it, or in the config the screen started with while that
    /// does not load.
    settings: Vec<Setting>,
    /// The count `active` carries in the box title.
    active: usize,
    /// Why [`State::settings`] holds the config the screen started with
    /// rather than the one in use now, drawn at the top of the box.
    stale: Option<String>,
    /// The pipeline section, which keeps its own list and popups.
    pipeline: pipeline::PipelineSection,
}

impl State {
    fn new(repo: &Repo) -> State {
        let mut state = State {
            section: Section::Config,
            mode: Mode::Browsing,
            filter: String::new(),
            cursor: 0,
            settings: Vec::new(),
            active: 0,
            stale: None,
            pipeline: pipeline::PipelineSection::new(),
        };
        state.reload(repo);
        state
    }

    /// Read the config, the pipelines and the layer again, after a write or
    /// on a visit.
    fn reload(&mut self, repo: &Repo) {
        let (config, stale) = in_use(repo);
        self.settings = settings(&config);
        self.stale = stale;
        self.pipeline.reload(repo, &config);
        self.active = layer_key_count(&repo.overrides_dir());
        self.clamp();
    }

    /// The keys the search leaves, in list order. A key matches on its
    /// dotted name or on the value shown beside it, `not set` included, so
    /// a search for that finds every key with no value.
    fn matches(&self) -> Vec<&Setting> {
        let query = self.filter.to_lowercase();
        self.settings
            .iter()
            .filter(|s| {
                query.is_empty()
                    || s.key().to_lowercase().contains(&query)
                    || shown(&s.value).to_lowercase().contains(&query)
            })
            .collect()
    }

    /// The key under the cursor, `None` on `+ new key`.
    fn under_cursor(&self) -> Option<&Setting> {
        self.cursor
            .checked_sub(1)
            .and_then(|at| self.matches().get(at).copied())
    }

    /// Keep the cursor on a row the search still shows.
    fn clamp(&mut self) {
        self.cursor = self.cursor.min(self.matches().len());
    }

    /// Whether the open section's list has the keys, with nothing open over
    /// it: the only time `tab`, `q` and `←`/`→` reach past the section.
    fn at_rest(&self) -> bool {
        matches!(self.mode, Mode::Browsing)
            && (self.section != Section::Pipeline || self.pipeline.at_rest())
    }
}

/// The config in use now: the tracked file with `overrides/config.toml`
/// merged onto it, read fresh so a key the tab just wrote shows its new
/// value.
///
/// A tracked file or a layer that no longer loads falls back to the config
/// the screen started with rather than emptying the list, and answers why
/// beside it. Left unsaid, every value would still read as the one in use,
/// while every command that loads the config refuses it, and `active` would
/// drop the broken file to nothing with no reason given.
fn in_use(repo: &Repo) -> (Config, Option<String>) {
    let tracked = match Config::load_tracked(&repo.checkout) {
        Ok(tracked) => tracked,
        Err(err) => {
            return (
                repo.config.clone(),
                Some(format!(
                    "the tracked config no longer loads: {}. this list shows the config the \
                     screen started with; fix the file with `spoolway config edit`.",
                    one_line(&err)
                )),
            );
        }
    };
    match crate::overrides::apply_config_patch(tracked, &repo.overrides_dir()) {
        Ok((config, _)) => (config, None),
        Err(err) => (
            repo.config.clone(),
            Some(format!(
                "overrides/config.toml no longer merges: {}. this list shows the config \
                 the screen started with; reopen the file with `spoolway config override`.",
                one_line(&err)
            )),
        ),
    }
}

/// `err` and its causes, the first line of each joined the way `{:#}` joins
/// them. A TOML parse error spends its later lines drawing the bad line with
/// a caret under it, which reads as noise once wrapped into a box. A file
/// that does not read or parse is named by its full path in the error
/// itself, so the note names the file only by its short name.
fn one_line(err: &anyhow::Error) -> String {
    err.chain()
        .filter_map(|cause| cause.to_string().lines().next().map(str::to_string))
        .collect::<Vec<_>>()
        .join(": ")
}

/// Every key [`crate::confkv::all_settings`] lists, grouped by table.
///
/// That list is sorted by the whole key, which splits a table around a
/// nested one: a `[models]` glob's own fields sort on both sides of its
/// tier's table, and its heading would be drawn twice. Sorting by table
/// first keeps each table's keys under one heading.
fn settings(config: &Config) -> Vec<Setting> {
    let mut out: Vec<Setting> = crate::confkv::all_settings(config)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let (table, name) = entry.key.rsplit_once('.')?;
            Some(Setting {
                table: table.to_string(),
                name: name.to_string(),
                value: entry.value,
            })
        })
        .collect();
    out.sort_by(|a, b| (&a.table, &a.name).cmp(&(&b.table, &b.name)));
    out
}

/// How many keys the layer under `overrides` holds: every config key, every
/// field of every patched pipeline step, a pipeline's `description` and
/// `task_template`, and each prompt as one. A file that does not read adds
/// nothing, so a broken patch never hides the tab; `override list` is where
/// it is reported, and a config patch is named over the list as well.
fn layer_key_count(overrides: &std::path::Path) -> usize {
    let config = crate::overrides::config_patch_keys(overrides)
        .ok()
        .flatten()
        .map_or(0, |keys| keys.len());
    let pipelines: usize = crate::overrides::list_pipeline_patches(overrides)
        .unwrap_or_default()
        .iter()
        .filter_map(|name| {
            crate::overrides::read_pipeline_patch(overrides, name)
                .ok()
                .flatten()
        })
        .map(|patch| {
            usize::from(patch.description.is_some())
                + usize::from(patch.task_template.is_some())
                + patch
                    .steps
                    .values()
                    .map(|fields| fields.len())
                    .sum::<usize>()
        })
        .sum();
    let prompts = crate::overrides::list_prompt_overrides(overrides).map_or(0, |names| names.len());
    config + pipelines + prompts
}

/// Bare `spoolway`'s overrides tab, over the terminal the shell already
/// holds. Answers the [`Leave`] that ended it. Every visit opens on config,
/// with the config, the pipelines and the layer read again.
pub(crate) fn tab(
    repo: &Repo,
    writer: &mut super::frame_writer::FrameWriter,
    input: &mut impl PollableRead,
    out: &mut impl Write,
) -> Result<Leave> {
    let mut state = State::new(repo);
    loop {
        writer.write_frame(&frame(&state), super::pane_size(), out);
        // Drawn again on every idle slice, not only after a key: a pane
        // resized while nothing is typed would otherwise keep a frame
        // measured for the old size, wrapped or cut, until the next key.
        // The writer skips a frame that has not changed.
        let Some(key) = shell::wait_key(input, || {
            writer.write_frame(&frame(&state), super::pane_size(), out)
        }) else {
            return Ok(Leave::Quit);
        };
        // Only at rest: the search rows, the fields, the picking list and
        // the notices read `q` and the arrows as their own.
        if state.at_rest()
            && let Some(leave) = shell::leave_on(key)
        {
            return Ok(leave);
        }
        handle_key(repo, &mut state, key);
    }
}

/// One key, read by whatever is open: the list, the search row, a popup or
/// a notice. The pipeline section reads its own keys, all but the `tab` that
/// leaves it.
fn handle_key(repo: &Repo, state: &mut State, key: Key) {
    if state.section == Section::Pipeline && matches!(state.mode, Mode::Browsing) {
        if key == Key::Tab && state.pipeline.at_rest() {
            state.section = state.section.next();
        } else if state.pipeline.handle_key(repo, key) {
            state.reload(repo);
        }
        return;
    }
    match std::mem::replace(&mut state.mode, Mode::Browsing) {
        Mode::Browsing => browse(state, key),
        Mode::Search => match key {
            Key::Enter => {}
            Key::Esc => {
                state.filter.clear();
                state.clamp();
            }
            Key::Up | Key::Down => {
                move_cursor(state, key);
                state.mode = Mode::Search;
            }
            other => {
                edit(&mut state.filter, other);
                state.clamp();
                state.mode = Mode::Search;
            }
        },
        Mode::Set { key: name, value } => match key {
            Key::Esc => {}
            Key::Enter => save(repo, state, &name, &value),
            other => {
                let mut value = value;
                edit(&mut value, other);
                state.mode = Mode::Set { key: name, value };
            }
        },
        Mode::NewKey {
            key: name,
            value,
            on_value,
        } => match key {
            Key::Esc => {}
            Key::Enter => save(repo, state, name.trim(), &value),
            Key::Tab => {
                state.mode = Mode::NewKey {
                    key: name,
                    value,
                    on_value: !on_value,
                }
            }
            other => {
                let (mut name, mut value) = (name, value);
                edit(if on_value { &mut value } else { &mut name }, other);
                state.mode = Mode::NewKey {
                    key: name,
                    value,
                    on_value,
                };
            }
        },
        Mode::Notice(notice) => {
            if key != Key::Enter {
                state.mode = Mode::Notice(notice);
            }
        }
    }
}

/// One key over the list, with nothing open on it.
fn browse(state: &mut State, key: Key) {
    match key {
        Key::Tab => state.section = state.section.next(),
        // Prompts and active have no list yet: only `tab` reaches past them.
        _ if state.section != Section::Config => {}
        Key::Up | Key::Down => move_cursor(state, key),
        Key::Char('f') => state.mode = Mode::Search,
        Key::Enter => {
            state.mode = match state.under_cursor() {
                Some(setting) => Mode::Set {
                    key: setting.key(),
                    value: setting.value.clone(),
                },
                None => Mode::NewKey {
                    key: String::new(),
                    value: String::new(),
                    on_value: false,
                },
            }
        }
        _ => {}
    }
}

/// The cursor moved by `↑` or `↓` over `+ new key` and the keys the search
/// leaves, held at either end.
fn move_cursor(state: &mut State, key: Key) {
    state.cursor = super::move_cursor(state.cursor, state.matches().len() + 1, key);
}

/// `key` typed into a one-line field holding `text`: a character is added,
/// backspace takes the last one off, and anything else is ignored. `0x08`
/// is the backspace many terminals send in place of `DEL`, the same pairing
/// the queue tab's search reads.
fn edit(text: &mut String, key: Key) {
    match key {
        Key::Backspace | Key::Char('\u{8}') => {
            text.pop();
        }
        Key::Char(c) if !c.is_control() => text.push(c),
        _ => {}
    }
}

/// Write `key` as `value` into the layer, as typed. An empty key, or a
/// value left empty or holding only spaces, writes nothing: `enter` on an
/// empty field is how a popup is left without a write. A write that fails
/// is held as a notice, since the popup it came from has already closed.
fn save(repo: &Repo, state: &mut State, key: &str, value: &str) {
    if key.is_empty() || value.trim().is_empty() {
        return;
    }
    match crate::commands::write_config_override(repo, key, value) {
        Ok(_) => state.reload(repo),
        Err(err) => {
            state.mode = Mode::Notice(Notice::new(
                "not written",
                format!(
                    "{key} was not written to the overrides layer: {err:#}. \
                     nothing changed; fix what stopped it and set the key again."
                ),
            ))
        }
    }
}

/// The frame width with no terminal to measure: the 80 columns every mockup
/// of this tab is drawn to.
const FALLBACK_WIDTH: usize = 80;

/// What the frame takes off the drawing area's height besides the list: the
/// box's two borders, the key line's first row, and one spare row so the
/// last line's own newline does not scroll the strip away — the same count
/// `eval`'s own `frame_chrome` gives.
const FRAME_CHROME: usize = 4;

/// The column a key's value starts at, counted from the key's own name: wide
/// enough for every name the default config carries with room between, as
/// the mockup lines them up. A longer name widens the column for every key,
/// so the values still line up under each other.
const NAME_FIELD: usize = 28;

/// The text inside a popup, so both popups are 50 columns wide inside their
/// border whatever is typed into them, as the mockups draw them.
const POPUP_TEXT: usize = 46;

/// The whole frame: the strip, the list box, any popup over it, and the key
/// line under it.
fn frame(state: &State) -> Vec<String> {
    let area = super::drawing_area();
    let width = area.map_or(FALLBACK_WIDTH, |(width, _)| width);
    let footer = super::key_rows(&footer(state));
    let rows = area.map(|(_, height)| {
        height
            .saturating_sub(FRAME_CHROME + shell::strip_rows() + footer.len().saturating_sub(1))
            .max(1)
    });
    let inner = width.saturating_sub(2);
    let popup = popup(state, rows);

    let mut body = list(state, rows, inner.saturating_sub(1));
    // A frame with no terminal under it is only as tall as its list, and
    // `overlay` draws nothing past the frame's last row: a popup taller
    // than the list would lose its key row and bottom border. A measured
    // terminal keeps its own height instead, popup or not: a body grown
    // past it would run the frame off the bottom and scroll the strip away.
    let tall = match rows {
        Some(rows) => rows,
        None => popup.as_ref().map_or(0, |p| p.len() + 2),
    };
    while body.len() < tall {
        body.push(String::new());
    }

    let mut frame = vec![top_border(state, width)];
    frame.extend(
        body.iter()
            .map(|line| format!("│ {}│", pad_to(line, inner.saturating_sub(1)))),
    );
    frame.push(format!(
        "{}{}{}",
        super::corner::BOTTOM_LEFT,
        "─".repeat(inner),
        super::corner::BOTTOM_RIGHT
    ));
    if let Some(popup) = popup {
        super::overlay(&mut frame, &popup);
    }
    let mut out = shell::strip();
    out.extend(frame);
    out.extend(footer);
    out
}

/// The box's top border, its title naming every section with the open one
/// bracketed, and `active` carrying the layer's key count. While the
/// pipeline section picks a field's steps, the title names that instead.
fn top_border(state: &State, width: usize) -> String {
    let picking = match state.section {
        Section::Pipeline => state.pipeline.title(),
        _ => None,
    };
    let title = picking.unwrap_or_else(|| section_names(state));
    let head = format!("─ {title} ");
    let dashes = width.saturating_sub(head.chars().count() + 2);
    format!(
        "{}{head}{}{}",
        super::corner::TOP_LEFT,
        "─".repeat(dashes),
        super::corner::TOP_RIGHT
    )
}

/// Every section's name, the open one bracketed.
fn section_names(state: &State) -> String {
    SECTIONS
        .iter()
        .map(|section| {
            let name = match section {
                Section::Config => "config".to_string(),
                Section::Pipeline => "pipeline".to_string(),
                Section::Prompts => "prompts".to_string(),
                Section::Active => format!("active ({})", state.active),
            };
            match *section == state.section {
                true => format!("[{name}]"),
                false => name,
            }
        })
        .collect::<Vec<_>>()
        .join("   ")
}

/// The list box's lines, `width` columns each: the open section's list, cut
/// to `rows` with the cursor's row kept in view.
///
/// Why the list is stale, when it is, heads the box above all of that and
/// never scrolls: it is about every value shown, not about the rows near it.
fn list(state: &State, rows: Option<usize>, width: usize) -> Vec<String> {
    let stale = match state.section {
        Section::Config => &state.stale,
        Section::Pipeline => &state.pipeline.stale,
        Section::Prompts | Section::Active => &None,
    };
    let mut head: Vec<String> = Vec::new();
    if let Some(stale) = stale {
        head.push(String::new());
        // One column short of the box, so no word touches its right border.
        head.extend(super::wrap(&format!("  {stale}"), width.saturating_sub(1)));
    }
    // Held to the rows the box has, with one left for the list itself.
    if let Some(rows) = rows {
        head.truncate(rows.saturating_sub(1));
    }
    let rows = rows.map(|rows| rows.saturating_sub(head.len()).max(1));
    match state.section {
        Section::Pipeline => head.extend(state.pipeline.lines(rows, width)),
        _ => head.extend(keys_list(state, rows, width)),
    }
    head
}

/// [`list`]'s lines below the note on a stale config: the search row while
/// a query is typed, a blank row, then `+ new key` and every key under its
/// table, the last row saying how many keys are out of sight. Prompts and
/// active, with no list yet, draw the blank row alone.
fn keys_list(state: &State, rows: Option<usize>, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if matches!(state.mode, Mode::Search) {
        lines.push(format!("search: {}▏", state.filter));
    }
    lines.push(String::new());
    if state.section != Section::Config {
        return lines;
    }

    let matches = state.matches();
    let name_field = matches
        .iter()
        .map(|s| s.name.chars().count() + 2)
        .max()
        .unwrap_or(0)
        .max(NAME_FIELD);
    let mark = |at: usize| if at == state.cursor { "> " } else { "  " };
    let mut starts = vec![lines.len()];
    lines.push(format!("{}+ new key", mark(0)));
    let mut table: Option<&str> = None;
    for (at, setting) in matches.iter().enumerate() {
        if table != Some(setting.table.as_str()) {
            table = Some(&setting.table);
            lines.push(format!("  {}", setting.table));
        }
        starts.push(lines.len());
        lines.push(format!(
            "{}  {:name_field$}{}",
            mark(at + 1),
            setting.name,
            shown(&setting.value)
        ));
    }
    if matches.is_empty() && !state.filter.is_empty() {
        lines.push("  no keys match this search".to_string());
    }
    let focus = starts[state.cursor.min(starts.len() - 1)];
    super::pane::window(
        &lines,
        super::pane::Items {
            starts: &starts,
            noun: "key",
        },
        (focus, focus),
        rows,
        width,
    )
}

/// The popup over the list, if one is open. `rows` is how many rows the
/// list box has.
fn popup(state: &State, rows: Option<usize>) -> Option<Vec<String>> {
    if state.section == Section::Pipeline && matches!(state.mode, Mode::Browsing) {
        return state.pipeline.popup(rows);
    }
    match &state.mode {
        Mode::Set { key, value } => {
            let body = vec![
                String::new(),
                field("value: ", value, true),
                String::new(),
                current(state, key),
            ];
            Some(super::panel(&format!("set {key}"), &body, &keys(SET_KEYS)))
        }
        Mode::NewKey {
            key,
            value,
            on_value,
        } => {
            let body = vec![
                String::new(),
                field("key:     ", key, !on_value),
                field("value:   ", value, *on_value),
                String::new(),
                current(state, key.trim()),
            ];
            Some(super::panel("new key", &body, &keys(NEW_KEY_KEYS)))
        }
        Mode::Notice(notice) => Some(notice.panel(super::NOTICE_WRAP)),
        Mode::Browsing | Mode::Search => None,
    }
}

/// One field's row: `label`, then `text` with the typing cursor after it
/// when the field has the keys, padded to [`POPUP_TEXT`]. Text too long for
/// the row shows its end, where the typing is.
fn field(label: &str, text: &str, typing: bool) -> String {
    let cursor = if typing { "▏" } else { "" };
    let room = POPUP_TEXT.saturating_sub(label.chars().count() + cursor.chars().count());
    let skip = text.chars().count().saturating_sub(room);
    let tail: String = text.chars().skip(skip).collect();
    pad_to(&format!("{label}{tail}{cursor}"), POPUP_TEXT)
}

/// The `current` row: the value `key` has in the list, which is the value in
/// use now unless the box says the list is stale, or `not set` for a key
/// with none or one the list does not show.
///
/// Read off the list rather than through `confkv::get`, which answers a
/// `[models]` glob the config has never named with the zero that absence
/// stands for: a new key's `current` would read `0.0` for a price nobody
/// has set.
fn current(state: &State, key: &str) -> String {
    let value = state
        .settings
        .iter()
        .find(|s| s.key() == key)
        .map_or("", |s| s.value.as_str());
    format!("current   {}", shown(value))
}

/// The set popup's keys, in the popup and on the line under the frame.
const SET_KEYS: &[(&str, &str)] = &[("enter", "save"), ("esc", "back")];

/// The new key popup's keys, in the popup and on the line under the frame.
const NEW_KEY_KEYS: &[(&str, &str)] = &[("tab", "next field"), ("enter", "save"), ("esc", "back")];

/// The search row's keys, the same the queue tab's search names.
const SEARCH_KEYS: &[(&str, &str)] = &[("enter", "leave search"), ("esc", "cancel")];

/// The key line under the frame: whatever is open names its own keys, and
/// the list names what `enter` does on the row under the cursor.
fn footer(state: &State) -> String {
    let pairs: Vec<(&str, &str)> = match &state.mode {
        Mode::Search => SEARCH_KEYS.to_vec(),
        Mode::Set { .. } => SET_KEYS.to_vec(),
        Mode::NewKey { .. } => NEW_KEY_KEYS.to_vec(),
        Mode::Notice(_) => vec![("enter", "confirm")],
        Mode::Browsing if state.section == Section::Pipeline => state.pipeline.footer(),
        Mode::Browsing if state.section != Section::Config => {
            [[("tab", "section")].as_slice(), shell::quit_hint()].concat()
        }
        Mode::Browsing => {
            let enter = match state.under_cursor() {
                Some(_) => ("enter", "set value"),
                None => ("enter", "new key"),
            };
            [
                [enter, ("f", "search"), ("tab", "section")].as_slice(),
                shell::quit_hint(),
            ]
            .concat()
        }
    };
    super::key_hint(&pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project with a tracked config and no layer yet, its machine home
    /// inside the fixture so a write never reaches the real one.
    pub(super) fn project(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let (repo, guard) = crate::status::testutil::fixture(&format!("overrides-tab-{name}"));
        let path = Config::path_in(&repo.root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[dispatch]\nlane_quiet = \"15m\"\n").unwrap();
        (repo, guard)
    }

    /// `keys` typed at `state` one key at a time, decoded the way the
    /// terminal's bytes are.
    pub(super) fn type_keys(repo: &Repo, state: &mut State, keys: &str) {
        let mut input = std::io::Cursor::new(keys.as_bytes().to_vec());
        while let Some(key) = super::super::read_key(&mut input) {
            handle_key(repo, state, key);
        }
    }

    /// The frame's lines with the colour of its key line taken out.
    pub(super) fn lines(state: &State) -> Vec<String> {
        frame(state).iter().map(|l| shell::plain(l)).collect()
    }

    /// The layer's config patch, as the file holds it.
    fn layer(repo: &Repo) -> String {
        std::fs::read_to_string(crate::overrides::config_patch_path(&repo.overrides_dir()))
            .unwrap_or_default()
    }

    /// Put the cursor on `key`'s row.
    fn cursor_on(state: &mut State, key: &str) {
        let at = state.matches().iter().position(|s| s.key() == key).unwrap();
        state.cursor = at + 1;
    }

    // `tab` moves the brackets through the four sections in the box title
    // and wraps from active back to config, which is where the tab opens.
    #[test]
    fn tab_walks_the_sections_in_the_box_title_and_wraps_to_config() {
        let (repo, _guard) = project("sections");
        let mut state = State::new(&repo);
        let mut titles = vec![top_border(&state, 80)];
        for _ in 0..4 {
            type_keys(&repo, &mut state, "\t");
            titles.push(top_border(&state, 80));
        }
        let dashes = |n| "─".repeat(n);
        assert_eq!(
            titles,
            [
                format!(
                    "╭─ [config]   pipeline   prompts   active (0) {}╮",
                    dashes(33)
                ),
                format!(
                    "╭─ config   [pipeline]   prompts   active (0) {}╮",
                    dashes(33)
                ),
                format!(
                    "╭─ config   pipeline   [prompts]   active (0) {}╮",
                    dashes(33)
                ),
                format!(
                    "╭─ config   pipeline   prompts   [active (0)] {}╮",
                    dashes(33)
                ),
                format!(
                    "╭─ [config]   pipeline   prompts   active (0) {}╮",
                    dashes(33)
                ),
            ]
        );
    }

    // The sections with no list yet draw an empty box, and name only the
    // key that leaves them.
    #[test]
    fn a_section_with_no_list_yet_draws_an_empty_box() {
        let (repo, _guard) = project("empty-sections");
        let mut state = State::new(&repo);
        type_keys(&repo, &mut state, "\t\t");
        let drawn = lines(&state);
        assert!(!drawn.iter().any(|l| l.contains("new key")), "{drawn:?}");
        assert_eq!(drawn.last().unwrap(), " [tab] section");
    }

    // Every config key, every pipeline step field and a pipeline's
    // description count one each, and a prompt counts as one however long
    // it is.
    #[test]
    fn active_counts_every_key_in_the_layer_with_a_prompt_as_one() {
        let (repo, _guard) = project("active-count");
        let dir = repo.overrides_dir();
        crate::commands::write_config_override(&repo, "dispatch.lane_quiet", "5m").unwrap();
        crate::commands::write_config_override(&repo, "dispatch.visible_lanes", "4").unwrap();
        let pipeline = crate::overrides::pipeline_patch_path(&dir, "impl");
        std::fs::create_dir_all(pipeline.parent().unwrap()).unwrap();
        std::fs::write(
            &pipeline,
            "description: mine\nsteps:\n  implement:\n    model: opus\n    effort: high\n",
        )
        .unwrap();
        let prompt = crate::overrides::prompt_patch_path(&dir, "reviewer");
        std::fs::create_dir_all(prompt.parent().unwrap()).unwrap();
        std::fs::write(&prompt, "one\ntwo\nthree\n").unwrap();

        let state = State::new(&repo);
        assert_eq!(state.active, 2 + 3 + 1);
        assert!(top_border(&state, 80).contains("   active (6) "));
    }

    // The list as the mockup draws it: `+ new key` first, each key under
    // its table with the value in use now, and `not set` for a key with
    // none.
    #[test]
    fn the_config_list_shows_every_key_under_its_table_with_not_set() {
        let (repo, _guard) = project("list");
        let _terminal = crate::screen::test_terminal((80, 200));
        let mut state = State::new(&repo);
        cursor_on(&mut state, "dispatch.lane_quiet");
        let drawn = lines(&state);
        assert_eq!(drawn[1], format!("│{}│", " ".repeat(78)));
        assert_eq!(drawn[2], format!("│   + new key{}│", " ".repeat(66)));
        for row in [
            "│   dispatch",
            "│ >   lane_quiet                  15m",
            "│     visible_lanes               3",
            "│     hide_commands               false",
            "│   watch",
            "│     dirs                        not set",
        ] {
            assert!(
                drawn.iter().any(|l| l.starts_with(row)),
                "{row}: {drawn:#?}"
            );
        }
        assert_eq!(
            drawn
                .iter()
                .filter(|l| l.trim_end_matches('│').trim() == "│   dispatch")
                .count(),
            1,
            "one heading per table"
        );
        assert_eq!(
            drawn.last().unwrap(),
            " [enter] set value   [f] search   [tab] section"
        );
    }

    // `f` opens the search row, which narrows the list on key and value as
    // it is typed; `enter` keeps the results and `esc` clears them.
    #[test]
    fn f_opens_the_search_row_and_esc_clears_it() {
        let (repo, _guard) = project("search");
        let mut state = State::new(&repo);
        let all = state.matches().len();
        type_keys(&repo, &mut state, "fnot set");
        let drawn = lines(&state);
        assert!(drawn[1].starts_with("│ search: not set▏"), "{drawn:?}");
        assert_eq!(
            drawn.last().unwrap(),
            " [enter] leave search   [esc] cancel"
        );
        assert!(state.matches().iter().all(|s| s.value.is_empty()));
        assert!(state.matches().iter().any(|s| s.key() == "watch.dirs"));

        type_keys(&repo, &mut state, "\r");
        assert!(matches!(state.mode, Mode::Browsing));
        assert!(state.matches().len() < all, "enter keeps the results");
        assert!(!lines(&state)[1].contains("search:"));

        type_keys(&repo, &mut state, "fzzz");
        assert!(
            lines(&state)
                .iter()
                .any(|l| l.contains("no keys match this search"))
        );
        type_keys(&repo, &mut state, "\x1b");
        assert_eq!(state.matches().len(), all, "esc clears the search");
    }

    // A list taller than the screen keeps the cursor's row in view and
    // spends its last row on how many keys are out of sight.
    #[test]
    fn a_list_taller_than_the_screen_scrolls_with_the_cursor() {
        let (repo, _guard) = project("scroll");
        let _terminal = crate::screen::test_terminal((80, 16));
        let mut state = State::new(&repo);
        let drawn = lines(&state);
        assert_eq!(drawn.len(), 16 - 1, "one row is left for the cursor");
        let marker = &drawn[drawn.len() - 3];
        assert!(
            marker.contains("keys below") && !marker.contains("above"),
            "{marker}"
        );

        let down = "\x1b[B".repeat(state.matches().len());
        type_keys(&repo, &mut state, &down);
        let drawn = lines(&state);
        let marker = &drawn[drawn.len() - 3];
        assert!(
            marker.contains("keys above") && !marker.contains("below"),
            "{marker}"
        );
        assert!(drawn.iter().any(|l| l.starts_with("│ > ")), "{drawn:#?}");
    }

    // `enter` on a key opens the field filled with the value in use now,
    // `current` shows it, and `enter` writes what was typed.
    #[test]
    fn enter_on_a_key_sets_it_in_the_layer_as_typed() {
        let (repo, _guard) = project("set");
        let mut state = State::new(&repo);
        cursor_on(&mut state, "dispatch.lane_quiet");
        type_keys(&repo, &mut state, "\r");
        let drawn = lines(&state);
        let popup = |text: &str| drawn.iter().any(|l| l.contains(text));
        assert!(
            popup("╭─ set dispatch.lane_quiet ────────────────────────╮"),
            "{drawn:#?}"
        );
        assert!(popup(&format!("│  {}│", pad_to("value: 15m▏", 48))));
        assert!(popup(&format!("│  {}│", pad_to("current   15m", 48))));
        assert!(popup(&format!(
            "│  {}│",
            pad_to("[enter] save   [esc] back", 48)
        )));
        assert_eq!(drawn.last().unwrap(), " [enter] save   [esc] back");

        type_keys(&repo, &mut state, "\x7f\x7f\x7f5m\r");
        assert!(matches!(state.mode, Mode::Browsing));
        assert!(
            layer(&repo).contains("lane_quiet = \"5m\""),
            "{}",
            layer(&repo)
        );
        assert_eq!(state.active, 1);

        // The field opens on the new value, and `current` says it is in use.
        type_keys(&repo, &mut state, "\r");
        assert!(lines(&state).iter().any(|l| l.contains("current   5m")));
    }

    // A field emptied before `enter` writes nothing, and `esc` writes
    // nothing whatever the field holds.
    #[test]
    fn an_empty_field_or_esc_writes_nothing() {
        let (repo, _guard) = project("set-empty");
        let mut state = State::new(&repo);
        cursor_on(&mut state, "dispatch.lane_quiet");
        type_keys(&repo, &mut state, "\r\x7f\x7f\x7f\r");
        type_keys(&repo, &mut state, "\r\x7f\x7f\x7f   \r");
        type_keys(&repo, &mut state, "\r9m\x1b");
        assert!(matches!(state.mode, Mode::Browsing));
        assert!(!crate::overrides::config_patch_path(&repo.overrides_dir()).exists());
    }

    // A key with no value reads `not set` in its `current` row and opens on
    // an empty field.
    #[test]
    fn a_key_with_no_value_opens_on_an_empty_field() {
        let (repo, _guard) = project("set-unset");
        let mut state = State::new(&repo);
        cursor_on(&mut state, "watch.dirs");
        type_keys(&repo, &mut state, "\r");
        let drawn = lines(&state);
        assert!(
            drawn.iter().any(|l| l.contains("│  value: ▏ ")),
            "{drawn:#?}"
        );
        assert!(drawn.iter().any(|l| l.contains("│  current   not set ")));
    }

    // `+ new key` opens a key field and a value field, `tab` moves between
    // them, and `enter` writes the typed key with the typed value.
    #[test]
    fn new_key_writes_the_typed_key_and_value() {
        let (repo, _guard) = project("new-key");
        let mut state = State::new(&repo);
        assert_eq!(
            lines(&state).last().unwrap(),
            " [enter] new key   [f] search   [tab] section"
        );
        type_keys(&repo, &mut state, "\rmodels.qwen3-coder.input\t0.4");
        let drawn = lines(&state);
        let row = |text: &str| format!("│  {}│", pad_to(text, 48));
        for text in [
            "key:     models.qwen3-coder.input",
            "value:   0.4▏",
            "current   not set",
            "[tab] next field   [enter] save   [esc] back",
        ] {
            assert!(
                drawn.iter().any(|l| l.contains(&row(text))),
                "{text}: {drawn:#?}"
            );
        }
        assert!(drawn.iter().any(|l| l.contains("╭─ new key ─")));

        // Back to the key field: the cursor moves with the keys.
        type_keys(&repo, &mut state, "\t");
        let drawn = lines(&state);
        assert!(
            drawn
                .iter()
                .any(|l| l.contains(&row("key:     models.qwen3-coder.input▏")))
        );
        assert!(drawn.iter().any(|l| l.contains(&row("value:   0.4"))));

        type_keys(&repo, &mut state, "\r");
        let text = layer(&repo);
        assert!(text.contains("[models.qwen3-coder]"), "{text}");
        assert!(text.contains("input = 0.4"), "{text}");
    }

    // A value is written as the TOML it reads as: a number, a boolean and an
    // array keep their type, and a duration, a number with a leading zero
    // and plain text are written as strings.
    #[test]
    fn each_value_is_written_as_the_toml_it_reads_as() {
        let (repo, _guard) = project("typed");
        let mut state = State::new(&repo);
        for (key, value) in [
            ("dispatch.visible_lanes", "5"),
            ("dispatch.hide_commands", "true"),
            ("watch.dirs", "[\"a\", \"b\"]"),
            ("dispatch.lane_quiet", "5m"),
            ("dispatch.lane_child_ceiling", "010"),
            ("dispatch.priority", "group first"),
        ] {
            save(&repo, &mut state, key, value);
        }
        let text = layer(&repo);
        for line in [
            "visible_lanes = 5\n",
            "hide_commands = true\n",
            "dirs = [\"a\", \"b\"]\n",
            "lane_quiet = \"5m\"\n",
            "lane_child_ceiling = \"010\"\n",
            "priority = \"group first\"\n",
        ] {
            assert!(text.contains(line), "{line}: {text}");
        }
    }

    // A write the layer refuses, such as into a patch that does not parse,
    // is held as a notice naming the key, and `enter` closes it with the
    // file left as it was.
    #[test]
    fn a_write_that_fails_says_so_until_enter() {
        let (repo, _guard) = project("write-fails");
        let path = crate::overrides::config_patch_path(&repo.overrides_dir());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[dispatch\n").unwrap();
        let mut state = State::new(&repo);
        cursor_on(&mut state, "dispatch.lane_quiet");
        type_keys(&repo, &mut state, "\r\x7f\x7f\x7f5m\r");
        let drawn = lines(&state);
        assert!(
            drawn.iter().any(|l| l.contains("╭─ not written ─")),
            "{drawn:#?}"
        );
        assert!(
            drawn
                .iter()
                .any(|l| l.contains("dispatch.lane_quiet was not written")),
            "{drawn:#?}"
        );
        assert_eq!(drawn.last().unwrap(), " [enter] confirm");
        type_keys(&repo, &mut state, "x\x1b");
        assert!(
            matches!(state.mode, Mode::Notice(_)),
            "only enter closes it"
        );
        type_keys(&repo, &mut state, "\r");
        assert!(matches!(state.mode, Mode::Browsing));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[dispatch\n");
    }

    // The tab under the shell's strip, as the mockup draws it opened: the
    // strip, the box title and the key line with `q`.
    #[test]
    fn the_tab_opens_on_config_under_the_strip() {
        let (repo, _guard) = project("hosted");
        let _hosting = shell::Hosting::open(shell::Tab::Overrides);
        let mut input = std::io::Cursor::new(b"\x1b[C".to_vec());
        let mut out = Vec::new();
        let leave = tab(
            &repo,
            &mut super::super::frame_writer::FrameWriter::new(),
            &mut input,
            &mut out,
        )
        .unwrap();
        assert_eq!(leave, Leave::Switch(shell::Toward::Right));
        let drawn = shell::plain(&String::from_utf8(out).unwrap());
        assert!(drawn.contains("[OVERRIDES]"), "{drawn}");
        assert!(drawn.contains("╭─ [config]   pipeline   prompts   active (0) ─"));
        assert!(drawn.contains("[enter] new key   [f] search   [tab] section   [q] quit"));
    }

    // A popup taller than the list a short terminal leaves room for is
    // drawn over the rows there are: the frame stays one row short of the
    // terminal, so the strip is not scrolled away.
    #[test]
    fn a_popup_on_a_short_terminal_keeps_the_frame_inside_it() {
        let (repo, _guard) = project("short");
        let _terminal = crate::screen::test_terminal((80, 12));
        let mut state = State::new(&repo);
        type_keys(&repo, &mut state, "\r");
        assert!(matches!(state.mode, Mode::NewKey { .. }));
        assert_eq!(lines(&state).len(), 12 - 1, "{:#?}", lines(&state));

        type_keys(&repo, &mut state, "\x1b");
        cursor_on(&mut state, "dispatch.lane_quiet");
        type_keys(&repo, &mut state, "\r");
        assert!(matches!(state.mode, Mode::Set { .. }));
        assert_eq!(lines(&state).len(), 12 - 1, "{:#?}", lines(&state));
    }

    // A layer that no longer merges is named at the top of the box, and
    // stays there however far the list scrolls; once it merges again the
    // note goes.
    #[test]
    fn a_layer_that_no_longer_merges_is_said_above_the_list() {
        let (repo, _guard) = project("stale-layer");
        let _terminal = crate::screen::test_terminal((80, 16));
        let path = crate::overrides::config_patch_path(&repo.overrides_dir());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[dispatch\n").unwrap();
        let mut state = State::new(&repo);
        let note = |state: &State| {
            lines(state)
                .iter()
                .map(|l| l.trim_matches('│').trim().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let drawn = note(&state);
        assert!(drawn.contains("no longer merges"), "{drawn}");
        assert!(drawn.contains("TOML parse error"), "{drawn}");
        assert!(drawn.contains("`spoolway config override`"), "{drawn}");

        let down = "\x1b[B".repeat(state.matches().len());
        type_keys(&repo, &mut state, &down);
        assert_eq!(lines(&state).len(), 16 - 1);
        assert!(note(&state).contains("no longer merges"), "scrolled away");

        std::fs::write(&path, "[dispatch]\nlane_quiet = \"5m\"\n").unwrap();
        state.reload(&repo);
        assert!(!note(&state).contains("no longer merges"));
        assert_eq!(state.active, 1);
    }

    // A tracked config that no longer loads is named the same way, with the
    // command that edits it.
    #[test]
    fn a_tracked_config_that_no_longer_loads_is_said_above_the_list() {
        let (repo, _guard) = project("stale-tracked");
        std::fs::write(Config::path_in(&repo.checkout), "[dispatch\n").unwrap();
        let state = State::new(&repo);
        let stale = state.stale.as_deref().unwrap_or_default();
        assert!(stale.starts_with("the tracked config "), "{stale}");
        assert!(stale.contains("`spoolway config edit`"), "{stale}");
        assert!(!state.settings.is_empty(), "the list is kept");
    }

    /// Input that stays quiet for three poll slices, resizing the terminal
    /// on the second, and then types `→`.
    struct ResizedWhileIdle {
        slices: std::cell::Cell<usize>,
        keys: std::io::Cursor<Vec<u8>>,
    }

    impl std::io::Read for ResizedWhileIdle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.keys.read(buf)
        }
    }

    impl PollableRead for ResizedWhileIdle {
        fn byte_pending(&self, _timeout: std::time::Duration) -> bool {
            let slice = self.slices.get();
            self.slices.set(slice + 1);
            if slice == 1 {
                // Cleared by the test's own guard when it drops.
                std::mem::forget(crate::screen::test_terminal((100, 24)));
            }
            slice >= 3
        }
    }

    // A pane resized while nothing is typed is drawn again at its new size
    // on the next idle slice, and an idle slice with nothing changed writes
    // nothing.
    #[test]
    fn a_pane_resized_while_idle_is_drawn_again() {
        let (repo, _guard) = project("resize");
        let _terminal = crate::screen::test_terminal((80, 24));
        let _hosting = shell::Hosting::open(shell::Tab::Overrides);
        let mut input = ResizedWhileIdle {
            slices: std::cell::Cell::new(0),
            keys: std::io::Cursor::new(b"\x1b[C".to_vec()),
        };
        let mut out = Vec::new();
        tab(
            &repo,
            &mut super::super::frame_writer::FrameWriter::new(),
            &mut input,
            &mut out,
        )
        .unwrap();
        let drawn = String::from_utf8(out).unwrap();
        assert_eq!(drawn.matches("\x1b[?2026h").count(), 2, "{drawn:?}");
        let wide = format!("{}╮", "─".repeat(50));
        assert!(shell::plain(&drawn).contains(&wide), "{drawn:?}");
    }
}
