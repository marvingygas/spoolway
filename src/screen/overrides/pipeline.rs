//! The overrides tab's pipeline section: one step field set on many pipeline
//! steps at once.
//!
//! The layer keeps one patch per pipeline, keyed by step id, but a person
//! thinks "every step on this model uses that one now", whatever the steps
//! are called. So the section lists step fields, not pipelines. `enter` on a
//! field lists every tracked pipeline with a step that takes it, each
//! followed by those steps, so one key selects any one step. The person
//! selects steps across as many pipelines as they like, types one value, and
//! it lands as a step patch in every pipeline with a selected step.
//!
//! A step that leaves the field out reads `not set` and is selected the same
//! way as any other, since overriding often means setting what a pipeline
//! leaves out. The value is written as the YAML it reads as, the same as
//! `pipeline override --set`, and is not checked here: a value the merge
//! cannot use stays in the layer, where `override list` reports it as
//! ignored.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};

use super::{POPUP_TEXT, SEARCH_KEYS, SET_KEYS, edit, field, one_line, shown};
use crate::config::Config;
use crate::overrides::PipelinePatch;
use crate::pipeline::{Pipelines, StepKind};
use crate::repo::Repo;
use crate::screen::{Key, Notice, keys, pad_to, plural};

/// A step field the section lists, and the kind of step that takes it:
/// `None` for a field every step takes, whichever kind it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Field {
    name: &'static str,
    kind: Option<StepKind>,
}

/// The fields the section lists, in the order they are drawn: the ones
/// every step takes first, then agent fields, then command fields. Every
/// step key but `id`, which the merge refuses because a patch is keyed by
/// it. Agent fields go only on agent steps and command fields only on
/// command steps, because `pipeline check` refuses anything else: `slot`
/// is an agent field since a command step takes no worker slot, and
/// `gate` one since a command has no lane to tell a person what it did.
const FIELDS: [Field; 19] = [
    Field::every("description"),
    Field::every("on_pass"),
    Field::every("on_fail"),
    Field::every("loop"),
    Field::every("last"),
    Field::every("first"),
    Field::agent("model"),
    Field::agent("effort"),
    Field::agent("agent"),
    Field::agent("prompt"),
    Field::agent("session"),
    Field::agent("skills"),
    Field::agent("slot"),
    Field::agent("gate"),
    Field::command("run"),
    Field::command("timeout"),
    Field::command("hide"),
    Field::command("background"),
    Field::command("serial"),
];

impl Field {
    const fn every(name: &'static str) -> Field {
        Field { name, kind: None }
    }

    const fn agent(name: &'static str) -> Field {
        Field {
            name,
            kind: Some(StepKind::Agent),
        }
    }

    const fn command(name: &'static str) -> Field {
        Field {
            name,
            kind: Some(StepKind::Command),
        }
    }

    /// Whether a step of `kind` takes this field.
    fn takes(self, kind: StepKind) -> bool {
        self.kind.is_none_or(|own| own == kind)
    }
}

/// The header each kind's fields are listed under, `None` being the fields
/// every step takes.
fn kind_header(kind: Option<StepKind>) -> &'static str {
    match kind {
        None => "all steps",
        Some(StepKind::Agent) => "agent steps",
        Some(StepKind::Command) => "command steps",
    }
}

/// One step of a tracked pipeline, as the selection shows it.
#[derive(Debug, Clone)]
struct StepRow {
    id: String,
    kind: StepKind,
    /// Each field its kind takes, with the value in use now: the tracked
    /// step with the layer merged onto it. Empty for a field it leaves out.
    values: BTreeMap<&'static str, String>,
    /// The fields the layer's patch already sets on this step.
    overridden: BTreeSet<String>,
}

impl StepRow {
    fn value(&self, field: Field) -> &str {
        self.values.get(field.name).map_or("", String::as_str)
    }
}

/// One tracked pipeline and its steps, in file order.
#[derive(Debug, Clone)]
struct PipelineRows {
    name: String,
    steps: Vec<StepRow>,
}

impl PipelineRows {
    /// The indexes of the steps that take `field`.
    fn taking(&self, field: Field) -> impl Iterator<Item = usize> + '_ {
        self.steps
            .iter()
            .enumerate()
            .filter(move |(_, step)| field.takes(step.kind))
            .map(|(at, _)| at)
    }
}

