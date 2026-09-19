//! Composing the text an agent is handed for a step: the system prompt and
//! the opening message, and the paragraphs each is built from.
//!
//! Everything here is a function of a task, a step, a pipeline and a
//! prompt's own text, with no dispatcher state. That is the whole reason it
//! lives apart from [`crate::dispatch`]: composing what a lane is told does
//! not need a running pass, a multiplexer or a task lock, only the same few
//! facts every one of these functions takes.
//! One file is read on the way, when the task has a dependency: that
//! dependency's own task file, for the branch its worktree was cut from
//! (see [`Repo::dependency_branch`]) — why [`system_prompt`] and
//! [`situating`] take a [`Repo`] and return a `Result`: a dependency that
//! cannot be resolved is an error, not a guessed branch name in the prompt.
//! The seven typed messages a lane's pane receives need nothing from disk —
//! they are spoolway's own fixed wording, with no project override left to
//! resolve against them — so their own functions take no [`Repo`] at all.
//! Writing the system prompt to disk (`write_system_prompt`) and everything
//! about actually starting a lane with it stays in `dispatch`, which is the
//! half of this that owns the filesystem and the multiplexer.

use anyhow::Result;

use crate::pipeline::Pipeline;
use crate::pipeline::Step;
use crate::repo::Repo;
use crate::task::Task;

/// The column [`what_you_have`] and [`what_you_write_down`] share, so their
/// rows line up when a lane reads one block straight after the other — the
/// widest label between the two blocks, computed once rather than kept as a
/// literal in two places that would drift apart. `WHAT YOU HAVE`'s own
/// labels are the wider of the two sets today ("scratch space"), but this is
/// read from both rather than assumed, so a longer heading added to either
/// still lines up.
fn column_width() -> usize {
    WHAT_YOU_HAVE_LABELS
        .iter()
        .map(|l| l.len())
        .chain(HEADINGS.iter().map(|(h, _)| h.len()))
        .max()
        .unwrap_or(0)
}

const WHAT_YOU_HAVE_LABELS: [&str; 4] =
    ["your change", "your commits", "scratch space", "you sit on"];

/// The three headings spoolway appends to a task file, each paired with the
/// one-line prose `WHAT YOU WRITE DOWN` states it in. Fixed and unconfigurable
/// — a project that wants a lane told more about what a handoff is for has
/// its own prompt to say it in, not a second copy of this file's own rules.
/// Order matters: this is also the order the block lists them in.
const HEADINGS: &[(&str, &str)] = &[
    ("Status Log", "one line per transition"),
    (
        "Handoff",
        "what the next step needs and the diff does not show",
    ),
    ("Blocker", "what is in the way, and what you tried"),
];

/// The system prompt a lane is started with: spoolway's framing, the project's
/// prompt, and this pass's own policy and report contract — composed into
/// one file, under three headings, and handed to the agent as
/// `{prompt_file}`.
///
/// Composing rather than sending two messages is what makes spoolway's half
/// unskippable. Everything the dispatcher knows and a prompt cannot — which
/// step this is, what the route is, what the powers are, how the turn ends —
/// used to ride partly in the typed message, which is the half a model treats
/// as *a request* rather than as *the rules*. A project that deleted the
/// shipped prompts and wrote its own kept the config but lost the prose that
/// made the config mean anything. Here it cannot: the prompt is a section of
/// this file, not a rival to it.
///
/// The file on disk is untouched by all of this. `.spoolway/prompts/<name>/`
/// stays the project's outright, spoolway never rewrites a word of it, and the
/// composition happens per lane at launch — which is the same trade every other
/// derived fact in here makes.
///
/// Three headings, plain text rather than the old `====`-ruled banners:
/// `YOUR LANE` (which carries [`what_you_have`] and [`what_you_write_down`]
/// inside it, rather than opening `THIS PASS` on its own), `YOUR ROLE AT
/// THIS STEP`, and `THIS PASS` — which now closes with the report contract
/// itself, so the old fourth heading, `HOW THIS TURN ENDS`, is gone. Order
/// is otherwise what it always was: the framing first, because it is the
/// premise the rest is read
/// against; the prompt in the middle, as the role; and the contract last,
/// because the one thing a model must not have lost track of by the end of a
/// long turn is the command that ends it. `blocked` alone also gets
/// [`toolbox`], right after the prompt — no other lane reaches `queue
/// list`, `queue show` or `lane` through this file.
pub(crate) fn system_prompt(
    repo: &Repo,
    task: &Task,
    pipeline: &Pipeline,
    step: &Step,
    prompt: &str,
) -> Result<String> {
    // A toolbox for `blocked` alone — see [`toolbox`]. Every other step reads
    // this empty, which is also what keeps `cost`, `eval`, `doctor` and
    // `config get` out of a lane's reach entirely: nothing here ever names
    // them.
    let toolbox = match step.id == crate::pipeline::BLOCKED {
        true => format!("\n\n{}", toolbox()),
        false => String::new(),
    };

    Ok(format!(
        "{situating}\n\n\
         YOUR ROLE AT THIS STEP\n\n\
         {prompt}{toolbox}\n\n\
         THIS PASS\n\n\
         {policy}{contract}",
        situating = situating(pipeline, step, task, repo)?,
        prompt = prompt.trim(),
        policy = policy(repo, task, pipeline, step),
        contract = report_contract(task, step),
    ))
}

