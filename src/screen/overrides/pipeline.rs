//! The overrides tab's pipeline section: one step field set on many pipeline
//! steps at once.
//!
//! The layer keeps one patch per pipeline, keyed by step id, but a person
//! thinks "every step on this model uses that one now", whatever the steps
//! are called. So the section lists step fields, not pipelines. `enter` on a
//! field opens every tracked pipeline with a step that takes it, each closed
//! to one row. The person selects steps across as many pipelines as they
//! like, types one value, and it lands as a step patch in every pipeline
//! with a selected step.
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
use crate::pipeline::{Pipelines, StepKind};
use crate::repo::Repo;
use crate::screen::{Key, Notice, keys, pad_to, plural};

/// A step field the section lists, and the kind of step that takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Field {
    name: &'static str,
    kind: StepKind,
}

/// The fields the section lists, agent fields first, in the order they are
/// drawn. Only the fields that say how a step runs: the ones that route a
/// task (`on_pass`, `on_fail`, `loop`, `gate`) differ per step by design,
/// so one value across many pipelines is not something to offer for them.
/// Agent fields go only on agent steps and command fields only on command
/// steps, because the merge refuses anything else.
const FIELDS: [Field; 9] = [
    Field::agent("model"),
    Field::agent("effort"),
    Field::agent("agent"),
    Field::agent("prompt"),
    Field::agent("session"),
    Field::agent("skills"),
    Field::command("run"),
    Field::command("timeout"),
    Field::command("hide"),
];

impl Field {
    const fn agent(name: &'static str) -> Field {
        Field {
            name,
            kind: StepKind::Agent,
        }
    }

    const fn command(name: &'static str) -> Field {
        Field {
            name,
            kind: StepKind::Command,
        }
    }
}

/// The header each kind's fields are listed under.
fn kind_header(kind: StepKind) -> &'static str {
    match kind {
        StepKind::Agent => "agent steps",
        StepKind::Command => "command steps",
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
            .filter(move |(_, step)| step.kind == field.kind)
            .map(|(at, _)| at)
    }
}

