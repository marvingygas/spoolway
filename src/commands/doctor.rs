//! `spoolway doctor`: every check spoolway can run against a project, and
//! what each one found.
//!
//! Every finding here — not only the checks that read `config.toml` directly
//! — answers from `repo.checkout`'s own copy, not `repo.root`'s: `doctor`
//! loads its own [`Config`] from the checkout rather than reading
//! `repo.config` (loaded from `repo.root` back in `Repo::discover`), because
//! a task validating its own branch wants an answer about the file it
//! actually edited. `repo.root`'s copy still matters — it is what the real
//! dispatcher runs on — so its own parse failure, when there is one, is
//! reported as a finding of its own rather than silently dropped. See
//! [`doctor`].

use super::*;
use serde::Serialize;

/// One line of the report. Held rather than printed, so that a single run of
/// the checks can be rendered as the full listing or as only its exceptions.
///
/// `Serialize` is what `--json` reads back out: a `kind` tag plus each
/// variant's own fields, so a consumer like the `spoolway-doctor` skill can
/// match on `kind` instead of pattern-matching the plain-text prefixes
/// [`Report::render`] prints.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Row {
    /// A check that passed, and what it found.
    Ok { label: String, note: Option<String> },
    /// Something worth saying that is not a check: never counted, never a
    /// failure. `verbose_only` marks the ones that describe the machine this
    /// ran on rather than the project, which the short report leaves out.
    Note { text: String, verbose_only: bool },
    /// A check that failed, and why.
    Fail { label: String, why: String },
}

/// Everything a `doctor` run found, in the order the checks are declared.
#[derive(Debug, Default)]
struct Report {
    rows: Vec<Row>,
}

impl Report {
    /// Record one check's outcome.
    fn check(&mut self, label: &str, outcome: Result<Option<String>>) {
        self.rows.push(match outcome {
            Ok(note) => Row::Ok {
                label: label.to_string(),
                note,
            },
            Err(err) => Row::Fail {
                label: label.to_string(),
                why: format!("{err:#}"),
            },
        });
    }

    /// Record a note, which both modes print.
    fn note(&mut self, text: impl Into<String>) {
        self.rows.push(Row::Note {
            text: text.into(),
            verbose_only: false,
        });
    }

    /// Record a note only the full listing prints.
    fn note_verbose(&mut self, text: impl Into<String>) {
        self.rows.push(Row::Note {
            text: text.into(),
            verbose_only: true,
        });
    }

    /// How many checks failed.
    fn problems(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row, Row::Fail { .. }))
            .count()
    }

    /// How many checks ran at all. Notes are not checks and are not counted.
    fn checks(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| !matches!(row, Row::Note { .. }))
            .count()
    }

    /// The whole report as one block of text.
    ///
    /// The short form keeps the exceptions — the failures and the notes — and
    /// collapses every passing row into the closing count, because a screen of
    /// rows that all say `ok` is a screen nobody reads. `verbose` brings the
    /// listing back, in the same order and the same wording.
    fn render(&self, verbose: bool) -> String {
        let mut out = String::new();
        for row in &self.rows {
            match row {
                Row::Ok { label, note } if verbose => match note {
                    Some(note) => out.push_str(&format!("  ok    {label} — {note}\n")),
                    None => out.push_str(&format!("  ok    {label}\n")),
                },
                Row::Ok { .. } => {}
                Row::Note {
                    verbose_only: true, ..
                } if !verbose => {}
                Row::Note { text, .. } => out.push_str(&format!("  note  {text}\n")),
                Row::Fail { label, why } => out.push_str(&format!("  FAIL  {label}: {why}\n")),
            }
        }

        // The blank line separates the listing from the count, so there is
        // nothing to separate when nothing above it printed.
        if !out.is_empty() {
            out.push('\n');
        }
        let checks = self.checks();
        let problems = self.problems();
        if problems == 0 {
            out.push_str(&format!("{checks} checks passed. Everything checks out.\n"));
        } else {
            out.push_str(&format!(
                "{} of {checks} checks passed.\n",
                checks - problems
            ));
        }
        out
    }

    /// The same report `render` prints, structured instead of prosed —
    /// `--json`'s own reading of it. `verbose` narrows the rows the same way
    /// it narrows the text form: a passing `Row::Ok` and a `verbose_only`
    /// note are left out of the short report entirely, rather than emitted
    /// and left for the reader to filter.
    fn render_json(&self, verbose: bool) -> serde_json::Value {
        let rows: Vec<&Row> = self
            .rows
            .iter()
            .filter(|row| match row {
                Row::Ok { .. } => verbose,
                Row::Note { verbose_only, .. } => verbose || !verbose_only,
                Row::Fail { .. } => true,
            })
            .collect();
        serde_json::json!({
            "checks": self.checks(),
            "problems": self.problems(),
            "rows": rows,
        })
    }
}

/// One outcome a `check_*` helper found, in the order it belongs in the
/// report. Not `Row` itself: a single helper often wants to emit a check
/// alongside one or more notes — the model and agent checks below all do —
/// and `Finding` is what lets `doctor()` fold a whole `Vec` of those into the
/// report with one loop, instead of every helper taking `&mut Report` and
/// every call site losing track of what order things run in.
#[derive(Debug)]
enum Finding {
    /// Becomes a `Row::Ok` or `Row::Fail`, via [`Report::check`].
    Check(String, Result<Option<String>>),
    /// Becomes a `Row::Note` that both report modes print.
    Note(String),
    /// Becomes a `Row::Note` that only the full listing prints.
    NoteVerbose(String),
}

impl Report {
    /// Record one [`Finding`], however it turns out to be shaped.
    fn record(&mut self, finding: Finding) {
        match finding {
            Finding::Check(label, outcome) => self.check(&label, outcome),
            Finding::Note(text) => self.note(text),
            Finding::NoteVerbose(text) => self.note_verbose(text),
        }
    }

    /// Record a whole `Vec` of findings, in the order a `check_*` helper
    /// produced them.
    fn record_all(&mut self, findings: Vec<Finding>) {
        for finding in findings {
            self.record(finding);
        }
    }
}

/// Print a finished report in the caller's chosen mode, and exit non-zero
/// when it carries a problem — the tail every `doctor` path shares.
fn finish(report: &Report, verbose: bool, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report.render_json(verbose))?
        );
    } else {
        print!("{}", report.render(verbose));
    }
    let problems = report.problems();
    if problems == 0 {
        Ok(())
    } else {
        bail!("{problems} problem(s) — fix these before dispatching")
    }
}

/// Check that everything the configured pipeline needs is actually present,
/// before a dispatch pass discovers it the hard way.
///
/// `config_error` is why `repo.root`'s `config.toml` could not be read, when
/// it could not — the lenient discovery that built `repo` already folded that
/// into `repo.config` falling back to defaults. It no longer gates whether
/// the rest of this can run: every finding below is against a fresh load of
/// `repo.checkout`'s own copy instead, so a checkout whose file parses is
/// checked in full even when the project's does not. That load is what does
/// the gating now — see the early return below — and `config_error`, when
/// `Some`, becomes a finding of its own instead.
///
/// `pipelines` is handed in as a `Result` for the same reason: a pipeline
/// file that will not parse is what this command exists to name, so its
/// load failure is a finding, not a gate — see the second early return.
pub fn doctor(
    repo: &Repo,
    pipelines: Result<Pipelines>,
    config_error: Option<anyhow::Error>,
    verbose: bool,
    json: bool,
    no_live: bool,
) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }
    let config = match Config::load(&repo.checkout) {
        Ok(config) => config,
        Err(err) => return doctor_unconfigured(repo, pipelines, err, verbose, json),
    };

    // Built once and shared by every check below that asks the multiplexer
    // anything, so `lanes can be started`, the live pane and `backend and
    // checkout` all read the same backend rather than each constructing —
    // and possibly disagreeing about — their own.
    let mux = crate::mux::backend(repo);

    let mut report = Report::default();

    // A pipeline file that will not parse must not stop the command you run
    // to find out which one it is. Report the load failure as a failed row
    // and run the checks that read no pipeline — config, issue tracking,
    // retired keys, the multiplexer, the update check — since the graph,
    // agent and prompt checks below all need a loaded pipeline set.
    let pipelines = match pipelines {
        Ok(pipelines) => pipelines,
        Err(err) => {
            report.check("pipelines load", Err(err));
            report.record_all(config_checks(repo, config_error, &config));
            report.record_all(issue_tracking_checks(repo, &config.issue_tracking));
            report.record_all(retired_key_notes(&repo.checkout));
            report.record(mux_check(mux.as_ref()));
            doctor_update(repo, &mut report);
            report.record(match crate::lock::Lock::holder(&repo.lock_file())? {
                Some(pid) => Finding::Note(format!("a dispatcher is running (pid {pid})")),
                None => Finding::NoteVerbose("no dispatcher running".into()),
            });
            return finish(&report, verbose, json);
        }
    };
    let pipelines = &pipelines;

    // A task file can be hand-edited into a graph nothing can get through, and
    // the only symptom is tasks that quietly never start.
    let tasks = repo.tasks().unwrap_or_default();
    let graph = Graph::build_for_run(&tasks, pipelines, &repo.archive_dir(), repo.unattended());

    report.record_all(config_checks(repo, config_error, &config));
    report.record_all(issue_tracking_checks(repo, &config.issue_tracking));
    report.record_all(retired_key_notes(&repo.checkout));
    warmth_notes(repo, pipelines, &config, &mut report);
    report.record_all(pipeline_graph_checks(pipelines, &config, &graph));
    report.record_all(branch_and_forge_checks(
        repo,
        &tasks,
        pipelines,
        &config.issue_tracking,
    ));
    report.record(mux_check(mux.as_ref()));
    report.record(live_check(mux.as_ref(), no_live));
    report.record(Finding::Check(
        "git identity".into(),
        crate::commands::dispatch::check_git_identity(repo, pipelines, &config),
    ));
    report.record(Finding::Check(
        "no stale .git/index.lock".into(),
        crate::commands::dispatch::check_index_lock(repo),
    ));
    report.record(Finding::Check(
        "backend and checkout".into(),
        crate::commands::dispatch::check_backend_checkout(repo, mux.as_ref()),
    ));
    report.record_all(agent_checks(pipelines, &config));
    report.record_all(model_health_checks(pipelines, &config));
    report.record_all(agent_kind_checks(&config));
    doctor_update(repo, &mut report);
    report.record_all(prompt_checks(repo, pipelines));
    report.record_all(jobs_checks(repo, pipelines));

    // A dispatcher that is running is a fact about this moment that changes
    // what a person should do next. Its absence is the ordinary case, and says
    // nothing, so it keeps to the full listing.
    report.record(match crate::lock::Lock::holder(&repo.lock_file())? {
        Some(pid) => Finding::Note(format!("a dispatcher is running (pid {pid})")),
        None => Finding::NoteVerbose("no dispatcher running".into()),
    });

    finish(&report, verbose, json)
}