/// What a lane is, in the words of somebody who has never heard of spoolway,
/// plus [`what_you_have`] — together, the whole of `YOUR LANE`.
///
/// Not decoration. A model handed a task file and a role reads a solo
/// assignment, and the mistakes that follow all rest on that one premise: it
/// widens past the step it was given, it asks a question into a pane nobody
/// is watching, it ends its turn expecting something to bring it back to one,
/// or it treats what it was told not to do as an obstacle to work around
/// rather than a boundary of the step.
///
/// `lane` is spoolway's own word — one running agent, on one task, at one
/// step — and the reader has no glossary, so it is defined in the sentence
/// that introduces it rather than assumed.
///
/// One bullet varies by step — `blocked` alone rewrites the first: every
/// other lane owns one task's own step, and `blocked`'s does not. The rest
/// read the same for every lane; a gated step's fact that a person opens
/// this pane belongs to [`report_contract`], the form it qualifies, not
/// here.
///
/// The last bullet is fixed for every lane, gated or not, because composing
/// happens once at launch and this file is all a pane has once the person in
/// it starts talking: a `p` park from the board, a gate, or a step landing
/// on `blocked` with nobody staffed to clear it all leave the same lane
/// sitting in the same pane, and none of the three is knowable ahead of the
/// turn that might cause it. A stopped task belongs to whoever is looking at
/// its pane — see `commands::task_edit` — so the one thing worth fixing in
/// place, for every step, is that the lane's own remit stops being the
/// ceiling on what it will do there.
pub(crate) fn situating(
    pipeline: &Pipeline,
    step: &Step,
    task: &Task,
    repo: &Repo,
) -> Result<String> {
    let first_bullet = match step.id == crate::pipeline::BLOCKED {
        true => format!(
            "- Your remit is the run, not one task's step. `{task}` may have stopped inside \
             its worktree or well outside it. Stay inside your prompt's bounds.",
            task = task.id(),
        ),
        false => "- One step's worth of the job, and nothing enforces it. Your role, below, is \
                   the whole of what is yours."
            .to_string(),
    };

    Ok(format!(
        "YOUR LANE\n\n\
         spoolway runs one task at a time through a pipeline of steps, one agent per step. You \
         are a lane: step `{step}` of task `{task}` in pipeline `{pipeline}`, in a git \
         worktree spoolway cut for you. That worktree is your current directory.\n\n\
         {first_bullet}\n\
         - Your output is not read; only what you write to the task file reaches anyone. Ask \
         nothing unless told to.\n\
         - Nothing will wake you. Poll anything you wait on.\n\
         - Reporting is the only exit. A turn ended any other way stalls the task.\n\
         - Commit as you go. Anything uncommitted is committed for you when you report.\n\
         - If this task is ever held on `paused` or `blocked` and a person carries on \
         talking in this pane, do what they ask — including work your step would \
         otherwise leave to another. Resuming it stays theirs alone.\
         {what_you_have}\
         {what_you_write_down}",
        step = step.id,
        task = task.id(),
        pipeline = pipeline.name,
        what_you_have = what_you_have(repo, task)?,
        what_you_write_down = what_you_write_down(),
    ))
}

