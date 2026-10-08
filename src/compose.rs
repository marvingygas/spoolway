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
//! The eight typed messages a lane's pane receives need nothing from disk —
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
        contract = report_contract(task, pipeline, step),
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
/// The person bullets are fixed for every lane, gated or not, because
/// composing happens once at launch and this file is all a pane has once the
/// person in it starts talking: a `p` park from the board, a gate, or a step
/// landing on `blocked` with nobody staffed to clear it all leave the same
/// lane sitting in the same pane, and none of the three is knowable ahead of
/// the turn that might cause it. They hold in a running pane too, so a lane's
/// remit is never the ceiling on what it does for a person. `task edit`
/// refuses a task that is still moving (see `commands::task_edit`), which is
/// why the bullet sends a running lane to `--handoff` instead.
///
/// The route is read from `spoolway queue route`, never pasted here: every
/// lane reads every line on every launch, and the pipeline is already one
/// command away.
///
/// The last bullet is fixed the same way: what a person does about anything
/// a lane leaves for them belongs on the board, not quoted as a `spoolway`
/// command a prompt could as easily just run and skip the person entirely.
/// It hands over no command at all: to reroute a task, the person presses `r`
/// on its row and picks the step in the board's picker.
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
                   the whole of what is yours. Steps split the work for a reason; do not do \
                   another step's on your own."
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
         - `spoolway queue route {task}` shows every step this task runs, what each does, and where \
         resuming sends this task. Read it before you tell a person what happens next.\n\
         - If a person talks to you in this pane, do what they ask, whichever step's work it \
         is. Write every change they ask for into the task file, so later steps see it: \
         `spoolway task edit {task} --section <heading> --from -` while the task is held on \
         `paused` or `blocked`, `--handoff` while it runs.\n\
         - Resuming a held task stays the person's: once their request is done, tell them \
         where resuming sends it, and to resume it on the board.\n\
         - What a person has to do, name on the board, never as a `spoolway` command. To send \
         the task to another step than resuming would, tell them to press `r` on its row and \
         pick the step.\
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
/// in the three ways `commands::report::resume` itself enforces: never past a
/// task waiting on a gate, never with `--stage`, which reroutes rather than
/// clears a block, and never the lane's own task.
fn toolbox() -> String {
    "READING THE RUN — yours at this step only:\n\n\
     `spoolway queue list` — every task, and where each sits\n\
     `spoolway queue show <task>` — one task's file, goal to `## Status Log`\n\
     `spoolway lane` — this run's lanes; name one for its transcript\n\
     `spoolway prompt show <name>` — the craft a step's lane is briefed with\n\
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
        stands_in_for_paragraph(task, pipeline, step),
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

/// `blocked` alone: which step this pass stands in for, and where its craft
/// is written — a prompt never names spoolway's own mechanisms, and the step
/// changes with every task, so this is the only place a lane on `blocked` can
/// learn either fact.
///
/// Read off `commands::resume_target`, the same lookup a plain `--pass`
/// itself resolves against — never the pipeline's entry, and never
/// `blocked` itself. Empty wherever that resolves to a stage this pipeline
/// does not define as a step (the sample task `spoolway prompt contract`
/// renders with no real task carries no `blocked_from` and so no origin to
/// name), rather than guessing one.
///
/// The step's own `description:` is data a pipeline already declares, not a
/// paraphrase of its prompt — the one fact this can state without pasting a
/// word of that prompt's own prose into `blocked`'s. A command step's
/// craft is the line it runs, told to run again once this pass carries the
/// task past it; an agent step's is the prompt it reads, pointed at rather
/// than quoted, through the one command this file may name.
fn stands_in_for_paragraph(task: &Task, pipeline: &Pipeline, step: &Step) -> String {
    if step.id != crate::pipeline::BLOCKED {
        return String::new();
    }
    let origin_id = crate::commands::resume_target(task, pipeline);
    let Some(origin) = pipeline.step(&origin_id) else {
        return String::new();
    };

    let craft = match origin.run.as_deref() {
        Some(run) => format!(
            "Its craft is the command `{run}`, which runs again once your pass carries this \
             task past it."
        ),
        None => {
            let prompt = origin.prompt_name();
            format!("Its craft is prompt `{prompt}`:\n`spoolway prompt show {prompt}`.")
        }
    };

    let description = origin
        .description
        .as_deref()
        .map(|d| format!("{d} "))
        .unwrap_or_default();

    format!(
        "`{task}` stopped at `{origin}`: {description}Your pass stands in for that step's \
         work. {craft}",
        task = task.id(),
        origin = origin.id,
    )
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

/// One offered form: the exact command line, and the sentence under it
/// saying what reporting it claims — the fact [`report_contract`] used to
/// leave for `spoolway report --help` alone to say, though `--help` is not
/// where a lane reads its own contract from.
type Form = (&'static str, &'static str);

const PASS: Form = (
    "    spoolway report --pass  -m \"<one line on what happened>\"",
    "  This step's work is done and it came out right.",
);

const FAIL: Form = (
    "    spoolway report --fail  -m \"<what is not right, and where>\"",
    "  This step's work is done, and what you found is not right. Your\n  \
     message is what somebody works from to put it right, so say what\n  \
     is wrong and where.",
);

const BLOCK: Form = (
    "    spoolway report --block -m \"<what is in the way>\"",
    "  You cannot settle this within what this step is allowed to do. It\n  \
     goes up to a person, or to a stronger agent. Say what you tried\n  \
     and what stopped you. When a person has to act, name the\n  \
     action and end with: then resume it on the board.",
);

const BLOCKED_PASS: Form = (
    "    spoolway report --pass  -m \"<one line on what happened>\"",
    "  The way is clear. Your own work stands in for the step that got\n  \
     stuck, so the task carries on from there.",
);

const BLOCKED_PASS_STAGE: Form = (
    "    spoolway report --pass --stage <step> -m \"<one line on what happened>\"",
    "  The same, except you name where the task goes next. Only a step\n  \
     this task has already been through.",
);

const BLOCKED_PAUSE: Form = (
    "    spoolway report --pause -m \"<what needs a person, and why>\"",
    "  You could not clear it, and a person has to. Say plainly what they\n  \
     need to do, and end with: then resume it on the board.",
);

const HANDOFF: Form = (
    "    --handoff \"<what the next step should know>\"   repeatable",
    "  Anything worth passing on that one line cannot hold. One per thing.",
);

/// Render a list of [`Form`]s as `report_contract` prints them: the command
/// line, a blank line, its explanation, a blank line, the next form. Takes
/// any lifetime, not only `'static`: `blocked`'s own `--stage` form has an
/// explanation built per task, not one of the fixed [`Form`] constants.
fn render_forms(forms: &[(&str, &str)]) -> String {
    forms
        .iter()
        .map(|(line, explanation)| format!("{line}\n\n{explanation}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The report contract: the exact `spoolway report` command that ends a
/// lane's turn, every flag it takes, and the sentence under each saying what
/// reporting it claims. Closes `THIS PASS`, after whatever [`policy`] added
/// — the one thing a model must not have lost track of by the end of a long
/// turn is the command that ends it.
///
/// Forms and their claims, and nothing else: no prose choosing a verb for
/// the lane, no mention of any step or role, no `stage:` warning — those are
/// the project's own prompt to give or spoolway's policy to enforce, not a
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
/// A form left out is named anyway, under refusal wording — a lane
/// that has never been told a command exists cannot be tempted to reach for
/// it, but a lane that infers `--fail` from having seen `--block` and
/// `--pass` can, so the gap is closed rather than left silent. `--stage`
/// gets no such treatment any more: `commands::report` already refuses it
/// by name off every step but `blocked`, with a message that quotes the
/// task's own step and offers a plain `--pass` instead, so repeating the
/// refusal here would be a second copy of a fact the command itself already
/// enforces — `blocked`'s own forms are what teach a lane the flag exists at
/// all. A step with nothing left to withhold prints no "not available to
/// you" block.
///
/// One line closes the whole contract when `step` would hold a pass here —
/// see [`crate::commands::gate_hold`], read against a hypothetical pass,
/// since no real outcome exists yet while a prompt is being composed. Which
/// gate answers picks the sentence: a step's own `gate: true` catches a pass
/// and nothing else, so it reads *a pass is held here*; a task's own
/// `gate_at` catches whatever is reported, so it reads *this report is held
/// here, whatever it is*.
pub(crate) fn report_contract(task: &Task, pipeline: &Pipeline, step: &Step) -> String {
    use crate::pipeline::Outcome;

    let blocked = step.id == crate::pipeline::BLOCKED;
    let fail_redundant =
        !blocked && step.destination(Outcome::Fail) == step.destination(Outcome::Block);

    // Where this task actually stopped — `blocked` alone, and only to ask
    // two questions of it: whether the first gated step at or after there
    // bounds `--stage`, and whether the origin itself is that step, in which
    // case a plain `--pass` is held there too. Empty for the sample task
    // `spoolway prompt contract` renders with no real task, which carries no
    // `blocked_from` to name.
    let origin = blocked.then(|| crate::commands::resume_target(task, pipeline));
    let stage_gate = origin
        .as_deref()
        .and_then(|origin| crate::commands::first_gated_from(pipeline, origin));

    let forms = if blocked {
        let stage_explanation = match &stage_gate {
            Some(gate_step) => format!(
                "  The same, except you name where the task goes next. Only a step\n  \
                 this task has already been through, and never one past `{gate_step}`."
            ),
            None => BLOCKED_PASS_STAGE.1.to_string(),
        };
        let blocked_stage: (&str, &str) = (BLOCKED_PASS_STAGE.0, stage_explanation.as_str());
        render_forms(&[BLOCKED_PASS, blocked_stage, BLOCKED_PAUSE, HANDOFF])
    } else if fail_redundant {
        render_forms(&[PASS, BLOCK, HANDOFF])
    } else {
        render_forms(&[PASS, FAIL, BLOCK, HANDOFF])
    };

    // `--stage` is never withheld now — see this function's own doc for why.
    // What is left: `--fail` off an ordinary step whose fail and block
    // destinations collide, and both `--fail` and `--block` off `blocked`,
    // which offers neither as a usable form.
    let withheld: &[&str] = if blocked {
        &["spoolway report --fail", "spoolway report --block"]
    } else if fail_redundant {
        &["spoolway report --fail"]
    } else {
        &[]
    };

    let mut contract = format!("Your last action is one `spoolway report` command.\n\n{forms}");
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

    // `blocked` asks this of the step it stands in for, not of itself: a
    // plain `--pass` is taken at the unblocker's word for `origin`, so
    // `origin`'s own `gate: true` is what would hold it — never `blocked`'s,
    // which no pipeline may even declare. A task's own `gate_at` plays no
    // part here: it catches this step's outcome before the task ever reaches
    // `blocked` — see `commands::report::route`'s own doc.
    let gate = match &origin {
        Some(origin) => pipeline
            .step(origin)
            .filter(|step| step.gate)
            .map(|_| crate::commands::Gate::Step),
        None => {
            let hypothetical_destination = step
                .destination(Outcome::Pass)
                .unwrap_or(crate::pipeline::BLOCKED)
                .to_string();
            crate::commands::gate_hold(task, step, Outcome::Pass, &hypothetical_destination)
        }
    };
    if let Some(gate) = gate {
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
    "restart",
    "resume",
    "resume-unattended",
    "carry",
    "park",
    "park-escalated",
    "reminder",
];

/// The briefing a lane is opened with: a pointer to the task file, and
/// nothing else, whatever the step lists.
///
/// Everything else — the report forms, `--handoff`, the reasoning behind
/// them — is in [`system_prompt`], the half a model reads
/// as *the rules* rather than as *a request*; repeating it here would be a
/// second copy in the half most likely to be treated as optional.
/// The unused half of the signature is kept on purpose: this and
/// [`system_prompt`] are the two things a lane is sent, and a caller should
/// not have to remember which one needs less. The day a fact about the step
/// belongs in the typed message again, it is already here.
///
/// What a step's `skills:` add is sent ahead of it as messages of their own —
/// see [`opening_messages`].
pub(crate) fn opening_prompt(task: &Task, _pipeline: &Pipeline, _step: &Step) -> String {
    format!("Read {} before anything else.", task.path.display())
}

/// Every message typed into a lane's pane to open it, in the order they are
/// sent: one `/name` invocation per skill the step lists, in declaration
/// order, then [`opening_prompt`]'s briefing — once.
///
/// Each skill is its own single-line message, and the briefing is not on any
/// of those lines, for two reasons that pull against each other. Claude Code
/// only expands a slash command that opens a message and arrives as typed
/// text. A message spanning more than one line is sent as a paste, wrapped in
/// `<pasted_content>`, where no slash command is ever expanded, so the skills
/// cannot share one multi-line message with the briefing. And whatever
/// follows a `/name` on the same line is that skill's argument, so putting
/// the briefing after the skills on one line hands the whole sentence to
/// every skill and shows it once per skill. One message each avoids both.
///
/// A skill's message is a whole turn, so [`crate::dispatch`] sends only the
/// first at launch and each next one on a later pass that finds the lane
/// settled — never while the turn before it runs, and without holding the pass.
///
/// A step without `skills:` gets exactly one message, the briefing.
///
/// `restarting` is a launch that begins a new conversation on a step that has
/// already run, so its worktree is not clean. The briefing then gains one
/// paragraph saying so. It joins the briefing message rather than arriving as
/// one of its own, because a message of its own would be a second turn, and
/// the skill messages ahead of the briefing are untouched. The briefing is
/// then more than one line, so herdr delivers it as one paste, wrapped in
/// `<pasted_content>`. That costs nothing a slash command needs: the briefing
/// is never a `/name` message, and the lane's actual rules are in its system
/// prompt, not in this message. The paragraph stays on its own line, apart
/// from the pointer to the task file, so the two read as two things.
pub(crate) fn opening_messages(
    task: &Task,
    pipeline: &Pipeline,
    step: &Step,
    restarting: bool,
) -> Vec<String> {
    let mut briefing = opening_prompt(task, pipeline, step);
    if restarting {
        briefing.push_str(
            "\n\nAn earlier attempt on this step ran in this worktree, and its changes are \
             still here — some of them committed. Read what it did in the `## Status Log` \
             before you add to it.",
        );
    }
    step.skills
        .iter()
        .map(|name| format!("/{name}"))
        .chain(std::iter::once(briefing))
        .collect()
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

/// The eighth typed message: what a lane that has gone quiet is nudged with,
/// repeating the report contract it was launched with. Its own function
/// rather than inlined at the one call site, so `spoolway prompt contract`
/// can render it too, against the same wording a real nudge would use.
pub(crate) fn reminder_prompt(task: &Task, pipeline: &Pipeline, step: &Step) -> String {
    format!(
        "`{}` ended its turn without reporting.\n\n{}",
        step.id,
        report_contract(task, pipeline, step),
    )
}

/// The messages a lane's pane is sent in `state`, rendered against a real or
/// sample task — what `spoolway prompt contract`'s section 3 shows, one state
/// at a time. Every state is one message but `opening` and `restart` on a
/// step that lists `skills:`, which are one per skill and then the briefing, kept apart here
/// because the lane receives them apart. `state` outside [`STATES`] renders
/// none rather than panicking: the contract's own loop is the only caller,
/// and it never asks for anything else.
pub(crate) fn lane_prompt_for_state(
    task: &Task,
    pipeline: &Pipeline,
    step: &Step,
    state: &str,
) -> Vec<String> {
    match state {
        "opening" => opening_messages(task, pipeline, step, false),
        "restart" => opening_messages(task, pipeline, step, true),
        "resume" => vec![resume_prompt(task, pipeline, false)],
        "resume-unattended" => vec![resume_prompt(task, pipeline, true)],
        "carry" => vec![carry_prompt(task, pipeline)],
        "park" => vec![park_prompt(task, pipeline, false)],
        "park-escalated" => vec![park_prompt(task, pipeline, true)],
        "reminder" => vec![reminder_prompt(task, pipeline, step)],
        _ => Vec::new(),
    }
}