/// `doctor` on a checkout whose own `config.toml` does not parse — `err` is
/// [`Config::load`]'s failure against `repo.checkout`, not `repo.root`.
///
/// Reporting the parse error is most of the value here: it is one line, it
/// names the file and the line in it, and until it is fixed nothing else in
/// spoolway runs at all. The rest is what can still be said without knowing a
/// single setting — the pipeline is a separate file, and a branch is git's
/// answer, not the config's. Everything else is skipped rather than guessed at:
/// running the agent and path checks against built-in defaults would report
/// on settings this project never wrote.
fn doctor_unconfigured(
    repo: &Repo,
    pipelines: Result<Pipelines>,
    err: anyhow::Error,
    verbose: bool,
    json: bool,
) -> Result<()> {
    let mut report = Report::default();

    report.check("config parses", Err(err));
    report.check(
        "pipelines are valid",
        pipelines
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e:#}"))
            .and_then(|pipelines| {
                pipelines
                    .validate()
                    .map(|()| Some(format!("{:?}", pipelines.names())))
            }),
    );
    report.check("on a usable branch", repo.branch().map(Some));

    match crate::lock::Lock::holder(&repo.lock_file())? {
        Some(pid) => report.note(format!("a dispatcher is running (pid {pid})")),
        None => report.note_verbose("no dispatcher running"),
    }
    report.note(format!(
        "every other check reads a setting, so they are skipped until {} parses (this \
         checkout's own copy)",
        relative(&repo.checkout, &Config::path_in(&repo.checkout))
    ));

    finish(&report, verbose, json)
}

/// The checks that only need the checkout's own loaded `config` and whether
/// the project's own copy (`repo.root`'s) parsed — see the module doc for why
/// those are two different questions. Order matches `doctor`'s own: the
/// checkout's parse, the project's parse (only when it failed), then the one
/// setting an unattended run cannot start without.
fn config_checks(
    repo: &Repo,
    config_error: Option<anyhow::Error>,
    config: &Config,
) -> Vec<Finding> {
    let mut findings = vec![Finding::Check(
        "config parses".into(),
        Ok(Some(format!(
            "{} — this checkout's own copy",
            Config::path_in(&repo.checkout).display()
        ))),
    )];
    // The checkout's file parsing says nothing about the project's own copy —
    // the one the real dispatcher runs on — when the two differ. In the main
    // checkout they are the same file, so `config_error` and the check above
    // agree and this never fires.
    if let Some(err) = config_error {
        findings.push(Finding::Check(
            "the project's own config parses".into(),
            Err(err.context(Config::path_in(&repo.root).display().to_string())),
        ));
    }
    // An unattended run resumes a block by staffing `blocked` with a lane —
    // every pipeline has one now, materialised by `Pipelines::assemble` from
    // `[unattended]`'s own keys, or overridden by the pipeline itself — and a
    // lane needs a model to run on. Checked here rather than left to
    // `pipeline check`'s soft "names no model" problem, because this is the
    // one missing model that stops a *run* from starting at all rather than
    // just one step of it.
    findings.push(Finding::Check(
        "an unattended run staffs `blocked`".into(),
        match config.unattended.enabled && config.unattended.blocked_model.trim().is_empty() {
            true => Err(anyhow::anyhow!("unattended.blocked_model is blank")),
            false => Ok(None),
        },
    ));
    if let Some(note) = current_price_table_age_note(config.prices.max_age_days) {
        findings.push(Finding::Note(note));
    }
    findings
}

/// The old table is advisory state: it is a note rather than a check, and a
/// zero limit suppresses even the table read so "off" stays wholly silent.
fn current_price_table_age_note(max_age_days: u64) -> Option<String> {
    if max_age_days == 0 {
        return None;
    }
    price_table_age_note(max_age_days, crate::models::price_table_age().days)
}

fn price_table_age_note(max_age_days: u64, age_days: u64) -> Option<String> {
    (max_age_days > 0 && age_days > max_age_days).then(|| {
        format!(
            "the price table was generated {} days ago, past the {max_age_days} in \
             `prices.max_age_days` — `spoolway models refresh` takes litellm's current prices",
            age_days
        )
    })
}

/// Everything `[issue_tracking]` can get wrong on its own, independent of the
/// pipeline or the branch: a hook named with nothing to hand it, a hook that
/// cannot resolve to a real file, a hook file that was never written, a hook
/// script too old to have a `fetch` branch, and — for the one hook that shells
/// out — the binaries it needs beside it.
fn issue_tracking_checks(
    repo: &Repo,
    tracking: &crate::config::IssueTrackingConfig,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let config_path = Config::path_in(&repo.checkout).display().to_string();

    // The one half-set shape a person can actually reach: a hook named with
    // nothing to hand it. `hook` blank means no issue tracking, whatever
    // `project_key` holds — that combination is fine and reported nowhere —
    // but a hook that runs with a blank `project_key` opens no ticket ever,
    // silently, on every one of a task's four events.
    findings.push(Finding::Check(
        "`[issue_tracking]` is fully configured or fully off".into(),
        match !tracking.hook.trim().is_empty() && tracking.project_key.trim().is_empty() {
            true => Err(anyhow::anyhow!(
                "{config_path}: [issue_tracking] names `{}` but project_key is empty — no \
                 ticket will ever open. Set it, or clear hook to switch issue tracking off.",
                tracking.hook.trim()
            )),
            false => Ok(None),
        },
    ));
    // The hook's own doc comment promises a bare filename can never resolve
    // outside `.spoolway/hooks/` — `crate::tracking::is_bare_filename` is
    // what actually enforces that at the point the value is read, and a
    // config naming something else silently runs no hook at all. Worth
    // saying here rather than only failing quietly on every one of a task's
    // four events.
    findings.push(Finding::Check(
        "`issue_tracking.hook` is a bare filename".into(),
        match !tracking.hook.trim().is_empty()
            && !crate::tracking::is_bare_filename(tracking.hook.trim())
        {
            true => Err(anyhow::anyhow!(
                "{config_path}: [issue_tracking] names `{}`, which is not a bare filename — a \
                 hook only ever runs a script inside .spoolway/hooks/, so this can never run. \
                 Use a bare name, or clear hook to switch issue tracking off.",
                tracking.hook.trim()
            )),
            false => Ok(None),
        },
    ));
    // A bare, well-formed name still fails silently on every one of a task's
    // four events if the file behind it was never written — `init` scaffolds
    // both shipped hooks, but nothing stops a hand-edited config naming one
    // that was renamed or never copied in.
    if let Some(path) = crate::tracking::hook_path_in(&repo.checkout, &tracking.hook) {
        findings.push(Finding::Check(
            "`issue_tracking.hook` script exists".into(),
            match path.exists() {
                true => Ok(Some(relative(&repo.checkout, &path))),
                false => Err(anyhow::anyhow!(
                    "{} does not exist — `spoolway init` writes it, or clear hook to switch \
                     issue tracking off",
                    path.display()
                )),
            },
        ));
    }
    // `spoolway update` never rewrites a hook a project already has, so
    // every install that named a tracker before this event existed has a
    // script with no `fetch` branch — and running one anyway would read as
    // "fetched an issue with nothing on it" rather than "never
    // implemented", see `tracking::has_fetch_branch`. Worth saying here
    // rather than only failing `spoolway issue show` with an exit code and
    // no explanation.
    findings.push(Finding::Check(
        "the configured hook script has a `fetch` branch".into(),
        match crate::tracking::missing_fetch_branch(&repo.checkout, &tracking.hook) {
            Some(name) => Err(anyhow::anyhow!(
                "{config_path}: [issue_tracking] names `{name}`, whose script has no `fetch` \
                 branch — `spoolway issue show` needs one to read an issue back out of the \
                 tracker. Add a case for `SPOOLWAY_EVENT=fetch` the way the shipped samples \
                 do, or regenerate one with `spoolway init` into a fresh directory to copy \
                 the branch across by hand."
            )),
            None => Ok(None),
        },
    ));
    // `issue_tracking.key_in_names` prefixes every generated name with a
    // `slug=` the hook answers — but `spoolway update` never rewrites a hook
    // a project already has, so a project that turned the flag on without
    // adding the line gets no prefix at all, silently, on every `queue add`.
    findings.push(Finding::Check(
        "the configured hook writes a `slug=` line".into(),
        match crate::tracking::missing_slug_line(
            &repo.checkout,
            &tracking.hook,
            tracking.key_in_names,
        ) {
            Some(name) => Err(anyhow::anyhow!(
                "{config_path}: [issue_tracking] key_in_names is on, but {name} never writes \
                 `slug=` — groups, branches and worktrees will be named without a prefix. Add a \
                 slug= line the way the shipped samples do, or clear key_in_names."
            )),
            None => Ok(None),
        },
    ));
    // The two shipped hooks each shell out to a binary this project does not
    // otherwise need — `gh` is already checked below for `spoolway stack`,
    // so only the one hook that would go unnoticed is checked here: `jira.sh`
    // needs both `acli` and, since `workitem create --json` is the only way
    // it gets a ticket key back, `jq` beside it.
    if tracking.hook.trim() == crate::cli::Tracker::Jira.hook_name() {
        for binary in ["acli", "jq"] {
            findings.push(Finding::Check(
                format!("`{binary}` is on PATH"),
                match which(binary) {
                    Some(path) => Ok(Some(path)),
                    None => Err(anyhow::anyhow!(
                        "`{binary}` is not on PATH — issue_tracking.hook names `jira.sh`, and \
                         it cannot open or close a ticket without it"
                    )),
                },
            ));
        }
    }
    findings
}

