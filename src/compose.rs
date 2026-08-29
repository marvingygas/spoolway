//! Composing the text an agent is handed for a step: the system prompt and
//! the opening message, and the paragraphs each is built from.
//!
//! Everything here is a function of a task, a step, a pipeline and a
//! prompt's own text, with no dispatcher state. That is the whole reason it
//! lives apart from [`crate::dispatch`]: composing what a lane is told does
//! not need a running pass, a multiplexer or a task lock, only the same few
//! facts every one of these functions takes and returns a `String` from.
//! The one thing any of it reads is the project's own
//! `.spoolway/templates/lane-prompts.md`, through
//! [`crate::lane_prompts::render`] — which is why the typed-message
//! functions take a [`Repo`] they otherwise have no use for. Writing the
//! system prompt to disk (`write_system_prompt`) and everything about
//! actually starting a lane with it stays in `dispatch`, which is the half
//! of this that owns the filesystem and the multiplexer.

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
        .chain(crate::task_log::HEADINGS.iter().map(|(h, _)| h.len()))
        .max()
        .unwrap_or(0)
}

const WHAT_YOU_HAVE_LABELS: [&str; 4] =
    ["your change", "your commits", "scratch space", "you sit on"];

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
) -> String {
    // A toolbox for `blocked` alone — see [`toolbox`]. Every other step reads
    // this empty, which is also what keeps `cost`, `eval`, `doctor` and
    // `config get` out of a lane's reach entirely: nothing here ever names
    // them.
    let toolbox = match step.id == crate::pipeline::BLOCKED {
        true => format!("\n\n{}", toolbox()),
        false => String::new(),
    };

    format!(
        "{situating}\n\n\
         YOUR ROLE AT THIS STEP\n\n\
         {prompt}{toolbox}\n\n\
         THIS PASS\n\n\
         {policy}{contract}",
        situating = situating(pipeline, step, task, repo),
        prompt = prompt.trim(),
        policy = policy(repo, task, pipeline, step),
        contract = report_contract(pipeline, step.id == crate::pipeline::BLOCKED),
    )
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
/// Two bullets vary by step. The second states whether a person reads this
/// pane once the lane reports — true only for a gated step, whose pass parks
/// rather than routes on; [`policy`] is the paragraph that asks the lane to
/// act on that fact, this is only the fact itself. `blocked` alone rewrites
/// the first bullet: every other lane owns one task's own step, and
/// `blocked`'s does not.
pub(crate) fn situating(pipeline: &Pipeline, step: &Step, task: &Task, repo: &Repo) -> String {
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

    let second_bullet = match is_gated(step, task) {
        true => {
            "- Nobody follows your output as you produce it, but this step is gated: a \
                  person opens this pane once you report. Ask nothing unless told to."
        }
        false => {
            "- Nobody reads your output as you produce it, and no follow-up comes. Ask \
                   nothing unless told to."
        }
    };

    format!(
        "YOUR LANE\n\n\
         spoolway runs one task at a time through a pipeline of steps, one agent per step. You \
         are a lane: step `{step}` of task `{task}` in pipeline `{pipeline}`, in a git \
         worktree spoolway cut for you. That worktree is your current directory.\n\n\
         {first_bullet}\n\
         {second_bullet}\n\
         - Nothing will wake you — not a background job, not a timer. Poll anything you wait \
         on.\n\
         - Reporting is the only exit. A turn ended any other way stalls the task.\n\
         - Commit as you go. Anything left uncommitted is committed for you in one lump when \
         you report.\
         {what_you_have}\
         {what_you_write_down}",
        step = step.id,
        task = task.id(),
        pipeline = pipeline.name,
        what_you_have = what_you_have(repo, task),
        what_you_write_down = what_you_write_down(repo),
    )
}

/// The `WHAT YOU HAVE` block: the exact commands and paths this lane needs,
/// resolved rather than named. A prompt that may not say `base:` still has
/// to read its own diff; naming the field and leaving the lane to compose the
/// command from it keeps spoolway's vocabulary in the prompt regardless —
/// handing over the finished line is what actually gets it out.
///
/// The diff and log read against `task.front.base` when there is no
/// dependency, and against the first dependency's own branch when there is —
/// `task/<id>`, the one `spoolway queue add` always writes, see
/// `commands::queue::add`. Not `task.front.base` even then: it is only the
/// queueing worktree's branch at the moment this task was added, which is
/// the dependency's branch solely when a planner happened to queue from
/// inside it, and something else — the shared plan branch, say — when it
/// queued both tasks up front instead. The branch name is the one fact that
/// never depends on where the queueing happened.
///
/// A dependency's branch cannot say what finishing that task actually left
/// behind, which is the one thing `spoolway queue show` adds — one line per
/// entry of `depends_on`, not only the first, since the diff picks one
/// dependency to read against but a lane may still owe reading to the rest.
/// This is the reading list's old job, folded in here now that `reading_block`
/// is gone: a single line named once is one line to read, not two.
fn what_you_have(repo: &Repo, task: &Task) -> String {
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
            (
                format!("task/{first}"),
                format!("`{first}`, branch `task/{first}`"),
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

    format!("\n\nWHAT YOU HAVE\n\n{}", lines.join("\n"))
}

/// Total characters on one wrapped line of `WHAT YOU WRITE DOWN`, margin
/// included. A fixed number rather than one read off the terminal: the text
/// this wraps is sent to a model, not printed to a screen, and a model reads
/// a stable shape on every pass rather than one that reflows with whoever's
/// window happens to be open. Chosen to keep the built-in wording for all
/// three headings — the common case, since a project rewriting `task-log.md`
/// is the exception — inside the word budget `dispatch::tests` holds the
/// rest of this file to.
const WRAP_WIDTH: usize = 64;

/// The `WHAT YOU WRITE DOWN` block: what belongs under each of the three
/// headings spoolway appends to a task file, resolved from the project's own
/// `.spoolway/templates/task-log.md` — see [`crate::task_log`] — or
/// spoolway's own built-in wording when the project has written no such file
/// at all.
///
/// Drawn like [`what_you_have`] — same label column, same wrapped-line
/// indent — and only for a heading [`crate::task_log::resolve`] actually
/// answers for: a project whose file exists but leaves one heading out gets
/// no row for it, which is that module's own rule and not this function's to
/// second-guess.
fn what_you_write_down(repo: &Repo) -> String {
    let width = column_width();
    let margin = width + 2;
    let avail = WRAP_WIDTH.saturating_sub(margin);

    let rows: Vec<String> = crate::task_log::HEADINGS
        .iter()
        .filter_map(|(heading, builtin)| {
            let prose = crate::task_log::resolve(repo, heading, builtin)?;
            let mut wrapped = wrap(&prose, avail).into_iter();
            let first = wrapped.next().unwrap_or_default();
            let mut row = format!(" {heading:width$} {first}");
            for line in wrapped {
                row.push('\n');
                row.push_str(&" ".repeat(margin));
                row.push_str(&line);
            }
            Some(row)
        })
        .collect();

    match rows.is_empty() {
        true => String::new(),
        false => format!("\n\nWHAT YOU WRITE DOWN\n\n{}", rows.join("\n")),
    }
}

/// Break `text` into lines of at most `width` characters, on word
/// boundaries only. A single word longer than `width` is kept whole on its
/// own line rather than cut mid-word — an overlong line reads better than a
/// severed one.
///
/// Measured in `chars()`, not bytes: the shipped `task-log.md`'s own Handoff
/// prose carries an em dash, three bytes for one character, and a
/// byte-counted line would wrap two columns early the moment a project's own
/// prose used one too.
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
/// run, offered to no other step and placed right after the prompt rather
/// than inside `THIS PASS` — see [`system_prompt`].
///
/// Clearing a block is often a question about another task, or about a lane's
/// own transcript — and once a prompt may not name a spoolway command, that
/// has to come from here or nowhere. Every other step goes without it: the
/// tokens are not worth spending on a lane whose remit is its own one task,
/// and a wider command surface is a wider set of traps for a review to
/// imagine. `cost`, `eval`, `doctor` and `config get` stay out for the same
/// reason none of the four reach a lane at all — they answer for a run or an
/// installation as a whole, which is a person's question, not a lane's.
fn toolbox() -> String {
    "READING THE RUN — yours at this step only:\n\n\
     `spoolway queue list` — every task, and where each sits\n\
     `spoolway queue show <task>` — one task's file, goal to `## Status Log`\n\
     `spoolway lane` — this run's lanes; name one for its transcript"
        .to_string()
}

/// Whether a step's pass will be parked for a person rather than routed on —
/// the same two roads `commands::report` checks when it actually parks one.
///
/// Duplicated rather than shared: `commands::report` computes this alongside
/// `outcome == Outcome::Pass` and `destination != BLOCKED`, neither of which
/// exists yet while a prompt is being composed, and pulling the two clauses
/// that do apply here out into a shared function would cost report.rs a
/// rewrite this task does not ask for.
fn is_gated(step: &Step, task: &Task) -> bool {
    step.gate || task.front.gate_at.as_deref() == Some(step.id.as_str())
}

/// Everything true of *this* pass and no other, as the paragraphs `THIS
/// PASS` opens with: the gate, a fix pass's findings, a failed command step.
///
/// This is the half that must not be a prompt's to write. Each of these is
/// either per-project configuration or per-pass state, and a file that stated
/// them would be a file that can contradict the config.
pub(crate) fn policy(repo: &Repo, task: &Task, pipeline: &Pipeline, step: &Step) -> String {
    // A gated step used to say so in a paragraph asking the lane to print its
    // question and end its turn. That did not work — the request *was* the
    // enforcement, and a small model reads it, decides the work is fine and
    // reports a pass straight through the gate. `gate:` stays entirely
    // `commands::report`'s to enforce; this paragraph tells the lane the one
    // fact worth knowing about its own pane instead — that a person opens it
    // once the pass lands, where every other pane goes unread — so it can
    // leave a real screen running instead of tearing it down.
    let gate = match is_gated(step, task) {
        false => String::new(),
        true => "This step is gated: a person opens this pane once you report a pass. Leave \
                 anything viewable running, close everything else, and finish with a short \
                 account — what you built, what changed, where to look, and the name of the \
                 pane you left running. One screen."
            .to_string(),
    };

    let paragraphs: Vec<String> = [
        gate,
        arrived_by_fail_paragraph(repo, task, pipeline, step),
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
/// step's own `on_fail` named this step and its `on_pass` named somewhere
/// else — never when both routes lead here, which cannot say which one was
/// taken. Shared by [`failed_command`] and [`arrived_by_fail_paragraph`],
/// which each answer for one kind of step that can leave this true: a
/// command step's exit code for the former, an agent step's own reported
/// fail for the latter.
fn arrived_by_fail<'a>(task: &Task, pipeline: &'a Pipeline, step: &Step) -> Option<&'a Step> {
    let from = task.front.arrived_from.as_deref()?;
    let previous = pipeline.step(from)?;
    let arrived_by = |route: &Option<String>| route.as_deref() == Some(step.id.as_str());
    (arrived_by(&previous.on_fail) && !arrived_by(&previous.on_pass)).then_some(previous)
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
         report; its whole output is at\n\n    {}\n\nRead that first and find the actual \
         failure before you touch anything. The tree is as `{from}` found it, so repeating \
         work that already passed sends it back here unchanged.",
        log.display(),
    )
}

/// The built-in `## arrived-by-fail` wording — see [`arrived_by_fail_paragraph`].
const ARRIVED_BY_FAIL: &str = "Fix pass. `{from}` failed this task back to you; its findings \
     are in {task_file}'s `## Handoff`, credited to `{from}`. Address exactly those. Do not \
     re-litigate the verdict.";

/// The paragraph a lane gets when an *agent* step failed it back here — what
/// a failing `## Review` verdict used to say, before a review became this
/// project's own concern and not spoolway's. The project's own words, from
/// `.spoolway/templates/lane-prompts.md`'s `## arrived-by-fail` section, or
/// spoolway's own built-in — see [`crate::lane_prompts::render`], the same
/// per-section resolution the six typed messages already use, reached
/// directly here rather than through [`crate::lane_prompts::STATES`]: this
/// is not one of the six messages typed into a pane, it is a paragraph of
/// the system prompt itself.
///
/// [`failed_command`]'s own case, not this one: a command step reports
/// nothing, so there is no verdict here for it to name.
fn arrived_by_fail_paragraph(repo: &Repo, task: &Task, pipeline: &Pipeline, step: &Step) -> String {
    let Some(previous) = arrived_by_fail(task, pipeline, step) else {
        return String::new();
    };
    if previous.run.is_some() {
        return String::new();
    }

    let task_file = task.path.display().to_string();
    crate::lane_prompts::render(
        repo,
        "arrived-by-fail",
        ARRIVED_BY_FAIL,
        &[("from", previous.id.as_str()), ("task_file", &task_file)],
    )
}

/// The report contract: the exact `spoolway report` command that ends a
/// lane's turn, and every flag it takes. Closes `THIS PASS`, after whatever
/// [`policy`] added — the one thing a model must not have lost track of by
/// the end of a long turn is the command that ends it.
pub(crate) fn report_contract(pipeline: &Pipeline, blocked: bool) -> String {
    // `blocked` gets two forms, not three: `commands::report` turns a
    // `--fail` or a `--block` from this step straight into `paused` anyway,
    // so offering them here would teach a lane a shape that no longer exists.
    let forms = match blocked {
        true => {
            "    spoolway report --pass  -m \"<one line on what happened>\"\n    \
                  spoolway report --pause -m \"<what needs a person, and why>\""
        }
        false => {
            "    spoolway report --pass  -m \"<one line on what happened>\"\n    \
                  spoolway report --fail  -m \"<one line on what happened>\"\n    \
                  spoolway report --block -m \"<what is in the way>\""
        }
    };
    let body = match blocked {
        true => {
            "Exactly one, exactly once. Add `--handoff \"<text>\"` for each thing the next \
             step should know, any outcome; repeatable, appended to the task file's \
             `## Handoff`. If you cannot clear it yourself, `--pause` with the reason."
        }
        false => {
            "Exactly one, exactly once. `--handoff \"<text>\"` per thing the next step should \
             know, any outcome; repeatable, appended to the task file. If you cannot tell how \
             it went, `--block`."
        }
    };
    format!(
        "Your last action is one `spoolway report` command:\n\n\
         {forms}\n\n\
         {body} Pipeline `{pipeline}` routes from here; never edit the task file's `stage:` \
         yourself.",
        pipeline = pipeline.name,
    )
}

/// The built-in wording for each of the six typed messages a lane's pane
/// receives, in [`crate::lane_prompts::STATES`] order — what
/// [`crate::lane_prompts::render`] falls back to when a project's own
/// `.spoolway/templates/lane-prompts.md` is silent about that state. See that
/// module for the substitution rule and the fallback chain.
const OPENING: &str = "{skills}\nRead {task_file} before anything else.";

const RESUME: &str = "You blocked, and a person has unblocked you. Same session, continued: do \
     not start over, and do not re-read what you are still holding. The last `## Status Log` \
     entry in {task_file} is what they did — decide whether it clears what stopped you and \
     carry on. If your work was already done, say so and `--pass`. If the same thing is still \
     in the way, `--block` again and say so rather than working around it.";

const RESUME_UNATTENDED: &str = "You blocked, and nobody is coming — this run is unattended, so it has sent you straight \
     back. Same session, continued: do not start over, and do not re-read what you are still \
     holding. Nothing changed while you waited. What stopped you is the last `## Blocker` entry \
     in {task_file}, exactly as you left it, and clearing it is yours:\n\n\
     1. Reproduce it. Believe what you see over what you remember.\n\
     2. Clear the smallest thing in the way.\n\
     3. Run what failed and see it not fail. Then carry on with your step.\n\n\
     Write down what you tried under `## Blocker` before you finish, whatever the outcome. If \
     your work was already done, say so and `--pass`. If what is in the way needs a decision \
     that is not yours — spending money, changing what the code is supposed to do, touching \
     something outside this task — `--block` again with what you found and what you tried.";

const CARRY: &str = "Same session, continued: your next visit to this task, with nobody in between. What it \
     asked for, or left, has been done. Do not start over, and do not re-read what you are \
     still holding. What changed is in the `## Status Log` of {task_file}.";

const PARK: &str = "A person stopped your turn with a keypress — not anything you reported — and has put you \
     back. Nothing was blocked and nothing changed: no work of yours was undone, nothing was \
     added to the task, nothing new is in your way. Same session, continued: pick up where the \
     interrupt cut you off.";

const REMINDER: &str = "`{step}` ended its turn without reporting. Here is the report contract again:\n\n\
     {report_contract}";

/// The message typed into a lane's pane once it is up: a pointer to the task
/// file, and nothing else — spoolway's own wording, or the project's, if
/// `.spoolway/templates/lane-prompts.md` names an `## opening` section of
/// its own. See [`lane_prompts`].
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
/// declaration order, then a blank line, then `{task_file}`'s line,
/// unchanged. It has to lead: a harness only expands a slash command where
/// it opens a message, so an invocation appended after it would just be
/// read as text about a skill rather than a command to run one. Substituted
/// in as `{skills}` rather than hard-coded ahead of the template, so a
/// project rewriting `## opening` still gets its skills invoked — and
/// renders to nothing, not a stray blank line, when a step names none: the
/// whole message is trimmed after substitution, which is what keeps the
/// built-in wording identical to what this returned before the template
/// existed.
pub(crate) fn opening_prompt(
    repo: &Repo,
    task: &Task,
    _pipeline: &Pipeline,
    step: &Step,
) -> String {
    let skills: String = step
        .skills
        .iter()
        .map(|name| format!("/{name}\n"))
        .collect();
    let task_file = task.path.display().to_string();
    crate::lane_prompts::render(
        repo,
        "opening",
        OPENING,
        &[("task_file", &task_file), ("skills", &skills)],
    )
}

/// What a resumed lane is told, instead of the opening briefing.
///
/// It is the same session: it has the task, the work it did and the reason it
/// stopped, and handing it the opening prompt again is an instruction to redo
/// the reading a resume exists to avoid. What it cannot know is what happened
/// while it was stopped, so that is what this says — and the two runs have
/// opposite answers.
///
/// Attended, somebody has been and gone, and the `## Status Log` says what
/// they did. Unattended, nobody has: the block was answered by the run
/// itself, which sent the lane straight back, and the obstacle is exactly
/// where it was left. Telling an unattended lane that a person has cleared
/// its path sends it looking through the log for a fix that is not there.
///
/// So the unattended half also carries the job a second, dedicated lane used
/// to be sent to do, said to the one session that does not have to
/// reconstruct anything to do it: reproduce the obstacle, clear the smallest
/// thing in the way, prove it is clear.
///
/// No report form and no `stage:` warning here any more — both are already
/// in [`system_prompt`], which a resumed lane was also launched with, so
/// repeating them a second time in the typed message is exactly the
/// duplication this task cuts. `pipeline` is unused for the same reason it
/// still appears: kept so the three prompt functions share one shape and a
/// caller never has to remember which needs it.
pub(crate) fn resume_prompt(
    repo: &Repo,
    task: &Task,
    _pipeline: &Pipeline,
    unattended: bool,
) -> String {
    let task_file = task.path.display().to_string();
    let (state, builtin) = match unattended {
        false => ("resume", RESUME),
        true => ("resume-unattended", RESUME_UNATTENDED),
    };
    crate::lane_prompts::render(repo, state, builtin, &[("task_file", &task_file)])
}

/// What a lane resumed by `session:` is told, instead of the opening
/// briefing — distinct from [`resume_prompt`], because nothing here stopped.
///
/// A blocked lane was unblocked *by a person*, and that is the fact
/// `resume_prompt` exists to say. A `session:` step's prompt simply comes
/// back for its next visit — a fix after a review, a second review after the
/// fix — with nobody in between and nothing to explain except what changed.
pub(crate) fn carry_prompt(repo: &Repo, task: &Task, _pipeline: &Pipeline) -> String {
    let task_file = task.path.display().to_string();
    crate::lane_prompts::render(repo, "carry", CARRY, &[("task_file", &task_file)])
}

/// What a lane resumed after a park is told, instead of the opening
/// briefing — distinct from both [`resume_prompt`] and [`carry_prompt`],
/// because it must say the one thing neither of those may: nothing here was
/// ever a blocker.
///
/// Two gestures land a task on `paused` with `parked_from` set rather than a
/// gate or a real block, and this prompt answers for both without knowing
/// which: the board's `p` key, and a person's own Escape typed straight into
/// the pane — see [`crate::dispatch::Dispatcher::park_after_interrupt`].
/// [`resume_prompt`]'s unattended half sends a lane hunting through `##
/// Blocker` for an obstacle; a park never wrote one, so this says the
/// opposite, plainly.
pub(crate) fn park_prompt(repo: &Repo, _task: &Task, _pipeline: &Pipeline) -> String {
    // `task` and `pipeline` are unused: unlike `resume_prompt` and
    // `carry_prompt`, nothing here points a lane back at its own task file
    // or names a report form any more, but they are kept so all three
    // prompt functions share one shape and a caller never has to remember
    // which needs which.
    crate::lane_prompts::render(repo, "park", PARK, &[])
}

/// The sixth typed message: what a lane that has gone quiet is nudged with,
/// naming the step it is on and repeating the report contract it was
/// launched with. Its own function rather than inlined at the one call
/// site, so `spoolway prompt contract` can render it too, against the same
/// wording a real nudge would use.
pub(crate) fn reminder_prompt(repo: &Repo, pipeline: &Pipeline, step: &Step) -> String {
    let contract = report_contract(pipeline, step.id == crate::pipeline::BLOCKED);
    crate::lane_prompts::render(
        repo,
        "reminder",
        REMINDER,
        &[("step", &step.id), ("report_contract", &contract)],
    )
}

/// Every one of the six typed messages, rendered for `state` against a real
/// or sample task — what `spoolway prompt contract`'s section 3 shows, one
/// state at a time. `state` outside [`crate::lane_prompts::STATES`] renders
/// empty rather than panicking: the contract's own loop is the only caller,
/// and it never asks for anything else.
pub(crate) fn lane_prompt_for_state(
    repo: &Repo,
    task: &Task,
    pipeline: &Pipeline,
    step: &Step,
    state: &str,
) -> String {
    match state {
        "opening" => opening_prompt(repo, task, pipeline, step),
        "resume" => resume_prompt(repo, task, pipeline, false),
        "resume-unattended" => resume_prompt(repo, task, pipeline, true),
        "carry" => carry_prompt(repo, task, pipeline),
        "park" => park_prompt(repo, task, pipeline),
        "reminder" => reminder_prompt(repo, pipeline, step),
        _ => String::new(),
    }
}