/// Every tracked pipeline, with each step's values read off the merged
/// pipelines, and why the list is empty or shows the tracked values when
/// it does.
///
/// Only the pipelines in the repo's `.spoolway/`: a private pipeline's patch
/// waits until the pipeline is promoted, so a value set on one would read as
/// written and do nothing. The `blocked` step is left out too: every
/// pipeline gets it from `[unattended]`, which the config section sets once
/// for all of them.
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
    let (merged, stale) = match Pipelines::load_quietly(&repo.checkout, config) {
        Ok(merged) => (Some(merged), None),
        Err(err) => (
            None,
            Some(format!(
                "the pipeline overrides no longer merge: {}. the values shown are the tracked \
                 ones; fix or delete the patch it names.",
                one_line(&err)
            )),
        ),
    };
    let overrides = repo.overrides_dir();
    let pipelines = tracked
        .pipelines
        .values()
        .map(|pipeline| {
            let patch = crate::overrides::read_pipeline_patch(&overrides, &pipeline.name)
                .ok()
                .flatten()
                .unwrap_or_default();
            let steps = pipeline
                .steps
                .iter()
                .filter(|step| step.id != crate::pipeline::BLOCKED)
                .map(|step| {
                    let in_use = merged
                        .as_ref()
                        .and_then(|m| m.pipelines.get(&pipeline.name))
                        .and_then(|p| p.steps.iter().find(|s| s.id == step.id))
                        .unwrap_or(step);
                    let kind = step.kind();
                    let values = FIELDS
                        .iter()
                        .filter(|field| field.kind == kind)
                        .map(|field| {
                            let value = crate::overrides::field_display(in_use, field.name)
                                .unwrap_or_default();
                            (field.name, value)
                        })
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
    (pipelines, stale)
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
    /// The pipelines drawn open, their steps listed under them.
    open: BTreeSet<String>,
    /// Every selected step, as (pipeline, step id).
    selected: BTreeSet<(String, String)>,
}

impl Selection {
    /// A selection on `field` with the steps whose patch already sets it
    /// selected, and their pipelines open.
    fn new(field: Field, pipelines: &[PipelineRows]) -> Selection {
        let mut selection = Selection {
            field,
            filter: String::new(),
            searching: false,
            cursor: 0,
            open: BTreeSet::new(),
            selected: BTreeSet::new(),
        };
        for pipeline in pipelines {
            for at in pipeline.taking(field) {
                let step = &pipeline.steps[at];
                if step.overridden.contains(field.name) {
                    selection.open.insert(pipeline.name.clone());
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
/// followed by those steps when it is open.
fn picking_rows(pipelines: &[PipelineRows], selection: &Selection) -> Vec<Row> {
    let mut rows = Vec::new();
    for (p, pipeline) in pipelines.iter().enumerate() {
        let steps = visible_steps(pipeline, selection);
        if steps.is_empty() {
            continue;
        }
        rows.push(Row::Pipeline(p));
        if selection.open.contains(&pipeline.name) {
            rows.extend(steps.into_iter().map(|s| Row::Step(p, s)));
        }
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
                Key::Up | Key::Down => self.move_selection(&mut selection, key),
                other => edit(&mut selection.filter, other),
            }
            self.refocus(&mut selection, before);
            return View::Select(selection);
        }
        match key {
            Key::Esc => return View::Fields,
            Key::Up | Key::Down => self.move_selection(&mut selection, key),
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
    /// or on that row's pipeline when the row itself is gone, as when `a`
    /// closes the pipeline a step was in.
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

    /// `space` on a pipeline: one with a selected step closes and clears
    /// them all, and any other opens with every step the search leaves
    /// selected.
    fn toggle_pipeline(&self, selection: &mut Selection, p: usize) {
        let pipeline = &self.pipelines[p];
        if selection.selected_in(&pipeline.name) > 0 {
            selection
                .selected
                .retain(|(name, _)| *name != pipeline.name);
            selection.open.remove(&pipeline.name);
            return;
        }
        selection.open.insert(pipeline.name.clone());
        for s in visible_steps(pipeline, selection) {
            selection
                .selected
                .insert((pipeline.name.clone(), pipeline.steps[s].id.clone()));
        }
    }

    /// `a`: every pipeline and step the search leaves is selected, opening
    /// them all, unless they already are, and then the selection is cleared.
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
        if visible.iter().all(|id| selection.selected.contains(id)) {
            selection.selected.clear();
            selection.open.clear();
            return;
        }
        for (pipeline, step) in visible {
            selection.open.insert(pipeline.clone());
            selection.selected.insert((pipeline, step));
        }
    }

    /// Write `value` onto every selected step, then show what was written,
    /// or why nothing was. Answers whether the layer changed.
    fn save(&mut self, repo: &Repo, selection: Selection, value: &str) -> bool {
        let targets = selection.targets(&self.pipelines);
        let old = current_text(&self.pipelines, &selection);
        match write(repo, selection.field, value, &targets) {
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
            Err(err) => {
                let field = selection.field.name;
                self.view = View::Failed {
                    notice: Notice::new(
                        "not written",
                        format!(
                            "{field} was not set: {err:#}. fix what stopped it and set the \
                             value again."
                        ),
                    ),
                    selection,
                };
                // A write that stopped partway still changed the layer.
                true
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
    /// many steps in how many pipelines take it, and how many of those the
    /// layer already sets it on.
    fn field_lines(&self, rows: Option<usize>, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        if matches!(self.view, View::Search) {
            lines.push(format!("search: {}▏", self.filter));
        }
        lines.push(String::new());
        let fields = self.fields();
        let counts: Vec<(usize, usize, usize)> =
            fields.iter().map(|field| self.count(*field)).collect();
        let digits = |n: usize| n.to_string().len();
        let steps_w = counts.iter().map(|c| digits(c.0)).max().unwrap_or(1);
        let pipes_w = counts.iter().map(|c| digits(c.1)).max().unwrap_or(1);
        let mut starts = Vec::new();
        let mut kind = None;
        for (at, (field, (steps, pipelines, overridden))) in fields.iter().zip(counts).enumerate() {
            if kind != Some(field.kind) {
                kind = Some(field.kind);
                lines.push(format!("  {}", kind_header(field.kind)));
            }
            let mark = if at == self.cursor { "> " } else { "  " };
            let count = format!(
                "{steps:>steps_w$} {} in {pipelines:>pipes_w$} {}",
                noun(steps, "step"),
                noun(pipelines, "pipeline")
            );
            let overridden = match overridden {
                0 => String::new(),
                n => format!("{n} overridden"),
            };
            starts.push(lines.len());
            lines.push(
                format!(
                    "{mark}  {:FIELD_NAME$}{}{overridden}",
                    field.name,
                    pad_to(&count, FIELD_COUNT)
                )
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

    /// How many steps take `field`, in how many pipelines, and on how many
    /// of them the layer already sets it.
    fn count(&self, field: Field) -> (usize, usize, usize) {
        let mut steps = 0;
        let mut pipelines = 0;
        let mut overridden = 0;
        for pipeline in &self.pipelines {
            let taking: Vec<usize> = pipeline.taking(field).collect();
            if taking.is_empty() {
                continue;
            }
            pipelines += 1;
            steps += taking.len();
            overridden += taking
                .iter()
                .filter(|&&at| pipeline.steps[at].overridden.contains(field.name))
                .count();
        }
        (steps, pipelines, overridden)
    }

    /// The picking list: each pipeline on one row with its step count, and
    /// an open one's steps under it with their values.
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
                    let total = pipeline.taking(selection.field).count();
                    let picked = selection.selected_in(&pipeline.name);
                    let count = match selection.open.contains(&pipeline.name) {
                        true => format!("{picked} of {total} selected"),
                        false => plural(total, "step"),
                    };
                    starts.push(lines.len());
                    lines.push(format!(
                        "{mark}{} {}{count}",
                        tick(picked > 0),
                        pad_to(&pipeline.name, PIPELINE_NAME)
                    ));
                }
                Row::Step(p, s) => {
                    let (pipeline, step) = (&self.pipelines[p], &self.pipelines[p].steps[s]);
                    lines.push(format!(
                        "{mark}      {} {}{}",
                        tick(selection.is_selected(&pipeline.name, &step.id)),
                        pad_to(&step.id, STEP_ID),
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
    ("a", "all"),
    ("enter", "set value"),
    ("esc", "back"),
];

/// The column a field's step count starts at, counted from the field's name.
const FIELD_NAME: usize = 21;

/// How wide a field's step count is padded, so `overridden` lines up after
/// it on every row.
const FIELD_COUNT: usize = 30;

/// The column a pipeline's step count starts at, counted from its name.
const PIPELINE_NAME: usize = 49;

/// The column a step's value starts at, counted from its id: room for the
/// 26 characters an id may run to would push the values past the middle
/// of the box, so a longer id pushes only its own value along.
const STEP_ID: usize = 21;

/// How wide the value popup's label column is at least: `current` and the
/// gap after it.
const CURRENT_LABEL: usize = 10;

/// `noun`, with an `s` unless `n` is one.
fn noun(n: usize, noun: &str) -> String {
    match n {
        1 => noun.to_string(),
        _ => format!("{noun}s"),
    }
}

/// A row's checkbox.
fn tick(on: bool) -> &'static str {
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

/// Each pipeline's block of rows, dropped from the end until they fit in
/// `room` rows, with one row saying how many pipelines were left out. All of
/// them when `room` is `None`, with no terminal to measure.
fn fit_blocks(mut blocks: Vec<Vec<String>>, room: Option<usize>, label: usize) -> Vec<String> {
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
            "{:label$}and {} more",
            "",
            plural(left_out, "pipeline")
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

/// Write `value`, read as YAML, as `field` on every step `targets` names,
/// one patch per pipeline.
///
/// Every patch is built before any is written, so a patch that does not
/// read, or a value that is not YAML, leaves every file as it was rather
/// than some pipelines patched and some not. Each one starts from the patch
/// already on disk, so a key a step's patch already holds is kept beside
/// the new one.
fn write(repo: &Repo, field: Field, value: &str, targets: &[(String, Vec<String>)]) -> Result<()> {
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
                serde_norway::Value::String(field.name.to_string()),
                parsed.clone(),
            );
        }
        patches.push((name, patch));
    }
    let mut written: Vec<&str> = Vec::new();
    for (name, patch) in &patches {
        crate::overrides::write_pipeline_patch(&overrides, name, patch).with_context(|| {
            match written.is_empty() {
                true => "nothing was written".to_string(),
                false => format!("{} had already been written", written.join(", ")),
            }
        })?;
        written.push(name);
    }
    Ok(())
}