/// Retired: the guard this used to size — how many times a lane may be
/// LAUNCHED at the step a task is on before a person is asked instead — is a
/// constant now (`dispatch::MAX_LAUNCHES`), never a number anybody tuned in
/// practice. Both spellings still parse and both are dropped on the next
/// save; the only way a project finds out is here.
fn retired_key_notes(checkout: &Path) -> Vec<Finding> {
    if names_key(checkout, "max_attempts") || names_key(checkout, "max_launches") {
        vec![Finding::Note(
            "dispatch.max_launches (and its old name, max_attempts) is retired in this \
             checkout's config — the launch guard it sized is a constant now, always one launch \
             before a person is asked. The key still parses; it is dropped on the next save."
                .into(),
        )]
    } else {
        Vec::new()
    }
}

/// `pipelines are valid` and `task dependency graph` bracket one note about
/// an unattended run with no ceiling at all — grouped together because all
/// three read `config.unattended`, and `doctor` reports them in this order.
fn pipeline_graph_checks(pipelines: &Pipelines, config: &Config, graph: &Graph) -> Vec<Finding> {
    let mut findings = vec![Finding::Check(
        "pipelines are valid".into(),
        pipelines
            .validate()
            .map(|()| Some(format!("{:?}", pipelines.names()))),
    )];
    // An unattended run has exactly two things that can stop it short of an
    // empty queue — one in tokens, one in money — and a project that turned
    // the mode on without setting either has asked for a run which nothing
    // ends. Worth saying out loud here because it cannot be noticed anywhere
    // else — every setting here is valid, each is fine on its own, and the
    // combination only shows up as a bill. Either ceiling alone is enough;
    // only their both being off is worth a note.
    if config.unattended.enabled
        && config.unattended.max_output_tokens == 0
        && config.unattended.max_cost_usd <= 0.0
    {
        findings.push(Finding::Note(
            "unattended.enabled is on with no unattended.max_output_tokens and no \
             unattended.max_cost_usd, so nothing bounds this project's runs: a task that \
             blocks starts a lane on `blocked` with no per-task bound on how many times it \
             round-trips, and two agents that will not agree keep going until you stop them. \
             Set a ceiling in output tokens or in dollars — both are reported by `spoolway \
             spend` and the board's footer — or leave both deliberately at 0"
                .into(),
        ));
    }
    findings.push(Finding::Check(
        "task dependency graph".into(),
        graph.validate().map(|()| Some(graph.summary())),
    ));
    findings
}

/// The branch (or branches) work is based on, whether the forge already knows
/// about them, and — the one thing that actually shells out to `gh` — whether
/// `gh` itself is usable when something here would call it.
///
/// Grouped together because the branches computed for the first two checks
/// (`bases`) feed the second, and because all three answer one question a
/// person asks together: can a task queued right now actually get its work
/// out?
fn branch_and_forge_checks(
    repo: &Repo,
    tasks: &[Task],
    pipelines: &Pipelines,
    tracking: &crate::config::IssueTrackingConfig,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    // The branches work is actually based on — what the queue names, which with
    // several plans in flight is several branches and none of them necessarily
    // the one this checkout is on. An empty queue has nothing to report yet, so
    // the branch a task queued here would get stands in for it.
    let mut bases: Vec<String> = tasks.iter().filter_map(|t| t.front.base.clone()).collect();
    bases.sort();
    bases.dedup();
    if bases.is_empty() {
        findings.push(Finding::Check(
            "on a usable branch".into(),
            repo.branch().map(Some),
        ));
        bases.extend(repo.branch());
    } else {
        findings.push(Finding::Check(
            "work is based on".into(),
            Ok(Some(bases.join(", "))),
        ));
    }

    // Task pull requests are opened against their base, so the forge has to
    // know about it before the first one.
    if repo.has_remote() {
        for base in &bases {
            let listed = repo
                .git(&["ls-remote", "--heads", "origin", base])
                .map(|listed| {
                    Some(if listed.trim().is_empty() {
                        format!("`{base}` is not on origin yet — dispatch will push it")
                    } else {
                        format!("`{base}` is on origin")
                    })
                });
            findings.push(Finding::Check(format!("`{base}` is publishable"), listed));
        }
    } else {
        // A note, not a problem. Nothing in a pipeline says it reaches a forge
        // any more — what `handover` does with a branch is its own step's
        // business — so spoolway cannot tell a project that finishes by
        // merging into its own checkout from one whose hand-off is about to
        // fail. Saying which it is here would be guessing, and reporting a
        // deliberate setup as a fault sends somebody to add a remote nothing
        // would use.
        findings.push(Finding::Note(
            "no git remote — a `handover` that pushes or opens a pull request has nowhere to \
             send it"
                .into(),
        ));
    }

    // `spoolway stack` shells out to `gh` for everything past the push — the
    // pull request and the stack registration both go through it — and a
    // missing or unauthenticated `gh` fails there with no earlier symptom.
    // But a project whose pipeline never reaches a forge — no step calls
    // `spoolway stack`, and issue tracking does not name the `github` hook —
    // has nothing to lose from that, so this is a hard problem only when
    // something here would actually run `gh`.
    if reaches_forge(pipelines, tracking) {
        findings.push(Finding::Check(
            "gh is on PATH and authenticated".into(),
            gh_status(),
        ));
    } else if let Err(err) = gh_status() {
        findings.push(Finding::Note(format!(
            "`gh` is not usable ({err:#}), but nothing here calls it: no step runs `spoolway \
             stack`, and issue tracking does not name the `github` hook"
        )));
    }

    findings
}

/// Whether a lane can be started at all — the one check that never depends on
/// the pipeline or the config, only on whether the terminal multiplexer this
/// machine has is one spoolway knows how to drive.
fn mux_check(mux: &dyn Mux) -> Finding {
    Finding::Check(
        "lanes can be started".into(),
        if mux.is_available() {
            Ok(Some(mux.name().into()))
        } else {
            Err(anyhow::anyhow!("{}", mux.unavailable()))
        },
    )
}

/// Open a throwaway pane on a scratch directory, run a trivial command in
/// it, wait for the wrapper's own pid file, and close everything again —
/// the one check here that finds out a lane can *really* start rather than
/// only that the backend answers. Reuses the same wrapper and wait
/// [`crate::command_step::Runs`] gives a real command step, so this is
/// exactly the path a lane's own turn takes, not a stand-in for it.
///
/// `no_live` is `--no-live`: skips this row alone, leaving every other check
/// unchanged, for a machine where opening a real pane is slow or noisy
/// (CI, say) but the rest of `doctor` is still worth running.
///
/// A backend with no real pane to test — headless, whose panes are names
/// rather than processes — answers with a note instead of a check: nothing
/// was opened, so there is nothing to say passed or failed.
fn live_check(mux: &dyn Mux, no_live: bool) -> Finding {
    if no_live {
        return Finding::NoteVerbose("--no-live: skipped the live pane check".into());
    }
    if !mux.is_available() {
        // Already a `FAIL` on `lanes can be started` — nothing more to say.
        return Finding::NoteVerbose("no live pane check: this backend is not available".into());
    }
    match live_pane(mux) {
        Ok(Some(note)) => Finding::Check("a lane really starts".into(), Ok(Some(note))),
        Ok(None) => Finding::NoteVerbose(format!(
            "no live pane check: {} has no real pane to run a command in",
            mux.name()
        )),
        Err(err) => Finding::Check("a lane really starts".into(), Err(err)),
    }
}