/// Every tracked pipeline, each step with its values in use now, and why
/// the list is empty or shows a pipeline's tracked values when it does.
///
/// Only the pipelines in the repo's `.spoolway/`: a private pipeline's patch
/// waits until the pipeline is promoted, so a value set on one would read as
/// written and do nothing. The `blocked` step is left out too: every
/// pipeline gets it from `[unattended]`, which the config section sets once
/// for all of them.
///
/// Each patch is merged here, from the repo's own layer, the way the config
/// section merges `config.toml`. A patch that does not read leaves only its
/// own pipeline on the tracked values, and every such patch is named above
/// the list.
fn load(repo: &Repo, config: &Config) -> (Vec<PipelineRows>, Option<String>) {
    let tracked = match Pipelines::load_tracked(&repo.checkout, config) {
        Ok(tracked) => tracked,
        Err(err) => {
            return (
                Vec::new(),
                Some(format!(
                    "the tracked pipelines no longer load: {}. fix the file it names, then \
                     `spoolway pipeline check` says whether they load again.",
                    one_line(&err)
                )),
            );
        }
    };
    let overrides = repo.overrides_dir();
    let mut stale = Vec::new();
    let pipelines = tracked
        .pipelines
        .values()
        .map(|pipeline| {
            let mut merged = pipeline.clone();
            if let Err(err) = crate::overrides::apply_pipeline_patch(&mut merged, &overrides) {
                merged = pipeline.clone();
                stale.push((pipeline.name.clone(), one_line(&err)));
            }
            let patch = crate::overrides::read_pipeline_patch(&overrides, &pipeline.name)
                .ok()
                .flatten()
                .unwrap_or_default();
            let steps = merged
                .steps
                .iter()
                .filter(|step| step.id != crate::pipeline::BLOCKED)
                .map(|step| {
                    let kind = step.kind();
                    let values = FIELDS
                        .iter()
                        .filter(|field| field.takes(kind))
                        .map(|field| (field.name, step_value(step, field.name)))
                        .collect();
                    let overridden = patch
                        .steps
                        .get(&step.id)
                        .map(|fields| {
                            fields
                                .keys()
                                .filter_map(|key| key.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    StepRow {
                        id: step.id.clone(),
                        kind,
                        values,
                        overridden,
                    }
                })
                .collect();
            PipelineRows {
                name: pipeline.name.clone(),
                steps,
            }
        })
        .collect();
    (pipelines, stale_note(&stale))
}

/// The note above the list naming every pipeline whose patch no longer
/// merges, each with why and the command that drops it, or `None` when
/// every patch merges.
fn stale_note(stale: &[(String, String)]) -> Option<String> {
    match stale {
        [] => None,
        [(name, err)] => Some(format!(
            "the patch on {name} no longer merges: {err}. its values are the tracked ones; fix \
             the file it names, or drop it with `spoolway override drop {name}`."
        )),
        many => {
            let names: Vec<&str> = many.iter().map(|(name, _)| name.as_str()).collect();
            let errors: Vec<String> = many
                .iter()
                .map(|(name, err)| format!("{name}: {err}"))
                .collect();
            let drops: Vec<String> = names
                .iter()
                .map(|name| format!("`spoolway override drop {name}`"))
                .collect();
            Some(format!(
                "the patches on {} no longer merge: {}. their values are the tracked ones; fix \
                 the files they name, or drop them with {}.",
                names.join(", "),
                errors.join("; "),
                drops.join(" and ")
            ))
        }
    }
}

/// `step`'s value for the field `name`, as the list shows it: empty for a
/// field it leaves out. A blank string reads as left out too. The shipped
/// pipelines carry `model: ""` and `effort: ""` on every agent step, which
/// send nothing, and YAML would spell them `''`. The active section reads
/// the setup's values the same way.
pub(super) fn step_value(step: &crate::pipeline::Step, name: &str) -> String {
    match crate::overrides::field_display(step, name) {
        Ok(value) if value != "''" => value,
        _ => String::new(),
    }
}

/// The steps picked for one field, and how the picking list is drawn.
#[derive(Debug, Clone)]
struct Selection {
    field: Field,
    /// The search query. Kept, and shown above the list, after `enter`
    /// leaves the search row: it decides which steps a pipeline selects, so
    /// a narrowed list has to say so.
    filter: String,
    /// Whether the search row has the keys.
    searching: bool,
    /// Which of [`picking_rows`] the cursor is on.
    cursor: usize,
    /// Every selected step, as (pipeline, step id).
    selected: BTreeSet<(String, String)>,
}

impl Selection {
    /// A selection on `field` with the steps whose patch already sets it
    /// selected.
    fn new(field: Field, pipelines: &[PipelineRows]) -> Selection {
        let mut selection = Selection {
            field,
            filter: String::new(),
            searching: false,
            cursor: 0,
            selected: BTreeSet::new(),
        };
        for pipeline in pipelines {
            for at in pipeline.taking(field) {
                let step = &pipeline.steps[at];
                if step.overridden.contains(field.name) {
                    selection
                        .selected
                        .insert((pipeline.name.clone(), step.id.clone()));
                }
            }
        }
        selection
    }

    fn is_selected(&self, pipeline: &str, step: &str) -> bool {
        self.selected
            .contains(&(pipeline.to_string(), step.to_string()))
    }

    /// How many of `pipeline`'s steps are selected.
    fn selected_in(&self, pipeline: &str) -> usize {
        self.selected.iter().filter(|(p, _)| p == pipeline).count()
    }

    /// The selected steps, by pipeline, in the order the list draws them.
    fn targets(&self, pipelines: &[PipelineRows]) -> Vec<(String, Vec<String>)> {
        pipelines
            .iter()
            .filter_map(|pipeline| {
                let steps: Vec<String> = pipeline
                    .taking(self.field)
                    .map(|at| pipeline.steps[at].id.clone())
                    .filter(|id| self.is_selected(&pipeline.name, id))
                    .collect();
                (!steps.is_empty()).then(|| (pipeline.name.clone(), steps))
            })
            .collect()
    }
}

/// One row of the picking list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// A pipeline, by its index.
    Pipeline(usize),
    /// A step, by its pipeline's index and its own.
    Step(usize, usize),
}

/// The steps of `pipeline` that take the selection's field and the search
/// leaves. A pipeline whose name matches keeps every one. A step matches on
/// its id or on the value shown beside it, `not set` included, so a search
/// for that finds every step that leaves the field out.
fn visible_steps(pipeline: &PipelineRows, selection: &Selection) -> Vec<usize> {
    let query = selection.filter.to_lowercase();
    let named = pipeline.name.to_lowercase().contains(&query);
    pipeline
        .taking(selection.field)
        .filter(|&at| {
            let step = &pipeline.steps[at];
            named
                || step.id.to_lowercase().contains(&query)
                || shown(step.value(selection.field))
                    .to_lowercase()
                    .contains(&query)
        })
        .collect()
}

/// The picking list's rows: every pipeline with a step the search leaves,
/// followed by those steps. Nothing opens or closes, so only a search
/// changes which rows there are.
fn picking_rows(pipelines: &[PipelineRows], selection: &Selection) -> Vec<Row> {
    let mut rows = Vec::new();
    for (p, pipeline) in pipelines.iter().enumerate() {
        let steps = visible_steps(pipeline, selection);
        if steps.is_empty() {
            continue;
        }
        rows.push(Row::Pipeline(p));
        rows.extend(steps.into_iter().map(|s| Row::Step(p, s)));
    }
    rows
}

/// The value every selected step shares, or `None` when they differ.
fn shared_value(pipelines: &[PipelineRows], selection: &Selection) -> Option<String> {
    let mut values = pipelines.iter().flat_map(|pipeline| {
        pipeline
            .taking(selection.field)
            .map(|at| &pipeline.steps[at])
            .filter(|step| selection.is_selected(&pipeline.name, &step.id))
            .map(|step| step.value(selection.field))
    });
    let first = values.next()?;
    values.all(|v| v == first).then(|| first.to_string())
}

/// What the `current` row reads for a selection: the shared value, `not
/// set`, or `varies` when the selected steps differ.
fn current_text(pipelines: &[PipelineRows], selection: &Selection) -> String {
    match shared_value(pipelines, selection) {
        Some(value) => shown(&value).to_string(),
        None => "varies".to_string(),
    }
}

/// A finished write, as the `overrides written` popup reports it.
#[derive(Debug, Clone)]
struct Written {
    field: Field,
    steps: usize,
    pipelines: usize,
    old: String,
    new: String,
}

/// What the section shows.
#[derive(Debug, Clone)]
enum View {
    /// The field list.
    Fields,
    /// The field list's search row has the keys.
    Search,
    /// The picking list for one field.
    Select(Selection),
    /// The value field over the picking list.
    Value { selection: Selection, value: String },
    /// A write that went through, until `enter` closes it. The picking list
    /// stays under it.
    Written {
        selection: Selection,
        written: Written,
    },
    /// A write that failed, until `enter` goes back to the picking list.
    Failed {
        selection: Selection,
        notice: Notice,
    },
}

/// The pipeline section while the tab is open. Built fresh on every visit.
pub(super) struct PipelineSection {
    pipelines: Vec<PipelineRows>,
    /// Why the list is empty or shows the tracked values, drawn at the top
    /// of the box.
    pub(super) stale: Option<String>,
    view: View,
    /// Which field the cursor is on, among the ones the search leaves.
    cursor: usize,
    /// The field list's search query, kept after `enter` leaves the row.
    filter: String,
}

impl PipelineSection {
    pub(super) fn new() -> PipelineSection {
        PipelineSection {
            pipelines: Vec::new(),
            stale: None,
            view: View::Fields,
            cursor: 0,
            filter: String::new(),
        }
    }

    /// Read the pipelines and the layer again, after a write or on a visit,
    /// with `config` the config in use now.
    pub(super) fn reload(&mut self, repo: &Repo, config: &Config) {
        let (pipelines, stale) = load(repo, config);
        self.pipelines = pipelines;
        self.stale = stale;
        self.cursor = self.cursor.min(self.fields().len().saturating_sub(1));
    }

    /// Whether the field list has the keys, with nothing open over it: the
    /// only time `tab`, `q` and `←`/`→` reach past the section.
    pub(super) fn at_rest(&self) -> bool {
        matches!(self.view, View::Fields)
    }

    /// The fields the search leaves, in list order.
    fn fields(&self) -> Vec<Field> {
        let query = self.filter.to_lowercase();
        FIELDS
            .iter()
            .copied()
            .filter(|field| field.name.contains(&query))
            .collect()
    }

    /// One key, read by whatever the section shows. Answers whether the
    /// layer was written, so the tab can count its keys again.
    pub(super) fn handle_key(&mut self, repo: &Repo, key: Key) -> bool {
        match std::mem::replace(&mut self.view, View::Fields) {
            View::Fields => match key {
                Key::Up | Key::Down => self.move_cursor(key),
                Key::Char('f') => self.view = View::Search,
                Key::Enter => {
                    if let Some(field) = self.fields().get(self.cursor).copied() {
                        self.view = View::Select(Selection::new(field, &self.pipelines));
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
                self.cursor = self.cursor.min(self.fields().len().saturating_sub(1));
                if key != Key::Esc {
                    self.view = View::Search;
                }
            }
            View::Select(selection) => {
                self.view = self.select_key(selection, key);
            }
            View::Value { selection, value } => match key {
                Key::Esc => self.view = View::Select(selection),
                // `enter` on an empty field is how the popup is left without
                // a write, as on the config section.
                Key::Enter if value.trim().is_empty() => self.view = View::Select(selection),
                Key::Enter => return self.save(repo, selection, &value),
                other => {
                    let mut value = value;
                    edit(&mut value, other);
                    self.view = View::Value { selection, value };
                }
            },
            View::Written { selection, written } => {
                if key != Key::Enter {
                    self.view = View::Written { selection, written };
                }
            }
            View::Failed { selection, notice } => {
                self.view = match key {
                    Key::Enter => View::Select(selection),
                    _ => View::Failed { selection, notice },
                };
            }
        }
        false
    }

    fn move_cursor(&mut self, key: Key) {
        self.cursor = crate::screen::move_cursor(self.cursor, self.fields().len(), key);
    }

    /// The field under the cursor while the field list has the keys, for
    /// `o`'s `delete override`. `None` anywhere else, where `o` is a letter
    /// of a search or does nothing.
    pub(super) fn field_under_cursor(&self) -> Option<&'static str> {
        match self.view {
            View::Fields => self.fields().get(self.cursor).map(|field| field.name),
            _ => None,
        }
    }

    /// One key over the picking list, answering what the section shows next.
    fn select_key(&self, mut selection: Selection, key: Key) -> View {
        let before = picking_rows(&self.pipelines, &selection)
            .get(selection.cursor)
            .copied();
        if selection.searching {
            match key {
                Key::Enter => selection.searching = false,
                Key::Esc => {
                    selection.filter.clear();
                    selection.searching = false;
                }
                Key::Up | Key::Down => {
                    self.move_selection(&mut selection, key);
                    return View::Select(selection);
                }
                other => edit(&mut selection.filter, other),
            }
            self.refocus(&mut selection, before);
            return View::Select(selection);
        }
        match key {
            Key::Esc => return View::Fields,
            Key::Up | Key::Down => {
                self.move_selection(&mut selection, key);
                return View::Select(selection);
            }
            Key::Char('f') => selection.searching = true,
            Key::Char(' ') => match before {
                Some(Row::Pipeline(p)) => self.toggle_pipeline(&mut selection, p),
                Some(Row::Step(p, s)) => {
                    let id = (
                        self.pipelines[p].name.clone(),
                        self.pipelines[p].steps[s].id.clone(),
                    );
                    if !selection.selected.remove(&id) {
                        selection.selected.insert(id);
                    }
                }
                None => {}
            },
            Key::Char('a') => self.toggle_all(&mut selection),
            // Nothing selected leaves nothing to set, so `enter` waits for a
            // step, as the trial picker's does for a pipeline.
            Key::Enter if !selection.selected.is_empty() => {
                let value = shared_value(&self.pipelines, &selection).unwrap_or_default();
                return View::Value { selection, value };
            }
            _ => {}
        }
        self.refocus(&mut selection, before);
        View::Select(selection)
    }

    fn move_selection(&self, selection: &mut Selection, key: Key) {
        let len = picking_rows(&self.pipelines, selection).len();
        selection.cursor = crate::screen::move_cursor(selection.cursor, len, key);
    }

    /// Keep the cursor on the row it was on before a key changed the rows,
    /// or on that row's pipeline when the row itself is gone, as when a
    /// search edit leaves out the step the cursor was on. Every other key
    /// leaves the rows as they were, and the cursor with them.
    fn refocus(&self, selection: &mut Selection, before: Option<Row>) {
        let rows = picking_rows(&self.pipelines, selection);
        let pipeline_of = |row: Row| match row {
            Row::Pipeline(p) | Row::Step(p, _) => p,
        };
        let found = before.and_then(|before| {
            rows.iter().position(|row| *row == before).or_else(|| {
                rows.iter()
                    .position(|row| *row == Row::Pipeline(pipeline_of(before)))
            })
        });
        selection.cursor = found.unwrap_or(selection.cursor);
        selection.cursor = selection.cursor.min(rows.len().saturating_sub(1));
    }

    /// `space` on a pipeline: one with a selected step has them all
    /// cleared, and any other has every step the search leaves selected.
    fn toggle_pipeline(&self, selection: &mut Selection, p: usize) {
        let pipeline = &self.pipelines[p];
        if selection.selected_in(&pipeline.name) > 0 {
            selection
                .selected
                .retain(|(name, _)| *name != pipeline.name);
            return;
        }
        for s in visible_steps(pipeline, selection) {
            selection
                .selected
                .insert((pipeline.name.clone(), pipeline.steps[s].id.clone()));
        }
    }

    /// `a`: every step the search leaves is selected, unless they already
    /// are, and then the selection is cleared.
    /// With no step left by the search it does nothing.
    fn toggle_all(&self, selection: &mut Selection) {
        let visible: Vec<(String, String)> = self
            .pipelines
            .iter()
            .flat_map(|pipeline| {
                visible_steps(pipeline, selection)
                    .into_iter()
                    .map(|s| (pipeline.name.clone(), pipeline.steps[s].id.clone()))
            })
            .collect();
        // A search that leaves no step leaves nothing for `a` to select,
        // and clearing the selection then would drop steps the person
        // cannot see, the preselected ones among them.
        if visible.is_empty() {
            return;
        }
        if visible.iter().all(|id| selection.selected.contains(id)) {
            selection.selected.clear();
            return;
        }
        selection.selected.extend(visible);
    }

    /// Write `value` onto every selected step, then show what was written,
    /// or why none or only some of it was. Answers whether the layer
    /// changed.
    fn save(&mut self, repo: &Repo, selection: Selection, value: &str) -> bool {
        let targets = selection.targets(&self.pipelines);
        let old = current_text(&self.pipelines, &selection);
        match write(repo, selection.field.name, value, &targets) {
            Ok(()) => {
                self.view = View::Written {
                    written: Written {
                        field: selection.field,
                        steps: targets.iter().map(|(_, steps)| steps.len()).sum(),
                        pipelines: targets.len(),
                        old,
                        new: value.trim().to_string(),
                    },
                    selection,
                };
                true
            }
            Err(failure) => {
                // Only a write that stopped partway changed the layer.
                let changed = !failure.written.is_empty();
                self.view = View::Failed {
                    notice: unwritten_notice(selection.field.name, &failure),
                    selection,
                };
                changed
            }
        }
    }

    /// The box title in place of the section names, while a field's steps
    /// are being picked.
    pub(super) fn title(&self) -> Option<String> {
        self.selection()
            .map(|selection| format!("select steps for {}", selection.field.name))
    }

    fn selection(&self) -> Option<&Selection> {
        match &self.view {
            View::Fields | View::Search => None,
            View::Select(selection)
            | View::Value { selection, .. }
            | View::Written { selection, .. }
            | View::Failed { selection, .. } => Some(selection),
        }
    }

    /// The list box's lines, `width` columns each, cut to `rows` with the
    /// cursor's row kept in view: the field list, or the picking list while
    /// a field's steps are picked.
    pub(super) fn lines(&self, rows: Option<usize>, width: usize) -> Vec<String> {
        match self.selection() {
            Some(selection) => self.selection_lines(selection, rows, width),
            None => self.field_lines(rows, width),
        }
    }

    /// The field list: each kind's fields under its header, each with how
    /// many steps the layer already sets it on.
    fn field_lines(&self, rows: Option<usize>, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        if matches!(self.view, View::Search) {
            lines.push(format!("search: {}▏", self.filter));
        }
        lines.push(String::new());
        let fields = self.fields();
        let mut starts = Vec::new();
        let mut kind = None;
        for (at, field) in fields.iter().enumerate() {
            if kind != Some(field.kind) {
                kind = Some(field.kind);
                lines.push(format!("  {}", kind_header(field.kind)));
            }
            let mark = if at == self.cursor { "> " } else { "  " };
            let overridden = match self.count(*field) {
                0 => String::new(),
                n => format!("{n} overridden"),
            };
            starts.push(lines.len());
            lines.push(
                format!("{mark}  {:FIELD_NAME$}{overridden}", field.name)
                    .trim_end()
                    .to_string(),
            );
        }
        if fields.is_empty() {
            lines.push("  no fields match this search".to_string());
            return lines;
        }
        let focus = starts[self.cursor.min(starts.len() - 1)];
        window(&lines, &starts, "field", focus, rows, width)
    }

    /// On how many steps the layer already sets `field`.
    fn count(&self, field: Field) -> usize {
        self.pipelines
            .iter()
            .map(|pipeline| {
                pipeline
                    .taking(field)
                    .filter(|&at| pipeline.steps[at].overridden.contains(field.name))
                    .count()
            })
            .sum()
    }

    /// The picking list: each pipeline on one row with only its box and
    /// name, and its steps under it with their values.
    fn selection_lines(
        &self,
        selection: &Selection,
        rows: Option<usize>,
        width: usize,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        if selection.searching {
            lines.push(format!("search: {}▏", selection.filter));
        } else if !selection.filter.is_empty() {
            lines.push(format!("search: {}", selection.filter));
        }
        lines.push(String::new());
        let all = picking_rows(&self.pipelines, selection);
        if all.is_empty() {
            lines.push(match selection.filter.is_empty() {
                true => format!(
                    "  no pipeline has a step that takes {}",
                    selection.field.name
                ),
                false => "  no steps match this search".to_string(),
            });
            return lines;
        }
        let mut starts = Vec::new();
        let mut focus = 0;
        for (at, row) in all.iter().enumerate() {
            let mark = if at == selection.cursor { "> " } else { "  " };
            if at == selection.cursor {
                focus = lines.len();
            }
            match *row {
                Row::Pipeline(p) => {
                    let pipeline = &self.pipelines[p];
                    starts.push(lines.len());
                    lines.push(format!(
                        "{mark}{} {}",
                        tick(selection.selected_in(&pipeline.name) > 0),
                        pipeline.name
                    ));
                }
                Row::Step(p, s) => {
                    let (pipeline, step) = (&self.pipelines[p], &self.pipelines[p].steps[s]);
                    lines.push(format!(
                        "{mark}      {} {}{}",
                        tick(selection.is_selected(&pipeline.name, &step.id)),
                        column(&step.id, STEP_ID),
                        shown(step.value(selection.field))
                    ));
                }
            }
        }
        window(&lines, &starts, "pipeline", focus, rows, width)
    }

    /// The popup over the list, if one is open. `rows` is how many rows the
    /// list box has, so a value popup naming many pipelines stays inside it.
    pub(super) fn popup(&self, rows: Option<usize>) -> Option<Vec<String>> {
        match &self.view {
            View::Value { selection, value } => Some(self.value_popup(selection, value, rows)),
            View::Written { written, .. } => Some(written_popup(written)),
            View::Failed { notice, .. } => Some(notice.panel(crate::screen::NOTICE_WRAP)),
            View::Fields | View::Search | View::Select(_) => None,
        }
    }

    /// The value field: the typed value, the `current` row, and the
    /// selected steps of each pipeline.
    fn value_popup(&self, selection: &Selection, value: &str, rows: Option<usize>) -> Vec<String> {
        let targets = selection.targets(&self.pipelines);
        let steps: usize = targets.iter().map(|(_, steps)| steps.len()).sum();
        // The label column fits the longest pipeline name, and is never
        // narrower than `current` and the gap after it.
        let label = targets
            .iter()
            .map(|(name, _)| name.chars().count() + 2)
            .max()
            .unwrap_or(0)
            .max(CURRENT_LABEL);
        let mut body = vec![
            String::new(),
            field("value: ", value, true),
            String::new(),
            format!(
                "{:label$}{}",
                "current",
                current_text(&self.pipelines, selection)
            ),
        ];
        let blocks: Vec<Vec<String>> = targets
            .iter()
            .map(|(name, ids)| {
                crate::screen::wrap(&ids.join(", "), POPUP_TEXT.saturating_sub(label))
                    .into_iter()
                    .enumerate()
                    .map(|(at, line)| match at {
                        0 => format!("{name:label$}{line}"),
                        _ => format!("{:label$}{line}", ""),
                    })
                    .collect()
            })
            .collect();
        // The rows the popup's body may take, for `overlay` to draw it
        // whole: the box's two borders, less the row `overlay` always keeps
        // above a popup, less the popup's own two borders, blank row and
        // key row.
        let room = rows.map(|rows| (rows + 2).saturating_sub(1 + 4));
        body.extend(fit_blocks(
            blocks,
            room.map(|room| room.saturating_sub(body.len())),
            label,
            "pipeline",
        ));
        crate::screen::panel(
            &format!("set {} on {}", selection.field.name, plural(steps, "step")),
            &body,
            &keys(SET_KEYS),
        )
    }

    /// The key line under the frame: whatever the section shows names its
    /// own keys.
    pub(super) fn footer(&self) -> Vec<(&'static str, &'static str)> {
        match &self.view {
            View::Fields => [
                [
                    ("enter", "select steps"),
                    ("f", "search"),
                    super::OPTIONS,
                    super::OPEN_WORKSPACE,
                    ("tab", "section"),
                ]
                .as_slice(),
                crate::screen::shell::quit_hint(),
            ]
            .concat(),
            View::Search => SEARCH_KEYS.to_vec(),
            View::Select(selection) if selection.searching => SEARCH_KEYS.to_vec(),
            View::Select(_) => SELECT_KEYS.to_vec(),
            View::Value { .. } => SET_KEYS.to_vec(),
            View::Written { .. } => vec![("enter", "close")],
            View::Failed { .. } => vec![("enter", "confirm")],
        }
    }
}

/// The picking list's keys, on the line under the frame.
const SELECT_KEYS: &[(&str, &str)] = &[
    ("space", "select"),
    ("a", "select all"),
    ("enter", "set value"),
    ("esc", "back"),
];

/// The column a field's `overridden` note starts at, counted from the
/// field's name.
const FIELD_NAME: usize = 21;

/// The column a step's value starts at, counted from its id. The shipped
/// ids run to 15 characters. Room for a longer one on every row would push
/// every value right for one rare id, so an id of 21 characters or more
/// pushes only its own value along, one space past its end.
const STEP_ID: usize = 21;

/// How wide the value popup's label column is at least: `current` and the
/// gap after it.
const CURRENT_LABEL: usize = 10;

/// `s` padded to `width` columns, or followed by one space when it is
/// that long or longer. [`pad_to`] would cut a long step id or file name
/// to the width instead, and glue what is left of it to the value or note
/// after it.
pub(super) fn column(s: &str, width: usize) -> String {
    pad_to(s, width.max(s.chars().count() + 1))
}

/// `noun`, with an `s` unless `n` is one.
fn noun(n: usize, noun: &str) -> String {
    match n {
        1 => noun.to_string(),
        _ => format!("{noun}s"),
    }
}

/// A row's checkbox.
pub(super) fn tick(on: bool) -> &'static str {
    if on { "[x]" } else { "[ ]" }
}

/// `lines` cut to `rows` with the line `focus` kept in view, its last row
/// counting the items out of sight under the same indent as the rows.
fn window(
    lines: &[String],
    starts: &[usize],
    noun: &'static str,
    focus: usize,
    rows: Option<usize>,
    width: usize,
) -> Vec<String> {
    let cut = rows.is_some_and(|rows| lines.len() > rows);
    let mut shown = crate::screen::pane::window(
        lines,
        crate::screen::pane::Items { starts, noun },
        (focus, focus),
        rows,
        width.saturating_sub(2),
    );
    if cut && let Some(marker) = shown.last_mut() {
        *marker = format!("  {marker}");
    }
    shown
}

/// Each block of rows, one per pipeline or file, dropped from the end until
/// they fit in `room` rows, with one row saying how many of `what` were left
/// out. All of them when `room` is `None`, with no terminal to measure.
pub(super) fn fit_blocks(
    mut blocks: Vec<Vec<String>>,
    room: Option<usize>,
    label: usize,
    what: &str,
) -> Vec<String> {
    let total = blocks.len();
    if let Some(room) = room {
        let len = |blocks: &[Vec<String>]| blocks.iter().map(Vec::len).sum::<usize>();
        if len(&blocks) > room {
            while !blocks.is_empty() && len(&blocks) + 1 > room {
                blocks.pop();
            }
        }
    }
    let left_out = total - blocks.len();
    let mut lines: Vec<String> = blocks.into_iter().flatten().collect();
    if left_out > 0 {
        lines.push(format!(
            "{:label$}and {left_out} more {}",
            "",
            noun(left_out, what)
        ));
    }
    lines
}

/// The `overrides written` popup: the field, how many steps in how many
/// pipelines, and the value it replaced.
fn written_popup(written: &Written) -> Vec<String> {
    let mut body = vec![String::new()];
    let lines = [
        format!(
            "{} on {} in {}",
            written.field.name,
            plural(written.steps, "step"),
            plural(written.pipelines, "pipeline")
        ),
        format!("  {} → {}", written.old, written.new),
    ];
    for line in lines {
        body.extend(
            crate::screen::wrap(&line, POPUP_TEXT)
                .iter()
                .map(|line| pad_to(line, POPUP_TEXT)),
        );
    }
    crate::screen::panel("overrides written", &body, &keys(&[("enter", "close")]))
}

/// Write `value`, read as YAML, as the field named `field` on every step
/// `targets` names, one patch per pipeline. The section's own `save` calls
/// it, and so does the active section's `edit` on a step key, so a value
/// one of them takes the other takes too.
///
/// Every patch is built before any is written, so a patch that does not
/// read, or a value that is not YAML, leaves every file as it was rather
/// than some pipelines patched and some not. Each one starts from the patch
/// already on disk, so a key a step's patch already holds is kept beside
/// the new one. A file that fails to write after others were written leaves
/// those written, and the error names which pipelines they are.
pub(super) fn write(
    repo: &Repo,
    field: &str,
    value: &str,
    targets: &[(String, Vec<String>)],
) -> std::result::Result<(), Unwritten> {
    repo.refuse_in_lane("the overrides layer")?;
    let parsed: serde_norway::Value = serde_norway::from_str(value)
        .with_context(|| format!("`{value}` does not read as YAML, so nothing was written"))?;
    let overrides = repo.overrides_dir();
    // Held from the read of the patches to their write, so a parallel
    // `pipeline override --set` starts from what this saved.
    let _lock = crate::lock::EditLock::acquire(&repo.edit_lock_path())?;
    let mut patches = Vec::new();
    for (name, steps) in targets {
        let mut patch = crate::overrides::read_pipeline_patch(&overrides, name)
            .context("nothing was written")?
            .unwrap_or_default();
        for step in steps {
            patch.steps.entry(step.clone()).or_default().insert(
                serde_norway::Value::String(field.to_string()),
                parsed.clone(),
            );
        }
        patches.push((name.clone(), patch));
    }
    write_patches(&patches, |name, patch| {
        crate::overrides::write_pipeline_patch(&overrides, name, patch)
    })
}

/// Why [`write`] stopped, and which pipelines it had written by then.
pub(super) struct Unwritten {
    /// The pipelines whose patch was written before the failure, in order.
    /// Empty when nothing was.
    written: Vec<String>,
    /// The pipeline whose write failed and every one after it. Empty when
    /// the write stopped before any file was touched.
    unwritten: Vec<String>,
    pub(super) err: anyhow::Error,
}

/// An error from before the first file is written: nothing was.
impl From<anyhow::Error> for Unwritten {
    fn from(err: anyhow::Error) -> Self {
        Unwritten {
            written: Vec::new(),
            unwritten: Vec::new(),
            err,
        }
    }
}

/// Each of `patches` handed to `put` in order, stopping at the first that
/// fails with the pipelines written before it and those left unwritten.
fn write_patches(
    patches: &[(String, PipelinePatch)],
    mut put: impl FnMut(&str, &PipelinePatch) -> Result<()>,
) -> std::result::Result<(), Unwritten> {
    for (at, (name, patch)) in patches.iter().enumerate() {
        if let Err(err) = put(name, patch) {
            let names = |patches: &[(String, PipelinePatch)]| {
                patches.iter().map(|(name, _)| name.clone()).collect()
            };
            return Err(Unwritten {
                written: names(&patches[..at]),
                unwritten: names(&patches[at..]),
                err,
            });
        }
    }
    Ok(())
}

/// The notice for a write that stopped: `not written` when no file
/// changed, and `written in part`, naming the pipelines on each side, when
/// some did, so nobody reads a half-applied value as never set.
fn unwritten_notice(field: &str, failure: &Unwritten) -> Notice {
    let err = &failure.err;
    match failure.written.is_empty() {
        true => Notice::new(
            "not written",
            format!("{field} was not set: {err:#}. fix what stopped it and set the value again."),
        ),
        false => Notice::new(
            "written in part",
            format!(
                "{field} was set on {} but not on {}: {err:#}. fix what stopped it and set the \
                 value again.",
                failure.written.join(", "),
                failure.unwritten.join(", ")
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::State;
    use super::super::tests::{key_line, lines, project, type_keys};
    use super::*;

    /// Two agent steps on sonnet with an effort, one step that sets no
    /// effort, and a command step.
    const ALPHA: &str = "steps:
  - id: implement
    agent: claude
    model: claude-sonnet-5-5
    effort: high
    on_pass: review
  - id: review
    agent: claude
    model: claude-sonnet-5-5
    effort: high
    on_pass: check
  - id: check
    run: make test
    on_pass: create-pr
  - id: create-pr
    agent: claude
    model: claude-haiku-5-5
    on_pass: done
";

    /// One agent step on sonnet with an effort, and one with a blank
    /// effort, the way the shipped pipelines leave it.
    const BETA: &str = "steps:
  - id: fix
    agent: claude
    model: claude-sonnet-5-5
    effort: medium
    on_pass: create-pr
  - id: create-pr
    agent: claude
    model: claude-haiku-5-5
    effort: \"\"
    on_pass: done
";

    /// Command steps alone, so it takes no agent field.
    const GAMMA: &str = "steps:
  - id: build
    run: make
    on_pass: done
";

    /// A project with the three pipelines above tracked, and no layer yet.
    fn pipelines_project(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let (repo, guard) = project(&format!("pipeline-{name}"));
        let dir = Pipelines::dir_in(&repo.checkout);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in [("alpha", ALPHA), ("beta", BETA), ("gamma", GAMMA)] {
            std::fs::write(dir.join(format!("{name}.yml")), body).unwrap();
        }
        (repo, guard)
    }

    /// The overrides tab, opened on its pipeline section with the cursor
    /// on `model`, the field most of these tests set.
    fn on_pipeline(repo: &Repo) -> State {
        let mut state = State::new(repo);
        type_keys(repo, &mut state, "\t");
        type_keys(repo, &mut state, &"\x1b[B".repeat(at_field("model")));
        state
    }

    /// Where the field `name` sits in the field list, so a test reaches it
    /// with that many `↓` from the top.
    fn at_field(name: &str) -> usize {
        FIELDS.iter().position(|field| field.name == name).unwrap()
    }

    /// `pipeline`'s patch as the file holds it, or empty with none.
    fn patch(repo: &Repo, pipeline: &str) -> String {
        std::fs::read_to_string(crate::overrides::pipeline_patch_path(
            &repo.overrides_dir(),
            pipeline,
        ))
        .unwrap_or_default()
    }

    /// Write `body` as `pipeline`'s patch.
    fn write_patch(repo: &Repo, pipeline: &str, body: &str) {
        let path = crate::overrides::pipeline_patch_path(&repo.overrides_dir(), pipeline);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// The box's rows with its borders and the padding after the text taken
    /// off, so a row reads as the mockup writes it.
    fn rows_of(state: &State) -> Vec<String> {
        lines(state)
            .iter()
            .filter_map(|l| l.strip_prefix("│ "))
            .map(|l| l.trim_end_matches('│').trim_end().to_string())
            .collect()
    }

    /// The popup's rows, cut out of the frame between its borders, with
    /// its two-column margin and the padding after the text taken off.
    fn popup_rows(state: &State) -> Vec<String> {
        let drawn: Vec<Vec<char>> = lines(state).iter().map(|l| l.chars().collect()).collect();
        let Some(top) = (1..drawn.len()).find(|&at| drawn[at].contains(&'╭')) else {
            return Vec::new();
        };
        let left = drawn[top].iter().position(|c| *c == '╭').unwrap();
        let right = drawn[top].iter().position(|c| *c == '╮').unwrap();
        drawn[top + 1..]
            .iter()
            .take_while(|row| row.get(left) == Some(&'│'))
            .map(|row| {
                let inner: String = row[left + 1..right].iter().collect();
                let inner = inner.trim_end();
                inner.strip_prefix("  ").unwrap_or(inner).to_string()
            })
            .collect()
    }

    /// `esc`, on its own: the reader takes the byte after an escape as part
    /// of it, so it is never typed together with the next key.
    fn esc(repo: &Repo, state: &mut State) {
        type_keys(repo, state, "\x1b");
    }

    // The field list as the mockup draws it: the fields every step takes,
    // then each kind's own, each under its header, with how many steps the
    // layer already sets it on, and no count of the steps and pipelines
    // that take it. Every step key but `id` is listed.
    #[test]
    fn the_section_lists_step_fields_under_a_header_per_kind() {
        let (repo, _guard) = pipelines_project("fields");
        write_patch(&repo, "gamma", "steps:\n  build:\n    timeout: 5m\n");
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = State::new(&repo);
        type_keys(&repo, &mut state, "\t");
        let rows = rows_of(&state);
        assert_eq!(
            &rows[..25],
            [
                "",
                "  all steps",
                ">   description",
                "    on_pass",
                "    on_fail",
                "    loop",
                "    last",
                "    first",
                "  agent steps",
                "    model",
                "    effort",
                "    agent",
                "    prompt",
                "    session",
                "    skills",
                "    slot",
                "    gate",
                "  command steps",
                "    run",
                "    timeout              1 overridden",
                "    hide",
                "    background",
                "    serial",
                "",
                "",
            ]
        );
        assert!(
            lines(&state)[0].starts_with("╭─ config   [pipeline]   prompts   active (1) ─"),
            "{:?}",
            lines(&state)[0]
        );
        assert_eq!(
            key_line(&state),
            " [enter] select steps   [f] search   [o] options   [w] open workspace   [tab] section"
        );
    }

    // `enter` on a field lists only the pipelines with a step that takes it,
    // each followed by those steps, and with no count on its own row.
    #[test]
    fn enter_on_a_field_lists_the_pipelines_with_a_step_that_takes_it() {
        let (repo, _guard) = pipelines_project("select");
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\r");
        let drawn = lines(&state);
        assert!(
            drawn[0].starts_with("╭─ select steps for model ─"),
            "{:?}",
            drawn[0]
        );
        let rows = rows_of(&state);
        assert_eq!(
            &rows[..8],
            [
                "",
                "> [ ] alpha",
                "        [ ] implement            claude-sonnet-5-5",
                "        [ ] review               claude-sonnet-5-5",
                "        [ ] create-pr            claude-haiku-5-5",
                "  [ ] beta",
                "        [ ] fix                  claude-sonnet-5-5",
                "        [ ] create-pr            claude-haiku-5-5",
            ]
        );
        assert!(!rows.iter().any(|r| r.contains("gamma")), "{rows:#?}");
        assert_eq!(
            drawn.last().unwrap(),
            " [space] select   [a] select all   [enter] set value   [esc] back"
        );

        // `esc` goes back to the fields, and `tab` only then leaves.
        type_keys(&repo, &mut state, "\t");
        assert!(
            state.pipeline.title().is_some(),
            "tab does not leave a pick"
        );
        esc(&repo, &mut state);
        type_keys(&repo, &mut state, "\t");
        assert_eq!(state.section, super::super::Section::Prompts);
    }

    // A step that leaves the field out, or leaves it blank, reads `not set`,
    // and the steps the layer already sets the field on start selected.
    #[test]
    fn overridden_steps_start_selected_and_a_missing_field_reads_not_set() {
        let (repo, _guard) = pipelines_project("preselected");
        write_patch(&repo, "beta", "steps:\n  fix:\n    effort: low\n");
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\x1b[B\r");
        let rows = rows_of(&state);
        assert_eq!(
            &rows[..8],
            [
                "",
                "> [ ] alpha",
                "        [ ] implement            high",
                "        [ ] review               high",
                "        [ ] create-pr            not set",
                "  [x] beta",
                "        [x] fix                  low",
                "        [ ] create-pr            not set",
            ]
        );
    }

    // `space` on a pipeline selects every step under it, and again clears
    // them. `a` selects every step, and a second `a` clears the selection.
    #[test]
    fn space_selects_a_pipeline_and_a_selects_everything() {
        let (repo, _guard) = pipelines_project("space");
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\r ");
        let rows = rows_of(&state);
        assert_eq!(
            &rows[1..6],
            [
                "> [x] alpha",
                "        [x] implement            claude-sonnet-5-5",
                "        [x] review               claude-sonnet-5-5",
                "        [x] create-pr            claude-haiku-5-5",
                "  [ ] beta",
            ]
        );
        // A step is unselected on its own, and its pipeline's box stays
        // ticked while any of its steps is selected.
        type_keys(&repo, &mut state, "\x1b[B\x1b[B ");
        assert_eq!(rows_of(&state)[1], "  [x] alpha");
        assert!(rows_of(&state)[3].starts_with(">       [ ] review"));

        type_keys(&repo, &mut state, "\x1b[A\x1b[A ");
        assert_eq!(
            &rows_of(&state)[1..6],
            [
                "> [ ] alpha",
                "        [ ] implement            claude-sonnet-5-5",
                "        [ ] review               claude-sonnet-5-5",
                "        [ ] create-pr            claude-haiku-5-5",
                "  [ ] beta",
            ]
        );

        type_keys(&repo, &mut state, "a");
        let View::Select(selection) = &state.pipeline.view else {
            panic!("still picking");
        };
        assert_eq!(selection.selected.len(), 5);
        assert!(rows_of(&state).iter().any(|r| r == "  [x] beta"));
        type_keys(&repo, &mut state, "a");
        let View::Select(selection) = &state.pipeline.view else {
            panic!("still picking");
        };
        assert!(selection.selected.is_empty());
        assert!(rows_of(&state)[1].starts_with("> [ ] alpha"), "on alpha");
    }

    // `space` on a step selects that step alone, with nothing to open
    // first, and ticks its pipeline's box.
    #[test]
    fn space_on_a_step_selects_that_step_alone() {
        let (repo, _guard) = pipelines_project("one-step");
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\r\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B ");
        let View::Select(selection) = &state.pipeline.view else {
            panic!("still picking");
        };
        assert_eq!(
            selection.selected.iter().collect::<Vec<_>>(),
            [&("beta".to_string(), "fix".to_string())]
        );
        assert_eq!(
            &rows_of(&state)[1..9],
            [
                "  [ ] alpha",
                "        [ ] implement            claude-sonnet-5-5",
                "        [ ] review               claude-sonnet-5-5",
                "        [ ] create-pr            claude-haiku-5-5",
                "  [x] beta",
                ">       [x] fix                  claude-sonnet-5-5",
                "        [ ] create-pr            claude-haiku-5-5",
                "",
            ]
        );
    }

    // A field every step takes lists the steps of both kinds, a command
    // step under the same pipeline as its agent steps, and a field only
    // command steps take lists those alone.
    #[test]
    fn a_field_under_all_steps_lists_every_kind_of_step() {
        let (repo, _guard) = pipelines_project("all-steps");
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = State::new(&repo);
        type_keys(&repo, &mut state, "\t");
        let down = "\x1b[B".repeat(at_field("on_pass"));
        type_keys(&repo, &mut state, &format!("{down}\r"));
        assert_eq!(
            &rows_of(&state)[..11],
            [
                "",
                "> [ ] alpha",
                "        [ ] implement            review",
                "        [ ] review               check",
                "        [ ] check                create-pr",
                "        [ ] create-pr            done",
                "  [ ] beta",
                "        [ ] fix                  create-pr",
                "        [ ] create-pr            done",
                "  [ ] gamma",
                "        [ ] build                done",
            ]
        );

        esc(&repo, &mut state);
        let down = "\x1b[B".repeat(at_field("background") - at_field("on_pass"));
        type_keys(&repo, &mut state, &format!("{down}\r"));
        assert_eq!(
            &rows_of(&state)[..6],
            [
                "",
                "> [ ] alpha",
                "        [ ] check                not set",
                "  [ ] gamma",
                "        [ ] build                not set",
                "",
            ]
        );
    }

    // A routing key set to a step one pipeline does not have is written as
    // typed, since the tab checks no value. The merge then leaves it out,
    // `override list` names it as ignored, and the pipeline still loads on
    // its tracked route.
    #[test]
    fn a_route_to_a_missing_step_is_written_and_reported_as_ignored() {
        let (repo, _guard) = pipelines_project("missing-route");
        let mut state = State::new(&repo);
        type_keys(&repo, &mut state, "\t");
        let down = "\x1b[B".repeat(at_field("on_pass"));
        let erase = "\x7f".repeat(20);
        type_keys(
            &repo,
            &mut state,
            &format!("{down}\r\x1b[B \r{erase}nowhere\r\r"),
        );
        assert!(
            patch(&repo, "alpha").contains("steps:\n  implement:\n    on_pass: nowhere\n"),
            "{}",
            patch(&repo, "alpha")
        );

        let rows = crate::commands::collect_override_rows(&repo).unwrap();
        let alpha = rows
            .iter()
            .find(|row| row.target == "pipelines/alpha.yml")
            .unwrap();
        assert_eq!(alpha.ignored.len(), 1, "{:?}", alpha.ignored);
        assert_eq!(alpha.ignored[0].fields, "implement.on_pass");
        assert!(state.pipeline.stale.is_none(), "{:?}", state.pipeline.stale);
        let implement = &state.pipeline.pipelines[0].steps[0];
        assert_eq!(implement.value(FIELDS[at_field("on_pass")]), "review");
    }

    // A field under `all steps` counts the steps of both kinds the layer
    // sets it on, and `o`'s `delete override` takes it off all of them.
    #[test]
    fn a_field_under_all_steps_is_counted_and_deleted_on_both_kinds() {
        let (repo, _guard) = pipelines_project("all-steps-count");
        write_patch(
            &repo,
            "alpha",
            "steps:\n  implement:\n    description: writes it\n  check:\n    description: \
             tests it\n",
        );
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = State::new(&repo);
        type_keys(&repo, &mut state, "\t");
        assert_eq!(rows_of(&state)[2], ">   description          2 overridden");

        type_keys(&repo, &mut state, "o\r");
        assert!(
            popup_rows(&state)
                .iter()
                .any(|row| row.contains("2 keys in 1 file")),
            "{:#?}",
            popup_rows(&state)
        );
        type_keys(&repo, &mut state, "\r");
        assert!(!patch(&repo, "alpha").contains("description"));
        assert_eq!(rows_of(&state)[2], ">   description");
    }

    // Neither `space` nor `a` changes which rows are listed, so the cursor
    // stays on the row it was on, a step or a pipeline.
    #[test]
    fn space_and_a_leave_the_cursor_on_its_row() {
        let (repo, _guard) = pipelines_project("cursor");
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = on_pipeline(&repo);
        let cursor_row = |state: &State| -> String {
            rows_of(state)
                .into_iter()
                .find(|r| r.starts_with('>'))
                .expect("a cursor row")
        };
        type_keys(&repo, &mut state, "\r\x1b[B\x1b[B");
        for key in ["a", " ", "a", "a"] {
            type_keys(&repo, &mut state, key);
            assert!(cursor_row(&state).starts_with(">       ["), "{key:?}");
            assert!(cursor_row(&state).contains(" review "), "{key:?}");
        }
        type_keys(&repo, &mut state, "\x1b[B\x1b[B");
        for key in [" ", " ", "a", " "] {
            type_keys(&repo, &mut state, key);
            assert!(cursor_row(&state).ends_with("] beta"), "{key:?}");
        }
    }

    // A list taller than the screen keeps the cursor's own row in view
    // when it sits far below the first row of its pipeline.
    #[test]
    fn a_step_far_below_its_pipeline_is_drawn_when_the_cursor_is_on_it() {
        let (repo, _guard) = pipelines_project("deep-step");
        let _terminal = crate::screen::test_terminal((80, 8));
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\r");
        type_keys(&repo, &mut state, &"\x1b[B".repeat(6));
        let rows = rows_of(&state);
        assert!(
            rows.iter()
                .any(|r| r == ">       [ ] create-pr            claude-haiku-5-5"),
            "{rows:#?}"
        );
        assert!(!rows.iter().any(|r| r.contains("[ ] alpha")), "{rows:#?}");
    }

    // A step id as long as its column, or longer, is drawn whole with a
    // space before the value after it, and a long pipeline name whole with
    // nothing after it.
    #[test]
    fn a_long_name_or_step_id_is_drawn_whole() {
        let (repo, _guard) = pipelines_project("long-id");
        let name = "a-pipeline-whose-name-runs-on-past-fifty-columns-xy";
        std::fs::write(
            Pipelines::file_in(&repo.checkout, name),
            "steps:\n  - id: a-step-id-past-its-column\n    agent: claude\n    \
             model: claude-opus-5-5\n    on_pass: done\n",
        )
        .unwrap();
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\r ");
        assert_eq!(
            &rows_of(&state)[1..3],
            [
                "> [x] a-pipeline-whose-name-runs-on-past-fifty-columns-xy",
                "        [x] a-step-id-past-its-column claude-opus-5-5",
            ]
        );
    }

    // `a` with a search that leaves no step changes nothing, so the steps
    // the layer already overrides stay selected out of sight.
    #[test]
    fn a_with_a_search_that_matches_nothing_keeps_the_selection() {
        let (repo, _guard) = pipelines_project("all-none");
        write_patch(
            &repo,
            "beta",
            "steps:\n  fix:\n    model: claude-opus-5-5\n",
        );
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\rfmatches-no-step\ra");
        let View::Select(selection) = &state.pipeline.view else {
            panic!("still picking");
        };
        assert!(
            selection.is_selected("beta", "fix"),
            "{:?}",
            selection.selected
        );
    }

    // Search matches pipeline names, step ids, current values and `not
    // set`, and `a` then selects only what the search leaves.
    #[test]
    fn search_matches_names_ids_values_and_not_set() {
        let (repo, _guard) = pipelines_project("search");
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = on_pipeline(&repo);
        let picked = |state: &State| -> Vec<String> {
            let View::Select(selection) = &state.pipeline.view else {
                panic!("still picking");
            };
            selection
                .selected
                .iter()
                .map(|(p, s)| format!("{p}.{s}"))
                .collect()
        };
        // On effort: `not set` finds the two steps that leave it out.
        type_keys(&repo, &mut state, "\x1b[B\rfnot set\ra");
        assert_eq!(picked(&state), ["alpha.create-pr", "beta.create-pr"]);
        let rows = rows_of(&state);
        assert_eq!(rows[0], "search: not set");
        assert_eq!(
            &rows[2..6],
            [
                "> [x] alpha",
                "        [x] create-pr            not set",
                "  [x] beta",
                "        [x] create-pr            not set",
            ]
        );
        // A value, a step id and a pipeline name.
        for (query, expected) in [
            ("medium", vec!["beta.fix"]),
            ("review", vec!["alpha.review"]),
            ("beta", vec!["beta.create-pr", "beta.fix"]),
        ] {
            type_keys(&repo, &mut state, "a");
            esc(&repo, &mut state);
            type_keys(&repo, &mut state, "\rf");
            type_keys(&repo, &mut state, query);
            type_keys(&repo, &mut state, "\ra");
            assert_eq!(picked(&state), expected, "{query}");
        }
    }

    // `enter` opens the value field filled with the value every selected
    // step shares, and its `current` row says so, as the mockup draws it.
    #[test]
    fn the_value_field_opens_on_the_shared_value() {
        let (repo, _guard) = pipelines_project("value");
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = on_pipeline(&repo);
        // implement and review, on alpha, share sonnet.
        type_keys(&repo, &mut state, "\r \x1b[B\x1b[B\x1b[B \r");
        assert_eq!(
            popup_rows(&state),
            [
                "",
                "value: claude-sonnet-5-5▏",
                "",
                "current   claude-sonnet-5-5",
                "alpha     implement, review",
                "",
                "[enter] save   [esc] back",
            ]
        );
        assert!(
            lines(&state)
                .iter()
                .any(|l| l.contains("╭─ set model on 2 steps ─")),
            "{:#?}",
            lines(&state)
        );
        assert_eq!(lines(&state).last().unwrap(), " [enter] save   [esc] back");
    }

    // Selected steps that differ open an empty field over `varies`, and
    // steps that all leave the field out an empty one over `not set`.
    #[test]
    fn the_value_field_is_empty_when_the_steps_differ_or_leave_it_out() {
        let (repo, _guard) = pipelines_project("varies");
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\r \r");
        let rows = popup_rows(&state);
        assert_eq!(rows[1], "value: ▏");
        assert_eq!(rows[3], "current   varies");

        esc(&repo, &mut state);
        esc(&repo, &mut state);
        type_keys(&repo, &mut state, "\x1b[B\rfnot set\ra\r");
        let rows = popup_rows(&state);
        assert_eq!(rows[1], "value: ▏");
        assert_eq!(rows[3], "current   not set");
        assert_eq!(rows[4], "alpha     create-pr");
        assert_eq!(rows[5], "beta      create-pr");
    }

    // Saving writes the value, read as YAML, into every pipeline with a
    // selected step, steps that leave the field out included, and shows
    // the popup the mockup draws.
    #[test]
    fn saving_writes_one_patch_per_pipeline_and_says_so() {
        let (repo, _guard) = pipelines_project("save");
        let _terminal = crate::screen::test_terminal((80, 40));
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\x1b[B\rfnot set\ra\rmax\r");
        assert_eq!(
            popup_rows(&state),
            [
                "",
                "effort on 2 steps in 2 pipelines",
                "  not set → max",
                "",
                "[enter] close",
            ]
        );
        assert!(
            lines(&state)
                .iter()
                .any(|l| l.contains("╭─ overrides written ──────────────────────────────╮"))
        );
        assert!(
            patch(&repo, "alpha").contains("steps:\n  create-pr:\n    effort: max\n"),
            "{}",
            patch(&repo, "alpha")
        );
        assert!(patch(&repo, "beta").contains("steps:\n  create-pr:\n    effort: max\n"));
        assert_eq!(state.active, 2);

        // `enter` closes it onto the field list, which counts the new
        // overrides and shows the new value.
        type_keys(&repo, &mut state, "\r");
        assert!(state.pipeline.at_rest());
        assert!(
            rows_of(&state).contains(&">   effort               2 overridden".to_string()),
            "{:?}",
            rows_of(&state)
        );
        // beta's create-pr is the seventh row under the blank one.
        type_keys(&repo, &mut state, "\r");
        type_keys(&repo, &mut state, &"\x1b[B".repeat(6));
        assert!(
            rows_of(&state)
                .iter()
                .any(|r| r == ">       [x] create-pr            max")
        );
    }

    // A value is written as the YAML it reads as: a boolean stays one, and
    // a duration is a string.
    #[test]
    fn a_value_is_written_as_the_yaml_it_reads_as() {
        let (repo, _guard) = pipelines_project("yaml");
        let mut state = on_pipeline(&repo);
        // hide, then timeout, on alpha's check.
        let down = "\x1b[B".repeat(at_field("hide") - at_field("model"));
        type_keys(&repo, &mut state, &format!("{down}\r \rtrue\r\r"));
        type_keys(&repo, &mut state, "\x1b[A\r \r\x7f\x7f\x7f\x7f5m\r\r");
        let text = patch(&repo, "alpha");
        assert!(text.contains("    hide: true\n"), "{text}");
        assert!(text.contains("    timeout: 5m\n"), "{text}");
        let parsed = crate::overrides::read_pipeline_patch(&repo.overrides_dir(), "alpha")
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.steps["check"]["hide"],
            serde_norway::Value::Bool(true)
        );
        assert_eq!(parsed.steps["check"]["timeout"].as_str(), Some("5m"));
    }

    // A step whose patch already sets another field keeps it beside the
    // new one.
    #[test]
    fn a_write_keeps_a_field_the_patch_already_sets() {
        let (repo, _guard) = pipelines_project("keeps");
        write_patch(&repo, "beta", "steps:\n  fix:\n    effort: low\n");
        let mut state = on_pipeline(&repo);
        // beta's row is the fifth, under alpha's three steps.
        let down = "\x1b[B".repeat(4);
        type_keys(&repo, &mut state, &format!("\r{down} \rclaude-opus-5-5\r"));
        let parsed = crate::overrides::read_pipeline_patch(&repo.overrides_dir(), "beta")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.steps["fix"]["effort"].as_str(), Some("low"));
        assert_eq!(
            parsed.steps["fix"]["model"].as_str(),
            Some("claude-opus-5-5")
        );
        assert_eq!(
            parsed.steps["create-pr"]["model"].as_str(),
            Some("claude-opus-5-5")
        );
    }

    // A patch that does not read stops the whole write before any file is
    // touched: alpha, written first by name, is left with no patch, and the
    // broken one is left as it was. The notice says so until `enter` goes
    // back to the picking list.
    #[test]
    fn a_patch_that_does_not_read_leaves_every_file_as_it_was() {
        let (repo, _guard) = pipelines_project("atomic");
        write_patch(&repo, "beta", "steps: [\n");
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\ra\rclaude-opus-5-5\r");
        let drawn = lines(&state);
        assert!(
            drawn.iter().any(|l| l.contains("╭─ not written ─")),
            "{drawn:#?}"
        );
        let text = drawn.join(" ");
        assert!(text.contains("model was not set"), "{text}");
        assert!(text.contains("nothing was written"), "{text}");
        assert_eq!(patch(&repo, "alpha"), "");
        assert_eq!(patch(&repo, "beta"), "steps: [\n");

        type_keys(&repo, &mut state, "\r");
        assert!(matches!(state.pipeline.view, View::Select(_)));
    }

    // A value that does not read as YAML is refused the same way.
    #[test]
    fn a_value_that_is_not_yaml_writes_nothing() {
        let (repo, _guard) = pipelines_project("not-yaml");
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\r \r[oops\r");
        assert!(lines(&state).join(" ").contains("does not read as YAML"));
        assert_eq!(patch(&repo, "alpha"), "");
    }

    // A write that fails after earlier pipelines were written stops there,
    // and its notice names what was written and what was not, rather than
    // saying nothing was. A real file cannot be made to fail its write once
    // it has read: every patch sits in the same directory, and a path that
    // refuses the write refuses the read before it, so `put` fails instead.
    #[test]
    fn a_write_that_stops_partway_names_what_it_wrote() {
        let patches: Vec<(String, PipelinePatch)> = ["alpha", "beta", "gamma"]
            .into_iter()
            .map(|name| (name.to_string(), PipelinePatch::default()))
            .collect();
        let mut tried = Vec::new();
        let failure = write_patches(&patches, |name, _| {
            tried.push(name.to_string());
            match name {
                "beta" => anyhow::bail!("writing beta.yml: disk full"),
                _ => Ok(()),
            }
        })
        .unwrap_err();
        assert_eq!(tried, ["alpha", "beta"]);
        assert_eq!(failure.written, ["alpha"]);
        assert_eq!(failure.unwritten, ["beta", "gamma"]);
        let notice = unwritten_notice("model", &failure);
        assert_eq!(notice.title, "written in part");
        assert!(
            notice
                .text
                .starts_with("model was set on alpha but not on beta, gamma: writing beta.yml"),
            "{}",
            notice.text
        );

        let before = unwritten_notice("model", &anyhow::anyhow!("no lock").into());
        assert_eq!(before.title, "not written");
        assert!(before.text.starts_with("model was not set: no lock"));
    }

    // `enter` with nothing selected waits for a step, and `enter` on an
    // emptied field or `esc` goes back to the picking list with nothing
    // written.
    #[test]
    fn nothing_selected_or_an_empty_value_writes_nothing() {
        let (repo, _guard) = pipelines_project("empty");
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\r\r");
        assert!(matches!(state.pipeline.view, View::Select(_)));
        type_keys(&repo, &mut state, " \r");
        let erase = "\x7f".repeat(20);
        type_keys(&repo, &mut state, &format!("{erase}\r"));
        assert!(matches!(state.pipeline.view, View::Select(_)));
        type_keys(&repo, &mut state, "\rx");
        esc(&repo, &mut state);
        assert!(matches!(state.pipeline.view, View::Select(_)));
        assert!(!crate::overrides::pipeline_patches_dir(&repo.overrides_dir()).exists());
    }

    // A value popup naming more pipelines than a short terminal has rows
    // for says how many it left out, and keeps the frame inside it.
    #[test]
    fn a_value_popup_on_a_short_terminal_leaves_pipelines_out() {
        let (repo, _guard) = pipelines_project("short");
        let _terminal = crate::screen::test_terminal((80, 12));
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\ra\r");
        let drawn = lines(&state);
        assert_eq!(drawn.len(), 12 - 1, "{drawn:#?}");
        let rows = popup_rows(&state);
        assert!(
            rows.iter().any(|r| r == "          and 2 more pipelines"),
            "{rows:#?}"
        );
        assert!(
            rows.iter().any(|r| r == "[enter] save   [esc] back"),
            "{rows:#?}"
        );
    }

    // A patch that stops reading while the screen is open is named above
    // the list with the command that drops it, and leaves only its own
    // pipeline on the tracked values.
    #[test]
    fn a_patch_that_no_longer_merges_is_said_above_the_list() {
        let (repo, _guard) = pipelines_project("stale-patch");
        write_patch(&repo, "alpha", "steps: [\n");
        write_patch(
            &repo,
            "beta",
            "steps:\n  fix:\n    model: claude-opus-5-5\n",
        );
        let state = on_pipeline(&repo);
        let stale = state.pipeline.stale.as_deref().unwrap_or_default();
        assert!(
            stale.starts_with("the patch on alpha no longer merges"),
            "{stale}"
        );
        assert!(stale.contains("`spoolway override drop alpha`"), "{stale}");
        let beta = &state.pipeline.pipelines[1];
        assert_eq!(
            beta.steps[0].value(FIELDS[at_field("model")]),
            "claude-opus-5-5"
        );
        assert!(lines(&state).join(" ").contains("no longer merges"));
    }

    // Every patch that no longer merges is named above the list, each with
    // why and the command that drops it, not only the first.
    #[test]
    fn every_patch_that_no_longer_merges_is_said_above_the_list() {
        let (repo, _guard) = pipelines_project("stale-patches");
        write_patch(&repo, "alpha", "steps: [\n");
        write_patch(&repo, "beta", "steps: [\n");
        let state = on_pipeline(&repo);
        let stale = state.pipeline.stale.as_deref().unwrap_or_default();
        assert!(
            stale.starts_with("the patches on alpha, beta no longer merge: alpha: "),
            "{stale}"
        );
        assert!(stale.contains("; beta: "), "{stale}");
        assert!(
            stale.contains(
                "drop them with `spoolway override drop alpha` and `spoolway override drop beta`."
            ),
            "{stale}"
        );
    }

    // Tracked pipelines that no longer load leave the list with no
    // pipelines, and say why.
    #[test]
    fn tracked_pipelines_that_no_longer_load_are_said_above_the_list() {
        let (repo, _guard) = pipelines_project("stale-tracked");
        std::fs::write(Pipelines::file_in(&repo.checkout, "alpha"), "steps: [\n").unwrap();
        let state = on_pipeline(&repo);
        let stale = state.pipeline.stale.as_deref().unwrap_or_default();
        assert!(
            stale.starts_with("the tracked pipelines no longer load"),
            "{stale}"
        );
        assert!(stale.contains("`spoolway pipeline check`"), "{stale}");
        assert!(state.pipeline.pipelines.is_empty());
    }

    // A picking list taller than the screen keeps the cursor's row in view
    // and counts the pipelines out of sight under the rows' indent.
    #[test]
    fn a_long_picking_list_scrolls_by_pipeline() {
        let (repo, _guard) = pipelines_project("scroll");
        let _terminal = crate::screen::test_terminal((80, 10));
        let mut state = on_pipeline(&repo);
        type_keys(&repo, &mut state, "\ra");
        let rows = rows_of(&state);
        assert_eq!(rows.last().unwrap(), "  ↓ 1 pipeline below", "{rows:#?}");
    }
}
