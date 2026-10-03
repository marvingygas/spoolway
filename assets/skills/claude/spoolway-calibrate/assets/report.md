# Calibration report — <project>, <window>

<!--
The shape every calibration report takes. Copy it, fill every <placeholder>, and delete these
comments. Keep the headings and the table's columns exactly as they are: the person reading this
is choosing which rows to apply, and a familiar shape is what lets them do that quickly.
-->

## Scope

- **Tasks:** <N> archived tasks, finished <first date> to <last date>.
- **Spend:** <$ total>, <the pipelines it was spent in>.
- **Read:** <the prompts, pipelines, settings, skills, scripts and source these runs went through>.

## What the runs show

<!--
The evidence, in prose, in the same order as the table. One short paragraph per finding or group
of findings: the claim first, then the task names and the lanes' own words that show it. Say
where the evidence is thin or another cause is still possible. Evidence never goes inside a
table cell, where it will not fit.
-->

<Finding 1 in plain words.> In `<task>`, the `<step>` lane wrote "<its own words>". <What that
shows, and what it cost.>

## Findings

<!--
One row per grounded finding, ranked by likely value — most valuable first, whatever file it
touches. Kind is one of: prompt, pipeline, config, template, skill, script, test, source, docs.
Files & edit size names every file the fix touches, with its current length and the size of the
edit at the line it lands on. A finding with two honest fixes puts both in the Proposed fix cell
with both sizes, and says which you would take.
-->

| # | Finding | Proposed fix | Kind | Files & edit size |
|---|---------|--------------|------|-------------------|
| 1 | <What is wrong, and what it cost> | <The exact edit, named> | <kind> | `<path/to/file>` (<N> lines)<br>**+<N> / -<N>** at :<LL> |

**Total:** <+N / -N lines across N files>, if all of it is applied.

<!-- Drop the total when there are only a few rows and nobody needs one number. -->

## Decisions for you

<!--
Only what the person has to choose beyond "apply or not": a finding with two fixes, a setting
whose right value depends on how they work, an override whose purpose only they know. Delete
the section when there is nothing to decide.
-->

- **<Decision>:** <the options, and which you would take and why>.

## Next step

Nothing has been changed yet. Name the rows to apply — "all", "1–4", or "everything but 6" — and
any decision above. Each change is checked after it is written.