/// The `WHAT YOU HAVE` block: the exact commands and paths this lane needs,
/// resolved rather than named. A prompt that may not say `base:` still has
/// to read its own diff; naming the field and leaving the lane to compose the
/// command from it keeps spoolway's vocabulary in the prompt regardless —
/// handing over the finished line is what actually gets it out.
///
/// The diff and log read against `task.front.base` when there is no
/// dependency, and against the first dependency's own branch when there is —
/// read from that task's own `branch:` field through
/// [`Repo::dependency_branch`], not `task.front.base` even then: `base` is
/// only the queueing worktree's branch at the moment this task was added,
/// which is the dependency's branch solely when a planner happened to queue
/// from inside it, and something else — the shared plan branch, say — when
/// it queued both tasks up front instead. The branch name is the one fact
/// that never depends on where the queueing happened.
///
/// A dependency's branch cannot say what finishing that task actually left
/// behind, which is the one thing `spoolway queue show` adds — one line per
/// entry of `depends_on`, not only the first, since the diff picks one
/// dependency to read against but a lane may still owe reading to the rest.
/// This is the reading list's old job, folded in here now that `reading_block`
/// is gone: a single line named once is one line to read, not two.
fn what_you_have(repo: &Repo, task: &Task) -> Result<String> {
    // Computed rather than read off the environment: the scratch directory is
    // made later, in the same launch that calls this, and a lane resolving
    // its own facts from `$SPOOLWAY_SCRATCH` is exactly the naming this block
    // exists to replace. See the identical join beside `SPOOLWAY_SCRATCH`
    // above — two computations rather than one is what threading the value
    // down here would cost instead.
    let scratch = repo.scratch_dir().join(&task.front.id);

    // One column for every label, its width computed rather than counted by
    // hand — hand-counting is what broke this the first time: Rust's
    // backslash line-continuation strips a continued source line's own
    // leading whitespace, so a label written indented in the source landed
    // at column 0 in the actual prompt, and the fix that follows builds each
    // line as its own value instead of relying on that continuation for
    // anything but joining words. Shared with `WHAT YOU WRITE DOWN` — see
    // [`column_width`] — so the two blocks' rows line up.
    let width = column_width();
    let blank = " ".repeat(width + 2);

    let (base, sits_value, extra) = match task.front.depends_on.first() {
        Some(first) => {
            let extra: String = task
                .front
                .depends_on
                .iter()
                .map(|dep| format!("\n{blank}`spoolway queue show {dep}` — what it left you"))
                .collect();
            // The dependency's real branch, read from its task file — not
            // rebuilt as `task/<first>`, because `issue_tracking.key_in_names`
            // may have stamped `task/<slug>-<first>`. A dependency that cannot
            // be resolved is a hard error here, the same one `ensure_workspace`
            // raises when it cuts this
            // task's worktree: a prompt that named a guessed branch would
            // tell the lane to diff against a ref that need not exist.
            let branch = repo.dependency_branch(first)?;
            (
                branch.clone(),
                format!("`{first}`, branch `{branch}`"),
                extra,
            )
        }
        None => {
            let base = task
                .front
                .base
                .clone()
                .unwrap_or_else(|| "HEAD".to_string());
            let sits_value = format!("`{base}`");
            (base, sits_value, String::new())
        }
    };

    let lines = [
        format!(" {:width$} `git diff {base}...HEAD`", "your change"),
        format!(
            " {:width$} `git log --oneline {base}..HEAD`",
            "your commits"
        ),
        format!(" {:width$} `{}`", "scratch space", scratch.display()),
        format!("{blank}yours, outside the worktree, deleted with the task"),
        format!(" {:width$} {sits_value}{extra}", "you sit on"),
    ];

    Ok(format!("\n\nWHAT YOU HAVE\n\n{}", lines.join("\n")))
}

