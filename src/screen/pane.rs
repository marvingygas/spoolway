//! The scrolling window the queue and jobs screens cut a tall pane down to.
//!
//! The queue screen, its routines view and the jobs screen each draw a pane
//! that can be taller than the terminal. This module decides which lines of
//! the pane stay in view and spends the pane's bottom row on a marker that
//! says how many items are out of sight. It lives here, outside any one
//! screen, so the screens' markers read the same and cannot drift apart.

use crate::screen::plural;

/// Where a pane's items start among its lines, and what to call them, so
/// [`window`]'s marker row can count what is out of sight in the pane's own
/// terms rather than in lines.
///
/// A task in the tasks pane is six lines, so a count of lines said "21
/// below" when four tasks were out of sight. Only the builder that draws a
/// pane knows where one item ends and the next begins, so it notes each
/// start in the same loop that pushes the item's first line. A line no
/// start points into ahead of the first item — the filter's `find:` row —
/// and a blank separator row are never counted.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Items<'a> {
    /// The line each item starts on, in ascending order.
    pub(crate) starts: &'a [usize],
    /// The singular noun the marker counts in (`"task"`, `"group"`,
    /// `"folder"`), or `None` for the jobs screen, which passes one item per
    /// line and keeps the one-sided `↓ N below` it has always drawn.
    pub(crate) noun: Option<&'static str>,
}

/// The slice of `lines` a pane `rows` rows tall shows, with the block at
/// `focus` — a group's row, or the highlighted task and everything drawn
/// under it — kept in view.
///
/// `None` rows is a run with no terminal to measure, where nothing is cut at
/// all. Where the content does not fit, the pane's bottom row is spent on
/// saying how many of `items` are out of sight rather than on a line that
/// would be silently the last one a person sees — see [`marker_row`], which
/// fits that row to `width`.
pub(crate) fn window(
    lines: &[String],
    items: Items<'_>,
    focus: (usize, usize),
    rows: Option<usize>,
    width: usize,
) -> Vec<String> {
    let Some(rows) = rows else {
        return lines.to_vec();
    };
    if lines.len() <= rows {
        return lines.to_vec();
    }
    let view = rows - 1;
    let (start, end) = focus;
    let offset = (end + 1)
        .saturating_sub(view)
        .min(start)
        .min(lines.len() - view);
    let mut shown = lines[offset..offset + view].to_vec();
    let (above, below) = hidden_items(lines, items.starts, offset, offset + view);
    shown.push(marker_row(above, below, items.noun, width));
    shown
}

/// How many items are not fully in view above and below the lines
/// `top..bottom`. An item cut in half at either edge counts as out of sight
/// on that side: part of it is, and the marker is what says so.
///
/// An item runs from its start to the last non-blank line before the next
/// one starts. The blank row the tasks pane draws ahead of every task is a
/// separator, not the end of the task before it — counting it would call a
/// task whose own last line is on screen "below" whenever only that blank
/// row had scrolled off.
pub(crate) fn hidden_items(
    lines: &[String],
    starts: &[usize],
    top: usize,
    bottom: usize,
) -> (usize, usize) {
    let mut above = 0;
    let mut below = 0;
    for (k, &start) in starts.iter().enumerate() {
        let next = starts.get(k + 1).copied().unwrap_or(lines.len());
        let last = (start..next)
            .rev()
            .find(|&line| !lines[line].is_empty())
            .unwrap_or(start);
        if start < top {
            above += 1;
        } else if last >= bottom {
            below += 1;
        }
    }
    (above, below)
}

