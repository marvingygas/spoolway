//! `spoolway stack`: hand a task's change over with git and `gh`, no model and
//! no rebase.
//!
//! This is a command step's whole job — see docs/pipelines.md, "Command
//! steps" — so it needs no worker slot and no prompt, and its exit code is
//! already its outcome. The dependent's worktree already sits on its
//! dependency's branch (`Frontmatter::cut_from`, set when the worktree is
//! cut), so there is no rebase left to run here; that ancestry is the
//! previous task's whole reason to exist.
//!
//! The one seam a local test can reach into: every `gh` call runs the
//! program named by `SPOOLWAY_GH`, `gh` absent, so an end-to-end suite with no
//! real forge can point it at a stub script and still exercise everything
//! else — the commit, the squash, the push, the body and its trailer.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::*;

/// The pull request body's hard ceiling. GitHub's own API refuses a body past
/// this outright rather than truncating it, so `spoolway stack` has to.
const MAX_BODY: usize = 65_536;

/// `gh`'s program name. Overridable so a local end-to-end suite — which has
/// no forge to talk to — can point this at a stub and still drive everything
/// around it for real.
fn gh_program() -> String {
    std::env::var("SPOOLWAY_GH").unwrap_or_else(|_| "gh".to_string())
}

/// One labeled line of `spoolway stack`'s own report — `<label>` padded to a
/// fixed column so a run's whole report reads as a table, the way the plan's
/// own mockup draws it.
fn report_line(label: &str, value: impl std::fmt::Display) {
    println!("  {label:<12}{value}");
}