/// Total characters on one wrapped line of `WHAT YOU WRITE DOWN`, margin
/// included. A fixed number rather than one read off the terminal: the text
/// this wraps is sent to a model, not printed to a screen, and a model reads
/// a stable shape on every pass rather than one that reflows with whoever's
/// window happens to be open. 66 is the smallest value that keeps
/// [`HEADINGS`]' own Handoff line — the longest of the three — on one row.
const WRAP_WIDTH: usize = 66;

/// The `WHAT YOU WRITE DOWN` block: three built-in lines, one per heading
/// spoolway appends to a task file — see [`HEADINGS`]. Fixed rather than a
/// project's to rewrite: the contract this file states is spoolway's own,
/// and a project with more to say about a handoff says it in its own prompt.
///
/// Drawn like [`what_you_have`] — same label column, same wrapped-line
/// indent, though none of the three lines here is long enough to wrap today.
fn what_you_write_down() -> String {
    let width = column_width();
    let margin = width + 2;
    let avail = WRAP_WIDTH.saturating_sub(margin);

    let rows: Vec<String> = HEADINGS
        .iter()
        .map(|(heading, prose)| {
            let mut wrapped = wrap(prose, avail).into_iter();
            let first = wrapped.next().unwrap_or_default();
            let mut row = format!(" {heading:width$} {first}");
            for line in wrapped {
                row.push('\n');
                row.push_str(&" ".repeat(margin));
                row.push_str(&line);
            }
            row
        })
        .collect();

    format!("\n\nWHAT YOU WRITE DOWN\n\n{}", rows.join("\n"))
}

/// Break `text` into lines of at most `width` characters, on word
/// boundaries only. A single word longer than `width` is kept whole on its
/// own line rather than cut mid-word — an overlong line reads better than a
/// severed one.
///
/// Measured in `chars()`, not bytes: [`HEADINGS`]' own Handoff line carries
/// no multi-byte character today, but a byte count would wrap early the
/// moment one of these three lines gained one.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_len = 0;
    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        let fits = line.is_empty() || line_len + 1 + word_len <= width;
        if !fits {
            lines.push(std::mem::take(&mut line));
            line_len = 0;
        }
        if !line.is_empty() {
            line.push(' ');
            line_len += 1;
        }
        line.push_str(word);
        line_len += word_len;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The `blocked` step's own toolbox: three read-only commands for reading the
/// run, plus the one write a `blocked` lane alone may make, offered to no
/// other step and placed right after the prompt rather than inside `THIS
/// PASS` — see [`system_prompt`].
///
/// Clearing a block is often a question about another task, or about a lane's
/// own transcript — and once a prompt may not name a spoolway command, that
/// has to come from here or nowhere. Every other step goes without it: the
/// tokens are not worth spending on a lane whose remit is its own one task,
/// and a wider command surface is a wider set of traps for a review to
/// imagine. `cost`, `eval`, `doctor` and `config get` stay out for the same
/// reason none of the four reach a lane at all — they answer for a run or an
/// installation as a whole, which is a person's question, not a lane's.
///
/// `spoolway resume` is the exception to the read-only rule above it, bounded
/// in the two ways `commands::report::resume` itself enforces: never past a
/// task waiting on a gate, and never with `--stage`, which reroutes rather
/// than clears a block.
fn toolbox() -> String {
    "READING THE RUN — yours at this step only:\n\n\
     `spoolway queue list` — every task, and where each sits\n\
     `spoolway queue show <task>` — one task's file, goal to `## Status Log`\n\
     `spoolway lane` — this run's lanes; name one for its transcript\n\
     `spoolway resume <task>` — put another stopped task back on its step; not\n  \
     one waiting on a gate, and never with `--stage`"
        .to_string()
}