/// The three calls [`live_check`] makes: [`Mux::create_pane`] a throwaway
/// workspace on a scratch directory, [`Mux::run_in_pane`] to actually run a
/// trivial command in it, then [`Mux::close_pane`] twice — the split pane
/// the command ran in, then the workspace's own root pane, which under
/// every real backend takes its now-empty tab and workspace with it.
///
/// `Ok(None)` from a backend whose [`Workspace::tab_id`] is absent —
/// headless, which hands back a pane id that names no real process — since
/// there is nothing here for [`Mux::run_in_pane`] to run a script in.
fn live_pane(mux: &dyn Mux) -> Result<Option<String>> {
    let dir = crate::scratch::root("doctor-live");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let workspace = mux.create_pane(&dir, "doctor")?;
    let Some(tab_id) = workspace.tab_id.clone() else {
        return Ok(None);
    };

    let runs = crate::command_step::Runs::new(&dir);
    let key = "doctor · live";
    let env = std::collections::BTreeMap::new();
    let script = runs.script_for_pane(key, "true", &env)?;

    let started = std::time::Instant::now();
    let outcome = mux
        .run_in_pane(&tab_id, &dir, "doctor", &script, &env)
        .and_then(|pane| {
            pane.ok_or_else(|| {
                anyhow::anyhow!("{} reports panes but ran nothing in one", mux.name())
            })
        })
        .and_then(|pane| runs.await_started(key).map(|_pid| pane));
    runs.forget(key);

    // Best-effort either way: a pane left standing after a failed check is a
    // worse trail to leave than a close call whose own error is swallowed.
    if let Ok(pane) = &outcome {
        let _ = mux.close_pane(pane);
    }
    let _ = mux.close_pane(&workspace.pane_id);
    // Stopped only now, after both closes: the row says how long the whole
    // check took — opening the pane, running the command, closing
    // everything again — not just the time to see it start.
    let elapsed = started.elapsed();

    outcome.map(|pane| {
        Some(format!(
            "pane {pane}, closed in {:.1}s",
            elapsed.as_secs_f64()
        ))
    })
}

/// Per agent profile a pipeline actually references: whether its binary is on
/// PATH, whether every step that runs on it names a model, and whether its
/// permission mode is one spoolway recognises. Three checks per agent rather
/// than one, because each is fixed a different way and a person should not
/// have to guess which of three things "agent `x` is broken" means.
fn agent_checks(pipelines: &Pipelines, config: &Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (agent, steps) in pipelines.referenced_agents() {
        let outcome = config.agent(agent).and_then(|profile| {
            let found = which(&profile.kind);
            match found {
                Some(path) => Ok(Some(format!("{} at {path}", profile.kind))),
                None => Err(anyhow::anyhow!(
                    "`{}` is not on PATH, but {steps:?} need it",
                    profile.kind
                )),
            }
        });
        findings.push(Finding::Check(format!("agent `{agent}`"), outcome));

        // spoolway names no model of its own: every step that runs on this
        // agent has to name one, and finding that out here beats finding it
        // out from an agent that was handed an empty `--model`.
        // `blocked` is materialised even in attended mode, where it parks for
        // a person and launches no agent. Config checks enforce its model once
        // unattended mode actually staffs it; do not misdirect an attended
        // project to pipeline YAML for this config-derived blank.
        let model = if steps
            .iter()
            .filter(|step| config.unattended.enabled || **step != crate::pipeline::BLOCKED)
            .all(|step| pipelines.step_has_model(step))
        {
            Ok(Some(
                "set per step in .spoolway/pipelines/<name>.yml".into(),
            ))
        } else {
            Err(anyhow::anyhow!(
                "some step running on `{agent}` names no model: — give it one in \
                 .spoolway/pipelines/<name>.yml"
            ))
        };
        findings.push(Finding::Check(
            format!("agent `{agent}` has a model"),
            model,
        ));

        // A wrong mode is only ever caught here or at the lane, and at the lane
        // it is an agent dying on an unrecognised flag several passes into a
        // run — long after whoever set it has stopped watching.
        let permissions = config
            .agent(agent)
            .and_then(|profile| profile.permission_mode_status());
        findings.push(Finding::Check(
            format!("agent `{agent}` permission mode"),
            permissions,
        ));
    }
    findings
}

/// Every check and note that reads a model's own settings rather than an
/// agent profile's: the legacy/template placeholder, a name that resolves to nothing,
/// a step nothing caps, an `exclusive` model with no `slots` of its own, a
/// `slots`/`exclusive` model that has not said whether it is `local`, and a
/// `session_blocked_ctx` set against a model that cannot honour it. Grouped
/// together because all six walk the same `pipelines`/`config.models` data,
/// several of them by way of the same `named` map.
fn model_health_checks(pipelines: &Pipelines, config: &Config) -> Vec<Finding> {
    let mut findings = Vec::new();

    // The one unresolvable name that is a failure rather than a note: the
    // placeholder older scaffolds and the annotated template carry, which
    // says "name your local model here" and is not a model. It satisfies checks that ask
    // whether a step names something — that is what a placeholder does — so a
    // project that has not read the comment above it looks configured and is
    // not, and finds out one lane later when an agent is handed a model
    // nothing serves.
    let named = crate::models::named(pipelines);
    let unset: Vec<&str> = named
        .get(crate::models::PLACEHOLDER)
        .cloned()
        .unwrap_or_default();
    findings.push(Finding::Check(
        "a model is named for every step".into(),
        match unset.is_empty() {
            true => Ok(None),
            false => Err(anyhow::anyhow!(
                "{} still name `{}`, which is a spoolway placeholder rather than a \
                 model — put your own local model's name in the pipeline file",
                unset.join(", "),
                crate::models::PLACEHOLDER
            )),
        },
    ));

    // A step naming a model is not the same as that name meaning anything.
    // This is a note rather than a failure — an unpriced, unsized model is a
    // lane that still runs, just unaccounted, exactly like a dropped
    // `{session_id}` — but it is worth a person seeing before `spoolway spend`
    // shows them a bill with a hole in it. The placeholder is left out: it was
    // a problem a moment ago, and saying it twice in two weights reads as two
    // findings.
    let unresolved: Vec<(&str, Vec<&str>)> = named
        .iter()
        .filter(|(model, _)| **model != crate::models::PLACEHOLDER)
        .filter(|(model, _)| {
            crate::models::resolve(&config.models, model).source == crate::models::Source::Unknown
        })
        .map(|(model, steps)| (*model, steps.clone()))
        .collect();
    if !unresolved.is_empty() {
        for (model, steps) in &unresolved {
            findings.push(Finding::Note(format!(
                "model `{model}` resolves to nothing — {} will run unpriced and unsized. \
                 `spoolway models` lists every model this way; price it with `spoolway config \
                 set models.'{model}'.input <usd per 1M>`",
                steps.join(", ")
            )));
        }
    }

    // Nothing caps this step at all. The two counts that could — the model's
    // own `slots` and its profile's `concurrency` — are both a fallback for
    // the other, so a step whose model sets no `slots` and whose profile sets
    // no `concurrency` runs as many lanes at once as the queue offers it.
    //
    // That combination is reachable by doing nothing rather than by asking for
    // it: the shipped local profiles carry no `concurrency` on purpose, since
    // how much a local server serves at once is the model's fact and not the
    // harness's, and a project that has not yet written its models into
    // `[models]` lands here without touching a setting. Unlimited is still a
    // legal answer — `concurrency = 0` has always meant it — so this is a note
    // and not a failure, but it should be a choice somebody made.
    // Keyed by the pair, and not by the step: the same model on the same
    // profile is one thing to fix however many steps name it, and a line per
    // step said it six times in a shipped pipeline. The steps are still listed,
    // the way the two checks either side of this one list theirs.
    let mut uncapped: std::collections::BTreeMap<(&str, &str), Vec<&str>> =
        std::collections::BTreeMap::new();
    for step in pipelines
        .pipelines
        .values()
        .flat_map(|pipeline| &pipeline.steps)
        .filter(|step| step.kind() == StepKind::Agent)
    {
        let Some(model) = step.model.as_deref().filter(|m| !m.trim().is_empty()) else {
            continue;
        };
        if model == crate::models::PLACEHOLDER {
            continue;
        }
        let Some(agent) = step.agent.as_deref() else {
            continue;
        };
        let Some(profile) = config.agents.get(agent) else {
            continue;
        };
        if profile.concurrency > 0 {
            continue;
        }
        let slots = crate::models::resolve(&config.models, model)
            .price
            .map_or(0, |p| p.slots);
        if slots > 0 {
            continue;
        }
        let steps = uncapped.entry((agent, model)).or_default();
        if !steps.contains(&step.id.as_str()) {
            steps.push(&step.id);
        }
    }
    for ((agent, model), steps) in &uncapped {
        findings.push(Finding::Note(format!(
            "nothing caps `{model}` on agent `{agent}` — the model sets no `slots` and the \
             profile no `concurrency`, so {} run as many lanes at once as the queue offers. Set \
             the model's count with `spoolway config set models.'{model}'.slots <n>`",
            steps.join(", ")
        )));
    }

    // `exclusive` says this model never shares a card with a different
    // exclusive one — the point of that, on a server holding one set of
    // weights at a time, is that a step naming it still runs several at once
    // up to a real cap. With no `slots` of its own that cap silently falls
    // back to its profile's `concurrency`, which was sized for the binary,
    // not the model.
    let exclusive_no_slots: Vec<(&str, Vec<&str>)> = named
        .into_iter()
        .filter(|(model, _)| *model != crate::models::PLACEHOLDER)
        .filter_map(|(model, steps)| {
            let price = crate::models::resolve(&config.models, model).price?;
            (price.exclusive && price.slots == 0).then_some((model, steps))
        })
        .collect();
    for (model, steps) in &exclusive_no_slots {
        findings.push(Finding::Note(format!(
            "model `{model}` is `exclusive` with no `slots` — {} falls back to its profile's \
             `concurrency`, which a local profile does not set. Give it its own count with \
             `spoolway config set models.'{model}'.slots <n>`",
            steps.join(", ")
        )));
    }

    // `slots` and `exclusive` both describe one card's worth of hardware, so a
    // model carrying either is almost certainly local — and a local model that
    // has not said so gets no board line when a queued task routes to it,
    // which is the only thing that would ever tell a person the card is
    // shared. Read straight off `config.models` rather than the `named` map:
    // the flag is worth setting on a sized model whether or not a pipeline
    // here points at it yet. `local` itself is never a failure — a run takes
    // the same decisions with it set or absent — so this is a note.
    for (glob, price) in &config.models {
        if price.local || (price.slots == 0 && !price.exclusive) {
            continue;
        }
        // Name `slots` whenever it is set, whether or not `exclusive` is too:
        // the mockup this note follows reads "sets `slots` but not `local`".
        // `exclusive` is named only for an entry that carries it alone — the
        // `continue` above has already ruled out neither being set.
        let which = if price.slots > 0 {
            "`slots`"
        } else {
            "`exclusive`"
        };
        findings.push(Finding::Note(format!(
            "model `{glob}` sets {which} but not `local`, so a run that puts lanes on it says \
             nothing about the card being shared — set it with `spoolway config set \
             models.'{glob}'.local true`"
        )));
    }

    // `agents.<profile>.session_blocked_ctx` only ever fires against the
    // model that actually answered — resolved the same way
    // `dispatch::ctx_ceiling_hold` reads it, through `models::resolve`. A
    // profile that sets the ceiling against a step whose model resolves no
    // `context_window` believes it has a brake that nothing is pulling: the
    // dispatcher's own check answers `None` for exactly this config, in
    // silence, so this is the one place a person finds out.
    let mut blocked_ceiling_unresolved: std::collections::BTreeSet<(&str, &str)> =
        std::collections::BTreeSet::new();
    for step in pipelines
        .pipelines
        .values()
        .flat_map(|pipeline| &pipeline.steps)
        .filter(|step| step.kind() == StepKind::Agent)
    {
        let Some(agent) = step.agent.as_deref() else {
            continue;
        };
        let Some(profile) = config.agents.get(agent) else {
            continue;
        };
        if profile.session_blocked_ctx == 0 {
            continue;
        }
        let Some(model) = step.model.as_deref().filter(|m| !m.trim().is_empty()) else {
            continue;
        };
        let resolves = crate::models::resolve(&config.models, model)
            .price
            .is_some_and(|price| price.context_window > 0);
        if !resolves {
            blocked_ceiling_unresolved.insert((agent, model));
        }
    }
    for (agent, model) in &blocked_ceiling_unresolved {
        let ceiling = config.agents[*agent].session_blocked_ctx;
        findings.push(Finding::Note(format!(
            "agents.{agent}: session_blocked_ctx = {ceiling}, but `{model}` resolves to no \
             context_window — the ceiling never fires for this profile. Set: `spoolway config \
             set models.'{model}'.context_window <tokens>`"
        )));
    }

    findings
}