pub fn stack(repo: &Repo, args: &StackArgs) -> Result<()> {
    let id = args
        .task
        .clone()
        .or_else(|| std::env::var(TASK_ENV).ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no task given and ${TASK_ENV} is not set — pass the task id explicitly"
            )
        })?;
    let task = repo.task(&id)?;
    // `title:` is required at queue time (`queue_add::parse_submission`), but
    // an older task file can still carry a blank one — checked here rather
    // than trusted, since this is the one place that value becomes a commit
    // subject and, absent a summary, a pull request's title. It is written in
    // the Conventional Commits shape — `feat(queue): add a --dry-run flag` —
    // and used verbatim, so nothing here prefixes the task's id onto it.
    if task.front.title.trim().is_empty() {
        bail!("task `{id}` has no `title:` to make the squashed commit's subject from");
    }
    println!("{id}\n");

    // The worktree a command step runs in — see docs/pipelines.md — falls
    // back to the current directory so a person can run this by hand from
    // inside one.
    let worktree = match std::env::var("SPOOLWAY_WORKTREE") {
        Ok(path) => PathBuf::from(path),
        Err(_) => std::env::current_dir().context("resolving the current directory")?,
    };
    let step = std::env::var(crate::dispatch::ENV_STEP).unwrap_or_else(|_| "stack".to_string());
    // Empty, always: `SPOOLWAY_HEAD` is where a branch stood when an *agent*
    // lane's turn began, and a command step's env carries no such thing —
    // there is no session here for a HEAD to have moved during. Empty reads
    // to `auto_commit` as "I do not know", which sweeps unconditionally, and
    // that is the right call specifically at `handover`: every earlier step
    // already reported, and reporting already ran this same sweep against
    // *that* step's own real `SPOOLWAY_HEAD` — so anything still uncommitted
    // now is either ordinary residue or something an earlier lane's own
    // sweep deliberately left alone because that lane had committed its real
    // work. Left alone here too, it is not spared — it is lost the moment
    // this worktree is torn down at `done`. Handover is the last stop before
    // that, so sweeping it into the pull request is what keeps it at all.
    let started_at = std::env::var("SPOOLWAY_HEAD").unwrap_or_default();
    match auto_commit(repo, &worktree, &started_at, &id, &step) {
        Some(note) => report_line("commit", note),
        None => report_line("commit", "nothing uncommitted"),
    }

    let branch = match task.front.branch.clone() {
        Some(branch) => branch,
        None => crate::repo::branch_at(&worktree)?,
    };
    // The branch this pull request is opened against: what the worktree was
    // actually cut from — the dependency's branch under a stack, `base`
    // otherwise. `base` alone is kept only for a checkout old enough to
    // predate `cut_from`.
    let cut_from = task
        .front
        .cut_from
        .clone()
        .or_else(|| task.front.base.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "task `{id}` records neither `cut_from` nor `base` — nothing to open a pull \
                 request against"
            )
        })?;

    // A dependency's branch was published by its own handover, and this
    // checkout may predate that, so a fetch runs before anything looks at
    // what this branch sits on. Best-effort: a fetch that fails (no remote,
    // offline) is exactly what the diff below will fail loudly on if it
    // actually matters.
    let _ = crate::repo::run(&worktree, "git", &["fetch", "origin", &cut_from]);
    let cut_ref = remote_ref(&worktree, &cut_from);

    // The three-dot diff: what this branch changed since it diverged from
    // its cut point, ignoring anything the cut point picked up afterwards.
    // `cut_ref..HEAD` would report the cut point's own later commits as
    // deletions the moment it moved out from under this branch.
    let changed = crate::repo::run(
        &worktree,
        "git",
        &["diff", "--name-only", &format!("{cut_ref}...HEAD")],
    )
    .with_context(|| format!("diffing `{branch}` against `{cut_ref}`"))?;
    let changed_files: Vec<String> = changed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if changed_files.is_empty() {
        bail!(
            "`{branch}` has no changes against `{cut_from}` (three-dot diff is empty) — \
             nothing to open a pull request for"
        );
    }

    // Everything `[stack.summary]` could refuse over — a blank half, a
    // missing or empty template — is settled before a single bit of git
    // state changes: not just before the push, but before the squash below
    // too. Nothing in `run_summary` depends on the squash having happened
    // (the summary turn is given only the task file), so a broken setup
    // refuses a branch that still looks exactly as it did when this command
    // was invoked — nothing squashed, nothing pushed, nothing opened.
    let summary = run_summary(repo, &task)?;

    // Squash to one commit before anything downstream reasons about this
    // branch's shape: `git merge-tree` is only sound against a single commit,
    // and a two-commit branch can read clean at the tips while a real replay
    // conflicts partway through.
    let subject = task.front.title.trim().to_string();
    let merge_base = crate::repo::run(&worktree, "git", &["merge-base", &cut_ref, "HEAD"])
        .with_context(|| format!("finding the merge base with `{cut_ref}`"))?
        .trim()
        .to_string();
    let commit_count = crate::repo::run(
        &worktree,
        "git",
        &["rev-list", "--count", &format!("{merge_base}..HEAD")],
    )
    .map(|out| out.trim().to_string())
    .unwrap_or_default();
    crate::repo::run(&worktree, "git", &["reset", "--soft", &merge_base])
        .context("squashing the branch onto one commit")?;
    crate::repo::run(&worktree, "git", &["commit", "-q", "-m", &subject])
        .context("committing the squashed change")?;
    report_line("squash", format!("{commit_count} commits → 1   {subject}"));

    if let Err(err) = crate::repo::run(
        &worktree,
        "git",
        &[
            "push",
            "--force-with-lease",
            "origin",
            &format!("HEAD:{branch}"),
        ],
    ) {
        let msg = format!("{err:#}");
        // `--force-with-lease` fails two different ways, and only one of them
        // is what it exists to catch: `(stale info)` means the remote moved
        // since this branch was last fetched, which is worth saying plainly
        // rather than as a bare command failure.
        if msg.contains("stale info") {
            report_line("push", format!("! [rejected] {branch} (stale info)"));
            bail!(
                "push refused: the remote moved since this branch was last fetched — fetch and \
                 look at what arrived before trying again ({msg})"
            );
        }
        report_line("push", "! [rejected]");
        bail!("`git push --force-with-lease` failed: {msg}");
    }
    report_line("push", format!("{branch} → origin      --force-with-lease"));

    let gaps = touches_gap(&task.front.touches, &changed_files);
    let conflicts = parallel_conflicts(repo, &worktree, &id);
    report_line(
        "touches",
        match gaps.is_empty() {
            true => format!("ok, {} file(s), all declared", changed_files.len()),
            false => format!("{} file(s) not declared: {}", gaps.len(), gaps.join(", ")),
        },
    );
    report_line(
        "siblings",
        match conflicts.is_empty() {
            true => "none open".to_string(),
            false => format!("conflicts with {}", conflicts.join(", ")),
        },
    );

    let title = summary
        .as_ref()
        .map(|(title, _)| title.clone())
        .unwrap_or_else(|| subject.clone());
    let body = compose_body(
        &task.body,
        summary.as_ref().map(|(_, rest)| rest.as_str()),
        &gaps,
        &conflicts,
    );

    let (own_number, url) = open_or_reuse_pr(&worktree, &branch, &cut_from, &title, &body)?;
    report_line("pull req", format!("#{own_number} — {url}"));
    let stacked = register_stack(&worktree, &id, &task.front.depends_on, own_number)?;
    report_line("stack", stacked);

    println!("\ndone. exit 0");
    Ok(())
}