/// Everything true of *this* pass and no other, as the paragraphs `THIS
/// PASS` opens with: a fix pass's findings, a failed command step.
///
/// This is the half that must not be a prompt's to write. Each of these is
/// per-pass state, and a file that stated it would be a file that can
/// contradict the run. The gate fact — whether a person will read this pane
/// once the lane reports, and how it ends its turn — used to live here too,
/// as advice the binary had no way to enforce; it is gone, and
/// [`report_contract`] states the one part of it a lane can act on, as a
/// form rather than a paragraph.
pub(crate) fn policy(repo: &Repo, task: &Task, pipeline: &Pipeline, step: &Step) -> String {
    let paragraphs: Vec<String> = [
        arrived_by_fail_paragraph(task, pipeline, step),
        failed_command(repo, task, pipeline, step),
    ]
    .into_iter()
    .filter(|p| !p.is_empty())
    .collect();
    match paragraphs.is_empty() {
        true => String::new(),
        false => format!("{}\n\n", paragraphs.join("\n\n")),
    }
}

/// The step this task arrived from, when this pass exists because that
/// step's *fail* route led here and its *pass* route led somewhere else —
/// never when both routes lead here, which cannot say which one was taken.
/// Shared by [`failed_command`] and [`arrived_by_fail_paragraph`], which
/// each answer for one kind of step that can leave this true: a command
/// step's exit code for the former, an agent step's own reported fail for
/// the latter.
///
/// Read through [`Step::destination`], not the raw `on_fail`/`on_pass`
/// fields: a step declaring no `on_fail` of its own still fails to
/// `blocked`, and this has to notice that arrival exactly the same as one
/// that names the step outright.
fn arrived_by_fail<'a>(task: &Task, pipeline: &'a Pipeline, step: &Step) -> Option<&'a Step> {
    let from = task.front.arrived_from.as_deref()?;
    let previous = pipeline.step(from)?;
    let arrived_by =
        |outcome: crate::pipeline::Outcome| previous.destination(outcome) == Some(step.id.as_str());
    (arrived_by(crate::pipeline::Outcome::Fail) && !arrived_by(crate::pipeline::Outcome::Pass))
        .then_some(previous)
}

/// The paragraph a lane gets when a *command* step failed into it.
///
/// A command step reports nothing: it is an exit code, and the dispatcher
/// routes on the number without a word about what the number meant. That was
/// silently wrong for `on_fail` — a step whose failure route is a lane sent
/// that lane the same prompt it would have got arriving from anywhere else,
/// which redid the work, reported the same pass, and handed the tree back to
/// the command step unchanged for it to fail on again — a loop that cannot
/// converge, because the one fact that would end it is the one fact never
/// passed along.
///
/// So this names the step and points at the log rather than quoting it: the
/// output of a real gate is tens of thousands of lines, and a tail cut to fit
/// a prompt would as often as not cut away the error.
fn failed_command(repo: &Repo, task: &Task, pipeline: &Pipeline, step: &Step) -> String {
    let Some(previous) = arrived_by_fail(task, pipeline, step) else {
        return String::new();
    };
    if previous.run.is_none() {
        return String::new();
    }

    let from = previous.id.as_str();
    let runs = crate::command_step::Runs::new(&repo.commands_dir());
    let log = runs.log_path(&crate::command_step::Runs::key(from, task.id()));
    format!(
        "The command step `{from}` failed and routes back here. It left an exit code and no \
         report; its whole output is at\n\n    {}",
        log.display(),
    )
}

/// The paragraph a lane gets when an *agent* step failed it back here — what
/// a failing `## Review` verdict used to say, before a review became this
/// project's own concern and not spoolway's. Spoolway's own fixed wording:
/// a situation and where to read the rest, nothing about how to treat it —
/// a project that wants a fix pass held to some further habit writes that
/// into its own prompt.
///
/// [`failed_command`]'s own case, not this one: a command step reports
/// nothing, so there is no verdict here for it to name.
fn arrived_by_fail_paragraph(task: &Task, pipeline: &Pipeline, step: &Step) -> String {
    let Some(previous) = arrived_by_fail(task, pipeline, step) else {
        return String::new();
    };
    if previous.run.is_some() {
        return String::new();
    }

    format!(
        "Fix pass. `{from}` failed this task back. Its findings are in {task_file}'s `## \
         Handoff`. Address exactly those.",
        from = previous.id,
        task_file = task.path.display(),
    )
}