/// Per configured agent profile — not only the ones a pipeline references —
/// whether its `kind` is one spoolway knows how to launch at all, and, for a
/// kind that launches but that spoolway cannot meter, what that costs a
/// project that adopted it.
fn agent_kind_checks(config: &Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    // A profile's argv is fixed per kind in `agent::ADAPTERS` now, so a lane
    // starting with no prompt can only mean the kind itself is not one
    // spoolway knows how to launch.
    for (name, profile) in &config.agents {
        let adapter = crate::agent::adapter(&profile.kind);
        if adapter.map(|a| a.args.is_empty()).unwrap_or(true) {
            findings.push(Finding::Check(
                format!("agent `{name}` kind"),
                Err(anyhow::anyhow!(
                    "spoolway does not yet know how to launch kind `{}` — add an `args` row to \
                     `agent::ADAPTERS`",
                    profile.kind
                )),
            ));
            continue;
        }

        // A note, and never a failure. An unmetered kind launches, runs,
        // reports and lands its work; what it loses is three readings, two of
        // which already have a fallback the dispatcher takes today. A project
        // running one on purpose should not have a permanently red doctor —
        // but it should not be able to adopt one without ever being told what
        // it gave up either, and this is the only surface that would otherwise
        // stay silent about it.
        if adapter.is_some_and(|a| !a.meters()) {
            findings.push(Finding::Note(format!(
                "agent `{name}` runs kind `{}`, which spoolway cannot account for: no \
                 ledger lines, so `spoolway spend` will not see its lanes; no session reuse, so \
                 every step starts fresh; and silence read from the pane rather than the \
                 transcript. It launches and runs regardless — `spoolway agent verify {}` \
                 reports it clause by clause",
                profile.kind, profile.kind
            )));
        }
    }
    findings
}

/// Every prompt a pipeline's agent steps actually run: whether its file
/// exists, and — once, for the whole project rather than per file — whether
/// `spoolway prompt check`'s lint has anything to say.
fn prompt_checks(repo: &Repo, pipelines: &Pipelines) -> Vec<Finding> {
    let mut findings = Vec::new();

    // One row per prompt file rather than one per step that names one:
    // `path_for` resolves a prompt from its name alone and never sees a step,
    // so the step id on a passing row never described what was read. The steps
    // come back when the file is missing, which is the only time they say
    // anything — and in prompt-name order, since step order stops meaning
    // anything once a file is checked once.
    let mut prompts: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for step in pipelines.pipelines.values().flat_map(|p| &p.steps) {
        if step.kind() != StepKind::Agent {
            continue;
        }
        let steps = prompts.entry(step.prompt_name()).or_default();
        if !steps.contains(&step.id.as_str()) {
            steps.push(&step.id);
        }
    }
    for (prompt, steps) in &prompts {
        let path = crate::prompt::path_for(repo, prompt);
        findings.push(Finding::Check(
            format!("prompt `{prompt}`"),
            if path.exists() {
                Ok(None)
            } else {
                Err(anyhow::anyhow!(
                    "{} is missing, but {steps:?} run it — run `spoolway init`",
                    path.display()
                ))
            },
        ));
    }

    // Read as a note, never a failure: a finding is prose read by a machine,
    // and the project is the one that knows whether it is wrong.
    match crate::prompt::lint(repo, pipelines) {
        Ok(lints) if lints.is_empty() => {
            findings.push(Finding::Check("prompts read clean".into(), Ok(None)));
        }
        Ok(lints) => findings.push(Finding::Note(format!(
            "{} prompt finding(s) — `spoolway prompt check` to read them",
            lints.len()
        ))),
        Err(err) => findings.push(Finding::Note(format!("prompts could not be read: {err:#}"))),
    }

    // Same read, over the seven typed messages a lane's pane receives rather
    // than a role's own prose — see `crate::lane_prompts`. A section that
    // names none of the placeholders its state needs, or names one this
    // binary does not substitute, is a finding here too.
    let lane_prompt_findings = crate::lane_prompts::lint(repo);
    if lane_prompt_findings.is_empty() {
        findings.push(Finding::Check("lane prompts read clean".into(), Ok(None)));
    } else {
        findings.push(Finding::Note(format!(
            "{} lane-prompts finding(s) — `spoolway prompt check` to read them",
            lane_prompt_findings.len()
        )));
    }

    findings
}

/// Cron jobs: one that names a routine that is gone, one whose expression
/// will not parse, one whose pipeline is not defined. A store that will not
/// load at all — a name in both, malformed TOML — is a single finding.
fn jobs_checks(repo: &Repo, pipelines: &Pipelines) -> Vec<Finding> {
    let mut findings = Vec::new();

    let jobs = match crate::jobs::load(repo) {
        Ok(jobs) => jobs,
        Err(err) => {
            findings.push(Finding::Check("job stores load".into(), Err(err)));
            return findings;
        }
    };

    for job in &jobs {
        let name = &job.name;

        // Parses *and* comes round: `0 0 30 2 *` parses cleanly and never
        // fires, and `spoolway jobs list` / the dispatcher's "run doctor"
        // line both need this to be the thing that names it.
        findings.push(Finding::Check(
            format!("job `{name}` schedule fires"),
            match crate::cron::Cron::parse(&job.spec.schedule) {
                Err(why) => Err(anyhow::anyhow!("`{}` — {why}", job.spec.schedule)),
                Ok(cron) => match cron.next_after(chrono::Local::now().naive_local()) {
                    Some(_) => Ok(None),
                    None => Err(anyhow::anyhow!(
                        "`{}` parses but never comes round — check its day-of-month and month \
                         fields in {}",
                        job.spec.schedule,
                        job.source.display()
                    )),
                },
            },
        ));

        findings.push(Finding::Check(
            format!("job `{name}` routine exists"),
            match job.target(repo) {
                Err(why) => Err(why),
                Ok(target) if target.exists() => Ok(None),
                Ok(target) => Err(anyhow::anyhow!(
                    "{} is not there — nothing under `.spoolway/routines/` matches `{}`",
                    target.display(),
                    job.spec.routine
                )),
            },
        ));

        findings.push(Finding::Check(
            format!("job `{name}` pipeline is defined"),
            if pipelines.pipelines.contains_key(&job.spec.pipeline) {
                Ok(None)
            } else {
                Err(anyhow::anyhow!(
                    "`{}` is not a pipeline (defined: {})",
                    job.spec.pipeline,
                    pipelines.names().join(", ")
                ))
            },
        ));
    }

    findings
}