/// `origin/<branch>` when it actually exists there, the bare local branch
/// name otherwise — used both for the diff's cut point and for a candidate
/// stack-mate's branch, neither of which this process is guaranteed to have
/// fetched under a name that resolves any other way.
fn remote_ref(worktree: &Path, branch: &str) -> String {
    let remote = format!("origin/{branch}");
    match crate::repo::run(
        worktree,
        "git",
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/remotes/{remote}"),
        ],
    ) {
        Ok(_) => remote,
        Err(_) => branch.to_string(),
    }
}

/// Files this branch changed that no glob in `touches` reaches — informational
/// only, named in the trailer, never a reason to refuse. See
/// [`crate::globs::overlaps`] for what "reaches" means.
fn touches_gap(touches: &[String], changed: &[String]) -> Vec<String> {
    changed
        .iter()
        .filter(|file| {
            !touches
                .iter()
                .any(|glob| crate::globs::overlaps(glob, file))
        })
        .cloned()
        .collect()
}

/// Every open task, marked `parallel: true`, whose branch `git merge-tree
/// --write-tree` predicts a real conflict with this one.
///
/// Best-effort throughout: a task with no branch yet, a git old enough to
/// lack `merge-tree --write-tree`, or a queue this process cannot read all
/// fall through to "nothing found" rather than failing the command — this is
/// the trailer's business, not a reason to refuse the pull request.
fn parallel_conflicts(repo: &Repo, worktree: &Path, id: &str) -> Vec<String> {
    let Ok(tasks) = repo.tasks() else {
        return Vec::new();
    };
    let mut conflicts = Vec::new();
    for other in tasks {
        if other.id() == id || !other.front.parallel {
            continue;
        }
        let Some(branch) = other.front.branch.clone() else {
            continue;
        };
        let candidate = remote_ref(worktree, &branch);
        // `git merge-tree --write-tree` exits 1 both for a real conflict and
        // for a ref it cannot even resolve (verified on git 2.53.0: a
        // missing ref prints "not something we can merge" and still exits
        // 1) — so the exit code alone cannot tell the two apart. A branch
        // written at queue time (`branch:`) but never actually cut yet is
        // exactly that missing-ref case, so resolve the candidate first and
        // skip it — silently, this is the trailer's business, not a reason
        // to refuse the pull request — rather than trust the exit code to
        // say why it failed.
        let resolves = crate::repo::run(
            worktree,
            "git",
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("{candidate}^{{commit}}"),
            ],
        )
        .is_ok();
        if !resolves {
            continue;
        }
        let Ok(output) = Command::new("git")
            .args(["merge-tree", "--write-tree", "HEAD", &candidate])
            .current_dir(worktree)
            .output()
        else {
            continue;
        };
        if output.status.code() == Some(1) {
            conflicts.push(other.id().to_string());
        }
    }
    conflicts
}

/// The pull request's body, with the trailer below it, cut to fit GitHub's
/// body limit.
///
/// `summary_text` is `Some` exactly when `[stack.summary]` ran a model turn —
/// see [`run_summary`] — and when it is, it *is* the body: the task file's
/// own text does not also appear, so one change is not described twice on
/// one page. `None` is task-file mode, unchanged from before this turn
/// existed: the body is everything after the task file's frontmatter fence,
/// verbatim.
fn compose_body(
    task_body: &str,
    summary_text: Option<&str>,
    gaps: &[String],
    conflicts: &[String],
) -> String {
    let mut body = String::new();
    match summary_text {
        Some(summary) => {
            body.push_str(summary.trim());
            body.push('\n');
        }
        None => {
            body.push_str(task_body.trim_end());
            body.push('\n');
        }
    }

    // The trailer's gap and conflict lists are unbounded — a branch touching
    // a few dozen undeclared files makes a trailer no fixed guess would
    // budget for — so the cut below is sized against the trailer this run
    // would actually carry, worst case first: `truncated: true` is always
    // the larger of the two shapes trailer() can print, so budgeting against
    // it is never an underestimate whichever way the truncation check comes
    // out below.
    let worst_case_trailer = trailer(gaps, conflicts, true);
    let budget = MAX_BODY.saturating_sub(worst_case_trailer.len() + 2);
    let truncated = body.len() > budget;
    if truncated {
        // Cut at the last section boundary — a line opening `## ` — that
        // still fits, so a reader never lands mid-heading. A body with no
        // heading at all inside the budget falls back to a hard cut.
        let candidate = char_boundary_floor(&body, budget);
        let window = &body[..candidate];
        let cut = window.rfind("\n## ").unwrap_or(candidate);
        body.truncate(cut);
        body.push('\n');
    }

    body.push('\n');
    body.push_str(&if truncated {
        worst_case_trailer
    } else {
        trailer(gaps, conflicts, false)
    });
    body
}