/// The report contract: the exact `spoolway report` command that ends a
/// lane's turn, and every flag it takes. Closes `THIS PASS`, after whatever
/// [`policy`] added — the one thing a model must not have lost track of by
/// the end of a long turn is the command that ends it.
///
/// Forms only, and nothing else: no prose choosing a verb for the lane, no
/// mention of any step or role, no `stage:` warning — those are the
/// project's own prompt to give or spoolway's policy to enforce, not a
/// paragraph competing with them for the same attention. `--handoff` stays,
/// beside the others, because it is the one mechanism by which any step
/// leaves something for the next — spoolway's to enable, never a project's
/// to be reminded about.
///
/// A lane is offered only the outcomes that route somewhere different from
/// each other. `blocked` is the fixed case — [`crate::pipeline::Step::
/// destination`] never actually asks it for a `Fail` or a `Block`, since
/// `commands::report` reads either the same way it reads a `--pause` before
/// the question reaches the graph — so it keeps its own three forms,
/// `--pass`, `--pass --stage <step>` and `--pause`. Every other step
/// compares `destination(Fail)` against
/// `destination(Block)`: equal, and a `--fail` would park the task exactly
/// where a `--block` already does, so it is withheld; distinct, and both are
/// offered. `--block` is the one kept on a collision rather than `--fail`,
/// because a step declaring no `on_fail` of its own reads as "nowhere in
/// particular to send a failure", which is what a block already means, and
/// not as a promise that failing this step is a distinct outcome from
/// getting stuck on it.
///
/// A form this left out is named anyway, under refusal wording — a lane
/// that has never been told a command exists cannot be tempted to reach for
/// it, but a lane that infers `--fail` from having seen `--block` and
/// `--pass` can, so the gap is closed rather than left silent.
///
/// One line closes the whole contract when `step` would hold a pass here —
/// see [`crate::commands::gate_hold`], read against a hypothetical pass,
/// since no real outcome exists yet while a prompt is being composed. Which
/// gate answers picks the sentence: a step's own `gate: true` catches a pass
/// and nothing else, so it reads *a pass is held here*; a task's own
/// `gate_at` catches whatever is reported, so it reads *this report is held
/// here, whatever it is*.
pub(crate) fn report_contract(task: &Task, step: &Step) -> String {
    use crate::pipeline::Outcome;

    let blocked = step.id == crate::pipeline::BLOCKED;
    let fail_redundant =
        !blocked && step.destination(Outcome::Fail) == step.destination(Outcome::Block);

    let forms = if blocked {
        "    spoolway report --pass  -m \"<one line on what happened>\"\n    \
              spoolway report --pass  --stage <step> -m \"<one line on what happened>\"\n    \
              spoolway report --pause -m \"<what needs a person, and why>\"\n    \
              --handoff \"<what the next step should know>\"   repeatable"
    } else if fail_redundant {
        "    spoolway report --pass  -m \"<one line on what happened>\"\n    \
              spoolway report --block -m \"<what is in the way>\"\n    \
              --handoff \"<what the next step should know>\"   repeatable"
    } else {
        "    spoolway report --pass  -m \"<one line on what happened>\"\n    \
              spoolway report --fail  -m \"<one line on what happened>\"\n    \
              spoolway report --block -m \"<what is in the way>\"\n    \
              --handoff \"<what the next step should know>\"   repeatable"
    };

    // `--stage` only ever means anything alongside `--pass` on `blocked`
    // itself — see `commands::report`'s own refusal by name. Unlike
    // `--pause`, which is simply never offered off `blocked` (there is
    // nothing there to withhold: no other step's contract has ever printed
    // it), `--stage` is a flag every step's own report line could plausibly
    // reach for once it exists at all, so it is named under the refusal
    // wording here rather than left for a lane to discover by trying it.
    let withheld: &[&str] = if blocked {
        &["spoolway report --fail", "spoolway report --block"]
    } else if fail_redundant {
        &[
            "spoolway report --fail",
            "spoolway report --pass --stage <step>",
        ]
    } else {
        &["spoolway report --pass --stage <step>"]
    };

    let mut contract = format!("Your last action is one `spoolway report` command:\n\n{forms}");
    if !withheld.is_empty() {
        contract.push_str(
            "\n\nThese commands are not available to you. Never use one, under any\n\
             circumstance:\n\n",
        );
        let lines: String = withheld
            .iter()
            .map(|command| format!("    {command}\n"))
            .collect();
        contract.push_str(lines.trim_end());
    }

    let hypothetical_destination = step
        .destination(Outcome::Pass)
        .unwrap_or(crate::pipeline::BLOCKED)
        .to_string();
    if let Some(gate) =
        crate::commands::gate_hold(task, step, Outcome::Pass, &hypothetical_destination)
    {
        let line = match gate {
            crate::commands::Gate::Step => "A pass is held here for a person, who opens this pane.",
            crate::commands::Gate::Schedule => {
                "This report is held here for a person, whatever it is."
            }
        };
        contract.push_str("\n\n");
        contract.push_str(line);
    }
    contract
}

