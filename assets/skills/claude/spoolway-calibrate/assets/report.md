<!--
The shape every calibration report takes. Copy it, fill every <placeholder>, and delete these
comments. The report is tables only: print nothing above the first table and nothing below the
last. Keep the columns exactly as they are, because the person reading this is choosing rows.
One table per kind of fix, in this order, each only when it has a row:
pipeline, prompt, config, template, skill, script, test, source, docs.
Row numbers run on across the tables. Rows in a table go by reward first, then lowest risk.
Evidence is the task name and the lane's own words. A finding with two fixes puts both in
Proposed fix and names the one to take.
Reward is +, ++ or +++. Risk is -, -- or ---.
-->

### Pipeline

| # | Finding | Evidence | Proposed fix | Risk | Reward |
|---|---------|----------|--------------|------|--------|
| 1 | <what is wrong> | <task: "lane's words"> | <the exact edit> | - | +++ |

### Prompt

| # | Finding | Evidence | Proposed fix | Risk | Reward |
|---|---------|----------|--------------|------|--------|
| 2 | <what is wrong> | <task: "lane's words"> | <fix A>, or <fix B>; take A | -- | ++ |