/// Whether anything here would actually shell out to `gh`: a step whose
/// `run:` calls `spoolway stack`, or issue tracking naming the `github` hook.
/// Those are the only two callers `gh` has anywhere in this project — see
/// `commands::stack::gh_program` and `assets/hooks/github.sh` — so a project
/// with neither will never notice whether `gh` works at all.
fn reaches_forge(pipelines: &Pipelines, tracking: &crate::config::IssueTrackingConfig) -> bool {
    tracking.hook.trim() == crate::cli::Tracker::Github.hook_name()
        || crate::commands::dispatch::stack_step(pipelines).is_some()
}

/// Whether `gh` can actually do what `spoolway stack` needs of it: found on
/// PATH, and holding a working login.
///
/// Two different failures rather than one, because they are fixed two
/// different ways — installing the binary and running `gh auth login` are not
/// the same afternoon — and a person seeing only "gh is broken" would not
/// know which one to reach for.
fn gh_status() -> Result<Option<String>> {
    let Some(path) = which("gh") else {
        bail!("`gh` is not on PATH — `spoolway stack` cannot open a pull request without it");
    };
    let output = std::process::Command::new("gh")
        .args(["auth", "status"])
        .output()
        .with_context(|| format!("running `{path} auth status`"))?;
    if !output.status.success() {
        bail!(
            "`gh` is at {path} but is not authenticated: {}",
            first_line(&String::from_utf8_lossy(&output.stderr))
        );
    }
    Ok(Some(path))
}

/// What `spoolway update` would do here, and above all what it would refuse.
///
/// The report `update` prints is the paths it wrote and one line, because
/// which files moved is the only question anybody runs it to answer. That
/// leaves one thing homeless: a file it *declined* to touch, where somebody
/// has edited the half a machine reads. Silence there is a project quietly
/// running an old contract, so it surfaces here — which is where
/// `docs/installation.md` has claimed to report it all along.
///
/// Notes rather than problems, both of them. An outstanding update is a
/// command somebody has not run yet, and a refusal is usually a deliberate
/// edit; neither is a broken project, and neither should fail a `doctor` that
/// a lane runs.
fn doctor_update(repo: &Repo, report: &mut Report) {
    let dry = crate::cli::UpdateArgs {
        dry_run: true,
        replace: Vec::new(),
    };
    let Ok(outcomes) = crate::update::scan(repo, &dry) else {
        return;
    };

    let mut behind: Vec<(&str, &str)> = Vec::new();
    for outcome in &outcomes {
        match outcome {
            // One file can be behind for several reasons at once — a config
            // gains a setting and has a note rewritten in the same pass — and
            // this is a count of files, not of reasons.
            crate::update::Outcome::Wrote { path, detail } => {
                if !behind.iter().any(|(known, _)| *known == path.as_str()) {
                    behind.push((path, detail));
                }
            }
            crate::update::Outcome::Blocked { path, why } => {
                report.note(format!("{path}: {why}"));
            }
            crate::update::Outcome::Kept => {}
        }
    }

    if !behind.is_empty() {
        // One note, carrying its own continuation lines, so that the short
        // report and the full listing print the same block.
        let mut note = format!(
            "{} file(s) here are behind this spoolway — `spoolway update` takes them",
            behind.len()
        );
        for (path, detail) in &behind {
            // Bounded, because one of these reasons is a list of every setting
            // a config has gained since it was written, and a note that fills
            // a screen is a note nobody reads to the end of.
            note += &format!("\n          {path} ({})", ellipsis(detail, 72));
        }
        report.note(note);
    }
}

/// `text`, cut to `width` on a word boundary, with an ellipsis where it was
/// cut.
fn ellipsis(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let head: String = text.chars().take(width).collect();
    let cut = head.rfind(' ').unwrap_or(head.len());
    format!("{}…", &head[..cut])
}

/// Both keys `retire-warmth` retired, named for a project that still writes
/// them — parsed straight out of `config.toml` rather than off the loaded
/// [`Config`], since a struct that keeps a retired field only for parsing has
/// already forgotten which profile or glob wrote it.
///
/// `agents.<profile>.session_reuse_uncached` is reported against the models
/// that profile's steps actually run, so a project sees whether it was ever
/// load-bearing rather than a generic "this is gone". `models.<glob>.cache_ttl`
/// is reported per glob: it still parses, under its new name, and is rewritten
/// on the next save either way.
///
/// `config` is the checkout's own loaded copy — the same one `doctor` checks
/// everything else against — so `[models]`'s rates are resolved from the same
/// file the retired keys below were parsed out of, rather than from
/// `repo.config`'s (`repo.root`'s) copy, which can disagree with it.
fn warmth_notes(repo: &Repo, pipelines: &Pipelines, config: &Config, report: &mut Report) {
    let Ok(raw) = std::fs::read_to_string(Config::path_in(&repo.checkout)) else {
        return;
    };
    let Ok(doc) = raw.parse::<toml::Value>() else {
        return;
    };

    if let Some(agents) = doc.get("agents").and_then(toml::Value::as_table) {
        for (name, entry) in agents {
            if entry.get("session_reuse_uncached").is_none() {
                continue;
            }
            let idle = pipelines
                .pipelines
                .values()
                .flat_map(|p| &p.steps)
                .filter(|step| step.agent.as_deref() == Some(name.as_str()))
                .filter_map(|step| step.model.as_deref())
                .filter(|model| !model.trim().is_empty())
                .find_map(|model| {
                    let idle = crate::models::resolve(&config.models, model)
                        .price?
                        .session_reuse_idle?;
                    Some((model, idle))
                });
            match idle {
                Some((model, idle)) => report.note(format!(
                    "agents.{name}.session_reuse_uncached retired in this checkout's config — \
                     the horizon is now models.'{model}'.session_reuse_idle, which is set \
                     ({}). Drop the line.",
                    crate::config::human_duration::format(idle)
                )),
                None => report.note(format!(
                    "agents.{name}.session_reuse_uncached retired in this checkout's config — \
                     no model this profile runs sets session_reuse_idle, so nothing was being \
                     refused. Drop it."
                )),
            }
        }
    }

    if let Some(models) = doc.get("models").and_then(toml::Value::as_table) {
        for (glob, entry) in models {
            if entry.get("cache_ttl").is_some() {
                report.note(format!(
                    "models.'{glob}'.cache_ttl renamed session_reuse_idle in this checkout's \
                     config; still read, rewritten on the next save."
                ));
            }
        }
    }
}