/// Every state a lane's pane is prompted in, in the order a lane can reach
/// them — what `spoolway prompt contract`'s section 3 shows, one state at a
/// time. `reminder` is the odd one out — not a launch at all, but the nudge
/// sent to a lane that has gone quiet.
pub(crate) const STATES: &[&str] = &[
    "opening",
    "resume",
    "resume-unattended",
    "carry",
    "park",
    "park-escalated",
    "reminder",
];

/// The message typed into a lane's pane once it is up: a pointer to the task
/// file, and nothing else.
///
/// Everything else — the report forms, `--handoff`, the reasoning behind
/// them — is in [`system_prompt`], the half a model reads
/// as *the rules* rather than as *a request*; repeating it here would be a
/// second copy in the half most likely to be treated as optional.
/// The unused half of the signature is kept on purpose: this and
/// [`system_prompt`] are the two things a lane is sent, they are called from
/// the same three places, and a caller should not have to remember which one
/// needs less. The day a fact about the step belongs in the typed message
/// again, it is already here.
///
/// A step naming `skills:` gets one `/name` line per skill first, in
/// declaration order, then a blank line, then the task file's own line. It
/// has to lead: a harness only expands a slash command where it opens a
/// message, so an invocation appended after it would just be read as text
/// about a skill rather than a command to run one.
pub(crate) fn opening_prompt(task: &Task, _pipeline: &Pipeline, step: &Step) -> String {
    let skills: String = step
        .skills
        .iter()
        .map(|name| format!("/{name}\n"))
        .collect();
    format!(
        "{skills}\nRead {} before anything else.",
        task.path.display()
    )
    .trim()
    .to_string()
}

/// What a resumed lane is told, instead of the opening briefing.
///
/// It is the same session: it has the task, the work it did and the reason it
/// stopped, and handing it the opening prompt again is an instruction to redo
/// the reading a resume exists to avoid. What it cannot know is what happened
/// while it was stopped, so that is what this says — and the two runs have
/// opposite answers.
///
/// Attended, a person has been and gone, and the `## Status Log` says what
/// they did. Unattended, an unblocker lane has — the default staffing for
/// `blocked` under `unattended.blocked_prompt` — and both `## Status Log`
/// and `## Blocker` say what it tried.
///
/// No report form and no `stage:` warning here any more — both are already
/// in [`system_prompt`], which a resumed lane was also launched with, so
/// repeating them a second time in the typed message is exactly the
/// duplication this task cuts. `pipeline` is unused for the same reason it
/// still appears: kept so the three prompt functions share one shape and a
/// caller never has to remember which needs it.
pub(crate) fn resume_prompt(task: &Task, _pipeline: &Pipeline, unattended: bool) -> String {
    let task_file = task.path.display();
    match unattended {
        false => format!(
            "This lane was blocked; a person has cleared it. Same session — do not start \
             over. What they did is the last `## Status Log` entry in {task_file}."
        ),
        true => format!(
            "This lane was blocked; an unblocker lane has since run. Same session — do not \
             start over. What it did is the last `## Status Log` entry in {task_file}; what \
             it tried is `## Blocker`."
        ),
    }
}