/// The marker row under a cut pane: both directions on one row when items
/// are hidden on both sides, only the one side otherwise.
///
/// Named with its noun first — `↑ 11 groups above · ↓ 19 groups below` —
/// and shortened when the pane is too narrow, first to `↑ 11 above · ↓ 19
/// below` and then to `↑ 11 · ↓ 19`. The first form that fits `width` is
/// drawn: the queue screen's `two_pane_frame` would otherwise cut a wider row
/// off mid-word, through its own `pad_to`, without a sign it had. The last
/// form is drawn whatever the width, as nothing shorter still says both
/// counts.
///
/// With no noun this is the jobs screen's marker exactly as it has always
/// read: one direction only, below whenever anything is.
///
/// Nothing out of sight on either side — every hidden line was a separator
/// — draws an empty row rather than a `↓ 0 below` that says nothing.
pub(crate) fn marker_row(above: usize, below: usize, noun: Option<&str>, width: usize) -> String {
    let Some(noun) = noun else {
        return match below {
            0 => format!("↑ {above} above"),
            _ => format!("↓ {below} below"),
        };
    };
    // 0 is the full row, 1 drops the noun, 2 drops the words as well.
    let form = |short: u8| {
        let side = |arrow: &str, n: usize, word: &str| match short {
            0 => format!("{arrow} {} {word}", plural(n, noun)),
            1 => format!("{arrow} {n} {word}"),
            _ => format!("{arrow} {n}"),
        };
        let mut sides = Vec::new();
        if above > 0 {
            sides.push(side("↑", above, "above"));
        }
        if below > 0 {
            sides.push(side("↓", below, "below"));
        }
        sides.join(" · ")
    };
    let forms = [form(0), form(1), form(2)];
    forms
        .iter()
        .find(|row| row.chars().count() <= width)
        .unwrap_or(&forms[2])
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::{BODY, fixture, listed, task_text, write_pending};
    use crate::commands::{MIN_LEFT_PANE, ScreenState, tasks_pane_lines};
    use crate::pipeline::Pipelines;

    /// `Items` for a pane of `len` lines where every line is an item and the
    /// marker names no noun — what the jobs screen passes to [`window`].
    fn per_line(len: usize) -> Vec<usize> {
        (0..len).collect()
    }

    /// A pane taller than the terminal scrolls to the highlighted block and
    /// says how much is out of sight, rather than letting the frame run off
    /// the bottom of the screen.
    ///
    /// Passed one item per line and no noun, the way the jobs screen calls
    /// it, so this also pins that screen's marker text exactly as it was
    /// before panes counted items: one direction only, below whenever
    /// anything is, even with lines hidden above as well.
    #[test]
    fn a_pane_taller_than_its_rows_scrolls_to_the_focused_block() {
        let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        let starts = per_line(lines.len());
        let items = Items {
            starts: &starts,
            noun: None,
        };

        let top = window(&lines, items, (0, 0), Some(5), 40);
        assert_eq!(top.len(), 5);
        assert_eq!(top[0], "line 0");
        assert_eq!(top[4], "↓ 16 below");

        let middle = window(&lines, items, (11, 12), Some(5), 40);
        assert_eq!(middle.len(), 5);
        assert!(
            middle.contains(&"line 11".to_string()) && middle.contains(&"line 12".to_string()),
            "the focused block must stay in view, got {middle:?}"
        );
        assert_eq!(middle[4], "↓ 7 below", "the jobs screen never shows above");

        let bottom = window(&lines, items, (19, 19), Some(5), 40);
        assert_eq!(bottom[3], "line 19");
        assert_eq!(bottom[4], "↑ 16 above");

        assert_eq!(
            window(&lines, items, (0, 0), None, 40).len(),
            20,
            "no terminal, no cut"
        );
        let short = per_line(3);
        let items = Items {
            starts: &short,
            noun: None,
        };
        assert_eq!(window(&lines[..3], items, (0, 0), Some(9), 40).len(), 3);
    }

    /// The marker names what it counts, singular for one, and shows both
    /// directions on one row only when both have something hidden.
    #[test]
    fn the_marker_row_names_its_items_and_shows_both_directions() {
        assert_eq!(
            marker_row(11, 19, Some("group"), 80),
            "↑ 11 groups above · ↓ 19 groups below"
        );
        assert_eq!(marker_row(0, 4, Some("task"), 80), "↓ 4 tasks below");
        assert_eq!(marker_row(0, 1, Some("task"), 80), "↓ 1 task below");
        assert_eq!(
            marker_row(3, 1, Some("task"), 80),
            "↑ 3 tasks above · ↓ 1 task below"
        );
        assert_eq!(marker_row(2, 0, Some("folder"), 80), "↑ 2 folders above");
        assert_eq!(marker_row(0, 0, Some("group"), 80), "");
    }

    /// The tasks pane's marker counts tasks, not lines, at every scroll
    /// position: a task cut at the bottom edge counts as below, one cut at
    /// the top edge as above, and the blank row ahead of each task is never
    /// counted. The truth each marker is checked against is read off where
    /// each task's header and description rows sit, not off the starts the
    /// pane handed `window`.
    #[test]
    fn the_tasks_pane_marker_counts_tasks_at_every_scroll_position() {
        let (repo, _root_guard) = fixture("scroll-counts-tasks");
        let ids: Vec<String> = (1..=6).map(|i| format!("task-{i}")).collect();
        for (i, id) in ids.iter().enumerate() {
            let depends = match i {
                0 => String::new(),
                _ => format!("depends_on: [{}]\n", ids[i - 1]),
            };
            write_pending(
                &repo,
                id,
                &task_text(id, &format!("group: one\n{depends}"), BODY),
            );
        }
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();
        let (lines, starts, _) = tasks_pane_lines(&groups, &pipelines, &state, 60);
        assert_eq!(starts.len(), ids.len(), "one start per task: {lines:?}");

        // Where each task's header and its last row, the description, sit.
        let header = |id: &str| lines.iter().position(|l| l.trim_end() == format!("  {id}"));
        let last = |id: &str| {
            lines
                .iter()
                .position(|l| l.contains("Description:") && l.contains(&format!("{id}, done")))
        };
        let blocks: Vec<(usize, usize)> = ids
            .iter()
            .map(|id| (header(id).unwrap(), last(id).unwrap()))
            .collect();

        // Every scroll position: a focus on each line in turn walks the
        // view from the top of the pane to its bottom one line at a time.
        for rows in [4, 7, 8, 13] {
            for line in 0..lines.len() {
                let shown = window(
                    &lines,
                    Items {
                        starts: &starts,
                        noun: Some("task"),
                    },
                    (line, line),
                    Some(rows),
                    200,
                );
                // Where the view sits, found by matching its own rows
                // against the pane's, and checked to be the only place
                // they match so the count below is not read off a guess.
                let view = &shown[..shown.len() - 1];
                let at: Vec<usize> = (0..=lines.len() - view.len())
                    .filter(|&o| lines[o..o + view.len()] == *view)
                    .collect();
                assert_eq!(at.len(), 1, "rows {rows}, focus line {line}: {view:?}");
                let (top, bottom) = (at[0], at[0] + view.len());
                let above = blocks.iter().filter(|&&(first, _)| first < top).count();
                let below = blocks
                    .iter()
                    .filter(|&&(first, end)| first >= top && end >= bottom)
                    .count();
                let marker = shown.last().unwrap();
                let expected = marker_row(above, below, Some("task"), 200);
                assert_eq!(
                    marker, &expected,
                    "rows {rows}, focus line {line}: view {view:?}"
                );
            }
        }
    }

    /// Too narrow for the full row, the marker drops the noun, then the
    /// words, and draws the first form that fits — never a row wider than
    /// the narrowest pane the screen ever draws, where `pad_to` would cut
    /// it off without a sign.
    #[test]
    fn the_marker_row_shortens_itself_to_fit_the_pane() {
        assert_eq!(
            marker_row(11, 19, Some("group"), MIN_LEFT_PANE),
            "↑ 11 above · ↓ 19 below"
        );
        assert_eq!(marker_row(11, 19, Some("group"), 12), "↑ 11 · ↓ 19");
        for noun in ["group", "task", "folder"] {
            for above in [0, 1, 9, 11, 99, 111] {
                for below in [0, 1, 9, 19, 99, 199] {
                    let row = marker_row(above, below, Some(noun), MIN_LEFT_PANE);
                    assert!(
                        row.chars().count() <= MIN_LEFT_PANE,
                        "{row:?} is wider than {MIN_LEFT_PANE} columns"
                    );
                }
            }
        }
    }
}