/// The largest byte offset at or before `at` that lands on a UTF-8 character
/// boundary, so a cut through a multi-byte character never panics.
fn char_boundary_floor(s: &str, at: usize) -> usize {
    let mut at = at.min(s.len());
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The rule that opens the trailer — a plain dashed line, the way a footer is
/// set off from the letter above it.
const TRAILER_RULE: &str = "─────────────────────────────────────────────────────────";

/// The tag every pull request closes with.
///
/// No address beside the name, deliberately. A trailer here names no email —
/// not the person who ran the task, and not one for the model either.
const CO_AUTHOR: &str = "Co-Authored-By: Claude Code";

/// The pull request's trailer: what the branch touched outside `touches`,
/// which open `parallel: true` task it is predicted to conflict with, and the
/// co-authorship tag under both.
///
/// Only the tag is always there. The other two appear when there is something
/// to say, and neither is a reason to refuse — both are what a reviewer would
/// otherwise find out when they came to land the stack.
fn trailer(gaps: &[String], conflicts: &[String], truncated: bool) -> String {
    let mut out = String::new();
    out.push_str(TRAILER_RULE);
    out.push('\n');

    if truncated {
        out.push_str("The body above was cut short to fit GitHub's 65,536-character limit.\n\n");
    }

    if !gaps.is_empty() {
        out.push_str(&format!(
            "Changed {} file{} it did not declare in `touches`:\n",
            gaps.len(),
            if gaps.len() == 1 { "" } else { "s" }
        ));
        for file in gaps {
            out.push_str(&format!("  {file}\n"));
        }
        out.push('\n');
    }

    for conflict in conflicts {
        out.push_str(&format!(
            "Will conflict with `{conflict}`, which is not ordered against this task.\n\n"
        ));
    }

    out.push_str(CO_AUTHOR);
    out.push('\n');
    out
}

/// Run the configured `[stack.summary]` prompt on the task file for one
/// turn, and read back its title line and the text below it. `Ok(None)` when
/// `agent` and `model` are both blank — the ordinary, unconfigured case, and
/// not a failure. Those two blanks are the only thing that decides it, the
/// same shape `pipeline_gen.pipeline_model` already uses for its own
/// refusal — and exactly one of them set is refused outright, naming
/// whichever is blank, rather than silently falling back to task-file mode
/// or guessing at the other half.
fn run_summary(repo: &Repo, task: &Task) -> Result<Option<(String, String)>> {
    let summary = &repo.config.stack.summary;
    let agent_blank = summary.agent.trim().is_empty();
    let model_blank = summary.model.trim().is_empty();
    if agent_blank && model_blank {
        return Ok(None);
    }
    if agent_blank {
        bail!(
            "`stack.summary.agent` is blank while `stack.summary.model` is set — set both or \
             neither"
        );
    }
    if model_blank {
        bail!(
            "`stack.summary.model` is blank while `stack.summary.agent` is set — set both or \
             neither"
        );
    }

    // The template is this turn's whole shape, read here and handed to the
    // model as text rather than as a path — a missing template used to
    // produce an improvised body and no error, because nothing in the
    // binary ever opened it; the model's own file access was the only thing
    // standing between a broken setup and silence.
    let template_path = repo.pull_request_template_path();
    let template = std::fs::read_to_string(&template_path)
        .ok()
        .filter(|text| !text.trim().is_empty());
    let Some(template) = template else {
        bail!(
            "`[stack.summary]` names a summary model, and there is no {} to fill in.\n\n       \
             spoolway update --replace {}",
            crate::config::PULL_REQUEST_TEMPLATE,
            crate::config::PULL_REQUEST_TEMPLATE,
        );
    };

    let profile = repo.config.agent(&summary.agent)?;
    let adapter = crate::agent::adapter(&profile.kind)
        .ok_or_else(|| anyhow::anyhow!("spoolway does not know agent kind `{}`", profile.kind))?;
    let prompt_path = crate::prompt::path_for(repo, &summary.prompt);
    if !prompt_path.is_file() {
        bail!(
            "`stack.summary.prompt` names `{}`, and there is no {} — run `spoolway init` or \
             `spoolway update --replace` to restore it",
            summary.prompt,
            prompt_path.display()
        );
    }

    let session = crate::usage::new_session_id();
    let state_dir = repo.root.join(crate::config::STATE_DIR);
    let values = std::collections::BTreeMap::from([
        ("model", summary.model.clone()),
        ("session_id", session.clone()),
        ("prompt_file", prompt_path.display().to_string()),
        ("task_file", task.path.display().to_string()),
        ("worktree", repo.root.display().to_string()),
        ("repo", repo.root.display().to_string()),
        ("state_dir", state_dir.display().to_string()),
    ]);
    let mut rendered = profile.render_args(&values)?;
    let effort = (!summary.effort.trim().is_empty()).then_some(summary.effort.as_str());
    rendered.extend(profile.effort_args(effort));
    let argv = adapter
        .headless_args(&rendered, false)
        .context("this agent kind has no headless row to run one turn with")?;
    crate::agent::prepare_session_home(&profile.kind, &session);
    let env = adapter.session_env(&session);

    let message = format!(
        "Read the task file and write the pull request's whole body, exactly as your \
         instructions say. Here is the template to fill in, comment included:\n\n{template}"
    );
    let output = Command::new(adapter.program())
        .args(&argv)
        .arg(&message)
        .current_dir(&repo.root)
        .envs(env)
        .output()
        .with_context(|| format!("running the summary turn (`{}`)", profile.kind))?;
    if !output.status.success() {
        bail!(
            "the summary turn failed: {}",
            first_line(&String::from_utf8_lossy(&output.stderr))
        );
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let mut lines = text.splitn(2, '\n');
    let title = lines.next().unwrap_or_default().trim().to_string();
    let rest = lines.next().unwrap_or_default().trim().to_string();
    if title.is_empty() {
        bail!("the summary turn printed nothing to use as a title");
    }
    Ok(Some((title, rest)))
}

/// The pull request already open on `branch`, or a freshly opened one.
///
/// `gh pr view` first, always: a branch may already have one — reused rather
/// than refused, since a second pull request for one branch is not a thing
/// `gh pr create` allows anyway.
fn open_or_reuse_pr(
    worktree: &Path,
    branch: &str,
    base: &str,
    title: &str,
    body: &str,
) -> Result<(u64, String)> {
    let gh = gh_program();
    if let Some(existing) = pr_view(&gh, worktree, branch)?.filter(Pr::is_open) {
        println!("reusing the pull request already open on `{branch}`");
        return Ok((existing.number, existing.url));
    }

    let body_path = write_temp_body(body)?;
    let create = Command::new(&gh)
        .args([
            "pr", "create", "--base", base, "--head", branch, "--title", title,
        ])
        .arg("--body-file")
        .arg(&body_path)
        .current_dir(worktree)
        .output()
        .with_context(|| format!("running `{gh} pr create`"))?;
    let _ = std::fs::remove_file(&body_path);
    if !create.status.success() {
        bail!(
            "`gh pr create` failed: {}",
            first_line(&String::from_utf8_lossy(&create.stderr))
        );
    }

    let opened = pr_view(&gh, worktree, branch)?.ok_or_else(|| {
        anyhow::anyhow!("opened the pull request but `gh pr view` cannot find it")
    })?;
    Ok((opened.number, opened.url))
}

/// One pull request, as `gh pr view` reports it.
struct Pr {
    number: u64,
    url: String,
    /// `OPEN`, `MERGED` or `CLOSED`. A branch keeps its pull request after it
    /// lands, so the state is the only thing separating a live stack-mate
    /// from one that has already been merged out from under this branch.
    state: String,
}

impl Pr {
    fn is_open(&self) -> bool {
        self.state == "OPEN"
    }
}

/// `gh pr view <branch>`'s pull request, whatever state it is in, or `None`
/// when that branch has none at all — the one outcome that is not a failure
/// here. Callers that need a live pull request check `is_open` themselves.
fn pr_view(gh: &str, worktree: &Path, branch: &str) -> Result<Option<Pr>> {
    let view = Command::new(gh)
        .args(["pr", "view", branch, "--json", "number,url,state"])
        .current_dir(worktree)
        .output()
        .with_context(|| format!("running `{gh} pr view {branch}`"))?;
    if !view.status.success() {
        return Ok(None);
    }
    parse_pr_view(&view.stdout)
}

/// The parsing half of `pr_view`, split out so it can be tested without `gh`.
fn parse_pr_view(stdout: &[u8]) -> Result<Option<Pr>> {
    let json: serde_json::Value =
        serde_json::from_slice(stdout).context("parsing `gh pr view` output")?;
    let number = json
        .get("number")
        .and_then(serde_json::Value::as_u64)
        .context("`gh pr view` printed no `number`")?;
    let url = json
        .get("url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let state = json
        .get("state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(Some(Pr { number, url, state }))
}

fn write_temp_body(body: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("spoolway-stack-body-{}.md", std::process::id()));
    std::fs::write(&path, body).context("writing the pull request body to a temp file")?;
    Ok(path)
}

/// Register this task's pull request in the GitHub stack: read both pull
/// requests' `.stack`, then join, create, or do nothing — no judgement
/// call in it, just bookkeeping. Returns what happened, for the `stack`
/// line of the run's own report.
fn register_stack(
    worktree: &Path,
    id: &str,
    depends_on: &[String],
    own_number: u64,
) -> Result<String> {
    // The foot of the stack makes no stack of its own — the task above it
    // does, from the two of them together.
    let Some(dep_id) = depends_on.first() else {
        return Ok("none — foot of stack".to_string());
    };
    let gh = gh_program();
    let (owner, repo_name) = owner_repo(worktree)?;

    let dep_branch = format!("task/{dep_id}");
    let dep = pr_view(&gh, worktree, &dep_branch)?.ok_or_else(|| {
        anyhow::anyhow!("dependency `{dep_id}` has no pull request on `{dep_branch}` to stack onto")
    })?;
    // A stack joins two pull requests that are both still open. Once the
    // dependency lands, its change is in the base branch and this one stands
    // on its own — asking GitHub to stack onto it is refused, and rightly so.
    if !dep.is_open() {
        return Ok(format!(
            "none — dependency `{dep_id}`'s pull request #{} is {}",
            dep.number,
            dep.state.to_lowercase()
        ));
    }
    let dep_number = dep.number;

    if pr_stack_number(&gh, worktree, &owner, &repo_name, own_number)?.is_some() {
        return Ok(format!("`{id}`'s pull request is already in a stack"));
    }

    match pr_stack_number(&gh, worktree, &owner, &repo_name, dep_number)? {
        Some(stack_number) => {
            // A GitHub stack is one linear chain: each pull request's base has
            // to be the previous one's head. Two tasks depending on the same
            // task are siblings, and only one of them can hold the slot above
            // it — `gh api .../stacks/{n}/add` refuses the second with HTTP
            // 422 rather than forking the chain. The other sibling is not
            // broken and its pull request is not wrong; there is simply no
            // position for it, so this is said plainly instead of failing
            // `handover` over a shape GitHub cannot represent.
            let top = stack_top_head(&gh, worktree, &owner, &repo_name, stack_number)?;
            if let Some((top_number, top_ref)) = &top
                && top_ref != &dep_branch
            {
                return Ok(format!(
                    "none — `{id}` is a sibling of #{top_number} on #{dep_number}, outside \
                     stack #{stack_number}"
                ));
            }
            gh_api_post(
                &gh,
                worktree,
                &format!("repos/{owner}/{repo_name}/stacks/{stack_number}/add"),
                &format!(r#"{{"pull_requests":[{own_number}]}}"#),
            )
            .context("adding to the GitHub stack")?;
            Ok(format!("#{stack_number} — added #{own_number} to it"))
        }
        None => {
            gh_api_post(
                &gh,
                worktree,
                &format!("repos/{owner}/{repo_name}/stacks"),
                &format!(r#"{{"pull_requests":[{dep_number},{own_number}]}}"#),
            )
            .context("creating the GitHub stack")?;
            Ok(format!("created, bottom #{dep_number}, top #{own_number}"))
        }
    }
}

/// `gh api <path> -X POST --input -`, with `body` sent as real JSON through
/// stdin — `-f` sends strings, and the stack endpoints refuse those where an
/// integer is required.
fn gh_api_post(gh: &str, worktree: &Path, path: &str, body: &str) -> Result<()> {
    let mut child = Command::new(gh)
        .args(["api", path, "-X", "POST", "--input", "-"])
        .current_dir(worktree)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running `{gh} api {path}`"))?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(body.as_bytes())
        .with_context(|| format!("writing to `{gh} api {path}`"))?;
    let output = child
        .wait_with_output()
        .with_context(|| format!("waiting for `{gh} api {path}`"))?;
    if !output.status.success() {
        bail!("{}", first_line(&String::from_utf8_lossy(&output.stderr)));
    }
    Ok(())
}

/// The `.stack.number` a pull request already belongs to, or `None` when it
/// is in no stack — including when `gh api` itself refuses, since a pull
/// request that cannot be asked is treated the same as one with no stack
/// rather than failing the whole command over a read.
fn pr_stack_number(
    gh: &str,
    worktree: &Path,
    owner: &str,
    repo_name: &str,
    number: u64,
) -> Result<Option<u64>> {
    let out = Command::new(gh)
        .args([
            "api",
            &format!("repos/{owner}/{repo_name}/pulls/{number}"),
            "-q",
            ".stack.number",
        ])
        .current_dir(worktree)
        .output()
        .with_context(|| "reading a pull request's stack")?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok())
}

/// The pull request currently on top of a stack — its number and its head
/// ref — or `None` when the read fails, in the same "cannot be asked reads as
/// unknown" spirit as `pr_stack_number`. `register_stack` uses this to tell a
/// sibling task from the next real link: if the top's head ref is not the
/// dependency's own branch, something else already holds the slot above it.
fn stack_top_head(
    gh: &str,
    worktree: &Path,
    owner: &str,
    repo_name: &str,
    stack_number: u64,
) -> Result<Option<(u64, String)>> {
    let out = Command::new(gh)
        .args([
            "api",
            &format!("repos/{owner}/{repo_name}/stacks/{stack_number}"),
            "-q",
            ".pull_requests[-1] | .number, .head.ref",
        ])
        .current_dir(worktree)
        .output()
        .with_context(|| "reading a stack's own top")?;
    if !out.status.success() {
        return Ok(None);
    }
    let mut lines = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>()
        .into_iter();
    let number = lines
        .next()
        .and_then(|line| line.trim().parse::<u64>().ok());
    let head_ref = lines.next().map(|line| line.trim().to_string());
    Ok(number.zip(head_ref))
}

fn owner_repo(worktree: &Path) -> Result<(String, String)> {
    // `git config`, not `git remote get-url`: the latter expands
    // `url.<base>.insteadOf` rewrites, which name what a push or fetch
    // actually reaches rather than what the remote is *of* — a project
    // routing its pushes through a mirror would have this read the mirror's
    // host instead of GitHub's.
    let url = crate::repo::run(worktree, "git", &["config", "--get", "remote.origin.url"])
        .context("reading the `origin` remote")?;
    parse_owner_repo(url.trim()).ok_or_else(|| {
        anyhow::anyhow!(
            "`origin` ({}) is not a github.com remote `gh` can register a stack against",
            url.trim()
        )
    })
}

/// `owner/repo` out of an `origin` remote, whether it is spelled
/// `git@github.com:owner/repo.git` or `https://github.com/owner/repo`.
fn parse_owner_repo(url: &str) -> Option<(String, String)> {
    let stripped = url.trim().trim_end_matches(".git");
    let (_, after) = stripped.split_once("github.com")?;
    let after = after.trim_start_matches([':', '/']);
    let (owner, repo) = after.split_once('/')?;
    Some((owner.to_string(), repo.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A branch keeps its pull request after it lands, so `gh pr view` still
    /// answers for one that is merged. Reading the state is what separates a
    /// live stack-mate from a dependency that has already gone in.
    #[test]
    fn a_merged_pull_request_is_read_back_as_not_open() {
        let merged =
            parse_pr_view(br#"{"number":92,"url":"https://example.invalid/92","state":"MERGED"}"#)
                .unwrap()
                .expect("gh answered, so there is a pull request");
        assert_eq!(merged.number, 92);
        assert!(!merged.is_open());

        let open =
            parse_pr_view(br#"{"number":104,"url":"https://example.invalid/104","state":"OPEN"}"#)
                .unwrap()
                .expect("gh answered, so there is a pull request");
        assert!(open.is_open());
    }

    /// A pull request `gh` reports without a state is not treated as open —
    /// stacking onto something unreadable is the failure this guards.
    #[test]
    fn a_pull_request_with_no_state_is_not_open() {
        let pr = parse_pr_view(br#"{"number":7,"url":"https://example.invalid/7"}"#)
            .unwrap()
            .expect("gh answered, so there is a pull request");
        assert!(!pr.is_open());
    }

    #[test]
    fn owner_repo_reads_both_url_shapes() {
        assert_eq!(
            parse_owner_repo("git@github.com:marvingygas/spoolway.git"),
            Some(("marvingygas".to_string(), "spoolway".to_string()))
        );
        assert_eq!(
            parse_owner_repo("https://github.com/marvingygas/spoolway"),
            Some(("marvingygas".to_string(), "spoolway".to_string()))
        );
        assert_eq!(parse_owner_repo("https://gitlab.com/a/b"), None);
    }

    /// A `parallel: true` task whose `branch:` was written at queue time but
    /// never actually cut is not a conflict — it is a ref `git merge-tree`
    /// cannot even resolve, and that failure exits 1 the same way a real
    /// conflict does, so `parallel_conflicts` must not trust the exit code
    /// alone.
    #[test]
    fn a_queued_task_with_no_branch_yet_is_not_a_predicted_conflict() {
        let repo = crate::commands::testutil::fixture("parallel-conflicts-no-ref");
        std::fs::write(repo.root.join("file.txt"), "one\n").unwrap();
        crate::repo::run(&repo.root, "git", &["add", "-A"]).unwrap();
        crate::repo::run(&repo.root, "git", &["commit", "-q", "-m", "seed"]).unwrap();

        let doc = "---\nid: other\ntitle: other, done\ngroup: demo\nparallel: true\n\
                   branch: task/other\n---\n## Goal\n\nDo the thing.\n";
        let path = repo.root.join(".other-doc.md");
        std::fs::write(&path, doc).unwrap();
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &QueueAddArgs {
                from: vec![path.display().to_string()],
            },
            &repo.root,
            false,
        )
        .unwrap();

        let conflicts = parallel_conflicts(&repo, &repo.root, "self");
        assert!(
            conflicts.is_empty(),
            "an unresolvable branch must not be reported as a conflict: {conflicts:?}"
        );
    }

    #[test]
    fn touches_gap_names_only_what_no_glob_reaches() {
        let touches = vec!["src/commands/stack.rs".to_string(), "docs/**".to_string()];
        let changed = vec![
            "src/commands/stack.rs".to_string(),
            "docs/pipelines.md".to_string(),
            "src/cli.rs".to_string(),
        ];
        assert_eq!(
            touches_gap(&touches, &changed),
            vec!["src/cli.rs".to_string()]
        );
    }

    /// Nothing to report leaves the tag standing on its own, and no trace of
    /// who ran the task: an address in a trailer is what this shape dropped.
    #[test]
    fn compose_body_is_verbatim_below_the_summary_with_a_trailer() {
        let body = compose_body("## Goal\n\nDo the thing.\n", None, &[], &[]);
        assert!(body.starts_with("## Goal\n\nDo the thing.\n"));
        assert!(body.trim_end().ends_with("Co-Authored-By: Claude Code"));
        assert!(!body.contains("did not declare"));
        assert!(!body.contains("Will conflict"));
        assert!(!body.contains('@'), "a trailer names no address");
    }

    /// With a model turn's output to work from, that output *is* the body —
    /// the task file's own text does not also appear below it, so one
    /// change is not described twice on one page.
    #[test]
    fn compose_body_is_the_model_output_alone_in_model_mode() {
        let body = compose_body(
            "## Goal\n\nDo the thing.\n",
            Some("## Why\n\nA one-line reason."),
            &["untouched.rs".to_string()],
            &["other-task".to_string()],
        );
        assert!(body.starts_with("## Why\n\nA one-line reason.\n"));
        assert!(!body.contains("## Goal\n\nDo the thing."));
        assert!(body.contains("Changed 1 file it did not declare in `touches`:\n  untouched.rs"));
        assert!(
            body.contains(
                "Will conflict with `other-task`, which is not ordered against this task."
            )
        );
    }

    /// The order the trailer reads in: what the branch did that the task file
    /// did not say it would, and the tag last.
    #[test]
    fn the_touches_gap_comes_before_the_co_author_tag() {
        let body = compose_body("## Goal\n", None, &["untouched.rs".to_string()], &[]);
        let gap = body.find("did not declare").expect("the gap is reported");
        let tag = body.find(CO_AUTHOR).expect("the tag is there");
        assert!(gap < tag, "the gap belongs above the tag:\n{body}");
    }

    /// Over the limit, the cut lands on a section boundary and the trailer
    /// says so — never a hard cut mid-sentence when a heading was available
    /// to cut at instead.
    #[test]
    fn compose_body_cuts_at_a_section_boundary_past_the_limit() {
        let mut long_body = String::new();
        for i in 0..2000 {
            long_body.push_str(&format!(
                "## Section {i}\n\nSome text about section {i}.\n\n"
            ));
        }
        let body = compose_body(&long_body, None, &[], &[]);
        assert!(body.len() <= MAX_BODY);
        assert!(body.contains("cut short to fit GitHub's 65,536-character limit"));
        // The cut happened right at a section boundary: what is left ends on
        // a blank line right after a paragraph, never mid-heading or
        // mid-sentence.
        let before_trailer = body.split(TRAILER_RULE).next().unwrap();
        assert!(before_trailer.trim_end().ends_with('.'));
    }

    /// A trailer's own lists are unbounded — one line per undeclared file,
    /// one paragraph per predicted conflict — so a fixed reservation for it
    /// can undercount and push the composed body past `MAX_BODY` on exactly
    /// the run that fixed budget existed to protect. A few dozen of each is
    /// nowhere near a realistic branch, and still has to fit.
    #[test]
    fn compose_body_stays_under_the_limit_with_a_large_trailer() {
        let gaps: Vec<String> = (0..80)
            .map(|i| format!("src/generated/file-{i}.rs"))
            .collect();
        let conflicts: Vec<String> = (0..40).map(|i| format!("sibling-task-{i}")).collect();
        let mut long_body = String::new();
        for i in 0..2000 {
            long_body.push_str(&format!(
                "## Section {i}\n\nSome text about section {i}.\n\n"
            ));
        }
        let body = compose_body(&long_body, None, &gaps, &conflicts);
        assert!(body.len() <= MAX_BODY, "{} bytes", body.len());
        // The whole trailer survived intact — cutting the body is what paid
        // for it, not dropping any of the trailer's own lines.
        assert!(body.contains("src/generated/file-79.rs"));
        assert!(body.contains("Will conflict with `sibling-task-39`"));
    }
}