/// What a lane resumed by `session:` is told, instead of the opening
/// briefing — distinct from [`resume_prompt`], because nothing here stopped.
///
/// A blocked lane was unblocked *by a person*, and that is the fact
/// `resume_prompt` exists to say. A `session:` step's prompt simply comes
/// back for its next visit — a fix after a review, a second review after the
/// fix — with nobody in between and nothing to explain except what changed.
pub(crate) fn carry_prompt(task: &Task, _pipeline: &Pipeline) -> String {
    format!(
        "Same session, continued: your next visit to this task, with nobody in between. Do \
         not start over. Why you are back is in `THIS PASS`; what changed is in the `## \
         Status Log` of {}.",
        task.path.display(),
    )
}

/// What a lane resumed after a park is told, instead of the opening
/// briefing — distinct from both [`resume_prompt`] and [`carry_prompt`],
/// because it must say the one thing neither of those may: nothing here
/// failed a check.
///
/// Three gestures land a task on `paused` with `parked_from` set rather than
/// a gate or a real block, and `escalated` — [`crate::task::Frontmatter::
/// escalated`] — is what tells the third apart from the first two, which this
/// answers for without knowing which of them it was: the board's `p` key,
/// and a person's own Escape typed straight into the pane — see
/// [`crate::dispatch::Dispatcher::park_after_interrupt`]. Neither of those
/// changed anything, so the `park` state says so, plainly, the opposite of
/// what [`resume_prompt`]'s unattended half sends a lane hunting `## Blocker`
/// for. `escalated` is the third gesture — [`crate::dispatch::Dispatcher::
/// tear_down_and_escalate`], a lane reminded three times and torn down, or
/// one stopped past its context ceiling — where something *did* happen, and
/// `park`'s own wording would be false; `park-escalated` says what.
pub(crate) fn park_prompt(task: &Task, _pipeline: &Pipeline, escalated: bool) -> String {
    match escalated {
        false => "A person stopped this lane's turn and has put it back. Nothing changed. \
                  Same session — pick up where you were cut off."
            .to_string(),
        true => format!(
            "This lane went quiet and was torn down; the task is back here. Nothing failed a \
             check. Same session — why is in the last `## Status Log` entry in {}.",
            task.path.display(),
        ),
    }
}

/// The seventh typed message: what a lane that has gone quiet is nudged with,
/// repeating the report contract it was launched with. Its own function
/// rather than inlined at the one call site, so `spoolway prompt contract`
/// can render it too, against the same wording a real nudge would use.
pub(crate) fn reminder_prompt(task: &Task, step: &Step) -> String {
    format!(
        "`{}` ended its turn without reporting.\n\n{}",
        step.id,
        report_contract(task, step),
    )
}

/// Every one of the seven typed messages, rendered for `state` against a real
/// or sample task — what `spoolway prompt contract`'s section 3 shows, one
/// state at a time. `state` outside [`STATES`] renders empty rather than
/// panicking: the contract's own loop is the only caller, and it never asks
/// for anything else.
pub(crate) fn lane_prompt_for_state(
    task: &Task,
    pipeline: &Pipeline,
    step: &Step,
    state: &str,
) -> String {
    match state {
        "opening" => opening_prompt(task, pipeline, step),
        "resume" => resume_prompt(task, pipeline, false),
        "resume-unattended" => resume_prompt(task, pipeline, true),
        "carry" => carry_prompt(task, pipeline),
        "park" => park_prompt(task, pipeline, false),
        "park-escalated" => park_prompt(task, pipeline, true),
        "reminder" => reminder_prompt(task, step),
        _ => String::new(),
    }
}