/// Whether `config.toml` names a key at all, which is not the same question as
/// what the loaded config says: a key absent from the file takes whatever
/// default this spoolway ships, and a key present in it was someone's decision.
///
/// `checkout` because every doctor check reads against the checkout's own
/// copy — see the module doc.
fn names_key(checkout: &Path, key: &str) -> bool {
    std::fs::read_to_string(Config::path_in(checkout))
        .map(|raw| {
            raw.lines().any(|line| {
                line.split('=')
                    .next()
                    .is_some_and(|name| name.trim() == key)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn price_table_age_is_noted_only_after_the_enabled_limit() {
        assert!(price_table_age_note(0, 300).is_none());
        assert!(price_table_age_note(30, 30).is_none());
        assert!(price_table_age_note(30, 29).is_none());
        assert_eq!(
            price_table_age_note(30, 31).as_deref(),
            Some(
                "the price table was generated 31 days ago, past the 30 in \
                 `prices.max_age_days` — `spoolway models refresh` takes litellm's current prices"
            )
        );
    }

    /// Two directories, each holding its own `.spoolway/config.toml` with one
    /// key the other's file does not have — standing in for a project's root
    /// and a linked worktree's checkout without paying for real git. What
    /// `doctor` reads is a function of which directory it is handed, not of
    /// anything git-specific, so this is enough to pin that down.
    fn two_configs(name: &str, root_body: &str, checkout_body: &str) -> (PathBuf, PathBuf) {
        let base = crate::scratch::root(&format!("doctor-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("root");
        let checkout = base.join("checkout");
        for (dir, body) in [(&root, root_body), (&checkout, checkout_body)] {
            let state = dir.join(crate::config::STATE_DIR);
            std::fs::create_dir_all(&state).unwrap();
            std::fs::write(state.join("config.toml"), body).unwrap();
        }
        (root, checkout)
    }

    /// `--json`'s reading of a report: a `kind` tag per row, and the same
    /// short-form filtering `render`'s own text form applies — a passing
    /// check and a verbose-only note both left out unless `verbose` is set.
    #[test]
    fn render_json_carries_a_kind_tag_and_honours_verbose() {
        let mut report = Report::default();
        report.check("config parses", Ok(Some("looks fine".to_string())));
        report.check("pipelines are valid", Err(anyhow::anyhow!("bad pipeline")));
        report.note_verbose("no dispatcher running");

        let short = report.render_json(false);
        let rows = short["rows"].as_array().unwrap();
        assert_eq!(
            rows.len(),
            1,
            "only the failure survives the short form: {rows:?}"
        );
        assert_eq!(rows[0]["kind"], "fail");
        assert_eq!(rows[0]["label"], "pipelines are valid");

        let full = report.render_json(true);
        let rows = full["rows"].as_array().unwrap();
        assert_eq!(
            rows.len(),
            3,
            "verbose keeps the passing check and the note: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|r| r["kind"] == "ok" && r["note"] == "looks fine")
        );
        assert!(rows.iter().any(|r| r["kind"] == "note"));

        assert_eq!(short["checks"], 2);
        assert_eq!(short["problems"], 1);
    }

    /// `doctor`'s model messages name `.spoolway/pipelines/<name>.yml`, where a
    /// step's `model:` actually lives — not the retired single `pipeline.yml`
    /// that `Pipelines::load` now refuses (finding 25).
    #[test]
    fn the_model_check_points_at_the_pipelines_directory() {
        let pipelines = crate::pipeline::Pipelines::builtin();
        let config = Config::default();
        let findings = agent_checks(&pipelines, &config);

        let notes: Vec<String> = findings
            .iter()
            .filter_map(|f| match f {
                Finding::Check(label, outcome) if label.ends_with("has a model") => match outcome {
                    Ok(Some(note)) => Some(note.clone()),
                    Err(err) => Some(format!("{err:#}")),
                    Ok(None) => None,
                },
                _ => None,
            })
            .collect();

        assert!(!notes.is_empty(), "a model check was produced");
        for note in notes {
            assert!(note.contains(".spoolway/pipelines/"), "{note}");
            assert!(!note.contains(" in pipeline.yml"), "{note}");
        }
    }

    #[test]
    fn an_attended_synthetic_blocked_step_needs_no_model() {
        let mut pipelines = crate::pipeline::Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            pipeline
                .steps
                .iter_mut()
                .find(|step| step.id == crate::pipeline::BLOCKED)
                .unwrap()
                .model = None;
        }
        let mut config = Config::default();
        config.unattended.blocked_model.clear();

        let attended = agent_checks(&pipelines, &config);
        assert!(attended.iter().all(|finding| match finding {
            Finding::Check(label, result) if label.ends_with("has a model") => result.is_ok(),
            _ => true,
        }));

        config.unattended.enabled = true;
        let unattended = agent_checks(&pipelines, &config);
        assert!(unattended.iter().any(|finding| match finding {
            Finding::Check(label, result) if label.ends_with("has a model") => result.is_err(),
            _ => false,
        }));
    }

    /// One note per `[models]` entry that sets `slots` or `exclusive` without
    /// saying whether it is `local`, naming the `config set` command that
    /// answers it. A model that has set `local`, and one that sets neither
    /// `slots` nor `exclusive`, draw nothing. The note names `slots` whenever
    /// it is set — the task mockup's wording — and `exclusive` only for an
    /// entry that carries it alone.
    #[test]
    fn a_sized_model_that_has_not_set_local_is_noted() {
        let pipelines = crate::pipeline::Pipelines::builtin();
        let mut config = Config::default();
        config.models.insert(
            "*Qwen3.6-35B-A3B".into(),
            crate::usage::ModelPrice {
                slots: 3,
                exclusive: true,
                ..Default::default()
            },
        );
        config.models.insert(
            "*Ornith-1.5-35B-A3B".into(),
            crate::usage::ModelPrice {
                slots: 3,
                exclusive: true,
                local: true,
                ..Default::default()
            },
        );
        config.models.insert(
            "*Muse-*".into(),
            crate::usage::ModelPrice {
                exclusive: true,
                ..Default::default()
            },
        );
        config.models.insert(
            "priced-cloud-*".into(),
            crate::usage::ModelPrice {
                input: 5.0,
                ..Default::default()
            },
        );

        let local_notes: Vec<String> = model_health_checks(&pipelines, &config)
            .iter()
            .filter_map(|f| match f {
                Finding::Note(text) if text.contains("but not `local`") => Some(text.clone()),
                _ => None,
            })
            .collect();

        // One for Qwen, one for the exclusive-only Muse glob; nothing for the
        // `local` Ornith entry or the priced cloud one.
        assert_eq!(local_notes.len(), 2, "{local_notes:#?}");
        let note = |glob: &str| {
            local_notes
                .iter()
                .find(|n| n.contains(&format!("model `{glob}`")))
                .unwrap_or_else(|| panic!("no note for {glob}: {local_notes:#?}"))
        };
        // `slots` is set, so the note names `slots` even though `exclusive` is
        // set too — the wording the task mockup draws.
        assert!(
            note("*Qwen3.6-35B-A3B").contains("sets `slots` but not `local`"),
            "{}",
            note("*Qwen3.6-35B-A3B")
        );
        assert!(
            note("*Qwen3.6-35B-A3B")
                .contains("spoolway config set models.'*Qwen3.6-35B-A3B'.local true"),
            "{}",
            note("*Qwen3.6-35B-A3B")
        );
        // `exclusive` alone is the only case that names `exclusive`.
        assert!(
            note("*Muse-*").contains("sets `exclusive` but not `local`"),
            "{}",
            note("*Muse-*")
        );
    }

    #[test]
    fn names_key_reads_whichever_directory_it_is_given() {
        let (root, checkout) = two_configs(
            "names-key",
            "[dispatch]\nmax_launches = 3\n",
            "[dispatch]\ndefault_pipeline = \"x\"\n",
        );
        assert!(
            names_key(&root, "max_launches"),
            "the root's own file names the key"
        );
        assert!(
            !names_key(&checkout, "max_launches"),
            "the checkout's file does not — reading the wrong directory here is exactly the \
             bug this task fixes"
        );
    }

    fn single_step_pipelines(step_yaml: &str) -> Pipelines {
        let raw = format!("steps:\n{step_yaml}");
        let pipeline: Pipeline = serde_norway::from_str(&raw).unwrap();
        pipeline.validate().unwrap();
        let mut pipelines = std::collections::BTreeMap::new();
        pipelines.insert("default".to_string(), pipeline);
        Pipelines {
            default: "default".to_string(),
            pipelines,
        }
    }

    fn tracking(hook: &str) -> crate::config::IssueTrackingConfig {
        crate::config::IssueTrackingConfig {
            hook: hook.to_string(),
            ..Default::default()
        }
    }

    /// A `Repo` whose checkout is a scratch directory, real enough for
    /// `issue_tracking_checks` to stat a hook path against — nothing else
    /// reads `config` or `home` here, so both are defaults.
    fn scratch_repo(name: &str) -> Repo {
        let checkout = crate::scratch::root(&format!("doctor-{name}"));
        let _ = std::fs::remove_dir_all(&checkout);
        std::fs::create_dir_all(&checkout).unwrap();
        Repo {
            root: checkout.clone(),
            home: checkout.join(".home"),
            checkout,
            config: Config::default(),
        }
    }

    /// A hook named with nothing to hand it: `project_key` blank while
    /// `hook` names something. The one half-set shape a person can reach by
    /// hand, and the only one of the four that fires on `project_key` rather
    /// than on `hook` itself.
    #[test]
    fn issue_tracking_checks_refuses_a_hook_with_a_blank_project_key() {
        let repo = scratch_repo("blank-project-key");
        let findings = issue_tracking_checks(&repo, &tracking("record.sh"));
        let Finding::Check(label, outcome) = &findings[0] else {
            panic!("{:?}", findings[0])
        };
        assert_eq!(label, "`[issue_tracking]` is fully configured or fully off");
        let err = outcome.as_ref().unwrap_err();
        assert!(err.to_string().contains("record.sh"), "{err}");
        assert!(err.to_string().contains("project_key"), "{err}");
    }

    /// A hook holding a path rather than a bare filename — refused before it
    /// is ever joined onto `.spoolway/hooks/`, per
    /// `crate::tracking::is_bare_filename`.
    #[test]
    fn issue_tracking_checks_refuses_a_hook_that_is_not_a_bare_filename() {
        let repo = scratch_repo("not-bare");
        let findings = issue_tracking_checks(&repo, &tracking("../record.sh"));
        let Finding::Check(label, outcome) = &findings[1] else {
            panic!("{:?}", findings[1])
        };
        assert_eq!(label, "`issue_tracking.hook` is a bare filename");
        let err = outcome.as_ref().unwrap_err();
        assert!(err.to_string().contains("../record.sh"), "{err}");
        assert!(err.to_string().contains("not a bare filename"), "{err}");
    }

    /// A bare, well-formed name whose file was never written — the gap
    /// `init` closes for the two shipped hooks but not for a hand-edited
    /// config naming a script that was renamed or never copied in.
    #[test]
    fn issue_tracking_checks_refuses_a_hook_naming_a_script_that_does_not_exist() {
        let repo = scratch_repo("missing-script");
        let findings = issue_tracking_checks(&repo, &tracking("ghost.sh"));
        let Finding::Check(label, outcome) = &findings[2] else {
            panic!("{:?}", findings[2])
        };
        assert_eq!(label, "`issue_tracking.hook` script exists");
        let err = outcome.as_ref().unwrap_err();
        assert!(err.to_string().contains("ghost.sh"), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    /// `jira.sh` is the one shipped hook that shells out to binaries this
    /// project does not otherwise need — checked only when the hook actually
    /// names it, unlike the three checks above, which run for any hook.
    #[test]
    fn issue_tracking_checks_requires_acli_and_jq_for_the_jira_hook() {
        let repo = scratch_repo("jira-hook");
        let hook = crate::cli::Tracker::Jira.hook_name();
        let hooks_dir = repo.checkout.join(".spoolway/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(hooks_dir.join(&hook), "#!/bin/sh\n").unwrap();
        let findings = issue_tracking_checks(&repo, &tracking(&hook));

        let labels: Vec<&str> = findings
            .iter()
            .map(|f| match f {
                Finding::Check(label, _) => label.as_str(),
                _ => "",
            })
            .collect();
        assert!(labels.contains(&"`acli` is on PATH"), "{labels:?}");
        assert!(labels.contains(&"`jq` is on PATH"), "{labels:?}");

        // `acli` is an Atlassian CLI nothing in this project's own toolchain
        // installs, so it is reliably absent wherever this test runs —
        // unlike `jq`, common enough on a build image that asserting its
        // presence or absence here would make the test depend on the
        // machine rather than on `issue_tracking_checks`'s own logic.
        let acli = findings
            .iter()
            .find_map(|f| match f {
                Finding::Check(label, outcome) if label == "`acli` is on PATH" => Some(outcome),
                _ => None,
            })
            .unwrap();
        let err = acli.as_ref().unwrap_err();
        assert!(err.to_string().contains("jira.sh"), "{err}");
    }

    /// `key_in_names` on, but the configured hook script never writes
    /// `slug=`: a standing gap `doctor` names, and one it stays quiet about
    /// both when the script does write the line and when the flag is off.
    #[test]
    fn issue_tracking_checks_reports_key_in_names_without_a_slug_line() {
        let repo = scratch_repo("slug-gap");
        let hooks_dir = repo.checkout.join(".spoolway/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("jira.sh"),
            "#!/bin/sh\n{ echo \"epic=$SPOOLWAY_EPIC\"; echo \"ticket=x\"; } >\"$SPOOLWAY_OUT\"\n",
        )
        .unwrap();

        let on = crate::config::IssueTrackingConfig {
            hook: "jira.sh".into(),
            project_key: "PROJ".into(),
            key_in_names: true,
            ..Default::default()
        };
        let findings = issue_tracking_checks(&repo, &on);
        let gap = findings.iter().find_map(|f| match f {
            Finding::Check(label, outcome)
                if label == "the configured hook writes a `slug=` line" =>
            {
                Some(outcome)
            }
            _ => None,
        });
        let err = gap
            .expect("the slug check is missing")
            .as_ref()
            .unwrap_err();
        assert!(err.to_string().contains("key_in_names is on"), "{err}");
        assert!(err.to_string().contains("jira.sh"), "{err}");

        // Flag off: nothing to report.
        let off = crate::config::IssueTrackingConfig {
            key_in_names: false,
            ..on.clone()
        };
        let quiet = issue_tracking_checks(&repo, &off)
            .into_iter()
            .find_map(|f| match f {
                Finding::Check(label, outcome)
                    if label == "the configured hook writes a `slug=` line" =>
                {
                    Some(outcome)
                }
                _ => None,
            });
        assert!(quiet.unwrap().is_ok());
    }

    /// Neither of `gh`'s two callers is present: no step runs `spoolway
    /// stack`, and issue tracking names no hook at all.
    #[test]
    fn reaches_forge_is_false_with_neither_caller() {
        let pipelines = single_step_pipelines("  - id: a\n    end: true\n");
        assert!(!reaches_forge(&pipelines, &tracking("")));
    }

    /// A step whose `run:` calls `spoolway stack` is one of the two callers,
    /// on its own.
    #[test]
    fn reaches_forge_is_true_with_a_stack_step() {
        let pipelines = single_step_pipelines(
            "  - id: a\n    run: spoolway stack\n    on_pass: b\n  - id: b\n    end: true\n",
        );
        assert!(reaches_forge(&pipelines, &tracking("")));
    }

    /// Issue tracking naming the `github` hook is the other caller, even with
    /// no step reaching for `gh` itself.
    #[test]
    fn reaches_forge_is_true_with_the_github_hook() {
        let pipelines = single_step_pipelines("  - id: a\n    end: true\n");
        assert!(reaches_forge(
            &pipelines,
            &tracking(&crate::cli::Tracker::Github.hook_name())
        ));
    }

    /// A report with one of everything: a passing check, a note both modes
    /// print, a note only the listing prints, and a failure.
    fn mixed() -> Report {
        let mut report = Report::default();
        report.check("config parses", Ok(Some("/p/config.toml".into())));
        report.check("lanes can be started", Ok(None));
        report.note("no git remote");
        report.check("agent `pi`", Err(anyhow::anyhow!("`pi` is not on PATH")));
        report.note_verbose("no dispatcher running");
        report
    }

    /// What the whole change is for: the short report is the exceptions, and
    /// nothing else. Every passing row goes into the closing count instead.
    #[test]
    fn the_short_report_keeps_only_the_exceptions() {
        let out = mixed().render(false);
        assert_eq!(
            out,
            "  note  no git remote\n  FAIL  agent `pi`: `pi` is not on PATH\n\n\
             2 of 3 checks passed.\n"
        );
    }

    /// `-v` brings the listing back, in the order the checks were declared and
    /// with the wording each one already had.
    #[test]
    fn verbose_lists_every_row_in_declaration_order() {
        let out = mixed().render(true);
        assert_eq!(
            out,
            "  ok    config parses — /p/config.toml\n\
             \x20 ok    lanes can be started\n\
             \x20 note  no git remote\n\
             \x20 FAIL  agent `pi`: `pi` is not on PATH\n\
             \x20 note  no dispatcher running\n\n\
             2 of 3 checks passed.\n"
        );
    }

    /// A project with nothing to say about it is one line, with no blank line
    /// above it: there is nothing for the blank line to separate.
    #[test]
    fn a_clean_project_is_one_line() {
        let mut report = Report::default();
        report.check("config parses", Ok(None));
        report.check("on a usable branch", Ok(Some("main".into())));
        assert_eq!(
            report.render(false),
            "2 checks passed. Everything checks out.\n"
        );
    }

    /// Notes are not checks. They print, and they are not counted either way.
    #[test]
    fn notes_are_not_counted_as_checks() {
        let mut report = Report::default();
        report.check("config parses", Ok(None));
        report.note("no git remote");
        assert_eq!(report.checks(), 1);
        assert_eq!(report.problems(), 0);
        assert_eq!(
            report.render(false),
            "  note  no git remote\n\n1 checks passed. Everything checks out.\n"
        );
    }

    /// A failure replaces the reassurance with a tally, and is what the caller
    /// reads to decide the exit status.
    #[test]
    fn a_failure_gives_a_tally_and_a_problem_count() {
        let mut report = Report::default();
        report.check("config parses", Ok(None));
        report.check("agent `pi`", Err(anyhow::anyhow!("not on PATH")));
        report.check("agent `claude`", Err(anyhow::anyhow!("not on PATH")));
        assert_eq!(report.problems(), 2);
        assert!(report.render(false).ends_with("1 of 3 checks passed.\n"));
    }

    /// A note that carries its own continuation lines — the list of files
    /// `spoolway update` would take — prints the same block in both modes.
    #[test]
    fn a_multi_line_note_keeps_its_continuation_lines() {
        let mut report = Report::default();
        report.note("2 file(s) here are behind this spoolway\n          a.md (rewritten)\n          b.md (rewritten)");
        assert_eq!(
            report.render(false),
            "  note  2 file(s) here are behind this spoolway\n\
             \x20         a.md (rewritten)\n\
             \x20         b.md (rewritten)\n\n\
             0 checks passed. Everything checks out.\n"
        );
    }

    /// A backend built fresh for each call, headless: real, but with no
    /// panes to test — [`crate::headless::Headless::create_pane`] hands back
    /// a `tab_id` of `None`, which is exactly the shape `live_check` has to
    /// answer with a note rather than a check for.
    fn headless_mux(name: &str) -> crate::headless::Headless {
        let root = crate::scratch::root(&format!("doctor-live-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::headless::Headless::new(
            &root,
            &crate::config::DispatchConfig::default(),
            root.clone(),
        )
    }

    /// `--no-live` skips the row outright, without asking the backend
    /// anything — the row it produces is a note, not a check, so it never
    /// counts towards `problems()` either way.
    #[test]
    fn no_live_skips_the_row_without_touching_the_backend() {
        let mux = headless_mux("no-live");
        assert!(matches!(
            live_check(&mux, true),
            Finding::NoteVerbose(text) if text.contains("--no-live")
        ));
    }

    /// A backend with no real pane — headless — reads as a note explaining
    /// why, never as a failed check: nothing was opened, so there is
    /// nothing to say passed or failed.
    #[test]
    fn a_backend_with_no_real_pane_is_a_note_not_a_check() {
        let mux = headless_mux("no-real-pane");
        assert!(matches!(
            live_check(&mux, false),
            Finding::NoteVerbose(text) if text.contains("no real pane")
        ));
    }
}
